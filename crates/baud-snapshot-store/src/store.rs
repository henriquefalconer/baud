// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// specs/baud-snapshot-store.md §3's `SnapshotStore` API, plus `put_tape`/`get_tape` and
// `put_records`/`get_records` (this crate's own extension, not literally in the spec's Rust
// pseudocode) — §1's Goals name "the tape" as part of what this crate durably records
// ("the durable record of a run: the tree of universes, the tape, and the run manifest"), and the
// spec's own dependency list (§2: deps include `baud-proto`) has no other use in this crate
// without them; `baud_proto::Msg` (`MarkBranch`/`Log`/`Observe`/`Outcome`/...) is exactly what a
// guest's tape-device writes flow out as (specs/baud-tape-device.md §4, baud-multiverse's
// `drain_tape_records()`), so persisting them per-node is the natural audit trail this store
// exists to keep.
//
// Layout on disk, one directory tree per store root:
//   <root>/runs/<sanitized run id>/
//     manifest.json         -- RunManifest, in clear (§4: only bodies are ciphertext)
//     tape.age               -- the run's whole tape, age-encrypted (may contain guest-chosen
//                                bytes an attacker controls; still worth encrypting since a leaked
//                                tape reproduces exact guest execution, §1's "leaked store cannot
//                                reproduce guest execution")
//     nodes/<node id hex>.json     -- Node, in clear (the "(run, node) index", §4)
//     universes/<hash hex>.age     -- one captured universe body, age-encrypted
//     pages/<hash hex>.age         -- one content-addressed RAM/observation page, age-encrypted
//     records/<node id hex>.age    -- the guest's tape-device Msg records observed up to this
//                                      node, age-encrypted (CBOR via baud_proto::encode per Msg,
//                                      length-prefixed)
//     driver_state.age             -- the run's latest baud_driver::DriverState, age-encrypted,
//                                      caller-serialized JSON (this crate does not depend on
//                                      baud-driver — same opaque-blob pattern as universes/pages)

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::StoreError;
use crate::types::{Node, NodeId, PageRef, RunId, RunManifest, Sha};

pub struct SnapshotStore {
    root: PathBuf,
    /// age recipient (`age1...`) — required for every `put_*` call.
    recipient: String,
    /// age identity file path — required for every `get_*` call. `None` means this store can
    /// write but never read back (a write-only/publish-only setup); every `get_*` call returns
    /// [`StoreError::MissingKey`] in that case, matching §4's "missing key: store is unreadable".
    identity_path: Option<PathBuf>,
}

impl SnapshotStore {
    /// Open a store rooted at `root`, resolving the age recipient/identity from `baud-keys`
    /// (specs/baud-snapshot-store.md §4: "Key source: age recipient (public) + identity (path)
    /// from `baud-keys`"). Fails with [`StoreError::MissingKey`] if no age key is configured —
    /// deliberately loud rather than silently creating an unreadable store.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let recipient = baud_keys::age_public_key().ok_or(StoreError::MissingKey)?;
        let identity_path = baud_keys::age_key_path();
        Ok(Self::open_with_keys(root, recipient, identity_path))
    }

    /// Open a store with an explicit recipient/identity, bypassing `baud-keys`' filesystem/env
    /// resolution — the constructor tests use (and any caller that already resolved its own
    /// keys, e.g. a per-run recipient per specs/baud-snapshot-store.md §8's "Future
    /// Considerations").
    pub fn open_with_keys(
        root: impl Into<PathBuf>,
        recipient: String,
        identity_path: Option<PathBuf>,
    ) -> Self {
        SnapshotStore { root: root.into(), recipient, identity_path }
    }

    // -- paths ---------------------------------------------------------------------------------

    fn run_dir(&self, run: &RunId) -> PathBuf {
        self.root.join("runs").join(sanitize_component(&run.0))
    }
    fn manifest_path(&self, run: &RunId) -> PathBuf {
        self.run_dir(run).join("manifest.json")
    }
    fn tape_path(&self, run: &RunId) -> PathBuf {
        self.run_dir(run).join("tape.age")
    }
    fn node_path(&self, run: &RunId, node: NodeId) -> PathBuf {
        self.run_dir(run).join("nodes").join(format!("{}.json", node.to_hex()))
    }
    fn universe_body_path(&self, run: &RunId, hash: Sha) -> PathBuf {
        self.run_dir(run).join("universes").join(format!("{}.age", hash.to_hex()))
    }
    fn page_body_path(&self, run: &RunId, hash: Sha) -> PathBuf {
        self.run_dir(run).join("pages").join(format!("{}.age", hash.to_hex()))
    }
    fn records_path(&self, run: &RunId, node: NodeId) -> PathBuf {
        self.run_dir(run).join("records").join(format!("{}.age", node.to_hex()))
    }
    fn driver_state_path(&self, run: &RunId) -> PathBuf {
        self.run_dir(run).join("driver_state.age")
    }

    // -- low-level encrypted-body helpers -------------------------------------------------------

    /// Encrypt `plaintext` and write it to `path` unless a body is already there — the
    /// content-addressing dedup point (specs/baud-snapshot-store.md §4: age ciphertext is
    /// non-deterministic, so dedup happens here, before encryption, by not re-encrypting when the
    /// plaintext-addressed path already exists — not by comparing ciphertext bytes).
    fn write_body_if_absent(&self, path: &Path, plaintext: &[u8]) -> Result<bool, StoreError> {
        if path.exists() {
            return Ok(false);
        }
        let ciphertext = baud_keys::age_encrypt(&self.recipient, plaintext)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, ciphertext)?;
        Ok(true)
    }

    fn read_and_decrypt(&self, path: &Path) -> Result<Vec<u8>, StoreError> {
        let identity_path = self.identity_path.as_ref().ok_or(StoreError::MissingKey)?;
        let ciphertext = fs::read(path)?;
        Ok(baud_keys::age_decrypt(identity_path, &ciphertext)?)
    }

    fn read_and_verify(&self, path: &Path, expected: Sha, kind: &'static str) -> Result<Vec<u8>, StoreError> {
        let plaintext = self.read_and_decrypt(path)?;
        let actual = Sha::of(&plaintext);
        if actual != expected {
            return Err(StoreError::IntegrityMismatch {
                kind,
                expected: expected.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(plaintext)
    }

    // -- manifest --------------------------------------------------------------------------------

    pub fn put_manifest(&self, run: &RunId, manifest: &RunManifest) -> Result<(), StoreError> {
        let path = self.manifest_path(run);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(manifest)?)?;
        Ok(())
    }

    pub fn get_manifest(&self, run: &RunId) -> Result<RunManifest, StoreError> {
        let path = self.manifest_path(run);
        let bytes = fs::read(&path)
            .map_err(|_| StoreError::ManifestNotFound(run.0.clone()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    // -- tape ------------------------------------------------------------------------------------

    /// Store the run's whole tape (age-encrypted — see this module's top doc comment for why).
    /// One tape per run; calling this again overwrites it (a run's tape is fixed once the run
    /// starts, per todo.md §2's "tape: ... Fully reproduces everything", but overwrite rather than
    /// refuse keeps this usable for an in-progress run whose tape is still being appended to by
    /// `baud-driver`).
    pub fn put_tape(&self, run: &RunId, tape: &[u8]) -> Result<(), StoreError> {
        let path = self.tape_path(run);
        let ciphertext = baud_keys::age_encrypt(&self.recipient, tape)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, ciphertext)?;
        Ok(())
    }

    pub fn get_tape(&self, run: &RunId) -> Result<Vec<u8>, StoreError> {
        self.read_and_decrypt(&self.tape_path(run))
    }

    // -- pages -----------------------------------------------------------------------------------

    /// Intern one content-addressed, encrypted page (specs/baud-snapshot-store.md §3's
    /// `put_page`). Returns the same [`PageRef`] for identical plaintext across any number of
    /// calls without re-encrypting (dedup by plaintext hash, §4).
    pub fn put_page(&self, run: &RunId, page: &[u8]) -> Result<PageRef, StoreError> {
        const PAGE_SIZE: usize = 4096;
        if page.len() != PAGE_SIZE {
            return Err(StoreError::InvalidLength { kind: "page", expected: PAGE_SIZE, actual: page.len() });
        }
        let hash = Sha::of(page);
        self.write_body_if_absent(&self.page_body_path(run, hash), page)?;
        Ok(PageRef { address: hash })
    }

    pub fn get_page(&self, run: &RunId, page: PageRef) -> Result<Vec<u8>, StoreError> {
        let bytes = self.read_and_verify(&self.page_body_path(run, page.address), page.address, "page")?;
        if bytes.len() != 4096 {
            return Err(StoreError::InvalidLength { kind: "page", expected: 4096, actual: bytes.len() });
        }
        Ok(bytes)
    }

    // -- nodes / universes -------------------------------------------------------------------------

    fn write_node(&self, run: &RunId, node: &Node) -> Result<(), StoreError> {
        let id = Sha::from_hex(&node.id)?;
        let path = self.node_path(run, id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(node)?)?;
        Ok(())
    }

    pub fn read_node(&self, run: &RunId, node: NodeId) -> Result<Node, StoreError> {
        let path = self.node_path(run, node);
        let bytes = fs::read(&path).map_err(|_| StoreError::NodeNotFound(node.to_hex()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Record a branch point with no captured universe yet (this crate's extension beyond the
    /// spec's literal pseudocode — see [`crate::types`]'s module doc, point 1). Idempotent: the
    /// same `(parent, at_step, tape_range)` always content-addresses to the same [`NodeId`]
    /// ([`Sha::of_node_identity`]), so calling this twice for the same branch point is a no-op
    /// the second time, not a duplicate.
    pub fn mark_branch(
        &self,
        run: &RunId,
        parent: Option<NodeId>,
        at_step: u64,
        tape_range: (u64, u64),
    ) -> Result<NodeId, StoreError> {
        let id = Sha::of_node_identity(parent, at_step, tape_range);
        // If a universe was already captured at this exact node, mark_branch must not clobber it
        // back to None.
        let existing_universe = self.read_node(run, id).ok().and_then(|n| n.universe);
        self.write_node(
            run,
            &Node {
                id: id.to_hex(),
                parent: parent.map(|p| p.to_hex()),
                at_step,
                universe: existing_universe,
                tape_range,
            },
        )?;
        Ok(id)
    }

    /// Capture a universe at a (possibly new) node (specs/baud-snapshot-store.md §3's
    /// `put_universe`). `body` is whatever the caller's own capture serialization produced (this
    /// crate does not know or need to know `baud-snapshot::Universe`'s layout — §1's Non-Goal:
    /// "Capturing/restoring VM state"). The universe body is deduplicated by plaintext hash across
    /// every node in the run (§4) — two nodes that captured byte-identical state share one
    /// encrypted body on disk.
    pub fn put_universe(
        &self,
        run: &RunId,
        parent: Option<NodeId>,
        at_step: u64,
        tape_range: (u64, u64),
        body: &[u8],
    ) -> Result<NodeId, StoreError> {
        let id = Sha::of_node_identity(parent, at_step, tape_range);
        let universe_hash = Sha::of(body);
        self.write_body_if_absent(&self.universe_body_path(run, universe_hash), body)?;
        self.write_node(
            run,
            &Node {
                id: id.to_hex(),
                parent: parent.map(|p| p.to_hex()),
                at_step,
                universe: Some(universe_hash.to_hex()),
                tape_range,
            },
        )?;
        Ok(id)
    }

    pub fn get_universe(&self, run: &RunId, node: NodeId) -> Result<Vec<u8>, StoreError> {
        let n = self.read_node(run, node)?;
        let hash_hex = n.universe.ok_or_else(|| StoreError::NoUniverseAtNode(node.to_hex()))?;
        let hash = Sha::from_hex(&hash_hex)?;
        self.read_and_verify(&self.universe_body_path(run, hash), hash, "universe")
    }

    /// Walk from `target` up through `parent` links to the nearest node (inclusive of `target`
    /// itself) that has a captured universe (specs/baud-snapshot-store.md §3's `nearest`,
    /// §5: "find the nearest ancestor node with a stored universe").
    pub fn nearest(&self, run: &RunId, target: NodeId) -> Result<NodeId, StoreError> {
        let mut current_id = target;
        loop {
            let current = self.read_node(run, current_id)?;
            if current.universe.is_some() {
                return Ok(current_id);
            }
            match current.parent {
                Some(parent_hex) => current_id = Sha::from_hex(&parent_hex)?,
                None => return Err(StoreError::NoAncestorWithUniverse(target.to_hex())),
            }
        }
    }

    /// How many tape steps would need replaying to reach `target` from the nearest stored
    /// universe (specs/baud-snapshot-store.md §5: "O(local range), not O(prefix)"). This crate
    /// only reports the count — actually replaying is `baud-snapshot`'s restore plus
    /// `baud-multiverse`'s run loop, outside this crate's Non-Goal boundary (§1).
    pub fn reconstruct(&self, run: &RunId, target: NodeId) -> Result<u64, StoreError> {
        let nearest_id = self.nearest(run, target)?;
        let nearest_node = self.read_node(run, nearest_id)?;
        let target_node = self.read_node(run, target)?;
        Ok(target_node.at_step.saturating_sub(nearest_node.at_step))
    }

    // -- tape-device observation records -----------------------------------------------------------

    /// Persist the guest's tape-device records ([`baud_proto::Msg`], the wire type
    /// specs/baud-tape-device.md §4's opcodes decode to) observed up to `node`, encrypted (they
    /// may carry guest-chosen `Log`/`Observe` payloads, i.e. potential secrets). Each `Msg` is
    /// encoded with `baud_proto::encode` (length-prefixed so multiple messages concatenate into
    /// one plaintext blob before encryption) — reusing baud-proto's own wire encoding rather than
    /// inventing a second one, per this workspace's "single source of truth" rule.
    pub fn put_records(
        &self,
        run: &RunId,
        node: NodeId,
        records: &[baud_proto::Msg],
    ) -> Result<(), StoreError> {
        let mut plaintext = Vec::new();
        for msg in records {
            let encoded = baud_proto::encode(msg)?;
            plaintext.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            plaintext.extend_from_slice(&encoded);
        }
        let path = self.records_path(run, node);
        let ciphertext = baud_keys::age_encrypt(&self.recipient, &plaintext)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, ciphertext)?;
        Ok(())
    }

    // -- baud-driver exploration state -------------------------------------------------------------

    /// Persist a caller-opaque, serialized `baud_driver::DriverState` blob for `run` — one per run,
    /// overwritten on every call (like `put_tape`: the latest state supersedes the last, there is
    /// no history of intermediate driver states to keep). This crate does not depend on
    /// `baud-driver` and does not parse the bytes (this crate's own "does not know the caller's
    /// serialization" pattern — see `put_universe`'s doc); the caller (`baud-server`) is
    /// responsible for `serde_json::to_vec`/`from_slice` on `baud_driver::DriverState`.
    /// Age-encrypted like the tape: `best`/`reservoir` are recorded draw bytes, which may embed
    /// guest-influenced data the same way a tape does.
    pub fn put_driver_state(&self, run: &RunId, state: &[u8]) -> Result<(), StoreError> {
        let path = self.driver_state_path(run);
        let ciphertext = baud_keys::age_encrypt(&self.recipient, state)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, ciphertext)?;
        Ok(())
    }

    /// Whether `run` has a persisted driver state yet — callers use this to decide between a
    /// fresh `Driver::new` and `Driver::new` + `apply_state` without treating "no state yet" (the
    /// first generate call for a run) as an error the way a missing tape/universe would be.
    pub fn has_driver_state(&self, run: &RunId) -> bool {
        self.driver_state_path(run).exists()
    }

    pub fn get_driver_state(&self, run: &RunId) -> Result<Vec<u8>, StoreError> {
        self.read_and_decrypt(&self.driver_state_path(run))
    }

    pub fn get_records(&self, run: &RunId, node: NodeId) -> Result<Vec<baud_proto::Msg>, StoreError> {
        let plaintext = self.read_and_decrypt(&self.records_path(run, node))?;
        let mut records = Vec::new();
        let mut offset = 0usize;
        while offset < plaintext.len() {
            let end_of_prefix = offset.checked_add(4).ok_or_else(|| {
                StoreError::BadHash("records length prefix overflow".into())
            })?;
            let len_bytes: [u8; 4] = plaintext
                .get(offset..end_of_prefix)
                .ok_or_else(|| StoreError::BadHash("truncated records length prefix".into()))?
                .try_into()
                .expect("a four-byte slice has the requested length");
            let len = u32::from_le_bytes(len_bytes) as usize;
            offset = end_of_prefix;
            let end_of_record = offset.checked_add(len).ok_or_else(|| {
                StoreError::BadHash("records length overflow".into())
            })?;
            let encoded = plaintext
                .get(offset..end_of_record)
                .ok_or_else(|| StoreError::BadHash("truncated record body".into()))?;
            records.push(baud_proto::decode(encoded)?);
            offset = end_of_record;
        }
        Ok(records)
    }
}

/// Prevent a caller-chosen [`RunId`] from escaping the store root via `..`/path separators —
/// every non-alphanumeric byte except `-`/`_`/`.` becomes `_`.
pub(crate) fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}
