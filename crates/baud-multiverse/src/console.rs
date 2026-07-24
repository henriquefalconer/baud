// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The console device: a minimal 16550-compatible UART on COM1 (I/O ports 0x3f8-0x3ff), built on
// `vm_superio::Serial` — the crate specs/baud-multiverse.md §2 pins for exactly this purpose
// (`specs/baud-snapshot.md`'s "restore into a live shell" step re-wires the same device onto a PTY
// trigger later, §5/H5). At H1 baud only needs guest -> host output (the guest kernel's serial
// console), so the writer here is a plain in-memory buffer and interrupt delivery (IRQ4) uses a
// no-op-but-recording `Trigger` — no guest-visible console *input* is modeled yet (todo.md §3.6's
// subtractive rule: "down to a console plus the tape device", and the tape device is not built
// yet either).
//
// `vm_superio` has no non-dev dependencies at all (checked against its own Cargo.toml — not even
// `libc`), so unlike the rest of `linux/`, this whole module — including its tests — is
// hardware-independent and runs on this Windows dev machine with no KVM/perf, the same pattern
// `cpuid.rs`/`layout.rs`/`baud-vcpu`'s `boundary.rs` use.

use crate::tape_bus::TapeBus;
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

/// Composes [`Console`] (COM1) and [`TapeBus`] (the tape device, specs/baud-tape-device.md) with
/// [`OpenBusFallback`] for every other address — the device bus the boot flow's run loop
/// dispatches every exit through (`linux::Multiverse`). Matches todo.md §3.6's subtractive rule:
/// "down to a console plus the tape device."
#[derive(Default)]
pub struct DeviceBus {
    pub console: Console,
    pub tape: TapeBus,
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
        DeviceBus { console: Console::with_output(console_output), tape: tape_bus, fallback: OpenBusFallback }
    }
}

impl Bus for DeviceBus {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        if Console::in_range(port).is_some() {
            self.console.pio_read(port, data);
        } else if TapeBus::in_range(port).is_some() {
            self.tape.pio_read(port, data);
        } else {
            self.fallback.pio_read(port, data);
        }
    }

    fn pio_write(&mut self, port: u16, data: &[u8]) {
        if Console::in_range(port).is_some() {
            self.console.pio_write(port, data);
        } else if TapeBus::in_range(port).is_some() {
            self.tape.pio_write(port, data);
        } else {
            self.fallback.pio_write(port, data);
        }
    }

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.fallback.mmio_read(addr, data);
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
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
