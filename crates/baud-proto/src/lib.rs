// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-proto — wire and domain types
//
// Rules:
// - No IO, no async, no network, no tokio, no chrono
// - time = u64 virtual steps
// - deps = {serde, ciborium} only
// - soft budget <= 700 LOC

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// Blake3 hash (32 bytes)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

impl Default for Hash {
    fn default() -> Self {
        Hash([0u8; 32])
    }
}

/// Pixel format for frame data
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixFmt {
    Rgba8888,
    Rgb565,
    Indexed8,
}

/// Source of an eBPF record — native kernel or fallback shim
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Native,
    Fallback,
}

// ---------------------------------------------------------------------------
// Manifest & Run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemLayout {
    pub brk: u64,
    pub stack_top: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub spec_hash: Hash,
    pub closure_hash: Hash,
    pub seed: u64,
    pub strategy: StrategySpec,
    pub tactics: TacticsSpec,
    /// CPU class + vendor/model recorded for reconstruction pinning
    pub cpu_class: String,
    pub layout: MemLayout,
}

// ---------------------------------------------------------------------------
// Tape & Draws
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceChunk {
    pub step: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkovParams {
    pub p_start: f64,
    pub p_stop: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawRequest {
    Bits(u32),
    Int { lo: i64, hi: i64 },
    Choice(Vec<u32>),
    Hold { mean: u32 },
    Weather(MarkovParams),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawResult {
    pub bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Observation vocabulary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    U64(u64),
    I64(i64),
    Bytes(Vec<u8>),
    Utf8(String),
    Hash(Hash),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub probe: String,
    pub node: u16,
    pub value: Value,
    pub step: u64,
}

/// The three and only outcome messages. No temporal types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Outcome {
    GoalReached { metric: String },
    Crash {
        node: Option<u16>,
        invariant: Option<String>,
        signal: Option<i32>,
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyscallRecord {
    pub node: u16,
    pub sysno: u32,
    pub args_digest: Hash,
    pub ret: i64,
    pub vtime: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EbpfRecord {
    pub node: u16,
    pub event: String,
    pub value: u64,
    pub vtime: u64,
    pub source: Source,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameRecord {
    pub node: u16,
    pub step: u64,
    pub width: u32,
    pub height: u32,
    pub format: PixFmt,
    pub hash: Hash,
    /// Absent in hash-only mode (fuzz runs)
    pub bytes: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Spec types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeAdapter {
    StdoutKv { prefix: Option<String> },
    VfsFile { path: String, mode: VfsMode },
    SyscallCounter { pattern: String },
    EbpfCounter { event: String },
    ExitHash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VfsMode {
    Hash,
    U64,
    Utf8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSpec {
    pub name: String,
    pub adapter: ProbeAdapter,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reservoir {
    pub keep: u32,
    pub p_backoff: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Predicate {
    pub probe: String,
    pub value: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrategySpec {
    /// Probe names to maximize, in priority order (lexicographic)
    pub maximize: Vec<String>,
    pub buckets: Vec<String>,
    pub reservoir: Option<Reservoir>,
    pub goal: Option<Predicate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTactic {
    Random,
    StatefulMask { p_flip: f64 },
    Hold { geom_mean: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeatherTactic {
    MarkovPartition(MarkovParams),
    BurstDelay { regimes: Vec<(u64, u64)> },
    CrashRestart { p: f64, min_up_ticks: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchBias {
    pub weights: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Weighted<T> {
    pub weight: f64,
    pub value: T,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TacticsSpec {
    pub input: Vec<Weighted<InputTactic>>,
    pub weather: Vec<Weighted<WeatherTactic>>,
    pub schedule: Option<SwitchBias>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAdapter {
    Stdin,
    Fifo { path: String },
    Net,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSpec {
    pub name: String,
    pub argv: Vec<String>,
    pub inputs: Vec<InputAdapter>,
    pub probes: Vec<ProbeSpec>,
}

// ---------------------------------------------------------------------------
// Protocol messages
// ---------------------------------------------------------------------------

pub const PROTO_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "msg")]
pub enum Msg {
    Hello {
        identity: String,
        manifest_hash: Hash,
    },
    DrawRequest(DrawRequest),
    DrawResult(DrawResult),
    Observe(Observation),
    Syscall(SyscallRecord),
    Ebpf(EbpfRecord),
    Frame(FrameRecord),
    Checkpoint {
        stream_hash: Hash,
        step: u64,
    },
    Outcome(Outcome),
    Eof,
}

// ---------------------------------------------------------------------------
// CBOR encode / decode helpers
// ---------------------------------------------------------------------------

/// Encode a message to CBOR bytes, prefixed with the version byte.
pub fn encode(msg: &Msg) -> Result<Vec<u8>, EncodeError> {
    let mut buf = vec![PROTO_VERSION];
    ciborium::into_writer(msg, &mut buf).map_err(EncodeError)?;
    Ok(buf)
}

/// Decode a message from CBOR bytes, checking the version byte.
pub fn decode(data: &[u8]) -> Result<Msg, DecodeError> {
    let (version, body) = data.split_first().ok_or(DecodeError::Empty)?;
    if *version != PROTO_VERSION {
        return Err(DecodeError::UnsupportedVersion(*version));
    }
    ciborium::from_reader(body).map_err(|e| DecodeError::Cbor(e.to_string()))
}

#[derive(Debug, thiserror::Error)]
#[error("cbor encode error: {0}")]
pub struct EncodeError(#[from] ciborium::ser::Error<std::io::Error>);

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("empty buffer")]
    Empty,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
    #[error("cbor decode error: {0}")]
    Cbor(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Msg) {
        let encoded = encode(&msg).expect("encode");
        let decoded = decode(&encoded).expect("decode");
        // Compare via re-encode (Msg doesn't implement PartialEq due to f64)
        let re = encode(&decoded).expect("re-encode");
        assert_eq!(encoded, re, "roundtrip mismatch for {:?}", msg);
    }

    #[test]
    fn hello_roundtrip() {
        roundtrip(Msg::Hello {
            identity: "baud://tape/t1/run/r1".into(),
            manifest_hash: Hash([1u8; 32]),
        });
    }

    #[test]
    fn observe_roundtrip() {
        roundtrip(Msg::Observe(Observation {
            probe: "depth".into(),
            node: 0,
            value: Value::U64(42),
            step: 100,
        }));
    }

    #[test]
    fn outcome_crash_roundtrip() {
        roundtrip(Msg::Outcome(Outcome::Crash {
            node: Some(1),
            invariant: Some("log_prefix_agreement".into()),
            signal: None,
            detail: "two leaders in same term".into(),
        }));
    }

    #[test]
    fn eof_roundtrip() {
        roundtrip(Msg::Eof);
    }

    #[test]
    fn wrong_version_rejected() {
        let mut bad = encode(&Msg::Eof).unwrap();
        bad[0] = 99;
        assert!(matches!(decode(&bad), Err(DecodeError::UnsupportedVersion(99))));
    }

    #[test]
    fn empty_rejected() {
        assert!(matches!(decode(&[]), Err(DecodeError::Empty)));
    }
}
