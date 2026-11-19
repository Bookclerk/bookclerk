//! What a player sends the playback routes, parsed sans IO so the routes,
//! the unit tests and the `playback_request` fuzz target drive the same
//! code (`docs/adr/untrusted-input-fuzzing.md`):
//!
//! - the body of `POST /api/playback/sessions/{id}/sync` and `.../close`
//!   ([`parse_playback_sync_request`]), which moves a person's progress;
//! - the bodies that open a session, set progress outright and add or
//!   change a bookmark ([`parse_open_session_request`],
//!   [`parse_progress_edit_request`], [`parse_new_bookmark_request`],
//!   [`parse_bookmark_edit_request`]);
//! - the `cursor` of `GET /api/playback/progress`
//!   ([`parse_progress_cursor`]);
//! - the path of `GET /api/playback/sessions/{id}/tracks/{n}`
//!   ([`parse_track_path`]), which names the file a stream request reads.
//!   Its `Range` header is `bookclerk_node_proto::parse_copy_range`'s.

use serde::Deserialize;

use super::playback::{
    PlaybackMs, PlaybackProgressEdit, PlaybackSync, PlayerKind, ProgressCursor,
    MAX_BOOKMARK_TITLE_BYTES, MAX_PLAYBACK_MS, MAX_PLAYER_NAME_BYTES,
};
use crate::error::{LibraryError, Result};

/// Largest sync body the routes read, in bytes; a real one is under 100.
pub const MAX_PLAYBACK_SYNC_BYTES: usize = 1024;

/// Most tracks a play session lists: a stream path's index is below it.
pub const MAX_PLAYBACK_TRACKS: usize = 512;

/// `POST /api/playback/sessions/{id}/sync` and `.../close` body: where
/// the listener is.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `PlaybackSyncFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct PlaybackSyncBody {
    /// Position on the edition's timeline, in milliseconds from its start
    /// (at most sixty days).
    #[schema(example = 754_000)]
    position_ms: u64,
    /// Milliseconds listened since this session's last report, as the
    /// player counted them; left out reads as 0.
    #[schema(example = 15_000)]
    listened_ms: u64,
    /// The edition's whole runtime in milliseconds, when the player knows
    /// it; left out or `null` keeps the stored one.
    #[schema(example = 41_400_000)]
    duration_ms: Option<u64>,
}

/// [`PlaybackSyncBody`]'s fields as JSON carries them; read only from a
/// JSON object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaybackSyncFields {
    /// See [`PlaybackSyncBody::position_ms`].
    position_ms: u64,
    /// See [`PlaybackSyncBody::listened_ms`].
    #[serde(default)]
    listened_ms: u64,
    /// See [`PlaybackSyncBody::duration_ms`].
    #[serde(default)]
    duration_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for PlaybackSyncBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: PlaybackSyncFields =
            crate::control_plane::from_object(deserializer, "a JSON object with position_ms")?;
        Ok(Self {
            position_ms: fields.position_ms,
            listened_ms: fields.listened_ms,
            duration_ms: fields.duration_ms,
        })
    }
}

impl PlaybackSyncBody {
    /// Checks the body: every length at most [`MAX_PLAYBACK_MS`].
    ///
    /// # Arguments
    ///
    /// * `close` - True for the `close` route: the report ends the session.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] naming the field that is too
    /// large.
    pub fn validate(self, close: bool) -> Result<PlaybackSync> {
        let length = |field: &str, ms: u64| {
            PlaybackMs::new(ms).ok_or_else(|| {
                LibraryError::Invalid(format!("{field} must be at most {MAX_PLAYBACK_MS}"))
            })
        };
        Ok(PlaybackSync {
            position: length("position_ms", self.position_ms)?,
            listened: length("listened_ms", self.listened_ms)?,
            duration: self
                .duration_ms
                .map(|ms| length("duration_ms", ms))
                .transpose()?,
            close,
        })
    }
}

/// Parses and validates a sync or close body from its raw bytes, sans IO,
/// the way the routes read it: JSON into [`PlaybackSyncBody`], then
/// [`PlaybackSyncBody::validate`].
///
/// # Arguments
///
/// * `body` - The request body.
/// * `close` - True for the `close` route.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a body over
/// [`MAX_PLAYBACK_SYNC_BYTES`], bytes that are not the body's JSON shape
/// (unknown or repeated fields, wrong types, a missing `position_ms`, an
/// array instead of an object) or a length past [`MAX_PLAYBACK_MS`].
pub fn parse_playback_sync_request(body: &[u8], close: bool) -> Result<PlaybackSync> {
    if body.len() > MAX_PLAYBACK_SYNC_BYTES {
        return Err(LibraryError::Invalid(format!(
            "a sync body is at most {MAX_PLAYBACK_SYNC_BYTES} bytes"
        )));
    }
    serde_json::from_slice::<PlaybackSyncBody>(body)
        .map_err(|err| LibraryError::Invalid(format!("sync body: {err}")))?
        .validate(close)
}

/// Largest body the other playback parsers read, in bytes: room for an
/// escaped bookmark title or a long player name.
pub const MAX_PLAYBACK_BODY_BYTES: usize = 4096;

/// Longest edition id a session is opened for, in bytes; a real one is a
/// 36-byte UUID.
pub const MAX_EDITION_ID_BYTES: usize = 128;

/// A body length as a timeline length.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] naming `field` when `ms` is past
/// [`MAX_PLAYBACK_MS`].
fn length(field: &str, ms: u64) -> Result<PlaybackMs> {
    PlaybackMs::new(ms)
        .ok_or_else(|| LibraryError::Invalid(format!("{field} must be at most {MAX_PLAYBACK_MS}")))
}

/// Reads one of the bodies below from raw bytes: at most
/// [`MAX_PLAYBACK_BODY_BYTES`] of JSON in the body's shape.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] naming `what` for a longer body or
/// bytes that are not the body's JSON shape.
fn read_body<'de, T: Deserialize<'de>>(body: &'de [u8], what: &str) -> Result<T> {
    if body.len() > MAX_PLAYBACK_BODY_BYTES {
        return Err(LibraryError::Invalid(format!(
            "{what} is at most {MAX_PLAYBACK_BODY_BYTES} bytes"
        )));
    }
    serde_json::from_slice(body).map_err(|err| LibraryError::Invalid(format!("{what}: {err}")))
}

/// The kind of first-party player that opens a session. Outside player
/// apps open theirs through the player API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlayerKindParam {
    /// The web app in a browser.
    #[default]
    Web,
    /// The desktop window.
    Desktop,
}

impl From<PlayerKindParam> for PlayerKind {
    fn from(value: PlayerKindParam) -> Self {
        match value {
            PlayerKindParam::Web => Self::Web,
            PlayerKindParam::Desktop => Self::Desktop,
        }
    }
}

/// `POST /api/playback/sessions` body.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `OpenSessionFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct OpenSessionBody {
    /// The edition to play (1 to 128 bytes).
    #[schema(example = "5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d")]
    edition_id: String,
    /// What kind of player asks; `web` when left out.
    #[serde(default)]
    player: PlayerKindParam,
    /// The player's own name, shown in listening history. Control
    /// characters are dropped and at most 64 bytes are kept.
    #[serde(default)]
    #[schema(example = "Firefox on Linux")]
    player_name: String,
}

/// [`OpenSessionBody`]'s fields as JSON carries them; read only from a
/// JSON object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenSessionFields {
    /// See [`OpenSessionBody::edition_id`].
    edition_id: String,
    /// See [`OpenSessionBody::player`].
    #[serde(default)]
    player: PlayerKindParam,
    /// See [`OpenSessionBody::player_name`].
    #[serde(default)]
    player_name: String,
}

impl<'de> Deserialize<'de> for OpenSessionBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: OpenSessionFields =
            crate::control_plane::from_object(deserializer, "a JSON object with edition_id")?;
        Ok(Self {
            edition_id: fields.edition_id,
            player: fields.player,
            player_name: fields.player_name,
        })
    }
}

/// A checked request to open a play session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSessionRequest {
    /// The edition to play, as sent.
    pub edition_id: String,
    /// What kind of player asks.
    pub player: PlayerKind,
    /// The player's name as stored: no control characters, trimmed, at
    /// most [`MAX_PLAYER_NAME_BYTES`] bytes.
    pub player_name: String,
}

impl OpenSessionBody {
    /// Checks the body and tidies the player's name.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for an empty edition id or one
    /// over [`MAX_EDITION_ID_BYTES`].
    pub fn validate(self) -> Result<OpenSessionRequest> {
        if self.edition_id.is_empty() || self.edition_id.len() > MAX_EDITION_ID_BYTES {
            return Err(LibraryError::Invalid(format!(
                "edition_id must be 1 to {MAX_EDITION_ID_BYTES} bytes"
            )));
        }
        let name: String = self
            .player_name
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let name = name.trim();
        let mut end = name.len().min(MAX_PLAYER_NAME_BYTES);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        Ok(OpenSessionRequest {
            edition_id: self.edition_id,
            player: self.player.into(),
            player_name: name[..end].to_string(),
        })
    }
}

/// Parses and validates a `POST /api/playback/sessions` body from its raw
/// bytes, sans IO: JSON into [`OpenSessionBody`], then
/// [`OpenSessionBody::validate`].
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a body over
/// [`MAX_PLAYBACK_BODY_BYTES`], bytes that are not the body's JSON shape
/// (unknown or repeated fields, wrong types, a missing `edition_id`, an
/// unknown `player`, an array instead of an object) or an edition id that
/// is empty or too long.
pub fn parse_open_session_request(body: &[u8]) -> Result<OpenSessionRequest> {
    read_body::<OpenSessionBody>(body, "session body")?.validate()
}

/// `PUT /api/playback/progress/{edition_id}` body: what to set. A field
/// left out keeps its stored value.
#[derive(Debug, Default, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `ProgressEditFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct ProgressEditBody {
    /// Position in milliseconds (at most sixty days).
    #[serde(default)]
    #[schema(example = 0)]
    position_ms: Option<u64>,
    /// The edition's runtime in milliseconds, when the page knows it.
    #[serde(default)]
    #[schema(example = 41_400_000)]
    duration_ms: Option<u64>,
    /// True marks the edition and its work finished; false clears the
    /// edition's flag (the work's stays, and is changed with the work's
    /// own state). Left out, the position decides.
    #[serde(default)]
    #[schema(example = true)]
    is_finished: Option<bool>,
}

/// [`ProgressEditBody`]'s fields as JSON carries them; read only from a
/// JSON object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgressEditFields {
    /// See [`ProgressEditBody::position_ms`].
    #[serde(default)]
    position_ms: Option<u64>,
    /// See [`ProgressEditBody::duration_ms`].
    #[serde(default)]
    duration_ms: Option<u64>,
    /// See [`ProgressEditBody::is_finished`].
    #[serde(default)]
    is_finished: Option<bool>,
}

impl<'de> Deserialize<'de> for ProgressEditBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: ProgressEditFields =
            crate::control_plane::from_object(deserializer, "a JSON object")?;
        Ok(Self {
            position_ms: fields.position_ms,
            duration_ms: fields.duration_ms,
            is_finished: fields.is_finished,
        })
    }
}

impl ProgressEditBody {
    /// Checks the body: every length at most [`MAX_PLAYBACK_MS`].
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] naming the field that is too
    /// large.
    pub fn validate(self) -> Result<PlaybackProgressEdit> {
        Ok(PlaybackProgressEdit {
            position: self
                .position_ms
                .map(|ms| length("position_ms", ms))
                .transpose()?,
            duration: self
                .duration_ms
                .map(|ms| length("duration_ms", ms))
                .transpose()?,
            finished: self.is_finished,
        })
    }
}

/// Parses and validates a `PUT /api/playback/progress/{edition_id}` body
/// from its raw bytes, sans IO.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a body over
/// [`MAX_PLAYBACK_BODY_BYTES`], bytes that are not the body's JSON shape
/// or a length past [`MAX_PLAYBACK_MS`].
pub fn parse_progress_edit_request(body: &[u8]) -> Result<PlaybackProgressEdit> {
    read_body::<ProgressEditBody>(body, "progress body")?.validate()
}

/// A bookmark title as stored.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] when the trimmed title is over
/// [`MAX_BOOKMARK_TITLE_BYTES`] or has a control character.
fn bookmark_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.len() > MAX_BOOKMARK_TITLE_BYTES {
        return Err(LibraryError::Invalid(format!(
            "title must be at most {MAX_BOOKMARK_TITLE_BYTES} bytes"
        )));
    }
    if title.chars().any(char::is_control) {
        return Err(LibraryError::Invalid(
            "title must not contain control characters".into(),
        ));
    }
    Ok(title.to_string())
}

/// `POST /api/library/editions/{id}/bookmarks` body.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `NewBookmarkFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct NewBookmarkBody {
    /// The place to mark, in milliseconds (at most sixty days).
    #[schema(example = 754_000)]
    position_ms: u64,
    /// A note for it, at most 200 bytes, no control characters; empty
    /// when left out.
    #[serde(default)]
    #[schema(example = "The storm")]
    title: String,
}

/// [`NewBookmarkBody`]'s fields as JSON carries them; read only from a
/// JSON object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewBookmarkFields {
    /// See [`NewBookmarkBody::position_ms`].
    position_ms: u64,
    /// See [`NewBookmarkBody::title`].
    #[serde(default)]
    title: String,
}

impl<'de> Deserialize<'de> for NewBookmarkBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: NewBookmarkFields =
            crate::control_plane::from_object(deserializer, "a JSON object with position_ms")?;
        Ok(Self {
            position_ms: fields.position_ms,
            title: fields.title,
        })
    }
}

/// A checked bookmark to add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBookmarkRequest {
    /// The place to mark.
    pub position: PlaybackMs,
    /// The note, trimmed; may be empty.
    pub title: String,
}

impl NewBookmarkBody {
    /// Checks the body.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for a position past
    /// [`MAX_PLAYBACK_MS`], or a title that is too long or has a control
    /// character.
    pub fn validate(self) -> Result<NewBookmarkRequest> {
        Ok(NewBookmarkRequest {
            position: length("position_ms", self.position_ms)?,
            title: bookmark_title(&self.title)?,
        })
    }
}

/// Parses and validates a `POST /api/library/editions/{id}/bookmarks`
/// body from its raw bytes, sans IO.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a body over
/// [`MAX_PLAYBACK_BODY_BYTES`], bytes that are not the body's JSON shape,
/// a position past [`MAX_PLAYBACK_MS`] or a refused title.
pub fn parse_new_bookmark_request(body: &[u8]) -> Result<NewBookmarkRequest> {
    read_body::<NewBookmarkBody>(body, "bookmark body")?.validate()
}

/// `PUT /api/playback/bookmarks/{id}` body: what to change. A field left
/// out keeps its stored value.
#[derive(Debug, Default, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `BookmarkEditFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct BookmarkEditBody {
    /// The new place, in milliseconds.
    #[serde(default)]
    #[schema(example = 760_000)]
    position_ms: Option<u64>,
    /// The new note; an empty string clears it.
    #[serde(default)]
    #[schema(example = "The storm breaks")]
    title: Option<String>,
}

/// [`BookmarkEditBody`]'s fields as JSON carries them; read only from a
/// JSON object, never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BookmarkEditFields {
    /// See [`BookmarkEditBody::position_ms`].
    #[serde(default)]
    position_ms: Option<u64>,
    /// See [`BookmarkEditBody::title`].
    #[serde(default)]
    title: Option<String>,
}

impl<'de> Deserialize<'de> for BookmarkEditBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: BookmarkEditFields =
            crate::control_plane::from_object(deserializer, "a JSON object")?;
        Ok(Self {
            position_ms: fields.position_ms,
            title: fields.title,
        })
    }
}

/// A checked change to a bookmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkEditRequest {
    /// The new place; `None` keeps it.
    pub position: Option<PlaybackMs>,
    /// The new note, trimmed; `None` keeps it.
    pub title: Option<String>,
}

impl BookmarkEditBody {
    /// Checks the body.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for a position past
    /// [`MAX_PLAYBACK_MS`], or a title that is too long or has a control
    /// character.
    pub fn validate(self) -> Result<BookmarkEditRequest> {
        Ok(BookmarkEditRequest {
            position: self
                .position_ms
                .map(|ms| length("position_ms", ms))
                .transpose()?,
            title: self.title.as_deref().map(bookmark_title).transpose()?,
        })
    }
}

/// Parses and validates a `PUT /api/playback/bookmarks/{id}` body from
/// its raw bytes, sans IO.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a body over
/// [`MAX_PLAYBACK_BODY_BYTES`], bytes that are not the body's JSON shape,
/// a position past [`MAX_PLAYBACK_MS`] or a refused title.
pub fn parse_bookmark_edit_request(body: &[u8]) -> Result<BookmarkEditRequest> {
    read_body::<BookmarkEditBody>(body, "bookmark body")?.validate()
}

/// Longest progress cursor token the route reads, in bytes; a real one is
/// about 100.
pub const MAX_PROGRESS_CURSOR_BYTES: usize = 256;

/// The opaque token of a progress cursor: unpadded URL-safe Base64 of
/// the row's change time, a `|`, and its edition id.
#[must_use]
pub fn progress_cursor_token(cursor: &ProgressCursor) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("{}|{}", cursor.updated_at, cursor.edition_id))
}

/// True when `text` has the shape of a stored change time: an RFC 3339
/// date and time with up to nine fraction digits and either `Z` or a
/// numeric offset. The cursor only compares it with stored text, so the
/// shape is checked and the calendar is not.
fn is_stamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |from: usize, to: usize| {
        bytes
            .get(from..to)
            .is_some_and(|run| run.iter().all(u8::is_ascii_digit))
    };
    let is = |at: usize, byte: u8| bytes.get(at) == Some(&byte);
    let date_time = digits(0, 4)
        && is(4, b'-')
        && digits(5, 7)
        && is(7, b'-')
        && digits(8, 10)
        && is(10, b'T')
        && digits(11, 13)
        && is(13, b':')
        && digits(14, 16)
        && is(16, b':')
        && digits(17, 19);
    if !date_time {
        return false;
    }
    let mut rest = &bytes[19..];
    if let Some((b'.', fraction)) = rest.split_first() {
        let count = fraction.iter().take_while(|b| b.is_ascii_digit()).count();
        if count == 0 || count > 9 {
            return false;
        }
        rest = &fraction[count..];
    }
    match rest {
        [b'Z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => {
            [h1, h2, m1, m2].iter().all(|byte| byte.is_ascii_digit())
        }
        _ => false,
    }
}

/// Parses the `cursor` of `GET /api/playback/progress`, sans IO.
///
/// # Arguments
///
/// * `token` - The query value as sent.
///
/// # Returns
///
/// `None` unless `token` is at most [`MAX_PROGRESS_CURSOR_BYTES`] of
/// canonical unpadded URL-safe Base64 that decodes to UTF-8 text of a
/// change time (see [`progress_cursor_token`]), a `|` and an edition id
/// of 1 to [`MAX_EDITION_ID_BYTES`] bytes. An accepted token is exactly
/// what [`progress_cursor_token`] writes for the cursor it yields.
#[must_use]
pub fn parse_progress_cursor(token: &str) -> Option<ProgressCursor> {
    use base64::Engine as _;
    if token.len() > MAX_PROGRESS_CURSOR_BYTES {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (updated_at, edition_id) = text.split_once('|')?;
    let fits =
        is_stamp(updated_at) && !edition_id.is_empty() && edition_id.len() <= MAX_EDITION_ID_BYTES;
    fits.then(|| ProgressCursor {
        updated_at: updated_at.to_string(),
        edition_id: edition_id.to_string(),
    })
}

/// The file a stream request names ([`parse_track_path`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPath {
    /// The play session's id: a hyphenated lowercase UUID.
    pub session_id: String,
    /// The track's index in the session's list, below
    /// [`MAX_PLAYBACK_TRACKS`].
    pub index: usize,
}

/// Parses the two path parts of
/// `GET /api/playback/sessions/{id}/tracks/{n}`, sans IO.
///
/// # Arguments
///
/// * `session_id` - The `{id}` part as sent.
/// * `index` - The `{n}` part as sent.
///
/// # Returns
///
/// `None` unless `session_id` is a UUID in its canonical spelling (36
/// bytes, lowercase, hyphenated) and `index` is a decimal number below
/// [`MAX_PLAYBACK_TRACKS`] written with ASCII digits only and no leading
/// zero. Anything else names no session or track, and the route answers
/// 404 without reading the library.
#[must_use]
pub fn parse_track_path(session_id: &str, index: &str) -> Option<TrackPath> {
    let canonical = uuid::Uuid::try_parse(session_id)
        .ok()
        .filter(|uuid| uuid.hyphenated().to_string() == session_id)?;
    let digits = !index.is_empty()
        && index.len() <= 3
        && index.bytes().all(|byte| byte.is_ascii_digit())
        && (index == "0" || !index.starts_with('0'));
    let index = digits
        .then(|| index.parse::<usize>().ok())
        .flatten()
        .filter(|index| *index < MAX_PLAYBACK_TRACKS)?;
    Some(TrackPath {
        session_id: canonical.hyphenated().to_string(),
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_body_is_an_object_with_bounded_lengths() {
        let sync = parse_playback_sync_request(br#"{"position_ms": 754000}"#, false).unwrap();
        assert_eq!(sync.position.get(), 754_000);
        assert_eq!(sync.listened, PlaybackMs::ZERO);
        assert_eq!(sync.duration, None);
        assert!(!sync.close);
        let sync = parse_playback_sync_request(
            br#"{"position_ms": 0, "listened_ms": 15000, "duration_ms": 41400000}"#,
            true,
        )
        .unwrap();
        assert_eq!(sync.listened.get(), 15_000);
        assert_eq!(sync.duration.map(PlaybackMs::get), Some(41_400_000));
        assert!(sync.close);
        assert!(
            parse_playback_sync_request(br#"{"position_ms": 1, "duration_ms": null}"#, false)
                .unwrap()
                .duration
                .is_none()
        );
        for refused in [
            &b""[..],
            b"[1, 2, 3]",
            b"{}",
            br#"{"position_ms": -1}"#,
            br#"{"position_ms": 1.5}"#,
            br#"{"position_ms": "1"}"#,
            br#"{"position_ms": 1, "position_ms": 2}"#,
            br#"{"position_ms": 1, "speed": 2}"#,
            br#"{"position_ms": 5184000001}"#,
            br#"{"position_ms": 1, "listened_ms": 5184000001}"#,
            br#"{"position_ms": 1, "duration_ms": 5184000001}"#,
        ] {
            assert!(
                matches!(
                    parse_playback_sync_request(refused, false),
                    Err(LibraryError::Invalid(_))
                ),
                "{:?}",
                String::from_utf8_lossy(refused)
            );
        }
        let long = format!(
            r#"{{"position_ms": 1{}}}"#,
            " ".repeat(MAX_PLAYBACK_SYNC_BYTES)
        );
        assert!(parse_playback_sync_request(long.as_bytes(), false).is_err());
    }

    #[test]
    fn a_session_body_names_an_edition_and_tidies_the_players_name() {
        let open = parse_open_session_request(br#"{"edition_id": "ed1"}"#).unwrap();
        assert_eq!(
            open,
            OpenSessionRequest {
                edition_id: "ed1".into(),
                player: PlayerKind::Web,
                player_name: String::new(),
            }
        );
        let long = format!(
            r#"{{"edition_id": "ed1", "player": "desktop", "player_name": " a\u0007b{}"}}"#,
            "é".repeat(40)
        );
        let open = parse_open_session_request(long.as_bytes()).unwrap();
        assert_eq!(open.player, PlayerKind::Desktop);
        assert_eq!(open.player_name, format!("ab{}", "é".repeat(31)));
        let id = "e".repeat(MAX_EDITION_ID_BYTES);
        assert!(
            parse_open_session_request(format!(r#"{{"edition_id": "{id}"}}"#).as_bytes()).is_ok()
        );
        for refused in [
            &br#"["ed1"]"#[..],
            br#"{}"#,
            br#"{"edition_id": ""}"#,
            br#"{"edition_id": 7}"#,
            br#"{"edition_id": "ed1", "player": "app"}"#,
            br#"{"edition_id": "ed1", "player": null}"#,
            br#"{"edition_id": "ed1", "edition_id": "ed2"}"#,
            br#"{"edition_id": "ed1", "extra": 1}"#,
            format!(r#"{{"edition_id": "{id}e"}}"#).as_bytes(),
            format!(
                r#"{{"edition_id": "ed1"{}}}"#,
                " ".repeat(MAX_PLAYBACK_BODY_BYTES)
            )
            .as_bytes(),
        ] {
            assert!(
                matches!(
                    parse_open_session_request(refused),
                    Err(LibraryError::Invalid(_))
                ),
                "{:?}",
                String::from_utf8_lossy(refused)
            );
        }
    }

    #[test]
    fn a_progress_body_sets_only_what_it_names() {
        assert_eq!(
            parse_progress_edit_request(b"{}").unwrap(),
            PlaybackProgressEdit {
                position: None,
                duration: None,
                finished: None
            }
        );
        let edit = parse_progress_edit_request(
            br#"{"position_ms": 5, "duration_ms": null, "is_finished": false}"#,
        )
        .unwrap();
        assert_eq!(edit.position.map(PlaybackMs::get), Some(5));
        assert_eq!(edit.duration, None);
        assert_eq!(edit.finished, Some(false));
        for refused in [
            &b"[1, 2, true]"[..],
            br#"{"position_ms": 5184000001}"#,
            br#"{"duration_ms": 5184000001}"#,
            br#"{"is_finished": 1}"#,
            br#"{"finished": true}"#,
        ] {
            assert!(
                matches!(
                    parse_progress_edit_request(refused),
                    Err(LibraryError::Invalid(_))
                ),
                "{:?}",
                String::from_utf8_lossy(refused)
            );
        }
    }

    #[test]
    fn a_bookmark_body_has_a_place_and_a_short_plain_title() {
        let new =
            parse_new_bookmark_request(br#"{"position_ms": 9, "title": "  The storm "}"#).unwrap();
        assert_eq!(new.position.get(), 9);
        assert_eq!(new.title, "The storm");
        assert_eq!(
            parse_new_bookmark_request(br#"{"position_ms": 9}"#)
                .unwrap()
                .title,
            ""
        );
        let longest = "é".repeat(MAX_BOOKMARK_TITLE_BYTES / 2);
        assert!(parse_new_bookmark_request(
            format!(r#"{{"position_ms": 1, "title": "{longest}"}}"#).as_bytes()
        )
        .is_ok());
        for refused in [
            &b"[9]"[..],
            br#"{"title": "x"}"#,
            br#"{"position_ms": 5184000001}"#,
            br#"{"position_ms": 1, "title": "a\nb"}"#,
            br#"{"position_ms": 1, "title": null}"#,
            format!(r#"{{"position_ms": 1, "title": "{longest}x"}}"#).as_bytes(),
        ] {
            assert!(
                matches!(
                    parse_new_bookmark_request(refused),
                    Err(LibraryError::Invalid(_))
                ),
                "{:?}",
                String::from_utf8_lossy(refused)
            );
        }

        assert_eq!(
            parse_bookmark_edit_request(b"{}").unwrap(),
            BookmarkEditRequest {
                position: None,
                title: None
            }
        );
        let edit = parse_bookmark_edit_request(br#"{"position_ms": 3, "title": " "}"#).unwrap();
        assert_eq!(edit.position.map(PlaybackMs::get), Some(3));
        assert_eq!(edit.title.as_deref(), Some(""));
        assert_eq!(
            parse_bookmark_edit_request(br#"{"title": null}"#)
                .unwrap()
                .title,
            None
        );
        for refused in [
            &b"[]"[..],
            br#"{"position_ms": 5184000001}"#,
            br#"{"title": "a\u0000b"}"#,
            br#"{"note": "x"}"#,
        ] {
            assert!(
                matches!(
                    parse_bookmark_edit_request(refused),
                    Err(LibraryError::Invalid(_))
                ),
                "{:?}",
                String::from_utf8_lossy(refused)
            );
        }
    }

    #[test]
    fn a_progress_cursor_round_trips_and_refuses_what_the_route_never_made() {
        use base64::Engine as _;
        let token_of = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
        for stamp in [
            "2026-11-19T09:02:41+00:00",
            "2026-11-19T09:02:41.123456789+00:00",
            "2026-11-19T09:02:41.5Z",
            "2026-11-19T09:02:41-05:30",
        ] {
            let cursor = ProgressCursor {
                updated_at: stamp.into(),
                edition_id: "5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d".into(),
            };
            let token = progress_cursor_token(&cursor);
            assert_eq!(parse_progress_cursor(&token), Some(cursor), "{stamp}");
        }
        let long_id = "e".repeat(MAX_EDITION_ID_BYTES + 1);
        for refused in [
            String::new(),
            "not base64!".into(),
            token_of("no delimiter"),
            token_of("2026-11-19T09:02:41+00:00|"),
            token_of("yesterday|ed1"),
            token_of("2026-11-19 09:02:41+00:00|ed1"),
            token_of("2026-11-19T09:02:41.+00:00|ed1"),
            token_of("2026-11-19T09:02:41.1234567890Z|ed1"),
            token_of("2026-11-19T09:02:41|ed1"),
            token_of("2026-11-19T09:02:41+0000|ed1"),
            token_of(&format!("2026-11-19T09:02:41Z|{long_id}")),
            format!("{}=", token_of("2026-11-19T09:02:41Z|e")),
            "A".repeat(MAX_PROGRESS_CURSOR_BYTES + 4),
        ] {
            assert_eq!(parse_progress_cursor(&refused), None, "{refused}");
        }
    }

    #[test]
    fn a_track_path_is_a_canonical_uuid_and_a_small_index() {
        const ID: &str = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b";
        assert_eq!(
            parse_track_path(ID, "0"),
            Some(TrackPath {
                session_id: ID.into(),
                index: 0
            })
        );
        assert_eq!(
            parse_track_path(ID, "511").map(|path| path.index),
            Some(511)
        );
        for (id, index) in [
            (ID, "512"),
            (ID, "01"),
            (ID, "+1"),
            (ID, ""),
            (ID, "1 "),
            (ID, "٣"),
            (ID, "1000"),
            ("0B8F6A52-3C1D-4E2F-9A7B-6C5D4E3F2A1B", "0"),
            ("0b8f6a523c1d4e2f9a7b6c5d4e3f2a1b", "0"),
            ("{0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b}", "0"),
            ("", "0"),
            ("../copies", "0"),
        ] {
            assert_eq!(parse_track_path(id, index), None, "{id:?} {index:?}");
        }
    }
}
