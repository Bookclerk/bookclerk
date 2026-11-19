//! Replays the checked-in `s3_event` fuzz corpus in PR CI (no libFuzzer
//! required), so a notification message that once broke the parser stays
//! fixed (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing.
#![allow(clippy::missing_panics_doc, clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use bookclerk_storage::s3_events::{parse_event_message, EventParseError, EventScope};

#[path = "../../../fuzz/oracles/s3_event.rs"]
mod s3_event_oracle;

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized and
/// confined to that directory.
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
        .canonicalize()
        .expect("canonicalize corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty corpus for {target}");
    files
}

#[test]
fn fuzz_corpus_s3_event_holds_the_oracle() {
    let mut accepted = 0;
    for path in corpus_files("s3_event") {
        let data = std::fs::read(&path).expect("read");
        s3_event_oracle::check(&data);
        if let Some((&selector, body)) = data.split_first() {
            let prefix = if selector % 2 == 0 { "" } else { "library/" };
            let scope = EventScope {
                bucket: "books",
                prefix,
            };
            if parse_event_message(body, scope).is_ok_and(|events| !events.is_empty()) {
                accepted += 1;
            }
        }
    }
    assert!(
        accepted >= 5,
        "the seeds must reach the oracle's comparison, {accepted} did"
    );
}

/// Parses one seed with the scope its first byte selects.
fn parse_seed(name: &str) -> Result<Vec<String>, EventParseError> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/s3_event")
        .join(name);
    let data = std::fs::read(&path).expect("read seed");
    let (&selector, body) = data.split_first().expect("non-empty seed");
    let prefix = if selector % 2 == 0 { "" } else { "library/" };
    let scope = EventScope {
        bucket: "books",
        prefix,
    };
    parse_event_message(body, scope)
        .map(|events| events.into_iter().map(|event| event.change.key).collect())
}

#[test]
fn named_seeds_keep_their_meaning() {
    assert_eq!(
        parse_seed("sns_envelope"),
        Ok(vec!["big.m4b".to_string()]),
        "an SNS envelope is unwrapped"
    );
    assert_eq!(parse_seed("test_event"), Ok(Vec::new()));
    assert_eq!(parse_seed("bad_escape"), Err(EventParseError::BadKey));
}
