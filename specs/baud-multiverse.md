<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Multiverse Specification

**Status:** Planned\
**Version:** 2.0\
**Last Updated:** 2026-07-24

---

## 1. Overview

### Purpose

`baud-multiverse` is the deterministic virtual machine monitor (VMM) and the foundation of baud. It runs a
whole guest machine (a bootable OS image + the software under test) on Linux KVM + Intel VT-x such that the
machine's execution is a reproducible function of `(guest image, tape)`. It owns every guest-visible source
of nondeterminism — time, randomness, device input, interrupt timing — and serves each from, or seeds each
from, the tape. It is the first deliverable.

### Goals

- **Own the machine**: control the guest at the virtualization layer, not by intercepting a process
- **Every exit deterministic**: each VM exit resolves to a computed value; the catch-all fails loud
- **Work-clock time**: guest time is a function of work done (retired conditional branches), not wall-clock
- **Snapshot-branchable**: capture any moment, fork many continuations sharing memory

### Non-Goals

- More than one vCPU per VM (multi-core guest determinism is out of scope)
- Real device emulation beyond the console + tape device
- Claiming enforced guarantees while running on stock KVM (see §7 regimes)

---

## 2. Crate Architecture

```
┌───────────────────────────────────────────────────────────┐
│                      baud-multiverse                        │
│  KVM/VT-x VMM · CPUID+TSC+MSR control · work-clock          │
│  interrupt injection engine · tape device · snapshot hooks  │
└───────────────────────────────────────────────────────────┘
   uses baud-vcpu · baud-tape-device · baud-snapshot · baud-proto
```

### Rationale

- Deps = `{kvm-ioctls 0.25, kvm-bindings 0.14, vm-memory 0.18, linux-loader 0.14, vmm-sys-util 0.15,
  vm-superio 0.8, perf, baud-vcpu, baud-tape-device, baud-snapshot, baud-proto}`. One VMM thread + one vCPU
  thread. Soft budget ≤ 4,000 LOC. Knows the machine, not workloads.

---

## 3. Nondeterminism Handling (normative)

| Source | Handling |
| --------------------------------------- | ---------------------------------------------------------- |
| CPUID (RDRAND/RDSEED/TSX/x2APIC/topology) | Always exits under VT-x; served fixed via `KVM_SET_CPUID2`; nondeterministic bits masked |
| RDTSC / RDTSCP                          | Cooperative: `KVM_SET_TSC_KHZ` + `KVM_VCPU_TSC_OFFSET`. Enforced: force RDTSC-exiting → work-clock value |
| Other time (kvmclock, APIC/TSC-deadline) | Follow the virtual TSC; delivered by the injection engine |
| HPET / PIT / PM-timer / RTC             | Deleted — a minimal machine has none |
| Randomness                              | Masked in CPUID (cooperative); hardware-trapped and tape-served (enforced) |
| External input / entropy                | Served from the tape via the tape device |
| Interrupt timing                        | Injected at an exact instruction boundary (§5) |
| Memory init                             | Zeroed RAM at fixed guest-physical addresses |
| Any unmodeled exit                      | `Err(DeterminismHole)` — never a best-effort continue |

---

## 4. CPUID & Time Control

- **CPUID**: start from `KVM_GET_SUPPORTED_CPUID`; clear RDRAND `01H:ECX[30]`, RDSEED `07H:EBX[18]`, TSX
  `07H:EBX[4]/[11]`, x2APIC `01H:ECX[21]`; pin topology `0BH/1FH`; set invariant-TSC `80000007H:EDX[8]` and
  a fixed hypervisor-present bit; `KVM_SET_CPUID2`.
- **Work-clock**: `perf_event_open(PERF_COUNT_HW_BRANCH_INSTRUCTIONS, conditional, guest-filtered)` on the
  vCPU thread; `virtual_tsc = base + k × rcb`. Raw retired-instruction count is forbidden (double-counts).
- **MSR filter**: `KVM_X86_SET_MSR_FILTER` routes `IA32_TSC`/`TSC_AUX`/`TSC_DEADLINE` to the VMM.

---

## 5. Interrupt Injection at an Exact Boundary

Arm the branch counter a margin before the target work-count → take the early exit →
`KVM_SET_GUEST_DEBUG(SINGLESTEP | BLOCKIRQ)` and step until `(PC + GP regs + RCB [+RCX/+stack checksum])`
matches → confirm `ready_for_interrupt_injection` (else `request_interrupt_window`) → inject via
`KVM_INTERRUPT` / `KVM_SET_VCPU_EVENTS`. (Detail in `specs/baud-vcpu.md`.)

---

## 6. API

```rust
impl Multiverse {
    fn load(image: GuestImage, manifest: RunManifest) -> Result<Self>;
    fn run(&mut self, tape: impl TapeSource) -> ObservationStream;   // to next Hlt/branch point
    fn snapshot(&self) -> Universe;                                  // baud-snapshot capture
    fn restore(u: Universe) -> Result<Self>;
}
```

`ObservationStream` exposes `completed()` (guest halted normally) and `work_clock_reads_are_monotone()`
(the emitted rdtsc observations are non-decreasing); both operate on real guest execution, not synthetic
data.

---

## 7. Regimes

- **Cooperative (stock KVM)** — first target. Full CPUID control, fixed-frequency virtual TSC + controllable
  offset, MSR trapping, single vCPU, zeroed memory, tape device. Reproducible for guests that take
  entropy/clock/input from the tape device.
- **Enforced (custom KVM module)** — forces every RDTSC and random instruction to exit and be served from
  the work-clock/tape, so even an adversarial guest is reproducible.
- The manifest records the regime; `run` and `verify` report guarantees only for the regime in force.

---

## 8. Testing

```rust
#[test] fn double_boot_memory_identical() {
    let a = boot(hello_image(), tape.clone()).ram_hash_at_first_hlt();
    let b = boot(hello_image(), tape).ram_hash_at_first_hlt();
    assert_eq!(a, b);
}

#[test] fn cpuid_leaves_are_fixed() {
    let (a, b) = (served_cpuid(&run1), served_cpuid(&run2));
    assert_eq!(a, b);
    assert!(a.rdrand_bit() == 0 && a.rdseed_bit() == 0 && a.tsx_bits() == 0 && a.x2apic_bit() == 0);
}

#[test] fn work_clock_is_monotone_and_reproducible() {
    let s1 = run(timestamp_guest(), tape.clone()).tsc_reads();
    let s2 = run(timestamp_guest(), tape).tsc_reads();
    assert!(is_monotone(&s1) && s1 == s2);
}

#[test] fn rdrand_guest_is_flagged() {
    // cooperative: divergent double-run; enforced: Crash{detail:"rdrand"}
    let out = run(rdrand_guest(), tape);
    assert!(out.is_divergent() || matches!(out.outcome, Crash{ detail, .. } if detail.contains("rdrand")));
}

#[test] fn regime_is_recorded_and_not_overclaimed() {
    let run = start_on_stock_kvm(spec);
    assert_eq!(run.manifest.regime, Regime::Cooperative);
    assert!(request_enforced_guarantee(&run).is_err());   // exit 1, not a false pass
}
```

Additional named tests owned here and exercised by drives: `no_unmodeled_exit_is_silent` (§`baud-vcpu`),
`host_tsc_is_stable` / `rcb_is_deterministic_on_this_cpu` (H0 via `baud-host`),
`divergence_is_detected_and_reported`, `amd_host_refused_in_enforced_regime`.

---

## 9. Security Considerations

| Concern | Handling |
| --------------------------- | --------------------------------------------- |
| Guest escapes the machine   | KVM confines the guest to its memory slots; no host device/DMA |
| Nondeterminism leaks via an exit | Catch-all fails loud; open-bus reads fixed |
| Over-claimed determinism    | Regime recorded; enforced needs the module + Intel host |
| Host TSC instability        | Rejected at H0 (`KVM_GET_TSC_KHZ` = -EIO) |

---

## 10. Future Considerations

| Feature | Description |
| ------------------ | -------------------------------------------------- |
| Custom KVM module  | The enforced regime: hardware RDTSC/random-instruction exiting |
| AMD support        | VMCB intercepts + TSC-ratio scaling (phase-2) |
| Multi-machine nets | Several single-vCPU VMs on a tape-driven virtual network |
