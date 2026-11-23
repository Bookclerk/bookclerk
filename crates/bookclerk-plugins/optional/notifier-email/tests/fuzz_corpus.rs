//! Replays the checked-in `smtp_session` fuzz corpus in PR CI (no
//! libFuzzer required), through the same oracle the target uses, so a
//! server stream or address document that once broke an invariant stays
//! fixed (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `smtp_session` oracle, shared with the fuzz target.
#[path = "../../../../../fuzz/oracles/smtp_session.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/smtp_session/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../fuzz/corpus/smtp_session")
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
    assert!(!files.is_empty(), "empty smtp_session corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn smtp_session_corpus_keeps_every_invariant() {
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
    }
}
