//! Replays the checked-in `mux_peer` fuzz corpus in PR CI (no libFuzzer
//! required) with the fuzz target's oracle: every peer byte stream in the
//! corpus is fed to a server mux, which must settle, stay within its bounds
//! and leave no task behind (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The oracle lives in `fuzz/oracles/mux_peer.rs` and is included here with
//! `#[path]`, so this test and the fuzz target cannot drift apart.
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

#[path = "../../../fuzz/oracles/mux_peer.rs"]
#[allow(dead_code)]
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

/// The oracle's task-count check is process-wide, so every mux this binary
/// creates runs inside this one test.
#[test]
fn fuzz_corpus_mux_peer_settles_and_stays_bounded() {
    let mut delivered_any = false;
    for path in corpus_files("mux_peer") {
        let data = std::fs::read(&path).expect("read");
        let outcome = oracle::check(&data);
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        match stem {
            // One peer stream carrying "hello", then closed.
            "open_data_close" => {
                assert_eq!(outcome.accepted, 1, "{stem}: {outcome:?}");
                assert_eq!(outcome.received, 5, "{stem}: {outcome:?}");
            }
            // An even id is the server's own parity: the open is rejected.
            "open_wrong_parity" | "data_unknown_stream" | "empty" => {
                assert_eq!(outcome.accepted, 0, "{stem}: {outcome:?}");
                assert_eq!(outcome.received, 0, "{stem}: {outcome:?}");
            }
            _ => {}
        }
        assert!(outcome.local_write_ok, "{stem}: local open or write failed");
        delivered_any |= outcome.received > 0;
    }
    assert!(delivered_any, "no mux_peer seed delivered any payload");
}
