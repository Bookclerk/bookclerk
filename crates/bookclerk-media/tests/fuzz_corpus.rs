//! Replays the checked-in `media_decode` fuzz corpus in PR CI (no libFuzzer
//! required), so a file that once broke the media worker's decode path
//! stays fixed (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The generated seeds come from `decode::tests::generate_fuzz_seeds`
//! (`UPDATE_FUZZ_SEEDS=1 cargo test -p bookclerk-media generate_fuzz_seeds`).
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
