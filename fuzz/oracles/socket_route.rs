//! Shared oracle for the `socket_route` fuzz target and the
//! `bookclerk-workerd` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one `POST /sockets/<op>` call a workerd isolate sends on the
//! granted reverse channel: the first byte picks the operation (`0` to `4`
//! name `connect`, `send`, `close`, `attachment` and `list`; anything else is
//! an operation the route does not serve) and the rest is the JSON body.
//! [`parse_socket_call`] must:
//!
//! - never panic;
//! - refuse an unknown operation with `not_found`, and nothing else with it;
//! - refuse a body that is not a JSON object, or that carries a field the
//!   operation does not define, with `invalid_params`;
//! - fail only with `invalid_params`, `payload_too_large` or `not_found`;
//! - accept only calls inside the restated ABI rules: socket ids of 1 to 64
//!   bytes from `[A-Za-z0-9_-]`, frames up to `MAX_SOCKET_FRAME_BYTES`,
//!   attachments up to `MAX_SOCKET_ATTACHMENT_BYTES`, close codes a peer may
//!   send with reasons up to 123 bytes, and connect requests the broker
//!   would dial (the `socket_request` oracle's independent restatement of
//!   `validate_socket_connect`);
//! - parse what it accepts the same way again after the call is written back
//!   out in the route's JSON form.

use bookclerk_plugin_abi::sockets::validate_socket_connect;
use bookclerk_plugin_abi::{
    PluginErrorCode, SocketFrame, MAX_SOCKET_ATTACHMENT_BYTES, MAX_SOCKET_FRAME_BYTES,
};
use bookclerk_workerd::granted_sockets::{
    parse_socket_call, RouteAttachment, RouteAutoResponse, RouteClose, RouteConnect, RouteList,
    RouteSend, SocketCall, SOCKET_ROUTE_OPS,
};

/// The connect rules, restated independently of `validate_socket_connect` in
/// the `socket_request` oracle; this target uses only its connect check.
#[allow(dead_code)]
#[path = "socket_request.rs"]
mod socket_request;

/// Fields each operation's body may carry.
fn allowed_fields(op: &str) -> &'static [&'static str] {
    match op {
        "connect" => &[
            "url",
            "protocols",
            "headers",
            "autoResponses",
            "attachment",
            "trustAnchors",
            "serverName",
        ],
        "send" => &["socket", "frame"],
        "close" => &["socket", "code", "reason"],
        "attachment" => &["socket", "attachment"],
        _ => &[],
    }
}

/// The socket id rule, restated: 1 to 64 bytes of `[A-Za-z0-9_-]`.
fn id_ok(socket: &str) -> bool {
    (1..=64).contains(&socket.len())
        && socket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Bytes one frame carries.
fn frame_len(frame: &SocketFrame) -> usize {
    match frame {
        SocketFrame::Text(text) => text.len(),
        SocketFrame::Binary(bytes) => bytes.len(),
    }
}

/// Writes an accepted call back out as its route JSON body.
fn route_body(call: &SocketCall) -> Vec<u8> {
    let encoded = match call {
        SocketCall::Connect(request) => serde_json::to_vec(&RouteConnect {
            url: request.url.clone(),
            protocols: request.protocols.clone(),
            headers: request.headers.clone(),
            auto_responses: request
                .auto_responses
                .iter()
                .map(|pair| RouteAutoResponse {
                    request: pair.request.clone().into(),
                    response: pair.response.clone().into(),
                })
                .collect(),
            attachment: request.attachment.clone(),
            trust_anchors: request
                .trust_anchors
                .iter()
                .map(|der| bookclerk_plugin_abi::json_bytes::encode(der))
                .collect(),
            server_name: request.server_name.clone(),
        }),
        SocketCall::Send { socket, frame } => serde_json::to_vec(&RouteSend {
            socket: socket.clone(),
            frame: frame.clone().into(),
        }),
        SocketCall::Close {
            socket,
            code,
            reason,
        } => serde_json::to_vec(&RouteClose {
            socket: socket.clone(),
            code: *code,
            reason: reason.clone(),
        }),
        SocketCall::SetAttachment { socket, attachment } => serde_json::to_vec(&RouteAttachment {
            socket: socket.clone(),
            attachment: attachment.clone(),
        }),
        SocketCall::List => serde_json::to_vec(&RouteList {}),
    };
    match encoded {
        Ok(bytes) => bytes,
        Err(err) => panic!("an accepted call must encode: {err}"),
    }
}

/// Checks one input against the invariants in the module docs.
pub fn check(data: &[u8]) {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    let op = SOCKET_ROUTE_OPS
        .get(usize::from(selector))
        .copied()
        .unwrap_or("unknown");
    let parsed = parse_socket_call(op, body);
    if !SOCKET_ROUTE_OPS.contains(&op) {
        let code = parsed.as_ref().err().map(|err| err.code);
        assert_eq!(code, Some(PluginErrorCode::NotFound), "unknown op");
        return;
    }
    let object = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned());
    let stray = object.as_ref().map(|fields| {
        fields
            .keys()
            .any(|key| !allowed_fields(op).contains(&key.as_str()))
    });
    match &parsed {
        Err(err) => {
            assert!(
                matches!(
                    err.code,
                    PluginErrorCode::InvalidParams | PluginErrorCode::PayloadTooLarge
                ),
                "{op}: unexpected {:?}",
                err.code
            );
            return;
        }
        Ok(_) => {
            assert!(
                object.is_some(),
                "{op}: accepted a body that is not an object"
            );
            assert_eq!(stray, Some(false), "{op}: accepted an unknown field");
        }
    }
    let Ok(call) = parsed else {
        return;
    };
    match &call {
        SocketCall::Connect(request) => {
            socket_request::check_connect(request);
            assert!(
                validate_socket_connect(request).is_ok(),
                "connect outside the rules"
            );
        }
        SocketCall::Send { socket, frame } => {
            assert!(id_ok(socket), "send id");
            assert!(
                frame_len(frame) <= MAX_SOCKET_FRAME_BYTES as usize,
                "send frame"
            );
        }
        SocketCall::Close {
            socket,
            code,
            reason,
        } => {
            assert!(id_ok(socket), "close id");
            assert!(
                matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999),
                "close code {code}"
            );
            assert!(reason.len() <= 123, "close reason");
        }
        SocketCall::SetAttachment { socket, attachment } => {
            assert!(id_ok(socket), "attachment id");
            assert!(
                attachment.len() <= MAX_SOCKET_ATTACHMENT_BYTES as usize,
                "attachment size"
            );
        }
        SocketCall::List => assert_eq!(op, "list"),
    }
    let again = parse_socket_call(op, &route_body(&call));
    assert_eq!(again.ok(), Some(call), "{op}: round trip");
}
