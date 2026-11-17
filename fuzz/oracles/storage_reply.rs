//! Shared oracle for the `storage_reply` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one storage guest's reply to an intent operation. Byte 0
//! modulo 5 selects the reply: 0 a `moveMany` page, 1 a `deleteMany`
//! result, 2 a `headMany` result, 3 a `changes` page and 4 a hinted `list`
//! page; the rest is the Cap'n Proto message (unpacked). The invariants:
//!
//! - decoding and validation never panic;
//! - a decoded reply survives encode then decode unchanged;
//! - each validator accepts the reply, against a fixed request, exactly
//!   when the independent restatement below does.

use bookclerk_plugin_abi::{
    decode_storage_reply_bytes, encode_storage_reply_bytes, validate_changes_page,
    validate_head_many, validate_key_outcomes, validate_move_many_page, ChangesPage, KeyOutcome,
    KeyPair, MoveManyPage, MoveManyRequest, MoveManyTarget, MoveOptions, ObjectMetadata,
    StorageReply, StorageReplyKind, MAX_BATCH_KEYS, MAX_CHANGES_CURSOR_BYTES, MAX_CHANGES_PAGE,
    MAX_LIST_PAGE, MAX_SCALAR_BYTES,
};

/// Largest input worth decoding; a real reply page is well under 1 MiB.
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Keys the host "sent" for the batch operations.
const SENT: [&str; 2] = ["a/1.m4b", "a/2.m4b"];

/// Source prefix of the fixed prefix move.
const PREFIX: &str = "a/";

/// Cursor the host "sent" with the fixed prefix move and change-feed read.
const SENT_CURSOR: &str = "c1";

/// The key rule restated: non-empty, no NUL, at most one scalar long.
fn key_ok(key: &str) -> bool {
    !key.is_empty() && !key.contains('\0') && key.len() <= MAX_SCALAR_BYTES as usize
}

/// The fixed sent keys as owned strings.
fn sent() -> Vec<String> {
    SENT.iter().map(|k| (*k).to_string()).collect()
}

/// Outcomes naming exactly the sent keys, in order.
fn outcomes_match(results: &[KeyOutcome]) -> bool {
    results.len() == SENT.len() && results.iter().zip(SENT).all(|(r, k)| r.key == k)
}

/// The prefix-page rules restated.
fn prefix_page_ok(page: &MoveManyPage) -> bool {
    page.results.len() <= MAX_BATCH_KEYS as usize
        && page
            .results
            .iter()
            .all(|r| r.key.starts_with(PREFIX) && key_ok(&r.key))
        && page
            .next_cursor
            .as_deref()
            .is_none_or(|c| c.len() <= MAX_CHANGES_CURSOR_BYTES as usize && c != SENT_CURSOR)
}

/// The change-feed rules restated.
fn changes_ok(page: &ChangesPage) -> bool {
    page.changes.len() <= MAX_CHANGES_PAGE as usize
        && !page.cursor.is_empty()
        && page.cursor.len() <= MAX_CHANGES_CURSOR_BYTES as usize
        && !(page.more && page.cursor == SENT_CURSOR)
        && page.changes.iter().all(|c| key_ok(&c.key))
}

/// The probe rules restated.
fn head_ok(objects: &[Option<ObjectMetadata>]) -> bool {
    objects.len() == SENT.len()
        && objects
            .iter()
            .zip(SENT)
            .all(|(m, k)| m.as_ref().is_none_or(|m| m.key == k))
}

/// Checks one fuzz input against the invariants above.
///
/// # Panics
///
/// Panics when an invariant does not hold.
// A failed expectation is the invariant breaking, which is what the fuzzer
// reports.
#[allow(clippy::expect_used)]
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, bytes)) = data.split_first() else {
        return;
    };
    let kind = match selector % 5 {
        0 => StorageReplyKind::MoveMany,
        1 => StorageReplyKind::DeleteMany,
        2 => StorageReplyKind::HeadMany,
        3 => StorageReplyKind::Changes,
        _ => StorageReplyKind::List,
    };
    let Ok(reply) = decode_storage_reply_bytes(kind, bytes) else {
        return;
    };
    let encoded = encode_storage_reply_bytes(&reply).expect("re-encode a decoded reply");
    let again = decode_storage_reply_bytes(kind, &encoded).expect("decode a re-encoded reply");
    assert_eq!(again, reply, "storage reply does not round-trip");

    match &reply {
        StorageReply::MoveMany(page) => {
            let pairs = MoveManyRequest {
                target: MoveManyTarget::Pairs(
                    SENT.iter()
                        .map(|k| KeyPair {
                            from: (*k).to_string(),
                            to: format!("b/{k}"),
                        })
                        .collect(),
                ),
                cursor: None,
                limit: 0,
                options: MoveOptions::default(),
            };
            assert_eq!(
                validate_move_many_page(&pairs, page).is_ok(),
                outcomes_match(&page.results) && page.next_cursor.is_none(),
                "pairs page verdict"
            );
            let prefix = MoveManyRequest {
                target: MoveManyTarget::Prefix(KeyPair {
                    from: PREFIX.into(),
                    to: "b/".into(),
                }),
                cursor: Some(SENT_CURSOR.into()),
                limit: 0,
                options: MoveOptions::default(),
            };
            assert_eq!(
                validate_move_many_page(&prefix, page).is_ok(),
                prefix_page_ok(page),
                "prefix page verdict"
            );
        }
        StorageReply::DeleteMany(results) => {
            assert_eq!(
                validate_key_outcomes("deleteMany", &sent(), results).is_ok(),
                outcomes_match(results),
                "deleteMany verdict"
            );
        }
        StorageReply::HeadMany(objects) => {
            assert_eq!(
                validate_head_many(&sent(), objects).is_ok(),
                head_ok(objects),
                "headMany verdict"
            );
        }
        StorageReply::Changes(page) => {
            assert_eq!(
                validate_changes_page(Some(SENT_CURSOR), page).is_ok(),
                changes_ok(page),
                "changes verdict"
            );
        }
        StorageReply::List(page) => {
            assert!(
                page.objects.len() <= MAX_LIST_PAGE as usize,
                "list page above its cap decoded"
            );
        }
    }
}
