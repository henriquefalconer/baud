<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Proto Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-proto` defines every wire and domain type shared across baud: run manifests, tape chunks,
observations, probe/strategy/tactics specs, and the protocol messages exchanged between the supervisor,
agent, and server. It is the one crate everything depends on and that depends on nothing.

### Goals

- **Single source of types**: no component defines its own copy of a shared shape
- **Stable wire format**: versioned CBOR, forward-compatible with unknown fields
- **No behavior**: pure data, no IO, no async
- **Opaque probe values**: the type system carries no workload meaning

### Non-Goals

- Serialization transport (that is each component's concern)
- Any temporal/formula type (properties are crash / invariant / goal only)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                 baud-proto                   │
│  serde + ciborium types only. No IO.           │
└──────────────────────────────────────────────┘
        ▲  (depended on by every other crate)
```

### Rationale

- Deps = `{serde, ciborium}` only; no `tokio`, no `chrono` (time is `u64` virtual steps).
- Soft budget ≤ 700 LOC.

---

## 3. Core Types

### Manifest & Run

```rust
struct RunManifest {
    spec_hash: Hash,
    closure_hash: Hash,
    seed: u64,
    strategy: StrategySpec,
    tactics: TacticsSpec,
    cpu_class: String,      // recorded for reconstruction pinning
    layout: MemLayout,      // brk/stack, recorded
}
```

### Tape & Draws

```rust
struct ChoiceChunk { step: u64, bytes: Vec<u8> }
enum DrawRequest { Bits(u32), Int{lo:i64,hi:i64}, Choice(Vec<u32>), Hold{mean:u32}, Weather(MarkovParams) }
struct DrawResult { bytes: Vec<u8> }
```

### Observation Vocabulary

```rust
enum Value { U64(u64), I64(i64), Bytes(Vec<u8>), Utf8(String), Hash(Hash) }
struct Observation { probe: String, node: u16, value: Value, step: u64 }
```

There are exactly three outcome messages. No temporal types exist; a harness needing "eventually" encodes
an invariant or goal probe instead.

```rust
enum Outcome {
    GoalReached { metric: String },
    Crash { node: Option<u16>, invariant: Option<String>, signal: Option<i32>, detail: String },
}
```

### Records

```rust
struct SyscallRecord { node: u16, sysno: u32, args_digest: Hash, ret: i64, vtime: u64 }
struct EbpfRecord   { node: u16, event: String, value: u64, vtime: u64, source: Source } // Native | Fallback
struct FrameRecord  { node: u16, step: u64, width: u32, height: u32, format: PixFmt, hash: Hash, bytes: Option<Vec<u8>> }
```

---

## 4. Spec Types

```rust
struct ProbeSpec    { name: String, adapter: ProbeAdapter }
struct StrategySpec { maximize: Vec<String>, buckets: Vec<String>, reservoir: Option<Reservoir>, goal: Option<Predicate> }
struct TacticsSpec  { input: Vec<Weighted<InputTactic>>, weather: Vec<Weighted<WeatherTactic>>, schedule: Option<SwitchBias> }
struct NodeSpec     { name: String, argv: Vec<String>, inputs: Vec<InputAdapter>, probes: Vec<ProbeSpec> }
```

---

## 5. Protocol Messages

```rust
enum Msg {
    Hello { identity: String, manifest_hash: Hash },
    DrawRequest(DrawRequest),
    DrawResult(DrawResult),
    Observe(Observation),
    Syscall(SyscallRecord),
    Ebpf(EbpfRecord),
    Frame(FrameRecord),
    Checkpoint { stream_hash: Hash, step: u64 },
    Outcome(Outcome),
    MarkBranch { step: u64 },       // guest-requested branch point (baud-tape-device MARK_BRANCH)
    Log { bytes: Vec<u8>, step: u64 }, // guest log line (baud-tape-device LOG)
    Eof,
}
```

Every message carries a leading version byte; decoders tolerate unknown trailing fields.

---

## 6. Testing

```rust
proptest! {
    #[test]
    fn cbor_roundtrips(m in any::<Msg>()) {
        prop_assert_eq!(decode(&encode(&m)), m);
    }

    #[test]
    fn unknown_trailing_field_still_decodes(o in any::<Observation>()) {
        prop_assert!(decode_lenient(&with_extra_field(&o)).is_ok());
    }
}
```

- Golden vectors: fixed CBOR byte strings checked in, so wire drift is caught.

---

## 7. Compatibility Considerations

| Change                     | Rule                                             |
| -------------------------- | ------------------------------------------------ |
| Add a field                | Allowed; must be optional or defaulted           |
| Remove/repurpose a field   | Requires a version-byte bump                     |
| Add an enum variant        | Allowed; decoders treat unknown variants as errors only where semantically required |

---

## 8. Security Considerations

| Threat                          | Handling                                    |
| ------------------------------- | ------------------------------------------- |
| Hostile/oversized CBOR          | Bounded decoders; length caps on collection fields |
| Version confusion               | Leading version byte; incompatible majors rejected |
| Sensitive data in transit       | Carries opaque `Value`; secrets wrapped as `Secret<T>` upstream |

---

## 9. Future Considerations

| Feature                 | Description                                  |
| ----------------------- | -------------------------------------------- |
| Golden-vector registry  | Track the wire format across tool versions   |
| Optional compression    | zstd chunk bodies before journaling          |
