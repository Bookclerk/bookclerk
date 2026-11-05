//! Cover image pipeline for untrusted cover art.
//!
//! A cover reaches the host as bytes a plugin downloaded from some public
//! website. Nothing about them is trusted: not the `Content-Type`, not the
//! extension, not the dimensions the header claims. [`process`] turns such
//! bytes into a fixed set of JPEGs the host can store and serve:
//!
//! 1. [`sniff`] reads the file signature and accepts only JPEG, PNG and WebP.
//! 2. The `image` crate decodes it with that format forced (never guessed)
//!    and with [`MAX_DIMENSION`] and [`MAX_DECODE_ALLOC`] as hard limits.
//! 3. Transparency is flattened onto white, so every output is plain RGB.
//! 4. `fast_image_resize` (Lanczos3) fits the image into each
//!    [`CoverSize`], never enlarging it.
//! 5. `jpeg-encoder` writes each size as a 4:4:4 JPEG (no chroma
//!    subsampling), at quality [`THUMB_QUALITY`] for the small sizes and
//!    [`FULL_QUALITY`] for the full one.
//!
//! Every codec here is pure Rust, and the function works on a byte slice with
//! no I/O, so the fuzzer and the tests drive exactly what the media worker
//! runs. [`wire`] is the frame format the host and the worker exchange.

pub mod wire;

use std::io::Cursor;

use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, ImageFormat, ImageReader, Limits};

/// Largest input accepted, in bytes. Matches the plugin ABI's
/// `maxCoverBytes`, the most a cover fetcher may hand the host.
pub const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;

/// Widest or tallest image decoded, in pixels. Store covers are at most a
/// few thousand pixels on a side; anything larger is refused before decoding.
pub const MAX_DIMENSION: u32 = 5000;

/// Most memory the decoder may allocate for one image, in bytes.
pub const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;

/// Longest edge of the [`CoverSize::Full`] output, in pixels.
pub const FULL_MAX_EDGE: u32 = 2400;

/// JPEG quality of the [`CoverSize::Thumb`] and [`CoverSize::Card`] outputs.
pub const THUMB_QUALITY: u8 = 85;

/// JPEG quality of the [`CoverSize::Full`] output.
pub const FULL_QUALITY: u8 = 90;

/// An image format [`sniff`] recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverFormat {
    /// JPEG (`FF D8 FF`).
    Jpeg,
    /// PNG (the eight-byte PNG signature).
    Png,
    /// WebP (`RIFF` … `WEBP`).
    Webp,
}

impl CoverFormat {
    /// The `image` crate format to force when decoding.
    fn image_format(self) -> ImageFormat {
        match self {
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Png => ImageFormat::Png,
            Self::Webp => ImageFormat::WebP,
        }
    }
}

/// Reads the file signature of `bytes`.
///
/// # Arguments
///
/// * `bytes` - The start of the file (the whole file is fine).
///
/// # Returns
///
/// The format the signature names, or `None` for anything else, including
/// GIF, SVG, HTML and every text format.
#[must_use]
pub fn sniff(bytes: &[u8]) -> Option<CoverFormat> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(CoverFormat::Jpeg);
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(CoverFormat::Png);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some(CoverFormat::Webp);
    }
    None
}

/// One of the sizes [`process`] produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CoverSize {
    /// Fits in 128 × 128: list rows and search results.
    Thumb,
    /// Fits in 360 × 360: cards and detail panes.
    Card,
    /// Fits in [`FULL_MAX_EDGE`] on its longer edge: the book page and the
    /// cover embedded into an acquired file.
    Full,
}

impl CoverSize {
    /// Every size, smallest first.
    pub const ALL: [Self; 3] = [Self::Thumb, Self::Card, Self::Full];

    /// The longest edge this size allows, in pixels.
    #[must_use]
    pub fn max_edge(self) -> u32 {
        match self {
            Self::Thumb => 128,
            Self::Card => 360,
            Self::Full => FULL_MAX_EDGE,
        }
    }

    /// The size's name in URLs and file names (`128`, `360`, `full`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thumb => "128",
            Self::Card => "360",
            Self::Full => "full",
        }
    }

    /// Parses a name [`Self::as_str`] produces.
    ///
    /// # Arguments
    ///
    /// * `name` - `128`, `360` or `full`, exactly.
    ///
    /// # Returns
    ///
    /// The size, or `None` for any other text.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|size| size.as_str() == name)
    }
}

/// The JPEGs [`process`] produced for one cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverSet {
    /// Width of the [`CoverSize::Full`] image, in pixels.
    pub width: u32,
    /// Height of the [`CoverSize::Full`] image, in pixels.
    pub height: u32,
    /// [`CoverSize::Thumb`] JPEG.
    pub thumb: Vec<u8>,
    /// [`CoverSize::Card`] JPEG.
    pub card: Vec<u8>,
    /// [`CoverSize::Full`] JPEG.
    pub full: Vec<u8>,
}

impl CoverSet {
    /// The JPEG for `size`.
    #[must_use]
    pub fn get(&self, size: CoverSize) -> &[u8] {
        match size {
            CoverSize::Thumb => &self.thumb,
            CoverSize::Card => &self.card,
            CoverSize::Full => &self.full,
        }
    }
}

/// Why [`process`] refused a cover.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoverError {
    /// The input was empty.
    #[error("empty image")]
    Empty,
    /// The input is larger than [`MAX_INPUT_BYTES`].
    #[error("image is larger than {MAX_INPUT_BYTES} bytes")]
    TooLarge,
    /// The signature is not JPEG, PNG or WebP.
    #[error("not a JPEG, PNG or WebP image")]
    UnknownFormat,
    /// The decoder refused the image or hit a limit.
    #[error("could not decode image: {0}")]
    Decode(String),
    /// Resizing failed.
    #[error("could not resize image: {0}")]
    Resize(String),
    /// Encoding failed.
    #[error("could not encode image: {0}")]
    Encode(String),
}

/// Sniffs, decodes, resizes and re-encodes one untrusted cover.
///
/// # Arguments
///
/// * `bytes` - The downloaded file, untrusted.
///
/// # Returns
///
/// Every [`CoverSize`] as a JPEG.
///
/// # Errors
///
/// A [`CoverError`] for an empty, oversized or unrecognized input, an image
/// over the decode limits, or a codec failure.
pub fn process(bytes: &[u8]) -> Result<CoverSet, CoverError> {
    if bytes.is_empty() {
        return Err(CoverError::Empty);
    }
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(CoverError::TooLarge);
    }
    let format = sniff(bytes).ok_or(CoverError::UnknownFormat)?;
    let source = decode(bytes, format)?;
    let full = fit(&source, CoverSize::Full.max_edge())?;
    let card = fit(&full, CoverSize::Card.max_edge())?;
    let thumb = fit(&card, CoverSize::Thumb.max_edge())?;
    Ok(CoverSet {
        width: full.width,
        height: full.height,
        thumb: encode(&thumb, THUMB_QUALITY, false)?,
        card: encode(&card, THUMB_QUALITY, false)?,
        full: encode(&full, FULL_QUALITY, true)?,
    })
}

/// An RGB image with three bytes per pixel, row-major, no padding.
#[derive(Debug, Clone)]
struct Rgb {
    /// Width in pixels; at least one.
    width: u32,
    /// Height in pixels; at least one.
    height: u32,
    /// `width * height * 3` bytes.
    pixels: Vec<u8>,
}

/// Decodes `bytes` as `format` under the limits, flattened to RGB on white.
fn decode(bytes: &[u8], format: CoverFormat) -> Result<Rgb, CoverError> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format.image_format());
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|err| CoverError::Decode(err.to_string()))?;
    if image.width() == 0 || image.height() == 0 {
        return Err(CoverError::Decode("image has no pixels".into()));
    }
    Ok(flatten(image))
}

/// Converts any decoded image to RGB, blending transparency onto white.
fn flatten(image: DynamicImage) -> Rgb {
    let (width, height) = (image.width(), image.height());
    let pixels = if image.color().has_alpha() {
        let rgba = image.into_rgba8();
        let mut out = Vec::with_capacity(rgba.as_raw().len() / 4 * 3);
        for [r, g, b, a] in rgba.as_raw().as_chunks::<4>().0 {
            let alpha = u16::from(*a);
            for &channel in [r, g, b] {
                // channel * a + 255 * (255 - a), over 255, rounded.
                let blended = (u16::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255;
                out.push(u8::try_from(blended).unwrap_or(u8::MAX));
            }
        }
        out
    } else {
        image.into_rgb8().into_raw()
    };
    Rgb {
        width,
        height,
        pixels,
    }
}

/// The size `width × height` takes inside `edge × edge`, never enlarged.
///
/// # Returns
///
/// The new width and height, each at least one pixel.
#[must_use]
fn fitted(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= edge {
        return (width, height);
    }
    let scale = |side: u32| -> u32 {
        // side * edge / longest, rounded, in integer arithmetic.
        let scaled =
            (u64::from(side) * u64::from(edge) + u64::from(longest) / 2) / u64::from(longest);
        u32::try_from(scaled).unwrap_or(edge).clamp(1, edge)
    };
    (scale(width), scale(height))
}

/// Resizes `image` to fit `edge`, or clones it when it already fits.
fn fit(image: &Rgb, edge: u32) -> Result<Rgb, CoverError> {
    let (width, height) = fitted(image.width, image.height, edge);
    if (width, height) == (image.width, image.height) {
        return Ok(image.clone());
    }
    let src = Image::from_vec_u8(
        image.width,
        image.height,
        image.pixels.clone(),
        PixelType::U8x3,
    )
    .map_err(|err| CoverError::Resize(err.to_string()))?;
    let mut dst = Image::new(width, height, PixelType::U8x3);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    Resizer::new()
        .resize(&src, &mut dst, &options)
        .map_err(|err| CoverError::Resize(err.to_string()))?;
    Ok(Rgb {
        width,
        height,
        pixels: dst.into_vec(),
    })
}

/// Encodes `image` as a 4:4:4 JPEG with optimized Huffman tables.
fn encode(image: &Rgb, quality: u8, progressive: bool) -> Result<Vec<u8>, CoverError> {
    let width = u16::try_from(image.width).map_err(|_| CoverError::Encode("too wide".into()))?;
    let height = u16::try_from(image.height).map_err(|_| CoverError::Encode("too tall".into()))?;
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
    encoder.set_optimized_huffman_tables(true);
    encoder.set_progressive(progressive);
    encoder
        .encode(&image.pixels, width, height, jpeg_encoder::ColorType::Rgb)
        .map_err(|err| CoverError::Encode(err.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The low byte of a coordinate.
    fn byte(v: u32) -> u8 {
        v.to_le_bytes()[0]
    }

    /// A `width × height` PNG of a diagonal gradient, with alpha when asked.
    fn png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let image = if alpha {
            DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |x, y| {
                image::Rgba([byte(x), byte(y), 128, 0])
            }))
        } else {
            DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
                image::Rgb([byte(x), byte(y), 128])
            }))
        };
        image
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .expect("encode png");
        out
    }

    /// Decodes a JPEG output to its dimensions and first pixel.
    fn jpeg_info(bytes: &[u8]) -> (u32, u32, [u8; 3]) {
        assert_eq!(sniff(bytes), Some(CoverFormat::Jpeg));
        let image = image::load_from_memory_with_format(bytes, ImageFormat::Jpeg)
            .expect("decode output")
            .into_rgb8();
        let first = image.get_pixel(0, 0).0;
        (image.width(), image.height(), first)
    }

    #[test]
    fn sniff_reads_signatures_only() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(CoverFormat::Jpeg));
        assert_eq!(sniff(&png(1, 1, false)), Some(CoverFormat::Png));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(CoverFormat::Webp));
        for other in [
            &b""[..],
            b"\xFF\xD8",
            b"GIF89a",
            b"<svg xmlns=",
            b"<!doctype html>",
            b"RIFF\0\0\0\0WAVE",
            b"RIFF\0\0\0\0WEB",
        ] {
            assert_eq!(sniff(other), None, "{other:?}");
        }
    }

    #[test]
    fn sizes_round_trip_their_names() {
        for size in CoverSize::ALL {
            assert_eq!(CoverSize::parse(size.as_str()), Some(size));
        }
        for bad in ["", "64", "FULL", "full ", "1280"] {
            assert_eq!(CoverSize::parse(bad), None, "{bad:?}");
        }
        assert!(CoverSize::Thumb.max_edge() < CoverSize::Card.max_edge());
        assert!(CoverSize::Card.max_edge() < CoverSize::Full.max_edge());
    }

    #[test]
    fn fitted_keeps_aspect_and_never_enlarges() {
        assert_eq!(fitted(100, 50, 128), (100, 50));
        assert_eq!(fitted(1000, 1000, 128), (128, 128));
        assert_eq!(fitted(2000, 1000, 360), (360, 180));
        assert_eq!(fitted(1000, 3000, 360), (120, 360));
        assert_eq!(fitted(5000, 1, 128), (128, 1));
    }

    #[test]
    fn process_writes_every_size_as_jpeg() {
        let set = process(&png(3000, 1500, false)).expect("processed");
        assert_eq!((set.width, set.height), (2400, 1200));
        assert_eq!(jpeg_info(&set.full).0, 2400);
        let (w, h, _) = jpeg_info(&set.card);
        assert_eq!((w, h), (360, 180));
        let (w, h, _) = jpeg_info(&set.thumb);
        assert_eq!((w, h), (128, 64));
        for size in CoverSize::ALL {
            assert!(!set.get(size).is_empty());
        }
    }

    #[test]
    fn process_keeps_small_images_at_their_size() {
        let set = process(&png(100, 80, false)).expect("processed");
        assert_eq!((set.width, set.height), (100, 80));
        let (w, h, _) = jpeg_info(&set.thumb);
        assert_eq!((w, h), (100, 80));
    }

    #[test]
    fn transparency_becomes_white() {
        let set = process(&png(16, 16, true)).expect("processed");
        let (_, _, first) = jpeg_info(&set.thumb);
        assert!(first.iter().all(|&c| c > 245), "{first:?}");
    }

    #[test]
    fn process_refuses_what_is_not_a_cover() {
        assert_eq!(process(&[]), Err(CoverError::Empty));
        assert_eq!(
            process(&vec![0xFF; MAX_INPUT_BYTES + 1]),
            Err(CoverError::TooLarge)
        );
        assert_eq!(process(b"GIF89a....."), Err(CoverError::UnknownFormat));
        assert_eq!(
            process(b"<html>not an image</html>"),
            Err(CoverError::UnknownFormat)
        );
        assert!(matches!(
            process(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
            Err(CoverError::Decode(_))
        ));
        let mut truncated = png(64, 64, false);
        truncated.truncate(40);
        assert!(matches!(process(&truncated), Err(CoverError::Decode(_))));
    }

    #[test]
    fn process_refuses_oversized_dimensions() {
        let wide = png(MAX_DIMENSION + 1, 1, false);
        assert!(matches!(process(&wide), Err(CoverError::Decode(_))));
    }
}
