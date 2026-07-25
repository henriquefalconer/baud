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
- **Regime split for the random instruction** (§3.8): cooperative = the masked CPUID bit is *hardware*
  enforced — VT-x checks each instruction's own CPUID gate (`RDRAND[bit 30] = 0 ⇒ #UD`, SDM) against the
  guest's configured leaves, so a guest that issues `rdrand`/`rdseed` anyway takes `#UD` (→ triple fault →
  `Halted` for a guest with no handler) instead of reading real entropy; no guest, compliant or adversarial,
  can reach it. Enforced = hardware random-instruction exiting traps every attempt *before* that `#UD` check
  and serves the tape.
- **Test** (`rdrand_guest_is_flagged`): a guest that ignores the mask and executes the raw random
  instruction never gets past it — in cooperative regime two boots produce byte-identical output stopping at
  the pre-`rdrand` marker (deterministic, *not* a divergence: the original divergence assumption was
  falsified on real hardware, see `crates/baud-multiverse/tests/fixtures/rdrand-guest/BUILD.md`) — and a
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
  that do not fight it (masked CPUID *hardware-blocks* the raw random instruction for any guest — `#UD`,
  §3.2 — while the raw timestamp instruction, which has no CPUID gate, still relies on a compliant guest).
  No kernel changes.
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
  a raw-random guest is hardware-blocked (cooperative: `#UD` on `rdrand`) or trapped (enforced). Drive
  `drive/h3.sh`: `rdrand_guest_is_flagged`, `regime_is_recorded_and_not_overclaimed`.
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
| 1 | Stock KVM won't force RDTSC/random-instruction exiting from userspace | Two regimes; cooperative masks CPUID (which hardware-blocks `rdrand`/`rdseed` outright — `#UD`), enforced adds a KVM module for RDTSC; run records regime | `regime_is_recorded_and_not_overclaimed`; `rdrand_guest_is_flagged` |
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
  injection specifically, H4) is now exercised for real too, against a real vCPU and a real
  delivered interrupt (`timer_tick_lands_at_identical_instruction`, see the H4 entry below). The
  once-known `F_SETOWN`/SIGIO overflow-signal gap this line used to flag is moot: that whole
  mechanism was found unfit for purpose at H4 (a design gap, not just a threading-model gap) and
  removed outright in favor of RCB polling — see the H4 entry below for why.
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
  - **Not yet done**: booting a *real* Linux kernel end-to-end still needs more than the interrupt-
    injection engine alone — `baud-vcpu::boundary`'s arm-early-then-single-step engine is now wired
    into this crate's run loop (`Multiverse::inject_timer_tick`/`run_with_timer_ticks`, H4, see that
    entry below) and proven against a real, if hand-assembled, guest, but a real Linux kernel's
    `calibrate_delay()`/scheduler tick additionally needs a real periodic timer device (e.g. a
    modeled LAPIC timer or PIT channel 0 driving those same injection calls on a schedule), which
    does not exist yet; `hello-guest`'s hand-assembled payload deliberately has no such dependency
    and is not a placeholder for that work, just this crate's own H1 fixture.
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
- **`drive/h1.sh` rewritten to match the current KVM-era H1** (the prior version tested the
  pre-pivot ptrace-era "supervisor MVP" — see the H1 boot-flow entry above): runs `baud host probe`
  (asserts a non-rejected regime) then `cargo test -p baud-multiverse double_boot_memory_identical`,
  both against real `/dev/kvm`. Still passing on the real WSL2/KVM host.
- **H2 (deterministic double-run, todo.md §10) — real-hardware core done; `drive/h2.sh` rewritten
  to match.** The one genuinely new piece: `crates/baud-multiverse/tests/fixtures/tape-echo-guest/`
  (a second hand-assembled fixture, same build mechanics as `hello-guest` — see its own `BUILD.md`)
  is a 20-byte payload that reads 4 bytes from the tape device's real PIO `DATA` port (`0x0500`,
  one real `IN` per byte, through `tape_bus::TapeBus`/`DeviceBus`) and echoes each to COM1, then
  halts. Backs a new `linux::tests::all_input_is_tape_derived` in `baud-multiverse`, run for real
  against `/dev/kvm` for the first time: same tape twice → byte-identical console output; a tape
  with one changed byte → different output — closing test-matrix row 21 ("fake determinism") for
  real, the same way `hello-guest` closed rows 11/12 at H1. `cargo test -p baud-multiverse`: 51/51
  (was 50/50). The rest of H2's guarantees (`cpuid_leaves_are_fixed`, `work_clock_is_monotone_and_
  reproducible`, `no_unmodeled_exit_is_silent`) were already unit-tested hardware-independent from
  earlier iterations — `drive/h2.sh` now runs all of them plus `double_boot_memory_identical` and
  the new tape test together, end-to-end, against real `/dev/kvm` (ALL 6 CHECKS PASSED,
  regime=cooperative), replacing the fully stale pre-pivot version (which depended on `python3`,
  `examples/parser`, and the `/runs/fuzz` endpoint — none part of the current plan).
  **`cpuid_leaves_are_fixed`'s real-hardware readback half is now closed too, and closing it found
  a genuine, previously-undiscovered determinism bug (test-matrix row 20).** New
  `linux::tests::cpuid_leaves_are_fixed` (`crates/baud-multiverse/src/linux/mod.rs`): boots
  hello-guest twice, reads every served CPUID leaf back from each live vCPU via a real
  `KVM_GET_CPUID2` (not the synthetic `kvm_cpuid_entry2` payload the pure-function unit tests use),
  asserts the two full leaf sets are byte-identical, and asserts RDRAND/x2APIC (01H:ECX[30]/[21])
  and RDSEED/TSX-HLE/TSX-RTM (07H:EBX[18]/[4]/[11]) all read back cleared. First few runs
  intermittently failed (~1/7): leaf 01H:EBX bits[31:24] ("Initial APIC ID") disagreed across the
  two boots. Root cause: `KVM_GET_SUPPORTED_CPUID` fills that specific field from a real `cpuid`
  the kernel executes on whatever host logical CPU the ioctl call itself happens to run on — not a
  virtualized/synthesized value — so it drifts whenever the host scheduler migrates the calling
  thread between the two boots. `apply_determinism_mask` (`cpuid.rs`) never touched this field.
  Fixed by pinning leaf 01H:EBX[31:24] to `0` (`EBX_INITIAL_APIC_ID_MASK`), matching the topology
  leaves' existing "single vCPU is always APIC ID 0" convention (`pin_topology_sub_leaf`'s
  `EDX = 0`). New unit test `initial_apic_id_is_pinned_on_leaf_1` (hardware-independent) plus 30/30
  consecutive real-hardware reruns of `cpuid_leaves_are_fixed` confirm the fix (was flaky before).
  `drive/h2.sh` gained a new H2.7 step running this test. `cargo test -p baud-multiverse`: 63/63
  (was 61/61). `cargo build/test/clippy --workspace` and `drive/h0.sh`-`h5.sh` all re-verified
  passing with zero regressions.
- **H3 (randomness + time control, todo.md §10) — rdrand half done on real hardware, found a
  stronger-than-specified guarantee; `drive/h3.sh` rewritten to match.** New fixture
  `crates/baud-multiverse/tests/fixtures/rdrand-guest/` (hand-assembled x86-64 payload, same build
  mechanics as `hello-guest`/`tape-echo-guest` — see its own `BUILD.md`): writes a marker byte `'X'`
  to COM1, then executes `rdrand eax` directly ignoring the CPUID mask, then would echo 4 result
  bytes if reached. Booting it twice against real `/dev/kvm` revealed that masking RDRAND out of
  CPUID (`cpuid.rs`) doesn't just discourage a compliant guest — real VT-x hardware raises `#UD`
  immediately when `rdrand` executes and the guest's configured CPUID reports the feature absent
  (the Intel SDM's own gating clause, genuinely enforced, not just descriptive). This fixture has no
  IDT, so the `#UD` cascades to a triple fault, which `baud-vcpu`'s run loop already treats
  identically to a clean `Hlt` (`VcpuExit::Shutdown` -> `DispatchOutcome::Halted`). Both boots
  produce byte-identical single-marker-byte output — deterministic, not the "divergent double-run"
  the spec originally assumed. This is a **stronger** guarantee than specified: under cooperative
  regime, the raw random instruction is hardware-unreachable by *any* guest (compliant or
  adversarial), not merely caught after the fact. New test `rdrand_guest_is_flagged` added to
  `crates/baud-multiverse/src/linux/mod.rs` (`linux::tests`). `cargo test -p baud-multiverse`:
  52/52 (was 51/51). `specs/baud-multiverse.md` and todo.md §1-§13 were corrected this same
  iteration (separate pass) to state this real, stronger guarantee in place of the original
  divergence assumption.
  - **New CLI feature closes `regime_is_recorded_and_not_overclaimed`** (test-matrix row 1's second
    named test): `baud host probe` gained `--require <cooperative|enforced>`
    (`crates/baud-cli/src/cmds/host.rs`, new `RequiredRegime` enum + a pure `regime_satisfies`
    function). Comparison is entirely client-side against the existing `/host/probe` JSON response
    — no server-side change needed. Exits 1 with a clear message ("this host only supports regime
    '...', which does not meet the requested '--require ...' guarantee — refusing to report a
    stronger determinism guarantee than this host actually verified") when the probed regime
    doesn't meet the requirement; exits 0 when it does. New unit test
    `regime_is_recorded_and_not_overclaimed` in the same file (hardware-independent, pure function).
    Manually verified end-to-end on real hardware: `baud host probe --require enforced` on this
    cooperative-only host (no custom KVM module — `enforced_module_present()` is `false`) correctly
    exits 1 with the message; `--require cooperative` exits 0. `cargo test -p baud-cli
    regime_is_recorded_and_not_overclaimed`: 1/1 pass.
  - **`drive/h3.sh` rewritten** (was stale — validated the old ptrace-era "multi-guest cluster + net
    device" H3, not this document's real H3), mirroring the exact rewrite pattern `h1.sh`/`h2.sh`
    already established: H3.1 `baud host probe` reports a non-rejected regime, H3.2
    `rdrand_guest_is_flagged`, H3.3 `regime_is_recorded_and_not_overclaimed` (both the unit test and
    a live end-to-end CLI check of `--require enforced` failing correctly and `--require
    cooperative` passing). Passes end-to-end for real against `/dev/kvm` on this machine (ALL
    CHECKS PASSED, regime=cooperative). `drive/h0.sh`, `drive/h1.sh`, `drive/h2.sh` all
    re-verified passing with no regressions. `cargo build --workspace` / `cargo test --workspace` /
    `cargo clippy --workspace --all-targets` all green — zero regressions, zero new warnings
    anywhere.
  - **Found and fixed a genuine flaky-host bug during final re-verification**: `Host::probe()`'s
    `rcb_deterministic` check (`crates/baud-host/src/linux.rs`) — the H0-gate PMU-reproducibility
    check H3.1 depends on — was empirically flaky on this exact hardware, 2/30 (~7%) back-to-back
    `baud host probe` calls with zero other load coming back `regime: "rejected"` when the very next
    call reported `cooperative`. This broke `drive/h3.sh` H3.3 on a re-run (a transient rejection
    mid-script hit a different, still-correct, exit-1 path than the one H3.3 was asserting, unrelated
    to `--require` itself). Root cause: `measure_fixed_loop_branches()` uses an unpinned
    `PERF_COUNT_HW_BRANCH_INSTRUCTIONS` counter that the kernel occasionally multiplexes off the PMU
    mid-trial on this contended WSL2/nested-virt host, undercounting one trial — not real hardware
    nondeterminism. Fixed with two changes in the same file: (1) `.pinned(true)` on the
    `perf_event::Builder` (~25% -> ~7% flake rate alone), (2) `rcb_deterministic()` changed from
    "two trials must agree" to "three trials, accept if any two of three agree" (majority vote; still
    rejects a genuinely unstable CPU). Also fixed a stale module-doc comment claiming this module
    "has not yet been exercised on real KVM hardware" — it has, extensively, since H1. Verified: 40
    consecutive `baud host probe` calls, 0/40 false rejections (was 2/30). `cargo test -p baud-host`
    still 7/7. `cargo build/test/clippy --workspace` all green. `drive/h3.sh` re-run 3x back-to-back,
    0 failures (previously flaky on a re-run). Still a heuristic, not a proof: majority-of-3 hardens
    the pre-flight *check*, but doesn't add rigor to the spec's own named test
    `rcb_is_deterministic_on_this_cpu` (todo.md §3.7's H0 gate), which doesn't exist verbatim anywhere
    yet — only this `rcb_deterministic()` boolean does; a future iteration should revisit whether
    majority-of-3 is right once that named test is actually written, or whether pinned+isolated-core
    measurement removes the need for voting entirely.
  - **Not yet done**: the enforced-regime half of `rdrand_guest_is_flagged` (`Crash{detail:
    "rdrand"}`) still needs the custom out-of-tree KVM module (specs/baud-host.md §8, not built —
    `enforced_module_present()` hardcoded `false` in `crates/baud-host/src/linux.rs`), same gap as
    before. `regime_is_recorded_and_not_overclaimed`'s check is CLI-side only (comparing the JSON
    `baud host probe` already returns) — there is still no `RunManifest`-level enforcement (no code
    path yet compares a *run's actual* regime against a caller's requirement at run-start time;
    `baud-snapshot-store`'s `RunManifest.regime: String` remains purely archival, per that crate's
    own entry above). H3's other named guarantee area — the raw *timestamp* instruction (RDTSC)
    still needs a compliant guest under cooperative regime (RDTSC has no CPUID gate, doesn't
    self-#UD) — was not touched this iteration; H3's rdrand half is done, the RDTSC-compliance half
    of "randomness + time control" is unexplored territory for an adversarial-guest test (only
    compliant-guest work-clock tests exist so far, from H2).
- **H4 (interrupt at an exact instruction boundary, specs/baud-vcpu.md §5, todo.md §10) —
  `timer_tick_lands_at_identical_instruction` now passes for real against real KVM hardware;
  `drive/h4.sh` now exists and passes end-to-end (H4.1 host probe, H4.2 the named test).** This
  iteration picked up a previous session's uncommitted
  work-in-progress — the real in-memory flat GDT (`crates/baud-multiverse/src/layout.rs`'s
  `build_flat_gdt`/`GDT_ADDR`, `linux/pagetables.rs`'s `write_gdt`), `Multiverse::
  inject_timer_tick`/`run_with_timer_ticks`/`TimerTick` (`crates/baud-multiverse/src/linux/mod.rs`),
  `LinuxPmuStepper::with_baseline_rcb` (`crates/baud-vcpu/src/linux/pmu.rs`), and a new
  `tests/fixtures/timer-guest/` fixture (a real 64-bit IDT + ISR + busy loop, `payload.s`/
  `BUILD.md`) — none of which had ever been run to completion; the prior session's own notes
  described the test as hanging. It was not a hang: `strace -p -c` on the live process showed
  ~8,600 ioctl/sec (real work, not a stuck syscall), and `/proc/<pid>/task/*/stat` per-thread
  utime/stime confirmed the vCPU thread was actively burning CPU — `inject_at`'s single-step
  while-loop was closing a gap of tens of thousands of branches one `KVM_RUN` at a time, ~100,000
  real round-trips, because `run_until_exit` was returning tens of thousands of branches short of
  the real target. Running it for the first time surfaced three real, previously-undiscovered
  production bugs, one host limitation, one design gap, and one hardware-precision finding:
  - **Bug 1 — signal misattribution.** `LinuxPmuStepper` used to arm its branch counter's overflow
    via a process-wide `F_SETSIG`/SIGIO handler (`on_branch_overflow`, since fully removed) with no
    way to tell "this stepper's own counter fired" from "an earlier, already-superseded stepper's
    counter fired late." On this project's own nested-virtualized dev host (CLAUDE.md), a PMU
    overflow signal can arrive very late — sometimes only once the guest returns to userspace for
    an unrelated reason, well after the owning `LinuxPmuStepper` was already dropped and a new one
    armed for the next tick — which is the actual mechanism behind the "hang" above.
  - **Bug 2 — sticky `kvm_run.immediate_exit`.** The (now-removed) SIGIO handler wrote
    `kvm_run.immediate_exit = 1` to unblock a blocking `KVM_RUN`, but nothing ever cleared it back
    to `0`. Since this is a kernel-owned byte in the vCPU's persistent mmap'd `kvm_run` struct, not
    per-stepper state, once any overflow fired even once, every future `KVM_RUN` on that vCPU for
    the rest of the process's life — including a totally different, later, non-stepper run loop
    like `run_to_first_halt` — returned `-EINTR` instantly forever. This, not bug 1, was the actual
    multi-minute-to-indefinite hang; fixed by resetting `immediate_exit = 0` the moment the overflow
    was consumed (moot now that the SIGIO path is gone entirely, but the same stickiness class
    recurred in bug 3).
  - **Bug 3 — sticky `kvm_run.request_interrupt_window`.** Same class of bug, still present today:
    `LinuxPmuStepper::request_interrupt_window` sets `kvm_run.request_interrupt_window = 1` but
    nothing cleared it, so once the fallback "wait for an interrupt window" path was ever taken,
    every later `KVM_RUN` on a genuinely non-stepper run loop could spuriously exit with
    `KVM_EXIT_IRQ_WINDOW_OPEN`, which the generic dispatcher doesn't handle (hits the
    determinism-hole catch-all). Fixed in `run_until_irq_window` (`crates/baud-vcpu/src/linux/
    pmu.rs`) by clearing the flag the instant `ready_for_interrupt_injection` reports the window is
    actually open.
  - **Host limitation — `exclude_host(true)` is non-functional here.** The textbook "guest-filtered"
    branch count (specs/baud-vcpu.md §5's own assumption) was tried on both the free-running
    work-clock counter (`linux::mod::LinuxBranchCounter::new`) and the stepper's armed counter
    (`pmu::LinuxPmuStepper::arm_overflow`); with it set, the counter reads back `0` for the whole
    run. Root cause: perf's guest/host execution-mode discrimination needs the KVM module to
    register `perf_guest_cbs`, which this host does not do under nested virtualization. Reverted;
    documented in both call sites' doc comments as a real, tried-and-ruled-out host limitation, the
    same family as the already-documented PMI-in-guest-mode signal-unreliability finding above.
  - **Design gap — abandoned the SIGIO overflow signal entirely, not just its misattribution bug.**
    Even with bugs 1-3 fixed, the test still failed nondeterministically (the RCB landing point
    differed by tens to hundreds of branches between two boots of the identical image+tape) because
    the signal's own wall-clock-driven arrival timing was itself a nondeterminism source — it forced
    a real VM exit unrelated to the guest's deterministic instruction stream. Replaced with pure RCB
    polling after every real `KVM_RUN` exit (`LinuxPmuStepper::run_until_exit`, `pmu.rs`): the
    landing point is now a pure function of the guest's own deterministic execution trace, the only
    thing that can move `current_rcb()` past `poll_target`. This required fixing
    `tests/fixtures/timer-guest/payload.s` itself: its forced-trap interval (every 1000 branches,
    via a harmless `out 0x80, al`) was coarser than `boundary::MARGIN` (64), so the "early exit"
    always landed past the real target and silently skipped `inject_at`'s single-step phase every
    time; shrunk to every 16 branches and the fixture's `bzImage` regenerated via `build.py`.
  - **Hardware-precision finding (accepted, not fixed) — residual `rcb` read jitter.** Even after
    all of the above, two runs could still disagree by ±1-4 in reported `rcb` while `rip` (the
    actual injected-instruction landing point) was bit-identical every time and RAM hash/console
    output were also exactly identical — proving genuine `perf_event` branch-counter read-precision
    jitter on this host, not an execution/state divergence. `.pinned(true)` was applied to both
    counter builders (the same fix `crates/baud-host/src/linux.rs`'s `measure_fixed_loop_branches`
    already uses per the H3 entry above) — it did not eliminate the jitter but is still correct
    practice. The test was changed to assert `rip` exactly and `rcb` within a small, documented
    tolerance, `linux::tests::RCB_HARDWARE_JITTER_TOLERANCE = 8`, mirroring the precision-vs-
    determinism distinction the H3 entry's `rcb_deterministic()` majority-of-3 heuristic already
    established for this same host class ("still a heuristic, not a proof").
  - **Verification**: `cargo test -p baud-multiverse --lib linux::tests::
    timer_tick_lands_at_identical_instruction` run 20+ times back to back on real `/dev/kvm`
    hardware, 100% pass rate after the final fix (was: hung indefinitely before the
    `immediate_exit` fix, then failed ~50-100% of the time before the tolerance fix). Full
    `cargo test --workspace` green, 0 failures across every crate (`baud-multiverse` 54/54, was
    52/52). `cargo clippy --workspace --all-targets` — zero new warnings in any touched file
    (`pmu.rs`, `boundary.rs`, `baud-vcpu`'s `linux/mod.rs`, `baud-multiverse`'s `layout.rs`/
    `linux/mod.rs`/`linux/pagetables.rs`/`timesource.rs`); pre-existing warnings remain confined to
    unrelated files, same as previous iterations.
  - **Files touched**: `crates/baud-vcpu/src/linux/pmu.rs` (removed the whole SIGIO/signal-handler
    apparatus — `F_SETSIG`/`ensure_signal_handler_installed`/`arm_signal_delivery`/
    `on_branch_overflow` and its process-wide `AtomicBool` — kept `.pinned(true)`);
    `crates/baud-vcpu/src/boundary.rs` (no functional change; debug instrumentation added then
    removed while root-causing the hang); `crates/baud-multiverse/src/linux/mod.rs`
    (`inject_timer_tick`/`run_with_timer_ticks`/`TimerTick` now real and tested;
    `LinuxBranchCounter::new` gets `.pinned(true)`; the test itself with its new tolerance);
    `crates/baud-multiverse/src/layout.rs`/`linux/pagetables.rs` (the GDT wiring, inherited from the
    previous session, now actually exercised for the first time — see `tests/fixtures/timer-guest/
    BUILD.md` for why an interrupt gate's far transfer needs a real in-memory GDT even though
    ordinary execution never does); `crates/baud-multiverse/src/timesource.rs`
    (`WorkClock::current_rcb`, inherited from the previous session); `crates/baud-multiverse/
    tests/fixtures/timer-guest/payload.s` + regenerated `bzImage` + updated `BUILD.md`
    (forced-trap interval 1000→16).
  - **Not yet done**: `RCB_HARDWARE_JITTER_TOLERANCE = 8` is a documented heuristic, not a proof — a
    future iteration could investigate PEBS (`precise_ip`) or a different counting approach to
    eliminate the residual jitter, or accept it as a limitation of this host class alongside the
    already-documented PMI-in-guest-mode and `exclude_host` non-functionality findings. `drive/
    h4.sh` now exists (mirrors `drive/h1.sh`-`h3.sh`'s pattern: host probe, then the named test)
    and passes end-to-end on real `/dev/kvm`.
- H4 is done, including `drive/h4.sh` (`timer_tick_lands_at_identical_instruction` passing
  repeatably on real KVM hardware, see immediately above).
- **H5 (snapshot/branch/restore, todo.md §10) — first slice done: `snapshot_roundtrip_is_bit_
  identical` and `restore_refuses_mismatched_cpu` both pass on real KVM hardware; `drive/h5.sh`
  exists and passes end-to-end (H5.1-H5.3).** Prior
  iterations had built `Multiverse::snapshot`/`Multiverse::restore` and the underlying
  `baud-snapshot::linux::capture`/`restore` but never called either against a real, running guest
  (blocked on H1 existing at all). This iteration ran them for the first time
  (`crates/baud-multiverse/src/linux/mod.rs`'s `linux::tests::snapshot_roundtrip_is_bit_identical`):
  boot `timer-guest` (H4's fixture), deliver one tick, capture a `Universe` at that point (`K`),
  restore into a brand-new `Multiverse`, deliver a second tick and run to halt — the restored run's
  landed instruction (`rip`) and whole observation stream (console output, RAM hash) match a
  straight, never-snapshotted two-tick run exactly. `drive/h5.sh` (new, mirrors `h1.sh`-`h4.sh`'s
  pattern: host-probe gate, then the named test) passes end-to-end; `drive/h0.sh`-`h4.sh`
  re-verified with zero regressions; `cargo build/test/clippy --workspace` all green, zero new
  warnings in any touched file.
  - **Two real, previously-undiscovered production bugs found and fixed**, neither reachable
    without a real running guest to snapshot: (1) `baud_snapshot::linux::capture` unconditionally
    called `KVM_GET_LAPIC`, which fails `EINVAL` on every boot in this workspace — this VMM never
    calls `KVM_CREATE_IRQCHIP` (H4's arm-early-then-single-step engine injects interrupts directly
    via `KVM_INTERRUPT`, bypassing in-kernel LAPIC emulation entirely), so there is no in-kernel
    APIC state to capture in the first place; fixed by removing LAPIC from the capture set
    (`VcpuState`, `RestoreStep`, `restore_plan`) — any interrupt bookkeeping direct-injection needs
    is already covered by `KVM_GET_VCPU_EVENTS`. (2) `WorkClock::restore` only restored the
    virtual-TSC base, not the cumulative RCB value — a restored guest's branch counter is a
    brand-new `perf_event` fd (a process cannot resurrect another fd's already-elapsed hardware
    count) and silently restarted counting from zero, corrupting every `target_rcb`
    `inject_timer_tick` computes after a restore. Fixed by adding `WorkClock::rcb_offset` (added to
    every raw counter read) and `ClockState::rcb_anchor` (the cumulative RCB captured at snapshot
    time, threaded through `Multiverse::snapshot`/`restore`). Both fixes are also reflected in
    specs/baud-snapshot.md §3/§6/§10 (the spec's original capture-set table listed LAPIC and only a
    single-field "work-clock anchor," both now corrected to match this real-hardware finding).
  - **A real-hardware precision finding (accepted, not fixed)**: even after both bugs above,
    the *internal* `rcb` value at the restored run's second tick still disagrees with the straight
    run's by several hundred branches (confirmed via instrumentation to be one-time `perf_event`
    fd-creation/enable overhead — a cost a continuously-running counter never re-pays, since a
    straight run's later ticks reuse the same fd created once at boot). This does not reach the
    guest: `rip`, console output, and RAM hash are all exactly identical either way, the same
    "precision vs. determinism" distinction already established by H3's `rcb_deterministic`
    majority-of-3 heuristic and H4's `RCB_HARDWARE_JITTER_TOLERANCE`. `snapshot_roundtrip_is_bit_
    identical` therefore does not assert `rcb` equality across the restore boundary at all (only
    `rip` + the console/RAM observation stream, matching the spec's own pseudocode), with a doc
    comment explaining why — a documented design decision, not a gap.
  - **`restore_refuses_mismatched_cpu` now closed on real hardware.** New test
    `linux::tests::restore_refuses_mismatched_cpu` (`crates/baud-multiverse/src/linux/mod.rs`):
    boots `hello-guest`, captures a `Universe`, confirms restoring the real unmodified universe
    onto this exact host succeeds (positive control), then forges `universe.cpu_signature` (flips
    its low bit — indistinguishable from `restore`'s point of view from a genuine cross-model
    capture, since the field is opaque data the restore path only compares, never interprets) and
    asserts `Multiverse::restore` refuses with `RestoreError::Snapshot(CpuMismatch{captured,
    current})` reporting both the forged and this host's real signature, then that
    `template_active=true` lets the same mismatched restore proceed. No production code changed —
    `universe::model_matches`/`linux::restore`'s `CpuMismatch` check/`Multiverse::restore`'s
    `template_active` plumbing were already fully wired since H5's first slice; only the real-KVM
    exercise was missing. `cargo test -p baud-multiverse`: 57/57 (was 56/56). `drive/h5.sh` gained
    a new H5.3 step running this test; H5.1/H5.2 and `drive/h0.sh`-`h4.sh` re-verified with zero
    regressions. `cargo build/test/clippy --workspace` all green, zero new warnings in any touched
    file.
  - **`reset_cost_scales_with_write_set` now closed on real hardware — and closing it surfaced
    three real, previously-undiscovered production bugs in code that had only ever been
    type-checked, never run.** New test `linux::tests::reset_cost_scales_with_write_set`
    (`crates/baud-multiverse/src/linux/mod.rs`): boots `timer-guest` with a dirty ring requested,
    snapshots the pristine pre-run state as `base`, delivers two ticks and runs to halt (dirtying
    a handful of pages), calls `Multiverse::reset_dirty_pages(&base.ram)`, and asserts the
    returned page count is small and nonzero (a generous `<= 64` bound, documented as covering the
    ISR's stack pushes/pops *plus* a few page-table `ACCESSED`-bit updates from the guest's first
    address translations — a real, accepted, non-bug finding, not a leak) and far below total RAM
    (65536 pages), then that guest RAM is byte-identical to the pristine base again after reset.
    1. **`KVM_CAP_DIRTY_LOG_RING` cannot be negotiated once any vCPU already exists** (the kernel's
       own `kvm->created_vcpus` check, `EINVAL`) — the old `Multiverse::enable_dirty_ring(&mut
       self, entries)` API, documented as callable any time after `boot`, could therefore never
       actually succeed in this workspace, since `boot` always already has a vCPU by the time it
       returns; this test's first run failed immediately on `enable_cap`. Fixed by moving
       negotiation into `create_vm_vcpu_shell` itself, between `create_vm` and `create_vcpu` —
       `baud_snapshot::linux::DirtyRing::enable` (the old combined call) is now split into
       `negotiate_capability(vm, entries)` (pre-`create_vcpu`) and `open(vcpu, entries)`
       (post-`create_vcpu`, the mmap step); `Multiverse::boot`/`restore` gained a
       `dirty_ring_entries: Option<u32>` parameter threading this through (`enable_dirty_ring` is
       gone — there is no correct time to call it after construction, so the option moved to
       construction time). `boot_guest`/`restore_guest`/`create_vm_vcpu_shell` all updated to
       match (no external callers outside this file's own tests, confirmed by grep).
    2. **The ring mmap was `PROT_READ`-only, but `DirtyRing::collect` writes the `RESET` flag bit
       back into that same mapping** to mark harvested entries (the kernel's own
       harvest/act/confirm protocol requires in-place mutation, the same way e.g. QEMU maps this
       ring read-write) — a read-only mapping segfaulted (`SIGSEGV`) the instant `collect` was
       first called for real, caught via `gdb --batch -ex run -ex bt` (`gdb` was not previously
       installed on this dev machine; installed via `apt-get install gdb`) pointing at
       `core::ptr::write_volatile` inside `DirtyRing::collect`. Fixed: `libc::PROT_READ` →
       `libc::PROT_READ | libc::PROT_WRITE` in `DirtyRing::open`.
    3. **The guest-RAM memory slot was registered with `flags: 0`**, so KVM was never tracking
       dirty pages for it at all (neither the classic bitmap nor the ring mechanism logs a slot
       without `KVM_MEM_LOG_DIRTY_PAGES` — the ring changes *how* dirty pages are reported, not
       *whether* a slot opts in) — after fixing bugs 1-2 the test still got a hard `0` back from
       `reset_dirty_pages` despite RAM visibly having changed (`ram_hash` differed). Fixed:
       `allocate_and_register_guest_ram` gained a `log_dirty_pages: bool` parameter
       (`create_vm_vcpu_shell` passes `dirty_ring_entries.is_some()`), setting
       `KVM_MEM_LOG_DIRTY_PAGES` on the region only when a caller actually wants dirty tracking
       (the flag has a real write-protection cost callers that never reset should not pay).
    `cargo test -p baud-multiverse`: 58/58 (was 57/57). `drive/h5.sh` gained a new H5.4 step
    running this test; H5.1-H5.3 and `drive/h0.sh`-`h4.sh` re-verified with zero regressions.
    `cargo build/test/clippy --workspace` all green, zero new warnings in any touched file
    (confirmed `cargo clippy -p baud-multiverse -p baud-snapshot --all-targets` specifically).
  - **`thousand_branches_are_independent_and_deterministic` now closed on real hardware, via the
    spec's own documented small-N fallback, not the spec's literal `UFFDIO_CONTINUE` mechanism.**
    New `Multiverse::branch` (`crates/baud-multiverse/src/linux/mod.rs`) realizes
    specs/baud-snapshot.md §4's "`fork()` copy-on-write is the small-N fallback" as a full
    `Multiverse::restore` per branch rather than a literal `fork(2)` — a real architectural finding
    surfaced while implementing it: a raw OS `fork()` cannot safely reuse an already-open KVM
    `vm`/`vcpu` fd at all, independent of the threading-model hazard previously flagged here (the
    "one VMM thread + one vCPU thread" model is not even live yet — confirmed no `thread::spawn`
    anywhere in `baud-multiverse`/`baud-vcpu` — but the deeper reason fork() can't work is that a
    `VmFd` is tied to its *creating* process's `mm` at `KVM_CREATE_VM` time; a forked child sharing
    the parent's `vm` fd would still have guest-physical memory resolve through KVM's EPT against
    the *parent's* address space, not the child's own post-fork CoW pages, no matter what the two
    processes' host page tables look like). New test `linux::tests::thousand_branches_are_
    independent_and_deterministic` (`crates/baud-multiverse/src/linux/mod.rs`): captures a branch
    point immediately after boot (before the guest runs a single instruction) using `tape-echo-
    guest` (H2's fixture — reads 4 tape bytes, echoes to COM1, halts), forks 1000 branches from it
    each on a unique 4-byte tape suffix, and asserts every branch's output matches exactly its own
    suffix (a direct, stronger proof of "no branch perturbs another" than a pairwise comparison —
    any cross-branch memory bleed would show up as a mismatched byte). A sample of 8 branches is
    re-forked a second time from the same universe+suffix and proven byte-identical (console output
    + RAM hash), closing the spec pseudocode's `b.is_deterministic_double_run()` for a
    representative subset (full-N double-run wasn't worth 2x the real-hardware wall time, given
    every branch takes the same `restore` path `snapshot_roundtrip_is_bit_identical` already proved
    bit-identical). Real cost: ~213s for 1000+8 branches on this dev machine (~200ms/branch — each
    is a real `KVM_CREATE_VM`/vCPU/256MiB-guest-RAM-region lifecycle, dominated by the full RAM
    copy `restore_ram` does unconditionally). `drive/h5.sh` gained a new H5.5 step running this
    test. `cargo test -p baud-multiverse`: 59/59 (was 58/58). specs/baud-snapshot.md (§4/§10) and
    `crates/baud-snapshot/src/lib.rs`'s module doc updated to match.
  - **Not yet done**: the spec's literal `O(write-set)` memory-efficiency guarantee — real
    `UFFDIO_CONTINUE` CoW sharing — is still blocked on the same real architecture gap as before:
    guest RAM is a private anonymous mapping today, but `UFFDIO_CONTINUE` needs a shared
    (memfd/hugetlbfs) backing, an architecture change to `baud-multiverse` no single crate can
    absorb alone. `Multiverse::branch`'s current cost is `O(total RAM)` per branch (a real 256MiB
    copy each), not `O(write-set)` — correct and fully independent, just not yet "cheap" the way
    the spec's own framing promises.
  - **`shell_into_universe_resumes` now closed on real hardware, at the crate level — the
    `baud shell-into` CLI/server verb the test's name references is not.** New: `Console::
    enqueue_input` (`crates/baud-multiverse/src/console.rs`, wraps `vm_superio::Serial::
    enqueue_raw_bytes` into the UART's RX FIFO — the console's write side was already generic
    enough via `vm_superio::Serial<T, EV, W: Write>`, only `Vec<u8>` was ever chosen as `W`, so no
    Console API/PTY-dependency change was actually needed, contrary to a prior iteration's
    estimate); `Multiverse::{console_output, enqueue_console_input, step_exit,
    run_until_console_len}` (`crates/baud-multiverse/src/linux/mod.rs`) — the building blocks an
    interactive session needs instead of `run_to_first_halt`, which by design stops at `Hlt`. New
    fixture `tests/fixtures/shell-guest/` (prints a `$ ` prompt, polls COM1 LSR for input, echoes
    it, re-prompts on `\r`, never halts — the first fixture in this workspace to exercise the
    UART's *receive* side against real hardware; polls LSR rather than blocking on IRQ4 since this
    workspace has no in-kernel LAPIC, same reason H4's interrupt engine injects directly via
    `KVM_INTERRUPT`). New test `linux::tests::shell_into_universe_resumes`
    (`crates/baud-multiverse/src/linux/mod.rs`): captures a `Universe` right at the prompt,
    restores it into a brand-new `Multiverse`, confirms the restored console output matches the
    captured tail exactly, then feeds it `"hi\r"` and confirms it echoes and re-prompts
    byte-identically to an equivalent straight run that never snapshotted at all. `drive/h5.sh`
    gained a new H5.6 step. `cargo test -p baud-multiverse`: 61/61 (was 59/59).
    specs/baud-snapshot.md gained a new §5.1 documenting both the closure and the gap below.
    - **Real-hardware finding and fix, found by this test**: `Multiverse::snapshot` could capture
      a stale, not-yet-retired `RIP` when called immediately after a plain PIO exit. None of this
      crate's ports are in-kernel-emulated, so every `IN`/`OUT` round-trips to userspace; KVM
      defers that instruction's retirement (including the `RIP` advance) to the *next* `KVM_RUN`
      call, not the exit that reported it. Every snapshot point before this test existed either at
      a fresh boot (zero exits behind it: `thousand_branches`/`restore_refuses_mismatched_cpu`) or
      right after `inject_timer_tick`'s single-step confirmation loop (already calls `KVM_RUN`
      enough times to retire whatever was pending: `snapshot_roundtrip_is_bit_identical`/
      `reset_cost_scales_with_write_set`) — this is the first snapshot taken right after a plain,
      uninterrupted `step_exit()`, and hit it for real: a universe restored from the stale capture
      silently re-executed the just-completed instruction (an observable duplicate byte for `OUT`,
      confirmed by step-by-step debug tracing before the fix — `RIP` was identical pre-capture and
      post-restore, `0x200209` both times, yet the very first post-restore step produced an extra
      byte). Fixed with `Multiverse::flush_pending_pio_completion`: the standard `kvm_run.
      immediate_exit` technique (set it, call `KVM_RUN` once — it retires the pending completion
      at entry and returns `-EINTR` immediately with no new guest instruction executed — clear it
      again), called at the top of `snapshot` before any `KVM_GET_*` read. This is a real, general
      correctness gap in `Multiverse::snapshot`, not specific to this fixture — any future caller
      that snapshots right after a plain (non-interrupt-injection) PIO exit would have hit it too.
    - **Not yet done**: the actual `baud shell-into <universe>` CLI/server surface. `baud-server`
      has never called into `linux::Multiverse` at all (every existing route still imports the old
      pre-pivot `Multiverse` in `baud-multiverse::lib.rs`, confirmed by grep) — this would be its
      first route to do so. A real interactive terminal session needs bidirectional-streaming
      server infrastructure this codebase does not have yet (no WebSocket route exists anywhere;
      `routes/stream.rs`'s "tail" endpoint is plain request/response, not live) — CLAUDE.md's own
      "CLI: thin client, one subcommand = one server call" rule means this can't be a CLI-local
      shortcut either. Also needs a `SnapshotStore`-backed universe lookup by ID (`get_universe`
      already exists in `baud-snapshot-store`, but nothing deserializes its bytes back into a
      `baud_snapshot::Universe` today). An `EventFd`-backed `Trigger` (replacing `NoIrqTrigger`)
      for a guest that blocks on IRQ4 instead of polling LSR is also open, tracked in `console.rs`'s
      module doc. **H6 is now closed (see the H6 entry below, added a later iteration); the rest
      of the M-series (rebuilding `baud-driver`/`baud-server`/snapshot-store wiring on this KVM
      core) remains not yet started.**
- **Learned this iteration**: the dev environment is now genuinely WSL2 Ubuntu (not the Windows-side
  git-bash environment several older entries below reference) — `python3` (3.14.4) is present at
  `/usr/bin/python3`, unlike the prior "known gap" noted below. `drive/m1.sh` was spot-checked and
  still does not pass here, but for a different, not-yet-diagnosed reason (the server did not come
  up within the script's health-check loop; not investigated further as it is unrelated to this
  iteration's H2 work) — the `python3`-missing gap specifically is resolved by the new environment,
  but M1 itself needs a fresh look, not assumed fixed. **Root-caused and fixed in a later iteration**
  (see the "`drive/m1.sh` was fundamentally broken" entry at the very end of this section): the
  health-check-loop failure was never a `python3`/environment issue at all — `drive/m1.sh` alone
  among every `drive/h*.sh`/`drive/m*.sh` script started the server with `cargo run -q --bin
  baud-server &` (real cold-start cost ~15.7s, timed directly) while budgeting only `20 * sleep 0.2 =
  4s` in its own health-check loop before giving up, so it failed on a timing race every single run,
  4-5x too early, regardless of `python3` or any other environmental factor.
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
  `drive/full-demo.sh` for the same pattern. **Superseded by a later iteration**: this `python3`
  gap was specific to an older, since-replaced dev environment (the current WSL2 host has
  `python3` at `/usr/bin/python3`, per the "Learned this iteration" entry above), and separately,
  `drive/m1.sh`'s actual failure on the current host turned out to be the unrelated `cargo run`
  cold-start/health-check-timeout race documented and fixed in the "`drive/m1.sh` was fundamentally
  broken" entry at the very end of this section — `drive/m1.sh` now passes end-to-end on this
  machine, so neither the `python3` parsing question nor a `jq`/shell rewrite is blocking it
  today (though auditing `drive/m*.sh`/`drive/full-demo.sh` for the same JSON-parsing pattern is
  still worth doing if a `python3`-less host is ever targeted again).
- **This iteration picked "fix the broken M-series drive scripts" as its task**, after an
  investigation agent confirmed `vm_creation_refuses_multiple_vcpus` and
  `capacity_refuses_sibling_split` (todo.md §12's test matrix) were already implemented and
  unit-tested (pure logic only — no real-KVM exercise needed for either), and re-confirmed
  `crates/baud-server` still has zero WebSocket/streaming infrastructure, so `baud shell-into` still
  has no server-side surface (already documented above in the H5 `shell_into_universe_resumes`
  entry's "Not yet done" — no change needed there). Two real bugs were found and fixed, both in
  `drive/*.sh` shell scripts, not Rust code:
  - **`drive/m1.sh` was fundamentally broken and had never actually passed as it was written.**
    Root cause: unlike every other `drive/h*.sh`/`drive/m*.sh` script — which all `cargo build` up
    front and then exec the pre-built `target/debug/baud-server` binary directly (sub-1s start) —
    `drive/m1.sh` alone started the server with `cargo run -q --bin baud-server &` (a cold-start
    costing ~15.7s, confirmed by direct timing) while its own health-check loop only budgeted
    `20 * sleep 0.2 = 4s` before giving up, so it failed with "baud-server did not start" on every
    run, 4-5x too early. This finally root-causes the previously-vague "Learned this iteration"/
    "Known gap" entries above (old text: "the server did not come up within the script's health-
    check loop; not investigated further") — both have been corrected in place rather than left as
    an open mystery. Fixed: `drive/m1.sh` now defines `SCRIPT_DIR`/`REPO_ROOT` (matching
    `drive/m0.sh`'s existing pattern), sets `BAUD="$REPO_ROOT/target/debug/baud"` and
    `BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"`, and execs `"$BAUD_SERVER_BIN"` directly
    instead of `cargo run -q --bin baud-server`. Verified: `bash drive/m1.sh` now passes end-to-end
    twice in a row (M1.1 through M1.8 all PASS, "M1 milestone: ALL CHECKS PASSED").
  - **`drive/full-demo.sh` (the M9 "full system demonstration" chaining M0-M8, `FD.1`-`FD.10`)
    silently aborted after step FD.1c ("baud keys show") on every run, in ~6.6s, exit 1, no error
    message.** Root cause: `set -euo pipefail` is active, and FD.1d assigned
    `DOCTOR=$(BAUD_SERVER=... "$BAUD" doctor --json 2>&1)` with no `|| true` guard — `baud doctor
    --json` itself exits 1 whenever an optional local tool (sops/age) isn't installed, which is true
    on this dev machine (confirmed: its own JSON output reports `age.ok=false`, `sops.ok=false` —
    a real, expected environmental fact, not a bug). Under `set -e`, that nonzero exit from inside a
    bare command-substitution assignment killed the whole script immediately and silently (no
    `fail()` message, since the script died before the `[[ -n "$DOCTOR" ]]` check ever ran).
    `drive/m0.sh` already knew about and handled this exact same command's nonzero exit
    (`"$BAUD" doctor --json || true`, with a comment noting "may fail if sops/age not installed") —
    `drive/full-demo.sh` was simply missing that same guard. Fixed by adding the identical `|| true`
    to `drive/full-demo.sh`'s `DOCTOR=$(...)` line, with an explanatory comment. Verified:
    `bash drive/full-demo.sh` now runs to completion, "Checks passed: 32 / 32", "ALL 32 CHECKS
    PASSED" — the first time in this project's history (as far as this log shows) that
    `drive/full-demo.sh` has been confirmed passing end-to-end on real hardware.
  - Both fixes are shell-script-only, so no new Rust unit tests were added (nothing to unit-test in
    bash timing/guard logic), but full re-verification is completely green: `cargo build
    --workspace`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` all pass
    with zero regressions (only pre-existing warnings in unrelated files: `baud-tracing`'s
    deprecated `aya::Bpf`, and test-only lints in `baud-proto`/`baud-secret`/`baud-driver`/
    `baud-journal`/`baud-stream`). Every one of the 16 `drive/*.sh` scripts now passes end-to-end on
    this real `/dev/kvm` host in one sitting: `drive/h0.sh` through `drive/h5.sh`, `drive/m0.sh`
    through `drive/m8.sh`, and `drive/full-demo.sh` — the first time this project's full
    drive-script suite has been all-green together.
- **H6 (multi-VM fleet, todo.md §10) — CLOSED for real on real KVM hardware; `drive/h6.sh` now
  exists and passes end-to-end.** Prior iterations had built `baud-host::Host::place` (pure core
  arithmetic over a real `/sys` topology read) and `baud_vcpu::linux::pin_thread_to_core` (a real
  `sched_setaffinity` wrapper) but nothing ever called either together, and no code spawned more
  than one `Multiverse` at once — confirmed by grep before starting (`pin_thread_to_core` had zero
  call sites anywhere in the workspace). This iteration added `baud_multiverse::linux::run_fleet`
  (`crates/baud-multiverse/src/linux/mod.rs`): given a probed `Host` and a kernel image, it calls
  `Host::place(n)`, spawns one `std::thread::scope`-scoped thread per assigned physical core, pins
  each thread to its core via `pin_thread_to_core` (that function's first real call site), boots a
  real `Multiverse` per thread with its own tape, and returns each VM's `HaltOutcome` plus its core
  id and elapsed time. New test `linux::tests::fleet_of_vms_run_in_parallel_without_interference`
  closes all three of H6's milestone bullets in one real-hardware test: (1) `capacity_refuses_
  sibling_split` re-exercised against this host's *real* probed topology (`baud-host`'s own unit
  test only ever used a synthetic fake topology) — placing one VM over real capacity is refused,
  and a full-capacity placement never splits an SMT sibling pair; (2) no cross-VM interference —
  `tape-echo-guest` (H2's fixture) is booted once per VM with a unique 4-byte tape suffix, and every
  VM's console output is asserted to match exactly its own suffix, the same construction H5's
  `thousand_branches_are_independent_and_deterministic` uses for branches, here applied across
  genuinely concurrent OS threads instead of sequential `restore` calls; (3) aggregate throughput —
  a single-VM serial baseline is timed, then the N-VM fleet is timed running concurrently, and the
  fleet's wall time must stay under 85% of the naively-extrapolated N-times-serial estimate (real
  measured margin on this dev machine, n=3: ~160-210ms parallel vs. a ~260-310ms threshold, stable
  across 6+ consecutive runs — comfortably real, not a coin-flip). This host reports `capacity=3`,
  so `n = host.capacity().clamp(1, 4)` sizes the fleet to exactly this machine's real placement
  limit rather than a hardcoded guess. `cargo test -p baud-multiverse`: 64/64 (was 63/63). New
  `drive/h6.sh` mirrors `h0.sh`-`h5.sh`'s exact pattern (host-probe gate, then the named test) and
  passes end-to-end; `drive/h0.sh`-`h5.sh` and every `drive/m*.sh`/`full-demo.sh` re-verified with
  zero regressions; `cargo build/test/clippy --workspace` all green, zero new warnings in any
  touched file (`HaltOutcome` gained `#[derive(Debug)]` so `FleetVmResult` could derive it too — the
  only change outside the new `run_fleet`/`FleetVmResult`/`FleetError` code and the new test).
  - **Not yet done**: this closes H6 as todo.md §10 literally specifies it (many single-vCPU VMs
    pinned across cores, running in parallel on one host, with no cross-VM interference and real
    aggregate throughput) — it does not add NUMA-local memory placement (specs/baud-host.md §5
    names it, but this dev machine is single-socket so there is nothing to exercise it against), and
    it does not wire any real exploration/scheduling logic across the fleet's VMs (that is
    `baud-driver`'s job, still part of the not-yet-started M-series rebuild). `run_fleet` always
    boots fresh (no `restore`-based fleet-of-branches variant yet) — a natural follow-up once
    `baud-driver`'s snapshot-tree exploration exists to actually want one.
- **M-series rebuild — first brick laid: `baud-server` now has its first route that calls into the
  real, H0-H6-proven KVM core instead of the pre-pivot `Multiverse`.** With H0-H6 all closed,
  this iteration picked up the M-series ("rebuild server/CLI/driver/store/stream on this core",
  todo.md §10) — before touching anything, confirmed by grep that it was still true: every
  existing `baud-server` route (`/runs`, `/verify/determinism`, `/replay/:id`, etc.) only ever
  imports the old pre-pivot `Multiverse` from `crates/baud-multiverse/src/lib.rs`; nothing
  anywhere in `baud-server` called `baud_multiverse::linux::Multiverse` at all. New route
  `POST /run/kvm` (`crates/baud-server/src/routes/run_kvm.rs`, new file, `#[cfg(target_os =
  "linux")]`-gated in `routes/mod.rs` and via a new `add_run_kvm_route` helper in `main.rs`'s
  `build_router` — a `#[cfg(not(target_os = "linux"))]` no-op variant keeps the workspace
  buildable on a hypothetical non-Linux host, though CLAUDE.md confirms this workspace only ever
  builds/runs on real Linux+KVM) takes `{kernel_path, cmdline (default "console=ttyS0"),
  tape_hex (default empty)}`, calls `Multiverse::boot(...)` + `.run_to_first_halt()` inside
  `tokio::task::spawn_blocking` (real ioctls block — the same pattern `routes/host.rs::probe`
  already established), and returns `{ok, console_output_hex, ram_hash}` or `{error}`. New CLI
  surface `baud run kvm --kernel <path> [--cmdline] [--tape-hex]`
  (`crates/baud-cli/src/cmds/run.rs`, new `RunAction::Kvm` variant) posts to it and exits 1 on
  `{error}`, matching every other command's convention. New tests in `run_kvm.rs`:
  `run_kvm_boot_is_deterministic` (boots `tests/fixtures/hello-guest/bzImage` twice through the
  route's own `boot_and_run` — the exact function the HTTP handler calls, minus only axum/JSON
  plumbing — asserting identical console output + RAM hash, the server-level analogue of
  `baud-multiverse`'s own `double_boot_memory_identical`) and `hex_roundtrip`; `cargo test -p
  baud-server run_kvm`: 2/2. Manually verified end-to-end against a live `baud-server` + `baud`
  binary too, not just unit tests: `baud run kvm --kernel <hello-guest bzImage> --json` returned
  `console_output_hex` decoding to the exact `BAUD_HELLO_GUEST\n` marker and a `blake3:...`
  `ram_hash` with `ok:true`; a nonexistent kernel path returned a clean `{"error": "boot error:
  failed to open kernel image ... No such file or directory"}` and exit 1. Full re-verification,
  zero regressions: `cargo build/test/clippy --workspace` all green (one new lint on this file,
  `manual_is_multiple_of`, fixed inline; every other reported warning is pre-existing in
  unrelated files, confirmed via targeted `cargo clippy -p baud-server -p baud-cli
  --all-targets`); all 16 `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m8.sh`,
  `full-demo.sh`) re-run individually and still pass end-to-end on real `/dev/kvm`, including
  h5's ~230s 1000-branch test and h6's fleet test.
  - **Not yet done**: this is a raw boot-and-run-to-halt primitive only, the first brick, not the
    M-series rebuild itself — no snapshot/branch/rewind/shell-into surface on this route, no
    `SnapshotStore` wiring (`put_universe`/`get_universe`/`nearest`/`reconstruct` are still never
    called from `baud-server`), no tape-tree exploration or strategy/tactics (`baud-driver`'s
    snapshot-tree exploration is untouched), no framebuffer stream. The pre-pivot `/runs`,
    `/verify/determinism`, `/replay/:id` routes are completely unchanged and still import the old
    `Multiverse` — `/run/kvm` is additive, sitting alongside them, not a replacement. Natural next
    steps: (1) accept an already-captured `Universe` (from `SnapshotStore`) as an alternative to
    `kernel_path` so a run can resume instead of always cold-booting; (2) a `POST /branch` route
    wrapping `Multiverse::branch` for real snapshot-tree exploration; (3) wiring `baud-driver`'s
    tape generation into this route instead of a caller-supplied fixed `tape_hex`. The M-series is
    no longer "not started" but the bulk of it (driver/store/stream wiring) remains open.
- **M-series — second brick: `baud-server` gained its first real snapshot-tree-exploration route,
  `POST /run/kvm/branch`, closing "Natural next steps" bullet (2) from the `/run/kvm` entry above.**
  Before starting, two Sonnet explore agents confirmed the actual gap: `baud-server` had zero
  dependency on/usage of `baud-snapshot` or `baud-snapshot-store` (confirmed by grep — no
  `SnapshotStore`/`put_universe`/`get_universe`/`Universe` reference anywhere in
  `crates/baud-server/src`), and `baud_snapshot::Universe` has no `Serialize`/`Deserialize` impl
  anywhere in the codebase (no `serde` dep in `baud-snapshot/Cargo.toml` at all) — so wiring bullet
  (1) ("resume from a `SnapshotStore`-persisted `Universe`") is blocked on writing that serializer
  first, a real prerequisite, not yet done. Bullet (2) (a `/branch` route wrapping
  `Multiverse::branch`) has no such blocker: `Multiverse::snapshot`/`Multiverse::branch` are already
  proven correct by `thousand_branches_are_independent_and_deterministic` (H5) and only need a
  `Universe` that lives for the duration of one request, never crossing a request boundary — no
  `SnapshotStore` involvement required for a same-request fork-and-score primitive.
  - New handler `crates/baud-server/src/routes/run_kvm.rs::branch` (`POST /run/kvm/branch`,
    registered in `main.rs`'s `add_run_kvm_route` alongside the existing `/run/kvm`, same
    Linux-only cfg gate): takes `{kernel_path, cmdline, branch_tapes_hex: Vec<String>}`, boots the
    kernel once with an empty tape, calls `Multiverse::snapshot` immediately after boot (before any
    guest instruction runs — the same branch-point convention
    `thousand_branches_are_independent_and_deterministic` established) to capture the shared
    `Universe`, then calls `Multiverse::branch(&universe, suffix, WORK_CLOCK_K, None)` once per
    `branch_tapes_hex` entry and runs each to its first halt, returning
    `{ok, branches: [{console_output_hex, ram_hash}, ...]}` or `{error}`. Capped at
    `MAX_BRANCHES_PER_REQUEST = 256` (real per-branch cost is a full `KVM_CREATE_VM`/vCPU/RAM
    lifecycle, ~200ms on this dev host per H5's own measurement — an unbounded list turns one HTTP
    request into an arbitrarily long blocking call); empty/invalid-hex `branch_tapes_hex` are
    rejected with a clear `{error}`, never a false pass. `crates/baud-server/Cargo.toml` gained a
    `baud-snapshot` path dependency (needed for `PageStore::new()`, the interning store `snapshot`
    requires — no other part of that crate is touched, `Universe` itself is only ever passed by
    reference within the one blocking closure, never serialized).
  - New CLI surface `baud run kvm-branch --kernel <path> [--cmdline] --branch-tape-hex <hex>...`
    (`crates/baud-cli/src/cmds/run.rs`, new `RunAction::KvmBranch` variant, `branch_tapes_hex:
    Vec<String>` — clap derive's default repeated-flag collection, the same pattern already used
    elsewhere in this crate) — posts straight through to `/run/kvm/branch`, exits 1 on `{error}`,
    matching `RunAction::Kvm`'s existing convention exactly.
  - New tests in `run_kvm.rs`: `run_kvm_branch_produces_independent_and_deterministic_branches`
    (boots `tape-echo-guest` — H2's fixture — snapshots, forks 6 branches on unique 4-byte suffixes
    via this route's own `boot_snapshot_and_branch` — the exact function the HTTP handler calls,
    minus only axum/JSON plumbing — asserts each branch's console output equals exactly its own
    suffix, then re-runs the whole thing a second time and asserts the two full result sets are
    byte-identical, the server-level analogue of H5's `thousand_branches_are_independent_and_
    deterministic`). `cargo test -p baud-server`: 3/3 (was 2/2), all passing against real
    `/dev/kvm`.
  - **Manually verified end-to-end against a live server, not just unit tests**: `baud run
    kvm-branch --kernel <tape-echo-guest bzImage> --branch-tape-hex 11223344 --branch-tape-hex
    aabbccdd --branch-tape-hex 00010203 --json` returned three branches, each `console_output_hex`
    decoding to exactly its own suffix, all three sharing one `ram_hash` (expected — this fixture's
    tape bytes only ever pass through registers/UART, never touch guest RAM, so RAM state is
    identical regardless of which 4 bytes were echoed). Direct `curl` against the route confirmed
    both validation paths: an empty `branch_tapes_hex` and a non-hex entry both return a clean
    `{error}`, never a panic or false pass.
  - **Verification**: `cargo build --workspace` clean (no new warnings). `cargo test --workspace`
    100% green across every crate (`baud-multiverse` still 64/64, `baud-server` 3/3, everything
    else unchanged — zero regressions). `cargo clippy --workspace --all-targets` — zero new
    warnings in any touched file (`run_kvm.rs`, `main.rs`, `cmds/run.rs`, both `Cargo.toml`s),
    confirmed via targeted `cargo clippy -p baud-server -p baud-cli --all-targets`; every reported
    warning remains pre-existing in unrelated files. All 16 `drive/*.sh` scripts (`h0.sh`-`h6.sh`,
    `m0.sh`-`m8.sh`, `full-demo.sh`) re-run individually end-to-end on real `/dev/kvm`, zero
    regressions, including h5's ~220s 1000-branch stress test and full-demo's "32/32 CHECKS
    PASSED".
  - **Bullet (1) is now closed — see the "M-series — third brick" entry directly below.** Bullet
    (3) (`baud-driver`'s tape generation feeding branch scoring) remains open: confirmed by a
    second explore agent to be fully hardware-independent already and structurally ready to wire
    in — `Driver::draw_bits(n)` already produces exactly the byte stream `Multiverse::boot`/
    `branch`'s `tape` argument wants, and `baud-server`'s existing `fuzz.rs` already demonstrates
    the `Driver`-drives-a-generation-loop shape for two non-KVM simulated workloads — parser and
    raftlet — just never yet pointed at `Multiverse`. Natural next M-series increment.
- **M-series — third brick: `Universe <-> bytes` serialization built, and `baud-server` now
  persists/resumes real branch-point universes across requests, closing bullet (1) above** ("a
  real prerequisite for any `SnapshotStore`-backed resume/persist route").
  - New `crates/baud-snapshot/src/wire.rs` (`baud-snapshot` gained `serde`+`ciborium` deps,
    workspace-pinned): `Universe::to_body()`/`Universe::ram_pages()` project a captured `Universe`
    into a CBOR-serializable `UniverseBody` — `ram` becomes page **hashes only**
    (`[[u8;32]; N]`), never inline bytes, matching specs/baud-snapshot-store.md §3's "split into
    content-addressed pages" — plus the actual page bytes for a caller's own store;
    `universe_from_body(body, page_store, fetch_page)` reverses it, re-interning every fetched
    page through a `PageStore` (so a reconstructed universe keeps the same content-addressed
    sharing a freshly captured one has) and rejecting a fetched page whose content doesn't hash to
    the address the body claims. `MsrWrite`/`VcpuState`/`ClockState`/`DeviceState` gained
    `Serialize`/`Deserialize` derives (every field was already a plain value type — no projection
    needed there); `PageHash` gained `to_bytes`/`from_bytes`. 31/31 `baud-snapshot` tests (was
    23/23), fully hardware-independent (no KVM/perf involved, just serde + blake3 comparison).
  - `crates/baud-keys` gained `generate_identity_file()`/`parse_public_key()` (refactored out of
    the existing `age_public_key()`): a caller can now bootstrap and persist its own self-contained
    age identity with no external `sops`/`age` binary and no pre-configured
    `$SOPS_AGE_KEY_FILE` — needed because this dev host has neither installed (`baud doctor --json`
    reports `age.ok=false`/`sops.ok=false`) and requiring `baud keys init` before `baud-server`
    could even boot would make every persist/resume call depend on unrelated external setup.
    12/12 `baud-keys` tests (was 9/9).
  - `baud-server`'s `AppState` gained `snapshot_store: Arc<SnapshotStore>` (new
    `baud-snapshot-store` dependency), opened at startup against `$BAUD_SNAPSHOT_STORE` (default
    `baud-snapshots`, mirroring `BAUD_DB`'s own env-override convention; gitignored, same treatment
    as `*.sqlite`) — self-bootstraps `.age-identity.txt` under that root on first run via the new
    `baud-keys` helpers, stable across restarts of the same root.
  - `POST /run/kvm/branch` gained an optional `persist_run_id`: when set, the shared branch-point
    universe's distinct RAM pages (`SnapshotStore::put_page`) plus its CBOR-encoded body
    (`SnapshotStore::put_universe`, `parent: None`, fresh root node) are persisted before forking,
    and the response gains `persisted: {run_id, node_id}`. New `POST /run/kvm/resume` (`{run_id,
    node_id, branch_tapes_hex}`) reconstructs the `Universe` from the store
    (`get_universe`+`decode_universe_body`+`get_page`-per-hash+`universe_from_body`) and forks
    fresh `Multiverse::branch` continuations from it — **no kernel image, no re-boot required at
    all**. New CLI: `baud run kvm-branch --persist-run-id <id>`, `baud run kvm-resume --run-id
    --node-id --branch-tape-hex...`. New tests in `run_kvm.rs`:
    `persisted_universe_resumes_and_branches_without_reboot` (persist via `boot_snapshot_and_branch`,
    resume via `resume_and_branch` in a separate call against a temp `SnapshotStore`, assert
    byte-identical outcomes to branching directly from the in-memory universe) and
    `resume_rejects_unknown_run`. `cargo test -p baud-server run_kvm`: 5/5 (was 3/3), ~4.5s.
  - **Manually verified end-to-end against a live server, not just unit tests — and this manual
    check found and fixed a real bug the automated tests did not force.** `baud run kvm-branch
    --persist-run-id` then a separate-process `baud run kvm-resume` genuinely hung (minutes, not
    seconds) the first two times this was tried live. Root-caused with `gdb -p <pid> -batch -ex
    'thread apply all bt'` against the live, actually-stuck process (not a deadlock — a real
    worker thread, caught mid-`age::primitives::stream::Stream::decrypt_chunk`, called from
    `SnapshotStore::get_page` via `resume_and_branch`'s `fetch_page` closure): `Universe::
    ram_pages()`/`universe_from_body` deliberately iterate one entry per RAM page **slot** — 65536
    for this workspace's fixed 256 MiB `GUEST_RAM_SIZE`, not one per distinct content (`wire.rs`'s
    own module doc) — but neither `persist_universe`'s `put_page` loop nor `resume_and_branch`'s
    `fetch_page` closure deduplicated by hash, so a guest whose RAM is mostly one shared zero page
    (the common case for a boot-time snapshot before any instruction runs) still paid up to 65536
    real disk-read+age-decrypt round trips per resume. Fixed with per-call memoization: a
    `HashSet<PageHash>` short-circuits repeat `put_page` calls in `persist_universe`, a
    `HashMap<PageHash, Vec<u8>>` cache short-circuits repeat `get_page` calls in
    `resume_and_branch`. Effect measured directly: `cargo test -p baud-server run_kvm` dropped from
    83-90s to 4.52s; the live manual repro dropped from an unbounded multi-minute hang to
    `kvm-branch --persist-run-id` in 1.4s and `kvm-resume` in 1.6s, byte-identical output, no
    kernel re-boot. A bad `node_id` still returns a clean `{"error": ...}`, exit 1, confirmed live.
  - **Verification**: `cargo build --workspace` clean. `cargo test --workspace` 100% green (0
    failures across every crate — one `baud-multiverse::linux::tests::
    fleet_of_vms_run_in_parallel_without_interference` failure seen mid-iteration during a
    heavily-loaded concurrent run was confirmed transient by an immediate isolated re-run, unrelated
    to this change, H6's own real-hardware throughput-margin test). `cargo clippy --workspace
    --all-targets` — zero new warnings in any touched file (confirmed via targeted `-p baud-server
    -p baud-cli -p baud-snapshot -p baud-snapshot-store -p baud-keys`; fixed one new
    `type_complexity` lint on `boot_snapshot_and_branch`'s return type via `BranchOutcome`/
    `PersistedRef` type aliases). All 16 `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m8.sh`,
    `full-demo.sh`) re-run individually end-to-end on real `/dev/kvm`, zero regressions,
    `full-demo.sh` "32/32 CHECKS PASSED".
  - **Not yet done**: `Sha::from_hex(&hash.to_hex())` in `resume_and_branch`'s fetch closure is a
    correct but wasteful bridge between `baud_snapshot::PageHash` and `baud_snapshot_store::Sha`
    (both blake3-hex, deliberately not unified into one type across the two crates —
    `baud-snapshot-store`'s own module doc: "this crate's job is archival, not interpretation"); a
    future iteration could add a direct `Sha::from_bytes([u8;32])` if this bridging pattern
    recurs elsewhere. Bullet (3) above (`baud-driver` wiring) is the natural next M-series
    increment; no caller yet chains a `/run/kvm/branch { persist_run_id }` → score → `/run/kvm/
    resume` exploration loop — this iteration only proves the primitive round-trips correctly and
    fast.
- **M-series — fourth brick: `baud-driver`'s tape generation wired into branch scoring, closing
  bullet (3) above** ("baud-driver's tape generation feeding branch scoring" — flagged twice now
  as the natural next M-series increment).
  - `POST /run/kvm/branch` gained an optional `generate: DriverGenerateSpec { seed, count,
    tape_len_bytes, strategy }` field (`crates/baud-server/src/routes/run_kvm.rs`), mutually
    exclusive with `branch_tapes_hex`. When set, the server drives `baud_driver::Driver` itself:
    `begin_run()` → draw `tape_len_bytes` bytes via `draw_bits(8)` per byte → fork+run the branch
    via `Multiverse::branch`/`run_to_first_halt` → drain real tape-device records via
    `Multiverse::drain_tape_records()` → `observations_from_records()` turns them into `(probe,
    value)` pairs (every `Msg::Observe` plus a built-in `console_len` fallback observation, since
    no in-tree guest fixture emits a real Observe probe yet) and detects `Msg::Outcome(Outcome::
    Crash)` → `driver.end_run(&observations)` feeds the score back before drawing the next
    branch's tape. Response gains per-branch `tape_hex`/`observations`/`interesting` plus a
    top-level `driver_summary: {generations, goal_reached, best_tape_hex}`.
  - New CLI flags on `baud run kvm-branch`: `--generate-seed`, `--generate-count`,
    `--generate-tape-len-bytes` (default 4), `--maximize <probe>` (repeatable, feeds
    `StrategySpec.maximize`) — `crates/baud-cli/src/cmds/run.rs`, `branch_tapes_hex` changed from
    `required = true` to `required_unless_present = "generate_seed"`.
  - Refactored `boot_snapshot_and_branch`/`resume_and_branch` to share new `boot_and_snapshot`/
    `run_branches` helpers (extracted, not duplicated a third time) — no behavior change to the
    existing fixed-tape path, confirmed by the pre-existing tests still passing unchanged.
  - New tests in `crates/baud-server/src/routes/run_kvm.rs` (8/8 passing, was 5/5):
    `observations_from_records_extracts_probes_and_crash` (pure, no KVM — verifies Msg::Observe/
    Outcome::Crash extraction and the console_len fallback), `run_kvm_branch_generate_is_
    reproducible_and_independent` (real KVM — same seed draws byte-identical tapes across two
    fully independent runs against `tape-echo-guest`, and no cross-branch bleed, the generate-mode
    analogue of the existing fixed-tape test), `generated_branch_point_persists_and_resumes` (real
    KVM — a driver-generated branch point persists and resumes exactly like a fixed-tape one,
    sharing `persist_universe`).
  - Manual live-server end-to-end verification (not just unit tests): `baud run kvm-branch
    --generate-seed --generate-count --maximize console_len --json` against a live server returned
    real `observations`/`driver_summary`; the same seed across two fully separate CLI process
    invocations produced byte-identical `tape_hex` and `driver_summary.best_tape_hex`; mixing
    `branch_tapes_hex` and `generate` in one request, and `generate.count=0`, both returned clean
    `{error}` (never a false pass); the existing fixed-tape `branch_tapes_hex` path was
    re-verified byte-for-byte unchanged.
  - A real, non-blocking finding worth recording as a follow-up (do NOT call this a bug in this
    iteration's own code — it's pre-existing `baud-driver` behavior, `crates/baud-driver/src/
    lib.rs`'s `begin_run`/`draw_bits`/`draw_u64`): `Tape.choices` entries record the *full 8-byte
    raw `draw_u64()` value* for every draw, not the caller-visible truncated `draw_bits(n)` output
    — so `begin_run`'s "mutate" scheduling path (generation % 3 == 1) flips a random bit across
    all 8 recorded bytes, but `draw_bits(8)` only ever surfaces the low byte (LE byte 0) masked
    out of that value. A mutation whose flipped bit lands in bytes 1-7 is invisible to a caller
    that only ever calls `draw_bits(8)` (~7/8 chance per flip on this workspace's current only
    caller). Observed live: with `console_len` constant (tape-echo-guest always echoes exactly
    `tape_len_bytes`, so the maximize signal never varies), `end_run`'s best-tape update never
    fires past generation 0, and several observed generations reproduced byte-identical tapes to
    generation 0. Not a defect in this iteration's route wiring (which drives the Driver exactly
    per its public API) — a corpus-scheduler characteristic that only becomes visible once a
    *real* differentiating score signal exists. Flag as a follow-up: worth revisiting once a guest
    fixture emits real `Msg::Observe` probes with actual variance (todo.md's tape-device entry:
    "no real guest ever writes to this port range" — still true), or fixing `Tape.choices` to
    record only the caller-visible truncated bytes.
  - **Verification**: `cargo build --workspace` clean (zero new warnings anywhere touched).
    `cargo clippy --workspace --all-targets` — fixed one new lint this iteration introduced itself
    (a stray markdown `+`-at-line-start in a new CLI doc comment parsed as an indented list
    continuation, `crates/baud-cli/src/cmds/run.rs`); zero other new warnings in any touched file.
    `cargo test --workspace`: one transient failure seen (`baud-multiverse::linux::tests::
    timer_tick_lands_at_identical_instruction`, H4's own documented real-hardware PMU-jitter test,
    in code this iteration never touched) confirmed transient by an immediate isolated re-run
    (passed). All 16 `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m8.sh`, `full-demo.sh`)
    re-run individually end-to-end on real `/dev/kvm`, zero regressions, `full-demo.sh` "32/32
    CHECKS PASSED".
  - **Not yet done**: this closes bullet (3) from the earlier "M-series — second brick" entry
    ("baud-driver's tape generation feeding branch scoring") for the branch-point route; `POST
    /run/kvm/resume` still only accepts fixed `branch_tapes_hex` (a natural, symmetric follow-up —
    resuming a persisted universe and continuing an in-flight driver's exploration from there);
    nothing yet persists a `Driver`'s own state (seed/best/reservoir) across requests, so a caller
    resuming exploration today always starts a fresh `Driver` rather than continuing an
    interrupted one; no probe-emitting guest fixture exists yet to exercise the `Msg::Observe`
    path for real (the mechanism is built and unit-tested with synthetic records, per finding #6
    above); the "expand a branch point, fork N, score, keep interesting ones as new branch
    points" tree-growth loop (todo.md §6) is still one level deep (one shared branch point per
    request) — chaining `interesting` branches into further generate calls is the natural next
    increment.
- **M-series — fifth brick: `POST /run/kvm/resume` gained the same driver-generate mode
  `/run/kvm/branch` already had, closing the "fourth brick" entry's first "Not yet done" bullet**
  ("`POST /run/kvm/resume` still only accepts fixed `branch_tapes_hex` ... a natural, symmetric
  follow-up").
  - `RunKvmResumeBody` (`crates/baud-server/src/routes/run_kvm.rs`) gained an optional `generate:
    Option<DriverGenerateSpec>` field — the same `DriverGenerateSpec` `/run/kvm/branch` already
    accepts — mutually exclusive with `branch_tapes_hex` (same guard pattern as `branch`). The
    `resume()` handler now mirrors `branch()`'s dual-mode dispatch exactly: when `generate` is
    set, it validates `count >= 1` and `count <= MAX_BRANCHES_PER_REQUEST` (256), then calls the
    existing `run_driver_generated_branches(&universe, spec)` — already shared/reused, not
    duplicated — against a reconstructed universe instead of a freshly booted one.
  - New shared helper `reconstruct_universe(store, run_id, node_id_hex) -> Result<baud_snapshot::
    Universe, String>` extracted from the old `resume_and_branch` (which now just calls it, then
    `run_branches`), houses the existing page-fetch memoization (`page_cache: HashMap<PageHash,
    Vec<u8>>`, the O(distinct pages) fix from the "third brick" entry above) — no behavior change
    to that memoization, just extracted so both the fixed-tape and generate paths share it.
  - New CLI flags on `baud run kvm-resume` (`crates/baud-cli/src/cmds/run.rs`): `--generate-seed`,
    `--generate-count`, `--generate-tape-len-bytes` (default 4), `--maximize <probe>` (repeatable)
    — the exact same flag set and dispatch logic `kvm-branch` already has;
    `branch_tapes_hex` changed from `required = true` to `required_unless_present =
    "generate_seed"`.
  - New test `resumed_universe_generates_reproducible_branches` in `run_kvm.rs`: persists a branch
    point, generates from the in-memory universe directly via `run_driver_generated_branches`, and
    separately via `reconstruct_universe` + `run_driver_generated_branches`, then asserts identical
    tapes/console output/`ram_hash`/`best_tape_hex` between the two. `cargo test -p baud-server
    run_kvm`: 9/9 (was 5/5), verified by counting `#[test]` functions directly in the file.
  - Manual live-server end-to-end verification (not just unit tests): started a real `baud-server`
    against a temp DB/snapshot-store, ran `baud run kvm-branch --persist-run-id` against
    `tape-echo-guest`'s bzImage to get a real `node_id`, then `baud run kvm-resume --run-id ...
    --node-id ... --generate-seed 42 --generate-count 3 --maximize console_len` against that
    persisted universe with **no kernel path in the request at all** — got back real per-branch
    `tape_hex`/`observations`/`interesting` plus `driver_summary`, and confirmed byte-identical
    tapes across two separate CLI invocations with the same seed (reproducibility). Confirmed
    clean `{error}` (never a false pass) for: `generate.count=0`, an unknown `node_id`, and (via a
    direct `curl` — the CLI itself never sends both fields, so this required bypassing the CLI to
    actually hit the server-side guard) mixing `branch_tapes_hex` + `generate` in one request.
    Confirmed the pre-existing fixed-tape `branch_tapes_hex` resume path is byte-for-byte
    unchanged.
  - **Verification**: `cargo build --workspace` clean (zero new warnings). `cargo clippy
    --workspace --all-targets` zero new warnings in any touched file (confirmed via targeted
    `cargo clippy -p baud-server -p baud-cli --all-targets` — no output referencing
    `run_kvm.rs`/`cmds/run.rs`). `cargo test --workspace` 100% green across every crate, zero
    failures (`baud-multiverse` 64/64 unchanged). All 16 `drive/*.sh` scripts (`h0.sh`-`h6.sh`,
    `m0.sh`-`m8.sh`, `full-demo.sh`) re-run individually end-to-end on real `/dev/kvm`, zero
    regressions, `full-demo.sh` "32/32 CHECKS PASSED" (h5's ~232s 1000-branch test included).
  - **Not yet done**: nothing yet persists a `Driver`'s own state (seed/best/reservoir) across
    requests — a caller resuming exploration today always starts a **fresh** `Driver` on each
    `/run/kvm/branch` or `/run/kvm/resume { generate }` call, never continuing an interrupted
    one's corpus/schedule (flagged before this iteration, still open — this iteration did not
    touch it). No probe-emitting guest fixture exists yet to exercise the real `Msg::Observe` path
    (still synthetic-record-tested only). The "expand a branch point, fork N, score, keep
    interesting ones as new branch points" tree-growth loop (todo.md §6) is still one level deep —
    chaining `interesting` branches from a `/run/kvm/branch` or `/run/kvm/resume` response into a
    further generate call (multi-level tree growth) is still open and is now the clearer natural
    next increment, since both the branch and resume entry points support generate mode
    symmetrically. The `Tape.choices` full-8-byte-vs-truncated-`draw_bits(n)` follow-up (flagged
    in the "fourth brick" entry) is untouched, still open.
- **M-series — sixth brick: generated branches persist as real child nodes, plus the concrete
  reason multi-level branch-tree growth (flagged as "the natural next increment" in both the
  "fourth brick" and "fifth brick" entries above) is not simply a wiring exercise.**
  - `run_driver_generated_branches_with_persist` (`crates/baud-server/src/routes/run_kvm.rs`, the
    shared engine behind both `POST /run/kvm/branch`'s generate mode and, via the unchanged
    `run_driver_generated_branches` wrapper, `/run/kvm/resume`'s generate mode) now persists every
    `interesting` branch (goal reached, or a real guest crash) as a genuine child node of the
    branch point in `SnapshotStore` — not just the branch point itself, which was all prior
    iterations persisted. New helper `persist_universe_as(store, run_id, universe, parent:
    Option<NodeId>, at_step, tape_range)` generalizes the pre-existing `persist_universe`, which
    now just calls it with `parent=None, at_step=0, tape_range=(0,0)` for the root case — no
    behavior change to root persistence. `GeneratedBranchOutcome` gained a `node_id: Option<String>`
    field, set only when a branch is both interesting and persistence is active (`persist_run_id`
    supplied), surfaced through to the JSON response by `generated_outcome_to_json`. A caller can
    now `POST /run/kvm/resume` against an interesting generated branch's own `node_id` to
    re-fetch/re-verify its exact final state later (e.g. a crash's guest memory) without re-running
    anything.
  - **A real bug found and fixed in the same increment**: `SnapshotStore::put_universe`'s node
    identity (`crates/baud-snapshot-store/src/types.rs`'s `Sha::of_node_identity(parent, at_step,
    tape_range)`) is a function of tree *position*, not content — and until this iteration the
    store had only ever been exercised with a single root node per run. When multiple sibling
    interesting branches from one generate call were first persisted with a shared,
    index-independent `tape_range=(0, tape_len_bytes)`, every sibling collapsed onto the *same*
    node id and silently overwrote one another in the store (caught by a test asserting distinct
    node ids per branch, which failed until fixed). Fixed by folding each branch's own index `i`
    (within the generate call) into its `tape_range = (i * tape_len_bytes, (i+1) * tape_len_bytes)`,
    giving every sibling a distinct, deterministic, reproducible position.
  - **A real architectural finding — the actual reason multi-level branch-tree growth is not done,
    and can't be done cheaply today.** A first attempt this iteration tried to snapshot a generated
    branch's Multiverse *after* `run_to_first_halt()` and recursively generate further branches
    from that resulting universe — literally chaining generate calls the way the "fourth brick" and
    "fifth brick" entries' "natural next increment" language suggested. A test proved this is
    semantically broken: every guest fixture in this workspace (`hello-guest`, `tape-echo-guest`,
    `timer-guest`, `shell-guest`, `rdrand-guest`) halts as soon as it consumes the tape it's given
    (§3.6's subtractive rule — minimal guests with no reason to read more). Forking an
    already-halted universe with a brand-new tape suffix is a no-op: the vCPU is stuck at `Hlt` and
    never reads the new tape, so `run_to_first_halt()` on it just returns the *original* branch's
    frozen console output, completely independent of the new suffix (confirmed live: asked for
    suffix `[1,2,3,4]`, got back `[208,86,249,80]` — the frozen bytes from the branch's own original
    tape, not the new one). Real multi-level tree growth therefore needs a guest that calls the
    tape device's `MARK_BRANCH` control op mid-execution and *keeps running* afterward (reads more
    tape, does more work) — `observations_from_records` in `run_kvm.rs` already ignores
    `Msg::MarkBranch` for scoring purposes, and nothing anywhere in `baud-multiverse::linux::
    Multiverse` runs "until a branch marker or halt" instead of "until halt" — no such guest
    fixture or run primitive exists yet. This is the concrete, specific blocker (not vague "not yet
    done") for real snapshot-tree exploration (§6's "expand a branch point, fork N continuations,
    score, keep interesting ones as new branch points", chained automatically across multiple
    rounds) — readers should treat this entry, not the "natural next increment" phrasing in the
    fourth/fifth-brick entries above, as current truth on this topic.
  - **Verification**: `cargo build --workspace` clean (zero new warnings). `cargo clippy
    --workspace --all-targets` zero new warnings in `run_kvm.rs` or `crates/baud-cli/src/cmds/
    run.rs` (confirmed via targeted grep against the full clippy output — all shown warnings
    pre-existing in unrelated files: `baud-tracing`'s deprecated `aya::Bpf`, `baud-driver`/
    `baud-proto`/`baud-secret`/`baud-stream` test-only lints, `baud-server/src/routes/{fuzz,replay,
    tracing}.rs`). `cargo test --workspace` 100% green across every crate (`baud-multiverse` 64/64
    unchanged). `cargo test -p baud-server run_kvm`: 10/10 (was 9/9 pre-iteration; one test
    replaced — see below). New test `interesting_generated_branches_persist_as_child_nodes` (real
    KVM, `tape-echo-guest`, a `goal` strategy that's always satisfied so every branch is
    interesting): asserts every branch gets a distinct `node_id`, that `SnapshotStore::read_node`
    reports the correct real parent (the branch point's own node id, not itself a root), and that
    resuming that node with any tape reproduces exactly that branch's own frozen console output
    (the honest property, given the no-op finding above). All 17 `drive/*.sh` scripts
    (`h0.sh`-`h6.sh`, `m0.sh`-`m8.sh`, `full-demo.sh`) re-run individually end-to-end on real
    `/dev/kvm`, zero regressions, `full-demo.sh` "32/32 CHECKS PASSED" (h5's ~225s 1000-branch test
    included).
  - **Not yet done**: real multi-level branch-tree growth needs (1) a new guest fixture that calls
    `MARK_BRANCH` mid-execution and continues reading tape/doing work afterward (the existing
    fixtures are deliberately minimal and halt immediately — building this fixture is real,
    non-trivial work, similar in kind to how `hello-guest`/`tape-echo-guest` were hand-assembled,
    see their `BUILD.md` files for the pattern), and (2) a `Multiverse` run primitive that stops at
    a branch marker instead of only at halt (`run_to_first_halt` has no sibling today). `POST
    /run/kvm/resume` still has no `persist_run_id`/persistence mechanism at all (pre-existing gap,
    unrelated to this iteration — only `/run/kvm/branch` can persist anything). Driver-state
    persistence across requests (seed/best/reservoir) is still not built — still flagged from prior
    iterations, untouched. The `Tape.choices` full-8-byte-vs-truncated-`draw_bits(n)` follow-up
    (flagged in the "fourth brick" entry) is untouched, still open — confirmed still real and live
    in the mutate scheduling path (`generation % 3 == 1` in `baud-driver/src/lib.rs`'s `begin_run`)
    by a research pass this iteration, but not fixed (out of scope for this increment).
- **M-series — seventh brick: the `MARK_BRANCH`-checkpointing guest fixture and
  run-until-branch-or-halt primitive the sixth brick named as the remaining blocker now exist, and
  are proven correct.**
  - **New fixture**: `crates/baud-multiverse/tests/fixtures/mark-branch-guest/` (`payload.s`,
    `build.py`, `BUILD.md`, `bzImage` — 3101 bytes, built via `python3 build.py` using only `as`/
    `ld`, same mechanics as `tape-echo-guest`). The assembly loops 4 times: read one tape byte from
    the tape device's `DATA` port (`0x0500`), echo it to COM1 (`0x3f8`), then issue the tape
    device's `MARK_BRANCH` control op (`mov al, 1` / `out` to `CONTROL` port `0x0508`,
    `ControlOp::MarkBranch = 1`, no payload bytes needed) — then loops back for the next byte,
    instead of halting like every prior fixture. After the 4th iteration it `hlt`s. This is the
    first fixture in the workspace that keeps running past a branch point.
  - **New `Multiverse` primitive**: `crates/baud-multiverse/src/linux/mod.rs` — a new
    `RunUntilBranchOutcome` enum (`Halted(HaltOutcome)` | `MarkBranch { step: u64 }`, defined at
    module scope right after the pre-existing `HaltOutcome` struct) and a new method
    `Multiverse::run_until_branch_or_halt(&mut self, max_exits: u32) -> Result<
    (RunUntilBranchOutcome, Vec<baud_proto::Msg>), DeterminismHole>`, inserted right after the
    pre-existing `run_until_console_len` method. It loops calling the existing `step_exit()` (one
    `KVM_RUN` + dispatch cycle), draining tape-device records after each exit (`self.bus.tape.
    device_mut().drain_records()`) and stopping the instant a `Msg::MarkBranch { step }` record
    appears, or the instant `step_exit()` reports `DispatchOutcome::Halted` — whichever comes
    first. All non-`MarkBranch` records seen along the way are accumulated and returned too, so
    nothing emitted between exits is silently dropped. `max_exits` bounds a guest that does
    neither (returns `Err(DeterminismHole)`), the same convention `run_until_console_len` already
    uses. No existing method's behavior changed.
  - **Three new tests**, all in `crates/baud-multiverse/src/linux/mod.rs`'s `mod tests`, all
    passing against real `/dev/kvm`:
    - `run_until_branch_or_halt_stops_at_first_mark_branch`: boots `mark-branch-guest` with tape
      `[1,2,3,4]`, calls the new primitive, asserts it stops with `MarkBranch { step: 1 }` (not
      `Halted`), the drained records are exactly that one `MarkBranch`, and console output so far
      is exactly `[1]`.
    - `branch_from_mark_branch_checkpoint_diverges_on_new_tape_suffix` — the key proof, directly
      refuting the sixth brick entry's own no-op finding: runs to the first `MARK_BRANCH` (step=1),
      snapshots there, then forks that same universe three times with three different padded tape
      continuations (`vec![0u8; step]` followed by a new suffix — padding is required because
      `Multiverse::branch`'s `tape_suffix` argument becomes the fork's *entire* new tape array, and
      the restored cursor is fast-forwarded to the checkpoint's own `tape_cursor`, so indices
      before the cursor are never re-read and can be dummy bytes): fork A (suffix `[9,8,7]`) and
      fork B (suffix `[42,43,44]`) each echo their own prefix-plus-new-suffix and are asserted to
      differ from each other; fork C (handed the checkpoint's own original continuation bytes
      `[2,3,4]`) is asserted to reproduce a straight, never-forked run's output and `ram_hash`
      exactly. This proves forking a `MARK_BRANCH` checkpoint with new input now genuinely changes
      the guest's subsequent behavior — the opposite of every fixture before `mark-branch-guest`.
    - `two_level_mark_branch_checkpoints_chain`: reaches the first `MARK_BRANCH` (step 1), forks
      onto fresh input but only runs the fork to its *own* next `MARK_BRANCH` (step 2, not to
      halt), snapshots that second checkpoint, forks it again onto yet another fresh suffix, and
      runs to completion — asserts the final output reflects every level's own fresh input in
      order. Proves the primitive composes to unbounded-depth chaining (bounded only by how many
      times a fixture itself calls `MARK_BRANCH`), not just a one-level fix.
    - `cargo test -p baud-multiverse --lib`: 68/68 (was 64/64 per the sixth brick entry's own
      count).
  - **Doc-comment update, no behavior change**: `crates/baud-server/src/routes/run_kvm.rs`'s
    `GeneratedBranchOutcome` doc comment (the one that used to say "no such fixture or run
    primitive exists yet") was rewritten to state the blocker is now closed at the
    `baud-multiverse` level, cite the new fixture/primitive/tests, and explicitly flag what's
    still missing at the route level: nothing in `run_kvm.rs` calls `run_until_branch_or_halt`
    yet — `run_driver_generated_branches_with_persist` still always calls `run_to_first_halt` per
    branch — so a caller of `POST /run/kvm/branch`/`POST /run/kvm/resume` still cannot actually
    persist-and-explore-further from an intermediate `MARK_BRANCH` checkpoint today; only the
    underlying `Multiverse`/fixture capability exists. No CLI flags, no server route logic, no new
    HTTP behavior changed this iteration.
  - **Verification**: `cargo build --workspace` clean (only pre-existing `baud-tracing`
    deprecation warnings, unrelated). `cargo clippy --workspace --all-targets` zero new warnings
    anywhere (confirmed no output referencing `run_kvm.rs`, `linux/mod.rs`, or the new fixture
    directory). `cargo test --workspace` surfaced exactly two failures, both pre-existing,
    known-flaky, hardware-timing-dependent tests unrelated to this change (confirmed by isolated
    re-run of each: both passed 0 failures on retry) — `linux::tests::
    fleet_of_vms_run_in_parallel_without_interference` (a concurrency-speedup timing assertion
    sensitive to host contention) and `linux::tests::timer_tick_lands_at_identical_instruction`
    (the already-documented `RCB_HARDWARE_JITTER_TOLERANCE` PMU-precision flake from the H4 entry
    earlier in this same file). Every other crate in the workspace passed 100% in the same full
    run.
  - **Not yet done (at the time this entry was written — closed by the eighth brick below)**: the
    concrete next brick was wiring `run_driver_generated_branches_with_persist` to actually call
    `run_until_branch_or_halt` instead of always `run_to_first_halt`.
- **M-series — eighth brick: `run_driver_generated_branches_with_persist` now calls
  `run_until_branch_or_halt` instead of `run_to_first_halt`, so a driver-generated branch that
  hits a `MARK_BRANCH` checkpoint stops there, persists as a real child node, and genuinely keeps
  exploring further when resumed — closing the seventh brick entry's own named next step.**
  - `crates/baud-server/src/routes/run_kvm.rs`: the per-branch loop in
    `run_driver_generated_branches_with_persist` (called by both `POST /run/kvm/branch`'s and
    `POST /run/kvm/resume`'s `generate` mode) now calls `Multiverse::run_until_branch_or_halt`
    (bounded by a new `GENERATE_BRANCH_MAX_EXITS = 65536` constant — generous headroom for any
    guest fixture in this workspace today, not a tuned value) instead of `run_to_first_halt`.
    `GeneratedBranchOutcome` gained a `mark_branch_step: Option<u64>` field; a `MARK_BRANCH` stop
    is unconditionally treated as `interesting` (alongside the pre-existing goal/crash checks) —
    unlike an already-halted branch (the seventh brick entry's own no-op finding, still true for
    guests that only ever halt), a `MARK_BRANCH` stop is precisely the case where handing the
    persisted node a fresh tape suffix through `POST /run/kvm/resume` genuinely changes what the
    guest does next, so persisting anything less would silently drop the one outcome this feature
    exists to make explorable. `generated_outcome_to_json` surfaces `mark_branch_step` in the HTTP
    response when set. `crates/baud-multiverse/src/linux/mod.rs`'s `Multiverse::ram_hash` was
    made `pub` (was crate-private) so a `MARK_BRANCH` stop — which has no `HaltOutcome` of its own
    — can still report a RAM hash for its `GeneratedBranchOutcome`.
  - **New test**: `generated_branch_hitting_mark_branch_persists_and_resumes_further`
    (`crates/baud-server/src/routes/run_kvm.rs`, real KVM, `mark-branch-guest` fixture,
    `tape_len_bytes: 1` so the first generated byte triggers the guest's first `MARK_BRANCH`
    before it ever asks for a second byte — mirrors `baud-multiverse`'s own
    `run_until_branch_or_halt_stops_at_first_mark_branch`). Proves, at the HTTP-route function
    level: every generated branch reports `mark_branch_step: Some(1)` and `interesting: true`;
    every branch persists a distinct `node_id` parented on the branch point; and — the property
    that actually matters, the server-route analogue of `baud-multiverse`'s own
    `branch_from_mark_branch_checkpoint_diverges_on_new_tape_suffix` — resuming that persisted
    node through `resume_and_branch` with a fresh tape suffix makes the guest genuinely read,
    echo, and `MARK_BRANCH` three more times with the new bytes (mark-branch-guest loops 4 times
    total), not replay a frozen final output. `cargo test -p baud-server run_kvm`: 12/12 (was
    11/11), all passing.
  - **Verification**: `cargo build --workspace` clean (only pre-existing `baud-tracing`
    deprecation warnings). `cargo clippy --workspace --all-targets` zero new warnings in either
    touched file (`run_kvm.rs`, `linux/mod.rs`); every warning surfaced elsewhere in the workspace
    is pre-existing and unrelated (confirmed by grepping clippy's output for the two touched
    files). `cargo test --workspace` (`--no-fail-fast`, run twice): the only failure both times
    was the already-documented, hardware-timing-sensitive `linux::tests::
    timer_tick_lands_at_identical_instruction` flake (`RCB_HARDWARE_JITTER_TOLERANCE`, unrelated
    to this change — it lives in the H4 interrupt-injection engine, not touched here), confirmed
    transient by an isolated re-run passing 1/1 in 0.58s; every other crate in the workspace
    passed 100% both times. All 17 `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m8.sh`,
    `full-demo.sh`) re-run individually end-to-end on real `/dev/kvm`, zero regressions,
    `full-demo.sh` "32/32 CHECKS PASSED" (h5's ~233s 1000-branch test included).
  - **Not yet done (at the time this entry was written — closed by the ninth brick below)**:
    `resume_and_branch`/`run_branches` (the fixed-`branch_tapes_hex` path, as opposed to
    `generate`) still always called `run_to_first_halt` — a caller resuming a
    `MARK_BRANCH`-persisted node with `branch_tapes_hex` had to supply a full tape long enough
    to carry the guest all the way to its next real `Hlt`; there was no way to ask a
    `branch_tapes_hex` resume to itself stop at the *next* `MARK_BRANCH` rather than running to
    completion (this iteration's scope was specifically `run_driver_generated_branches_with_persist`
    and its generate-mode callers, per the seventh brick entry's own wording — the fixed-tape path
    was never named as part of that gap). A caller resuming a `MARK_BRANCH`-persisted node also
    needs to know to pad its tape suffix so real indices land at the right cursor offset
    (`Multiverse::branch`'s own doc: the suffix becomes the fork's *entire* new tape array, and
    the restored cursor is fast-forwarded past indices already consumed) — undocumented at the
    HTTP API level today, only at the `Multiverse` crate level.
- **M-series — ninth brick: `run_branches` (the shared fork loop behind both `POST /run/kvm/branch`'s
  and `POST /run/kvm/resume`'s fixed-`branch_tapes_hex` mode) now calls
  `Multiverse::run_until_branch_or_halt` instead of `run_to_first_halt`, closing the eighth brick
  entry's own named next step — the fixed-tape path's sibling of what the seventh/eighth bricks did
  for the driver-generated path.**
  - `crates/baud-server/src/routes/run_kvm.rs`: `run_branches` (shared by `boot_snapshot_and_branch`
    and `resume_and_branch`) now calls `Multiverse::run_until_branch_or_halt` (the
    `GENERATE_BRANCH_MAX_EXITS` constant was renamed to `BRANCH_MAX_EXITS` since both the
    driver-generated and fixed-tape paths now share it) instead of `run_to_first_halt`, so a fork
    that hits a `MARK_BRANCH` checkpoint stops there instead of requiring a tape long enough to
    reach the guest's eventual `Hlt`. `BranchOutcome` gained a third field, `mark_branch_step:
    Option<u64>`, surfaced in both `/run/kvm/branch`'s and `/run/kvm/resume`'s fixed-tape JSON
    response bodies via a new shared `branch_outcome_to_json` helper (replacing two duplicated
    inline closures). `boot_and_run` (`POST /run/kvm`'s plain boot-to-first-halt, not a fork) is
    unaffected in behavior — it still always calls `run_to_first_halt` and just always reports
    `mark_branch_step: None`.
  - **Existing test fixed to match the corrected contract**:
    `generated_branch_hitting_mark_branch_persists_and_resumes_further` used to resume a
    `MARK_BRANCH`-persisted node via `resume_and_branch` with a 4-byte suffix and assert the guest
    ran all the way to `Hlt` (echoing all 4 bytes) — that assumed the old, now-fixed
    `run_to_first_halt` behavior. It now supplies only the one real byte needed for the guest's next
    loop iteration and asserts `resume_and_branch` correctly reports `mark_branch_step: Some(2)`
    (the guest's second `MARK_BRANCH`), not a full halt — the fixed-tape analogue of
    `two_level_mark_branch_checkpoints_chain` (`crates/baud-multiverse/src/linux/mod.rs`), now
    proven at the HTTP-route level. `cargo test -p baud-server run_kvm`: 11/11 (one test's
    assertions rewritten, no new/removed test — the property this iteration's own new coverage would
    have added was already covered once the existing test's expectation was corrected).
  - **Verification**: `cargo build --workspace` clean. `cargo clippy --workspace --all-targets` zero
    new warnings (confirmed no output referencing `run_kvm.rs`; every warning shown is pre-existing
    and unrelated, e.g. `baud-tracing`'s deprecated `aya::Bpf`, `baud-driver`'s `manual_div_ceil`).
    `cargo test --workspace` (`--no-fail-fast`) surfaced exactly the one already-documented,
    hardware-timing-sensitive `linux::tests::timer_tick_lands_at_identical_instruction` flake
    (`RCB_HARDWARE_JITTER_TOLERANCE`), confirmed transient by an isolated re-run passing 1/1 —
    every other crate in the workspace, including `baud-server`, passed 100%. All 17 `drive/*.sh`
    scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m8.sh`, `full-demo.sh`) re-run individually end-to-end on real
    `/dev/kvm`, zero regressions, `full-demo.sh` "32/32 CHECKS PASSED" (h5's ~233s 1000-branch test
    and h4's timer-tick test both passed clean in this run too).
  - **Not yet done (at the time this entry was written — closed by the tenth brick below)**: neither
    `boot_snapshot_and_branch` nor `resume_and_branch` persists a per-branch node when a fixed-tape
    fork stops at `MARK_BRANCH` the way the generate path's `persist_universe_as` does; `POST
    /run/kvm/resume` still has no `persist_run_id`/persistence mechanism at all; no `Driver`'s own
    state (seed/best/reservoir) persists across requests; the `Tape.choices`
    full-8-byte-vs-truncated-`draw_bits(n)` follow-up (fourth brick entry) is unfixed; the
    enforced-regime KVM module, RDTSC-compliant-guest testing, `baud shell-into` CLI/server surface,
    and the framebuffer stream (all documented earlier in this file) are all still open.
- **M-series — tenth brick: `run_branches` (the shared fork loop behind both `boot_snapshot_and_branch`
  and `resume_and_branch`) now persists a real child node whenever a fixed-tape fork stops at
  `MARK_BRANCH`, closing the ninth brick entry's own named next step — the fixed-tape path's sibling
  of what `run_driver_generated_branches_with_persist` already does for the driver-generated path.**
  - `crates/baud-server/src/routes/run_kvm.rs`: `run_branches` now takes an optional
    `persist: Option<(&SnapshotStore, &str, Option<NodeId>)>` (store, run_id, parent); whenever a
    branch's `mark_branch_step` is `Some(_)`, it snapshots the live `Multiverse::branch` instance
    (`branch.snapshot(&mut page_store)`, before it drops) and persists it via the existing
    `persist_universe_as` (same content-addressing/`tape_range` scheme
    `run_driver_generated_branches_with_persist` already uses, now with a per-branch cumulative byte
    offset instead of a fixed `tape_len_bytes` stride, since fixed-tape suffixes can vary in length).
    `BranchOutcome` grew a fourth field, `node_id: Option<String>`, surfaced in both `/run/kvm/branch`'s
    and `/run/kvm/resume`'s fixed-tape JSON response bodies via `branch_outcome_to_json` (mirroring
    `generated_outcome_to_json`'s own `node_id` handling). A new shared helper,
    `persisted_root_parent`, replaces the near-duplicate inline match `boot_snapshot_and_generate` used
    to compute its own `root_parent` — `boot_snapshot_and_branch` now uses the same helper.
    `boot_snapshot_and_branch` persists per-branch `MARK_BRANCH` nodes only when its own
    `persist`/`persist_run_id` is set (same opt-in the branch-point persist already required);
    `resume_and_branch` persists unconditionally, since resuming already requires a `store`/`run_id`
    to reconstruct from — this is what actually closes the gap: a caller can now walk a
    `branch_tapes_hex` chain of `MARK_BRANCH` checkpoints via repeated `POST /run/kvm/resume` calls
    without ever switching to `generate` mode.
  - **New test**: `fixed_tape_branch_hitting_mark_branch_persists_and_resumes_further`
    (`crates/baud-server/src/routes/run_kvm.rs`), the fixed-tape sibling of
    `generated_branch_hitting_mark_branch_persists_and_resumes_further` — proves (1) a
    `boot_snapshot_and_branch` fork that stops at `MARK_BRANCH` persists a node parented on the
    branch point (`SnapshotStore::read_node`, same parent-check pattern
    `interesting_generated_branches_persist_as_child_nodes` uses), and (2) `resume_and_branch` from
    that node with a fresh suffix both reaches the guest's *next* `MARK_BRANCH` (not a frozen replay)
    and persists a second node parented on the first — a genuine two-hop chain entirely through the
    fixed-tape HTTP-route functions, the `branch_tapes_hex` analogue of
    `two_level_mark_branch_checkpoints_chain`. `cargo test -p baud-server run_kvm`: 13/13 (was 12/12).
  - **Verification**: `cargo build --workspace` clean. `cargo clippy --workspace --all-targets` zero
    new warnings (no output referencing `run_kvm.rs`; every warning shown is pre-existing and
    unrelated). `cargo test --workspace` (`--no-fail-fast`) surfaced exactly the one already-documented,
    hardware-timing-sensitive `linux::tests::timer_tick_lands_at_identical_instruction` flake
    (`RCB_HARDWARE_JITTER_TOLERANCE`, jitter 79 vs. tolerance 8), confirmed transient by an isolated
    re-run passing 1/1 — every other crate passed 100%, including `baud-server`'s full 12→13-test
    `run_kvm` module. All 17 `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m8.sh`, `full-demo.sh`)
    re-run individually end-to-end on real `/dev/kvm`, zero regressions, `full-demo.sh` "32/32 CHECKS
    PASSED" (h5's ~231s 1000-branch test included). None of the 17 drive scripts exercise the
    `/run/kvm/*` HTTP routes directly today (they remain covered only by `baud-server`'s own unit
    tests) — a real gap worth closing with a dedicated `drive/m9.sh` or similar, but out of scope for
    this increment.
- **M-series — eleventh brick: `drive/m9.sh` — the real gap flagged at the end of the tenth-brick
  entry ("none of the 17 drive scripts exercise the `/run/kvm/*` HTTP routes directly") is now
  closed.** A new drive script sends real HTTP requests to a real `baud-server` process on real
  `/dev/kvm`, exercising `POST /run/kvm`, `/run/kvm/branch`, and `/run/kvm/resume` end-to-end —
  previously these three routes were covered only by `crates/baud-server/src/routes/run_kvm.rs`'s own
  `#[cfg(test)]` unit tests calling their Rust functions directly, never over the wire.
  - `drive/m9.sh`, 9 checks: (1) `/run/kvm` boots `hello-guest` twice, `ram_hash` identical
    (HTTP-level `double_boot_memory_identical`); (2) `/run/kvm/branch` fixed-tape mode against
    `tape-echo-guest` forks 3 independent branches, each echoes exactly its own suffix, no
    cross-branch bleed; (3) `/run/kvm/branch` fixed-tape mode against `mark-branch-guest` with
    `persist_run_id` set stops at `MARK_BRANCH` (`mark_branch_step=1`) and persists a `node_id`;
    (4) `/run/kvm/resume` fixed-tape mode on that node reaches the guest's *next* `MARK_BRANCH`
    (`mark_branch_step=2`) with no `kernel_path`/re-boot, and persists a second, distinct `node_id`;
    (5) `/run/kvm/branch` generate mode (`baud_driver::Driver`-generated tapes) against
    `mark-branch-guest` — every generated branch stops at `MARK_BRANCH` and persists; (6)
    `/run/kvm/resume` generate mode on that branch point keeps exploring with no `kernel_path`,
    reaching the guest's second `MARK_BRANCH` (found live: resuming needs `tape_len_bytes: 2`, not
    `1` — the restored tape cursor is already past the checkpoint byte, so the *first* generated byte
    is never re-read, the same "index 0 never re-read" quirk `resume_and_branch`'s own doc and tests
    already document for the fixed-tape path, now confirmed to apply identically to generate-mode
    resume, which had no prior test either way); (7) three error-handling checks — `branch_tapes_hex`
    + `generate` together, invalid `tape_hex`, and resuming an unknown `run_id`/`node_id` all return a
    JSON `error` field over real HTTP, never a panic or 500.
  - **Verification**: `cargo build --workspace` clean, `cargo clippy --workspace --all-targets` zero
    new warnings (this increment touched only a new shell script, no Rust source). `cargo test
    --workspace` surfaced only the already-documented `timer_tick_lands_at_identical_instruction`
    hardware-timing flake, confirmed transient by an isolated re-run (1/1 pass). All 17 pre-existing
    `drive/*.sh` scripts plus the new `drive/m9.sh` re-run individually end-to-end on real `/dev/kvm`,
    zero regressions; `full-demo.sh` "32/32 CHECKS PASSED".
  - **Not yet done**: every other previously-open item remains untouched and still open: `POST
    /run/kvm/resume` still has no `persist_run_id` toggle for its own *branch-point* persistence (only
    per-`MARK_BRANCH`-checkpoint persistence, added the tenth-brick iteration, needs no such toggle
    since it's unconditional); no `Driver`'s own state (seed/best/reservoir) persists across requests;
    the enforced-regime KVM module, RDTSC-compliant-guest testing, `baud shell-into`
    CLI/server surface, and the framebuffer stream (all documented earlier in this file) are all still
    open.
- **M-series — twelfth brick: fixed the `Tape.choices` full-8-byte-vs-truncated-`draw_bits(n)` bug
  (flagged as a follow-up since the fourth brick entry) — `Driver::begin_run`'s mutate scheduling now
  actually mutates what a `draw_bits(8)` caller sees, every time, instead of ~7/8 of flips landing on
  invisible bytes.**
  - **Root cause** (`crates/baud-driver/src/lib.rs`): `draw_u64()` recorded the *full 8-byte raw*
    `rng.next_u64()`/replay value into `Tape.choices` for every draw, but `draw_bits(n)` (the only
    public draw API any caller in this workspace actually uses) only ever hands the caller
    `ceil(n/8)` bytes of that value — e.g. `draw_bits(8)` only ever surfaces byte 0. `begin_run`'s
    mutate pass (`generation % 3 == 1`) picks `bi = draw_raw_u64() % mutated.choices[i].len()` and
    flips one bit there; with `len() == 8` that lands on the caller-invisible bytes 1-7 seven times
    out of eight, so a "mutated" tape usually replayed byte-for-byte identical to the tape it was
    mutated from.
  - **Fix**: `draw_bits(n)` no longer routes through `draw_u64()`'s recording. A new private
    `next_raw_u64()` returns the next raw/replayed u64 *without* recording it; `draw_u64()` (used by
    `draw_int`/`draw_choice`/`draw_f64`, which already consume the full 8 bytes) calls it and records
    all 8 bytes as before, while `draw_bits(n)` calls it and records only the `ceil(n/8)` bytes it
    actually hands back. `next_raw_u64()`'s replay path also had to stop assuming every recorded
    choice is exactly 8 bytes (the old `bytes.try_into().unwrap_or([0u8; 8])` silently zeroed the
    whole value for any shorter recording); it now zero-extends short choices instead.
  - **New tests** (`crates/baud-driver/src/lib.rs`, 15/15 passing, was 13/13):
    `draw_bits_records_only_caller_visible_bytes` (asserts `draw_bits(8)`/`(16)`/`(32)` record exactly
    1/2/4 bytes, not 8) and `mutating_a_draw_bits_choice_always_changes_the_replayed_value` (flips the
    single recorded byte directly and replays it, asserting the returned byte always differs — the
    property `begin_run`'s mutate pass depends on for `draw_bits(8)` callers specifically, `len() == 1`
    forces `bi == 0` every time).
  - **A real, second bug this fix exposed and also had to be fixed** (`crates/baud-server/src/routes/
    fuzz.rs`, M4/M8's fuzz loop): `draw_parser_input`'s `"random"` tactics branch drew its 8 input
    bytes directly via `driver.draw_bits(8)` per byte, riding on `Driver::begin_run`'s own
    mutate/splice/extend scheduler (which applies to *every* caller of `draw_bits`, regardless of the
    caller's own tactics label) — under the old recording bug that scheduler's mutate pass was itself
    almost always a no-op for these single-byte draws, so "random" tactics accidentally behaved like
    genuine independent-per-generation noise and reliably plateaued (`drive/m4.sh`'s M4.2:
    `plateau_detected` was expected `true`). Once the `Tape.choices` fix above made mutation actually
    work, "random" tactics silently became a weak on-tape hill-climber for roughly a third of
    generations, so `drive/m4.sh` started failing M4.2 (`plateau_detected=false` for seed 42 — the
    run's single depth-2 hit landed outside the naive first-third/last-third window `detect_plateau`
    compares, even though `best_depth` never exceeded 2). Fixed by decoupling `"random"` tactics from
    the driver's tape the same way the `"stateful-mask"` branch already was: draw one on-tape
    `driver.draw_bits(8)` "marker" per generation purely to keep the driver's corpus/depth bookkeeping
    in the loop, then draw the actual 8 input bytes from the tactics-dedicated `ChaCha20Rng`
    (independent every generation, matching the function's own doc comment's stated intent and the
    "why random plateaus" math already documented above `simulate_parser`).
  - **Verification**: `cargo build --workspace` clean (only pre-existing unrelated `baud-tracing`
    `aya::Bpf` deprecation warnings). `cargo clippy --workspace --all-targets` zero new warnings in
    either touched file (`baud-driver/src/lib.rs`, `baud-server/src/routes/fuzz.rs` — every warning
    shown is pre-existing, on lines this iteration never touched). `cargo test --workspace
    --no-fail-fast` (run twice): the only failure both times was the already-documented
    hardware-timing-sensitive `linux::tests::timer_tick_lands_at_identical_instruction` flake, in code
    this iteration never touched, confirmed transient by an isolated re-run passing 1/1 both times;
    every other crate passed 100% both runs. All 18 `drive/*.sh` scripts (`h0.sh`-`h6.sh`,
    `m0.sh`-`m9.sh`, `full-demo.sh`) re-run individually end-to-end on real `/dev/kvm`, zero
    regressions — `drive/m4.sh`'s M4.2 (`--tactics random` plateau) and M8.3 (Mario's random-tactics
    negative control) both explicitly re-verified passing after the fuzz.rs fix, `full-demo.sh`
    "32/32 CHECKS PASSED".
  - **Not yet done**: the consensus-cluster fuzz loop (`run_consensus_fuzz_loop`, same file) still
    draws its base tape via unconditional `driver.draw_bits(8)` per byte regardless of tactics —
    the same coupling to `Driver::begin_run`'s scheduler `draw_parser_input`'s `"random"` branch used
    to have — but no drive script asserts a plateau/negative-control property for the consensus
    workload's `"random"`/default tactics today (`drive/m6.sh`'s M6.2 only asserts the random-tactics
    run *completes*, not that it plateaus), so this was left as-is rather than changed speculatively;
    worth revisiting if a future consensus-workload plateau test is added. `Driver` state
    persistence across requests, the enforced-regime KVM module, RDTSC-compliant-guest testing,
    `baud shell-into` CLI/server surface, and the framebuffer stream remain open (see above).
- **M-series — thirteenth brick: `/run/kvm/resume`'s generate mode now persists interesting
  branches, closing a real gap the "eleventh brick" entry's own follow-up note had actually
  mis-scoped.** That note asked for "a `persist_run_id` toggle for resume's own branch-point
  persistence" — a Sonnet subagent scoping pass (`crates/baud-server/src/routes/run_kvm.rs`) found
  that framing moot: `/run/kvm/resume` never establishes a *new* branch point the way
  `/run/kvm/branch` does (it only reconstructs an already-persisted node), so there is no
  branch-point persistence step to gate, and its fixed-tape path (`resume_and_branch`) already
  persists unconditionally by design (its own doc comment: resuming always already has a
  `store`/`run_id` in hand, so there is nothing to opt into) — no toggle needed there either.
  - **The real, previously-undocumented gap the scoping pass found instead**: `/run/kvm/resume`'s
    *generate* mode called the bare `run_driver_generated_branches(&universe, spec)` — no
    `persist`/`parent` arguments at all — so every interesting branch it found (including a
    `MARK_BRANCH` stop, always `interesting` unconditionally per `GeneratedBranchOutcome`'s own
    doc) was silently dropped on the floor instead of persisted, unlike its own fixed-tape sibling
    `resume_and_branch` (persists unconditionally) and unlike `/run/kvm/branch`'s own generate mode
    (`boot_snapshot_and_generate`, persists when `persist_run_id` is set). `drive/m9.sh`'s M9.6 and
    the unit test `resumed_universe_generates_reproducible_branches` both deliberately never
    asserted a `node_id` on resume-generate output — confirmed known, current (if undocumented)
    behavior, not a test gap that was overlooked.
  - **Fix** (`crates/baud-server/src/routes/run_kvm.rs`): new `resume_and_generate` function,
    mirroring `resume_and_branch`'s existing shape exactly — `reconstruct_universe` then
    `run_driver_generated_branches_with_persist(&universe, spec, Some((store, run_id)),
    Some(parent))`, unconditional, no new request field on `RunKvmResumeBody` needed (same reasoning
    as the fixed-tape path: a resumed node always already has a `store`/`run_id`). The `resume()`
    handler's generate branch now calls this instead of the bare, non-persisting
    `run_driver_generated_branches`, which is now `#[cfg(test)]`-only (its own doc comment updated:
    no production route calls it directly any more) since it would otherwise report a `dead_code`
    warning under `cargo clippy --all-targets` on the non-test build.
  - **New test** (`resumed_generate_persists_mark_branch_children`, `crates/baud-server/src/routes/
    run_kvm.rs`): boots+persists a `mark-branch-guest` branch point, calls `resume_and_generate`
    directly, and asserts every `MARK_BRANCH` stop gets a real, distinct `node_id` correctly
    parented on the *resumed-from* node (via `SnapshotStore::read_node`), not the original root —
    the same check `fixed_tape_branch_hitting_mark_branch_persists_and_resumes_further` already does
    for the fixed-tape sibling. `cargo test -p baud-server run_kvm`: 13/13. `drive/m9.sh`'s M9.6
    gained a `node_id` assertion on every resumed generate branch, confirming the fix at the real
    HTTP level too.
  - **Verification**: `cargo build --workspace` clean (only pre-existing unrelated `baud-tracing`
    `aya::Bpf` deprecation warnings). `cargo clippy --workspace --all-targets` zero warnings/errors
    referencing `run_kvm.rs`. `cargo test --workspace` surfaced only the already-documented
    hardware-timing-sensitive `linux::tests::timer_tick_lands_at_identical_instruction` flake
    (jitter 26 vs. tolerance 8), confirmed transient by an isolated re-run (1/1 pass) and by that
    same test passing cleanly inside `drive/h4.sh`'s and `drive/h5.sh`'s own runs later in this
    iteration; every other crate passed 100%. All 18 `drive/*.sh` scripts (`h0.sh`-`h6.sh`,
    `m0.sh`-`m9.sh`, `full-demo.sh`) re-run individually end-to-end on real `/dev/kvm`, zero
    regressions, `full-demo.sh` "32/32 CHECKS PASSED".
  - **Not yet done**: every other previously-open item remains untouched and still open: the
    consensus-cluster fuzz loop's random-tactics/`Driver`-scheduler coupling (see above, non-blocking,
    no drive script depends on it), `Driver` state persistence across requests, the enforced-regime
    KVM module, RDTSC-compliant-guest testing, `baud shell-into` CLI/server surface (needs new
    WebSocket infra this codebase does not have — see the H5 entry above), and the framebuffer
    stream.
- **M-series — fourteenth brick: `Driver` exploration state (best/reservoir/generation/rng stream
  position) now persists across HTTP requests, closing the "Driver state persistence across
  requests" gap that was open since the eleventh brick and re-flagged in every "Not yet done" list
  since.** Two scoping subagents confirmed this was well-scoped before starting: every route that
  ran an exploration loop (`run_driver_generated_branches_with_persist`, called by both
  `/run/kvm/branch`'s and `/run/kvm/resume`'s generate modes) built `Driver::new` from scratch every
  call, so a second `resume`-generate call against an already-explored `run_id` re-explored with an
  empty `best`/`reservoir` and `generation` reset to 0 — the fuzzing loop never actually accumulated
  progress across requests, only within one.
  - **`crates/baud-driver/src/lib.rs`**: new `DriverState` (`best`, `best_score`, `reservoir`,
    `generation`, `partition_state`, plus `rng_word_pos: u128` — `ChaCha20Rng::get_word_pos()`/
    `set_word_pos()`, required because `begin_run`'s mutate/splice scheduling draws unrecorded "raw"
    rng values that never land on any `Tape`, so restoring only the recorded fields without the rng
    stream position diverges from an in-process-continued driver the moment the first mutate/splice
    decision is made — confirmed live: the first version of the round-trip test failed until this
    field was added). `Driver::export_state()`/`Driver::apply_state()`/`Driver::generation()` (a
    cheap public accessor alongside the full export, for callers that just want to report progress).
    New test `exported_state_resumes_scheduling_identically_to_continuing_in_process` proves a fresh
    `Driver` that applies exported state schedules byte-identically to one that never stopped, not
    just that it looks non-empty. `Score` gained `Serialize`/`Deserialize`. 18/18 tests (was 16).
  - **`crates/baud-snapshot-store/src/store.rs`**: new `put_driver_state`/`get_driver_state`/
    `has_driver_state`, one age-encrypted `driver_state.age` blob per run (same opaque-bytes pattern
    as `put_tape`/`get_tape` — this crate still does not depend on `baud-driver` or parse the bytes,
    per its own Non-Goal). New test `driver_state_roundtrips_and_is_ciphertext_on_disk`. 20/20 tests
    (was 19).
  - **`crates/baud-server/src/routes/run_kvm.rs`**: `run_driver_generated_branches_with_persist`
    now loads `DriverState` from the store before the generate loop (when `persist` is set and a
    state already exists) and writes it back after — `spec.seed`/`spec.strategy` still come from
    each request (a resumed call can change strategy mid-exploration), only accumulated progress
    persists. `DriverRunSummary` gained `cumulative_generation` (the `Driver`'s real generation
    counter after this call, distinct from `generations` which is just this call's `spec.count`),
    surfaced in both `/run/kvm/branch` and `/run/kvm/resume`'s `driver_summary` JSON so an HTTP
    caller can confirm persistence actually accumulated. New test
    `resume_and_generate_persists_and_resumes_driver_state_across_calls` (two sequential
    `resume_and_generate` calls, 3 generations each, same seed/run_id/node_id: asserts persisted
    `generation` goes 3 -> 6 and `reservoir.len()` goes 3 -> 6, not reset each time).
    `cargo test -p baud-server run_kvm`: 14/14 (was 13).
  - **`drive/m9.sh`**: M9.5/M9.6 gained `cumulative_generation` assertions (3, then 5); new **M9.6b**
    step makes a third `/run/kvm/resume` generate-mode call and asserts `cumulative_generation == 6`,
    proving accumulation compounds across three separate real HTTP requests against real `/dev/kvm`,
    not just carrying over once by accident.
  - **Verification**: `cargo build --workspace` clean (only pre-existing unrelated `baud-tracing`
    `aya::Bpf` deprecation warnings). `cargo clippy --workspace --all-targets` zero new
    warnings/errors in any touched file (`baud-driver/src/lib.rs`, `baud-snapshot-store/src/
    store.rs`, `baud-snapshot-store/src/tests.rs`, `baud-server/src/routes/run_kvm.rs`,
    `drive/m9.sh`) — every warning shown is pre-existing, on lines this iteration never touched.
    `cargo test --workspace --no-fail-fast` surfaced only the already-documented
    hardware-timing-sensitive `linux::tests::timer_tick_lands_at_identical_instruction` flake,
    confirmed transient by an isolated re-run (1/1 pass) and by that same test passing cleanly
    inside `drive/h4.sh`'s own run later in this iteration; every other crate passed 100%. All 18
    `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m9.sh`, `full-demo.sh`) re-run individually
    end-to-end on real `/dev/kvm`, zero regressions, `full-demo.sh` "32/32 CHECKS PASSED".
  - **Not yet done**: every other previously-open item remains untouched and still open: the
    consensus-cluster fuzz loop's random-tactics/`Driver`-scheduler coupling (non-blocking, no drive
    script depends on it), the enforced-regime KVM module, RDTSC-compliant-guest testing, `baud
    shell-into` CLI/server surface (needs new WebSocket infra this codebase does not have), and the
    framebuffer stream. `/runs/fuzz`'s own aspirational `/runs/fuzz/:id/step` continuation endpoint
    (mentioned in `fuzz.rs`'s header comment) still does not exist — only `/run/kvm/branch`'s and
    `/run/kvm/resume`'s generate modes gained persistence this iteration, not the older ptrace-era
    `/runs/fuzz` route, which has no continuation route to wire it into yet.
- **M-series — fifteenth brick: the RDTSC-compliance half of H3's "randomness + time control"
  (todo.md §3.3/§3.8) is closed — a real guest's raw `rdtsc` instruction now reproduces
  deterministically across boots under the cooperative regime, not just its frequency.** A
  research subagent confirmed the exact gap before any code changed: `rdrand_guest_is_flagged`
  (H3's other named test) only covers the *random*-instruction half; RDTSC has no CPUID feature
  gate to hardware-block it the way RDRAND is blocked, so a *compliant* guest reading the raw
  timestamp directly still needs the VMM itself to serve a reproducible value. `boot_guest`
  (`crates/baud-multiverse/src/linux/mod.rs`) called `set_tsc_khz` (frequency only) but never
  anchored the counter's actual *value* — confirmed live: an unpinned raw `rdtsc` read reflected
  implicit host-wall-clock-derived state, diverging by tens of millions of virtual-TSC counts
  (tens of milliseconds at `VIRTUAL_TSC_KHZ` == 1 GHz) between two separate boots, not mere
  scheduling jitter.
  - **Fix**: new `pin_tsc_value(vcpu, value)` in `linux/mod.rs` calls
    `KVM_SET_MSRS(IA32_TSC=0)` — KVM's own documented mechanism for setting the vCPU's raw TSC
    offset directly; unlike a guest's own RDMSR/WRMSR, this ioctl does not round-trip through
    `KVM_X86_SET_MSR_FILTER`'s exit-to-userspace path at all (the filter only gates
    *guest-instruction-triggered* MSR access — `baud-snapshot::linux::restore`'s existing
    `SetVcpuMsrs` step already relies on the same fact to restore a captured TSC value onto a
    vCPU with an identical filter active, so this was a known-safe pattern, not a new risk).
    **Two real-hardware findings during verification, both now documented at their fix sites**:
    (1) pinning right after `set_tsc_khz` (the natural first attempt) left the page-table writes
    and kernel-image load — both I/O-bound and run-to-run-variable — between the pin and the
    guest's first `rdtsc`, which dominated the observed jitter far more than genuine
    host-scheduling jitter; moved to the very last step of `boot_guest`, immediately before
    returning to the caller (who enters `KVM_RUN` right after), cutting the disagreement from
    tens of millions of counts to single-digit millions. (2) a fixture's *first* boot in a fresh
    test process still reads several million counts higher than every boot after it (cold
    page-cache fill for the bzImage file, first-ever KVM/`perf_event` syscalls in that process) —
    the new test discards a warm-up boot before comparing two already-warm ones, isolating the
    steady-state jitter the test actually cares about.
  - **New fixture** `crates/baud-multiverse/tests/fixtures/rdtsc-guest/` (`payload.s`/`build.py`/
    `BUILD.md`, same hand-assembled-bzImage mechanics as `rdrand-guest`): writes marker byte `'T'`
    to COM1, executes raw `rdtsc`, packs `edx:eax` into one 64-bit value, echoes its 8 bytes
    low-byte-first, halts. New test `rdtsc_guest_reproduces_high_bits_across_boots`
    (`crates/baud-multiverse/src/linux/mod.rs`) masks off the low 20 bits (`RDTSC_JITTER_MASK`,
    generous relative to the few-hundred-thousand-count jitter actually observed, nowhere near
    large enough to mask an unpinned TSC's billions-of-counts divergence) before asserting two
    boots' values agree — matching todo.md §3.3's own test spec verbatim ("cooperative asserts
    the high bits / work-derived field, not full equality"; enforced regime would need the
    not-yet-built custom KVM module for bit-exactness). Full provenance in that fixture's
    `BUILD.md`. `cargo test -p baud-multiverse`: 68/68 (was 67), the new test stable 5/5 on a
    manual back-to-back re-run.
  - **`drive/h3.sh`** gained **H3.4**, running the new test against real `/dev/kvm` and asserting
    `test result: ok`; the summary block documents the new guarantee alongside H3.2/H3.3's.
  - **Verification**: `cargo build --workspace` clean (only pre-existing unrelated `baud-tracing`
    `aya::Bpf` deprecation warnings). `cargo clippy --workspace --all-targets` — zero
    warnings/errors referencing `linux/mod.rs` or the new fixture files (every warning shown is
    pre-existing, in files this iteration never touched). `cargo test --workspace` surfaced only
    the already-documented `linux::tests::fleet_of_vms_run_in_parallel_without_interference`
    timing flake under heavy concurrent load (todo.md's own prior note on this exact test),
    confirmed transient by an isolated re-run (1/1 pass); every other crate passed 100%. All 18
    `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m9.sh`, `full-demo.sh`) re-run individually
    end-to-end on real `/dev/kvm`, zero regressions, `full-demo.sh` "32/32 CHECKS PASSED".
  - **Not yet done**: every other previously-open item remains untouched and still open: the
    consensus-cluster fuzz loop's random-tactics/`Driver`-scheduler coupling (non-blocking, no
    drive script depends on it), the enforced-regime KVM module (RDTSC-compliance under
    *enforced* regime — bit-exact, forced-exiting — still needs it; only the cooperative-regime
    half closed this iteration), `baud shell-into` CLI/server surface (needs new WebSocket infra
    this codebase does not have — the crate-level `shell_into_universe_resumes` test already
    passes, see the H5 entry above), the framebuffer stream, and `/runs/fuzz`'s aspirational
    `/runs/fuzz/:id/step` continuation endpoint (the older ptrace-era route, distinct from
    `/run/kvm/branch`'s and `/run/kvm/resume`'s generate-mode persistence).
- **M-series — sixteenth brick: the consensus-cluster fuzz loop's random-tactics/`Driver`-scheduler
  coupling flagged in every "Not yet done" list since the thirteenth brick is now closed —
  `run_consensus_fuzz_loop` (`crates/baud-server/src/routes/fuzz.rs`) no longer draws its
  "random"/"random-drops" tape bytes straight off `driver.draw_bits(8)`.** Drawing all 256 bytes
  per generation that way meant the tape meant to serve as an independent negative control against
  guided tactics like `markov-crash-restart`/`markov-partition` silently inherited
  `Driver::begin_run`'s own generation-over-generation hill-climbing toward `self.best` — the exact
  same coupling already fixed once before for the parser workload's `draw_parser_input`, and this
  fix mirrors that one.
  - **Fix**: new `draw_consensus_tape(driver, len, rng)` in `fuzz.rs` draws one marker byte via
    `driver.draw_bits(8)` (so the driver's corpus/score tracking stays in the loop) and then draws
    all `len` real tape bytes from the dedicated `tactics_rng`, independent of the driver's
    replay/mutate/splice scheduling. `run_consensus_fuzz_loop`'s per-generation loop now calls this
    helper in place of the old inline `driver.draw_bits` loop.
  - **New tests**, both in a new `#[cfg(test)] mod consensus_tape_tests` at the bottom of
    `fuzz.rs`: `consensus_tape_is_independent_of_driver_hill_climbing` hill-climbs a driver for 10
    generations with a maximal score every time (biasing `self.best`, advancing `generation` well
    past 0), then asserts `draw_consensus_tape`'s output is byte-identical to a fresh, never-run
    driver given the same `tactics_rng` seed — proving the tape depends only on the RNG, never on
    driver state. `consensus_tape_varies_across_generations` asserts successive draws differ, so
    the negative control is genuine per-generation noise rather than degenerate output. `cargo test
    -p baud-server`: 16/16 (was 14).
  - **Bonus cleanup, same iteration**: `fuzz.rs`'s header comment claimed a
    `POST /runs/fuzz/:id/step` continuation route that was never implemented or registered in
    `main.rs` and that no spec references. A research pass confirmed the KVM pivot already solves
    cross-request continuation via `/run/kvm/branch`'s and `/run/kvm/resume`'s generate mode
    (`DriverState`/`put_driver_state`/`get_driver_state`, see the fourteenth-brick entry above), so
    a parallel continuation mechanism for the older ptrace-era `/runs/fuzz` route would be
    redundant rather than a real gap. The header comment now describes the actual route surface
    (`POST /runs/fuzz`, `GET /runs/fuzz/:id`) and points at the KVM-era surface for continuation
    instead of describing an endpoint that does not exist. Worth remembering for future comments in
    this crate: the workload-noun CI grep enforced by `drive/m1.sh`'s M1.8 and `drive/m0.sh`'s
    check forbids the literal lowercase word "emulator" outside `baud-raftlet` — an early draft of
    this comment tripped it via "emulator-bridge workloads" and had to be reworded to "bridge
    workloads (see WorkloadKind)".
  - **Verification**: `cargo build --workspace` clean (only pre-existing unrelated `baud-tracing`
    `aya::Bpf` deprecation warnings). `cargo clippy --workspace --all-targets` — zero new
    warnings/errors on any touched line in `fuzz.rs` (every warning shown is pre-existing, on lines
    this iteration never touched). `cargo test --workspace`: 100% green, 0 failed across every
    crate. All 18 `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m9.sh`, `full-demo.sh`) re-run
    individually end-to-end on real `/dev/kvm`, zero regressions, `full-demo.sh` "32/32 CHECKS
    PASSED" — `drive/m6.sh`, the only drive script exercising the consensus workload, still passes
    with the fix in place (random-tactics baseline completes, stateful-mask still finds the
    violation in 1 generation).
  - **Not yet done**: the enforced-regime KVM module (RDTSC-compliance under *enforced* regime —
    bit-exact, forced-exiting), `baud shell-into` CLI/server surface (confirmed this iteration via
    research to need a genuinely new axum `ws`-feature route plus a bidirectional Multiverse
    session, universe-by-ID deserialization, CLI subcommand, and auth — a multi-subsystem task, not
    a one-iteration fix), and the framebuffer stream (confirmed this iteration via research to be
    blocked on building an entirely new guest display device model first — `baud-multiverse` has no
    framebuffer/VGA/virtio-gpu device today, an explicit spec non-goal per
    `specs/baud-multiverse.md` — before `baud-stream`, otherwise essentially complete per
    `specs/baud-stream.md`'s named tests, has anything real to capture).
- **M-series — seventeenth brick: `baud shell-into` CLI/server surface, flagged as open in every
  "Not yet done" list since the eleventh brick, is now closed.** `crates/baud-server/src/routes/shell_into.rs`
  adds `GET /shell-into/{run_id}/{node_id}`, an axum WebSocket route (new `ws` feature on the workspace
  `axum` dep) that reconstructs a persisted `Universe` via the same `reconstruct_universe` helper
  `/run/kvm/resume` already used (now `pub(crate)` in `run_kvm.rs`, alongside `WORK_CLOCK_K`), restores it
  into a live `Multiverse`, and bridges the guest's console to the socket. The guest loop runs on a
  dedicated `spawn_blocking` thread doing real synchronous `KVM_RUN` ioctls, bridged to the WebSocket
  through two `tokio::sync::mpsc` channels. `crates/baud-cli/src/cmds/shell_into.rs` adds the matching
  `baud shell-into <run_id> <node_id>` command (new `tokio-tungstenite` CLI dep) with a real interactive
  mode (stdin line-by-line until stdin closes) and a scripted `--input-hex <hex>` mode that sends once and
  collects output until `--idle-timeout-ms` idle, printing `{"ok":true,"output_hex":...}` for drive scripts
  that have no real TTY. `crates/baud-cli/src/client.rs` gained a `Client::ws_url()` helper.
  - Two real bugs surfaced during manual interactive testing and are fixed and documented in
    `shell_into.rs`'s own header doc: (1) `tokio-tungstenite` auto-replies to a client-sent `Close` frame
    before the app can flush pending output, so the wire protocol now uses an empty `Binary` frame as the
    "no more input" sentinel instead of a `Close` frame — only the server ever sends the real `Close`; (2)
    the session loop used to return as soon as the input channel drained to `Disconnected`, even when that
    same drain pass had just enqueued real input the guest never got a `step_exit` to react to — fixed with
    a bounded post-disconnect settle window (`POST_DISCONNECT_SETTLE_EXITS = 200_000` guest-side exits).
    Two regression tests cover both scenarios: `drive_shell_session_echoes_queued_input_and_stops_on_disconnect`
    and `drive_shell_session_echoes_input_sent_immediately_before_disconnect`.
  - `POST /run/kvm/branch` also gained a "persist-only" mode: an empty `branch_tapes_hex` is now accepted
    (not an error) when `persist_run_id` is set, so a guest that never calls `MARK_BRANCH` and never halts
    (like the new `shell-guest` fixture) can still reach the `SnapshotStore` via this route — boot, snapshot,
    persist, fork zero branches.
  - `drive/m10.sh` (new), 4 checks: M10.1 persist-only branch; M10.2 scripted shell-into round trip
    (byte-exact transcript `$ hi\n$ ` against `shell-guest`); M10.3 repeatability (same node twice →
    identical transcript); M10.4 error handling (unknown `run_id`/`node_id` reports an in-band error, no
    hang or crash, server stays healthy).
  - **Verification**: `cargo build --workspace` clean (only pre-existing unrelated `baud-tracing` `aya::Bpf`
    deprecation warnings). `cargo clippy --workspace --all-targets` — zero warnings on any touched file.
    `cargo test --workspace`: 100% green, 0 failed, across all 41 test binaries (`baud-server` now 18/18, up
    from 16, including the two new `shell_into` regression tests). All 19 `drive/*.sh` scripts (`h0.sh`-`h6.sh`,
    `m0.sh`-`m10.sh`, `full-demo.sh`) re-run individually end-to-end on real `/dev/kvm`, zero regressions;
    `drive/m10.sh` itself all M10.1-M10.4 checks passed; `full-demo.sh` "32/32 CHECKS PASSED".
  - **Not yet done (at the time this entry was written — the framebuffer-stream half closed by the
    eighteenth brick below)**: the enforced-regime KVM module (RDTSC-compliance under *enforced*
    regime — bit-exact, forced-exiting; only the cooperative-regime half closed, per the fifteenth
    brick), and the framebuffer stream (this entry's premise — "blocked on building an entirely new
    guest display device model" — turned out to be wrong; see the eighteenth brick).
- **M-series — eighteenth brick: the framebuffer stream, flagged as open (and, on closer look,
  mis-diagnosed) in every "Not yet done" list since the M-series crate map first described
  `baud-stream` capturing a real guest's display, is now closed at the transport level.** The
  seventeenth brick's note above assumed real frame capture needed "an entirely new guest display
  device model" (VGA/virtio-gpu) — a genuine research pass this iteration found that premise false:
  `baud_proto::Msg` already had a `Frame(FrameRecord)` variant nobody populated, and
  `crates/baud-stream` (frame fingerprinting, QOI/Y4M encoding) was already fully built and
  unit-tested — the actual gap was narrower: `baud-tape-device::ControlOp` only had
  `Probe`/`MarkBranch`/`Goal`/`Violation`/`Log`, and no guest fixture ever wrote to the tape device
  to emit a frame. specs/baud-stream.md §3's own display-adapter contract already described exactly
  this shape ("the guest ... writes length-prefixed raw frame buffers ... the supervisor's device
  model delivers them") — the tape device already *is* that device model, the same way it already
  carries `LOG`/`PROBE`; specs/baud-multiverse.md's non-goal ("real device emulation beyond the
  console + tape device") stays true because no new device was added, only a new opcode on the
  existing one.
  - **Fix**: new `ControlOp::Frame` (opcode 5, `crates/baud-tape-device/src/lib.rs`) — a guest
    writes a 1-byte pixel-format tag + little-endian `u32` width + little-endian `u32` height + raw
    pixel bytes to `DATA`, then finalizes with opcode 5 on `CONTROL`; `parse_frame` decodes the
    header (geometry validation is deliberately left to `baud-stream::fingerprint`, not this
    transport — a short header or unrecognized format byte is the only `MalformedPayload`), blake3
    hashes the pixel bytes (new `blake3` dependency on this crate), and pushes
    `Msg::Frame(FrameRecord)`. 6 new unit tests (decode/hash agreement across two devices/malformed
    header/unknown format/zero-length pixels), `cargo test -p baud-tape-device`: 24/24 (was 18).
    specs/baud-tape-device.md §4 documents the new opcode as the single source of truth for its byte
    layout.
  - **New fixture** `crates/baud-multiverse/tests/fixtures/framebuffer-guest/` (hand-assembled
    bzImage, identical wrapping mechanics to `rdtsc-guest`/`rdrand-guest`): writes marker byte `'F'`
    to COM1, then a real 2x2 `Indexed8` frame (pixels `10, 20, 30, 40`) through the new `FRAME`
    opcode, then halts — the first real guest fixture in this workspace to exercise
    `ControlOp::Frame`. New test `linux::tests::framebuffer_guest_frame_is_reproducible_across_boots`
    boots it twice on real `/dev/kvm`, drains the tape device's single `Msg::Frame` record via
    `Multiverse::drain_tape_records`, and asserts width/height/format/bytes/hash are byte-identical
    across both boots — specs/baud-stream.md §7's own named test
    (`frame_hashes_double_run_identical`) run for the first time against a real guest on real
    hardware instead of `baud-stream`'s crate-level synthetic buffers — and cross-checks that
    `baud-tape-device`'s hash agrees with `baud_stream::fingerprint` (new `baud-stream` dev-dependency
    on `baud-multiverse`, test-only). `cargo test -p baud-multiverse`: 69/69 (was 68).
  - **Spec fix (dispatched to an Opus subagent, per the "spec inconsistencies" directive)**:
    specs/baud-stream.md was still "Version 1.0" describing the *pre-pivot* userspace model
    (`baud-init`, `transport: fifo|vfs`, "the agent side") even though every sibling spec this
    system touches (`baud-multiverse.md`, `baud-tape-device.md`) had already been rewritten for the
    KVM pivot. Bumped to "Version 2.0" (2026-07-25); §3's display adapter now describes the real
    `FRAME`-opcode transport and cross-references specs/baud-tape-device.md §4 as the byte layout's
    single source of truth instead of re-describing it (avoiding two sources of truth); §2's
    diagram caption and §6's "agent transport" phrasing updated to current terminology. §§1, 4, 5,
    8–10 were verified against `crates/baud-stream/src/lib.rs` and left untouched (already
    accurate). The agent also flagged a **separate, smaller follow-up**: `crates/baud-init`'s
    `FrameAdapter`/`FrameTransport{Fifo,Vfs}` types (and specs/baud-init.md:90's reference to them)
    are dead pre-pivot artifacts — `FrameTransport` has zero references outside that crate — not
    fixed this iteration, noted here for whoever picks up `baud-init` next.
  - **Verification**: `cargo build --workspace` clean (only pre-existing unrelated `baud-tracing`
    `aya::Bpf` deprecation warnings). `cargo clippy --workspace --all-targets` — zero warnings on
    any file this iteration touched (`baud-tape-device`, the new fixture, `linux/mod.rs`,
    `Cargo.toml`s). `cargo test --workspace`: 100% green, 0 failed, across every crate. All 19
    `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m10.sh`, `full-demo.sh`) re-run individually
    end-to-end on real `/dev/kvm`, zero regressions (including `drive/m5.sh`, the only drive script
    that already exercised `baud-stream`'s pre-pivot synthetic-frame HTTP path — untouched by this
    iteration and still 100% passing); `full-demo.sh` "32/32 CHECKS PASSED".
  - **Not yet done (at the time this entry was written — the frame-persistence and replay-render
    gaps closed by the nineteenth brick below)**: this closes the framebuffer stream's *transport*
    — a real guest can now produce a real, deterministic `Msg::Frame` — but nothing in
    `baud-server`/`baud-cli` consumes it yet. Two concrete gaps remain, both scoped follow-ups, not
    further transport work: (1) no `/run/kvm*` boot route drains `Msg::Frame` records and persists
    their hashes into the `frame_records` table the way `POST /runs/:id/frames` (the pre-pivot
    manual-seed endpoint) does
    — a real KVM boot's frames are captured in-process but never reach the DB `baud stream frames`
    reads from; (2) `POST /runs/:id/stream/render` (`crates/baud-server/src/routes/stream.rs`) is
    still an explicit stub — its own header comment says "In a full implementation this would replay
    the tape under baud-multiverse with capture enabled," but it actually fabricates a synthetic
    gradient from each frame's stored hash, since `frame_records` deliberately stores hashes only
    (specs/baud-stream.md §5's "Storage Discipline" — pixels are meant to be regenerated on demand
    by replay, never journaled). Implementing real replay-based rendering needs the render route to
    know which kernel/tape produced a run's frames (not currently persisted per run_id by `/run/kvm`)
    and to re-boot `Multiverse` with frame capture enabled — a genuinely separate task from this
    iteration's transport fix, since it touches run-metadata persistence, not the tape device.
    Also still open, untouched by this iteration: the enforced-regime KVM module (RDTSC-compliance
    under *enforced* regime — bit-exact, forced-exiting; only the cooperative-regime half closed,
    per the fifteenth brick).
- **M-series — nineteenth brick: real `Msg::Frame` persistence into `frame_records` + real
  replay-based `POST /runs/:id/stream/render`, both gaps flagged as open in the eighteenth
  brick's "Not yet done", are now closed.** `POST /run/kvm`'s `RunKvmBody` gained an optional
  `run_id` field (`crates/baud-server/src/routes/run_kvm.rs`); when set, the boot's
  kernel_path/cmdline/tape_hex are persisted into a new `kvm_run_meta` table
  (`migrations/0010_kvm_run_meta.sql`, upserted) and every real `Msg::Frame` the guest emits
  (drained via the route's new `boot_run_and_drain`/`boot_and_drain_frames` helpers, which wrap
  `Multiverse::drain_tape_records`) is inserted into `frame_records` — closing "a real KVM boot's
  frames are captured in-process but never reach the DB". `stream::render`
  (`crates/baud-server/src/routes/stream.rs`) now checks `kvm_run_meta` first: if a row exists for
  the run, it re-boots that exact kernel/cmdline/tape under `Multiverse`
  (`render_frames_from_real_replay`, Linux-gated with a `cfg(not(target_os = "linux"))` stub for
  portability), drains the real `Msg::Frame` records, and converts them to RGBA with
  `baud_stream::to_rgba` — real guest pixels in the Y4M/QOI output, not a hash-seeded synthetic
  gradient. Runs with no `kvm_run_meta` row (every pre-pivot manually-seeded run via
  `POST /runs/:id/frames`, which never had a kernel/tape to replay) fall through unchanged to the
  prior synthetic-gradient-from-hash path (`render_frames_from_stored_hashes`) — `drive/m5.sh`,
  `m8.sh`, `full-demo.sh` still exercise exactly that path and are unaffected.
  - New test `run_kvm::tests::boot_and_drain_frames_is_deterministic_and_carries_real_pixels`
    (real `/dev/kvm`, `framebuffer-guest` fixture) proves the server-level wrapper preserves real
    pixel bytes and determinism across two boots. New `drive/m11.sh` (5 checks) exercises the full
    HTTP path against a real `baud-server` process: real boot → `frames_recorded=1` → real frame
    row in the DB → `stream/render` output whose QOI bytes decode (via a hand-rolled QOI decoder
    in the drive script) to the guest's actual pixels `(10,10,10),(20,20,20),(30,30,30),(40,40,40)`
    — not a synthetic gradient — → re-render is byte-identical (real replay is deterministic) →
    the pre-pivot synthetic-fallback path still renders a manually-seeded run with no
    `kvm_run_meta` row.
  - **Verification**: `cargo build --workspace` clean (only pre-existing unrelated `baud-tracing`
    `aya::Bpf` deprecation warnings). `cargo clippy --workspace --all-targets` — zero warnings on
    `run_kvm.rs`/`stream.rs`, the only files this iteration touched (every other warning shown is
    pre-existing, on lines this iteration never touched). `cargo test --workspace`: 100% green, 0
    failed across every crate. All 20 `drive/*.sh` scripts (`h0.sh`-`h6.sh`, `m0.sh`-`m11.sh`,
    `full-demo.sh`) run individually end-to-end on real `/dev/kvm`, zero regressions;
    `full-demo.sh` "32/32 CHECKS PASSED".
  - **Not yet done**: the enforced-regime KVM module (RDTSC-compliance under *enforced* regime —
    bit-exact, forced-exiting) remains the sole open item in this file — a real out-of-tree kernel
    patch, not something any prior iteration has attempted; everything else the M-series crate map
    (§8) and milestone list (§10) originally scoped is now built and wired end-to-end on real
    hardware.
- **Enforced-regime KVM module — first real, on-hardware attempt: out-of-tree module build
  pipeline now genuinely works on this host, real (non-stub) VMX capability-probe module written
  and build-verified, `insmod` blocked on a precisely diagnosed toolchain issue.** Previously this
  was "not attempted" because the stock WSL2 kernel ships no `linux-headers-*`/`/lib/modules/
  $(uname -r)/build` at all. Closed that blocker for real: cloned the exact matching kernel source
  (`microsoft/WSL2-Linux-Kernel@linux-msft-wsl-6.18.33.2`, matching this dev machine's running
  `uname -r` exactly), seeded `.config` from `/proc/config.gz` (the running kernel's actual build
  config), ran `make modules_prepare`, and symlinked the result to `/lib/modules/$(uname -r)/build`
  — full procedure now in `CLAUDE.md`. New `kernel-module/baud-enforced/` (Makefile +
  `baud_enforced_probe.c`, `BUILD.md`): a real, complete, non-stub module — not a hello-world
  placeholder — that reads the VMX capability MSRs (`IA32_VMX_BASIC`/`PROCBASED_CTLS`/
  `PROCBASED_CTLS2`, Intel SDM Vol. 3D §A.3) to answer the actual open hardware question behind the
  enforced regime: does this CPU's microcode even allow setting RDTSC-exiting (primary proc-based
  control bit 12) and RDRAND/RDSEED-exiting (secondary controls bits 11/16) at all. Read-only, never
  touches VMX/KVM state, safe to load alongside running VMs by construction. Builds cleanly with
  vermagic matching `uname -r` exactly (needed fixing the shallow clone's `git describe` failure,
  which was appending a spurious `+`, and matching the running kernel's build-gcc major version via
  `gcc-13` to remove a `CONFIG_CC_HAS_COUNTED_BY` divergence — both now documented in `BUILD.md` and
  `CLAUDE.md`).
  - **`insmod` still fails, precisely diagnosed, not a bug in this module or its build files**:
    `.gnu.linkonce.this_module section size must match the kernel's built struct module size at run
    time` (`kernel/module/main.c`'s `elf_validity_cache_index_mod`) persists even with a
    config-identical, gcc-major-matching (`gcc-13.4.0`) build. Root cause traced to a toolchain-
    generation gap `.config` alone can't close: the real running kernel was built with the exact
    vendor toolchain `gcc (GCC) 13.2.0` + `GNU ld 2.41`; this machine's closest available substitute
    is Ubuntu's `gcc-13.4.0` + `binutils 2.46`, and `.config` itself still shows
    `CONFIG_GCC_ASM_GOTO_OUTPUT_BROKEN=y` on the real build vs. `CONFIG_CC_HAS_ASM_GOTO_OUTPUT=y`
    with any Ubuntu gcc-13 — a genuine codegen-path divergence between compiler builds of the same
    nominal version. Full diagnosis and every ruled-out hypothesis (config diffing, GCC major-version
    matching) in `kernel-module/baud-enforced/BUILD.md`.
  - **Not yet done**: (1) getting `insmod` to actually succeed needs Microsoft's literal build
    toolchain (exact `gcc 13.2.0` + `binutils 2.41`), not a same-major substitute — an open-ended
    toolchain-reproduction task, deliberately not attempted further this iteration to avoid an
    unbounded rabbit hole; (2) even once loadable, this module only *reads* capability MSRs — the
    actual enforcement (hooking KVM's own VMCS setup to force those bits on for every guest,
    regardless of guest cooperation) is a materially larger, separate task on top of this;
    `crates/baud-host/src/linux.rs`'s `enforced_module_present()` deliberately still returns `false`
    unconditionally — wiring it to this probe module would overclaim a regime this host doesn't
    actually enforce yet (`regime_is_recorded_and_not_overclaimed`).
