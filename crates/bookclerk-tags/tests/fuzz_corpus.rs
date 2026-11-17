//! Replays the checked-in `tags_read` and `tags_wire` fuzz corpora in PR CI
//! (no libFuzzer required), so a file that once broke the tag reader, or a
//! frame that once broke the worker protocol, stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! `UPDATE_FUZZ_SEEDS=1` rewrites the generated seeds; without it the test
//! fails when a generated seed no longer matches its file.
// Test code: a failed assertion or a missing corpus file is the test failing.
#![allow(clippy::missing_panics_doc, clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use bookclerk_tags::fixtures::{mp4_box, tagged_flac, tagged_m4b, tagged_mp3};
use bookclerk_tags::wire::{encode_reply, encode_request, Reply, Request};
use bookclerk_tags::{read_all, ByteSpan};

#[path = "../../../fuzz/oracles/tags_read.rs"]
mod tags_read_oracle;

#[path = "../../../fuzz/oracles/tags_wire.rs"]
mod tags_wire_oracle;

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

/// The generated `tags_read` seeds: each container whole, cut short, and
/// with a structure that lies about its size.
fn read_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let m4b = tagged_m4b();
    let mp3 = tagged_mp3();
    let flac = tagged_flac();
    let mut huge_box = m4b.clone();
    // The moov box (after ftyp and mdat) claims to run past the file.
    let moov_at = huge_box.windows(4).position(|w| w == b"moov").unwrap() - 4;
    huge_box[moov_at..moov_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut huge_id3 = mp3.clone();
    huge_id3[6..10].copy_from_slice(&[0x7F, 0x7F, 0x7F, 0x7F]);
    let mut cbr = Vec::new();
    for _ in 0..40 {
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        cbr.extend(frame);
    }
    vec![
        ("tagged.m4b", m4b.clone()),
        ("tagged.mp3", mp3.clone()),
        ("tagged.flac", flac.clone()),
        ("cbr.mp3", cbr),
        ("cut.m4b", m4b[..m4b.len() - 100].to_vec()),
        ("cut.mp3", mp3[..mp3.len() / 3].to_vec()),
        ("cut.flac", flac[..flac.len() / 2].to_vec()),
        ("huge_moov.m4b", huge_box),
        ("huge_id3.mp3", huge_id3),
        ("ftyp_only.m4b", mp4_box(b"ftyp", b"M4B \0\0\0\0")),
        (
            "not_audio.txt",
            b"just some text, not a media file".to_vec(),
        ),
    ]
}

/// The generated `tags_wire` seeds: selector byte (even: request, odd:
/// reply), then a frame body.
fn wire_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let request = |request: Request| {
        let frame = encode_request(&request).unwrap();
        [&[0u8][..], &frame[4..]].concat()
    };
    let reply = |reply: Reply| {
        let frame = encode_reply(&reply).unwrap();
        [&[1u8][..], &frame[4..]].concat()
    };
    let record = read_all(&tagged_flac()).unwrap();
    let mut too_many = vec![1u8, 0];
    too_many.extend_from_slice(&1u64.to_be_bytes());
    too_many.extend_from_slice(&65u16.to_be_bytes());
    vec![
        ("open.bin", request(Request::Open { id: 1, size: 4096 })),
        (
            "data.bin",
            request(Request::Data {
                id: 1,
                offset: 0,
                bytes: b"fLaC".to_vec(),
            }),
        ),
        ("close.bin", request(Request::Close { id: 1 })),
        (
            "need.bin",
            reply(Reply::Need {
                id: 1,
                spans: vec![
                    ByteSpan { offset: 0, len: 10 },
                    ByteSpan {
                        offset: 100,
                        len: 65_536,
                    },
                ],
            }),
        ),
        (
            "done.bin",
            reply(Reply::Done {
                id: 1,
                record: Box::new(record),
            }),
        ),
        (
            "failed.bin",
            reply(Reply::Failed {
                id: 1,
                message: "malformed MP4".into(),
            }),
        ),
        ("need_too_many.bin", too_many),
        ("unknown_tag.bin", vec![1, 9, 0, 0, 0, 0, 0, 0, 0, 1]),
        (
            "done_not_json.bin",
            vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 1, b'{'],
        ),
    ]
}

#[test]
fn generated_seeds_are_current() {
    for (name, bytes) in read_seeds() {
        seed("tags_read", name, &bytes);
    }
    for (name, bytes) in wire_seeds() {
        seed("tags_wire", name, &bytes);
    }
}

#[test]
fn fuzz_corpus_tags_read_holds_invariants() {
    let mut accepted = 0;
    for path in corpus_files("tags_read") {
        let data = std::fs::read(&path).expect("read");
        tags_read_oracle::check(&data);
        accepted += usize::from(read_all(&data).is_ok());
    }
    // The whole files in the corpus exercise the success invariants.
    assert!(accepted >= 4, "only {accepted} seeds read");
}

#[test]
fn fuzz_corpus_tags_wire_holds_invariants() {
    for path in corpus_files("tags_wire") {
        tags_wire_oracle::check(&std::fs::read(&path).expect("read"));
    }
}
