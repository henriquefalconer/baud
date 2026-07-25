<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Stream Specification

**Status:** Planned\
**Version:** 2.0\
**Last Updated:** 2026-07-25

---

## 1. Overview

### Purpose

`baud-stream` captures, fingerprints, and renders the graphical surface of any guest that declares one. It
treats a display as a stream of raw byte buffers, hashes each frame for determinism and strategy, and
regenerates pixels on demand by deterministic replay — so video is a derived artifact of the tape, not a
recording.

### Goals

- **Surface-agnostic**: any declared byte surface streams (framebuffer, cell grid, anything)
- **Determinism participation**: frame hashes join double-run verification and can drive strategy
- **Storage discipline**: journal hashes during fuzzing; regenerate pixels on demand
- **Dependency-light encoding**: in-crate QOI and Y4M writers, no codec dependency

### Non-Goals

- Knowing what a frame depicts
- Bundling a video codec (users bring their own ffmpeg for mp4)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                 baud-stream                  │
│  ingest raw frames → validate → blake3 hash    │
│  render (replay w/ capture) · QOI · Y4M        │
└──────────────────────────────────────────────┘
        ▲ fed by the tape device's FRAME opcode (specs/baud-tape-device.md §4)
```

### Rationale

- Deps = `{baud-proto, blake3}`; QOI (~300 LOC) and Y4M writers in-crate; soft budget ≤ 1,200 LOC.
- Knows byte surfaces, dimensions, and formats — never content.

---

## 3. Display Adapter

A guest emits a frame through the one paravirtual tape device — there is no separate display device and
no side channel. At each frame boundary the guest (or its bridge fixture) writes the frame's header
(pixel format ∈ `rgba8888|rgb565|indexed8`, then width, then height) followed by the raw pixel buffer to
the tape device's `DATA` port, and finalizes the record with the `FRAME` control opcode (opcode 5) on the
`CONTROL` port.

**specs/baud-tape-device.md §4 is the single source of truth for that byte layout** — it is deliberately
not restated here.

The VMM blake3-hashes the pixel bytes and forwards `baud_proto::Msg::Frame(FrameRecord)` —
`{node, step, width, height, format, hash, bytes}` — to this crate. Geometry is thus declared per frame by
the guest itself rather than once up front, and the tape device does not check it: validating the buffer
length against the declared geometry is this crate's job (§4).

---

## 4. Ingest & Fingerprint

| Step | Behavior |
| ------------- | ------------------------------------------------ |
| Validate      | Buffer length must equal `width×height×format`; mismatch → `Crash{detail:"frame-format"}` |
| Fingerprint   | blake3 per frame → `Observe{probe:"<node>.frame_hash", value: hash}` at the frame's step |
| Participate   | Frame hashes join `verify determinism`; usable as `buckets` strategy input (explore by distinct screens) |

---

## 5. Storage Discipline

- During fuzz runs, **only hashes are journaled — never pixel bytes**.
- Pixels are regenerated on demand: `stream render` replays the tape prefix under the supervisor with
  capture enabled and materializes frames.
- Rendered frames are stored content-addressed (identical frames stored once; unchanged screens collapse).

---

## 6. Encoding & Live View

| Output | Format |
| ------------- | ------------------------------------------------ |
| Single frame  | QOI (in-crate encoder) |
| Sequence      | Y4M (raw, pipeable → user's ffmpeg for mp4) |
| Live          | `--stream` runs and `stream render` forward `FrameRecord`s over `baud-tape-agent`'s stream transport; server re-serves via SSE |

### Commands

```
baud stream tail   --run <id> [--node I] [-o out.y4m] [--hashes-only]
baud stream render --run <id> [--from-step A --to-step B] [--format qoi-seq|y4m] -o PATH
baud stream frames --run <id> [--node I]
```

---

## 7. Testing

```rust
#[test]
fn frame_hashes_double_run_identical() {
    assert_eq!(run1.frame_hashes(), run2.frame_hashes());
}

#[test]
fn render_is_byte_identical() {
    assert_eq!(render(run, Y4m), render(run, Y4m));
}

#[test]
fn bad_geometry_is_a_crash() {
    assert!(matches!(ingest(short_buffer()), Outcome::Crash { .. }));
}
```

- **framebuffer-guest (H-series)**: `crates/baud-multiverse/tests/fixtures/framebuffer-guest/` emits one
  `indexed8` frame via §3's `FRAME` opcode;
  `linux::tests::framebuffer_guest_frame_is_reproducible_across_boots` proves the frame hash is identical
  across two boots on real KVM, and that it matches `baud-stream`'s own `fingerprint`.
- **framedemo (M5)**: a guest writing a moving `indexed8` gradient; the tests above pass on it.
- **Mario (M8)**: `stream render -o mario-completion.y4m` produces the watchable completion video from the
  tape alone.

---

## 8. Storage Considerations

| Concern            | Handling                                    |
| ------------------ | ------------------------------------------- |
| Video size         | Costs tape-sized storage; pixels derived, not stored |
| Repeated frames    | Content addressing collapses them           |
| Any past step      | Renderable after the fact from the journal  |

---

## 9. Security Considerations

| Threat                        | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| Sensitive pixels journaled    | Only frame hashes are journaled; pixels regenerated on demand |
| Malformed frame buffer        | Length validated against declared geometry; mismatch is a `Crash` |
| Rendered artifacts leak       | Rendered frames follow the journal's at-rest encryption |

---

## 10. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| More formats       | rgb888/gray8 surfaces; an audio channel        |
| Perceptual bucketing | Downsampled hashes for near-duplicate screen exploration |
