<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Snapshot Store Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-24

---

## 1. Overview

### Purpose

`baud-snapshot-store` is the durable record of a run: the tree of universes, the tape, and the run manifest,
content-addressed and age-encrypted at rest. It supersedes the replay-from-zero journal — reconstruction and
shrinking fork from the nearest stored universe instead of replaying a prefix.

### Goals

- **Durable tree**: universes + branch edges + tape, so any moment is reconstructable
- **Content-addressed**: identical universes/pages stored once
- **Encrypted at rest**: a leaked store cannot reproduce guest execution or secrets
- **Fork-from-nearest**: reconstruction is a tree lookup, not a linear replay

### Non-Goals

- Capturing/restoring VM state (that is `baud-snapshot`)
- Interpreting workload semantics

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│              baud-snapshot-store               │
│  content-addressed universes (blake3)          │
│  branch tree + tape · age-encrypted bodies     │
└───────────────────┬──────────────────────────┘
                    │ depends on
              baud-keys (age recipient + identity)
        ▲ owned by baud-server
```

### Rationale

- Deps = `{blake3, baud-keys, baud-proto}`. Append-only content-addressed files + one `(run, node)` index;
  no database. Owns no cryptography — encryption delegated to `age` via `baud-keys`.

---

## 3. Model

```rust
struct Node { id: Hash, parent: Option<Hash>, at_step: u64, universe: Hash, tape_range: (u64,u64) }
struct RunManifest { seed: u64, image_hash: Hash, regime: Regime, root: Hash }
```

- A **universe** body = the `baud-snapshot` capture, split into content-addressed pages (unchanged pages
  shared across nodes via blake3 address).
- The **tree** is nodes keyed by `(run, node id)`; edges are `parent`.

### API

```rust
impl SnapshotStore {
    pub fn put_universe(&self, run: RunId, node: NodeId, u: &Universe);  // age-encrypted, blake3-addressed
    pub fn put_page(&self, run: RunId, page: &[u8]) -> PageRef;          // dedup by plaintext hash
    pub fn get_universe(&self, run: RunId, node: NodeId) -> Universe;    // decrypt via baud-keys identity
    pub fn nearest(&self, target: NodeId) -> NodeId;                     // fork-from-nearest ancestor
    pub fn reconstruct(&self, target: NodeId) -> u64;                    // returns steps replayed (≤ LOCAL_RANGE)
}
```

---

## 4. Encryption at Rest

| Rule | Behavior |
| -------------------------- | ------------------------------------------------ |
| Key source                 | age recipient (public) + identity (path) from `baud-keys` |
| Content address            | `blake3(plaintext page)`; ciphertext stored under it |
| Deduplication              | keyed by plaintext hash (age is non-deterministic, so ciphertext can't dedup) |
| In clear                   | only the `(run, node)` index + addresses; bodies are always ciphertext |
| Missing key                | store is unreadable; the key that guards `infra/secrets` guards the store (`doctor` checks) |

---

## 5. Reconstruction & Shrinking

- To reproduce a moment: find the nearest ancestor node with a stored universe, restore it (`baud-snapshot`),
  and replay only the short tape range to the target. O(local range), not O(prefix).
- Shrinking edits the tape and re-runs from the nearest node; the deliverable is the smallest input+fault
  path reaching the finding.

---

## 6. Testing

```rust
#[test] fn snapshot_store_bodies_are_ciphertext() {
    store.put_universe(run, node, universe_with("sk-secret"));
    let raw = fs::read(body_path(run, node)).unwrap();
    assert!(!raw.windows(9).any(|w| w == b"sk-secret"));
}

#[test] fn pages_dedup_by_plaintext_hash() {
    let a = store.put_page(run, same_page());
    let b = store.put_page(run, same_page());
    assert_eq!(a.address, b.address);           // one stored body
}

#[test] fn reconstruct_forks_from_nearest_node() {
    let target = deep_node();
    let steps_replayed = store.reconstruct(target);
    assert!(steps_replayed <= LOCAL_RANGE);     // not the whole prefix from root
}
```

---

## 7. Security Considerations

| Threat | Handling |
| ------------------------------ | ------------------------------------------ |
| Leaked store reproduces execution | Bodies age-encrypted; unreadable without the identity |
| Tampering                      | blake3 over plaintext verified on decrypt |
| Secret in an index             | Index holds only hashes + step numbers, never values |

---

## 8. Future Considerations

| Feature | Description |
| ------------------ | ---------------------------------------------- |
| Garbage collection | Drop nodes no branch references |
| Per-run recipients | A distinct age recipient per run |
| Remote store       | Push/pull universes across hosts for shared exploration |

---

## 9. Implementation Notes (2026-07-24)

The real `crates/baud-snapshot-store` implementation follows this spec with three deliberate,
documented departures from §3's literal Rust pseudocode:

- **`Node.universe` is `Option<Hash>`, not `Hash`.** Not every branch point in the tree has a
  captured universe — capturing full VM state is comparatively expensive, so a guest emitting
  `MARK_BRANCH` (specs/baud-tape-device.md §4) far more often than the driver chooses to snapshot
  is the normal case. `SnapshotStore::mark_branch` records a branch-point-only node (`universe:
  None`); `SnapshotStore::put_universe` attaches a capture. `nearest`/`reconstruct` walk past
  `None` nodes to find the nearest ancestor that actually has one — this is what makes §5's "fork
  from nearest, not root" guarantee non-trivial to test (`reconstruct_forks_from_nearest_node`).
- **`RunManifest.regime` is a `String`, not `baud_host::Regime`.** This crate's declared
  dependency set (§2) deliberately excludes `baud-host`; regime is archived as an opaque tag
  (§1's own Non-Goal: "Interpreting workload semantics"). `baud_host::Regime` remains the single
  source of truth for the enum; a caller that depends on both crates (`baud-server`) converts via
  `to_string()`/parse at the boundary.
- **`NodeId` is content-addressed from `(parent, at_step, tape_range)`**, not an opaque counter —
  `Sha::of_node_identity`. Two `put_universe`/`mark_branch` calls describing the same branch point
  (e.g. replaying an identical deterministic prefix) collapse onto the same node, consistent with
  the rest of this spec's content-addressing philosophy.

Two additive API methods beyond §3's pseudocode, both exercising the declared `baud-proto`
dependency (otherwise unused by the spec's literal model) and both encrypted at rest for the same
reason universe/page bodies are (§4 — a leaked store must not reproduce guest execution or
secrets):

- **`put_tape`/`get_tape`** — the run's whole tape, named in §1's Goals ("the durable record of a
  run: the tree of universes, **the tape**, and the run manifest") but absent from §3's `SnapshotStore`
  API. One encrypted blob per run.
- **`put_records`/`get_records`** — the guest's tape-device write-channel messages
  (`baud_proto::Msg::MarkBranch`/`Log`/`Observe`/`Outcome`/...) observed up to a node, CBOR-encoded
  via `baud_proto::encode` (reusing the wire format rather than inventing a second one) and
  encrypted per node.

**Encryption**: §4 calls for "age recipient (public) + identity (path) from `baud-keys`". Rather
than shelling out to the `age` CLI binary per call (as `baud-keys`' existing `sops`-based
functions do, and as the superseded `baud-journal` crate's own age wrapper did), `baud-keys`
gained `age_encrypt`/`age_decrypt`/`age_public_key` using the pure-Rust `age` crate in-process —
no subprocess per call, no `libclang`/`bindgen` dependency (confirmed against the real crate),
so both `baud-keys` and `baud-snapshot-store` build and test fully on this Windows dev machine
with no external `sops`/`age` binaries installed at all. `age_key_path()`'s existing resolution
order ($SOPS_AGE_KEY_FILE → macOS → Linux `~/.config/sops/age/keys.txt`) is reused unchanged —
one identity file guards both the sops-encrypted `infra/secrets` and this store, per §4's "the key
that guards `infra/secrets` guards the store."

Not yet built: garbage collection, per-run recipients, and the remote store (§8, all listed there
as future work already).
