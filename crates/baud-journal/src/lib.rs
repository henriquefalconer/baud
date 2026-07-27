// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-journal — append-only CBOR chunk journal with blake3 content addressing
//
// Schema:
//   - Journal root dir: <base>/<run-id>/
//   - Index file: <run-id>/index.cbor  — list of (step, chunk_addr)
//   - Chunk files: <run-id>/chunks/<blake3-hex> — CBOR-encoded JournalChunk
//
// Content addressing:
//   - blake3 hash computed over plaintext CBOR bytes
//   - Chunk file named by hex(blake3(plaintext))
//   - Identical chunks stored once (deduplication)
//
// Encryption note:
//   - Per spec §4, chunks should be age-encrypted at rest. `open_encrypted` enables it;
//     `open` (no recipient) stores plaintext.
//   - Chunk bodies are age-encrypted in-process via baud_keys::age_encrypt/age_decrypt (the
//     pure-Rust `age` crate, no `age`/`sops` binary on PATH required) — binary (non-armored)
//     age format, which still begins with the ASCII magic `age-encryption.org/v1`.
//   - The blake3 address is always over plaintext (as spec requires), so encryption never
//     perturbs the content-addressing scheme.
//
// Rules:
//   - No IO beyond std::fs (no tokio, no async)
//   - deps = {ciborium, blake3, serde, baud-proto, anyhow}
//   - Streaming reader: indexed by (run, step)

use std::path::{Path, PathBuf};
use std::fs;
use serde::{Deserialize, Serialize};
use baud_proto::{Msg, Observation};

// ---------------------------------------------------------------------------
// Journal chunk types
// ---------------------------------------------------------------------------

/// A chunk stored in the journal. Each chunk corresponds to one event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum JournalChunk {
    /// An observation from a probe
    Observe(Observation),
    /// A draw result recorded on the tape
    Draw {
        step: u64,
        node: u16,
        bytes: Vec<u8>,
    },
    /// A syscall record
    Syscall(baud_proto::SyscallRecord),
    /// An outcome (crash or goal reached)
    Outcome {
        step: u64,
        outcome: baud_proto::Outcome,
    },
    /// A checkpoint (stream hash at step)
    Checkpoint {
        step: u64,
        stream_hash: baud_proto::Hash,
    },
    /// Protocol message (generic)
    Msg(Msg),
}

impl JournalChunk {
    pub fn step(&self) -> u64 {
        match self {
            JournalChunk::Observe(o) => o.step,
            JournalChunk::Draw { step, .. } => *step,
            JournalChunk::Syscall(s) => s.vtime,
            JournalChunk::Outcome { step, .. } => *step,
            JournalChunk::Checkpoint { step, .. } => *step,
            JournalChunk::Msg(_) => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Index entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub step: u64,
    /// hex(blake3(plaintext)) of the chunk file
    pub chunk_addr: String,
}

// ---------------------------------------------------------------------------
// Journal writer
// ---------------------------------------------------------------------------

pub struct Journal {
    base: PathBuf,
    run_id: String,
    index: Vec<IndexEntry>,
    stream_hasher: blake3::Hasher,
    /// Age recipient public key for encryption-at-rest.
    /// When Some, every chunk body is age-encrypted before writing to disk.
    /// Content addressing (blake3) is always over the plaintext.
    age_recipient: Option<String>,
}

impl Journal {
    /// Open or create a journal for the given run.
    ///
    /// If `age_recipient` is provided, all chunk bodies are age-encrypted before writing.
    /// Content addressing (blake3) is always over the plaintext.
    pub fn open(base: &Path, run_id: &str) -> Result<Self, JournalError> {
        Self::open_with_encryption(base, run_id, None)
    }

    /// Open or create a journal with age encryption enabled.
    ///
    /// `age_recipient` is the age public key (e.g. `age1...`) to encrypt to.
    pub fn open_encrypted(base: &Path, run_id: &str, age_recipient: &str) -> Result<Self, JournalError> {
        Self::open_with_encryption(base, run_id, Some(age_recipient.to_owned()))
    }

    fn open_with_encryption(base: &Path, run_id: &str, age_recipient: Option<String>) -> Result<Self, JournalError> {
        let run_dir = base.join(run_id);
        fs::create_dir_all(run_dir.join("chunks"))
            .map_err(|e| JournalError::Io(format!("create_dir_all: {e}")))?;

        // Load existing index if present
        let index_path = run_dir.join("index.cbor");
        let index = if index_path.exists() {
            let bytes = fs::read(&index_path)
                .map_err(|e| JournalError::Io(format!("read index: {e}")))?;
            ciborium::from_reader(bytes.as_slice())
                .map_err(|e| JournalError::Cbor(e.to_string()))?
        } else {
            Vec::new()
        };

        Ok(Journal {
            base: base.to_path_buf(),
            run_id: run_id.to_owned(),
            index,
            stream_hasher: blake3::Hasher::new(),
            age_recipient,
        })
    }

    /// Append a chunk to the journal.
    ///
    /// If encryption is enabled (`age_recipient` set at open time), the chunk body
    /// is age-encrypted before writing. The blake3 address is always over the plaintext.
    pub fn append(&mut self, chunk: JournalChunk) -> Result<String, JournalError> {
        let step = chunk.step();

        // Encode chunk to CBOR (plaintext)
        let mut cbor_bytes = Vec::new();
        ciborium::into_writer(&chunk, &mut cbor_bytes)
            .map_err(|e| JournalError::Cbor(e.to_string()))?;

        // Content address = blake3 over PLAINTEXT (spec requirement)
        let addr = blake3::hash(&cbor_bytes);
        let addr_hex = hex_encode(addr.as_bytes());

        // Write chunk file (idempotent: same content → same path)
        let chunk_path = self.chunk_path(&addr_hex);
        if !chunk_path.exists() {
            // Encrypt if recipient is configured; otherwise write plaintext.
            let bytes_to_write = if let Some(recipient) = &self.age_recipient {
                baud_keys::age_encrypt(recipient, &cbor_bytes)
                    .map_err(|e| JournalError::Io(format!("age encrypt: {e}")))?
            } else {
                cbor_bytes.clone()
            };
            fs::write(&chunk_path, &bytes_to_write)
                .map_err(|e| JournalError::Io(format!("write chunk: {e}")))?;
        }

        // Update stream hash (over plaintext, so encryption does not perturb verification)
        self.stream_hasher.update(&cbor_bytes);

        // Record in index
        self.index.push(IndexEntry { step, chunk_addr: addr_hex.clone() });

        // Persist index
        self.flush_index()?;

        Ok(addr_hex)
    }

    /// Append an observation.
    pub fn append_observation(&mut self, obs: Observation) -> Result<String, JournalError> {
        self.append(JournalChunk::Observe(obs))
    }

    /// Get the current stream hash (over all chunks appended so far).
    pub fn stream_hash(&self) -> baud_proto::Hash {
        let bytes = *self.stream_hasher.clone().finalize().as_bytes();
        baud_proto::Hash(bytes)
    }

    /// Iterate over all chunks for this run (in step order).
    pub fn iter(&self) -> Result<Vec<JournalChunk>, JournalError> {
        let mut result = Vec::new();
        for entry in &self.index {
            let chunk = self.read_chunk(&entry.chunk_addr)?;
            result.push(chunk);
        }
        Ok(result)
    }

    /// Read chunks up to a given step (inclusive).
    pub fn iter_to_step(&self, max_step: u64) -> Result<Vec<JournalChunk>, JournalError> {
        let mut result = Vec::new();
        for entry in &self.index {
            if entry.step > max_step { break; }
            let chunk = self.read_chunk(&entry.chunk_addr)?;
            result.push(chunk);
        }
        Ok(result)
    }

    /// Read a chunk by its address.
    fn read_chunk(&self, addr: &str) -> Result<JournalChunk, JournalError> {
        let path = self.chunk_path(addr);
        let stored_bytes = fs::read(&path)
            .map_err(|e| JournalError::Io(format!("read chunk {addr}: {e}")))?;

        // Decrypt if encryption is enabled
        let plaintext = if self.age_recipient.is_some() {
            let identity_path = baud_keys::age_key_path().ok_or_else(|| {
                JournalError::Io(format!(
                    "age decrypt {addr}: no age identity file found (checked $SOPS_AGE_KEY_FILE \
                     and the OS-standard sops/age locations — see baud_keys::age_key_path)"
                ))
            })?;
            baud_keys::age_decrypt(&identity_path, &stored_bytes)
                .map_err(|e| JournalError::Io(format!("age decrypt {addr}: {e}")))?
        } else {
            stored_bytes
        };

        // Verify content integrity (blake3 over plaintext)
        let computed = blake3::hash(&plaintext);
        let computed_hex = hex_encode(computed.as_bytes());
        if computed_hex != addr {
            return Err(JournalError::Integrity {
                expected: addr.to_owned(),
                got: computed_hex,
            });
        }

        ciborium::from_reader(plaintext.as_slice())
            .map_err(|e| JournalError::Cbor(e.to_string()))
    }

    /// Get the run directory.
    fn run_dir(&self) -> PathBuf {
        self.base.join(&self.run_id)
    }

    fn chunk_path(&self, addr: &str) -> PathBuf {
        self.run_dir().join("chunks").join(addr)
    }

    fn flush_index(&self) -> Result<(), JournalError> {
        let index_path = self.run_dir().join("index.cbor");
        let mut bytes = Vec::new();
        ciborium::into_writer(&self.index, &mut bytes)
            .map_err(|e| JournalError::Cbor(e.to_string()))?;
        fs::write(index_path, bytes)
            .map_err(|e| JournalError::Io(format!("write index: {e}")))?;
        Ok(())
    }

    /// Return all observations for this run.
    pub fn observations(&self) -> Result<Vec<Observation>, JournalError> {
        let chunks = self.iter()?;
        Ok(chunks.into_iter().filter_map(|c| {
            if let JournalChunk::Observe(o) = c { Some(o) } else { None }
        }).collect())
    }

    /// Return the index entries (step → chunk_addr pairs).
    pub fn index(&self) -> &[IndexEntry] {
        &self.index
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("io error: {0}")]
    Io(String),
    #[error("cbor error: {0}")]
    Cbor(String),
    #[error("integrity error: expected {expected}, got {got}")]
    Integrity { expected: String, got: String },
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use baud_proto::Value;
    use tempfile::TempDir;

    fn obs(probe: &str, step: u64) -> Observation {
        Observation {
            probe: probe.into(),
            node: 0,
            value: Value::U64(step),
            step,
        }
    }

    #[test]
    fn append_and_read_observations() {
        let dir = TempDir::new().unwrap();
        let mut j = Journal::open(dir.path(), "run-001").unwrap();

        j.append_observation(obs("depth", 1)).unwrap();
        j.append_observation(obs("depth", 2)).unwrap();
        j.append_observation(obs("depth", 3)).unwrap();

        let observations = j.observations().unwrap();
        assert_eq!(observations.len(), 3);
        assert_eq!(observations[0].step, 1);
        assert_eq!(observations[2].step, 3);
    }

    #[test]
    fn dedup_by_plaintext_hash() {
        let dir = TempDir::new().unwrap();
        let mut j = Journal::open(dir.path(), "run-002").unwrap();

        // Append the same observation twice
        let addr1 = j.append_observation(obs("x", 1)).unwrap();
        let addr2 = j.append_observation(obs("x", 1)).unwrap();

        // Same content → same address
        assert_eq!(addr1, addr2, "identical chunks should have identical addresses");

        // But index has 2 entries
        assert_eq!(j.index().len(), 2);

        // Only one chunk file on disk
        let chunk_dir = dir.path().join("run-002").join("chunks");
        let files: Vec<_> = fs::read_dir(&chunk_dir).unwrap().collect();
        assert_eq!(files.len(), 1, "deduplication: only 1 chunk file for identical content");
    }

    #[test]
    fn stream_hash_changes_with_content() {
        let dir = TempDir::new().unwrap();
        let mut j1 = Journal::open(dir.path(), "run-003a").unwrap();
        let mut j2 = Journal::open(dir.path(), "run-003b").unwrap();

        j1.append_observation(obs("x", 1)).unwrap();
        j2.append_observation(obs("y", 1)).unwrap();

        let h1 = j1.stream_hash();
        let h2 = j2.stream_hash();
        assert_ne!(h1.0, h2.0, "different content → different stream hash");
    }

    #[test]
    fn stream_hash_same_for_same_content() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let mut j1 = Journal::open(dir1.path(), "run-004").unwrap();
        let mut j2 = Journal::open(dir2.path(), "run-004").unwrap();

        j1.append_observation(obs("depth", 10)).unwrap();
        j2.append_observation(obs("depth", 10)).unwrap();

        assert_eq!(j1.stream_hash().0, j2.stream_hash().0,
            "same content → same stream hash (reproducibility)");
    }

    #[test]
    fn iter_to_step_filters_correctly() {
        let dir = TempDir::new().unwrap();
        let mut j = Journal::open(dir.path(), "run-005").unwrap();

        j.append_observation(obs("x", 1)).unwrap();
        j.append_observation(obs("x", 5)).unwrap();
        j.append_observation(obs("x", 10)).unwrap();

        let chunks = j.iter_to_step(5).unwrap();
        assert_eq!(chunks.len(), 2, "iter_to_step(5) should return chunks at step 1 and 5");
    }

    #[test]
    fn integrity_check_on_read() {
        let dir = TempDir::new().unwrap();
        let mut j = Journal::open(dir.path(), "run-006").unwrap();
        let addr = j.append_observation(obs("x", 1)).unwrap();

        // Corrupt the chunk file
        let chunk_path = dir.path().join("run-006").join("chunks").join(&addr);
        let mut content = fs::read(&chunk_path).unwrap();
        if !content.is_empty() {
            content[0] ^= 0xFF; // flip first byte
        }
        fs::write(&chunk_path, content).unwrap();

        // Reading should fail with integrity error
        let result = j.iter();
        assert!(matches!(result, Err(JournalError::Integrity { .. })),
            "corrupted chunk should fail integrity check");
    }

    #[test]
    fn journal_reopens_existing_index() {
        let dir = TempDir::new().unwrap();

        // Write 3 observations
        {
            let mut j = Journal::open(dir.path(), "run-007").unwrap();
            j.append_observation(obs("a", 1)).unwrap();
            j.append_observation(obs("b", 2)).unwrap();
            j.append_observation(obs("c", 3)).unwrap();
        }

        // Reopen and verify
        let j = Journal::open(dir.path(), "run-007").unwrap();
        assert_eq!(j.index().len(), 3, "reopened journal should have 3 index entries");
        let obs_list = j.observations().unwrap();
        assert_eq!(obs_list.len(), 3);
        assert_eq!(obs_list[1].probe, "b");
    }

    /// The security half of `chunk_bodies_are_ciphertext` that holds on *every* host,
    /// with or without the `age` binary: asking for an encrypted journal must never
    /// result in plaintext on disk. Encryption itself is in-process (`baud_keys::
    /// age_encrypt`, pure Rust — no `age` binary needed to write a chunk, unlike before
    /// this crate stopped shelling out), so the append should always succeed here; the
    /// `Err` arm below is kept as a fail-closed safety net in case that ever changes.
    ///
    /// Without this, the only coverage of `open_encrypted` is the `#[ignore]`d test
    /// below, which needs the real `age` binary on PATH for its CLI-interop decrypt step.
    #[test]
    fn requesting_encryption_never_leaves_plaintext_on_disk() {
        // A real recipient, generated in-process via the `age` crate — no binary needed.
        let identity = baud_keys::generate_identity_file();
        let recipient = baud_keys::parse_public_key(&identity).expect("recipient");

        let dir = TempDir::new().unwrap();
        let mut j = Journal::open_encrypted(dir.path(), "enc-run-002", &recipient)
            .expect("open_encrypted should succeed");
        let chunks_dir = dir.path().join("enc-run-002").join("chunks");

        match j.append_observation(obs("probe", 1)) {
            Ok(addr) => {
                let on_disk = fs::read(chunks_dir.join(&addr)).expect("chunk file should exist");
                // The chunk address is blake3 over the *plaintext*, so bytes that hash
                // to their own address are unencrypted.
                assert_ne!(
                    hex_encode(blake3::hash(&on_disk).as_bytes()),
                    addr,
                    "chunk written under an encrypting journal is the plaintext CBOR"
                );
                assert!(
                    on_disk.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----")
                        || on_disk.starts_with(b"age-encryption.org/v1"),
                    "chunk on disk is neither armored nor binary age ciphertext: {:?}",
                    &on_disk[..on_disk.len().min(40)]
                );
            }
            Err(e) => {
                // Encryption failed for some other reason — the append must still fail
                // closed: nothing at all may be written.
                let written: Vec<_> = fs::read_dir(&chunks_dir)
                    .expect("chunks dir")
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name())
                    .collect();
                assert!(
                    written.is_empty(),
                    "encryption failed ({e}) but chunk bytes were still written: {written:?}"
                );
                assert!(
                    j.observations().map(|o| o.is_empty()).unwrap_or(true),
                    "a chunk that was never written must not be indexed"
                );
            }
        }
    }

    /// Verify that `Journal`'s in-process age encryption (`baud_keys::age_encrypt`, no
    /// `age`/`sops` binary needed to write a chunk) produces ciphertext the *real* `age` CLI
    /// can decrypt — i.e. baud_keys's pure-Rust implementation is standard age format, not a
    /// baud-specific variant that merely round-trips against itself.
    ///
    /// `#[ignore]`d rather than self-skipping: it needs the real `age` binary on PATH (absent
    /// on the dev host per CLAUDE.md) for this one-way interop check only — encryption itself
    /// needs no external binary any more. A test that returns early having asserted nothing is
    /// indistinguishable from one that passes, so this fails loudly rather than skipping. Run
    /// with `cargo test -p baud-journal -- --ignored` on a host with `age` on PATH.
    /// `requesting_encryption_never_leaves_plaintext_on_disk` above covers the part of this
    /// contract (encrypted chunks are never plaintext) that holds without that binary.
    #[test]
    #[ignore = "requires the real `age` binary on PATH, for CLI-interop decryption only"]
    fn chunk_bodies_are_ciphertext() {
        let age_present = std::process::Command::new("age").arg("--version").output();
        assert!(
            age_present.is_ok(),
            "chunk_bodies_are_ciphertext needs `age` on PATH (that is why it is #[ignore]d)"
        );

        // Identity + recipient generated in-process (baud_keys::generate_identity_file matches
        // age-keygen's own output format) — no `age-keygen` binary needed either.
        let identity = baud_keys::generate_identity_file();
        let recipient = baud_keys::parse_public_key(&identity).expect("recipient");
        let key_dir = TempDir::new().unwrap();
        let key_path = key_dir.path().join("key.txt");
        fs::write(&key_path, &identity).unwrap();

        // Chunk files on disk must be ciphertext (binary, non-armored age format — still
        // starts with the format's own ASCII magic line) when `open_encrypted()` is used.
        let data_dir = TempDir::new().unwrap();
        let mut j = Journal::open_encrypted(data_dir.path(), "enc-run-001", &recipient)
            .expect("open_encrypted should succeed");
        let addr = j.append_observation(obs("probe", 1))
            .expect("append_observation should succeed");

        let chunk_path = data_dir.path().join("enc-run-001").join("chunks").join(&addr);
        let on_disk = fs::read(&chunk_path).expect("chunk file should exist");
        assert!(
            on_disk.starts_with(b"age-encryption.org/v1"),
            "chunk on disk should be binary age ciphertext, got: {:?}",
            &on_disk[..on_disk.len().min(60)]
        );

        // Decrypt using the real `age` CLI (bypassing baud_keys entirely) — proves the
        // in-process ciphertext baud_keys produced is standard age format.
        let decrypt_out = std::process::Command::new("age")
            .args(["--decrypt", "--identity"])
            .arg(&key_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(&on_disk).ok();
                child.wait_with_output()
            })
            .expect("age --decrypt should succeed");

        assert!(
            decrypt_out.status.success(),
            "age --decrypt should succeed: {}",
            String::from_utf8_lossy(&decrypt_out.stderr)
        );
        // The decrypted bytes should be valid CBOR (non-empty)
        assert!(!decrypt_out.stdout.is_empty(), "decrypted chunk should be non-empty CBOR");
    }
}
