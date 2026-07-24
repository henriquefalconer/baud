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

use crate::page_store::{PageRef, PageStore, PAGE_SIZE};
use crate::universe::{order_msrs_tsc_first, restore_plan, model_matches, ClockState, DeviceState, MsrWrite, RestoreStep, Universe, VcpuState};
use kvm_bindings::{kvm_msr_entry, Msrs, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

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
}

/// Copy `size_of::<T>()` bytes out of `v` verbatim. Every `T` this is called with below
/// (`kvm_regs`, `kvm_sregs`, `kvm_lapic_state`, `kvm_xcrs`, `kvm_vcpu_events`, `kvm_mp_state`,
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
/// for `T`, typically all-zero) before `bytes` is copied over it; the copy length is clamped to
/// `size_of::<T>()`, so this never writes past `v`.
unsafe fn bytes_to_struct<T: Default>(bytes: &[u8]) -> T {
    let mut v = T::default();
    let len = bytes.len().min(std::mem::size_of::<T>());
    // SAFETY: see function doc — `v` is live and exactly `size_of::<T>()` bytes, `len <=
    // size_of::<T>()`, and `bytes` has at least `len` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), &mut v as *mut T as *mut u8, len) };
    v
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
    tape_cursor: u64,
    console: Vec<u8>,
) -> Result<Universe, CaptureError> {
    let ram = capture_ram(mem, ram_start, ram_size, page_store)?;

    let regs = vcpu.get_regs()?;
    let sregs = vcpu.get_sregs()?;
    let lapic = vcpu.get_lapic()?;
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
            lapic: struct_to_bytes(&lapic),
            xsave: struct_to_bytes(&xsave),
            xcrs: struct_to_bytes(&xcrs),
            events: struct_to_bytes(&events),
            mp_state: struct_to_bytes(&mp_state),
        }
    };

    let clock =
        ClockState { kvm_clock: unsafe { struct_to_bytes(&kvm_clock) }, tsc_khz, work_clock_base };

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
            RestoreStep::SetVcpuLapic => vcpu.set_lapic(&unsafe { bytes_to_struct(&universe.vcpu.lapic) })?,
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
