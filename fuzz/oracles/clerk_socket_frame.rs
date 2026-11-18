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
//!   with no repeated keys, no key but `v`, `type`, `after`, `store`,
//!   `marketplace` and `product_id`, `v` the integer 1, and either:
//!   `type` is `books`, `after` is a cursor (a string, empty or 1 to 64
//!   ASCII letters, digits and `-`) and no listing field is stated, and
//!   then it is `books` with that `after`; or `type` is `copy.grant`, no
//!   `after` is stated, `store` is a store id (1 to 64 bytes of lowercase
//!   ASCII letters, digits, `_`, `-` and `.`), `marketplace`, when stated,
//!   is at most 16 lowercase ASCII letters and digits, and `product_id` is
//!   1 to 256 bytes with no whitespace at its ends and no control
//!   character, and then it is `copy.grant` with that listing.
//! - A library frame is refused when it is over 1 MiB, not one object, has
//!   a repeated key, `v` is not the integer 1, its `type` is not `hello`,
//!   `changed`, `books.page`, `copy.granted` or `error`, or it carries a
//!   field of another type. `hello` and `changed` parse exactly when `topics` is an array
//!   of at most 16 strings, each 1 to 32 bytes of lowercase ASCII letters,
//!   digits, `_` and `.`, and then carry the names `library` and
//!   `discover` among them and nothing else. `error` parses exactly when
//!   `code` is `invalid`, `unavailable` or `not_found`. `books.page` parses exactly
//!   when `after` is a cursor, `works` is an array of at most 100 works
//!   with distinct `work` ids, each valid by the window boundary's own
//!   rules, and `next`, when present, is a non-empty cursor sorting
//!   byte-wise past `after`. `copy.granted` parses exactly when its
//!   listing follows the `copy.grant` rules, `copy_id` is a lowercase
//!   hyphenated UUID, `token` is 1 to 1024 base64url characters,
//!   `expires_in_secs` is an integer from 1 to 86 400, `copy` is well
//!   formed by the window boundary's rules, and `files` is 1 to 1000 files
//!   with distinct names, each well formed by the copy boundary's rules,
//!   exactly one of them the primary audio.
//! - Every accepted frame encodes to text that parses back to the same
//!   frame, unless the text is over the size limit (encoding writes out a
//!   probe's defaults, so a `copy.granted` just under 1 MiB can grow past
//!   it; the library never sends such a frame).

use std::collections::BTreeSet;

use bookclerk_node_proto::{
    parse_clerk_frame, parse_library_frame, ClerkCopy, ClerkFrame, ClerkSocketError, ClerkTopic,
    CopyFile, CopyRole, CopyToken, LibraryFrame, WindowCopy, WindowCursor, WindowWork,
    MAX_WINDOW_WORKS,
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
        .and_then(|fields| expected_clerk(&fields));
    assert_eq!(
        parsed,
        expected,
        "clerk frame {:?}",
        String::from_utf8_lossy(frame)
    );
    let accepted = parsed.is_some();
    if let Some(frame) = parsed {
        assert_eq!(
            parse_clerk_frame(frame.encode().as_bytes()),
            Some(frame.clone())
        );
    }
    accepted
}

/// The clerk frame `fields` should parse as.
fn expected_clerk(fields: &Map<String, Value>) -> Option<ClerkFrame> {
    let known = fields.keys().all(|key| {
        matches!(
            key.as_str(),
            "v" | "type" | "after" | "store" | "marketplace" | "product_id"
        )
    });
    if !known || !is_version(fields.get("v")) {
        return None;
    }
    let listed = ["store", "marketplace", "product_id"]
        .iter()
        .any(|key| field(fields, key).is_some());
    match fields.get("type").and_then(Value::as_str)? {
        "books" if !listed => {
            cursor(field(fields, "after")?).map(|after| ClerkFrame::Books { after })
        }
        "copy.grant" if field(fields, "after").is_none() => {
            let (store, marketplace, product_id) = listing(fields)?;
            Some(ClerkFrame::CopyGrant {
                store,
                marketplace,
                product_id,
            })
        }
        _ => None,
    }
}

/// The store listing `fields` names (store, marketplace, product id), when
/// it follows the listing rules; an unstated marketplace is empty.
fn listing(fields: &Map<String, Value>) -> Option<(String, String, String)> {
    let store = field(fields, "store")?.as_str()?;
    let marketplace = match field(fields, "marketplace") {
        None => "",
        Some(marketplace) => marketplace.as_str()?,
    };
    let product_id = field(fields, "product_id")?.as_str()?;
    let store_ok = (1..=64).contains(&store.len())
        && store.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        });
    let marketplace_ok = marketplace.len() <= 16
        && marketplace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let product_ok = (1..=256).contains(&product_id.len())
        && !product_id.starts_with(char::is_whitespace)
        && !product_id.ends_with(char::is_whitespace)
        && !product_id.chars().any(char::is_control);
    (store_ok && marketplace_ok && product_ok).then(|| {
        (
            store.to_owned(),
            marketplace.to_owned(),
            product_id.to_owned(),
        )
    })
}

/// The `copy.granted` frame `frame` should parse as. The copy's and each
/// file's own fields are the window and copy boundaries' rules
/// ([`WindowCopy::is_well_formed`], [`CopyFile::is_well_formed`]), which
/// the `node_frame` target checks.
///
/// # Arguments
///
/// * `frame` - The frame's bytes, already one object of type
///   `copy.granted`.
/// * `fields` - Its top-level fields.
fn expected_granted(frame: &[u8], fields: &Map<String, Value>) -> Option<LibraryFrame> {
    /// Just the copy and its files, read from the frame's own bytes so a
    /// nested repeated key is refused here as it is by the parser.
    #[derive(serde::Deserialize)]
    struct Parts {
        /// What the copy is.
        copy: WindowCopy,
        /// The copy's files.
        files: Vec<CopyFile>,
    }

    let (store, marketplace, product_id) = listing(fields)?;
    let copy_id = field(fields, "copy_id")?.as_str()?;
    let groups: Vec<&str> = copy_id.split('-').collect();
    let id_ok = groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|group| {
            group
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        });
    let token = field(fields, "token")?.as_str()?;
    let token_ok = (1..=1024).contains(&token.len())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let expires_in_secs = field(fields, "expires_in_secs")?.as_u64()?;
    if !id_ok || !token_ok || !(1..=86_400).contains(&expires_in_secs) {
        return None;
    }
    let Parts { copy, files } = serde_json::from_slice(frame).ok()?;
    let names: BTreeSet<&str> = files.iter().map(|file| file.name.as_str()).collect();
    let primaries = files
        .iter()
        .filter(|file| file.role == CopyRole::Audio)
        .count();
    // A file's media type, read here: `type/subtype`, neither part empty.
    let typed = |file: &CopyFile| {
        file.content_type.as_deref().is_none_or(|media| {
            media
                .split_once('/')
                .is_some_and(|(kind, sub)| !kind.is_empty() && !sub.is_empty())
        })
    };
    let well_formed = copy.is_well_formed()
        && (1..=1000).contains(&files.len())
        && files.iter().all(CopyFile::is_well_formed)
        && files.iter().all(typed)
        && names.len() == files.len()
        && primaries == 1;
    well_formed.then(|| {
        LibraryFrame::CopyGranted(Box::new(ClerkCopy {
            store,
            marketplace,
            product_id,
            copy_id: copy_id.to_owned(),
            token: CopyToken::new(token),
            expires_in_secs,
            copy,
            files,
        }))
    })
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
        "copy.granted" => &[
            "store",
            "marketplace",
            "product_id",
            "copy_id",
            "token",
            "expires_in_secs",
            "copy",
            "files",
        ],
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
            "v" | "type"
                | "topics"
                | "after"
                | "works"
                | "next"
                | "code"
                | "store"
                | "marketplace"
                | "product_id"
                | "copy_id"
                | "token"
                | "expires_in_secs"
                | "copy"
                | "files"
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
                Some("not_found") => Some(LibraryFrame::Error {
                    code: ClerkSocketError::NotFound,
                }),
                _ => None,
            };
            assert_eq!(parsed, expected, "error frame {text:?}");
        }
        "copy.granted" => {
            let expected = expected_granted(frame, &fields);
            assert_eq!(parsed, expected, "copy.granted frame");
        }
        _ => {
            let expected = expected_page(frame, &fields);
            assert_eq!(parsed, expected, "books.page frame {text:?}");
        }
    }
    if let Some(frame) = &parsed {
        let encoded = frame.encode();
        if encoded.len() <= 1024 * 1024 {
            assert_eq!(
                parse_library_frame(encoded.as_bytes()).as_ref(),
                Some(frame),
                "{text:?} does not round-trip"
            );
        }
    }
    parsed.is_some()
}

/// The `books.page` frame `frame` should parse as, worked out here rather
/// than by `window_page_problem`: `after` a [`cursor`], `works` an array
/// of at most [`MAX_WINDOW_WORKS`] works with distinct `work` ids, and
/// `next`, when present, a non-empty cursor sorting byte-wise past
/// `after`. Each
/// work's own fields are the window boundary's rules
/// ([`WindowWork::problem`]), which its own target checks.
///
/// # Arguments
///
/// * `frame` - The frame's bytes, already one object of type `books.page`.
/// * `fields` - Its top-level fields.
fn expected_page(frame: &[u8], fields: &Map<String, Value>) -> Option<LibraryFrame> {
    /// Just the works, read from the frame's own bytes so a nested
    /// repeated key is refused here as it is by the parser.
    #[derive(serde::Deserialize)]
    struct Works {
        /// The page's works.
        works: Vec<WindowWork>,
    }

    let after = cursor(field(fields, "after")?)?;
    let next = match field(fields, "next") {
        None => None,
        Some(next) => {
            let next = cursor(next)?;
            if next.as_str().is_empty() || next.as_str().as_bytes() <= after.as_str().as_bytes() {
                return None;
            }
            Some(next)
        }
    };
    let works = serde_json::from_slice::<Works>(frame).ok()?.works;
    if works.len() > MAX_WINDOW_WORKS || works.iter().any(|work| work.problem().is_some()) {
        return None;
    }
    let ids: BTreeSet<&str> = works.iter().map(|work| work.work.as_str()).collect();
    if ids.len() != works.len() {
        return None;
    }
    Some(LibraryFrame::BooksPage { after, works, next })
}

/// The cursor `value` is, read here: a JSON string, empty or 1 to 64
/// ASCII letters, digits and `-`; `None` for anything else.
fn cursor(value: &Value) -> Option<WindowCursor> {
    let text = value.as_str()?;
    if text.len() > 64 || !text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    let parsed = WindowCursor::parse(text);
    assert!(parsed.is_some(), "the cursor {text:?} was refused");
    parsed
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
