//! Shared oracle for the `socket_request` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one host-brokered socket message. Byte 0 modulo 3 selects
//! the form: 0 is a socket URL (lossy UTF-8), 1 is a Cap'n Proto
//! `SocketConnectRequest` a guest sends to `Sockets.connect`, and 2 is a
//! Cap'n Proto `SocketMessage` delivered to a guest's `SocketHandler` (both
//! unpacked, single segment). The invariants:
//!
//! - parsing, decoding and validation never panic;
//! - every URL [`parse_socket_url`] accepts is one a WHATWG URL parser (the
//!   `url` crate, which WebSocket clients use) reads as the same scheme,
//!   host and port, with no credentials or fragment, so the network grant
//!   and the connection can never disagree about the target;
//! - [`validate_socket_connect`] accepts a decoded request exactly when the
//!   URL parses and the independent restatement of the header, subprotocol,
//!   heartbeat and attachment rules below holds;
//! - [`auto_response_for`] answers exactly the frames equal (same kind, same
//!   bytes) to a declared heartbeat request, with the first such pair;
//! - a delivery passes the id, frame and attachment checks exactly when the
//!   restated bounds hold;
//! - a Cap'n Proto value the RPC codec accepts survives `write` then `read`
//!   unchanged.

use bookclerk_plugin_abi::sockets::{
    auto_response_for, check_socket_attachment, check_socket_frame, check_socket_id,
    parse_socket_url, validate_socket_connect, SocketTarget,
};
use bookclerk_plugin_abi::{
    roundtrip_socket_message, RoundTripFailure, SocketAutoResponse, SocketCodec,
    SocketConnectRequest, SocketFrame, SocketWireMessage, MAX_SOCKET_ATTACHMENT_BYTES,
    MAX_SOCKET_AUTO_RESPONSES, MAX_SOCKET_AUTO_RESPONSE_BYTES, MAX_SOCKET_FRAME_BYTES,
    MAX_SOCKET_HEADERS, MAX_SOCKET_URL_BYTES,
};

/// Largest input worth parsing; a real connect request is under 64 KiB.
pub const MAX_INPUT_BYTES: usize = 512 * 1024;

/// Headers a guest may never set, restated (lowercase).
const BROKER_HEADERS: [&str; 10] = [
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// RFC 9110 `tchar`, restated.
fn tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// True when `text` is a non-empty token.
fn token(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(tchar)
}

/// Byte length of a frame's payload.
fn frame_len(frame: &SocketFrame) -> usize {
    match frame {
        SocketFrame::Text(text) => text.len(),
        SocketFrame::Binary(bytes) => bytes.len(),
    }
}

/// The non-URL connect rules restated.
fn connect_rules_hold(request: &SocketConnectRequest) -> bool {
    let max_headers = MAX_SOCKET_HEADERS as usize;
    let protocols_ok =
        request.protocols.len() <= max_headers && request.protocols.iter().all(|p| token(p));
    let headers_ok = request.headers.len() <= max_headers
        && request.headers.iter().all(|h| {
            let lower = h.name.to_ascii_lowercase();
            token(&h.name)
                && !BROKER_HEADERS.contains(&lower.as_str())
                && !lower.starts_with("sec-websocket-")
                && h.value.len() <= MAX_SOCKET_URL_BYTES as usize
                && h.value
                    .bytes()
                    .all(|b| b == b'\t' || (b >= b' ' && b != 0x7f))
        });
    let heartbeats_ok = request.auto_responses.len() <= MAX_SOCKET_AUTO_RESPONSES as usize
        && request.auto_responses.iter().all(|pair| {
            [&pair.request, &pair.response].iter().all(|frame| {
                let len = frame_len(frame);
                len >= 1 && len <= MAX_SOCKET_AUTO_RESPONSE_BYTES as usize
            })
        });
    let attachment_ok = request.attachment.len() <= MAX_SOCKET_ATTACHMENT_BYTES as usize;
    protocols_ok && headers_ok && heartbeats_ok && attachment_ok
}

/// Checks an accepted URL against the WHATWG parse.
fn check_accepted_url(url: &str, target: &SocketTarget) {
    assert!(
        url.len() <= MAX_SOCKET_URL_BYTES as usize,
        "overlong url accepted"
    );
    let parsed = url::Url::parse(url)
        .unwrap_or_else(|err| panic!("accepted {url:?} that WHATWG refuses: {err}"));
    let scheme = if target.secure { "wss" } else { "ws" };
    assert_eq!(parsed.scheme(), scheme, "scheme of {url:?}");
    assert!(parsed.username().is_empty(), "credentials in {url:?}");
    assert!(parsed.password().is_none(), "credentials in {url:?}");
    assert!(parsed.fragment().is_none(), "fragment in {url:?}");
    let host = parsed
        .host_str()
        .unwrap_or_else(|| panic!("accepted {url:?} with no WHATWG host"));
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match (
        host.parse::<std::net::IpAddr>(),
        target.host.parse::<std::net::IpAddr>(),
    ) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "address of {url:?}"),
        _ => assert_eq!(host, target.host, "host of {url:?}"),
    }
    assert_eq!(
        parsed.port_or_known_default(),
        Some(target.port),
        "port of {url:?}"
    );
}

/// Runs one URL through the parser and the WHATWG comparison.
fn check_url(url: &str) {
    if let Ok(target) = parse_socket_url(url) {
        check_accepted_url(url, &target);
    }
}

/// Checks a decoded connect request.
fn check_connect(request: &SocketConnectRequest) {
    check_url(&request.url);
    let expected = parse_socket_url(&request.url).is_ok() && connect_rules_hold(request);
    let actual = validate_socket_connect(request);
    assert_eq!(
        actual.is_ok(),
        expected,
        "validate_socket_connect disagrees with the restated rules: {actual:?}"
    );
    if actual.is_ok() {
        check_heartbeats(&request.auto_responses);
    }
}

/// Checks heartbeat matching against a linear restatement.
fn check_heartbeats(pairs: &[SocketAutoResponse]) {
    for pair in pairs {
        let first = pairs
            .iter()
            .find(|p| p.request == pair.request)
            .map(|p| &p.response);
        assert_eq!(auto_response_for(&pair.request, pairs), first);
        let flipped = match &pair.request {
            SocketFrame::Text(text) => SocketFrame::Binary(text.as_bytes().to_vec()),
            SocketFrame::Binary(bytes) => {
                SocketFrame::Text(String::from_utf8_lossy(bytes).into_owned())
            }
        };
        let expected = pairs
            .iter()
            .find(|p| p.request == flipped)
            .map(|p| &p.response);
        assert_eq!(auto_response_for(&flipped, pairs), expected);
    }
}

/// Checks the delivery checks against restated bounds.
fn check_delivery(socket: &str, frame: &SocketFrame, attachment: &[u8]) {
    let id_ok = !socket.is_empty()
        && socket.len() <= 64
        && socket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    assert_eq!(
        check_socket_id(socket).is_ok(),
        id_ok,
        "socket id {socket:?}"
    );
    assert_eq!(
        check_socket_frame(frame).is_ok(),
        frame_len(frame) <= MAX_SOCKET_FRAME_BYTES as usize
    );
    assert_eq!(
        check_socket_attachment(attachment).is_ok(),
        attachment.len() <= MAX_SOCKET_ATTACHMENT_BYTES as usize
    );
}

/// Runs one input through the oracle.
///
/// # Panics
///
/// When an invariant in the module docs breaks.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, bytes)) = data.split_first() else {
        return;
    };
    match selector % 3 {
        0 => check_url(&String::from_utf8_lossy(bytes)),
        1 => match roundtrip_socket_message(SocketCodec::ConnectRequest, bytes) {
            Ok(SocketWireMessage::ConnectRequest(request)) => check_connect(&request),
            Ok(other) => panic!("connect codec returned {other:?}"),
            Err(RoundTripFailure::Codec(why)) => panic!("{why}"),
            Err(RoundTripFailure::Rejected(_)) => {}
        },
        _ => match roundtrip_socket_message(SocketCodec::Message, bytes) {
            Ok(SocketWireMessage::Message(event)) => {
                check_delivery(&event.socket, &event.frame, &event.attachment);
            }
            Ok(other) => panic!("message codec returned {other:?}"),
            Err(RoundTripFailure::Codec(why)) => panic!("{why}"),
            Err(RoundTripFailure::Rejected(_)) => {}
        },
    }
}
