#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h3-enforced-rdseed.sh — first real, on-hardware boot of the enforced-regime's RDSEED
# enforcement (todo.md §4/§3.8's "custom KVM module", kernel-module/baud-enforced/
# ENFORCEMENT_DESIGN.md and ud2-enforce.patch).
#
# Sibling of drive/h3-enforced-rdtsc.sh and drive/h3-enforced-rdrand.sh, same unconditional-restore
# discipline: this is the *host kernel module*, not just this repo's own binaries, so it always
# swaps the stock kvm_intel/kvm modules back on the way out, success or failure — every other
# drive/*.sh assumes the *stock* module.
#
# What "enforced RDSEED" means, concretely (ud2-enforce.patch), and why it is structurally unlike
# its two siblings: SECONDARY_EXEC_RDSEED_EXITING is **not settable on this host's VMX microcode**
# (baud_enforced_probe's own dmesg report, kernel-module/baud-enforced/BUILD.md), so RDSEED is
# never trapped as an instruction at all. Instead `baud-packages` rewrites every `rdseed` opcode in
# the guest image to `UD2` (0F 0B) + `NOP` padding at **build** time, in place and
# length-preserving (crates/baud-packages/src/rdseed.rs, todo.md §4) — the real opcode never
# executes in the guest, so the missing secondary control is moot for this path. The resulting
# `UD2` raises an ordinary `#UD`, which stock KVM *already* traps (vmx_update_exception_bitmap
# unconditionally sets UD_VECTOR, for its own emulation fallback), so ud2-enforce.patch adds no
# exec-control or exception-bitmap change whatsoever: it intercepts exactly one branch of
# `handle_exception_nmi` with `handle_baud_ud2_exit`, which reads the 2 bytes at RIP and hands only
# a genuine `0F 0B` to userspace as `KVM_EXIT_BAUD_DETERMINISM` (payload low byte 2), leaving RIP
# *at* the UD2. Everything else — a real invalid opcode, a guest kernel's own BUG()/WARN_ON()
# (also a bare UD2) — falls straight through to stock `handle_ud`, untouched.
#
#   Uses `tests/fixtures/rdseed-guest/` (new for this test): a hand-assembled guest whose one
#   `rdseed eax` has *already been rewritten to UD2+NOP in the checked-in binary*, exactly as a
#   real `baud image build` would emit it. That fixture's BUILD.md records the exact guest address
#   of the UD2 (0x0020_0207), its destination register (gpr_index 0 = EAX) and the original
#   instruction's length (3) — the three numbers the test hardcodes into
#   `Multiverse::boot_with_rdseed_sites`, since nothing plumbs an image build's
#   `RdseedRewriteReport` into a boot yet (todo.md §14).
#
#   Two `#[ignore]`d tests (crates/baud-multiverse/src/linux/mod.rs), both invoked below, covering
#   the two halves of the patch's contract:
#     - `rdseed_enforced_regime_is_bit_exact_across_boots`: with the site registered, the guest gets
#       past the marker and echoes 4 served value bytes, bit-for-bit identical across two boots.
#     - `ud2_outside_the_rdseed_site_table_reinjects_ud`: with an *empty* site table (the same UD2,
#       now indistinguishable from an unrelated BUG()), the #UD is re-injected verbatim and the
#       guest never gets past the marker — no bogus value served. If `reinject_ud` were broken this
#       test would *hang* rather than fail (RIP is deliberately never advanced by the kernel
#       handler, so the guest would re-trap the same instruction forever); a hang here is a real
#       signal, not a flake.

set -uo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h3-enforced-rdseed] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H3-enforced: build-time-rewritten RDSEED trapped as UD2 and served by the enforced-regime KVM module ==="
echo ""

KSRC="$HOME/wsl-kernel-src/src"
RDTSC_PATCH="$REPO_ROOT/kernel-module/baud-enforced/rdtsc-enforce.patch"
RDRAND_PATCH="$REPO_ROOT/kernel-module/baud-enforced/rdrand-enforce.patch"
UD2_PATCH="$REPO_ROOT/kernel-module/baud-enforced/ud2-enforce.patch"
SUDO() { echo baud | sudo -S "$@"; }

[[ -d "$KSRC" ]] || fail "kernel source tree '$KSRC' not found — see CLAUDE.md's \
\"Building an out-of-tree kernel module against this WSL2 kernel\" for the one-time clone/config \
this script assumes is already done."

# ---------------------------------------------------------------------------
# Build the patched kvm.ko/kvm-intel.ko: ud2-enforce.patch is layered on top of rdrand-enforce.patch
# on top of rdtsc-enforce.patch (all three idempotent — skip whichever is already applied; the
# ordering matters because ud2-enforce.patch's context includes the other two's own hunks and it
# needs the KVM_EXIT_BAUD_DETERMINISM constant rdtsc-enforce.patch introduces).
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
if grep -q "handle_baud_ud2_exit" "$KSRC/arch/x86/kvm/vmx/vmx.c" 2>/dev/null; then
    log "ud2-enforce.patch already applied to this tree, skipping"
else
    patch -p1 -d "$KSRC" < "$UD2_PATCH" || fail "ud2-enforce.patch failed to apply to $KSRC — must be applied on top of rdtsc-enforce.patch + rdrand-enforce.patch, which the steps above just confirmed are present"
fi

( cd "$KSRC" && KBUILD_MODPOST_WARN=1 make CC=gcc-13 M=arch/x86/kvm modules -j"$(nproc)" ) \
    > /tmp/h3-enforced-rdseed-build.log 2>&1
BUILD_RC=$?
if [[ $BUILD_RC -ne 0 ]]; then
    tail -60 /tmp/h3-enforced-rdseed-build.log >&2
    fail "patched kvm.ko/kvm-intel.ko build failed (see /tmp/h3-enforced-rdseed-build.log)"
fi
[[ -f "$KSRC/arch/x86/kvm/kvm.ko" && -f "$KSRC/arch/x86/kvm/kvm-intel.ko" ]] \
    || fail "build reported success but kvm.ko/kvm-intel.ko are missing from $KSRC/arch/x86/kvm"
pass "patched kvm.ko + kvm-intel.ko built (RDTSC + RDRAND + UD2/RDSEED enforcement)"

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
# The real-hardware tests: only meaningful with the patched module loaded. Also re-runs the RDTSC
# and RDRAND enforced tests in the same swapped-in session (cheap, and this module now carries all
# three patches — a regression in either would show up here first).
# ---------------------------------------------------------------------------
log "Running rdseed_enforced_regime_is_bit_exact_across_boots against the patched module..."
ENFORCED_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
    linux::tests::rdseed_enforced_regime_is_bit_exact_across_boots --test-threads=1 2>&1)
echo "$ENFORCED_OUT"
echo "$ENFORCED_OUT" | grep -q "test result: ok" || fail "rdseed_enforced_regime_is_bit_exact_across_boots FAILED"
pass "rdseed_enforced_regime_is_bit_exact_across_boots — the rewritten site's UD2 served a tape-seeded value into EAX, bit-exact across two boots"

log "Running ud2_outside_the_rdseed_site_table_reinjects_ud (the genuine-#UD passthrough half)..."
REINJECT_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
    linux::tests::ud2_outside_the_rdseed_site_table_reinjects_ud --test-threads=1 2>&1)
echo "$REINJECT_OUT"
echo "$REINJECT_OUT" | grep -q "test result: ok" || fail "ud2_outside_the_rdseed_site_table_reinjects_ud FAILED — an unrecognized UD2 must re-inject #UD, never be served a value"
pass "ud2_outside_the_rdseed_site_table_reinjects_ud — an unregistered UD2 (a BUG()/WARN_ON() or genuine invalid opcode) still faults exactly as it would with no patch loaded"

log "Re-running rdrand_enforced_regime_is_bit_exact_across_boots (same patched module carries all three patches)..."
RDRAND_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
    linux::tests::rdrand_enforced_regime_is_bit_exact_across_boots --test-threads=1 2>&1)
echo "$RDRAND_OUT"
echo "$RDRAND_OUT" | grep -q "test result: ok" || fail "rdrand_enforced_regime_is_bit_exact_across_boots FAILED (regression from ud2-enforce.patch layered on top)"
pass "rdrand_enforced_regime_is_bit_exact_across_boots — still bit-exact with ud2-enforce.patch layered on"

log "Re-running rdtsc_enforced_regime_is_bit_exact_across_boots..."
RDTSC_OUT=$(cargo test -q -p baud-multiverse --lib -- --ignored --exact \
    linux::tests::rdtsc_enforced_regime_is_bit_exact_across_boots --test-threads=1 2>&1)
echo "$RDTSC_OUT"
echo "$RDTSC_OUT" | grep -q "test result: ok" || fail "rdtsc_enforced_regime_is_bit_exact_across_boots FAILED (regression from ud2-enforce.patch layered on top)"
pass "rdtsc_enforced_regime_is_bit_exact_across_boots — still bit-exact with ud2-enforce.patch layered on"

echo ""
echo "=== H3-enforced RDSEED: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm with the patched kvm_intel.ko loaded:"
echo "  - RDSEED enforcement needs no VMX secondary control at all (SECONDARY_EXEC_RDSEED_EXITING is"
echo "    unsettable on this host's microcode): baud-packages rewrites rdseed -> UD2+NOP at build"
echo "    time, so the real opcode never executes in the guest"
echo "  - The UD2's #UD already VM-exits under stock KVM's own exception bitmap; ud2-enforce.patch"
echo "    intercepts exactly one branch of handle_exception_nmi, adding no exec-control changes"
echo "  - The trap reaches userspace as KVM_EXIT_BAUD_DETERMINISM (payload byte 2) with RIP left at"
echo "    the UD2, resolved by baud-vcpu's dispatch_exit (Exit::RdseedEnforced) against the image's"
echo "    own site table, which supplies the destination GPR and the original instruction's length"
echo "  - The served value is bit-exact across two boots of the same guest+tape"
echo "  - A UD2 with no registered site re-injects #UD verbatim, so kernel BUG()/WARN_ON() and"
echo "    genuine invalid opcodes keep behaving exactly as they do with no patch loaded"
echo "  - RDTSC and RDRAND enforcement still work with ud2-enforce.patch layered on top"
echo ""
echo "Not yet done: nothing plumbs a real \`baud image build\`'s RdseedRewriteReport into"
echo "Multiverse::boot_with_rdseed_sites yet — this test hardcodes the hand-verified site of a fixed"
echo "fixture image (tests/fixtures/rdseed-guest/BUILD.md), an explicit scope cut, not a gap in the"
echo "serve-path mechanism this script exercises."
