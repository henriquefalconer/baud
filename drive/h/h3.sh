#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h/h3.sh — H3 drive script: randomness + time control (todo.md §10's H3 definition)
#
# H3's spec: "Entropy and timestamps flow only through masked CPUID + tape/work-clock; a
# raw-random guest is hardware-blocked (cooperative) or trapped (enforced)." This validates that
# for real against actual /dev/kvm — this is NOT the pre-pivot "multi-guest cluster + net device"
# H3 a prior version of this script tested (todo.md §14 flagged h3.sh as stale, still validating
# an old ptrace-era milestone definition; this rewrite replaces it, mirroring h1.sh/h2.sh's own
# rewrites once real KVM hardware existed to make the current H-series meaningful).
#
#   H3.1  baud host probe still reports runnable=true (cheap, early sanity check)
#   H3.2  rdrand_guest_is_flagged: a guest that ignores the masked CPUID feature bit and executes
#         `rdrand` anyway never gets past it — real hardware raises #UD immediately (VT-x's own
#         instruction-level CPUID gate), which cascades to a triple fault this crate's run loop
#         already treats as a clean halt; two boots produce byte-identical output stopping at the
#         pre-rdrand marker (deterministic, not a divergence — see
#         crates/baud-multiverse/tests/fixtures/rdrand-guest/BUILD.md for the full finding)
#   H3.3  capability_is_recorded_and_not_overclaimed: `baud host probe --require enforced` on this
#         cooperative-only host (no custom KVM module exists yet) exits 1 with a clear message,
#         never a false pass; `--require cooperative` passes
#   H3.4  rdtsc_guest_reproduces_high_bits_across_boots: RDTSC has no CPUID gate (unlike RDRAND),
#         so a *compliant* guest reading the raw timestamp instruction directly still needs the
#         VMM to serve it a reproducible value — `boot_guest` now pins the vCPU's raw TSC via
#         `KVM_SET_MSRS(IA32_TSC=0)` right before entry; this asserts a real guest's `rdtsc` reads
#         reproduce in their high bits across two boots (see
#         crates/baud-multiverse/tests/fixtures/rdtsc-guest/BUILD.md for the full finding)

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h3] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H3: Randomness + time control ==="
echo ""

# ---------------------------------------------------------------------------
# H3.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# Same pattern as drive/h/h0.sh, drive/h/h1.sh, drive/h/h2.sh.
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli/baud-multiverse..."
# BAUD_GATE_PREBUILT: set by a gate that has already built the workspace, so the (~7s, target-dir
# locking) no-op `cargo build` below can be skipped when many drive scripts run concurrently.
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q -p baud-host -p baud-server -p baud-cli -p baud-multiverse 2>&1
fi

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h3-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"
REQUIRE_JSON="$(mktemp -t baud-h3-require-enforced-XXXXXX.json)"
REQUIRE_ERR="$(mktemp -t baud-h3-require-enforced-XXXXXX.err)"

# Own port + own snapshot store, so this script can run concurrently with any other drive script.
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"
SNAP_ROOT="$(mktemp -d -t baud-h3-snap-XXXXXX)"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" "$REQUIRE_JSON" "$REQUIRE_ERR" 2>/dev/null || true
    rm -rf "$SNAP_ROOT" 2>/dev/null || true
}
trap cleanup EXIT
# bash does NOT run an EXIT trap when it dies from an untrapped signal, so `trap cleanup EXIT`
# alone leaks this script's baud-server, temp DB and snapshot dir whenever the script is
# interrupted -- Ctrl-C, or drive/gate.sh reaping its pool. Re-raising through `exit` makes the
# EXIT trap fire, so cleanup() runs on every exit path. (This is how 21 stray temp SQLite files
# and two orphaned servers survived a killed gate run.)
trap 'exit 130' INT
trap 'exit 143' TERM

log "Starting baud-server on $SRV..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$BAUD_PORT" \
    BAUD_SNAPSHOT_STORE="$SNAP_ROOT" "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "$SRV/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf "$SRV/health" > /dev/null 2>&1 || fail "baud-server did not come up on $SRV"

log "baud host probe --json"
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H3.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
RUNNABLE="$(echo "$PROBE_JSON" | grep -oE '"runnable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
if [[ "$RUNNABLE" != "true" ]]; then
    fail "H3.1: host probe runnable is '$RUNNABLE' — no real /dev/kvm, H3 cannot mean anything here."
fi
pass "H3.1: host probe runnable='$RUNNABLE' (real KVM present)"

# ---------------------------------------------------------------------------
# H3.2 — rdrand_guest_is_flagged
# ---------------------------------------------------------------------------
log "Running rdrand_guest_is_flagged against real /dev/kvm (rdrand-guest fixture)..."
RDRAND_OUT=$(cargo test -q -p baud-multiverse rdrand_guest_is_flagged -- --test-threads=1 2>&1)
echo "$RDRAND_OUT"
echo "$RDRAND_OUT" | grep -q "test result: ok" || fail "H3.2: rdrand_guest_is_flagged FAILED"
pass "H3.2: rdrand_guest_is_flagged — masked CPUID hardware-blocks rdrand (#UD), deterministic across boots"

# ---------------------------------------------------------------------------
# H3.3 — capability_is_recorded_and_not_overclaimed
# ---------------------------------------------------------------------------
log "Running capability_is_recorded_and_not_overclaimed (baud-cli unit test)..."
OVERCLAIM_OUT=$(cargo test -q -p baud-cli capability_is_recorded_and_not_overclaimed 2>&1)
echo "$OVERCLAIM_OUT"
echo "$OVERCLAIM_OUT" | grep -q "test result: ok" || fail "H3.3: capability_is_recorded_and_not_overclaimed unit test FAILED"

log "Checking 'baud host probe --require enforced' refuses to overclaim on this cooperative-only host..."
if "$BAUD" host probe --json --require enforced > "$REQUIRE_JSON" 2>"$REQUIRE_ERR"; then
    fail "H3.3: 'baud host probe --require enforced' exited 0 on a runnable='$RUNNABLE' host — this is a false pass"
fi
grep -q "does not meet the requested" "$REQUIRE_ERR" \
    || fail "H3.3: '--require enforced' failed without the expected clear message"
pass "H3.3: 'baud host probe --require enforced' exits 1 with a clear message on runnable='$RUNNABLE' (no false pass)"

log "Checking 'baud host probe --require cooperative' passes on this host..."
"$BAUD" host probe --json --require cooperative > /dev/null \
    || fail "H3.3: 'baud host probe --require cooperative' unexpectedly failed on runnable='$RUNNABLE'"
pass "H3.3: 'baud host probe --require cooperative' exits 0 on runnable='$RUNNABLE'"

# ---------------------------------------------------------------------------
# H3.4 — rdtsc_guest_reproduces_high_bits_across_boots
# ---------------------------------------------------------------------------
log "Running rdtsc_guest_reproduces_high_bits_across_boots against real /dev/kvm (rdtsc-guest fixture)..."
RDTSC_OUT=$(cargo test -q -p baud-multiverse rdtsc_guest_reproduces_high_bits_across_boots -- --test-threads=1 2>&1)
echo "$RDTSC_OUT"
echo "$RDTSC_OUT" | grep -q "test result: ok" || fail "H3.4: rdtsc_guest_reproduces_high_bits_across_boots FAILED"
pass "H3.4: rdtsc_guest_reproduces_high_bits_across_boots — raw rdtsc reproduces in its high bits across two boots once pinned"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H3 milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (runnable=$RUNNABLE):"
echo "  - A guest that ignores the masked CPUID feature bit and executes rdrand anyway never"
echo "    reaches real entropy: real hardware #UDs it immediately (VT-x's own instruction-level"
echo "    CPUID gate), deterministically and identically across two boots"
echo "  - baud host probe --require <capability> never overclaims: asking for 'enforced' on this"
echo "    cooperative-only host exits 1 with a clear message, never a false pass"
echo "  - A compliant guest's raw rdtsc reproduces in its high bits across two boots now that"
echo "    the vCPU's TSC value is pinned at boot (KVM_SET_MSRS(IA32_TSC=0)), not just its frequency"
echo ""
echo "H-series H0-H3 complete. Proceed to H4: ./drive/h/h4.sh"
