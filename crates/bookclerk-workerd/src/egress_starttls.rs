//! The framed tunnel that lets a workerd plugin upgrade a `connect()` socket
//! to TLS: `startTls()`, and `secureTransport: "on"`, for SMTP, Postgres and
//! other protocols that do not speak HTTP.
//!
//! The pinned workerd cannot upgrade a socket it relays through the egress
//! worker ("not supported on this connection"), so the launcher's egress
//! relay does it, the same way it originates TLS for `https` fetches. The
//! SDK's `connect()` asks for a framed tunnel by dialing the host with
//! [`STARTTLS_HOST_SUFFIX`] appended. The egress gate strips the suffix, checks
//! the real host and port against the plugin's `tcp` grant, and dials on with
//! the suffix kept; the relay strips it again and asks the host for a tunnel
//! to the real host, so the host's own check sees the real host too.
//!
//! On a framed tunnel every byte between the plugin and the relay is a frame:
//! a one-byte kind, a two-byte big-endian payload length, then the payload.
//!
//! | Direction | Kind | Payload |
//! | --- | --- | --- |
//! | plugin to relay | `0` data | 1 to [`MAX_FRAME_PAYLOAD`] bytes for the server |
//! | plugin to relay | `1` start TLS | empty |
//! | relay to plugin | `0` data | 1 to [`MAX_FRAME_PAYLOAD`] bytes from the server |
//! | relay to plugin | `1` TLS ready | empty |
//! | relay to plugin | `2` TLS failed | UTF-8 reason, at most [`MAX_FAILURE_REASON_BYTES`] |
//!
//! Data frames carry plaintext: before the upgrade the bytes on the wire, and
//! after it the bytes inside the TLS session, which the relay originates with
//! the server name the plugin dialed and Mozilla's roots. A tunnel upgrades at
//! most once. Anything else (an unknown kind, a bad length, a second start)
//! ends the tunnel.
//!
//! The decoder is sans-IO ([`FrameDecoder`]) so the `egress_starttls_frames`
//! fuzz target and the unit tests drive the same code the relay runs.

use std::fmt;

/// Host-name suffix that asks the egress gate and relay for a framed tunnel.
///
/// `.invalid` is reserved (RFC 6761), so no real host ends this way.
pub const STARTTLS_HOST_SUFFIX: &str = ".starttls.bookclerk.invalid";

/// Largest payload one frame carries, in bytes.
pub const MAX_FRAME_PAYLOAD: usize = 16 * 1024;

/// Largest reason a TLS-failed frame carries, in bytes.
pub const MAX_FAILURE_REASON_BYTES: usize = 512;

/// Bytes in a frame header: kind, then the payload length.
pub const FRAME_HEADER_BYTES: usize = 3;

/// Frame kind byte for data, in either direction.
const KIND_DATA: u8 = 0;
/// Frame kind byte for start TLS (plugin to relay) and TLS ready (relay to
/// plugin).
const KIND_TLS: u8 = 1;
/// Frame kind byte for TLS failed (relay to plugin).
const KIND_TLS_FAILED: u8 = 2;

/// One frame the plugin sent the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    /// Bytes for the server.
    Data(Vec<u8>),
    /// Upgrade the tunnel to TLS now.
    StartTls,
}

/// Why the plugin's frames were refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The kind byte is not data or start TLS.
    UnknownKind(u8),
    /// A data frame is empty or longer than [`MAX_FRAME_PAYLOAD`].
    BadDataLength(usize),
    /// A start-TLS frame carries a payload.
    StartTlsPayload(usize),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKind(kind) => write!(f, "unknown frame kind {kind}"),
            Self::BadDataLength(len) => write!(
                f,
                "data frame of {len} bytes (1 to {MAX_FRAME_PAYLOAD} allowed)"
            ),
            Self::StartTlsPayload(len) => {
                write!(f, "start-TLS frame carries {len} payload bytes")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Splits the plugin's byte stream into [`ClientFrame`]s.
///
/// Holds at most one partial frame: [`FrameDecoder::next_frame`] consumes
/// each complete frame as soon as it is pushed, so the buffer never grows
/// past one frame plus the bytes of one read.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    /// Bytes received and not yet consumed as a frame.
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// A decoder with nothing buffered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends bytes read from the plugin.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The next bytes, in order.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes buffered that do not yet make a whole frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Takes the next complete frame, if one is buffered.
    ///
    /// # Returns
    ///
    /// `Ok(Some(frame))` for a complete frame, `Ok(None)` when more bytes are
    /// needed.
    ///
    /// # Errors
    ///
    /// [`FrameError`] as soon as the header shows the frame is malformed,
    /// before its payload arrives; the tunnel must then end.
    pub fn next_frame(&mut self) -> Result<Option<ClientFrame>, FrameError> {
        let Some(header) = self.buf.get(..FRAME_HEADER_BYTES) else {
            return Ok(None);
        };
        let kind = header[0];
        let len = usize::from(u16::from_be_bytes([header[1], header[2]]));
        match kind {
            KIND_DATA if len == 0 || len > MAX_FRAME_PAYLOAD => {
                return Err(FrameError::BadDataLength(len));
            }
            KIND_DATA => {}
            KIND_TLS if len != 0 => return Err(FrameError::StartTlsPayload(len)),
            KIND_TLS => {}
            other => return Err(FrameError::UnknownKind(other)),
        }
        let end = FRAME_HEADER_BYTES + len;
        if self.buf.len() < end {
            return Ok(None);
        }
        let mut frame: Vec<u8> = self.buf.drain(..end).collect();
        Ok(Some(if kind == KIND_DATA {
            frame.drain(..FRAME_HEADER_BYTES);
            ClientFrame::Data(frame)
        } else {
            ClientFrame::StartTls
        }))
    }
}

/// Appends one frame header to `out`.
fn push_header(out: &mut Vec<u8>, kind: u8, len: usize) {
    // Callers bound `len` by MAX_FRAME_PAYLOAD (16 KiB), which fits a u16.
    let len = u16::try_from(len).unwrap_or(u16::MAX);
    out.push(kind);
    out.extend_from_slice(&len.to_be_bytes());
}

/// Encodes server bytes as data frames for the plugin.
///
/// # Arguments
///
/// * `bytes` - Bytes from the server, in order; empty encodes nothing.
///
/// # Returns
///
/// One data frame per [`MAX_FRAME_PAYLOAD`] bytes.
#[must_use]
pub fn encode_data(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        bytes.len() + FRAME_HEADER_BYTES * bytes.len().div_ceil(MAX_FRAME_PAYLOAD),
    );
    for chunk in bytes.chunks(MAX_FRAME_PAYLOAD) {
        push_header(&mut out, KIND_DATA, chunk.len());
        out.extend_from_slice(chunk);
    }
    out
}

/// The TLS-ready frame: the upgrade succeeded and later data frames are
/// inside the TLS session.
#[must_use]
pub fn encode_tls_ready() -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES);
    push_header(&mut out, KIND_TLS, 0);
    out
}

/// The TLS-failed frame, with `reason` cut to [`MAX_FAILURE_REASON_BYTES`] on
/// a character boundary.
///
/// # Arguments
///
/// * `reason` - Why the handshake failed, for the plugin's error; never
///   carries plugin data.
#[must_use]
pub fn encode_tls_failed(reason: &str) -> Vec<u8> {
    let mut end = reason.len().min(MAX_FAILURE_REASON_BYTES);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let reason = &reason.as_bytes()[..end];
    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + reason.len());
    push_header(&mut out, KIND_TLS_FAILED, reason.len());
    out.extend_from_slice(reason);
    out
}

/// Encodes one plugin frame, the inverse of [`FrameDecoder`]: for tests, the
/// fuzz oracle and the SDK vectors.
///
/// # Arguments
///
/// * `frame` - The frame; a data payload must be 1 to [`MAX_FRAME_PAYLOAD`]
///   bytes.
#[must_use]
pub fn encode_client_frame(frame: &ClientFrame) -> Vec<u8> {
    match frame {
        ClientFrame::Data(bytes) => {
            let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + bytes.len());
            push_header(&mut out, KIND_DATA, bytes.len());
            out.extend_from_slice(bytes);
            out
        }
        ClientFrame::StartTls => {
            let mut out = Vec::with_capacity(FRAME_HEADER_BYTES);
            push_header(&mut out, KIND_TLS, 0);
            out
        }
    }
}

/// Splits a framed-tunnel host into the real host, or returns `None` when
/// `host` does not ask for a framed tunnel.
///
/// # Arguments
///
/// * `host` - Lowercased host from the `CONNECT` line.
///
/// # Returns
///
/// `Some(real)` when `host` ends in [`STARTTLS_HOST_SUFFIX`] after a
/// non-empty name; `real` may still be malformed, which the host's check
/// refuses.
#[must_use]
pub fn strip_starttls_suffix(host: &str) -> Option<&str> {
    host.strip_suffix(STARTTLS_HOST_SUFFIX)
        .filter(|real| !real.is_empty() && !real.ends_with('.'))
}

#[cfg(test)]
#[path = "egress_starttls_tests.rs"]
mod tests;
