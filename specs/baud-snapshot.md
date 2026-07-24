<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Snapshot Specification

**Status:** Planned (capture/restore/reset built, unexercised on real hardware; branching open — see §10)\
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

---

## 10. Implementation status

- **Capture/restore (§3, §6) — built.** `crates/baud-snapshot/src/universe.rs` (enumerated capture
  set, ordered restore plan, CPU-model guard, the pure write-set diff) + `src/linux.rs`'s real
  `capture`/`restore` walking every `KVM_GET_*`/`KVM_SET_*` the table above lists, in the plan's
  exact order. Type-checked cross-target, unexercised on real hardware (no KVM host on the
  reference dev machine — this applies to every claim in this section).
- **Reset (§5) — built and wired into `baud-multiverse`.** `KVM_CAP_DIRTY_LOG_RING` is real:
  `crates/baud-snapshot/src/dirty_ring.rs` is the hardware-independent ring-scan protocol (decode a
  `kvm_dirty_gfn` ring into harvested `(slot, offset)` pairs, unit- and property-tested with no KVM
  involved), driven by `src/linux.rs`'s `DirtyRing` (`KVM_ENABLE_CAP(KVM_CAP_DIRTY_LOG_RING, ...)`,
  an mmap of the per-vCPU ring at `KVM_DIRTY_LOG_PAGE_OFFSET`, and `KVM_RESET_DIRTY_RINGS` to
  reclaim harvested pages). Because §1's hard constraint (one vCPU per VM) holds workspace-wide, a
  per-vCPU ring is a whole VM's dirty-page record with no cross-vCPU merge to do.
  `KVM_RESET_DIRTY_RINGS`'s ioctl number is not in the pinned `kvm-ioctls` 0.25 — it is derived
  from the same `ioctl_expr` helper `kvm-ioctls` itself is built from (`vmm_sys_util::ioctl`), not
  hand-encoded, to minimize the risk of an unverifiable-on-this-machine mistake.
  - **Wired into `baud-multiverse`**: `Multiverse::enable_dirty_ring(entries)` negotiates the ring
    right after a guest is booted/restored (before any guest execution, matching
    `DirtyRing::enable`'s own "must be negotiated before any dirty page could occur" requirement);
    `Multiverse::reset_dirty_pages(base_ram)` collects the harvest, restores exactly those RAM
    pages from a caller-supplied base `Universe::ram` slice, and only then confirms the reset to
    the kernel — a mid-loop write failure leaves the un-restored pages un-confirmed rather than
    lying to the kernel about what was reclaimed. The one piece of real logic in that wiring —
    reducing a harvest's `(slot, offset)` pairs down to the RAM page indices a rewind must touch —
    is factored into a small, hardware-independent, *ungated* module
    (`crates/baud-multiverse/src/dirty.rs`, deliberately outside the `#[cfg(target_os = "linux")]`
    `linux/` tree so `cargo test -p baud-multiverse` actually exercises it on this Windows dev
    machine, unlike the KVM-calling code around it): `ram_page_indices` keeps only entries for the
    single registered RAM memslot and passes every other offset through as the literal RAM page
    index (`universe.rs`'s "page `i` covers `[i*PAGE_SIZE, (i+1)*PAGE_SIZE)`" convention), proven
    by 8 unit/property tests with no KVM/mmap at all — `reset_dirty_pages`'s return value (pages
    actually restored) is therefore provably bounded by the dirty ring's own harvest count, the
    direct observable `reset_cost_scales_with_write_set` checks. The real
    `enable_dirty_ring`/`reset_dirty_pages` calls themselves are, like every other `linux/`-tree
    method in this workspace, type-checked (`cargo check --target x86_64-unknown-linux-gnu -p
    baud-multiverse`) but not yet exercised on real KVM hardware.
- **Branching (§4) — not built; a real blocker found, not just a missing wrapper.** The spec's
  `UFFDIO_CONTINUE`-based sharing requires the kernel's *minor-fault* mechanism, which only exists
  for shared (memfd/hugetlbfs/shmem) mappings — but `baud-multiverse`'s guest RAM
  (`GuestMemoryMmap::from_ranges`) is a private anonymous mapping. Wiring `UFFDIO_CONTINUE` today
  would need switching guest-RAM backing to a shared memfd first, an architecture change to
  `baud-multiverse`, not something this crate can absorb alone. The spec's own "small-N fallback",
  `fork()`, is not a safe drop-in either: once specs/baud-multiverse.md §3.1's "one VMM thread + one
  vCPU thread" model is live, `fork()`ing that process only leaves the calling thread in the child —
  any lock the other thread held at fork time is frozen forever, a real hazard for this specific
  threading model. Both findings are tracked in `crates/baud-snapshot/src/lib.rs`'s module doc and
  todo.md §14; neither is fixed here.
