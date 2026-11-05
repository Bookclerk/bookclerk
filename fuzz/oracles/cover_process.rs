//! Shared oracle for the `cover_process` fuzz target and the
//! `bookclerk-cover` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A cover is downloaded from any public website, so its bytes are
//! untrusted. The invariants, checked against independent readings of the
//! input signature and the output JPEG headers rather than the `image`
//! crate the pipeline decodes with:
//!
//! - an input without a JPEG, PNG or WebP signature is refused;
//! - every accepted cover yields three baseline-or-progressive JPEGs that
//!   start with SOI, end with EOI and have three components sampled 1×1
//!   (4:4:4, never 4:2:0);
//! - the full image's frame header matches the reported size and fits in
//!   2400 px, the card fits in 360 px and within the full image, and the
//!   thumbnail fits in 128 px and within the card.

use bookclerk_cover::{process, CoverError, FULL_MAX_EDGE};

/// Largest input worth checking; decoding is the expensive part.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The would-be cover file.
///
/// # Panics
///
/// When the pipeline disagrees with the oracle.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let result = process(data);
    if data.is_empty() {
        assert_eq!(result, Err(CoverError::Empty));
        return;
    }
    if !has_signature(data) {
        assert_eq!(
            result,
            Err(CoverError::UnknownFormat),
            "accepted {} bytes without a signature",
            data.len()
        );
        return;
    }
    let Ok(set) = result else {
        return;
    };
    let full = frame(&set.full).expect("full image is a JPEG");
    let card = frame(&set.card).expect("card image is a JPEG");
    let thumb = frame(&set.thumb).expect("thumbnail is a JPEG");
    assert_eq!(full, (set.width, set.height), "reported size");
    for ((width, height), edge, within) in [
        (full, FULL_MAX_EDGE, full),
        (card, 360, full),
        (thumb, 128, card),
    ] {
        assert!(width >= 1 && height >= 1, "empty image");
        assert!(width.max(height) <= edge, "{width}x{height} over {edge}");
        assert!(width <= within.0 && height <= within.1, "enlarged");
    }
}

/// Whether `data` starts with a JPEG, PNG or WebP signature, read byte by
/// byte.
fn has_signature(data: &[u8]) -> bool {
    let jpeg = data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF;
    let png = data.len() >= 8 && data[..8] == [137, 80, 78, 71, 13, 10, 26, 10];
    let webp = data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP";
    jpeg || png || webp
}

/// Width and height from a JPEG's frame header, after checking the SOI and
/// EOI markers and 4:4:4 sampling.
///
/// # Returns
///
/// `None` when the bytes are not such a JPEG.
fn frame(jpeg: &[u8]) -> Option<(u32, u32)> {
    if jpeg.len() < 4 || jpeg[..2] != [0xFF, 0xD8] || jpeg[jpeg.len() - 2..] != [0xFF, 0xD9] {
        return None;
    }
    let mut at = 2;
    loop {
        if *jpeg.get(at)? != 0xFF {
            return None;
        }
        let marker = *jpeg.get(at + 1)?;
        let len = usize::from(*jpeg.get(at + 2)?) << 8 | usize::from(*jpeg.get(at + 3)?);
        let segment = jpeg.get(at + 4..at + 2 + len)?;
        // SOF0 (baseline) or SOF2 (progressive), Huffman-coded.
        if marker == 0xC0 || marker == 0xC2 {
            let height = u32::from(segment[1]) << 8 | u32::from(segment[2]);
            let width = u32::from(segment[3]) << 8 | u32::from(segment[4]);
            let components = usize::from(segment[5]);
            if components != 3 || segment.len() < 6 + 3 * components {
                return None;
            }
            for component in 0..components {
                if segment[6 + 3 * component + 1] != 0x11 {
                    return None;
                }
            }
            return Some((width, height));
        }
        if marker == 0xDA {
            return None;
        }
        at += 2 + len;
    }
}
