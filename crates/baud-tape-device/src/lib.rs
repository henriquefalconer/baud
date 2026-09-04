// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-tape-device — the one paravirtual device through which the guest does all input and
// output (specs/baud-tape-device.md). It is the sole nondeterministic-input channel: every byte
// the guest reads that would otherwise be nondeterministic (entropy, external input, a simulated
// device response) comes from the tape; every observation the guest emits goes out through it.
//
// Hardware-independent by design (deps = {baud-proto} only, per specs/baud-tape-device.md §2's
// "Rationale") — a pure function of the tape and the guest's own register writes, no KVM/perf
// needed to test it, same split as `baud-vcpu`'s `boundary.rs` and `baud-multiverse`'s
// `cpuid.rs`/`layout.rs`/`console.rs`. Wiring this onto the real PIO/MMIO exit bus (implementing
// `baud_vcpu::Bus`) is `baud-multiverse`'s job (specs/baud-tape-device.md §2's architecture
// diagram: "served on the vCPU bus by baud-vcpu") — see `crates/baud-multiverse/src/tape_bus.rs`.

use baud_proto::{FrameRecord, Msg, Observation, Outcome, PixFmt, Value};

/// The tape device's PIO/MMIO register offsets, relative to whatever base address/port the caller
/// maps it at (specs/baud-tape-device.md §3).
pub mod reg {
    /// Read: next tape byte (advances the cursor). Write: append one byte to the outbound record.
    pub const DATA: u16 = 0x00;
    /// Write: control opcode — finalizes the outbound record as that opcode's payload (§4).
    pub const CONTROL: u16 = 0x08;
    /// Read: status byte (§`TapeDevice::status_byte`'s encoding).
    pub const STATUS: u16 = 0x10;
}

/// The fixed byte a read past end-of-tape returns — never host entropy
/// (specs/baud-tape-device.md §5: "Reads past end-of-tape return a fixed sentinel ... never host
/// entropy"). Matches `baud_vcpu::OPEN_BUS_BYTE`'s value for the same "fixed, never real data"
/// reasoning, though this crate does not depend on `baud-vcpu` to stay hardware-independent.
pub const EOT_SENTINEL: u8 = 0xFF;

/// The byte a guest writes to [`reg::CONTROL`] to finalize the current outbound record
/// (specs/baud-tape-device.md §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlOp {
    /// `PROBE(key,value)` — emit an observation `key=value`.
    Probe = 0,
    /// `MARK_BRANCH` — request a snapshot here (a branch point).
    MarkBranch = 1,
    /// `GOAL(metric)` — emit `Outcome::GoalReached`.
    Goal = 2,
    /// `VIOLATION(inv)` — emit `Outcome::Crash`.
    Violation = 3,
    /// `LOG(bytes)` — emit a log line.
    Log = 4,
    /// `FRAME(format,width,height,pixels)` — emit one graphical-surface frame for `baud-stream`
    /// (specs/baud-stream.md §3's display adapter: "the guest ... writes length-prefixed raw
    /// frame buffers ... the supervisor's device model delivers them"). This *is* that device
    /// model — no separate VGA/virtio-gpu device is added (specs/baud-multiverse.md's non-goal
    /// "real device emulation beyond the console + tape device" stays true; a frame is just
    /// another tape-device record, the same way `LOG`/`PROBE` are).
    Frame = 5,
}

impl ControlOp {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(ControlOp::Probe),
            1 => Some(ControlOp::MarkBranch),
            2 => Some(ControlOp::Goal),
            3 => Some(ControlOp::Violation),
            4 => Some(ControlOp::Log),
            5 => Some(ControlOp::Frame),
            _ => None,
        }
    }
}

/// The result of the most recent control opcode, packed into [`reg::STATUS`]'s high bit
/// (`TapeDevice::status_byte`). Exposed so tests (and a future in-guest driver) can distinguish
/// "the VMM understood my record" from "malformed payload" / "unknown opcode" without needing to
/// inspect `drain_records`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpcodeResult {
    /// The most recent finalized record was well-formed and queued.
    Ok,
    /// [`reg::CONTROL`] was written a byte that is not a known [`ControlOp`].
    UnknownOpcode,
    /// The opcode was recognized but the outbound bytes could not be parsed as its payload
    /// (e.g. a `PROBE` whose declared key length exceeds the buffered bytes, or a `GOAL`/
    /// `VIOLATION` name that is not valid UTF-8).
    MalformedPayload,
    /// The guest attempted to create a record larger than the protocol limit.
    OversizedPayload,
}

/// The host-side model of the tape device: a pure function of the tape bytes and the guest's own
/// register writes (specs/baud-tape-device.md §5). No wall-clock, no host randomness, no real I/O.
pub struct TapeDevice {
    /// Advances only on guest reads of [`reg::DATA`] (specs/baud-tape-device.md §5).
    cursor: u64,
    /// The sole nondeterministic input: fixed for the life of one run.
    tape: Vec<u8>,
    /// The record currently being assembled by the guest's writes to [`reg::DATA`], cleared each
    /// time [`reg::CONTROL`] finalizes it.
    outbound: Vec<u8>,
    /// Finalized records the VMM has not yet drained ([`TapeDevice::drain_records`]).
    records: Vec<Msg>,
    /// Prevent a guest from growing one outbound record without bound.
    outbound_overflowed: bool,
    last_result: OpcodeResult,
    /// Set once a read has hit end-of-tape (`read_past_end_is_fixed`'s assertion target). Sticky
    /// for the life of the device — once the guest has seen the sentinel, it stays observably true
    /// even if the cursor is later rewound by a snapshot restore (out of this crate's scope).
    hit_eot: bool,
}

impl TapeDevice {
    /// Construct a device over `tape` — the run's entire nondeterministic-input budget, fixed
    /// up front (the tape is generated once by `baud-driver`, not extended mid-run).
    pub fn new(tape: Vec<u8>) -> Self {
        TapeDevice {
            cursor: 0,
            tape,
            outbound: Vec::new(),
            records: Vec::new(),
            outbound_overflowed: false,
            last_result: OpcodeResult::Ok,
            hit_eot: false,
        }
    }

    /// Serve a PIO/MMIO read at `off` (specs/baud-tape-device.md §3's exact per-byte API — a
    /// caller adapting this to `baud_vcpu::Bus`'s slice-based reads, as `baud-multiverse` does,
    /// calls this once per byte, matching real single-byte `IN` instruction semantics).
    pub fn pio_read(&mut self, off: u16) -> u8 {
        match off {
            reg::DATA => self.next_tape_byte(),
            reg::STATUS => self.status_byte(),
            // Any other offset within the device's window: not a modeled register. Return the
            // same fixed sentinel as end-of-tape rather than inventing a value — still "never
            // host memory", just an unmapped sub-register instead of an unmapped device.
            _ => EOT_SENTINEL,
        }
    }

    /// Serve a PIO/MMIO write at `off` carrying byte `b` (specs/baud-tape-device.md §3).
    pub fn pio_write(&mut self, off: u16, b: u8) {
        match off {
            reg::DATA => {
                const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
                if self.outbound.len() < MAX_RECORD_BYTES {
                    self.outbound.push(b);
                } else {
                    self.outbound_overflowed = true;
                }
            }
            reg::CONTROL => self.finalize_record(b),
            // Writes to any other offset (including the read-only STATUS register) are absorbed
            // silently — matches `baud_vcpu::OpenBusFallback`'s write side.
            _ => {}
        }
    }

    /// Take every record finalized since the last drain, in emission order. Corresponds to
    /// specs/baud-tape-device.md §3's `drain_records`.
    pub fn drain_records(&mut self) -> Vec<Msg> {
        std::mem::take(&mut self.records)
    }

    /// The tape cursor — captured in a `Universe` snapshot (`baud-snapshot::universe::DeviceState`).
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Fast-forward the cursor to `cursor` — the other half of the capture above: a `Universe`
    /// restore (`baud-snapshot::linux::restore`'s `RestoreStep::RestoreDevice`, deliberately left
    /// to the caller since this crate has no dependency on `baud-snapshot`) reconstructs a fresh
    /// `TapeDevice` over the run's full tape and must replay the cursor to exactly where the
    /// captured guest had gotten to, so the very next read continues from the same tape byte a
    /// straight run would have reached. Does not touch `hit_eot`/`records`/`outbound` — a restored
    /// universe resumes with an empty in-flight record and a clean drain queue, matching what a
    /// freshly-captured device's caller already observed and drained before the snapshot was taken.
    pub fn restore_cursor(&mut self, cursor: u64) {
        self.cursor = cursor;
    }

    /// Whether a guest read has ever run past the end of the tape (`read_past_end_is_fixed`).
    pub fn hit_eot_sentinel(&self) -> bool {
        self.hit_eot
    }

    /// The result of the most recently finalized control record.
    pub fn last_opcode_result(&self) -> OpcodeResult {
        self.last_result
    }

    fn next_tape_byte(&mut self) -> u8 {
        match self.tape.get(self.cursor as usize) {
            Some(&b) => {
                self.cursor += 1;
                b
            }
            None => {
                // Past end-of-tape: the cursor does not advance further (there is nothing left to
                // consume) but every such read still yields the same fixed sentinel, keeping a
                // guest that spins reading past EOT fully deterministic across a double-run.
                self.hit_eot = true;
                EOT_SENTINEL
            }
        }
    }

    /// Packs "bytes remaining" and "last opcode result" into one byte (specs/baud-tape-device.md
    /// §3's status register: "bytes-remaining, last-opcode-result" — the spec does not fix an
    /// exact bit layout, so this crate defines one: bits 0-6 are the remaining tape bytes,
    /// saturating at 127 so a guest merely checking "am I near the end" never needs a wider read;
    /// bit 7 is set whenever the last finalized record was NOT [`OpcodeResult::Ok`]).
    fn status_byte(&self) -> u8 {
        let remaining = self.tape.len().saturating_sub(self.cursor as usize);
        let remaining_bits = remaining.min(0x7f) as u8;
        let error_bit = match self.last_result {
            OpcodeResult::Ok => 0u8,
            OpcodeResult::UnknownOpcode | OpcodeResult::MalformedPayload | OpcodeResult::OversizedPayload => 0x80,
        };
        remaining_bits | error_bit
    }

    fn finalize_record(&mut self, opcode_byte: u8) {
        let payload = std::mem::take(&mut self.outbound);
        let overflowed = std::mem::take(&mut self.outbound_overflowed);
        let step = self.cursor;
        if overflowed {
            self.last_result = OpcodeResult::OversizedPayload;
            return;
        }
        match ControlOp::from_byte(opcode_byte) {
            Some(ControlOp::Probe) => match parse_probe(&payload) {
                Some((probe, value)) => {
                    self.records.push(Msg::Observe(Observation {
                        probe,
                        node: 0,
                        value: Value::Bytes(value),
                        step,
                    }));
                    self.last_result = OpcodeResult::Ok;
                }
                None => self.last_result = OpcodeResult::MalformedPayload,
            },
            Some(ControlOp::MarkBranch) => {
                self.records.push(Msg::MarkBranch { step });
                self.last_result = OpcodeResult::Ok;
            }
            Some(ControlOp::Goal) => match String::from_utf8(payload) {
                Ok(metric) => {
                    self.records.push(Msg::Outcome(Outcome::GoalReached { metric }));
                    self.last_result = OpcodeResult::Ok;
                }
                Err(_) => self.last_result = OpcodeResult::MalformedPayload,
            },
            Some(ControlOp::Violation) => match String::from_utf8(payload) {
                Ok(invariant) => {
                    self.records.push(Msg::Outcome(Outcome::Crash {
                        node: None,
                        invariant: Some(invariant),
                        signal: None,
                        detail: "tape device VIOLATION".to_string(),
                    }));
                    self.last_result = OpcodeResult::Ok;
                }
                Err(_) => self.last_result = OpcodeResult::MalformedPayload,
            },
            Some(ControlOp::Log) => {
                self.records.push(Msg::Log { bytes: payload, step });
                self.last_result = OpcodeResult::Ok;
            }
            Some(ControlOp::Frame) => match parse_frame(&payload) {
                Some((width, height, format, bytes)) => {
                    let hash = baud_proto::Hash(*blake3::hash(&bytes).as_bytes());
                    self.records.push(Msg::Frame(FrameRecord {
                        node: 0,
                        step,
                        width,
                        height,
                        format,
                        hash,
                        bytes: Some(bytes),
                    }));
                    self.last_result = OpcodeResult::Ok;
                }
                None => self.last_result = OpcodeResult::MalformedPayload,
            },
            None => self.last_result = OpcodeResult::UnknownOpcode,
        }
    }
}

/// `PROBE`'s outbound payload format (this crate's own wire choice — specs/baud-tape-device.md
/// §4 leaves the byte layout to the implementation, only naming the opcode's meaning): the first
/// byte is the UTF-8 key's length, followed by that many key bytes, followed by the (opaque)
/// value bytes. Returns `None` if the declared key length exceeds the buffered payload or the key
/// bytes are not valid UTF-8 — both are `OpcodeResult::MalformedPayload`, not a panic or a
/// best-effort guess (matches this codebase's "fail loud, never silently continue" rule).
fn parse_probe(payload: &[u8]) -> Option<(String, Vec<u8>)> {
    let &key_len = payload.first()?;
    let key_len = key_len as usize;
    if payload.len() < 1 + key_len {
        return None;
    }
    let key = std::str::from_utf8(&payload[1..1 + key_len]).ok()?.to_string();
    let value = payload[1 + key_len..].to_vec();
    Some((key, value))
}

/// `FRAME`'s outbound payload format (this crate's own wire choice, same latitude
/// specs/baud-tape-device.md §4 leaves `PROBE` — see [`parse_probe`]): byte 0 is the pixel format
/// tag (`0` = Rgba8888, `1` = Rgb565, `2` = Indexed8), bytes 1..5 are the little-endian `u32`
/// width, bytes 5..9 the little-endian `u32` height, and everything after that is the raw pixel
/// buffer, verbatim. Geometry (buffer length vs. `width * height * bytes-per-pixel`) is
/// deliberately *not* validated here — that is `baud-stream::fingerprint`'s job
/// (`bad_geometry_is_a_crash`); this transport only rejects payloads too short to carry a header
/// or tagged with an unrecognized format byte, both `OpcodeResult::MalformedPayload`.
fn parse_frame(payload: &[u8]) -> Option<(u32, u32, PixFmt, Vec<u8>)> {
    if payload.len() < 9 {
        return None;
    }
    let format = match payload[0] {
        0 => PixFmt::Rgba8888,
        1 => PixFmt::Rgb565,
        2 => PixFmt::Indexed8,
        _ => return None,
    };
    let width = u32::from_le_bytes(payload[1..5].try_into().ok()?);
    let height = u32::from_le_bytes(payload[5..9].try_into().ok()?);
    let bytes = payload[9..].to_vec();
    Some((width, height, format, bytes))
}

#[cfg(test)]
mod tests;
