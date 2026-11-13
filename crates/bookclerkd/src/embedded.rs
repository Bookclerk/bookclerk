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
//! `daemon.auth.enabled = false` no token is added.
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
use std::sync::Arc;
use std::time::Duration;

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
        Ok(Self { state, app })
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
    /// The router's response, whole, without `Set-Cookie`; or `404` for a
    /// refused path, `502` for an oversized body, `504` on timeout.
    pub async fn handle(&self, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
        let auth = self.state.auth_snapshot().await;
        let bearer = auth.enabled.then(|| auth.token.clone());
        drop(auth);
        serve(self.app.clone(), bearer.as_deref(), request).await
    }

    /// Stops the integration watchers before the process exits, as
    /// `bookclerkd` does on shutdown.
    pub async fn shutdown(&self) {
        self.state.integrations.read().await.stop_all().await;
    }
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
fn refusal(path: &str) -> Option<Refusal> {
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
/// less credentials, origins and forwarding headers, plus `Host` and, when
/// operator auth is on, the operator's bearer token.
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
    if let Some(reason) = refusal(parts.uri.path()) {
        tracing::debug!(?reason, path = parts.uri.path(), "embedded request refused");
        return status_only(StatusCode::NOT_FOUND);
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
            "accept=application/json\nauthorization=Bearer op-token\nhost=localhost"
        );
    }

    #[tokio::test]
    async fn auth_off_adds_no_token() {
        let response = serve(echo_router(), None, page_request("/api/echo")).await;
        let seen = String::from_utf8(response.into_body()).unwrap();
        assert_eq!(seen, "accept=application/json\nhost=localhost");
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
        assert_eq!(refusal("/api/node"), Some(Refusal::NodeApi));
        assert_eq!(refusal("/api/node/"), Some(Refusal::NodeApi));
        assert_eq!(refusal("/api/live"), Some(Refusal::LiveSocket));
        assert_eq!(refusal("/api/nodes"), None);
        assert_eq!(refusal("/api/livexyz"), None);
        assert_eq!(refusal("/"), None);
    }
}
