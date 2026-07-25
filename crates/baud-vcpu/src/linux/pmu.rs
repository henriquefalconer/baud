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

use super::{convert_exit, write_enforced_rdtsc_result};
use crate::boundary::{ExecPoint, PmuStepper};
use crate::{dispatch_exit, Bus, DispatchOutcome, TimeSource};
use kvm_bindings::kvm_regs;
use kvm_ioctls::VcpuFd;
use perf_event::events::Hardware;
use perf_event::{Builder, Counter};
use std::io;

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
    /// The cumulative RCB `arm_overflow` was asked to overflow at — kept so [`run_until_exit`]
    /// can also poll [`current_rcb`] directly instead of trusting the overflow *signal* alone
    /// (see [`run_until_exit`]'s doc for why: a real PMU overflow interrupt occurring while the
    /// physical core is in VMX non-root/guest mode may not surface as a delivered signal on every
    /// host — found for real under this project's own nested-virtualized dev host, todo.md §14).
    ///
    /// [`run_until_exit`]: Self::run_until_exit
    /// [`current_rcb`]: Self::current_rcb
    poll_target: u64,
}

impl<'vcpu, 'io> LinuxPmuStepper<'vcpu, 'io> {
    pub fn new(vcpu: &'vcpu mut VcpuFd, bus: &'io mut dyn Bus, time: &'io mut dyn TimeSource) -> Self {
        LinuxPmuStepper { vcpu, bus, time, counter: None, baseline_rcb: 0, poll_target: 0 }
    }

    /// Anchor this stepper's own RCB space to an externally-known cumulative branch count (e.g.
    /// the caller's `WorkClock::current_rcb()`) instead of starting at `0`. This stepper's armed
    /// counter is a distinct `perf_event` fd from whatever counter feeds a caller's `TimeSource`
    /// (`arm_overflow` always resets its own file descriptor's count to 0 on creation), so without
    /// this a `target_rcb` computed in the caller's RCB space would land at entirely the wrong
    /// point relative to this stepper's counter. Must be called before [`PmuStepper::arm_overflow`]
    /// — it only takes effect while `counter` is still `None`.
    pub fn with_baseline_rcb(mut self, baseline_rcb: u64) -> Self {
        self.baseline_rcb = baseline_rcb;
        self
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

        // NOTE: `exclude_host(true)` would be the textbook "guest-filtered" fix here too, but is
        // non-functional on this dev host — see `LinuxBranchCounter::new`'s doc
        // (`baud-multiverse::linux::mod`) for why it was tried and reverted. This stepper's own
        // bookkeeping between `KVM_RUN` calls (this file) must therefore stay free of
        // data-dependent branching if `target_rcb` is to mean the same thing across two runs.
        let mut builder = Builder::new().kind(Hardware::BRANCH_INSTRUCTIONS);
        // `pinned(true)`: same fix as `crates/baud-host/src/linux.rs`'s `measure_fixed_loop_branches`
        // (todo.md §14/H3) — under this project's own nested-virtualized dev host, an unpinned
        // counter is occasionally multiplexed off the PMU for part of the measurement window,
        // undercounting by a small, run-to-run-varying amount. Without this, two boots of the
        // same image+tape landed an injected tick's `rip` identically but its `rcb` reading off
        // by ±1-2 — not genuine execution nondeterminism, just this counter losing the PMU.
        builder.pinned(true);
        let mut counter = builder.build()?;
        counter.enable()?;

        self.counter = Some(counter);
        self.baseline_rcb = baseline;
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
        loop {
            if self.current_rcb()? >= self.poll_target {
                return Ok(());
            }
            match self.vcpu.run() {
                Ok(exit) => match dispatch_exit(convert_exit(exit), self.bus, self.time) {
                    Ok(DispatchOutcome::Continue) => continue,
                    Ok(DispatchOutcome::SingleStepBoundary) => continue,
                    Ok(DispatchOutcome::ServeEnforcedRdtsc(value)) => {
                        match write_enforced_rdtsc_result(self.vcpu, value) {
                            Ok(()) => continue,
                            Err(e) => return Err(e),
                        }
                    }
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
                    Ok(DispatchOutcome::ServeEnforcedRdtsc(value)) => {
                        match write_enforced_rdtsc_result(self.vcpu, value) {
                            Ok(()) => continue,
                            Err(e) => break Err(e),
                        }
                    }
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
