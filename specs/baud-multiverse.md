<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Multiverse Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-multiverse` is the deterministic supervisor and the foundation of baud. It runs guest processes
inside a sandbox such that execution is a pure function of `(binary, manifest, tape)`. It owns every
interaction between a guest and the outside world, serving each from a device model whose decisions come
from the tape. It is the first deliverable.

### Goals

- **Total mediation**: no guest syscall reaches the real kernel unmediated
- **Enforcement over trust**: contract violations kill the guest at the offending instruction, with a report
- **Deterministic multi-guest**: N guests share one virtual clock and one network; switching is a tape draw
- **Rich observation**: every syscall is recorded (observation plane 1)

### Non-Goals

- Running arbitrary multi-threaded software (guests are single-threaded)
- Full device emulation beyond the closed model set
- Instruction-counting or PMC-based scheduling

---

## 2. Crate Architecture

```
┌───────────────────────────────────────────────────────────┐
│                     baud-multiverse                       │
│  seccomp user-notify (allowlist) + ptrace (trap handling)   │
│  Device models: clock · entropy · fs · input · net · exit   │
│  Syscall log (observation plane 1)                          │
└───────────────────────────────────────────────────────────┘
        ▲ launched by baud-tape-agent · types from baud-proto
```

### Rationale

- Deps = `{nix/libc, seccomp bindings, baud-proto}`; no `tokio` (single-threaded event loop).
- Soft budget ≤ 4,000 LOC. Knows syscalls, not workloads.

---

## 3. Guest Contract (enforced)

| Rule | Enforcement |
| ---------------------------------- | ------------------------------------------ |
| One thread, one process            | `clone`/`fork`/`vfork`/post-start `execve` → kill |
| No async signals                   | None delivered; only synchronous faults |
| Static, no-PIE, musl               | Built by baud-packages; verified at spec lint |
| Fixed argv/env/locale              | From the manifest |
| `ADDR_NO_RANDOMIZE`                | Set before exec; layout recorded |
| Allowlisted syscalls (~25)         | Others → kill with report |

---

## 4. Nondeterminism Handling

| Source | Handling |
| --------------------------------------- | ---------------------------------------------------------- |
| Thread/process scheduling               | Eliminated; cross-guest switch at syscall boundaries, order = draw |
| Async signals/interrupts                | Eliminated |
| Clocks (`clock_gettime`, `nanosleep`, …) | Virtual clock; advances deterministically per syscall/quantum |
| `rdtsc`/`rdtscp`                         | `PR_SET_TSC=SIGSEGV` → trap → emulate from virtual clock |
| `cpuid`                                 | `ARCH_SET_CPUID=0` → trap → synthetic fixed leaves; else record-and-pin |
| `rdrand`/`rdseed`                       | Masked in synthetic CPUID; misuse caught by double-run check |
| Entropy (`getrandom`, `/dev/urandom`, `AT_RANDOM`) | Served from tape draws |
| Filesystem                              | RO snapshot + in-memory COW; writes hashed into observations |
| Network                                 | Virtual socket device; order/delay/drop/dup/partition = draws |
| External input (stdin, fifo)            | Tape-fed input channel |
| Other syscalls (pids, uids, uname)      | Fixed virtualized values |
| CPU/FP/microarch variation              | CPU class + CPUID leaves in manifest; double-run backstop |

---

## 5. Mechanism

- **seccomp user-notify** for allowlisted syscalls → supervisor serves them from device models.
- **ptrace** for trap handling: TSC/CPUID emulation, kill-with-report.
- **Device models** (`clock`, `entropy`, `fs`, `input`, `net`, `exit`) consume draws via baud-proto and
  emit observations.

### API

```rust
impl Multiverse {
    fn load(manifest: RunManifest, guests: Vec<GuestImage>) -> Result<Self>;
    fn run(&mut self, tape: impl DrawSource) -> ObservationStream;
}
```

---

## 6. Multi-Guest Clusters

- N guests, one supervisor, one virtual clock, one net device.
- Guests run one at a time; the switch order at each syscall boundary is a draw (schedule chaos = tactics).
- A guest that spins without syscalls starves the cluster → wall-clock watchdog (outside the deterministic
  boundary) kills it with a report.

---

## 7. Determinism Claim & Verification

Single-threaded guests + no async delivery + all syscalls served deterministically + trapped TSC/CPUID +
fixed layout ⇒ execution is a pure function of `(binary, manifest, tape)`.

Verified by `baud verify determinism`: same seed, two fresh tapes, byte-identical observation-stream
hashes. Untrappable violations (CPU-class drift, RDRAND misuse on non-faulting CPUs) surface as a reported
first-divergent step; the run is marked unusable for replay/shrink/reconstruct.

---

## 8. Testing

```rust
#[test]
fn double_run_is_bit_identical() {
    assert_eq!(hyper.run(tape.clone()).hash_stream(), hyper.run(tape).hash_stream());
}

#[test]
fn clone_syscall_is_killed() {
    assert!(matches!(hyper.run(guest("calls_clone")).outcome,
        Crash { detail, .. } if detail.contains("clone")));
}

#[test]
fn rdtsc_is_trapped_and_served_virtual_time() {
    let obs = hyper.run(guest("reads_rdtsc"));
    assert!(obs.completed() && obs.tsc_reads_are_monotonic_virtual());
}
```

- **H1 exit criterion**: the double-run test above passes on a static hello guest.
- Multi-guest: a 3-guest topology under `markov-partition` weather is double-run identical (H3).

---

## 9. Security Considerations

| Concern                     | Handling                                      |
| --------------------------- | --------------------------------------------- |
| Guest escapes mediation     | eBPF cross-check (plane 2) compares syscall sequences |
| Guest writes real disk/net  | Impossible via allowlist; attempts killed     |
| ptrace/seccomp unavailable  | H0 spike decides fallback (seccomp-only + static scan) |

---

## 10. Future Considerations

| Feature            | Description                                        |
| ------------------ | -------------------------------------------------- |
| Threaded guests    | Deterministic syscall-boundary switching for N threads |
| More device models | Deterministic timezone, DNS, and clock-adjust surfaces |
