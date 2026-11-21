//! Replays the checked-in `bookclerk-config` fuzz corpora (`cache_settings`,
//! `destination_id`, `diagnostics_submit_reply`, `diagnostics_lookup_reply`,
//! `relayed_log_line`)
//! through their shared
//! oracles, so ordinary CI checks every seed
//! (`docs/adr/untrusted-input-fuzzing.md`).

use std::path::PathBuf;

/// The `cache_settings` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/cache_settings.rs"]
mod cache_settings_oracle;

/// The `destination_filter` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/destination_filter.rs"]
mod destination_filter_oracle;

/// The `destination_id` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/destination_id.rs"]
mod destination_id_oracle;

/// The `output_copies` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/output_copies.rs"]
mod output_copies_oracle;

/// The `diagnostics_submit_reply` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/diagnostics_submit_reply.rs"]
mod diagnostics_submit_reply_oracle;

/// The `relayed_log_line` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/relayed_log_line.rs"]
mod relayed_log_line_oracle;

/// The `diagnostics_lookup_reply` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/diagnostics_lookup_reply.rs"]
mod diagnostics_lookup_reply_oracle;

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

/// Mirrors `fuzz/fuzz_targets/destination_id.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_destination_id_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("destination_id") {
        destination_id_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no destination_id seed");
}

/// Mirrors `fuzz/fuzz_targets/diagnostics_submit_reply.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_diagnostics_submit_reply_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("diagnostics_submit_reply") {
        diagnostics_submit_reply_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no diagnostics_submit_reply seed");
}

/// Mirrors `fuzz/fuzz_targets/diagnostics_lookup_reply.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_diagnostics_lookup_reply_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("diagnostics_lookup_reply") {
        diagnostics_lookup_reply_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no diagnostics_lookup_reply seed");
}

/// Mirrors `fuzz/fuzz_targets/destination_filter.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_destination_filter_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("destination_filter") {
        destination_filter_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no destination_filter seed");
}

/// Mirrors `fuzz/fuzz_targets/output_copies.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_output_copies_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("output_copies") {
        output_copies_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no output_copies seed");
}

/// Mirrors `fuzz/fuzz_targets/relayed_log_line.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_relayed_log_line_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("relayed_log_line") {
        relayed_log_line_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no relayed_log_line seed");
}
