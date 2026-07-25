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

- baud tests a whole machine, not a process. Determinism is imposed at the virtualization layer, so the
  guest may run threads, dynamic binaries, multiple processes, any language — none of the single-process
  limits apply.
- Every guest-visible nondeterminism source — time, randomness, device input, interrupt timing — is served
  by the VMM from, or seeded from, the tape. Same guest image + same tape ⇒ byte-identical execution.
- Because the machine is reproducible, baud snapshots any moment, forks many continuations that share memory
  copy-on-write, rewinds, and measures what fraction of variations still hit a bug — a tree of universes.
- The guest runs **unmodified**. baud does not ask the guest to be deterministic; it makes the guest
  deterministic by controlling the machine underneath it — under VT-x `cpuid` always exits, `rdtsc`/`rdrand`
  are trapped, the one instruction the host virtualization can't trap (`rdseed`) is rewritten out of the
  guest image at build time, and the guest kernel's own entropy inputs are therefore all deterministic.

## 1. Hard constraints

- **Host exposes `/dev/kvm` with Intel VT-x.** Managed containers do not expose it; baud runs on bare-metal
  or nested-virtualization hosts you provision (§9). Verified at H0, re-checked on every host by
  `baud host probe`.
- **x86_64 Intel primary.** The determinism instructions (`rdtsc`, `cpuid`, the branch counter) are x86;
  Intel has full CPUID interception and TSC control. AMD is deferred (§3.9); arm64 is out.
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
- **run**: `{ seed, guest-image hash, strategy, tactics, snapshot-tree }`. Fully reproduces everything.

## 3. `baud-multiverse` — the deterministic VMM (core deliverable)

A single-vCPU KVM VMM whose every exit resolves to a deterministic value. There is one determinism model:
every guest-visible nondeterminism source is handled by exactly one mechanism (below); there are no tiers or
modes. Full component detail in `specs/baud-multiverse.md`; the single-vCPU state machine in
`specs/baud-vcpu.md`.

**The determinism model at a glance** — each source, its one mechanism:

| Source | Mechanism |
|---|---|
| Time (`rdtsc`/`rdtscp`, kvmclock, APIC timer) | Trapped and served a **work-clock** value (retired conditional branches); virtual clocks follow it; no wall clocks (§3.3) |
| Operating-system entropy (`getrandom`, `/dev/urandom`, kernel CRNG) | Deterministic **because the hypervisor makes the kernel's entropy inputs deterministic** — unmodified guest (§3.8) |
| `rdrand` instruction | Trapped in hardware and served a tape-seeded value (§3.2) |
| `rdseed` instruction | Rewritten out of the guest image at build time to a trap served a tape-seeded value (§3.8, §4) |
| `cpuid` | Fixed leaves via `KVM_SET_CPUID2`; nondeterministic feature bits cleared (§3.2) |
| Interrupt timing | Injected at an exact instruction boundary (§3.4) |
| External input / I/O | Served from the tape via the tape device (§3.5) |
| Memory | Zeroed RAM at fixed addresses; `nokaslr` |
| Any unmodeled VM exit | Fails loud (`DeterminismHole`) (§3.6) |

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

### 3.2 CPUID and the random instructions

- Under VT-x, `cpuid` always exits — the VMM owns every leaf via `KVM_SET_CPUID2` (start from
  `KVM_GET_SUPPORTED_CPUID`, then edit).
- **Mask table**: clear RDRAND `01H:ECX[30]`, RDSEED `07H:EBX[18]`, TSX HLE/RTM `07H:EBX[4]/[11]`, x2APIC
  `01H:ECX[21]`; pin topology leaves `0BH/1FH` to one core; set invariant-TSC `80000007H:EDX[8]`; set a fixed
  hypervisor-present bit. Masking makes every CPUID-gating library skip the raw instruction (essentially all
  real crypto/runtime code checks these bits before using `rdrand`/`rdseed`).
- **`rdrand` — hardware-trapped.** Clearing RDRAND in the guest CPUID makes stock KVM set
  `SECONDARY_EXEC_RDRAND_EXITING`, so `rdrand` already exits to the VMM. baud patches only the exit-handler
  table entry (no execution-control change) to serve a tape-seeded value: a `SplitMix64` draw on a sub-stream
  seeded from `blake3(tape)` — separate from the tape device's guest-facing cursor, its state carried across
  snapshot/restore in `ClockState::entropy_state` — written into the instruction-encoded destination GPR with
  `RFLAGS.CF = 1` and `OF/SF/ZF/AF/PF` cleared (baud's `rdrand` always succeeds). A guest that ignores the
  CPUID mask and issues raw `rdrand` still receives a deterministic value.
- **`rdseed` — rewritten at build time (§3.8, §4).** The current host cannot trap `rdseed` in hardware (its
  VMX exit control is not exposed here — §3.8); the build-time image rewrite (§4) removes it and the VMM
  serves the resulting `#UD` a tape-seeded value.
- **Test** (`cpuid_leaves_are_fixed`): every served leaf identical across two runs; RDRAND/RDSEED/TSX/x2APIC
  bits read 0.
- **Test** (`rdrand_guest_is_deterministic`): a guest that ignores the mask and issues raw `rdrand` runs
  *past* it on the served value, and two boots echo byte-identical result bytes. (The earlier assumption that
  the raw instruction should be treated as a divergence was falsified on real hardware — see
  `crates/baud-multiverse/tests/fixtures/rdrand-guest/BUILD.md`.)

### 3.3 Time — a work-clock

- **Spec**: the guest's time is a function of work done. Count **retired conditional branches** with
  `perf_event_open` on the vCPU thread (guest-filtered); virtual timestamp = `base + k × branch_count`; feed
  that into every time source. Raw retired-instruction count is forbidden (it double-counts faults and
  interrupts).
- **`rdtsc`/`rdtscp` — hardware-trapped.** Every `rdtsc` exits to the VMM (the patched `kvm_intel` sets
  `CPU_BASED_RDTSC_EXITING`, a VM-execution control stock KVM does not expose to userspace) and is served the
  work-clock value — bit-exact and work-proportional, not wall-clock-affine. `KVM_SET_TSC_KHZ` pins the
  reported frequency for consistency.
- **MSR trapping**: `KVM_X86_SET_MSR_FILTER` routes `IA32_TSC (0x10)`, `IA32_TSC_AUX`, `IA32_TSC_DEADLINE`
  to the VMM; serve virtual values. Delete HPET/PIT/PM-timer/RTC entirely.
- **Test** (`work_clock_is_monotone_and_reproducible`): a guest that reads the timestamp N times yields a
  non-decreasing sequence, byte-identical across a double-run (full equality — the trapped path returns the
  work-clock value, not a host-affine one).
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
- Deterministic interrupt timing is also what makes the guest kernel's *interrupt-entropy* mixing
  reproducible (§3.8).
- **Test** (`timer_tick_lands_at_identical_instruction`): drive a timer-interrupt guest with many ticks from
  the same tape twice; assert the injection tuple (PC + branch count) is identical at every tick across the
  two runs.

### 3.5 Deterministic I/O — the tape device

- **Spec**: the guest does all input/output through one paravirtual device served over PIO/MMIO exits. No
  real disks, network, DMA, or host interrupts. Reads return the next tape bytes (external input, simulated
  device responses); writes hand data out (log lines, probe values, `reached goal` / `invariant violated`
  markers) and issue control requests (`checkpoint here` = branch point). Full detail in
  `specs/baud-tape-device.md`.
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
  count at a fixed PC is identical. On failure the host/CPU is rejected and the result recorded in
  `docs/determinism.md`.
- **Spec (divergence, not perfection)**: a rare hardware miscount must be detected, never assumed away —
  every run supports a double-run stream-hash comparison; a mismatch marks the run `divergent` and excludes
  it from replay/branch/shrink.
- **Test** (`divergence_is_detected_and_reported`): inject a synthetic one-step observation difference;
  assert the comparator reports the first divergent step (node/PC/probe) and the run is marked divergent.

### 3.8 Randomness and entropy

- baud makes the guest's randomness deterministic by controlling the **inputs** the guest's entropy sources
  read from — at the machine layer, on an **unmodified guest kernel**. On a modern kernel (5.17+, the
  post-rewrite ChaCha20 CRNG) the per-request output path (`crng_make_state` → `crng_fast_key_erasure`) is
  pure ChaCha20 keystream from the CRNG key, with **no per-request `rdrand`/`rdseed` fold-in** (the old
  `state[14] ^= rdrand` line was deleted in the 5.17 rewrite). Entropy enters **only** through
  seeding/reseeding — `extract_entropy` BLAKE2s-hashes the collected inputs into the next key. So if baud
  makes every seeding input deterministic, `getrandom`, `/dev/urandom`, and the whole CRNG become a pure
  function of the tape **without touching the guest**. The complete set of seeding inputs, and how baud pins
  each:
  - **Boot RNG seed** — baud constructs the guest's `boot_params`, so it owns the one boot-seed path that
    applies to an x86_64 **direct kernel boot**: the `SETUP_RNG_SEED` (type 9) `setup_data` node, which the
    kernel mixes via `add_bootloader_randomness()` (then zeroes). baud writes a **fixed tape-derived 32-byte
    seed** there (or omits the node); it must never pass a firmware/host-derived seed through. Crediting a
    deterministic boot seed also makes the CRNG *ready early*, so the blocking jitter path (below) never runs.
    The EFI `EFI_RNG_PROTOCOL` seed and the device-tree `/chosen/rng-seed` are **not reachable on x86
    direct-boot without UEFI/DT**, so they do not apply here (they would matter only under an OVMF/UEFI boot).
  - **`rdtsc` / `rdrand`** — hardware-trapped and served tape-derived values (§3.2, §3.3), so the kernel's
    `arch_get_random_*` seeding reads and every `random_get_entropy()` (RDTSC) read are deterministic.
  - **Jitter dance** (`try_to_generate_entropy`) — a CPU-timing entropy collector on the `getrandom()`
    blocking path. It is deterministic under baud's already-deterministic TSC, and in practice never even runs
    because the pinned boot seed makes the CRNG ready before any caller blocks.
  - **Interrupt timing** (`add_interrupt_randomness`) — folds `random_get_entropy()` + IP/IRQ into the pool
    and credits it. This drives the **initial** CRNG init (EMPTY→EARLY→READY), not just later reseeds, so
    baud's exact-boundary interrupt injection (§3.4) plus the deterministic TSC make the initial CRNG state
    reproducible, not only the steady state.
  - **virtio-rng / hwrng** — the kernel's `hwrng_fillfn` kthread reseeds from a virtio-rng device for the
    whole uptime, credited at full quality. baud either **omits the device**, or backs it with an
    **ever-ready deterministic byte source** — a named FIFO (or `rng-egd`) fed from the tape, **not** a plain
    file (a file backend returns bytes once then EOF, starving the reseed loop rather than making it
    deterministic).
  - **`rdseed`** — cannot be hardware-trapped on the current host (host-capability note below), so every
    `rdseed` opcode in the guest image (kernel **and** userspace binaries) is **rewritten at build time** to a
    trap served a tape-seeded value (§4). With that pass, no `rdseed` reaches real entropy.
  - **vDSO `getrandom`** (kernel 6.11+ / glibc 2.41+) — userspace runs ChaCha20 itself, but its key still
    comes from a real `getrandom()` syscall with **no added userspace entropy**, so it is deterministic for
    free once the syscall path is; baud needs no guest change for it.
- **Why not just set kernel knobs.** `random.trust_cpu`, `random.trust_bootloader`, `clearcpuid`, and the
  `CONFIG_RANDOM_TRUST_*` options gate only whether an input is **credited**, never whether it is **mixed** —
  the bytes still stir the pool regardless — so no boot-param/config combination alone yields a deterministic
  CRNG. That is exactly why a config-only approach has to fall back to **patching `random.c`**, which means
  shipping a modified guest kernel (a per-version fork that breaks any guest the user brings and cannot work
  on a closed/signed image). baud rejects both: because the CRNG is already a pure function of its seed
  material, controlling the inputs from the layer baud already owns (TSC, interrupts, `rdrand`, `boot_params`,
  virtio) is strictly more powerful and needs **zero guest-kernel changes**.
- **Coverage**: almost all software gets randomness through the OS; the few libraries that issue raw
  `rdrand`/`rdseed` gate on CPUID first (OpenSSL, BoringSSL, LibreSSL, and the kernel all check the feature
  bit or route through `getrandom`), so the masked CPUID (§3.2) stops them. The build-time rewrite closes the
  one remaining sliver — raw, un-gated `rdseed` from code that both skips CPUID *and* prefers `rdseed` over
  `rdrand`, which is absent from every major library surveyed. Nothing is forbidden and nothing needs to
  cooperate. (An NES emulator, for instance, touches no entropy source at all.)
- **Host-capability note (mechanism, not policy)**: `rdseed` can be trapped in hardware where the host VMX
  exposes the RDSEED-exiting secondary control (`IA32_VMX_PROCBASED_CTLS2`, MSR `0x48B`, allowed-1 bit **48** =
  secondary control bit 16). Bare-metal Intel (including this machine's Tiger Lake) exposes it; the current
  WSL2 host does not — its Hyper-V L0 synthesizes the VMX capability MSRs and masks that control under nested
  virtualization (per the host probe, RDSEED-exiting is not settable, while RDRAND-exiting — MSR `0x48B` bit
  **43** = secondary bit 11 — **is** settable, which is what makes `rdrand` hardware-trapping work here). It is
  an L0 nested-virt mask, not a CPU limitation or microcode (bare-metal Tiger Lake supports it; corroborated
  by the QEMU/Azure report that Hyper-V hosts expose RDSEED in CPUID but omit RDSEED-exiting). Re-measure on
  any host with `rdmsr -f 48:48 0x48B` (RDSEED-exiting) and `rdmsr -f 43:43 0x48B` (RDRAND-exiting). baud uses
  the build-time rewrite for `rdseed` here; on a host that exposes the control it may additionally trap
  `rdseed` like `rdrand`, and the rewrite becomes belt-and-suspenders. The guest runs deterministically
  either way.
- **Test** (`entropy_guest_is_deterministic`): a guest that reads `getrandom`/`/dev/urandom` N times — via
  both the syscall and, on glibc 2.41+, the vDSO path — produces a byte-identical sequence across two boots on
  the same tape.
- **Test** (`initial_crng_state_is_reproducible`): the CRNG's first `getrandom` output after boot is identical
  across two boots — exercises the pinned `SETUP_RNG_SEED` seed plus deterministic interrupt/TSC seeding.
- **Test** (`virtio_rng_reseed_is_deterministic`): with a tape-fed virtio-rng source (or none), continuous
  reseeding does not perturb the output stream across a double-run.
- **Test** (`no_rdseed_opcode_survives_in_image`): after the build-time rewrite, a **decoder-based** scan
  (Capstone over `SHF_EXECINSTR` sections, not a byte grep) finds zero `rdseed` opcodes; a guest whose source
  contained a raw `rdseed` is double-run identical.

### 3.9 AMD (deferred)

- **Spec**: AMD configures CPUID/TSC intercepts through the VMCB and scales TSC via a ratio MSR; whether it
  exposes an RDTSC-exiting equivalent must be confirmed against the AMD virtualization manual before use.
  Intel-first; AMD is a later phase.
- **Test** (`amd_host_is_deferred`): on an AMD host, baud returns exit `1` with "AMD support deferred
  (Intel-first)" rather than running with unverified intercepts.

## 4. Guests and workloads (the contract is on the image)

- A workload is a **bootable guest image**: a small Linux (or unikernel) + the software under test + a tiny
  in-guest agent that speaks to the tape device.
- Threads, dynamic linking, multiple processes, arbitrary binaries are all supported — determinism is at the
  machine layer, and the guest kernel is unmodified (§3.8).
- **Spec (image contract)**: the guest routes external input through the tape device (a boot-time shim /
  small driver) and carries no real hardware timers baud did not model; and `baud-packages` applies the
  build-time **`rdseed`→trap rewrite** to every executable section (kernel + userspace). Entropy determinism
  is the hypervisor's job (§3.8), not the image's — nothing in the guest's RNG logic is changed.
- **The `rdseed` rewrite pass (real instructions)**: `baud-packages` disassembles each `SHF_EXECINSTR`
  section with a real decoder (Capstone; match `X86_INS_RDSEED` with `ModRM.reg = 7` and `mod = 11b` — never
  byte-grep `0F C7`, which is a group opcode where `/6` is RDRAND and `/1` is CMPXCHG8B/16B). Each `rdseed` is
  overwritten **in place, length-preserving**: `RDSEED r32` (`0F C7 /7`, 3 bytes) → `UD2` (`0F 0B`) + `NOP`
  (`90`); `RDSEED r64` (`48 0F C7 /7`, 4 bytes) → `UD2` + two `NOP`s. Because no bytes shift, all addresses,
  jump targets, and relocations stay intact and no rewriting framework is needed. The pass scans only bytes
  decoded from real instruction boundaries (never `.rodata`/`.data`) and re-decodes after patching to confirm
  the stream is still valid.
- **Serving the trap (real instructions)**: a `UD2` raises `#UD`, which by default the **guest's own** IDT
  vector-6 handler would swallow — the VMM would never see it. So `baud-multiverse` sets **bit 6 in the VMCS
  exception bitmap**, making `#UD` cause a VM-exit to the VMM. On that exit the VMM (1) confirms the faulting
  RIP is a rewritten `UD2`+`NOP` site (otherwise it re-injects a genuine `#UD`, so real invalid-opcode
  handling is untouched), (2) writes a tape-seeded value into the decoded destination GPR with `RDSEED` flag
  semantics (`CF = 1` + value and `OF/SF/ZF/AF/PF` cleared — baud always succeeds), and (3) advances RIP past
  the rewritten sequence. The value comes from the same tape-seeded entropy sub-stream as `rdrand` (§3.2),
  its state carried across snapshot/restore.
- **Fallback for un-enumerable code**: guests with JIT / self-modifying code whose `rdseed` sites cannot be
  found at build time run under a whole-guest **QEMU-TCG + `icount` + record/replay** path (~10× slowdown, no
  hardware virtualization), recorded in `docs/determinism.md`. This is rarely needed — baud's target workloads
  (e.g. the NES emulator, §11) execute no entropy instructions at all.
- **Test** (`image_lint_requires_tape_driver`): an image without the tape-device input path, or with a real
  RTC/HPET enabled, fails `baud image lint` with a specific reason.
- **Test** (`image_rewrites_rdseed`): `baud image build` rewrites every `rdseed` opcode; a follow-up scan
  (`no_rdseed_opcode_survives_in_image`) finds none.
- **`baud-packages` builds guest images** reproducibly with pinned Nix (kernel + rootfs + agent), applies the
  rewrite pass, and warms them into the snapshot store; the image hash is the environmental identity. Full
  detail in `specs/baud-packages.md`.

## 5. Snapshot-branch multiverse (replaces replay-from-zero)

Full detail in `specs/baud-snapshot.md` (capture/restore/branch) and `specs/baud-snapshot-store.md`
(durable tree).

- **Capture a universe** = guest RAM + full vCPU state (`KVM_GET_REGS/SREGS/MSRS/LAPIC/XSAVE2/XCRS/
  VCPU_EVENTS/MP_STATE`) + VM clock/TSC (`KVM_GET_CLOCK`, `KVM_GET_TSC_KHZ`) + tape-device cursor +
  entropy-substream state + console state. Omitting a field diverges the restored universe.
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
  agent, partly in the VMM. `baud-proto` carries the hypercall/tape-device probe + outcome messages.
- **Two-plane cross-check** survives: the VMM exit log is plane 1; an in-guest audit or a second independent
  counter is plane 2; disagreement fails the run.
- **Test** (`planes_agree_on_healthy_run`): plane 1 and plane 2 exit sequences agree; a deliberately broken
  VMM build fails the cross-check.

## 8. Crate map

- **`baud-multiverse`** — the deterministic VMM (KVM setup, run loop, CPUID/TSC/MSR control, the
  interrupt-injection engine, the tape device, snapshot hooks, the `rdrand` serve path). `specs/baud-multiverse.md`.
- **`baud-vcpu`** — the single-vCPU state machine and exit dispatch. `specs/baud-vcpu.md`.
- **`baud-snapshot`** — universe capture/restore, userfaultfd CoW branching, dirty-ring reset, the branch
  tree. `specs/baud-snapshot.md`.
- **`baud-snapshot-store`** — content-addressed durable universes + tapes + tree; age-encrypted at rest.
  `specs/baud-snapshot-store.md`. (Supersedes `baud-journal`.)
- **`baud-tape-device`** — the paravirtual device model + guest-side driver contract. `specs/baud-tape-device.md`.
- **`baud-host`** — the KVM-capable host manager: fleet of single-vCPU VMs, core pinning, capacity
  accounting, `host probe`. `specs/baud-host.md`.
- **`baud-packages`** — builds reproducible guest images, applies the `rdseed`-rewrite pass, warms the store.
  `specs/baud-packages.md`.
- **`baud-driver`** — tape/fuzzing engine + snapshot-tree exploration. `specs/baud-driver.md`.
- **`baud-proto`** — wire types incl. hypercall/tape-device probe + outcome messages. `specs/baud-proto.md`.
- **`baud-server`, `baud-cli`** — orchestration + command surface; adds `snapshot`/`branch`/`rewind`/
  `shell-into`/`host`/`image`/`stream` verbs.
- **`baud-tracing`, `baud-stream`, `baud-secret`, `baud-identity`, `baud-keys`** — carry over; `baud-stream`
  captures and renders the guest framebuffer (a whole OS runs, so a real display exists — §11).
- **Targets** (`baud-raftlet`, the NES emulator for §11, parser) become **guest images** under `examples/`,
  not in-tree simulations.

## 9. Infrastructure (`infra/`) — the host substrate

- **Managed containers are out for the VMM** — no `/dev/kvm`. baud-multiverse runs only on hosts you control
  with real VT-x.
- **Substrate**: bare-metal instances (best determinism/latency, and the only place `rdseed` is
  hardware-trappable) or nested-virtualization-enabled hosts (Intel; WSL2 on a bare-metal box is the current
  dev host). Verify `grep vmx /proc/cpuinfo` and, if nested, `kvm_intel nested=1`.
- **Fleet — one physical core per VM**: pin each vCPU thread (emulator/IO threads off the isolated cores),
  isolate cores (`isolcpus`/cpuset + `nohz_full` + `rcu_nocbs`), NUMA-local memory, **SMT disabled** (or
  both siblings in one VM — siblings share cache and leak). Budget ~28–30 VMs per 32-core host, ~56–60 per
  64-core, minus housekeeping.
- **Test** (`capacity_refuses_sibling_split`): the host manager never places two VMs on hyperthread siblings;
  a placement attempt that would is rejected.
- **`infra/nixos-modules/baud-host.nix`** provisions such a host (libvirtd/direct KVM, `kvm-intel`,
  isolation kernel params, pinning, `nested=1` when itself nested). `infra/machines/` composes bare-metal and
  nested-VM host definitions.
- **Developer machine**: a bare-metal Dell XPS 13 9310 (Intel Tiger Lake) running **WSL2 Ubuntu**, where
  `/dev/kvm` is available natively; run the build agent inside WSL2 (see `CLAUDE.md`). `rdseed`-exiting is
  masked by Hyper-V here, so `rdseed` is handled by the build-time rewrite (§3.8); everything else is
  hardware-trapped.
- **Test** (`doctor_checks_kvm`): `baud doctor` in WSL2 asserts `/dev/kvm` present, VT-x exposed, and the
  branch counter deterministic, and reports the host's capabilities (including whether `rdseed`-exiting is
  available: `rdmsr -f 48:48 0x48B`).
- `infra/secrets` (multi-recipient sops) and `infra/pkgs` (the guest-image builder + `rdseed`-rewrite pass)
  carry over.

## 10. Milestones (each ends with a drive script and named tests)

- **H0 — capability spike (must actually run).** In WSL2 (and on any bare-metal host), probe `/dev/kvm`,
  VT-x, `KVM_SET_CPUID2`, TSC-khz/offset control, `KVM_X86_SET_MSR_FILTER`, `KVM_SET_GUEST_DEBUG`
  single-step, branch-counter determinism (`rcb_is_deterministic_on_this_cpu`), host TSC stability
  (`host_tsc_is_stable`), and whether `rdseed`-exiting is available (`rdmsr -f 48:48 0x48B`). Record results
  in `docs/determinism.md`. Drive `drive/h0.sh`: `baud host probe --json` asserts each capability; a failing
  capability is recorded, never hidden.
- **H1 — boot a guest.** The run loop boots a minimal guest kernel that prints to the serial console; clean
  `Hlt`/`Shutdown`. Drive `drive/h1.sh`: boot the hello image, assert expected console output;
  `double_boot_memory_identical` passes.
- **H2 — deterministic double-run.** Same image + tape twice ⇒ byte-identical observation stream
  (console + probes + final memory hash), CPUID masked, work-clock TSC. Drive `drive/h2.sh`:
  `cpuid_leaves_are_fixed`, `work_clock_is_monotone_and_reproducible`, `all_input_is_tape_derived`,
  `no_unmodeled_exit_is_silent`.
- **H3 — randomness + time control.** OS entropy is a function of the tape (the kernel's seeding inputs are
  hypervisor-controlled — pinned `SETUP_RNG_SEED`, trapped `rdtsc`/`rdrand`, deterministic interrupts, tape-fed
  or omitted virtio-rng; `rdseed` rewritten); timestamps are the work-clock; a guest reading
  `getrandom`/`/dev/urandom` and issuing raw `rdrand` is double-run identical. Drive `drive/h3.sh`:
  `entropy_guest_is_deterministic`, `initial_crng_state_is_reproducible`, `virtio_rng_reseed_is_deterministic`,
  `rdrand_guest_is_deterministic`, `no_rdseed_opcode_survives_in_image`.
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
  snapshot-store reconstruction/shrinking, the framebuffer stream (§11), a distributed target as a **guest
  image** reaching a planted safety violation via guided branch search (`planes_agree_on_healthy_run`, the
  planted-bug interleaving test), and the **Super Mario Bros validation** (§11) as the flagship end-to-end
  proof.

## 11. Super Mario Bros — arbitrary-system validation

The flagship end-to-end proof that baud can drive a black-box, chaotic, history-dependent interactive system
to a goal state purely by exploring the tape — the same capability that finds bugs in real systems. baud
makes a headless NES emulator, running as a guest under the VMM, **complete Super Mario Bros** from
controller input alone, and shows the emulator live in a window on the desktop.

### 11.1 Guest and bridge

- **Guest image** (`examples/mario/`): a headless NES emulator built by `baud-packages` as a bootable guest
  image, started from a fixed savestate at the beginning of world 1-1 past the title screen (a baud design
  choice for a stable start, not a required detail).
- **ROM + savestate are user-supplied paths, never bundled** (copyright). CI uses a free homebrew ROM with a
  reduced completion goal.
- **Bridge (in-guest harness)**: an emulator script that, each 1/60 s frame, (1) reads one **controller byte**
  from the tape device, (2) applies it to joypad 1, (3) advances exactly one frame, (4) writes the probe
  values (§11.2) out the tape-device channel. The controller byte is `A | B<<1 | Select<<2 | Start<<3 |
  Up<<4 | Down<<5 | Left<<6 | Right<<7`. The emulator is itself deterministic (its own PRNG is seeded and
  captured), and touches no entropy instruction, so the whole guest is a pure function of the tape.

### 11.2 Probes

Read from NES RAM by the bridge each frame:

- `x = mem[0x006D] * 256 + mem[0x0086]` — global horizontal position (`0x0086` = on-screen x, `0x006D` =
  screen page). **Confirmed** against the SMB RAM map; the primary progress signal.
- `y = mem[0x00CE]` — on-screen vertical position. **Confirmed**; the grid's second dimension.
- **Progress / end-state probes — verify each address against a reference SMB RAM map before use.** Commonly
  cited but *not yet confirmed here*: world (commonly `0x075F`), level/area (commonly `0x075C`), a
  game-completed flag (address TBD — "past world 8-4" is the narrative end state, not a known address), and
  `lives`. `baud image lint` requires these addresses be pinned in `examples/mario/probes.toml` and validated
  against the reference map, never hard-coded on faith.

### 11.3 Strategy (progress = "how far into the game")

- **Objective**: maximize global `x` — the run is scored purely by how far right it has driven the character,
  because farther right is farther into the game and the victory state is the maximum `x` of the final world.
- **Nothing beyond the score belongs here.** The strategy is only this progress score over the §11.2 probes.
  The exploration that spends the score — which prefixes to keep, which to extend — is baud's existing tape
  engine (§6, `baud-driver`), reused unchanged; the Mario example adds none of its own. If a run appears to
  need exploration behaviour the driver lacks, add it to the driver so every workload inherits it — never
  encode workload-specific search here.
- **Goal**: reach the game's victory / end state (the completion probe of §11.2).

### 11.4 Tactics (input distribution)

- **Sticky flip-mask**: `next_byte = prev_byte XOR low_probability_mask` — each controller bit flips with a
  small per-frame probability (tuned), so buttons stay held across many frames. Correlate input across frames;
  never draw a fresh independent byte per frame. A jump needs A held for ~30–100 frames, which independent
  50%-per-frame input reaches with probability `1/2^N` (≈ 0 for a long jump) — so uncorrelated input clears no
  jump, while flipping a bit only to start or stop pressing makes long holds common.
- **Pure per-frame random** is kept only as a negative control — its reachable-position heat-map decays fast
  near the spawn.

### 11.5 Why this validates baud

- **Completion is an instrumental objective — the real goal is finding bugs.** Beating the game is not the
  point; it is the proof that the tape engine explores a state space effectively, and that same capacity for
  efficient state-space exploration is what finds bugs in real systems. Reaching completion requires getting
  "sequentially lucky" hundreds of frames in a row — strictly harder than the 2–4-step interleavings of a
  typical distributed-systems bug — so a system that can beat the game can explore anything.
- **Unknown-unknowns.** The same exploration surfaces emergent glitches no scripted test would write a check
  for — e.g. a spot where the character gets stuck and **clips through a wall**. baud's invariant/goal probes
  (§7) flag these as anomalies, not just the win condition.
- **Non-fragility (a required test, not a nicety).** The **unchanged** setup — zero re-tuning — must also make
  progress on a much harder hand-authored ROM variant. A pre-recorded input tape would desync instantly on a
  changed ROM; baud's exploration adapts because nothing is scripted. `drive/mario.sh` runs this variant as a
  second, non-gating case.

### 11.6 Acceptance test (`drive/mario.sh`)

1. `baud image build examples/mario` (applies the `rdseed`-rewrite pass — a no-op here, the emulator has
   none) and `baud image lint` (verifies the probe addresses of §11.2).
2. `baud verify determinism` — the same tape twice yields an identical probe stream and identical framebuffer
   hashes.
3. **Negative control**: `baud run --tactics random` — positions plateau near spawn (the fast-decaying
   heat-map), checked via `baud obs`.
4. **Main run**: `baud run --strategy examples/mario/strategy.toml --tactics sticky-mask` — the max-`x` score
   climbs run over run until the character reaches the victory state.
5. Mid-run `baud tape kill` + `baud tape reconstruct` + resume — proves journal-free reconstruction on this
   workload.
6. Terminates with `GoalReached` on the completion probe (§11.2); the winning tape is journaled and exported
   as a replayable input movie.
7. `baud shrink` → `baud replay` of the shrunk tape still completes.
8. **Non-fragility case (non-gating)**: the same command on a harder hand-authored ROM variant still makes
   progress and, if it gets stuck, reports the anomaly (§11.5) rather than crashing the harness.
- **Release gate**: full completion on the base ROM. **CI variant**: until the completion-probe address is
  verified (§11.2), CI gates on `x` progress past the first world within budget.

### 11.7 Live display (mandatory, every run)

- `baud-stream` captures the emulator framebuffer. The frames are a **derived artifact of the tape**: replay
  the tape through the emulator to regenerate identical frames on demand (fits `baud-stream`'s render-on-
  demand — no video is stored, only the tape).
- Every Super Mario Bros run renders a live window on the **Windows desktop via WSLg** (a Linux viewer
  launched from WSL2 appears as an ordinary Windows window), sized to **~25% of the screen** and updated live:
  ```bash
  # ~25% area = half width × half height; NES 256:240 aspect preserved
  SW=$(powershell.exe -NoProfile -Command '[System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width' | tr -d '\r')
  XW=$((SW/2)); YH=$((XW*240/256))
  baud stream tail --run "$RUN" --format y4m \
    | ffplay -f yuv4mpegpipe -i - -an -framedrop -infbuf \
        -x "$XW" -y "$YH" -left 40 -top 40 -window_title "baud: mario"
  ```
  (A small in-crate SDL viewer is the alternative to `ffplay`, with identical placement/sizing.) Concurrent
  runs render as separate tiled WSLg windows — one per branch being watched.
- **Test** (`mario_stream_window_is_live`): during a run, `baud stream tail` produces frames at ~60 fps and a
  re-render from the tape is byte-identical to the live frames (derived-artifact property).

## 12. Problem → specification → test matrix

Every risk found in review, the guarantee it becomes, and the test that proves it.

| # | Problem | Specification (what must be built/guaranteed) | Test |
|---|---------|-----------------------------------------------|------|
| 1 | Stock KVM won't force RDTSC/random-instruction exiting from userspace | Patched `kvm_intel` forces RDTSC-exiting (served the work-clock) and re-routes the `rdrand` exit stock KVM already takes to serve a tape-seeded value; `rdseed` is rewritten at build time (host can't trap it) | `work_clock_is_monotone_and_reproducible`; `rdrand_guest_is_deterministic`; `no_rdseed_opcode_survives_in_image` |
| 2 | Branch counter is nondeterministic on some CPUs | Validate on deploy silicon at H0; reject on failure | `rcb_is_deterministic_on_this_cpu` (H0 gate) |
| 3 | Raw instruction count double-counts faults/interrupts | Forbid raw count; use RCB + PC + registers + stack checksum to name a point | `timer_tick_lands_at_identical_instruction` |
| 4 | PMU interrupts are delivered late/imprecisely (skid) | Arm-early-then-single-step to the exact boundary | `timer_tick_lands_at_identical_instruction` |
| 5 | Rare (~1e-12) counter miscount | Detect divergence, never assume perfection; double-run comparator | `divergence_is_detected_and_reported` |
| 6 | `/dev/kvm` absent in managed containers | KVM-capable host substrate; `host probe` gate | `doctor_checks_kvm`; H0 `baud host probe` |
| 7 | SMT siblings leak/jitter and add no deterministic capacity | SMT off or siblings same-VM; placement refuses splits | `capacity_refuses_sibling_split` |
| 8 | Restore is host-locked (CPU model/kernel) | Same model or CPUID template; refuse mismatch | `restore_refuses_mismatched_cpu` |
| 9 | A snapshot missing any state field diverges | Enumerated capture set (RAM + all vCPU + clock + entropy substream + device) | `snapshot_roundtrip_is_bit_identical` |
| 10 | TSC restore ordering (khz before vCPU; TSC before deadline) | Ordered restore sequence | `restore_refuses_mismatched_cpu` (timer resumes clean) |
| 11 | Uninitialized memory is a determinism leak | Zeroed memory at fixed addresses; `nokaslr` | `double_boot_memory_identical` |
| 12 | An unhandled VM exit silently continues | Catch-all returns `Err(DeterminismHole)` | `no_unmodeled_exit_is_silent` |
| 13 | Host TSC instability (`KVM_GET_TSC_KHZ` = -EIO) | Pin constant-/invariant-TSC host; reject unstable | `host_tsc_is_stable` (H0) |
| 14 | Making guest RNG deterministic without modifying the guest | Modern CRNG output is a pure function of its seed material; the hypervisor pins every seeding input — fixed `SETUP_RNG_SEED` boot seed, trapped `rdtsc`/`rdrand`, rewritten `rdseed`, deterministic interrupt timing, tape-fed-or-omitted virtio-rng; vDSO getrandom rides on syscall determinism | `entropy_guest_is_deterministic`; `initial_crng_state_is_reproducible`; `virtio_rng_reseed_is_deterministic` |
| 15 | Raw un-gated `rdseed` bypasses CPUID masking | Build-time `rdseed`→`UD2`(+`NOP`) rewrite of all executable sections; stock KVM already traps `#UD` unconditionally (no VMCS exception-bitmap change needed) and the enforced `kvm_intel.ko` (`ud2-enforce.patch`) forwards a confirmed rewritten site as `KVM_EXIT_BAUD_DETERMINISM`, verifies the site, serves a tape value, advances RIP; an unregistered `UD2` re-injects `#UD` verbatim | `no_rdseed_opcode_survives_in_image`; `image_rewrites_rdseed`; `rdseed_enforced_regime_is_bit_exact_across_boots`; `ud2_outside_the_rdseed_site_table_reinjects_ud` |
| 16 | Branch-point residual nondeterminism (RDRAND/TSC/wall-clock) | VMM serves them deterministically so branches are bit-identical | `thousand_branches_are_independent_and_deterministic` |
| 17 | Dev host needs `/dev/kvm` | Run the agent in WSL2 on the bare-metal box; `/dev/kvm` native | `doctor_checks_kvm` |
| 18 | Multi-core guest determinism is unsolved cheaply | Single vCPU only; refuse >1 | `vm_creation_refuses_multiple_vcpus` |
| 19 | AMD intercept/TSC differences unverified | Intel-first; AMD deferred | `amd_host_is_deferred` |
| 20 | CPUID leaks core index / topology nondeterminism | Fixed CPUID leaves + topology pinned + affinity | `cpuid_leaves_are_fixed` |
| 21 | Input not actually flowing from the tape (fake determinism) | Tape device is the sole input; byte-sensitivity | `all_input_is_tape_derived` |
| 22 | Shrinking re-runs from zero (slow) | Fork from nearest snapshot | `shrink_reproduces_from_nearest_snapshot` |
| 23 | Journal/observations in plaintext at rest | `baud-snapshot-store` age-encrypts universes + tapes | `snapshot_store_bodies_are_ciphertext` |
| 24 | Two-plane cross-check is counts-only, misses ordering | Compare ordered exit sequences | `planes_agree_on_healthy_run` |
| 25 | `rdseed`-exiting unavailable under nested virt (WSL2) | An L0 Hyper-V mask, not a CPU limit — MSR `0x48B` bit 48 (RDSEED-exiting) absent while bit 43 (RDRAND-exiting) is present; handled by the build-time rewrite; both trappable on bare-metal Intel | H0 records both bits (`rdmsr -f 48:48`/`-f 43:43 0x48B`); `no_rdseed_opcode_survives_in_image` |

## 13. Migration map (from the current userspace plan)

- **Execution layer**: userspace ptrace/seccomp of one process → **KVM/VT-x VMM of a whole machine**.
  `baud-multiverse` is rewritten.
- **Guest contract**: single-threaded static musl process → **any OS + any software**; the contract moves to
  the **image build** (external input via the tape device + the `rdseed`-rewrite pass; the guest kernel is
  unmodified).
- **Randomness**: untrappable in userspace → **controlled at the machine layer** (CPUID masked, `rdtsc`/
  `rdrand` hardware-trapped, `rdseed` rewritten, the kernel's entropy inputs therefore deterministic).
- **Time**: `PR_SET_TSC` per-process → **VM-level work-clock** (`rdtsc` trapped) plus exact-boundary
  interrupt injection.
- **State model**: replay-from-zero journal (O(prefix) per reconstruct) → **snapshot-branch tree**
  (O(write-set) per branch). `baud-journal` → `baud-snapshot` / `baud-snapshot-store`.
- **Infra**: Daytona containers → **KVM-capable hosts**, one core per VM; dev host = WSL2 on a bare-metal
  Intel box.
- **Targets**: in-tree simulations → **guest images** under `examples/` (incl. the NES emulator, §11).
- **Cleanup owed to this rewrite**: remove any two-mode / "one KVM module vs stock" split from the code — the
  `Regime`-style enum and its branches in `baud-multiverse`/`baud-vcpu`/`baud-host`/`baud-snapshot-store` —
  since there is now one determinism model. `host probe` reports capabilities, not a mode.

## 14. Build status

The authoritative, blow-by-blow build log is **`ralph/progress.txt`** — this section is only a short current
snapshot, not a duplicate of it.

- **Built and hardware-tested** (details in `ralph/progress.txt`): `baud-host` (`host probe`), `baud-vcpu`
  (exit dispatch, single-step interrupt injection), `baud-multiverse` (KVM boot flow, CPUID mask, work-clock,
  console, tape bus, dirty-ring reset), `baud-tape-device`, `baud-snapshot` (capture/restore, userfaultfd
  branching, dirty-ring), `baud-snapshot-store`, `baud-packages` guest-image contract + `baud image lint` +
  the build-time `rdseed`→`UD2`(+`NOP`) rewrite pass (`crates/baud-packages/src/rdseed.rs`, wired end-to-end
  via `POST /image/rewrite-rdseed` and `baud image rewrite-rdseed <path>`), and the patched `kvm_intel`
  module that hardware-traps `rdtsc`/`rdrand` and serves them from the work-clock / tape **and** now serves
  the `rdseed`-rewrite's `UD2` trap too: `kernel-module/baud-enforced/ud2-enforce.patch` intercepts
  `handle_exception_nmi`'s existing `is_invalid_opcode` branch (stock KVM already traps `#UD` unconditionally
  for its own emulation fallback, so no exception-bitmap change was needed), forwards a confirmed rewritten
  site as `KVM_EXIT_BAUD_DETERMINISM`, and tail-calls the original `handle_ud` for anything else (so kernel
  `BUG()`/`WARN_ON()` and genuine invalid opcodes are untouched). `drive/h3-enforced-rdseed.sh` hardware-
  validated both halves on real `/dev/kvm`: `rdseed_enforced_regime_is_bit_exact_across_boots` (a registered
  site serves a tape-seeded value into the destination GPR, bit-exact across two boots) and
  `ud2_outside_the_rdseed_site_table_reinjects_ud` (an unregistered `UD2` re-injects `#UD` verbatim, never
  served a value), plus regression re-runs of the RDTSC/RDRAND enforced tests with all three patches layered
  on the same module. Stock module restores cleanly on exit every time. `rdseed`'s own rewrite pass is still
  only exercised against synthetic hand-built ELF fixtures (`crates/baud-packages` tests) plus the small
  hand-assembled `tests/fixtures/rdseed-guest/` boot fixture — not yet run against a real linked
  kernel/userspace ELF; do that before relying on it for a real guest image.
- **Next actions (this rewrite)**:
  1. **OS-entropy determinism** — pin `SETUP_RNG_SEED` (type 9) `setup_data` in the `boot_params` baud already
     builds; make virtio-rng tape-fed (an ever-ready FIFO, never a plain file) or omitted; confirm the
     deterministic-TSC + exact-interrupt seeding covers the initial CRNG state. Add
     `entropy_guest_is_deterministic`, `initial_crng_state_is_reproducible`,
     `virtio_rng_reseed_is_deterministic`. No guest-kernel patch. **Prerequisite gap found**: no fixture in
     the test suite boots a real Linux kernel yet (hello/rdrand/rdtsc/rdseed-guest are all hand-assembled
     non-Linux flat binaries with no IDT/scheduler); these tests cannot be written non-vacuously without
     first building a real-kernel guest image — a larger antecedent task than the phrasing here implies.
  2. **Model cleanup** — remove the two-mode `Regime` enum and its branches from
     `baud-multiverse`/`baud-vcpu`/`baud-host`/`baud-snapshot-store`; there is one determinism model
     (§13). `host probe` reports capabilities. (Note: the enforced/cooperative-style kernel-module swap-in
     dance itself stays necessary for RDTSC, RDRAND, and `#UD`/RDSEED — this cleanup is about collapsing the
     *reporting*/API-surface split, not about removing that mechanism. `enforced_module_present()`
     (`crates/baud-host/src/linux.rs`) still correctly returns `false` outside a `drive/h3-enforced-*.sh` swap
     window — wiring it to a real runtime check needs a new `KVM_CHECK_EXTENSION` the patches don't add yet.)
  3. **Super Mario Bros validation (§11)** — `examples/mario/` guest image + strategy/tactics + `drive/mario.sh`
     completion gate + the mandatory ~25% WSLg live window (`baud stream tail | ffplay`). The current
     `examples/mario/` predates the KVM pivot (a static-musl-process spec, not a bootable guest image) and
     will need to be rebuilt from scratch under the new model.
  4. **`RdseedRewriteReport` → boot wiring** — nothing yet plumbs a real `baud image build`'s rewrite-site
     table into `Multiverse::boot_with_rdseed_sites`; the enforced-RDSEED test hardcodes the hand-verified
     site of the fixed `tests/fixtures/rdseed-guest/` fixture. Needed before a real, non-fixture guest image
     can use enforced RDSEED end-to-end.
- **Specs to update alongside**: `specs/baud-multiverse.md` and `specs/README.md` (one model, entropy-by-
  input-control, `rdseed` rewrite), `specs/baud-packages.md` (rewrite pass — now implemented, update from
  planned to built), `specs/baud-host.md` (`host probe` reports capabilities), a new `specs/baud-stream.md`
  note on the WSLg live window.
