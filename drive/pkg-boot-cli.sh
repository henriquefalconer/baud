#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg-boot-cli.sh — `baud run kvm --initramfs ... --periodic-timer-period-rcb ...`
# end-to-end, through the real CLI/server (todo.md §14 item 1's final open item under "Guest boot
# pipeline": "`baud run kvm` (`RunKvmBody`) has no `initramfs` field at all, so a `baud image
# build`-produced image cannot yet be booted through the CLI/server path end-to-end").
#
# drive/pkg-build-cli.sh already proves `baud image build` produces a real bzImage +
# initramfs.cpio.gz pair over the CLI/server; drive/h7.sh already proves the real linux-guest
# kernel+initramfs boots to /init through baud-multiverse directly (Rust test, no HTTP). This
# script is the missing middle: the exact checked-in linux-guest fixture (bzImage +
# initramfs.cpio.gz), booted through a real `baud run kvm` CLI invocation against a real running
# baud-server over real HTTP — proving both the new `--initramfs` flag and the new
# `--periodic-timer-*` flags (a real Linux kernel's own scheduler calibration hangs forever
# without periodic timer injection, unlike every hand-assembled fixture the pre-existing drive/m9.sh
# exercises) actually reach a real guest boot, not just the Rust-level `boot_run_and_drain` unit
# test in crates/baud-server/src/routes/run_kvm.rs.
#
# Uses the already-built, checked-in fixture (no kernel compile needed), so unlike
# drive/pkg-build-cli.sh / drive/pkg-image-build.sh this runs in seconds, not minutes — but it is
# still opt-in (not part of the standard h0-h7 gate) purely to keep the pkg-*/enforced-* opt-in
# convention consistent for every script under this "real KVM, real fixture" umbrella.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[pkg-boot-cli] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== baud run kvm --initramfs/--periodic-timer-*: real linux-guest, CLI/server end-to-end ==="
echo ""

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
BAUD="$REPO_ROOT/target/debug/baud"
FIXTURE_DIR="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/linux-guest"
KERNEL="$FIXTURE_DIR/bzImage"
INITRAMFS="$FIXTURE_DIR/initramfs.cpio.gz"
DB_FILE="$(mktemp -u -t baud-pkg-boot-cli-XXXXXX.sqlite)"
SERVER_PID=""

for f in "$KERNEL" "$INITRAMFS"; do
    [[ -f "$f" ]] || fail "fixture missing: $f (see $FIXTURE_DIR/BUILD.md)"
done

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" 2>/dev/null || true
}
trap cleanup EXIT

log "Building baud-server/baud-cli..."
cargo build -q -p baud-server -p baud-cli 2>&1

log "Starting baud-server (DB: $DB_FILE)..."
pkill -f "baud-server" 2>/dev/null || true; sleep 0.2
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for i in $(seq 1 30); do
    if curl -sf http://127.0.0.1:7734/health > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf http://127.0.0.1:7734/health > /dev/null || fail "baud-server did not start"
pass "baud-server is running (PID $SERVER_PID)"

# Spec §4.2's exact deterministic cmdline (bootparams::DETERMINISTIC_CMDLINE) — the same string
# guest_kernel_boots_to_userspace uses against this exact fixture.
CMDLINE="console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc tsc=reliable no-kvmclock \
no_timer_check pci=off acpi=off reboot=t panic=-1 quiet loglevel=1 printk.time=0 \
random.trust_cpu=off random.trust_bootloader=on i8042.noaux i8042.nomux i8042.nopnp \
8250.nr_uarts=1 nomodule rdinit=/init"

log "baud run kvm --kernel $KERNEL --initramfs $INITRAMFS --periodic-timer-period-rcb 500000 ..."
BOOT_JSON="$("$BAUD" run kvm \
    --kernel "$KERNEL" \
    --initramfs "$INITRAMFS" \
    --cmdline "$CMDLINE" \
    --periodic-timer-period-rcb 500000 \
    --periodic-timer-vector 236 \
    --periodic-timer-max-ticks 2000 \
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

echo ""
echo "=== baud run kvm --initramfs/--periodic-timer-*: PASSED ==="
echo ""
echo "POST /run/kvm now threads an optional initramfs_path + periodic_timer spec through to"
echo "Multiverse::boot_with_rdseed_sites/run_to_first_halt_with_periodic_timer, closing todo.md"
echo "§14 item 1's 'a baud image build-produced image cannot yet be booted through the"
echo "CLI/server path end-to-end' gap for a real, unmodified Linux kernel guest."
