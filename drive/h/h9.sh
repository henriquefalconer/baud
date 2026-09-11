#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h/h9.sh — H9 real Ubuntu cross-VM fingerprint acceptance drive
#
# The drive boots the pinned Ubuntu 18.04.1 artifacts in two independent server processes and
# compares the complete timed-exit fingerprint. It never substitutes a hand-built fixture.
#
#   H9.1  baud host probe still reports runnable=true (cheap, early sanity check)
#   H9.2  `baud verify fingerprint --times 2` on Ubuntu: two boots match through the real path
#   H9.3  an incorrect expected banner fails with exit 1 instead of a false pass
#   H9.4  Two separate `baud-server` processes (own PID, own port, own DB, own snapshot dir),
#         pinned to distinct cores when `taskset`/`nproc` allow it, each capture ONE fingerprint
#         (`--times 1`) for the identical (kernel, cmdline, tape, target_rcb); this script compares
#         events/rip/gpa/mem_hash/banner across the two processes' JSON responses directly
#   H9.5  Comparator sanity: the same two-process setup at two DIFFERENT `target_rcb` values must
#         be caught as a divergence by this script's own comparison — guards against H9.4 passing
#         vacuously (e.g. if a field were empty on both sides)

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[h9] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H9: Ubuntu fingerprint, real CLI/server end-to-end ==="
echo ""

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
BAUD="$REPO_ROOT/target/debug/baud"
UBUNTU_OUT="${BAUD_UBUNTU_OUT:-$HOME/.baud-tmp/ubuntu-1804}"
KERNEL="${BAUD_UBUNTU_KERNEL:-$UBUNTU_OUT/vmlinuz-generic}"
ROOTFS="${BAUD_UBUNTU_ROOTFS:-$UBUNTU_OUT/rootfs.raw}"
INITRAMFS="${BAUD_UBUNTU_INITRAMFS:-$UBUNTU_OUT/initrd-generic}"
EXPECTED_BANNER=$'Ubuntu 18.04.1 LTS ubuntu ttyS0\n\nubuntu login: '
UBUNTU_CMDLINE="systemd.unit=multi-user.target cloud-init=disabled console=ttyS0 root=/dev/vda1 ro rootwait nokaslr nosmp maxcpus=1 net.ifnames=0 biosdevname=0 clocksource=tsc tsc=reliable no-kvmclock no_timer_check pci=conf1 pci=assign-busses acpi=on scsi_mod.scan=sync udev.children_max=1 fsck.mode=skip systemd.mask=systemd-timesyncd.service systemd.mask=systemd-time-wait-sync.service systemd.mask=systemd-random-seed.service systemd.mask=systemd-networkd.service systemd.mask=systemd-networkd-wait-online.service systemd.mask=systemd-resolved.service systemd.mask=cloud-init-local.service systemd.mask=cloud-init.service systemd.mask=cloud-config.service systemd.mask=cloud-final.service systemd.mask=apt-daily.timer systemd.mask=apt-daily-upgrade.timer systemd.mask=motd-news.timer systemd.mask=man-db.timer systemd.mask=fstrim.timer systemd.mask=systemd-tmpfiles-clean.timer systemd.mask=snapd.service systemd.mask=snapd.socket systemd.mask=snapd.seeded.service i8042.noaux i8042.nomux i8042.nopnp 8250.nr_uarts=1"
DB_FILE="$(mktemp -u -t baud-h9-XXXXXX.sqlite)"
SNAP_ROOT="$(mktemp -d -t baud-h9-snap-XXXXXX)"
SERVER_PID=""

# H9.4/H9.5's two genuinely separate baud-server OS processes — own PID, own port, own DB, own
# snapshot dir each, so they share nothing but the kernel image and are indistinguishable (from
# this script's vantage point) from two independent hosts.
VM0_DB_FILE="$(mktemp -u -t baud-h9-vm0-XXXXXX.sqlite)"
VM1_DB_FILE="$(mktemp -u -t baud-h9-vm1-XXXXXX.sqlite)"
VM0_SNAP_ROOT="$(mktemp -d -t baud-h9-vm0-snap-XXXXXX)"
VM1_SNAP_ROOT="$(mktemp -d -t baud-h9-vm1-snap-XXXXXX)"
VM0_PID=""
VM1_PID=""

[[ -f "$KERNEL" ]] || fail "real Ubuntu kernel missing: $KERNEL (run examples/ubuntu/fetch.sh or set BAUD_UBUNTU_OUT/BAUD_UBUNTU_KERNEL)"
[[ -f "$ROOTFS" ]] || fail "real Ubuntu rootfs missing: $ROOTFS (run examples/ubuntu/fetch.sh or set BAUD_UBUNTU_OUT/BAUD_UBUNTU_ROOTFS)"
[[ -f "$INITRAMFS" ]] || fail "real Ubuntu initramfs missing: $INITRAMFS (run examples/ubuntu/fetch.sh or set BAUD_UBUNTU_OUT/BAUD_UBUNTU_INITRAMFS)"

# Own port + own snapshot store, so this script can run concurrently with any other drive script.
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"

VM0_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')"
VM1_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')"
VM0_SRV="http://127.0.0.1:$VM0_PORT"
VM1_SRV="http://127.0.0.1:$VM1_PORT"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ -n "${VM0_PID:-}" ]]; then
        kill "$VM0_PID" 2>/dev/null || true
        wait "$VM0_PID" 2>/dev/null || true
    fi
    if [[ -n "${VM1_PID:-}" ]]; then
        kill "$VM1_PID" 2>/dev/null || true
        wait "$VM1_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" "$VM0_DB_FILE" "$VM1_DB_FILE" 2>/dev/null || true
    rm -rf "$SNAP_ROOT" "$VM0_SNAP_ROOT" "$VM1_SNAP_ROOT" 2>/dev/null || true
}
trap cleanup EXIT
# bash does NOT run an EXIT trap when it dies from an untrapped signal, so `trap cleanup EXIT`
# alone leaks this script's baud-server, temp DB and snapshot dir whenever the script is
# interrupted -- Ctrl-C, or drive/gate.sh reaping its pool.
trap 'exit 130' INT
trap 'exit 143' TERM

log "Building baud-host/baud-server/baud-cli..."
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q -p baud-host -p baud-server -p baud-cli 2>&1
fi

log "Starting baud-server on $SRV..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$BAUD_PORT" \
    BAUD_SNAPSHOT_STORE="$SNAP_ROOT" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "$SRV/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf "$SRV/health" > /dev/null 2>&1 || fail "baud-server did not come up on $SRV"

# ---------------------------------------------------------------------------
# H9.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# ---------------------------------------------------------------------------
log "baud host probe --json"
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H9.1: 'baud host probe --json' FAILED to run"
RUNNABLE="$(echo "$PROBE_JSON" | grep -oE '"runnable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
if [[ "$RUNNABLE" != "true" ]]; then
    fail "H9.1: host probe runnable is '$RUNNABLE' — no real /dev/kvm, H9 cannot mean anything here."
fi
pass "H9.1: host probe runnable='$RUNNABLE' (real KVM present)"

# ---------------------------------------------------------------------------
# H9.2 — two independent boots must produce matching fingerprints
# ---------------------------------------------------------------------------
log "baud verify fingerprint --kernel $KERNEL --target-rcb 100000000 --times 2 ..."
FP_JSON="$("$BAUD" verify fingerprint \
    --kernel "$KERNEL" \
    --cmdline "$UBUNTU_CMDLINE" \
    --initramfs "$INITRAMFS" \
    --target-rcb 100000000 \
    --periodic-timer-period-rcb 500000 \
    --periodic-timer-vector 238 \
    --periodic-timer-max-ticks 20000 \
    --virtio-blk-image "$ROOTFS" \
    --expected-banner "$EXPECTED_BANNER" \
    --times 2 \
    --json 2>&1)" || { echo "$FP_JSON"; fail "H9.2: 'baud verify fingerprint' FAILED to run"; }
echo "$FP_JSON"

OK="$(echo "$FP_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")"
[[ "$OK" == "True" ]] || fail "H9.2: 'baud verify fingerprint' reported ok!=true: $FP_JSON"
DIVERGENCE="$(echo "$FP_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('divergence'))")"
[[ "$DIVERGENCE" == "None" ]] || fail "H9.2: unexpected divergence reported: $DIVERGENCE"
pass "H9.2: two independent Ubuntu boots produced matching fingerprints through the real CLI/server path"

# ---------------------------------------------------------------------------
# H9.3 — a banner the guest never prints must fail the capture, not silently pass
# ---------------------------------------------------------------------------
log "baud verify fingerprint --expected-banner '<never printed>' (must fail)..."
set +e
BAD_FP_JSON="$("$BAUD" verify fingerprint \
    --kernel "$KERNEL" \
    --cmdline "$UBUNTU_CMDLINE" \
    --initramfs "$INITRAMFS" \
    --target-rcb 100000000 \
    --virtio-blk-image "$ROOTFS" \
    --expected-banner "a banner Ubuntu never prints" \
    --times 2 \
    --json)"
BAD_STATUS=$?
set -e
echo "$BAD_FP_JSON"
[[ "$BAD_STATUS" -ne 0 ]] || fail "H9.3: 'baud verify fingerprint' with an unreachable banner exited 0"
echo "$BAD_FP_JSON" | grep -q "NoBanner\|did not reach the expected banner" \
    || fail "H9.3: failure did not name the real cause: $BAD_FP_JSON"
pass "H9.3: an unreachable expected banner fails the capture (exit $BAD_STATUS), not a silent false pass"

# ---------------------------------------------------------------------------
# H9.4/H9.5 — true cross-process/cross-core orchestration: two separate baud-server OS processes,
# each capturing one fingerprint, compared here in bash — never inside one Rust process.
# ---------------------------------------------------------------------------
fp_field() {
    # $1 = full /verify/fingerprint JSON response, $2 = field name within fingerprints[0]
    python3 -c "
import sys, json
d = json.loads(sys.argv[1])
print(d['fingerprints'][0][sys.argv[2]])
" "$1" "$2"
}

NPROC="$(nproc 2>/dev/null || echo 1)"
TASKSET0=()
TASKSET1=()
if command -v taskset > /dev/null 2>&1 && [[ "$NPROC" -ge 2 ]]; then
    TASKSET0=(taskset -c 0)
    TASKSET1=(taskset -c 1)
    log "H9.4: pinning vm0 -> core 0, vm1 -> core 1 (nproc=$NPROC)"
else
    log "H9.4: taskset unavailable or nproc<2 (nproc=$NPROC) — vm0/vm1 remain separate processes, just not pinned to distinct cores"
fi

log "Starting vm0 baud-server on $VM0_SRV (own process)..."
BAUD_DB="sqlite://${VM0_DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$VM0_PORT" \
    BAUD_SNAPSHOT_STORE="$VM0_SNAP_ROOT" BAUD_LOG=warn "${TASKSET0[@]}" "$BAUD_SERVER_BIN" &
VM0_PID=$!

log "Starting vm1 baud-server on $VM1_SRV (own process)..."
BAUD_DB="sqlite://${VM1_DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$VM1_PORT" \
    BAUD_SNAPSHOT_STORE="$VM1_SNAP_ROOT" BAUD_LOG=warn "${TASKSET1[@]}" "$BAUD_SERVER_BIN" &
VM1_PID=$!

for SRV_URL in "$VM0_SRV" "$VM1_SRV"; do
    for _ in $(seq 1 60); do
        if curl -sf "$SRV_URL/health" > /dev/null 2>&1; then
            break
        fi
        sleep 0.2
    done
    curl -sf "$SRV_URL/health" > /dev/null 2>&1 || fail "H9.4: baud-server did not come up on $SRV_URL"
done
pass "H9.4: vm0 (pid $VM0_PID) and vm1 (pid $VM1_PID) are two separate baud-server processes, each with its own port/DB/snapshot store"

log "vm0 - timed exit: capturing single fingerprint (--times 1) at target_rcb=100000000..."
VM0_FP_JSON="$(BAUD_SERVER="$VM0_SRV" "$BAUD" verify fingerprint \
    --kernel "$KERNEL" \
    --cmdline "$UBUNTU_CMDLINE" \
    --initramfs "$INITRAMFS" \
    --target-rcb 100000000 \
    --periodic-timer-period-rcb 500000 \
    --periodic-timer-vector 238 \
    --periodic-timer-max-ticks 20000 \
    --virtio-blk-image "$ROOTFS" \
    --expected-banner "$EXPECTED_BANNER" \
    --times 1 \
    --json)" || fail "H9.4: vm0 'baud verify fingerprint --times 1' FAILED to run"
VM0_EVENTS="$(fp_field "$VM0_FP_JSON" events)"
VM0_RIP="$(fp_field "$VM0_FP_JSON" rip)"
VM0_GPA="$(fp_field "$VM0_FP_JSON" gpa)"
VM0_HASH="$(fp_field "$VM0_FP_JSON" mem_hash)"
VM0_BANNER="$(fp_field "$VM0_FP_JSON" banner_hex)"
echo "$EXPECTED_BANNER"
echo ""
echo "vm0 - timed exit:"
echo "deterministic events = $VM0_EVENTS"
echo "guest RIP = $VM0_RIP (-> guest physical = $VM0_GPA)"
echo "guest memory hash = $VM0_HASH"
echo "vm0: done"

log "vm1 - timed exit: capturing single fingerprint (--times 1) at target_rcb=100000000 (own OS process, own port)..."
VM1_FP_JSON="$(BAUD_SERVER="$VM1_SRV" "$BAUD" verify fingerprint \
    --kernel "$KERNEL" \
    --cmdline "$UBUNTU_CMDLINE" \
    --initramfs "$INITRAMFS" \
    --target-rcb 100000000 \
    --periodic-timer-period-rcb 500000 \
    --periodic-timer-vector 238 \
    --periodic-timer-max-ticks 20000 \
    --virtio-blk-image "$ROOTFS" \
    --expected-banner "$EXPECTED_BANNER" \
    --times 1 \
    --json)" || fail "H9.4: vm1 'baud verify fingerprint --times 1' FAILED to run"
VM1_EVENTS="$(fp_field "$VM1_FP_JSON" events)"
VM1_RIP="$(fp_field "$VM1_FP_JSON" rip)"
VM1_GPA="$(fp_field "$VM1_FP_JSON" gpa)"
VM1_HASH="$(fp_field "$VM1_FP_JSON" mem_hash)"
VM1_BANNER="$(fp_field "$VM1_FP_JSON" banner_hex)"
echo ""
echo "vm1 - timed exit:"
echo "deterministic events = $VM1_EVENTS"
echo "guest RIP = $VM1_RIP (-> guest physical = $VM1_GPA)"
echo "guest memory hash = $VM1_HASH"
echo "vm1: done"
echo ""

[[ -n "$VM0_HASH" && "$VM0_HASH" != "None" ]] || fail "H9.4: vm0 mem_hash empty/None — capture must have failed silently"
[[ "$VM0_EVENTS" == "$VM1_EVENTS" ]] || fail "H9.4: events diverged across processes: vm0=$VM0_EVENTS vm1=$VM1_EVENTS"
[[ "$VM0_RIP" == "$VM1_RIP" ]] || fail "H9.4: RIP diverged across processes: vm0=$VM0_RIP vm1=$VM1_RIP"
[[ "$VM0_GPA" == "$VM1_GPA" ]] || fail "H9.4: guest physical address diverged across processes: vm0=$VM0_GPA vm1=$VM1_GPA"
[[ "$VM0_HASH" == "$VM1_HASH" ]] || fail "H9.4: guest memory hash diverged across processes: vm0=$VM0_HASH vm1=$VM1_HASH"
[[ "$VM0_BANNER" == "$VM1_BANNER" ]] || fail "H9.4: console banner diverged across processes: vm0=$VM0_BANNER vm1=$VM1_BANNER"
pass "H9.4: two separate baud-server OS processes ($([[ ${#TASKSET0[@]} -gt 0 ]] && echo "pinned to distinct cores" || echo "unpinned, taskset unavailable")) produced a byte-identical fingerprint, compared in this script — never inside one Rust process"

log "H9.5: comparator sanity — a second real capture at a different target_rcb must diverge..."
# Exercise the same public path at a distinct retired-branch target. Comparing a fabricated hash
# would only test bash string inequality, not the fingerprint comparator's real inputs.
VM1_ALT_JSON="$(BAUD_SERVER="$VM1_SRV" "$BAUD" verify fingerprint \
    --kernel "$KERNEL" \
    --cmdline "$UBUNTU_CMDLINE" \
    --initramfs "$INITRAMFS" \
    --target-rcb 100000001 \
    --periodic-timer-period-rcb 500000 \
    --periodic-timer-vector 238 \
    --periodic-timer-max-ticks 20000 \
    --virtio-blk-image "$ROOTFS" \
    --expected-banner "$EXPECTED_BANNER" \
    --times 1 \
    --json)" || fail "H9.5: alternate target_rcb capture FAILED"
VM1_ALT_EVENTS="$(fp_field "$VM1_ALT_JSON" events)"
VM1_ALT_RIP="$(fp_field "$VM1_ALT_JSON" rip)"
VM1_ALT_GPA="$(fp_field "$VM1_ALT_JSON" gpa)"
VM1_ALT_HASH="$(fp_field "$VM1_ALT_JSON" mem_hash)"
VM1_ALT_BANNER="$(fp_field "$VM1_ALT_JSON" banner_hex)"
if [[ "$VM1_ALT_EVENTS" == "$VM1_EVENTS" && "$VM1_ALT_RIP" == "$VM1_RIP" && "$VM1_ALT_GPA" == "$VM1_GPA" && "$VM1_ALT_HASH" == "$VM1_HASH" && "$VM1_ALT_BANNER" == "$VM1_BANNER" ]]; then
    fail "H9.5: changing target_rcb did not change any compared execution field"
fi
pass "H9.5: a second real capture at target_rcb=100001 diverged from target_rcb=100000"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H9 milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (runnable=$RUNNABLE):"
echo "  - POST /verify/fingerprint (crates/baud-server/src/routes/verify_fingerprint.rs) boots a"
echo "    guest N times, captures a timed-exit fingerprint from each (baud-fingerprint's capture()),"
echo "    and compares them, closing todo.md §14 item 9's 'baud verify fingerprint CLI/HTTP route'"
echo "    gap"
echo "  - 'baud verify fingerprint' (crates/baud-cli/src/cmds/verify.rs) drives it end-to-end over"
echo "    real HTTP, exit code 0 on a match and 1 on a divergence or capture error"
echo "  - A banner the guest never reaches fails the whole call loud (FpError::NoBanner), never a"
echo "    fingerprint for the wrong point"
echo "  - H9.4: two genuinely separate baud-server OS processes (own PID/port/DB/snapshot-store,"
echo "    pinned to distinct CPU cores via taskset when available) each capture one fingerprint"
echo "    (--times 1) and this script — not any single Rust process — proves them byte-identical,"
echo "    closing the true cross-process/cross-core orchestration gap todo.md §14 items 9/10 named"
echo "    as still open"
echo "  - H9.5: a second real fingerprint capture at a different target_rcb diverges, so H9.4's"
echo "    equality comparison is not vacuous"
echo ""
echo "The drive requires the verified Ubuntu kernel/rootfs artifacts and fails closed when the"
echo "real boot path cannot reach the exact login banner."
