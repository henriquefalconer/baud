<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud

Deterministic-validation infrastructure for distributed systems.

Baud runs guest programs under a deterministic supervisor so that execution is a pure function of
`(binary, manifest, tape)`, then pure-fuzzes the input tape toward a strategy goal — journaling every
observation so any run can be replayed, shrunk, streamed as video, or reconstructed from the journal alone.

## Documents

- [`todo.md`](./todo.md) — implementation plan: milestones, crates, risks, and the CLI drive
  scripts that validate each step.
- [`specs/`](./specs) — one normative specification per component. Start with
  [`specs/README.md`](./specs/README.md).

## Components

| Crate | Role |
| ------------------------ | ----------------------------------------- |
| `baud-multiverse`      | Deterministic supervisor (first deliverable) |
| `baud-proto`           | Wire & domain types |
| `baud-driver`          | Fuzzing engine (tape / strategy / tactics / shrink) |
| `baud-server`          | Local daemon, orchestration, reconstruction |
| `baud-cli`             | The `baud` command surface |
| `baud-tape`         | Sandbox backend (Daytona) |
| `baud-tape-local`   | Sandbox backend (local subprocess) |
| `baud-tape-agent`      | In-sandbox agent |
| `baud-init`            | Provisioning + adapters |
| `baud-packages`             | Guest builds from pinned Nix |
| `baud-journal`         | Journal + replay + reconstruction |
| `baud-tracing`            | Kernel-side observation plane |
| `baud-stream`          | Frame capture / fingerprint / render |
| `baud-secret`          | Type-safe secret wrapper |
| `baud-identity`        | Workload identity |
| `baud-keys`            | Secrets at rest |
| `baud-raftlet`         | Validation target (planted-bug distributed toy) |

## Exit Codes

| Code | Meaning |
| ---- | ------------------------------------------ |
| 0    | Completed (including budget/time limit) |
| 1    | Error |
| 2    | Goal reached or invariant/property violated |

# License

Proprietary. Copyright (c) 2026 Henrique Falconer. All rights reserved.
