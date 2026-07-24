// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// specs/baud-snapshot-store.md §3's Model, translated from the spec's Rust-pseudocode `Node`/
// `RunManifest` into the real types this crate persists. Two deliberate departures from the
// literal pseudocode, both documented where they matter:
//
//   1. `Node::universe` is `Option<Hash>`, not `Hash` — not every branch point in the tree has a
//      captured universe (capturing a full VM state is comparatively expensive; a guest emitting
//      `MARK_BRANCH` far more often than the driver chooses to snapshot is the normal case, per
//      specs/baud-tape-device.md §4 and baud-proto's `Msg::MarkBranch`). `None` marks a
//      branch-point-only node; [`crate::SnapshotStore::nearest`] walks past these to find the
//      nearest ancestor that actually has one, which is what makes §5's "fork from nearest, not
//      root" guarantee non-trivial to test.
//   2. `RunManifest::regime` is a `String`, not `baud_host::Regime` — this crate's declared
//      dependency set (§2: "Deps = {blake3, baud-keys, baud-proto}") deliberately excludes
//      `baud-host`, and this crate's job is archival, not interpretation (§1's Non-Goal:
//      "Interpreting workload semantics" — the regime tag is exactly that kind of semantics).
//      `baud_host::Regime` remains the single source of truth for the enum itself; callers that
//      have one (i.e. `baud-server`, which already depends on both crates) pass
//      `regime.to_string()` in and parse it back out on the other side.

use std::fmt;

/// A blake3 content hash — of a universe body, a page body, or (via [`Sha::of_node_identity`])
/// the fields that make a tree node content-addressed. 32 bytes, hex-encoded for filenames/JSON
/// so the on-disk index stays human-inspectable (specs/baud-snapshot-store.md §4: "In clear: only
/// the (run, node) index + addresses").
#[derive(Clone, Copy, PartialEq, Eq, std::hash::Hash, Debug)]
pub struct Sha([u8; 32]);

impl Sha {
    pub fn of(bytes: &[u8]) -> Self {
        Sha(*blake3::hash(bytes).as_bytes())
    }

    pub fn to_hex(self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }

    pub fn from_hex(s: &str) -> Result<Self, crate::StoreError> {
        let hash = blake3::Hash::from_hex(s)
            .map_err(|e| crate::StoreError::BadHash(format!("{s}: {e}")))?;
        Ok(Sha(*hash.as_bytes()))
    }

    /// Content-address a tree node from the fields that define its identity: which parent it
    /// forked from, at which tape step, and over which tape range. Two `put_universe`/
    /// `mark_branch` calls with identical `(parent, at_step, tape_range)` — e.g. re-running the
    /// same deterministic prefix twice — collapse onto the same [`NodeId`], the same
    /// content-addressing philosophy §4 already applies to page/universe bodies.
    pub fn of_node_identity(parent: Option<Sha>, at_step: u64, tape_range: (u64, u64)) -> Self {
        let mut buf = Vec::with_capacity(1 + 32 + 8 + 8 + 8);
        match parent {
            Some(p) => {
                buf.push(1u8);
                buf.extend_from_slice(&p.0);
            }
            None => buf.push(0u8),
        }
        buf.extend_from_slice(&at_step.to_le_bytes());
        buf.extend_from_slice(&tape_range.0.to_le_bytes());
        buf.extend_from_slice(&tape_range.1.to_le_bytes());
        Sha::of(&buf)
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// A run identifier — caller-chosen (e.g. a UUID, or the run's own seed rendered as text), scoped
/// to one directory under the store root. Sanitized before touching the filesystem
/// ([`crate::store::sanitize_component`]) so a hostile/malformed run id cannot escape the store
/// root via `..`/path separators.
#[derive(Clone, PartialEq, Eq, std::hash::Hash, Debug)]
pub struct RunId(pub String);

impl RunId {
    pub fn new(id: impl Into<String>) -> Self {
        RunId(id.into())
    }
}

/// Identifies one node in a run's branch tree. A content hash ([`Sha::of_node_identity`]), not a
/// counter — see that function's doc for why.
pub type NodeId = Sha;

/// One node in the branch tree (specs/baud-snapshot-store.md §3). Stored **in clear** (JSON) —
/// only `universe`/page bodies are ever encrypted (§4).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Node {
    pub id: String,
    pub parent: Option<String>,
    pub at_step: u64,
    /// Hex hash of the captured universe body, or `None` if this node is a branch point with no
    /// stored capture yet (see this module's doc comment, point 1).
    pub universe: Option<String>,
    pub tape_range: (u64, u64),
}

/// The run-level manifest (specs/baud-snapshot-store.md §3). Stored in clear at
/// `runs/<run>/manifest.json` — none of these fields are secret.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunManifest {
    pub seed: u64,
    pub image_hash: String,
    /// See this module's doc comment, point 2, for why this is a string.
    pub regime: String,
    pub root: String,
}

/// A handle to one content-addressed, encrypted page body (specs/baud-snapshot-store.md §3's
/// `put_page` return value). `address` is the hash of the *plaintext* (§4: "Deduplication: keyed
/// by plaintext hash — age is non-deterministic, so ciphertext can't dedup").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageRef {
    pub address: Sha,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_identity_is_deterministic_given_same_fields() {
        let a = Sha::of_node_identity(None, 10, (0, 10));
        let b = Sha::of_node_identity(None, 10, (0, 10));
        assert_eq!(a, b);
    }

    #[test]
    fn node_identity_differs_on_any_field_change() {
        let base = Sha::of_node_identity(None, 10, (0, 10));
        assert_ne!(base, Sha::of_node_identity(None, 11, (0, 10)), "at_step must matter");
        assert_ne!(base, Sha::of_node_identity(None, 10, (0, 11)), "tape_range must matter");
        let parent = Sha::of(b"some parent");
        assert_ne!(base, Sha::of_node_identity(Some(parent), 10, (0, 10)), "parent must matter");
    }

    #[test]
    fn hex_roundtrips() {
        let h = Sha::of(b"hello");
        let hex = h.to_hex();
        assert_eq!(Sha::from_hex(&hex).unwrap(), h);
    }

    #[test]
    fn from_hex_rejects_garbage() {
        assert!(Sha::from_hex("not-a-hash").is_err());
    }
}
