// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real arm-early-then-single-step engine (specs/baud-vcpu.md §5): a `perf_event_open`
// retired-conditional-branch counter armed a margin before the target work-count, polled after
// every real `KVM_RUN` exit until it passes that margin (the "sloppy early exit"), then
// single-stepped the remainder via `KVM_SET_GUEST_DEBUG` to land exactly on the target.
//
// HOST FINDING (todo.md §14 — read before reaching for `perf_event`'s SIGIO/`F_SETSIG` overflow
// signal instead of polling): an earlier version of this module wired the counter's overflow to a
// signal (the standard technique real VMMs use, per the KVM API docs' `kvm_run.immediate_exit`)
// so a blocking `KVM_RUN` would return `EINTR` the instant it fired. On this project's own
// nested-virtualized dev host (WSL2-on-Hyper-V, CLAUDE.md) this was actively harmful, not just
// unreliable: (1) a PMU overflow occurring while the physical core is in VMX non-root (guest)
// mode is frequently never recognized as a signal at all here — `counter.read()` still returns
// the correct raw count, proving the counter itself works, but no signal ever arrives; (2) when a
// signal *did* arrive, it forced a real VM exit at a wall-clock-driven instant with no relation to
// the guest's own deterministic instruction stream, making the observed landing point vary run to
// run for the *same* image+tape (caught by `timer_tick_lands_at_identical_instruction`); (3) a
// signal belonging to an already-superseded counter (a previous tick's) could arrive late and get
// misattributed to whichever counter was current by the time it landed, an even worse variant of
// the same problem. Polling avoids all three: the only thing that can move `current_rcb()` past
// `poll_target` is the guest's own deterministic execution, observed at real VM exits the guest
// itself causes (this crate's own fixtures that want to exercise this path must therefore produce
// a periodic real exit, e.g. a harmless forced PIO write — see
// `crates/baud-multiverse/tests/fixtures/timer-guest/BUILD.md`).

use super::{
    cancelled_error, reinject_ud, run_and_convert_rcb_bracketed, write_enforced_rdrand_result,
    write_enforced_rdseed_result, write_enforced_rdtsc_result, write_enforced_rdtscp_result,
    ConvertedExit,
};
use crate::boundary::{ExecPoint, PmuStepper};
use crate::{dispatch_exit, Bus, DispatchOutcome, Exit, TimeSource};
use kvm_bindings::kvm_regs;
use kvm_ioctls::VcpuFd;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Is the supervisor's cancellation flag installed *and* set? A free function taking the field by
/// reference, rather than only a `&self` method, because every `KVM_RUN` loop below needs to test
/// it on a path where a `&mut self.vcpu` reborrow is still live and `&self` would therefore be a
/// borrow conflict — each such loop clones the (cheap, usually `None`) `Option<Arc<_>>` into a
/// local once on entry and tests that instead.
fn flag_is_set(cancel: &Option<Arc<AtomicBool>>) -> bool {
    cancel.as_ref().is_some_and(|flag| flag.load(Ordering::SeqCst))
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

/// The real [`PmuStepper`]: drives one vCPU's `KVM_RUN` loop, polling the caller's own
/// `TimeSource::current_rcb` (backed by `WorkClock`'s single, long-lived `perf_event` branch
/// counter — todo.md §14 next-actions item 2(c)) and single-stepping via `KVM_SET_GUEST_DEBUG`
/// to land injection at an exact boundary (specs/baud-vcpu.md §5). This stepper used to own a
/// second, independent `perf_event` fd of its own, reconciled to the caller's via a baseline
/// (`with_baseline_rcb`) — found, by direct hardware instrumentation, to disagree with the
/// caller's fd by a small amount at the instant a target is judged crossed, since the two fds'
/// pause/resume epochs (`run_and_convert_rcb_bracketed`) were independent even though both
/// counted the identical hardware event on the identical thread. Reading `time.current_rcb()`
/// directly instead means there is only ever one pinned RCB fd for the whole boot, so there is,
/// by construction, no second epoch left to disagree with. Borrows the same `Bus`/`TimeSource`
/// the ordinary run loop uses, so exits taken while arming/stepping toward the injection point
/// are still served deterministically rather than skipped.
pub struct LinuxPmuStepper<'vcpu, 'io> {
    vcpu: &'vcpu mut VcpuFd,
    bus: &'io mut dyn Bus,
    time: &'io mut dyn TimeSource,
    /// The cumulative RCB `arm_overflow` was asked to overflow at — kept so [`run_until_exit`]
    /// can also poll [`current_rcb`] directly instead of trusting the overflow *signal* alone
    /// (see [`run_until_exit`]'s doc for why: a real PMU overflow interrupt occurring while the
    /// physical core is in VMX non-root/guest mode may not surface as a delivered signal on every
    /// host — found for real under this project's own nested-virtualized dev host, todo.md §14).
    ///
    /// [`run_until_exit`]: Self::run_until_exit
    /// [`current_rcb`]: Self::current_rcb
    poll_target: u64,
    /// Set the moment any real `KVM_RUN` call taken while arming/stepping toward the target
    /// observes the guest halt (`Hlt`/`Shutdown`) on its own — a real guest surviving an unknown
    /// number of periodic ticks before its own shutdown is the ordinary case (todo.md §14, "wire
    /// H4 into the boot path"), not a determinism hole, so this is reported via
    /// [`PmuStepper::is_halted`] instead of an error. Once set, every method here stops calling
    /// `KVM_RUN` again (a halted vCPU with no in-kernel irqchip and no way to become injectable
    /// again would otherwise risk blocking indefinitely on the next entry).
    halted: bool,
    /// The supervisor's cancellation flag, if [`with_cancel`](Self::with_cancel) installed one —
    /// `None` for every caller that did not, which is every caller that is not driving a run on
    /// behalf of a client that can disappear. Read at [`PmuStepper::check_cancelled`] (once per
    /// single-step iteration of the boundary walk) and after an `EINTR` from `KVM_RUN`; never
    /// inside an exit's dispatch, so it cannot perturb what the guest observes.
    cancel: Option<Arc<AtomicBool>>,
}

impl<'vcpu, 'io> LinuxPmuStepper<'vcpu, 'io> {
    pub fn new(vcpu: &'vcpu mut VcpuFd, bus: &'io mut dyn Bus, time: &'io mut dyn TimeSource) -> Self {
        LinuxPmuStepper { vcpu, bus, time, poll_target: 0, halted: false, cancel: None }
    }

    /// Install the supervisor's cancellation flag on this stepper (builder style, so the five-odd
    /// `LinuxPmuStepper::new(...)` call sites that do not want one are untouched). With a flag
    /// installed, an `EINTR` from `KVM_RUN` observed while it is set stops the run with
    /// `linux::cancelled_error` instead of transparently retrying the ioctl, and the boundary
    /// walk's [`PmuStepper::check_cancelled`] stops it between two single steps. `None` restores
    /// exactly the no-cancellation behaviour.
    pub fn with_cancel(mut self, cancel: Option<Arc<AtomicBool>>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Whether a flag is installed *and* set. `None` short-circuits with no atomic access at all.
    fn is_cancelled(&self) -> bool {
        flag_is_set(&self.cancel)
    }

    fn current_rcb(&mut self) -> u64 {
        self.time.current_rcb()
    }

    fn read_point(&mut self) -> io::Result<ExecPoint> {
        let regs = self.vcpu.get_regs().map_err(io::Error::from)?;
        let rcb = self.current_rcb();
        Ok(ExecPoint { rip: regs.rip, gp_regs: gp_regs_from_kvm(&regs), rcb, rcx: None, stack_checksum: None })
    }
}

impl<'vcpu, 'io> PmuStepper for LinuxPmuStepper<'vcpu, 'io> {
    type Error = io::Error;

    fn arm_overflow(&mut self, armed_target: u64) -> io::Result<()> {
        // No counter to create here any more (todo.md §14 next-actions item 2(c)): `current_rcb`
        // reads the caller's own `WorkClock`-backed `TimeSource` directly, the same single pinned
        // `perf_event` fd for the whole boot — see this stepper's own doc for why a second,
        // independently-epoched fd was removed rather than kept reconciled.
        self.poll_target = armed_target;
        Ok(())
    }

    /// Block until this stepper's counter has passed `armed_target` retired conditional branches
    /// (arming a margin short of the real injection target, specs/baud-vcpu.md §5 step 2's
    /// "sloppy early exit") — by polling [`current_rcb`](Self::current_rcb) against
    /// [`poll_target`](Self::poll_target) after every real `KVM_RUN` exit (see this module's own
    /// doc for why an overflow *signal* is deliberately not used here). The landing point this
    /// produces is a pure function of the guest's own deterministic execution trace: the first
    /// real VM exit at or after which `current_rcb()` reaches `poll_target`.
    /// `tests/fixtures/timer-guest/`'s payload periodically performs a harmless forced VM exit
    /// (`out 0x80, al`, absorbed by `OpenBusFallback`) so this poll always gets a chance to run.
    fn run_until_exit(&mut self) -> io::Result<()> {
        // See `flag_is_set`: hoisted into a local because the `EINTR` arm below sits inside a live
        // `&mut self.vcpu` reborrow. Free (a discriminant copy) in the overwhelmingly common
        // no-flag case.
        let cancel = self.cancel.clone();
        loop {
            if flag_is_set(&cancel) {
                return Err(cancelled_error());
            }
            if self.current_rcb() >= self.poll_target {
                return Ok(());
            }
            let exit = match run_and_convert_rcb_bracketed(self.vcpu, self.time) {
                Ok(ConvertedExit::Exit(exit)) => Ok(exit),
                // `converted` (a fieldless variant) borrows nothing further from `self.vcpu` past
                // this match, so this fresh `get_regs()` is not competing with any live borrow —
                // see `ConvertedExit`'s doc for why the same fetch cannot happen one level down,
                // inside `run_and_convert` itself.
                Ok(ConvertedExit::RdseedTrapNeedsRip) => {
                    self.vcpu.get_regs().map(|regs| Exit::RdseedEnforced { rip: regs.rip })
                }
                Err(e) => Err(e),
            };
            match exit {
                Ok(exit) => match dispatch_exit(exit, self.bus, self.time) {
                    Ok(DispatchOutcome::Continue) => continue,
                    Ok(DispatchOutcome::SingleStepBoundary) => continue,
                    Ok(DispatchOutcome::ServeEnforcedRdtsc(value)) => {
                        match write_enforced_rdtsc_result(self.vcpu, value) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdtscp { value, tsc_aux }) => {
                        match write_enforced_rdtscp_result(self.vcpu, value, tsc_aux) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdrand { gpr_index, value }) => {
                        match write_enforced_rdrand_result(self.vcpu, gpr_index, value) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdseed { rip, site, value }) => {
                        match write_enforced_rdseed_result(self.vcpu, rip, site, value) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ReinjectUd) => match reinject_ud(self.vcpu) {
                        Ok(()) => continue,
                        Err(e) => return Err(e),
                    },
                    Ok(DispatchOutcome::Halted) => {
                        // Stop calling KVM_RUN the instant a halt is observed — see `halted`'s doc.
                        self.halted = true;
                        return Ok(());
                    }
                    Err(hole) => return Err(io::Error::other(hole.to_string())),
                },
                // Any `KVM_RUN` failure observed while this run is cancelled — the `EINTR` its own
                // `CancelKicker`'s signal produces, or the `EAGAIN` a signal-interrupted entry can
                // also surface as — ends the run as cancelled, rather than retrying the very ioctl
                // that kick was sent to escape (and never as a determinism hole, which an
                // abandoned run is not). Unreachable unless a flag was installed *and* set.
                Err(_) if flag_is_set(&cancel) => return Err(cancelled_error()),
                Err(e) if e.errno() == libc::EINTR => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn current_point(&mut self) -> ExecPoint {
        // Trait is infallible here (mirrors specs/baud-vcpu.md §5's pseudocode); a register read
        // failure is exceedingly unlikely immediately after a successful exit and is not itself
        // the guest's fault, so this reports the last-known RCB (now infallible: `current_rcb`
        // just reads the caller's `TimeSource`, no fd of its own to fail) rather than panicking a
        // determinism-critical loop. `step`/`arm_overflow` surface real I/O errors.
        let rcb = self.current_rcb();
        self.read_point().unwrap_or(ExecPoint { rip: 0, gp_regs: [0; 16], rcb, rcx: None, stack_checksum: None })
    }

    /// Retire exactly one guest instruction under `KVM_GUESTDBG_SINGLESTEP | BLOCKIRQ`
    /// (specs/baud-vcpu.md §5 step 3) and report the point landed on.
    ///
    /// **The inner `continue`-on-`DispatchOutcome::Continue` loop is required for correctness, not
    /// a leak — do not "fix" it by returning to the caller after an I/O/MMIO/MSR exit.** todo.md
    /// §14.1 hypothesised that this loop was what let more than one instruction retire per call and
    /// so caused the filed `run_to_events`/`inject_at` landing-precision bug; direct instrumentation
    /// on real `/dev/kvm` disproved that (the real cause was the work-clock counter filtering out
    /// the CPL-0 guest entirely — see `baud_multiverse::linux::LinuxBranchCounter::new`), and the
    /// architecture says the loop must stay:
    ///
    /// * Every exit `dispatch_exit` resolves to `Continue` here is either taken at an instruction
    ///   boundary *before* the trapping instruction has retired, or is not an instruction event at
    ///   all. Per the Intel SDM (Vol. 3C §26.1.3 and §27.1), a VM exit caused by an I/O-bitmap trap
    ///   on `IN`/`OUT` is *fault-like*: it is taken before the instruction executes, and the saved
    ///   guest RIP still points at the `IN`/`OUT` itself. Same for an EPT-violation-triggered MMIO
    ///   exit, which is a fault on the memory operand. KVM finishes those instructions on the *next*
    ///   `KVM_RUN` entry, via `complete_userspace_io` (`kvm_fast_pio_out`/`complete_emulated_mmio`
    ///   → `kvm_skip_emulated_instruction`, which is also where RIP finally advances). `Rdmsr`/
    ///   `Wrmsr` complete the same way; `IrqWindowOpen` is a pure control signal with no instruction
    ///   attached.
    /// * Returning after such an exit would therefore hand the caller an [`ExecPoint`] that is not
    ///   an architectural instruction boundary at all: RIP would name an instruction whose
    ///   device-visible side effect this dispatch has *already* performed (`bus.pio_write`) but
    ///   which has not retired — precisely the state a fingerprint or a snapshot must never be
    ///   taken in.
    /// * That completion re-entry costs no work-clock time, so nothing is gained by cutting it
    ///   short: the trapping instruction retires at most one conditional branch in total no matter
    ///   how many `KVM_RUN` calls it took to get there.
    /// * The `ServeEnforced*`/`ReinjectUd` arms loop for the stronger reason the trait doc already
    ///   implies: the guest's trapped instruction is still waiting on the register value being
    ///   written, so it provably has not retired.
    fn step(&mut self) -> io::Result<ExecPoint> {
        super::set_singlestep(self.vcpu, true, true)?;
        let cancel = self.cancel.clone(); // see `run_until_exit`
        let result = loop {
            let exit = match run_and_convert_rcb_bracketed(self.vcpu, self.time) {
                Ok(ConvertedExit::Exit(exit)) => Ok(exit),
                Ok(ConvertedExit::RdseedTrapNeedsRip) => {
                    self.vcpu.get_regs().map(|regs| Exit::RdseedEnforced { rip: regs.rip })
                }
                Err(e) => Err(e),
            };
            match exit {
                Ok(exit) => match dispatch_exit(exit, self.bus, self.time) {
                    Ok(DispatchOutcome::Continue) => continue,
                    Ok(DispatchOutcome::SingleStepBoundary) => break Ok(()),
                    Ok(DispatchOutcome::ServeEnforcedRdtsc(value)) => {
                        match write_enforced_rdtsc_result(self.vcpu, value) {
                            Ok(()) => continue,
                            Err(e) => break Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdtscp { value, tsc_aux }) => {
                        match write_enforced_rdtscp_result(self.vcpu, value, tsc_aux) {
                            Ok(()) => continue,
                            Err(e) => break Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdrand { gpr_index, value }) => {
                        match write_enforced_rdrand_result(self.vcpu, gpr_index, value) {
                            Ok(()) => continue,
                            Err(e) => break Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdseed { rip, site, value }) => {
                        match write_enforced_rdseed_result(self.vcpu, rip, site, value) {
                            Ok(()) => continue,
                            Err(e) => break Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ReinjectUd) => match reinject_ud(self.vcpu) {
                        Ok(()) => continue,
                        Err(e) => break Err(e),
                    },
                    Ok(DispatchOutcome::Halted) => {
                        // See `halted`'s doc: stop single-stepping the instant a halt is observed,
                        // never keep driving KVM_RUN on an already-halted vCPU.
                        self.halted = true;
                        break Ok(());
                    }
                    Err(hole) => break Err(io::Error::other(hole.to_string())),
                },
                // See `run_until_exit`'s identical arm. `break` (not `return`) so this method's own
                // `set_singlestep(false)` still runs on the way out — a cancelled run must leave
                // the vCPU in the same state every other exit path leaves it in.
                Err(_) if flag_is_set(&cancel) => break Err(cancelled_error()),
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
        let cancel = self.cancel.clone(); // see `run_until_exit`
        loop {
            if flag_is_set(&cancel) {
                return Err(cancelled_error());
            }
            let exit = match run_and_convert_rcb_bracketed(self.vcpu, self.time) {
                Ok(ConvertedExit::Exit(exit)) => Ok(exit),
                Ok(ConvertedExit::RdseedTrapNeedsRip) => {
                    self.vcpu.get_regs().map(|regs| Exit::RdseedEnforced { rip: regs.rip })
                }
                Err(e) => Err(e),
            };
            match exit {
                Ok(exit) => match dispatch_exit(exit, self.bus, self.time) {
                    Ok(DispatchOutcome::Continue) => {
                        if self.vcpu.get_kvm_run().ready_for_interrupt_injection != 0 {
                            // `kvm_run.request_interrupt_window` is sticky exactly like
                            // `immediate_exit` (this module's earlier finding, todo.md §14): the
                            // kernel never clears it back to `0` on its own, so every later
                            // `KVM_RUN` on this vCPU — including a plain, non-stepper run loop
                            // with no idea what `IrqWindowOpen` even means — would otherwise keep
                            // exiting with `KVM_EXIT_IRQ_WINDOW_OPEN` every time interrupts become
                            // enabled, for the rest of the process's life, hitting the generic run
                            // loop's determinism-hole catch-all. Clear it the moment the window
                            // this request was actually asking for has opened.
                            self.vcpu.get_kvm_run().request_interrupt_window = 0;
                            return Ok(());
                        }
                        continue;
                    }
                    Ok(DispatchOutcome::SingleStepBoundary) => continue,
                    Ok(DispatchOutcome::ServeEnforcedRdtsc(value)) => {
                        match write_enforced_rdtsc_result(self.vcpu, value) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdtscp { value, tsc_aux }) => {
                        match write_enforced_rdtscp_result(self.vcpu, value, tsc_aux) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdrand { gpr_index, value }) => {
                        match write_enforced_rdrand_result(self.vcpu, gpr_index, value) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ServeEnforcedRdseed { rip, site, value }) => {
                        match write_enforced_rdseed_result(self.vcpu, rip, site, value) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(DispatchOutcome::ReinjectUd) => match reinject_ud(self.vcpu) {
                        Ok(()) => continue,
                        Err(e) => return Err(e),
                    },
                    Ok(DispatchOutcome::Halted) => {
                        // See `halted`'s doc: stop waiting for a window on an already-halted vCPU.
                        self.halted = true;
                        return Ok(());
                    }
                    Err(hole) => return Err(io::Error::other(hole.to_string())),
                },
                // See `run_until_exit`'s identical arm.
                Err(_) if flag_is_set(&cancel) => return Err(cancelled_error()),
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

    fn is_halted(&self) -> bool {
        self.halted
    }

    /// [`PmuStepper::check_cancelled`]'s real implementation: report the supervisor's flag to
    /// `boundary::inject_at`/`run_to_events`' single-step walk, so a cancelled run stops between
    /// two steps even if no signal ever had to be delivered.
    fn check_cancelled(&self) -> io::Result<()> {
        if self.is_cancelled() {
            return Err(cancelled_error());
        }
        Ok(())
    }
}
