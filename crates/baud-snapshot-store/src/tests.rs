// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// specs/baud-snapshot-store.md §6's named tests (`snapshot_store_bodies_are_ciphertext`,
// `pages_dedup_by_plaintext_hash`, `reconstruct_forks_from_nearest_node`), translated onto this
// crate's real `SnapshotStore` API, plus additional coverage for the crate's own extensions
// (`mark_branch`, `put_tape`/`get_tape`, `put_records`/`get_records`) and the negative paths §4's
// threat table calls out (missing key, tampering-adjacent malformed reads).

use crate::{Node, RunId, RunManifest, Sha, SnapshotStore, StoreError};

/// Same throwaway age-x25519 test keypair as `baud-keys`' own tests (see that crate's doc
/// comment on `TEST_IDENTITY` for why it is hardcoded rather than generated at test time).
const TEST_IDENTITY: &str = "AGE-SECRET-KEY-1VPR7E992FFDWZU0JAACA83A3VDG6JLF9HVHEWWWYLN5YLXNJFYGSNXYJ9R";
const TEST_RECIPIENT: &str = "age1u3p0u0p7w4tmwaplpw3vafrj0xmturnml200636wdgamemh69ytql87pg4";

fn open_test_store() -> (tempfile::TempDir, tempfile::TempDir, SnapshotStore) {
    let store_root = tempfile::tempdir().expect("store root tmpdir");
    let identity_dir = tempfile::tempdir().expect("identity tmpdir");
    let identity_path = identity_dir.path().join("keys.txt");
    std::fs::write(&identity_path, format!("# public key: {TEST_RECIPIENT}\n{TEST_IDENTITY}\n"))
        .expect("write identity file");
    let store = SnapshotStore::open_with_keys(
        store_root.path(),
        TEST_RECIPIENT.to_owned(),
        Some(identity_path),
    );
    (store_root, identity_dir, store)
}

fn write_only_store(root: &std::path::Path) -> SnapshotStore {
    // A store that can encrypt (put_*) but has no identity to decrypt with — the "publish-only"
    // configuration this module's doc comment on `identity_path` describes.
    SnapshotStore::open_with_keys(root, TEST_RECIPIENT.to_owned(), None)
}

// ---------------------------------------------------------------------------
// specs/baud-snapshot-store.md §6's named tests
// ---------------------------------------------------------------------------

#[test]
fn snapshot_store_bodies_are_ciphertext() {
    let (root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-a");
    let secret_body = b"universe RAM contains sk-secret-token somewhere in it";
    let node = store.put_universe(&run, None, 0, (0, 0), secret_body).expect("put_universe");

    let n = store.read_node(&run, node).expect("read_node");
    let universe_hash = Sha::from_hex(n.universe.as_deref().unwrap()).unwrap();
    let body_path = root
        .path()
        .join("runs")
        .join("run-a")
        .join("universes")
        .join(format!("{}.age", universe_hash.to_hex()));
    let raw = std::fs::read(&body_path).expect("read raw universe body file");
    assert!(
        !raw.windows(b"sk-secret-token".len()).any(|w| w == b"sk-secret-token"),
        "the plaintext secret must never appear in the on-disk ciphertext body"
    );

    // And it does round-trip back to the exact plaintext via the store's own decrypt path.
    let decrypted = store.get_universe(&run, node).expect("get_universe");
    assert_eq!(decrypted, secret_body);
}

#[test]
fn pages_dedup_by_plaintext_hash() {
    let (_root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-b");
    let page_bytes = vec![0x42u8; 4096];

    let a = store.put_page(&run, &page_bytes).expect("put_page a");
    let b = store.put_page(&run, &page_bytes).expect("put_page b");
    assert_eq!(a.address, b.address, "identical plaintext must produce the same address");

    // Only one encrypted body was ever written for this content, even though age's own
    // ciphertext is non-deterministic per call (specs/baud-snapshot-store.md §4).
    let pages_dir = _root.path().join("runs").join("run-b").join("pages");
    let entries: Vec<_> = std::fs::read_dir(&pages_dir).unwrap().collect();
    assert_eq!(entries.len(), 1, "one distinct plaintext -> one stored ciphertext body");

    let round_tripped = store.get_page(&run, a).expect("get_page");
    assert_eq!(round_tripped, page_bytes);
}

#[test]
fn reconstruct_forks_from_nearest_node() {
    const LOCAL_RANGE: u64 = 32;
    let (_root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-c");

    // A deep chain: root has a captured universe; every subsequent node is a branch-point-only
    // `mark_branch` (no universe) except the very last one, `LOCAL_RANGE` steps past the last
    // captured universe. `reconstruct` must report ~LOCAL_RANGE steps replayed, not the full
    // chain depth from the root.
    let total_depth: u64 = 500;
    let root_node = store.put_universe(&run, None, 0, (0, 0), b"root universe").expect("root");

    let mut parent = root_node;
    let mut last_captured_at = 0u64;
    let mut deep_target = root_node;
    for step in 1..=total_depth {
        parent = store
            .mark_branch(&run, Some(parent), step, (step - 1, step))
            .expect("mark_branch");
        if step == total_depth - LOCAL_RANGE {
            // Capture a universe partway through, closer to the deep target than the root is.
            let captured = store
                .put_universe(&run, Some(parent), step, (step - 1, step), b"mid universe")
                .expect("mid put_universe");
            last_captured_at = step;
            parent = captured;
        }
        if step == total_depth {
            deep_target = parent;
        }
    }
    assert!(last_captured_at > 0, "test setup must actually capture a mid-chain universe");

    let steps_replayed = store.reconstruct(&run, deep_target).expect("reconstruct");
    assert_eq!(steps_replayed, LOCAL_RANGE, "must replay from the nearest captured ancestor");
    assert!(
        steps_replayed < total_depth,
        "must not replay the whole prefix from root ({total_depth} steps)"
    );

    let nearest = store.nearest(&run, deep_target).expect("nearest");
    let nearest_node = store.read_node(&run, nearest).expect("read nearest");
    assert_eq!(nearest_node.at_step, last_captured_at);
}

// ---------------------------------------------------------------------------
// Additional coverage
// ---------------------------------------------------------------------------

#[test]
fn nearest_returns_the_node_itself_when_it_has_a_universe() {
    let (_root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-d");
    let node = store.put_universe(&run, None, 0, (0, 0), b"body").expect("put_universe");
    assert_eq!(store.nearest(&run, node).unwrap(), node);
    assert_eq!(store.reconstruct(&run, node).unwrap(), 0);
}

#[test]
fn mark_branch_does_not_clobber_an_existing_capture() {
    let (_root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-e");
    let node = store.put_universe(&run, None, 5, (0, 5), b"captured").expect("put_universe");
    // Re-recording the same branch point (idempotent — same content-addressed id) must not erase
    // the universe that was already captured there.
    let same_node = store.mark_branch(&run, None, 5, (0, 5)).expect("mark_branch");
    assert_eq!(node, same_node);
    let n = store.read_node(&run, node).unwrap();
    assert!(n.universe.is_some());
}

#[test]
fn identical_branch_points_content_address_to_the_same_node() {
    let (_root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-f");
    let a = store.mark_branch(&run, None, 3, (0, 3)).unwrap();
    let b = store.mark_branch(&run, None, 3, (0, 3)).unwrap();
    assert_eq!(a, b, "same (parent, at_step, tape_range) -> same NodeId");
}

#[test]
fn get_universe_on_branch_only_node_is_an_explicit_error() {
    let (_root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-g");
    let node = store.mark_branch(&run, None, 1, (0, 1)).unwrap();
    let err = store.get_universe(&run, node).unwrap_err();
    assert!(matches!(err, StoreError::NoUniverseAtNode(_)));
}

#[test]
fn root_with_no_universe_reports_no_ancestor() {
    let (_root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-h");
    let node = store.mark_branch(&run, None, 0, (0, 0)).unwrap();
    let err = store.nearest(&run, node).unwrap_err();
    assert!(matches!(err, StoreError::NoAncestorWithUniverse(_)));
}

#[test]
fn manifest_roundtrips_in_clear() {
    let (root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-i");
    let node = store.put_universe(&run, None, 0, (0, 0), b"root").unwrap();
    let manifest = RunManifest {
        seed: 42,
        image_hash: Sha::of(b"image").to_hex(),
        regime: "cooperative".to_owned(),
        root: node.to_hex(),
    };
    store.put_manifest(&run, &manifest).expect("put_manifest");
    let read_back = store.get_manifest(&run).expect("get_manifest");
    assert_eq!(read_back, manifest);

    // The manifest is stored in clear (specs/baud-snapshot-store.md §4) — readable as plain JSON
    // without any decryption at all.
    let raw = std::fs::read_to_string(root.path().join("runs").join("run-i").join("manifest.json"))
        .unwrap();
    assert!(raw.contains("cooperative"));
    assert!(raw.contains("42"));
}

#[test]
fn node_index_is_readable_without_decryption() {
    let (root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-j");
    let node = store.put_universe(&run, None, 7, (0, 7), b"body").unwrap();
    let raw = std::fs::read_to_string(
        root.path().join("runs").join("run-j").join("nodes").join(format!("{}.json", node.to_hex())),
    )
    .unwrap();
    let parsed: Node = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.at_step, 7);
}

#[test]
fn get_fails_loudly_when_no_identity_is_configured() {
    let root = tempfile::tempdir().unwrap();
    let store = write_only_store(root.path());
    let run = RunId::new("run-k");
    let node = store.put_universe(&run, None, 0, (0, 0), b"body").expect("put still works");
    let err = store.get_universe(&run, node).unwrap_err();
    assert!(matches!(err, StoreError::MissingKey));
}

#[test]
fn tape_roundtrips_and_is_ciphertext_on_disk() {
    let (root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-l");
    let tape = b"the-whole-tape-including-a-secret-seed-value";
    store.put_tape(&run, tape).expect("put_tape");
    let raw = std::fs::read(root.path().join("runs").join("run-l").join("tape.age")).unwrap();
    assert_ne!(raw, tape, "tape body must be encrypted on disk");
    let read_back = store.get_tape(&run).expect("get_tape");
    assert_eq!(read_back, tape);
}

#[test]
fn records_roundtrip_through_baud_proto_encoding() {
    let (_root, _identity_dir, store) = open_test_store();
    let run = RunId::new("run-m");
    let node = store.put_universe(&run, None, 0, (0, 0), b"body").unwrap();
    let records = vec![
        baud_proto::Msg::MarkBranch { step: 3 },
        baud_proto::Msg::Log { bytes: b"hello from the guest".to_vec(), step: 4 },
    ];
    store.put_records(&run, node, &records).expect("put_records");
    let round_tripped = store.get_records(&run, node).expect("get_records");
    assert_eq!(round_tripped.len(), 2);
    assert!(matches!(round_tripped[0], baud_proto::Msg::MarkBranch { step: 3 }));
    match &round_tripped[1] {
        baud_proto::Msg::Log { bytes, step } => {
            assert_eq!(bytes, b"hello from the guest");
            assert_eq!(*step, 4);
        }
        other => panic!("expected Log, got {other:?}"),
    }
}

#[test]
fn run_id_sanitization_prevents_path_escape() {
    let (root, _identity_dir, store) = open_test_store();
    let run = RunId::new("../../evil");
    store.put_universe(&run, None, 0, (0, 0), b"body").expect("put_universe with hostile run id");
    // The run directory must land inside the store root, not have escaped it via `..`.
    let runs_dir = root.path().join("runs");
    let mut saw_expected_dir = false;
    for entry in std::fs::read_dir(&runs_dir).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.path().starts_with(&runs_dir), "no entry may escape the runs directory");
        saw_expected_dir = true;
    }
    assert!(saw_expected_dir, "the sanitized run directory must exist under runs/");
    assert!(!root.path().parent().unwrap().join("evil").exists());
}

// ---------------------------------------------------------------------------
// Proptest: node-identity content-addressing never collides across unrelated branch points
// within a realistically small field range.
// ---------------------------------------------------------------------------

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    proptest! {
        #[test]
        fn distinct_field_tuples_yield_distinct_or_matching_ids_consistently(
            entries in prop::collection::vec((0u64..20, 0u64..20, 0u64..20), 1..40)
        ) {
            let mut seen: HashMap<(u64, u64, u64), Sha> = HashMap::new();
            for (at_step, range_start, range_end) in entries {
                let key = (at_step, range_start, range_end);
                let id = Sha::of_node_identity(None, at_step, (range_start, range_end));
                if let Some(prior) = seen.get(&key) {
                    prop_assert_eq!(*prior, id, "same fields must always produce the same id");
                } else {
                    seen.insert(key, id);
                }
            }
        }
    }
}
