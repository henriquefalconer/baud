// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-snapshot (specs/baud-snapshot.md, todo.md §5) — captures a complete VM state (a
// **universe**: guest RAM + all vCPU state + device state + work-clock), restores it, and forks
// many continuations that share memory copy-on-write. This is the mechanism behind the branching
// multiverse; it replaces replay-from-zero reconstruction (`baud-journal`'s old model) with
// O(write-set) branching instead of O(prefix) replay.
//
// Layering (specs/baud-snapshot.md §2's architecture diagram: "used by baud-multiverse /
// baud-driver; persisted by baud-snapshot-store"): this crate sits *below* `baud-multiverse` in
// the dependency graph — `baud-multiverse` will depend on `baud-snapshot` (not the reverse), which
// is why the MSR constants both crates need (`msr` module) live here as the single source of
// truth and `baud-multiverse::timesource` re-exports them rather than duplicating the values.
//
// Built (todo.md §14's "not yet started: baud-snapshot ... crate does not exist yet" — this closes
// that gap for the hardware-independent core plus real capture/restore/reset):
//   - `page_store` — content-addressed guest-RAM pages, shared across universes by content hash
//     (specs/baud-snapshot.md §4's "per-branch memory ∝ write set" starts here: unchanged pages
//     between two captures are the *same* `Arc`, not a copy).
//   - `universe` — the enumerated capture set (`Universe`/`VcpuState`/`ClockState`/`DeviceState`,
//     specs/baud-snapshot.md §3), the ordered restore plan (§6), MSR-ordering (TSC before
//     TSC_DEADLINE), the CPU-model-match guard (`restore_refuses_mismatched_cpu`), and the dirty-page
//     diff a rewind's cost is proportional to (§5's "reset cost scales with write-set" guarantee,
//     the pure planning half of the page-content-diff path).
//   - `dirty_ring` — the pure ring-buffer-decoding half of `KVM_CAP_DIRTY_LOG_RING`-based reset
//     (§5): scans a `kvm_dirty_gfn` ring for newly-dirtied `(slot, offset)` pairs and marks them
//     harvested, hardware-independent (no mmap/ioctl, just slice scanning) so the "reset cost
//     scales with the dirty set, not total RAM" guarantee is unit-tested here directly.
//   - `tree` — in-memory branch-point bookkeeping (parent/child snapshot links, nearest-ancestor
//     lookup) so exploration/shrinking can fork from the nearest snapshot instead of from boot
//     (§5's `shrink_reproduces_from_nearest_snapshot`, todo.md problem #22). Durable persistence of
//     this tree is `baud-snapshot-store`'s job (not this crate, per the architecture diagram).
//   - `msr` — the three TSC-family MSR numbers both this crate's restore-ordering logic and
//     `baud-multiverse`'s work-clock need to agree on.
//   - `linux` (cfg(target_os = "linux")) — the real `KVM_GET_*`/`KVM_SET_*` capture/restore calls
//     enumerated in specs/baud-snapshot.md §3, walking the `universe::restore_plan` in order, plus
//     `DirtyRing` (real `KVM_CAP_DIRTY_LOG_RING` enable/mmap/harvest/`KVM_RESET_DIRTY_RINGS`,
//     specs/baud-snapshot.md §5's "reset" guarantee — `dirty_ring`'s pure scan logic driven by a
//     real per-vCPU ring mmap). Type-checked via `cargo check --target x86_64-unknown-linux-gnu -p
//     baud-snapshot`, not yet exercised on real KVM hardware (same caveat as every other `linux/`
//     module in this workspace — CLAUDE.md).
//
// Branching (`Snapshot::branch`, specs/baud-snapshot.md §4) — the small-N fallback is built
// (`baud_multiverse::linux::Multiverse::branch`, one layer up, since it needs the KVM handles this
// crate deliberately doesn't own), the spec's actual memory-efficient mechanism is not:
//   - Built: specs/baud-snapshot.md §4's "`fork()` copy-on-write is the small-N fallback" is
//     realized as a full `Multiverse::restore` per branch, not a literal `fork(2)` — a raw OS
//     `fork()` cannot safely reuse an already-open KVM `vm`/`vcpu` fd at all (independent of any
//     threading-model concern): a `VmFd` is tied to its *creating* process's `mm` at
//     `KVM_CREATE_VM` time, so a forked child sharing the parent's `vm` fd would still have guest
//     memory resolve through KVM's EPT against the *parent's* address space, not the child's own
//     post-fork CoW pages. Proven correct and independent on real hardware
//     (`thousand_branches_are_independent_and_deterministic`, todo.md §14), at `O(total RAM)` cost
//     per branch rather than the spec's `O(write-set)` guarantee.
//   - NOT built: userfaultfd-based CoW branching, the spec's actual `O(write-set)` "cheap N-way
//     branching" mechanism — `UFFDIO_CONTINUE`-based page sharing needs guest RAM backed by a
//     shared (memfd/hugetlbfs) mapping for the kernel's "minor fault" mechanism, but today's guest
//     RAM (`baud_multiverse::linux::GuestMemory` = `GuestMemoryMmap::from_ranges`) is a private
//     anonymous mapping — a real architecture change, not just a missing ioctl wrapper, so it
//     remains scoped out rather than built on a foundation that cannot support it.

#![allow(dead_code)]

pub mod dirty_ring;
pub mod msr;
pub mod page_store;
pub mod tree;
pub mod universe;

#[cfg(target_os = "linux")]
pub mod linux;

pub use dirty_ring::{harvest, RawDirtyGfn, DIRTY_BIT, RESET_BIT};
pub use page_store::{PageHash, PageRef, PageStore, PAGE_SIZE};
pub use tree::{NodeId, Tree};
pub use universe::{ClockState, DeviceState, MsrWrite, Universe, VcpuState};
