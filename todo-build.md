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

- **G1 stock-KVM run integration.** Affected paths: `crates/baud-multiverse/src/lib.rs`, `crates/baud-multiverse/src/linux/`, `src/{cpuid.rs,timesource.rs,tape_bus.rs}`, `crates/baud-vcpu/src/{lib.rs,boundary.rs,linux/}`, and active server callers under `crates/baud-server/src/routes/{run_kvm,replay,verify}.rs`. Replace the still-active simulation request paths with the real one-vCPU stock-KVM boot and exit loop, wire CPUID, work-clock, tape, console, and fail-closed dispatch into every production request path, and report cooperative guarantees without claiming enforced ones. Acceptance: `double_boot_memory_identical`, `cpuid_leaves_are_fixed`, `work_clock_is_monotone_and_reproducible`, `all_input_is_tape_derived`, and `no_unmodeled_exit_is_silent` pass through `drive/h/h1.sh`, `drive/h/h2.sh`, and `drive/h/h4.sh`; unavailable KVM, unsupported exits, or unavailable enforcement return diagnostic errors rather than falling back to simulation.

- **G3 shared-memory branching and live shell proof.** Affected paths: `crates/baud-multiverse/src/linux/`, `crates/baud-snapshot/src/`, `crates/baud-server/src/routes/shell_into.rs`, `crates/baud-cli/src/cmds/shell_into.rs`, and `drive/h/h5.sh`. Replace full-RAM branch copies with shared memfd backing plus userfaultfd continue/write-protect handling, retain the full-restore fallback and dirty-ring reset, and add the persisted-universe shell-into path to the maintained H5 drive. Dependencies: the G1 stock-KVM path and existing snapshot CPU/state validation. Acceptance: `thousand_branches_are_independent_and_deterministic`, `reset_cost_scales_with_write_set`, and `shell_into_universe_resumes` prove write-set-scaled memory, independent branches, bidirectional shell I/O, and restore errors; unsupported userfaultfd, missing keys, mismatched CPUs, dirty-ring failures, and unavailable shell resources fail explicitly rather than falling back silently.

- **G2 real image pipeline and tape endpoint.** Affected paths: `crates/baud-packages/`, `crates/baud-tape-device/`, `crates/baud-tape-agent/`, `crates/baud-multiverse/src/`, `drive/pkg/`, and `drive/h/h7.sh`. Replace the source-fixture builder with a spec-driven Buildroot bring-up that can later select the pinned Nix path, package the actual guest tape transport, apply rdseed rewriting and lint as build gates, emit the combined image hash and site metadata, and boot the emitted artifacts twice. Dependencies: the G1 real run contract and a chosen kernel/userspace tape-driver design. Acceptance: `image_build_is_reproducible`, `image_lint_requires_tape_driver`, `no_rdseed_opcode_survives_in_image`, `guest_tape_roundtrip`, and H7 pass through `drive/pkg/pkg-image-build.sh`, `drive/pkg/pkg-build-cli.sh`, and `drive/h/h7.sh`; missing tools, invalid config, absent endpoint, residual rdseed, timeout, or hash mismatch fails with diagnostics and never falls back to a fixture or open bus.

- **G3 snapshot integrity and live shell.** Affected paths: `crates/baud-snapshot/{src/}`, `crates/baud-snapshot-store/src/`, `crates/baud-server/src/routes/shell_into.rs`, `crates/baud-cli/src/cmds/shell_into.rs`, and `drive/h/h5.sh`. Make XSAVE2 and restore fields strict, verify plaintext-addressed body/page hashes, reject truncated records, and promote persisted shell-into with bounded disconnect cancellation and explicit polling-versus-IRQ capability reporting. Dependencies: existing G1 KVM path and snapshot CPU/state validation. Acceptance: `snapshot_roundtrip_is_bit_identical`, tamper/truncation tests, `shell_into_universe_resumes`, and H5/M10 prove independent branches, typed corruption errors, bidirectional I/O, and no orphaned vCPU; unsupported userfaultfd, missing keys, CPU mismatch, malformed state, and disconnects fail explicitly or report the documented fallback.

- **G4 independent proof and frame pipeline.** Affected paths: `crates/baud-driver/`, `crates/baud-proto/`, `crates/baud-tape-device/`, `crates/baud-snapshot-store/`, `crates/baud-tracing/`, `crates/baud-stream/`, and server stream/tracing routes. Complete bounded tape/record validation, scheduler tactics and nearest-universe shrinking, store integrity checks, independent observation-plane selection, and incremental frame streaming with strict geometry/hash validation. Dependencies: snapshot-tree APIs and the G2 guest endpoint for end-to-end frame proof. Acceptance: `driver_is_reproducible`, `shrink_reproduces_from_nearest_snapshot`, `planes_agree_on_healthy_run`, frame double-run/render tests, ciphertext/reconstruction tests, and drives M3/M5/M7/M11/M13; malformed or oversized records, corrupt pages, unavailable probes, bad frames, missing replay bytes, and disconnects return typed non-success results, never synthetic success.

- **G5 authenticated lifecycle and real replay surface.** Affected paths: `crates/baud-server/src/routes/{runs,run_kvm,replay,shrink,stream,shell_into}.rs`, `crates/baud-cli/src/`, `crates/baud-identity/`, `crates/baud-keys/`, migrations, and M drives. Replace simulation-only lifecycle/replay/shrink with stored tape and snapshot-backed execution, add journal-before-ack ownership, authenticated REST/WebSocket agent access, live status/streaming, rewind, and cancellation that kicks every KVM loop. Dependencies: G1-G4 contracts and a token subject/scope decision. Acceptance: lifecycle kill/reconstruct, replay/shrink, fingerprint/stream, auth, stable exit-code, and disconnect tests pass through M10-M13 and package cancellation drives; missing artifacts, invalid scope, client death, watchdog, malformed range, and backend failure return non-2xx JSON/structured errors and release resources without claiming success.

- **G6 real Ubuntu H9 blocker and proof.** Affected paths: `examples/ubuntu/`, `crates/baud-server/src/routes/verify_fingerprint.rs`, `crates/baud-multiverse/src/{acpi,pci,virtio_blk}.rs`, `crates/baud-fingerprint/`, and `drive/h/h9.sh`. Validate the exact Ubuntu artifact manifest and complete the combined ACPI/PCI/virtio-blk/timer H9 runner, then resolve the current zero-exit stall in measured order using matching symbols, initramfs udev handlers, and controlled network parameters before asserting login. Dependencies: locally fetched Ubuntu artifacts, G1 RCB host acceptance, and real G2 image/device contracts. Acceptance: `ubuntu_boots_to_login`, `timed_exit_fingerprint_is_stable`, and `cross_vm_fingerprint_matches` compare banner, events, RIP, GPA, and canonical RAM across two processes/cores; missing metadata, stall, timeout, unsupported device, or no banner remains a diagnostic failure, never a pass.

- **G7 real Mario guest proof.** Affected paths: `examples/mario/`, `crates/baud-packages/`, `crates/baud-driver/`, `crates/baud-stream/`, and a new `drive/mario.sh`. Build an image containing unmodified FCEUX, Lua harness, user ROM, deterministic init, and two tape-backed channels, then connect generic goal search, probes, frames, nearest-snapshot reconstruction, shrink/replay, and live output without core game knowledge. Dependencies: G2 pipeline and endpoint, plus an allowed homebrew ROM and the FCEUX SDL/X11/Xvfb closure. Acceptance: the drive builds/lints, proves identical probe/frame streams, random negative control, sticky-mask goal reach, reconstruction, reduced replay, and re-rendered frames while harder-ROM progress is non-gating; missing ROM/dependency, failed goal, unsupported display, timeout, or mismatch fails diagnostically and never switches to simulation or synthetic frames.

- **DONE G8 host module.** Added the NixOS baud host module and Intel reference machine definition with explicit KVM, CPU-isolation, housekeeping, store, and secret-path configuration; existing host tests and the full gate pass.
- **DONE Gate validation.** `drive/gate.sh` passes all required checks; the documented load-flake `rdtsc_guest_reproduces_high_bits_across_boots` passes in isolation.
