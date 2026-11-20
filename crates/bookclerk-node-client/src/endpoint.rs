//! Reaching the owner: the transport policy, TLS with the SPKI pin, the
//! socket upgrade, and the HTTP requests a host makes (enrollment,
//! artifacts, and streamed uploads).
//!
//! Decision 1 of the H-4 design: TLS everywhere except loopback.
//!
//! - `https://` origins are dialed over TLS. With an SPKI pin from the
//!   enrollment link, the owner's certificate must carry exactly that
//!   public key (base64url SHA-256 of its SubjectPublicKeyInfo) and must
//!   sign the handshake with it; names and issuers are not consulted, so a
//!   self-signed owner on a private network works. Without a pin the
//!   certificate must chain to the webpki roots and match the host name.
//! - `http://` origins are accepted only for a loopback host (an IP
//!   literal, or `localhost`), every address the name resolves to must be
//!   loopback, and a pinned link may not fall back to plain HTTP.
//!
//! Everything else is refused before any byte is sent.

use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_node_proto::{
    canonical_origin, UploadAnswer, UploadPart, UploadRange, CLERK_AUTH_SCHEME, CLERK_CONNECT_PATH,
    CLERK_SUBPROTOCOL, MAX_FRAME_BYTES, SUBPROTOCOL,
};
use sha2::Digest as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    verify_tls12_signature, verify_tls13_signature, CryptoProvider, WebPkiSupportedAlgorithms,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::server::ParsedCertificate;
use tokio_rustls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::WebSocketStream;
use zeroize::Zeroizing;

/// Path of the node socket.
pub const CONNECT_PATH: &str = "/api/node/v1/connect";

/// Path of the enroll endpoint.
pub const ENROLL_PATH: &str = "/api/node/v1/enroll";

pub use bookclerk_node_proto::{ENROLL_OAUTH_PATH, INFO_PATH};

/// A message on an [`OwnerSocket`] (text, binary, ping, pong or close).
pub use tokio_tungstenite::tungstenite::Message as SocketMessage;

/// Seconds allowed for the TCP connect and for the TLS handshake.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Seconds allowed for an HTTP exchange (enrollment).
const HTTP_TIMEOUT_SECS: u64 = 30;

/// Largest HTTP answer read (enrollment answers are tiny).
const MAX_HTTP_RESPONSE_BYTES: usize = 64 * 1024;

/// Seconds allowed for an artifact download.
const ARTIFACT_TIMEOUT_SECS: u64 = 120;

/// Seconds an upload's answer may take after its last byte (the owner's
/// destinations finish their writes first).
pub const UPLOAD_ANSWER_TIMEOUT_SECS: u64 = 300;

/// Seconds one read of an upload's body, or one write of it to the owner,
/// may stall before the upload is abandoned (a stalled source or an owner
/// that stopped reading); also how long an answer is awaited after a
/// failed write.
#[cfg(not(test))]
const UPLOAD_IDLE_TIMEOUT_SECS: u64 = HTTP_TIMEOUT_SECS;

/// Seconds one upload read or write may stall (short under test).
#[cfg(test)]
const UPLOAD_IDLE_TIMEOUT_SECS: u64 = 1;

/// Bytes moved per read / write of a streamed upload.
const UPLOAD_COPY_CHUNK: usize = 64 * 1024;

/// Runs one I/O step of a streamed upload, failing it with
/// [`std::io::ErrorKind::TimedOut`] once it stalls past
/// [`UPLOAD_IDLE_TIMEOUT_SECS`].
///
/// # Errors
///
/// Returns the step's error, or `TimedOut`.
async fn idle_step<T>(
    step: impl std::future::Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    tokio::time::timeout(Duration::from_secs(UPLOAD_IDLE_TIMEOUT_SECS), step)
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the upload stalled",
            ))
        })
}

/// A byte stream to the owner, plain (loopback) or TLS.
pub trait OwnerIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> OwnerIo for T {}

/// The owner socket.
pub type OwnerSocket = WebSocketStream<Box<dyn OwnerIo>>;

/// Why the owner could not be reached.
#[derive(Debug, thiserror::Error)]
pub enum DialError {
    /// The transport policy refuses this origin (plain HTTP to a
    /// non-loopback host, or a pin on a plain-HTTP origin).
    #[error("insecure transport refused: {0}")]
    Insecure(&'static str),
    /// The origin or pin is malformed.
    #[error("invalid owner endpoint: {0}")]
    Invalid(String),
    /// TCP connect or I/O failed.
    #[error("owner unreachable: {0}")]
    Io(#[from] std::io::Error),
    /// The TLS handshake failed: a wrong pin, an untrusted certificate, or
    /// a protocol error.
    #[error("TLS to the owner failed: {0}")]
    Tls(String),
    /// The owner answered the upgrade with an HTTP status.
    #[error("owner refused the socket with HTTP {0}")]
    Refused(u16),
    /// The WebSocket handshake failed otherwise.
    #[error("socket handshake failed: {0}")]
    Handshake(String),
    /// The owner's HTTP answer was malformed or too large.
    #[error("malformed HTTP answer from the owner")]
    Http,
    /// A step did not finish in time.
    #[error("timed out reaching the owner")]
    Timeout,
}

/// Where and how to reach one owner.
#[derive(Clone)]
pub struct OwnerEndpoint {
    /// Canonical origin (`https://owner.example:8443`), also the origin the
    /// handshake signature covers.
    origin: String,
    /// True for `https`.
    tls: bool,
    /// Host name or IP literal, without IPv6 brackets.
    host: String,
    /// TCP port.
    port: u16,
    /// SHA-256 of the owner certificate's SubjectPublicKeyInfo.
    pin: Option<[u8; 32]>,
}

impl std::fmt::Debug for OwnerEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerEndpoint")
            .field("origin", &self.origin)
            .field("pinned", &self.pin.is_some())
            .finish()
    }
}

/// True for a host the policy treats as loopback before resolving it.
fn is_loopback_host(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

impl OwnerEndpoint {
    /// Checks `origin` (and the link's `spki_pin`) against the transport
    /// policy.
    ///
    /// # Errors
    ///
    /// Returns [`DialError::Invalid`] for a malformed origin or pin and
    /// [`DialError::Insecure`] for plain HTTP to a non-loopback host or a
    /// pinned plain-HTTP origin.
    pub fn new(origin: &str, spki_pin: Option<&str>) -> Result<Self, DialError> {
        let canonical = canonical_origin(origin)
            .ok_or_else(|| DialError::Invalid("owner origin must be an http(s) origin".into()))?;
        let url = url::Url::parse(&canonical)
            .map_err(|_| DialError::Invalid("owner origin does not parse".into()))?;
        let tls = url.scheme() == "https";
        let host = url
            .host_str()
            .map(|host| host.trim_matches(['[', ']']).to_string())
            .filter(|host| !host.is_empty())
            .ok_or_else(|| DialError::Invalid("owner origin has no host".into()))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| DialError::Invalid("owner origin has no port".into()))?;
        let pin = spki_pin
            .map(|pin| {
                bookclerk_library::control_plane::decode_spki_pin(pin)
                    .map_err(|_| DialError::Invalid("SPKI pin is malformed".into()))
            })
            .transpose()?;
        if !tls {
            if pin.is_some() {
                return Err(DialError::Insecure("pinned_link_over_plain_http"));
            }
            if !is_loopback_host(&host) {
                return Err(DialError::Insecure("plain_http_off_loopback"));
            }
        }
        Ok(Self {
            origin: canonical,
            tls,
            host,
            port,
            pin,
        })
    }

    /// The canonical origin the handshake signs.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// True when the link pins the owner's certificate.
    #[must_use]
    pub fn is_pinned(&self) -> bool {
        self.pin.is_some()
    }

    /// `host[:port]` as the origin spells it.
    fn authority(&self) -> &str {
        self.origin
            .split_once("://")
            .map_or(self.origin.as_str(), |(_, authority)| authority)
    }

    /// Resolves the host. A plain-HTTP origin must resolve only to
    /// loopback addresses.
    async fn addresses(&self) -> Result<Vec<SocketAddr>, DialError> {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await?
            .collect();
        if addrs.is_empty() {
            return Err(DialError::Invalid("owner host does not resolve".into()));
        }
        if !self.tls && !addrs.iter().all(|addr| addr.ip().is_loopback()) {
            return Err(DialError::Insecure("plain_http_off_loopback"));
        }
        Ok(addrs)
    }

    /// Opens a byte stream to the owner under the policy.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the owner is unreachable or TLS fails.
    pub async fn connect(&self) -> Result<Box<dyn OwnerIo>, DialError> {
        let timeout = Duration::from_secs(CONNECT_TIMEOUT_SECS);
        let mut last = None;
        let mut tcp = None;
        for addr in self.addresses().await? {
            match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => {
                    tcp = Some(stream);
                    break;
                }
                Ok(Err(err)) => last = Some(DialError::Io(err)),
                Err(_) => last = Some(DialError::Timeout),
            }
        }
        let tcp = tcp.ok_or_else(|| last.unwrap_or(DialError::Timeout))?;
        let _ = tcp.set_nodelay(true);
        if !self.tls {
            return Ok(Box::new(tcp));
        }
        let name = ServerName::try_from(self.host.clone())
            .map_err(|_| DialError::Invalid("owner host is not a valid TLS name".into()))?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config(self.pin)?));
        let tls = tokio::time::timeout(timeout, connector.connect(name, tcp))
            .await
            .map_err(|_| DialError::Timeout)?
            .map_err(|err| DialError::Tls(err.to_string()))?;
        Ok(Box::new(tls))
    }

    /// Dials the node socket, offering [`SUBPROTOCOL`] and requiring the
    /// owner to select it.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the connection or the upgrade fails.
    pub async fn connect_socket(&self) -> Result<OwnerSocket, DialError> {
        self.upgrade(CONNECT_PATH, SUBPROTOCOL, None, MAX_FRAME_BYTES)
            .await
    }

    /// Dials a library's clerk socket ([`CLERK_CONNECT_PATH`]) with a
    /// library card, offering [`CLERK_SUBPROTOCOL`] and requiring the
    /// library to select it. The card is written straight to the stream and
    /// never logged.
    ///
    /// # Arguments
    ///
    /// * `card` - The library card (the token alone, without the scheme).
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the connection or the upgrade fails;
    /// [`DialError::Refused`] carries the library's status (401 when the
    /// card was ended).
    pub async fn connect_clerk_socket(&self, card: &str) -> Result<OwnerSocket, DialError> {
        let authorization = Zeroizing::new(format!("{CLERK_AUTH_SCHEME} {card}"));
        self.upgrade(
            CLERK_CONNECT_PATH,
            CLERK_SUBPROTOCOL,
            Some(&authorization),
            MAX_FRAME_BYTES,
        )
        .await
    }

    /// Upgrades `path` to a WebSocket offering `subprotocol`, with an
    /// optional `Authorization` header, and requires the peer to select it.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the connection or the upgrade fails.
    async fn upgrade(
        &self,
        path: &str,
        subprotocol: &'static str,
        authorization: Option<&str>,
        max_message: usize,
    ) -> Result<OwnerSocket, DialError> {
        let io = self.connect().await?;
        let scheme = if self.tls { "wss" } else { "ws" };
        let mut request = format!("{scheme}://{}{path}", self.authority())
            .into_client_request()
            .map_err(|err| DialError::Invalid(err.to_string()))?;
        request.headers_mut().insert(
            "sec-websocket-protocol",
            tokio_tungstenite::tungstenite::http::HeaderValue::from_static(subprotocol),
        );
        if let Some(authorization) = authorization {
            let mut value =
                tokio_tungstenite::tungstenite::http::HeaderValue::from_str(authorization)
                    .map_err(|_| DialError::Invalid("malformed authorization".into()))?;
            value.set_sensitive(true);
            request.headers_mut().insert("authorization", value);
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_message))
            .max_frame_size(Some(max_message));
        let upgrade = tokio_tungstenite::client_async_with_config(request, io, Some(config));
        let (socket, response) =
            tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), upgrade)
                .await
                .map_err(|_| DialError::Timeout)?
                .map_err(|err| match err {
                    tokio_tungstenite::tungstenite::Error::Http(response) => {
                        DialError::Refused(response.status().as_u16())
                    }
                    other => DialError::Handshake(other.to_string()),
                })?;
        let selected = response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok());
        if selected != Some(subprotocol) {
            return Err(DialError::Handshake(format!(
                "the peer did not select the {subprotocol} subprotocol"
            )));
        }
        Ok(socket)
    }

    /// `POST path` with a JSON body; answers the status and body. The body
    /// is written straight to the stream and never logged.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the request cannot be made or the answer
    /// is malformed.
    pub async fn post_json(&self, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), DialError> {
        self.post_json_within(path, body, 0).await
    }

    /// [`post_json`](Self::post_json) for an answer that may carry up to
    /// `extra_body` more bytes than the usual response limit (a redeem
    /// answer with sealed store connections).
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the request cannot be made or the answer
    /// is malformed or over the limit.
    pub async fn post_json_within(
        &self,
        path: &str,
        body: &[u8],
        extra_body: usize,
    ) -> Result<(u16, Vec<u8>), DialError> {
        let exchange = async {
            let mut io = self.connect().await?;
            let head = format!(
                "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
                 Accept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                self.authority(),
                body.len()
            );
            io.write_all(head.as_bytes()).await?;
            io.write_all(body).await?;
            io.flush().await?;
            read_http_response(&mut io, MAX_HTTP_RESPONSE_BYTES.saturating_add(extra_body)).await
        };
        tokio::time::timeout(Duration::from_secs(HTTP_TIMEOUT_SECS), exchange)
            .await
            .map_err(|_| DialError::Timeout)?
    }

    /// `GET path` of a small JSON answer (at most `max_body` bytes), over the
    /// verified transport (the link's pin or the webpki roots). Nothing
    /// secret is sent.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the request cannot be made, the answer is
    /// malformed, or it is larger than `max_body`.
    pub async fn get_json(&self, path: &str, max_body: usize) -> Result<(u16, Vec<u8>), DialError> {
        if path.contains(['\r', '\n', ' ']) {
            return Err(DialError::Http);
        }
        let exchange = async {
            let mut io = self.connect().await?;
            let head = format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\n\
                 Connection: close\r\n\r\n",
                self.authority(),
            );
            io.write_all(head.as_bytes()).await?;
            io.flush().await?;
            read_http_response(&mut io, max_body.saturating_add(MAX_HTTP_RESPONSE_BYTES)).await
        };
        let (status, body) = tokio::time::timeout(Duration::from_secs(HTTP_TIMEOUT_SECS), exchange)
            .await
            .map_err(|_| DialError::Timeout)??;
        if body.len() > max_body {
            return Err(DialError::Http);
        }
        Ok((status, body))
    }

    /// `GET path` with an `Authorization` header; answers the status and a
    /// body of at most `max_body` bytes. The header is written straight to
    /// the stream and never logged.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the request cannot be made, the answer is
    /// malformed, or it is larger than `max_body`.
    pub async fn get_authorized(
        &self,
        path: &str,
        authorization: &str,
        max_body: usize,
    ) -> Result<(u16, Vec<u8>), DialError> {
        if authorization.contains(['\r', '\n']) || path.contains(['\r', '\n', ' ']) {
            return Err(DialError::Http);
        }
        let exchange = async {
            let mut io = self.connect().await?;
            let head = format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: {authorization}\r\n\
                 Accept: application/octet-stream\r\nConnection: close\r\n\r\n",
                self.authority(),
            );
            io.write_all(head.as_bytes()).await?;
            io.flush().await?;
            read_http_response(&mut io, max_body.saturating_add(MAX_HTTP_RESPONSE_BYTES)).await
        };
        let (status, body) =
            tokio::time::timeout(Duration::from_secs(ARTIFACT_TIMEOUT_SECS), exchange)
                .await
                .map_err(|_| DialError::Timeout)??;
        if body.len() > max_body {
            return Err(DialError::Http);
        }
        Ok((status, body))
    }
}

/// Most times one file sent in parts resumes before
/// [`OwnerEndpoint::put_file`] gives up (a later attempt resumes again).
pub const MAX_UPLOAD_RESUMES: u32 = 8;

/// Where an uploaded file's bytes come from: readable from any offset, so
/// an interrupted file resends only what the owner lacks.
pub trait UploadSource: Sync {
    /// A reader of the file from `offset` to its end.
    ///
    /// # Errors
    ///
    /// An I/O error when the file cannot be opened at `offset`.
    fn open_at(
        &self,
        offset: u64,
    ) -> impl std::future::Future<Output = std::io::Result<Pin<Box<dyn AsyncRead + Send>>>> + Send;
}

/// A file on this install's disk as an [`UploadSource`].
#[derive(Debug, Clone)]
pub struct LocalFileSource(pub std::path::PathBuf);

impl UploadSource for LocalFileSource {
    async fn open_at(&self, offset: u64) -> std::io::Result<Pin<Box<dyn AsyncRead + Send>>> {
        use tokio::io::AsyncSeekExt as _;
        let mut file = tokio::fs::File::open(&self.0).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        Ok(Box::pin(file))
    }
}

/// The offset an upload answer tells the host to send from: `200 partial`
/// or `503 unavailable` with reason `resume`; `None` for any other answer.
fn resume_offset(status: u16, body: &[u8]) -> Option<u64> {
    let answer: UploadAnswer = serde_json::from_slice(body).ok()?;
    let resumable = (status == 200 && answer.status == "partial")
        || (status == 503 && answer.reason.as_deref() == Some("resume"));
    if resumable {
        answer.offset
    } else {
        None
    }
}

impl OwnerEndpoint {
    /// `PUT path` streaming exactly `length` bytes from `body`, with an
    /// `Authorization` header; answers the status and the (small) body.
    /// The header and the bytes are written straight to the stream and
    /// never logged. Nothing is buffered beyond one copy chunk.
    ///
    /// An owner may answer before it has read the whole body (a lost
    /// fence); the answer is still read after a failed write, for at most
    /// the idle timeout. Every read of `body` and every write to the owner
    /// must make progress within `UPLOAD_IDLE_TIMEOUT_SECS`.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the request cannot be made, `body` ends
    /// early, a read or write stalls ([`DialError::Timeout`]), or the
    /// answer is malformed or does not come within
    /// `UPLOAD_ANSWER_TIMEOUT_SECS` of the last byte.
    pub async fn put_stream<R: AsyncRead + Unpin + Send>(
        &self,
        path: &str,
        authorization: &str,
        length: u64,
        body: R,
    ) -> Result<(u16, Vec<u8>), DialError> {
        self.put_stream_range(path, authorization, length, None, body)
            .await
    }

    /// `PUT path` of a declared upload file of `size` bytes read from
    /// `source`: in one request when it fits in `part_bytes`, otherwise as
    /// consecutive parts of at most `part_bytes`, each its own request with
    /// `Content-Range` ([`UploadPart`]), so a relay that caps request bodies
    /// passes it.
    ///
    /// A file sent in parts resumes: it first asks where the owner is
    /// (`bytes */<size>`, [`UploadRange::Status`]) and sends from that
    /// offset, and after a broken part or a `resume` answer it asks again
    /// and sends only the rest, at most [`MAX_UPLOAD_RESUMES`] times.
    /// Answers the status and body of the request that settled the file:
    /// the last part's, the first that was neither `200 partial` nor a
    /// `resume`, or the first that says the owner already holds the whole
    /// file (`received`).
    ///
    /// # Errors
    ///
    /// As [`Self::put_stream`], once the resumes run out; and
    /// [`DialError::Io`] when `source` cannot be read from an offset.
    pub async fn put_file<S: UploadSource + ?Sized>(
        &self,
        path: &str,
        authorization: &str,
        size: u64,
        part_bytes: u64,
        source: &S,
    ) -> Result<(u16, Vec<u8>), DialError> {
        let part_bytes = part_bytes.max(1);
        if size <= part_bytes {
            let body = source.open_at(0).await.map_err(DialError::Io)?;
            return self.put_stream(path, authorization, size, body).await;
        }
        let mut resumes = 0u32;
        let mut offset = match self.upload_status(path, authorization, size).await? {
            Ok(offset) => offset,
            Err(settled) => return Ok(settled),
        };
        'file: loop {
            if offset >= size {
                return Err(DialError::Http);
            }
            let mut body = source.open_at(offset).await.map_err(DialError::Io)?;
            while offset < size {
                let part = UploadPart {
                    offset,
                    length: part_bytes.min(size - offset),
                };
                let range = part.content_range(size);
                let sent = self
                    .put_stream_range(path, authorization, part.length, Some(&range), &mut body)
                    .await;
                let next = match sent {
                    Ok((status, answer)) => match resume_offset(status, &answer) {
                        Some(next) if status == 200 && next == part.end() => {
                            offset = next;
                            continue;
                        }
                        Some(next) => next,
                        None => return Ok((status, answer)),
                    },
                    Err(err) => {
                        if resumes >= MAX_UPLOAD_RESUMES {
                            return Err(err);
                        }
                        match self.upload_status(path, authorization, size).await? {
                            Ok(next) => next,
                            Err(settled) => return Ok(settled),
                        }
                    }
                };
                // The owner stands somewhere else: read on from there.
                resumes += 1;
                if resumes > MAX_UPLOAD_RESUMES {
                    return Err(DialError::Http);
                }
                offset = next;
                continue 'file;
            }
            return Err(DialError::Http);
        }
    }

    /// Asks where the owner stands with a declared file of `size` bytes
    /// (an empty `PUT` with `Content-Range: bytes */<size>`).
    ///
    /// # Returns
    ///
    /// `Ok(offset)`: the next byte the owner needs; `Err(answer)`: an
    /// answer that settles the file (it is `received`, or refused).
    async fn upload_status(
        &self,
        path: &str,
        authorization: &str,
        size: u64,
    ) -> Result<Result<u64, (u16, Vec<u8>)>, DialError> {
        let range = UploadRange::status_header(size);
        let (status, answer) = self
            .put_stream_range(path, authorization, 0, Some(&range), &[][..])
            .await?;
        Ok(match resume_offset(status, &answer) {
            Some(offset) => Ok(offset),
            None => Err((status, answer)),
        })
    }

    /// [`Self::put_stream`], with an optional `Content-Range` value sent
    /// as a header (an upload part).
    async fn put_stream_range<R: AsyncRead + Unpin + Send>(
        &self,
        path: &str,
        authorization: &str,
        length: u64,
        content_range: Option<&str>,
        body: R,
    ) -> Result<(u16, Vec<u8>), DialError> {
        if authorization.contains(['\r', '\n'])
            || path.contains(['\r', '\n', ' '])
            || content_range.is_some_and(|range| range.contains(['\r', '\n']))
        {
            return Err(DialError::Http);
        }
        let mut io = self.connect().await?;
        let range = content_range
            .map(|range| format!("Content-Range: {range}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "PUT {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: {authorization}\r\n\
             Content-Type: application/octet-stream\r\nAccept: application/json\r\n\
             {range}Content-Length: {length}\r\nConnection: close\r\n\r\n",
            self.authority(),
        );
        let sent = async {
            idle_step(io.write_all(head.as_bytes())).await?;
            let mut body = body.take(length);
            let mut chunk = vec![0u8; UPLOAD_COPY_CHUNK];
            let mut copied = 0u64;
            loop {
                let n = idle_step(body.read(&mut chunk)).await?;
                if n == 0 {
                    break;
                }
                idle_step(io.write_all(&chunk[..n])).await?;
                copied += n as u64;
            }
            if copied != length {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the upload body ended early",
                ));
            }
            idle_step(io.flush()).await
        }
        .await;
        // After a failed write only an answer the owner already sent (or
        // sends promptly) is worth waiting for.
        let answer_secs = if sent.is_ok() {
            UPLOAD_ANSWER_TIMEOUT_SECS
        } else {
            UPLOAD_IDLE_TIMEOUT_SECS
        };
        let answer = tokio::time::timeout(
            Duration::from_secs(answer_secs),
            read_http_response(&mut io, MAX_HTTP_RESPONSE_BYTES),
        )
        .await;
        match (sent, answer) {
            (_, Ok(Ok(answer))) => Ok(answer),
            (Err(err), _) if err.kind() == std::io::ErrorKind::TimedOut => Err(DialError::Timeout),
            (Err(err), _) => Err(DialError::Io(err)),
            (Ok(()), Ok(Err(err))) => Err(err),
            (Ok(()), Err(_)) => Err(DialError::Timeout),
        }
    }

    /// `POST path` with a JSON body and an `Authorization` header; answers
    /// the status and body. Neither is logged.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the request cannot be made or the answer
    /// is malformed.
    pub async fn post_json_authorized(
        &self,
        path: &str,
        authorization: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), DialError> {
        if authorization.contains(['\r', '\n']) || path.contains(['\r', '\n', ' ']) {
            return Err(DialError::Http);
        }
        let exchange = async {
            let mut io = self.connect().await?;
            let head = format!(
                "POST {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: {authorization}\r\n\
                 Content-Type: application/json\r\nAccept: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                self.authority(),
                body.len()
            );
            io.write_all(head.as_bytes()).await?;
            io.write_all(body).await?;
            io.flush().await?;
            read_http_response(&mut io, MAX_HTTP_RESPONSE_BYTES).await
        };
        tokio::time::timeout(Duration::from_secs(HTTP_TIMEOUT_SECS), exchange)
            .await
            .map_err(|_| DialError::Timeout)?
    }
}

impl OwnerEndpoint {
    /// `GET path` of one file of a copy the library granted, streamed into
    /// `file`: the bytes from `resume_from` on when the library honours
    /// the range, else the whole file. Nothing is buffered beyond one copy
    /// chunk, and the header is written straight to the stream and never
    /// logged.
    ///
    /// The answer's head is read by
    /// [`bookclerk_node_proto::parse_copy_answer`], so a body is taken
    /// only when it is exactly the bytes asked for: `file` is cut to
    /// empty before a whole file is written and is written from
    /// `resume_from` otherwise, and no more than the stated length is
    /// taken. Every read from the library must make progress within the
    /// idle timeout.
    ///
    /// # Arguments
    ///
    /// * `resume_from` - Bytes of the file `file` already holds; 0 asks
    ///   for the whole file.
    /// * `size` - The file's exact size, as the grant stated it.
    ///
    /// # Returns
    ///
    /// The status: `200` or `206` once `file` holds all `size` bytes, any
    /// other status when the library sent no file bytes (`file` is then
    /// untouched).
    ///
    /// # Errors
    ///
    /// Returns [`CopyStreamError::Transfer`] when the request cannot be
    /// made, the answer does not state exactly the bytes asked for or ends
    /// early ([`DialError::Http`]), or a read stalls
    /// ([`DialError::Timeout`]); [`CopyStreamError::Local`] when `file`
    /// cannot be written.
    pub async fn get_copy_stream(
        &self,
        path: &str,
        authorization: &str,
        resume_from: u64,
        size: u64,
        file: &mut tokio::fs::File,
    ) -> Result<u16, CopyStreamError> {
        use bookclerk_node_proto::{copy_answer_head_end, parse_copy_answer, CopyAnswer};
        use tokio::io::AsyncSeekExt as _;
        use CopyStreamError::Local;

        if authorization.contains(['\r', '\n']) || path.contains(['\r', '\n', ' ']) {
            return Err(DialError::Http.into());
        }
        let mut io = self.connect().await?;
        let range = if resume_from > 0 {
            format!("Range: bytes={resume_from}-\r\n")
        } else {
            String::new()
        };
        let head = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: {authorization}\r\n\
             Accept: application/octet-stream\r\nAccept-Encoding: identity\r\n{range}\
             Connection: close\r\n\r\n",
            self.authority(),
        );
        let stalled = |err: std::io::Error| {
            if err.kind() == std::io::ErrorKind::TimedOut {
                DialError::Timeout
            } else {
                DialError::Io(err)
            }
        };
        idle_step(io.write_all(head.as_bytes()))
            .await
            .map_err(stalled)?;
        idle_step(io.flush()).await.map_err(stalled)?;

        let mut chunk = vec![0u8; UPLOAD_COPY_CHUNK];
        let mut buf = Vec::new();
        let broken = |problem: &'static str| {
            tracing::warn!(problem, "a copy download's answer breaks the rules");
            DialError::Http
        };
        // Where the head ends is decided over everything received so far,
        // so it does not depend on how the answer was cut into reads.
        let head_end = loop {
            if let Some(at) = copy_answer_head_end(&buf).map_err(broken)? {
                break at;
            }
            let n = idle_step(io.read(&mut chunk)).await.map_err(stalled)?;
            if n == 0 {
                return Err(DialError::Http.into());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let answer = parse_copy_answer(&buf[..head_end], resume_from, size).map_err(broken)?;
        let (offset, length) = match answer {
            CopyAnswer::Status(status) => return Ok(status),
            CopyAnswer::Body { offset, length } => (offset, length),
        };
        if offset == 0 {
            cut_to_empty(file).await.map_err(Local)?;
        }
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(Local)?;
        // Nothing past the stated length is taken, whenever it arrives.
        let early = &buf[head_end + 4..];
        let early =
            &early[..usize::try_from(length).map_or(early.len(), |most| most.min(early.len()))];
        let mut taken = u64::try_from(early.len()).map_err(|_| DialError::Http)?;
        let body = async {
            file.write_all(early).await.map_err(Local)?;
            while taken < length {
                let want = usize::try_from(length - taken)
                    .unwrap_or(usize::MAX)
                    .min(chunk.len());
                let n = idle_step(io.read(&mut chunk[..want]))
                    .await
                    .map_err(stalled)?;
                if n == 0 {
                    return Err(DialError::Http.into());
                }
                file.write_all(&chunk[..n]).await.map_err(Local)?;
                taken += u64::try_from(n).map_err(|_| DialError::Http)?;
            }
            Ok::<(), CopyStreamError>(())
        }
        .await;
        // What arrived reaches the file even when the download failed, so
        // the next request resumes after it.
        file.flush().await.map_err(Local)?;
        body?;
        Ok(if offset == 0 { 200 } else { 206 })
    }
}

/// Why [`OwnerEndpoint::get_copy_stream`] did not bring the bytes.
#[derive(Debug, thiserror::Error)]
pub enum CopyStreamError {
    /// The request or the library's answer failed; asking again may work.
    #[error(transparent)]
    Transfer(#[from] DialError),
    /// The destination file could not be cut, positioned or written (a
    /// full disk, say); asking again does not help until that is fixed.
    #[error("the local file failed: {0}")]
    Local(std::io::Error),
}

/// Held while a destination file is being cut to empty, and taken before
/// a destination's length is read ([`cuts_finished`]).
///
/// The cut runs on the blocking pool and finishes even when the download
/// that asked for it is dropped. Without this a later download of the
/// same path could read the old length first, resume from it, and have
/// the late cut leave a hole before the bytes it then wrote.
static CUTTING: std::sync::LazyLock<Arc<tokio::sync::Mutex<()>>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(())));

/// Cuts `file` to empty, holding [`CUTTING`] until the cut is done
/// whether or not the caller is still waiting for it.
async fn cut_to_empty(file: &tokio::fs::File) -> std::io::Result<()> {
    let cutting = Arc::clone(&CUTTING).lock_owned().await;
    let handle = file.try_clone().await?.into_std().await;
    tokio::task::spawn_blocking(move || {
        let done = handle.set_len(0);
        drop(cutting);
        done
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Waits until no destination file is being cut to empty by a download,
/// a dropped one included; a length read after this is not behind a cut.
pub(crate) async fn cuts_finished() {
    drop(CUTTING.lock().await);
}

/// What a first TLS handshake showed of an owner's certificate, before any
/// pin is known ([`OwnerEndpoint::probe_json`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsProbe {
    /// base64url SHA-256 of the presented certificate's SubjectPublicKeyInfo.
    pub spki_pin: String,
    /// True when the certificate chains to the webpki roots and names the
    /// host (what an unpinned connection requires).
    pub webpki_trusted: bool,
}

impl OwnerEndpoint {
    /// `GET path` on an owner whose certificate is not known yet: the TLS
    /// handshake accepts any certificate whose key signs the handshake,
    /// records its SPKI pin and whether it would pass the webpki roots, and
    /// answers both with the status and body. Plain-HTTP (loopback) origins
    /// answer no probe.
    ///
    /// Only for reading what a library says about itself before its
    /// operator confirms the pin (trust on first use); nothing secret is
    /// sent. A pinned endpoint is refused.
    ///
    /// # Errors
    ///
    /// Returns [`DialError`] when the request cannot be made or the answer
    /// is malformed.
    pub async fn probe_json(
        &self,
        path: &str,
    ) -> Result<(u16, Vec<u8>, Option<TlsProbe>), DialError> {
        if self.pin.is_some() {
            return Err(DialError::Invalid("a pinned endpoint is not probed".into()));
        }
        if path.contains(['\r', '\n', ' ']) {
            return Err(DialError::Http);
        }
        let exchange = async {
            let probe = Arc::new(std::sync::Mutex::new(None));
            let mut io = if self.tls {
                let tcp = self.connect_tcp().await?;
                let name = ServerName::try_from(self.host.clone())
                    .map_err(|_| DialError::Invalid("owner host is not a valid TLS name".into()))?;
                let config = probe_config(Arc::clone(&probe))?;
                let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
                let tls = tokio::time::timeout(
                    Duration::from_secs(CONNECT_TIMEOUT_SECS),
                    connector.connect(name, tcp),
                )
                .await
                .map_err(|_| DialError::Timeout)?
                .map_err(|err| DialError::Tls(err.to_string()))?;
                Box::new(tls) as Box<dyn OwnerIo>
            } else {
                self.connect().await?
            };
            let head = format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\n\
                 Connection: close\r\n\r\n",
                self.authority(),
            );
            io.write_all(head.as_bytes()).await?;
            io.flush().await?;
            let (status, body) = read_http_response(&mut io, MAX_HTTP_RESPONSE_BYTES).await?;
            let seen = probe
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            Ok((status, body, seen))
        };
        tokio::time::timeout(Duration::from_secs(HTTP_TIMEOUT_SECS), exchange)
            .await
            .map_err(|_| DialError::Timeout)?
    }

    /// A TCP stream to the first address that answers.
    async fn connect_tcp(&self) -> Result<TcpStream, DialError> {
        let timeout = Duration::from_secs(CONNECT_TIMEOUT_SECS);
        let mut last = None;
        for addr in self.addresses().await? {
            match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    return Ok(stream);
                }
                Ok(Err(err)) => last = Some(DialError::Io(err)),
                Err(_) => last = Some(DialError::Timeout),
            }
        }
        Err(last.unwrap_or(DialError::Timeout))
    }
}

/// TLS settings that record the presented certificate ([`ProbeVerifier`]).
fn probe_config(seen: Arc<std::sync::Mutex<Option<TlsProbe>>>) -> Result<ClientConfig, DialError> {
    let provider = provider();
    let algorithms = provider.signature_verification_algorithms;
    let webpki = tokio_rustls::rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        }),
        Arc::clone(&provider),
    )
    .build()
    .map_err(|err| DialError::Tls(err.to_string()))?;
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| DialError::Tls(err.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ProbeVerifier {
            webpki,
            algorithms,
            seen,
        }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Accepts any certificate whose key signs the handshake and records its
/// pin and whether the webpki roots would have accepted it. Used only for
/// [`OwnerEndpoint::probe_json`]; the caller decides from the record.
#[derive(Debug)]
struct ProbeVerifier {
    /// The verifier an unpinned connection uses.
    webpki: Arc<tokio_rustls::rustls::client::WebPkiServerVerifier>,
    /// Signature algorithms of the crypto provider.
    algorithms: WebPkiSupportedAlgorithms,
    /// What the handshake showed.
    seen: Arc<std::sync::Mutex<Option<TlsProbe>>>,
}

impl ServerCertVerifier for ProbeVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        use base64::Engine as _;
        let pin = certificate_pin(end_entity).ok_or(
            tokio_rustls::rustls::Error::InvalidCertificate(CertificateError::BadEncoding),
        )?;
        let webpki_trusted = self
            .webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .is_ok();
        *self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(TlsProbe {
            spki_pin: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pin),
            webpki_trusted,
        });
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Reads one HTTP/1.1 answer of at most `max_bytes` (head and body): the
/// status and the body (by `Content-Length`, chunked, or until close).
async fn read_http_response(
    io: &mut Box<dyn OwnerIo>,
    max_bytes: usize,
) -> Result<(u16, Vec<u8>), DialError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut eof = false;
    let header_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at;
        }
        if eof || buf.len() > max_bytes {
            return Err(DialError::Http);
        }
        eof = read_some(io, &mut buf, &mut chunk).await?;
    };
    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| DialError::Http)?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(DialError::Http)?;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse::<usize>().map_err(|_| DialError::Http)?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }
    let body_start = header_end + 4;
    // Where the next chunk's size line starts in the body: each read only
    // scans what arrived since, so a large chunked body is not rescanned
    // (and re-copied) after every read.
    let mut next_chunk = 0usize;
    loop {
        let body = &buf[body_start..];
        let done = if chunked {
            chunked_body_complete(body, &mut next_chunk, max_bytes)?
        } else if let Some(length) = length {
            body.len() >= length
        } else {
            eof
        };
        if done || eof {
            break;
        }
        if buf.len() > max_bytes {
            return Err(DialError::Http);
        }
        eof = read_some(io, &mut buf, &mut chunk).await?;
    }
    let body = &buf[body_start..];
    let body = if chunked {
        decode_chunked(body, max_bytes)?.ok_or(DialError::Http)?
    } else if let Some(length) = length {
        body.get(..length).ok_or(DialError::Http)?.to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// Reads more bytes into `buf`; true at end of stream. A TLS peer that
/// closes without `close_notify` reads as end of stream.
async fn read_some(
    io: &mut Box<dyn OwnerIo>,
    buf: &mut Vec<u8>,
    chunk: &mut [u8; 4096],
) -> Result<bool, DialError> {
    match io.read(chunk).await {
        Ok(0) => Ok(true),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(false)
        }
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(true),
        Err(err) => Err(DialError::Io(err)),
    }
}

/// Position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Decodes a complete chunked body; `None` while incomplete.
///
/// `max_bytes` is the cap the caller read the response under (for an
/// artifact, the archive limit plus head room), so a chunked answer is held
/// to the same limit as one with a `Content-Length`.
///
/// # Errors
///
/// Returns [`DialError::Http`] for a malformed chunk size, or one whose
/// chunk would end past `max_bytes` of body.
fn decode_chunked(mut body: &[u8], max_bytes: usize) -> Result<Option<Vec<u8>>, DialError> {
    let mut out = Vec::new();
    // Bytes of the whole body already behind `body`.
    let mut consumed = 0usize;
    loop {
        let Some(line_end) = find(body, b"\r\n") else {
            return Ok(None);
        };
        let size_text = std::str::from_utf8(&body[..line_end]).map_err(|_| DialError::Http)?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| DialError::Http)?;
        let data_start = line_end + 2;
        if size == 0 {
            return Ok(Some(out));
        }
        // The size is the owner's: a chunk that cannot fit in a response
        // this client accepts is refused rather than waited for.
        let data_end = data_start
            .checked_add(size)
            .filter(|end| {
                consumed
                    .checked_add(*end)
                    .is_some_and(|end| end <= max_bytes)
            })
            .ok_or(DialError::Http)?;
        let rest_start = data_end.checked_add(2).ok_or(DialError::Http)?;
        let Some(data) = body.get(data_start..data_end) else {
            return Ok(None);
        };
        out.extend_from_slice(data);
        let Some(rest) = body.get(rest_start..) else {
            return Ok(None);
        };
        consumed += rest_start;
        body = rest;
    }
}

/// True once `body` holds a whole chunked body, scanning chunk headers from
/// `*next_chunk` (the offset of the first size line not yet passed, which
/// it advances) without copying chunk data. Fails as [`decode_chunked`]
/// does on a malformed size line or a chunk past `max_bytes`.
///
/// # Errors
///
/// [`DialError::Http`] on a malformed size line or a chunk that cannot fit
/// in `max_bytes`.
fn chunked_body_complete(
    body: &[u8],
    next_chunk: &mut usize,
    max_bytes: usize,
) -> Result<bool, DialError> {
    loop {
        let rest = body.get(*next_chunk..).ok_or(DialError::Http)?;
        let Some(line_end) = find(rest, b"\r\n") else {
            return Ok(false);
        };
        let size_text = std::str::from_utf8(&rest[..line_end]).map_err(|_| DialError::Http)?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| DialError::Http)?;
        if size == 0 {
            return Ok(true);
        }
        let data_end = (*next_chunk)
            .checked_add(line_end + 2)
            .and_then(|start| start.checked_add(size))
            .filter(|end| *end <= max_bytes)
            .ok_or(DialError::Http)?;
        let after = data_end.checked_add(2).ok_or(DialError::Http)?;
        if after > body.len() {
            return Ok(false);
        }
        *next_chunk = after;
    }
}

/// The hosts' one crypto provider (aws-lc-rs); see `bookclerk_tls`.
fn provider() -> Arc<CryptoProvider> {
    bookclerk_tls::crypto_provider()
}

/// TLS settings: the pinned verifier, or the webpki roots.
fn client_config(pin: Option<[u8; 32]>) -> Result<ClientConfig, DialError> {
    let provider = provider();
    let algorithms = provider.signature_verification_algorithms;
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| DialError::Tls(err.to_string()))?;
    let mut config = match pin {
        Some(pin) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedVerifier { pin, algorithms }))
            .with_no_client_auth(),
        None => builder
            .with_root_certificates(RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            })
            .with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Accepts exactly the certificate whose public key hashes to the pin, and
/// only when the handshake is signed with that key.
#[derive(Debug)]
struct PinnedVerifier {
    /// SHA-256 of the expected SubjectPublicKeyInfo.
    pin: [u8; 32],
    /// Signature algorithms of the crypto provider.
    algorithms: WebPkiSupportedAlgorithms,
}

/// The SPKI pin of a DER certificate.
fn certificate_pin(cert: &CertificateDer<'_>) -> Option<[u8; 32]> {
    let parsed = ParsedCertificate::try_from(cert).ok()?;
    Some(sha2::Sha256::digest(parsed.subject_public_key_info().as_ref()).into())
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        match certificate_pin(end_entity) {
            Some(pin) if pin == self.pin => Ok(ServerCertVerified::assertion()),
            Some(_) => {
                tracing::warn!("owner certificate does not match the enrollment SPKI pin");
                Err(tokio_rustls::rustls::Error::InvalidCertificate(
                    CertificateError::ApplicationVerificationFailure,
                ))
            }
            None => Err(tokio_rustls::rustls::Error::InvalidCertificate(
                CertificateError::BadEncoding,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_loopback_only_and_never_pinned() {
        let pin = "A".repeat(43);
        for ok in [
            "http://127.0.0.1:8787",
            "http://[::1]:8787",
            "http://localhost:8787",
            "https://owner.example",
            "https://192.168.1.5:8443",
        ] {
            assert!(OwnerEndpoint::new(ok, None).is_ok(), "{ok}");
        }
        assert!(OwnerEndpoint::new("https://owner.lan:8443", Some(&pin))
            .unwrap()
            .is_pinned());
        for refused in [
            "http://192.168.1.5:8787",
            "http://owner.example",
            "http://10.0.0.1",
        ] {
            assert!(
                matches!(
                    OwnerEndpoint::new(refused, None),
                    Err(DialError::Insecure(_))
                ),
                "{refused}"
            );
        }
        assert!(matches!(
            OwnerEndpoint::new("http://127.0.0.1:8787", Some(&pin)),
            Err(DialError::Insecure("pinned_link_over_plain_http"))
        ));
        assert!(matches!(
            OwnerEndpoint::new("ftp://owner.example", None),
            Err(DialError::Invalid(_))
        ));
        assert!(matches!(
            OwnerEndpoint::new("https://owner.example", Some("short")),
            Err(DialError::Invalid(_))
        ));
        let endpoint = OwnerEndpoint::new("HTTPS://Owner.Example:443/enroll", None).unwrap();
        assert_eq!(endpoint.origin(), "https://owner.example");
        assert_eq!(endpoint.authority(), "owner.example");
    }

    /// A source that never yields a byte.
    struct Stalled;

    impl AsyncRead for Stalled {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    /// An owner that accepts one connection and then neither reads nor
    /// answers; the connection is held until the test ends.
    async fn silent_owner() -> (OwnerEndpoint, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let endpoint = OwnerEndpoint::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        (endpoint, held)
    }

    /// A library that answers each connection with the next of `answers`
    /// (written as given, then closed) and hands back each request head.
    async fn scripted_library(
        answers: Vec<Vec<u8>>,
    ) -> (OwnerEndpoint, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = tokio::spawn(async move {
            let mut requests = Vec::new();
            for answer in answers {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                requests.push(String::from_utf8(request).unwrap());
                // The reader may hang up once it refuses the head, so a
                // failed write is not the test failing.
                let _ = socket.write_all(&answer).await;
                let _ = socket.shutdown().await;
            }
            requests
        });
        let endpoint = OwnerEndpoint::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        (endpoint, served)
    }

    /// A `200` answer carrying `body`.
    fn whole(body: &[u8]) -> Vec<u8> {
        let mut out =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// A `206` answer carrying `file` from `from` on.
    fn rest(file: &[u8], from: usize) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{}\r\n\
             Content-Length: {}\r\n\r\n",
            file.len() - 1,
            file.len(),
            file.len() - from
        )
        .into_bytes();
        out.extend_from_slice(&file[from..]);
        out
    }

    /// The head and the first file bytes are told apart the same way
    /// wherever the library's writes fall, the blank line cut in two
    /// included, and a destination that cannot be written is a local
    /// failure, not one to ask the library again about.
    #[tokio::test]
    async fn a_copy_answer_cut_anywhere_gives_the_same_file() {
        const COPY: &str = "0b7f6c7e-3d1a-4a52-9c1e-6f2f0f0c9a11";
        let content = b"\r\n\r\nfile bytes that look like a blank line\r\n\r\n".to_vec();
        let size = content.len() as u64;
        let answer = whole(&content);
        let head = answer.len() - content.len();
        let dir = tempfile::tempdir().unwrap();
        for cut in [1, head - 3, head - 2, head - 1, head, head + 1, head + 5] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let pieces = [answer[..cut].to_vec(), answer[cut..].to_vec()];
            let served = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                    request.push(byte[0]);
                }
                for piece in pieces {
                    socket.write_all(&piece).await.unwrap();
                    socket.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
            });
            let endpoint = OwnerEndpoint::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
            let dest = dir.path().join(format!("cut-{cut}"));
            crate::fetch_copy_file(&endpoint, "Bookclerk-Copy secret", COPY, 0, size, &dest)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), content, "cut at {cut}");
            served.await.unwrap();
        }

        // A file that cannot be written.
        let dest = dir.path().join("read-only");
        std::fs::write(&dest, b"old").unwrap();
        let mut file = tokio::fs::File::open(&dest).await.unwrap();
        let (endpoint, _served) = scripted_library(vec![answer]).await;
        let path = format!("/api/copies/v1/{COPY}/files/0");
        let out = endpoint
            .get_copy_stream(&path, "Bookclerk-Copy secret", 0, size, &mut file)
            .await;
        assert!(matches!(out, Err(CopyStreamError::Local(_))), "{out:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    }

    /// A copy's file comes whole, resumes where a cut download stopped,
    /// is replaced when the library ignores the range, and is never taken
    /// from an answer that states other bytes than the ones asked for.
    #[tokio::test]
    async fn a_copy_file_is_fetched_whole_resumed_or_refused() {
        const COPY: &str = "0b7f6c7e-3d1a-4a52-9c1e-6f2f0f0c9a11";
        let content: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
        let size = content.len() as u64;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.part");
        let fetch = |endpoint: OwnerEndpoint, dest: std::path::PathBuf| async move {
            crate::fetch_copy_file(&endpoint, "Bookclerk-Copy secret", COPY, 3, size, &dest).await
        };

        // Whole, with bytes past the stated length left alone.
        let mut answer = whole(&content);
        answer.extend_from_slice(b"extra");
        let (endpoint, served) = scripted_library(vec![answer]).await;
        fetch(endpoint, dest.clone()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        let requests = served.await.unwrap();
        assert!(
            requests[0].starts_with(&format!("GET /api/copies/v1/{COPY}/files/3 HTTP/1.1\r\n")),
            "{requests:?}"
        );
        assert!(requests[0].contains("Authorization: Bookclerk-Copy secret\r\n"));
        assert!(!requests[0].contains("Range:"), "{requests:?}");

        // A complete file asks nothing.
        let (endpoint, served) = scripted_library(Vec::new()).await;
        fetch(endpoint, dest.clone()).await.unwrap();
        assert!(served.await.unwrap().is_empty());

        // An answer cut short keeps what arrived; the next call resumes.
        std::fs::remove_file(&dest).unwrap();
        let cut = whole(&content)[..70_000].to_vec();
        let (endpoint, served) = scripted_library(vec![cut]).await;
        let out = fetch(endpoint, dest.clone()).await;
        assert!(
            matches!(out, Err(crate::CopyFetchError::Transfer(DialError::Http))),
            "{out:?}"
        );
        served.await.unwrap();
        let held = std::fs::read(&dest).unwrap();
        assert!(!held.is_empty() && held.len() < content.len());
        assert_eq!(held, content[..held.len()]);
        let (endpoint, served) = scripted_library(vec![rest(&content, held.len())]).await;
        fetch(endpoint, dest.clone()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        let requests = served.await.unwrap();
        assert!(
            requests[0].contains(&format!("Range: bytes={}-\r\n", held.len())),
            "{requests:?}"
        );

        // A library that ignores the range replaces what was there.
        std::fs::write(&dest, b"stale start").unwrap();
        let (endpoint, served) = scripted_library(vec![whole(&content)]).await;
        fetch(endpoint, dest.clone()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        served.await.unwrap();

        // A range the library no longer takes starts over, once.
        std::fs::write(&dest, &content[..10]).unwrap();
        let unsatisfiable =
            b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (endpoint, served) = scripted_library(vec![unsatisfiable, whole(&content)]).await;
        fetch(endpoint, dest.clone()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        assert_eq!(served.await.unwrap().len(), 2);

        // A longer local file is replaced.
        let mut longer = content.clone();
        longer.extend_from_slice(b"tail");
        std::fs::write(&dest, &longer).unwrap();
        let (endpoint, served) = scripted_library(vec![whole(&content)]).await;
        fetch(endpoint, dest.clone()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        served.await.unwrap();

        // Answers that state other bytes leave the file as it was.
        for bad in [
            whole(&content[..100]),
            rest(&content, 4),
            rest(&content[..1000], 10),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
            b"garbage\r\n\r\n".to_vec(),
        ] {
            std::fs::write(&dest, &content[..10]).unwrap();
            let (endpoint, served) = scripted_library(vec![bad]).await;
            let out = fetch(endpoint, dest.clone()).await;
            assert!(
                matches!(out, Err(crate::CopyFetchError::Transfer(DialError::Http))),
                "{out:?}"
            );
            assert_eq!(std::fs::read(&dest).unwrap(), content[..10]);
            served.await.unwrap();
        }

        // A refusal is the status, with the file as it was.
        let refusal = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 24\r\n\r\n\
                        {\"error\":\"unauthorized\"}"
            .to_vec();
        let (endpoint, served) = scripted_library(vec![refusal]).await;
        let out = fetch(endpoint, dest.clone()).await;
        assert!(
            matches!(out, Err(crate::CopyFetchError::Status(401))),
            "{out:?}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), content[..10]);
        served.await.unwrap();

        // A name that is not a copy id asks nothing.
        let (endpoint, served) = scripted_library(Vec::new()).await;
        let out =
            crate::fetch_copy_file(&endpoint, "Bookclerk-Copy secret", "../x", 0, size, &dest)
                .await;
        assert!(
            matches!(out, Err(crate::CopyFetchError::InvalidCopy)),
            "{out:?}"
        );
        served.await.unwrap();
    }

    /// A library that stops sending fails the download at the idle
    /// timeout, and what arrived is kept.
    #[tokio::test]
    async fn a_copy_download_that_stalls_times_out() {
        const COPY: &str = "0b7f6c7e-3d1a-4a52-9c1e-6f2f0f0c9a11";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let endpoint = OwnerEndpoint::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.part");
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            crate::fetch_copy_file(&endpoint, "Bookclerk-Copy secret", COPY, 0, 10, &dest),
        )
        .await
        .expect("the stalled download is abandoned");
        assert!(
            matches!(
                out,
                Err(crate::CopyFetchError::Transfer(DialError::Timeout))
            ),
            "{out:?}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        held.abort();
    }

    /// A body source that stalls fails the upload at the idle timeout
    /// instead of holding it open forever.
    #[tokio::test]
    async fn an_upload_whose_body_stalls_times_out() {
        let (endpoint, held) = silent_owner().await;
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            endpoint.put_stream("/upload", "Bookclerk-Upload t", 10, Stalled),
        )
        .await
        .expect("the stalled read is abandoned");
        assert!(matches!(out, Err(DialError::Timeout)), "{out:?}");
        held.abort();
    }

    /// An owner that stops reading fails the upload at the idle timeout
    /// once the socket buffers are full.
    #[tokio::test]
    async fn an_upload_the_owner_stops_reading_times_out() {
        let (endpoint, held) = silent_owner().await;
        let length = 256u64 << 20;
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            endpoint.put_stream(
                "/upload",
                "Bookclerk-Upload t",
                length,
                tokio::io::repeat(7).take(length),
            ),
        )
        .await
        .expect("the stalled write is abandoned");
        assert!(matches!(out, Err(DialError::Timeout)), "{out:?}");
        held.abort();
    }

    /// A body shorter than its declared length is refused after the copy.
    #[tokio::test]
    async fn an_upload_body_that_ends_early_is_refused() {
        let (endpoint, held) = silent_owner().await;
        let out = endpoint
            .put_stream("/upload", "Bookclerk-Upload t", 10, &b"abc"[..])
            .await;
        assert!(
            matches!(&out, Err(DialError::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof),
            "{out:?}"
        );
        held.abort();
    }

    /// Accepts one upload request per answer, one after another, replying
    /// each `(status line, JSON body)` in turn; hands back each request's
    /// `Content-Range` (if any) and body.
    async fn recording_owner(
        answers: Vec<(&'static str, String)>,
    ) -> (
        OwnerEndpoint,
        tokio::task::JoinHandle<Vec<(Option<String>, Vec<u8>)>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, answer) in answers {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0u8; 1];
                    socket.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let header = |name: &str| {
                    head.lines().find_map(|line| {
                        line.split_once(": ")
                            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                            .map(|(_, value)| value.to_string())
                    })
                };
                let length: usize = header("content-length").unwrap().parse().unwrap();
                let mut body = vec![0u8; length];
                socket.read_exact(&mut body).await.unwrap();
                seen.push((header("content-range"), body));
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{answer}",
                    answer.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        let endpoint = OwnerEndpoint::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        (endpoint, served)
    }

    /// `200 partial` naming `offset`.
    fn partial(offset: u64) -> (&'static str, String) {
        (
            "200 OK",
            format!(r#"{{"status":"partial","offset":{offset},"job_completed":false}}"#),
        )
    }

    /// `503 unavailable`, reason `resume`, naming `offset`.
    fn resume(offset: u64) -> (&'static str, String) {
        (
            "503 Service Unavailable",
            format!(
                r#"{{"status":"unavailable","reason":"resume","offset":{offset},"job_completed":false}}"#
            ),
        )
    }

    const RECEIVED: &str = r#"{"status":"received","job_completed":false}"#;

    /// `200 received`.
    fn received() -> (&'static str, String) {
        ("200 OK", RECEIVED.to_string())
    }

    /// An in-memory file as an [`UploadSource`].
    struct Memory(Vec<u8>);

    impl UploadSource for Memory {
        async fn open_at(&self, offset: u64) -> std::io::Result<Pin<Box<dyn AsyncRead + Send>>> {
            let start = usize::try_from(offset).unwrap().min(self.0.len());
            Ok(Box::pin(std::io::Cursor::new(self.0[start..].to_vec())))
        }
    }

    /// The `Content-Range` of each recorded request.
    fn ranges(seen: &[(Option<String>, Vec<u8>)]) -> Vec<Option<&str>> {
        seen.iter().map(|(range, _)| range.as_deref()).collect()
    }

    /// The bodies of recorded requests, joined in order.
    fn joined(seen: &[(Option<String>, Vec<u8>)]) -> Vec<u8> {
        seen.iter().flat_map(|(_, body)| body.clone()).collect()
    }

    /// A file larger than a part asks where the owner is, then goes as
    /// consecutive ranged requests that add up to the file; one that fits
    /// goes whole, with no range.
    #[tokio::test]
    async fn a_large_upload_goes_in_parts() {
        let file: Vec<u8> = (0..25u8).collect();
        let (endpoint, served) =
            recording_owner(vec![partial(0), partial(10), partial(20), received()]).await;
        let (status, answer) = endpoint
            .put_file(
                "/upload",
                "Bookclerk-Upload t",
                25,
                10,
                &Memory(file.clone()),
            )
            .await
            .unwrap();
        assert_eq!((status, answer), (200, RECEIVED.as_bytes().to_vec()));
        let seen = served.await.unwrap();
        assert_eq!(
            ranges(&seen),
            vec![
                Some("bytes */25"),
                Some("bytes 0-9/25"),
                Some("bytes 10-19/25"),
                Some("bytes 20-24/25"),
            ]
        );
        assert_eq!(joined(&seen), file);

        let (endpoint, served) = recording_owner(vec![received()]).await;
        endpoint
            .put_file(
                "/upload",
                "Bookclerk-Upload t",
                10,
                10,
                &Memory(file[..10].to_vec()),
            )
            .await
            .unwrap();
        let seen = served.await.unwrap();
        assert_eq!(ranges(&seen), vec![None]);
        assert_eq!(seen[0].1, file[..10]);
    }

    /// An owner that already holds the whole file (an earlier attempt
    /// finished) says so to the status query, and nothing is sent.
    #[tokio::test]
    async fn an_upload_stops_once_the_owner_holds_the_file() {
        let (endpoint, served) = recording_owner(vec![received()]).await;
        let (status, answer) = endpoint
            .put_file(
                "/upload",
                "Bookclerk-Upload t",
                25,
                10,
                &Memory(vec![7; 25]),
            )
            .await
            .unwrap();
        assert_eq!((status, answer), (200, RECEIVED.as_bytes().to_vec()));
        assert_eq!(served.await.unwrap().len(), 1);
    }

    /// An upload resumes where the owner stands: from the status query's
    /// offset, and from a `resume` answer's offset mid-file (not on a part
    /// boundary), sending only the bytes the owner lacks.
    #[tokio::test]
    async fn an_upload_resumes_from_the_owners_offset() {
        let file: Vec<u8> = (0..25u8).collect();
        let (endpoint, served) = recording_owner(vec![
            partial(5),
            partial(15),
            resume(12),
            partial(22),
            received(),
        ])
        .await;
        let (status, _) = endpoint
            .put_file(
                "/upload",
                "Bookclerk-Upload t",
                25,
                10,
                &Memory(file.clone()),
            )
            .await
            .unwrap();
        assert_eq!(status, 200);
        let seen = served.await.unwrap();
        assert_eq!(
            ranges(&seen),
            vec![
                Some("bytes */25"),
                Some("bytes 5-14/25"),
                Some("bytes 15-24/25"),
                Some("bytes 12-21/25"),
                Some("bytes 22-24/25"),
            ]
        );
        assert_eq!(seen[1].1, file[5..15]);
        assert_eq!(seen[2].1, file[15..]);
        assert_eq!(seen[3].1, file[12..22]);
        assert_eq!(seen[4].1, file[22..]);
    }

    #[test]
    fn chunked_bodies_decode() {
        assert_eq!(
            decode_chunked(
                b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n",
                MAX_HTTP_RESPONSE_BYTES
            )
            .unwrap(),
            Some(b"Wikipedia".to_vec())
        );
        assert_eq!(
            decode_chunked(b"4\r\nWi", MAX_HTTP_RESPONSE_BYTES).unwrap(),
            None
        );
        assert!(decode_chunked(b"zz\r\n", MAX_HTTP_RESPONSE_BYTES).is_err());
        // Scanned in pieces, the cursor only moves past whole chunks.
        let whole = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut next = 0;
        assert!(!chunked_body_complete(&whole[..6], &mut next, MAX_HTTP_RESPONSE_BYTES).unwrap());
        assert_eq!(next, 0);
        assert!(!chunked_body_complete(&whole[..12], &mut next, MAX_HTTP_RESPONSE_BYTES).unwrap());
        assert_eq!(next, 9);
        assert!(chunked_body_complete(whole, &mut next, MAX_HTTP_RESPONSE_BYTES).unwrap());
        assert!(chunked_body_complete(b"zz\r\n", &mut 0, MAX_HTTP_RESPONSE_BYTES).is_err());
        assert!(chunked_body_complete(b"ffffffffffffffff\r\nab", &mut 0, usize::MAX).is_err());
        // An owner-chosen size that overflows, or that no accepted
        // response could hold, is refused instead of waited for.
        assert!(matches!(
            decode_chunked(b"ffffffffffffffff\r\nab", usize::MAX),
            Err(DialError::Http)
        ));
        let too_big = format!("{:x}\r\nab", MAX_HTTP_RESPONSE_BYTES);
        assert!(matches!(
            decode_chunked(too_big.as_bytes(), MAX_HTTP_RESPONSE_BYTES),
            Err(DialError::Http)
        ));
    }

    /// A chunked artifact larger than the fixed frame cap decodes under the
    /// caller's larger cap (a proxy may re-encode a `Content-Length` body
    /// as chunked), and is still refused past that cap.
    #[test]
    fn a_chunked_body_is_held_to_the_callers_cap() {
        let payload = vec![b'x'; MAX_HTTP_RESPONSE_BYTES * 2];
        let mut body = format!("{:x}\r\n", payload.len()).into_bytes();
        body.extend_from_slice(&payload);
        body.extend_from_slice(b"\r\n0\r\n\r\n");
        let cap = payload.len() + MAX_HTTP_RESPONSE_BYTES;
        assert_eq!(decode_chunked(&body, cap).unwrap(), Some(payload.clone()));
        assert!(matches!(
            decode_chunked(&body, MAX_HTTP_RESPONSE_BYTES),
            Err(DialError::Http)
        ));
    }
}
