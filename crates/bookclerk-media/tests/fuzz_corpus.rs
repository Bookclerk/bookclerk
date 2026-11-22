//! Replays the checked-in `media_decode` fuzz corpus in PR CI (no libFuzzer
//! required), so a file that once broke the media worker's decode path
//! stays fixed (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The generated seeds come from `decode::tests::generate_fuzz_seeds`
//! (`UPDATE_FUZZ_SEEDS=1 cargo test -p bookclerk-media generate_fuzz_seeds`).
//! The `libav_*` seeds are `bookclerk-libav`'s sample files (WMA, AC-3,
//! E-AC-3, AMR, AMR in 3GP), which only the `libav` feature decodes.
// Test code: a failed assertion or a missing corpus file is the test failing.
#![allow(clippy::missing_panics_doc, clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use bookclerk_media::{decode_pcm_bytes, OutChannels};

#[path = "../../../fuzz/oracles/media_decode.rs"]
mod media_decode_oracle;

/// Regular files directly under `fuzz/corpus/media_decode/`, canonicalized
/// and confined to that directory.
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/media_decode")
        .canonicalize()
        .expect("canonicalize corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty media_decode corpus");
    files
}

#[test]
fn fuzz_corpus_media_decode_holds_invariants() {
    let mut accepted = 0;
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read");
        media_decode_oracle::check(&data);
        accepted += usize::from(decode_pcm_bytes(data, OutChannels::Mono, |_, _| Ok(())).is_ok());
    }
    // The whole AAC and MP3 files exercise the success invariants.
    assert!(accepted >= 4, "only {accepted} seeds decoded");
}

/// With the FFmpeg decoders (every Unix and Windows test build), each
/// `libav_*` seed decodes.
#[cfg(any(unix, all(windows, target_env = "msvc")))]
#[test]
fn fuzz_corpus_media_decode_libav_seeds_decode() {
    let seeds: Vec<PathBuf> = corpus_files()
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libav_"))
        })
        .collect();
    assert!(seeds.len() >= 8, "only {} libav seeds", seeds.len());
    for path in seeds {
        let data = std::fs::read(&path).expect("read");
        decode_pcm_bytes(data, OutChannels::Mono, |_, _| Ok(()))
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    }
}

#[path = "../../../fuzz/oracles/media_keep_codec.rs"]
mod media_keep_codec_oracle;

#[test]
fn fuzz_corpus_media_keep_codec_holds_invariants() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/media_keep_codec")
        .canonicalize()
        .expect("canonicalize corpus");
    let (mut count, mut kept) = (0, 0);
    for entry in std::fs::read_dir(&dir).expect("fuzz corpus") {
        let Ok(path) = entry.expect("entry").path().canonicalize() else {
            continue;
        };
        if path.starts_with(&dir) && path.is_file() {
            kept += usize::from(media_keep_codec_oracle::check(
                &std::fs::read(&path).expect("read"),
            ));
            count += 1;
        }
    }
    assert!(count >= 6, "only {count} seeds");
    // The Opus, FLAC and ALAC seeds exercise the success invariants.
    assert!(kept >= 4, "only {kept} seeds were kept");
}

#[path = "../../../fuzz/oracles/media_pcm_wire.rs"]
mod media_pcm_wire_oracle;

/// Regular files directly under `fuzz/corpus/media_pcm_wire/`.
fn pcm_wire_corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/media_pcm_wire")
}

/// The generated `media_pcm_wire` seeds: selector byte (`0 mod 3`
/// request, `1 mod 3` reply, `2 mod 3` stream after a piece-size byte),
/// then the bytes.
fn pcm_wire_seeds() -> Vec<(&'static str, Vec<u8>)> {
    use bookclerk_media::pcm_wire::{encode_reply, encode_request, PcmReply, PcmRequest};
    use bookclerk_media::PcmHeader;

    let body = |frame: Vec<u8>| frame[4..].to_vec();
    let with = |selector: u8, bytes: Vec<u8>| [vec![selector], bytes].concat();
    let header = PcmHeader {
        sample_rate: 44_100,
        channels: 2,
        start_ms: 2_000,
        duration_ms: Some(3_600_000),
    };
    let reply = |reply: PcmReply| encode_reply(&reply).expect("encode");
    let stream: Vec<u8> = [
        reply(PcmReply::Need {
            offset: 0,
            len: 262_144,
        }),
        reply(PcmReply::Format(header)),
        reply(PcmReply::Pcm(vec![1, -1, 2, -2])),
        reply(PcmReply::Need {
            offset: 262_144,
            len: 4096,
        }),
        reply(PcmReply::Pcm(vec![i16::MAX, i16::MIN])),
        reply(PcmReply::End),
    ]
    .concat();
    let half_frame: Vec<u8> = [
        reply(PcmReply::Format(header)),
        reply(PcmReply::Pcm(vec![1, 2, 3])),
    ]
    .concat();
    let failed: Vec<u8> = [
        reply(PcmReply::Format(PcmHeader {
            channels: 1,
            duration_ms: None,
            ..header
        })),
        reply(PcmReply::Failed {
            message: "decode error".into(),
        }),
        reply(PcmReply::End),
    ]
    .concat();
    vec![
        (
            "request_open",
            with(
                0,
                body(
                    encode_request(&PcmRequest::Open {
                        size: 1 << 30,
                        start_ms: 90_000,
                    })
                    .expect("encode"),
                ),
            ),
        ),
        (
            "request_data",
            with(
                3,
                body(
                    encode_request(&PcmRequest::Data {
                        offset: 7,
                        bytes: vec![0xaa; 16],
                    })
                    .expect("encode"),
                ),
            ),
        ),
        (
            "reply_need",
            with(1, body(reply(PcmReply::Need { offset: 9, len: 1 }))),
        ),
        (
            "reply_format",
            with(4, body(reply(PcmReply::Format(header)))),
        ),
        (
            "reply_pcm",
            with(7, body(reply(PcmReply::Pcm(vec![0, 1, 2, 3])))),
        ),
        (
            "reply_failed",
            with(
                10,
                body(reply(PcmReply::Failed {
                    message: "no audio track".into(),
                })),
            ),
        ),
        ("reply_end", with(13, body(reply(PcmReply::End)))),
        ("reply_unknown_tag", vec![1, 9]),
        ("reply_bad_format", vec![1, 1, 0, 0, 0, 0, 3]),
        ("stream_whole", [vec![2, 7], stream.clone()].concat()),
        ("stream_one_byte_pieces", [vec![5, 1], stream].concat()),
        ("stream_half_frame", [vec![8, 64], half_frame].concat()),
        ("stream_after_failure", [vec![11, 3], failed].concat()),
        (
            "stream_oversized_prefix",
            vec![2, 4, 0xff, 0xff, 0xff, 0xff, 0],
        ),
    ]
}

#[test]
fn fuzz_corpus_media_pcm_wire_holds_invariants() {
    let dir = pcm_wire_corpus();
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::create_dir_all(&dir).expect("corpus dir");
        for (name, bytes) in pcm_wire_seeds() {
            std::fs::write(dir.join(name), bytes).expect("write seed");
        }
    }
    for (name, bytes) in pcm_wire_seeds() {
        let stored =
            std::fs::read(dir.join(name)).expect("seed missing; run with UPDATE_FUZZ_SEEDS=1");
        assert_eq!(
            stored, bytes,
            "{name} is stale; run with UPDATE_FUZZ_SEEDS=1"
        );
    }
    let dir = dir.canonicalize().expect("canonicalize corpus");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("fuzz corpus") {
        let path = entry.expect("entry").path();
        let Ok(path) = path.canonicalize() else {
            continue;
        };
        if path.starts_with(&dir) && path.is_file() {
            media_pcm_wire_oracle::check(&std::fs::read(&path).expect("read"));
            count += 1;
        }
    }
    assert!(count >= pcm_wire_seeds().len());
}
