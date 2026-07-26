#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h7.sh — H7 drive script: real Linux guest, boot to userspace (todo.md §10's H7, partial)
#
# H7's full spec is "real Linux guest: boot -> double-boot -> OS-entropy" (guest_kernel_boots_to_
# userspace, double_boot_ram_hash_identical, os_entropy_is_deterministic). This script demonstrates
# the first, foundational leg for real: a real, compiled (not hand-assembled) Linux 6.18 kernel
# booting through baud-multiverse's real KVM boot flow, driven by H4's open-ended periodic-timer-
# injection engine, all the way to a real /init process that prints a marker and cleanly powers off
# -- tests/fixtures/linux-guest/BUILD.md has the full account, including three real bugs this
# fixture's first real boot caught (two in baud-multiverse, one in baud-vcpu). The OS-entropy leg
# and a guest-driven-checkpoint double-boot RAM-hash comparison remain open (todo.md §14).
#
#   H7.1  baud host probe still reports runnable=true (cheap, early sanity check)
#   H7.2  guest_kernel_boots_to_userspace: two boots of the real kernel+initramfs each reach /init,
#         print the marker, and halt cleanly after the same number of periodic ticks

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h7] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H7 (partial): real Linux guest boots to userspace ==="
echo ""

# ---------------------------------------------------------------------------
# H7.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli/baud-multiverse..."
cargo build -q -p baud-host -p baud-server -p baud-cli -p baud-multiverse 2>&1

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h7-XXXXXX.sqlite)"
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
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H7.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
RUNNABLE="$(echo "$PROBE_JSON" | grep -oE '"runnable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
if [[ "$RUNNABLE" != "true" ]]; then
    fail "H7.1: host probe runnable is '$RUNNABLE' — no real /dev/kvm, H7 cannot mean anything here."
fi
pass "H7.1: host probe runnable='$RUNNABLE' (real KVM present)"

# ---------------------------------------------------------------------------
# H7.2 — guest_kernel_boots_to_userspace
# ---------------------------------------------------------------------------
log "Running guest_kernel_boots_to_userspace against real /dev/kvm (linux-guest fixture)..."
BOOT_OUT=$(cargo test -q -p baud-multiverse guest_kernel_boots_to_userspace -- --test-threads=1 2>&1)
echo "$BOOT_OUT"
echo "$BOOT_OUT" | grep -q "test result: ok" || fail "H7.2: guest_kernel_boots_to_userspace FAILED"
pass "H7.2: guest_kernel_boots_to_userspace — a real Linux kernel reaches /init and halts cleanly, twice, with matching tick counts"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H7 (partial) milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (runnable=$RUNNABLE):"
echo "  - A real, compiled Linux 6.18 kernel (tests/fixtures/linux-guest/, not a hand-assembled"
echo "    payload) boots through baud-multiverse's real KVM boot flow to a real /init process"
echo "  - H4's open-ended periodic-timer-injection engine, not a pre-known tick count, drives the"
echo "    guest's own scheduler timer needs — no LAPIC device model needed (see that fixture's"
echo "    BUILD.md for why)"
echo "  - /init prints its marker (via raw port I/O, not the interrupt-driven tty path — the"
echo "    machine has no interrupt controller) and reboot(RB_POWER_OFF) falls back to a clean halt"
echo "  - Two boots of the same image+tape survive the same number of periodic ticks before their"
echo "    own natural halt"
echo ""
echo "Still open for full H7 (todo.md §14): os_entropy_is_deterministic and double_boot_ram_hash_"
echo "identical both exist now (drive/h7-enforced-entropy.sh, drive/h7-enforced-checkpoint.sh) but"
echo "are not yet 100% reproducible on real hardware — see linux-guest/BUILD.md for the residual"
echo "RCB/perf_event-read-jitter findings behind each."
