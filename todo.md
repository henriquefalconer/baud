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
- **H7 — real Linux guest: boot → double-boot → OS-entropy.** Build a real Linux image (§4) and boot it on
  the deterministic machine, then walk the validation ladder. Drive `drive/h7.sh`:
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
| 26 | No real Linux guest exists yet — only hand-assembled payloads | A real image pipeline: minimal builtin kernel + deterministic cmdline + boot_params (E820, `SETUP_RNG_SEED`) + reproducible initramfs + `/init` + a tape endpoint, built by `baud-packages` (§4) | `guest_kernel_boots_to_userspace`; `boot_params_seed_is_pinned`; `init_powers_off_deterministically`; `image_build_is_reproducible` |
| 27 | OS-entropy determinism must be shown, not asserted | Boot a real Linux guest and prove the CRNG is a pure function of the tape end-to-end (§4, §3.8) | `os_entropy_is_deterministic` (H7); `double_boot_ram_hash_identical` |
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
  `GET /host/probe` and all 7 `drive/h0.sh`-`h6.sh` scripts now read the renamed JSON fields
  (`enforced_module_present`/`runnable`/`enforced_capable`) instead of a `"regime"` string.
- **Next actions (this rewrite)** — a sequence, each step enabling the next:
  1. **Guest boot pipeline (§4)** — the enabling milestone. Wire H4 interrupt injection into the boot path
     (an earlier real-kernel attempt hung in `calibrate_delay()` waiting on a jiffies tick because injection
     wasn't wired in), then build the real image pipeline in `baud-packages`: a minimal builtin kernel
     (Buildroot → pinned Nix, §4.5), the deterministic cmdline (§4.2), `bootparams.rs` gaining
     `setup_data`/`SETUP_RNG_SEED` + `e820` + initramfs fields, a reproducible initramfs + static `/init`, and
     the `/dev/vport` (or PIO) tape endpoint. Tests `guest_kernel_boots_to_userspace`,
     `boot_params_seed_is_pinned`, `init_powers_off_deterministically`, `image_build_is_reproducible`.
  2. **H7 — OS-entropy end-to-end (rides on #1)** — boot the real Linux guest and prove the CRNG is a pure
     function of the tape: `os_entropy_is_deterministic`, `double_boot_ram_hash_identical`,
     `entropy_guest_is_deterministic`, `initial_crng_state_is_reproducible`,
     `virtio_rng_reseed_is_deterministic` (virtio-rng tape-fed via an ever-ready FIFO, or omitted). No
     guest-kernel patch.
  3. **H8 — Super Mario Bros example (§11, rides on #1)** — rebuild `examples/mario/` under the new model: a
     real Linux image with FCEUX + the Lua harness + `/init` (the pre-KVM `nes_bridge.c` stdin stub is
     retired), `probes.toml` / `strategy.toml`, `drive/mario.sh` completion gate, the ~25% live window
     (`baud stream tail | ffplay`), and the README hero + centralized GIF. All NES specifics stay under
     `examples/` (`no_workload_specifics_in_core`).
  4. **Generic-core guardrail** — add `no_workload_specifics_in_core`; keep the exploration primitives in
     `baud-driver`, never in the example.
- **Specs to update alongside**: `specs/baud-packages.md` (the real kernel + initramfs pipeline, §4), a new
  `specs/baud-stream.md` note (the framebuffer frame path + the ~25% live window), and `specs/README.md` /
  `specs/baud-multiverse.md` (the one determinism model + entropy-by-input-control).
