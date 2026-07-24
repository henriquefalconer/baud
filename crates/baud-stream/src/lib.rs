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

pub use frame::{FrameProcessor, FrameError, ProcessedFrame};
pub use qoi::encode_qoi;
pub use y4m::Y4mWriter;

use baud_proto::{FrameRecord, PixFmt};

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
    let expected = expected_size(width, height, format);
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
    let pixels = (width as usize) * (height as usize);
    match format {
        PixFmt::Rgba8888 => pixels * 4,
        PixFmt::Rgb565   => pixels * 2,
        PixFmt::Indexed8 => pixels,
    }
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
}
