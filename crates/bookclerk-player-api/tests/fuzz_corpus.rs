//! Replays the checked-in `player_request` and `player_socket` fuzz
//! corpora in PR CI (no libFuzzer required), through the same oracles the
//! targets use, so an input that once broke a parsing rule stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `player_request` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/player_request.rs"]
mod request_oracle;

/// The `player_socket` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/player_socket.rs"]
mod socket_oracle;

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized
/// and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
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
    assert!(!files.is_empty(), "empty {target} corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn player_request_corpus_keeps_every_rule() {
    for path in corpus_files("player_request") {
        let data = std::fs::read(&path).expect("read corpus file");
        request_oracle::check(&data);
    }
}

#[test]
#[allow(clippy::expect_used)]
fn player_socket_corpus_keeps_every_rule() {
    let mut accepted = 0usize;
    for path in corpus_files("player_socket") {
        let data = std::fs::read(&path).expect("read corpus file");
        socket_oracle::check(&data);
        let text = std::str::from_utf8(&data).expect("UTF-8 seed");
        accepted += usize::from(bookclerk_player_api::socket::parse_client_frame(text).is_ok());
    }
    assert!(accepted > 0, "no corpus frame parsed");
}
