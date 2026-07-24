<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Raftlet Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-raftlet` is a validation target, not infrastructure: a small 3-node leader-election and replicated-log
toy with a deliberately planted safety bug. It demonstrates that baud finds a rare distributed-systems bug
that unguided random testing does not, and that it does so through the same CLI, adapters, and code paths as
any other workload.

### Goals

- **A realistic modal bug**: reachable only via a specific rare interleaving
- **In-harness invariants**: violations reported as `Crash{invariant}` — no temporal logic
- **Zero special treatment**: deployed via `examples/raftlet/spec.toml` like any workload
- **Small**: a specimen, not a database

### Non-Goals

- Being a correct or complete Raft implementation
- Depending on anything beyond baud-proto

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                baud-raftlet                  │
│  3 single-threaded guests over the net device  │
│  leader election + replicated log              │
│  in-harness invariant checks                   │
└──────────────────────────────────────────────┘
        ▲ a TARGET, deployed via examples/raftlet/spec.toml
```

### Rationale

- < 1,000 LOC, no deps beyond baud-proto. Each node is a single-threaded guest speaking via the net device.

---

## 3. The Planted Bug

A safety violation — two nodes commit different values at the same log index — reachable only via:

```
leader-election  ×  in-flight-truncation  ×  second-partition
```

Sequential luck with multiplied probabilities: unreachable by white-noise packet-dropping within budget,
reachable by guided exploration with stateful weather.

---

## 4. Invariants (checked in-harness)

| Invariant | Meaning |
| ------------------------- | ------------------------------------------ |
| single-leader-per-term    | At most one leader per term |
| log-prefix-agreement      | Committed logs never disagree at an index |

A violated invariant emits `Crash{invariant: "log_prefix_agreement"}`.

---

## 5. Strategy & Tactics (for the drive)

| Aspect | Value |
| ---------- | ------------------------------------------------ |
| strategy   | `maximize = ["probe:op_depth"]`, `buckets = ["probe:leader_count","probe:partition_state","probe:term_band"]` |
| tactics    | `markov-partition` + `crash-restart` (finds it); `random-drops` (negative control, does not) |

---

## 6. Testing (M6 drive)

```rust
#[test]
fn planted_bug_needs_the_interleaving() {
    // per-packet random drops never trip it within budget
    assert!(run(random_drops(), budget).outcome.is_none());
    // election × in-flight truncation × second partition does
    assert!(matches!(run(guided(), budget).outcome,
        Some(Crash { invariant: Some(i), .. }) if i == "log_prefix_agreement"));
}
```

The M6 drive script wraps the same sequence: negative control → guided run reaching `Crash{invariant}`
(exit `2`) → `net weather --run` shows the causal timeline → mid-run `tape kill` + `reconstruct` + resume →
`shrink` → `replay` reproduces the violation.

---

## 7. Considerations

| Concern              | Handling                                    |
| -------------------- | ------------------------------------------- |
| Node footprint       | < 10 MiB RSS each (1 GiB sandbox)           |
| Special requirements | None; same code path as every workload      |
| Bug stability        | The planted interleaving is the only path to the violation |

---

## 8. Security Considerations

| Threat                        | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| Target treated as trusted     | It is a guest under the supervisor; same mediation as any workload |
| Node footprint                | < 10 MiB RSS each; bounded logs             |

---

## 9. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| More invariants    | Leader completeness, commit monotonicity       |
| Second planted bug | A liveness-shaped defect for a distinct exploration challenge |
