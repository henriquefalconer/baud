// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Y4M (YUV4MPEG2) writer — raw, pipeable format.
//
// Format overview:
//   STREAM_HEADER\n
//   "YUV4MPEG2 W{w} H{h} F{fps_num}:{fps_den} Ip A0:0 C420mpeg2\n"
//   For each frame:
//     "FRAME\n"
//     <Y plane: w*h bytes>
//     <Cb plane: (w/2)*(h/2) bytes>
//     <Cr plane: (w/2)*(h/2) bytes>
//
// We write C420 (4:2:0 chroma subsampling) which is what most players expect.
// RGBA → YCbCr conversion uses BT.601 coefficients.

use std::io::{self, Write};

/// Y4M writer — write to any `Write` sink.
pub struct Y4mWriter<W: Write> {
    sink: W,
    width: u32,
    height: u32,
    frame_count: u64,
}

impl<W: Write> Y4mWriter<W> {
    /// Create a new Y4M writer and write the stream header.
    ///
    /// `fps_num / fps_den` is the frame rate (e.g. 60/1 or 30/1).
    /// For deterministic replay we default to 30/1.
    pub fn new(mut sink: W, width: u32, height: u32, fps_num: u32, fps_den: u32) -> io::Result<Self> {
        // Y4M requires even dimensions for 4:2:0
        let w = width & !1;
        let h = height & !1;
        write!(sink, "YUV4MPEG2 W{w} H{h} F{fps_num}:{fps_den} Ip A0:0 C420mpeg2\n")?;
        Ok(Y4mWriter { sink, width: w, height: h, frame_count: 0 })
    }

    /// Write a single frame from RGBA8888 pixel data.
    ///
    /// `rgba` must be `width * height * 4` bytes.
    pub fn write_frame(&mut self, rgba: &[u8]) -> io::Result<()> {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if rgba.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Y4M: expected {expected} RGBA bytes, got {}", rgba.len()),
            ));
        }

        self.sink.write_all(b"FRAME\n")?;

        let w = self.width as usize;
        let h = self.height as usize;

        // Y plane (luma) — one value per pixel
        let mut y_plane = Vec::with_capacity(w * h);
        for i in 0..w * h {
            let (r, g, b) = rgb_at(rgba, i);
            y_plane.push(rgb_to_y(r, g, b));
        }
        self.sink.write_all(&y_plane)?;

        // Cb, Cr planes (4:2:0 — average 2x2 blocks)
        let cw = w / 2;
        let ch = h / 2;
        let mut cb_plane = Vec::with_capacity(cw * ch);
        let mut cr_plane = Vec::with_capacity(cw * ch);
        for block_y in 0..ch {
            for block_x in 0..cw {
                let idx00 = (block_y * 2) * w + (block_x * 2);
                let idx10 = (block_y * 2 + 1) * w + (block_x * 2);
                let idx01 = (block_y * 2) * w + (block_x * 2 + 1);
                let idx11 = (block_y * 2 + 1) * w + (block_x * 2 + 1);

                let (r00, g00, b00) = rgb_at(rgba, idx00);
                let (r10, g10, b10) = rgb_at(rgba, idx10);
                let (r01, g01, b01) = rgb_at(rgba, idx01);
                let (r11, g11, b11) = rgb_at(rgba, idx11);

                let cb00 = rgb_to_cb(r00, g00, b00) as u16;
                let cb10 = rgb_to_cb(r10, g10, b10) as u16;
                let cb01 = rgb_to_cb(r01, g01, b01) as u16;
                let cb11 = rgb_to_cb(r11, g11, b11) as u16;
                cb_plane.push(((cb00 + cb10 + cb01 + cb11 + 2) / 4) as u8);

                let cr00 = rgb_to_cr(r00, g00, b00) as u16;
                let cr10 = rgb_to_cr(r10, g10, b10) as u16;
                let cr01 = rgb_to_cr(r01, g01, b01) as u16;
                let cr11 = rgb_to_cr(r11, g11, b11) as u16;
                cr_plane.push(((cr00 + cr10 + cr01 + cr11 + 2) / 4) as u8);
            }
        }
        self.sink.write_all(&cb_plane)?;
        self.sink.write_all(&cr_plane)?;

        self.frame_count += 1;
        Ok(())
    }

    /// Flush and return the number of frames written.
    pub fn finish(mut self) -> io::Result<(W, u64)> {
        self.sink.flush()?;
        Ok((self.sink, self.frame_count))
    }
}

// ---------------------------------------------------------------------------
// BT.601 YCbCr conversion helpers (full swing: Y ∈ [16,235], CbCr ∈ [16,240])
// ---------------------------------------------------------------------------

fn rgb_at(rgba: &[u8], pixel_idx: usize) -> (u8, u8, u8) {
    let base = pixel_idx * 4;
    (rgba[base], rgba[base + 1], rgba[base + 2])
}

fn rgb_to_y(r: u8, g: u8, b: u8) -> u8 {
    let r = r as i32;
    let g = g as i32;
    let b = b as i32;
    let y = (66 * r + 129 * g + 25 * b + 128) / 256 + 16;
    y.clamp(16, 235) as u8
}

fn rgb_to_cb(r: u8, g: u8, b: u8) -> u8 {
    let r = r as i32;
    let g = g as i32;
    let b = b as i32;
    let cb = (-38 * r - 74 * g + 112 * b + 128) / 256 + 128;
    cb.clamp(16, 240) as u8
}

fn rgb_to_cr(r: u8, g: u8, b: u8) -> u8 {
    let r = r as i32;
    let g = g as i32;
    let b = b as i32;
    let cr = (112 * r - 94 * g - 18 * b + 128) / 256 + 128;
    cr.clamp(16, 240) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_single_frame() {
        let mut buf = Vec::new();
        let rgba = vec![128u8; 8 * 8 * 4]; // 8x8 grey
        let mut writer = Y4mWriter::new(&mut buf, 8, 8, 30, 1).unwrap();
        writer.write_frame(&rgba).unwrap();
        let (_, n) = writer.finish().unwrap();
        assert_eq!(n, 1);
        // Check header
        let s = std::str::from_utf8(&buf[..30]).unwrap();
        assert!(s.starts_with("YUV4MPEG2 W8 H8"));
    }

    #[test]
    fn write_multiple_frames() {
        let mut buf = Vec::new();
        let rgba = vec![200u8; 4 * 4 * 4];
        let mut writer = Y4mWriter::new(&mut buf, 4, 4, 60, 1).unwrap();
        writer.write_frame(&rgba).unwrap();
        writer.write_frame(&rgba).unwrap();
        writer.write_frame(&rgba).unwrap();
        let (_, n) = writer.finish().unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn wrong_size_errors() {
        let mut buf = Vec::new();
        let rgba = vec![0u8; 10]; // too small
        let mut writer = Y4mWriter::new(&mut buf, 4, 4, 30, 1).unwrap();
        assert!(writer.write_frame(&rgba).is_err());
    }

    #[test]
    fn luma_conversion_white() {
        let y = rgb_to_y(255, 255, 255);
        assert!(y >= 230, "white Y should be near 235, got {y}");
    }

    #[test]
    fn luma_conversion_black() {
        let y = rgb_to_y(0, 0, 0);
        assert_eq!(y, 16);
    }
}
