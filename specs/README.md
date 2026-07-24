<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Specifications

Design documentation for Baud, deterministic-validation infrastructure for distributed systems. Guest
programs run under a deterministic supervisor so that execution is a pure function of
`(binary, manifest, tape)`; the input tape is then pure-fuzzed toward a strategy goal, with every
observation journaled so any run can be replayed, shrunk, streamed as video, or reconstructed from the
journal alone.

**Implementation Plan:** [../todo.md](../todo.md) — milestones (H0–H3, M0–M9), CLI drive scripts, risks

## Core (determinism-critical)

Owned from scratch; everything determinism depends on.

| Spec | Code | Purpose |
|------|------|---------|
| [baud-multiverse.md](./baud-multiverse.md) | [crates/baud-multiverse](../crates/baud-multiverse/) | Deterministic supervisor (first deliverable) |
| [baud-proto.md](./baud-proto.md) | [crates/baud-proto](../crates/baud-proto/) | Wire & domain types |
| [baud-driver.md](./baud-driver.md) | [crates/baud-driver](../crates/baud-driver/) | Fuzzing engine (tape / strategy / tactics / shrink) |
| [baud-journal.md](./baud-journal.md) | [crates/baud-journal](../crates/baud-journal/) | Journal, replay, reconstruction (age-encrypted at rest) |

## Orchestration

| Spec | Code | Purpose |
|------|------|---------|
| [baud-server.md](./baud-server.md) | [crates/baud-server](../crates/baud-server/) | Local daemon, orchestration, reconstruction |
| [baud-cli.md](./baud-cli.md) | [crates/baud-cli](../crates/baud-cli/) | The `baud` command surface |

## Sandboxing & Provisioning

| Spec | Code | Purpose |
|------|------|---------|
| [baud-tape.md](./baud-tape.md) | [crates/baud-tape](../crates/baud-tape/) | Sandbox backend (Daytona) |
| [baud-tape-local.md](./baud-tape-local.md) | [crates/baud-tape-local](../crates/baud-tape-local/) | Sandbox backend (local subprocess) |
| [baud-tape-agent.md](./baud-tape-agent.md) | [crates/baud-tape-agent](../crates/baud-tape-agent/) | In-sandbox agent |
| [baud-init.md](./baud-init.md) | [crates/baud-init](../crates/baud-init/) | Provisioning + adapters |
| [baud-packages.md](./baud-packages.md) | [crates/baud-packages](../crates/baud-packages/) | Guest builds from pinned Nix |

## Observation

| Spec | Code | Purpose |
|------|------|---------|
| [baud-tracing.md](./baud-tracing.md) | [crates/baud-tracing](../crates/baud-tracing/) | Kernel-side observation plane (eBPF) |
| [baud-stream.md](./baud-stream.md) | [crates/baud-stream](../crates/baud-stream/) | Frame capture / fingerprint / render |

## Security & Identity

| Spec | Code | Purpose |
|------|------|---------|
| [baud-secret.md](./baud-secret.md) | [crates/baud-secret](../crates/baud-secret/) | Type-safe secret wrapper |
| [baud-identity.md](./baud-identity.md) | [crates/baud-identity](../crates/baud-identity/) | Workload identity (ed25519 JWTs) |
| [baud-keys.md](./baud-keys.md) | [crates/baud-keys](../crates/baud-keys/) | Secrets at rest (sops + age) |

## Targets

| Spec | Code | Purpose |
|------|------|---------|
| [baud-raftlet.md](./baud-raftlet.md) | [crates/baud-raftlet](../crates/baud-raftlet/) | Validation target (planted-bug distributed toy) |

## Determinism Contract

All guests obey, and the supervisor enforces:

| Rule | Enforcement |
|------|-------------|
| One thread, one process per guest | `clone`/`fork`/`vfork`/`execve` → kill with report |
| No async signal delivery | Only synchronous faults reach guests |
| No wall clocks | Virtual time served per syscall/quantum |
| No `rdtsc`/`cpuid` free execution | Trapped and emulated |
| All entropy from the tape | `getrandom`, `/dev/urandom`, `AT_RANDOM` served from draws |
| Fixed memory layout | `ADDR_NO_RANDOMIZE`; layout in manifest |
| Statically linked, no-PIE, musl | Built by baud-packages |
| Syscalls outside the allowlist | Kill with report |

Execution is a pure function of `(binary, manifest, tape)`, verified by double-run observation-stream-hash
equality (`baud verify determinism`).

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Completed (including budget/time limit) |
| 1 | Error |
| 2 | Goal reached or invariant/property violated |
