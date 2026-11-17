//! Audiobookshelf's pushed events, read from its Socket.io channel.
//!
//! Audiobookshelf pushes changes to signed-in admins over Socket.io (Engine.IO
//! v4 over a WebSocket). The host holds that socket while the guest sleeps
//! (`[sockets]`): it answers the server's `2` ping with `3` itself, and wakes
//! the guest for every other frame. This module is the sans-IO side of that
//! conversation: [`socket_url`] names the endpoint, and [`on_frame`] reads one
//! text frame and says what to send back, whether to close, and whether the
//! frame announced a new user.
//!
//! The handshake is:
//!
//! 1. The server opens Engine.IO with `0{...}`; the guest joins the default
//!    namespace with `40`.
//! 2. The server confirms with `40{...}`; the guest authenticates with
//!    `42["auth","<api key>"]`.
//! 3. The server answers `42["init",...]` and from then on pushes events
//!    such as `42["user_added",{...}]`. A rejected key gets
//!    `42["invalid_token"]`.

use serde_json::Value;

/// The Socket.io path Audiobookshelf serves, with the Engine.IO query.
const SOCKET_PATH: &str = "/socket.io/?EIO=4&transport=websocket";

/// The Engine.IO ping the server sends; the host answers it without waking
/// the guest.
pub const PING: &str = "2";

/// The Engine.IO pong the host sends back for [`PING`].
pub const PONG: &str = "3";

/// What the guest does after one pushed frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reaction {
    /// A text frame to send back, if any.
    pub reply: Option<String>,
    /// Whether to close the socket: the server rejected the key or left the
    /// namespace.
    pub close: bool,
    /// Whether the frame announced a user created on the server, so the
    /// host should look for new users now.
    pub new_user: bool,
}

/// The WebSocket URL of the Socket.io endpoint under `base_url`.
///
/// `http` becomes `ws` and `https` becomes `wss`; a path prefix on the base
/// URL (Audiobookshelf behind a sub-path) is kept.
///
/// # Arguments
///
/// * `base_url` - The configured server origin, such as `https://abs.example`.
///
/// # Returns
///
/// `None` when the base URL is not `http` or `https`.
#[must_use]
pub fn socket_url(base_url: &str) -> Option<String> {
    let base = base_url.trim().trim_end_matches('/');
    let rest = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        format!("ws://{}", base.strip_prefix("http://")?)
    };
    if rest.ends_with("://") {
        return None;
    }
    Some(format!("{rest}{SOCKET_PATH}"))
}

/// Reads one text frame from the server and says how to answer it.
///
/// Unknown or malformed frames are ignored, so a server that adds events or
/// sends noise never breaks the socket.
///
/// # Arguments
///
/// * `frame` - The text frame as received.
/// * `api_key` - The key to authenticate with once the namespace is joined.
///
/// # Returns
///
/// The reply to send, whether to close, and whether a user was added.
#[must_use]
pub fn on_frame(frame: &str, api_key: &str) -> Reaction {
    match frame.as_bytes().first() {
        // Engine.IO open: join the default namespace.
        Some(b'0') => Reaction {
            reply: Some("40".into()),
            ..Reaction::default()
        },
        // Engine.IO close.
        Some(b'1') => Reaction {
            close: true,
            ..Reaction::default()
        },
        Some(b'4') => on_socket_io(&frame[1..], api_key),
        _ => Reaction::default(),
    }
}

/// Reads a Socket.io packet (an Engine.IO message without its `4`).
fn on_socket_io(packet: &str, api_key: &str) -> Reaction {
    match packet.as_bytes().first() {
        // Namespace joined: authenticate.
        Some(b'0') => Reaction {
            reply: Some(auth_frame(api_key)),
            ..Reaction::default()
        },
        // Namespace left, or the connect was refused.
        Some(b'1' | b'4') => Reaction {
            close: true,
            ..Reaction::default()
        },
        Some(b'2') => on_event(&packet[1..]),
        _ => Reaction::default(),
    }
}

/// Reads a Socket.io event's JSON array (after its `2` packet type).
fn on_event(body: &str) -> Reaction {
    // An event may carry an ack id before the array; the default namespace
    // carries no prefix. Skip to the array either way.
    let Some(start) = body.find('[') else {
        return Reaction::default();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&body[start..]) else {
        return Reaction::default();
    };
    match items.first().and_then(Value::as_str) {
        Some("user_added") => Reaction {
            new_user: true,
            ..Reaction::default()
        },
        Some("invalid_token") => Reaction {
            close: true,
            ..Reaction::default()
        },
        _ => Reaction::default(),
    }
}

/// The `auth` event carrying `api_key`.
fn auth_frame(api_key: &str) -> String {
    let event = Value::Array(vec![
        Value::String("auth".into()),
        Value::String(api_key.into()),
    ]);
    format!("42{event}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_url_follows_the_base_url() {
        assert_eq!(
            socket_url("https://abs.example/").as_deref(),
            Some("wss://abs.example/socket.io/?EIO=4&transport=websocket")
        );
        assert_eq!(
            socket_url("http://127.0.0.1:13378").as_deref(),
            Some("ws://127.0.0.1:13378/socket.io/?EIO=4&transport=websocket")
        );
        assert_eq!(
            socket_url("https://home.example/abs").as_deref(),
            Some("wss://home.example/abs/socket.io/?EIO=4&transport=websocket")
        );
        assert_eq!(socket_url("ftp://abs.example"), None);
        assert_eq!(socket_url("https://"), None);
        assert_eq!(socket_url(""), None);
    }

    #[test]
    fn the_handshake_joins_then_authenticates() {
        let open = on_frame(r#"0{"sid":"x","pingInterval":25000}"#, "key");
        assert_eq!(open.reply.as_deref(), Some("40"));
        let joined = on_frame(r#"40{"sid":"y"}"#, "k\"ey");
        assert_eq!(joined.reply.as_deref(), Some(r#"42["auth","k\"ey"]"#));
        assert!(!joined.close && !joined.new_user);
        assert_eq!(
            on_frame(r#"42["init",{"usersOnline":[]}]"#, "key"),
            Reaction::default()
        );
    }

    #[test]
    fn a_user_added_event_asks_for_a_check() {
        let added = on_frame(r#"42["user_added",{"id":"u1","username":"ada"}]"#, "key");
        assert!(added.new_user);
        assert_eq!(added.reply, None);
        assert!(
            on_frame(r#"421["user_added",{}]"#, "key").new_user,
            "an ack id"
        );
        assert!(!on_frame(r#"42["user_online",{"id":"u1"}]"#, "key").new_user);
        assert!(!on_frame(r#"42["item_updated",{}]"#, "key").new_user);
    }

    #[test]
    fn a_rejected_key_or_a_disconnect_closes() {
        assert!(on_frame(r#"42["invalid_token"]"#, "key").close);
        assert!(on_frame("41", "key").close);
        assert!(on_frame(r#"44{"message":"no"}"#, "key").close);
        assert!(on_frame("1", "key").close);
    }

    #[test]
    fn noise_is_ignored() {
        for frame in [
            "",
            "3",
            "6",
            "42",
            "42[",
            "42{}",
            "42[1]",
            "4",
            "\u{1F600}",
            "40",
        ] {
            let reaction = on_frame(frame, "key");
            assert!(!reaction.new_user && !reaction.close, "{frame:?}");
        }
    }
}
