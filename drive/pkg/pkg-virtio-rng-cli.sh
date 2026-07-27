#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg/pkg-virtio-rng-cli.sh — `baud run kvm --virtio-rng-seed ...` end-to-end, through the real
# CLI/server (todo.md §14 next-actions item 1's last-open virtio-rng gap: "nothing wires
# enable_virtio_rng()/seed_virtio_rng_entropy()/service_virtio_rng_interrupt() into any real boot's
# cmdline/CLI/server route").
#
# The virtio-rng device model itself (transport register window, split-virtqueue ring parsing,
# interrupt delivery with no in-kernel irqchip) was already hardware-verified against
# `tests/fixtures/virtio-rng-guest/` by two Rust tests calling `Multiverse` directly
# (`virtio_rng_interrupt_reaches_the_guests_own_isr`,
# `virtio_rng_interrupt_delivery_is_reproducible_across_two_boots`). This script proves the same
# fixture boots through the actual `baud run kvm` CLI invocation against a live `baud-server` over
# real HTTP — the missing "boot/cmdline/CLI wiring" piece, not a new hardware capability.
#
# Uses the already-built, checked-in fixture (no kernel/assembler build needed), so this runs in
# under a second — still opt-in (not part of the standard h0-h7 gate), matching the
# drive/pkg/pkg-*.sh/drive/h*-enforced-*.sh convention for anything exercising a specific fixture
# rather than the standard gate.

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[pkg-virtio-rng-cli] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== baud run kvm --virtio-rng-seed: real virtio-rng-guest, CLI/server end-to-end ==="
echo ""

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
BAUD="$REPO_ROOT/target/debug/baud"
FIXTURE_DIR="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/virtio-rng-guest"
KERNEL="$FIXTURE_DIR/bzImage"
DB_FILE="$(mktemp -u -t baud-pkg-virtio-rng-cli-XXXXXX.sqlite)"
SNAP_ROOT="$(mktemp -d -t baud-pkg-virtio-rng-cli-snap-XXXXXX)"
SERVER_PID=""

# Ephemeral port + per-script snapshot store, so this script can run concurrently with any other
# drive/*.sh (each server gets its own port, its own SQLite file and its own SnapshotStore root).
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"

[[ -f "$KERNEL" ]] || fail "fixture missing: $KERNEL (see $FIXTURE_DIR/BUILD.md)"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" 2>/dev/null || true
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

log "Building baud-server/baud-cli..."
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q -p baud-server -p baud-cli 2>&1
fi

log "Starting baud-server (DB: $DB_FILE, port: $BAUD_PORT)..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$BAUD_PORT" \
    BAUD_SNAPSHOT_STORE="$SNAP_ROOT" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for i in $(seq 1 60); do
    if curl -sf "$SRV/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf "$SRV/health" > /dev/null || fail "baud-server did not start"
pass "baud-server is running (PID $SERVER_PID)"

log "baud run kvm --kernel $KERNEL --virtio-rng-seed 42 --virtio-rng-vector 49 ..."
BOOT_JSON="$("$BAUD" run kvm \
    --kernel "$KERNEL" \
    --cmdline "console=ttyS0" \
    --virtio-rng-seed 42 \
    --virtio-rng-vector 49 \
    --json)" || fail "'baud run kvm --json' FAILED to run"
echo "$BOOT_JSON"

OK="$(echo "$BOOT_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")"
[[ "$OK" == "True" ]] || fail "'baud run kvm' reported ok!=true: $BOOT_JSON"
pass "'baud run kvm' reported ok=true"

CONSOLE_HEX="$(echo "$BOOT_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)['console_output_hex'])")"
# The guest's ISR (vector 0x31 = 49) writes a fixed 'R' marker then the one entropy byte
# service_virtio_rng filled its posted buffer with (crates/baud-multiverse/tests/fixtures/
# virtio-rng-guest/payload.s's isr:) -- exactly 2 bytes total, first byte 'R' (0x52), if and only
# if the interrupt was actually delivered through the real CLI/server path.
[[ "$CONSOLE_HEX" == 52* ]] || fail "console output does not start with the ISR's 'R' marker (0x52): $CONSOLE_HEX"
[[ ${#CONSOLE_HEX} -eq 4 ]] || fail "console output must be exactly 2 bytes (marker + entropy byte): $CONSOLE_HEX"
pass "guest's ISR fired through a real CLI/server-driven interrupt (console: $CONSOLE_HEX)"

echo ""
echo "=== baud run kvm --virtio-rng-seed: PASSED ==="
echo ""
echo "POST /run/kvm now threads an optional virtio_rng spec through to"
echo "Multiverse::enable_virtio_rng/seed_virtio_rng_entropy/run_to_first_halt_with_virtio_rng,"
echo "closing todo.md §14 item 1's last-open virtio-rng 'boot/cmdline/CLI wiring' gap for the"
echo "primary /run/kvm boot route (branch/resume and stream replay wiring remain open, smaller"
echo "follow-ups, per todo.md)."
