// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// QOI (Quite OK Image) encoder — ~300 lines, zero external deps.
//
// Spec: https://qoiformat.org/qoi-specification.pdf
// Encodes RGBA8888 pixel data to QOI format.
//
// Chunks:
//   QOI_OP_RGB    = 0b11111110 (0xFE) r g b
//   QOI_OP_RGBA   = 0b11111111 (0xFF) r g b a
//   QOI_OP_INDEX  = 0b00xxxxxx (index into 64-entry seen array)
//   QOI_OP_DIFF   = 0b01xxxxxx (dr-2, dg-2, db-2 in 2-bit each)
//   QOI_OP_LUMA   = 0b10xxxxxx dg-32 (dr-dg-8, db-dg-8 in 4-bit each)
//   QOI_OP_RUN    = 0b11xxxxxx (run length - 1 in 6 bits)
//
// End marker: 7 zero bytes + one 0x01 byte.

const QOI_MAGIC: &[u8] = b"qoif";
const QOI_HEADER_SIZE: usize = 14;
const QOI_END_MARK: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 1];

const QOI_OP_INDEX: u8 = 0x00;
const QOI_OP_DIFF:  u8 = 0x40;
const QOI_OP_LUMA:  u8 = 0x80;
const QOI_OP_RUN:   u8 = 0xC0;
const QOI_OP_RGB:   u8 = 0xFE;
const QOI_OP_RGBA:  u8 = 0xFF;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Pixel { r: u8, g: u8, b: u8, a: u8 }

impl Pixel {
    fn from_rgba(rgba: &[u8]) -> Self {
        Pixel { r: rgba[0], g: rgba[1], b: rgba[2], a: rgba[3] }
    }

    fn hash_index(self) -> usize {
        let v = (self.r as usize * 3)
            .wrapping_add(self.g as usize * 5)
            .wrapping_add(self.b as usize * 7)
            .wrapping_add(self.a as usize * 11);
        v % 64
    }
}

/// Encode RGBA8888 pixel data to QOI format.
///
/// # Errors
/// Returns an error string if `pixels.len() != width * height * 4`.
pub fn encode_qoi(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let expected = (width as usize) * (height as usize) * 4;
    if pixels.len() != expected {
        return Err(format!("QOI: expected {expected} bytes, got {}", pixels.len()));
    }

    let mut out = Vec::with_capacity(QOI_HEADER_SIZE + pixels.len() + QOI_END_MARK.len());

    // Header
    out.extend_from_slice(QOI_MAGIC);
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.push(4); // channels: 4 = RGBA
    out.push(0); // colorspace: 0 = sRGB with linear alpha

    let mut seen = [Pixel::default(); 64];
    let mut prev = Pixel { r: 0, g: 0, b: 0, a: 255 };
    let mut run: u8 = 0;

    let total = (width as usize) * (height as usize);
    for i in 0..total {
        let px = Pixel::from_rgba(&pixels[i * 4..]);

        if px == prev {
            run += 1;
            if run == 62 {
                out.push(QOI_OP_RUN | (run - 1));
                run = 0;
            }
        } else {
            if run > 0 {
                out.push(QOI_OP_RUN | (run - 1));
                run = 0;
            }

            let idx = px.hash_index();
            if seen[idx] == px {
                out.push(QOI_OP_INDEX | idx as u8);
            } else {
                seen[idx] = px;

                if px.a != prev.a {
                    out.push(QOI_OP_RGBA);
                    out.push(px.r);
                    out.push(px.g);
                    out.push(px.b);
                    out.push(px.a);
                } else {
                    let dr = (px.r as i16) - (prev.r as i16);
                    let dg = (px.g as i16) - (prev.g as i16);
                    let db = (px.b as i16) - (prev.b as i16);

                    if dr >= -2 && dr <= 1 && dg >= -2 && dg <= 1 && db >= -2 && db <= 1 {
                        out.push(QOI_OP_DIFF
                            | (((dr + 2) as u8) << 4)
                            | (((dg + 2) as u8) << 2)
                            | ((db + 2) as u8));
                    } else {
                        let dr_dg = dr - dg;
                        let db_dg = db - dg;
                        if dg >= -32 && dg <= 31 && dr_dg >= -8 && dr_dg <= 7 && db_dg >= -8 && db_dg <= 7 {
                            out.push(QOI_OP_LUMA | ((dg + 32) as u8));
                            out.push(((dr_dg + 8) as u8) << 4 | ((db_dg + 8) as u8));
                        } else {
                            out.push(QOI_OP_RGB);
                            out.push(px.r);
                            out.push(px.g);
                            out.push(px.b);
                        }
                    }
                }
            }
        }
        prev = px;
    }

    if run > 0 {
        out.push(QOI_OP_RUN | (run - 1));
    }

    out.extend_from_slice(QOI_END_MARK);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_single_pixel() {
        let px = [255u8, 0, 0, 255]; // red
        let out = encode_qoi(&px, 1, 1).unwrap();
        assert!(out.starts_with(b"qoif"));
        assert!(out.ends_with(QOI_END_MARK));
    }

    #[test]
    fn encode_solid_color() {
        // 4x4 solid white RGBA
        let px = vec![255u8; 64];
        let out = encode_qoi(&px, 4, 4).unwrap();
        assert!(out.starts_with(b"qoif"));
        // Should use RUN encoding heavily — much smaller than raw
        assert!(out.len() < px.len() + QOI_HEADER_SIZE + QOI_END_MARK.len());
    }

    #[test]
    fn encode_wrong_size_errors() {
        let px = vec![0u8; 10]; // too small for 4x4
        assert!(encode_qoi(&px, 4, 4).is_err());
    }

    #[test]
    fn encode_gradient() {
        // 8x8 gradient
        let mut px = Vec::with_capacity(256);
        for y in 0u8..8 {
            for x in 0u8..8 {
                px.push(x * 32);
                px.push(y * 32);
                px.push(128);
                px.push(255);
            }
        }
        let out = encode_qoi(&px, 8, 8).unwrap();
        assert!(out.starts_with(b"qoif"));
        assert!(out.ends_with(QOI_END_MARK));
    }
}
