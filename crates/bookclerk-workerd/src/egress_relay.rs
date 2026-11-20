//! The launcher's egress relay: a workerd isolate's only way out of its jail.
//!
//! Every workerd jail is `NetPolicy::Deny`: no IPv4, IPv6 or packet sockets.
//! The generated config points the egress worker's `globalOutbound` at a
//! Unix socket (Windows: in-container loopback) this launcher owns, as a
//! workerd `external` service in HTTP proxy style. Workerd then sends:
//!
//! - each `fetch()` as one absolute-form request (`GET https://host/path`),
//!   in plain text, because TLS to the origin is this relay's job; and
//! - each `connect()` socket as `CONNECT host:port`.
//!
//! For either, the relay opens one stream on the host's `CONNECT` proxy link
//! (the same multiplexed link a native guest uses) and asks the host for a
//! tunnel to `host:port`. The host checks the tunnel against the plugin's
//! approved grant ([`crate::socket_proxy::TunnelRule::IsolateEgress`]),
//! resolves the name, checks every address, and dials. The relay then
//! originates TLS for `https` (rustls on the hosts' one provider, Mozilla's
//! roots), forwards the request in origin form, and splices bytes.
//!
//! Nothing here widens what the plugin can reach: the host's check is the
//! boundary, and it runs outside the jail. The relay keeps TLS and HTTP inside
//! the jail so the unsandboxed host never parses an origin's response or
//! sees a plugin's plaintext.
//!
//! One request per connection. The relay rewrites the request to
//! `connection: close` (or keeps `Upgrade` for WebSockets) and rewrites the
//! final response head to `connection: close`, so workerd never sends a
//! second request down a tunnel opened for another origin. Interim `1xx`
//! responses are forwarded and the next head is read, for the same reason.
//!
//! Transport failures (a refused tunnel, a failed dial or TLS handshake)
//! close the workerd connection without a response, so the plugin's
//! `fetch()` rejects as it would on a lost connection, rather than resolving
//! to a synthetic status. The relay never logs URLs or headers, which carry
//! plugin tokens.
//!
//! The parsing is sans-IO ([`parse_request_head`], [`parse_response_head`],
//! [`find_head_end`]) so the `egress_relay_head` fuzz target and the unit
//! tests drive the same code the relay runs.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Semaphore;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::TlsConnector;

/// Largest request or response head the relay buffers, in bytes.
///
/// Workers URLs may be up to 16 KiB; headers add more. Anything larger is
/// refused rather than buffered.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Most header lines the relay accepts in one head.
pub const MAX_HEADER_LINES: usize = 128;

/// Tunnels the relay keeps open at once.
///
/// Matches the host proxy's per-link handler cap, so a burst of `fetch()`
/// calls waits here for a free tunnel instead of being dropped by the host.
pub const MAX_CONCURRENT_TUNNELS: usize = 16;

/// Time allowed for workerd to send a complete request head.
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Time allowed for the host to answer the tunnel request (DNS plus dial).
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(30);

/// Time allowed for the TLS handshake with the origin.
const TLS_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a head was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadError {
    /// The head exceeds [`MAX_HEAD_BYTES`] or [`MAX_HEADER_LINES`].
    TooLarge,
    /// The request or status line is not well formed.
    BadStartLine,
    /// A header line is not `name: value`, has an invalid name, or carries a
    /// stray CR or LF, or is an obsolete folded continuation.
    BadHeader,
    /// The request target is not absolute-form `http(s)://` or a `CONNECT`
    /// authority, or it carries userinfo or a fragment.
    BadTarget,
    /// The head is not valid UTF-8 where text is required.
    NotText,
}

impl std::fmt::Display for HeadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::TooLarge => "head exceeds the relay's size cap",
            Self::BadStartLine => "malformed start line",
            Self::BadHeader => "malformed header line",
            Self::BadTarget => "request target is not absolute-form http(s) or CONNECT",
            Self::NotText => "head is not UTF-8",
        };
        f.write_str(text)
    }
}

impl std::error::Error for HeadError {}

/// URL scheme of a forwarded request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// Plain HTTP; the relay forwards bytes as they are.
    Http,
    /// HTTPS; the relay originates TLS to the origin.
    Https,
}

impl Scheme {
    /// Port a URL without an explicit port uses.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

/// A tunnel target: host and port, as the host proxy's `CONNECT` line names
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelTarget {
    /// Lowercased host name or IP literal, without IPv6 brackets.
    pub host: String,
    /// Destination TCP port.
    pub port: u16,
}

impl TunnelTarget {
    /// `host:port` with IPv6 literals in brackets, for a `CONNECT` line.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// What workerd asked the relay to do with one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayRequest {
    /// A `connect()` socket: open a tunnel and splice raw bytes.
    Tunnel(TunnelTarget),
    /// A `fetch()` request: open a tunnel, add TLS for `https`, then send
    /// [`Self::Forward::head`] followed by the request body as workerd sends
    /// it.
    Forward {
        /// Scheme of the request URL.
        scheme: Scheme,
        /// Origin the request goes to.
        target: TunnelTarget,
        /// The rewritten head in origin form, ending in `\r\n\r\n`.
        head: Vec<u8>,
    },
}

/// A parsed response head from an origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    /// HTTP status code.
    pub status: u16,
    /// The head to hand to workerd, ending in `\r\n\r\n`. Interim (`1xx`)
    /// and `101` heads are unchanged; a final head is rewritten to
    /// `connection: close`.
    pub head: Vec<u8>,
}

impl ResponseHead {
    /// Whether another head follows this one (an interim `1xx` other than
    /// `101 Switching Protocols`).
    #[must_use]
    pub fn is_interim(&self) -> bool {
        (100..200).contains(&self.status) && self.status != 101
    }
}

/// Byte offset just past the `\r\n\r\n` that ends a head, if `buf` holds one.
///
/// # Arguments
///
/// * `buf` - Bytes read so far.
///
/// # Returns
///
/// `Some(n)` where `buf[..n]` is the head, or `None` when more bytes are
/// needed.
#[must_use]
pub fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// A head's start line and its `(name, value)` header lines.
type SplitHead<'a> = (&'a str, Vec<(&'a str, &'a str)>);

/// Splits a head into its start line and header lines.
///
/// # Errors
///
/// [`HeadError`] when the head is too large, not UTF-8, missing its
/// terminator, or has a header line with a stray CR or LF or a folded
/// continuation.
fn split_head(head: &[u8]) -> std::result::Result<SplitHead<'_>, HeadError> {
    if head.len() > MAX_HEAD_BYTES {
        return Err(HeadError::TooLarge);
    }
    let text = std::str::from_utf8(head).map_err(|_| HeadError::NotText)?;
    let body = text
        .strip_suffix("\r\n\r\n")
        .ok_or(HeadError::BadStartLine)?;
    let mut lines = body.split("\r\n");
    let start = lines.next().ok_or(HeadError::BadStartLine)?;
    if start.contains(['\r', '\n']) {
        return Err(HeadError::BadStartLine);
    }
    let mut headers = Vec::new();
    for line in lines {
        if headers.len() >= MAX_HEADER_LINES {
            return Err(HeadError::TooLarge);
        }
        if line.contains(['\r', '\n']) || line.starts_with([' ', '\t']) {
            return Err(HeadError::BadHeader);
        }
        let (name, value) = line.split_once(':').ok_or(HeadError::BadHeader)?;
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(HeadError::BadHeader);
        }
        headers.push((name, value.trim_matches([' ', '\t'])));
    }
    Ok((start, headers))
}

/// Whether `byte` may appear in an HTTP token (RFC 9110 `tchar`).
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// Parses `host[:port]` from a URL authority or `CONNECT` target.
///
/// # Arguments
///
/// * `authority` - Authority text, IPv6 literals in brackets.
/// * `default_port` - Port when none is written; `None` requires one.
///
/// # Errors
///
/// [`HeadError::BadTarget`] for userinfo, an empty host, a bad port, or an
/// unterminated IPv6 literal.
fn parse_target(
    authority: &str,
    default_port: Option<u16>,
) -> std::result::Result<TunnelTarget, HeadError> {
    if authority.is_empty() || authority.contains(['@', '/', '?', '#', ' ', '\\']) {
        return Err(HeadError::BadTarget);
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or(HeadError::BadTarget)?;
        let port = match after.strip_prefix(':') {
            Some(port) => Some(port),
            None if after.is_empty() => None,
            None => return Err(HeadError::BadTarget),
        };
        if !host.contains(':') {
            return Err(HeadError::BadTarget);
        }
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => {
                if host.contains(':') {
                    return Err(HeadError::BadTarget);
                }
                (host, Some(port))
            }
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err(HeadError::BadTarget);
    }
    let port = match port {
        Some(text) => text
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or(HeadError::BadTarget)?,
        None => default_port.ok_or(HeadError::BadTarget)?,
    };
    Ok(TunnelTarget {
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Request headers the relay never forwards: hop-by-hop and proxy headers it
/// replaces, and `expect`, so no `100 Continue` exchange is needed.
const DROPPED_REQUEST_HEADERS: [&str; 6] = [
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "expect",
];

/// Parses the head workerd sent and decides what to do with the connection.
///
/// # Arguments
///
/// * `head` - The complete request head, through `\r\n\r\n`
///   ([`find_head_end`]).
///
/// # Returns
///
/// [`RelayRequest::Tunnel`] for `CONNECT host:port`, or
/// [`RelayRequest::Forward`] with the head rewritten to origin form: `host`
/// set from the URL, proxy and hop-by-hop headers dropped, and `connection:
/// close` (or `connection: upgrade` when the request carries `upgrade`).
///
/// # Errors
///
/// [`HeadError`] when the head is malformed or the target is not an absolute
/// `http`/`https` URL or a `CONNECT` authority.
pub fn parse_request_head(head: &[u8]) -> std::result::Result<RelayRequest, HeadError> {
    let (start, headers) = split_head(head)?;
    let mut parts = start.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HeadError::BadStartLine);
    };
    if method.is_empty() || !method.bytes().all(is_token_byte) || version != "HTTP/1.1" {
        return Err(HeadError::BadStartLine);
    }
    if method == "CONNECT" {
        return Ok(RelayRequest::Tunnel(parse_target(target, None)?));
    }
    let (scheme, rest) = if let Some(rest) = strip_prefix_ignore_case(target, "https://") {
        (Scheme::Https, rest)
    } else if let Some(rest) = strip_prefix_ignore_case(target, "http://") {
        (Scheme::Http, rest)
    } else {
        return Err(HeadError::BadTarget);
    };
    if rest.contains('#') {
        return Err(HeadError::BadTarget);
    }
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    let target = parse_target(authority, Some(scheme.default_port()))?;
    let path = match path.strip_prefix('?') {
        Some(query) => format!("/?{query}"),
        None if path.is_empty() => "/".to_string(),
        None => path.to_string(),
    };
    let upgrade = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("upgrade"));
    let host_header = if target.port == scheme.default_port() {
        bracketed_host(&target.host)
    } else {
        target.authority()
    };
    let mut out = format!("{method} {path} HTTP/1.1\r\nhost: {host_header}\r\n");
    for (name, value) in headers {
        if DROPPED_REQUEST_HEADERS
            .iter()
            .any(|dropped| name.eq_ignore_ascii_case(dropped))
        {
            continue;
        }
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str(if upgrade {
        "connection: upgrade\r\n\r\n"
    } else {
        "connection: close\r\n\r\n"
    });
    Ok(RelayRequest::Forward {
        scheme,
        target,
        head: out.into_bytes(),
    })
}

/// `host` with brackets added when it is an IPv6 literal.
fn bracketed_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// `text` without `prefix`, compared ASCII case-insensitively.
fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Response headers dropped from a final head before `connection: close` is
/// added.
const DROPPED_RESPONSE_HEADERS: [&str; 3] = ["connection", "keep-alive", "proxy-connection"];

/// Parses a response head from an origin and rewrites a final one.
///
/// # Arguments
///
/// * `head` - The complete response head, through `\r\n\r\n`.
///
/// # Returns
///
/// The status and the head to hand to workerd: interim and `101` heads
/// unchanged, a final head with its connection headers replaced by
/// `connection: close`.
///
/// # Errors
///
/// [`HeadError`] when the status line or a header is malformed.
pub fn parse_response_head(head: &[u8]) -> std::result::Result<ResponseHead, HeadError> {
    let (start, headers) = split_head(head)?;
    let mut parts = start.splitn(3, ' ');
    let (Some(version), Some(code)) = (parts.next(), parts.next()) else {
        return Err(HeadError::BadStartLine);
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HeadError::BadStartLine);
    }
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HeadError::BadStartLine);
    }
    let status: u16 = code.parse().map_err(|_| HeadError::BadStartLine)?;
    if !(100..600).contains(&status) {
        return Err(HeadError::BadStartLine);
    }
    if (100..200).contains(&status) {
        return Ok(ResponseHead {
            status,
            head: head.to_vec(),
        });
    }
    let mut out = String::with_capacity(head.len() + 20);
    out.push_str(start);
    out.push_str("\r\n");
    for (name, value) in headers {
        if DROPPED_RESPONSE_HEADERS
            .iter()
            .any(|dropped| name.eq_ignore_ascii_case(dropped))
        {
            continue;
        }
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("connection: close\r\n\r\n");
    Ok(ResponseHead {
        status,
        head: out.into_bytes(),
    })
}

/// Reads one head (through `\r\n\r\n`) and returns it with any bytes read
/// past it.
///
/// # Errors
///
/// When the peer closes first, the read fails, or the head passes
/// [`MAX_HEAD_BYTES`].
async fn read_head<R>(reader: &mut R, carry: Vec<u8>) -> Result<(Vec<u8>, Vec<u8>)>
where
    R: AsyncRead + Unpin,
{
    let mut buf = carry;
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(end) = find_head_end(&buf) {
            let rest = buf.split_off(end);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD_BYTES {
            bail!(HeadError::TooLarge);
        }
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            bail!("peer closed before a complete head");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Opens one stream on the host's proxy link.
///
/// A trait so tests can stand in for the multiplexed link.
#[async_trait::async_trait]
pub trait TunnelOpener: Send + Sync + 'static {
    /// Byte stream the host proxy answers `CONNECT` on.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Opens a fresh stream to the host proxy.
    ///
    /// # Errors
    ///
    /// When the link is closed.
    async fn open(&self) -> Result<Self::Stream>;
}

#[async_trait::async_trait]
impl TunnelOpener for bookclerk_plugin_sdk::mux::Mux {
    type Stream = bookclerk_plugin_sdk::mux::MuxStream;

    async fn open(&self) -> Result<Self::Stream> {
        bookclerk_plugin_sdk::mux::Mux::open(self)
            .await
            .map_err(|err| anyhow::anyhow!("open proxy stream: {err}"))
    }
}

/// Asks the host for a tunnel to `target` on a fresh stream.
///
/// # Arguments
///
/// * `opener` - Opens streams on the host's proxy link.
/// * `target` - Where the tunnel should go.
///
/// # Returns
///
/// The stream, now carrying the tunnel's bytes.
///
/// # Errors
///
/// When the stream cannot be opened, the host refuses (any status but 200),
/// or the host does not answer within [`TUNNEL_TIMEOUT`].
pub async fn open_tunnel<O: TunnelOpener>(opener: &O, target: &TunnelTarget) -> Result<O::Stream> {
    let mut stream = opener.open().await?;
    let authority = target.authority();
    let request = format!("CONNECT {authority} HTTP/1.1\r\nhost: {authority}\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let (head, rest) = tokio::time::timeout(TUNNEL_TIMEOUT, read_head(&mut stream, Vec::new()))
        .await
        .context("host proxy did not answer the tunnel request")??;
    let ok = head.starts_with(b"HTTP/1.1 200 ") || head.starts_with(b"HTTP/1.0 200 ");
    if !ok || !rest.is_empty() {
        bail!("host proxy refused the tunnel");
    }
    Ok(stream)
}

/// Opens the host's proxy link that the host delivered to this process.
///
/// The host passes the link in [`bookclerk_sandbox::WORKERD_EGRESS_ENV`]
/// (plus [`bookclerk_sandbox::WORKERD_EGRESS_WRITE_ENV`] on Windows) and the
/// session challenge in [`bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV`]. The
/// challenge is written first, then the link is multiplexed. The challenge is
/// removed from this process's environment once read, so a child (workerd)
/// never inherits it.
///
/// # Returns
///
/// `None` when no link was delivered: the plugin's network is denied.
///
/// # Errors
///
/// When the link spec or challenge is malformed, or writing the challenge
/// fails.
pub async fn host_link_from_env() -> Result<Option<Arc<bookclerk_plugin_sdk::mux::Mux>>> {
    use crate::inherited_link::InheritedDuplex;

    let Ok(spec) = std::env::var(bookclerk_sandbox::WORKERD_EGRESS_ENV) else {
        return Ok(None);
    };
    let challenge = std::env::var(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV).ok();
    std::env::remove_var(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV);
    #[cfg(not(windows))]
    let (reader, mut writer) = InheritedDuplex::open(&spec)
        .context("open the inherited host proxy link")?
        .into_split();
    #[cfg(windows)]
    let (reader, mut writer) = {
        let write_spec =
            std::env::var(bookclerk_sandbox::WORKERD_EGRESS_WRITE_ENV).with_context(|| {
                format!(
                    "{} is required on Windows when {} is set",
                    bookclerk_sandbox::WORKERD_EGRESS_WRITE_ENV,
                    bookclerk_sandbox::WORKERD_EGRESS_ENV
                )
            })?;
        InheritedDuplex::open_halves(&spec, &write_spec)
            .context("open the inherited host proxy pipes")?
    };
    // workerd inherits this environment; the link names mean nothing there
    // (the descriptors are close-on-exec) but are not its business either.
    std::env::remove_var(bookclerk_sandbox::WORKERD_EGRESS_ENV);
    std::env::remove_var(bookclerk_sandbox::WORKERD_EGRESS_WRITE_ENV);
    if let Some(hex_text) = challenge {
        let bytes = hex::decode(hex_text.trim()).context("session challenge is not hex")?;
        if bytes.len() != bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN {
            bail!("session challenge has the wrong length");
        }
        writer
            .write_all(&bytes)
            .await
            .context("write the session challenge")?;
    }
    Ok(Some(Arc::new(bookclerk_plugin_sdk::mux::Mux::client(
        reader, writer,
    ))))
}

/// Copies a raw tunnel both ways until the upstream side ends.
///
/// The host link carries no TCP half-close: a stream `Close` ends both
/// directions. So when workerd half-closes (`allowHalfOpen`: the plugin
/// closed its writer and still reads), the relay does not pass the EOF on;
/// it keeps the tunnel open and relays the reply until the upstream ends,
/// then closes workerd's side. A plugin that fully closes its socket ends
/// the relay through the failed write. The upstream therefore never sees a
/// half-close, which the rare protocol that waits for one cannot use.
///
/// # Errors
///
/// A read or write failure on either side.
async fn pump_tunnel<C, U>(client: &mut C, upstream: &mut U) -> Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let mut client_buf = vec![0_u8; 16 * 1024];
    let mut upstream_buf = vec![0_u8; 16 * 1024];
    let mut client_open = true;
    loop {
        tokio::select! {
            n = client.read(&mut client_buf), if client_open => {
                let n = n?;
                if n == 0 {
                    client_open = false;
                } else {
                    upstream.write_all(&client_buf[..n]).await?;
                    upstream.flush().await?;
                }
            }
            n = upstream.read(&mut upstream_buf) => {
                let n = n?;
                if n == 0 {
                    let _ = client.shutdown().await;
                    return Ok(());
                }
                client.write_all(&upstream_buf[..n]).await?;
                client.flush().await?;
            }
        }
    }
}

/// Serves one workerd connection end to end.
///
/// # Errors
///
/// Any failure; the caller drops the connection, so workerd sees it lost.
async fn relay_one<C, O>(mut client: C, opener: Arc<O>, tls: Arc<ClientConfig>) -> Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    O: TunnelOpener,
{
    let (head, body_start) =
        tokio::time::timeout(REQUEST_HEAD_TIMEOUT, read_head(&mut client, Vec::new()))
            .await
            .context("workerd did not send a request head")??;
    match parse_request_head(&head)? {
        RelayRequest::Tunnel(target) => {
            let mut upstream = open_tunnel(opener.as_ref(), &target).await?;
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            client.flush().await?;
            if !body_start.is_empty() {
                upstream.write_all(&body_start).await?;
            }
            let _ = pump_tunnel(&mut client, &mut upstream).await;
            Ok(())
        }
        RelayRequest::Forward {
            scheme,
            target,
            head,
        } => {
            let tunnel = open_tunnel(opener.as_ref(), &target).await?;
            match scheme {
                Scheme::Http => forward(client, tunnel, &head, &body_start).await,
                Scheme::Https => {
                    let name = ServerName::try_from(target.host.clone())
                        .context("origin host is not a valid TLS server name")?;
                    let tls_stream = tokio::time::timeout(
                        TLS_TIMEOUT,
                        TlsConnector::from(tls).connect(name, tunnel),
                    )
                    .await
                    .context("TLS handshake with the origin timed out")??;
                    forward(client, tls_stream, &head, &body_start).await
                }
            }
        }
    }
}

/// Sends the rewritten request, relays the response heads, then splices.
///
/// # Errors
///
/// When either side fails before the final response head is relayed.
async fn forward<C, U>(client: C, mut upstream: U, head: &[u8], body_start: &[u8]) -> Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    upstream.write_all(head).await?;
    if !body_start.is_empty() {
        upstream.write_all(body_start).await?;
    }
    upstream.flush().await?;
    // The request body (if any) keeps flowing while the response head is
    // read: an origin may answer before it has read the whole body.
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut up_read, mut up_write) = tokio::io::split(upstream);
    let to_origin = async {
        let _ = tokio::io::copy(&mut client_read, &mut up_write).await;
        let _ = up_write.shutdown().await;
    };
    let to_client = async {
        let mut carry = Vec::new();
        loop {
            let (raw, rest) = read_head(&mut up_read, carry).await?;
            let response = parse_response_head(&raw)?;
            client_write.write_all(&response.head).await?;
            carry = rest;
            if !response.is_interim() {
                break;
            }
        }
        if !carry.is_empty() {
            client_write.write_all(&carry).await?;
        }
        tokio::io::copy(&mut up_read, &mut client_write).await?;
        client_write.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    };
    tokio::pin!(to_origin, to_client);
    // The response decides when the connection is done: once it has been
    // relayed, stop copying the request side too.
    tokio::select! {
        result = &mut to_client => result,
        () = &mut to_origin => to_client.await,
    }
}

/// Accepts workerd connections and relays each through the host until the
/// listener fails.
///
/// # Arguments
///
/// * `accept` - Yields the next workerd connection.
/// * `opener` - Opens streams on the host's proxy link.
/// * `tls` - Client TLS settings for `https` origins.
///
/// # Errors
///
/// When accepting fails; individual connections fail on their own.
pub async fn serve<A, F, C, O>(mut accept: A, opener: Arc<O>, tls: Arc<ClientConfig>) -> Result<()>
where
    A: FnMut() -> F,
    F: std::future::Future<Output = std::io::Result<C>>,
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    O: TunnelOpener,
{
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_TUNNELS));
    loop {
        let client = accept().await.context("accept on the egress relay")?;
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .context("egress relay semaphore closed")?;
        let opener = Arc::clone(&opener);
        let tls = Arc::clone(&tls);
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) = relay_one(client, opener, tls).await {
                // Never the URL or headers: they carry plugin tokens.
                tracing::debug!(error = %err, "egress relay connection ended");
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    fn forward_head(head: &[u8]) -> (Scheme, TunnelTarget, String) {
        match parse_request_head(head).unwrap() {
            RelayRequest::Forward {
                scheme,
                target,
                head,
            } => (scheme, target, String::from_utf8(head).unwrap()),
            RelayRequest::Tunnel(t) => panic!("expected forward, got tunnel {t:?}"),
        }
    }

    #[test]
    fn absolute_https_becomes_origin_form_with_host_and_close() {
        let (scheme, target, head) = forward_head(
            b"GET https://API.Example.com/path?q=1 HTTP/1.1\r\nx-t: 1\r\nhost: evil.example\r\nConnection: keep-alive\r\nProxy-Authorization: x\r\n\r\n",
        );
        assert_eq!(scheme, Scheme::Https);
        assert_eq!(
            target,
            TunnelTarget {
                host: "api.example.com".into(),
                port: 443
            }
        );
        assert_eq!(
            head,
            "GET /path?q=1 HTTP/1.1\r\nhost: api.example.com\r\nx-t: 1\r\nconnection: close\r\n\r\n"
        );
    }

    #[test]
    fn explicit_ports_ipv6_and_bare_query_are_kept() {
        let (scheme, target, head) =
            forward_head(b"POST http://[2001:db8::1]:8080?a=b HTTP/1.1\r\n\r\n");
        assert_eq!(scheme, Scheme::Http);
        assert_eq!(target.authority(), "[2001:db8::1]:8080");
        assert!(head.starts_with("POST /?a=b HTTP/1.1\r\nhost: [2001:db8::1]:8080\r\n"));
        let (_, target, head) = forward_head(b"GET https://example.com HTTP/1.1\r\n\r\n");
        assert_eq!(target.port, 443);
        assert!(head.starts_with("GET / HTTP/1.1\r\nhost: example.com\r\n"));
    }

    #[test]
    fn websocket_upgrades_keep_the_connection_open() {
        let (_, _, head) = forward_head(
            b"GET https://ws.example.com/s HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        );
        assert!(head.contains("upgrade: websocket\r\n") || head.contains("Upgrade: websocket\r\n"));
        assert!(head.ends_with("connection: upgrade\r\n\r\n"));
        assert!(!head.contains("connection: close"));
    }

    #[test]
    fn connect_becomes_a_tunnel() {
        assert_eq!(
            parse_request_head(b"CONNECT db.example.com:5432 HTTP/1.1\r\n\r\n").unwrap(),
            RelayRequest::Tunnel(TunnelTarget {
                host: "db.example.com".into(),
                port: 5432
            })
        );
        assert_eq!(
            parse_request_head(b"CONNECT db.example.com HTTP/1.1\r\n\r\n"),
            Err(HeadError::BadTarget)
        );
    }

    #[test]
    fn malformed_and_smuggling_shaped_heads_are_refused() {
        for (head, want) in [
            (&b"GET /path HTTP/1.1\r\n\r\n"[..], HeadError::BadTarget),
            (b"GET ftp://x/ HTTP/1.1\r\n\r\n", HeadError::BadTarget),
            (
                b"GET https://user@x/ HTTP/1.1\r\n\r\n",
                HeadError::BadTarget,
            ),
            (b"GET https://x/#f HTTP/1.1\r\n\r\n", HeadError::BadTarget),
            (b"GET https://x:0/ HTTP/1.1\r\n\r\n", HeadError::BadTarget),
            (
                b"GET https://x:99999/ HTTP/1.1\r\n\r\n",
                HeadError::BadTarget,
            ),
            (b"GET https://[::1/ HTTP/1.1\r\n\r\n", HeadError::BadTarget),
            (b"GET https://x/ HTTP/1.0\r\n\r\n", HeadError::BadStartLine),
            (b"GET  https://x/ HTTP/1.1\r\n\r\n", HeadError::BadStartLine),
            (
                b"GET https://x/ HTTP/1.1\r\nbad header\r\n\r\n",
                HeadError::BadHeader,
            ),
            (
                b"GET https://x/ HTTP/1.1\r\na: b\r\n folded\r\n\r\n",
                HeadError::BadHeader,
            ),
            (
                b"GET https://x/ HTTP/1.1\r\na: b\nc: d\r\n\r\n",
                HeadError::BadHeader,
            ),
            (
                b"GET https://x/ HTTP/1.1\r\nb@d: v\r\n\r\n",
                HeadError::BadHeader,
            ),
            (b"GET https://x/ HTTP/1.1\r\n", HeadError::BadStartLine),
        ] {
            assert_eq!(
                parse_request_head(head),
                Err(want),
                "{}",
                String::from_utf8_lossy(head)
            );
        }
    }

    #[test]
    fn heads_over_the_caps_are_refused() {
        let mut big = b"GET https://x/ HTTP/1.1\r\n".to_vec();
        for i in 0..=MAX_HEADER_LINES {
            big.extend_from_slice(format!("h{i}: v\r\n").as_bytes());
        }
        big.extend_from_slice(b"\r\n");
        assert_eq!(parse_request_head(&big), Err(HeadError::TooLarge));
        let mut long = b"GET https://x/".to_vec();
        long.extend(std::iter::repeat_n(b'a', MAX_HEAD_BYTES));
        long.extend_from_slice(b" HTTP/1.1\r\n\r\n");
        assert_eq!(parse_request_head(&long), Err(HeadError::TooLarge));
    }

    #[test]
    fn final_responses_are_rewritten_to_close_and_interim_ones_pass() {
        let r = parse_response_head(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\n\r\n",
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert!(!r.is_interim());
        assert_eq!(
            r.head,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nconnection: close\r\n\r\n".to_vec()
        );
        let hints = parse_response_head(b"HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n").unwrap();
        assert!(hints.is_interim());
        let switching =
            parse_response_head(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\r\n")
                .unwrap();
        assert!(!switching.is_interim());
        assert_eq!(
            switching.head,
            b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\r\n".to_vec()
        );
        assert_eq!(
            parse_response_head(b"HTTP/1.1 2000 OK\r\n\r\n"),
            Err(HeadError::BadStartLine)
        );
        assert_eq!(
            parse_response_head(b"HTTP/2 200\r\n\r\n"),
            Err(HeadError::BadStartLine)
        );
    }

    /// Stands in for the host proxy: answers each tunnel with `reply`, then
    /// plays an origin that records the request and answers `response`.
    struct FakeHost {
        /// Status line the host proxy answers the `CONNECT` with.
        reply: &'static str,
        /// What the fake origin sends back after the request head.
        response: &'static [u8],
        /// Bytes each tunnel received, CONNECT included.
        seen: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait::async_trait]
    impl TunnelOpener for FakeHost {
        type Stream = tokio::io::DuplexStream;

        async fn open(&self) -> Result<Self::Stream> {
            let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
            let (reply, response, seen) = (self.reply, self.response, Arc::clone(&self.seen));
            tokio::spawn(async move {
                let Ok((connect, _)) = read_head(&mut theirs, Vec::new()).await else {
                    return;
                };
                let mut got = connect;
                let _ = theirs.write_all(reply.as_bytes()).await;
                if reply.contains(" 200 ") {
                    if let Ok((head, _)) = read_head(&mut theirs, Vec::new()).await {
                        got.extend_from_slice(&head);
                        let _ = theirs.write_all(response).await;
                    }
                }
                seen.lock().unwrap().push(got);
            });
            Ok(ours)
        }
    }

    async fn run_relay(host: FakeHost, request: &[u8]) -> Vec<u8> {
        let (mut workerd, relay_side) = tokio::io::duplex(64 * 1024);
        let tls = crate::socket_broker::default_tls().unwrap();
        let task = tokio::spawn(relay_one(relay_side, Arc::new(host), tls));
        workerd.write_all(request).await.unwrap();
        let mut out = Vec::new();
        let _ = workerd.read_to_end(&mut out).await;
        let _ = task.await;
        out
    }

    #[tokio::test]
    async fn plain_http_is_forwarded_through_the_tunnel_and_closed() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let out = run_relay(
            FakeHost {
                reply: "HTTP/1.1 200 Connection Established\r\n\r\n",
                response: b"HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: keep-alive\r\n\r\nok",
                seen: Arc::clone(&seen),
            },
            b"GET http://plain.example.com:8080/p HTTP/1.1\r\nx-a: 1\r\n\r\n",
        )
        .await;
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(
            String::from_utf8(seen[0].clone()).unwrap(),
            "CONNECT plain.example.com:8080 HTTP/1.1\r\nhost: plain.example.com:8080\r\n\r\n\
             GET /p HTTP/1.1\r\nhost: plain.example.com:8080\r\nx-a: 1\r\nconnection: close\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn a_refused_tunnel_closes_without_a_response() {
        let out = run_relay(
            FakeHost {
                reply: "HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain\r\n\r\nno",
                response: b"",
                seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            },
            b"GET http://denied.example.com/ HTTP/1.1\r\n\r\n",
        )
        .await;
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    #[tokio::test]
    async fn connect_is_spliced_after_the_host_accepts() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut workerd, relay_side) = tokio::io::duplex(64 * 1024);
        let tls = crate::socket_broker::default_tls().unwrap();
        let host = FakeHost {
            reply: "HTTP/1.1 200 Connection Established\r\n\r\n",
            response: b"",
            seen: Arc::clone(&seen),
        };
        let task = tokio::spawn(relay_one(relay_side, Arc::new(host), tls));
        workerd
            .write_all(b"CONNECT db.example.com:5432 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let (head, _) = read_head(&mut workerd, Vec::new()).await.unwrap();
        assert_eq!(
            head,
            b"HTTP/1.1 200 Connection Established\r\n\r\n".to_vec()
        );
        drop(workerd);
        let _ = task.await;
        assert!(seen.lock().unwrap()[0].starts_with(b"CONNECT db.example.com:5432 HTTP/1.1\r\n"));
    }

    #[test]
    fn head_end_is_found_only_after_the_blank_line() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
        assert_eq!(find_head_end(b"a\r\n\r\nbody"), Some(5));
    }

    /// The whole egress chain, host side included: workerd's CONNECT, a
    /// half-close after the request, and the reply still arriving.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn half_closed_tunnel_still_gets_the_reply_through_the_host_proxy() {
        use bookclerk_plugin_manifest::{EgressPolicy, NetworkMode, TcpGrant};
        use std::sync::atomic::AtomicBool;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for _ in 0..20 {
            let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = echo.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (mut s, _) = echo.accept().await.unwrap();
                let mut buf = [0_u8; 64];
                let n = s.read(&mut buf).await.unwrap();
                s.write_all(&buf[..n]).await.unwrap();
            });
            let policy = EgressPolicy {
                mode: NetworkMode::Outbound,
                tcp: vec![TcpGrant {
                    host: "127.0.0.1".into(),
                    ports: vec![port],
                }],
                address_cidrs: vec!["127.0.0.1/32".into()],
                ..EgressPolicy::deny()
            };
            let (host, launcher) = tokio::net::UnixStream::pair().unwrap();
            let challenge = [3_u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
            let _proxy = crate::socket_proxy::spawn_link_with_challenge(
                host,
                crate::socket_proxy::LinkPolicy::isolate(policy),
                Arc::new(AtomicBool::new(false)),
                challenge,
            )
            .unwrap();
            let (reader, mut writer) = tokio::io::split(launcher);
            writer.write_all(&challenge).await.unwrap();
            let mux = Arc::new(bookclerk_plugin_sdk::mux::Mux::client(reader, writer));
            let (mut client, relay_end) = tokio::io::duplex(64 * 1024);
            let tls = crate::socket_broker::default_tls().unwrap();
            let relay = tokio::spawn(relay_one(relay_end, mux, tls));
            client
                .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut head = [0_u8; 39];
            client.read_exact(&mut head).await.unwrap();
            assert_eq!(&head[..], b"HTTP/1.1 200 Connection Established\r\n\r\n");
            client.write_all(b"ping\n").await.unwrap();
            client.shutdown().await.unwrap();
            let mut got = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut got))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(got, b"ping\n");
            let _ = relay.await;
        }
    }
}
