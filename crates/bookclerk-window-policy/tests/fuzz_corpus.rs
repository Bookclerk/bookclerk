//! Replays the checked-in `window_policy` fuzz corpus in PR CI (no libFuzzer
//! required) with the fuzz target's oracle: the desktop window's
//! custom-scheme policy refuses, rewrites and navigates exactly as the
//! oracle restates it (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The oracle lives in `fuzz/oracles/window_policy.rs` and is included here
//! with `#[path]`, so this test and the fuzz target cannot drift apart.
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

#[path = "../../../fuzz/oracles/window_policy.rs"]
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
    assert!(!files.is_empty(), "empty corpus for {target}");
    files
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_window_policy_replays() {
    let (mut routed, mut refused) = (0, 0);
    for path in corpus_files("window_policy") {
        let reached = oracle::check(&std::fs::read(&path).expect("read"));
        let named_refused = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("refused"));
        if !name_is_regression(&path) {
            assert_eq!(
                !reached,
                named_refused,
                "{} was {}",
                path.display(),
                if reached { "routed" } else { "refused" }
            );
        }
        if reached {
            routed += 1;
        } else {
            refused += 1;
        }
    }
    // The seeds reach both sides.
    assert!(routed > 0, "no window_policy seed reached the router");
    assert!(refused > 0, "every window_policy seed reached the router");
}

/// Whether a seed is a minimized fuzzing regression, whose verdict is not
/// named in its file.
fn name_is_regression(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("regression_"))
}
