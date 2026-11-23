//! Sans-IO checks for host-brokered sockets (`Bindings.sockets`).
//!
//! A socket is a WebSocket (`ws://`, `wss://`) or a raw TLS byte stream
//! (`tls://host:port`) whose chunks travel as binary frames.
//!
//! Every side applies the same rules: the guest SDK before it sends, the host
//! when it serves `Sockets`, and the broker before it dials or answers a
//! heartbeat. Keeping them here, over plain values, lets the fuzz target and
//! the unit tests drive the code the broker runs.

use crate::generated::{SocketAutoResponse, SocketConnectRequest, SocketFrame};
use crate::limits::{
    MAX_SOCKET_ATTACHMENT_BYTES, MAX_SOCKET_AUTO_RESPONSES, MAX_SOCKET_AUTO_RESPONSE_BYTES,
    MAX_SOCKET_FRAME_BYTES, MAX_SOCKET_HEADERS, MAX_SOCKET_TRUST_ANCHORS,
    MAX_SOCKET_TRUST_ANCHOR_BYTES, MAX_SOCKET_URL_BYTES,
};
use crate::{PluginError, Result};

/// Longest close reason a WebSocket close frame can carry, in UTF-8 bytes.
pub const MAX_SOCKET_CLOSE_REASON_BYTES: usize = 123;

/// Close code the broker sends when the guest passes zero.
pub const SOCKET_CLOSE_NORMAL: u16 = 1000;

/// Close code reported when a connection dropped without a close frame.
pub const SOCKET_CLOSE_ABNORMAL: u16 = 1006;

/// Request headers the broker sets itself or that would change the
/// connection rather than the request. Compared case-insensitively.
const FORBIDDEN_HEADERS: &[&str] = &[
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

/// What kind of connection a socket URL asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketScheme {
    /// `ws://`: a WebSocket over plain TCP.
    Ws,
    /// `wss://`: a WebSocket over TLS.
    Wss,
    /// `tls://`: a raw TLS byte stream; frames are binary chunks.
    Tls,
}

impl SocketScheme {
    /// True when the connection runs over TLS (`wss://` and `tls://`).
    #[must_use]
    pub fn uses_tls(self) -> bool {
        match self {
            Self::Ws => false,
            Self::Wss | Self::Tls => true,
        }
    }

    /// True for a raw byte stream rather than a WebSocket.
    #[must_use]
    pub fn is_stream(self) -> bool {
        match self {
            Self::Ws | Self::Wss => false,
            Self::Tls => true,
        }
    }

    /// The scheme as a URL spells it, lowercase.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ws => "ws",
            Self::Wss => "wss",
            Self::Tls => "tls",
        }
    }
}

/// Longest ALPN protocol id a `tls://` stream may offer, in bytes (the TLS
/// limit).
pub const MAX_SOCKET_ALPN_BYTES: usize = 255;

/// Where a validated socket URL points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketTarget {
    /// The kind of connection.
    pub scheme: SocketScheme,
    /// Host name or IP literal, lowercased, without brackets.
    pub host: String,
    /// Port, defaulted from the scheme when the URL names none.
    pub port: u16,
}

/// Number of bytes a frame carries.
#[must_use]
pub fn socket_frame_len(frame: &SocketFrame) -> usize {
    match frame {
        SocketFrame::Text(text) => text.len(),
        SocketFrame::Binary(bytes) => bytes.len(),
    }
}

/// Fails closed when `frame` is larger than `maxSocketFrameBytes`.
///
/// # Errors
///
/// Returns `payload_too_large` for an oversized frame.
pub fn check_socket_frame(frame: &SocketFrame) -> Result<()> {
    let len = socket_frame_len(frame);
    if len > MAX_SOCKET_FRAME_BYTES as usize {
        return Err(PluginError::payload_too_large(format!(
            "socket frame of {len} bytes exceeds {MAX_SOCKET_FRAME_BYTES}"
        )));
    }
    Ok(())
}

/// Fails closed when `attachment` is larger than `maxSocketAttachmentBytes`.
///
/// # Errors
///
/// Returns `payload_too_large` for an oversized attachment.
pub fn check_socket_attachment(attachment: &[u8]) -> Result<()> {
    if attachment.len() > MAX_SOCKET_ATTACHMENT_BYTES as usize {
        return Err(PluginError::payload_too_large(format!(
            "socket attachment of {} bytes exceeds {MAX_SOCKET_ATTACHMENT_BYTES}",
            attachment.len()
        )));
    }
    Ok(())
}

/// Checks a socket id: non-empty, at most 64 bytes, `[A-Za-z0-9_-]` only.
///
/// # Errors
///
/// Returns `invalid_params` for any other id.
pub fn check_socket_id(socket: &str) -> Result<()> {
    let ok = !socket.is_empty()
        && socket.len() <= 64
        && socket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(PluginError::invalid_params("malformed socket id"))
    }
}

/// Normalizes a close request into the code and reason the broker sends.
///
/// # Arguments
///
/// * `code` - Close code from the guest; zero means 1000.
/// * `reason` - Close reason from the guest.
///
/// # Returns
///
/// The code to send.
///
/// # Errors
///
/// Returns `invalid_params` for a code a peer may not send (below 1000, the
/// reserved 1004 to 1006 and 1015, 1016 to 2999, or above 4999) or a reason
/// over [`MAX_SOCKET_CLOSE_REASON_BYTES`].
pub fn check_socket_close(code: u16, reason: &str) -> Result<u16> {
    if reason.len() > MAX_SOCKET_CLOSE_REASON_BYTES {
        return Err(PluginError::invalid_params(format!(
            "close reason of {} bytes exceeds {MAX_SOCKET_CLOSE_REASON_BYTES}",
            reason.len()
        )));
    }
    let code = if code == 0 { SOCKET_CLOSE_NORMAL } else { code };
    let allowed = matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999);
    if !allowed {
        return Err(PluginError::invalid_params(format!(
            "close code {code} may not be sent"
        )));
    }
    Ok(code)
}

/// Parses a socket URL into its target.
///
/// Only `ws://` and `wss://` URLs with a host, no userinfo and no fragment
/// are accepted, at most `maxSocketUrlBytes` long, and `tls://host:port`
/// with an explicit port and nothing after it. A name host may use only
/// letters, digits, `.`, `-` and `_`, with no punycode (`xn--`) label; a
/// bracketed host must be an IPv6 address. Percent-encoded hosts fail closed. Whether the host is reachable is the network grant's call, not
/// this function's.
///
/// # Errors
///
/// Returns `invalid_params` naming what is wrong with the URL.
pub fn parse_socket_url(url: &str) -> Result<SocketTarget> {
    if url.len() > MAX_SOCKET_URL_BYTES as usize {
        return Err(PluginError::invalid_params(format!(
            "socket url of {} bytes exceeds {MAX_SOCKET_URL_BYTES}",
            url.len()
        )));
    }
    if url.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(PluginError::invalid_params(
            "socket url has whitespace or control bytes",
        ));
    }
    let (scheme, rest) = if let Some(rest) = strip_prefix_ci(url, "wss://") {
        (SocketScheme::Wss, rest)
    } else if let Some(rest) = strip_prefix_ci(url, "ws://") {
        (SocketScheme::Ws, rest)
    } else if let Some(rest) = strip_prefix_ci(url, "tls://") {
        (SocketScheme::Tls, rest)
    } else {
        return Err(PluginError::invalid_params(
            "socket url must start with ws://, wss:// or tls://",
        ));
    };
    if rest.contains('#') {
        return Err(PluginError::invalid_params(
            "socket url may not carry a fragment",
        ));
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return Err(PluginError::invalid_params(
            "socket url may not carry credentials",
        ));
    }
    if authority.contains('%') {
        return Err(PluginError::invalid_params(
            "socket url host may not be percent-encoded",
        ));
    }
    if scheme.is_stream() && authority_end != rest.len() {
        return Err(PluginError::invalid_params(
            "a tls:// socket url is only a host and port",
        ));
    }
    let default_port = match scheme {
        SocketScheme::Ws => Some(80),
        SocketScheme::Wss => Some(443),
        SocketScheme::Tls => None,
    };
    let (host, port) = split_host_port(authority, default_port)?;
    if host.is_empty() {
        return Err(PluginError::invalid_params("socket url has no host"));
    }
    if !host.contains(':')
        && !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(PluginError::invalid_params(
            "socket url host may only use letters, digits, '.', '-' and '_'",
        ));
    }
    // URL parsers decode `xn--` labels as punycode and refuse invalid ones,
    // so accepting them here could disagree with the connection about the
    // host. Internationalized hosts are refused until the broker carries an
    // IDNA decoder.
    if host.split('.').any(|label| {
        label
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("xn--"))
    }) {
        return Err(PluginError::invalid_params(
            "socket url host may not use punycode (xn--) labels",
        ));
    }
    if ends_in_number(host) && host.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(PluginError::invalid_params(
            "socket url IPv4 host must be four decimal octets",
        ));
    }
    Ok(SocketTarget {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Splits `host[:port]` or `[v6][:port]`.
///
/// # Errors
///
/// `invalid_params` for a malformed or missing port (when there is no
/// `default_port`), an unclosed or non-IPv6 bracketed host, or an
/// unbracketed IPv6 host.
fn split_host_port(authority: &str, default_port: Option<u16>) -> Result<(&str, u16)> {
    let bad_port = || PluginError::invalid_params("socket url has a malformed port");
    if let Some(rest) = authority.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or_else(|| PluginError::invalid_params("socket url has an unclosed IPv6 host"))?;
        let host = &rest[..close];
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(PluginError::invalid_params(
                "socket url bracketed host is not an IPv6 address",
            ));
        }
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(port) => parse_port(port).ok_or_else(bad_port)?,
            None if after.is_empty() => default_port.ok_or_else(bad_port)?,
            None => return Err(bad_port()),
        };
        return Ok((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') {
                return Err(PluginError::invalid_params(
                    "socket url IPv6 host must be bracketed",
                ));
            }
            Ok((host, parse_port(port).ok_or_else(bad_port)?))
        }
        None => Ok((authority, default_port.ok_or_else(bad_port)?)),
    }
}

/// True when URL parsers would read `host` as an IPv4 address: its last
/// label (ignoring one trailing dot) is all digits or `0x` hex. Such a host
/// must then be a canonical dotted quad, so `127.1` or `0x7f.1` cannot pass
/// the network grant as a name and connect to an address.
fn ends_in_number(host: &str) -> bool {
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    let last = trimmed.rsplit('.').next().unwrap_or(trimmed);
    if last.is_empty() {
        return false;
    }
    if last.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    match last.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("0x") => {
            last[2..].bytes().all(|b| b.is_ascii_hexdigit())
        }
        _ => false,
    }
}

/// Parses a decimal port in 1..=65535 with no sign or leading zero.
fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || text.len() > 5 || text.starts_with('0') {
        return None;
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|port| *port != 0)
}

/// `str::strip_prefix` ignoring ASCII case.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// True when `name` is an HTTP token (RFC 9110 `tchar`).
pub(crate) fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Validates a whole `Sockets.connect` request.
///
/// # Returns
///
/// The parsed target of `request.url`.
///
/// # Errors
///
/// Returns `invalid_params` or `payload_too_large` naming the first problem:
/// the URL, too many or malformed subprotocols or headers, a header the broker
/// owns, a header value with control bytes, too many or oversized heartbeat
/// frames, or an oversized attachment.
pub fn validate_socket_connect(request: &SocketConnectRequest) -> Result<SocketTarget> {
    let target = parse_socket_url(&request.url)?;
    if target.scheme.is_stream() {
        return validate_stream_connect(request, target);
    }
    if request.protocols.len() > MAX_SOCKET_HEADERS as usize {
        return Err(PluginError::invalid_params(format!(
            "{} subprotocols exceed {MAX_SOCKET_HEADERS}",
            request.protocols.len()
        )));
    }
    if let Some(bad) = request.protocols.iter().find(|p| !is_token(p)) {
        return Err(PluginError::invalid_params(format!(
            "subprotocol {bad:?} is not a token"
        )));
    }
    if request.headers.len() > MAX_SOCKET_HEADERS as usize {
        return Err(PluginError::invalid_params(format!(
            "{} headers exceed {MAX_SOCKET_HEADERS}",
            request.headers.len()
        )));
    }
    for header in &request.headers {
        if !is_token(&header.name) {
            return Err(PluginError::invalid_params(format!(
                "header name {:?} is not a token",
                header.name
            )));
        }
        let lower = header.name.to_ascii_lowercase();
        if FORBIDDEN_HEADERS.contains(&lower.as_str()) || lower.starts_with("sec-websocket-") {
            return Err(PluginError::invalid_params(format!(
                "header {} is set by the broker",
                header.name
            )));
        }
        // The value may be a credential: never echo it.
        if header
            .value
            .bytes()
            .any(|b| (b < b' ' && b != b'\t') || b == 0x7f)
        {
            return Err(PluginError::invalid_params(format!(
                "header {} has control bytes in its value",
                header.name
            )));
        }
        if header.value.len() > MAX_SOCKET_URL_BYTES as usize {
            return Err(PluginError::invalid_params(format!(
                "header {} value exceeds {MAX_SOCKET_URL_BYTES} bytes",
                header.name
            )));
        }
    }
    check_auto_responses(&request.auto_responses)?;
    if !request.trust_anchors.is_empty() {
        return Err(PluginError::invalid_params(
            "trust anchors are for tls:// sockets only",
        ));
    }
    check_socket_attachment(&request.attachment)?;
    Ok(target)
}

/// The rest of a `tls://` connect request: ALPN ids instead of
/// subprotocols, and no headers or heartbeats, which a byte stream has no
/// place for.
///
/// # Errors
///
/// `invalid_params` naming the first problem, or the attachment check's
/// `payload_too_large`.
fn validate_stream_connect(
    request: &SocketConnectRequest,
    target: SocketTarget,
) -> Result<SocketTarget> {
    if request.protocols.len() > MAX_SOCKET_HEADERS as usize {
        return Err(PluginError::invalid_params(format!(
            "{} ALPN protocols exceed {MAX_SOCKET_HEADERS}",
            request.protocols.len()
        )));
    }
    if request.protocols.iter().any(|p| !is_alpn_id(p)) {
        return Err(PluginError::invalid_params(format!(
            "an ALPN protocol id must be 1 to {MAX_SOCKET_ALPN_BYTES} bytes of visible ASCII"
        )));
    }
    if !request.headers.is_empty() {
        return Err(PluginError::invalid_params(
            "a tls:// socket takes no request headers",
        ));
    }
    if !request.auto_responses.is_empty() {
        return Err(PluginError::invalid_params(
            "a tls:// socket takes no heartbeats",
        ));
    }
    check_trust_anchors(&request.trust_anchors)?;
    check_socket_attachment(&request.attachment)?;
    Ok(target)
}

/// Checks a `tls://` request's trust anchors: at most
/// [`MAX_SOCKET_TRUST_ANCHORS`], each a DER certificate of at most
/// [`MAX_SOCKET_TRUST_ANCHOR_BYTES`] whose outer SEQUENCE spans exactly its
/// bytes. The broker's certificate parser decides the rest.
///
/// # Errors
///
/// `invalid_params` naming the first problem.
pub fn check_trust_anchors(anchors: &[Vec<u8>]) -> Result<()> {
    if anchors.len() > MAX_SOCKET_TRUST_ANCHORS as usize {
        return Err(PluginError::invalid_params(format!(
            "{} trust anchors exceed {MAX_SOCKET_TRUST_ANCHORS}",
            anchors.len()
        )));
    }
    if anchors.iter().any(|der| !is_der_certificate_shape(der)) {
        return Err(PluginError::invalid_params(format!(
            "a trust anchor must be one DER certificate of at most \
             {MAX_SOCKET_TRUST_ANCHOR_BYTES} bytes"
        )));
    }
    Ok(())
}

/// True when `der` is at most [`MAX_SOCKET_TRUST_ANCHOR_BYTES`] and is one
/// DER SEQUENCE (tag `0x30`, definite minimal length) covering all of it.
#[must_use]
pub fn is_der_certificate_shape(der: &[u8]) -> bool {
    if der.len() > MAX_SOCKET_TRUST_ANCHOR_BYTES as usize {
        return false;
    }
    let Some((&0x30, rest)) = der.split_first() else {
        return false;
    };
    let Some((&first, rest)) = rest.split_first() else {
        return false;
    };
    let (len, rest) = match first {
        0..=0x7f => (usize::from(first), rest),
        0x81 => match rest.split_first() {
            Some((&len, rest)) if len >= 0x80 => (usize::from(len), rest),
            _ => return false,
        },
        0x82 => match rest {
            [hi, lo, rest @ ..] if *hi != 0 => (usize::from(*hi) << 8 | usize::from(*lo), rest),
            _ => return false,
        },
        _ => return false,
    };
    len == rest.len()
}

/// True when `id` is an ALPN protocol id the broker offers: 1 to
/// [`MAX_SOCKET_ALPN_BYTES`] bytes of visible ASCII.
#[must_use]
pub fn is_alpn_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_SOCKET_ALPN_BYTES && id.bytes().all(|b| b.is_ascii_graphic())
}

/// Checks a frame the guest sends on a socket of `scheme`: a `tls://`
/// stream carries bytes only.
///
/// # Errors
///
/// `invalid_params` for a text frame on a stream; the frame size check's
/// `payload_too_large`.
pub fn check_socket_frame_for(scheme: SocketScheme, frame: &SocketFrame) -> Result<()> {
    if scheme.is_stream() && matches!(frame, SocketFrame::Text(_)) {
        return Err(PluginError::invalid_params(
            "a tls:// socket carries binary frames only",
        ));
    }
    check_socket_frame(frame)
}

/// Checks declared heartbeat pairs: at most `maxSocketAutoResponses`, each
/// frame non-empty and at most `maxSocketAutoResponseBytes`.
///
/// # Errors
///
/// Returns `invalid_params` naming the first problem.
pub fn check_auto_responses(pairs: &[SocketAutoResponse]) -> Result<()> {
    if pairs.len() > MAX_SOCKET_AUTO_RESPONSES as usize {
        return Err(PluginError::invalid_params(format!(
            "{} heartbeat pairs exceed {MAX_SOCKET_AUTO_RESPONSES}",
            pairs.len()
        )));
    }
    for pair in pairs {
        for frame in [&pair.request, &pair.response] {
            let len = socket_frame_len(frame);
            if len == 0 || len > MAX_SOCKET_AUTO_RESPONSE_BYTES as usize {
                return Err(PluginError::invalid_params(format!(
                    "heartbeat frames must be 1 to {MAX_SOCKET_AUTO_RESPONSE_BYTES} bytes"
                )));
            }
        }
    }
    Ok(())
}

/// The reply the broker sends for an incoming frame, when it matches a
/// declared heartbeat exactly (same kind, same bytes).
///
/// # Returns
///
/// The declared response of the first matching pair, or `None` when the
/// frame is for the guest.
#[must_use]
pub fn auto_response_for<'a>(
    frame: &SocketFrame,
    pairs: &'a [SocketAutoResponse],
) -> Option<&'a SocketFrame> {
    pairs
        .iter()
        .find(|pair| &pair.request == frame)
        .map(|pair| &pair.response)
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
#[path = "sockets_tests.rs"]
mod tests;
