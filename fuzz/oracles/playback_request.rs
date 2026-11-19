//! Shared oracle for the `playback_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is what a signed-in patron's player sends the playback
//! routes. An input's first byte picks the part under test:
//!
//! - `s` or `c`: the rest is the body of
//!   `POST /api/playback/sessions/{id}/sync` (`s`) or `.../close` (`c`),
//!   read by [`parse_playback_sync_request`]. The invariant, checked
//!   against this file's own reading of the bytes as an untyped JSON value
//!   rather than the typed body: a body is accepted exactly when it is at
//!   most 1024 bytes and one JSON object with no repeated keys, a
//!   `position_ms`, optionally a `listened_ms` and a `duration_ms` (which
//!   may be `null`) and no other key, each a whole number from 0 to sixty
//!   days of milliseconds. An accepted body yields those numbers
//!   unchanged; a refused one is `Invalid`, never another error.
//! - `t`: the rest is the two path parts of
//!   `GET /api/playback/sessions/{id}/tracks/{n}` separated by a line
//!   feed, read by [`parse_track_path`]. The invariant, checked byte by
//!   byte rather than through a UUID parser: a path is accepted exactly
//!   when the id is 36 bytes of lowercase hexadecimal with hyphens after
//!   bytes 8, 13, 18 and 23 and the index is a decimal number from 0 to
//!   511 with no sign, space or leading zero; an accepted path yields the
//!   same id and number.
//!
//! Any other first byte, and an empty input, is outside the boundary and
//! checks nothing.

use bookclerk_library::{parse_playback_sync_request, parse_track_path, LibraryError};
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking; a real body is under 100 bytes.
pub const MAX_INPUT_BYTES: usize = 4096;

/// Largest body the routes document, in bytes.
const DOCUMENTED_MAX_BODY_BYTES: usize = 1024;

/// Longest length the routes document: sixty days, in milliseconds.
const DOCUMENTED_MAX_MS: u64 = 60 * 24 * 60 * 60 * 1000;

/// Number of tracks the routes document: an index is below it.
const DOCUMENTED_MAX_TRACKS: usize = 512;

/// The numbers an accepted sync body carries.
#[derive(Debug, PartialEq, Eq)]
struct Lengths {
    /// `position_ms`.
    position: u64,
    /// `listened_ms`, 0 when left out.
    listened: u64,
    /// `duration_ms`, `None` when left out or `null`.
    duration: Option<u64>,
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A tag byte, then the request bytes (see the module docs).
///
/// # Returns
///
/// True when the parser accepted the request.
///
/// # Panics
///
/// When a parser disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    match data.split_first() {
        Some((b's', body)) => check_sync(body, false),
        Some((b'c', body)) => check_sync(body, true),
        Some((b't', path)) => check_track_path(path),
        Some(_) | None => false,
    }
}

/// Checks one sync or close body against [`expected_sync`].
fn check_sync(body: &[u8], close: bool) -> bool {
    let got = parse_playback_sync_request(body, close);
    match expected_sync(body) {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(body)
            );
            false
        }
        Some(lengths) => {
            let sync = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(body))
            });
            assert_eq!(
                Lengths {
                    position: sync.position.get(),
                    listened: sync.listened.get(),
                    duration: sync.duration.map(|ms| ms.get()),
                },
                lengths
            );
            assert_eq!(sync.close, close);
            true
        }
    }
}

/// Reads a body as an untyped JSON value and applies the documented rules
/// to it; `None` is a refusal.
fn expected_sync(body: &[u8]) -> Option<Lengths> {
    if body.len() > DOCUMENTED_MAX_BODY_BYTES {
        return None;
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    // An untyped object keeps only the last of a repeated key; the body
    // refuses repeats, so count the keys as written.
    if has_repeated_top_level_key(body) {
        return None;
    }
    if fields
        .keys()
        .any(|key| !matches!(key.as_str(), "position_ms" | "listened_ms" | "duration_ms"))
    {
        return None;
    }
    let length = |value: &Value| value.as_u64().filter(|ms| *ms <= DOCUMENTED_MAX_MS);
    let position = length(fields.get("position_ms")?)?;
    let listened = match fields.get("listened_ms") {
        None => 0,
        Some(value) => length(value)?,
    };
    let duration = match fields.get("duration_ms") {
        None | Some(Value::Null) => None,
        Some(value) => Some(length(value)?),
    };
    Some(Lengths {
        position,
        listened,
        duration,
    })
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

/// Checks one stream path against [`expected_track_path`].
fn check_track_path(path: &[u8]) -> bool {
    // A path part reaches the route as text; bytes that are not UTF-8
    // never do.
    let Ok(path) = std::str::from_utf8(path) else {
        return false;
    };
    let Some((session_id, index)) = path.split_once('\n') else {
        return false;
    };
    let got = parse_track_path(session_id, index);
    match expected_track_path(session_id, index) {
        None => {
            assert_eq!(got, None, "{session_id:?} {index:?}");
            false
        }
        Some(expected_index) => {
            let Some(track) = got else {
                panic!("{session_id:?} {index:?} refused");
            };
            assert_eq!(track.session_id, session_id);
            assert_eq!(track.index, expected_index);
            true
        }
    }
}

/// Applies the documented path rules byte by byte; `None` is a refusal.
fn expected_track_path(session_id: &str, index: &str) -> Option<usize> {
    let id = session_id.as_bytes();
    if id.len() != 36 {
        return None;
    }
    for (at, byte) in id.iter().enumerate() {
        let fits = match at {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
        };
        if !fits {
            return None;
        }
    }
    let digits = index.as_bytes();
    if digits.is_empty() || digits.len() > 3 || (digits.len() > 1 && digits[0] == b'0') {
        return None;
    }
    let mut number = 0usize;
    for byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        number = number * 10 + usize::from(byte - b'0');
    }
    (number < DOCUMENTED_MAX_TRACKS).then_some(number)
}
