#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg/pkg-boot-virtio-blk-cli.sh — `baud run kvm --virtio-blk-image ...` end-to-end, through
# the real CLI/server (todo.md §14 item 5's remaining "boot/cmdline/CLI wiring" gap: the
# virtio-pci-blk device model, PCI transport, and a real-hardware-tested Multiverse-level
# combinator already existed, hardware-proven by baud_multiverse::linux::
# guest_virtio_pci_blk_driver_reads_and_writes_real_sectors, but nothing wired it into
# RunKvmBody/the CLI -- the concrete next prerequisite before attempting H9's real Ubuntu
# cloud-image boot).
#
# Mirrors drive/pkg/pkg-boot-cli.sh's structure (same checked-in linux-guest bzImage, no kernel
# compile needed, so this runs in seconds) but for the virtio_blk_initramfs.cpio.gz fixture +
# --virtio-blk-image instead of the plain --initramfs path, proving the real
# virtio_pci_legacy/virtio_blk kernel drivers probe baud's device and complete a real
# read+write+readback round-trip against a disk image supplied over the actual CLI/HTTP surface,
# not just crates/baud-server/src/routes/run_kvm.rs's own Rust-level test.

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[pkg-boot-virtio-blk-cli] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== baud run kvm --virtio-blk-image: real linux-guest, CLI/server end-to-end ==="
echo ""

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
BAUD="$REPO_ROOT/target/debug/baud"
FIXTURE_DIR="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/linux-guest"
KERNEL="$FIXTURE_DIR/bzImage"
INITRAMFS="$FIXTURE_DIR/virtio_blk_initramfs.cpio.gz"
DB_FILE="$(mktemp -u -t baud-pkg-boot-virtio-blk-cli-XXXXXX.sqlite)"
SNAP_ROOT="$(mktemp -d -t baud-pkg-boot-virtio-blk-cli-snap-XXXXXX)"
BLK_IMAGE="$(mktemp -t baud-pkg-boot-virtio-blk-cli-disk-XXXXXX.img)"
SERVER_PID=""

# Ephemeral port + per-script snapshot store, so this script can run concurrently with any other
# drive/*.sh (each server gets its own port, its own SQLite file and its own SnapshotStore root).
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"

for f in "$KERNEL" "$INITRAMFS"; do
    [[ -f "$f" ]] || fail "fixture missing: $f (see $FIXTURE_DIR/BUILD.md)"
done

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" "$BLK_IMAGE" 2>/dev/null || true
    rm -rf "$SNAP_ROOT" 2>/dev/null || true
}
trap cleanup EXIT
# bash does NOT run an EXIT trap when it dies from an untrapped signal, so `trap cleanup EXIT`
# alone leaks this script's baud-server, temp DB/disk-image and snapshot dir whenever the script
# is interrupted -- Ctrl-C, or drive/gate.sh reaping its pool. Re-raising through `exit` makes the
# EXIT trap fire, so cleanup() runs on every exit path.
trap 'exit 130' INT
trap 'exit 143' TERM

# Same fixed formula tests/fixtures/linux-guest/virtio_blk_init.c's own /init expects at sector 0
# (crates/baud-multiverse/src/linux/mod.rs's virtio_blk_test_base_image): byte i is i % 256,
# repeating every 256 bytes -- 4 sectors, matching the primitive-level and route-level tests.
python3 -c "
import sys
sector_size = 512
sectors = 4
with open(sys.argv[1], 'wb') as f:
    f.write(bytes(i % 256 for i in range(sector_size * sectors)))
" "$BLK_IMAGE"

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

# Spec §4.2's exact deterministic cmdline (bootparams::DETERMINISTIC_CMDLINE) with `pci=off`
# stripped -- a virtio-pci device needs real PCI enumeration to be found at all (same requirement
# drive/h/h7.sh's acpi-enabled leg has for `acpi=off`).
CMDLINE="console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc tsc=reliable no-kvmclock \
no_timer_check acpi=off reboot=t panic=-1 quiet loglevel=1 printk.time=0 \
random.trust_cpu=off random.trust_bootloader=on i8042.noaux i8042.nomux i8042.nopnp \
8250.nr_uarts=1 nomodule rdinit=/init"

log "baud run kvm --kernel $KERNEL --initramfs $INITRAMFS --virtio-blk-image $BLK_IMAGE ..."
BOOT_JSON="$("$BAUD" run kvm \
    --kernel "$KERNEL" \
    --initramfs "$INITRAMFS" \
    --cmdline "$CMDLINE" \
    --periodic-timer-period-rcb 500000 \
    --periodic-timer-vector 236 \
    --periodic-timer-max-ticks 2000 \
    --virtio-blk-image "$BLK_IMAGE" \
    --virtio-blk-vector 59 \
    --json)" || fail "'baud run kvm --json' FAILED to run"
echo "$BOOT_JSON"

OK="$(echo "$BOOT_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")"
[[ "$OK" == "True" ]] || fail "'baud run kvm' reported ok!=true: $BOOT_JSON"
pass "'baud run kvm' reported ok=true"

CONSOLE_HEX="$(echo "$BOOT_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)['console_output_hex'])")"
CONSOLE_TEXT="$(python3 -c "import sys; print(bytes.fromhex(sys.argv[1]).decode('utf-8', 'replace'))" "$CONSOLE_HEX")"

echo "$CONSOLE_TEXT" | grep -q "baud-guest: minimal kernel reached /init" \
    || fail "console output does not contain the /init marker:\n$CONSOLE_TEXT"
pass "guest reached /init (marker found in console output over real HTTP)"

echo "$CONSOLE_TEXT" | grep -q "baud-guest: blk-open-ok" \
    || fail "console output does not contain the virtio-blk open marker:\n$CONSOLE_TEXT"
pass "guest's real virtio_pci_legacy/virtio_blk drivers opened /dev/vda over real HTTP wiring"

echo "$CONSOLE_TEXT" | grep -q "baud-guest: blk-write-sector1-ok" \
    || fail "console output does not contain the virtio-blk write marker:\n$CONSOLE_TEXT"
pass "a real VIRTIO_BLK_T_OUT write completed through the CLI-supplied disk image"

echo ""
echo "=== baud run kvm --virtio-blk-image: PASSED ==="
echo ""
echo "POST /run/kvm now threads an optional virtio_blk spec through to"
echo "Multiverse::enable_virtio_pci_blk/run_to_first_halt_with_virtio_pci_blk (and the three-device"
echo "periodic-timer combinator when periodic_timer is also set), closing todo.md §14 item 5's"
echo "remaining 'boot/cmdline/CLI wiring' gap for a real, unmodified Linux kernel guest's virtio-blk"
echo "driver -- the concrete prerequisite before attempting H9's real Ubuntu cloud-image boot."
