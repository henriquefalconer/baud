// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-snapshot-store — specs/baud-snapshot-store.md
//
// The durable record of a run: the tree of universes, the tape, and the run manifest,
// content-addressed and age-encrypted at rest (§1). Supersedes `baud-journal`'s
// replay-from-zero design — reconstruction/shrinking fork from the nearest stored universe
// instead of replaying a prefix (§5).
//
// Hardware-independent by construction (§1's Non-Goal: "Capturing/restoring VM state"; §2's
// pinned deps have no KVM/perf crate in them at all) — this crate never touches a guest or a
// vCPU, so unlike `baud-multiverse`/`baud-vcpu`/`baud-snapshot`/`baud-host` it has no
// `cfg(target_os = "linux")` half and no "not yet exercised on real KVM hardware" caveat: every
// test here runs and proves the real behavior on this Windows dev machine, no cross-target check
// needed.

mod error;
mod store;
mod types;

pub use error::StoreError;
pub use store::SnapshotStore;
pub use types::{Node, NodeId, PageRef, RunId, RunManifest, Sha};

#[cfg(test)]
mod tests;
