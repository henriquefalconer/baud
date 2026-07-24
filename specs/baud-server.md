<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Server Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-server` is the local daemon that orchestrates runs. It drives the fuzzing loop, provisions sandboxes
through a backend, journals every draw and observation, and owns reconstruction and the sandbox-minute
budget. Once it exists, all functionality is driven and tested through it via the CLI.

### Goals

- **Single control point**: every capability reachable through one localhost API
- **Journal-first**: durably record before letting a run proceed past a checkpoint
- **Backend-agnostic**: same logic over Daytona or local subprocess
- **No workload interpretation**: stores and serves opaque named series

### Non-Goals

- Remote/multi-tenant operation (localhost only)
- Business logic in the CLI (the CLI is a thin client)

---

## 2. Crate Architecture

```
┌───────────────────────────────────────────────────────────┐
│                      baud-server                          │
│  axum (localhost) · SQLite (metadata) · CAS files (journal) │
│  drives: baud-driver   provisions: Backend trait          │
│  reconstruct · budget · REST + SSE                          │
└───────────────────────────────────────────────────────────┘
     ▲ baud-cli          ▼ baud-tape-agent (per sandbox)
```

### Rationale

- Storage = SQLite (metadata) + flat content-addressed files (journal). No external services, no ORM.
- Every endpoint exists because a CLI subcommand needs it, 1:1.

---

## 3. Run Model

```rust
struct Run {
    id: RunId,
    spec_hash: Hash,
    closure_hash: Hash,
    seed: u64,
    strategy: StrategySpec,
    tactics: TacticsSpec,
    journal: JournalRef,
    status: RunStatus,   // Provisioning | Running | Paused | Done{code} | Diverged
}
```

The server stores probe streams, syscall logs, eBPF streams, and frame hashes as named series without
interpreting workload semantics.

---

## 4. Responsibilities

| Area | Behavior |
| ------------------- | ---------------------------------------------------------- |
| Fuzz loop           | Feed driver draws to the agent, collect observations, score, iterate |
| Journal             | Append draws/observations to CAS before checkpoint acknowledgement |
| Reconstruction      | `(manifest + tape prefix) → new sandbox → replay → verify → resume` |
| Budget              | Track sandbox-minutes vs replay cost; expose via `baud budget` |
| Lifecycle           | `ensure` restarts stopped, restores archived, reconstructs deleted tapes |
| Verification        | Serve `verify determinism` and `verify observation` |

---

## 5. Transport

- REST for commands and queries; SSE for live tails (`run watch`, `obs tail`, `syscalls tail`, `tracing tail`,
  `stream tail`).
- Agent connection is authenticated by a minted identity token (baud-identity); nothing unauthenticated.

---

## 6. Testing

```rust
#[tokio::test]
async fn journal_first_survives_sandbox_kill() {
    let run = server.start(spec).await;
    server.ack_through(run, step).await;
    backend.kill(run.tape).await;
    assert_eq!(server.journal(run).last_acked(), step);
}
```

- End-to-end through the CLI at every milestone (`drive/*.sh`).
- Reconstruction determinism: a reconstructed run's observation-stream hash prefix equals the original.

---

## 7. Security Considerations

| Concern                  | Handling                                    |
| ------------------------ | ------------------------------------------- |
| Bind exposure            | Localhost only                              |
| Secret handling          | Tokens held as `SecretString`; never logged |
| Agent authenticity       | Per-tape identity token required on connect  |

---

## 8. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| Parallel runs      | Multiple sandboxes exploring different seeds   |
| Coverage store     | Persist branch/syscall coverage for strategy   |
