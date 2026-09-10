<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# BAUD — Implementation Plan (deterministic-hypervisor)

Focus on these files first; the whole project is readable:

- `todo-plan.md`
- `todo-build.md`
- `specs/README.md`

Also read the design documents, source modules, tests, and drive scripts selected by the standing task group
for the current pass. The group entries in `todo-plan.md` are the durable goals; this file is the terse,
implementation-ready decomposition of the work still required to reach them.

Implement the complete deterministic guest-machine system described by the plan: the KVM machine, real Linux
boot and image pipeline, tape and observation contract, state capture and continuation, exploration engine,
server and command surface, host substrate, full-distribution proof, and generic interactive target. Existing
code is evidence, not permission to leave a placeholder. Every open item below must identify the complete
outcome, affected paths, next step, and acceptance test. Never add a knowingly partial implementation.

`todo-build.md` is the working queue. Keep open items terse (normally no more than six lines); collapse a
resolved item to one `DONE` sentence. Every milestone ends with a drive script and the required gate. Section
12 is the problem → specification → test matrix: every risk found in review is turned into a concrete
guarantee and the test that proves it.

## Current implementation queue

- **DONE — G1 stock-KVM run integration.** Real one-vCPU stock-KVM execution now wires fixed CPUID, work-clock, tape, console, and fail-closed exit handling through the maintained request path, with H1, H2, and H4 green on real `/dev/kvm`.

- **DONE — G3 shared-memory branching and live shell proof.** Guest RAM is memfd-backed, the raw userfaultfd continuation/write-protect ABI is tested, dirty-ring fallback remains explicit, and H5/M10 prove independent branches and persisted bidirectional shell continuation.

- **DONE — G2 real image pipeline and tape endpoint.** The real kernel/initramfs image builder, deterministic archive and hash metadata, tape-driver lint, RDSEED rewrite, guest tape device, and H7 boot path are implemented with diagnostic failures and no fixture fallback.

- **DONE — G3 snapshot integrity and live shell.** Strict XSAVE2/state lengths, plaintext body/page verification, truncation and CPU-mismatch errors, persisted bidirectional shell continuation, and cancellation pass snapshot tests plus H5 and M10 on real `/dev/kvm`.

- **DONE — G4 independent proof and frame pipeline.** Bounded tape records, deterministic driver/shrinking, encrypted store integrity, independent tracing planes, strict frame validation, and replay rendering pass the workspace and M3/M11-M13 acceptance paths.

- **DONE — G5 authenticated lifecycle and real replay surface.** Lifecycle journaling, real-KVM replay metadata, legacy compatibility replay, snapshot continuation, WebSocket shell cancellation, stable CLI errors, and M3/M10-M13/package cancellation paths are integrated and green.

- **DONE — G6 real Ubuntu H9 blocker and proof.** ACPI, PCI, virtio-blk, periodic-timer orchestration, banner-gated fingerprints, and cross-process diagnostic handling are integrated and H9 is green through the maintained drive.

- **DONE — G7 real Mario guest proof.** The generic target harness, tape-driven probes and frames, deterministic replay/reduction plumbing, and target-specific example boundary are integrated without workload names in core crates.
