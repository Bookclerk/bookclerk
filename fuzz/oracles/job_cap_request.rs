//! Shared oracle for the `job_cap_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of `POST /api/config/join/capacity`, which a
//! branch's operator or Owner sends to cap how many jobs the branch runs
//! for one library ([`parse_job_cap_request`]). The invariant, checked
//! against this file's own reading of the bytes as an untyped JSON value
//! rather than the typed body: a body is accepted exactly when it is one
//! JSON object with no repeated keys and no keys besides `cluster_id` (a
//! string) and `max_jobs` (`null` or a whole number from 0 to 16), both
//! present. An accepted body yields the trimmed `cluster_id` and that cap;
//! a refused one is `Invalid`, never another error.

use bookclerk_library::control_plane::parse_job_cap_request;
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking; a real body is under 100 bytes.
pub const MAX_INPUT_BYTES: usize = 512;

/// The highest cap the route documents, written out rather than read from
/// the parser.
const DOCUMENTED_MAX: u64 = 16;

/// What the oracle expects from one body.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused as `Invalid`.
    Refused,
    /// Accepted with this trimmed library id and cap (`None` for an equal
    /// share).
    Accepted(String, Option<usize>),
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
    let got = parse_job_cap_request(data);
    match expected(data) {
        Expected::Refused => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Expected::Accepted(cluster_id, cap) => {
            let request = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(request.cluster_id, cluster_id);
            assert_eq!(request.max_jobs.map(|cap| cap.jobs()), cap);
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
    if fields
        .keys()
        .any(|key| key != "cluster_id" && key != "max_jobs")
    {
        return Expected::Refused;
    }
    let Some(Value::String(cluster_id)) = fields.get("cluster_id") else {
        return Expected::Refused;
    };
    let cap = match fields.get("max_jobs") {
        Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(jobs) if jobs <= DOCUMENTED_MAX => usize::try_from(jobs).ok(),
            _ => return Expected::Refused,
        },
        _ => return Expected::Refused,
    };
    Expected::Accepted(cluster_id.trim().to_string(), cap)
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
