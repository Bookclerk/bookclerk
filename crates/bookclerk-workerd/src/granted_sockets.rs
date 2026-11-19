//! `POST /sockets/<op>` on the granted reverse channel: a workerd isolate's
//! `env.SOCKETS` calls, parsed and answered over plain byte slices.
//!
//! The isolate holds a sockets-only grant token minted at `open`; each call
//! names its operation in the path (`connect`, `send`, `close`,
//! `attachment`, `list`) and carries that operation's fields as JSON. Binary
//! frames and attachments travel as base64. Every request is checked with the
//! same `bookclerk_plugin_abi::sockets` rules the host applies again when it
//! serves `Sockets`, so a malformed call fails here with a typed error and
//! never reaches the host. Keeping parsing sans-IO lets the `socket_request`
//! fuzz target drive the code the bridge runs.

use bookclerk_plugin_abi::sockets::{
    check_socket_attachment, check_socket_close, check_socket_frame, check_socket_id,
    validate_socket_connect,
};
use bookclerk_plugin_abi::{
    PluginError, SocketAutoResponse, SocketConnectRequest, SocketFrame, SocketHeader, SocketInfo,
    MAX_SOCKET_FRAME_BYTES,
};
use serde::{Deserialize, Serialize};

/// Largest `/sockets/*` request body: one frame at `maxSocketFrameBytes` in
/// its worst JSON form, plus room for a connect request's URL, headers and
/// heartbeat pairs. A text frame of control characters escapes to six bytes
/// per byte (`\u0000`), which outgrows base64's four per three. The ABI checks
/// bound each field once decoded; this only bounds the read.
pub const MAX_SOCKET_ROUTE_BODY_BYTES: u32 = MAX_SOCKET_FRAME_BYTES * 6 + 64 * 1024;

/// Operations the route serves, in path spelling.
pub const SOCKET_ROUTE_OPS: [&str; 5] = ["connect", "send", "close", "attachment", "list"];

/// One message as the route carries it: `{"text": "..."}` or
/// `{"binary": "<base64>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum RouteFrame {
    /// Text message, UTF-8.
    Text(String),
    /// Binary message, base64 on the wire.
    Binary(#[serde(with = "bookclerk_plugin_abi::json_bytes::b64")] Vec<u8>),
}

impl From<RouteFrame> for SocketFrame {
    fn from(frame: RouteFrame) -> Self {
        match frame {
            RouteFrame::Text(text) => Self::Text(text),
            RouteFrame::Binary(bytes) => Self::Binary(bytes),
        }
    }
}

impl From<SocketFrame> for RouteFrame {
    fn from(frame: SocketFrame) -> Self {
        match frame {
            SocketFrame::Text(text) => Self::Text(text),
            SocketFrame::Binary(bytes) => Self::Binary(bytes),
        }
    }
}

/// One heartbeat pair as the route carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteAutoResponse {
    /// Frame to match.
    pub request: RouteFrame,
    /// Frame to send back.
    pub response: RouteFrame,
}

/// `POST /sockets/connect` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteConnect {
    /// `wss://` or `ws://` URL on the plugin's network grant.
    pub url: String,
    /// Requested subprotocols.
    #[serde(default)]
    pub protocols: Vec<String>,
    /// Extra request headers. Values may be credentials.
    #[serde(default)]
    pub headers: Vec<SocketHeader>,
    /// Heartbeats the broker answers without waking the guest.
    #[serde(default)]
    pub auto_responses: Vec<RouteAutoResponse>,
    /// Initial attachment, base64.
    #[serde(default, with = "bookclerk_plugin_abi::json_bytes::b64")]
    pub attachment: Vec<u8>,
}

/// `POST /sockets/send` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteSend {
    /// Socket id from `connect` or `list`.
    pub socket: String,
    /// The message.
    pub frame: RouteFrame,
}

/// `POST /sockets/close` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteClose {
    /// Socket id.
    pub socket: String,
    /// WebSocket close code; zero or absent sends 1000.
    #[serde(default)]
    pub code: u16,
    /// Close reason, at most 123 UTF-8 bytes.
    #[serde(default)]
    pub reason: String,
}

/// `POST /sockets/attachment` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteAttachment {
    /// Socket id.
    pub socket: String,
    /// New attachment, base64.
    #[serde(with = "bookclerk_plugin_abi::json_bytes::b64")]
    pub attachment: Vec<u8>,
}

/// `POST /sockets/list` body: an empty object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteList {}

/// One checked `env.SOCKETS` call, ready for the host's `Sockets` stub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketCall {
    /// `Sockets.connect`.
    Connect(SocketConnectRequest),
    /// `Sockets.send`.
    Send {
        /// Socket id.
        socket: String,
        /// The message.
        frame: SocketFrame,
    },
    /// `Sockets.close`.
    Close {
        /// Socket id.
        socket: String,
        /// Close code to send (zero already mapped to 1000).
        code: u16,
        /// Close reason.
        reason: String,
    },
    /// `Sockets.setAttachment`.
    SetAttachment {
        /// Socket id.
        socket: String,
        /// New attachment.
        attachment: Vec<u8>,
    },
    /// `Sockets.list`.
    List,
}

/// What the host answered one [`SocketCall`] with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketReply {
    /// `connect` opened this socket.
    Connected(SocketInfo),
    /// `send`, `close` or `attachment` finished.
    Done,
    /// `list` found these open sockets, oldest first.
    Listed(Vec<SocketInfo>),
}

/// Decodes `body` as the JSON object of one route operation.
///
/// Serde would also read a struct from a JSON array of its fields in order;
/// the route takes only an object, so the shape is checked first.
fn decode<T: serde::de::DeserializeOwned>(op: &str, body: &[u8]) -> Result<T, PluginError> {
    let malformed = |err: serde_json::Error| {
        PluginError::invalid_params(format!("malformed sockets {op}: {err}"))
    };
    let value: serde_json::Value = serde_json::from_slice(body).map_err(malformed)?;
    if !value.is_object() {
        return Err(PluginError::invalid_params(format!(
            "malformed sockets {op}: the body must be a JSON object"
        )));
    }
    serde_json::from_value(value).map_err(malformed)
}

/// Parses and checks one `POST /sockets/<op>` call.
///
/// # Arguments
///
/// * `op` - Path segment after `/sockets/`.
/// * `body` - Request body bytes, already bounded by
///   [`MAX_SOCKET_ROUTE_BODY_BYTES`].
///
/// # Returns
///
/// The call, with every field within the ABI limits.
///
/// # Errors
///
/// Returns `not_found` for an operation the route does not serve,
/// `invalid_params` for malformed JSON, an unknown field, a malformed socket
/// id, URL, header, subprotocol, heartbeat or close code, and
/// `payload_too_large` for an oversized frame or attachment.
pub fn parse_socket_call(op: &str, body: &[u8]) -> Result<SocketCall, PluginError> {
    match op {
        "connect" => {
            let route: RouteConnect = decode(op, body)?;
            let request = SocketConnectRequest {
                url: route.url,
                protocols: route.protocols,
                headers: route.headers,
                auto_responses: route
                    .auto_responses
                    .into_iter()
                    .map(|pair| SocketAutoResponse {
                        request: pair.request.into(),
                        response: pair.response.into(),
                    })
                    .collect(),
                attachment: route.attachment,
            };
            validate_socket_connect(&request)?;
            Ok(SocketCall::Connect(request))
        }
        "send" => {
            let route: RouteSend = decode(op, body)?;
            check_socket_id(&route.socket)?;
            let frame = SocketFrame::from(route.frame);
            check_socket_frame(&frame)?;
            Ok(SocketCall::Send {
                socket: route.socket,
                frame,
            })
        }
        "close" => {
            let route: RouteClose = decode(op, body)?;
            check_socket_id(&route.socket)?;
            let code = check_socket_close(route.code, &route.reason)?;
            Ok(SocketCall::Close {
                socket: route.socket,
                code,
                reason: route.reason,
            })
        }
        "attachment" => {
            let route: RouteAttachment = decode(op, body)?;
            check_socket_id(&route.socket)?;
            check_socket_attachment(&route.attachment)?;
            Ok(SocketCall::SetAttachment {
                socket: route.socket,
                attachment: route.attachment,
            })
        }
        "list" => {
            let RouteList {} = decode(op, body)?;
            Ok(SocketCall::List)
        }
        other => Err(PluginError::not_found(format!(
            "unknown sockets operation {other:?}"
        ))),
    }
}

/// `list` reply body.
#[derive(Serialize)]
struct ListedBody<'a> {
    /// Open sockets, oldest first.
    sockets: &'a [SocketInfo],
}

/// Encodes a host answer as the route's JSON reply body.
///
/// `connect` answers the [`SocketInfo`] (attachment as base64), `list`
/// answers `{"sockets": [...]}`, and the rest answer `{}`.
///
/// # Errors
///
/// Returns the serializer's error, which a well-formed [`SocketInfo`] never
/// produces.
pub fn socket_reply_json(reply: &SocketReply) -> serde_json::Result<Vec<u8>> {
    match reply {
        SocketReply::Connected(info) => serde_json::to_vec(info),
        SocketReply::Done => Ok(b"{}".to_vec()),
        SocketReply::Listed(sockets) => serde_json::to_vec(&ListedBody { sockets }),
    }
}

#[cfg(test)]
#[path = "granted_sockets_tests.rs"]
mod tests;
