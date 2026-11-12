//! Replays the checked-in `graphicaudio_catalog` fuzz corpus in PR CI (no
//! libFuzzer required), through the same oracle the target uses, so a
//! product page that once broke a parsing invariant stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `graphicaudio_catalog` oracle, shared with the fuzz target.
#[path = "../../../../../fuzz/oracles/graphicaudio_catalog.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/graphicaudio_catalog/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../fuzz/corpus/graphicaudio_catalog")
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
    assert!(!files.is_empty(), "empty graphicaudio_catalog corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn graphicaudio_catalog_corpus_keeps_every_invariant() {
    let mut records = 0usize;
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
        records += usize::from(
            bookclerk_plugin_source_graphicaudio::parse_product_detail(&String::from_utf8_lossy(
                &data,
            ))
            .is_some(),
        );
    }
    // The seeds include real pages, so the per-field checks run.
    assert!(records > 0, "no corpus input yielded a record");
}
