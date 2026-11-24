//! Replays the checked-in `chirp_catalog_detail` fuzz corpus in PR CI (no
//! libFuzzer required), through the same oracle the target uses, so a
//! detail answer that once broke a parsing invariant stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `chirp_catalog_detail` oracle, shared with the fuzz target.
#[path = "../../../../../fuzz/oracles/chirp_catalog_detail.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/chirp_catalog_detail/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../fuzz/corpus/chirp_catalog_detail")
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
    assert!(!files.is_empty(), "empty chirp_catalog_detail corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn chirp_catalog_detail_corpus_keeps_every_invariant() {
    let mut samples = 0usize;
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
        let answer: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(answer) => answer,
            Err(_) => continue,
        };
        if let Ok(Some(hit)) = bookclerk_plugin_source_chirp::audiobook_detail_record(&answer) {
            samples += usize::from(hit.sample_url.is_some());
        }
    }
    // The seeds include a real answer, so the sample checks run.
    assert!(samples > 0, "no corpus input yielded a sample");
}
