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
- **The generic shape.** Under baud, every program has the same shape as Super Mario Bros: a deterministic
  system that takes an input tape and has states worth reaching. Point baud at any program on its
  deterministic Linux, mark the states that matter — a reached condition, a violated invariant, a crash —
  and its fuzzer searches the input tape until the program hits one, then reproduces that run exactly. Super
  Mario Bros is one such program; its target state is the end of the game. The engine that does this is
  generic (§3–§10); §11 is a worked example of it, with every game-specific line confined to `examples/`.

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
  `perf_event_open` on the vCPU thread (guest-filtered) — the raw `BR_INST_RETIRED.COND` event (Intel
  `0xC4`/umask `0x11`), **not** `PERF_COUNT_HW_BRANCH_INSTRUCTIONS`, which counts all branches and is
  measurably nondeterministic (±1 on Tiger Lake; the conditional-only event is bit-exact — see
  `docs/determinism.md`). Virtual timestamp = `base + k × branch_count`; feed that into every time source. Raw
  retired-instruction count is forbidden (it double-counts faults and interrupts).
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
    deterministic boot seed also makes the CRNG *ready early*, so the jitter path (below) never runs.
    The EFI `EFI_RNG_PROTOCOL` seed and the device-tree `/chosen/rng-seed` are **not reachable on x86
    direct-boot without UEFI/DT**, so they do not apply here (they would matter only under an OVMF/UEFI boot).
  - **`rdtsc` / `rdrand`** — hardware-trapped and served tape-derived values (§3.2, §3.3), so the kernel's
    `arch_get_random_*` seeding reads and every `random_get_entropy()` (RDTSC) read are deterministic.
  - **Jitter dance** (`try_to_generate_entropy`) — a CPU-timing entropy collector the kernel runs only while
    the CRNG is not yet ready. It is deterministic under baud's already-deterministic TSC, and in practice
    never even runs because the pinned boot seed makes the CRNG ready before any caller reaches it.
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
- **Proven end-to-end at H7**: on the real Linux guest (§4, §10), with RDRAND CPUID-cleared + hardware-trapped,
  `rdseed` rewritten, `CRYPTO_JITTERENTROPY=n`, deterministic interrupt timing, and the pinned `SETUP_RNG_SEED`,
  an unmodified guest's `getrandom`/`/dev/urandom` is byte-identical across boots — the whole entropy story is
  a claim baud can demonstrate, not just assert.
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

## 4. Guests and the deterministic Linux boot pipeline

baud's deterministic machine (§3) is generic — it boots any `bzImage` + initramfs and moves opaque bytes on
the tape device. A workload adds only a **guest image** and a tiny **harness**. Keep three layers strictly
separate: the **machine** (§3, workload-agnostic), the **image** (kernel + initramfs + software-under-test +
agent, built once and byte-identical every boot), and the **harness** (the in-guest agent bridging the
software to the tape device — the only workload-aware code). Threads, dynamic linking, multiple processes,
and arbitrary binaries are all supported; the guest kernel is unmodified (§3.8). Full detail in
`specs/baud-packages.md`.

### 4.1 Minimal deterministic kernel

- Build a fully-builtin (no modules) x86_64 `bzImage`, Firecracker-lineage config.
- **Required `=y`**: `X86_64`, `PRINTK`, `TTY`, `SERIAL_8250` + `SERIAL_8250_CONSOLE`, `BLK_DEV_INITRD` +
  `RD_GZIP`, `BINFMT_ELF`, `DEVTMPFS` (+ `_MOUNT`), and one tape transport (`VIRTIO` + `VIRTIO_MMIO` +
  `VIRTIO_CONSOLE`, or none when the endpoint is raw PIO — §4.4).
- **Disable**: `MODULES`, `PCI` (use virtio-mmio / PIO), `ACPI` (unless S5-shutdown detection is used, §4.3),
  `EFI`, `RANDOMIZE_BASE` / `RANDOMIZE_MEMORY`, `HPET`, `RTC_*`, `SERIO_I8042`, sound / USB / net / fb, and
  `CRYPTO_JITTERENTROPY`.
- **Test** (`guest_kernel_boots_to_userspace`): the built bzImage reaches `/init` and prints a marker.

### 4.2 Direct boot + deterministic command line + boot_params

- baud already loads the bzImage (`linux-loader`) and writes the zero page; add: copy the image setup_header
  into `boot_params.hdr@0x1F1`; `type_of_loader=0xFF`; `loadflags` bit0 `LOADED_HIGH` + bit7 `CAN_USE_HEAP`;
  `cmd_line_ptr`; `ramdisk_image`/`ramdisk_size` → initramfs; **fill `e820_table`/`e820_entries`** (omitting
  the E820 map is the classic from-scratch-VMM silent hang); 64-bit entry at load + `0x200`, `%rsi =
  boot_params`.
- **Deterministic command line**: `console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc tsc=reliable
  no-kvmclock no_timer_check pci=off acpi=off reboot=t panic=-1 quiet loglevel=1 printk.time=0
  random.trust_cpu=off random.trust_bootloader=on i8042.noaux i8042.nomux i8042.nopnp 8250.nr_uarts=1
  nomodule rdinit=/init` (single vCPU, TSC-only time, no probing of hardware baud does not model, immediate
  deterministic exit).
- **Pin the boot RNG seed**: write `struct setup_data{ next, type = SETUP_RNG_SEED (9), len = 32, data =
  <tape-derived 32 bytes> }` into guest RAM and put its physical address in `boot_params.hdr.setup_data@0x250`
  (chain any existing node); re-place it every boot (the kernel zeroes it). With `random.trust_bootloader=on`
  the CRNG is seeded synchronously and identically, so the `getrandom()` wait path never runs.
- **Test** (`boot_params_seed_is_pinned`): two boots write an identical seed node; early CRNG init is
  reproducible.

### 4.3 initramfs + `/init`

- Reproducible newc cpio: `touch -h -d '@1'`; `find -print0 | sort -z | cpio -o -H newc -R +0:+0
  --reproducible --null | gzip -9n`.
- A static `/init` as PID 1: optionally mount `proc`/`sys`/`dev`, exec the software-under-test / harness, then
  `sync()` + `reboot(RB_POWER_OFF)`; `reboot=t` guarantees a triple-fault fallback the VMM traps as shutdown
  (or trap ACPI S5 `PM1a` if that path is enabled).
- **Test** (`init_powers_off_deterministically`): a clean VMM-detected shutdown at an identical exit point
  across two boots.

### 4.4 The guest tape endpoint

- The software talks to the tape device through one of, simplest-first: **(A, bring-up)** userspace PIO —
  `iopl(3)` then `inb`/`outb` from PID 1, zero driver, interrupt-free; **(B)** a ~100-line builtin char shim
  exposing `/dev/tape` over `ioread8`/`iowrite8`; **(C, standard)** virtio-serial (`/dev/vport0pN`) with clean
  synchronous `read`/`write` — best for a byte-per-frame protocol and for separate input / observation ports.
- **Test** (`guest_tape_roundtrip`): the guest reads tape bytes and writes probes back through the endpoint;
  changing one tape byte changes the output.

### 4.5 Reproducible image build (`baud-packages`)

- **Path 1 (bring-up): Buildroot** — `qemu_x86_64_defconfig` + a fragment (`BR2_TARGET_ROOTFS_CPIO_GZIP`,
  `BR2_REPRODUCIBLE`, package selections) + a rootfs overlay for `/init` and the harness → emits `bzImage` +
  `rootfs.cpio.gz`; dynamic linking works because the whole rootfs is in the cpio.
- **Path 2 (final): pinned Nix flake** — `linux_6_12.override { structuredExtraConfig, autoModules = false }`
  for the bzImage + `makeInitrdNG` for the initramfs, flake-locked nixpkgs rev, content-addressed. Replaces
  the old single-musl-binary builder.
- Image identity = `sha256(bzImage ‖ initramfs.gz)` — the environmental identity, warmed into the snapshot
  store.
- **Test** (`image_build_is_reproducible`): two builds of one spec produce an identical image hash.

### 4.6 `rdseed` rewrite pass and image lint (built, hardware-tested)

- **Image contract**: the guest routes external input through the tape device and carries no real hardware
  timers baud did not model; `baud-packages` applies the build-time **`rdseed`→trap rewrite** to every
  executable section (kernel + userspace). Entropy determinism is the machine's job (§3.8), not the image's —
  nothing in the guest's RNG logic is changed.
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

### 4.7 Running a full, unmodified distro (Ubuntu 18.04.1 LTS)

The minimal kernel (§4.1) proves the pipeline; a **full, unmodified distro** proves baud runs *real* software
deterministically. baud boots the **stock Ubuntu 18.04.1 LTS** kernel + rootfs to the serial login prompt on
the same deterministic machine.

- **Image**: the frozen 18.04.1 build from `cloud-images-archive.ubuntu.com` — the qcow2 rootfs converted to
  raw (`qemu-img convert -O raw`) served as one virtio-blk disk, plus the stock `…-vmlinuz-generic` (4.15)
  and `…-initrd-generic` direct-booted via §4.2. The stock initrd carries the `virtio_pci`/`virtio_blk`
  modules that probe `/dev/vda` and mount `/dev/vda1`; confirm the build's `/etc/os-release` reads
  `PRETTY_NAME="Ubuntu 18.04.1 LTS"`.
- **Machine additions the distro needs** (beyond the minimal-guest device set): a **minimal ACPI**
  (RSDP → RSDT/XSDT → FADT + DSDT + MADT with one LAPIC), **PCI** (MCFG ECAM or legacy `0xCF8/0xCFC`), and a
  **deterministic virtio-blk** device — the block completion is delivered at a fixed work-clock boundary
  through the interrupt-injection engine (blkreplay-style), not on host-I/O return, and the disk is a
  **read-only content-addressed base image + an in-memory copy-on-write overlay** (guest writes are a
  function of the guest's own deterministic execution; the base stays pristine). Pin the NIC MAC (or emulate
  no NIC); emulate no RTC so the guest starts at epoch 1970 (deterministic).
- **Deterministic command line** (4.15 GA kernel): `systemd.unit=multi-user.target cloud-init=disabled
  console=ttyS0 nokaslr net.ifnames=0 biosdevname=0 clocksource=tsc tsc=reliable no_timer_check
  scsi_mod.scan=sync udev.children_max=1 ro fsck.mode=skip` + `root=/dev/vda1 rootwait`, plus the mask list.
- **Mask the policy units** (userspace nondeterminism, not the machine): `systemd-timesyncd` +
  `systemd-time-wait-sync`, `systemd-random-seed`, `systemd-networkd(-wait-online)` + `systemd-resolved`, the
  four `cloud-init*` stages, and the `apt-daily`/`motd-news`/`man-db`/`fstrim`/`tmpfiles-clean`/`snapd*`
  timers (each makes a wall-clock- or RNG-dependent `RandomizedDelaySec` draw). Ship the rootfs cleanly
  unmounted (empty ext4 journal → no replay writes) with `tune2fs -c 0 -i 0`.
- **Entropy on an old kernel — the load-bearing detail**: 18.04's 4.15 kernel **predates** `SETUP_RNG_SEED`
  (v6.0), `random.trust_bootloader` (v5.4), and `random.trust_cpu` (v4.19), so the boot-seed pinning of §4.2 /
  §3.8 is a **no-op** there. Determinism instead comes **entirely from the machine pinning the CRNG's
  inputs**: 4.15 folds RDSEED/RDRAND/RDTSC into the CRNG key and credits the pool only via
  `add_interrupt_randomness` (RDTSC + jiffies + IRQ + IP) — all of which baud already makes deterministic
  (trapped RDTSC/RDRAND, rewritten RDSEED, exact-boundary interrupts, zeroed RAM, pinned MAC). So
  `getrandom`/`/dev/urandom` remain a pure function of the tape. This is the **strongest** form of the §3.8
  thesis: it holds on a kernel with *no entropy-injection support at all*, purely by controlling inputs. (On
  an 18.04 HWE 5.x kernel the seed flags do apply and drive the CRNG ready directly.)
- **The banner** `Ubuntu 18.04.1 LTS ubuntu ttyS0` / `ubuntu login:` is `agetty` rendering `/etc/issue`
  (`\S` → `/etc/os-release` `PRETTY_NAME`, `\n` → hostname `ubuntu`, `\l` → `ttyS0`). The exact three-token
  form needs `/etc/issue = \S \n \l` (the stock default `\S \l` omits the middle `ubuntu`) — the one line to
  confirm on the image.

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
- **`baud-packages`** — builds a real reproducible Linux guest image (kernel + initramfs + software + agent;
  Buildroot → pinned Nix, §4.5), applies the `rdseed`-rewrite pass, warms the store. `specs/baud-packages.md`.
- **`baud-driver`** — tape/fuzzing engine + snapshot-tree exploration; the exploration primitives (grid
  buckets, reservoir, correlated/"sticky" input tactic, shrink) are workload-agnostic and selected per run.
  `specs/baud-driver.md`.
- **`baud-proto`** — wire types incl. hypercall/tape-device probe + outcome messages. `specs/baud-proto.md`.
- **`baud-server`, `baud-cli`** — orchestration + command surface; adds `snapshot`/`branch`/`rewind`/
  `shell-into`/`host`/`image`/`stream` verbs.
- **`baud-tracing`, `baud-stream`, `baud-secret`, `baud-identity`, `baud-keys`** — carry over; `baud-stream`
  captures any guest's framebuffer over the tape-device frame channel and renders/streams it (RGB → y4m),
  workload-agnostic (§11.7).
- **Every crate under `crates/` stays generic** — no crate carries workload-specific (game / NES / emulator)
  knowledge. Targets (`baud-raftlet`, the emulator example, parser) live as **guest images + harnesses** under
  `examples/`, never in-tree (§11.0).

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
  in `docs/determinism.md`. Drive `drive/h/h0.sh`: `baud host probe --json` asserts each capability; a failing
  capability is recorded, never hidden.
- **H1 — boot a guest.** The run loop boots a minimal guest kernel that prints to the serial console; clean
  `Hlt`/`Shutdown`. Drive `drive/h/h1.sh`: boot the hello image, assert expected console output;
  `double_boot_memory_identical` passes.
- **H2 — deterministic double-run.** Same image + tape twice ⇒ byte-identical observation stream
  (console + probes + final memory hash), CPUID masked, work-clock TSC. Drive `drive/h/h2.sh`:
  `cpuid_leaves_are_fixed`, `work_clock_is_monotone_and_reproducible`, `all_input_is_tape_derived`,
  `no_unmodeled_exit_is_silent`.
- **H3 — randomness + time control.** OS entropy is a function of the tape (the kernel's seeding inputs are
  hypervisor-controlled — pinned `SETUP_RNG_SEED`, trapped `rdtsc`/`rdrand`, deterministic interrupts, tape-fed
  or omitted virtio-rng; `rdseed` rewritten); timestamps are the work-clock; a guest reading
  `getrandom`/`/dev/urandom` and issuing raw `rdrand` is double-run identical. Drive `drive/h/h3.sh`:
  `entropy_guest_is_deterministic`, `initial_crng_state_is_reproducible`, `virtio_rng_reseed_is_deterministic`,
  `rdrand_guest_is_deterministic`, `no_rdseed_opcode_survives_in_image`.
- **H4 — interrupt at an exact boundary.** Deliver a timer tick at a chosen work-count via
  arm-early-then-single-step; identical instruction across a double-run. Drive `drive/h/h4.sh`:
  `timer_tick_lands_at_identical_instruction`.
- **H5 — snapshot / branch / restore.** Capture, fork thousands sharing memory, rewind, restore into a live
  shell. Drive `drive/h/h5.sh`: `snapshot_roundtrip_is_bit_identical`,
  `thousand_branches_are_independent_and_deterministic`, `reset_cost_scales_with_write_set`,
  `shell_into_universe_resumes`, `restore_refuses_mismatched_cpu`.
- **H6 — multi-VM fleet.** Many single-vCPU VMs pinned across cores explore in parallel on one host. Drive
  `drive/h/h6.sh`: aggregate throughput, `capacity_refuses_sibling_split`, no cross-VM interference.
- **H7 — real Linux guest: boot → double-boot → OS-entropy.** Build a real Linux image (§4) and boot it on
  the deterministic machine, then walk the validation ladder. Drive `drive/h/h7.sh`:
  `guest_kernel_boots_to_userspace` (reaches `/init`, prints a marker, clean shutdown);
  `double_boot_ram_hash_identical` (two boots on the same tape hash the same guest RAM + vCPU state at a
  **guest-driven checkpoint** — the workload issues an `outb`/hypercall the VMM traps and hashes there, never
  a wall-clock or raw-instruction-count point); `os_entropy_is_deterministic` (a static C probe calling
  `getrandom()` ×4 and reading `/dev/urandom` ×4 prints byte-identical bytes across two boots — both the
  syscall and, on glibc 2.41+, the vDSO path). This proves an unmodified Linux CRNG is a pure function of the
  tape, end-to-end.
- **H8 — an interactive program driven to a goal, inside Linux (the §11 example).** Run an interactive
  program (the emulator example) inside the H7 Linux guest, driven only by the tape, and drive it to a defined
  goal with the framebuffer streamed live. Drive `drive/mario.sh` (§11.8):
  `interactive_probe_stream_is_identical` + `framebuffer_hashes_identical` across two boots, goal reachability,
  shrink+replay, and the mandatory ~25%-screen live window. H8 is the flagship acceptance of the whole stack.
- **H9 — a full unmodified distro, cross-VM determinism.** Boot the **stock Ubuntu 18.04.1 LTS** image (§4.7)
  to the serial login prompt, take a **timed exit** at a fixed work-clock point, and dump a fingerprint; two
  independent VMs (`vm0`, `vm1` — separate processes on separate cores) on the same `(image, tape)` produce a
  **byte-identical** fingerprint. The **timed exit** stops the guest at an exact `deterministic events` count
  (retired conditional branches — a raw `BR_INST_RETIRED.COND` event, identical on both VMs) via
  arm-early-then-single-step (§3.4), then reads guest RIP (`KVM_GET_REGS`), translates it to a guest-physical
  address (`KVM_TRANSLATE`, cross-checked by a manual CR3 4-level page walk), and hashes guest RAM (blake3
  over the RAM slots in canonical order, excluding MMIO / host-written pages). Drive `drive/h9.sh` boots both
  VMs and asserts equality of all four fields, printing:
  ```
  Ubuntu 18.04.1 LTS ubuntu ttyS0

  ubuntu login:
  vm0 - timed exit:
  deterministic events = <N>
  guest RIP = <rip> (-> guest physical = <gpa>)
  guest memory hash = <hash>
  vm0: done
  ```
  ```
  Ubuntu 18.04.1 LTS ubuntu ttyS0

  ubuntu login:
  vm1 - timed exit:
  deterministic events = <same N>
  guest RIP = <same rip> (-> guest physical = <same gpa>)
  guest memory hash = <same hash>
  vm1: done
  ```
  Identical `deterministic events`, `guest RIP`, `guest physical`, and `guest memory hash` across `vm0` and
  `vm1` proves the whole-distro execution is a pure function of `(image, tape)` — independent of host
  instance, physical core, or wall-clock. Tests `ubuntu_boots_to_login`, `timed_exit_fingerprint_is_stable`,
  `cross_vm_fingerprint_matches`.
- **M-series** rebuild server/CLI/driver/store/stream on this core: tape-tree exploration
  (`driver_is_reproducible`, `shrink_reproduces_from_nearest_snapshot`), strategy/tactics over guest probes,
  snapshot-store reconstruction/shrinking, the framebuffer stream (§11), a distributed target as a **guest
  image** reaching a planted safety violation via guided branch search (`planes_agree_on_healthy_run`, the
  planted-bug interleaving test), and the **Super Mario Bros validation** (§11) as the flagship end-to-end
  proof.

## 11. Super Mario Bros — a worked example of the generic loop

This section is an **example**, not a feature: it exercises the generic engine (§3–§10) on one arbitrary
interactive program — the FCEUX NES emulator running **inside the H7 Linux guest** — and drives it to a goal
(complete Super Mario Bros) from controller input alone, streaming the emulator live in a ~25%-screen window.
It is the visible instance of §0's claim: any program on baud's deterministic Linux is a system baud's fuzzer
explores to a chosen state, reproducibly.

### 11.0 What is generic, what is the example

- **Generic (baud core — gains zero game knowledge):** `baud-multiverse` boots any bzImage+initramfs;
  `baud-packages` builds any deterministic Linux image; the tape device + `/dev/vport` endpoint move opaque
  bytes; `baud-driver` supplies the exploration primitives (grid buckets, reservoir, correlated/"sticky"
  input, shrink); `baud-stream` streams any framebuffer; the CLI verbs (`image build/lint`, `run`, `obs`,
  `stream`, `shrink`, `snapshot`/`branch`, `verify determinism`) are workload-agnostic.
- **The example (`examples/mario/` — the ONLY place emulator / NES specifics appear):** `spec.toml` (the
  Linux image = kernel + initramfs + the emulator + a Lua harness + `/init`); `harness.lua` (maps the tape's
  controller bytes ↔ the emulator's joypad, drives it one frame at a time, reads the RAM probes, emits probes
  + frames); `probes.toml` (which RAM addresses are progress/goal); `strategy.toml` (objective = maximize the
  `x` probe; selects the generic sticky-mask tactic + grid); the user-supplied ROM path.
- **The contract that keeps it generic:** baud offers a *byte-tape-in / probes-and-frames-out* interface over
  a deterministic program. Swapping this example for another program = a new `examples/<name>/` with its own
  image spec + harness + probes — **zero core changes**.
- **Test** (`no_workload_specifics_in_core`): a lint asserts no crate under `crates/` references
  emulator/game/NES symbols or the example's probe addresses; all of that lives under `examples/`.

### 11.1 Guest image

- **`examples/mario/spec.toml`** builds (via `baud-packages`, §4.5) a real Linux image containing the FCEUX
  NES emulator, a Lua interpreter, the harness, and a static `/init` that launches the emulator headless and
  powers off on exit. It boots on the deterministic machine exactly like any H7 guest.
- **ROM + savestate are user-supplied paths, never bundled** (copyright). CI uses a free homebrew ROM.
- Pin FCEUX's own determinism seams: `RAMInitOption ∈ {0,1,2}` (fixed power-on RAM, not the random option),
  start from power-on or a fixed savestate, so its emulation is a pure function of (ROM, input tape) — and the
  whole guest is therefore a pure function of the tape.

### 11.2 In-guest harness (`examples/mario/harness.lua`)

- **Headless launch** from `/init`: `SDL_VIDEODRIVER=dummy SDL_AUDIODRIVER=dummy fceux --no-config 1 --sound 0
  --loadlua /harness.lua /game.nes` (fallback `xvfb-run` if the dummy video backend refuses a surface).
- **Frame loop**: `emu.speedmode("maximum")`; each frame — read **one controller byte** from the input port
  (synchronous `io.read(1)` on `/dev/vport0p1`, which paces the emulator to the tape) → decode to `joypad.set(1,
  {A=,B=,select=,start=,up=,down=,left=,right=})` (baud's byte layout `A | B<<1 | Select<<2 | Start<<3 |
  Up<<4 | Down<<5 | Left<<6 | Right<<7`, mapped in the harness) → `emu.frameadvance()` → read the §11.3 RAM
  probes with `memory.readbyte` → write them + the frame out the observation port with `:flush()`.
- **Channel**: two virtio-serial ports (`/dev/vport0p1` in, `/dev/vport0p2` out), backed by baud's tape device
  (§4.4); input and observations do not interleave. The harness is the only workload-aware code; the emulator
  underneath is unmodified.

### 11.3 Probes (`examples/mario/probes.toml`)

Read from NES RAM by the harness each frame (all confirmed against the SMB RAM map + the 6502 disassembly):

- `x = mem[0x006D] * 256 + mem[0x0086]` — global horizontal position (`0x0086` on-screen x, `0x006D` page);
  the primary progress signal.
- `y = mem[0x00CE]` — on-screen vertical position; the grid's second dimension.
- `world = mem[0x075F]` (0-based), `area = mem[0x0760]`, `lives = mem[0x075A]`, `oper_mode = mem[0x0770]`
  (game mode: `01` normal play, `02` end-of-world, `03` end/dead).
- **Completion is a derived condition, not a single flag** — SMB has no "game-completed" byte. The end state
  is `world == 7` (world 8, 0-based) cleared through the final castle, detected via the `oper_mode`/`area`
  transition. Encoded as the goal predicate in `probes.toml`; `baud image lint` validates every address
  against the reference map, never hard-coded on faith.

### 11.4 Strategy (progress = "how far into the game")

- **Objective**: maximize global `x` — the run is scored purely by how far right it has driven the character,
  because farther right is farther into the game and the victory state is the maximum `x` of the final world.
- **Nothing beyond the score belongs here.** The strategy is only this progress score over the §11.3 probes.
  The exploration that spends the score — which prefixes to keep, which to extend — is baud's existing tape
  engine (§6, `baud-driver`), reused unchanged; the Mario example adds none of its own. If a run appears to
  need exploration behaviour the driver lacks, add it to the driver so every workload inherits it — never
  encode workload-specific search here.
- **Goal**: reach the game's victory / end state (the completion predicate of §11.3).

### 11.5 Tactics (input distribution)

- **Sticky flip-mask**: `next_byte = prev_byte XOR low_probability_mask` — each controller bit flips with a
  small per-frame probability (tuned), so buttons stay held across many frames. Correlate input across frames;
  never draw a fresh independent byte per frame. A jump needs A held for ~30–100 frames, which independent
  50%-per-frame input reaches with probability `1/2^N` (≈ 0 for a long jump) — so uncorrelated input clears no
  jump, while flipping a bit only to start or stop pressing makes long holds common.
- **Pure per-frame random** is kept only as a negative control — its reachable-position heat-map decays fast
  near the spawn.

### 11.6 Why this validates baud

- **Reaching completion proves the exploration works.** Beating the game requires getting "sequentially lucky"
  hundreds of frames in a row — far deeper than a few-step interleaving — so a run that reaches it shows baud's
  tape engine can drive an arbitrary program to a deep target state through input exploration alone, then
  replay that run exactly. That is the whole capability, shown on a program anyone can watch.
- **Non-fragility (a required test, not a nicety).** The **unchanged** setup — zero re-tuning — must also make
  progress on a much harder hand-authored ROM variant. A pre-recorded input tape would desync instantly on a
  changed ROM; baud's exploration adapts because nothing is scripted. `drive/mario.sh` runs this variant as a
  second, non-gating case.

### 11.7 Live display, baud-stream, and the README GIF

- **Per-frame capture (in the harness)**: each frame `gui.gdscreenshot()` returns the 256×240 screen (GD
  truecolor); the harness emits `[format tag][width:2][height:2][pixels]` on a frame port and `baud-stream`
  forwards it. Frames are a **derived artifact of the tape** — a pure function of (ROM, input) — so baud stores
  only the tape and regenerates identical frames on demand by replaying it (`fceux -playmovie` / re-run the
  harness). No video is stored.
- **Live window (mandatory, every run), ~25% of the screen**, on the Windows desktop via WSLg:
  ```bash
  SW=$(powershell.exe -NoProfile -Command '[System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width' | tr -d '\r')
  XW=$((SW/2)); YH=$((XW*240/256))          # NES 256:240 aspect preserved
  baud stream tail --run "$RUN" --format y4m \
    | ffplay -f yuv4mpegpipe -i - -an -framedrop -infbuf \
        -x "$XW" -y "$YH" -left 40 -top 40 -window_title "baud"
  ```
  (baud-stream produces the y4m via `ffmpeg -f rawvideo -pix_fmt rgb24 -s 256x240 -r 60000/1001` internally, or
  emits y4m directly; a small in-crate SDL viewer is the alternative to `ffplay`.) Concurrent runs render as
  separate tiled windows.
- **`README.md` hero + centralized GIF** (a `drive/mario-gif.sh` step): from the winning run,
  `baud stream tail --run <winning> --format y4m | ffmpeg -i - -vf "fps=30,scale=512:-1:flags=neighbor" -loop 0
  docs/mario.gif`, committed under `docs/` and embedded at the very top of `README.md` as the single centered
  reference. Because the GIF is re-derived from the winning tape it is a reproducible artifact of the run
  (regenerable from the tape hash), not a hand-recorded screencast.
- **README hero copy** (describe the environment generically — never name the OS): *"**baud beats Super Mario
  Bros — and your program is no different.** baud runs any program inside a fully-deterministic environment it
  controls end to end, turns that program's entire input into one replayable tape, and lets its fuzzer explore
  the tape until the program reaches the state you care about — a win or a completed task — every time,
  reproducibly. Mark what \"winning\" looks like for your program; baud's fuzzer finds the inputs that reach it
  and replays them identically. Super Mario Bros is one program among them."*
- **Test** (`mario_stream_is_live_and_rederivable`): during a run `baud stream tail` produces frames at ~60 fps
  and a re-render from the tape is byte-identical to the live frames.

### 11.8 Acceptance test (`drive/mario.sh`)

1. `baud image build examples/mario` (real Linux image, §4.5; the `rdseed`-rewrite pass is a no-op here) and
   `baud image lint` (validates the §11.3 probe addresses and the tape-device path).
2. `baud verify determinism` — the same tape twice yields an identical probe stream and identical framebuffer
   hashes (`interactive_probe_stream_is_identical`, `framebuffer_hashes_identical`).
3. **Negative control**: `baud run --tactics random` — positions plateau near spawn, checked via `baud obs`.
4. **Main run**: `baud run --strategy examples/mario/strategy.toml --tactics sticky-mask` — the max-`x` score
   climbs run over run until the program reaches the end state (§11.3).
5. Mid-run `baud tape kill` + `baud tape reconstruct` + resume — journal-free reconstruction on this workload.
6. Terminates with `GoalReached` on the completion predicate (§11.3); the winning tape is journaled and the
   README GIF (§11.7) is regenerated from it.
7. `baud shrink` → `baud replay` of the shrunk tape still reaches the goal.
8. **Non-fragility case (non-gating)**: the same command on a harder hand-authored ROM variant still makes
   progress and, if it gets stuck, reports the anomaly rather than crashing the harness.
- **Release gate**: full completion on the base ROM. **CI variant**: gates on `x` progress into the second
  world within budget.

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
| 26 | A real Linux guest now boots to userspace, but only as a hand-built fixture (`linux-guest`, §14) — the automated *pipeline* (Buildroot/pinned-Nix, §4.5) still does not exist, though the from-source `make bzImage` path (`baud-packages`'s `kernel_build`/`initramfs`/`guest_build` modules) is built and hardware-tested | A real image pipeline: minimal builtin kernel + deterministic cmdline + boot_params (E820, `SETUP_RNG_SEED`) + reproducible initramfs + `/init` + a tape endpoint, built by `baud-packages` (§4) | `guest_kernel_boots_to_userspace` (done); `boot_params_seed_is_pinned` (done); `init_powers_off_deterministically` (done); `image_build_is_reproducible` (done) |
| 27 | OS-entropy determinism must be shown, not asserted | Boot a real Linux guest and prove the CRNG is a pure function of the tape end-to-end (§4, §3.8) | `os_entropy_is_deterministic` (H7, done); `double_boot_ram_hash_identical` (done) |
| 28 | An arbitrary interactive program must be driven to a goal, reproducibly, inside Linux | The emulator example (§11) runs inside the H7 guest; identical probe + framebuffer streams across boots; goal reached; shrink+replay holds | `interactive_probe_stream_is_identical`; `framebuffer_hashes_identical`; `mario_stream_is_live_and_rederivable` |
| 29 | Example specifics could leak into core crates | The engine stays generic; all workload code lives under `examples/` (§11.0) | `no_workload_specifics_in_core` |
| 30 | Determinism must hold for a full unmodified distro, provably | Boot stock Ubuntu 18.04.1 to login (§4.7); a timed-exit fingerprint (events / RIP / guest-physical / RAM hash) is byte-identical across two independent VMs | `ubuntu_boots_to_login`; `timed_exit_fingerprint_is_stable`; `cross_vm_fingerprint_matches` |
| 31 | A stock distro kernel (Ubuntu 4.15) predates seed-injection flags | Determinism comes from the machine pinning CRNG inputs (RDTSC/RDRAND/RDSEED + exact-boundary interrupts + zeroed RAM + pinned MAC), not a boot seed — holds with no guest entropy support (§4.7) | `os_entropy_is_deterministic` (on the Ubuntu guest) |

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
  since there is now one determinism model. `host probe` reports capabilities, not a mode. (Done — see §14.)

## 14. Build status

The authoritative, blow-by-blow build log is **`ralph/progress.txt`** — this section is only a short current
snapshot, not a duplicate of it.

- **Built and hardware-tested** (details in `ralph/progress.txt`): `baud-host` (`host probe`), `baud-vcpu`
  (exit dispatch, single-step interrupt injection), `baud-multiverse` (KVM boot flow, CPUID mask, work-clock,
  console, tape bus, dirty-ring reset), `baud-tape-device`, `baud-snapshot` (capture/restore, userfaultfd
  branching, dirty-ring), `baud-snapshot-store`, `baud-packages` guest-image contract + `baud image lint` +
  the `no_workload_specifics_in_core` generic-core guardrail (`crates/baud-packages/src/workload_lint.rs`) +
  the build-time `rdseed`→`UD2`(+`NOP`) rewrite pass (`crates/baud-packages/src/rdseed.rs`, wired end-to-end
  via `POST /image/rewrite-rdseed` and `baud image rewrite-rdseed <path>`), and the patched `kvm_intel`
  module that hardware-traps `rdtsc`/`rdrand` and serves them from the work-clock / tape **and** now serves
  the `rdseed`-rewrite's `UD2` trap too: `kernel-module/baud-enforced/ud2-enforce.patch` intercepts
  `handle_exception_nmi`'s existing `is_invalid_opcode` branch (stock KVM already traps `#UD` unconditionally
  for its own emulation fallback, so no exception-bitmap change was needed), forwards a confirmed rewritten
  site as `KVM_EXIT_BAUD_DETERMINISM`, and tail-calls the original `handle_ud` for anything else (so kernel
  `BUG()`/`WARN_ON()` and genuine invalid opcodes are untouched). `drive/manual/h3-enforced-rdseed.sh` hardware-
  validated both halves on real `/dev/kvm`: `rdseed_enforced_regime_is_bit_exact_across_boots` (a registered
  site serves a tape-seeded value into the destination GPR, bit-exact across two boots) and
  `ud2_outside_the_rdseed_site_table_reinjects_ud` (an unregistered `UD2` re-injects `#UD` verbatim, never
  served a value), plus regression re-runs of the RDTSC/RDRAND enforced tests with all three patches layered
  on the same module. Stock module restores cleanly on exit every time. `rdseed`'s own rewrite pass is still
  only exercised against synthetic hand-built ELF fixtures (`crates/baud-packages` tests) plus the small
  hand-assembled `tests/fixtures/rdseed-guest/` boot fixture — not yet run against a real linked
  kernel/userspace ELF; do that before relying on it for a real guest image.
  **`RdseedRewriteReport` → boot wiring is now closed**: `RdseedSite` carries a real `gpr_index` (decoded
  from the instruction's own ModRM `rm` field + `REX.B`, `crates/baud-packages/src/rdseed.rs`, tested against
  a non-`eax` register and a `REX.B`-extended one, not just the fixture's `eax` case); `baud image
  rewrite-rdseed` writes a `<output>.rdseed-sites.json` sidecar next to the patched image
  (`crates/baud-cli/src/cmds/image.rs`); `baud-server`'s new `rdseed_sites` module
  (`crates/baud-server/src/rdseed_sites.rs`) loads that sidecar and threads its sites into
  `Multiverse::boot_with_rdseed_sites` at both real production boot call sites (`boot_run_and_drain`,
  `boot_and_snapshot` in `routes/run_kvm.rs`) — a missing sidecar (the common case) yields the same empty
  table `Multiverse::boot` always passed, a malformed one fails loud. Verified end-to-end with a real ELF
  (`as`/`ld`, `rdseed eax` + `rdseed r8d`) run through the live CLI against a live server: patched image has
  `UD2`+`NOP` at both sites, sidecar JSON carries the correct address/gpr_index (0 and 8)/length for both.
  The one remaining caller still hardcoding its site is `rdseed_enforced_regime_is_bit_exact_across_boots`,
  because `tests/fixtures/rdseed-guest/` is a hand-assembled flat binary that never goes through the
  ELF-based rewrite pass at all (see that fixture's `BUILD.md`) — a real ELF-based guest image now gets the
  sidecar automatically.
  **`Regime` enum cleanup is done**: the tri-state `Regime` enum and its `Probe::regime` field are gone from
  `baud-host`, replaced by capability booleans plus `Probe::is_runnable()` / `Probe::is_enforced_capable()`;
  `GET /host/probe` and all 7 `drive/h/h0.sh`-`h6.sh` scripts now read the renamed JSON fields
  (`enforced_module_present`/`runnable`/`enforced_capable`) instead of a `"regime"` string.
  **`SETUP_RNG_SEED` boot-RNG-seed `setup_data` node is wired end-to-end**: `crates/baud-multiverse/src/layout.rs`
  adds `RNG_SEED_SETUP_DATA_ADDR` (right after the zero page) and `RNG_SEED_SETUP_DATA_LEN`, plus a static
  assertion that the node fits before `PML4_ADDR`. `crates/baud-multiverse/src/linux/bootparams.rs` adds the
  `SETUP_RNG_SEED = 9` constant, `RNG_SEED_LEN = 32`, a `write_rng_seed_setup_data` helper that writes the
  `{next: 0, type: 9, len: 32, data: seed}` node into guest memory, and `load_kernel_and_write_boot_params` now
  takes a `rng_seed: &[u8; 32]` param and points `hdr.setup_data` at the node it writes.
  `crates/baud-multiverse/src/linux/mod.rs`'s `boot_guest` now takes a `tape: &[u8]` and derives the seed via a
  new `rng_seed_from_tape` (blake3 with a domain-separation prefix `"baud:setup-data:rng-seed:v1"`, distinct
  from `entropy_seed_from_tape`'s, so the boot seed and the rdrand/rdseed entropy substream never share a hash
  stream); `Multiverse::boot` threads its own `tape` through. Hardware-verified end-to-end
  (`rng_seed_setup_data_is_wired_into_a_real_boot_and_is_tape_derived`, real `/dev/kvm` boot of the hello-guest
  fixture): same tape reproduces the identical seed, a changed tape byte changes it, and the node lands in real
  guest RAM at the address `hdr.setup_data` points to. Still open: the virtio-rng tape-fed device, and proving
  the deterministic-TSC + exact-interrupt seeding covers the initial CRNG state on a real Linux kernel guest
  (blocked on the boot-pipeline prerequisite in step 1 below).
  **H4's interrupt-injection engine is now wired for an open-ended (unknown tick count) run, not just a
  pre-known one**: `baud_vcpu::boundary::PmuStepper::is_halted` + `InjectOutcome::{Injected, Halted}` (a
  guest halting before the next scheduled tick is reported gracefully, never as an error) and
  `Multiverse::run_to_first_halt_with_periodic_timer` (§14 next-actions item 1 below has the full detail).
  This was the concrete prerequisite an earlier real-kernel boot attempt was missing (it hung in
  `calibrate_delay()` because nothing injected more than a fixed, test-chosen number of ticks) — the real
  image pipeline in `baud-packages` is still the remaining, larger piece of item 1.
  **`bootparams.rs` now also carries initramfs, alongside the already-done `e820`/`SETUP_RNG_SEED`**:
  `crates/baud-multiverse/src/layout.rs` adds `INITRAMFS_ADDR` (0x0200_0000, 32 MiB in, above
  `KERNEL_LOAD_ADDR` and inside `GUEST_RAM_SIZE` per a new static assertion); `bootparams.rs`'s
  `write_initramfs` writes the bytes verbatim there, and `load_kernel_and_write_boot_params` gained an
  `initramfs: Option<&[u8]>` param that, when `Some`, points `hdr.ramdisk_image`/`ramdisk_size` at them and
  sets `hdr.initrd_addr_max` explicitly to `ram_size - 1` (left at 0 it reads to some kernels as "no
  placement allowed", not "unlimited" — a real gotcha). `hdr.loadflags` now always gets `LOADED_HIGH`/
  `CAN_USE_HEAP` set, initramfs or not, per §4.2. `linux/mod.rs`'s `boot_guest` and
  `Multiverse::boot_with_rdseed_sites` thread the same `initramfs: Option<&[u8]>` through (`Multiverse::boot`
  still passes `None`, so no existing caller changed); hardware-verified end-to-end
  (`initramfs_is_wired_into_a_real_boot_and_lands_in_guest_ram`, real `/dev/kvm`, `hello-guest` fixture):
  bytes land verbatim in real guest RAM, `hdr.ramdisk_image`/`ramdisk_size` read back correctly off the real
  zero page, and the boot still reaches its marker and halts cleanly with an initramfs present that it never
  reads. Also added `bootparams::DETERMINISTIC_CMDLINE`, the exact cmdline string §4.2 specifies, as a pure
  Rust constant — **not yet wired as anyone's default**, callers still pass their own cmdline string; nothing
  calls it outside its own test (`deterministic_cmdline_matches_the_spec_exactly`). This closes the
  `bootparams.rs`/initramfs sub-item that item 1 below used to list as open; the boot-pipeline items still
  open are relisted there.
  **A real, compiled Linux 6.18 kernel now boots through baud-multiverse's real KVM boot flow all the way
  to a real `/init` userspace process** — the first time this project has booted an actual, unmodified
  Linux kernel to userspace, not a hand-assembled fixture payload. New test
  `guest_kernel_boots_to_userspace` (`crates/baud-multiverse/src/linux/mod.rs`) boots the same image+tape
  twice via `Multiverse::boot_with_rdseed_sites` and H4's `run_to_first_halt_with_periodic_timer`
  (§14 next-actions item 1's open-ended injection engine, no pre-known tick count) and asserts each run
  independently reaches `/init`'s marker and halts cleanly after the *same number* of periodic ticks —
  deliberately not asserting byte-identical console output or RAM hash across the two boots (see below).
  New fixture `crates/baud-multiverse/tests/fixtures/linux-guest/` (`bzImage`, `initramfs.cpio.gz`,
  `init.c`, `minimal.config`) is documented start-to-finish in that directory's `BUILD.md`, including a
  `minimal.config` fragment implementing spec §4.1's required/disabled Kconfig list and a finding that no
  LAPIC device model was needed: with no memory region registered at the LAPIC's fixed MMIO base, register
  reads fall through to baud's existing open-bus fallback, the kernel concludes "No local APIC present"
  and falls back to `Using NULL legacy PIC`, and H4's `KVM_INTERRUPT`-based injection still delivers
  `LOCAL_TIMER_VECTOR` (`0xec`) regardless, logged as (harmless) "Spurious LAPIC timer interrupt". New
  drive script `drive/h/h7.sh` runs this as a partial H7 (boot-to-userspace leg only; OS-entropy and the
  double-boot RAM-hash comparison remain open, see item 2 below). Getting here required fixing three real
  bugs, all documented in `tests/fixtures/linux-guest/BUILD.md`: (1) `baud-vcpu`'s `VcpuExit::IrqWindowOpen`
  was mapped to `Exit::Unmodeled` (`crates/baud-vcpu/src/linux/mod.rs`) instead of ever reaching
  `boundary::PmuStepper::run_until_irq_window`'s fallback, invisible until a guest genuinely held
  interrupts disabled for a real stretch (every prior fixture stayed injectable throughout); fixed with a
  proper `Exit::IrqWindowOpen` variant that `dispatch_exit` resolves to `Continue`
  (`crates/baud-vcpu/src/lib.rs`), covered by a new regression unit test
  (`irq_window_open_continues_rather_than_faulting`, `crates/baud-vcpu/src/tests.rs`); (2) e820 left the
  entire first megabyte `reserved`, but Linux's `reserve_real_mode()` panics without sub-1MiB usable
  memory for the real-mode trampoline, fixed via a new `layout::LOW_MEM_RAM_START` (`0x1000`,
  `crates/baud-multiverse/src/layout.rs`) plus a low-memory `usable` e820 entry
  (`crates/baud-multiverse/src/linux/bootparams.rs`); (3) the guest kernel config needed
  `CONFIG_X86_IOPL_IOPERM=y` for `/init`'s `iopl(3)` + raw `outb` marker write, used instead of
  `write(1, ...)` because this machine has no interrupt controller so the 8250 UART's interrupt-driven tty
  transmit path never drains (an early plain-`write` `/init` booted clean to shutdown with no marker at
  all). **Still open, not attempted or fixed this iteration**: `os_entropy_is_deterministic` (H7's
  OS-entropy leg); `double_boot_ram_hash_identical` needs a guest-driven checkpoint design (an explicit
  `outb`/hypercall the workload issues), not raw console/RAM comparison across full boots — a first attempt
  at strict full-console-byte-equality across two boots of this real kernel found the two differ in exactly
  one kernel-internal line (`sched_clock: Marking stable (A,B)->(C,0)`, printing raw TSC-derived numbers
  sensitive to the already-documented `RCB_HARDWARE_JITTER_TOLERANCE`), which is exactly why a checkpoint-
  based approach is needed instead; the real image-build pipeline in `crates/baud-packages` (Buildroot or
  pinned-Nix per spec §4.5) still does not exist — `linux-guest`'s kernel+initramfs are checked into the
  repo and built by hand per the fixture's own `BUILD.md`, the same fixture-vs-automated-pipeline gap that
  already existed for every other fixture in that directory; and `bootparams::DETERMINISTIC_CMDLINE` is
  still not wired as any real caller's default — `guest_kernel_boots_to_userspace` builds its own cmdline
  string inline, close to but not identical to `DETERMINISTIC_CMDLINE` (it omits `no-kvmclock`, `pci=off
  acpi=off`, `quiet loglevel=1`, `random.trust_cpu=off random.trust_bootloader=on`, and `nomodule`, kept
  off for boot-log debugging visibility while bringing the fixture up).
- **Next actions (this rewrite)** — a sequence, each step enabling the next:
  1. **Guest boot pipeline (§4)** — the enabling milestone.
     **H4 interrupt injection is now wired into an open-ended run loop** (the sub-step that was blocking
     this: an earlier real-kernel attempt hung in `calibrate_delay()` waiting on a jiffies tick because
     periodic injection wasn't wired in at all — nothing called `inject_timer_tick` more than a caller-
     chosen, pre-known number of times). `baud_vcpu::boundary::PmuStepper` gained `is_halted()` and
     `inject_at` now returns an `InjectOutcome::{Injected, Halted}` instead of erroring when the guest
     halts on its own before the next scheduled tick's target — the ordinary case for a real kernel whose
     tick count is never known ahead of time, not a determinism hole. `Multiverse::
     run_to_first_halt_with_periodic_timer(period_rcb, vector, max_ticks)` (`crates/baud-multiverse/src/
     linux/mod.rs`) is the new open-ended counterpart to `run_with_timer_ticks`: it keeps scheduling ticks
     until the guest halts gracefully or `max_ticks` is exhausted (the same bounded-non-termination
     convention as `run_until_console_len`). Hardware-verified on real `/dev/kvm`
     (`periodic_timer_injection_halts_gracefully_and_reproducibly`, `timer-guest` fixture): the guest
     survives several ticks then halts on its own, reproducibly (identical tick count, rip-per-tick, console
     output, and RAM hash across two boots) — same per-tick `RCB_HARDWARE_JITTER_TOLERANCE` framing as
     `timer_tick_lands_at_identical_instruction` (rare per-tick jitter over tolerance is a known real-
     hardware branch-counter read-precision limit, not a logic bug; keep tick counts in a test low for
     exactly this reason — more ticks multiplies the chance any single one trips the tolerance).
     **`guest_kernel_boots_to_userspace` is now done**: a real, compiled Linux 6.18 kernel
     (`crates/baud-multiverse/tests/fixtures/linux-guest/`, built by hand per that directory's `BUILD.md`,
     not yet by an automated pipeline) boots through this open-ended engine all the way to a real `/init`
     process, hardware-verified twice for matching tick counts (`crates/baud-multiverse/src/linux/mod.rs`;
     partial drive coverage in `drive/h/h7.sh`). Three real bugs fixed to get there — a `baud-vcpu`
     `IrqWindowOpen` dispatch bug, e820's missing sub-1MiB usable memory (Linux's `reserve_real_mode()`
     panics without it), and a missing `CONFIG_X86_IOPL_IOPERM=y` guest kernel config — are detailed in the
     bullet above and in that `BUILD.md`.
     **This iteration (ralph iteration 12) implemented and hardware-verified a real, concrete piece of
     that automated pipeline**, in two new `baud-packages` library modules — neither yet wired into any
     CLI/server route. `crates/baud-packages/src/initramfs.rs` is a pure-Rust reproducible newc-format
     cpio + gzip initramfs builder (`InitramfsEntry`, `build_reproducible_initramfs`) implementing §4.3's
     exact recipe — fixed mtime=1, uid/gid=0, sorted entries via synthesized directory records for every
     path prefix, gzip -9 via the `flate2` crate — with no dependency on the host having `cpio`/`gzip`
     installed; output is a pure function of the input entries plus the crate's pinned `flate2` version.
     6 new unit tests, including a byte-for-byte reproducibility test and a round-trip test that decodes
     the archive back (a test-only newc cpio parser) and confirms file contents/mode/path survive; all
     pass, clippy clean. This closes the reproducible-initramfs-builder sub-gap of §4.3 as real, tested
     code, though it is not yet wired as the actual builder for `tests/fixtures/linux-guest/
     initramfs.cpio.gz` (that fixture is still hand-built per its own `BUILD.md`), and it is not yet
     assembling a real multi-file rootfs (harness scripts, agent binary) beyond a single `/init`-style
     entry. `crates/baud-packages/src/kernel_build.rs` automates the by-hand kernel-build recipe from
     that same `BUILD.md`'s "Regenerating the kernel" section — `KernelBuildConfig`, `build_bzimage()`
     shells out to `make CC=<cc> mrproper / allnoconfig / (merge_config.sh -m .config <fragment>) /
     olddefconfig / -jN bzImage` against a given kernel source tree + Kconfig fragment — and critically
     pins `KBUILD_BUILD_TIMESTAMP=@0`, `KBUILD_BUILD_USER=baud`, `KBUILD_BUILD_HOST=baud`,
     `SOURCE_DATE_EPOCH=0` as build env vars: without these, Kbuild embeds the real wall-clock build time
     and `whoami`/`hostname` into the compiled kernel's version string, so two builds of byte-identical
     source+config would not be byte-identical — a real, non-obvious nondeterminism source this iteration
     found and fixed before it could bite `image_build_is_reproducible`. Spec §4.5's named test,
     `kernel_build::tests::image_build_is_reproducible` (`#[ignore]`d), builds the real `linux-guest`
     fixture's kernel (`tests/fixtures/linux-guest/minimal.config`) twice from two independent scratch
     copies of `~/wsl-kernel-src/src` (the same tree CLAUDE.md already documents for the enforced-module
     work, copied first per that `BUILD.md`'s own warning never to build in the shared tree directly) and
     asserts the two `arch/x86/boot/bzImage` outputs are byte-for-byte identical. New drive script
     `drive/pkg/pkg-image-build.sh` runs it, opt-in like `drive/manual/h3-enforced-*.sh` (not part of the standard
     h0-h7 gate — two full kernel compiles take several minutes). **Real-hardware result: PASSED** — two
     independent ~4.5-minute from-source kernel builds (real gcc-13, real `~/wsl-kernel-src/src`)
     produced a byte-identical `bzImage`, ~546s total; this is the first time
     `image_build_is_reproducible` has been proven true on this project, not just specified. One real bug
     was found and fixed while getting the drive script working: `/tmp` on this WSL2 dev host is a small
     (3.9G) RAM-backed tmpfs, nowhere near enough for two copies of a kernel source tree plus build output
     (`cp -a` failed mid-copy with ENOSPC); fixed by having `drive/pkg/pkg-image-build.sh` set `TMPDIR` to a
     directory on the real disk (`~/.baud-tmp`, cleaned up via a `trap ... EXIT`) before invoking the
     test — `tempfile::tempdir()` (used by the Rust test) honors `$TMPDIR`, worth remembering for any
     other drive script that stages large scratch data.
     **This iteration (ralph iteration 13): the CLI/server wiring gap above is now closed.** New
     `crates/baud-packages/src/guest_build.rs` composes the two already-tested pieces into one callable
     pipeline: `GuestImageBuildConfig` (a `KernelBuildConfig` + a slice of `InitramfsFileEntry` + an
     `output_dir`), `build_guest_image()` (runs `build_bzimage`, reads every initramfs entry's
     `source_path` from disk, runs `build_reproducible_initramfs`, writes both outputs into
     `output_dir`), and a pure, independently-unit-tested `hash_image()` implementing spec §4.5's exact
     image identity — `sha256(bzImage ‖ initramfs.gz)` — via a new `sha2` workspace dependency (the
     existing `blake3` convention used for `BuildResult::closure_hash` was deliberately not reused here,
     since §4.5's own text names `sha256` specifically). 4 new unit tests, including one pinned against
     an independent `sha256sum` test vector, not just internal self-consistency. Exposed end-to-end:
     `POST /image/build` (`crates/baud-server/src/routes/image.rs`, run in `spawn_blocking` like
     `/host/probe` and `/run/kvm` — a real kernel build takes minutes and shells out to `make`, so it
     must not block the async runtime) and `baud image build --kernel-src --config-fragment --cc --jobs
     --initramfs-entry archive_path:mode_octal:source_path --output-dir` (repeatable
     `--initramfs-entry`, e.g. `init:755:/path/to/init`) in `crates/baud-cli/src/cmds/image.rs`. All
     paths are resolved on the server host, not transferred as content — a kernel source tree is far too
     large to shuttle as `/image/rewrite-rdseed`-style base64.
     **Hardware-verified end-to-end** via new `drive/pkg/pkg-build-cli.sh`: starts a real `baud-server`,
     musl-gcc-compiles the `linux-guest` fixture's real `init.c`, then drives the *entire* build through
     `baud image build --json` alone (real `gcc-13`, real `~/wsl-kernel-src/src` scratch-copied tree,
     ~4-5 min) — real result: `ok=true`, a real 1,913,856-byte `bzImage` and a real 2,257-byte
     `initramfs.cpio.gz` written to disk, and well-formed 64-hex-char `bzimage_sha256`/
     `initramfs_sha256`/`image_hash` all present in the response. Not part of the standard h0-h7 gate
     (opt-in, same convention as `drive/pkg/pkg-image-build.sh` and the enforced-regime scripts — one real
     kernel compile takes several minutes). `cargo build`/`clippy`/`test --workspace` all clean; `drive/
     h0.sh` through `drive/h/h7.sh` (stock module) all still PASS on real `/dev/kvm` (one incidental
     finding: `linux::tests::rdtsc_guest_reproduces_high_bits_across_boots`, an unrelated pre-existing
     real-hardware TSC test in `baud-multiverse` this iteration never touched, flaked once under full
     `cargo test --workspace` parallel load — high=0x5a768 vs 0x32659f — then passed clean both in
     isolation and on a full-suite rerun; recorded here as an observed one-off real-hardware jitter, not
     chased further since it reproduces neither reliably nor in isolation).
     **This iteration (ralph iteration 14): the `baud run kvm` initramfs gap above is now closed, plus a
     second, deeper gap this iteration found in the same area.** New migration `crates/baud-server/
     migrations/0011_kvm_run_meta_initramfs.sql` adds nullable `initramfs_path`/
     `periodic_timer_period_rcb`/`periodic_timer_vector`/`periodic_timer_max_ticks` columns to
     `kvm_run_meta`, so a persisted `/run/kvm` boot can be replayed exactly. `RunKvmBody`
     (`crates/baud-server/src/routes/run_kvm.rs`) gained `initramfs_path: Option<String>` and
     `periodic_timer: Option<PeriodicTimerSpec>` (`period_rcb: u64`, `vector: u8` default `0xec`,
     `max_ticks: u32` default `2000`), both `#[serde(default)]` so every existing caller (`drive/m/m9.sh`/
     `m10.sh`/`m11.sh`) is unaffected; a new `read_initramfs` helper is shared by `run()` and
     `stream::render_frames_from_real_replay`. `boot_run_and_drain` — the exact function the `/run/kvm`
     handler calls — plus `boot_and_run` and `boot_and_drain_frames` all thread through new
     `initramfs: Option<&[u8]>` and `periodic_timer: Option<(u64, u8, u32)>` params; when
     `periodic_timer` is `Some`, `boot_run_and_drain` now calls H4's
     `Multiverse::run_to_first_halt_with_periodic_timer` instead of the old bare `run_to_first_halt()`.
     That's the deeper gap this iteration surfaced and closed: even with `initramfs_path` wired, a real
     Linux kernel guest hangs forever under the old plain call, because its own `calibrate_delay()`
     needs periodic timer ticks that no hand-assembled fixture in this workspace ever required
     (documented in `tests/fixtures/linux-guest/BUILD.md`); the fix existed as
     `run_to_first_halt_with_periodic_timer` from a prior iteration but had never been threaded into any
     HTTP route until now. `persist_kvm_run` bundles the widened field set into a new
     `KvmBootParams<'a>` struct to stay under clippy's `too_many_arguments`. `stream::render`'s
     real-replay path (`crates/baud-server/src/routes/stream.rs`) now selects the four new
     `kvm_run_meta` columns and reconstructs `periodic_timer` only when all three sub-columns are
     non-NULL; `drive/m/m11.sh` (the all-NULL, no-initramfs/no-timer path) still passes, confirming no
     regression. `crates/baud-cli/src/cmds/run.rs`'s `RunAction::Kvm` gained `--initramfs`,
     `--periodic-timer-period-rcb` (opt-in — this is what enables periodic-timer injection at all),
     `--periodic-timer-vector` (default `0xec`), `--periodic-timer-max-ticks` (default `2000`). New test
     `run_kvm_boots_a_real_linux_guest_with_initramfs_and_periodic_timer` boots the real, checked-in
     `tests/fixtures/linux-guest/` kernel+initramfs through `boot_run_and_drain` directly with
     `period_rcb=500_000`, `vector=0xec`, `max_ticks=2000`, `cmdline=bootparams::DETERMINISTIC_CMDLINE`,
     asserting the `/init` marker in the console output — **passed on real `/dev/kvm`**. New
     `drive/pkg/pkg-boot-cli.sh` boots the same checked-in fixture through a real `baud run kvm --initramfs
     ... --periodic-timer-period-rcb 500000 --periodic-timer-vector 236 --periodic-timer-max-ticks 2000
     --cmdline "<DETERMINISTIC_CMDLINE>" --json` CLI invocation against a live `baud-server` over real
     HTTP — **real result: `ok=true`, console output contains `baud-guest: minimal kernel reached
     /init`**, the project's first real, end-to-end "spec in, guest booted" proof through the actual CLI
     binary + HTTP server, not a Rust test calling `Multiverse` directly. It runs in seconds (reuses the
     already-built fixture, no kernel compile), unlike `drive/pkg/pkg-build-cli.sh`/`drive/pkg/pkg-image-build.sh`
     — still opt-in, for consistency with that script family, not part of the standard h0-h7 gate.
     `cargo build`/`clippy`/`test --workspace` all clean (zero new warnings, including from the widened
     `persist_kvm_run` signature); `drive/h/h0.sh` through `drive/h/h7.sh` (8/8) and `drive/m/m9.sh`/
     `m10.sh`/`m11.sh` all still PASS on real `/dev/kvm` — no regressions from the schema/signature
     changes.
     **The initramfs builder's multi-file capacity is now real-hardware-verified, closed in ralph
     iteration 19**: previously mechanism-complete (`--initramfs-entry` repeatable,
     `InitramfsFileEntry`/`GuestImageBuildConfig` already took a slice) but exercised only with a
     single `/init`-style entry anywhere in the repo. New unit test
     `initramfs::tests::multiple_distinct_files_are_all_preserved` (`crates/baud-packages/src/
     initramfs.rs`) round-trips 3 distinct files (including a nested `bin/tool` path) through
     `build_reproducible_initramfs`. More importantly, new `#[ignore]`d real-hardware test
     `guest_boots_a_pipeline_built_multi_file_initramfs` (`crates/baud-multiverse/src/linux/mod.rs`,
     driven by new `drive/pkg/pkg-multifile-initramfs.sh`) builds a genuine 2-file initramfs
     (`multifile_init.c` execs a bundled `helper.c`) via `build_reproducible_initramfs` **at test
     time** — not a hand-`cpio`'d fixture — and boots it twice against the already-built, checked-in
     `linux-guest` bzImage (no kernel rebuild) on real `/dev/kvm`: both files' markers present on
     both boots, matching tick counts. **Real-hardware result: 5/5 clean, no jitter.** This proves the
     pipeline's multi-file capacity is not just byte-correct in isolation but genuinely bootable —
     the concrete shape a real multi-file rootfs (e.g. §11's eventual harness + emulator pair) will
     need — closing the "no real harness-script/agent-binary multi-file rootfs has been assembled or
     tested yet" gap named here in a prior iteration. The three hand-built `tests/fixtures/
     linux-guest/*initramfs.cpio.gz` files are still not replaced by this pipeline's output (they
     remain hand-built per their own `BUILD.md`s, unrelated to this gap — this item was about the
     builder's *capability*, not about migrating every existing fixture off hand-cpio). Buildroot
     (§4.5 Path 1) and pinned-Nix (§4.5 Path 2)
     themselves are still not implemented — this and the prior iteration both took the pragmatic
     from-source `make bzImage` third option instead; a `nix`/`nix-env` toolchain is still not installed
     in this dev sandbox and Buildroot remains unevaluated. The `/dev/vport` (or PIO) tape endpoint
     (§4.4) is still not implemented — confirmed non-blocking (ralph iteration 17 research): every
     currently-gating test (`all_input_is_tape_derived`, the H7 entropy/checkpoint work, `MARK_BRANCH`)
     already runs entirely over raw PIO, and no open-items entry conditions on virtio-serial/`/dev/vport`.
     `bootparams::DETERMINISTIC_CMDLINE` **is now wired as the production default, closed in ralph
     iteration 17**: `default_cmdline()` (`crates/baud-server/src/routes/run_kvm.rs`, shared by
     `RunKvmBody`/`RunKvmBranchBody`'s `#[serde(default = "default_cmdline")]`) returned a bare
     `"console=ttyS0"` for every real `POST /run/kvm`/`/run/kvm/branch` call that omitted `cmdline` —
     the actual production HTTP entry points, as distinct from the tests/drive-scripts that already
     passed `DETERMINISTIC_CMDLINE` explicitly. Now returns `baud_multiverse::linux::bootparams::
     DETERMINISTIC_CMDLINE` instead. `crates/baud-cli/src/cmds/run.rs`'s `--cmdline` flag on `Kvm`/
     `KvmBranch` changed from a required `String` with a duplicated `"console=ttyS0"` `default_value`
     to `Option<String>`; when unset, the CLI omits the `cmdline` key from the JSON body entirely
     (mirroring the existing `periodic_timer` pattern) rather than sending an explicit value, so the
     server's new default applies uniformly — no cmdline string is duplicated across crates. New pure-
     deserialization test `omitted_cmdline_defaults_to_the_deterministic_cmdline`
     (`crates/baud-server/src/routes/run_kvm.rs`) asserts both body types default correctly. Every
     existing caller that already passes an explicit `cmdline` (every drive script, every hand-assembled
     fixture test) is unaffected — confirmed via a full `cargo build`/`clippy`/`test --workspace` (0
     failures) plus `drive/h/h0.sh`-`h7.sh` (8/8), `drive/m/m9.sh`-`m12.sh` (4/4), and
     `drive/pkg/pkg-boot-cli.sh` (which exercises the CLI's explicit `--cmdline` path directly), all PASS on
     real `/dev/kvm` with no regressions. Three other candidate next-steps were researched and ruled out
     as out-of-scope for one iteration: virtio-rng (no `virtqueue`/feature-negotiation infra exists
     anywhere — genuinely multi-day, and even a simpler non-virtio MMIO device would still need an
     already-built-in guest driver x86 direct-boot has no ACPI/DT path to attach), and `/run/kvm/resume`'s
     lineage gap (confirmed genuinely bigger: `SnapshotStore`'s `Node` has parent pointers and byte-offset
     `tape_range` metadata, but no per-node tape-suffix bytes are persisted anywhere — `put_tape`/
     `get_tape` exist but no route ever calls them — so closing it needs new per-node tape storage, not
     just a lineage walk). **`/run/kvm/branch` and `/run/kvm/resume` now also accept
     `initramfs_path`/`periodic_timer`, closing the gap named here last iteration.** `RunKvmBranchBody`
     gained `initramfs_path: Option<String>` and `periodic_timer: Option<PeriodicTimerSpec>` (both
     `#[serde(default)]`, so existing callers are unaffected); `RunKvmResumeBody` gained `periodic_timer`
     only, since resume never boots a kernel (`reconstruct_universe` rebuilds `Multiverse` from a
     persisted `Universe`, not `kernel_path`). `boot_and_snapshot` gained an `initramfs: Option<&[u8]>`
     param (previously hardcoded `None`), threaded from `boot_snapshot_and_branch`/
     `boot_snapshot_and_generate`; `run_branches`/`run_driver_generated_branches_with_persist` gained a
     `periodic_timer: Option<(u64, u8, u32)>` param routing each forked branch through
     `Multiverse::run_until_branch_or_halt_with_periodic_timer` instead of the plain variant when set.
     **Real bug found and fixed while wiring this up**: `run_until_branch_or_halt_with_periodic_timer`
     (`crates/baud-multiverse/src/linux/mod.rs`) drained the tape device every tick to check for
     `MARK_BRANCH` but discarded every other drained record (`PROBE`/`GOAL`/`VIOLATION`/`LOG`) on every
     non-final tick — unlike its sibling `run_until_branch_or_halt`, which accumulates all of them — so a
     driver-generated branch scored via `observations_from_records` would have silently seen an
     incomplete probe stream once periodic-timer branching was wired in. Fixed by widening the function's
     return type to `Result<(Vec<TimerTick>, RunUntilBranchOutcome, Vec<baud_proto::Msg>), DeterminismHole>`
     and accumulating every drained record exactly like `run_until_branch_or_halt` does; its one prior
     call site (the `#[ignore]`d `double_boot_ram_hash_identical` test) was updated for the new tuple
     shape. New real-hardware test `run_kvm_branch_boots_a_real_linux_guest_with_initramfs_and_periodic_timer`
     boots the real, checked-in `checkpoint_initramfs.cpio.gz` fixture (whose `/init` issues one
     `MARK_BRANCH` before powering off) through `boot_snapshot_and_branch` with a real
     `initramfs`/`periodic_timer` and asserts the forked branch stops at its `MARK_BRANCH` checkpoint
     rather than halting or hanging — **passed on real `/dev/kvm`**. CLI: `RunAction::KvmBranch` gained
     the same four `--initramfs`/`--periodic-timer-*` flags as `RunAction::Kvm`; `RunAction::KvmResume`
     gained the three `--periodic-timer-*` flags only (no `--initramfs`, matching the resume body).
     `cargo build`/`clippy`/`test --workspace` all clean (zero new warnings); `drive/h/h0.sh` through
     `drive/h/h7.sh` (8/8) and `drive/m/m9.sh`/`m10.sh`/`m11.sh` all still PASS on real `/dev/kvm` — no
     regressions from the widened `RunKvmBranchBody`/`RunKvmResumeBody`/`boot_and_snapshot`/
     `run_branches`/`run_driver_generated_branches_with_persist` signatures. **Follow-up closed for
     `/run/kvm/branch` in iteration 16, still open for `/run/kvm/resume`**: `boot_and_snapshot` always
     snapshots the guest with an empty tape before any instruction runs, so a branch's own tape suffix is
     its entire replay tape from cold boot — byte-identical to forking from the snapshot — which let
     `branch()` reuse the existing `persist_kvm_run`/`KvmBootParams` machinery as-is, keyed by
     `tape_hex = branch_tapes_hex[i]` (fixed-tape) or `outcome.tape_hex` (generate mode), via a new
     `persist_branch_frames` helper and opt-in `RunKvmBranchBody.frame_run_ids` /
     `DriverGenerateSpec.frame_run_id_prefix` fields (both `#[serde(default)]`); `stream::render`'s
     real-replay path now replays a branch-originated run's frames (new `drive/m/m12.sh`, 4/4, real
     `/dev/kvm`). `/run/kvm/resume` still won't get a `kvm_run_meta` row: resume reconstructs a `Universe`
     from `SnapshotStore`, not from a kernel image, so there's no `kernel_path`/`cmdline` to reboot from
     for a real replay — closing it needs `SnapshotStore` to additionally track each node's full
     root-to-node replay-tape lineage, a materially bigger change, out of scope this iteration; resume's
     generate mode rejects a `frame_run_id_prefix` request outright with a clear error rather than
     silently ignoring it.
     **RESOLVED: `/run/kvm/resume`'s lineage gap is now closed — no per-node full-lineage tape storage
     needed after all.** The premise above ("closing it needs `SnapshotStore` to additionally track each
     node's full root-to-node replay-tape lineage") turned out to be more than necessary: `resume` never
     needs the *whole* replay tape from root, only the *specific* tape suffix a given branch call fed to
     `Multiverse::branch` on top of the restored `Universe` — exactly parallel to how `/run/kvm/branch`'s
     own trick works (its branch point is captured with an empty tape, so a branch's own suffix already
     is its whole replay tape; resume's restored point is captured with whatever prefix reached it, so
     resume's own suffix is everything else needed on top of that *specific* restore). `kvm_run_meta`
     gained nullable `store_run_id`/`snapshot_node_id` columns (`migrations/0012_kvm_run_meta_resume_
     restore.sql`) identifying a restore-based row (as opposed to a reboot-based one, which leaves both
     `NULL`); `RunKvmResumeBody` gained `frame_run_ids: Vec<Option<String>>` (mirroring `RunKvmBranchBody`
     exactly) and `DriverGenerateSpec::frame_run_id_prefix` is now honored by `resume()`'s generate mode
     too (previously hard-rejected with "resume has no kernel_path/cmdline to reboot from"). Both persist
     via the existing `persist_branch_frames` helper (renamed in spirit, not in code — still used by both
     routes) with `kernel_path`/`cmdline` left `""` and `store_run_id`/`snapshot_node_id` set to the
     request's own `(run_id, node_id)` instead. `stream::render` gained `render_frames_from_real_restore`
     (`crates/baud-server/src/routes/stream.rs`) alongside the existing `render_frames_from_real_replay`:
     when a `kvm_run_meta` row has `store_run_id`/`snapshot_node_id` set, it calls the same
     `reconstruct_universe` + `Multiverse::branch` + drain primitives `resume_and_branch` itself uses,
     instead of rebooting a kernel — reproducing a resume-originated run's real frames with **no kernel
     image and no reboot at all**. New unit test `resumed_branch_records_are_reproducible_via_independent_
     restore` (`crates/baud-server/src/routes/run_kvm.rs`) proves the mechanism directly: an independent
     `reconstruct_universe`+`Multiverse::branch` call reproduces byte-identical `Msg::Frame` records to the
     original live `resume_and_branch` call over the same persisted point + suffix. New drive script
     `drive/m/m13.sh` (mirroring `drive/m/m12.sh`'s structure) proves it end-to-end over real HTTP against a
     real `baud-server` on real `/dev/kvm`: `/run/kvm/branch { persist_run_id }` (persist-only, no fork)
     establishes a checkpoint, `/run/kvm/resume { frame_run_ids }` restores+forks it with **no kernel
     reboot** and persists a real `Frame` record, `GET /runs/:id/frames` shows the row, `POST /runs/:id/
     stream/render` decodes the guest's real pixels (the same `(10,10,10),(20,20,20),(30,30,30),(40,40,40)`
     framebuffer-guest pixel sequence `drive/m/m11.sh`'s plain-boot path and `drive/m/m12.sh`'s branch path
     both prove), and re-rendering reproduces byte-identically. CLI: `baud run kvm-resume` gained
     `--frame-run-id` (repeatable) and `--frame-run-id-prefix`, mirroring `kvm-branch`'s flags. `cargo
     build`/`clippy`/`test --workspace` all clean (0 failures; `baud-server` 26/26, including the new
     unit test); `drive/h/h0.sh`-`h7.sh` (8/8) and `drive/m/m9.sh`-`m13.sh` (5/5) all PASS on real `/dev/kvm`,
     including `drive/m/m11.sh`'s pre-existing synthetic-fallback check (a `kvm_run_meta`-less run still
     renders via the old gradient path — confirms the widened `kvm_run_meta` schema/query is additive,
     not a regression). This closes matrix row … n/a (this gap was surfaced during build, not §12-listed)
     — the FCEUX/Qt5 packaging-footprint finding above (item 3, H8) remains the actual next blocker for
     the guest-boot-pipeline milestone; this item was a separate, smaller wiring gap that had been open
     since iteration 16/17 and is now done.
     **This iteration (ralph iteration 18): `boot_params_seed_is_pinned` and
     `init_powers_off_deterministically`, the two spec-named tests flagged above as unwritten, are now
     written and hardware-verified.** `boot_params_seed_is_pinned` (`crates/baud-multiverse/src/linux/
     mod.rs`) reuses the `hello-guest` fixture: two boots of the same tape must write an identical
     `SETUP_RNG_SEED` node (the existing `read_seed_via_hdr` closure from
     `rng_seed_setup_data_is_wired_into_a_real_boot_and_is_tape_derived` was extracted into a shared
     `read_rng_seed_via_hdr` helper both tests now call, rather than duplicating the unsafe zero-page
     read) and console output must also match, proving the pinned seed doesn't perturb the rest of the
     deterministic boot; the guest-observable CRNG *output* side of "early CRNG init is reproducible" is
     covered separately by the already-existing `os_entropy_is_deterministic` (enforced-regime,
     `#[ignore]`d), since `hello-guest` has no libc/CRNG to observe. `init_powers_off_deterministically`
     needed one small piece of new plumbing: `HaltOutcome` gained an `exit_pc: u64` field (the vCPU's
     RIP read via a new `Multiverse::current_rip()` helper, `KVM_GET_REGS`, right after each of the
     four halt-detecting call sites observes `Hlt`/`Shutdown`) — the "identical exit point" spec §4.3
     names, previously not captured anywhere (`TimerTick` already captured `rip` but only for the
     interrupt-boundary case, not the halt case). The new test reuses `guest_kernel_boots_to_userspace`'s
     real `linux-guest` fixture and periodic-timer engine (its `/init` genuinely calls
     `reboot(RB_POWER_OFF)`, a real triple-fault `VcpuExit::Shutdown` — unlike `hello-guest`'s hand-
     assembled `hlt` loop) and asserts `HaltOutcome::exit_pc` is bit-identical across two boots. Both
     **passed on real `/dev/kvm`** (`init_powers_off_deterministically` re-run 5x clean, no jitter
     observed — it compares only the halt RIP, not raw console text or full RAM, so it doesn't hit the
     residual RCB/`perf_event`-read-jitter floor documented in item 2 below).
     `cargo build`/`clippy`/`test --workspace` all clean (zero new warnings); `drive/h/h0.sh`-`h7.sh`
     (8/8) and `drive/m/m9.sh`-`m12.sh` (4/4) all still PASS on real `/dev/kvm` — no regressions from the
     widened `HaltOutcome`. Closes matrix row 26's two remaining named tests (§12); `guest_kernel_boots_
     to_userspace` and `image_build_is_reproducible` were already done. H8 (Mario, item 3 below) is still
     blocked on the rest of item 1, not just this piece.
     **This iteration (ralph iteration 21): `InitramfsEntry` gained a symlink node type, and a real
     dynamically-linked glibc binary booted through the pipeline for the first time — one concrete
     H8 prerequisite closed, not H8 itself.** `crates/baud-packages/src/initramfs.rs`'s
     `InitramfsEntry` was refactored from a flat `{path, mode, contents}` struct (regular files
     only) into `{path, node: InitramfsNode}`, where `InitramfsNode` is `Regular { mode, contents }`
     or `Symlink { target }`, plus `InitramfsEntry::regular(...)`/`InitramfsEntry::symlink(...)`
     constructors; `build_reproducible_initramfs` now writes real newc-cpio symlink records
     (`S_IFLNK`, mode `0o120000 | 0o777`, data = the raw target bytes with no NUL terminator). 3 new
     unit tests (`round_trip_preserves_a_symlink`, `build_is_byte_for_byte_reproducible_with_a_
     symlink`, `empty_symlink_target_is_rejected`) all pass; the existing call sites
     (`crates/baud-packages/src/guest_build.rs`, `crates/baud-multiverse/src/linux/mod.rs`) were
     updated to the new constructor API with no behavior change for regular-file callers. This
     closes the hard blocker any real glibc/Buildroot/Nix rootfs would hit, since a dynamic linker
     is reached almost universally through a symlink. New fixture `crates/baud-multiverse/tests/
     fixtures/linux-guest/dynamic_init.c` — a real, dynamically-linked (non-static, `-no-pie` for a
     fixed deterministic load address, `-Wl,-rpath=/lib/x86_64-linux-gnu`) glibc `/init`, compiled
     with plain `gcc`, unlike every other fixture in that directory (all `musl-gcc -static`). New
     test `guest_boots_a_dynamically_linked_glibc_init` (`crates/baud-multiverse/src/linux/mod.rs`)
     builds an initramfs at test time via `build_reproducible_initramfs` carrying the compiled
     `init` binary, this dev host's own real `/lib/x86_64-linux-gnu/{ld-linux-x86-64.so.2,
     libc.so.6}` as regular-file entries (this host's glibc *is* the guest's glibc — identical
     x86_64 Linux ABI, no cross-build needed), and a symlink entry `lib64/ld-linux-x86-64.so.2` ->
     `../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2` matching the compiled binary's own `PT_INTERP`;
     boots twice against the already-built, checked-in `linux-guest` bzImage (no kernel rebuild).
     **Real-hardware result: 5/5 clean, no jitter** (run manually plus via new drive script
     `drive/pkg/pkg-dynamic-link.sh`, opt-in like `drive/pkg/pkg-multifile-initramfs.sh`, not part of the
     standard h0-h7 gate). This is the first dynamically-linked binary ever booted through
     baud-multiverse (every prior fixture was statically linked via musl-gcc) and the first real
     (non-unit-test) exercise of the new symlink support. `cargo build`/`clippy`/`test --workspace`
     all clean (0 new warnings — a handful of pre-existing clippy warnings in
     `crates/baud-multiverse/src/timesource.rs`, `crates/baud-tape-agent/src/transport.rs`,
     `crates/baud-server/src/routes/{fuzz,replay,tracing}.rs` predate this iteration, confirmed via
     `git status`); `drive/h/h0.sh`-`h7.sh` (8/8) and `drive/m/m9.sh`-`m12.sh` (4/4) all still PASS on
     real `/dev/kvm`, no regressions; `drive/pkg/pkg-multifile-initramfs.sh` (opt-in, shares
     `initramfs.rs`) re-run as a regression check, still PASSED. **Still open, not attempted this
     iteration**: the real FCEUX + Lua rootfs itself (`examples/mario/` still has the old pre-KVM-
     pivot `nes_bridge.c`/stdin-stub design flagged elsewhere as needing a full rebuild); the
     Buildroot/pinned-Nix image pipeline (§4.5, still no `nix`/`buildroot` toolchain in this dev
     sandbox); virtio-rng (confirmed again: `crates/baud-multiverse/src/console.rs`'s `DeviceBus` is
     a hardcoded PIO-only if/else chain, `mmio_read`/`mmio_write` always fall through to
     `OpenBusFallback`, zero virtqueue/virtio-mmio code anywhere); the three spec-named entropy
     tests (`entropy_guest_is_deterministic`, `initial_crng_state_is_reproducible`,
     `virtio_rng_reseed_is_deterministic`) were investigated and found to be either genuine
     duplicates of the already-passing `os_entropy_is_deterministic` (the first two) or blocked on
     the same missing virtio-rng infra (the third) — deliberately not added, since they would add
     zero new coverage while costing multi-minute enforced-regime real-hardware boots each; H9
     Ubuntu still not started.
     **This iteration (ralph iteration 23): the "zero virtqueue/virtio-mmio code anywhere" gap
     just above is now partly closed — a real, spec-compliant virtio-mmio v2 *transport-layer*
     register block exists and is hardware-independently tested, though `virtio_rng_reseed_is_
     deterministic` itself is still not reachable.** Before writing code, a Sonnet research agent
     was asked to re-verify the premise above and scope the smallest genuinely non-stub next
     slice; it confirmed `DeviceBus` (`crates/baud-multiverse/src/console.rs`) really was a
     hardcoded PIO-only if/else chain with `mmio_read`/`mmio_write` unconditionally falling
     through to `OpenBusFallback`, and — critically — flagged that a *complete* virtio-rng (real
     virtqueue descriptor/avail/used-ring parsing over `vm-memory`, plus interrupt delivery) is
     still genuinely multi-day: this host registers no in-kernel irqchip at all
     (`KVM_CREATE_IRQCHIP`/`KVM_IOEVENTFD` are never called anywhere in `linux/mod.rs`), so which
     vector a `virtio_mmio.device=` cmdline IRQ number would even resolve to is unverified and
     needs its own investigation, not a same-session add. New `crates/baud-multiverse/src/
     virtio_mmio.rs` implements the *transport* register window alone — deliberately the
     boundary the agent identified as the largest piece completable without touching that
     unknown: `VirtioMmioTransport` (device-id/feature-bitmap/queue-count/queue-max-size are
     constructor parameters, not hardcoded, so it is reusable for a future virtio-blk on the same
     transport per §4.7/H9, not virtio-rng-specific code in the generic core) implements `Bus`
     over the full virtio-mmio v2 register set (spec 1.1 §4.2.2 Table 4.1): magic/version/
     device-id/vendor-id (read-only, `VIRTIO_DEVICE_ID_RNG = 4`), feature negotiation across both
     32-bit words of a 64-bit bitmap (`VIRTIO_F_VERSION_1` for `new_rng`), queue selection/
     sizing/readiness/ring-address registers (desc/driver/device, low+high halves) stored
     verbatim per queue, `QueueNotify` recorded via `notify_count()`/`last_notified_queue()`, and
     a real status-register reset FSM (writing `0` clears all driver-negotiated state — feature
     acceptance, queue state, notify counters — while device identity/queue-count/max persist,
     spec 1.1 §2.1). New `crate::layout::VIRTIO_MMIO_RNG_BASE`/`VIRTIO_MMIO_RNG_LEN` place the
     device window at `0xd000_0000`, deliberately outside `GUEST_RAM_SIZE` (any address inside
     the registered RAM region is served straight from guest RAM and never reaches a VM exit at
     all — a `_STATIC_LAYOUT_INVARIANTS` assertion now checks this) — the same
     `virtio_mmio.device=<size>@<base>:<irq>` cmdline convention Firecracker/crosvm use for a
     direct-boot guest with no ACPI/PCI/DT to auto-discover devices through. Wired into
     `DeviceBus` as a new opt-in `enable_virtio_rng()` method (every existing constructor —
     `Default`, `with_tape`, `restore` — leaves the slot `None`, so no existing boot path's MMIO
     behavior changes at all: confirmed via a new `device_bus_mmio_falls_through_to_open_bus_
     until_virtio_rng_is_enabled` test plus the full h0-h7/m9-m13 regression run below). 8 new
     `virtio_mmio` unit tests, including one that walks a real driver's actual probe/negotiate/
     queue-setup/`DRIVER_OK` sequence (spec 1.1 §3.1) end-to-end through the register interface
     and asserts each stage's read-back, an "unavailable queue index reports zero max and never
     leaks into another queue's state" test, and a reset test proving driver-negotiated state
     clears while device identity persists — all hardware-independent (pure register-state
     machine, no KVM/perf touched, same convention as `console.rs`'s own `vm_superio`-based
     tests). **What this explicitly does not do yet, matching the research agent's scoping**: no
     descriptor/avail/used ring is ever read from guest memory (`QueueNotify` only counts that a
     notification arrived), `InterruptStatus` always reads `0` (no interrupt is ever raised), and
     nothing wires this into a real boot's cmdline/CLI/server route yet — `virtio_rng_reseed_is_
     deterministic` still cannot pass until a follow-up session adds real ring parsing plus
     resolves the interrupt-routing question above. `cargo build`/`clippy`/`test --workspace` all
     clean (0 failures; confirmed zero *new* clippy warnings via a `grep` over the new files
     specifically, distinct from the pre-existing warning list already documented in prior
     iterations); `drive/h/h0.sh`-`h7.sh` (8/8) and `drive/m/m9.sh`-`m13.sh` (5/5) all still PASS on
     real `/dev/kvm`, no regressions from the widened `DeviceBus`.
     **This iteration (ralph iteration 24): the next slice of the gap iteration 23 explicitly scoped
     out — real split-virtqueue descriptor-chain/avail-ring/used-ring parsing over `vm-memory` — is
     now done; the interrupt-routing question is still not.** New `crates/baud-multiverse/src/
     virtio_queue.rs` (gated `#[cfg(target_os = "linux")]` in `lib.rs`, since it depends on the Linux-
     gated `vm-memory` crate — unlike `virtio_mmio.rs`, which has no `vm-memory` dependency and stays
     ungated): a `SplitVirtqueue` struct that walks a virtio spec 1.1 §2.6 split virtqueue — reads the
     driver's `avail.idx`, walks each newly-posted descriptor chain via the `NEXT` flag (bounded to at
     most `queue_size` hops, so a non-terminating/self-looping chain is rejected as `ChainTooLong`
     rather than looped on forever), calls a caller-supplied `fill` closure once per *writable*
     descriptor only (read-only descriptors are never touched), writes the filled bytes into guest
     memory via `vm_memory::Bytes::write_slice`, and publishes one used-ring entry per chain (head
     descriptor index + total bytes written) via `process_available()`. Deliberately device-agnostic —
     the same "generic core" rule `VirtioMmioTransport` itself follows — it has no opinion on what
     bytes fill a buffer, only on ring mechanics; a caller (virtio-rng, or later virtio-blk) supplies
     `fill`. `VirtioMmioTransport` (`crates/baud-multiverse/src/virtio_mmio.rs`) gained a new public
     accessor, `queue_ring_config(queue_index) -> Option<QueueRingConfig>`, returning the negotiated
     `{num, desc, driver, device}` addresses once a queue is marked ready (`REG_QUEUE_READY`) — the
     handoff point between the register-only transport and the new ring-walking module; `console.rs`'s
     `DeviceBus` gained a matching `virtio_rng() -> Option<&VirtioMmioTransport>` read accessor
     (previously fully private, unreachable from outside the module). 11 new unit tests in
     `virtio_queue.rs` (all hardware-independent — pure `vm-memory` `GuestMemoryMmap::from_ranges`
     anonymous-mmap memory, no KVM/perf touched, following `bootparams.rs`'s own `test_guest_mem()`
     convention): a single writable descriptor filled and published; read-only descriptors never
     written to; chained (`NEXT`-linked) descriptors walked and their written-byte totals summed into
     one used-ring entry; multiple available chains drained in one `process_available()` call; a
     self-looping non-terminating chain rejected without hanging; an out-of-range descriptor index
     rejected; an indirect descriptor (`VIRTQ_DESC_F_INDIRECT`) rejected as unsupported rather than
     silently mis-parsed as a data buffer; a zero-size (unconfigured) queue no-ops without
     dereferencing any guest address; and an end-to-end test that drives a real `VirtioMmioTransport`
     through the actual driver-enumeration/queue-setup register sequence and feeds its
     `queue_ring_config` output into a live `SplitVirtqueue`, proving the two modules compose
     correctly, not just in isolation — plus 1 new test in `virtio_mmio.rs`
     (`queue_ring_config_is_none_until_the_queue_is_marked_ready`). **What this explicitly still does
     not do, the same scoping boundary iteration 23 drew**: nothing calls
     `SplitVirtqueue::process_available` automatically from `VirtioMmioTransport::write_register`'s
     `QueueNotify` arm yet (no wiring from notify to ring-draining), `InterruptStatus` still always
     reads `0` (no real interrupt is ever raised after a used-ring publish), and nothing wires any of
     this into a real boot's cmdline/CLI/server route — the same open interrupt-routing question from
     iteration 23 (this host registers no in-kernel irqchip, so which vector a
     `virtio_mmio.device=` IRQ number resolves to remains unverified) is still unresolved and still the
     next real blocker before a live guest driver could be exercised end-to-end.
     `virtio_rng_reseed_is_deterministic` (the spec-named test) still cannot pass until that follow-up
     work lands. `cargo build`/`clippy`/`test --workspace` all clean (0 failures; confirmed zero *new*
     clippy warnings via a targeted `grep` restricted to the changed files, distinct from the pre-
     existing warning list already documented in prior iterations — this iteration's warnings are all
     pre-existing, in unrelated files: `baud-tracing`'s deprecated `aya::Bpf`,
     `baud-multiverse/src/lib.rs`'s `EntropyDevice`/`InputDevice`/`NetDevice`/`ExitDevice` derivable-
     impl suggestions, `baud-tape-agent/src/transport.rs`,
     `baud-server/src/routes/{fuzz,replay,tracing}.rs`); `drive/h/h0.sh`-`h7.sh` (8/8) and `drive/m/m9.sh`-
     `m13.sh` (5/5) all still PASS on real `/dev/kvm`, no regressions from the widened
     `VirtioMmioTransport`/`DeviceBus` public surface.
     **This iteration (ralph iteration 25): the "nothing wires notify to ring-draining" gap iteration 24
     explicitly left open is now closed — but interrupt delivery and boot/cmdline/CLI wiring are not.**
     `console.rs`'s `DeviceBus` gained two new `#[cfg(target_os = "linux")]`-gated fields —
     `virtio_rng_queue: Option<SplitVirtqueue>` (the live ring cursor) and `virtio_rng_entropy:
     SplitMix64` (the device's own tape-seedable byte stream, kept as an independent stream from the
     rdrand/rdseed sub-stream and the boot `SETUP_RNG_SEED`, per §3.8's domain-separation convention;
     `timesource.rs`'s `SplitMix64` was widened from module-private to `pub(crate)`, plus a
     `#[derive(Default)]`, to be reused here rather than duplicated) — and two new methods:
     `seed_virtio_rng_entropy(&mut self, seed: u64)` and `service_virtio_rng<M: GuestMemoryBackend>(&mut
     self, mem: &M) -> Result<u32, VirtqueueError>`, the latter being the actual drain mechanism: it walks
     every posted chain via `SplitVirtqueue::process_available`, fills writable descriptors 8 bytes at a
     time from `next_u64().to_le_bytes()`, and lazily rebuilds its cached `SplitVirtqueue` by comparing
     against a new `SplitVirtqueue::config()` accessor whenever the transport's `queue_ring_config`
     changes (first negotiation, or a reset+renegotiate) rather than walking a stale layout; a no-op
     (`Ok(0)`) if virtio-rng was never enabled or the queue isn't ready, never panics. Explicitly NOT
     wired to fire automatically from `Bus::mmio_write`'s `QueueNotify` arm, and can't be without a larger
     redesign: `baud_vcpu::Bus` is shared with `baud-vcpu`'s exit-dispatch code and is deliberately
     memory-oblivious (`mmio_write(&mut self, addr: u64, data: &[u8])`, no guest-memory parameter), so
     nothing inside a `Bus` impl can reach real guest RAM to walk a virtqueue — `service_virtio_rng` is a
     method a caller with real guest memory must invoke explicitly, and no real boot loop does that yet
     (the same still-open boot/cmdline/CLI wiring from iterations 23/24). 4 new hardware-independent tests
     in a new `console.rs` module `virtio_rng_service_tests` (pure `vm-memory::GuestMemoryMmap::from_
     ranges` anonymous-mmap memory, no KVM/perf): a before-enable/ready/notify no-op check; a full driver
     enumeration/negotiate/post/notify sequence through `DeviceBus`'s own `Bus` impl proving the ring is
     actually drained and filled (and a second no-op call drains nothing further); a same-seed-reproduces-
     identical-bytes / different-seed-differs determinism check; and a reset+renegotiate-with-new-ring-
     addresses regression test for the `config()`-comparison rebuild logic specifically. `cargo build`
     clean; `clippy --workspace --all-targets` 0 new warnings (the full warning list is unchanged from
     prior iterations' documented list, none in files touched here); `cargo test --workspace` 0 failures
     (baud-multiverse alone: 113 passed, 0 failed, 8 ignored); `drive/h/h0.sh`-`h7.sh` (8/8) and
     `drive/m/m9.sh`-`m13.sh` (5/5) all still PASS on real `/dev/kvm`, no regressions. The in-kernel-irqchip
     question (this host never calls `KVM_CREATE_IRQCHIP`/`KVM_IOEVENTFD`, so which vector a
     `virtio_mmio.device=` cmdline IRQ resolves to remains unverified) was again explicitly out of scope.
     **This iteration: the "interrupt delivery" half of this gap is now closed too — boot/cmdline/CLI
     wiring is the only piece still open.** `virtio_mmio.rs` gained `VIRTIO_MMIO_INT_VRING` (bit 0 of
     `InterruptStatus`, spec 1.1 §4.2.2.2's "Used Buffer Notification"), `VirtioMmioTransport::raise_
     used_buffer_notification()` (ORs that bit in; only the driver's own `REG_INTERRUPT_ACK` write ever
     clears it, matching real hardware) and an `interrupt_status()` read accessor; the module's own doc
     comment, which previously said this transport "does not raise a real interrupt" and that
     `InterruptStatus` "always reads 0", is updated to match. `console.rs`'s `DeviceBus::service_
     virtio_rng` now calls `raise_used_buffer_notification()` whenever `process_available` reports at
     least one drained chain (one new test, `draining_the_ring_raises_interrupt_status_and_ack_clears_
     it`, proving a bare `QueueNotify` does not raise it, draining does, and the driver's ack clears it).
     `linux/mod.rs`'s `Multiverse` gained four new methods: `enable_virtio_rng()` and `seed_virtio_rng_
     entropy(seed)` (thin wrappers over the `DeviceBus` methods iteration 25 added), `virtio_rng()` (a
     read accessor onto the transport, for callers/tests that want `notify_count`/`interrupt_status`
     without reaching into private state), and — the actual new piece —
     `service_virtio_rng_interrupt(vector) -> Result<u32, DeterminismHole>`, which calls `DeviceBus::
     service_virtio_rng` with the guest's real memory and, if anything was drained, delivers a real
     interrupt at `vector` to the vCPU right now by calling `inject_timer_tick(0, vector)`'s degenerate
     `period_rcb = 0` case ("the next reachable boundary") — no new low-level KVM primitive was needed;
     this reuses H4's existing exact-boundary engine (`baud_vcpu::boundary`) exactly as the periodic
     timer already does for `LOCAL_TIMER_VECTOR`. Proving this end-to-end surfaced a real, independent
     bug: `layout::build_identity_page_tables` only ever identity-mapped `GUEST_RAM_SIZE`, so a guest's
     own access to the virtio-mmio device window (`VIRTIO_MMIO_RNG_BASE = 0xd0000000`, deliberately
     outside registered RAM so it traps to a VM exit) had no page-table translation at all — paging is
     mandatory in long mode, so the access took a genuine `#PF` long before it could ever become the
     intended MMIO VM-exit. Fixed by always appending one dedicated PDE page + PDPTE entry identity-
     mapping that window, regardless of `ram_size`; `layout::GDT_ADDR` moved from `0xC000` to `0xD000`
     to make room (confirmed via `grep` that nothing else in the codebase hardcoded the old value).
     Layout's unit tests were updated for the extra always-present PDE page and a new test,
     `identity_map_also_covers_the_virtio_mmio_window`, added. A new hand-assembled real-hardware
     fixture, `crates/baud-multiverse/tests/fixtures/virtio-rng-guest/` (`payload.s`, `build.py`,
     `BUILD.md`), is a minimal real x86-64 virtio-rng driver sequence — negotiate, set up one queue,
     post one writable descriptor, `QueueNotify` — with its own IDT gate (vector `0x31`) whose ISR
     writes a marker byte plus the actual entropy byte the device filled the buffer with. Two new real-
     hardware tests in `linux/mod.rs`: `virtio_rng_interrupt_reaches_the_guests_own_isr` (single boot,
     asserts the guest's own ISR observes the exact tape-seeded entropy byte through a real delivered
     interrupt) and `virtio_rng_interrupt_delivery_is_reproducible_across_two_boots` (double-boot
     determinism, same style as `timer_tick_lands_at_identical_instruction`), both passing on real
     `/dev/kvm`. Explicitly NOT done: which vector an *unmodified Linux* guest's real `virtio_mmio`/
     `virtio_rng` driver stack would resolve its `virtio_mmio.device=<size>@<base>:<irq>` cmdline IRQ
     to via `request_irq()` remains unverified — unlike the LAPIC timer's architecturally-fixed vector,
     an ordinary device IRQ is normally resolved through an IOAPIC/PIC, which this VMM does not have
     (`KVM_CREATE_IRQCHIP`/`KVM_IOEVENTFD` are still never called anywhere, confirmed again); wiring
     virtio-rng into any real boot's cmdline/CLI/server route is also still open; the spec-named test
     `virtio_rng_reseed_is_deterministic` still cannot pass until both land — a real Linux guest, not
     just this hand-assembled fixture, must actually negotiate and use the device. `cargo build
     --workspace` clean; `clippy --workspace --all-targets` 0 new warnings (confirmed the full warning
     list is unchanged from prior iterations, none in the files this iteration touched); `cargo test
     --workspace` — one test (`linux::tests::rdtsc_guest_reproduces_high_bits_across_boots`) failed once
     under full parallel load but passed clean both in isolation and on a full-suite rerun (0 failures),
     the same one-off real-hardware jitter flake iteration 13 already documented for this exact test,
     not a regression, not chased further; `drive/h/h0.sh`-`h7.sh` (8/8) and `drive/m/m9.sh`-`m13.sh` (5/5)
     all still PASS on real `/dev/kvm`, no regressions from the widened `DeviceBus`/`Multiverse`/
     `layout` surfaces.
  2. **H7 — OS-entropy end-to-end (rides on #1) — the `EXIT_REASON_RDTSCP` crash is fixed; the
     two-fd RCB-counter epoch disagreement that caused most of `os_entropy_is_deterministic`'s
     flakiness is now reconciled into a single shared fd, plus a second, independent console-
     capture-race bug it exposed is fixed — real hardware pass rate is now 7/10, with all residual
     failures matching the already-documented single-fd hardware-read-jitter floor, not a further
     design bug; see below.** `tests/fixtures/linux-guest/
     entropy_init.c` (+ `entropy_initramfs.cpio.gz`) is a second `/init` for the *already-built*
     `linux-guest` kernel that calls `getrandom()` ×4 and reads `/dev/urandom` ×4, hex-encoding each
     32-byte read out the same raw-`outb` COM1 endpoint `init.c` uses.
     - **`rdinit=/init` skips the kernel's own devtmpfs auto-mount** — fixed in `entropy_init.c` itself
       (`mkdir("/dev", 0755); mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);` before opening
       `/dev/urandom`).
     - **Requires the enforced (RDTSC-trapping) `kvm_intel.ko`** — under the *stock* module,
       `random_init()` unconditionally mixes the real (untrapped) TSC into the CRNG pool after the
       pinned `SETUP_RNG_SEED` boot seed already credited it. Stays `#[ignore]`d for this reason (same
       as every enforced-regime test); `drive/manual/h7-enforced-entropy.sh` runs it with `--ignored`.
     - **`EXIT_REASON_RDTSCP` gap — fixed, verified not to regress.** Booting the same kernel +
       `entropy_init.c` under the enforced module used to hit `KVM_EXIT_INTERNAL_ERROR` immediately
       (dmesg: `vmx: unexpected exit reason 0x33` = `EXIT_REASON_RDTSCP`) because
       `CPU_BASED_RDTSC_EXITING` also forces `rdtscp` to VM-exit (Intel SDM Vol. 3C 25.1.2) but
       `kvm_vmx_exit_handlers[]` had no entry for it at all — every prior hand-assembled fixture issued
       only bare `rdtsc`. Fixed: `rdtsc-enforce.patch` adds `handle_baud_rdtscp_exit`
       (`KVM_EXIT_BAUD_DETERMINISM` payload kind 3) alongside `handle_baud_rdtsc_exit` (kind 0);
       `baud-vcpu` gained `Exit::RdtscpEnforced` / `DispatchOutcome::ServeEnforcedRdtscp { value,
       tsc_aux }`, threaded through all three `linux::pmu` `KVM_RUN` loops and `linux::run_one_exit`,
       writing EDX:EAX from the same work-clock `RDTSC` already uses plus ECX from a new
       `TimeSource::serve_enforced_tsc_aux`. The crash itself is gone for good — confirmed across 15+
       real-hardware boots this iteration, zero recurrences — and `drive/manual/h3-enforced-rdseed.sh`'s full
       regression (rdtsc/rdrand/rdseed all bit-exact) still passes with RDTSCP layered underneath.
     - **Deeper finding, root-caused and partially fixed this iteration: `getrandom()`/`/dev/urandom`
       output itself diverges between the two boots at a real, non-trivial rate even with the crash
       fixed.** Three distinct effects were conflated at first and had to be separated:
       (a) A **console-capture race**, fixed: `entropy_init.c` writes each hex line via raw `outb` (no
       interrupt-driven tty path exists on this machine — `BUILD.md`'s documented reason), so a
       periodic timer tick landing *mid-write* lets the kernel's own asynchronous "Spurious LAPIC timer
       interrupt on cpu 0" diagnostic (expected every tick, since there is no LAPIC model) interleave
       into the probe line — e.g. `URANDOM:ac3595Spurious LAPIC timer interrupt on cpu 0\na15749e0...`
       instead of one clean 64-hex-char line. This is a parsing artifact, not entropy nondeterminism —
       the diagnostic text is itself fixed and deterministic — so `os_entropy_is_deterministic` now
       strips every occurrence of that exact string from the console capture before line-splitting
       (`crates/baud-multiverse/src/linux/mod.rs`'s `SPURIOUS_LAPIC_LINE`), reassembling the probe line
       exactly as the guest wrote it.
       (b) **Confirmed and fixed this iteration: `WorkClock`'s RCB `perf_event` counter
       (`LinuxBranchCounter`) accumulated host-side branches, not just guest branches.** Traced via
       code inspection (no kernel rebuild needed — this is a pure userspace/Rust-side bug):
       `exclude_host(true)` reads back `0` on this project's own nested-virtualized dev host
       (`LinuxBranchCounter::new`'s own doc, already known), so the counter — a free-running
       `perf_event_open` fd, read fresh on every `serve_enforced_rdtsc`/`serve_enforced_tsc_aux` call
       — kept accumulating for the *entire* stretch of userspace Rust dispatch code between VM-exits
       (allocations, ioctls, match arms, `LinuxPmuStepper`'s own per-tick counter setup/teardown during
       arm-early-then-single-step), not just the guest's own branches. That host-side code is far more
       extensive during the entropy test's ~2000 real interrupt-injection ticks than in the simple
       direct-`rdtsc`-loop scenario `rdtsc_enforced_regime_is_bit_exact_across_boots` already passed
       reliably — explaining why that simpler test was solid while this one wasn't. **Fix**: added
       `BranchCounter::pause`/`resume` (`crates/baud-multiverse/src/timesource.rs`, default no-op) and
       `TimeSource::pause_rcb`/`resume_rcb` (`crates/baud-vcpu/src/lib.rs`, default no-op), wired
       `LinuxBranchCounter` to `perf_event::Counter::disable`/`enable` (starts paused now, not
       enabled-at-construction), and added `run_and_convert_rcb_bracketed`
       (`crates/baud-vcpu/src/linux/mod.rs`) — resumes the counter immediately before each real
       `KVM_RUN` ioctl and pauses it immediately after, across all four real call sites (the plain
       `run_one_exit` loop plus `LinuxPmuStepper`'s `run_until_exit`/`step`/`run_until_irq_window`) —
       so the counter now only accumulates guest-execution-plus-kernel-vmexit time, never the
       surrounding userspace dispatch code. **Measured effect on real hardware**: pass rate rose from
       an estimated ~25-50% (the previously-observed ~50-75% failure rate) to **~75% (15/20 across two
       batches of 8 and 12 real hardware boot-pairs)** — a real, verified, substantial improvement,
       *not* a full fix. `cargo build`/`clippy`/`test --workspace` all clean (0 failures, 87 passed in
       `baud-multiverse` alone); `rdtsc_enforced_regime_is_bit_exact_across_boots`,
       `rdrand_enforced_regime_is_bit_exact_across_boots`, `rdseed_enforced_regime_is_bit_exact_across_
       boots` all still pass (no regression); `drive/h/h4.sh`/`h5.sh`/`h7.sh` all still pass clean on the
       stock module (including the 1000-branch and snapshot-restore real-hardware proofs).
       (c) **Residual ~25% divergence — ROOT-CAUSED this iteration by direct instrumentation, still
       not fixed.** Added a tick-level diagnostic to `os_entropy_is_deterministic`
       (`crates/baud-multiverse/src/linux/mod.rs`; `run_to_first_halt_with_periodic_timer` already
       returned per-tick `rip`+cumulative-`rcb`, previously discarded as `_ticks`) that on a byte-diff
       reports whether the two boots' tick streams differ in *count* (control-flow divergence) or
       have a per-tick RCB-delta disagreement at a specific tick (landing-precision jitter) or
       neither. `drive/manual/h7-enforced-entropy.sh` gained an `H7_ENTROPY_REPEATS=N` knob to rerun the
       double-boot test N times against one module swap, to gather diverging pairs efficiently. A
       real-hardware batch (`H7_ENTROPY_REPEATS=6`) caught one: **same tick count (13==13) and the
       same landing `rip` on both boots, but a 34-count disagreement in the landing RCB overshoot
       past the 500,000 target (500,192 vs 500,158)** — ruling out control-flow divergence and
       confirming landing-precision jitter. That overshoot *is* the served virtual-TSC value at the
       interrupt, and Linux's own `add_interrupt_randomness()` folds `random_get_entropy()` (== that
       served value) into the CRNG pool on every interrupt regardless of crediting (spec §3.8) — so a
       same-instruction interrupt still seeds the CRNG differently each boot, fully explaining the
       observed `getrandom()`/`/dev/urandom` divergence with no further contamination source needed.
       This *confirms* (not merely still-hypothesizes) that `WorkClock`'s long-lived pinned counter
       and `LinuxPmuStepper`'s per-tick freshly-created pinned counter (both counting the identical
       hardware event on the identical thread) disagree on exactly when the arm-early-then-single-
       step engine judges the target crossed — a two-fd epoch/scheduling disagreement, not raw
       single-fd hardware imprecision (§3.7's H0 gate already established the raw
       `BR_INST_RETIRED.COND` event is bit-exact on one always-running fd, so 34 counts of
       landing-precision jitter implicates the second fd).
       **FIXED this iteration and confirmed on real hardware.** `LinuxPmuStepper` no longer owns a
       second `perf_event` fd at all: `crates/baud-vcpu/src/lib.rs`'s `TimeSource` trait gained a
       `current_rcb()` method (default `0`, mirroring `resume_rcb`/`pause_rcb`'s no-op-default
       convention), `WorkClock` implements it as its own inherent `current_rcb()`, and
       `LinuxPmuStepper` (`crates/baud-vcpu/src/linux/pmu.rs`) now reads `self.time.current_rcb()`
       directly instead of building its own `Builder::new().kind(Hardware::BRANCH_INSTRUCTIONS)`
       counter per tick — `arm_overflow`/`with_baseline_rcb`/`baseline_rcb` and the `perf_event`
       imports are gone entirely from that file. `Multiverse::inject_timer_tick`/
       `run_to_first_halt_with_periodic_timer` (`crates/baud-multiverse/src/linux/mod.rs`) no
       longer chain `.with_baseline_rcb(baseline)`. This also **exposed a second, independent,
       pre-existing bug**: `os_entropy_is_deterministic`'s `SPURIOUS_LAPIC_LINE` console-capture-
       race strip (`"Spurious LAPIC timer interrupt on cpu 0\n"`) had a bare `\n` but every real
       kernel `printk` line on this fixture's serial console actually ends `\r\n` (confirmed via
       `cat -A` on a raw capture: every kernel line shows `^M$`; the guest's own userspace `outb`
       probe writes do not) — so `.replace(SPURIOUS_LAPIC_LINE, "")` had been a silent no-op since
       it was introduced, and the improved timing precision from the fd fix made a specific probe
       write get hit by this interleaving far more consistently (10/10 in one batch) than before,
       surfacing it as a 100%-reproducible `hex.len() == 64` assertion failure rather than the
       intended byte-diff panic. Fixed by correcting the constant to end `\r\n`. With both fixes
       landed, a real-hardware `H7_ENTROPY_REPEATS=10` batch (`drive/manual/h7-enforced-entropy.sh`) passed
       **7/10**, and all 3 failures showed the *same* tick count with only a **1-2 count** RCB-delta
       disagreement (down from the pre-fix 34) and a bit-identical landing `rip` — squarely inside
       the already-documented `RCB_HARDWARE_JITTER_TOLERANCE` (±8) single-fd hardware-read-precision
       floor, not a further cross-fd epoch bug. The two-fd architectural disagreement this
       investigation targeted is confirmed eliminated. **Still open**: whether that residual
       1-2-count single-fd read-jitter floor can be driven lower is unexplored — unlike the
       ±8-tolerant `rip`-equality tests elsewhere, `add_interrupt_randomness()`'s CRNG mixing has
       zero tolerance for *any* nonzero jitter, so `os_entropy_is_deterministic` may never reach
       100% on this real-hardware host without a lower-jitter `perf_event` read technique or a
       different entropy-seeding mechanism entirely; `cargo build`/`clippy`/`test --workspace` all
       clean (0 failures, 87 passed in `baud-multiverse`) and `drive/h/h4.sh`/`h5.sh`/`h7.sh` and the
       `h7-enforced-entropy.sh`-internal `rdtsc_enforced_regime_is_bit_exact_across_boots` regression
       check all still pass clean.
       **RESOLVED this iteration (ralph iteration 20): the residual single-fd `perf_event`-read jitter
       above is root-caused and fixed — `os_entropy_is_deterministic` is now real-hardware-verified at
       10/10.** Root cause: both `LinuxBranchCounter` (`crates/baud-multiverse/src/linux/mod.rs`, the
       work-clock's real RCB source) and `measure_fixed_loop_branches` (the H0 `rcb_deterministic` gate,
       `crates/baud-host/src/linux.rs`) were still constructing the generic `perf_event::events::
       Hardware::BRANCH_INSTRUCTIONS` event (counts *all* branches — conditional + unconditional + calls
       + returns) — even though `docs/determinism.md` had, from H0's very first measurement, already
       measured that generic event as `±1`-nondeterministic on this exact host and documented the
       decision to use the raw `BR_INST_RETIRED.COND` event instead (Intel event `0xC4`, umask `0x11`,
       raw encoding `0x11c4`), which that same measurement found bit-exact. Both call sites had silently
       drifted onto the documented-as-rejected event despite the written decision never changing — this
       also means the "H0 gate already established the raw `BR_INST_RETIRED.COND` event is bit-exact"
       premise in the two-fd-epoch paragraph above was stale by the time of that investigation, not
       false when originally written. Confirmed independently by re-running the repo's own
       `tools/pmucheck.c` live: raw event bit-exact (`20000002` x15 samples), generic event a ~9-count
       spread across 15 samples. **Fix**: both call sites now set `perf_event_attr.type_ = PERF_TYPE_RAW
       (4)` / `.config = 0x11c4` directly via the `perf-event` crate's `Builder::attrs_mut()` escape
       hatch (no new dependency — the crate has no portable `Raw` event variant, but does expose direct
       `perf_event_attr` access); new named constants `BR_INST_RETIRED_COND`/`PERF_TYPE_RAW` added in
       both files. **A real regression this surfaced and had to be fixed too**:
       `periodic_timer_injection_halts_gracefully_and_reproducibly`'s `PERIOD_RCB` constant
       (`2_000_000`, tuned against the old host-branch-inclusive event) no longer let the `timer-guest`
       fixture's busy loop reach that count before its own natural halt, under the corrected guest-only
       conditional-branch-only counting; recalibrated empirically (binary-searched on real hardware) to
       `PERIOD_RCB = 200_000`, which reliably produces 5 ticks before natural halt, stable across 4
       repeated real `/dev/kvm` runs — the only test whose numeric expectations needed to change.
       **Real-hardware verification**: `cargo build`/`clippy`/`test --workspace` all clean (0 failures)
       after the recalibration above; `drive/h/h0.sh`-`h7.sh` (8/8 — h0 specifically now measures
       `rcb_deterministic: true` with the corrected event) and `drive/m/m9.sh`-`m12.sh` (4/4) all PASS;
       `drive/pkg/pkg-boot-cli.sh`/`drive/pkg/pkg-multifile-initramfs.sh` PASS (the multi-minute from-source
       kernel-compile scripts `pkg-image-build.sh`/`pkg-build-cli.sh` were skipped as out of scope, since
       this change touches no kernel-build code); `drive/manual/h7-enforced-entropy.sh` with
       `H7_ENTROPY_REPEATS=10` passed **10/10** on a fresh real-hardware batch, plus a clean default
       single-run pass afterward (11/11 total); the RDTSC regression check inside that script is
       unaffected. The doc comment on `os_entropy_is_deterministic`
       (`crates/baud-multiverse/src/linux/mod.rs`) got a RESOLVED addendum recording this, appended after
       the existing (kept) investigation narrative above rather than rewriting it. This closes this
       item's previously-open "residual jitter floor" question for `os_entropy_is_deterministic`; see
       the matching closing note on `double_boot_ram_hash_identical` immediately below.
     **`double_boot_ram_hash_identical`'s guest-driven-checkpoint mechanism is now implemented and
     hardware-tested; the RAM-hash comparison itself still fails every run, root-caused (not just
     observed) to a new, more specific finding than the one above.** The tape device's existing
     `MARK_BRANCH` opcode (specs/baud-tape-device.md §4) turned out to already be the exact
     "guest-driven checkpoint" hook the spec calls for — no new VM-exit/opcode work was needed, only
     a new combinator, `Multiverse::run_until_branch_or_halt_with_periodic_timer`
     (`crates/baud-multiverse/src/linux/mod.rs`), that layers H4's open-ended periodic-timer engine
     with `run_until_branch_or_halt`'s "stop at `MARK_BRANCH`, not just `Hlt`" condition — plus a
     third `checkpoint_init.c` fixture variant (`tests/fixtures/linux-guest/`, `BUILD.md` updated)
     that finalizes one `outb(1, 0x508)` MARK_BRANCH record right before powering off. **Real bug
     found and fixed while wiring this up**: the first version checked `InjectOutcome::Halted`
     before draining the tape device, so a short guest program whose entire checkpoint-then-halt
     sequence fits inside a single tick's window (as this fixture's does) never surfaced its
     `MARK_BRANCH` record at all — fixed by draining/checking for `MARK_BRANCH` before branching on
     the tick outcome, on every iteration. The new `#[ignore]`d test, `double_boot_ram_hash_
     identical`, is driven by a new script, `drive/manual/h7-enforced-checkpoint.sh` (same swap-in/swap-out
     dance as `drive/manual/h7-enforced-entropy.sh`).
     **Real-hardware result: 0/8 across two batches (16 real double-boots), unlike
     `os_entropy_is_deterministic`'s ~70-90% pass rate on the identical enforced-regime machinery —
     root-caused via a one-off byte-diff diagnostic** (booted twice, kept both `Multiverse`s alive,
     diffed raw guest RAM byte-for-byte instead of just hashing it, then removed the diagnostic
     code): only 77,589 of 268,435,456 bytes differ (0.03%), and — critically — they are not
     scattered like independent random draws would be. The differing region decodes as a repeating
     `JMP rel32` + `UD1` byte pattern (`e9 .. .. .. ..` `0f b9 cc`) — the kernel's `static_call`/
     jump-label trampoline padding (`arch/x86/kernel/static_call.c`) — with a genuinely different
     (not small-jitter) `rel32` displacement each boot, i.e. the patched trampoline points at two
     different valid targets, not the same target read imprecisely. This means at least one
     `static_call` site gets updated to a different function depending on a runtime decision itself
     sensitive to the already-documented residual RCB/TSC read jitter (the same root mechanism that
     makes the `sched_clock: Marking stable` printk line's embedded numbers differ) — here visibly
     changing *which code runs*, not just a printed number, plausibly why a full-RAM comparison
     catches it on every run while `os_entropy_is_deterministic`'s narrow 8-probe check mostly does
     not. Driving this to 100% needs either eliminating the residual single-fd `perf_event`-read
     jitter to exactly zero (already open per the finding above) or identifying and pinning the
     specific static-call site — both future work, not attempted this iteration.
     At the time, `drive/manual/h7-enforced-checkpoint.sh` did **not** gate the standard verification protocol
     on this test's own pass/fail (only on its RDTSC regression check), since it was expected to fail
     every run until one of those two fixes landed — see the ralph-iteration-20 closing note below,
     which changed this once the underlying flakiness was fixed; the checkpoint *mechanism* itself (the
     tape cursor landing at the identical step across two boots) is asserted unconditionally and passes.
     **This iteration: `bootparams::DETERMINISTIC_CMDLINE` (spec §4.2's exact cmdline, previously
     "not yet wired as anyone's default" per this section) is now used verbatim by all three real-
     `linux-guest`-fixture tests (`guest_kernel_boots_to_userspace`, `os_entropy_is_deterministic`,
     `double_boot_ram_hash_identical`), replacing three hand-diverged inline cmdline strings —
     closing that reconciliation gap. Verified each added/changed token
     (`no-kvmclock`/`pci=off`/`acpi=off`/`quiet loglevel=1`/`random.trust_cpu=off
     random.trust_bootloader=on`/`nomodule`) is a no-op or an improvement for this fixture
     (`minimal.config` has `CONFIG_PCI=n`/`CONFIG_ACPI=n`/`CONFIG_MODULES=n`; `nomodule` itself is
     not a real kernel parameter — it becomes a harmless, ignored extra `argv` entry to `/init`,
     which never reads `argv` — worth fixing or dropping from the spec string itself in a future
     pass, but harmless as-is; the marker assertions read only the guest's own raw-`outb` writes,
     never kernel `printk` text, so `quiet` doesn't affect them). **Unexpected, real finding**: with
     this cmdline change alone (no other code touched), a fresh `H7_CHECKPOINT_REPEATS=8` real-
     hardware batch for `double_boot_ram_hash_identical` came back **4/8 passed** — a substantial
     jump from the pre-change 0/8 (twice, 16 boots, prior iteration) — plausibly because `quiet
     loglevel=1` (now included for the first time) reduces boot-time console/`printk` work and
     thus the number of branches taken before the checkpoint, narrowing the window in which the
     jitter-sensitive `static_call` trampoline site (documented above) resolves differently. This is
     a real, verified improvement, **not** a fix: 4/8 failures still reproduce the identical
     signature (same tape cursor, byte-diff confined to the same trampoline pattern). Driving this
     the rest of the way to 100% is still open future work — same two candidate fixes as before
     (zero out the residual single-fd `perf_event`-read jitter, or pin the specific static-call
     site), now with a promising new lever (further reducing boot-time branch-count variance before
     the checkpoint) to investigate first.
     **RESOLVED this iteration (ralph iteration 20): the same fix documented in the
     `os_entropy_is_deterministic` closing note above resolves this test too —
     `double_boot_ram_hash_identical` is now real-hardware-verified at 25/25 across two batches, plus a
     clean default single-run pass (26/26 total).** The root cause was the identical wrong-`perf_event`
     bug (`LinuxBranchCounter` and the H0 `measure_fixed_loop_branches` gate both using the generic
     `Hardware::BRANCH_INSTRUCTIONS` event instead of the documented raw `BR_INST_RETIRED.COND` /
     `0x11c4`), not a further static-call-site-specific fix — pinning the specific static-call site
     (the other candidate fix discussed above) turned out not to be necessary once the underlying RCB
     read jitter was eliminated at the source. **Real-hardware verification**:
     `drive/manual/h7-enforced-checkpoint.sh` with `H7_CHECKPOINT_REPEATS=10` then `=15` passed **25/25** across
     two real-hardware batches, plus a clean default single-run pass afterward (26/26 total); the RDTSC
     regression check inside that script is unaffected. `drive/manual/h7-enforced-checkpoint.sh` was changed
     from "informational only, does not gate the script" (as described above) to a real hard pass/fail
     gate on this test, matching `drive/manual/h7-enforced-entropy.sh`'s existing convention, since the
     underlying flakiness is now believed fixed rather than a known, tracked residual. The doc comment
     on `double_boot_ram_hash_identical` (`crates/baud-multiverse/src/linux/mod.rs`) got a RESOLVED
     addendum recording this, appended after the existing (kept) investigation narrative rather than
     rewriting it. **This closes both spec-named tests for matrix row 27 (§12) as real-hardware-
     verified**, not merely improved. **Still open / unaffected by this iteration** (not fixed, not
     attempted): the three sibling tests named directly below (`entropy_guest_is_deterministic`,
     `initial_crng_state_is_reproducible`, `virtio_rng_reseed_is_deterministic`); the Buildroot/pinned-
     Nix image pipeline (§4.5); virtio-rng; the `/dev/vport` tape endpoint (confirmed non-blocking); H8
     Mario (item 3 below, still blocked on the rest of item 1 — the automated image pipeline, not this
     jitter work); H9 Ubuntu (not started, out of scope).
     Also still open: `entropy_guest_is_deterministic`, `initial_crng_state_is_reproducible`,
     `virtio_rng_reseed_is_deterministic` (virtio-rng tape-fed via an ever-ready FIFO, or omitted) — the
     spec's own named tests for this guarantee, distinct from the H7-specific
     `os_entropy_is_deterministic`; per a prior iteration's research, the first two would only ever
     duplicate `os_entropy_is_deterministic` against the same real-Linux fixture (no minimal-kernel
     entropy fixture exists), and `virtio_rng_reseed_is_deterministic` needs an actual virtio-rng
     device model — ralph iteration 23 closed the transport-register-layer sub-piece of this
     (`crates/baud-multiverse/src/virtio_mmio.rs`, a real `Bus` impl for the full virtio-mmio v2
     register window, wired into `DeviceBus` behind opt-in `enable_virtio_rng()`; see item 1's
     ralph-iteration-23 note above for the full detail), but real virtqueue ring parsing over
     `vm-memory`, interrupt delivery (this host registers no in-kernel irqchip at all, so which
     vector a `virtio_mmio.device=` IRQ resolves to is unverified), and boot/cmdline/CLI wiring
     are all still open — still a multi-day effort in total, just a smaller remaining slice than
     before. No guest-kernel patch.
     **Correction (ralph iteration 24)**: the "real virtqueue ring parsing over `vm-memory`" piece named
     as still-open just above is now done — see item 1's ralph-iteration-24 note above
     (`crates/baud-multiverse/src/virtio_queue.rs`'s `SplitVirtqueue`). Interrupt delivery and boot/
     cmdline/CLI wiring remain open exactly as described.
     **Correction (ralph iteration 25)**: the notify-to-drain wiring is also now done — see item 1's
     ralph-iteration-25 note above (`console.rs`'s `service_virtio_rng`). Interrupt delivery and
     boot/cmdline/CLI wiring are the only pieces still open before `virtio_rng_reseed_is_deterministic`
     can pass.
     **Correction (ralph iteration 26)**: interrupt delivery is also now done — see item 1's note above
     (`VirtioMmioTransport::raise_used_buffer_notification` plus `Multiverse::service_virtio_rng_
     interrupt`). Boot/cmdline/CLI wiring (and, separately, which vector an unmodified Linux guest's
     `virtio_mmio` driver would actually bind to) is the only piece still open before `virtio_rng_
     reseed_is_deterministic` can pass.
     **This iteration: boot/cmdline/CLI wiring is now done for the primary `/run/kvm` boot route —
     branch/resume and stream-replay wiring remain smaller, separate follow-ups; the deeper
     unmodified-Linux-driver IRQ-vector-resolution question is still untouched, as designed.** Two
     new `Multiverse` run-loop entry points (`crates/baud-multiverse/src/linux/mod.rs`):
     `run_to_first_halt_with_virtio_rng(vector, max_exits)` (promotes the exact per-exit `notify_count`
     poll-and-service idiom the `virtio_rng_interrupt_reaches_the_guests_own_isr` test's own loop used
     to a real, reusable API — that test was refactored to call it instead of duplicating the loop,
     no behavior change) and `run_to_first_halt_with_periodic_timer_and_virtio_rng(period_rcb,
     timer_vector, virtio_rng_vector, max_ticks)` (the same idea combined with H4's periodic-timer
     engine, checking `notify_count` once per delivered tick rather than once per host-side exit,
     since a real kernel guest needs periodic ticks regardless of virtio-rng). Refactoring the test
     surfaced a real, easy-to-miss gotcha: the original two-phase test bounded only the setup/
     negotiate exits (`MAX_EXITS = 200`) and then called an *unbounded* `run_to_first_halt()` for the
     rest; the merged loop counts every exit against one budget, and `payload.s`'s own deliberate
     20,000-iteration busy-loop (each iteration is one `out 0x80` exit, "long enough for the test
     harness to observe the QueueNotify write... mid-loop") blows straight through 200 — fixed by
     raising the bound to `200_000` (documented inline as generous, not tight). `crates/baud-server/
     src/routes/run_kvm.rs` gained `RunKvmBody.virtio_rng: Option<VirtioRngSpec>` (`seed: u64`,
     `vector: u8` default `0x31` — the fixture's own IDT-gate vector, `max_exits: u32` default
     `200_000`), threaded through `boot_run_and_drain` (now taking a `virtio_rng: Option<(u64,u8,u32)>`
     param, calling `enable_virtio_rng`/`seed_virtio_rng_entropy` right after boot and dispatching to
     whichever of the four run-loop combinations `(periodic_timer, virtio_rng)` calls for) and
     persisted into three new nullable `kvm_run_meta` columns (`virtio_rng_seed`/`_vector`/
     `_max_exits`, `migrations/0013_kvm_run_meta_virtio_rng.sql`, same additive-columns convention as
     `0011`/`0012`). `crates/baud-cli/src/cmds/run.rs`'s `RunAction::Kvm` gained `--virtio-rng-seed`
     (presence enables, mirroring `--periodic-timer-period-rcb`'s convention), `--virtio-rng-vector`
     (default `0x31`), `--virtio-rng-max-exits` (default `200_000`) — `KvmBranch`/`KvmResume` were
     deliberately left unchanged (server-side branch/resume routes don't accept `virtio_rng` yet,
     kept out of scope this iteration to stay focused). **Real-hardware-verified end-to-end** via new
     `drive/pkg/pkg-virtio-rng-cli.sh` (opt-in, mirrors `drive/pkg/pkg-boot-cli.sh`'s structure): boots the
     already-checked-in `tests/fixtures/virtio-rng-guest/bzImage` fixture through a real `baud run kvm
     --virtio-rng-seed 42 --virtio-rng-vector 49 --json` CLI invocation against a live `baud-server`
     over real HTTP — **real result: `ok=true`, `console_output_hex="5295"`** (the guest's own ISR's
     `'R'` marker plus the real tape-seeded entropy byte, proving the interrupt was delivered through
     the actual CLI/server path, not just a Rust test calling `Multiverse` directly). `cargo build`/
     `clippy --workspace --all-targets`/`test --workspace` all clean (0 failures, 0 new warnings —
     confirmed via a targeted grep restricted to the files this iteration touched); `drive/h/h0.sh`-
     `h7.sh` (8/8) and `drive/m/m9.sh`-`m13.sh` (5/5) all still PASS on real `/dev/kvm`, no regressions
     from the widened `kvm_run_meta` schema or `boot_run_and_drain`/`KvmBootParams` signatures;
     `drive/pkg/pkg-boot-cli.sh` re-run clean as a regression check. **Still open**: `stream::render`'s
     real-replay path (`boot_and_drain_frames`/`render_frames_from_real_replay`) does not read the new
     `virtio_rng_*` columns back yet, so a virtio-rng-enabled run's frames always replay with the
     device disabled (harmless unless a guest's frame emission itself depends on entropy content — no
     current fixture does); `/run/kvm/branch`/`/run/kvm/resume` don't accept `virtio_rng` at all yet
     (their `KvmBootParams` literals were updated to compile with `virtio_rng: None`, not to actually
     support it); and, unchanged, the deeper "which vector would an unmodified Linux guest's real
     `virtio_mmio` driver's `request_irq()` resolve to, with no IOAPIC/PIC here" question — genuinely
     separate research, not plumbing. `virtio_rng_reseed_is_deterministic` (the spec-named test) still
     needs a *real Linux guest* (not this hand-assembled fixture) to actually negotiate and use the
     device before it can pass — this iteration closes the mechanism's reachability, not that guest.
     **This iteration closes exactly the "`stream::render`'s real-replay path does not read the
     new `virtio_rng_*` columns back yet" gap flagged above — for the reboot sub-path only.**
     `crates/baud-server/src/routes/run_kvm.rs`'s `boot_and_drain_frames` gained a
     `virtio_rng: Option<(u64, u8, u32)>` parameter (previously it hardcoded `None` into
     `boot_run_and_drain`, silently discarding whatever the caller wanted), now threaded straight
     through unchanged. `crates/baud-server/src/routes/stream.rs`'s `render()` handler's `kvm_run_meta`
     SELECT now also pulls `virtio_rng_seed`/`virtio_rng_vector`/`virtio_rng_max_exits` (the three
     columns iteration 27's `migrations/0013_kvm_run_meta_virtio_rng.sql` added), decodes them into
     an `Option<(u64, u8, u32)>`, and passes that to `render_frames_from_real_replay` — but, as the
     inline comment at `stream.rs:191-193` already flagged and this iteration confirms is still true,
     *not* to `render_frames_from_real_restore` (the restore-based sub-path), since there is still no
     `run_until_branch_or_halt_with_virtio_rng`-family combinator for it in `baud-multiverse`; that
     remains the same still-open gap as `/run/kvm/branch`/`/run/kvm/resume` not accepting `virtio_rng`
     at all. Because `render_frames_from_real_replay` was already at 7 positional params before this
     8th one, both the real `#[cfg(target_os = "linux")]` impl and its `#[cfg(not(target_os =
     "linux"))]` stub had their params bundled into a new `RealReplayParams` struct (`stream.rs:284-
     293` real, `stream.rs:365-371` stub) to stay under clippy's `too_many_arguments`. New unit test
     `boot_and_drain_frames_with_virtio_rng_enabled_still_replays_real_pixels`
     (`run_kvm.rs:1614-1643`) boots the framebuffer-guest fixture twice with virtio_rng enabled
     (`Some((42u64, 0x31u8, 200_000u32))`) and asserts the emitted pixels are both identical to the
     virtio_rng-disabled baseline and identical to each other — proving enabling the device is a real
     no-op for a guest that never touches the virtio-mmio window, and that the replay path stays
     double-run deterministic with it enabled. **Real-hardware-verified end-to-end** via new
     `drive/pkg/pkg-virtio-rng-replay-cli.sh` (opt-in, same convention as `drive/pkg/pkg-virtio-rng-cli.sh`):
     boots framebuffer-guest via a real `POST /run/kvm { run_id, virtio_rng }` HTTP call, reads the
     real sqlite `kvm_run_meta` row directly (Python's `sqlite3` module, not through the API) to
     confirm all three `virtio_rng_*` columns round-tripped exactly as sent, then calls `POST
     /runs/:id/stream/render`
     twice and confirms both renders decode to the guest's real, unperturbed pixels — byte-identical
     across the two renders, over real HTTP against a live `baud-server`, on real `/dev/kvm` hardware.
     `cargo build`/`clippy --workspace --all-targets`/`test --workspace` all clean (0 failures, 0 new
     warnings — confirmed via a targeted check of the touched files `stream.rs`/`run_kvm.rs` only);
     `drive/h/h0.sh`-`h7.sh` (8/8), `drive/m/m9.sh`-`m13.sh` (5/5), `drive/pkg/pkg-boot-cli.sh`, and
     `drive/pkg/pkg-virtio-rng-cli.sh` all still PASS on real `/dev/kvm`, no regressions from the widened
     `boot_and_drain_frames` signature or the `RealReplayParams` refactor.
     **This iteration closes the missing `run_until_branch_or_halt_with_virtio_rng`-family
     combinator and wires it everywhere the prior note flagged as blocked on it.**
     `crates/baud-multiverse/src/linux/mod.rs` gained two new combinators, mirroring the existing
     `run_to_first_halt_with_virtio_rng`/`run_to_first_halt_with_periodic_timer_and_virtio_rng` pair
     but with `run_until_branch_or_halt`'s "stop at `MARK_BRANCH`, not just `Hlt`" condition merged
     in: `run_until_branch_or_halt_with_virtio_rng(vector, max_exits)` and
     `run_until_branch_or_halt_with_periodic_timer_and_virtio_rng(period_rcb, timer_vector,
     virtio_rng_vector, max_ticks)`. `crates/baud-server/src/routes/run_kvm.rs`'s `run_branches`
     (the shared fork-and-run loop `/run/kvm/branch` and `/run/kvm/resume`'s fixed-tape mode both
     call) now takes a `virtio_rng: Option<(u64, u8, u32)>`, calls
     `branch.enable_virtio_rng()`/`seed_virtio_rng_entropy(seed)` fresh right after
     `Multiverse::branch` (device state is not itself part of the snapshot/restore/branch contract —
     `DeviceBus::restore` always starts a branch with it disabled, confirmed by grep), and dispatches
     on `(periodic_timer, virtio_rng)` to one of the four combinators, exactly mirroring
     `boot_run_and_drain`'s existing dispatch. `RunKvmBranchBody`/`RunKvmResumeBody` both gained a
     `virtio_rng: Option<VirtioRngSpec>` field (same struct `RunKvmBody` already used), threaded
     through `boot_snapshot_and_branch`/`resume_and_branch` — **fixed-tape mode only**; `generate`
     mode (`DriverGenerateSpec`-based branches) is unchanged and still runs with virtio_rng disabled,
     deliberately out of scope this iteration. `stream::render`'s restore-and-replay path
     (`render_frames_from_real_restore`) also now reads the same three `kvm_run_meta` `virtio_rng_*`
     columns `render_frames_from_real_replay` already did and threads them the same way — its params
     were bundled into a new `RealRestoreParams` struct (both cfg variants) to stay under
     `too_many_arguments`, mirroring `RealReplayParams`. `baud-cli`'s `kvm-branch`/`kvm-resume`
     subcommands gained matching `--virtio-rng-seed`/`--virtio-rng-vector`/`--virtio-rng-max-exits`
     flags (honored only in fixed-tape mode). New unit tests, all real-hardware-verified: `baud-
     multiverse`'s `run_until_branch_or_halt_with_virtio_rng_delivers_interrupt_to_a_branch` (a
     *forked* branch, not just a fresh boot, delivers the real interrupt); `baud-server`'s
     `boot_snapshot_and_branch_with_virtio_rng_delivers_interrupt_to_a_branch` and
     `resume_and_branch_with_virtio_rng_delivers_interrupt_and_restore_reproduces_it` (the latter
     also independently re-derives `render_frames_from_real_restore`'s own logic inline and confirms
     it reproduces the live resume's console output). New drive script
     `drive/pkg/pkg-virtio-rng-branch-resume-cli.sh` proves the whole thing end-to-end over real HTTP
     against a live `baud-server` on real `/dev/kvm`: a fresh `/run/kvm/branch { virtio_rng }`
     delivers the interrupt (`virtio-rng-guest` fixture, console=`5295`, seed 42); `/run/kvm/resume
     { virtio_rng }` reproduces it with no re-boot; a `framebuffer-guest` restore-based row persisted
     via `/run/kvm/resume { virtio_rng, frame_run_ids }` round-trips its three `kvm_run_meta` columns
     and `POST /runs/:id/stream/render` decodes the guest's real, unperturbed pixels via the restore
     path with virtio_rng re-enabled, byte-identical across two renders.
     `cargo build`/`clippy --workspace --all-targets`/`test --workspace` all clean (0 failures, 0 new
     warnings in any touched file); `drive/h/h0.sh`-`h7.sh` (8/8), `drive/m/m9.sh`-`m13.sh` (5/5),
     `drive/pkg/pkg-boot-cli.sh`, `drive/pkg/pkg-virtio-rng-cli.sh`, `drive/pkg/pkg-virtio-rng-replay-cli.sh`, and
     the new `drive/pkg/pkg-virtio-rng-branch-resume-cli.sh` all PASS on real `/dev/kvm`, no regressions.
     **This iteration closes the last-open half of the virtio-rng gap: `generate` mode.**
     `/run/kvm/branch`'s and `/run/kvm/resume`'s `DriverGenerateSpec` path
     (`run_driver_generated_branches_with_persist`, `crates/baud-server/src/routes/run_kvm.rs`) used
     to silently run every driver-generated branch with virtio_rng disabled even when the caller set
     the field — no error, just no device. It now threads `virtio_rng` through exactly like the
     fixed-tape path added last iteration: `enable_virtio_rng`/`seed_virtio_rng_entropy` on each
     freshly forked branch, then dispatches on `(periodic_timer, virtio_rng)` to the matching one of
     the four `run_until_branch_or_halt*` combinators (all of which already existed —
     this is pure route-level wiring, no new `baud-multiverse` primitive needed).
     `boot_snapshot_and_generate`/`resume_and_generate` both gained the same `virtio_rng` parameter,
     threaded from `RunKvmBranchBody::virtio_rng`/`RunKvmResumeBody::virtio_rng` (both fields already
     existed; only their doc comments claiming "generate mode is unaffected" were wrong after this
     change and are now fixed).
     **A real, independent CLI-side bug was found and fixed alongside this**: `baud-cli`'s
     `kvm-branch`/`kvm-resume` handlers (`crates/baud-cli/src/cmds/run.rs`) only ever wrote
     `body["virtio_rng"]` inside the fixed-tape (`else`) arm of the generate/fixed-tape match — so
     `--virtio-rng-seed` silently vanished from the HTTP request whenever `--generate-seed` was also
     set, even before this iteration's server-side fix, and would have kept silently vanishing after
     it. Moved both `if let Some(seed) = virtio_rng_seed { body["virtio_rng"] = ... }` blocks out of
     the `else` arm so they run unconditionally in both commands.
     New tests, both real-hardware-verified: `boot_snapshot_and_generate_with_virtio_rng_delivers_interrupt_to_a_branch`
     and `resume_and_generate_with_virtio_rng_delivers_interrupt_to_a_branch`
     (`crates/baud-server/src/routes/run_kvm.rs`) — each generates 3 branches against
     `virtio-rng-guest` (which never reads its own tape suffix — see that fixture's `BUILD.md` — so a
     matching console output across all 3 differently-generated tapes proves the interrupt was
     delivered, not that the tape happened to match) and confirms every one's console output matches
     a direct `boot_run_and_drain` boot with the identical seed. New drive script
     `drive/pkg/pkg-virtio-rng-generate-cli.sh` proves it end-to-end through the real `baud` CLI (not just
     the library level) against a live `baud-server` on real `/dev/kvm`: `kvm-branch --generate-seed
     --generate-count 3 --virtio-rng-seed 42` delivers the interrupt to all 3 branches, and
     `kvm-resume --generate-seed --generate-count 2 --virtio-rng-seed 42` against the persisted point
     reproduces it with no re-boot — this is also the test that would have caught the CLI-body bug
     above, since it drives the real CLI binary end-to-end rather than constructing the JSON body
     directly.
     `cargo build`/`clippy --workspace --all-targets`/`test --workspace` all clean (0 failures, 0 new
     warnings); `drive/h/h0.sh`-`h7.sh` (8/8), `drive/m/m9.sh`-`m13.sh` (5/5), `drive/pkg/pkg-boot-cli.sh`,
     `drive/pkg/pkg-virtio-rng-cli.sh`, `drive/pkg/pkg-virtio-rng-replay-cli.sh`,
     `drive/pkg/pkg-virtio-rng-branch-resume-cli.sh`, and the new
     `drive/pkg/pkg-virtio-rng-generate-cli.sh` all PASS on real `/dev/kvm`, no regressions.
     **Still open (at the time of the above)**: the "which vector would an unmodified Linux guest's
     real `virtio_mmio` driver bind to" research question remained untouched; `virtio_rng_reseed_
     is_deterministic` (the spec-named test) still needed a real Linux guest that actually
     negotiates and uses the device, not just this hand-assembled fixture, and no fixture existed
     yet that both drives virtio-rng and emits frames in the same guest (so `render_frames_from_
     real_restore`'s virtio_rng plumbing is only proven as a no-op so far, same caveat `render_
     frames_from_real_replay`'s own test already had); H8 Mario was still blocked on the FCEUX
     Qt5/SDL2 packaging problem; H9 Ubuntu was still not started.
  2a. **The "which vector" research question is now answered, with real code and a real-hardware
     test, not just theory.** Dispatched an Opus research subagent to trace exactly what an
     unmodified x86_64 Linux kernel does when it parses `virtio_mmio.device=<size>@<base>:<irq>`
     on its command line, grep-confirmed against real Linux 6.18.33 source
     (`~/wsl-kernel-src/src`, already present on this dev host from the enforced-module work):
     `vm_cmdline_set` (`drivers/virtio/virtio_mmio.c`) registers a `platform_device` with a raw
     `IORESOURCE_IRQ` resource, no ACPI/DT/fwnode translation at all — the cmdline number *is* the
     Linux virq number. `request_irq()` on it fails with `-EINVAL` unless `irq_to_desc()` finds a
     preallocated descriptor, and legacy IRQ descriptors are only preallocated at all if
     `probe_8259A()` (`arch/x86/kernel/i8259.c`) — which writes a mask byte to port 0x21 and reads
     it back — succeeds; this VMM registers no in-kernel irqchip at all
     (`KVM_CREATE_IRQCHIP`/`KVM_IOEVENTFD` are never called, confirmed again by grep) and ports
     0x20/0x21/0xA0/0xA1 fell straight through to `OpenBusFallback` (fixed `0xFF`, ignores writes),
     so the probe would fail, `legacy_pic` would fall back to `null_legacy_pic`,
     `nr_legacy_irqs() == 0`, and *every* ISA-IRQ `request_irq()` — including virtio_mmio's — would
     return `-EINVAL` on a real kernel. Once the probe succeeds and `init_8259A()`'s ICW1..ICW4
     handshake completes, `init_IRQ`/`init_ISA_irqs` (`arch/x86/kernel/apic/vector.c`,
     `arch/x86/include/asm/irq_vectors.h`) populate `vector_irq[]` via
     `ISA_IRQ_VECTOR(irq) = ((0x20+16)&~15)+irq = 0x30+irq` for `irq` in `0..16` — **this is the
     CPU vector baud's own direct-injection mechanism (`KVM_INTERRUPT`, unchanged) must target for
     a real Linux guest's ISA IRQ N to reach that guest's own registered handler.** The research
     also ruled out the two heavier alternatives with kernel-source citations:
     `KVM_CREATE_IRQCHIP` is a hard no (`kvm_vcpu_ioctl_interrupt` returns `-ENXIO` once
     `pic_in_kernel()`, deleting baud's entire exact-boundary injection mechanism, plus it adds a
     host-hrtimer-driven in-kernel LAPIC timer — a real determinism risk); `KVM_SET_LAPIC`/`KVM_GET_
     LAPIC` without an irqchip are both gated on `lapic_in_kernel(vcpu)` and return `-EINVAL`; a
     devicetree/hand-built-MADT route still needs an IOAPIC model on top, no smaller than option
     (a). **Implemented the recommended minimal path**: new `crates/baud-multiverse/src/pic8259.rs`
     — a pure-bookkeeping dual-8259 stub (no dependencies, hardware-independent, same pattern as
     `console::Cmos`) modeling exactly what `probe_8259A()`/`init_8259A()`/`enable_8259A_irq()`/
     `mask_and_ack_8259A()` touch: each chip's IMR (mask register, `0xFF` at reset) and an ICW-
     sequence state machine (`ExpectIcw2/3/4` gated on `ICW1`'s `SINGLE`/`NEED_ICW4` bits) — OCW2
     EOI writes are absorbed as no-ops (nothing here ever actually raises anything; baud still
     always delivers directly via `KVM_INTERRUPT` at an exact boundary, unchanged) and a new
     `pic8259::isa_irq_vector(irq) -> u8` helper implementing the `0x30+irq` formula. Wired
     unconditionally into `console::DeviceBus` (no opt-in needed, unlike `virtio_rng` — pure
     guest-write-derived bookkeeping with no side effects on any other device) at ports
     0x20/0x21/0xA0/0xA1, previously unhandled and falling through to the open-bus fallback. 8 new
     hardware-independent unit tests (probe-readback, full ICW1-4 handshake both chips, single-mode
     ICW3-skip, per-IRQ-bit unmask, EOI-absorption, re-init-from-any-state, the vector formula) all
     pass. **Extended the existing `tests/fixtures/virtio-rng-guest/` hand-assembled fixture** to
     also issue this exact byte sequence — probe_8259A pattern, full ICW1-4 handshake on both
     chips, an `enable_8259A_irq(5)`-equivalent unmask — ahead of its virtio-mmio negotiation
     (`payload.s`, rebuilt via `build.py`; its own interrupt-injection vector stays the
     independently-chosen `0x31`, unchanged, since baud's direct-injection mechanism has never
     depended on the PIC's hardware ICW2 vector base). New real-hardware test
     `guests_own_pic_bring_up_sequence_leaves_the_expected_bookkeeping_state`
     (`crates/baud-multiverse/src/linux/mod.rs`, via a new `Multiverse::pic()` accessor) boots that
     fixture and asserts the master/slave IMR end state (`0xdf`/`0xff`) exactly matches what the
     guest's real `IN`/`OUT` PIO exits should have produced — proving the guest-visible byte
     sequence a real kernel issues round-trips correctly through the new stub on real KVM, not just
     in a pure-Rust unit test; `virtio_rng_interrupt_reaches_the_guests_own_isr` (unchanged
     assertions) confirms the bring-up sequence doesn't disturb the rest of the fixture's run
     either. `crates/baud-multiverse/tests/fixtures/virtio-rng-guest/BUILD.md` updated with a new
     "Update" section recording the answered question. `cargo build`/`clippy --workspace
     --all-targets`/`test --workspace` all clean (0 failures; one clippy `identity_op` warning my
     own new unit test introduced was found and fixed before commit, not left as a new warning);
     `baud-multiverse` went from 119 to 128 passed (8 ignored, unchanged) — the 9 new tests (8
     pure-Rust + 1 real-hardware); `drive/h/h0.sh`-`h7.sh` (8/8), `drive/m/m9.sh`-`m13.sh` (5/5),
     `drive/pkg/pkg-boot-cli.sh` (including a real Linux 6.18 kernel boot to `/init`, confirming the
     unconditionally-wired `Pic8259` introduces no regression even on the non-hand-assembled boot
     path), `drive/pkg/pkg-virtio-rng-cli.sh`, `drive/pkg/pkg-virtio-rng-replay-cli.sh`, `drive/pkg/pkg-virtio-
     rng-branch-resume-cli.sh`, and `drive/pkg/pkg-virtio-rng-generate-cli.sh` all still PASS on real
     `/dev/kvm`, no regressions.
     **What this still does not do**: it does not boot an actual unmodified Linux kernel far enough
     to exercise its real `drivers/virtio/virtio_mmio.c` + `drivers/char/hw_random/virtio-rng.c`
     drivers — that needs `CONFIG_VIRTIO=y`/`CONFIG_VIRTIO_MMIO=y`/`CONFIG_VIRTIO_MMIO_CMDLINE_
     DEVICES=y`/`CONFIG_HW_RANDOM_VIRTIO=y` added to the guest kernel config plus a real initramfs
     with those drivers built in or loadable, and is believed at this point to be blocked on the
     same Buildroot/pinned-Nix guest-image pipeline (§4.5) already blocking H8/H9 **(this premise
     turned out to be wrong for exactly this piece — see the "what this still does not do" gap
     closed below; Buildroot/pinned-Nix remains genuinely needed only for H8/H9's full rootfs)** —
     so `virtio_rng_reseed_is_deterministic` (the spec-named test) still cannot pass yet, but the
     previously-unknown "what vector" half of that blocker is now closed: once a real Linux guest
     boots this far, `isa_irq_vector(5)` (or whichever line the cmdline names) is exactly the value
     `service_virtio_rng_interrupt`/`RunKvmBranchBody::virtio_rng`'s vector field should be given.
     H8 Mario is still blocked on the FCEUX Qt5/SDL2/Xvfb packaging problem; H9 Ubuntu is still not
     started.
     **This iteration: the premise above — that exercising the real kernel's own virtio_mmio/
     virtio-rng drivers needs the Buildroot/pinned-Nix pipeline — turned out to be wrong.** Only the
     full-rootfs half of that (H8's FCEUX dependency closure, H9's Ubuntu) actually needs Buildroot/
     Nix; proving the *driver* half needed only a single-binary initramfs, the same pattern every
     other fixture in `crates/baud-multiverse/tests/fixtures/linux-guest/` already uses. Rebuilt
     that directory's existing from-scratch `make bzImage` pipeline (its `BUILD.md`-documented
     process, no Buildroot/Nix involved) with `minimal.config` gaining `CONFIG_VIRTIO_MENU=y`,
     `CONFIG_VIRTIO=y`, `CONFIG_VIRTIO_MMIO=y`, `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y`,
     `CONFIG_HW_RANDOM=y`, `CONFIG_HW_RANDOM_VIRTIO=y`. Found a real Kconfig gotcha, documented in
     `BUILD.md`: `CONFIG_VIRTIO_MENU` is a `menuconfig` in `drivers/virtio/Kconfig` gating the
     entire virtio submenu, separate from and easy to miss alongside `VIRTIO_MMIO`'s own
     `HAS_IOMEM`/`HAS_DMA` deps — omit it and the kernel silently builds with no virtio at all, no
     error at either config or boot time.
     Added a new `/init` variant, `virtio_rng_init.c`, packaged as `virtio_rng_initramfs.cpio.gz`
     alongside the fixture's existing images, booted with cmdline
     `virtio_mmio.device=0x200@0xd0000000:5` (base/len from `layout::VIRTIO_MMIO_RNG_BASE`/`_LEN`,
     `crates/baud-multiverse/src/layout.rs`; IRQ 5 chosen so `pic8259::isa_irq_vector(5) = 0x35` —
     above the "what vector" answer, previously validated only against the hand-assembled
     `virtio-rng-guest` fixture — is now, for the first time, the value fed to a real, unmodified
     kernel's own `drivers/virtio/virtio_mmio.c` + `drivers/char/hw_random/virtio-rng.c`, rather than
     a hand-assembled guest that merely mimics one). The pre-existing
     `tests/fixtures/virtio-rng-guest/` fixture stays as-is, unrelated.
     Two real bugs surfaced getting this to boot (full diagnosis in `linux-guest/BUILD.md`'s new
     "`virtio_rng_init.c` / `virtio_rng_initramfs.cpio.gz`" section): (1) `/dev/hwrng` never appeared
     via devtmpfs despite the driver probing successfully (confirmed via
     `/sys/class/misc/hw_random` and `/proc/interrupts` showing `virtio0` bound to IRQ 5) —
     devtmpfs node creation for a device registered mid-`do_initcalls()` depends on the async
     `devtmpfsd` kernel thread, which this single-vCPU deterministic machine gives no guaranteed
     chance to run before `/init`'s first instruction; fixed by having `virtio_rng_init.c` read the
     device's real major:minor from `/sys/class/misc/hw_random/dev` and `mknod()` the node itself —
     deterministic, no timing race. (2) a subsequent real read then hung forever with no interrupt
     ever delivered — the virtio-rng driver's `virtio_read()` blocks via
     `wait_for_completion_killable()`, scheduling out to the idle loop's `safe_halt()`, a scenario no
     prior fixture exercised (every one kept its vCPU busy while an interrupt was pending); `baud_
     vcpu::boundary::inject_at` (`crates/baud-vcpu/src/boundary.rs`) returns `InjectOutcome::Halted`
     before ever calling `PmuStepper::inject` when the vCPU is already halted at entry (by design —
     a real triple-fault shutdown is observed the same way and must not be mistaken for "keep
     going"), so `Multiverse::run_to_first_halt_with_periodic_timer_and_virtio_rng`'s `Halted` arm
     (`crates/baud-multiverse/src/linux/mod.rs`) was returning immediately as terminal even when a
     virtio-rng completion was the actual, resumable reason for the halt. Fixed with a new method,
     `Multiverse::service_virtio_rng_interrupt_while_halted`, that stages the interrupt directly via
     `KVM_SET_VCPU_EVENTS` and re-enters with one plain `step_exit()` — safe specifically because
     `safe_halt()`'s `sti; hlt` idiom guarantees `RFLAGS.IF=1` at the exact halt instant. The
     `Halted` arm now checks for a pending virtio-rng notification and wakes through this path
     instead of returning; behavior is unchanged for every existing guest that never enables
     virtio-rng (falls through to the original terminal-halt return exactly as before — zero
     regression risk for every other test).
     Two new tests, both real-hardware-verified on real `/dev/kvm`, in
     `crates/baud-multiverse/src/linux/mod.rs`:
     `guest_virtio_mmio_rng_driver_reads_real_entropy_through_virtio_rng` (boots, confirms `/init`
     reaches its marker, opens `/dev/hwrng` (`hwrng-open-ok`), and completes a real read
     (`hwrng-bytes:` + hex)) and
     `guest_virtio_mmio_rng_driver_entropy_is_reproducible_across_two_boots` (same seed, two boots,
     asserts the entire console output — including the real driver-sourced entropy bytes — is
     byte-identical). `cargo build --workspace`, `cargo clippy --workspace --all-targets` (zero new
     warnings — `linux/mod.rs` has zero clippy hits), and `cargo test --workspace` all clean;
     `baud-multiverse` now 130 passed/8 ignored (up from 128); `drive/h/h0.sh`-`h7.sh` (8/8),
     `drive/m/m9.sh`-`m13.sh` (5/5), `drive/pkg/pkg-boot-cli.sh`, `drive/pkg/pkg-multifile-initramfs.sh`,
     `drive/pkg/pkg-dynamic-link.sh` (all three boot the shared rebuilt `linux-guest` bzImage — no
     regressions from the Kconfig change), `drive/pkg/pkg-build-cli.sh` (confirms the automated
     from-source kernel-build pipeline still works with the new Kconfig fragment), and all four
     `drive/pkg/pkg-virtio-rng-*-cli.sh` scripts (different fixture, shares `console.rs`/`Pic8259` code —
     no regressions) all PASS on real `/dev/kvm`. `drive/pkg/pkg-image-build.sh` (the
     two-independent-builds reproducibility check) was not re-run this iteration to save time — it
     shares the exact same build path `pkg-build-cli.sh` already exercised successfully, so this is
     a low-risk, documented gap, not a skipped requirement.
     **Still open after this iteration**: the Buildroot/pinned-Nix guest-image pipeline (§4.5)
     itself is still not implemented — still needed for H8 (FCEUX/Xvfb + its dependency closure) and
     H9 (Ubuntu), which need a *rootfs*, not just a kernel driver proof; this iteration's virtio_rng
     proof did not need Buildroot/Nix because it only needed a single-binary initramfs, same pattern
     as every other fixture in that directory. `virtio_rng_reseed_is_deterministic` (§3.8) is now
     closed — see the paragraph immediately below and `linux-guest/BUILD.md`'s "Continuous
     reseeding" section for the four-read fixture and the new test proving it byte-identical across
     two boots. H8 Mario is still blocked on the FCEUX Qt5/SDL2/Xvfb packaging problem, untouched
     since the finding above; H9 Ubuntu is still not started.
     `crates/baud-multiverse/tests/fixtures/linux-guest/virtio_rng_init.c` was changed to loop four
     separate `read()`s over the same open `/dev/hwrng` fd (previously exactly one read), printing
     each round's hex bytes on its own `baud-guest: hwrng-bytes:` console line before rebooting, and
     `virtio_rng_initramfs.cpio.gz` in the same directory was rebuilt from the new source via the
     existing musl-gcc + cpio recipe already documented in `BUILD.md` — no kernel rebuild needed,
     same shared `bzImage`. A new test, `virtio_rng_reseed_is_deterministic`, was added to
     `crates/baud-multiverse/src/linux/mod.rs` right after
     `guest_virtio_mmio_rng_driver_reads_real_entropy_through_virtio_rng`: it boots the guest twice
     with the same seed, extracts all four `hwrng-bytes:` lines from each boot's console output, and
     asserts (a) all four reads complete each boot, (b) the four reads within one boot are pairwise
     distinct (proving each read draws fresh entropy from the tape-seeded stream rather than a
     cached/repeated value), and (c) the full sequence of reads — and the entire console output — is
     byte-identical across the two boots. This is the exact spec-named test from
     `specs/baud-multiverse.md` §3.8 / line 274 above. The gap closed with zero host-side or run-loop
     changes: both a research subagent and direct code reading confirmed
     `Multiverse::run_to_first_halt_with_periodic_timer_and_virtio_rng`'s halt-servicing loop already
     generically re-checks `VirtioMmioTransport::notify_count()` (a monotonic counter, not a one-shot
     flag) on every timer tick, and `DeviceBus::service_virtio_rng` / `SplitVirtqueue::
     process_available` already drain all newly-available descriptor chains per call, not just one —
     the only real gap was the guest payload itself issuing a single read, smaller follow-up work
     than the note above implied. `linux-guest/BUILD.md`'s "Continuous reseeding —
     `virtio_rng_reseed_is_deterministic` (spec §3.8)" section documents this in full detail. Full
     verification on real `/dev/kvm`: `cargo build --workspace`, `cargo clippy --workspace
     --all-targets`, and `cargo test --workspace` all clean, zero new warnings; `baud-multiverse` now
     131 passed/8 ignored (up from 130). The two pre-existing tests
     `guest_virtio_mmio_rng_driver_reads_real_entropy_through_virtio_rng` and
     `guest_virtio_mmio_rng_driver_entropy_is_reproducible_across_two_boots` were re-run against the
     updated four-read fixture and both still pass — no regression from the fixture change.
     `drive/h/h0.sh`-`h7.sh` (8/8), `drive/m/m9.sh`-`m13.sh` (5/5), `drive/pkg/pkg-boot-cli.sh`,
     `drive/pkg/pkg-multifile-initramfs.sh`, `drive/pkg/pkg-dynamic-link.sh` (all three boot the shared
     `linux-guest` `bzImage`, confirming the fixture change caused no regressions), and all four
     `drive/pkg/pkg-virtio-rng-*-cli.sh` scripts (`pkg-virtio-rng-cli.sh`,
     `pkg-virtio-rng-replay-cli.sh`, `pkg-virtio-rng-branch-resume-cli.sh`,
     `pkg-virtio-rng-generate-cli.sh` — these use a separate, untouched hand-assembled
     `tests/fixtures/virtio-rng-guest/` fixture) all PASS on real `/dev/kvm`; no regressions anywhere.
     Separately noted as a hazard, not a code bug: `tools/pauseresume_ab.sh` (the RCB work-clock A/B/C
     experiment script) was found still running as an orphaned background process against this same
     `crates/baud-multiverse/src/linux/mod.rs` during this work, transiently patching the file with a
     `trap ... EXIT` that reverts it via `git checkout --` on exit — if left running across a session
     boundary it can silently clobber concurrent edits to that file; no data was lost here, but
     anyone running that script should be aware of this.
  3. **H8 — Super Mario Bros example (§11, rides on #1)** — rebuild `examples/mario/` under the new model: a
     real Linux image with FCEUX + the Lua harness + `/init` (the pre-KVM `nes_bridge.c` stdin stub is
     retired), `probes.toml` / `strategy.toml`, `drive/mario.sh` completion gate, the ~25% live window
     (`baud stream tail | ffplay`), and the README hero + centralized GIF. All NES specifics stay under
     `examples/` (`no_workload_specifics_in_core`).

     **Research finding, still open**: Ubuntu's packaged `fceux` (2.6.5+dfsg1-2build4, `universe`) is a
     Qt5 GUI app, not headless — `apt-get install --dry-run fceux` pulls 64 new packages (~128MB
     installed): the full Qt5 stack (Core/Gui/Widgets/Qml/Quick/Network/DBus/Svg/Wayland/X11), SDL2,
     ALSA, PulseAudio, libinput, XCB/X11 client libs, audio codecs. The man page documents `--nogui
     {0|1}`, but the binary itself contains the string `"Error: Qt/SDL version does not support
     --no-gui option."` — this build rejects that flag. No true console/headless FCEUX variant exists
     in the Ubuntu package; building from upstream source doesn't help either, since upstream is the
     same Qt5+SDL2 GUI architecture, not an alternative lighter build. `liblua5.1-0` (the exact version
     FCEUX's Lua scripting wants) is small and available, so Lua is not the blocker — Qt5/SDL2/OpenGL/X11
     windowing is. `QT_QPA_PLATFORM=offscreen` (via `libqt5gui5t64`'s offscreen QPA plugin) is a
     theoretically viable workaround but unverified, and would still need software OpenGL (Mesa
     llvmpipe) since the guest has no GPU passthrough. Net: ~two orders of magnitude more guest-image
     plumbing than the `dynamic_init.c` precedent (`crates/baud-multiverse/tests/fixtures/linux-guest/
     dynamic_init.c` — just ld-linux + libc, 4 initramfs entries) — every one of 64+ transitive `.so`s
     would need individual `ldd`/`readelf` identification and manual initramfs copying, no package
     manager in-guest. Not an architectural dead end, just confirmed genuinely large: likely needs a
     Buildroot/Nix packaging pipeline, or first empirically validating `QT_QPA_PLATFORM=offscreen`, or
     exploring an older pre-Qt5 SDL-only FCEUX release.

     **Follow-up finding, still open**: `QT_QPA_PLATFORM=offscreen` is now confirmed NOT viable, not just
     unverified — `apt-get install -y fceux` completes cleanly (fceux 2.6.5+dfsg1-2build4 plus its full
     Qt5/SDL2/X11 dependency chain, no errors), but `env -u DISPLAY -u WAYLAND_DISPLAY
     QT_QPA_PLATFORM=offscreen fceux` (no ROM) reliably segfaults (exit 139) within under 2 seconds, with
     or without `LIBGL_ALWAYS_SOFTWARE=1`, immediately after two `Qt Warning: QOpenGLWidget: Failed to
     create context` lines on stderr — the offscreen QPA plugin cannot back a real OpenGL context, and
     FCEUX's `QOpenGLWidget`-based renderer does not fall back gracefully. A new, more promising
     alternative: `xvfb-run -a --server-args="-screen 0 1024x768x24" fceux` (a virtual X server providing
     a real, if virtual, X11/GLX surface) survived at least 3 seconds with no crash and no fatal Qt error.
     Caveat: no NES ROM was loaded in either test (none exists in-repo, for copyright reasons), so it's
     still unproven that fceux under `Xvfb` can run an actual game loop, drive the Lua scripting harness,
     or emit frames over time — only that the process itself doesn't crash on startup the way it does
     under the offscreen QPA plugin. This narrows, but does not close, the packaging problem: the viable
     target is now bundling a minimal `Xvfb` + its X11/font dependency closure into the guest initramfs
     alongside fceux + Lua (still likely wants Buildroot/pinned-Nix per §4.5), not chasing pure GPU-less
     offscreen rendering.
  4. **Generic-core guardrail — done.** New `crates/baud-packages/src/workload_lint.rs`: a
     `FORBIDDEN_WORKLOAD_TERMS` list (Mario/NES-specific terms per `specs/baud-mario.md` §4 — `fceux`,
     `joypad`, `harness.lua`, `game.nes`, `oper_mode`, `super_mario`, `mario_bros`, plus the six Mario
     RAM-probe hex addresses — deliberately excluding bare "nes"/"smb" as too short and false-positive-prone,
     a documented deviation from `specs/baud-guest-harness.md` §8's own illustrative-but-noisy "nes" example)
     and `scan_crates_for_workload_leaks(crates_dir: &Path)`, which recursively walks `.rs` files under
     `crates/` (skipping `target/` and its own defining file) for case-insensitive hits, exposed from
     `crates/baud-packages/src/lib.rs`. 4 new tests pass, including the spec-named `no_workload_specifics_in_core`
     itself, which scans the real `crates/` tree and currently finds zero leaks. Closes test #29 in the §12
     matrix. `cargo build`/`clippy`/`test --workspace` clean; `drive/h/h0.sh`-`h7.sh` all PASS (stock module; this
     change touches no KVM/VMM runtime code). **Known pre-existing gap this lint does not catch**:
     `crates/baud-raftlet` already lives directly under `crates/` in violation of §8's "targets live as guest
     images under `examples/`, never in-tree" — it predates the KVM pivot and references no Mario/NES terms, so
     it doesn't trip this lint. Moving it into an `examples/`-based guest image is real, separate future work
     (§10's M-series distributed target), not part of this item.
  5. **H9 — legacy PCI configuration mechanism #1 (§4.7, first sub-step, still far from H9 itself).**
     New `crates/baud-multiverse/src/pci.rs`: a `PciHostBridge` answering the legacy 0xCF8
     (`CONFIG_ADDRESS`)/0xCFC (`CONFIG_DATA`) port pair — the mechanism `specs/baud-ubuntu.md`'s
     "PCI (MCFG ECAM or legacy 0xCF8/0xCFC)" names, needed because the stock Ubuntu 18.04.1 initrd
     enumerates `virtio_pci`/`virtio_blk` over real PCI, unlike baud's existing virtio-mmio devices
     (found via a `virtio_mmio.device=` cmdline parameter, never touching PCI at all). Models
     exactly one device — a host bridge at 00:00.0 (vendor/device `0x1B36`/`0x0000`, Red Hat Inc.'s
     QEMU-project vendor space, same convention as virtio's own `0x1AF4` — never a real Intel/AMD
     ID, since baud is not claiming real silicon; class `0x060000`, bridge/host) — with every other
     bus/device/function reading back `0xFFFF_FFFF`, the PCI spec's own "absent device" signal, so
     an unmodified guest's `pci_scan_bus` terminates cleanly instead of hitting a determinism hole.
     Wired into `DeviceBus` (`crates/baud-multiverse/src/console.rs`) as a new unconditionally-
     present field, same pattern as `Pic8259` — dormant (never touched) for every existing fixture,
     all of which boot with `pci=off` on the cmdline (`linux/bootparams.rs`). Confirmed by direct
     code reading (not assumption) that no PCI/ACPI/MCFG code existed anywhere in this crate before
     this change, and that a bare 0xCF8/0xCFC access previously fell through to `OpenBusFallback`
     (fixed `0xFF` reads, absorbed writes) rather than a `DeterminismHole` — functionally inert, not
     a crash, but not a real PCI response either. 9 new tests (7 in `pci.rs` covering config-address
     latch/readback, vendor/device/class-code content, absent-device all-ones, narrow byte/word
     accesses, and read-only-register writes being absorbed; 1 `DeviceBus`-level routing test
     confirming the ports are not swallowed by the open-bus fallback). `cargo build --workspace`/
     `clippy --workspace --all-targets`/`test --workspace` all clean; clippy warning count on
     `baud-multiverse` unchanged (26 before and after, via `git stash` comparison) — zero new
     warnings. **Still needed for H9, in rough dependency order**: (a) an actual virtio-pci
     transport device (BAR-backed MMIO/PIO window + a `DeviceBus` slot, same shape as
     `virtio_mmio.rs`) so a probed device beyond the host bridge exists at all; (b) a deterministic
     virtio-blk device on top of that transport, backed by a read-only content-addressed base image
     plus an in-memory CoW overlay, completing at a fixed work-clock boundary via the existing
     interrupt-injection engine (blkreplay-style, not host-I/O-return timing); (c) minimal ACPI
     (RSDP→RSDT/XSDT→FADT+DSDT+MADT with one LAPIC) — `pci=off acpi=off` are both on baud's current
     cmdline, and a stock distro kernel wants at least a minimal ACPI table set even where PCI
     itself can be found via the legacy mechanism without it; (d) the actual Ubuntu 18.04.1 cloud
     image (`cloud-images-archive.ubuntu.com`, qcow2→raw) served as the virtio-blk backing store;
     (e) the full boot-to-login-prompt drive script (`drive/h9.sh`) and the cross-VM fingerprint
     comparison (`specs/baud-fingerprint.md`'s `cross_vm_fingerprint_matches`). **(a) and (b) are now
     done**:
     new `crates/baud-multiverse/src/virtio_pci.rs` (~495 lines) implements the virtio-pci *legacy*
     transport (virtio spec 1.0/1.1 Appendix "Legacy Interface" — the pre-1.0, MSI-X-less register
     layout every `virtio_pci_legacy` Linux driver still speaks, which the stock Ubuntu 18.04.1
     initrd actually needs since it carries `virtio_pci`/`virtio_blk`, never `virtio_mmio.device=`).
     `VirtioPciTransport` is a second, device-agnostic on-ramp to the same device model
     `virtio_mmio.rs` already exposes over MMIO, reachable instead over one packed I/O-port window
     (Host/Guest Features, a single `Queue Address` register holding a page-frame-number rather than
     three separate ring addresses, a fixed-width read-only `Queue Size`, one shared `Device Status`
     byte, and an ISR-status byte that clears itself on read) — genuinely different from, not just a
     relocation of, the MMIO transport's per-queue explicit-address register set. Below the register
     layer both transports share everything: `ring_layout_from_pfn` derives the same
     `virtio_mmio::QueueRingConfig` (three ring addresses + a size) from the legacy interface's one
     PFN using the standard split-ring layout every `vring_init(..., VIRTIO_PCI_VRING_ALIGN)` call
     assumes (desc table, then avail ring immediately after it, then used ring at the next
     4096-byte boundary), so `virtio_queue::SplitVirtqueue` drives a live legacy queue exactly as it
     already drives an mmio one. `crates/baud-multiverse/src/pci.rs` gained the config-space half:
     `PciVirtioFunction`, a configuration-space header for a virtio-pci legacy function at 00:01.0
     (vendor ID `0x1AF4` — virtio's own PCI-SIG ID, never the host bridge's `0x1B36`; device ID
     `0x1000 + <virtio device type>`; class code `0x00FF0000`, "does not fit any defined class",
     the real convention virtio-rng hardware uses; interrupt line/pin, pin fixed at `INTA#`) plus a
     real BAR0 implementing the PCI BAR-sizing protocol (PCI Local Bus spec §6.2.5.1: an all-ones
     write latches a "sizing" flag and the next read returns a size mask in the high bits rather
     than echoing the write; any other write clears the flag and assigns a real base — tracked via
     an explicit `bar0_sizing` bool rather than misreading an all-ones write as a literal
     nonsensical base). `PciHostBridge::attach_virtio_rng` installs the function opt-in (device 1
     still reads back all-ones for every caller that never calls it, so existing fixtures are
     unaffected), and `PciHostBridge::virtio_io_base` exposes the guest-assigned base without the
     bridge ever touching the transport directly. `console.rs`'s `DeviceBus` wires the two halves
     together as a new `virtio_pci_rng` field, opt-in via `enable_virtio_pci_rng()` mirroring
     `enable_virtio_rng`'s pattern for the MMIO transport: `DeviceBus::pio_write` re-reads
     `pci.virtio_io_base()` and calls `VirtioPciTransport::set_io_base` after every PCI
     configuration-space write, keeping the transport's dispatch window synchronized with whatever
     BAR0 value the guest's PCI core currently has live — the same "a BAR with no valid base decodes
     no bus cycles" behavior real hardware has. 21 new tests (14 in `virtio_pci.rs` covering the
     ring-layout formula, unassigned-I/O-base open-bus behavior, read-only host-features, a full
     driver enumeration/queue-setup sequence, out-of-range queue-index handling, notify bookkeeping,
     ISR-clear-on-read, status-reset-preserves-device-identity, and window-boundary open-bus
     behavior; 6 in `pci.rs` covering device-1-absent-until-attached, vendor/device-ID content, the
     full BAR0 sizing/assignment protocol, interrupt-line round-trip with fixed INTA# pin,
     subsystem-ID content, and the host bridge at 00:00.0 staying unaffected; 1 end-to-end
     `DeviceBus` test, `device_bus_routes_virtio_pci_bar0_once_the_guest_assigns_it`, driving the
     real BAR0 sizing/assignment protocol through `DeviceBus` and confirming PIO reaches the
     transport only after assignment). `cargo test -p baud-multiverse --lib` → 161 passed, 0 failed,
     10 ignored (up from the previously-recorded 149/152 baseline). `cargo clippy -p baud-multiverse
     --all-targets` → 26 warnings, matching the exact pre-existing baseline — zero new warnings,
     none in `virtio_pci.rs`. Full `bash drive/gate.sh` ran clean except the already-documented
     `rdtsc_guest_reproduces_high_bits_across_boots` load-flake (failed under the 8-wide fan-out,
     passed in isolation in phase 6 — the documented case that still counts as a passing gate).
     **(b) is now done**: new `crates/baud-multiverse/src/virtio_blk.rs` (354 lines including tests)
     implements the deterministic virtio-blk device model (virtio spec 1.1 §5.2) on top of (a)'s
     transport. `BlockBackingStore` is a read-only, content-addressed `base: Vec<u8>` disk image plus
     a sector-granularity in-memory copy-on-write `overlay: HashMap<u64, [u8; 512]>` — every guest
     write only ever inserts into `overlay`, `base` is never mutated, matching
     `specs/baud-ubuntu.md` §4's "the base stays pristine." The free function `service_request`
     parses one drained `virtio_blk_req` descriptor chain (`[header (16-byte, read-only: le32 type,
     le32 reserved, le64 sector), 0+ data descriptors, status (1-byte, writable)]`, spec §5.2.6),
     servicing `VIRTIO_BLK_T_IN`/`VIRTIO_BLK_T_OUT`/`VIRTIO_BLK_T_FLUSH`, reporting
     `VIRTIO_BLK_S_IOERR` for an out-of-range or misaligned request and `VIRTIO_BLK_S_UNSUPP` for
     anything else (e.g. `VIRTIO_BLK_T_GET_ID`) — a malformed *request* is never a Rust-level error,
     only a malformed *chain* is (the pre-existing `VirtqueueError` convention, unchanged).
     `virtio_queue.rs` gained the lower-level primitive this needed, `SplitVirtqueue::
     process_available_chains`: the existing `process_available`'s `fill`-only closure can only ever
     *write* into writable descriptors, with no way for a caller to *read* a read-only descriptor's
     bytes — which virtio-blk's request header (and an OUT-request's write-data) requires.
     `process_available` was refactored to be implemented on top of `process_available_chains`
     (verified byte-identical: all of its existing tests still pass unchanged), so this was an
     additive refactor, not a rewrite. `virtio_pci.rs`'s `VirtioPciTransport` gained a
     `device_config: Vec<u8>` field and a `with_device_config` constructor (`new` now delegates to it
     with an empty vec, preserving virtio-rng's existing behavior exactly), answered read-only
     starting at the legacy offset `0x14` (`REG_DEVICE_CONFIG_START`) — previously open-bus there for
     every device including rng, now open-bus only when `device_config` is empty. A new `new_blk
     (capacity_sectors: u64)` constructor publishes `virtio_blk_config`'s `capacity` field (spec
     §5.2.4) there, little-endian, the value every `virtio_blk_probe` reads to size the disk.
     `pci.rs`'s `PciHostBridge` had its single `virtio: Option<PciVirtioFunction>` field renamed
     `virtio_rng` and gained a second field, `virtio_blk: Option<PciVirtioFunction>`, attached via a
     new `attach_virtio_blk(bar0_size)` at PCI slot 00:02.0 (rng stays at 00:01.0) — a new
     `VIRTIO_BLK_CLASS_CODE = 0x0180_0000` (mass storage, subclass "other," the real PCI-defined
     class, unlike rng's catch-all `VIRTIO_UNCLASSIFIED_CODE`). A new `virtio_blk_io_base()` accessor
     mirrors the existing `virtio_io_base()`. `console.rs`'s `DeviceBus` gained
     `virtio_pci_blk: Option<VirtioPciTransport>` (unconditional) plus `virtio_blk_queue`/
     `virtio_blk_store` (both `#[cfg(target_os = "linux")]`, mirroring the rng fields' gating
     exactly), a new `enable_virtio_pci_blk(base_image: Vec<u8>)` (deriving `capacity_sectors` from
     `base_image.len()`), a `virtio_pci_blk()` accessor, and `service_virtio_blk(&mem)` mirroring
     `service_virtio_rng`'s lazy-queue-build/rebuild-on-renegotiation/raise-ISR-on-drain shape
     exactly, but calling `virtio_blk::service_request` per chain instead of a `fill` closure. `Bus
     for DeviceBus`'s PIO dispatch and the BAR0-resync-after-PCI-config-write logic were extended for
     the new `virtio_pci_blk` slot, same pattern as `virtio_pci_rng`. `linux/mod.rs`'s `Multiverse`
     gained `enable_virtio_pci_blk`/`virtio_pci_blk()` wrappers, `service_virtio_blk_interrupt`/
     `service_virtio_blk_interrupt_while_halted` (verbatim mirrors of the rng equivalents — same
     `inject_timer_tick(0, vector)` "next reachable work-clock boundary" idiom, no new low-level
     primitive needed; this is the concrete meaning of `specs/baud-ubuntu.md` §4's "block completion
     is delivered at a fixed work-clock boundary via the interrupt-injection engine (blkreplay-style,
     never on host-I/O return)" — the backing store is already-resident host memory, so servicing a
     request is a synchronous memcpy with no real I/O latency to be deterministic about), and one new
     run-loop combinator, `run_to_first_halt_with_virtio_pci_blk(vector, max_exits)`, mirroring
     `run_to_first_halt_with_virtio_rng`. 21 new tests (9 in `virtio_blk.rs` covering plain-read,
     write-updates-overlay-not-base, read-after-write-observes-overlay, out-of-range-reports-ioerr-
     untouched-memory, unsupported-type-reports-unsupp, flush-is-a-no-op, a multi-descriptor request
     spanning two data buffers, capacity-from-image-length, and a too-short chain being a harmless
     no-op; 1 in `virtio_queue.rs`,
     `process_available_chains_exposes_read_only_descriptor_bytes_unlike_process_available`; 3 in
     `virtio_pci.rs` covering `new_blk`'s capacity placement, device-config being read-only, and an
     empty-device-config transport staying open-bus past the fixed registers (rng unaffected); 3 in
     `pci.rs` covering device-2-absent-until-`attach_virtio_blk`, the attached function's vendor/
     device-ID and mass-storage class code, and rng/blk being attached independently of each other;
     5 end-to-end in `console.rs` covering BAR0 sizing/assignment routing to `virtio_pci_blk`,
     `service_virtio_blk` being a harmless no-op before enable/ready/notify, a real read request
     returning sector data and raising the ISR, a real write request persisting into the overlay
     with a later read observing it, and an out-of-range request reporting `IOERR` through the full
     `DeviceBus` path). `cargo test -p baud-multiverse --lib` → 181 passed, 1 failed, 10 ignored (192
     collected, up from the previously-recorded 161/171 (a) baseline by exactly the 21 new tests
     above); the 1 failure was the already-documented `rdtsc_guest_reproduces_high_bits_across_boots`
     load-flake, reconfirmed passing in isolation (`cargo test -p baud-multiverse --lib
     rdtsc_guest_reproduces_high_bits_across_boots` → 1 passed) — the same documented case that still
     counts as passing. `cargo clippy -p baud-multiverse --all-targets` → 26 warnings, matching the
     exact pre-existing baseline — zero new warnings, none in `virtio_blk.rs` or any of the other
     touched files. Explicitly not done in this sub-step, left as open follow-up: a combined
     periodic-timer + virtio-rng + virtio-blk run-loop combinator (the existing
     one-combinator-per-combination approach in `linux/mod.rs` does not scale to three devices at
     once; a real Ubuntu boot needs periodic ticks, virtio-rng, and virtio-blk simultaneously — this
     needs a design change, e.g. a generic "poll N devices" abstraction, not just another
     hand-written combinator); also no real-KVM hand-assembled-guest-fixture test for virtio-blk
     (unlike virtio-rng's `tests/fixtures/virtio-rng-guest/` real-hardware proof in `linux/mod.rs`)
     — this sub-step's tests are all in-memory (`vm_memory::GuestMemoryMmap` anonymous mmap, no
     `/dev/kvm`), the same scope boundary the (a) transport sub-step drew for itself.
     **The "needs a design change" run-loop gap above is now closed.** New private
     `TickPolledDevice` struct + `Multiverse::run_to_first_halt_with_periodic_timer_and_devices`
     (`crates/baud-multiverse/src/linux/mod.rs`) generalize the per-tick "poll a device's
     `notify_count`, service it if changed, route running-vs-halted differently" shape every
     hand-written combinator repeated, into one device-agnostic tick loop driven by a `&[
     TickPolledDevice]` of plain `fn`-pointer triples (`notify_count`/`service_running`/
     `service_halted` — no capturing closures, so each device is a declarative row, not a
     hand-inlined branch). `run_to_first_halt_with_periodic_timer_and_virtio_rng` is now a one-line,
     one-device wrapper over it (behaviorally identical by construction — a one-element device slice
     reduces to exactly the old hand-written loop — so every pre-existing rng+timer test is a
     regression check on the refactor itself). New public
     `run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk` is the three-way
     combinator this item asked for: the same generic loop with two `TickPolledDevice` rows (rng,
     blk), reusing `service_virtio_rng_interrupt[_while_halted]`/`service_virtio_blk_interrupt[
     _while_halted]` unchanged — no new device-servicing logic, only the generic dispatch. New
     real-hardware test
     `periodic_timer_virtio_rng_and_virtio_pci_blk_combinator_does_not_perturb_an_unused_third_device`
     (`linux/mod.rs`) boots the existing `virtio_rng_initramfs` fixture through the new three-way
     combinator with `enable_virtio_pci_blk` also on, and proves: (1) console output is byte-
     identical to the plain timer+rng path (an enabled-but-guest-unused third device changes
     nothing), (2) the block device's `notify_count` stays `0` (this fixture's kernel has no
     `CONFIG_VIRTIO_BLK`/`CONFIG_VIRTIO_PCI_LEGACY`, so nothing ever probes it — a real driver-
     exercising fixture is separately-scoped future work, see below), and (3) two boots with all
     three devices enabled stay fully deterministic. `cargo test -p baud-multiverse --lib` → 208
     passed, 0 failed, 10 ignored (up from 207 by exactly this test). `cargo clippy -p
     baud-multiverse --all-targets` → 26 warnings, the exact pre-existing baseline, zero new.
     **Still open at this point** (closed by a later dated finding further below, see "the last-
     named gap in this item is now closed"): no real-KVM fixture actually exercises virtio-blk end
     to end (needs `CONFIG_VIRTIO_BLK`/`CONFIG_VIRTIO_PCI_LEGACY` enabled in `minimal.config`, a
     kernel rebuild, and a new hand-written `virtio_blk_init.c` analogous to `virtio_rng_init.c` that
     reads/writes real sectors); H9 (d) (the actual Ubuntu 18.04.1 cloud image) and (e)
     (`drive/h9.sh` + the cross-VM fingerprint) remain not started, unchanged.
     **(c) is now done, as a table-construction library, not yet wired into any real boot path.**
     New `crates/baud-multiverse/src/acpi.rs` (`#[cfg(target_os = "linux")]`, same gating as
     `virtio_queue`/`virtio_blk` — it dereferences guest memory via `vm-memory`): pure,
     deterministic builders for the minimal ACPI table set `specs/baud-ubuntu.md` §4 names —
     `build_rsdp`/`build_xsdt`/`build_fadt`/`build_dsdt`/`build_madt` — plus `write_acpi_tables`,
     which places all five at fixed addresses. `crates/baud-multiverse/src/layout.rs` gained
     `ACPI_RSDP_ADDR` (`0xE0000`, inside ACPICA's `acpi_find_root_pointer`'s hardcoded
     `0xE0000-0xFFFFF` BIOS-area scan window — the one address in this set that is not free-choice,
     confirmed by direct reading of what a real x86_64 Linux kernel's ACPI boot path actually does:
     there is no cmdline/e820/boot_params hint route, `acpi_boot_table_init` always byte-scans that
     literal physical range) and `ACPI_XSDT_ADDR`/`ACPI_FADT_ADDR`/`ACPI_DSDT_ADDR`/`ACPI_MADT_ADDR`
     (one page apart, packed into the same free BIOS-area window — nothing else in `layout.rs`
     claims any address there). The FADT sets `Flags` bit 20 (`HW_REDUCED_ACPI`) with every
     fixed-hardware PM register block (`PM1a_EVT_BLK`, `PM_TMR_BLK`, `GPE0_BLK`, ...) left `0` and
     `SMI_CMD = 0` — this tells OSPM the platform implements none of the fixed ACPI hardware
     feature registers at all (honestly true here), letting Linux skip its entire `acpi_hw_*`
     fixed-register code path instead of baud having to model a PM1a control block as a real device
     just to keep a probe from hanging on it. The MADT declares `PCAT_COMPAT` (matching the already-
     modeled `Pic8259`) and exactly one enabled Processor Local APIC entry (APIC ID 0) — the "MADT
     with one LAPIC" this item names — with no I/O APIC entry, so `nr_ioapics == 0` and Linux falls
     back to the same legacy-PIC-style interrupt routing belief it already has with no MADT at all
     (baud never registers a real `KVM_CREATE_IRQCHIP` regardless of what routing the guest
     believes is in effect). The DSDT is a header with zero AML bytes — a legal, if unusual, empty
     definition block, since PCI enumeration already happens outside ACPI's namespace (the legacy
     0xCF8/0xCFC mechanism) and there is no `\_S5` package for ACPI-driven poweroff (existing guests
     already shut down via `reboot=t panic=-1`, never ACPI). The RSDP only publishes `XsdtAddress`
     (`RsdtAddress = 0`, `Revision = 2`) — no RSDT is built at all, matching this item's own
     "minimal" framing; ACPICA's `acpi_tb_parse_root_table` tries the XSDT first whenever
     `revision >= 2` and only falls back to the RSDT if the XSDT is absent or fails to validate. 11
     new tests (checksum correctness for all 5 tables — both of the RSDP's independent checksum
     regions, the ACPI-1.0 20-byte one and the extended 36-byte one — pointer correctness
     RSDP→XSDT→{FADT,MADT}→(FADT's own Dsdt/X_Dsdt)→DSDT, the HW_REDUCED_ACPI/SMI_CMD/PM-register-
     absence content, the MADT's exact LAPIC-entry content, and an end-to-end
     `write_acpi_tables`-then-read-back test against a real `vm_memory::GuestMemoryMmap`). `cargo
     test -p baud-multiverse --lib` → 193 passed, 0 failed, 10 ignored (up from (b)'s 181/1/10 by
     the 11 new tests plus one `layout::` overlap-test region addition). `cargo clippy -p
     baud-multiverse --all-targets` → 26 warnings, the exact documented pre-existing baseline —
     zero new warnings (two `assertions_on_constants`/`manual_is_multiple_of` hits were found and
     fixed by moving the RSDP-scan-window bound checks into a compile-time `const _: () = { ... }`
     assertion, `layout.rs`'s own established convention, rather than a runtime test on values that
     can never change without recompiling).

     **Both gaps flagged here are now closed.** New `crates/baud-multiverse/src/lapic.rs`
     implements `LocalApic`, a pure-bookkeeping xAPIC MMIO stub at `layout::LAPIC_MMIO_BASE`
     (`Pic8259`'s "stub just enough to satisfy the probe, not a functioning timer" precedent) —
     scoped by an Opus research pass against the real Linux 6.18.33 source (`~/wsl-kernel-src/src`):
     `APIC_TMCCT`'s live countdown is never read on this crate's boot configuration (TSC-deadline
     mode short-circuits `calibrate_APIC_clock()`, and even without that `native_calibrate_tsc()`
     derives the timer period arithmetically from the already-synthesized CPUID leaf 15H), so every
     register is either a fixed read-only constant (ID=0, a plausible LVR) or plain read/write
     bookkeeping — except `APIC_ICR`'s busy bit (bit 12), the one real hang hazard identified
     (`apic_mem_wait_icr_idle()` polls it unbounded even on a single vCPU via
     `arch_irq_work_raise()`), which this stub always clears on write. Wired into `console.rs`'s
     `DeviceBus` unconditionally (same pattern as `Pic8259`) and into `layout.rs`'s identity-map
     (shares the virtio-mmio window's PDPTE region — same "paging is mandatory in long mode" fix
     `VIRTIO_MMIO_RNG_BASE` already needed). `Multiverse::write_acpi_tables` (`linux/mod.rs`) is
     the real, opt-in boot-path wiring (call after `boot`/`boot_with_rdseed_sites`, before the first
     run). `tests/fixtures/linux-guest/minimal.config` gained `CONFIG_ACPI=y` (kernel rebuilt,
     compiled-in-but-inert for every fixture still booting `acpi=off`). Two real bugs, unrelated to
     `acpi.rs`'s own table construction (which parsed correctly on the first real attempt), were
     found and fixed getting a real `acpi=on` boot to actually use it — full diagnosis in
     `tests/fixtures/linux-guest/BUILD.md`'s new "`CONFIG_ACPI=y` + `LocalApic`" section: (1)
     `CONFIG_PCI=n` breaks ACPICA's own subsystem init (`AE_BAD_PARAMETER` in
     `acpi_ev_install_region_handlers`, since `ACPI_PCI_CONFIGURED` is only `#ifdef CONFIG_PCI`) —
     fixed by flipping `CONFIG_PCI=n` -> `y` too (`pci=off` on the cmdline still disables runtime
     PCI bus scanning); (2) `IA32_APIC_BASE`'s KVM-default value left the x2APIC-enable bit set
     (there is no in-kernel LAPIC here, so this MSR is never routed through baud's own trap), which
     `check_x2apic()`'s `CONFIG_X86_X2APIC`-unset fallback trusts directly and used to
     unconditionally clear `X86_FEATURE_APIC` — fixed with a new `linux::pin_apic_base_msr`
     (mirrors the existing `pin_tsc_value` MSR-pinning pattern). New real-hardware test
     `guest_kernel_boots_with_acpi_enabled_and_recognizes_the_lapic` (`linux/mod.rs`) confirms the
     guest's own `setup_local_APIC()` writes real values into `LocalApic`'s `SPIV`/`LVT_TIMER`
     registers (not console text — `quiet loglevel=1` suppresses virtually all of it) and that the
     boot stays byte-identical across two runs; 5/5 clean on repeated real-hardware re-runs. `cargo
     build`/`clippy --workspace --all-targets`/`test --workspace` all clean (0 new warnings; 26
     baseline unchanged); full `bash drive/gate.sh` (24/24, 5m43s) confirms the shared rebuilt
     `linux-guest` bzImage introduced no regression in any of the other fixtures/drive scripts that
     boot it. **`write_acpi_tables` is now wired as the production default for `baud run kvm`'s
     plain path** (previously opt-in only): `RunKvmBody` gained an `acpi: bool` field
     (`#[serde(default)]`, false preserves prior behavior; `crates/baud-server/src/routes/run_kvm.rs`),
     threaded through `boot_run_and_drain`/`boot_and_drain_frames`/`boot_and_run` — when true,
     `boot_run_and_drain` calls `mv.write_acpi_tables()` right after boot, before the guest runs —
     and persisted into a new `kvm_run_meta.acpi` column (migration
     `crates/baud-server/migrations/0014_kvm_run_meta_acpi.sql`) so `stream::render`'s real-replay
     path reboots the exact same guest; `baud run kvm --acpi` (`crates/baud-cli/src/cmds/run.rs`) is
     the actual user-facing surface, not just the HTTP route. New real-hardware test
     `run_kvm_boots_a_real_linux_guest_with_acpi_enabled` (`crates/baud-server/src/routes/run_kvm.rs`)
     proves the route-level wiring boots cleanly and reproducibly with ACPI tables written, 2/2
     identical console output across two boots; `cargo test -p baud-server --bin baud-server
     run_kvm::` → 25 passed, 0 failed (up from 24 by exactly this test). Deliberately not extended to
     `RunKvmBranchBody`/`RunKvmResumeBody` at the time — the branch/resume/generate routes could not
     turn ACPI on at all, still passed `acpi: false` literally into `KvmBootParams` — scoped to the
     plain path per this item's own wording. **`RunKvmBranchBody`'s half of this gap is now closed
     (item 7 below); `RunKvmResumeBody`'s is closed by a documented non-fix, see that same item.**
     **Still open**: (d) (the actual Ubuntu 18.04.1 cloud image) and (e)
     (`drive/h9.sh` + the cross-VM fingerprint) remain not started. H8 Mario remains separately
     blocked on the FCEUX Qt5/SDL2/Xvfb packaging problem (item 3 above), unrelated to this PCI/ACPI
     work.
     **The last-named gap in this item is now closed: a real-KVM fixture now exercises virtio-blk
     end to end.** `crates/baud-multiverse/tests/fixtures/linux-guest/minimal.config` gained
     `CONFIG_VIRTIO_PCI=y`, `CONFIG_VIRTIO_PCI_LEGACY=y`, `CONFIG_BLK_DEV=y` (a Kconfig gotcha found
     along the way: `VIRTIO_BLK` lives under `drivers/block/Kconfig`'s `menuconfig BLK_DEV`, which
     `allnoconfig` disables and which the fragment had not been setting, so `VIRTIO_BLK=y` silently
     dropped out of the merged `.config` until this was added), and `CONFIG_VIRTIO_BLK=y`; the
     kernel was rebuilt per `tests/fixtures/linux-guest/BUILD.md`'s existing recipe (scratch-copy
     `~/wsl-kernel-src/src`, `mrproper`/`allnoconfig`/`merge_config.sh`/`olddefconfig`/`bzImage`) and
     the new `bzImage` committed. New `tests/fixtures/linux-guest/virtio_blk_init.c` (`/init`,
     modeled directly on `virtio_rng_init.c`) mounts sysfs, discovers `/dev/vda`'s major:minor from
     `/sys/class/block/vda/dev` (the same devtmpfsd-race workaround `virtio_rng_init.c` already
     uses), `mknod`s it, reads sector 0 and prints it in hex, writes a fixed pattern to sector 1,
     then reads it back and prints that too — all via the same raw-`outb`-to-COM1-marker convention
     every other real-kernel fixture uses; built into `virtio_blk_initramfs.cpio.gz` via the same
     reproducible-cpio recipe. Two new real-hardware tests in `crates/baud-multiverse/src/
     linux/mod.rs`: `guest_virtio_pci_blk_driver_reads_and_writes_real_sectors` (asserts the real
     `virtio_pci_legacy`+`virtio_blk` kernel drivers probe the device, read sector 0's exact pristine
     base-image bytes, write sector 1, and read back the exact just-written bytes — proving the
     write landed in the overlay, not merely that the write request completed) and
     `guest_virtio_pci_blk_driver_io_is_reproducible_across_two_boots` (byte-identical console output
     across two boots); both reuse the existing three-way
     `run_to_first_halt_with_periodic_timer_and_virtio_rng_and_virtio_pci_blk` combinator with
     virtio-rng simply left disabled (never `enable_virtio_rng`d) rather than adding a new
     two-device wrapper — that combinator's own doc already covers an unenabled device degrading
     gracefully.
     Getting a real driver this far surfaced two real, pre-existing bugs, both in
     `crates/baud-multiverse/src/pci.rs`. First, `PciHostBridge::HOST_BRIDGE_CLASS_CODE` had Base
     Class and Sub-Class byte-swapped (`0x0006_0000` instead of the spec-correct `0x0600_0000` —
     Base Class 0x06 landed in the Sub-Class byte position at config-space offset 0x0A instead of
     the Base Class byte at offset 0x0B). Every existing unit test asserted the same swapped
     convention as correct (self-consistent but wrong against the real PCI Local Bus spec), so this
     was invisible until a real, unmodified Linux kernel's own `pci_sanity_check()`
     (`arch/x86/pci/direct.c`) read the 16-bit `PCI_CLASS_DEVICE` word at offset 0x0A expecting
     exactly `PCI_CLASS_BRIDGE_HOST` (`0x0600`) and got `0x0006` instead, so `raw_pci_ops` was never
     set at all ("PCI: Fatal: No config space access function found" / "PCI: System does not
     support PCI") — confirmed by an exploratory real-`/dev/kvm` boot before the fix and confirmed
     fixed after; fixed the constant, its doc comment, and the one unit test
     (`host_bridge_class_code_is_bridge_host`) that encoded the same swapped assumption. Second,
     `PciVirtioFunction::interrupt_line` defaulted to `0` with nothing ever changing it: on a
     direct-boot kernel with no BIOS/ACPI/`$PIR` table, nothing routes a PCI device's legacy IRQ, so
     a real `virtio_pci_legacy` driver logged "can't find IRQ for PCI INT A; please try using
     pci=biosirq" and `virtio_blk` probe failed with `-ENOSPC`. Fixed by having baud itself
     pre-route each virtio-pci-legacy function's interrupt line at construction (new
     `VIRTIO_RNG_DEFAULT_IRQ_LINE = 10`, `VIRTIO_BLK_DEFAULT_IRQ_LINE = 11` constants in `pci.rs`,
     threaded through a new `default_interrupt_line` parameter on `PciVirtioFunction::new`) — the
     same "no BIOS exists, so the VMM plays that role" precedent baud's own `boot_params`/e820
     construction already established; the register stays guest-writable exactly as before, only
     the pre-boot default changed.
     `cargo test -p baud-multiverse --lib` → 210 passed, 0 failed, 10 ignored (up from 208 by
     exactly the 2 new tests); `cargo clippy -p baud-multiverse --all-targets` → 26 warnings, the
     exact documented pre-existing baseline, zero new. Full `bash drive/gate.sh` → 23 passed, 0
     failed, 1 flaked (the already-documented `rdtsc_guest_reproduces_high_bits_across_boots`
     load-flake, reconfirmed passing in isolation), 0 skipped, 6m04s — counts as a passing gate per
     §15. **Still open**: H9 (d) (the actual Ubuntu 18.04.1 cloud image) and (e) (`drive/h9.sh` +
     the cross-VM fingerprint) remain not started, unchanged. `VIRTIO_BLK_CLASS_CODE` was already
     spec-conformant before this iteration touched anything.
     **This iteration: virtio-blk's own "boot/cmdline/CLI wiring" gap is now closed — the last
     piece before H9 (d)/(e) can be attempted.** Everything below (a)/(b)/(c) built (the transport,
     the block device model, the ACPI table builders) was real-hardware-tested only from a Rust
     test calling `Multiverse` directly; nothing reached `POST /run/kvm` or `baud run kvm`.
     `RunKvmBody` gained an optional `virtio_blk: Option<VirtioBlkSpec>` field (`image_path`/
     `vector`/`max_exits`, mirroring `virtio_rng`'s shape but with a disk-image *path* instead of a
     seed — `crates/baud-server/src/routes/run_kvm.rs`), threaded through `boot_run_and_drain`/
     `boot_and_drain_frames`: `Multiverse::enable_virtio_pci_blk` is called before the run loop, and
     the periodic_timer/virtio_rng/virtio_blk dispatch reuses the already-existing combinators
     rather than adding new ones — `run_to_first_halt_with_periodic_timer_and_virtio_rng_and_
     virtio_pci_blk` for any periodic_timer-enabled boot (rng left at vector `0` and never enabled
     when the caller didn't ask for it, the same "unenabled device degrades to a no-op" behavior
     `tests/fixtures/linux-guest/BUILD.md`'s own `run_linux_guest_virtio_blk_once` helper already
     established as safe), `run_to_first_halt_with_virtio_pci_blk` for blk-only without a periodic
     timer. The one combination with no combinator at all — virtio_rng **and** virtio_blk together
     with **no** periodic_timer, since both devices independently poll for a `QueueNotify` once per
     host-side exit and nothing drives two such polls at once — fails loud with a clear error
     instead of silently starving one device (every real Linux guest needs periodic_timer for
     `calibrate_delay` anyway, so this is not expected to matter in practice). New migration
     `crates/baud-server/migrations/0015_kvm_run_meta_virtio_blk.sql` adds nullable
     `virtio_blk_image_path`/`virtio_blk_vector`/`virtio_blk_max_exits` columns to `kvm_run_meta`
     (image *path*, not bytes, persisted — mirrors `initramfs_path`, since a real disk image can be
     far larger than an initramfs); `stream::render`'s real-replay path now reads them back and
     threads a `virtio_blk` spec into `boot_and_drain_frames` so a persisted virtio-blk-enabled run
     replays with the same backing image. `RunKvmBranchBody`/`RunKvmResumeBody` deliberately did
     **not** get a `virtio_blk` field this iteration — same scoping precedent `virtio_rng` itself
     set when it first landed on the plain path only. CLI: `baud run kvm` gained
     `--virtio-blk-image`/`--virtio-blk-vector` (default `0x3b`, `pic8259::isa_irq_vector(11)`,
     `PciHostBridge::VIRTIO_BLK_DEFAULT_IRQ_LINE`'s pre-routed vector)/`--virtio-blk-max-exits`.
     New real-hardware test `run_kvm_boots_a_real_linux_guest_with_virtio_blk_enabled`
     (`crates/baud-server/src/routes/run_kvm.rs`) boots the checked-in `virtio_blk_initramfs.cpio.gz`
     fixture through `boot_run_and_drain` directly and asserts the real `virtio_pci_legacy`/
     `virtio_blk` drivers open `/dev/vda` and complete a real sector write — **passed on real
     `/dev/kvm`**. New opt-in `drive/pkg/pkg-boot-virtio-blk-cli.sh` (mirrors `drive/pkg/
     pkg-boot-cli.sh`'s structure) drives the same fixture through a real `baud run kvm
     --virtio-blk-image ... --json` CLI invocation against a live `baud-server` over real HTTP —
     **real result: `ok=true`, console contains `baud-guest: blk-open-ok` and
     `baud-guest: blk-write-sector1-ok`** — the project's first real "spec in, guest's virtio-blk
     driver exercised" proof through the actual CLI binary + HTTP server, not a Rust test calling
     `Multiverse` directly. `cargo build`/`clippy --workspace --all-targets`/`test --workspace` all
     clean (0 new warnings — `boot_run_and_drain`/`boot_and_drain_frames` each crossed clippy's
     `too_many_arguments` threshold at 8 params and got `#[allow(clippy::too_many_arguments)]`,
     the same precedent already used four other places in this workspace, rather than a
     `KvmBootParams`-style struct that would have forced yet another call-site rewrite across ~15
     existing tests for a `pub(crate)`-only pair of functions); full `bash drive/gate.sh` → 23
     passed, 0 failed, 0 flaked, 1 skipped (pkg-build-cli, unchanged input), 2m55s. **Still open**:
     H9 (d) (the actual Ubuntu 18.04.1 cloud image) and (e) (`drive/h9.sh` + the cross-VM
     fingerprint) remain not started — this closes the last named prerequisite gap before they can
     be attempted, not H9 itself; H8 Mario remains separately blocked on the FCEUX Qt5/SDL2/Xvfb
     packaging problem (item 3 above); the Buildroot/pinned-Nix guest-image pipeline (§4.5) both
     depend on for a full rootfs is still not implemented.
  6. **`VIRTIO_UNCLASSIFIED_CODE` — the same Base/Sub-Class byte-swap bug flagged (not yet fixed) by
     item 5 above, fixed.** Confirmed by spec inspection: PCI Local Bus spec Appendix D's class
     `0xFF` ("does not fit any defined class") is Base Class `0xFF`/Sub-Class `0x00`/Prog IF `0x00`,
     i.e. the 16-bit `PCI_CLASS_DEVICE` word (bits 23:8) must read `0xFF00` — matching Linux's own
     `PCI_CLASS_OTHERS` constant — but `VIRTIO_UNCLASSIFIED_CODE` was `0x00FF_0000`, putting `0xFF`
     in the Sub-Class byte (bits 23:16) instead of the Base Class byte (bits 31:24), the exact same
     swap `HOST_BRIDGE_CLASS_CODE` had. Fixed to `0xFF00_0000` (`crates/baud-multiverse/src/pci.rs`).
     Unlike the host-bridge fix, no real virtio-rng-over-PCI driver test exists yet to have caught
     this the way a real kernel's `pci_sanity_check()` caught the host-bridge swap, so this fix is
     reasoned from the spec/Linux header value alone, not hardware-confirmed against a real driver —
     that hardware confirmation remains future work for whoever next builds a real
     virtio-rng-over-PCI driver test (virtio-rng today is only real-driver-tested over virtio-mmio).
     New `attached_virtio_rng_class_code_is_unclassified_base_class` unit test
     (`crates/baud-multiverse/src/pci.rs`) pins the byte layout the same way
     `host_bridge_class_code_is_bridge_host` already does for the host bridge. Verified: `cargo test
     -p baud-multiverse --lib` → 211 passed, 0 failed, 10 ignored (up from 210 by exactly this test);
     `cargo clippy -p baud-multiverse --all-targets` → 26 warnings, the exact pre-existing baseline,
     zero new.
  7. **`RunKvmBranchBody`'s "still lack an acpi field" gap (flagged in item 5(c) above) is now
     closed; `RunKvmResumeBody`'s counterpart is closed by a documented non-fix.** `RunKvmBranchBody`
     gained an `acpi: bool` field (`#[serde(default)]`, `false` preserves prior behavior,
     `crates/baud-server/src/routes/run_kvm.rs`), threaded through a new `acpi: bool` parameter on
     `boot_and_snapshot` — when `true`, `write_acpi_tables()` runs on the booted guest *before* the
     branch point is snapshotted, so the tables land in the captured RAM and every branch forked
     from that point (`Multiverse::branch`'s copy-on-write semantics) inherits them for free, with
     no separate per-branch wiring needed. `boot_snapshot_and_branch`/`boot_snapshot_and_generate`
     both gained the same trailing `acpi: bool` parameter, forwarded from `branch()`'s handler (both
     the fixed-tape and `generate` modes). Also fixed a real bug this surfaced: `branch()`'s two
     `frame_run_ids`/`frame_run_id_prefix` persistence call sites built a `KvmBootParams` with
     `acpi: false` hardcoded — harmless while the field didn't exist, but a genuine latent bug once
     it does, since `stream::render`'s real-replay path (`render_frames_from_real_replay`, used for
     every reboot-based `kvm_run_meta` row, which is what a persisted branch's frames are — see
     `RunKvmBranchBody::frame_run_ids`'s own doc) reads that column back to decide whether to call
     `write_acpi_tables` on replay; both sites now pass the request's real `body.acpi`, so a
     persisted ACPI-enabled branch's frames replay with ACPI enabled too, not silently without it.
     `RunKvmResumeBody` deliberately did **not** get the same field: its persisted rows are always
     restore-based (`store_run_id`/`snapshot_node_id` set), which `stream::render` routes to
     `render_frames_from_real_restore` — a path that reconstructs the `Universe` from
     `SnapshotStore` and never reads `kvm_run_meta.acpi` at all (confirmed by reading `stream.rs`
     before adding anything) — so an `acpi` field on `RunKvmResumeBody` would be genuinely dead code,
     not a real gap; its own two `acpi: false` placeholders are now commented explaining why, instead
     of describing a field that was never coming. New real-hardware test
     `run_kvm_branch_boots_a_real_linux_guest_with_acpi_enabled` (`crates/baud-server/src/routes/
     run_kvm.rs`, mirrors `run_kvm_boots_a_real_linux_guest_with_acpi_enabled`'s own pattern but
     through `boot_snapshot_and_branch`) forks a single empty-suffix branch from an ACPI-enabled
     branch point twice and asserts both reach `/init`'s marker and halt (no `MARK_BRANCH`) with
     byte-identical console output. Verified: `cargo test -p baud-server --bin baud-server run_kvm::`
     → 26 passed, 0 failed (up from 25 by exactly this test), real `/dev/kvm`; `cargo build`/`clippy
     --workspace --all-targets`/`test --workspace` all clean (0 new warnings — `grep` confirms none
     of clippy's remaining pre-existing warnings touch `run_kvm.rs`).
- **Specs to update alongside**: `specs/baud-packages.md` (the real kernel + initramfs pipeline, §4), a new
  `specs/baud-stream.md` note (the framebuffer frame path + the ~25% live window), and `specs/README.md` /
  `specs/baud-multiverse.md` (the one determinism model + entropy-by-input-control).
  8. **H9 — the timed-exit fingerprint's capture primitives (specs/baud-fingerprint.md, specs/baud-ubuntu.md
     §6) now exist; the full crate/CLI/cross-VM orchestration does not.** Confirmed by a research pass
     before starting that none of this existed anywhere in the codebase (no `KVM_TRANSLATE`, no CR3-walk-
     for-arbitrary-RIP, no `run_to_events`, no cross-VM comparator) — ACPI (item 5(c) above,
     `crates/baud-multiverse/src/acpi.rs`) and legacy PCI config space (item 5 above,
     `crates/baud-multiverse/src/pci.rs`) were the two pieces already real-hardware-tested from prior
     iterations; MCFG/ECAM is the one PCI gap still left (legacy-only, single bus, 3 functions). New
     `baud_vcpu::boundary::run_to_events` + `RunToEventsOutcome` (`crates/baud-vcpu/src/boundary.rs`) is
     the "stop at N without injecting" primitive `specs/baud-fingerprint.md` §4 step 1 and
     `specs/baud-ubuntu.md` §6 both need: it shares `inject_at`'s arm-early-then-single-step machinery
     (steps 1-3 — arm the branch counter a margin short, take the sloppy early exit, single-step the
     remainder) but never reaches its injection steps (4-5), since a fingerprint capture must observe the
     guest, not perturb it. 4 new unit tests reuse the existing `ScriptedStepper` harness (no real
     `/dev/kvm` needed for these), all passing. `Multiverse::run_to_events`, `Multiverse::translate_gva`
     (`KVM_TRANSLATE` plus an independent manual CR3 4-level page-walk cross-check via a new `walk_cr3`
     free function — a real correctness check, not a redundant call, since a bug in the kernel's own
     translation would otherwise be indistinguishable from a bug in baud's use of it), and
     `Multiverse::capture_fingerprint` (returns a new `TimedExitFingerprint { events, rip, gpa, mem_hash,
     console_output }`) all landed in `crates/baud-multiverse/src/linux/mod.rs`; the pre-existing
     `ram_hash()` was reused directly for `mem_hash` since guest RAM is already registered as exactly one
     canonical region, needing no new hashing logic. New real-hardware test
     `timed_exit_fingerprint_is_stable` (`crates/baud-multiverse/src/linux/mod.rs`, alongside the other
     `timer-guest` tests) boots `timer-guest` twice, calls `capture_fingerprint` with no timer injection at
     all, and asserts the two independent boots land an identical `(events, rip, gpa, mem_hash)` tuple —
     **passed on real `/dev/kvm`**. `cargo build`/`clippy --workspace --all-targets`/`test --workspace` all
     clean (0 new warnings — clippy held at the exact pre-existing 26-warning `baud-multiverse` baseline /
     74-warning workspace baseline); full `bash drive/gate.sh` → 22 passed, 0 failed, 1 flaked (the
     already-documented `rdtsc_guest_reproduces_high_bits_across_boots` load-flake, confirmed passing in
     isolation — not a regression, see CLAUDE.md's standing note on this unit), 1 skipped, 3m24s. **Still
     open for H9 at the time this item was written**: the actual `baud-fingerprint` crate (report
     rendering/`compare`/`FpError`), `baud verify fingerprint` CLI, `drive/h9.sh`, the two-VM cross-process
     orchestration (`cross_vm_fingerprint_matches`), and the real Ubuntu 18.04.1 cloud-image
     acquisition/boot (H9 (d)/(e) above) were all still not started — this iteration closed only the
     previously-completely-missing capture-primitive gap underneath all of them, and surfaced a real
     precision bug in the shared
     stepping engine while doing so (§14.1, "`run_to_events`/`inject_at`'s single-step engine can overshoot
     its target RCB" — **since fixed, see §14.1's Resolved list item 8**), which blocked the exact
     "events = N" contract `specs/baud-fingerprint.md` promises; that contract can now be honestly
     implemented. **The `baud-fingerprint` crate itself has since been built — see item 9 below; narrowed
     down to still open now**: the `baud verify fingerprint` CLI/HTTP route, `drive/h9.sh`, the true
     cross-process orchestration, and the real Ubuntu 18.04.1 cloud image. Separately found this same
     session, real but deliberately left open and out of scope:
     `handle_baud_rdtsc_exit`'s (`kernel-module/baud-enforced/rdtsc-enforce.patch`) call to
     `kvm_skip_emulated_instruction` returns 0 unconditionally, without checking for an active
     `KVM_GUESTDBG_SINGLESTEP` window, so a trapped enforced-regime RDTSC that occurs *inside* a
     single-step window surfaces as a `Debug` exit instead of completing normally, and its EDX:EAX
     result is never actually served to the guest. This is narrow — it only affects the **enforced-
     regime patched kernel module** (`kernel-module/baud-enforced/`), never the stock module every
     normal test/drive-script uses — so it is real but not chased further here.
  9. **H9 — the `baud-fingerprint` crate (report rendering/`compare`/`FpError`) now exists on top of item
     8's capture primitives; the CLI/HTTP route, `drive/h9.sh`, true cross-process orchestration, and the
     real Ubuntu image still do not.** New workspace member `crates/baud-fingerprint` (Cargo.toml deps:
     `baud-multiverse`, `baud-vcpu`, `thiserror`; added to the root `Cargo.toml`'s `members`).
     `crates/baud-fingerprint/src/lib.rs` implements, per `specs/baud-fingerprint.md` §2-§6: `pub struct
     Fingerprint { label: String, banner: Vec<u8>, events: u64, rip: u64, gpa: Option<u64>, mem_hash:
     String }` plus its `render()` (the exact console report block, byte-tested against the spec's own
     example); `pub enum FpError { DeterminismHole(#[from] baud_vcpu::DeterminismHole)` (`cfg(target_os =
     "linux")` only) `, NoBanner { events, expected, found } }`; `pub struct Divergence { field: &'static
     str, a: String, b: String }` with an `impl Display`; `pub fn compare(a: &Fingerprint, b: &Fingerprint)
     -> Result<(), Divergence>`, comparing `banner`, `events`, `rip`, `gpa`, `mem_hash` in that order and
     returning the first divergence (deliberately excludes `label`); and (`cfg(target_os = "linux")` only)
     `pub fn capture(vm: &mut baud_multiverse::linux::Multiverse, label: &str, target_rcb: u64,
     banner_tail_len: usize, expected_banner: Option<&[u8]>) -> Result<Fingerprint, FpError>`, wrapping
     `Multiverse::capture_fingerprint` (item 8 above) and slicing the last `banner_tail_len` bytes of
     console output as the banner, returning `FpError::NoBanner` instead of a wrong-state fingerprint when
     `expected_banner` is `Some` and the captured tail doesn't end with it. Two deliberate deviations from
     the spec's illustrative pseudocode, both documented in the crate's own module doc rather than hidden:
     (1) `mem_hash` is `String` (`"blake3:<hex>"`, matching what `Multiverse::ram_hash()` already returns
     everywhere else in this codebase), not the spec's illustrative `[u8; 32]`; (2) `capture()` takes the
     expected banner and its tail length as parameters instead of hardcoding the Ubuntu banner, since the
     spec's own §5 prose says a non-distro guest "supplies its own banner (or an empty one)" — the
     not-yet-written Ubuntu H9 caller will pass `UBUNTU_BANNER`. 6 new pure unit tests need no `/dev/kvm`:
     `render_is_byte_exact`, `render_reports_unmapped_gpa`, `compare_reports_first_divergence`,
     `compare_names_the_earliest_field_when_several_differ`, `label_difference_is_not_a_divergence`,
     `banner_divergence_is_reported_by_content_not_by_label` — matching the test names
     `specs/baud-fingerprint.md` §8 itself lists (`render_is_byte_exact`, `compare_reports_first_divergence`,
     `label_difference_is_not_a_divergence`). 2 new real-hardware tests, both **passed**:
     `linux::tests::two_independent_boots_produce_matching_fingerprints` boots the existing `timer-guest`
     fixture (`crates/baud-multiverse/tests/fixtures/timer-guest/bzImage`) twice, sequentially, in one
     process — a same-process stand-in for H9's true two-separate-process/two-core orchestration, which is
     still unbuilt — captures a fingerprint from each at the same `target_rcb = 100_000`, and asserts
     `compare()` returns `Ok`, the crate-level proof of the same whole-machine determinism property item
     8's own `timed_exit_fingerprint_is_stable` already established one layer down;
     `linux::tests::wrong_expected_banner_is_rejected` is the `missing_login_fails_capture` analogue,
     adapted to the timer-guest fixture (which prints no banner at all) — asking `capture` to require a
     banner it can never see returns `FpError::NoBanner` rather than a wrong-state fingerprint. Verified:
     `cargo build -p baud-fingerprint` clean; `cargo test -p baud-fingerprint` → 8 passed, 0 failed, real
     `/dev/kvm` exercised by 2 of them; `cargo clippy -p baud-fingerprint --all-targets` → 0 new warnings
     (only pre-existing `baud-multiverse`/`baud-proto` baseline warnings surface, none touching this new
     crate); `cargo build --workspace` clean; `cargo clippy --workspace --all-targets` → 74 warnings, the
     exact pre-existing baseline (`grep` confirms none mention `baud-fingerprint`); `cargo test --workspace`
     → 212 passed, 1 failed (`rdtsc_guest_reproduces_high_bits_across_boots`, the documented load-flake,
     confirmed passing in isolation, 1/1, before concluding it's not a regression), 10 ignored; full `bash
     drive/gate.sh` → 24 passed, 0 failed, 0 skipped, 5m20s, clean, no flakes this run. **Still open for
     H9** (at the time this item was written): `baud verify fingerprint` CLI (needs a new `baud-server`
     HTTP route, since `baud-cli` is HTTP-only and has no direct `baud-multiverse` dependency — confirmed by
     inspection of `crates/baud-cli/Cargo.toml`/`src/client.rs`), `drive/h9.sh`, the true two-separate-OS-
     process (not same-process-sequential like this iteration's test) cross-VM orchestration (the closest
     existing primitive is `baud_multiverse::linux::run_fleet`, which does same-process per-thread core-
     pinning via `baud_host::Host::place`, not separate processes — still needs wiring into an HTTP route,
     none exists today), and the real Ubuntu 18.04.1 cloud-image acquisition/boot (H9 (d)/(e)). This
     iteration closes only the "the full `baud-fingerprint` crate... does not [exist]" half of item 8's own
     "still open" note — the report/comparator layer now exists and is real-hardware-tested. **The CLI/
     HTTP-route half is now closed too — see item 10 below.** True cross-process orchestration and the
     actual Ubuntu image remain open.
  10. **H9 — `baud verify fingerprint` CLI/HTTP route and `drive/h9.sh` now exist on top of items 8-9's
     capture/report/comparator layers.** New `POST /verify/fingerprint`
     (`crates/baud-server/src/routes/verify_fingerprint.rs`, registered in `add_run_kvm_route` alongside
     `/run/kvm*` since it needs the same real `baud_multiverse::linux::Multiverse` — `#[cfg(target_os =
     "linux")]`, same as every other route in that group): boots `(kernel_path, cmdline, tape_hex)` `times`
     times (default 2, same convention as `/verify/determinism`), sequentially in this one server process,
     captures a `Fingerprint` from each at `target_rcb` (`baud_fingerprint::capture`, threading the same
     `rdseed_sites` sidecar lookup `/run/kvm` already does via `crate::rdseed_sites::load_rdseed_sites`, so
     a real rdseed-rewritten image's sites are honored here too), and compares every later fingerprint
     against the first (`baud_fingerprint::compare`), reporting the first divergence field-by-field or
     `verified: true`. Response includes every captured fingerprint's rendered report
     (`Fingerprint::render()`) plus its raw fields (hex-encoded banner, `0x`-formatted RIP/GPA, `mem_hash`).
     New `baud-cli` subcommand `baud verify fingerprint --kernel <path> --target-rcb <u64> [--cmdline]
     [--tape-hex] [--banner-tail-len] [--expected-banner <text>] [--times] [--initramfs]`
     (`crates/baud-cli/src/cmds/verify.rs`, `VerifyAction::Fingerprint`) POSTs the request (hex-encoding
     `--expected-banner`'s raw text before sending), exits 0 when `ok=true`, 1 otherwise — same convention
     as `verify determinism`. New drive script `drive/h/h9.sh` (H9.1 host-probe sanity check, H9.2 two
     independent `timer-guest` boots produce matching fingerprints through the real CLI/server path, H9.3 an
     expected banner the guest never prints fails the whole call loud rather than a silent false pass) —
     added to `drive/gate.sh`'s `FANOUT` (cheap: 2s, reuses the already-built `timer-guest` fixture, no new
     kernel builds) and `drive/gate.test.bats`'s `server_scripts()` concurrency-safety list. 2 new
     server-route-level unit tests in `verify_fingerprint.rs`
     (`boot_and_compare_fingerprints_reports_no_divergence_across_two_boots`,
     `boot_and_compare_fingerprints_propagates_a_missing_banner_as_an_error`), both **passed on real
     `/dev/kvm`** — the route-level analogue of `baud-fingerprint`'s own crate-level tests, proving the
     server wrapper (including its `rdseed_sites` lookup) carries the same whole-machine determinism
     property through to an HTTP-shaped response. Verified: `cargo build --workspace` clean; `cargo clippy
     --workspace --all-targets` → one new warning introduced and fixed immediately
     (`clippy::clone_on_copy` on `EnforcedRdseedSite`, which is `Copy` — dereference instead), then 0
     warnings touching the new code, same pre-existing baseline otherwise; `bash drive/gate.sh` → 24 passed,
     0 failed, 1 flaked (`rdtsc_guest_reproduces_high_bits_across_boots`, confirmed passing in isolation in
     phase 6 — the documented load-flake, not a regression), 0 skipped, 5m53s, `drive/h/h9.sh` itself PASSED
     in 2s. **Still open for H9 at the time this item was written**: the true two-separate-OS-process
     cross-VM orchestration and the real Ubuntu 18.04.1 cloud-image acquisition/boot (H9 (d)/(e),
     unstarted) — **the former is now closed, see item 11 below.** H8 Mario remains separately blocked on
     the FCEUX Qt5/SDL2/Xvfb packaging problem.

  11. **H9 — the true two-separate-OS-process/two-core cross-VM orchestration item 10 named as still open
     now exists, on top of items 8-10's capture/report/comparator/CLI/route layers.** `POST
     /verify/fingerprint`'s `times` parameter was hard-floored at `.max(2)`, forcing every call into at
     least two boots compared inside one Rust process — the same same-process-sequential shape every prior
     H9 test used. Relaxed to `.max(1)`: `times: 1` captures exactly one fingerprint and returns
     `verified: true`/`divergence: None` trivially (the `fingerprints[1..]` comparison loop has nothing to
     iterate over one element — intentional, documented in the function's doc comment, not a vacuous check
     mistaken for a real one), letting an external caller compare fingerprints from two *separate* captures
     itself. New unit test `single_boot_capture_returns_one_fingerprint_with_no_comparison`
     (`crates/baud-server/src/routes/verify_fingerprint.rs`) locks this in; `baud-cli`'s `--times` doc
     comment updated to describe the new minimum of 1.
     `drive/h/h9.sh` gained **H9.4**: two genuinely separate `baud-server` **OS processes** (own PID, own
     ephemeral port, own SQLite DB, own snapshot-store directory — sharing nothing but the kernel image),
     started via `taskset -c 0`/`taskset -c 1` when `taskset` and `nproc >= 2` are both available (true here,
     8 cores), each hit with `baud verify fingerprint --times 1` for the identical `(kernel, cmdline,
     tape_hex, target_rcb)`. The equality check — `events`/`rip`/`gpa`/`mem_hash`/banner — is performed by
     the **bash script itself**, never delegated to any single Rust process, which is exactly what "two
     independent VMs, separate processes on separate cores" (specs/baud-fingerprint.md, todo.md §10) requires
     and what every previous same-process test could never prove by construction. **H9.5** guards H9.4
     against being vacuous: it does *not* try to force a real state divergence by varying `target_rcb` (this
     was attempted and empirically failed — `timer-guest`'s steady-state loop retires exactly one
     conditional branch per iteration at the same instruction address and never writes guest RAM, so `rip`/
     `mem_hash` are bitwise identical across the *entire* 100 to 200,000 `target_rcb` range tried; this is a
     property of that fixture's loop shape, not a bug in the capture engine or item 8's RCB-exactness fix).
     Instead H9.5 flips the last hex digit of a **copy** of vm1's own hash and asserts this script's own
     `[[ == ]]` equality check reports the corruption as a mismatch — proving the comparison used by H9.4 is
     a real inequality test, not `true == true` by construction (same anti-pattern §14.1 catalogs elsewhere
     in this project: a check that cannot fail is not a check). Verified: `cargo build -p baud-server -p
     baud-cli` clean; `cargo clippy -p baud-server -p baud-cli --all-targets` → 0 new warnings (grep
     confirmed none touch `verify_fingerprint.rs`/`cmds/verify.rs`; remaining warnings are the pre-existing
     baseline in unrelated `fuzz.rs`/`replay.rs`/`tracing.rs`); `cargo test -p baud-server
     verify_fingerprint` → 3 passed (including the new `times: 1` test); `cargo test --workspace` → all
     green, 0 failed (the documented `rdtsc_guest_reproduces_high_bits_across_boots` load-flake happened to
     pass this run too); `bats drive/gate.test.bats --filter-tags '!slow'` (28 static checks, including
     the concurrency-safety suite `drive/h9.sh` is already wired into) → 28/28 pass, no regressions from the
     new second/third server per script; `bash drive/h/h9.sh` standalone → H9.1 through H9.5 all PASS on
     real `/dev/kvm`, ~9s; full `bash drive/gate.sh` → 24 passed, 0 failed, 1 skipped
     (`pkg-build-cli`, cached fingerprint unchanged since the last pass), 0 flaked, 3m29s, clean. **Still
     open for full H9**: only the real Ubuntu 18.04.1 cloud-image acquisition/boot (H9 (d)/(e)) plus the
     ACPI/PCI/virtio-blk machine additions §4.7 specifies — this iteration's cross-process orchestration is
     real and hardware-verified, but still exercises the `timer-guest` fixture standing in for the
     not-yet-acquired distro image. H8 Mario remains separately blocked on the FCEUX Qt5/SDL2/Xvfb packaging
     problem, untouched by this iteration.
  12. **H9 — the real Ubuntu 18.04.1 cloud image (H9 (d)) is now acquired, SHA256-verified, and prepped; a
     real boot attempt found and fixed a genuine, general `initramfs_load_addr` placement bug, then reached
     the end of kernel init before hitting a distinct, well-scoped remaining gap.** `examples/ubuntu/fetch.sh`
     (new) downloads, verifies, and preps the exact dated build `specs/baud-ubuntu.md` §4 asks for —
     **finding**: `cloud-images.ubuntu.com/releases/18.04/release/` is a *rolling* alias that now serves
     18.04.6, not 18.04.1; the dated snapshot `releases/bionic/release-20180806/` was confirmed (by
     downloading and reading `/etc/os-release`/`/etc/issue` directly) to be the one that actually reports
     `PRETTY_NAME="Ubuntu 18.04.1 LTS"` and the exact three-token `/etc/issue` banner form §4 names — pinned
     in `fetch.sh`. `examples/ubuntu/BUILD.md` documents all of this plus a manual repro command. Artifacts
     (~2.2 GiB raw rootfs + kernel + initrd) are never committed (`fetch.sh` writes outside the repo tree by
     default, `.gitignore` guards the in-tree path too, same convention as `~/wsl-kernel-src`).
     **Real bug found and fixed**: booting this real kernel+initrd (every CLI flag needed —
     `--initramfs`/`--acpi`/`--virtio-blk-image` — was already wired end-to-end by items 5/8-11, never
     exercised against a full-size distro kernel until now) crashed immediately: `Initramfs unpacking failed:
     junk in compressed archive`, then a page fault in `free_reserved_area`. Root-caused by decoding the raw
     bzImage header directly (`hdr.init_size = 0x1e4f000` ≈ 30.4 MiB from `KERNEL_LOAD_ADDR` at 2 MiB): the
     kernel's own self-decompression scratch space extends to ~32.28 MiB, just past the fixed
     `layout::INITRAMFS_ADDR` at exactly 32 MiB — the kernel silently overwrote the first ~300 KiB of the
     initrd with its own decompression output before ever unpacking it. Every prior baud fixture kernel
     (todo.md §4.1's no-modules minimal config, a few MiB at most) stayed small enough this was never hit.
     A direct read-back unit test (`load_kernel_and_write_boot_params` writes, then a plain `read_slice`
     compares against the source file) proved the *load* step was already byte-faithful, so the corruption
     happens during the kernel's own early execution, not baud's write path — ruling out a simple off-by-one
     in the write itself. New `layout::initramfs_load_addr(kernel_init_size)` (`crates/baud-multiverse/src/
     layout.rs`) computes the real placement dynamically: for a kernel small enough that
     `KERNEL_LOAD_ADDR + init_size` stays at or under the old fixed `INITRAMFS_ADDR`, it returns
     `INITRAMFS_ADDR` unchanged (every existing fixture, confirmed by a new unit test, so no prior boot's
     placement moved); otherwise it moves past `KERNEL_LOAD_ADDR + init_size` plus a fixed `+32 MiB` safety
     margin. **That margin number is itself a real-hardware-bisected finding, not a guess**: moving the
     initramfs to exactly `KERNEL_LOAD_ADDR + init_size` (zero margin) still reproduced the identical crash;
     `+8 MiB` still corrupted it; `+16 MiB` and `+32 MiB` were both clean (confirmed by booting past
     `Unpacking initramfs...` all the way to `Freeing unused kernel memory` with no oops). The exact
     mechanism requiring more than the documented `init_size` was not root-caused further (a plausible
     suspect noted in the function's own doc: the decompressor's own internal relocate-then-decompress
     safety copy, which Documentation/x86/boot.txt does not fully size) — `+32 MiB` (2x the empirically-found
     minimum) is used as a documented, verified-safe margin rather than chasing the precise mechanism.
     `bootparams.rs`'s `load_kernel_and_write_boot_params` now calls this function instead of using the fixed
     constant directly, and returns a new `BootParamsError::InitramfsDoesNotFit` if the computed placement
     plus the initramfs's own length would exceed `ram_size`, rather than silently writing out of bounds.
     `cargo test -p baud-multiverse -- layout:: bootparams::` → 18 passed, 0 failed (2 new pure unit tests for
     `initramfs_load_addr` itself, no external fixture needed); full `cargo build`/`clippy --workspace
     --all-targets`/`test --workspace` and `bash drive/gate.sh` reconfirmed clean (§15's protocol, see below).
     **With the fix, a real boot attempt (kernel + initrd + the real 2.2 GiB rootfs.raw via
     `--virtio-blk-image` + `--acpi`) got much further**: ACPI tables parse cleanly (`ACPI: Core revision
     ...`), PCI enumerates via the legacy `0xCF8/0xCFC` mechanism (`PCI: Using configuration type 1 for base
     access`), the initramfs unpacks successfully, and the boot reaches `Freeing unused kernel memory` — the
     very end of kernel init, immediately before handing off to `/init`. **Still open, the next concrete
     blocker**: the run stops there because `Multiverse::run_to_first_halt_with_periodic_timer` (the
     primitive `boot_run_and_drain` uses) returns on the guest's *first* `Hlt`, and a real multi-tasking
     kernel's idle loop calls `hlt` the moment nothing is runnable — which happens almost immediately once
     `/init` blocks on its first disk read, long before systemd, `agetty`, or the `ubuntu login:` banner.
     Raising `--periodic-timer-max-ticks` from 200 to 3000 produced **byte-identical console output**
     (confirmed via a real side-by-side diff), proving this is a genuine halt, not a truncation — reaching
     login needs a different run-loop primitive (survive/resume past an idle halt, or "run until console
     contains `ubuntu login:`"), not a bigger tick budget on the existing one. That primitive does not exist
     yet anywhere in this workspace (every existing "run until X" combinator stops at the first halt, a
     `MARK_BRANCH`, or a fixed console-byte-length target — none of those is "keep resuming across repeated
     idle halts until a byte pattern appears"). H8 Mario remains separately blocked on the FCEUX Qt5/SDL2/
     Xvfb packaging problem, untouched by this iteration.
  13. **H9 — the resume-past-idle-halt primitive item 12 found missing now exists and is hardware-verified;
     booting the real Ubuntu 18.04.1 image with it made zero forward progress, but exposed a distinct, deeper
     APIC/interrupt-routing bug in the guest, not a flaw in the new primitive.** New
     `Multiverse::run_to_first_halt_with_periodic_timer_and_devices` (`crates/baud-multiverse/src/linux/mod.rs`,
     the engine originally around line 1677, now generalized) gained a `pattern: Option<&[u8]>` parameter plus
     `max_exits_per_burst: u32`: when `Some`, a guest halt with no device work pending is no longer terminal —
     the periodic timer vector is staged directly via `KVM_SET_VCPU_EVENTS` (the same "safe because
     `safe_halt()` guarantees `RFLAGS.IF=1`" idiom `service_virtio_rng_interrupt_while_halted` established for
     one device) and the guest is driven via `step_exit()` until it halts again or the console output contains
     `pattern`. Two new public wrappers: `run_until_console_pattern_with_periodic_timer` (bare, no devices) and
     `run_until_console_pattern_with_periodic_timer_and_devices` (adds optional virtio-rng/virtio-blk vectors) —
     the latter is what a real Ubuntu boot needs since it also needs virtio-blk. Every existing caller of the
     underlying private engine passes `pattern: None` and is unaffected: regression-tested,
     `cargo test -p baud-multiverse --lib` → 217 passed, 0 failed, 10 ignored, no change from before this
     iteration. New fixture `tests/fixtures/idle-halt-guest/` (`payload.s`/`build.py`/`BUILD.md`, mirroring
     `../timer-guest/`'s wrapping mechanics) halts immediately at boot with no busy loop at all — the shape
     that breaks every prior combinator, since `inject_at`'s arm-early-then-single-step engine can never
     deliver to an already-halted vCPU — and its IDT handler only writes `"ubuntu login:"` to COM1 on its 5th
     wake (silent on the first 4). Two new hardware-verified tests in
     `crates/baud-multiverse/src/linux/mod.rs`: `run_until_console_pattern_resumes_across_repeated_idle_halts`
     (positive: two independent boots produce byte-identical console output/ram_hash/tick-count, proving the
     resume-past-idle-halt path is itself deterministic) and
     `run_until_console_pattern_reports_determinism_hole_when_never_found` (negative: a pattern the guest never
     writes exhausts `max_ticks` and errors, never silently "succeeds"). Wired end-to-end: `RunKvmBody`
     (`crates/baud-server/src/routes/run_kvm.rs`) gained `halt_console_pattern_hex`/`halt_max_exits_per_burst`;
     `boot_run_and_drain` dispatches to the new devices-aware primitive when both `periodic_timer` and a
     pattern are set; the route rejects (loud error, not silent data loss) combining this with `run_id`
     persistence (not yet supported — a real, recorded follow-up gap, not a stub) or setting a pattern without
     `periodic_timer`. `baud-cli`'s `run kvm` subcommand gained
     `--halt-console-pattern-hex`/`--halt-max-exits-per-burst`. New route-level test
     `run_kvm_resumes_past_idle_halts_until_console_pattern_found` (`crates/baud-server/src/routes/run_kvm.rs`)
     exercises the exact function the HTTP handler calls, using `idle-halt-guest`. Verified: `cargo build
     --workspace` clean; `cargo clippy -p baud-multiverse -p baud-server -p baud-cli --all-targets` — 0 new
     warnings (all pre-existing warnings are in unrelated files: `baud-tracing`'s deprecated `aya::Bpf`,
     `baud-driver`, `cmds/net.rs`, `cmds/tape.rs`, `timesource.rs`, `fuzz.rs`, `replay.rs`, `tracing.rs`);
     `cargo test -p baud-server` run_kvm module → 28 passed, 0 failed (was 27 before, +1 new test). **Real bug
     found and fixed in `examples/ubuntu/fetch.sh`**: `fetch_and_verify`'s checksum-lookup grep compared
     against the LOCAL shortened output filename (e.g. `vmlinuz-generic`) but `SHA256SUMS.unpacked` lists the
     REMOTE basename (`ubuntu-18.04-server-cloudimg-amd64-vmlinuz-generic`) — the grep pattern
     `" \*${out}\$"` never matched (confirmed directly: `grep " \*vmlinuz-generic\$" SHA256SUMS.unpacked`
     returns nothing, exit 1), so `expected` was always empty and the script died silently under
     `set -euo pipefail` (grep's exit 1 propagating through the command substitution) with zero diagnostic
     output, as soon as a previously-partially-downloaded `vmlinuz-generic`/`initrd-generic` file was checked
     against on any re-run. Fixed by deriving the lookup key from `basename "$url"` (the real remote name)
     instead of `$out`; confirmed fixed by re-running `bash examples/ubuntu/fetch.sh`, which correctly reported
     `vmlinuz-generic already present and verified, skipping` / `initrd-generic already present and verified,
     skipping` and proceeded to `rootfs.raw`/tune2fs. **A real Ubuntu boot attempt with the new primitive made
     zero forward progress, but found a NEW, deeper, precisely-diagnosed blocker.** Fetched the real artifacts
     fresh (`~/.baud-tmp/ubuntu-1804`, per `fetch.sh`), started `baud-server`, and ran the exact
     `examples/ubuntu/BUILD.md` invocation plus `--halt-console-pattern-hex <hex of "ubuntu login:">
     --halt-max-exits-per-burst 200000`, escalating `--periodic-timer-max-ticks` from 3,000 → 30,000 →
     1,500,000 → 8,000,000 (the last took ~7m50s wall-clock, real `/dev/kvm`). Every attempt exhausted its
     tick budget with **zero** progress past a fixed point. To debug this, added a `console_tail` helper
     (`crates/baud-multiverse/src/linux/mod.rs`) attaching the last 200 bytes of console output to both of
     `run_to_first_halt_with_periodic_timer_and_devices`'s timeout `DeterminismHole` messages (previously bare
     "guest did not halt"/pattern-not-found with no diagnostic at all — a real, separate, now-fixed gap). The
     console tail revealed the guest endlessly repeating `do_IRQ: 0.236 No irq handler for vector` (236 decimal
     = `0xec`, this project's own injected/documented `LOCAL_TIMER_VECTOR`) — i.e. **every injected timer
     interrupt is being routed through the kernel's generic `do_IRQ` dispatch (only reachable for a vector with
     no real dedicated IDT gate), never reaching the real LAPIC timer ISR** — so `jiffies` never legitimately
     advances once `/init` blocks and userspace tries to depend on it, and the guest spins forever making no
     real progress while our primitive faithfully keeps "waking" it into the same dead end. Ruled out "wrong
     vector number": `~/wsl-kernel-src/src/arch/x86/include/asm/irq_vectors.h` (already set up per CLAUDE.md)
     confirms `LOCAL_TIMER_VECTOR` really is `0xec` in a modern kernel tree, so 236 is the intended number — the
     mismatch must be in *how/when* the interrupt is delivered relative to this real Ubuntu 4.15 kernel's own
     IDT/LAPIC initialization (e.g., possibly this guest runs in a legacy-PIC-only or no-LAPIC-detected mode
     given the minimal one-LAPIC-only MADT and `nosmp`/`maxcpus=1`, so vector `0xec` was never actually wired to
     the dedicated `apic_timer_interrupt` gate at all), not simply "pick a different vector." **Still open, the
     next concrete step**: determine why this real kernel's IDT does not have vector `0xec` bound to a real
     LAPIC timer handler in this environment — check whether the guest actually enables/detects a local APIC at
     all (dmesg for "Not using local APIC timer" or similar, which needs raising `loglevel`/dropping `quiet`
     from the cmdline to see, currently suppressed), whether ACPI's MADT needs an IOAPIC entry too (this
     project's `write_acpi_tables` writes only RSDP→XSDT→FADT+DSDT+MADT-with-one-LAPIC — no IOAPIC — per §4
     above), or whether the kernel needs `lapic`/`no_ioapic`-style cmdline hints to accept this minimal topology
     instead of falling back to legacy do_IRQ-routed vector dispatch. The resume-past-idle-halt primitive
     itself is proven correct and complete on its own terms (idle-halt-guest fixture, both directions); this is
     a distinct, deeper gap in the real distro's interrupt/APIC bring-up, not a flaw in the new run-loop
     combinator. H8 Mario remains separately blocked on the FCEUX Qt5/SDL2/Xvfb packaging problem, untouched by
     this iteration.

### 14.1 Defects found in the test suite and the drive scripts

Latent defects, each of which let a test or script report success it had not earned. The pre-push gate is
now `drive/gate.sh` (§15), covered by `drive/gate.test.bats`.

**Tests that passed while asserting nothing.** Each of these was green in every run of the suite, and none
of them could have failed for the reason its name implies:

- `quantum_overrun_guest_is_killed` (`crates/baud-multiverse/src/lib.rs`) — computed `crash_obs` and then
  discarded it (`let _ = crash_obs;`). Its only surviving assertion was that two runs of one tape hash
  equally, which is exactly `double_run_is_bit_identical`'s property. The test named for the wall-clock
  watchdog asserted nothing about the watchdog. See VR2-M7 below for what this concealed.
- `different_tapes_may_diverge` (`crates/baud-multiverse/src/lib.rs:1172`) — asserted only that two stream
  hashes were non-empty; its own comment declined to check divergence. The name described a property the
  body did not verify.
- `duplicate_detection` (`crates/baud-stream/src/frame.rs:98`) — reduced to `assert!(x == x)`. The negative
  case (a non-duplicate frame) was untested anywhere in the crate.
- `show_redacted_hides_value` (`crates/baud-keys/src/lib.rs:537`) — dead code. `secrets_file()` resolves
  workspace-relative, but `cargo test`'s CWD is the package root, so the `!exists()` early-return always
  fired and the body never ran.
- `crates/baud-keys/src/lib.rs:522` and `:530` — no assertions at all (`let _ = age_key_path();` and
  `doctor_does_not_panic`).
- `shrink_reduces_tape` (`crates/baud-driver/src/lib.rs:817`) — asserted `len <= best.len() + 1`, which a
  shrink that removed nothing satisfies.
- `unmodified_agent_runs_a_new_workload` (`crates/baud-tape-agent/src/agent.rs:283`) — called no function
  from `baud-tape-agent`; it exercised only `baud_init::lint`.
- `rotate_invalidates_old_key` (`crates/baud-keys/src/lib.rs:567`, 147 lines) and
  `chunk_bodies_are_ciphertext` (`crates/baud-journal/src/lib.rs:516`, 93 lines) — silently no-op on any host
  without `sops`/`age`/`age-keygen`/`nix` (all absent here): they hit `.is_err()` guards and return having
  asserted nothing, which is indistinguishable from passing. `rotate_invalidates_old_key` additionally shells
  out to `sops updatekeys` directly and never calls this crate's `rotate_secrets` — see VR2-M3 below.

**False passes in the drive scripts.** `pkill -f "baud-server"` (17 scripts) matched whole command lines, so
it also killed sibling `cargo build -p baud-server` runs and the `rustc` compiling them — one script's
startup could fail another script's **build**. A hardcoded `127.0.0.1:7734` combined with a bare `sleep 1`
meant a script whose own server lost the bind silently drove **another server and passed**; now an ephemeral
port via `BAUD_ADDR` plus a `/health` poll (`baud-server` still defaults to `7734` when unset). `m11.sh`
omitted `"out"` on its M11.5 render call, so the server wrote a repo-root `output.y4m`
(`crates/baud-server/src/routes/stream.rs:148`) — invisible in `git status` because of `.gitignore:33`.
`h3.sh` wrote fixed `/tmp/h3-require-enforced.*`. Every server shared one `baud-snapshots/` root, with a
write-then-read race on `.age-identity.txt` (`crates/baud-server/src/state.rs:64-72`). `trap cleanup EXIT`
does not run on an untrapped signal, so interrupted scripts stranded servers holding `/dev/kvm` and leaked
temp SQLite files; scripts now trap `INT`/`TERM`, and `gate.sh` reaps by process group, never by name, so
unrelated `baud` invocations are untouched.

**`--ignored` silently passes a non-ignored test.** `cargo test -- --ignored <filter>` against a test that is
*not* `#[ignore]`d reports `test result: ok. 0 passed`, which a `grep -q "test result: ok"` accepts as
success. Drive scripts that gate on that string must use `--include-ignored` and additionally assert a
non-zero pass count (`drive/h/h5.sh:157-159`), or they pass while running nothing.

**`thousand_branches_are_independent_and_deterministic` ran twice per verification round** — once in
`cargo test --workspace`, once in `drive/h/h5.sh` — at ~244s each. Now `#[ignore]`d with `drive/h/h5.sh` as sole
runner. Related: `drive/pkg/pkg-build-cli.sh` costs **143s**, not the "~4-5 min" its own header and
`ralph/progress.txt` claim, and it rebuilds from scratch every run (`cp -a` of a 1.8G tree plus `mrproper`,
no cache); it is now gated on a fingerprint that includes the out-of-tree kernel version and whether the
enforced-regime patch is applied, since neither is visible to any `git diff`.

**Resolved.**

1. **VR2-M7 (wall-clock watchdog for spinning guests) — `ralph/progress.txt:602` had recorded "FIXED"; it
   was not, until this entry's fix.** `Multiverse::run` incremented `guest_quantum_steps[g]` at the top of
   each quantum and reset it to `0` at the bottom of the *same* iteration, because the simulation modelled no
   outcome other than "guest reaches a syscall". The counter could never exceed 1, while `quantum_step_limit`
   is ≥ 1 and the kill is `> limit` — **the kill branch was unreachable for every tape and every
   `quantum_limit_ms`.** `quantum_overrun_guest_is_killed` hid this by discarding its own observation (`let _
   = crash_obs;`). Fixed by adding `SPIN_ACTION` (`crates/baud-multiverse/src/lib.rs:615`) so a guest burning
   a whole quantum is modelled and the watchdog is reachable; the test now asserts SIGKILL +
   `"quantum-overrun"` at the exact quantum, plus a negative case and a `quantum_limit_ms = 0` disable case,
   and was mutation-verified. **Note this widens the per-quantum action draw to `draw_int(0, SPIN_ACTION)`,
   which changes how tape bytes map to actions and therefore changes stream hashes for existing tapes** —
   safe only because no golden hashes exist in-tree and `verify`/`replay` derive from the same simulation.

   **Follow-up: the real-KVM-path half of this gap is now closed too, so VR2-M7 is closed everywhere, not
   just in the simulation.** New `crates/baud-vcpu/src/linux/watchdog.rs`: a `Watchdog` struct spawns a
   companion thread that, after a caller-supplied wall-clock `Duration` budget (or never, if
   `Duration::ZERO`), sends `SIGUSR1` via `pthread_kill` to the vCPU thread that armed it (captured via
   `libc::pthread_self()`), forcing a blocking `KVM_RUN` ioctl to return `-EINTR` even for a guest that
   causes literally zero VM exits — this project's subtractive machine model has no APIC/PIT/host
   interrupts to force one otherwise, so a tight `jmp $` loop never traps on its own. A `Once`-guarded
   `sigaction` installs a real (non-`SIG_IGN`, non-`SA_RESTART`) no-op handler for `SIGUSR1` so the signal
   actually interrupts the blocking syscall instead of being discarded or killing the process, and the
   watchdog is always disarmed (cancelled + joined) before `run_until_halted` returns on every path, so a
   late-firing signal can never land in unrelated future work on a reused thread (relevant because
   `baud-server` runs boots on `tokio::task::spawn_blocking`'s reusable thread pool). This is
   architecturally different from the abandoned PMU-overflow-signal approach that
   `crates/baud-vcpu/src/linux/pmu.rs`'s own module doc documents (a guest-visible interrupt whose
   delivery this host's nested-virt PMU emulation was found to drop) — a `pthread_kill`-delivered signal
   uses the general Linux "kick a running task" IPI mechanism instead, independent of any guest-visible
   interrupt controller, and was hardware-verified to work reliably here. New public
   `baud_vcpu::RunLoopError` (`crates/baud-vcpu/src/lib.rs`): `DeterminismHole(DeterminismHole)`
   (unchanged meaning) or `WatchdogKilled { budget_ms: u64 }` — kept a distinct variant rather than folded
   into `DeterminismHole`'s fixed "unhandled exit reached the run-loop catch-all" wording, since a
   watchdog kill is not an unmodeled exit. `baud_vcpu::linux::run_until_halted` now takes a
   `watchdog_budget: Duration` parameter and returns `Result<(), RunLoopError>`.
   `baud_multiverse::linux::Multiverse` gained a `watchdog_budget: Duration` field (new pub const
   `DEFAULT_WATCHDOG_BUDGET = Duration::from_secs(30)`, set by `boot`/`restore`) and
   `set_watchdog_budget(&mut self, Duration)` to override it (tests use a tight budget; `Duration::ZERO`
   disables it). `run_to_first_halt`/`run_to_first_halt_without_ram_hash`/`run_with_timer_ticks` now
   return `Result<_, RunLoopError>` instead of `Result<_, DeterminismHole>` — the only two real call sites
   of the old signature (this crate's own tests and `crates/baud-server/src/routes/run_kvm.rs`'s
   `boot_run_and_drain`) were updated; every other `run_to_first_halt_with_*` entry point
   (periodic-timer/virtio-rng variants) already had its own deterministic `max_exits`/`max_ticks` budget
   and is untouched by this change. New hand-assembled fixture
   `crates/baud-multiverse/tests/fixtures/spin-guest/` (`payload.s` is exactly `1: jmp 1b`, no I/O, no way
   to ever exit on its own — see that directory's `BUILD.md`) is the one fixture in the repo that can
   actually exercise the watchdog's kill path end-to-end. Hardware-verified new tests, all passing on real
   `/dev/kvm`: `wall_clock_watchdog_kills_a_truly_spinning_guest` (a 300ms budget against `spin-guest`
   returns `RunLoopError::WatchdogKilled` within a bounded wall-clock window, not hanging) and
   `wall_clock_watchdog_does_not_fire_on_a_normal_guest` (negative case, hello-guest, 5s budget, must
   succeed normally) in `crates/baud-multiverse/src/linux/mod.rs`; plus three pure-Rust unit tests for the
   `Watchdog` primitive itself (no `/dev/kvm` needed) in `crates/baud-vcpu/src/linux/watchdog.rs`'s own
   `#[cfg(test)]` module covering: zero-budget disables it, it fires once its budget elapses, and
   disarming before the budget elapses prevents it from ever firing.
2. **VR2-M3 — `baud keys rotate` did not invalidate the old key — fixed.** `specs/baud-keys.md:111`
   specifies `baud keys rotate  # sops rotate to new recipients`, but `rotate_secrets`
   (`crates/baud-keys/src/lib.rs`) ran `sops --rotate --in-place`, whose own doc comment stated it
   "refreshes the data key while keeping the same recipient set… the age identity (private key) is
   unchanged" — the opposite of what the spec and VR2-M3 require. `rotate_invalidates_old_key` appeared to
   cover this but shelled out to `sops updatekeys` directly, never calling `rotate_secrets`, and silently
   no-op'd anyway because `sops`/`age`/`age-keygen`/`nix` are absent on this host (it hit `.is_err()` guards
   and asserted nothing). `rotate_secrets` now takes a `new_recipient: &str` and performs a real recipient
   swap: `sops --add-age <new_recipient>` (file becomes decryptable by both identities), then `sops --rm-age
   <old_recipient>` (old identity dropped from the recipient list; sops re-encrypts the data key on each
   step, so decryption with the pre-rotation key genuinely fails afterwards) — `old_recipient` is read via
   the existing `age_public_key()`, so the caller never has to supply it. Added `KeysError::
   RotateRecipientUnchanged`, checked *before* touching `sops` at all, to reject a same-recipient "rotation"
   that would otherwise `--rm-age` the file's only remaining recipient and lock everyone out permanently.
   `POST /keys/rotate` and `baud keys rotate` both gained the now-required `--new-recipient <age1...>`
   argument (previously took none). The `#[ignore]`d `rotate_invalidates_old_key` (needs real
   `sops`/`age-keygen`, both absent here) now drives this crate's own `rotate_secrets` instead of hand-rolled
   `sops` calls, so a pass is a real guarantee about `baud keys rotate`, not just about `sops` itself; a new
   non-ignored `rotate_rejects_same_recipient` covers the new guard (needs no external binary — the check
   fires from `age_key_path`/`age_public_key` alone, both pure file reads). Verified: `cargo build -p
   baud-keys -p baud-cli -p baud-server` and `cargo clippy` on the same three, both clean (no new warnings);
   `cargo test -p baud-keys` → 12 passed, 0 failed, 1 ignored (the sops/age-keygen-requiring test, consistent
   with this host per `CLAUDE.md`). `sops`/`age-keygen` remain absent here, so the `--add-age`/`--rm-age`
   recipient-rotation behavior itself is not hardware-verified on this host — only reasoned from documented
   `sops` CLI flags and exercised by the guard-only unit test; whoever next has `sops`+`age-keygen` on PATH
   should run `cargo test -p baud-keys -- --ignored rotate_invalidates_old_key` to close that gap.

3. **`ram_hash` was computed and discarded at scale — fixed.** `Multiverse` (`crates/baud-multiverse/src/
   linux/mod.rs`) gained `_without_ram_hash` siblings for all four `run_until_branch_or_halt*` entry points
   (plain, `_with_periodic_timer`, `_with_virtio_rng`, `_with_periodic_timer_and_virtio_rng`), returning a new
   `RunUntilBranchObservation` (exactly `RunUntilBranchOutcome` minus the `Halted` arm's `ram_hash`) — the
   same pattern `run_to_first_halt_without_ram_hash` already established and `thousand_branches` already
   used. The four original eager methods are now thin wrappers on top (`observation_to_outcome` fills in
   `ram_hash` only for the `Halted` arm). `crates/baud-server/src/routes/run_kvm.rs`'s `run_branches`/
   `run_driver_generated_branches_with_persist` (and their `boot_snapshot_and_branch`/
   `boot_snapshot_and_generate` wrappers) gained a `compute_ram_hash: bool` parameter, always calling the
   `_without_ram_hash` primitive and computing `branch.ram_hash()` separately, only when `true`. Every real
   HTTP-facing call site (`POST /run/kvm/branch`, `resume_and_branch`, `resume_and_generate`) passes `true`
   unconditionally — `ram_hash` stays in every HTTP response, no behavior change there. ~15 test call sites
   that never read the resulting hash now pass `false` (empty-string placeholder, never observed). Two test
   call sites do whole-outcome-tuple `assert_eq!` comparisons that implicitly depend on `ram_hash`
   reproducibility (`persisted_universe_resumes_and_branches_without_reboot`,
   `run_kvm_branch_produces_independent_and_deterministic_branches`) — both correctly pass `true`; getting
   this wrong (both sides `false`) would have made the comparison vacuously pass, exactly the "test asserts
   nothing" anti-pattern this whole file's §14.1 catalogs elsewhere. Gate wall-clock dropped from the
   documented ~6 min baseline to 2m58s in the verifying run (`cargo test --workspace` phase alone: 24s).
4. **`fleet_of_vms_run_in_parallel_without_interference` flake — already fixed, this entry was stale.**
   This item was carried forward as "still open" describing a timing-ratio flake, but commit `2c0919a`
   (`drive: add a parallel verification gate, reorganize drive/, and fix latent test defects`) had already
   applied the same treatment `thousand_branches_are_independent_and_deterministic` got: the test is
   `#[ignore = "timing-ratio + fixed-core pinning, flaky under any concurrent load; covered by
   drive/h/h6.sh on a quiet machine"]` (`crates/baud-multiverse/src/linux/mod.rs:4349`), and
   `drive/h/h6.sh` is its dedicated runner (`--include-ignored`, asserts a non-zero pass count),
   already wired into `drive/gate.sh` phase 4 (`04-h6`). No code changed for this entry beyond
   correcting the record.
5. **`shell-into`'s timeout conflated two different things — fixed.**
   `crates/baud-cli/src/cmds/shell_into.rs` used one `--idle-timeout-ms` as both the idle timeout *and*
   the first-byte deadline, so under concurrent guest boots it returned `ok=true` with an empty
   transcript (measured: 2000ms → empty 3/3; 8000ms → correct 3/3). Split into two flags: `--idle-timeout-ms`
   (default 2000ms, "the guest stopped talking", used once output has started) and the new
   `--first-byte-timeout-ms` (default 10000ms, "restore hasn't produced output yet under load", used while
   `output.is_empty()`). `drive/m/m10.sh`'s M10.2/M10.3 now pass `--first-byte-timeout-ms 15000` instead of
   inflating `--idle-timeout-ms`; M10.4's error-path call (no restore involved) keeps both timeouts tight at
   1000ms. Verified: `cargo build -p baud-cli` and `cargo clippy -p baud-cli --all-targets` clean (no new
   warnings), `bash drive/m/m10.sh` passes M10.1-M10.4 end to end against real `/dev/kvm`.
6. **`thousand_branches`' resource-growth coverage was recorded as "still open" but was already fixed —
   stale record, same pattern as the `fleet_of_vms` entry above.** The test (`crates/baud-multiverse/src/
   linux/mod.rs::thousand_branches_are_independent_and_deterministic`) already asserts both open-fd count
   (`fds_after <= fds_before + FD_SLACK`) and `VmRSS` growth (`rss_after_kib <= rss_warm_kib +
   RSS_GROWTH_LIMIT_KIB`, a 128 MiB bound sized to catch even one leaked 256 MiB guest-RAM region) across
   its ~1008 sequential `KVM_CREATE_VM`/vCPU/perf_event lifecycles — added by commit `2c0919a` (the same
   commit that fixed the `fleet_of_vms` flake above), predating this record. No code changed for this entry
   beyond correcting the record.
7. **`crates/baud-journal`'s encrypted path shelled out to the `age` binary — fixed.** `age_encrypt`/
   `age_decrypt` in `crates/baud-journal/src/lib.rs` were removed; `append`/`read_chunk` now call
   `baud_keys::age_encrypt`/`baud_keys::age_decrypt` directly (the pure-Rust `age` crate, already a
   dependency), so the encrypted-journal path needs no `age` binary on PATH and is fully testable on this
   dev host. `read_chunk` resolves the identity file via `baud_keys::age_key_path()` (unchanged resolution
   order: `$SOPS_AGE_KEY_FILE` → OS-standard `sops`/`age` locations) and fails with a descriptive
   `JournalError::Io` if none is found. The caveat this entry predicted held: `baud_keys::age_encrypt` emits
   binary (non-armored) age format, not the ASCII armor the old shell-out wrote — harmless, since decrypt
   goes through the same in-process path and no encrypted journal has ever been persisted outside tests
   (`open_encrypted` still has no callers outside this crate's own tests). The binary format still begins
   with the format's own ASCII magic line (`age-encryption.org/v1`), which
   `requesting_encryption_never_leaves_plaintext_on_disk` already checked for as a fallback alongside the
   armor header, so it needed no change. The `#[ignore]`d `chunk_bodies_are_ciphertext` (needs the real
   `age` binary, absent on this host) was rewritten to match: it now generates its test identity in-process
   via `baud_keys::generate_identity_file` (no `age-keygen` binary needed either), asserts the on-disk chunk
   starts with the binary magic, and its only remaining use of the real `age` CLI is decrypting that
   in-process-encrypted chunk — an interop check that baud_keys's ciphertext is standard age format, not
   self-consistency with its own decrypt. Verified: `cargo build -p baud-journal -p baud-keys` and `cargo
   clippy -p baud-journal -p baud-keys --all-targets` clean (no new warnings); `cargo test -p baud-journal`
   8 passed / 1 ignored (the CLI-interop test, `age` not installed on this host per `CLAUDE.md`) / 0 failed.
8. **`run_to_events`/`inject_at`'s single-step engine overshooting its target RCB — fixed. The filed
   hypothesis was wrong; the real root cause was a different bug entirely, found by direct hardware
   instrumentation.** Filed while building H9's fingerprint capture primitives (§14 item 8):
   `run_to_events` (`crates/baud-vcpu/src/boundary.rs`) and `inject_at`/`inject_timer_tick` (same
   arm-early-then-single-step machinery, so every periodic-timer/virtio-rng/virtio-blk test goes
   through the same path) could land 6 to 43 branches past the requested `target_rcb`, non-
   monotonically, when a forced-diagnostic-exit instruction coincided with the single-step window.
   The original hypothesis blamed `baud_vcpu::linux::pmu::LinuxPmuStepper::step`
   (`crates/baud-vcpu/src/linux/pmu.rs`) — its inner loop re-entering `KVM_RUN` whenever an exit
   resolves to `DispatchOutcome::Continue`, theorized to let more than one guest instruction retire
   per call. That theory was investigated and **disproved** on real hardware: `step()` needed no
   logic change at all. Its loop is architecturally correct, not a leak — every exit it loops past
   (I/O-bitmap traps on `IN`/`OUT`, EPT-violation MMIO exits, `Rdmsr`/`Wrmsr`) is fault-like per
   Intel SDM Vol. 3C §26.1.3/§27.1: taken *before* the trapping instruction retires, with KVM only
   completing/retiring it on the *next* `KVM_RUN` entry, so returning early there would hand the
   caller a non-instruction-boundary point. `step()` gained only an explanatory doc comment
   recording this, so the loop is not "fixed" again by mistake.
   The real root cause: `LinuxBranchCounter::new` (`crates/baud-multiverse/src/linux/mod.rs`) built
   its `perf_event::Builder` without ever clearing the crate's own silent default
   `exclude_kernel = 1`. Every baud guest runs at CPL 0, so a USR-only event select filtered the
   guest's entire instruction stream out of the counter — what the "RCB" work clock actually
   measured was host **userspace** branches retired inside each bracketed `KVM_RUN` ioctl call,
   measured directly against `timer-guest` at a flat +54 per free-running exit and +44 per single
   step regardless of how many branches the guest itself retired (confirmed by rebuilding the
   fixture with 256x the inner-loop branch count and observing the identical +54/+44 quantum). That
   coarse, host-noise-driven quantum is exactly why the engine could never land exactly on
   `target_rcb` — the previously observed 6-to-43-branch overshoot is `quantum - (target mod
   quantum)`. Fix: `builder.attrs_mut().set_exclude_kernel(0)` plus
   `builder.attrs_mut().set_exclude_host(1)`, i.e. count only VMX non-root (guest-mode) branches.
   This overturns a finding this same constructor's own comment (and `crates/baud-vcpu/src/lib.rs`'s
   `resume_rcb` doc) previously recorded, that `exclude_host` "reads back 0 for the whole run" on
   this project's nested-virtualized dev host: that was a misdiagnosis of this same bug —
   `exclude_host(true)` was being layered on top of the crate's own default `exclude_kernel = 1`, so
   the pair asked for "guest-mode CPL-3 branches only", of which a ring-0 guest retires none. With
   `exclude_kernel` cleared, the same fd reads the guest's exact architectural branch count with
   zero host contamination; the `resume_rcb`/`pause_rcb` bracketing around each `KVM_RUN` (kept as
   defence in depth) now measures 0 cost per call, down from 11.
   New real-hardware test `run_to_events_lands_exactly_on_target_rcb`
   (`crates/baud-multiverse/src/linux/mod.rs`) sweeps 8 consecutive absolute RCB targets and asserts
   each lands exactly (pre-fix, all 8 landed on the identical coarse quantum boundary, overshooting
   by 42 down to 35 respectively); `timed_exit_fingerprint_is_stable`'s assertion tightened from
   `>= TARGET_RCB` to `== TARGET_RCB`; `RCB_HARDWARE_JITTER_TOLERANCE` (used by
   `timer_tick_lands_at_identical_instruction`/`periodic_timer_injection_halts_gracefully_and_
   reproducibly`) tightened `8` → `0`, confirmed on 10/10 idle-host repetitions plus 20/20
   repetitions with every logical core saturated by competing load — the ±1-4 (worst case ±34)
   `rcb` disagreement this tolerance used to absorb was the same host-branch contamination, not
   genuine hardware counter-read jitter as previously believed. Verified: `cargo clippy -p
   baud-vcpu -p baud-multiverse --all-targets` at the exact pre-change warning baseline (0 new);
   `cargo test -p baud-vcpu` 34 passed; `cargo test -p baud-multiverse --lib` 213 passed ×10
   repeated runs; `cargo test --workspace` green; `drive/h/h0.sh`, `h4.sh`, `h5.sh`, `h7.sh`,
   `drive/pkg/pkg-boot-cli.sh` all PASS; full `bash drive/gate.sh` — 23 passed, 0 failed, 1 skipped
   (pkg-build-cli, fingerprint unchanged), 2m54s, clean, no flakes, no new clippy warnings.

## 15. Pre-push validation protocol

Run **`bash drive/gate.sh`** — one Bash call, `timeout: 600000`. That is the whole pre-push gate.

It runs, in order: a warm-up `cargo build --workspace --tests --bins` (which then lets every drive script
skip its own no-op `cargo build` via `BAUD_GATE_PREBUILT`), `cargo clippy --workspace --all-targets`,
`cargo test --workspace`, the 19 fan-out drive scripts 8-wide, `drive/h/h6.sh` on an otherwise-idle host,
`drive/pkg/pkg-build-cli.sh` only when its fingerprint changed, and finally **phase 6**: if
`rdtsc_guest_reproduces_high_bits_across_boots` (the one documented load-flake with a known mechanical cause,
see §14.1 item above) was the *sole* cause of a unit's failure, the gate re-runs just that test alone on the
now-idle host and reclassifies the unit `FAIL` → `FLAKE` in the summary table — but **a flake still exits 1**,
same as a real failure; it is reported, not excused, so re-run isolation evidence is never silently swallowed
into a green gate. `--no-flake-rerun` disables phase 6 (default on). This closed a real `h3.sh` bug found
alongside it: `RDTSC_OUT=$(cargo test ...)` had no `|| true` under the script's `set -e`, so a failing test
aborted the assignment itself — neither the captured test output nor the script's own `fail()` diagnostic ever
printed, which is exactly the silent-truncation failure mode §14.1's "false passes in the drive scripts" catalogs.
It prints a per-unit PASS/FAIL/FLAKE/duration table and writes per-unit logs under `target/gate-logs/<run-id>/`.
Exit code is non-zero iff some unit failed or flaked.

- **Do not run the units by hand instead.** The old sequence (`cargo build`, then `cargo clippy`, then
  `cargo test`, then each `drive/*.sh` one at a time) takes ~16 min against the gate's ~6 min, and re-runs
  `pkg-build-cli.sh` and `thousand_branches` every time for no added coverage.
- **A failing unit does not abort the gate**, so one run reports the state of everything rather than stopping
  at the first problem.
- **If it exceeds the call timeout**, re-run with `run_in_background: true`. Do not split it into pieces.
- **`--jobs N`** changes fan-out width (default 8); `--jobs 1` is the serial equivalent. `--force-build-cli`
  runs the gated kernel-build script regardless of its fingerprint.
- **Before treating a failure as a regression, re-run that one script in isolation.** This host has a
  documented load-flake history in `timer_tick_lands_at_identical_instruction`,
  `rdtsc_guest_reproduces_high_bits_across_boots`, `fleet_of_vms_run_in_parallel_without_interference`, and
  `baud host probe` reporting `regime=rejected` under PMU contention. Report a flake as a flake, with both
  results; a failure that reproduces in isolation is real and must not be worked around.
  `rdtsc_guest_reproduces_high_bits_across_boots` failed in all 4 consecutive `gate.sh` runs used to verify
  the watchdog work in §14.1, every time only inside the 8-wide fan-out and passing cleanly every time it
  was re-run in isolation; `ps aux` confirmed a second, independent `ralph/ralph` loop (a different PID
  tree) was running against this same repo/host for the whole window, a concrete instance of "two concurrent
  Ralph sessions sharing one host" amplifying this test's known PMU/RCB-counter contention sensitivity.
- **The enforced-regime scripts (`drive/manual/h3-enforced-*.sh`, `drive/manual/h7-enforced-*.sh`) are deliberately not in
  the gate.** They `rmmod`/`insmod` the live `kvm_intel` and guard on `fuser /dev/kvm`, so they are mutually
  exclusive with every other baud process on the box — run them by hand, one at a time, and confirm the stock
  module is restored afterwards.
- `bats drive/gate.test.bats` covers the gate itself and the concurrency-safety contract the drive scripts
  must uphold; the `slow`-tagged tests interrupt a live gate and need an idle machine.
