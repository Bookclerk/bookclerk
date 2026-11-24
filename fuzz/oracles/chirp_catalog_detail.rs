//! Oracle for the `chirp_catalog_detail` fuzz target and its replay test:
//! Chirp's answer to a title detail query, which the `chirp` guest turns
//! into a catalog record with its audio sample
//! (`bookclerk-plugin-source-chirp::audiobook_detail_record`).
//!
//! Invariants, checked against the answer read as plain JSON:
//!
//! - an answer whose `data.audiobook` is missing or null names no record;
//! - a record keeps the audiobook's `id` as its product id and has a
//!   non-blank title;
//! - a record has a sample only when `sampleUrl` is a string, and then it
//!   is that string trimmed: an absolute `https` link with a host and no
//!   whitespace or control characters; such a string is always kept;
//! - reading the same answer twice gives the same record.

use bookclerk_plugin_source_chirp::audiobook_detail_record;
use serde_json::Value;

/// Whether `raw`, trimmed, is a sample link a record may carry.
fn usable_sample(raw: &str) -> bool {
    let raw = raw.trim();
    let Some(rest) = raw.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty() && !raw.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Checks every invariant on one input; the input is JSON text, else it is
/// skipped (the network path parses JSON first).
///
/// # Panics
///
/// When an invariant does not hold.
pub fn check(data: &[u8]) {
    let Ok(answer) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let record = audiobook_detail_record(&answer);
    let again = audiobook_detail_record(&answer);
    assert_eq!(
        format!("{record:?}"),
        format!("{again:?}"),
        "detail not deterministic"
    );
    let book = answer.pointer("/data/audiobook").filter(|b| !b.is_null());
    let Some(book) = book else {
        assert!(
            matches!(record, Ok(None)),
            "a record from an answer naming no audiobook"
        );
        return;
    };
    let Ok(Some(hit)) = record else {
        assert!(
            book.get("id").and_then(Value::as_str).is_none() || record.is_err(),
            "an audiobook with an id read as no record"
        );
        return;
    };
    assert_eq!(
        Some(hit.product_id.as_str()),
        book.get("id").and_then(Value::as_str),
        "product id differs from the audiobook's id"
    );
    assert!(!hit.title.is_empty(), "blank title");
    let raw = book.get("sampleUrl").and_then(Value::as_str);
    match (&hit.sample_url, raw) {
        (Some(sample), Some(raw)) => {
            assert_eq!(sample, raw.trim(), "sample differs from sampleUrl");
            assert!(usable_sample(sample), "unusable sample {sample:?}");
        }
        (Some(sample), None) => panic!("a sample without sampleUrl: {sample:?}"),
        (None, Some(raw)) => assert!(!usable_sample(raw), "usable sampleUrl dropped: {raw:?}"),
        (None, None) => {}
    }
}
