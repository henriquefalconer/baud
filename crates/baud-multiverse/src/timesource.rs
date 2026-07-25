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

use baud_vcpu::{EnforcedRdseedSite, TimeSource};
use std::collections::BTreeMap;

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
    /// Added to every raw [`BranchCounter::read`] before it is treated as "the" RCB value
    /// (specs/baud-snapshot.md §3). `0` for a freshly booted guest ([`new`](Self::new)): a brand
    /// new `perf_event` counter genuinely starts at zero, so no offset is needed. Nonzero after
    /// [`restore`](Self::restore): the restored guest's `counter` is a *new* `perf_event` fd (a
    /// process cannot resurrect another fd's already-elapsed hardware count), which restarts
    /// counting from zero the instant it is created — without this offset, `current_rcb()` would
    /// report a value discontinuous with the guest's true cumulative branch count, silently
    /// rewinding the RCB space `Multiverse::inject_timer_tick` computes `target_rcb` from.
    rcb_offset: u64,
    tsc_deadline: u64,
    tsc_aux: u64,
    /// Backs [`serve_enforced_rdrand`](TimeSource::serve_enforced_rdrand) — a deterministic draw
    /// stream independent of the RCB-derived `virtual_tsc` formula above. `0` (via
    /// [`SplitMix64::new`]`(0)`) until [`with_entropy_seed`](Self::with_entropy_seed) or
    /// [`restore`](Self::restore) sets it; a fresh [`new`](Self::new) with no seed still produces a
    /// deterministic (if not tape-derived) sequence, never a panic or a real-entropy fallback.
    entropy: SplitMix64,
    /// Every known `rdseed`→`UD2`+`NOP` build-time rewrite site (`baud_packages::rdseed::
    /// RdseedSite`, todo.md §4) in the currently-loaded guest image, keyed by the address of the
    /// `UD2` itself. Empty for a guest with no such sites (or none registered yet, e.g. every
    /// fixture that predates this table) — every `#UD` then re-injects, exactly the same as a
    /// real un-rewritten guest, never silently served a guess.
    rdseed_sites: BTreeMap<u64, EnforcedRdseedSite>,
}

impl<C: BranchCounter> WorkClock<C> {
    pub fn new(base: u64, k: u64, counter: C) -> Self {
        WorkClock {
            base,
            k,
            counter,
            rcb_offset: 0,
            tsc_deadline: 0,
            tsc_aux: 0,
            entropy: SplitMix64::new(0),
            rdseed_sites: BTreeMap::new(),
        }
    }

    /// Register this guest image's known `rdseed` rewrite sites (from `baud_packages::rewrite_
    /// rdseed`'s `RdseedRewriteReport`, todo.md §4) so [`resolve_rdseed_site`]
    /// (TimeSource::resolve_rdseed_site) can recognize a trapped `UD2` as a confirmed site rather
    /// than a genuine invalid-opcode fault. Call once, right after [`new`](Self::new)/
    /// [`with_entropy_seed`](Self::with_entropy_seed), before any guest code runs — mirrors that
    /// method's "call once, before boot" contract.
    pub fn with_rdseed_sites(
        mut self,
        sites: impl IntoIterator<Item = (u64, EnforcedRdseedSite)>,
    ) -> Self {
        self.rdseed_sites = sites.into_iter().collect();
        self
    }

    /// Seed the enforced-regime `RDRAND` entropy stream (todo.md §3.2: enforced regime "serves the
    /// tape" for the random instruction) from a run-specific value — `linux::Multiverse::boot`
    /// derives this from a blake3 hash of the run's own tape, so the same tape always produces the
    /// same `RDRAND` draw sequence, and a different tape byte changes it (mirroring
    /// `all_input_is_tape_derived`'s guarantee for the guest-facing tape device). Call once, right
    /// after [`new`](Self::new), before any guest code runs; a restored `WorkClock` uses
    /// [`restore`](Self::restore)'s `entropy_state` instead, to *continue* the sequence rather than
    /// restart it.
    pub fn with_entropy_seed(mut self, seed: u64) -> Self {
        self.entropy = SplitMix64::new(seed);
        self
    }

    /// The entropy stream's current internal state — captured into
    /// `baud_snapshot::universe::ClockState::entropy_state` so a restored guest's next `RDRAND`
    /// continues the exact draw sequence a straight run would have produced, rather than replaying
    /// from the original seed (which would repeat already-served values).
    pub fn entropy_state(&self) -> u64 {
        self.entropy.state()
    }

    /// Reconstruct a [`WorkClock`] from a captured `Universe`'s clock state
    /// (`baud-snapshot::universe::ClockState::work_clock_base`/`rcb_anchor`/`tsc_deadline`/
    /// `tsc_aux`) rather than a fresh guest's zeroed defaults — the counterpart to
    /// [`new`](Self::new) that a `Multiverse::restore` uses so a guest resumes reading the exact
    /// virtual-TSC/RCB/deadline/aux sequence a straight run would have produced from this point,
    /// not a clock that appears to have just reset to zero (specs/baud-snapshot.md §3: "Work-clock
    /// anchor | branch-count base" is only half the served state — a guest that had already armed
    /// `IA32_TSC_DEADLINE` or set `IA32_TSC_AUX` before the snapshot must see those same values
    /// after restore too, since both MSRs are served entirely in software here and never reach
    /// KVM's own MSR storage once the MSR filter routes them to userspace — see
    /// `linux::configure_msr_filter`'s doc). `rcb_anchor` is the cumulative RCB value
    /// [`current_rcb`](Self::current_rcb) reported at the moment of capture — see
    /// [`rcb_offset`](WorkClock::rcb_offset)'s doc for why the restored `counter`'s own raw reads
    /// cannot be trusted to continue that sequence unaided.
    ///
    /// The rdseed site table isn't part of the captured `Universe` (it's a property of the guest
    /// image, not of any one run's state); a caller that needs it re-applies
    /// [`with_rdseed_sites`](Self::with_rdseed_sites) after this call, same as a fresh
    /// [`new`](Self::new) would.
    #[allow(clippy::too_many_arguments)]
    pub fn restore(
        base: u64,
        k: u64,
        rcb_anchor: u64,
        tsc_deadline: u64,
        tsc_aux: u64,
        entropy_state: u64,
        counter: C,
    ) -> Self {
        WorkClock {
            base,
            k,
            counter,
            rcb_offset: rcb_anchor,
            tsc_deadline,
            tsc_aux,
            entropy: SplitMix64::new(entropy_state),
            rdseed_sites: BTreeMap::new(),
        }
    }

    /// The current virtual TSC value: `base + k * rcb`. Saturating, not wrapping — a silent
    /// wraparound would itself be a determinism hole disguised as a valid-looking small number.
    pub fn virtual_tsc(&mut self) -> u64 {
        let rcb = self.current_rcb();
        self.base.saturating_add(self.k.saturating_mul(rcb))
    }

    /// The current cumulative retired-conditional-branch count: [`rcb_offset`](WorkClock::
    /// rcb_offset) plus the underlying [`BranchCounter`]'s own raw reading — the same RCB space
    /// `virtual_tsc` derives from, and the space `Multiverse::inject_timer_tick` (H4,
    /// specs/baud-vcpu.md §5) anchors a `baud_vcpu::linux::pmu::LinuxPmuStepper`'s own armed
    /// counter to, so a `target_rcb` computed from "now" means the same thing to both the
    /// work-clock and the interrupt-injection engine (they are two distinct `perf_event` file
    /// descriptors counting the identical architectural event on the identical thread — their
    /// deltas over the same interval agree by construction, only their absolute epochs differ,
    /// which is exactly what this seeding reconciles) on a fresh boot, and what `rcb_offset`
    /// reconciles across a restore.
    pub fn current_rcb(&mut self) -> u64 {
        self.rcb_offset.saturating_add(self.counter.read())
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
                let rcb = self.current_rcb();
                self.base = value.wrapping_sub(self.k.wrapping_mul(rcb));
            }
            MSR_IA32_TSC_DEADLINE => self.tsc_deadline = value,
            MSR_IA32_TSC_AUX => self.tsc_aux = value,
            _ => {}
        }
    }

    fn serve_enforced_rdtsc(&mut self) -> u64 {
        // Must agree bit-for-bit with `serve_rdmsr(MSR_IA32_TSC)` — a guest that reads the clock
        // via the trapped instruction and one that reads it via the MSR are the same work-clock.
        self.virtual_tsc()
    }

    fn serve_enforced_rdrand(&mut self) -> u64 {
        self.entropy.next_u64()
    }

    fn resolve_rdseed_site(&self, rip: u64) -> Option<EnforcedRdseedSite> {
        self.rdseed_sites.get(&rip).copied()
    }

    fn serve_enforced_rdseed(&mut self) -> u64 {
        // Same tape-seeded entropy sub-stream as RDRAND (todo.md §3.8: "the value comes from the
        // same tape-seeded entropy sub-stream as rdrand").
        self.entropy.next_u64()
    }
}

/// A tiny, dependency-free deterministic PRNG (SplitMix64, Steele/Lea/Flood 2014 — the algorithm
/// behind Java's `SplittableRandom`) used only to back [`WorkClock::serve_enforced_rdrand`]. No
/// crate dependency needed for this (`kernel-module/baud-enforced/ENFORCEMENT_DESIGN.md`:
/// "the userspace side needs zero changes to any pinned crate").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn state(&self) -> u64 {
        self.state
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
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

        let mut restored = WorkClock::restore(
            original.base(),
            3,
            original.current_rcb(),
            original.tsc_deadline(),
            original.tsc_aux(),
            0,
            ConstantCounter(0),
        );
        assert_eq!(restored.serve_rdmsr(MSR_IA32_TSC), original.serve_rdmsr(MSR_IA32_TSC));
        assert_eq!(restored.serve_rdmsr(MSR_IA32_TSC_DEADLINE), 0xABCD);
        assert_eq!(restored.serve_rdmsr(MSR_IA32_TSC_AUX), 42);
    }

    /// The bug H5's real-hardware test surfaced: a restored `WorkClock`'s `counter` is a brand new
    /// `perf_event` fd starting from zero, not a continuation of the original's hardware count —
    /// without `rcb_offset`, `current_rcb()` after restore would silently report a value far
    /// smaller than the guest's true cumulative branch count, corrupting every `target_rcb`
    /// computed from "now" thereafter (`Multiverse::inject_timer_tick`, H4). Asserts `current_rcb`
    /// after restore continues from the captured anchor, not from the new counter's raw zero.
    #[test]
    fn restore_continues_the_rcb_sequence_instead_of_resetting_it() {
        let mut original = WorkClock::new(0, 1, ScriptedCounter::new(vec![50_000]));
        let anchor = original.current_rcb();
        assert_eq!(anchor, 50_000);

        // The restored side's counter is a fresh fd: its own raw reads start small again, exactly
        // like a real `LinuxBranchCounter::new()` would after `Multiverse::restore`.
        let mut restored =
            WorkClock::restore(original.base(), 1, anchor, 0, 0, 0, ScriptedCounter::new(vec![7]));
        assert_eq!(
            restored.current_rcb(),
            50_007,
            "current_rcb after restore must continue from the captured anchor plus the new \
             counter's own delta, not reset to the new counter's raw (small) reading"
        );
    }

    /// todo.md §3.3: the enforced-regime trapped-`RDTSC` path and the cooperative `IA32_TSC` MSR
    /// path must serve the identical work-clock value at the same RCB — this is the invariant
    /// `handle_baud_rdtsc_exit`'s served value depends on when a guest mixes the two.
    #[test]
    fn serve_enforced_rdtsc_matches_the_tsc_msr_at_the_same_rcb() {
        let mut clock = WorkClock::new(1_000, 3, ConstantCounter(12));
        assert_eq!(clock.serve_enforced_rdtsc(), clock.serve_rdmsr(MSR_IA32_TSC));
        assert_eq!(clock.serve_enforced_rdtsc(), 1_000 + 3 * 12);
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

    /// todo.md §3.2's enforced regime: `RDRAND` draws must reproduce identically across a
    /// double-run of the same seed (`with_entropy_seed`, the same tape ⇒ the same seed in
    /// `linux::Multiverse::boot`), and a different seed must diverge the sequence — the same
    /// `all_input_is_tape_derived` guarantee the guest-facing tape device already provides,
    /// applied to this VMM-internal entropy stream instead.
    #[test]
    fn enforced_rdrand_is_reproducible_from_the_same_seed_and_diverges_from_a_different_one() {
        let mut clock_a = WorkClock::new(0, 1, ConstantCounter(0)).with_entropy_seed(0xC0FF_EE);
        let mut clock_b = WorkClock::new(0, 1, ConstantCounter(0)).with_entropy_seed(0xC0FF_EE);
        let draws_a: Vec<u64> = (0..5).map(|_| clock_a.serve_enforced_rdrand()).collect();
        let draws_b: Vec<u64> = (0..5).map(|_| clock_b.serve_enforced_rdrand()).collect();
        assert_eq!(draws_a, draws_b, "the same seed must produce the identical draw sequence");
        assert!(draws_a.windows(2).all(|w| w[0] != w[1]), "a real PRNG must not repeat consecutive draws");

        let mut clock_c = WorkClock::new(0, 1, ConstantCounter(0)).with_entropy_seed(0xDEAD_BEEF);
        let draws_c: Vec<u64> = (0..5).map(|_| clock_c.serve_enforced_rdrand()).collect();
        assert_ne!(draws_a, draws_c, "a different seed must diverge the draw sequence");
    }

    /// The RDRAND counterpart to `restore_continues_the_rcb_sequence_instead_of_resetting_it`: a
    /// restored `WorkClock` must continue the exact same `RDRAND` draw sequence a straight run
    /// would have produced from that point, not repeat the seed's already-served values —
    /// `entropy_state()`/`restore`'s `entropy_state` parameter is what makes that possible.
    #[test]
    fn restore_continues_the_rdrand_sequence_instead_of_repeating_it() {
        let mut original = WorkClock::new(0, 1, ConstantCounter(0)).with_entropy_seed(42);
        let first_three: Vec<u64> = (0..3).map(|_| original.serve_enforced_rdrand()).collect();
        let captured_entropy_state = original.entropy_state();
        let next_from_original: Vec<u64> = (0..3).map(|_| original.serve_enforced_rdrand()).collect();

        let mut restored =
            WorkClock::restore(0, 1, 0, 0, 0, captured_entropy_state, ConstantCounter(0));
        let next_from_restored: Vec<u64> = (0..3).map(|_| restored.serve_enforced_rdrand()).collect();

        assert_eq!(
            next_from_restored, next_from_original,
            "a restored WorkClock's next RDRAND draws must continue the captured stream, not repeat \
             the first three values the original already served"
        );
        assert_ne!(next_from_restored, first_three);
    }

    /// todo.md §4/§12 row 15: a known rewrite site resolves by exact RIP match, and its serve
    /// draws from the same entropy stream RDRAND uses (distinct from, but not diverging from,
    /// whatever RDRAND has already drawn -- they share one SplitMix64 stream by design).
    #[test]
    fn known_rdseed_site_resolves_and_serves_from_the_shared_entropy_stream() {
        let site = EnforcedRdseedSite { gpr_index: 7, length: 4 };
        let mut clock = WorkClock::new(0, 1, ConstantCounter(0))
            .with_entropy_seed(0xC0FF_EE)
            .with_rdseed_sites([(0x1000, site)]);

        assert_eq!(clock.resolve_rdseed_site(0x1000), Some(site));
        assert_eq!(clock.resolve_rdseed_site(0x2000), None, "an unregistered rip must resolve to None");

        let rdrand_first = {
            let mut reference = WorkClock::new(0, 1, ConstantCounter(0)).with_entropy_seed(0xC0FF_EE);
            reference.serve_enforced_rdrand()
        };
        assert_eq!(
            clock.serve_enforced_rdseed(),
            rdrand_first,
            "RDSEED's first served value must match what RDRAND would have drawn from the same seed \
             at the same point in the stream"
        );
    }

    /// A `WorkClock` with no registered sites at all (every fixture that predates this table, or a
    /// guest image with no rdseed instructions) resolves every rip to `None` -- never a panic, and
    /// never a guessed site.
    #[test]
    fn no_registered_sites_resolves_every_rip_to_none() {
        let clock = WorkClock::new(0, 1, ConstantCounter(0));
        assert_eq!(clock.resolve_rdseed_site(0), None);
        assert_eq!(clock.resolve_rdseed_site(0xFFFF_FFFF), None);
    }
}
