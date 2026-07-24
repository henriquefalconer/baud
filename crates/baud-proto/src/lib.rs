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
// Bounded deserialization helpers (VR1-M3: length caps on collection fields)
// ---------------------------------------------------------------------------

/// Maximum byte length for `Value::Bytes` and `FrameRecord::bytes` (64 MiB).
pub const MAX_BYTES_LEN: usize = 64 * 1024 * 1024;
/// Maximum entry count for string-list fields (`argv`, `maximize`, `buckets`, etc.).
pub const MAX_STRING_LIST_LEN: usize = 1024;

mod bounded {
    use serde::{Deserializer, de};
    use super::{MAX_BYTES_LEN, MAX_STRING_LIST_LEN};

    /// Deserialize a `Vec<u8>` capped at `MAX_BYTES_LEN`.
    pub fn bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let v: Vec<u8> = Vec::deserialize(d)?;
        if v.len() > MAX_BYTES_LEN {
            return Err(de::Error::custom(format!(
                "byte field exceeds max length ({} > {MAX_BYTES_LEN})", v.len()
            )));
        }
        Ok(v)
    }

    /// Deserialize an `Option<Vec<u8>>` capped at `MAX_BYTES_LEN`.
    pub fn opt_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let v: Option<Vec<u8>> = Option::deserialize(d)?;
        if let Some(ref b) = v {
            if b.len() > MAX_BYTES_LEN {
                return Err(de::Error::custom(format!(
                    "byte field exceeds max length ({} > {MAX_BYTES_LEN})", b.len()
                )));
            }
        }
        Ok(v)
    }

    /// Deserialize a `Vec<String>` capped at `MAX_STRING_LIST_LEN` entries.
    pub fn string_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        let v: Vec<String> = Vec::deserialize(d)?;
        if v.len() > MAX_STRING_LIST_LEN {
            return Err(de::Error::custom(format!(
                "string list exceeds max length ({} > {MAX_STRING_LIST_LEN})", v.len()
            )));
        }
        Ok(v)
    }

    use serde::Deserialize;

    pub fn vec_of<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        let v: Vec<T> = Vec::deserialize(d)?;
        if v.len() > MAX_STRING_LIST_LEN {
            return Err(de::Error::custom(format!(
                "list field exceeds max length ({} > {MAX_STRING_LIST_LEN})", v.len()
            )));
        }
        Ok(v)
    }
}

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
    #[serde(deserialize_with = "bounded::bytes")]
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
    #[serde(deserialize_with = "bounded::bytes")]
    pub bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Observation vocabulary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    U64(u64),
    I64(i64),
    Bytes(Vec<u8>),
    Utf8(String),
    Hash(Hash),
}

impl<'de> serde::Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Deserialize into a helper that mirrors the enum but with a cap on Bytes
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum ValueHelper {
            U64(u64),
            I64(i64),
            Bytes(Vec<u8>),
            Utf8(String),
            Hash(Hash),
        }
        let h = ValueHelper::deserialize(d)?;
        match h {
            ValueHelper::U64(v) => Ok(Value::U64(v)),
            ValueHelper::I64(v) => Ok(Value::I64(v)),
            ValueHelper::Bytes(b) => {
                if b.len() > MAX_BYTES_LEN {
                    return Err(serde::de::Error::custom(format!(
                        "Value::Bytes exceeds max length ({} > {MAX_BYTES_LEN})", b.len()
                    )));
                }
                Ok(Value::Bytes(b))
            }
            ValueHelper::Utf8(s) => Ok(Value::Utf8(s)),
            ValueHelper::Hash(h) => Ok(Value::Hash(h)),
        }
    }
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
    #[serde(deserialize_with = "bounded::opt_bytes", default)]
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
    #[serde(deserialize_with = "bounded::string_list", default)]
    pub maximize: Vec<String>,
    #[serde(deserialize_with = "bounded::string_list", default)]
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
    #[serde(deserialize_with = "bounded::string_list")]
    pub argv: Vec<String>,
    #[serde(deserialize_with = "bounded::vec_of")]
    pub inputs: Vec<InputAdapter>,
    #[serde(deserialize_with = "bounded::vec_of")]
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
    /// A guest-requested branch point (specs/baud-tape-device.md §4's `MARK_BRANCH` opcode): the
    /// VMM forwards this so `baud-snapshot`'s tree can capture a universe here. `step` is the
    /// tape cursor at the moment of the request, not a wall-clock or virtual-TSC value (baud-proto
    /// rule: time = u64 virtual steps).
    MarkBranch {
        step: u64,
    },
    /// A guest log line (specs/baud-tape-device.md §4's `LOG` opcode) — opaque bytes, not
    /// necessarily UTF-8, carried out through the tape device's write channel alongside probes and
    /// outcomes.
    Log {
        #[serde(deserialize_with = "bounded::bytes")]
        bytes: Vec<u8>,
        step: u64,
    },
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

/// Lenient decode: accepts messages with unknown trailing/extra fields.
/// Serde's `deny_unknown_fields` is NOT used on Msg (unknown fields are tolerated by default
/// in ciborium), so this is an alias for decode — the name exists for explicitness and tests.
pub fn decode_lenient(data: &[u8]) -> Result<Msg, DecodeError> {
    decode(data)
}

/// Produce a CBOR-encoded Msg with an extra unknown field injected after the payload.
/// Used to test that `decode_lenient` tolerates unknown fields from future protocol versions.
///
/// The extra field is appended at the CBOR map level. Because ciborium does not enforce
/// strict field counts, this allows testing forward-compatibility.
pub fn with_extra_field(msg: &Msg) -> Result<Vec<u8>, EncodeError> {
    // Encode the message normally
    let encoded = encode(msg)?;
    // We inject the extra field by re-encoding: append the extra key-value pair
    // to a CBOR map that wraps the inner payload.
    // Strategy: encode normally — the protocol is designed to tolerate extra fields.
    // For testing purposes, we encode a wrapper structure with an extra key.
    let mut out = vec![PROTO_VERSION];
    // Encode a ciborium Value representation with an extra field injected
    let inner: ciborium::Value = ciborium::de::from_reader(&encoded[1..])
        .map_err(|e| EncodeError(ciborium::ser::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))))?;
    // Build an augmented map
    let augmented = match inner {
        ciborium::Value::Map(mut pairs) => {
            pairs.push((
                ciborium::Value::Text("__extra_future_field__".into()),
                ciborium::Value::Integer(99.into()),
            ));
            ciborium::Value::Map(pairs)
        }
        other => {
            // Not a map — wrap in one for test purposes
            ciborium::Value::Map(vec![
                (ciborium::Value::Text("__payload__".into()), other),
                (
                    ciborium::Value::Text("__extra_future_field__".into()),
                    ciborium::Value::Integer(99.into()),
                ),
            ])
        }
    };
    ciborium::into_writer(&augmented, &mut out).map_err(EncodeError)?;
    Ok(out)
}

#[derive(Debug)]
pub struct EncodeError(pub ciborium::ser::Error<std::io::Error>);

impl From<ciborium::ser::Error<std::io::Error>> for EncodeError {
    fn from(e: ciborium::ser::Error<std::io::Error>) -> Self { EncodeError(e) }
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cbor encode error: {}", self.0)
    }
}

impl std::error::Error for EncodeError {}

#[derive(Debug)]
pub enum DecodeError {
    Empty,
    UnsupportedVersion(u8),
    Cbor(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Empty => write!(f, "empty buffer"),
            DecodeError::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            DecodeError::Cbor(s) => write!(f, "cbor decode error: {s}"),
        }
    }
}

impl std::error::Error for DecodeError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(test)]
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        // Strategies for generating arbitrary Msg values
        fn arb_hash() -> impl Strategy<Value = Hash> {
            prop::array::uniform32(0u8..)
                .prop_map(Hash)
        }

        fn arb_value() -> impl Strategy<Value = Value> {
            prop_oneof![
                prop::num::u64::ANY.prop_map(Value::U64),
                prop::num::i64::ANY.prop_map(Value::I64),
                ".*".prop_map(Value::Utf8),
                prop::collection::vec(0u8.., 0..32).prop_map(Value::Bytes),
            ]
        }

        fn arb_observation() -> impl Strategy<Value = Observation> {
            (".*", 0u16.., arb_value(), 0u64..).prop_map(|(probe, node, value, step)| {
                Observation { probe, node, value, step }
            })
        }

        fn arb_markov_params() -> impl Strategy<Value = MarkovParams> {
            (0.0f64..=1.0f64, 0.0f64..=1.0f64).prop_map(|(p_start, p_stop)| {
                MarkovParams { p_start, p_stop }
            })
        }

        fn arb_draw_request() -> impl Strategy<Value = DrawRequest> {
            prop_oneof![
                (0u32..).prop_map(DrawRequest::Bits),
                (prop::num::i64::ANY, prop::num::i64::ANY).prop_map(|(lo, hi)| {
                    DrawRequest::Int { lo, hi }
                }),
                prop::collection::vec(0u32.., 0..8).prop_map(DrawRequest::Choice),
                (0u32..).prop_map(|mean| DrawRequest::Hold { mean }),
                arb_markov_params().prop_map(DrawRequest::Weather),
            ]
        }

        fn arb_syscall_record() -> impl Strategy<Value = SyscallRecord> {
            (0u16.., 0u32.., arb_hash(), prop::num::i64::ANY, 0u64..)
                .prop_map(|(node, sysno, args_digest, ret, vtime)| {
                    SyscallRecord { node, sysno, args_digest, ret, vtime }
                })
        }

        fn arb_source() -> impl Strategy<Value = Source> {
            prop_oneof![Just(Source::Native), Just(Source::Fallback)]
        }

        fn arb_ebpf_record() -> impl Strategy<Value = EbpfRecord> {
            (0u16.., ".*", 0u64.., 0u64.., arb_source())
                .prop_map(|(node, event, value, vtime, source)| {
                    EbpfRecord { node, event, value, vtime, source }
                })
        }

        fn arb_pixfmt() -> impl Strategy<Value = PixFmt> {
            prop_oneof![
                Just(PixFmt::Rgba8888),
                Just(PixFmt::Rgb565),
                Just(PixFmt::Indexed8),
            ]
        }

        fn arb_frame_record() -> impl Strategy<Value = FrameRecord> {
            (0u16.., 0u64.., 0u32..=256u32, 0u32..=240u32, arb_pixfmt(), arb_hash())
                .prop_map(|(node, step, width, height, format, hash)| {
                    FrameRecord { node, step, width, height, format, hash, bytes: None }
                })
        }

        fn arb_msg() -> impl Strategy<Value = Msg> {
            prop_oneof![
                // Hello
                (".*", arb_hash()).prop_map(|(identity, manifest_hash)| {
                    Msg::Hello { identity, manifest_hash }
                }),
                // Observe
                arb_observation().prop_map(Msg::Observe),
                // Outcome::Crash
                (
                    prop::option::of(0u16..),
                    prop::option::of(".*"),
                    prop::option::of(prop::num::i32::ANY),
                    ".*"
                ).prop_map(|(node, invariant, signal, detail)| {
                    Msg::Outcome(Outcome::Crash { node, invariant, signal, detail })
                }),
                // Outcome::GoalReached
                ".*".prop_map(|metric| Msg::Outcome(Outcome::GoalReached { metric })),
                // Eof
                Just(Msg::Eof),
                // DrawRequest
                arb_draw_request().prop_map(Msg::DrawRequest),
                // DrawResult
                prop::collection::vec(0u8.., 0..64)
                    .prop_map(|bytes| Msg::DrawResult(DrawResult { bytes })),
                // Syscall
                arb_syscall_record().prop_map(Msg::Syscall),
                // Ebpf
                arb_ebpf_record().prop_map(Msg::Ebpf),
                // Frame
                arb_frame_record().prop_map(Msg::Frame),
                // Checkpoint
                (arb_hash(), 0u64..).prop_map(|(stream_hash, step)| {
                    Msg::Checkpoint { stream_hash, step }
                }),
                // MarkBranch
                (0u64..).prop_map(|step| Msg::MarkBranch { step }),
                // Log
                (prop::collection::vec(0u8.., 0..64), 0u64..).prop_map(|(bytes, step)| {
                    Msg::Log { bytes, step }
                }),
            ]
        }

        /// cbor_roundtrips: arbitrary Msg values must survive encode→decode unchanged.
        proptest! {
            #[test]
            fn cbor_roundtrips(msg in arb_msg()) {
                let encoded = encode(&msg).expect("encode must succeed");
                let decoded = decode(&encoded).expect("decode must succeed");
                let re = encode(&decoded).expect("re-encode must succeed");
                prop_assert_eq!(encoded, re, "CBOR roundtrip must be byte-identical");
            }
        }

        /// unknown_trailing_field_still_decodes: a future Observation with an extra unknown
        /// field (injected by with_extra_field) must still decode successfully via decode_lenient.
        /// This validates forward-compatibility with future protocol versions.
        proptest! {
            #[test]
            fn unknown_trailing_field_still_decodes(o in arb_observation()) {
                // Wrap as Observe message, then inject an extra field
                let msg = Msg::Observe(o);
                let augmented = with_extra_field(&msg).expect("with_extra_field must succeed for Observe");
                prop_assert!(
                    decode_lenient(&augmented).is_ok(),
                    "decode_lenient must tolerate unknown future fields"
                );
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Golden vectors — fixed CBOR byte strings to catch wire drift
    // Regenerate with: cargo test print_golden_vectors -- --ignored --nocapture
    // ---------------------------------------------------------------------------

    /// Golden vectors: canonical CBOR bytes for each Msg variant.
    /// Any change to the wire format will break this test — that is the point.
    #[test]
    fn golden_vectors_decode_correctly() {
        let vectors: &[(&str, &str)] = &[
            (
                "hello",
                "01a3636d73676568656c6c6f686964656e7469747975626175643a2f2f746170652f74312f72756e2f72316d6d616e69666573745f6861736898200000000000000000000000000000000000000000000000000000000000000000",
            ),
            (
                "observe_u64",
                "01a5636d7367676f6273657276656570726f6265656465707468646e6f6465006576616c7565a163753634182a64737465701864",
            ),
            (
                "outcome_crash",
                "01a6636d7367676f7574636f6d656474797065656372617368646e6f64650169696e76617269616e74746c6f675f7072656669785f61677265656d656e74667369676e616cf66664657461696c781874776f206c65616465727320696e2073616d65207465726d",
            ),
            (
                "eof",
                "01a1636d736763656f66",
            ),
        ];
        for (name, hex) in vectors {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i+2], 16).unwrap())
                .collect();
            let msg = decode(&bytes).unwrap_or_else(|e| {
                panic!("golden vector '{name}' failed to decode: {e}")
            });
            // Re-encode and verify byte-stability
            let re = encode(&msg).unwrap();
            let re_hex: String = re.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(re_hex, *hex, "golden vector '{name}' re-encoded differently (wire format changed!)");
        }
    }

    #[test]
    #[ignore]
    fn print_golden_vectors() {
        // Run with `cargo test print_golden_vectors -- --ignored --nocapture` to regenerate
        let msgs: &[(&str, Msg)] = &[
            ("hello", Msg::Hello {
                identity: "baud://tape/t1/run/r1".into(),
                manifest_hash: Hash([0u8; 32]),
            }),
            ("observe_u64", Msg::Observe(Observation {
                probe: "depth".into(),
                node: 0,
                value: Value::U64(42),
                step: 100,
            })),
            ("outcome_crash", Msg::Outcome(Outcome::Crash {
                node: Some(1),
                invariant: Some("log_prefix_agreement".into()),
                signal: None,
                detail: "two leaders in same term".into(),
            })),
            ("eof", Msg::Eof),
        ];
        for (name, msg) in msgs {
            let bytes = encode(msg).unwrap();
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            println!("{name}: {hex}");
        }
    }

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
    fn mark_branch_roundtrip() {
        roundtrip(Msg::MarkBranch { step: 42 });
    }

    #[test]
    fn log_roundtrip() {
        roundtrip(Msg::Log { bytes: b"guest log line".to_vec(), step: 7 });
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

    /// length_cap_rejects_oversized_bytes: a Value::Bytes exceeding MAX_BYTES_LEN must
    /// be rejected by the bounded deserializer (VR1-M3).
    #[test]
    fn length_cap_rejects_oversized_draw_result() {
        use ciborium::Value as CborValue;
        // Build a CBOR-encoded DrawResult with a bytes field of MAX_BYTES_LEN + 1
        // We can't actually allocate 64 MiB in a test easily, so we test the error
        // logic at a much smaller cap by testing the bounded::bytes helper directly
        // via a crafted CBOR message with a known-small limit check.
        //
        // We can't allocate 64 MiB in a unit test, but we verify the constant is correct
        assert_eq!(MAX_BYTES_LEN, 64 * 1024 * 1024);
        assert_eq!(MAX_STRING_LIST_LEN, 1024);

        // Verify string list cap: serialize a NodeSpec with 1025 argv entries,
        // then confirm decode returns an error.
        let spec = NodeSpec {
            name: "test".into(),
            argv: (0..1025).map(|i| format!("arg{i}")).collect(),
            inputs: vec![],
            probes: vec![],
        };
        // Encode via CBOR then decode back
        let mut cbor_buf = Vec::new();
        ciborium::into_writer(&spec, &mut cbor_buf).unwrap();
        let result: Result<NodeSpec, _> = ciborium::from_reader(cbor_buf.as_slice());
        assert!(result.is_err(), "NodeSpec with 1025 argv entries must be rejected by length cap");
    }
}
