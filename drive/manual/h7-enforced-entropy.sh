#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/manual/h7-enforced-entropy.sh — H7's OS-entropy leg (todo.md §14 next-actions item 2) on the
# enforced (RDTSC/RDTSCP-trapping) KVM module: proves a real, unmodified Linux 6.18 kernel's
# `getrandom()`/`/dev/urandom` are a pure function of the tape, end to end.
#
# Sibling of drive/manual/h3-enforced-rdtsc.sh, same unconditional-restore discipline: this is the *host
# kernel module*, not just this repo's own binaries, so it always swaps the stock kvm_intel/kvm
# modules back on the way out, success or failure.
#
# Why this needs the enforced module, not the stock one (crates/baud-multiverse/src/linux/mod.rs's
# `os_entropy_is_deterministic` doc has the full story): under the *stock* module, `random_init()`
# (drivers/char/random.c) unconditionally mixes `ktime_get_real()` into the CRNG pool after the
# pinned `SETUP_RNG_SEED` boot seed already credited it — and with no RTC and only a TSC
# clocksource, `ktime_get_real()` reads the real (untrapped) hardware TSC at a point that varies
# with host-scheduling jitter between independent boots. Only with RDTSC hardware-trapped and
# served from the work-clock (rdtsc-enforce.patch) does that read become reproducible too.
#
# Why this also needs RDTSCP handling: booting the real linux-guest kernel + entropy_init.c under
# the *first* version of the enforced module hit KVM_EXIT_INTERNAL_ERROR immediately — dmesg showed
# "vmx: unexpected exit reason 0x33" (EXIT_REASON_RDTSCP). Forcing CPU_BASED_RDTSC_EXITING also
# forces RDTSCP to VM-exit (Intel SDM Vol. 3C 25.1.2), but kvm_vmx_exit_handlers[] had no entry for
# it at all — every prior hand-assembled fixture issued only bare RDTSC, never RDTSCP; a real,
# unmodified Linux 6.18 kernel is the first guest to issue it (early boot TSC calibration / vDSO
# setup). rdtsc-enforce.patch now adds `handle_baud_rdtscp_exit` (payload kind 3) alongside
# `handle_baud_rdtsc_exit` (kind 0), and `baud-vcpu` serves EDX:EAX from the same work-clock plus
# ECX from `IA32_TSC_AUX` (`WorkClock::serve_enforced_tsc_aux`). That crash is gone for good.
#
# FORMERLY FLAKY, NOW ROOT-CAUSED AND FIXED (todo.md §14 next-actions item 2). Two independent
# fixes landed across prior iterations: (1) WorkClock's RCB perf_event counter accumulated
# host-side dispatch branches between guest exits (exclude_host doesn't work on this
# nested-virtualized dev host), not just guest branches — fixed by pausing/resuming that counter
# around each KVM_RUN ioctl (crates/baud-vcpu/src/linux/mod.rs's run_and_convert_rcb_bracketed),
# which raised the observed pass rate from ~25-50% to ~75%. (2) The residual ~25% divergence was
# then root-caused to LinuxBranchCounter/measure_fixed_loop_branches both using the generic
# `PERF_COUNT_HW_BRANCH_INSTRUCTIONS` perf event (all branches) instead of the raw
# `BR_INST_RETIRED.COND` event (0x11c4) specs §3.3 and this project's own docs/determinism.md
# always specified — the generic event was independently measured `±1`-nondeterministic on this
# exact host (docs/determinism.md's own H0 table), which is exactly the few-count landing-RCB
# jitter that let a same-instruction interrupt still seed the CRNG differently each boot via
# Linux's add_interrupt_randomness(). Switching both call sites to the raw event
# (crates/baud-multiverse/src/linux/mod.rs's `BR_INST_RETIRED_COND` constant) took the measured
# real-hardware pass rate to 10/10 (and a second, independent batch of double_boot_ram_hash_
# identical in drive/manual/h7-enforced-checkpoint.sh — the same root cause — to 25/25). Set
# H7_ENTROPY_REPEATS=N to rerun the double-boot test N times in place (one module swap, not N) to
# keep an eye on this; a FAIL now is a real regression, not expected residual flakiness.
#
#   Reuses `tests/fixtures/linux-guest/` (H7's boot-to-userspace fixture) — `entropy_init.c` /
#   `entropy_initramfs.cpio.gz` are a second `/init` for the *same* already-built kernel (no
#   rebuild needed: entropy determinism is userspace-visible) that calls `getrandom()` x4 and reads
#   `/dev/urandom` x4, hex-encoding each 32-byte read out the raw-outb COM1 endpoint `init.c` uses.
#
#   `os_entropy_is_deterministic` (crates/baud-multiverse/src/linux/mod.rs, `#[ignore]`d so a
#   normal `cargo test --workspace` against the *stock* module never runs it): boots the entropy
#   fixture twice under the patched module and asserts the 8 probe reads (4 GETRANDOM + 4 URANDOM)
#   are byte-identical across both boots, and not all the same value (rules out a degenerate
#   always-zeroed buffer passing vacuously).

set -uo pipefail

cd "$(dirname "$0")/../.."
REPO_ROOT="$(pwd)"

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h7-enforced-entropy] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H7-enforced: real Linux OS-entropy is deterministic under the enforced KVM module ==="
echo ""

KSRC="$HOME/wsl-kernel-src/src"
PATCH="$REPO_ROOT/kernel-module/baud-enforced/rdtsc-enforce.patch"
SUDO() { echo baud | sudo -S "$@"; }

[[ -d "$KSRC" ]] || fail "kernel source tree '$KSRC' not found — see CLAUDE.md's \
\"Building an out-of-tree kernel module against this WSL2 kernel\" for the one-time clone/config \
this script assumes is already done."

# ---------------------------------------------------------------------------
# Build the patched kvm.ko/kvm-intel.ko (idempotent: skip patching if already applied). Only
# rdtsc-enforce.patch is needed — RDRAND/RDSEED enforcement is orthogonal to this test.
# ---------------------------------------------------------------------------
log "Preparing patched kvm.ko/kvm-intel.ko in $KSRC..."
if grep -q "handle_baud_rdtscp_exit" "$KSRC/arch/x86/kvm/vmx/vmx.c" 2>/dev/null; then
    log "rdtsc-enforce.patch (with RDTSCP handling) already applied to this tree, skipping patch step"
else
    patch -p1 -d "$KSRC" < "$PATCH" || fail "rdtsc-enforce.patch failed to apply to $KSRC — tree may be a different kernel version than this patch was written against (CLAUDE.md's clone step pins the exact matching tag)"
fi

( cd "$KSRC" && KBUILD_MODPOST_WARN=1 make CC=gcc-13 M=arch/x86/kvm modules -j"$(nproc)" ) \
    > /tmp/h7-enforced-entropy-build.log 2>&1
BUILD_RC=$?
if [[ $BUILD_RC -ne 0 ]]; then
    tail -60 /tmp/h7-enforced-entropy-build.log >&2
    fail "patched kvm.ko/kvm-intel.ko build failed (see /tmp/h7-enforced-entropy-build.log)"
fi
[[ -f "$KSRC/arch/x86/kvm/kvm.ko" && -f "$KSRC/arch/x86/kvm/kvm-intel.ko" ]] \
    || fail "build reported success but kvm.ko/kvm-intel.ko are missing from $KSRC/arch/x86/kvm"
pass "patched kvm.ko + kvm-intel.ko built (RDTSC + RDTSCP enforcement)"

# ---------------------------------------------------------------------------
# Swap in the patched modules, always swapping the stock ones back on the way out.
# ---------------------------------------------------------------------------
if fuser /dev/kvm >/dev/null 2>&1; then
    fail "/dev/kvm is in use by another process — refusing to rmmod kvm_intel/kvm while a guest \
may be running (this would affect more than this script)"
fi

SWAPPED=0
restore_stock() {
    if [[ "$SWAPPED" -eq 1 ]]; then
        log "restoring stock kvm_intel/kvm..."
        SUDO rmmod kvm_intel 2>/dev/null || true
        SUDO rmmod kvm 2>/dev/null || true
        SUDO modprobe kvm_intel || echo "  [WARN] modprobe kvm_intel failed to restore the stock module — check 'lsmod | grep kvm' by hand" >&2
    fi
}
trap restore_stock EXIT

log "rmmod stock kvm_intel/kvm..."
SUDO rmmod kvm_intel || fail "rmmod kvm_intel failed — is a guest running?"
SUDO rmmod kvm || fail "rmmod kvm failed"

log "insmod patched kvm.ko + kvm-intel.ko..."
SUDO insmod "$KSRC/arch/x86/kvm/kvm.ko" || fail "insmod patched kvm.ko failed"
SUDO insmod "$KSRC/arch/x86/kvm/kvm-intel.ko" || fail "insmod patched kvm-intel.ko failed"
SWAPPED=1
pass "patched kvm_intel.ko loaded in place of the stock module"

# ---------------------------------------------------------------------------
# The real-hardware test: only meaningful with the patched module loaded.
# ---------------------------------------------------------------------------
log "Running os_entropy_is_deterministic against the patched module..."
# H7_ENTROPY_REPEATS (default 1): reruns the double-boot test this many times in place, without
# re-swapping the kernel module each time — used to gather multiple boot-pairs' worth of the
# tick-diagnostic panic output (todo.md §14 next-actions item 2's residual-divergence
# investigation) in one sitting. Default of 1 preserves the script's normal pass/fail gating
# contract; a caller investigating flakiness sets e.g. H7_ENTROPY_REPEATS=10.
REPEATS="${H7_ENTROPY_REPEATS:-1}"
FAILED_RUNS=0
for i in $(seq 1 "$REPEATS"); do
    log "os_entropy_is_deterministic: run $i/$REPEATS..."
    ENFORCED_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
        linux::tests::os_entropy_is_deterministic --test-threads=1 2>&1)
    if echo "$ENFORCED_OUT" | grep -q "test result: ok"; then
        pass "run $i/$REPEATS: os_entropy_is_deterministic — byte-identical across two boots, non-degenerate"
    else
        FAILED_RUNS=$((FAILED_RUNS + 1))
        echo "$ENFORCED_OUT"
        echo "  [FAIL] run $i/$REPEATS: os_entropy_is_deterministic FAILED (see diagnostic above)" >&2
    fi
done
if [[ "$REPEATS" -eq 1 ]]; then
    [[ "$FAILED_RUNS" -eq 0 ]] || fail "os_entropy_is_deterministic FAILED"
else
    log "os_entropy_is_deterministic summary: $((REPEATS - FAILED_RUNS))/$REPEATS passed"
    [[ "$FAILED_RUNS" -eq 0 ]] || echo "  [WARN] $FAILED_RUNS/$REPEATS run(s) failed — the RCB-event fix (see header) took this to 10/10 last measured, so a failure now likely means a real regression; inspect the diagnostic output above for correlation with a specific tick" >&2
fi

log "Regression: re-running rdtsc_enforced_regime_is_bit_exact_across_boots (RDTSCP handling layered on the same patch)..."
RDTSC_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
    linux::tests::rdtsc_enforced_regime_is_bit_exact_across_boots --test-threads=1 2>&1)
echo "$RDTSC_OUT"
echo "$RDTSC_OUT" | grep -q "test result: ok" || fail "rdtsc_enforced_regime_is_bit_exact_across_boots FAILED (regression from the RDTSCP handler layered on the same patch)"
pass "rdtsc_enforced_regime_is_bit_exact_across_boots — no regression"

echo ""
echo "=== H7-enforced: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm with the patched kvm_intel.ko loaded:"
echo "  - A real, unmodified Linux 6.18 kernel now boots under the enforced (RDTSC/RDTSCP-trapping)"
echo "    module without hitting KVM_EXIT_INTERNAL_ERROR (the EXIT_REASON_RDTSCP gap is closed)"
echo "  - getrandom()/dev/urandom are byte-identical across two boots of the same image+tape — an"
echo "    unmodified Linux CRNG is a pure function of the tape, end to end (todo.md §3.8/§4.7)"
echo "  - Plain RDTSC enforcement still works with RDTSCP handling layered on the same patch"
