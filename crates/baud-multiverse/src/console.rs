// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The console device: a minimal 16550-compatible UART on COM1 (I/O ports 0x3f8-0x3ff), built on
// `vm_superio::Serial` — the crate specs/baud-multiverse.md §2 pins for exactly this purpose. At
// H1 baud only needed guest -> host output (the guest kernel's serial console), so the writer here
// was a plain in-memory buffer and interrupt delivery (IRQ4) used a no-op-but-recording `Trigger`.
// H5's `shell_into_universe_resumes` (specs/baud-snapshot.md §5, "restore into a live shell") adds
// the other direction: [`Console::enqueue_input`] wraps `vm_superio::Serial::enqueue_raw_bytes` to
// push host-supplied bytes into the UART's RX FIFO, so a restored guest's own polling read loop
// (`IN` on the DATA register, offset 0) observes them exactly as it would a real keystroke — no
// change to the write-side buffer or the no-op `Trigger` was needed: `enqueue_raw_bytes` only sets
// the LSR "data ready" bit directly (`Serial::read`'s existing DATA-offset arm already pops
// `in_buffer`), it does not require a real interrupt to be delivered for a guest that polls LSR
// instead of waiting for IRQ4 (`crates/baud-multiverse/tests/fixtures/shell-guest` does exactly
// that). A real IRQ-driven trigger (an `EventFd`-backed one, replacing `NoIrqTrigger`) remains
// future work for a guest that blocks on the interrupt instead of polling.
//
// `vm_superio` has no non-dev dependencies at all (checked against its own Cargo.toml — not even
// `libc`), so unlike the rest of `linux/`, this whole module — including its tests — is
// hardware-independent and runs on this Windows dev machine with no KVM/perf, the same pattern
// `cpuid.rs`/`layout.rs`/`baud-vcpu`'s `boundary.rs` use.

use crate::tape_bus::TapeBus;
use crate::virtio_mmio::VirtioMmioTransport;
use baud_vcpu::{Bus, OpenBusFallback, OPEN_BUS_BYTE};
use std::cell::Cell;
use std::convert::Infallible;
use vm_superio::serial::NoEvents;
use vm_superio::{Serial, Trigger};

#[cfg(target_os = "linux")]
use crate::timesource::SplitMix64;
#[cfg(target_os = "linux")]
use crate::virtio_queue::{SplitVirtqueue, VirtqueueError};
#[cfg(target_os = "linux")]
use vm_memory::GuestMemoryBackend;

/// COM1's 8-register I/O window — the only PIO range this milestone's console serves; every other
/// address still falls through to [`OpenBusFallback`] via [`DeviceBus`].
pub const COM1_BASE: u16 = 0x3f8;
pub const COM1_LEN: u16 = 8;

/// A [`vm_superio::Trigger`] that records "an interrupt was requested" without delivering one — H1
/// does not yet drive an interrupt controller (that lands with the tape device / interrupt-
/// injection wiring, todo.md §3.4). Recording rather than silently dropping means a future
/// integration can assert on it instead of rediscovering that IRQ4 was never wired up.
#[derive(Debug, Default)]
pub struct NoIrqTrigger {
    fired: Cell<u64>,
}

impl NoIrqTrigger {
    /// How many times the UART asked to raise IRQ4 since construction.
    pub fn fired_count(&self) -> u64 {
        self.fired.get()
    }
}

impl Trigger for NoIrqTrigger {
    type E = Infallible;
    fn trigger(&self) -> Result<(), Self::E> {
        self.fired.set(self.fired.get() + 1);
        Ok(())
    }
}

/// The COM1 UART, capturing guest console output into an in-memory buffer.
pub struct Console {
    serial: Serial<NoIrqTrigger, NoEvents, Vec<u8>>,
}

impl Default for Console {
    fn default() -> Self {
        Console { serial: Serial::new(NoIrqTrigger::default(), Vec::new()) }
    }
}

impl Console {
    /// A [`Console`] pre-seeded with `output` — restoring a `Universe` snapshot
    /// (`baud-snapshot::universe::DeviceState::console`) reconstructs the console this way so that
    /// [`output`](Self::output) immediately after restore returns the full history (captured bytes
    /// followed by anything written post-restore), matching what a straight run would show at the
    /// same point. `vm_superio::Serial`'s writer is a plain `Vec<u8>` (any `std::io::Write`
    /// implementor), so pre-filling it and letting subsequent `write()` calls append is exact — no
    /// separate "history" field is needed.
    pub fn with_output(output: Vec<u8>) -> Self {
        Console { serial: Serial::new(NoIrqTrigger::default(), output) }
    }

    /// Bytes the guest has written to the UART's transmit register so far, in order.
    pub fn output(&self) -> &[u8] {
        self.serial.writer()
    }

    /// How many times the emulated UART requested an interrupt (see [`NoIrqTrigger`]) — exposed so
    /// a future integration can assert IRQ4 was actually requested once real injection lands.
    pub fn irq_requests(&self) -> u64 {
        self.serial.interrupt_evt().fired_count()
    }

    /// Push host-supplied bytes into the UART's receive FIFO — the guest's next `IN` on the DATA
    /// register (offset 0) pops them in order, same as real keystrokes arriving over a wire
    /// (specs/baud-snapshot.md §5's "restore into a live shell"). Silently caps at the FIFO's
    /// remaining capacity (`vm_superio::Serial::enqueue_raw_bytes`'s own behavior) rather than
    /// erroring — a full 16550 RX FIFO drops bytes on real hardware too; a caller that cares can
    /// compare the returned count against `bytes.len()`.
    pub fn enqueue_input(&mut self, bytes: &[u8]) -> usize {
        self.serial.enqueue_raw_bytes(bytes).unwrap_or(0)
    }

    /// `port`'s offset within the COM1 register window, or `None` if `port` is outside it.
    pub(crate) fn in_range(port: u16) -> Option<u8> {
        if (COM1_BASE..COM1_BASE + COM1_LEN).contains(&port) {
            Some((port - COM1_BASE) as u8)
        } else {
            None
        }
    }
}

impl Bus for Console {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        match Self::in_range(port) {
            Some(offset) => {
                // A real UART register read is always a single byte; anything wider reaching
                // here would itself be a modeling gap, not a value to invent, so pad with the
                // fixed open-bus byte instead of fabricating extra register reads.
                if let Some(first) = data.first_mut() {
                    *first = self.serial.read(offset);
                }
                if data.len() > 1 {
                    data[1..].fill(OPEN_BUS_BYTE);
                }
            }
            None => data.fill(OPEN_BUS_BYTE),
        }
    }

    fn pio_write(&mut self, port: u16, data: &[u8]) {
        if let Some(offset) = Self::in_range(port) {
            if let Some(&byte) = data.first() {
                // `Serial::write`'s only error sources here are the (infallible) trigger and the
                // writer's `flush()` — a `Vec<u8>` writer can never fail either, so there is
                // nothing meaningful to do with `Err` besides drop it.
                let _ = self.serial.write(offset, byte);
            }
        }
        // Ports outside the COM1 window: absorbed silently, matching OpenBusFallback's write side
        // (an open-bus write has nowhere to go and nothing to report).
    }

    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE); // no MMIO devices modeled yet
    }

    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
}

/// The legacy CMOS RTC index/data port pair (ports 0x70/0x71) — not a real clock (todo.md §3.6:
/// "no real RTC ... deleted entirely"), but Linux's `mach_get_cmos_time()` (`arch/x86/kernel/
/// rtc.c`) reads these two ports unconditionally at boot on every x86 kernel, regardless of any
/// `CONFIG_RTC_*` setting (those Kconfig symbols — the ones `baud image lint` checks, todo.md §4 —
/// gate the `/dev/rtc` *driver*, not this always-compiled-in early-boot platform code). Found as a
/// second real guest hang on the very first real-KVM boot this crate was ever exercised against,
/// right after the CPUID-leaf PIT-calibration hang (`cpuid.rs`'s `LEAF_TSC_CRYSTAL`/
/// `LEAF_PROCESSOR_FREQ` doc): `mach_get_cmos_time` first polls Status Register A's "Update In
/// Progress" bit (bit 7) until it reads clear, but this port pair was previously unhandled and
/// fell through to [`OPEN_BUS_BYTE`] (`0xFF`) — all bits set, so the UIP bit *always* read as
/// "busy", spinning the guest forever. [`Cmos`] answers deterministically instead: UIP (and every
/// other register this shim is asked for) always reads `0`, so the poll loop exits on its first
/// read and `mach_get_cmos_time` parses a fixed (all-BCD-zero, binary/BCD mode from Status
/// Register B's cleared bit 2) but *reproducible* date — accuracy of the parsed date is not a
/// baud guarantee (no real clock exists on this machine), determinism of it is.
pub const CMOS_ADDR_PORT: u16 = 0x70;
pub const CMOS_DATA_PORT: u16 = 0x71;

/// A minimal, stateless-in-effect CMOS/MC146818 RTC index+data port shim: every data-register read
/// returns a fixed `0`, regardless of which register was last selected via the index port. This
/// happens to already answer "Update In Progress" (Status Register A, bit 7) as clear and
/// "binary/BCD + 12/24h" (Status Register B) as BCD/12h — the two register reads
/// `mach_get_cmos_time` actually branches on — so no register-selection state needs to be modeled,
/// tracked, or captured by a snapshot for this shim's output to stay reproducible.
#[derive(Debug, Default, Clone, Copy)]
pub struct Cmos;

impl Cmos {
    pub(crate) fn in_range(port: u16) -> bool {
        port == CMOS_ADDR_PORT || port == CMOS_DATA_PORT
    }
}

impl Bus for Cmos {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        // Both the index port (0x70, real hardware treats it as write-only/undefined-on-read) and
        // the data port (0x71) read as a fixed `0` here — see this type's doc for why that value
        // in particular is what keeps `mach_get_cmos_time`'s poll loop from hanging.
        debug_assert!(Self::in_range(port));
        if let Some(first) = data.first_mut() {
            *first = 0;
        }
        if data.len() > 1 {
            data[1..].fill(OPEN_BUS_BYTE);
        }
    }

    fn pio_write(&mut self, _port: u16, _data: &[u8]) {
        // Register-index writes (0x70) and any data writes (0x71, real hardware only accepts
        // these when unlocked for clock-setting) are both absorbed silently: this shim never
        // varies its read response by selected register, so there is nothing to record.
    }

    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE);
    }

    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
}

/// Composes [`Console`] (COM1), [`Cmos`] (ports 0x70/0x71), and [`TapeBus`] (the tape device,
/// specs/baud-tape-device.md) with [`OpenBusFallback`] for every other address — the device bus
/// the boot flow's run loop dispatches every exit through (`linux::Multiverse`). Matches todo.md
/// §3.6's subtractive rule: "down to a console plus the tape device" (`Cmos` is not a fourth real
/// device in the same sense — it never reads real time or real hardware, it exists only to
/// terminate a boot-time poll loop deterministically, see that type's doc).
#[derive(Default)]
pub struct DeviceBus {
    pub console: Console,
    pub tape: TapeBus,
    cmos: Cmos,
    /// The virtio-rng transport register block (`crate::virtio_mmio`), `None` until
    /// [`Self::enable_virtio_rng`] is called — every existing constructor (`Default`,
    /// [`Self::with_tape`], [`Self::restore`]) leaves it unset, so no existing boot path's MMIO
    /// behavior changes: an unset slot falls straight through to `fallback` exactly as before this
    /// device existed. Not yet wired into any real boot's cmdline/CLI (todo.md-tracked next step —
    /// The actual ring-draining mechanism is now real — see [`Self::service_virtio_rng`] — but
    /// nothing calls it automatically from `QueueNotify` (the memory-oblivious [`Bus`] trait has no
    /// guest-memory access to do so, see `virtio_mmio.rs`'s doc), and real interrupt delivery is
    /// still unimplemented.
    virtio_rng: Option<VirtioMmioTransport>,
    /// The live ring-walking cursor for `virtio_rng`'s sole queue, lazily built (and rebuilt on
    /// driver re-negotiation) by [`Self::service_virtio_rng`] — `None` until the queue is both
    /// enabled and marked ready. Linux-only: it borrows real `vm-memory`, unlike every other field
    /// on this struct.
    #[cfg(target_os = "linux")]
    virtio_rng_queue: Option<SplitVirtqueue>,
    /// The virtio-rng device's own tape-seeded byte stream (todo.md §3.8: an "ever-ready
    /// deterministic byte source"), independent of the `rdrand`/`rdseed` entropy sub-stream
    /// (`timesource::WorkClock`'s `entropy` field) and of the boot `SETUP_RNG_SEED` — see
    /// [`Self::seed_virtio_rng_entropy`].
    #[cfg(target_os = "linux")]
    virtio_rng_entropy: SplitMix64,
    fallback: OpenBusFallback,
}

impl DeviceBus {
    /// A [`DeviceBus`] whose tape device is seeded with `tape` — the constructor
    /// `linux::Multiverse::boot` uses, since [`DeviceBus`]'s `fallback` field is private to this
    /// module (struct-update syntax like `DeviceBus { tape, ..Default::default() }` cannot be used
    /// from outside `console.rs`).
    pub fn with_tape(tape: Vec<u8>) -> Self {
        DeviceBus { tape: TapeBus::new(tape), ..Default::default() }
    }

    /// Installs a virtio-rng transport at [`crate::layout::VIRTIO_MMIO_RNG_BASE`] — opt-in (no
    /// existing caller does this yet), so [`Bus::mmio_read`]/[`Bus::mmio_write`] start dispatching
    /// that address window to it instead of [`OpenBusFallback`]. The entropy stream starts seeded
    /// at `0` (matching `WorkClock::new`'s own "deterministic but not tape-derived until seeded"
    /// convention) — call [`Self::seed_virtio_rng_entropy`] before any guest code runs to make it
    /// tape-derived instead.
    pub fn enable_virtio_rng(&mut self) {
        self.virtio_rng = Some(VirtioMmioTransport::new_rng(crate::layout::VIRTIO_MMIO_RNG_BASE));
        #[cfg(target_os = "linux")]
        {
            self.virtio_rng_queue = None;
            self.virtio_rng_entropy = SplitMix64::new(0);
        }
    }

    /// The virtio-rng transport, if [`Self::enable_virtio_rng`] has been called — the read access a
    /// caller needs to fetch `queue_ring_config` and drive `crate::virtio_queue::SplitVirtqueue`
    /// against real guest memory (`self.virtio_rng` is otherwise private to this module).
    pub fn virtio_rng(&self) -> Option<&VirtioMmioTransport> {
        self.virtio_rng.as_ref()
    }

    /// Reseed the virtio-rng device's byte stream — call once, right after
    /// [`Self::enable_virtio_rng`], before any guest code runs (mirrors `WorkClock::with_entropy_
    /// seed`'s "call once before boot" contract). A real caller derives `seed` from the run's own
    /// tape via its own domain-separated hash, keeping this stream independent of both the
    /// `rdrand`/`rdseed` sub-stream and the boot `SETUP_RNG_SEED` (todo.md §3.8).
    #[cfg(target_os = "linux")]
    pub fn seed_virtio_rng_entropy(&mut self, seed: u64) {
        self.virtio_rng_entropy = SplitMix64::new(seed);
    }

    /// Drain every virtqueue chain the driver has posted to `virtio_rng`'s sole queue since the
    /// last call, filling each writable descriptor with bytes drawn from the tape-seeded entropy
    /// stream — the actual virtio-rng device behavior (spec 1.1 §5.4: the device's only job is
    /// writing random data into whatever buffer the driver posts to `requestq`). A no-op (`Ok(0)`)
    /// if virtio-rng was never enabled, or its queue is not yet negotiated/ready.
    ///
    /// Requires real guest memory to walk the ring, unlike every other method on this type — that
    /// is exactly why this lives behind `#[cfg(target_os = "linux")]` and takes an explicit `mem`
    /// parameter rather than being invoked automatically from [`Bus::mmio_write`]: the [`Bus`]
    /// trait is shared with `baud-vcpu`'s exit dispatch and is deliberately memory-oblivious (see
    /// `virtio_mmio.rs`'s doc), so it cannot drive this itself. A caller — a real boot loop, once
    /// wired (todo.md §14 next-actions item 1's still-open "boot/cmdline/CLI wiring") — is expected
    /// to call this with the guest's real memory after an `MmioWrite` lands on `QueueNotify`; until
    /// then, nothing does, matching the "next real step, not stubbed here" framing this module's
    /// prior iterations left in place.
    #[cfg(target_os = "linux")]
    pub fn service_virtio_rng<M: GuestMemoryBackend>(&mut self, mem: &M) -> Result<u32, VirtqueueError> {
        let Some(transport) = self.virtio_rng.as_ref() else { return Ok(0) };
        let Some(config) = transport.queue_ring_config(0) else {
            self.virtio_rng_queue = None; // not ready (e.g. just reset): drop any stale cursor
            return Ok(0);
        };
        if self.virtio_rng_queue.as_ref().map(SplitVirtqueue::config) != Some(config) {
            // First negotiation, or the driver renegotiated (new addresses/size after a reset):
            // a stale cursor must never keep walking the old layout.
            self.virtio_rng_queue = Some(SplitVirtqueue::new(config));
        }
        let queue = self.virtio_rng_queue.as_mut().expect("just set above");
        let entropy = &mut self.virtio_rng_entropy;
        queue.process_available(mem, |buf| {
            for chunk in buf.chunks_mut(8) {
                let word = entropy.next_u64().to_le_bytes();
                chunk.copy_from_slice(&word[..chunk.len()]);
            }
        })
    }

    /// A [`DeviceBus`] reconstructed from a `Universe` snapshot's device row
    /// (`baud-snapshot::universe::DeviceState`, `RestoreStep::RestoreDevice` — deliberately left to
    /// the caller by `baud-snapshot::linux::restore`, since that crate does not know this device
    /// model's concrete types). `tape` is the run's whole tape (unchanged across the run's
    /// lifetime, same as [`with_tape`](Self::with_tape) — restore does not need a different tape,
    /// only a fast-forwarded cursor into the same one); `tape_cursor`/`console_output` are the
    /// captured [`DeviceState::tape_cursor`](baud_snapshot::DeviceState)/`console` fields.
    pub fn restore(tape: Vec<u8>, tape_cursor: u64, console_output: Vec<u8>) -> Self {
        let mut tape_bus = TapeBus::new(tape);
        tape_bus.device_mut().restore_cursor(tape_cursor);
        DeviceBus {
            console: Console::with_output(console_output),
            tape: tape_bus,
            cmos: Cmos,
            virtio_rng: None,
            #[cfg(target_os = "linux")]
            virtio_rng_queue: None,
            #[cfg(target_os = "linux")]
            virtio_rng_entropy: SplitMix64::new(0),
            fallback: OpenBusFallback,
        }
    }
}

impl Bus for DeviceBus {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        if Console::in_range(port).is_some() {
            self.console.pio_read(port, data);
        } else if TapeBus::in_range(port).is_some() {
            self.tape.pio_read(port, data);
        } else if Cmos::in_range(port) {
            self.cmos.pio_read(port, data);
        } else {
            self.fallback.pio_read(port, data);
        }
    }

    fn pio_write(&mut self, port: u16, data: &[u8]) {
        if Console::in_range(port).is_some() {
            self.console.pio_write(port, data);
        } else if TapeBus::in_range(port).is_some() {
            self.tape.pio_write(port, data);
        } else if Cmos::in_range(port) {
            self.cmos.pio_write(port, data);
        } else {
            self.fallback.pio_write(port, data);
        }
    }

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        if let Some(virtio_rng) = self.virtio_rng.as_mut() {
            if virtio_rng.in_range(addr).is_some() {
                virtio_rng.mmio_read(addr, data);
                return;
            }
        }
        self.fallback.mmio_read(addr, data);
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        if let Some(virtio_rng) = self.virtio_rng.as_mut() {
            if virtio_rng.in_range(addr).is_some() {
                virtio_rng.mmio_write(addr, data);
                return;
            }
        }
        self.fallback.mmio_write(addr, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_written_to_the_data_register_appears_in_output() {
        let mut console = Console::default();
        console.pio_write(COM1_BASE, b"h");
        console.pio_write(COM1_BASE, b"i");
        assert_eq!(console.output(), b"hi");
    }

    #[test]
    fn enqueued_input_is_readable_from_the_data_register_in_order() {
        let mut console = Console::default();
        let written = console.enqueue_input(b"hi");
        assert_eq!(written, 2);
        let mut lsr = [0u8; 1];
        console.pio_read(COM1_BASE + 5, &mut lsr);
        assert_eq!(lsr[0] & 0b0000_0001, 1, "LSR data-ready bit must be set once input is queued");
        let mut byte = [0u8; 1];
        console.pio_read(COM1_BASE, &mut byte);
        assert_eq!(byte, *b"h");
        console.pio_read(COM1_BASE, &mut byte);
        assert_eq!(byte, *b"i");
    }

    #[test]
    fn line_status_register_reports_transmitter_always_ready() {
        // Offset 5 (LSR) — bit 5 (THR empty) and bit 6 (idle) must both read set, or a real guest
        // UART driver would spin forever waiting for "ready to transmit" that never comes.
        let mut console = Console::default();
        let mut lsr = [0u8; 1];
        console.pio_read(COM1_BASE + 5, &mut lsr);
        assert_eq!(lsr[0] & 0b0110_0000, 0b0110_0000, "THR-empty and idle bits must both be set");
    }

    #[test]
    fn ports_outside_com1_are_open_bus_on_the_bare_console() {
        let mut console = Console::default();
        let mut data = [0u8; 2];
        console.pio_read(0x60, &mut data); // PS/2 keyboard controller port, not COM1
        assert_eq!(data, [OPEN_BUS_BYTE, OPEN_BUS_BYTE]);
    }

    #[test]
    fn device_bus_routes_com1_to_the_console_and_everything_else_to_open_bus() {
        let mut bus = DeviceBus::default();
        bus.pio_write(COM1_BASE, b"X");
        assert_eq!(bus.console.output(), b"X");

        let mut mmio_data = [0u8; 4];
        bus.mmio_read(0x1000_0000, &mut mmio_data);
        assert_eq!(mmio_data, [OPEN_BUS_BYTE; 4]);

        let mut other_port = [0u8; 1];
        bus.pio_read(0x80, &mut other_port); // POST diagnostic port, not COM1 or the tape device
        assert_eq!(other_port, [OPEN_BUS_BYTE]);
    }

    /// The bug this type exists to fix, made concrete: reading Status Register A's "Update In
    /// Progress" bit (bit 7) through the raw [`Cmos`] shim (not the fixed [`OPEN_BUS_BYTE`] a
    /// pre-fix unhandled port would have returned) must never read as set, or
    /// `mach_get_cmos_time`'s poll loop hangs forever (this crate's `linux::tests::
    /// double_boot_memory_identical` hung on exactly this, against real KVM hardware, before this
    /// device existed).
    #[test]
    fn cmos_status_register_a_never_reports_update_in_progress() {
        let mut cmos = Cmos;
        cmos.pio_write(CMOS_ADDR_PORT, &[0x0A]); // select Status Register A
        let mut value = [0xFFu8]; // start from a value that WOULD show UIP set, to prove it's overwritten
        cmos.pio_read(CMOS_DATA_PORT, &mut value);
        assert_eq!(value[0] & 0b1000_0000, 0, "UIP bit must read clear or the guest's poll loop hangs");
    }

    #[test]
    fn device_bus_routes_cmos_ports_to_the_cmos_shim_not_open_bus() {
        let mut bus = DeviceBus::default();
        let mut value = [0xFFu8];
        bus.pio_read(CMOS_DATA_PORT, &mut value);
        assert_eq!(value, [0], "CMOS data-port reads must not fall through to open-bus (0xFF)");
    }

    #[test]
    fn device_bus_routes_the_tape_device_window_to_the_tape_bus_not_open_bus() {
        use crate::tape_bus::{TapeBus, TAPE_DEVICE_BASE};
        use baud_tape_device::{reg, ControlOp};

        let mut bus = DeviceBus { tape: TapeBus::new(vec![0x42]), ..Default::default() };
        let mut data = [0u8; 1];
        bus.pio_read(TAPE_DEVICE_BASE + reg::DATA, &mut data);
        assert_eq!(data, [0x42], "tape device window must not fall through to open-bus");

        bus.pio_write(TAPE_DEVICE_BASE + reg::CONTROL, &[ControlOp::MarkBranch as u8]);
        assert_eq!(bus.tape.device_mut().drain_records().len(), 1);
    }

    #[test]
    fn device_bus_mmio_falls_through_to_open_bus_until_virtio_rng_is_enabled() {
        let mut bus = DeviceBus::default();
        let mut data = [0u8; 4];
        bus.mmio_read(crate::layout::VIRTIO_MMIO_RNG_BASE, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4], "no MMIO device is modeled until opted in");

        bus.enable_virtio_rng();
        bus.mmio_read(crate::layout::VIRTIO_MMIO_RNG_BASE, &mut data);
        assert_eq!(
            u32::from_le_bytes(data),
            crate::virtio_mmio::VIRTIO_MMIO_MAGIC,
            "once enabled, the window must route to the real transport, not open bus"
        );
        // An address just past the device's window still falls through to open bus.
        bus.mmio_read(crate::layout::VIRTIO_MMIO_RNG_BASE + crate::layout::VIRTIO_MMIO_RNG_LEN, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4]);
    }

    #[test]
    fn a_read_of_more_than_one_byte_only_the_first_byte_is_a_real_register_read() {
        let mut console = Console::default();
        let mut data = [0u8; 4];
        console.pio_read(COM1_BASE, &mut data); // DATA register, empty RX FIFO -> 0
        assert_eq!(&data[1..], &[OPEN_BUS_BYTE; 3]);
    }

    /// A restored console's output starts with the captured history, and further writes append
    /// after it — exactly what a straight run's `output()` would show at the same point.
    #[test]
    fn console_with_output_preserves_captured_history_and_appends_new_writes() {
        let mut console = Console::with_output(b"captured before snapshot, ".to_vec());
        assert_eq!(console.output(), b"captured before snapshot, ");
        // A real UART `OUT` instruction writes one byte at a time (same convention as
        // `a_byte_written_to_the_data_register_appears_in_output` above), so drive the write byte
        // by byte rather than handing `pio_write` a multi-byte slice.
        for &byte in b"after restore" {
            console.pio_write(COM1_BASE, &[byte]);
        }
        assert_eq!(console.output(), b"captured before snapshot, after restore");
    }

    /// `DeviceBus::restore` must reproduce both the tape cursor position and the console history —
    /// the two halves of `baud-snapshot::universe::DeviceState` this crate is responsible for
    /// reassembling (the third field, the tape bytes themselves, is not part of `DeviceState` at
    /// all: it is the run's own input, supplied by the caller here just as `with_tape` requires).
    #[test]
    fn device_bus_restore_reproduces_tape_cursor_and_console_history() {
        use crate::tape_bus::TAPE_DEVICE_BASE;
        use baud_tape_device::reg;

        let tape = vec![10, 20, 30, 40];
        let mut bus = DeviceBus::restore(tape, 2, b"hello".to_vec());
        assert_eq!(bus.console.output(), b"hello");

        // Cursor was restored to 2: the next tape read must be the third byte (30), not the first.
        let mut data = [0u8; 1];
        bus.pio_read(TAPE_DEVICE_BASE + reg::DATA, &mut data);
        assert_eq!(data, [30]);
    }
}

/// `DeviceBus::service_virtio_rng` — the mechanism todo.md §14 next-actions item 1 named as still
/// open after ralph iteration 24 ("nothing calls `SplitVirtqueue::process_available` automatically
/// from `QueueNotify` yet"): a real driver enumeration/negotiation sequence through `DeviceBus`'s
/// own `Bus` impl, followed by an explicit `service_virtio_rng(&mem)` call, actually drains the
/// posted descriptor chain and fills it with tape-seeded entropy bytes. Hardware-independent (pure
/// `vm-memory` `GuestMemoryMmap::from_ranges` anonymous-mmap memory, no KVM/perf), same convention
/// as `virtio_queue.rs`'s own tests — gated to Linux only since it needs real `vm-memory`.
#[cfg(all(test, target_os = "linux"))]
mod virtio_rng_service_tests {
    use super::*;
    use crate::virtio_mmio::{
        VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FEATURES_OK,
    };
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    type GuestMemory = GuestMemoryMmap<()>;

    const DESC_BASE: u64 = 0x1000;
    const AVAIL_BASE: u64 = 0x2000;
    const USED_BASE: u64 = 0x3000;
    const BUF_BASE: u64 = 0x4000;

    fn test_guest_mem() -> GuestMemory {
        GuestMemory::from_ranges(&[(GuestAddress(0), crate::layout::GUEST_RAM_SIZE)])
            .expect("anonymous-mmap guest memory for a unit test")
    }

    fn write_reg(bus: &mut DeviceBus, offset: u64, value: u32) {
        bus.mmio_write(crate::layout::VIRTIO_MMIO_RNG_BASE + offset, &value.to_le_bytes());
    }

    fn read_reg(bus: &mut DeviceBus, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        bus.mmio_read(crate::layout::VIRTIO_MMIO_RNG_BASE + offset, &mut data);
        u32::from_le_bytes(data)
    }

    /// Drives the real driver-enumeration/negotiation sequence (mirroring `virtio_mmio.rs`'s own
    /// `a_full_driver_enumeration_and_queue_setup_sequence_succeeds` test) through `DeviceBus`
    /// itself, then posts one writable descriptor of `len` bytes and notifies — everything a real
    /// virtio-rng driver's `probe` + one `hwrng_fillfn` request would do.
    fn negotiate_and_post_one_descriptor(bus: &mut DeviceBus, mem: &GuestMemory, len: u32) {
        write_reg(bus, 0x070, VIRTIO_STATUS_ACKNOWLEDGE);
        write_reg(bus, 0x070, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
        write_reg(bus, 0x014, 1); // DeviceFeaturesSel = word 1
        let offered = read_reg(bus, 0x010);
        write_reg(bus, 0x024, 1); // DriverFeaturesSel = word 1
        write_reg(bus, 0x020, offered);
        write_reg(bus, 0x070, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK);

        write_reg(bus, 0x030, 0); // QueueSel = 0
        write_reg(bus, 0x038, 256); // QueueNum
        write_reg(bus, 0x080, DESC_BASE as u32);
        write_reg(bus, 0x090, AVAIL_BASE as u32);
        write_reg(bus, 0x0a0, USED_BASE as u32);
        write_reg(bus, 0x044, 1); // QueueReady
        write_reg(
            bus,
            0x070,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK,
        );

        let mut raw = [0u8; 16];
        raw[0..8].copy_from_slice(&BUF_BASE.to_le_bytes());
        raw[8..12].copy_from_slice(&len.to_le_bytes());
        raw[12..14].copy_from_slice(&2u16.to_le_bytes()); // VIRTQ_DESC_F_WRITE
        raw[14..16].copy_from_slice(&0u16.to_le_bytes());
        mem.write_slice(&raw, GuestAddress(DESC_BASE)).expect("write descriptor");
        mem.write_slice(&1u16.to_le_bytes(), GuestAddress(AVAIL_BASE + 2)).expect("avail.idx = 1");
        mem.write_slice(&0u16.to_le_bytes(), GuestAddress(AVAIL_BASE + 4)).expect("avail.ring[0] = 0");

        write_reg(bus, 0x050, 0); // QueueNotify
    }

    fn read_used_buffer(mem: &GuestMemory, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        mem.read_slice(&mut buf, GuestAddress(BUF_BASE)).expect("read filled buffer");
        buf
    }

    #[test]
    fn service_virtio_rng_is_a_harmless_no_op_before_enable_ready_or_notify() {
        let mem = test_guest_mem();
        let mut bus = DeviceBus::default();
        assert_eq!(bus.service_virtio_rng(&mem).unwrap(), 0, "virtio-rng was never enabled");

        bus.enable_virtio_rng();
        assert_eq!(bus.service_virtio_rng(&mem).unwrap(), 0, "queue never negotiated/ready");
    }

    #[test]
    fn queue_notify_now_actually_drains_the_ring_with_entropy_bytes() {
        let mem = test_guest_mem();
        let mut bus = DeviceBus::default();
        bus.enable_virtio_rng();
        bus.seed_virtio_rng_entropy(42);
        negotiate_and_post_one_descriptor(&mut bus, &mem, 32);

        let processed = bus.service_virtio_rng(&mem).unwrap();
        assert_eq!(processed, 1);
        assert_eq!(read_reg(&mut bus, 0x044), 1, "queue is still ready after servicing");
        let written = read_used_buffer(&mem, 32);
        assert_ne!(written, vec![0u8; 32], "the buffer must actually be filled, not left zeroed");

        // A second call with no further driver activity drains nothing new.
        assert_eq!(bus.service_virtio_rng(&mem).unwrap(), 0);
    }

    #[test]
    fn the_same_seed_reproduces_the_identical_byte_stream() {
        let bytes_from = |seed: u64| {
            let mem = test_guest_mem();
            let mut bus = DeviceBus::default();
            bus.enable_virtio_rng();
            bus.seed_virtio_rng_entropy(seed);
            negotiate_and_post_one_descriptor(&mut bus, &mem, 24);
            bus.service_virtio_rng(&mem).unwrap();
            read_used_buffer(&mem, 24)
        };

        assert_eq!(bytes_from(7), bytes_from(7), "same seed must reproduce the identical byte stream");
        assert_ne!(bytes_from(7), bytes_from(8), "a different seed must change the byte stream");
    }

    #[test]
    fn resetting_and_renegotiating_rebuilds_the_stale_ring_cursor() {
        let mem = test_guest_mem();
        let mut bus = DeviceBus::default();
        bus.enable_virtio_rng();
        bus.seed_virtio_rng_entropy(1);
        negotiate_and_post_one_descriptor(&mut bus, &mem, 8);
        assert_eq!(bus.service_virtio_rng(&mem).unwrap(), 1);

        // Device reset (status = 0) clears queue readiness; a real driver would renegotiate with
        // potentially different ring addresses before posting again.
        write_reg(&mut bus, 0x070, 0);
        assert_eq!(bus.service_virtio_rng(&mem).unwrap(), 0, "queue is unready right after reset");

        const NEW_DESC_BASE: u64 = 0x5000;
        const NEW_AVAIL_BASE: u64 = 0x6000;
        const NEW_USED_BASE: u64 = 0x7000;
        const NEW_BUF_BASE: u64 = 0x8000;

        write_reg(&mut bus, 0x070, VIRTIO_STATUS_ACKNOWLEDGE);
        write_reg(&mut bus, 0x070, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
        write_reg(&mut bus, 0x014, 1);
        let offered = read_reg(&mut bus, 0x010);
        write_reg(&mut bus, 0x024, 1);
        write_reg(&mut bus, 0x020, offered);
        write_reg(
            &mut bus,
            0x070,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        );
        write_reg(&mut bus, 0x030, 0);
        write_reg(&mut bus, 0x038, 256);
        write_reg(&mut bus, 0x080, NEW_DESC_BASE as u32);
        write_reg(&mut bus, 0x090, NEW_AVAIL_BASE as u32);
        write_reg(&mut bus, 0x0a0, NEW_USED_BASE as u32);
        write_reg(&mut bus, 0x044, 1);
        write_reg(
            &mut bus,
            0x070,
            VIRTIO_STATUS_ACKNOWLEDGE
                | VIRTIO_STATUS_DRIVER
                | VIRTIO_STATUS_FEATURES_OK
                | VIRTIO_STATUS_DRIVER_OK,
        );

        let mut raw = [0u8; 16];
        raw[0..8].copy_from_slice(&NEW_BUF_BASE.to_le_bytes());
        raw[8..12].copy_from_slice(&8u32.to_le_bytes());
        raw[12..14].copy_from_slice(&2u16.to_le_bytes());
        raw[14..16].copy_from_slice(&0u16.to_le_bytes());
        mem.write_slice(&raw, GuestAddress(NEW_DESC_BASE)).unwrap();
        mem.write_slice(&1u16.to_le_bytes(), GuestAddress(NEW_AVAIL_BASE + 2)).unwrap();
        mem.write_slice(&0u16.to_le_bytes(), GuestAddress(NEW_AVAIL_BASE + 4)).unwrap();
        write_reg(&mut bus, 0x050, 0);

        let processed = bus.service_virtio_rng(&mem).unwrap();
        assert_eq!(processed, 1, "the rebuilt cursor must walk the new ring, not a stale one");
        let mut written = [0u8; 8];
        mem.read_slice(&mut written, GuestAddress(NEW_BUF_BASE)).unwrap();
        assert_ne!(written, [0u8; 8]);
    }
}
