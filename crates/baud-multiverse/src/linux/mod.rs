// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real KVM/VT-x boot flow (specs/baud-multiverse.md §2, todo.md §3.1): `Kvm::new` →
// `create_vm` → register one zeroed guest-RAM region at a fixed guest-physical address →
// `create_vcpu` → set CPUID/sregs/regs → load the guest kernel with `linux-loader` and write boot
// params at fixed addresses → enter the run loop (`baud_vcpu::linux::run_until_halted`).
//
// Like `crates/baud-host/src/linux.rs` and `crates/baud-vcpu/src/linux/`, this module is written
// and type-checked against the real `kvm-ioctls`/`kvm-bindings`/`linux-loader`/`vm-memory` crate
// sources (`cargo check --target x86_64-unknown-linux-gnu -p baud-multiverse`) but has not yet
// been exercised on real KVM hardware — this dev machine has no Linux/KVM host (CLAUDE.md,
// todo.md §14). It is additive: nothing in `baud-server`/`baud-tape-agent` calls into this module
// yet (see the pivot notice at the top of `lib.rs`).

pub mod bootparams;
pub mod pagetables;

use crate::console::DeviceBus;
use crate::cpuid::{self, CpuidEntry};
use crate::layout;
use crate::timesource::{BranchCounter, WorkClock, MSR_IA32_TSC, MSR_IA32_TSC_DEADLINE, MSR_IA32_TSC_AUX};
use baud_snapshot::{PageStore, Universe};
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
/// rate.
pub const VIRTUAL_TSC_KHZ: u32 = 1_000_000;

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

    // One bit each — "this exact MSR is covered by this range" — with empty `flags` (neither
    // READ nor WRITE marked allowed), so a covered MSR is never allowed by KVM's own filter logic
    // and instead exits to userspace (the `Filter` reason enabled above).
    let covered_single_msr = [0b0000_0001u8];
    let trapped_msrs = [MSR_IA32_TSC, MSR_IA32_TSC_DEADLINE, MSR_IA32_TSC_AUX];
    let ranges: Vec<MsrFilterRange<'_>> = trapped_msrs
        .iter()
        .map(|&base| MsrFilterRange {
            flags: MsrFilterRangeFlags::empty(),
            base,
            msr_count: 1,
            bitmap: &covered_single_msr,
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
        let mut counter = Builder::new().kind(Hardware::BRANCH_INSTRUCTIONS).build()?;
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
        Ok(Multiverse { guest, bus, time: WorkClock::new(base, k, counter) })
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
        Ok(Multiverse { guest, bus, time })
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
}
