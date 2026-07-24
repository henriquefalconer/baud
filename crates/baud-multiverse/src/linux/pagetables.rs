// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Writes the pure `layout::build_identity_page_tables` output into real guest RAM, and builds the
// matching `kvm_sregs` (long mode: `CR0.PG|PE`, `CR4.PAE`, `EFER.LME|LMA`, flat 64-bit code/data
// segments) — the standard direct-boot technique every minimal rust-vmm reference VMM
// (Firecracker, cloud-hypervisor) uses to skip the kernel's own 16-/32-bit real-mode setup code
// entirely and jump straight to the Linux/x86 64-bit kernel entry point (specs/baud-multiverse.md
// §3.6's subtractive rule: no real BIOS, no real-mode trampoline — down to a console plus the tape
// device).

use crate::layout::{self, IdentityPageTables};
use kvm_bindings::{kvm_dtable, kvm_segment, kvm_sregs};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};

/// Reinterpret a `[u64; 512]` page-table page as its 4096 little-endian bytes. Safe: a plain
/// array of `u64` has no padding and both host and guest are x86_64 (native-endian = the page
/// table's on-the-wire byte order Intel's page-walker reads).
fn page_as_bytes(page: &[u64; 512]) -> &[u8] {
    // SAFETY: `[u64; 512]` is `Copy`, has no padding, and its alignment (8) only relaxes the
    // byte-slice's required alignment (1); reinterpreting it as `&[u8]` of the same byte length
    // never reads past the array or produces an unaligned/uninitialized read.
    unsafe { std::slice::from_raw_parts(page.as_ptr() as *const u8, std::mem::size_of_val(page)) }
}

/// Write the fixed identity map for `ram_size` bytes of guest RAM into guest memory at
/// [`layout::PML4_ADDR`]/[`layout::PDPTE_ADDR`]/[`layout::PDE_ADDR`] (contiguous PDE pages).
pub fn write_identity_page_tables<M: GuestMemoryBackend>(
    guest_mem: &M,
    ram_size: usize,
) -> Result<(), vm_memory::guest_memory::Error> {
    let tables: IdentityPageTables = layout::build_identity_page_tables(ram_size);

    guest_mem.write_slice(page_as_bytes(&tables.pml4), GuestAddress(layout::PML4_ADDR))?;
    guest_mem.write_slice(page_as_bytes(&tables.pdpte), GuestAddress(layout::PDPTE_ADDR))?;
    for (i, pde_page) in tables.pde_pages.iter().enumerate() {
        let addr = GuestAddress(layout::PDE_ADDR + (i as u64) * 0x1000);
        guest_mem.write_slice(page_as_bytes(pde_page), addr)?;
    }
    Ok(())
}

/// x86_64 `CR0`/`CR4`/`EFER` bits this boot flow sets, named (not left as bare hex in the
/// assignment below) so the "why long mode is on" is legible at the call site.
const CR0_PE: u64 = 1 << 0; // Protection Enable
const CR0_MP: u64 = 1 << 1; // Monitor Coprocessor
const CR0_ET: u64 = 1 << 4; // Extension Type (always 1 on modern CPUs)
const CR0_NE: u64 = 1 << 5; // Numeric Error (x87 exception reporting)
const CR0_WP: u64 = 1 << 16; // Write Protect (CPL0 respects read-only pages too)
const CR0_AM: u64 = 1 << 18; // Alignment Mask
const CR0_PG: u64 = 1 << 31; // Paging Enable
const CR4_PAE: u64 = 1 << 5; // Physical Address Extension (required for long mode)
const EFER_LME: u64 = 1 << 8; // Long Mode Enable
const EFER_LMA: u64 = 1 << 10; // Long Mode Active

/// Populate `kvm_sregs` for entry directly into 64-bit long mode: paging on with the identity map
/// built by [`write_identity_page_tables`], a flat 64-bit code segment (selector `0x08`) and a
/// flat data segment (selector `0x10`) covering all of guest-virtual address space, no local
/// descriptor table, and `GDT`/`IDT` pointed at guest-physical `0` with limit `0` (KVM's shadow
/// segment-descriptor cache is set directly through this struct; the guest never executes `LGDT`
/// on this boot path, so no in-memory GDT is ever built or read).
pub fn long_mode_sregs() -> kvm_sregs {
    let code_segment = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x08,
        type_: 0xB, // execute, read, accessed
        present: 1,
        dpl: 0,
        db: 0,  // 64-bit code segments must have D=0
        s: 1,   // code/data segment (not a system segment)
        l: 1,   // 64-bit long mode segment
        g: 1,   // limit is in 4 KiB pages
        avl: 0,
        unusable: 0,
        padding: 0,
    };
    let data_segment = kvm_segment {
        type_: 0x3, // read, write, accessed
        db: 1,      // 32-bit-style operand size for data segments in long mode (l=0 here)
        l: 0,
        selector: 0x10,
        ..code_segment
    };

    let mut sregs = kvm_sregs {
        cs: code_segment,
        ds: data_segment,
        es: data_segment,
        fs: data_segment,
        gs: data_segment,
        ss: data_segment,
        gdt: kvm_dtable { base: 0, limit: 0, padding: [0; 3] },
        idt: kvm_dtable { base: 0, limit: 0, padding: [0; 3] },
        cr0: CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_WP | CR0_AM | CR0_PG,
        cr3: layout::PML4_ADDR,
        cr4: CR4_PAE,
        efer: EFER_LME | EFER_LMA,
        ..Default::default()
    };
    // `tr`/`ldt` stay unusable (default-zeroed `kvm_segment`, `unusable` left `0`... KVM treats an
    // all-zero segment with `present = 0` as not present, which is correct here: this boot flow
    // never uses a task register or an LDT).
    sregs.tr.selector = 0;
    sregs.ldt.selector = 0;
    sregs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_as_bytes_round_trips_a_known_pattern() {
        let mut page = [0u64; 512];
        page[0] = 0x0000_9000 | 0x3; // present | writable
        page[511] = u64::MAX;
        let bytes = page_as_bytes(&page);
        assert_eq!(bytes.len(), 4096);
        assert_eq!(&bytes[0..8], &(0x0000_9003u64).to_le_bytes());
        assert_eq!(&bytes[4088..4096], &u64::MAX.to_le_bytes());
    }

    #[test]
    fn long_mode_sregs_enables_paging_pae_and_long_mode() {
        let sregs = long_mode_sregs();
        assert_eq!(sregs.cr0 & CR0_PG, CR0_PG, "paging must be on");
        assert_eq!(sregs.cr0 & CR0_PE, CR0_PE, "protected mode must be on");
        assert_eq!(sregs.cr4 & CR4_PAE, CR4_PAE, "PAE is required for long mode");
        assert_eq!(sregs.efer & EFER_LME, EFER_LME);
        assert_eq!(sregs.efer & EFER_LMA, EFER_LMA);
        assert_eq!(sregs.cr3, layout::PML4_ADDR, "CR3 must point at the identity map's PML4");
        assert_eq!(sregs.cs.l, 1, "code segment must be a 64-bit long-mode segment");
        assert_eq!(sregs.ds.l, 0, "data segments carry l=0 (Intel SDM 3A table 3-5)");
    }
}
