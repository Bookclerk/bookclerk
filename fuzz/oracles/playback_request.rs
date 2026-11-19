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
//! - `o`: the rest is the body of `POST /api/playback/sessions`, read by
//!   [`parse_open_session_request`]. Accepted exactly when it is at most
//!   4096 bytes and one JSON object with no repeated keys and no keys
//!   besides `edition_id`, `player` and `player_name`, where `edition_id`
//!   is a string of 1 to 128 bytes, `player` is absent, `"web"` or
//!   `"desktop"`, and `player_name` is absent or a string. An accepted
//!   body yields the id unchanged, the player's kind, and the name with
//!   its control characters dropped, trimmed and cut to at most 64 bytes
//!   on a character boundary.
//! - `p`: the rest is the body of `PUT /api/playback/progress/{id}`, read
//!   by [`parse_progress_edit_request`]. Accepted exactly when it is at
//!   most 4096 bytes and one JSON object with no repeated keys and no
//!   keys besides `position_ms`, `duration_ms` and `is_finished`, the two
//!   lengths each absent, `null` or a whole number from 0 to sixty days
//!   of milliseconds, and `is_finished` absent, `null` or a boolean. An
//!   accepted body yields those values, absent and `null` alike unset.
//! - `b` and `e`: the rest is the body of
//!   `POST /api/library/editions/{id}/bookmarks` (`b`, read by
//!   [`parse_new_bookmark_request`]) or `PUT /api/playback/bookmarks/{id}`
//!   (`e`, read by [`parse_bookmark_edit_request`]). Accepted exactly
//!   when it is at most 4096 bytes and one JSON object with no repeated
//!   keys and no keys besides `position_ms` and `title`, with a
//!   `position_ms` from 0 to sixty days of milliseconds (required for
//!   `b`; absent or `null` allowed for `e`) and a `title` string (absent
//!   allowed for both, `null` for `e` only) that, trimmed, is at most 200
//!   bytes and has no control character. An accepted body yields the
//!   position and the trimmed title.
//!
//! - `q`: the rest is the `cursor` query value of
//!   `GET /api/playback/progress`, read by [`parse_progress_cursor`]. The
//!   invariant, checked with this file's own Base64 decoder and a
//!   character-class pattern rather than the parser's: a token is
//!   accepted exactly when it is at most 256 bytes of canonical unpadded
//!   URL-safe Base64 that decodes to UTF-8 text of a change time
//!   (`YYYY-MM-DDTHH:MM:SS`, an optional fraction of one to nine digits,
//!   then `Z` or a `+HH:MM` / `-HH:MM` offset; digits are not range
//!   checked), a `|`, and an edition id of 1 to 128 bytes. An accepted
//!   token yields that time and id, and writing them back gives the same
//!   token.
//!
//! Any other first byte, and an empty input, is outside the boundary and
//! checks nothing. A refused body is `Invalid`, never another error.

use bookclerk_library::{
    parse_bookmark_edit_request, parse_new_bookmark_request, parse_open_session_request,
    parse_playback_sync_request, parse_progress_cursor, parse_progress_edit_request,
    parse_track_path, progress_cursor_token, LibraryError, PlayerKind,
};
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// Largest input worth checking: past the largest documented body, so
/// the size rule is reached.
pub const MAX_INPUT_BYTES: usize = 8192;

/// Largest session, progress or bookmark body the routes document.
const DOCUMENTED_MAX_OTHER_BODY_BYTES: usize = 4096;

/// Longest edition id the routes document, in bytes.
const DOCUMENTED_MAX_EDITION_ID_BYTES: usize = 128;

/// Longest stored player name the routes document, in bytes.
const DOCUMENTED_MAX_PLAYER_NAME_BYTES: usize = 64;

/// Longest progress cursor the routes document, in bytes.
const DOCUMENTED_MAX_CURSOR_BYTES: usize = 256;

/// Longest bookmark title the routes document, in bytes.
const DOCUMENTED_MAX_TITLE_BYTES: usize = 200;

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
        Some((b'o', body)) => check_open(body),
        Some((b'p', body)) => check_progress(body),
        Some((b'b', body)) => check_bookmark(body, false),
        Some((b'e', body)) => check_bookmark(body, true),
        Some((b'q', token)) => check_cursor(token),
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

/// Compares a parser's answer with the oracle's: both refuse (the parser
/// with `Invalid`), or both accept the same value.
///
/// # Returns
///
/// True when the body was accepted.
///
/// # Panics
///
/// When they disagree.
fn agree<T: std::fmt::Debug + PartialEq>(
    body: &[u8],
    got: Result<T, LibraryError>,
    expected: Option<T>,
) -> bool {
    match expected {
        None => {
            assert!(
                matches!(got, Err(LibraryError::Invalid(_))),
                "{:?}: {got:?}",
                String::from_utf8_lossy(body)
            );
            false
        }
        Some(expected) => {
            let got = got.unwrap_or_else(|err| {
                panic!("{:?} refused: {err:?}", String::from_utf8_lossy(body))
            });
            assert_eq!(got, expected, "{:?}", String::from_utf8_lossy(body));
            true
        }
    }
}

/// Reads a session, progress or bookmark body as an untyped JSON object
/// with only `allowed` keys, none repeated; `None` is a refusal.
fn object_with(body: &[u8], allowed: &[&str]) -> Option<serde_json::Map<String, Value>> {
    if body.len() > DOCUMENTED_MAX_OTHER_BODY_BYTES {
        return None;
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    if has_repeated_top_level_key(body) || fields.keys().any(|key| !allowed.contains(&key.as_str()))
    {
        return None;
    }
    Some(fields)
}

/// A documented length: a whole number from 0 to sixty days.
fn documented_ms(value: &Value) -> Option<u64> {
    value.as_u64().filter(|ms| *ms <= DOCUMENTED_MAX_MS)
}

/// An optional documented length: absent and `null` are unset. The outer
/// `None` is a refusal.
fn optional_ms(value: Option<&Value>) -> Option<Option<u64>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(value) => documented_ms(value).map(Some),
    }
}

/// True for a character Unicode classes as a control (`Cc`): U+0000 to
/// U+001F and U+007F to U+009F.
fn is_control(c: char) -> bool {
    matches!(u32::from(c), 0..=0x1f | 0x7f..=0x9f)
}

/// A documented bookmark title: trimmed, at most 200 bytes, no control
/// character; `None` is a refusal.
fn documented_title(title: &str) -> Option<String> {
    let title = title.trim();
    (title.len() <= DOCUMENTED_MAX_TITLE_BYTES && !title.chars().any(is_control))
        .then(|| title.to_string())
}

/// Checks one session body.
fn check_open(body: &[u8]) -> bool {
    let got = parse_open_session_request(body)
        .map(|open| (open.edition_id, open.player, open.player_name));
    agree(body, got, expected_open(body))
}

/// Applies the documented session-body rules; `None` is a refusal.
fn expected_open(body: &[u8]) -> Option<(String, PlayerKind, String)> {
    let fields = object_with(body, &["edition_id", "player", "player_name"])?;
    let edition_id = fields.get("edition_id")?.as_str()?;
    if edition_id.is_empty() || edition_id.len() > DOCUMENTED_MAX_EDITION_ID_BYTES {
        return None;
    }
    let player = match fields.get("player") {
        None => PlayerKind::Web,
        Some(Value::String(kind)) if kind == "web" => PlayerKind::Web,
        Some(Value::String(kind)) if kind == "desktop" => PlayerKind::Desktop,
        Some(_) => return None,
    };
    let written = match fields.get("player_name") {
        None => "",
        Some(value) => value.as_str()?,
    };
    let plain: String = written.chars().filter(|c| !is_control(*c)).collect();
    let mut name = String::new();
    for c in plain.trim().chars() {
        if name.len() + c.len_utf8() > DOCUMENTED_MAX_PLAYER_NAME_BYTES {
            break;
        }
        name.push(c);
    }
    Some((edition_id.to_string(), player, name))
}

/// Checks one progress body.
fn check_progress(body: &[u8]) -> bool {
    let got = parse_progress_edit_request(body).map(|edit| {
        (
            edit.position.map(|ms| ms.get()),
            edit.duration.map(|ms| ms.get()),
            edit.finished,
        )
    });
    agree(body, got, expected_progress(body))
}

/// Applies the documented progress-body rules; `None` is a refusal.
fn expected_progress(body: &[u8]) -> Option<(Option<u64>, Option<u64>, Option<bool>)> {
    let fields = object_with(body, &["position_ms", "duration_ms", "is_finished"])?;
    let finished = match fields.get("is_finished") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(flag)) => Some(*flag),
        Some(_) => return None,
    };
    Some((
        optional_ms(fields.get("position_ms"))?,
        optional_ms(fields.get("duration_ms"))?,
        finished,
    ))
}

/// Checks one bookmark body: a new bookmark's, or with `edit` a change's.
fn check_bookmark(body: &[u8], edit: bool) -> bool {
    let got = if edit {
        parse_bookmark_edit_request(body).map(|edit| (edit.position.map(|ms| ms.get()), edit.title))
    } else {
        parse_new_bookmark_request(body).map(|new| (Some(new.position.get()), Some(new.title)))
    };
    agree(body, got, expected_bookmark(body, edit))
}

/// Applies the documented bookmark-body rules; `None` is a refusal.
fn expected_bookmark(body: &[u8], edit: bool) -> Option<(Option<u64>, Option<String>)> {
    let fields = object_with(body, &["position_ms", "title"])?;
    let position = if edit {
        optional_ms(fields.get("position_ms"))?
    } else {
        Some(documented_ms(fields.get("position_ms")?)?)
    };
    let title = match fields.get("title") {
        None if edit => None,
        None => Some(String::new()),
        Some(Value::Null) if edit => None,
        Some(value) => Some(documented_title(value.as_str()?)?),
    };
    Some((position, title))
}

/// Checks one progress cursor against [`expected_cursor`].
fn check_cursor(token: &[u8]) -> bool {
    // A query value reaches the route as text; bytes that are not UTF-8
    // never do.
    let Ok(token) = std::str::from_utf8(token) else {
        return false;
    };
    let got = parse_progress_cursor(token);
    match expected_cursor(token) {
        None => {
            assert_eq!(got, None, "{token:?}");
            false
        }
        Some((updated_at, edition_id)) => {
            let Some(cursor) = got else {
                panic!("{token:?} refused");
            };
            assert_eq!(cursor.updated_at, updated_at, "{token:?}");
            assert_eq!(cursor.edition_id, edition_id, "{token:?}");
            assert_eq!(progress_cursor_token(&cursor), token);
            true
        }
    }
}

/// Decodes canonical unpadded URL-safe Base64 by hand; `None` for any
/// other text (padding, another alphabet, a dangling character, or
/// leftover bits that are not zero).
fn base64url(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut bits, mut held) = (0u32, 0u32);
    for byte in text.bytes() {
        let six = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        held = (held << 6) | u32::from(six);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((held >> bits) & 0xff).ok()?);
        }
    }
    // Six leftover bits are a dangling character; any set leftover bit is
    // a second spelling of the same bytes.
    (bits < 6 && held & ((1 << bits) - 1) == 0).then_some(out)
}

/// Applies the documented cursor rules; `None` is a refusal.
fn expected_cursor(token: &str) -> Option<(String, String)> {
    if token.len() > DOCUMENTED_MAX_CURSOR_BYTES {
        return None;
    }
    let text = String::from_utf8(base64url(token)?).ok()?;
    let (updated_at, edition_id) = text.split_once('|')?;
    if edition_id.is_empty() || edition_id.len() > DOCUMENTED_MAX_EDITION_ID_BYTES {
        return None;
    }
    // The time as a pattern: `d` for each digit, every other character
    // as itself (a written `d` becomes `?`, which no shape has).
    let pattern: String = updated_at
        .chars()
        .map(|c| match c {
            '0'..='9' => 'd',
            'd' => '?',
            other => other,
        })
        .collect();
    let mut shapes = Vec::new();
    for fraction in 0..=9 {
        let fraction = if fraction == 0 {
            String::new()
        } else {
            format!(".{}", "d".repeat(fraction))
        };
        for zone in ["Z", "+dd:dd", "-dd:dd"] {
            shapes.push(format!("dddd-dd-ddTdd:dd:dd{fraction}{zone}"));
        }
    }
    shapes
        .contains(&pattern)
        .then(|| (updated_at.to_string(), edition_id.to_string()))
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
            assert!(got.is_none(), "a path the oracle refuses was read");
            false
        }
        Some(expected_index) => {
            let Some(track) = got else {
                panic!("a path the oracle accepts was refused");
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
