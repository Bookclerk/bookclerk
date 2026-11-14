//! Replays the checked-in `node_frame`, `node_account_bundle`,
//! `client_authorization`, `client_authorize` and `client_token_request` fuzz
//! corpora in PR CI (no libFuzzer required), through the same oracles the
//! targets use, so an input that once broke a parsing invariant stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// The `node_frame` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/node_frame.rs"]
mod oracle;

/// The `node_account_bundle` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/node_account_bundle.rs"]
mod bundle_oracle;

/// The `client_authorization` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/client_authorization.rs"]
mod client_authorization_oracle;

/// The `client_authorize` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/client_authorize.rs"]
mod client_authorize_oracle;

/// The `client_token_request` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/client_token_request.rs"]
mod client_token_oracle;

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized
/// and confined to that directory.
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
    assert!(!files.is_empty(), "empty {target} corpus");
    files
}

#[test]
#[allow(clippy::expect_used)]
fn node_frame_corpus_keeps_every_invariant() {
    let mut accepted = 0usize;
    for path in corpus_files("node_frame") {
        let data = std::fs::read(&path).expect("read corpus file");
        oracle::check(&data);
        let parsed = std::str::from_utf8(&data)
            .ok()
            .and_then(|text| bookclerk_node_proto::Frame::parse(text).ok());
        accepted += usize::from(parsed.is_some());
    }
    // The seeds include frames the parser accepts, so the round trip runs.
    assert!(accepted > 0, "no corpus frame parsed");
}

#[test]
#[allow(clippy::expect_used)]
fn node_account_bundle_corpus_keeps_every_invariant() {
    let mut accepted = 0usize;
    let mut refused = 0usize;
    for path in corpus_files("node_account_bundle") {
        let data = std::fs::read(&path).expect("read corpus file");
        bundle_oracle::check(&data);
        let parsed = bookclerk_node_proto::parse_account_bundle(
            &data,
            bundle_oracle::CLUSTER_ID,
            bundle_oracle::NODE_ID,
        );
        if parsed.is_ok() {
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

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_client_authorization_replays() {
    let (mut accepted, mut refused) = (0, 0);
    for path in corpus_files("client_authorization") {
        if client_authorization_oracle::check(&std::fs::read(&path).expect("read")) {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    // The seeds reach both sides.
    assert!(accepted > 0, "no client_authorization seed was accepted");
    assert!(refused > 0, "every client_authorization seed was accepted");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_client_authorize_replays() {
    let (mut accepted, mut refused) = (0, 0);
    for path in corpus_files("client_authorize") {
        let data = std::fs::read(&path).expect("read");
        let ok = client_authorize_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say which side they are on in their name.
        assert_eq!(ok, name.starts_with("accepted"), "seed {name}");
        if ok {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    assert!(accepted > 0, "no client_authorize seed was accepted");
    assert!(refused > 0, "every client_authorize seed was accepted");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_client_token_request_replays() {
    let (mut accepted, mut refused) = (0, 0);
    for path in corpus_files("client_token_request") {
        let data = std::fs::read(&path).expect("read");
        let ok = client_token_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say which side they are on in their name.
        assert_eq!(ok, name.starts_with("accepted"), "seed {name}");
        if ok {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    assert!(accepted > 0, "no client_token_request seed was accepted");
    assert!(refused > 0, "every client_token_request seed was accepted");
}
