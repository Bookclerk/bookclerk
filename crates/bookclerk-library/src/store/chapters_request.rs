//! What an Administrator sends the chapter routes, parsed sans IO so the
//! routes, the unit tests and the `chapter_request` fuzz target drive the
//! same code (`docs/adr/untrusted-input-fuzzing.md`):
//!
//! - the body of `PUT /api/library/editions/{id}/chapters`, a chapter list
//!   checked strictly ([`parse_chapter_edit_request`]);
//! - the body of `PUT /api/library/editions/{id}/chapters/pin`, which names
//!   one of the edition's claims ([`parse_chapter_pin_request`]).

use serde::Deserialize;

use super::chapters::{ChapterCheck, ChapterInput, ChapterList, ChapterSourceKind};
use crate::error::{LibraryError, Result};

/// Largest chapter edit body the route reads, in bytes: room for
/// [`MAX_CHAPTERS`](super::chapters::MAX_CHAPTERS) chapters with titles at [`MAX_CHAPTER_TITLE_BYTES`](super::chapters::MAX_CHAPTER_TITLE_BYTES),
/// some of them escaped.
pub const MAX_CHAPTER_EDIT_BYTES: usize = 2 * 1024 * 1024;

/// Largest pin body the route reads, in bytes.
pub const MAX_CHAPTER_PIN_BYTES: usize = 1024;

/// Longest `source_id` a pin names, in bytes; a PluginKey is far shorter.
pub const MAX_CHAPTER_SOURCE_ID_BYTES: usize = 256;

/// One chapter of an edit, as JSON carries it: an object, never an array.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `ChapterEditEntryFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct ChapterEditEntry {
    /// Chapter title: 1 to 512 bytes, no control characters, no leading or
    /// trailing white space.
    #[schema(example = "Chapter 1: The Boy Who Lived")]
    title: String,
    /// Start, in milliseconds from the start of the edition's timeline; no
    /// earlier than the chapter before it.
    #[schema(example = 33_000)]
    start_ms: u64,
    /// Nesting depth, 0 for a top-level chapter; at most one more than the
    /// chapter before it, and at most 7. Left out reads as 0.
    #[serde(default)]
    #[schema(example = 0)]
    depth: u32,
}

/// [`ChapterEditEntry`]'s fields as JSON carries them; read only from a
/// JSON object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChapterEditEntryFields {
    /// See [`ChapterEditEntry::title`].
    title: String,
    /// See [`ChapterEditEntry::start_ms`].
    start_ms: u64,
    /// See [`ChapterEditEntry::depth`].
    #[serde(default)]
    depth: u32,
}

impl<'de> Deserialize<'de> for ChapterEditEntry {
    /// Reads one chapter from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: ChapterEditEntryFields = crate::control_plane::from_object(
            deserializer,
            "a JSON object with title and start_ms",
        )?;
        Ok(Self {
            title: fields.title,
            start_ms: fields.start_ms,
            depth: fields.depth,
        })
    }
}

/// `PUT /api/library/editions/{id}/chapters` body: the edition's chapters
/// as an Administrator sets them, in pre-order. An empty list clears them.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `ChapterEditFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct ChapterEditBody {
    /// The chapters, at most 2,000.
    #[schema(max_items = 2000)]
    chapters: Vec<ChapterEditEntry>,
}

/// [`ChapterEditBody`]'s fields as JSON carries them; read only from a
/// JSON object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChapterEditFields {
    /// See [`ChapterEditBody::chapters`].
    chapters: Vec<ChapterEditEntry>,
}

impl<'de> Deserialize<'de> for ChapterEditBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: ChapterEditFields =
            crate::control_plane::from_object(deserializer, "a JSON object with chapters")?;
        Ok(Self {
            chapters: fields.chapters,
        })
    }
}

impl ChapterEditBody {
    /// Checks the chapters strictly ([`ChapterCheck::Strict`]).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] naming the first chapter that
    /// breaks a rule.
    pub fn validate(self) -> Result<ChapterList> {
        ChapterList::new(
            self.chapters
                .into_iter()
                .map(|entry| ChapterInput {
                    title: entry.title,
                    start_ms: entry.start_ms,
                    depth: entry.depth,
                })
                .collect(),
            ChapterCheck::Strict,
        )
    }
}

/// Reads `body` as JSON after checking its length.
fn read_body<'de, T: Deserialize<'de>>(body: &'de [u8], max: usize, what: &str) -> Result<T> {
    if body.len() > max {
        return Err(LibraryError::Invalid(format!(
            "{what} is at most {max} bytes"
        )));
    }
    serde_json::from_slice(body).map_err(|err| LibraryError::Invalid(format!("{what}: {err}")))
}

/// Parses and checks a chapter edit body from its raw bytes, sans IO, the
/// way the route reads it.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a body over
/// [`MAX_CHAPTER_EDIT_BYTES`], bytes that are not the body's JSON shape, or
/// chapters that break a rule: more than [`MAX_CHAPTERS`](super::chapters::MAX_CHAPTERS), a title that is
/// empty, padded, over [`MAX_CHAPTER_TITLE_BYTES`](super::chapters::MAX_CHAPTER_TITLE_BYTES) or holds a control
/// character, a start past [`MAX_CHAPTER_START_MS`](super::chapters::MAX_CHAPTER_START_MS) or before the one
/// above, or a depth that skips a level or passes [`MAX_CHAPTER_DEPTH`](super::chapters::MAX_CHAPTER_DEPTH).
pub fn parse_chapter_edit_request(body: &[u8]) -> Result<ChapterList> {
    read_body::<ChapterEditBody>(body, MAX_CHAPTER_EDIT_BYTES, "chapter body")?.validate()
}

/// `PUT /api/library/editions/{id}/chapters/pin` body: the claim to pin.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `ChapterPinFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct ChapterPinBody {
    /// The claim's source kind, as the claims list names it.
    source_kind: ChapterSourceKind,
    /// The claim's source id (a PluginKey; empty for an edit or for
    /// Bookclerk's own processing). Left out reads as empty.
    #[schema(example = "plugin:audible")]
    source_id: Option<String>,
}

/// [`ChapterPinBody`]'s fields as JSON carries them; read only from a JSON
/// object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChapterPinFields {
    /// See [`ChapterPinBody::source_kind`].
    source_kind: ChapterSourceKind,
    /// See [`ChapterPinBody::source_id`].
    #[serde(default)]
    source_id: Option<String>,
}

impl<'de> Deserialize<'de> for ChapterPinBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: ChapterPinFields =
            crate::control_plane::from_object(deserializer, "a JSON object with source_kind")?;
        Ok(Self {
            source_kind: fields.source_kind,
            source_id: fields.source_id,
        })
    }
}

/// A checked pin request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterPinRequest {
    /// The claim's source kind.
    pub source_kind: ChapterSourceKind,
    /// The claim's source id.
    pub source_id: String,
}

impl ChapterPinBody {
    /// Checks the source id's length and characters.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for a source id over
    /// [`MAX_CHAPTER_SOURCE_ID_BYTES`] or with a control character.
    pub fn validate(self) -> Result<ChapterPinRequest> {
        let source_id = self.source_id.unwrap_or_default();
        if source_id.len() > MAX_CHAPTER_SOURCE_ID_BYTES || source_id.chars().any(char::is_control)
        {
            return Err(LibraryError::Invalid(format!(
                "source_id must be at most {MAX_CHAPTER_SOURCE_ID_BYTES} bytes without control characters"
            )));
        }
        Ok(ChapterPinRequest {
            source_kind: self.source_kind,
            source_id,
        })
    }
}

/// Parses and checks a pin body from its raw bytes, sans IO.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a body over
/// [`MAX_CHAPTER_PIN_BYTES`], bytes that are not the body's JSON shape, an
/// unknown source kind, or a source id [`ChapterPinBody::validate`]
/// refuses.
pub fn parse_chapter_pin_request(body: &[u8]) -> Result<ChapterPinRequest> {
    read_body::<ChapterPinBody>(body, MAX_CHAPTER_PIN_BYTES, "pin body")?.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_reads_strictly() {
        let list = parse_chapter_edit_request(
            br#"{"chapters":[{"title":"Part One","start_ms":0},{"title":"One","start_ms":5,"depth":1}]}"#,
        )
        .unwrap();
        assert_eq!(list.chapters().len(), 2);
        assert_eq!(list.chapters()[1].depth, 1);
        assert!(parse_chapter_edit_request(br#"{"chapters":[]}"#)
            .unwrap()
            .is_empty());
        for refused in [
            &br#"[{"title":"x","start_ms":0}]"#[..],
            br#"{"chapters":[{"title":" x","start_ms":0}]}"#,
            br#"{"chapters":[{"title":"x","start_ms":0,"depth":1}]}"#,
            br#"{"chapters":[{"title":"x","start_ms":9},{"title":"y","start_ms":1}]}"#,
            br#"{"chapters":[{"title":"x","start_ms":0,"extra":1}]}"#,
            br#"{"chapters":[{"title":"x\u0007","start_ms":0}]}"#,
            br#"{"chapters":[],"more":1}"#,
            // A chapter is an object, never an array of its fields.
            br#"{"chapters":[["x",0]]}"#,
            br#"{"chapters":[["x",0,0]]}"#,
        ] {
            assert!(parse_chapter_edit_request(refused).is_err());
        }
        let long = vec![b' '; MAX_CHAPTER_EDIT_BYTES + 1];
        assert!(parse_chapter_edit_request(&long).is_err());
    }

    #[test]
    fn a_pin_names_a_known_source_kind() {
        assert_eq!(
            parse_chapter_pin_request(br#"{"source_kind":"listing","source_id":"plugin:audible"}"#)
                .unwrap(),
            ChapterPinRequest {
                source_kind: ChapterSourceKind::Listing,
                source_id: "plugin:audible".into()
            }
        );
        assert_eq!(
            parse_chapter_pin_request(br#"{"source_kind":"processed"}"#)
                .unwrap()
                .source_id,
            ""
        );
        assert!(parse_chapter_pin_request(br#"{"source_kind":"wizard"}"#).is_err());
        assert!(parse_chapter_pin_request(br#"["listing","plugin:audible"]"#).is_err());
        assert!(parse_chapter_pin_request(br#"{"source_kind":"listing","x":1}"#).is_err());
        assert!(
            parse_chapter_pin_request(br#"{"source_kind":"person","source_id":"a\nb"}"#).is_err()
        );
        let id = "x".repeat(MAX_CHAPTER_SOURCE_ID_BYTES + 1);
        let body = format!(r#"{{"source_kind":"person","source_id":"{id}"}}"#);
        assert!(parse_chapter_pin_request(body.as_bytes()).is_err());
    }
}
