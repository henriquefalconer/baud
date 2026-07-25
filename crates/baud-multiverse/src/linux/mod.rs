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
    kvm_cpuid_entry2, kvm_enable_cap, kvm_userspace_memory_region, KVM_MAX_CPUID_ENTRIES,
    KVM_MEM_LOG_DIRTY_PAGES,
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

/// Run the full boot flow (specs/baud-multiverse.md §2's `Kvm::new → create_vm → register guest
/// RAM → create_vcpu → CPUID/TSC/MSR setup → linux-loader boot`) and return a [`BootedGuest`]
/// positioned at the kernel's 64-bit entry point, ready to enter `KVM_RUN`, alongside a
/// [`baud_snapshot::linux::DirtyRing`] if `dirty_ring_entries` was `Some` (see
/// [`create_vm_vcpu_shell`]'s doc for why negotiation must happen this early).
pub fn boot_guest(
    kernel_path: &Path,
    cmdline: &str,
    dirty_ring_entries: Option<u32>,
) -> Result<(BootedGuest, Option<baud_snapshot::linux::DirtyRing>), BootError> {
    let (guest, dirty_ring) = create_vm_vcpu_shell(dirty_ring_entries)?;
    guest.vcpu.set_tsc_khz(VIRTUAL_TSC_KHZ)?;

    pagetables::write_identity_page_tables(&guest.guest_mem, layout::GUEST_RAM_SIZE)
        .map_err(BootError::PageTables)?;
    pagetables::write_gdt(&guest.guest_mem).map_err(BootError::PageTables)?;
    guest.vcpu.set_sregs(&pagetables::long_mode_sregs())?;

    let loader_result = bootparams::load_kernel_and_write_boot_params(
        &guest.guest_mem,
        kernel_path,
        cmdline,
        layout::GUEST_RAM_SIZE,
    )?;

    let mut regs = guest.vcpu.get_regs()?;
    regs.rip = loader_result.kernel_load.raw_value() + layout::KERNEL_64BIT_ENTRY_OFFSET;
    regs.rsi = layout::ZERO_PAGE_ADDR; // Linux/x86 64-bit entry contract: RSI = &boot_params
    regs.rsp = layout::BOOT_STACK_POINTER;
    regs.rflags = 0x2; // bit 1 is reserved-must-be-1; every other flag starts clear
    guest.vcpu.set_regs(&regs)?;

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
/// `PERF_COUNT_HW_BRANCH_INSTRUCTIONS`, read (never armed/overflow-driven — that is
/// `baud_vcpu::linux::pmu::LinuxPmuStepper`'s separate concern for interrupt injection,
/// specs/baud-vcpu.md §5) on every `IA32_TSC` access (specs/baud-multiverse.md §4's work-clock
/// row). This is a distinct perf-event fd from `LinuxPmuStepper`'s armed counter; reconciling the
/// two into a single counter source is deferred until `baud-multiverse`'s thread model actually
/// exists and can be exercised on real perf/KVM hardware (see `crates/baud-vcpu/src/linux/pmu.rs`'s
/// module doc for the sibling scope note on this same "not yet exercised" boundary).
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
        // `LinuxPmuStepper`'s already-documented PMI-in-guest-mode signal gap). Left off; the
        // caller side (every consumer of this counter) must not execute data-dependent host code
        // between reads if it wants reproducible RCB deltas across runs — see
        // `crates/baud-vcpu/src/linux/pmu.rs`'s `arm_overflow`, which has the identical caveat.
        let mut builder = Builder::new().kind(Hardware::BRANCH_INSTRUCTIONS);
        // `pinned(true)`: same fix as `crates/baud-host/src/linux.rs`'s
        // `measure_fixed_loop_branches` (todo.md §14/H3) — keeps this counter resident on the PMU
        // instead of occasionally being multiplexed off mid-measurement under this project's own
        // nested-virtualized dev host, which otherwise undercounts by a small, run-varying amount.
        builder.pinned(true);
        let mut counter = builder.build()?;
        counter.enable()?;
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
        let (guest, dirty_ring) = boot_guest(kernel_path, cmdline, dirty_ring_entries)?;
        let counter = LinuxBranchCounter::new()?;
        let bus = DeviceBus::with_tape(tape);
        Ok(Multiverse { guest, bus, time: WorkClock::new(base, k, counter), dirty_ring })
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
    /// toward the target is still served deterministically, never skipped), anchors that
    /// stepper's own armed counter to the same RCB space via `with_baseline_rcb` (see that
    /// method's doc for why the two counters would otherwise disagree), then calls
    /// `boundary::inject_at`. Returns the exact `(rip, rcb)` the interrupt landed at — the tuple
    /// `timer_tick_lands_at_identical_instruction` compares across a double-run.
    pub fn inject_timer_tick(&mut self, period_rcb: u64, vector: u8) -> Result<TimerTick, DeterminismHole> {
        let baseline = self.time.current_rcb();
        let target_rcb = baseline.saturating_add(period_rcb);
        let mut stepper =
            baud_vcpu::linux::pmu::LinuxPmuStepper::new(&mut self.guest.vcpu, &mut self.bus, &mut self.time)
                .with_baseline_rcb(baseline);
        let point = baud_vcpu::boundary::inject_at(&mut stepper, target_rcb, vector)
            .map_err(|e| DeterminismHole(e.to_string()))?;
        Ok(TimerTick { rip: point.rip, rcb: point.rcb })
    }

    /// Inject `num_ticks` timer ticks spaced `period_rcb` apart (via repeated
    /// [`inject_timer_tick`](Self::inject_timer_tick)), then drive the guest to its first
    /// `Hlt`/`Shutdown` exactly like [`run_to_first_halt`](Self::run_to_first_halt). The natural
    /// entry point for a guest fixture that survives more than one delivered interrupt before
    /// halting — `timer_tick_lands_at_identical_instruction` calls this twice on the same
    /// image+tape and compares every returned [`TimerTick`] pairwise across the two runs.
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

    /// Every tape-device record (`PROBE`/`MARK_BRANCH`/`GOAL`/`VIOLATION`/`LOG`,
    /// specs/baud-tape-device.md §4) the guest has emitted and not yet drained. Callers typically
    /// call this after [`run_to_first_halt`](Self::run_to_first_halt) to collect what the guest
    /// reported before it halted.
    pub fn drain_tape_records(&mut self) -> Vec<baud_proto::Msg> {
        self.bus.tape.device_mut().drain_records()
    }

    /// blake3 of every byte of guest RAM, read in fixed-size chunks so this never needs to
    /// allocate the whole [`layout::GUEST_RAM_SIZE`] region at once.
    fn ram_hash(&self) -> String {
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
    /// `LinuxBranchCounter::new`'s and `LinuxPmuStepper::arm_overflow`'s docs). What remains is
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
            matches!(host.regime, baud_host::Regime::Cooperative | baud_host::Regime::Enforced),
            "this test needs a real KVM-capable host; got {:?} ({:?})",
            host.regime, host.reason
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

