//! Replays the checked-in `library_resolve` fuzz corpus in PR CI (no
//! libFuzzer required), through the same `run_case` oracle the target
//! uses, so an input that once broke a resolver invariant stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// Regular files directly under `fuzz/corpus/library_resolve/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/library_resolve")
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
    assert!(!files.is_empty(), "empty library_resolve corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn library_resolve_corpus_keeps_every_invariant() {
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        if let Err(broken) = bookclerk_resolve::fuzzing::run_case(&data) {
            panic!("{}: {broken}", path.display());
        }
    }
}
