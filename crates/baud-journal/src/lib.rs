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
//   - Per spec §4, chunks should be age-encrypted at rest.
//   - This implementation stores plaintext (baud-keys age integration is M5+).
//   - The blake3 address is always over plaintext (as spec requires), so when
//     encryption is added later, the content-addressing scheme remains correct.
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
}

impl Journal {
    /// Open or create a journal for the given run.
    pub fn open(base: &Path, run_id: &str) -> Result<Self, JournalError> {
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
        })
    }

    /// Append a chunk to the journal.
    pub fn append(&mut self, chunk: JournalChunk) -> Result<String, JournalError> {
        let step = chunk.step();

        // Encode chunk to CBOR
        let mut cbor_bytes = Vec::new();
        ciborium::into_writer(&chunk, &mut cbor_bytes)
            .map_err(|e| JournalError::Cbor(e.to_string()))?;

        // Content address = blake3 over plaintext
        let addr = blake3::hash(&cbor_bytes);
        let addr_hex = hex_encode(addr.as_bytes());

        // Write chunk file (idempotent: same content → same path)
        let chunk_path = self.chunk_path(&addr_hex);
        if !chunk_path.exists() {
            fs::write(&chunk_path, &cbor_bytes)
                .map_err(|e| JournalError::Io(format!("write chunk: {e}")))?;
        }

        // Update stream hash
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
        let bytes = fs::read(&path)
            .map_err(|e| JournalError::Io(format!("read chunk {addr}: {e}")))?;

        // Verify content integrity
        let computed = blake3::hash(&bytes);
        let computed_hex = hex_encode(computed.as_bytes());
        if computed_hex != addr {
            return Err(JournalError::Integrity {
                expected: addr.to_owned(),
                got: computed_hex,
            });
        }

        ciborium::from_reader(bytes.as_slice())
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
    use baud_proto::{Value, Hash};
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
    fn content_addressing_deduplication() {
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
}
