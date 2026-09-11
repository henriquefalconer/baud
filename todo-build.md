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

- **G2 reproducible image pipeline.** Finish the Buildroot bring-up and pinned Nix kernel/initramfs/userspace path in `crates/baud-packages`, including deterministic newc archives, image hashes, store warming, and double-build verification, then wire `baud image build` and lint without fixture fallback. Acceptance is `drive/pkg/pkg-build-cli.sh`, the maintained real-image build drive, `image_build_is_reproducible`, and real-image entropy/lint checks; any non-reproducible stage or missing prerequisite must fail with its stage and diagnostic.

- **G2 guest tape endpoint and harness.** Implement the preferred virtio-serial endpoint and documented PIO/character-device fallback across `crates/baud-tape-device`, `crates/baud-multiverse`, and the guest image, then run the one-record-per-step generic harness against a freshly built image. Pass `guest_tape_roundtrip`, `guest_kernel_boots_to_userspace`, `boot_params_seed_is_pinned`, `init_powers_off_deterministically`, and the real-image drive; missing endpoint, malformed records, unavailable tape input, or unsupported kernel configuration must fail closed.

- **G3 write-set-scaled branching.** Connect `crates/baud-snapshot/src/userfaultfd.rs` to shared memfd-backed guest RAM in `crates/baud-multiverse`, with minor-fault continuation, write protection, per-branch isolation, and an explicit full-restore fallback. Prove unchanged-page sharing and private dirty pages with `thousand_branches_are_independent_and_deterministic`, write-set memory measurements, and `drive/h/h5.sh`; unsupported UFFD or memfd negotiation must select the documented fallback and never claim scaling.

- **G3 serial wake for shell continuation.** DONE. Eventfd-backed UART receive wakes are drained into direct IRQ4 injection, and the WebSocket shell-into path preserves ordered output, cancellation, restore, disconnect, blocked-run, and determinism-hole errors.

- **G4 complete deterministic driver tactics and scheduling.** DONE. Configured input/weather tactics, deterministic grid buckets, replay/resume state, bounded reservoir growth, hold-shortening shrinking, neutral malformed-parameter paths, and focused tests now run through the driver and M3 acceptance path.

- **G5 authenticated lifecycle and public API contract.** Implement agent/client authentication, journal-before-ack ownership, disconnect cancellation, bounded watchdogs, live progress/watch delivery, and consistent HTTP/JSON/stable-exit mapping across `crates/baud-server/src/{main.rs,routes,state.rs}` and `crates/baud-cli/src/client.rs`, then update migrations, specs, and end-to-end drives. Acceptance must cover authenticated and unauthenticated requests, backend kill after journal, reboot and restore cancellation, watchdog diagnostics, live watch, redaction, and exit codes 0/1/2; timeout or cancellation must never become success.

- **G5 unified run ownership and image memory.** Share one cancellation and ownership registry across `run_kvm`, replay, frame rendering, branch/restore tasks, abort handlers, and watchdogs in `crates/baud-server/src/routes/{run_kvm,replay,stream,runs}.rs` and `state.rs`. Add a drive covering boot, replay, render, restore/branch, abort, disconnect, RSS/image mappings, cleanup, and terminal status; canceled or timed-out work must release KVM/image resources and report failure, never `done`.

- **G5 complete advertised CLI and route coverage.** Synchronize `crates/baud-cli/src/cmds`, `crates/baud-server/src/routes`, migrations, specs, and drives so every advertised command has its route, JSON schema, redaction, stable exit behavior, and malformed/missing-identifier errors. Replace stubbed tape reconstruction and non-SSE stream tail, fix capability-route mismatches, and test fresh plus upgraded databases in a maintained CLI drive; unsupported operations must return structured non-success responses, not HTTP 200 success.

- **G6 Ubuntu H9 proof.** Finish `examples/ubuntu`, `crates/baud-multiverse`, `crates/baud-fingerprint`, CLI wiring, and `drive/h/h9.sh` by using Linux `LOCAL_TIMER_VECTOR` 0xec and fixing the remaining post-`Freeing unused kernel memory` rootfs/userspace progress gap in the virtio-blk/timer run loop, then validate exact artifacts and compare two independent VMs at `ubuntu login:`; pass `ubuntu_boots_to_login`, `timed_exit_fingerprint_is_stable`, and `cross_vm_fingerprint_matches` with diagnostic timeout evidence never treated as success.


- **G8 production host safety contract.** DONE. Linux probing, regime reporting, sibling-safe placement, inherited affinity selection, housekeeping reservations, doctor diagnostics, and real H0/H6 capacity/fleet checks now pass without silent downgrade.

- **G8 relocate distributed validation images.** DONE. Authoritative Linux image artifacts and manifest now live under `examples/linux-guest`, all production references use that path, the gate rejects stale fixture references, and real two-build plus H-series/generic-core checks pass.
