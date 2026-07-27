<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud

<p align="center">
  <img src="docs/mario.gif" alt="baud driving an emulator to beat Super Mario Bros, fully deterministically" width="512">
</p>
<p align="center"><sub>Not a screen recording. baud regenerated this from the winning run's tape hash.</sub></p>

**baud beats Super Mario Bros. Point it at your program and it does the same thing.**

baud runs a program inside a deterministic environment it controls end to end. Everything the program reads
becomes one replayable tape, and baud's fuzzer searches that tape for the state you care about: a win, a
completed task, a crash. You say what winning looks like, baud finds inputs that get there, and replaying
those inputs lands in the same place every time. Super Mario Bros is just one program you can point it at.

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
