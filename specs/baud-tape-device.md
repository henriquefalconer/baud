<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Tape Device Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-24

---

## 1. Overview

### Purpose

`baud-tape-device` is the one paravirtual device through which the guest does all input and output. It is
the sole nondeterministic-input channel: every byte the guest reads that would otherwise be nondeterministic
(entropy, external input, a simulated device response) comes from the tape; every observation the guest
emits goes out through it. It also carries control requests (mark a branch point, signal an outcome).

### Goals

- **Single input channel**: all nondeterministic input funnels through one device fed by the tape
- **Bidirectional**: guest reads tape bytes; guest writes probes, logs, outcomes, and control requests
- **No real devices**: replaces disk, network, RNG, and clock-beyond-the-work-clock

### Non-Goals

- Emulating a real hardware device faithfully (it is a baud-specific transport)
- Interpreting workload semantics (values are opaque; the driver/agent give them meaning)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│               baud-tape-device                 │
│  PIO/MMIO device model (host side)             │
│  guest-side driver contract (headers)          │
└───────────────────┬──────────────────────────┘
        ▲ served on the vCPU bus by baud-vcpu
        │ types from baud-proto
```

### Rationale

- Deps = `{baud-proto}`; the host-side model is pure logic over a tape cursor. Soft budget ≤ 900 LOC.
- The guest-side driver is a tiny kernel shim shipped in the image (see `baud-packages`).

### Types & API

```rust
pub struct TapeDevice {
    cursor: u64,          // advances only on guest reads
    tape: TapeSource,     // the sole nondeterministic input
    outbound: Vec<u8>,    // current record being written by the guest
}

impl TapeDevice {
    pub fn pio_read(&mut self, off: u16) -> u8;        // 0x00 → next tape byte; 0x10 → status
    pub fn pio_write(&mut self, off: u16, b: u8);      // 0x00 → append outbound; 0x08 → control opcode
    pub fn drain_records(&mut self) -> Vec<Msg>;       // PROBE/MARK_BRANCH/GOAL/VIOLATION/LOG/FRAME → baud-proto
    pub fn cursor(&self) -> u64;                        // captured in a Universe (baud-snapshot)
}
```

---

## 3. Register Interface (PIO/MMIO)

| Offset | Direction | Meaning |
| ------ | --------- | ------------------------------------------ |
| `0x00` | read      | Next tape byte (advances the cursor) |
| `0x00` | write     | Append one byte to the outbound record |
| `0x08` | write     | Control opcode (see §4) |
| `0x10` | read      | Status (bytes-remaining, last-opcode-result) |

The whole surface is exit-served by `baud-vcpu`; a read pops the tape cursor, a write appends to the current
outbound record. All values are opaque bytes.

---

## 4. Control Opcodes (guest → VMM)

| Opcode | Meaning |
| ------------------- | ------------------------------------------ |
| `PROBE(key,value)`  | Emit an observation `key=value` |
| `MARK_BRANCH`       | Request a snapshot here (a branch point) |
| `GOAL(metric)`      | Emit `GoalReached` |
| `VIOLATION(inv)`    | Emit `Crash{invariant}` |
| `LOG(bytes)`        | Emit a log line |
| `FRAME(format,width,height,pixels)` | Emit one graphical-surface frame for `baud-stream` |

Opcodes map to `baud-proto` messages the VMM forwards to the server/driver.

`FRAME` is the display adapter specs/baud-stream.md §3 describes, and this section is that byte
layout's single source of truth: a guest (or its bridge fixture) writes a one-byte pixel-format
tag, the little-endian `u32` width, the little-endian `u32` height, then the raw pixel bytes to
`DATA`, then finalizes with this opcode.
The VMM hashes the pixel bytes (blake3) and forwards a `baud_proto::Msg::Frame(FrameRecord)` —
geometry validation (buffer length vs. `width × height × bytes-per-pixel`) is `baud-stream`'s job,
not this device's; a short header or unrecognized format byte is the only way this opcode reports
`MalformedPayload`. This is deliberately *not* a new device — specs/baud-multiverse.md's non-goal
"real device emulation beyond the console + tape device" stays true, since a frame rides the same
transport as every other control record.

---

## 5. Determinism Properties

- The cursor advances only on guest reads; given the same tape, the byte sequence the guest observes is
  fixed.
- Reads past end-of-tape return a fixed sentinel and set a status bit (never host entropy).
- The device holds no wall-clock, no host randomness, no real I/O — it is a pure function of the tape and the
  guest's own writes.

---

## 6. Testing

```rust
#[test] fn all_input_is_tape_derived() {
    let a = run(io_guest(), tape.clone()).guest_output();
    let b = run(io_guest(), tape.clone()).guest_output();
    assert_eq!(a, b);                       // same tape → same output
    let c = run(io_guest(), flip_one_byte(&tape)).guest_output();
    assert_ne!(a, c);                       // input actually flows from the tape
}

#[test] fn read_past_end_is_fixed() {
    let out = run(drain_guest(), short_tape());
    assert!(out.hit_eot_sentinel && out.is_deterministic_double_run());
}
```

---

## 7. Security Considerations

| Threat | Handling |
| ------------------------------ | ------------------------------------------ |
| Guest reads host entropy       | Impossible — the device has none; EOT returns a fixed sentinel |
| Nondeterministic device response | All responses are tape bytes; no real I/O exists |
| Probe values carry secrets     | Values are opaque bytes; the store encrypts them at rest |

---

## 8. Future Considerations

| Feature | Description |
| ------------------ | ---------------------------------------------- |
| Virtio transport   | A virtio-mmio front end for higher-throughput guest I/O |
| Multiple tapes     | Separate cursors for input vs fault/weather streams |
