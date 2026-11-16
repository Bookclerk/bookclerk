//! Shared oracle for the `work_split_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of `POST /api/library/works/{id}/split`, which
//! a library's operator, an Owner or an Administrator sends to move some of
//! a work's expressions to a new work ([`parse_work_split_request`]). The
//! invariant, checked against this file's own reading of the bytes as an
//! untyped JSON value rather than the typed body: a body is accepted
//! exactly when it is one JSON object with no repeated keys and exactly the
//! key `expression_ids`, an array of strings each 1 to 64 bytes long, with
//! 1 to 12 distinct values (a repeated id counts once). An accepted body
//! yields those distinct ids in sorted order; a refused one is `Invalid`,
//! never another error.

use std::collections::BTreeSet;

use bookclerk_library::{parse_work_split_request, LibraryError};
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking; a real body is under 1 KiB.
pub const MAX_INPUT_BYTES: usize = 4096;

/// Most distinct ids the route documents.
const DOCUMENTED_MAX_IDS: usize = 12;

/// Longest id the route documents, in bytes.
const DOCUMENTED_MAX_ID_BYTES: usize = 64;

/// What the oracle expects from one body.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused as `Invalid`.
    Refused,
    /// Accepted with these distinct ids, sorted.
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
    let got = parse_work_split_request(data);
    match expected(data) {
        Expected::Refused => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Expected::Accepted(ids) => {
            let request = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(request.expression_ids(), ids.as_slice());
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
    if has_repeated_top_level_key(data) {
        return Expected::Refused;
    }
    if fields.len() != 1 {
        return Expected::Refused;
    }
    let Some(Value::Array(items)) = fields.get("expression_ids") else {
        return Expected::Refused;
    };
    let mut distinct = BTreeSet::new();
    for item in items {
        let Value::String(id) = item else {
            return Expected::Refused;
        };
        if id.is_empty() || id.len() > DOCUMENTED_MAX_ID_BYTES {
            return Expected::Refused;
        }
        distinct.insert(id.clone());
    }
    if distinct.is_empty() || distinct.len() > DOCUMENTED_MAX_IDS {
        return Expected::Refused;
    }
    Expected::Accepted(distinct.into_iter().collect())
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
