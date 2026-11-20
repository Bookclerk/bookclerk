//! Replays the checked-in `title_meta_fill` fuzz corpus in PR CI (no
//! libFuzzer required) with the fuzz target's oracle: a title-meta query is
//! served exactly when it has the documented shape, and the storefront
//! lookups it plans follow the documented order and caps
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The oracle lives in `fuzz/oracles/title_meta_fill.rs` and is included
//! here with `#[path]`, so this test and the fuzz target cannot drift apart.
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

#[path = "../../../fuzz/oracles/title_meta_fill.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized and
/// confined to that directory.
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
    assert!(!files.is_empty(), "empty fuzz corpus for {target}");
    files
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_title_meta_fill_replays() {
    let (mut served, mut refused) = (0, 0);
    for path in corpus_files("title_meta_fill") {
        if oracle::check(&std::fs::read(&path).expect("read")) {
            served += 1;
        } else {
            refused += 1;
        }
    }
    // The seeds reach both sides.
    assert!(served > 0, "no title_meta_fill seed was served");
    assert!(refused > 0, "every title_meta_fill seed was refused");
}
