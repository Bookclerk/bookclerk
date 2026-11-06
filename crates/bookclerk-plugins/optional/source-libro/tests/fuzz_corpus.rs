//! Replays the checked-in `libro_catalog` fuzz corpus in PR CI (no
//! libFuzzer required), through the same oracle the target uses, so a
//! catalog answer that once broke a parsing invariant stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `libro_catalog` oracle, shared with the fuzz target.
#[path = "../../../../../fuzz/oracles/libro_catalog.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/libro_catalog/`, canonicalized
/// and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../fuzz/corpus/libro_catalog")
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
    assert!(!files.is_empty(), "empty libro_catalog corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn libro_catalog_corpus_keeps_every_invariant() {
    let mut hits = 0usize;
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
        if let Some((&selector, rest)) = data.split_first() {
            hits += if selector % 2 == 0 {
                rest.split_first()
                    .and_then(|(&limit, body)| {
                        bookclerk_plugin_source_libro::parse_catalog_search_page(
                            body,
                            usize::from(limit),
                        )
                        .ok()
                    })
                    .map_or(0, |h| h.len())
            } else {
                bookclerk_plugin_source_libro::parse_catalog_details(rest)
                    .ok()
                    .map_or(0, |d| d.related.len() + usize::from(d.audiobook.is_some()))
            };
        }
    }
    // The seeds include real pages, so the per-row checks run.
    assert!(hits > 0, "no corpus input yielded a hit");
}
