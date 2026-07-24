// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Fixed guest-physical memory layout for the boot flow (specs/baud-multiverse.md §2's
// `Kvm::new → create_vm → register one zeroed guest-RAM region at a fixed guest-physical address
// → create_vcpu → ... → load the guest kernel ... and write boot params at fixed addresses`).
//
// Every address here is a compile-time constant, not derived from anything host- or run-specific
// — "Memory init: Zeroed RAM at fixed guest-physical addresses" is its own row in specs/
// baud-multiverse.md §3's nondeterminism table. The identity-map page-table builder is pure
// (no KVM/host memory touched), so its byte-for-byte output is unit-tested here; `linux/mod.rs`
// writes that exact output into guest RAM before `KVM_SET_SREGS` enables paging.

/// Where guest RAM starts (guest-physical address 0) and its fixed size for the boot-flow guests
/// this milestone targets (H1's "hello" image, specs/baud-multiverse.md §8's `double_boot_memory_
/// identical`). A real fleet host may size this per-workload later; the boot-flow constants below
/// only need to fit comfortably inside it.
pub const GUEST_RAM_START: u64 = 0x0;
pub const GUEST_RAM_SIZE: usize = 256 * 1024 * 1024; // 256 MiB

/// High-memory start: nothing baud loads may sit below the first megabyte (BIOS/legacy-reserved
/// range in every x86 boot convention linux-loader follows). Passed as `highmem_start_address` to
/// `KernelLoader::load`.
pub const HIMEM_START: u64 = 0x0010_0000;

/// Where the compressed kernel image itself is loaded — comfortably above `HIMEM_START` and below
/// where a small guest's own working set would grow, per the same fixed-address convention every
/// rust-vmm reference VMM (Firecracker/cloud-hypervisor) uses for direct kernel boot.
pub const KERNEL_LOAD_ADDR: u64 = 0x0020_0000;

/// The Linux/x86 64-bit boot protocol's kernel entry point is exactly 0x200 bytes past the start
/// of the protected-mode kernel image (Documentation/x86/boot.txt, "the 64-bit boot protocol"):
/// the VMM must set `RIP` there directly, skipping the 16-/32-bit real-mode setup code entirely,
/// which is why baud (like other minimal direct-boot VMMs) never emulates real mode at all.
pub const KERNEL_64BIT_ENTRY_OFFSET: u64 = 0x200;

/// The "zero page" (`struct boot_params`) address — low memory, below the kernel and any page
/// tables, per convention shared with Firecracker's `arch::x86_64::layout::ZERO_PAGE_START`.
pub const ZERO_PAGE_ADDR: u64 = 0x0000_7000;

/// Kernel command line: also low memory, sized generously for the tape-device driver's boot
/// arguments (specs/baud-tape-device.md's guest-side driver contract).
pub const CMDLINE_ADDR: u64 = 0x0002_0000;
pub const CMDLINE_MAX_SIZE: usize = 0x1_0000;

/// The three fixed page-table pages built fresh on every boot (`build_identity_page_tables`
/// below) — one PML4 page, one PDPTE page, and enough PDE pages to cover `GUEST_RAM_SIZE` via
/// 2 MiB pages (never any 4 KiB leaf, so the table is small and construction stays O(RAM/2MiB)).
pub const PML4_ADDR: u64 = 0x0000_9000;
pub const PDPTE_ADDR: u64 = 0x0000_A000;
pub const PDE_ADDR: u64 = 0x0000_B000;

/// Boot-time stack pointer: high enough in low memory to not collide with the tables/cmdline/zero
/// page above, growing down.
pub const BOOT_STACK_POINTER: u64 = 0x0000_FFF0;

/// The command line must fit before the kernel load address, and the kernel must load at or above
/// high memory — both sides of each comparison are `const`, so this is checked once at compile
/// time rather than as a runtime assertion on values that can never change without recompiling
/// (clippy: `assertions_on_constants`).
const _STATIC_LAYOUT_INVARIANTS: () = {
    assert!(CMDLINE_ADDR + CMDLINE_MAX_SIZE as u64 <= KERNEL_LOAD_ADDR);
    assert!(KERNEL_LOAD_ADDR >= HIMEM_START);
};

const PAGE_TABLE_ENTRY_COUNT: usize = 512;
const PDE_PAGE_SIZE_BYTES: u64 = 2 * 1024 * 1024; // one PDE entry maps a 2 MiB page

/// Page-table entry flags (Intel SDM Vol. 3A §4.5): present (bit 0), writable (bit 1), and — only
/// on PDE entries — page-size (bit 7, "this PDE maps a 2 MiB page directly, no PT beneath it").
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_PAGE_SIZE_2MB: u64 = 1 << 7;

/// One fixed, minimal identity map: guest-virtual address == guest-physical address for every
/// byte of `GUEST_RAM_SIZE`, built from exactly `1 PML4 + 1 PDPTE + ceil(RAM / 1GiB) PDE pages`
/// (one PDPTE page's 512 entries already cover 512 GiB, so `GUEST_RAM_SIZE` never needs more than
/// one PDPTE page at the sizes baud boots today). This is the long-mode direct-boot technique
/// every minimal rust-vmm VMM uses in place of emulating the kernel's own real-mode/legacy
/// page-table setup code (specs/baud-multiverse.md §3.6's subtractive rule: no host interrupts, no
/// real BIOS, "down to a console plus the tape device" — including no real-mode boot trampoline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPageTables {
    /// One page (4 KiB, 512 x 8-byte entries) — entry 0 points at the PDPTE page.
    pub pml4: [u64; PAGE_TABLE_ENTRY_COUNT],
    /// One page — entry 0 points at the first PDE page (only one needed while `GUEST_RAM_SIZE`
    /// stays under 1 GiB per PDPTE entry).
    pub pdpte: [u64; PAGE_TABLE_ENTRY_COUNT],
    /// One page per 1 GiB of `ram_size`, each holding up to 512 2 MiB leaf mappings.
    pub pde_pages: Vec<[u64; PAGE_TABLE_ENTRY_COUNT]>,
}

/// Build the fixed identity map covering `ram_size` bytes of guest RAM starting at
/// [`GUEST_RAM_START`]. Pure function of `ram_size` alone — no KVM, no host memory — so it is
/// byte-for-byte unit-tested here; `linux::write_identity_page_tables` writes the three page
/// kinds into guest RAM at [`PML4_ADDR`]/[`PDPTE_ADDR`]/[`PDE_ADDR`] verbatim.
pub fn build_identity_page_tables(ram_size: usize) -> IdentityPageTables {
    let mut pml4 = [0u64; PAGE_TABLE_ENTRY_COUNT];
    let mut pdpte = [0u64; PAGE_TABLE_ENTRY_COUNT];

    pml4[0] = PDPTE_ADDR | PTE_PRESENT | PTE_WRITABLE;

    let two_mb_pages_needed = ram_size.div_ceil(PDE_PAGE_SIZE_BYTES as usize);
    let pde_page_count = two_mb_pages_needed.div_ceil(PAGE_TABLE_ENTRY_COUNT).max(1);

    let mut pde_pages = Vec::with_capacity(pde_page_count);
    // `pde_page_index` is used for its own arithmetic value (the PDPTE-entry address offset and
    // the leaf-GPA calculation below), not just to index `pdpte` — `enumerate()` wouldn't remove
    // any of that arithmetic, only rename the index.
    #[allow(clippy::needless_range_loop)]
    for pde_page_index in 0..pde_page_count {
        pdpte[pde_page_index] = (PDE_ADDR + (pde_page_index as u64) * 0x1000)
            | PTE_PRESENT
            | PTE_WRITABLE;

        let mut pde = [0u64; PAGE_TABLE_ENTRY_COUNT];
        for (entry_index, entry) in pde.iter_mut().enumerate() {
            let leaf_index = pde_page_index * PAGE_TABLE_ENTRY_COUNT + entry_index;
            let leaf_gpa = leaf_index as u64 * PDE_PAGE_SIZE_BYTES;
            if leaf_gpa < ram_size as u64 {
                *entry = leaf_gpa | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE_2MB;
            }
            // Entries past the end of `ram_size` stay zeroed (not present) — a guest access past
            // real RAM must page-fault, never silently map open bus as executable/writable
            // memory (specs/baud-vcpu.md §3's open-bus contract is about PIO/MMIO reads; this is
            // the paging-layer equivalent for addresses with no backing RAM at all).
        }
        pde_pages.push(pde);
    }

    IdentityPageTables { pml4, pdpte, pde_pages }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pml4_entry_zero_points_at_pdpte_present_and_writable() {
        let tables = build_identity_page_tables(GUEST_RAM_SIZE);
        assert_eq!(tables.pml4[0], PDPTE_ADDR | PTE_PRESENT | PTE_WRITABLE);
        assert!(tables.pml4[1..].iter().all(|&e| e == 0), "only entry 0 is used below 512 GiB");
    }

    #[test]
    fn pdpte_entries_point_at_consecutive_pde_pages() {
        let tables = build_identity_page_tables(GUEST_RAM_SIZE);
        for (i, page) in tables.pde_pages.iter().enumerate() {
            assert_eq!(tables.pdpte[i], (PDE_ADDR + (i as u64) * 0x1000) | PTE_PRESENT | PTE_WRITABLE);
            assert!(!page.is_empty());
        }
    }

    #[test]
    fn every_2mb_of_ram_is_identity_mapped_present_writable_2mb_page() {
        let ram = 256 * 1024 * 1024; // 256 MiB -> 128 leaf entries, all in one PDE page
        let tables = build_identity_page_tables(ram);
        assert_eq!(tables.pde_pages.len(), 1);
        let pde = &tables.pde_pages[0];
        let mapped_count = ram / (PDE_PAGE_SIZE_BYTES as usize);
        for (i, &entry) in pde.iter().enumerate() {
            if i < mapped_count {
                let expected_gpa = (i as u64) * PDE_PAGE_SIZE_BYTES;
                assert_eq!(entry, expected_gpa | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE_2MB);
                assert_eq!(entry & !0xFFF, expected_gpa, "identity map: virtual == physical");
            } else {
                assert_eq!(entry, 0, "past RAM must be not-present, never a fabricated mapping");
            }
        }
    }

    #[test]
    fn ram_larger_than_one_gib_spans_multiple_pde_pages() {
        let ram = 1536 * 1024 * 1024; // 1.5 GiB -> needs 2 PDE pages (768 leaf entries)
        let tables = build_identity_page_tables(ram);
        assert_eq!(tables.pde_pages.len(), 2);
        // Last mapped leaf entry (index 767, in the second PDE page at offset 255).
        let last_mapped = &tables.pde_pages[1][255];
        assert_eq!(*last_mapped & !0xFFF, 767 * PDE_PAGE_SIZE_BYTES);
        // First not-present entry right after it.
        assert_eq!(tables.pde_pages[1][256], 0);
    }

    #[test]
    fn identity_map_is_a_pure_function_of_ram_size() {
        assert_eq!(build_identity_page_tables(GUEST_RAM_SIZE), build_identity_page_tables(GUEST_RAM_SIZE));
    }

    /// The boot-flow fixed addresses must not overlap each other — a real overlap would silently
    /// corrupt whichever structure is written second. This is a static layout invariant, checked
    /// once here rather than trusted by inspection.
    #[test]
    fn fixed_boot_addresses_do_not_overlap() {
        let regions: &[(&str, u64, u64)] = &[
            ("pml4", PML4_ADDR, 0x1000),
            ("pdpte", PDPTE_ADDR, 0x1000),
            ("pde", PDE_ADDR, 0x1000), // first PDE page; additional pages follow contiguously
            ("zero_page", ZERO_PAGE_ADDR, 0x1000),
        ];
        for (i, &(name_a, start_a, len_a)) in regions.iter().enumerate() {
            for &(name_b, start_b, len_b) in &regions[i + 1..] {
                let end_a = start_a + len_a;
                let end_b = start_b + len_b;
                assert!(
                    end_a <= start_b || end_b <= start_a,
                    "boot region {name_a} [{start_a:#x}, {end_a:#x}) overlaps {name_b} [{start_b:#x}, {end_b:#x})"
                );
            }
        }
        // The command-line/kernel-load ordering is a compile-time invariant of the constants
        // themselves — checked once at the module level (`STATIC_LAYOUT_INVARIANTS` below), not
        // here, since both sides are `const` (clippy: `assertions_on_constants`).
    }
}
