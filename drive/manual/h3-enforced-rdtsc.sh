#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/manual/h3-enforced-rdtsc.sh — first real, on-hardware boot of the enforced-determinism regime
# (todo.md §3.8's "custom KVM module", kernel-module/baud-enforced/ENFORCEMENT_DESIGN.md).
#
# Unlike every other drive/*.sh script, this one's subject is the *host kernel module*, not just
# this repo's own binaries: it patches and rebuilds `kvm_intel.ko`/`kvm.ko` from the kernel source
# tree CLAUDE.md's "Building an out-of-tree kernel module" section prepares
# (`~/wsl-kernel-src/src`), swaps them in for the currently-loaded stock modules, runs the one
# real-hardware test that can only mean anything with the patched module loaded, then always swaps
# the stock modules back — the rest of this workspace's test suite (todo.md's mandatory green-build
# protocol) assumes the *stock* module, so this script must never leave the host in the patched
# state, success or failure.
#
# What "enforced" means here, concretely (kernel-module/baud-enforced/ENFORCEMENT_DESIGN.md): RDTSC
# is forced to VM-exit (CPU_BASED_RDTSC_EXITING left set, never cleared) and a new handler
# (`handle_baud_rdtsc_exit`) hands the trap to userspace as `KVM_EXIT_BAUD_DETERMINISM` instead of
# executing the instruction natively; `baud-vcpu::linux::run_one_exit` resolves it via
# `WorkClock::serve_enforced_rdtsc()` and writes EDX:EAX before resuming. RDRAND and RDSEED
# enforcement are separate increments layered on this patch, each with its own drive script —
# `drive/manual/h3-enforced-rdrand.sh` (rdrand-enforce.patch) and `drive/manual/h3-enforced-rdseed.sh`
# (ud2-enforce.patch).
#
#   Reuses `tests/fixtures/rdtsc-guest/` (already built for H3.4's cooperative-regime test) —
#   its payload has no CPUID gate and no dependency on which module served RDTSC, so the exact
#   same fixture image proves the enforced path with no new guest image needed.
#
#   `rdtsc_enforced_regime_is_bit_exact_across_boots` (crates/baud-multiverse/src/linux/mod.rs,
#   `#[ignore]`d so a normal `cargo test --workspace` against the *stock* module never runs it):
#   boots rdtsc-guest twice under the patched module and asserts the served 64-bit value is
#   bit-for-bit identical — not just high-bits-tolerant like H3.4's cooperative counterpart, since
#   the enforced-regime value is a pure function of the branch counter, never real hardware time.

set -uo pipefail

cd "$(dirname "$0")/../.."
REPO_ROOT="$(pwd)"

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h3-enforced-rdtsc] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H3-enforced: RDTSC trapped and served by the enforced-regime KVM module ==="
echo ""

KSRC="$HOME/wsl-kernel-src/src"
PATCH="$REPO_ROOT/kernel-module/baud-enforced/rdtsc-enforce.patch"
SUDO() { echo baud | sudo -S "$@"; }

[[ -d "$KSRC" ]] || fail "kernel source tree '$KSRC' not found — see CLAUDE.md's \
\"Building an out-of-tree kernel module against this WSL2 kernel\" for the one-time clone/config \
this script assumes is already done."

# ---------------------------------------------------------------------------
# Build the patched kvm.ko/kvm-intel.ko (idempotent: skip patching if already applied, `make`
# itself skips recompiling unchanged objects).
# ---------------------------------------------------------------------------
log "Preparing patched kvm.ko/kvm-intel.ko in $KSRC..."
if grep -q "handle_baud_rdtsc_exit" "$KSRC/arch/x86/kvm/vmx/vmx.c" 2>/dev/null; then
    log "rdtsc-enforce.patch already applied to this tree, skipping patch step"
else
    patch -p1 -d "$KSRC" < "$PATCH" || fail "rdtsc-enforce.patch failed to apply to $KSRC — tree may be a different kernel version than this patch was written against (CLAUDE.md's clone step pins the exact matching tag)"
fi

( cd "$KSRC" && KBUILD_MODPOST_WARN=1 make CC=gcc-13 M=arch/x86/kvm modules -j"$(nproc)" ) \
    > /tmp/h3-enforced-build.log 2>&1
BUILD_RC=$?
if [[ $BUILD_RC -ne 0 ]]; then
    tail -60 /tmp/h3-enforced-build.log >&2
    fail "patched kvm.ko/kvm-intel.ko build failed (see /tmp/h3-enforced-build.log)"
fi
[[ -f "$KSRC/arch/x86/kvm/kvm.ko" && -f "$KSRC/arch/x86/kvm/kvm-intel.ko" ]] \
    || fail "build reported success but kvm.ko/kvm-intel.ko are missing from $KSRC/arch/x86/kvm"
pass "patched kvm.ko + kvm-intel.ko built"

# ---------------------------------------------------------------------------
# Swap in the patched modules, always swapping the stock ones back on the way out — this is the
# one drive script that touches host-global kernel state, so cleanup must be unconditional.
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
log "Running rdtsc_enforced_regime_is_bit_exact_across_boots against the patched module..."
ENFORCED_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
    linux::tests::rdtsc_enforced_regime_is_bit_exact_across_boots --test-threads=1 2>&1)
echo "$ENFORCED_OUT"
echo "$ENFORCED_OUT" | grep -q "test result: ok" || fail "rdtsc_enforced_regime_is_bit_exact_across_boots FAILED"
pass "rdtsc_enforced_regime_is_bit_exact_across_boots — trapped RDTSC served from the work-clock, bit-exact across two boots"

echo ""
echo "=== H3-enforced: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm with the patched kvm_intel.ko loaded:"
echo "  - RDTSC is forced to VM-exit (CPU_BASED_RDTSC_EXITING) instead of executing natively"
echo "  - The trap reaches userspace as KVM_EXIT_BAUD_DETERMINISM, resolved by baud-vcpu's own"
echo "    dispatch_exit (Exit::RdtscEnforced) via the work-clock, with zero pinned-crate changes"
echo "  - The served value is bit-exact across two boots of the same guest+tape, not just"
echo "    high-bits-tolerant like the cooperative regime's native rdtsc"
echo ""
echo "Covered by sibling scripts, not this one: RDRAND enforcement (drive/manual/h3-enforced-rdrand.sh,"
echo "rdrand-enforce.patch) and RDSEED enforcement (drive/manual/h3-enforced-rdseed.sh, ud2-enforce.patch —"
echo "RDSEED needs no VMX secondary control at all, so this host's unsettable"
echo "SECONDARY_EXEC_RDSEED_EXITING bit does not block it: baud-packages rewrites every rdseed opcode"
echo "to UD2+NOP at build time and the UD2's ordinary #UD exit is what gets served)."
