// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The work-clock TimeSource (specs/baud-multiverse.md §4, todo.md §3.3): guest time is a function
// of work done, not wall-clock. `virtual_tsc = base + k * rcb`, where `rcb` is the
// retired-conditional-branch count read from a `BranchCounter` — the real source is a free-running
// `perf_event_open` counter (`linux::LinuxBranchCounter`); this module stays hardware-independent
// by taking the counter as a generic parameter, so the formula itself — and its
// determinism/monotonicity properties (`work_clock_is_monotone_and_reproducible`) — is unit-tested
// on this Windows dev machine with no perf/KVM at all, the same pattern `boundary.rs` uses for
// `PmuStepper`.
//
// Served MSRs (specs/baud-multiverse.md §4's "MSR filter" row, routed here by
// `linux::configure_msr_filter`'s `KVM_X86_SET_MSR_FILTER`):
//   IA32_TSC (0x10)           - the virtual TSC itself: `base + k * rcb`.
//   IA32_TSC_DEADLINE (0x6E0) - absorbed/served verbatim; landing the *interrupt* at the right RCB
//                               is `baud_vcpu::boundary::inject_at`'s job (specs/baud-vcpu.md §5),
//                               not this MSR — this just keeps the register's read-your-write value
//                               consistent for a guest that polls it.
//   IA32_TSC_AUX (0xC0000103) - absorbed/served verbatim (RDTSCP's auxiliary value).

use baud_vcpu::TimeSource;

// Single source of truth: `baud-snapshot::msr` (this crate depends on `baud-snapshot`, not the
// reverse, per specs/baud-snapshot.md §2's architecture diagram) — `baud-snapshot`'s restore-order
// logic needs these same three MSR numbers to sequence `IA32_TSC` before `IA32_TSC_DEADLINE` on
// restore (specs/baud-snapshot.md §6), so they are defined once there and re-exported here rather
// than duplicated.
pub use baud_snapshot::msr::{MSR_IA32_TSC, MSR_IA32_TSC_AUX, MSR_IA32_TSC_DEADLINE};

/// Source of the retired-conditional-branch count the work-clock is a function of. The real
/// implementation (`linux::LinuxBranchCounter`) wraps a free-running `perf_event_open` counter;
/// unit tests here use a scripted sequence instead.
pub trait BranchCounter {
    fn read(&mut self) -> u64;
}

/// `virtual_tsc = base + k * rcb` (todo.md §3.3), plus the two auxiliary TSC-family MSRs a
/// cooperative-regime guest may also touch. Generic over [`BranchCounter`] so the formula and its
/// monotonicity/reproducibility properties are testable without perf/KVM hardware.
pub struct WorkClock<C: BranchCounter> {
    base: u64,
    k: u64,
    counter: C,
    tsc_deadline: u64,
    tsc_aux: u64,
}

impl<C: BranchCounter> WorkClock<C> {
    pub fn new(base: u64, k: u64, counter: C) -> Self {
        WorkClock { base, k, counter, tsc_deadline: 0, tsc_aux: 0 }
    }

    /// Reconstruct a [`WorkClock`] from a captured `Universe`'s clock state
    /// (`baud-snapshot::universe::ClockState::work_clock_base`/`tsc_deadline`/`tsc_aux`) rather
    /// than a fresh guest's zeroed defaults — the counterpart to [`new`](Self::new) that a
    /// `Multiverse::restore` uses so a guest resumes reading the exact virtual-TSC/deadline/aux
    /// sequence a straight run would have produced from this point, not a clock that appears to
    /// have just reset to zero (specs/baud-snapshot.md §3: "Work-clock anchor | branch-count
    /// base" is only half the served state — a guest that had already armed `IA32_TSC_DEADLINE` or
    /// set `IA32_TSC_AUX` before the snapshot must see those same values after restore too, since
    /// both MSRs are served entirely in software here and never reach KVM's own MSR storage once
    /// the MSR filter routes them to userspace — see `linux::configure_msr_filter`'s doc).
    pub fn restore(base: u64, k: u64, tsc_deadline: u64, tsc_aux: u64, counter: C) -> Self {
        WorkClock { base, k, counter, tsc_deadline, tsc_aux }
    }

    /// The current virtual TSC value: `base + k * rcb`. Saturating, not wrapping — a silent
    /// wraparound would itself be a determinism hole disguised as a valid-looking small number.
    pub fn virtual_tsc(&mut self) -> u64 {
        let rcb = self.counter.read();
        self.base.saturating_add(self.k.saturating_mul(rcb))
    }

    /// The work-clock anchor at the moment of the most recent `IA32_TSC` write (or construction, if
    /// none yet) — the value `baud-snapshot::universe::ClockState::work_clock_base` captures
    /// (specs/baud-snapshot.md §3's "Work-clock anchor" row).
    pub fn base(&self) -> u64 {
        self.base
    }

    /// The last value the guest wrote to `IA32_TSC_DEADLINE` (`0` if never written) — captured
    /// alongside `base` so a restore reproduces it exactly (see [`restore`](Self::restore)'s doc).
    pub fn tsc_deadline(&self) -> u64 {
        self.tsc_deadline
    }

    /// The last value the guest wrote to `IA32_TSC_AUX` (`0` if never written) — same rationale as
    /// [`tsc_deadline`](Self::tsc_deadline).
    pub fn tsc_aux(&self) -> u64 {
        self.tsc_aux
    }
}

impl<C: BranchCounter> TimeSource for WorkClock<C> {
    fn serve_rdmsr(&mut self, msr: u32) -> u64 {
        match msr {
            MSR_IA32_TSC => self.virtual_tsc(),
            MSR_IA32_TSC_DEADLINE => self.tsc_deadline,
            MSR_IA32_TSC_AUX => self.tsc_aux,
            // Any other MSR reaching this TimeSource would itself be a configuration bug in the
            // MSR filter upstream (only the three above are ever routed here) — served fixed
            // rather than panicking a determinism-critical exit handler.
            _ => 0,
        }
    }

    fn absorb_wrmsr(&mut self, msr: u32, value: u64) {
        match msr {
            MSR_IA32_TSC => {
                // A guest write to IA32_TSC must make the *next* read return exactly `value`
                // (some kernels write 0 at boot) — rebase `base` from the RCB at the moment of
                // the write rather than ignoring the write outright.
                let rcb = self.counter.read();
                self.base = value.wrapping_sub(self.k.wrapping_mul(rcb));
            }
            MSR_IA32_TSC_DEADLINE => self.tsc_deadline = value,
            MSR_IA32_TSC_AUX => self.tsc_aux = value,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted branch counter: each `read()` returns the next value from a fixed sequence,
    /// repeating the last value once exhausted (mirrors a real free-running counter that keeps
    /// climbing between reads without the test needing to know exactly how far).
    struct ScriptedCounter {
        sequence: Vec<u64>,
        pos: usize,
    }
    impl ScriptedCounter {
        fn new(sequence: Vec<u64>) -> Self {
            ScriptedCounter { sequence, pos: 0 }
        }
    }
    impl BranchCounter for ScriptedCounter {
        fn read(&mut self) -> u64 {
            let v = *self.sequence.get(self.pos).unwrap_or_else(|| self.sequence.last().unwrap_or(&0));
            self.pos += 1;
            v
        }
    }

    /// A counter frozen at one value — used to test the wrmsr-rebase arithmetic in isolation from
    /// any counter advancement.
    struct ConstantCounter(u64);
    impl BranchCounter for ConstantCounter {
        fn read(&mut self) -> u64 {
            self.0
        }
    }

    /// todo.md §3.3 `work_clock_is_monotone_and_reproducible`: a guest that reads the timestamp N
    /// times yields a non-decreasing sequence, identical across a double-run driven by the same
    /// underlying branch-count sequence.
    #[test]
    fn work_clock_is_monotone_and_reproducible() {
        let rcb_sequence = vec![0u64, 5, 5, 12, 40, 41, 100];

        let mut clock_a = WorkClock::new(1_000, 3, ScriptedCounter::new(rcb_sequence.clone()));
        let reads_a: Vec<u64> = rcb_sequence.iter().map(|_| clock_a.virtual_tsc()).collect();

        let mut clock_b = WorkClock::new(1_000, 3, ScriptedCounter::new(rcb_sequence.clone()));
        let reads_b: Vec<u64> = rcb_sequence.iter().map(|_| clock_b.virtual_tsc()).collect();

        assert_eq!(reads_a, reads_b, "same base/k/rcb-sequence must yield an identical read sequence");
        assert!(reads_a.windows(2).all(|w| w[0] <= w[1]), "virtual TSC must be non-decreasing: {reads_a:?}");
        // Sanity: the formula is actually being applied, not a constant.
        assert_eq!(reads_a[0], 1_000, "rcb=0 must read back exactly base");
        assert_eq!(reads_a[3], 1_000 + 3 * 12);
    }

    #[test]
    fn different_k_or_base_diverges_the_read_sequence() {
        let rcb_sequence = vec![0u64, 1, 2, 3];
        let mut clock_a = WorkClock::new(0, 1, ScriptedCounter::new(rcb_sequence.clone()));
        let mut clock_b = WorkClock::new(0, 2, ScriptedCounter::new(rcb_sequence.clone()));
        let reads_a: Vec<u64> = rcb_sequence.iter().map(|_| clock_a.virtual_tsc()).collect();
        let reads_b: Vec<u64> = rcb_sequence.iter().map(|_| clock_b.virtual_tsc()).collect();
        assert_ne!(reads_a, reads_b, "a different k must produce a different observed sequence");
    }

    #[test]
    fn rdmsr_and_wrmsr_route_by_msr_number() {
        let mut clock = WorkClock::new(0, 1, ConstantCounter(10));
        assert_eq!(clock.serve_rdmsr(MSR_IA32_TSC), 10);

        clock.absorb_wrmsr(MSR_IA32_TSC_DEADLINE, 0xDEAD_BEEF);
        assert_eq!(clock.serve_rdmsr(MSR_IA32_TSC_DEADLINE), 0xDEAD_BEEF);

        clock.absorb_wrmsr(MSR_IA32_TSC_AUX, 7);
        assert_eq!(clock.serve_rdmsr(MSR_IA32_TSC_AUX), 7);

        // An MSR this TimeSource was never meant to see is served a fixed value, not a panic.
        assert_eq!(clock.serve_rdmsr(0xDEAD), 0);
    }

    /// The counterpart to `wrmsr_to_tsc_rebases_...` below: `base()`/`tsc_deadline()`/`tsc_aux()`
    /// expose exactly what a caller needs to capture into `ClockState`, and `restore()` rebuilds a
    /// `WorkClock` that reads back identically to the one it was captured from — the round-trip
    /// `Multiverse::snapshot`/`Multiverse::restore` depends on.
    #[test]
    fn restore_reproduces_the_captured_base_deadline_and_aux() {
        let mut original = WorkClock::new(1_000, 3, ConstantCounter(7));
        original.absorb_wrmsr(MSR_IA32_TSC_DEADLINE, 0xABCD);
        original.absorb_wrmsr(MSR_IA32_TSC_AUX, 42);
        assert_eq!(original.base(), 1_000);
        assert_eq!(original.tsc_deadline(), 0xABCD);
        assert_eq!(original.tsc_aux(), 42);

        let mut restored =
            WorkClock::restore(original.base(), 3, original.tsc_deadline(), original.tsc_aux(), ConstantCounter(7));
        assert_eq!(restored.serve_rdmsr(MSR_IA32_TSC), original.serve_rdmsr(MSR_IA32_TSC));
        assert_eq!(restored.serve_rdmsr(MSR_IA32_TSC_DEADLINE), 0xABCD);
        assert_eq!(restored.serve_rdmsr(MSR_IA32_TSC_AUX), 42);
    }

    #[test]
    fn wrmsr_to_tsc_rebases_so_the_next_read_matches_the_written_value() {
        let mut clock = WorkClock::new(0, 5, ConstantCounter(42));
        assert_eq!(clock.virtual_tsc(), 210); // 0 + 5*42

        clock.absorb_wrmsr(MSR_IA32_TSC, 9_999);
        assert_eq!(
            clock.serve_rdmsr(MSR_IA32_TSC),
            9_999,
            "a write to IA32_TSC must be reflected exactly on the very next read at the same RCB"
        );
    }
}
