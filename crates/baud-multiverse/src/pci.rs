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
// Two devices now exist beyond the host bridge: [`PciVirtioFunction`] models the configuration-
// space header (vendor/device ID, class code, one I/O-space BAR0 with the real BAR
// sizing/assignment protocol, interrupt line/pin) for a virtio-pci *legacy* function — one at
// 00:01.0 for entropy (`todo.md` §14 item 5(a)), one at 00:02.0 for the block device
// (item 5(b)) — the config-space half of `crate::virtio_pci`'s transport, which owns the actual
// I/O-port register block each device's BAR0 ends up pointing at. `PciHostBridge` itself still
// answers only the enumeration/config-space side; it never touches either transport directly,
// only exposing [`PciHostBridge::virtio_io_base`]/[`PciHostBridge::virtio_blk_io_base`] so a caller
// (`console.rs`'s `DeviceBus`) can keep each transport's own idea of its I/O window synchronized
// with whatever base the guest has assigned.
//
// Deliberately still out of scope here (future H9 work): MCFG/ECAM (the memory-mapped mechanism
// #2 for full 4096-byte extended config space) and ACPI (a real Ubuntu boot also wants a minimal
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
/// Class code (bits 31:8 of the class/revision register: bits 15:8 Prog IF, bits 23:16 Sub-Class,
/// bits 31:24 Base Class, PCI Local Bus spec §6.2.1) — class 06h (bridge), subclass 00h (host), no
/// programming interface. **Real-hardware bug, found and fixed**: this used to be `0x0006_0000`
/// (Base Class and Sub-Class swapped — Base Class 0x06 landed in the Sub-Class byte at offset
/// 0x0A instead of the Base Class byte at offset 0x0B), which every existing unit test asserted as
/// correct (self-consistently checking the same swapped byte positions) but which a real,
/// unmodified Linux kernel's `pci_sanity_check()` (`arch/x86/pci/direct.c`) rejects outright: it
/// reads the 16-bit `PCI_CLASS_DEVICE` word at offset 0x0A expecting exactly `PCI_CLASS_BRIDGE_HOST`
/// (`0x0600`), and the swapped byte order answered `0x0006` instead, so `raw_pci_ops` was never set
/// at all ("PCI: Fatal: No config space access function found") — confirmed by booting
/// `tests/fixtures/linux-guest/virtio_blk_init.c` on real `/dev/kvm` before this fix.
const HOST_BRIDGE_CLASS_CODE: u32 = 0x0600_0000;

/// Configuration-space register offsets this bridge answers with a non-zero value (PCI Local Bus
/// spec §6.1, "Type 0" header) — every other offset in the 256-byte space reads back `0`, matching
/// an otherwise-reserved/unimplemented register on real hardware (a `0` header-type/BIST/latency/
/// cache-line-size byte is exactly what a single-function, non-bridge-capable device reports).
const REG_VENDOR_DEVICE: u8 = 0x00; // vendor ID (bits 15:0), device ID (bits 31:16)
const REG_CLASS_REVISION: u8 = 0x08; // revision ID (bits 7:0), class code (bits 31:8)
const REG_HEADER_BIST: u8 = 0x0C; // cache-line-size/latency-timer/header-type/BIST
const REG_BAR0: u8 = 0x10;
const REG_SUBSYSTEM: u8 = 0x2C; // subsystem vendor ID (bits 15:0), subsystem ID (bits 31:16)
const REG_INTERRUPT: u8 = 0x3C; // interrupt line (bits 7:0), interrupt pin (bits 15:8)

/// virtio's own PCI-SIG-assigned vendor ID (never Red Hat's QEMU-project ID the host bridge above
/// uses) — real virtio-pci hardware and every virtio guest driver's `pci_device_id` table key off
/// this exact value.
const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
/// Legacy transitional virtio-pci device IDs are `0x1000 + <virtio device type>` (virtio spec,
/// Legacy Interface: A Note on Virtio Device Discovery).
const VIRTIO_LEGACY_DEVICE_ID_BASE: u16 = 0x1000;
/// PCI class code `0xFF` ("device does not fit any defined class", PCI Local Bus spec Appendix D)
/// — virtio's entropy device has no PCI-defined class of its own, and real virtio-pci-legacy
/// hardware uses exactly this catch-all for devices without a better match. Base Class 0xFF,
/// Sub-Class 0x00, Prog IF 0x00 (bits 31:24 / 23:16 / 15:8 respectively, PCI Local Bus spec
/// §6.2.1) — matches Linux's `PCI_CLASS_OTHERS` (`0xff00` as the 16-bit base<<8|sub word).
/// **Real bug, found by inspection and fixed**: this used to be `0x00FF_0000` — the exact same
/// Base/Sub-Class byte swap [`HOST_BRIDGE_CLASS_CODE`]'s doc describes (0xFF landed in the
/// Sub-Class byte at offset 0x0A instead of the Base Class byte at offset 0x0B). Unlike the host
/// bridge, no real virtio-rng-over-PCI driver test exists yet to have caught this the same way, so
/// this fix is reasoned from the spec/Linux header value alone, not hardware-confirmed.
const VIRTIO_UNCLASSIFIED_CODE: u32 = 0xFF00_0000;
/// Class 01h (mass storage controller), subclass 80h ("other mass storage controller"), prog-if
/// 00h — the real PCI-defined class virtio-blk-pci hardware uses (PCI Local Bus spec Appendix D;
/// unlike the entropy device, mass storage has a real class of its own, so this is not the
/// catch-all [`VIRTIO_UNCLASSIFIED_CODE`]).
const VIRTIO_BLK_CLASS_CODE: u32 = 0x0180_0000;
/// The I/O-space BAR indicator (bit 0 of a BAR register) plus the size this bridge advertises for
/// [`PciVirtioFunction::bar0_size`] — matches `crate::virtio_pci::VIRTIO_PCI_IO_WINDOW_LEN`
/// exactly, so the transport's own dispatch window and what the guest's PCI core believes it
/// sized never disagree.
const BAR_IO_SPACE_BIT: u32 = 0x1;
/// `Interrupt Pin` value `1` = `INTA#` (PCI Local Bus spec §6.2.4) — baud's virtio functions each
/// claim exactly one interrupt pin, never sharing a line with anything else in this model.
const INTERRUPT_PIN_INTA: u32 = 1;
/// Legacy ISA IRQ lines baud pre-routes each virtio-pci-legacy function's `Interrupt Line`
/// register to at construction, standing in for what a real BIOS's PIRQ router (or ACPI's
/// `_PRT`) would normally program before the OS boots — direct-boot Linux has neither, so nothing
/// else ever assigns one (see [`PciVirtioFunction::interrupt_line`]'s doc for the real-hardware
/// failure this fixes). Both are otherwise-unused legacy IRQs in every existing fixture (no i8042/
/// RTC/ATA modeled here, and the periodic-timer engine injects the LAPIC's own
/// `LOCAL_TIMER_VECTOR` directly, never a legacy IRQ0 PIT tick), and distinct from each other so
/// the two devices never share a line.
const VIRTIO_RNG_DEFAULT_IRQ_LINE: u8 = 10;
const VIRTIO_BLK_DEFAULT_IRQ_LINE: u8 = 11;

/// One PCI function's configuration-space header for a virtio-pci *legacy* device — the
/// enumeration-and-BAR-assignment half of `crate::virtio_pci::VirtioPciTransport`; see this
/// module's own doc for why the two are split. Lives at 00:01.0, the first slot past the host
/// bridge.
#[derive(Debug, Clone, Copy)]
pub struct PciVirtioFunction {
    /// The virtio device-type id (spec §5's device id table, e.g. `4` for entropy) — determines
    /// this function's PCI Device ID and Subsystem ID per the legacy convention.
    device_kind: u32,
    /// A fixed class-code dword (see [`VIRTIO_UNCLASSIFIED_CODE`]'s doc for why entropy uses it);
    /// a future device on this same machinery (e.g. virtio-blk) would pass its own real class.
    class_code: u32,
    /// BAR0's advertised size in bytes — must be a power of two (PCI BAR sizing depends on it)
    /// and must match the transport's own window length or the guest will address bytes the
    /// transport never answers.
    bar0_size: u32,
    /// The guest-assigned I/O base, or `0` before any guest write (real hardware: an unassigned
    /// BAR decodes no bus cycles, matching `0` here never being treated as a valid base by
    /// [`Self::io_base`]).
    bar0_base: u32,
    /// Set for exactly as long as the guest is mid-way through the BAR-sizing protocol (having
    /// just written all-ones and not yet written a real value back) — real hardware has no
    /// separate flag for this; a raw register can't distinguish "the guest wants the size" from
    /// "the guest wants to point the BAR at address `0xFFFF_FFFC`", so this bridge tracks it
    /// explicitly instead of misinterpreting an all-ones write as a real (nonsensical) base.
    bar0_sizing: bool,
    /// `Interrupt Line` (PCI Local Bus spec §6.2.4) — guest-writable, but seeded with a nonzero
    /// default at construction (see [`Self::new`]'s `default_interrupt_line` param): baud never
    /// reads this field back to pick an injection vector itself (a caller always names the vector
    /// explicitly), but the *guest*'s own `pci_read_irq()` (`drivers/pci/probe.c`) does, at
    /// enumeration time, straight from this register — and on a direct-boot kernel with no BIOS/
    /// ACPI/`$PIR` table to program it, nothing else would ever give this register a real value.
    /// Real-hardware finding: leaving it at `0` (the pre-fix default) makes a real
    /// `virtio_pci_legacy`/`virtio_blk` driver print "can't find IRQ for PCI INT A" and fail probe
    /// with `-ENOSPC`, confirmed booting `tests/fixtures/linux-guest/virtio_blk_init.c` — the same
    /// "no BIOS exists, so the VMM must pre-program what a BIOS normally would" role baud already
    /// plays for e.g. `boot_params`/e820 (§4.2).
    interrupt_line: u8,
}

impl PciVirtioFunction {
    fn new(device_kind: u32, class_code: u32, bar0_size: u32, default_interrupt_line: u8) -> Self {
        PciVirtioFunction {
            device_kind,
            class_code,
            bar0_size,
            bar0_base: 0,
            bar0_sizing: false,
            interrupt_line: default_interrupt_line,
        }
    }

    fn device_id(&self) -> u16 {
        VIRTIO_LEGACY_DEVICE_ID_BASE + self.device_kind as u16
    }

    fn bar0_read(&self) -> u32 {
        if self.bar0_sizing {
            // PCI BAR sizing protocol (PCI Local Bus spec §6.2.5.1): a device reports its region
            // size by answering an all-ones write with the size mask in the high bits rather than
            // echoing the all-ones value back — bit 0 is always 1 here (I/O-space indicator), bit
            // 1 stays reserved-0, and the size mask occupies bits 31:2.
            (!(self.bar0_size - 1) & 0xFFFF_FFFC) | BAR_IO_SPACE_BIT
        } else {
            self.bar0_base | BAR_IO_SPACE_BIT
        }
    }

    fn bar0_write(&mut self, value: u32) {
        if value == 0xFFFF_FFFF {
            self.bar0_sizing = true;
        } else {
            self.bar0_sizing = false;
            self.bar0_base = value & !0x3; // clear the fixed I/O-space bit / reserved bit
        }
    }

    /// The guest-assigned I/O base, or `None` while unassigned or mid-sizing-protocol — the value
    /// `crate::virtio_pci::VirtioPciTransport::set_io_base` needs to start decoding real bus
    /// cycles.
    pub fn io_base(&self) -> Option<u16> {
        if self.bar0_sizing || self.bar0_base == 0 {
            None
        } else {
            Some(self.bar0_base as u16)
        }
    }

    fn config_read_dword(&self, register: u8) -> u32 {
        match register {
            REG_VENDOR_DEVICE => (VIRTIO_VENDOR_ID as u32) | ((self.device_id() as u32) << 16),
            REG_CLASS_REVISION => self.class_code,
            REG_HEADER_BIST => 0, // header type 0 (single-function), no BIST capability
            REG_BAR0 => self.bar0_read(),
            REG_SUBSYSTEM => {
                // Legacy convention: subsystem ID mirrors the virtio device type, subsystem
                // vendor ID mirrors the real virtio vendor ID (virtio spec, Legacy Interface).
                (VIRTIO_VENDOR_ID as u32) | (self.device_kind << 16)
            }
            REG_INTERRUPT => (self.interrupt_line as u32) | (INTERRUPT_PIN_INTA << 8),
            _ => 0,
        }
    }

    fn config_write_dword(&mut self, register: u8, value: u32) {
        match register {
            REG_BAR0 => self.bar0_write(value),
            REG_INTERRUPT => self.interrupt_line = (value & 0xFF) as u8,
            // Vendor/device/class/subsystem are read-only; anything else this header doesn't
            // model (Command/Status, Cache Line Size, other BARs) is silently absorbed.
            _ => {}
        }
    }
}

/// Configuration mechanism #1: a `CONFIG_ADDRESS` latch plus a `CONFIG_DATA` window onto whichever
/// function that latch currently names.
#[derive(Debug, Clone, Copy, Default)]
pub struct PciHostBridge {
    /// The last value written to `CONFIG_ADDRESS` — real hardware also just latches this
    /// unconditionally, decoding it fresh on every `CONFIG_DATA` access rather than at write time.
    config_address: u32,
    /// The virtio-pci legacy function at 00:01.0, `None` until [`Self::attach_virtio_rng`] is
    /// called — every existing constructor leaves this unset, so bus 0's enumeration is unchanged
    /// for any caller that never opts in (device 1 still reads back all-ones, exactly as before
    /// this type existed).
    virtio_rng: Option<PciVirtioFunction>,
    /// The virtio-pci legacy function at 00:02.0, `None` until [`Self::attach_virtio_blk`] is
    /// called — same opt-in convention as [`Self::virtio_rng`], the next slot past it (todo.md §14
    /// item 5(b)).
    virtio_blk: Option<PciVirtioFunction>,
}

impl PciHostBridge {
    pub(crate) fn in_range(port: u16) -> bool {
        (PCI_CONFIG_ADDRESS..PCI_CONFIG_ADDRESS + 4).contains(&port)
            || (PCI_CONFIG_DATA..PCI_CONFIG_DATA + 4).contains(&port)
    }

    /// Installs a virtio-pci legacy entropy-device function at 00:01.0 with a `bar0_size`-byte
    /// I/O BAR — opt-in, mirroring `console.rs::DeviceBus::enable_virtio_rng`'s own convention for
    /// the MMIO transport. `bar0_size` must match `crate::virtio_pci::VIRTIO_PCI_IO_WINDOW_LEN`
    /// (the caller's responsibility — this module has no dependency on `virtio_pci.rs` to check it
    /// itself, keeping config-space bookkeeping and the transport register block independent).
    pub fn attach_virtio_rng(&mut self, bar0_size: u32) {
        self.virtio_rng = Some(PciVirtioFunction::new(
            crate::virtio_mmio::VIRTIO_DEVICE_ID_RNG,
            VIRTIO_UNCLASSIFIED_CODE,
            bar0_size,
            VIRTIO_RNG_DEFAULT_IRQ_LINE,
        ));
    }

    /// Installs a virtio-pci legacy block-device function at 00:02.0 with a `bar0_size`-byte I/O
    /// BAR — opt-in, mirroring [`Self::attach_virtio_rng`] exactly (todo.md §14 item 5(b)).
    /// `bar0_size` must match `crate::virtio_pci::VIRTIO_PCI_IO_WINDOW_LEN`, same caller
    /// responsibility as `attach_virtio_rng`'s.
    pub fn attach_virtio_blk(&mut self, bar0_size: u32) {
        self.virtio_blk = Some(PciVirtioFunction::new(
            crate::virtio_mmio::VIRTIO_DEVICE_ID_BLK,
            VIRTIO_BLK_CLASS_CODE,
            bar0_size,
            VIRTIO_BLK_DEFAULT_IRQ_LINE,
        ));
    }

    /// The virtio-rng function's guest-assigned I/O base, if attached and assigned — `DeviceBus`
    /// reads this after every configuration-space write to keep `VirtioPciTransport::set_io_base`
    /// synchronized with whatever BAR0 the guest's PCI core has settled on.
    pub fn virtio_io_base(&self) -> Option<u16> {
        self.virtio_rng.as_ref().and_then(PciVirtioFunction::io_base)
    }

    /// [`Self::virtio_io_base`]'s counterpart for the virtio-blk function at 00:02.0.
    pub fn virtio_blk_io_base(&self) -> Option<u16> {
        self.virtio_blk.as_ref().and_then(PciVirtioFunction::io_base)
    }

    /// The dword this bridge presents at `addr`, or `0xFFFF_FFFF` for any unmodeled
    /// `bus`/`device`/`function` — the value a real absent device reads back as (PCI Local Bus
    /// spec §6.1).
    fn config_read_dword(&self, addr: ConfigAddress) -> u32 {
        if addr.bus != 0 {
            return 0xFFFF_FFFF;
        }
        if addr.device == 0 && addr.function == 0 {
            return match addr.register {
                REG_VENDOR_DEVICE => (HOST_BRIDGE_VENDOR_ID as u32) | ((HOST_BRIDGE_DEVICE_ID as u32) << 16),
                REG_CLASS_REVISION => HOST_BRIDGE_CLASS_CODE, // revision ID 0 in the low byte
                _ => 0,
            };
        }
        if addr.device == 1 && addr.function == 0 {
            if let Some(virtio) = &self.virtio_rng {
                return virtio.config_read_dword(addr.register);
            }
        }
        if addr.device == 2 && addr.function == 0 {
            if let Some(virtio) = &self.virtio_blk {
                return virtio.config_read_dword(addr.register);
            }
        }
        0xFFFF_FFFF
    }

    /// The write-side counterpart of [`Self::config_read_dword`] — a no-op for any device this
    /// bridge either doesn't model or hasn't attached, matching real hardware's own behavior for a
    /// write to an absent function.
    fn config_write_dword(&mut self, addr: ConfigAddress, value: u32) {
        if addr.bus != 0 || addr.function != 0 {
            return; // the host bridge itself (0/0/0) has no writable register either
        }
        match addr.device {
            1 => {
                if let Some(virtio) = self.virtio_rng.as_mut() {
                    virtio.config_write_dword(addr.register, value);
                }
            }
            2 => {
                if let Some(virtio) = self.virtio_blk.as_mut() {
                    virtio.config_write_dword(addr.register, value);
                }
            }
            _ => {}
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
            let bytes = self.config_read_dword(addr).to_le_bytes();
            copy_window(&bytes, (port - PCI_CONFIG_DATA) as usize, data);
        }
    }

    fn pio_write(&mut self, port: u16, data: &[u8]) {
        debug_assert!(Self::in_range(port));
        if (PCI_CONFIG_ADDRESS..PCI_CONFIG_ADDRESS + 4).contains(&port) {
            let mut bytes = self.config_address.to_le_bytes();
            write_window(&mut bytes, (port - PCI_CONFIG_ADDRESS) as usize, data);
            self.config_address = u32::from_le_bytes(bytes);
        } else {
            // CONFIG_DATA write: read-modify-write the targeted register (narrower-than-dword
            // accesses merge onto the current value, same convention CONFIG_ADDRESS itself uses
            // above), then hand the merged dword to config_write_dword. For the host bridge
            // (00:00.0), which has no writable register, this is a no-op — real hardware returns
            // the same behavior for a write to a read-only register.
            let addr = ConfigAddress::decode(self.config_address & !CONFIG_ENABLE);
            let mut bytes = self.config_read_dword(addr).to_le_bytes();
            write_window(&mut bytes, (port - PCI_CONFIG_DATA) as usize, data);
            self.config_write_dword(addr, u32::from_le_bytes(bytes));
        }
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
        assert_eq!((dword >> 8) & 0xFF, 0x00, "prog IF 00h");
        assert_eq!((dword >> 16) & 0xFF, 0x00, "subclass 00h (host)");
        assert_eq!((dword >> 24) & 0xFF, 0x06, "base class 06h (bridge)");
        assert_eq!(
            (dword >> 16) as u16,
            0x0600,
            "the 16-bit PCI_CLASS_DEVICE word (offset 0x0A, what a real kernel's \
             pci_sanity_check() reads) must equal PCI_CLASS_BRIDGE_HOST exactly"
        );
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

    /// `CONFIG_ADDRESS` selecting bus 0 / device 1 / function 0 — where [`PciHostBridge::
    /// attach_virtio_rng`] places the virtio-pci legacy entropy function.
    fn select_virtio(register: u8) -> u32 {
        CONFIG_ENABLE | (1u32 << 11) | ((register as u32) & 0xFC)
    }

    #[test]
    fn device_1_reads_absent_until_attached() {
        let mut bus = PciHostBridge::default();
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio(REG_VENDOR_DEVICE));
        assert_eq!(read_u32(&mut bus, PCI_CONFIG_DATA), 0xFFFF_FFFF, "no virtio function attached yet");
    }

    #[test]
    fn attached_virtio_function_reports_virtio_vendor_and_legacy_rng_device_id() {
        let mut bus = PciHostBridge::default();
        bus.attach_virtio_rng(0x20);
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio(REG_VENDOR_DEVICE));
        let dword = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!(dword & 0xFFFF, VIRTIO_VENDOR_ID as u32, "virtio's real PCI-SIG vendor ID, not 0x1B36");
        assert_eq!(
            dword >> 16,
            VIRTIO_LEGACY_DEVICE_ID_BASE as u32 + crate::virtio_mmio::VIRTIO_DEVICE_ID_RNG,
            "legacy device ID = 0x1000 + virtio device type"
        );
        assert_ne!(dword & 0xFFFF, 0xFFFF, "a present device must never report vendor ID 0xFFFF");
    }

    #[test]
    fn attached_virtio_rng_class_code_is_unclassified_base_class() {
        let mut bus = PciHostBridge::default();
        bus.attach_virtio_rng(0x20);
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio(REG_CLASS_REVISION));
        let dword = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!((dword >> 8) & 0xFF, 0x00, "prog IF 00h");
        assert_eq!((dword >> 16) & 0xFF, 0x00, "subclass 00h");
        assert_eq!((dword >> 24) & 0xFF, 0xFF, "base class FFh (does not fit any defined class)");
        assert_eq!(
            (dword >> 16) as u16,
            0xFF00,
            "the 16-bit PCI_CLASS_DEVICE word (offset 0x0A) must equal Linux's PCI_CLASS_OTHERS \
             exactly, not the byte-swapped 0x00FF a real pci_sanity_check()-style base/sub-class \
             read would have gotten before this fix"
        );
    }

    #[test]
    fn bar0_sizing_protocol_reports_the_advertised_window_size() {
        let mut bus = PciHostBridge::default();
        bus.attach_virtio_rng(0x20);
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio(REG_BAR0));
        assert_eq!(read_u32(&mut bus, PCI_CONFIG_DATA), BAR_IO_SPACE_BIT, "unassigned BAR0 starts at 0, I/O bit set");

        write_u32(&mut bus, PCI_CONFIG_DATA, 0xFFFF_FFFF); // enter the sizing protocol
        let size_mask = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!(size_mask, !(0x20u32 - 1) | BAR_IO_SPACE_BIT, "size mask for a 0x20-byte I/O BAR");
        assert!(bus.virtio_io_base().is_none(), "still sizing: no valid base yet");

        write_u32(&mut bus, PCI_CONFIG_DATA, 0xC000); // guest assigns a real base
        assert_eq!(read_u32(&mut bus, PCI_CONFIG_DATA), 0xC000 | BAR_IO_SPACE_BIT);
        assert_eq!(bus.virtio_io_base(), Some(0xC000));
    }

    #[test]
    fn interrupt_line_round_trips_and_pin_is_fixed_at_inta() {
        let mut bus = PciHostBridge::default();
        bus.attach_virtio_rng(0x20);
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio(REG_INTERRUPT));
        write_u32(&mut bus, PCI_CONFIG_DATA, 11); // guest assigns IRQ line 11
        let dword = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!(dword & 0xFF, 11, "interrupt line round-trips");
        assert_eq!((dword >> 8) & 0xFF, 1, "interrupt pin is fixed at INTA#");
    }

    #[test]
    fn subsystem_id_mirrors_the_virtio_device_type() {
        let mut bus = PciHostBridge::default();
        bus.attach_virtio_rng(0x20);
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio(REG_SUBSYSTEM));
        let dword = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!(dword & 0xFFFF, VIRTIO_VENDOR_ID as u32, "subsystem vendor ID");
        assert_eq!(dword >> 16, crate::virtio_mmio::VIRTIO_DEVICE_ID_RNG, "subsystem ID = virtio device type");
    }

    #[test]
    fn host_bridge_at_00_0_is_unaffected_by_an_attached_virtio_function() {
        let mut bus = PciHostBridge::default();
        bus.attach_virtio_rng(0x20);
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_host_bridge(REG_VENDOR_DEVICE));
        let dword = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!(dword & 0xFFFF, HOST_BRIDGE_VENDOR_ID as u32, "00:00.0 is still the host bridge");
    }

    /// `CONFIG_ADDRESS` selecting bus 0 / device 2 / function 0 — where
    /// [`PciHostBridge::attach_virtio_blk`] places the virtio-pci legacy block function.
    fn select_virtio_blk(register: u8) -> u32 {
        CONFIG_ENABLE | (2u32 << 11) | ((register as u32) & 0xFC)
    }

    #[test]
    fn device_2_reads_absent_until_blk_is_attached() {
        let mut bus = PciHostBridge::default();
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio_blk(REG_VENDOR_DEVICE));
        assert_eq!(read_u32(&mut bus, PCI_CONFIG_DATA), 0xFFFF_FFFF, "no virtio-blk function attached yet");
    }

    #[test]
    fn attached_virtio_blk_reports_virtio_vendor_legacy_device_id_and_mass_storage_class() {
        let mut bus = PciHostBridge::default();
        bus.attach_virtio_blk(0x20);
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio_blk(REG_VENDOR_DEVICE));
        let dword = read_u32(&mut bus, PCI_CONFIG_DATA);
        assert_eq!(dword & 0xFFFF, VIRTIO_VENDOR_ID as u32);
        assert_eq!(
            dword >> 16,
            VIRTIO_LEGACY_DEVICE_ID_BASE as u32 + crate::virtio_mmio::VIRTIO_DEVICE_ID_BLK,
            "legacy device ID = 0x1000 + virtio device type"
        );

        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio_blk(REG_CLASS_REVISION));
        assert_eq!(read_u32(&mut bus, PCI_CONFIG_DATA), VIRTIO_BLK_CLASS_CODE, "mass storage, not the catch-all class");
    }

    #[test]
    fn virtio_rng_and_virtio_blk_are_attached_independently() {
        let mut bus = PciHostBridge::default();
        bus.attach_virtio_rng(0x20);
        // virtio-blk is untouched: still absent even with rng attached.
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio_blk(REG_VENDOR_DEVICE));
        assert_eq!(read_u32(&mut bus, PCI_CONFIG_DATA), 0xFFFF_FFFF);
        assert_eq!(bus.virtio_blk_io_base(), None);

        bus.attach_virtio_blk(0x20);
        write_u32(&mut bus, PCI_CONFIG_ADDRESS, select_virtio_blk(REG_BAR0));
        write_u32(&mut bus, PCI_CONFIG_DATA, 0xD000);
        assert_eq!(bus.virtio_blk_io_base(), Some(0xD000));
        assert_eq!(bus.virtio_io_base(), None, "rng's BAR0 must be unaffected by blk's own assignment");
    }
}
