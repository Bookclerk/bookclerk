//! Shared oracle for the `scan_page` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one `ContentSource.scan` reply page a storefront guest could
//! return, framed with the pass it answers: byte 0 is the length `n` of the
//! cursor the host sent, the next `n` bytes are that cursor (lossy UTF-8),
//! the next byte is the length `m` of the pass's account id, the next `m`
//! bytes are that id, and the rest is the unpacked `ScanPage` message. The
//! invariants:
//!
//! - decoding and validation never panic;
//! - [`validate_scan_page`] accepts the page exactly when the independent
//!   recount below finds every bound and pass rule kept, including that any
//!   `refreshedCredentials` the guest handed back is at most
//!   `maxRefreshedCredentialBytes` and reads as one JSON object (read here
//!   as a typed map rather than an untyped value), against a fixed
//!   declaration of audio books and podcasts (every title names all three
//!   kinds, its work kind is book or podcast and its carrier is audio);
//! - an accepted page re-encodes, decodes back equal, and is accepted again.

use bookclerk_plugin_abi::{
    decode_scan_page_bytes, encode_scan_page_bytes, validate_scan_page, ExpressionKind, MediaKind,
    MediaSupport, ScanPage, WorkKind, MAX_REFRESHED_CREDENTIAL_BYTES, MAX_SCAN_CURSOR_BYTES,
    MAX_SCAN_PAGE_BOOKS,
};

/// Largest input worth decoding; real pages at every cap are far smaller.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// The storefront declaration every input is checked against.
fn declaration() -> MediaSupport {
    MediaSupport {
        work_kinds: vec![WorkKind::Book, WorkKind::Podcast],
        lists: vec![MediaKind::Audio],
        fetches: vec![MediaKind::Audio],
    }
}

/// Splits one length-prefixed field off `data`.
fn take(data: &[u8]) -> Option<(String, &[u8])> {
    let (&len, rest) = data.split_first()?;
    let len = usize::from(len);
    if rest.len() < len {
        return None;
    }
    let (field, rest) = rest.split_at(len);
    Some((String::from_utf8_lossy(field).into_owned(), rest))
}

/// The page rules restated directly: true when the page keeps them all.
fn keeps_every_rule(page: &ScanPage, sent: &str, account: &str) -> bool {
    let cap = MAX_SCAN_PAGE_BOOKS as usize;
    let cursor_cap = MAX_SCAN_CURSOR_BYTES as usize;
    page.books.len() <= cap
        && page.removed.len() <= cap
        && page.next_cursor.len() <= cursor_cap
        && page.sync_token.len() <= cursor_cap
        && (page.next_cursor.is_empty() || page.next_cursor != sent)
        && page.books.iter().all(|b| {
            (b.account_id.is_empty() || b.account_id == account)
                && !b.product_id.trim().is_empty()
                && matches!(b.work_kind, WorkKind::Book | WorkKind::Podcast)
                && b.expression_kind != ExpressionKind::Unspecified
                && b.media_kind == MediaKind::Audio
        })
        && page.removed.iter().all(|id| !id.trim().is_empty())
        && page.refreshed_credentials.as_deref().is_none_or(|blob| {
            blob.is_empty()
                || (u32::try_from(blob.len()).is_ok_and(|n| n <= MAX_REFRESHED_CREDENTIAL_BYTES)
                    && serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(blob)
                        .is_ok())
        })
}

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((sent, rest)) = take(data) else {
        return;
    };
    let Some((account, message)) = take(rest) else {
        return;
    };
    let Ok(page) = decode_scan_page_bytes(message) else {
        return;
    };
    let media = declaration();
    let verdict = validate_scan_page(&page, &sent, &account, &media);
    assert_eq!(
        verdict.is_ok(),
        keeps_every_rule(&page, &sent, &account),
        "validation disagrees with the oracle: {verdict:?}"
    );
    if verdict.is_ok() {
        let bytes = encode_scan_page_bytes(&page)
            .unwrap_or_else(|err| panic!("an accepted page must encode: {err}"));
        let again = decode_scan_page_bytes(&bytes)
            .unwrap_or_else(|err| panic!("an encoded page must decode: {err}"));
        assert_eq!(again, page, "an accepted page round-trips");
        assert!(validate_scan_page(&again, &sent, &account, &media).is_ok());
    }
}
