# baud Protocol

## Overview

The baud protocol is a **Hegel-like** protocol: the driver is the source of all
randomness; the supervisor's device models *request* draws; recorded results ARE
the tape; replay = feed the tape back; shrink = edit the tape and replay.

This document specifies the wire protocol between the baud-tape-agent (inside
the sandbox) and baud-server (on the dev machine), and between baud-multiverse
(the supervisor inside the sandbox) and the agent.

---

## Transport

- **Agent ↔ Server**: WebSocket over the Daytona preview URL, token-authenticated
  (baud-identity JWT).  Fallback: batch CBOR files via Daytona exec/file API.
- **Supervisor ↔ Agent**: Unix socket (supervisor is a child of the agent).
- **Encoding**: CBOR (ciborium) everywhere; messages carry a version byte and
  tolerate unknown fields.

---

## Message types

All messages are versioned CBOR structs defined in `baud-proto`.

### Hello
```
Hello {
    identity: String,       // JWT token (baud://tape/<id>/run/<run-id>)
    manifest_hash: [u8; 32] // blake3 of RunManifest (CBOR)
}
```
Sent by the agent upon WebSocket connect.  The server verifies the JWT and
manifest hash before accepting draw requests.

### DrawRequest / DrawResult
```
DrawRequest {
    kind:   DrawKind,       // Bits | Int | Choice | Hold | Weather
    bounds: DrawBounds,     // kind-specific bounds
}

DrawResult {
    bytes: Vec<u8>,         // the drawn bytes; interpreted per kind
}
```
Device models call `DrawRequest`; the driver produces `DrawResult`.  The pair
IS the tape: recording = store `DrawResult` bytes in order; replay = feed them
back.

#### DrawKind variants
| Kind | Bounds | Semantics |
|---|---|---|
| `Bits(n)` | n: u8 (1–64) | n random bits |
| `Int` | lo: i64, hi: i64 | uniform integer in [lo, hi] |
| `Choice` | weights: Vec<u64> | weighted discrete choice |
| `Hold` | geom_mean: f64 | geometric-distribution duration |
| `Weather` | markov_params: MarkovParams | Markov-chain state draw |

### Observe
```
Observe {
    probe_id: String,       // probe name from spec
    node:     Option<u32>,  // node index
    value:    Value,        // Int(i64) | Float(f64) | Bytes(Vec<u8>) | Text(String)
    step:     u64,          // virtual step (draw count)
}
```
Emitted by probe adapters; journaled; fed to the strategy scorer.

### SyscallRecord
```
SyscallRecord {
    node:       u32,
    sysno:      u32,
    args_digest: [u8; 8],  // first 8 bytes of blake3(args)
    ret:        i64,
    vtime:      u64,       // virtual timestamp
}
```
Emitted by baud-multiverse for every mediated syscall.  Plane 1.

### EbpfRecord
```
EbpfRecord {
    node:       Option<u32>,
    event_type: String,     // sched | exec | syscall | fault
    sysno:      Option<u32>,
    pid:        u32,
    vtime:      u64,
    source:     String,     // "ebpf" | "fallback"
}
```
Emitted by baud-tracing.  Plane 2.

### FrameRecord
```
FrameRecord {
    node:   u32,
    step:   u64,
    width:  u32,
    height: u32,
    format: FrameFormat,    // Rgba8888 | Rgb565 | Indexed8
    hash:   [u8; 32],       // blake3 of raw frame bytes
    bytes:  Option<Vec<u8>> // absent in hash-only mode; present during render
}
```
Emitted by the `frame` display adapter.  In fuzz runs only hashes are journaled;
bytes are materialized on demand via `baud stream render`.

### Checkpoint
```
Checkpoint {
    stream_hash: [u8; 32],  // blake3 of all Observe records up to this step
    step:        u64,
}
```
Emitted periodically; used for prefix-equality checks during replay.

### GoalReached
```
GoalReached {
    metric: f64,
}
```
Emitted when the goal predicate in `StrategySpec` is satisfied.  Causes the
run to exit with code 2.

### Crash
```
Crash {
    node:      Option<u32>,
    invariant: Option<String>, // invariant name if checked by harness
    signal:    Option<i32>,    // OS signal number if killed
    detail:    String,         // human-readable description
}
```
Emitted on guest contract violation, invariant failure, frame format mismatch,
or guest signal.  Causes the run to exit with code 2.

### Eof
```
Eof {}
```
Sent by the agent when the workload is complete (all guests exited normally).

---

## Session lifecycle

```
Agent                              Server
  |-- Hello{identity, manifest_hash} -->|
  |<- (accept / reject JWT)            |
  |                                     |
  [for each draw:]
  |-- DrawRequest{kind, bounds} ------>|
  |<- DrawResult{bytes} ---------------|
  |                                     |
  [for each probe sample:]
  |-- Observe{...} ------------------>|
  |-- SyscallRecord{...} ------------>| (plane 1)
  |-- EbpfRecord{...} --------------->| (plane 2)
  |-- FrameRecord{...} -------------->| (display)
  |-- Checkpoint{...} --------------->|
  |                                     |
  [on goal / crash / normal end:]
  |-- GoalReached{...} / Crash{...}  ->|
  |-- Eof{} -------------------------->|
```

---

## Tape format

A tape is a flat sequence of `DrawResult.bytes` concatenated in draw order,
prefixed with a header:

```
TapeHeader {
    version:       u8,         // currently 1
    seed:          u64,
    manifest_hash: [u8; 32],
    strategy_hash: [u8; 32],
    tactics_hash:  [u8; 32],
}
ChoiceChunk {
    step:  u64,
    bytes: Vec<u8>,
}
```

Chunks are CBOR-encoded and CBOR-concatenated (streaming CBOR).  The tape is
content-addressed by `blake3(plaintext)` and encrypted at rest (age).

---

## Journal format

The journal stores opaque probe values, draw bytes, syscall records, eBPF
records, and frame hashes.

- Append-only CBOR chunk files.
- Content addressing: `blake3(plaintext)` → file path under `journal/`.
- Encryption: each chunk is age-encrypted; address is computed over plaintext.
- Index: `(run_id, step)` → chunk address (stored in SQLite, unencrypted).
- Readers are streaming iterators; no compaction; no database for chunk bodies.

---

## Shrink protocol

Shrinking edits the tape and replays to find the minimal tape that reproduces a
violation:

1. **Chunk deletion**: try removing each choice chunk; replay; keep if violation
   still occurs.
2. **Zeroing**: replace chunk bytes with zeros; replay; keep if violation still
   occurs.
3. **Hold-shortening**: for `Hold` draws, reduce `geom_mean` and retry.
4. **Dedup**: merge identical consecutive chunks.

Shrinking batches many candidate tapes inside one sandbox process (never one
sandbox per trial) to stay within the 1-minute sandbox economics.

---

## Verification commands

| Command | What it checks |
|---|---|
| `baud verify determinism --spec S --seed N` | Two runs, same seed → byte-identical observation stream hashes |
| `baud verify observation --run R` | Plane 1 (syscall log) vs plane 2 (eBPF) per-guest syscall counts agree |

---

## Error semantics

| Situation | Result |
|---|---|
| Guest violates contract | `Crash{signal?, detail}`, run marked `crashed` |
| Invariant violated | `Crash{invariant, detail}`, exit code 2 |
| Goal reached | `GoalReached{metric}`, exit code 2 |
| Normal completion | `Eof`, exit code 0 |
| Determinism failure | First divergent step reported, run marked `unusable` |
| Observation cross-check failure | Disagreement reported, run flagged |
| Frame format mismatch | `Crash{detail: "frame-format"}` |

---

## Token format

Agent connections authenticate with an ed25519-signed JWT:

```
Header: { alg: "EdDSA", typ: "JWT" }
Payload: {
    sub: "baud://tape/<sandbox-id>/run/<run-id>[/node/<i>]",
    iat: <unix timestamp>,
    exp: <iat + 600>,   // 10-minute TTL
    jti: "<uuid>",      // replay prevention
}
```

The server is the sole trust root (holds the ed25519 private key).  Tokens are
held as `SecretString`; they never appear in logs.  Unauthenticated WebSocket
connections are refused.
