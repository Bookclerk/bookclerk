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
//!   and the connection can never disagree about the target (a `tls://` URL
//!   is non-special to WHATWG, so its host compares without case and its
//!   port must be explicit, with nothing after it);
//! - [`validate_socket_connect`] accepts a decoded request exactly when the
//!   URL parses and the independent restatement of the header, subprotocol,
//!   heartbeat and attachment rules below holds (for `tls://`: ALPN ids of
//!   1 to 255 visible ASCII bytes, no headers and no heartbeats, and trust
//!   anchors that are each one DER SEQUENCE of at most 8 KiB, and a server
//!   name that is empty or a DNS name that isn't an IP address, neither of
//!   which a WebSocket may send);
//! - [`check_socket_frame_for`] refuses text on a `tls://` stream and
//!   otherwise agrees with the frame size check;
//! - [`auto_response_for`] answers exactly the frames equal (same kind, same
//!   bytes) to a declared heartbeat request, with the first such pair;
//! - a delivery passes the id, frame and attachment checks exactly when the
//!   restated bounds hold;
//! - a Cap'n Proto value the RPC codec accepts survives `write` then `read`
//!   unchanged.

use bookclerk_plugin_abi::sockets::{
    auto_response_for, check_socket_attachment, check_socket_frame, check_socket_frame_for,
    check_socket_id, parse_socket_url, validate_socket_connect, SocketScheme, SocketTarget,
};
use bookclerk_plugin_abi::{
    roundtrip_socket_message, RoundTripFailure, SocketAutoResponse, SocketCodec,
    SocketConnectRequest, SocketFrame, SocketWireMessage, MAX_SOCKET_ATTACHMENT_BYTES,
    MAX_SOCKET_AUTO_RESPONSES, MAX_SOCKET_AUTO_RESPONSE_BYTES, MAX_SOCKET_FRAME_BYTES,
    MAX_SOCKET_HEADERS, MAX_SOCKET_SERVER_NAME_BYTES, MAX_SOCKET_TRUST_ANCHORS,
    MAX_SOCKET_TRUST_ANCHOR_BYTES, MAX_SOCKET_URL_BYTES,
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

/// The non-URL rules of a `tls://` connect request restated.
fn stream_rules_hold(request: &SocketConnectRequest) -> bool {
    request.protocols.len() <= MAX_SOCKET_HEADERS as usize
        && request
            .protocols
            .iter()
            .all(|p| (1..=255).contains(&p.len()) && p.bytes().all(|b| (0x21..=0x7e).contains(&b)))
        && request.headers.is_empty()
        && request.auto_responses.is_empty()
        && request.attachment.len() <= MAX_SOCKET_ATTACHMENT_BYTES as usize
        && request.trust_anchors.len() <= MAX_SOCKET_TRUST_ANCHORS as usize
        && request.trust_anchors.iter().all(|der| der_shape(der))
        && server_name_shape(&request.server_name)
}

/// A server name restated as a byte-by-byte state machine: empty, or at
/// most 253 bytes of dot-separated labels of 1 to 63 letters, digits and
/// inner hyphens, none starting `xn--`, the last not all digits (so no IPv4
/// address, and no IPv6 since `:` never passes).
fn server_name_shape(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    if name.len() > MAX_SOCKET_SERVER_NAME_BYTES as usize {
        return false;
    }
    let label_ok = |label: &[u8]| {
        !label.is_empty()
            && label.len() <= 63
            && label[0] != b'-'
            && label[label.len() - 1] != b'-'
            && !(label.len() >= 4 && label[..4].eq_ignore_ascii_case(b"xn--"))
    };
    let (mut label, mut digits_only) = (Vec::new(), true);
    for &b in name.as_bytes() {
        match b {
            b'.' => {
                if !label_ok(&label) {
                    return false;
                }
                label.clear();
                digits_only = true;
            }
            b'0'..=b'9' => label.push(b),
            b'a'..=b'z' | b'A'..=b'Z' | b'-' => {
                label.push(b);
                digits_only = false;
            }
            _ => return false,
        }
    }
    label_ok(&label) && !digits_only
}

/// One DER SEQUENCE covering all of `der`, restated with a general
/// definite-length reader and the size cap.
fn der_shape(der: &[u8]) -> bool {
    if der.len() > MAX_SOCKET_TRUST_ANCHOR_BYTES as usize || der.len() < 2 || der[0] != 0x30 {
        return false;
    }
    let first = der[1];
    if first < 0x80 {
        return usize::from(first) == der.len() - 2;
    }
    let count = usize::from(first & 0x7f);
    if count == 0 || count > 4 || der.len() < 2 + count || der[2] == 0 {
        return false;
    }
    let len = der[2..2 + count]
        .iter()
        .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
    // DER: the long form only for lengths the short form can't carry.
    len >= 0x80 && len == der.len() - 2 - count
}

/// The non-URL connect rules restated.
fn connect_rules_hold(request: &SocketConnectRequest, scheme: SocketScheme) -> bool {
    if scheme == SocketScheme::Tls {
        return stream_rules_hold(request);
    }
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
    protocols_ok
        && headers_ok
        && heartbeats_ok
        && attachment_ok
        && request.trust_anchors.is_empty()
        && request.server_name.is_empty()
}

/// Checks an accepted URL against the WHATWG parse.
fn check_accepted_url(url: &str, target: &SocketTarget) {
    assert!(
        url.len() <= MAX_SOCKET_URL_BYTES as usize,
        "overlong url accepted"
    );
    let parsed = url::Url::parse(url)
        .unwrap_or_else(|err| panic!("accepted {url:?} that WHATWG refuses: {err}"));
    let stream = target.scheme == SocketScheme::Tls;
    let scheme = match target.scheme {
        SocketScheme::Ws => "ws",
        SocketScheme::Wss => "wss",
        SocketScheme::Tls => "tls",
    };
    assert_eq!(parsed.scheme(), scheme, "scheme of {url:?}");
    if stream {
        assert!(
            matches!(parsed.path(), "" | "/") && parsed.query().is_none(),
            "tls url {url:?} has more than a host and port"
        );
        assert!(parsed.port().is_some(), "tls url {url:?} has no port");
    }
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
        // WHATWG keeps a non-special (tls://) host's case.
        _ if stream => assert!(host.eq_ignore_ascii_case(&target.host), "host of {url:?}"),
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

/// Checks a decoded connect request against the restated rules.
pub fn check_connect(request: &SocketConnectRequest) {
    check_url(&request.url);
    let expected = parse_socket_url(&request.url)
        .is_ok_and(|target| connect_rules_hold(request, target.scheme));
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
    let fits = frame_len(frame) <= MAX_SOCKET_FRAME_BYTES as usize;
    assert_eq!(check_socket_frame(frame).is_ok(), fits);
    for scheme in [SocketScheme::Ws, SocketScheme::Wss, SocketScheme::Tls] {
        let text = matches!(frame, SocketFrame::Text(_));
        let expected = fits && !(scheme == SocketScheme::Tls && text);
        assert_eq!(check_socket_frame_for(scheme, frame).is_ok(), expected);
    }
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
