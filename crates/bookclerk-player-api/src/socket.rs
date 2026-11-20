//! Engine.IO (v4) and Socket.IO (v5) framing of `/player/socket.io`.
//!
//! The endpoint speaks websocket only: an app opens
//! `/player/socket.io/?EIO=4&transport=websocket`, the host sends the
//! Engine.IO open packet, the app connects to the default namespace and
//! then sends `auth` with its access token, as apps written for an
//! Audiobookshelf server do. After that the host pushes events
//! (`user_item_progress_updated`, `user_updated`, `item_updated`, ...)
//! and pings; the app answers pings and may send its own.
//!
//! Everything here is sans-IO: [`parse_client_frame`] reads one text frame
//! an app sent and the `*_frame` functions build the frames the host
//! sends. Binary frames and polling are refused by the host before they
//! reach this module.

use serde::Serialize;
use serde_json::Value;

/// The longest text frame an app may send, in bytes. An `auth` event with
/// a token is under a hundred bytes; nothing an app sends is near this.
pub const MAX_CLIENT_FRAME_BYTES: usize = 16 * 1024;

/// The longest event name read back, in bytes.
pub const MAX_EVENT_NAME_BYTES: usize = 64;

/// How often the host pings, in milliseconds (Engine.IO's default).
pub const PING_INTERVAL_MS: u64 = 25_000;

/// How long the host waits for a pong before closing, in milliseconds.
pub const PING_TIMEOUT_MS: u64 = 20_000;

/// The Engine.IO protocol revision the endpoint speaks (`EIO=4`).
pub const ENGINE_IO_REVISION: &str = "4";

/// Why a frame an app sent was refused. The host closes the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketError {
    /// The frame is longer than [`MAX_CLIENT_FRAME_BYTES`].
    TooLong,
    /// The frame is empty or its packet type is not one an app sends.
    UnknownPacket,
    /// A Socket.IO packet whose namespace, ack id or payload is malformed.
    Malformed,
}

impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLong => "socket frame too long",
            Self::UnknownPacket => "unknown socket packet",
            Self::Malformed => "malformed socket packet",
        })
    }
}

impl std::error::Error for SocketError {}

/// One text frame an app sent, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    /// Engine.IO close (`1`): the app is leaving.
    Close,
    /// Engine.IO ping (`2`), answered with a pong.
    Ping,
    /// Engine.IO pong (`3`), the answer to the host's ping.
    Pong,
    /// Engine.IO upgrade or noop (`5`, `6`): nothing to do.
    Noop,
    /// Socket.IO connect (`40`) to a namespace.
    Connect {
        /// True when the namespace is the default one (`/`), the only one
        /// the endpoint serves.
        default_namespace: bool,
        /// The `token` field of the connect payload, when the app sent one.
        token: Option<String>,
    },
    /// Socket.IO disconnect (`41`).
    Disconnect,
    /// Socket.IO event (`42`) on the default namespace.
    Event {
        /// The event the app sent.
        event: ClientEvent,
        /// The ack id the app asked to be answered on, if any.
        ack: Option<u64>,
    },
}

/// An event an app sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// `auth` with an access token: ties the socket to a patron.
    Auth(String),
    /// The application-level `ping` event, answered with `pong`.
    Ping,
    /// Any other event (scans, cover search, log listeners): ignored.
    Other(String),
}

/// Reads one text frame an app sent.
///
/// # Arguments
///
/// * `frame` - The websocket text frame.
///
/// # Returns
///
/// The frame.
///
/// # Errors
///
/// [`SocketError`] when the frame is too long, empty, of a packet type an
/// app does not send, or malformed.
pub fn parse_client_frame(frame: &str) -> Result<ClientFrame, SocketError> {
    if frame.len() > MAX_CLIENT_FRAME_BYTES {
        return Err(SocketError::TooLong);
    }
    let mut chars = frame.chars();
    let engine = chars.next().ok_or(SocketError::UnknownPacket)?;
    let rest = chars.as_str();
    match engine {
        '1' if rest.is_empty() => Ok(ClientFrame::Close),
        // A ping or pong may carry a probe string; it is not echoed.
        '2' => Ok(ClientFrame::Ping),
        '3' => Ok(ClientFrame::Pong),
        '5' | '6' if rest.is_empty() => Ok(ClientFrame::Noop),
        '4' => parse_socket_packet(rest),
        _ => Err(SocketError::UnknownPacket),
    }
}

/// Reads a Socket.IO packet: `<type>[<nsp>,][<ack id>][<json>]`.
fn parse_socket_packet(packet: &str) -> Result<ClientFrame, SocketError> {
    let mut chars = packet.chars();
    let kind = chars.next().ok_or(SocketError::UnknownPacket)?;
    let mut rest = chars.as_str();
    let mut default_namespace = true;
    if rest.starts_with('/') {
        let (nsp, after) = rest.split_once(',').unwrap_or((rest, ""));
        if nsp.len() > MAX_EVENT_NAME_BYTES {
            return Err(SocketError::Malformed);
        }
        default_namespace = nsp == "/";
        rest = after;
    }
    match kind {
        '0' => {
            let token = if rest.is_empty() {
                None
            } else {
                let payload: Value =
                    serde_json::from_str(rest).map_err(|_| SocketError::Malformed)?;
                if !payload.is_object() {
                    return Err(SocketError::Malformed);
                }
                payload
                    .get("token")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            Ok(ClientFrame::Connect {
                default_namespace,
                token,
            })
        }
        '1' => Ok(ClientFrame::Disconnect),
        '2' => {
            if !default_namespace {
                return Err(SocketError::Malformed);
            }
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let (ack_digits, json) = rest.split_at(digits);
            let ack = if ack_digits.is_empty() {
                None
            } else if ack_digits.len() > 15 {
                return Err(SocketError::Malformed);
            } else {
                Some(ack_digits.parse().map_err(|_| SocketError::Malformed)?)
            };
            let event = parse_event(json)?;
            Ok(ClientFrame::Event { event, ack })
        }
        _ => Err(SocketError::UnknownPacket),
    }
}

/// Reads an event array: `["name", args...]`.
fn parse_event(json: &str) -> Result<ClientEvent, SocketError> {
    let value: Value = serde_json::from_str(json).map_err(|_| SocketError::Malformed)?;
    let items = value.as_array().ok_or(SocketError::Malformed)?;
    let name = items
        .first()
        .and_then(Value::as_str)
        .ok_or(SocketError::Malformed)?;
    if name.is_empty() || name.len() > MAX_EVENT_NAME_BYTES {
        return Err(SocketError::Malformed);
    }
    Ok(match name {
        "auth" => {
            // Apps send the token as a string; a missing or non-string one
            // is an auth that fails, not a malformed frame.
            let token = items.get(1).and_then(Value::as_str).unwrap_or_default();
            ClientEvent::Auth(token.to_owned())
        }
        "ping" => ClientEvent::Ping,
        other => ClientEvent::Other(other.to_owned()),
    })
}

/// The Engine.IO open packet's payload.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenPayload<'a> {
    /// The Engine.IO session id.
    sid: &'a str,
    /// Transports the app may upgrade to: none, it is already a websocket.
    upgrades: [&'a str; 0],
    /// [`PING_INTERVAL_MS`].
    ping_interval: u64,
    /// [`PING_TIMEOUT_MS`].
    ping_timeout: u64,
    /// [`MAX_CLIENT_FRAME_BYTES`].
    max_payload: usize,
}

/// The Engine.IO open packet (`0{...}`), the first frame the host sends.
///
/// # Arguments
///
/// * `sid` - A fresh, unguessable session id for this socket.
#[must_use]
pub fn open_frame(sid: &str) -> String {
    let payload = OpenPayload {
        sid,
        upgrades: [],
        ping_interval: PING_INTERVAL_MS,
        ping_timeout: PING_TIMEOUT_MS,
        max_payload: MAX_CLIENT_FRAME_BYTES,
    };
    format!("0{}", json(&payload))
}

/// The Socket.IO connect answer (`40{"sid":...}`) on the default namespace.
#[must_use]
pub fn connect_frame(sid: &str) -> String {
    format!("40{}", json(&serde_json::json!({ "sid": sid })))
}

/// The Socket.IO connect error (`44{"message":...}`) for a namespace the
/// endpoint does not serve.
#[must_use]
pub fn connect_error_frame(namespace_message: &str) -> String {
    format!(
        "44{}",
        json(&serde_json::json!({ "message": namespace_message }))
    )
}

/// The Engine.IO ping (`2`) the host sends every [`PING_INTERVAL_MS`].
pub const PING_FRAME: &str = "2";

/// The Engine.IO pong (`3`) answering an app's ping.
pub const PONG_FRAME: &str = "3";

/// The Engine.IO close (`1`).
pub const CLOSE_FRAME: &str = "1";

/// A Socket.IO event (`42["name",data]`) on the default namespace.
///
/// # Arguments
///
/// * `name` - The event name, such as `user_item_progress_updated`.
/// * `data` - Its one argument.
#[must_use]
pub fn event_frame<T: Serialize>(name: &str, data: &T) -> String {
    let data = serde_json::to_value(data).unwrap_or(Value::Null);
    format!("42{}", json(&serde_json::json!([name, data])))
}

/// A Socket.IO event with no argument (`42["name"]`), such as `pong`.
#[must_use]
pub fn bare_event_frame(name: &str) -> String {
    format!("42{}", json(&serde_json::json!([name])))
}

/// Serializes a value the host built; these never fail to serialize.
fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_packets_are_read() {
        assert_eq!(parse_client_frame("1"), Ok(ClientFrame::Close));
        assert_eq!(parse_client_frame("2"), Ok(ClientFrame::Ping));
        assert_eq!(parse_client_frame("2probe"), Ok(ClientFrame::Ping));
        assert_eq!(parse_client_frame("3"), Ok(ClientFrame::Pong));
        assert_eq!(parse_client_frame("6"), Ok(ClientFrame::Noop));
        for bad in ["", "0", "7", "x", "1x", "4", "45[]"] {
            assert!(parse_client_frame(bad).is_err(), "{bad}");
        }
        let long = format!("42[\"auth\",\"{}\"]", "a".repeat(MAX_CLIENT_FRAME_BYTES));
        assert_eq!(parse_client_frame(&long), Err(SocketError::TooLong));
    }

    #[test]
    fn connects_are_read_with_their_namespace_and_token() {
        assert_eq!(
            parse_client_frame("40"),
            Ok(ClientFrame::Connect {
                default_namespace: true,
                token: None
            })
        );
        assert_eq!(
            parse_client_frame("40{\"token\":\"abc\"}"),
            Ok(ClientFrame::Connect {
                default_namespace: true,
                token: Some("abc".into())
            })
        );
        assert_eq!(
            parse_client_frame("40/admin,"),
            Ok(ClientFrame::Connect {
                default_namespace: false,
                token: None
            })
        );
        assert!(parse_client_frame("40[1]").is_err());
        assert_eq!(parse_client_frame("41"), Ok(ClientFrame::Disconnect));
    }

    #[test]
    fn events_are_read() {
        assert_eq!(
            parse_client_frame("42[\"auth\",\"tok\"]"),
            Ok(ClientFrame::Event {
                event: ClientEvent::Auth("tok".into()),
                ack: None
            })
        );
        assert_eq!(
            parse_client_frame("4217[\"ping\"]"),
            Ok(ClientFrame::Event {
                event: ClientEvent::Ping,
                ack: Some(17)
            })
        );
        assert_eq!(
            parse_client_frame("42[\"auth\"]"),
            Ok(ClientFrame::Event {
                event: ClientEvent::Auth(String::new()),
                ack: None
            })
        );
        assert_eq!(
            parse_client_frame("42[\"cancel_scan\",\"x\"]"),
            Ok(ClientFrame::Event {
                event: ClientEvent::Other("cancel_scan".into()),
                ack: None
            })
        );
        for bad in ["42", "42{}", "42[]", "42[1]", "42[\"\"]", "42/x,[\"a\"]"] {
            assert!(parse_client_frame(bad).is_err(), "{bad}");
        }
        assert!(parse_client_frame("421234567890123456[\"ping\"]").is_err());
    }

    #[test]
    fn host_frames_have_the_wire_shape() {
        let open = open_frame("s1");
        assert!(open.starts_with("0{\"sid\":\"s1\",\"upgrades\":[]"));
        assert!(open.contains("\"pingInterval\":25000"));
        assert_eq!(connect_frame("x"), "40{\"sid\":\"x\"}");
        assert_eq!(bare_event_frame("pong"), "42[\"pong\"]");
        assert_eq!(
            event_frame("init", &serde_json::json!({"userId": "u"})),
            "42[\"init\",{\"userId\":\"u\"}]"
        );
        assert!(connect_error_frame("Invalid namespace").starts_with("44{"));
        // What the host sends as an event, an app sending it back reads.
        let echoed = event_frame("auth", &"tok");
        assert_eq!(
            parse_client_frame(&echoed),
            Ok(ClientFrame::Event {
                event: ClientEvent::Auth("tok".into()),
                ack: None
            })
        );
    }
}
