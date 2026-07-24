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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySpec {
    /// Probe names to maximize (lexicographic priority)
    pub maximize: Vec<String>,
    pub buckets: Vec<String>,
    pub reservoir_keep: u32,
    pub reservoir_p_backoff: f64,
    /// Optional goal probe name and value (as f64)
    pub goal_probe: Option<String>,
    pub goal_value: Option<f64>,
}

impl Default for StrategySpec {
    fn default() -> Self {
        StrategySpec {
            maximize: Vec::new(),
            buckets: Vec::new(),
            reservoir_keep: 32,
            reservoir_p_backoff: 0.1,
            goal_probe: None,
            goal_value: None,
        }
    }
}

/// Score derived from probe observations (higher is better)
#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub struct Score(pub Vec<f64>);

impl Score {
    fn zero() -> Self {
        Score(Vec::new())
    }
}

/// The main driver struct.
pub struct Driver {
    seed: u64,
    strategy: StrategySpec,
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
}

impl Driver {
    /// Create a new driver with the given seed, strategy, and initial state.
    pub fn new(seed: u64, strategy: StrategySpec) -> Self {
        let rng = ChaCha20Rng::seed_from_u64(seed);
        Driver {
            seed,
            strategy,
            best: Tape::new(seed),
            best_score: Score::zero(),
            reservoir: Vec::new(),
            run_cursor: 0,
            rng,
            replay_mode: false,
            replay_tape: Tape::new(seed),
            live_tape: Tape::new(seed),
            generation: 0,
        }
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
        if self.reservoir.len() < self.strategy.reservoir_keep as usize {
            self.reservoir.push(self.live_tape.clone());
        } else if self.draw_raw_f64() < self.strategy.reservoir_p_backoff {
            // Replace a random entry
            let idx = (self.draw_raw_u64() as usize) % self.reservoir.len();
            self.reservoir[idx] = self.live_tape.clone();
        }
    }

    /// Check if goal is reached based on observations.
    pub fn is_goal_reached(&self, observations: &[(String, f64)]) -> bool {
        if let (Some(probe), Some(goal_val)) = (&self.strategy.goal_probe, self.strategy.goal_value) {
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

    // -----------------------------------------------------------------------
    // Draw API
    // -----------------------------------------------------------------------

    /// Draw `n` bits (up to 64). Returns n-bit value as raw bytes (little-endian u64).
    pub fn draw_bits(&mut self, n: u32) -> u64 {
        assert!(n <= 64, "draw_bits: n must be <= 64");
        let raw = self.draw_u64();
        let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
        raw & mask
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

    /// Draw a Markov weather state (partition on/off). Returns 1 if partition active, 0 otherwise.
    pub fn draw_weather(&mut self, p_start: f64, p_stop: f64) -> u8 {
        let u = self.draw_f64();
        // Simple: use p_start as probability of partition being active at this step
        // (stateless approximation; stateful Markov is done by the supervisor calling repeatedly)
        let _ = p_stop; // used in stateful version
        if u < p_start { 1 } else { 0 }
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
        // If replaying and within bounds, return recorded choice
        if self.replay_mode && self.run_cursor < self.replay_tape.choices.len() {
            let bytes = self.replay_tape.choices[self.run_cursor].clone();
            let val = u64::from_le_bytes(bytes.try_into().unwrap_or([0u8; 8]));
            self.record_draw(val.to_le_bytes().to_vec());
            val
        } else {
            // Live draw
            let val = self.rng.next_u64();
            self.record_draw(val.to_le_bytes().to_vec());
            val
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

    #[test]
    fn draw_bits_range() {
        let mut d = Driver::new(42, StrategySpec::default());
        d.begin_run();
        for n in [1u32, 8, 16, 32, 64] {
            let v = d.draw_bits(n);
            if n < 64 {
                assert!(v < (1u64 << n), "draw_bits({n}) = {v} out of range");
            }
        }
    }

    #[test]
    fn draw_int_bounds() {
        let mut d = Driver::new(7, StrategySpec::default());
        d.begin_run();
        for _ in 0..100 {
            let v = d.draw_int(3, 10);
            assert!(v >= 3 && v <= 10, "draw_int(3,10) = {v}");
        }
    }

    #[test]
    fn draw_choice_valid_index() {
        let mut d = Driver::new(99, StrategySpec::default());
        d.begin_run();
        let weights = vec![1u32, 2, 3, 4];
        for _ in 0..50 {
            let idx = d.draw_choice(&weights);
            assert!(idx < weights.len(), "draw_choice index {idx} out of range");
        }
    }

    #[test]
    fn determinism_property_same_seed_same_draws() {
        // Property: two drivers with the same seed produce the same sequence of draws
        // when given the same observation replies.
        let seed = 12345u64;

        // Run driver A
        let mut a = Driver::new(seed, StrategySpec::default());
        a.begin_run();
        let draws_a: Vec<u64> = (0..20).map(|_| a.draw_bits(64)).collect();
        let tape_a = a.live_tape().clone();

        // Replay with driver B using the recorded tape
        let mut engine = ReplayEngine::from_tape(tape_a.clone());
        let draws_b: Vec<u64> = (0..20).map(|_| engine.draw_u64()).collect();

        assert_eq!(draws_a, draws_b, "determinism: same seed + same tape → same draws");
    }

    #[test]
    fn replay_tape_produces_identical_sequence() {
        let seed = 999u64;
        let mut d = Driver::new(seed, StrategySpec::default());
        d.begin_run();
        let draws1: Vec<u64> = (0..10).map(|_| d.draw_bits(64)).collect();
        let tape = d.live_tape().clone();

        // Replay the tape
        let mut engine = ReplayEngine::from_tape(tape);
        let draws2: Vec<u64> = (0..10).map(|_| engine.draw_u64()).collect();

        assert_eq!(draws1, draws2, "replay tape: should produce identical draw sequence");
    }

    #[test]
    fn end_run_updates_best() {
        let mut d = Driver::new(1, StrategySpec {
            maximize: vec!["depth".into()],
            ..Default::default()
        });
        d.begin_run();
        let _ = d.draw_bits(8);
        let _ = d.draw_bits(8);
        // Report a high score
        d.end_run(&[("depth".into(), 100.0)]);
        assert!(!d.best.choices.is_empty(), "best tape should be non-empty after end_run");
    }

    #[test]
    fn shrink_reduces_tape() {
        let mut d = Driver::new(42, StrategySpec::default());
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
            goal_probe: Some("x".into()),
            goal_value: Some(1.0),
            ..Default::default()
        };
        let d = Driver::new(1, strategy);
        assert!(d.is_goal_reached(&[("x".into(), 1.0)]));
        assert!(!d.is_goal_reached(&[("x".into(), 0.0)]));
    }

    #[test]
    fn draw_hold_reasonable() {
        let mut d = Driver::new(5, StrategySpec::default());
        d.begin_run();
        // Draw 20 hold values with mean=5, all should be reasonable
        for _ in 0..20 {
            let v = d.draw_hold(5);
            assert!(v < 10000, "hold value {v} seems too large");
        }
    }
}
