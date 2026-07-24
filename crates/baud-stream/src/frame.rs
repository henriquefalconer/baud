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
        let buf = vec![42u8; 4];
        proc.ingest(0, &buf, true).unwrap();
        let hash = proc.prev_hash.clone().unwrap();
        assert!(proc.is_duplicate(&hash));
    }
}
