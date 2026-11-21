//! Shared oracle for the `collections_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of `PUT /api/library/collections`, which a
//! library's operator, an Owner or an Administrator sends to define the
//! library's collections ([`parse_collections_request`]); the same reader
//! parses the stored `core.collections` document. The invariant, checked
//! against this file's own reading of the bytes as an untyped JSON value
//! rather than the typed body:
//!
//! - The body is one JSON object with no repeated keys and exactly the key
//!   `collections`, an array of at most 32 collections.
//! - Each collection is an object with no repeated keys and exactly the
//!   keys `name` (a string) and `prefixes` (an array of 1 to 8 strings).
//! - Each name, trimmed, is 1 to 64 bytes with no NUL, and no two names
//!   match ignoring case.
//! - Each prefix, trimmed, is non-empty, doesn't start with `/`, holds no
//!   NUL, and with a closing `/` added when missing is at most 256 bytes.
//! - The normalized collections come to at most 8 KiB as compact JSON.
//!
//! An accepted body yields the trimmed names and the normalized prefixes,
//! repeats within a collection dropped, in the order sent; a refused one is
//! `Invalid`, never another error.

use bookclerk_library::control_plane::{parse_collections_request, LibraryCollection};
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking: twice the largest set as compact JSON,
/// plus room for whitespace and escapes.
pub const MAX_INPUT_BYTES: usize = 32 * 1024;

/// Most collections the route documents.
const DOCUMENTED_MAX_COLLECTIONS: usize = 32;

/// Most prefixes per collection the route documents.
const DOCUMENTED_MAX_PREFIXES: usize = 8;

/// Longest trimmed name the route documents, in bytes.
const DOCUMENTED_MAX_NAME_BYTES: usize = 64;

/// Longest normalized prefix the route documents, in bytes.
const DOCUMENTED_MAX_PREFIX_BYTES: usize = 256;

/// Largest normalized set as compact JSON the route documents, in bytes.
const DOCUMENTED_MAX_JSON_BYTES: usize = 8 * 1024;

/// One collection as the oracle reads it: name and prefixes.
type Collection = (String, Vec<String>);

/// What the oracle expects from one body.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused as `Invalid`.
    Refused,
    /// Accepted with these collections.
    Accepted(Vec<Collection>),
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
    let got = parse_collections_request(data);
    match expected(data) {
        Expected::Refused => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Expected::Accepted(collections) => {
            let parsed = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            let got: Vec<Collection> = parsed
                .collections
                .into_iter()
                .map(|LibraryCollection { name, prefixes }| (name, prefixes))
                .collect();
            assert_eq!(got, collections);
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
    // refuses repeats at every level, so count the keys as written.
    if has_repeated_key(data) || fields.len() != 1 {
        return Expected::Refused;
    }
    let Some(Value::Array(items)) = fields.get("collections") else {
        return Expected::Refused;
    };
    if items.len() > DOCUMENTED_MAX_COLLECTIONS {
        return Expected::Refused;
    }
    let mut out: Vec<Collection> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for item in items {
        let Value::Object(collection) = item else {
            return Expected::Refused;
        };
        if collection.len() != 2 {
            return Expected::Refused;
        }
        let (Some(Value::String(name)), Some(Value::Array(prefixes))) =
            (collection.get("name"), collection.get("prefixes"))
        else {
            return Expected::Refused;
        };
        let name = name.trim();
        if name.is_empty() || name.len() > DOCUMENTED_MAX_NAME_BYTES || name.contains('\0') {
            return Expected::Refused;
        }
        let key = name.to_lowercase();
        if names.contains(&key) {
            return Expected::Refused;
        }
        names.push(key);
        if prefixes.is_empty() || prefixes.len() > DOCUMENTED_MAX_PREFIXES {
            return Expected::Refused;
        }
        let mut normalized: Vec<String> = Vec::new();
        for prefix in prefixes {
            let Value::String(prefix) = prefix else {
                return Expected::Refused;
            };
            let prefix = prefix.trim();
            if prefix.is_empty() || prefix.starts_with('/') || prefix.contains('\0') {
                return Expected::Refused;
            }
            let prefix = if prefix.ends_with('/') {
                prefix.to_string()
            } else {
                format!("{prefix}/")
            };
            if prefix.len() > DOCUMENTED_MAX_PREFIX_BYTES {
                return Expected::Refused;
            }
            if !normalized.contains(&prefix) {
                normalized.push(prefix);
            }
        }
        out.push((name.to_string(), normalized));
    }
    let compact = serde_json::json!({
        "collections": out
            .iter()
            .map(|(name, prefixes)| serde_json::json!({"name": name, "prefixes": prefixes}))
            .collect::<Vec<_>>()
    })
    .to_string();
    if compact.len() > DOCUMENTED_MAX_JSON_BYTES {
        return Expected::Refused;
    }
    Expected::Accepted(out)
}

/// True when any JSON object in `data` writes a key twice. `data` already
/// parsed as JSON.
fn has_repeated_key(data: &[u8]) -> bool {
    /// Walks a value, failing at the first object with a repeated key.
    struct Walk;

    impl<'de> serde::de::DeserializeSeed<'de> for Walk {
        type Value = ();

        fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
            de.deserialize_any(Walk)
        }
    }

    impl<'de> Visitor<'de> for Walk {
        type Value = ();

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("any JSON value")
        }

        fn visit_bool<E>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while seq.next_element_seed(Walk)?.is_some() {}
            Ok(())
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            let mut keys: Vec<String> = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                if keys.contains(&key) {
                    return Err(serde::de::Error::custom("repeated key"));
                }
                keys.push(key);
                map.next_value_seed(Walk)?;
            }
            Ok(())
        }
    }

    let mut de = serde_json::Deserializer::from_slice(data);
    de.deserialize_any(Walk).is_err()
}
