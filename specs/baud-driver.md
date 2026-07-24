<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Driver Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-driver` is the fuzzing engine. It is the source of all randomness in a run: device models request
draws, the recorded draws are the tape, and the driver decides which tapes to grow, keep, splice, and shrink
in pursuit of a strategy goal.

### Goals

- **Driver-as-randomness-source**: every nondeterministic decision is a draw the driver produced
- **Guided search**: strategy scores + reservoir + grid buckets steer exploration past local maxima
- **Deterministic replay**: same seed + same observation replies ⇒ byte-identical tape
- **Tape shrinking**: reduce a failing tape to a minimal reproducer

### Non-Goals

- Any IO, async, or threads (pure library)
- Any workload knowledge (sees only bytes in, named numbers out)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                baud-driver                   │
│  PRNG · draw API · corpus · scheduler · shrink │
│  Pure. No IO. Deps: rand_chacha, serde, proto  │
└──────────────────────────────────────────────┘
        ▲ used by baud-server (drives the loop)
```

### Rationale

- Deps = `{rand_chacha, serde, baud-proto}`; no IO so it is trivially testable and deterministic.

---

## 3. Draw API

```rust
impl Driver {
    fn draw_bits(&mut self, n: u32) -> Vec<u8>;
    fn draw_int(&mut self, lo: i64, hi: i64) -> i64;
    fn draw_choice(&mut self, weights: &[u32]) -> usize;
    fn draw_hold(&mut self, geom_mean: u32) -> u32;         // button-holding statefulness
    fn draw_weather(&mut self, p: MarkovParams) -> Weather; // stateful network condition
}
```

---

## 4. Corpus & Scheduler

| Element | Behavior |
| ------------------- | ---------------------------------------------------------- |
| Current-best tape   | Highest strategy score seen |
| Reservoir           | Sample of earlier prefixes; chosen with `p_backoff` (escapes doomed dead-ends) |
| Grid buckets        | Discretized probe tuples; exploration equalized across incomparable cells |
| Scheduler moves     | extend-best · mutate · splice-from-reservoir |

Strategy scoring reads `{probe name → numeric value}` from the observation stream; the driver never names a
button, packet, or node.

---

## 5. Shrinker

Passes over the choice sequence, each replays and keeps the reduction only if the outcome is preserved:

- chunk deletion
- zeroing
- hold-shortening
- block dedup

For clusters the same passes reduce the fault schedule. Deliverable = smallest ops + faults reproducing the
outcome.

### API

```rust
impl Driver {
    fn new(seed: u64, strategy: StrategySpec, tactics: TacticsSpec) -> Self;
    fn next_draw(&mut self, req: DrawRequest) -> DrawResult;
    fn report_observation(&mut self, obs: Observation);
    fn shrink(&mut self, tape: Tape, still_fails: impl Fn(&Tape) -> bool) -> Tape;
}
```

---

## 6. Testing

```rust
// M3 gate
#[test]
fn same_seed_same_replies_same_tape() {
    let a = run_driver(seed, &script);
    let b = run_driver(seed, &script);
    assert_eq!(a.tape_bytes(), b.tape_bytes());
}
```

- White-noise tactics plateau on a depth probe; `stateful-mask` penetrates (parser workload).
- Shrinking a known failing tape converges to a fixed minimal length.

---

## 7. Tactics Reference

| Kind | Tactic | Notes |
| -------- | ---------------------------- | ------------------------------------- |
| input    | `random`                     | negative control; expected to plateau |
| input    | `stateful-mask{p_flip}`      | remember previous byte, flip bits low-prob |
| input    | `hold{geom_mean}`            | geometric hold durations |
| weather  | `markov-partition{p_start,p_stop}` | stateful partitions |
| weather  | `burst-delay{regimes}`       | bursty latency |
| weather  | `crash-restart{p,min_up_ticks}` | node crash/restart schedule |
| schedule | `switch-bias{weights}`       | cross-guest switch-order distribution |

---

## 8. Security Considerations

| Threat                          | Handling                                    |
| ------------------------------- | ------------------------------------------- |
| Adversarial observations skew search | Probe values are untrusted numbers; none is executed |
| Non-reproducible tape           | Pure ChaCha PRNG; the determinism property test gates releases |
| Unbounded corpus growth         | Reservoir is capped; grid buckets discretized |

---

## 9. Future Considerations

| Feature            | Description                                        |
| ------------------ | -------------------------------------------------- |
| WASM escape hatch  | User `score(obs)->f64` and `mutate(prev,rand)->bytes` modules |
| Coverage feedback  | Feed syscall/branch signals into strategy scoring  |
