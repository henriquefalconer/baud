// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// [`crate::SnapshotStore::open`] could not resolve an age recipient/identity from
    /// `baud-keys` (specs/baud-snapshot-store.md §4: "Missing key: store is unreadable").
    #[error("no age key configured (baud-keys::age_public_key/age_key_path returned nothing) — the store is unreadable/unwritable without one")]
    MissingKey,
    #[error("node {0} not found")]
    NodeNotFound(String),
    /// [`crate::SnapshotStore::get_universe`] on a node whose [`crate::Node::universe`] is
    /// `None` — a pure branch point with no captured state; call
    /// [`crate::SnapshotStore::nearest`] first.
    #[error("node {0} has no captured universe (it is a branch-point-only node)")]
    NoUniverseAtNode(String),
    #[error("no ancestor of {0} has a captured universe")]
    NoAncestorWithUniverse(String),
    #[error("run manifest not found for run {0}")]
    ManifestNotFound(String),
    #[error("malformed hash: {0}")]
    BadHash(String),
    #[error("content hash mismatch for {kind}: expected {expected}, got {actual}")]
    IntegrityMismatch { kind: &'static str, expected: String, actual: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json encode/decode error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cbor encode error: {0}")]
    Encode(#[from] baud_proto::EncodeError),
    #[error("cbor decode error: {0}")]
    Decode(#[from] baud_proto::DecodeError),
    #[error(transparent)]
    Keys(#[from] baud_keys::KeysError),
}
