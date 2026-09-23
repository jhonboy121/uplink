//! The picture of an identity: the key as a QR code, the uplink mark on a plate in the middle,
//! and a line underneath saying what to do with it. This is what "share my identity" sends, and
//! what the other side opens with "choose an image".
//!
//! Everything is drawn here rather than by the platform, so the one thing that matters about the
//! card — that it still decodes with the mark over it — is a test rather than a hope.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

use crate::{Error, qr};

/// The design's paper and ink. The code is drawn in ink like everything else: at 5% luminance it
/// is black as far as a decoder is concerned, and it keeps the card one object.
const PAPER: [u8; 3] = [0xFF, 0xFF, 0xFF];
const INK: [u8; 3] = [0x0E, 0x15, 0x1E];
/// Pixels per module. A code of a key is ~45 modules, so the drawing lands near 1100px across —
/// large enough to survive whatever it is sent through, small enough to be a picture.
const MODULE: u32 = 21;
/// Paper around the code, in modules. The code is drawn bare, so this is also its quiet zone: the
/// spec asks for four, and a decoder hunting for the finder patterns needs them. The rest is so
/// the card survives being cropped.
const MARGIN: u32 = 6;
/// Between the code and the line under it, and the same again under the line.
const CAPTION_GAP: u32 = 2;
/// The caption's size, also in modules, so the whole card scales with one number.
const CAPTION_SIZE: f32 = 3.4;
/// The plate under the mark is round, so the mark is the square on its diameter — 1/√2 of the
/// width. `Theme.mark-in-plate` says the same about the code on screen.
const MARK_IN_PLATE: f32 = 0.707;

/// The app's own face and mark, vendored once and used by the UI through Slint's include path.
const FONT: &[u8] = include_bytes!("../../../assets/fonts/Outfit-SemiBold.ttf");
const MARK: &[u8] = include_bytes!("../../../assets/icons/mark-badge.png");

/// Draws `key` as a card captioned `caption`, and returns it as a PNG.
pub fn identity(key: &str, caption: &str) -> Result<Vec<u8>, Error> {
    let matrix = qr::encode(key)?;
    let size = u32::try_from(matrix.size).map_err(|_| Error::Qr("code too large".into()))?;
    let (code, margin) = (size * MODULE, MARGIN * MODULE);
    let width = code + margin * 2;

    let font = FontRef::try_from_slice(FONT).map_err(|e| Error::Card(e.to_string()))?;
    let caption = Caption::fit(&font, caption, code);
    let height = margin * 2 + code + CAPTION_GAP * MODULE + caption.height;

    let mut card = Canvas::new(width, height);
    for y in 0..matrix.size {
        for x in 0..matrix.size {
            if !matrix.dark(x, y) {
                continue;
            }
            let (left, top) = (u32::try_from(x), u32::try_from(y));
            let (Ok(left), Ok(top)) = (left, top) else { continue };
            card.fill(margin + left * MODULE, margin + top * MODULE, MODULE, MODULE, INK);
        }
    }

    // The plate takes the middle of the code, which is why the code is drawn at the error
    // correction that can lose it; `logo` is how many modules qr says that is.
    let centre = margin as f32 + code as f32 / 2.0;
    let plate = matrix.logo() as f32 * MODULE as f32;
    card.disc(centre, centre, plate / 2.0, PAPER);
    card.stamp(MARK, centre, centre, plate * MARK_IN_PLATE)?;

    let baseline = (margin + code + CAPTION_GAP * MODULE) as f32 + caption.ascent;
    caption.draw(&mut card, width as f32 / 2.0, baseline);
    card.png()
}

/// A line of text, measured once so the card can be sized around it.
struct Caption<'a, 'font> {
    font: &'a FontRef<'font>,
    text: &'a str,
    scale: PxScale,
    /// Distance from the line's top to its baseline, and the whole line's height.
    ascent: f32,
    height: u32,
    width: f32,
}

impl<'a, 'font> Caption<'a, 'font> {
    /// At the card's own size, unless that would run off the paper — a longer translation shrinks
    /// to fit rather than being clipped.
    fn fit(font: &'a FontRef<'font>, text: &'a str, available: u32) -> Self {
        let mut size = CAPTION_SIZE * MODULE as f32;
        let width = advance(font, text, PxScale::from(size));
        if width > available as f32 {
            size *= available as f32 / width;
        }
        let scale = PxScale::from(size);
        let scaled = font.as_scaled(scale);
        Self {
            font,
            text,
            scale,
            ascent: scaled.ascent(),
            height: (scaled.ascent() - scaled.descent()).ceil() as u32,
            width: advance(font, text, scale),
        }
    }

    /// Centred on `centre`, sitting on `baseline`.
    fn draw(&self, card: &mut Canvas, centre: f32, baseline: f32) {
        let scaled = self.font.as_scaled(self.scale);
        let mut caret = centre - self.width / 2.0;
        let mut previous = None;
        for character in self.text.chars() {
            let id = scaled.glyph_id(character);
            if let Some(previous) = previous {
                caret += scaled.kern(previous, id);
            }
            let glyph = id.with_scale_and_position(self.scale, ab_glyph::point(caret, baseline));
            if let Some(outline) = self.font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|x, y, coverage| {
                    let x = bounds.min.x + x as f32;
                    let y = bounds.min.y + y as f32;
                    card.blend(x as i64, y as i64, INK, coverage);
                });
            }
            caret += scaled.h_advance(id);
            previous = Some(id);
        }
    }
}

/// How wide `text` is at `scale`, kerning included.
fn advance(font: &FontRef, text: &str, scale: PxScale) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let id = scaled.glyph_id(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

/// Somewhere to draw: RGB, because nothing about a card is transparent.
struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Canvas {
    const CHANNELS: usize = 3;

    fn new(width: u32, height: u32) -> Self {
        let pixels = PAPER.iter().copied().cycle().take(width as usize * height as usize * Self::CHANNELS);
        Self { width, height, pixels: pixels.collect() }
    }

    fn fill(&mut self, left: u32, top: u32, width: u32, height: u32, colour: [u8; 3]) {
        for y in top..(top + height).min(self.height) {
            for x in left..(left + width).min(self.width) {
                self.set(x, y, colour);
            }
        }
    }

    /// A filled circle, with its edge softened by how much of each pixel it covers.
    fn disc(&mut self, centre_x: f32, centre_y: f32, radius: f32, colour: [u8; 3]) {
        let reach = radius.ceil() as i64 + 1;
        for y in -reach..=reach {
            for x in -reach..=reach {
                let (px, py) = (centre_x + x as f32, centre_y + y as f32);
                let distance = ((px - centre_x).powi(2) + (py - centre_y).powi(2)).sqrt();
                let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
                self.blend(px as i64, py as i64, colour, coverage);
            }
        }
    }

    /// Draws a PNG centred on a point, scaled to `side` and composited over what is there.
    fn stamp(&mut self, png: &[u8], centre_x: f32, centre_y: f32, side: f32) -> Result<(), Error> {
        let side = side.round().max(1.0) as u32;
        let mark = image::load_from_memory_with_format(png, image::ImageFormat::Png)
            .map_err(|e| Error::Card(e.to_string()))?;
        let mark = image::imageops::resize(&mark.to_rgba8(), side, side, image::imageops::FilterType::Lanczos3);
        let left = centre_x - side as f32 / 2.0;
        let top = centre_y - side as f32 / 2.0;
        for (x, y, pixel) in mark.enumerate_pixels() {
            let [r, g, b, alpha] = pixel.0;
            let coverage = f32::from(alpha) / f32::from(u8::MAX);
            self.blend(left as i64 + i64::from(x), top as i64 + i64::from(y), [r, g, b], coverage);
        }
        Ok(())
    }

    /// `coverage` of `colour` over whatever is already there. Off the canvas is a no-op, so
    /// callers can draw shapes that run past an edge without checking first.
    fn blend(&mut self, x: i64, y: i64, colour: [u8; 3], coverage: f32) {
        let coverage = coverage.clamp(0.0, 1.0);
        if coverage == 0.0 {
            return;
        }
        let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else { return };
        let Some(under) = self.at(x, y) else { return };
        let mut over = [0; Self::CHANNELS];
        for (channel, (new, old)) in over.iter_mut().zip(colour.iter().zip(under)) {
            *channel = (f32::from(*new) * coverage + f32::from(old) * (1.0 - coverage)).round() as u8;
        }
        self.set(x, y, over);
    }

    fn at(&self, x: u32, y: u32) -> Option<[u8; 3]> {
        let start = self.index(x, y)?;
        let pixel = self.pixels.get(start..start + Self::CHANNELS)?;
        Some([*pixel.first()?, *pixel.get(1)?, *pixel.get(2)?])
    }

    fn set(&mut self, x: u32, y: u32, colour: [u8; 3]) {
        let Some(start) = self.index(x, y) else { return };
        if let Some(pixel) = self.pixels.get_mut(start..start + Self::CHANNELS) {
            pixel.copy_from_slice(&colour);
        }
    }

    fn index(&self, x: u32, y: u32) -> Option<usize> {
        (x < self.width && y < self.height)
            .then(|| (y as usize * self.width as usize + x as usize) * Self::CHANNELS)
    }

    fn png(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            &self.pixels,
            self.width,
            self.height,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| Error::Card(e.to_string()))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0e62c81291e383bbc63b305338200d8562904d24a46cd6412f9bae0c7ae111f4";
    const CAPTION: &str = "Scan to connect";

    /// The whole point: what we send is still a code someone can scan, mark, caption and all.
    #[test]
    fn the_card_reads_back_as_the_key() -> Result<(), Error> {
        let png = identity(KEY, CAPTION)?;
        let card = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .map_err(|e| Error::Card(e.to_string()))?
            .to_luma8();
        let (width, height) = (card.width() as usize, card.height() as usize);
        assert_eq!(qr::decode_luma(card.as_raw(), width, height, width).as_deref(), Some(KEY));
        Ok(())
    }

    /// Paper at the corners, so the quiet zone is really there and the card is not drawn edge to
    /// edge — a code that touches the border is a code a decoder walks past.
    #[test]
    fn the_code_has_its_quiet_zone() -> Result<(), Error> {
        let png = identity(KEY, CAPTION)?;
        let card = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .map_err(|e| Error::Card(e.to_string()))?
            .to_rgb8();
        for (x, y) in [(0, 0), (card.width() - 1, 0), (0, card.height() - 1)] {
            assert_eq!(card.get_pixel(x, y).0, PAPER, "({x}, {y}) is not paper");
        }
        Ok(())
    }

    /// A caption too long for the card is made to fit rather than running off it.
    #[test]
    fn a_long_caption_shrinks() -> Result<(), Error> {
        let font = FontRef::try_from_slice(FONT).map_err(|e| Error::Card(e.to_string()))?;
        let long = "Scan this code to connect to me, wherever either of us happens to be";
        let code = 45 * MODULE;
        assert!(Caption::fit(&font, long, code).width <= code as f32);
        assert!(Caption::fit(&font, CAPTION, code).width < code as f32);
        Ok(())
    }
}
