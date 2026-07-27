// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Interrupt injection at an exact instruction boundary (specs/baud-vcpu.md §5,
// specs/baud-multiverse.md §5): arm the branch counter a margin before the target work-count,
// take the early (sloppy) exit, single-step to the exact point, confirm the vCPU is injectable,
// then inject. `at_point`/`ExecPoint` name a point by (PC + all GP registers + RCB), extended
// with RCX (for `rep`-prefixed loops) and a stack checksum only when a bare tuple collides.
//
// The state machine here (`inject_at`) is generic over [`PmuStepper`] so it is testable without
// real KVM/perf hardware — `linux::LinuxPmuStepper` is the real implementation, driven by a
// perf-event branch counter and `KVM_SET_GUEST_DEBUG` (cfg-gated, type-checked via cross-compile
// per CLAUDE.md, not yet exercised on real silicon).

/// How many retired-conditional-branches before the target to arm the counter's overflow at.
/// Sized to comfortably survive a handful of single-step exits' worth of scheduling jitter
/// without risking overshoot (specs/baud-vcpu.md §5 step 1, "arm early").
pub const MARGIN: u64 = 64;

/// The identity of one instruction boundary the vCPU passed through: what
/// `timer_tick_lands_at_identical_instruction` compares across two independent runs
/// (specs/baud-vcpu.md §6). `rcx`/`stack_checksum` are populated only when needed to
/// disambiguate — a bare `(rip, gp_regs, rcb)` collision (specs/baud-vcpu.md §5's `at_point`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecPoint {
    /// The instruction pointer (RIP).
    pub rip: u64,
    /// All general-purpose registers, in a fixed order (RAX..R15 on x86_64).
    pub gp_regs: [u64; 16],
    /// Retired-conditional-branch count (the work-clock unit; never raw instruction count —
    /// specs/baud-multiverse.md §3's "Time" row forbids that double-counting source).
    pub rcb: u64,
    /// RCX, included when the guest is mid a `rep`-prefixed string instruction (the same RIP
    /// repeats every iteration; RCX is what actually advances).
    pub rcx: Option<u64>,
    /// A checksum of a bounded stack window, included only when `(rip, gp_regs, rcb)` collided
    /// with another point and needs a tie-breaker.
    pub stack_checksum: Option<u64>,
}

impl ExecPoint {
    /// Does this point already carry enough identity to be unambiguous against `other`, or does
    /// it need the stack-checksum tie-breaker? (`rcx` is orthogonal — always included when the
    /// guest is genuinely mid-`rep`, never added just to disambiguate.)
    pub fn collides_without_stack_checksum(&self, other: &ExecPoint) -> bool {
        self.stack_checksum.is_none()
            && other.stack_checksum.is_none()
            && self.rip == other.rip
            && self.gp_regs == other.gp_regs
            && self.rcb == other.rcb
            && self.rcx == other.rcx
    }
}

/// The hardware seam `inject_at` drives. Each method is one step of specs/baud-vcpu.md §5's
/// arm-early-then-single-step engine; a real implementation talks to `perf_event_open` +
/// `KVM_SET_GUEST_DEBUG` + `KVM_INTERRUPT`, a test implementation scripts a fixed point sequence.
pub trait PmuStepper {
    type Error;

    /// Arm the branch-counter overflow to fire at `armed_target` retired conditional branches
    /// (specs/baud-vcpu.md §5 step 1 — always `target_rcb - MARGIN`, computed by `inject_at`).
    fn arm_overflow(&mut self, armed_target: u64) -> Result<(), Self::Error>;
    /// Re-enter `KVM_RUN` until the armed overflow fires (the "sloppy early exit", step 2).
    fn run_until_exit(&mut self) -> Result<(), Self::Error>;
    /// The vCPU's current boundary identity, valid immediately after `run_until_exit` or `step`.
    /// `&mut self` (not `&self`): a real implementation reads live registers/counter state
    /// through the same `&mut VcpuFd`/`Counter` handles every other step here uses.
    fn current_point(&mut self) -> ExecPoint;
    /// Single-step exactly one instruction under `KVM_GUESTDBG_SINGLESTEP | BLOCKIRQ` and report
    /// the point landed on (step 3).
    fn step(&mut self) -> Result<ExecPoint, Self::Error>;
    /// `KVM_GET_VCPU_EVENTS`' `ready_for_interrupt_injection` (step 4).
    fn ready_for_interrupt_injection(&mut self) -> bool;
    /// Set `request_interrupt_window` and re-enter until the window opens (step 4's fallback).
    fn request_interrupt_window(&mut self) -> Result<(), Self::Error>;
    fn run_until_irq_window(&mut self) -> Result<(), Self::Error>;
    /// `KVM_INTERRUPT` / `KVM_SET_VCPU_EVENTS` (step 5).
    fn inject(&mut self, vector: u8) -> Result<(), Self::Error>;
    /// Has the guest already halted (`Hlt`/`Shutdown`) at some point during arming/stepping toward
    /// the target? A guest whose own natural halt falls *before* the next scheduled tick is not a
    /// determinism hole — it is the ordinary end of a periodic-timer-driven run (todo.md §14's
    /// "wire H4 into the boot path": nothing upstream of one `inject_at` call knows in advance how
    /// many ticks a real kernel survives before its own shutdown) — so `inject_at` checks this
    /// after every step that could have observed a halt exit, instead of erroring.
    fn is_halted(&self) -> bool;

    /// Has the supervisor cancelled this run (`baud_multiverse::linux::Multiverse::
    /// set_cancel_flag`)? Called by [`inject_at`]/[`run_to_events`] once per single-step iteration,
    /// which is the only place either of them spends unbounded time in: one call to
    /// [`run_until_exit`](Self::run_until_exit) can sit inside a single multi-second `KVM_RUN`
    /// (a real implementation breaks *that* out with a signal — `linux::watchdog::CancelKicker`),
    /// but the walk that follows it is a userspace loop that would otherwise keep single-stepping
    /// a guest whose caller has already gone away.
    ///
    /// `Err` (rather than a `bool`) because the implementor's own [`Error`](Self::Error) type is
    /// the only error currency these generic functions have; the real implementation returns
    /// `linux::cancelled_error()`, which its caller maps back to
    /// `baud_vcpu::RunLoopError::Cancelled` — never to a determinism hole, which cancellation is
    /// not. The default `Ok(())` means a stepper that knows nothing about cancellation (every test
    /// double, and every caller that never installed a flag) behaves exactly as it did before this
    /// method existed: no atomic access, no branch that can ever be taken.
    fn check_cancelled(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// What [`inject_at`] actually managed to do: land the injection (carrying the [`ExecPoint`] it
/// landed on, compared across runs by `timer_tick_lands_at_identical_instruction`), or discover
/// the guest halted on its own before the target boundary was ever reached (carrying the last
/// observed point, for diagnostics — no interrupt was injected in this case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectOutcome {
    Injected(ExecPoint),
    Halted(ExecPoint),
}

impl InjectOutcome {
    /// The [`ExecPoint`] either variant carries — the landing point whether or not injection
    /// actually happened.
    pub fn point(&self) -> &ExecPoint {
        match self {
            InjectOutcome::Injected(p) | InjectOutcome::Halted(p) => p,
        }
    }

    pub fn was_injected(&self) -> bool {
        matches!(self, InjectOutcome::Injected(_))
    }
}

/// What [`run_to_events`] actually observed: the guest is still running, landed exactly on
/// `target_rcb` (carrying the [`ExecPoint`], compared across runs the same way
/// [`InjectOutcome::Injected`] is), or it halted on its own before ever reaching that boundary
/// (carrying the last observed point — mirrors [`InjectOutcome::Halted`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunToEventsOutcome {
    Reached(ExecPoint),
    Halted(ExecPoint),
}

impl RunToEventsOutcome {
    /// The [`ExecPoint`] either variant carries — the landing point whether or not the target was
    /// actually reached.
    pub fn point(&self) -> &ExecPoint {
        match self {
            RunToEventsOutcome::Reached(p) | RunToEventsOutcome::Halted(p) => p,
        }
    }

    pub fn was_reached(&self) -> bool {
        matches!(self, RunToEventsOutcome::Reached(_))
    }
}

/// Drive `stepper` to land exactly on `target_rcb` retired conditional branches without injecting
/// anything — the timed-exit fingerprint's "stop at exactly N" primitive (specs/baud-ubuntu.md §6,
/// specs/baud-fingerprint.md §4 step 1). Shares [`inject_at`]'s arm-early-then-single-step
/// machinery (steps 1-3: arm a margin short, take the sloppy early exit, single-step the
/// remainder) but never reaches its injection steps (4-5) — a fingerprint capture must observe the
/// guest, not perturb it.
pub fn run_to_events<S: PmuStepper>(
    stepper: &mut S,
    target_rcb: u64,
) -> Result<RunToEventsOutcome, S::Error> {
    let armed_target = target_rcb.saturating_sub(MARGIN);
    stepper.arm_overflow(armed_target)?;
    stepper.run_until_exit()?;
    if stepper.is_halted() {
        return Ok(RunToEventsOutcome::Halted(stepper.current_point()));
    }

    let mut point = stepper.current_point();
    while point.rcb < target_rcb {
        // Same per-step supervisory check `inject_at`'s walk makes, for the same reason — see
        // `PmuStepper::check_cancelled`.
        stepper.check_cancelled()?;
        point = stepper.step()?;
        if stepper.is_halted() {
            return Ok(RunToEventsOutcome::Halted(point));
        }
    }
    Ok(RunToEventsOutcome::Reached(point))
}

/// Drive `stepper` to land the injection of `vector` at exactly `target_rcb` retired conditional
/// branches, per specs/baud-vcpu.md §5 — or discover the guest halted on its own first (see
/// [`PmuStepper::is_halted`]'s doc). Returns the outcome, so callers can compare the landed
/// [`ExecPoint`] against another run's tuple (`timer_tick_lands_at_identical_instruction`) when
/// injection happened, or handle a graceful halt without treating it as an error.
pub fn inject_at<S: PmuStepper>(
    stepper: &mut S,
    target_rcb: u64,
    vector: u8,
) -> Result<InjectOutcome, S::Error> {
    let armed_target = target_rcb.saturating_sub(MARGIN);
    stepper.arm_overflow(armed_target)?;
    stepper.run_until_exit()?;
    if stepper.is_halted() {
        return Ok(InjectOutcome::Halted(stepper.current_point()));
    }

    let mut point = stepper.current_point();
    while point.rcb < target_rcb {
        // The one unbounded userspace loop in this engine: a tick whose target is thousands of
        // branches away single-steps thousands of times here, so a run whose caller has gone away
        // stops between two steps rather than at the end of the tick (`check_cancelled`'s doc).
        stepper.check_cancelled()?;
        point = stepper.step()?;
        if stepper.is_halted() {
            return Ok(InjectOutcome::Halted(point));
        }
    }

    if !stepper.ready_for_interrupt_injection() {
        stepper.request_interrupt_window()?;
        stepper.run_until_irq_window()?;
        if stepper.is_halted() {
            return Ok(InjectOutcome::Halted(stepper.current_point()));
        }
    }
    stepper.inject(vector)?;
    Ok(InjectOutcome::Injected(point))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted vCPU: `run_until_exit` jumps straight to `start_rcb`, then `step()` advances
    /// RCB by one and RIP by a fixed stride each call, exactly like a real single-stepped guest
    /// loop. `injectable_after` models `ready_for_interrupt_injection` only becoming true after N
    /// extra windows, exercising the request-window fallback path.
    struct ScriptedStepper {
        start_rcb: u64,
        rcb: u64,
        rip_base: u64,
        rip_stride: u64,
        armed: Option<u64>,
        windows_opened: u32,
        injectable_after_windows: u32,
        injected: Option<(u8, ExecPoint)>,
        steps_taken: u32,
        /// When `Some(r)`, `step()` marks the guest halted the moment its `rcb` reaches `r` —
        /// models a guest whose own natural halt falls before the requested target boundary.
        halt_at_rcb: Option<u64>,
        halted: bool,
        /// When `Some(n)`, `check_cancelled` starts reporting cancellation once `n` steps have
        /// been taken — models the supervisor's flag being set mid-walk by another thread.
        cancel_after_steps: Option<u32>,
    }

    impl ScriptedStepper {
        fn new(start_rcb: u64, injectable_after_windows: u32) -> Self {
            ScriptedStepper {
                start_rcb,
                rcb: 0,
                rip_base: 0x1000,
                rip_stride: 4,
                armed: None,
                windows_opened: 0,
                injectable_after_windows,
                injected: None,
                steps_taken: 0,
                halt_at_rcb: None,
                halted: false,
                cancel_after_steps: None,
            }
        }

        fn point_at(&self, rcb: u64) -> ExecPoint {
            let mut gp_regs = [0u64; 16];
            gp_regs[0] = rcb; // a stand-in "register" that also tracks progress deterministically
            ExecPoint {
                rip: self.rip_base + rcb * self.rip_stride,
                gp_regs,
                rcb,
                rcx: None,
                stack_checksum: None,
            }
        }
    }

    impl PmuStepper for ScriptedStepper {
        type Error = &'static str;

        fn arm_overflow(&mut self, armed_target: u64) -> Result<(), Self::Error> {
            self.armed = Some(armed_target);
            Ok(())
        }

        fn run_until_exit(&mut self) -> Result<(), Self::Error> {
            let armed = self.armed.ok_or("arm_overflow was never called")?;
            // The "sloppy early exit": lands somewhere at-or-before the armed target, never past
            // it, matching the real overflow semantics (fires once >= armed_target retire).
            self.rcb = armed.min(self.start_rcb);
            Ok(())
        }

        fn current_point(&mut self) -> ExecPoint {
            self.point_at(self.rcb)
        }

        fn step(&mut self) -> Result<ExecPoint, Self::Error> {
            self.rcb += 1;
            self.steps_taken += 1;
            if let Some(halt_at) = self.halt_at_rcb {
                if self.rcb >= halt_at {
                    self.halted = true;
                }
            }
            Ok(self.point_at(self.rcb))
        }

        fn ready_for_interrupt_injection(&mut self) -> bool {
            self.windows_opened >= self.injectable_after_windows
        }

        fn request_interrupt_window(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn run_until_irq_window(&mut self) -> Result<(), Self::Error> {
            // `inject_at` calls this exactly once (specs/baud-vcpu.md §5 step 4 is not a retry
            // loop at that call site) — so, like a real implementation backed by `KVM_RUN`, this
            // must itself loop until the window is actually open before returning.
            while self.windows_opened < self.injectable_after_windows {
                self.windows_opened += 1;
            }
            Ok(())
        }

        fn inject(&mut self, vector: u8) -> Result<(), Self::Error> {
            if !self.ready_for_interrupt_injection() {
                return Err("injected while not ready_for_interrupt_injection");
            }
            self.injected = Some((vector, self.current_point()));
            Ok(())
        }

        fn is_halted(&self) -> bool {
            self.halted
        }

        fn check_cancelled(&self) -> Result<(), Self::Error> {
            match self.cancel_after_steps {
                Some(n) if self.steps_taken >= n => Err("cancelled"),
                _ => Ok(()),
            }
        }
    }

    #[test]
    fn injects_exactly_at_target_rcb() {
        let mut stepper = ScriptedStepper::new(1_000, 0);
        let outcome = inject_at(&mut stepper, 1_000, 42).expect("injection must succeed");
        assert!(outcome.was_injected());
        assert_eq!(outcome.point().rcb, 1_000);
        assert_eq!(stepper.injected.as_ref().unwrap().0, 42);
        assert_eq!(stepper.injected.as_ref().unwrap().1.rcb, 1_000);
        // Armed strictly before the target by MARGIN, then stepped the remainder one at a time.
        assert_eq!(stepper.armed, Some(1_000 - MARGIN));
        assert_eq!(stepper.steps_taken as u64, MARGIN);
    }

    #[test]
    fn falls_back_to_interrupt_window_when_not_immediately_injectable() {
        let mut stepper = ScriptedStepper::new(500, 2);
        let outcome = inject_at(&mut stepper, 500, 7).expect("injection must succeed");
        assert!(outcome.was_injected());
        assert_eq!(outcome.point().rcb, 500);
        assert_eq!(stepper.windows_opened, 2);
        assert!(stepper.ready_for_interrupt_injection());
    }

    /// specs/baud-vcpu.md §6 `timer_tick_lands_at_identical_instruction`: two independent
    /// "runs" (fresh scripted steppers) driven to the same target RCB land on identical tuples.
    #[test]
    fn identical_target_yields_identical_injection_tuple_across_runs() {
        let mut run_a = ScriptedStepper::new(10_000, 0);
        let mut run_b = ScriptedStepper::new(10_000, 0);
        let outcome_a = inject_at(&mut run_a, 10_000, 99).unwrap();
        let outcome_b = inject_at(&mut run_b, 10_000, 99).unwrap();
        assert_eq!(outcome_a, outcome_b);
    }

    #[test]
    fn small_margin_still_lands_exactly_when_start_is_already_at_target() {
        // start_rcb == target_rcb: run_until_exit already overshoots-to-the-target under this
        // stepper's model, and the while-loop takes zero extra steps — still exact.
        let mut stepper = ScriptedStepper::new(50, 0);
        let outcome = inject_at(&mut stepper, 50, 1).unwrap();
        assert_eq!(outcome.point().rcb, 50);
    }

    /// The graceful-halt path this iteration adds (todo.md §14, "wire H4 into the boot path"): a
    /// guest whose own natural halt falls before the requested target boundary must be reported as
    /// [`InjectOutcome::Halted`], never as an error and never injected into.
    #[test]
    fn reports_halted_instead_of_injecting_when_guest_halts_before_target() {
        let mut stepper = ScriptedStepper::new(500, 0);
        stepper.halt_at_rcb = Some(510); // halts partway through the single-step walk to 1_000
        let outcome = inject_at(&mut stepper, 1_000, 42).expect("a graceful halt must not surface as an error");
        assert_eq!(outcome, InjectOutcome::Halted(stepper.point_at(510)));
        assert!(!outcome.was_injected());
        assert!(stepper.injected.is_none(), "must never inject once the guest has halted on its own");
    }

    /// [`run_to_events`]'s core contract, mirroring [`injects_exactly_at_target_rcb`] but without
    /// ever calling `inject`: it must land exactly on `target_rcb`, having armed the same
    /// `MARGIN`-short overflow and single-stepped the rest, without ever opening an interrupt
    /// window or injecting anything.
    #[test]
    fn reaches_exactly_target_rcb_without_injecting() {
        let mut stepper = ScriptedStepper::new(1_000, 0);
        let outcome = run_to_events(&mut stepper, 1_000).expect("run_to_events must succeed");
        assert!(outcome.was_reached());
        assert_eq!(outcome.point().rcb, 1_000);
        assert_eq!(stepper.armed, Some(1_000 - MARGIN));
        assert_eq!(stepper.steps_taken as u64, MARGIN);
        assert!(stepper.injected.is_none(), "run_to_events must never inject anything");
        assert_eq!(stepper.windows_opened, 0, "run_to_events must never open an interrupt window");
    }

    /// The fingerprint's own determinism proof in miniature (specs/baud-fingerprint.md §8's
    /// `timed_exit_fingerprint_is_stable`): two independent "runs" driven to the same target RCB
    /// land on an identical tuple.
    #[test]
    fn identical_target_yields_identical_run_to_events_tuple_across_runs() {
        let mut run_a = ScriptedStepper::new(10_000, 0);
        let mut run_b = ScriptedStepper::new(10_000, 0);
        let outcome_a = run_to_events(&mut run_a, 10_000).unwrap();
        let outcome_b = run_to_events(&mut run_b, 10_000).unwrap();
        assert_eq!(outcome_a, outcome_b);
    }

    /// A guest that halts on its own before the requested `target_rcb` must be reported as
    /// [`RunToEventsOutcome::Halted`], never as an error — the fingerprint-capture analogue of
    /// [`reports_halted_instead_of_injecting_when_guest_halts_before_target`].
    #[test]
    fn reports_halted_when_guest_halts_before_target_rcb() {
        let mut stepper = ScriptedStepper::new(500, 0);
        stepper.halt_at_rcb = Some(510);
        let outcome = run_to_events(&mut stepper, 1_000).expect("a graceful halt must not surface as an error");
        assert_eq!(outcome, RunToEventsOutcome::Halted(stepper.point_at(510)));
        assert!(!outcome.was_reached());
    }

    /// The (b) half of the supervisory-cancellation fix: the single-step walk is where a
    /// periodic-timer run spends nearly all of its time, and it used to have no cancellation check
    /// at all, so a cancelled run kept stepping to the end of the tick no matter what. A stepper
    /// that reports cancellation mid-walk must abort the walk right there — not inject, not run to
    /// the target.
    #[test]
    fn inject_at_stops_mid_walk_when_the_stepper_reports_cancellation() {
        let mut stepper = ScriptedStepper::new(1_000, 0);
        stepper.cancel_after_steps = Some(3); // the walk to 1_000 would otherwise take MARGIN steps
        let err = inject_at(&mut stepper, 1_000, 42).expect_err("a cancelled walk must not succeed");
        assert_eq!(err, "cancelled");
        assert_eq!(stepper.steps_taken, 3, "the walk must stop at the first check that reports cancellation");
        assert!(stepper.injected.is_none(), "a cancelled walk must never inject anything");
    }

    /// [`run_to_events`]'s half of the same fix — the fingerprint-capture walk is the identical
    /// unbounded loop.
    #[test]
    fn run_to_events_stops_mid_walk_when_the_stepper_reports_cancellation() {
        let mut stepper = ScriptedStepper::new(1_000, 0);
        stepper.cancel_after_steps = Some(2);
        let err = run_to_events(&mut stepper, 1_000).expect_err("a cancelled walk must not succeed");
        assert_eq!(err, "cancelled");
        assert_eq!(stepper.steps_taken, 2);
    }

    /// The determinism half: a stepper that never reports cancellation (the default
    /// `check_cancelled`, i.e. every caller that installed no flag) must take the identical number
    /// of steps and land on the identical point it did before the check existed —
    /// [`injects_exactly_at_target_rcb`] above pins the exact same numbers, so the two together
    /// prove the check added nothing observable.
    #[test]
    fn an_uncancelled_walk_is_unchanged_by_the_cancellation_check() {
        let mut stepper = ScriptedStepper::new(1_000, 0);
        assert!(stepper.cancel_after_steps.is_none());
        let outcome = inject_at(&mut stepper, 1_000, 42).expect("injection must succeed");
        assert_eq!(outcome.point().rcb, 1_000);
        assert_eq!(stepper.steps_taken as u64, MARGIN);
    }

    #[test]
    fn collides_without_stack_checksum_ignores_stack_when_absent() {
        let a = ExecPoint { rip: 1, gp_regs: [0; 16], rcb: 1, rcx: None, stack_checksum: None };
        let b = ExecPoint { rip: 1, gp_regs: [0; 16], rcb: 1, rcx: None, stack_checksum: None };
        assert!(a.collides_without_stack_checksum(&b));

        let c = ExecPoint { stack_checksum: Some(7), ..a.clone() };
        assert!(!a.collides_without_stack_checksum(&c));
    }
}
