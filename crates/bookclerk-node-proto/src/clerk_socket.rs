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
//! | clerk → library | `books` | `after` | the page of the library's books after this cursor ([`WindowCursor`]; empty for the first) |
//! | library → clerk | `books.page` | `after`, `works`, `next?` | that page ([`WindowWork`]); no `next` on the last page |
//! | clerk → library | `copy.grant` | `store`, `marketplace?`, `product_id` | ask for the library's copy of the edition that store listing names ([`crate::copy_grant_problem`]) |
//! | library → clerk | `copy.granted` | `store`, `marketplace?`, `product_id`, `copy_id`, `token`, `expires_in_secs`, `copy`, `files` | the copy may be downloaded ([`ClerkCopy`]) |
//! | library → clerk | `error` | `code` | a `books` or `copy.grant` frame was not answered ([`ClerkSocketError`]) |
//!
//! Answers come in the order the frames were asked. A page echoes the
//! `after` it answers and a granted copy echoes its listing.
//!
//! A clerk has no box key, so the copy token is frame text here, where a
//! node's is sealed: the socket is the card's own TLS session (the
//! transport policy admits plain HTTP on loopback only), and the token is
//! good for one hour, for that card's person and that edition.
//!
//! A page carries what a window's `window.page` carries: works
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

use crate::{
    copy_grant_problem, copy_granted_problem, window_page_problem, CopyFile, CopyGrantedFrame,
    WindowCopy, WindowCursor, WindowWork, MAX_FRAME_BYTES,
};

/// Path of the clerk socket.
pub const CLERK_CONNECT_PATH: &str = "/api/clerk/v1/connect";

/// The WebSocket subprotocol a clerk must offer.
pub const CLERK_SUBPROTOCOL: &str = "bookclerk-clerk.v1";

/// The `v` every frame carries.
pub const CLERK_SOCKET_VERSION: u8 = 1;

/// Largest frame a library accepts from a clerk, in bytes; a `books` frame
/// is under 120.
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
    /// The library's books changed: read them again.
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

/// Why the library did not answer a `books` or `copy.grant` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClerkSocketError {
    /// The clerk sent a frame the library could not read.
    Invalid,
    /// The library could not read the books just now; ask again later.
    Unavailable,
    /// `copy.grant` only: the person may not take that edition, or the
    /// library holds no copy of it. One code for both.
    NotFound,
}

/// A copy token as a clerk socket frame carries it: the bearer of the
/// download endpoint ([`crate::COPY_AUTH_SCHEME`]). Its `Debug` prints no
/// token, and the text is zeroized when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct CopyToken(zeroize::Zeroizing<String>);

impl CopyToken {
    /// Wraps `token`.
    #[must_use]
    pub fn new(token: &str) -> Self {
        Self(zeroize::Zeroizing::new(token.to_owned()))
    }

    /// The token text, for the `Authorization` header.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Serialize for CopyToken {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}

impl<'de> Deserialize<'de> for CopyToken {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The string's buffer moves into the zeroizing wrapper; no second
        // copy of the token is made.
        String::deserialize(deserializer).map(|token| Self(zeroize::Zeroizing::new(token)))
    }
}

impl std::fmt::Debug for CopyToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CopyToken(<redacted>)")
    }
}

/// What a `copy.granted` frame carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClerkCopy {
    /// Store id of the listing the clerk asked with.
    pub store: String,
    /// That listing's marketplace; empty for a store without marketplaces.
    pub marketplace: String,
    /// That listing's product id.
    pub product_id: String,
    /// Copy id; the files are at `COPIES_PATH<copy_id>/files/<n>`.
    pub copy_id: String,
    /// The bearer token for the download endpoint.
    pub token: CopyToken,
    /// Seconds the token stays valid.
    pub expires_in_secs: u64,
    /// What the copy is, as the library's page states it.
    pub copy: WindowCopy,
    /// The copy's files; a file's index here is its `<n>`.
    pub files: Vec<CopyFile>,
}

impl ClerkCopy {
    /// Why the grant is malformed, or `None`: a listing that passes
    /// [`copy_grant_problem`] and the rest passing
    /// [`copy_granted_problem`], the token held to the same rule as a
    /// node's sealed one (1 to 1024 base64url characters).
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        copy_grant_problem(&self.store, &self.marketplace, &self.product_id).or_else(|| {
            copy_granted_problem(&CopyGrantedFrame {
                copy_id: &self.copy_id,
                sealed_token: self.token.expose(),
                expires_in_secs: self.expires_in_secs,
                copy: &self.copy,
                files: &self.files,
            })
        })
    }
}

/// A frame from a clerk to a library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClerkFrame {
    /// `books`: the page of the library's books after `after` (empty for
    /// the first page).
    Books {
        /// The cursor of the previous page.
        after: WindowCursor,
    },
    /// `copy.grant`: ask for the library's copy of one edition, named by
    /// its store listing; the person is the card's.
    CopyGrant {
        /// Store id of the edition's listing.
        store: String,
        /// The listing's marketplace; empty for a store without
        /// marketplaces.
        marketplace: String,
        /// The listing's product id.
        product_id: String,
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
    /// `books.page`: one page of the library's books, the card's person's
    /// own marked ([`crate::WindowEdition::yours`]).
    BooksPage {
        /// The cursor the clerk asked with.
        after: WindowCursor,
        /// At most [`crate::MAX_WINDOW_WORKS`] works.
        works: Vec<WindowWork>,
        /// The cursor of the next page; `None` on the last page.
        next: Option<WindowCursor>,
    },
    /// `copy.granted`: the copy may be downloaded.
    CopyGranted(Box<ClerkCopy>),
    /// `error`: a `books` or `copy.grant` frame was not answered.
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
    /// Frame type; `books` or `copy.grant`.
    #[serde(rename = "type")]
    kind: String,
    /// `books`: the cursor of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<WindowCursor>,
    /// `copy.grant`: the listing's store id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    store: Option<String>,
    /// `copy.grant`: the listing's marketplace, absent when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    marketplace: Option<String>,
    /// `copy.grant`: the listing's product id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    product_id: Option<String>,
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
    after: Option<WindowCursor>,
    /// `books.page`: the works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    works: Option<Vec<WindowWork>>,
    /// `books.page`: the next cursor, absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next: Option<WindowCursor>,
    /// `error`: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<ClerkSocketError>,
    /// `copy.granted`: the listing's store id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    store: Option<String>,
    /// `copy.granted`: the listing's marketplace, absent when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    marketplace: Option<String>,
    /// `copy.granted`: the listing's product id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    product_id: Option<String>,
    /// `copy.granted`: the copy id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copy_id: Option<String>,
    /// `copy.granted`: the download token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<CopyToken>,
    /// `copy.granted`: seconds the token stays valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_in_secs: Option<u64>,
    /// `copy.granted`: what the copy is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copy: Option<WindowCopy>,
    /// `copy.granted`: the copy's files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    files: Option<Vec<CopyFile>>,
}

/// A marketplace as a wire field: absent when empty.
fn marketplace_field(marketplace: &str) -> Option<String> {
    (!marketplace.is_empty()).then(|| marketplace.to_owned())
}

impl ClerkFrame {
    /// The frame's JSON text, as a clerk sends it.
    #[must_use]
    pub fn encode(&self) -> String {
        let wire = match self {
            Self::Books { after } => ClerkWire {
                v: CLERK_SOCKET_VERSION,
                kind: "books".into(),
                after: Some(after.clone()),
                store: None,
                marketplace: None,
                product_id: None,
            },
            Self::CopyGrant {
                store,
                marketplace,
                product_id,
            } => ClerkWire {
                v: CLERK_SOCKET_VERSION,
                kind: "copy.grant".into(),
                after: None,
                store: Some(store.clone()),
                marketplace: marketplace_field(marketplace),
                product_id: Some(product_id.clone()),
            },
        };
        // A struct of an integer and strings always serializes.
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
            store: None,
            marketplace: None,
            product_id: None,
            copy_id: None,
            token: None,
            expires_in_secs: None,
            copy: None,
            files: None,
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
                wire.after = Some(after.clone());
                wire.works = Some(works.clone());
                wire.next = next.clone();
            }
            Self::CopyGranted(granted) => {
                wire.kind = "copy.granted".into();
                wire.store = Some(granted.store.clone());
                wire.marketplace = marketplace_field(&granted.marketplace);
                wire.product_id = Some(granted.product_id.clone());
                wire.copy_id = Some(granted.copy_id.clone());
                wire.token = Some(granted.token.clone());
                wire.expires_in_secs = Some(granted.expires_in_secs);
                wire.copy = Some(granted.copy.clone());
                wire.files = Some(granted.files.clone());
            }
            Self::Error { code } => {
                wire.kind = "error".into();
                wire.code = Some(*code);
            }
        }
        // Strings, integers and the derived works and files always
        // serialize.
        serde_json::to_string(&wire).unwrap_or_default()
    }
}

/// Reads a frame a clerk sent: at most [`MAX_CLERK_FRAME_BYTES`], one JSON
/// object with `v` (1), a known `type`, and exactly that type's fields,
/// each once (a field written as `null` counts as absent).
///
/// - `books`: `after`, a string [`WindowCursor`].
/// - `copy.grant`: `store`, `product_id` and, for a store with
///   marketplaces, `marketplace`, passing [`copy_grant_problem`].
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
    if wire.v != CLERK_SOCKET_VERSION {
        return None;
    }
    let ClerkWire {
        kind,
        after,
        store,
        marketplace,
        product_id,
        ..
    } = wire;
    match kind.as_str() {
        "books" => {
            if store.is_some() || marketplace.is_some() || product_id.is_some() {
                return None;
            }
            Some(ClerkFrame::Books { after: after? })
        }
        "copy.grant" => {
            if after.is_some() {
                return None;
            }
            let (store, product_id) = (store?, product_id?);
            let marketplace = marketplace.unwrap_or_default();
            if copy_grant_problem(&store, &marketplace, &product_id).is_some() {
                return None;
            }
            Some(ClerkFrame::CopyGrant {
                store,
                marketplace,
                product_id,
            })
        }
        _ => None,
    }
}

/// Reads a frame a library sent: at most [`crate::MAX_FRAME_BYTES`], one
/// JSON object with `v` (1), a known `type`, and exactly that type's fields,
/// each once.
///
/// - `hello` and `changed`: `topics`, at most [`MAX_CLERK_TOPICS`] names,
///   each [`is_topic_name`]; names this version does not know are dropped.
/// - `books.page`: `after` (a [`WindowCursor`]), `works`, and `next` unless it is
///   the last page, passing [`crate::window_page_problem`].
/// - `copy.granted`: `store`, `product_id`, `copy_id`, `token`,
///   `expires_in_secs`, `copy`, `files` and, for a store with
///   marketplaces, `marketplace`, passing [`ClerkCopy::problem`].
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
        store,
        marketplace,
        product_id,
        copy_id,
        token,
        expires_in_secs,
        copy,
        files,
        ..
    } = wire;
    let page = after.is_some() || works.is_some() || next.is_some();
    let granted = store.is_some()
        || marketplace.is_some()
        || product_id.is_some()
        || copy_id.is_some()
        || token.is_some()
        || expires_in_secs.is_some()
        || copy.is_some()
        || files.is_some();
    match kind.as_str() {
        "hello" | "changed" => {
            if page || granted || code.is_some() {
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
            if topics.is_some() || code.is_some() || granted {
                return None;
            }
            let (after, works) = (after?, works?);
            if window_page_problem(&after, &works, next.as_ref()).is_some() {
                return None;
            }
            Some(LibraryFrame::BooksPage { after, works, next })
        }
        "copy.granted" => {
            if topics.is_some() || code.is_some() || page {
                return None;
            }
            let granted = ClerkCopy {
                store: store?,
                marketplace: marketplace.unwrap_or_default(),
                product_id: product_id?,
                copy_id: copy_id?,
                token: token?,
                expires_in_secs: expires_in_secs?,
                copy: copy?,
                files: files?,
            };
            if granted.problem().is_some() {
                return None;
            }
            Some(LibraryFrame::CopyGranted(Box::new(granted)))
        }
        "error" => {
            if topics.is_some() || page || granted {
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
    use crate::WindowEdition;

    fn at(text: &str) -> WindowCursor {
        WindowCursor::parse(text).unwrap()
    }

    fn work() -> WindowWork {
        WindowWork {
            work: "w-1".into(),
            identifiers: Vec::new(),
            editions: vec![WindowEdition {
                edition: LookupEdition {
                    store: "audible".into(),
                    marketplace: "us".into(),
                    product_id: "B0TEST".into(),
                    availability: Availability::Owned,
                    formats: vec![LookupFormat::Audio],
                    metadata: CatalogMetadata {
                        title: "A Title".into(),
                        ..CatalogMetadata::default()
                    },
                },
                yours: true,
                copy: None,
            }],
        }
    }

    #[test]
    fn a_books_frame_round_trips_and_nothing_else_is_read() {
        let frame = ClerkFrame::Books { after: at("h-41") };
        assert_eq!(frame.encode(), r#"{"v":1,"type":"books","after":"h-41"}"#);
        assert_eq!(parse_clerk_frame(frame.encode().as_bytes()), Some(frame));
        assert_eq!(
            parse_clerk_frame(br#"{"v":1,"type":"books","after":""}"#),
            Some(ClerkFrame::Books {
                after: WindowCursor::start()
            })
        );
        for refused in [
            r#"{"v":1,"type":"books","after":0}"#,
            r#"{"v":1,"type":"books","after":"a b"}"#,
            r#"{"v":2,"type":"books","after":""}"#,
            r#"{"v":1,"type":"claim","after":""}"#,
            r#"{"v":1,"type":"books"}"#,
            r#"{"v":1,"type":"books","after":"","user":7}"#,
            r#"{"v":1,"type":"books","after":"","after":"a"}"#,
            r#"{"v":1,"type":"books","after":null}"#,
            r#"[1,"books",""]"#,
            r#" [1,"books",""]"#,
            "",
        ] {
            assert_eq!(parse_clerk_frame(refused.as_bytes()), None, "{refused}");
        }
        let padded = format!(
            r#"{{"v":1,"type":"books","after":""}}{}"#,
            " ".repeat(MAX_CLERK_FRAME_BYTES)
        );
        assert_eq!(parse_clerk_frame(padded.as_bytes()), None);
    }

    fn granted() -> ClerkCopy {
        ClerkCopy {
            store: "audible".into(),
            marketplace: "us".into(),
            product_id: "B0TEST".into(),
            copy_id: "6f1c2b7e-0d4a-4e8b-9a51-2c3d4e5f6a7b".into(),
            token: CopyToken::new("dG9rZW4"),
            expires_in_secs: 3600,
            copy: WindowCopy {
                source_store: None,
                content_type: "audio/mp4".into(),
                bitrate_kbps: Some(64),
                lossless: false,
                version: "v1".into(),
            },
            files: vec![CopyFile {
                name: "book.m4b".into(),
                role: crate::CopyRole::Audio,
                size: 10,
                content_type: Some("audio/mp4".into()),
                probe: None,
            }],
        }
    }

    #[test]
    fn a_copy_grant_round_trips_and_is_held_to_the_listing_rules() {
        let frame = ClerkFrame::CopyGrant {
            store: "audible".into(),
            marketplace: "us".into(),
            product_id: "B0TEST".into(),
        };
        assert_eq!(
            frame.encode(),
            r#"{"v":1,"type":"copy.grant","store":"audible","marketplace":"us","product_id":"B0TEST"}"#
        );
        assert_eq!(parse_clerk_frame(frame.encode().as_bytes()), Some(frame));
        let bare = ClerkFrame::CopyGrant {
            store: "libro".into(),
            marketplace: String::new(),
            product_id: "9781".into(),
        };
        assert_eq!(
            bare.encode(),
            r#"{"v":1,"type":"copy.grant","store":"libro","product_id":"9781"}"#
        );
        assert_eq!(parse_clerk_frame(bare.encode().as_bytes()), Some(bare));
        for refused in [
            r#"{"v":1,"type":"copy.grant","store":"audible"}"#,
            r#"{"v":1,"type":"copy.grant","product_id":"B0TEST"}"#,
            r#"{"v":1,"type":"copy.grant","store":"","product_id":"B0TEST"}"#,
            r#"{"v":1,"type":"copy.grant","store":"audible","product_id":"B0TEST","after":""}"#,
            r#"{"v":1,"type":"copy.grant","store":"audible","product_id":"B0TEST","user":"u"}"#,
            r#"{"v":1,"type":"copy.grant","store":"audible","product_id":7}"#,
            r#"{"v":1,"type":"books","after":"","store":"audible"}"#,
        ] {
            assert_eq!(parse_clerk_frame(refused.as_bytes()), None, "{refused}");
        }
    }

    #[test]
    fn a_granted_copy_round_trips_and_a_malformed_one_is_refused() {
        let frame = LibraryFrame::CopyGranted(Box::new(granted()));
        let text = frame.encode();
        assert!(text.contains(r#""token":"dG9rZW4""#), "{text}");
        assert_eq!(parse_library_frame(text.as_bytes()), Some(frame));

        let not_found = LibraryFrame::Error {
            code: ClerkSocketError::NotFound,
        };
        assert_eq!(
            not_found.encode(),
            r#"{"v":1,"type":"error","code":"not_found"}"#
        );
        assert_eq!(
            parse_library_frame(not_found.encode().as_bytes()),
            Some(not_found)
        );

        let broken: [fn(&mut ClerkCopy); 5] = [
            |copy| copy.store.clear(),
            |copy| copy.copy_id = "short".into(),
            |copy| copy.token = CopyToken::new(""),
            |copy| copy.expires_in_secs = 0,
            |copy| copy.files.clear(),
        ];
        for breaks in broken {
            let mut copy = granted();
            breaks(&mut copy);
            let text = LibraryFrame::CopyGranted(Box::new(copy)).encode();
            assert_eq!(parse_library_frame(text.as_bytes()), None, "{text}");
        }
        // A grant's fields on another frame, and another frame's on a grant.
        let with = |extra: &str| text.replacen('{', &format!("{{{extra},"), 1);
        assert_eq!(parse_library_frame(with(r#""after":"""#).as_bytes()), None);
        assert_eq!(
            parse_library_frame(with(r#""code":"invalid""#).as_bytes()),
            None
        );
        assert_eq!(
            parse_library_frame(br#"{"v":1,"type":"error","code":"invalid","copy_id":"x"}"#),
            None
        );
    }

    #[test]
    fn a_copy_token_prints_no_token() {
        let shown = format!("{:?}", granted());
        assert!(!shown.contains("dG9rZW4"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
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
                after: WindowCursor::start(),
                works: vec![work()],
                next: Some(at("9")),
            },
            LibraryFrame::BooksPage {
                after: at("9"),
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
        let page = |after: &str, next: &str| {
            format!(r#"{{"v":1,"type":"books.page","after":{after},"works":[]{next}}}"#)
        };
        assert!(parse_library_frame(page(r#""3""#, "").as_bytes()).is_some());
        for refused in [
            r#"{"v":1,"type":"changed","topics":["Library"]}"#.to_owned(),
            r#"{"v":1,"type":"changed"}"#.to_owned(),
            r#"{"v":1,"type":"changed","topics":[],"code":"invalid"}"#.to_owned(),
            r#"{"v":1,"type":"error","code":"later"}"#.to_owned(),
            r#"{"v":1,"type":"work","topics":[]}"#.to_owned(),
            r#"{"v":2,"type":"hello","topics":[]}"#.to_owned(),
            page(r#""3""#, r#","next":"3""#),
            page(r#""3""#, r#","next":"2""#),
            page(r#""3""#, r#","next":4"#),
            page("3", ""),
            page(r#""a/b""#, ""),
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
        bad.editions[0].edition.availability = Availability::Reachable;
        let frame = LibraryFrame::BooksPage {
            after: WindowCursor::start(),
            works: vec![bad],
            next: None,
        };
        assert_eq!(parse_library_frame(frame.encode().as_bytes()), None);
    }
}
