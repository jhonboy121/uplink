//! Compares two renders by the bands of content they contain, rather than pixel by pixel.
//!
//! The design is light and the build is dark, so colours cannot be subtracted. What can be
//! compared is where ink sits: each row of the image is classified as background or not, giving
//! a list of horizontal bands with their vertical and horizontal extents. Those are the same
//! numbers in either theme.

use anyhow::{Context, Result};
use image::RgbImage;

/// How far from the background a channel must be to count as ink, out of 255. Low enough to
/// catch a hairline: the design's #E7ECF2 separator is only 24 off its white ground.
const INK: i32 = 10;
/// Rows with less ink than this are background; kills antialiasing fringes.
const MIN_PIXELS: u32 = 2;
/// Bands closer than this merge, so a row of text is one band and not one per glyph row.
const MERGE: u32 = 3;

struct Band {
    top: u32,
    bottom: u32,
    start: u32,
    end: u32,
}

pub fn run(left: &str, right: &str) -> Result<()> {
    let (a, b) = (load(left)?, load(right)?);
    let (bands_a, bands_b) = (bands(&a), bands(&b));
    println!("{left}: {}x{}, {} bands", a.width(), a.height(), bands_a.len());
    println!("{right}: {}x{}, {} bands", b.width(), b.height(), bands_b.len());
    println!("\n  #  {:>18}  {:>18}   dy  dh  dx  dw", "design y..y  x..x", "build y..y  x..x");

    for (index, pair) in bands_a.iter().zip(&bands_b).enumerate() {
        let (one, two) = pair;
        let height = |band: &Band| band.bottom - band.top + 1;
        let width = |band: &Band| band.end - band.start + 1;
        let delta = |x: u32, y: u32| i64::from(y) - i64::from(x);
        println!(
            "{:3}  {:>7}..{:<3} {:>3}..{:<3}  {:>7}..{:<3} {:>3}..{:<3}  {:>3} {:>3} {:>3} {:>3}",
            index + 1,
            one.top,
            one.bottom,
            one.start,
            one.end,
            two.top,
            two.bottom,
            two.start,
            two.end,
            delta(one.top, two.top),
            delta(height(one), height(two)),
            delta(one.start, two.start),
            delta(width(one), width(two)),
        );
    }
    if bands_a.len() != bands_b.len() {
        println!("\nband counts differ: {} vs {}", bands_a.len(), bands_b.len());
    }
    Ok(())
}

fn load(path: &str) -> Result<RgbImage> {
    Ok(image::open(path).with_context(|| format!("opening {path}"))?.to_rgb8())
}

/// Contiguous runs of rows that hold ink, with the horizontal extent of each run.
fn bands(image: &RgbImage) -> Vec<Band> {
    let background = *image.get_pixel(1, 1);
    let inked = |pixel: &image::Rgb<u8>| {
        (0..3).any(|channel| (i32::from(pixel.0[channel]) - i32::from(background.0[channel])).abs() > INK)
    };

    let mut out: Vec<Band> = Vec::new();
    for y in 0..image.height() {
        let row: Vec<u32> = (0..image.width()).filter(|&x| inked(image.get_pixel(x, y))).collect();
        if u32::try_from(row.len()).unwrap_or(u32::MAX) < MIN_PIXELS {
            continue;
        }
        let (start, end) = (row[0], row[row.len() - 1]);
        match out.last_mut() {
            Some(last) if y <= last.bottom + MERGE => {
                last.bottom = y;
                last.start = last.start.min(start);
                last.end = last.end.max(end);
            }
            _ => out.push(Band { top: y, bottom: y, start, end }),
        }
    }
    out
}
