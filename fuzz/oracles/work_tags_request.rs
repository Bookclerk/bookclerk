//! Shared oracle for the `work_tags_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of `PUT /api/library/works/{id}/tags`, which a
//! library's operator, an Owner or an Administrator sends to set a work's
//! library tags ([`parse_work_tags_request`]). The invariant, checked
//! against this file's own reading of the bytes as an untyped JSON value
//! rather than the typed body: a body is accepted exactly when it is one
//! JSON object with no repeated keys and exactly the key `tags`, an array
//! of strings, each at most 256 bytes once trimmed, with at most 32
//! distinct non-blank values ignoring case. An accepted body yields those
//! values trimmed, in the order sent, first spelling kept; a refused one is
//! `Invalid`, never another error.

use bookclerk_library::{parse_work_tags_request, LibraryError};
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking: 32 tags of 256 bytes with room for
/// escapes and whitespace.
pub const MAX_INPUT_BYTES: usize = 32 * 1024;

/// Most distinct tags the route documents.
const DOCUMENTED_MAX_TAGS: usize = 32;

/// Longest trimmed tag the route documents, in bytes.
const DOCUMENTED_MAX_TAG_BYTES: usize = 256;

/// What the oracle expects from one body.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused as `Invalid`.
    Refused,
    /// Accepted with these tags.
    Accepted(Vec<String>),
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
    let got = parse_work_tags_request(data);
    match expected(data) {
        Expected::Refused => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Expected::Accepted(tags) => {
            let request = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(request.tags(), tags.as_slice());
            true
        }
    }
}

/// Reads the body as an untyped JSON value and applies the documented
/// rules to it.
fn expected(data: &[u8]) -> Expected {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(data) else {
        return Expected::Refused;
    };
    // An untyped object keeps only the last of a repeated key; the body
    // refuses repeats, so count the keys as written.
    if has_repeated_top_level_key(data) || fields.len() != 1 {
        return Expected::Refused;
    }
    let Some(Value::Array(items)) = fields.get("tags") else {
        return Expected::Refused;
    };
    let mut out: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for item in items {
        let Value::String(tag) = item else {
            return Expected::Refused;
        };
        let tag = tag.trim();
        if tag.is_empty() {
            continue;
        }
        if tag.len() > DOCUMENTED_MAX_TAG_BYTES {
            return Expected::Refused;
        }
        let key = tag.to_lowercase();
        if !keys.contains(&key) {
            keys.push(key);
            out.push(tag.to_string());
        }
    }
    if out.len() > DOCUMENTED_MAX_TAGS {
        return Expected::Refused;
    }
    Expected::Accepted(out)
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
