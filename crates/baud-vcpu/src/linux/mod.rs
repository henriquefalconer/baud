// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real Linux/KVM half of baud-vcpu: converting a real `kvm_ioctls::VcpuExit` into baud-vcpu's
// own `Exit` (never silently dropping an exit kind), thread affinity pinning, the
// `KVM_SET_GUEST_DEBUG` single-step toggle, and the `KVM_RUN` loop that drives `dispatch_exit`.
//
// NOTE: like `crates/baud-host/src/linux.rs`, this module is written and type-checked against the
// real `kvm-ioctls`/`kvm-bindings`/`perf-event` crate sources (`cargo check --target
// x86_64-unknown-linux-gnu -p baud-vcpu`) but has not yet been exercised on real KVM hardware —
// this dev machine has no Linux/KVM host (CLAUDE.md). `pmu` additionally documents a
// process-wide-signal simplification that a full multi-thread VMM integration must revisit.

pub mod pmu;

use crate::{dispatch_exit, Bus, DeterminismHole, DispatchOutcome, Exit, TimeSource};
use kvm_bindings::{kvm_guest_debug, KVM_GUESTDBG_BLOCKIRQ, KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP};
use kvm_ioctls::{VcpuExit, VcpuFd};
use std::io;

/// Convert a real KVM exit into baud-vcpu's own vocabulary (specs/baud-vcpu.md §3). This is the
/// one place a KVM exit kind can enter the system; anything this match does not model becomes
/// `Exit::Unmodeled(name)`, which `dispatch_exit`'s catch-all always turns into
/// `Err(DeterminismHole)` — never a best-effort continue (specs/baud-multiverse.md §3's last row).
pub fn convert_exit(exit: VcpuExit<'_>) -> Exit<'_> {
    match exit {
        VcpuExit::IoIn(port, data) => Exit::IoIn(port, data),
        VcpuExit::IoOut(port, data) => Exit::IoOut(port, data),
        VcpuExit::MmioRead(addr, data) => Exit::MmioRead(addr, data),
        VcpuExit::MmioWrite(addr, data) => Exit::MmioWrite(addr, data),
        VcpuExit::X86Rdmsr(read) => {
            // baud always serves a computed value for every trapped MSR; never inject a #GP for
            // a read (specs/baud-multiverse.md §4's MSR-filter row routes IA32_TSC/TSC_AUX/
            // TSC_DEADLINE to the VMM specifically so they can always be served).
            *read.error = 0;
            Exit::Rdmsr(read.index, read.data)
        }
        VcpuExit::X86Wrmsr(write) => {
            *write.error = 0;
            Exit::Wrmsr(write.index, write.data)
        }
        VcpuExit::Hlt => Exit::Hlt,
        VcpuExit::Shutdown => Exit::Shutdown,
        VcpuExit::Debug(_) => Exit::Debug,
        VcpuExit::Unknown => Exit::Unmodeled("Unknown"),
        VcpuExit::Exception => Exit::Unmodeled("Exception"),
        VcpuExit::Hypercall(_) => Exit::Unmodeled("Hypercall"),
        VcpuExit::IrqWindowOpen => Exit::Unmodeled("IrqWindowOpen"),
        VcpuExit::FailEntry(..) => Exit::Unmodeled("FailEntry"),
        VcpuExit::Intr => Exit::Unmodeled("Intr"),
        VcpuExit::SetTpr => Exit::Unmodeled("SetTpr"),
        VcpuExit::TprAccess => Exit::Unmodeled("TprAccess"),
        VcpuExit::S390Sieic => Exit::Unmodeled("S390Sieic"),
        VcpuExit::S390Reset => Exit::Unmodeled("S390Reset"),
        VcpuExit::Dcr => Exit::Unmodeled("Dcr"),
        VcpuExit::Nmi => Exit::Unmodeled("Nmi"),
        VcpuExit::InternalError => Exit::Unmodeled("InternalError"),
        VcpuExit::Osi => Exit::Unmodeled("Osi"),
        VcpuExit::PaprHcall => Exit::Unmodeled("PaprHcall"),
        VcpuExit::S390Ucontrol => Exit::Unmodeled("S390Ucontrol"),
        VcpuExit::Watchdog => Exit::Unmodeled("Watchdog"),
        VcpuExit::S390Tsch => Exit::Unmodeled("S390Tsch"),
        VcpuExit::Epr => Exit::Unmodeled("Epr"),
        VcpuExit::SystemEvent(..) => Exit::Unmodeled("SystemEvent"),
        VcpuExit::S390Stsi => Exit::Unmodeled("S390Stsi"),
        VcpuExit::IoapicEoi(_) => Exit::Unmodeled("IoapicEoi"),
        VcpuExit::Hyperv => Exit::Unmodeled("Hyperv"),
        VcpuExit::MemoryFault { .. } => Exit::Unmodeled("MemoryFault"),
        VcpuExit::Unsupported(_) => Exit::Unmodeled("Unsupported"),
    }
}

/// Pin the calling thread to exactly one physical core (specs/baud-vcpu.md §4: "Thread pinned to
/// one core"; specs/baud-host.md §5's one-core-per-VM fleet placement picks which one).
pub fn pin_thread_to_core(core_id: usize) -> io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core_id, &mut set);
        let rc = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Enable (`block_irq = true` also sets `KVM_GUESTDBG_BLOCKIRQ`, specs/baud-vcpu.md §5 step 3) or
/// disable single-step via `KVM_SET_GUEST_DEBUG`.
pub fn set_singlestep(vcpu: &VcpuFd, enabled: bool, block_irq: bool) -> io::Result<()> {
    let mut control = 0u32;
    if enabled {
        control |= KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP;
        if block_irq {
            control |= KVM_GUESTDBG_BLOCKIRQ;
        }
    }
    let debug = kvm_guest_debug { control, ..Default::default() };
    vcpu.set_guest_debug(&debug).map_err(io::Error::from)
}

/// Run the `KVM_RUN` loop until a `Halted` outcome, retrying transparently on `EINTR` (a signal
/// arriving mid-ioctl, e.g. from `pmu`'s armed overflow — this call site does not care why).
pub fn run_until_halted(
    vcpu: &mut VcpuFd,
    bus: &mut dyn Bus,
    time: &mut dyn TimeSource,
) -> Result<(), DeterminismHole> {
    loop {
        match run_one_exit(vcpu, bus, time)? {
            DispatchOutcome::Continue => continue,
            DispatchOutcome::Halted => return Ok(()),
            DispatchOutcome::SingleStepBoundary => continue, // no boundary walk in progress here
        }
    }
}

/// Drive exactly one `KVM_RUN` call to completion (retrying on `EINTR`) and dispatch its exit.
pub fn run_one_exit(
    vcpu: &mut VcpuFd,
    bus: &mut dyn Bus,
    time: &mut dyn TimeSource,
) -> Result<DispatchOutcome, DeterminismHole> {
    loop {
        match vcpu.run() {
            Ok(exit) => return dispatch_exit(convert_exit(exit), bus, time),
            Err(e) if e.errno() == libc::EINTR => continue,
            Err(e) => {
                // An ioctl failure that is not a benign signal interruption is itself a
                // determinism hole: the run loop has no deterministic value to serve for "KVM
                // itself errored" (specs/baud-vcpu.md §3's catch-all covers this too).
                return Err(DeterminismHole(format!("KVM_RUN ioctl failed: {e}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_bindings::kvm_regs;
    use kvm_ioctls::{MsrExitReason, ReadMsrExit, WriteMsrExit};

    // These construct `VcpuExit` values directly (plain structs/enums, no ioctl involved), so
    // `convert_exit` is exercised without any real `/dev/kvm` — only `cargo test --target
    // x86_64-unknown-linux-gnu` on an actual Linux host can run them, but they type-check via
    // `cargo check --target x86_64-unknown-linux-gnu --tests -p baud-vcpu` from this Windows box.

    #[test]
    fn modeled_exits_convert_without_becoming_unmodeled() {
        let mut buf = [0u8; 4];
        assert!(matches!(convert_exit(VcpuExit::IoIn(0x3f8, &mut buf)), Exit::IoIn(0x3f8, _)));
        assert!(matches!(convert_exit(VcpuExit::IoOut(0x3f8, &buf)), Exit::IoOut(0x3f8, _)));
        assert!(matches!(convert_exit(VcpuExit::MmioRead(0x1000, &mut buf)), Exit::MmioRead(0x1000, _)));
        assert!(matches!(convert_exit(VcpuExit::MmioWrite(0x1000, &buf)), Exit::MmioWrite(0x1000, _)));
        assert!(matches!(convert_exit(VcpuExit::Hlt), Exit::Hlt));
        assert!(matches!(convert_exit(VcpuExit::Shutdown), Exit::Shutdown));
        assert!(matches!(
            convert_exit(VcpuExit::Debug(unsafe { std::mem::zeroed() })),
            Exit::Debug
        ));
    }

    #[test]
    fn rdmsr_and_wrmsr_convert_and_clear_the_error_flag() {
        let mut error = 1u8; // start "failed" to prove convert_exit clears it
        let mut data = 0u64;
        let read = ReadMsrExit { error: &mut error, reason: MsrExitReason::Unknown, index: 0x10, data: &mut data };
        match convert_exit(VcpuExit::X86Rdmsr(read)) {
            Exit::Rdmsr(0x10, _) => {}
            other => panic!("expected Exit::Rdmsr, got {other:?}"),
        }
        assert_eq!(error, 0, "a served RDMSR must never leave the #GP error flag set");

        let mut error2 = 1u8;
        let write = WriteMsrExit { error: &mut error2, reason: MsrExitReason::Unknown, index: 0x10, data: 7 };
        match convert_exit(VcpuExit::X86Wrmsr(write)) {
            Exit::Wrmsr(0x10, 7) => {}
            other => panic!("expected Exit::Wrmsr, got {other:?}"),
        }
        assert_eq!(error2, 0);
    }

    #[test]
    fn unmodeled_exit_kinds_never_silently_pass_through() {
        assert!(matches!(convert_exit(VcpuExit::Unknown), Exit::Unmodeled("Unknown")));
        assert!(matches!(convert_exit(VcpuExit::Exception), Exit::Unmodeled("Exception")));
        assert!(matches!(convert_exit(VcpuExit::Unsupported(999)), Exit::Unmodeled("Unsupported")));
        assert!(matches!(
            convert_exit(VcpuExit::MemoryFault { flags: 0, gpa: 0, size: 0 }),
            Exit::Unmodeled("MemoryFault")
        ));
    }

    #[test]
    fn gp_regs_layout_matches_kvm_regs_field_order() {
        // Documents the mapping `pmu::gp_regs_from_kvm` relies on so a future field reorder in
        // kvm-bindings is caught here rather than silently scrambling ExecPoint comparisons.
        let regs = kvm_regs { rax: 1, rbx: 2, rcx: 3, rdx: 4, rsi: 5, rdi: 6, rbp: 7, rsp: 8,
            r8: 9, r9: 10, r10: 11, r11: 12, r12: 13, r13: 14, r14: 15, r15: 16, rip: 0, rflags: 0 };
        assert_eq!(pmu::gp_regs_from_kvm(&regs), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }
}
