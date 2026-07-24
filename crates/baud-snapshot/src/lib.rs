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
// Built this iteration (todo.md §14's "not yet started: baud-snapshot ... crate does not exist
// yet" — this closes that gap for the hardware-independent core plus a real capture/restore):
//   - `page_store` — content-addressed guest-RAM pages, shared across universes by content hash
//     (specs/baud-snapshot.md §4's "per-branch memory ∝ write set" starts here: unchanged pages
//     between two captures are the *same* `Arc`, not a copy).
//   - `universe` — the enumerated capture set (`Universe`/`VcpuState`/`ClockState`/`DeviceState`,
//     specs/baud-snapshot.md §3), the ordered restore plan (§6), MSR-ordering (TSC before
//     TSC_DEADLINE), the CPU-model-match guard (`restore_refuses_mismatched_cpu`), and the dirty-page
//     diff a rewind's cost is proportional to (§5's "reset cost scales with write-set" guarantee,
//     the pure planning half of `KVM_CAP_DIRTY_LOG_RING`).
//   - `tree` — in-memory branch-point bookkeeping (parent/child snapshot links, nearest-ancestor
//     lookup) so exploration/shrinking can fork from the nearest snapshot instead of from boot
//     (§5's `shrink_reproduces_from_nearest_snapshot`, todo.md problem #22). Durable persistence of
//     this tree is `baud-snapshot-store`'s job (not this crate, per the architecture diagram).
//   - `msr` — the three TSC-family MSR numbers both this crate's restore-ordering logic and
//     `baud-multiverse`'s work-clock need to agree on.
//   - `linux` (cfg(target_os = "linux")) — the real `KVM_GET_*`/`KVM_SET_*` capture/restore calls
//     enumerated in specs/baud-snapshot.md §3, walking the `universe::restore_plan` in order.
//     Type-checked via `cargo check --target x86_64-unknown-linux-gnu -p baud-snapshot`, not yet
//     exercised on real KVM hardware (same caveat as every other `linux/` module in this
//     workspace — CLAUDE.md).
//
// Deliberately NOT built this iteration (see `linux`'s module doc for why, and todo.md §14 for the
// tracked next action): userfaultfd-based CoW branching (`Snapshot::branch`) and
// `KVM_CAP_DIRTY_LOG_RING`-based cheap reset (`Snapshot::reset`) — the real ioctl wiring for both,
// as opposed to the pure write-set-diff planning logic in `universe::dirty_pages` which IS built
// and tested here.

#![allow(dead_code)]

pub mod msr;
pub mod page_store;
pub mod tree;
pub mod universe;

#[cfg(target_os = "linux")]
pub mod linux;

pub use page_store::{PageHash, PageRef, PageStore, PAGE_SIZE};
pub use tree::{NodeId, Tree};
pub use universe::{ClockState, DeviceState, MsrWrite, Universe, VcpuState};
