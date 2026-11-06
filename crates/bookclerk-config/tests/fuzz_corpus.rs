//! Replays the checked-in `cache_settings` fuzz corpus through the shared
//! oracle, so ordinary CI checks every seed
//! (`docs/adr/untrusted-input-fuzzing.md`).

use std::path::PathBuf;

/// The `cache_settings` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/cache_settings.rs"]
mod cache_settings_oracle;

/// Every file in `fuzz/corpus/<target>/`, sorted.
// Test code: an unreadable corpus directory is the test failing.
#[allow(clippy::expect_used)]
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("corpus directory")
        .map(|entry| entry.expect("corpus entry").path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

/// Mirrors `fuzz/fuzz_targets/cache_settings.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_cache_settings_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("cache_settings") {
        cache_settings_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no cache_settings seed");
}
