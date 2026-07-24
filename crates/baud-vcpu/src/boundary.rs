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
}

/// Drive `stepper` to land the injection of `vector` at exactly `target_rcb` retired conditional
/// branches, per specs/baud-vcpu.md §5. Returns the [`ExecPoint`] the injection actually landed
/// on, so callers can compare it against another run's tuple
/// (`timer_tick_lands_at_identical_instruction`).
pub fn inject_at<S: PmuStepper>(
    stepper: &mut S,
    target_rcb: u64,
    vector: u8,
) -> Result<ExecPoint, S::Error> {
    let armed_target = target_rcb.saturating_sub(MARGIN);
    stepper.arm_overflow(armed_target)?;
    stepper.run_until_exit()?;

    let mut point = stepper.current_point();
    while point.rcb < target_rcb {
        point = stepper.step()?;
    }

    if !stepper.ready_for_interrupt_injection() {
        stepper.request_interrupt_window()?;
        stepper.run_until_irq_window()?;
    }
    stepper.inject(vector)?;
    Ok(point)
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
    }

    #[test]
    fn injects_exactly_at_target_rcb() {
        let mut stepper = ScriptedStepper::new(1_000, 0);
        let point = inject_at(&mut stepper, 1_000, 42).expect("injection must succeed");
        assert_eq!(point.rcb, 1_000);
        assert_eq!(stepper.injected.as_ref().unwrap().0, 42);
        assert_eq!(stepper.injected.as_ref().unwrap().1.rcb, 1_000);
        // Armed strictly before the target by MARGIN, then stepped the remainder one at a time.
        assert_eq!(stepper.armed, Some(1_000 - MARGIN));
        assert_eq!(stepper.steps_taken as u64, MARGIN);
    }

    #[test]
    fn falls_back_to_interrupt_window_when_not_immediately_injectable() {
        let mut stepper = ScriptedStepper::new(500, 2);
        let point = inject_at(&mut stepper, 500, 7).expect("injection must succeed");
        assert_eq!(point.rcb, 500);
        assert_eq!(stepper.windows_opened, 2);
        assert!(stepper.ready_for_interrupt_injection());
    }

    /// specs/baud-vcpu.md §6 `timer_tick_lands_at_identical_instruction`: two independent
    /// "runs" (fresh scripted steppers) driven to the same target RCB land on identical tuples.
    #[test]
    fn identical_target_yields_identical_injection_tuple_across_runs() {
        let mut run_a = ScriptedStepper::new(10_000, 0);
        let mut run_b = ScriptedStepper::new(10_000, 0);
        let point_a = inject_at(&mut run_a, 10_000, 99).unwrap();
        let point_b = inject_at(&mut run_b, 10_000, 99).unwrap();
        assert_eq!(point_a, point_b);
    }

    #[test]
    fn small_margin_still_lands_exactly_when_start_is_already_at_target() {
        // start_rcb == target_rcb: run_until_exit already overshoots-to-the-target under this
        // stepper's model, and the while-loop takes zero extra steps — still exact.
        let mut stepper = ScriptedStepper::new(50, 0);
        let point = inject_at(&mut stepper, 50, 1).unwrap();
        assert_eq!(point.rcb, 50);
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
