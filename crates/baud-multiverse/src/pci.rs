// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Minimal PCI configuration-space access — "configuration mechanism #1", the legacy
// 0xCF8 (CONFIG_ADDRESS) / 0xCFC (CONFIG_DATA) port pair every x86 BIOS/kernel falls back to when
// no MCFG/ECAM table is present (specs/baud-ubuntu.md: "PCI (MCFG ECAM or legacy 0xCF8/0xCFC)") —
// the first step toward H9 (todo.md §10: booting a stock Ubuntu 18.04.1 guest, whose initrd
// carries `virtio_pci`/`virtio_blk` and enumerates PCI to find its root disk, unlike baud's
// existing virtio-mmio devices, which the guest finds via a `virtio_mmio.device=` cmdline
// parameter instead and never touch PCI at all — `pic8259.rs`'s doc).
//
// Models exactly one device today: a host bridge at bus 0, device 0, function 0 — enough for an
// unmodified Linux guest's PCI core (`pci_scan_bus`) to enumerate bus 0 and terminate cleanly.
// Every other bus/device/function reads back all-ones (0xFFFF_FFFF), the PCI Local Bus spec's
// architectural "nothing here" signal (§6.1: a vendor ID of 0xFFFF means "device not present") —
// so an unmodified guest's scan stops exactly where baud has not yet modeled a device, the same
// "no unmodeled exit is silent" determinism discipline applied to bus enumeration rather than a
// VM exit (this module never returns a `DeterminismHole`: every config-space read/write, for any
// bus/device/function, resolves to a computed value, matching real hardware's own behavior for an
// absent device).
//
// A guest that never touches PCI (baud's existing minimal-guest cmdline sets `pci=off`,
// `linux/bootparams.rs`) never exercises this at all — Linux's own `pci=off` skips config-space
// probing entirely, so this device sits dormant, exactly like `Pic8259` sits dormant for a guest
// with no ISA IRQs to request.
//
// Deliberately out of scope here (future H9 work, not this module): MCFG/ECAM (the memory-mapped
// mechanism #2 for full 4096-byte extended config space), any device other than the host bridge
// (a virtio-pci block device needs its own BAR-backed MMIO/PIO window plus a `DeviceBus` slot to
// answer it, same pattern as `virtio_mmio.rs`), and ACPI (a real Ubuntu boot also wants a minimal
// RSDP/RSDT/FADT/DSDT/MADT, `specs/baud-ubuntu.md`'s "machine additions" list — unrelated to
// config-space access itself).

use baud_vcpu::{Bus, OPEN_BUS_BYTE};

/// `CONFIG_ADDRESS`, a 4-byte I/O port (PCI Local Bus spec §3.2.2.3.2).
pub const PCI_CONFIG_ADDRESS: u16 = 0x0CF8;
/// `CONFIG_DATA`, a 4-byte I/O port aliased onto whichever function/register `CONFIG_ADDRESS`
/// currently names.
pub const PCI_CONFIG_DATA: u16 = 0x0CFC;

/// `CONFIG_ADDRESS` bit 31 ("enable"). Real hardware ignores `CONFIG_DATA` while this is clear;
/// baud does not need to enforce that (every guest that touches `CONFIG_DATA` at all sets it
/// first, per the standard access pattern), but it is still masked out of the decoded tuple below
/// so a guest's choice of this bit never perturbs which device/register gets answered.
const CONFIG_ENABLE: u32 = 1 << 31;

/// A decoded `CONFIG_ADDRESS` value (PCI Local Bus spec §3.2.2.3.2: bits 23:16 bus, bits 15:11
/// device, bits 10:8 function, bits 7:2 register-number, bits 1:0 reserved/always 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigAddress {
    bus: u8,
    device: u8,
    function: u8,
    /// Byte offset within the 256-byte configuration space, always a multiple of 4 under
    /// mechanism #1.
    register: u8,
}

impl ConfigAddress {
    fn decode(raw: u32) -> Self {
        ConfigAddress {
            bus: ((raw >> 16) & 0xFF) as u8,
            device: ((raw >> 11) & 0x1F) as u8,
            function: ((raw >> 8) & 0x07) as u8,
            register: (raw & 0xFC) as u8,
        }
    }
}

/// The one device this bridge models: a host bridge at 00:00.0. Vendor/device ID `0x1B36`/`0x0000`
/// is Red Hat, Inc.'s QEMU-project vendor ID — already this ecosystem's convention for a
/// synthetic virtual-machine device (virtio's own devices register under the sibling `0x1AF4`),
/// never a real Intel/AMD host-bridge ID, since baud never claims to be real silicon. Class code
/// `0x0600_00` is "Bridge, Host bridge" (PCI Local Bus spec Appendix D, class 06h/subclass 00h),
/// header type 0 (single-function, not a PCI-PCI bridge) — the minimum `pci_scan_bus` needs to
/// enumerate bus 0 and stop; a real host bridge is also not bound to any generic Linux driver,
/// only enumerated, so no vendor/device-ID-specific quirk is needed for boot to proceed.
const HOST_BRIDGE_VENDOR_ID: u16 = 0x1B36;
const HOST_BRIDGE_DEVICE_ID: u16 = 0x0000;
/// Class code (bits 31:8 of the class/revision register) — class 06h, subclass 00h, no
/// programming interface.
const HOST_BRIDGE_CLASS_CODE: u32 = 0x0006_0000;

/// Configuration-space register offsets this bridge answers with a non-zero value (PCI Local Bus
/// spec §6.1, "Type 0" header) — every other offset in the 256-byte space reads back `0`, matching
/// an otherwise-reserved/unimplemented register on real hardware (a `0` header-type/BIST/latency/
/// cache-line-size byte is exactly what a single-function, non-bridge-capable device reports).
const REG_VENDOR_DEVICE: u8 = 0x00; // vendor ID (bits 15:0), device ID (bits 31:16)
const REG_CLASS_REVISION: u8 = 0x08; // revision ID (bits 7:0), class code (bits 31:8)

/// Configuration mechanism #1: a `CONFIG_ADDRESS` latch plus a `CONFIG_DATA` window onto whichever
/// function that latch currently names.
#[derive(Debug, Clone, Copy, Default)]
pub struct PciHostBridge {
    /// The last value written to `CONFIG_ADDRESS` — real hardware also just latches this
    /// unconditionally, decoding it fresh on every `CONFIG_DATA` access rather than at write time.
    config_address: u32,
}

impl PciHostBridge {
    pub(crate) fn in_range(port: u16) -> bool {
        (PCI_CONFIG_ADDRESS..PCI_CONFIG_ADDRESS + 4).contains(&port)
            || (PCI_CONFIG_DATA..PCI_CONFIG_DATA + 4).contains(&port)
    }

    /// The dword this bridge's one modeled device (00:00.0) presents at `register`, or
    /// `0xFFFF_FFFF` for any other `bus`/`device`/`function` — the value a real absent device
    /// reads back as (PCI Local Bus spec §6.1).
    fn config_read_dword(addr: ConfigAddress) -> u32 {
        if addr.bus != 0 || addr.device != 0 || addr.function != 0 {
            return 0xFFFF_FFFF;
        }
        match addr.register {
            REG_VENDOR_DEVICE => (HOST_BRIDGE_VENDOR_ID as u32) | ((HOST_BRIDGE_DEVICE_ID as u32) << 16),
            REG_CLASS_REVISION => HOST_BRIDGE_CLASS_CODE, // revision ID 0 in the low byte
            _ => 0,
        }
    }
}

/// Copy up to 4 bytes of `src` starting at byte `offset` into `dst` — models a real dword register
/// answering a narrower (byte/word) access at an unaligned port within its 4-byte window (e.g. a
/// guest reading only the device-ID half-word at `CONFIG_DATA + 2`). Bytes past the end of `src`
/// (an `offset` a legal 4-byte-wide port can never actually produce, but kept total instead of
/// panicking) read back [`OPEN_BUS_BYTE`], matching this crate's open-bus convention elsewhere.
fn copy_window(src: &[u8; 4], offset: usize, dst: &mut [u8]) {
    for (i, b) in dst.iter_mut().enumerate() {
        *b = src.get(offset + i).copied().unwrap_or(OPEN_BUS_BYTE);
    }
}

/// The write-side counterpart of [`copy_window`]: bytes of `src` land into `dst` starting at byte
/// `offset`, any that would fall past `dst`'s end are dropped rather than panicking.
fn write_window(dst: &mut [u8; 4], offset: usize, src: &[u8]) {
    for (i, &b) in src.iter().enumerate() {
        if let Some(slot) = dst.get_mut(offset + i) {
            *slot = b;
        }
    }
}

impl Bus for PciHostBridge {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        debug_assert!(Self::in_range(port));
        if (PCI_CONFIG_ADDRESS..PCI_CONFIG_ADDRESS + 4).contains(&port) {
            let bytes = self.config_address.to_le_bytes();
            copy_window(&bytes, (port - PCI_CONFIG_ADDRESS) as usize, data);
        } else {
            let addr = ConfigAddress::decode(self.config_address & !CONFIG_ENABLE);
            let bytes = Self::config_read_dword(addr).to_le_bytes();
            copy_window(&bytes, (port - PCI_CONFIG_DATA) as usize, data);
        }
    }

    fn pio_write(&mut self, port: u16, data: &[u8]) {
        debug_assert!(Self::in_range(port));
        if (PCI_CONFIG_ADDRESS..PCI_CONFIG_ADDRESS + 4).contains(&port) {
            let mut bytes = self.config_address.to_le_bytes();
            write_window(&mut bytes, (port - PCI_CONFIG_ADDRESS) as usize, data);
            self.config_address = u32::from_le_bytes(bytes);
        }
        // CONFIG_DATA writes: baud models no writable register on its one device (a host bridge
        // has none a guest needs to change), so they are silently absorbed — real hardware
        // returns the same behavior for a read-only register.
    }

    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE);
    }

    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_u32(bus: &mut PciHostBridge, port: u16, value: u32) {
        bus.pio_write(port, &value.to_le_bytes());
    }

    fn read_u32(bus: &mut PciHostBridge, port: u16) -> u32 {
        let mut data = [0u8; 4];
        bus.pio_read(port, &mut data);
        u32::from_le_bytes(data)
    }

    /// `CONFIG_ADDRESS` selecting bus 0 / device 0 / function 0 / register 0 (vendor/device ID),
    /// with the enable bit set — the exact dword a real `pci_bus_read_config_dword` issues first
    /// when probing 00:00.0.
    fn select_host_bridge(register: u8) -> u32 {
        CONFIG_ENABLE | ((register as u32) & 0xFC)
    }

    #[test]
    fn config_address_read_back_matches_last_write() {
        let mut bus = PciHostBridge::default();
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, 0x8000_0008);
        assert_eq!(read_u32(&mut bus, PCI_CONFIG_ADDRESS), 0x8000_0008);
    }

    #[test]
    fn host_bridge_vendor_and_device_id_are_present() {
        let mut bus = PciHostBridge::default();
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_host_bridge(REG_VENDOR_DEVICE));
        let dword = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!(dword & 0xFFFF, HOST_BRIDGE_VENDOR_ID as u32, "vendor ID in the low half-word");
        assert_eq!(dword >> 16, HOST_BRIDGE_DEVICE_ID as u32, "device ID in the high half-word");
        assert_ne!(dword & 0xFFFF, 0xFFFF, "a present device must never report vendor ID 0xFFFF");
    }

    #[test]
    fn host_bridge_class_code_is_bridge_host() {
        let mut bus = PciHostBridge::default();
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_host_bridge(REG_CLASS_REVISION));
        let dword = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!(dword & 0xFF, 0, "revision ID 0 in the low byte");
        assert_eq!((dword >> 8) & 0xFF, 0x00, "subclass 00h (host)");
        assert_eq!((dword >> 16) & 0xFF, 0x06, "class 06h (bridge)");
        assert_eq!(dword, HOST_BRIDGE_CLASS_CODE, "revision 0 plus the class code, bits 31:8");
    }

    #[test]
    fn absent_device_reads_all_ones() {
        let mut bus = PciHostBridge::default();
        for (bus_no, device, function) in [(0u8, 1u8, 0u8), (0, 0, 1), (1, 0, 0)] {
            let raw = CONFIG_ENABLE
                | ((bus_no as u32) << 16)
                | ((device as u32) << 11)
                | ((function as u32) << 8);
            write_u32(&mut bus, PCI_CONFIG_ADDRESS, raw);
            assert_eq!(
                read_u32(&mut bus, PCI_CONFIG_DATA),
                0xFFFF_FFFF,
                "bus {bus_no} device {device} function {function} must read back as absent"
            );
        }
    }

    #[test]
    fn unimplemented_register_reads_zero_not_all_ones() {
        // Only vendor ID 0xFFFF means "absent" — every other unimplemented register on a real,
        // present device reads back 0, never all-ones (which would misreport the device as gone).
        let mut bus = PciHostBridge::default();
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_host_bridge(0x10)); // BAR0, unimplemented here
        assert_eq!(read_u32(&mut bus, PCI_CONFIG_DATA), 0);
    }

    #[test]
    fn narrow_word_read_of_device_id_matches_the_dword_high_half() {
        // A guest reading only the 16-bit device-ID half-word at CONFIG_DATA+2 (real kernels do
        // this via `inw`), not the full dword.
        let mut bus = PciHostBridge::default();
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_host_bridge(REG_VENDOR_DEVICE));
        let mut half = [0u8; 2];
        bus.pio_read(PCI_CONFIG_DATA + 2, &mut half);
        assert_eq!(u16::from_le_bytes(half), HOST_BRIDGE_DEVICE_ID);
    }

    #[test]
    fn config_data_write_is_absorbed_without_changing_subsequent_reads() {
        let mut bus = PciHostBridge::default();
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_host_bridge(REG_VENDOR_DEVICE));
        write_u32(&mut bus, PCI_CONFIG_DATA, 0xDEAD_BEEF); // must not corrupt the read-only register
        assert_eq!(
            read_u32(&mut bus, PCI_CONFIG_DATA) & 0xFFFF,
            HOST_BRIDGE_VENDOR_ID as u32,
            "a CONFIG_DATA write to a read-only register must not change what a later read sees"
        );
    }

    #[test]
    fn in_range_covers_all_four_bytes_of_each_port_and_nothing_else() {
        for port in PCI_CONFIG_ADDRESS..PCI_CONFIG_ADDRESS + 4 {
            assert!(PciHostBridge::in_range(port));
        }
        for port in PCI_CONFIG_DATA..PCI_CONFIG_DATA + 4 {
            assert!(PciHostBridge::in_range(port));
        }
        assert!(!PciHostBridge::in_range(PCI_CONFIG_ADDRESS - 1));
        assert!(!PciHostBridge::in_range(PCI_CONFIG_DATA + 4));
    }
}
