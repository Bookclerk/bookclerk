//! Shared oracle for the `chapter_request` fuzz target and the
//! `bookclerk-library` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is what an Administrator sends the chapter routes: the
//! body of `PUT /api/library/editions/{id}/chapters`
//! ([`parse_chapter_edit_request`]) and of
//! `PUT /api/library/editions/{id}/chapters/pin`
//! ([`parse_chapter_pin_request`]). The input's first byte picks the route
//! (even: edit, odd: pin) and the rest is the body.
//!
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value rather than the typed bodies:
//!
//! - An edit is accepted exactly when it is one JSON object, with no key
//!   written twice anywhere, holding only `chapters`: an array of at most
//!   2,000 objects, each with a string `title`, an unsigned integer
//!   `start_ms` and optionally an unsigned 32-bit `depth`, and nothing
//!   else. Every title is 1 to 512 bytes with no control character and no
//!   leading or trailing white space; every start is at most 60 days in
//!   milliseconds and no earlier than the one before; the first depth is 0
//!   and each later one at most one deeper than the one before, and at
//!   most 7. An accepted edit yields exactly those chapters.
//! - A pin is accepted exactly when it is one JSON object, with no key
//!   written twice, holding a `source_kind` that names one of the seven
//!   kinds and optionally a `source_id` that is a string (or null, read as
//!   empty) of at most 256 bytes with no control character. An accepted
//!   pin yields that kind and id.
//!
//! A refused body is `Invalid`, never another error.

use bookclerk_library::{
    parse_chapter_edit_request, parse_chapter_pin_request, ChapterSourceKind, LibraryError,
};
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Largest input worth checking; enough for a few hundred chapters.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Most chapters the route documents.
const DOCUMENTED_MAX_CHAPTERS: usize = 2_000;

/// Longest title the route documents, in bytes.
const DOCUMENTED_MAX_TITLE_BYTES: usize = 512;

/// Deepest nesting the route documents.
const DOCUMENTED_MAX_DEPTH: u64 = 7;

/// Latest start the route documents: 60 days, in milliseconds.
const DOCUMENTED_MAX_START_MS: u64 = 60 * 24 * 60 * 60 * 1000;

/// Longest source id the route documents, in bytes.
const DOCUMENTED_MAX_SOURCE_ID_BYTES: usize = 256;

/// The source kinds as the claims list spells them.
const KINDS: [(&str, ChapterSourceKind); 7] = [
    ("person", ChapterSourceKind::Person),
    ("outside_edit", ChapterSourceKind::OutsideEdit),
    ("processed", ChapterSourceKind::Processed),
    ("provider", ChapterSourceKind::Provider),
    ("listing", ChapterSourceKind::Listing),
    ("file", ChapterSourceKind::File),
    ("part_names", ChapterSourceKind::PartNames),
];

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The route selector byte, then the request body bytes.
///
/// # Returns
///
/// True when the parser accepted the body.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    let Some((&selector, body)) = data.split_first() else {
        return false;
    };
    if body.len() > MAX_INPUT_BYTES {
        return false;
    }
    if selector % 2 == 0 {
        check_edit(body)
    } else {
        check_pin(body)
    }
}

/// Checks one edit body.
fn check_edit(body: &[u8]) -> bool {
    let got = parse_chapter_edit_request(body);
    match expected_edit(body) {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(body)
            );
            false
        }
        Some(chapters) => {
            let list = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(body))
            });
            let read: Vec<(String, u64, u32)> = list
                .chapters()
                .iter()
                .map(|c| (c.title.clone(), c.start_ms, c.depth))
                .collect();
            assert_eq!(read, chapters);
            true
        }
    }
}

/// Checks one pin body.
fn check_pin(body: &[u8]) -> bool {
    let got = parse_chapter_pin_request(body);
    match expected_pin(body) {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(body)
            );
            false
        }
        Some((kind, id)) => {
            let pin = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(body))
            });
            assert_eq!(pin.source_kind, kind);
            assert_eq!(pin.source_id, id);
            true
        }
    }
}

/// The top-level object of `body`, when it is one with no key written
/// twice anywhere.
fn object(body: &[u8]) -> Option<Map<String, Value>> {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    // An untyped object keeps only the last of a repeated key; the bodies
    // refuse repeats, so count the keys as written.
    let mut de = serde_json::Deserializer::from_slice(body);
    match RepeatedKey::deserialize(&mut de) {
        Ok(RepeatedKey(false)) => Some(fields),
        _ => None,
    }
}

/// The chapters an edit body documents, or `None` when it is refused.
fn expected_edit(body: &[u8]) -> Option<Vec<(String, u64, u32)>> {
    let fields = object(body)?;
    if fields.len() != 1 {
        return None;
    }
    let Some(Value::Array(entries)) = fields.get("chapters") else {
        return None;
    };
    if entries.len() > DOCUMENTED_MAX_CHAPTERS {
        return None;
    }
    let mut out: Vec<(String, u64, u32)> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Value::Object(entry) = entry else {
            return None;
        };
        if entry
            .keys()
            .any(|key| !matches!(key.as_str(), "title" | "start_ms" | "depth"))
        {
            return None;
        }
        let Some(Value::String(title)) = entry.get("title") else {
            return None;
        };
        let start_ms = entry.get("start_ms")?.as_u64()?;
        let depth = match entry.get("depth") {
            None => 0,
            Some(depth) => u32::try_from(depth.as_u64()?).ok()?,
        };
        let title_ok = !title.is_empty()
            && title.len() <= DOCUMENTED_MAX_TITLE_BYTES
            && !title.chars().any(char::is_control)
            && !title.starts_with(char::is_whitespace)
            && !title.ends_with(char::is_whitespace);
        if !title_ok || start_ms > DOCUMENTED_MAX_START_MS {
            return None;
        }
        let deepest = match out.last() {
            None => 0,
            Some(&(_, before, _)) if start_ms < before => return None,
            Some(&(_, _, above)) => (u64::from(above) + 1).min(DOCUMENTED_MAX_DEPTH),
        };
        if u64::from(depth) > deepest {
            return None;
        }
        out.push((title.clone(), start_ms, depth));
    }
    Some(out)
}

/// The claim a pin body names, or `None` when it is refused.
fn expected_pin(body: &[u8]) -> Option<(ChapterSourceKind, String)> {
    let fields = object(body)?;
    if fields
        .keys()
        .any(|key| !matches!(key.as_str(), "source_kind" | "source_id"))
    {
        return None;
    }
    let Some(Value::String(kind)) = fields.get("source_kind") else {
        return None;
    };
    let kind = KINDS.iter().find(|(name, _)| name == kind)?.1;
    let id = match fields.get("source_id") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(id)) => id.clone(),
        Some(_) => return None,
    };
    if id.len() > DOCUMENTED_MAX_SOURCE_ID_BYTES || id.chars().any(char::is_control) {
        return None;
    }
    Some((kind, id))
}

/// Whether any object in a JSON document writes a key twice.
struct RepeatedKey(bool);

impl<'de> Deserialize<'de> for RepeatedKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_any(RepeatedKeyVisitor)
            .map(RepeatedKey)
    }
}

/// Walks one JSON value for [`RepeatedKey`].
struct RepeatedKeyVisitor;

impl<'de> Visitor<'de> for RepeatedKeyVisitor {
    type Value = bool;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_i64<E>(self, _: i64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_u64<E>(self, _: u64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_f64<E>(self, _: f64) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_str<E>(self, _: &str) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_unit<E>(self) -> Result<bool, E> {
        Ok(false)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        let mut repeated = false;
        while let Some(RepeatedKey(inner)) = seq.next_element()? {
            repeated |= inner;
        }
        Ok(repeated)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        let mut keys = std::collections::BTreeSet::new();
        let mut repeated = false;
        while let Some((key, RepeatedKey(inner))) = map.next_entry::<String, RepeatedKey>()? {
            repeated |= inner || !keys.insert(key);
        }
        Ok(repeated)
    }
}
