<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Journal Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-journal` is the durable record of a run and the engine of reconstruction. It appends every draw and
observation content-addressed, and it can rebuild any run in a fresh sandbox from the journal alone by
replaying the tape and verifying observation-stream equality.

### Goals

- **Journal-first durability**: the server's only durable state; sandboxes are disposable
- **Content-addressed storage**: identical chunks stored once
- **Reconstruction**: `(manifest + tape prefix) → fresh sandbox → replay → verify → resume`
- **Divergence detection**: report the first mismatching step and its node/probe/syscall

### Non-Goals

- Mid-run state snapshots (reconstruction is replay-from-zero)
- A general database (append-only files, one index)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                baud-journal                  │
│  append-only CBOR chunks · blake3 CAS          │
│  age-encrypted at rest · streaming iterators   │
│  reconstruction                                │
└───────────────────┬──────────────────────────┘
                    │ depends on
              baud-keys  (age recipient + identity resolution)
        ▲ owned by baud-server
```

### Rationale

- No database, no compaction; index only `(run, step)`. Readers are streaming iterators.
- Stores opaque probe values, draw bytes, syscall records, eBPF records, frame hashes.
- Owns no cryptography: encryption is delegated to `age`, with the key resolved by baud-keys.

---

## 3. Encryption at Rest

A journal reproduces the entire deterministic execution — tapes, inputs, every observation — so a leaked
journal directory would reproduce any secret the workload processed. Every chunk body is therefore stored
age-encrypted.

| Rule | Behavior |
| -------------------------- | ------------------------------------------------ |
| Key source                 | age recipient (public) + identity (private path) from baud-keys |
| Content address            | `blake3(plaintext)`; ciphertext stored under it |
| Deduplication              | Keyed by plaintext hash (age is non-deterministic, so ciphertext can't dedup) |
| Verification/reconstruction| Observation-stream hashes computed over decrypted plaintext, never ciphertext |
| In clear                   | Only the `(run, step)` index and chunk addresses; chunk bodies are always ciphertext |
| Missing key                | Journal is unreadable; the key that guards `secrets/baud.enc.yaml` guards the journal (`doctor` checks) |

---

## 4. Reconstruction

```
new sandbox → same closure → replay tape 0..K under baud-multiverse
            → verify observation-stream-hash prefix equality → resume at K
```

- Replay cost is O(steps in prefix). There is no state snapshot; resuming at step K always replays 0..K.
- Shrinking therefore batches many candidate tapes inside one sandbox process (never one sandbox per trial).

---

## 5. Divergence

- On replay, the first step whose observation hash differs from the journal is reported, naming the
  node/probe/syscall that diverged (the "moved-block" detector).
- A divergent run is marked and excluded from replay/shrink/reconstruct.

---

## 6. Testing

```rust
#[test]
fn chunk_bodies_are_ciphertext() {
    journal.append(run, step, plaintext_with("sk-secret"));
    let raw = fs::read(chunk_path(run, step)).unwrap();
    assert!(!raw.windows(9).any(|w| w == b"sk-secret"));
}

#[test]
fn dedup_by_plaintext_hash() {
    let a = journal.append(run, 0, same_plaintext());
    let b = journal.append(run, 1, same_plaintext());
    assert_eq!(a.address, b.address); // one stored body
}
```

- Reconstruction determinism: a reconstructed run's hash prefix equals the original (asserted for both
  raftlet and Mario via the identical `tape reconstruct` command).
- Journal-first: a sandbox killed mid-checkpoint loses no server-acked step.

---

## 7. Storage Considerations

| Concern              | Handling                                    |
| -------------------- | ------------------------------------------- |
| Storage growth       | Content addressing; pixels never journaled (only frame hashes) |
| Index size           | Single `(run, step)` index                  |
| Corruption           | blake3 verifies chunk integrity on read     |

---

## 8. Security Considerations

| Threat                        | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| Leaked journal directory      | Chunk bodies age-encrypted at rest (baud-keys) |
| Ciphertext tampering          | blake3 over plaintext verified on decrypt   |
| Missing/rotated key           | Journal unreadable without the age identity; `doctor` checks it |

---

## 9. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| Per-run recipients | A distinct age recipient per run for compartmentalization |
| Compaction         | Optional prune of superseded shrink candidates |
