//! Shared oracle for the `sharing_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of `POST /api/config/join/sharing`, which a
//! branch's operator or Owner sends to change the branch's sharing and work
//! settings for one library ([`parse_sharing_request`]). The invariant,
//! checked against this file's own reading of the bytes as an untyped JSON
//! value rather than the typed body: a body is accepted exactly when it is
//! one JSON object with no repeated keys, no keys besides `cluster_id`,
//! `share_books`, `include_branches`, `list_members_books`,
//! `work_for_members` and `keep_member_copies`, a string `cluster_id`, and
//! booleans for `share_books` and `include_branches`; each of the other
//! three is absent, `null` or a boolean. An accepted body yields the
//! trimmed `cluster_id` and those values, with an absent or `null` setting
//! left unset (kept as stored), never read as `false`. A refused one is
//! `Invalid`, never another error.

use bookclerk_library::control_plane::{parse_sharing_request, ClusterLinkSettings};
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Value};

/// Largest input worth checking; a real body is under 200 bytes.
pub const MAX_INPUT_BYTES: usize = 1024;

/// The keys the route documents, written out rather than read from the
/// parser.
const KEYS: [&str; 6] = [
    "cluster_id",
    "share_books",
    "include_branches",
    "list_members_books",
    "work_for_members",
    "keep_member_copies",
];

/// What the oracle expects from one body.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused as `Invalid`.
    Refused,
    /// Accepted with this trimmed library id and these settings.
    Accepted(String, ClusterLinkSettings),
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
    let got = parse_sharing_request(data);
    match expected(data) {
        Expected::Refused => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Expected::Accepted(cluster_id, settings) => {
            let request = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(request.cluster_id, cluster_id);
            assert_eq!(request.settings, settings);
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
    if fields.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Expected::Refused;
    }
    let Some(Value::String(cluster_id)) = fields.get("cluster_id") else {
        return Expected::Refused;
    };
    let (Some(Value::Bool(share_books)), Some(Value::Bool(include_branches))) =
        (fields.get("share_books"), fields.get("include_branches"))
    else {
        return Expected::Refused;
    };
    let (Some(list_members_books), Some(work_for_members), Some(keep_member_copies)) = (
        optional(&fields, "list_members_books"),
        optional(&fields, "work_for_members"),
        optional(&fields, "keep_member_copies"),
    ) else {
        return Expected::Refused;
    };
    Expected::Accepted(
        cluster_id.trim().to_string(),
        ClusterLinkSettings {
            share_books: *share_books,
            include_branches: *include_branches,
            list_members_books,
            work_for_members,
            keep_member_copies,
        },
    )
}

/// Reads an optional setting: `Some(None)` when absent or `null`,
/// `Some(Some(value))` for a boolean, and `None` for any other value (the
/// body is refused).
fn optional(fields: &Map<String, Value>, key: &str) -> Option<Option<bool>> {
    match fields.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::Bool(value)) => Some(Some(*value)),
        Some(_) => None,
    }
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
