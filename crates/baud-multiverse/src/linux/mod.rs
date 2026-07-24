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

/// The `Kvm::new → create_vm → register zeroed guest RAM → create_vcpu → CPUID mask + MSR filter`
/// prefix shared by both ways a [`BootedGuest`] comes into existence (specs/baud-multiverse.md §2):
/// [`boot_guest`] continues it with a fresh kernel image (page tables, boot params, entry-point
/// regs); [`restore_guest`] continues it by walking a captured [`Universe`]'s `restore_plan`
/// instead (specs/baud-snapshot.md §6) — RAM/regs/sregs/etc. all come from the universe rather than
/// a freshly-loaded image, so this prefix is exactly the part both paths need identically and
/// nothing more.
fn create_vm_vcpu_shell() -> Result<BootedGuest, BootError> {
    baud_vcpu::validate_vcpu_count(1)?; // todo.md §1: exactly one vCPU per VM, checked first

    let kvm = Kvm::new()?;
    let vm = kvm.create_vm()?;

    let guest_mem = allocate_and_register_guest_ram(&vm, layout::GUEST_RAM_SIZE)?;

    let vcpu = vm.create_vcpu(0)?;
    apply_cpuid_mask(&kvm, &vcpu)?;
    configure_msr_filter(&vm)?;

    Ok(BootedGuest { kvm, vm, vcpu, guest_mem })
}

/// Run the full boot flow (specs/baud-multiverse.md §2's `Kvm::new → create_vm → register guest
/// RAM → create_vcpu → CPUID/TSC/MSR setup → linux-loader boot`) and return a [`BootedGuest`]
/// positioned at the kernel's 64-bit entry point, ready to enter `KVM_RUN`.
pub fn boot_guest(kernel_path: &Path, cmdline: &str) -> Result<BootedGuest, BootError> {
    let guest = create_vm_vcpu_shell()?;
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

    Ok(guest)
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
    #[error("reset_dirty_pages called before enable_dirty_ring")]
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
pub fn restore_guest(universe: &Universe, template_active: bool) -> Result<BootedGuest, RestoreError> {
    let guest = create_vm_vcpu_shell()?;
    baud_snapshot::linux::restore(
        &guest.kvm,
        &guest.vm,
        &guest.vcpu,
        &guest.guest_mem,
        layout::GUEST_RAM_START,
        universe,
        template_active,
    )?;
    Ok(guest)
}

/// Register [`layout::GUEST_RAM_START`]..`+ram_size` as one zeroed, anonymous-mmap-backed memory
/// slot (specs/baud-multiverse.md §3's "Memory init: Zeroed RAM at fixed guest-physical
/// addresses" — `GuestMemoryMmap::from_ranges` anonymous-mmaps zeroed pages, and nothing in this
/// boot flow ever writes host data into guest RAM except the specific structures this module
/// builds).
fn allocate_and_register_guest_ram(vm: &VmFd, ram_size: usize) -> Result<GuestMemory, BootError> {
    let guest_mem = GuestMemory::from_ranges(&[(GuestAddress(layout::GUEST_RAM_START), ram_size)])
        .map_err(|e| BootError::GuestMemory(ram_size, e.to_string()))?;

    let host_addr = guest_mem
        .get_host_address(GuestAddress(layout::GUEST_RAM_START))
        .map_err(|e| BootError::GuestMemory(ram_size, e.to_string()))?;
    let region = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: layout::GUEST_RAM_START,
        memory_size: ram_size as u64,
        userspace_addr: host_addr as u64,
        flags: 0,
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
    /// `Some` once [`enable_dirty_ring`](Self::enable_dirty_ring) has negotiated
    /// `KVM_CAP_DIRTY_LOG_RING` on this guest's vCPU (specs/baud-snapshot.md §5) — `None` until
    /// then, since the ring is an opt-in cost (an extra mmap + capability negotiation) a caller
    /// that never rewinds this `Multiverse` should not pay. [`reset_dirty_pages`](Self::
    /// reset_dirty_pages) requires it to be `Some`.
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
pub struct HaltOutcome {
    /// Every byte the guest wrote to the console (COM1 data register), in order.
    pub console_output: Vec<u8>,
    /// `blake3:<hex>` of the whole guest-RAM region, computed right after the halt.
    pub ram_hash: String,
}

impl Multiverse {
    /// Run [`boot_guest`] and wire up the work-clock (`base + k * rcb`, specs/baud-multiverse.md
    /// §4), console, and tape (specs/baud-tape-device.md) devices the run loop needs. `base` is
    /// normally `0` (a guest booting at virtual time zero); `k` scales RCB into a plausible Hz
    /// range for the guest's own clock arithmetic to work with sane-looking values. `tape` is the
    /// run's entire nondeterministic-input budget — the sole source the tape device serves
    /// (specs/baud-tape-device.md §5), fixed for this `Multiverse`'s whole lifetime.
    pub fn boot(
        kernel_path: &Path,
        cmdline: &str,
        base: u64,
        k: u64,
        tape: Vec<u8>,
    ) -> Result<Self, BootError> {
        let guest = boot_guest(kernel_path, cmdline)?;
        let counter = LinuxBranchCounter::new()?;
        let bus = DeviceBus::with_tape(tape);
        Ok(Multiverse { guest, bus, time: WorkClock::new(base, k, counter), dirty_ring: None })
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
    pub fn snapshot(
        &mut self,
        page_store: &mut PageStore,
    ) -> Result<Universe, baud_snapshot::linux::CaptureError> {
        baud_snapshot::linux::capture(
            &self.guest.kvm,
            &self.guest.vm,
            &self.guest.vcpu,
            &self.guest.guest_mem,
            layout::GUEST_RAM_START,
            layout::GUEST_RAM_SIZE,
            page_store,
            self.time.base(),
            self.time.tsc_deadline(),
            self.time.tsc_aux(),
            self.bus.tape.device().cursor(),
            self.bus.console.output().to_vec(),
        )
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
    pub fn restore(
        universe: &Universe,
        tape: Vec<u8>,
        k: u64,
        template_active: bool,
    ) -> Result<Self, RestoreError> {
        let guest = restore_guest(universe, template_active)?;
        let counter = LinuxBranchCounter::new().map_err(BootError::BranchCounter)?;
        let bus = DeviceBus::restore(tape, universe.device.tape_cursor, universe.device.console.clone());
        let time = WorkClock::restore(
            universe.clock.work_clock_base,
            k,
            universe.clock.tsc_deadline,
            universe.clock.tsc_aux,
            counter,
        );
        Ok(Multiverse { guest, bus, time, dirty_ring: None })
    }

    /// Negotiate `KVM_CAP_DIRTY_LOG_RING` on this guest's vCPU and start tracking dirtied RAM
    /// pages from this moment forward (specs/baud-snapshot.md §5's "reset" guarantee:
    /// `baud_snapshot::linux::DirtyRing::enable`'s doc — "the capability must be negotiated before
    /// any dirty page could occur"). Callers that intend to [`reset_dirty_pages`](Self::
    /// reset_dirty_pages) later must call this *before* [`run_to_first_halt`](Self::
    /// run_to_first_halt) or any other guest execution — any page dirtied before `enable_
    /// dirty_ring` runs is invisible to the ring and would not be restored by a later reset
    /// (it is, however, already baked into whatever `Universe` a subsequent [`snapshot`](Self::
    /// snapshot) captures as the "base" to reset back to, so a `boot`-then-`enable_dirty_ring`-
    /// then-`snapshot` sequence is self-consistent: everything written before enablement is part
    /// of the base itself, not a page the reset needs to touch).
    ///
    /// `entries` is the ring's slot count and must be a nonzero power of two (`baud_snapshot::
    /// linux::DirtyRing::enable`'s own validation) — 4096 slots (one page's worth of `kvm_dirty_
    /// gfn` entries times 256, comfortably above a typical branch's write set) is a reasonable
    /// default for callers with no sharper estimate of how many pages a run segment will dirty.
    pub fn enable_dirty_ring(&mut self, entries: u32) -> Result<(), baud_snapshot::linux::DirtyRingError> {
        let ring = baud_snapshot::linux::DirtyRing::enable(&self.guest.vm, &self.guest.vcpu, entries)?;
        self.dirty_ring = Some(ring);
        Ok(())
    }

    /// Rewind guest RAM to `base_ram`'s content for exactly the pages the dirty ring reports as
    /// touched since the last [`enable_dirty_ring`](Self::enable_dirty_ring)/`reset_dirty_pages`
    /// call (specs/baud-snapshot.md §5: "rewind copies back only dirtied pages ... cost ∝ change,
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

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![]).expect("first boot failed");
        let first_outcome = first.run_to_first_halt().expect("first run failed");
        assert_eq!(
            String::from_utf8_lossy(&first_outcome.console_output),
            HELLO_GUEST_MARKER,
            "guest must print exactly its marker line before halting"
        );

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![]).expect("second boot failed");
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

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, tape_a.clone())
            .expect("first boot (tape A) failed");
        let first_outcome = first.run_to_first_halt().expect("first run (tape A) failed");
        assert_eq!(
            first_outcome.console_output, tape_a,
            "guest must echo exactly the 4 tape bytes it read, byte for byte"
        );

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, tape_a.clone())
            .expect("second boot (tape A) failed");
        let second_outcome = second.run_to_first_halt().expect("second run (tape A) failed");
        assert_eq!(
            second_outcome.console_output, first_outcome.console_output,
            "same tape twice must produce byte-identical guest output"
        );

        let mut third = Multiverse::boot(&kernel, cmdline, 0, 1, tape_b.clone())
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

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![]).expect("first boot failed");
        let first_outcome = first.run_to_first_halt().expect("first run failed");
        assert_eq!(
            first_outcome.console_output, RDRAND_GUEST_MARKER,
            "guest must never get past the pre-rdrand marker: rdrand with a masked CPUID feature \
             bit must #UD immediately (real hardware behavior), not execute and produce output"
        );

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![]).expect("second boot failed");
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

        let mut first = Multiverse::boot(&kernel, cmdline, 0, 1, vec![]).expect("first boot failed");
        let (first_ticks, first_halt) = first
            .run_with_timer_ticks(PERIOD_RCB, TIMER_VECTOR, NUM_TICKS)
            .expect("first run with timer ticks failed");

        let mut second = Multiverse::boot(&kernel, cmdline, 0, 1, vec![]).expect("second boot failed");
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
}
