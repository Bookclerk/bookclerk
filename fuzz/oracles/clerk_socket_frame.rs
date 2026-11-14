//! Shared oracle for the `clerk_socket_frame` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the clerk socket, `GET /api/clerk/v1/connect`, in both
//! directions: the frames a library reads from a clerk
//! (`bookclerk-node-proto::parse_clerk_frame`) and the frames a clerk reads
//! from a library (`parse_library_frame`). The first input byte picks the
//! direction (even: clerk to library; odd: library to clerk); the rest is
//! the frame.
//!
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value (a key whose value is `null` counts as absent for
//! the optional fields):
//!
//! - A clerk frame parses exactly when it is at most 4 KiB, one object
//!   with no repeated keys and exactly the keys `v` (the integer 1),
//!   `type` (`books`) and `after` (an integer, not negative), and then it
//!   is `books` with that `after`.
//! - A library frame is refused when it is over 1 MiB, not one object, has
//!   a repeated key, `v` is not the integer 1, its `type` is not `hello`,
//!   `changed`, `books.page` or `error`, or it carries a field of another
//!   type. `hello` and `changed` parse exactly when `topics` is an array
//!   of at most 16 strings, each 1 to 32 bytes of lowercase ASCII letters,
//!   digits, `_` and `.`, and then carry the names `library` and
//!   `discover` among them and nothing else. `error` parses exactly when
//!   `code` is `invalid` or `unavailable`. An accepted `books.page` has an
//!   `after` that is not negative and passes `window_page_problem`.
//! - Every accepted frame encodes to text that parses back to the same
//!   frame.

use std::collections::BTreeSet;

use bookclerk_node_proto::{
    parse_clerk_frame, parse_library_frame, window_page_problem, ClerkFrame, ClerkSocketError,
    ClerkTopic, LibraryFrame,
};
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Value};

/// Largest input worth checking: a clerk frame is at most 4 KiB, and a
/// library frame's rules are all reached well under this.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Largest clerk frame, written out.
const MAX_CLERK_BYTES: usize = 4 * 1024;

/// Checks one input against the invariant.
///
/// # Arguments
///
/// * `data` - The direction byte, then the frame.
///
/// # Returns
///
/// True when the frame was accepted.
///
/// # Panics
///
/// When a parser disagrees with the oracle.
#[must_use]
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let Some((&pick, frame)) = data.split_first() else {
        return false;
    };
    if pick % 2 == 0 {
        check_clerk(frame)
    } else {
        check_library(frame)
    }
}

/// The frame as one JSON object with no repeated keys, else `None`.
fn object(frame: &[u8]) -> Option<Map<String, Value>> {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(frame) else {
        return None;
    };
    (!has_repeated_top_level_key(frame)).then_some(fields)
}

/// True when `value` is the integer 1.
fn is_version(value: Option<&Value>) -> bool {
    value.and_then(Value::as_u64) == Some(1) && value.is_some_and(|v| !v.is_f64())
}

/// Checks a clerk frame.
fn check_clerk(frame: &[u8]) -> bool {
    let parsed = parse_clerk_frame(frame);
    let expected = (frame.len() <= MAX_CLERK_BYTES)
        .then(|| object(frame))
        .flatten()
        .and_then(|fields| {
            let keys: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
            if keys != BTreeSet::from(["v", "type", "after"])
                || !is_version(fields.get("v"))
                || fields.get("type") != Some(&Value::String("books".into()))
            {
                return None;
            }
            let after = fields.get("after")?;
            if after.is_f64() {
                return None;
            }
            after
                .as_i64()
                .filter(|after| *after >= 0)
                .map(|after| ClerkFrame::Books { after })
        });
    assert_eq!(
        parsed,
        expected,
        "clerk frame {:?}",
        String::from_utf8_lossy(frame)
    );
    if let Some(frame) = parsed {
        assert_eq!(parse_clerk_frame(frame.encode().as_bytes()), Some(frame));
    }
    parsed.is_some()
}

/// The field `key` when it is present and not `null`.
fn field<'a>(fields: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    fields.get(key).filter(|value| !value.is_null())
}

/// The topics `value` names, when it follows the topic rules.
fn topics(value: &Value) -> Option<BTreeSet<ClerkTopic>> {
    let names = value.as_array()?;
    if names.len() > 16 {
        return None;
    }
    let mut known = BTreeSet::new();
    for name in names {
        let name = name.as_str()?;
        let well_formed = !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.');
        if !well_formed {
            return None;
        }
        match name {
            "library" => {
                known.insert(ClerkTopic::Library);
            }
            "discover" => {
                known.insert(ClerkTopic::Discover);
            }
            _ => {}
        }
    }
    Some(known)
}

/// Checks a library frame.
fn check_library(frame: &[u8]) -> bool {
    let parsed = parse_library_frame(frame);
    let text = String::from_utf8_lossy(frame);
    let Some(fields) = (frame.len() <= 1024 * 1024)
        .then(|| object(frame))
        .flatten()
    else {
        assert_eq!(parsed, None, "not one object: {text:?}");
        return false;
    };
    let kind = fields.get("type").and_then(Value::as_str).unwrap_or("");
    let allowed: &[&str] = match kind {
        "hello" | "changed" => &["topics"],
        "books.page" => &["after", "works", "next"],
        "error" => &["code"],
        _ => {
            assert_eq!(parsed, None, "unknown type: {text:?}");
            return false;
        }
    };
    let foreign = fields.keys().any(|key| {
        !matches!(key.as_str(), "v" | "type")
            && !allowed.contains(&key.as_str())
            && field(&fields, key).is_some()
    });
    let unknown_key = fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "v" | "type" | "topics" | "after" | "works" | "next" | "code"
        )
    });
    if foreign || unknown_key || !is_version(fields.get("v")) {
        assert_eq!(parsed, None, "a field of another type or version: {text:?}");
        return false;
    }
    match kind {
        "hello" | "changed" => {
            let expected = field(&fields, "topics").and_then(topics).map(|topics| {
                if kind == "hello" {
                    LibraryFrame::Hello { topics }
                } else {
                    LibraryFrame::Changed { topics }
                }
            });
            assert_eq!(parsed, expected, "topics frame {text:?}");
        }
        "error" => {
            let expected = match field(&fields, "code").and_then(Value::as_str) {
                Some("invalid") => Some(LibraryFrame::Error {
                    code: ClerkSocketError::Invalid,
                }),
                Some("unavailable") => Some(LibraryFrame::Error {
                    code: ClerkSocketError::Unavailable,
                }),
                _ => None,
            };
            assert_eq!(parsed, expected, "error frame {text:?}");
        }
        _ => {
            if let Some(LibraryFrame::BooksPage { after, works, next }) = &parsed {
                assert!(*after >= 0, "negative cursor accepted: {text:?}");
                assert_eq!(window_page_problem(*after, works, *next), None, "{text:?}");
                let sent = field(&fields, "after").and_then(Value::as_i64);
                assert_eq!(sent, Some(*after), "{text:?}");
            } else {
                assert!(parsed.is_none(), "{text:?} parsed as another type");
            }
        }
    }
    if let Some(frame) = &parsed {
        assert_eq!(
            parse_library_frame(frame.encode().as_bytes()).as_ref(),
            Some(frame),
            "{text:?} does not round-trip"
        );
    }
    parsed.is_some()
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
