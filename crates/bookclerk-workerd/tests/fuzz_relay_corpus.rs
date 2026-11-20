//! Replays the checked-in `egress_relay_head` fuzz corpus in PR CI (no
//! libFuzzer required) through the same oracle the fuzz target runs, so a
//! head that once broke the egress relay's parsing stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).

// Test code: the shared oracle asserts with expect/unwrap.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

#[path = "../../../fuzz/oracles/egress_relay_head.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/egress_relay_head/`,
/// canonicalized and confined to that directory.
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/egress_relay_head")
        .canonicalize()
        .expect("canonicalize fuzz corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty egress_relay_head corpus");
    files
}

#[test]
fn fuzz_corpus_egress_relay_head_holds_the_oracle() {
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read");
        oracle::check(&data);
    }
}
