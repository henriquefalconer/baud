// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

//! Minimal deterministic I/O APIC register window for ACPI PCI INTx routing.
//!
//! Linux needs an interrupt domain when the FADT advertises hardware-reduced ACPI.  The device
//! does not raise interrupts itself.  The run loop still injects the vector at a measured boundary,
//! while this model records the redirection table Linux programs and exposes the vector selected
//! for each GSI.

use baud_vcpu::{Bus, OPEN_BUS_BYTE};

const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;
const REG_ID: u8 = 0x00;
const REG_VERSION: u8 = 0x01;
const REG_ARB: u8 = 0x02;
const REDIR_BASE: u8 = 0x10;
const REDIR_COUNT: usize = 24;
const MASKED: u64 = 1 << 16;

#[derive(Debug, Clone, Copy)]
pub struct IoApic {
    register: u8,
    redirection: [u64; REDIR_COUNT],
    asserted: [bool; REDIR_COUNT],
    remote_irr: [bool; REDIR_COUNT],
}

impl Default for IoApic {
    fn default() -> Self {
        Self {
            register: 0,
            redirection: [MASKED; REDIR_COUNT],
            asserted: [false; REDIR_COUNT],
            remote_irr: [false; REDIR_COUNT],
        }
    }
}

impl IoApic {
    pub const WINDOW_LEN: u64 = 0x20;

    pub fn in_range(addr: u64) -> Option<u64> {
        let base = crate::layout::IOAPIC_MMIO_BASE;
        if addr >= base && addr < base + Self::WINDOW_LEN {
            Some(addr - base)
        } else {
            None
        }
    }

    pub fn vector_for_irq(&self, irq: u8) -> Option<u8> {
        let entry = *self.redirection.get(irq as usize)?;
        if entry & MASKED != 0 {
            return None;
        }
        Some((entry & 0xff) as u8)
    }

    /// Assert a level-triggered PCI INTx line after publishing a used-ring entry.
    pub fn assert_irq(&mut self, irq: u8) {
        let index = irq as usize;
        if let Some(line) = self.asserted.get_mut(index) {
            *line = true;
        }
        if let Some(remote) = self.remote_irr.get_mut(index) {
            *remote = true;
        }
    }

    /// Clear the guest-visible line when the legacy virtio transport's ISR is read. A level
    /// interrupt's remote-IRR remains set until the APIC EOI, matching the IOAPIC contract.
    pub fn clear_irq(&mut self, irq: u8) {
        if let Some(line) = self.asserted.get_mut(irq as usize) {
            *line = false;
        }
    }

    /// Complete the APIC acknowledgement for every modeled level-triggered line.
    pub fn eoi_all(&mut self) {
        self.remote_irr.fill(false);
    }

    pub fn irq_asserted(&self, irq: u8) -> bool {
        self.asserted.get(irq as usize).copied().unwrap_or(false)
    }

    pub fn clear_all_asserted(&mut self) {
        self.asserted.fill(false);
    }

    fn read_register(&self, register: u8) -> u32 {
        match register {
            REG_ID => 0,
            REG_VERSION => ((REDIR_COUNT as u32 - 1) << 16) | 0x11,
            REG_ARB => 0,
            r if r >= REDIR_BASE => {
                let index = usize::from(r - REDIR_BASE);
                let entry = index / 2;
                if entry >= REDIR_COUNT {
                    0
                } else if index % 2 == 0 {
                    self.redirection[entry] as u32
                } else {
                    (self.redirection[entry] >> 32) as u32
                }
            }
            _ => 0,
        }
    }

    fn write_register(&mut self, register: u8, value: u32) {
        if register < REDIR_BASE {
            return;
        }
        let index = usize::from(register - REDIR_BASE);
        let entry = index / 2;
        if entry >= REDIR_COUNT {
            return;
        }
        if index % 2 == 0 {
            self.redirection[entry] = (self.redirection[entry] & !0xffff_ffff) | u64::from(value);
        } else {
            self.redirection[entry] =
                (self.redirection[entry] & 0xffff_ffff) | (u64::from(value) << 32);
        }
    }
}

impl Bus for IoApic {
    fn pio_read(&mut self, _port: u16, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE);
    }
    fn pio_write(&mut self, _port: u16, _data: &[u8]) {}

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        let Some(offset) = Self::in_range(addr) else {
            data.fill(OPEN_BUS_BYTE);
            return;
        };
        let value = if offset == IOREGSEL {
            u32::from(self.register)
        } else if offset == IOWIN {
            self.read_register(self.register)
        } else {
            0
        };
        let bytes = value.to_le_bytes();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = bytes.get(i).copied().unwrap_or(OPEN_BUS_BYTE);
        }
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        let Some(offset) = Self::in_range(addr) else {
            return;
        };
        let mut bytes = [0u8; 4];
        let n = data.len().min(4);
        bytes[..n].copy_from_slice(&data[..n]);
        let value = u32::from_le_bytes(bytes);
        if offset == IOREGSEL {
            self.register = value as u8;
        } else if offset == IOWIN {
            self.write_register(self.register, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(io: &mut IoApic, offset: u64, value: u32) {
        io.mmio_write(
            crate::layout::IOAPIC_MMIO_BASE + offset,
            &value.to_le_bytes(),
        );
    }

    #[test]
    fn reports_version_and_tracks_linux_redirection_entry() {
        let mut io = IoApic::default();
        write(&mut io, IOREGSEL, 0x01);
        let mut data = [0; 4];
        io.mmio_read(crate::layout::IOAPIC_MMIO_BASE + IOWIN, &mut data);
        assert_eq!(u32::from_le_bytes(data), (23 << 16) | 0x11);
        write(&mut io, IOREGSEL, u32::from(REDIR_BASE + 11 * 2));
        write(&mut io, IOWIN, 0x3b);
        assert_eq!(io.vector_for_irq(11), Some(0x3b));
        io.assert_irq(11);
        assert!(io.irq_asserted(11));
        io.clear_irq(11);
        assert!(!io.irq_asserted(11));
    }

    #[test]
    fn eoi_clear_releases_all_level_triggered_lines() {
        let mut io = IoApic::default();
        io.assert_irq(10);
        io.assert_irq(11);
        io.clear_all_asserted();
        assert!(!io.irq_asserted(10));
        assert!(!io.irq_asserted(11));
        io.eoi_all();
    }
}
