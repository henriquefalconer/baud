// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real arm-early-then-single-step engine (specs/baud-vcpu.md §5): a `perf_event_open`
// retired-conditional-branch counter armed to overflow a margin before the target work-count,
// wired to a signal so the blocking `KVM_RUN` ioctl returns `EINTR` the instant it fires — the
// standard technique real VMMs use to interrupt a blocking vCPU-thread ioctl from a timer/signal
// source (`kvm_run.immediate_exit`, documented in the kernel's KVM API docs, is written from the
// handler for the same reason: it also makes any in-flight or next `KVM_RUN` return promptly).
//
// SCOPE NOTE (read before wiring this into baud-multiverse): `arm_signal_delivery` below targets
// the counter's overflow signal at this *process* via plain `F_SETOWN`, not at this specific
// *thread* via `F_SETOWN_EX(F_OWNER_TID, ...)`. That is correct as long as the vCPU thread is the
// only thread that can receive the signal (true for a single-threaded harness; NOT automatically
// true once specs/baud-multiverse.md §3.1's "one VMM thread + one vCPU thread" both exist and
// both have the signal unblocked). Revisit with `F_SETOWN_EX`/`F_OWNER_TID` when that thread
// model lands. Like the rest of `linux/`, this is type-checked via `cargo check --target
// x86_64-unknown-linux-gnu -p baud-vcpu` but not yet exercised on real KVM+perf hardware.

use super::convert_exit;
use crate::boundary::{ExecPoint, PmuStepper};
use crate::{dispatch_exit, Bus, DispatchOutcome, TimeSource};
use kvm_bindings::kvm_regs;
use kvm_ioctls::VcpuFd;
use perf_event::events::Hardware;
use perf_event::{Builder, Counter};
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Once;

/// The signal used to interrupt `KVM_RUN` when the armed branch-counter overflow fires. Installed
/// with `SA_SIGINFO` and no `SA_RESTART` so a blocking ioctl actually returns `EINTR` rather than
/// being transparently retried by the kernel (specs/baud-vcpu.md §5 step 2's "sloppy early exit").
const OVERFLOW_SIGNAL: libc::c_int = libc::SIGIO;

/// `fcntl(2)`'s `F_SETSIG` — not exposed by the `libc` crate for the glibc/x86_64 target, but a
/// stable value across every Linux architecture except a handful of legacy ports (see
/// `include/uapi/asm-generic/fcntl.h` in the kernel source).
const F_SETSIG: libc::c_int = 10;

static SIGNAL_HANDLER_INSTALLED: Once = Once::new();
static OVERFLOW_FIRED: AtomicBool = AtomicBool::new(false);
/// Points at the live `kvm_run.immediate_exit` byte for the vCPU currently armed, published by
/// [`LinuxPmuStepper::arm_overflow`].
static IMMEDIATE_EXIT_PTR: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn on_branch_overflow(_sig: libc::c_int, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // Async-signal-safe: one atomic store plus at most one volatile byte write — no allocation,
    // no locking, no syscalls (specs/baud-vcpu.md §5's boundary engine must never itself become a
    // nondeterminism source; a handler that could block or fail unpredictably would be one).
    OVERFLOW_FIRED.store(true, Ordering::SeqCst);
    let ptr = IMMEDIATE_EXIT_PTR.load(Ordering::SeqCst);
    if !ptr.is_null() {
        // SAFETY: `ptr` was published by `arm_overflow` from a `&mut kvm_run` borrowed for the
        // owning `LinuxPmuStepper`'s whole lifetime; the target byte stays valid for as long as
        // this handler can fire (the stepper is dropped, or re-armed, before its vCPU's mmap is
        // ever unmapped).
        unsafe { std::ptr::write_volatile(ptr, 1) };
    }
}

fn ensure_signal_handler_installed() {
    SIGNAL_HANDLER_INSTALLED.call_once(|| unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_branch_overflow as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(OVERFLOW_SIGNAL, &action, std::ptr::null_mut());
    });
}

/// Route `counter`'s overflow to [`OVERFLOW_SIGNAL`] (see the scope note above).
fn arm_signal_delivery(counter: &Counter) -> io::Result<()> {
    let fd = counter.as_raw_fd();
    unsafe {
        let pid = libc::getpid();
        if libc::fcntl(fd, libc::F_SETOWN, pid) < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, F_SETSIG, OVERFLOW_SIGNAL) < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_ASYNC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// All general-purpose registers in [`ExecPoint::gp_regs`]'s fixed order (RAX..R15). A free
/// function (not a method) so `linux::mod`'s test can pin the field order down without needing a
/// live `LinuxPmuStepper`.
pub fn gp_regs_from_kvm(r: &kvm_regs) -> [u64; 16] {
    [
        r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rbp, r.rsp, r.r8, r.r9, r.r10, r.r11, r.r12,
        r.r13, r.r14, r.r15,
    ]
}

/// The real [`PmuStepper`]: drives one vCPU's `KVM_RUN` loop, arming a `perf_event_open` branch
/// counter and single-stepping via `KVM_SET_GUEST_DEBUG` to land injection at an exact boundary
/// (specs/baud-vcpu.md §5). Borrows the same `Bus`/`TimeSource` the ordinary run loop uses, so
/// exits taken while arming/stepping toward the injection point are still served deterministically
/// rather than skipped.
pub struct LinuxPmuStepper<'vcpu, 'io> {
    vcpu: &'vcpu mut VcpuFd,
    bus: &'io mut dyn Bus,
    time: &'io mut dyn TimeSource,
    counter: Option<Counter>,
    /// The cumulative RCB the moment the current `counter` was armed — `perf_event`'s raw count
    /// restarts at 0 per file descriptor, so `baseline_rcb + counter.read()` is the true total.
    baseline_rcb: u64,
}

impl<'vcpu, 'io> LinuxPmuStepper<'vcpu, 'io> {
    pub fn new(vcpu: &'vcpu mut VcpuFd, bus: &'io mut dyn Bus, time: &'io mut dyn TimeSource) -> Self {
        ensure_signal_handler_installed();
        LinuxPmuStepper { vcpu, bus, time, counter: None, baseline_rcb: 0 }
    }

    fn current_rcb(&mut self) -> io::Result<u64> {
        match &mut self.counter {
            Some(c) => Ok(self.baseline_rcb + c.read()?),
            None => Ok(self.baseline_rcb),
        }
    }

    fn read_point(&mut self) -> io::Result<ExecPoint> {
        let regs = self.vcpu.get_regs().map_err(io::Error::from)?;
        let rcb = self.current_rcb()?;
        Ok(ExecPoint { rip: regs.rip, gp_regs: gp_regs_from_kvm(&regs), rcb, rcx: None, stack_checksum: None })
    }
}

impl<'vcpu, 'io> PmuStepper for LinuxPmuStepper<'vcpu, 'io> {
    type Error = io::Error;

    fn arm_overflow(&mut self, armed_target: u64) -> io::Result<()> {
        let baseline = self.current_rcb()?;
        let remaining = armed_target.saturating_sub(baseline).max(1);

        let mut builder = Builder::new().kind(Hardware::BRANCH_INSTRUCTIONS);
        builder.sample_period(remaining);
        builder.wakeup_events(1);
        let mut counter = builder.build()?;
        arm_signal_delivery(&counter)?;
        counter.enable()?;

        OVERFLOW_FIRED.store(false, Ordering::SeqCst);
        let run = self.vcpu.get_kvm_run();
        run.immediate_exit = 0;
        let ptr: *mut u8 = &mut run.immediate_exit as *mut u8;
        IMMEDIATE_EXIT_PTR.store(ptr, Ordering::SeqCst);

        self.counter = Some(counter);
        self.baseline_rcb = baseline;
        Ok(())
    }

    fn run_until_exit(&mut self) -> io::Result<()> {
        loop {
            if OVERFLOW_FIRED.load(Ordering::SeqCst) {
                return Ok(());
            }
            match self.vcpu.run() {
                Ok(exit) => match dispatch_exit(convert_exit(exit), self.bus, self.time) {
                    Ok(DispatchOutcome::Continue) => continue,
                    Ok(DispatchOutcome::SingleStepBoundary) => continue,
                    Ok(DispatchOutcome::Halted) => {
                        return Err(io::Error::other("guest halted while armed for interrupt injection"))
                    }
                    Err(hole) => return Err(io::Error::other(hole.to_string())),
                },
                Err(e) if e.errno() == libc::EINTR => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn current_point(&mut self) -> ExecPoint {
        // Trait is infallible here (mirrors specs/baud-vcpu.md §5's pseudocode); a register/
        // counter read failure is exceedingly unlikely immediately after a successful exit and
        // is not itself the guest's fault, so this reports the last-known RCB rather than
        // panicking a determinism-critical loop. `step`/`arm_overflow` surface real I/O errors.
        self.read_point().unwrap_or(ExecPoint {
            rip: 0,
            gp_regs: [0; 16],
            rcb: self.baseline_rcb,
            rcx: None,
            stack_checksum: None,
        })
    }

    fn step(&mut self) -> io::Result<ExecPoint> {
        super::set_singlestep(self.vcpu, true, true)?;
        let result = loop {
            match self.vcpu.run() {
                Ok(exit) => match dispatch_exit(convert_exit(exit), self.bus, self.time) {
                    Ok(DispatchOutcome::Continue) => continue,
                    Ok(DispatchOutcome::SingleStepBoundary) => break Ok(()),
                    Ok(DispatchOutcome::Halted) => {
                        break Err(io::Error::other("guest halted mid single-step boundary walk"))
                    }
                    Err(hole) => break Err(io::Error::other(hole.to_string())),
                },
                Err(e) if e.errno() == libc::EINTR => continue,
                Err(e) => break Err(e.into()),
            }
        };
        super::set_singlestep(self.vcpu, false, false)?;
        result?;
        self.read_point()
    }

    fn ready_for_interrupt_injection(&mut self) -> bool {
        self.vcpu.get_kvm_run().ready_for_interrupt_injection != 0
    }

    fn request_interrupt_window(&mut self) -> io::Result<()> {
        self.vcpu.get_kvm_run().request_interrupt_window = 1;
        Ok(())
    }

    fn run_until_irq_window(&mut self) -> io::Result<()> {
        loop {
            match self.vcpu.run() {
                Ok(exit) => match dispatch_exit(convert_exit(exit), self.bus, self.time) {
                    Ok(DispatchOutcome::Continue) => {
                        if self.vcpu.get_kvm_run().ready_for_interrupt_injection != 0 {
                            return Ok(());
                        }
                        continue;
                    }
                    Ok(DispatchOutcome::SingleStepBoundary) => continue,
                    Ok(DispatchOutcome::Halted) => {
                        return Err(io::Error::other("guest halted waiting for an interrupt window"))
                    }
                    Err(hole) => return Err(io::Error::other(hole.to_string())),
                },
                Err(e) if e.errno() == libc::EINTR => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn inject(&mut self, vector: u8) -> io::Result<()> {
        let mut events = self.vcpu.get_vcpu_events().map_err(io::Error::from)?;
        events.interrupt.injected = 1;
        events.interrupt.nr = vector;
        events.interrupt.soft = 0;
        self.vcpu.set_vcpu_events(&events).map_err(io::Error::from)
    }
}
