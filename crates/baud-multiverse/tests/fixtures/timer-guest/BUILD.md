<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `timer-guest` — H4's real bootable fixture for `timer_tick_lands_at_identical_instruction`

Same rationale and build mechanics as `../hello-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header `linux_loader::loader::bzimage::BzImage::load` accepts, no
kernel source tree/Nix/cross-compiler needed) — only the payload differs. Regenerate with
`python3 build.py` (needs only `as`/`ld`).

## What it does

`payload.s` builds a real 64-bit IDT (16-byte interrupt-gate descriptors, one filled in at vector
`0x30`, every lower vector left zeroed/not-present) pointing at a handler that writes one marker
byte (`'T'`) to COM1 and returns via `iretq`, points `IDTR` at it (`lidt`), enables interrupts
(`sti`), then busy-loops (`dec`/`jnz`, ~340,000 iterations, a forced VM exit every 16 branches —
plenty of retired conditional branches for two `Multiverse::inject_timer_tick` calls to land
inside) before a clean `hlt`. The forced-exit interval must stay smaller than
`baud_vcpu::boundary::MARGIN` (64) — see `payload.s`'s own comment on that `out 0x80, al` for why:
a coarser interval (1000, this fixture's original value) let the "early exit" always land *past*
the real target, since that's the only place `LinuxPmuStepper::run_until_exit`'s poll ever gets
checked, which silently skipped `inject_at`'s single-step phase every time and left the reported
landing point sensitive to a few branches of unfiltered host-side counter jitter per run.

This is the first fixture in this project with an IDT at all — `hello-guest`/`tape-echo-guest`
have none, and `rdrand-guest` relies on having none (so its `#UD` cascades to a triple fault). A
real interrupt injection needs a real handler to land in, which needs a real IDT.

## Why this fixture (and a real in-memory GDT) had to exist for H4

specs/baud-vcpu.md §5's arm-early-then-single-step engine (`baud_vcpu::boundary::inject_at`,
`baud_vcpu::linux::pmu::LinuxPmuStepper`) existed and was hardware-independently unit-tested
(`crates/baud-vcpu/src/boundary.rs`'s scripted-stepper tests) since before this iteration, but
nothing in `baud-multiverse` had ever called it, and no guest fixture could receive a real
delivered interrupt to prove it end-to-end.

Wiring it in surfaced a real, previously-latent boot-flow gap: `pagetables::long_mode_sregs`'s
comment claimed "the guest never executes `LGDT`, so no in-memory GDT is ever built or read" —
true for *ordinary* instruction execution (KVM's segment-descriptor cache is loaded directly via
`KVM_SET_SREGS`, bypassing any in-memory table), but **not** true the moment an interrupt is
actually delivered: per the Intel SDM, an IDT gate's far transfer always reloads `CS` via a real
GDT descriptor-table lookup of the gate's target selector, regardless of how the *current* CS
segment cache got populated. Before this fixture, nothing in this project had ever delivered an
interrupt, so this never mattered; `layout::build_flat_gdt`/`pagetables::write_gdt` (a minimal
3-entry flat GDT matching `long_mode_sregs`'s existing code/data segment definitions exactly) now
build a real one, and `long_mode_sregs`'s `GDT` `kvm_dtable` points at it instead of `base=0,
limit=0`.

## Reconciling two independent RCB counters

`Multiverse::inject_timer_tick` (`crates/baud-multiverse/src/linux/mod.rs`) constructs a
`LinuxPmuStepper` per call — a distinct `perf_event` file descriptor from the work-clock's own
free-running `LinuxBranchCounter`. Both count the identical architectural event
(`PERF_COUNT_HW_BRANCH_INSTRUCTIONS`) on the identical thread, so their *deltas* over the same
interval agree by construction, but their absolute epochs (each fd resets to 0 on creation) do
not — `WorkClock::current_rcb()` reads the work-clock's counter immediately before constructing
the stepper, and `LinuxPmuStepper::with_baseline_rcb` seeds the new stepper's own counter space
with that same reading, so a `target_rcb` computed as "now + period" means the same thing to both.

## Injection is staged, not delivered, by `inject_at` alone

`boundary::inject_at` single-steps to the target boundary and calls `KVM_SET_VCPU_EVENTS` —
staging the interrupt, not delivering it. KVM actually injects a staged interrupt at the *next*
`KVM_RUN` entry, which is why `Multiverse::run_with_timer_ticks` injects every tick first and then
calls `run_to_first_halt` (or, for a second tick, the next `inject_timer_tick`'s own arm/step
loop) to let the guest actually run forward and take the delivered interrupt.
