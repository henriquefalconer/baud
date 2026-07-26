<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `virtio-rng-guest` — the "interrupt delivery" half of virtio-rng, closed for real

Same build mechanics as `../timer-guest/BUILD.md` (hand-assembled flat binary wrapped in a
minimal bzImage header, no kernel source/Nix/cross-compiler needed). Regenerate with
`python3 build.py` (needs only `as`/`ld`/`objcopy`/`nm`).

## What it does, and why it exists

todo.md §14 next-actions item 1 tracked virtio-rng through several small, hardware-tested slices
(the transport register layer, real split-virtqueue ring parsing, notify-to-drain wiring), each
one explicitly leaving "interrupt delivery" open — this host registers no in-kernel irqchip at
all (`KVM_CREATE_IRQCHIP`/`KVM_IOEVENTFD` are never called anywhere), so whether a real interrupt
could ever reach a guest's virtio-rng ISR was an open question, not a stubbed mechanism.

This fixture answers it directly, the same way `timer-guest` answered the equivalent question for
H4's periodic timer: `payload.s` is a real (if minimal) virtio-rng driver, issued from hand-written
x86-64 assembly rather than Linux's `drivers/virtio/virtio_mmio.c` + `drivers/char/hw_random/
virtio-rng.c`, but performing the *identical* wire protocol those drivers do — status-register
ACK/DRIVER/FEATURES_OK/DRIVER_OK, feature negotiation (accepting whatever `VIRTIO_F_VERSION_1` bit
the device offers verbatim), queue selection/sizing/ring-address setup, posting one writable
descriptor, and a `QueueNotify` write — against the real `VirtioMmioTransport` at
`layout::VIRTIO_MMIO_RNG_BASE` (spec 1.1 §4.2.2/§3.1, §5.4). It then busy-loops (same
forced-VM-exit-every-16-branches shape as `timer-guest`, for the same `LinuxPmuStepper::
run_until_exit` polling reason) with a real IDT gate at vector `0x31` (distinct from
`timer-guest`'s `0x30`) pointing at an ISR that writes a marker byte plus the first byte of the
buffer the device filled, then `iretq`s back into the loop.

The test driving this (`crates/baud-multiverse/src/linux/mod.rs`,
`virtio_rng_interrupt_reaches_the_guests_own_isr` and its double-boot sibling) does the host-side
half: step the guest one exit at a time (`Multiverse::step_exit`) until `Multiverse::virtio_rng()`
reports `notify_count() == 1` (the `QueueNotify` write has landed), call
`Multiverse::service_virtio_rng_interrupt(0x31)` — which drains the ring with tape-seeded entropy
bytes and, since at least one chain was drained, delivers a real interrupt at vector `0x31` via
`inject_timer_tick(0, 0x31)`'s degenerate "inject right now" case (`period_rcb = 0`) — then lets
the guest run forward (`Multiverse::run_to_first_halt`) to actually take the staged interrupt and
reach its own clean `hlt`.

**Real-hardware result**: the guest's ISR fires, and the second byte it writes out matches the
exact byte an independent `SplitMix64::new(seed).next_u64()` computation on the host side predicts
— proving not just that an interrupt landed, but that the specific entropy bytes
`DeviceBus::service_virtio_rng` wrote into guest memory are the ones the guest's own code reads
back, through a real delivered CPU interrupt, with no in-kernel irqchip at all.

## Update: the PIC bring-up sequence, and the vector question

This closed the "can baud deliver *a* real interrupt to *a* guest's virtio-rng ISR, at a vector
baud itself chooses" question — using exactly the same "just stage `KVM_SET_VCPU_EVENTS` and let
the next `KVM_RUN` deliver it" trick `timer-guest` already proved for Linux's fixed
`LOCAL_TIMER_VECTOR`. What it did **not** answer, at the time it was written, was which vector an
*unmodified Linux* kernel's real `virtio_mmio`/`virtio_rng` driver stack would resolve its
`virtio_mmio.device=<size>@<base>:<irq>` cmdline IRQ number to via `request_irq()` — unlike the
LAPIC timer, an ordinary device IRQ line is normally resolved through an IOAPIC/PIC, which this
VMM registers no in-kernel emulation of at all.

That question is now answered, and `payload.s` was extended to prove the mechanism on real
hardware, not just reason about it: `crate::pic8259::Pic8259` is a new dual-8259 bookkeeping stub
(ports 0x20/0x21/0xA0/0xA1) wired into `DeviceBus`, and `_start` now issues the exact byte
sequence Linux's own `probe_8259A()` + `init_8259A()` (`arch/x86/kernel/i8259.c`) do — a
distinguishing mask write/readback on each chip's data port, then the full ICW1→ICW4 handshake on
both chips, then an `enable_8259A_irq(5)`-equivalent unmask — before the virtio-mmio negotiation
below, proving the new stub doesn't disturb anything else this fixture depends on. Per
`arch/x86/include/asm/irq_vectors.h`'s `ISA_IRQ_VECTOR(irq) = 0x30 + irq` (grep-confirmed against
real Linux 6.18.33 source, see `pic8259.rs`'s own doc for the full derivation), an unmodified
guest's `virtio_mmio.device=…:5` would resolve to vector `0x35` — this fixture's own IDT still
uses its own independently-chosen `VECTOR` (`0x31`, unchanged) for the interrupt baud actually
injects, since baud's direct-injection mechanism has never depended on the PIC's hardware ICW2
vector base or on `ISA_IRQ_VECTOR()` either; `0x35` is what a *real* Linux `request_irq(5, …)`
driver would need baud to inject at, once one exists.

Nor does this wire virtio-rng into any real boot's cmdline/CLI/server route, or actually boot an
unmodified Linux kernel far enough to exercise its real `virtio_mmio.c`/`virtio-rng.c` drivers
(still blocked on the Buildroot/pinned-Nix guest-image pipeline, §4.5 — the same prerequisite
blocking H8/H9) — both remain separate, still-open work (todo.md §14).
