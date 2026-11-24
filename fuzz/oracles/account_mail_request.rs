//! Shared oracle for the `account_mail_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is two account-mail bodies: an Administrator's choice of
//! notifier (`PUT /api/notifications/account-mail`,
//! [`parse_account_mail_settings`]) and the token anyone holding a
//! verification link sends back (`POST /api/auth/verify-email`,
//! [`parse_verify_email_request`]). The input's first byte picks the body
//! (even: settings, odd: verify); the rest is the body.
//!
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value: a body is accepted exactly when it is one JSON
//! object with no repeated keys whose only key is present and well formed.
//! For settings, `plugin_instance_id` is `null` or 1 to 128 printable ASCII
//! bytes with no space; for verify, `token` is exactly 64 lowercase hex
//! characters. The parsed value equals the one written. A refused body is
//! `Invalid`, never another error.

use bookclerk_library::notifications::{parse_account_mail_settings, parse_verify_email_request};
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking; a real body is under 200 bytes.
pub const MAX_INPUT_BYTES: usize = 1024;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A selector byte, then the request body bytes.
///
/// # Returns
///
/// True when the parser accepted the body.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let Some((selector, body)) = data.split_first() else {
        return false;
    };
    let shown = String::from_utf8_lossy(body);
    if selector % 2 == 0 {
        // The messages name the input and the verdict, never the parsed
        // value, so a failure prints only what the fuzzer fed in.
        let got = parse_account_mail_settings(body);
        match expected_settings(body) {
            None => {
                assert!(
                    matches!(got, Err(LibraryError::Invalid(_))),
                    "{shown:?}: the parser took a body the oracle refuses"
                );
                false
            }
            Some(want) => {
                assert!(
                    got.is_ok_and(|id| id == want),
                    "{shown:?}: the parser and the oracle disagree"
                );
                true
            }
        }
    } else {
        let got = parse_verify_email_request(body);
        match expected_token(body) {
            None => {
                assert!(
                    matches!(got, Err(LibraryError::Invalid(_))),
                    "{shown:?}: the parser took a body the oracle refuses"
                );
                false
            }
            Some(want) => {
                assert!(
                    got.is_ok_and(|token| token.expose() == want),
                    "{shown:?}: the parser and the oracle disagree"
                );
                true
            }
        }
    }
}

/// The one value of an object whose only key is `key`; `None` when the
/// body is not such an object.
fn only_value(body: &[u8], key: &str) -> Option<Value> {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    if fields.len() != 1 || has_repeated_top_level_key(body) {
        return None;
    }
    fields.get(key).cloned()
}

/// What the settings body must parse to: `Some(None)` for `null`.
fn expected_settings(body: &[u8]) -> Option<Option<String>> {
    match only_value(body, "plugin_instance_id")? {
        Value::Null => Some(None),
        Value::String(id)
            if (1..=128).contains(&id.len()) && id.bytes().all(|b| (0x21..=0x7e).contains(&b)) =>
        {
            Some(Some(id))
        }
        _ => None,
    }
}

/// What the verify body's token must be.
fn expected_token(body: &[u8]) -> Option<String> {
    match only_value(body, "token")? {
        Value::String(token)
            if token.len() == 64 && token.chars().all(|c| "0123456789abcdef".contains(c)) =>
        {
            Some(token)
        }
        _ => None,
    }
}

/// True when the top-level JSON object in `data` writes a key twice.
fn has_repeated_top_level_key(data: &[u8]) -> bool {
    /// Collects an object's keys in the order written.
    struct Keys;

    impl<'de> Visitor<'de> for Keys {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Vec<String>, A::Error> {
            let mut keys = Vec::new();
            while let Some((key, IgnoredAny)) = map.next_entry::<String, IgnoredAny>()? {
                keys.push(key);
            }
            Ok(keys)
        }
    }

    let mut de = serde_json::Deserializer::from_slice(data);
    let Ok(mut keys) = de.deserialize_map(Keys) else {
        return false;
    };
    let written = keys.len();
    keys.sort();
    keys.dedup();
    keys.len() != written
}
