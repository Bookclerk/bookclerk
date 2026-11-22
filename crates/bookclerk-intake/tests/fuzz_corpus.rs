//! Replays the checked-in `intake_items` fuzz corpus through its shared
//! oracle, so ordinary CI checks every seed
//! (`docs/adr/untrusted-input-fuzzing.md`).

use std::path::PathBuf;

/// The `intake_items` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/intake_items.rs"]
mod intake_items_oracle;

/// Mirrors `fuzz/fuzz_targets/intake_items.rs` with the shared oracle.
// Test code: an unreadable corpus is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_intake_items_holds_invariants() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/intake_items");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("corpus directory")
        .map(|entry| entry.expect("corpus entry").path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no intake_items seed");
    for path in files {
        intake_items_oracle::check(&std::fs::read(&path).expect("read"));
    }
}
