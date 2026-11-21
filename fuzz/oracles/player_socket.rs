//! Shared oracle for the `player_socket` fuzz target and the
//! `bookclerk-player-api` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is every text frame a player app sends on the host's
//! live socket, `GET /player/socket.io/` (Engine.IO 4 and Socket.IO 5
//! over a websocket), read by `parse_client_frame`. The invariants,
//! checked from the frame's leading characters rather than through the
//! parser's own reading:
//!
//! - A frame over 16 KiB is refused as too long.
//! - The Engine.IO packet type decides the frame's kind: `1` alone is a
//!   close, `2…` a ping, `3…` a pong, `5` or `6` alone nothing, and only
//!   a frame starting `4` is a Socket.IO packet; `40` is a connect, `41`
//!   a disconnect and `42` an event. Any other accepted shape fails.
//! - An `auth` event carries the event's first argument when it is a
//!   string (else an empty token), and any other event's name is at most
//!   64 bytes.

use bookclerk_player_api::socket::{
    parse_client_frame, ClientEvent, ClientFrame, SocketError, MAX_CLIENT_FRAME_BYTES,
    MAX_EVENT_NAME_BYTES,
};

/// Largest input worth checking: past the frame limit.
pub const MAX_INPUT_BYTES: usize = MAX_CLIENT_FRAME_BYTES + 64;

/// Checks one frame.
///
/// # Panics
///
/// When the parser breaks a rule.
#[allow(clippy::expect_used)] // The oracle re-reads frames the parser accepted.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(frame) = std::str::from_utf8(data) else {
        return;
    };
    let parsed = parse_client_frame(frame);
    if frame.len() > MAX_CLIENT_FRAME_BYTES {
        assert_eq!(parsed, Err(SocketError::TooLong));
        return;
    }
    let Ok(parsed) = parsed else {
        return;
    };
    match parsed {
        ClientFrame::Close => assert_eq!(frame, "1"),
        ClientFrame::Ping => assert!(frame.starts_with('2')),
        ClientFrame::Pong => assert!(frame.starts_with('3')),
        ClientFrame::Noop => assert!(frame == "5" || frame == "6"),
        ClientFrame::Connect { .. } => assert!(frame.starts_with("40")),
        ClientFrame::Disconnect => assert!(frame.starts_with("41")),
        ClientFrame::Event { event, .. } => {
            assert!(frame.starts_with("42"));
            let start = frame.find('[').expect("an event's array");
            let array: serde_json::Value =
                serde_json::from_str(&frame[start..]).expect("an event's JSON array");
            let name = array[0].as_str().expect("an event name");
            match event {
                ClientEvent::Auth(token) => {
                    assert_eq!(name, "auth");
                    // A missing or non-string token is an auth that fails.
                    assert_eq!(array[1].as_str().unwrap_or_default(), token);
                }
                ClientEvent::Ping => assert_eq!(name, "ping"),
                ClientEvent::Other(other) => {
                    assert_eq!(other, name);
                    assert!(other.len() <= MAX_EVENT_NAME_BYTES);
                }
            }
        }
    }
}
