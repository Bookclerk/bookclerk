//! The host embedded in a desktop window, with no listener.
//!
//! [`EmbeddedHost::start`] runs the same host `bookclerkd` runs (control
//! plane, plugin deployments, jobs, events, scheduler, cluster links) but
//! binds no socket: not `daemon.listen`, not the node API's TLS listener.
//! The window's webview loads the SPA over its own custom scheme, and the
//! shell passes each request to [`EmbeddedHost::handle`], which calls the
//! router in-process.
//!
//! **Who the caller is.** The window belongs to whoever runs the desktop app
//! on this machine, the person the tray already hands an operator session
//! to. Every request is the operator: the host adds the current operator
//! token as `Authorization: Bearer`, read again for every request so a
//! rotation applies at once, and the token never reaches the page. Whatever
//! the page sent as cookies, `Authorization`, `Origin`, `Referer` or
//! forwarding headers is dropped first, and `Set-Cookie` is dropped from
//! every response, so the page holds no credential of its own. With
//! `daemon.auth.enabled = false` no token is added. The router sees every
//! request as coming from [`EMBEDDED_ORIGIN`], a loopback origin, so a join
//! asks the library to send the window back there.
//!
//! **Other pages.** Because the token is added here, a page from anywhere
//! else must not be able to reach the scheme: a request whose `Origin` is
//! not one of [`APP_ORIGINS`] (or, for a write, whose `Referer` is from
//! elsewhere) is refused with `403` before routing.
//!
//! **Leaving the app.** The window shows only app pages, with one exception:
//! joining a library signs the person in on the library's own site. From a
//! successful `POST /api/config/join/start` until the library sends the
//! window back (or the sign-in expires), [`EmbeddedHost::navigation`] lets
//! the window open `https` pages; the library's return to
//! `http://localhost/join/callback` is handed back to the app scheme.
//!
//! **What the window cannot reach.** The request carries no peer address,
//! so routes that admit only a direct loopback connection (the tray
//! handoff) and the node API's transport policy refuse it. The node API
//! (`/api/node/...`) is for other nodes over TLS and is refused here before
//! routing. The live socket (`/api/live`) needs a WebSocket, which a custom
//! scheme cannot carry; it is refused, and the SPA falls back to polling.
//!
//! **Responses** are collected whole, up to [`MAX_RESPONSE_BYTES`] and
//! [`RESPONSE_TIMEOUT`]; a custom scheme cannot stream.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use axum::Router;
use tower_service::Service as _;

use crate::api::AppState;
use crate::{init_logging, start_host, HostMode, HostParts};

/// Largest response body the window receives. Larger answers (none of the
/// web app's routes produce one) become `502 Bad Gateway`.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// Longest a single request may take before it is answered with
/// `504 Gateway Timeout`. A route that streams without end (none should be
/// reachable here) would otherwise hold the request forever.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);

/// The `Host` every embedded request is routed under.
const EMBEDDED_HOST: &str = "localhost";

/// The origin the router sees every embedded request come from: loopback,
/// so a join's return address is one the library accepts.
pub const EMBEDDED_ORIGIN: &str = "http://localhost";

/// Origins of the window's own pages: the custom scheme, and the form
/// WebView2 (Windows) gives it.
pub const APP_ORIGINS: [&str; 2] = ["bookclerk://localhost", "http://bookclerk.localhost"];

/// The route that starts a join; its answer opens the sign-in window.
const JOIN_START_PATH: &str = "/api/config/join/start";

/// Request headers dropped before routing, so the page cannot present a
/// credential, an origin or a forwarded address of its own.
const DROPPED_REQUEST_HEADERS: [HeaderName; 7] = [
    header::AUTHORIZATION,
    header::COOKIE,
    header::ORIGIN,
    header::REFERER,
    header::FORWARDED,
    header::HOST,
    header::PROXY_AUTHORIZATION,
];

/// Where the embedded host reads its files and configuration, as
/// `bookclerkd`'s `--bookclerk-files` and `--config` do.
#[derive(Debug, Clone, Default)]
pub struct EmbeddedOptions {
    /// Bookclerk files directory; `None` uses `BOOKCLERK_FILES_DIR` or the
    /// platform default.
    pub bookclerk_files: Option<PathBuf>,
    /// Path to `config.toml`; `None` uses the one in the files directory.
    pub config: Option<PathBuf>,
}

/// A running host with no listener, answering requests handed to it.
pub struct EmbeddedHost {
    /// Shared daemon state, held by the background runtimes too.
    state: Arc<AppState>,
    /// The web app and API router.
    app: Router,
    /// Until when the window may show the library's sign-in pages: set by a
    /// successful join start, cleared when the library sends it back.
    join_open_until: Mutex<Option<Instant>>,
}

/// Where the window may go, as [`EmbeddedHost::navigation`] decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Navigation {
    /// One of the app's own pages.
    App,
    /// A library's sign-in page, while a join is open.
    JoinSignIn,
    /// The library sending the window back after sign-in. The window opens
    /// this path and query on the app scheme instead.
    JoinReturn(String),
    /// Anywhere else: refused.
    Refuse,
}

impl std::fmt::Debug for EmbeddedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedHost").finish_non_exhaustive()
    }
}

/// Why a request was refused before it reached the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// A node API path: other nodes reach it over TLS, never the window.
    NodeApi,
    /// The live WebSocket, which a custom scheme cannot carry.
    LiveSocket,
    /// A request a page from another origin sent.
    ForeignOrigin,
}

impl EmbeddedHost {
    /// Loads the configuration, installs logging and starts the host.
    ///
    /// Call it once per process, from inside a Tokio runtime that outlives
    /// the host: the background runtimes run on it.
    ///
    /// # Arguments
    ///
    /// * `options` - Where the files directory and configuration are.
    /// * `syslog_identifier` - The name the OS log facility records for this
    ///   process.
    ///
    /// # Returns
    ///
    /// The running host.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration cannot be loaded or the host
    /// cannot start (an interrupted clerk join, a control plane that does
    /// not open, or operator auth that cannot be built).
    pub async fn start(options: EmbeddedOptions, syslog_identifier: &str) -> anyhow::Result<Self> {
        let config = bookclerk_config::Config::load(options.bookclerk_files, options.config)?;
        init_logging(&config, syslog_identifier);
        let HostParts { state, app, .. } = start_host(config, HostMode::Embedded).await?;
        tracing::info!("host started embedded in the desktop window; no listener is bound");
        Ok(Self {
            state,
            app,
            join_open_until: Mutex::new(None),
        })
    }

    /// Answers one request from the window.
    ///
    /// # Arguments
    ///
    /// * `request` - The webview's request. Only its method, path, query,
    ///   body and non-credential headers are used.
    ///
    /// # Returns
    ///
    /// The router's response, whole, without `Set-Cookie`; or `403` for a
    /// request from another origin, `404` for a refused path, `502` for an
    /// oversized body, `504` on timeout.
    pub async fn handle(&self, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
        let auth = self.state.auth_snapshot().await;
        let bearer = auth.enabled.then(|| auth.token.clone());
        drop(auth);
        let join_start =
            request.method() == axum::http::Method::POST && request.uri().path() == JOIN_START_PATH;
        let response = serve(self.app.clone(), bearer.as_deref(), request).await;
        if join_start && response.status().is_success() {
            if let Some(ttl) = join_sign_in_ttl(response.body()) {
                self.set_join_open(Some(Instant::now() + ttl));
            }
        }
        response
    }

    /// Decides whether the window may show `url`.
    ///
    /// # Arguments
    ///
    /// * `url` - Where the window is about to go.
    ///
    /// # Returns
    ///
    /// What the window should do (see [`Navigation`]). A
    /// [`Navigation::JoinReturn`] also closes the join's sign-in window.
    pub fn navigation(&self, url: &str) -> Navigation {
        let open = self
            .join_open_until
            .lock()
            .map(|until| until.is_some_and(|until| Instant::now() < until))
            .unwrap_or(false);
        let decision = classify_navigation(url, open);
        if matches!(decision, Navigation::JoinReturn(_)) {
            self.set_join_open(None);
        }
        decision
    }

    /// Sets or clears the join sign-in deadline.
    fn set_join_open(&self, until: Option<Instant>) {
        if let Ok(mut slot) = self.join_open_until.lock() {
            *slot = until;
        }
    }

    /// Stops the integration watchers before the process exits, as
    /// `bookclerkd` does on shutdown.
    pub async fn shutdown(&self) {
        self.state.integrations.read().await.stop_all().await;
    }
}

/// Refuses requests from other origins and paths the window must not reach.
///
/// # Arguments
///
/// * `method` - The request method.
/// * `headers` - The headers the webview sent.
/// * `path` - The request path, undecoded (as the router matches it).
///
/// # Returns
///
/// `Some` with the reason for a refused request, `None` otherwise.
fn refusal(method: &axum::http::Method, headers: &HeaderMap, path: &str) -> Option<Refusal> {
    if from_another_origin(method, headers) {
        return Some(Refusal::ForeignOrigin);
    }
    refused_path(path)
}

/// Whether a request was sent by a page that is not the app's: its
/// `Origin` names another origin (or is opaque), or, for a write without
/// `Origin`, its `Referer` does.
///
/// # Arguments
///
/// * `method` - The request method.
/// * `headers` - The headers the webview sent.
///
/// # Returns
///
/// `true` when the request must be refused.
fn from_another_origin(method: &axum::http::Method, headers: &HeaderMap) -> bool {
    if let Some(origin) = headers.get(header::ORIGIN) {
        return !origin
            .to_str()
            .is_ok_and(|origin| APP_ORIGINS.contains(&origin));
    }
    let writes = !matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if !writes {
        return false;
    }
    headers.get(header::REFERER).is_some_and(|referer| {
        !referer
            .to_str()
            .is_ok_and(|referer| url::Url::parse(referer).is_ok_and(|url| is_app_url(&url)))
    })
}

/// Whether `url` is one of the app's own pages.
fn is_app_url(url: &url::Url) -> bool {
    match url.scheme() {
        "bookclerk" => url.host_str() == Some("localhost"),
        "http" => url.host_str() == Some("bookclerk.localhost"),
        _ => false,
    }
}

/// Whether `url` is a loopback `http` address (`localhost`, `127.0.0.1`,
/// `[::1]`, any port).
fn is_loopback_http(url: &url::Url) -> bool {
    url.scheme() == "http"
        && match url.host() {
            Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
}

/// Where the window may go.
///
/// # Arguments
///
/// * `raw` - The URL the window is about to open.
/// * `join_open` - Whether a join's sign-in is under way.
///
/// # Returns
///
/// The app's own pages always; the library's return to
/// `http://localhost/join/callback` (any loopback host and port) as a
/// [`Navigation::JoinReturn`] while a join is open; other `https` pages,
/// or loopback `http` ones, only while a join is open.
fn classify_navigation(raw: &str, join_open: bool) -> Navigation {
    let Ok(url) = url::Url::parse(raw) else {
        return Navigation::Refuse;
    };
    if is_app_url(&url) {
        return Navigation::App;
    }
    if !join_open {
        return Navigation::Refuse;
    }
    if is_loopback_http(&url) && url.path() == bookclerk_node_proto::JOIN_CALLBACK_PATH {
        let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        return Navigation::JoinReturn(format!("{}{query}", url.path()));
    }
    if url.scheme() == "https" || is_loopback_http(&url) {
        return Navigation::JoinSignIn;
    }
    Navigation::Refuse
}

/// How long the window may show the library's sign-in, from a successful
/// join start's JSON answer (`expires_in_secs`, at most an hour).
///
/// # Arguments
///
/// * `body` - The answer's body.
///
/// # Returns
///
/// The time to allow, or `None` when the body is not a join start.
fn join_sign_in_ttl(body: &[u8]) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value.get("authorize_url")?.as_str()?;
    let secs = value.get("expires_in_secs")?.as_u64()?.min(3600);
    Some(Duration::from_secs(secs))
}

/// Refuses paths the window must not reach.
///
/// # Arguments
///
/// * `path` - The request path, undecoded (as the router matches it).
///
/// # Returns
///
/// `Some` with the reason for a refused path, `None` otherwise.
fn refused_path(path: &str) -> Option<Refusal> {
    let under = |prefix: &str| {
        path == prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    if under("/api/node") {
        Some(Refusal::NodeApi)
    } else if under(crate::live::LIVE_PATH) {
        Some(Refusal::LiveSocket)
    } else {
        None
    }
}

/// The headers the router sees for an embedded request: the page's own,
/// less credentials, origins and forwarding headers, plus `Host`, `Origin`
/// ([`EMBEDDED_ORIGIN`]) and, when operator auth is on, the operator's bearer
/// token.
///
/// # Arguments
///
/// * `page` - The headers the webview sent.
/// * `bearer` - The operator token, or `None` when operator auth is off.
///
/// # Returns
///
/// The rewritten headers.
fn rewrite_request_headers(page: &HeaderMap, bearer: Option<&str>) -> HeaderMap {
    let mut headers = page.clone();
    for name in &DROPPED_REQUEST_HEADERS {
        headers.remove(name);
    }
    let forwarded: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("x-forwarded-") || name.as_str() == "x-real-ip")
        .cloned()
        .collect();
    for name in forwarded {
        headers.remove(name);
    }
    headers.insert(header::HOST, HeaderValue::from_static(EMBEDDED_HOST));
    headers.insert(header::ORIGIN, HeaderValue::from_static(EMBEDDED_ORIGIN));
    if let Some(token) = bearer {
        match HeaderValue::from_str(&format!("Bearer {token}")) {
            Ok(mut value) => {
                value.set_sensitive(true);
                headers.insert(header::AUTHORIZATION, value);
            }
            // An operator token is URL-safe text; one that is not a valid
            // header value leaves the request unauthenticated (fail closed).
            Err(_) => tracing::error!("operator token is not a valid header value"),
        }
    }
    headers
}

/// Routes one request through `app` under the embedded policy.
///
/// # Arguments
///
/// * `app` - The router.
/// * `bearer` - The operator token to present, or `None` when auth is off.
/// * `request` - The webview's request.
///
/// # Returns
///
/// The collected response (see [`EmbeddedHost::handle`]).
async fn serve(
    mut app: Router,
    bearer: Option<&str>,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let (parts, body) = request.into_parts();
    if let Some(reason) = refusal(&parts.method, &parts.headers, parts.uri.path()) {
        tracing::debug!(?reason, path = parts.uri.path(), "embedded request refused");
        return status_only(match reason {
            Refusal::ForeignOrigin => StatusCode::FORBIDDEN,
            Refusal::NodeApi | Refusal::LiveSocket => StatusCode::NOT_FOUND,
        });
    }
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| String::from("/"), |pq| pq.as_str().to_owned());
    let mut routed = Request::new(Body::from(body));
    *routed.method_mut() = parts.method;
    *routed.uri_mut() = match path_and_query.parse() {
        Ok(uri) => uri,
        Err(_) => return status_only(StatusCode::BAD_REQUEST),
    };
    *routed.headers_mut() = rewrite_request_headers(&parts.headers, bearer);

    let answer = async {
        let response = match app.call(routed).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
        let (mut parts, body) = response.into_parts();
        parts.headers.remove(header::SET_COOKIE);
        match axum::body::to_bytes(body, MAX_RESPONSE_BYTES).await {
            Ok(bytes) => Response::from_parts(parts, bytes.to_vec()),
            Err(err) => {
                tracing::warn!(error = %err, "embedded response body could not be collected");
                status_only(StatusCode::BAD_GATEWAY)
            }
        }
    };
    tokio::time::timeout(RESPONSE_TIMEOUT, answer)
        .await
        .unwrap_or_else(|_| status_only(StatusCode::GATEWAY_TIMEOUT))
}

/// An empty response with `status`.
fn status_only(status: StatusCode) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};

    /// A router that echoes the headers it sees and sets a cookie.
    fn echo_router() -> Router {
        async fn echo(headers: HeaderMap) -> Response<Body> {
            let mut lines: Vec<String> = headers
                .iter()
                .map(|(name, value)| format!("{name}={}", value.to_str().unwrap_or("?")))
                .collect();
            lines.sort();
            let mut response = Response::new(Body::from(lines.join("\n")));
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_static("bookclerk_operator_session=x; Path=/"),
            );
            response
        }
        Router::new()
            .route("/api/echo", get(echo))
            .route("/api/node/v1/enroll", post(echo))
            .route("/api/live", get(echo))
            .route("/api/nodes", get(echo))
    }

    fn page_request(path: &str) -> Request<Vec<u8>> {
        Request::builder()
            .uri(format!("bookclerk://localhost{path}"))
            .header(header::COOKIE, "bookclerk_operator_session=stolen")
            .header(header::AUTHORIZATION, "Bearer page-supplied")
            .header(header::ORIGIN, "bookclerk://localhost")
            .header(header::REFERER, "bookclerk://localhost/")
            .header("x-forwarded-for", "203.0.113.9")
            .header("x-forwarded-proto", "https")
            .header(header::ACCEPT, "application/json")
            .body(Vec::new())
            .unwrap()
    }

    #[tokio::test]
    async fn requests_carry_the_operator_token_and_nothing_the_page_sent() {
        let response = serve(
            echo_router(),
            Some("op-token"),
            page_request("/api/echo?x=1"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let seen = String::from_utf8(response.into_body()).unwrap();
        assert_eq!(
            seen,
            "accept=application/json\nauthorization=Bearer op-token\nhost=localhost\norigin=http://localhost"
        );
    }

    #[tokio::test]
    async fn auth_off_adds_no_token() {
        let response = serve(echo_router(), None, page_request("/api/echo")).await;
        let seen = String::from_utf8(response.into_body()).unwrap();
        assert_eq!(
            seen,
            "accept=application/json\nhost=localhost\norigin=http://localhost"
        );
    }

    #[tokio::test]
    async fn node_api_and_live_socket_are_refused_before_routing() {
        for path in [
            "/api/node",
            "/api/node/v1/enroll",
            "/api/live",
            "/api/live/x",
        ] {
            let response = serve(echo_router(), Some("t"), page_request(path)).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(response.body().is_empty(), "{path}");
        }
        // A sibling route that merely shares a prefix is not refused.
        let response = serve(echo_router(), Some("t"), page_request("/api/nodes")).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn refusal_matches_whole_segments_only() {
        assert_eq!(refused_path("/api/node"), Some(Refusal::NodeApi));
        assert_eq!(refused_path("/api/node/"), Some(Refusal::NodeApi));
        assert_eq!(refused_path("/api/live"), Some(Refusal::LiveSocket));
        assert_eq!(refused_path("/api/nodes"), None);
        assert_eq!(refused_path("/api/livexyz"), None);
        assert_eq!(refused_path("/"), None);
    }

    fn with(origin: Option<&str>, referer: Option<&str>, method: &str) -> Request<Vec<u8>> {
        let mut builder = Request::builder()
            .method(method)
            .uri("bookclerk://localhost/api/echo");
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        if let Some(referer) = referer {
            builder = builder.header(header::REFERER, referer);
        }
        builder.body(Vec::new()).unwrap()
    }

    #[tokio::test]
    async fn pages_from_other_origins_are_refused() {
        let app = || echo_router().route("/api/echo", post(|| async { "posted" }));
        for (origin, referer, method) in [
            (Some("https://library.example"), None, "POST"),
            (Some("https://library.example"), None, "GET"),
            (Some("null"), None, "POST"),
            (None, Some("https://library.example/join/approve"), "POST"),
        ] {
            let response = serve(app(), Some("t"), with(origin, referer, method)).await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{origin:?} {referer:?} {method}"
            );
        }
        for (origin, referer, method) in [
            (Some("bookclerk://localhost"), None, "POST"),
            (Some("http://bookclerk.localhost"), None, "POST"),
            (None, Some("bookclerk://localhost/settings"), "POST"),
            (None, None, "POST"),
            // A plain read with a foreign referer (an image, a link) is not a write.
            (None, Some("https://library.example/"), "GET"),
        ] {
            let response = serve(app(), Some("t"), with(origin, referer, method)).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{origin:?} {referer:?} {method}"
            );
        }
    }

    #[test]
    fn the_window_stays_on_the_app_unless_a_join_is_open() {
        for open in [false, true] {
            assert_eq!(
                classify_navigation("bookclerk://localhost/settings", open),
                Navigation::App
            );
            assert_eq!(
                classify_navigation("http://bookclerk.localhost/", open),
                Navigation::App
            );
            assert_eq!(
                classify_navigation("file:///etc/passwd", open),
                Navigation::Refuse
            );
            assert_eq!(classify_navigation("not a url", open), Navigation::Refuse);
        }
        assert_eq!(
            classify_navigation("https://library.example/join/approve", false),
            Navigation::Refuse
        );
        assert_eq!(
            classify_navigation("https://library.example/join/approve", true),
            Navigation::JoinSignIn
        );
        assert_eq!(
            classify_navigation("http://localhost/join/callback?code=c&state=s", false),
            Navigation::Refuse
        );
        assert_eq!(
            classify_navigation("http://localhost/join/callback?code=c&state=s", true),
            Navigation::JoinReturn("/join/callback?code=c&state=s".into())
        );
        assert_eq!(
            classify_navigation("http://127.0.0.1:8787/join/callback", true),
            Navigation::JoinReturn("/join/callback".into())
        );
        assert_eq!(
            classify_navigation("http://evil.example/join/callback", true),
            Navigation::Refuse
        );
    }

    #[test]
    fn join_ttl_comes_from_a_join_start_answer_only() {
        let started =
            br#"{"authorize_url":"https://l.example/join/approve","expires_in_secs":600}"#;
        assert_eq!(join_sign_in_ttl(started), Some(Duration::from_secs(600)));
        let long = br#"{"authorize_url":"x","expires_in_secs":999999}"#;
        assert_eq!(join_sign_in_ttl(long), Some(Duration::from_secs(3600)));
        assert_eq!(join_sign_in_ttl(br#"{"expires_in_secs":600}"#), None);
        assert_eq!(join_sign_in_ttl(b"not json"), None);
    }

    #[tokio::test]
    async fn a_join_start_opens_the_sign_in_until_the_library_sends_the_window_back() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let (state, _store) = crate::node_api::tests::test_state(&files).await;
        let app = echo_router().route(
            JOIN_START_PATH,
            post(|| async {
                axum::Json(serde_json::json!({
                    "authorize_url": "https://library.example/join/approve",
                    "expires_in_secs": 600,
                }))
            }),
        );
        let host = EmbeddedHost {
            state,
            app,
            join_open_until: Mutex::new(None),
        };
        assert!(format!("{host:?}").starts_with("EmbeddedHost"));
        let sign_in = "https://library.example/join/approve";
        assert_eq!(host.navigation(sign_in), Navigation::Refuse);

        let echoed = host.handle(page_request("/api/echo")).await;
        assert_eq!(echoed.status(), StatusCode::OK);
        assert_eq!(host.navigation(sign_in), Navigation::Refuse);

        let mut start = page_request(JOIN_START_PATH);
        *start.method_mut() = axum::http::Method::POST;
        let started = host.handle(start).await;
        assert_eq!(started.status(), StatusCode::OK);
        assert_eq!(host.navigation(sign_in), Navigation::JoinSignIn);
        assert_eq!(
            host.navigation("http://localhost/join/callback?code=c"),
            Navigation::JoinReturn("/join/callback?code=c".into())
        );
        assert_eq!(host.navigation(sign_in), Navigation::Refuse);
        host.shutdown().await;
    }
}
