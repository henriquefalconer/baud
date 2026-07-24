// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Adapts `baud_tape_device::TapeDevice` (pure, hardware-independent, deps = {baud-proto} only,
// per specs/baud-tape-device.md §2's "Rationale") onto `baud_vcpu::Bus`'s slice-based PIO/MMIO
// interface — the "served on the vCPU bus by baud-vcpu" line in that spec's §2 architecture
// diagram. This module is the only place a fixed guest-visible port range is chosen for the tape
// device; `TapeDevice` itself has no notion of "where" it lives on the bus (same split
// `console.rs` uses for COM1).
//
// Hardware-independent, like `console.rs`: `baud-tape-device` has no KVM/perf dependency at all,
// so this whole module — including its tests — runs on this Windows dev machine with no KVM.

use baud_tape_device::TapeDevice;
use baud_vcpu::{Bus, OPEN_BUS_BYTE};

/// Base I/O port for the tape device. An arbitrary but fixed choice, outside the legacy COM1
/// range (`console::COM1_BASE`, 0x3f8-0x3ff) this codebase already occupies — the in-guest driver
/// contract that will document this for real guest images (todo.md §4's "image contract",
/// `baud-packages`' tape-device shim) does not exist yet.
pub const TAPE_DEVICE_BASE: u16 = 0x0500;
/// Highest offset `TapeDevice` serves (`baud_tape_device::reg::STATUS` = 0x10) plus headroom for
/// future registers without immediately colliding with the next device.
pub const TAPE_DEVICE_LEN: u16 = 0x18;

/// Wraps a [`TapeDevice`] as a [`Bus`], anchored at [`TAPE_DEVICE_BASE`].
pub struct TapeBus {
    device: TapeDevice,
}

impl TapeBus {
    pub fn new(tape: Vec<u8>) -> Self {
        TapeBus { device: TapeDevice::new(tape) }
    }

    /// Read-only access to the underlying device (e.g. to call
    /// [`TapeDevice::drain_records`](baud_tape_device::TapeDevice::drain_records) or
    /// [`TapeDevice::cursor`](baud_tape_device::TapeDevice::cursor) from the run loop after each
    /// exit is served).
    pub fn device_mut(&mut self) -> &mut TapeDevice {
        &mut self.device
    }

    pub fn device(&self) -> &TapeDevice {
        &self.device
    }

    /// `port`'s offset within the tape device's window, or `None` if `port` is outside it.
    pub(crate) fn in_range(port: u16) -> Option<u16> {
        if (TAPE_DEVICE_BASE..TAPE_DEVICE_BASE + TAPE_DEVICE_LEN).contains(&port) {
            Some(port - TAPE_DEVICE_BASE)
        } else {
            None
        }
    }
}

impl Default for TapeBus {
    fn default() -> Self {
        TapeBus::new(Vec::new())
    }
}

impl Bus for TapeBus {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        match Self::in_range(port) {
            Some(off) => {
                // Real PIO `IN` is always a single byte at a time; a wider read reaching here
                // would itself be a modeling gap, so only the first byte is a real register read
                // (matches `console::Console::pio_read`'s convention for the same reason).
                if let Some(first) = data.first_mut() {
                    *first = self.device.pio_read(off);
                }
                if data.len() > 1 {
                    data[1..].fill(OPEN_BUS_BYTE);
                }
            }
            None => data.fill(OPEN_BUS_BYTE),
        }
    }

    fn pio_write(&mut self, port: u16, data: &[u8]) {
        if let Some(off) = Self::in_range(port) {
            if let Some(&byte) = data.first() {
                self.device.pio_write(off, byte);
            }
        }
        // Ports outside the window: absorbed silently, matching OpenBusFallback's write side.
    }

    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE); // the tape device is PIO-only in this milestone
    }

    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use baud_tape_device::{reg, ControlOp};
    use baud_proto::Msg;

    #[test]
    fn a_byte_read_at_the_data_offset_returns_the_next_tape_byte() {
        let mut bus = TapeBus::new(vec![0x11, 0x22]);
        let mut data = [0u8; 1];
        bus.pio_read(TAPE_DEVICE_BASE + reg::DATA, &mut data);
        assert_eq!(data, [0x11]);
        bus.pio_read(TAPE_DEVICE_BASE + reg::DATA, &mut data);
        assert_eq!(data, [0x22]);
    }

    #[test]
    fn a_write_at_the_control_offset_finalizes_a_record_visible_via_drain() {
        let mut bus = TapeBus::new(vec![]);
        bus.pio_write(TAPE_DEVICE_BASE + reg::CONTROL, &[ControlOp::MarkBranch as u8]);
        let records = bus.device_mut().drain_records();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0], Msg::MarkBranch { step: 0 }));
    }

    #[test]
    fn ports_outside_the_tape_device_window_are_open_bus() {
        let mut bus = TapeBus::new(vec![0xAB]);
        let mut data = [0u8; 2];
        bus.pio_read(0x60, &mut data); // PS/2 controller port, well outside the window
        assert_eq!(data, [OPEN_BUS_BYTE, OPEN_BUS_BYTE]);
    }

    #[test]
    fn a_read_of_more_than_one_byte_only_the_first_byte_is_a_real_register_read() {
        let mut bus = TapeBus::new(vec![7, 8, 9]);
        let mut data = [0u8; 4];
        bus.pio_read(TAPE_DEVICE_BASE + reg::DATA, &mut data);
        assert_eq!(data[0], 7);
        assert_eq!(&data[1..], &[OPEN_BUS_BYTE; 3]);
    }
}
