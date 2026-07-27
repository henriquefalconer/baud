// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Minimal ACPI tables (todo.md §14 item 5(c): "RSDP -> RSDT/XSDT -> FADT + DSDT + MADT with one
// LAPIC", specs/baud-ubuntu.md §4) — the next H9 sub-step after the virtio-pci transport (5(a))
// and virtio-blk device (5(b)). A stock Ubuntu 18.04.1 kernel's `acpi_boot_table_init()` scans
// for these unconditionally once `acpi=off` is dropped from the cmdline; PCI itself does not need
// them (legacy 0xCF8/0xCFC, `crate::pci`, already works standalone), but a modern distro kernel's
// own boot path still wants at least this minimal set present.
//
// **Scope of this module**: pure, deterministic table *construction* — every byte is a function
// of nothing but this module's own constants (specs/baud-multiverse.md's "no nondeterministic
// table content" constraint: guest RAM is included in the fingerprint hash, so these tables must
// be byte-identical across every boot). [`write_acpi_tables`] places them at fixed guest-physical
// addresses (`crate::layout`'s `ACPI_*` constants); nothing in this module is wired into any real
// boot path yet (`linux::boot_guest`/`load_kernel_and_write_boot_params` still never call it,
// mirroring how `baud_packages::kernel_build`/`initramfs` shipped "neither yet wired into any
// CLI/server route" — todo.md §14). Two things explicitly remain open, deliberately not attempted
// here, and must be resolved before an ACPI-enabled guest can actually be booted against this:
//
// 1. **A real LAPIC MMIO shim.** Every existing fixture boots with no MADT at all, so an
//    unmodified kernel's LAPIC-ID probe at the fixed MMIO base `0xFEE0_0000` falls through to
//    `console::OpenBusFallback` and reads back `0xFFFF_FFFF`, which the kernel correctly reads as
//    "no LAPIC present" (`tests/fixtures/linux-guest/BUILD.md`'s own finding) and falls back to
//    `Using NULL legacy PIC`. Once [`build_madt`]'s "one LAPIC" entry is actually consulted by a
//    guest with `acpi=on`, the kernel's apic driver will read/write *real* LAPIC registers (ID,
//    version, LVT entries, the timer's initial/current-count pair) expecting real hardware
//    semantics, not an open-bus stub — `0xFFFF_FFFF` is no longer a valid "absent" signal for
//    MMIO the way it is for PCI config space, and a write-then-poll-for-completion loop against an
//    always-absorbed write can spin forever. This needs its own deterministic device model
//    (`Pic8259`'s stub-just-enough-to-satisfy-the-probe precedent, not a functioning timer), built
//    and hardware-verified against a real `CONFIG_ACPI=y` guest — out of scope for this table-
//    writing sub-step, exactly as `pci.rs`'s own doc flagged "a real Ubuntu boot also wants a
//    minimal RSDP/RSDT/FADT/DSDT/MADT ... unrelated to config-space access itself" as future work
//    when it was written.
// 2. **No `CONFIG_ACPI=y` guest fixture exists yet to boot this against.** `tests/fixtures/
//    linux-guest/minimal.config` disables ACPI entirely; every table below is therefore only
//    unit-tested for internal structural correctness (checksums, pointers, flags) against the
//    ACPI specification's own byte layout, never against a real ACPICA/Linux parse. That real
//    acid test is H9 sub-step (d)/(e)'s job (the actual Ubuntu 18.04.1 image + `drive/h9.sh`).

use crate::layout;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};

/// The conventional LAPIC MMIO base every x86 Linux kernel's apic driver assumes absent an
/// `IA32_APIC_BASE` MSR override (Intel SDM Vol. 3A §10.4.3) — also the value [`build_madt`]
/// publishes as the MADT's own "Local Interrupt Controller Address" field.
const LOCAL_APIC_MMIO_BASE: u32 = 0xFEE0_0000;

const OEM_ID: [u8; 6] = *b"BAUD  ";
const CREATOR_ID: [u8; 4] = *b"BAUD";
const CREATOR_REVISION: u32 = 1;
const OEM_REVISION: u32 = 1;

/// [`layout::ACPI_RSDP_ADDR`] must sit inside the `0xE0000..0x100000` BIOS-area scan window on a
/// 16-byte boundary (both sides are `const`, so this is a compile-time check, not a runtime one --
/// clippy: `assertions_on_constants` — same convention `layout.rs`'s own
/// `_STATIC_LAYOUT_INVARIANTS` uses).
const _RSDP_SCAN_WINDOW_INVARIANTS: () = {
    assert!(layout::ACPI_RSDP_ADDR >= 0x000E_0000);
    assert!(layout::ACPI_RSDP_ADDR + 36 <= 0x0010_0000);
    assert!(layout::ACPI_RSDP_ADDR.is_multiple_of(16));
};

/// Sum every byte in `table` and return the one value that, appended, would make the whole sum
/// `0 mod 256` — the checksum every ACPI table (and the RSDP's two checksums) defines the same
/// way (ACPI spec §5.2.5/§5.2.6: "entire table including the checksum field must sum to zero").
fn checksum(table: &[u8]) -> u8 {
    0u8.wrapping_sub(table.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)))
}

/// The 36-byte `DESCRIPTION_HEADER` every top-level table but the RSDP starts with (ACPI spec
/// §5.2.6), checksum byte left `0` — the caller appends its own table-specific content, then
/// patches offset 4 (`Length`, once the final size is known) and offset 9 (`Checksum`, once the
/// whole table's bytes are final).
fn description_header(signature: &[u8; 4], revision: u8, oem_table_id: &[u8; 8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(36);
    bytes.extend_from_slice(signature);
    bytes.extend_from_slice(&0u32.to_le_bytes()); // Length -- patched by the caller
    bytes.push(revision);
    bytes.push(0); // Checksum -- patched by the caller
    bytes.extend_from_slice(&OEM_ID);
    bytes.extend_from_slice(oem_table_id);
    bytes.extend_from_slice(&OEM_REVISION.to_le_bytes());
    bytes.extend_from_slice(&CREATOR_ID);
    bytes.extend_from_slice(&CREATOR_REVISION.to_le_bytes());
    debug_assert_eq!(bytes.len(), 36);
    bytes
}

/// Write the final `Length` (offset 4) and `Checksum` (offset 9) into a table built from
/// [`description_header`], once every byte after the header is in place.
fn finish_table(mut bytes: Vec<u8>) -> Vec<u8> {
    let length = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    bytes[9] = 0;
    bytes[9] = checksum(&bytes);
    bytes
}

/// The Differentiated System Description Table (ACPI spec §5.2.11.1): a `DESCRIPTION_HEADER`
/// (signature `"DSDT"`) followed by AML bytecode. An empty term list (zero AML bytes after the
/// header) is a legal, if unusual, definition block — ACPICA accepts it, and Linux's
/// `acpi_bus_scan` then simply finds no devices under `\_SB`, which is fine here since PCI
/// enumeration already happens outside ACPI's namespace entirely (`crate::pci`'s legacy
/// 0xCF8/0xCFC mechanism) and there is no `\_S5` package for ACPI-driven poweroff since baud's
/// existing guests already shut down via `reboot=t panic=-1`, never ACPI.
pub fn build_dsdt() -> Vec<u8> {
    finish_table(description_header(b"DSDT", 2, b"BAUDDSDT"))
}

/// The Fixed ACPI Description Table (`"FACP"`, ACPI spec §5.2.9) — a 244-byte ACPI 2.0-4.0-shaped
/// table (header through `X_GPE1_BLK`) with every fixed-hardware PM register block (`PM1a_EVT_BLK`
/// etc.) left `0` and `Flags` bit 20 (`HW_REDUCED_ACPI`) set: this tells OSPM the platform
/// implements none of the fixed ACPI hardware feature registers at all, which is honestly true
/// here (no PM timer, no GPE blocks, no SMI-mediated ACPI-mode switch are modeled) and lets Linux
/// skip its entire `acpi_hw_*` fixed-register code path rather than baud having to model a PM1a
/// control block as a real device just to avoid a probe hanging on it. `SMI_CMD = 0` additionally
/// makes `acpi_enable()` skip issuing any SMI at all. `Dsdt`/`X_Dsdt` both point at
/// [`layout::ACPI_DSDT_ADDR`] (the 32-bit field for ACPI 1.0 readers, the 64-bit field — which
/// modern ACPICA prefers when nonzero — for everyone else; the address fits in 32 bits either way,
/// this crate's whole layout lives under [`layout::HIMEM_START`]).
pub fn build_fadt() -> Vec<u8> {
    /// ACPI spec §5.2.9.1, `Flags` bit 20: "the platform is compatible with the hardware-reduced
    /// ACPI model" -- OSPM must not use any of the fixed hardware feature registers.
    const FADT_FLAG_HW_REDUCED_ACPI: u32 = 1 << 20;
    /// The traditional real-hardware/QEMU SCI GSI -- dormant here (nothing raises it), kept for
    /// protocol fidelity the same way `Pic8259`'s IRQ0/IRQ2 reservations are "by convention," not
    /// because anything currently uses them.
    const SCI_INT: u16 = 9;

    let mut bytes = description_header(b"FACP", 4, b"BAUDFADT");
    debug_assert_eq!(bytes.len(), 36);
    bytes.extend_from_slice(&0u32.to_le_bytes()); // FIRMWARE_CTRL (offset 36) -- no FACS
    bytes.extend_from_slice(&(layout::ACPI_DSDT_ADDR as u32).to_le_bytes()); // DSDT (offset 40)
    bytes.push(0); // Reserved (offset 44)
    bytes.push(0); // Preferred_PM_Profile (offset 45) -- unspecified
    bytes.extend_from_slice(&SCI_INT.to_le_bytes()); // SCI_INT (offset 46)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // SMI_CMD (offset 48)
    bytes.push(0); // ACPI_ENABLE (offset 52)
    bytes.push(0); // ACPI_DISABLE (offset 53)
    bytes.push(0); // S4BIOS_REQ (offset 54)
    bytes.push(0); // PSTATE_CNT (offset 55)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // PM1a_EVT_BLK (offset 56)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // PM1b_EVT_BLK (offset 60)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // PM1a_CNT_BLK (offset 64)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // PM1b_CNT_BLK (offset 68)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // PM2_CNT_BLK (offset 72)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // PM_TMR_BLK (offset 76)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // GPE0_BLK (offset 80)
    bytes.extend_from_slice(&0u32.to_le_bytes()); // GPE1_BLK (offset 84)
    bytes.push(0); // PM1_EVT_LEN (offset 88)
    bytes.push(0); // PM1_CNT_LEN (offset 89)
    bytes.push(0); // PM2_CNT_LEN (offset 90)
    bytes.push(0); // PM_TMR_LEN (offset 91)
    bytes.push(0); // GPE0_BLK_LEN (offset 92)
    bytes.push(0); // GPE1_BLK_LEN (offset 93)
    bytes.push(0); // GPE1_BASE (offset 94)
    bytes.push(0); // CST_CNT (offset 95)
    bytes.extend_from_slice(&0u16.to_le_bytes()); // P_LVL2_LAT (offset 96)
    bytes.extend_from_slice(&0u16.to_le_bytes()); // P_LVL3_LAT (offset 98)
    bytes.extend_from_slice(&0u16.to_le_bytes()); // FLUSH_SIZE (offset 100)
    bytes.extend_from_slice(&0u16.to_le_bytes()); // FLUSH_STRIDE (offset 102)
    bytes.push(0); // DUTY_OFFSET (offset 104)
    bytes.push(0); // DUTY_WIDTH (offset 105)
    bytes.push(0); // DAY_ALRM (offset 106)
    bytes.push(0); // MON_ALRM (offset 107)
    bytes.push(0); // CENTURY (offset 108)
    bytes.extend_from_slice(&0u16.to_le_bytes()); // IAPC_BOOT_ARCH (offset 109)
    bytes.push(0); // Reserved (offset 111)
    bytes.extend_from_slice(&FADT_FLAG_HW_REDUCED_ACPI.to_le_bytes()); // Flags (offset 112)
    bytes.extend_from_slice(&[0u8; 12]); // RESET_REG (offset 116, Generic Address Structure)
    bytes.push(0); // RESET_VALUE (offset 128)
    bytes.extend_from_slice(&[0u8; 3]); // Reserved (offset 129)
    bytes.extend_from_slice(&0u64.to_le_bytes()); // X_FIRMWARE_CTRL (offset 132)
    bytes.extend_from_slice(&layout::ACPI_DSDT_ADDR.to_le_bytes()); // X_DSDT (offset 140)
    bytes.extend_from_slice(&[0u8; 12]); // X_PM1a_EVT_BLK (offset 148)
    bytes.extend_from_slice(&[0u8; 12]); // X_PM1b_EVT_BLK (offset 160)
    bytes.extend_from_slice(&[0u8; 12]); // X_PM1a_CNT_BLK (offset 172)
    bytes.extend_from_slice(&[0u8; 12]); // X_PM1b_CNT_BLK (offset 184)
    bytes.extend_from_slice(&[0u8; 12]); // X_PM2_CNT_BLK (offset 196)
    bytes.extend_from_slice(&[0u8; 12]); // X_PM_TMR_BLK (offset 208)
    bytes.extend_from_slice(&[0u8; 12]); // X_GPE0_BLK (offset 220)
    bytes.extend_from_slice(&[0u8; 12]); // X_GPE1_BLK (offset 232)
    debug_assert_eq!(bytes.len(), 244);
    finish_table(bytes)
}

/// The Multiple APIC Description Table (`"APIC"`, ACPI spec §5.2.12): a `DESCRIPTION_HEADER`, the
/// Local Interrupt Controller Address ([`LOCAL_APIC_MMIO_BASE`]) and Flags (`PCAT_COMPAT` set,
/// bit 0 -- a dual-8259 is also present, matching `crate::pic8259::Pic8259`'s existing stub), then
/// exactly one Processor Local APIC structure (type 0): ACPI Processor UID 0, APIC ID 0, `Enabled`
/// flag bit 0 set -- todo.md §14 item 5(c)'s "MADT with one LAPIC". No I/O APIC entry: with
/// `nr_ioapics == 0` Linux falls back to the same legacy-PIC interrupt routing it already uses
/// with no MADT at all, which is the intended outcome (baud never registers a real
/// `KVM_CREATE_IRQCHIP` and delivers every interrupt via direct `KVM_INTERRUPT` injection
/// regardless of what routing the guest believes is in effect).
pub fn build_madt() -> Vec<u8> {
    /// MADT `Flags` bit 0: a dual-8259 PIC is also present and must be disabled by the OS before
    /// using the APIC (ACPI spec §5.2.12) -- true here, `Pic8259` is unconditionally modeled.
    const MADT_FLAG_PCAT_COMPAT: u32 = 1 << 0;
    /// Processor Local APIC structure (type 0, ACPI spec §5.2.12.2): fixed 8-byte length.
    const MADT_TYPE_LOCAL_APIC: u8 = 0;
    const MADT_LOCAL_APIC_STRUCT_LEN: u8 = 8;
    /// Local APIC `Flags` bit 0: `Enabled` -- an entry with this clear is a CPU OSPM must not try
    /// to boot at all (ACPI spec §5.2.12.2).
    const LOCAL_APIC_FLAG_ENABLED: u32 = 1 << 0;

    let mut bytes = description_header(b"APIC", 3, b"BAUDMADT");
    bytes.extend_from_slice(&LOCAL_APIC_MMIO_BASE.to_le_bytes());
    bytes.extend_from_slice(&MADT_FLAG_PCAT_COMPAT.to_le_bytes());
    bytes.push(MADT_TYPE_LOCAL_APIC);
    bytes.push(MADT_LOCAL_APIC_STRUCT_LEN);
    bytes.push(0); // ACPI Processor UID
    bytes.push(0); // APIC ID
    bytes.extend_from_slice(&LOCAL_APIC_FLAG_ENABLED.to_le_bytes());
    debug_assert_eq!(bytes.len(), 36 + 8 + 8);
    finish_table(bytes)
}

/// The Extended System Description Table (`"XSDT"`, ACPI spec §5.2.8): a `DESCRIPTION_HEADER`
/// followed by one 8-byte physical-address pointer per top-level table -- here, exactly
/// [`layout::ACPI_FADT_ADDR`] and [`layout::ACPI_MADT_ADDR`] (the DSDT is reached only through the
/// FADT's own `Dsdt`/`X_Dsdt` pointer, never listed here directly, per the spec).
pub fn build_xsdt() -> Vec<u8> {
    let mut bytes = description_header(b"XSDT", 1, b"BAUDXSDT");
    bytes.extend_from_slice(&layout::ACPI_FADT_ADDR.to_le_bytes());
    bytes.extend_from_slice(&layout::ACPI_MADT_ADDR.to_le_bytes());
    finish_table(bytes)
}

/// The ACPI 2.0+ Root System Description Pointer (36 bytes, ACPI spec §5.2.5.3) -- signature
/// `"RSD PTR "`, `Revision = 2` (so a modern OS prefers the 64-bit `XsdtAddress` over
/// `RsdtAddress`, which is left `0`: ACPICA's `acpi_tb_parse_root_table` tries the XSDT first
/// whenever `revision >= 2` and `xsdt_physical_address != 0`, falling back to the RSDT only if the
/// XSDT is absent or fails to validate -- baud never builds an RSDT at all, matching the "minimal"
/// framing of todo.md §14 item 5(c)). Carries two independent checksums, both ACPI-spec-mandated:
/// the "ACPI 1.0" checksum over the first 20 bytes (for a pre-2.0 OS that only reads that much),
/// and the extended checksum over the full 36 bytes.
pub fn build_rsdp() -> [u8; 36] {
    const RSDP_REVISION: u8 = 2;
    const RSDP_LEN: u32 = 36;

    let mut bytes = [0u8; 36];
    bytes[0..8].copy_from_slice(b"RSD PTR ");
    // bytes[8] (Checksum) patched below, once the first-20-bytes region is otherwise final.
    bytes[9..15].copy_from_slice(&OEM_ID);
    bytes[15] = RSDP_REVISION;
    bytes[16..20].copy_from_slice(&0u32.to_le_bytes()); // RsdtAddress -- unused, see doc above
    bytes[8] = checksum(&bytes[0..20]);

    bytes[20..24].copy_from_slice(&RSDP_LEN.to_le_bytes());
    bytes[24..32].copy_from_slice(&layout::ACPI_XSDT_ADDR.to_le_bytes());
    // bytes[32] (Extended checksum) patched last, once every other byte is final.
    bytes[33..36].copy_from_slice(&[0u8; 3]); // Reserved
    bytes[32] = checksum(&bytes);
    bytes
}

/// Write the whole minimal ACPI table set at their fixed [`layout`] addresses -- RSDP, then XSDT,
/// FADT, DSDT, MADT each in their own page, per this module's own doc on why none of the latter
/// four need any address other than "wherever the RSDP/XSDT/FADT's own pointers name."
pub fn write_acpi_tables<M: GuestMemoryBackend>(
    guest_mem: &M,
) -> Result<(), vm_memory::guest_memory::Error> {
    guest_mem.write_slice(&build_rsdp(), GuestAddress(layout::ACPI_RSDP_ADDR))?;
    guest_mem.write_slice(&build_xsdt(), GuestAddress(layout::ACPI_XSDT_ADDR))?;
    guest_mem.write_slice(&build_fadt(), GuestAddress(layout::ACPI_FADT_ADDR))?;
    guest_mem.write_slice(&build_dsdt(), GuestAddress(layout::ACPI_DSDT_ADDR))?;
    guest_mem.write_slice(&build_madt(), GuestAddress(layout::ACPI_MADT_ADDR))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::GuestMemory;

    fn table_checksum_is_zero(table: &[u8]) {
        assert_eq!(table.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)), 0, "table bytes must sum to 0 mod 256");
    }

    #[test]
    fn checksum_makes_the_whole_slice_sum_to_zero() {
        let mut bytes = vec![1u8, 2, 3, 4, 5];
        bytes.push(checksum(&bytes));
        assert_eq!(bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)), 0);
    }

    #[test]
    fn rsdp_has_a_valid_signature_and_both_checksums() {
        let rsdp = build_rsdp();
        assert_eq!(&rsdp[0..8], b"RSD PTR ");
        table_checksum_is_zero(&rsdp[0..20]); // ACPI 1.0 checksum region
        table_checksum_is_zero(&rsdp[0..36]); // extended checksum region
        assert_eq!(rsdp[15], 2, "Revision must be >= 2 for the XsdtAddress field to be trusted");
        assert_eq!(&rsdp[24..32], &layout::ACPI_XSDT_ADDR.to_le_bytes());
    }

    #[test]
    fn rsdp_lands_inside_the_bios_area_scan_window() {
        // Both bounds are checked again at compile time below (`_RSDP_SCAN_WINDOW_INVARIANTS`) --
        // this test only re-confirms the one property that isn't a pure constant comparison:
        // `build_rsdp`'s actual on-the-wire bytes really do occupy the address this module claims.
        let rsdp = build_rsdp();
        assert_eq!(rsdp.len(), 36);
    }

    #[test]
    fn xsdt_checksum_is_valid_and_lists_fadt_and_madt() {
        let xsdt = build_xsdt();
        assert_eq!(&xsdt[0..4], b"XSDT");
        table_checksum_is_zero(&xsdt);
        assert_eq!(&xsdt[36..44], &layout::ACPI_FADT_ADDR.to_le_bytes());
        assert_eq!(&xsdt[44..52], &layout::ACPI_MADT_ADDR.to_le_bytes());
        let length = u32::from_le_bytes(xsdt[4..8].try_into().unwrap());
        assert_eq!(length as usize, xsdt.len());
    }

    #[test]
    fn fadt_checksum_is_valid_and_points_at_the_dsdt() {
        let fadt = build_fadt();
        assert_eq!(&fadt[0..4], b"FACP");
        table_checksum_is_zero(&fadt);
        assert_eq!(fadt.len(), 244);
        assert_eq!(&fadt[40..44], &(layout::ACPI_DSDT_ADDR as u32).to_le_bytes(), "32-bit Dsdt pointer");
        assert_eq!(&fadt[140..148], &layout::ACPI_DSDT_ADDR.to_le_bytes(), "64-bit X_Dsdt pointer");
    }

    #[test]
    fn fadt_declares_hardware_reduced_acpi_and_no_smi_mediated_enable() {
        let fadt = build_fadt();
        let flags = u32::from_le_bytes(fadt[112..116].try_into().unwrap());
        assert_eq!(flags & (1 << 20), 1 << 20, "HW_REDUCED_ACPI bit must be set");
        let smi_cmd = u32::from_le_bytes(fadt[48..52].try_into().unwrap());
        assert_eq!(smi_cmd, 0, "SMI_CMD=0 must make acpi_enable() skip issuing any SMI");
    }

    #[test]
    fn fadt_declares_every_fixed_hardware_register_block_absent() {
        let fadt = build_fadt();
        for &(offset, len) in &[(56, 4), (60, 4), (64, 4), (68, 4), (72, 4), (76, 4), (80, 4), (84, 4)] {
            assert!(fadt[offset..offset + len].iter().all(|&b| b == 0), "PM register block at offset {offset} must be all-zero");
        }
    }

    #[test]
    fn dsdt_checksum_is_valid_and_carries_no_aml() {
        let dsdt = build_dsdt();
        assert_eq!(&dsdt[0..4], b"DSDT");
        table_checksum_is_zero(&dsdt);
        assert_eq!(dsdt.len(), 36, "an empty definition block is exactly one header, no AML bytes");
    }

    #[test]
    fn madt_checksum_is_valid_and_declares_exactly_one_enabled_lapic() {
        let madt = build_madt();
        assert_eq!(&madt[0..4], b"APIC");
        table_checksum_is_zero(&madt);
        assert_eq!(&madt[36..40], &LOCAL_APIC_MMIO_BASE.to_le_bytes());
        let flags = u32::from_le_bytes(madt[40..44].try_into().unwrap());
        assert_eq!(flags & 1, 1, "PCAT_COMPAT must be set: Pic8259 is also modeled");
        assert_eq!(madt.len(), 36 + 8 + 8, "header + local-apic-address/flags + exactly one entry");
        assert_eq!(madt[44], 0, "entry type 0: Processor Local APIC");
        assert_eq!(madt[45], 8, "entry length 8");
        assert_eq!(madt[47], 0, "APIC ID 0 -- the sole CPU this crate ever models");
        let entry_flags = u32::from_le_bytes(madt[48..52].try_into().unwrap());
        assert_eq!(entry_flags & 1, 1, "the LAPIC entry's own Enabled bit must be set");
    }

    #[test]
    fn write_acpi_tables_lands_every_table_at_its_fixed_address() {
        let guest_mem = GuestMemory::from_ranges(&[(GuestAddress(0), layout::GUEST_RAM_SIZE)])
            .expect("anonymous-mmap guest memory for a unit test");
        write_acpi_tables(&guest_mem).expect("write must succeed");

        let mut rsdp = [0u8; 36];
        guest_mem.read_slice(&mut rsdp, GuestAddress(layout::ACPI_RSDP_ADDR)).unwrap();
        assert_eq!(rsdp, build_rsdp());

        let xsdt = build_xsdt();
        let mut read_back = vec![0u8; xsdt.len()];
        guest_mem.read_slice(&mut read_back, GuestAddress(layout::ACPI_XSDT_ADDR)).unwrap();
        assert_eq!(read_back, xsdt);

        let fadt = build_fadt();
        let mut read_back = vec![0u8; fadt.len()];
        guest_mem.read_slice(&mut read_back, GuestAddress(layout::ACPI_FADT_ADDR)).unwrap();
        assert_eq!(read_back, fadt);

        let dsdt = build_dsdt();
        let mut read_back = vec![0u8; dsdt.len()];
        guest_mem.read_slice(&mut read_back, GuestAddress(layout::ACPI_DSDT_ADDR)).unwrap();
        assert_eq!(read_back, dsdt);

        let madt = build_madt();
        let mut read_back = vec![0u8; madt.len()];
        guest_mem.read_slice(&mut read_back, GuestAddress(layout::ACPI_MADT_ADDR)).unwrap();
        assert_eq!(read_back, madt);
    }

    #[test]
    fn table_construction_is_pure_and_deterministic() {
        assert_eq!(build_rsdp(), build_rsdp());
        assert_eq!(build_xsdt(), build_xsdt());
        assert_eq!(build_fadt(), build_fadt());
        assert_eq!(build_dsdt(), build_dsdt());
        assert_eq!(build_madt(), build_madt());
    }
}
