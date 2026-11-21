//! Shared oracle for the `notify_cursor` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the `cursor` query parameter of
//! `GET /api/notifications/deliveries` and `GET /api/me/notifications/inbox`
//! ([`NotifyCursor::decode`]): a token the caller sends back, so any bytes.
//! The invariant, checked against this file's own base64url decoder and an
//! untyped reading of the JSON: a token is accepted exactly when it is at
//! most 512 bytes of unpadded base64url whose bytes are one JSON object
//! with exactly the keys `created_at` and `id`, each written once, each a
//! string of at most 128 bytes with no U+0000 (which SQL-v1 cannot bind).
//! An accepted token yields those strings and
//! its re-encoding decodes to the same cursor; a refused one is `Invalid`.

use bookclerk_library::{LibraryError, NotifyCursor};
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Longest token the route accepts, in bytes.
const MAX_TOKEN_BYTES: usize = 512;

/// Longest field the route accepts, in bytes.
const MAX_FIELD_BYTES: usize = 128;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The token bytes.
///
/// # Returns
///
/// True when the decoder accepted the token.
///
/// # Panics
///
/// When the decoder disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    let Ok(token) = std::str::from_utf8(data) else {
        // A query string reaches the decoder as text.
        return false;
    };
    let got = NotifyCursor::decode(token);
    match expected(token) {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{token:?}: {got:?}"
            );
            false
        }
        Some((created_at, id)) => {
            let cursor = got.unwrap_or_else(|err| panic!("{token:?} refused: {err:?}"));
            assert_eq!(cursor.created_at, created_at);
            assert_eq!(cursor.id, id);
            let again = NotifyCursor::decode(&cursor.encode())
                .unwrap_or_else(|err| panic!("re-encoded cursor refused: {err:?}"));
            assert_eq!(again, cursor);
            true
        }
    }
}

/// The documented reading of `token`.
fn expected(token: &str) -> Option<(String, String)> {
    if token.len() > MAX_TOKEN_BYTES {
        return None;
    }
    let bytes = base64url_decode(token)?;
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(&bytes) else {
        return None;
    };
    if written_keys(&bytes)?.len() != 2 || fields.len() != 2 {
        return None;
    }
    let field = |key: &str| match fields.get(key) {
        Some(Value::String(s)) if s.len() <= MAX_FIELD_BYTES && !s.contains('\0') => {
            Some(s.clone())
        }
        _ => None,
    };
    Some((field("created_at")?, field("id")?))
}

/// Decodes unpadded base64url (RFC 4648 section 5) the strict way: no
/// padding, no other characters, and zero bits in a final partial group.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let input = text.as_bytes();
    if input.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for chunk in input.chunks(4) {
        let mut acc = 0u32;
        for &c in chunk {
            acc = (acc << 6) | value(c)?;
        }
        match chunk.len() {
            4 => out.extend_from_slice(&acc.to_be_bytes()[1..]),
            3 => {
                if acc & 0b11 != 0 {
                    return None;
                }
                out.extend_from_slice(&(acc >> 2).to_be_bytes()[2..]);
            }
            2 => {
                if acc & 0b1111 != 0 {
                    return None;
                }
                out.extend_from_slice(&(acc >> 4).to_be_bytes()[3..]);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// The top-level object's keys as written (repeats kept), when `data` is
/// an object.
fn written_keys(data: &[u8]) -> Option<Vec<String>> {
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
    let keys = de.deserialize_map(Keys).ok()?;
    let mut distinct = keys.clone();
    distinct.sort();
    distinct.dedup();
    (distinct.len() == keys.len()).then_some(keys)
}
