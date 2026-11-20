//! The host embedded in a desktop window, with no listener.
//!
//! [`EmbeddedHost::start`] runs the same host `bookclerkd` runs (control
//! plane, plugin deployments, jobs, events, scheduler, library links) but
//! binds no socket: not `daemon.listen`, not the node API's TLS listener.
//! The window's webview loads the SPA over its own custom scheme, and the
//! shell passes each request to [`EmbeddedHost::handle`], which calls the
//! router in-process.
//!
//! **Who the caller is.** The window belongs to the person signed in to this
//! computer's OS account (see `docs/adr/desktop-window-identity.md`). Until
//! the library has an active Owner (first run, or a computer that is only a
//! clerk of another library) every request is the operator: the host adds
//! the current operator token as `Authorization: Bearer`, read again for
//! every request so a rotation applies at once. After that the window is a
//! patron: the one whose portal session this branch keeps sealed in its
//! own database, which the host renews at every launch, or nobody until
//! they sign in on the app's own sign-in screen. The host keeps that patron's session cookies in a
//! [`session::WindowJar`] and adds them to each request; an elevation's
//! operator session lands there too, so elevation, impersonation and
//! sign-out work as on the web. Whatever the page sent as cookies,
//! `Authorization`, `Origin`, `Referer` or forwarding headers is dropped
//! first, and `Set-Cookie` never reaches the page, so the page holds no
//! credential of its own. With `daemon.auth.enabled = false` nothing is
//! added.
//!
//! **The kept session.** A sign-in in the window (a response that sets the
//! patron's portal session while no elevation is active) seals the session
//! for the next launch; creating the library's first Owner in the window
//! signs the window in as that Owner; signing out forgets it. Elevating,
//! impersonating and ending either never touch it. The router sees every
//! request as coming from [`EMBEDDED_ORIGIN`], a loopback origin, so a join
//! or a library card sign-in asks the library to send the window back there.
//!
//! **Other pages.** Because the token is added here, a page from anywhere
//! else must not be able to reach the scheme: a request whose `Origin` is
//! not one of [`APP_ORIGINS`] (or, for a write, whose `Referer` is from
//! elsewhere) is refused with `403` before routing.
//!
//! **Leaving the app.** The window shows only app pages, with one exception:
//! joining a library, or signing in to one with a library card, signs the
//! person in on the library's own site. From a successful
//! `POST /api/config/join/start` or `POST /api/config/clerk/start` until the
//! library sends the window back (or the sign-in expires),
//! [`EmbeddedHost::navigation`] lets the window open `https` pages; the
//! library's return to `http://localhost/join/callback` or
//! `http://localhost/clerk/callback` is handed back to the app scheme.
//!
//! **What the window cannot reach.** The request carries no peer address,
//! so routes that admit only a direct loopback connection (the tray
//! handoff) and the node API's transport policy refuse it. The node API
//! (`/api/node/...`) is for other nodes over TLS and is refused here before
//! routing. The live socket (`/api/live`) needs a WebSocket, which a custom
//! scheme cannot carry; it is refused here. The window hears the same
//! frames from [`EmbeddedHost::live`] instead, which the shell forwards to
//! the page over its own channel.
//!
//! **Responses** are collected whole, up to [`MAX_RESPONSE_BYTES`] and
//! [`RESPONSE_TIMEOUT`]; a custom scheme cannot stream.

pub mod session;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};
use axum::http::{HeaderValue, Method};
use axum::Router;
use bookclerk_window_policy::{
    classify_navigation, refusal, rewrite_request_headers, sign_in_ttl, Credential, Refusal,
    SIGN_IN_START_PATHS,
};
pub use bookclerk_window_policy::{
    live_refusal, LiveRefusal, Navigation, APP_ORIGINS, EMBEDDED_ORIGIN, MAIN_WINDOW_LABEL,
};
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

/// The route that creates the library's first Owner; in the window, its
/// success signs the window in as that Owner.
const BOOTSTRAP_PATH: &str = "/api/auth/bootstrap";

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
    /// successful join or library card start, cleared when the library
    /// sends it back.
    sign_in_open_until: Mutex<Option<Instant>>,
    /// The patron's session cookies, which the page never sees.
    jar: Mutex<session::WindowJar>,
    /// This branch's node id, which keys the window's sealed session, or
    /// `None` when the node identity cannot be read (the window then
    /// forgets its sign-in when it closes).
    node_id: Option<String>,
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
    /// cannot start (an interrupted shared branch join, a control plane that does
    /// not open, or operator auth that cannot be built).
    pub async fn start(options: EmbeddedOptions, syslog_identifier: &str) -> anyhow::Result<Self> {
        let config = bookclerk_config::Config::load(options.bookclerk_files, options.config)?;
        init_logging(&config, syslog_identifier, crate::ProcessRole::Desktop);
        let node_id = crate::node_queue::node_id(&config.paths().files_dir);
        let host = Self::from_host(start_host(config, HostMode::Embedded).await?, node_id);
        host.open_session().await;
        tracing::info!("host started embedded in the desktop window; no listener is bound");
        Ok(host)
    }

    /// Wraps a started host for the window: its state and router, with the
    /// library's sign-in pages closed.
    ///
    /// # Arguments
    ///
    /// * `host` - The host [`start_host`] (or a test) started.
    /// * `node_id` - This branch's node id, which keys the window's sealed
    ///   session, when it can be read.
    ///
    /// # Returns
    ///
    /// The embedded host, signed in as nobody until [`Self::open_session`].
    pub(crate) fn from_host(host: HostParts, node_id: Option<String>) -> Self {
        let HostParts { state, app, .. } = host;
        Self {
            state,
            app,
            sign_in_open_until: Mutex::new(None),
            jar: Mutex::new(session::WindowJar::default()),
            node_id,
        }
    }

    /// Signs the window in as the patron whose session this branch keeps.
    ///
    /// The kept session is swapped for a fresh one lasting
    /// [`session::DESKTOP_SESSION_TTL`]: the new one is sealed first and the
    /// old one ended after, so a failed write keeps the old, still valid,
    /// session. A kept session that was revoked or expired, or whose patron
    /// was disabled, is forgotten, and the window shows the sign-in screen.
    /// A row that cannot be read is left alone and logged.
    pub(crate) async fn open_session(&self) {
        if !self.state.auth_snapshot().await.enabled {
            return;
        }
        let Some(node_id) = self.node_id.as_deref() else {
            return;
        };
        let library = self.state.library_snapshot().await;
        let kept = match session::remembered(&library, node_id).await {
            Ok(Some(kept)) => kept,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(error = %err, "window session not read; the window opens signed out");
                return;
            }
        };
        let Some(user_id) = session::patron(&library, &kept).await else {
            tracing::info!("the window's session has ended; the window opens signed out");
            if let Err(err) = session::forget(&library, node_id).await {
                tracing::warn!(error = %err, "ended window session not forgotten");
            }
            return;
        };
        let current = match session::mint(&library, user_id).await {
            Some(fresh) => match session::remember(&library, node_id, &fresh).await {
                Ok(()) => {
                    session::end(&library, &kept).await;
                    fresh
                }
                Err(err) => {
                    tracing::warn!(error = %err, "window session not renewed; keeping it");
                    session::end(&library, &fresh).await;
                    kept
                }
            },
            None => kept,
        };
        self.update_jar(|jar| jar.portal = Some(current.expose().to_owned()));
        tracing::info!(user_id, "desktop window signed in with its kept session");
    }

    /// The same host as a new launch of the window would see it: the same
    /// state, router and node id, signed in as nobody.
    #[cfg(test)]
    pub(crate) fn relaunched(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            app: self.app.clone(),
            sign_in_open_until: Mutex::new(None),
            jar: Mutex::new(session::WindowJar::default()),
            node_id: self.node_id.clone(),
        }
    }

    /// Changes the jar under its lock.
    fn update_jar(&self, change: impl FnOnce(&mut session::WindowJar)) {
        match self.jar.lock() {
            Ok(mut jar) => change(&mut jar),
            Err(poisoned) => change(&mut poisoned.into_inner()),
        }
    }

    /// A copy of the jar.
    fn jar_now(&self) -> session::WindowJar {
        match self.jar.lock() {
            Ok(jar) => jar.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
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
        let (enabled, token) = (auth.enabled, auth.token.clone());
        drop(auth);
        let cookies = self.jar_now().cookie_header();
        let operator = enabled && cookies.is_none() && self.needs_owner().await;
        let credential = match (&cookies, enabled, operator) {
            (_, false, _) => Credential::None,
            (Some(cookies), true, _) => Credential::Cookies(cookies),
            (None, true, true) => Credential::Operator(&token),
            (None, true, false) => Credential::None,
        };
        let post = request.method() == Method::POST;
        let path = request.uri().path().to_owned();
        let (response, set_cookies) = serve(self.app.clone(), credential, request).await;
        let ok = response.status().is_success();
        if post && ok && SIGN_IN_START_PATHS.contains(&path.as_str()) {
            if let Some(ttl) = sign_in_ttl(response.body()) {
                self.set_sign_in_open(Some(Instant::now() + ttl));
            }
        }
        if enabled {
            if post && ok && path == BOOTSTRAP_PATH {
                self.sign_in_bootstrapped_owner(response.body()).await;
            }
            self.absorb(&set_cookies).await;
        }
        response
    }

    /// Whether the library has no active Owner yet, so the window acts as
    /// the operator (first run, or a clerk of another library). A failed
    /// read counts as having one: the window then asks for a sign-in rather
    /// than act as the operator.
    async fn needs_owner(&self) -> bool {
        let library = self.state.library_snapshot().await;
        matches!(library.count_active_owners().await, Ok(0))
    }

    /// Signs the window in as the Owner a bootstrap just created: the OS
    /// user setting up the library on this computer is that Owner.
    ///
    /// # Arguments
    ///
    /// * `body` - The bootstrap answer, which names the new Owner's `user_id`.
    async fn sign_in_bootstrapped_owner(&self, body: &[u8]) {
        let Some(user_id) = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|answer| answer.get("user_id")?.as_i64())
        else {
            return;
        };
        let library = self.state.library_snapshot().await;
        let Some(fresh) = session::mint(&library, user_id).await else {
            tracing::warn!(user_id, "the new Owner's window session could not be made");
            return;
        };
        self.remember(Some(&fresh)).await;
        self.update_jar(|jar| jar.portal = Some(fresh.expose().to_owned()));
        tracing::info!(
            user_id,
            "desktop window signed in as the library's new Owner"
        );
    }

    /// Applies a response's session cookies to the jar, and keeps the
    /// sealed session in step with a sign-in or a sign-out.
    ///
    /// The kept session changes only when the patron's own session changes
    /// while no operator session is held: impersonation and its end always
    /// run under an elevated session, so they never rewrite it.
    async fn absorb(&self, set_cookies: &[HeaderValue]) {
        if set_cookies.is_empty() {
            return;
        }
        let (before, after) = {
            let mut jar = match self.jar.lock() {
                Ok(jar) => jar,
                Err(poisoned) => poisoned.into_inner(),
            };
            let before = jar.clone();
            jar.absorb(set_cookies);
            (before, jar.clone())
        };
        if before.portal == after.portal || after.operator.is_some() {
            return;
        }
        let signed_in = after
            .portal
            .as_deref()
            .and_then(session::WindowSession::new);
        self.remember(signed_in.as_ref()).await;
    }

    /// Seals `session` for the next launch, or forgets the kept one for
    /// `None`.
    async fn remember(&self, session: Option<&session::WindowSession>) {
        let Some(node_id) = self.node_id.as_deref() else {
            return;
        };
        let library = self.state.library_snapshot().await;
        let written = match session {
            Some(session) => session::remember(&library, node_id, session).await,
            None => session::forget(&library, node_id).await,
        };
        if let Err(err) = written {
            tracing::warn!(error = %err, "window session not kept");
        }
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
    /// [`Navigation::SignInReturn`] also closes the sign-in window.
    pub fn navigation(&self, url: &str) -> Navigation {
        let open = self
            .sign_in_open_until
            .lock()
            .map(|until| until.is_some_and(|until| Instant::now() < until))
            .unwrap_or(false);
        let decision = classify_navigation(url, open);
        if matches!(decision, Navigation::SignInReturn(_)) {
            self.set_sign_in_open(None);
        }
        decision
    }

    /// Sets or clears the sign-in deadline.
    fn set_sign_in_open(&self, until: Option<Instant>) {
        if let Ok(mut slot) = self.sign_in_open_until.lock() {
            *slot = until;
        }
    }

    /// Feeds the live frames the SPA's `/api/live` socket would carry, for
    /// the shell to forward to the page: `hello` with every topic, then a
    /// `changed` frame for each change. The frames name topics only, so a
    /// patron's window hears the same ones the operator's would.
    ///
    /// Runs until `send` answers false. The frames name topics only, as on
    /// the socket.
    ///
    /// # Arguments
    ///
    /// * `send` - Takes each frame's JSON text; false stops the feed.
    pub async fn live(&self, send: impl FnMut(String) -> bool + Send) {
        crate::live::operator_feed(Arc::clone(&self.state), send).await;
    }

    /// Stops the integration watchers before the process exits, as
    /// `bookclerkd` does on shutdown.
    pub async fn shutdown(&self) {
        self.state.integrations.read().await.stop_all().await;
    }
}

/// Marks a request the window handed to the router. The window refuses
/// pages from other origins itself, before routing, so the cookie CSRF
/// check (`csrf::require_csrf_for_cookie_api`), which compares `Origin` with
/// `integrations.public_origin`, does not apply. Only the embedded host can
/// set it: it is a request extension, not a header.
#[derive(Debug, Clone, Copy)]
pub struct WindowRequest;

/// Routes one request through `app` under the embedded policy.
///
/// # Arguments
///
/// * `app` - The router.
/// * `credential` - What the window presents (see [`Credential`]).
/// * `request` - The webview's request.
///
/// # Returns
///
/// The collected response without `Set-Cookie` (see
/// [`EmbeddedHost::handle`]), and the `Set-Cookie` values taken from it for
/// the window's jar.
async fn serve(
    mut app: Router,
    credential: Credential<'_>,
    request: Request<Vec<u8>>,
) -> (Response<Vec<u8>>, Vec<HeaderValue>) {
    let (parts, body) = request.into_parts();
    if let Some(reason) = refusal(&parts.method, &parts.headers, parts.uri.path()) {
        tracing::debug!(?reason, path = parts.uri.path(), "embedded request refused");
        let status = match reason {
            Refusal::ForeignOrigin => StatusCode::FORBIDDEN,
            Refusal::NodeApi | Refusal::LiveSocket => StatusCode::NOT_FOUND,
        };
        return (status_only(status), Vec::new());
    }
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| String::from("/"), |pq| pq.as_str().to_owned());
    let mut routed = Request::new(Body::from(body));
    *routed.method_mut() = parts.method;
    *routed.uri_mut() = match path_and_query.parse() {
        Ok(uri) => uri,
        Err(_) => return (status_only(StatusCode::BAD_REQUEST), Vec::new()),
    };
    let headers = rewrite_request_headers(&parts.headers, credential);
    match credential {
        Credential::Operator(_) if !headers.contains_key(header::AUTHORIZATION) => {
            // An operator token is URL-safe text; one that is not a valid
            // header value leaves the request unauthenticated (fail closed).
            tracing::error!("operator token is not a valid header value");
        }
        Credential::Cookies(_) if !headers.contains_key(header::COOKIE) => {
            tracing::error!("the window's session cookies are not a valid header value");
        }
        Credential::None | Credential::Operator(_) | Credential::Cookies(_) => {}
    }
    *routed.headers_mut() = headers;
    routed.extensions_mut().insert(WindowRequest);

    let answer = async {
        let response = match app.call(routed).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
        let (mut parts, body) = response.into_parts();
        let set_cookies: Vec<HeaderValue> = match parts.headers.entry(header::SET_COOKIE) {
            axum::http::header::Entry::Occupied(entry) => entry.remove_entry_mult().1.collect(),
            axum::http::header::Entry::Vacant(_) => Vec::new(),
        };
        match axum::body::to_bytes(body, MAX_RESPONSE_BYTES).await {
            Ok(bytes) => (Response::from_parts(parts, bytes.to_vec()), set_cookies),
            Err(err) => {
                tracing::warn!(error = %err, "embedded response body could not be collected");
                (status_only(StatusCode::BAD_GATEWAY), Vec::new())
            }
        }
    };
    tokio::time::timeout(RESPONSE_TIMEOUT, answer)
        .await
        .unwrap_or_else(|_| (status_only(StatusCode::GATEWAY_TIMEOUT), Vec::new()))
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
        let (response, set_cookies) = serve(
            echo_router(),
            Credential::Operator("op-token"),
            page_request("/api/echo?x=1"),
        )
        .await;
        assert_eq!(set_cookies.len(), 1);
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
        let (response, _) = serve(echo_router(), Credential::None, page_request("/api/echo")).await;
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
            let (response, _) =
                serve(echo_router(), Credential::Operator("t"), page_request(path)).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(response.body().is_empty(), "{path}");
        }
        // A sibling route that merely shares a prefix is not refused.
        let (response, _) = serve(
            echo_router(),
            Credential::Operator("t"),
            page_request("/api/nodes"),
        )
        .await;
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
            let (response, _) = serve(
                app(),
                Credential::Operator("t"),
                with(origin, referer, method),
            )
            .await;
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
            let (response, _) = serve(
                app(),
                Credential::Operator("t"),
                with(origin, referer, method),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{origin:?} {referer:?} {method}"
            );
        }
    }

    #[tokio::test]
    async fn a_join_or_library_card_start_opens_the_sign_in_until_the_library_sends_the_window_back(
    ) {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let (state, _store) = crate::node_api::tests::test_state(&files).await;
        let started = || async {
            axum::Json(serde_json::json!({
                "authorize_url": "https://library.example/sign-in",
                "expires_in_secs": 600,
            }))
        };
        let app = echo_router()
            .route(bookclerk_window_policy::JOIN_START_PATH, post(started))
            .route(bookclerk_window_policy::CLERK_START_PATH, post(started));
        let host = EmbeddedHost {
            state,
            app,
            sign_in_open_until: Mutex::new(None),
            jar: Mutex::new(session::WindowJar::default()),
            node_id: None,
        };
        assert!(format!("{host:?}").starts_with("EmbeddedHost"));
        let sign_in = "https://library.example/sign-in";
        assert_eq!(host.navigation(sign_in), Navigation::Refuse);

        let echoed = host.handle(page_request("/api/echo")).await;
        assert_eq!(echoed.status(), StatusCode::OK);
        assert_eq!(host.navigation(sign_in), Navigation::Refuse);

        for (start_path, callback) in [
            ("/api/config/join/start", "/join/callback"),
            ("/api/config/clerk/start", "/clerk/callback"),
        ] {
            let mut start = page_request(start_path);
            *start.method_mut() = axum::http::Method::POST;
            let answer = host.handle(start).await;
            assert_eq!(answer.status(), StatusCode::OK, "{start_path}");
            assert_eq!(host.navigation(sign_in), Navigation::SignIn, "{start_path}");
            assert_eq!(
                host.navigation(&format!("http://localhost{callback}?code=c")),
                Navigation::SignInReturn(format!("{callback}?code=c")),
            );
            assert_eq!(host.navigation(sign_in), Navigation::Refuse, "{start_path}");
        }
        host.shutdown().await;
    }

    #[tokio::test]
    async fn the_window_hears_live_frames_without_the_socket() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let (state, _store) = crate::node_api::tests::test_state(&files).await;
        let host = EmbeddedHost {
            state,
            app: echo_router(),
            sign_in_open_until: Mutex::new(None),
            jar: Mutex::new(session::WindowJar::default()),
            node_id: None,
        };
        let mut frames = Vec::new();
        // Stop after the first frame: `hello`, naming every topic.
        host.live(|text| {
            frames.push(text);
            false
        })
        .await;
        assert_eq!(frames.len(), 1);
        let hello: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(hello["type"], "hello");
        assert_eq!(
            hello["topics"].as_array().map(Vec::len),
            Some(bookclerk_library::control_plane::LiveTopic::ALL.len())
        );
    }
}
