//! Replays the checked-in `throttle_reason` fuzz corpus in PR CI (no
//! libFuzzer required), through the same oracle the target uses, so a
//! refusal text that was once misclassified stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `throttle_reason` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/throttle_reason.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/throttle_reason/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/throttle_reason")
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
    assert!(!files.is_empty(), "empty throttle_reason corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn throttle_reason_corpus_keeps_every_invariant() {
    let mut throttles = 0usize;
    let mut refusals = 0usize;
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
        if bookclerk_storefront::is_throttle_reason(&String::from_utf8_lossy(&data)) {
            throttles += 1;
        } else {
            refusals += 1;
        }
    }
    // The seeds hold both kinds, so both sides of each check run.
    assert!(throttles > 0, "no corpus input was a throttle");
    assert!(refusals > 0, "no corpus input was a plain refusal");
}
