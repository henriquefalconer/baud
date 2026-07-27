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

/// Start of the low-memory `usable` e820 range `write_e820_map` reports (real-hardware finding,
/// todo.md §14: a real Linux kernel's `reserve_real_mode()` unconditionally needs sub-1MiB memory
/// for the AP-bringup/ACPI-resume real-mode trampoline — `init_real_mode()` panics outright
/// ("Real mode trampoline was not allocated") if none is available, even though this machine never
/// uses SMP or ACPI resume). Page zero itself stays reserved (the conventional real-mode IVT/BDA
/// carve-out every x86 boot convention keeps aside, matching Firecracker's own low-memory e820
/// layout) even though nothing here is a real BIOS; every fixed low-memory structure this crate
/// writes (zero page, RNG seed, page tables, GDT, cmdline) sits inside this same usable range too —
/// safe because the kernel copies everything it still needs (`boot_params`, the command line) into
/// its own compiled-in memory during early 64-bit entry, well before `memblock`/e820 parsing ever
/// runs, and switches off baud's bootstrap page tables onto its own static ones just as early — the
/// same handoff contract every direct-boot VMM (Firecracker included) already relies on.
pub const LOW_MEM_RAM_START: u64 = 0x0000_1000;

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
/// tables, per convention shared with Firecracker's `arch::x86_64::layout::ZERO_PAGE_START`. The
/// zero page itself is one 4 KiB page (`struct boot_params` is exactly `PAGE_SIZE`), so the next
/// free page is `ZERO_PAGE_ADDR + 0x1000`.
pub const ZERO_PAGE_ADDR: u64 = 0x0000_7000;

/// The Linux/x86 `SETUP_RNG_SEED` (type 9) `setup_data` node baud pins (specs/baud-multiverse.md
/// §3.8's "Boot RNG seed"): a `struct setup_data { next: u64; type: u32; len: u32; data: [u8] }`
/// with `type = 9` and `data` holding the tape-derived seed, which `arch/x86/kernel/setup.c`'s
/// `parse_setup_data` feeds straight to `add_bootloader_randomness()`. Sits in the free page
/// right after the zero page, well before [`PML4_ADDR`].
pub const RNG_SEED_SETUP_DATA_ADDR: u64 = ZERO_PAGE_ADDR + 0x1000;

/// Total on-the-wire size of the `setup_data` node at [`RNG_SEED_SETUP_DATA_ADDR`]: the 16-byte
/// `{next: u64, type: u32, len: u32}` header plus a 32-byte seed.
pub const RNG_SEED_SETUP_DATA_LEN: u64 = 16 + 32;

/// Kernel command line: also low memory, sized generously for the tape-device driver's boot
/// arguments (specs/baud-tape-device.md's guest-side driver contract).
pub const CMDLINE_ADDR: u64 = 0x0002_0000;
pub const CMDLINE_MAX_SIZE: usize = 0x1_0000;

/// Where the initramfs (`ramdisk_image`/`ramdisk_size`, todo.md §4.2) is loaded by default — 32 MiB
/// in, comfortably clear of [`KERNEL_LOAD_ADDR`] for any minimal builtin kernel (todo.md §4.1's
/// no-modules config; a `bzImage` built that way is a few MiB at most) with room to grow, and well
/// inside [`GUEST_RAM_SIZE`] so a compressed initramfs has tens of MiB of headroom before it would
/// need to shrink below what a real rootfs (§4.5) needs. **Not always the actual load address** —
/// see [`initramfs_load_addr`], which every real caller uses instead of this constant directly.
pub const INITRAMFS_ADDR: u64 = 0x0200_0000;

/// The real initramfs load address for a kernel whose own self-decompression footprint is
/// `kernel_init_size` bytes (`setup_header.init_size`, read straight off the loaded bzImage —
/// Documentation/x86/boot.txt: the bootloader must place nothing else in
/// `[code load address, code load address + init_size)` until the kernel proper has decompressed
/// itself). A real distro kernel found this fixed-32-MiB constant broken in practice: Ubuntu
/// 18.04.1's stock `vmlinuz-generic` (4.15) reports `init_size = 0x1e4f000` (~30.4 MiB) from
/// [`KERNEL_LOAD_ADDR`] (2 MiB) — the kernel's own decompression scratch space extends to
/// ~32.28 MiB, just past [`INITRAMFS_ADDR`] at exactly 32 MiB, so the kernel silently overwrote the
/// first ~300 KiB of the initrd with its own decompression output before ever unpacking it
/// (`Initramfs unpacking failed: junk in compressed archive`, then a page fault in
/// `free_reserved_area` freeing the now-inconsistent initrd pages). Every prior baud fixture kernel
/// (todo.md §4.1's no-modules minimal config) stayed small enough that `KERNEL_LOAD_ADDR +
/// init_size` never reached 32 MiB, so this collision was real but unobserved until a full-size
/// distro kernel was tried.
///
/// **`init_size` alone is not the whole story**: moving the initramfs to exactly
/// `KERNEL_LOAD_ADDR + init_size` (no margin) still reproduced the identical crash — real-hardware
/// bisection against this exact kernel found the true requirement lands somewhere between 8 MiB and
/// 16 MiB *past* `init_size` (a fixed +8 MiB margin still corrupted the initrd; +16 MiB and +32 MiB
/// were both clean, confirmed by booting past `Unpacking initramfs...` to `Freeing unused kernel
/// memory` with no oops). The precise mechanism was not root-caused further (a plausible suspect:
/// the decompressor's own internal relocate-then-decompress safety copy, Documentation/x86/boot.txt
/// does not fully spell out its size) — rather than chase it further, a fixed `+32 MiB` safety
/// margin is applied on top of `init_size`, but **only when the kernel's raw (no-margin) scratch
/// space already reaches [`INITRAMFS_ADDR`]** — every existing fixture kernel's raw
/// `KERNEL_LOAD_ADDR + init_size` sits comfortably below [`INITRAMFS_ADDR`] (a few MiB at most, todo.md
/// §4.1's no-modules config), so this returns exactly [`INITRAMFS_ADDR`] unchanged for all of them
/// (verified by `initramfs_load_addr_is_unchanged_for_any_kernel_small_enough_to_fit_under_the_old_
/// constant` below); applying the margin unconditionally would have pushed every one of those small
/// kernels' placement a full 32 MiB higher for no reason, breaking their locked-in
/// `ramdisk_image == INITRAMFS_ADDR` assertions.
pub fn initramfs_load_addr(kernel_init_size: u32) -> u64 {
    const DECOMPRESSION_SAFETY_MARGIN: u64 = 32 * 1024 * 1024;
    let kernel_scratch_end = KERNEL_LOAD_ADDR + u64::from(kernel_init_size);
    if kernel_scratch_end <= INITRAMFS_ADDR {
        INITRAMFS_ADDR
    } else {
        (kernel_scratch_end + DECOMPRESSION_SAFETY_MARGIN).next_multiple_of(0x1000)
    }
}

/// The three fixed page-table pages built fresh on every boot (`build_identity_page_tables`
/// below) — one PML4 page, one PDPTE page, and enough PDE pages to cover `GUEST_RAM_SIZE` via
/// 2 MiB pages (never any 4 KiB leaf, so the table is small and construction stays O(RAM/2MiB)),
/// plus one more PDE page `build_identity_page_tables` always appends after the RAM-covering ones
/// to identity-map both MMIO device windows ([`VIRTIO_MMIO_RNG_BASE`] and [`LAPIC_MMIO_BASE`] —
/// they share one PDPTE region, so both leaves fit in this same page) — paging is mandatory in
/// long mode, so a guest can only *reach* a GPA outside registered RAM at all if its own page
/// tables have a present translation for it; without this, a guest touching either window takes a
/// genuine `#PF` long before the access could ever become the intended VM exit. [`GDT_ADDR`]
/// starts one page later than it otherwise would, to leave room for it.
pub const PML4_ADDR: u64 = 0x0000_9000;
pub const PDPTE_ADDR: u64 = 0x0000_A000;
pub const PDE_ADDR: u64 = 0x0000_B000;

/// Boot-time stack pointer: high enough in low memory to not collide with the tables/cmdline/zero
/// page above, growing down.
pub const BOOT_STACK_POINTER: u64 = 0x0000_FFF0;

/// A real in-memory flat GDT (H4, specs/baud-vcpu.md §5): `long_mode_sregs` used to set `GDT`
/// base/limit to `0`/`0` on the theory that "the guest never executes `LGDT`, so no in-memory GDT
/// is ever built or read" — true for ordinary instruction execution (KVM's segment-descriptor
/// cache is loaded directly from `kvm_sregs`), but **not** true the moment a real interrupt is
/// injected: per the Intel SDM, an IDT interrupt/trap gate's far transfer always reloads `CS` via
/// a real GDT descriptor-table lookup of the gate's target selector, regardless of how the
/// *current* CS got there. `inject_at` (`baud_vcpu::boundary`) landing an interrupt into a guest's
/// IDT-registered handler therefore needs an actual GDT in guest memory or the CPU faults trying
/// to read it. One page is reserved; only 3 entries (24 bytes) are ever written. Starts at
/// `0xD000`, not `0xC000`, to leave room for [`PDE_ADDR`]'s second (virtio-mmio-window) page.
pub const GDT_ADDR: u64 = 0x0000_D000;

/// Selector for the flat 64-bit code segment ([`build_flat_gdt`]'s index 1) — matches
/// `pagetables::long_mode_sregs`'s `cs.selector` exactly, so `kvm_sregs` (KVM's direct-loaded
/// segment cache) and this in-memory table describe the identical segment.
pub const GDT_CODE_SELECTOR: u16 = 0x08;
/// Selector for the flat data segment ([`build_flat_gdt`]'s index 2) — matches
/// `pagetables::long_mode_sregs`'s data-segment selector.
pub const GDT_DATA_SELECTOR: u16 = 0x10;

/// The minimal 3-entry flat GDT every long-mode direct-boot VMM needs once real IDT-gate interrupt
/// delivery is possible (H4): a null descriptor, a flat 64-bit code segment (selector
/// [`GDT_CODE_SELECTOR`], `L=1`, `D=0`, present, ring 0, execute/read), and a flat data segment
/// (selector [`GDT_DATA_SELECTOR`], present, ring 0, read/write) — the standard "flat long-mode
/// GDT" triple (base/limit ignored by the CPU for `L=1` code and any data segment in 64-bit mode,
/// so both carry base=0/limit=0). Pure function of nothing (this table never varies), unit-tested
/// byte-for-byte here exactly like [`build_identity_page_tables`].
pub fn build_flat_gdt() -> [u64; 3] {
    const NULL_DESCRIPTOR: u64 = 0;
    // Access byte (bit 47..40 of the descriptor): P=1, DPL=00, S=1, Type=1010 (code, execute/read).
    // Flags nibble (bit 55..52): G=1, D/B=0, L=1, AVL=0 -- base/limit fields left 0 (ignored, L=1).
    const CODE_DESCRIPTOR: u64 = 0x00A0_9A00_0000_0000;
    // Access byte: P=1, DPL=00, S=1, Type=0010 (data, read/write). Flags nibble: G=1, D/B=1, L=0
    // (matches `pagetables::long_mode_sregs`'s data segment: `db=1`).
    const DATA_DESCRIPTOR: u64 = 0x00C0_9200_0000_0000;
    [NULL_DESCRIPTOR, CODE_DESCRIPTOR, DATA_DESCRIPTOR]
}

/// The ACPI Root System Description Pointer (ACPI spec §5.2.5) must sit somewhere inside the
/// conventional BIOS-area scan window `0xE0000..0x100000` — ACPICA's `acpi_find_root_pointer`
/// (called unconditionally by `acpi_boot_table_init`, with no cmdline/e820/boot_params hint route
/// on x86_64 Linux) byte-scans exactly that fixed physical range, on 16-byte boundaries, for the
/// 8-byte `"RSD PTR "` signature — real hardware or not, a direct-boot VMM with no BIOS still has
/// to land the RSDP there or the guest's ACPI subsystem will never find it at all. This is the one
/// ACPI-table address that is not free-choice, unlike every other table below (reached only via
/// the RSDP's own pointers). Nothing else in this file claims any address in `0xE0000..0x100000`
/// today (the next lower claim is [`GDT_ADDR`] at `0xD000`, the next higher is [`HIMEM_START`]
/// itself), so the whole window is free.
pub const ACPI_RSDP_ADDR: u64 = 0x000E_0000;
/// XSDT/FADT/DSDT/MADT (todo.md §14 item 5(c), "minimal ACPI: RSDP -> RSDT/XSDT -> FADT + DSDT +
/// MADT with one LAPIC") have no fixed-address requirement of their own — only the RSDP's own
/// `XsdtAddress`/`Dsdt`/table-pointer fields need to name them, so they are packed one per page
/// right after the RSDP, comfortably inside the same free BIOS-area window above, with room to
/// grow if any one of them needs more than a page.
pub const ACPI_XSDT_ADDR: u64 = ACPI_RSDP_ADDR + 0x1000;
pub const ACPI_FADT_ADDR: u64 = ACPI_XSDT_ADDR + 0x1000;
pub const ACPI_DSDT_ADDR: u64 = ACPI_FADT_ADDR + 0x1000;
pub const ACPI_MADT_ADDR: u64 = ACPI_DSDT_ADDR + 0x1000;

/// MMIO device windows — deliberately **outside** [`GUEST_RAM_SIZE`], since any address KVM has a
/// registered memory region for is served straight from guest RAM and never reaches a VM exit at
/// all; a device window must sit somewhere `KVM_SET_USER_MEMORY_REGION` never claims so a guest
/// access to it always traps. Matches the address an unmodified Linux kernel is told to probe via
/// the `virtio_mmio.device=<size>@<base>:<irq>` cmdline parameter (the same convention Firecracker
/// and crosvm use for a direct-boot guest with no ACPI/PCI/DT to auto-discover devices through —
/// todo.md §3.8's virtio-rng entry). One 512-byte (`0x200`) window comfortably covers every
/// register `virtio_mmio.rs`'s [`crate::virtio_mmio::VirtioMmioTransport`] defines, config space
/// included, with room to spare (virtio-mmio v2, specs/baud-multiverse.md §3's determinism table).
pub const VIRTIO_MMIO_RNG_BASE: u64 = 0xd000_0000;
pub const VIRTIO_MMIO_RNG_LEN: u64 = 0x200;

/// The conventional Local APIC MMIO base (Intel SDM Vol. 3A §10.4.3, absent an `IA32_APIC_BASE`
/// MSR override this crate never installs) -- also the value [`crate::acpi::build_madt`] publishes
/// as the MADT's own "Local Interrupt Controller Address" field, so [`crate::lapic::LocalApic`]
/// (todo.md §14 item 5(c)'s second flagged gap) answers exactly the address a real kernel's
/// LAPIC-ID probe reads. One 4 KiB window (`LocalApic::WINDOW_LEN`), the standard xAPIC MMIO size.
pub const LAPIC_MMIO_BASE: u64 = 0xFEE0_0000;
pub const LAPIC_MMIO_LEN: u64 = 0x1000;

/// The command line must fit before the kernel load address, and the kernel must load at or above
/// high memory — both sides of each comparison are `const`, so this is checked once at compile
/// time rather than as a runtime assertion on values that can never change without recompiling
/// (clippy: `assertions_on_constants`).
const _STATIC_LAYOUT_INVARIANTS: () = {
    assert!(CMDLINE_ADDR + CMDLINE_MAX_SIZE as u64 <= KERNEL_LOAD_ADDR);
    assert!(KERNEL_LOAD_ADDR >= HIMEM_START);
    assert!(RNG_SEED_SETUP_DATA_ADDR + RNG_SEED_SETUP_DATA_LEN <= PML4_ADDR);
    assert!(INITRAMFS_ADDR > KERNEL_LOAD_ADDR);
    assert!((INITRAMFS_ADDR as usize) < GUEST_RAM_SIZE);
    assert!(VIRTIO_MMIO_RNG_BASE >= GUEST_RAM_SIZE as u64);
    assert!(ACPI_RSDP_ADDR >= 0x000E_0000 && ACPI_RSDP_ADDR + 0x1000 <= HIMEM_START);
    assert!(ACPI_MADT_ADDR + 0x1000 <= HIMEM_START);
    assert!(LAPIC_MMIO_BASE >= GUEST_RAM_SIZE as u64);
    // Both device windows must share one PDPTE entry (`build_identity_page_tables` only ever
    // allocates one dedicated PDE page for both leaves) -- true today (both fall in the top 1 GiB
    // of 32-bit address space) but re-checked here so a future address change fails loudly at
    // compile time instead of silently mis-mapping one window.
    assert!(LAPIC_MMIO_BASE / (1u64 << 30) == VIRTIO_MMIO_RNG_BASE / (1u64 << 30));
};

const PAGE_TABLE_ENTRY_COUNT: usize = 512;
const PDE_PAGE_SIZE_BYTES: u64 = 2 * 1024 * 1024; // one PDE entry maps a 2 MiB page

/// Page-table entry flags (Intel SDM Vol. 3A §4.5): present (bit 0), writable (bit 1), and — only
/// on PDE entries — page-size (bit 7, "this PDE maps a 2 MiB page directly, no PT beneath it").
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_PAGE_SIZE_2MB: u64 = 1 << 7;

/// One fixed, minimal identity map: guest-virtual address == guest-physical address for every
/// byte of `GUEST_RAM_SIZE`, plus one dedicated leaf for the virtio-mmio device window
/// ([`VIRTIO_MMIO_RNG_BASE`], see [`build_identity_page_tables`]'s doc for why that leaf must
/// exist at all). Built from `1 PML4 + 1 PDPTE + ceil(RAM / 1GiB) PDE pages` for RAM, plus exactly
/// one more PDE page for the virtio-mmio window (one PDPTE page's 512 entries already cover
/// 512 GiB, so neither RAM nor the device window ever needs more than this one PDPTE page at the
/// sizes baud boots today). This is the long-mode direct-boot technique every minimal rust-vmm VMM
/// uses in place of emulating the kernel's own real-mode/legacy page-table setup code
/// (specs/baud-multiverse.md §3.6's subtractive rule: no host interrupts, no real BIOS, "down to a
/// console plus the tape device" — including no real-mode boot trampoline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPageTables {
    /// One page (4 KiB, 512 x 8-byte entries) — entry 0 points at the PDPTE page.
    pub pml4: [u64; PAGE_TABLE_ENTRY_COUNT],
    /// One page — one entry per 1 GiB region that has a mapped PDE page: RAM's (always index 0,
    /// while `GUEST_RAM_SIZE` stays under 1 GiB) and the virtio-mmio window's (a fixed, distinct
    /// index derived from [`VIRTIO_MMIO_RNG_BASE`]).
    pub pdpte: [u64; PAGE_TABLE_ENTRY_COUNT],
    /// One page per 1 GiB of `ram_size` (each holding up to 512 2 MiB leaf mappings), plus exactly
    /// one more page — always last — holding the virtio-mmio window's single leaf mapping.
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

    let mut pde_pages = Vec::with_capacity(pde_page_count + 1); // +1 for the virtio-mmio-window page below
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

    // Also identity-map the fixed virtio-mmio device window (`VIRTIO_MMIO_RNG_BASE`): it sits
    // outside `GUEST_RAM_SIZE` so it is never backed by a registered KVM memory region (an access
    // still traps to a real VM exit, per this module's own MMIO-window doc), but paging is
    // mandatory in long mode, so a guest can only *reach* that GPA at all if its own page tables
    // have a present translation for it -- an identity map that only covers RAM leaves this
    // address entirely unmapped, so a guest touching it takes a genuine #PF long before the
    // access could ever become a VM exit (found for real: `virtio-rng-guest`'s first boot attempt
    // halted via an unhandled fault before ever reaching its own `QueueNotify` write). One
    // dedicated PDPTE entry + PDE page, present in every boot's bootstrap identity map -- harmless
    // for any guest that never touches this GVA (every fixture before `virtio-rng-guest`), since
    // real Linux replaces this whole bootstrap map with its own dynamic (`ioremap`-built) page
    // tables during early boot regardless.
    let mmio_pdpte_index = (VIRTIO_MMIO_RNG_BASE / (1u64 << 30)) as usize;
    let mmio_pde_index = ((VIRTIO_MMIO_RNG_BASE % (1u64 << 30)) / PDE_PAGE_SIZE_BYTES) as usize;
    assert!(
        mmio_pdpte_index >= pde_page_count,
        "the virtio-mmio window's PDPTE entry must not collide with a RAM-covering one"
    );
    let mut mmio_pde = [0u64; PAGE_TABLE_ENTRY_COUNT];
    mmio_pde[mmio_pde_index] = VIRTIO_MMIO_RNG_BASE | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE_2MB;

    // The LAPIC MMIO window ([`LAPIC_MMIO_BASE`], `crate::lapic`) shares this same 1 GiB PDPTE
    // region with the virtio-mmio window (`_STATIC_LAYOUT_INVARIANTS` above asserts this), so its
    // leaf goes into this same PDE page rather than needing a whole new PDPTE entry/PDE page --
    // paging is mandatory in long mode, so without this a guest whose MADT advertises a LAPIC
    // would take a genuine `#PF` reaching it, exactly the bug this same treatment already fixed
    // for the virtio-mmio window (`virtio-rng-guest`'s first boot attempt).
    let lapic_pde_index = ((LAPIC_MMIO_BASE % (1u64 << 30)) / PDE_PAGE_SIZE_BYTES) as usize;
    assert!(lapic_pde_index != mmio_pde_index, "the two device windows must not alias one leaf");
    mmio_pde[lapic_pde_index] = LAPIC_MMIO_BASE | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE_2MB;

    let mmio_pde_page_index = pde_pages.len();
    pdpte[mmio_pdpte_index] =
        (PDE_ADDR + (mmio_pde_page_index as u64) * 0x1000) | PTE_PRESENT | PTE_WRITABLE;
    pde_pages.push(mmio_pde);

    IdentityPageTables { pml4, pdpte, pde_pages }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initramfs_load_addr_is_unchanged_for_any_kernel_small_enough_to_fit_under_the_old_constant() {
        // Every fixture kernel in this workspace (todo.md §4.1's no-modules minimal config) has an
        // init_size far under `INITRAMFS_ADDR - KERNEL_LOAD_ADDR` (30 MiB) -- confirm the new
        // dynamic placement is a no-op for them, so no existing boot's placement changes.
        assert_eq!(initramfs_load_addr(0), INITRAMFS_ADDR);
        assert_eq!(initramfs_load_addr(4 * 1024 * 1024), INITRAMFS_ADDR); // a 4 MiB kernel
        assert_eq!(
            initramfs_load_addr((INITRAMFS_ADDR - KERNEL_LOAD_ADDR) as u32),
            INITRAMFS_ADDR,
            "landing exactly on the old boundary must not push past it"
        );
    }

    #[test]
    fn initramfs_load_addr_moves_past_a_real_distro_kernels_larger_init_size() {
        // Ubuntu 18.04.1's stock vmlinuz-generic (4.15): init_size = 0x1e4f000, which the fixed
        // 32 MiB constant collided with (the kernel's own self-decompression overwrote the first
        // ~300 KiB of the initrd before it was ever unpacked). The dynamic address must clear the
        // kernel's own scratch region entirely.
        const UBUNTU_INIT_SIZE: u32 = 0x01e4_f000;
        let addr = initramfs_load_addr(UBUNTU_INIT_SIZE);
        assert!(
            addr >= KERNEL_LOAD_ADDR + u64::from(UBUNTU_INIT_SIZE),
            "initramfs must load at or after the kernel's own decompression scratch space ends"
        );
        assert!(addr > INITRAMFS_ADDR, "this kernel's footprint exceeds the old fixed constant");
        assert_eq!(addr % 0x1000, 0, "must stay 4 KiB-aligned");
    }

    #[test]
    fn pml4_entry_zero_points_at_pdpte_present_and_writable() {
        let tables = build_identity_page_tables(GUEST_RAM_SIZE);
        assert_eq!(tables.pml4[0], PDPTE_ADDR | PTE_PRESENT | PTE_WRITABLE);
        assert!(tables.pml4[1..].iter().all(|&e| e == 0), "only entry 0 is used below 512 GiB");
    }

    #[test]
    fn pdpte_entries_point_at_consecutive_pde_pages() {
        let tables = build_identity_page_tables(GUEST_RAM_SIZE);
        // The last page is always the dedicated virtio-mmio-window page (a distinct, non-
        // contiguous PDPTE index) -- only the RAM-covering pages before it are contiguous.
        let ram_pde_pages = tables.pde_pages.len() - 1;
        for i in 0..ram_pde_pages {
            assert_eq!(tables.pdpte[i], (PDE_ADDR + (i as u64) * 0x1000) | PTE_PRESENT | PTE_WRITABLE);
            assert!(!tables.pde_pages[i].is_empty());
        }
    }

    #[test]
    fn every_2mb_of_ram_is_identity_mapped_present_writable_2mb_page() {
        let ram = 256 * 1024 * 1024; // 256 MiB -> 128 leaf entries, all in one PDE page
        let tables = build_identity_page_tables(ram);
        assert_eq!(tables.pde_pages.len(), 2, "one RAM page plus the dedicated virtio-mmio-window page");
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
        let ram = 1536 * 1024 * 1024; // 1.5 GiB -> needs 2 RAM PDE pages (768 leaf entries)
        let tables = build_identity_page_tables(ram);
        assert_eq!(tables.pde_pages.len(), 3, "2 RAM pages plus the dedicated virtio-mmio-window page");
        // Last mapped leaf entry (index 767, in the second RAM PDE page at offset 255).
        let last_mapped = &tables.pde_pages[1][255];
        assert_eq!(*last_mapped & !0xFFF, 767 * PDE_PAGE_SIZE_BYTES);
        // First not-present entry right after it.
        assert_eq!(tables.pde_pages[1][256], 0);
    }

    /// The dedicated leaf `build_identity_page_tables` always appends for
    /// [`VIRTIO_MMIO_RNG_BASE`] — the fix for a real bug `virtio-rng-guest`'s first boot attempt
    /// found (that fixture halted on an unhandled fault before ever reaching its own `QueueNotify`
    /// write): without this, a guest touching that GVA has no present page-table translation for
    /// it at all, so paging (mandatory in long mode) faults before the access could ever become
    /// the intended VM exit.
    #[test]
    fn identity_map_also_covers_the_virtio_mmio_window() {
        let tables = build_identity_page_tables(GUEST_RAM_SIZE);
        let mmio_pdpte_index = (VIRTIO_MMIO_RNG_BASE / (1u64 << 30)) as usize;
        let mmio_pde_index = ((VIRTIO_MMIO_RNG_BASE % (1u64 << 30)) / PDE_PAGE_SIZE_BYTES) as usize;

        assert_ne!(mmio_pdpte_index, 0, "must not collide with RAM's own PDPTE entry 0");
        let mmio_pde_page = tables.pde_pages.last().expect("at least one PDE page always exists");
        let expected_pdpte_entry =
            (PDE_ADDR + ((tables.pde_pages.len() - 1) as u64) * 0x1000) | PTE_PRESENT | PTE_WRITABLE;
        assert_eq!(tables.pdpte[mmio_pdpte_index], expected_pdpte_entry);

        assert_eq!(
            mmio_pde_page[mmio_pde_index],
            VIRTIO_MMIO_RNG_BASE | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE_2MB
        );
        // Every other entry in this page stays not-present, except the LAPIC window's own leaf
        // (`identity_map_also_covers_the_lapic_window` below) -- only these two are ever mapped.
        let lapic_pde_index = ((LAPIC_MMIO_BASE % (1u64 << 30)) / PDE_PAGE_SIZE_BYTES) as usize;
        for (i, &entry) in mmio_pde_page.iter().enumerate() {
            if i != mmio_pde_index && i != lapic_pde_index {
                assert_eq!(entry, 0, "no fabricated mapping outside the two device windows");
            }
        }
    }

    /// [`LAPIC_MMIO_BASE`]'s leaf, added alongside the virtio-mmio window's in the very same PDE
    /// page (both share one PDPTE region) -- see `build_identity_page_tables`'s own comment on why
    /// a real `CONFIG_ACPI=y` guest whose MADT advertises a LAPIC needs this exactly like
    /// `identity_map_also_covers_the_virtio_mmio_window` above.
    #[test]
    fn identity_map_also_covers_the_lapic_window() {
        let tables = build_identity_page_tables(GUEST_RAM_SIZE);
        let lapic_pdpte_index = (LAPIC_MMIO_BASE / (1u64 << 30)) as usize;
        let lapic_pde_index = ((LAPIC_MMIO_BASE % (1u64 << 30)) / PDE_PAGE_SIZE_BYTES) as usize;
        assert_eq!(
            lapic_pdpte_index,
            (VIRTIO_MMIO_RNG_BASE / (1u64 << 30)) as usize,
            "both device windows share one PDPTE entry"
        );
        let mmio_pde_page = tables.pde_pages.last().expect("at least one PDE page always exists");
        assert_eq!(
            mmio_pde_page[lapic_pde_index],
            LAPIC_MMIO_BASE | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE_2MB
        );
    }

    #[test]
    fn identity_map_is_a_pure_function_of_ram_size() {
        assert_eq!(build_identity_page_tables(GUEST_RAM_SIZE), build_identity_page_tables(GUEST_RAM_SIZE));
    }

    /// `build_flat_gdt`'s three entries decoded by hand against the Intel SDM's segment-descriptor
    /// bit layout (byte 5 = access, byte 6 = flags nibble | limit-high nibble) — every field this
    /// crate's own GDT actually depends on (present, S, type, DPL, L, D/B) checked explicitly
    /// rather than trusted from the hex literal alone.
    #[test]
    fn flat_gdt_entries_have_the_expected_descriptor_fields() {
        let gdt = build_flat_gdt();
        assert_eq!(gdt[0], 0, "entry 0 must be the null descriptor");

        let code = gdt[1].to_le_bytes();
        assert_eq!(code[5] & 0x80, 0x80, "code segment must be present");
        assert_eq!(code[5] & 0x60, 0, "code segment DPL must be 0 (ring 0)");
        assert_eq!(code[5] & 0x10, 0x10, "code segment must be S=1 (code/data, not system)");
        assert_eq!(code[5] & 0x0F, 0x0A, "code segment type must be execute/read (0xA)");
        assert_eq!(code[6] & 0x20, 0x20, "code segment must be L=1 (64-bit long mode)");
        assert_eq!(code[6] & 0x40, 0, "a 64-bit (L=1) code segment must have D=0");

        let data = gdt[2].to_le_bytes();
        assert_eq!(data[5] & 0x80, 0x80, "data segment must be present");
        assert_eq!(data[5] & 0x10, 0x10, "data segment must be S=1");
        assert_eq!(data[5] & 0x0F, 0x02, "data segment type must be read/write (0x2)");
        assert_eq!(data[6] & 0x20, 0, "data segment must have L=0");
    }

    /// The boot-flow fixed addresses must not overlap each other — a real overlap would silently
    /// corrupt whichever structure is written second. This is a static layout invariant, checked
    /// once here rather than trusted by inspection.
    #[test]
    fn fixed_boot_addresses_do_not_overlap() {
        let regions: &[(&str, u64, u64)] = &[
            ("pml4", PML4_ADDR, 0x1000),
            ("pdpte", PDPTE_ADDR, 0x1000),
            // `GUEST_RAM_SIZE` (256 MiB) needs exactly one RAM-covering PDE page, plus the
            // dedicated virtio-mmio-window page `build_identity_page_tables` always appends --
            // 2 pages total in the common (this project's) case; `GDT_ADDR` starts right after.
            ("pde", PDE_ADDR, 0x2000),
            ("zero_page", ZERO_PAGE_ADDR, 0x1000),
            ("gdt", GDT_ADDR, 0x1000),
            // The 5 ACPI tables (RSDP..MADT) are packed one page apart, so one combined region
            // spanning all of them is enough to confirm they don't collide with anything above.
            ("acpi_tables", ACPI_RSDP_ADDR, ACPI_MADT_ADDR + 0x1000 - ACPI_RSDP_ADDR),
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
