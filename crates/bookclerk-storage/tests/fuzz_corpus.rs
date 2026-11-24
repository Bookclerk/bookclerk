//! Replays the checked-in `s3_event`, `r2_event` and `cloudflare_api` fuzz
//! corpora in PR CI (no libFuzzer required), so a notification message,
//! Cloudflare API reply or queue reply that once broke a parser stays
//! fixed
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

#[path = "../../../fuzz/oracles/cloudflare_api.rs"]
mod cloudflare_api_oracle;

#[test]
fn fuzz_corpus_cloudflare_api_holds_the_oracle() {
    for path in corpus_files("cloudflare_api") {
        let data = std::fs::read(&path).expect("read");
        cloudflare_api_oracle::check(&data);
    }
}

/// Whether the reader byte 0 picks accepts one `cloudflare_api` seed.
fn cloudflare_seed_reads(name: &str) -> bool {
    use bookclerk_storage::cloudflare_api as api;
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/cloudflare_api")
        .join(name);
    let data = std::fs::read(&path).expect("read seed");
    let (&selector, rest) = data.split_first().expect("non-empty seed");
    match selector % 10 {
        0 => api::parse_envelope(rest).is_ok(),
        1 => api::parse_accounts(rest).is_ok(),
        2 => api::parse_bucket_created(rest).is_ok(),
        3 => api::find_permission_group(rest, api::BUCKET_ITEM_WRITE).is_ok_and(|g| g.is_some()),
        4 => api::parse_minted_token(rest).is_ok(),
        6 => api::find_d1_database(rest, "books").is_ok_and(|d| d.is_some()),
        7 => api::parse_d1_created(rest).is_ok(),
        8 => api::check_d1_database_name(std::str::from_utf8(rest).expect("utf-8")).is_ok(),
        9 => api::find_permission_group(rest, api::D1_WRITE).is_ok_and(|g| g.is_some()),
        _ => api::check_bucket_name(std::str::from_utf8(rest).expect("utf-8")).is_ok(),
    }
}

#[test]
fn cloudflare_api_seeds_mean_what_they_say() {
    for name in [
        "envelope_ok",
        "accounts_one",
        "accounts_two",
        "bucket_created",
        "bucket_owned",
        "groups_found",
        "token_minted",
        "bucket_name_ok",
        "d1_list_found",
        "d1_created",
        "d1_name_ok",
        "d1_group_found",
    ] {
        assert!(cloudflare_seed_reads(name), "{name} reads");
    }
    for name in [
        "envelope_refused",
        "envelope_array",
        "envelope_long_message",
        "accounts_bad_id",
        "bucket_forbidden",
        "groups_missing",
        "groups_bad_id",
        "token_bad_value",
        "token_no_id",
        "bucket_name_upper",
        "bucket_name_hyphen_end",
        "bucket_name_short",
        "d1_list_missing",
        "d1_list_bad_uuid",
        "d1_created_no_uuid",
        "d1_create_limit",
        "d1_name_upper",
        "d1_name_underscore_start",
    ] {
        assert!(!cloudflare_seed_reads(name), "{name} is refused");
    }
}
