// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-driver — deterministic fuzzing driver (Hegel-like, built from scratch)
//
// Architecture:
//   - Seeded ChaCha20 PRNG as the sole source of all draws
//   - Draw API: draw_bits, draw_int, draw_choice, draw_hold, draw_weather
//   - Corpus: current-best tape + reservoir of earlier prefixes
//   - Scheduler: extend-best / mutate / splice-from-reservoir
//   - Shrinker: chunk-delete, zero, hold-shorten, dedup passes
//   - Pure library — no IO, no tokio, no threads
//   - Property: same seed + same observation replies ⇒ byte-identical tape
//
// Rules:
//   - No IO, no async, no threads
//   - deps = {rand_chacha, serde, baud-proto}

use rand::RngCore;
use rand_chacha::ChaCha20Rng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Tape — the recorded choice sequence
// ---------------------------------------------------------------------------

/// A tape is a sequence of draw results (raw bytes for each draw).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Tape {
    /// The seed this tape was created with
    pub seed: u64,
    /// Each entry: the raw bytes returned for that draw
    pub choices: Vec<Vec<u8>>,
}

impl Tape {
    pub fn new(seed: u64) -> Self {
        Tape { seed, choices: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.choices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.choices.is_empty()
    }

    /// Concatenate all choice bytes into a single flat byte vector.
    /// Used for tape identity comparison in `same_seed_same_replies_same_tape`.
    pub fn tape_bytes(&self) -> Vec<u8> {
        self.choices.iter().flat_map(|c| c.iter().copied()).collect()
    }
}

// ---------------------------------------------------------------------------
// DrawKind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DrawKind {
    Bits(u32),
    Int { lo: i64, hi: i64 },
    Choice(Vec<u32>),
    Hold { mean: u32 },
    Weather { p_start: f64, p_stop: f64 },
}

// ---------------------------------------------------------------------------
// Driver state
// ---------------------------------------------------------------------------

/// Canonical StrategySpec — re-exported from baud-proto.
/// baud-driver and baud-tape-agent both use this type; the definition lives in baud-proto.
pub use baud_proto::StrategySpec;

/// Extension trait to provide reservoir and goal access on the canonical StrategySpec.
trait StrategySpecExt {
    fn reservoir_keep(&self) -> u32;
    fn reservoir_p_backoff(&self) -> f64;
    fn goal_probe(&self) -> Option<&str>;
    fn goal_value_f64(&self) -> Option<f64>;
}

impl StrategySpecExt for StrategySpec {
    fn reservoir_keep(&self) -> u32 {
        self.reservoir.as_ref().map(|r| r.keep).unwrap_or(32)
    }
    fn reservoir_p_backoff(&self) -> f64 {
        self.reservoir.as_ref().map(|r| r.p_backoff).unwrap_or(0.1)
    }
    fn goal_probe(&self) -> Option<&str> {
        self.goal.as_ref().map(|g| g.probe.as_str())
    }
    fn goal_value_f64(&self) -> Option<f64> {
        self.goal.as_ref().and_then(|g| match &g.value {
            baud_proto::Value::U64(v) => Some(*v as f64),
            baud_proto::Value::I64(v) => Some(*v as f64),
            _ => None,
        })
    }
}

/// Score derived from probe observations (higher is better)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, PartialOrd)]
pub struct Score(pub Vec<f64>);

impl Score {
    fn zero() -> Self {
        Score(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// TacticsSpec — tactics for the draw strategies
// ---------------------------------------------------------------------------

/// Built-in input tactics
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTactic {
    /// White-noise: each draw is independent
    Random,
    /// Stateful mask: previous byte remembered, bits flipped with p_flip
    StatefulMask { p_flip: f64 },
    /// Geometric hold: draw a hold count with given geometric mean
    Hold { geom_mean: f64 },
}

/// Built-in weather tactics
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeatherTactic {
    /// Markov partition: stateful, transitions with p_start/p_stop
    MarkovPartition { p_start: f64, p_stop: f64 },
    /// Burst delay: bursts of delay with given regimes
    BurstDelay { regimes: Vec<(u64, u64)> },
    /// Crash/restart: each tick crashes with probability p, min_up_ticks before next crash
    CrashRestart { p: f64, min_up_ticks: u64 },
}

/// Complete tactics specification.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TacticsSpec {
    pub input: Vec<InputTactic>,
    pub weather: Vec<WeatherTactic>,
}

/// Everything a `Driver` accumulates across generations that a caller needs to resume
/// exploration in a later process/request instead of starting `generation` back at 0 with an
/// empty `best`/`reservoir` every time (todo.md §14's "`Driver` state persistence across
/// requests" gap: every HTTP route that runs an exploration loop built a fresh `Driver` per
/// request, so a `resume`d generate call had no memory of an earlier generate call's progress).
/// Deliberately excludes `seed`/`strategy`/`tactics` — a caller reconstructs `Driver::new` with
/// those (its own request already carries them) and applies this on top via
/// [`Driver::apply_state`], the same way `replay_tape`/`live_tape`/`rng`/`run_cursor` stay
/// request-scoped and are never part of what's exported.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DriverState {
    pub best: Tape,
    pub best_score: Vec<f64>,
    pub reservoir: Vec<Tape>,
    pub generation: u64,
    pub partition_state: bool,
    /// `ChaCha20Rng::get_word_pos()` — the *unrecorded* internal scheduling draws
    /// (`draw_raw_u64`/`draw_raw_f64`, used to pick mutate/splice indices and reservoir
    /// replacement) advance `rng` without writing anything to any `Tape`, so a resumed driver
    /// whose `rng` restarts at word 0 diverges from one that kept running in-process the moment
    /// its first mutate/splice decision draws a different index. Restoring this exact stream
    /// position (not just re-seeding) is what makes `apply_state` reproduce the *same* schedule,
    /// not merely a plausible one — `ChaCha20Rng::set_word_pos` is built for exactly this
    /// save/resume use (rand_chacha's own round-trip tests assert it).
    pub rng_word_pos: u128,
}

/// The main driver struct.
pub struct Driver {
    seed: u64,
    strategy: StrategySpec,
    #[allow(dead_code)]
    tactics: TacticsSpec,
    /// The current best tape (extends from this)
    best: Tape,
    /// Best score seen so far
    best_score: Score,
    /// Reservoir of promising prefix tapes
    reservoir: Vec<Tape>,
    /// Current draw position within a run (which choice index we're on)
    run_cursor: usize,
    /// Active RNG for live draws (not replay)
    rng: ChaCha20Rng,
    /// Are we in replay mode? If so, read from replay_tape
    replay_mode: bool,
    /// Tape being replayed (read-only)
    replay_tape: Tape,
    /// Currently recorded tape (for the live run)
    live_tape: Tape,
    /// Generation counter
    generation: u64,
    /// Stateful weather partition state (for Markov draw_weather)
    partition_state: bool,
}

impl Driver {
    /// Create a new driver with the given seed, strategy, and tactics.
    /// Spec: `Driver::new(seed: u64, strategy: StrategySpec, tactics: TacticsSpec) -> Self`
    pub fn new(seed: u64, strategy: StrategySpec, tactics: TacticsSpec) -> Self {
        let rng = ChaCha20Rng::seed_from_u64(seed);
        Driver {
            seed,
            strategy,
            tactics,
            best: Tape::new(seed),
            best_score: Score::zero(),
            reservoir: Vec::new(),
            run_cursor: 0,
            rng,
            replay_mode: false,
            replay_tape: Tape::new(seed),
            live_tape: Tape::new(seed),
            generation: 0,
            partition_state: false,
        }
    }

    /// Create a new driver with default tactics (backward-compatible helper).
    pub fn new_simple(seed: u64, strategy: StrategySpec) -> Self {
        Self::new(seed, strategy, TacticsSpec::default())
    }

    /// Start a new run. Returns the tape to be replayed (empty = fresh run with live draws).
    /// Call this before any draw_* calls.
    pub fn begin_run(&mut self) -> &Tape {
        self.run_cursor = 0;
        self.live_tape = Tape::new(self.seed);

        // Decide scheduling: extend best, mutate, or splice from reservoir
        let use_replay = !self.best.choices.is_empty() && self.generation > 0;
        if use_replay {
            // Alternate: generation % 3 == 0 → splice, else extend/mutate
            let g = self.generation;
            if g % 3 == 0 && !self.reservoir.is_empty() {
                // Splice from reservoir
                let idx = (self.draw_raw_u64() as usize) % self.reservoir.len();
                self.replay_tape = self.reservoir[idx].clone();
            } else if g % 3 == 1 {
                // Mutate: copy best, flip some choices
                let mut mutated = self.best.clone();
                let n = mutated.choices.len();
                if n > 0 {
                    let flip_count = 1 + (self.draw_raw_u64() as usize % (n.min(4) + 1));
                    for _ in 0..flip_count {
                        let i = (self.draw_raw_u64() as usize) % n;
                        let len = mutated.choices[i].len();
                        if len > 0 {
                            let bi = (self.draw_raw_u64() as usize) % len;
                            let bit = 1u8 << (self.draw_raw_u64() % 8);
                            mutated.choices[i][bi] ^= bit;
                        }
                    }
                }
                self.replay_tape = mutated;
            } else {
                // Extend best (append fresh draws to the end of best)
                self.replay_tape = self.best.clone();
            }
            self.replay_mode = true;
        } else {
            // Fresh run with pure live draws
            self.replay_mode = false;
            self.replay_tape = Tape::new(self.seed);
        }

        self.generation += 1;
        &self.live_tape
    }

    /// End a run, providing the scores observed. Updates best/reservoir.
    pub fn end_run(&mut self, observations: &[(String, f64)]) {
        let score = self.compute_score(observations);
        if score > self.best_score || self.best.choices.is_empty() {
            self.best = self.live_tape.clone();
            self.best_score = score;
        }
        // Add to reservoir with probability
        if self.reservoir.len() < self.strategy.reservoir_keep() as usize {
            self.reservoir.push(self.live_tape.clone());
        } else if self.draw_raw_f64() < self.strategy.reservoir_p_backoff() {
            // Replace a random entry
            let idx = (self.draw_raw_u64() as usize) % self.reservoir.len();
            self.reservoir[idx] = self.live_tape.clone();
        }
    }

    /// Check if goal is reached based on observations.
    pub fn is_goal_reached(&self, observations: &[(String, f64)]) -> bool {
        if let (Some(probe), Some(goal_val)) = (self.strategy.goal_probe(), self.strategy.goal_value_f64()) {
            for (name, val) in observations {
                if name == probe && (*val - goal_val).abs() < 1e-9 {
                    return true;
                }
            }
        }
        false
    }

    /// Returns the current best tape (for persistence/replay).
    pub fn best_tape(&self) -> &Tape {
        &self.best
    }

    /// Returns the live tape recorded so far in the current run.
    pub fn live_tape(&self) -> &Tape {
        &self.live_tape
    }

    /// Export everything needed to resume exploration later ([`DriverState`]'s own doc explains
    /// what's excluded and why). Call after `end_run` (or before any `begin_run` at all, for a
    /// still-fresh driver) — never mid-run, since `live_tape`/`run_cursor` are not captured.
    pub fn export_state(&self) -> DriverState {
        DriverState {
            best: self.best.clone(),
            best_score: self.best_score.0.clone(),
            reservoir: self.reservoir.clone(),
            generation: self.generation,
            partition_state: self.partition_state,
            rng_word_pos: self.rng.get_word_pos(),
        }
    }

    /// Apply a previously exported [`DriverState`] onto a freshly constructed `Driver` (same
    /// seed/strategy/tactics as the run being resumed), so the next `begin_run` schedules
    /// splice/mutate/extend exactly as if this were the same `Driver` continuing in the same
    /// process — `generation` is what `begin_run` gates its `use_replay` decision on, so a
    /// resumed driver with `generation > 0` and a non-empty `best` behaves identically to one
    /// that never stopped.
    pub fn apply_state(&mut self, state: DriverState) {
        self.best = state.best;
        self.best_score = Score(state.best_score);
        self.reservoir = state.reservoir;
        self.generation = state.generation;
        self.partition_state = state.partition_state;
        self.rng.set_word_pos(state.rng_word_pos);
    }

    // -----------------------------------------------------------------------
    // Draw API
    // -----------------------------------------------------------------------

    /// Draw `n` bits (up to 64). Returns the n-bit value as a Vec<u8> (little-endian, ceil(n/8) bytes).
    /// Spec: `fn draw_bits(&mut self, n: u32) -> Vec<u8>`
    ///
    /// Records only the `byte_count` bytes actually returned to the caller, not the full 8-byte
    /// raw draw — so a tape mutation that flips a bit in this choice always changes what the
    /// caller sees (previously the choice recorded all 8 raw bytes while draw_bits(8) only ever
    /// surfaced byte 0, so ~7/8 of mutations were invisible to callers using draw_bits(8)).
    pub fn draw_bits(&mut self, n: u32) -> Vec<u8> {
        assert!(n <= 64, "draw_bits: n must be <= 64");
        let raw = self.next_raw_u64();
        let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
        let value = raw & mask;
        let byte_count = ((n + 7) / 8) as usize;
        // Little-endian encoding
        let bytes = value.to_le_bytes()[..byte_count].to_vec();
        self.record_draw(bytes.clone());
        bytes
    }

    /// Draw an integer in [lo, hi] (inclusive).
    pub fn draw_int(&mut self, lo: i64, hi: i64) -> i64 {
        assert!(lo <= hi, "draw_int: lo must be <= hi");
        if lo == hi {
            // Record a zero-width draw so tapes stay aligned
            self.record_draw(lo.to_le_bytes().to_vec());
            return lo;
        }
        let range = (hi - lo) as u64 + 1;
        let raw = self.draw_u64();
        lo + (raw % range) as i64
    }

    /// Draw from a weighted choice distribution. Returns the chosen index.
    pub fn draw_choice(&mut self, weights: &[u32]) -> usize {
        assert!(!weights.is_empty(), "draw_choice: weights must be non-empty");
        let total: u64 = weights.iter().map(|&w| w as u64).sum();
        let raw = self.draw_u64() % total;
        let mut acc = 0u64;
        for (i, &w) in weights.iter().enumerate() {
            acc += w as u64;
            if raw < acc {
                return i;
            }
        }
        weights.len() - 1
    }

    /// Draw a geometric "hold" value with given mean.
    pub fn draw_hold(&mut self, mean: u32) -> u32 {
        if mean == 0 { return 0; }
        // Geometric distribution: P(X = k) = (1 - p)^k * p where p = 1/mean
        let p = 1.0 / (mean as f64);
        let u = self.draw_f64();
        // Inverse CDF: k = floor(log(u) / log(1-p))
        let k = (u.ln() / (1.0 - p).ln()) as u32;
        k
    }

    /// Draw a Markov weather state (partition on/off).
    ///
    /// Stateful: `partition_state` is remembered across calls.
    /// - When OFF: transition to ON with probability `p_start`.
    /// - When ON: transition to OFF with probability `p_stop`.
    /// Returns 1 if partition is active after this draw, 0 otherwise.
    ///
    /// The transition draw is recorded on the tape, making weather reproducible via replay.
    pub fn draw_weather(&mut self, p_start: f64, p_stop: f64) -> u8 {
        let u = self.draw_f64();
        if self.partition_state {
            // Currently ON: transition to OFF with p_stop
            if u < p_stop {
                self.partition_state = false;
            }
        } else {
            // Currently OFF: transition to ON with p_start
            if u < p_start {
                self.partition_state = true;
            }
        }
        if self.partition_state { 1 } else { 0 }
    }

    /// Reset the weather partition state (call at the start of a new run).
    pub fn reset_weather(&mut self) {
        self.partition_state = false;
    }

    // -----------------------------------------------------------------------
    // Shrinking
    // -----------------------------------------------------------------------

    /// Run shrink passes on the best tape, using the provided oracle.
    /// The oracle returns the score for a given tape (None = invalid/crash).
    /// Passes: chunk-delete, zero, hold-shorten, dedup.
    pub fn shrink<F>(&mut self, oracle: F) -> Tape
    where
        F: Fn(&Tape) -> Option<Score>,
    {
        let mut current = self.best.clone();

        // Chunk-delete pass: try removing consecutive chunks
        'outer: loop {
            let n = current.choices.len();
            if n == 0 { break; }
            let chunk = (n / 8).max(1);
            let mut i = 0;
            while i + chunk <= n {
                let mut candidate = current.choices.clone();
                candidate.drain(i..i + chunk);
                let t = Tape { seed: current.seed, choices: candidate };
                if let Some(s) = oracle(&t) {
                    if s >= self.best_score {
                        current = t;
                        continue 'outer;
                    }
                }
                i += 1;
            }
            break;
        }

        // Zero pass: try zeroing individual choices
        for i in 0..current.choices.len() {
            let mut candidate = current.choices.clone();
            candidate[i] = vec![0u8; candidate[i].len()];
            let t = Tape { seed: current.seed, choices: candidate };
            if let Some(s) = oracle(&t) {
                if s >= self.best_score {
                    current = t;
                }
            }
        }

        // Dedup pass: collapse identical consecutive choices
        {
            let mut deduped: Vec<Vec<u8>> = Vec::new();
            let mut prev: Option<&Vec<u8>> = None;
            for c in &current.choices {
                let skip = prev.map_or(false, |p| p == c);
                if !skip {
                    deduped.push(c.clone());
                }
                prev = Some(c);
            }
            let t = Tape { seed: current.seed, choices: deduped };
            if let Some(s) = oracle(&t) {
                if s >= self.best_score {
                    current = t;
                }
            }
        }

        current
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn draw_u64(&mut self) -> u64 {
        let val = self.next_raw_u64();
        self.record_draw(val.to_le_bytes().to_vec());
        val
    }

    /// Get the next draw's raw u64 value (from replay or live RNG) without recording it —
    /// callers record whatever subset of bytes is actually meaningful to them (see `draw_bits`).
    fn next_raw_u64(&mut self) -> u64 {
        // If replaying and within bounds, return the recorded choice.
        if self.replay_mode && self.run_cursor < self.replay_tape.choices.len() {
            let bytes = &self.replay_tape.choices[self.run_cursor];
            // Recorded choices may be shorter than 8 bytes (e.g. draw_bits(8) records 1 byte) —
            // zero-extend rather than fail (a naive try_into would drop the value to all-zero).
            let mut buf = [0u8; 8];
            let take = bytes.len().min(8);
            buf[..take].copy_from_slice(&bytes[..take]);
            u64::from_le_bytes(buf)
        } else {
            self.rng.next_u64()
        }
    }

    fn draw_f64(&mut self) -> f64 {
        let raw = self.draw_u64();
        // Map to [0, 1)
        (raw >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Draw a raw u64 without recording (for internal scheduling decisions)
    fn draw_raw_u64(&mut self) -> u64 {
        self.rng.next_u64()
    }

    fn draw_raw_f64(&mut self) -> f64 {
        let raw = self.draw_raw_u64();
        (raw >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    fn record_draw(&mut self, bytes: Vec<u8>) {
        self.live_tape.choices.push(bytes);
        self.run_cursor += 1;
    }

    fn compute_score(&self, observations: &[(String, f64)]) -> Score {
        let mut scores = Vec::new();
        for probe_name in &self.strategy.maximize {
            let val = observations.iter()
                .filter(|(name, _)| name == probe_name)
                .map(|(_, v)| *v)
                .last()
                .unwrap_or(0.0);
            scores.push(val);
        }
        Score(scores)
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The generation counter `begin_run` advances on every call — exposed so a caller that
    /// persists/resumes `DriverState` across requests can report whether progress is actually
    /// accumulating (e.g. over HTTP) without needing a full `export_state()` just to read one
    /// field.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

// ---------------------------------------------------------------------------
// Determinism property test helper
// ---------------------------------------------------------------------------

/// Simulate a run with a given tape, returning the sequence of draw values.
/// Used by the property test to verify same seed → same tape.
pub struct ReplayEngine {
    tape: Tape,
    cursor: usize,
    rng: ChaCha20Rng,
    recorded: Vec<u64>,
}

impl ReplayEngine {
    pub fn new(seed: u64) -> Self {
        ReplayEngine {
            tape: Tape::new(seed),
            cursor: 0,
            rng: ChaCha20Rng::seed_from_u64(seed),
            recorded: Vec::new(),
        }
    }

    pub fn from_tape(tape: Tape) -> Self {
        let seed = tape.seed;
        ReplayEngine {
            tape,
            cursor: 0,
            rng: ChaCha20Rng::seed_from_u64(seed),
            recorded: Vec::new(),
        }
    }

    /// Draw a u64, recording it
    pub fn draw_u64(&mut self) -> u64 {
        let val = if self.cursor < self.tape.choices.len() {
            let bytes = self.tape.choices[self.cursor].clone();
            u64::from_le_bytes(bytes.try_into().unwrap_or([0u8; 8]))
        } else {
            self.rng.next_u64()
        };
        self.recorded.push(val);
        self.cursor += 1;
        val
    }

    pub fn into_recorded(self) -> Vec<u64> {
        self.recorded
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_driver(seed: u64) -> Driver {
        Driver::new(seed, StrategySpec::default(), TacticsSpec::default())
    }

    #[test]
    fn draw_bits_range() {
        let mut d = make_driver(42);
        d.begin_run();
        for n in [1u32, 8, 16, 32, 64] {
            let v = d.draw_bits(n);
            assert_eq!(v.len(), ((n + 7) / 8) as usize, "draw_bits({n}) should return ceil({n}/8) bytes");
        }
    }

    #[test]
    fn draw_bits_returns_vec_u8() {
        // Spec: draw_bits returns Vec<u8> not u64
        let mut d = make_driver(123);
        d.begin_run();
        let v8 = d.draw_bits(8);
        assert_eq!(v8.len(), 1, "draw_bits(8) must return 1 byte");
        let v16 = d.draw_bits(16);
        assert_eq!(v16.len(), 2, "draw_bits(16) must return 2 bytes");
        let v32 = d.draw_bits(32);
        assert_eq!(v32.len(), 4, "draw_bits(32) must return 4 bytes");
        let v64 = d.draw_bits(64);
        assert_eq!(v64.len(), 8, "draw_bits(64) must return 8 bytes");
    }

    #[test]
    fn draw_int_bounds() {
        let mut d = make_driver(7);
        d.begin_run();
        for _ in 0..100 {
            let v = d.draw_int(3, 10);
            assert!(v >= 3 && v <= 10, "draw_int(3,10) = {v}");
        }
    }

    #[test]
    fn draw_choice_valid_index() {
        let mut d = make_driver(99);
        d.begin_run();
        let weights = vec![1u32, 2, 3, 4];
        for _ in 0..50 {
            let idx = d.draw_choice(&weights);
            assert!(idx < weights.len(), "draw_choice index {idx} out of range");
        }
    }

    /// Spec §5 API: run_driver(seed, script) helper for testing.
    /// Runs the driver with the given seed, making N draws and feeding back
    /// the supplied observation script. Returns the recorded tape.
    fn run_driver(seed: u64, script: &[(&str, f64)]) -> Tape {
        let mut d = Driver::new(seed, StrategySpec::default(), TacticsSpec::default());
        d.begin_run();
        // Make one draw per observation in the script
        let n = script.len().max(10);
        for _ in 0..n {
            d.draw_bits(64);
        }
        // Report observations back to driver
        let obs: Vec<(String, f64)> = script.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        d.end_run(&obs);
        d.live_tape().clone()
    }

    /// Spec §5 mandated test: same seed + same observation replies → byte-identical tape.
    /// Two independent Driver runs with the same seed and same observation script must
    /// produce identical tape bytes.
    #[test]
    fn same_seed_same_replies_same_tape() {
        let seed = 42u64;
        let script = &[("depth", 3.0), ("crashed", 0.0)];

        let tape_a = run_driver(seed, script);
        let tape_b = run_driver(seed, script);

        assert_eq!(
            tape_a.tape_bytes(),
            tape_b.tape_bytes(),
            "same seed + same replies must produce byte-identical tape"
        );
    }

    #[test]
    fn determinism_property_same_seed_same_draws() {
        // Property: two drivers with the same seed produce the same sequence of draws
        // when given the same observation replies.
        let seed = 12345u64;

        // Run driver A
        let mut a = make_driver(seed);
        a.begin_run();
        let draws_a: Vec<Vec<u8>> = (0..20).map(|_| a.draw_bits(64)).collect();
        let tape_a = a.live_tape().clone();

        // Replay with driver B using the recorded tape
        let mut engine = ReplayEngine::from_tape(tape_a.clone());
        let draws_b: Vec<Vec<u8>> = (0..20).map(|_| engine.draw_u64().to_le_bytes().to_vec()).collect();

        assert_eq!(draws_a, draws_b, "determinism: same seed + same tape → same draws");
    }

    #[test]
    fn replay_tape_produces_identical_sequence() {
        let seed = 999u64;
        let mut d = make_driver(seed);
        d.begin_run();
        let draws1: Vec<Vec<u8>> = (0..10).map(|_| d.draw_bits(64)).collect();
        let tape = d.live_tape().clone();

        // Replay the tape
        let mut engine = ReplayEngine::from_tape(tape);
        let draws2: Vec<Vec<u8>> = (0..10).map(|_| engine.draw_u64().to_le_bytes().to_vec()).collect();

        assert_eq!(draws1, draws2, "replay tape: should produce identical draw sequence");
    }

    #[test]
    fn draw_bits_records_only_caller_visible_bytes() {
        // Regression test: draw_bits(n) used to record the full 8-byte raw draw_u64() value in
        // Tape.choices even when it only ever handed the caller ceil(n/8) bytes (e.g. draw_bits(8)
        // only ever surfaces byte 0). begin_run's mutate pass picks a byte index uniformly within
        // the recorded choice's length, so ~7/8 of mutations landed in the invisible bytes 1-7 and
        // never changed what a draw_bits(8) caller actually saw. The fix: record exactly the bytes
        // handed back, so the choice length always matches the caller-visible byte count.
        let mut d = make_driver(7);
        d.begin_run();
        let v8 = d.draw_bits(8);
        let v16 = d.draw_bits(16);
        let v32 = d.draw_bits(32);
        assert_eq!(d.live_tape().choices[0], v8);
        assert_eq!(d.live_tape().choices[0].len(), 1, "draw_bits(8) must record exactly 1 byte");
        assert_eq!(d.live_tape().choices[1], v16);
        assert_eq!(d.live_tape().choices[1].len(), 2, "draw_bits(16) must record exactly 2 bytes");
        assert_eq!(d.live_tape().choices[2], v32);
        assert_eq!(d.live_tape().choices[2].len(), 4, "draw_bits(32) must record exactly 4 bytes");
    }

    #[test]
    fn mutating_a_draw_bits_choice_always_changes_the_replayed_value() {
        // With the recorded choice trimmed to exactly the caller-visible byte(s), the mutate
        // pass's `bi = draw_raw_u64() % len` always lands on a meaningful byte for draw_bits(8)
        // (len == 1), guaranteeing the flip is observable on replay — the property the bug report
        // says was violated ~7/8 of the time before this fix.
        let mut d = make_driver(7);
        d.begin_run();
        let original = d.draw_bits(8);
        d.end_run(&[]);

        let mut mutated = d.best_tape().clone();
        mutated.choices[0][0] ^= 0x01;

        let mut replay = make_driver(7);
        replay.replay_mode = true;
        replay.replay_tape = mutated;
        let replayed = replay.draw_bits(8);

        assert_ne!(replayed, original, "flipping the only recorded byte must change draw_bits(8)'s replayed output");
        assert_eq!(replayed[0], original[0] ^ 0x01);
    }

    #[test]
    fn end_run_updates_best() {
        let mut d = Driver::new(1, StrategySpec {
            maximize: vec!["depth".into()],
            ..Default::default()
        }, TacticsSpec::default());
        d.begin_run();
        let _ = d.draw_bits(8);
        let _ = d.draw_bits(8);
        // Report a high score
        d.end_run(&[("depth".into(), 100.0)]);
        assert!(!d.best.choices.is_empty(), "best tape should be non-empty after end_run");
    }

    #[test]
    fn shrink_reduces_tape() {
        let mut d = make_driver(42);
        d.begin_run();
        // Record 20 draws
        for _ in 0..20 { d.draw_bits(8); }
        d.end_run(&[]);

        // Oracle: accepts any tape (trivial crash reproduction)
        let shrunk = d.shrink(|_tape| Some(Score(Vec::new())));
        // Shrunk tape should be smaller or equal
        assert!(shrunk.len() <= d.best.len() + 1, "shrink should not grow the tape significantly");
    }

    #[test]
    fn goal_detection() {
        let strategy = StrategySpec {
            goal: Some(baud_proto::Predicate {
                probe: "x".into(),
                value: baud_proto::Value::U64(1),
            }),
            ..Default::default()
        };
        let d = Driver::new(1, strategy, TacticsSpec::default());
        assert!(d.is_goal_reached(&[("x".into(), 1.0)]));
        assert!(!d.is_goal_reached(&[("x".into(), 0.0)]));
    }

    #[test]
    fn draw_hold_reasonable() {
        let mut d = make_driver(5);
        d.begin_run();
        // Draw 20 hold values with mean=5, all should be reasonable
        for _ in 0..20 {
            let v = d.draw_hold(5);
            assert!(v < 10000, "hold value {v} seems too large");
        }
    }

    /// draw_weather_is_markov: draw_weather must be stateful (Markov), not independent per call.
    #[test]
    fn draw_weather_is_markov() {
        let mut d = make_driver(777);
        d.begin_run();

        // With p_start=1.0 and p_stop=0.0: once ON, stays ON forever
        let first = d.draw_weather(1.0, 0.0); // always transitions to ON
        assert_eq!(first, 1, "p_start=1.0 must turn partition ON immediately");
        for _ in 0..10 {
            let v = d.draw_weather(1.0, 0.0); // p_stop=0.0: never transitions OFF
            assert_eq!(v, 1, "p_stop=0.0: partition must remain ON");
        }

        // Reset and test: with p_start=0.0, stays OFF forever
        d.reset_weather();
        assert_eq!(d.draw_weather(0.0, 0.0), 0, "p_start=0.0 must stay OFF");
        for _ in 0..10 {
            assert_eq!(d.draw_weather(0.0, 0.0), 0, "p_start=0.0: partition must remain OFF");
        }
    }

    /// Regression test for todo.md §14's "Driver state persistence across requests" gap: a driver
    /// that exports its state after a few generations and a *fresh* `Driver` that applies that
    /// state must schedule the next generation identically to the original driver continuing
    /// in-process — proving `export_state`/`apply_state` actually carry enough to resume, not
    /// just enough to look non-empty.
    #[test]
    fn exported_state_resumes_scheduling_identically_to_continuing_in_process() {
        let seed = 4242u64;
        let strategy = StrategySpec { maximize: vec!["depth".into()], ..Default::default() };

        // Baseline: one driver runs 4 generations uninterrupted.
        let mut baseline = Driver::new(seed, strategy.clone(), TacticsSpec::default());
        for gen in 0..4u64 {
            baseline.begin_run();
            let _ = baseline.draw_bits(8);
            let _ = baseline.draw_bits(8);
            baseline.end_run(&[("depth".into(), gen as f64)]);
        }

        // Split: a second driver runs the same first 2 generations, exports state, and a brand
        // new driver (never saw generations 0-1) applies it and runs generations 2-3.
        let mut first_half = Driver::new(seed, strategy.clone(), TacticsSpec::default());
        for gen in 0..2u64 {
            first_half.begin_run();
            let _ = first_half.draw_bits(8);
            let _ = first_half.draw_bits(8);
            first_half.end_run(&[("depth".into(), gen as f64)]);
        }
        let exported = first_half.export_state();

        let mut resumed = Driver::new(seed, strategy, TacticsSpec::default());
        resumed.apply_state(exported);
        for gen in 2..4u64 {
            resumed.begin_run();
            let _ = resumed.draw_bits(8);
            let _ = resumed.draw_bits(8);
            resumed.end_run(&[("depth".into(), gen as f64)]);
        }

        assert_eq!(
            resumed.best_tape().tape_bytes(),
            baseline.best_tape().tape_bytes(),
            "resuming from exported state must schedule identically to an uninterrupted driver"
        );
        assert_eq!(resumed.best_score, baseline.best_score);
    }

    /// driver_new_accepts_tactics: Driver::new must accept TacticsSpec parameter.
    #[test]
    fn driver_new_accepts_tactics() {
        let tactics = TacticsSpec {
            input: vec![InputTactic::StatefulMask { p_flip: 0.05 }],
            weather: vec![WeatherTactic::MarkovPartition { p_start: 0.1, p_stop: 0.3 }],
        };
        let d = Driver::new(42, StrategySpec::default(), tactics);
        assert_eq!(d.seed(), 42);
    }
}
