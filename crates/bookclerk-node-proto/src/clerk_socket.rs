//! The clerk socket: a clerk's live link to a library it signed in to
//! with a library card ([`crate::CLERK_TOKEN_PATH`]).
//!
//! The clerk's daemon dials [`CLERK_CONNECT_PATH`] with the WebSocket
//! subprotocol [`CLERK_SUBPROTOCOL`] and its card as
//! `Authorization: Bookclerk-Clerk <token>`. The socket is a person's
//! window onto their own books, not a node link: it carries no node id,
//! no key and no work, and nothing on it claims a job.
//!
//! Frames are JSON text objects with `"v": 1` and a `type`:
//!
//! | Direction | `type` | Fields | Meaning |
//! |---|---|---|---|
//! | library → clerk | `hello` | `topics` | once, after the upgrade: the topics this card hears about |
//! | library → clerk | `changed` | `topics` | those topics changed; read again |
//! | clerk → library | `books` | `after` | the page of the person's books after this cursor (0 for the first) |
//! | library → clerk | `books.page` | `after`, `works`, `next?` | that page ([`WindowWork`]); no `next` on the last page |
//! | library → clerk | `error` | `code` | a `books` frame was not answered ([`ClerkSocketError`]) |
//!
//! Pages answer in the order they were asked, and each echoes the `after`
//! it answers. A page carries what a window's `window.page` carries: works
//! grouped by the library's own work id, each with its identifiers and
//! owned editions, and no account, user or library field.
//!
//! Both directions are untrusted input. The library reads clerk frames with
//! [`parse_clerk_frame`] (at most [`MAX_CLERK_FRAME_BYTES`]); the clerk
//! reads library frames with [`parse_library_frame`] (at most
//! [`crate::MAX_FRAME_BYTES`]), which checks every page with
//! [`crate::window_page_problem`] and drops topic names it does not know,
//! so a newer library can add topics without breaking an older clerk.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{window_page_problem, WindowWork, MAX_FRAME_BYTES};

/// Path of the clerk socket.
pub const CLERK_CONNECT_PATH: &str = "/api/clerk/v1/connect";

/// The WebSocket subprotocol a clerk must offer.
pub const CLERK_SUBPROTOCOL: &str = "bookclerk-clerk.v1";

/// The `v` every frame carries.
pub const CLERK_SOCKET_VERSION: u8 = 1;

/// Largest frame a library accepts from a clerk, in bytes; a `books` frame
/// is under 50.
pub const MAX_CLERK_FRAME_BYTES: usize = 4 * 1024;

/// Most topic names one `hello` or `changed` frame may carry, known or not.
pub const MAX_CLERK_TOPICS: usize = 16;

/// Longest topic name, in bytes.
pub const MAX_CLERK_TOPIC_BYTES: usize = 32;

/// Something a clerk hears about on the socket. Each names the routes to
/// read again; the socket itself carries no row data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClerkTopic {
    /// The library's books changed: read the person's books again.
    Library,
    /// The person's stored Discover shelves may have changed.
    Discover,
}

impl ClerkTopic {
    /// Every topic.
    pub const ALL: [Self; 2] = [Self::Library, Self::Discover];

    /// The topic's wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Discover => "discover",
        }
    }

    /// The topic named `name`, or `None` for a name this version does not
    /// know.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|topic| topic.as_str() == name)
    }
}

/// True when `name` could be a topic name: 1 to [`MAX_CLERK_TOPIC_BYTES`]
/// bytes of lowercase ASCII letters, digits, `_` and `.`.
#[must_use]
pub fn is_topic_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_CLERK_TOPIC_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// Why the library did not answer a `books` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClerkSocketError {
    /// The clerk sent a frame the library could not read.
    Invalid,
    /// The library could not read the books just now; ask again later.
    Unavailable,
}

/// A frame from a clerk to a library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClerkFrame {
    /// `books`: the page of the person's books after `after` (0 for the
    /// first page; never negative).
    Books {
        /// The cursor of the previous page.
        after: i64,
    },
}

/// A frame from a library to a clerk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryFrame {
    /// `hello`: the topics this card hears about, sent once.
    Hello {
        /// The topics, without names this version does not know.
        topics: BTreeSet<ClerkTopic>,
    },
    /// `changed`: these topics changed.
    Changed {
        /// The topics, without names this version does not know.
        topics: BTreeSet<ClerkTopic>,
    },
    /// `books.page`: one page of the person's books.
    BooksPage {
        /// The cursor the clerk asked with.
        after: i64,
        /// At most [`crate::MAX_WINDOW_WORKS`] works.
        works: Vec<WindowWork>,
        /// The cursor of the next page; `None` on the last page.
        next: Option<i64>,
    },
    /// `error`: a `books` frame was not answered.
    Error {
        /// Why.
        code: ClerkSocketError,
    },
}

/// A clerk frame as written; every field is checked by
/// [`parse_clerk_frame`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClerkWire {
    /// Protocol version; must be [`CLERK_SOCKET_VERSION`].
    v: u8,
    /// Frame type; must be `books`.
    #[serde(rename = "type")]
    kind: String,
    /// The cursor of the previous page.
    after: i64,
}

/// A library frame as written; every field is checked by
/// [`parse_library_frame`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LibraryWire {
    /// Protocol version; must be [`CLERK_SOCKET_VERSION`].
    v: u8,
    /// Frame type.
    #[serde(rename = "type")]
    kind: String,
    /// `hello` and `changed`: topic names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topics: Option<Vec<String>>,
    /// `books.page`: the cursor the clerk asked with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<i64>,
    /// `books.page`: the works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    works: Option<Vec<WindowWork>>,
    /// `books.page`: the next cursor, absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next: Option<i64>,
    /// `error`: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<ClerkSocketError>,
}

impl ClerkFrame {
    /// The frame's JSON text, as a clerk sends it.
    #[must_use]
    pub fn encode(&self) -> String {
        let wire = match *self {
            Self::Books { after } => ClerkWire {
                v: CLERK_SOCKET_VERSION,
                kind: "books".into(),
                after,
            },
        };
        // A struct of a string and two integers always serializes.
        serde_json::to_string(&wire).unwrap_or_default()
    }
}

impl LibraryFrame {
    /// The frame's JSON text, as a library sends it.
    #[must_use]
    pub fn encode(&self) -> String {
        let names = |topics: &BTreeSet<ClerkTopic>| {
            Some(topics.iter().map(|t| t.as_str().to_owned()).collect())
        };
        let mut wire = LibraryWire {
            v: CLERK_SOCKET_VERSION,
            kind: String::new(),
            topics: None,
            after: None,
            works: None,
            next: None,
            code: None,
        };
        match self {
            Self::Hello { topics } => {
                wire.kind = "hello".into();
                wire.topics = names(topics);
            }
            Self::Changed { topics } => {
                wire.kind = "changed".into();
                wire.topics = names(topics);
            }
            Self::BooksPage { after, works, next } => {
                wire.kind = "books.page".into();
                wire.after = Some(*after);
                wire.works = Some(works.clone());
                wire.next = *next;
            }
            Self::Error { code } => {
                wire.kind = "error".into();
                wire.code = Some(*code);
            }
        }
        // Strings, integers and the derived works always serialize.
        serde_json::to_string(&wire).unwrap_or_default()
    }
}

/// Reads a frame a clerk sent: at most [`MAX_CLERK_FRAME_BYTES`], one JSON
/// object with exactly `v` (1), `type` (`books`) and `after` (an integer,
/// not negative), each once.
///
/// # Arguments
///
/// * `text` - The frame's bytes.
///
/// # Returns
///
/// The frame, or `None` when it breaks any rule.
#[must_use]
pub fn parse_clerk_frame(text: &[u8]) -> Option<ClerkFrame> {
    if text.len() > MAX_CLERK_FRAME_BYTES || !starts_object(text) {
        return None;
    }
    let wire: ClerkWire = serde_json::from_slice(text).ok()?;
    if wire.v != CLERK_SOCKET_VERSION || wire.kind != "books" || wire.after < 0 {
        return None;
    }
    Some(ClerkFrame::Books { after: wire.after })
}

/// Reads a frame a library sent: at most [`crate::MAX_FRAME_BYTES`], one
/// JSON object with `v` (1), a known `type`, and exactly that type's fields,
/// each once.
///
/// - `hello` and `changed`: `topics`, at most [`MAX_CLERK_TOPICS`] names,
///   each [`is_topic_name`]; names this version does not know are dropped.
/// - `books.page`: `after` (not negative), `works`, and `next` unless it is
///   the last page, passing [`crate::window_page_problem`].
/// - `error`: a [`ClerkSocketError`] `code`.
///
/// # Arguments
///
/// * `text` - The frame's bytes.
///
/// # Returns
///
/// The frame, or `None` when it breaks any rule.
#[must_use]
pub fn parse_library_frame(text: &[u8]) -> Option<LibraryFrame> {
    if text.len() > MAX_FRAME_BYTES || !starts_object(text) {
        return None;
    }
    let wire: LibraryWire = serde_json::from_slice(text).ok()?;
    if wire.v != CLERK_SOCKET_VERSION {
        return None;
    }
    let LibraryWire {
        kind,
        topics,
        after,
        works,
        next,
        code,
        ..
    } = wire;
    match kind.as_str() {
        "hello" | "changed" => {
            if after.is_some() || works.is_some() || next.is_some() || code.is_some() {
                return None;
            }
            let topics = known_topics(topics?)?;
            Some(if kind == "hello" {
                LibraryFrame::Hello { topics }
            } else {
                LibraryFrame::Changed { topics }
            })
        }
        "books.page" => {
            if topics.is_some() || code.is_some() {
                return None;
            }
            let (after, works) = (after?, works?);
            if after < 0 || window_page_problem(after, &works, next).is_some() {
                return None;
            }
            Some(LibraryFrame::BooksPage { after, works, next })
        }
        "error" => {
            if topics.is_some() || after.is_some() || works.is_some() || next.is_some() {
                return None;
            }
            Some(LibraryFrame::Error { code: code? })
        }
        _ => None,
    }
}

/// True when the first byte after JSON whitespace opens an object: a
/// derived struct would otherwise also read an array of its fields.
fn starts_object(text: &[u8]) -> bool {
    text.iter()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .is_some_and(|b| *b == b'{')
}

/// The known topics among `names`, or `None` when there are more than
/// [`MAX_CLERK_TOPICS`] or one is not [`is_topic_name`].
fn known_topics(names: Vec<String>) -> Option<BTreeSet<ClerkTopic>> {
    if names.len() > MAX_CLERK_TOPICS || !names.iter().all(|name| is_topic_name(name)) {
        return None;
    }
    Some(
        names
            .iter()
            .filter_map(|name| ClerkTopic::from_name(name))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogMetadata;
    use crate::lookup::{Availability, LookupEdition, LookupFormat};

    fn work() -> WindowWork {
        WindowWork {
            work: "w-1".into(),
            identifiers: Vec::new(),
            editions: vec![LookupEdition {
                store: "audible".into(),
                marketplace: "us".into(),
                product_id: "B0TEST".into(),
                availability: Availability::Owned,
                formats: vec![LookupFormat::Audio],
                metadata: CatalogMetadata {
                    title: "A Title".into(),
                    ..CatalogMetadata::default()
                },
            }],
        }
    }

    #[test]
    fn a_books_frame_round_trips_and_nothing_else_is_read() {
        let frame = ClerkFrame::Books { after: 41 };
        assert_eq!(frame.encode(), r#"{"v":1,"type":"books","after":41}"#);
        assert_eq!(parse_clerk_frame(frame.encode().as_bytes()), Some(frame));
        for refused in [
            r#"{"v":1,"type":"books","after":-1}"#,
            r#"{"v":2,"type":"books","after":0}"#,
            r#"{"v":1,"type":"claim","after":0}"#,
            r#"{"v":1,"type":"books"}"#,
            r#"{"v":1,"type":"books","after":0,"user":7}"#,
            r#"{"v":1,"type":"books","after":0,"after":1}"#,
            r#"{"v":1,"type":"books","after":1.5}"#,
            r#"[1,"books",0]"#,
            r#" [1,"books",0]"#,
            "",
        ] {
            assert_eq!(parse_clerk_frame(refused.as_bytes()), None, "{refused}");
        }
        let padded = format!(
            r#"{{"v":1,"type":"books","after":0}}{}"#,
            " ".repeat(MAX_CLERK_FRAME_BYTES)
        );
        assert_eq!(parse_clerk_frame(padded.as_bytes()), None);
    }

    #[test]
    fn library_frames_round_trip() {
        let frames = [
            LibraryFrame::Hello {
                topics: ClerkTopic::ALL.into_iter().collect(),
            },
            LibraryFrame::Changed {
                topics: BTreeSet::from([ClerkTopic::Library]),
            },
            LibraryFrame::BooksPage {
                after: 0,
                works: vec![work()],
                next: Some(9),
            },
            LibraryFrame::BooksPage {
                after: 9,
                works: Vec::new(),
                next: None,
            },
            LibraryFrame::Error {
                code: ClerkSocketError::Unavailable,
            },
        ];
        for frame in frames {
            let text = frame.encode();
            assert_eq!(parse_library_frame(text.as_bytes()), Some(frame), "{text}");
        }
        assert_eq!(
            LibraryFrame::Changed {
                topics: BTreeSet::from([ClerkTopic::Library]),
            }
            .encode(),
            r#"{"v":1,"type":"changed","topics":["library"]}"#
        );
    }

    #[test]
    fn unknown_topics_are_dropped_and_bad_frames_refused() {
        assert_eq!(
            parse_library_frame(br#"{"v":1,"type":"changed","topics":["library","shelves"]}"#),
            Some(LibraryFrame::Changed {
                topics: BTreeSet::from([ClerkTopic::Library]),
            })
        );
        let page = |after: i64, next: &str| {
            format!(r#"{{"v":1,"type":"books.page","after":{after},"works":[]{next}}}"#)
        };
        assert!(parse_library_frame(page(3, "").as_bytes()).is_some());
        for refused in [
            r#"{"v":1,"type":"changed","topics":["Library"]}"#.to_owned(),
            r#"{"v":1,"type":"changed"}"#.to_owned(),
            r#"{"v":1,"type":"changed","topics":[],"code":"invalid"}"#.to_owned(),
            r#"{"v":1,"type":"error","code":"later"}"#.to_owned(),
            r#"{"v":1,"type":"work","topics":[]}"#.to_owned(),
            r#"{"v":2,"type":"hello","topics":[]}"#.to_owned(),
            page(3, r#","next":3"#),
            page(-1, ""),
            format!(
                r#"{{"v":1,"type":"hello","topics":[{}]}}"#,
                vec![r#""a""#; MAX_CLERK_TOPICS + 1].join(",")
            ),
        ] {
            assert_eq!(parse_library_frame(refused.as_bytes()), None, "{refused}");
        }
    }

    #[test]
    fn a_page_with_a_malformed_work_is_refused() {
        let mut bad = work();
        bad.editions[0].availability = Availability::Reachable;
        let frame = LibraryFrame::BooksPage {
            after: 0,
            works: vec![bad],
            next: None,
        };
        assert_eq!(parse_library_frame(frame.encode().as_bytes()), None);
    }
}
