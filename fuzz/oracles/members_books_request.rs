//! Shared oracle for the `members_books_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of `PUT /api/config/branches/members-books`,
//! which a library's operator or an Owner sends to turn "Acquire all of
//! this branch's members' books" on or off ([`parse_members_books_request`]).
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value rather than the typed body: a body is accepted
//! exactly when it is one JSON object with no repeated keys and exactly the
//! keys `node_id` (a string) and `acquire` (a boolean), where the trimmed
//! `node_id` is 1 to 58 lowercase ASCII letters, digits, `.`, `_` and `-`.
//! An accepted body yields that trimmed id and the boolean; a refused one is
//! `Invalid`, never another error.

use bookclerk_library::control_plane::parse_members_books_request;
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking; a real body is under 100 bytes.
pub const MAX_INPUT_BYTES: usize = 1024;

/// Longest node id the route documents (`node.<id>` within a 63-byte
/// queue name).
const DOCUMENTED_NODE_ID_MAX: usize = 58;

/// What the oracle expects from one body.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused as `Invalid`.
    Refused,
    /// Accepted with this trimmed node id and choice.
    Accepted(String, bool),
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The request body bytes.
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
    let got = parse_members_books_request(data);
    match expected(data) {
        Expected::Refused => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Expected::Accepted(node_id, acquire) => {
            let choice = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(choice.node_id, node_id);
            assert_eq!(choice.acquire, acquire);
            true
        }
    }
}

/// True for a documented node id, checked byte by byte.
fn node_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= DOCUMENTED_NODE_ID_MAX
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// Reads the body as an untyped JSON value and applies the documented
/// rules to it.
fn expected(data: &[u8]) -> Expected {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(data) else {
        return Expected::Refused;
    };
    // An untyped object keeps only the last of a repeated key; the body
    // refuses repeats, so count the keys as written.
    if has_repeated_top_level_key(data) {
        return Expected::Refused;
    }
    if fields
        .keys()
        .any(|key| key != "node_id" && key != "acquire")
    {
        return Expected::Refused;
    }
    let Some(Value::String(node_id)) = fields.get("node_id") else {
        return Expected::Refused;
    };
    let Some(Value::Bool(acquire)) = fields.get("acquire") else {
        return Expected::Refused;
    };
    let node_id = node_id.trim();
    if !node_id_ok(node_id) {
        return Expected::Refused;
    }
    Expected::Accepted(node_id.to_string(), *acquire)
}

/// True when the top-level JSON object in `data` writes a key twice.
/// `data` already parsed as an object.
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
