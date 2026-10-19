//! Transport policy for the node API (H-4 design section 4, as amended by
//! decision 1: TLS everywhere except loopback).
//!
//! [`classify_transport`] is a pure function of the connection, the request
//! headers, and the bootstrap `[daemon.node_api]` settings. It runs before
//! every enroll (and, later, every node socket upgrade and upload). A
//! connection is allowed in exactly one mode:
//!
//! - `acme`: it arrived on the built-in rustls listener with an ACME
//!   certificate (`[daemon.node_api] mode = "acme"`).
//! - `self_signed`: it arrived on the built-in rustls listener with a pinned
//!   self-signed certificate (`mode = "self_signed"`).
//! - `proxy`: plain HTTP from a peer in `daemon.trusted_proxies`, carrying
//!   exactly one `X-Forwarded-Proto: https`, with an https
//!   `integrations.public_origin` (`mode = "proxy"`).
//! - `loopback`: both ends are loopback and no forwarding header is present.
//!   Allowed in every mode: the traffic never leaves the machine. In proxy
//!   mode a trusted proxy's connection is always checked as proxied, so a
//!   same-host proxy cannot relay a request as loopback.
//!
//! Everything else is refused with 403 `transport_insecure`. Forwarding
//! headers from a peer that is not a trusted proxy are never believed, and
//! plain HTTP from any non-loopback peer (a private RFC 1918 address
//! included) is refused.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::task::{Context, Poll};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::Request;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::serve::IncomingStream;
use axum::{Json, Router};
use bookclerk_config::{NodeApiConfig, NodeApiMode};
use serde_json::json;
use tokio::net::TcpListener;
use tower_service::Service;

/// Which listener accepted the connection. The built-in TLS listener
/// ([`super::tls`]) attaches its kind as a request extension; a request
/// without one came through a plain `daemon.listen` listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerKind {
    /// The plain-HTTP `daemon.listen` listener.
    Plain,
    /// The built-in rustls listener serving an ACME certificate.
    AcmeTls,
    /// The built-in rustls listener serving the pinned self-signed
    /// certificate.
    SelfSignedTls,
}

/// The local address of the connection a request arrived on, attached as a
/// request extension by [`ConnAddrsMakeService`].
///
/// This is the accepted socket's own address, not the listener's bind
/// address: on a wildcard bind (`0.0.0.0` / `[::]`) a same-machine client
/// still shows a loopback local address here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalAddr(pub SocketAddr);

/// Make-service for `axum::serve` that records both ends of each accepted
/// TCP connection: the peer as `ConnectInfo<SocketAddr>` (as
/// `into_make_service_with_connect_info` would) and the socket's local
/// address as [`LocalAddr`].
#[derive(Clone)]
pub struct ConnAddrsMakeService {
    /// Router served on every connection.
    app: Router,
}

impl ConnAddrsMakeService {
    /// Wraps `app` so every connection's addresses reach its handlers.
    ///
    /// # Arguments
    ///
    /// * `app` - The router to serve.
    ///
    /// # Returns
    ///
    /// The make-service to hand to `axum::serve`.
    #[must_use]
    pub fn new(app: Router) -> Self {
        Self { app }
    }
}

impl Service<IncomingStream<'_, TcpListener>> for ConnAddrsMakeService {
    type Response = ConnAddrsService;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, stream: IncomingStream<'_, TcpListener>) -> Self::Future {
        std::future::ready(Ok(ConnAddrsService {
            app: self.app.clone(),
            peer: *stream.remote_addr(),
            // The accepted socket's address, never the wildcard bind.
            local: stream.io().local_addr().ok(),
        }))
    }
}

/// Per-connection service built by [`ConnAddrsMakeService`].
#[derive(Clone)]
pub struct ConnAddrsService {
    /// Router serving the request.
    app: Router,
    /// Remote peer of the connection.
    peer: SocketAddr,
    /// Local address of the connection, when the socket reported one.
    local: Option<SocketAddr>,
}

impl Service<Request> for ConnAddrsService {
    type Response = Response;
    type Error = Infallible;
    type Future = <Router as Service<Request>>::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Service::<Request>::poll_ready(&mut self.app, cx)
    }

    fn call(&mut self, mut request: Request) -> Self::Future {
        let extensions = request.extensions_mut();
        extensions.insert(ConnectInfo(self.peer));
        if let Some(local) = self.local {
            extensions.insert(LocalAddr(local));
        }
        Service::<Request>::call(&mut self.app, request)
    }
}

/// What is known about one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnInfo {
    /// Remote peer, when the server recorded it.
    pub peer: Option<SocketAddr>,
    /// Local address of the connection, when the server recorded it.
    pub local: Option<SocketAddr>,
    /// Listener that accepted the connection.
    pub listener: ListenerKind,
}

/// Settings [`classify_transport`] reads.
#[derive(Debug, Clone, Copy)]
pub struct TransportConfig<'a> {
    /// `[daemon.node_api]`.
    pub node_api: &'a NodeApiConfig,
    /// `daemon.trusted_proxies`.
    pub trusted_proxies: &'a [String],
    /// `integrations.public_origin`.
    pub public_origin: Option<&'a str>,
}

impl<'a> TransportConfig<'a> {
    /// The policy inputs of a loaded [`bookclerk_config::Config`].
    #[must_use]
    pub fn from_config(cfg: &'a bookclerk_config::Config) -> Self {
        Self {
            node_api: &cfg.daemon.node_api,
            trusted_proxies: &cfg.daemon.trusted_proxies,
            public_origin: cfg.integrations.public_origin.as_deref(),
        }
    }
}

/// Mode a connection was allowed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedMode {
    /// Built-in listener, ACME certificate.
    Acme,
    /// Trusted TLS-terminating proxy.
    Proxy,
    /// Built-in listener, pinned self-signed certificate.
    SelfSigned,
    /// Same machine, no proxy.
    Loopback,
}

/// Why a connection was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// Plain HTTP from a peer that is not loopback and not a trusted proxy.
    PlainHttp,
    /// The server did not record the peer or local address.
    UnknownPeer,
    /// Forwarding headers from a loopback peer that is not a trusted proxy.
    UntrustedForwarding,
    /// A trusted proxy sent no `X-Forwarded-Proto`.
    ProxyProtoMissing,
    /// A trusted proxy sent more than one forwarded protocol, or the
    /// `Forwarded` header disagrees.
    ProxyProtoAmbiguous,
    /// A trusted proxy forwarded plain HTTP.
    ProxyProtoNotHttps,
    /// Proxy mode needs an https `integrations.public_origin`.
    PublicOriginNotHttps,
    /// A TLS listener accepted the connection under a different mode.
    ListenerModeMismatch,
    /// `[daemon.node_api]` is incomplete for its mode.
    Misconfigured,
}

impl RefusalReason {
    /// Stable code for the response body and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlainHttp => "plain_http",
            Self::UnknownPeer => "unknown_peer",
            Self::UntrustedForwarding => "untrusted_forwarding",
            Self::ProxyProtoMissing => "proxy_proto_missing",
            Self::ProxyProtoAmbiguous => "proxy_proto_ambiguous",
            Self::ProxyProtoNotHttps => "proxy_proto_not_https",
            Self::PublicOriginNotHttps => "public_origin_not_https",
            Self::ListenerModeMismatch => "listener_mode_mismatch",
            Self::Misconfigured => "node_api_misconfigured",
        }
    }
}

/// Outcome of [`classify_transport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportDecision {
    /// Serve the request.
    Allowed(AllowedMode),
    /// Answer 403 `transport_insecure`.
    Refused(RefusalReason),
}

/// Classifies one node-API connection. Pure: no I/O, no clock.
#[must_use]
pub fn classify_transport(
    conn: &ConnInfo,
    headers: &HeaderMap,
    cfg: &TransportConfig<'_>,
) -> TransportDecision {
    use TransportDecision::{Allowed, Refused};
    let mode = cfg.node_api.mode;
    if cfg
        .node_api
        .validate(cfg.trusted_proxies, cfg.public_origin)
        .is_err()
    {
        return Refused(RefusalReason::Misconfigured);
    }
    match conn.listener {
        ListenerKind::AcmeTls => {
            return if mode == NodeApiMode::Acme {
                Allowed(AllowedMode::Acme)
            } else {
                Refused(RefusalReason::ListenerModeMismatch)
            };
        }
        ListenerKind::SelfSignedTls => {
            return if mode == NodeApiMode::SelfSigned {
                Allowed(AllowedMode::SelfSigned)
            } else {
                Refused(RefusalReason::ListenerModeMismatch)
            };
        }
        ListenerKind::Plain => {}
    }
    let (Some(peer), Some(local)) = (conn.peer, conn.local) else {
        return Refused(RefusalReason::UnknownPeer);
    };
    let forwarded = has_forwarding_headers(headers);
    let peer_ip = peer.ip().to_canonical();
    // In proxy mode a trusted proxy's connection is always treated as
    // proxied, even without forwarding headers: a same-host proxy connects
    // from loopback, and a request it relays without headers must not pass
    // as local traffic.
    if mode == NodeApiMode::Proxy && crate::auth::peer_is_trusted(peer_ip, cfg.trusted_proxies) {
        return classify_proxied(headers, cfg.public_origin);
    }
    if is_loopback(peer_ip) && is_loopback(local.ip().to_canonical()) {
        return if forwarded {
            Refused(RefusalReason::UntrustedForwarding)
        } else {
            Allowed(AllowedMode::Loopback)
        };
    }
    // Forwarding headers from anyone else are ignored: this is plain HTTP.
    Refused(RefusalReason::PlainHttp)
}

/// Checks what a trusted proxy forwarded.
fn classify_proxied(headers: &HeaderMap, public_origin: Option<&str>) -> TransportDecision {
    use TransportDecision::{Allowed, Refused};
    let mut protos = Vec::new();
    for value in headers.get_all("x-forwarded-proto") {
        let Ok(value) = value.to_str() else {
            return Refused(RefusalReason::ProxyProtoAmbiguous);
        };
        protos.extend(value.split(',').map(|proto| proto.trim().to_string()));
    }
    match protos.as_slice() {
        [] => return Refused(RefusalReason::ProxyProtoMissing),
        [only] if only.eq_ignore_ascii_case("https") => {}
        [_] => return Refused(RefusalReason::ProxyProtoNotHttps),
        _ => return Refused(RefusalReason::ProxyProtoAmbiguous),
    }
    // An RFC 7239 `Forwarded` header must not claim another protocol.
    for value in headers.get_all(header::FORWARDED) {
        let Ok(value) = value.to_str() else {
            return Refused(RefusalReason::ProxyProtoAmbiguous);
        };
        for pair in value.split([',', ';']) {
            if let Some((name, proto)) = pair.trim().split_once('=') {
                if name.trim().eq_ignore_ascii_case("proto")
                    && !proto.trim().trim_matches('"').eq_ignore_ascii_case("https")
                {
                    return Refused(RefusalReason::ProxyProtoAmbiguous);
                }
            }
        }
    }
    if !origin_is_https(public_origin) {
        return Refused(RefusalReason::PublicOriginNotHttps);
    }
    Allowed(AllowedMode::Proxy)
}

/// True when `origin` is an absolute https URL.
pub(crate) fn origin_is_https(origin: Option<&str>) -> bool {
    origin
        .map(str::trim)
        .and_then(|origin| url::Url::parse(origin).ok())
        .is_some_and(|url| url.scheme() == "https" && url.host().is_some())
}

/// True for 127.0.0.0/8 and ::1.
fn is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

/// True when any reverse-proxy forwarding header is present.
fn has_forwarding_headers(headers: &HeaderMap) -> bool {
    headers.keys().any(|name| {
        let n = name.as_str();
        n == "forwarded" || n == "via" || n == "x-real-ip" || n.starts_with("x-forwarded-")
    })
}

/// The 403 answer for a refused connection.
#[must_use]
pub fn refused_response(reason: RefusalReason) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "transport_insecure",
            "reason": reason.as_str(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(raw: &str) -> SocketAddr {
        raw.parse().unwrap()
    }

    fn plain(peer: &str, local: &str) -> ConnInfo {
        ConnInfo {
            peer: Some(addr(peer)),
            local: Some(addr(local)),
            listener: ListenerKind::Plain,
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    fn node_api(mode: NodeApiMode) -> NodeApiConfig {
        NodeApiConfig {
            mode,
            acme_contact: Some("mailto:ops@example.com".into()),
            tls_listen: Some("0.0.0.0:8443".into()),
            ..NodeApiConfig::default()
        }
    }

    struct Case {
        name: &'static str,
        mode: NodeApiMode,
        conn: ConnInfo,
        headers: Vec<(&'static str, &'static str)>,
        trusted: Vec<&'static str>,
        origin: Option<&'static str>,
        want: TransportDecision,
    }

    #[test]
    fn classify_transport_table() {
        use AllowedMode as A;
        use NodeApiMode as M;
        use RefusalReason as R;
        use TransportDecision::{Allowed, Refused};
        let proxy = "10.0.0.2:40000";
        let public = "203.0.113.9:5555";
        let lan = "192.168.1.20:5555";
        let listen = "0.0.0.0:8787";
        let https = Some("https://owner.example");
        let cases = vec![
            Case {
                name: "loopback with no forwarding headers is allowed",
                mode: M::Loopback,
                conn: plain("127.0.0.1:5000", "127.0.0.1:8787"),
                headers: vec![],
                trusted: vec![],
                origin: None,
                want: Allowed(A::Loopback),
            },
            Case {
                name: "ipv6 loopback is allowed in proxy mode too",
                mode: M::Proxy,
                conn: plain("[::1]:5000", "[::1]:8787"),
                headers: vec![],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Allowed(A::Loopback),
            },
            Case {
                name: "loopback peer to a non-loopback listener is refused",
                mode: M::Loopback,
                conn: plain("127.0.0.1:5000", "192.168.1.5:8787"),
                headers: vec![],
                trusted: vec![],
                origin: None,
                want: Refused(R::PlainHttp),
            },
            Case {
                name: "loopback peer with spoofed forwarding headers is refused",
                mode: M::Loopback,
                conn: plain("127.0.0.1:5000", "127.0.0.1:8787"),
                headers: vec![("x-forwarded-proto", "https")],
                trusted: vec![],
                origin: https,
                want: Refused(R::UntrustedForwarding),
            },
            Case {
                name: "private rfc1918 peer over plain http is refused",
                mode: M::Loopback,
                conn: plain(lan, listen),
                headers: vec![],
                trusted: vec![],
                origin: None,
                want: Refused(R::PlainHttp),
            },
            Case {
                name: "private peer is refused in proxy mode when not trusted",
                mode: M::Proxy,
                conn: plain(lan, listen),
                headers: vec![],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::PlainHttp),
            },
            Case {
                name: "spoofed forwarded headers from an untrusted peer are ignored",
                mode: M::Proxy,
                conn: plain(public, listen),
                headers: vec![
                    ("x-forwarded-proto", "https"),
                    ("x-forwarded-for", "127.0.0.1"),
                    ("forwarded", "for=127.0.0.1;proto=https"),
                ],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::PlainHttp),
            },
            Case {
                name: "trusted proxy forwarding https is allowed",
                mode: M::Proxy,
                conn: plain(proxy, listen),
                headers: vec![("x-forwarded-proto", "https")],
                trusted: vec!["10.0.0.0/24"],
                origin: https,
                want: Allowed(A::Proxy),
            },
            Case {
                name: "trusted proxy with two forwarded-proto headers is refused",
                mode: M::Proxy,
                conn: plain(proxy, listen),
                headers: vec![
                    ("x-forwarded-proto", "https"),
                    ("x-forwarded-proto", "https"),
                ],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::ProxyProtoAmbiguous),
            },
            Case {
                name: "trusted proxy with a comma-joined forwarded-proto is refused",
                mode: M::Proxy,
                conn: plain(proxy, listen),
                headers: vec![("x-forwarded-proto", "https, http")],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::ProxyProtoAmbiguous),
            },
            Case {
                name: "trusted proxy forwarding http is refused",
                mode: M::Proxy,
                conn: plain(proxy, listen),
                headers: vec![("x-forwarded-proto", "http")],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::ProxyProtoNotHttps),
            },
            Case {
                name: "a trusted loopback proxy without forwarding headers is refused",
                mode: M::Proxy,
                conn: plain("127.0.0.1:5000", "127.0.0.1:8787"),
                headers: vec![],
                trusted: vec!["127.0.0.1"],
                origin: https,
                want: Refused(R::ProxyProtoMissing),
            },
            Case {
                name: "a trusted loopback proxy forwarding https is allowed",
                mode: M::Proxy,
                conn: plain("127.0.0.1:5000", "127.0.0.1:8787"),
                headers: vec![("x-forwarded-proto", "https")],
                trusted: vec!["127.0.0.1"],
                origin: https,
                want: Allowed(A::Proxy),
            },
            Case {
                name: "trusted proxy without forwarded-proto is refused",
                mode: M::Proxy,
                conn: plain(proxy, listen),
                headers: vec![("x-forwarded-for", "198.51.100.1")],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::ProxyProtoMissing),
            },
            Case {
                name: "a Forwarded header that disagrees is refused",
                mode: M::Proxy,
                conn: plain(proxy, listen),
                headers: vec![
                    ("x-forwarded-proto", "https"),
                    ("forwarded", "for=198.51.100.1;proto=http"),
                ],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::ProxyProtoAmbiguous),
            },
            Case {
                name: "proxy mode without an https public origin is misconfigured",
                mode: M::Proxy,
                conn: plain(proxy, listen),
                headers: vec![("x-forwarded-proto", "https")],
                trusted: vec!["10.0.0.2"],
                origin: Some("http://owner.example"),
                want: Refused(R::Misconfigured),
            },
            Case {
                name: "a trusted proxy peer is not trusted outside proxy mode",
                mode: M::Loopback,
                conn: plain(proxy, listen),
                headers: vec![("x-forwarded-proto", "https")],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::PlainHttp),
            },
            Case {
                name: "plain http in acme mode is refused",
                mode: M::Acme,
                conn: plain(public, listen),
                headers: vec![],
                trusted: vec![],
                origin: https,
                want: Refused(R::PlainHttp),
            },
            Case {
                name: "the acme listener is allowed in acme mode",
                mode: M::Acme,
                conn: ConnInfo {
                    peer: Some(addr(public)),
                    local: Some(addr("0.0.0.0:8443")),
                    listener: ListenerKind::AcmeTls,
                },
                headers: vec![],
                trusted: vec![],
                origin: https,
                want: Allowed(A::Acme),
            },
            Case {
                name: "the self-signed listener is allowed in self_signed mode",
                mode: M::SelfSigned,
                conn: ConnInfo {
                    peer: Some(addr(lan)),
                    local: Some(addr("0.0.0.0:8443")),
                    listener: ListenerKind::SelfSignedTls,
                },
                headers: vec![],
                trusted: vec![],
                origin: None,
                want: Allowed(A::SelfSigned),
            },
            Case {
                name: "a TLS listener under another mode is refused",
                mode: M::Proxy,
                conn: ConnInfo {
                    peer: Some(addr(lan)),
                    local: Some(addr("0.0.0.0:8443")),
                    listener: ListenerKind::SelfSignedTls,
                },
                headers: vec![],
                trusted: vec!["10.0.0.2"],
                origin: https,
                want: Refused(R::ListenerModeMismatch),
            },
            Case {
                name: "an unrecorded peer is refused",
                mode: M::Loopback,
                conn: ConnInfo {
                    peer: None,
                    local: Some(addr("127.0.0.1:8787")),
                    listener: ListenerKind::Plain,
                },
                headers: vec![],
                trusted: vec![],
                origin: None,
                want: Refused(R::UnknownPeer),
            },
        ];
        for case in cases {
            let api = node_api(case.mode);
            let trusted: Vec<String> = case.trusted.iter().map(|s| (*s).to_string()).collect();
            let cfg = TransportConfig {
                node_api: &api,
                trusted_proxies: &trusted,
                public_origin: case.origin,
            };
            let got = classify_transport(&case.conn, &headers(&case.headers), &cfg);
            assert_eq!(got, case.want, "{}", case.name);
        }
    }

    #[test]
    fn incomplete_modes_are_misconfigured() {
        let bare = |mode| NodeApiConfig {
            mode,
            ..NodeApiConfig::default()
        };
        let https = Some("https://owner.example");
        let none: Vec<String> = Vec::new();
        assert!(bare(NodeApiMode::Loopback).validate(&none, None).is_ok());
        assert!(bare(NodeApiMode::Acme).validate(&none, https).is_err());
        assert!(bare(NodeApiMode::SelfSigned).validate(&none, None).is_err());
        assert!(bare(NodeApiMode::Proxy).validate(&none, https).is_err());
        let conn = plain("127.0.0.1:5000", "127.0.0.1:8787");
        let api = bare(NodeApiMode::Acme);
        let cfg = TransportConfig {
            node_api: &api,
            trusted_proxies: &none,
            public_origin: https,
        };
        assert_eq!(
            classify_transport(&conn, &HeaderMap::new(), &cfg),
            TransportDecision::Refused(RefusalReason::Misconfigured)
        );
    }

    #[tokio::test]
    async fn wildcard_bind_records_the_connections_loopback_local_addr() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn echo(
            ConnectInfo(peer): ConnectInfo<SocketAddr>,
            axum::Extension(LocalAddr(local)): axum::Extension<LocalAddr>,
        ) -> String {
            let conn = ConnInfo {
                peer: Some(peer),
                local: Some(local),
                listener: ListenerKind::Plain,
            };
            let node_api = node_api(NodeApiMode::Loopback);
            let cfg = TransportConfig {
                node_api: &node_api,
                trusted_proxies: &[],
                public_origin: None,
            };
            format!(
                "{} {:?}",
                local.ip(),
                classify_transport(&conn, &HeaderMap::new(), &cfg)
            )
        }

        let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/", axum::routing::get(echo));
        let server = tokio::spawn(async move {
            axum::serve(listener, ConnAddrsMakeService::new(app))
                .await
                .unwrap();
        });

        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        server.abort();

        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.ends_with("127.0.0.1 Allowed(Loopback)"),
            "a localhost client on a wildcard bind is loopback: {response}"
        );
    }
}
