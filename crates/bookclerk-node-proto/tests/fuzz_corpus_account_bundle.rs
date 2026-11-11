//! Replays the checked-in `node_account_bundle` fuzz corpus in PR CI (no
//! libFuzzer required), through the same oracle the target uses, so a
//! bundle that once broke a parsing invariant stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `node_account_bundle` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/node_account_bundle.rs"]
mod oracle;

/// Regular files directly under `fuzz/corpus/node_account_bundle/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/node_account_bundle")
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
    assert!(!files.is_empty(), "empty node_account_bundle corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn node_account_bundle_corpus_keeps_every_invariant() {
    let mut accepted = 0usize;
    let mut refused = 0usize;
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
        if bookclerk_node_proto::parse_account_bundle(&data, oracle::CLUSTER_ID, oracle::NODE_ID)
            .is_ok()
        {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    // The seeds include bundles the parser accepts, so the round trip
    // runs, and bundles it refuses.
    assert!(accepted > 0, "no corpus bundle parsed");
    assert!(refused > 0, "every corpus bundle parsed");
}
