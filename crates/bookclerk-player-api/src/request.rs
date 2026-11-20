//! Sans-IO parsers for what player apps send the `/player` API.
//!
//! Every parser takes bytes or text exactly as they arrived (a body, a
//! header value, a raw query string) and answers a typed, bounded value or
//! a [`RequestError`]. The routes call these and nothing else on app
//! input, and the `player_request` fuzz target drives the same functions
//! against an independent oracle.
//!
//! Apps send more fields than are read here (device details, cached copies
//! of objects the server sent); unknown fields are ignored, as an
//! Audiobookshelf server ignores them. A field that is read must have the
//! documented type and range, or the whole request is refused.

use base64::Engine as _;
use serde_json::{Map, Value};

use crate::ids::{is_entity_id, MAX_ID_BYTES};
use crate::time::{epoch_ms, secs_to_ms};

/// Largest sign-in body (`POST /player/login`).
pub const MAX_LOGIN_BODY_BYTES: usize = 4 * 1024;

/// Longest username a sign-in may carry, in bytes.
pub const MAX_USERNAME_BYTES: usize = 256;

/// Longest password a sign-in may carry, in bytes.
pub const MAX_PASSWORD_BYTES: usize = 256;

/// Largest body of a play, sync, progress or bookmark request. Apps send a
/// device description with a play request, so this is larger than the
/// fields read need.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// Largest body of a request that carries several items or sessions
/// (`/api/me/progress/batch/update`, `/api/session/local-all`,
/// `/api/items/batch/get`). An offline session as the apps send it holds a
/// copy of the item, so each can be several kilobytes.
pub const MAX_BATCH_BODY_BYTES: usize = 1024 * 1024;

/// Most entries a batch request may carry.
pub const MAX_BATCH_ENTRIES: usize = 100;

/// Longest bookmark title read, in bytes.
pub const MAX_BOOKMARK_TITLE_BYTES: usize = 200;

/// Longest raw query string read, in bytes.
pub const MAX_QUERY_BYTES: usize = 4 * 1024;

/// Longest search term read, in bytes.
pub const MAX_SEARCH_BYTES: usize = 256;

/// Most items one list page may ask for.
pub const MAX_PAGE_LIMIT: u64 = 1000;

/// Length of a card token: base64url without padding of 32 random bytes.
pub const CARD_TOKEN_LEN: usize = 43;

/// Longest player name kept from a play request's device details.
pub const MAX_DEVICE_NAME_BYTES: usize = 64;

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The body is longer than its limit.
    TooLong(usize),
    /// The body is not a JSON value of the expected kind.
    NotJson,
    /// A field is missing or has the wrong type.
    Shape(&'static str),
    /// A field is outside its range.
    OutOfRange(&'static str),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong(max) => write!(f, "the body is at most {max} bytes"),
            Self::NotJson => f.write_str("the body is not the expected JSON"),
            Self::Shape(field) => write!(f, "{field} is missing or has the wrong type"),
            Self::OutOfRange(field) => write!(f, "{field} is out of range"),
        }
    }
}

impl std::error::Error for RequestError {}

/// Reads `body` as a JSON object of at most `max` bytes.
fn object(body: &[u8], max: usize) -> Result<Map<String, Value>, RequestError> {
    if body.len() > max {
        return Err(RequestError::TooLong(max));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(RequestError::NotJson),
    }
}

/// A required string field.
fn string<'a>(map: &'a Map<String, Value>, field: &'static str) -> Result<&'a str, RequestError> {
    map.get(field)
        .and_then(Value::as_str)
        .ok_or(RequestError::Shape(field))
}

/// An optional number field: absent and `null` are `None`.
fn number(map: &Map<String, Value>, field: &'static str) -> Result<Option<f64>, RequestError> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_f64().map(Some).ok_or(RequestError::Shape(field)),
        Some(_) => Err(RequestError::Shape(field)),
    }
}

/// An optional length of time in seconds, as milliseconds.
fn seconds(map: &Map<String, Value>, field: &'static str) -> Result<Option<u64>, RequestError> {
    number(map, field)?
        .map(|secs| secs_to_ms(secs).ok_or(RequestError::OutOfRange(field)))
        .transpose()
}

/// An optional time of day in epoch milliseconds.
fn epoch(map: &Map<String, Value>, field: &'static str) -> Result<Option<i64>, RequestError> {
    number(map, field)?
        .map(|ms| epoch_ms(ms).ok_or(RequestError::OutOfRange(field)))
        .transpose()
}

/// An optional boolean field: absent and `null` are `None`.
fn flag(map: &Map<String, Value>, field: &'static str) -> Result<Option<bool>, RequestError> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RequestError::Shape(field)),
    }
}

/// A required entity id field ([`is_entity_id`]).
fn entity_id(map: &Map<String, Value>, field: &'static str) -> Result<String, RequestError> {
    let id = string(map, field)?;
    if is_entity_id(id) {
        Ok(id.to_owned())
    } else {
        Err(RequestError::OutOfRange(field))
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// True when `text` holds a control character.
fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

/// `POST /player/login`: a username and a player password.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginRequest {
    /// The patron's login name or email, trimmed.
    pub username: String,
    /// The player password as typed.
    pub password: String,
}

impl std::fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRequest")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Parses a sign-in body: a JSON object with string `username` and
/// `password`.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_LOGIN_BODY_BYTES`],
/// [`RequestError::NotJson`] for anything but an object,
/// [`RequestError::Shape`] when a field is missing or not a string, and
/// [`RequestError::OutOfRange`] for an empty or overlong field or a
/// control character.
pub fn parse_login_request(body: &[u8]) -> Result<LoginRequest, RequestError> {
    let map = object(body, MAX_LOGIN_BODY_BYTES)?;
    let username = string(&map, "username")?.trim();
    let password = string(&map, "password")?;
    if username.is_empty() || username.len() > MAX_USERNAME_BYTES || has_control(username) {
        return Err(RequestError::OutOfRange("username"));
    }
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES || has_control(password) {
        return Err(RequestError::OutOfRange("password"));
    }
    Ok(LoginRequest {
        username: username.to_owned(),
        password: password.to_owned(),
    })
}

/// A card token as an app sends it (`Authorization: Bearer`, `?token=`,
/// `x-refresh-token`, the socket's `auth` event).
///
/// # Arguments
///
/// * `value` - The token text, trimmed of surrounding spaces.
///
/// # Returns
///
/// `None` unless it is exactly [`CARD_TOKEN_LEN`] base64url characters,
/// the only shape this library mints; anything else is refused without a
/// lookup.
#[must_use]
pub fn parse_card_token(value: &str) -> Option<&str> {
    let value = value.trim();
    (value.len() == CARD_TOKEN_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(value)
}

/// The token of an `Authorization: Bearer <token>` value.
///
/// # Returns
///
/// `None` unless the scheme is `Bearer` (any case) and the token passes
/// [`parse_card_token`].
#[must_use]
pub fn parse_bearer(value: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(value).ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    parse_card_token(token)
}

/// What a player reported about the device it plays on, kept for the
/// session list and history.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceInfo {
    /// A name for the player, built from the app's client name and the
    /// device's model, at most [`MAX_DEVICE_NAME_BYTES`] bytes.
    pub name: String,
}

/// Reads the player's name out of a `deviceInfo` object: client name,
/// then manufacturer and model, joined. Values with control characters
/// are skipped.
fn device_info(value: Option<&Value>) -> DeviceInfo {
    let Some(Value::Object(map)) = value else {
        return DeviceInfo::default();
    };
    let part = |key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && !has_control(s))
    };
    let mut parts: Vec<&str> = Vec::new();
    for key in ["clientName", "manufacturer", "model", "deviceName"] {
        if let Some(text) = part(key) {
            if !parts.contains(&text) {
                parts.push(text);
            }
        }
    }
    DeviceInfo {
        name: clip(&parts.join(" "), MAX_DEVICE_NAME_BYTES)
            .trim()
            .to_owned(),
    }
}

/// `POST /player/api/items/{id}/play`: what the app can play.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlayRequest {
    /// The player's device details.
    pub device: DeviceInfo,
    /// True when the app asked for the stored file even if it might not
    /// play it (`forceDirectPlay`).
    pub force_direct_play: bool,
    /// True when the app asked for a transcode (`forceTranscode`); the
    /// library has none, so this is answered as not playable.
    pub force_transcode: bool,
}

/// Parses a play request. An empty body counts as `{}`.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_BODY_BYTES`],
/// [`RequestError::NotJson`] for anything but an object, and
/// [`RequestError::Shape`] when a flag is not a boolean.
pub fn parse_play_request(body: &[u8]) -> Result<PlayRequest, RequestError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(PlayRequest::default());
    }
    let map = object(body, MAX_BODY_BYTES)?;
    Ok(PlayRequest {
        device: device_info(map.get("deviceInfo")),
        force_direct_play: flag(&map, "forceDirectPlay")?.unwrap_or(false),
        force_transcode: flag(&map, "forceTranscode")?.unwrap_or(false),
    })
}

/// `POST /player/api/session/{id}/sync` and `.../close`: where the player
/// is, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncRequest {
    /// The position (`currentTime`); `None` when the report leaves it out,
    /// which a close may.
    pub position_ms: Option<u64>,
    /// Time listened since the last report (`timeListened`).
    pub listened_ms: u64,
    /// The recording's runtime as the player knows it (`duration`).
    pub duration_ms: Option<u64>,
}

/// Parses a sync or close body. An empty body counts as `{}` (a close with
/// nothing to report).
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_BODY_BYTES`],
/// [`RequestError::NotJson`] for anything but an object,
/// [`RequestError::Shape`] for a field that is not a number, and
/// [`RequestError::OutOfRange`] for a negative or overlong length.
pub fn parse_sync_request(body: &[u8]) -> Result<SyncRequest, RequestError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(SyncRequest::default());
    }
    let map = object(body, MAX_BODY_BYTES)?;
    Ok(SyncRequest {
        position_ms: seconds(&map, "currentTime")?,
        listened_ms: seconds(&map, "timeListened")?.unwrap_or(0),
        duration_ms: seconds(&map, "duration")?.filter(|d| *d > 0),
    })
}

/// A session a player kept while offline, sent later
/// (`POST /player/api/session/local`, one entry of `.../local-all`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSession {
    /// The app's own id for the session, echoed in the answer.
    pub id: String,
    /// The item it played (`libraryItemId`).
    pub item_id: String,
    /// Where it stopped.
    pub position_ms: u64,
    /// Time listened in it (`timeListening`).
    pub listened_ms: u64,
    /// The recording's runtime as the player knew it.
    pub duration_ms: Option<u64>,
    /// When it was last reported, epoch milliseconds (`updatedAt`).
    pub updated_at_ms: Option<i64>,
}

/// Reads one offline session object.
fn local_session(map: &Map<String, Value>) -> Result<LocalSession, RequestError> {
    let id = string(map, "id")?;
    if id.is_empty() || id.len() > MAX_ID_BYTES || has_control(id) {
        return Err(RequestError::OutOfRange("id"));
    }
    Ok(LocalSession {
        id: id.to_owned(),
        item_id: entity_id(map, "libraryItemId")?,
        position_ms: seconds(map, "currentTime")?.ok_or(RequestError::Shape("currentTime"))?,
        listened_ms: seconds(map, "timeListening")?.unwrap_or(0),
        duration_ms: seconds(map, "duration")?.filter(|d| *d > 0),
        updated_at_ms: epoch(map, "updatedAt")?,
    })
}

/// Parses `POST /player/api/session/local`: one offline session.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_BATCH_BODY_BYTES`], and the field
/// errors of an offline session.
pub fn parse_local_session(body: &[u8]) -> Result<LocalSession, RequestError> {
    local_session(&object(body, MAX_BATCH_BODY_BYTES)?)
}

/// One entry of a batch of local sessions: the session, or why it was
/// refused together with the id the app gave it (when it gave one), so the
/// answer can name the entry it refuses.
pub type LocalSessionEntry = Result<LocalSession, (Option<String>, RequestError)>;

/// Parses `POST /player/api/session/local-all`: `{"sessions": [...]}`.
///
/// # Returns
///
/// One result per entry, in order: an entry that does not parse is its
/// own error, so one bad session does not refuse the rest.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_BATCH_BODY_BYTES`],
/// [`RequestError::NotJson`] for anything but an object,
/// [`RequestError::Shape`] when `sessions` is not an array, and
/// [`RequestError::OutOfRange`] past [`MAX_BATCH_ENTRIES`] entries.
pub fn parse_local_sessions(body: &[u8]) -> Result<Vec<LocalSessionEntry>, RequestError> {
    let map = object(body, MAX_BATCH_BODY_BYTES)?;
    let sessions = map
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or(RequestError::Shape("sessions"))?;
    if sessions.len() > MAX_BATCH_ENTRIES {
        return Err(RequestError::OutOfRange("sessions"));
    }
    Ok(sessions
        .iter()
        .map(|entry| match entry {
            Value::Object(map) => local_session(map).map_err(|err| {
                let id = map
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| id.len() <= MAX_ID_BYTES && !has_control(id))
                    .map(str::to_owned);
                (id, err)
            }),
            _ => Err((None, RequestError::NotJson)),
        })
        .collect())
}

/// A change to the caller's progress in one item
/// (`PATCH /player/api/me/progress/{id}` and each batch entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressPatch {
    /// The new position (`currentTime`).
    pub position_ms: Option<u64>,
    /// The runtime the app knows (`duration`).
    pub duration_ms: Option<u64>,
    /// Finished or not (`isFinished`).
    pub is_finished: Option<bool>,
    /// True to take the item off Continue listening
    /// (`hideFromContinueListening`).
    pub hide: Option<bool>,
}

/// Reads one progress change object.
fn progress_patch(map: &Map<String, Value>) -> Result<ProgressPatch, RequestError> {
    let duration_ms = seconds(map, "duration")?.filter(|d| *d > 0);
    let mut position_ms = seconds(map, "currentTime")?;
    // An app that sends only a fraction (`progress`, 0 to 1) means that
    // share of the runtime it also sent.
    if position_ms.is_none() {
        if let (Some(fraction), Some(duration)) = (number(map, "progress")?, duration_ms) {
            if !(0.0..=1.0).contains(&fraction) {
                return Err(RequestError::OutOfRange("progress"));
            }
            let secs = crate::time::ms_to_secs(duration) * fraction;
            position_ms = secs_to_ms(secs);
        }
    }
    Ok(ProgressPatch {
        position_ms,
        duration_ms,
        is_finished: flag(map, "isFinished")?,
        hide: flag(map, "hideFromContinueListening")?,
    })
}

/// Parses a progress change body.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_BODY_BYTES`],
/// [`RequestError::NotJson`] for anything but an object, and the field
/// errors.
pub fn parse_progress_patch(body: &[u8]) -> Result<ProgressPatch, RequestError> {
    progress_patch(&object(body, MAX_BODY_BYTES)?)
}

/// Parses `PATCH /player/api/me/progress/batch/update`: an array of
/// progress changes, each naming its item in `libraryItemId`.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_BATCH_BODY_BYTES`],
/// [`RequestError::NotJson`] for anything but an array of objects,
/// [`RequestError::OutOfRange`] past [`MAX_BATCH_ENTRIES`] entries, and
/// the field errors of any entry.
pub fn parse_progress_batch(body: &[u8]) -> Result<Vec<(String, ProgressPatch)>, RequestError> {
    if body.len() > MAX_BATCH_BODY_BYTES {
        return Err(RequestError::TooLong(MAX_BATCH_BODY_BYTES));
    }
    let Ok(Value::Array(entries)) = serde_json::from_slice::<Value>(body) else {
        return Err(RequestError::NotJson);
    };
    if entries.len() > MAX_BATCH_ENTRIES {
        return Err(RequestError::OutOfRange("entries"));
    }
    entries
        .iter()
        .map(|entry| {
            let Value::Object(map) = entry else {
                return Err(RequestError::NotJson);
            };
            Ok((entity_id(map, "libraryItemId")?, progress_patch(map)?))
        })
        .collect()
}

/// A bookmark as an app sends it
/// (`POST`/`PATCH /player/api/me/item/{id}/bookmark`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkRequest {
    /// The marked place (`time`).
    pub position_ms: u64,
    /// The note (`title`), trimmed; may be empty.
    pub title: String,
}

/// Parses a bookmark body: a number `time` in seconds and an optional
/// string `title`.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_BODY_BYTES`],
/// [`RequestError::NotJson`] for anything but an object,
/// [`RequestError::Shape`] when `time` is missing or a field has the wrong
/// type, and [`RequestError::OutOfRange`] for a length out of range or a
/// title over [`MAX_BOOKMARK_TITLE_BYTES`] bytes or with a control
/// character.
pub fn parse_bookmark_request(body: &[u8]) -> Result<BookmarkRequest, RequestError> {
    let map = object(body, MAX_BODY_BYTES)?;
    let position_ms = seconds(&map, "time")?.ok_or(RequestError::Shape("time"))?;
    let title = match map.get("title") {
        None | Some(Value::Null) => "",
        Some(Value::String(title)) => title.trim(),
        Some(_) => return Err(RequestError::Shape("title")),
    };
    if title.len() > MAX_BOOKMARK_TITLE_BYTES || has_control(title) {
        return Err(RequestError::OutOfRange("title"));
    }
    Ok(BookmarkRequest {
        position_ms,
        title: title.to_owned(),
    })
}

/// The bookmark a `DELETE /player/api/me/item/{id}/bookmark/{time}` path
/// names, by its time in seconds.
///
/// # Returns
///
/// `None` unless `time` is a plain non-negative decimal number in range.
#[must_use]
pub fn parse_bookmark_time(time: &str) -> Option<u64> {
    if time.is_empty()
        || time.len() > 24
        || !time.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || time.bytes().filter(|b| *b == b'.').count() > 1
    {
        return None;
    }
    secs_to_ms(time.parse().ok()?)
}

/// Parses `POST /player/api/items/batch/get`: `{"libraryItemIds": [...]}`.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_BODY_BYTES`],
/// [`RequestError::NotJson`] for anything but an object,
/// [`RequestError::Shape`] when the list is missing or holds a non-string,
/// and [`RequestError::OutOfRange`] past [`MAX_BATCH_ENTRIES`] ids or for
/// an id of the wrong shape.
pub fn parse_batch_get(body: &[u8]) -> Result<Vec<String>, RequestError> {
    let map = object(body, MAX_BODY_BYTES)?;
    let ids = map
        .get("libraryItemIds")
        .and_then(Value::as_array)
        .ok_or(RequestError::Shape("libraryItemIds"))?;
    if ids.len() > MAX_BATCH_ENTRIES {
        return Err(RequestError::OutOfRange("libraryItemIds"));
    }
    ids.iter()
        .map(|id| match id.as_str() {
            Some(id) if is_entity_id(id) => Ok(id.to_owned()),
            Some(_) => Err(RequestError::OutOfRange("libraryItemIds")),
            None => Err(RequestError::Shape("libraryItemIds")),
        })
        .collect()
}

/// How a list is ordered (`sort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListSort {
    /// By title (`media.metadata.title`, and anything not listed here).
    #[default]
    Title,
    /// By author (`media.metadata.authorName`, `...authorNameLF`).
    Author,
    /// By when the library got it (`addedAt`, `birthtimeMs`, `mtimeMs`).
    Added,
    /// By runtime (`media.duration`, `size`).
    Duration,
    /// By place in a series (`sequence`).
    Series,
}

/// Which part of a library a list shows (`filter=<group>.<base64 value>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListFilter {
    /// Works by this author: an author id ([`crate::ids::facet_id`]).
    Author(String),
    /// Works in this series: a series id ([`crate::ids::facet_id`]).
    Series(String),
    /// Works in no series (`series.no-series`).
    NoSeries,
    /// Works with this genre.
    Genre(String),
    /// Works the caller tagged with this tag.
    Tag(String),
    /// Works with this narrator.
    Narrator(String),
    /// Works from this publisher.
    Publisher(String),
    /// Works the caller finished.
    Finished,
    /// Works the caller has not finished.
    NotFinished,
    /// Works the caller has started and not finished.
    InProgress,
    /// Works the caller has not started.
    NotStarted,
    /// A filter group this library does not keep (languages, ebooks,
    /// missing metadata, feeds, ...): nothing matches.
    Unsupported,
}

/// The query of a list route (`/player/api/libraries/{id}/items` and the
/// other paged lists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// Rows per page; `0` asks for every row (bounded by
    /// [`MAX_PAGE_LIMIT`]).
    pub limit: u64,
    /// Page number, from 0.
    pub page: u64,
    /// The order.
    pub sort: ListSort,
    /// True to reverse it (`desc=1`).
    pub descending: bool,
    /// The filter, when one was given.
    pub filter: Option<ListFilter>,
    /// True to fold works of one series into one row (`collapseseries=1`).
    pub collapse_series: bool,
    /// True when the caller's progress should ride along on each item
    /// (`include=progress`).
    pub include_progress: bool,
    /// True when the minified item shape was asked for (`minified=1`).
    pub minified: bool,
    /// A search term (`q`), trimmed.
    pub search: Option<String>,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            limit: 0,
            page: 0,
            sort: ListSort::Title,
            descending: false,
            filter: None,
            collapse_series: false,
            include_progress: false,
            minified: false,
            search: None,
        }
    }
}

/// Decodes a filter value: standard or URL-safe base64, padded or not, of
/// UTF-8 text; Audiobookshelf also percent-decodes the result.
fn filter_value(encoded: &str, max: usize) -> Option<String> {
    let trimmed = encoded.trim_end_matches('=');
    if trimmed.is_empty() || trimmed.len() > 4 * MAX_SEARCH_BYTES {
        return None;
    }
    let bytes = if trimmed.contains(['-', '_']) {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed)
    } else {
        base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed)
    }
    .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let text = url::form_urlencoded::parse(format!("v={}", text.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or(text);
    let text = text.trim();
    (!text.is_empty() && text.len() <= max && !has_control(text)).then(|| text.to_owned())
}

/// Parses a `filter` value (`group.base64`).
///
/// # Returns
///
/// `None` for a value of the wrong shape; [`ListFilter::Unsupported`] for
/// a well-formed filter on a group this library does not keep.
#[must_use]
pub fn parse_list_filter(raw: &str) -> Option<ListFilter> {
    let (group, encoded) = raw.split_once('.')?;
    let max = if matches!(group, "authors" | "series") {
        MAX_ID_BYTES
    } else {
        MAX_SEARCH_BYTES
    };
    let value = filter_value(encoded, max)?;
    Some(match group {
        "authors" if is_entity_id(&value) => ListFilter::Author(value),
        "series" if value == "no-series" => ListFilter::NoSeries,
        "series" if is_entity_id(&value) => ListFilter::Series(value),
        "genres" => ListFilter::Genre(value),
        "tags" => ListFilter::Tag(value),
        "narrators" => ListFilter::Narrator(value),
        "publishers" => ListFilter::Publisher(value),
        "progress" => match value.as_str() {
            "finished" => ListFilter::Finished,
            "not-finished" => ListFilter::NotFinished,
            "in-progress" => ListFilter::InProgress,
            "not-started" => ListFilter::NotStarted,
            _ => ListFilter::Unsupported,
        },
        "authors" | "series" => return None,
        _ => ListFilter::Unsupported,
    })
}

/// Parses a list route's raw query string.
///
/// Unknown keys are ignored; a known key with a bad value refuses the
/// query.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_QUERY_BYTES`], and
/// [`RequestError::OutOfRange`] naming the first bad value.
pub fn parse_list_query(raw: &str) -> Result<ListQuery, RequestError> {
    if raw.len() > MAX_QUERY_BYTES {
        return Err(RequestError::TooLong(MAX_QUERY_BYTES));
    }
    let mut query = ListQuery::default();
    let number = |value: &str, field: &'static str, max: u64| -> Result<u64, RequestError> {
        let n: u64 = value
            .trim()
            .parse()
            .map_err(|_| RequestError::OutOfRange(field))?;
        if n > max {
            return Err(RequestError::OutOfRange(field));
        }
        Ok(n)
    };
    let truthy = |value: &str| matches!(value.trim(), "1" | "true");
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "limit" => query.limit = number(&value, "limit", MAX_PAGE_LIMIT)?,
            "page" => query.page = number(&value, "page", 1_000_000)?,
            "desc" => query.descending = truthy(&value),
            "collapseseries" => query.collapse_series = truthy(&value),
            "minified" => query.minified = truthy(&value),
            "include" => {
                query.include_progress = value.split(',').any(|part| part.trim() == "progress");
            }
            "sort" => {
                query.sort = match value.as_ref() {
                    "media.metadata.authorName" | "media.metadata.authorNameLF" => ListSort::Author,
                    "addedAt" | "birthtimeMs" | "mtimeMs" => ListSort::Added,
                    "media.duration" | "size" => ListSort::Duration,
                    "sequence" => ListSort::Series,
                    _ => ListSort::Title,
                };
            }
            "filter" => {
                if !value.is_empty() {
                    query.filter =
                        Some(parse_list_filter(&value).ok_or(RequestError::OutOfRange("filter"))?);
                }
            }
            "q" => {
                let term = value.trim();
                if term.len() > MAX_SEARCH_BYTES || has_control(term) {
                    return Err(RequestError::OutOfRange("q"));
                }
                query.search = (!term.is_empty()).then(|| term.to_owned());
            }
            _ => {}
        }
    }
    Ok(query)
}

/// The query of a single-entity or small-list route (`GET
/// /api/items/{id}`, `/api/authors/{id}`, `/api/libraries/{id}`,
/// `/search`, `/me/listening-sessions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetailQuery {
    /// `expanded=1`: the item with its tracks and files.
    pub expanded: bool,
    /// `include=progress`: the patron's progress rides along.
    pub include_progress: bool,
    /// `include=filterdata`: a library with its filter values.
    pub include_filterdata: bool,
    /// `include=items`: an author with their items.
    pub include_items: bool,
    /// `include=series`: an author with their series.
    pub include_series: bool,
    /// `limit` or `itemsPerPage`, at most [`MAX_PAGE_LIMIT`].
    pub limit: Option<u64>,
    /// `page`, from 0.
    pub page: u64,
    /// `q`, trimmed; `None` when empty.
    pub search: Option<String>,
}

/// Parses the query of a single-entity or small-list route.
///
/// Unknown keys are ignored; a known key with a bad value refuses the
/// query.
///
/// # Errors
///
/// [`RequestError::TooLong`] past [`MAX_QUERY_BYTES`], and
/// [`RequestError::OutOfRange`] naming the first bad value.
pub fn parse_detail_query(raw: &str) -> Result<DetailQuery, RequestError> {
    if raw.len() > MAX_QUERY_BYTES {
        return Err(RequestError::TooLong(MAX_QUERY_BYTES));
    }
    let mut query = DetailQuery::default();
    let number = |value: &str, field: &'static str, max: u64| -> Result<u64, RequestError> {
        let n: u64 = value
            .trim()
            .parse()
            .map_err(|_| RequestError::OutOfRange(field))?;
        if n > max {
            return Err(RequestError::OutOfRange(field));
        }
        Ok(n)
    };
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "expanded" => query.expanded = matches!(value.trim(), "1" | "true"),
            "include" => {
                for part in value.split(',').map(str::trim) {
                    match part {
                        "progress" => query.include_progress = true,
                        "filterdata" => query.include_filterdata = true,
                        "items" => query.include_items = true,
                        "series" => query.include_series = true,
                        _ => {}
                    }
                }
            }
            "limit" | "itemsPerPage" => {
                query.limit = Some(number(&value, "limit", MAX_PAGE_LIMIT)?);
            }
            "page" => query.page = number(&value, "page", 1_000_000)?,
            "q" => {
                let term = value.trim();
                if term.len() > MAX_SEARCH_BYTES || has_control(term) {
                    return Err(RequestError::OutOfRange("q"));
                }
                query.search = (!term.is_empty()).then(|| term.to_owned());
            }
            _ => {}
        }
    }
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_needs_both_strings() {
        let ok = parse_login_request(br#"{"username":" riley ","password":"abc-def"}"#).unwrap();
        assert_eq!(ok.username, "riley");
        assert!(!format!("{ok:?}").contains("abc-def"));
        assert_eq!(
            parse_login_request(br#"{"username":"riley"}"#),
            Err(RequestError::Shape("password"))
        );
        assert_eq!(
            parse_login_request(br#"{"username":"","password":"x"}"#),
            Err(RequestError::OutOfRange("username"))
        );
        assert_eq!(parse_login_request(b"[]"), Err(RequestError::NotJson));
        let long = format!(r#"{{"username":"a","password":"{}"}}"#, "p".repeat(5000));
        assert!(matches!(
            parse_login_request(long.as_bytes()),
            Err(RequestError::TooLong(_))
        ));
    }

    #[test]
    fn tokens_have_one_shape() {
        let token = "a".repeat(CARD_TOKEN_LEN);
        assert_eq!(parse_card_token(&token), Some(token.as_str()));
        assert_eq!(
            parse_card_token(&format!(" {token} ")),
            Some(token.as_str())
        );
        assert_eq!(parse_card_token("eyJ.abc.def"), None);
        assert_eq!(parse_card_token(&"a".repeat(CARD_TOKEN_LEN + 1)), None);
        let header = format!("Bearer {token}");
        assert_eq!(parse_bearer(header.as_bytes()), Some(token.as_str()));
        assert_eq!(parse_bearer(format!("Basic {token}").as_bytes()), None);
    }

    #[test]
    fn a_sync_reads_seconds_as_milliseconds() {
        let sync =
            parse_sync_request(br#"{"currentTime":754.2,"timeListened":5,"duration":0}"#).unwrap();
        assert_eq!(sync.position_ms, Some(754_200));
        assert_eq!(sync.listened_ms, 5000);
        assert_eq!(sync.duration_ms, None);
        assert_eq!(parse_sync_request(b"").unwrap(), SyncRequest::default());
        assert_eq!(
            parse_sync_request(br#"{"currentTime":-1}"#),
            Err(RequestError::OutOfRange("currentTime"))
        );
        assert_eq!(
            parse_sync_request(br#"{"currentTime":"1"}"#),
            Err(RequestError::Shape("currentTime"))
        );
    }

    #[test]
    fn a_play_request_names_the_device() {
        let play = parse_play_request(
            br#"{"deviceInfo":{"clientName":"Abs Android","manufacturer":"Google","model":"Pixel 9"},"forceDirectPlay":true,"supportedMimeTypes":["audio/mp4"]}"#,
        )
        .unwrap();
        assert_eq!(play.device.name, "Abs Android Google Pixel 9");
        assert!(play.force_direct_play);
        assert_eq!(parse_play_request(b"  ").unwrap(), PlayRequest::default());
        assert_eq!(
            parse_play_request(br#"{"forceTranscode":"yes"}"#),
            Err(RequestError::Shape("forceTranscode"))
        );
    }

    #[test]
    fn offline_sessions_parse_one_by_one() {
        let body = br#"{"sessions":[{"id":"s1","libraryItemId":"ed-1","currentTime":10,"timeListening":4,"updatedAt":1763600000000},{"id":"s2","libraryItemId":"../x","currentTime":1},7]}"#;
        let parsed = parse_local_sessions(body).unwrap();
        assert_eq!(parsed.len(), 3);
        let first = parsed[0].as_ref().unwrap();
        assert_eq!(first.position_ms, 10_000);
        assert_eq!(first.updated_at_ms, Some(1_763_600_000_000));
        assert_eq!(
            parsed[1].as_ref().unwrap_err(),
            &(Some("s2".into()), RequestError::OutOfRange("libraryItemId"))
        );
        assert_eq!(parsed[2].as_ref().unwrap_err().0, None);
        assert!(parse_local_session(br#"{"id":"s","libraryItemId":"e"}"#).is_err());
    }

    #[test]
    fn progress_changes_take_seconds_or_a_fraction() {
        let patch = parse_progress_patch(br#"{"progress":0.5,"duration":100}"#).unwrap();
        assert_eq!(patch.position_ms, Some(50_000));
        let patch = parse_progress_patch(br#"{"isFinished":true}"#).unwrap();
        assert_eq!(patch.is_finished, Some(true));
        assert_eq!(
            parse_progress_patch(br#"{"progress":2,"duration":100}"#),
            Err(RequestError::OutOfRange("progress"))
        );
        let batch = parse_progress_batch(
            br#"[{"libraryItemId":"e1","currentTime":3},{"libraryItemId":"e2"}]"#,
        )
        .unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].1.position_ms, Some(3000));
        assert!(parse_progress_batch(br#"[{"currentTime":3}]"#).is_err());
        assert!(parse_progress_batch(br#"{}"#).is_err());
    }

    #[test]
    fn bookmarks_are_bounded() {
        let bm = parse_bookmark_request(br#"{"time":12.5,"title":" Storm "}"#).unwrap();
        assert_eq!(bm.position_ms, 12_500);
        assert_eq!(bm.title, "Storm");
        assert!(parse_bookmark_request(br#"{"title":"x"}"#).is_err());
        assert!(parse_bookmark_request(br#"{"time":1,"title":"a\u0007"}"#).is_err());
        assert_eq!(parse_bookmark_time("12.5"), Some(12_500));
        assert_eq!(parse_bookmark_time("12"), Some(12_000));
        for bad in ["", "-1", "1e3", "1.2.3", "abc"] {
            assert_eq!(parse_bookmark_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn batch_gets_list_ids() {
        assert_eq!(
            parse_batch_get(br#"{"libraryItemIds":["a","b"]}"#).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_batch_get(br#"{"libraryItemIds":[1]}"#).is_err());
        assert!(parse_batch_get(br#"{"libraryItemIds":["a b"]}"#).is_err());
    }

    #[test]
    fn list_queries_map_sorts_and_filters() {
        let q = parse_list_query(
            "limit=50&page=2&sort=addedAt&desc=1&minified=1&include=rssfeed,progress&filter=series.c2VyLTE%3D",
        )
        .unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.page, 2);
        assert_eq!(q.sort, ListSort::Added);
        assert!(q.descending && q.minified && q.include_progress);
        assert_eq!(q.filter, Some(ListFilter::Series("ser-1".into())));
        assert_eq!(
            parse_list_filter("progress.ZmluaXNoZWQ="),
            Some(ListFilter::Finished)
        );
        assert_eq!(
            parse_list_filter("languages.ZW5n"),
            Some(ListFilter::Unsupported)
        );
        assert_eq!(
            parse_list_filter("genres.RmFudGFzeQ"),
            Some(ListFilter::Genre("Fantasy".into()))
        );
        assert_eq!(parse_list_filter("nodot"), None);
        assert_eq!(parse_list_filter("authors.Li4vZXRj"), None);
        assert!(parse_list_query("limit=5000").is_err());
        assert!(parse_list_query("filter=series.%%%").is_err());
        assert_eq!(parse_list_query("unknown=1").unwrap(), ListQuery::default());
    }

    #[test]
    fn detail_queries_are_bounded() {
        let q = parse_detail_query(
            "expanded=1&include=progress,series&itemsPerPage=10&page=2&q=+hobbit+",
        )
        .unwrap();
        assert!(q.expanded && q.include_progress && q.include_series && !q.include_items);
        assert_eq!((q.limit, q.page), (Some(10), 2));
        assert_eq!(q.search.as_deref(), Some("hobbit"));
        assert_eq!(
            parse_detail_query("limit=5000"),
            Err(RequestError::OutOfRange("limit"))
        );
        assert_eq!(
            parse_detail_query("q=%01"),
            Err(RequestError::OutOfRange("q"))
        );
        assert_eq!(parse_detail_query("").unwrap(), DetailQuery::default());
    }
}
