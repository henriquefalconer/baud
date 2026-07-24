<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Snapshot Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-24

---

## 1. Overview

### Purpose

`baud-snapshot` captures a complete VM state (a **universe**), restores it, and forks many continuations that
share memory copy-on-write. It is the mechanism behind the branching multiverse and replaces replay-from-zero
reconstruction: exploration forks from the nearest universe instead of re-running the whole prefix.

### Goals

- **Complete capture**: everything that affects future execution, so a restored universe continues bit-identically
- **Cheap N-way branching**: thousands of universes share unchanged pages; per-branch cost ∝ its write set
- **Cheap rewind**: reset restores only dirtied pages
- **Tree, not line**: snapshots at branch points form a tree explored from the nearest node

### Non-Goals

- Cross-CPU-model portability (restore is host-locked unless a CPUID template is active)
- Persisting to disk (that is `baud-snapshot-store`)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                 baud-snapshot                  │
│  capture(KVM get*) · restore(KVM set*)         │
│  userfaultfd CoW branching · dirty-ring reset  │
│  the branch tree                               │
└───────────────────┬──────────────────────────┘
        ▲ used by baud-multiverse / baud-driver
        │ persisted by baud-snapshot-store
```

### Rationale

- Deps = `{kvm-ioctls, vm-memory, userfaultfd, baud-proto}`. Soft budget ≤ 2,500 LOC.

### Types & API

```rust
pub struct Universe {
    ram: Vec<PageRef>,        // content-addressed pages (shared across universes)
    vcpu: VcpuState,          // regs/sregs/msrs/lapic/xsave2/xcrs/events/mp_state
    clock: ClockState,        // KVM_GET_CLOCK + TSC khz + work-clock anchor
    device: DeviceState,      // tape-device cursor + console
}

pub struct Branch { base: Universe, uffd: Uffd, dirty: DirtyRing, /* … */ }

impl Snapshot {
    pub fn capture(vm: &Multiverse) -> Universe;             // KVM_GET_* (see §3)
    pub fn restore(u: &Universe) -> Result<Multiverse>;      // ordered restore (see §6)
    pub fn branch(parent: &Universe, suffix: TapeSuffix) -> Branch;  // userfaultfd CoW share
    pub fn reset(branch: &mut Branch);                       // dirty-ring: restore only dirtied pages
}
```

---

## 3. Capture Set (omitting any field diverges the restore)

| State | Mechanism |
| ---------------------------- | ------------------------------------------ |
| Guest RAM                    | read each memslot backing (or dirty-ring delta) |
| GP + segment registers       | `KVM_GET_REGS` / `KVM_GET_SREGS` |
| MSRs (incl. TSC, TSC_AUX, deadline) | `KVM_GET_MSRS` (indices from `KVM_GET_MSR_INDEX_LIST`) |
| Local APIC                   | `KVM_GET_LAPIC` |
| Extended (FPU/SSE/AVX/AMX)   | `KVM_GET_XSAVE2` + `KVM_GET_XCRS` |
| Pending events               | `KVM_GET_VCPU_EVENTS` |
| MP state                     | `KVM_GET_MP_STATE` |
| VM clock + TSC freq          | `KVM_GET_CLOCK`, `KVM_GET_TSC_KHZ` |
| Tape-device cursor + console | device model state |
| Work-clock anchor            | branch-count base |

---

## 4. Branching (copy-on-write)

- Parent memory is a shared read-only backing; each child arms **userfaultfd** over its guest regions:
  - `UFFDIO_CONTINUE` — serve an unchanged page from the shared backing (share across many universes)
  - `UFFDIO_WRITEPROTECT` — copy-on-first-write so the child diverges only on the pages it writes
- Per-branch memory ∝ the child's write set, not total RAM. `fork()` copy-on-write is the small-N fallback.

## 5. Reset (rewind)

- Track dirtied pages with the **KVM dirty ring** (`KVM_CAP_DIRTY_LOG_RING`); rewind copies back only those
  pages. Cost ∝ change, not machine size.

## 6. Restore Ordering (determinism)

1. Set TSC frequency (`KVM_SET_TSC_KHZ`) **before** creating the vCPU.
2. Restore `IA32_TSC` **before** `IA32_TSC_DEADLINE`.
3. Restore RAM → vCPU registers/MSRs/LAPIC/XSAVE/events/MP → VM clock → device/console.
4. Refuse restore on a mismatched CPU model unless a fixed CPUID template is active.

---

## 7. Testing

```rust
#[test] fn snapshot_roundtrip_is_bit_identical() {
    let straight = run(image(), tape.clone()).obs_stream(K..K+M);
    let u = capture_at(image(), tape.clone(), K);
    let resumed = restore(u).run_to(K+M).obs_stream(K..K+M);
    assert_eq!(straight, resumed);          // a missing capture field would diverge here
}

#[test] fn thousand_branches_are_independent_and_deterministic() {
    let u = capture_at(image(), tape, K);
    let outs: Vec<_> = (0..1000).map(|i| {
        let b = branch(&u, suffix(i));
        assert!(b.is_deterministic_double_run());
        b.output()
    }).collect();
    assert!(no_branch_perturbs_another(&outs));
}

#[test] fn reset_cost_scales_with_write_set() {
    let u = capture_at(image(), tape, K);
    let b = branch(&u, suffix); b.run_a_bit();
    assert_eq!(pages_restored_on_reset(&b), dirty_ring_count(&b)); // not total RAM
}

#[test] fn restore_refuses_mismatched_cpu() {
    let u = capture_on(cpu_model_a());
    assert!(restore_on(cpu_model_b(), &u).is_err());       // unless a CPUID template is active
    assert!(restore(timer_universe()).timer_resumes_cleanly()); // TSC-before-deadline ordering
}
```

---

## 8. Security Considerations

| Threat | Handling |
| ------------------------------ | ------------------------------------------ |
| Universe reproduces guest secrets | Persisted only encrypted by `baud-snapshot-store` |
| Restore on wrong hardware diverges silently | Refused unless a CPUID template normalizes it |
| CoW page leaks across branches | userfaultfd write-protect isolates per-branch writes |

---

## 9. Future Considerations

| Feature | Description |
| ------------------ | ---------------------------------------------- |
| Incremental snapshots | CoW-remap a base to store only the delta at each branch point |
| CPUID templates    | Normalize leaves so universes restore across CPU models |
| Live migration     | Move a universe between hosts of the same class |
