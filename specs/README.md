<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Specifications

Design documentation for Baud. baud runs a whole guest machine inside a virtual machine (Linux KVM +
Intel VT-x) and makes its execution a reproducible function of one input tape, then fuzzes the tape while
snapshotting any moment and forking many continuations that share memory — a branching tree of universes.

**Implementation Plan:** [../todo.md](../todo.md) — milestones (H0–H6, M-series), drive scripts, and the
problem → specification → test matrix.

## Core (determinism-critical)

The deterministic VMM and everything execution reproducibility depends on.

| Spec | Code | Purpose |
|------|------|---------|
| [baud-multiverse.md](./baud-multiverse.md) | [crates/baud-multiverse](../crates/baud-multiverse/) | Deterministic KVM/VT-x VMM (first deliverable) |
| [baud-vcpu.md](./baud-vcpu.md) | [crates/baud-vcpu](../crates/baud-vcpu/) | Single-vCPU state machine + exit dispatch + interrupt injection |
| [baud-tape-device.md](./baud-tape-device.md) | [crates/baud-tape-device](../crates/baud-tape-device/) | Paravirtual device — the sole nondeterministic-input channel |
| [baud-snapshot.md](./baud-snapshot.md) | [crates/baud-snapshot](../crates/baud-snapshot/) | Universe capture/restore + userfaultfd CoW branching + rewind |
| [baud-snapshot-store.md](./baud-snapshot-store.md) | [crates/baud-snapshot-store](../crates/baud-snapshot-store/) | Durable branch tree of universes (age-encrypted) — supersedes the journal |
| [baud-proto.md](./baud-proto.md) | [crates/baud-proto](../crates/baud-proto/) | Wire & domain types incl. hypercall/probe messages |
| [baud-driver.md](./baud-driver.md) | [crates/baud-driver](../crates/baud-driver/) | Tape/fuzzing engine + snapshot-tree exploration |

## Host & Orchestration

| Spec | Code | Purpose |
|------|------|---------|
| [baud-host.md](./baud-host.md) | [crates/baud-host](../crates/baud-host/) | KVM-capable host: probe, regime decision, one-core-per-VM fleet |
| [baud-server.md](./baud-server.md) | [crates/baud-server](../crates/baud-server/) | Local daemon, orchestration, reconstruction |
| [baud-cli.md](./baud-cli.md) | [crates/baud-cli](../crates/baud-cli/) | The `baud` command surface (adds `host`/`image`/`snapshot`/`branch`/`rewind`/`shell-into`) |

## Guest Images & Provisioning

| Spec | Code | Purpose |
|------|------|---------|
| [baud-packages.md](./baud-packages.md) | [crates/baud-packages](../crates/baud-packages/) | Builds reproducible bootable guest images (kernel + rootfs + agent) |

## Observation

| Spec | Code | Purpose |
|------|------|---------|
| [baud-tracing.md](./baud-tracing.md) | [crates/baud-tracing](../crates/baud-tracing/) | Cross-check plane (VMM exit log vs independent witness) |
| [baud-stream.md](./baud-stream.md) | [crates/baud-stream](../crates/baud-stream/) | Guest framebuffer capture / fingerprint / render |

## Security & Identity

| Spec | Code | Purpose |
|------|------|---------|
| [baud-secret.md](./baud-secret.md) | [crates/baud-secret](../crates/baud-secret/) | Type-safe secret wrapper |
| [baud-identity.md](./baud-identity.md) | [crates/baud-identity](../crates/baud-identity/) | Workload identity (ed25519 JWTs) |
| [baud-keys.md](./baud-keys.md) | [crates/baud-keys](../crates/baud-keys/) | Secrets at rest (sops + age) |

## Targets

| Spec | Code | Purpose |
|------|------|---------|
| [baud-raftlet.md](./baud-raftlet.md) | examples/raftlet (guest image) | Validation target (planted-bug distributed toy) |

## Superseded

| Spec | Note |
|------|------|
| baud-journal.md | Superseded by `baud-snapshot` + `baud-snapshot-store` (snapshot-branch replaces replay-from-zero) |
| baud-tape.md / baud-tape-local.md / baud-tape-agent.md / baud-init.md | Container-sandbox model; replaced by `baud-host` (KVM hosts) + guest images. Retained for history until removed. |

## Determinism Model

baud makes a whole guest machine reproducible by owning it at the virtualization layer:

| Source | Handling |
|--------|----------|
| CPUID (RDRAND/RDSEED/TSX/x2APIC/topology) | Always exits under VT-x; served fixed; nondeterministic bits masked |
| RDTSC / time | Work-clock (retired conditional branches); TSC offset/scale (cooperative) or forced exit (enforced) |
| Randomness | Masked in CPUID (cooperative) or hardware-trapped and tape-served (enforced) |
| External input / entropy | Served from the tape via the tape device |
| Interrupt timing | Injected at an exact instruction boundary (arm-early-then-single-step) |
| Memory | Zeroed RAM at fixed addresses |
| Any unmodeled exit | Fails loud (`DeterminismHole`) |

Two regimes: **cooperative** (stock KVM; reproducible for guests that take entropy/clock/input from the tape
device) and **enforced** (custom KVM module; hardware-traps the raw random and timestamp instructions). Each
run records its regime; guarantees are reported only for the regime in force.

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Completed (including budget/time limit) |
| 1 | Error |
| 2 | Goal reached or invariant/property violated |
