//! Time budget for a page of covers on one core: the small-VPS case.
//!
//! A search or Discover page warms 20 to 24 covers. On a one-core node they
//! are processed one after another, so a page must finish well inside the
//! time a person waits for thumbnails. Store covers are usually 500 px
//! squares; one 2400 px cover stands in for the large outliers.

#![allow(clippy::expect_used)]

use std::io::Cursor;
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, RgbImage};

/// Covers on one page.
const PAGE: usize = 20;

/// The longest a page may take on one core in an optimized build.
const RELEASE_BUDGET: Duration = Duration::from_secs(2);

/// The same budget for an unoptimized test build, where decoding and
/// resizing run many times slower.
const DEBUG_BUDGET: Duration = Duration::from_secs(60);

/// A `size × size` JPEG with photo-like detail, so the decoder and resizer
/// do real work.
///
/// # Arguments
///
/// * `size` - Edge length in pixels.
/// * `seed` - Varies the pixels so no two covers are the same.
fn cover(size: u32, seed: u32) -> Vec<u8> {
    let pixels = RgbImage::from_fn(size, size, |x, y| {
        let mix = x.wrapping_mul(31) ^ y.wrapping_mul(17) ^ seed.wrapping_mul(2_654_435_761);
        let [a, b, c, _] = mix.to_le_bytes();
        image::Rgb([a, b ^ c, c.wrapping_add(a)])
    });
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Jpeg)
        .expect("encode jpeg");
    out
}

#[test]
fn a_page_of_covers_fits_the_one_core_budget() {
    let mut page: Vec<Vec<u8>> = (1..PAGE)
        .map(|i| cover(500, u32::try_from(i).expect("small")))
        .collect();
    page.push(cover(2400, 0));

    let started = Instant::now();
    for bytes in &page {
        let set = bookclerk_cover::process(bytes).expect("cover processes");
        assert!(!set.thumb.is_empty() && !set.card.is_empty() && !set.full.is_empty());
    }
    let took = started.elapsed();

    let budget = if cfg!(debug_assertions) {
        DEBUG_BUDGET
    } else {
        RELEASE_BUDGET
    };
    eprintln!("{PAGE} covers on one core: {took:?} (budget {budget:?})");
    assert!(
        took <= budget,
        "{PAGE} covers took {took:?}, over {budget:?}"
    );
}
