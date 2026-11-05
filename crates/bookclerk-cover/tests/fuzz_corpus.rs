//! Replays the checked-in `cover_process`, `cover_wire` and `cover_token`
//! fuzz corpora in PR CI (no libFuzzer required), so an input that once broke
//! the cover pipeline, the worker frames or the token check stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! `UPDATE_FUZZ_SEEDS=1` rewrites the generated seeds; without it the test
//! fails when a generated seed no longer matches its file.
// Test code: a failed assertion or a missing corpus file is the test failing.
#![allow(
    clippy::missing_panics_doc,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::cast_possible_truncation
)]

use std::io::Cursor;
use std::path::{Path, PathBuf};

use bookclerk_cover::wire::{encode_reply, encode_request, Reply, Request};
use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

#[path = "../../../fuzz/oracles/cover_process.rs"]
mod cover_process_oracle;

#[path = "../../../fuzz/oracles/cover_wire.rs"]
mod cover_wire_oracle;

#[path = "../../../fuzz/oracles/cover_token.rs"]
mod cover_token_oracle;

/// `fuzz/corpus/<target>`.
fn corpus_dir(target: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
}

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized and
/// confined to that directory.
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = corpus_dir(target)
        .canonicalize()
        .expect("canonicalize corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty corpus for {target}");
    files
}

/// Writes (with `UPDATE_FUZZ_SEEDS`) or checks one generated seed.
fn seed(target: &str, name: &str, bytes: &[u8]) {
    let path = corpus_dir(target).join(name);
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::write(&path, bytes).expect("write seed");
    }
    assert_eq!(
        std::fs::read(&path).expect("read seed"),
        bytes,
        "{} is stale; rerun with UPDATE_FUZZ_SEEDS=1",
        path.display()
    );
}

/// `image` encoded as `format`.
fn encoded(image: &image::DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, format).expect("encode seed");
    out.into_inner()
}

/// A 5 × 9 solid image.
fn tall() -> image::DynamicImage {
    image::DynamicImage::ImageRgb8(RgbImage::from_pixel(5, 9, Rgb([1, 2, 3])))
}

/// The generated `cover_process` seeds, by file name.
fn process_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let wide = image::DynamicImage::ImageRgb8(RgbImage::from_fn(400, 90, |x, y| {
        Rgb([(x % 256) as u8, (y * 2) as u8, 128])
    }));
    let alpha = image::DynamicImage::ImageRgba8(RgbaImage::from_fn(3, 2, |x, _| {
        Rgba([200, 10, 10, (x * 100) as u8])
    }));
    let png = encoded(&wide, ImageFormat::Png);
    let mut big_header = encoded(&tall(), ImageFormat::Png);
    // IHDR width and height (bytes 16..24) claim 6000 × 6000.
    big_header[16..24].copy_from_slice(&[0, 0, 0x17, 0x70, 0, 0, 0x17, 0x70]);
    vec![
        ("wide.png", png.clone()),
        ("alpha.png", encoded(&alpha, ImageFormat::Png)),
        ("tall.jpg", encoded(&tall(), ImageFormat::Jpeg)),
        ("wide.jpg", encoded(&wide, ImageFormat::Jpeg)),
        ("alpha.webp", encoded(&alpha, ImageFormat::WebP)),
        ("truncated.png", png[..png.len() / 2].to_vec()),
        ("oversized_header.png", big_header),
        (
            "svg.txt",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"alert(1)\"/>".to_vec(),
        ),
        ("gif.bin", b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec()),
        ("riff_junk.bin", b"RIFF\x04\x00\x00\x00WEBPVP8 ".to_vec()),
        ("jpeg_signature_only.bin", vec![0xFF, 0xD8, 0xFF]),
    ]
}

/// The generated `cover_wire` seeds: selector byte (even: request, odd:
/// reply), then the frame body.
fn wire_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let set = bookclerk_cover::process(&encoded(&tall(), ImageFormat::Png)).expect("set");
    let body = |frame: Vec<u8>| frame[4..].to_vec();
    let with = |selector: u8, body: Vec<u8>| [vec![selector], body].concat();
    let request = body(
        encode_request(&Request {
            id: 7,
            cover: b"\xFF\xD8\xFFcover".to_vec(),
        })
        .expect("request"),
    );
    let ok = body(encode_reply(&Reply::Ok { id: 7, set }).expect("ok"));
    let err = body(
        encode_reply(&Reply::Err {
            id: 8,
            message: "not a JPEG, PNG or WebP image".into(),
        })
        .expect("err"),
    );
    let mut trailing = ok.clone();
    trailing.push(0);
    let mut unknown_tag = err.clone();
    unknown_tag[8] = 9;
    vec![
        ("request.bin", with(0, request.clone())),
        ("request_short.bin", with(0, request[..5].to_vec())),
        ("reply_ok.bin", with(1, ok.clone())),
        ("reply_err.bin", with(1, err)),
        ("reply_trailing.bin", with(1, trailing)),
        ("reply_truncated.bin", with(1, ok[..ok.len() - 3].to_vec())),
        ("reply_unknown_tag.bin", with(1, unknown_tag)),
    ]
}

/// The generated `cover_token` seeds: tokens under the oracle's key, and
/// URL inputs (first byte picks the byte to flip, the rest is the URL).
fn token_seeds() -> Vec<(&'static str, Vec<u8>)> {
    use base64::Engine as _;
    let key = bookclerk_cover::token::TokenKey::from_bytes([0x5a; 32]);
    let url = "https://m.media-amazon.com/images/I/51abc.jpg";
    let token = bookclerk_cover::token::sign(&key, url).expect("token");
    let (encoded, tag) = token.split_once('.').expect("dot");
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let http = engine.encode("http://img.example/a.jpg");
    vec![
        ("valid.txt", token.clone().into_bytes()),
        (
            "short_tag.txt",
            format!("{encoded}.{}", &tag[..8]).into_bytes(),
        ),
        ("long_tag.txt", format!("{token}AA").into_bytes()),
        ("no_dot.txt", encoded.as_bytes().to_vec()),
        ("swapped_url.txt", format!("{http}.{tag}").into_bytes()),
        ("padded.txt", format!("{encoded}=.{tag}").into_bytes()),
        ("url_https.bin", [b"".as_slice(), url.as_bytes()].concat()),
        ("url_http.bin", b"http://img.example/a.jpg".to_vec()),
        (
            "url_userinfo.bin",
            b"https://u:p@img.example:8443/a?b#c".to_vec(),
        ),
    ]
}

#[test]
fn generated_seeds_are_current() {
    for (name, bytes) in process_seeds() {
        seed("cover_process", name, &bytes);
    }
    for (name, bytes) in wire_seeds() {
        seed("cover_wire", name, &bytes);
    }
    for (name, bytes) in token_seeds() {
        seed("cover_token", name, &bytes);
    }
}

#[test]
fn fuzz_corpus_cover_process_holds_invariants() {
    let mut accepted = 0;
    for path in corpus_files("cover_process") {
        let data = std::fs::read(&path).expect("read");
        cover_process_oracle::check(&data);
        accepted += usize::from(bookclerk_cover::process(&data).is_ok());
    }
    // The valid images in the corpus exercise the success invariants.
    assert!(accepted >= 4, "only {accepted} seeds decoded");
}

#[test]
fn fuzz_corpus_cover_wire_holds_invariants() {
    for path in corpus_files("cover_wire") {
        cover_wire_oracle::check(&std::fs::read(&path).expect("read"));
    }
}

#[test]
fn fuzz_corpus_cover_token_holds_invariants() {
    let key = bookclerk_cover::token::TokenKey::from_bytes([0x5a; 32]);
    let mut opened = 0;
    for path in corpus_files("cover_token") {
        let data = std::fs::read(&path).expect("read");
        cover_token_oracle::check(&data);
        opened += usize::from(
            std::str::from_utf8(&data)
                .ok()
                .and_then(|token| bookclerk_cover::token::open(&key, token))
                .is_some(),
        );
    }
    // Only the one valid token in the corpus opens.
    assert_eq!(opened, 1);
}
