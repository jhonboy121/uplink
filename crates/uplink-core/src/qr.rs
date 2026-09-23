//! QR codes for exchanging keys: our own key as a matrix to draw, and a decoder for camera
//! frames (luma only — QR is a black-and-white code, so colour is wasted work).

use crate::Error;

/// Modules of light around the code. Four is what the spec asks for and what a decoder looks for
/// while it hunts for the finder patterns; a code drawn without it reads only by luck.
pub const QUIET: usize = 4;

/// The mark sits in the middle of every code we draw, so a decoder has to be able to lose it.
/// Level H spends about 30% of the code on recovery, which is far more than [`Matrix::logo`]
/// covers — the slack is for the photograph, the screen and the angle.
const CORRECTION: qrcode::EcLevel = qrcode::EcLevel::H;

/// How much of the code's width the mark may cover. A fifth of the width is a twenty-fifth of
/// the area, which H absorbs without noticing.
const LOGO: f32 = 0.2;

/// A square of modules, row-major; `true` is dark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matrix {
    pub size: usize,
    pub modules: Vec<bool>,
}

impl Matrix {
    /// Outside the code is light — checking `x` matters: without it a column past the right edge
    /// wraps into the next row and the whole drawing shears.
    pub fn dark(&self, x: usize, y: usize) -> bool {
        x < self.size && self.modules.get(y * self.size + x).copied().unwrap_or_default()
    }

    /// The side in modules of everything that gets drawn, quiet zone included.
    pub const fn framed(&self) -> usize {
        self.size + QUIET * 2
    }

    /// The side in modules of the mark in the middle. Odd, like every code size, so the two share
    /// a centre module and the mark lands on the grid instead of straddling it.
    pub fn logo(&self) -> usize {
        #[expect(clippy::cast_precision_loss, reason = "a code is at most 177 modules across")]
        #[expect(clippy::cast_sign_loss, clippy::cast_possible_truncation, reason = "positive, small")]
        let modules = (self.size as f32 * LOGO).round() as usize;
        modules | 1
    }

    /// Draws the code as greyscale, at least `size` across. The scale is whole pixels per module,
    /// because a module split across a pixel boundary is a module a decoder has to guess at.
    pub fn render(&self, size: usize) -> (Vec<u8>, usize) {
        self.draw(size.div_ceil(self.framed()).max(1))
    }

    /// Draws the code as greyscale, `scale` pixels to the module, quiet zone included; returns the
    /// pixels and the side in pixels.
    pub fn draw(&self, scale: usize) -> (Vec<u8>, usize) {
        let side = self.framed() * scale;
        let mut luma = vec![u8::MAX; side * side];
        for y in 0..self.size {
            for x in 0..self.size {
                if !self.dark(x, y) {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = (x + QUIET) * scale + dx;
                        let py = (y + QUIET) * scale + dy;
                        if let Some(pixel) = luma.get_mut(py * side + px) {
                            *pixel = 0;
                        }
                    }
                }
            }
        }
        (luma, side)
    }
}

pub fn encode(text: &str) -> Result<Matrix, Error> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), CORRECTION)
        .map_err(|e| Error::Qr(e.to_string()))?;
    let size = code.width();
    let modules = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    Ok(Matrix { size, modules })
}

/// Encodes `text` and draws it in one go, for callers that want nothing else from the code.
pub fn render(text: &str, size: usize) -> Result<(Vec<u8>, usize), Error> {
    Ok(encode(text)?.render(size))
}

/// Reads the first QR code in a greyscale frame. `stride` is the bytes per row, which camera
/// planes pad beyond `width`.
pub fn decode_luma(luma: &[u8], width: usize, height: usize, stride: usize) -> Option<String> {
    if width == 0 || height == 0 || stride < width || luma.len() < (height - 1) * stride + width {
        return None;
    }
    let mut image = rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| {
        luma.get(y * stride + x).copied().unwrap_or_default()
    });
    image.detect_grids().into_iter().find_map(|grid| grid.decode().ok().map(|(_, content)| content))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real key, which is what every code we draw carries.
    const KEY: &str = "0e62c81291e383bbc63b305338200d8562904d24a46cd6412f9bae0c7ae111f4";

    #[test]
    fn a_key_survives_the_round_trip() -> Result<(), Error> {
        let matrix = encode(KEY)?;
        assert!(matrix.size >= 21 && matrix.modules.len() == matrix.size * matrix.size);
        let (luma, side) = matrix.draw(4);
        assert_eq!(decode_luma(&luma, side, side, side).as_deref(), Some(KEY));
        Ok(())
    }

    /// The mark is painted over the middle of the code, so the code has to read without it. This
    /// is the whole reason the level is H; at the default it is a coin toss.
    #[test]
    fn the_mark_can_cover_the_middle() -> Result<(), Error> {
        let matrix = encode(KEY)?;
        let scale = 4;
        let (mut luma, side) = matrix.draw(scale);
        let logo = matrix.logo() * scale;
        let start = (side - logo) / 2;
        for y in start..start + logo {
            for x in start..start + logo {
                if let Some(pixel) = luma.get_mut(y * side + x) {
                    *pixel = u8::MAX;
                }
            }
        }
        assert_eq!(decode_luma(&luma, side, side, side).as_deref(), Some(KEY));
        Ok(())
    }

    /// Both are odd, so the mark sits on the grid rather than half a module off it.
    #[test]
    fn the_mark_shares_the_codes_centre() -> Result<(), Error> {
        let matrix = encode(KEY)?;
        assert_eq!(matrix.size % 2, 1);
        assert_eq!(matrix.logo() % 2, 1);
        assert!(matrix.logo() * 4 < matrix.size, "the mark is a fifth of the code, not a quarter");
        Ok(())
    }

    /// Camera planes are padded, so the decoder must read rows by stride, not by width.
    #[test]
    fn padded_rows_still_decode() -> Result<(), Error> {
        let key = "7d192bb40af655c2";
        let (luma, side) = encode(key)?.draw(4);
        let stride = side + 37;
        let mut padded = vec![u8::MAX; stride * side];
        for (y, row) in luma.chunks(side).enumerate() {
            if let Some(target) = padded.get_mut(y * stride..y * stride + side) {
                target.copy_from_slice(row);
            }
        }
        assert_eq!(decode_luma(&padded, side, side, stride).as_deref(), Some(key));
        Ok(())
    }

    /// What the app shows must be readable by another phone, quiet zone and all.
    #[test]
    fn the_rendered_code_reads_back() -> Result<(), Error> {
        let (luma, side) = render(KEY, 512)?;
        assert!(side >= 512, "asked for 512, got {side}");
        assert_eq!(luma.len(), side * side);
        // Corners are quiet zone: light in every direction, including right and bottom.
        assert_eq!(luma.first(), Some(&u8::MAX));
        assert_eq!(luma.get(side - 1), Some(&u8::MAX));
        assert_eq!(luma.get(side * side - 1), Some(&u8::MAX));
        assert_eq!(decode_luma(&luma, side, side, side).as_deref(), Some(KEY));
        Ok(())
    }

    #[test]
    fn a_blank_frame_decodes_to_nothing() {
        let blank = vec![u8::MAX; 640 * 480];
        assert_eq!(decode_luma(&blank, 640, 480, 640), None);
        assert_eq!(decode_luma(&blank, 64, 64, 4), None);
    }
}
