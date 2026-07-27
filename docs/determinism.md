# Determinism in baud

> **2026-07-24 pivot notice.** baud moved from a userspace ptrace/seccomp supervisor of a single
> guest process to a KVM/VT-x deterministic VMM (`baud-multiverse`) that owns a whole guest
> machine — see `todo.md` and `specs/baud-multiverse.md` v2.0. The sections below this notice
> (H0 results, the nondeterminism catalogue, CPUID path selection) describe the **superseded**
> ptrace/seccomp mechanism and are retained for history until rewritten; they no longer describe
> what the code does. The current H0 capability-spike result is recorded immediately below.

## H0 (KVM/VT-x) — capability spike, current pivot

Probed via `baud host probe --json` (`specs/baud-host.md` §3, `crates/baud-host`), run from
`drive/h/h0.sh` on the machine this iteration's work was done on:

| Field | This dev machine (Windows 11, no WSL2 distro installed) |
|---|---|
| `kvm` | `false` — `/dev/kvm` does not exist on Windows |
| `vmx` / `cpuid` / `tsc_stable` / `msr_filter` / `singlestep` / `rcb_deterministic` / `nested` | `false` — all gated behind `/dev/kvm`; never assumed true |
| `vendor` | `other` (no `/proc/cpuinfo` to read) |
| `enforced_module_present` / `runnable` / `enforced_capable` | `false` |
| `reason` | `"/dev/kvm unavailable: expose /dev/kvm to this host (bare-metal, or a nested-virt host with kvm_intel nested=1)"` |

This is the **honest, expected** result on this exact machine, not a bug: per `specs/baud-host.md`
§2/§9's "developer machine" guidance, a Linux host with real `/dev/kvm` is required — either
bare-metal Linux, a nested-virtualization-enabled Linux VM, or (on this Windows box) a WSL2
distro with `nestedVirtualization=true` in `.wslconfig`, none of which are installed here yet
(`wsl --status` shows the WSL2 feature is enabled but **no distro is installed**). `baud host
probe` and the CLI both refuse to overclaim: `runnable` reports `false` and names the failing
check rather than reporting a false pass (`baud host probe` exits `1`).

`crates/baud-host`'s own capability-decision logic (§4 of the spec: the required-vs-cooperative
gate, the Intel/AMD split, the sibling-safe fleet placement) is hardware-independent and is
covered by `cargo test -p baud-host` via an injectable `CapabilityChecks` seam — those tests do
not need real KVM and pass on any OS. The Linux ioctl/`/proc`/`/sys` implementation
(`crates/baud-host/src/linux.rs`) type-checks against the real `kvm-ioctls`/`kvm-bindings`/
`perf-event` crate sources (`cargo check --target x86_64-unknown-linux-gnu -p baud-host`) but has
**not yet been exercised against real KVM hardware** — that validation is blocked on getting a
Linux/KVM host (e.g. install a WSL2 distro with nested virt, or bare-metal Linux) and is the
next H0 milestone action, tracked in `todo.md`.

## Branch-counter (PMU) probe — 2026-07-25, WSL2 dev box

The dev machine now has a WSL2 Ubuntu distro installed (nested virt on, `/dev/kvm` present). A userspace
probe of the retired-branch PMU counter baud's work-clock depends on (`tools/pmucheck.c`: `perf_event_open`
over a fixed 10M-iteration loop, three runs) measured, on the Dell XPS 13 9310 (Intel Tiger Lake) **inside
WSL2**:

| Event | Runs | Verdict |
|---|---|---|
| `BR_INST_RETIRED.COND` (raw `0x11c4`; the work-clock event) | `20000003`, `20000003`, `20000003` | **deterministic** ✓ |
| `PERF_COUNT_HW_BRANCH_INSTRUCTIONS` (all branches) | `25000016`, `25000017`, `25000016` | ±1 — **rejected** |

Environment confirmations: `systemd-detect-virt` = `wsl`, `grep -c vmx /proc/cpuinfo` = `16`, `/dev/kvm`
present, `CONFIG_PERF_EVENTS=y`. So the hardware PMU **is** exposed to the WSL2 (L1) guest under Hyper-V — the
conditional-branch counter baud uses is bit-exact, while the all-branch event is not. This is exactly why the
work-clock counts conditional branches only (`specs/baud-multiverse.md` §4, `todo.md` §3.3): had it used
`HW_BRANCH_INSTRUCTIONS`, "stop at count N" would land a branch off run-to-run and the cross-VM fingerprint
(`specs/baud-fingerprint.md`) would spuriously diverge.

**Status: userspace PASS, guest-level H0 pending.** This proves the counter is available and deterministic in
L1 userspace, but the authoritative gate is guest-filtered counting across `KVM_RUN`
(`rcb_is_deterministic_on_this_cpu`, via `baud host probe` / `drive/h/h0.sh`) — run once baud builds in this
WSL2 distro. If the guest-level check also passes, the H9 cross-VM fingerprint can run nested; only if it
fails does the fingerprint move to bare-metal Intel.

---

## Prior art: Antithesis

Antithesis's hypervisor is a modified bhyve on Intel VMX.  Their published
experience is the closest prior art:

- One physical core per instance; virtual time pegged to instruction counts.
- PMC-based interrupt injection for deterministic async delivery.
- Known limits: instruction counters miscount ~1 in 10¹² instructions; interrupt
  delivery lands dozens of instructions late with variable latency.

That is the engineering cost of supporting **arbitrary threaded software**.

baud avoids the entire cost class by constraining guests (single-threaded,
syscall-boundary switching) instead of counting instructions.

---

## The nondeterminism catalogue

| Source | baud handling |
|---|---|
| Thread/process scheduling | Eliminated: one thread per guest; cross-guest switching only at syscall boundaries, order is a tape draw |
| Async signals/interrupts | Eliminated: none delivered to guests |
| Clocks (`clock_gettime`, `gettimeofday`, `nanosleep`, …) | Virtual clock device; advances deterministically per syscall and per scheduling quantum |
| `rdtsc`/`rdtscp` | `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)` → trap → virtual clock emulation |
| `cpuid` | `arch_prctl(ARCH_SET_CPUID, 0)` → trap → synthetic fixed CPUID; on CPUs without CPUID faulting: record all leaves + vendor/model in manifest, pin reconstruction, double-run backstop |
| `rdrand`/`rdseed` | Feature bits masked in synthetic CPUID; direct use without CPUID check caught by double-run verification |
| Entropy (`getrandom`, `/dev/urandom`, `AT_RANDOM` auxv) | Served from tape draws, including auxv bytes at exec |
| Filesystem | In-memory read-only snapshot + copy-on-write; writes hashed into observations |
| Network | Virtual socket device: all connect/send/recv mediated; delivery order, delay, drop, duplicate, partition are tape draws |
| External input | Tape-fed input channel device |
| Other syscall results (pids, uids, `uname`, `sysinfo`, `/proc`) | Virtualized fixed values from the ~25-syscall allowlist |
| CPU/FP/microarchitectural variation | CPU class + CPUID leaves in manifest; reconstruction requires same class; double-run backstop |

---

## H0 capability probe results

H0 was run in a real Daytona sandbox (Linux x86_64 kernel).

| Capability | Result |
|---|---|
| `ptrace(PTRACE_SYSCALL)` | Available |
| `seccomp(SECCOMP_SET_MODE_FILTER, SECCOMP_RET_USER_NOTIF)` | Available |
| `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)` | Available (Intel) |
| `arch_prctl(ARCH_SET_CPUID, 0)` | Available (Intel) |
| eBPF (`bpf(BPF_PROG_LOAD, …)`) | Unavailable in shared container runtimes → fallback to /proc-sampling + strace-shim |
| CPUID faulting support | Confirmed Intel; AMD path is record-and-pin |
| Kernel version | 5.15+ |

The chosen mechanism is: **seccomp user-notify** for the allowlist (supervisor
serves syscalls from device models) + **ptrace** for trap handling (TSC/CPUID
emulation and kill-with-report for violations).

---

## The determinism claim

> Single-threaded guests + no async delivery + all syscalls served
> deterministically + trapped TSC/CPUID + fixed memory layout ⇒ execution is a
> **pure function of (binary, manifest, tape)**.

This is a claim, not an axiom.  It is verified by `baud verify determinism`:

1. Run the workload twice from the same seed (same tape choices).
2. Compare the observation stream hashes (blake3 over all `Observe` records in
   order).
3. Agreement is gate-level evidence; disagreement reports the first divergent
   step.

Contract violations the supervisor cannot trap (CPU-class drift, RDRAND misuse
on non-faulting CPUs) surface as a reported first-divergent-step, and the run
is marked unusable for replay/shrink/reconstruct.

---

## CPUID path selection

| CPU | CPUID faulting? | baud handling |
|---|---|---|
| Intel Skylake+ | Yes (`CPUID_FAULT` bit in IA32_MISC_ENABLE) | `arch_prctl(ARCH_SET_CPUID, 0)` → trap → synthetic CPUID |
| AMD | No | Record all CPUID leaves + vendor/model at H0 → store in manifest → pin reconstruction to same CPU class → double-run backstop |

---

## Guest contract (enforced, not requested)

Violations kill the guest at the offending instruction with a detailed report:

- One thread, one process: `clone`, `fork`, `vfork`, `execve` (post-start) → kill
- No async signal delivery
- Statically linked, no-PIE, musl-built via nix; fixed argv/env/locale from manifest
- `personality(ADDR_NO_RANDOMIZE)`; brk/stack layout in manifest
- Syscalls outside the allowlist → kill with report

The allowlist (~25 syscalls): `read`, `write`, `open`, `openat`, `close`,
`stat`, `fstat`, `lstat`, `poll`, `lseek`, `mmap`, `mprotect`, `munmap`,
`brk`, `pread64`, `pwrite64`, `access`, `getcwd`, `chdir`, `exit`,
`exit_group`, `clock_gettime`, `gettimeofday`, `nanosleep`, `getrandom`, and
`uname`/`sysinfo`/`/proc` variants — each served from a deterministic device
model.

---

## Observation planes

Two independent observation planes provide cross-checks:

**Plane 1 — baud-multiverse syscall log**: every syscall with arguments,
result, and virtual timestamp.  Primary observation stream.

**Plane 2 — baud-tracing (eBPF / fallback)**: kernel-side ground truth.  Where
eBPF is available: CO-RE probes (sched/exec/syscall/fault); where denied
(shared container runtimes): /proc-sampling + strace-shim emitting the same
`EbpfRecord` schema flagged `source=fallback`.

`baud verify observation --run` cross-checks planes 1 and 2 (per-guest syscall
counts and sequences must agree); disagreement indicates a supervisor bug or an
escaped guest.

---

## Reconstruction

Reconstruction = `(manifest + tape prefix) → fresh sandbox → same closure →
replay under supervisor → verify observation-stream-hash prefix equality →
resume`.

- Replay cost is O(steps in prefix); no mid-run state snapshot.
- Resuming at step K always replays 0..K.
- Shrinking batches many candidate tapes inside one sandbox process.
- Divergence detection reports the first mismatching step and the node/probe/
  syscall that diverged; a divergent run is marked and excluded.

---

## Known limits

1. **CPU-class drift**: if the reconstruction host has a different CPU class
   than the original, CPUID leaves may differ.  Manifest records CPU class;
   `doctor` checks it; `verify determinism` is the backstop.
2. **RDRAND misuse**: a guest that calls `rdrand` without checking CPUID will
   get real entropy.  The double-run backstop reports this as divergence.
3. **Kernel version drift**: syscall semantics can change across kernel
   versions.  The manifest records the kernel version; reconstruction logs a
   warning on version mismatch.
4. **Wall-clock watchdog**: a guest spinning without syscalls starves the
   cluster.  The supervisor's wall-clock watchdog (outside the deterministic
   boundary) kills it with a report.  This is the one non-deterministic
   intervention; its trigger is logged but not replayed.
