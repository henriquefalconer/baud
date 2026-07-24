#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h3.sh — H3 drive script: randomness + time control (todo.md §10's H3 definition)
#
# H3's spec: "Entropy and timestamps flow only through masked CPUID + tape/work-clock; a
# raw-random guest is hardware-blocked (cooperative) or trapped (enforced)." This validates that
# for real against actual /dev/kvm — this is NOT the pre-pivot "multi-guest cluster + net device"
# H3 a prior version of this script tested (todo.md §14 flagged h3.sh as stale, still validating
# an old ptrace-era milestone definition; this rewrite replaces it, mirroring h1.sh/h2.sh's own
# rewrites once real KVM hardware existed to make the current H-series meaningful).
#
#   H3.1  baud host probe still reports a non-rejected regime (cheap, early sanity check)
#   H3.2  rdrand_guest_is_flagged: a guest that ignores the masked CPUID feature bit and executes
#         `rdrand` anyway never gets past it — real hardware raises #UD immediately (VT-x's own
#         instruction-level CPUID gate), which cascades to a triple fault this crate's run loop
#         already treats as a clean halt; two boots produce byte-identical output stopping at the
#         pre-rdrand marker (deterministic, not a divergence — see
#         crates/baud-multiverse/tests/fixtures/rdrand-guest/BUILD.md for the full finding)
#   H3.3  regime_is_recorded_and_not_overclaimed: `baud host probe --require enforced` on this
#         cooperative-only host (no custom KVM module exists yet) exits 1 with a clear message,
#         never a false pass; `--require cooperative` passes

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h3] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H3: Randomness + time control ==="
echo ""

# ---------------------------------------------------------------------------
# H3.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# Same pattern as drive/h0.sh, drive/h1.sh, drive/h2.sh.
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli/baud-multiverse..."
cargo build -q -p baud-host -p baud-server -p baud-cli -p baud-multiverse 2>&1

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h3-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" 2>/dev/null || true
}
trap cleanup EXIT

log "Starting baud-server..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" "$BAUD_SERVER_BIN" &
SERVER_PID=$!
sleep 1

log "baud host probe --json"
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H3.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
REGIME="$(echo "$PROBE_JSON" | grep -o '"regime":[[:space:]]*"[^"]*"' | sed -E 's/.*"([^"]+)"$/\1/')"
if [[ "$REGIME" == "rejected" || -z "$REGIME" ]]; then
    fail "H3.1: host probe regime is '$REGIME' — no real /dev/kvm, H3 cannot mean anything here."
fi
pass "H3.1: host probe regime='$REGIME' (real KVM present)"

# ---------------------------------------------------------------------------
# H3.2 — rdrand_guest_is_flagged
# ---------------------------------------------------------------------------
log "Running rdrand_guest_is_flagged against real /dev/kvm (rdrand-guest fixture)..."
RDRAND_OUT=$(cargo test -q -p baud-multiverse rdrand_guest_is_flagged -- --test-threads=1 2>&1)
echo "$RDRAND_OUT"
echo "$RDRAND_OUT" | grep -q "test result: ok" || fail "H3.2: rdrand_guest_is_flagged FAILED"
pass "H3.2: rdrand_guest_is_flagged — masked CPUID hardware-blocks rdrand (#UD), deterministic across boots"

# ---------------------------------------------------------------------------
# H3.3 — regime_is_recorded_and_not_overclaimed
# ---------------------------------------------------------------------------
log "Running regime_is_recorded_and_not_overclaimed (baud-cli unit test)..."
OVERCLAIM_OUT=$(cargo test -q -p baud-cli regime_is_recorded_and_not_overclaimed 2>&1)
echo "$OVERCLAIM_OUT"
echo "$OVERCLAIM_OUT" | grep -q "test result: ok" || fail "H3.3: regime_is_recorded_and_not_overclaimed unit test FAILED"

log "Checking 'baud host probe --require enforced' refuses to overclaim on this cooperative-only host..."
if "$BAUD" host probe --json --require enforced > /tmp/h3-require-enforced.json 2>/tmp/h3-require-enforced.err; then
    fail "H3.3: 'baud host probe --require enforced' exited 0 on a '$REGIME' host — this is a false pass"
fi
grep -q "does not meet the requested" /tmp/h3-require-enforced.err \
    || fail "H3.3: '--require enforced' failed without the expected clear message"
pass "H3.3: 'baud host probe --require enforced' exits 1 with a clear message on regime='$REGIME' (no false pass)"

log "Checking 'baud host probe --require cooperative' passes on this host..."
"$BAUD" host probe --json --require cooperative > /dev/null \
    || fail "H3.3: 'baud host probe --require cooperative' unexpectedly failed on regime='$REGIME'"
pass "H3.3: 'baud host probe --require cooperative' exits 0 on regime='$REGIME'"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H3 milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (regime=$REGIME):"
echo "  - A guest that ignores the masked CPUID feature bit and executes rdrand anyway never"
echo "    reaches real entropy: real hardware #UDs it immediately (VT-x's own instruction-level"
echo "    CPUID gate), deterministically and identically across two boots"
echo "  - baud host probe --require <regime> never overclaims: asking for 'enforced' on this"
echo "    cooperative-only host exits 1 with a clear message, never a false pass"
echo ""
echo "H-series H0-H3 complete. Proceed to H4: ./drive/h4.sh"
