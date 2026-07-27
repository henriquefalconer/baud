// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Frame processor — validates, fingerprints and accumulates frames.

use crate::{fingerprint, to_rgba};
use baud_proto::{FrameRecord, Hash, PixFmt};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame size mismatch: expected {expected} bytes, got {got}")]
    SizeMismatch { expected: usize, got: usize },
}

/// A processed frame ready for journaling or encoding.
#[derive(Debug, Clone)]
pub struct ProcessedFrame {
    pub record: FrameRecord,
    /// RGBA8888 pixels (always present after processing)
    pub rgba: Vec<u8>,
}

/// Stateful processor that accumulates frames for a single node.
pub struct FrameProcessor {
    pub node: u16,
    pub width: u32,
    pub height: u32,
    pub format: PixFmt,
    pub frames: Vec<ProcessedFrame>,
    /// Hash of the previous frame (for deduplication bookkeeping)
    pub prev_hash: Option<Hash>,
}

impl FrameProcessor {
    pub fn new(node: u16, width: u32, height: u32, format: PixFmt) -> Self {
        FrameProcessor {
            node,
            width,
            height,
            format,
            frames: Vec::new(),
            prev_hash: None,
        }
    }

    /// Ingest a raw frame buffer at the given virtual step.
    /// Returns the processed frame (or error if size mismatch).
    pub fn ingest(&mut self, step: u64, buf: &[u8], hash_only: bool) -> Result<&ProcessedFrame, FrameError> {
        let hash = fingerprint(buf, self.width, self.height, &self.format)?;
        let rgba = to_rgba(buf, &self.format);
        let record = FrameRecord {
            node: self.node,
            step,
            width: self.width,
            height: self.height,
            format: self.format.clone(),
            hash: hash.clone(),
            bytes: if hash_only { None } else { Some(buf.to_vec()) },
        };
        self.prev_hash = Some(hash);
        self.frames.push(ProcessedFrame { record, rgba });
        Ok(self.frames.last().unwrap())
    }

    /// List all frame hashes in order.
    pub fn frame_hashes(&self) -> Vec<(u64, Hash)> {
        self.frames.iter().map(|f| (f.record.step, f.record.hash.clone())).collect()
    }

    /// Check whether this is a duplicate of the previous frame.
    pub fn is_duplicate(&self, hash: &Hash) -> bool {
        self.prev_hash.as_ref() == Some(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_basic() {
        let mut proc = FrameProcessor::new(0, 4, 4, PixFmt::Indexed8);
        let buf = vec![128u8; 16];
        let frame = proc.ingest(0, &buf, false).unwrap();
        assert_eq!(frame.record.width, 4);
        assert_eq!(frame.rgba.len(), 64); // 4x4 RGBA
    }

    #[test]
    fn ingest_wrong_size_errors() {
        let mut proc = FrameProcessor::new(0, 4, 4, PixFmt::Indexed8);
        let buf = vec![0u8; 10]; // wrong size
        assert!(proc.ingest(0, &buf, false).is_err());
    }

    #[test]
    fn duplicate_detection() {
        let mut proc = FrameProcessor::new(0, 2, 2, PixFmt::Indexed8);
        let frame_a = vec![42u8; 4];
        let frame_b = vec![7u8; 4]; // different pixels → different fingerprint

        let hash_a = proc.ingest(0, &frame_a, true).unwrap().record.hash.clone();

        // Positive case: re-presenting the frame just ingested is a duplicate.
        assert!(
            proc.is_duplicate(&hash_a),
            "the hash of the frame just ingested must be reported as a duplicate"
        );

        // Negative case: a *different* frame is not a duplicate of the previous one.
        // (This is the half the old test never covered — `is_duplicate` returning
        // `true` unconditionally would have passed it.)
        let hash_b = fingerprint(&frame_b, proc.width, proc.height, &proc.format).unwrap();
        assert_ne!(hash_a, hash_b, "different pixel content must fingerprint differently");
        assert!(
            !proc.is_duplicate(&hash_b),
            "a frame with different content must not be reported as a duplicate"
        );

        // Ingesting the different frame moves `prev_hash` on: the new frame is now the
        // duplicate candidate and the old one no longer is.
        proc.ingest(1, &frame_b, true).unwrap();
        assert!(proc.is_duplicate(&hash_b), "prev_hash must track the most recent frame");
        assert!(
            !proc.is_duplicate(&hash_a),
            "the previous frame's hash must stop being a duplicate once a new frame is ingested"
        );

        // Ingesting identical content again yields the same hash — the duplicate the
        // dedup bookkeeping exists to spot.
        let repeat = proc.ingest(2, &frame_b, true).unwrap().record.hash.clone();
        assert_eq!(repeat, hash_b, "identical content must produce an identical fingerprint");
        assert!(proc.is_duplicate(&repeat));
    }

    #[test]
    fn is_duplicate_is_false_before_any_frame_is_ingested() {
        let proc = FrameProcessor::new(0, 2, 2, PixFmt::Indexed8);
        let hash = fingerprint(&[42u8; 4], 2, 2, &PixFmt::Indexed8).unwrap();
        assert!(
            !proc.is_duplicate(&hash),
            "with no previous frame nothing can be a duplicate"
        );
    }
}
