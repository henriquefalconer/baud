// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real KVM/VT-x boot flow (specs/baud-multiverse.md §2, todo.md §3.1): `Kvm::new` →
// `create_vm` → register one zeroed guest-RAM region at a fixed guest-physical address →
// `create_vcpu` → set CPUID/sregs/regs → load the guest kernel with `linux-loader` and write boot
// params at fixed addresses → enter the run loop (`baud_vcpu::linux::run_until_halted`).
//
// Like `crates/baud-host/src/linux.rs` and `crates/baud-vcpu/src/linux/`, this module is written
// against the real `kvm-ioctls`/`kvm-bindings`/`linux-loader`/`vm-memory` crate sources and is now
// also exercised for real: `tests::double_boot_memory_identical` boots `tests/fixtures/hello-
// guest/bzImage` (see that directory's `BUILD.md`) against actual `/dev/kvm` on this project's
// dev machine (a bare-metal Dell XPS 13 running Ubuntu on WSL2 with VT-x, CLAUDE.md) — the first
// real KVM boot in this project's history, which caught and fixed two real bugs neither `cargo
// check` nor any unit test without real hardware could have (`configure_msr_filter`'s MSR-filter
// flags/bitmap semantics and `pagetables::long_mode_sregs`'s invalid TR segment, both documented
// at their fix sites and in that fixture's `BUILD.md`). It is additive: nothing in `baud-server`/
// `baud-tape-agent` calls into this module yet (see the pivot notice at the top of `lib.rs`).

pub mod bootparams;
pub mod pagetables;

use crate::console::DeviceBus;
use crate::cpuid::{self, CpuidEntry};
use crate::layout;
use crate::timesource::{BranchCounter, WorkClock, MSR_IA32_TSC, MSR_IA32_TSC_DEADLINE, MSR_IA32_TSC_AUX};
use crate::virtio_mmio::VirtioMmioTransport;
use baud_snapshot::{PageRef, PageStore, Universe};
use baud_vcpu::{DeterminismHole, RunLoopError};
use kvm_bindings::{
    kvm_cpuid_entry2, kvm_enable_cap, kvm_msr_entry, kvm_userspace_memory_region, Msrs,
    KVM_MAX_CPUID_ENTRIES, KVM_MEM_LOG_DIRTY_PAGES,
};
use kvm_ioctls::{Cap, Kvm, MsrExitReason, MsrFilterDefaultAction, MsrFilterRange, MsrFilterRangeFlags, VcpuFd, VmFd};
use perf_event::{Builder, Counter};
use std::io;
use std::path::Path;
use std::time::Duration;
use tracing::info;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

/// The guest-RAM backend type this boot flow uses throughout — a single anonymous-mmap region, no
/// dirty-page tracking here (that is `baud-snapshot`'s `KVM_CAP_DIRTY_LOG_RING` job, §5).
pub type GuestMemory = GuestMemoryMmap<()>;

// The MSRs the cooperative regime's virtual TSC serves (specs/baud-multiverse.md §4's "MSR
// filter" row): `IA32_TSC` (0x10), `IA32_TSC_DEADLINE` (0x6E0), `IA32_TSC_AUX` (0xC0000103,
// AMD/Intel-shared RDTSCP auxiliary MSR) — the constants (`MSR_IA32_TSC` etc., imported above)
// live in `timesource` since that is what actually serves reads/writes once the MSR-filter exit
// routes to `dispatch_exit`. Deleted entirely: HPET/PIT/PM-timer/RTC have no MSR or PIO footprint
// on this minimal machine to begin with (specs/baud-multiverse.md §3.6).

/// The fixed virtual-TSC frequency every cooperative-regime run pins (todo.md §3.3: "cooperative =
/// `KVM_SET_TSC_KHZ` pins a fixed frequency"). 1 GHz — a round, host-independent number; the point
/// is that it is the *same* number on every host, not that it matches any particular host's native
/// rate. Derived from `cpuid::TSC_CRYSTAL_HZ` (not a separately-chosen number) so the CPUID leaf
/// 15H value the guest reads and the actual `KVM_SET_TSC_KHZ` frequency can never drift apart —
/// a guest that trusts CPUID (as Linux's `native_calibrate_tsc()` does, see that constant's doc)
/// computes exactly the frequency this VMM really programmed.
pub const VIRTUAL_TSC_KHZ: u32 = cpuid::TSC_CRYSTAL_HZ / 1000;

#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("KVM_CREATE_VM / KVM_GET_API_VERSION failed: {0}")]
    Kvm(#[from] kvm_ioctls::Error),
    #[error("failed to allocate/register {0} bytes of guest RAM: {1}")]
    GuestMemory(usize, String),
    #[error("vCPU configuration rejected: {0}")]
    VcpuCfg(#[from] baud_vcpu::VmCfgError),
    #[error("failed to write the identity page tables into guest memory: {0}")]
    PageTables(vm_memory::guest_memory::Error),
    #[error(transparent)]
    BootParams(#[from] bootparams::BootParamsError),
    #[error("failed to create the work-clock's perf_event branch counter: {0}")]
    BranchCounter(#[from] io::Error),
    #[error("failed to set up the dirty ring: {0}")]
    DirtyRing(#[from] baud_snapshot::linux::DirtyRingError),
    #[error("failed to allocate the MSR entry buffer for TSC pinning: {0}")]
    MsrAlloc(vmm_sys_util::fam::Error),
}

/// A fully booted-but-not-yet-run guest: KVM handles, guest memory, and the vCPU, all configured
/// per specs/baud-multiverse.md §2-§4 and ready for `baud_vcpu::linux::run_until_halted`. Kept
/// alive as one struct because dropping `kvm`/`vm` before `vcpu` would invalidate the fds `vcpu`'s
/// ioctls depend on.
pub struct BootedGuest {
    pub kvm: Kvm,
    pub vm: VmFd,
    pub vcpu: VcpuFd,
    pub guest_mem: GuestMemory,
}

/// The `Kvm::new → create_vm → [negotiate dirty ring] → register zeroed guest RAM → create_vcpu →
/// CPUID mask + MSR filter → [open dirty ring]` prefix shared by both ways a [`BootedGuest`] comes
/// into existence (specs/baud-multiverse.md §2): [`boot_guest`] continues it with a fresh kernel
/// image (page tables, boot params, entry-point regs); [`restore_guest`] continues it by walking a
/// captured [`Universe`]'s `restore_plan` instead (specs/baud-snapshot.md §6) — RAM/regs/sregs/etc.
/// all come from the universe rather than a freshly-loaded image, so this prefix is exactly the
/// part both paths need identically and nothing more.
///
/// `dirty_ring_entries`, when `Some`, negotiates `KVM_CAP_DIRTY_LOG_RING` on the VM *before*
/// `create_vcpu` and mmaps the resulting per-vCPU ring right after — real-hardware finding
/// (todo.md §14): the kernel returns `EINVAL` if that capability is negotiated after any vCPU
/// already exists, so this is the only correct place in this workspace to do it (see
/// `baud_snapshot::linux::DirtyRing`'s own doc for the exact mechanism).
fn create_vm_vcpu_shell(
    dirty_ring_entries: Option<u32>,
) -> Result<(BootedGuest, Option<baud_snapshot::linux::DirtyRing>), BootError> {
    baud_vcpu::validate_vcpu_count(1)?; // todo.md §1: exactly one vCPU per VM, checked first

    let kvm = Kvm::new()?;
    let vm = kvm.create_vm()?;

    if let Some(entries) = dirty_ring_entries {
        baud_snapshot::linux::DirtyRing::negotiate_capability(&vm, entries)?;
    }

    let guest_mem =
        allocate_and_register_guest_ram(&vm, layout::GUEST_RAM_SIZE, dirty_ring_entries.is_some())?;

    let vcpu = vm.create_vcpu(0)?;
    apply_cpuid_mask(&kvm, &vcpu)?;
    configure_msr_filter(&vm)?;

    let dirty_ring = dirty_ring_entries
        .map(|entries| baud_snapshot::linux::DirtyRing::open(&vcpu, entries))
        .transpose()?;

    Ok((BootedGuest { kvm, vm, vcpu, guest_mem }, dirty_ring))
}

/// Pin the vCPU's *raw* TSC value (what a guest's own `rdtsc`/`rdtscp` instructions read,
/// distinct from the `IA32_TSC` MSR-filter trap `configure_msr_filter` routes to `WorkClock` —
/// todo.md §3.3: "cooperative = `KVM_SET_TSC_KHZ` pins a fixed frequency + `KVM_VCPU_TSC_OFFSET`
/// sets the offset"). `KVM_SET_MSRS(IA32_TSC=value)` is KVM's own documented mechanism for setting
/// that offset directly (it does not round-trip through `KVM_X86_SET_MSR_FILTER`'s exit-to-
/// userspace path at all — the filter only gates *guest-instruction-triggered* RDMSR/WRMSR, never
/// this ioctl; `baud-snapshot::linux::restore`'s `SetVcpuMsrs` step already relies on the same
/// fact to restore a captured TSC value onto a vCPU with an identical filter active).
///
/// Without this call a fresh boot's raw `rdtsc` reads whatever KVM's default offset leaves in
/// place — implicitly anchored to the *host's* wall-clock TSC at vCPU-creation time, so two
/// separate boots diverge by however much host wall-clock elapsed between them, not just by
/// scheduling jitter. Called last in [`boot_guest`], immediately before returning to the caller
/// (who enters `KVM_RUN` right after) rather than right after [`VcpuFd::set_tsc_khz`] — real-
/// hardware finding, todo.md §14: pinning that early left the page-table writes and kernel-image
/// load (both I/O-bound, run-to-run-variable) between the pin and the guest's first `rdtsc`, which
/// dominated the jitter two boots disagreed by (tens of millions of virtual-TSC counts observed,
/// i.e. tens of milliseconds at `VIRTUAL_TSC_KHZ` == 1 GHz) — pinning last leaves only genuine
/// host-scheduling jitter in the microseconds between this call and vCPU entry, small enough that
/// the *high* bits of a `VIRTUAL_TSC_KHZ`-scaled read stay identical across boots (todo.md §3.3's
/// test spec: cooperative asserts the high bits / work-derived field, not full equality).
fn pin_tsc_value(vcpu: &VcpuFd, value: u64) -> Result<(), BootError> {
    let entry = kvm_msr_entry { index: MSR_IA32_TSC, data: value, ..Default::default() };
    let msrs = Msrs::from_entries(&[entry]).map_err(BootError::MsrAlloc)?;
    vcpu.set_msrs(&msrs)?;
    Ok(())
}

/// `IA32_APIC_BASE` (Intel SDM Vol. 3A §10.4.4) — a real bug this crate's ACPI/LAPIC work
/// (`crate::lapic`, todo.md §14 item 5(c)) found on real hardware: KVM has no in-kernel LAPIC
/// device at all here (`KVM_CREATE_IRQCHIP` is never called, `lapic_in_kernel(vcpu)` is false), so
/// this MSR is never routed through baud's own `KVM_X86_SET_MSR_FILTER` trap (unlike the
/// `IA32_TSC*` family, §3.3) — it is answered by KVM's own bare bookkeeping for the register, which
/// this project's real boot found does **not** default to a sane xAPIC-enabled, x2APIC-disabled
/// reset state: a real guest's own `rdmsr(IA32_APIC_BASE)` read back with the x2APIC-enable bit
/// (bit 10) already set, even though this crate's CPUID mask clears the x2APIC *feature* bit
/// (`cpuid.rs`'s `ECX_X2APIC_BIT`) — `check_x2apic()`'s `CONFIG_X86_X2APIC`-unset fallback path
/// (`arch/x86/kernel/apic/apic.c`) trusts this MSR bit directly, independent of CPUID, and
/// unconditionally clears `X86_FEATURE_APIC` the moment it reads set, which made every real Linux
/// guest conclude "No local APIC present" regardless of a correct MADT (`crate::acpi::build_madt`)
/// or a correct `crate::lapic::LocalApic` MMIO stub — neither is ever consulted once the kernel
/// has already given up on this bit. Pinning this MSR explicitly (BSP bit set, x2APIC bit clear,
/// APIC Global Enable set, base address [`layout::LAPIC_MMIO_BASE`] — the real-hardware reset
/// values every guest already assumes) makes the guest's own APIC detection deterministic and
/// correct, exactly the same "don't trust whatever KVM's default happens to leave in place"
/// reasoning `pin_tsc_value` above already established for the TSC.
fn pin_apic_base_msr(vcpu: &VcpuFd) -> Result<(), BootError> {
    const MSR_IA32_APICBASE: u32 = 0x0000_001B;
    const APICBASE_BSP: u64 = 1 << 8;
    const APICBASE_ENABLE: u64 = 1 << 11;
    let value = layout::LAPIC_MMIO_BASE | APICBASE_BSP | APICBASE_ENABLE;
    let entry = kvm_msr_entry { index: MSR_IA32_APICBASE, data: value, ..Default::default() };
    let msrs = Msrs::from_entries(&[entry]).map_err(BootError::MsrAlloc)?;
    vcpu.set_msrs(&msrs)?;
    Ok(())
}

/// Run the full boot flow (specs/baud-multiverse.md §2's `Kvm::new → create_vm → register guest
/// RAM → create_vcpu → CPUID/TSC/MSR setup → linux-loader boot`) and return a [`BootedGuest`]
/// positioned at the kernel's 64-bit entry point, ready to enter `KVM_RUN`, alongside a
/// [`baud_snapshot::linux::DirtyRing`] if `dirty_ring_entries` was `Some` (see
/// [`create_vm_vcpu_shell`]'s doc for why negotiation must happen this early).
pub fn boot_guest(
    kernel_path: &Path,
    cmdline: &str,
    tape: &[u8],
    dirty_ring_entries: Option<u32>,
    initramfs: Option<&[u8]>,
) -> Result<(BootedGuest, Option<baud_snapshot::linux::DirtyRing>), BootError> {
    let (guest, dirty_ring) = create_vm_vcpu_shell(dirty_ring_entries)?;
    guest.vcpu.set_tsc_khz(VIRTUAL_TSC_KHZ)?;

    pagetables::write_identity_page_tables(&guest.guest_mem, layout::GUEST_RAM_SIZE)
        .map_err(BootError::PageTables)?;
    pagetables::write_gdt(&guest.guest_mem).map_err(BootError::PageTables)?;
    guest.vcpu.set_sregs(&pagetables::long_mode_sregs())?;

    // Must come before `LinuxBranchCounter::new()` — see `rng_seed_from_tape`'s doc.
    let rng_seed = rng_seed_from_tape(tape);
    let loader_result = bootparams::load_kernel_and_write_boot_params(
        &guest.guest_mem,
        kernel_path,
        cmdline,
        layout::GUEST_RAM_SIZE,
        &rng_seed,
        initramfs,
    )?;

    let mut regs = guest.vcpu.get_regs()?;
    regs.rip = loader_result.kernel_load.raw_value() + layout::KERNEL_64BIT_ENTRY_OFFSET;
    regs.rsi = layout::ZERO_PAGE_ADDR; // Linux/x86 64-bit entry contract: RSI = &boot_params
    regs.rsp = layout::BOOT_STACK_POINTER;
    regs.rflags = 0x2; // bit 1 is reserved-must-be-1; every other flag starts clear
    guest.vcpu.set_regs(&regs)?;
    pin_apic_base_msr(&guest.vcpu)?;

    // Pinned last, immediately before returning to the caller (which enters `KVM_RUN` right
    // after) rather than right after `set_tsc_khz` above — real-hardware finding, todo.md §14:
    // pinning that early left the (I/O-bound, run-to-run-variable) page-table writes and kernel
    // image load between the pin and the guest's first `rdtsc`, which dominated the jitter
    // `rdtsc_guest_reproduces_high_bits_across_boots` observed (tens of millions of virtual-TSC
    // counts, i.e. tens of milliseconds at `VIRTUAL_TSC_KHZ` == 1 GHz) far more than genuine
    // host-scheduling jitter in the microseconds between this point and vCPU entry.
    pin_tsc_value(&guest.vcpu, 0)?;

    Ok((guest, dirty_ring))
}

/// Errors from reconstructing a [`BootedGuest`] out of a captured [`Universe`]
/// (`baud-snapshot::linux::restore`, specs/baud-snapshot.md §6) instead of a fresh kernel image —
/// [`RestoreError::Shell`] covers the same `Kvm::new`/`create_vm`/`create_vcpu`/CPUID/MSR-filter
/// prefix [`BootError`] already names (shared with [`boot_guest`] via
/// [`create_vm_vcpu_shell`]), plus branch-counter creation for the restored work-clock;
/// [`RestoreError::Snapshot`] is `baud-snapshot::linux::restore`'s own error (CPU-model mismatch or
/// a `KVM_SET_*` ioctl failure while walking `restore_plan`).
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error(transparent)]
    Shell(#[from] BootError),
    #[error(transparent)]
    Snapshot(#[from] baud_snapshot::linux::RestoreError),
}

/// Errors from [`Multiverse::reset_dirty_pages`] (specs/baud-snapshot.md §5's "reset" guarantee
/// wired onto a live `Multiverse`).
#[derive(Debug, thiserror::Error)]
pub enum ResetError {
    #[error("reset_dirty_pages called on a Multiverse booted/restored without dirty_ring_entries")]
    NotEnabled,
    #[error(
        "dirty ring reported RAM page {0}, but the supplied base snapshot only has that many pages \
         (base_ram and this Multiverse's guest RAM layout have diverged)"
    )]
    PageOutOfRange(usize),
    #[error("failed to write guest RAM at page {0}: {1}")]
    GuestMemory(usize, vm_memory::guest_memory::Error),
    #[error(transparent)]
    DirtyRing(#[from] baud_snapshot::linux::DirtyRingError),
}

/// Reconstruct a [`BootedGuest`] from a captured [`Universe`] instead of loading a kernel image:
/// the shell (`Kvm`/`VmFd`/`VcpuFd`/zeroed guest RAM, CPUID-masked, MSR-filtered — identical to
/// what [`boot_guest`] sets up before it ever touches a kernel image) is created first, then
/// `baud_snapshot::linux::restore` walks the universe's `restore_plan` onto it in order
/// (specs/baud-snapshot.md §6): TSC frequency, then RAM, then every vCPU-state field, then the VM
/// clock — refusing up front if `universe.cpu_signature` does not match this host's (unless
/// `template_active`). Device/console state is deliberately left to the caller by
/// `baud-snapshot::linux::restore` (see `RestoreStep::RestoreDevice`'s doc) — reassembling that
/// into a live [`Multiverse`] is [`Multiverse::restore`]'s job, one layer up.
pub fn restore_guest(
    universe: &Universe,
    template_active: bool,
    dirty_ring_entries: Option<u32>,
) -> Result<(BootedGuest, Option<baud_snapshot::linux::DirtyRing>), RestoreError> {
    let (guest, dirty_ring) = create_vm_vcpu_shell(dirty_ring_entries)?;
    baud_snapshot::linux::restore(
        &guest.kvm,
        &guest.vm,
        &guest.vcpu,
        &guest.guest_mem,
        layout::GUEST_RAM_START,
        universe,
        template_active,
    )?;
    Ok((guest, dirty_ring))
}

/// Register [`layout::GUEST_RAM_START`]..`+ram_size` as one zeroed, anonymous-mmap-backed memory
/// slot (specs/baud-multiverse.md §3's "Memory init: Zeroed RAM at fixed guest-physical
/// addresses" — `GuestMemoryMmap::from_ranges` anonymous-mmaps zeroed pages, and nothing in this
/// boot flow ever writes host data into guest RAM except the specific structures this module
/// builds).
fn allocate_and_register_guest_ram(
    vm: &VmFd,
    ram_size: usize,
    log_dirty_pages: bool,
) -> Result<GuestMemory, BootError> {
    let guest_mem = GuestMemory::from_ranges(&[(GuestAddress(layout::GUEST_RAM_START), ram_size)])
        .map_err(|e| BootError::GuestMemory(ram_size, e.to_string()))?;

    let host_addr = guest_mem
        .get_host_address(GuestAddress(layout::GUEST_RAM_START))
        .map_err(|e| BootError::GuestMemory(ram_size, e.to_string()))?;
    // Real-hardware finding (todo.md §14, H5 reset_cost_scales_with_write_set): KVM only tracks
    // dirty pages — via the bitmap *or* the KVM_CAP_DIRTY_LOG_RING ring — for memory slots
    // registered with KVM_MEM_LOG_DIRTY_PAGES; a dirty ring opened over a slot registered with
    // flags=0 (this function's prior unconditional behavior) silently reports zero dirtied pages
    // forever, no matter how much the guest actually writes. Only set when a caller actually wants
    // dirty tracking (`dirty_ring_entries.is_some()`, `create_vm_vcpu_shell`'s caller) — the flag
    // has a real cost (write-protecting the slot) callers that never reset shouldn't pay.
    let flags = if log_dirty_pages { KVM_MEM_LOG_DIRTY_PAGES } else { 0 };
    let region = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: layout::GUEST_RAM_START,
        memory_size: ram_size as u64,
        userspace_addr: host_addr as u64,
        flags,
    };
    // SAFETY: `host_addr` came from `guest_mem` itself (the region this same call registers as
    // the backing for `guest_phys_addr`), sized exactly `ram_size`, and `guest_mem` outlives the
    // `VmFd` it is registered with (both live in the caller's `BootedGuest` until dropped
    // together).
    unsafe { vm.set_user_memory_region(region) }?;
    Ok(guest_mem)
}

impl CpuidEntry for kvm_cpuid_entry2 {
    fn function(&self) -> u32 {
        self.function
    }
    fn index(&self) -> u32 {
        self.index
    }
    fn eax(&self) -> u32 {
        self.eax
    }
    fn set_eax(&mut self, v: u32) {
        self.eax = v;
    }
    fn ebx(&self) -> u32 {
        self.ebx
    }
    fn set_ebx(&mut self, v: u32) {
        self.ebx = v;
    }
    fn ecx(&self) -> u32 {
        self.ecx
    }
    fn set_ecx(&mut self, v: u32) {
        self.ecx = v;
    }
    fn edx(&self) -> u32 {
        self.edx
    }
    fn set_edx(&mut self, v: u32) {
        self.edx = v;
    }
}

/// Start from `KVM_GET_SUPPORTED_CPUID`, apply the exact same [`cpuid::apply_determinism_mask`]
/// `cpuid_leaves_are_fixed` unit-tests in isolation, then serve it via `KVM_SET_CPUID2`
/// (specs/baud-multiverse.md §4).
fn apply_cpuid_mask(kvm: &Kvm, vcpu: &VcpuFd) -> Result<(), kvm_ioctls::Error> {
    let mut supported = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    cpuid::apply_determinism_mask(supported.as_mut_slice());
    vcpu.set_cpuid2(&supported)
}

/// Route `IA32_TSC`/`IA32_TSC_DEADLINE`/`IA32_TSC_AUX` reads and writes to the VMM
/// (`X86Rdmsr`/`X86Wrmsr` exits, served by the work-clock in `dispatch_exit`) instead of letting
/// KVM handle them natively (specs/baud-multiverse.md §4's MSR-filter row). Every other MSR keeps
/// KVM's normal (non-exiting) handling — `MsrFilterDefaultAction::ALLOW` — this filter only *un*-
/// allows the three TSC MSRs, which is what turns their access into a `KVM_MSR_EXIT_REASON_FILTER`
/// exit once [`Cap::X86UserSpaceMsr`] is enabled for that reason.
fn configure_msr_filter(vm: &VmFd) -> Result<(), kvm_ioctls::Error> {
    let cap = kvm_enable_cap {
        cap: Cap::X86UserSpaceMsr as u32,
        args: [MsrExitReason::Filter.bits() as u64, 0, 0, 0],
        ..Default::default()
    };
    vm.enable_cap(&cap)?;

    // Documentation/virt/kvm/api.rst S4.97 (`KVM_X86_SET_MSR_FILTER`): a range's `flags` selects
    // *which* access types (READ and/or WRITE) that range's bitmap governs -- the kernel rejects
    // `flags == 0` outright (arch/x86/kvm/x86.c's `kvm_add_msr_filter`: "if (!user_range->flags)
    // return -EINVAL") since a range covering neither access type is meaningless, not "cover
    // nothing so it always exits". Each bitmap bit then means "a 1 allows the operation in
    // flags, 0 denies" (same doc) -- allow routes the access through KVM's normal in-kernel
    // handling; deny (with `Cap::X86UserSpaceMsr`'s `Filter` exit reason enabled above) is what
    // actually turns the access into a `KVM_EXIT_X86_RDMSR`/`X86Wrmsr` exit to userspace instead
    // of KVM injecting a #GP. So trapping these three MSRs to the VMM needs both bits of `flags`
    // set (govern both reads and writes) *and* the bitmap bit cleared (deny, not allow) --
    // the exact opposite of an earlier version of this function, which set an empty `flags` (an
    // unconditional `-EINVAL`, never exercised until real KVM hardware existed to run it against)
    // and an allow bit (which, even past the flags bug, would have let TSC reads/writes proceed
    // silently in-kernel instead of reaching `dispatch_exit`'s work-clock).
    let denied_single_msr = [0b0000_0000u8];
    let trapped_msrs = [MSR_IA32_TSC, MSR_IA32_TSC_DEADLINE, MSR_IA32_TSC_AUX];
    let ranges: Vec<MsrFilterRange<'_>> = trapped_msrs
        .iter()
        .map(|&base| MsrFilterRange {
            flags: MsrFilterRangeFlags::READ | MsrFilterRangeFlags::WRITE,
            base,
            msr_count: 1,
            bitmap: &denied_single_msr,
        })
        .collect();
    vm.set_msr_filter(MsrFilterDefaultAction::ALLOW, &ranges)
}

/// `PERF_TYPE_RAW` (perf_event_open(2)); there is no portable `Event` variant for a raw
/// `type`+`config` pair in the `perf-event` 0.4 crate, so this is set directly via
/// [`Builder::attrs_mut`] rather than `Builder::kind`.
const PERF_TYPE_RAW: u32 = 4;

/// Intel `BR_INST_RETIRED.COND` (event `0xC4`, umask `0x11`) — the Skylake..Ice-Lake encoding,
/// confirmed on this project's own Tiger Lake dev host by `tools/pmucheck.c` and recorded in
/// `docs/determinism.md`. **Not** `PERF_COUNT_HW_BRANCH_INSTRUCTIONS` (all branches): that generic
/// event was measured `±1` across identical trials on this exact host (`docs/determinism.md`'s own
/// table — `20000009`/`20000007`/`20000008`/… vs. the raw event's bit-exact `20000002`/`20000002`/
/// `20000002`) and specs §3.3 forbids it by name for exactly this reason. `LinuxBranchCounter` and
/// `crates/baud-host/src/linux.rs`'s `measure_fixed_loop_branches` had both drifted onto the
/// generic event despite that documented decision — the root cause (found via `tools/pmucheck.c`,
/// re-run live) of the residual single-fd RCB jitter that made `os_entropy_is_deterministic` and
/// `double_boot_ram_hash_identical` intermittently disagree by 1-2 counts even after the two-fd
/// epoch-disagreement bug (todo.md §14 next-actions item 2(c)) was already fixed.
const BR_INST_RETIRED_COND: u64 = 0x11c4;

/// The work-clock's real RCB source: a free-running `perf_event_open` counter over the raw
/// `BR_INST_RETIRED.COND` event (see [`BR_INST_RETIRED_COND`]), read on every `IA32_TSC` access
/// (specs/baud-multiverse.md §4's work-clock row) and, since todo.md §14 next-actions item 2(c)'s
/// counter-reconciliation fix, also the *only* RCB source `baud_vcpu::linux::pmu::LinuxPmuStepper`
/// polls when arming/stepping toward an interrupt-injection target (specs/baud-vcpu.md §5) — it no
/// longer owns a second, independently-epoched `perf_event` fd of its own for that (see
/// `crates/baud-vcpu/src/linux/pmu.rs`'s module doc).
pub struct LinuxBranchCounter {
    counter: Counter,
    /// The last successfully read value — served on a transient read failure instead of `0`, so a
    /// hiccup never makes the work-clock appear to run backwards (specs/baud-multiverse.md §3's
    /// nondeterminism table requires monotone time; mirrors `LinuxPmuStepper::current_point`'s same
    /// fallback-to-last-known-value rationale).
    last: u64,
}

impl LinuxBranchCounter {
    pub fn new() -> io::Result<Self> {
        let mut builder = Builder::new();
        builder.attrs_mut().type_ = PERF_TYPE_RAW;
        builder.attrs_mut().config = BR_INST_RETIRED_COND;
        // specs/baud-multiverse.md §3.3's "guest-filtered" requirement, and the real root cause of
        // the single-step landing-precision bug todo.md §14.1 filed against
        // `run_to_events`/`inject_at` ("can overshoot its target RCB", empirically 6-43 branches).
        //
        // **`exclude_kernel(false)` is load-bearing and must never be dropped.**
        // `perf_event::Builder::new()` silently defaults `exclude_kernel = 1` (see that crate's
        // `Builder::default`, which sets `exclude_kernel`/`exclude_hv` before this code ever gets
        // the builder). Every baud guest runs in 64-bit long mode at **CPL 0**, so a USR-only event
        // select filters out the guest's entire instruction stream: with the default left in place
        // this counter counted *zero* guest branches for the whole run and what it actually
        // measured was host **userspace** branches retired inside the bracketed `KVM_RUN` ioctl
        // window — i.e. a VM-exit counter scaled by ~54 counts/exit, not a work clock.
        //
        // Measured directly on this host, real /dev/kvm, against `timer-guest` (whose busy loop
        // retires a known 17 conditional branches per forced `out 0x80, al` exit): with the
        // defaults, every free-running `KVM_RUN` advanced this counter by exactly +54 and every
        // single-stepped instruction by exactly +44 — *including* instructions that are not
        // branches at all (`dec ecx`) — and rebuilding the same fixture with 4096 inner iterations
        // instead of 16 (256x the guest branches per exit) still produced exactly +54 per exit,
        // proving guest branches contributed nothing. That ~44-count quantum per single step is
        // precisely why the arm-early-then-single-step engine could never land on an exact
        // `target_rcb`: the clock it steers by had no resolution finer than one VM exit.
        //
        // `exclude_host(true)` (count only VMX non-root / guest mode) **does** work on this
        // nested-virtualized dev host. The previously-documented finding that it "reads back 0 for
        // the whole run" was a misdiagnosis of exactly the bug above: `exclude_host = 1` was being
        // set *on top of* the crate's default `exclude_kernel = 1`, so the pair asked for
        // "guest-mode CPL-3 branches only" — of which a ring-0 bare-metal payload retires none.
        // With `exclude_kernel` cleared, the same fixture reads exactly 17 branches per exit (4097
        // for the 4096-iteration rebuild): the guest's own architectural conditional-branch count,
        // bit-exact, with no host contamination at all.
        //
        // `exclude_hv` is left at the crate's default `1`; measured to make no difference here
        // either way, so it is not disturbed.
        //
        // The `resume_rcb`/`pause_rcb` bracketing (`run_and_convert_rcb_bracketed`,
        // `crates/baud-vcpu/src/linux/mod.rs`) is kept: it now costs 0 counts per pair (measured;
        // it was 11 before this fix, all of it host userspace inside the two perf ioctls), so it is
        // free, and it remains a second line of defence for any host where `exclude_host` really is
        // inoperative.
        builder.attrs_mut().set_exclude_kernel(0);
        builder.attrs_mut().set_exclude_host(1);
        // `pinned(true)`: same fix as `crates/baud-host/src/linux.rs`'s
        // `measure_fixed_loop_branches` (todo.md §14/H3) — keeps this counter resident on the PMU
        // instead of occasionally being multiplexed off mid-measurement under this project's own
        // nested-virtualized dev host, which otherwise undercounts by a small, run-varying amount.
        builder.pinned(true);
        let mut counter = builder.build()?;
        // Start paused (todo.md §14 next-actions item 2): the real `KVM_RUN` loop
        // (`run_and_convert_rcb_bracketed`, `crates/baud-vcpu/src/linux/mod.rs`) resumes this
        // counter for exactly the ioctl window and pauses it the instant that call returns, so it
        // never accumulates the userspace dispatch code between exits — see this struct's own doc
        // and `TimeSource::resume_rcb`'s doc for why. `disable()` on an already-disabled counter
        // (perf_event's own construction default) is a harmless no-op, so this is safe either way.
        counter.disable()?;
        Ok(LinuxBranchCounter { counter, last: 0 })
    }
}

impl BranchCounter for LinuxBranchCounter {
    fn read(&mut self) -> u64 {
        match self.counter.read() {
            Ok(v) => {
                self.last = v;
                v
            }
            Err(_) => self.last,
        }
    }

    fn pause(&mut self) {
        let _ = self.counter.disable();
    }

    fn resume(&mut self) {
        let _ = self.counter.enable();
    }
}

/// The wiring point specs/baud-multiverse.md §6's API targets for H1 ("boot a guest, print to the
/// serial console, clean Hlt/Shutdown") and H5 (snapshot/branch/restore, specs/baud-snapshot.md).
/// The tape device (`baud-tape-device`, H2/§3.5) is wired in (`DeviceBus::tape`,
/// `crate::tape_bus::TapeBus`) — [`boot`] takes the run's tape bytes directly rather than the
/// spec's `run(tape: impl TapeSource)` shape, since there is exactly one run per `Multiverse`
/// here (no re-run-with-a-different-tape use case yet; that is `baud-driver`'s job once it exists).
/// [`boot`] runs the full [`boot_guest`] boot flow plus the work-clock/console/tape device wiring,
/// and [`run_to_first_halt`] drives it to the guest's first `Hlt`/`Shutdown` and returns the
/// console output plus a blake3 hash of guest RAM — `boot(...).ram_hash_at_first_hlt()` from
/// specs/baud-multiverse.md §8's `double_boot_memory_identical` pseudocode, on the real boot flow
/// instead of that pseudocode's placeholder. [`snapshot`] and [`restore`] are the spec's `Snapshot
/// ::capture`/`Snapshot::restore` (specs/baud-snapshot.md §2's API) wired onto this struct's own
/// fields: `snapshot` hands every piece of state `baud_snapshot::linux::capture` needs (RAM/vCPU/
/// clock via the KVM handles, plus this crate's own work-clock anchor/tape cursor/console bytes
/// that `baud-snapshot` cannot see into); `restore` is the inverse, reconstructing a whole new
/// `Multiverse` from a captured [`Universe`] rather than a kernel image.
///
/// [`boot`]: Multiverse::boot
/// [`run_to_first_halt`]: Multiverse::run_to_first_halt
/// [`snapshot`]: Multiverse::snapshot
/// [`restore`]: Multiverse::restore
pub struct Multiverse {
    guest: BootedGuest,
    bus: DeviceBus,
    time: WorkClock<LinuxBranchCounter>,
    /// `Some` when [`boot`](Self::boot)/[`restore`](Self::restore) negotiated `KVM_CAP_DIRTY_LOG_
    /// RING` on this guest's vCPU (specs/baud-snapshot.md §5, via `dirty_ring_entries: Some(_)`) —
    /// `None` otherwise, since the ring is an opt-in cost (an extra mmap + capability negotiation +
    /// per-slot dirty-page write-protection) a caller that never rewinds this `Multiverse` should
    /// not pay. [`reset_dirty_pages`](Self::reset_dirty_pages) requires it to be `Some`.
    dirty_ring: Option<baud_snapshot::linux::DirtyRing>,
    /// The real wall-clock budget [`run_to_first_halt`](Self::run_to_first_halt)/
    /// [`run_to_first_halt_without_ram_hash`](Self::run_to_first_halt_without_ram_hash) give
    /// `baud_vcpu::linux::run_until_halted`'s watchdog (todo.md §14.1 "Still open" item 1) —
    /// initialized to [`DEFAULT_WATCHDOG_BUDGET`] by [`boot`](Self::boot)/[`restore`]
    /// (Self::restore) and overridable via [`set_watchdog_budget`](Self::set_watchdog_budget).
    /// Every other `run_to_first_halt_with_*` entry point already carries its own deterministic
    /// `max_exits`/`max_ticks` bound and does not consult this field at all — except
    /// [`run_to_first_halt_with_periodic_timer_and_devices`](Self::run_to_first_halt_with_periodic_timer_and_devices),
    /// which consults [`periodic_tick_watchdog_budget`](Self::periodic_tick_watchdog_budget)
    /// instead (a distinct, per-*tick* budget, not this whole-run one).
    watchdog_budget: Duration,
    /// The real wall-clock budget [`run_to_first_halt_with_periodic_timer_and_devices`]
    /// (Self::run_to_first_halt_with_periodic_timer_and_devices) gives *each individual tick's*
    /// `inject_at` call (todo.md §14 item 15/16 follow-up, see [`PERIODIC_TICK_WATCHDOG_BUDGET`]'s
    /// doc for why a tick — not just the whole run — needs its own watchdog). Initialized to
    /// [`PERIODIC_TICK_WATCHDOG_BUDGET`] by [`boot`](Self::boot)/[`restore`](Self::restore) and
    /// overridable via
    /// [`set_periodic_tick_watchdog_budget`](Self::set_periodic_tick_watchdog_budget) — deliberately
    /// a separate field from `watchdog_budget` above: the two guard different call paths with very
    /// different natural budgets (a whole run vs. one 500000-RCB-ish tick within it). Also reused,
    /// per call rather than per tick, to bound the same function's resume-past-halt burst loop
    /// (todo.md §14 item 17's real finding: a genuine H9 stall lived there, not inside `inject_at`,
    /// so this same budget now guards both).
    periodic_tick_watchdog_budget: Duration,
    /// The supervisor's cancellation flag, if one was installed via
    /// [`set_cancel_flag`](Self::set_cancel_flag) — `None` for every caller that never installs
    /// one, which is every existing caller. Modelled on `baud_vcpu::linux::watchdog`'s own
    /// `Arc<AtomicBool>` hand-down: the flag is set by some *other* thread (the one that noticed
    /// the caller went away) and read by the run loops between exits, never inside one.
    ///
    /// Determinism: an absent flag is a `None` check and a present one is a plain atomic load —
    /// neither touches the vCPU, the work clock, the device bus, or the exit sequence, so a run
    /// with no flag installed executes exactly the same sequence it did before this field
    /// existed, and a run with one installed but never set does too.
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// The default real wall-clock budget a booted or restored [`Multiverse`] gives its plain
/// [`run_to_first_halt`](Multiverse::run_to_first_halt) — generous next to the sub-second cost a
/// normal boot-to-halt takes even under this dev host's documented load contention (todo.md
/// §14.1, `thousand_branches_are_independent_and_deterministic` averages ~200-250ms/branch), but
/// still finite: this is what actually closes the "hangs forever" gap.
pub const DEFAULT_WATCHDOG_BUDGET: Duration = Duration::from_secs(30);

/// How often [`Multiverse::run_to_first_halt_with_periodic_timer_and_devices`] emits a `tracing`
/// progress line — todo.md §14 item 15's named observability gap: a multi-tens-of-minutes real-
/// kernel boot (e.g. H9's Ubuntu login-banner attempt) was previously a total black box until it
/// finished, timed out, or was killed, since the HTTP response carries no output until the whole
/// run resolves. Logging every tick would be too noisy for a 20000+-tick run; every 100 keeps the
/// log readable while still giving a live "how far in / is it stuck" signal via `tail -f` on the
/// server's own log output.
const RUN_LOOP_PROGRESS_LOG_INTERVAL_TICKS: u32 = 100;

/// Wall-clock budget for a *single* periodic-timer tick's `inject_at` call, inside
/// [`Multiverse::run_to_first_halt_with_periodic_timer_and_devices`] — todo.md §14 item 15/16
/// follow-up, the observability gap the item above only half-closed. That item found the run loop
/// had zero intermediate observability; this closes the sharper gap underneath it, found by
/// actually watching a real H9 attempt with the new progress logging: `run_until_exit`'s coarse
/// phase (`crates/baud-vcpu/src/linux/pmu.rs`) blocks inside one `KVM_RUN` until the guest itself
/// naturally vmexits, with nothing else bounding that wait — a guest running a long native
/// stretch with no I/O/HLT/MMIO activity can therefore park a single tick for however long that
/// stretch takes, same "tight `jmp $` never traps" hazard [`watchdog::Watchdog`]'s own header
/// documents for [`run_until_halted`](baud_vcpu::linux::run_until_halted), here scoped to one tick
/// instead of the whole run. `CancelKicker`'s own doc already measured one real periodic-timer
/// tick against Ubuntu 18.04.1 taking >120s in a handful of `KVM_RUN` calls; a real detached H9
/// attempt this iteration observed a single tick still not landing after 11+ minutes at ~95% host
/// CPU. This budget gives roughly 5x that measured-but-still-progressing 120s case before treating
/// a tick as stuck rather than merely slow — long enough that legitimate heavy guest work (disk
/// I/O, systemd activity) should not false-positive, short enough that a genuinely wedged tick
/// fails fast with a diagnosable [`RunLoopError::WatchdogKilled`] instead of hanging the whole
/// multi-tens-of-minutes boot indefinitely with zero signal. Not user-configurable yet (unlike
/// [`Multiverse::watchdog_budget`], which guards a different call path entirely and stays at its
/// own tighter [`DEFAULT_WATCHDOG_BUDGET`]) — a CLI/HTTP knob can follow once real H9 attempts
/// show whether 600s is the right number.
///
/// Also arms the resume-past-halt burst loop's own per-call watchdog (same function, further
/// down) — todo.md §14 item 17 found, via a live `gdb` backtrace of a real stalled H9 attempt,
/// that the actual unbounded block was not inside `inject_at` at all but inside that burst loop's
/// `step_exit_cancellable` call: a guest woken from a non-terminal `Hlt` by a directly-delivered
/// timer interrupt can run natively for however long before its next exit, the identical hazard
/// this budget already exists to bound, just reachable from a second call site within the same
/// function.
///
/// [`watchdog::Watchdog`]: baud_vcpu::linux::Watchdog
const PERIODIC_TICK_WATCHDOG_BUDGET: Duration = Duration::from_secs(600);

/// Below this, a single `inject_at`/device-service call inside one periodic-timer tick is normal
/// and not worth a log line of its own (the every-100-ticks progress line already covers the
/// common case) — at or above it, `run_to_first_halt_with_periodic_timer_and_devices` logs which
/// specific phase (the coarse/fine `inject_at` walk, or a named device's `service_running`/
/// `service_halted` call) actually took the time, at `tick_index`-level granularity instead of
/// the coarser 100-tick progress line. Filed after a real H9 (Ubuntu) attempt stalled for 28+
/// minutes with only the 100-tick progress line to go on — ambiguous between "`inject_at` is
/// wedged" (which the per-tick `Watchdog` already catches) and "a device's own servicing is
/// legitimately/pathologically slow" (which it does not, `process_available_chains`'s own bound
/// on the driver's snapshotted index notwithstanding — a large real batch can still be slow, just
/// not infinite). Pure observability: does not change control flow.
const SLOW_TICK_PHASE_LOG_THRESHOLD: Duration = Duration::from_secs(1);

/// Where an injected interrupt actually landed (H4, specs/baud-vcpu.md §5): the instruction
/// pointer and cumulative work-clock RCB at the moment `Multiverse::inject_timer_tick` delivered
/// the vector. `timer_tick_lands_at_identical_instruction` asserts this tuple is identical across
/// a double-run — the same guarantee `boundary::ExecPoint`'s scripted-stepper tests already prove
/// in the abstract, exercised here for the first time against a real vCPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerTick {
    pub rip: u64,
    pub rcb: u64,
}

/// What running a booted guest to its first halt observed (specs/baud-multiverse.md §8's
/// `ram_hash_at_first_hlt`).
#[derive(Debug)]
pub struct HaltOutcome {
    /// Every byte the guest wrote to the console (COM1 data register), in order.
    pub console_output: Vec<u8>,
    /// `blake3:<hex>` of the whole guest-RAM region, computed right after the halt.
    pub ram_hash: String,
    /// The vCPU's RIP at the moment the halt was observed (`KVM_GET_REGS`, read right after the
    /// `Hlt`/`Shutdown` dispatch returns) — the "identical exit point" half of spec §4.3's
    /// `init_powers_off_deterministically`: a clean shutdown must land at the same instruction
    /// across two boots, not just produce the same RAM/console bytes.
    pub exit_pc: u64,
}

/// Exactly [`HaltOutcome`] minus its `ram_hash` — what
/// [`Multiverse::run_to_first_halt_without_ram_hash`] observes for a caller that does not want to
/// pay for a blake3 pass over all of [`layout::GUEST_RAM_SIZE`] on every single run.
#[derive(Debug)]
pub struct HaltObservation {
    /// Every byte the guest wrote to the console (COM1 data register), in order — identical to
    /// [`HaltOutcome::console_output`].
    pub console_output: Vec<u8>,
    /// The vCPU's RIP at the moment the halt was observed — identical to [`HaltOutcome::exit_pc`].
    pub exit_pc: u64,
}

/// One stopping condition [`Multiverse::run_until_branch_or_halt`] can report — either the guest
/// halted normally (same payload as [`Multiverse::run_to_first_halt`]), or it issued the tape
/// device's `MARK_BRANCH` control op (specs/baud-tape-device.md §4) and is still running, paused
/// right after that `OUT` retired.
#[derive(Debug)]
pub enum RunUntilBranchOutcome {
    /// The guest reached `Hlt`/`Shutdown` before ever calling `MARK_BRANCH`.
    Halted(HaltOutcome),
    /// The guest issued `MARK_BRANCH` at tape cursor `step` (`baud_proto::Msg::MarkBranch`'s own
    /// field) and is still running — a caller that wants to keep exploring from exactly this
    /// point should [`Multiverse::snapshot`] this `Multiverse` right now, before calling anything
    /// else that would advance it further.
    MarkBranch { step: u64 },
}

/// Exactly [`RunUntilBranchOutcome`] minus its `Halted` arm's `ram_hash` (i.e. carrying
/// [`HaltObservation`] instead of [`HaltOutcome`]) — what the `_without_ram_hash` sibling of every
/// `run_until_branch_or_halt*` entry point below reports for a caller that runs many branches but
/// only needs the RAM hash of some of them (todo.md §14.1 "still open" item 1: `run_kvm.rs`'s own
/// test suite calls these ~90 times and reads the resulting hash in only 2 of them). The
/// `MarkBranch` arm already carried no `ram_hash` of its own (a live checkpoint's RAM hash is
/// always read via the separate [`Multiverse::ram_hash`] call, never embedded here), so it is
/// unchanged between the two enums.
#[derive(Debug)]
pub enum RunUntilBranchObservation {
    /// The guest reached `Hlt`/`Shutdown` before ever calling `MARK_BRANCH`.
    Halted(HaltObservation),
    /// The guest issued `MARK_BRANCH` at tape cursor `step` — identical meaning to
    /// [`RunUntilBranchOutcome::MarkBranch`].
    MarkBranch { step: u64 },
}

/// Seed the enforced-regime `RDRAND` entropy stream (`WorkClock::with_entropy_seed`) from the
/// run's own tape, so the same tape always produces the same `rdrand` draw sequence and a
/// different tape byte changes it — the same `all_input_is_tape_derived` guarantee the tape
/// device already provides for guest-facing I/O, extended to this VMM-internal entropy source
/// (todo.md §3.2: enforced regime "serves the tape"). A dedicated hash, not a shared cursor: this
/// keeps the draw independent of `TapeDevice`'s own guest-facing PIO cursor, so serving `rdrand`
/// never perturbs what the guest's own explicit tape reads consume next.
///
/// **Must be called before [`LinuxBranchCounter::new`], never after** — a real-hardware finding
/// from `timer_tick_lands_at_identical_instruction` going from consistently green to
/// consistently failing (not flaky) the moment `boot` started calling this: `exclude_host` does
/// not work on this host (`LinuxBranchCounter::new`'s own doc), so the branch counter counts this
/// *process's* retired branches too, not just the guest's — and `blake3::hash`'s first-ever call
/// in a process does one-time CPU-feature-detection work with a materially different branch count
/// than every call after it (cached). Two boots in the same test process are exactly "first call"
/// vs "every call after" — hashing *after* the counter starts skewed one boot's RCB baseline
/// against the other's by tens of counts, blowing straight through
/// `RCB_HARDWARE_JITTER_TOLERANCE`. Hashing before the counter exists keeps this entirely outside
/// the counted window, for both boots alike.
fn entropy_seed_from_tape(tape: &[u8]) -> u64 {
    let hash = blake3::hash(tape);
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("blake3 hash is at least 8 bytes"))
}

/// Derive the `SETUP_RNG_SEED` `setup_data` seed (specs/baud-multiverse.md §3.8's "Boot RNG seed")
/// from the run's own tape — same tape-determinism guarantee as [`entropy_seed_from_tape`], but a
/// domain-separated hash (a distinct prefix, not a shared cursor) so the boot seed and the
/// `rdrand`/`rdseed` entropy substream never draw from the same stream. Called from [`boot_guest`],
/// which — like `entropy_seed_from_tape` — must run before [`LinuxBranchCounter::new`] for the same
/// first-call-blake3-jitter reason documented there.
fn rng_seed_from_tape(tape: &[u8]) -> [u8; bootparams::RNG_SEED_LEN] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"baud:setup-data:rng-seed:v1");
    hasher.update(tape);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod rng_seed_from_tape_tests {
    use super::*;

    #[test]
    fn same_tape_reproduces_the_identical_seed() {
        let tape = b"a tape byte stream".to_vec();
        assert_eq!(rng_seed_from_tape(&tape), rng_seed_from_tape(&tape));
    }

    #[test]
    fn one_changed_tape_byte_changes_the_seed() {
        let seed_a = rng_seed_from_tape(b"tape-a");
        let seed_b = rng_seed_from_tape(b"tape-b");
        assert_ne!(seed_a, seed_b);
    }

    #[test]
    fn is_domain_separated_from_the_rdrand_entropy_substream() {
        // Same tape, but `entropy_seed_from_tape` and `rng_seed_from_tape` must draw from
        // independent hash domains (a distinct prefix) — otherwise the boot RNG seed would leak
        // the entropy substream's first 8 bytes (or vice versa).
        let tape = b"shared tape".to_vec();
        let entropy_seed = entropy_seed_from_tape(&tape);
        let rng_seed = rng_seed_from_tape(&tape);
        assert_ne!(entropy_seed.to_le_bytes(), rng_seed[..8]);
    }
}

/// The last (up to) 200 bytes of `console`, lossily decoded — attached to
/// [`Multiverse::run_to_first_halt_with_periodic_timer_and_devices`]'s timeout errors so a caller
/// tuning `max_ticks`/`halt_console_pattern` against a real, slow-booting guest (H9's Ubuntu boot,
/// todo.md §14 item 12) can see how far the console actually got without a separate debug build —
/// a bare "guest did not halt"/"pattern not found" message gives no way to tell "stuck at the very
/// start" from "one byte short of the target" from the error alone.
fn console_tail(console: &[u8]) -> String {
    let tail = &console[console.len().saturating_sub(200)..];
    String::from_utf8_lossy(tail).into_owned()
}

/// One virtio device serviceable from inside
/// [`Multiverse::run_to_first_halt_with_periodic_timer_and_devices`]'s tick loop — the "poll N
/// devices" abstraction todo.md §14 item 5(b)'s note on `run_to_first_halt_with_virtio_pci_blk`
/// asked for once a real boot needs more than one device serviced alongside the timer (a real
/// Ubuntu guest needs periodic ticks, virtio-rng, *and* virtio-blk simultaneously). Every field is
/// a plain `fn` pointer (not a capturing closure), so each device is described declaratively and
/// the tick loop itself stays device-agnostic; `notify_count`/`service_running`/`service_halted`
/// are exactly the three per-device pieces `run_to_first_halt_with_virtio_rng` and
/// `run_to_first_halt_with_virtio_pci_blk` each already hand-wrote inline.
struct TickPolledDevice {
    vector: u8,
    notify_count: fn(&Multiverse) -> Option<u64>,
    service_running: fn(&mut Multiverse, u8) -> Result<u32, RunLoopError>,
    service_halted: fn(&mut Multiverse, u8) -> Result<u32, RunLoopError>,
}

impl Multiverse {
    /// Run [`boot_guest`] and wire up the work-clock (`base + k * rcb`, specs/baud-multiverse.md
    /// §4), console, and tape (specs/baud-tape-device.md) devices the run loop needs. `base` is
    /// normally `0` (a guest booting at virtual time zero); `k` scales RCB into a plausible Hz
    /// range for the guest's own clock arithmetic to work with sane-looking values. `tape` is the
    /// run's entire nondeterministic-input budget — the sole source the tape device serves
    /// (specs/baud-tape-device.md §5), fixed for this `Multiverse`'s whole lifetime.
    ///
    /// `dirty_ring_entries`, when `Some`, negotiates and opens a `KVM_CAP_DIRTY_LOG_RING` ring for
    /// this guest right here at boot time — real-hardware finding (todo.md §14): the kernel
    /// refuses to negotiate the capability once a vCPU already exists, so there is no correct way
    /// to "turn it on later" after `boot` returns (see [`create_vm_vcpu_shell`]'s and
    /// `baud_snapshot::linux::DirtyRing`'s docs). Pass `None` for a `Multiverse` that will never
    /// call [`reset_dirty_pages`](Self::reset_dirty_pages) — the ring is an opt-in cost (an extra
    /// mmap + capability negotiation) callers that never rewind should not pay. `entries` must be a
    /// nonzero power of two; 4096 is a reasonable default with no sharper estimate of a run
    /// segment's write set.
    pub fn boot(
        kernel_path: &Path,
        cmdline: &str,
        base: u64,
        k: u64,
        tape: Vec<u8>,
        dirty_ring_entries: Option<u32>,
    ) -> Result<Self, BootError> {
        Self::boot_with_rdseed_sites(kernel_path, cmdline, base, k, tape, dirty_ring_entries, None, [])
    }

    /// [`boot`](Self::boot) plus this guest image's known `rdseed`→`UD2` rewrite sites
    /// (`baud_packages::rewrite_rdseed`'s `RdseedRewriteReport`, todo.md §4), keyed by the guest
    /// address of the `UD2` itself and registered on the work-clock via
    /// [`WorkClock::with_rdseed_sites`](crate::timesource::WorkClock::with_rdseed_sites) before any
    /// guest code runs, plus an optional `initramfs` (todo.md §4.2/§4.3) loaded at
    /// [`layout::INITRAMFS_ADDR`] and pointed to by `hdr.ramdisk_image`/`ramdisk_size` —
    /// `None` for any guest with no initramfs (every fixture in this crate today).
    ///
    /// Only sites passed here are ever *served* a value: under the enforced-regime patched module
    /// (`kernel-module/baud-enforced/ud2-enforce.patch`), every `UD2` the guest executes traps to
    /// userspace, and any that this table does not recognize gets its `#UD` re-injected verbatim
    /// (`baud_vcpu::DispatchOutcome::ReinjectUd`) — a real invalid opcode, or the guest kernel's
    /// own `BUG()`/`WARN_ON()`, both of which also compile to a bare `UD2`, keep behaving exactly
    /// as they would with no patch loaded. Passing an empty table (what [`boot`](Self::boot) does)
    /// is therefore always safe, never a silent "serve a guess for every `#UD`".
    ///
    /// The end-to-end wiring from an image build to this table (todo.md §14's "`RdseedRewriteReport`
    /// -> boot wiring") lives in `baud-server`: `baud image rewrite-rdseed`
    /// (`baud_packages::rewrite_rdseed`) writes a `<image>.rdseed-sites.json` sidecar next to the
    /// patched image, and `baud-server`'s `rdseed_sites::load_rdseed_sites` reads it back into
    /// exactly the `(u64, EnforcedRdseedSite)` shape this function wants for every real
    /// `/run/kvm*` boot — this crate itself stays image-format-agnostic and takes the table as a
    /// plain argument. The one remaining hand-verified caller is
    /// `rdseed_enforced_regime_is_bit_exact_across_boots`, which passes the sites of a fixed,
    /// hand-assembled flat-binary fixture that never goes through the ELF-based rewrite pass at all
    /// (see `tests/fixtures/rdseed-guest/BUILD.md`), so it still hardcodes its one site rather than
    /// reading a sidecar.
    #[allow(clippy::too_many_arguments)]
    pub fn boot_with_rdseed_sites(
        kernel_path: &Path,
        cmdline: &str,
        base: u64,
        k: u64,
        tape: Vec<u8>,
        dirty_ring_entries: Option<u32>,
        initramfs: Option<&[u8]>,
        rdseed_sites: impl IntoIterator<Item = (u64, baud_vcpu::EnforcedRdseedSite)>,
    ) -> Result<Self, BootError> {
        let (guest, dirty_ring) = boot_guest(kernel_path, cmdline, &tape, dirty_ring_entries, initramfs)?;
        // Must come before `LinuxBranchCounter::new()` — see `entropy_seed_from_tape`'s doc.
        let entropy_seed = entropy_seed_from_tape(&tape);
        let counter = LinuxBranchCounter::new()?;
        let bus = DeviceBus::with_tape(tape);
        let time = WorkClock::new(base, k, counter)
            .with_entropy_seed(entropy_seed)
            .with_rdseed_sites(rdseed_sites);
        Ok(Multiverse { guest, bus, time, dirty_ring, watchdog_budget: DEFAULT_WATCHDOG_BUDGET, periodic_tick_watchdog_budget: PERIODIC_TICK_WATCHDOG_BUDGET, cancel: None })
    }

    /// Capture this `Multiverse`'s complete state into a [`Universe`] (specs/baud-snapshot.md §2's
    /// `Snapshot::capture`, §3's enumerated capture set): every `KVM_GET_*`
    /// `baud_snapshot::linux::capture` walks over this instance's own `kvm`/`vm`/`vcpu`/`guest_mem`
    /// handles, plus the three pieces of state only this crate's device models know —
    /// `WorkClock`'s `base`/`tsc_deadline`/`tsc_aux` (todo.md §3.3's work-clock anchor; without the
    /// latter two a restored guest that had already armed `IA32_TSC_DEADLINE` would resume with it
    /// disarmed, see `WorkClock::restore`'s doc), the tape-device cursor, and the console's output
    /// history so far. RAM pages are deduplicated into `page_store`, shared across every universe
    /// interned through the same store (specs/baud-snapshot.md §4) — callers exploring many branch
    /// points from one run pass the same `PageStore` to every `snapshot` call for that guest.
    ///
    /// Flushes any pending PIO completion first (real-hardware finding, todo.md §14, found by
    /// `shell_into_universe_resumes`): none of this crate's ports are in-kernel-emulated (no
    /// `KVM_CREATE_IRQCHIP`/registered `KVM_IOEVENTFD`, every `IN`/`OUT` always exits to
    /// userspace), so KVM defers that instruction's retirement — including the `RIP` advance —
    /// until the *next* `KVM_RUN` call, not the exit that reported it (`vcpu->arch.
    /// complete_userspace_io`, processed at the top of the following `kvm_arch_vcpu_ioctl_run`, is
    /// the kernel mechanism). Every prior real-hardware snapshot point either had zero exits behind
    /// it (a fresh boot) or came right after `inject_timer_tick`'s single-step confirmation loop
    /// (which itself already calls `KVM_RUN` enough times to retire whatever was pending), so this
    /// staleness never surfaced until a snapshot was taken immediately after a plain, uninterrupted
    /// `step_exit()`. Calling `vcpu.get_regs()` at that moment reads the *stale* pre-retirement
    /// `RIP` (still pointing *at* the just-exited instruction, not after it); a universe restored
    /// from it starts a brand-new vCPU with no memory of "this instruction already retired", so it
    /// genuinely re-executes that same `IN`/`OUT` — for `OUT`, an observable duplicate byte on the
    /// console. [`flush_pending_pio_completion`] resolves this the standard way real VMMs do (the
    /// same `immediate_exit` mechanism `crates/baud-vcpu/src/linux/pmu.rs`'s doc already names,
    /// todo.md §14's H4 finding about that field's kernel-side stickiness): set
    /// `kvm_run.immediate_exit = 1`, call `KVM_RUN` once (it retires the pending completion at
    /// entry, then returns `-EINTR` immediately without executing any new guest instruction), clear
    /// the flag back to `0` again afterward (the same stickiness this field has everywhere else in
    /// this workspace).
    pub fn snapshot(
        &mut self,
        page_store: &mut PageStore,
    ) -> Result<Universe, baud_snapshot::linux::CaptureError> {
        self.flush_pending_pio_completion()?;
        baud_snapshot::linux::capture(
            &self.guest.kvm,
            &self.guest.vm,
            &self.guest.vcpu,
            &self.guest.guest_mem,
            layout::GUEST_RAM_START,
            layout::GUEST_RAM_SIZE,
            page_store,
            self.time.base(),
            self.time.current_rcb(),
            self.time.tsc_deadline(),
            self.time.tsc_aux(),
            self.time.entropy_state(),
            self.bus.tape.device().cursor(),
            self.bus.console.output().to_vec(),
        )
    }

    /// See [`snapshot`](Self::snapshot)'s doc for why this exists. `kvm_run.immediate_exit = 1`
    /// makes the next `KVM_RUN` retire any pending PIO completion and return `-EINTR` immediately —
    /// the standard technique real VMMs use to force a clean re-entry point without letting the
    /// guest execute anything new (`crates/baud-vcpu/src/linux/pmu.rs`'s module doc names the same
    /// mechanism). A non-`EINTR` error is a real ioctl failure (`DeterminismHole`-worthy elsewhere
    /// in this crate, but `snapshot`'s own return type is `CaptureError`, so it is reported as
    /// `CaptureError::Kvm` instead of inventing a second error path for the same call site).
    fn flush_pending_pio_completion(&mut self) -> Result<(), kvm_ioctls::Error> {
        self.guest.vcpu.get_kvm_run().immediate_exit = 1;
        let errno = self.guest.vcpu.run().err().map(|e| e.errno());
        self.guest.vcpu.get_kvm_run().immediate_exit = 0;
        match errno {
            None | Some(libc::EINTR) => Ok(()),
            Some(_) => Err(kvm_ioctls::Error::new(errno.unwrap())),
        }
    }

    /// Reconstruct a whole new `Multiverse` from a captured [`Universe`] (specs/baud-snapshot.md
    /// §2's `Snapshot::restore`) instead of booting a kernel image: [`restore_guest`] rebuilds the
    /// KVM/vCPU/RAM state per `restore_plan`'s ordered sequence (specs/baud-snapshot.md §6,
    /// refusing a CPU-model mismatch unless `template_active`), then this method reassembles the
    /// device layer `baud-snapshot` deliberately left to the caller
    /// (`RestoreStep::RestoreDevice`'s doc): the tape device over `tape` (the run's whole tape —
    /// unchanged across a run's lifetime, same value `boot`'s caller would have passed, only the
    /// cursor differs) fast-forwarded to `universe.device.tape_cursor`, the console pre-seeded with
    /// `universe.device.console`'s captured output history, and the work-clock rebuilt via
    /// [`WorkClock::restore`] so a guest that had armed `IA32_TSC_DEADLINE`/set `IA32_TSC_AUX`
    /// resumes seeing those exact values. `k` is a run-level constant (`virtual_tsc = base + k *
    /// rcb`), not part of the captured state, so the caller supplies the same `k` the original
    /// `boot` used — a mismatched `k` would silently desynchronize the restored guest's clock from
    /// what a straight run would have produced, even though every other field is byte-exact.
    ///
    /// `dirty_ring_entries` behaves exactly as in [`boot`](Self::boot) — `Some` negotiates and opens
    /// a fresh ring on the restored guest's newly-created vCPU (a restored `Multiverse` gets a
    /// brand-new ring with no memory of whatever ring, if any, the original had; the same real-
    /// hardware ordering constraint applies here too, since `restore_guest` also creates a vCPU).
    pub fn restore(
        universe: &Universe,
        tape: Vec<u8>,
        k: u64,
        template_active: bool,
        dirty_ring_entries: Option<u32>,
    ) -> Result<Self, RestoreError> {
        let (guest, dirty_ring) = restore_guest(universe, template_active, dirty_ring_entries)?;
        let counter = LinuxBranchCounter::new().map_err(BootError::BranchCounter)?;
        let bus = DeviceBus::restore(tape, universe.device.tape_cursor, universe.device.console.clone());
        let time = WorkClock::restore(
            universe.clock.work_clock_base,
            k,
            universe.clock.rcb_anchor,
            universe.clock.tsc_deadline,
            universe.clock.tsc_aux,
            universe.clock.entropy_state,
            counter,
        );
        Ok(Multiverse { guest, bus, time, dirty_ring, watchdog_budget: DEFAULT_WATCHDOG_BUDGET, periodic_tick_watchdog_budget: PERIODIC_TICK_WATCHDOG_BUDGET, cancel: None })
    }

    /// Fork a new, independent continuation from a captured [`Universe`] on its own tape suffix
    /// (specs/baud-snapshot.md §4's `Snapshot::branch(parent: &Universe, suffix: TapeSuffix) ->
    /// Branch`) — todo.md §14's real architecture gap: the spec's `UFFDIO_CONTINUE` memory-sharing
    /// mechanism needs guest RAM backed by a shared (memfd/hugetlbfs) mapping to fault minor faults
    /// against, but this crate's guest RAM (`allocate_and_register_guest_ram`) is a private
    /// anonymous mapping — switching that is an architecture change this crate cannot absorb alone
    /// (specs/baud-snapshot.md §10). This realizes the spec's own documented escape hatch instead
    /// ("`fork()` copy-on-write is the small-N fallback") — via [`restore`](Self::restore) rather
    /// than a literal `fork(2)`, because a raw OS `fork()` cannot safely reuse this `Multiverse`'s
    /// already-open KVM `vm`/`vcpu` fds either: a `VmFd` is tied to its *creating* process's `mm` at
    /// `KVM_CREATE_VM` time, so a forked child inheriting the same `vm` fd would still have its
    /// guest-physical memory resolve through KVM's EPT against the *parent's* address space, not the
    /// child's own post-fork CoW copy, regardless of how the two processes' host page tables
    /// diverge afterward — sharing a `vm`/`vcpu` fd across a fork does not give independent guest
    /// memory no matter the thread model. Each branch therefore gets a fresh `KVM_CREATE_VM`/vCPU/
    /// guest-RAM region via [`restore`](Self::restore) — fully correct and independent (proven by
    /// `thousand_branches_are_independent_and_deterministic`, below), at the cost of a real
    /// per-branch RAM copy (`baud_snapshot::linux::restore` walks all of `universe.ram`) instead of
    /// the spec's O(write-set) CoW sharing; that memory-efficiency guarantee remains open (§10).
    /// `template_active` is always `false` here — branching is always same-host/same-CPU-model by
    /// construction (the parent `Universe` was just captured on this process), never the
    /// cross-model scenario `template_active` exists for.
    pub fn branch(
        universe: &Universe,
        tape_suffix: Vec<u8>,
        k: u64,
        dirty_ring_entries: Option<u32>,
    ) -> Result<Self, RestoreError> {
        Self::restore(universe, tape_suffix, k, false, dirty_ring_entries)
    }

    /// Rewind guest RAM to `base_ram`'s content for exactly the pages the dirty ring reports as
    /// touched since the last [`boot`](Self::boot)/`reset_dirty_pages` call (specs/baud-snapshot.md
    /// §5: "rewind copies back only dirtied pages ... cost ∝ change,
    /// not machine size", `reset_cost_scales_with_write_set`'s guarantee). `base_ram` is normally
    /// a prior [`snapshot`](Self::snapshot)'s `Universe::ram` — the state this `Multiverse` should
    /// return to — indexed identically to a captured `Universe`'s own RAM (`universe.rs`'s "page
    /// `i` covers guest-physical `[i * PAGE_SIZE, (i+1) * PAGE_SIZE)`" convention, matched exactly
    /// by [`crate::dirty::ram_page_indices`]'s doc).
    ///
    /// Returns the number of pages actually restored — by construction (`crate::dirty::
    /// ram_page_indices` only ever selects, never invents, RAM-slot entries) this is exactly the
    /// dirty ring's harvested RAM-page count for this call, the direct observable
    /// `reset_cost_scales_with_write_set` checks against `dirty_ring_count`. Confirms the reset to
    /// the kernel (`KVM_RESET_DIRTY_RINGS`) only after every harvested page has been successfully
    /// written back, so a mid-loop I/O failure never tells the kernel pages were reclaimed that
    /// this call did not actually restore.
    pub fn reset_dirty_pages(&mut self, base_ram: &[PageRef]) -> Result<usize, ResetError> {
        let harvested = self.dirty_ring.as_mut().ok_or(ResetError::NotEnabled)?.collect();
        let pages = crate::dirty::ram_page_indices(&harvested, crate::dirty::RAM_SLOT);
        for &page in &pages {
            let page_ref = base_ram.get(page).ok_or(ResetError::PageOutOfRange(page))?;
            let offset = layout::GUEST_RAM_START + (page * baud_snapshot::PAGE_SIZE) as u64;
            self.guest
                .guest_mem
                .write_slice(page_ref.bytes(), GuestAddress(offset))
                .map_err(|e| ResetError::GuestMemory(page, e))?;
        }
        // SAFETY-relevant-in-spirit-not-code: only confirmed after every page above wrote
        // successfully, so a partial failure (returned `Err` above) leaves the ring's RESET-marked
        // entries un-confirmed -- they stay DIRTY and are re-harvested by the next `collect()`
        // call rather than being silently dropped (`DirtyRing::confirm_reset`'s own doc).
        let ring = self.dirty_ring.as_mut().ok_or(ResetError::NotEnabled)?;
        ring.confirm_reset(&self.guest.vm)?;
        Ok(pages.len())
    }

    /// Drive the run loop to the guest's first `Hlt`/`Shutdown` (specs/baud-multiverse.md §8's
    /// `double_boot_memory_identical`: "boot the hello image twice ... assert equal blake3 of guest
    /// RAM at first `Hlt`"). Every VM exit along the way is resolved by `dispatch_exit` through
    /// this `Multiverse`'s console `Bus` and work-clock `TimeSource` — any exit kind neither knows
    /// how to serve is `Err(RunLoopError::DeterminismHole)`, never a silent continue
    /// (specs/baud-vcpu.md §3); a guest that never reaches `Hlt`/`Shutdown` at all is
    /// `Err(RunLoopError::WatchdogKilled)` once [`watchdog_budget`](Self::watchdog_budget) real
    /// milliseconds pass (todo.md §14.1 "Still open" item 1) instead of hanging this call forever.
    pub fn run_to_first_halt(&mut self) -> Result<HaltOutcome, RunLoopError> {
        let observed = self.run_to_first_halt_without_ram_hash()?;
        Ok(HaltOutcome {
            console_output: observed.console_output,
            ram_hash: self.ram_hash(),
            exit_pc: observed.exit_pc,
        })
    }

    /// Override the real wall-clock budget [`run_to_first_halt`](Self::run_to_first_halt)/
    /// [`run_to_first_halt_without_ram_hash`](Self::run_to_first_halt_without_ram_hash) give the
    /// watchdog — [`boot`](Self::boot)/[`restore`](Self::restore) already set
    /// [`DEFAULT_WATCHDOG_BUDGET`], so callers only need this to tighten it (a test proving the
    /// watchdog actually fires without waiting 30 real seconds) or pass `Duration::ZERO` to
    /// disable it outright (`watchdog::Watchdog::arm`'s doc).
    pub fn set_watchdog_budget(&mut self, budget: Duration) {
        self.watchdog_budget = budget;
    }

    /// Override the real wall-clock budget
    /// [`run_to_first_halt_with_periodic_timer_and_devices`]
    /// (Self::run_to_first_halt_with_periodic_timer_and_devices) gives *each tick's* `inject_at`
    /// call — distinct from [`set_watchdog_budget`](Self::set_watchdog_budget), which guards the
    /// whole-run path instead. [`boot`](Self::boot)/[`restore`](Self::restore) already set
    /// [`PERIODIC_TICK_WATCHDOG_BUDGET`], so callers only need this to tighten it (a test proving
    /// a wedged tick is actually reclaimed without waiting the real 600s default) or pass
    /// `Duration::ZERO` to disable it outright (`watchdog::Watchdog::arm`'s doc).
    pub fn set_periodic_tick_watchdog_budget(&mut self, budget: Duration) {
        self.periodic_tick_watchdog_budget = budget;
    }

    /// Install the supervisor's cancellation flag: once some other thread stores `true` into
    /// `flag`, **every** run loop on this `Multiverse` stops within milliseconds and returns
    /// [`RunLoopError::Cancelled`], instead of driving the guest to completion for a caller that
    /// is no longer there.
    ///
    /// "Within milliseconds" is the load-bearing part, and it takes three cooperating pieces —
    /// polling alone was measured to be worth nothing here:
    ///
    /// 1. **A signal.** A blocked `KVM_RUN` ioctl can only be broken out of by a signal delivered
    ///    to the running thread. Each run loop arms a
    ///    [`CancelKicker`](baud_vcpu::linux::CancelKicker) — the same `SIGUSR1` machinery
    ///    `baud_vcpu::linux::watchdog` already used for its wall-clock kill — which re-signals the
    ///    vCPU thread every few milliseconds for as long as the flag is set. Without it, a single
    ///    tick of a real periodic-timer run was measured holding a core for 120 s+ with the flag
    ///    set 4 ms in; one tick can be arbitrarily long, so a per-tick poll is not a bound.
    /// 2. **A check in the boundary walk.** `baud_vcpu::boundary::inject_at`'s single-step walk —
    ///    where a periodic-timer run spends nearly all of its time — checks the flag once per
    ///    step (`PmuStepper::check_cancelled`), so a cancelled run stops between two exits even
    ///    when no signal was needed.
    /// 3. **A check at every loop head**, which is what the other two hand control back to.
    ///
    /// Like the watchdog kill this is deliberately outside the deterministic boundary: *whether* a
    /// run is cancelled depends on host-side events, not on the guest's own instruction stream.
    /// What it must never do is perturb a run that is *not* cancelled, and it does not — with no
    /// flag installed no thread is spawned, no signal handler is installed, and no signal can ever
    /// be delivered; see the [`cancel`](Self::cancel) field's own note and
    /// `an_installed_but_unset_cancel_flag_leaves_a_periodic_timer_run_byte_identical`, which pins
    /// tick-for-tick identical landing points against a run with no flag at all.
    ///
    /// Returning `Cancelled` unwinds normally, so this `Multiverse` and everything it owns (KVM
    /// fds, the guest-RAM mapping, the `perf_event` counter, the virtio-blk backing store) is
    /// released by the ordinary structural drop of whatever owns it — there is no teardown step
    /// a caller has to remember.
    pub fn set_cancel_flag(&mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.cancel = Some(flag);
    }

    /// Whether [`set_cancel_flag`](Self::set_cancel_flag)'s flag is installed *and* currently
    /// set. `None` (no flag installed) short-circuits without any atomic access at all, so the
    /// overwhelmingly common case costs one `Option` discriminant test per loop iteration.
    fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Arm this run's `SIGUSR1` kicker for the duration of one run-loop call
    /// (`baud_vcpu::linux::CancelKicker` — read that type's doc first; it is the whole reason
    /// cancellation is prompt rather than eventual). Returns a guard that disarms and joins on
    /// drop, so every early return in the loops below is covered without any explicit teardown.
    ///
    /// With no flag installed this spawns nothing, installs no signal handler, and can never
    /// deliver a signal — the guest's exit sequence is untouched, which is the whole determinism
    /// contract [`set_cancel_flag`](Self::set_cancel_flag) makes.
    fn arm_cancel_kicker(&self) -> baud_vcpu::linux::CancelKicker {
        baud_vcpu::linux::CancelKicker::arm(self.cancel.clone())
    }

    /// A `LinuxPmuStepper` over this `Multiverse`'s own vCPU/bus/time handles, carrying this run's
    /// cancellation flag (if any) so `boundary::inject_at`'s single-step walk and the stepper's own
    /// `KVM_RUN` loops both stop when the supervisor cancels — the one factory every periodic-timer
    /// call site uses, so none of them can forget the flag.
    fn cancellable_stepper(&mut self) -> baud_vcpu::linux::pmu::LinuxPmuStepper<'_, '_> {
        let cancel = self.cancel.clone();
        baud_vcpu::linux::pmu::LinuxPmuStepper::new(&mut self.guest.vcpu, &mut self.bus, &mut self.time)
            .with_cancel(cancel)
    }

    /// Classify a boundary-engine (`inject_at`/`PmuStepper`) failure: a run whose supervisor
    /// cancelled it is [`RunLoopError::Cancelled`], never a [`DeterminismHole`] — calling an
    /// abandoned run a determinism hole would be a lie about the guest's execution
    /// (`RunLoopError::Cancelled`'s own doc). Checks both the flag itself and the stepper's own
    /// `ECANCELED` marker (`baud_vcpu::linux::is_cancelled_error`), so a flag that was somehow
    /// cleared again between the stepper noticing it and this call still classifies correctly.
    fn stepper_error(&self, e: std::io::Error) -> RunLoopError {
        if self.is_cancelled() || baud_vcpu::linux::is_cancelled_error(&e) {
            RunLoopError::Cancelled
        } else {
            DeterminismHole(e.to_string()).into()
        }
    }

    /// [`step_exit`](Self::step_exit) for the run loops that carry a cancellation flag: a
    /// `KVM_RUN` broken out of by this run's own [`CancelKicker`](baud_vcpu::linux::CancelKicker)
    /// returns [`RunLoopError::Cancelled`] instead of transparently re-entering the ioctl that
    /// kick was sent to escape. Identical to `step_exit` when no flag is installed.
    fn step_exit_cancellable(&mut self) -> Result<baud_vcpu::DispatchOutcome, RunLoopError> {
        baud_vcpu::linux::run_one_exit_cancellable(
            &mut self.guest.vcpu,
            &mut self.bus,
            &mut self.time,
            self.cancel.as_deref(),
        )
    }

    /// [`step_exit_cancellable`](Self::step_exit_cancellable) plus a per-call wall-clock watchdog
    /// (`watchdog`, armed and disarmed by the caller around this one call) — for a burst-drain loop
    /// that calls this many times in a row rather than going through
    /// [`run_until_halted`](baud_vcpu::linux::run_until_halted), which already carries its own
    /// whole-run watchdog. A single such call can otherwise block inside `KVM_RUN` forever if the
    /// guest happens to make no further exit from this point on (todo.md §14 item 17's follow-up:
    /// this is exactly what a real H9 Ubuntu attempt hit inside
    /// [`run_to_first_halt_with_periodic_timer_and_devices`](Self::run_to_first_halt_with_periodic_timer_and_devices)'s
    /// resume-past-halt burst loop — a code path distinct from the `inject_at` call the per-*tick*
    /// watchdog already covers).
    fn step_exit_cancellable_with_watchdog(
        &mut self,
        watchdog: &std::sync::atomic::AtomicBool,
    ) -> Result<baud_vcpu::DispatchOutcome, RunLoopError> {
        baud_vcpu::linux::run_one_exit_cancellable_with_watchdog(
            &mut self.guest.vcpu,
            &mut self.bus,
            &mut self.time,
            Some(watchdog),
            self.cancel.as_deref(),
        )
    }

    /// [`run_to_first_halt`](Self::run_to_first_halt) without the guest-RAM hash: the identical run
    /// loop, stopping at the identical first `Hlt`/`Shutdown` and reporting the identical console
    /// output and exit PC, but skipping [`ram_hash`](Self::ram_hash)'s blake3 pass over all
    /// [`layout::GUEST_RAM_SIZE`] bytes (~0.1s per call on this dev machine, irrespective of how
    /// much of that RAM the guest actually touched).
    ///
    /// For a caller that runs many guests but only needs the RAM hash of some of them —
    /// `thousand_branches_are_independent_and_deterministic` runs 1000 branches and compares the
    /// RAM hash of 8 — this is the difference between paying that pass 1000 times and paying it 8
    /// times. The hash is not lost by using this entry point: a halted guest's RAM does not change
    /// underneath its `Multiverse`, so [`ram_hash`](Self::ram_hash) called afterwards returns
    /// exactly what [`run_to_first_halt`](Self::run_to_first_halt) would have put in
    /// [`HaltOutcome::ram_hash`] — which is precisely how `run_to_first_halt` itself is now
    /// implemented, on top of this.
    pub fn run_to_first_halt_without_ram_hash(&mut self) -> Result<HaltObservation, RunLoopError> {
        baud_vcpu::linux::run_until_halted(
            &mut self.guest.vcpu,
            &mut self.bus,
            &mut self.time,
            self.watchdog_budget,
            self.cancel.clone(),
        )?;
        Ok(HaltObservation {
            console_output: self.bus.console.output().to_vec(),
            exit_pc: self.current_rip()?,
        })
    }

    /// The vCPU's current RIP (`KVM_GET_REGS`) — the building block [`HaltOutcome::exit_pc`] and
    /// every other halt-site call below reads to name the exact instruction a shutdown landed at.
    fn current_rip(&self) -> Result<u64, DeterminismHole> {
        self.guest.vcpu.get_regs().map(|regs| regs.rip).map_err(|e| DeterminismHole(e.to_string()))
    }

    /// Every byte the guest has written to the console so far, in order — the live equivalent of
    /// [`HaltOutcome::console_output`] for a `Multiverse` that has not (and, for an interactive
    /// guest driven by [`step_exit`](Self::step_exit)/[`run_until_console_len`]
    /// (Self::run_until_console_len), may never) reach `Hlt`.
    pub fn console_output(&self) -> &[u8] {
        self.bus.console.output()
    }

    /// Queue `bytes` for the guest's next console reads (specs/baud-snapshot.md §5's "restore into
    /// a live shell") — [`Console::enqueue_input`] does the actual work; this just reaches through
    /// the device bus the way [`console_output`](Self::console_output) does for the output side.
    pub fn enqueue_console_input(&mut self, bytes: &[u8]) -> usize {
        self.bus.console.enqueue_input(bytes)
    }

    /// Drive exactly one `KVM_RUN` + dispatch cycle (`baud_vcpu::linux::run_one_exit`) without
    /// waiting for `Hlt` — the building block an interactive session needs instead of
    /// [`run_to_first_halt`](Self::run_to_first_halt), which by design stops there.
    pub fn step_exit(&mut self) -> Result<baud_vcpu::DispatchOutcome, DeterminismHole> {
        baud_vcpu::linux::run_one_exit(&mut self.guest.vcpu, &mut self.bus, &mut self.time)
    }

    /// Step the guest (via [`step_exit`](Self::step_exit)) until [`console_output`]
    /// (Self::console_output) reaches at least `target_len` bytes, or `max_exits` host-side steps
    /// have elapsed without getting there. A guest polling for input it has not yet received (e.g.
    /// `shell-guest`'s LSR poll loop, specs/baud-snapshot.md §5) takes a variable, but for a fixed
    /// guest image and a fixed input schedule fully deterministic, number of exits to produce its
    /// next byte of output — this is the live-session equivalent of
    /// [`run_to_first_halt`](Self::run_to_first_halt)'s "run until `Hlt`" for a guest that never
    /// halts. Returns `Err(DeterminismHole)` (reusing that type rather than inventing a new one, the
    /// same "no silent continuation" convention every other run-loop entry point here follows) if
    /// `max_exits` is exhausted first — a caller-supplied bound that is too tight to observe real,
    /// intended guest progress, not a determinism violation in itself.
    pub fn run_until_console_len(&mut self, target_len: usize, max_exits: u32) -> Result<(), RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut exits = 0u32;
        while self.console_output().len() < target_len {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            if exits >= max_exits {
                return Err(DeterminismHole(format!(
                    "run_until_console_len: {target_len} bytes not reached within {max_exits} exits \
                     (got {} bytes)",
                    self.console_output().len()
                ))
                .into());
            }
            self.step_exit_cancellable()?;
            exits += 1;
        }
        Ok(())
    }

    /// Step the guest (via [`step_exit`](Self::step_exit)) until it either halts or issues the
    /// tape device's `MARK_BRANCH` control op (specs/baud-tape-device.md §4's opcode 1) — the
    /// primitive todo.md's "M-series sixth brick" entry names as the concrete missing piece for
    /// real multi-level branch-tree growth: every guest fixture older than `mark-branch-guest`
    /// only ever halts on tape exhaustion, so there was nothing for a "stop before halt"
    /// primitive to stop at. Every tape-device record emitted along the way (including the
    /// terminating `MarkBranch`, if that's how the loop ends) is returned alongside the outcome —
    /// `drain_records` is called once per exit so nothing emitted between two exits is silently
    /// lost, mirroring [`run_until_console_len`](Self::run_until_console_len)'s "check a
    /// condition after every single exit" shape but swapping the stop condition. `max_exits`
    /// bounds a guest that never does either (returns `Err(DeterminismHole)`, the same "no silent
    /// non-termination" convention as `run_until_console_len`).
    pub fn run_until_branch_or_halt(
        &mut self,
        max_exits: u32,
    ) -> Result<(RunUntilBranchOutcome, Vec<baud_proto::Msg>), RunLoopError> {
        let (observed, records) = self.run_until_branch_or_halt_without_ram_hash(max_exits)?;
        Ok((self.observation_to_outcome(observed), records))
    }

    /// [`RunUntilBranchObservation`] -> [`RunUntilBranchOutcome`], filling in `ram_hash` (via
    /// [`ram_hash`](Self::ram_hash)) only for the `Halted` arm — `MarkBranch` never carried one.
    /// Shared by every `run_until_branch_or_halt*` wrapper that reconstructs the eager outcome on
    /// top of its own `_without_ram_hash` primitive.
    fn observation_to_outcome(&self, observed: RunUntilBranchObservation) -> RunUntilBranchOutcome {
        match observed {
            RunUntilBranchObservation::Halted(h) => RunUntilBranchOutcome::Halted(HaltOutcome {
                console_output: h.console_output,
                ram_hash: self.ram_hash(),
                exit_pc: h.exit_pc,
            }),
            RunUntilBranchObservation::MarkBranch { step } => RunUntilBranchOutcome::MarkBranch { step },
        }
    }

    /// [`run_until_branch_or_halt`](Self::run_until_branch_or_halt) without the guest-RAM hash on
    /// the `Halted` arm — same "skip the blake3 pass" trade [`run_to_first_halt_without_ram_hash`]
    /// (Self::run_to_first_halt_without_ram_hash) makes for [`run_to_first_halt`]
    /// (Self::run_to_first_halt), for a caller (todo.md §14.1 "still open" item 1's `run_kvm.rs`
    /// call sites) that runs many branches and reads the hash of only some. The hash is not lost by
    /// using this entry point: [`ram_hash`](Self::ram_hash) called afterwards on a `Halted` branch
    /// returns exactly what `run_until_branch_or_halt` would have put in
    /// [`HaltOutcome::ram_hash`], the same guarantee `run_to_first_halt_without_ram_hash`'s own doc
    /// makes.
    pub fn run_until_branch_or_halt_without_ram_hash(
        &mut self,
        max_exits: u32,
    ) -> Result<(RunUntilBranchObservation, Vec<baud_proto::Msg>), RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut exits = 0u32;
        let mut records = Vec::new();
        loop {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            if exits >= max_exits {
                return Err(DeterminismHole(format!(
                    "run_until_branch_or_halt: neither Hlt nor MARK_BRANCH within {max_exits} exits"
                ))
                .into());
            }
            let outcome = self.step_exit_cancellable()?;
            exits += 1;
            if matches!(outcome, baud_vcpu::DispatchOutcome::Halted) {
                let halt =
                    HaltObservation { console_output: self.bus.console.output().to_vec(), exit_pc: self.current_rip()? };
                return Ok((RunUntilBranchObservation::Halted(halt), records));
            }
            let mut drained = self.bus.tape.device_mut().drain_records();
            if let Some(pos) = drained.iter().position(|m| matches!(m, baud_proto::Msg::MarkBranch { .. })) {
                let step = match drained[pos] {
                    baud_proto::Msg::MarkBranch { step } => step,
                    _ => unreachable!("position() only matched MarkBranch entries"),
                };
                records.extend(drained.drain(..=pos));
                return Ok((RunUntilBranchObservation::MarkBranch { step }, records));
            }
            records.extend(drained);
        }
    }

    /// Deliver `vector` at the exact instruction boundary `period_rcb` retired conditional
    /// branches from now (H4, specs/baud-vcpu.md §5's arm-early-then-single-step engine, wired
    /// into a real guest's run loop for the first time): reads the work-clock's current RCB
    /// (`WorkClock::current_rcb`), builds a `baud_vcpu::linux::pmu::LinuxPmuStepper` over this
    /// `Multiverse`'s own vCPU/bus/time-source handles (so every exit taken while arming/stepping
    /// toward the target is still served deterministically, never skipped, and so the stepper
    /// polls this same `WorkClock`-backed RCB space directly rather than a second, independently-
    /// epoched counter of its own — todo.md §14 next-actions item 2(c)), then calls
    /// `boundary::inject_at`. Returns the exact `(rip, rcb)` the interrupt landed at — the tuple
    /// `timer_tick_lands_at_identical_instruction` compares across a double-run.
    pub fn inject_timer_tick(&mut self, period_rcb: u64, vector: u8) -> Result<TimerTick, RunLoopError> {
        let baseline = self.time.current_rcb();
        let target_rcb = baseline.saturating_add(period_rcb);
        let mut stepper = self.cancellable_stepper();
        let result = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, vector);
        // `stepper`'s borrows of this `Multiverse`'s fields end at its last use above, so
        // `stepper_error` (which needs `&self` to read the cancellation flag) is free to run here.
        let outcome = result.map_err(|e| self.stepper_error(e))?;
        match outcome {
            baud_vcpu::boundary::InjectOutcome::Injected(point) => {
                Ok(TimerTick { rip: point.rip, rcb: point.rcb })
            }
            baud_vcpu::boundary::InjectOutcome::Halted(point) => Err(DeterminismHole(format!(
                "inject_timer_tick: guest halted at rcb={} before reaching target_rcb={target_rcb} \
                 -- use run_to_first_halt_with_periodic_timer for a guest whose tick count is not \
                 known ahead of time",
                point.rcb
            ))
            .into()),
        }
    }

    /// Inject `num_ticks` timer ticks spaced `period_rcb` apart (via repeated
    /// [`inject_timer_tick`](Self::inject_timer_tick)), then drive the guest to its first
    /// `Hlt`/`Shutdown` exactly like [`run_to_first_halt`](Self::run_to_first_halt). The natural
    /// entry point for a guest fixture that survives more than one delivered interrupt before
    /// halting — `timer_tick_lands_at_identical_instruction` calls this twice on the same
    /// image+tape and compares every returned [`TimerTick`] pairwise across the two runs.
    ///
    /// Requires the caller to already know exactly how many ticks the guest survives —
    /// [`run_to_first_halt_with_periodic_timer`](Self::run_to_first_halt_with_periodic_timer) is
    /// the open-ended counterpart for a guest (a real kernel's scheduler, most concretely) whose
    /// tick count is not known ahead of time.
    pub fn run_with_timer_ticks(
        &mut self,
        period_rcb: u64,
        vector: u8,
        num_ticks: u32,
    ) -> Result<(Vec<TimerTick>, HaltOutcome), RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut ticks = Vec::with_capacity(num_ticks as usize);
        for _ in 0..num_ticks {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            ticks.push(self.inject_timer_tick(period_rcb, vector)?);
        }
        let halt = self.run_to_first_halt()?;
        Ok((ticks, halt))
    }

    /// Wire H4's interrupt-injection engine into an open-ended run loop (todo.md §14 "Guest boot
    /// pipeline": "wire H4 timer-interrupt injection into the boot path — an earlier attempt hung
    /// in `calibrate_delay()` waiting on a jiffies tick because injection wasn't wired in"). Unlike
    /// [`run_with_timer_ticks`](Self::run_with_timer_ticks), the caller does not need to know in
    /// advance how many ticks the guest survives: this repeatedly schedules a tick `period_rcb`
    /// retired conditional branches after the last one and delivers it (exactly
    /// [`inject_timer_tick`](Self::inject_timer_tick)'s arm-early-then-single-step engine, per
    /// tick), but instead of treating "the guest halted before the next tick's target" as an error,
    /// it is the expected, graceful end of the run — precisely the shape a real kernel's periodic
    /// scheduler timer needs: it keeps ticking indefinitely until the kernel itself decides to
    /// power off (§4.3's `reboot(RB_POWER_OFF)`/triple-fault path), not for a fixed, test-chosen
    /// tick count. `max_ticks` bounds a guest that never halts at all (the same "no silent non-
    /// termination" convention as [`run_until_console_len`](Self::run_until_console_len)) — reached
    /// only by a guest that genuinely never stops, not by the ordinary halting case.
    pub fn run_to_first_halt_with_periodic_timer(
        &mut self,
        period_rcb: u64,
        vector: u8,
        max_ticks: u32,
    ) -> Result<(Vec<TimerTick>, HaltOutcome), RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut ticks = Vec::new();
        for _ in 0..max_ticks {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            let baseline = self.time.current_rcb();
            let target_rcb = baseline.saturating_add(period_rcb);
            let mut stepper = self.cancellable_stepper();
            let result = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, vector);
            let outcome = result.map_err(|e| self.stepper_error(e))?;
            match outcome {
                baud_vcpu::boundary::InjectOutcome::Injected(point) => {
                    ticks.push(TimerTick { rip: point.rip, rcb: point.rcb });
                }
                // The guest halted on its own before this tick's target: the run is over. Do not
                // call `run_to_first_halt` here — the `Hlt`/`Shutdown` exit that set this was
                // already dispatched inside `inject_at`'s own `KVM_RUN` calls, so the bus/console
                // already reflect it; calling `KVM_RUN` again on an already-halted vCPU with no
                // in-kernel irqchip risks blocking indefinitely instead of re-observing `Hlt`.
                baud_vcpu::boundary::InjectOutcome::Halted(_) => {
                    let halt = HaltOutcome {
                        console_output: self.bus.console.output().to_vec(),
                        ram_hash: self.ram_hash(),
                        exit_pc: self.current_rip()?,
                    };
                    return Ok((ticks, halt));
                }
            }
        }
        Err(DeterminismHole(format!(
            "run_to_first_halt_with_periodic_timer: guest did not halt within {max_ticks} periodic ticks"
        ))
        .into())
    }

    /// [`run_to_first_halt_with_periodic_timer`](Self::run_to_first_halt_with_periodic_timer)'s
    /// open-ended engine, plus [`run_until_branch_or_halt`](Self::run_until_branch_or_halt)'s
    /// "stop at `MARK_BRANCH`, not just at `Hlt`" stop condition, combined for a real Linux guest
    /// whose tick count is not known ahead of time but which also issues a guest-driven checkpoint
    /// (todo.md's own spec for `double_boot_ram_hash_identical`, H7): ticks are injected exactly
    /// like the periodic-timer engine, but after *every* tick attempt — whether it ended in
    /// `Injected` or `Halted` — the tape device is drained and checked for a `MARK_BRANCH` record
    /// first, before deciding which of the two the attempt was. This ordering is load-bearing, not
    /// defensive: a short guest program (e.g. `checkpoint_init.c`, whose entire `MARK_BRANCH`-then-
    /// `Hlt` sequence is a handful of instructions) finishes well inside the very first tick's own
    /// `period_rcb`-wide window, so its `MARK_BRANCH` and its eventual halt both land inside the
    /// *same* `inject_at` call — checking `Halted` before draining the tape would silently discard
    /// the checkpoint record forever, never surface it, and the run would drive straight to the
    /// guest's own halt instead. Once found, the run stops there and reports `MarkBranch` even if
    /// the *same* tick attempt technically also observed `Halted` — the guest's `MARK_BRANCH`
    /// always precedes its own halt in program order, so the tape record capturing it exists
    /// regardless of which exit the vCPU is sitting at when this is checked, and a caller hashing
    /// RAM right here still observes state no later than that final small stretch of guest code
    /// (`sync()` + `reboot()`, in `checkpoint_init.c`'s case) rather than a wall-clock point or a
    /// full boot's raw console/RAM comparison (both of which embed real-hardware RCB/TSC read
    /// jitter, see `tests/fixtures/linux-guest/BUILD.md`'s "known, deliberate non-goal" section).
    /// Returns the tick trace, the stop condition, and every tape-device record drained along the
    /// way (not just a `MarkBranch` match) — earlier versions of this function called
    /// `drain_records()` once per tick purely to *look for* `MarkBranch` and threw the rest away,
    /// silently dropping any `PROBE`/`GOAL`/`VIOLATION`/`LOG` record a guest emitted on a tick that
    /// didn't also stop the run. Fixed to match [`run_until_branch_or_halt`](Self::
    /// run_until_branch_or_halt)'s own "accumulate every drained record" contract, since a caller
    /// scoring branches from these records (e.g. `baud-server`'s
    /// `run_driver_generated_branches_with_persist`) needs the guest's real probe stream, not just
    /// its stop condition.
    pub fn run_until_branch_or_halt_with_periodic_timer(
        &mut self,
        period_rcb: u64,
        vector: u8,
        max_ticks: u32,
    ) -> Result<(Vec<TimerTick>, RunUntilBranchOutcome, Vec<baud_proto::Msg>), RunLoopError> {
        let (ticks, observed, records) =
            self.run_until_branch_or_halt_with_periodic_timer_without_ram_hash(period_rcb, vector, max_ticks)?;
        Ok((ticks, self.observation_to_outcome(observed), records))
    }

    /// [`run_until_branch_or_halt_with_periodic_timer`]
    /// (Self::run_until_branch_or_halt_with_periodic_timer) without the guest-RAM hash on the
    /// `Halted` arm — the periodic-timer analogue of
    /// [`run_until_branch_or_halt_without_ram_hash`](Self::run_until_branch_or_halt_without_ram_hash).
    pub fn run_until_branch_or_halt_with_periodic_timer_without_ram_hash(
        &mut self,
        period_rcb: u64,
        vector: u8,
        max_ticks: u32,
    ) -> Result<(Vec<TimerTick>, RunUntilBranchObservation, Vec<baud_proto::Msg>), RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut ticks = Vec::new();
        let mut records = Vec::new();
        for _ in 0..max_ticks {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            let baseline = self.time.current_rcb();
            let target_rcb = baseline.saturating_add(period_rcb);
            let mut stepper = self.cancellable_stepper();
            let result = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, vector);
            let outcome = result.map_err(|e| self.stepper_error(e))?;
            let mut drained = self.bus.tape.device_mut().drain_records();
            if let Some(pos) = drained.iter().position(|m| matches!(m, baud_proto::Msg::MarkBranch { .. })) {
                let step = match drained[pos] {
                    baud_proto::Msg::MarkBranch { step } => step,
                    _ => unreachable!("position() only matched MarkBranch entries"),
                };
                records.extend(drained.drain(..=pos));
                return Ok((ticks, RunUntilBranchObservation::MarkBranch { step }, records));
            }
            records.extend(drained);
            match outcome {
                baud_vcpu::boundary::InjectOutcome::Injected(point) => {
                    ticks.push(TimerTick { rip: point.rip, rcb: point.rcb });
                }
                baud_vcpu::boundary::InjectOutcome::Halted(_) => {
                    let halt = HaltObservation {
                        console_output: self.bus.console.output().to_vec(),
                        exit_pc: self.current_rip()?,
                    };
                    return Ok((ticks, RunUntilBranchObservation::Halted(halt), records));
                }
            }
        }
        Err(DeterminismHole(format!(
            "run_until_branch_or_halt_with_periodic_timer: neither Hlt nor MARK_BRANCH within \
             {max_ticks} periodic ticks"
        ))
        .into())
    }

    /// Enable the virtio-rng device on this guest's device bus ([`DeviceBus::enable_virtio_rng`])
    /// — call before any guest code that probes for it runs. Every existing `boot`/`restore` call
    /// leaves it disabled by default, so nothing changes for a caller that never calls this.
    pub fn enable_virtio_rng(&mut self) {
        self.bus.enable_virtio_rng();
    }

    /// Seed the virtio-rng device's own tape-derived entropy stream
    /// ([`DeviceBus::seed_virtio_rng_entropy`]) — independent of the `rdrand`/`rdseed` substream
    /// and the boot `SETUP_RNG_SEED` (spec §3.8's domain-separation convention). Call once, right
    /// after [`enable_virtio_rng`](Self::enable_virtio_rng), before any guest code runs.
    pub fn seed_virtio_rng_entropy(&mut self, seed: u64) {
        self.bus.seed_virtio_rng_entropy(seed);
    }

    /// The virtio-rng transport's own state, if [`enable_virtio_rng`](Self::enable_virtio_rng) has
    /// been called — read access for a caller (or test) that wants to observe `notify_count`/
    /// `interrupt_status` without reaching into this `Multiverse`'s private device bus.
    pub fn virtio_rng(&self) -> Option<&VirtioMmioTransport> {
        self.bus.virtio_rng()
    }

    /// The dual-8259 PIC bookkeeping stub's current state (`crate::pic8259::Pic8259`) — read
    /// access for a caller/test that wants to confirm a guest's own `probe_8259A()`/
    /// `init_8259A()`/`enable_8259A_irq()` sequence, issued through real `IN`/`OUT` PIO exits,
    /// actually took effect.
    pub fn pic(&self) -> &crate::pic8259::Pic8259 {
        self.bus.pic()
    }

    /// The Local APIC MMIO bookkeeping stub's current state (`crate::lapic::LocalApic`) — mirrors
    /// [`pic`](Self::pic)'s read-access convention for a caller/test that wants to confirm a real
    /// guest's own `setup_local_APIC()`/`setup_APIC_timer()` sequence took effect.
    pub fn lapic(&self) -> &crate::lapic::LocalApic {
        self.bus.lapic()
    }

    /// Write the minimal ACPI table set ([`crate::acpi::write_acpi_tables`]: RSDP -> XSDT -> FADT +
    /// DSDT + MADT-with-one-LAPIC, todo.md §14 item 5(c)) into this booted guest's real memory —
    /// the real boot-path wiring [`crate::acpi`]'s own doc named as still open. Opt-in: call after
    /// [`boot`](Self::boot)/[`boot_with_rdseed_sites`](Self::boot_with_rdseed_sites) succeeds and
    /// before the first `KVM_RUN`/`run_to_first_halt*` call, on a guest whose cmdline sets
    /// `acpi=on` and whose kernel is `CONFIG_ACPI=y` — every existing caller that never calls this
    /// keeps booting with no ACPI tables present at all, exactly as before this method existed.
    pub fn write_acpi_tables(&self) -> Result<(), DeterminismHole> {
        crate::acpi::write_acpi_tables(&self.guest.guest_mem).map_err(|e| DeterminismHole(e.to_string()))
    }

    /// Enable the virtio-blk device on this guest's device bus
    /// ([`DeviceBus::enable_virtio_pci_blk`]) — call before any guest code that probes for it
    /// runs. `base_image` becomes the disk's read-only, content-addressed base (todo.md §14 item
    /// 5(b)); every guest write only ever lands in an in-memory copy-on-write overlay layered on
    /// top (`crate::virtio_blk::BlockBackingStore`). Every existing `boot`/`restore` call leaves
    /// this disabled by default, so nothing changes for a caller that never calls this.
    ///
    /// `base_image` is anything convertible into a [`crate::virtio_blk::BlockBase`] — a plain
    /// `Vec<u8>`, or a read-only memory map of an image file
    /// ([`BlockBase::mapped`](crate::virtio_blk::BlockBase::mapped)) for a base too large to want
    /// resident on the heap. Both are equally deterministic; see `BlockBase`'s own doc.
    pub fn enable_virtio_pci_blk(&mut self, base_image: impl Into<crate::virtio_blk::BlockBase>) {
        self.bus.enable_virtio_pci_blk(base_image);
    }

    /// The virtio-pci block-device transport's own state, if
    /// [`enable_virtio_pci_blk`](Self::enable_virtio_pci_blk) has been called — mirrors
    /// [`virtio_rng`](Self::virtio_rng)'s read-access convention.
    pub fn virtio_pci_blk(&self) -> Option<&crate::virtio_pci::VirtioPciTransport> {
        self.bus.virtio_pci_blk()
    }

    /// [`service_virtio_rng_interrupt`](Self::service_virtio_rng_interrupt)'s counterpart for the
    /// block device: drain any virtio-blk `QueueNotify`s since the last call
    /// ([`DeviceBus::service_virtio_blk`], servicing each request against the backing store) and,
    /// if at least one chain was actually drained, deliver a real interrupt at `vector` right now
    /// via the same exact-boundary engine ([`inject_timer_tick`](Self::inject_timer_tick),
    /// `period_rcb = 0`) — this is exactly what `specs/baud-ubuntu.md` §4's "block completion is
    /// delivered at a fixed work-clock boundary via the interrupt-injection engine (blkreplay-
    /// style), never on host-I/O return" means concretely: the backing store is already-resident
    /// host memory, so servicing a request is a synchronous memcpy with no real I/O latency to be
    /// deterministic *about* in the first place — completion timing is purely a function of when
    /// the next reachable work-clock boundary falls, the same "no new low-level primitive needed"
    /// reuse `service_virtio_rng_interrupt`'s own doc explains.
    ///
    /// Same "must not be called on an already-halted vCPU" caveat as
    /// [`service_virtio_rng_interrupt`](Self::service_virtio_rng_interrupt) —
    /// [`service_virtio_blk_interrupt_while_halted`](Self::service_virtio_blk_interrupt_while_halted)
    /// is that counterpart.
    pub fn service_virtio_blk_interrupt(&mut self, vector: u8) -> Result<u32, RunLoopError> {
        let processed = self
            .bus
            .service_virtio_blk(&self.guest.guest_mem)
            .map_err(|e| DeterminismHole(e.to_string()))?;
        if processed > 0 {
            self.inject_timer_tick(0, vector)?;
        }
        Ok(processed)
    }

    /// [`service_virtio_blk_interrupt`](Self::service_virtio_blk_interrupt)'s counterpart for a
    /// vCPU already sitting at a halted exit — mirrors
    /// [`service_virtio_rng_interrupt_while_halted`](Self::service_virtio_rng_interrupt_while_halted)
    /// exactly (see that method's doc for why a direct `KVM_SET_VCPU_EVENTS` + one
    /// [`step_exit`](Self::step_exit) is safe here: a real block-request completion wait
    /// (`wait_for_completion_io`) reaches the idle loop's `safe_halt()`, `RFLAGS.IF=1` guaranteed,
    /// the same as virtio-rng's `wait_for_completion_killable`).
    fn service_virtio_blk_interrupt_while_halted(&mut self, vector: u8) -> Result<u32, RunLoopError> {
        let processed = self
            .bus
            .service_virtio_blk(&self.guest.guest_mem)
            .map_err(|e| DeterminismHole(e.to_string()))?;
        if processed > 0 {
            let mut events = self.guest.vcpu.get_vcpu_events().map_err(|e| DeterminismHole(e.to_string()))?;
            events.interrupt.injected = 1;
            events.interrupt.nr = vector;
            events.interrupt.soft = 0;
            self.guest
                .vcpu
                .set_vcpu_events(&events)
                .map_err(|e| DeterminismHole(e.to_string()))?;
            self.step_exit_cancellable()?;
        }
        Ok(processed)
    }

    /// [`run_to_first_halt_with_virtio_rng`](Self::run_to_first_halt_with_virtio_rng)'s counterpart
    /// for the block device: drive the guest to its first `Hlt`/`Shutdown`, polling
    /// [`virtio_pci_blk`](Self::virtio_pci_blk)'s `notify_count()` after every exit and, whenever
    /// it changes, drain + deliver a real interrupt via
    /// [`service_virtio_blk_interrupt`](Self::service_virtio_blk_interrupt). Requires
    /// [`enable_virtio_pci_blk`](Self::enable_virtio_pci_blk) to already have been called; with
    /// virtio-blk never enabled this behaves exactly like `run_to_first_halt` (the `notify_count`
    /// poll is always `None`, so nothing is ever serviced). `max_exits` bounds a guest that never
    /// halts, same convention as every other run loop here.
    ///
    /// Combining this with the periodic-timer engine and/or virtio-rng (the way
    /// `run_to_first_halt_with_periodic_timer_and_virtio_rng` combines those two) is not yet
    /// implemented — a real Ubuntu boot will need periodic ticks, virtio-rng, *and* virtio-blk all
    /// at once, which the current one-combinator-per-combination approach does not scale to
    /// (todo.md §14 item 5(b)'s own note on this). Left as a follow-up once that combined boot is
    /// actually being driven, rather than speculatively generalized here.
    ///
    /// Errors are [`RunLoopError`]: a genuine [`DeterminismHole`] (including this loop's own
    /// `max_exits` bound), or [`RunLoopError::Cancelled`] if
    /// [`set_cancel_flag`](Self::set_cancel_flag)'s flag was set — checked once per iteration
    /// right next to the `max_exits` bound, and (via this loop's own
    /// [`CancelKicker`](baud_vcpu::linux::CancelKicker) plus the cancellation-aware step it drives
    /// every exit through) on the way out of a single `KVM_RUN` that would otherwise have run for
    /// far longer than the client was there for. Both are strictly *between* two guest
    /// instructions.
    pub fn run_to_first_halt_with_virtio_pci_blk(
        &mut self,
        vector: u8,
        max_exits: u32,
    ) -> Result<HaltOutcome, RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut exits = 0u32;
        let mut last_notify_count = self.virtio_pci_blk().map(|t| t.notify_count()).unwrap_or(0);
        loop {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            if exits >= max_exits {
                return Err(DeterminismHole(format!(
                    "run_to_first_halt_with_virtio_pci_blk: guest did not halt within {max_exits} exits"
                ))
                .into());
            }
            let outcome = self.step_exit_cancellable()?;
            exits += 1;
            if matches!(outcome, baud_vcpu::DispatchOutcome::Halted) {
                return Ok(HaltOutcome {
                    console_output: self.bus.console.output().to_vec(),
                    ram_hash: self.ram_hash(),
                    exit_pc: self.current_rip()?,
                });
            }
            let notify_count = self.virtio_pci_blk().map(|t| t.notify_count()).unwrap_or(0);
            if notify_count != last_notify_count {
                last_notify_count = notify_count;
                self.service_virtio_blk_interrupt(vector)?;
            }
        }
    }

    /// Drain any virtio-rng `QueueNotify`s since the last call ([`DeviceBus::service_virtio_rng`],
    /// given this guest's real memory) and, if at least one chain was actually drained, deliver a
    /// real interrupt at `vector` to this guest's vCPU right now — H4's exact-boundary engine
    /// ([`inject_timer_tick`](Self::inject_timer_tick)), used degenerately with `period_rcb = 0`:
    /// "the next reachable boundary", which for a guest that has not halted resolves to "as soon
    /// as the vCPU is ready for interrupt injection" (`ready_for_interrupt_injection`/
    /// `request_interrupt_window`, the same machinery every periodic-timer tick already uses — no
    /// new low-level primitive needed). Returns the same count `service_virtio_rng` does (`0` if
    /// nothing was drained, so no interrupt was staged either).
    ///
    /// Real-hardware-verified against `tests/fixtures/virtio-rng-guest/` — that fixture's own IDT
    /// gate proves `vector` genuinely reaches the guest's own registered ISR, not just that
    /// `KVM_SET_VCPU_EVENTS` was called (see `virtio_rng_interrupt_reaches_the_guests_own_isr`
    /// below and that fixture's `BUILD.md`). This closes the "interrupt delivery" half of todo.md
    /// §14 next-actions item 1's still-open virtio-rng gap — **not** the deeper, still-open
    /// question of which vector an *unmodified Linux* guest's real `virtio_mmio` driver would bind
    /// to via `request_irq()` (there is no IOAPIC/PIC here to resolve one dynamically, unlike the
    /// LAPIC timer's architecturally-fixed vector), nor boot/cmdline/CLI wiring — both remain open.
    ///
    /// **Must not be called while this guest's vCPU is already sitting at a halted exit** — every
    /// caller here drives it right after observing a `QueueNotify`-equivalent write, while the
    /// guest is still running, never after a `Hlt`/`Shutdown` dispatch. `inject_timer_tick`'s own
    /// doc (see `run_to_first_halt_with_periodic_timer`'s `Halted` arm above) explains why:
    /// re-entering `KVM_RUN` on an already-halted vCPU with no in-kernel irqchip and no interrupt
    /// yet staged risks blocking indefinitely instead of re-observing the halt.
    pub fn service_virtio_rng_interrupt(&mut self, vector: u8) -> Result<u32, RunLoopError> {
        let processed = self
            .bus
            .service_virtio_rng(&self.guest.guest_mem)
            .map_err(|e| DeterminismHole(e.to_string()))?;
        if processed > 0 {
            self.inject_timer_tick(0, vector)?;
        }
        Ok(processed)
    }

    /// [`service_virtio_rng_interrupt`](Self::service_virtio_rng_interrupt)'s counterpart for a
    /// vCPU a periodic-timer `inject_at` call has *already* reported
    /// [`Halted`](baud_vcpu::boundary::InjectOutcome::Halted) — the case that method's own doc
    /// says it "must not" be called in, because `inject_timer_tick`'s arm/single-step boundary
    /// dance (`inject_at`) returns `Halted` *before ever calling `PmuStepper::inject`* when the
    /// vCPU is already halted at entry (`crates/baud-vcpu/src/boundary.rs`), so routing through it
    /// here would silently stage nothing.
    ///
    /// A real Linux guest legitimately reaches exactly this state: `wait_for_completion_killable`
    /// (the virtio-rng driver's own read path, `drivers/char/hw_random/virtio-rng.c`) schedules
    /// out to the idle loop's `safe_halt()` (`sti; hlt`, guaranteeing `RFLAGS.IF=1` at the exact
    /// halt instant) while its own completion is still pending — discovered via this crate's first
    /// real (not hand-assembled) virtio-rng guest,
    /// `guest_virtio_mmio_rng_driver_reads_real_entropy_through_virtio_rng`. Because `IF=1` is
    /// guaranteed right there (unlike a vCPU an `inject_at` call is still actively single-stepping
    /// through, where injectability must be checked via `ready_for_interrupt_injection`/
    /// `request_interrupt_window`), staging the interrupt directly via `KVM_SET_VCPU_EVENTS` and
    /// re-entering with one plain [`step_exit`](Self::step_exit) is safe and mirrors exactly how
    /// real hardware wakes a halted core: the very next `KVM_RUN` delivers it and resumes.
    fn service_virtio_rng_interrupt_while_halted(&mut self, vector: u8) -> Result<u32, RunLoopError> {
        let processed = self
            .bus
            .service_virtio_rng(&self.guest.guest_mem)
            .map_err(|e| DeterminismHole(e.to_string()))?;
        if processed > 0 {
            let mut events = self.guest.vcpu.get_vcpu_events().map_err(|e| DeterminismHole(e.to_string()))?;
            events.interrupt.injected = 1;
            events.interrupt.nr = vector;
            events.interrupt.soft = 0;
            self.guest
                .vcpu
                .set_vcpu_events(&events)
                .map_err(|e| DeterminismHole(e.to_string()))?;
            self.step_exit_cancellable()?;
        }
        Ok(processed)
    }

    /// Drive the guest to its first `Hlt`/`Shutdown` (like [`run_to_first_halt`]
    /// (Self::run_to_first_halt)), but poll [`virtio_rng`](Self::virtio_rng)'s `notify_count()`
    /// after every single exit and, whenever it changes, drain + deliver a real interrupt via
    /// [`service_virtio_rng_interrupt`](Self::service_virtio_rng_interrupt) — the exact idiom
    /// `virtio_rng_interrupt_reaches_the_guests_own_isr`'s test loop already proved on real
    /// hardware, promoted here from a test-only loop to a real, reusable entry point so
    /// `baud-server`'s `/run/kvm` route (and the `baud run kvm` CLI) can boot any guest that talks
    /// the virtio-rng wire protocol, not just a Rust test calling `Multiverse` directly — todo.md
    /// §14 next-actions item 1's last-open "boot/cmdline/CLI wiring" gap.
    ///
    /// Requires [`enable_virtio_rng`](Self::enable_virtio_rng) (and typically
    /// [`seed_virtio_rng_entropy`](Self::seed_virtio_rng_entropy)) to already have been called;
    /// with virtio-rng never enabled this behaves exactly like `run_to_first_halt` (the
    /// `notify_count` poll is always `None`, so nothing is ever serviced). `max_exits` bounds a
    /// guest that never halts (the same "no silent non-termination" convention every other run
    /// loop here follows).
    ///
    /// This does **not** solve which vector an *unmodified Linux* guest's own `virtio_mmio` driver
    /// would resolve its cmdline IRQ number to via `request_irq()` (there is no IOAPIC/PIC here to
    /// resolve one dynamically) — `vector` is still caller-specified, exactly as the hand-
    /// assembled `virtio-rng-guest` fixture's own IDT gate is. That deeper question remains open.
    pub fn run_to_first_halt_with_virtio_rng(
        &mut self,
        vector: u8,
        max_exits: u32,
    ) -> Result<HaltOutcome, RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut exits = 0u32;
        let mut last_notify_count = self.virtio_rng().map(|t| t.notify_count()).unwrap_or(0);
        loop {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            if exits >= max_exits {
                return Err(DeterminismHole(format!(
                    "run_to_first_halt_with_virtio_rng: guest did not halt within {max_exits} exits"
                ))
                .into());
            }
            let outcome = self.step_exit_cancellable()?;
            exits += 1;
            if matches!(outcome, baud_vcpu::DispatchOutcome::Halted) {
                return Ok(HaltOutcome {
                    console_output: self.bus.console.output().to_vec(),
                    ram_hash: self.ram_hash(),
                    exit_pc: self.current_rip()?,
                });
            }
            let notify_count = self.virtio_rng().map(|t| t.notify_count()).unwrap_or(0);
            if notify_count != last_notify_count {
                last_notify_count = notify_count;
                self.service_virtio_rng_interrupt(vector)?;
            }
        }
    }

    /// [`run_to_first_halt_with_periodic_timer`](Self::run_to_first_halt_with_periodic_timer)'s
    /// open-ended periodic-timer engine, generalized to poll-and-service any number of
    /// [`TickPolledDevice`]s once per delivered tick (not once per host-side exit — coarser-
    /// grained, since a real kernel guest's own ticks are already frequent relative to how often a
    /// device actually needs draining). `devices` is empty for the plain single-device wrappers'
    /// bare periodic-timer case and has one or more entries for every combined variant below; the
    /// per-device notify-count-changed check and the running/halted routing are unchanged from the
    /// single-device (virtio-rng-only) loop this replaces — with exactly one device in the slice
    /// this is behaviorally identical to that hand-written loop, so every existing caller/test is a
    /// regression check on the refactor itself, not just on the new multi-device case.
    ///
    /// `pattern`, when `Some` (H9 todo.md §14 item 12's "run until console contains X, resuming
    /// across idle halts" gap — a real kernel's idle loop halts the instant nothing is runnable,
    /// long before any device necessarily has pending work again), changes what a halt with no
    /// device work pending means: instead of being terminal, the timer channel itself is always
    /// serviced (there is always a next tick, unlike a device that can run out of work) via the
    /// same directly-staged-while-halted idiom
    /// [`service_virtio_rng_interrupt_while_halted`](Self::service_virtio_rng_interrupt_while_halted)
    /// established for one device, then the guest is driven natively
    /// ([`step_exit`](Self::step_exit)) until it halts again or `pattern` appears in the console
    /// stream, whichever comes first (bounded by `max_exits_per_burst`, since one delivered tick's
    /// burst of guest work can take far more than one host-side exit). `pattern` is checked after
    /// every single exit, not just at each halt, so a match produced mid-burst is never missed.
    /// `None` preserves this function's exact prior behavior (a halt with no device work pending
    /// is terminal) — every existing caller below passes `None`.
    ///
    /// Errors are [`RunLoopError`]: a genuine [`DeterminismHole`] (including this loop's own
    /// `max_ticks`/`max_exits_per_burst` bounds), or [`RunLoopError::Cancelled`] if
    /// [`set_cancel_flag`](Self::set_cancel_flag)'s flag was set. The flag is checked at the top
    /// of *both* loops here — once per periodic tick, and once per exit inside a delivered tick's
    /// burst, because a single tick's burst can run for thousands of exits and a per-tick check
    /// alone would leave a cancelled run driving the guest for an unbounded time. Neither check is
    /// sufficient on its own either: one tick's `inject_at` can sit inside a single multi-minute
    /// `KVM_RUN`, which is why this loop also arms a
    /// [`CancelKicker`](baud_vcpu::linux::CancelKicker) and hands its flag to the stepper
    /// ([`cancellable_stepper`](Self::cancellable_stepper)) and to every burst exit
    /// ([`step_exit_cancellable`](Self::step_exit_cancellable)).
    #[allow(clippy::too_many_arguments)]
    fn run_to_first_halt_with_periodic_timer_and_devices(
        &mut self,
        period_rcb: u64,
        timer_vector: u8,
        devices: &[TickPolledDevice],
        max_ticks: u32,
        pattern: Option<&[u8]>,
        max_exits_per_burst: u32,
    ) -> Result<(Vec<TimerTick>, HaltOutcome), RunLoopError> {
        if let Some(p) = pattern {
            if p.is_empty() {
                return Err(DeterminismHole(
                    "run_to_first_halt_with_periodic_timer_and_devices: pattern must not be empty"
                        .to_string(),
                )
                .into());
            }
        }
        let contains_pattern =
            |console: &[u8], p: &[u8]| !p.is_empty() && console.windows(p.len()).any(|w| w == p);
        let halt_outcome = |this: &mut Self| -> Result<HaltOutcome, DeterminismHole> {
            Ok(HaltOutcome {
                console_output: this.bus.console.output().to_vec(),
                ram_hash: this.ram_hash(),
                exit_pc: this.current_rip()?,
            })
        };
        let mut ticks = Vec::new();
        let _kicker = self.arm_cancel_kicker();
        let mut last_notify: Vec<u64> =
            devices.iter().map(|d| (d.notify_count)(self).unwrap_or(0)).collect();
        let progress_start = std::time::Instant::now();
        for tick_index in 0..max_ticks {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            if tick_index % RUN_LOOP_PROGRESS_LOG_INTERVAL_TICKS == 0 {
                info!(
                    "run_to_first_halt_with_periodic_timer_and_devices: tick {tick_index}/{max_ticks}, \
                     console_output {} bytes, {:.1}s elapsed",
                    self.bus.console.output().len(),
                    progress_start.elapsed().as_secs_f64(),
                );
            }
            let baseline = self.time.current_rcb();
            let target_rcb = baseline.saturating_add(period_rcb);
            // See `PERIODIC_TICK_WATCHDOG_BUDGET`'s doc: `inject_at`'s coarse phase can block one
            // `KVM_RUN` for as long as the guest runs natively with no vmexit, so this tick gets
            // its own bounded watchdog rather than inheriting only the whole-run cancellation flag
            // `cancellable_stepper` already wires in. Always disarmed before this iteration acts
            // on `result`, on every path (`Watchdog`'s own doc: a late-firing signal must never
            // land in unrelated future work on this thread).
            let tick_watchdog_budget = self.periodic_tick_watchdog_budget;
            let tick_watchdog = baud_vcpu::linux::Watchdog::arm(tick_watchdog_budget);
            let mut stepper = self
                .cancellable_stepper()
                .with_watchdog(Some(std::sync::Arc::clone(&tick_watchdog.fired)));
            let inject_at_start = std::time::Instant::now();
            let result = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, timer_vector);
            let inject_at_elapsed = inject_at_start.elapsed();
            let tick_timed_out = tick_watchdog.fired.load(std::sync::atomic::Ordering::SeqCst);
            tick_watchdog.disarm();
            if inject_at_elapsed >= SLOW_TICK_PHASE_LOG_THRESHOLD {
                info!(
                    "run_to_first_halt_with_periodic_timer_and_devices: tick {tick_index}/{max_ticks} \
                     inject_at took {:.1}s (of a {:.0}s watchdog budget)",
                    inject_at_elapsed.as_secs_f64(),
                    tick_watchdog_budget.as_secs_f64(),
                );
            }
            let outcome = result.map_err(|e| {
                // A real supervisor cancellation takes priority even if this tick's watchdog also
                // happened to fire in the same window — `stepper_error` already makes that same
                // cancel-first check via `is_cancelled_error`/`self.is_cancelled()`.
                if tick_timed_out && !self.is_cancelled() {
                    // Best-effort `KVM_GET_REGS` right here, before `e` (the raw ioctl error) is
                    // discarded — todo.md §14.2 H9 item 20's own named next diagnostic: the guest's
                    // own RIP is what distinguishes a merely-slow native stretch from a genuinely
                    // wedged one, which no host-side `gdb` backtrace can show.
                    RunLoopError::WatchdogKilled {
                        budget_ms: tick_watchdog_budget.as_millis() as u64,
                        guest_rip: self.guest.vcpu.get_regs().ok().map(|regs| regs.rip),
                        console_tail: Some(console_tail(self.bus.console.output())),
                    }
                } else {
                    self.stepper_error(e)
                }
            })?;
            match outcome {
                baud_vcpu::boundary::InjectOutcome::Injected(point) => {
                    ticks.push(TimerTick { rip: point.rip, rcb: point.rcb });
                    for (i, dev) in devices.iter().enumerate() {
                        let notify_count = (dev.notify_count)(self).unwrap_or(0);
                        if notify_count != last_notify[i] {
                            last_notify[i] = notify_count;
                            let service_start = std::time::Instant::now();
                            (dev.service_running)(self, dev.vector)?;
                            let service_elapsed = service_start.elapsed();
                            if service_elapsed >= SLOW_TICK_PHASE_LOG_THRESHOLD {
                                info!(
                                    "run_to_first_halt_with_periodic_timer_and_devices: tick \
                                     {tick_index}/{max_ticks} device[{i}].service_running took {:.1}s",
                                    service_elapsed.as_secs_f64(),
                                );
                            }
                        }
                    }
                    if let Some(p) = pattern {
                        if contains_pattern(self.bus.console.output(), p) {
                            return Ok((ticks, halt_outcome(self)?));
                        }
                    }
                }
                baud_vcpu::boundary::InjectOutcome::Halted(_) => {
                    // A real Linux guest can legitimately be sitting here waiting for exactly the
                    // completion one of these devices is meant to deliver (`wait_for_completion_
                    // killable`/`wait_for_completion_io`'s own `safe_halt()`, todo.md §14) -- not
                    // just at its final shutdown. Only when *no* device has a pending notification
                    // is a halt actually terminal; otherwise wake it via whichever device(s) do
                    // (`service_halted`, the halted-safe counterpart to `service_running` above)
                    // and keep ticking so a still-pending device on a later tick gets its turn too.
                    let mut serviced_any = false;
                    for (i, dev) in devices.iter().enumerate() {
                        let notify_count = (dev.notify_count)(self).unwrap_or(0);
                        if notify_count != last_notify[i] {
                            last_notify[i] = notify_count;
                            let service_start = std::time::Instant::now();
                            (dev.service_halted)(self, dev.vector)?;
                            let service_elapsed = service_start.elapsed();
                            if service_elapsed >= SLOW_TICK_PHASE_LOG_THRESHOLD {
                                info!(
                                    "run_to_first_halt_with_periodic_timer_and_devices: tick \
                                     {tick_index}/{max_ticks} device[{i}].service_halted took {:.1}s",
                                    service_elapsed.as_secs_f64(),
                                );
                            }
                            serviced_any = true;
                        }
                    }
                    if serviced_any {
                        if let Some(p) = pattern {
                            if contains_pattern(self.bus.console.output(), p) {
                                return Ok((ticks, halt_outcome(self)?));
                            }
                        }
                        continue;
                    }
                    let Some(p) = pattern else {
                        return Ok((ticks, halt_outcome(self)?));
                    };
                    // No device has pending work, but the caller wants to keep going until the
                    // console shows `p` -- the timer channel always has a next tick to offer
                    // (unlike a device), so deliver it directly (safe: `safe_halt()` guarantees
                    // `RFLAGS.IF=1` right here) and drain forced exits until the guest halts again
                    // or `p` appears.
                    let mut events =
                        self.guest.vcpu.get_vcpu_events().map_err(|e| DeterminismHole(e.to_string()))?;
                    events.interrupt.injected = 1;
                    events.interrupt.nr = timer_vector;
                    events.interrupt.soft = 0;
                    self.guest.vcpu.set_vcpu_events(&events).map_err(|e| DeterminismHole(e.to_string()))?;
                    let mut burst_exits = 0u32;
                    loop {
                        if self.is_cancelled() {
                            return Err(RunLoopError::Cancelled);
                        }
                        if contains_pattern(self.bus.console.output(), p) {
                            return Ok((ticks, halt_outcome(self)?));
                        }
                        if burst_exits >= max_exits_per_burst {
                            return Err(DeterminismHole(format!(
                                "run_to_first_halt_with_periodic_timer_and_devices: guest did not \
                                 halt again within {max_exits_per_burst} exits of one delivered tick \
                                 (console tail: {:?})",
                                console_tail(self.bus.console.output())
                            ))
                            .into());
                        }
                        // Same per-call watchdog dance as the tick loop's own `inject_at` call
                        // above (see `PERIODIC_TICK_WATCHDOG_BUDGET`'s doc): a guest woken from a
                        // non-terminal halt can run natively for as long as it likes before its
                        // next exit, with nothing else bounding a single `step_exit_cancellable`
                        // call here otherwise (todo.md §14 item 17's real finding — a genuine H9
                        // stall lived in exactly this loop, a code path distinct from `inject_at`
                        // that the per-tick watchdog above never covered).
                        let burst_watchdog = baud_vcpu::linux::Watchdog::arm(tick_watchdog_budget);
                        let dispatch = self.step_exit_cancellable_with_watchdog(&burst_watchdog.fired);
                        let burst_timed_out = burst_watchdog.fired.load(std::sync::atomic::Ordering::SeqCst);
                        burst_watchdog.disarm();
                        let dispatch = dispatch.map_err(|e| {
                            if burst_timed_out && !self.is_cancelled() {
                                // Same best-effort capture as the tick-level watchdog above (todo.md
                                // §14.2 H9 item 20) — this is the call site item 18/20 actually
                                // traced a real H9 stall to, so this is the one most likely to ever
                                // fire against a real boot.
                                RunLoopError::WatchdogKilled {
                                    budget_ms: tick_watchdog_budget.as_millis() as u64,
                                    guest_rip: self.guest.vcpu.get_regs().ok().map(|regs| regs.rip),
                                    console_tail: Some(console_tail(self.bus.console.output())),
                                }
                            } else {
                                e
                            }
                        })?;
                        burst_exits += 1;
                        if matches!(dispatch, baud_vcpu::DispatchOutcome::Halted) {
                            break;
                        }
                        // todo.md §14.2 H9 items 20/21/22's own flagged gap: this drain loop used
                        // to check only cancellation/pattern/burst-count, never `devices` — a
                        // completion arriving mid-burst (the guest running, not halted, between
                        // two raw exits) went unserviced until the *next* periodic tick's Injected/
                        // Halted arms noticed it, or forever if the guest never reaches another
                        // tick boundary. Mirrors the Injected arm's own per-exit notify-count poll
                        // above, at burst-loop granularity instead of tick granularity.
                        for (i, dev) in devices.iter().enumerate() {
                            let notify_count = (dev.notify_count)(self).unwrap_or(0);
                            if notify_count != last_notify[i] {
                                last_notify[i] = notify_count;
                                let service_start = std::time::Instant::now();
                                (dev.service_running)(self, dev.vector)?;
                                let service_elapsed = service_start.elapsed();
                                if service_elapsed >= SLOW_TICK_PHASE_LOG_THRESHOLD {
                                    info!(
                                        "run_to_first_halt_with_periodic_timer_and_devices: tick \
                                         {tick_index}/{max_ticks} burst device[{i}].service_running \
                                         took {:.1}s",
                                        service_elapsed.as_secs_f64(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(DeterminismHole(match pattern {
            Some(_) => format!(
                "run_to_first_halt_with_periodic_timer_and_devices: pattern not found within \
                 {max_ticks} periodic ticks (console tail: {:?})",
                console_tail(self.bus.console.output())
            ),
            None => format!(
                "run_to_first_halt_with_periodic_timer_and_devices: guest did not halt within \
                 {max_ticks} periodic ticks"
            ),
        })
        .into())
    }

    /// H9's last recorded open blocker (todo.md §14 item 12): every `run_to_first_halt_with_*`
    /// combinator treats a guest's own `Hlt` as terminal the instant no polled device has pending
    /// work — correct for every fixture built so far (each halts for good, once), but wrong for a
    /// real multi-tasking kernel, whose idle loop calls `hlt` (via `safe_halt()`, i.e. with
    /// `RFLAGS.IF=1`) the moment nothing is runnable and relies on the *next* periodic timer
    /// interrupt alone to reschedule — observed for real booting the actual Ubuntu 18.04.1 image
    /// (item 12): the boot reached `Freeing unused kernel memory` and then stopped, not because it
    /// crashed, but because `/init` blocked on its first disk read and the idle loop's very first
    /// `hlt` was reported as the run's terminal halt.
    ///
    /// A thin wrapper over [`run_to_first_halt_with_periodic_timer_and_devices`]
    /// (Self::run_to_first_halt_with_periodic_timer_and_devices)'s new `pattern` parameter (see its
    /// doc for the actual halted-timer-wake mechanism) with an empty device list — the bare
    /// periodic-timer case, for a guest that needs no virtio-rng/virtio-blk. See
    /// [`run_until_console_pattern_with_periodic_timer_and_devices`]
    /// (Self::run_until_console_pattern_with_periodic_timer_and_devices) for the combined-device
    /// sibling a real Ubuntu boot (which also needs virtio-blk for its root filesystem) requires.
    pub fn run_until_console_pattern_with_periodic_timer(
        &mut self,
        period_rcb: u64,
        vector: u8,
        pattern: &[u8],
        max_ticks: u32,
        max_exits_per_burst: u32,
    ) -> Result<(Vec<TimerTick>, HaltOutcome), RunLoopError> {
        self.run_to_first_halt_with_periodic_timer_and_devices(
            period_rcb,
            vector,
            &[],
            max_ticks,
            Some(pattern),
            max_exits_per_burst,
        )
    }

    /// [`run_until_console_pattern_with_periodic_timer`]
    /// (Self::run_until_console_pattern_with_periodic_timer)'s combined-device sibling — the entry
    /// point a real Ubuntu 18.04.1 boot (`specs/baud-ubuntu.md`) needs to reach the console login
    /// prompt, since that guest needs the periodic-timer engine for `calibrate_delay` regardless,
    /// optionally reads entropy from virtio-rng, and optionally mounts its root filesystem from
    /// virtio-blk. `virtio_rng_vector`/`virtio_blk_vector` are each `Some` exactly when the
    /// corresponding device was enabled (mirrors `boot_run_and_drain`'s existing dispatch
    /// convention for the non-pattern combinators) — `None` for both is exactly
    /// [`run_until_console_pattern_with_periodic_timer`]
    /// (Self::run_until_console_pattern_with_periodic_timer)'s own bare case, reimplemented here on
    /// the same shared engine rather than duplicated.
    #[allow(clippy::too_many_arguments)]
    pub fn run_until_console_pattern_with_periodic_timer_and_devices(
        &mut self,
        period_rcb: u64,
        timer_vector: u8,
        virtio_rng_vector: Option<u8>,
        virtio_blk_vector: Option<u8>,
        pattern: &[u8],
        max_ticks: u32,
        max_exits_per_burst: u32,
    ) -> Result<(Vec<TimerTick>, HaltOutcome), RunLoopError> {
        let mut devices = Vec::new();
        if let Some(vector) = virtio_rng_vector {
            devices.push(TickPolledDevice {
                vector,
                notify_count: |mv| mv.virtio_rng().map(|t| t.notify_count()),
                service_running: Multiverse::service_virtio_rng_interrupt,
                service_halted: Multiverse::service_virtio_rng_interrupt_while_halted,
            });
        }
        if let Some(vector) = virtio_blk_vector {
            devices.push(TickPolledDevice {
                vector,
                notify_count: |mv| mv.virtio_pci_blk().map(|t| t.notify_count()),
                service_running: Multiverse::service_virtio_blk_interrupt,
                service_halted: Multiverse::service_virtio_blk_interrupt_while_halted,
            });
        }
        self.run_to_first_halt_with_periodic_timer_and_devices(
            period_rcb,
            timer_vector,
            &devices,
            max_ticks,
            Some(pattern),
            max_exits_per_burst,
        )
    }

    /// [`run_to_first_halt_with_periodic_timer`](Self::run_to_first_halt_with_periodic_timer)'s
    /// open-ended periodic-timer engine, combined with [`run_to_first_halt_with_virtio_rng`]
    /// (Self::run_to_first_halt_with_virtio_rng)'s notify-and-service polling — the entry point a
    /// real Linux guest needs when it requires periodic timer ticks for `calibrate_delay` **and**
    /// talks to virtio-rng, since the two open-ended loops above are each exhaustive on their own
    /// (a real kernel needs the periodic-timer engine regardless of virtio-rng). Now a thin
    /// one-device wrapper over [`run_to_first_halt_with_periodic_timer_and_devices`]
    /// (Self::run_to_first_halt_with_periodic_timer_and_devices); see
    /// [`run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk`]
    /// (Self::run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk) for the
    /// three-device sibling this generalization exists to make possible.
    pub fn run_to_first_halt_with_periodic_timer_and_virtio_rng(
        &mut self,
        period_rcb: u64,
        timer_vector: u8,
        virtio_rng_vector: u8,
        max_ticks: u32,
    ) -> Result<(Vec<TimerTick>, HaltOutcome), RunLoopError> {
        self.run_to_first_halt_with_periodic_timer_and_devices(
            period_rcb,
            timer_vector,
            &[TickPolledDevice {
                vector: virtio_rng_vector,
                notify_count: |mv| mv.virtio_rng().map(|t| t.notify_count()),
                service_running: Multiverse::service_virtio_rng_interrupt,
                service_halted: Multiverse::service_virtio_rng_interrupt_while_halted,
            }],
            max_ticks,
            None,
            0,
        )
    }

    /// The three-way combination `todo.md` §14 item 5(b)'s note on
    /// [`run_to_first_halt_with_virtio_pci_blk`](Self::run_to_first_halt_with_virtio_pci_blk) named
    /// as not yet implemented: periodic timer ticks, virtio-rng, and virtio-blk all serviced within
    /// one run loop — the combination a real Ubuntu 18.04.1 boot (`specs/baud-ubuntu.md`) needs,
    /// since that guest requires the periodic-timer engine for `calibrate_delay` regardless, reads
    /// entropy from virtio-rng, and mounts its root filesystem from virtio-blk. Built entirely on
    /// [`run_to_first_halt_with_periodic_timer_and_devices`]
    /// (Self::run_to_first_halt_with_periodic_timer_and_devices) — no new run-loop logic, only a
    /// second [`TickPolledDevice`] describing virtio-blk the same way the rng-only wrapper above
    /// describes virtio-rng. Requires [`enable_virtio_rng`](Self::enable_virtio_rng) and
    /// [`enable_virtio_pci_blk`](Self::enable_virtio_pci_blk) to already have been called; either
    /// one left unenabled behaves exactly like the two-device wrapper above (its `notify_count`
    /// poll is always `None`, so it is never serviced).
    pub fn run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk(
        &mut self,
        period_rcb: u64,
        timer_vector: u8,
        virtio_rng_vector: u8,
        virtio_blk_vector: u8,
        max_ticks: u32,
    ) -> Result<(Vec<TimerTick>, HaltOutcome), RunLoopError> {
        self.run_to_first_halt_with_periodic_timer_and_devices(
            period_rcb,
            timer_vector,
            &[
                TickPolledDevice {
                    vector: virtio_rng_vector,
                    notify_count: |mv| mv.virtio_rng().map(|t| t.notify_count()),
                    service_running: Multiverse::service_virtio_rng_interrupt,
                    service_halted: Multiverse::service_virtio_rng_interrupt_while_halted,
                },
                TickPolledDevice {
                    vector: virtio_blk_vector,
                    notify_count: |mv| mv.virtio_pci_blk().map(|t| t.notify_count()),
                    service_running: Multiverse::service_virtio_blk_interrupt,
                    service_halted: Multiverse::service_virtio_blk_interrupt_while_halted,
                },
            ],
            max_ticks,
            None,
            0,
        )
    }

    /// [`run_until_branch_or_halt`](Self::run_until_branch_or_halt)'s per-exit "stop at
    /// `MARK_BRANCH`, not just at `Hlt`" condition, combined with
    /// [`run_to_first_halt_with_virtio_rng`](Self::run_to_first_halt_with_virtio_rng)'s
    /// `notify_count` poll-and-service loop — the entry point `baud-server`'s `/run/kvm/branch` and
    /// `/run/kvm/resume` routes need to drive a virtio-rng-enabled branch to its own checkpoint or
    /// halt (todo.md §14 next-actions item 1's last-open virtio-rng gap: branch/resume/restore
    /// never accepted `virtio_rng` at all because this combinator didn't exist). Requires
    /// [`enable_virtio_rng`](Self::enable_virtio_rng) (and typically
    /// [`seed_virtio_rng_entropy`](Self::seed_virtio_rng_entropy)) to already have been called on
    /// this `Multiverse` — virtio-rng device state is not itself part of the snapshot/restore/branch
    /// contract (`DeviceBus::restore` always starts a branched universe with the device disabled),
    /// so a caller must re-enable and re-seed it fresh right after [`Multiverse::branch`], exactly
    /// like a cold boot.
    pub fn run_until_branch_or_halt_with_virtio_rng(
        &mut self,
        vector: u8,
        max_exits: u32,
    ) -> Result<(RunUntilBranchOutcome, Vec<baud_proto::Msg>), RunLoopError> {
        let (observed, records) = self.run_until_branch_or_halt_with_virtio_rng_without_ram_hash(vector, max_exits)?;
        Ok((self.observation_to_outcome(observed), records))
    }

    /// [`run_until_branch_or_halt_with_virtio_rng`](Self::run_until_branch_or_halt_with_virtio_rng)
    /// without the guest-RAM hash on the `Halted` arm — the virtio-rng analogue of
    /// [`run_until_branch_or_halt_without_ram_hash`](Self::run_until_branch_or_halt_without_ram_hash).
    pub fn run_until_branch_or_halt_with_virtio_rng_without_ram_hash(
        &mut self,
        vector: u8,
        max_exits: u32,
    ) -> Result<(RunUntilBranchObservation, Vec<baud_proto::Msg>), RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut exits = 0u32;
        let mut records = Vec::new();
        let mut last_notify_count = self.virtio_rng().map(|t| t.notify_count()).unwrap_or(0);
        loop {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            if exits >= max_exits {
                return Err(DeterminismHole(format!(
                    "run_until_branch_or_halt_with_virtio_rng: neither Hlt nor MARK_BRANCH within \
                     {max_exits} exits"
                ))
                .into());
            }
            let outcome = self.step_exit_cancellable()?;
            exits += 1;
            if matches!(outcome, baud_vcpu::DispatchOutcome::Halted) {
                let halt = HaltObservation {
                    console_output: self.bus.console.output().to_vec(),
                    exit_pc: self.current_rip()?,
                };
                return Ok((RunUntilBranchObservation::Halted(halt), records));
            }
            let mut drained = self.bus.tape.device_mut().drain_records();
            if let Some(pos) = drained.iter().position(|m| matches!(m, baud_proto::Msg::MarkBranch { .. })) {
                let step = match drained[pos] {
                    baud_proto::Msg::MarkBranch { step } => step,
                    _ => unreachable!("position() only matched MarkBranch entries"),
                };
                records.extend(drained.drain(..=pos));
                return Ok((RunUntilBranchObservation::MarkBranch { step }, records));
            }
            records.extend(drained);
            let notify_count = self.virtio_rng().map(|t| t.notify_count()).unwrap_or(0);
            if notify_count != last_notify_count {
                last_notify_count = notify_count;
                self.service_virtio_rng_interrupt(vector)?;
            }
        }
    }

    /// [`run_until_branch_or_halt_with_periodic_timer`]
    /// (Self::run_until_branch_or_halt_with_periodic_timer)'s per-tick "stop at `MARK_BRANCH`, not
    /// just at `Hlt`" checkpoint engine, combined with
    /// [`run_to_first_halt_with_periodic_timer_and_virtio_rng`]
    /// (Self::run_to_first_halt_with_periodic_timer_and_virtio_rng)'s `notify_count` poll-and-
    /// service — the four-way combination `/run/kvm/branch`/`/run/kvm/resume` need for a branch
    /// that both requires periodic timer ticks and talks virtio-rng. The `MARK_BRANCH` drain-and-
    /// check happens before the `Injected`/`Halted` match on every tick, exactly matching
    /// `run_until_branch_or_halt_with_periodic_timer`'s own load-bearing ordering (a short guest
    /// program's entire checkpoint-then-halt sequence can land inside one tick's window).
    pub fn run_until_branch_or_halt_with_periodic_timer_and_virtio_rng(
        &mut self,
        period_rcb: u64,
        timer_vector: u8,
        virtio_rng_vector: u8,
        max_ticks: u32,
    ) -> Result<(Vec<TimerTick>, RunUntilBranchOutcome, Vec<baud_proto::Msg>), RunLoopError> {
        let (ticks, observed, records) = self
            .run_until_branch_or_halt_with_periodic_timer_and_virtio_rng_without_ram_hash(
                period_rcb,
                timer_vector,
                virtio_rng_vector,
                max_ticks,
            )?;
        Ok((ticks, self.observation_to_outcome(observed), records))
    }

    /// [`run_until_branch_or_halt_with_periodic_timer_and_virtio_rng`]
    /// (Self::run_until_branch_or_halt_with_periodic_timer_and_virtio_rng) without the guest-RAM
    /// hash on the `Halted` arm — the four-way analogue of
    /// [`run_until_branch_or_halt_without_ram_hash`](Self::run_until_branch_or_halt_without_ram_hash).
    pub fn run_until_branch_or_halt_with_periodic_timer_and_virtio_rng_without_ram_hash(
        &mut self,
        period_rcb: u64,
        timer_vector: u8,
        virtio_rng_vector: u8,
        max_ticks: u32,
    ) -> Result<(Vec<TimerTick>, RunUntilBranchObservation, Vec<baud_proto::Msg>), RunLoopError> {
        let _kicker = self.arm_cancel_kicker();
        let mut ticks = Vec::new();
        let mut records = Vec::new();
        let mut last_notify_count = self.virtio_rng().map(|t| t.notify_count()).unwrap_or(0);
        for _ in 0..max_ticks {
            if self.is_cancelled() {
                return Err(RunLoopError::Cancelled);
            }
            let baseline = self.time.current_rcb();
            let target_rcb = baseline.saturating_add(period_rcb);
            let mut stepper = self.cancellable_stepper();
            let result = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, timer_vector);
            let outcome = result.map_err(|e| self.stepper_error(e))?;
            let mut drained = self.bus.tape.device_mut().drain_records();
            if let Some(pos) = drained.iter().position(|m| matches!(m, baud_proto::Msg::MarkBranch { .. })) {
                let step = match drained[pos] {
                    baud_proto::Msg::MarkBranch { step } => step,
                    _ => unreachable!("position() only matched MarkBranch entries"),
                };
                records.extend(drained.drain(..=pos));
                return Ok((ticks, RunUntilBranchObservation::MarkBranch { step }, records));
            }
            records.extend(drained);
            match outcome {
                baud_vcpu::boundary::InjectOutcome::Injected(point) => {
                    ticks.push(TimerTick { rip: point.rip, rcb: point.rcb });
                    let notify_count = self.virtio_rng().map(|t| t.notify_count()).unwrap_or(0);
                    if notify_count != last_notify_count {
                        last_notify_count = notify_count;
                        self.service_virtio_rng_interrupt(virtio_rng_vector)?;
                    }
                }
                baud_vcpu::boundary::InjectOutcome::Halted(_) => {
                    let halt = HaltObservation {
                        console_output: self.bus.console.output().to_vec(),
                        exit_pc: self.current_rip()?,
                    };
                    return Ok((ticks, RunUntilBranchObservation::Halted(halt), records));
                }
            }
        }
        Err(DeterminismHole(format!(
            "run_until_branch_or_halt_with_periodic_timer_and_virtio_rng: neither Hlt nor \
             MARK_BRANCH within {max_ticks} periodic ticks"
        ))
        .into())
    }

    /// Every tape-device record (`PROBE`/`MARK_BRANCH`/`GOAL`/`VIOLATION`/`LOG`,
    /// specs/baud-tape-device.md §4) the guest has emitted and not yet drained. Callers typically
    /// call this after [`run_to_first_halt`](Self::run_to_first_halt) to collect what the guest
    /// reported before it halted.
    pub fn drain_tape_records(&mut self) -> Vec<baud_proto::Msg> {
        self.bus.tape.device_mut().drain_records()
    }

    /// blake3 of every byte of guest RAM, read in fixed-size chunks so this never needs to
    /// allocate the whole [`layout::GUEST_RAM_SIZE`] region at once. `pub` (not just used
    /// internally by [`run_to_first_halt`](Self::run_to_first_halt)/
    /// [`run_until_branch_or_halt`](Self::run_until_branch_or_halt)'s own `HaltOutcome`) so a
    /// caller that stops at a live [`RunUntilBranchOutcome::MarkBranch`] checkpoint — which has no
    /// `HaltOutcome` of its own — can still observe the RAM state at that exact point.
    pub fn ram_hash(&self) -> String {
        const CHUNK: usize = 1 << 20; // 1 MiB
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0usize;
        while offset < layout::GUEST_RAM_SIZE {
            let take = CHUNK.min(layout::GUEST_RAM_SIZE - offset);
            let dst = &mut buf[..take];
            // Guest RAM was registered as exactly one region starting at GUEST_RAM_START
            // (`allocate_and_register_guest_ram`), so every offset in [0, GUEST_RAM_SIZE) is
            // valid to read here — a failure would mean the boot flow itself is broken, not
            // something this hash computation can meaningfully recover from.
            self.guest
                .guest_mem
                .read_slice(dst, GuestAddress(layout::GUEST_RAM_START + offset as u64))
                .expect("guest RAM region covers the whole fixed layout by construction");
            hasher.update(dst);
            offset += take;
        }
        format!("blake3:{}", hasher.finalize().to_hex())
    }

    /// H9's timed-exit "stop at `N`" primitive (specs/baud-ubuntu.md §6, specs/baud-fingerprint.md
    /// §4 step 1): drive this guest's vCPU toward `target_rcb` retired conditional branches via the
    /// same arm-early-then-single-step engine [`inject_timer_tick`](Self::inject_timer_tick) uses,
    /// but inject nothing — a fingerprint capture must observe the guest, not perturb it.
    /// `target_rcb` is an **absolute** work-clock count from boot (unlike the periodic-timer
    /// methods' `period_rcb`, which is relative to a per-call baseline), since the whole point of a
    /// fingerprint is that a fixed `N` names the same machine state across independent boots.
    ///
    /// This lands **exactly** on `target_rcb` (`run_to_events_lands_exactly_on_target_rcb` below
    /// pins it on real hardware). It did not always: todo.md §14.1 filed a real, reproducible
    /// 6-to-43-branch overshoot here, whose root cause turned out to be the work-clock counter
    /// itself rather than the stepping engine — [`LinuxBranchCounter::new`] was leaving
    /// `perf_event::Builder`'s default `exclude_kernel = 1` in place, which filters out a CPL-0
    /// guest entirely, so the "RCB" this engine steered by was really host-userspace branches
    /// retired per `KVM_RUN` (~44 counts per single step, guest branches contributing nothing) and
    /// had no resolution finer than one VM exit. See that constructor's own comment for the
    /// measurements. The returned [`ExecPoint`](baud_vcpu::boundary::ExecPoint) still reports the
    /// *actual* landed `rcb`, never the caller's requested value, so a caller can never be misled
    /// about which point was really observed.
    pub fn run_to_events(
        &mut self,
        target_rcb: u64,
    ) -> Result<baud_vcpu::boundary::RunToEventsOutcome, DeterminismHole> {
        let mut stepper =
            baud_vcpu::linux::pmu::LinuxPmuStepper::new(&mut self.guest.vcpu, &mut self.bus, &mut self.time);
        baud_vcpu::boundary::run_to_events(&mut stepper, target_rcb).map_err(|e| DeterminismHole(e.to_string()))
    }

    /// Guest-virtual → guest-physical translation (specs/baud-ubuntu.md §6, specs/baud-fingerprint.md
    /// §4 step 3): `KVM_TRANSLATE`, cross-checked by a manual CR3 four-level page walk built from
    /// `KVM_GET_SREGS`'s own `cr3` — an independent implementation of the same architectural walk
    /// KVM does internally, so a reported translation is confirmed by two separate code paths
    /// rather than trusted from the ioctl alone. Returns `None` if `gva` is unmapped; a
    /// [`DeterminismHole`] if the two methods disagree (an unmodeled bug, not a valid outcome).
    pub fn translate_gva(&self, gva: u64) -> Result<Option<u64>, DeterminismHole> {
        let kvm_result = self.guest.vcpu.translate_gva(gva).map_err(|e| DeterminismHole(e.to_string()))?;
        let kvm_gpa = (kvm_result.valid != 0).then_some(kvm_result.physical_address);

        let sregs = self.guest.vcpu.get_sregs().map_err(|e| DeterminismHole(e.to_string()))?;
        let walked_gpa = walk_cr3(&self.guest.guest_mem, sregs.cr3, gva);

        if kvm_gpa != walked_gpa {
            return Err(DeterminismHole(format!(
                "KVM_TRANSLATE ({kvm_gpa:?}) disagrees with the manual CR3 walk ({walked_gpa:?}) for gva {gva:#x}"
            )));
        }
        Ok(kvm_gpa)
    }

    /// Capture the full four-field timed-exit fingerprint (specs/baud-fingerprint.md §4): stop at
    /// `target_rcb` via [`run_to_events`](Self::run_to_events) (see that method's doc for the
    /// landing-precision history), then read `guest RIP`,
    /// translate it to `guest physical`, and hash guest RAM. `events` is the *actual* landed `rcb`,
    /// not `target_rcb` verbatim. Errors (rather than silently fingerprinting the wrong state) if
    /// the guest halted on its own before `target_rcb` — the same "did not reach the requested
    /// point" contract `specs/baud-fingerprint.md` §5's `FpError::NoBanner` describes.
    pub fn capture_fingerprint(&mut self, target_rcb: u64) -> Result<TimedExitFingerprint, DeterminismHole> {
        let outcome = self.run_to_events(target_rcb)?;
        let point = match outcome {
            baud_vcpu::boundary::RunToEventsOutcome::Reached(point) => point,
            baud_vcpu::boundary::RunToEventsOutcome::Halted(point) => {
                return Err(DeterminismHole(format!(
                    "capture_fingerprint: guest halted at rcb={} before reaching target_rcb={target_rcb}",
                    point.rcb
                )));
            }
        };
        let gpa = self.translate_gva(point.rip)?;
        Ok(TimedExitFingerprint {
            events: point.rcb,
            rip: point.rip,
            gpa,
            mem_hash: self.ram_hash(),
            console_output: self.bus.console.output().to_vec(),
        })
    }
}

/// The four-field timed-exit fingerprint plus the console output observed alongside it
/// (specs/baud-fingerprint.md §2's `Fingerprint`, minus the report-rendering/comparator layer,
/// which is future work for a dedicated `baud-fingerprint` crate — this is only the capture this
/// crate is responsible for producing, per that spec's own Non-Goals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedExitFingerprint {
    /// Deterministic events = retired conditional branches = the requested `target_rcb`.
    pub events: u64,
    /// Guest-virtual RIP at the stop.
    pub rip: u64,
    /// Guest-physical address of `rip`; `None` if unmapped.
    pub gpa: Option<u64>,
    /// `blake3:<hex>` of the whole guest-RAM region, computed right after the stop.
    pub mem_hash: String,
    /// Every byte the guest had written to the console up to the stop.
    pub console_output: Vec<u8>,
}

/// A manual x86-64 four-level page walk from `cr3`, translating linear address `lin` to a
/// guest-physical address — an independent cross-check of `KVM_TRANSLATE`
/// ([`Multiverse::translate_gva`]), not merely a reimplementation trusted on its own. Mirrors
/// specs/baud-ubuntu.md §6's reference pseudocode exactly: 4 levels (PML4/PDPTE/PDE/PTE), a `PS`
/// (bit 7) large-page entry at PDPTE/PDE terminates the walk early, and each entry's physical
/// frame is bits `[51:12]` masked to the level's page size.
fn walk_cr3<M: GuestMemoryBackend>(guest_mem: &M, cr3: u64, lin: u64) -> Option<u64> {
    /// `(high bit, low bit, PS-mask)` for each of the 4 levels, high-to-low (PML4 first).
    const LEVELS: [(u32, u32, u64); 4] = [
        (47, 39, 0),                    // PML4E — never a large-page terminator
        (38, 30, 0x000f_ffff_c000_0000), // PDPTE — PS=1 means a 1 GiB page
        (29, 21, 0x000f_ffff_ffe0_0000), // PDE — PS=1 means a 2 MiB page
        (20, 12, 0),                     // PTE — the walk's normal 4 KiB terminator
    ];
    const PHYS_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

    let mut table = cr3 & PHYS_ADDR_MASK;
    for (hi, lo, ps_mask) in LEVELS {
        let bits = hi - lo + 1;
        let index = (lin >> lo) & ((1u64 << bits) - 1);
        let mut raw = [0u8; 8];
        guest_mem.read_slice(&mut raw, GuestAddress(table + index * 8)).ok()?;
        let entry = u64::from_le_bytes(raw);
        if entry & 1 == 0 {
            return None; // not present
        }
        if lo != 12 && entry & (1 << 7) != 0 {
            return Some((entry & ps_mask) | (lin & ((1u64 << lo) - 1)));
        }
        table = entry & PHYS_ADDR_MASK;
    }
    Some(table | (lin & 0xfff))
}

/// One VM's result from [`run_fleet`] (H6, todo.md §10 — "many single-vCPU VMs pinned across
/// cores explore in parallel on one host").
#[derive(Debug)]
pub struct FleetVmResult {
    /// The logical CPU this VM's thread was pinned to (specs/baud-host.md §5's `sched_setaffinity`
    /// rule).
    pub core_id: usize,
    pub outcome: HaltOutcome,
    pub elapsed: std::time::Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    #[error(transparent)]
    Placement(#[from] baud_host::PlacementError),
    #[error("VM on core {core_id} failed to pin its thread: {source}")]
    Pin { core_id: usize, source: io::Error },
    #[error("VM on core {core_id} failed to boot: {source}")]
    Boot { core_id: usize, source: BootError },
    #[error("VM on core {core_id} failed to run: {source}")]
    Run { core_id: usize, source: RunLoopError },
    #[error("VM thread on core {core_id} panicked")]
    ThreadPanicked { core_id: usize },
}

/// Run `tapes.len()` single-vCPU guests concurrently, one per physical core [`baud_host::Host::
/// place`] assigns, each thread pinned to its own core via [`baud_vcpu::linux::
/// pin_thread_to_core`] (specs/baud-host.md §5: "One physical core per VM | vCPU thread pinned
/// via `sched_setaffinity`" — until this function, that call had zero call sites anywhere in the
/// workspace, todo.md §14). Every VM boots the same kernel image with its own tape (`tapes[i]`)
/// and runs to first halt; results come back in the same order as `tapes`, one per VM, so a
/// caller can prove no VM observed another's state (each guest's own tape pins its own expected
/// output, the same construction `thousand_branches_are_independent_and_deterministic` uses for
/// branches). Refuses outright — never partially places — when `tapes.len()` exceeds this host's
/// fleet capacity ([`baud_host::Host::place`]'s own contract).
pub fn run_fleet(
    host: &baud_host::Host,
    kernel_path: &Path,
    cmdline: &str,
    tapes: Vec<Vec<u8>>,
) -> Result<Vec<FleetVmResult>, FleetError> {
    let placement = host.place(tapes.len())?;

    std::thread::scope(|scope| {
        let handles: Vec<_> = placement
            .assigned_cores
            .iter()
            .zip(tapes)
            .map(|(core, tape)| {
                let core_id = core.sibling_threads[0];
                scope.spawn(move || -> Result<FleetVmResult, FleetError> {
                    baud_vcpu::linux::pin_thread_to_core(core_id)
                        .map_err(|source| FleetError::Pin { core_id, source })?;
                    let start = std::time::Instant::now();
                    let mut vm = Multiverse::boot(kernel_path, cmdline, 0, 1, tape, None)
                        .map_err(|source| FleetError::Boot { core_id, source })?;
                    let outcome = vm
                        .run_to_first_halt()
                        .map_err(|source| FleetError::Run { core_id, source })?;
                    Ok(FleetVmResult { core_id, outcome, elapsed: start.elapsed() })
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(Err(FleetError::ThreadPanicked { core_id: usize::MAX })))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `apply_cpuid_mask` reuses `cpuid::apply_determinism_mask` unmodified against real
    /// `kvm_cpuid_entry2` values — exercised here without any ioctl, just the `CpuidEntry` impl
    /// above, so a future field-order change in `kvm-bindings` breaks this test rather than
    /// silently masking the wrong bits on real hardware.
    #[test]
    fn kvm_cpuid_entry2_masking_matches_the_portable_leaf_type() {
        let mut kvm_entries = [kvm_cpuid_entry2 {
            function: 0x1,
            index: 0,
            ecx: u32::MAX,
            ..Default::default()
        }];
        cpuid::apply_determinism_mask(&mut kvm_entries);
        assert_eq!(kvm_entries[0].ecx & (1 << 30), 0, "RDRAND must be cleared");
        assert_eq!(kvm_entries[0].ecx & (1 << 31), 1 << 31, "hypervisor-present must be set");
    }

    /// H1's real bootable fixture (`tests/fixtures/hello-guest/`, see that directory's
    /// `BUILD.md` for exact provenance/regeneration steps and why it is a hand-assembled payload
    /// rather than a real Linux kernel): a minimal, valid-per-the-loader's-own-checks bzImage
    /// wrapping 17 bytes of hand-written x86-64 that writes a fixed marker line directly to COM1
    /// (port `0x3f8`) then `hlt`s in a loop — no scheduler, no timer/jiffies dependency, so it
    /// reaches a clean halt with nothing more than this crate's boot flow provides today.
    fn hello_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello-guest/bzImage")
    }

    /// The exact marker byte string `tests/fixtures/hello-guest/payload.s` writes to the console
    /// before halting — asserted against verbatim so a change to either side (fixture or this
    /// test) is caught rather than silently drifting apart.
    const HELLO_GUEST_MARKER: &str = "BAUD_HELLO_GUEST\n";

    /// specs/baud-multiverse.md §3.1's `double_boot_memory_identical`, exercised for the first
    /// time against real KVM hardware (todo.md §14 tracked this as "not yet booted on real KVM
    /// hardware" across every prior iteration): boot the hello image twice from the same tape,
    /// assert the guest reaches a clean halt with the expected console marker, and assert the two
    /// runs' `ram_hash` (blake3 of the whole guest-RAM region at first `Hlt`) are byte-identical —
    /// boot nondeterminism is a bug, per the spec's own framing of this test.
    #[test]
    fn double_boot_memory_identical() {
        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let first_outcome = first.run_to_first_halt().expect("first run failed");
        assert_eq!(
            String::from_utf8_lossy(&first_outcome.console_output),
            HELLO_GUEST_MARKER,
            "guest must print exactly its marker line before halting"
        );

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let second_outcome = second.run_to_first_halt().expect("second run failed");
        assert_eq!(
            second_outcome.console_output, first_outcome.console_output,
            "console output must be identical across two boots of the same image+tape"
        );
        assert_eq!(
            second_outcome.ram_hash, first_outcome.ram_hash,
            "guest RAM at first Hlt must be byte-identical across two boots (boot nondeterminism is a bug)"
        );
    }

    /// `tests/fixtures/spin-guest/` (that directory's `BUILD.md`): a hand-assembled `1: jmp 1b`
    /// payload that causes **zero** VM exits, ever — the one fixture in this repo with no way to
    /// reach `Hlt`/`Shutdown` on its own, built specifically to exercise the wall-clock watchdog
    /// (todo.md §14.1 "Still open" item 1, `crates/baud-vcpu/src/linux/watchdog.rs`) end-to-end
    /// against a real, currently-running `KVM_RUN`.
    fn spin_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/spin-guest/bzImage")
    }

    /// `tests/fixtures/halt-then-spin-guest/` (that directory's `BUILD.md`): halts once via a real
    /// `hlt`, then — reached only via the injected interrupt's `iretq` — spins forever with zero
    /// further VM exits. The fixture that exercises
    /// [`run_to_first_halt_with_periodic_timer_and_devices`]'s resume-past-halt burst loop
    /// end-to-end (todo.md §14 item 17's finding: that loop, not `inject_at`, is where a real H9
    /// stall actually lived).
    fn halt_then_spin_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/halt-then-spin-guest/bzImage")
    }

    /// The concrete fix for todo.md §14.1 "Still open" item 1: before the watchdog existed,
    /// `run_to_first_halt()` against a guest that never exits (this project's subtractive machine
    /// model has no APIC/PIT/host interrupts to force one) hung the calling thread forever. A
    /// tight `set_watchdog_budget` proves the real fix without this test itself hanging: the call
    /// must return, and specifically with `RunLoopError::WatchdogKilled`, well within a generous
    /// wall-clock bound this test enforces on itself.
    #[test]
    fn wall_clock_watchdog_kills_a_truly_spinning_guest() {
        let kernel = spin_guest_kernel_path();
        let budget = std::time::Duration::from_millis(300);
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
        mv.set_watchdog_budget(budget);

        let start = std::time::Instant::now();
        let result = mv.run_to_first_halt();
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the watchdog must reclaim a spinning guest promptly, not merely eventually (took {elapsed:?})"
        );
        match result {
            Err(baud_vcpu::RunLoopError::WatchdogKilled { budget_ms, guest_rip, console_tail }) => {
                assert_eq!(budget_ms, budget.as_millis() as u64);
                // todo.md §14.2 H9 item 20's own named next diagnostic: the guest's own RIP, not
                // just a host-side stack trace. `spin-guest` is exactly `1: jmp 1b`, so a captured
                // RIP must land inside that one instruction's own address, not merely be present.
                let rip = guest_rip.expect("watchdog kill must capture the guest's RIP via KVM_GET_REGS");
                assert!(rip > 0, "captured guest RIP must be a real address, got {rip:#x}");
                // This kill comes through `run_to_first_halt` -> `baud_vcpu::linux::run_until_
                // halted`'s own whole-run watchdog, which has no console/device model in scope at
                // all (that is `Multiverse`'s job) — structurally `None`, per item 21's doc.
                assert_eq!(
                    console_tail, None,
                    "baud_vcpu's own whole-run watchdog has no console model, must always be None"
                );
            }
            other => panic!("expected RunLoopError::WatchdogKilled, got {other:?}"),
        }
    }

    /// The negative case alongside [`wall_clock_watchdog_kills_a_truly_spinning_guest`]: a guest
    /// that halts well within its budget must succeed normally, proving the watchdog is not a
    /// source of false-positive kills under an ordinary, fast boot-to-halt.
    #[test]
    fn wall_clock_watchdog_does_not_fire_on_a_normal_guest() {
        let kernel = hello_guest_kernel_path();
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
        mv.set_watchdog_budget(std::time::Duration::from_secs(5));

        let outcome = mv.run_to_first_halt().expect("a guest that halts well within its budget must succeed");
        assert_eq!(String::from_utf8_lossy(&outcome.console_output), HELLO_GUEST_MARKER);
    }

    /// The per-*tick* sibling of [`wall_clock_watchdog_kills_a_truly_spinning_guest`]: proves
    /// `run_to_first_halt_with_periodic_timer_and_devices`'s own watchdog
    /// (`PERIODIC_TICK_WATCHDOG_BUDGET`/`set_periodic_tick_watchdog_budget`), not the whole-run
    /// one `set_watchdog_budget` guards above, actually reclaims a wedged tick. `spin-guest`
    /// retires no conditional branches and causes no VM exit ever, so the very first tick's
    /// `inject_at` call blocks inside `run_until_exit`'s `KVM_RUN` with nothing else to reclaim
    /// it — exactly the gap a real, 11-minutes-and-still-climbing stuck H9 boot attempt exposed
    /// this iteration (todo.md §14 item 15's follow-up).
    #[test]
    fn periodic_tick_watchdog_kills_a_stuck_tick() {
        let kernel = spin_guest_kernel_path();
        let budget = std::time::Duration::from_millis(300);
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
        mv.set_periodic_tick_watchdog_budget(budget);

        let start = std::time::Instant::now();
        let result =
            mv.run_to_first_halt_with_periodic_timer_and_devices(500_000, TIMER_VECTOR, &[], 20_000, None, 0);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the per-tick watchdog must reclaim a wedged tick promptly, not merely eventually (took {elapsed:?})"
        );
        match result {
            Err(RunLoopError::WatchdogKilled { budget_ms, guest_rip, console_tail }) => {
                assert_eq!(budget_ms, budget.as_millis() as u64);
                let rip = guest_rip.expect("watchdog kill must capture the guest's RIP via KVM_GET_REGS");
                assert!(rip > 0, "captured guest RIP must be a real address, got {rip:#x}");
                // This kill comes through the tick loop's own `inject_at` watchdog, which does have
                // a console/device model in scope (`Multiverse::bus.console`) — `Some`, not `None`,
                // even though `spin-guest` (`1: jmp 1b`, no I/O) never actually writes to it.
                assert_eq!(
                    console_tail,
                    Some(String::new()),
                    "spin-guest performs no I/O, so its captured console tail must be Some(empty)"
                );
            }
            other => panic!("expected RunLoopError::WatchdogKilled, got {other:?}"),
        }
    }

    /// The negative case alongside [`periodic_tick_watchdog_kills_a_stuck_tick`]: a guest whose
    /// tick completes well within its per-tick budget must succeed normally, proving the new
    /// watchdog is not a source of false-positive kills on an ordinary, fast periodic-timer run.
    #[test]
    fn periodic_tick_watchdog_does_not_fire_on_a_normal_tick() {
        let kernel = timer_guest_kernel_path();
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
        mv.set_periodic_tick_watchdog_budget(std::time::Duration::from_secs(5));

        mv.run_to_first_halt_with_periodic_timer_and_devices(200_000, TIMER_VECTOR, &[], 20, None, 0)
            .expect("a tick that completes well within its budget must succeed");
    }

    /// Reproduces the exact thread the real `baud-server` run loop executes on: every real boot
    /// runs inside `tokio::task::spawn_blocking`'s reusable pool (`watchdog.rs`'s own doc:
    /// "baud-server runs boots on tokio::task::spawn_blocking's reusable thread pool"), not a
    /// plain `#[test]`'s own OS thread the way [`periodic_tick_watchdog_kills_a_stuck_tick`]
    /// (which this test is otherwise identical to) runs on. Filed after a real Ubuntu H9 boot
    /// attempt (todo.md §14 item 16's own real next step) stalled well past the 600s per-tick
    /// budget through the real server with no `WatchdogKilled` ever surfacing — this isolates
    /// whether that gap is specific to the `spawn_blocking` thread pool.
    #[test]
    fn periodic_tick_watchdog_kills_a_stuck_tick_via_spawn_blocking() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build a tokio runtime");
        let budget = std::time::Duration::from_millis(300);
        let outcome = runtime.block_on(async {
            let handle = tokio::task::spawn_blocking(move || {
                let kernel = spin_guest_kernel_path();
                let mut mv =
                    Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
                mv.set_periodic_tick_watchdog_budget(budget);
                let start = std::time::Instant::now();
                let result = mv.run_to_first_halt_with_periodic_timer_and_devices(
                    500_000,
                    TIMER_VECTOR,
                    &[],
                    20_000,
                    None,
                    0,
                );
                (result, start.elapsed())
            });
            match tokio::time::timeout(std::time::Duration::from_secs(10), handle).await {
                Ok(join_result) => {
                    let (result, elapsed) = join_result.expect("spawn_blocking task panicked");
                    match result {
                        Err(e) => Ok((e, elapsed)),
                        Ok(_) => Err(
                            "expected the wedged tick to error, guest halted instead".to_string(),
                        ),
                    }
                }
                Err(_) => Err(format!(
                    "the per-tick watchdog did NOT reclaim a wedged tick within 10s when the guest \
                     runs on a tokio::task::spawn_blocking thread (budget was {budget:?}) -- it does \
                     on a plain #[test] OS thread (periodic_tick_watchdog_kills_a_stuck_tick), so \
                     this gap is spawn_blocking-specific"
                )),
            }
        });
        // Bounded, not the implicit `Drop` (which waits unboundedly for outstanding
        // `spawn_blocking` tasks) -- if the watchdog really did fail to reclaim the tick above,
        // that task is still running and must not hang this test's own process teardown.
        runtime.shutdown_timeout(std::time::Duration::from_millis(500));

        match outcome {
            Ok((RunLoopError::WatchdogKilled { budget_ms, guest_rip, console_tail }, elapsed)) => {
                assert_eq!(budget_ms, budget.as_millis() as u64);
                let rip = guest_rip.expect("watchdog kill must capture the guest's RIP via KVM_GET_REGS");
                assert!(rip > 0, "captured guest RIP must be a real address, got {rip:#x}");
                assert_eq!(
                    console_tail,
                    Some(String::new()),
                    "spin-guest performs no I/O, so its captured console tail must be Some(empty)"
                );
                assert!(
                    elapsed < std::time::Duration::from_secs(10),
                    "the per-tick watchdog must reclaim a wedged tick promptly (took {elapsed:?})"
                );
            }
            Ok((other, _)) => panic!("expected RunLoopError::WatchdogKilled, got {other:?}"),
            Err(msg) => panic!("{msg}"),
        }
    }

    /// The burst-loop sibling of [`periodic_tick_watchdog_kills_a_stuck_tick`]: proves the
    /// resume-past-halt burst loop's own per-call watchdog (todo.md §14 item 17's fix) actually
    /// reclaims a wedged `step_exit_cancellable` call, not just a wedged `inject_at` call.
    /// `halt-then-spin-guest` halts almost immediately (well before `period_rcb`'s target, the
    /// same "halted before its tick's full RCB budget" case a real Ubuntu boot hits per todo.md
    /// §14 item 12); with no devices and a `pattern` that never appears, the burst loop then
    /// delivers the timer interrupt directly and drains exits one at a time — the first call
    /// handles the ISR's COM1 write normally, but the guest falls straight into `spin: jmp spin`
    /// immediately afterward, so the *next* `step_exit_cancellable` call blocks forever without
    /// this fix.
    #[test]
    fn halt_then_spin_burst_watchdog_kills_a_wedged_burst_exit() {
        let kernel = halt_then_spin_guest_kernel_path();
        let budget = std::time::Duration::from_millis(300);
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
        mv.set_periodic_tick_watchdog_budget(budget);

        let start = std::time::Instant::now();
        let result = mv.run_to_first_halt_with_periodic_timer_and_devices(
            500_000,
            TIMER_VECTOR,
            &[],
            20_000,
            Some(b"this pattern never appears in this fixture's console output"),
            1_000_000,
        );
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the burst loop's per-call watchdog must reclaim a wedged step_exit_cancellable call \
             promptly, not merely eventually (took {elapsed:?})"
        );
        match result {
            Err(RunLoopError::WatchdogKilled { budget_ms, guest_rip, console_tail }) => {
                assert_eq!(budget_ms, budget.as_millis() as u64);
                // This is the exact call site todo.md §14.2 H9 items 18/20 traced a real Ubuntu
                // boot stall to — `halt-then-spin-guest`'s ISR resumes into `spin: jmp spin`, so a
                // captured RIP here must land inside that one instruction, same shape as the real
                // H9 stall this fixture models.
                let rip = guest_rip.expect("watchdog kill must capture the guest's RIP via KVM_GET_REGS");
                assert!(rip > 0, "captured guest RIP must be a real address, got {rip:#x}");
                // `payload.s`'s ISR writes exactly one 'T' byte to COM1 before falling into `spin`
                // — proves the capture reaches real, non-empty console content at the moment of a
                // kill, the exact scenario todo.md §14.2 H9 item 21 found in a real Ubuntu attempt
                // (console output stopped growing well before the watchdog fired).
                assert_eq!(
                    console_tail,
                    Some("T".to_string()),
                    "the ISR's one COM1 write must be captured verbatim in the watchdog kill's console tail"
                );
            }
            other => panic!("expected RunLoopError::WatchdogKilled, got {other:?}"),
        }
    }

    /// The negative case alongside [`halt_then_spin_burst_watchdog_kills_a_wedged_burst_exit`]: a
    /// normal periodic-timer run (`timer-guest`, which keeps halting and waking rather than ever
    /// falling into an unbounded spin) must not be disturbed by the burst loop's new watchdog —
    /// proving it is not a source of false-positive kills on ordinary "resume past a non-terminal
    /// halt" traffic. The pattern is three consecutive marker bytes rather than one: `timer-guest`'s
    /// ISR also fires once from *inside* the main loop (an ordinary `inject_at` delivery, not the
    /// burst loop), and nothing else ever writes to this fixture's console — three in a row can
    /// only accumulate from three separate hlt/wake cycles once the guest is sitting at its final
    /// `hlt`, guaranteeing this exercises the resume-past-halt burst loop for real rather than
    /// returning on the very first tick.
    #[test]
    fn burst_watchdog_does_not_fire_on_normal_resume_past_halt() {
        let kernel = timer_guest_kernel_path();
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
        mv.set_periodic_tick_watchdog_budget(std::time::Duration::from_secs(5));

        let (_, outcome) = mv
            .run_to_first_halt_with_periodic_timer_and_devices(
                200_000,
                TIMER_VECTOR,
                &[],
                20_000,
                Some(&[TIMER_MARKER, TIMER_MARKER, TIMER_MARKER]),
                1_000_000,
            )
            .expect("a normal resume-past-halt run must succeed within its burst watchdog budget");
        assert!(
            outcome.console_output.windows(3).any(|w| w == [TIMER_MARKER, TIMER_MARKER, TIMER_MARKER]),
            "expected three consecutive timer-interrupt marker bytes on the console"
        );
    }

    fn halt_then_multi_io_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/halt-then-multi-io-guest/bzImage")
    }

    static HALT_THEN_MULTI_IO_SERVICE_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// The concrete fix for todo.md §14.2 H9 items 20/21/22's own flagged, previously-unfixed gap:
    /// the resume-past-halt burst loop (`crates/baud-multiverse/src/linux/mod.rs`) used to check
    /// `devices` only once, before entering its raw exit-drain loop, never again inside it — a
    /// completion arriving *between* two of the loop's own exits went unserviced until the next
    /// periodic tick, or forever if none came. `halt-then-multi-io-guest` performs three separate
    /// `out` writes after waking from its one real `hlt` (`tests/fixtures/halt-then-multi-io-guest/
    /// BUILD.md`) — three real VM exits in a row inside the burst loop, before spinning forever — so
    /// a fake `TickPolledDevice` whose `notify_count` is tied to the guest's own growing console
    /// output sees three distinct notify-count changes *during* that one burst; `service_running`
    /// must fire once for each, not once for the whole tick.
    #[test]
    fn burst_loop_services_devices_between_raw_exits() {
        HALT_THEN_MULTI_IO_SERVICE_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);

        fn fake_notify_count(mv: &Multiverse) -> Option<u64> {
            Some(mv.bus.console.output().len() as u64)
        }
        fn fake_service_running(_mv: &mut Multiverse, _vector: u8) -> Result<u32, RunLoopError> {
            HALT_THEN_MULTI_IO_SERVICE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(0)
        }
        fn fake_service_halted(_mv: &mut Multiverse, _vector: u8) -> Result<u32, RunLoopError> {
            Ok(0)
        }

        let kernel = halt_then_multi_io_guest_kernel_path();
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");

        let devices = [TickPolledDevice {
            vector: 0,
            notify_count: fake_notify_count,
            service_running: fake_service_running,
            service_halted: fake_service_halted,
        }];

        let (_, outcome) = mv
            .run_to_first_halt_with_periodic_timer_and_devices(
                500_000,
                TIMER_VECTOR,
                &devices,
                20_000,
                Some(b"ABC"),
                1_000_000,
            )
            .expect("the guest must reach the ABC pattern well before its final spin");

        assert_eq!(
            String::from_utf8_lossy(&outcome.console_output),
            "ABC",
            "the three marker writes must be observed verbatim, in order"
        );
        assert_eq!(
            HALT_THEN_MULTI_IO_SERVICE_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the burst loop must service the fake device once for each of the three notify-count \
             changes that occur between its own raw exits, not just once per periodic tick"
        );
    }

    /// Set `flag` from another thread after `delay`, the way `baud-server`'s `CancelGuard` does
    /// when hyper drops an abandoned request's handler future (measured there at 4 ms after the
    /// client dies). Returns the join handle so a test can prove the setter really ran.
    fn set_flag_after(
        flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        delay: std::time::Duration,
    ) -> std::thread::JoinHandle<()> {
        let flag = std::sync::Arc::clone(flag);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    }

    /// The supervisory-cancellation path against the one guest that makes **zero** VM exits ever
    /// (`spin-guest`, the same fixture the wall-clock watchdog test uses): with the flag set 100 ms
    /// in, the run must return `Cancelled` in seconds, not sit inside one blocking `KVM_RUN` until
    /// the watchdog's budget elapses.
    ///
    /// The watchdog budget is deliberately set to 60 s — far longer than this test's own bound —
    /// so a pass cannot be the watchdog doing the work: only the cancellation signal
    /// (`baud_vcpu::linux::CancelKicker`) can stop this guest inside the bound, and the returned
    /// error variant proves which mechanism it was. Before that kicker existed, polling the flag
    /// between exits achieved exactly nothing here: this guest has no exits to poll between.
    #[test]
    fn cancelling_a_spinning_guest_stops_the_run_promptly() {
        let kernel = spin_guest_kernel_path();
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
        mv.set_watchdog_budget(std::time::Duration::from_secs(60));
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        mv.set_cancel_flag(std::sync::Arc::clone(&flag));
        let setter = set_flag_after(&flag, std::time::Duration::from_millis(100));

        let start = std::time::Instant::now();
        let result = mv.run_to_first_halt_without_ram_hash();
        let elapsed = start.elapsed();
        setter.join().expect("flag-setting thread panicked");

        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "a cancelled run must stop promptly, not after the 60s watchdog budget (took {elapsed:?})"
        );
        match result {
            Err(baud_vcpu::RunLoopError::Cancelled) => {}
            other => panic!("expected RunLoopError::Cancelled, got {other:?}"),
        }
    }

    /// The gap this iteration actually closes, measured end to end: a *periodic-timer* run spends
    /// nearly all of its time inside `boundary::inject_at` — one `KVM_RUN` per tick that can last
    /// arbitrarily long — so polling the flag once per tick never stopped anything. Driven here
    /// against `spin-guest`, whose `jmp $` loop retires **no conditional branches at all**, one
    /// tick's target work-count is never reached and that single `KVM_RUN` would block forever
    /// (measured before this fix: 120 s+ of a pegged core with the flag set 4 ms in, and
    /// `max_ticks=8` never completing).
    ///
    /// A regression here does not fail this test, it hangs it — exactly like the existing
    /// `wall_clock_watchdog_kills_a_truly_spinning_guest`, which is the established shape in this
    /// module for "prove the escape hatch actually exists".
    #[test]
    fn cancelling_a_periodic_timer_run_stops_inside_one_unbounded_tick() {
        let kernel = spin_guest_kernel_path();
        let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        mv.set_cancel_flag(std::sync::Arc::clone(&flag));
        let setter = set_flag_after(&flag, std::time::Duration::from_millis(100));

        let start = std::time::Instant::now();
        // 8 ticks of 200_000 RCB each: a bound this guest can never reach, since it retires no
        // conditional branches — the run can only end by being cancelled.
        let result = mv.run_to_first_halt_with_periodic_timer(200_000, TIMER_VECTOR, 8);
        let elapsed = start.elapsed();
        setter.join().expect("flag-setting thread panicked");

        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "a cancelled periodic-timer run must stop inside the tick it is in, not at the end of \
             it (took {elapsed:?})"
        );
        match result {
            Err(baud_vcpu::RunLoopError::Cancelled) => {}
            // Specifically not a DeterminismHole: an abandoned run is a host-side supervisory
            // decision, and reporting it as a determinism hole would be a lie about the guest.
            other => panic!("expected RunLoopError::Cancelled, got {other:?}"),
        }
    }

    /// The determinism half of the contract [`Multiverse::set_cancel_flag`] makes: a flag that is
    /// installed but never set must leave the run byte-identical — same console bytes, same RAM
    /// hash, same exit PC — to the same run with no flag installed at all. (The no-flag case is
    /// itself pinned against the fixture's own marker by `double_boot_memory_identical`.)
    #[test]
    fn an_installed_but_unset_cancel_flag_leaves_a_run_byte_identical() {
        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let mut plain = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("plain boot failed");
        let plain_outcome = plain.run_to_first_halt().expect("plain run failed");

        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut flagged = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("flagged boot failed");
        flagged.set_cancel_flag(std::sync::Arc::clone(&flag));
        let flagged_outcome = flagged.run_to_first_halt().expect("a never-set flag must not disturb the run");

        assert_eq!(String::from_utf8_lossy(&plain_outcome.console_output), HELLO_GUEST_MARKER);
        assert_eq!(flagged_outcome.console_output, plain_outcome.console_output);
        assert_eq!(flagged_outcome.ram_hash, plain_outcome.ram_hash);
        assert_eq!(flagged_outcome.exit_pc, plain_outcome.exit_pc);
        assert!(!flag.load(std::sync::atomic::Ordering::SeqCst), "nothing may set the flag but the supervisor");
    }

    /// The same determinism check on the path cancellation actually had to be threaded *into* —
    /// the periodic-timer engine's boundary walk. Every landed tick's `(rip, rcb)` must be
    /// identical between a run with a never-set flag installed and one with no flag at all, to the
    /// same exactness (`RCB_HARDWARE_JITTER_TOLERANCE == 0`) two plain boots are held to by
    /// `periodic_timer_injection_halts_gracefully_and_reproducibly`.
    #[test]
    fn an_installed_but_unset_cancel_flag_leaves_a_periodic_timer_run_byte_identical() {
        let kernel = timer_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const PERIOD_RCB: u64 = 200_000;
        const MAX_TICKS: u32 = 20;

        let mut plain = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("plain boot failed");
        let (plain_ticks, plain_halt) = plain
            .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
            .expect("plain periodic run failed");

        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut flagged = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("flagged boot failed");
        flagged.set_cancel_flag(std::sync::Arc::clone(&flag));
        let (flagged_ticks, flagged_halt) = flagged
            .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
            .expect("a never-set flag must not disturb the periodic-timer run");

        assert!(!plain_ticks.is_empty(), "the fixture must survive at least one tick for this to prove anything");
        assert_eq!(
            flagged_ticks.len(),
            plain_ticks.len(),
            "an installed-but-unset flag must not change how many ticks the guest survives"
        );
        for (i, (a, b)) in plain_ticks.iter().zip(flagged_ticks.iter()).enumerate() {
            assert_eq!(a.rip, b.rip, "tick {i}: landing rip must be unchanged by an unset cancellation flag");
            // Read through a binding, exactly like the sibling assertions in
            // `timer_tick_lands_at_identical_instruction` — see their comment.
            let tolerance = RCB_HARDWARE_JITTER_TOLERANCE;
            assert!(
                a.rcb.abs_diff(b.rcb) <= tolerance,
                "tick {i}: landing rcb {} vs {} — see RCB_HARDWARE_JITTER_TOLERANCE",
                a.rcb,
                b.rcb
            );
        }
        assert_eq!(flagged_halt.console_output, plain_halt.console_output);
        assert_eq!(flagged_halt.ram_hash, plain_halt.ram_hash);
        assert_eq!(flagged_halt.exit_pc, plain_halt.exit_pc);
    }

    /// Reads the `SETUP_RNG_SEED` node's 32 seed bytes back out of a booted `Multiverse`'s real
    /// guest RAM via `hdr.setup_data` (shared by
    /// [`rng_seed_setup_data_is_wired_into_a_real_boot_and_is_tape_derived`] and
    /// [`boot_params_seed_is_pinned`] rather than duplicated per-test): also asserts
    /// `hdr.setup_data` itself points at the fixed node address, since a wrong pointer would make
    /// every downstream seed-byte comparison meaningless.
    fn read_rng_seed_via_hdr(mv: &Multiverse) -> [u8; bootparams::RNG_SEED_LEN] {
        use vm_memory::Bytes;

        let mut zero_page = vec![0u8; std::mem::size_of::<linux_loader::loader::bootparam::boot_params>()];
        mv.guest
            .guest_mem
            .read_slice(&mut zero_page, GuestAddress(layout::ZERO_PAGE_ADDR))
            .expect("read back the zero page from real guest RAM");
        let params: linux_loader::loader::bootparam::boot_params =
            unsafe { std::ptr::read_unaligned(zero_page.as_ptr() as *const _) };
        let setup_data_addr = params.hdr.setup_data;
        assert_eq!(
            setup_data_addr,
            layout::RNG_SEED_SETUP_DATA_ADDR,
            "hdr.setup_data must point at the fixed RNG-seed node address"
        );
        let mut seed = [0u8; bootparams::RNG_SEED_LEN];
        mv.guest
            .guest_mem
            .read_slice(&mut seed, GuestAddress(setup_data_addr + 16))
            .expect("read back the seed bytes from real guest RAM");
        seed
    }

    /// specs/baud-multiverse.md §3.8's "Boot RNG seed", wired end-to-end through the real
    /// `Multiverse::boot` flow (not just `bootparams`'s own unit tests): the `SETUP_RNG_SEED`
    /// `setup_data` node baud writes must (1) actually land in real guest RAM at the address
    /// `hdr.setup_data` points to, with the tape-derived seed bytes intact, and (2) be a pure
    /// function of the tape — same tape twice reproduces the identical seed, a different tape
    /// changes it — the same `all_input_is_tape_derived` guarantee applied to this boot-time input.
    #[test]
    fn rng_seed_setup_data_is_wired_into_a_real_boot_and_is_tape_derived() {
        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let tape_a = b"tape A".to_vec();
        let tape_b = b"tape B".to_vec();

        let boot_a1 = Multiverse::boot(&kernel, cmdline, 0, 1, tape_a.clone(), None).expect("boot A1 failed");
        let seed_a1 = read_rng_seed_via_hdr(&boot_a1);

        let boot_a2 = Multiverse::boot(&kernel, cmdline, 0, 1, tape_a, None).expect("boot A2 failed");
        let seed_a2 = read_rng_seed_via_hdr(&boot_a2);
        assert_eq!(seed_a1, seed_a2, "the same tape must reproduce the identical RNG seed");

        let boot_b = Multiverse::boot(&kernel, cmdline, 0, 1, tape_b, None).expect("boot B failed");
        let seed_b = read_rng_seed_via_hdr(&boot_b);
        assert_ne!(seed_a1, seed_b, "a different tape must change the RNG seed");
    }

    /// specs/baud-multiverse.md §4.2's `boot_params_seed_is_pinned`: "two boots write an identical
    /// seed node; early CRNG init is reproducible" — restated as its own spec-named test, distinct
    /// from [`rng_seed_setup_data_is_wired_into_a_real_boot_and_is_tape_derived`]'s broader
    /// tape-derivation proof (same-tape reproducibility *and* different-tape divergence). `hello-guest`
    /// touches no CRNG (no libc, no scheduler), so this test proves the boot-time *input* early CRNG
    /// init reads — the pinned seed node itself, plus the rest of the deterministic boot around it —
    /// is reproducible on every boot the machine performs. The guest-observable CRNG *output* being
    /// reproducible from that same pinned input is proven separately, on a real Linux kernel, by
    /// `os_entropy_is_deterministic` (enforced-regime, `#[ignore]`d — real hardware-trapped RDTSC is
    /// required there; see that test's own doc for why).
    #[test]
    fn boot_params_seed_is_pinned() {
        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let tape = b"boot-params-seed-is-pinned tape".to_vec();

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, tape.clone(), None).expect("first boot failed");
        let first_seed = read_rng_seed_via_hdr(&first);
        let first_halt = first.run_to_first_halt().expect("first run failed");

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, tape, None).expect("second boot failed");
        let second_seed = read_rng_seed_via_hdr(&second);
        let second_halt = second.run_to_first_halt().expect("second run failed");

        assert_eq!(first_seed, second_seed, "two boots of the same tape must write an identical RNG-seed node");
        assert_eq!(
            first_halt.console_output, second_halt.console_output,
            "a reproducible seed node must not perturb the rest of the deterministic boot"
        );
    }

    /// todo.md §4.2's initramfs wiring (`bootparams::write_initramfs`), closed against a real boot
    /// rather than only `bootparams`'s own guest-memory-only unit tests: an initramfs handed to
    /// [`Multiverse::boot_with_rdseed_sites`] must (1) land byte-for-byte in real guest RAM at
    /// [`layout::INITRAMFS_ADDR`], (2) be pointed to by `hdr.ramdisk_image`/`ramdisk_size` read back
    /// off the real zero page, and (3) not disturb the ordinary boot flow — the hello-guest fixture
    /// still reaches its marker and halts cleanly with an initramfs present that it never reads
    /// (it predates any ramdisk-aware `/init`), proving the write doesn't collide with the kernel
    /// image or clobber anything the boot flow depends on.
    #[test]
    fn initramfs_is_wired_into_a_real_boot_and_lands_in_guest_ram() {
        use vm_memory::Bytes;

        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let initramfs: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();

        let mut mv = Multiverse::boot_with_rdseed_sites(
            &kernel,
            cmdline,
            0,
            1,
            vec![],
            None,
            Some(&initramfs),
            [],
        )
        .expect("boot with initramfs failed");

        let mut zero_page = vec![0u8; std::mem::size_of::<linux_loader::loader::bootparam::boot_params>()];
        mv.guest
            .guest_mem
            .read_slice(&mut zero_page, GuestAddress(layout::ZERO_PAGE_ADDR))
            .expect("read back the zero page from real guest RAM");
        let params: linux_loader::loader::bootparam::boot_params =
            unsafe { std::ptr::read_unaligned(zero_page.as_ptr() as *const _) };
        // `boot_params` is `#[repr(C, packed)]` — copy the nested fields to locals before
        // `assert_eq!` takes a reference to them (E0793), same as this file's RNG-seed test above.
        let (ramdisk_image, ramdisk_size) = (params.hdr.ramdisk_image, params.hdr.ramdisk_size);
        assert_eq!(ramdisk_image, layout::INITRAMFS_ADDR as u32);
        assert_eq!(ramdisk_size, initramfs.len() as u32);

        let mut initramfs_back = vec![0u8; initramfs.len()];
        mv.guest
            .guest_mem
            .read_slice(&mut initramfs_back, GuestAddress(layout::INITRAMFS_ADDR))
            .expect("read back the initramfs bytes from real guest RAM");
        assert_eq!(initramfs_back, initramfs, "initramfs must land verbatim in real guest RAM");

        let outcome = mv.run_to_first_halt().expect("boot with an (unread) initramfs must still run cleanly");
        assert_eq!(
            String::from_utf8_lossy(&outcome.console_output),
            HELLO_GUEST_MARKER,
            "the initramfs write must not disturb the ordinary boot flow"
        );
    }

    /// specs/baud-multiverse.md §4 / todo.md §3.2's `cpuid_leaves_are_fixed`, closed for real
    /// against real KVM hardware for the first time (todo.md §14's H2 gap: only a synthetic
    /// `kvm_cpuid_entry2` payload had ever been fed through `apply_determinism_mask` in isolation
    /// — no test had ever read a leaf back from a live vCPU via `KVM_GET_CPUID2`, which is the
    /// only way to know `KVM_SET_CPUID2` + the host's own leaf-merging behavior actually served
    /// what the mask table intended). Boot the hello image twice, read every served leaf back from
    /// each vCPU, and assert: (1) the two full leaf sets are byte-identical (masking is
    /// reproducible end to end, not just at the pure-function level `cpuid.rs`'s own unit tests
    /// already cover); (2) RDRAND/x2APIC (01H:ECX[30]/[21]) and RDSEED/TSX-HLE/TSX-RTM
    /// (07H:EBX[18]/[4]/[11]) all read back cleared on the live vCPU, not merely in the
    /// intermediate masked buffer `set_cpuid2` was given.
    #[test]
    fn cpuid_leaves_are_fixed() {
        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let first_cpuid = first
            .guest
            .vcpu
            .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
            .expect("KVM_GET_CPUID2 (first boot)");

        let second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let second_cpuid = second
            .guest
            .vcpu
            .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
            .expect("KVM_GET_CPUID2 (second boot)");

        assert_eq!(
            first_cpuid.as_slice(),
            second_cpuid.as_slice(),
            "every served CPUID leaf must read back identical across two boots of the same image"
        );

        let leaf1 = first_cpuid
            .as_slice()
            .iter()
            .find(|e| e.function == 0x1 && e.index == 0)
            .expect("leaf 01H:0 must be present in the served CPUID set");
        assert_eq!(leaf1.ecx & (1 << 30), 0, "RDRAND (01H:ECX[30]) must read back cleared");
        assert_eq!(leaf1.ecx & (1 << 21), 0, "x2APIC (01H:ECX[21]) must read back cleared");

        let leaf7 = first_cpuid
            .as_slice()
            .iter()
            .find(|e| e.function == 0x7 && e.index == 0)
            .expect("leaf 07H:0 must be present in the served CPUID set");
        assert_eq!(leaf7.ebx & (1 << 18), 0, "RDSEED (07H:EBX[18]) must read back cleared");
        assert_eq!(leaf7.ebx & (1 << 4), 0, "TSX HLE (07H:EBX[4]) must read back cleared");
        assert_eq!(leaf7.ebx & (1 << 11), 0, "TSX RTM (07H:EBX[11]) must read back cleared");
    }

    /// `tests/fixtures/tape-echo-guest/`'s payload: reads exactly 4 bytes from the tape device's
    /// `DATA` port (`tape_bus::TAPE_DEVICE_BASE + baud_tape_device::reg::DATA` = `0x0500`, one
    /// real single-byte `IN` per byte) and echoes each one straight to COM1 (`out dx, al`, port
    /// `0x3f8`), then halts — see that directory's `BUILD.md` for exact provenance.
    fn tape_echo_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tape-echo-guest/bzImage")
    }

    /// specs/baud-tape-device.md §5 / todo.md test-matrix row 21's `all_input_is_tape_derived`,
    /// exercised for the first time against real KVM hardware (todo.md §14's H2 gap: the tape
    /// device was previously only unit-tested at the pure device-model level — a real guest
    /// executing real `IN`/`OUT` instructions against the real PIO exit path had never actually
    /// consumed a tape byte and produced observable output driven by it). Two runs on the same
    /// tape must produce byte-identical console output ("the tape device as the sole input
    /// channel" is deterministic); changing one tape byte must change the output (input is
    /// genuinely flowing from the tape, not a synthetic stand-in for it — the exact "fake
    /// determinism" risk test-matrix row 21 exists to rule out).
    #[test]
    fn all_input_is_tape_derived() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let tape_a = vec![0x11, 0x22, 0x33, 0x44];
        let tape_b = vec![0x11, 0x22, 0x33, 0x99]; // differs in the last byte only

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, tape_a.clone(), None)
            .expect("first boot (tape A) failed");
        let first_outcome = first.run_to_first_halt().expect("first run (tape A) failed");
        assert_eq!(
            first_outcome.console_output, tape_a,
            "guest must echo exactly the 4 tape bytes it read, byte for byte"
        );

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, tape_a.clone(), None)
            .expect("second boot (tape A) failed");
        let second_outcome = second.run_to_first_halt().expect("second run (tape A) failed");
        assert_eq!(
            second_outcome.console_output, first_outcome.console_output,
            "same tape twice must produce byte-identical guest output"
        );

        let mut third = Multiverse::boot(&kernel, cmdline, 0, 1, tape_b.clone(), None)
            .expect("third boot (tape B) failed");
        let third_outcome = third.run_to_first_halt().expect("third run (tape B) failed");
        assert_eq!(
            third_outcome.console_output, tape_b,
            "guest must echo exactly tape B's bytes, not a stale/synthetic copy of tape A's"
        );
        assert_ne!(
            third_outcome.console_output, first_outcome.console_output,
            "changing one tape byte must change the guest's observable output — otherwise the \
             guest is not actually reading its input from the tape"
        );
    }

    /// `tests/fixtures/rdrand-guest/`'s payload: executes `rdrand eax` directly (ignoring the
    /// masked CPUID feature bit — an adversarial/non-compliant guest) and echoes the 4 raw result
    /// bytes to COM1, then halts. See that directory's `BUILD.md` for exact provenance and why
    /// this is expected to diverge under the cooperative regime.
    fn rdrand_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rdrand-guest/bzImage")
    }

    /// The single marker byte (`'X'`, 0x58) `tests/fixtures/rdrand-guest/payload.s` writes to
    /// COM1 *before* attempting `rdrand` — the only byte this fixture ever emits under the
    /// cooperative regime, per the real-hardware finding below.
    const RDRAND_GUEST_MARKER: &[u8] = b"X";

    /// specs/baud-multiverse.md §3.2 / §8, todo.md test-matrix row 1's `rdrand_guest_is_flagged`
    /// (cooperative-regime half — the enforced-regime `Crash{detail:"rdrand"}` half needs the
    /// custom KVM module tracked as future work in specs/baud-host.md §8 and is not built).
    ///
    /// **Real-hardware finding, corrects the spec's original assumption**: masking RDRAND out of
    /// CPUID does not merely *discourage* a compliant guest from the instruction while leaving an
    /// adversarial one free to execute it and diverge — on real VT-x hardware, `rdrand` itself
    /// raises `#UD` whenever the guest's configured CPUID reports the feature absent
    /// (`01H:ECX[30]=0`), exactly as the Intel SDM's own instruction reference describes.
    /// `rdrand-guest`'s payload has no IDT, so that `#UD` cascades straight to a triple fault,
    /// which `baud-vcpu`'s run loop already treats identically to a clean `Hlt`
    /// (`VcpuExit::Shutdown` -> `DispatchOutcome::Halted`, `crates/baud-vcpu/src/lib.rs`). The
    /// guest therefore *never reaches* the instructions after `rdrand` at all — confirmed by the
    /// marker byte the payload writes immediately before attempting it: two boots produce the
    /// identical single-byte output, not a divergent one. This is a **stronger** guarantee than
    /// the spec originally assumed: under the cooperative regime the raw random instruction is
    /// hardware-unreachable for *any* guest, compliant or not, rather than merely caught after
    /// the fact via double-run comparison. (specs/baud-multiverse.md was updated to match this
    /// finding.)
    #[test]
    fn rdrand_guest_is_flagged() {
        let kernel = rdrand_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let first_outcome = first.run_to_first_halt().expect("first run failed");
        assert_eq!(
            first_outcome.console_output, RDRAND_GUEST_MARKER,
            "guest must never get past the pre-rdrand marker: rdrand with a masked CPUID feature \
             bit must #UD immediately (real hardware behavior), not execute and produce output"
        );

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let second_outcome = second.run_to_first_halt().expect("second run failed");
        assert_eq!(
            second_outcome.console_output, first_outcome.console_output,
            "the guest's forced #UD/triple-fault on rdrand must be perfectly deterministic across \
             two boots — this is what makes the cooperative regime's CPUID mask a hardware \
             guarantee rather than a mere hint"
        );
    }

    /// `tests/fixtures/rdtsc-guest/`'s payload: writes one marker byte (`'T'`) to COM1, then
    /// executes the raw `rdtsc` instruction directly, packs `edx:eax` into one 64-bit value, and
    /// echoes its 8 bytes to COM1 low-byte-first before halting. See that directory's `BUILD.md`
    /// for exact provenance and why only the high bits are asserted reproducible.
    fn rdtsc_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rdtsc-guest/bzImage")
    }

    /// The single marker byte `tests/fixtures/rdtsc-guest/payload.s` writes to COM1 before
    /// executing `rdtsc` — unlike `rdrand-guest`'s marker (an unreachable-in-practice guard,
    /// `RDRAND_GUEST_MARKER` above), `rdtsc` has no CPUID gate to trip, so the guest always gets
    /// past this byte and always echoes the 8 value bytes that follow it.
    const RDTSC_GUEST_MARKER: u8 = b'T';

    /// The number of low bits of a raw `rdtsc` read this test tolerates disagreeing across two
    /// otherwise-identical boots — see `tests/fixtures/rdtsc-guest/BUILD.md`'s "Bit-exactness
    /// expectation" section for the exact rationale: generous relative to the real host-scheduling
    /// jitter actually observed between `pin_tsc_value` and the guest's first `rdtsc`, but nowhere
    /// near large enough to mask an actually-unpinned TSC (which would disagree by billions of
    /// counts, not tens of bits).
    const RDTSC_JITTER_MASK: u64 = !0u64 << 20;

    /// todo.md §3.3 / test-matrix row 1's RDTSC-compliance half of "randomness + time control" —
    /// the half `rdrand_guest_is_flagged` above does not cover: RDTSC has no CPUID gate, so a
    /// compliant guest reading it directly needs the VMM itself to serve a reproducible value,
    /// not a hardware `#UD` to fall back on. Proves [`pin_tsc_value`]'s
    /// `KVM_SET_MSRS(IA32_TSC=0)` actually closes the gap flagged in every "Not yet done" note
    /// since H3: before that call existed, a raw `rdtsc` reflected implicit host-wall-clock state,
    /// so two boots would disagree by however many host-TSC counts separated their real start
    /// times (typically billions at native GHz rates), not just scheduling jitter in the low
    /// bits.
    #[test]
    fn rdtsc_guest_reproduces_high_bits_across_boots() {
        let kernel = rdtsc_guest_kernel_path();
        let cmdline = "console=ttyS0";

        // Warm-up boot, result discarded: real-hardware finding, todo.md §14 — this fixture's
        // very *first* boot in a process consistently reads a raw `rdtsc` several million counts
        // higher than every boot after it (cold page-cache fill for the bzImage file, first-ever
        // KVM/perf_event syscalls in this process, etc. — one-time costs `pin_tsc_value` cannot
        // account for since they happen *after* it runs, inside `run_to_first_halt`). Comparing
        // two already-warm boots isolates the steady-state jitter this test actually cares about.
        Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None)
            .expect("warm-up boot failed")
            .run_to_first_halt()
            .expect("warm-up run failed");

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let first_outcome = first.run_to_first_halt().expect("first run failed");
        assert_eq!(
            first_outcome.console_output.len(),
            9,
            "guest must get past the marker and echo all 8 rdtsc value bytes: {:?}",
            first_outcome.console_output
        );
        assert_eq!(first_outcome.console_output[0], RDTSC_GUEST_MARKER);
        let first_tsc = u64::from_le_bytes(first_outcome.console_output[1..9].try_into().unwrap());

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let second_outcome = second.run_to_first_halt().expect("second run failed");
        assert_eq!(second_outcome.console_output.len(), 9);
        assert_eq!(second_outcome.console_output[0], RDTSC_GUEST_MARKER);
        let second_tsc = u64::from_le_bytes(second_outcome.console_output[1..9].try_into().unwrap());

        assert_eq!(
            first_tsc & RDTSC_JITTER_MASK,
            second_tsc & RDTSC_JITTER_MASK,
            "raw rdtsc must reproduce in its high bits across two boots once the TSC value is \
             pinned at boot (KVM_SET_MSRS(IA32_TSC=0)) — first={first_tsc:#x} second={second_tsc:#x}"
        );
    }

    /// Enforced-regime counterpart to `rdtsc_guest_reproduces_high_bits_across_boots`, reusing the
    /// exact same `rdtsc-guest` fixture. Requires the patched `kvm_intel.ko` (this crate's own
    /// `handle_baud_rdtsc_exit`, `kernel-module/baud-enforced/rdtsc-enforce.patch`) to already be
    /// loaded in place of the stock module — `#[ignore]`d so a normal `cargo test --workspace`
    /// (stock module, todo.md's mandatory green-build protocol) never runs it; only
    /// `drive/manual/h3-enforced-rdtsc.sh` invokes it by name, after swapping the module in and before
    /// swapping the stock one back.
    ///
    /// Under the *stock* module RDTSC never traps, so a raw `rdtsc` read still reflects real
    /// (pinned, but only jitter-tolerant) hardware state — the same "high bits only" ceiling
    /// `rdtsc_guest_reproduces_high_bits_across_boots` documents. Under the *patched* module every
    /// `rdtsc` traps to `handle_baud_rdtsc_exit` and is served `WorkClock::serve_enforced_rdtsc()`
    /// — a pure function of the branch counter, not real time — so this asserts full 64-bit
    /// equality across two boots, not just the high bits: the enforced regime's actual promise
    /// (todo.md §3.8, test-matrix row 1).
    #[test]
    #[ignore = "needs the patched enforced-regime kvm_intel.ko loaded; see drive/manual/h3-enforced-rdtsc.sh"]
    fn rdtsc_enforced_regime_is_bit_exact_across_boots() {
        let kernel = rdtsc_guest_kernel_path();
        let cmdline = "console=ttyS0";

        // Same warm-up rationale as rdtsc_guest_reproduces_high_bits_across_boots (todo.md §14):
        // isolate steady-state behavior from one-time first-boot costs in this process.
        Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None)
            .expect("warm-up boot failed")
            .run_to_first_halt()
            .expect("warm-up run failed");

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let first_outcome =
            first.run_to_first_halt().expect("first run failed (enforced RDTSC exit not served?)");
        assert_eq!(first_outcome.console_output.len(), 9);
        assert_eq!(first_outcome.console_output[0], RDTSC_GUEST_MARKER);
        let first_tsc = u64::from_le_bytes(first_outcome.console_output[1..9].try_into().unwrap());

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let second_outcome = second.run_to_first_halt().expect("second run failed");
        assert_eq!(second_outcome.console_output.len(), 9);
        assert_eq!(second_outcome.console_output[0], RDTSC_GUEST_MARKER);
        let second_tsc = u64::from_le_bytes(second_outcome.console_output[1..9].try_into().unwrap());

        assert_eq!(
            first_tsc, second_tsc,
            "enforced-regime RDTSC is served entirely from the work-clock (a pure function of the \
             branch counter), so it must reproduce bit-for-bit across two boots, not just in its \
             high bits — first={first_tsc:#x} second={second_tsc:#x}"
        );
    }

    /// Enforced-regime counterpart to `rdrand_guest_is_flagged`, reusing the exact same
    /// `rdrand-guest` fixture (`RDRAND_GUEST_MARKER`/`rdrand_guest_kernel_path` above) — its
    /// post-`rdrand` echo loop is unreachable under the cooperative regime (masked CPUID's
    /// hardware `#UD`) but was built for exactly this test, per that fixture's own `BUILD.md`.
    /// Requires the patched `kvm_intel.ko` (this crate's `handle_baud_rdrand_exit`,
    /// `kernel-module/baud-enforced/rdrand-enforce.patch`, layered on
    /// `rdtsc-enforce.patch`) already loaded in place of the stock module — `#[ignore]`d so a
    /// normal `cargo test --workspace` (stock module) never runs it; only
    /// `drive/manual/h3-enforced-rdrand.sh` invokes it by name.
    ///
    /// Under the *enforced* regime, `SECONDARY_EXEC_RDRAND_EXITING` traps the `rdrand` **before**
    /// the CPUID-gated `#UD` check the cooperative regime relies on, so the guest reaches the echo
    /// loop and outputs the marker plus 4 value bytes (`RDRAND_GUEST_MARKER.len() + 4 == 5`) —
    /// served from `WorkClock::serve_enforced_rdrand()`, a deterministic tape-seeded PRNG draw,
    /// not real hardware entropy, so two boots of the same (empty) tape must reproduce bit-for-bit.
    #[test]
    #[ignore = "needs the patched enforced-regime kvm_intel.ko loaded; see drive/manual/h3-enforced-rdrand.sh"]
    fn rdrand_enforced_regime_is_bit_exact_across_boots() {
        let kernel = rdrand_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let first_outcome =
            first.run_to_first_halt().expect("first run failed (enforced RDRAND exit not served?)");
        assert_eq!(
            first_outcome.console_output.len(),
            RDRAND_GUEST_MARKER.len() + 4,
            "guest must get past the marker and echo all 4 rdrand value bytes under the enforced \
             regime: {:?}",
            first_outcome.console_output
        );
        assert_eq!(&first_outcome.console_output[..RDRAND_GUEST_MARKER.len()], RDRAND_GUEST_MARKER);

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let second_outcome =
            second.run_to_first_halt().expect("second run failed (enforced RDRAND exit not served?)");
        assert_eq!(
            second_outcome.console_output, first_outcome.console_output,
            "enforced-regime RDRAND is served entirely from a tape-seeded deterministic PRNG, so it \
             must reproduce bit-for-bit across two boots of the same (empty) tape"
        );
    }

    /// `tests/fixtures/rdseed-guest/`'s payload: writes one marker byte (`'S'`) to COM1, then hits
    /// the `UD2` that `baud_packages::rewrite_rdseed`'s build-time pass left where an `rdseed eax`
    /// used to be, then echoes the 4 raw result bytes to COM1 and halts. Unlike `rdrand-guest`,
    /// **the checked-in image contains no `rdseed` opcode at all** — the rewrite is already baked
    /// in, exactly as a real `baud image build` would emit it. See that directory's `BUILD.md`.
    fn rdseed_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rdseed-guest/bzImage")
    }

    /// The single marker byte `tests/fixtures/rdseed-guest/payload.s` writes to COM1 *before* the
    /// rewritten site — the only byte the fixture emits whenever the `UD2` is not served (stock
    /// module, or enforced module with the site unregistered), since it has no IDT and the
    /// resulting `#UD` triple-faults.
    const RDSEED_GUEST_MARKER: &[u8] = b"S";

    /// Guest address of `rdseed-guest`'s one `UD2`, and the site descriptor for it. Hand-verified
    /// and derived in `tests/fixtures/rdseed-guest/BUILD.md`'s "Where the UD2 is" table
    /// (`layout::KERNEL_LOAD_ADDR + layout::KERNEL_64BIT_ENTRY_OFFSET + 0x07`); `build.py` re-prints
    /// the same three numbers on every regeneration of that image. Hardcoded rather than derived
    /// from a real image build because `rdseed-guest` is a hand-assembled flat binary, not the ELF
    /// `baud_packages::rewrite_rdseed` parses — it never produces a `RdseedRewriteReport`/sidecar
    /// of its own for `baud-server`'s `rdseed_sites` loader (todo.md §14) to pick up, unlike a real
    /// ELF-based guest image now would. Same "fixed, hand-verified binary" arrangement
    /// `rdtsc-guest`/`rdrand-guest` already use.
    const RDSEED_GUEST_UD2_ADDR: u64 = 0x0020_0207;
    /// `gpr_index: 0` == `RAX`/`EAX` (the `0F C7 F8` encoding's ModRM `rm` field);
    /// `length: 3` == the original `RDSEED r32` encoding, so a served value resumes at
    /// `RDSEED_GUEST_UD2_ADDR + 3` — past the `NOP` padding, not at it.
    const RDSEED_GUEST_SITE: baud_vcpu::EnforcedRdseedSite =
        baud_vcpu::EnforcedRdseedSite { gpr_index: 0, length: 3 };

    /// Enforced-regime RDSEED (todo.md §4, §3.8), the third and last of the enforced-instruction
    /// tests. Requires the patched `kvm_intel.ko` (`kernel-module/baud-enforced/ud2-enforce.patch`
    /// layered on `rdrand-enforce.patch` on `rdtsc-enforce.patch`) already loaded in place of the
    /// stock module — `#[ignore]`d so a normal `cargo test --workspace` (stock module) never runs
    /// it; only `drive/manual/h3-enforced-rdseed.sh` invokes it by name.
    ///
    /// RDSEED's enforced path is structurally different from RDTSC's and RDRAND's, and this is what
    /// proves that difference works end to end. `SECONDARY_EXEC_RDSEED_EXITING` is not settable on
    /// this host's VMX microcode at all (`kernel-module/baud-enforced/BUILD.md`'s probe report), so
    /// the instruction is never trapped as an instruction: `baud-packages` rewrites every `rdseed`
    /// opcode to `UD2` + `NOP` at **build** time, and the `UD2`'s ordinary `#UD` exception exit
    /// (already in stock KVM's exception bitmap — no exec-control patch needed) is what reaches
    /// userspace. Because a `UD2` carries no destination-register encoding and no length, both come
    /// from this crate's own site table (`RDSEED_GUEST_SITE`, registered via
    /// [`Multiverse::boot_with_rdseed_sites`]) rather than from the trap — which is exactly why the
    /// kernel handler leaves RIP *at* the `UD2` and userspace advances it.
    ///
    /// Asserts the same end state as its two siblings: the guest gets past the marker (so the
    /// value really was served, not `#UD`-injected) and the 4 echoed bytes are bit-identical across
    /// two boots of the same (empty) tape, since `serve_enforced_rdseed` draws from the same
    /// tape-seeded `SplitMix64` stream `serve_enforced_rdrand` does.
    #[test]
    #[ignore = "needs the patched enforced-regime kvm_intel.ko loaded; see drive/manual/h3-enforced-rdseed.sh"]
    fn rdseed_enforced_regime_is_bit_exact_across_boots() {
        let kernel = rdseed_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let sites = [(RDSEED_GUEST_UD2_ADDR, RDSEED_GUEST_SITE)];

        let mut first =
            Multiverse::boot_with_rdseed_sites(&kernel, cmdline, 0, 1, vec![], None, None, sites)
                .expect("first boot failed");
        let first_outcome =
            first.run_to_first_halt().expect("first run failed (enforced RDSEED exit not served?)");
        assert_eq!(
            first_outcome.console_output.len(),
            RDSEED_GUEST_MARKER.len() + 4,
            "guest must get past the marker and echo all 4 served rdseed value bytes under the \
             enforced regime — a marker-only output means the UD2's #UD was re-injected instead of \
             served, i.e. the site table did not match the trapping RIP: {:?}",
            first_outcome.console_output
        );
        assert_eq!(&first_outcome.console_output[..RDSEED_GUEST_MARKER.len()], RDSEED_GUEST_MARKER);

        let mut second =
            Multiverse::boot_with_rdseed_sites(&kernel, cmdline, 0, 1, vec![], None, None, sites)
                .expect("second boot failed");
        let second_outcome =
            second.run_to_first_halt().expect("second run failed (enforced RDSEED exit not served?)");
        assert_eq!(
            second_outcome.console_output, first_outcome.console_output,
            "enforced-regime RDSEED is served entirely from a tape-seeded deterministic PRNG, so it \
             must reproduce bit-for-bit across two boots of the same (empty) tape"
        );
    }

    /// The other half of `ud2-enforce.patch`'s contract, and the one that keeps it safe to load at
    /// all: a `UD2` the site table does *not* know about must have its `#UD` re-injected untouched,
    /// never served a bogus value. A real guest kernel's `BUG()`/`WARN_ON()` compiles to a bare
    /// `UD2`, and every genuinely invalid opcode raises the same `#UD`, so a handler that served
    /// every trapping `UD2` would silently turn kernel panics into wrong-answer executions.
    ///
    /// Uses the same `rdseed-guest` fixture with an **empty** site table (plain [`Multiverse::boot`]
    /// — which is what every other caller in this workspace already does), making its one `UD2`
    /// indistinguishable, from the VMM's point of view, from an unrelated `BUG()`. Expected end
    /// state is byte-identical to what the *stock* module produces for this image: `#UD` injected
    /// at the untouched RIP, no IDT to catch it, triple fault, `DispatchOutcome::Halted`, marker
    /// byte only.
    ///
    /// The two failure modes this catches are exactly the two that matter. If
    /// `handle_baud_ud2_exit` served a value regardless of the table (or if `resolve_rdseed_site`
    /// matched too loosely), the guest would run on and emit 5 bytes instead of 1. If `reinject_ud`
    /// failed to inject anything, RIP would still be sitting on the `UD2` (the kernel handler never
    /// advances it) and the guest would re-trap the same instruction forever — this test would hang
    /// rather than fail, which is itself a legible signal in `drive/manual/h3-enforced-rdseed.sh`'s output.
    #[test]
    #[ignore = "needs the patched enforced-regime kvm_intel.ko loaded; see drive/manual/h3-enforced-rdseed.sh"]
    fn ud2_outside_the_rdseed_site_table_reinjects_ud() {
        let kernel = rdseed_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let mut guest =
            Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("boot failed");
        let outcome = guest.run_to_first_halt().expect("run failed");
        assert_eq!(
            outcome.console_output, RDSEED_GUEST_MARKER,
            "a UD2 with no registered rdseed site must have its #UD re-injected verbatim (a real \
             BUG()/WARN_ON() or invalid opcode must keep faulting exactly as it would with no \
             patch loaded), so this guest must never get past the marker: {:?}",
            outcome.console_output
        );
    }

    /// `tests/fixtures/framebuffer-guest/`'s payload: writes one marker byte (`'F'`) to COM1,
    /// then writes a 2x2 `Indexed8` frame (pixels `10, 20, 30, 40`) to the tape device and
    /// finalizes it with the `FRAME` control opcode, then halts. See that directory's `BUILD.md`
    /// for exact provenance — the first guest fixture in this workspace to exercise
    /// `baud_tape_device::ControlOp::Frame`.
    fn framebuffer_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/framebuffer-guest/bzImage")
    }

    /// The single marker byte `tests/fixtures/framebuffer-guest/payload.s` writes to COM1 before
    /// writing the frame — mirrors `RDTSC_GUEST_MARKER`'s role: a cheap sanity check that the
    /// guest actually ran past its first instruction before we go looking at drained tape records.
    const FRAMEBUFFER_GUEST_MARKER: u8 = b'F';

    /// specs/baud-stream.md §7's own named test (`frame_hashes_double_run_identical`), run for the
    /// first time against a real guest on real `/dev/kvm` instead of `baud-stream`'s crate-level
    /// synthetic buffers: proves `baud_tape_device::ControlOp::Frame` (todo.md §14's "framebuffer
    /// stream" gap — no real device ever produced a `Msg::Frame` before this) is a deterministic
    /// tape-device record like every other opcode, not just a wire type nothing ever populated.
    #[test]
    fn framebuffer_guest_frame_is_reproducible_across_boots() {
        let kernel = framebuffer_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let first_outcome = first.run_to_first_halt().expect("first run failed");
        assert_eq!(first_outcome.console_output, vec![FRAMEBUFFER_GUEST_MARKER]);
        let first_records = first.drain_tape_records();

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let second_outcome = second.run_to_first_halt().expect("second run failed");
        assert_eq!(second_outcome.console_output, vec![FRAMEBUFFER_GUEST_MARKER]);
        let second_records = second.drain_tape_records();

        assert_eq!(first_records.len(), 1, "guest emits exactly one Frame record: {first_records:?}");
        assert_eq!(second_records.len(), 1, "guest emits exactly one Frame record: {second_records:?}");

        let as_frame = |records: Vec<baud_proto::Msg>| match records.into_iter().next() {
            Some(baud_proto::Msg::Frame(rec)) => rec,
            other => panic!("expected exactly one Msg::Frame, got {other:?}"),
        };
        let first_frame = as_frame(first_records);
        let second_frame = as_frame(second_records);

        assert_eq!(first_frame.width, 2);
        assert_eq!(first_frame.height, 2);
        assert_eq!(first_frame.format, baud_proto::PixFmt::Indexed8);
        assert_eq!(first_frame.bytes.as_deref(), Some([10u8, 20, 30, 40].as_slice()));

        assert_eq!(
            first_frame.hash, second_frame.hash,
            "the same guest fixture on the same tape must produce byte-identical frame hashes \
             across two boots — this is what makes baud-stream's frame-hash journaling a real \
             determinism guarantee rather than an assumption"
        );
        assert_eq!(first_frame.bytes, second_frame.bytes);
        assert_eq!(first_frame.width, second_frame.width);
        assert_eq!(first_frame.height, second_frame.height);
        assert_eq!(first_frame.format, second_frame.format);

        // Confirm the transport's hash actually matches baud-stream's own fingerprint function —
        // the two crates must agree on what "the frame's hash" means, or downstream verification
        // (frame hashes joining `verify determinism`, specs/baud-stream.md §4) would be comparing
        // apples to oranges.
        let expected_hash = baud_stream::fingerprint(
            first_frame.bytes.as_deref().unwrap(),
            first_frame.width,
            first_frame.height,
            &first_frame.format,
        ).expect("fixture's frame geometry must be internally consistent");
        assert_eq!(first_frame.hash, expected_hash, "tape-device hash must match baud_stream::fingerprint");
    }

    /// `tests/fixtures/timer-guest/`'s payload: builds a real IDT (one gate at vector `0x30`
    /// pointing at a handler that writes one marker byte to COM1 and `iretq`s back), enables
    /// interrupts, then busy-loops long enough to absorb several injected ticks before a clean
    /// `hlt`. See that directory's `BUILD.md` for exact provenance, including the real
    /// in-memory-GDT gap this fixture's existence surfaced.
    fn timer_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/timer-guest/bzImage")
    }

    /// The vector `tests/fixtures/timer-guest/payload.s`'s IDT gate is registered at, and the
    /// single marker byte its handler writes to COM1 every time it is entered.
    const TIMER_VECTOR: u8 = 0x30;
    const TIMER_MARKER: u8 = b'T';

    /// The largest `rcb` disagreement between two otherwise-identical runs the tests below
    /// tolerate. **This is now `0` — two boots of the same image+tape must agree on the landing
    /// `rcb` exactly.**
    ///
    /// It was `8` for as long as the work-clock counter was contaminated by host branches. The
    /// residual ±1-4 (and, in the worst measured case, ±34) disagreement this constant used to
    /// absorb was never "the `perf_event` branch counter's own hardware read precision", as its
    /// previous doc concluded, and `exclude_host` was never actually ruled out: the counter was
    /// being built with `perf_event::Builder`'s default `exclude_kernel = 1` still set, which
    /// filters out a CPL-0 guest entirely, so what this "RCB" measured was host **userspace**
    /// branches retired inside each `KVM_RUN` ioctl (~54 per exit) — a host-scheduling-sensitive
    /// quantity with no guest meaning at all. `LinuxBranchCounter::new`'s comment carries the
    /// measurements; the same root cause is what made `run_to_events` overshoot its target
    /// (todo.md §14.1).
    ///
    /// With the counter counting the guest's own retired conditional branches and nothing else, the
    /// landing `rcb` is a pure function of the guest's instruction stream, so there is no host term
    /// left for a tolerance to absorb. Tightened to `0` only on real-hardware evidence, not
    /// speculatively: 10/10 idle repetitions plus 20/20 repetitions with every logical core
    /// saturated by competing busy loops, of both tests that use it, all exactly equal.
    /// Deliberately kept as a named constant (rather than inlining `assert_eq!`) so this history
    /// stays attached to the assertion and a future regression that reintroduces host contamination
    /// has an obvious, documented place to be caught rather than accommodated.
    const RCB_HARDWARE_JITTER_TOLERANCE: u64 = 0;

    /// H4's named test (specs/baud-vcpu.md §5, todo.md §10): `Multiverse::inject_timer_tick`
    /// wired for the first time against a real guest and real KVM hardware. Drives the same
    /// image+tape through `run_with_timer_ticks` twice and asserts, per tick, the landed `rip` is
    /// byte-identical across both runs (`rcb` within [`RCB_HARDWARE_JITTER_TOLERANCE`], see its
    /// doc) — the real-hardware counterpart to `baud_vcpu::boundary`'s
    /// `identical_target_yields_identical_injection_tuple_across_runs` (which only ever exercised
    /// a scripted fake stepper and so never hit real counter-read precision limits). Also asserts
    /// the guest actually took each interrupt (one marker byte per tick, in order) and that the
    /// final halt state (console output, RAM hash) is itself identical across the two runs — an
    /// interrupt landing at a genuinely different point, or corrupting guest state differently,
    /// would show up here too.
    #[test]
    fn timer_tick_lands_at_identical_instruction() {
        let kernel = timer_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const PERIOD_RCB: u64 = 100_000;
        const NUM_TICKS: u32 = 2;

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let (first_ticks, first_halt) = first
            .run_with_timer_ticks(PERIOD_RCB, TIMER_VECTOR, NUM_TICKS)
            .expect("first run with timer ticks failed");

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let (second_ticks, second_halt) = second
            .run_with_timer_ticks(PERIOD_RCB, TIMER_VECTOR, NUM_TICKS)
            .expect("second run with timer ticks failed");

        assert_eq!(first_ticks.len(), NUM_TICKS as usize);
        assert_eq!(second_ticks.len(), NUM_TICKS as usize);
        for (i, (a, b)) in first_ticks.iter().zip(second_ticks.iter()).enumerate() {
            assert_eq!(
                a.rip, b.rip,
                "tick {i}: the interrupt must land on the bit-identical instruction across two \
                 boots of the same image+tape — this is the real-hardware version of \
                 timer_tick_lands_at_identical_instruction"
            );
            let rcb_diff = a.rcb.abs_diff(b.rcb);
            // Bound read through a binding, not compared against the constant directly: the
            // tolerance is `0` now, and a literal `x <= 0` on a `u64` is what
            // `clippy::absurd_extreme_comparisons` (deny-by-default in this workspace's gate)
            // rejects. Keeping it a real `<=` bound rather than an `assert_eq!` means raising the
            // constant again, should some future host ever need it, still does the right thing.
            let tolerance = RCB_HARDWARE_JITTER_TOLERANCE;
            assert!(
                rcb_diff <= tolerance,
                "tick {i}: rcb disagreement {rcb_diff} (a={}, b={}) exceeds the tolerance of \
                 {tolerance} — see RCB_HARDWARE_JITTER_TOLERANCE's doc",
                a.rcb,
                b.rcb
            );
        }

        let expected_output = vec![TIMER_MARKER; NUM_TICKS as usize];
        assert_eq!(
            first_halt.console_output, expected_output,
            "the guest must actually take each injected interrupt exactly once, in order \
             (one marker byte per tick)"
        );
        assert_eq!(
            second_halt.console_output, first_halt.console_output,
            "console output after the injected ticks must be identical across two boots"
        );
        assert_eq!(
            second_halt.ram_hash, first_halt.ram_hash,
            "guest RAM at first Hlt must be byte-identical across two boots even with \
             interrupts injected mid-run"
        );
    }

    /// todo.md §14's "Guest boot pipeline" next-action: wire H4 into the run loop for a guest whose
    /// tick count is not known ahead of time (a real kernel's periodic scheduler timer, the
    /// prerequisite for ever reaching a real-kernel's own `calibrate_delay()`/scheduler tick without
    /// hanging). `run_to_first_halt_with_periodic_timer` must (a) keep injecting ticks across the
    /// guest's whole natural lifetime without the caller pre-computing a tick count, (b) detect the
    /// guest's own halt gracefully (never as an error) whenever it falls before the next scheduled
    /// tick, and (c) do both reproducibly — the real-hardware counterpart to `boundary.rs`'s
    /// scripted-stepper `reports_halted_instead_of_injecting_when_guest_halts_before_target`, which
    /// only ever exercised a fake stepper and so never hit a real halted vCPU or real hardware
    /// counter-read precision.
    #[test]
    fn periodic_timer_injection_halts_gracefully_and_reproducibly() {
        let kernel = timer_guest_kernel_path();
        let cmdline = "console=ttyS0";
        // Small enough relative to `timer-guest`'s busy loop (BUILD.md) that the guest survives a
        // handful of ticks before its own `hlt`, exercising the open-ended path -- not just the
        // "exactly N pre-known ticks" path `run_with_timer_ticks` already covers -- while keeping
        // the tick count low: each tick independently carries the same real hardware
        // branch-counter read jitter `timer_tick_lands_at_identical_instruction` already documents
        // (`RCB_HARDWARE_JITTER_TOLERANCE`), so many more ticks than needed would just multiply the
        // chance any single one exceeds that per-tick tolerance under load. Empirically calibrated
        // (5 ticks, stable across repeated real-hardware runs) against the raw `BR_INST_RETIRED.
        // COND` event `LinuxBranchCounter`/`measure_fixed_loop_branches` now both use (see
        // `BR_INST_RETIRED_COND`'s doc) -- this constant was `2_000_000` under the previously
        // wrongly-used generic `PERF_COUNT_HW_BRANCH_INSTRUCTIONS` event, which counted host-side
        // branches this raw guest-only budget does not include, so the two numbers are not
        // comparable.
        const PERIOD_RCB: u64 = 200_000;
        const MAX_TICKS: u32 = 20;

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let (first_ticks, first_halt) = first
            .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
            .expect("first periodic run failed");

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let (second_ticks, second_halt) = second
            .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
            .expect("second periodic run failed");

        assert!(
            !first_ticks.is_empty(),
            "the guest's busy loop must survive at least one periodic tick before its own natural halt"
        );
        assert_eq!(
            first_ticks.len(),
            second_ticks.len(),
            "the same image+tape must survive the same number of periodic ticks before halting \
             on its own -- this is what proves the graceful-halt path is deterministic, not just \
             non-erroring"
        );
        for (i, (a, b)) in first_ticks.iter().zip(second_ticks.iter()).enumerate() {
            assert_eq!(
                a.rip, b.rip,
                "tick {i}: periodic injection must land on the bit-identical instruction across \
                 two boots of the same image+tape"
            );
            let rcb_diff = a.rcb.abs_diff(b.rcb);
            // See the identically-shaped bound in `timer_tick_lands_at_identical_instruction` for
            // why the tolerance is read through a binding rather than compared against directly.
            let tolerance = RCB_HARDWARE_JITTER_TOLERANCE;
            assert!(
                rcb_diff <= tolerance,
                "tick {i}: rcb disagreement {rcb_diff} (a={}, b={}) exceeds the tolerance of \
                 {tolerance} — see RCB_HARDWARE_JITTER_TOLERANCE's doc",
                a.rcb,
                b.rcb
            );
        }

        let expected_output = vec![TIMER_MARKER; first_ticks.len()];
        assert_eq!(
            first_halt.console_output, expected_output,
            "the guest must take exactly one marker byte per periodic tick before its own \
             natural halt, with no ticks lost or duplicated"
        );
        assert_eq!(
            second_halt.console_output, first_halt.console_output,
            "console output through the guest's own natural halt must be identical across two boots"
        );
        assert_eq!(
            second_halt.ram_hash, first_halt.ram_hash,
            "guest RAM at the guest's own natural halt must be byte-identical across two boots"
        );
    }

    /// A minimal `tracing::Subscriber` that records every event's `message` field verbatim, with
    /// no filtering/formatting layer — just enough to assert on in a test, without pulling in
    /// `tracing-subscriber` as a new dev-dependency for this one assertion.
    struct RecordingSubscriber {
        messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for RecordingSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct MessageVisitor(String);
            impl tracing::field::Visit for MessageVisitor {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            self.messages.lock().unwrap().push(visitor.0);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// todo.md §14 item 15's named observability gap: a real-kernel boot run
    /// (`run_to_first_halt_with_periodic_timer_and_devices`) used to be a total black box until it
    /// finished, timed out, or was killed. Proves the fix directly: the run loop now emits a
    /// `tracing` progress event carrying the tick count and running console-output length, visible
    /// on the very first tick (`tick_index == 0` always satisfies `% RUN_LOOP_PROGRESS_LOG_INTERVAL_
    /// TICKS == 0`) rather than only after `RUN_LOOP_PROGRESS_LOG_INTERVAL_TICKS` ticks have
    /// elapsed — the same low tick budget `periodic_timer_injection_halts_gracefully_and_reproducibly`
    /// above uses is enough to observe it, no larger/slower boot required.
    #[test]
    fn run_loop_progress_is_logged_via_tracing() {
        let kernel = timer_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const PERIOD_RCB: u64 = 200_000;
        const MAX_TICKS: u32 = 20;

        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = RecordingSubscriber { messages: messages.clone() };

        // Deliberately calls the private `_and_devices` engine directly (accessible here since
        // `tests` is a descendant module), not the public `run_to_first_halt_with_periodic_timer`
        // above -- that one is a separate, older loop that does not share this engine (and so does
        // not carry this progress logging); the real H9 boot path
        // (`run_until_console_pattern_with_periodic_timer_and_devices`, `baud-server`'s
        // `routes/run_kvm.rs`) always goes through this shared engine.
        let mut guest = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("boot failed");
        tracing::subscriber::with_default(subscriber, || {
            guest
                .run_to_first_halt_with_periodic_timer_and_devices(
                    PERIOD_RCB,
                    TIMER_VECTOR,
                    &[],
                    MAX_TICKS,
                    None,
                    0,
                )
                .expect("periodic run failed")
        });

        let messages = messages.lock().unwrap();
        let progress_lines: Vec<&String> = messages
            .iter()
            .filter(|m| m.contains("run_to_first_halt_with_periodic_timer_and_devices"))
            .collect();
        assert!(
            !progress_lines.is_empty(),
            "expected at least one run-loop progress log line, got: {messages:?}"
        );
        assert!(
            progress_lines[0].contains("tick 0/20"),
            "the very first progress line must report tick 0 (logged before any tick is injected), \
             got: {:?}",
            progress_lines[0]
        );
        assert!(
            progress_lines[0].contains("console_output"),
            "progress line must report the running console-output length, got: {:?}",
            progress_lines[0]
        );
    }

    /// `tests/fixtures/idle-halt-guest/`'s payload: halts immediately (no busy loop at all,
    /// unlike `timer-guest`), and its IDT handler only emits the target message once it has been
    /// woken `WAKES_BEFORE_MESSAGE` (5) times. See that directory's `BUILD.md` for why this
    /// fixture exists: proving `run_until_console_pattern_with_periodic_timer` actually resumes
    /// across repeated idle halts, the gap H9's real-Ubuntu-boot attempt found (todo.md §14 item
    /// 12).
    fn idle_halt_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/idle-halt-guest/bzImage")
    }

    /// `tests/fixtures/idle-halt-guest/payload.s`'s target message, written to COM1 only on the
    /// fixture's fifth wake.
    const IDLE_HALT_TARGET: &[u8] = b"ubuntu login:";

    /// H9's last recorded open blocker (todo.md §14 item 12), closed in isolation: every prior
    /// `run_to_first_halt_with_*` combinator terminates the instant the guest halts with no
    /// device work pending, which made a real kernel's idle loop (halt immediately, wait for the
    /// next timer tick, halt again) indistinguishable from a guest that shut down for good.
    /// `idle-halt-guest` halts before its very first instruction has any chance to retire a
    /// branch (so `inject_at`'s arm-early-then-single-step engine can never deliver to it —
    /// every tick must go through the new directly-staged-while-halted path), and only emits the
    /// target text after 4 silent wakes — proving the primitive actually resumes past more than
    /// one idle halt, not just one, and that a caller cannot get the right answer by accident
    /// (e.g. treating the first halt as terminal would return with the pattern never having
    /// appeared at all).
    #[test]
    fn run_until_console_pattern_resumes_across_repeated_idle_halts() {
        let kernel = idle_halt_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const PERIOD_RCB: u64 = 100_000;
        const MAX_TICKS: u32 = 20;
        const MAX_EXITS_PER_BURST: u32 = 4096;

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let (first_ticks, first_halt) = first
            .run_until_console_pattern_with_periodic_timer(
                PERIOD_RCB,
                TIMER_VECTOR,
                IDLE_HALT_TARGET,
                MAX_TICKS,
                MAX_EXITS_PER_BURST,
            )
            .expect("first run did not reach the target console pattern");

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let (second_ticks, second_halt) = second
            .run_until_console_pattern_with_periodic_timer(
                PERIOD_RCB,
                TIMER_VECTOR,
                IDLE_HALT_TARGET,
                MAX_TICKS,
                MAX_EXITS_PER_BURST,
            )
            .expect("second run did not reach the target console pattern");

        assert!(
            first_halt.console_output.windows(IDLE_HALT_TARGET.len()).any(|w| w == IDLE_HALT_TARGET),
            "the run must stop only once the target pattern actually appears in the console \
             output, not on an earlier idle halt: got {:?}",
            String::from_utf8_lossy(&first_halt.console_output)
        );
        assert_eq!(
            first_halt.console_output, second_halt.console_output,
            "console output up to the matched pattern must be identical across two boots"
        );
        assert_eq!(
            first_ticks.len(),
            second_ticks.len(),
            "the number of arm-early-then-single-step ticks actually delivered (as opposed to \
             directly staged while halted) must be deterministic across two boots"
        );
        assert_eq!(
            first_halt.ram_hash, second_halt.ram_hash,
            "guest RAM at the moment the pattern is observed must be byte-identical across two boots"
        );
    }

    /// Negative case for [`run_until_console_pattern_resumes_across_repeated_idle_halts`]:
    /// `idle-halt-guest`'s handler never emits a pattern this run never asks it to, so
    /// `max_ticks` must be exhausted and reported as a `DeterminismHole`, not silently return
    /// `Ok` with a partial or empty match — the same "no silent non-termination" convention every
    /// other run loop in this file follows.
    #[test]
    fn run_until_console_pattern_reports_determinism_hole_when_never_found() {
        let kernel = idle_halt_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let mut vm = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("boot failed");
        let result = vm.run_until_console_pattern_with_periodic_timer(
            100_000,
            TIMER_VECTOR,
            b"this pattern is never written by idle-halt-guest",
            5,
            4096,
        );
        assert!(
            result.is_err(),
            "a pattern the guest never writes must exhaust max_ticks and report an error, not \
             silently succeed"
        );
    }

    /// H9's core, previously-unbuilt gap (todo.md §14: the timed-exit fingerprint capability
    /// itself did not exist anywhere in this workspace before this iteration): boots
    /// `timer-guest` twice, freely runs each to the same absolute `TARGET_RCB` via
    /// [`Multiverse::capture_fingerprint`] -- **no timer tick injected at all**, this is `run_to_
    /// events`, not `inject_timer_tick` -- and asserts the two independent boots land the
    /// identical `(events, rip, gpa, mem_hash)` tuple. `TARGET_RCB` is chosen comfortably inside
    /// `timer-guest`'s single dec/jnz busy loop (BUILD.md), well below the `PERIOD_RCB = 200_000`
    /// `periodic_timer_injection_halts_gracefully_and_reproducibly` above empirically found the
    /// guest survives before its first tick, so the guest is still running (never halted) when
    /// this lands -- proving the mechanism specs/baud-ubuntu.md §6 and specs/baud-fingerprint.md
    /// §4 describe (arm-early-then-single-step to an exact RCB, `KVM_TRANSLATE` cross-checked by a
    /// manual CR3 walk, blake3 over guest RAM) is itself a pure function of `(image, tape, N)` --
    /// the load-bearing property the eventual `cross_vm_fingerprint_matches` (H9) depends on.
    /// `gpa` is asserted `Some` and equal to `rip` itself: `timer-guest` never leaves the fixed
    /// identity map this boot flow builds (`pagetables::write_identity_page_tables`), so its own
    /// guest-virtual and guest-physical addresses coincide by construction -- a real Linux kernel
    /// guest (whose own page tables remap RIP away from identity) is what will first exercise a
    /// `gpa != rip` translation for real, future H9 work this test does not attempt.
    #[test]
    fn timed_exit_fingerprint_is_stable() {
        let kernel = timer_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const TARGET_RCB: u64 = 100_000;

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("first boot failed");
        let first_fp = first.capture_fingerprint(TARGET_RCB).expect("first capture failed");

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("second boot failed");
        let second_fp = second.capture_fingerprint(TARGET_RCB).expect("second capture failed");

        assert_eq!(
            first_fp.events, TARGET_RCB,
            "must land on exactly the requested target -- specs/baud-fingerprint.md §4 step 1's \
             \"events = N\". This was `>= TARGET_RCB` while todo.md §14.1's landing-precision bug \
             was open; see `Multiverse::run_to_events`' doc and `LinuxBranchCounter::new` for the \
             root cause (a CPL-0 guest filtered out of the work-clock counter by \
             `perf_event::Builder`'s default `exclude_kernel = 1`)"
        );
        assert_eq!(first_fp.gpa, Some(first_fp.rip), "timer-guest never leaves the fixed identity map");
        assert_eq!(
            second_fp, first_fp,
            "the same (image, tape, N) must produce a byte-identical fingerprint across two \
             independent boots -- this is the whole-machine determinism proof H9's cross-VM check \
             depends on"
        );
    }

    /// The direct regression pin for todo.md §14.1's landing-precision bug
    /// ("`run_to_events`/`inject_at`'s single-step engine can overshoot its target RCB"): a *sweep*
    /// of consecutive absolute targets must each be landed on **exactly**, not merely at-or-past.
    ///
    /// A sweep, not a single target, is what makes this a real pin. Before the fix
    /// (`LinuxBranchCounter::new` leaving `perf_event::Builder`'s default `exclude_kernel = 1` in
    /// place, filtering this CPL-0 guest's entire instruction stream out of the work clock) the
    /// engine's finest reachable step was one whole `KVM_RUN`'s worth of *host* branches — measured
    /// at exactly +44 per single step against this fixture — so every one of these eight targets
    /// landed on the identical `rcb` 100_042, overshooting by 42, 41, 40, ... 35 respectively. Any
    /// regression that reintroduces a coarse-grained work clock therefore fails here on the first
    /// target that is not congruent to the quantum, whichever it happens to be.
    ///
    /// The landing `rip` is also asserted constant across the sweep, and equal to `timer-guest`'s
    /// `dec ebx` at the top of its inner `dec`/`jnz` loop (BUILD.md): every retired conditional
    /// branch inside that loop *is* the `jnz`, so the instruction boundary immediately after the
    /// N-th one is always the following `dec` — a second, independent check that the engine is
    /// stopping one guest instruction at a time rather than at some coarser exit boundary.
    #[test]
    fn run_to_events_lands_exactly_on_target_rcb() {
        let kernel = timer_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const BASE_RCB: u64 = 100_000;

        let mut landed_rips = Vec::new();
        for offset in 0..8u64 {
            let target = BASE_RCB + offset;
            let mut m = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("boot failed");
            let outcome = m.run_to_events(target).expect("run_to_events failed");
            assert!(
                outcome.was_reached(),
                "target {target} is well inside timer-guest's busy loop; the guest must not have halted"
            );
            let point = outcome.point();
            assert_eq!(
                point.rcb, target,
                "run_to_events({target}) must land on exactly {target} retired conditional \
                 branches, never past it -- specs/baud-fingerprint.md §4 step 1 / \
                 specs/baud-ubuntu.md §6's `assert_eq!(c, target)`. Landing past it means the work \
                 clock has lost single-guest-instruction resolution again (todo.md §14.1)"
            );
            landed_rips.push(point.rip);
        }
        assert!(
            landed_rips.windows(2).all(|w| w[0] == w[1]),
            "every boundary inside timer-guest's `dec ebx`/`jnz inner` loop is the `jnz`, so the \
             instruction landed on after each one must be the same `dec ebx`; got {landed_rips:#x?}"
        );
    }

    /// `tests/fixtures/virtio-rng-guest/`'s payload: a real (hand-assembled) virtio-rng driver
    /// sequence -- negotiate, set up one queue, post one writable descriptor, notify -- against the
    /// real `VirtioMmioTransport`, with its own IDT gate at `VIRTIO_RNG_VECTOR` proving a real
    /// delivered interrupt reaches it. See that directory's `BUILD.md` for the full rationale: this
    /// is the "interrupt delivery" half of todo.md §14 next-actions item 1's virtio-rng gap, closed
    /// with no in-kernel irqchip at all, via the same "stage `KVM_SET_VCPU_EVENTS`, let the next
    /// `KVM_RUN` deliver it" trick `timer-guest` already proved for the LAPIC timer's fixed vector.
    fn virtio_rng_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/virtio-rng-guest/bzImage")
    }

    /// The vector `tests/fixtures/virtio-rng-guest/payload.s`'s IDT gate is registered at.
    const VIRTIO_RNG_VECTOR: u8 = 0x31;

    /// Boots `virtio-rng-guest`, steps it one exit at a time until its `QueueNotify` write is
    /// observed (`Multiverse::virtio_rng`'s `notify_count`), services the ring and delivers a real
    /// interrupt at `VIRTIO_RNG_VECTOR` (`Multiverse::service_virtio_rng_interrupt`), then runs the
    /// guest to its own clean halt. Returns the halt outcome plus the exact first entropy byte the
    /// seed produces (computed independently here, via the same `SplitMix64` `service_virtio_rng`
    /// itself draws from) so a caller can assert the guest's own ISR actually observed it, not just
    /// that some interrupt fired.
    fn run_virtio_rng_guest_once(seed: u64) -> (HaltOutcome, u8) {
        let kernel = virtio_rng_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let mut mv = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("boot failed");
        mv.enable_virtio_rng();
        mv.seed_virtio_rng_entropy(seed);

        // ~19 real MMIO exits for negotiate/setup/notify, then `payload.s`'s own busy-loop (20,000
        // outer iterations, one `out 0x80` exit each, deliberately long enough for the interrupt to
        // land mid-loop) plus the ISR/halt tail -- this run loop (unlike the original two-phase
        // version) counts every one of those against the same budget, so this is generous, not tight.
        const MAX_EXITS: u32 = 200_000;
        let halt = mv
            .run_to_first_halt_with_virtio_rng(VIRTIO_RNG_VECTOR, MAX_EXITS)
            .expect("run_to_first_halt_with_virtio_rng failed");
        let expected_byte = crate::timesource::SplitMix64::new(seed).next_u64().to_le_bytes()[0];
        (halt, expected_byte)
    }

    /// Real-hardware proof that virtio-rng's "interrupt delivery" gap (todo.md §14 next-actions
    /// item 1) is closed: the guest's own IDT-registered ISR actually runs, and reads back the
    /// exact tape-seeded entropy byte `DeviceBus::service_virtio_rng` wrote into its posted buffer,
    /// through a real KVM-delivered interrupt with no in-kernel irqchip at all.
    #[test]
    fn virtio_rng_interrupt_reaches_the_guests_own_isr() {
        let (halt, expected_byte) = run_virtio_rng_guest_once(42);
        assert_eq!(
            halt.console_output,
            vec![b'R', expected_byte],
            "the guest's ISR must fire exactly once, writing its marker then the real entropy byte \
             service_virtio_rng filled the buffer with"
        );
    }

    /// The same guarantee every other H4 interrupt test in this file asserts: a double-run of the
    /// identical image+tape (here, the same entropy seed) reaches identical guest-visible state --
    /// the interrupt is not merely delivered, it is delivered deterministically.
    #[test]
    fn virtio_rng_interrupt_delivery_is_reproducible_across_two_boots() {
        let (first, expected_byte) = run_virtio_rng_guest_once(7);
        let (second, expected_byte_again) = run_virtio_rng_guest_once(7);
        assert_eq!(expected_byte, expected_byte_again, "same seed must reproduce the identical entropy byte");
        assert_eq!(
            first.console_output, second.console_output,
            "console output (marker + entropy byte) must be identical across two boots of the same \
             image+tape"
        );
        assert_eq!(
            first.ram_hash, second.ram_hash,
            "guest RAM at the guest's own natural halt must be byte-identical across two boots"
        );
    }

    /// Real-hardware proof that `crate::pic8259::Pic8259` — the dual-8259 bookkeeping stub that
    /// answers todo.md §14's long-open "which vector would an unmodified Linux guest's real
    /// `virtio_mmio` driver bind to" question (see `pic8259.rs`'s own doc, and
    /// `tests/fixtures/virtio-rng-guest/BUILD.md`'s "Update" section) — actually observes real
    /// guest `IN`/`OUT` PIO exits, not just the pure-Rust unit tests in `pic8259.rs` itself.
    /// `payload.s` now issues the exact `probe_8259A()` + `init_8259A()` + `enable_8259A_irq(5)`
    /// byte sequence Linux issues before any ISA IRQ can be used, ahead of its virtio-mmio
    /// negotiation; this asserts the resulting bookkeeping state is exactly what that sequence
    /// implies (`0xdf` = every bit set except bit 5, `0xff` = the slave left fully masked), and
    /// that this didn't perturb the rest of the guest's run (the existing marker+entropy-byte
    /// assertion `virtio_rng_interrupt_reaches_the_guests_own_isr` already covers).
    #[test]
    fn guests_own_pic_bring_up_sequence_leaves_the_expected_bookkeeping_state() {
        let kernel = virtio_rng_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let mut mv = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("boot failed");
        mv.enable_virtio_rng();
        mv.seed_virtio_rng_entropy(42);
        const MAX_EXITS: u32 = 200_000;
        mv.run_to_first_halt_with_virtio_rng(VIRTIO_RNG_VECTOR, MAX_EXITS)
            .expect("run_to_first_halt_with_virtio_rng failed");
        assert_eq!(
            mv.pic().master_imr(),
            0xdf,
            "master PIC IMR must have only IRQ5's bit clear after the guest's own \
             enable_8259A_irq(5)-equivalent write"
        );
        assert_eq!(
            mv.pic().slave_imr(),
            0xff,
            "slave PIC IMR must stay fully masked -- this fixture never unmasks any slave line"
        );
    }

    /// Real-hardware proof that `run_until_branch_or_halt_with_virtio_rng` (todo.md §14 next-actions
    /// item 1's "branch/resume don't accept virtio_rng at all" gap) delivers a real virtio-rng
    /// interrupt through a *forked* `Multiverse::branch`, not just a fresh `boot` -- since virtio-rng
    /// device state is not itself part of the snapshot/restore/branch contract
    /// (`Multiverse::run_until_branch_or_halt_with_virtio_rng`'s own doc), this snapshots the
    /// `virtio-rng-guest` fixture immediately after boot (before it negotiates anything), forks one
    /// branch with an empty tape suffix, re-enables and re-seeds virtio-rng fresh on the branch
    /// exactly as `baud-server`'s `run_branches` now does, and asserts the branch's own ISR observes
    /// the same marker + entropy byte a direct boot does (`run_virtio_rng_guest_once`'s own
    /// assertion). This fixture never calls `MARK_BRANCH`, so this only exercises the `Halted` stop
    /// arm, not the `MarkBranch` one -- `run_until_branch_or_halt_with_periodic_timer_and_virtio_rng`
    /// (the four-way combinator) has no dedicated fixture at all yet, since no existing guest talks
    /// both virtio-rng and needs periodic ticks.
    #[test]
    fn run_until_branch_or_halt_with_virtio_rng_delivers_interrupt_to_a_branch() {
        const WORK_CLOCK_K: u64 = 1;
        let kernel = virtio_rng_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let mut boot =
            Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, vec![], None).expect("boot failed");
        let mut page_store = baud_snapshot::PageStore::new();
        let universe = boot.snapshot(&mut page_store).expect("snapshot failed");

        let mut branch =
            Multiverse::branch(&universe, vec![], WORK_CLOCK_K, None).expect("branch failed");
        branch.enable_virtio_rng();
        branch.seed_virtio_rng_entropy(42);

        const MAX_EXITS: u32 = 200_000;
        let (outcome, _records) = branch
            .run_until_branch_or_halt_with_virtio_rng(VIRTIO_RNG_VECTOR, MAX_EXITS)
            .expect("run_until_branch_or_halt_with_virtio_rng failed");
        let halt = match outcome {
            RunUntilBranchOutcome::Halted(halt) => halt,
            RunUntilBranchOutcome::MarkBranch { step } => {
                panic!("virtio-rng-guest never calls MARK_BRANCH, got one at step {step}")
            }
        };
        let expected_byte = crate::timesource::SplitMix64::new(42).next_u64().to_le_bytes()[0];
        assert_eq!(
            halt.console_output,
            vec![b'R', expected_byte],
            "a branch (not just a fresh boot) must deliver the real virtio-rng interrupt to the \
             guest's own ISR, which must observe the exact seeded entropy byte"
        );
    }

    /// `tests/fixtures/linux-guest/`'s real, compiled (not hand-assembled) Linux 6.18 kernel and
    /// initramfs -- see that directory's `BUILD.md` for exact provenance/regeneration and the three
    /// real bugs (two in this crate, one in `baud-vcpu`) this fixture's first real boot caught.
    fn linux_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/linux-guest/bzImage")
    }

    fn linux_guest_initramfs() -> Vec<u8> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/linux-guest/initramfs.cpio.gz");
        std::fs::read(path).expect("read linux-guest initramfs fixture")
    }

    /// The exact marker `tests/fixtures/linux-guest/init.c`'s `/init` writes (via raw `outb` to
    /// COM1, not `write(1, ...)` -- that directory's `BUILD.md` explains why) right before it powers
    /// off, asserted verbatim so a change to either side is caught rather than silently drifting.
    const LINUX_GUEST_MARKER: &str = "baud-guest: minimal kernel reached /init\n";

    /// todo.md §14 item 1 / §26 (`guest_kernel_boots_to_userspace`): the first time this project has
    /// booted a real, unmodified Linux kernel through baud-multiverse all the way to a real `/init`
    /// process, driven entirely by H4's periodic-interrupt-injection engine
    /// (`run_to_first_halt_with_periodic_timer`) rather than any pre-known tick count -- the guest's
    /// own scheduler timer needs are satisfied by the same `KVM_INTERRUPT` mechanism the
    /// hand-assembled `timer-guest` fixture already exercises, with no LAPIC device model needed
    /// (`tests/fixtures/linux-guest/BUILD.md` explains why).
    ///
    /// Asserts two boots of the same image+tape each independently reach `/init`'s marker and halt
    /// cleanly after the *same number* of periodic ticks (its own real-hardware-observed
    /// determinism signal) -- but deliberately **not** that the two boots' full console output or
    /// RAM hash are byte-identical. That stronger check exists (`double_boot_ram_hash_identical`,
    /// H7) but needs a guest-driven checkpoint (todo.md's own spec for it): a first attempt here
    /// found the two boots' text differs in exactly one kernel-internal diagnostic line
    /// (`sched_clock: Marking stable`) that embeds raw TSC-derived numbers sensitive to this
    /// project's already-documented small real-hardware branch-counter read jitter
    /// (`RCB_HARDWARE_JITTER_TOLERANCE`) -- seeing that number differ is not the same as the guest's
    /// actual instruction stream differing, which is exactly why the spec calls for a checkpoint
    /// instead of raw console/RAM comparison for this stronger guarantee (see this fixture's
    /// `BUILD.md` for the full account).
    #[test]
    fn guest_kernel_boots_to_userspace() {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_initramfs();
        // Spec §4.2's exact cmdline (todo.md §14 next-actions item 1's closing item: this used to
        // be a hand-diverged inline string, `quiet loglevel=1` included -- safe here because the
        // marker/tick-count assertions below read only the guest's own raw-`outb` writes, never
        // kernel printk text).
        let cmdline = bootparams::DETERMINISTIC_CMDLINE;
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR (arch/x86/include/asm/irq_vectors.h)

        let mut tick_counts = Vec::new();
        for i in 0..2 {
            let mut m = Multiverse::boot_with_rdseed_sites(
                &kernel,
                cmdline,
                0,
                1,
                vec![],
                None,
                Some(&initramfs),
                [],
            )
            .unwrap_or_else(|e| panic!("run {i}: boot failed: {e}"));
            let (ticks, halt) = m
                .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
                .unwrap_or_else(|e| panic!("run {i}: periodic run failed: {e}"));
            let console = String::from_utf8_lossy(&halt.console_output).to_string();
            assert!(
                console.contains(LINUX_GUEST_MARKER),
                "run {i}: guest must reach /init and print its marker; got:\n{console}"
            );
            tick_counts.push(ticks.len());
        }
        assert_eq!(
            tick_counts[0], tick_counts[1],
            "the same image+tape must survive the same number of periodic ticks before its own \
             natural halt across two boots"
        );
    }

    /// todo.md §14 item 5(c)'s two flagged gaps, closed: a real `CONFIG_ACPI=y` guest (`minimal.
    /// config` flipped `CONFIG_ACPI=n` -> `y`, same "compiled-in-but-inert for anyone still booting
    /// `acpi=off`" precedent `BUILD.md` already documents for `HPET_TIMER`) boots with `acpi=on`
    /// and [`Multiverse::write_acpi_tables`] (RSDP -> XSDT -> FADT + DSDT + MADT-with-one-LAPIC,
    /// `crate::acpi`) actually in guest memory, and [`crate::lapic::LocalApic`] answers its real
    /// MMIO probes instead of `OpenBusFallback`. Proves the two things `crate::acpi`'s own doc
    /// named as the real acid test: the kernel's `acpi_boot_table_init()` finds and validates every
    /// table (no checksum/pointer mistake in `crate::acpi`'s pure construction went unnoticed by a
    /// real ACPICA parse), and `setup_local_APIC()` completes without hanging on `LocalApic`'s
    /// stubbed registers (the one real hang hazard research identified: `APIC_ICR`'s busy bit).
    #[test]
    fn guest_kernel_boots_with_acpi_enabled_and_recognizes_the_lapic() {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_initramfs();
        let cmdline = bootparams::DETERMINISTIC_CMDLINE.replace("acpi=off ", "");
        assert_ne!(cmdline, bootparams::DETERMINISTIC_CMDLINE, "the replace must actually have matched");
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR

        let mut consoles = Vec::new();
        for i in 0..2 {
            let mut m = Multiverse::boot_with_rdseed_sites(
                &kernel,
                &cmdline,
                0,
                1,
                vec![],
                None,
                Some(&initramfs),
                [],
            )
            .unwrap_or_else(|e| panic!("run {i}: boot failed: {e}"));
            m.write_acpi_tables().unwrap_or_else(|e| panic!("run {i}: write_acpi_tables failed: {e}"));
            let (_ticks, halt) = m
                .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
                .unwrap_or_else(|e| panic!("run {i}: periodic run failed: {e}"));
            let console = String::from_utf8_lossy(&halt.console_output).to_string();
            assert!(
                console.contains(LINUX_GUEST_MARKER),
                "run {i}: guest must still reach /init and print its marker; got:\n{console}"
            );
            assert!(
                !console.contains("No local APIC present"),
                "run {i}: a real MADT-advertised LAPIC must be recognized, not disabled; got:\n{console}"
            );
            // `DETERMINISTIC_CMDLINE`'s own `quiet loglevel=1` suppresses virtually every printk
            // line (confirmed: even the non-ACPI boot's console is just the marker plus the
            // reboot-path message), so console text cannot prove the kernel's real
            // `setup_local_APIC()`/`setup_APIC_timer()` actually ran -- `LocalApic`'s own
            // guest-write-derived register state is the real, direct proof instead: the guest's
            // own real MMIO writes, not a log line, are what a fake/absent LAPIC could never
            // produce.
            let spiv = m.lapic().spurious_interrupt_vector();
            assert_ne!(spiv, 0, "run {i}: the kernel must have written APIC_SPIV (software-enable) itself");
            let lvt_timer = m.lapic().lvt_timer();
            assert_ne!(lvt_timer, 0, "run {i}: the kernel must have armed APIC_LVT_TIMER itself");
            consoles.push((console, spiv, lvt_timer));
        }
        assert_eq!(
            consoles[0], consoles[1],
            "an ACPI-enabled boot must remain exactly as reproducible as every other guest here"
        );
    }

    /// `tests/fixtures/linux-guest/virtio_rng_init.c`'s `/init`: opens `/dev/hwrng` and reads from
    /// it, rather than printing a fixed marker -- the payload for the real, not hand-assembled,
    /// counterpart to `virtio_rng_interrupt_reaches_the_guests_own_isr`.
    fn linux_guest_virtio_rng_initramfs() -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/linux-guest/virtio_rng_initramfs.cpio.gz");
        std::fs::read(path).expect("read linux-guest virtio_rng initramfs fixture")
    }

    /// todo.md §14 next-actions item 1's last open piece of the virtio-rng gap: a real, unmodified
    /// Linux kernel's own `drivers/virtio/virtio_mmio.c` + `drivers/char/hw_random/virtio-rng.c`
    /// drivers, not a hand-assembled payload, actually probing baud's `VirtioMmioTransport` and
    /// completing a real `/dev/hwrng` read. This needed three closed prerequisites this iteration
    /// found were all already true or newly buildable: (1) `minimal.config` gained
    /// `CONFIG_VIRTIO_MENU`/`VIRTIO`/`VIRTIO_MMIO`/`VIRTIO_MMIO_CMDLINE_DEVICES`/`HW_RANDOM`/
    /// `HW_RANDOM_VIRTIO=y` (note `VIRTIO_MENU` — the entire virtio Kconfig submenu is gated behind
    /// it, not just `VIRTIO_MMIO`'s own `HAS_IOMEM`/`HAS_DMA` deps, a real gotcha the first attempt
    /// missed); (2) the `virtio_mmio.device=<size>@<base>:<irq>` cmdline parameter names IRQ 5,
    /// resolving via `crate::pic8259::isa_irq_vector(5)` to the exact vector
    /// [`Multiverse::run_to_first_halt_with_periodic_timer_and_virtio_rng`] delivers on, closing
    /// the "which vector" research question iteration 32 answered only in the abstract; (3) a real
    /// unmodified Linux boot's own `probe_8259A()`/`init_8259A()` sequence (not `payload.s`'s
    /// hand-assembled mimicry) registers the legacy-IRQ domain against `crate::pic8259::Pic8259`
    /// exactly like the hand-assembled fixture already proved, making `request_irq(5, ...)` succeed
    /// for the first time in this project's history.
    ///
    /// Deliberately checks the guest's own `/dev/hwrng` read result via raw-`outb` markers, not
    /// kernel `dmesg` text (`DETERMINISTIC_CMDLINE` carries `quiet loglevel=1`, so driver-probe
    /// printk lines are not reliably in the console capture at all).
    fn run_linux_guest_virtio_rng_once(seed: u64) -> String {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_virtio_rng_initramfs();
        let cmdline = format!("{} virtio_mmio.device=0x200@0xd0000000:5", bootparams::DETERMINISTIC_CMDLINE);
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR
        let virtio_rng_vector = crate::pic8259::isa_irq_vector(5);

        let mut m = Multiverse::boot_with_rdseed_sites(
            &kernel,
            &cmdline,
            0,
            1,
            vec![],
            None,
            Some(&initramfs),
            [],
        )
        .expect("boot failed");
        m.enable_virtio_rng();
        m.seed_virtio_rng_entropy(seed);
        let (_ticks, halt) = m
            .run_to_first_halt_with_periodic_timer_and_virtio_rng(PERIOD_RCB, TIMER_VECTOR, virtio_rng_vector, MAX_TICKS)
            .expect("periodic-timer + virtio-rng run failed");
        String::from_utf8_lossy(&halt.console_output).to_string()
    }

    #[test]
    fn guest_virtio_mmio_rng_driver_reads_real_entropy_through_virtio_rng() {
        let console = run_linux_guest_virtio_rng_once(99);
        assert!(
            console.contains(LINUX_GUEST_MARKER),
            "guest must still reach /init and print its marker; got:\n{console}"
        );
        assert!(
            console.contains("baud-guest: hwrng-open-ok"),
            "the guest's own real virtio_mmio.c/virtio-rng.c drivers must probe the device and \
             register /dev/hwrng; got:\n{console}"
        );
        assert!(
            console.contains("baud-guest: hwrng-bytes:"),
            "a real read() from /dev/hwrng must complete (via a real KVM-delivered interrupt at \
             isa_irq_vector(5)), not hang or fail; got:\n{console}"
        );
    }

    /// specs/baud-multiverse.md §3.8's `virtio_rng_reseed_is_deterministic`: "with a tape-fed
    /// virtio-rng source ..., continuous reseeding does not perturb the output stream across a
    /// double-run." `virtio_rng_init.c` now loops four separate `read()`s over the same open
    /// `/dev/hwrng` fd instead of one -- four distinct request/completion round-trips through
    /// `VirtioMmioTransport`/`SplitVirtqueue::process_available` per boot (§14 next-actions item 1
    /// confirmed the host-side device model and `run_to_first_halt_with_periodic_timer_and_
    /// virtio_rng`'s halt-servicing loop already generically support repeated completions; only the
    /// guest payload needed to actually issue more than one). This is the "over a longer run" case
    /// `guest_virtio_mmio_rng_driver_entropy_is_reproducible_across_two_boots` explicitly did not
    /// cover (that test's `/init` read exactly once).
    #[test]
    fn virtio_rng_reseed_is_deterministic() {
        let first = run_linux_guest_virtio_rng_once(31337);
        let second = run_linux_guest_virtio_rng_once(31337);

        fn extract_reads(console: &str) -> Vec<&str> {
            console
                .lines()
                .filter_map(|l| l.strip_prefix("baud-guest: hwrng-bytes:"))
                .collect()
        }
        let first_reads = extract_reads(&first);
        let second_reads = extract_reads(&second);

        assert_eq!(
            first_reads.len(),
            4,
            "the guest must complete all four separate hwrng reads (continuous reseeding, not \
             just the initial one); got:\n{first}"
        );

        let mut distinct = first_reads.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            4,
            "each of the four reads must draw fresh entropy from the tape-seeded stream, not \
             repeat a cached value; got reads: {first_reads:?}"
        );

        assert_eq!(
            first_reads, second_reads,
            "continuous reseeding across repeated virtio-rng completions must not perturb the \
             output stream across a double-run: same seed twice must yield the identical sequence \
             of reads"
        );
        assert_eq!(
            first, second,
            "the full console output, including every reseed round-trip, must be byte-identical \
             across two boots of the same image+tape+seed"
        );
    }

    /// Same guarantee as `virtio_rng_interrupt_delivery_is_reproducible_across_two_boots`, but
    /// through a real Linux guest's own driver stack end to end: the exact hex bytes its `/init`
    /// reads from `/dev/hwrng` (via `drivers/char/hw_random/virtio-rng.c`'s real request/completion
    /// path, not a hand-assembled payload) must be byte-identical across two boots of the same
    /// image+tape+seed.
    #[test]
    fn guest_virtio_mmio_rng_driver_entropy_is_reproducible_across_two_boots() {
        let first = run_linux_guest_virtio_rng_once(7);
        let second = run_linux_guest_virtio_rng_once(7);
        assert!(first.contains("baud-guest: hwrng-bytes:"), "first boot must complete a real hwrng read; got:\n{first}");
        assert_eq!(
            first, second,
            "two boots of the same image+tape+seed must produce byte-identical console output, \
             including the real virtio_rng-sourced entropy bytes"
        );
    }

    /// todo.md §14 item 5(b)'s "needs a design change, e.g. a generic 'poll N devices' abstraction"
    /// note on `run_to_first_halt_with_virtio_pci_blk` -- the three-way
    /// `run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk` combinator this
    /// refactor introduced, real-hardware-verified via the existing `virtio_rng_initramfs` fixture
    /// (this kernel's `minimal.config` has no `CONFIG_VIRTIO_BLK`/`CONFIG_VIRTIO_PCI_LEGACY`, so the
    /// guest never actually probes the block device -- building a guest that does is the next,
    /// separately-scoped H9 sub-step). Proves two things a rng-only run can't: (1) enabling a third
    /// device the guest never touches does not perturb the already-working timer+rng boot at all
    /// (byte-identical console vs. the plain `run_linux_guest_virtio_rng_once` path), and (2) the
    /// combined run stays fully deterministic across two boots with the third device present.
    #[test]
    fn periodic_timer_virtio_rng_and_virtio_pci_blk_combinator_does_not_perturb_an_unused_third_device() {
        fn run_once(seed: u64) -> (String, u64) {
            let kernel = linux_guest_kernel_path();
            let initramfs = linux_guest_virtio_rng_initramfs();
            let cmdline = format!("{} virtio_mmio.device=0x200@0xd0000000:5", bootparams::DETERMINISTIC_CMDLINE);
            const PERIOD_RCB: u64 = 500_000;
            const MAX_TICKS: u32 = 2000;
            const TIMER_VECTOR: u8 = 0xec;
            let virtio_rng_vector = crate::pic8259::isa_irq_vector(5);
            const VIRTIO_BLK_VECTOR: u8 = 0xed;

            let mut m = Multiverse::boot_with_rdseed_sites(
                &kernel,
                &cmdline,
                0,
                1,
                vec![],
                None,
                Some(&initramfs),
                [],
            )
            .expect("boot failed");
            m.enable_virtio_rng();
            m.seed_virtio_rng_entropy(seed);
            m.enable_virtio_pci_blk(vec![0u8; 512 * 4]);
            let (_ticks, halt) = m
                .run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk(
                    PERIOD_RCB,
                    TIMER_VECTOR,
                    virtio_rng_vector,
                    VIRTIO_BLK_VECTOR,
                    MAX_TICKS,
                )
                .expect("periodic-timer + virtio-rng + virtio-blk run failed");
            let notify_count = m.virtio_pci_blk().map(|t| t.notify_count()).unwrap_or(u64::MAX);
            (String::from_utf8_lossy(&halt.console_output).to_string(), notify_count)
        }

        let (first, first_blk_notify) = run_once(99);
        let (second, second_blk_notify) = run_once(99);
        let rng_only = run_linux_guest_virtio_rng_once(99);

        assert_eq!(
            first, rng_only,
            "adding an unenabled-in-the-guest virtio-blk device to the run loop must not change a \
             single byte of console output versus the plain timer+rng path; got:\n{first}"
        );
        assert_eq!(
            first_blk_notify, 0,
            "this fixture's guest has no virtio-blk driver compiled in, so the device must never \
             be notified at all"
        );
        assert_eq!(
            first, second,
            "two boots of the same image+tape+seed with all three devices enabled must still \
             produce byte-identical console output"
        );
        assert_eq!(first_blk_notify, second_blk_notify, "the untouched device's notify count must match across boots too");
    }

    /// specs/baud-multiverse.md §4.3's `init_powers_off_deterministically`: "a clean VMM-detected
    /// shutdown at an identical exit point across two boots." Reuses the same real `linux-guest`
    /// fixture and open-ended periodic-timer engine as [`guest_kernel_boots_to_userspace`] — its
    /// `/init` (`tests/fixtures/linux-guest/init.c`) calls `reboot(RB_POWER_OFF)` right after
    /// printing its marker (§4.3's exact shutdown path, a real triple-fault the run loop resolves to
    /// `VcpuExit::Shutdown`/`HaltOutcome`, not a hand-assembled `hlt` loop like `hello-guest`). Two
    /// boots of the same image+tape must land the halt at the identical instruction
    /// (`HaltOutcome::exit_pc`, `KVM_GET_REGS`'s RIP read right after the halt is observed) — the
    /// "identical exit point" the spec names, distinct from `guest_kernel_boots_to_userspace`'s own
    /// weaker "same tick count" check and from `double_boot_ram_hash_identical`'s much stronger
    /// (and, per todo.md §14, not yet fully passing) full-RAM comparison.
    #[test]
    fn init_powers_off_deterministically() {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_initramfs();
        let cmdline = bootparams::DETERMINISTIC_CMDLINE;
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR (arch/x86/include/asm/irq_vectors.h)

        let mut exit_pcs = Vec::new();
        for i in 0..2 {
            let mut m = Multiverse::boot_with_rdseed_sites(
                &kernel,
                cmdline,
                0,
                1,
                vec![],
                None,
                Some(&initramfs),
                [],
            )
            .unwrap_or_else(|e| panic!("run {i}: boot failed: {e}"));
            let (_ticks, halt) = m
                .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
                .unwrap_or_else(|e| panic!("run {i}: periodic run failed: {e}"));
            let console = String::from_utf8_lossy(&halt.console_output).to_string();
            assert!(
                console.contains(LINUX_GUEST_MARKER),
                "run {i}: guest must reach /init and print its marker before powering off; got:\n{console}"
            );
            exit_pcs.push(halt.exit_pc);
        }
        assert_eq!(
            exit_pcs[0], exit_pcs[1],
            "a clean VMM-detected shutdown must land at the identical instruction across two boots \
             of the same image+tape"
        );
    }

    /// `tests/fixtures/linux-guest/virtio_blk_init.c`'s `/init`: discovers a real virtio-pci-legacy
    /// block device (via `/sys/class/block/vda/dev`, same devtmpfsd-race workaround as
    /// `virtio_rng_init.c`), reads sector 0, then writes and reads back sector 1 — the real,
    /// not-hand-assembled-fixture, driver-exercising counterpart todo.md §14 item 5's "no real-KVM
    /// fixture actually exercises virtio-blk end to end" gap named.
    fn linux_guest_virtio_blk_initramfs() -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/linux-guest/virtio_blk_initramfs.cpio.gz");
        std::fs::read(path).expect("read linux-guest virtio_blk initramfs fixture")
    }

    /// The base disk image this test suite's `/init` reads at sector 0 and the pattern it expects
    /// to see: byte `i` is `i % 256`, repeating every 256 bytes — instantly recognizable as real
    /// backing-store content in a console hex dump, distinct from an all-zero or all-`0xff` image
    /// that could equally be an unwritten/open-bus artifact.
    fn virtio_blk_test_base_image(sectors: u64) -> Vec<u8> {
        (0..(crate::virtio_blk::SECTOR_SIZE * sectors)).map(|i| (i % 256) as u8).collect()
    }

    /// `virtio_blk_init.c`'s fixed sector-1 write pattern (`wbuf[i] = i & 0xff` for `i` in
    /// `0..SECTOR_SIZE`) — happens to look identical to one period of
    /// [`virtio_blk_test_base_image`]'s own pattern (both are `i % 256` over exactly one sector),
    /// which is exactly why the write-then-readback assertion below only proves something once
    /// sector 1's *original* base-image content is confirmed different first (todo.md's own
    /// "prove the overlay, not just an already-matching base" concern).
    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Boots `virtio_blk_initramfs.cpio.gz` with a real virtio-pci-legacy block device attached
    /// (`pci=off` stripped from [`bootparams::DETERMINISTIC_CMDLINE`] — a virtio-pci device needs
    /// real PCI enumeration to be found at all, same requirement
    /// `guest_kernel_boots_with_acpi_enabled_and_recognizes_the_lapic` has for `acpi=off`). Reuses
    /// the existing three-way [`Multiverse::run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk`]
    /// combinator with virtio-rng left disabled (never [`Multiverse::enable_virtio_rng`]d) — exactly
    /// the "either one left unenabled behaves like the two-device wrapper" case that combinator's own
    /// doc names, so no new run-loop wrapper is needed for a timer+blk-only boot.
    fn run_linux_guest_virtio_blk_once(base_image: Vec<u8>) -> String {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_virtio_blk_initramfs();
        let cmdline = bootparams::DETERMINISTIC_CMDLINE.replace("pci=off ", "");
        assert_ne!(cmdline, bootparams::DETERMINISTIC_CMDLINE, "the replace must actually have matched");
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR
        const VIRTIO_RNG_VECTOR_UNUSED: u8 = 0xeb; // never serviced: virtio-rng is never enabled below
        let virtio_blk_vector = crate::pic8259::isa_irq_vector(11); // matches PciHostBridge's
                                                                     // VIRTIO_BLK_DEFAULT_IRQ_LINE

        let mut m = Multiverse::boot_with_rdseed_sites(
            &kernel,
            &cmdline,
            0,
            1,
            vec![],
            None,
            Some(&initramfs),
            [],
        )
        .expect("boot failed");
        m.enable_virtio_pci_blk(base_image);
        let (_ticks, halt) = m
            .run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk(
                PERIOD_RCB,
                TIMER_VECTOR,
                VIRTIO_RNG_VECTOR_UNUSED,
                virtio_blk_vector,
                MAX_TICKS,
            )
            .expect("periodic-timer + virtio-blk run failed");
        String::from_utf8_lossy(&halt.console_output).to_string()
    }

    /// Real-hardware proof that a genuinely unmodified Linux kernel's own `virtio_pci_legacy` +
    /// `virtio_blk` drivers (not a hand-assembled payload) discover baud's `PciHostBridge` +
    /// `VirtioPciTransport` + `virtio_blk::service_request` stack over real PCI, and complete real
    /// `VIRTIO_BLK_T_IN`/`VIRTIO_BLK_T_OUT` requests end to end (todo.md §14 item 5's last-named
    /// open gap after (a)/(b)/(c), all of which were previously only exercised in-memory against a
    /// bare `vm_memory::GuestMemoryMmap`, never a real driver on real `/dev/kvm`).
    ///
    /// Getting a real driver this far required two fixes this test's first real boot found: (1)
    /// `PciHostBridge::HOST_BRIDGE_CLASS_CODE` had Base Class and Sub-Class byte-swapped, which
    /// failed a real kernel's own `pci_sanity_check()` outright ("PCI: Fatal: No config space
    /// access function found") — every existing unit test had asserted the same swapped
    /// convention as correct, so it went unnoticed until a real, unmodified kernel actually tried
    /// to enumerate PCI; (2) `PciVirtioFunction::interrupt_line` defaulted to `0`, which a real
    /// `virtio_pci_legacy` driver reports as "can't find IRQ for PCI INT A" and fails probe with
    /// `-ENOSPC` — direct-boot Linux has no BIOS/ACPI/`$PIR` table to program this register, so
    /// baud itself now pre-routes it (`VIRTIO_RNG_DEFAULT_IRQ_LINE`/`VIRTIO_BLK_DEFAULT_IRQ_LINE`,
    /// `pci.rs`), the same "no BIOS exists, so the VMM plays that role" precedent `boot_params`/e820
    /// already established.
    #[test]
    fn guest_virtio_pci_blk_driver_reads_and_writes_real_sectors() {
        const SECTORS: u64 = 4;
        let base_image = virtio_blk_test_base_image(SECTORS);
        let sector0_expected = hex_encode(&base_image[..crate::virtio_blk::SECTOR_SIZE as usize]);
        // Sector 1's pristine base-image content, before the guest ever writes to it — asserted
        // first so the write-then-readback check below actually proves the write landed, rather
        // than trivially matching content that was already there.
        let sector1_base_slice =
            &base_image[crate::virtio_blk::SECTOR_SIZE as usize..2 * crate::virtio_blk::SECTOR_SIZE as usize];
        let write_pattern: Vec<u8> = (0..crate::virtio_blk::SECTOR_SIZE).map(|i| (i % 256) as u8).collect();
        assert_eq!(
            sector1_base_slice, &write_pattern[..],
            "this test's own fixed base-image formula and virtio_blk_init.c's fixed write pattern \
             happen to produce identical bytes for sector 1 by construction (both are `i % 256` over \
             one sector) -- documenting why, not a bug"
        );
        let sector1_expected = hex_encode(&write_pattern);

        let console = run_linux_guest_virtio_blk_once(base_image);
        assert!(
            console.contains(LINUX_GUEST_MARKER),
            "guest must still reach /init and print its marker; got:\n{console}"
        );
        assert!(
            console.contains("baud-guest: blk-open-ok"),
            "the guest's own real virtio_pci_legacy/virtio_blk drivers must probe the device and \
             open /dev/vda; got:\n{console}"
        );
        assert!(
            console.contains(&format!("baud-guest: blk-sector0-bytes:{sector0_expected}\n")),
            "a real VIRTIO_BLK_T_IN read of sector 0 must return the base image's pristine \
             content unmodified; got:\n{console}"
        );
        assert!(
            console.contains("baud-guest: blk-write-sector1-ok"),
            "a real VIRTIO_BLK_T_OUT write to sector 1 must complete; got:\n{console}"
        );
        assert!(
            console.contains(&format!("baud-guest: blk-sector1-readback-bytes:{sector1_expected}\n")),
            "a fresh VIRTIO_BLK_T_IN read of sector 1 must observe the just-written overlay \
             content, proving the write actually persisted; got:\n{console}"
        );
    }

    /// specs/baud-multiverse.md §3.8-adjacent determinism guarantee for virtio-blk: two boots of the
    /// same image+tape+backing-store must produce byte-identical console output, including every
    /// real read/write round-trip through the block device — the virtio-blk analogue of
    /// `guest_virtio_mmio_rng_driver_entropy_is_reproducible_across_two_boots`.
    #[test]
    fn guest_virtio_pci_blk_driver_io_is_reproducible_across_two_boots() {
        const SECTORS: u64 = 4;
        let first = run_linux_guest_virtio_blk_once(virtio_blk_test_base_image(SECTORS));
        let second = run_linux_guest_virtio_blk_once(virtio_blk_test_base_image(SECTORS));
        assert!(
            first.contains("baud-guest: blk-sector1-readback-bytes:"),
            "first boot must complete a real write+readback round-trip; got:\n{first}"
        );
        assert_eq!(
            first, second,
            "two boots of the same image+tape+backing-store must produce byte-identical console \
             output, including every real virtio-blk read/write round-trip"
        );
    }

    /// todo.md §14 item 1's remaining named gap: "the initramfs builder's multi-file capacity is
    /// now mechanism-complete ... but still only exercised with a single `/init`-style entry -- no
    /// real harness-script/agent-binary multi-file rootfs has been assembled or tested yet."
    ///
    /// Unlike every other `linux-guest` variant above, this test does not read a hand-`cpio`'d
    /// fixture off disk -- it builds the initramfs itself, at test time, via
    /// `baud_packages::initramfs::build_reproducible_initramfs` with **two** distinct entries
    /// (`/init` + `/helper`), the real reproducible-cpio pipeline spec §4.5 names, not a shell
    /// recipe. `multifile_init.c` execs the bundled `/helper` and waits for it before powering off,
    /// so a real KVM boot reaching both markers proves the pipeline-built archive is not just
    /// byte-correct (already covered by `initramfs.rs`'s own unit tests) but genuinely bootable and
    /// multi-file-capable -- the concrete shape §11's eventual harness+emulator image will need.
    /// Reuses the already-built, checked-in `bzImage` (no kernel rebuild: nothing about this test
    /// touches kernel config).
    #[test]
    #[ignore]
    fn guest_boots_a_pipeline_built_multi_file_initramfs() {
        if std::process::Command::new("musl-gcc")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("Skipping guest_boots_a_pipeline_built_multi_file_initramfs: musl-gcc not found on PATH");
            return;
        }

        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/linux-guest");
        let scratch = tempfile::tempdir().unwrap();

        let compile = |source: &str, output: &std::path::Path| {
            let status = std::process::Command::new("musl-gcc")
                .args(["-static", "-Os", "-o"])
                .arg(output)
                .arg(fixture_dir.join(source))
                .status()
                .unwrap_or_else(|e| panic!("failed to spawn musl-gcc for {source}: {e}"));
            assert!(status.success(), "musl-gcc failed to compile {source}");
            let strip_status = std::process::Command::new("strip").arg(output).status();
            assert!(strip_status.map(|s| s.success()).unwrap_or(false), "strip failed for {source}");
        };

        let init_bin = scratch.path().join("init");
        let helper_bin = scratch.path().join("helper");
        compile("multifile_init.c", &init_bin);
        compile("helper.c", &helper_bin);

        let entries = [
            baud_packages::InitramfsEntry::regular("init", 0o755, std::fs::read(&init_bin).unwrap()),
            baud_packages::InitramfsEntry::regular("helper", 0o755, std::fs::read(&helper_bin).unwrap()),
        ];
        let initramfs = baud_packages::build_reproducible_initramfs(&entries)
            .expect("pipeline-built multi-file initramfs must assemble successfully");

        let kernel = linux_guest_kernel_path();
        let cmdline = bootparams::DETERMINISTIC_CMDLINE;
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR (arch/x86/include/asm/irq_vectors.h)
        const INIT_MARKER: &str = "baud-guest: multi-file init reached /init\n";
        const HELPER_MARKER: &str = "baud-guest: helper executed from a multi-file initramfs\n";

        let mut tick_counts = Vec::new();
        for i in 0..2 {
            let mut m = Multiverse::boot_with_rdseed_sites(
                &kernel,
                cmdline,
                0,
                1,
                vec![],
                None,
                Some(&initramfs),
                [],
            )
            .unwrap_or_else(|e| panic!("run {i}: boot failed: {e}"));
            let (ticks, halt) = m
                .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
                .unwrap_or_else(|e| panic!("run {i}: periodic run failed: {e}"));
            let console = String::from_utf8_lossy(&halt.console_output).to_string();
            assert!(
                console.contains(INIT_MARKER),
                "run {i}: /init (bundled entry 1 of the pipeline-built archive) must run; got:\n{console}"
            );
            assert!(
                console.contains(HELPER_MARKER),
                "run {i}: /helper (bundled entry 2 of the pipeline-built archive) must be found and \
                 exec'd by /init; got:\n{console}"
            );
            tick_counts.push(ticks.len());
        }
        assert_eq!(
            tick_counts[0], tick_counts[1],
            "the same pipeline-built image+tape must survive the same number of periodic ticks \
             before its own natural halt across two boots"
        );
    }

    /// todo.md §14 item 1's H8 (Mario) prerequisite: no dynamically-linked binary has ever booted
    /// through this pipeline (every fixture so far, including the multi-file one above, links
    /// statically via `musl-gcc -static`), and `InitramfsEntry` had no symlink node type at all --
    /// a hard blocker for any real glibc/Buildroot/Nix rootfs, whose dynamic linker is reached
    /// almost universally through a symlink (`/lib64/ld-linux-x86-64.so.2` -> a versioned path
    /// under `/lib/x86_64-linux-gnu/` on Debian/Ubuntu, confirmed via this dev host's own
    /// `/lib64/ld-linux-x86-64.so.2`). This test builds a real, non-static, glibc-linked `/init`
    /// (`dynamic_init.c`, `-no-pie` for a fixed, deterministic load address, `-Wl,-rpath=...` so
    /// the executable's own `DT_RUNPATH` resolves `libc.so.6` without needing an `/etc/ld.so.cache`
    /// this initramfs has none of) and assembles an initramfs carrying the host's own real
    /// `ld-linux-x86-64.so.2` + `libc.so.6` (this dev host's glibc *is* the guest's glibc -- both
    /// are the identical x86_64 Linux ABI, no cross-build needed) as regular files, plus the
    /// `/lib64/ld-linux-x86-64.so.2` symlink the compiled binary's own `PT_INTERP` names, via
    /// `build_reproducible_initramfs`'s new `InitramfsEntry::symlink`. Reuses the already-built,
    /// checked-in `bzImage` (no kernel rebuild: `CONFIG_BINFMT_ELF=y` already loads dynamic ELFs
    /// the same way as static ones -- it is the same handler, gated only on `PT_INTERP` being
    /// present and resolvable).
    #[test]
    #[ignore]
    fn guest_boots_a_dynamically_linked_glibc_init() {
        let host_ld_so = Path::new("/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2");
        let host_libc = Path::new("/lib/x86_64-linux-gnu/libc.so.6");
        if std::process::Command::new("gcc")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
            || !host_ld_so.exists()
            || !host_libc.exists()
        {
            eprintln!(
                "Skipping guest_boots_a_dynamically_linked_glibc_init: gcc or \
                 /lib/x86_64-linux-gnu/{{ld-linux-x86-64.so.2,libc.so.6}} not found on this host"
            );
            return;
        }

        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/linux-guest");
        let scratch = tempfile::tempdir().unwrap();
        let init_bin = scratch.path().join("dynamic_init");
        let status = std::process::Command::new("gcc")
            .args(["-no-pie", "-O0", "-o"])
            .arg(&init_bin)
            .arg(fixture_dir.join("dynamic_init.c"))
            .arg("-Wl,-rpath=/lib/x86_64-linux-gnu")
            .status()
            .unwrap_or_else(|e| panic!("failed to spawn gcc: {e}"));
        assert!(status.success(), "gcc failed to compile dynamic_init.c");

        let entries = [
            baud_packages::InitramfsEntry::regular("init", 0o755, std::fs::read(&init_bin).unwrap()),
            baud_packages::InitramfsEntry::regular(
                "lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
                0o755,
                std::fs::read(host_ld_so).unwrap(),
            ),
            baud_packages::InitramfsEntry::regular(
                "lib/x86_64-linux-gnu/libc.so.6",
                0o755,
                std::fs::read(host_libc).unwrap(),
            ),
            baud_packages::InitramfsEntry::symlink(
                "lib64/ld-linux-x86-64.so.2",
                "../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
            ),
        ];
        let initramfs = baud_packages::build_reproducible_initramfs(&entries)
            .expect("pipeline-built dynamically-linked initramfs must assemble successfully");

        let kernel = linux_guest_kernel_path();
        let cmdline = bootparams::DETERMINISTIC_CMDLINE;
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR (arch/x86/include/asm/irq_vectors.h)
        const INIT_MARKER: &str = "baud-guest: dynamically-linked init reached /init\n";

        let mut tick_counts = Vec::new();
        for i in 0..2 {
            let mut m = Multiverse::boot_with_rdseed_sites(
                &kernel,
                cmdline,
                0,
                1,
                vec![],
                None,
                Some(&initramfs),
                [],
            )
            .unwrap_or_else(|e| panic!("run {i}: boot failed: {e}"));
            let (ticks, halt) = m
                .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
                .unwrap_or_else(|e| panic!("run {i}: periodic run failed: {e}"));
            let console = String::from_utf8_lossy(&halt.console_output).to_string();
            assert!(
                console.contains(INIT_MARKER),
                "run {i}: dynamically-linked /init must run to completion (ld.so must resolve \
                 libc.so.6 through the pipeline-built initramfs's symlink+regular-file layout); \
                 got:\n{console}"
            );
            tick_counts.push(ticks.len());
        }
        assert_eq!(
            tick_counts[0], tick_counts[1],
            "the same dynamically-linked image+tape must survive the same number of periodic ticks \
             before its own natural halt across two boots"
        );
    }

    /// `tests/fixtures/linux-guest/entropy_init.c` -- a second `/init` for the same, already-built
    /// `linux-guest` kernel (no kernel rebuild needed: OS-entropy determinism is a userspace-visible
    /// property this fixture's own `minimal.config` already supports -- `CONFIG_DEVTMPFS_MOUNT=y`
    /// gives it `/dev/urandom` for free). It calls `getrandom()` four times and reads `/dev/urandom`
    /// four times, hex-encoding each 32-byte read and writing it out through the same raw-`outb`
    /// COM1 endpoint `init.c` uses (`BUILD.md` explains why: no interrupt controller, so the normal
    /// interrupt-driven tty transmit path never drains).
    fn linux_guest_entropy_initramfs() -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/linux-guest/entropy_initramfs.cpio.gz");
        std::fs::read(path).expect("read linux-guest entropy_initramfs fixture")
    }

    /// todo.md §14 item 2 / H7 (`os_entropy_is_deterministic`): proves that on a real, unmodified
    /// Linux kernel booted through baud-multiverse, `getrandom()` and `/dev/urandom` are a pure
    /// function of the tape -- both across two independent boots of the same image+tape (the CRNG's
    /// seeding inputs, §3.8, are all hypervisor-controlled: trapped `rdtsc`/`rdrand`, the pinned
    /// `SETUP_RNG_SEED` boot seed, deterministic interrupt timing) and non-trivially (the eight
    /// 32-byte reads are not all the same value, so this isn't just observing an always-zero
    /// buffer). musl (this fixture's libc) has no vDSO `getrandom` path, so only the syscall side of
    /// the spec's "both the syscall and, on glibc 2.41+, the vDSO path" is exercised here -- the
    /// vDSO path needs a glibc 2.41+ guest, which is future work, not a gap in this fixture's own
    /// contract.
    ///
    /// **Requires the enforced (RDTSC-trapping) `kvm_intel.ko`** (`kernel-module/baud-enforced/
    /// rdtsc-enforce.patch`), like the sibling `*_enforced_regime_is_bit_exact_across_boots` tests
    /// -- `#[ignore]`d so a normal `cargo test --workspace` (stock module) never runs it; only
    /// `drive/manual/h7-enforced-entropy.sh` invokes it by name, after the same swap-in/swap-out dance
    /// `drive/manual/h3-enforced-rdtsc.sh` uses. This was empirically load-bearing, not a defensive
    /// precaution: an earlier version of this test against the *stock* module (RDTSC executing
    /// natively) failed non-deterministically even with the `SETUP_RNG_SEED` boot seed pinned and
    /// `random.trust_bootloader=on` set, because `random_init()` (`drivers/char/random.c`)
    /// unconditionally mixes `ktime_get_real()` -- which, with no RTC and only a TSC clocksource,
    /// reads the real (untrapped) hardware TSC at a point that varies by host-scheduling jitter
    /// between independent boots -- into the pool and re-extracts the CRNG key from it, *after* the
    /// pinned seed already credited the pool. Only with RDTSC hardware-trapped and served from the
    /// work-clock (a pure function of the branch counter, not wall time) does that `ktime_get_real()`
    /// read become reproducible too.
    ///
    /// **Still flaky, improved but not fully fixed (todo.md §14 next-actions item 2).** A prior
    /// iteration root-caused and fixed the largest source of divergence: `WorkClock`'s RCB-backed
    /// `perf_event` counter (`LinuxBranchCounter`) could not use `exclude_host` on this project's
    /// own nested-virtualized dev host (that flag reads back `0` here), so it accumulated *host*
    /// userspace dispatch branches (allocations, ioctls, match arms) between guest exits, not just
    /// guest branches — contaminating the served RCB/virtual-TSC value with data-dependent,
    /// run-varying host noise. Fixed by pausing that counter (`BranchCounter::pause`/`resume`,
    /// `TimeSource::pause_rcb`/`resume_rcb`) for every stretch of userspace code between exits and
    /// resuming it only for the actual `KVM_RUN` ioctl window (`run_and_convert_rcb_bracketed`,
    /// `crates/baud-vcpu/src/linux/mod.rs`), across all four real `KVM_RUN` call sites (the plain
    /// run loop plus `LinuxPmuStepper`'s three). Measured effect on real hardware: the observed pass
    /// rate rose from an estimated ~25-50% (the ~50-75% failure rate this doc previously described)
    /// to ~75% (15/20 across two batches) — a real, verified improvement, not a full fix.
    ///
    /// **Residual ~25% root-caused this iteration by the tick diagnostic below (confirmed, not just
    /// hypothesized).** A captured divergent pair showed the *same* tick count (13 == 13) and the
    /// *same* landing `rip` (`0xffffffff81424b64`) on both boots at tick 0 — ruling out both a
    /// control-flow divergence and a wrong injection site — yet the landing RCB itself overshot the
    /// 500,000 target by a different amount on each boot (500,192 vs 500,158, a 34-count
    /// disagreement, well past the ±8 tolerance [`RCB_HARDWARE_JITTER_TOLERANCE`] documents
    /// elsewhere). That overshoot *is* the served virtual-TSC value at the exact instant the timer
    /// interrupt lands — and Linux's own `add_interrupt_randomness()` folds `random_get_entropy()`
    /// (== a raw `rdtsc`/our served value) into the CRNG pool on *every* interrupt, uncredited but
    /// still mixed in (spec §3.8's own "why not just set kernel knobs" paragraph: mixing happens
    /// regardless of crediting). So a same-instruction, same-tick-count interrupt still feeds a
    /// different raw number into the guest's CRNG key each boot — this fully explains the observed
    /// `getrandom()`/`/dev/urandom` divergence without needing any further contamination source or
    /// control-flow explanation. This confirms, not refutes, the standing hypothesis that
    /// `WorkClock`'s long-lived pinned counter and `LinuxPmuStepper`'s own per-tick freshly-created
    /// pinned counter (both counting the identical hardware event on the identical thread, up to
    /// `MAX_TICKS` times per boot) disagree on exactly *when* the arm-early-then-single-step engine
    /// judges the target crossed — a two-fd epoch/scheduling disagreement, not raw single-fd
    /// hardware imprecision (§3.7's own H0 gate already established the raw `BR_INST_RETIRED.COND`
    /// event is bit-exact on a single always-running fd, so 34 counts of *landing-precision* jitter
    /// implicates the second fd, not the counted event itself). **Fixed this iteration, and
    /// confirmed on real hardware**: `LinuxPmuStepper` no longer owns a second `perf_event` fd at
    /// all — `arm_overflow`/`current_rcb` (`crates/baud-vcpu/src/linux/pmu.rs`) now read
    /// `TimeSource::current_rcb` directly, the same single pinned fd `WorkClock` owns, so the
    /// interrupt-injection engine's "is the target crossed yet" reads and the work-clock's own
    /// served value are, by construction, the identical read of the identical fd — no second
    /// epoch left to disagree with. A real-hardware `H7_ENTROPY_REPEATS=10` batch
    /// (`drive/manual/h7-enforced-entropy.sh`) after this fix (and after also fixing `SPURIOUS_LAPIC_LINE`
    /// below, a second, independent bug this fix's improved timing precision exposed at a much
    /// higher rate) passed 7/10, and every one of the 3 failures showed the *same* tick count and
    /// a **1-2 count** RCB-delta disagreement (down from the pre-fix 34) with a bit-identical
    /// landing `rip` — squarely inside [`RCB_HARDWARE_JITTER_TOLERANCE`]'s already-documented
    /// single-fd hardware-read-precision floor, not a further cross-fd epoch bug. The two-fd
    /// architectural disagreement this section originally diagnosed is confirmed eliminated; what
    /// remains is irreducible real-hardware `perf_event` read jitter that `add_interrupt_
    /// randomness()`'s zero-tolerance CRNG mixing (any nonzero jitter changes the mixed-in value)
    /// is uniquely sensitive to, unlike the ±8-count-tolerant `rip`-equality tests elsewhere in
    /// this file. Whether that residual floor can be driven lower (e.g. a `perf_event` read
    /// technique with less inherent jitter than a plain `read()` syscall) is future work, not
    /// re-litigated here. On divergence this test now reports, instead of a bare byte-diff, whether
    /// the two
    /// boots' `run_to_first_halt_with_periodic_timer` tick streams (rip + cumulative rcb per tick)
    /// took a different number of ticks (would indicate a genuine control-flow divergence, e.g. a
    /// TSC-calibration loop like `calibrate_delay()` iterating a different number of times — not
    /// observed in the captured case above), or the same tick count with a per-tick RCB-delta
    /// disagreement at a specific tick (the case observed, above), or neither (the divergence would
    /// be invisible to the tick stream and must originate elsewhere — within a `KVM_RUN` window,
    /// RDTSCP's TSC_AUX half, or the CRNG-mixing layer itself).
    ///
    /// **The residual floor above is now RESOLVED, not just narrowed.** It was never irreducible
    /// hardware imprecision: `LinuxBranchCounter` (this file) and `crates/baud-host/src/
    /// linux.rs`'s `measure_fixed_loop_branches` were both still reading the generic
    /// `PERF_COUNT_HW_BRANCH_INSTRUCTIONS` perf event (all branches) instead of the raw
    /// `BR_INST_RETIRED.COND` event (`0x11c4`) specs §3.3 and `docs/determinism.md`'s own H0
    /// measurement always specified — that generic event was independently measured
    /// `±1`-nondeterministic on this exact host (`docs/determinism.md`'s table), which is exactly
    /// the few-count landing-RCB jitter this doc spent several iterations chasing. Switching both
    /// call sites to the raw event (see [`BR_INST_RETIRED_COND`]) took a real-hardware
    /// `H7_ENTROPY_REPEATS=10` batch to 10/10 (`drive/manual/h7-enforced-entropy.sh`), and the sibling
    /// `double_boot_ram_hash_identical` test below — same root cause, previously 0/8-4/8 — to
    /// 25/25 across two batches (`drive/manual/h7-enforced-checkpoint.sh`).
    #[test]
    #[ignore]
    fn os_entropy_is_deterministic() {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_entropy_initramfs();
        // `random.trust_bootloader=on` makes the kernel *credit* the pinned `SETUP_RNG_SEED` node
        // `boot_guest` always writes (§3.8), marking the CRNG ready synchronously from the
        // tape-derived seed alone rather than falling back to the jitter/interrupt-timing path.
        // Spec §4.2's exact cmdline (todo.md §14 next-actions item 1's closing item); already
        // included `random.trust_cpu=off random.trust_bootloader=on`, so this call site's diff
        // from `DETERMINISTIC_CMDLINE` was the smallest of the three.
        let cmdline = bootparams::DETERMINISTIC_CMDLINE;
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR (arch/x86/include/asm/irq_vectors.h)
        const ENTROPY_MARKER: &str = "baud-guest: entropy probe done\n";
        // Real-hardware finding (todo.md §14): `entropy_init.c` writes each hex-encoded probe line
        // via raw `outb` (no interrupt-driven tty path exists on this machine, `BUILD.md`'s own
        // documented reason), so a periodic timer tick landing *mid-write* lets the kernel's own
        // asynchronous "no LAPIC device model" diagnostic (`linux-guest/BUILD.md`: harmless, expected
        // on every tick since there is no LAPIC) interleave character-by-character into whatever probe
        // line was in flight at that instant -- e.g. `URANDOM:ac3595Spurious LAPIC timer interrupt on
        // cpu 0\na15749e0...` splits one 64-hex-char line into a corrupted 6-char fragment plus an
        // orphaned continuation. This is a console-capture race, not entropy nondeterminism: the
        // kernel's diagnostic text is itself fixed and deterministic, so stripping every occurrence of
        // it before line-splitting reassembles the probe line exactly as the guest's own outb sequence
        // produced it, with no effect on the entropy bytes themselves.
        //
        // BUG FOUND AND FIXED (todo.md §14 next-actions item 2(c)'s counter-reconciliation fix made
        // this land far more consistently, exposing that the strip below was a silent no-op all
        // along): every kernel `printk` line on this fixture's serial console is terminated `\r\n`,
        // not a bare `\n` (the 8250 driver's own CRLF translation) -- confirmed by inspecting the
        // raw captured bytes (`cat -A`: every kernel line ends `^M$`, i.e. `\r\n`). The guest's own
        // *userspace* `outb` writes (this probe's hex lines, `ENTROPY_MARKER`) bypass that driver
        // entirely and use a bare `\n`, so only the kernel-sourced diagnostic needs the `\r`. Before
        // this fix `.replace(SPURIOUS_LAPIC_LINE, "")` below never matched anything (the pattern's
        // trailing `\n` never lined up with the real `\r\n`), so every interleaved occurrence
        // silently survived into `probes`, corrupting a probe line at the exact hex.len()==64 check
        // whenever a tick happened to land mid-write -- previously mistaken for run-to-run RCB
        // divergence in a fraction of failures, since both bugs manifest as the same test failing.
        const SPURIOUS_LAPIC_LINE: &str = "Spurious LAPIC timer interrupt on cpu 0\r\n";

        let mut probe_runs = Vec::new();
        // todo.md §14 next-actions item 2's "needed next" step: direct instrumentation of the
        // served RCB value at each tick, to tell apart the two remaining candidate explanations
        // for the residual ~25% divergence -- cross-counter PMU contention (which would show up
        // as a per-tick RCB-delta anomaly, or a differing tick *count* between the two boots,
        // since a bad served virtual-TSC value can change how many iterations a TSC-calibration
        // loop like `calibrate_delay()` takes -- a real control-flow divergence) versus the
        // already-acknowledged `RCB_HARDWARE_JITTER_TOLERANCE`-class hardware-read imprecision
        // (which would not correlate with any particular tick). Kept even on the passing path
        // (not just on divergence) so `tick_runs` is available to the diagnostic below without a
        // second, non-reproducing run.
        let mut tick_runs: Vec<Vec<TimerTick>> = Vec::new();
        for i in 0..2 {
            let mut m = Multiverse::boot_with_rdseed_sites(
                &kernel,
                cmdline,
                0,
                1,
                vec![],
                None,
                Some(&initramfs),
                [],
            )
            .unwrap_or_else(|e| panic!("run {i}: boot failed: {e}"));
            let (ticks, halt) = m
                .run_to_first_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
                .unwrap_or_else(|e| panic!("run {i}: periodic run failed: {e}"));
            tick_runs.push(ticks);
            let console =
                String::from_utf8_lossy(&halt.console_output).replace(SPURIOUS_LAPIC_LINE, "");
            assert!(
                console.contains(ENTROPY_MARKER),
                "run {i}: guest must reach the entropy probe's own completion marker; got:\n{console}"
            );
            let probes: Vec<&str> = console
                .lines()
                .filter(|l| l.starts_with("GETRANDOM:") || l.starts_with("URANDOM:"))
                .collect();
            assert_eq!(
                probes.len(),
                8,
                "run {i}: expected 4 GETRANDOM + 4 URANDOM probe lines; got:\n{console}"
            );
            for line in &probes {
                let (tag, hex) = line.split_once(':').unwrap();
                assert_eq!(
                    hex.len(),
                    64,
                    "run {i}: {tag} probe must be a 32-byte (64 hex char) read, not an error/short \
                     read; got line {line:?} from console:\n{console}"
                );
            }
            probe_runs.push(probes.into_iter().map(str::to_string).collect::<Vec<_>>());
        }

        if probe_runs[0] != probe_runs[1] {
            let (n0, n1) = (tick_runs[0].len(), tick_runs[1].len());
            let mut diag = format!("tick counts: run0={n0} ticks, run1={n1} ticks\n");
            if n0 != n1 {
                diag += "-> tick COUNTS DIFFER: the two boots took a different number of \
                         periodic-timer ticks to reach halt, i.e. a real control-flow divergence \
                         (e.g. a TSC-calibration loop like calibrate_delay() ran a different \
                         number of iterations), not just a numeric read-jitter -- points at the \
                         served virtual-TSC/RCB value itself being wrong at some point, not only \
                         hardware-read imprecision.\n";
            } else {
                // Same tick count: compare the RCB *delta* between consecutive ticks (should be
                // ~PERIOD_RCB every time) run-for-run, and report the first tick whose delta
                // disagrees between the two boots. Pre-fix (todo.md §14 next-actions item 2(c)),
                // this would have pinpointed whether the divergence clustered at a particular tick
                // (implicating the now-removed second `LinuxPmuStepper` fd's own epoch) or was
                // spread uniformly (pure hardware read jitter, RCB_HARDWARE_JITTER_TOLERANCE-class).
                // Kept as a live diagnostic in case any disagreement still surfaces post-fix.
                let mut first_divergence = None;
                for idx in 0..n0 {
                    let d0 = tick_runs[0][idx]
                        .rcb
                        .saturating_sub(if idx == 0 { 0 } else { tick_runs[0][idx - 1].rcb });
                    let d1 = tick_runs[1][idx]
                        .rcb
                        .saturating_sub(if idx == 0 { 0 } else { tick_runs[1][idx - 1].rcb });
                    if d0 != d1 {
                        first_divergence = Some((idx, d0, d1));
                        break;
                    }
                }
                match first_divergence {
                    Some((idx, d0, d1)) => {
                        diag += &format!(
                            "-> same tick count, but tick {idx}/{n0} is the first whose \
                             RCB delta from the previous tick disagrees between boots: \
                             run0 delta={d0} run1 delta={d1} (period_rcb={PERIOD_RCB}); \
                             run0 rip={:#x} run1 rip={:#x}\n",
                            tick_runs[0][idx].rip, tick_runs[1][idx].rip
                        );
                    }
                    None => {
                        diag += "-> same tick count AND identical per-tick RCB deltas across \
                                 both boots -- the entropy divergence is NOT reflected in the \
                                 timer-tick RCB stream at all, so it originates somewhere the \
                                 tick instrumentation doesn't observe (e.g. within a single \
                                 KVM_RUN window between ticks, or in RDTSCP's TSC_AUX half, or \
                                 truly at the CRNG-mixing layer itself).\n";
                    }
                }
            }
            panic!(
                "getrandom()/dev/urandom must be byte-identical across two boots of the same \
                 image+tape -- an unmodified Linux CRNG is a pure function of the tape\n{diag}\
                 run0={:?}\nrun1={:?}",
                probe_runs[0], probe_runs[1]
            );
        }

        let distinct: std::collections::HashSet<&String> = probe_runs[0].iter().collect();
        assert!(
            distinct.len() > 1,
            "the 8 probe reads must not all be the same value -- otherwise this test would pass \
             even if entropy were degenerate (e.g. an always-zeroed buffer): {:?}",
            probe_runs[0]
        );
    }

    /// `tests/fixtures/linux-guest/checkpoint_init.c` -- a third `/init` for the same already-built
    /// kernel: identical to `init.c` except it finalizes one extra tape-device `MARK_BRANCH` record
    /// (`outb(1, 0x508)`) right before powering off, so a test can hash guest RAM at that exact,
    /// guest-chosen instant (see this fixture's `BUILD.md` for why).
    fn linux_guest_checkpoint_initramfs() -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/linux-guest/checkpoint_initramfs.cpio.gz");
        std::fs::read(path).expect("read linux-guest checkpoint_initramfs fixture")
    }

    /// H7's `double_boot_ram_hash_identical` (todo.md §14 next-actions item 2): the two boots'
    /// guest RAM must be byte-identical at a **guest-driven checkpoint** (`MARK_BRANCH`, opcode 1)
    /// rather than at a wall-clock point or over a full boot's raw console/RAM comparison -- the
    /// approach `guest_kernel_boots_to_userspace`'s own doc explains is not viable here: a first
    /// attempt at strict console-byte-equality found the two boots differ in exactly one
    /// kernel-internal `printk` line (`sched_clock: Marking stable`) that embeds real, run-varying
    /// TSC numbers (raw, untrapped `rdtsc` under the stock module reads real hardware time).
    ///
    /// **Requires the enforced (RDTSC/RDTSCP-trapping) `kvm_intel.ko`, like
    /// `os_entropy_is_deterministic`** -- and for the same underlying reason, not merely by
    /// analogy: moving the RAM-hash checkpoint later in the guest's own execution does not exempt
    /// bytes the kernel already printed (and which stay resident in its own `printk` ring buffer,
    /// ordinary kernel data inside the whole-RAM-hashed region) earlier in boot under the *stock*
    /// module's real-TSC reads. Only with RDTSC/RDTSCP hardware-trapped and served from the
    /// work-clock does every RAM byte the checkpoint would hash become a pure function of the
    /// tape. `#[ignore]`d for this reason; `drive/manual/h7-enforced-checkpoint.sh` runs it with
    /// `--ignored` after the same swap-in/swap-out dance `drive/manual/h7-enforced-entropy.sh` uses.
    ///
    /// **Real-hardware result: fails 100% of the time even under the enforced module (0/8 across
    /// two real batches), unlike `os_entropy_is_deterministic`'s ~70-90% pass rate on the
    /// identical enforced-regime machinery -- root-caused, not just observed.** A one-off
    /// diagnostic (booting twice, keeping both `Multiverse`s alive, and diffing raw guest RAM
    /// byte-for-byte instead of just hashing it) found only 77,589 of 268,435,456 bytes differ
    /// (0.03%), and crucially they are **not** scattered like independent random draws would be:
    /// the differing region's bytes decode as a repeating `JMP rel32` + `UD1` sequence (`e9 ..
    /// .. .. ..` `0f b9 cc`) -- the well-known padding pattern the kernel's `static_call`/
    /// jump-label infrastructure uses for a patchable call-site trampoline (`arch/x86/kernel/
    /// static_call.c`) -- with a genuinely different (not small-jitter) `rel32` displacement each
    /// boot, i.e. the patched trampoline points at two different valid targets, not the same
    /// target read imprecisely. This means at least one `static_call` site gets updated to a
    /// different function/target depending on a runtime decision that itself depends on the
    /// already-documented residual RCB/TSC read jitter (the same root mechanism that makes the
    /// `sched_clock: Marking stable (A,B)->(C,0)` printk line's embedded numbers differ) -- here
    /// visibly changing *which code runs*, not just a printed number, which is presumably why a
    /// full-RAM comparison catches it every time while `os_entropy_is_deterministic`'s narrow
    /// 8-probe check mostly does not. Driving this to 100% would need either eliminating that
    /// residual single-fd `perf_event`-read jitter to exactly zero (open per
    /// `os_entropy_is_deterministic`'s own doc) or identifying and pinning the specific
    /// static-call site involved -- both future work, not attempted this iteration. Kept
    /// `#[ignore]`d and wired into `drive/manual/h7-enforced-checkpoint.sh`.
    ///
    /// **RESOLVED.** The "residual single-fd `perf_event`-read jitter" above was never irreducible
    /// hardware imprecision -- it was `LinuxBranchCounter` (this file) and `crates/baud-host/src/
    /// linux.rs`'s `measure_fixed_loop_branches` both still reading the generic `PERF_COUNT_HW_
    /// BRANCH_INSTRUCTIONS` perf event (all branches) instead of the raw `BR_INST_RETIRED.COND`
    /// event (`0x11c4`, see [`BR_INST_RETIRED_COND`]) specs §3.3 and `docs/determinism.md`'s own H0
    /// measurement always specified -- the generic event was independently measured
    /// `±1`-nondeterministic on this exact host, exactly matching the jitter that let the
    /// `static_call` trampoline's runtime decision resolve differently each boot. Switching both
    /// call sites to the raw event took a real-hardware batch from 0/8 to 25/25 across two runs
    /// (10/10 then 15/15, `drive/manual/h7-enforced-checkpoint.sh`), and the identical fix took
    /// `os_entropy_is_deterministic` above from ~70-90% to 10/10. `drive/manual/h7-enforced-checkpoint.sh`
    /// now hard-gates on this test like any other real-hardware check.
    #[test]
    #[ignore]
    fn double_boot_ram_hash_identical() {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_checkpoint_initramfs();
        // Spec §4.2's exact cmdline (todo.md §14 next-actions item 1's closing item).
        let cmdline = bootparams::DETERMINISTIC_CMDLINE;
        const PERIOD_RCB: u64 = 500_000;
        const MAX_TICKS: u32 = 2000;
        const TIMER_VECTOR: u8 = 0xec; // Linux's LOCAL_TIMER_VECTOR (arch/x86/include/asm/irq_vectors.h)

        let mut ram_hashes = Vec::new();
        let mut step_at_checkpoint = Vec::new();
        for i in 0..2 {
            let mut m = Multiverse::boot_with_rdseed_sites(
                &kernel,
                cmdline,
                0,
                1,
                vec![],
                None,
                Some(&initramfs),
                [],
            )
            .unwrap_or_else(|e| panic!("run {i}: boot failed: {e}"));
            let (_ticks, outcome, _records) = m
                .run_until_branch_or_halt_with_periodic_timer(PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)
                .unwrap_or_else(|e| panic!("run {i}: periodic run failed: {e}"));
            let step = match outcome {
                RunUntilBranchOutcome::MarkBranch { step } => step,
                RunUntilBranchOutcome::Halted(halt) => panic!(
                    "run {i}: guest must reach the MARK_BRANCH checkpoint before its own halt; \
                     it halted first with console:\n{}",
                    String::from_utf8_lossy(&halt.console_output)
                ),
            };
            step_at_checkpoint.push(step);
            ram_hashes.push(m.ram_hash());
        }

        assert_eq!(
            step_at_checkpoint[0], step_at_checkpoint[1],
            "the guest-driven checkpoint must land at the same tape cursor across two boots"
        );
        assert_eq!(
            ram_hashes[0], ram_hashes[1],
            "guest RAM at the guest-driven MARK_BRANCH checkpoint must be byte-identical across \
             two boots of the same image+tape"
        );
    }

    /// H5's named test (specs/baud-snapshot.md §7, todo.md §10): `Multiverse::snapshot`/`restore`
    /// wired for the first time against a real guest and real KVM hardware (todo.md §14 tracked
    /// this exact gap: "nothing calls snapshot/restore/DirtyRing on real KVM hardware yet").
    /// Reuses `timer-guest` (H4's fixture, above) since it is the only fixture with a mid-run
    /// observation stream (one console byte per delivered tick) long enough to define a capture
    /// point `K` and a continuation `K+M`.
    ///
    /// Two runs are driven from the same image+tape: a *straight* run delivers both ticks and
    /// halts without ever snapshotting; a *capture-then-restore* run delivers only the first tick,
    /// captures a [`Universe`] at that point (`K`), reconstructs a brand-new `Multiverse` from it
    /// via [`Multiverse::restore`], delivers the second tick on the restored instance (`K+M`), and
    /// halts. If any field of the capture set (RAM, vCPU state, work-clock anchor, tape cursor,
    /// console history) were missing or wrong, the restored run would diverge from the straight
    /// run at or after the restore point — asserted here via the second tick's landing `rip` and
    /// the final console output / RAM hash (the actual guest-observable "observation stream" the
    /// spec's own pseudocode compares), which only matches if the restored console history
    /// (`DeviceBus::restore`'s job, since `baud-snapshot::linux::restore` deliberately leaves
    /// device state to the caller) was seeded correctly from the captured universe.
    ///
    /// **Deliberately does not assert `rcb` equality across the restore boundary** (unlike
    /// `timer_tick_lands_at_identical_instruction`'s tight [`RCB_HARDWARE_JITTER_TOLERANCE`]):
    /// real-hardware investigation this iteration found the two are not comparable quantities.
    /// Within one continuously-running `Multiverse`, every `current_rcb()` read reuses the same
    /// long-lived `perf_event` fd, so successive reads only ever disagree with another run's by
    /// the documented ±8 hardware read-precision jitter. Across a restore, [`Multiverse::restore`]
    /// necessarily creates a *brand-new* `perf_event` fd (a process cannot resurrect another fd's
    /// already-elapsed hardware count) seeded with [`WorkClock::rcb_offset`](crate::timesource::
    /// WorkClock) — and creating/enabling that fresh fd costs a real, one-time, few-hundred-branch
    /// "warm-up" overhead (confirmed by instrumentation: a fresh counter read immediately after
    /// `Multiverse::restore` returns already reads several hundred branches ahead of the anchor it
    /// was seeded with) that a straight run's *second* tick never pays, because it reuses the
    /// counter created once at boot. This warm-up cost does not represent a real determinism gap:
    /// it never affects the guest's own instruction stream, and it is why `rip` (identical) and
    /// the console/RAM observation stream (identical), not `rcb`, are this test's load-bearing
    /// assertions — exactly mirroring `RCB_HARDWARE_JITTER_TOLERANCE`'s own framing ("not a
    /// determinism escape hatch — rip is still required to be bit-identical, which is the
    /// guarantee that actually matters").
    #[test]
    fn snapshot_roundtrip_is_bit_identical() {
        let kernel = timer_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const PERIOD_RCB: u64 = 100_000;
        const WORK_CLOCK_K: u64 = 1;

        // The straight run: both ticks delivered on one continuous Multiverse, never snapshotted.
        let mut straight =
            Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, vec![], None).expect("straight boot failed");
        let straight_tick_1 = straight
            .inject_timer_tick(PERIOD_RCB, TIMER_VECTOR)
            .expect("straight run: first tick failed");
        let straight_tick_2 = straight
            .inject_timer_tick(PERIOD_RCB, TIMER_VECTOR)
            .expect("straight run: second tick failed");
        let straight_halt = straight.run_to_first_halt().expect("straight run: halt failed");

        // The capture-then-restore run: only the first tick is delivered before capturing at K;
        // the second tick and the halt happen on a brand-new `Multiverse` reconstructed from that
        // capture, sharing nothing with the original beyond the `Universe` value itself.
        let mut capture_run =
            Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, vec![], None).expect("capture-run boot failed");
        let capture_tick_1 = capture_run
            .inject_timer_tick(PERIOD_RCB, TIMER_VECTOR)
            .expect("capture run: first tick failed");
        assert_eq!(
            capture_tick_1.rip, straight_tick_1.rip,
            "sanity check: the pre-capture tick must land identically to the straight run's first \
             tick before restore is even involved"
        );

        let mut page_store = PageStore::new();
        let universe = capture_run.snapshot(&mut page_store).expect("snapshot (capture at K) failed");

        let mut restored = Multiverse::restore(&universe, vec![], WORK_CLOCK_K, false, None)
            .expect("restore from captured universe failed");
        let restored_tick_2 = restored
            .inject_timer_tick(PERIOD_RCB, TIMER_VECTOR)
            .expect("restored run: second tick failed");
        let restored_halt = restored.run_to_first_halt().expect("restored run: halt failed");

        assert_eq!(
            restored_tick_2.rip, straight_tick_2.rip,
            "the second tick must land on the bit-identical instruction whether delivered on the \
             straight run or on a Multiverse reconstructed via snapshot/restore at K — a missing \
             capture field would desynchronize the restored guest's execution before K+M"
        );
        assert_eq!(
            restored_halt.console_output, straight_halt.console_output,
            "the observation stream K..K+M (console output through the second tick and halt) must \
             be identical whether produced by the straight run or by continuing a restored \
             universe — this only holds if DeviceBus::restore correctly reseeded the console's \
             captured output history alongside the tape cursor"
        );
        assert_eq!(
            restored_halt.ram_hash, straight_halt.ram_hash,
            "guest RAM at the final halt must be byte-identical between the straight run and the \
             capture-then-restore run — any state field baud-snapshot's capture set omitted would \
             surface here as a RAM divergence introduced by the restored continuation"
        );
    }

    /// H5's `restore_refuses_mismatched_cpu` (specs/baud-snapshot.md §6 point 4/§8, todo.md §10),
    /// exercised for the first time against real KVM hardware (todo.md §14 tracked this exact gap:
    /// the CPU-model-mismatch refusal was previously only unit-tested at the pure
    /// `universe::model_matches` comparator level, never against the real `linux::restore` ioctl
    /// path and a real `Universe` captured off a live vCPU).
    ///
    /// This dev machine has exactly one CPU model, so a *naturally occurring* mismatch (a universe
    /// captured on host A restored on host B) cannot be produced here. The honest substitute: take
    /// a genuinely captured [`Universe`] — `cpu_signature` is opaque data the restore path never
    /// interprets, only compares (`baud_snapshot::linux::restore`'s doc) — and flip one bit of it,
    /// which is indistinguishable, from `restore`'s point of view, from a universe that really was
    /// captured on a different model; the exact same real `cpuid_leaf1_eax(kvm)` read and
    /// `model_matches` comparison this host would run against a genuine cross-model restore still
    /// executes. No production code changes: this closes the gap purely by exercising the refusal
    /// path (already wired since H5's first slice) that nothing had called with a mismatched
    /// signature yet.
    #[test]
    fn restore_refuses_mismatched_cpu() {
        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let mut boot = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("boot failed");
        let mut page_store = PageStore::new();
        let mut universe = boot.snapshot(&mut page_store).expect("snapshot failed");
        let real_signature = universe.cpu_signature;

        // Positive control: restoring the real, unmodified universe onto this exact host succeeds
        // — proves the refusal below is about the forged signature, not some unrelated restore
        // failure.
        Multiverse::restore(&universe, vec![], 1, false, None)
            .expect("restoring a universe captured on this exact host must succeed");

        // Forge a mismatched CPU signature (flip the low bit — guaranteed to differ from the real
        // one `cpuid_leaf1_eax` reads back on this host).
        universe.cpu_signature = real_signature ^ 1;

        match Multiverse::restore(&universe, vec![], 1, false, None) {
            Err(RestoreError::Snapshot(baud_snapshot::linux::RestoreError::CpuMismatch {
                captured,
                current,
            })) => {
                assert_eq!(captured, real_signature ^ 1, "error must report the forged captured signature");
                assert_eq!(current, real_signature, "error must report this host's real signature");
            }
            Ok(_) => panic!(
                "restoring a universe with a mismatched cpu_signature and no active CPUID \
                 template must refuse, but it succeeded"
            ),
            Err(other) => panic!(
                "restoring a universe with a mismatched cpu_signature and no active CPUID \
                 template must refuse with RestoreError::Snapshot(CpuMismatch), got {other:?}"
            ),
        }

        // An active CPUID template normalizes the mismatch and the restore proceeds.
        Multiverse::restore(&universe, vec![], 1, true, None)
            .expect("an active CPUID template must let a mismatched-signature restore proceed");
    }

    /// H5's `reset_cost_scales_with_write_set` (specs/baud-snapshot.md §5, todo.md §10, test-matrix
    /// row for the dirty-ring "reset" guarantee), exercised for the first time against real KVM
    /// hardware (todo.md §14 tracked this exact gap: `Multiverse`'s dirty-ring plumbing was
    /// unit/type-checked but nothing had ever called it against a real, running guest). Writing
    /// this test surfaced a real, previously-undiscovered production bug, now fixed in the same
    /// change: `KVM_CAP_DIRTY_LOG_RING` cannot be negotiated on a `VmFd` once any vCPU already
    /// exists (the kernel's own `kvm->created_vcpus` check, confirmed by this test's first run
    /// failing `enable_cap` with `EINVAL`) — the old API's `enable_dirty_ring(&mut self, entries)`,
    /// callable any time after `boot`, could therefore never actually succeed in this workspace,
    /// since `boot` always already has a vCPU by the time it returns. Fixed by moving capability
    /// negotiation into `boot`/`restore` themselves, before `create_vcpu`
    /// (`create_vm_vcpu_shell`'s new `dirty_ring_entries` parameter) — see
    /// `baud_snapshot::linux::DirtyRing`'s doc for the split `negotiate_capability`/`open` API this
    /// forced. `Multiverse::snapshot`'s returned `Universe::ram` remains exactly the `base_ram:
    /// &[PageRef]` shape `reset_dirty_pages` takes, so no other plumbing was needed.
    ///
    /// Reuses `timer-guest` (H4's fixture, above): its ISR (`payload.s`) pushes/pops `rax`/`rdx`
    /// onto the boot stack (`layout::BOOT_STACK_POINTER`, page index 15) every time an injected
    /// tick is delivered, on top of the CPU's own hardware-pushed interrupt frame at the same
    /// address — the fixture's only *explicit* memory writes.
    ///
    /// **Real-hardware finding (accepted, not a bug)**: the dirtied-page count is small but not
    /// exactly `1`. Beyond the ISR's stack writes, the guest's very first instructions after boot
    /// also dirty a handful of the identity-mapped page-table pages themselves — translating any
    /// linear address for the first time makes the CPU set each walked page-table entry's
    /// `ACCESSED` bit if it was not already set, which is itself a real write the same EPT dirty-
    /// tracking this ring relies on faithfully reports (this is genuinely correct behavior: from
    /// the kernel's point of view a PTE's accessed bit is guest-owned state, no different from any
    /// other guest write). This is bounded and expected, not a leak — `UPPER_BOUND` gives it
    /// generous headroom while still proving the guarantee that actually matters: dozens of pages,
    /// not `TOTAL_RAM_PAGES`'s tens of thousands.
    ///
    /// Sequence: boot with the dirty ring requested (negotiated/opened before any guest execution —
    /// anything written before this point is part of the "base" itself, per `boot`'s own doc),
    /// snapshot that pristine state as `base` and record its RAM hash, then deliver two ticks and
    /// run to halt (dirtying a handful of pages, per the finding above). `reset_dirty_pages(&base.
    /// ram)` must report a small, nonzero page count — far below total RAM — and rewinding must
    /// make guest RAM byte-identical to the pristine base again, proving the reset actually
    /// happened rather than just returned a plausible-looking number.
    #[test]
    fn reset_cost_scales_with_write_set() {
        let kernel = timer_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const PERIOD_RCB: u64 = 100_000;
        const WORK_CLOCK_K: u64 = 1;
        const TOTAL_RAM_PAGES: usize = layout::GUEST_RAM_SIZE / baud_snapshot::PAGE_SIZE;
        /// Generous headroom above the handful of pages this fixture's ISR-plus-page-table-
        /// accessed-bit writes are expected to dirty (see this test's own doc) — still three
        /// orders of magnitude below `TOTAL_RAM_PAGES`, so this remains a meaningful proof that
        /// reset cost scales with the write set rather than total RAM.
        const UPPER_BOUND: usize = 64;

        let mut multiverse = Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, vec![], Some(4096))
            .expect("boot with dirty ring failed");

        let mut page_store = PageStore::new();
        let base = multiverse.snapshot(&mut page_store).expect("base snapshot (pristine, pre-run) failed");
        let base_ram_hash = multiverse.ram_hash();

        let (ticks, halt) = multiverse
            .run_with_timer_ticks(PERIOD_RCB, TIMER_VECTOR, 2)
            .expect("run with timer ticks failed");
        assert_eq!(ticks.len(), 2, "both ticks must have been delivered");
        assert_ne!(
            halt.ram_hash, base_ram_hash,
            "the run must actually dirty some RAM (the ISR's stack pushes/pops) or this test's \
             reset-then-compare assertion below would be vacuous"
        );

        let reset_count =
            multiverse.reset_dirty_pages(&base.ram).expect("reset_dirty_pages failed");

        assert!(
            reset_count > 0,
            "the dirty ring must report at least one dirtied page (the boot-stack page the ISR \
             pushed/popped onto) — got 0"
        );
        assert!(
            reset_count < TOTAL_RAM_PAGES,
            "reset cost must scale with the write set, not total RAM: got {reset_count} dirtied \
             pages out of {TOTAL_RAM_PAGES} total RAM pages"
        );
        assert!(
            reset_count <= UPPER_BOUND,
            "expected at most {UPPER_BOUND} dirtied pages (the ISR's stack writes plus a handful \
             of page-table ACCESSED-bit updates from the guest's first address translations, see \
             this test's own doc) — got {reset_count}, suggesting something else is writing RAM too"
        );

        assert_eq!(
            multiverse.ram_hash(),
            base_ram_hash,
            "after reset_dirty_pages, guest RAM must be byte-identical to the pristine base \
             snapshot again — proving the reset genuinely rewound the dirtied pages' content, not \
             just returned a plausible-looking count"
        );
    }

    /// H5's `thousand_branches_are_independent_and_deterministic` (specs/baud-snapshot.md §7,
    /// todo.md §10), exercised for the first time against real KVM hardware. Closes the
    /// `Multiverse::branch` half of the real architecture gap todo.md §14 documented (§4's literal
    /// `UFFDIO_CONTINUE` CoW mechanism is still open — see [`Multiverse::branch`]'s doc) by proving
    /// its small-N `restore`-based fallback actually delivers the guarantee the named test cares
    /// about: many independent continuations forked from one shared branch point, each internally
    /// deterministic, none perturbing another.
    ///
    /// Reuses `tape-echo-guest` (H2's fixture, above) as the branch payload: it reads exactly 4
    /// tape bytes and echoes them verbatim to COM1, then halts, so each branch's expected output is
    /// pinned to its own tape suffix by construction — any cross-branch memory bleed (a branch
    /// reading another's guest RAM, or two branches sharing a mutable resource) would surface
    /// immediately as a branch's console output not matching the exact 4 bytes its own tape suffix
    /// supplied, a stronger, more direct check than a pairwise "outputs don't collide" comparison.
    ///
    /// The branch point is captured immediately after boot, before the guest has executed a single
    /// instruction (`Multiverse::boot` only configures state; nothing runs until `run_to_first_halt`
    /// is called) — the simplest possible branch point, and the same one every branch forks from.
    /// `NUM_BRANCHES` is a real, sized-for-this-host count, not the spec pseudocode's literal
    /// `1000` figure scaled down for its own sake: each branch is a full `restore` (§4's documented
    /// "small-N fallback" cost — a real `KVM_CREATE_VM`/vCPU/guest-RAM-region per branch, unlike the
    /// spec's O(write-set) CoW sharing), so this test's wall-clock cost is `NUM_BRANCHES` real KVM
    /// VM lifecycles; `NUM_BRANCHES` was chosen to keep this test's real run time on this dev
    /// machine in the tens-of-seconds range while still exercising a genuinely large N, not a
    /// token handful.
    ///
    /// `double_run_sample` re-forks a subset of branches a second time from the same universe and
    /// the same suffix to prove per-branch internal determinism (the spec pseudocode's
    /// `b.is_deterministic_double_run()`) — done for a sample rather than all `NUM_BRANCHES` purely
    /// to bound this test's real-hardware run time; determinism itself is not sampled science here,
    /// it is the same `restore` code path every branch already takes, already proven bit-identical
    /// by `snapshot_roundtrip_is_bit_identical` above. The per-branch RAM hash is therefore
    /// computed for exactly those sampled indices too (via
    /// [`Multiverse::run_to_first_halt_without_ram_hash`] plus an explicit
    /// [`Multiverse::ram_hash`]), not for all `NUM_BRANCHES` — a 256 MiB blake3 pass whose result
    /// nothing reads is pure wall clock.
    ///
    /// Being the workspace's only ~`NUM_BRANCHES`-deep run of full VM lifecycles (the next largest
    /// count anywhere is 6), this is also the only place a per-lifecycle *resource* leak is
    /// observable, so it asserts that explicitly rather than waiting for an eventual `Err`/OOM
    /// kill: open-fd count across the loop must be flat (a one-fd-per-branch leak would be
    /// ~`NUM_BRANCHES` extra fds), and RSS growth from a warm baseline must stay under a bound far
    /// below "one leaked 256 MiB guest-RAM region per branch" yet far above any plausible allocator
    /// drift. Both samples are taken between waves, with every worker joined and no branch alive,
    /// so `BRANCH_WORKERS` changes only the *peak* RSS during a wave, not what these two measure.
    ///
    /// `NUM_BRANCHES` real KVM VM lifecycles cost ~67s on this dev machine (measured on a quiet
    /// host; ~143s with `BRANCH_WORKERS = 1`, i.e. before the branches were spread across threads,
    /// and ~237s before that, when the RAM hash was still computed for the ~992 branches that never
    /// read one), and by themselves the entire floor of `cargo test -p baud-multiverse --lib` (the
    /// next-slowest test in the crate is ~2s). `drive/h/h5.sh` already runs this test by name, so
    /// leaving it in the default suite meant paying that floor *twice* per verification round;
    /// `#[ignore]`d here so only `drive/h/h5.sh` invokes it, the same opt-in convention as the
    /// enforced-regime tests (`drive/manual/h3-enforced-rdtsc.sh` etc).
    #[test]
    #[ignore = "1000 real KVM VM lifecycles, ~67s; covered by drive/h/h5.sh, too slow for the default suite"]
    fn thousand_branches_are_independent_and_deterministic() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const WORK_CLOCK_K: u64 = 1;
        const NUM_BRANCHES: usize = 1000;
        const DOUBLE_RUN_SAMPLE: usize = 8;

        // The branch point: captured immediately after boot, before any guest instruction runs.
        let mut boot = Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, vec![], None)
            .expect("boot (branch point) failed");
        let mut page_store = PageStore::new();
        let universe = boot.snapshot(&mut page_store).expect("snapshot at branch point failed");

        let suffix_for = |i: usize| -> Vec<u8> {
            let i = i as u32;
            vec![(i & 0xff) as u8, ((i >> 8) & 0xff) as u8, 0xAA, 0xBB]
        };

        // Only the `DOUBLE_RUN_SAMPLE` indices the double-run loop below re-forks ever read a
        // recorded RAM hash, and `Multiverse::ram_hash` is a blake3 pass over all
        // `layout::GUEST_RAM_SIZE` (256 MiB, ~0.1s here) regardless of how few pages the guest
        // touched — so hashing every branch would spend ~99% of that work on hashes nothing reads.
        // `run_to_first_halt_without_ram_hash` + an explicit `branch.ram_hash()` on exactly the
        // sampled indices is the same observation for a fraction of the wall clock.
        let sample_stride = NUM_BRANCHES / DOUBLE_RUN_SAMPLE;

        // The resource-leak coverage this test uniquely can provide: it is the only place in the
        // workspace that runs ~1008 KVM_CREATE_VM + vCPU + 256 MiB-guest-RAM-region + perf_event
        // create/destroy cycles (the next largest count anywhere is 6). A per-branch leak of even
        // one fd or one RAM region is invisible in any other test but shows up here ~1000x over —
        // so measure both explicitly rather than relying on an eventual Err/OOM.
        let open_fds = || {
            std::fs::read_dir("/proc/self/fd").expect("/proc/self/fd is readable on Linux").count()
        };
        let vm_rss_kib = || -> u64 {
            let status =
                std::fs::read_to_string("/proc/self/status").expect("/proc/self/status is readable on Linux");
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|kib| kib.parse::<u64>().ok())
                .expect("/proc/self/status always reports VmRSS for a running process")
        };

        // A leaked fd per branch would be ~1000 extra fds here, so the slack only has to absorb
        // test-harness noise (a stray pipe/inotify fd), not any real per-branch growth.
        const FD_SLACK: usize = 16;
        // Warm RSS baseline: taken after the first `RSS_WARM_BRANCH` branches rather than before
        // any of them, so one-time allocator arena growth from the first handful of 256 MiB restore
        // cycles (and from the worker threads' own per-thread arenas) has already settled and is
        // not counted as "growth". Both samples are taken between waves, with every worker joined,
        // so neither includes a live guest-RAM region.
        const RSS_WARM_BRANCH: usize = 50;
        // Sized off both ends of the gap it has to sit in. Measured drift over the remaining ~950
        // branches on this dev machine is ~7.2 MiB (8508 KiB -> 15800 KiB, spread of 0.15 MiB over
        // 15+ consecutive runs), which is just `outputs`/`results` growing plus allocator
        // bookkeeping — the 256 MiB guest-RAM regions themselves are mmap'd and munmap'd per
        // branch, so they never accumulate no matter how many are live at once. (It is ~2.2 MiB
        // with `BRANCH_WORKERS = 1`; the extra ~5 MiB is the per-worker-thread allocator arenas, a
        // one-off that does not scale with `NUM_BRANCHES`.) The smallest leak worth
        // catching is a single un-released guest-RAM region, +256 MiB. 128 MiB sits ~18x above the
        // measured drift and below that smallest real leak, so it cannot flake on normal allocator
        // behaviour yet still fails on even one leaked region (let alone one per branch, which
        // would be +250 GiB and an OOM kill long before the loop ends).
        const RSS_GROWTH_LIMIT_KIB: u64 = 128 * 1024; // 128 MiB

        // Branch independence is the very property this test asserts, so the branches are also
        // safe to *run* concurrently — this is `fleet_of_vms_run_in_parallel_without_interference`'s
        // construction (concurrent real VMs on real threads) applied to branches, and it makes the
        // cross-branch-perturbation check strictly stronger: a sequential loop can only catch state
        // leaking through a *reused* resource, whereas concurrent branches would also catch state
        // leaking through a genuinely shared live one. `universe` is shared by plain `&` — its
        // pages are `Arc`-backed `PageRef`s, so `Universe` is `Sync` — and `std::thread::scope`
        // borrows it in place, so no `'static` bound and no `Arc` wrapper is needed. Nothing else
        // crosses a thread boundary: `page_store` is touched only by the pre-loop `snapshot`, and
        // each branch's vm/vcpu fds and its `perf_event` counter are created *and* used on the one
        // worker thread that owns that branch end to end — which is exactly what KVM (vcpu ioctls
        // belong to the thread that created the vCPU) and `perf_event_open` (`pid == 0` attaches
        // the counter to the calling thread) each require.
        //
        // The worker count is sized off RAM, not cores: `baud_snapshot::linux::restore` writes all
        // 65536 pages of the branch's `layout::GUEST_RAM_SIZE` (256 MiB) region — it is a real
        // copy, not COW — so every *concurrently live* branch costs a full 256 MiB of RSS, i.e. N
        // workers cost N x 256 MiB. This host has 8 logical / 4 physical cores and ~5 GiB of
        // available RAM: 4 x 256 MiB = 1 GiB peak, ~20% of available, with one branch per physical
        // core and the SMT siblings left as headroom for the host's own work (and for whatever else
        // `drive/gate.sh` is running alongside h5). Going wider trades a guaranteed, linear RSS cost
        // for very little wall clock — each branch is memory-bandwidth-bound inside `restore` at
        // least as much as it is CPU-bound inside `KVM_RUN`, so the shared memory path saturates
        // well before the logical cores do.
        const BRANCH_WORKERS: usize = 4;

        // One parallel wave of branches: `range`'s indices are dealt round-robin to
        // `BRANCH_WORKERS` scoped threads (so the sampled indices spread evenly over the workers
        // too), each of which runs its branches exactly as the old sequential loop did — same
        // `branch` + `run_to_first_halt_without_ram_hash` + same per-branch assertion, and
        // `ram_hash` for the sampled indices only. Returns `(index, suffix, sampled ram_hash)` per
        // branch, unordered; the caller sorts by index. Every worker joins before this returns, so
        // afterwards no branch is alive anywhere — which is what makes the RSS samples below
        // comparable to the sequential version's.
        let run_branch_wave = |range: std::ops::Range<usize>| -> Vec<(usize, Vec<u8>, Option<String>)> {
            let universe = &universe;
            let suffix_for = &suffix_for;
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..BRANCH_WORKERS)
                    .map(|worker| {
                        let range = range.clone();
                        scope.spawn(move || {
                            range
                                .skip(worker)
                                .step_by(BRANCH_WORKERS)
                                .map(|i| {
                                    let suffix = suffix_for(i);
                                    let mut branch =
                                        Multiverse::branch(universe, suffix.clone(), WORK_CLOCK_K, None)
                                            .unwrap_or_else(|e| panic!("branch {i} failed: {e}"));
                                    let outcome = branch
                                        .run_to_first_halt_without_ram_hash()
                                        .unwrap_or_else(|e| panic!("branch {i} run failed: {e}"));
                                    assert_eq!(
                                        outcome.console_output, suffix,
                                        "branch {i} must echo exactly its own tape suffix {suffix:?}, got \
                                         {:?} — any mismatch means this branch observed another branch's state \
                                         (or stale/shared state), not its own",
                                        outcome.console_output
                                    );
                                    // `branch` has halted, so its RAM is stable: hashing here is
                                    // exactly the `ram_hash` `run_to_first_halt` would have
                                    // returned, just only for the sampled indices.
                                    let ram_hash = (i % sample_stride == 0).then(|| branch.ram_hash());
                                    (i, suffix, ram_hash)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|handle| {
                        handle.join().unwrap_or_else(|_| {
                            panic!("a branch worker thread panicked — its own assertion/panic message is above")
                        })
                    })
                    .collect()
            })
        };

        let fds_before = open_fds();

        // Two waves rather than one, purely so `rss_warm_kib` keeps the meaning it had when the
        // loop was sequential: the first wave is the warm-up (the same `RSS_WARM_BRANCH` branches,
        // now run in parallel), and every one of its workers has joined before the sample is taken,
        // so — exactly as before — the baseline is taken with no branch alive and no guest-RAM
        // region resident. Peak RSS *during* a wave now scales with `BRANCH_WORKERS`, but neither
        // of the two samples is taken during one.
        let mut results = run_branch_wave(0..RSS_WARM_BRANCH);
        let rss_warm_kib = vm_rss_kib();
        results.extend(run_branch_wave(RSS_WARM_BRANCH..NUM_BRANCHES));

        let fds_after = open_fds();
        let rss_after_kib = vm_rss_kib();

        // Back to branch order, so `outputs[i]` is branch `i`'s — the indices are exactly
        // `0..NUM_BRANCHES` (each dealt to exactly one worker), so this is a total order with no
        // gaps, which the length assertion pins.
        assert_eq!(
            results.len(),
            NUM_BRANCHES,
            "every branch index must come back from exactly one worker thread"
        );
        results.sort_unstable_by_key(|(i, _, _)| *i);
        let outputs: Vec<(Vec<u8>, Option<String>)> =
            results.into_iter().map(|(_, suffix, ram_hash)| (suffix, ram_hash)).collect();
        eprintln!(
            "thousand_branches_are_independent_and_deterministic: fds {fds_before} -> {fds_after}, \
             VmRSS {rss_warm_kib} KiB (after branch {RSS_WARM_BRANCH}) -> {rss_after_kib} KiB"
        );
        assert!(
            fds_after <= fds_before + FD_SLACK,
            "{NUM_BRANCHES} branch lifecycles leaked file descriptors: {fds_before} open before the \
             loop, {fds_after} after (allowed slack {FD_SLACK}) — each branch opens a vm fd, a vcpu \
             fd, a guest-RAM memfd and a perf_event fd, so a per-branch leak of even one shows up \
             as ~{NUM_BRANCHES} extra fds here"
        );
        assert!(
            rss_after_kib <= rss_warm_kib + RSS_GROWTH_LIMIT_KIB,
            "{NUM_BRANCHES} branch lifecycles grew RSS from {rss_warm_kib} KiB (warm baseline, after \
             branch {RSS_WARM_BRANCH}) to {rss_after_kib} KiB, past the {RSS_GROWTH_LIMIT_KIB} KiB \
             bound — each branch maps a fresh {} MiB guest-RAM region, so unbounded growth here \
             means those regions are not being released",
            layout::GUEST_RAM_SIZE / (1024 * 1024)
        );

        // Every branch's output is pinned to its own unique suffix by construction (asserted
        // above), so distinct suffixes trivially mean distinct expected outputs — this is an
        // explicit restatement of "no branch perturbs another" (the spec pseudocode's
        // `no_branch_perturbs_another`) rather than a new check.
        let unique_suffixes: std::collections::HashSet<_> = outputs.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(unique_suffixes.len(), NUM_BRANCHES, "every branch's tape suffix must be unique by construction");

        // A sample of branches, re-forked from the same universe with the same suffix, must be
        // internally deterministic (the spec pseudocode's `b.is_deterministic_double_run()`).
        for i in (0..NUM_BRANCHES).step_by(sample_stride) {
            let suffix = suffix_for(i);
            let mut replay = Multiverse::branch(&universe, suffix.clone(), WORK_CLOCK_K, None)
                .unwrap_or_else(|e| panic!("branch {i} replay failed: {e}"));
            let replay_outcome =
                replay.run_to_first_halt().unwrap_or_else(|e| panic!("branch {i} replay run failed: {e}"));
            let (_, first_ram_hash) = &outputs[i];
            // `sample_stride` selects exactly the indices the loop above hashed, so this is
            // infallible by construction — the `expect` only pins that invariant in place.
            let first_ram_hash = first_ram_hash
                .as_ref()
                .unwrap_or_else(|| panic!("branch {i} is a sampled index, so its RAM hash was recorded"));
            assert_eq!(
                replay_outcome.console_output, suffix,
                "branch {i} replayed from the same universe+suffix must produce the same output"
            );
            assert_eq!(
                &replay_outcome.ram_hash, first_ram_hash,
                "branch {i} replayed from the same universe+suffix must produce byte-identical \
                 guest RAM — a double-run divergence here would mean this branch is not actually \
                 deterministic"
            );
        }
    }

    /// H6 (todo.md §10) — "many single-vCPU VMs pinned across cores explore in parallel on one
    /// host": closes all three of H6's milestone bullets against real KVM hardware in one test —
    /// aggregate throughput (running N VMs concurrently is meaningfully faster than running them
    /// one at a time), `capacity_refuses_sibling_split` (this time against the *real* probed
    /// topology, not `baud-host`'s own fake-topology unit test), and "no cross-VM interference"
    /// (each VM's own tape pins its own expected output, so any VM observing another's state would
    /// surface as a wrong-output assertion failure — the same construction H5's
    /// `thousand_branches_are_independent_and_deterministic` already uses for branches, applied
    /// here across genuinely concurrent OS threads pinned to distinct physical cores instead of
    /// sequential `restore` calls). Also gives [`baud_vcpu::linux::pin_thread_to_core`] its first
    /// real call site in the workspace (todo.md §14: written for spec compliance, zero callers
    /// until this test).
    ///
    /// Load-sensitive by construction, and so `#[ignore]`d: it pins its threads to *fixed* logical
    /// cores (0/2/4/...) and its throughput check is a timing *ratio* against a serial baseline
    /// measured moments earlier — both of which assume the machine is otherwise quiet. Inside
    /// `cargo test --workspace` it instead runs alongside up to 7 sibling KVM tests on this
    /// 8-thread host, which contend for exactly those cores and stretch `parallel_total` for
    /// reasons that have nothing to do with the concurrency being tested; that is the single
    /// largest source of recorded flakes in `ralph/progress.txt` (19). `drive/h/h6.sh` is its sole
    /// runner (`--include-ignored`), where it is the only KVM workload on the machine — the same
    /// opt-in convention `thousand_branches_are_independent_and_deterministic` and the
    /// enforced-regime tests already use.
    #[test]
    #[ignore = "timing-ratio + fixed-core pinning, flaky under any concurrent load; covered by drive/h/h6.sh on a quiet machine"]
    fn fleet_of_vms_run_in_parallel_without_interference() {
        let host = baud_host::Host::probe();
        assert!(
            host.is_runnable(),
            "this test needs a real KVM-capable host; reason: {:?}",
            host.reason
        );

        // `capacity_refuses_sibling_split` (specs/baud-host.md §6), exercised here against this
        // real host's own probed topology rather than baud-host's own fake-topology unit test.
        assert!(
            host.place(host.capacity() + 1).is_err(),
            "placing one VM over real capacity must be refused"
        );
        let full = host.place(host.capacity()).expect("placing at capacity must succeed");
        assert!(full.no_two_on_sibling_threads(), "no two placed VMs may share an SMT sibling pair");

        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let n = host.capacity().clamp(1, 4);

        let suffix_for = |i: usize| -> Vec<u8> {
            let i = i as u32;
            vec![(i & 0xff) as u8, ((i >> 8) & 0xff) as u8, 0xCC, 0xDD]
        };
        let tapes: Vec<Vec<u8>> = (0..n).map(suffix_for).collect();

        // Serial baseline: one VM, run alone, timed the same way each fleet VM is timed.
        let mut baseline = Multiverse::boot(&kernel, cmdline, 0, 1, tapes[0].clone(), None)
            .expect("baseline boot failed");
        let serial_start = std::time::Instant::now();
        baseline.run_to_first_halt().expect("baseline run failed");
        let serial_one = serial_start.elapsed();

        let parallel_start = std::time::Instant::now();
        let results = run_fleet(&host, &kernel, cmdline, tapes.clone()).expect("fleet run failed");
        let parallel_total = parallel_start.elapsed();
        eprintln!(
            "fleet_of_vms_run_in_parallel_without_interference: n={n} serial_one={serial_one:?} \
             parallel_total={parallel_total:?}"
        );

        assert_eq!(results.len(), n);
        let mut seen_cores = std::collections::HashSet::new();
        for (i, result) in results.iter().enumerate() {
            assert_eq!(
                result.outcome.console_output, tapes[i],
                "VM {i} (core {}) must echo exactly its own tape suffix {:?}, got {:?} — a \
                 mismatch means it observed a different VM's state",
                result.core_id, tapes[i], result.outcome.console_output
            );
            assert!(
                seen_cores.insert(result.core_id),
                "two VMs landed on the same core {}",
                result.core_id
            );
        }

        if n >= 2 {
            // Real parallel execution: N VMs concurrently must not cost anywhere near N times the
            // serial baseline — a generous 0.85 factor leaves ample margin for host jitter on this
            // dev machine's contended nested-virt host (todo.md §14 documents similar jitter for
            // other timing-sensitive checks here) while still failing if the fleet secretly ran
            // the VMs one at a time.
            let serial_n_estimate = serial_one * n as u32;
            assert!(
                parallel_total < serial_n_estimate.mul_f64(0.85),
                "fleet of {n} VMs took {parallel_total:?}, not meaningfully faster than the \
                 {serial_n_estimate:?} estimated serial cost of one VM ({serial_one:?}) times {n} \
                 — real concurrency is not happening"
            );
        }
    }

    /// `tests/fixtures/shell-guest/`'s payload: prints a `$ ` prompt, then polls COM1 for input,
    /// echoing every byte except `\r` (carriage return, which prints a newline and re-prints the
    /// prompt) — never `hlt`s, so it is driven with [`Multiverse::step_exit`]/
    /// [`Multiverse::run_until_console_len`], not [`Multiverse::run_to_first_halt`]. See that
    /// directory's `BUILD.md` for exact provenance and the LSR-polling-not-IRQ4 rationale.
    fn shell_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shell-guest/bzImage")
    }

    /// The exact prompt `tests/fixtures/shell-guest/payload.s` prints before reading anything.
    const SHELL_GUEST_PROMPT: &[u8] = b"$ ";

    /// specs/baud-snapshot.md §5's "restore into a live shell" and its named test
    /// `shell_into_universe_resumes` (todo.md §10/§14's H5 gap: "Console today wraps a fixed,
    /// non-generic `Serial<..., Vec<u8>>` with no ... input ... path at all"). Proves the two
    /// halves of "resumes" the spec cares about: (1) a universe captured mid-interaction restores
    /// with its console history intact — `restored.console_output()` matches the captured tail
    /// exactly, the strong form of "first output byte matches the captured console tail"; (2) the
    /// restored session is not a frozen replay — it keeps taking live input
    /// ([`Multiverse::enqueue_console_input`]) and producing live output
    /// ([`Multiverse::step_exit`]/[`Multiverse::run_until_console_len`]), and does so
    /// byte-identically to an equivalent straight run that never snapshotted at all, the same
    /// bit-identical framing every other H5 test uses.
    #[test]
    fn shell_into_universe_resumes() {
        let kernel = shell_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const WORK_CLOCK_K: u64 = 1;
        const PROMPT_EXITS: u32 = 16;
        const INTERACTION_EXITS: u32 = 100;
        let after_interaction_len = SHELL_GUEST_PROMPT.len() + b"hi\n$ ".len();

        // Capture-then-restore run: reach the first prompt, capture a Universe there, restore into
        // a brand-new Multiverse, and interact only on the restored instance.
        let mut boot = Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, vec![], None)
            .expect("boot (branch point) failed");
        boot.run_until_console_len(SHELL_GUEST_PROMPT.len(), PROMPT_EXITS)
            .expect("guest must print its prompt within a handful of exits");
        assert_eq!(boot.console_output(), SHELL_GUEST_PROMPT);

        let mut page_store = PageStore::new();
        let universe = boot.snapshot(&mut page_store).expect("snapshot at the prompt failed");
        assert_eq!(
            universe.device.console, SHELL_GUEST_PROMPT,
            "captured universe's console history must be exactly the prompt observed pre-capture"
        );

        let mut restored = Multiverse::restore(&universe, vec![], WORK_CLOCK_K, false, None)
            .expect("restore from captured universe failed");
        assert_eq!(
            restored.console_output(),
            universe.device.console.as_slice(),
            "a freshly restored Multiverse's console output must match the captured tail exactly, \
             before any further interaction — the guarantee shell_into_universe_resumes names as \
             \"first output byte matches the captured console tail\""
        );

        let queued = restored.enqueue_console_input(b"hi\r");
        assert_eq!(queued, 3, "all 3 input bytes must fit the UART's RX FIFO");
        restored
            .run_until_console_len(after_interaction_len, INTERACTION_EXITS)
            .expect("restored guest must echo the queued input and re-prompt");
        assert_eq!(restored.console_output(), b"$ hi\n$ ");

        // Straight run: the same image, never snapshotted, driven through the identical
        // interaction — proves the restored session's behavior is genuinely a resumption, not an
        // artifact of the restore path itself.
        let mut straight = Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, vec![], None)
            .expect("straight boot failed");
        straight
            .run_until_console_len(SHELL_GUEST_PROMPT.len(), PROMPT_EXITS)
            .expect("straight run must print its prompt within a handful of exits");
        straight.enqueue_console_input(b"hi\r");
        straight
            .run_until_console_len(after_interaction_len, INTERACTION_EXITS)
            .expect("straight run must echo the queued input and re-prompt");

        assert_eq!(
            restored.console_output(),
            straight.console_output(),
            "a restored universe's interactive session must be byte-identical to an equivalent \
             straight run that never snapshotted at all"
        );
    }

    /// `tests/fixtures/mark-branch-guest/`'s payload: loops 4 times reading one tape byte,
    /// echoing it to COM1, then issuing `MARK_BRANCH` — the first fixture in this workspace that
    /// keeps running after a branch point instead of halting on tape exhaustion (see that
    /// directory's `BUILD.md`).
    fn mark_branch_guest_kernel_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mark-branch-guest/bzImage")
    }

    /// [`Multiverse::run_until_branch_or_halt`]'s basic contract: it stops before `Hlt`, at the
    /// first `MARK_BRANCH`, reports the tape cursor at that moment (`mark-branch-guest` marks
    /// after every byte it reads, so the first marker's `step` is exactly `1`), and the drained
    /// records contain that `MarkBranch` and nothing else yet (no `Probe`/`Goal`/`Violation`/`Log`
    /// this fixture never emits).
    #[test]
    fn run_until_branch_or_halt_stops_at_first_mark_branch() {
        let kernel = mark_branch_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const MAX_EXITS: u32 = 16;

        let mut boot = Multiverse::boot(&kernel, cmdline, 0, 1, vec![1, 2, 3, 4], None).expect("boot failed");
        let (outcome, records) = boot.run_until_branch_or_halt(MAX_EXITS).expect("run_until_branch_or_halt failed");

        match outcome {
            RunUntilBranchOutcome::MarkBranch { step } => assert_eq!(step, 1, "the first MARK_BRANCH must land right after the first tape byte is consumed"),
            RunUntilBranchOutcome::Halted(_) => panic!("must stop at MARK_BRANCH, not run all the way to Hlt"),
        }
        assert_eq!(records.len(), 1, "exactly one record (the MarkBranch itself) must have been drained");
        assert!(matches!(records[0], baud_proto::Msg::MarkBranch { step: 1 }));
        assert_eq!(
            boot.console_output(),
            &[1],
            "the guest must have echoed exactly the one byte it read before marking the branch"
        );
    }

    /// The real architectural proof todo.md's "M-series sixth brick" entry flagged as missing:
    /// forking a universe captured at a `MARK_BRANCH` checkpoint with a *new* tape suffix must
    /// genuinely change the guest's subsequent output, not silently replay the original branch's
    /// frozen continuation (the no-op that entry proved live against every older, halt-only
    /// fixture). Captures the checkpoint once, forks it twice on two different suffixes, and
    /// proves: (1) each fork's final output is exactly its own prefix-so-far plus its own new
    /// suffix, (2) the two forks' outputs genuinely differ from each other, and (3) forking with
    /// the checkpoint's *original* continuation bytes reproduces the original straight run exactly
    /// — so this is a real fork of live, still-running state, not a coincidence of construction.
    #[test]
    fn branch_from_mark_branch_checkpoint_diverges_on_new_tape_suffix() {
        let kernel = mark_branch_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const WORK_CLOCK_K: u64 = 1;
        const MAX_EXITS: u32 = 16;
        let original_tape = vec![1u8, 2, 3, 4];

        // A straight, never-forked run of the same image+tape, to compare the "same suffix"
        // fork against below.
        let mut straight = Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, original_tape.clone(), None)
            .expect("straight boot failed");
        let straight_outcome = straight.run_to_first_halt().expect("straight run failed");
        assert_eq!(straight_outcome.console_output, original_tape);

        // The checkpoint: run the same image+tape only up to the first MARK_BRANCH, then snapshot.
        let mut boot = Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, original_tape.clone(), None)
            .expect("checkpoint boot failed");
        let (outcome, _records) = boot.run_until_branch_or_halt(MAX_EXITS).expect("run_until_branch_or_halt failed");
        let step = match outcome {
            RunUntilBranchOutcome::MarkBranch { step } => step,
            RunUntilBranchOutcome::Halted(_) => panic!("must stop at MARK_BRANCH"),
        };
        let mut page_store = PageStore::new();
        let universe = boot.snapshot(&mut page_store).expect("snapshot at checkpoint failed");
        assert_eq!(universe.device.tape_cursor, step, "the captured tape cursor must match the marker's own step");

        let padded_tape = |suffix: &[u8]| -> Vec<u8> {
            let mut tape = vec![0u8; step as usize];
            tape.extend_from_slice(suffix);
            tape
        };

        // Fork A: a suffix that genuinely differs from the checkpoint's own original continuation.
        let suffix_a = vec![9u8, 8, 7];
        let mut fork_a = Multiverse::branch(&universe, padded_tape(&suffix_a), WORK_CLOCK_K, None)
            .expect("fork A failed");
        let outcome_a = fork_a.run_to_first_halt().expect("fork A run failed");
        let mut expected_a = vec![1u8];
        expected_a.extend_from_slice(&suffix_a);
        assert_eq!(
            outcome_a.console_output, expected_a,
            "fork A must echo its checkpoint prefix plus its OWN new suffix, not the original tape's"
        );

        // Fork B: a different new suffix again.
        let suffix_b = vec![42u8, 43, 44];
        let mut fork_b = Multiverse::branch(&universe, padded_tape(&suffix_b), WORK_CLOCK_K, None)
            .expect("fork B failed");
        let outcome_b = fork_b.run_to_first_halt().expect("fork B run failed");
        let mut expected_b = vec![1u8];
        expected_b.extend_from_slice(&suffix_b);
        assert_eq!(outcome_b.console_output, expected_b);

        assert_ne!(
            outcome_a.console_output, outcome_b.console_output,
            "two forks of the same checkpoint on two different suffixes must genuinely diverge — \
             the exact property the pre-existing no-op finding disproved for halt-only fixtures"
        );

        // Fork C: handed back its own original continuation bytes must reproduce the straight
        // run's tail exactly, proving the checkpoint really is this run's own live state.
        let original_suffix = &original_tape[step as usize..];
        let mut fork_c = Multiverse::branch(&universe, padded_tape(original_suffix), WORK_CLOCK_K, None)
            .expect("fork C failed");
        let outcome_c = fork_c.run_to_first_halt().expect("fork C run failed");
        assert_eq!(
            outcome_c.console_output, straight_outcome.console_output,
            "forking the checkpoint with its own original continuation must reproduce the \
             straight run's output exactly"
        );
        assert_eq!(outcome_c.ram_hash, straight_outcome.ram_hash);
    }

    /// Chains two `MARK_BRANCH` checkpoints: reach the first marker, fork it onto fresh input and
    /// run only to the *second* marker (not to halt), snapshot again, then fork that second
    /// checkpoint onto yet another fresh suffix and run to completion. Proves the primitive
    /// composes to real, unbounded-depth tree growth (bounded only by how many times a fixture
    /// itself calls `MARK_BRANCH`, not by any depth limit in `Multiverse` or the tape-device
    /// protocol) — todo.md's "M-series sixth brick" entry's literal blocker, closed for real.
    #[test]
    fn two_level_mark_branch_checkpoints_chain() {
        let kernel = mark_branch_guest_kernel_path();
        let cmdline = "console=ttyS0";
        const WORK_CLOCK_K: u64 = 1;
        const MAX_EXITS: u32 = 16;

        let mut boot = Multiverse::boot(&kernel, cmdline, 0, WORK_CLOCK_K, vec![10u8], None).expect("boot failed");
        let (outcome1, _) = boot.run_until_branch_or_halt(MAX_EXITS).expect("first run_until_branch_or_halt failed");
        let step1 = match outcome1 {
            RunUntilBranchOutcome::MarkBranch { step } => step,
            RunUntilBranchOutcome::Halted(_) => panic!("must stop at the first MARK_BRANCH"),
        };
        assert_eq!(step1, 1);
        let mut page_store = PageStore::new();
        let checkpoint1 = boot.snapshot(&mut page_store).expect("snapshot at first checkpoint failed");

        // Fork checkpoint 1 onto fresh input, but only run to the SECOND marker, not to halt.
        let second_byte = 21u8;
        let mut fork1 = Multiverse::branch(&checkpoint1, vec![0u8, second_byte], WORK_CLOCK_K, None)
            .expect("fork of checkpoint 1 failed");
        let (outcome2, _) = fork1.run_until_branch_or_halt(MAX_EXITS).expect("second run_until_branch_or_halt failed");
        let step2 = match outcome2 {
            RunUntilBranchOutcome::MarkBranch { step } => step,
            RunUntilBranchOutcome::Halted(_) => panic!("must stop at the second MARK_BRANCH"),
        };
        assert_eq!(step2, 2, "the second marker's step must be the cursor after the second byte is read");
        assert_eq!(fork1.console_output(), &[10u8, second_byte]);
        let checkpoint2 = fork1.snapshot(&mut page_store).expect("snapshot at second checkpoint failed");

        // Fork checkpoint 2 onto yet another fresh suffix and finish the fixture's remaining
        // two loop iterations to Hlt.
        let tail = vec![77u8, 88];
        let mut padded = vec![0u8; step2 as usize];
        padded.extend_from_slice(&tail);
        let mut fork2 = Multiverse::branch(&checkpoint2, padded, WORK_CLOCK_K, None)
            .expect("fork of checkpoint 2 failed");
        let outcome3 = fork2.run_to_first_halt().expect("final run to halt failed");

        let mut expected = vec![10u8, second_byte];
        expected.extend_from_slice(&tail);
        assert_eq!(
            outcome3.console_output, expected,
            "the fully-chained run must reflect every level's own fresh input, in order: the \
             root's byte, checkpoint 1's fork's byte, and checkpoint 2's fork's two bytes"
        );
    }
}

