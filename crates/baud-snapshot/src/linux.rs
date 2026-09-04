// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real `KVM_GET_*`/`KVM_SET_*` capture/restore calls enumerated in specs/baud-snapshot.md §3,
// walking `universe::restore_plan` in the order it defines. Like every other `linux/` module in
// this workspace, this is type-checked against the real crate sources (`cargo check --target
// x86_64-unknown-linux-gnu -p baud-snapshot`) but not yet exercised on real KVM hardware — this
// dev machine has no Linux/KVM host (CLAUDE.md, todo.md §14).
//
// NOT built here (deliberately, see todo.md §14 for the tracked next action): userfaultfd-based
// CoW branching (`Snapshot::branch`, specs/baud-snapshot.md §4) and `KVM_CAP_DIRTY_LOG_RING`-based
// cheap reset (`Snapshot::reset`, §5). The `userfaultfd` crate's sys bindings (`userfaultfd-sys`)
// generate their FFI layer with `bindgen` at build time, which needs `libclang` — and unlike the
// `cfg(target_os = "linux")` *code* in this module (which only needs to type-check, never link, on
// this Windows box), a build script always runs on the *host* regardless of `--target`, so
// `cargo check --target x86_64-unknown-linux-gnu` for a crate depending on `userfaultfd` fails
// right here with "Unable to find libclang" — confirmed by fetching the crate into a scratch
// project and running exactly that check. Two ways forward, for whoever picks this up: install
// LLVM/libclang on this dev machine so the existing `userfaultfd` crate's build script can run, or
// implement the handful of needed `UFFDIO_*` ioctls directly against `libc::syscall(SYS_userfaultfd,
// ..)` + hand-rolled `#[repr(C)]` mirrors of the kernel's `uffdio_*` structs (same pattern
// `baud-vcpu::linux::pmu` already uses for `F_SETSIG`, which isn't in the `libc` crate either) —
// avoiding `bindgen`/`libclang` entirely, at the cost of maintaining those struct layouts by hand.
// `universe::dirty_pages` already proves the *cost model* half of the reset guarantee
// (specs/baud-snapshot.md §5) without needing either path; only the real page-fault-driven sharing
// and the real in-kernel dirty bitmap are what's missing.

use crate::dirty_ring::{self, RawDirtyGfn};
use crate::page_store::{PageRef, PageStore, PAGE_SIZE};
use crate::universe::{order_msrs_tsc_first, restore_plan, model_matches, ClockState, DeviceState, MsrWrite, RestoreStep, Universe, VcpuState};
use kvm_bindings::{
    kvm_clock_data, kvm_dirty_gfn, kvm_enable_cap, kvm_mp_state, kvm_msr_entry,
    kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs, kvm_xsave, Msrs, KVM_CAP_DIRTY_LOG_RING,
    KVM_DIRTY_LOG_PAGE_OFFSET, KVM_MAX_CPUID_ENTRIES,
};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use std::os::fd::AsRawFd;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_sys_util::ioctl::{ioctl_expr, _IOC_NONE};

/// The guest-RAM backend type this module reads/writes — matches
/// `baud_multiverse::linux::GuestMemory` (a single anonymous-mmap region, no built-in dirty-page
/// tracking of its own).
pub type GuestMemory = GuestMemoryMmap<()>;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("KVM ioctl failed while capturing state: {0}")]
    Kvm(#[from] kvm_ioctls::Error),
    #[error("failed to read guest RAM at offset {0:#x}: {1}")]
    GuestMemory(u64, vm_memory::guest_memory::Error),
    #[error("failed to allocate the MSR FAM buffer: {0}")]
    MsrAlloc(vmm_sys_util::fam::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("KVM ioctl failed while restoring state: {0}")]
    Kvm(#[from] kvm_ioctls::Error),
    #[error("failed to write guest RAM at offset {0:#x}: {1}")]
    GuestMemory(u64, vm_memory::guest_memory::Error),
    #[error("failed to allocate the MSR FAM buffer: {0}")]
    MsrAlloc(vmm_sys_util::fam::Error),
    #[error(
        "universe captured on CPU signature {captured:#010x}, this host is {current:#010x} \
         (specs/baud-snapshot.md §6 point 4: refused, no CPUID template active)"
    )]
    CpuMismatch { captured: u32, current: u32 },
    #[error("universe field {field} has length {actual}, expected {expected}")]
    InvalidStateLength { field: &'static str, actual: usize, expected: usize },
}

/// Copy `size_of::<T>()` bytes out of `v` verbatim. Every `T` this is called with below
/// (`kvm_regs`, `kvm_sregs`, `kvm_xcrs`, `kvm_vcpu_events`, `kvm_mp_state`,
/// `kvm_xsave`, `kvm_clock_data`) is a `#[repr(C)]` plain-old-data struct from `kvm-bindings`; this
/// module never inspects the bytes, only round-trips them back into the *same* struct type via
/// [`bytes_to_struct`], so any uninitialized padding is written back unread.
///
/// # Safety
/// `v` is a live `&T` for the duration of the read, so `size_of::<T>()` bytes starting at its
/// address are valid to read.
unsafe fn struct_to_bytes<T>(v: &T) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }.to_vec()
}

/// The inverse of [`struct_to_bytes`]: reconstruct a `T` from bytes previously produced by it.
///
/// # Safety
/// `v` is freshly `Default`-constructed (so every byte starts at a valid, kernel-accepted value
/// for `T`, typically all-zero) before `bytes` is copied over it. Callers validate the exact
/// length before entering this function, so truncated wire records cannot become zero-padded
/// state.
unsafe fn bytes_to_struct<T: Default>(bytes: &[u8]) -> T {
    debug_assert_eq!(bytes.len(), std::mem::size_of::<T>());
    let mut v = T::default();
    let len = bytes.len().min(std::mem::size_of::<T>());
    // SAFETY: see function doc — `v` is live and exactly `size_of::<T>()` bytes, `len <=
    // size_of::<T>()`, and `bytes` has at least `len` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), &mut v as *mut T as *mut u8, len) };
    v
}

fn validate_state_lengths(universe: &Universe) -> Result<(), RestoreError> {
    macro_rules! exact {
        ($field:expr, $name:literal, $ty:ty) => {
            if $field.len() != std::mem::size_of::<$ty>() {
                return Err(RestoreError::InvalidStateLength {
                    field: $name,
                    actual: $field.len(),
                    expected: std::mem::size_of::<$ty>(),
                });
            }
        };
    }
    exact!(universe.vcpu.regs, "regs", kvm_regs);
    exact!(universe.vcpu.sregs, "sregs", kvm_sregs);
    exact!(universe.vcpu.xsave, "xsave", kvm_xsave);
    exact!(universe.vcpu.xcrs, "xcrs", kvm_xcrs);
    exact!(universe.vcpu.events, "events", kvm_vcpu_events);
    exact!(universe.vcpu.mp_state, "mp_state", kvm_mp_state);
    exact!(universe.clock.kvm_clock, "kvm_clock", kvm_clock_data);
    Ok(())
}

/// CPUID leaf 1's EAX (the x86 "processor signature": family/model/stepping) — the CPU-model
/// fingerprint [`crate::universe::model_matches`] compares between capture and restore hosts
/// (specs/baud-snapshot.md §6 point 4). Read via `KVM_GET_SUPPORTED_CPUID` rather than a live
/// vCPU's currently-set CPUID so it is available even before a vCPU exists (needed on the restoring
/// side, where `restore` runs before the caller has necessarily finished configuring the vCPU it
/// will run on) and is unaffected by `cpuid::apply_determinism_mask`'s edits (which never touch
/// leaf 1 EAX).
fn cpuid_leaf1_eax(kvm: &Kvm) -> Result<u32, kvm_ioctls::Error> {
    let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    Ok(cpuid.as_slice().iter().find(|e| e.function == 1 && e.index == 0).map(|e| e.eax).unwrap_or(0))
}

fn capture_ram(
    mem: &GuestMemory,
    ram_start: u64,
    ram_size: usize,
    page_store: &mut PageStore,
) -> Result<Vec<PageRef>, CaptureError> {
    let page_count = ram_size.div_ceil(PAGE_SIZE);
    let mut ram = Vec::with_capacity(page_count);
    let mut buf = [0u8; PAGE_SIZE];
    for i in 0..page_count {
        let offset = ram_start + (i * PAGE_SIZE) as u64;
        mem.read_slice(&mut buf, GuestAddress(offset)).map_err(|e| CaptureError::GuestMemory(offset, e))?;
        ram.push(page_store.intern(&buf));
    }
    Ok(ram)
}

fn restore_ram(mem: &GuestMemory, ram_start: u64, ram: &[PageRef]) -> Result<(), RestoreError> {
    for (i, page) in ram.iter().enumerate() {
        let offset = ram_start + (i * PAGE_SIZE) as u64;
        mem.write_slice(page.bytes(), GuestAddress(offset)).map_err(|e| RestoreError::GuestMemory(offset, e))?;
    }
    Ok(())
}

/// Capture a complete [`Universe`] from a live, already-booted guest (specs/baud-snapshot.md §3's
/// enumerated capture set): every `KVM_GET_*` this crate's `VcpuState`/`ClockState` fields require,
/// plus the guest-RAM pages (deduplicated into `page_store`, specs/baud-snapshot.md §4) and the
/// caller-supplied work-clock anchor / tape-device cursor / console bytes (this crate does not know
/// how to serialize `baud-multiverse`'s own device models, see `universe::DeviceState`'s doc).
#[allow(clippy::too_many_arguments)]
pub fn capture(
    kvm: &Kvm,
    vm: &VmFd,
    vcpu: &VcpuFd,
    mem: &GuestMemory,
    ram_start: u64,
    ram_size: usize,
    page_store: &mut PageStore,
    work_clock_base: u64,
    rcb_anchor: u64,
    tsc_deadline: u64,
    tsc_aux: u64,
    entropy_state: u64,
    tape_cursor: u64,
    console: Vec<u8>,
) -> Result<Universe, CaptureError> {
    let ram = capture_ram(mem, ram_start, ram_size, page_store)?;

    let regs = vcpu.get_regs()?;
    let sregs = vcpu.get_sregs()?;
    let xsave = vcpu.get_xsave()?;
    let xcrs = vcpu.get_xcrs()?;
    let events = vcpu.get_vcpu_events()?;
    let mp_state = vcpu.get_mp_state()?;

    let msr_index_list = kvm.get_msr_index_list()?;
    let entries: Vec<kvm_msr_entry> =
        msr_index_list.as_slice().iter().map(|&index| kvm_msr_entry { index, ..Default::default() }).collect();
    let mut msrs = Msrs::from_entries(&entries).map_err(CaptureError::MsrAlloc)?;
    let read = vcpu.get_msrs(&mut msrs)?;
    let mut msr_writes: Vec<MsrWrite> =
        msrs.as_slice()[..read].iter().map(|e| MsrWrite { index: e.index, data: e.data }).collect();
    order_msrs_tsc_first(&mut msr_writes);

    let tsc_khz = vcpu.get_tsc_khz()?;
    let kvm_clock = vm.get_clock()?;
    let cpu_signature = cpuid_leaf1_eax(kvm)?;

    // SAFETY: every argument here is a live reference to a `#[repr(C)]` kvm-bindings struct this
    // module just received from the corresponding `KVM_GET_*` call above — see `struct_to_bytes`'s
    // own doc for the full rationale.
    let vcpu_state = unsafe {
        VcpuState {
            regs: struct_to_bytes(&regs),
            sregs: struct_to_bytes(&sregs),
            msrs: msr_writes,
            xsave: struct_to_bytes(&xsave),
            xcrs: struct_to_bytes(&xcrs),
            events: struct_to_bytes(&events),
            mp_state: struct_to_bytes(&mp_state),
        }
    };

    let clock = ClockState {
        kvm_clock: unsafe { struct_to_bytes(&kvm_clock) },
        tsc_khz,
        work_clock_base,
        rcb_anchor,
        tsc_deadline,
        tsc_aux,
        entropy_state,
    };

    let device = DeviceState { tape_cursor, console };

    Ok(Universe { ram, vcpu: vcpu_state, clock, device, cpu_signature })
}

/// Restore a [`Universe`] onto an already-created (but not yet run) vCPU, walking
/// `universe::restore_plan`'s exact step order (specs/baud-snapshot.md §6). `mem` must already be
/// registered as the guest's memory backing at `ram_start` with at least `universe.ram.len() *
/// PAGE_SIZE` bytes (a fresh `boot_guest`-style call is `baud-multiverse`'s job, not this crate's —
/// this function only overwrites bytes into memory that already exists as a KVM memslot).
///
/// [`RestoreStep::SetTscKhz`] is issued first, matching §6 point 1's "set TSC frequency before
/// creating the vCPU": `KVM_SET_TSC_KHZ` is a per-vCPU ioctl in this crate's pinned `kvm-ioctls`
/// version (`vcpu.set_tsc_khz`), so it cannot literally precede `create_vcpu` — this restores it as
/// the very first thing done *to* the vCPU, before any other register/MSR/clock state, which is
/// the same ordering `baud_multiverse::linux::boot_guest` already uses for a fresh boot (set right
/// after `create_vcpu`, before CPUID/sregs/regs).
pub fn restore(
    kvm: &Kvm,
    vm: &VmFd,
    vcpu: &VcpuFd,
    mem: &GuestMemory,
    ram_start: u64,
    universe: &Universe,
    template_active: bool,
) -> Result<(), RestoreError> {
    validate_state_lengths(universe)?;
    let current_signature = cpuid_leaf1_eax(kvm)?;
    if !model_matches(universe.cpu_signature, current_signature, template_active) {
        return Err(RestoreError::CpuMismatch { captured: universe.cpu_signature, current: current_signature });
    }

    for step in restore_plan() {
        match step {
            RestoreStep::SetTscKhz => vcpu.set_tsc_khz(universe.clock.tsc_khz)?,
            RestoreStep::RegisterRam => restore_ram(mem, ram_start, &universe.ram)?,
            // SAFETY: `bytes_to_struct` reconstructs a fresh `Default`-initialized struct of the
            // exact type each `set_*` ioctl expects, then overwrites it with bytes this same
            // module's `capture` produced from that exact type via `struct_to_bytes` — see both
            // functions' docs.
            RestoreStep::SetVcpuRegs => vcpu.set_regs(&unsafe { bytes_to_struct(&universe.vcpu.regs) })?,
            RestoreStep::SetVcpuSregs => vcpu.set_sregs(&unsafe { bytes_to_struct(&universe.vcpu.sregs) })?,
            RestoreStep::SetVcpuMsrs => {
                let entries: Vec<kvm_msr_entry> = universe
                    .vcpu
                    .msrs
                    .iter()
                    .map(|m| kvm_msr_entry { index: m.index, data: m.data, ..Default::default() })
                    .collect();
                let msrs = Msrs::from_entries(&entries).map_err(RestoreError::MsrAlloc)?;
                vcpu.set_msrs(&msrs)?;
            }
            // SAFETY: `set_xsave` is itself `unsafe` (kvm-ioctls: dynamically-enabled XSTATE
            // features could need more than the traditional 4096-byte `kvm_xsave`) — this crate
            // captures only via the fixed-size `KVM_GET_XSAVE` (not `KVM_GET_XSAVE2`), so the
            // reconstructed struct is always exactly 4096 bytes, matching what was captured.
            RestoreStep::SetVcpuXsave => unsafe { vcpu.set_xsave(&bytes_to_struct(&universe.vcpu.xsave))? },
            RestoreStep::SetVcpuXcrs => vcpu.set_xcrs(&unsafe { bytes_to_struct(&universe.vcpu.xcrs) })?,
            RestoreStep::SetVcpuEvents => vcpu.set_vcpu_events(&unsafe { bytes_to_struct(&universe.vcpu.events) })?,
            RestoreStep::SetVcpuMpState => vcpu.set_mp_state(unsafe { bytes_to_struct(&universe.vcpu.mp_state) })?,
            RestoreStep::SetVmClock => vm.set_clock(&unsafe { bytes_to_struct(&universe.clock.kvm_clock) })?,
            // Device/console restoration is the caller's job (see this function's doc): the
            // caller reads `universe.device` after `restore` returns `Ok` and feeds it back into
            // its own tape-device/console model, same reason `DeviceState` stores opaque bytes
            // instead of a concrete type this crate would have to depend on `baud-multiverse` for.
            RestoreStep::RestoreDevice => {}
        }
    }
    Ok(())
}

/// `/dev/kvm`'s ioctl type character (`include/uapi/linux/kvm.h`'s `KVMIO`, `0xAE`) — every
/// `kvm-ioctls`-defined ioctl in this crate's dependency tree is built from the same constant
/// (confirmed against `vmm_sys_util::ioctl::ioctl_expr`'s own doctest, which uses this exact
/// value), so this is not a guess specific to this module.
const KVMIO: std::os::raw::c_uint = 0xAE;

/// `KVM_RESET_DIRTY_RINGS` — `_IO(KVMIO, 0xc7)`, a VM-scoped ioctl with no argument struct (the
/// kernel walks every dirty ring itself and returns the harvested-page count as the ioctl's own
/// return value, the same "`_IO()`, no payload" shape `kvm-ioctls` already uses for `KVM_RUN`/
/// `KVM_CREATE_VM`). Not defined by `kvm-ioctls` 0.25 (added to the kernel alongside
/// `KVM_CAP_DIRTY_LOG_RING`, which this crate's pinned `kvm-bindings` 0.14 *does* expose the
/// capability constant and `kvm_dirty_gfn` struct for — only the ioctl number itself is missing
/// from the pinned crate versions), hand-derived here from the same `ioctl_expr` helper
/// `kvm-ioctls` itself is built on (`0xc7` immediately follows `KVM_SET_MSR_FILTER`'s `0xc6`,
/// already defined one line away in `kvm_ioctls.rs` — see this module's doc for the general
/// "type-checked, not exercised on real hardware" caveat that applies to every ioctl number below
/// exactly as it does to every `KVM_GET_*`/`KVM_SET_*` call above).
fn kvm_reset_dirty_rings_nr() -> std::os::raw::c_ulong {
    ioctl_expr(_IOC_NONE, KVMIO, 0xc7, 0)
}

/// The host's page size, used both as the `KVM_DIRTY_LOG_PAGE_OFFSET` mmap-offset multiplier and
/// as the minimum/step granularity the kernel requires the dirty-ring's byte size to be a
/// power-of-two multiple of.
const HOST_PAGE_SIZE: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum DirtyRingError {
    #[error("KVM ioctl failed while configuring the dirty ring: {0}")]
    Kvm(#[from] kvm_ioctls::Error),
    #[error("dirty-ring entry count must be a power of two (got {0})")]
    NotPowerOfTwo(u32),
    #[error("mmap of the dirty-ring buffer failed: {0}")]
    Mmap(std::io::Error),
    #[error("KVM_RESET_DIRTY_RINGS ioctl failed: {0}")]
    ResetIoctl(std::io::Error),
}

/// A live `KVM_CAP_DIRTY_LOG_RING` ring buffer for one vCPU (specs/baud-snapshot.md §5's "reset"
/// guarantee: "rewind copies back only dirtied pages ... cost ∝ change, not machine size").
/// Because this workspace enforces exactly one vCPU per VM (todo.md §1's hard constraint), a
/// per-vCPU ring *is* the whole VM's dirty-page record — there is no cross-vCPU ring to merge.
///
/// Usage is a two-step negotiation, not one call — **real-hardware finding**: an earlier version
/// of this API offered a single combined `enable(vm, vcpu, entries)` documented as callable "right
/// after `create_vcpu`," which is wrong. The kernel refuses `KVM_ENABLE_CAP(KVM_CAP_DIRTY_LOG_RING)`
/// with `EINVAL` once the VM already has a vCPU (`kvm_vm_ioctl_enable_cap`'s own
/// `kvm->created_vcpus` check) — confirmed on real `/dev/kvm` hardware the first time any caller
/// actually exercised this path (todo.md §14's H5 `reset_cost_scales_with_write_set` test). So:
/// [`DirtyRing::negotiate_capability`] must run on the `VmFd` *before* `create_vcpu`;
/// [`DirtyRing::open`] mmaps the resulting per-vCPU ring afterward, once the vCPU exists, with the
/// same `entries` count (`baud_multiverse::linux::create_vm_vcpu_shell` is the one place in this
/// workspace that creates a VM and its vCPU together, so it is the caller responsible for getting
/// this ordering right). After that: [`DirtyRing::collect`] after a run segment to get every page
/// the guest touched since the last collect, in the order the kernel published them;
/// [`DirtyRing::confirm_reset`] once the caller has restored/rewound those exact pages, telling the
/// kernel it may resume tracking from a clean slate for that set — the classic "harvest, act,
/// confirm" three-step the kernel's own `KVM_CAP_DIRTY_LOG_RING` documentation describes, factored
/// here into explicit calls so a caller can (for example) crash between harvest and confirm without
/// silently losing dirty pages (an un-confirmed entry stays `DIRTY` and is re-harvested next time).
pub struct DirtyRing {
    ptr: *mut kvm_dirty_gfn,
    entries: usize,
    cursor: usize,
}

// SAFETY-relevant: the mmap'd ring is process-private (MAP_SHARED with the kernel, not with any
// other userspace mapping) and this crate's threading model gives exactly one owner at a time
// (specs/baud-multiverse.md §3.1's one vCPU thread) — `Send` is sound because nothing here is
// `!Send` except the raw pointer, and ownership transfer (not concurrent access) is the only thing
// `Send` needs to guarantee.
unsafe impl Send for DirtyRing {}

impl DirtyRing {
    /// Negotiate `KVM_CAP_DIRTY_LOG_RING` on `vm` for `entries` `kvm_dirty_gfn` slots. Must be
    /// called before `vm.create_vcpu` — see [`DirtyRing`]'s own doc for the real-hardware `EINVAL`
    /// this ordering requirement was found from.
    pub fn negotiate_capability(vm: &VmFd, entries: u32) -> Result<(), DirtyRingError> {
        if entries == 0 || !entries.is_power_of_two() {
            return Err(DirtyRingError::NotPowerOfTwo(entries));
        }
        let bytes = dirty_ring::ring_bytes(entries);
        let mut cap = kvm_enable_cap { cap: KVM_CAP_DIRTY_LOG_RING, ..Default::default() };
        cap.args[0] = bytes as u64;
        vm.enable_cap(&cap)?;
        Ok(())
    }

    /// Mmap the per-vCPU ring off `vcpu`'s file descriptor at `KVM_DIRTY_LOG_PAGE_OFFSET` pages in
    /// (the kernel's documented convention for this specific ring), `MAP_SHARED` so writes the
    /// kernel makes to publish new entries are visible without a re-mmap. **Real-hardware finding**:
    /// this must be `PROT_READ | PROT_WRITE`, not read-only — [`collect`](Self::collect) writes the
    /// `RESET` flag bit back into this same mapping to mark harvested entries (the kernel's own
    /// three-step "harvest, act, confirm" protocol requires userspace to mutate the ring in place,
    /// matching how e.g. QEMU maps this same ring read-write); a read-only mapping segfaults
    /// (`SIGSEGV`) the instant `collect` is first called, confirmed on real `/dev/kvm` hardware
    /// (todo.md §14's H5 `reset_cost_scales_with_write_set` test — the first caller ever to reach a
    /// live `collect()`). `entries` must be the exact same value already passed to
    /// [`DirtyRing::negotiate_capability`] on this vCPU's VM — the kernel sized the ring from that
    /// earlier call, not this one.
    pub fn open(vcpu: &VcpuFd, entries: u32) -> Result<Self, DirtyRingError> {
        if entries == 0 || !entries.is_power_of_two() {
            return Err(DirtyRingError::NotPowerOfTwo(entries));
        }
        let bytes = dirty_ring::ring_bytes(entries);

        // SAFETY: `vcpu.as_raw_fd()` is a live vCPU fd for the duration of this call;
        // `KVM_DIRTY_LOG_PAGE_OFFSET * HOST_PAGE_SIZE` is the kernel-documented mmap offset for
        // this exact ring (distinct from the vCPU's own `kvm_run` mmap at offset 0); `bytes` was
        // already accepted by `KVM_ENABLE_CAP` in `negotiate_capability`, so the kernel has sized
        // this mapping already; `PROT_WRITE` is required, see this method's doc.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                vcpu.as_raw_fd(),
                (KVM_DIRTY_LOG_PAGE_OFFSET as libc::off_t) * (HOST_PAGE_SIZE as libc::off_t),
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(DirtyRingError::Mmap(std::io::Error::last_os_error()));
        }
        Ok(DirtyRing { ptr: addr as *mut kvm_dirty_gfn, entries: entries as usize, cursor: 0 })
    }

    /// Copy every ring slot's current bytes into a portable [`RawDirtyGfn`] buffer, run
    /// [`dirty_ring::harvest`] (the hardware-independent protocol logic) over it, then write the
    /// (possibly `RESET`-flag-updated) slots back. The kernel only ever *appends* new entries at
    /// its own write position and never rewrites a slot userspace has already harvested, so a
    /// plain volatile copy in each direction — rather than an atomic per-field exchange — is
    /// sufficient: the two writers (kernel appending, userspace marking `RESET`) never touch the
    /// same slot in the same window this crate calls `collect` from (guest is not running between
    /// a snapshot's `collect`/`confirm_reset` pair, specs/baud-snapshot.md §5's rewind boundary).
    pub fn collect(&mut self) -> Vec<(u32, u64)> {
        // SAFETY: `self.ptr` was mmap'd for exactly `self.entries * size_of::<kvm_dirty_gfn>()`
        // bytes by `enable` and lives until `Drop::drop` below unmaps it; `self` is borrowed
        // mutably for this call's duration so no other access races it.
        let raw = unsafe { std::slice::from_raw_parts(self.ptr, self.entries) };
        let mut mirrored: Vec<RawDirtyGfn> = raw
            .iter()
            .map(|g| {
                // SAFETY: `g` points into the live mmap `raw` was built from above; a volatile
                // read is used because the kernel may concurrently publish new entries elsewhere
                // in this same mapping (only `g.flags` needs the volatile guarantee — see the
                // write-back loop below for why `slot`/`offset` do not).
                let flags = unsafe { std::ptr::read_volatile(&g.flags) };
                RawDirtyGfn { flags, slot: g.slot, offset: g.offset }
            })
            .collect();
        let harvested = dirty_ring::harvest(&mut mirrored, &mut self.cursor);
        // SAFETY: same mapping/lifetime/exclusivity argument as the read above; only `flags` may
        // have changed (harvest only ever sets RESET_BIT on entries it returns), so only that
        // field is written back, leaving `slot`/`offset` — which the kernel owns — untouched.
        unsafe {
            let raw_mut = std::slice::from_raw_parts_mut(self.ptr, self.entries);
            for (slot, mirror) in raw_mut.iter_mut().zip(mirrored.iter()) {
                std::ptr::write_volatile(&mut slot.flags, mirror.flags);
            }
        }
        harvested
    }

    /// `KVM_RESET_DIRTY_RINGS`: tell the kernel every slot this ring (across every vCPU, but with
    /// one vCPU per VM that is just this one) marked `RESET` in a prior [`DirtyRing::collect`] may
    /// now be reclaimed and its backing page re-armed for the next write-fault. Returns the number
    /// of pages the kernel actually reset — specs/baud-snapshot.md §5's "reset cost scales with
    /// write-set, not total RAM" is this return value being bounded by what was harvested, never
    /// by total guest-RAM page count.
    pub fn confirm_reset(&mut self, vm: &VmFd) -> Result<u32, DirtyRingError> {
        // SAFETY: `KVM_RESET_DIRTY_RINGS` takes no argument struct (`_IO`, size 0) and only reads
        // process-owned KVM state; `vm.as_raw_fd()` is a live VM fd for the call's duration.
        let ret = unsafe { libc::ioctl(vm.as_raw_fd(), kvm_reset_dirty_rings_nr(), 0) };
        if ret < 0 {
            return Err(DirtyRingError::ResetIoctl(std::io::Error::last_os_error()));
        }
        Ok(ret as u32)
    }
}

impl Drop for DirtyRing {
    fn drop(&mut self) {
        // SAFETY: `self.ptr`/`self.entries` are exactly the address/length `enable`'s `mmap` call
        // returned and reserved; nothing else in this process holds a reference to this mapping
        // (it is private to this `DirtyRing`, never exposed by any public API here).
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.entries * std::mem::size_of::<kvm_dirty_gfn>());
        }
    }
}

#[cfg(test)]
mod state_validation_tests {
    use super::*;

    #[test]
    fn truncated_register_state_is_rejected_before_kvm_restore() {
        let universe = Universe {
            ram: Vec::new(),
            vcpu: VcpuState {
                regs: vec![0; std::mem::size_of::<kvm_regs>() - 1],
                sregs: vec![0; std::mem::size_of::<kvm_sregs>()],
                msrs: Vec::new(),
                xsave: vec![0; std::mem::size_of::<kvm_xsave>()],
                xcrs: vec![0; std::mem::size_of::<kvm_xcrs>()],
                events: vec![0; std::mem::size_of::<kvm_vcpu_events>()],
                mp_state: vec![0; std::mem::size_of::<kvm_mp_state>()],
            },
            clock: ClockState {
                kvm_clock: vec![0; std::mem::size_of::<kvm_clock_data>()],
                tsc_khz: 1,
                work_clock_base: 0,
                rcb_anchor: 0,
                tsc_deadline: 0,
                tsc_aux: 0,
                entropy_state: 0,
            },
            device: DeviceState { tape_cursor: 0, console: Vec::new() },
            cpu_signature: 0,
        };
        assert!(matches!(validate_state_lengths(&universe), Err(RestoreError::InvalidStateLength { field: "regs", .. })));
    }
}
