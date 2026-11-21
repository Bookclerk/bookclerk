//! Shared oracle for the `player_request` fuzz target and the
//! `bookclerk-player-api` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is what a player app sends the host's `/player` API
//! before and after it holds a card: sign-in bodies, card tokens, list
//! and detail queries, ids in paths, listening bodies and the browser
//! sign-in's queries. An input's first byte picks the part under test;
//! each check holds the parser to a rule this file states on its own:
//!
//! - `l`: `POST /player/login`. Accepted only when the rest is one JSON
//!   object with string `username` and `password`; the username comes
//!   back trimmed, non-empty and at most 256 bytes, the password
//!   unchanged and at most 256 bytes, and neither shows in `Debug`.
//! - `t`: a card token (`Authorization: Bearer` or `?token=`). Accepted
//!   exactly when it is 43 bytes of URL-safe Base64 letters.
//! - `q` and `d`: a list or detail query. Accepted ones are at most 4096
//!   bytes, with a page size of at most 1000 and a search term of at most
//!   256 bytes with no control character.
//! - `f`: an author, series or narrator id. An accepted id names a key of
//!   1 to 256 bytes with no control character, and writing the key back
//!   gives the same id.
//! - `i`: a file's `ino`. Accepted exactly when it is a decimal number
//!   below 512 with no sign or leading zero; writing it back gives the
//!   same text.
//! - `s`, `p`, `a`, `b` and `k`: a session sync, a progress change, a
//!   batch of progress changes, a bookmark body and a bookmark time in a
//!   path. Every length an accepted one yields is at most sixty days of
//!   milliseconds, and a bookmark title at most 200 bytes.
//! - `g`: `POST /player/api/items/batch/get`. Every id an accepted body
//!   yields has the shape of an entity id, and there are at most 100.
//! - `o` and `m`: the browser sign-in's start and its way back to the
//!   app. An accepted one names one of the apps the library returns to,
//!   and a start carries a 43-byte S256 challenge.
//! - `w`: a typed player password. An accepted one normalizes to 20
//!   symbols of the password alphabet, which format back to the same
//!   password.
//!
//! Any other first byte, and an empty input, is outside the boundary.

use bookclerk_player_api::ids::{facet_id, file_ino, is_entity_id, parse_facet_id, parse_file_ino};
use bookclerk_player_api::oauth::{
    format_player_password, normalize_player_password, parse_mobile_redirect, parse_openid_start,
    PASSWORD_ALPHABET, PASSWORD_SYMBOLS, PLAYER_APPS,
};
use bookclerk_player_api::request::{
    parse_batch_get, parse_bookmark_request, parse_bookmark_time, parse_card_token,
    parse_detail_query, parse_list_query, parse_login_request, parse_progress_batch,
    parse_progress_patch, parse_sync_request, ProgressPatch,
};
use bookclerk_player_api::time::MAX_MS;

/// Largest input worth checking: past every documented limit but the
/// batch bodies', so the size rules are reached.
pub const MAX_INPUT_BYTES: usize = 20 * 1024;

/// True for a URL-safe Base64 letter.
fn base64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Checks one progress change.
fn check_patch(patch: &ProgressPatch) {
    assert!(patch.position_ms.is_none_or(|ms| ms <= MAX_MS));
    assert!(patch.duration_ms.is_none_or(|ms| ms <= MAX_MS));
}

/// Checks one input against the rule its first byte picks.
///
/// # Panics
///
/// When a parser breaks its rule.
#[allow(clippy::expect_used)] // The oracle re-reads input the parser accepted.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&part, rest)) = data.split_first() else {
        return;
    };
    let text = std::str::from_utf8(rest).ok();
    match part {
        b'l' => {
            if let Ok(login) = parse_login_request(rest) {
                let value: serde_json::Value = serde_json::from_slice(rest).expect("accepted JSON");
                let user = value["username"].as_str().expect("username string");
                let password = value["password"].as_str().expect("password string");
                assert_eq!(login.username, user.trim());
                assert!(!login.username.is_empty() && login.username.len() <= 256);
                assert_eq!(login.password, password);
                assert!(login.password.len() <= 256);
                // Long enough not to match the struct's own field names.
                if login.password.len() >= 16 && !login.username.contains(&login.password) {
                    assert!(!format!("{login:?}").contains(&login.password));
                }
            }
        }
        b't' => {
            let Some(text) = text else { return };
            let shaped = text.len() == 43 && text.bytes().all(base64url);
            assert_eq!(parse_card_token(text).is_some(), shaped, "{text:?}");
        }
        b'q' => {
            let Some(text) = text else { return };
            if let Ok(query) = parse_list_query(text) {
                assert!(text.len() <= 4096 && query.limit <= 1000);
                assert!(query
                    .search
                    .is_none_or(|q| q.len() <= 256 && !q.chars().any(char::is_control)));
            }
        }
        b'd' => {
            let Some(text) = text else { return };
            if let Ok(query) = parse_detail_query(text) {
                assert!(text.len() <= 4096 && query.limit.is_none_or(|l| l <= 1000));
                assert!(query
                    .search
                    .is_none_or(|q| q.len() <= 256 && !q.chars().any(char::is_control)));
            }
        }
        b'f' => {
            let Some(text) = text else { return };
            if let Some(key) = parse_facet_id(text) {
                assert!(!key.is_empty() && key.len() <= 256);
                assert!(!key.chars().any(char::is_control));
                assert_eq!(facet_id(&key), text);
            }
        }
        b'i' => {
            let Some(text) = text else { return };
            let canonical = !text.is_empty()
                && text.bytes().all(|b| b.is_ascii_digit())
                && (text == "0" || !text.starts_with('0'))
                && text.len() <= 3
                && text.parse::<u32>().is_ok_and(|n| n < 512);
            match parse_file_ino(text) {
                Some(index) => {
                    assert!(canonical);
                    assert_eq!(file_ino(index), text);
                }
                None => assert!(!canonical, "{text:?}"),
            }
        }
        b's' => {
            if let Ok(sync) = parse_sync_request(rest) {
                assert!(sync.position_ms.is_none_or(|ms| ms <= MAX_MS));
                assert!(sync.listened_ms <= MAX_MS);
                assert!(sync.duration_ms.is_none_or(|ms| ms <= MAX_MS));
            }
        }
        b'p' => {
            if let Ok(patch) = parse_progress_patch(rest) {
                check_patch(&patch);
            }
        }
        b'a' => {
            if let Ok(batch) = parse_progress_batch(rest) {
                assert!(batch.len() <= 100);
                for (id, patch) in &batch {
                    assert!(is_entity_id(id));
                    check_patch(patch);
                }
            }
        }
        b'b' => {
            if let Ok(bookmark) = parse_bookmark_request(rest) {
                assert!(bookmark.position_ms <= MAX_MS);
                assert!(bookmark.title.len() <= 200);
            }
        }
        b'k' => {
            let Some(text) = text else { return };
            if let Some(ms) = parse_bookmark_time(text) {
                assert!(ms <= MAX_MS);
            }
        }
        b'g' => {
            if let Ok(ids) = parse_batch_get(rest) {
                assert!(ids.len() <= 100);
                assert!(ids.iter().all(|id| is_entity_id(id)));
            }
        }
        b'o' => {
            let Some(text) = text else { return };
            if let Ok(start) = parse_openid_start(text) {
                assert!(PLAYER_APPS
                    .iter()
                    .any(|(uri, _)| *uri == start.redirect_uri));
                assert!(start.code_challenge.len() == 43);
                assert!(start.code_challenge.bytes().all(base64url));
            }
        }
        b'm' => {
            let Some(text) = text else { return };
            if let Ok(back) = parse_mobile_redirect(text) {
                assert!(PLAYER_APPS.iter().any(|(uri, _)| *uri == back.app_redirect));
            }
        }
        b'w' => {
            let Some(text) = text else { return };
            if let Some(normalized) = normalize_player_password(text) {
                assert_eq!(normalized.len(), PASSWORD_SYMBOLS);
                assert!(normalized.bytes().all(|b| PASSWORD_ALPHABET.contains(&b)));
                let mut symbols = [0u8; PASSWORD_SYMBOLS];
                for (slot, b) in symbols.iter_mut().zip(normalized.bytes()) {
                    *slot = u8::try_from(
                        PASSWORD_ALPHABET
                            .iter()
                            .position(|a| *a == b)
                            .expect("in the alphabet"),
                    )
                    .expect("small index");
                }
                let formatted = format_player_password(&symbols);
                assert_eq!(normalize_player_password(&formatted), Some(normalized));
            }
        }
        _ => {}
    }
}
