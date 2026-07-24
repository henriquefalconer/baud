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

- **H0 (capability spike) — in progress.** `crates/baud-host` implements `Host::probe()` /
  `Probe` / `Regime` / `Vendor` / fleet `Placement` per `specs/baud-host.md` §3-§5, wired to
  `GET /host/probe` (`baud-server`) and `baud host probe --json` (`baud-cli`). The
  regime-decision logic (`compute_probe`, capacity/placement, sibling-safety) is
  hardware-independent and unit-tested via an injectable `CapabilityChecks` seam
  (`cargo test -p baud-host` — `capacity_refuses_sibling_split`, `doctor_checks_kvm`,
  `rejected_host_names_the_failing_check`, plus vendor/gate edge cases). The real Linux
  implementation (`crates/baud-host/src/linux.rs`: `/dev/kvm`, `/proc/cpuinfo`,
  `kvm-ioctls`/`kvm-bindings` CPUID/TSC/MSR-filter/single-step round-trips, a
  `perf-event`-based branch-counter determinism smoke test, `/sys` topology parsing) type-checks
  against the real crate sources (`cargo check --target x86_64-unknown-linux-gnu -p baud-host`)
  but is **not yet validated on real KVM hardware** — this dev machine has no Linux/KVM host
  (see `docs/determinism.md`'s H0 section and `CLAUDE.md`). `drive/h0.sh` runs the probe
  end-to-end and asserts the JSON shape + honest-rejection behavior on whatever host it runs on.
  **Next H0 action**: get this validated on a real Linux/KVM host (bare-metal, or a WSL2 distro
  with nested virt) and record the result in `docs/determinism.md`, replacing the "not yet
  exercised" caveat.
- **`docs/determinism.md`** still describes the superseded ptrace/seccomp mechanism below its
  2026-07-24 pivot notice; a full rewrite for the KVM/VT-x model is still open (tracked here, not
  yet started beyond the H0 addendum).
- **`baud-vcpu` (specs/baud-vcpu.md) — core built, not yet wired into a real VMM.** New crate:
  the exit-dispatch match (`Exit`/`Bus`/`TimeSource`/`dispatch_exit`/`DeterminismHole`,
  `crates/baud-vcpu/src/lib.rs`) and the arm-early-then-single-step interrupt-injection engine
  (`ExecPoint`/`PmuStepper`/`inject_at`/`MARGIN`, `crates/baud-vcpu/src/boundary.rs`) are
  hardware-independent, exhaustively matched (no wildcard arm — `Exit::Unmodeled` is the only
  path to `Err`), and unit-tested on this Windows dev machine with no KVM at all: 15/15 tests
  pass, including a 1,000-case proptest fuzz for `no_unmodeled_exit_is_silent`, the open-bus
  fixed-byte (`0xFF`, never host memory) behavior, `vm_creation_refuses_multiple_vcpus`, and an
  `inject_at`-driven `timer_tick_lands_at_identical_instruction`-style tuple-equality test
  (`identical_target_yields_identical_injection_tuple_across_runs`). The real Linux half
  (`crates/baud-vcpu/src/linux/{mod,pmu}.rs`: `kvm_ioctls::VcpuExit` → `Exit` conversion covering
  every variant in kvm-ioctls 0.25, thread affinity pin, `KVM_SET_GUEST_DEBUG` single-step toggle,
  and a `perf_event_open` branch-counter + SIGIO-driven `LinuxPmuStepper` that interrupts a
  blocking `KVM_RUN` via `kvm_run.immediate_exit`) type-checks clean (`cargo check`/`clippy
  --target x86_64-unknown-linux-gnu -p baud-vcpu`) but is **not yet exercised on real KVM/perf
  hardware** — same caveat as `baud-host`. **Known scope gap to revisit once baud-multiverse's
  thread model exists**: `LinuxPmuStepper`'s signal delivery uses process-wide `F_SETOWN` (assumes
  the vCPU thread is the only thread with the overflow signal unblocked), not
  `F_SETOWN_EX(F_OWNER_TID, ...)` — documented in `crates/baud-vcpu/src/linux/pmu.rs`'s module
  doc, revisit when the "one VMM thread + one vCPU thread" model (specs/baud-multiverse.md §3.1)
  actually lands. **Not yet built**: `baud-vcpu` is not wired into any VM/vCPU creation flow (no
  `Kvm::new`/`create_vm`/`create_vcpu` boot flow — that is `baud-multiverse`'s job per the crate
  split in specs/baud-vcpu.md §1) and is not yet a dependency of `crates/baud-multiverse`.
- **`baud-multiverse` boot flow (specs/baud-multiverse.md §2) — built, not yet wired to a real
  KVM host or to `baud-server`/`baud-tape-agent`.** New modules, additive alongside the untouched
  pre-pivot ptrace simulation the server/agent still actually run guests through today (see the
  pivot notice at the top of `crates/baud-multiverse/src/lib.rs`):
  - `crates/baud-multiverse/src/cpuid.rs` — the determinism mask table (specs/baud-multiverse.md
    §4, todo.md §3.2): clears RDRAND/RDSEED/TSX-HLE/TSX-RTM/x2APIC, pins the `0BH`/`1FH`
    extended-topology leaves to one core no-SMT, sets invariant-TSC and a fixed hypervisor-present
    bit. Hardware-independent (a `CpuidEntry` trait implemented for both a portable `CpuidLeaf` and
    — directly, unmodified — `kvm_bindings::kvm_cpuid_entry2`), unit-tested on this Windows machine
    with no KVM at all: 7 tests including a proptest fuzz (`masked_bits_are_always_fixed_regardless_
    of_host_input`) covering `cpuid_leaves_are_fixed`'s guarantee for any starting bit pattern.
  - `crates/baud-multiverse/src/layout.rs` — fixed guest-physical addresses (RAM/kernel/cmdline/
    zero-page/page-table addresses, specs/baud-multiverse.md §2's "write boot params at fixed
    addresses") and `build_identity_page_tables`, a pure function building the long-mode identity
    map (1 PML4 + 1 PDPTE + N PDE pages of 2 MiB leaves). 6 tests, plus a module-level `const`
    assertion (not a runtime one — both sides are compile-time constants) that the fixed regions
    never overlap.
  - `crates/baud-multiverse/src/linux/{mod,pagetables,bootparams}.rs` (`cfg(target_os =
    "linux")`) — the real boot flow: `Kvm::new` → `create_vm` → register one zeroed guest-RAM
    region (`vm-memory` `GuestMemoryMmap`) → `create_vcpu` → apply the CPUID mask + `KVM_SET_
    CPUID2` → write the identity page tables into guest RAM → `KVM_SET_SREGS` into 64-bit long
    mode (CR0.PE|PG, CR4.PAE, EFER.LME|LMA, flat 64-bit code/data segments — the standard
    rust-vmm direct-boot technique that skips the kernel's own real-mode setup code entirely) →
    `KVM_X86_SET_MSR_FILTER` + `KVM_CAP_X86_USER_SPACE_MSR` (reason `Filter`) routes
    `IA32_TSC`/`IA32_TSC_DEADLINE`/`IA32_TSC_AUX` to userspace exits → `KVM_SET_TSC_KHZ` pins
    `VIRTUAL_TSC_KHZ` (1 GHz, host-independent) → `linux_loader::loader::bzimage::BzImage::load`
    loads the kernel, builds the `boot_params` zero page (e820 map: first MiB reserved, rest
    usable RAM) and command line, `LinuxBootConfigurator::write_bootparams` → `RIP` set to
    `kernel_load + 0x200` (the Linux/x86 64-bit boot protocol's direct entry point), `RSI` = zero
    page, ready for `baud_vcpu::linux::run_until_halted`. Type-checks and clippy-clean against the
    real crate sources (`cargo check`/`clippy --target x86_64-unknown-linux-gnu -p
    baud-multiverse`, including the `linux`-only tests) but **not yet exercised on real KVM
    hardware** — same caveat as `baud-host`/`baud-vcpu` (no Linux/KVM host on this dev machine,
    CLAUDE.md).
  - **Work-clock `TimeSource` + console `Bus` + a `Multiverse` boot/run struct — built, wired
    together, still type-check-only pending real hardware.** Closes most of the previous "Next
    build action" below:
    - `crates/baud-multiverse/src/timesource.rs` (hardware-independent, no `cfg` at all) — `WorkClock<C:
      BranchCounter>` implementing `baud_vcpu::TimeSource`: `virtual_tsc = base + k * rcb`
      (todo.md §3.3), served for `IA32_TSC`/`IA32_TSC_DEADLINE`/`IA32_TSC_AUX` (the MSR constants
      formerly duplicated in `linux/mod.rs` now live here and are imported by it); a guest `wrmsr`
      to `IA32_TSC` rebases `base` so the next read reflects the written value exactly. Generic
      over a `BranchCounter` trait, so the formula's monotonicity/reproducibility
      (`work_clock_is_monotone_and_reproducible`) is unit-tested on this Windows machine with a
      scripted mock counter — no perf/KVM hardware needed for this half. 5 tests.
    - `crates/baud-multiverse/src/console.rs` (hardware-independent — `vm_superio` has zero non-dev
      dependencies of its own, confirmed against its Cargo.toml, so it moved out of the
      `cfg(target_os = "linux")` dependency block to a plain one) — `Console`, a COM1 (0x3f8-0x3ff)
      16550 UART built on `vm_superio::Serial` with an in-memory `Vec<u8>` writer and a
      recording-not-delivering `NoIrqTrigger` (no interrupt controller wired yet — that lands with
      the tape device); `DeviceBus` composes `Console` with `baud_vcpu::OpenBusFallback` for every
      other address. 5 tests, including that the LSR always reports "ready to transmit" (a real
      guest UART driver would spin forever otherwise).
    - `crates/baud-multiverse/src/linux/mod.rs` — `LinuxBranchCounter` (a free-running
      `perf_event_open(PERF_COUNT_HW_BRANCH_INSTRUCTIONS)` counter, distinct from
      `baud_vcpu::linux::pmu::LinuxPmuStepper`'s own armed/overflow counter — reconciling the two
      into one perf fd is deferred to when this can actually be exercised on real perf/KVM
      hardware, documented in-line same as `pmu.rs`'s existing scope note) implementing
      `timesource::BranchCounter`; and `linux::Multiverse` — `boot(kernel_path, cmdline, base, k)`
      runs `boot_guest` plus this wiring, `run_to_first_halt()` drives
      `baud_vcpu::linux::run_until_halted` to the guest's first `Hlt`/`Shutdown` through the new
      `DeviceBus`/`WorkClock` and returns a `HaltOutcome { console_output, ram_hash }` — the real
      boot flow behind specs/baud-multiverse.md §8's `boot(...).ram_hash_at_first_hlt()`
      pseudocode. This is H1's target ("boot a guest, print to the serial console, clean
      `Hlt`/`Shutdown`"), not yet the full §6 API — `snapshot`/`restore` still need `baud-snapshot`
      (doesn't exist yet) and the tape-device-driven `run(tape: impl TapeSource)` signature still
      needs `baud-tape-device` (doesn't exist yet either), so neither is stubbed here.
    - **Verification**: `cargo test -p baud-multiverse` — 28/28 pass natively on this Windows
      machine (9 new: 5 `console` + 4 `timesource`, the rest pre-existing and unaffected); `cargo
      check`/`clippy --target x86_64-unknown-linux-gnu -p baud-multiverse --all-targets` clean for
      every new/touched file (0 new warnings; `console.rs`/`timesource.rs`/`linux/mod.rs` each
      contribute zero clippy warnings on both the native and Linux targets — the only warnings
      clippy reports are the same ~10 pre-existing ones in the untouched pre-pivot `lib.rs`
      simulation code, already tracked, not this increment's); `cargo build`/`test --workspace`
      green (all crates, no regressions); `drive/h0.sh` still exits 0 (re-verified post-change).
    - **Not yet done**: no tape-device `Bus` (still needs `baud-tape-device`, H2/§3.5); nothing
      calls `Multiverse::boot`/`run_to_first_halt` end-to-end on real hardware yet — no Linux/KVM
      host exists on this dev machine (same caveat as every other `linux/` module, CLAUDE.md).
      **Next build action**: get an actual Linux/KVM host (bare-metal or a WSL2 distro with nested
      virt enabled — `wsl -l -v` currently lists no installed distro on this machine, so that is
      itself the first step) and drive `linux::Multiverse::boot` against a real minimal kernel
      image end-to-end, replacing every "type-check-only" caveat above with an actual boot; H0's
      own "not yet validated on real KVM hardware" caveat blocks the same way and should be closed
      at the same time (same missing host). Once that lands: `drive/h1.sh` per §10 ("boot the hello
      image, assert expected console output; `double_boot_memory_identical` passes").
- **`baud-tape-device` (specs/baud-tape-device.md) — built and wired into `baud-multiverse`'s
  device bus.** New crate (`crates/baud-tape-device`, deps = `{baud-proto}` only, matching the
  spec's §2 rationale): `TapeDevice` — a pure function of the tape bytes and the guest's own
  register writes (`pio_read(off)`/`pio_write(off,b)` at `reg::DATA`/`CONTROL`/`STATUS` per §3),
  `ControlOp` (`PROBE`/`MARK_BRANCH`/`GOAL`/`VIOLATION`/`LOG`, §4), and `drain_records() ->
  Vec<Msg>`. Hardware-independent, no KVM/perf needed: 18 unit/property tests on this Windows
  machine, including the spec's own named tests (`all_input_is_tape_derived`,
  `read_past_end_is_fixed`) translated onto this crate's pure device model, plus a 128-op proptest
  fuzz (`arbitrary_register_traffic_never_panics`) asserting arbitrary guest register traffic never
  panics the device (may legitimately report `OpcodeResult::MalformedPayload`/`UnknownOpcode`).
  Extended `baud-proto`'s `Msg` enum with two new variants the tape device's control opcodes need
  and no existing message fit (`MarkBranch { step }` for `MARK_BRANCH`, `Log { bytes, step }` for
  `LOG`) — additive, not a breaking wire change (existing golden vectors unaffected; new
  roundtrip/proptest coverage added), and mirrored into `specs/baud-proto.md` §5 so the wire
  schema doc stays the single source of truth. `PROBE`'s outbound-buffer wire format (key-length
  byte + key bytes + opaque value bytes) is this crate's own implementation choice — the spec
  names the opcode's meaning but not a byte layout, documented inline in `parse_probe`'s doc
  comment. **Wired into `baud-multiverse`** (`crates/baud-multiverse/src/tape_bus.rs`, new,
  hardware-independent — same split as `console.rs`): `TapeBus` adapts `TapeDevice`'s per-byte API
  onto `baud_vcpu::Bus`'s slice-based PIO interface at a fixed port window (`TAPE_DEVICE_BASE =
  0x0500`, chosen to avoid `console::COM1_BASE`'s 0x3f8-0x3ff range); `console::DeviceBus` gained a
  `tape: TapeBus` field routed ahead of the `OpenBusFallback` catch-all (todo.md §3.6's subtractive
  rule: "down to a console plus the tape device" — now both are actually on the bus). `linux::
  Multiverse::boot` now takes the run's `tape: Vec<u8>` directly and seeds `DeviceBus::with_tape`
  with it (`DeviceBus::with_tape` added because `DeviceBus`'s `fallback` field is private to
  `console.rs` — struct-update syntax from `linux/mod.rs` couldn't construct it directly); a new
  `Multiverse::drain_tape_records()` exposes what the guest emitted after a run. This closes the
  "tape-device-driven `run(tape: impl TapeSource)` signature still needs `baud-tape-device`" gap
  called out in iteration 4's notes below (the shape differs slightly from the spec's — `boot`
  takes the tape directly since there is exactly one run per `Multiverse` today; re-running the
  same booted guest against a different tape is `baud-driver`'s eventual job, not built yet).
  **Verification**: `cargo test -p baud-proto -p baud-tape-device -p baud-multiverse` — 12+18+33 =
  63 tests pass (18 new in `baud-tape-device`, 5 new `tape_bus` tests + 2 new/extended `console`
  tests in `baud-multiverse`, 2 new roundtrip tests in `baud-proto`); `cargo check`/`clippy
  --target x86_64-unknown-linux-gnu -p baud-multiverse --all-targets` clean for the `tape_bus`
  module and the updated `linux/mod.rs` (0 new warnings — the pre-existing warnings are all in
  `lib.rs`'s untouched pre-pivot simulation code, same as every prior iteration); `cargo
  build/test/clippy --workspace` all green, 0 regressions; `drive/h0.sh` and `drive/m0.sh` both
  re-verified passing end-to-end (stale-server port-7734 environmental issue avoided this time by
  killing any leftover `baud-server.exe` via PowerShell before each run, per the note below).
  **Not yet done**: no real guest ever writes to this port range yet (no in-guest driver/shim
  exists — that is `baud-packages`' job per specs/baud-tape-device.md §2's "guest-side driver
  contract", not started); `TapeBus`/`DeviceBus` wiring is type-check-only on this dev machine,
  same "not yet exercised on real KVM hardware" caveat as the rest of `linux/`.
- **`baud-snapshot` (specs/baud-snapshot.md) — hardware-independent core built and unit-tested,
  plus a real Linux capture/restore backend; branch/reset (userfaultfd/dirty-ring) not yet
  built.** New crate (`crates/baud-snapshot`, deps = `{baud-proto, kvm-ioctls, kvm-bindings,
  vm-memory}` — `userfaultfd` deliberately not pinned yet, see below):
  - `page_store.rs` — `PageStore`/`PageRef`/`PageHash`: content-addressed guest-RAM pages
    (blake3-hashed, same hash `baud-multiverse::linux::Multiverse::ram_hash` already uses),
    deduplicated across every universe interned through one store (specs/baud-snapshot.md §4's
    "per-branch memory ∝ write set" — proven at this layer by `Arc::ptr_eq` identity, not just
    content equality). Hardware-independent, no KVM at all: 4 tests including a 1,000-universe
    dedup proof (`a_thousand_mostly_identical_universes_share_the_common_pages`).
  - `universe.rs` — the enumerated capture set (`Universe`/`VcpuState`/`ClockState`/`DeviceState`,
    specs/baud-snapshot.md §3), `order_msrs_tsc_first` (§6: "restore `IA32_TSC` before
    `IA32_TSC_DEADLINE`" — a stable 3-way-rank sort, order-of-input-independent), `restore_plan`
    (§6's ordered restore sequence: TSC frequency first, RAM before any vCPU-state write, VM clock
    only after every vCPU-state field, device state last), `model_matches`
    (`restore_refuses_mismatched_cpu`'s CPU-signature guard), and `dirty_pages` (the *planning*
    half of §5's "reset cost scales with write-set, not total RAM" — real
    `KVM_CAP_DIRTY_LOG_RING` bookkeeping is not built yet, but the guarantee that a reset only
    touches what actually changed is proven here independent of it). 7 tests.
  - `tree.rs` — `Tree`/`NodeId`: in-memory branch-point bookkeeping (parent/child links,
    `nearest_ancestor_at_or_before`) so exploration/shrinking can fork from the nearest snapshot
    instead of the root (§5's `shrink_reproduces_from_nearest_snapshot`, todo.md problem #22). 4
    tests.
  - `msr.rs` — `MSR_IA32_TSC`/`_DEADLINE`/`_AUX` moved here as the single source of truth (this
    crate sits below `baud-multiverse` in the dependency graph per specs/baud-snapshot.md §2's
    diagram); `baud-multiverse::timesource` now `pub use`s them instead of duplicating the values,
    and `baud-multiverse`'s `Cargo.toml` gained a `baud-snapshot` path dependency.
  - `linux.rs` (`cfg(target_os = "linux")`) — real `capture`/`restore` functions walking every
    `KVM_GET_*`/`KVM_SET_*` specs/baud-snapshot.md §3 enumerates (regs/sregs/msrs via
    `KVM_GET_MSR_INDEX_LIST` + `KVM_GET_MSRS`/`KVM_SET_MSRS`/lapic/xsave (`KVM_GET_XSAVE`, not
    `KVM_GET_XSAVE2` — see note below/xcrs/vcpu_events/mp_state/`KVM_GET_CLOCK`/`KVM_GET_TSC_KHZ`),
    plus RAM paged through `page_store`; `restore` walks `universe::restore_plan` literally,
    step-by-step, and refuses a CPU-model mismatch via `model_matches` before touching anything.
    Type-checks and clippy-clean against the real crate sources (`cargo check`/`clippy --target
    x86_64-unknown-linux-gnu -p baud-snapshot`) but **not yet exercised on real KVM hardware** —
    same caveat as every other `linux/` module (no Linux/KVM host on this dev machine, CLAUDE.md).
    Uses `KVM_GET_XSAVE`/`KVM_SET_XSAVE` (the fixed 4096-byte struct) rather than the spec's named
    `KVM_GET_XSAVE2` — sufficient for the minimal single-guest-kernel images baud targets through
    H5 (no dynamically-enabled AVX-512/AMX XSTATE features in play); switching to XSAVE2 for a
    guest that needs it is a bounded follow-up, not a redesign (`Xsave`'s FAM-sized allocation is
    the only new piece).
  - **Deliberately not built this iteration** (tracked as the next action, not a stub — nothing
    here pretends to work): userfaultfd-based CoW branching (`Snapshot::branch`) and
    `KVM_CAP_DIRTY_LOG_RING`-based cheap reset (`Snapshot::reset`), specs/baud-snapshot.md §4-§5.
    Root cause, discovered this iteration and worth recording so nobody re-derives it: the
    `userfaultfd` crate's sys bindings (`userfaultfd-sys`) generate their FFI layer with `bindgen`
    at build time, which needs `libclang` — and a build script always runs on the *host*
    regardless of `--target`, so even `cargo check --target x86_64-unknown-linux-gnu` for a crate
    depending on `userfaultfd` fails right here on this Windows box with "Unable to find libclang"
    (confirmed by fetching the crate into a scratch project and running exactly that check — see
    `crates/baud-snapshot/src/linux.rs`'s module doc for the two ways forward: install
    LLVM/libclang here, or hand-roll the handful of needed `UFFDIO_*` ioctls against
    `libc::syscall(SYS_userfaultfd, ..)` the same way `baud-vcpu::linux::pmu` already hand-rolls
    `F_SETSIG` instead of pulling in a heavier dependency).
  - **Verification**: `cargo test -p baud-snapshot` — 15/15 pass natively on this Windows machine
    (0 KVM/perf needed for any of it); `cargo check`/`clippy --target x86_64-unknown-linux-gnu -p
    baud-snapshot -p baud-multiverse --all-targets` clean for every new/touched file (0 new
    warnings — one unused-import warning surfaced mid-iteration and was fixed before this checkpoint,
    not left in; the only warnings clippy reports across the whole workspace are the same
    pre-existing ones in `baud-multiverse`'s untouched pre-pivot
    `lib.rs` and a handful of unrelated crates, already tracked, not this increment's); `cargo
    build/test --workspace` green (all crates, no regressions, 15 new tests total: `baud-snapshot`
    didn't exist before this iteration); `drive/h0.sh` and `drive/m0.sh` re-verified passing
    end-to-end (killing any leftover `baud-server.exe` via PowerShell first, per the established
    note below).
- **`baud-snapshot` wired into `baud-multiverse`'s boot/run flow — `Multiverse::snapshot`/
  `Multiverse::restore` built (specs/baud-multiverse.md §6's `Snapshot::capture`/
  `Snapshot::restore` API), type-check-only pending real hardware.** Closes the "capture/restore
  are not called from `baud-multiverse` yet" gap the previous iteration left open:
  - `crates/baud-multiverse/src/linux/mod.rs` — `create_vm_vcpu_shell()` extracted from
    `boot_guest` (the shared `Kvm::new → create_vm → register zeroed RAM → create_vcpu → CPUID mask
    → MSR filter` prefix both a fresh boot and a restore need identically); `restore_guest(universe,
    template_active)` builds that shell then walks `baud_snapshot::linux::restore`'s `restore_plan`
    onto it. `Multiverse::snapshot(&mut self, page_store: &mut PageStore) -> Result<Universe, ...>`
    calls `baud_snapshot::linux::capture` against this instance's own KVM handles plus the three
    pieces of state only this crate's device models know (work-clock anchor/deadline/aux,
    tape-device cursor, console output). `Multiverse::restore(universe, tape, k, template_active) ->
    Result<Self, RestoreError>` is the inverse: `restore_guest` rebuilds the KVM/vCPU/RAM state,
    then `DeviceBus::restore` (new, `console.rs`) and `WorkClock::restore` (new, `timesource.rs`)
    reassemble the device/clock layer `baud-snapshot` deliberately leaves to the caller
    (`RestoreStep::RestoreDevice`'s doc) — `tape` is the run's whole tape (unchanged across a run,
    same value the original `boot` call used), fast-forwarded to the captured cursor via
    `baud-tape-device`'s new `TapeDevice::restore_cursor`; the console is pre-seeded with the
    captured output history via `Console::with_output` so `.output()` shows the full history
    post-restore, not just what's written after.
  - **Correctness gap found and closed while wiring this**: `IA32_TSC_DEADLINE`/`IA32_TSC_AUX` are
    served entirely by `WorkClock` in software (the MSR filter routes them to userspace, so a guest
    `wrmsr` to either never reaches KVM's own MSR storage) — a capture that only saved
    `KVM_GET_MSRS`' view of those two indices would silently restore stale/zero values regardless of
    what the guest had actually armed. Fixed by extending `baud-snapshot::universe::ClockState` with
    `tsc_deadline`/`tsc_aux` fields (alongside the existing `work_clock_base`), threading them
    through `baud_snapshot::linux::capture`'s signature, and adding `WorkClock::restore`/`base()`/
    `tsc_deadline()`/`tsc_aux()` accessors so `Multiverse::snapshot`/`restore` round-trip them
    exactly — documented inline on both sides (`ClockState::tsc_deadline`'s doc, `WorkClock::
    restore`'s doc) so a future reader doesn't have to re-derive why `vcpu.get_msrs()` alone isn't
    enough for a filtered MSR.
  - **Verification**: `cargo test -p baud-tape-device -p baud-snapshot -p baud-multiverse` — 70/70
    pass natively (19+15+36; new: 1 `TapeDevice::restore_cursor` test, 2 `console.rs` tests
    (`Console::with_output`, `DeviceBus::restore`), 1 `WorkClock::restore` round-trip test); `cargo
    check`/`clippy --target x86_64-unknown-linux-gnu -p baud-snapshot -p baud-multiverse -p
    baud-tape-device --all-targets` clean (0 new warnings — one `clippy::default_constructed_unit_
    structs` this iteration introduced was fixed before the checkpoint, not left in); `cargo
    build/test/clippy --workspace` all green, 0 regressions; `drive/h0.sh` and `drive/m0.sh`
    re-verified passing end-to-end (both hit the already-documented "sleep 1 too short" race at
    least once this iteration — re-running cleanly after killing the leftover `baud-server.exe`
    resolved it both times, consistent with the existing note below, not a new issue).
  - **Not yet done**: `baud-snapshot`'s own branch/reset (userfaultfd/dirty-ring) is still open (see
    above); the `baud-snapshot-store` crate does not exist yet; no in-guest tape-device driver/shim
    exists in `baud-packages` yet; nothing calls `Multiverse::snapshot`/`restore` on real KVM
    hardware yet (same "no Linux/KVM host on this dev machine" caveat as every other `linux/`
    module, CLAUDE.md) — the round-trip (`snapshot_roundtrip_is_bit_identical`) is provable in unit
    tests at the level each underlying piece already covers (restore ordering, MSR round-tripping,
    tape-cursor/console reconstruction) but not yet as one end-to-end real-hardware test, which
    needs H1 (booting a real guest) to land first since there is no real halted guest to snapshot
    from yet.
- **`baud-snapshot-store` (specs/baud-snapshot-store.md) — built and unit-tested, fully
  hardware-independent.** New crate (`crates/baud-snapshot-store`, deps = `{blake3, baud-keys,
  baud-proto}` per the spec's §2 pinned skeleton, plus `serde`/`serde_json`/`thiserror`): the
  durable, content-addressed, age-encrypted-at-rest record of a run (universe tree + tape + run
  manifest, §1) that supersedes `baud-journal`'s replay-from-zero design. Unlike every other
  post-pivot crate so far, this one has **no `cfg(target_os = "linux")` half at all** — it never
  touches a guest/vCPU (§1's own Non-Goal: "Capturing/restoring VM state (that is
  `baud-snapshot`)"), so every test here runs and proves the real behavior on this Windows dev
  machine with no "not yet exercised on real KVM hardware" caveat.
  - `crates/baud-snapshot-store/src/types.rs` — `Sha` (blake3 content hash, hex-roundtrippable),
    `RunId`, `NodeId` (= `Sha`), `Node`, `RunManifest`, `PageRef`. Three documented departures
    from specs/baud-snapshot-store.md §3's literal Rust pseudocode (each with an inline doc
    comment explaining why, and mirrored into the spec's new §9 "Implementation Notes"): (1)
    `Node.universe: Option<Hash>` — not every branch point has a captured universe (capturing is
    comparatively expensive; `MARK_BRANCH` events are far more frequent than actual snapshots);
    (2) `RunManifest.regime: String` — this crate's declared deps deliberately exclude
    `baud-host`, so the regime enum (`baud_host::Regime` remains its single source of truth) is
    archived as an opaque tag, not interpreted; (3) `NodeId` is content-addressed from
    `(parent, at_step, tape_range)` via `Sha::of_node_identity`, not an opaque counter — two
    `put_universe`/`mark_branch` calls describing the same branch point collapse onto the same
    node.
  - `crates/baud-snapshot-store/src/store.rs` — `SnapshotStore`: `open`/`open_with_keys`,
    `put_manifest`/`get_manifest`, `put_tape`/`get_tape`, `put_page`/`get_page`,
    `put_universe`/`get_universe`, `mark_branch` (this crate's extension — records a
    branch-point-only node with `universe: None`, idempotent via content-addressing),
    `nearest`/`reconstruct` (walk parent links past `None`-universe nodes to the nearest captured
    ancestor, §5's "fork from nearest, not root"), `put_records`/`get_records` (this crate's other
    extension — persists the guest's tape-device `baud_proto::Msg` write-channel records per node,
    CBOR-encoded via `baud_proto::encode`/`decode`, reusing the existing wire format rather than
    inventing a second one). Every body (`universe`/page/tape/records) is age-encrypted via new
    `baud-keys` functions; every index file (`manifest.json`, `nodes/*.json`) is plain JSON, per
    §4's "In clear: only the (run, node) index + addresses."
  - **`baud-keys` gained real `age` encryption** (`age_public_key`/`age_encrypt`/`age_decrypt`,
    `crates/baud-keys/src/lib.rs`): uses the pure-Rust `age` crate 0.12 in-process (streamlined
    `age::encrypt(&Recipient, &[u8])`/`age::decrypt(&Identity, &[u8])` API, confirmed against the
    real crate docs) rather than shelling out to an `age`/`sops` binary per call — no
    `libclang`/`bindgen` dependency (unlike the `userfaultfd` crate blocking `baud-snapshot`'s
    branch/reset, §14's H5 entry below), so it builds and its tests run fully on this Windows dev
    machine with no external `sops`/`age` binaries installed at all (confirmed: neither is on
    PATH here). `age_key_path()`'s existing $SOPS_AGE_KEY_FILE → macOS → Linux resolution order is
    reused unchanged, so one identity file guards both `infra/secrets` and this new store.
  - **Verification**: `cargo test -p baud-keys -p baud-snapshot-store` — 28/28 pass (9 new
    `baud-keys` age tests incl. a roundtrip against a hardcoded throwaway test keypair and
    negative paths for malformed recipients/identities; 19 new `baud-snapshot-store` tests incl.
    the spec's own three named tests verbatim —
    `snapshot_store_bodies_are_ciphertext`/`pages_dedup_by_plaintext_hash`/
    `reconstruct_forks_from_nearest_node` — plus a 40-case proptest fuzz on node-identity
    content-addressing and coverage for run-id path-escape sanitization, missing-key error paths,
    and manifest/node-index plaintext-readability); `cargo build/test/clippy --workspace` all
    green (0 regressions, 0 new clippy warnings in either touched crate — the only warnings
    reported anywhere in the workspace are pre-existing ones in unrelated crates, same as every
    prior iteration); `drive/h0.sh` and `drive/m0.sh` both re-verified passing end-to-end.
  - **Not yet done**: no garbage collection / per-run recipients / remote store (§8, already
    tracked there as future work); nothing in `baud-server`/`baud-driver` calls into this crate
    yet (it exists standalone, same "built, not yet wired up" stage `baud-snapshot` itself passed
    through for two iterations before `baud-multiverse` wired it in) — wiring
    `baud-snapshot::Universe` capture bytes through `SnapshotStore::put_universe`'s `body: &[u8]`
    parameter, and `baud-driver`'s branch-point choices through `mark_branch`, is the natural next
    step once there is a real exploration loop to drive it.
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
