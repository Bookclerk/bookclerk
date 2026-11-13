//! Shared oracle for the `branch_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of `POST /api/branch-requests`, which any
//! signed-in user of a library sends to ask a branch for a book it listed
//! ([`parse_branch_request`]). The invariant, checked against this file's
//! own reading of the bytes as an untyped JSON value rather than the typed
//! body: a body is accepted exactly when it is one JSON object with no
//! repeated keys and no keys besides `work_id` (a string, required) and
//! `node_id` (absent, `null` or a string), where the trimmed `work_id` is 1
//! to 64 ASCII letters, digits and `-`, and a trimmed `node_id` is 1 to 58
//! lowercase ASCII letters, digits, `.`, `_` and `-`. An accepted body
//! yields those trimmed ids; a refused one is `Invalid`, never another
//! error.

use bookclerk_library::control_plane::parse_branch_request;
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking; a real body is under 200 bytes.
pub const MAX_INPUT_BYTES: usize = 1024;

/// Longest work id the route documents.
const DOCUMENTED_WORK_ID_MAX: usize = 64;

/// Longest node id the route documents (`node.<id>` within a 63-byte
/// queue name).
const DOCUMENTED_NODE_ID_MAX: usize = 58;

/// What the oracle expects from one body.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused as `Invalid`.
    Refused,
    /// Accepted with this trimmed work id and node id.
    Accepted(String, Option<String>),
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
    let got = parse_branch_request(data);
    match expected(data) {
        Expected::Refused => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Expected::Accepted(work_id, node_id) => {
            let ask = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(ask.work_id, work_id);
            assert_eq!(ask.node_id, node_id);
            true
        }
    }
}

/// True for a documented work id, checked byte by byte.
fn work_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= DOCUMENTED_WORK_ID_MAX
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
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
        .any(|key| key != "work_id" && key != "node_id")
    {
        return Expected::Refused;
    }
    let Some(Value::String(work_id)) = fields.get("work_id") else {
        return Expected::Refused;
    };
    let work_id = work_id.trim();
    if !work_id_ok(work_id) {
        return Expected::Refused;
    }
    let node_id = match fields.get("node_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(node)) => {
            let node = node.trim();
            if !node_id_ok(node) {
                return Expected::Refused;
            }
            Some(node.to_string())
        }
        Some(_) => return Expected::Refused,
    };
    Expected::Accepted(work_id.to_string(), node_id)
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
