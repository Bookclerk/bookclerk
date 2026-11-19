//! Shared oracle for the `pace_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of
//! `POST /api/portal/sources/{source}/connections/{account_id}/pace`, which
//! the person an account is linked to sends to change how fast Bookclerk
//! downloads from it ([`parse_pace_request`]). The invariant, checked
//! against this file's own reading of the bytes as an untyped JSON value
//! rather than the typed body: a body is accepted exactly when it is one
//! JSON object with no repeated keys, no keys besides `mode`,
//! `downloads_per_day`, `min_gap_seconds` and `clear_cooldown`, a `mode`
//! of `default`, `custom` or `unlimited`, `clear_cooldown` absent, `null`
//! or a boolean, and the two numbers each absent, `null` or a whole number
//! in range (1 to 10000 downloads, 0 to 86400 seconds); only `custom` may
//! carry a number, and it needs at least one. A refused body is `Invalid`,
//! never another error.

use std::num::NonZeroU32;

use bookclerk_library::pacing::{parse_pace_request, PaceOverride, PaceRequest};
use bookclerk_library::LibraryError;
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Value};

/// Largest input worth checking; a real body is under 100 bytes.
pub const MAX_INPUT_BYTES: usize = 1024;

/// The keys the route documents, written out rather than read from the
/// parser.
const KEYS: [&str; 4] = [
    "mode",
    "downloads_per_day",
    "min_gap_seconds",
    "clear_cooldown",
];

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
    let got = parse_pace_request(data);
    match expected(data) {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(data)
            );
            false
        }
        Some(want) => {
            let request = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(data))
            });
            assert_eq!(request, want, "{:?}", String::from_utf8_lossy(data));
            true
        }
    }
}

/// Reads the body as an untyped JSON value and applies the documented
/// rules to it; `None` when it must be refused.
fn expected(data: &[u8]) -> Option<PaceRequest> {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(data) else {
        return None;
    };
    if has_repeated_top_level_key(data) {
        return None;
    }
    if fields.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return None;
    }
    let clear_cooldown = match fields.get("clear_cooldown") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return None,
    };
    let per_day = number(&fields, "downloads_per_day", 1, 10_000)?;
    let gap = number(&fields, "min_gap_seconds", 0, 86_400)?;
    let override_ = match fields.get("mode") {
        Some(Value::String(mode)) if mode == "default" || mode == "unlimited" => {
            if per_day.is_some() || gap.is_some() {
                return None;
            }
            if mode == "default" {
                PaceOverride::Default
            } else {
                PaceOverride::Unlimited
            }
        }
        Some(Value::String(mode)) if mode == "custom" => {
            if per_day.is_none() && gap.is_none() {
                return None;
            }
            PaceOverride::Custom {
                downloads_per_day: per_day.and_then(NonZeroU32::new),
                min_gap_seconds: gap,
            }
        }
        _ => return None,
    };
    Some(PaceRequest {
        override_,
        clear_cooldown,
    })
}

/// Reads an optional whole number: `Some(None)` when absent or `null`,
/// `Some(Some(n))` for a whole number from `min` to `max`, and `None` for
/// anything else (the body is refused).
fn number(fields: &Map<String, Value>, key: &str, min: u64, max: u64) -> Option<Option<u32>> {
    match fields.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::Number(n)) => {
            let n = n.as_u64()?;
            if (min..=max).contains(&n) {
                u32::try_from(n).ok().map(Some)
            } else {
                None
            }
        }
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
