//! Native guest HTTP through the workerd socket proxy.
//!
//! When [`crate::SOCKET_PROXY_ENV`] is unset (crate tests, host-owned D1),
//! this module wraps [`reqwest::Client`] over ambient TCP. When the proxy
//! is set, every request is `CONNECT` + rustls over the Unix or Windows
//! named-pipe proxy — nested `NetPolicy::Deny` guests must not call
//! `socket(AF_INET)`. When [`crate::NESTED_NATIVE_JAIL_ENV`] is `1` without a
//! proxy, construction fails closed rather than falling back to ambient TCP.
//!
//! Fetch domain grants do **not** imply TCP. Native manifests must declare
//! `capabilities.network.tcp` (or rely on a host overlay) for each
//! `host:port` this client dials.

#![allow(clippy::missing_docs_in_private_items)]

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use cookie_store::CookieStore;
use futures::Stream;
use http_body::Body;
use http_body_util::{BodyExt, Full};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_rustls::client::TlsStream;
use url::Url;

use crate::error::{Result, SdkError};
use crate::net::{
    connect, nested_native_jail_requested, ConnectOptions, SecureTransport, SocketAddress,
    NESTED_NATIVE_JAIL_ENV, SOCKET_PROXY_ENV,
};

pub use http::header;
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

/// Errors from [`Client`] / [`RequestBuilder`].
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(String);

impl Error {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    /// True when the message looks like a timeout (retry hint).
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        self.0.to_ascii_lowercase().contains("timeout")
    }

    /// True when the message looks like a connect/DNS failure (retry hint).
    #[must_use]
    pub fn is_connect(&self) -> bool {
        let s = self.0.to_ascii_lowercase();
        s.contains("connect") || s.contains("dns") || s.contains("resolve")
    }

    /// True when the error is a transport-class failure worth retrying.
    #[must_use]
    pub fn is_request(&self) -> bool {
        self.is_timeout() || self.is_connect() || {
            let s = self.0.to_ascii_lowercase();
            s.contains("reset") || s.contains("broken pipe") || s.contains("connection")
        }
    }
}

impl From<SdkError> for Error {
    fn from(err: SdkError) -> Self {
        Self(err.to_string())
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Self(err.to_string())
    }
}

impl From<url::ParseError> for Error {
    fn from(err: url::ParseError) -> Self {
        Self(format!("url: {err}"))
    }
}

/// Result alias for guest HTTP.
pub type HttpResult<T> = std::result::Result<T, Error>;

/// Shared cookie jar (reqwest path + proxied `cookie_store`).
#[derive(Clone)]
pub struct CookieJar {
    reqwest: Arc<reqwest::cookie::Jar>,
    store: Arc<Mutex<CookieStore>>,
}

impl CookieJar {
    /// Empty jar shared across clients in one guest process.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            reqwest: Arc::new(reqwest::cookie::Jar::default()),
            store: Arc::new(Mutex::new(CookieStore::default())),
        })
    }

    /// Seeds a `Set-Cookie` line (Amazon login init cookies).
    pub fn add_cookie_str(&self, cookie: &str, url: &Url) {
        self.reqwest.add_cookie_str(cookie, url);
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = store.parse(cookie, url);
    }
}

impl Default for CookieJar {
    fn default() -> Self {
        Self {
            reqwest: Arc::new(reqwest::cookie::Jar::default()),
            store: Arc::new(Mutex::new(CookieStore::default())),
        }
    }
}

/// Redirect follow policy (reqwest-compatible names).
pub mod redirect {
    /// How many hops to follow after the initial request.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Policy {
        /// Do not follow `Location`.
        None,
        /// Follow at most `n` hops.
        Limited(usize),
        /// Follow at most 10 hops, but stop when the current URL's query
        /// contains `needle` (OAuth `authorization_code` capture — do not
        /// follow the post-maplanding hop that would drop the code).
        StopOnQueryContains(&'static str),
    }

    impl Policy {
        /// [`Policy::None`].
        #[must_use]
        pub fn none() -> Self {
            Self::None
        }

        /// [`Policy::Limited`].
        #[must_use]
        pub fn limited(max: usize) -> Self {
            Self::Limited(max)
        }
    }

    impl Default for Policy {
        fn default() -> Self {
            Self::Limited(10)
        }
    }
}

/// Builder for [`Client`].
#[derive(Clone)]
pub struct ClientBuilder {
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    user_agent: Option<String>,
    cookies: Option<Arc<CookieJar>>,
    cookie_store: bool,
    redirect: redirect::Policy,
    default_headers: HeaderMap,
    disable_compression: bool,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder {
    /// Empty builder (10-hop redirects, no cookies).
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout: None,
            connect_timeout: None,
            user_agent: None,
            cookies: None,
            cookie_store: false,
            redirect: redirect::Policy::default(),
            default_headers: HeaderMap::new(),
            disable_compression: false,
        }
    }

    /// Overall request timeout (connect + headers + body).
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// CONNECT / TCP timeout (ambient reqwest path).
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Alias for [`Self::timeout`] (audible-rs `read_timeout`).
    #[must_use]
    pub fn read_timeout(self, timeout: Duration) -> Self {
        self.timeout(timeout)
    }

    /// Default `User-Agent`.
    #[must_use]
    pub fn user_agent(mut self, value: impl Into<String>) -> Self {
        self.user_agent = Some(value.into());
        self
    }

    /// Enable reqwest / proxied cookie jars (separate stores; one process is
    /// either proxied or ambient).
    #[must_use]
    pub fn cookie_store(mut self, enable: bool) -> Self {
        self.cookie_store = enable;
        self
    }

    /// Shared jar for Magento-style login + follow-up clients.
    #[must_use]
    pub fn cookie_provider(mut self, jar: Arc<CookieJar>) -> Self {
        self.cookies = Some(jar);
        self
    }

    /// Redirect policy.
    #[must_use]
    pub fn redirect(mut self, policy: redirect::Policy) -> Self {
        self.redirect = policy;
        self
    }

    /// Default headers merged into every request (request wins on conflict).
    #[must_use]
    pub fn default_headers(mut self, headers: HeaderMap) -> Self {
        self.default_headers = headers;
        self
    }

    /// Do not advertise gzip/br/deflate (CloudFront CENC URLs 403 on those).
    #[must_use]
    pub fn no_gzip(mut self) -> Self {
        self.disable_compression = true;
        self
    }

    /// See [`Self::no_gzip`].
    #[must_use]
    pub fn no_brotli(self) -> Self {
        self.no_gzip()
    }

    /// See [`Self::no_gzip`].
    #[must_use]
    pub fn no_deflate(self) -> Self {
        self.no_gzip()
    }

    /// Builds the client.
    ///
    /// # Errors
    ///
    /// Returns when nested Deny is set without a socket proxy, when the
    /// ambient reqwest client cannot be constructed, or when the proxied
    /// rustls config fails.
    pub fn build(self) -> HttpResult<Client> {
        Client::from_builder(self)
    }
}

/// HTTP client: ambient reqwest, or CONNECT+rustls through the socket proxy.
#[derive(Clone)]
pub struct Client {
    inner: Inner,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.inner {
            Inner::Direct(_) => "direct",
            #[cfg(any(unix, windows))]
            Inner::Proxied(_) => "proxied",
        };
        f.debug_struct("Client").field("mode", &kind).finish()
    }
}

#[derive(Clone)]
enum Inner {
    Direct(reqwest::Client),
    #[cfg(any(unix, windows))]
    Proxied(Box<ProxiedClient>),
}

#[cfg(any(unix, windows))]
#[derive(Clone)]
struct ProxiedClient {
    hyper: hyper_util::client::legacy::Client<SocketProxyConnector, Full<Bytes>>,
    timeout: Option<Duration>,
    user_agent: Option<String>,
    cookies: Option<Arc<CookieJar>>,
    redirect: redirect::Policy,
    default_headers: HeaderMap,
}

/// Something [`Client::get`] / [`Client::request`] can target.
pub trait IntoUrlArg {
    /// Owned URL string.
    fn into_url_arg(self) -> String;
}

impl IntoUrlArg for String {
    fn into_url_arg(self) -> String {
        self
    }
}

impl IntoUrlArg for &String {
    fn into_url_arg(self) -> String {
        self.clone()
    }
}

impl IntoUrlArg for &str {
    fn into_url_arg(self) -> String {
        self.to_owned()
    }
}

impl IntoUrlArg for Url {
    fn into_url_arg(self) -> String {
        self.into()
    }
}

impl IntoUrlArg for &Url {
    fn into_url_arg(self) -> String {
        self.as_str().to_owned()
    }
}

impl Client {
    /// Default client (ambient or proxied from env).
    ///
    /// # Panics
    ///
    /// Panics when [`crate::SOCKET_PROXY_ENV`] or nested Deny is in force and
    /// the proxied client cannot be built. Does not fall back to ambient TCP.
    /// Ambient reqwest construction failure without those env vars also panics
    /// only if the last-resort `reqwest::Client::new` cannot be created.
    #[must_use]
    pub fn new() -> Self {
        match Self::builder().build() {
            Ok(client) => client,
            Err(err) if use_socket_proxy() || nested_native_jail_requested() => {
                panic!("HTTP client failed under nested Deny / SOCKET_PROXY: {err}");
            }
            Err(_) => Self {
                inner: Inner::Direct(reqwest::Client::new()),
            },
        }
    }

    /// Builder.
    #[must_use]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// # Errors
    ///
    /// Returns when nested Deny is set without [`crate::SOCKET_PROXY_ENV`],
    /// when the ambient reqwest builder fails, or when the proxied rustls
    /// config fails.
    fn from_builder(builder: ClientBuilder) -> HttpResult<Self> {
        if !use_socket_proxy() {
            if nested_native_jail_requested() {
                return Err(Error::new(format!(
                    "{SOCKET_PROXY_ENV} is required when {NESTED_NATIVE_JAIL_ENV}=1; \
                     nested Deny forbids ambient TCP"
                )));
            }
            return Ok(Self {
                inner: Inner::Direct(direct_reqwest(&builder)?),
            });
        }
        #[cfg(any(unix, windows))]
        {
            let connector = SocketProxyConnector {
                tls: rustls_client_config()?,
            };
            let hyper =
                hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                    .build(connector);
            let cookies = if builder.cookie_store {
                Some(match builder.cookies {
                    Some(jar) => jar,
                    None => CookieJar::new(),
                })
            } else {
                builder.cookies
            };
            Ok(Self {
                inner: Inner::Proxied(Box::new(ProxiedClient {
                    hyper,
                    timeout: builder.timeout.or(builder.connect_timeout),
                    user_agent: builder.user_agent,
                    cookies,
                    redirect: builder.redirect,
                    default_headers: builder.default_headers,
                })),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {
                inner: Inner::Direct(direct_reqwest(&builder)?),
            })
        }
    }

    /// `GET`.
    #[must_use]
    pub fn get(&self, url: impl IntoUrlArg) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// `POST`.
    #[must_use]
    pub fn post(&self, url: impl IntoUrlArg) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// `PUT`.
    #[must_use]
    pub fn put(&self, url: impl IntoUrlArg) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    /// `DELETE`.
    #[must_use]
    pub fn delete(&self, url: impl IntoUrlArg) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    /// Arbitrary method.
    #[must_use]
    pub fn request(&self, method: Method, url: impl IntoUrlArg) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            url: url.into_url_arg(),
            headers: HeaderMap::new(),
            body: Vec::new(),
            query: Vec::new(),
            timeout: None,
        }
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// In-flight request.
pub struct RequestBuilder {
    client: Client,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
    query: Vec<(String, String)>,
    timeout: Option<Duration>,
}

impl RequestBuilder {
    /// Extra header.
    #[must_use]
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
    {
        let Ok(k) = HeaderName::try_from(key) else {
            return self;
        };
        let Ok(v) = HeaderValue::try_from(value) else {
            return self;
        };
        self.headers.insert(k, v);
        self
    }

    /// Merge headers (existing keys on `self` win).
    #[must_use]
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        for (k, v) in headers {
            if let Some(k) = k {
                self.headers.entry(k).or_insert(v);
            }
        }
        self
    }

    /// `Authorization: Bearer`.
    #[must_use]
    pub fn bearer_auth(self, token: impl AsRef<str>) -> Self {
        self.header(header::AUTHORIZATION, format!("Bearer {}", token.as_ref()))
    }

    /// JSON body + `Content-Type`.
    #[must_use]
    pub fn json<T: Serialize>(mut self, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(bytes) => {
                self.body = bytes;
                self.headers
                    .entry(header::CONTENT_TYPE)
                    .or_insert(HeaderValue::from_static("application/json"));
            }
            Err(_) => self.body.clear(),
        }
        self
    }

    /// `application/x-www-form-urlencoded`.
    #[must_use]
    pub fn form<T: Serialize>(mut self, value: &T) -> Self {
        match serde_urlencoded::to_string(value) {
            Ok(s) => {
                self.body = s.into_bytes();
                self.headers
                    .entry(header::CONTENT_TYPE)
                    .or_insert(HeaderValue::from_static(
                        "application/x-www-form-urlencoded",
                    ));
            }
            Err(_) => self.body.clear(),
        }
        self
    }

    /// Raw body bytes.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Query string pairs (serde_urlencoded).
    ///
    /// Values are decoded after `serde_urlencoded` so the URL encoder emits
    /// them once (reqwest-compatible). Storing the encoded form would turn
    /// `,` into `%252C` and Audible catalog search into HTTP 400.
    #[must_use]
    pub fn query<T: Serialize>(mut self, query: &T) -> Self {
        if let Ok(s) = serde_urlencoded::to_string(query) {
            for (k, v) in url::form_urlencoded::parse(s.as_bytes()) {
                self.query.push((k.into_owned(), v.into_owned()));
            }
        }
        self
    }

    /// Per-request timeout override.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Dispatch.
    ///
    /// # Errors
    ///
    /// Returns when the URL is invalid, CONNECT/TLS fails, or the origin errors.
    pub async fn send(self) -> HttpResult<Response> {
        let inner = self.client.inner.clone();
        match inner {
            Inner::Direct(http) => self.send_direct(http).await,
            #[cfg(any(unix, windows))]
            Inner::Proxied(proxy) => self.send_proxied(*proxy).await,
        }
    }

    /// # Errors
    ///
    /// Returns when the URL is invalid or the ambient HTTP request fails.
    async fn send_direct(self, http: reqwest::Client) -> HttpResult<Response> {
        let mut url = Url::parse(&self.url)?;
        append_query(&mut url, &self.query);
        let mut req = http.request(reqwest_method(&self.method), url.as_str());
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_bytes());
        }
        if !self.body.is_empty() {
            req = req.body(self.body);
        }
        if let Some(timeout) = self.timeout {
            req = req.timeout(timeout);
        }
        let resp = req.send().await?;
        Ok(Response {
            status: reqwest_status(resp.status()),
            headers: reqwest_headers(resp.headers()),
            url: resp.url().clone(),
            inner: ResponseInner::Direct(resp),
        })
    }

    #[cfg(any(unix, windows))]
    /// # Errors
    ///
    /// Returns on CONNECT/TLS failure, timeout, or when the origin errors.
    async fn send_proxied(self, proxy: ProxiedClient) -> HttpResult<Response> {
        let timeout = self.timeout.or(proxy.timeout);
        let fut = self.send_proxied_inner(proxy);
        match timeout {
            Some(t) => tokio::time::timeout(t, fut)
                .await
                .map_err(|_| Error::new("http timeout"))?,
            None => fut.await,
        }
    }

    #[cfg(any(unix, windows))]
    /// # Errors
    ///
    /// Returns when CONNECT, TLS, or an origin hop fails, or redirects exceed the policy.
    async fn send_proxied_inner(self, proxy: ProxiedClient) -> HttpResult<Response> {
        let mut url = Url::parse(&self.url)?;
        append_query(&mut url, &self.query);
        let mut method = self.method.clone();
        let mut body = self.body;
        let mut headers = proxy.default_headers.clone();
        for (k, v) in self.headers {
            if let Some(k) = k {
                headers.insert(k, v);
            }
        }
        if let Some(ua) = &proxy.user_agent {
            headers.entry(header::USER_AGENT).or_insert_with(|| {
                HeaderValue::from_str(ua).unwrap_or(HeaderValue::from_static("bookclerk-plugin"))
            });
        }
        let max_hops = match proxy.redirect {
            redirect::Policy::None => 0,
            redirect::Policy::Limited(n) => n,
            redirect::Policy::StopOnQueryContains(_) => 10,
        };
        let mut hop = 0_usize;
        loop {
            apply_cookies(&proxy.cookies, &url, &mut headers);
            let resp = proxy_once(&proxy.hyper, &method, &url, &headers, body.clone()).await?;
            store_cookies(&proxy.cookies, &url, &resp.headers);
            if let redirect::Policy::StopOnQueryContains(needle) = proxy.redirect {
                if url.query().is_some_and(|q| q.contains(needle)) {
                    return Ok(resp);
                }
            }
            if hop < max_hops {
                if let Some(loc) = redirect_location(&url, &resp.headers, resp.status) {
                    hop += 1;
                    if redirect_to_get(resp.status, &method) {
                        method = Method::GET;
                        body.clear();
                    }
                    // Match workerd egress.js: strip Fetch credentials across origins.
                    if loc.origin() != url.origin() {
                        headers.remove(header::AUTHORIZATION);
                        headers.remove(header::COOKIE);
                        headers.remove(header::PROXY_AUTHORIZATION);
                        headers.remove("cookie2");
                    }
                    url = loc;
                    continue;
                }
            }
            return Ok(resp);
        }
    }
}

/// Origin response.
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    url: Url,
    inner: ResponseInner,
}

enum ResponseInner {
    Direct(reqwest::Response),
    #[cfg(any(unix, windows))]
    Proxied(hyper::body::Incoming),
}

impl Response {
    /// HTTP status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Response headers (after cookie extraction on the proxied path).
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Final URL after redirects.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// `Content-Length` when the origin sent a valid header.
    #[must_use]
    pub fn content_length(&self) -> Option<u64> {
        self.headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
    }

    /// Fail on 4xx/5xx.
    ///
    /// # Errors
    ///
    /// Returns when the status is a client or server error.
    pub fn error_for_status(self) -> HttpResult<Self> {
        if self.status.is_client_error() || self.status.is_server_error() {
            return Err(Error::new(format!("HTTP {} for {}", self.status, self.url)));
        }
        Ok(self)
    }

    /// Entire body.
    ///
    /// # Errors
    ///
    /// Returns on I/O or decode failure.
    pub async fn bytes(self) -> HttpResult<Bytes> {
        match self.inner {
            ResponseInner::Direct(resp) => Ok(resp.bytes().await?),
            #[cfg(any(unix, windows))]
            ResponseInner::Proxied(incoming) => collect_incoming(incoming).await,
        }
    }

    /// UTF-8 body (lossy).
    ///
    /// # Errors
    ///
    /// Returns on I/O failure.
    pub async fn text(self) -> HttpResult<String> {
        let bytes = self.bytes().await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// JSON body.
    ///
    /// # Errors
    ///
    /// Returns on I/O or JSON decode failure.
    pub async fn json<T: DeserializeOwned>(self) -> HttpResult<T> {
        let bytes = self.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|err| Error::new(format!("json: {err}")))
    }

    /// Next body chunk (download loops).
    ///
    /// # Errors
    ///
    /// Returns on I/O failure.
    pub async fn chunk(&mut self) -> HttpResult<Option<Bytes>> {
        match &mut self.inner {
            ResponseInner::Direct(resp) => Ok(resp.chunk().await?),
            #[cfg(any(unix, windows))]
            ResponseInner::Proxied(incoming) => loop {
                match incoming.frame().await {
                    None => return Ok(None),
                    Some(Err(err)) => return Err(Error::new(err.to_string())),
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            return Ok(Some(data));
                        }
                    }
                }
            },
        }
    }

    /// Streaming body (audible-rs / throttled downloads).
    #[must_use]
    pub fn bytes_stream(self) -> ByteStream {
        match self.inner {
            ResponseInner::Direct(resp) => ByteStream {
                inner: ByteStreamInner::Direct(Box::pin(resp.bytes_stream())),
            },
            #[cfg(any(unix, windows))]
            ResponseInner::Proxied(incoming) => ByteStream {
                inner: ByteStreamInner::Proxied(incoming),
            },
        }
    }
}

/// [`Stream`] of body chunks from [`Response::bytes_stream`].
pub struct ByteStream {
    inner: ByteStreamInner,
}

enum ByteStreamInner {
    Direct(std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>),
    #[cfg(any(unix, windows))]
    Proxied(hyper::body::Incoming),
}

impl Stream for ByteStream {
    type Item = HttpResult<Bytes>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match &mut this.inner {
            ByteStreamInner::Direct(stream) => match stream.as_mut().poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(b))) => std::task::Poll::Ready(Some(Ok(b))),
                std::task::Poll::Ready(Some(Err(err))) => {
                    std::task::Poll::Ready(Some(Err(err.into())))
                }
                std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
            #[cfg(any(unix, windows))]
            ByteStreamInner::Proxied(incoming) => {
                match std::pin::Pin::new(incoming).poll_frame(cx) {
                    std::task::Poll::Ready(Some(Ok(frame))) => {
                        if let Ok(data) = frame.into_data() {
                            std::task::Poll::Ready(Some(Ok(data)))
                        } else {
                            std::pin::Pin::new(this).poll_next(cx)
                        }
                    }
                    std::task::Poll::Ready(Some(Err(err))) => {
                        std::task::Poll::Ready(Some(Err(Error::new(err.to_string()))))
                    }
                    std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
                    std::task::Poll::Pending => std::task::Poll::Pending,
                }
            }
        }
    }
}

/// CONNECT then rustls to `address` (HTTPS origins).
///
/// # Errors
///
/// Returns when the proxy is missing, CONNECT is denied, or TLS fails.
#[cfg(any(unix, windows))]
pub async fn connect_tls(address: SocketAddress) -> Result<TlsStream<crate::net::ProxyStream>> {
    let hostname = address.hostname.clone();
    wrap_tls(
        &hostname,
        connect(
            address,
            ConnectOptions {
                secure_transport: SecureTransport::Off,
                allow_half_open: false,
            },
        )
        .await?
        .into_stream(),
    )
    .await
}

fn use_socket_proxy() -> bool {
    #[cfg(any(unix, windows))]
    {
        std::env::var_os(SOCKET_PROXY_ENV).is_some()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// # Errors
///
/// Returns when the reqwest builder rejects the requested options.
fn direct_reqwest(builder: &ClientBuilder) -> HttpResult<reqwest::Client> {
    let mut b = reqwest::Client::builder();
    if let Some(t) = builder.timeout {
        b = b.timeout(t);
    }
    if let Some(t) = builder.connect_timeout {
        b = b.connect_timeout(t);
    }
    if let Some(ua) = &builder.user_agent {
        b = b.user_agent(ua);
    }
    if let Some(jar) = &builder.cookies {
        b = b.cookie_provider(jar.reqwest.clone());
    } else if builder.cookie_store {
        b = b.cookie_store(true);
    }
    b = match builder.redirect {
        redirect::Policy::None => b.redirect(reqwest::redirect::Policy::none()),
        redirect::Policy::Limited(n) => b.redirect(reqwest::redirect::Policy::limited(n)),
        redirect::Policy::StopOnQueryContains(needle) => {
            b.redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let code_seen = attempt
                    .previous()
                    .iter()
                    .any(|url| url.query().is_some_and(|q| q.contains(needle)));
                if code_seen {
                    attempt.stop()
                } else if attempt.previous().len() >= 10 {
                    attempt.error("too many redirects")
                } else {
                    attempt.follow()
                }
            }))
        }
    };
    if builder.disable_compression {
        b = b.no_gzip().no_brotli().no_deflate();
    }
    if !builder.default_headers.is_empty() {
        b = b.default_headers(builder.default_headers.clone());
    }
    Ok(b.build()?)
}

fn append_query(url: &mut Url, query: &[(String, String)]) {
    if query.is_empty() {
        return;
    }
    let mut pairs = url.query_pairs_mut();
    for (k, v) in query {
        pairs.append_pair(k, v);
    }
}

fn reqwest_method(method: &Method) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

fn reqwest_status(status: reqwest::StatusCode) -> StatusCode {
    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn reqwest_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in headers {
        if let Ok(v) = HeaderValue::from_bytes(v.as_bytes()) {
            out.append(k.clone(), v);
        }
    }
    out
}

#[cfg(any(unix, windows))]
/// # Errors
///
/// Returns when the proxied response body cannot be collected.
async fn collect_incoming(incoming: hyper::body::Incoming) -> HttpResult<Bytes> {
    let collected = incoming
        .collect()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(collected.to_bytes())
}

#[cfg(any(unix, windows))]
fn apply_cookies(jar: &Option<Arc<CookieJar>>, url: &Url, headers: &mut HeaderMap) {
    let Some(jar) = jar else {
        return;
    };
    let store = jar
        .store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cookie = store
        .get_request_values(url)
        .map(|(n, v)| format!("{n}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    if cookie.is_empty() {
        // Clear a prior hop's Cookie so an empty jar for this origin cannot
        // leave stale credentials on the next request.
        headers.remove(header::COOKIE);
        return;
    }
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        headers.insert(header::COOKIE, v);
    }
}

#[cfg(any(unix, windows))]
fn store_cookies(jar: &Option<Arc<CookieJar>>, url: &Url, headers: &HeaderMap) {
    let Some(jar) = jar else {
        return;
    };
    let mut store = jar
        .store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for val in headers.get_all(header::SET_COOKIE) {
        let Ok(s) = val.to_str() else {
            continue;
        };
        let _ = store.parse(s, url);
    }
}

#[cfg(any(unix, windows))]
fn redirect_to_get(status: StatusCode, method: &Method) -> bool {
    let code = status.as_u16();
    (matches!(code, 301 | 302) && *method == Method::POST)
        || (code == 303 && *method != Method::GET && *method != Method::HEAD)
}

#[cfg(any(unix, windows))]
fn redirect_location(current: &Url, headers: &HeaderMap, status: StatusCode) -> Option<Url> {
    if !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let loc = headers.get(header::LOCATION)?.to_str().ok()?;
    current.join(loc).ok()
}

#[cfg(any(unix, windows))]
/// # Errors
///
/// Returns when the request cannot be built or the proxied origin hop fails.
async fn proxy_once(
    hyper: &hyper_util::client::legacy::Client<SocketProxyConnector, Full<Bytes>>,
    method: &Method,
    url: &Url,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> HttpResult<Response> {
    let uri: http::Uri = url
        .as_str()
        .parse()
        .map_err(|err| Error::new(format!("{err}")))?;
    let mut req = http::Request::builder().method(method.clone()).uri(uri);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let req = req
        .body(Full::new(Bytes::from(body)))
        .map_err(|err| Error::new(err.to_string()))?;
    let resp = hyper
        .request(req)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    Ok(Response {
        status,
        headers,
        url: url.clone(),
        inner: ResponseInner::Proxied(resp.into_body()),
    })
}

#[cfg(any(unix, windows))]
/// # Errors
///
/// Returns when the TLS server name is invalid or the handshake fails.
async fn wrap_tls(
    hostname: &str,
    stream: crate::net::ProxyStream,
) -> Result<TlsStream<crate::net::ProxyStream>> {
    let config = rustls_client_config()?;
    let server_name = rustls::pki_types::ServerName::try_from(hostname.to_string())
        .map_err(|err| SdkError::message(format!("tls server name `{hostname}`: {err}")))?;
    tokio_rustls::TlsConnector::from(config)
        .connect(server_name, stream)
        .await
        .map_err(|err| SdkError::message(format!("tls handshake {hostname}: {err}")))
}

#[cfg(any(unix, windows))]
/// # Errors
///
/// Returns when the rustls client config cannot be built (does not currently fail).
fn rustls_client_config() -> Result<Arc<rustls::ClientConfig>> {
    static CELL: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    Ok(Arc::clone(CELL.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(cfg)
    })))
}

#[cfg(any(unix, windows))]
#[derive(Clone)]
struct SocketProxyConnector {
    tls: Arc<rustls::ClientConfig>,
}

#[cfg(any(unix, windows))]
impl tower_service::Service<http::Uri> for SocketProxyConnector {
    type Response = hyper_util::rt::TokioIo<MaybeTls>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<Self::Response, Self::Error>>
                + Send,
        >,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: http::Uri) -> Self::Future {
        let tls = Arc::clone(&self.tls);
        Box::pin(async move { connect_uri(dst, tls).await })
    }
}

#[cfg(any(unix, windows))]
/// # Errors
///
/// Returns when CONNECT, TLS, or URI parsing fails.
async fn connect_uri(
    dst: http::Uri,
    tls: Arc<rustls::ClientConfig>,
) -> std::result::Result<hyper_util::rt::TokioIo<MaybeTls>, Box<dyn std::error::Error + Send + Sync>>
{
    let scheme = dst.scheme_str().unwrap_or("http");
    let host = dst
        .host()
        .ok_or_else(|| SdkError::message("http uri missing host"))?
        .to_string();
    let port = dst
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    let stream = connect(
        SocketAddress {
            hostname: host.clone(),
            port,
        },
        ConnectOptions::default(),
    )
    .await?
    .into_stream();
    let io = if scheme == "https" {
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|err| SdkError::message(format!("tls server name `{host}`: {err}")))?;
        let tls_stream = tokio_rustls::TlsConnector::from(tls)
            .connect(server_name, stream)
            .await
            .map_err(|err| SdkError::message(format!("tls handshake {host}: {err}")))?;
        MaybeTls::tls(tls_stream)
    } else {
        MaybeTls::plain(stream)
    };
    Ok(hyper_util::rt::TokioIo::new(io))
}

#[cfg(any(unix, windows))]
trait ProxyIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

#[cfg(any(unix, windows))]
impl<T> ProxyIo for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

#[cfg(any(unix, windows))]
struct MaybeTls {
    inner: Box<dyn ProxyIo>,
}

#[cfg(any(unix, windows))]
impl MaybeTls {
    fn plain(stream: crate::net::ProxyStream) -> Self {
        Self {
            inner: Box::new(stream),
        }
    }

    fn tls(stream: TlsStream<crate::net::ProxyStream>) -> Self {
        Self {
            inner: Box::new(stream),
        }
    }
}

#[cfg(any(unix, windows))]
impl hyper_util::client::legacy::connect::Connection for MaybeTls {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        hyper_util::client::legacy::connect::Connected::new()
    }
}

#[cfg(any(unix, windows))]
impl tokio::io::AsyncRead for MaybeTls {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.inner).poll_read(cx, buf)
    }
}

#[cfg(any(unix, windows))]
impl tokio::io::AsyncWrite for MaybeTls {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::missing_panics_doc, clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::net::SOCKET_PROXY_ENV_LOCK;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn proxied_http_get_through_fake_connect_proxy() {
        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("CONNECT example.test:80"), "{req}");
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut rest = vec![0_u8; 1024];
            let n = stream.read(&mut rest).await.unwrap();
            let http = String::from_utf8_lossy(&rest[..n]);
            assert!(http.starts_with("GET /ping"), "{http}");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nping")
                .await
                .unwrap();
        });
        std::env::set_var(SOCKET_PROXY_ENV, &path);
        let client = Client::builder()
            .redirect(redirect::Policy::none())
            .build()
            .unwrap();
        let resp = client
            .get("http://example.test/ping")
            .send()
            .await
            .expect("proxied get");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "ping");
        server.await.unwrap();
        std::env::remove_var(SOCKET_PROXY_ENV);
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod query_encoding_tests {
    use super::*;

    #[test]
    fn nested_jail_without_socket_proxy_fails_closed() {
        let _guard = crate::net::SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_proxy = std::env::var_os(SOCKET_PROXY_ENV);
        let previous_jail = std::env::var_os(NESTED_NATIVE_JAIL_ENV);
        std::env::remove_var(SOCKET_PROXY_ENV);
        std::env::set_var(NESTED_NATIVE_JAIL_ENV, "1");
        let err = Client::builder()
            .build()
            .expect_err("nested Deny must not use ambient TCP");
        assert!(err.to_string().contains("SOCKET_PROXY"), "{err}");
        match previous_proxy {
            Some(v) => std::env::set_var(SOCKET_PROXY_ENV, v),
            None => std::env::remove_var(SOCKET_PROXY_ENV),
        }
        match previous_jail {
            Some(v) => std::env::set_var(NESTED_NATIVE_JAIL_ENV, v),
            None => std::env::remove_var(NESTED_NATIVE_JAIL_ENV),
        }
    }

    /// Audible catalog `response_groups` uses commas. serde_urlencoded emits
    /// `%2C`; appending that string as a raw pair would become `%252C`.
    #[test]
    fn query_encodes_commas_once() {
        let _guard = crate::net::SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_jail = std::env::var_os(NESTED_NATIVE_JAIL_ENV);
        std::env::remove_var(NESTED_NATIVE_JAIL_ENV);
        let req = Client::new()
            .get("https://api.audible.com/1.0/catalog/search")
            .query(&[("response_groups", "product_attrs,product_desc")]);
        let mut url = Url::parse("https://api.audible.com/1.0/catalog/search").unwrap();
        append_query(&mut url, &req.query);
        let serialized = url.as_str();
        assert!(
            !serialized.contains("%252C"),
            "double-encoded comma: {serialized}"
        );
        assert!(
            serialized.contains("product_attrs%2Cproduct_desc"),
            "{serialized}"
        );
        match previous_jail {
            Some(v) => std::env::set_var(NESTED_NATIVE_JAIL_ENV, v),
            None => std::env::remove_var(NESTED_NATIVE_JAIL_ENV),
        }
    }
}
