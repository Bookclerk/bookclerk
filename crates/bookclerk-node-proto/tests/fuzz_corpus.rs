//! Replays the checked-in `node_frame` fuzz corpus in PR CI (no libFuzzer
//! required), through the same oracle the target uses, so a frame that once
//! broke a parsing invariant stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `node_frame` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/node_frame.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/node_frame/`, canonicalized
/// and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/node_frame")
        .canonicalize()
        .expect("canonicalize fuzz corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        // Inline canonicalize → starts_with at each sink (CodeQL two-state barrier).
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty node_frame corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn node_frame_corpus_keeps_every_invariant() {
    let mut accepted = 0usize;
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
        let parsed = std::str::from_utf8(&data)
            .ok()
            .and_then(|text| bookclerk_node_proto::Frame::parse(text).ok());
        accepted += usize::from(parsed.is_some());
    }
    // The seeds include frames the parser accepts, so the round trip runs.
    assert!(accepted > 0, "no corpus frame parsed");
}
