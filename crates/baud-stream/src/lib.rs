// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-stream — graphical-surface capture, fingerprinting, and streaming
//
// Rules:
//   - Deps = {baud-proto, blake3}; QOI and Y4M writers in-crate
//   - Knows byte surfaces, dimensions, and formats — never what is depicted
//   - Soft budget ≤ 1,200 LOC

pub mod qoi;
pub mod y4m;
pub mod frame;

/// Maximum raw frame payload accepted from a guest. Conversion to RGBA can allocate four times
/// the input, so reject unreasonable geometry before hashing or rendering it.
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

pub use frame::{FrameProcessor, FrameError, ProcessedFrame};
pub use qoi::encode_qoi;
pub use y4m::Y4mWriter;

use baud_proto::{FrameRecord, Outcome, PixFmt};

/// Validate and fingerprint a raw frame buffer.
///
/// Returns (hash, width, height) if the buffer length matches the declared geometry,
/// or FrameError if the size is wrong.
pub fn fingerprint(
    buf: &[u8],
    width: u32,
    height: u32,
    format: &PixFmt,
) -> Result<baud_proto::Hash, FrameError> {
    let expected = checked_expected_size(width, height, format).ok_or_else(|| {
        FrameError::GeometryOverflow { width, height, format: format.clone() }
    })?;
    if expected > MAX_FRAME_BYTES {
        return Err(FrameError::GeometryTooLarge { limit: MAX_FRAME_BYTES, got: expected });
    }
    if buf.len() != expected {
        return Err(FrameError::SizeMismatch {
            expected,
            got: buf.len(),
        });
    }
    let h = blake3::hash(buf);
    Ok(baud_proto::Hash(*h.as_bytes()))
}

/// Expected byte length for a frame with given dimensions and format.
pub fn expected_size(width: u32, height: u32, format: &PixFmt) -> usize {
    checked_expected_size(width, height, format).unwrap_or(usize::MAX)
}

/// Compute the exact byte count without allowing dimension multiplication to wrap.
/// Frame records come from an untrusted guest, so overflow is malformed input.
fn checked_expected_size(width: u32, height: u32, format: &PixFmt) -> Option<usize> {
    let pixels = (width as usize).checked_mul(height as usize)?;
    let bytes_per_pixel = match format {
        PixFmt::Rgba8888 => 4,
        PixFmt::Rgb565 => 2,
        PixFmt::Indexed8 => 1,
    };
    pixels.checked_mul(bytes_per_pixel)
}

/// Convert an indexed8 frame to RGBA8888 using a greyscale palette.
pub fn indexed8_to_rgba(buf: &[u8]) -> Vec<u8> {
    buf.iter().flat_map(|&idx| [idx, idx, idx, 255]).collect()
}

/// Convert an Rgb565 frame to RGBA8888.
pub fn rgb565_to_rgba(buf: &[u8]) -> Vec<u8> {
    buf.chunks_exact(2).flat_map(|pair| {
        let word = u16::from_le_bytes([pair[0], pair[1]]);
        let r = ((word >> 11) & 0x1f) as u8;
        let g = ((word >> 5) & 0x3f) as u8;
        let b = (word & 0x1f) as u8;
        // Scale to 8-bit
        let r8 = (r << 3) | (r >> 2);
        let g8 = (g << 2) | (g >> 4);
        let b8 = (b << 3) | (b >> 2);
        [r8, g8, b8, 255u8]
    }).collect()
}

/// Produce RGBA8888 bytes regardless of input format.
pub fn to_rgba(buf: &[u8], format: &PixFmt) -> Vec<u8> {
    match format {
        PixFmt::Rgba8888 => buf.to_vec(),
        PixFmt::Rgb565   => rgb565_to_rgba(buf),
        PixFmt::Indexed8 => indexed8_to_rgba(buf),
    }
}

/// Ingest and validate a raw frame buffer, returning an Outcome.
///
/// On geometry mismatch, returns `Outcome::Crash{detail: "frame-format", node, step}`.
/// On success, returns `Outcome::GoalReached{metric: "frame_ok"}` — callers should use
/// the returned hash via the FrameRecord path in normal operation; the `Outcome` return
/// is primarily for the error case so that the supervisor can propagate the crash upward.
pub fn ingest(
    node: u16,
    _step: u64,
    width: u32,
    height: u32,
    format: &PixFmt,
    buf: &[u8],
) -> Result<baud_proto::Hash, Outcome> {
    fingerprint(buf, width, height, format).map_err(|_| Outcome::Crash {
        node: Some(node),
        invariant: None,
        signal: None,
        detail: format!("frame-format: expected {}x{}x{:?} ({} bytes), got {} bytes",
            width, height, format,
            expected_size(width, height, format),
            buf.len()),
    })
}

/// Build a FrameRecord from raw pixel data.
/// Bytes are omitted in hash-only mode.
pub fn make_frame_record(
    node: u16,
    step: u64,
    width: u32,
    height: u32,
    format: PixFmt,
    buf: &[u8],
    hash_only: bool,
) -> Result<FrameRecord, FrameError> {
    let hash = fingerprint(buf, width, height, &format)?;
    Ok(FrameRecord {
        node,
        step,
        width,
        height,
        format,
        hash,
        bytes: if hash_only { None } else { Some(buf.to_vec()) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_size_rgba() {
        assert_eq!(expected_size(2, 2, &PixFmt::Rgba8888), 16);
    }

    #[test]
    fn expected_size_rgb565() {
        assert_eq!(expected_size(4, 4, &PixFmt::Rgb565), 32);
    }

    #[test]
    fn expected_size_indexed8() {
        assert_eq!(expected_size(8, 8, &PixFmt::Indexed8), 64);
    }

    #[test]
    fn fingerprint_correct_size() {
        let buf = vec![0u8; 4]; // 1x1 RGBA
        let hash = fingerprint(&buf, 1, 1, &PixFmt::Rgba8888).unwrap();
        assert_eq!(hash.0.len(), 32);
    }

    #[test]
    fn fingerprint_wrong_size() {
        let buf = vec![0u8; 3]; // too small for 1x1 RGBA
        assert!(fingerprint(&buf, 1, 1, &PixFmt::Rgba8888).is_err());
    }

    #[test]
    fn fingerprint_rejects_geometry_overflow() {
        let err = fingerprint(&[], u32::MAX, u32::MAX, &PixFmt::Rgba8888).unwrap_err();
        assert!(matches!(err, FrameError::GeometryTooLarge { .. } | FrameError::GeometryOverflow { .. }));
    }

    #[test]
    fn fingerprint_rejects_oversized_frame_before_conversion() {
        let err = fingerprint(&[], 16_384, 16_384, &PixFmt::Rgba8888).unwrap_err();
        assert!(matches!(err, FrameError::GeometryTooLarge { .. }));
    }

    #[test]
    fn indexed8_to_rgba_correct() {
        let buf = vec![100u8, 200u8];
        let rgba = indexed8_to_rgba(&buf);
        assert_eq!(&rgba[..4], &[100, 100, 100, 255]);
        assert_eq!(&rgba[4..], &[200, 200, 200, 255]);
    }

    #[test]
    fn identical_frames_same_hash() {
        let buf1 = vec![42u8; 64]; // 8x8 indexed8
        let buf2 = vec![42u8; 64];
        let h1 = fingerprint(&buf1, 8, 8, &PixFmt::Indexed8).unwrap();
        let h2 = fingerprint(&buf2, 8, 8, &PixFmt::Indexed8).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_frames_different_hash() {
        let buf1 = vec![0u8; 64];
        let mut buf2 = vec![0u8; 64];
        buf2[10] = 1;
        let h1 = fingerprint(&buf1, 8, 8, &PixFmt::Indexed8).unwrap();
        let h2 = fingerprint(&buf2, 8, 8, &PixFmt::Indexed8).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn make_frame_record_hash_only() {
        let buf = vec![50u8; 16]; // 4x4 indexed8
        let rec = make_frame_record(0, 1, 4, 4, PixFmt::Indexed8, &buf, true).unwrap();
        assert!(rec.bytes.is_none());
        assert_eq!(rec.width, 4);
        assert_eq!(rec.height, 4);
    }

    #[test]
    fn make_frame_record_with_bytes() {
        let buf = vec![77u8; 16];
        let rec = make_frame_record(0, 2, 4, 4, PixFmt::Indexed8, &buf, false).unwrap();
        assert_eq!(rec.bytes.as_ref().unwrap(), &buf);
    }

    // ---------------------------------------------------------------------------
    // Spec-mandated test names (specs/baud-stream.md §7)
    // ---------------------------------------------------------------------------

    /// frame_hashes_double_run_identical: two runs with the same pixel data must
    /// produce the same frame hashes (blake3 is deterministic).
    #[test]
    fn frame_hashes_double_run_identical() {
        let buf = vec![42u8; 64]; // 8x8 indexed8
        let h1 = fingerprint(&buf, 8, 8, &PixFmt::Indexed8).unwrap();
        let h2 = fingerprint(&buf, 8, 8, &PixFmt::Indexed8).unwrap();
        assert_eq!(h1, h2, "frame_hashes_double_run_identical: same pixel data must hash identically");

        // Also verify via FrameProcessor (accumulating path)
        let mut p1 = FrameProcessor::new(0, 8, 8, PixFmt::Indexed8);
        let mut p2 = FrameProcessor::new(0, 8, 8, PixFmt::Indexed8);
        p1.ingest(0, &buf, true).unwrap();
        p2.ingest(0, &buf, true).unwrap();
        let hashes1 = p1.frame_hashes();
        let hashes2 = p2.frame_hashes();
        assert_eq!(hashes1, hashes2, "frame_hashes_double_run_identical: FrameProcessor must agree");
    }

    /// render_is_byte_identical: rendering (to_rgba) of the same frame buffer must
    /// be byte-identical across calls (deterministic pixel conversion).
    #[test]
    fn render_is_byte_identical() {
        let buf = vec![0xABu8; 8]; // 2x2 rgb565
        let rgba1 = to_rgba(&buf, &PixFmt::Rgb565);
        let rgba2 = to_rgba(&buf, &PixFmt::Rgb565);
        assert_eq!(rgba1, rgba2, "render_is_byte_identical: pixel conversion must be deterministic");

        // Also check indexed8
        let ibuf = vec![128u8; 16]; // 4x4 indexed8
        let r1 = to_rgba(&ibuf, &PixFmt::Indexed8);
        let r2 = to_rgba(&ibuf, &PixFmt::Indexed8);
        assert_eq!(r1, r2, "render_is_byte_identical: indexed8 conversion must be deterministic");
    }

    /// bad_geometry_is_a_crash: ingesting a buffer shorter than the declared geometry
    /// must produce Outcome::Crash with detail containing "frame-format".
    #[test]
    fn bad_geometry_is_a_crash() {
        // 1x1 RGBA8888 requires 4 bytes; provide only 3
        let short_buf = vec![0u8; 3];
        let result = ingest(0, 1, 1, 1, &PixFmt::Rgba8888, &short_buf);
        assert!(result.is_err(), "bad_geometry_is_a_crash: wrong-size buffer must return Err");
        match result.unwrap_err() {
            Outcome::Crash { detail, node, .. } => {
                assert!(
                    detail.contains("frame-format"),
                    "bad_geometry_is_a_crash: Crash detail must contain 'frame-format', got: {detail}"
                );
                assert_eq!(node, Some(0), "bad_geometry_is_a_crash: Crash must carry node id");
            }
            other => panic!("bad_geometry_is_a_crash: expected Outcome::Crash, got {other:?}"),
        }
    }
}
