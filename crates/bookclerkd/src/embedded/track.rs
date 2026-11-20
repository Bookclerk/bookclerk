//! A play session's track, decoded for the desktop window's own player.
//!
//! The window cannot stream audio through its custom scheme (no `Range`),
//! so its native player asks the host for PCM instead: [`open_track`]
//! reads the track through the same stream route the web player uses,
//! `GET /api/playback/sessions/{id}/tracks/{index}`, by `Range`, in
//! process, and hands the bytes to a confined media worker that decodes
//! them ([`bookclerk_media::MediaPool::open_pcm`]). Every block is a fresh
//! request to that route, so its checks (the session's person, an open
//! session, this node, the patron scope, the files digest) apply to every
//! byte the worker reads, exactly as for a browser.

use std::sync::Arc;

use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use bookclerk_library::parse_track_path;
use bookclerk_media::{ByteSpan, PcmStream, RangeSource};

use crate::api::AppState;

/// Why a track cannot be played, in words for the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackError {
    /// The path is not a play session's track.
    NotATrack,
    /// The session ended (closed, idle, or the edition changed): the
    /// player opens a new one.
    SessionEnded,
    /// The stream route refused the track.
    Refused(u16),
    /// The track's size could not be read from the route's answer.
    UnknownSize,
    /// The media worker could not start or decode the track.
    Media(String),
}

impl std::fmt::Display for TrackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotATrack => f.write_str("not a track of a play session"),
            Self::SessionEnded => f.write_str("the play session ended"),
            Self::Refused(status) => write!(f, "the track could not be read (HTTP {status})"),
            Self::UnknownSize => f.write_str("the track's size is unknown"),
            Self::Media(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for TrackError {}

/// The host's router, as the track's byte source.
#[derive(Clone)]
struct HostRoute {
    /// The router the window's requests go to.
    app: Router,
    /// The host's state, for the current operator token.
    state: Arc<AppState>,
    /// The track's stream path.
    path: String,
}

impl HostRoute {
    /// Requests `range` (an HTTP `Range` value) of the track.
    ///
    /// # Returns
    ///
    /// The status, the `Content-Range` value if any, and the body.
    async fn get(&self, range: &str) -> (StatusCode, Option<String>, Vec<u8>) {
        let auth = self.state.auth_snapshot().await;
        let bearer = auth.enabled.then(|| auth.token.clone());
        drop(auth);
        let request = Request::builder()
            .method(Method::GET)
            .uri(&self.path)
            .header(header::RANGE, range)
            .header(header::ORIGIN, super::APP_ORIGINS[0])
            .body(Vec::new());
        let Ok(request) = request else {
            return (StatusCode::BAD_REQUEST, None, Vec::new());
        };
        let response = super::serve(self.app.clone(), bearer.as_deref(), request).await;
        let content_range = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        (response.status(), content_range, response.into_body())
    }
}

impl RangeSource for HostRoute {
    async fn read_range(&self, span: ByteSpan) -> std::io::Result<Vec<u8>> {
        let last = span
            .end()
            .and_then(|end| end.checked_sub(1))
            .ok_or_else(|| std::io::Error::other("empty span"))?;
        let (status, _, body) = self.get(&format!("bytes={}-{last}", span.offset)).await;
        match status {
            StatusCode::PARTIAL_CONTENT => Ok(body),
            StatusCode::CONFLICT => Err(std::io::Error::other(TrackError::SessionEnded)),
            other => Err(std::io::Error::other(TrackError::Refused(other.as_u16()))),
        }
    }
}

/// The file size a `Content-Range` value states: `bytes 0-0/12345` and
/// `bytes */12345` give 12345.
///
/// # Returns
///
/// `None` for any other shape, or an unknown (`*`) size.
pub(crate) fn content_range_size(value: &str) -> Option<u64> {
    let rest = value.strip_prefix("bytes ")?;
    let (_, size) = rest.split_once('/')?;
    if size.is_empty() || !size.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    size.parse().ok()
}

/// Opens a play session's track for the window's player, decoded to PCM
/// from `start_ms`.
///
/// # Arguments
///
/// * `app` - The host's router.
/// * `state` - The host's state.
/// * `path` - The track's stream path, as the session listed it.
/// * `start_ms` - Where to start, in milliseconds into the track.
///
/// # Errors
///
/// [`TrackError::NotATrack`] for any other path; what the stream route
/// answers for the first byte; [`TrackError::UnknownSize`] when its answer
/// states no size; [`TrackError::Media`] when the worker cannot start or
/// decode the track.
pub(crate) async fn open_track(
    app: Router,
    state: Arc<AppState>,
    path: &str,
    start_ms: u64,
) -> Result<PcmStream, TrackError> {
    let parsed = path
        .strip_prefix("/api/playback/sessions/")
        .and_then(|rest| rest.split_once("/tracks/"))
        .and_then(|(session, index)| parse_track_path(session, index))
        .ok_or(TrackError::NotATrack)?;
    let route = HostRoute {
        app,
        state,
        path: format!(
            "/api/playback/sessions/{}/tracks/{}",
            parsed.session_id, parsed.index
        ),
    };
    let (status, content_range, _) = route.get("bytes=0-0").await;
    let size = match status {
        StatusCode::PARTIAL_CONTENT => content_range
            .as_deref()
            .and_then(content_range_size)
            .ok_or(TrackError::UnknownSize)?,
        StatusCode::CONFLICT => return Err(TrackError::SessionEnded),
        other => return Err(TrackError::Refused(other.as_u16())),
    };
    bookclerk_media::pool()
        .open_pcm(size, start_ms, route)
        .await
        .map_err(|err| TrackError::Media(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_come_from_content_range() {
        assert_eq!(content_range_size("bytes 0-0/12345"), Some(12_345));
        assert_eq!(content_range_size("bytes */9"), Some(9));
        assert_eq!(content_range_size("bytes 0-0/*"), None);
        assert_eq!(content_range_size("bytes 0-0/"), None);
        assert_eq!(content_range_size("bytes 0-0/+5"), None);
        assert_eq!(content_range_size("items 0-0/5"), None);
        assert_eq!(content_range_size("bytes 0-0"), None);
        assert_eq!(
            content_range_size("bytes 0-0/99999999999999999999999"),
            None
        );
    }
}
