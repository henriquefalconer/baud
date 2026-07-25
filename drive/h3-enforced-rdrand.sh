#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h3-enforced-rdrand.sh — first real, on-hardware boot of the enforced-regime's RDRAND
# enforcement (todo.md §3.2/§3.8's "custom KVM module", kernel-module/baud-enforced/
# ENFORCEMENT_DESIGN.md and rdrand-enforce.patch).
#
# Sibling of drive/h3-enforced-rdtsc.sh, same unconditional-restore discipline: this is the *host
# kernel module*, not just this repo's own binaries, so it always swaps the stock kvm_intel/kvm
# modules back on the way out, success or failure — every other drive/*.sh assumes the *stock*
# module.
#
# What "enforced RDRAND" means, concretely (kernel-module/baud-enforced/ENFORCEMENT_DESIGN.md,
# rdrand-enforce.patch): SECONDARY_EXEC_RDRAND_EXITING is already forced on by stock KVM's own
# opt-in logic (baud's CPUID mask always clears the RDRAND feature bit, §3.2, so
# vmx_adjust_sec_exec_exiting never turns exiting back off) — no execution-control patch needed.
# The only change is the exit-handler table: `handle_baud_rdrand_exit` replaces the stock
# `kvm_handle_invalid_op` (which just injects #UD) so the trap reaches userspace as
# `KVM_EXIT_BAUD_DETERMINISM` (the same reason RDTSC uses, distinguished by a payload byte) instead
# of executing natively. `baud-vcpu::linux::run_and_convert`/`dispatch_exit` resolve it via
# `WorkClock::serve_enforced_rdrand()` (a tape-seeded deterministic PRNG) and write the value into
# the guest-chosen destination GPR (decoded from the trap itself) plus RFLAGS.CF=1.
#
#   Reuses `tests/fixtures/rdrand-guest/` (already built for the cooperative-regime
#   `rdrand_guest_is_flagged` test) — its post-`rdrand` echo loop is unreachable under the
#   cooperative regime (masked-CPUID hardware #UD fires first) but was built for exactly this
#   test, per that fixture's own BUILD.md: enforced-regime exiting traps *before* the #UD check.
#
#   `rdrand_enforced_regime_is_bit_exact_across_boots` (crates/baud-multiverse/src/linux/mod.rs,
#   `#[ignore]`d so a normal `cargo test --workspace` against the *stock* module never runs it):
#   boots rdrand-guest twice under the patched module and asserts the guest gets past the marker,
#   echoing 4 served value bytes bit-for-bit identically across both boots.

set -uo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h3-enforced-rdrand] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H3-enforced: RDRAND trapped and served by the enforced-regime KVM module ==="
echo ""

KSRC="$HOME/wsl-kernel-src/src"
RDTSC_PATCH="$REPO_ROOT/kernel-module/baud-enforced/rdtsc-enforce.patch"
RDRAND_PATCH="$REPO_ROOT/kernel-module/baud-enforced/rdrand-enforce.patch"
SUDO() { echo baud | sudo -S "$@"; }

[[ -d "$KSRC" ]] || fail "kernel source tree '$KSRC' not found — see CLAUDE.md's \
\"Building an out-of-tree kernel module against this WSL2 kernel\" for the one-time clone/config \
this script assumes is already done."

# ---------------------------------------------------------------------------
# Build the patched kvm.ko/kvm-intel.ko: rdrand-enforce.patch is layered on top of
# rdtsc-enforce.patch (both idempotent — skip whichever is already applied).
# ---------------------------------------------------------------------------
log "Preparing patched kvm.ko/kvm-intel.ko in $KSRC..."
if grep -q "handle_baud_rdtsc_exit" "$KSRC/arch/x86/kvm/vmx/vmx.c" 2>/dev/null; then
    log "rdtsc-enforce.patch already applied to this tree, skipping"
else
    patch -p1 -d "$KSRC" < "$RDTSC_PATCH" || fail "rdtsc-enforce.patch failed to apply to $KSRC"
fi
if grep -q "handle_baud_rdrand_exit" "$KSRC/arch/x86/kvm/vmx/vmx.c" 2>/dev/null; then
    log "rdrand-enforce.patch already applied to this tree, skipping"
else
    patch -p1 -d "$KSRC" < "$RDRAND_PATCH" || fail "rdrand-enforce.patch failed to apply to $KSRC — must be applied on top of rdtsc-enforce.patch, which the step above just confirmed is present"
fi

( cd "$KSRC" && KBUILD_MODPOST_WARN=1 make CC=gcc-13 M=arch/x86/kvm modules -j"$(nproc)" ) \
    > /tmp/h3-enforced-rdrand-build.log 2>&1
BUILD_RC=$?
if [[ $BUILD_RC -ne 0 ]]; then
    tail -60 /tmp/h3-enforced-rdrand-build.log >&2
    fail "patched kvm.ko/kvm-intel.ko build failed (see /tmp/h3-enforced-rdrand-build.log)"
fi
[[ -f "$KSRC/arch/x86/kvm/kvm.ko" && -f "$KSRC/arch/x86/kvm/kvm-intel.ko" ]] \
    || fail "build reported success but kvm.ko/kvm-intel.ko are missing from $KSRC/arch/x86/kvm"
pass "patched kvm.ko + kvm-intel.ko built (RDTSC + RDRAND enforcement)"

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
# The real-hardware test: only meaningful with the patched module loaded. Also re-runs the RDTSC
# enforced test in the same swapped-in session (cheap, and this module now carries both patches).
# ---------------------------------------------------------------------------
log "Running rdrand_enforced_regime_is_bit_exact_across_boots against the patched module..."
ENFORCED_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
    linux::tests::rdrand_enforced_regime_is_bit_exact_across_boots --test-threads=1 2>&1)
echo "$ENFORCED_OUT"
echo "$ENFORCED_OUT" | grep -q "test result: ok" || fail "rdrand_enforced_regime_is_bit_exact_across_boots FAILED"
pass "rdrand_enforced_regime_is_bit_exact_across_boots — trapped RDRAND served a tape-seeded value, bit-exact across two boots"

log "Re-running rdtsc_enforced_regime_is_bit_exact_across_boots (same patched module carries both patches)..."
RDTSC_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
    linux::tests::rdtsc_enforced_regime_is_bit_exact_across_boots --test-threads=1 2>&1)
echo "$RDTSC_OUT"
echo "$RDTSC_OUT" | grep -q "test result: ok" || fail "rdtsc_enforced_regime_is_bit_exact_across_boots FAILED (regression from the RDRAND patch layered on top)"
pass "rdtsc_enforced_regime_is_bit_exact_across_boots — still bit-exact with rdrand-enforce.patch layered on"

echo ""
echo "=== H3-enforced RDRAND: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm with the patched kvm_intel.ko loaded:"
echo "  - RDRAND-exiting was already forced on by stock KVM's own CPUID-driven opt-in logic; only"
echo "    the exit-handler table entry needed patching (kvm_handle_invalid_op -> handle_baud_rdrand_exit)"
echo "  - The trap reaches userspace as KVM_EXIT_BAUD_DETERMINISM (payload byte 1), resolved by"
echo "    baud-vcpu's dispatch_exit (Exit::RdrandEnforced) via a tape-seeded deterministic PRNG,"
echo "    written into the guest-chosen destination GPR decoded from the trap itself"
echo "  - The served value is bit-exact across two boots of the same guest+tape"
echo "  - RDTSC enforcement still works correctly with rdrand-enforce.patch layered on top"
echo ""
echo "Covered by a sibling script, not this one: RDSEED enforcement (drive/h3-enforced-rdseed.sh,"
echo "ud2-enforce.patch). SECONDARY_EXEC_RDSEED_EXITING being unsettable on this host's VMX microcode"
echo "(baud_enforced_probe's own dmesg report) turned out not to block it at all: baud-packages"
echo "rewrites every rdseed opcode to UD2+NOP at build time, so the real RDSEED instruction never"
echo "executes in the guest and the UD2's ordinary #UD exit — already trapped by stock KVM's own"
echo "exception bitmap — is what gets served instead."
