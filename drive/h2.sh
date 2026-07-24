#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h2.sh — H2 drive script: deterministic double-run (todo.md §10's H2 definition)
#
# H2's spec: "Same image + tape twice ⇒ byte-identical observation stream (console + probes +
# final memory hash), CPUID masked, virtual TSC pinned." This validates that for real against
# actual /dev/kvm — this is NOT the pre-pivot "tape-integration workload fuzzing" H2 a prior
# version of this script tested (todo.md §14 flagged h2.sh/h3.sh as stale, still validating an old
# ptrace-era milestone definition; this rewrite replaces it, mirroring h1.sh's own rewrite once
# real KVM hardware existed to make the current H2 meaningful).
#
#   H2.1  baud host probe still reports a non-rejected regime (cheap, early sanity check)
#   H2.2  cpuid_leaves_are_fixed (`masked_bits_are_always_fixed_regardless_of_host_input`):
#         RDRAND/RDSEED/TSX/x2APIC bits are always 0 regardless of host CPUID input
#   H2.3  work_clock_is_monotone_and_reproducible: the work-clock is non-decreasing and the full
#         sequence is identical across a same-(base,k) double-run
#   H2.4  double_boot_memory_identical (H1's own test, re-verified here as part of H2's "same
#         image+tape twice" guarantee): boots hello-guest twice against real /dev/kvm, asserts
#         console output and guest-RAM blake3 hash are byte-identical across both boots
#   H2.5  all_input_is_tape_derived: boots tape-echo-guest (reads 4 bytes from the tape device,
#         echoes them to console) three times against real /dev/kvm — same tape twice produces
#         byte-identical output; changing one tape byte changes the output (input is genuinely
#         tape-derived, not a synthetic stand-in — test-matrix row 21's "fake determinism" risk)
#   H2.6  no_unmodeled_exit_is_silent: a fuzz smoke proptest over random `Exit::Unmodeled` values
#         asserts the run loop's dispatch never returns anything but `Err(DeterminismHole)` for
#         them — no wildcard arm, no silent continue

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h2] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H2: Deterministic double-run ==="
echo ""

# ---------------------------------------------------------------------------
# H2.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# Same pattern as drive/h0.sh and drive/h1.sh.
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli..."
cargo build -q -p baud-host -p baud-server -p baud-cli 2>&1

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h2-XXXXXX.sqlite)"
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
pkill -f "baud-server" 2>/dev/null || true; sleep 0.2
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" "$BAUD_SERVER_BIN" &
SERVER_PID=$!
sleep 1

log "baud host probe --json"
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H2.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
REGIME="$(echo "$PROBE_JSON" | grep -o '"regime":[[:space:]]*"[^"]*"' | sed -E 's/.*"([^"]+)"$/\1/')"
if [[ "$REGIME" == "rejected" || -z "$REGIME" ]]; then
    fail "H2.1: host probe regime is '$REGIME' — no real /dev/kvm, H2 cannot mean anything here."
fi
pass "H2.1: host probe regime='$REGIME' (real KVM present)"

# ---------------------------------------------------------------------------
# H2.2 — cpuid_leaves_are_fixed
# ---------------------------------------------------------------------------
log "Building baud-multiverse..."
cargo build -q -p baud-multiverse 2>&1 || fail "H2.2: baud-multiverse build FAILED"

log "Running masked_bits_are_always_fixed_regardless_of_host_input (cpuid_leaves_are_fixed)..."
CPUID_OUT=$(cargo test -q -p baud-multiverse masked_bits_are_always_fixed_regardless_of_host_input 2>&1)
echo "$CPUID_OUT"
echo "$CPUID_OUT" | grep -q "test result: ok" || fail "H2.2: cpuid_leaves_are_fixed FAILED"
pass "H2.2: cpuid_leaves_are_fixed — RDRAND/RDSEED/TSX/x2APIC always masked to 0"

# ---------------------------------------------------------------------------
# H2.3 — work_clock_is_monotone_and_reproducible
# ---------------------------------------------------------------------------
log "Running work_clock_is_monotone_and_reproducible..."
CLOCK_OUT=$(cargo test -q -p baud-multiverse work_clock_is_monotone_and_reproducible 2>&1)
echo "$CLOCK_OUT"
echo "$CLOCK_OUT" | grep -q "test result: ok" || fail "H2.3: work_clock_is_monotone_and_reproducible FAILED"
pass "H2.3: work_clock_is_monotone_and_reproducible — non-decreasing and reproducible"

# ---------------------------------------------------------------------------
# H2.4 — double_boot_memory_identical (H1's test, re-verified as part of H2)
# ---------------------------------------------------------------------------
log "Running double_boot_memory_identical against real /dev/kvm..."
BOOT_OUT=$(cargo test -q -p baud-multiverse double_boot_memory_identical -- --test-threads=1 2>&1)
echo "$BOOT_OUT"
echo "$BOOT_OUT" | grep -q "test result: ok" || fail "H2.4: double_boot_memory_identical FAILED"
pass "H2.4: double_boot_memory_identical — console + RAM hash byte-identical across two real boots"

# ---------------------------------------------------------------------------
# H2.5 — all_input_is_tape_derived (real hardware, new this milestone)
# ---------------------------------------------------------------------------
log "Running all_input_is_tape_derived against real /dev/kvm (tape-echo-guest)..."
TAPE_OUT=$(cargo test -q -p baud-multiverse all_input_is_tape_derived -- --test-threads=1 2>&1)
echo "$TAPE_OUT"
echo "$TAPE_OUT" | grep -q "test result: ok" || fail "H2.5: all_input_is_tape_derived FAILED"
pass "H2.5: all_input_is_tape_derived — same tape twice identical; one changed byte changes output"

# ---------------------------------------------------------------------------
# H2.6 — no_unmodeled_exit_is_silent
# ---------------------------------------------------------------------------
log "Running no_unmodeled_exit_is_silent (baud-vcpu)..."
cargo build -q -p baud-vcpu 2>&1 || fail "H2.6: baud-vcpu build FAILED"
UNMODELED_OUT=$(cargo test -q -p baud-vcpu no_unmodeled_exit_is_silent 2>&1)
echo "$UNMODELED_OUT"
echo "$UNMODELED_OUT" | grep -q "test result: ok" || fail "H2.6: no_unmodeled_exit_is_silent FAILED"
pass "H2.6: no_unmodeled_exit_is_silent — every unmodeled exit fails loud, never a silent continue"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H2 milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (regime=$REGIME):"
echo "  - CPUID leaves are fixed (RDRAND/RDSEED/TSX/x2APIC masked, reproducible across reads)"
echo "  - The work-clock is monotone and reproducible for a fixed (base, k)"
echo "  - Same guest image + tape boots to byte-identical console + RAM state twice in a row"
echo "  - The tape device is the guest's genuine input channel: same tape -> same output,"
echo "    one changed tape byte -> changed output (tests/fixtures/tape-echo-guest/)"
echo "  - No VM exit is ever silently unhandled — the catch-all always fails loud"
echo ""
echo "Run H3 next: ./drive/h3.sh"
