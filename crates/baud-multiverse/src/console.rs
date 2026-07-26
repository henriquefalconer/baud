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
    /// this transport has no virtqueue-ring or interrupt-delivery implementation yet, see that
    /// module's doc).
    virtio_rng: Option<VirtioMmioTransport>,
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
    /// that address window to it instead of [`OpenBusFallback`].
    pub fn enable_virtio_rng(&mut self) {
        self.virtio_rng = Some(VirtioMmioTransport::new_rng(crate::layout::VIRTIO_MMIO_RNG_BASE));
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
