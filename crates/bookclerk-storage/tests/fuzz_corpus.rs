//! Replays the checked-in `s3_event`, `r2_event` and `b2_reply` fuzz
//! corpora in PR CI (no libFuzzer required), so a notification message,
//! queue reply or B2 reply that once broke a parser stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing.
#![allow(clippy::missing_panics_doc, clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use bookclerk_storage::r2_events::{parse_pull_reply, parse_r2_event, R2EventScope};
use bookclerk_storage::s3_events::{parse_event_message, EventParseError, EventScope};

#[path = "../../../fuzz/oracles/s3_event.rs"]
mod s3_event_oracle;

#[path = "../../../fuzz/oracles/r2_event.rs"]
mod r2_event_oracle;

#[path = "../../../fuzz/oracles/b2_reply.rs"]
mod b2_reply_oracle;

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

/// Reads one `r2_event` seed: its event changes' keys (even first byte) or
/// the lease ids of its pull reply (odd first byte), or the error text.
fn read_r2_seed(name: &str) -> Result<Vec<String>, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/r2_event")
        .join(name);
    let data = std::fs::read(&path).expect("read seed");
    let (&selector, rest) = data.split_first().expect("non-empty seed");
    let scope = R2EventScope {
        account: "3f4b7e3dcab231cbfdaa90a6a28bd548",
        bucket: "books",
        prefix: if selector & 2 == 0 { "" } else { "library/" },
    };
    if selector % 2 == 0 {
        parse_r2_event(rest, scope)
            .map(|events| events.into_iter().map(|event| event.change.key).collect())
            .map_err(|err| err.to_string())
    } else {
        parse_pull_reply(rest)
            .map(|messages| messages.into_iter().map(|m| m.lease_id).collect())
            .map_err(|err| err.to_string())
    }
}

#[test]
fn fuzz_corpus_r2_event_holds_the_oracle() {
    let mut accepted = 0;
    for path in corpus_files("r2_event") {
        let data = std::fs::read(&path).expect("read");
        r2_event_oracle::check(&data);
        let name = path.file_name().and_then(|n| n.to_str()).expect("name");
        if read_r2_seed(name).is_ok_and(|found| !found.is_empty()) {
            accepted += 1;
        }
    }
    assert!(
        accepted >= 5,
        "the seeds must reach the oracle's comparison, {accepted} did"
    );
}

#[test]
fn named_r2_seeds_keep_their_meaning() {
    assert_eq!(read_r2_seed("put_object"), Ok(vec!["a b.m4b".to_string()]));
    assert_eq!(read_r2_seed("other_account"), Ok(Vec::new()));
    assert_eq!(read_r2_seed("outside_prefix_long_key"), Ok(Vec::new()));
    assert!(read_r2_seed("bad_key_nul").is_err());
    assert!(read_r2_seed("event_as_array").is_err());
    assert_eq!(
        read_r2_seed("pull_base64_and_text"),
        Ok(vec!["l1".to_string(), "l2".to_string()])
    );
    assert!(read_r2_seed("pull_refused").is_err());
    assert!(read_r2_seed("pull_as_array").is_err());
}

#[test]
fn fuzz_corpus_b2_reply_holds_the_oracle() {
    for path in corpus_files("b2_reply") {
        let data = std::fs::read(&path).expect("read");
        b2_reply_oracle::check(&data);
    }
}

/// Whether the reader byte 0 picks accepts one `b2_reply` seed.
fn b2_seed_reads(name: &str) -> bool {
    use bookclerk_storage::b2_api;
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/b2_reply")
        .join(name);
    let data = std::fs::read(&path).expect("read seed");
    let (&selector, rest) = data.split_first().expect("non-empty seed");
    match selector % 9 {
        0 => b2_api::parse_authorization(rest).is_ok(),
        1 => b2_api::parse_buckets(rest).is_ok(),
        2 => b2_api::parse_upload_target(rest).is_ok(),
        3 => b2_api::parse_file_version(rest).is_ok(),
        4 => b2_api::parse_names_page(rest).is_ok(),
        5 => b2_api::parse_versions_page(rest).is_ok(),
        6 => b2_api::parse_part(rest).is_ok(),
        7 => !b2_api::parse_api_error(500, rest).code.is_empty(),
        _ => {
            let text = std::str::from_utf8(rest).expect("text seed");
            let lines: Vec<(&str, &[u8])> = text
                .lines()
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim(), value.trim().as_bytes()))
                .collect();
            b2_api::parse_download_headers(lines).is_ok()
        }
    }
}

#[test]
fn named_b2_error_seeds_read_as_expected() {
    use bookclerk_storage::b2_api;
    for (name, status, code, message_empty) in [
        ("error_html", 503, "unknown", true),
        ("error_expired", 401, "expired_auth_token", false),
        ("error_control", 400, "unknown", false),
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus/b2_reply")
            .join(name);
        let data = std::fs::read(&path).expect("read seed");
        // The same framing as the shared oracle: the selector byte, then
        // the status as two big-endian bytes, then the body.
        let (&selector, rest) = data.split_first().expect("non-empty seed");
        assert_eq!(selector % 9, 7, "{name} is an error seed");
        let (head, body) = rest.split_at(2);
        let err = b2_api::parse_api_error(u16::from_be_bytes([head[0], head[1]]), body);
        assert_eq!(err.status, status, "{name}");
        assert_eq!(err.code, code, "{name}");
        assert_eq!(err.message.is_empty(), message_empty, "{name}");
    }
}

#[test]
fn named_b2_seeds_keep_their_meaning() {
    for accepted in [
        "authorize_restricted",
        "authorize_unrestricted",
        "buckets",
        "buckets_dotted_name",
        "upload_target",
        "file_upload",
        "file_large_sha1",
        "file_start",
        "file_folder",
        "names_page",
        "names_page_last",
        "versions_page",
        "part",
        "error_html",
        "download_whole",
        "download_range",
    ] {
        assert!(b2_seed_reads(accepted), "{accepted} should read");
    }
    for refused in [
        "authorize_http_url",
        "authorize_not_utf8_in_skipped_field",
        "authorize_deleted_bucket",
        "authorize_positional_array",
        "buckets_positional_array",
        "authorize_positional_storage_api",
        "names_page_positional_file",
        "versions_page_positional_file",
        "versions_page_old_first",
        "versions_page_interleaved",
        "file_no_length",
        "buckets_bad_name",
        "buckets_ipv4_name",
        "buckets_double_dot",
        "buckets_edge_dot",
        "upload_target_userinfo",
        "file_backslash",
        "file_unknown_action",
        "names_page_backwards",
        "versions_half_continuation",
        "part_zero",
        "part_too_many",
        "download_range_mismatch",
        "download_no_id",
        "download_repeated",
    ] {
        assert!(!b2_seed_reads(refused), "{refused} should be refused");
    }
}
