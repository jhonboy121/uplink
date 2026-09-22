//! QR codes for exchanging keys: our own key as a matrix to draw, and a decoder for camera
//! frames (luma only — QR is a black-and-white code, so colour is wasted work).

use crate::Error;

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
}

pub fn encode(text: &str) -> Result<Matrix, Error> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| Error::Qr(e.to_string()))?;
    let size = code.width();
    let modules = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    Ok(Matrix { size, modules })
}

/// Draws `text` as a greyscale image at least `size` across, quiet zone included; returns the
/// pixels and the side length. `qrcode`'s own renderer, so the drawing isn't ours to get wrong.
pub fn render(text: &str, size: u32) -> Result<(Vec<u8>, u32), Error> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| Error::Qr(e.to_string()))?;
    let image = code
        .render::<image::Luma<u8>>()
        .min_dimensions(size, size)
        .quiet_zone(true)
        .dark_color(image::Luma([0]))
        .light_color(image::Luma([u8::MAX]))
        .build();
    let side = image.width();
    Ok((image.into_raw(), side))
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

    /// Draws a matrix as a greyscale image, `scale` pixels per module, with the quiet zone a
    /// decoder needs.
    fn draw(matrix: &Matrix, scale: usize, quiet: usize) -> (Vec<u8>, usize) {
        let side = (matrix.size + quiet * 2) * scale;
        let mut luma = vec![u8::MAX; side * side];
        for y in 0..matrix.size {
            for x in 0..matrix.size {
                if !matrix.dark(x, y) {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = (x + quiet) * scale + dx;
                        let py = (y + quiet) * scale + dy;
                        if let Some(pixel) = luma.get_mut(py * side + px) {
                            *pixel = 0;
                        }
                    }
                }
            }
        }
        (luma, side)
    }

    #[test]
    fn a_key_survives_the_round_trip() -> Result<(), Error> {
        let key = "0e62c81291e383bbc63b305338200d8562904d24a46cd6412f9bae0c7ae111f4";
        let matrix = encode(key)?;
        assert!(matrix.size >= 21 && matrix.modules.len() == matrix.size * matrix.size);
        let (luma, side) = draw(&matrix, 4, 4);
        assert_eq!(decode_luma(&luma, side, side, side).as_deref(), Some(key));
        Ok(())
    }

    /// Camera planes are padded, so the decoder must read rows by stride, not by width.
    #[test]
    fn padded_rows_still_decode() -> Result<(), Error> {
        let key = "7d192bb40af655c2";
        let (luma, side) = draw(&encode(key)?, 4, 4);
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
        let key = "0e62c81291e383bbc63b305338200d8562904d24a46cd6412f9bae0c7ae111f4";
        let (luma, side) = render(key, 512)?;
        assert!(side >= 512, "asked for 512, got {side}");
        let side = usize::try_from(side).unwrap_or_default();
        assert_eq!(luma.len(), side * side);
        // Corners are quiet zone: light in every direction, including right and bottom.
        assert_eq!(luma.first(), Some(&u8::MAX));
        assert_eq!(luma.get(side - 1), Some(&u8::MAX));
        assert_eq!(luma.get(side * side - 1), Some(&u8::MAX));
        assert_eq!(decode_luma(&luma, side, side, side).as_deref(), Some(key));
        Ok(())
    }

    #[test]
    fn a_blank_frame_decodes_to_nothing() {
        let blank = vec![u8::MAX; 640 * 480];
        assert_eq!(decode_luma(&blank, 640, 480, 640), None);
        assert_eq!(decode_luma(&blank, 64, 64, 4), None);
    }
}
