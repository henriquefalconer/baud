<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# BAUD — Standing Task Plan

This is the standing work queue derived from `todo-build.md` and the specifications under `specs/`.
It is continuous: completed work is recorded as a short `DONE` line, but the task groups remain in
place for the next pass. Each Ralph iteration selects the highest-priority unfinished item in one
group, completes that item as one coherent change, validates it through the applicable drive and gate,
and then returns to the queue.

## Priority order

1. **G1 — Deterministic machine core**
2. **G2 — Real Linux boot and image pipeline**
3. **G3 — Snapshot, branching, and live continuation**
4. **G4 — Tape, observations, exploration, and proof**
5. **G5 — Server and command surface**
6. **G6 — Full distro validation**
7. **G7 — Generic interactive target**
8. **G8 — Host, packaging, security, and release operations**

Priority may be temporarily superseded only by a blocker that prevents progress in a higher group.
The selected item is the iteration contract; do not silently replace it with a smaller or unrelated
item.

---

## G1 — Deterministic machine core

**Purpose:** make one-vCPU KVM execution deterministic, observable, and fail-closed.

Standing tasks:

- Finish the stock-KVM deterministic run path: CPUID masking, TSC/work-clock servicing, MSR handling,
  deterministic PIO/MMIO, console, tape bus, fixed memory, and `DeterminismHole` handling.
- Keep the exact-boundary interrupt engine correct: retired conditional-branch counter validation,
  early arm plus single-step convergence, interrupt-window handling, and complete point identity.
- Maintain the capability contract for Intel hosts, WSL2/nested operation, TSC stability, PMU checks,
  single-step, and deferred AMD behavior.
- Maintain the custom-module path for forced timestamp and random-instruction exits, including the
  rewritten `rdseed` trap path, site validation, normal invalid-opcode reinjection, and single-step
  interaction.
- Run the named proofs: `double_boot_memory_identical`, `cpuid_leaves_are_fixed`,
  `work_clock_is_monotone_and_reproducible`, `timer_tick_lands_at_identical_instruction`,
  `no_unmodeled_exit_is_silent`, `divergence_is_detected_and_reported`, and the H0 checks.

Exit criteria for a selected item: the relevant unit tests and H-series drive pass, the host capability
result is not overstated, and any hardware-only limitation is documented in `docs/determinism.md`.

---

## G2 — Real Linux boot and image pipeline

**Purpose:** produce reproducible bootable images and reach deterministic userspace without modifying the
software under test.

Standing tasks:

- Complete the automated `baud-packages` pipeline: Buildroot bring-up, then the pinned Nix kernel and
  initramfs path, deterministic newc archive creation, image hashing, store warming, and reproducible
  double-build verification.
- Finish the guest tape endpoint and its kernel/userspace contract, including the preferred virtio-serial
  path and the minimal PIO or character-device fallback.
- Keep `baud-boot` correct for kernel headers, E820, command line, initramfs placement, `SETUP_RNG_SEED`
  where supported, old-kernel behavior, and deterministic shutdown.
- Apply and verify the decoder-based `rdseed` rewrite to every executable section of real linked kernel
  and userspace artifacts, then lint images for tape input and forbidden real timers.
- Complete the generic guest harness contract: one input record per step, opaque observations, outcomes,
  optional frame records, and no host clock, entropy, filesystem, or network dependencies.
- Run `guest_kernel_boots_to_userspace`, `boot_params_seed_is_pinned`, `init_powers_off_deterministically`,
  `guest_tape_roundtrip`, `image_build_is_reproducible`, `image_lint_requires_tape_driver`, and the
  real-image entropy checks.

Current known focus: the pipeline and the guest tape endpoint are the main enabling work; the existing
from-source kernel/initramfs path and configuration lint are not substitutes for the complete pipeline.

---

## G3 — Snapshot, branching, and live continuation

**Purpose:** replace replay from the beginning with complete state capture and efficient continuation.

Standing tasks:

- Preserve the complete universe capture and ordered restore set: RAM, general and segment registers,
  MSRs, extended state, pending events, MP state, VM clock, TSC frequency, RCB anchor, device cursor,
  entropy state, and console state.
- Implement memory-efficient shared branching using shared backing plus userfaultfd continuation and
  write protection; retain the correct full-restore fallback for small branch counts.
- Preserve dirty-ring harvesting and write-set reset semantics, including negotiation before vCPU creation,
  writable ring mappings, dirty logging, and safe confirmation ordering.
- Add durable reconstruction from the nearest stored universe and continue to test independence across
  many branches.
- Build the actual server and command path for `shell-into <universe>`, including universe decoding,
  bidirectional terminal transport, and a wake mechanism for guests that wait on serial input.
- Run `snapshot_roundtrip_is_bit_identical`, `thousand_branches_are_independent_and_deterministic`,
  `reset_cost_scales_with_write_set`, `shell_into_universe_resumes`, and `restore_refuses_mismatched_cpu`.

Known limitation to resolve: the current branch implementation is correct but costs a full RAM copy per
branch; the intended write-set-scaled path remains open.

---

## G4 — Tape, observations, exploration, and proof

**Purpose:** make every input, observation, outcome, frame, and reduction reproducible and auditable.

Standing tasks:

- Keep the tape device as the sole guest input path, with fixed end-of-tape behavior, framed outbound
  records, branch markers, goals, violations, logs, and frame records.
- Complete the driver as a pure deterministic library: draw streams, stateful tactics, reservoir and grid
  scheduling, strategy scoring, tape reduction, nearest-universe reduction, and bounded corpus growth.
- Keep observation properties limited to goals, violations, crashes, and opaque probes; preserve the two
  independent observation planes and ordered comparison.
- Complete the durable snapshot store: encrypted bodies, plaintext-addressed pages, tapes, records, tree
  reconstruction, and later garbage collection or remote storage only when the core queue reaches them.
- Complete frame validation, hashing, QOI and Y4M generation, deterministic replay rendering, and live
  streaming without storing raw frame sequences as the primary run record.
- Run `all_input_is_tape_derived`, `driver_is_reproducible`, `shrink_reproduces_from_nearest_snapshot`,
  `planes_agree_on_healthy_run`, frame double-run/render checks, and ciphertext and reconstruction checks.

Each change must preserve opaque workload handling: core crates may not acquire target-specific names,
addresses, or behavior.

---

## G5 — Server and command surface

**Purpose:** expose every capability through the localhost daemon and the `baud` command without duplicating
business logic in the client.

Standing tasks:

- Keep server orchestration aligned with the KVM machine, image, snapshot, tape, driver, observation,
  tracing, frame, and fingerprint contracts.
- Add or finish routes for lifecycle, reconstruction, snapshots, branches, rewind, live continuation,
  fingerprint verification, observation-plane verification, frame streaming, and status queries.
- Add matching thin CLI commands with JSON output, stable exit codes, redaction, explicit identifiers,
  and no target-specific command names.
- Finish authenticated agent connections, journal-before-ack behavior, cancellation on client disconnect,
  bounded watchdogs, live run progress, and clear cancellation/error types across fingerprint and streaming
  paths.
- Resolve the measured image-memory problem: avoid duplicate multi-gigabyte disk-image copies, make run
  ownership and cancellation explicit, and prevent abandoned runs from exhausting the host.
- Keep migrations, API responses, and drive scripts synchronized; every public capability needs an
  end-to-end assertion.

Validation includes the complete workspace tests, CLI drives, lifecycle kill/reconstruct cases, and JSON
contract checks.

---

## G6 — Full distro validation

**Purpose:** prove that an unmodified real Linux distribution is deterministic across independent VMs.

Standing tasks:

- Finish the Ubuntu 18.04.1 artifact preparation and exact image metadata checks, including the clean
  filesystem state, fixed machine identity, deterministic command line, and expected serial banner.
- Complete the remaining boot blocker investigation in measured order: resolve the captured guest
  instruction addresses with the matching kernel symbols or module information; inspect the initramfs
  startup handlers; then test deterministic network naming or explicit boot networking parameters.
- Keep minimal ACPI, PCI discovery, virtio-blk, fixed-work completion, read-only base storage, and the
  in-memory write overlay deterministic and bounded.
- Complete the timed-exit fingerprint: exact event count, guest RIP, guest-physical translation with an
  independent page walk, canonical RAM hash, banner proof, structured first-divergence reporting, and
  two separate processes on separate cores.
- Run `ubuntu_boots_to_login`, `timed_exit_fingerprint_is_stable`, and `cross_vm_fingerprint_matches`
  only when the guest has reached the expected login banner; a watchdog stop is diagnostic, not success.

Current blocker: the real Ubuntu boot still stalls inside one KVM run after the tested interrupt and device
servicing fixes. Preserve all captured RIP, console-tail, watchdog, and phase-timing evidence while narrowing
that blocker.

---

## G7 — Generic interactive target

**Purpose:** demonstrate that the generic machine can drive a complex interactive program to a deep goal,
then reproduce and reduce the successful input.

Standing tasks:

- Rebuild the Mario example as a real Linux guest image containing the unmodified emulator, its harness,
  user-supplied ROM, deterministic startup state, and two tape-backed serial channels.
- Implement per-step controller input, progress probes, the derived completion predicate, sticky input
  tactics, a random negative control, and unchanged-configuration progress on the harder ROM variant.
- Connect frame capture to the tape device and stream, prove probe/frame equality across repeated boots,
  and regenerate the README artifact only from the winning tape.
- Add the complete acceptance drive: image build and lint, determinism verification, negative control,
  goal search, mid-run reconstruction, reduction, replay, live output, and non-gating harder-variant run.
- Keep every target-specific file under `examples/`; retain and run the generic-core guard.

The current packaging obstacle is the emulator's graphical/runtime dependency closure. Resolve it through the
image pipeline rather than adding target knowledge to core crates.

---

## G8 — Host, packaging, security, and release operations

**Purpose:** keep execution safe, reproducible, supportable, and releasable on the intended hardware.

Standing tasks:

- Maintain host provisioning: KVM access, Intel VMX, PMU permissions, core isolation, NUMA locality,
  sibling-safe placement, nested-host detection, secrets, and warm package stores.
- Keep the developer and deployment instructions accurate, including the required perf permission reset,
  stock-module restoration after manual enforced tests, and mutual exclusion for live module replacement.
- Finish the target relocation of the distributed validation workload from the crate tree into a guest-image
  example, without weakening generic-core checks.
- Preserve age encryption and identity boundaries for tapes, universes, observations, frames, and secrets;
  add garbage collection, per-run recipients, remote transfer, and CPU templates only after their prerequisites.
- Run `doctor_checks_kvm`, `capacity_refuses_sibling_split`, the generic-core guard, security checks, and
  the complete pre-push gate. Treat documented PMU/load flakes as flakes only with isolation evidence; never
  hide a reproducing failure.

---

## Task-generation method

At the beginning of a planning pass, the main thread must obtain an implementation audit and complete-task
proposal for **each** standing group. Subagents must drive through the actual code and compare each part of
the group's implementation with `todo-plan.md`, the relevant specifications, tests, drives, and progress
evidence. Subagents may inspect and validate the implementation, but only the main thread may change
`todo-build.md`.

For each group, start an independent implementation-audit subagent in parallel with subagents for the other
groups. Give it the exact group heading, purpose, standing tasks, current focus, and acceptance expectations
from this file. Its instruction must require:

1. Read the complete `todo-build.md` and the specifications named by that group.
2. Walk every part of the group's actual implementation: source modules, public paths, tests, drives,
   integrations, and failure handling. Compare what exists with what the plan and specifications require.
3. Search before claiming absence, using commands such as:
   ```sh
   rg -n "TODO|OPEN|Status|Test|Drive|blocked|deferred" <relevant-paths>
   git diff -- todo-build.md
   ```
4. Do not edit, write, commit, or reprioritize files. Return findings and proposals to the main thread.
5. Never propose a half-assed, partial, placeholder, or TODO-only implementation for `todo-build.md`.
   Propose only a complete, coherent implementation outcome sized for one build iteration. If a complete
   outcome cannot be executed because of a dependency or owner decision, report the blocker and evidence
   instead of inventing a partial task.
6. Return one compact record per proposed task:
   ```text
   Group: <standing group heading>
   Task: <one complete implementation outcome sized for one build iteration>
   Comparison: <what the plan/specs require versus what the code actually provides>
   Evidence: <paths, symbols, tests, commands, or progress entries>
   Already built: <what is complete and how it was verified>
   Dependencies: <prerequisites or none>
   Acceptance: <named test and drive, plus failure behavior>
   Status: built | partly built | blocked | deferred | not measured
   ```

After every subagent has returned, the main thread must compare proposals with existing headings and items
in `todo-build.md`, remove duplicates, reject unsupported claims, and add accepted complete tasks to
`todo-build.md` in dependency and priority order. **Every accepted task must be written as a Markdown bullet
point**, with affected paths, the complete next step, and an acceptance criterion. Never add prose paragraphs,
partial placeholders, or incomplete implementation fragments as task entries. Keep standing-group
scope in `todo-plan.md`; do not turn an owner decision into build work. Record the number of proposals
received, accepted, merged, and rejected in `ralph/progress.txt`.

## Standing rules for every pass

1. Read this file and the complete `todo-build.md` before selecting work. Also read the specification files
   named by the selected group and the directly affected implementation, tests, and drive scripts.
2. Select exactly one unfinished item sized for one coherent iteration. Record the selected group and item
   before changing files. Do not select the same still-pending item again under a new name.
3. Search the repository before declaring a capability absent. Distinguish **built**, **partly built**,
   **blocked**, **deferred**, and **not measured**. Keep those states explicit in the plan and progress log.
4. Work in dependency order: host and machine primitives before boot, boot before targets, and core
   persistence before optimization. Never solve a target-specific problem by adding target knowledge to a
   generic crate.
5. Every implementation item must name its acceptance test, the drive that exercises it, and the relevant
   failure behavior. A passing unit test alone is not proof of a hardware or cross-process guarantee.
6. For determinism claims, use the same image and tape, compare complete observable outputs, and include
   the relevant machine state or fingerprint. Do not use wall-clock timing, host randomness, or an
   unbounded wait as evidence of success.
7. For a stalled run, add bounded watchdogs and diagnostics before changing behavior. Preserve event count,
   guest instruction address, console tail, phase, and error classification; do not turn a timeout into a
   successful result.
8. Validate in the narrowest useful order, then run the complete pre-push protocol before committing:
   `bash drive/gate.sh`, with the required host preparation. Isolate a reported flaky unit before calling
   it a regression, and record both outcomes.
9. Keep changes and evidence together. Update the applicable spec, implementation, test, drive, and
   progress record in the same iteration when the contract changes. Do not leave generated transient
   artifacts as the source of truth.
10. Before closing an item, re-read this plan and collapse only that item to a concise `DONE` line with
    the commit or evidence pointer. If an item is blocked by an owner decision, mark it `BLOCKED` with
    the exact decision needed and the file where the answer belongs.
11. End every iteration with a clean repository state, an explicit validation result, and a concise progress
    entry. The next iteration resumes from the first unfinished item in priority order.

## Continuous focus rule

At the start of each pass, choose the first unfinished item in the highest-priority group whose prerequisites
are satisfied. If a higher group is blocked, choose the first actionable item in the next group and leave the
blocker visible. Never replace this standing queue with a temporary list: the queue itself is the durable
focus for all future passes.
