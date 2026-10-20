//! Reaching the owner: the transport policy, TLS with the SPKI pin, the
//! socket upgrade, and the one HTTP request enrollment makes.
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
use std::sync::Arc;
use std::time::Duration;

use bookclerk_node_proto::{canonical_origin, MAX_FRAME_BYTES, SUBPROTOCOL};
use sha2::Digest as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    aws_lc_rs, verify_tls12_signature, verify_tls13_signature, CryptoProvider,
    WebPkiSupportedAlgorithms,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::server::ParsedCertificate;
use tokio_rustls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::WebSocketStream;

/// Path of the node socket.
pub const CONNECT_PATH: &str = "/api/node/v1/connect";

/// Path of the enroll endpoint.
pub const ENROLL_PATH: &str = "/api/node/v1/enroll";

/// Seconds allowed for the TCP connect and for the TLS handshake.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Seconds allowed for an HTTP exchange (enrollment).
const HTTP_TIMEOUT_SECS: u64 = 30;

/// Largest HTTP answer read (enrollment answers are tiny).
const MAX_HTTP_RESPONSE_BYTES: usize = 64 * 1024;

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
        let io = self.connect().await?;
        let scheme = if self.tls { "wss" } else { "ws" };
        let mut request = format!("{scheme}://{}{CONNECT_PATH}", self.authority())
            .into_client_request()
            .map_err(|err| DialError::Invalid(err.to_string()))?;
        request.headers_mut().insert(
            "sec-websocket-protocol",
            tokio_tungstenite::tungstenite::http::HeaderValue::from_static(SUBPROTOCOL),
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
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
        if selected != Some(SUBPROTOCOL) {
            return Err(DialError::Handshake(
                "owner did not select the node subprotocol".into(),
            ));
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
            read_http_response(&mut io).await
        };
        tokio::time::timeout(Duration::from_secs(HTTP_TIMEOUT_SECS), exchange)
            .await
            .map_err(|_| DialError::Timeout)?
    }
}

/// Reads one HTTP/1.1 answer: the status and the body (by
/// `Content-Length`, chunked, or until close).
async fn read_http_response(io: &mut Box<dyn OwnerIo>) -> Result<(u16, Vec<u8>), DialError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut eof = false;
    let header_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at;
        }
        if eof || buf.len() > MAX_HTTP_RESPONSE_BYTES {
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
    loop {
        let body = &buf[body_start..];
        let done = if chunked {
            decode_chunked(body)?.is_some()
        } else if let Some(length) = length {
            body.len() >= length
        } else {
            eof
        };
        if done || eof {
            break;
        }
        if buf.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(DialError::Http);
        }
        eof = read_some(io, &mut buf, &mut chunk).await?;
    }
    let body = &buf[body_start..];
    let body = if chunked {
        decode_chunked(body)?.ok_or(DialError::Http)?
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
fn decode_chunked(mut body: &[u8]) -> Result<Option<Vec<u8>>, DialError> {
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
                    .is_some_and(|end| end <= MAX_HTTP_RESPONSE_BYTES)
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

/// The process crypto provider (aws-lc, like the rest of the tree).
fn provider() -> Arc<CryptoProvider> {
    Arc::new(aws_lc_rs::default_provider())
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

    #[test]
    fn chunked_bodies_decode() {
        assert_eq!(
            decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(),
            Some(b"Wikipedia".to_vec())
        );
        assert_eq!(decode_chunked(b"4\r\nWi").unwrap(), None);
        assert!(decode_chunked(b"zz\r\n").is_err());
        // An owner-chosen size that overflows, or that no accepted
        // response could hold, is refused instead of waited for.
        assert!(matches!(
            decode_chunked(b"ffffffffffffffff\r\nab"),
            Err(DialError::Http)
        ));
        let too_big = format!("{:x}\r\nab", MAX_HTTP_RESPONSE_BYTES);
        assert!(matches!(
            decode_chunked(too_big.as_bytes()),
            Err(DialError::Http)
        ));
    }
}
