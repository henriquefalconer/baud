<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# BAUD — Implementation Plan (deterministic-hypervisor)

baud runs a whole guest computer inside a virtual machine and makes that machine's entire execution a
reproducible function of one input byte stream (the **tape**). It owns the machine at the
hardware-virtualization layer (Linux KVM + Intel VT-x), replaces every source of nondeterminism the machine
can see with a value computed from or seeded by the tape, then fuzzes the tape — snapshotting any moment,
forking thousands of alternate continuations that share memory, rewinding, and re-running: a branching tree
of universes explored in parallel.

This plan is self-contained. Every milestone ends with a drive script that tests it through the CLI. Section
12 is the problem → specification → test matrix: every risk found in review is turned into a concrete
guarantee and the test that proves it.

---

## 0. Context and goal

- baud tests a whole machine, not a process. Determinism is enforced at the virtualization layer, so the
  guest may run threads, dynamic binaries, multiple processes, any language — none of the old single-process
  limits apply.
- Every guest-visible nondeterminism source — time, randomness, device input, interrupt timing — is served
  by the VMM from, or seeded from, the tape. Same guest image + same tape ⇒ byte-identical execution.
- Because the machine is reproducible, baud snapshots any moment, forks many continuations that share memory
  copy-on-write, rewinds, and measures what fraction of variations still hit a bug — a tree of universes.
- The one instruction a userspace tracer can never trap — the hardware random instruction — is controlled
  here, because under VT-x `cpuid` always exits and the random instruction can be masked or hardware-trapped.

## 1. Hard constraints

- **Host exposes `/dev/kvm` with Intel VT-x.** Managed containers do not expose it; baud runs on bare-metal
  or nested-virtualization hosts you provision (§9). Verified at H0, re-checked on every host by
  `baud host probe`.
- **x86_64 Intel primary.** The determinism instructions (`rdtsc`, `cpuid`, the branch counter) are x86;
  Intel has full CPUID interception and TSC control. AMD is phase-2 (§3.9); arm64 is out.
- **One virtual CPU per VM.** A single instruction stream removes cross-core memory races. Throughput comes
  from many single-vCPU VMs across physical cores, never from multi-core guests. VM creation with >1 vCPU is
  a hard error.
- **The tape is the only nondeterministic input.** Any VM exit not handled to a deterministic value is a
  determinism hole; the run loop fails loud rather than continuing.
- All control through the `baud` CLI, `--json` on every command. Exit codes: `0` completed · `1` error ·
  `2` goal/violation.
- Rust workspace; every crate under `crates/`, prefixed `baud-`.

## 2. Vocabulary

- **guest**: the whole virtual machine — a bootable OS image plus the software under test plus a small
  in-guest agent that talks to the tape device.
- **VMM / supervisor**: `baud-multiverse` — the deterministic VMM that owns the guest through KVM.
- **tape**: the input byte stream; the sole source of nondeterminism. Feeds the guest through the tape
  device and the VMM's own scheduling choices.
- **work-clock**: the guest's notion of time, computed from a hardware counter of work done (retired
  conditional branches), not wall-clock.
- **universe**: a complete captured VM state — guest RAM + all vCPU state + device state + work-clock. A
  snapshot.
- **branch**: a fork of a universe; a continuation sharing the parent's memory copy-on-write, diverging only
  on write.
- **branch point**: a moment (usually an input-ingestion point) where a snapshot is taken so exploration
  forks from a shared prefix.
- **regime**: which determinism level a run targets — **cooperative** (stock KVM) or **enforced** (custom
  KVM module). See §3.8.
- **run**: `{ seed, guest-image hash, regime, strategy, tactics, snapshot-tree }`. Fully reproduces
  everything.

## 3. `baud-multiverse` — the deterministic VMM (core deliverable)

A single-vCPU KVM VMM whose every exit resolves to a deterministic value. Full component detail in
`specs/baud-multiverse.md`; the single-vCPU state machine in `specs/baud-vcpu.md`.

### 3.1 Skeleton

- **Crates, pinned exactly**: `kvm-ioctls` 0.25, `kvm-bindings` 0.14, `vm-memory` 0.18, `linux-loader` 0.14,
  `vmm-sys-util` 0.15, `vm-superio` 0.8. One VMM thread + **one vCPU thread** running its own `KVM_RUN`
  loop; exits dispatched to a device bus.
- **Boot flow**: `Kvm::new` → `create_vm` → register one zeroed guest-RAM region at a fixed guest-physical
  address (`KVM_SET_USER_MEMORY_REGION`) → `create_vcpu` → set CPUID/sregs/regs → load the guest kernel with
  `linux-loader` and write boot params at fixed addresses → enter the run loop.
- **Spec**: the run loop matches every `VcpuExit`; `IoIn/IoOut/MmioRead/MmioWrite/X86Rdmsr/X86Wrmsr/Hlt/
  Shutdown` are handled deterministically; the catch-all returns `Err(DeterminismHole)`. Open-bus reads
  return a fixed byte, never host memory.
- **Test** (`double_boot_memory_identical`): boot the hello image twice from the same tape; assert equal
  blake3 of guest RAM at first `Hlt`. Boot-nondeterminism is a bug.

### 3.2 CPUID and randomness

- Under VT-x, `cpuid` always exits — the VMM owns every leaf via `KVM_SET_CPUID2` (start from
  `KVM_GET_SUPPORTED_CPUID`, then edit).
- **Spec — mask table** (a compliant guest then never issues a nondeterministic instruction): clear RDRAND
  `01H:ECX[30]`, RDSEED `07H:EBX[18]`, TSX HLE/RTM `07H:EBX[4]/[11]`, x2APIC `01H:ECX[21]`; pin topology
  leaves `0BH/1FH` to one core; set invariant-TSC `80000007H:EDX[8]`; set hypervisor-present to a fixed
  value.
- **Test** (`cpuid_leaves_are_fixed`): read every served leaf twice across two runs; assert identical, and
  assert RDRAND/RDSEED/TSX/x2APIC bits are 0.
- **Regime split for the random instruction** (§3.8): cooperative = masked CPUID keeps a well-formed guest
  away from it; a guest that issues it anyway is caught by double-run divergence (`Crash{detail:"rdrand"}`
  in the enforced regime, divergence report in the cooperative regime). Enforced = hardware
  random-instruction exiting traps every attempt and serves the tape.
- **Test** (`rdrand_guest_is_flagged`): a guest that executes the raw random instruction produces a
  divergent double-run in cooperative regime (run marked divergent, never `verified:true`) and a
  `Crash{detail:"rdrand"}` in enforced regime.

### 3.3 Time — a work-clock

- **Spec**: the guest's time is a function of work done. Count **retired conditional branches** with
  `perf_event_open` on the vCPU thread (guest-filtered); virtual timestamp = `base + k × branch_count`; feed
  that into every time source. Raw retired-instruction count is forbidden (it double-counts faults and
  interrupts).
- **RDTSC/RDTSCP**: cooperative = `KVM_SET_TSC_KHZ` pins a fixed frequency + `KVM_VCPU_TSC_OFFSET` sets the
  offset (native speed, affine-from-host, low bits jitter); enforced = force RDTSC-exiting and return the
  work-clock value (bit-exact, needs the custom module).
- **MSR trapping**: `KVM_X86_SET_MSR_FILTER` routes `IA32_TSC (0x10)`, `IA32_TSC_AUX`, `IA32_TSC_DEADLINE`
  to the VMM; serve virtual values. Delete HPET/PIT/PM-timer/RTC entirely.
- **Test** (`work_clock_is_monotone_and_reproducible`): a guest that reads the timestamp N times yields a
  non-decreasing sequence, and the full sequence is identical across a double-run (cooperative asserts the
  high bits / work-derived field; enforced asserts full equality).
- **Test** (`host_tsc_is_stable`, H0): `KVM_GET_TSC_KHZ` ≠ `-EIO`; if it errors, the host is rejected with a
  remediation message (disable C-states/turbo, require constant-/invariant-TSC).

### 3.4 Interrupts at an exact instruction boundary

- **Spec (arm-early-then-single-step)**: to land a timer tick (or any interrupt) at a reproducible
  instruction — (1) arm the branch counter to overflow a margin before the target work-count; (2) take the
  early exit; (3) `KVM_SET_GUEST_DEBUG` with `KVM_GUESTDBG_SINGLESTEP | KVM_GUESTDBG_BLOCKIRQ` and step
  until the point matches the tuple **(program counter + all GP registers + branch count**, + RCX for `rep`
  loops, + a stack checksum on collision); (4) confirm injectability (`ready_for_interrupt_injection`; else
  set `request_interrupt_window` and re-enter until the window opens); (5) inject via `KVM_INTERRUPT` or
  `KVM_SET_VCPU_EVENTS`.
- **Test** (`timer_tick_lands_at_identical_instruction`): drive a timer-interrupt guest with many ticks from
  the same tape twice; assert the injection tuple (PC + branch count) is identical at every tick across the
  two runs.

### 3.5 Deterministic I/O — the tape device

- **Spec**: the guest does all input/output through one paravirtual device served over PIO/MMIO exits. No
  real disks, network, DMA, or host interrupts. Reads return the next tape bytes (entropy seed, external
  input, simulated device responses); writes hand data out (log lines, probe values, `reached goal` /
  `invariant violated` markers) and issue control requests (`checkpoint here` = branch point). Full detail
  in `specs/baud-tape-device.md`.
- **Test** (`all_input_is_tape_derived`): with the tape device as the sole input channel, two runs on the
  same tape produce identical guest output; changing one tape byte changes the output (input actually flows
  from the tape).

### 3.6 The subtractive rule

- Build by removing: single vCPU; no real device emulation; no host interrupts; zeroed memory at fixed
  addresses; time, randomness, and input synthesized. Down to a console plus the tape device.
- **Test** (`no_unmodeled_exit_is_silent`): a fuzz smoke run over random tapes asserts the run loop never
  hits the catch-all without returning `Err` — any unmodeled exit fails the test, never continues.

### 3.7 Counter reliability (validated, not assumed)

- **Spec**: the branch counter is deterministic on some microarchitectures and not others; it must be
  validated on the exact deploy silicon at H0 before it is trusted, and the branch count alone never names an
  execution point (always paired with PC + registers + stack checksum).
- **Test** (`rcb_is_deterministic_on_this_cpu`, H0 gate): run a fixed guest loop twice; assert the branch
  count at a fixed PC is identical. On failure the host/CPU is rejected for the enforced regime and flagged
  for cooperative-only, recorded in `docs/determinism.md`.
- **Spec (divergence, not perfection)**: a rare hardware miscount must be detected, never assumed away —
  every run supports a double-run stream-hash comparison; a mismatch marks the run `divergent` and excludes
  it from replay/branch/shrink.
- **Test** (`divergence_is_detected_and_reported`): inject a synthetic one-step observation difference;
  assert the comparator reports the first divergent step (node/PC/probe) and the run is marked divergent.

### 3.8 Regimes (which conditions each run runs under)

- **Cooperative (stock KVM) — the first target.** Full CPUID control, fixed-frequency virtual TSC with
  controllable offset, MSR trapping, single vCPU, zeroed memory, the tape device. Reproducible for guests
  that do not fight it (masked CPUID keeps a compliant guest away from the raw random and timestamp
  instructions). No kernel changes.
- **Enforced (custom KVM module).** Turns on the hardware VM-execution controls that stock KVM does not
  expose to userspace — force every RDTSC and every random instruction to exit and be served from the
  work-clock/tape — so even an adversarial guest is deterministic. A small out-of-tree KVM patch/module.
- **Spec**: every run records its regime in the manifest; the CLI and `verify` refuse to report enforced
  guarantees while running on stock KVM. `baud host probe` reports which regime a host supports.
- **Test** (`regime_is_recorded_and_not_overclaimed`): a run on stock KVM is tagged `cooperative`; asking
  for enforced guarantees on such a host returns exit `1` with a clear message, not a false pass.

### 3.9 AMD (phase-2)

- **Spec**: AMD configures CPUID/TSC intercepts through the VMCB and scales TSC via a ratio MSR; whether it
  exposes an RDTSC-exiting equivalent must be confirmed against the AMD virtualization manual before use.
  Intel-first; AMD is a second phase.
- **Test** (`amd_host_refused_in_enforced_regime`): on an AMD host, the enforced regime returns exit `1`
  with "AMD enforced-regime unverified"; cooperative may proceed if H0 checks pass.

## 4. Guests and workloads (the contract is on the image)

- A workload is a **bootable guest image**: a small Linux (or unikernel) + the software under test + a tiny
  in-guest agent that speaks to the tape device.
- Threads, dynamic linking, multiple processes, arbitrary binaries are all supported — determinism is at the
  machine layer.
- **Spec (image contract)**: the guest kernel takes entropy, clock, and external input from the tape device
  (a boot-time shim / small driver) and carries no real hardware timers baud did not model. Enforced by
  `baud image lint`.
- **Test** (`image_lint_requires_tape_driver`): an image without the tape-device driver, or with a real
  RTC/HPET enabled, fails `baud image lint` with a specific reason.
- **`baud-packages` builds guest images** reproducibly with pinned Nix (kernel + rootfs + agent) and warms
  them into the snapshot store; the image hash is the environmental identity. Full detail in
  `specs/baud-packages.md`.

## 5. Snapshot-branch multiverse (replaces replay-from-zero)

Full detail in `specs/baud-snapshot.md` (capture/restore/branch) and `specs/baud-snapshot-store.md`
(durable tree).

- **Capture a universe** = guest RAM + full vCPU state (`KVM_GET_REGS/SREGS/MSRS/LAPIC/XSAVE2/XCRS/
  VCPU_EVENTS/MP_STATE`) + VM clock/TSC (`KVM_GET_CLOCK`, `KVM_GET_TSC_KHZ`) + tape-device cursor + console
  state. Omitting a field diverges the restored universe.
- **Test** (`snapshot_roundtrip_is_bit_identical`): capture at step K, restore, continue to step K+M; assert
  the observation stream K..K+M equals a straight run K..K+M. Any divergence means a state field is missing
  from capture.
- **Cheap N-way branching** via **userfaultfd**: parent memory is a shared read-only backing; each child
  serves pages with `UFFDIO_CONTINUE` (share) and `UFFDIO_WRITEPROTECT` (copy-on-first-write). Per-branch
  memory is proportional to its write set, not to total RAM. `fork()` copy-on-write is the small-N fallback.
- **Test** (`thousand_branches_are_independent_and_deterministic`): fork 1,000 continuations from one branch
  point on 1,000 tape suffixes; assert each is internally deterministic (double-run identical) and that
  branches do not perturb each other (a write in one does not change another's output).
- **Cheap reset** via the **KVM dirty ring** (`KVM_CAP_DIRTY_LOG_RING`): rewinding copies back only dirtied
  pages.
- **Test** (`reset_cost_scales_with_write_set`): assert the number of pages restored on rewind equals the
  dirty-ring count, not total RAM pages.
- **The tree**: snapshot at each branch point; exploration forks from the nearest one instead of replaying
  the prefix. `baud-journal` is superseded by `baud-snapshot-store` (content-addressed universes + tape +
  tree), age-encrypted at rest via `baud-keys`.
- **Restore into a live shell**: re-wire the console to a PTY (`vm-superio` serial on an EventFd trigger)
  and resume — a prompt inside any moment of any run.
- **Test** (`shell_into_universe_resumes`): `baud shell-into <universe>` yields a responsive prompt whose
  first output byte matches the captured console tail.
- **Restore determinism spec**: capture the work-clock anchor; restore TSC frequency **before** creating the
  vCPU and `IA32_TSC` **before** `IA32_TSC_DEADLINE`; restore only on the same host kernel + CPU model or via
  a fixed CPUID template.
- **Test** (`restore_refuses_mismatched_cpu`): restoring a universe captured on CPU model A onto model B is
  refused (exit `1`) unless a CPUID template is active; a restored timer guest resumes without a stuck
  deadline (the TSC-before-deadline ordering is exercised).

## 6. The tape / fuzzing engine (`baud-driver`)

- The driver owns all randomness and produces the tape; the tape feeds the guest (via the tape device) and
  the VMM's scheduling choices (which branch point to expand, which fault/weather to inject).
- **Strategy** scores universes by probe progress (§7); **tactics** shape input/weather/branch-expansion.
  Grid buckets, reservoir, and `stateful` weather carry over. Full detail in `specs/baud-driver.md`.
- **Exploration is tree search over snapshots**: expand a branch point, fork N continuations, score, keep
  interesting ones as new branch points.
- **Test** (`driver_is_reproducible`): same seed + same observation replies ⇒ byte-identical tape (property
  test).
- **Test** (`shrink_reproduces_from_nearest_snapshot`): shrinking a finding forks from the nearest snapshot
  (not from boot) and the minimized tape still reaches the finding.

## 7. Observation and properties

- A whole-VM guest is observed via: (a) the **tape-device write channel** (the in-guest agent emits
  `key=value` probes, logs, and `goal`/`invariant` markers); (b) **guest-memory reads** of known
  addresses/symbols by the VMM; (c) **VM-level facts** (halted, triple-fault, out-of-memory, exit code).
- Properties stay crash / invariant / goal — no temporal operators. The harness lives partly in the guest
  agent, partly in the VMM. `baud-proto` gains the hypercall/tape-device probe + outcome messages.
- **Two-plane cross-check** survives: the VMM exit log is plane 1; an in-guest audit or a second independent
  counter is plane 2; disagreement fails the run.
- **Test** (`planes_agree_on_healthy_run`): plane 1 and plane 2 syscall/exit sequences agree; a deliberately
  broken VMM build fails the cross-check.

## 8. Crate map

- **`baud-multiverse`** — the deterministic VMM (KVM setup, run loop, CPUID/TSC/MSR control, the
  interrupt-injection engine, the tape device, snapshot hooks). `specs/baud-multiverse.md`.
- **`baud-vcpu`** — the single-vCPU state machine and exit dispatch. `specs/baud-vcpu.md`.
- **`baud-snapshot`** — universe capture/restore, userfaultfd CoW branching, dirty-ring reset, the branch
  tree. `specs/baud-snapshot.md`.
- **`baud-snapshot-store`** — content-addressed durable universes + tapes + tree; age-encrypted at rest.
  `specs/baud-snapshot-store.md`. (Supersedes `baud-journal`.)
- **`baud-tape-device`** — the paravirtual device model + guest-side driver contract. `specs/baud-tape-device.md`.
- **`baud-host`** — the KVM-capable host manager: fleet of single-vCPU VMs, core pinning, capacity
  accounting, `host probe`. `specs/baud-host.md`. (Primary substrate; replaces the Daytona-container backend
  for the VMM.)
- **`baud-packages`** — builds reproducible guest images; warms the store. `specs/baud-packages.md`.
- **`baud-driver`** — tape/fuzzing engine + snapshot-tree exploration. `specs/baud-driver.md`.
- **`baud-proto`** — wire types incl. hypercall/tape-device probe + outcome messages. `specs/baud-proto.md`.
- **`baud-server`, `baud-cli`** — orchestration + command surface; adds `snapshot`/`branch`/`rewind`/
  `shell-into`/`host`/`image` verbs.
- **`baud-tracing`, `baud-stream`, `baud-secret`, `baud-identity`, `baud-keys`** — carry over; `baud-stream`
  now captures the guest framebuffer (a whole OS runs, so a real display exists).
- **Targets** (`baud-raftlet`, mario, parser) become **guest images** under `examples/`, not in-tree
  simulations.

## 9. Infrastructure (`infra/`) — the host substrate

- **Managed containers are out for the VMM** — no `/dev/kvm`. baud-multiverse runs only on hosts you control
  with real VT-x.
- **Substrate**: bare-metal instances (best determinism/latency) or nested-virtualization-enabled cloud VMs
  (Intel, measurable overhead, no device passthrough). Verify `grep vmx /proc/cpuinfo` and
  `kvm_intel nested=1`.
- **Fleet — one physical core per VM**: pin each vCPU thread (emulator/IO threads off the isolated cores),
  isolate cores (`isolcpus`/cpuset + `nohz_full` + `rcu_nocbs`), NUMA-local memory, **SMT disabled** (or
  both siblings in one VM — siblings share cache and leak). Budget ~28–30 VMs per 32-core host, ~56–60 per
  64-core, minus housekeeping.
- **Test** (`capacity_refuses_sibling_split`): the host manager never places two VMs on hyperthread siblings;
  a placement attempt that would is rejected.
- **`infra/nixos-modules/baud-host.nix`** provisions such a host (libvirtd/direct KVM, `kvm-intel`,
  isolation kernel params, pinning, `nested=1` when itself nested). `infra/machines/` composes bare-metal and
  nested-VM host definitions.
- **Developer machine**: Windows-11 x86 with WSL2 (ships KVM; `nestedVirtualization=true` in `.wslconfig`)
  or a Hyper-V/KVM stock-kernel Linux VM; Intel preferred for CPUID/TSC parity. macOS-arm64 cannot host
  (no x86 VT-x) and is control-plane only.
- **Test** (`doctor_checks_kvm`): `baud doctor` on the dev machine asserts `/dev/kvm` present, VT-x exposed,
  branch-counter deterministic, and reports the regime the machine supports.
- **Daytona re-scoped**: managed tier cannot host the VMM; a self-hosted runner on your own KVM host might
  pass `/dev/kvm` through (undocumented — test before betting), otherwise it is dropped for the VMM and kept
  only as a possible control plane over baud-owned hosts.
- `infra/secrets` (multi-recipient sops) and `infra/pkgs` (cross-builds + the guest-image builder) carry
  over.

## 10. Milestones (each ends with a drive script and named tests)

- **H0 — capability spike (must actually run).** On a real KVM host and the dev VM, probe `/dev/kvm`, VT-x,
  `KVM_SET_CPUID2`, TSC-khz/offset control, `KVM_X86_SET_MSR_FILTER`, `KVM_SET_GUEST_DEBUG` single-step,
  branch-counter determinism (`rcb_is_deterministic_on_this_cpu`), host TSC stability
  (`host_tsc_is_stable`), and nested-virt availability. Record results + the regime choice in
  `docs/determinism.md`. Drive `drive/h0.sh`: `baud host probe --json` asserts each capability; a failing
  capability downgrades the regime and is recorded, never hidden.
- **H1 — boot a guest.** The run loop boots a minimal guest kernel that prints to the serial console; clean
  `Hlt`/`Shutdown`. Drive `drive/h1.sh`: boot the hello image, assert expected console output;
  `double_boot_memory_identical` passes.
- **H2 — deterministic double-run.** Same image + tape twice ⇒ byte-identical observation stream
  (console + probes + final memory hash), CPUID masked, virtual TSC pinned. Drive `drive/h2.sh`:
  `cpuid_leaves_are_fixed`, `work_clock_is_monotone_and_reproducible`, `all_input_is_tape_derived`,
  `no_unmodeled_exit_is_silent`.
- **H3 — randomness + time control.** Entropy and timestamps flow only through masked CPUID + tape/work-clock;
  a raw-random guest is caught (cooperative) or trapped (enforced). Drive `drive/h3.sh`:
  `rdrand_guest_is_flagged`, `regime_is_recorded_and_not_overclaimed`.
- **H4 — interrupt at an exact boundary.** Deliver a timer tick at a chosen work-count via
  arm-early-then-single-step; identical instruction across a double-run. Drive `drive/h4.sh`:
  `timer_tick_lands_at_identical_instruction`.
- **H5 — snapshot / branch / restore.** Capture, fork thousands sharing memory, rewind, restore into a live
  shell. Drive `drive/h5.sh`: `snapshot_roundtrip_is_bit_identical`,
  `thousand_branches_are_independent_and_deterministic`, `reset_cost_scales_with_write_set`,
  `shell_into_universe_resumes`, `restore_refuses_mismatched_cpu`.
- **H6 — multi-VM fleet.** Many single-vCPU VMs pinned across cores explore in parallel on one host. Drive
  `drive/h6.sh`: aggregate throughput, `capacity_refuses_sibling_split`, no cross-VM interference.
- **M-series** rebuild server/CLI/driver/store/stream on this core: tape-tree exploration
  (`driver_is_reproducible`, `shrink_reproduces_from_nearest_snapshot`), strategy/tactics over guest probes,
  snapshot-store reconstruction/shrinking, the framebuffer stream, and a distributed target as a **guest
  image** reaching a planted safety violation via guided branch search (`planes_agree_on_healthy_run`, the
  planted-bug interleaving test).

## 11. Determinism regimes and required conditions

- The two regimes (§3.8) are the operating envelope, not caveats to gloss: **cooperative** is the first
  buildable target and is reproducible for guests that take entropy/clock/input from the tape device;
  **enforced** adds a custom KVM module to hardware-trap the raw random and timestamp instructions so even an
  adversarial guest is reproducible. Each run records its regime; guarantees are reported only for the regime
  actually in force (`regime_is_recorded_and_not_overclaimed`).
- Required conditions, each with a test in §12: a KVM-capable Intel host with a stable TSC; a
  microarchitecture whose branch counter is deterministic (validated at H0); SMT disabled or siblings kept in
  one VM; restore only on the matching CPU model or under a CPUID template; the guest image built to the tape
  contract; divergence detection on by default.
- With these met, baud delivers reproducible, snapshot-branchable execution of a whole guest machine and any
  software inside it on Intel KVM hosts — the capability the userspace approach could not reach (threads,
  dynamic binaries, and the hardware random instruction).

## 12. Problem → specification → test matrix

Every risk found in review, the guarantee it becomes, and the test that proves it.

| # | Problem | Specification (what must be built/guaranteed) | Test |
|---|---------|-----------------------------------------------|------|
| 1 | Stock KVM won't force RDTSC/random-instruction exiting from userspace | Two regimes; cooperative masks CPUID, enforced adds a KVM module; run records regime | `regime_is_recorded_and_not_overclaimed`; `rdrand_guest_is_flagged` |
| 2 | Branch counter is nondeterministic on some CPUs | Validate on deploy silicon at H0; reject/downgrade on failure | `rcb_is_deterministic_on_this_cpu` (H0 gate) |
| 3 | Raw instruction count double-counts faults/interrupts | Forbid raw count; use RCB + PC + registers + stack checksum to name a point | `timer_tick_lands_at_identical_instruction` (tuple identical) |
| 4 | PMU interrupts are delivered late/imprecisely (skid) | Arm-early-then-single-step to the exact boundary | `timer_tick_lands_at_identical_instruction` |
| 5 | Rare (~1e-12) counter miscount | Detect divergence, never assume perfection; double-run comparator | `divergence_is_detected_and_reported` |
| 6 | `/dev/kvm` absent in managed containers | KVM-capable host substrate; `host probe` gate | `doctor_checks_kvm`; H0 `baud host probe` |
| 7 | SMT siblings leak/jitter and add no deterministic capacity | SMT off or siblings same-VM; placement refuses splits | `capacity_refuses_sibling_split` |
| 8 | Restore is host-locked (CPU model/kernel) | Same model or CPUID template; refuse mismatch | `restore_refuses_mismatched_cpu` |
| 9 | A snapshot missing any state field diverges | Enumerated capture set (RAM + all vCPU + clock + device) | `snapshot_roundtrip_is_bit_identical` |
| 10 | TSC restore ordering (khz before vCPU; TSC before deadline) | Ordered restore sequence | `restore_refuses_mismatched_cpu` (timer resumes clean) |
| 11 | Uninitialized memory is a determinism leak | Zeroed memory at fixed addresses | `double_boot_memory_identical` |
| 12 | An unhandled VM exit silently continues | Catch-all returns `Err(DeterminismHole)` | `no_unmodeled_exit_is_silent` |
| 13 | Host TSC instability (`KVM_GET_TSC_KHZ` = -EIO) | Pin constant-/invariant-TSC host; reject unstable | `host_tsc_is_stable` (H0) |
| 14 | Guest must cooperate for entropy/clock/input | Image contract enforced by `baud image lint` | `image_lint_requires_tape_driver` |
| 15 | Nested-virt availability/overhead varies | H0 records nested support + overhead; accept or reject | H0 `baud host probe` records nested=1 |
| 16 | Snapshot-branch residual nondeterminism (RDRAND/TSC/wall-clock) | VMM intercepts them so branches are bit-identical | `thousand_branches_are_independent_and_deterministic` |
| 17 | Windows dev needs WSL2-KVM/Hyper-V | `doctor` checks `/dev/kvm` on the dev VM | `doctor_checks_kvm` |
| 18 | Multi-core guest determinism is unsolved cheaply | Single vCPU only; refuse >1 | `vm_creation_refuses_multiple_vcpus` |
| 19 | AMD intercept/TSC differences unverified | Intel-first; refuse AMD in enforced regime | `amd_host_refused_in_enforced_regime` |
| 20 | CPUID leaks core index / topology nondeterminism | Fixed CPUID leaves + topology pinned + affinity | `cpuid_leaves_are_fixed` |
| 21 | Input not actually flowing from the tape (fake determinism) | Tape device is the sole input; byte-sensitivity | `all_input_is_tape_derived` |
| 22 | Shrinking re-runs from zero (slow) | Fork from nearest snapshot | `shrink_reproduces_from_nearest_snapshot` |
| 23 | Journal/observations in plaintext at rest | `baud-snapshot-store` age-encrypts universes + tapes | `snapshot_store_bodies_are_ciphertext` |
| 24 | Two-plane cross-check is counts-only, misses ordering | Compare ordered exit/syscall sequences | `planes_agree_on_healthy_run` |

## 13. Migration map (from the current userspace plan)

- **Execution layer**: userspace ptrace/seccomp of one process → **KVM/VT-x VMM of a whole machine**.
  `baud-multiverse` is rewritten.
- **Guest contract**: single-threaded static musl RDRAND-free process → **any OS + any software**; the
  contract moves to the **image build** (entropy/clock/input via the tape device).
- **Randomness**: untrappable in userspace → **controlled** (CPUID always exits; optional hardware trap in
  the enforced regime).
- **Time**: `PR_SET_TSC` per-process → **VM-level virtual TSC / work-clock** plus exact-boundary interrupt
  injection.
- **State model**: replay-from-zero journal (O(prefix) per reconstruct) → **snapshot-branch tree**
  (O(write-set) per branch). `baud-journal` → `baud-snapshot` / `baud-snapshot-store`.
- **Infra**: Daytona containers → **KVM-capable bare-metal / nested-virt hosts**, one core per VM.
- **Dev machine**: macOS-arm64 (cannot host) → **Windows-x86 WSL2/Hyper-V or a Linux VM**, Intel preferred.
- **Targets**: in-tree simulations → **guest images** under `examples/`.
- The round-1/2/3 verification triage and the §15 userspace-port directive are superseded by this plan; only
  the driver, tape, and observation concepts carry forward, reshaped into snapshot-branch.

## 14. Build status (updated as milestones land — not a duplicate of ralph/progress.txt)

- **H0 (capability spike) — VALIDATED FOR REAL on real KVM hardware.** The dev machine is now a
  bare-metal Dell XPS 13 running Ubuntu on WSL2 with `/dev/kvm` genuinely present (`CLAUDE.md`) —
  the first real KVM host this project has ever had. `drive/h0.sh` passes end-to-end for real:
  `baud host probe --json` reports `regime="cooperative"`, `kvm/vmx/cpuid/msr_filter/singlestep/
  rcb_deterministic/tsc_stable/nested = true`, `vendor="intel"`, `capacity=3`. `crates/baud-host`:
  `Host::probe()`/`Probe`/`Regime`/`Vendor`/fleet `Placement` (specs/baud-host.md §3-§5), wired to
  `GET /host/probe` + `baud host probe --json`. Regime-decision logic is hardware-independent and
  unit-tested via an injectable `CapabilityChecks` seam (`cargo test -p baud-host`); the real Linux
  backend (`src/linux.rs`) is no longer just type-checked, it is confirmed correct against real
  hardware. `docs/determinism.md` still describes the superseded ptrace/seccomp mechanism below its
  pivot notice; a full rewrite for KVM/VT-x is still open.
- **`baud-vcpu` (specs/baud-vcpu.md) — core built; the real Linux run loop is now exercised for
  real.** Exit-dispatch match (`Exit`/`Bus`/`TimeSource`/`dispatch_exit`/`DeterminismHole`,
  `src/lib.rs`, no wildcard arm — `Exit::Unmodeled` is the only path to `Err`) and the
  arm-early-then-single-step interrupt-injection engine (`ExecPoint`/`PmuStepper`/`inject_at`,
  `src/boundary.rs`) are hardware-independent, 15/15 tests pass with no KVM at all. Real Linux half
  (`src/linux/mod.rs`'s `convert_exit`/`run_until_halted`/`run_one_exit`) is now called for real by
  `baud-multiverse::linux::Multiverse::run_to_first_halt` and confirmed correct against real
  `/dev/kvm` (`double_boot_memory_identical`, see below) — `pmu.rs`'s `LinuxPmuStepper` (interrupt
  injection specifically, H4) remains unexercised. **Known gap**: `LinuxPmuStepper` uses
  process-wide `F_SETOWN`, not `F_SETOWN_EX(F_OWNER_TID, ...)` — revisit once the one-VMM-thread/
  one-vCPU-thread model lands (documented in `pmu.rs`'s module doc).
- **`baud-multiverse` boot flow (specs/baud-multiverse.md §2) — BOOTS A REAL GUEST on real KVM
  hardware (H1, todo.md §10 — this project's first real KVM boot).** `cpuid.rs` (determinism mask
  table, now 9 rows incl. two added this iteration — see below), `layout.rs` (fixed
  guest-physical addresses + `build_identity_page_tables`, 6 tests), `linux/{mod,pagetables,
  bootparams}.rs` (`Kvm::new`→`create_vm`→zeroed RAM→`create_vcpu`→CPUID mask→identity page
  tables→64-bit long mode via `KVM_SET_SREGS`→MSR filter for `IA32_TSC`/`_DEADLINE`/`_AUX`→
  `KVM_SET_TSC_KHZ`→`linux_loader` bzImage load→`boot_params`→RIP at direct-boot entry),
  `timesource.rs` (`WorkClock<C: BranchCounter>`, hardware-independent), `console.rs` (`Console`:
  COM1 16550 UART, plus a new `Cmos` shim — see below), `linux::Multiverse` (`boot`/
  `run_to_first_halt` → `HaltOutcome{console_output, ram_hash}`). 50/50 native tests pass
  (`cargo test -p baud-multiverse`, up from 28), and `linux::tests::double_boot_memory_identical`
  (specs/baud-multiverse.md §3.1's named test) now runs for real: boots `tests/fixtures/hello-guest/
  bzImage` (a hand-assembled 17-byte payload wrapped in a minimal valid bzImage — see that
  directory's `BUILD.md` for exactly why, and why *not* a real Linux kernel yet) twice against
  actual `/dev/kvm`, asserts the console marker and RAM `blake3` hash are byte-identical across
  both boots. `drive/h1.sh` was rewritten to drive this for real (superseding the pre-pivot
  ptrace-era version).
  - **Three real, previously-unexercised production bugs found and fixed by this first real
    boot** (none reachable by `cargo check --target x86_64-unknown-linux-gnu`, all in code no
    prior iteration could run): (1) `linux::configure_msr_filter` set an empty
    `MsrFilterRangeFlags` (kernel rejects `flags == 0` outright, `KVM_X86_SET_MSR_FILTER`) with an
    "allow" bitmap bit (backwards — would have let TSC MSR accesses bypass the work-clock even
    past the flags bug); fixed to `READ | WRITE` flags + a "deny" bit. (2) `pagetables::
    long_mode_sregs` left `TR` an all-zero `kvm_segment` (`present=0`, `unusable` also `0`) — VMX
    requires TR always present with a valid busy-TSS type (its unusable bit is reserved, unlike
    every other segment register) — VM-entry failed outright (`KVM_EXIT_FAIL_ENTRY`,
    `EXIT_REASON_INVALID_STATE`); fixed with a minimal valid TR descriptor, `LDTR` explicitly
    marked unusable. (3) A real Linux kernel (the fixture's first, since-replaced version) hung
    forever polling PIT channel 2 (port `0x42`, `quick_pit_calibrate`/
    `native_calibrate_cpu_early`) because CPUID leaves `15H`/`16H` were present-but-zero; fixed by
    synthesizing both leaves in `cpuid.rs`'s mask table to a value matching `VIRTUAL_TSC_KHZ`
    (`LEAF_TSC_CRYSTAL`/`LEAF_PROCESSOR_FREQ`, `TSC_CRYSTAL_HZ`/`PROCESSOR_BASE_MHZ`) — and a
    second hang on CMOS RTC ports `0x70`/`0x71` (the open-bus fallback's fixed `0xFF` always read
    as "Update In Progress"), fixed with a new deterministic `Cmos` shim in `console.rs` (always
    reports UIP clear). All three fixes carry unit tests; full provenance and the exact bug
    mechanics are documented in `tests/fixtures/hello-guest/BUILD.md`.
  - **Not yet done, now unblocked**: booting a *real* Linux kernel end-to-end needs H4 (timer
    interrupt injection — `baud-vcpu::boundary`'s arm-early-then-single-step engine exists but
    is not wired into this crate's run loop yet) so `calibrate_delay()`/the scheduler tick have
    something to wait on; `hello-guest`'s hand-assembled payload deliberately has no such
    dependency and is not a placeholder for that work, just this crate's own H1 fixture.
- **`baud-tape-device` (specs/baud-tape-device.md) — built and wired into `baud-multiverse`'s
  device bus.** New crate (deps = `{baud-proto}` only): `TapeDevice` (pure function of tape bytes +
  guest register writes, `pio_read`/`pio_write` at DATA/CONTROL/STATUS), `ControlOp`
  (PROBE/MARK_BRANCH/GOAL/VIOLATION/LOG), `drain_records()`. Hardware-independent, 18+ tests incl.
  the spec's own named tests and a 128-op proptest fuzz. Extended `baud-proto::Msg` with
  `MarkBranch`/`Log` variants (additive, mirrored in specs/baud-proto.md §5). Wired into
  `baud-multiverse` via `tape_bus::TapeBus` (fixed port window 0x0500) composed into `DeviceBus`
  alongside console + open-bus fallback (todo.md §3.6's "console plus the tape device"); `linux::
  Multiverse::boot` takes the tape directly, `drain_tape_records()` exposes guest output.
  **Not yet done**: no real guest ever writes to this port range (no in-guest driver/shim built in
  `baud-packages` yet — the manifest/lint half now exists, see the guest-image entry below, but not
  the actual driver code); wiring is type-check-only pending real hardware.
- **`baud-snapshot` (specs/baud-snapshot.md) — hardware-independent core + real Linux
  capture/restore/reset built; branch (userfaultfd) still open, real blocker found.** `page_store.rs`
  (content-addressed `PageStore`, blake3-hashed, dedup proven via `Arc::ptr_eq`), `universe.rs`
  (`Universe`/`VcpuState`/`ClockState`/`DeviceState` enumerated capture set, `order_msrs_tsc_first`,
  `restore_plan`, `model_matches`, `dirty_pages`), `dirty_ring.rs` (new this iteration — the pure
  `KVM_CAP_DIRTY_LOG_RING` ring-scan protocol: `harvest()` decodes a `kvm_dirty_gfn` ring into
  harvested `(slot, offset)` pairs and marks them, hardware-independent, 8 tests incl. a proptest
  fuzz), `tree.rs` (branch-point bookkeeping, `nearest_ancestor_at_or_before` for shrink-from-nearest),
  `msr.rs` (MSR constants, single source of truth — `baud-multiverse::timesource` re-exports them),
  `linux.rs` (real `capture`/`restore` walking every `KVM_GET_*`/`KVM_SET_*` the spec enumerates —
  uses `KVM_GET_XSAVE` not `XSAVE2`, sufficient through H5, a bounded follow-up if a guest needs
  AVX-512/AMX — plus new `DirtyRing`: real `KVM_ENABLE_CAP(KVM_CAP_DIRTY_LOG_RING)` + mmap of the
  per-vCPU ring at `KVM_DIRTY_LOG_PAGE_OFFSET` + `KVM_RESET_DIRTY_RINGS`, specs/baud-snapshot.md §5's
  "reset" guarantee). `KVM_RESET_DIRTY_RINGS`'s ioctl number isn't in pinned `kvm-ioctls` 0.25;
  derived via `vmm_sys_util::ioctl::ioctl_expr` (the same helper `kvm-ioctls` itself is built from,
  `KVMIO=0xAE` corroborated from that crate's own doctest) rather than hand-encoded, to bound the
  risk of an unverifiable-on-this-machine mistake. 23/23 tests pass (was 15/15).
  - **`Snapshot::branch` (userfaultfd) still not built — found a real architecture blocker, not
    just a missing ioctl wrapper**: the spec's `UFFDIO_CONTINUE` page-sharing needs the kernel's
    *minor-fault* mechanism, which requires guest RAM backed by a shared (memfd/hugetlbfs) mapping —
    but `baud-multiverse`'s guest RAM (`GuestMemoryMmap::from_ranges`) is a private anonymous
    mapping today. Wiring `UFFDIO_CONTINUE` needs a `baud-multiverse` guest-RAM backing change
    first, not something this crate can absorb alone. The spec's own "small-N fallback" (`fork()`)
    isn't a safe drop-in either: once specs/baud-multiverse.md §3.1's "one VMM thread + one vCPU
    thread" model is live, forking that process only carries the calling thread into the child —
    any lock the other thread held at fork time is frozen forever. Both findings documented in
    `lib.rs`'s module doc and specs/baud-snapshot.md §10 (new); the `bindgen`/`libclang` build-script
    obstacle from prior iterations is now moot either way (blocked upstream of that on the memfd
    question). Two ways forward for whoever picks this up: switch guest RAM to a memfd-backed shared
    mapping and hand-roll the `UFFDIO_*` ioctls (the way `baud-vcpu::linux::pmu` hand-rolls
    `F_SETSIG` — `dirty_ring.rs`'s "derive don't hand-encode" ioctl-number approach applies there
    too), or implement `fork()` as the small-N path with a documented single-threaded-at-fork-time
    contract (e.g. only fork before the vCPU thread is spawned, or STW-pause it first).
  - **Wired into `baud-multiverse`**: `Multiverse::snapshot()`/`Multiverse::restore()`
    (specs/baud-multiverse.md §6). `create_vm_vcpu_shell()` extracted as the shared boot/restore
    prefix; `restore_guest` walks `restore_plan` onto it. **Correctness gap found and fixed**:
    `IA32_TSC_DEADLINE`/`IA32_TSC_AUX` are served entirely by `WorkClock` in software (the MSR
    filter means KVM's own `KVM_GET_MSRS` never sees a guest's real write) — `ClockState` extended
    with `tsc_deadline`/`tsc_aux` fields captured from `WorkClock` directly, not from KVM's MSR
    list (documented inline, `ClockState::tsc_deadline`'s doc). `DeviceBus::restore`/
    `Console::with_output`/`TapeDevice::restore_cursor`/`WorkClock::restore` reassemble the
    device/clock layer snapshot deliberately leaves to the caller. 70/70 tests pass across
    `baud-tape-device`/`baud-snapshot`/`baud-multiverse` (pre-`dirty_ring` count; now 78/78).
  - **`DirtyRing` now wired into `baud-multiverse`'s `Multiverse` (this iteration).**
    `Multiverse::enable_dirty_ring(entries)` negotiates the ring right after boot/restore, before
    any guest execution (`linux/mod.rs`); `Multiverse::reset_dirty_pages(base_ram)` collects the
    harvest, writes back exactly those RAM pages from a caller-supplied base `Universe::ram`, and
    only then confirms the reset to the kernel (a mid-loop write failure leaves the affected pages
    un-confirmed, re-harvested next time, rather than lying to the kernel about what was
    reclaimed). The one piece of real logic in that wiring — reducing a harvest's `(slot, offset)`
    pairs down to RAM page indices — is factored into a new hardware-independent module,
    `crates/baud-multiverse/src/dirty.rs` (`ram_page_indices`), deliberately **not** placed under
    `linux/` (which is `#[cfg(target_os = "linux")]`-gated and so never compiled by `cargo test`
    on this Windows dev machine — a pattern worth remembering for future KVM-adjacent-but-pure
    logic in this crate): 8 unit/property tests prove it with no KVM/mmap at all.
    `cargo test -p baud-multiverse` 42/42 (was 34/34). `enable_dirty_ring`/`reset_dirty_pages`
    themselves are, like every other real ioctl call in this workspace, type-checked
    (`cargo check --target x86_64-unknown-linux-gnu -p baud-multiverse`) but not yet exercised on
    real KVM hardware — nothing calls `snapshot`/`restore`/`DirtyRing` on real KVM hardware yet
    (needs H1 — a real halted guest — first).
  - **Not yet done**: `Snapshot::branch` (see above); no caller (`baud-driver`/`baud-server`) yet
    invokes `enable_dirty_ring`/`reset_dirty_pages` — that is blocked on a real exploration loop
    existing, same as the rest of this crate's snapshot API.
- **`baud-snapshot-store` (specs/baud-snapshot-store.md) — built and unit-tested, fully
  hardware-independent** (no `cfg(target_os = "linux")` half at all — never touches a guest/vCPU).
  `types.rs` (`Sha`/`RunId`/`NodeId`/`Node`/`RunManifest`/`PageRef`; three documented departures
  from the spec's literal pseudocode, mirrored into specs/baud-snapshot-store.md §9), `store.rs`
  (`SnapshotStore`: `put/get_manifest`, `put/get_tape`, `put/get_page` (dedup by plaintext hash),
  `put/get_universe`, `mark_branch` (branch-point-only nodes, this crate's extension),
  `nearest`/`reconstruct` (fork from nearest captured ancestor), `put/get_records` (guest
  tape-device records, CBOR via `baud_proto`)). Bodies are age-encrypted (new
  `baud-keys::age_encrypt`/`age_decrypt`/`age_public_key`, pure-Rust `age` crate 0.12 in-process, no
  libclang/sops/age binary needed); the (run,node) index is plain JSON per spec §4. 28/28 tests
  (9 `baud-keys` + 19 `baud-snapshot-store`) incl. the spec's own three named tests verbatim.
  **Not yet done**: no GC/per-run recipients/remote store (§8, tracked there); nothing in
  `baud-server`/`baud-driver` calls into this crate yet — wiring `baud-snapshot::Universe` bytes
  through `put_universe` and `baud-driver`'s branch choices through `mark_branch` is the natural
  next step once there is a real exploration loop.
- **Guest-image contract + `baud image lint` (specs/baud-packages.md §9, todo.md §4, test matrix
  row 14) — built and unit-tested, wired end-to-end (crate → server → CLI), fully
  hardware-independent.** Closes a real gap found this iteration: `crates/baud-packages` still only
  implemented the *pre-pivot* ptrace-tracee contract (static/no-PIE/musl ELF), not the KVM-era
  bootable-guest-image contract todo.md §4 actually requires — a genuine spec/code inconsistency
  (specs/baud-packages.md described the wrong deliverable), now fixed with a pivot notice at the
  top of that spec and a new §9 documenting the guest-image contract as the top-level one.
  - `crates/baud-packages/src/image.rs` (new module, no `cfg(target_os = "linux")` — operates on
    text, no KVM/nix/hardware needed): `GuestImageManifest::parse_kernel_config(text)` parses the
    standard Linux Kconfig `.config` text format (`CONFIG_FOO=y`/`=m`, `# CONFIG_FOO is not set`);
    `image_lint(manifest)` checks (1) `CONFIG_BAUD_TAPE_DEVICE` (the Kconfig symbol baud's
    out-of-tree tape-device kernel shim would register under, specs/baud-tape-device.md §2's
    "guest-side driver ... shipped in the image") is enabled, and (2) none of
    `CONFIG_RTC_CLASS`/`CONFIG_RTC_DRV_CMOS`/`CONFIG_HPET_TIMER`/`CONFIG_HPET_MMAP`
    (specs/baud-multiverse.md §3.3's "delete HPET/RTC entirely" — the device bus never serves
    them) are enabled; each violation carries a `symbol` + a specific `reason`, both reported
    together in one lint pass. 16 new tests including the spec's own named test
    (`image_lint_requires_tape_driver`) plus RTC/HPET rejection, a well-formed-image pass case,
    `.config`-format parsing (real Kconfig banner comments, string/int-valued symbols correctly
    ignored, module vs. built-in), and a proptest fuzz asserting every subset of the forbidden-timer
    set is fully and correctly reported regardless of which symbols are enabled.
  - Wired end-to-end, mirroring the existing `host probe` pattern exactly: `POST /image/lint`
    (`crates/baud-server/src/routes/image.rs`, registered in `main.rs`) → `baud image lint <path>`
    (`crates/baud-cli/src/cmds/image.rs`, registered in `main.rs`'s `Commands` enum) — reads the
    kernel `.config` file, posts it, exits `1` on any violation (never a false pass, same
    convention as `host probe`'s rejected-regime handling). Manually verified end-to-end this
    iteration (server + CLI, not just unit tests): a well-formed config → `ok:true`, exit 0; a
    config missing the tape driver with RTC enabled → both violations reported with their specific
    reasons, exit 1.
  - `crates/baud-packages/src/lib.rs`'s module doc gained a short pivot note explaining why the old
    `verify_guest_contract` (static/no-PIE ELF check) is retained unchanged — it still applies to
    building individual pieces that end up inside a guest image's rootfs (e.g. the in-guest agent
    binary), just demoted from "the top-level contract" to that narrower scope.
  - **Verification**: `cargo test -p baud-packages` (17/17 pass — 16 new `image` tests + the
    pre-existing `flake`/`spec` tests, 1 pre-existing `#[ignore]`d real-ELF integration test
    unaffected); `cargo build/test/clippy --workspace` all green, 0 regressions, 0 new clippy
    warnings anywhere (confirmed `cargo clippy -p baud-packages -p baud-server -p baud-cli
    --all-targets` specifically — the handful of warnings reported are all pre-existing, in
    unrelated files `net.rs`/`tape.rs`/`fuzz.rs`/`replay.rs`/`tracing.rs`, none in any new/touched
    file); `drive/h0.sh` and `drive/m0.sh` both re-verified passing end-to-end.
  - **Not yet built** (documented in specs/baud-packages.md §9.4): `CONFIG_PIT`/PM-timer gating
    (todo.md §3.3 names them too, but neither has as clean a single boolean Kconfig symbol as
    RTC/HPET — PIT is typically compiled into core x86 platform code, not a separately toggleable
    driver; needs a real kernel `.config` to see what actually needs gating, tracked as a
    follow-up); no real Nix guest-image build pipeline producing a `.config` from a `spec.toml`
    exists yet — `lint_kernel_config` operates on a `.config` handed to it, nothing yet generates
    that `.config` end-to-end from a guest-image spec. That, plus the actual in-guest tape-device
    driver/shim binary itself (the code `CONFIG_BAUD_TAPE_DEVICE=y` would compile), remain open —
    both need a real kernel-build toolchain (Nix + a Linux kernel source tree), not available on
    this dev machine, and are the natural next `baud-packages` increment.
- **`drive/h1.sh` rewritten this iteration to match the current KVM-era H1** (the prior version
  tested the pre-pivot ptrace-era "supervisor MVP" — see the H1 boot-flow entry above): now runs
  `baud host probe` (asserts a non-rejected regime) then `cargo test -p baud-multiverse
  double_boot_memory_identical`, both against real `/dev/kvm`. **`drive/h2.sh`/`h3.sh` remain
  stale** (still validate the old ptrace-era `rdtsc_is_trapped_and_served_virtual_time`/etc.
  definitions, not this document's §10 H2/H3) — the natural next drive-script increment, same
  rewrite pattern `h1.sh` now demonstrates, needs H2's actual double-run/CPUID-fixed-leaves
  behavior exercised for real first.
- H2-H6 and the rest of the M-series remain **not yet started** beyond the above.
- **Found while re-verifying `drive/h0.sh`/`drive/m0.sh` this iteration (environmental, not a code
  bug — not fixed, documented for the next person who hits it)**: on this Windows dev machine, a
  drive script's `trap cleanup EXIT` → `kill "$SERVER_PID"` does not reliably terminate the
  backgrounded native `baud-server.exe` — git-bash's `kill` against a Win32 child spawned with `&`
  can be a no-op instead of an actual termination. A leftover `baud-server.exe` keeps holding port
  7734, so the *next* drive script's freshly spawned server fails to bind and every subsequent
  client call gets "connection actively refused" — which looks exactly like the unrelated, also-
  present `sleep 1`-before-probe race (the fixed 1-second wait before the first CLI call is too
  short whenever the machine is under load, e.g. from a concurrent `cargo test`/`clippy` in another
  shell). Both were hit back-to-back while re-verifying this iteration's change and were not this
  increment's fault: `Get-Process baud-server` + `Stop-Process -Force` (PowerShell) between runs,
  then a clean re-run, confirmed both `drive/h0.sh` and `drive/m0.sh` pass end-to-end with an
  unoccupied port and an unloaded machine. A future iteration should either make the cleanup trap
  use `taskkill //F //PID "$SERVER_PID" //T` (or `wait`-then-verify the port is actually free)
  instead of a bare `kill`, and/or replace the fixed `sleep 1` with a poll-until-`/server/status`-
  responds loop, in whichever drive script is touched next — this was not the current increment's
  scope (`baud-multiverse`'s work-clock/console/`Multiverse` wiring) and neither script's own logic
  needed a change to pass.
- **Fixed while validating H0 on this Windows dev machine** (pre-existing, not specific to
  baud-host): every `drive/*.sh` that spawns `baud-server` with a temp SQLite file passed a
  POSIX `mktemp -t ...` path (e.g. `/tmp/baud-h0-XXXX.sqlite`) straight into `BAUD_DB`; a plain
  win32 binary doesn't understand that path and sqlx failed with "unable to open database file"
  — every drive script that starts a server was silently broken here. Fixed by translating the
  path through `cygpath -m` (falls back to the original path where `cygpath` doesn't exist, i.e.
  real Linux/macOS hosts) right after each `mktemp` call, and made the temp-file cleanup in each
  `trap ... EXIT` tolerant of Windows briefly holding the file handle after the server process is
  killed (`sleep 0.2; rm -f ... || true` — an unguarded failing `rm` in an `EXIT` trap was
  clobbering an otherwise-passing script's exit code under `set -e`). Verified via `drive/h0.sh`
  and `drive/m0.sh` (both now exit 0 end-to-end on this machine).
- **Known gap found while spot-checking `drive/m1.sh` on this machine** (not fixed this
  iteration — unrelated to H0/baud-host, and out of scope for this increment): `drive/m1.sh` (and
  likely other M-series scripts) shells out to `python3` to parse CLI JSON output; this dev
  machine has no `python3` installed, so M1's JSON-field assertions fail even though the
  underlying `baud tape create` call itself succeeds (valid JSON with an `id` field came back).
  Needs either installing `python3` on this dev machine or rewriting those parses in
  `jq`/shell — whichever a future iteration picks, audit all of `drive/m*.sh` and
  `drive/full-demo.sh` for the same pattern.
