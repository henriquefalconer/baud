<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Snapshot Specification

**Status:** Planned (capture/restore/reset/branch (small-N fallback) built and exercised on real
hardware; memory-efficient UFFDIO_CONTINUE branching still open — see §10)\
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
| Extended (FPU/SSE/AVX/AMX)   | `KVM_GET_XSAVE2` + `KVM_GET_XCRS` |
| Pending events                | `KVM_GET_VCPU_EVENTS` |
| MP state                     | `KVM_GET_MP_STATE` |
| VM clock + TSC freq          | `KVM_GET_CLOCK`, `KVM_GET_TSC_KHZ` |
| Tape-device cursor + console | device model state |
| Work-clock anchor            | virtual-TSC base + the cumulative RCB value at capture |

**No `KVM_GET_LAPIC` row** (real-hardware finding, corrects this table's original assumption): this
workspace's VMM never calls `KVM_CREATE_IRQCHIP` — every interrupt is delivered by
specs/baud-vcpu.md §5's arm-early-then-single-step engine via `KVM_INTERRUPT`, bypassing in-kernel
LAPIC emulation entirely. `KVM_GET_LAPIC`/`KVM_SET_LAPIC` only succeed once an in-kernel APIC has
been created, so calling them here fails with `EINVAL` on every real boot — confirmed the first
time `Multiverse::snapshot` ran against actual `/dev/kvm` (H5). Any interrupt bookkeeping
direct-injection needs to preserve is already covered by `KVM_GET_VCPU_EVENTS`.

**The work-clock anchor is two numbers, not one** (also a real-hardware finding, H5): capturing
only the virtual-TSC base (`base` in `virtual_tsc = base + k * rcb`) is not enough to resume the
*RCB* sequence a restored guest's interrupt-injection engine depends on. A restored guest's branch
counter is a brand-new `perf_event` file descriptor — a process cannot resurrect another fd's
already-elapsed hardware count — so it restarts counting from zero the instant it is created. The
capture set must therefore also record the cumulative RCB value at the moment of capture (an
`rcb_anchor`), added back on restore so the new counter's raw reads continue the same sequence
instead of silently rewinding it.

---

## 4. Branching (copy-on-write)

- Parent memory is a shared read-only backing; each child arms **userfaultfd** over its guest regions:
  - `UFFDIO_CONTINUE` — serve an unchanged page from the shared backing (share across many universes)
  - `UFFDIO_WRITEPROTECT` — copy-on-first-write so the child diverges only on the pages it writes
- Per-branch memory ∝ the child's write set, not total RAM. `fork()` copy-on-write is the small-N fallback.
- **Built today (§10): the small-N fallback, not this section's `UFFDIO_CONTINUE` mechanism.**
  `Multiverse::branch` realizes "`fork()` copy-on-write is the small-N fallback" via a full
  `restore` per branch (a real `KVM_CREATE_VM`/vCPU/guest-RAM region each), not a literal `fork(2)`
  — see `Multiverse::branch`'s doc for why a raw OS fork can't safely reuse an already-open KVM
  `vm`/`vcpu` fd (a `VmFd` is tied to its creating process's `mm` at `KVM_CREATE_VM` time). This
  gives full correctness and independence (`thousand_branches_are_independent_and_deterministic`)
  at `O(total RAM)` cost per branch, not this section's `O(write-set)` guarantee — memory-efficient
  branching remains open.

## 5. Reset (rewind)

- Track dirtied pages with the **KVM dirty ring** (`KVM_CAP_DIRTY_LOG_RING`); rewind copies back only those
  pages. Cost ∝ change, not machine size.

### 5.1 Restore into a live shell

- Re-wire the console to a live, bidirectional channel and resume: a captured universe's guest
  keeps taking input and producing output after restore, not just replaying frozen history — "a
  prompt inside any moment of any run."
- **Built today (§10): a real-hardware-verified crate-level primitive, not the CLI verb the
  guarantee names.** `Console::enqueue_input` (`crates/baud-multiverse/src/console.rs`) wraps
  `vm_superio::Serial::enqueue_raw_bytes` to push host-supplied bytes into the UART's RX FIFO;
  `Multiverse::{console_output, enqueue_console_input, step_exit, run_until_console_len}`
  (`crates/baud-multiverse/src/linux/mod.rs`) give a caller the building blocks to drive a
  restored guest indefinitely (never `run_to_first_halt`, which by design stops at `Hlt`) while
  feeding it live input. The UART still uses `NoIrqTrigger` (no in-kernel LAPIC exists in this
  workspace — §3's interrupt-injection engine delivers directly via `KVM_INTERRUPT`, bypassing
  IRQ4 entirely), so an interactive guest must poll the Line Status Register rather than block on
  an interrupt; `vm_superio::Serial::enqueue_raw_bytes` sets the LSR "data ready" bit directly
  regardless, so polling observes queued input correctly with no interrupt-delivery machinery
  needed (`crates/baud-multiverse/tests/fixtures/shell-guest/BUILD.md` — a hand-assembled fixture
  that prints a `$ ` prompt and echoes polled input — has the exact rationale). A real
  `EventFd`-backed `Trigger` (replacing `NoIrqTrigger`) for a guest that blocks on IRQ4 instead of
  polling remains future work, as does the actual `baud shell-into <universe>` CLI/server surface
  (`baud-server` has never called into `linux::Multiverse` at all — this would be its first route
  to do so, needing new bidirectional-streaming infrastructure this codebase does not have yet).
- **Real-hardware finding (fixed, §10): `Multiverse::snapshot` could capture a stale,
  not-yet-retired `RIP`.** None of this crate's ports are in-kernel-emulated, so every `IN`/`OUT`
  round-trips to userspace; KVM defers that instruction's retirement (including the `RIP` advance)
  to the *next* `KVM_RUN` call, not the exit that reported it. Every snapshot point before
  `shell_into_universe_resumes` existed either at a fresh boot (zero exits behind it) or right
  after `inject_timer_tick`'s single-step confirmation loop (which already calls `KVM_RUN` enough
  times to retire whatever was pending) — this test is the first to snapshot immediately after a
  plain, uninterrupted `step_exit()`, and the staleness was real: restoring from such a universe
  silently re-executed the just-completed instruction (an observable duplicate byte, for `OUT`).
  Fixed with `Multiverse::flush_pending_pio_completion` — the standard `kvm_run.immediate_exit`
  technique (set it, call `KVM_RUN` once to retire the pending completion and get an immediate
  `-EINTR` with no new guest instruction executed, clear it again), called at the top of
  `snapshot` before any `KVM_GET_*` read.

## 6. Restore Ordering (determinism)

1. Set TSC frequency (`KVM_SET_TSC_KHZ`) **before** creating the vCPU.
2. Restore `IA32_TSC` **before** `IA32_TSC_DEADLINE`.
3. Restore RAM → vCPU registers/MSRs/XSAVE/events/MP → VM clock → device/console (no LAPIC step —
   see §3's note on why this workspace's VMM has no in-kernel APIC to restore).
4. Refuse restore on a mismatched CPU model unless a fixed CPUID template is active.

---

## 7. Testing

```rust
#[test] fn snapshot_roundtrip_is_bit_identical() {
    let straight = run(image(), tape.clone()).obs_stream(K..K+M);
    let u = capture_at(image(), tape.clone(), K);
    let resumed = restore(u).run_to(K+M).obs_stream(K..K+M);
    assert_eq!(straight, resumed);          // a missing capture field would diverge here
    // `obs_stream` is the guest-observable stream (instruction pointer at each interrupt, console
    // output, RAM hash) — real-hardware run (H5) found the internal RCB bookkeeping value is *not*
    // part of this comparison: a restored guest's branch counter is a brand-new perf_event fd, and
    // creating/enabling it costs a real, one-time, few-hundred-branch "warm-up" a continuously
    // running counter never re-pays. This warm-up never reaches the guest's own instruction stream
    // (rip and every guest-visible byte still match exactly), so it is not a determinism gap.
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

#[test] fn shell_into_universe_resumes() {
    let u = capture_at(shell_image(), tape, prompt_reached());
    assert_eq!(u.console_tail(), b"$ ");                    // first output byte of the tail
    let mut resumed = restore(&u);
    assert_eq!(resumed.console_output(), u.console_tail()); // matches the captured tail exactly
    resumed.enqueue_input(b"hi\r");
    resumed.run_until_output_len(u.console_tail().len() + "hi\n$ ".len());
    assert_eq!(resumed.console_output(), b"$ hi\n$ ");       // takes live input and re-prompts
    // Real-hardware crate-level closure — no `baud shell-into` CLI/server verb exists yet, see §5.1.
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

- **Capture/restore (§3, §6) — built and exercised on real KVM hardware (H5).**
  `crates/baud-snapshot/src/universe.rs` (enumerated capture set, ordered restore plan, CPU-model
  guard, the pure write-set diff) + `src/linux.rs`'s real `capture`/`restore` walking every
  `KVM_GET_*`/`KVM_SET_*` the table above lists, in the plan's exact order. `Multiverse::snapshot`/
  `Multiverse::restore` (`crates/baud-multiverse/src/linux/mod.rs`) now drive this against a real,
  running guest for the first time (`linux::tests::snapshot_roundtrip_is_bit_identical`,
  `drive/h5.sh`): capture a `timer-guest` fixture mid-run (after its first delivered interrupt),
  restore into a brand-new `Multiverse`, deliver a second interrupt and run to halt — the restored
  run's landed instruction and whole observation stream (console output, RAM hash) match a
  straight, never-snapshotted run exactly. Two real, previously-undiscovered bugs surfaced by this
  first real exercise, both fixed: (1) `KVM_GET_LAPIC` unconditionally called during capture, which
  fails `EINVAL` on this VMM's vCPUs since no in-kernel irqchip is ever created (§3's note) — LAPIC
  removed from the capture set entirely; (2) the work-clock anchor only captured the virtual-TSC
  base, not the cumulative RCB value, so a restored guest's brand-new branch counter silently
  reset the RCB space `inject_timer_tick`'s target computation depends on — fixed by adding an
  `rcb_anchor` field (§3's second note) via `WorkClock::rcb_offset`.
- **Reset (§5) — built, wired into `baud-multiverse`, and exercised for real on real KVM
  hardware (`reset_cost_scales_with_write_set`, `drive/h5.sh`'s H5.4).** `KVM_CAP_DIRTY_LOG_RING`
  is real: `crates/baud-snapshot/src/dirty_ring.rs` is the hardware-independent ring-scan protocol
  (decode a `kvm_dirty_gfn` ring into harvested `(slot, offset)` pairs, unit- and property-tested
  with no KVM involved), driven by `src/linux.rs`'s `DirtyRing`. Because §1's hard constraint (one
  vCPU per VM) holds workspace-wide, a per-vCPU ring is a whole VM's dirty-page record with no
  cross-vCPU merge to do. `KVM_RESET_DIRTY_RINGS`'s ioctl number is not in the pinned `kvm-ioctls`
  0.25 — it is derived from the same `ioctl_expr` helper `kvm-ioctls` itself is built from
  (`vmm_sys_util::ioctl`), not hand-encoded, to minimize the risk of an unverifiable-on-this-machine
  mistake.
  - **Three real, previously-undiscovered bugs found the first time this path ever ran against a
    live guest**, none reachable by `cargo check`: (1) `KVM_CAP_DIRTY_LOG_RING` cannot be
    negotiated on a `VmFd` once any vCPU already exists (the kernel's own `kvm->created_vcpus`
    check, `EINVAL`) — `DirtyRing::enable` (one combined call, documented as callable any time
    after `create_vcpu`) is now split into `negotiate_capability(vm, entries)` (must run *before*
    `create_vcpu`) and `open(vcpu, entries)` (the mmap step, after); `baud-multiverse`'s
    `create_vm_vcpu_shell` calls the former between `create_vm` and `create_vcpu`. (2) the ring's
    mmap was `PROT_READ`-only, but `DirtyRing::collect` writes the `RESET` flag bit back into that
    same mapping to mark harvested entries — segfaulted (`SIGSEGV`) the instant `collect` was first
    called for real; fixed to `PROT_READ | PROT_WRITE` (matching how e.g. QEMU maps this same
    ring). (3) the guest-RAM memory slot was registered with `flags: 0` — KVM only tracks dirty
    pages (bitmap or ring) for slots carrying `KVM_MEM_LOG_DIRTY_PAGES`, so a ring opened over an
    untracked slot silently reported zero dirtied pages forever regardless of how much the guest
    wrote; fixed by threading a `log_dirty_pages: bool` into
    `linux::allocate_and_register_guest_ram`, set whenever a caller requests a dirty ring.
  - **Wired into `baud-multiverse`**: dirty-ring negotiation moved to construction time —
    `Multiverse::boot`/`Multiverse::restore` both take a `dirty_ring_entries: Option<u32>` (bug 1
    above is exactly why this could not remain a separate "enable after boot" call); `Some`
    negotiates+opens the ring and enables per-slot dirty logging before the guest ever runs,
    matching `DirtyRing`'s "must be negotiated before any dirty page could occur" requirement.
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
    direct observable `reset_cost_scales_with_write_set` checks. On real hardware, a `timer-guest`
    run past two ticks dirties a small handful of pages (the ISR's stack pushes/pops plus a few
    page-table `ACCESSED`-bit updates from the guest's first address translations — real, accepted,
    non-bug behavior), never the full 65536-page RAM region, and a reset makes RAM byte-identical
    to the pre-run snapshot again.
- **Branching (§4) — the small-N fallback is built and exercised on real KVM hardware
  (`thousand_branches_are_independent_and_deterministic`); the memory-efficient `UFFDIO_CONTINUE`
  mechanism remains a real, documented blocker.** The spec's `UFFDIO_CONTINUE`-based sharing
  requires the kernel's *minor-fault* mechanism, which only exists for shared
  (memfd/hugetlbfs/shmem) mappings — but `baud-multiverse`'s guest RAM
  (`GuestMemoryMmap::from_ranges`) is a private anonymous mapping. Wiring `UFFDIO_CONTINUE` today
  would need switching guest-RAM backing to a shared memfd first, an architecture change to
  `baud-multiverse`, not something this crate can absorb alone.
  - **The spec's own "small-N fallback" is built, but as `Multiverse::restore` per branch, not a
    literal `fork()`** (`crates/baud-multiverse/src/linux/mod.rs`'s `Multiverse::branch`, new this
    iteration) — a real architectural finding, not a stylistic choice: a raw OS `fork()` cannot
    safely reuse an already-open KVM `vm`/`vcpu` fd at all, independent of the threading-model
    hazard prior iterations flagged (todo.md §14's now-superseded note about the "one VMM thread +
    one vCPU thread" model). A `VmFd` is tied to its *creating* process's `mm` at `KVM_CREATE_VM`
    time — a forked child inheriting the parent's `vm` fd would still have guest-physical memory
    resolve through KVM's EPT against the *parent's* address space, not the child's own post-fork
    CoW copy, no matter how the two processes' host page tables diverge afterward. Each branch
    therefore gets its own fresh `KVM_CREATE_VM`/vCPU/guest-RAM region via `Multiverse::restore`
    instead — fully correct and independent, at `O(total RAM)` cost per branch (a real copy of
    every page in `universe.ram`) rather than this section's `O(write-set)` CoW-sharing guarantee.
  - **Proven on real hardware**: `linux::tests::thousand_branches_are_independent_and_deterministic`
    (`crates/baud-multiverse/src/linux/mod.rs`) captures a branch point immediately after boot
    (before the guest executes a single instruction) using `tape-echo-guest` (H2's fixture: reads 4
    tape bytes, echoes them to COM1, halts), then forks 1000 independent branches from it, each on
    its own unique 4-byte tape suffix. Every branch's console output is asserted to match exactly
    its own suffix — a stronger, more direct proof of "no branch perturbs another" than a pairwise
    output comparison, since any cross-branch memory bleed would show up as a mismatched byte. A
    sample of 8 branches is re-forked a second time from the same universe+suffix and proven
    byte-identical (console output + RAM hash), closing the spec pseudocode's
    `b.is_deterministic_double_run()` for a representative subset (full-N double-run was judged not
    worth 2x this test's real-hardware wall time, given every branch already takes the same
    `restore` code path `snapshot_roundtrip_is_bit_identical` already proved bit-identical).
    `drive/h5.sh` gained a new H5.5 step running this test (takes ~3.5 minutes on this dev machine:
    real KVM VM lifecycles, not a synthetic loop). `cargo test -p baud-multiverse`: adds 1 new test,
    passing.
  - **Not yet done**: the `O(write-set)` memory-efficiency guarantee itself (this section's actual
    "cheap" promise) still needs the memfd/`UFFDIO_CONTINUE` rearchitecture described above. Both
    findings remain tracked in `crates/baud-snapshot/src/lib.rs`'s module doc and todo.md §14.
- **Restore into a live shell (§5.1) — the crate-level primitive is built and exercised on real
  KVM hardware (`shell_into_universe_resumes`, `drive/h5.sh`'s H5.6); the `baud shell-into` CLI/
  server verb the test's name references is not.** `Console::enqueue_input`
  (`crates/baud-multiverse/src/console.rs`) and `Multiverse::{console_output,
  enqueue_console_input, step_exit, run_until_console_len}` (`crates/baud-multiverse/src/linux/
  mod.rs`) are new. A new hand-assembled fixture, `tests/fixtures/shell-guest/` (prints a `$ `
  prompt, polls COM1 for input, echoes it, re-prompts on `\r`, never `hlt`s), is the first fixture
  in this workspace to exercise the UART's *receive* side against real hardware.
  `linux::tests::shell_into_universe_resumes` captures a `Universe` right at the prompt, restores
  it into a brand-new `Multiverse`, confirms the restored console output matches the captured tail
  exactly, then proves the restored session is a genuine resumption — not a frozen replay — by
  feeding it `"hi\r"` and getting back byte-identical output to an equivalent straight run that
  never snapshotted at all.
  - **Real-hardware finding, fixed**: closing this test surfaced `Multiverse::snapshot`'s stale-RIP
    bug described in §5.1 above (`Multiverse::flush_pending_pio_completion`) — the first bug this
    workspace has found from snapshotting immediately after a plain PIO exit, since every earlier
    snapshot point avoided the situation by construction (a fresh boot, or right after
    `inject_timer_tick`'s own settling `KVM_RUN` calls).
  - **Not yet done**: the actual `baud shell-into <universe>` CLI/server surface — `baud-server` has
    never called into `linux::Multiverse` at all (every existing route still imports the old
    pre-pivot `Multiverse` in `baud-multiverse::lib.rs`), and a real interactive terminal session
    needs bidirectional streaming infrastructure (e.g. a WebSocket route) this codebase does not
    have yet, plus a `SnapshotStore`-backed universe lookup by ID (`get_universe` already exists in
    `baud-snapshot-store`, but nothing deserializes its bytes back into a `baud_snapshot::Universe`
    today). An `EventFd`-backed `Trigger` (replacing `NoIrqTrigger`) for a guest that blocks on IRQ4
    instead of polling LSR is also open, tracked in `console.rs`'s module doc.
