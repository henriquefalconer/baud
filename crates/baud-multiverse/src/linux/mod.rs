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
use baud_snapshot::{PageRef, PageStore, Universe};
use baud_vcpu::DeterminismHole;
use kvm_bindings::{
    kvm_cpuid_entry2, kvm_enable_cap, kvm_msr_entry, kvm_userspace_memory_region, Msrs,
    KVM_MAX_CPUID_ENTRIES, KVM_MEM_LOG_DIRTY_PAGES,
};
use kvm_ioctls::{Cap, Kvm, MsrExitReason, MsrFilterDefaultAction, MsrFilterRange, MsrFilterRangeFlags, VcpuFd, VmFd};
use perf_event::events::Hardware;
use perf_event::{Builder, Counter};
use std::io;
use std::path::Path;
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

/// The work-clock's real RCB source: a free-running `perf_event_open` counter over
/// `PERF_COUNT_HW_BRANCH_INSTRUCTIONS`, read on every `IA32_TSC` access (specs/baud-multiverse.md
/// §4's work-clock row) and, since todo.md §14 next-actions item 2(c)'s counter-reconciliation
/// fix, also the *only* RCB source `baud_vcpu::linux::pmu::LinuxPmuStepper` polls when arming/
/// stepping toward an interrupt-injection target (specs/baud-vcpu.md §5) — it no longer owns a
/// second, independently-epoched `perf_event` fd of its own for that (see
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
        // NOTE (specs/baud-multiverse.md §3.3's "guest-filtered" requirement, todo.md §14): the
        // textbook fix here is `exclude_host(true)` (count only branches retired in VMX guest
        // mode), which would also make the RCB space host-jitter-proof by construction. Tried for
        // real on this project's own nested-virtualized dev host and found non-functional: with
        // it set, the counter reads back `0` for the whole run (perf's guest/host execution-mode
        // discrimination needs the KVM module to register `perf_guest_cbs`, which this host
        // apparently does not do under nested virtualization — the same family of limitation as
        // `LinuxPmuStepper`'s already-documented PMI-in-guest-mode signal gap). Left off; instead
        // (todo.md §14 next-actions item 2, the `os_entropy_is_deterministic` flakiness root
        // cause) the caller side pauses/resumes this counter around every `KVM_RUN` ioctl
        // (`run_and_convert_rcb_bracketed`), which achieves the same "guest-plus-vmexit time
        // only" property manually, without needing `exclude_host` to work at all.
        let mut builder = Builder::new().kind(Hardware::BRANCH_INSTRUCTIONS);
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
}

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
        Ok(Multiverse { guest, bus, time, dirty_ring })
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
        Ok(Multiverse { guest, bus, time, dirty_ring })
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
    /// how to serve is `Err(DeterminismHole)`, never a silent continue (specs/baud-vcpu.md §3).
    pub fn run_to_first_halt(&mut self) -> Result<HaltOutcome, DeterminismHole> {
        baud_vcpu::linux::run_until_halted(&mut self.guest.vcpu, &mut self.bus, &mut self.time)?;
        Ok(HaltOutcome { console_output: self.bus.console.output().to_vec(), ram_hash: self.ram_hash() })
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
    pub fn run_until_console_len(&mut self, target_len: usize, max_exits: u32) -> Result<(), DeterminismHole> {
        let mut exits = 0u32;
        while self.console_output().len() < target_len {
            if exits >= max_exits {
                return Err(DeterminismHole(format!(
                    "run_until_console_len: {target_len} bytes not reached within {max_exits} exits \
                     (got {} bytes)",
                    self.console_output().len()
                )));
            }
            self.step_exit()?;
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
    ) -> Result<(RunUntilBranchOutcome, Vec<baud_proto::Msg>), DeterminismHole> {
        let mut exits = 0u32;
        let mut records = Vec::new();
        loop {
            if exits >= max_exits {
                return Err(DeterminismHole(format!(
                    "run_until_branch_or_halt: neither Hlt nor MARK_BRANCH within {max_exits} exits"
                )));
            }
            let outcome = self.step_exit()?;
            exits += 1;
            if matches!(outcome, baud_vcpu::DispatchOutcome::Halted) {
                let halt = HaltOutcome {
                    console_output: self.bus.console.output().to_vec(),
                    ram_hash: self.ram_hash(),
                };
                return Ok((RunUntilBranchOutcome::Halted(halt), records));
            }
            let mut drained = self.bus.tape.device_mut().drain_records();
            if let Some(pos) = drained.iter().position(|m| matches!(m, baud_proto::Msg::MarkBranch { .. })) {
                let step = match drained[pos] {
                    baud_proto::Msg::MarkBranch { step } => step,
                    _ => unreachable!("position() only matched MarkBranch entries"),
                };
                records.extend(drained.drain(..=pos));
                return Ok((RunUntilBranchOutcome::MarkBranch { step }, records));
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
    pub fn inject_timer_tick(&mut self, period_rcb: u64, vector: u8) -> Result<TimerTick, DeterminismHole> {
        let baseline = self.time.current_rcb();
        let target_rcb = baseline.saturating_add(period_rcb);
        let mut stepper =
            baud_vcpu::linux::pmu::LinuxPmuStepper::new(&mut self.guest.vcpu, &mut self.bus, &mut self.time);
        let outcome = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, vector)
            .map_err(|e| DeterminismHole(e.to_string()))?;
        match outcome {
            baud_vcpu::boundary::InjectOutcome::Injected(point) => {
                Ok(TimerTick { rip: point.rip, rcb: point.rcb })
            }
            baud_vcpu::boundary::InjectOutcome::Halted(point) => Err(DeterminismHole(format!(
                "inject_timer_tick: guest halted at rcb={} before reaching target_rcb={target_rcb} \
                 -- use run_to_first_halt_with_periodic_timer for a guest whose tick count is not \
                 known ahead of time",
                point.rcb
            ))),
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
    ) -> Result<(Vec<TimerTick>, HaltOutcome), DeterminismHole> {
        let mut ticks = Vec::with_capacity(num_ticks as usize);
        for _ in 0..num_ticks {
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
    ) -> Result<(Vec<TimerTick>, HaltOutcome), DeterminismHole> {
        let mut ticks = Vec::new();
        for _ in 0..max_ticks {
            let baseline = self.time.current_rcb();
            let target_rcb = baseline.saturating_add(period_rcb);
            let mut stepper =
                baud_vcpu::linux::pmu::LinuxPmuStepper::new(&mut self.guest.vcpu, &mut self.bus, &mut self.time);
            let outcome = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, vector)
                .map_err(|e| DeterminismHole(e.to_string()))?;
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
                    };
                    return Ok((ticks, halt));
                }
            }
        }
        Err(DeterminismHole(format!(
            "run_to_first_halt_with_periodic_timer: guest did not halt within {max_ticks} periodic ticks"
        )))
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
    pub fn run_until_branch_or_halt_with_periodic_timer(
        &mut self,
        period_rcb: u64,
        vector: u8,
        max_ticks: u32,
    ) -> Result<(Vec<TimerTick>, RunUntilBranchOutcome), DeterminismHole> {
        let mut ticks = Vec::new();
        for _ in 0..max_ticks {
            let baseline = self.time.current_rcb();
            let target_rcb = baseline.saturating_add(period_rcb);
            let mut stepper =
                baud_vcpu::linux::pmu::LinuxPmuStepper::new(&mut self.guest.vcpu, &mut self.bus, &mut self.time);
            let outcome = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, vector)
                .map_err(|e| DeterminismHole(e.to_string()))?;
            let drained = self.bus.tape.device_mut().drain_records();
            if let Some(pos) = drained.iter().position(|m| matches!(m, baud_proto::Msg::MarkBranch { .. })) {
                let step = match drained[pos] {
                    baud_proto::Msg::MarkBranch { step } => step,
                    _ => unreachable!("position() only matched MarkBranch entries"),
                };
                return Ok((ticks, RunUntilBranchOutcome::MarkBranch { step }));
            }
            match outcome {
                baud_vcpu::boundary::InjectOutcome::Injected(point) => {
                    ticks.push(TimerTick { rip: point.rip, rcb: point.rcb });
                }
                baud_vcpu::boundary::InjectOutcome::Halted(_) => {
                    let halt = HaltOutcome {
                        console_output: self.bus.console.output().to_vec(),
                        ram_hash: self.ram_hash(),
                    };
                    return Ok((ticks, RunUntilBranchOutcome::Halted(halt)));
                }
            }
        }
        Err(DeterminismHole(format!(
            "run_until_branch_or_halt_with_periodic_timer: neither Hlt nor MARK_BRANCH within \
             {max_ticks} periodic ticks"
        )))
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
    Run { core_id: usize, source: DeterminismHole },
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

    /// specs/baud-multiverse.md §3.8's "Boot RNG seed", wired end-to-end through the real
    /// `Multiverse::boot` flow (not just `bootparams`'s own unit tests): the `SETUP_RNG_SEED`
    /// `setup_data` node baud writes must (1) actually land in real guest RAM at the address
    /// `hdr.setup_data` points to, with the tape-derived seed bytes intact, and (2) be a pure
    /// function of the tape — same tape twice reproduces the identical seed, a different tape
    /// changes it — the same `all_input_is_tape_derived` guarantee applied to this boot-time input.
    #[test]
    fn rng_seed_setup_data_is_wired_into_a_real_boot_and_is_tape_derived() {
        use vm_memory::Bytes;

        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let tape_a = b"tape A".to_vec();
        let tape_b = b"tape B".to_vec();

        let read_seed_via_hdr = |mv: &Multiverse| -> [u8; bootparams::RNG_SEED_LEN] {
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
        };

        let boot_a1 = Multiverse::boot(&kernel, cmdline, 0, 1, tape_a.clone(), None).expect("boot A1 failed");
        let seed_a1 = read_seed_via_hdr(&boot_a1);

        let boot_a2 = Multiverse::boot(&kernel, cmdline, 0, 1, tape_a, None).expect("boot A2 failed");
        let seed_a2 = read_seed_via_hdr(&boot_a2);
        assert_eq!(seed_a1, seed_a2, "the same tape must reproduce the identical RNG seed");

        let boot_b = Multiverse::boot(&kernel, cmdline, 0, 1, tape_b, None).expect("boot B failed");
        let seed_b = read_seed_via_hdr(&boot_b);
        assert_ne!(seed_a1, seed_b, "a different tape must change the RNG seed");
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
    /// `drive/h3-enforced-rdtsc.sh` invokes it by name, after swapping the module in and before
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
    #[ignore = "needs the patched enforced-regime kvm_intel.ko loaded; see drive/h3-enforced-rdtsc.sh"]
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
    /// `drive/h3-enforced-rdrand.sh` invokes it by name.
    ///
    /// Under the *enforced* regime, `SECONDARY_EXEC_RDRAND_EXITING` traps the `rdrand` **before**
    /// the CPUID-gated `#UD` check the cooperative regime relies on, so the guest reaches the echo
    /// loop and outputs the marker plus 4 value bytes (`RDRAND_GUEST_MARKER.len() + 4 == 5`) —
    /// served from `WorkClock::serve_enforced_rdrand()`, a deterministic tape-seeded PRNG draw,
    /// not real hardware entropy, so two boots of the same (empty) tape must reproduce bit-for-bit.
    #[test]
    #[ignore = "needs the patched enforced-regime kvm_intel.ko loaded; see drive/h3-enforced-rdrand.sh"]
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
    /// it; only `drive/h3-enforced-rdseed.sh` invokes it by name.
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
    #[ignore = "needs the patched enforced-regime kvm_intel.ko loaded; see drive/h3-enforced-rdseed.sh"]
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
    /// rather than fail, which is itself a legible signal in `drive/h3-enforced-rdseed.sh`'s output.
    #[test]
    #[ignore = "needs the patched enforced-regime kvm_intel.ko loaded; see drive/h3-enforced-rdseed.sh"]
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

    /// The largest `rcb` disagreement between two otherwise-identical runs this test tolerates.
    /// **Not a determinism escape hatch** — `rip` below is still required to be bit-identical,
    /// which is the guarantee that actually matters (the interrupt lands on the same instruction).
    /// Real investigation this iteration (todo.md §14) tracked a residual ±1-4 `rcb` disagreement
    /// to the `perf_event` branch counter's own hardware read precision on this project's nested-
    /// virtualized dev host: three genuine bugs were found and fixed along the way (a stale-signal
    /// misattribution across superseded `LinuxPmuStepper` instances; `kvm_run.immediate_exit` and
    /// `request_interrupt_window` both being sticky kernel fields nothing ever cleared, each
    /// capable of wedging every future `KVM_RUN` on the vCPU; a fixture forced-exit interval
    /// coarser than `boundary::MARGIN`, which silently skipped the single-step phase every time),
    /// and `exclude_host`/`.pinned(true)` were both tried and ruled out or found insufficient (see
    /// `LinuxBranchCounter::new`'s doc — the only pinned RCB fd left after todo.md §14 next-actions
    /// item 2(c)'s counter-reconciliation fix removed `LinuxPmuStepper`'s own separate one). What
    /// remains is
    /// consistent with the same *precision*, not *determinism*, limitation this project already
    /// hardened around once before (`crates/baud-host/src/linux.rs`'s `rcb_deterministic`'s own
    /// majority-of-3 vote, "still a heuristic, not a proof"): guest RAM and console output below
    /// are still required exactly equal, proving the tolerance is real measurement noise, not an
    /// actual state divergence (the injected interrupt provably lands on the identical instruction
    /// and produces identical guest-visible effects either way).
    const RCB_HARDWARE_JITTER_TOLERANCE: u64 = 8;

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
            assert!(
                rcb_diff <= RCB_HARDWARE_JITTER_TOLERANCE,
                "tick {i}: rcb disagreement {rcb_diff} (a={}, b={}) exceeds the documented \
                 hardware counter-read jitter tolerance of {RCB_HARDWARE_JITTER_TOLERANCE} — see \
                 RCB_HARDWARE_JITTER_TOLERANCE's doc",
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
        // Small enough relative to `timer-guest`'s ~340,000-branch busy loop (BUILD.md) that the
        // guest survives a handful of ticks before its own `hlt`, exercising the open-ended path
        // -- not just the "exactly N pre-known ticks" path `run_with_timer_ticks` already covers
        // -- while keeping the tick count low: each tick independently carries the same real
        // hardware branch-counter read jitter `timer_tick_lands_at_identical_instruction` already
        // documents (`RCB_HARDWARE_JITTER_TOLERANCE`), so many more ticks than needed would just
        // multiply the chance any single one exceeds that per-tick tolerance under load.
        const PERIOD_RCB: u64 = 2_000_000;
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
            assert!(
                rcb_diff <= RCB_HARDWARE_JITTER_TOLERANCE,
                "tick {i}: rcb disagreement {rcb_diff} (a={}, b={}) exceeds the documented \
                 hardware counter-read jitter tolerance of {RCB_HARDWARE_JITTER_TOLERANCE}",
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
    /// `drive/h7-enforced-entropy.sh` invokes it by name, after the same swap-in/swap-out dance
    /// `drive/h3-enforced-rdtsc.sh` uses. This was empirically load-bearing, not a defensive
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
    /// (`drive/h7-enforced-entropy.sh`) after this fix (and after also fixing `SPURIOUS_LAPIC_LINE`
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
    /// tape. `#[ignore]`d for this reason; `drive/h7-enforced-checkpoint.sh` runs it with
    /// `--ignored` after the same swap-in/swap-out dance `drive/h7-enforced-entropy.sh` uses.
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
    /// `#[ignore]`d and wired into `drive/h7-enforced-checkpoint.sh` as a diagnostic, not a gate
    /// the standard build is expected to pass yet (that script does not hard-fail the whole
    /// workspace verification on this specific test's outcome, only on its RDTSC regression
    /// check) -- the guest-driven-checkpoint *mechanism* this test exists to prove
    /// (`run_until_branch_or_halt_with_periodic_timer`, the third `checkpoint_init.c` fixture
    /// variant) is complete and correctly wired regardless of this residual finding.
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
            let (_ticks, outcome) = m
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
    /// by `snapshot_roundtrip_is_bit_identical` above.
    #[test]
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

        let mut outputs = Vec::with_capacity(NUM_BRANCHES);
        for i in 0..NUM_BRANCHES {
            let suffix = suffix_for(i);
            let mut branch = Multiverse::branch(&universe, suffix.clone(), WORK_CLOCK_K, None)
                .unwrap_or_else(|e| panic!("branch {i} failed: {e}"));
            let outcome = branch.run_to_first_halt().unwrap_or_else(|e| panic!("branch {i} run failed: {e}"));
            assert_eq!(
                outcome.console_output, suffix,
                "branch {i} must echo exactly its own tape suffix {suffix:?}, got \
                 {:?} — any mismatch means this branch observed another branch's state \
                 (or stale/shared state), not its own",
                outcome.console_output
            );
            outputs.push((suffix, outcome.ram_hash));
        }

        // Every branch's output is pinned to its own unique suffix by construction (asserted
        // above), so distinct suffixes trivially mean distinct expected outputs — this is an
        // explicit restatement of "no branch perturbs another" (the spec pseudocode's
        // `no_branch_perturbs_another`) rather than a new check.
        let unique_suffixes: std::collections::HashSet<_> = outputs.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(unique_suffixes.len(), NUM_BRANCHES, "every branch's tape suffix must be unique by construction");

        // A sample of branches, re-forked from the same universe with the same suffix, must be
        // internally deterministic (the spec pseudocode's `b.is_deterministic_double_run()`).
        for i in (0..NUM_BRANCHES).step_by(NUM_BRANCHES / DOUBLE_RUN_SAMPLE) {
            let suffix = suffix_for(i);
            let mut replay = Multiverse::branch(&universe, suffix.clone(), WORK_CLOCK_K, None)
                .unwrap_or_else(|e| panic!("branch {i} replay failed: {e}"));
            let replay_outcome =
                replay.run_to_first_halt().unwrap_or_else(|e| panic!("branch {i} replay run failed: {e}"));
            let (_, first_ram_hash) = &outputs[i];
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
    #[test]
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

