<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Host Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-24

---

## 1. Overview

### Purpose

`baud-host` manages the KVM-capable host: it probes for the capabilities the VMM needs, decides which
determinism regime the host supports, and places a fleet of single-vCPU VMs across physical cores. It is the
primary substrate for `baud-multiverse`, replacing the managed-container backend (which cannot expose
`/dev/kvm`).

### Goals

- **Capability gate**: no run starts on a host missing a required capability; `host probe` reports them all
- **Regime decision**: report cooperative vs enforced support for this exact host + CPU
- **Deterministic placement**: one physical core per VM, never split across hyperthread siblings

### Non-Goals

- Owning the VMM run loop (that is `baud-multiverse` / `baud-vcpu`)
- Managed multi-tenant sandboxing (baud owns the host)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                   baud-host                    │
│  host probe · regime decision                  │
│  core pinning + capacity accounting            │
└───────────────────┬──────────────────────────┘
        ▲ used by baud-server; exposed via `baud host`
```

### Rationale

- Deps = `{kvm-ioctls, nix, perf (via baud-multiverse), baud-proto}`. Soft budget ≤ 1,500 LOC.

### Types & API

```rust
pub enum Vendor { Intel, Amd, Other }
pub enum Regime { Enforced, Cooperative, Rejected }

pub struct Probe {
    pub kvm: bool, pub vmx: bool, pub tsc_stable: bool, pub msr_filter: bool,
    pub singlestep: bool, pub rcb_deterministic: bool, pub nested: bool,
    pub vendor: Vendor, pub regime: Regime, pub reason: Option<String>,
}

impl Host {
    pub fn probe() -> Probe;                              // fills every capability + regime decision
    pub fn capacity(&self) -> usize;                      // physical cores − housekeeping (SMT adds none)
    pub fn place(&self, n: usize) -> Result<Placement>;   // one core/VM; never splits SMT siblings
}
```

---

## 3. Host Probe

Probes and records each capability; a failure downgrades the regime and is written to `docs/determinism.md`.

| Capability | Check |
| ------------------------------ | ------------------------------------------ |
| `/dev/kvm` present + VT-x       | open `/dev/kvm`; `grep vmx /proc/cpuinfo` |
| CPUID control                   | `KVM_SET_CPUID2` round-trips a masked leaf |
| TSC control                     | `KVM_GET_TSC_KHZ` ≠ `-EIO`; offset set/read |
| MSR filter                      | `KVM_X86_SET_MSR_FILTER` accepted |
| Single-step                     | `KVM_SET_GUEST_DEBUG` single-step exits |
| Branch counter deterministic    | fixed loop twice → equal count at a fixed PC |
| Nested virt (if applicable)     | `kvm_intel nested=1` |
| CPU vendor                      | Intel → both regimes possible; AMD → cooperative-only until phase-2 |

```
baud host probe --json
# → { kvm:true, vmx:true, cpuid:true, tsc_stable:true, msr_filter:true,
#     singlestep:true, rcb_deterministic:true, nested:true, vendor:"intel",
#     regime:"enforced-capable" }
```

---

## 4. Regime Decision

- **enforced-capable**: Intel + custom KVM module present + all checks pass.
- **cooperative**: all stock-KVM checks pass; no module.
- **rejected**: a required capability (kvm/vmx/tsc_stable/rcb_deterministic) failed — the host cannot run
  baud; the report names the failing check and its remediation.

---

## 5. Fleet Placement

| Rule | Enforcement |
| ------------------------------ | ------------------------------------------ |
| One physical core per VM        | vCPU thread pinned via `sched_setaffinity` |
| Never split SMT siblings        | placement refuses two VMs on sibling threads |
| Housekeeping cores reserved     | 2–4 cores per socket kept for host/RCU/IRQ |
| NUMA-local memory               | vCPU + RAM on the same node |
| Capacity                        | ~physical_cores − housekeeping (SMT adds none) |

---

## 6. Testing

```rust
#[test] fn capacity_refuses_sibling_split() {
    let host = Host::probe();
    let plan = host.place(host.capacity() + 1);   // one over capacity
    assert!(plan.is_err());                        // never oversubscribes / splits siblings
    assert!(host.place(host.capacity()).unwrap().no_two_on_sibling_threads());
}

#[test] fn doctor_checks_kvm() {
    let r = Host::probe();
    assert!(r.kvm && r.vmx && r.rcb_deterministic);
    assert!(matches!(r.regime, Regime::Cooperative | Regime::Enforced));
}

#[test] fn rejected_host_names_the_failing_check() {
    let r = probe_with(no_kvm());
    assert_eq!(r.regime, Regime::Rejected);
    assert!(r.reason.contains("/dev/kvm"));
}
```

---

## 7. Security Considerations

| Threat | Handling |
| ------------------------------ | ------------------------------------------ |
| Over-claimed regime            | Regime derived from probes; enforced needs the module + Intel |
| Cross-VM leakage via SMT       | Sibling-split placement refused |
| Untrusted host runs a guest    | KVM confines the guest; baud owns the host |

---

## 8. Future Considerations

| Feature | Description |
| ------------------ | ---------------------------------------------- |
| Multi-host fleet   | Schedule universes across a pool of KVM hosts |
| CPU-template mode  | Normalize CPUID across a heterogeneous fleet for portable restore |
| AMD support        | Phase-2 vendor path once enforced-regime AMD is verified |
