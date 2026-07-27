#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg/pkg-blk-cancel-cli.sh — regression test for the orphaned-run bug: a `POST /run/kvm`
# whose client goes away must be *cancelled* server-side, and everything that run was holding —
# the vCPU, the KVM VM, the guest RAM, the virtio-blk base image and its overlay, and above all
# the CPU it is burning — must be released. The user-facing requirement is simply "when the CLI is
# closed, however that happens, all its resources are freed"; this script asserts the observable
# consequences of that.
#
# WHAT THIS PROVES / WHY IT EXISTS. `crates/baud-server/src/routes/run_kvm.rs` runs the whole boot
# inside `tokio::task::spawn_blocking(...).await`. Dropping the response future (which is what
# hyper/axum do the moment the client's socket goes away) does NOT stop a `spawn_blocking` closure
# — the blocking pool thread runs to completion regardless. So a `baud run kvm` client that is
# killed mid-request leaves a full KVM run executing on the server: a vCPU thread spinning at 100%
# of a core inside `KVM_RUN`, holding its whole allocation, with nothing left that will ever read
# the result.
#
# WHY THIS NO LONGER ASSERTS ON RSS (the previous version of this script did, and was right to at
# the time). The original bug report was a memory report: the image was `std::fs::read` + `.to_vec`
# so a 512 MiB disk cost 2.02x its size on the heap, server RSS pinned at 4676 MiB after the client
# was confirmed dead, and a second request on top of that orphan drove the host to 349 MiB
# available and terminated the WSL VM. `BlockBase::mapped` (an `mmap` of the image, page-cache
# backed) has since landed and took the image's heap cost to ~0. MEASURED after the fix: this exact
# workload now costs the server ~20-22 MiB of RSS in total — baseline 21 MiB, peak 44 MiB — so the
# old "+256 MiB proves the run is underway" gate could no longer be met, and "RSS came back down"
# could no longer prove a cancellation. That failure *is* the evidence the mmap fix works; it is
# not evidence about cancellation either way. RSS is therefore reported here for information only,
# never asserted on.
#
# WHAT IT ASSERTS INSTEAD, in observable terms only (no implementation detail, so this script keeps
# working whatever shape the fix takes — an `is_cancelled` poll in the boundary walk, an armed
# `baud_vcpu::linux::watchdog` signal, an aborted blocking handle, a supervisor task, anything):
#
#   1. baseline: the server's CPU rate (`utime+stime` from /proc/<pid>/stat, sampled over a fixed
#      window) is ~idle, and it holds zero KVM file descriptors,
#   2. fire a `baud run kvm` whose guest never reaches a halt on its own (see WORKLOAD below), so
#      the run cannot end by itself and confound the measurement,
#   3. the run is underway: the server's CPU rate climbs to most of a full core AND it now holds
#      KVM fds (/dev/kvm plus anon_inode:kvm-vm / anon_inode:kvm-vcpu) — a live guest, executing,
#   4. SIGTERM the client and reap it, so the server's socket is definitively gone,
#   5. THE KEY ASSERTION: within a bounded grace period the server's CPU rate must fall back to
#      ~idle and stay there for two consecutive windows. This directly catches "the abandoned run
#      is still spinning a core", which is the resource that actually matters now that the image
#      costs no heap,
#   6. and its KVM fds must be back to zero — the vCPU/VM were really torn down, not merely left
#      idle,
#   7. and the server must immediately accept and COMPLETE a fresh run, in normal time. A server
#      that merely survived but never released the vCPU would fail here even if it looked idle,
#   8. and it must still be healthy on /health: cancelling one abandoned run must not take down the
#      process that hosts every other run.
#
# WHY THE CPU ASSERTION MAY LEGITIMATELY FAIL TODAY. Client disconnect *is* detected — the
# handler's `CancelGuard::drop` fires 4 ms after the client dies, measured over real HTTP. What is
# not yet prompt is the other half: the flag is only polled once per periodic tick, and a tick can
# exceed 120 s because the vCPU sits inside long `KVM_RUN` ioctls (measured on this host with this
# exact workload: `--periodic-timer-max-ticks 4` finishes in 0.31 s, `8` does not finish in 120 s;
# 8 ioctls took 5 s at 100% CPU). So until the run loop can be interrupted inside the boundary
# walk, a cancelled run keeps burning a full core and step 5 fails — with the real numbers printed.
#
# WORKLOAD. Every `tests/fixtures/linux-guest` /init ends in `reboot(RB_POWER_OFF)`, which with
# `acpi=off` lands the guest in `System halted` — a terminal `Hlt` about 1.2s after the request
# starts, far too short to kill a client "mid-run" against, and a run that ends on its own proves
# nothing about cancellation. So this script deliberately boots the same checked-in bzImage +
# virtio_blk initramfs with `rdinit=` pointed at a path that does not exist and `panic=0`: the
# kernel panics on "no working init found" and, with a zero panic timeout, spins in `panic()`'s own
# busy loop forever instead of halting. The periodic-timer + virtio-blk run loop has no wall-clock
# watchdog of its own (unlike plain `run_to_first_halt`), so the run genuinely never terminates by
# itself, and it burns a core the entire time. That is exactly the property this test needs: the
# CPU burn can only stop because the run was *cancelled*, never because it happened to finish.
# (The fresh run in step 7 uses the ordinary `rdinit=/init` workload, which is supposed to finish.)
#
# 512 MiB, NOT more: this box has 7.98 GB total and a careless version of this experiment already
# killed the WSL VM twice. `truncate` keeps the image sparse, so it costs ~4 KiB on /tmp's tmpfs
# while still forcing the server down the real `--virtio-blk-image` mapping path. The image is kept
# at that size so the path is genuinely exercised end to end; nothing is asserted about its memory.

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[pkg-blk-cancel-cli] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== POST /run/kvm: a killed client's run must be cancelled, not left burning a core ==="
echo ""

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
BAUD="$REPO_ROOT/target/debug/baud"
FIXTURE_DIR="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/linux-guest"
KERNEL="$FIXTURE_DIR/bzImage"
INITRAMFS="$FIXTURE_DIR/virtio_blk_initramfs.cpio.gz"
DB_FILE="$(mktemp -u -t baud-pkg-blk-cancel-cli-XXXXXX.sqlite)"
SNAP_ROOT="$(mktemp -d -t baud-pkg-blk-cancel-cli-snap-XXXXXX)"
BLK_IMAGE="$(mktemp -t baud-pkg-blk-cancel-cli-disk-XXXXXX.img)"
CLI_OUT="$(mktemp -t baud-pkg-blk-cancel-cli-out-XXXXXX.json)"
SERVER_PID=""
CLI_PID=""

# Ephemeral port + per-script snapshot store, so this script can run concurrently with any other
# drive/*.sh (each server gets its own port, its own SQLite file and its own SnapshotStore root).
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"

# CPU thresholds, expressed as percent of ONE core over a fixed sampling window. A live guest in
# this workload pins a core (100), an idle baud-server is ~0, so both thresholds sit far from
# anything either state actually produces and neither can be tripped by scheduler noise.
CLK_TCK="$(getconf CLK_TCK)"
CPU_WINDOW_S=1        # length of one utime+stime sampling window, seconds (integer)
CPU_BUSY_PCT=50       # >= half a core sustained => the run is genuinely executing guest code
CPU_IDLE_PCT=10       # <= a tenth of a core => nothing is spinning on this server any more
IDLE_WINDOWS=2        # consecutive idle windows required, so one scheduling gap cannot fake it
BUSY_TIMEOUT_S=60     # generous: the request has to boot a real kernel first
GRACE_S=30            # how long a correct implementation gets to notice and unwind
FRESH_RUN_TIMEOUT_S=90  # hard cap on the post-cancellation run that must complete normally

for f in "$KERNEL" "$INITRAMFS"; do
    [[ -f "$f" ]] || fail "fixture missing: $f (see $FIXTURE_DIR/BUILD.md)"
done

cleanup() {
    # Kill only PIDs this script captured itself. NEVER pkill/killall with a pattern: drive/
    # gate.test.bats asserts no gate-scope script does that, and it bit this very experiment --
    # `pkill -f target/debug/baud-server` matched the killing shell's own command line and took
    # the shell down with it.
    if [[ -n "${CLI_PID:-}" ]]; then
        kill "$CLI_PID" 2>/dev/null || true
        wait "$CLI_PID" 2>/dev/null || true
    fi
    # The server is killed unconditionally, which is also what reaps any still-running orphaned
    # run: pre-fix, that spawn_blocking thread only ever stops when its process does.
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" "$BLK_IMAGE" "$CLI_OUT" 2>/dev/null || true
    rm -rf "$SNAP_ROOT" 2>/dev/null || true
}
trap cleanup EXIT
# bash does NOT run an EXIT trap when it dies from an untrapped signal, so `trap cleanup EXIT`
# alone leaks this script's baud-server, temp DB/disk-image and snapshot dir whenever the script
# is interrupted -- Ctrl-C, or drive/gate.sh reaping its pool. Re-raising through `exit` makes the
# EXIT trap fire, so cleanup() runs on every exit path. That matters more here than in any sibling
# script: the server this one leaks would be leaking a whole running KVM guest with it.
trap 'exit 130' INT
trap 'exit 143' TERM

# Cumulative CPU the server has consumed, in clock ticks (utime+stime), or "" if it is gone. The
# ${stat#*) } strip is deliberate: /proc/<pid>/stat's comm field is parenthesised and may itself
# contain spaces, so positional awk on the raw line is not safe -- after the strip, field 12 is
# utime and 13 is stime.
cpu_ticks() {
    local stat rest
    [[ -n "${SERVER_PID:-}" ]] || return 0
    read -r stat < "/proc/$SERVER_PID/stat" 2>/dev/null || return 0
    rest="${stat#*) }"
    echo "$rest" | awk '{ print $12 + $13 }'
}

# One sampling window: percent of a single core the server burned over CPU_WINDOW_S. "" if the
# server died mid-window.
sample_cpu_pct() {
    local before after
    before="$(cpu_ticks)"
    [[ -n "$before" ]] || { echo ""; return 0; }
    sleep "$CPU_WINDOW_S"
    after="$(cpu_ticks)"
    [[ -n "$after" ]] || { echo ""; return 0; }
    echo $(( (after - before) * 100 / (CLK_TCK * CPU_WINDOW_S) ))
}

# How many KVM handles the server holds open right now: /dev/kvm itself plus the anon inodes KVM
# hands back for a VM and each vCPU. Scoped to OUR server's PID on purpose -- a global `lsof
# /dev/kvm` sweep would see any sibling drive/*.sh the gate is running concurrently and go flaky,
# and this is the stronger signal anyway: it says the run's VM and vCPU were actually torn down,
# not merely that some process somewhere closed the device node.
server_kvm_fds() {
    local fd tgt n=0
    [[ -n "${SERVER_PID:-}" ]] || { echo ""; return 0; }
    [[ -d "/proc/$SERVER_PID/fd" ]] || { echo ""; return 0; }
    for fd in "/proc/$SERVER_PID/fd/"*; do
        tgt="$(readlink "$fd" 2>/dev/null || true)"
        case "$tgt" in
            /dev/kvm|*kvm-vm*|*kvm-vcpu*) n=$((n + 1)) ;;
        esac
    done
    echo "$n"
}

# VmRSS in KiB, or "" if the process is gone. Reported only -- see the RSS note in the header.
server_rss_kib() {
    [[ -n "${SERVER_PID:-}" ]] || return 0
    awk '/^VmRSS:/ { print $2 }' "/proc/$SERVER_PID/status" 2>/dev/null || true
}

mib() { python3 -c "import sys; print(round(int(sys.argv[1]) / 1024))" "$1"; }

# Same fixed formula tests/fixtures/linux-guest/virtio_blk_init.c's own /init expects at sector 0
# (crates/baud-multiverse/src/linux/mod.rs's virtio_blk_test_base_image): byte i is i % 256,
# repeating every 256 bytes -- 4 sectors, exactly as drive/pkg/pkg-boot-virtio-blk-cli.sh writes
# it. The remaining 512 MiB is sparse zeroes.
python3 -c "
import sys
sector_size = 512
sectors = 4
with open(sys.argv[1], 'wb') as f:
    f.write(bytes(i % 256 for i in range(sector_size * sectors)))
" "$BLK_IMAGE"
truncate -s 512M "$BLK_IMAGE"
pass "512 MiB virtio-blk image built (first 4 sectors = the i%256 pattern /init expects)"

log "Building baud-server/baud-cli..."
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q -p baud-server -p baud-cli 2>&1
fi

log "Starting baud-server (DB: $DB_FILE, port: $BAUD_PORT)..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$BAUD_PORT" \
    BAUD_SNAPSHOT_STORE="$SNAP_ROOT" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "$SRV/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf "$SRV/health" > /dev/null || fail "baud-server did not start"
pass "baud-server is running (PID $SERVER_PID)"

BASE_CPU_PCT="$(sample_cpu_pct)"
[[ -n "$BASE_CPU_PCT" ]] || fail "could not sample CPU for baud-server PID $SERVER_PID"
(( BASE_CPU_PCT <= CPU_IDLE_PCT )) || fail \
    "baud-server is already burning ${BASE_CPU_PCT}% of a core with no run in flight (idle must be \
<= ${CPU_IDLE_PCT}%) -- something else is running on this server, so a later 'the burn stopped' \
measurement would be meaningless"
BASE_KVM_FDS="$(server_kvm_fds)"
[[ "$BASE_KVM_FDS" == "0" ]] || fail \
    "baud-server already holds $BASE_KVM_FDS KVM fd(s) before any run was requested"
BASELINE_KIB="$(server_rss_kib)"
pass "baseline: CPU ${BASE_CPU_PCT}% of a core, 0 KVM fds, RSS $(mib "$BASELINE_KIB") MiB (no run in flight)"

# Spec §4.2's deterministic cmdline with `pci=off` stripped (a virtio-pci device needs real PCI
# enumeration to be found at all), plus the two deliberate changes described in WORKLOAD above:
# `rdinit=` at a path that does not exist and `panic=0`, so the guest busy-loops in panic()
# forever rather than reaching the terminal `System halted` about a second in.
CMDLINE_BASE="console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc tsc=reliable no-kvmclock \
no_timer_check acpi=off reboot=t quiet loglevel=1 printk.time=0 \
random.trust_cpu=off random.trust_bootloader=on i8042.noaux i8042.nomux i8042.nopnp \
8250.nr_uarts=1 nomodule"
CMDLINE_NEVER_HALTS="$CMDLINE_BASE panic=0 rdinit=/baud-no-such-init-on-purpose"
CMDLINE_NORMAL="$CMDLINE_BASE panic=-1 rdinit=/init"

log "baud run kvm --virtio-blk-image $BLK_IMAGE (backgrounded; guest never halts on its own)..."
"$BAUD" run kvm \
    --kernel "$KERNEL" \
    --initramfs "$INITRAMFS" \
    --cmdline "$CMDLINE_NEVER_HALTS" \
    --periodic-timer-period-rcb 500000 \
    --periodic-timer-vector 236 \
    --periodic-timer-max-ticks 100000000 \
    --virtio-blk-image "$BLK_IMAGE" \
    --virtio-blk-vector 59 \
    --json > "$CLI_OUT" 2>&1 &
CLI_PID=$!
pass "'baud run kvm' client started (PID $CLI_PID)"

# The run is underway when the server is actually executing guest code -- a pinned core plus live
# KVM handles. Both are needed: fds alone could be a VM that was created and never entered, CPU
# alone could be any other server work.
PEAK_CPU_PCT=0
PEAK_RSS_KIB="$BASELINE_KIB"
BUSY_KVM_FDS=0
for _ in $(seq 1 "$BUSY_TIMEOUT_S"); do
    NOW_PCT="$(sample_cpu_pct)"
    [[ -n "$NOW_PCT" ]] || fail "baud-server died while the run was starting"
    NOW_RSS="$(server_rss_kib)"
    if [[ -n "$NOW_RSS" ]] && (( NOW_RSS > PEAK_RSS_KIB )); then PEAK_RSS_KIB="$NOW_RSS"; fi
    if (( NOW_PCT > PEAK_CPU_PCT )); then PEAK_CPU_PCT="$NOW_PCT"; fi
    NOW_FDS="$(server_kvm_fds)"
    if [[ -n "$NOW_FDS" ]] && (( NOW_FDS > BUSY_KVM_FDS )); then BUSY_KVM_FDS="$NOW_FDS"; fi
    if (( NOW_PCT >= CPU_BUSY_PCT )) && (( BUSY_KVM_FDS > 0 )); then break; fi
    kill -0 "$CLI_PID" 2>/dev/null \
        || fail "'baud run kvm' exited before the run ever got going (peak CPU ${PEAK_CPU_PCT}% of \
a core, peak KVM fds $BUSY_KVM_FDS); output:
$(cat "$CLI_OUT")"
done
(( PEAK_CPU_PCT >= CPU_BUSY_PCT )) || fail \
    "server never reached ${CPU_BUSY_PCT}% of a core within ${BUSY_TIMEOUT_S}s (peak \
${PEAK_CPU_PCT}%) -- the guest never really ran, so this script is not testing what it claims to. \
Fix the request, not the assertion."
(( BUSY_KVM_FDS > 0 )) || fail \
    "server never opened a KVM handle within ${BUSY_TIMEOUT_S}s -- no VM was ever created, so \
there is nothing to cancel. Fix the request, not the assertion."
pass "run is underway: server at ${PEAK_CPU_PCT}% of a core, holding $BUSY_KVM_FDS KVM fd(s)"
log "FYI, not asserted: RSS peaked at $(mib "$PEAK_RSS_KIB") MiB vs $(mib "$BASELINE_KIB") MiB \
baseline -- the 512 MiB image is mmapped (BlockBase::mapped), so it costs the heap ~nothing."

log "SIGTERMing the client (PID $CLI_PID) mid-run..."
kill -TERM "$CLI_PID" 2>/dev/null || true
wait "$CLI_PID" 2>/dev/null || true
for _ in $(seq 1 25); do
    kill -0 "$CLI_PID" 2>/dev/null || break
    sleep 0.2
done
kill -0 "$CLI_PID" 2>/dev/null && fail "client PID $CLI_PID survived SIGTERM; nothing was orphaned yet"
DEAD_CLI_PID="$CLI_PID"
CLI_PID=""
pass "client PID $DEAD_CLI_PID is dead and reaped -- its socket to baud-server is gone"

# THE KEY ASSERTION. Nobody is left to receive this run's result, so a correct server stops it --
# and the loudest, most user-visible consequence of NOT stopping it is that the abandoned run keeps
# a whole core pinned. Poll in windows rather than sleeping out the grace period, so a working
# implementation reports how fast it actually unwound.
IDLE_STREAK=0
STOPPED_AFTER=""
LAST_PCT=""
WINDOWS_USED=0
for i in $(seq 1 "$GRACE_S"); do
    LAST_PCT="$(sample_cpu_pct)"
    WINDOWS_USED="$i"
    [[ -n "$LAST_PCT" ]] || fail "baud-server died after the client was killed -- cancelling one \
abandoned run must not take the whole server down"
    if (( LAST_PCT <= CPU_IDLE_PCT )); then
        IDLE_STREAK=$((IDLE_STREAK + 1))
        if (( IDLE_STREAK >= IDLE_WINDOWS )); then
            STOPPED_AFTER=$((i * CPU_WINDOW_S))
            break
        fi
    else
        IDLE_STREAK=0
    fi
done

if [[ -z "$STOPPED_AFTER" ]]; then
    fail "ORPHANED RUN STILL BURNING CPU: the client is dead but baud-server is still executing \
its run $((WINDOWS_USED * CPU_WINDOW_S))s later.
    CPU while the run was live : ${PEAK_CPU_PCT}% of a core
    CPU in the last window     : ${LAST_PCT}% of a core (must be <= ${CPU_IDLE_PCT}% for \
${IDLE_WINDOWS} consecutive ${CPU_WINDOW_S}s windows)
    KVM fds held               : $(server_kvm_fds)
The disconnect itself IS seen -- the handler's CancelGuard drops 4 ms after the client dies. What
is missing is a run loop that can act on it: the flag is only polled once per periodic tick, and
the vCPU spends a tick inside long KVM_RUN ioctls (measured on this host with this exact workload:
--periodic-timer-max-ticks 4 finishes in 0.31 s, 8 does not finish in 120 s; 8 ioctls took 5 s at
100% CPU). Until the boundary walk polls is_cancelled -- or baud_vcpu::linux::watchdog's signal is
armed for cancellation, the only thing that can interrupt a KVM_RUN at all -- an abandoned run
keeps a full core for as long as the server lives."
fi
pass "the abandoned run's CPU burn stopped within ${STOPPED_AFTER}s (last window ${LAST_PCT}% of a core)"

FINAL_KVM_FDS="$(server_kvm_fds)"
[[ "$FINAL_KVM_FDS" == "0" ]] || fail \
    "baud-server still holds $FINAL_KVM_FDS KVM fd(s) (/dev/kvm, kvm-vm or kvm-vcpu) after the \
cancellation -- the run stopped executing but its VM/vCPU were never torn down, so the allocation \
is still committed"
pass "server holds 0 KVM fds again -- the VM and its vCPU were really torn down"

# Surviving is not the same as recovering. A fresh run completing in normal time is the strongest
# end-to-end evidence that the previous run's vCPU/KVM resources actually went back to the pool:
# this one boots the ordinary /init workload, which is supposed to reach a halt on its own.
log "firing a fresh 'baud run kvm' to prove the server recovered..."
FRESH_START="$SECONDS"
FRESH_JSON="$(timeout "$FRESH_RUN_TIMEOUT_S" "$BAUD" run kvm \
    --kernel "$KERNEL" \
    --initramfs "$INITRAMFS" \
    --cmdline "$CMDLINE_NORMAL" \
    --periodic-timer-period-rcb 500000 \
    --periodic-timer-vector 236 \
    --periodic-timer-max-ticks 2000 \
    --virtio-blk-image "$BLK_IMAGE" \
    --virtio-blk-vector 59 \
    --json)" || fail "a fresh 'baud run kvm' did not complete within ${FRESH_RUN_TIMEOUT_S}s after \
the cancellation -- the server is up but the previous run never gave its resources back"
FRESH_ELAPSED=$((SECONDS - FRESH_START))
OK="$(echo "$FRESH_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")"
[[ "$OK" == "True" ]] || fail "the fresh run after the cancellation reported ok!=true: $FRESH_JSON"
CONSOLE_HEX="$(echo "$FRESH_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)['console_output_hex'])")"
CONSOLE_TEXT="$(python3 -c "import sys; print(bytes.fromhex(sys.argv[1]).decode('utf-8', 'replace'))" "$CONSOLE_HEX")"
echo "$CONSOLE_TEXT" | grep -q "baud-guest: minimal kernel reached /init" \
    || fail "the fresh run's guest never reached /init:
$CONSOLE_TEXT"
pass "a fresh run completed normally in ${FRESH_ELAPSED}s (ok=true, guest reached /init)"

POST_KVM_FDS="$(server_kvm_fds)"
[[ "$POST_KVM_FDS" == "0" ]] || fail \
    "baud-server holds $POST_KVM_FDS KVM fd(s) after the fresh run finished -- runs leak KVM \
handles even on the ordinary completion path"
pass "server holds 0 KVM fds after the fresh run too -- no KVM handle is leaked per run"

curl -sf "$SRV/health" > /dev/null || fail "baud-server is not healthy after the cancellation"
pass "baud-server is still alive and healthy (/health) -- cancelling one run did not kill it"

echo ""
echo "=== POST /run/kvm client-disconnect cancellation: PASSED ==="
echo ""
echo "A killed 'baud run kvm' client no longer leaves a full KVM run executing on the server: the"
echo "burn stops, the VM and vCPU are torn down, and the server immediately serves a fresh run in"
echo "normal time. Server-side work is bounded by the client that asked for it, so one interrupted"
echo "request can no longer hold a core (or an allocation) that nobody can reach any more."
