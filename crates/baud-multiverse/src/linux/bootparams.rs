// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Loads a bzImage kernel and writes the Linux/x86 "zero page" (`struct boot_params`) plus the
// command line into guest memory (specs/baud-multiverse.md §2's "load the guest kernel with
// linux-loader and write boot params at fixed addresses"). The tape-device driver contract
// (specs/baud-tape-device.md) is carried entirely in the command line at this layer — this module
// only knows how to get a kernel image and a cmdline string into guest memory; it does not
// interpret what the cmdline says.

use crate::layout;
use linux_loader::cmdline::Cmdline;
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams as LoaderBootParams};
use linux_loader::loader::bootparam::{boot_e820_entry, boot_params};
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::{load_cmdline, KernelLoader, KernelLoaderResult};
use std::fs::File;
use std::path::Path;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};

/// `arch/x86/include/uapi/asm/bootparam.h`'s `enum { ... SETUP_RNG_SEED = 9, ... }` — the
/// `setup_data` node type `arch/x86/kernel/setup.c`'s `parse_setup_data` routes straight to
/// `add_bootloader_randomness(data->data, data->len)` (specs/baud-multiverse.md §3.8's "Boot RNG
/// seed": the one boot-seed path baud owns on a direct x86_64 kernel boot).
const SETUP_RNG_SEED: u32 = 9;

/// Seed length baud writes into the `SETUP_RNG_SEED` node — 32 bytes, matching the CRNG key size
/// `crng_reseed`/`extract_entropy` mix in, so the whole seed is credited as full-quality entropy.
pub const RNG_SEED_LEN: usize = 32;

/// Linux/x86 boot protocol magic values a well-formed `boot_params.hdr` must carry
/// (Documentation/x86/boot.txt) — `type_of_loader = 0xFF` marks baud as an "unknown bootloader",
/// which is the correct, honest value for a loader that is not one of the protocol's registered
/// IDs.
const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xAA55;
const KERNEL_HDR_MAGIC: u32 = 0x5372_6448; // "HdrS"
const KERNEL_LOADER_OTHER: u8 = 0xFF;
const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;

/// e820 memory-map types (Documentation/x86/boot.txt): `1` = usable RAM, `2` = reserved.
const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

/// `hdr.loadflags` bits (Documentation/x86/boot.txt): bit 0 `LOADED_HIGH` (the protected-mode
/// kernel is loaded at 1 MiB, not the legacy low-memory address — always true for baud's direct
/// boot, todo.md §4.2) and bit 7 `CAN_USE_HEAP` (a real-mode boot loader may use the heap fields
/// below `heap_end_ptr`; harmless on baud's 64-bit direct-entry boot, which never runs the
/// real-mode setup code that reads it, but §4.2 specifies both bits set for protocol fidelity).
const LOADFLAGS_LOADED_HIGH: u8 = 1 << 0;
const LOADFLAGS_CAN_USE_HEAP: u8 = 1 << 7;

#[derive(Debug, thiserror::Error)]
pub enum BootParamsError {
    #[error("failed to open kernel image {0}: {1}")]
    OpenKernel(std::path::PathBuf, std::io::Error),
    #[error("failed to load bzImage kernel: {0}")]
    LoadKernel(#[from] linux_loader::loader::Error),
    #[error("kernel image carries no setup_header (not a valid bzImage)")]
    MissingSetupHeader,
    #[error("invalid command line: {0}")]
    InvalidCmdline(String),
    #[error("failed writing command line to guest memory: {0}")]
    CmdlineWrite(linux_loader::loader::Error),
    #[error("failed writing zero page to guest memory: {0}")]
    ZeroPageWrite(linux_loader::configurator::Error),
    #[error("failed writing SETUP_RNG_SEED setup_data to guest memory: {0}")]
    RngSeedWrite(vm_memory::guest_memory::Error),
    #[error("failed writing initramfs ({0} bytes) to guest memory: {1}")]
    InitramfsWrite(usize, vm_memory::guest_memory::Error),
}

/// Write the `SETUP_RNG_SEED` `setup_data` node — `{next: 0, type: SETUP_RNG_SEED, len:
/// RNG_SEED_LEN, data: seed}` — at [`layout::RNG_SEED_SETUP_DATA_ADDR`]. `next = 0` terminates the
/// list there: baud never chains another `setup_data` node behind it.
fn write_rng_seed_setup_data<M: GuestMemoryBackend>(
    guest_mem: &M,
    seed: &[u8; RNG_SEED_LEN],
) -> Result<(), vm_memory::guest_memory::Error> {
    let mut bytes = Vec::with_capacity(16 + RNG_SEED_LEN);
    bytes.extend_from_slice(&0u64.to_le_bytes()); // next
    bytes.extend_from_slice(&SETUP_RNG_SEED.to_le_bytes()); // type
    bytes.extend_from_slice(&(RNG_SEED_LEN as u32).to_le_bytes()); // len
    bytes.extend_from_slice(seed); // data
    guest_mem.write_slice(&bytes, GuestAddress(layout::RNG_SEED_SETUP_DATA_ADDR))
}

/// Write `initramfs` verbatim at [`layout::INITRAMFS_ADDR`] — no framing, no compression baud adds
/// itself; the bytes are whatever `baud-packages` (todo.md §4.5) produced (a gzipped reproducible
/// newc cpio, §4.3), and the kernel's own initramfs unpacker handles that format.
fn write_initramfs<M: GuestMemoryBackend>(
    guest_mem: &M,
    initramfs: &[u8],
) -> Result<(), vm_memory::guest_memory::Error> {
    guest_mem.write_slice(initramfs, GuestAddress(layout::INITRAMFS_ADDR))
}

/// Load `kernel_path` (a bzImage) into `guest_mem`, write `cmdline` at [`layout::CMDLINE_ADDR`],
/// pin `rng_seed` into a `SETUP_RNG_SEED` `setup_data` node (specs/baud-multiverse.md §3.8) and
/// point `hdr.setup_data` at it, load `initramfs` (when `Some`) at [`layout::INITRAMFS_ADDR`] and
/// point `hdr.ramdisk_image`/`hdr.ramdisk_size` at it (todo.md §4.2), and write a complete
/// `boot_params` zero page at [`layout::ZERO_PAGE_ADDR`] covering `ram_size` bytes of RAM. Returns
/// the loader result so the caller can set `RIP` to the Linux/x86 64-bit entry point
/// (`kernel_load + ` [`layout::KERNEL_64BIT_ENTRY_OFFSET`]`).
pub fn load_kernel_and_write_boot_params<M: GuestMemoryBackend>(
    guest_mem: &M,
    kernel_path: &Path,
    cmdline: &str,
    ram_size: usize,
    rng_seed: &[u8; RNG_SEED_LEN],
    initramfs: Option<&[u8]>,
) -> Result<KernelLoaderResult, BootParamsError> {
    let mut file = File::open(kernel_path)
        .map_err(|e| BootParamsError::OpenKernel(kernel_path.to_path_buf(), e))?;

    let loader_result = BzImage::load(
        guest_mem,
        Some(GuestAddress(layout::KERNEL_LOAD_ADDR)),
        &mut file,
        Some(GuestAddress(layout::HIMEM_START)),
    )?;

    let mut hdr = loader_result.setup_header.ok_or(BootParamsError::MissingSetupHeader)?;
    hdr.type_of_loader = KERNEL_LOADER_OTHER;
    // A bzImage's own `setup_header` doesn't set the boot-flag/HdrS magics or a minimum kernel
    // alignment for a from-scratch VMM boot — `BzImage::load` only fills in what it read off disk
    // (`code32_start` etc); the rest of the protocol's required fields are the loader's job, same
    // as the crate's own worked example (linux-loader's `LinuxBootConfigurator` doctest).
    hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    hdr.header = KERNEL_HDR_MAGIC;
    hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;
    hdr.loadflags |= LOADFLAGS_LOADED_HIGH | LOADFLAGS_CAN_USE_HEAP;

    let mut kernel_cmdline = Cmdline::new(layout::CMDLINE_MAX_SIZE)
        .map_err(|e| BootParamsError::InvalidCmdline(format!("{e:?}")))?;
    kernel_cmdline
        .insert_str(cmdline)
        .map_err(|e| BootParamsError::InvalidCmdline(format!("{e:?}")))?;
    load_cmdline(guest_mem, GuestAddress(layout::CMDLINE_ADDR), &kernel_cmdline)
        .map_err(BootParamsError::CmdlineWrite)?;
    hdr.cmd_line_ptr = layout::CMDLINE_ADDR as u32;
    hdr.cmdline_size = cmdline.len() as u32 + 1;

    write_rng_seed_setup_data(guest_mem, rng_seed).map_err(BootParamsError::RngSeedWrite)?;
    hdr.setup_data = layout::RNG_SEED_SETUP_DATA_ADDR;

    if let Some(initramfs) = initramfs {
        write_initramfs(guest_mem, initramfs)
            .map_err(|e| BootParamsError::InitramfsWrite(initramfs.len(), e))?;
        hdr.ramdisk_image = layout::INITRAMFS_ADDR as u32;
        hdr.ramdisk_size = initramfs.len() as u32;
        // Highest guest-physical address the kernel may place/relocate the initrd at — left at 0
        // this reads to some kernels as "no initrd allowed above address 0" rather than "no
        // limit," so it must be set explicitly whenever a ramdisk is provided (Firecracker's
        // `x86_64::initrd_load_addr` makes the same call for the same reason).
        hdr.initrd_addr_max = (ram_size as u64).min(u32::MAX as u64) as u32 - 1;
    }

    let mut params = boot_params { hdr, ..zeroed_boot_params() };
    write_e820_map(&mut params, ram_size);

    let boot_params_wrapper = LoaderBootParams::new::<boot_params>(&params, GuestAddress(layout::ZERO_PAGE_ADDR));
    LinuxBootConfigurator::write_bootparams::<M>(&boot_params_wrapper, guest_mem)
        .map_err(BootParamsError::ZeroPageWrite)?;

    Ok(loader_result)
}

/// `boot_params` has no `Default` impl (its `screen_info`/`apm_bios_info`/etc. sub-structs are
/// plain C layouts) but every field baud does not explicitly set must still be zero — a
/// non-zeroed zero page is itself a determinism leak (uninitialized-adjacent host stack garbage
/// handed to the guest). `Default` is derived for those sub-structs; this just zero-initializes
/// the aggregate the same way the crate's own tests do (`unsafe { mem::zeroed() }` is exactly what
/// `setup_header`'s hand-rolled `Default` impl already does — see linux-loader's bootparam.rs).
fn zeroed_boot_params() -> boot_params {
    // SAFETY: `boot_params` is a `#[repr(C, packed)]` aggregate of plain-old-data integer/array
    // fields (Linux's `struct boot_params`, `arch/x86/include/uapi/asm/bootparam.h`) — the
    // all-zero bit pattern is a valid value for every field, matching the crate's own
    // `setup_header::default()` (`src/loader_gen/x86_64/bootparam.rs`).
    unsafe { std::mem::zeroed() }
}

/// A minimal, honest e820 map: page zero is `reserved` (the conventional real-mode IVT/BDA
/// carve-out, [`layout::LOW_MEM_RAM_START`]'s doc), [`layout::LOW_MEM_RAM_START`] to
/// [`layout::HIMEM_START`] is `usable` (real-hardware finding: Linux's `reserve_real_mode()`
/// unconditionally needs sub-1MiB memory, panicking otherwise — that constant's doc has the full
/// story), and everything from [`layout::HIMEM_START`] to `ram_size` is `usable` RAM. No holes, no
/// MMIO windows beyond that — baud's "subtractive rule" machine has none (specs/baud-multiverse.md
/// §3.6).
fn write_e820_map(params: &mut boot_params, ram_size: usize) {
    let entries = [
        boot_e820_entry { addr: 0, size: layout::LOW_MEM_RAM_START, r#type: E820_RESERVED },
        boot_e820_entry {
            addr: layout::LOW_MEM_RAM_START,
            size: layout::HIMEM_START - layout::LOW_MEM_RAM_START,
            r#type: E820_RAM,
        },
        boot_e820_entry {
            addr: layout::HIMEM_START,
            size: (ram_size as u64).saturating_sub(layout::HIMEM_START),
            r#type: E820_RAM,
        },
    ];
    params.e820_entries = entries.len() as u8;
    params.e820_table[..entries.len()].copy_from_slice(&entries);
}

/// The exact deterministic command line todo.md §4.2 specifies for the minimal builtin-kernel
/// guest boot pipeline: single vCPU, TSC-only time, no probing of hardware baud does not model,
/// immediate deterministic exit (`reboot=t panic=-1`), and both `random.trust_*` flags pointed at
/// the boot-seed path [`write_rng_seed_setup_data`] pins (specs/baud-multiverse.md §3.8). A pure
/// constant, not derived from any host- or run-specific value — every boot of every minimal-kernel
/// guest passes exactly this string (or, for the full-distro guest of todo.md §4.7, its own
/// distro-specific line documented there instead).
pub const DETERMINISTIC_CMDLINE: &str = "console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc \
    tsc=reliable no-kvmclock no_timer_check pci=off acpi=off reboot=t panic=-1 quiet loglevel=1 \
    printk.time=0 random.trust_cpu=off random.trust_bootloader=on i8042.noaux i8042.nomux \
    i8042.nopnp 8250.nr_uarts=1 nomodule rdinit=/init";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e820_map_reserves_page_zero_and_marks_the_rest_usable() {
        let mut params = boot_params { hdr: Default::default(), ..zeroed_boot_params() };
        write_e820_map(&mut params, layout::GUEST_RAM_SIZE);
        assert_eq!(params.e820_entries, 3);
        // `boot_e820_entry` is `#[repr(C, packed)]`, so even a copied-out local of that struct
        // type keeps its fields unaligned — `assert_eq!` takes references internally, which would
        // be undefined behavior on a packed field (E0793). Copy each scalar field out by value
        // first (a copy, not a reference, is always sound on a packed field).
        let (reserved_type, reserved_addr, reserved_size) = {
            let e = params.e820_table[0];
            (e.r#type, e.addr, e.size)
        };
        let (low_ram_type, low_ram_addr, low_ram_size) = {
            let e = params.e820_table[1];
            (e.r#type, e.addr, e.size)
        };
        let (ram_type, ram_addr, ram_size) = {
            let e = params.e820_table[2];
            (e.r#type, e.addr, e.size)
        };
        assert_eq!(reserved_type, E820_RESERVED);
        assert_eq!(reserved_addr, 0);
        assert_eq!(reserved_size, layout::LOW_MEM_RAM_START);
        assert_eq!(low_ram_type, E820_RAM);
        assert_eq!(low_ram_addr, layout::LOW_MEM_RAM_START);
        assert_eq!(low_ram_addr + low_ram_size, layout::HIMEM_START);
        assert_eq!(ram_type, E820_RAM);
        assert_eq!(ram_addr, layout::HIMEM_START);
        assert_eq!(ram_addr + ram_size, layout::GUEST_RAM_SIZE as u64);
    }

    fn test_guest_mem() -> super::super::GuestMemory {
        super::super::GuestMemory::from_ranges(&[(GuestAddress(0), layout::GUEST_RAM_SIZE)])
            .expect("anonymous-mmap guest memory for a unit test")
    }

    #[test]
    fn rng_seed_setup_data_node_matches_the_linux_setup_data_layout() {
        let guest_mem = test_guest_mem();
        let seed = [0x42u8; RNG_SEED_LEN];
        write_rng_seed_setup_data(&guest_mem, &seed).expect("write must succeed");

        let mut header = [0u8; 16];
        guest_mem
            .read_slice(&mut header, GuestAddress(layout::RNG_SEED_SETUP_DATA_ADDR))
            .expect("read back the setup_data header");
        assert_eq!(&header[0..8], &0u64.to_le_bytes(), "next must terminate the list (0)");
        assert_eq!(&header[8..12], &SETUP_RNG_SEED.to_le_bytes(), "type must be SETUP_RNG_SEED (9)");
        assert_eq!(&header[12..16], &(RNG_SEED_LEN as u32).to_le_bytes(), "len must be RNG_SEED_LEN");

        let mut data = [0u8; RNG_SEED_LEN];
        guest_mem
            .read_slice(&mut data, GuestAddress(layout::RNG_SEED_SETUP_DATA_ADDR + 16))
            .expect("read back the seed bytes");
        assert_eq!(data, seed, "the seed bytes themselves must follow the header untouched");
    }

    #[test]
    fn load_kernel_and_write_boot_params_points_hdr_setup_data_at_the_rng_seed_node() {
        let guest_mem = test_guest_mem();
        let kernel_path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/hello-guest/bzImage"
        ));
        let seed = [0x7Au8; RNG_SEED_LEN];
        let result = load_kernel_and_write_boot_params(
            &guest_mem,
            kernel_path,
            "console=ttyS0",
            layout::GUEST_RAM_SIZE,
            &seed,
            None,
        )
        .expect("hello-guest is a valid bzImage fixture already used elsewhere in this crate");
        let _ = result;

        let mut zero_page = vec![0u8; std::mem::size_of::<boot_params>()];
        guest_mem
            .read_slice(&mut zero_page, GuestAddress(layout::ZERO_PAGE_ADDR))
            .expect("read back the zero page");
        // `setup_header.setup_data` sits at a fixed byte offset inside `boot_params`; rather than
        // hardcode that offset, reconstruct it the same way the real reader (the guest kernel via
        // `RSI`) would: reinterpret the raw bytes as `boot_params` and read the field.
        let reread: boot_params = unsafe { std::ptr::read_unaligned(zero_page.as_ptr() as *const boot_params) };
        let setup_data = reread.hdr.setup_data;
        assert_eq!(setup_data, layout::RNG_SEED_SETUP_DATA_ADDR);

        let mut seed_back = [0u8; RNG_SEED_LEN];
        guest_mem
            .read_slice(&mut seed_back, GuestAddress(layout::RNG_SEED_SETUP_DATA_ADDR + 16))
            .expect("read back the seed the boot flow wrote");
        assert_eq!(seed_back, seed);
    }

    #[test]
    fn load_kernel_and_write_boot_params_with_no_initramfs_leaves_ramdisk_fields_zero() {
        let guest_mem = test_guest_mem();
        let kernel_path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/hello-guest/bzImage"
        ));
        let seed = [0x11u8; RNG_SEED_LEN];
        load_kernel_and_write_boot_params(&guest_mem, kernel_path, "console=ttyS0", layout::GUEST_RAM_SIZE, &seed, None)
            .expect("hello-guest is a valid bzImage fixture already used elsewhere in this crate");

        let mut zero_page = vec![0u8; std::mem::size_of::<boot_params>()];
        guest_mem.read_slice(&mut zero_page, GuestAddress(layout::ZERO_PAGE_ADDR)).expect("read back the zero page");
        let reread: boot_params = unsafe { std::ptr::read_unaligned(zero_page.as_ptr() as *const boot_params) };
        let (ramdisk_image, ramdisk_size, loadflags) =
            (reread.hdr.ramdisk_image, reread.hdr.ramdisk_size, reread.hdr.loadflags);
        assert_eq!(ramdisk_image, 0, "no initramfs was passed, so ramdisk_image must stay 0");
        assert_eq!(ramdisk_size, 0, "no initramfs was passed, so ramdisk_size must stay 0");
        assert_eq!(
            loadflags & (LOADFLAGS_LOADED_HIGH | LOADFLAGS_CAN_USE_HEAP),
            LOADFLAGS_LOADED_HIGH | LOADFLAGS_CAN_USE_HEAP,
            "LOADED_HIGH/CAN_USE_HEAP are protocol-fidelity bits set unconditionally, not just when \
             an initramfs is present"
        );
    }

    #[test]
    fn load_kernel_and_write_boot_params_points_hdr_ramdisk_fields_at_the_written_initramfs() {
        let guest_mem = test_guest_mem();
        let kernel_path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/hello-guest/bzImage"
        ));
        let seed = [0x22u8; RNG_SEED_LEN];
        let initramfs: Vec<u8> = (0..4096u32).map(|i| (i % 256) as u8).collect();
        load_kernel_and_write_boot_params(
            &guest_mem,
            kernel_path,
            "console=ttyS0",
            layout::GUEST_RAM_SIZE,
            &seed,
            Some(&initramfs),
        )
        .expect("hello-guest is a valid bzImage fixture already used elsewhere in this crate");

        let mut zero_page = vec![0u8; std::mem::size_of::<boot_params>()];
        guest_mem.read_slice(&mut zero_page, GuestAddress(layout::ZERO_PAGE_ADDR)).expect("read back the zero page");
        let reread: boot_params = unsafe { std::ptr::read_unaligned(zero_page.as_ptr() as *const boot_params) };
        // `boot_params` (and its nested `hdr: setup_header`) is `#[repr(C, packed)]`, so even a
        // nested field chain must be copied to a plain local before `assert_eq!` can take a
        // reference to it (E0793) — same pattern `write_e820_map`'s own test already uses.
        let (ramdisk_image, ramdisk_size, initrd_addr_max) =
            (reread.hdr.ramdisk_image, reread.hdr.ramdisk_size, reread.hdr.initrd_addr_max);
        assert_eq!(ramdisk_image, layout::INITRAMFS_ADDR as u32);
        assert_eq!(ramdisk_size, initramfs.len() as u32);
        assert_eq!(
            initrd_addr_max,
            layout::GUEST_RAM_SIZE as u32 - 1,
            "initrd_addr_max must be set explicitly (left at 0 it reads as \"no placement allowed\" \
             to some kernels, not \"unlimited\")"
        );

        let mut initramfs_back = vec![0u8; initramfs.len()];
        guest_mem
            .read_slice(&mut initramfs_back, GuestAddress(layout::INITRAMFS_ADDR))
            .expect("read back the initramfs bytes the boot flow wrote");
        assert_eq!(initramfs_back, initramfs, "the initramfs bytes must land verbatim, no re-framing");
    }

    #[test]
    fn deterministic_cmdline_matches_the_spec_exactly() {
        assert_eq!(
            DETERMINISTIC_CMDLINE,
            "console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc tsc=reliable no-kvmclock \
             no_timer_check pci=off acpi=off reboot=t panic=-1 quiet loglevel=1 printk.time=0 \
             random.trust_cpu=off random.trust_bootloader=on i8042.noaux i8042.nomux i8042.nopnp \
             8250.nr_uarts=1 nomodule rdinit=/init"
        );
        // Spot-check the flags most load-bearing for determinism individually, so a future edit
        // that accidentally drops one of these fails here with a specific name rather than only as
        // an opaque whole-string diff.
        let tokens: Vec<&str> = DETERMINISTIC_CMDLINE.split_whitespace().collect();
        for required in [
            "nokaslr",
            "maxcpus=1",
            "clocksource=tsc",
            "tsc=reliable",
            "reboot=t",
            "panic=-1",
            "random.trust_cpu=off",
            "random.trust_bootloader=on",
            "rdinit=/init",
        ] {
            assert!(tokens.contains(&required), "DETERMINISTIC_CMDLINE must include {required:?}");
        }
        assert!(
            DETERMINISTIC_CMDLINE.len() < layout::CMDLINE_MAX_SIZE,
            "the cmdline must fit the fixed guest-memory region Cmdline::new is sized for"
        );
    }

    #[test]
    fn zeroed_boot_params_is_actually_all_zero_bytes() {
        let params = zeroed_boot_params();
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                (&params as *const boot_params) as *const u8,
                std::mem::size_of::<boot_params>(),
            )
        };
        assert!(bytes.iter().all(|&b| b == 0));
    }
}
