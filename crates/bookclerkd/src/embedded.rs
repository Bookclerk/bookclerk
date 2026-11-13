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
use axum::http::{header, Request, Response, StatusCode};
use axum::Router;
use bookclerk_window_policy::{
    classify_navigation, join_sign_in_ttl, refusal, rewrite_request_headers, Refusal,
    JOIN_START_PATH,
};
pub use bookclerk_window_policy::{Navigation, APP_ORIGINS, EMBEDDED_ORIGIN};
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

impl std::fmt::Debug for EmbeddedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedHost").finish_non_exhaustive()
    }
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
    let headers = rewrite_request_headers(&parts.headers, bearer);
    if bearer.is_some() && !headers.contains_key(header::AUTHORIZATION) {
        // An operator token is URL-safe text; one that is not a valid header
        // value leaves the request unauthenticated (fail closed).
        tracing::error!("operator token is not a valid header value");
    }
    *routed.headers_mut() = headers;

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
    use axum::http::{HeaderMap, HeaderValue};
    use axum::routing::{get, post};

    #[test]
    fn the_policy_refuses_the_live_socket_this_daemon_serves() {
        assert_eq!(bookclerk_window_policy::LIVE_PATH, crate::live::LIVE_PATH);
    }

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
