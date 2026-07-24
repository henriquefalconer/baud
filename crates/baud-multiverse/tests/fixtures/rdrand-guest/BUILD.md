<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `rdrand-guest` — H3's real bootable fixture for `rdrand_guest_is_flagged`

Same rationale and build mechanics as `../hello-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header `linux_loader::loader::bzimage::BzImage::load` accepts, no
kernel source tree/Nix/cross-compiler needed) — only the payload differs. Regenerate with
`python3 build.py` (needs only `as`/`ld`).

## What it does

`payload.s` is hand-written x86-64: write one marker byte (`'X'`) to COM1 (`out dx, al` at port
`0x3f8`), then execute `rdrand eax` directly, then (unreachable under the cooperative regime — see
below) write the 4 raw result bytes to COM1 one at a time, then `hlt` in a loop.

## Why this fixture exists

specs/baud-multiverse.md §3.2's mask table clears the RDRAND feature bit (`01H:ECX[30]`) from
CPUID so a *compliant* guest never sees the feature as present and never issues the instruction.
This fixture is deliberately the opposite of compliant: it ignores CPUID entirely and executes
`rdrand` unconditionally, modeling an adversarial or buggy guest that does not check the feature
bit before using it — exactly the case todo.md §3.2 / test-matrix row 1 requires be caught rather
than silently producing non-reproducible output.

## Real-hardware finding: masked CPUID hardware-blocks the instruction, it does not just fail to divert a compliant guest

The spec's original text ("a guest that issues it anyway is caught by double-run divergence") was
an assumption never exercised against real hardware. Booting this fixture against real `/dev/kvm`
for the first time falsified it, in the good direction: `rdrand` with `CPUID.01H:ECX.RDRAND=0` (as
`cpuid.rs`'s mask table configures) raises `#UD` immediately, per the Intel SDM's own `RDRAND`
reference (`IF CPUID.01H:ECX.RDRAND[bit 30] = 0 THEN #UD; FI;`) — this check is real, not merely
descriptive of which CPU generations shipped the opcode. This fixture has no IDT, so the `#UD`
cascades straight to a triple fault, which `baud-vcpu`'s run loop (`crates/baud-vcpu/src/lib.rs`)
already treats identically to a clean `Hlt` (`VcpuExit::Shutdown` -> `DispatchOutcome::Halted`).
The guest therefore never reaches the code after `rdrand` at all: two boots of this fixture produce
byte-identical, single-marker-byte output, every time — not the divergent output the spec assumed.

This makes the cooperative regime's CPUID mask a **stronger** guarantee than originally specified:
under cooperative, the raw random instruction is hardware-unreachable by any guest, compliant or
adversarial, rather than merely being caught after the fact by comparing two runs. specs/
baud-multiverse.md's `rdrand_guest_is_flagged` description and §3.2 were updated to match this
finding; `linux::tests::rdrand_guest_is_flagged` in `crates/baud-multiverse/src/linux/mod.rs`
asserts the real, deterministic-not-divergent behavior directly.

The output loop after `rdrand` remains in the payload deliberately unreachable under cooperative:
it is what a future **enforced**-regime run (the hardware RDRAND-exiting VM-execution control,
specs/baud-host.md §8's not-yet-built custom KVM module) would actually exercise — `rdrand`
under an unconditional exiting control VM-exits *before* the CPU's own `#UD` check, so an enforced
run would reach the echo loop and this same fixture becomes the natural test of "served from the
tape" once that module exists.

## No feature-bit check on purpose

A real "compliant" guest fixture proving the *positive* half of the CPUID mask (that RDRAND is
absent so a well-behaved guest never calls it) is already covered by
`kvm_cpuid_entry2_masking_matches_the_portable_leaf_type` and the CPUID-readback assertion in
`linux::tests` — this fixture exists specifically to exercise the adversarial path, and in doing so
discovered that path is hardware-closed under the cooperative regime too.
