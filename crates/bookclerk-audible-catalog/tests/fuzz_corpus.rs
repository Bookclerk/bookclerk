//! Replays the checked-in `audible_catalog` fuzz corpus in PR CI (no
//! libFuzzer required), through the same oracle the target uses, so a
//! catalog answer that once broke a parsing invariant stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `audible_catalog` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/audible_catalog.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/audible_catalog/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/audible_catalog")
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
    assert!(!files.is_empty(), "empty audible_catalog corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn audible_catalog_corpus_keeps_every_invariant() {
    let mut genres = 0usize;
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
        if let Some((&selector, body)) = data.split_first() {
            let list = if selector % 2 == 0 {
                bookclerk_audible_catalog::CatalogList::Products
            } else {
                bookclerk_audible_catalog::CatalogList::SimilarProducts
            };
            genres += bookclerk_audible_catalog::parse_catalog_products(body, list)
                .map_or(0, |titles| {
                    titles.iter().filter(|t| t.categories.is_some()).count()
                });
        }
    }
    // The seeds include real answers, so the per-title genre checks run.
    assert!(genres > 0, "no corpus input yielded a title with genres");
}

#[test]
#[allow(clippy::expect_used)]
fn merchandising_flags_never_become_genres() {
    let body = br#"{"products":[{"asin":"B002UP1LXS","title":"The Great Hunt",
        "thesaurus_subject_keywords":["AI_edition_inclusion","la_confidential"]}]}"#;
    let titles = bookclerk_audible_catalog::parse_catalog_products(
        body,
        bookclerk_audible_catalog::CatalogList::Products,
    )
    .expect("parse");
    assert_eq!(titles.len(), 1);
    assert_eq!(titles[0].categories, None);
}
