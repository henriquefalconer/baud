// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use super::*;
use baud_proto::{encode, Msg, Outcome, Value};

// ---------------------------------------------------------------------------
// Register-level unit tests
// ---------------------------------------------------------------------------

#[test]
fn data_reads_return_tape_bytes_in_order_and_advance_the_cursor() {
    let mut dev = TapeDevice::new(vec![1, 2, 3]);
    assert_eq!(dev.cursor(), 0);
    assert_eq!(dev.pio_read(reg::DATA), 1);
    assert_eq!(dev.cursor(), 1);
    assert_eq!(dev.pio_read(reg::DATA), 2);
    assert_eq!(dev.pio_read(reg::DATA), 3);
    assert_eq!(dev.cursor(), 3);
}

#[test]
fn restore_cursor_fast_forwards_so_the_next_read_continues_from_the_captured_position() {
    let mut dev = TapeDevice::new(vec![1, 2, 3, 4, 5]);
    dev.pio_read(reg::DATA); // cursor -> 1, as if a straight run had consumed one byte
    let captured_cursor = dev.cursor();
    assert_eq!(captured_cursor, 1);

    // A restored device starts fresh (cursor 0) over the same tape, then fast-forwards.
    let mut restored = TapeDevice::new(vec![1, 2, 3, 4, 5]);
    restored.restore_cursor(captured_cursor);
    assert_eq!(restored.cursor(), captured_cursor);

    // From here both devices must read the identical remaining sequence.
    let mut original_rest = Vec::new();
    let mut restored_rest = Vec::new();
    for _ in 0..4 {
        original_rest.push(dev.pio_read(reg::DATA));
        restored_rest.push(restored.pio_read(reg::DATA));
    }
    assert_eq!(original_rest, restored_rest);
    assert_eq!(original_rest, vec![2, 3, 4, 5]);
}

#[test]
fn read_past_end_of_tape_returns_the_fixed_sentinel_never_host_memory() {
    let mut dev = TapeDevice::new(vec![9]);
    assert_eq!(dev.pio_read(reg::DATA), 9);
    assert!(!dev.hit_eot_sentinel());
    // Every further read is the same fixed sentinel — never garbage, never host memory.
    for _ in 0..8 {
        assert_eq!(dev.pio_read(reg::DATA), EOT_SENTINEL);
    }
    assert!(dev.hit_eot_sentinel());
    // The cursor does not run away past the tape's actual length.
    assert_eq!(dev.cursor(), 1);
}

#[test]
fn mark_branch_opcode_with_no_payload_queues_a_mark_branch_record() {
    let mut dev = TapeDevice::new(vec![]);
    dev.pio_write(reg::CONTROL, ControlOp::MarkBranch as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::Ok);
    let records = dev.drain_records();
    assert_eq!(records.len(), 1);
    assert!(matches!(records[0], Msg::MarkBranch { step: 0 }));
}

#[test]
fn probe_opcode_decodes_key_and_value_from_the_outbound_buffer() {
    let mut dev = TapeDevice::new(vec![]);
    let key = b"depth";
    dev.pio_write(reg::DATA, key.len() as u8);
    for &b in key {
        dev.pio_write(reg::DATA, b);
    }
    for &b in b"\x2a\x00\x00\x00" {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Probe as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::Ok);
    let records = dev.drain_records();
    assert_eq!(records.len(), 1);
    match &records[0] {
        Msg::Observe(obs) => {
            assert_eq!(obs.probe, "depth");
            assert_eq!(obs.node, 0);
            assert_eq!(obs.value, Value::Bytes(vec![0x2a, 0x00, 0x00, 0x00]));
        }
        other => panic!("expected Observe, got {other:?}"),
    }
}

#[test]
fn probe_opcode_with_declared_key_length_longer_than_buffer_is_malformed() {
    let mut dev = TapeDevice::new(vec![]);
    dev.pio_write(reg::DATA, 5); // claims a 5-byte key
    dev.pio_write(reg::DATA, b'x'); // only 1 byte actually buffered
    dev.pio_write(reg::CONTROL, ControlOp::Probe as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::MalformedPayload);
    assert!(dev.drain_records().is_empty());
}

#[test]
fn probe_opcode_with_empty_buffer_has_no_key_length_byte_and_is_malformed() {
    let mut dev = TapeDevice::new(vec![]);
    dev.pio_write(reg::CONTROL, ControlOp::Probe as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::MalformedPayload);
}

#[test]
fn goal_opcode_emits_goal_reached_with_the_buffered_utf8_metric_name() {
    let mut dev = TapeDevice::new(vec![]);
    for &b in b"latency_ok" {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Goal as u8);
    let records = dev.drain_records();
    assert_eq!(records.len(), 1);
    match &records[0] {
        Msg::Outcome(Outcome::GoalReached { metric }) => assert_eq!(metric, "latency_ok"),
        other => panic!("expected GoalReached, got {other:?}"),
    }
}

#[test]
fn violation_opcode_emits_crash_with_the_buffered_utf8_invariant_name() {
    let mut dev = TapeDevice::new(vec![]);
    for &b in b"no_double_free" {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Violation as u8);
    let records = dev.drain_records();
    assert_eq!(records.len(), 1);
    match &records[0] {
        Msg::Outcome(Outcome::Crash { invariant, node, signal, .. }) => {
            assert_eq!(invariant.as_deref(), Some("no_double_free"));
            assert_eq!(*node, None);
            assert_eq!(*signal, None);
        }
        other => panic!("expected Crash, got {other:?}"),
    }
}

#[test]
fn goal_and_violation_opcodes_with_non_utf8_payloads_are_malformed() {
    let mut dev = TapeDevice::new(vec![]);
    dev.pio_write(reg::DATA, 0xff); // not valid UTF-8 on its own
    dev.pio_write(reg::CONTROL, ControlOp::Goal as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::MalformedPayload);

    dev.pio_write(reg::DATA, 0xff);
    dev.pio_write(reg::CONTROL, ControlOp::Violation as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::MalformedPayload);
    assert!(dev.drain_records().is_empty());
}

#[test]
fn log_opcode_carries_arbitrary_non_utf8_bytes_through_unmodified() {
    let mut dev = TapeDevice::new(vec![]);
    let raw = [0x00, 0xff, 0x10, 0xfe];
    for &b in &raw {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Log as u8);
    let records = dev.drain_records();
    match &records[0] {
        Msg::Log { bytes, .. } => assert_eq!(bytes, &raw),
        other => panic!("expected Log, got {other:?}"),
    }
}

#[test]
fn frame_opcode_decodes_format_dimensions_and_pixels_and_hashes_them() {
    let mut dev = TapeDevice::new(vec![]);
    // format=Indexed8 (2), width=2, height=2, 4 pixel bytes
    let payload: Vec<u8> = [2u8]
        .into_iter()
        .chain(2u32.to_le_bytes())
        .chain(2u32.to_le_bytes())
        .chain([10u8, 20, 30, 40])
        .collect();
    for b in payload {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Frame as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::Ok);
    let records = dev.drain_records();
    assert_eq!(records.len(), 1);
    match &records[0] {
        Msg::Frame(rec) => {
            assert_eq!(rec.width, 2);
            assert_eq!(rec.height, 2);
            assert_eq!(rec.format, baud_proto::PixFmt::Indexed8);
            assert_eq!(rec.bytes.as_deref(), Some([10u8, 20, 30, 40].as_slice()));
            assert_eq!(rec.hash, baud_proto::Hash(*blake3::hash(&[10, 20, 30, 40]).as_bytes()));
        }
        other => panic!("expected Frame, got {other:?}"),
    }
}

#[test]
fn frame_opcode_two_identical_payloads_hash_identically() {
    let payload = |buf: &mut TapeDevice| {
        let bytes: Vec<u8> = [0u8]
            .into_iter()
            .chain(1u32.to_le_bytes())
            .chain(1u32.to_le_bytes())
            .chain([1u8, 2, 3, 4])
            .collect();
        for b in bytes {
            buf.pio_write(reg::DATA, b);
        }
        buf.pio_write(reg::CONTROL, ControlOp::Frame as u8);
    };
    let mut dev1 = TapeDevice::new(vec![]);
    payload(&mut dev1);
    let mut dev2 = TapeDevice::new(vec![]);
    payload(&mut dev2);
    let hash_of = |records: Vec<Msg>| match records.into_iter().next() {
        Some(Msg::Frame(rec)) => rec.hash,
        other => panic!("expected Frame, got {other:?}"),
    };
    assert_eq!(
        hash_of(dev1.drain_records()),
        hash_of(dev2.drain_records()),
        "identical frame bytes must hash identically across two devices"
    );
}

#[test]
fn frame_opcode_with_unknown_format_byte_is_malformed() {
    let mut dev = TapeDevice::new(vec![]);
    let payload: Vec<u8> = [0xffu8].into_iter().chain(0u32.to_le_bytes()).chain(0u32.to_le_bytes()).collect();
    for b in payload {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Frame as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::MalformedPayload);
    assert!(dev.drain_records().is_empty());
}

#[test]
fn frame_opcode_with_header_shorter_than_nine_bytes_is_malformed() {
    let mut dev = TapeDevice::new(vec![]);
    for b in [0u8, 1, 2, 3] {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Frame as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::MalformedPayload);
    assert!(dev.drain_records().is_empty());
}

#[test]
fn frame_opcode_with_zero_pixel_bytes_is_still_well_formed() {
    // Geometry validation is baud-stream's job (bad_geometry_is_a_crash), not this transport's —
    // a header with no trailing pixel bytes still finalizes as a well-formed (if empty) Frame.
    let mut dev = TapeDevice::new(vec![]);
    let payload: Vec<u8> = [1u8].into_iter().chain(3u32.to_le_bytes()).chain(3u32.to_le_bytes()).collect();
    for b in payload {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Frame as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::Ok);
    match &dev.drain_records()[0] {
        Msg::Frame(rec) => assert!(rec.bytes.as_ref().unwrap().is_empty()),
        other => panic!("expected Frame, got {other:?}"),
    }
}

#[test]
fn oversized_record_is_rejected_without_queueing() {
    let mut dev = TapeDevice::new(Vec::new());
    for _ in 0..(16 * 1024 * 1024 + 1) {
        dev.pio_write(reg::DATA, 0x41);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Log as u8);
    assert_eq!(dev.last_opcode_result(), OpcodeResult::OversizedPayload);
    assert!(dev.drain_records().is_empty());
    assert_ne!(dev.pio_read(reg::STATUS) & 0x80, 0);
}

#[test]
fn unknown_opcode_byte_is_reported_and_queues_nothing() {
    let mut dev = TapeDevice::new(vec![]);
    dev.pio_write(reg::DATA, 1);
    dev.pio_write(reg::CONTROL, 0xaa); // not a ControlOp discriminant
    assert_eq!(dev.last_opcode_result(), OpcodeResult::UnknownOpcode);
    assert!(dev.drain_records().is_empty());
}

#[test]
fn outbound_buffer_is_cleared_after_each_finalized_record() {
    let mut dev = TapeDevice::new(vec![]);
    for &b in b"first" {
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Goal as u8);
    // A second record with no further writes must not see leftover bytes from the first.
    dev.pio_write(reg::CONTROL, ControlOp::Goal as u8);
    let records = dev.drain_records();
    assert_eq!(records.len(), 2);
    match (&records[0], &records[1]) {
        (
            Msg::Outcome(Outcome::GoalReached { metric: m0 }),
            Msg::Outcome(Outcome::GoalReached { metric: m1 }),
        ) => {
            assert_eq!(m0, "first");
            assert_eq!(m1, "");
        }
        other => panic!("unexpected records: {other:?}"),
    }
}

#[test]
fn drain_records_empties_the_queue() {
    let mut dev = TapeDevice::new(vec![]);
    dev.pio_write(reg::CONTROL, ControlOp::MarkBranch as u8);
    assert_eq!(dev.drain_records().len(), 1);
    assert!(dev.drain_records().is_empty());
}

#[test]
fn status_byte_reports_bytes_remaining_and_the_last_opcode_error_bit() {
    let mut dev = TapeDevice::new(vec![1, 2, 3]);
    assert_eq!(dev.pio_read(reg::STATUS), 3); // 3 remaining, no error yet
    dev.pio_read(reg::DATA);
    assert_eq!(dev.pio_read(reg::STATUS), 2);
    dev.pio_write(reg::CONTROL, 0xaa); // unknown opcode -> error bit
    assert_eq!(dev.pio_read(reg::STATUS) & 0x80, 0x80);
    dev.pio_write(reg::CONTROL, ControlOp::MarkBranch as u8); // Ok -> error bit clears
    assert_eq!(dev.pio_read(reg::STATUS) & 0x80, 0);
}

#[test]
fn writes_to_the_control_register_never_go_into_the_outbound_buffer_of_the_next_record() {
    let mut dev = TapeDevice::new(vec![]);
    dev.pio_write(reg::CONTROL, ControlOp::MarkBranch as u8);
    dev.pio_write(reg::CONTROL, ControlOp::Goal as u8);
    let records = dev.drain_records();
    match &records[1] {
        Msg::Outcome(Outcome::GoalReached { metric }) => assert_eq!(metric, ""),
        other => panic!("expected empty-metric GoalReached, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// specs/baud-tape-device.md §6's named tests
// ---------------------------------------------------------------------------

/// A tiny scripted "guest": reads 4 bytes from the tape's DATA register, writes them straight
/// back out as a LOG record, then returns the CBOR-encoded bytes of every record drained — this
/// crate's stand-in for the spec's `guest_output()` (there is no real vCPU here, only the device
/// model; `baud-multiverse`'s `tape_bus.rs` is what a real guest talks to).
fn io_guest(tape: Vec<u8>) -> Vec<u8> {
    let mut dev = TapeDevice::new(tape);
    for _ in 0..4 {
        let b = dev.pio_read(reg::DATA);
        dev.pio_write(reg::DATA, b);
    }
    dev.pio_write(reg::CONTROL, ControlOp::Log as u8);
    dev.drain_records()
        .iter()
        .flat_map(|m| encode(m).expect("encode"))
        .collect()
}

fn flip_one_byte(tape: &[u8]) -> Vec<u8> {
    let mut out = tape.to_vec();
    assert!(!out.is_empty(), "flip_one_byte needs a non-empty tape");
    out[0] ^= 0xff;
    out
}

#[test]
fn all_input_is_tape_derived() {
    let tape = vec![10, 20, 30, 40, 50];
    let a = io_guest(tape.clone());
    let b = io_guest(tape.clone());
    assert_eq!(a, b, "same tape must produce byte-identical guest output");
    let c = io_guest(flip_one_byte(&tape));
    assert_ne!(a, c, "changing one tape byte must change the guest output");
}

/// A "guest" that just drains everything the tape has to offer, far past its actual length, and
/// reports whether it ever saw the EOT sentinel — the spec's `drain_guest()`/`short_tape()`.
fn drain_guest(tape: Vec<u8>, reads: usize) -> (Vec<u8>, bool) {
    let mut dev = TapeDevice::new(tape);
    let mut out = Vec::with_capacity(reads);
    for _ in 0..reads {
        out.push(dev.pio_read(reg::DATA));
    }
    (out, dev.hit_eot_sentinel())
}

#[test]
fn read_past_end_is_fixed() {
    let short_tape = vec![1, 2, 3];
    let (out_a, hit_a) = drain_guest(short_tape.clone(), 16);
    let (out_b, hit_b) = drain_guest(short_tape, 16);
    assert!(hit_a && hit_b, "reading well past the tape's length must hit the EOT sentinel");
    assert_eq!(out_a, out_b, "EOT behavior must itself be deterministic across a double-run");
    assert!(out_a[3..].iter().all(|&b| b == EOT_SENTINEL));
}

// ---------------------------------------------------------------------------
// Fuzz: the register interface must never panic on arbitrary guest input
// ---------------------------------------------------------------------------

proptest::proptest! {
    /// A guest is untrusted input from the VMM's point of view — arbitrary offsets, arbitrary
    /// bytes, arbitrary opcodes must never panic the device model (they may legitimately produce
    /// `OpcodeResult::MalformedPayload`/`UnknownOpcode`, but the run loop must stay up).
    #[test]
    fn arbitrary_register_traffic_never_panics(
        tape in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
        ops in proptest::collection::vec(
            (proptest::prelude::any::<u16>(), proptest::prelude::any::<bool>(), proptest::prelude::any::<u8>()),
            0..128,
        ),
    ) {
        let mut dev = TapeDevice::new(tape);
        for (off, is_read, byte) in ops {
            if is_read {
                let _ = dev.pio_read(off);
            } else {
                dev.pio_write(off, byte);
            }
        }
        let _ = dev.drain_records();
    }
}
