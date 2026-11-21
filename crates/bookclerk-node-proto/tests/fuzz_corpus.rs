//! Replays the checked-in `node_frame`, `node_account_bundle`,
//! `clerk_authorization`, `clerk_authorize`, `clerk_token_request`,
//! `clerk_token_answer`, `clerk_socket_frame`, `copy_range`, `copy_answer`
//! and `upload_part` fuzz corpora
//! in PR CI (no libFuzzer required), through the same oracles the targets
//! use, so an input that once broke a parsing invariant stays fixed
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

/// The `clerk_authorization` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/clerk_authorization.rs"]
mod clerk_authorization_oracle;

/// The `clerk_authorize` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/clerk_authorize.rs"]
mod clerk_authorize_oracle;

/// The `clerk_token_request` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/clerk_token_request.rs"]
mod clerk_token_oracle;

/// The `copy_range` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/copy_range.rs"]
mod copy_range_oracle;

/// The `copy_answer` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/copy_answer.rs"]
mod copy_answer_oracle;

/// The `upload_part` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/upload_part.rs"]
mod upload_part_oracle;

/// The `clerk_token_answer` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/clerk_token_answer.rs"]
mod clerk_answer_oracle;

/// The `clerk_socket_frame` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/clerk_socket_frame.rs"]
mod clerk_socket_oracle;

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
        // A copy seed's name says whether the library or the branch takes
        // the frame, so the oracle's copy rules run on frames of both kinds.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("copy_") {
            assert_eq!(
                parsed.as_ref().is_some_and(copy_frame_is_taken),
                !name.ends_with("_refused"),
                "{name}"
            );
        }
    }
    // The seeds include frames the parser accepts, so the round trip runs.
    assert!(accepted > 0, "no corpus frame parsed");
}

/// True when `frame` is a `copy.grant` or `copy.granted` its receiver
/// would act on.
fn copy_frame_is_taken(frame: &bookclerk_node_proto::Frame) -> bool {
    use bookclerk_node_proto::{copy_grant_problem, copy_granted_problem, Body, CopyGrantedFrame};
    match &frame.body {
        Body::CopyGrant {
            store,
            marketplace,
            product_id,
        } => copy_grant_problem(store, marketplace, product_id).is_none(),
        Body::CopyGranted {
            copy_id,
            sealed_token,
            expires_in_secs,
            copy,
            files,
        } => copy_granted_problem(&CopyGrantedFrame {
            copy_id,
            sealed_token,
            expires_in_secs: *expires_in_secs,
            copy,
            files,
        })
        .is_none(),
        _ => false,
    }
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
            bundle_oracle::LIBRARY_ID,
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
    for path in corpus_files("clerk_authorization") {
        if clerk_authorization_oracle::check(&std::fs::read(&path).expect("read")) {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    // The seeds reach both sides.
    assert!(accepted > 0, "no clerk_authorization seed was accepted");
    assert!(refused > 0, "every clerk_authorization seed was accepted");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_client_authorize_replays() {
    let (mut accepted, mut refused) = (0, 0);
    for path in corpus_files("clerk_authorize") {
        let data = std::fs::read(&path).expect("read");
        let ok = clerk_authorize_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say which side they are on in their name.
        assert_eq!(ok, name.starts_with("accepted"), "seed {name}");
        if ok {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    assert!(accepted > 0, "no clerk_authorize seed was accepted");
    assert!(refused > 0, "every clerk_authorize seed was accepted");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_clerk_token_request_replays() {
    let (mut accepted, mut refused) = (0, 0);
    for path in corpus_files("clerk_token_request") {
        let data = std::fs::read(&path).expect("read");
        let ok = clerk_token_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say which side they are on in their name.
        assert_eq!(ok, name.starts_with("accepted"), "seed {name}");
        if ok {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    assert!(accepted > 0, "no clerk_token_request seed was accepted");
    assert!(refused > 0, "every clerk_token_request seed was accepted");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_clerk_token_answer_replays() {
    let (mut accepted, mut refused) = (0, 0);
    for path in corpus_files("clerk_token_answer") {
        let data = std::fs::read(&path).expect("read");
        let ok = clerk_answer_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say which side they are on in their name.
        assert_eq!(ok, name.starts_with("accepted"), "seed {name}");
        if ok {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    assert!(accepted > 0, "no clerk_token_answer seed was accepted");
    assert!(refused > 0, "every clerk_token_answer seed was accepted");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_clerk_socket_frame_replays() {
    let (mut accepted, mut refused) = (0, 0);
    for path in corpus_files("clerk_socket_frame") {
        let data = std::fs::read(&path).expect("read");
        let ok = clerk_socket_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say which side they are on in their name.
        assert_eq!(ok, name.starts_with("accepted"), "seed {name}");
        if ok {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    assert!(accepted > 0, "no clerk_socket_frame seed was accepted");
    assert!(refused > 0, "every clerk_socket_frame seed was accepted");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_copy_range_replays() {
    let (mut parts, mut others) = (0, 0);
    for path in corpus_files("copy_range") {
        let data = std::fs::read(&path).expect("read");
        let part = copy_range_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say what they are read as in their name.
        assert_eq!(part, name.starts_with("part"), "seed {name}");
        if part {
            parts += 1;
        } else {
            others += 1;
        }
    }
    // The seeds reach both sides.
    assert!(parts > 0, "no copy_range seed named a part");
    assert!(others > 0, "every copy_range seed named a part");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_copy_answer_replays() {
    use copy_answer_oracle::Read;

    let (mut bodies, mut statuses, mut refused) = (0, 0, 0);
    for path in corpus_files("copy_answer") {
        let data = std::fs::read(&path).expect("read");
        let read = copy_answer_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say what they are read as in their name.
        let named = if name.starts_with("body") {
            Read::Body
        } else if name.starts_with("status") {
            Read::Status
        } else {
            Read::Refused
        };
        assert_eq!(read, named, "seed {name}");
        match read {
            Read::Body => bodies += 1,
            Read::Status => statuses += 1,
            Read::Refused => refused += 1,
        }
    }
    // The seeds reach every answer.
    assert!(bodies > 0, "no copy_answer seed was read as a body");
    assert!(statuses > 0, "no copy_answer seed was read as a status");
    assert!(refused > 0, "no copy_answer seed was refused");
}

#[test]
// A missing or unreadable seed is the test failing.
#[allow(clippy::expect_used)]
fn fuzz_corpus_upload_part_replays() {
    use upload_part_oracle::Read;

    let (mut parts, mut statuses, mut others) = (0, 0, 0);
    for path in corpus_files("upload_part") {
        let data = std::fs::read(&path).expect("read");
        let read = upload_part_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Seeds say what they are read as in their name.
        let named = if name.starts_with("part") {
            Read::Part
        } else if name.starts_with("status") {
            Read::Status
        } else {
            Read::Refused
        };
        assert_eq!(read, named, "seed {name}");
        match read {
            Read::Part => parts += 1,
            Read::Status => statuses += 1,
            Read::Refused => others += 1,
        }
    }
    // The seeds reach every answer.
    assert!(parts > 0, "no upload_part seed named a part");
    assert!(statuses > 0, "no upload_part seed named a status query");
    assert!(others > 0, "no upload_part seed named a refusal");
}
