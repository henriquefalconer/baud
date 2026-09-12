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

- **G1 exact-boundary interrupt proof and enforced capability.** DONE. Boundary identity and overshoot rejection use real PMU/single-step evidence, and host probing measures the loaded enforced module with named cooperative fallback; H0, H1, H2, and H4 pass on real KVM.

- **G2 reproducible image pipeline.** DONE. Pinned kernel/config assembly, deterministic initramfs and image hashes, staged diagnostics, `baud image build`, lint, and fresh-image KVM boot now pass `drive/pkg/pkg-build-cli.sh` and focused reproducibility tests.

- **G2 guest tape endpoint and harness.** DONE. The guest prefers virtio-console input with `/dev/tape` and direct PIO fallbacks, and real-KVM `guest_tape_roundtrip`, userspace boot, pinned seed, deterministic poweroff, and malformed-record tests pass.

- **G3 write-set-scaled branching.** DONE. Live continuation VMs now retain the parent's memfd, restore CPU/device state without copying RAM pages, resolve UFFD minor/write-protect faults in a worker, and retain explicit full-restore fallback; snapshot, KVM, and H5 coverage pass.

- **G3 serial wake for shell continuation.** DONE. Eventfd-backed UART receive wakes are drained into direct IRQ4 injection, and the WebSocket shell-into path preserves ordered output, cancellation, restore, disconnect, blocked-run, and determinism-hole errors.

- **G4 complete deterministic driver tactics and scheduling.** DONE. Configured input/weather tactics, deterministic grid buckets, replay/resume state, bounded reservoir growth, hold-shortening shrinking, neutral malformed-parameter paths, and focused tests now run through the driver and M3 acceptance path.

- **G5 authenticated lifecycle and public API contract.** DONE. Bearer and signed identity authentication, journal-before-ack ownership, cancellation, watchdog diagnostics, SSE progress, redaction, and stable exit mapping are wired through server/client routes and covered by workspace and drive validation.

- **G5 unified run ownership and image memory.** DONE. Shared cancellation/resource ownership now spans KVM, replay, rendering, restore/branch, abort, disconnect, and watchdog paths; cleanup guards prevent canceled work from reporting success, with workspace and drive coverage green.

- **G5 complete advertised CLI and route coverage.** DONE. Advertised commands, routes, aliases, schemas, redaction, identifier errors, stable exits, tape reconstruction, and SSE frame tails are synchronized; live frame payloads now persist when supplied and migrations cover fresh/upgraded databases.

- **G6 Ubuntu H9 proof.** Use pinned Ubuntu 18.04.1/Linux 4.15 vector `0xee` (238) and finish the real rootfs/userspace path to `ubuntu login:`; current evidence reaches `systemd-udevd` but Ubuntu never advances virtio-blk status beyond ACK or posts a queue request, so add PCI interrupt/MSI routing or the matching virtio-pci capability path next, then rerun `drive/h/h9.sh` and require the two-VM fingerprint tests.


- **G8 production host safety contract.** DONE. Linux probing, regime reporting, sibling-safe placement, inherited affinity selection, housekeeping reservations, doctor diagnostics, and real H0/H6 capacity/fleet checks now pass without silent downgrade.

- **G8 relocate distributed validation images.** DONE. Authoritative Linux image artifacts and manifest now live under `examples/linux-guest`, all production references use that path, the gate rejects stale fixture references, and real two-build plus H-series/generic-core checks pass.
