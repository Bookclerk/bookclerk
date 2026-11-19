//! What a player sends the playback routes, parsed sans IO so the routes,
//! the unit tests and the `playback_request` fuzz target drive the same
//! code (`docs/adr/untrusted-input-fuzzing.md`):
//!
//! - the body of `POST /api/playback/sessions/{id}/sync` and `.../close`
//!   ([`parse_playback_sync_request`]), which moves a person's progress;
//! - the path of `GET /api/playback/sessions/{id}/tracks/{n}`
//!   ([`parse_track_path`]), which names the file a stream request reads.
//!   Its `Range` header is `bookclerk_node_proto::parse_copy_range`'s.

use serde::Deserialize;

use super::playback::{PlaybackMs, PlaybackSync, MAX_PLAYBACK_MS};
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
