#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h1.sh — H1 drive script: boot a real guest (specs/baud-multiverse.md's KVM/VT-x pivot,
# todo.md §10's H1 definition)
#
# H1's spec: "The run loop boots a minimal guest kernel that prints to the serial console; clean
# Hlt/Shutdown." This validates that for real against actual /dev/kvm — this is NOT the pre-pivot
# ptrace/seccomp "supervisor MVP" a prior version of this script tested (todo.md §14 flagged that
# version as testing a superseded milestone definition; this rewrite replaces it now that real KVM
# hardware exists to make the current H1 meaningful, per that same todo.md entry's own suggestion).
#
#   H1.1  baud-multiverse crate builds for the real target (kvm-ioctls/kvm-bindings/linux-loader
#         linked, not just `cargo check`)
#   H1.2  double_boot_memory_identical passes: boots crates/baud-multiverse/tests/fixtures/
#         hello-guest/bzImage twice against real /dev/kvm, asserts the console marker and
#         guest-RAM blake3 hash are byte-identical across both boots
#   H1.3  baud host probe still reports runnable=true (the real hardware this milestone
#         needs is still present — a fast, early sanity check before trusting H1.2's result)

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h1] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H1: Boot a real guest ==="
echo ""

# ---------------------------------------------------------------------------
# H1.3 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# Same pattern as drive/h0.sh: `baud host probe` is a CLI-to-server call, so a server needs to be
# up first.
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli..."
cargo build -q -p baud-host -p baud-server -p baud-cli 2>&1

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h1-XXXXXX.sqlite)"
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
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H1.3: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
RUNNABLE="$(echo "$PROBE_JSON" | grep -oE '"runnable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
if [[ "$RUNNABLE" != "true" ]]; then
    fail "H1.3: host probe runnable='$RUNNABLE' — no real /dev/kvm, H1 cannot mean anything here."
fi
pass "H1.3: host probe runnable='$RUNNABLE' (real KVM present)"

# ---------------------------------------------------------------------------
# H1.1 — build baud-multiverse for real (links kvm-ioctls/kvm-bindings/linux-loader)
# ---------------------------------------------------------------------------
log "Building baud-multiverse..."
cargo build -q -p baud-multiverse 2>&1 || fail "H1.1: baud-multiverse build FAILED"
pass "H1.1: baud-multiverse builds (real KVM boot flow linked)"

# ---------------------------------------------------------------------------
# H1.2 — the real boot, twice, against actual /dev/kvm
# ---------------------------------------------------------------------------
log "Running double_boot_memory_identical against real /dev/kvm..."
TEST_OUT=$(cargo test -q -p baud-multiverse double_boot_memory_identical -- --test-threads=1 2>&1)
echo "$TEST_OUT"

if echo "$TEST_OUT" | grep -q "test result: ok"; then
    pass "H1.2: double_boot_memory_identical PASSED — real guest booted, console marker matched, RAM hash identical across two boots"
else
    fail "H1.2: double_boot_memory_identical FAILED"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H1 milestone: ALL CHECKS PASSED ==="
echo ""
echo "crates/baud-multiverse/src/linux/: real KVM/VT-x boot flow"
echo "  - Kvm::new -> create_vm -> zeroed guest RAM -> create_vcpu -> CPUID mask + MSR filter"
echo "    -> identity page tables -> 64-bit long mode -> linux-loader bzImage load -> KVM_RUN"
echo "  - Multiverse::boot(kernel_path, cmdline, base, k, tape) -> Result<Multiverse>"
echo "  - Multiverse::run_to_first_halt(&mut self) -> Result<HaltOutcome>"
echo ""
echo "Fixture: crates/baud-multiverse/tests/fixtures/hello-guest/ (see BUILD.md)"
echo ""
echo "Exit criterion met: the same guest image + tape boots to an identical console + RAM state"
echo "twice in a row on real /dev/kvm (runnable=$RUNNABLE)."
echo ""
echo "Run H2 next: ./drive/h2.sh"
