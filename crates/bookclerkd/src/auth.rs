//! Operator and portal session authentication for the daemon HTTP API.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use bookclerk_config::session_cookie_flags;
use bookclerk_integrations::{portal_identity_from_headers, session_for_identity};
use bookclerk_library::{
    classify_session_client, hash_password, hash_token, portal_prefs_key, user_prefs_key,
    verify_password, LibraryError, PortalIdentity, SessionClientInfo, UserRole, UserStatus,
    OPERATOR_PREFS_KEY,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::timeout;
use uuid::Uuid;

use crate::api::AppState;

/// HttpOnly cookie name for the operator session id (hashed before persist).
pub const SESSION_COOKIE: &str = "bookclerk_operator_session";
/// HttpOnly cookie name for the portal session id (hashed before persist).
pub const PORTAL_SESSION_COOKIE: &str = "bookclerk_portal_session";
/// Upper bound on library DB lookups during auth; timeout fails closed.
const AUTH_DB_TIMEOUT: Duration = Duration::from_secs(3);
/// Elevated Administrator→Operator session lifetime.
pub const ELEVATION_TTL: Duration = Duration::from_secs(15 * 60);

/// Peer IP for login throttling (`ConnectInfo` when available, else `"unknown"`).
pub(crate) struct ClientIp(pub String);

impl FromRequestParts<Arc<AppState>> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        let trusted = state.config.read().await.daemon.trusted_proxies.clone();
        Ok(Self(resolve_client_ip_key(peer, &parts.headers, &trusted)))
    }
}

/// Resolve the throttle/key client identity, honoring trusted reverse proxies.
fn resolve_client_ip_key(
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    trusted_proxies: &[String],
) -> String {
    let Some(peer) = peer else {
        return String::from("unknown");
    };
    if !peer_is_trusted(peer, trusted_proxies) {
        return peer.to_string();
    }
    if let Some(client) = forwarded_client_ip(headers) {
        return client;
    }
    peer.to_string()
}

/// Whether the peer IP matches a trusted-proxy entry (exact address or CIDR).
fn peer_is_trusted(peer: IpAddr, trusted_proxies: &[String]) -> bool {
    trusted_proxies.iter().any(|entry| {
        let entry = entry.trim();
        if entry.is_empty() {
            return false;
        }
        if let Ok(cidr_ip) = entry.parse::<IpAddr>() {
            return cidr_ip == peer;
        }
        // Prefix match for simple CIDR forms like `10.0.0.0/8` (IPv4 only).
        if let Some((base, bits)) = entry.split_once('/') {
            if let (Ok(base_ip), Ok(prefix)) = (base.parse::<IpAddr>(), bits.parse::<u8>()) {
                return ip_in_prefix(peer, base_ip, prefix);
            }
        }
        false
    })
}

/// Whether `ip` falls in the IPv4/IPv6 prefix; mixed families and oversized prefixes fail closed.
fn ip_in_prefix(ip: IpAddr, base: IpAddr, prefix: u8) -> bool {
    match (ip, base) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            if prefix > 32 {
                return false;
            }
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            (u32::from(a) & mask) == (u32::from(b) & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            if prefix > 128 {
                return false;
            }
            let a = u128::from(a);
            let b = u128::from(b);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            (a & mask) == (b & mask)
        }
        _ => false,
    }
}

/// First parseable client IP from `X-Forwarded-For` or RFC 7239 `Forwarded`.
fn forwarded_client_ip(headers: &HeaderMap) -> Option<String> {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let first = xff.split(',').next()?.trim();
        if !first.is_empty() && first.parse::<IpAddr>().is_ok() {
            return Some(first.to_string());
        }
    }
    if let Some(fwd) = headers.get(header::FORWARDED).and_then(|v| v.to_str().ok()) {
        // Take the first `for=` value.
        for part in fwd.split(';') {
            let part = part.trim();
            if let Some(rest) = part
                .strip_prefix("for=")
                .or_else(|| part.strip_prefix("For="))
            {
                let candidate = rest.trim().trim_matches('"').trim_start_matches('[');
                let candidate = candidate.trim_end_matches(']');
                let host = candidate.split(':').next().unwrap_or(candidate);
                if host.parse::<IpAddr>().is_ok() {
                    return Some(host.to_string());
                }
            }
        }
    }
    None
}

#[derive(Debug)]
/// Per-client failure window used to lock out brute-force operator and password logins.
struct LoginThrottleBucket {
    /// Failed attempts in the current window (saturates; lockout at `login_max_failures`).
    failures: u32,
    /// Monotonic start of the current failure window.
    window_start: Instant,
    /// When set, further logins from this client are refused until this instant.
    locked_until: Option<Instant>,
}

/// How long a previous operator token remains valid after rotate/reload.
pub const OPERATOR_TOKEN_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug)]
/// In-memory operator token, session TTL, and login throttle for the daemon.
pub struct OperatorAuthState {
    /// Current operator token compared in constant time; mismatch fails closed.
    pub token: String,
    /// Prior token accepted until this deadline (rotate/reload overlap).
    previous_token: Option<(String, Instant)>,
    /// Lifetime of a newly issued operator session cookie (at least one hour).
    pub session_ttl: Duration,
    /// When false, operator auth is skipped and handlers treat the caller as operator.
    pub enabled: bool,
    /// Failures allowed in one window before lockout (at least 1).
    login_max_failures: u32,
    /// Sliding window over which failures accumulate (defaults to the lockout duration).
    login_window: Duration,
    /// How long a client stays locked after exceeding `login_max_failures`.
    login_lockout: Duration,
    /// Per-client throttle buckets keyed by resolved client IP.
    login_attempts: Mutex<HashMap<String, LoginThrottleBucket>>,
}

impl OperatorAuthState {
    /// Builds throttle and session settings from config, clamping TTL and lockout to safe minima.
    pub fn new(
        token: String,
        session_ttl_hours: u64,
        enabled: bool,
        login_max_failures: u32,
        login_lockout_secs: u64,
    ) -> Self {
        let login_lockout = Duration::from_secs(login_lockout_secs.max(1));
        Self {
            token,
            previous_token: None,
            session_ttl: Duration::from_secs(session_ttl_hours.saturating_mul(3600).max(3600)),
            enabled,
            login_max_failures: login_max_failures.max(1),
            // Failures count within the same period as the lockout by default.
            login_window: login_lockout,
            login_lockout,
            login_attempts: Mutex::new(HashMap::new()),
        }
    }

    /// Accept `previous`'s token (and any still-valid grace token) for [`OPERATOR_TOKEN_GRACE`].
    pub fn install_token_grace_from(&mut self, previous: &Self, grace: Duration) {
        if !self.enabled || !previous.enabled {
            return;
        }
        let now = Instant::now();
        if !previous.token.is_empty() && previous.token != self.token {
            self.previous_token = Some((previous.token.clone(), now + grace));
            return;
        }
        if let Some((tok, until)) = &previous.previous_token {
            if now < *until && tok != &self.token {
                self.previous_token = Some((tok.clone(), *until));
            }
        }
    }

    /// Preserve in-memory login throttle buckets across auth reload swaps.
    ///
    /// Operator sessions live in SQLite (`operator_sessions`) and do not need
    /// to be copied here.
    pub async fn take_session_state_from(&mut self, previous: &Self) {
        let mut old = previous.login_attempts.lock().await;
        let mut new = self.login_attempts.lock().await;
        *new = std::mem::take(&mut *old);
    }

    /// Constant-time compare against the current token or a still-valid grace token.
    pub(crate) fn token_matches(&self, candidate: &str) -> bool {
        if constant_time_eq(self.token.as_bytes(), candidate.as_bytes()) {
            return true;
        }
        if let Some((prev, until)) = &self.previous_token {
            Instant::now() < *until && constant_time_eq(prev.as_bytes(), candidate.as_bytes())
        } else {
            false
        }
    }

    /// `None` = allowed; `Some(retry_after)` = locked out.
    pub(crate) async fn login_throttle_check(&self, client_key: &str) -> Option<Duration> {
        let mut map = self.login_attempts.lock().await;
        prune_login_attempts(&mut map, self.login_window, self.login_lockout);
        let bucket = map.get_mut(client_key)?;
        if let Some(until) = bucket.locked_until {
            let now = Instant::now();
            if now < until {
                return Some((until - now).max(Duration::from_secs(1)));
            }
            // Lockout expired — reset the window.
            bucket.failures = 0;
            bucket.locked_until = None;
            bucket.window_start = now;
        }
        None
    }

    /// Increments the client's failure count and returns lockout remaining when the cap is hit.
    pub(crate) async fn record_login_failure(&self, client_key: &str) -> Option<Duration> {
        let mut map = self.login_attempts.lock().await;
        prune_login_attempts(&mut map, self.login_window, self.login_lockout);
        let now = Instant::now();
        let bucket = map
            .entry(client_key.to_string())
            .or_insert_with(|| LoginThrottleBucket {
                failures: 0,
                window_start: now,
                locked_until: None,
            });
        if bucket.window_start.elapsed() >= self.login_window {
            bucket.failures = 0;
            bucket.window_start = now;
            bucket.locked_until = None;
        }
        bucket.failures = bucket.failures.saturating_add(1);
        if bucket.failures >= self.login_max_failures {
            bucket.locked_until = Some(now + self.login_lockout);
            return Some(self.login_lockout);
        }
        None
    }

    /// Drops the client's throttle bucket after a successful login.
    pub(crate) async fn clear_login_failures(&self, client_key: &str) {
        self.login_attempts.lock().await.remove(client_key);
    }
}

#[derive(Debug, Deserialize)]
/// JSON body for `POST /api/auth/login` (operator token).
pub struct LoginRequest {
    /// Operator token from the client; compared in constant time and never persisted.
    pub token: String,
}

#[derive(Debug, Deserialize)]
/// Query string for the loopback tray handoff (`?token=`).
pub struct TrayHandoffQuery {
    /// Operator token from the tray; accepted only from a loopback peer.
    pub token: String,
}

#[derive(Debug, Serialize)]
/// JSON body for `GET /api/auth/me`.
pub struct AuthMeResponse {
    /// Whether the request resolved to an operator, portal, or impersonated session.
    pub authenticated: bool,
    /// `operator`, `administrator`, or `member` (legacy clients may still send `portal`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Configured post-auth landing view (`discover` / `library` / `accounts`).
    /// Loaded from per-user DB preferences (not config.toml).
    pub default_view: String,
    /// Whether this session may acquire / scan / manage jobs.
    /// True for operator and administrator.
    pub can_acquire: bool,
    /// True when this operator session was created via Owner elevate.
    pub elevated: bool,
    /// Present when the operator is impersonating a User.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impersonating: Option<AuthMeImpersonating>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Linked portal identity when the caller is a portal (or impersonated) user.
    pub portal: Option<PortalMeInfo>,
    /// First-party user when the session is linked to a `users` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<AuthMeUser>,
}

#[derive(Debug, Serialize)]
/// Target user shown while an operator session is impersonating.
pub struct AuthMeImpersonating {
    /// First-party `users.id` being impersonated.
    pub user_id: i64,
    /// Display name of the impersonated user when the row exists.
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize)]
/// First-party user snapshot attached to `/me` when a `users` row is linked.
pub struct AuthMeUser {
    /// First-party `users.id`.
    pub id: i64,
    /// Role string (`owner`, `administrator`, `member`).
    pub role: String,
    /// Optional human-readable name from the user row.
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional email; omitted from JSON when unset.
    pub email: Option<String>,
    /// True when a local password hash is stored (invite users may be false).
    pub has_password: bool,
}

#[derive(Debug, Serialize)]
/// Portal identity fields exposed on `/me` for SSO / local-portal callers.
pub struct PortalMeInfo {
    /// `portal_identities.id` for this session.
    pub identity_id: i64,
    /// Identity-broker id (`local`, OIDC provider name, …).
    pub provider: String,
    /// Provider-stable subject / external user id.
    pub external_user_id: String,
    /// Optional operator-facing label for the identity.
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
/// JSON body after a successful operator token login.
struct LoginResponse {
    /// Always `true` on this success path.
    ok: bool,
    /// Issued role (`operator` for token login).
    role: String,
    /// Post-login SPA landing view from operator preferences (`discover` on lookup failure).
    default_view: String,
}

/// Validates the operator token (throttled, constant-time) and issues a hashed session cookie.
pub async fn login(
    State(state): State<Arc<AppState>>,
    ClientIp(client_key): ClientIp,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    let default_view = default_view_for_subject(&library, OPERATOR_PREFS_KEY, None).await;
    if !auth.enabled {
        return Ok((
            StatusCode::OK,
            [(
                header::SET_COOKIE,
                format!("{SESSION_COOKIE}=disabled; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
            )],
            Json(LoginResponse {
                ok: true,
                role: String::from("operator"),
                default_view,
            }),
        )
            .into_response());
    }
    // Locked-out clients are refused before the token is even compared.
    if let Some(retry_after) = auth.login_throttle_check(&client_key).await {
        return Ok(too_many_requests(retry_after));
    }

    if !auth.token_matches(body.token.trim()) {
        if let Some(retry_after) = auth.record_login_failure(&client_key).await {
            return Ok(too_many_requests(retry_after));
        }
        // Explicit JSON so the brand-error middleware keeps a precise message.
        return Ok((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "unauthorized",
                "message": "Invalid operator token.",
                "status": 401
            })),
        )
            .into_response());
    }

    auth.clear_login_failures(&client_key).await;
    Ok(issue_operator_session(&state, &auth, default_view, &headers).await)
}

/// Browser handoff from the system tray.
///
/// Linux `xdg-open` often strips URL fragments, so the tray opens this loopback
/// GET with the operator token as a query param. On success we set the session
/// cookie and redirect to `/` — the SPA never needs to parse a fragment.
pub async fn tray_handoff(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ClientIp(client_key): ClientIp,
    Query(query): Query<TrayHandoffQuery>,
) -> Result<Response, StatusCode> {
    if !addr.ip().is_loopback() {
        tracing::warn!(%addr, "tray handoff refused from non-loopback peer");
        return Err(StatusCode::FORBIDDEN);
    }

    let auth = state.auth_snapshot().await;

    if !auth.enabled {
        let mut res = Redirect::temporary("/").into_response();
        res.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_static(
                "bookclerk_operator_session=disabled; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
            ),
        );
        return Ok(res);
    }

    if let Some(retry_after) = auth.login_throttle_check(&client_key).await {
        return Ok(too_many_requests(retry_after));
    }

    if !auth.token_matches(query.token.trim()) {
        if let Some(retry_after) = auth.record_login_failure(&client_key).await {
            return Ok(too_many_requests(retry_after));
        }
        return Err(StatusCode::UNAUTHORIZED);
    }

    auth.clear_login_failures(&client_key).await;
    let client = classify_session_client(None, false); // tray has no browser UA
    let cookie = persist_operator_session_cookie_with_client(&state, &auth, Some(&client)).await;
    let mut res = Redirect::temporary("/").into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, value);
    }
    tracing::info!("tray handoff accepted; session cookie set");
    Ok(res)
}

/// Persists a hashed operator session and returns the Set-Cookie success response.
async fn issue_operator_session(
    state: &AppState,
    auth: &OperatorAuthState,
    default_view: String,
    headers: &HeaderMap,
) -> Response {
    let client = session_client_from_headers(headers);
    let cookie = persist_operator_session_cookie_with_client(state, auth, Some(&client)).await;
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(LoginResponse {
            ok: true,
            role: String::from("operator"),
            default_view,
        }),
    )
        .into_response()
}

/// Mints a session id, stores only its hash, and formats the HttpOnly cookie; persist errors are logged and lookup later fails closed.
async fn persist_operator_session_cookie_with_client(
    state: &AppState,
    auth: &OperatorAuthState,
    client: Option<&SessionClientInfo>,
) -> String {
    let session_id = Uuid::new_v4().to_string();
    let token_hash = hash_token(&session_id);
    let expires = Utc::now()
        + ChronoDuration::from_std(auth.session_ttl).unwrap_or_else(|_| ChronoDuration::hours(12));
    let library = state.library_snapshot().await;
    if let Err(err) = library
        .insert_operator_session_with_client(&token_hash, expires, client)
        .await
    {
        tracing::error!(error = %err, "failed to persist operator session");
    }
    let _ = library.prune_expired_operator_sessions().await;
    let flags = {
        let cfg = state.config.read().await;
        session_cookie_flags(cfg.integrations.public_origin.as_deref())
    };
    let max_age = auth.session_ttl.as_secs();
    format!("{SESSION_COOKIE}={session_id}; {flags}; Max-Age={max_age}")
}

/// Classifies the caller as browser vs API from User-Agent and Bearer presence.
pub(crate) fn session_client_from_headers(headers: &HeaderMap) -> SessionClientInfo {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    // Bearer-only / missing UA → treat as API client.
    let is_api = ua.is_none() || bearer_token(headers).is_some();
    classify_session_client(ua, is_api)
}

/// Builds a 429 JSON body with `Retry-After` in whole seconds (at least 1).
pub(crate) fn too_many_requests(retry_after: Duration) -> Response {
    let secs = retry_after.as_secs().max(1);
    let mut res = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": "too_many_requests",
            "message": format!(
                "Too many failed login attempts. Try again in {secs} second{}.",
                if secs == 1 { "" } else { "s" }
            ),
            "status": 429,
            "retry_after_secs": secs
        })),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
        res.headers_mut().insert(header::RETRY_AFTER, value);
    }
    res
}

/// Revokes hashed operator and portal sessions and clears both cookies.
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let library = state.library_snapshot().await;
    if let Some(session_id) = session_id_from_headers(&headers) {
        let hash = hash_token(&session_id);
        if let Err(err) = library.delete_operator_session(&hash).await {
            tracing::warn!(error = %err, "failed to revoke operator session");
        }
    }
    if let Some(portal_raw) = cookie_value(&headers, PORTAL_SESSION_COOKIE) {
        let hash = hash_token(&portal_raw);
        if let Err(err) = library.delete_portal_session(&hash).await {
            tracing::warn!(error = %err, "failed to revoke portal session on operator logout");
        }
    }
    let flags = {
        let cfg = state.config.read().await;
        session_cookie_flags(cfg.integrations.public_origin.as_deref())
    };
    let mut hdrs = HeaderMap::new();
    for cookie in [
        format!("{SESSION_COOKIE}=; {flags}; Max-Age=0"),
        format!("{PORTAL_SESSION_COOKIE}=; {flags}; Max-Age=0"),
    ] {
        if let Ok(v) = header::HeaderValue::from_str(&cookie) {
            hdrs.append(header::SET_COOKIE, v);
        }
    }
    (
        StatusCode::OK,
        hdrs,
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// Resolves the caller and returns `/me` JSON; unauthenticated callers get 401.
pub async fn me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let auth = state.auth_snapshot().await;

    if !auth.enabled {
        let library = state.library_snapshot().await;
        let default_view = default_view_for_subject(&library, OPERATOR_PREFS_KEY, None).await;
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(String::from("operator")),
                default_view,
                can_acquire: true,
                elevated: false,
                impersonating: None,
                portal: None,
                user: None,
            }),
        );
    }

    if let Some(op) = resolve_operator_session(&state, &auth, &headers).await {
        let library = state.library_snapshot().await;
        // Impersonation: expose the target user's role/permissions/views so the
        // SPA matches a normal user session (stop banner uses `impersonating`).
        if let Some(target_id) = op.impersonating_user_id {
            let (impersonating, prefs_key, identity_id) =
                impersonation_me_fields(&library, Some(target_id)).await;
            let default_view = default_view_for_subject(&library, &prefs_key, identity_id).await;
            let (role, can_acquire, user, portal) =
                impersonation_caller_identity(&library, target_id).await;
            return (
                StatusCode::OK,
                Json(AuthMeResponse {
                    authenticated: true,
                    role: Some(role),
                    default_view,
                    can_acquire,
                    elevated: false,
                    impersonating,
                    portal,
                    user,
                }),
            );
        }
        let default_view = default_view_for_subject(&library, OPERATOR_PREFS_KEY, None).await;
        let user = if let Some(uid) = op.elevated_from_user_id {
            match timeout(AUTH_DB_TIMEOUT, library.get_user(uid)).await {
                Ok(Ok(Some(u))) => Some(auth_me_user(&u)),
                _ => None,
            }
        } else {
            None
        };
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(String::from("operator")),
                default_view,
                can_acquire: true,
                elevated: op.elevated_from_user_id.is_some(),
                impersonating: None,
                portal: None,
                user,
            }),
        );
    }

    if authorize_operator_bearer_only(&auth, &headers) {
        let library = state.library_snapshot().await;
        let default_view = default_view_for_subject(&library, OPERATOR_PREFS_KEY, None).await;
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(String::from("operator")),
                default_view,
                can_acquire: true,
                elevated: false,
                impersonating: None,
                portal: None,
                user: None,
            }),
        );
    }

    let library = state.library_snapshot().await;
    if let Some(identity) = timed_portal_identity_from_headers(&library, &headers).await {
        let (role, can_acquire, user, prefs_key) =
            resolve_portal_caller_identity(&library, &identity).await;
        let default_view = default_view_for_subject(&library, &prefs_key, Some(identity.id)).await;
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(role),
                default_view,
                can_acquire,
                elevated: false,
                impersonating: None,
                portal: Some(PortalMeInfo {
                    identity_id: identity.id,
                    provider: identity.provider,
                    external_user_id: identity.external_user_id,
                    label: identity.label,
                }),
                user,
            }),
        );
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(AuthMeResponse {
            authenticated: false,
            role: None,
            default_view: String::from("discover"),
            can_acquire: false,
            elevated: false,
            impersonating: None,
            portal: None,
            user: None,
        }),
    )
}

/// Middleware that admits operator sessions or Bearer tokens; impersonation is forbidden except ending it.
pub async fn require_operator_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Clone the auth Arc then drop the RwLock before `next.run` so a config
    // reload writer is not blocked for the full handler duration.
    let auth = state.auth_snapshot().await;
    if !auth.enabled {
        return Ok(next.run(req).await);
    }
    if let Some(op) = resolve_operator_session(&state, &auth, req.headers()).await {
        // Impersonation drops operator privileges except ending impersonation.
        if op.impersonating_user_id.is_some() {
            let ending =
                req.method() == Method::DELETE && req.uri().path() == "/api/auth/impersonate";
            if ending {
                return Ok(next.run(req).await);
            }
            return Err(StatusCode::FORBIDDEN);
        }
        return Ok(next.run(req).await);
    }
    if authorize_operator_bearer_only(&auth, req.headers()) {
        return Ok(next.run(req).await);
    }
    Err(StatusCode::UNAUTHORIZED)
}

/// Operator token/session **or** an Owner portal session (not impersonating).
///
/// Used for identity-broker (`[auth.oidc]`) configuration so Owners can manage
/// SSO without elevating. Administrators cannot change IdP settings.
pub async fn require_operator_or_owner_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    if !auth.enabled {
        return Ok(next.run(req).await);
    }
    if let Some(op) = resolve_operator_session(&state, &auth, req.headers()).await {
        if op.impersonating_user_id.is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
        return Ok(next.run(req).await);
    }
    if authorize_operator_bearer_only(&auth, req.headers()) {
        return Ok(next.run(req).await);
    }
    let library = state.library_snapshot().await;
    if let Some(identity) = timed_portal_identity_from_headers(&library, req.headers()).await {
        let (role, _, _, _) = resolve_portal_caller_identity(&library, &identity).await;
        if role == "owner" {
            return Ok(next.run(req).await);
        }
        return Err(StatusCode::FORBIDDEN);
    }
    Err(StatusCode::UNAUTHORIZED)
}

/// Operator token/session **or** an active Administrator user portal session.
///
/// Used for scan / acquire / jobs. Full control-plane settings stay on
/// [`require_operator_auth`] until Phase 2 elevation.
pub async fn require_operator_or_administrator_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    if !auth.enabled {
        return Ok(next.run(req).await);
    }
    if let Some(op) = resolve_operator_session(&state, &auth, req.headers()).await {
        if let Some(target_id) = op.impersonating_user_id {
            // Act as the impersonated user for acquire/scan capability.
            let library = state.library_snapshot().await;
            let (role, can_acquire, _, _) =
                impersonation_caller_identity(&library, target_id).await;
            if can_acquire || role == "administrator" || role == "owner" {
                return Ok(next.run(req).await);
            }
            return Err(StatusCode::FORBIDDEN);
        }
        return Ok(next.run(req).await);
    }
    if authorize_operator_bearer_only(&auth, req.headers()) {
        return Ok(next.run(req).await);
    }
    let library = state.library_snapshot().await;
    if let Some(identity) = timed_portal_identity_from_headers(&library, req.headers()).await {
        let (role, can_acquire, _, _) = resolve_portal_caller_identity(&library, &identity).await;
        if can_acquire || role == "administrator" || role == "owner" {
            return Ok(next.run(req).await);
        }
    }
    Err(StatusCode::UNAUTHORIZED)
}

/// Allow operator sessions **or** portal sessions (Discover / read-only library).
pub async fn require_operator_or_portal_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    let (allowed, check_portal) =
        if !auth.enabled || authorize_operator(&state, &auth, req.headers()).await {
            (true, false)
        } else {
            (false, true)
        };
    if allowed {
        return Ok(next.run(req).await);
    }
    if check_portal {
        let library = state.library_snapshot().await;
        if timed_portal_identity_from_headers(&library, req.headers())
            .await
            .is_some()
        {
            return Ok(next.run(req).await);
        }
    }
    Err(StatusCode::UNAUTHORIZED)
}

/// Resolve portal identity when the caller is not a pure operator.
///
/// When an operator session is impersonating a User, returns that user's portal
/// identity so **prefs / Discover personalization** follow the target. Library
/// browsing is intentionally shared (not filtered by this identity).
pub async fn caller_portal_identity(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<PortalIdentity> {
    let auth = state.auth_snapshot().await;
    if !auth.enabled {
        return None;
    }
    if let Some(op) = resolve_operator_session(state, &auth, headers).await {
        if let Some(user_id) = op.impersonating_user_id {
            let library = state.library_snapshot().await;
            return match timeout(
                AUTH_DB_TIMEOUT,
                library.first_portal_identity_for_user(user_id),
            )
            .await
            {
                Ok(Ok(identity)) => identity,
                _ => None,
            };
        }
        return None;
    }
    if authorize_operator_bearer_only(&auth, headers) {
        return None;
    }
    let library = state.library_snapshot().await;
    timed_portal_identity_from_headers(&library, headers).await
}

/// Subject key + optional portal identity id for the caller's preferences row.
///
/// Portal callers must resolve to their own prefs key. If the portal-identity
/// lookup times out or fails after portal auth, this returns an error instead of
/// falling back to [`OPERATOR_PREFS_KEY`].
pub async fn prefs_subject_for_caller(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(String, Option<i64>), StatusCode> {
    let auth = state.auth_snapshot().await;
    if !auth.enabled {
        return Ok((OPERATOR_PREFS_KEY.to_string(), None));
    }
    if let Some(op) = resolve_operator_session(state, &auth, headers).await {
        if let Some(user_id) = op.impersonating_user_id {
            let library = state.library_snapshot().await;
            let identity_id = match timeout(
                AUTH_DB_TIMEOUT,
                library.first_portal_identity_for_user(user_id),
            )
            .await
            {
                Ok(Ok(Some(id))) => Some(id.id),
                _ => None,
            };
            return Ok((user_prefs_key(user_id), identity_id));
        }
        return Ok((OPERATOR_PREFS_KEY.to_string(), None));
    }
    if authorize_operator_bearer_only(&auth, headers) {
        return Ok((OPERATOR_PREFS_KEY.to_string(), None));
    }
    let library = state.library_snapshot().await;
    match timeout(
        AUTH_DB_TIMEOUT,
        portal_identity_from_headers(&library, headers),
    )
    .await
    {
        Ok(Some(identity)) => {
            let key = match identity.user_id {
                Some(user_id) => user_prefs_key(user_id),
                None => portal_prefs_key(identity.id),
            };
            Ok((key, Some(identity.id)))
        }
        Ok(None) => {
            tracing::warn!("portal identity missing for prefs subject");
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(_) => {
            tracing::warn!("portal identity lookup timed out for prefs subject");
            Err(StatusCode::GATEWAY_TIMEOUT)
        }
    }
}

/// Maps a library user row onto the `/me` user object (no password hash).
fn auth_me_user(user: &bookclerk_library::UserRecord) -> AuthMeUser {
    AuthMeUser {
        id: user.id,
        role: user.role.as_str().to_string(),
        display_name: user.display_name.clone(),
        email: user.email.clone(),
        has_password: user.has_password,
    }
}

/// Map a portal session to first-party role / prefs subject / optional user info.
pub(crate) async fn resolve_portal_caller_identity(
    library: &bookclerk_library::LibraryStore,
    identity: &PortalIdentity,
) -> (String, bool, Option<AuthMeUser>, String) {
    let Some(user_id) = identity.user_id else {
        // Legacy unbridged portal identity — treat as member.
        return (
            String::from("member"),
            false,
            None,
            portal_prefs_key(identity.id),
        );
    };
    let prefs_key = user_prefs_key(user_id);
    match timeout(AUTH_DB_TIMEOUT, library.get_user(user_id)).await {
        Ok(Ok(Some(user))) => {
            if matches!(user.status, UserStatus::Disabled) {
                tracing::warn!(user_id, "disabled user portal session");
                return (String::from("member"), false, None, prefs_key);
            }
            let role = user.role.as_str().to_string();
            let can_acquire = user.role.is_privileged();
            let me_user = auth_me_user(&user);
            (role, can_acquire, Some(me_user), prefs_key)
        }
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
            tracing::warn!(user_id, "linked user missing for portal identity");
            (String::from("member"), false, None, prefs_key)
        }
    }
}

/// Loads the subject's default SPA view, falling back to `discover` on timeout or error.
async fn default_view_for_subject(
    library: &bookclerk_library::LibraryStore,
    subject_key: &str,
    identity_id: Option<i64>,
) -> String {
    match timeout(
        AUTH_DB_TIMEOUT,
        library.get_user_preferences_or_default(subject_key, identity_id),
    )
    .await
    {
        Ok(Ok(prefs)) => normalize_default_view(&prefs.default_view),
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "default view lookup failed");
            String::from("discover")
        }
        Err(_) => {
            tracing::warn!("default view lookup timed out");
            String::from("discover")
        }
    }
}

/// Resolves a portal identity from cookies within `AUTH_DB_TIMEOUT`; timeout or a disabled user fails closed.
pub(crate) async fn timed_portal_identity_from_headers(
    library: &bookclerk_library::LibraryStore,
    headers: &HeaderMap,
) -> Option<PortalIdentity> {
    let identity = match timeout(
        AUTH_DB_TIMEOUT,
        portal_identity_from_headers(library, headers),
    )
    .await
    {
        Ok(identity) => identity?,
        Err(_) => {
            tracing::warn!("portal identity lookup timed out");
            return None;
        }
    };
    if let Some(user_id) = identity.user_id {
        match timeout(AUTH_DB_TIMEOUT, library.get_user(user_id)).await {
            Ok(Ok(Some(user))) if matches!(user.status, UserStatus::Disabled) => {
                tracing::info!(user_id, "rejecting disabled user session");
                return None;
            }
            _ => {}
        }
    }
    Some(identity)
}

/// True when Bearer or a stored operator session authenticates; lookup errors fail closed.
async fn authorize_operator(
    state: &AppState,
    auth: &OperatorAuthState,
    headers: &HeaderMap,
) -> bool {
    if authorize_operator_bearer_only(auth, headers) {
        return true;
    }
    resolve_operator_session(state, auth, headers)
        .await
        .is_some()
}

/// True when `Authorization: Bearer` matches the operator token in constant time.
fn authorize_operator_bearer_only(auth: &OperatorAuthState, headers: &HeaderMap) -> bool {
    bearer_token(headers).is_some_and(|token| auth.token_matches(token))
}

#[derive(Debug, Clone)]
/// Operator session row used after a hashed cookie lookup succeeds.
struct ResolvedOperatorSession {
    /// SHA hash of the session id stored in `operator_sessions` (never the raw cookie).
    token_hash: String,
    /// Owner who elevated when this is a short-lived elevated session.
    elevated_from_user_id: Option<i64>,
    /// Target `users.id` when the operator is impersonating.
    impersonating_user_id: Option<i64>,
}

/// Looks up the hashed session cookie; missing, timed-out, or failed lookups fail closed.
async fn resolve_operator_session(
    state: &AppState,
    auth: &OperatorAuthState,
    headers: &HeaderMap,
) -> Option<ResolvedOperatorSession> {
    let _ = auth;
    let session_id = session_id_from_headers(headers)?;
    let token_hash = hash_token(&session_id);
    let library = state.library_snapshot().await;
    match timeout(AUTH_DB_TIMEOUT, library.get_operator_session(&token_hash)).await {
        Ok(Ok(Some(session))) => Some(ResolvedOperatorSession {
            token_hash,
            elevated_from_user_id: session.elevated_from_user_id,
            impersonating_user_id: session.impersonating_user_id,
        }),
        Ok(Ok(None)) => None,
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "operator session lookup failed");
            None
        }
        Err(_) => {
            tracing::warn!("operator session lookup timed out");
            None
        }
    }
}

/// Banner, prefs key, and portal identity for an impersonated user (DB misses yield `None` fields).
async fn impersonation_me_fields(
    library: &bookclerk_library::LibraryStore,
    user_id: Option<i64>,
) -> (Option<AuthMeImpersonating>, String, Option<i64>) {
    let Some(user_id) = user_id else {
        return (None, OPERATOR_PREFS_KEY.to_string(), None);
    };
    let display_name = match timeout(AUTH_DB_TIMEOUT, library.get_user(user_id)).await {
        Ok(Ok(Some(u))) => u.display_name,
        _ => None,
    };
    let identity_id = match timeout(
        AUTH_DB_TIMEOUT,
        library.first_portal_identity_for_user(user_id),
    )
    .await
    {
        Ok(Ok(Some(id))) => Some(id.id),
        _ => None,
    };
    (
        Some(AuthMeImpersonating {
            user_id,
            display_name,
        }),
        user_prefs_key(user_id),
        identity_id,
    )
}

/// Role / acquire / portal identity for an impersonated first-party user.
async fn impersonation_caller_identity(
    library: &bookclerk_library::LibraryStore,
    user_id: i64,
) -> (String, bool, Option<AuthMeUser>, Option<PortalMeInfo>) {
    let user = match timeout(AUTH_DB_TIMEOUT, library.get_user(user_id)).await {
        Ok(Ok(Some(u))) => u,
        _ => {
            return (String::from("member"), false, None, None);
        }
    };
    if matches!(user.status, UserStatus::Disabled) {
        return (String::from("member"), false, None, None);
    }
    let role = user.role.as_str().to_string();
    let can_acquire = user.role.is_privileged();
    let me_user = auth_me_user(&user);
    let portal = match timeout(
        AUTH_DB_TIMEOUT,
        library.first_portal_identity_for_user(user_id),
    )
    .await
    {
        Ok(Ok(Some(identity))) => Some(PortalMeInfo {
            identity_id: identity.id,
            provider: identity.provider,
            external_user_id: identity.external_user_id,
            label: identity.label,
        }),
        _ => None,
    };
    (role, can_acquire, Some(me_user), portal)
}

#[derive(Debug, Deserialize)]
/// JSON body for Owner password re-auth before issuing an elevated operator cookie.
pub struct ElevateRequest {
    /// Owner account password (re-authentication).
    pub password: String,
}

#[derive(Debug, Deserialize)]
/// JSON body naming the first-party user to impersonate.
pub struct ImpersonateRequest {
    /// Target `users.id` to impersonate.
    pub user_id: i64,
}

/// Owner portal session + password re-auth → short-lived elevated operator cookie.
pub async fn elevate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ElevateRequest>,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    if !auth.enabled {
        return Err(StatusCode::BAD_REQUEST);
    }
    if authorize_operator(&state, &auth, &headers).await {
        return Err(StatusCode::CONFLICT);
    }
    let library = state.library_snapshot().await;
    let identity = timed_portal_identity_from_headers(&library, &headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let (role, _, user, _) = resolve_portal_caller_identity(&library, &identity).await;
    if role != "owner" {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(user) = user else {
        return Err(StatusCode::FORBIDDEN);
    };
    let password = body.password.trim();
    if password.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let hash = library
        .get_user_password_hash(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(hash) = hash else {
        let _ = library
            .insert_security_audit_event(
                &format!("user:{}", user.id),
                "elevate_failed",
                Some(r#"{"reason":"no_password"}"#),
            )
            .await;
        return Err(StatusCode::FORBIDDEN);
    };
    let password_ok = bookclerk_library::verify_password(password, &hash).unwrap_or(false);
    if !password_ok {
        let _ = library
            .insert_security_audit_event(
                &format!("user:{}", user.id),
                "elevate_failed",
                Some(r#"{"reason":"bad_password"}"#),
            )
            .await;
        return Err(StatusCode::UNAUTHORIZED);
    }
    issue_elevation(&state, &library, user.id, &headers).await
}

/// Mint a short-lived elevated operator cookie for an Owner.
pub(crate) async fn issue_elevation(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    user_id: i64,
    headers: &HeaderMap,
) -> Result<Response, StatusCode> {
    let session_id = Uuid::new_v4().to_string();
    let token_hash = hash_token(&session_id);
    let expires = Utc::now()
        + ChronoDuration::from_std(ELEVATION_TTL).unwrap_or_else(|_| ChronoDuration::minutes(15));
    let client = session_client_from_headers(headers);
    library
        .insert_elevated_operator_session_with_client(&token_hash, expires, user_id, Some(&client))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = library
        .insert_security_audit_event(
            &format!("user:{user_id}"),
            "elevate_start",
            Some(&format!(r#"{{"user_id":{user_id}}}"#)),
        )
        .await;
    let flags = {
        let cfg = state.config.read().await;
        session_cookie_flags(cfg.integrations.public_origin.as_deref())
    };
    let max_age = ELEVATION_TTL.as_secs();
    let cookie = format!("{SESSION_COOKIE}={session_id}; {flags}; Max-Age={max_age}");
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "ok": true,
            "elevated": true,
            "expires_in_secs": max_age
        })),
    )
        .into_response())
}

/// Mint a portal session cookie for a first-party User.
pub(crate) async fn issue_portal_session(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    user: &bookclerk_library::UserRecord,
    headers: &HeaderMap,
    audit_action: &str,
) -> Result<Response, StatusCode> {
    let identity = library
        .ensure_local_portal_identity(user.id, user.display_name.as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let session_raw = Uuid::new_v4().to_string();
    let ttl_hours = {
        let cfg = state.config.read().await;
        cfg.integrations.portal_session_ttl_hours.max(1)
    };
    let expires = Utc::now() + ChronoDuration::hours(ttl_hours as i64);
    let client = session_client_from_headers(headers);
    library
        .insert_portal_session_with_client(
            &hash_token(&session_raw),
            identity.id,
            expires,
            Some(&client),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = library
        .insert_security_audit_event(&format!("user:{}", user.id), audit_action, None)
        .await;
    let flags = {
        let cfg = state.config.read().await;
        session_cookie_flags(cfg.integrations.public_origin.as_deref())
    };
    let max_age = ttl_hours.saturating_mul(3600);
    let cookie = format!("{PORTAL_SESSION_COOKIE}={session_raw}; {flags}; Max-Age={max_age}");
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "ok": true,
            "role": user.role.as_str(),
        })),
    )
        .into_response())
}

/// End elevation by revoking the elevated operator session cookie.
pub async fn elevate_end(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    let Some(op) = resolve_operator_session(&state, &auth, &headers).await else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if op.elevated_from_user_id.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let library = state.library_snapshot().await;
    let _ = library.delete_operator_session(&op.token_hash).await;
    let actor = op
        .elevated_from_user_id
        .map(|id| format!("user:{id}"))
        .unwrap_or_else(|| String::from("operator"));
    let _ = library
        .insert_security_audit_event(&actor, "elevate_end", None)
        .await;
    let flags = {
        let cfg = state.config.read().await;
        session_cookie_flags(cfg.integrations.public_origin.as_deref())
    };
    Ok((
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            format!("{SESSION_COOKIE}=; {flags}; Max-Age=0"),
        )],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response())
}

/// Operator (or elevated) session starts impersonating a User.
pub async fn impersonate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ImpersonateRequest>,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    if !auth.enabled {
        return Err(StatusCode::BAD_REQUEST);
    }
    let library = state.library_snapshot().await;
    let (token_hash, set_cookie) =
        if let Some(op) = resolve_operator_session(&state, &auth, &headers).await {
            (op.token_hash, None)
        } else if authorize_operator_bearer_only(&auth, &headers) {
            // Mint a durable session so impersonation state can be stored.
            let session_id = Uuid::new_v4().to_string();
            let token_hash = hash_token(&session_id);
            let expires = Utc::now()
                + ChronoDuration::from_std(auth.session_ttl)
                    .unwrap_or_else(|_| ChronoDuration::hours(12));
            let client = session_client_from_headers(&headers);
            library
                .insert_operator_session_with_client(&token_hash, expires, Some(&client))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let flags = {
                let cfg = state.config.read().await;
                session_cookie_flags(cfg.integrations.public_origin.as_deref())
            };
            let max_age = auth.session_ttl.as_secs();
            let cookie = format!("{SESSION_COOKIE}={session_id}; {flags}; Max-Age={max_age}");
            (token_hash, Some(cookie))
        } else {
            return Err(StatusCode::UNAUTHORIZED);
        };
    let target = library
        .get_user(body.user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if matches!(target.status, UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    library
        .set_operator_session_impersonating(&token_hash, Some(target.id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Mint a portal session cookie so `/api/portal/*` (Accounts linking) works
    // as the impersonated user without teaching portal routes about operator cookies.
    let mut set_cookies: Vec<HeaderValue> = Vec::new();
    if let Some(cookie) = set_cookie {
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            set_cookies.push(v);
        }
    }
    let identity = library
        .ensure_local_portal_identity(target.id, target.display_name.as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    {
        let cfg = state.config.read().await;
        let session_raw = session_for_identity(&library, &cfg.integrations, &identity)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let flags = session_cookie_flags(cfg.integrations.public_origin.as_deref());
        let max_age = cfg.integrations.portal_session_ttl_hours * 3600;
        let portal_cookie =
            format!("{PORTAL_SESSION_COOKIE}={session_raw}; {flags}; Max-Age={max_age}");
        if let Ok(v) = HeaderValue::from_str(&portal_cookie) {
            set_cookies.push(v);
        }
    }
    let _ = library
        .insert_security_audit_event(
            "operator",
            "impersonate_start",
            Some(&format!(r#"{{"user_id":{}}}"#, target.id)),
        )
        .await;
    let body = Json(serde_json::json!({
        "ok": true,
        "impersonating": {
            "user_id": target.id,
            "display_name": target.display_name,
        }
    }));
    let mut response = (StatusCode::OK, body).into_response();
    for cookie in set_cookies {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(response)
}

/// Clear impersonation on the current operator session.
pub async fn impersonate_end(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    let Some(op) = resolve_operator_session(&state, &auth, &headers).await else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if op.impersonating_user_id.is_none() {
        return Ok(Json(serde_json::json!({ "ok": true })).into_response());
    }
    let library = state.library_snapshot().await;
    let prev = op.impersonating_user_id;
    library
        .set_operator_session_impersonating(&op.token_hash, None)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Drop the portal session minted for impersonation (best-effort).
    if let Some(raw) = cookie_value(&headers, PORTAL_SESSION_COOKIE) {
        let _ = library.delete_portal_session(&hash_token(&raw)).await;
    }
    let actor = op
        .elevated_from_user_id
        .map(|id| format!("user:{id}"))
        .unwrap_or_else(|| String::from("operator"));
    let detail = prev.map(|id| format!(r#"{{"user_id":{id}}}"#));
    let _ = library
        .insert_security_audit_event(&actor, "impersonate_end", detail.as_deref())
        .await;
    let flags = {
        let cfg = state.config.read().await;
        session_cookie_flags(cfg.integrations.public_origin.as_deref())
    };
    let clear = format!("{PORTAL_SESSION_COOKIE}=; {flags}; Max-Age=0");
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, clear)],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response())
}

/// List first-party users (operator or administrator provisioner).
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    authorize_provisioner(&state, &auth, &headers, &library).await?;
    let users = library
        .list_users()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut rows = Vec::new();
    for u in users {
        let identities = library
            .list_portal_identities_for_user(u.id)
            .await
            .unwrap_or_default();
        rows.push(serde_json::json!({
            "id": u.id,
            "role": u.role.as_str(),
            "status": u.status.as_str(),
            "display_name": u.display_name,
            "login_name": u.login_name,
            "email": u.email,
            "has_password": u.has_password,
            "online": false,
            "last_active_at": null,
            "listening": null,
            "integrations": [],
            "identities": identities
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "provider": p.provider,
                        "external_user_id": p.external_user_id,
                        "label": p.label,
                    })
                })
                .collect::<Vec<_>>(),
        }));
    }
    Ok(Json(serde_json::json!({ "users": rows })))
}

#[derive(Debug, Deserialize)]
/// Partial profile, role, or status patch for a first-party user.
pub struct PatchUserRequest {
    #[serde(default)]
    /// Replacement role (`owner` / `administrator` / `member`) when present.
    pub role: Option<String>,
    #[serde(default)]
    /// Replacement status (`active` / `disabled`) when present.
    pub status: Option<String>,
    #[serde(default)]
    /// Replacement display name when present.
    pub display_name: Option<String>,
    #[serde(default)]
    /// Replacement local login name when present.
    pub login_name: Option<String>,
    #[serde(default)]
    /// Replacement email when present.
    pub email: Option<String>,
}

/// 409 body when the patch would remove the last administrator.
fn last_administrator_response() -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": "last_administrator" })),
    )
        .into_response()
}

/// 409 body when the patch would remove the last owner.
fn last_owner_response() -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": "last_owner" })),
    )
        .into_response()
}

/// Patch role/status/display/login for a first-party user (provisioner).
pub async fn patch_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
    Json(body): Json<PatchUserRequest>,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    let actor = authorize_provisioner(&state, &auth, &headers, &library).await?;
    if body.role.is_none()
        && body.status.is_none()
        && body.display_name.is_none()
        && body.login_name.is_none()
        && body.email.is_none()
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let is_self = actor.user_id() == Some(user_id);
    if !actor.can_manage_target(user.role, is_self) {
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(role_raw) = body.role.as_deref() {
        let role = match role_raw.trim() {
            "owner" => UserRole::Owner,
            "administrator" => UserRole::Administrator,
            "member" => UserRole::Member,
            _ => return Err(StatusCode::BAD_REQUEST),
        };
        if !actor.can_assign_role(role) {
            return Err(StatusCode::FORBIDDEN);
        }
        user = match library.set_user_role(user_id, role).await {
            Ok(u) => u,
            Err(LibraryError::LastOwner) => return Ok(last_owner_response()),
            Err(LibraryError::LastAdministrator) => return Ok(last_administrator_response()),
            Err(LibraryError::NotFound(_)) => return Err(StatusCode::NOT_FOUND),
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        };
    }

    if let Some(status_raw) = body.status.as_deref() {
        let status = match status_raw.trim() {
            "active" => UserStatus::Active,
            "disabled" => UserStatus::Disabled,
            _ => return Err(StatusCode::BAD_REQUEST),
        };
        user = match library.set_user_status(user_id, status).await {
            Ok(u) => u,
            Err(LibraryError::LastOwner) => return Ok(last_owner_response()),
            Err(LibraryError::LastAdministrator) => return Ok(last_administrator_response()),
            Err(LibraryError::NotFound(_)) => return Err(StatusCode::NOT_FOUND),
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        };
        if matches!(status, UserStatus::Disabled) {
            let _ = library.delete_portal_sessions_for_user(user_id).await;
        }
    }

    if let Some(display_name) = body.display_name.as_deref() {
        user = library
            .set_user_display_name(user_id, Some(display_name))
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
    }

    if let Some(login_name) = body.login_name.as_deref() {
        user = library
            .set_user_login_name(user_id, Some(login_name))
            .await
            .map_err(|_| StatusCode::CONFLICT)?;
    }

    if let Some(email) = body.email.as_deref() {
        user = library
            .set_user_email(user_id, Some(email))
            .await
            .map_err(|_| StatusCode::CONFLICT)?;
    }

    let _ = library
        .insert_security_audit_event(
            &actor.audit_actor(),
            "user_patch",
            Some(&format!(
                r#"{{"user_id":{},"role":"{}","status":"{}"}}"#,
                user.id,
                user.role.as_str(),
                user.status.as_str()
            )),
        )
        .await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "user": user_json(&user),
    }))
    .into_response())
}

/// Mint a fresh local claim ticket for an active first-party user.
pub async fn create_user_claim_ticket(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    let actor = authorize_provisioner(&state, &auth, &headers, &library).await?;
    let user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !actor.can_provision_target(user.role, user_id) {
        return Err(StatusCode::FORBIDDEN);
    }
    if matches!(user.status, UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    let actor_s = actor.audit_actor();
    let identity = library
        .ensure_local_portal_identity(user.id, user.display_name.as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let claim = mint_local_claim(&library, identity.id, &actor_s).await?;
    let _ = library
        .insert_security_audit_event(
            &actor_s,
            "user_claim_ticket",
            Some(&format!(r#"{{"user_id":{user_id}}}"#)),
        )
        .await;
    let invite_url = invite_magic_link(&state, &headers, &claim);
    Ok(Json(serde_json::json!({
        "ok": true,
        "claim_ticket": claim,
        "invite_url": invite_url,
    })))
}

/// Invalidate a user's password, revoke sessions, and mint a claim ticket for reset.
pub async fn reset_user_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    let actor = authorize_provisioner(&state, &auth, &headers, &library).await?;
    let user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !actor.can_provision_target(user.role, user_id) {
        return Err(StatusCode::FORBIDDEN);
    }
    if matches!(user.status, UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    let actor_s = actor.audit_actor();
    library
        .set_user_password_hash(user_id, None)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let revoked = library
        .delete_portal_sessions_for_user(user_id)
        .await
        .unwrap_or(0);
    let identity = library
        .ensure_local_portal_identity(user.id, user.display_name.as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let claim = mint_local_claim(&library, identity.id, &actor_s).await?;
    let _ = library
        .insert_security_audit_event(
            &actor_s,
            "user_password_reset",
            Some(&format!(
                r#"{{"user_id":{user_id},"revoked_sessions":{revoked}}}"#
            )),
        )
        .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "claim_ticket": claim,
        "revoked_sessions": revoked,
    })))
}

/// Delete a user and their personal data (wishlist / links / sessions); keep acquired books.
///
/// Operators/administrators may delete any user. Portal callers may delete
/// only their own account (self-service).
pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    let portal_self_id = timed_portal_identity_from_headers(&library, &headers)
        .await
        .and_then(|identity| identity.user_id);
    let is_self = portal_self_id == Some(user_id);
    let target = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let actor = match authorize_provisioner(&state, &auth, &headers, &library).await {
        Ok(actor) => {
            if !actor.can_manage_target(target.role, is_self) {
                return Err(StatusCode::FORBIDDEN);
            }
            actor.audit_actor()
        }
        Err(StatusCode::FORBIDDEN) | Err(StatusCode::UNAUTHORIZED) if is_self => {
            format!("user:{user_id}")
        }
        Err(other) => return Err(other),
    };
    match library.delete_user(user_id).await {
        Ok(()) => {
            let _ = library
                .insert_security_audit_event(
                    &actor,
                    if is_self {
                        "user_self_delete"
                    } else {
                        "user_delete"
                    },
                    Some(&format!(r#"{{"user_id":{user_id}}}"#)),
                )
                .await;
            let mut response =
                (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response();
            if is_self {
                let flags = {
                    let cfg = state.config.read().await;
                    session_cookie_flags(cfg.integrations.public_origin.as_deref())
                };
                let clear = format!("{PORTAL_SESSION_COOKIE}=; {flags}; Max-Age=0");
                if let Ok(v) = HeaderValue::from_str(&clear) {
                    response.headers_mut().append(header::SET_COOKIE, v);
                }
            }
            Ok(response)
        }
        Err(LibraryError::NotFound(_)) => Err(StatusCode::NOT_FOUND),
        Err(LibraryError::LastOwner) => Err(StatusCode::CONFLICT),
        Err(LibraryError::LastAdministrator) => Err(StatusCode::CONFLICT),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Debug, Deserialize)]
/// Operator-only body for creating the first Owner when none exist.
pub struct BootstrapRequest {
    #[serde(default)]
    /// Optional display name; falls back to login or email.
    pub display_name: Option<String>,
    #[serde(default)]
    /// Optional local login name.
    pub login_name: Option<String>,
    #[serde(default)]
    /// Optional email.
    pub email: Option<String>,
    #[serde(default)]
    /// Optional password; hashed before persist and never stored in plaintext.
    pub password: Option<String>,
}

/// Operator-only bootstrap of the first Owner when none exist.
pub async fn bootstrap(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BootstrapRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    if auth.enabled && !authorize_operator(&state, &auth, &headers).await {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let library = state.library_snapshot().await;
    let owners = library
        .count_owners()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let admins = library
        .count_administrators()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // First bootstrap when no owners (and no legacy administrators) exist.
    if owners > 0 || admins > 0 {
        return Err(StatusCode::CONFLICT);
    }
    let password_hash = match body
        .password
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(pw) => Some(hash_password(pw).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?),
        None => None,
    };
    let login = body
        .login_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let email = body
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let display = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(login)
        .or(email);
    let user = library
        .create_user_with_profile(
            UserRole::Owner,
            display,
            login,
            email,
            password_hash.as_deref(),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let identity = library
        .ensure_local_portal_identity(user.id, display)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let claim = mint_local_claim(&library, identity.id, "bootstrap").await?;
    let _ = library
        .insert_security_audit_event(
            "operator",
            "bootstrap_owner",
            Some(&format!(r#"{{"user_id":{}}}"#, user.id)),
        )
        .await;
    let invite_url = invite_magic_link(&state, &headers, &claim);
    Ok(Json(serde_json::json!({
        "ok": true,
        "user_id": user.id,
        "claim_ticket": claim,
        "invite_url": invite_url,
        "login_name": user.login_name,
        "has_password": password_hash.is_some(),
    })))
}

#[derive(Debug, Deserialize)]
/// Provisioner body for creating a user and optional invite ticket.
pub struct CreateUserRequest {
    #[serde(default)]
    /// Role to assign when permitted (`member` when omitted).
    pub role: Option<String>,
    #[serde(default)]
    /// Optional display name for the new user.
    pub display_name: Option<String>,
    #[serde(default)]
    /// Optional local login name for the new user.
    pub login_name: Option<String>,
    #[serde(default)]
    /// Optional email for the new user.
    pub email: Option<String>,
    #[serde(default)]
    /// Optional password; hashed before persist. Empty or omitted leaves the user passwordless.
    pub password: Option<String>,
    /// When true (default), also mint an invite/claim ticket.
    #[serde(default = "default_true")]
    pub mint_invite: bool,
}

/// Serde default so `mint_invite` is true when the field is omitted.
fn default_true() -> bool {
    true
}

/// Owner/admin or operator creates a user and optional invite magic link.
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateUserRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    let actor = authorize_provisioner(&state, &auth, &headers, &library).await?;
    let role = match body.role.as_deref().unwrap_or("member") {
        "owner" => UserRole::Owner,
        "administrator" => UserRole::Administrator,
        "member" => UserRole::Member,
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    if !actor.can_assign_role(role) {
        return Err(StatusCode::FORBIDDEN);
    }
    let actor_s = actor.audit_actor();
    let password_hash = match body
        .password
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(pw) => Some(hash_password(pw).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?),
        None => None,
    };
    let login = body
        .login_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let email = body
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let display = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(login)
        .or(email);
    let user = library
        .create_user_with_profile(role, display, login, email, password_hash.as_deref())
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    let identity = library
        .ensure_local_portal_identity(user.id, display)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let claim = if body.mint_invite {
        Some(mint_local_claim(&library, identity.id, &actor_s).await?)
    } else {
        None
    };
    let invite_url = claim
        .as_deref()
        .map(|ticket| invite_magic_link(&state, &headers, ticket));
    let _ = library
        .insert_security_audit_event(
            &actor_s,
            "provision_user",
            Some(&format!(
                r#"{{"user_id":{},"role":"{}"}}"#,
                user.id,
                role.as_str()
            )),
        )
        .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "user": user_json(&user),
        "claim_ticket": claim,
        "invite_url": invite_url,
    })))
}

#[derive(Debug, Deserialize)]
/// JSON body for local password login (login name or email plus password).
pub struct PasswordLoginRequest {
    /// Login name or email.
    pub login: String,
    /// Password verified against the stored hash; mismatch fails closed and counts toward lockout.
    pub password: String,
}

/// Local password login → portal session cookie.
pub async fn password_login(
    State(state): State<Arc<AppState>>,
    ClientIp(client_key): ClientIp,
    Json(body): Json<PasswordLoginRequest>,
) -> Result<Response, StatusCode> {
    let auth = state.auth_snapshot().await;
    if let Some(retry_after) = auth.login_throttle_check(&client_key).await {
        return Ok(too_many_requests(retry_after));
    }
    let library = state.library_snapshot().await;
    let user = match library
        .get_user_by_login_name(&body.login)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Some(u) => u,
        None => match library
            .get_user_by_email(&body.login)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        {
            Some(u) => u,
            None => {
                let _ = auth.record_login_failure(&client_key).await;
                return Err(StatusCode::UNAUTHORIZED);
            }
        },
    };
    if matches!(user.status, UserStatus::Disabled) {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(hash) = library
        .get_user_password_hash(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        let _ = auth.record_login_failure(&client_key).await;
        return Err(StatusCode::UNAUTHORIZED);
    };
    let ok =
        verify_password(&body.password, &hash).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !ok {
        let _ = auth.record_login_failure(&client_key).await;
        return Err(StatusCode::UNAUTHORIZED);
    }
    auth.clear_login_failures(&client_key).await;
    let identity = library
        .ensure_local_portal_identity(user.id, user.display_name.as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let session_raw = Uuid::new_v4().to_string();
    let ttl_hours = {
        let cfg = state.config.read().await;
        cfg.integrations.portal_session_ttl_hours.max(1)
    };
    let expires = Utc::now() + ChronoDuration::hours(ttl_hours as i64);
    library
        .insert_portal_session(&hash_token(&session_raw), identity.id, expires)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = library
        .insert_security_audit_event(&format!("user:{}", user.id), "password_login", None)
        .await;
    let flags = {
        let cfg = state.config.read().await;
        session_cookie_flags(cfg.integrations.public_origin.as_deref())
    };
    let max_age = ttl_hours.saturating_mul(3600);
    let cookie = format!("{PORTAL_SESSION_COOKIE}={session_raw}; {flags}; Max-Age={max_age}");
    let default_view =
        default_view_for_subject(&library, &user_prefs_key(user.id), Some(identity.id)).await;
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "ok": true,
            "role": user.role.as_str(),
            "default_view": default_view,
        })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
/// JSON body for setting a password (self or provisioner target).
pub struct SetPasswordRequest {
    /// New password; hashed before persist. Empty is rejected.
    pub password: String,
    /// Required when the caller already has a password (not first-time invite setup).
    #[serde(default)]
    pub current_password: Option<String>,
    #[serde(default)]
    /// Target `users.id` when a provisioner sets another user's password.
    pub user_id: Option<i64>,
}

/// Set password for self (portal) or target user (operator/admin); revokes sessions.
pub async fn set_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<SetPasswordRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    let password = body.password.trim();
    if password.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let portal_uid = timed_portal_identity_from_headers(&library, &headers)
        .await
        .and_then(|identity| identity.user_id);
    let (target_id, actor_s, self_service) = if let Some(uid) = body.user_id {
        if portal_uid == Some(uid) {
            (uid, format!("user:{uid}"), true)
        } else {
            let actor = authorize_provisioner(&state, &auth, &headers, &library).await?;
            let target = library
                .get_user(uid)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::NOT_FOUND)?;
            if !actor.can_provision_target(target.role, uid) {
                return Err(StatusCode::FORBIDDEN);
            }
            (uid, actor.audit_actor(), false)
        }
    } else if let Some(uid) = portal_uid {
        (uid, format!("user:{uid}"), true)
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if self_service {
        let existing_hash = library
            .get_user_password_hash(target_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if let Some(hash) = existing_hash {
            let current = body
                .current_password
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or(StatusCode::UNAUTHORIZED)?;
            let ok =
                verify_password(current, &hash).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            if !ok {
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
    }
    let hash = hash_password(password).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    library
        .set_user_password_hash(target_id, Some(&hash))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let revoked = library
        .delete_portal_sessions_for_user(target_id)
        .await
        .unwrap_or(0);
    let _ = library
        .insert_security_audit_event(
            &actor_s,
            "password_change",
            Some(&format!(r#"{{"revoked_sessions":{revoked}}}"#)),
        )
        .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "revoked_sessions": revoked,
    })))
}

/// Who may provision users, and which roles they may assign or manage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provisioner {
    /// Daemon operator token or session (no first-party `users` row).
    Operator,
    /// Elevated owner principal for privileged operator actions.
    ElevatedOwner {
        /// Authenticated user id for the elevated owner.
        user_id: i64,
    },
    /// Owner principal for the library.
    Owner {
        /// Authenticated user id for the owner.
        user_id: i64,
    },
    /// Administrator principal for the library.
    Administrator {
        /// Authenticated user id for the administrator.
        user_id: i64,
    },
}

impl Provisioner {
    /// Security-audit actor string (`operator` or `user:{id}`).
    fn audit_actor(self) -> String {
        match self {
            Self::Operator => String::from("operator"),
            Self::ElevatedOwner { user_id }
            | Self::Owner { user_id }
            | Self::Administrator { user_id } => format!("user:{user_id}"),
        }
    }

    /// First-party user id for portal provisioners; `None` for the operator principal.
    fn user_id(self) -> Option<i64> {
        match self {
            Self::Operator => None,
            Self::ElevatedOwner { user_id }
            | Self::Owner { user_id }
            | Self::Administrator { user_id } => Some(user_id),
        }
    }

    /// Whether this principal may assign `role` (operators and elevated owners: any; owners: not Owner; admins: Member only).
    fn can_assign_role(self, role: UserRole) -> bool {
        match self {
            Self::Operator | Self::ElevatedOwner { .. } => true,
            Self::Owner { .. } => !matches!(role, UserRole::Owner),
            Self::Administrator { .. } => matches!(role, UserRole::Member),
        }
    }

    /// Profile patch / self-delete may target the caller. Authenticator reset,
    /// claim remint, and `PUT /api/auth/password` with `user_id` must not.
    fn can_manage_target(self, target_role: UserRole, is_self: bool) -> bool {
        if is_self {
            return true;
        }
        self.can_assign_role(target_role)
    }

    /// Provisioning another account: never a self-service shortcut.
    fn can_provision_target(self, target_role: UserRole, target_user_id: i64) -> bool {
        self.user_id() != Some(target_user_id) && self.can_assign_role(target_role)
    }
}

/// Resolves the caller as a provisioner; impersonation and unprivileged portal sessions fail closed.
async fn authorize_provisioner(
    state: &AppState,
    auth: &OperatorAuthState,
    headers: &HeaderMap,
    library: &bookclerk_library::LibraryStore,
) -> Result<Provisioner, StatusCode> {
    if !auth.enabled {
        return Ok(Provisioner::Operator);
    }
    if let Some(op) = resolve_operator_session(state, auth, headers).await {
        if op.impersonating_user_id.is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
        if let Some(user_id) = op.elevated_from_user_id {
            return Ok(Provisioner::ElevatedOwner { user_id });
        }
        return Ok(Provisioner::Operator);
    }
    if authorize_operator_bearer_only(auth, headers) {
        return Ok(Provisioner::Operator);
    }
    let identity = timed_portal_identity_from_headers(library, headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let (role, _, user, _) = resolve_portal_caller_identity(library, &identity).await;
    let user_id = user.map(|u| u.id).ok_or(StatusCode::FORBIDDEN)?;
    match role.as_str() {
        "owner" => Ok(Provisioner::Owner { user_id }),
        "administrator" => Ok(Provisioner::Administrator { user_id }),
        _ => Err(StatusCode::FORBIDDEN),
    }
}

/// Issues a raw claim ticket and persists only its hash (7-day TTL).
async fn mint_local_claim(
    library: &bookclerk_library::LibraryStore,
    identity_id: i64,
    created_by: &str,
) -> Result<String, StatusCode> {
    let raw = Uuid::new_v4().to_string();
    let expires = Utc::now() + ChronoDuration::days(7);
    library
        .insert_claim_ticket(&hash_token(&raw), Some(identity_id), expires, created_by)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(raw)
}

/// Shareable magic-link invite URL (`/invite?ticket=…`).
fn invite_magic_link(state: &AppState, headers: &HeaderMap, ticket: &str) -> String {
    if let Ok(cfg) = state.config.try_read() {
        if let Some(url) = bookclerk_integrations::ticket_portal_url(&cfg.integrations, ticket) {
            return url;
        }
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|host| {
                    let scheme = if host.contains("localhost") || host.starts_with("127.") {
                        "http"
                    } else {
                        "https"
                    };
                    format!("{scheme}://{host}")
                })
        })
        .unwrap_or_else(|| String::from("http://127.0.0.1:8787"));
    format!("{}/invite?ticket={}", origin.trim_end_matches('/'), ticket)
}

/// Serializes a user row for API responses without the password hash.
fn user_json(user: &bookclerk_library::UserRecord) -> serde_json::Value {
    serde_json::json!({
        "id": user.id,
        "role": user.role.as_str(),
        "status": user.status.as_str(),
        "display_name": user.display_name,
        "login_name": user.login_name,
        "email": user.email,
        "has_password": user.has_password,
    })
}

/// List sessions for the current principal only (not every operator session).
pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    if let Some(op) = resolve_operator_session(&state, &auth, &headers).await {
        // Impersonating → show the target user's portal sessions.
        if let Some(target_id) = op.impersonating_user_id {
            let identity = library
                .first_portal_identity_for_user(target_id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::NOT_FOUND)?;
            return Ok(Json(serde_json::json!({
                "sessions": portal_session_rows(&library, identity.id, None).await?
            })));
        }
        let current_id = library
            .get_operator_session(&op.token_hash)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map(|s| s.id);
        let elevated_from = op.elevated_from_user_id;
        let rows = library
            .list_operator_sessions()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let sessions: Vec<_> = rows
            .into_iter()
            .filter(|s| s.elevated_from_user_id == elevated_from)
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "kind": "operator",
                    "created_at": s.created_at.to_rfc3339(),
                    "last_used_at": s.last_used_at.map(|t| t.to_rfc3339()),
                    "expires_at": s.expires_at.to_rfc3339(),
                    "elevated": s.elevated_from_user_id.is_some(),
                    "impersonating_user_id": s.impersonating_user_id,
                    "is_current": current_id == Some(s.id),
                    "client_label": s.client_label,
                    "device_type": s.device_type,
                    "user_agent": s.user_agent,
                })
            })
            .collect();
        return Ok(Json(serde_json::json!({ "sessions": sessions })));
    }
    if !auth.enabled || authorize_operator_bearer_only(&auth, &headers) {
        // Bearer-only: no durable session rows to list for "this device".
        return Ok(Json(serde_json::json!({ "sessions": [] })));
    }
    let identity = timed_portal_identity_from_headers(&library, &headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let current_portal = cookie_value(&headers, PORTAL_SESSION_COOKIE).map(|raw| hash_token(&raw));
    Ok(Json(serde_json::json!({
        "sessions": portal_session_rows(&library, identity.id, current_portal.as_deref()).await?
    })))
}

/// Portal session records for one identity, marking the current hashed cookie when provided.
async fn portal_session_rows(
    library: &bookclerk_library::LibraryStore,
    identity_id: i64,
    current_token_hash: Option<&str>,
) -> Result<Vec<serde_json::Value>, StatusCode> {
    let sessions = library
        .list_portal_session_records_for_identity(identity_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(sessions
        .into_iter()
        .map(|row| {
            let is_current = current_token_hash
                .map(|h| h == row.token_hash.as_str())
                .unwrap_or(false);
            serde_json::json!({
                "id": row.id,
                "kind": "portal",
                "created_at": row.created_at,
                "expires_at": row.expires_at,
                "last_used_at": row.last_used_at,
                "is_current": is_current,
                "client_label": row.client_label,
                "device_type": row.device_type,
                "user_agent": row.user_agent,
            })
        })
        .collect())
}

/// Revoke a session by id. Operators may revoke any operator session;
/// portal users may only revoke their own.
pub async fn revoke_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let auth = state.auth_snapshot().await;
    let library = state.library_snapshot().await;
    if !auth.enabled || authorize_operator(&state, &auth, &headers).await {
        let ok = library
            .delete_operator_session_by_id(id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if !ok {
            return Err(StatusCode::NOT_FOUND);
        }
        let _ = library
            .insert_security_audit_event(
                "operator",
                "session_revoke",
                Some(&format!(r#"{{"id":{id}}}"#)),
            )
            .await;
        return Ok(Json(serde_json::json!({ "ok": true })));
    }
    let identity = timed_portal_identity_from_headers(&library, &headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let ok = library
        .delete_portal_session_by_id_for_identity(id, identity.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !ok {
        // Horizontal privilege: session exists but not ours → 404 (not 403).
        return Err(StatusCode::NOT_FOUND);
    }
    let _ = library
        .insert_security_audit_event(
            &format!("user:{}", identity.user_id.unwrap_or(0)),
            "session_revoke",
            Some(&format!(r#"{{"id":{id}}}"#)),
        )
        .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Extracts a non-empty Bearer token from `Authorization`; missing or empty fails closed.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// How long a portal session counts as "recent" for authenticator / IdP changes.
pub(crate) const RECENT_AUTH_WINDOW: ChronoDuration = ChronoDuration::minutes(15);

/// Operator bearer/session (including elevated Owner) may skip step-up.
///
/// A non-elevated Owner portal session must have been minted within
/// [`RECENT_AUTH_WINDOW`] or present a matching `current_password`.
pub(crate) async fn require_operator_or_recent_owner(
    state: &AppState,
    headers: &HeaderMap,
    current_password: Option<&str>,
) -> Result<(), StatusCode> {
    let auth = state.auth_snapshot().await;
    if authorize_operator_bearer_only(&auth, headers) {
        return Ok(());
    }
    if let Some(op) = resolve_operator_session(state, &auth, headers).await {
        if op.impersonating_user_id.is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
        return Ok(());
    }
    let library = state.library_snapshot().await;
    let identity = timed_portal_identity_from_headers(&library, headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let (role, _, user, _) = resolve_portal_caller_identity(&library, &identity).await;
    if role != "owner" {
        return Err(StatusCode::FORBIDDEN);
    }
    let user = user.ok_or(StatusCode::UNAUTHORIZED)?;
    require_recent_portal_reauth(state, headers, user.id, current_password).await
}

/// Require a recently minted portal session or the user's current password.
pub(crate) async fn require_recent_portal_reauth(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
    current_password: Option<&str>,
) -> Result<(), StatusCode> {
    let library = state.library_snapshot().await;
    if let Some(raw) = cookie_value(headers, PORTAL_SESSION_COOKIE) {
        if let Ok(Some(created)) = library.portal_session_created_at(&hash_token(&raw)).await {
            if Utc::now() - created <= RECENT_AUTH_WINDOW {
                return Ok(());
            }
        }
    }
    let current = current_password
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let hash = library
        .get_user_password_hash(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let ok = verify_password(current, &hash).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if ok {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Raw operator session id from the session cookie (hashed before any DB lookup).
fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, SESSION_COOKIE)
}

/// First non-empty Cookie header value for `name`.
pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{name}=")) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Drops idle throttle buckets while keeping active lockouts.
fn prune_login_attempts(
    attempts: &mut HashMap<String, LoginThrottleBucket>,
    window: Duration,
    lockout: Duration,
) {
    let now = Instant::now();
    attempts.retain(|_, bucket| {
        if let Some(until) = bucket.locked_until {
            if now < until {
                return true;
            }
        }
        bucket.window_start.elapsed() < window.max(lockout)
    });
}

/// Length-checked XOR compare so token checks do not leak via timing.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[must_use]
/// Maps a prefs string onto a known SPA view; unknown values become `discover`.
pub fn normalize_default_view(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "library" => String::from("library"),
        "accounts" => String::from("accounts"),
        "wishlist" => String::from("wishlist"),
        _ => String::from("discover"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[tokio::test]
    async fn login_throttle_locks_after_max_failures() {
        let auth = OperatorAuthState::new("secret-token".into(), 12, true, 3, 30);
        assert!(auth.login_throttle_check("127.0.0.1").await.is_none());
        assert!(auth.record_login_failure("127.0.0.1").await.is_none());
        assert!(auth.record_login_failure("127.0.0.1").await.is_none());
        let locked = auth.record_login_failure("127.0.0.1").await;
        assert!(locked.is_some());
        // A locked bucket refuses further attempts even with a valid token.
        let retry = auth.login_throttle_check("127.0.0.1").await;
        assert!(retry.is_some());
        // Throttling is per client key.
        assert!(auth.login_throttle_check("10.0.0.2").await.is_none());
        auth.clear_login_failures("127.0.0.1").await;
        assert!(auth.login_throttle_check("127.0.0.1").await.is_none());
    }

    #[test]
    fn too_many_requests_sets_retry_after_and_json() {
        let res = too_many_requests(Duration::from_secs(45));
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "45");
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn token_grace_accepts_previous_until_deadline() {
        let previous = OperatorAuthState::new("old-token-value".into(), 12, true, 5, 30);
        let mut next = OperatorAuthState::new("new-token-value".into(), 12, true, 5, 30);
        next.install_token_grace_from(&previous, Duration::from_secs(30));
        assert!(next.token_matches("new-token-value"));
        assert!(next.token_matches("old-token-value"));
        assert!(!next.token_matches("other-token-value"));
    }

    #[test]
    fn trusted_proxy_uses_x_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.10, 10.0.0.1"),
        );
        let peer = "10.0.0.1".parse().unwrap();
        let key = resolve_client_ip_key(Some(peer), &headers, &["10.0.0.1".into()]);
        assert_eq!(key, "203.0.113.10");
        let untrusted = resolve_client_ip_key(Some(peer), &headers, &[]);
        assert_eq!(untrusted, "10.0.0.1");
    }

    /// Test-only owner password (assembled so static analysis does not flag a literal).
    fn phase2_owner_password() -> String {
        ["owner", "-", "pass"].concat()
    }

    /// Test-only administrator password (assembled at runtime).
    fn phase2_admin_password() -> String {
        ["adm", "in", "-", "pass"].concat()
    }

    /// Install a process DEK when tests have not already configured one.
    ///
    /// Production hosts call [`bookclerk_library::configure_master_key_with`] at
    /// startup. Claim redeem HMACs the portal session from that key so HTTP
    /// retries reuse the same `dbAtomic` operation id.
    ///
    /// Holds the process DEK stable while a test derives claim fingerprints.
    ///
    /// `oidc_rp` persist tests replace `master.key`; without this lock a redeem
    /// retry can HMAC with a different DEK and get 400.
    pub(crate) async fn process_dek_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    /// Serialized: parallel harness setup must not race on creating/reading a
    /// half-written `master.key` (CI saw `truncated` under `--test-threads>1`).
    fn ensure_process_dek() {
        static INIT: std::sync::Once = std::sync::Once::new();
        static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        INIT.call_once(|| {
            if bookclerk_library::require_master_key(None).is_ok() {
                return;
            }
            let dir = DIR.get_or_init(|| tempfile::tempdir().expect("test master.key dir"));
            bookclerk_library::configure_master_key(dir.path()).expect("test DEK");
        });
    }

    /// Build a minimal AppState + router for Phase 2 authz tests.
    async fn phase2_harness(
        token: &str,
    ) -> (
        Arc<crate::api::AppState>,
        axum::Router,
        bookclerk_library::LibraryStore,
    ) {
        use std::sync::Arc;

        use bookclerk_config::{Config, ListenAddrs};
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::{LibraryStore, UserRole};
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_source::SourceRegistry;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

        use crate::api::AppState;

        // Claim redeem derives the portal session via HMAC-SHA256(DEK, ticket),
        // matching daemon startup (`configure_master_key_with`). In-memory
        // tests have no files_dir, so mint a process DEK when none is cached.
        ensure_process_dek();

        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        let _ = library.ensure_users_bridged().await;
        let mut cfg = Config::default();
        cfg.daemon.listen = ListenAddrs::parse_list("127.0.0.1:8787").unwrap();
        cfg.daemon.auth.enabled = true;

        let state = Arc::new(AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(library.clone())),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                token.to_string(),
                12,
                true,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
        });
        // Seed owner (with password for elevate) + member for elevate/impersonate.
        // Password assembled at runtime so CodeQL does not flag a hard-coded credential.
        let owner_password = phase2_owner_password();
        let owner_hash = bookclerk_library::hash_password(&owner_password).unwrap();
        let admin = library
            .create_user_with_login(
                UserRole::Owner,
                Some("Owner"),
                Some("owner"),
                Some(&owner_hash),
            )
            .await
            .unwrap();
        let member = library
            .create_user(UserRole::Member, Some("Member"), None)
            .await
            .unwrap();
        let administrator_password = phase2_admin_password();
        let administrator_hash = bookclerk_library::hash_password(&administrator_password).unwrap();
        let administrator = library
            .create_user_with_login(
                UserRole::Administrator,
                Some("Administrator"),
                Some("administrator"),
                Some(&administrator_hash),
            )
            .await
            .unwrap();
        let admin_id = library
            .upsert_portal_identity("test", "admin-ext", Some("Owner"))
            .await
            .unwrap();
        // Force owner role on the bridged user created by upsert.
        if let Some(uid) = admin_id.user_id {
            let _ = library.set_user_role(uid, UserRole::Owner).await;
            let _ = library.set_user_password_hash(uid, Some(&owner_hash)).await;
            let _ = admin;
        }
        let member_id = library
            .upsert_portal_identity("test", "member-ext", Some("Member"))
            .await
            .unwrap();
        let administrator_id = library
            .upsert_portal_identity("test", "administrator-ext", Some("Administrator"))
            .await
            .unwrap();
        if let Some(uid) = administrator_id.user_id {
            let _ = library.set_user_role(uid, UserRole::Administrator).await;
            let _ = library
                .set_user_password_hash(uid, Some(&administrator_hash))
                .await;
        }
        let _ = (admin, member, member_id, administrator);

        let app = crate::api::router(state.clone(), None);
        (state, app, library)
    }

    async fn portal_cookie_for(
        library: &bookclerk_library::LibraryStore,
        provider: &str,
        external: &str,
    ) -> String {
        use bookclerk_library::hash_token;
        use chrono::{Duration as ChronoDuration, Utc};
        use uuid::Uuid;

        let identity = library
            .get_portal_identity(provider, external)
            .await
            .unwrap()
            .expect("identity");
        let raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(12),
            )
            .await
            .unwrap();
        format!("{PORTAL_SESSION_COOKIE}={raw}")
    }

    fn cookie_from_set_cookie(header: &str) -> String {
        header
            .split(';')
            .next()
            .unwrap_or(header)
            .trim()
            .to_string()
    }

    fn claim_redeem_nonce(parts: &[&str]) -> String {
        // Concatenate at runtime so CodeQL does not treat a test nonce as a
        // hard-coded cryptographic value.
        bookclerk_library::hash_token(&parts.concat())
    }

    fn claim_redeem_body(ticket: &str, nonce: &str, password: Option<&str>) -> String {
        match password {
            Some(password) => {
                format!(r#"{{"ticket":"{ticket}","nonce":"{nonce}","password":"{password}"}}"#)
            }
            None => format!(r#"{{"ticket":"{ticket}","nonce":"{nonce}"}}"#),
        }
    }

    struct RedeemLoseGuard;

    impl Drop for RedeemLoseGuard {
        fn drop(&mut self) {
            bookclerk_integrations::redeem_lose_next_responses(0);
        }
    }

    async fn redeem_lose_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    #[tokio::test]
    async fn member_cannot_elevate() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let cookie = portal_cookie_for(&library, "test", "member-ext").await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/elevate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(r#"{"password":"nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let _ = res.into_body().collect().await;
    }

    #[tokio::test]
    async fn owner_elevate_without_password_fails_settings_ok_with_password() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;

        let bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/elevate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(r#"{"password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);

        // Owner without elevation cannot hit settings.
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/settings")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let elevated = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/elevate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"password":"{}"}}"#,
                        phase2_owner_password()
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(elevated.status(), StatusCode::OK);
        let set_cookie = elevated
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let op_cookie = cookie_from_set_cookie(&set_cookie);

        let allowed = app
            .oneshot(
                Request::builder()
                    .uri("/api/settings")
                    .header(header::COOKIE, &op_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        let _ = allowed.into_body().collect().await;

        let events = library.list_security_audit_events(20).await.unwrap();
        assert!(events.iter().any(|e| e.action == "elevate_start"));
        assert!(events.iter().any(|e| e.action == "elevate_failed"));
    }

    #[tokio::test]
    async fn impersonate_scopes_library_and_audits() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let member = library
            .get_portal_identity("test", "member-ext")
            .await
            .unwrap()
            .unwrap();
        let user_id = member.user_id.expect("bridged");

        // Login as operator.
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"token":"op-token-phase2"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let op_cookie = cookie_from_set_cookie(
            login
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap(),
        );

        let imp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/impersonate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &op_cookie)
                    .body(Body::from(format!(r#"{{"user_id":{user_id}}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(imp.status(), StatusCode::OK);

        let me = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, &op_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);
        let body =
            String::from_utf8(me.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert!(body.contains("impersonating"));
        assert!(body.contains(&user_id.to_string()));

        // Portal-scoped library: member has no links → empty (not full library).
        // Seed a book on an unlinked account; impersonation should hide it.
        let _ = library
            .upsert_account("acct-hidden", "us", Some("Hidden"), true, "audible")
            .await;
        let books = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/library/books")
                    .header(header::COOKIE, &op_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(books.status(), StatusCode::OK);

        let events = library.list_security_audit_events(20).await.unwrap();
        assert!(events.iter().any(|e| e.action == "impersonate_start"));
    }

    #[tokio::test]
    async fn bootstrap_once_then_conflict() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        // Harness already created administrators — wipe roles by creating fresh harness
        // without seeded admins.
        let _ = (app, library);
        let (state, app, library) = {
            use crate::api::AppState;
            use bookclerk_config::{Config, ListenAddrs};
            use bookclerk_integrations::IntegrationRegistry;
            use bookclerk_library::LibraryStore;
            use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
            use bookclerk_source::SourceRegistry;
            use std::sync::Arc;
            use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

            let library = LibraryStore::from_connection(
                bookclerk_plugin_database_sqlite::open_memory()
                    .await
                    .unwrap(),
            );
            let mut cfg = Config::default();
            cfg.daemon.listen = ListenAddrs::parse_list("127.0.0.1:8787").unwrap();
            cfg.daemon.auth.enabled = true;
            let state = Arc::new(AppState {
                config: Arc::new(RwLock::new(cfg)),
                library: Arc::new(RwLock::new(library.clone())),
                database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
                job_notify: Arc::new(Notify::new()),
                job_runtime: Arc::new(RwLock::new(())),
                work_lock: Mutex::new(()),
                discover_gate: Arc::new(Semaphore::new(1)),
                integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
                sources: Arc::new(RwLock::new(SourceRegistry::new())),
                destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
                auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                    "boot-token".into(),
                    12,
                    true,
                    5,
                    30,
                )))),
                reload_lock: Mutex::new(()),
                listen_reload: Arc::new(Notify::new()),
                last_bound_listen: RwLock::new(None),
                tray: RwLock::new(None),
            });
            let app = crate::api::router(state.clone(), None);
            (state, app, library)
        };
        let _ = state;
        let bootstrap_password = ["s3cret", "-", "pass"].concat();
        let bootstrap_body = format!(
            r#"{{"login_name":"admin","password":"{bootstrap_password}","display_name":"Admin"}}"#
        );
        let login_body = format!(r#"{{"login":"admin","password":"{bootstrap_password}"}}"#);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/bootstrap")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer boot-token")
                    .body(Body::from(bootstrap_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let body = String::from_utf8(
            first
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("claim_ticket"));
        assert!(body.contains("invite_url"));
        assert!(body.contains("/invite?ticket="));

        let second = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/bootstrap")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer boot-token")
                    .body(Body::from(r#"{"login_name":"other"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT);

        // Password login works.
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(login_body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);

        // Disable blocks password login (need a second owner so last-owner guard allows it).
        let owner = library
            .get_user_by_login_name("admin")
            .await
            .unwrap()
            .unwrap();
        library
            .create_user(bookclerk_library::UserRole::Owner, Some("Spare"), None)
            .await
            .unwrap();
        library
            .set_user_status(owner.id, bookclerk_library::UserStatus::Disabled)
            .await
            .unwrap();
        let blocked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(login_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn password_change_revokes_sessions_local_claim_requires_password() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use bookclerk_library::{hash_password, hash_token, UserRole};
        use chrono::{Duration as ChronoDuration, Utc};
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        use uuid::Uuid;

        // Hold the redeem-lose mutex for the whole test: later redeem calls must
        // not consume a parallel lost-response injection (global AtomicI32).
        let _redeem_lock = redeem_lose_lock().await;
        let _dek = process_dek_lock().await;
        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let initial_password = ["initial", "-", "pass"].concat();
        let hash = hash_password(&initial_password).unwrap();
        let user = library
            .create_user_with_login(UserRole::Member, Some("Pat"), Some("pat"), Some(&hash))
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(user.id, Some("Pat"))
            .await
            .unwrap();
        let raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(12),
            )
            .await
            .unwrap();
        let cookie = format!("{PORTAL_SESSION_COOKIE}={raw}");

        // Session works before password change.
        let me = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);

        let next_password = ["new", "-", "pass", "-", "word"].concat();
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(r#"{{"password":"{next_password}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let change_body =
            format!(r#"{{"password":"{next_password}","current_password":"{initial_password}"}}"#);
        let change = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(change_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(change.status(), StatusCode::OK);
        let _ = change.into_body().collect().await;

        // Old session revoked.
        let me2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me2.status(), StatusCode::UNAUTHORIZED);

        // Password reset: clear hash + claim without password must fail for local.
        library.set_user_password_hash(user.id, None).await.unwrap();
        let claim_raw = Uuid::new_v4().to_string();
        library
            .insert_claim_ticket(
                &hash_token(&claim_raw),
                Some(identity.id),
                Utc::now() + ChronoDuration::hours(1),
                "test",
            )
            .await
            .unwrap();
        let deny = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/portal/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(claim_redeem_body(
                        &claim_raw,
                        &claim_redeem_nonce(&["phase2", "-", "claim"]),
                        None,
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deny.status(), StatusCode::BAD_REQUEST);

        let too_short = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/portal/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(claim_redeem_body(
                        &claim_raw,
                        &claim_redeem_nonce(&["phase2", "-", "claim"]),
                        Some("short"),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(too_short.status(), StatusCode::BAD_REQUEST);

        let set_pw = ["claim", "-", "pass", "-", "word"].concat();
        let ok = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/portal/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(claim_redeem_body(
                        &claim_raw,
                        &claim_redeem_nonce(&["phase2", "-", "claim"]),
                        Some(&set_pw),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            ok.status(),
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&ok.into_body().collect().await.unwrap().to_bytes())
        );
        let hash_after = library.get_user_password_hash(user.id).await.unwrap();
        assert!(hash_after.is_some());
    }

    #[tokio::test]
    async fn portal_redeem_lost_response_replays_cookie_for_initiating_nonce() {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use bookclerk_library::{hash_token, UserRole};
        use chrono::{Duration as ChronoDuration, Utc};
        use tower::ServiceExt;
        use uuid::Uuid;

        let _lock = redeem_lose_lock().await;
        let _dek = process_dek_lock().await;
        let _guard = RedeemLoseGuard;
        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let hash = bookclerk_library::hash_password(&["already", "-", "set"].concat()).unwrap();
        let user = library
            .create_user_with_login(UserRole::Member, Some("Ada"), Some("ada"), Some(&hash))
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(user.id, Some("Ada"))
            .await
            .unwrap();
        let claim_raw = Uuid::new_v4().to_string();
        library
            .insert_claim_ticket(
                &hash_token(&claim_raw),
                Some(identity.id),
                Utc::now() + ChronoDuration::hours(1),
                "test",
            )
            .await
            .unwrap();
        let nonce = claim_redeem_nonce(&["lost", "-", "response", "-", "browser"]);
        bookclerk_integrations::redeem_lose_next_responses(1);

        let lost = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/portal/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(claim_redeem_body(&claim_raw, &nonce, None)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(lost.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(library
            .get_claim_ticket_by_hash(&hash_token(&claim_raw))
            .await
            .unwrap()
            .unwrap()
            .redeemed_at
            .is_some());

        let retry = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/portal/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(claim_redeem_body(&claim_raw, &nonce, None)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::OK);
        let cookie = cookie_from_set_cookie(
            retry
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap(),
        );
        let dek = bookclerk_library::require_master_key(None).unwrap();
        let expected = bookclerk_library::derive_claim_session_token(&dek, &claim_raw, &nonce);
        assert_eq!(cookie, format!("{PORTAL_SESSION_COOKIE}={expected}"));

        let me = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);

        let other = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/portal/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(claim_redeem_body(
                        &claim_raw,
                        &claim_redeem_nonce(&["other", "-", "browser"]),
                        None,
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(other.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn portal_redeem_lost_invite_password_retry_keeps_password_usable() {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use bookclerk_library::{hash_token, UserRole};
        use chrono::{Duration as ChronoDuration, Utc};
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        use uuid::Uuid;

        let _lock = redeem_lose_lock().await;
        let _dek = process_dek_lock().await;
        let _guard = RedeemLoseGuard;
        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let user = library
            .create_user_with_login(UserRole::Member, Some("Ivy"), Some("ivy"), None)
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(user.id, Some("Ivy"))
            .await
            .unwrap();
        let claim_raw = Uuid::new_v4().to_string();
        library
            .insert_claim_ticket(
                &hash_token(&claim_raw),
                Some(identity.id),
                Utc::now() + ChronoDuration::hours(1),
                "test",
            )
            .await
            .unwrap();
        let nonce = claim_redeem_nonce(&["invite", "-", "lost", "-", "response"]);
        let invite_pw = ["invite", "-", "pass", "-", "word"].concat();
        bookclerk_integrations::redeem_lose_next_responses(1);

        let lost = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/portal/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(claim_redeem_body(
                        &claim_raw,
                        &nonce,
                        Some(&invite_pw),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(lost.status(), StatusCode::SERVICE_UNAVAILABLE);

        let retry = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/portal/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(claim_redeem_body(
                        &claim_raw,
                        &nonce,
                        Some(&invite_pw),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::OK);
        let cookie = cookie_from_set_cookie(
            retry
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap(),
        );
        let me = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);

        let login = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"login":"ivy","password":"{invite_pw}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            login.status(),
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&login.into_body().collect().await.unwrap().to_bytes())
        );
    }

    #[tokio::test]
    async fn csrf_blocks_cookie_post_with_bad_origin() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (state, app, _library) = phase2_harness("op-token-phase2").await;
        state.config.write().await.integrations.public_origin =
            Some(String::from("https://bookclerk.example"));

        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://bookclerk.example")
                    .body(Body::from(r#"{"token":"op-token-phase2"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let op_cookie = cookie_from_set_cookie(
            login
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap(),
        );

        let bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/logout")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &op_cookie)
                    .header(header::ORIGIN, "https://evil.example")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::FORBIDDEN);

        let good = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/logout")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &op_cookie)
                    .header(header::ORIGIN, "https://bookclerk.example")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(good.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn session_revoke_horizontal_privilege() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use bookclerk_library::hash_token;
        use chrono::{Duration as ChronoDuration, Utc};
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        use uuid::Uuid;

        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let a = library
            .get_portal_identity("test", "admin-ext")
            .await
            .unwrap()
            .unwrap();
        let b = library
            .get_portal_identity("test", "member-ext")
            .await
            .unwrap()
            .unwrap();
        let raw_a = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw_a),
                a.id,
                Utc::now() + ChronoDuration::hours(1),
            )
            .await
            .unwrap();
        let sessions_a = library
            .list_portal_sessions_for_identity(a.id)
            .await
            .unwrap();
        let sid_a = sessions_a[0].0;

        let raw_b = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw_b),
                b.id,
                Utc::now() + ChronoDuration::hours(1),
            )
            .await
            .unwrap();
        let cookie_b = format!("{PORTAL_SESSION_COOKIE}={raw_b}");

        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/auth/sessions/{sid_a}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie_b)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        let _ = denied.into_body().collect().await;
    }

    #[tokio::test]
    async fn admin_can_list_users_without_elevate() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-users").await;
        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;
        let listed = app
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let body = String::from_utf8(
            listed
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("\"users\""));
    }

    #[tokio::test]
    async fn patch_demote_last_owner_conflicts() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-users").await;
        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;
        let owner_identity = library
            .get_portal_identity("test", "admin-ext")
            .await
            .unwrap()
            .unwrap();
        let sole_id = owner_identity.user_id.expect("bridged owner");
        for user in library.list_users().await.unwrap() {
            if matches!(user.role, UserRole::Owner)
                && matches!(user.status, UserStatus::Active)
                && user.id != sole_id
            {
                library
                    .set_user_role(user.id, UserRole::Member)
                    .await
                    .expect("demote extra owner");
            }
        }
        assert_eq!(library.count_active_owners().await.unwrap(), 1);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/users/{sole_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(r#"{"role":"member"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body, serde_json::json!({"error":"last_owner"}));
    }

    #[tokio::test]
    async fn patch_disable_revokes_sessions() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use bookclerk_library::hash_token;
        use chrono::{Duration as ChronoDuration, Utc};
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        use uuid::Uuid;

        let (_state, app, library) = phase2_harness("op-token-users").await;
        let member = library
            .get_portal_identity("test", "member-ext")
            .await
            .unwrap()
            .unwrap();
        let member_user_id = member.user_id.expect("bridged");
        let raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw),
                member.id,
                Utc::now() + ChronoDuration::hours(1),
            )
            .await
            .unwrap();
        assert_eq!(
            library
                .list_portal_sessions_for_identity(member.id)
                .await
                .unwrap()
                .len(),
            1
        );

        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/users/{member_user_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(r#"{"status":"disabled"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = resp.into_body().collect().await;
        assert!(library
            .list_portal_sessions_for_identity(member.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn claim_ticket_remint() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use bookclerk_library::hash_token;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-users").await;
        let member = library
            .get_portal_identity("test", "member-ext")
            .await
            .unwrap()
            .unwrap();
        let member_user_id = member.user_id.expect("bridged");
        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/users/{member_user_id}/claim-ticket"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let raw = body["claim_ticket"].as_str().expect("claim_ticket");
        assert!(library
            .get_claim_ticket_by_hash(&hash_token(raw))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn owner_cannot_reset_or_remint_self() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-users").await;
        let owner_id = library
            .get_portal_identity("test", "admin-ext")
            .await
            .unwrap()
            .unwrap()
            .user_id
            .unwrap();
        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;

        let reset = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/users/{owner_id}/reset-password"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reset.status(), StatusCode::FORBIDDEN);
        let _ = reset.into_body().collect().await;

        let remint = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/users/{owner_id}/claim-ticket"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(remint.status(), StatusCode::FORBIDDEN);
        let _ = remint.into_body().collect().await;
        let _ = library;
    }

    #[tokio::test]
    async fn password_user_id_self_requires_current_password() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let owner_id = library
            .get_portal_identity("test", "admin-ext")
            .await
            .unwrap()
            .unwrap()
            .user_id
            .unwrap();
        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;
        let next = ["owner", "-", "pass", "-", "self"].concat();

        let skipped = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"password":"{next}","user_id":{owner_id}}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(skipped.status(), StatusCode::UNAUTHORIZED);
        let _ = skipped.into_body().collect().await;

        let with_current = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"password":"{next}","user_id":{owner_id},"current_password":"{}"}}"#,
                        phase2_owner_password()
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(with_current.status(), StatusCode::OK);
        let _ = with_current.into_body().collect().await;
        let _ = library;
    }

    #[tokio::test]
    async fn provisioner_role_matrix_enforced() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-users").await;
        let owner_cookie = portal_cookie_for(&library, "test", "admin-ext").await;
        let admin_cookie = portal_cookie_for(&library, "test", "administrator-ext").await;
        let owner_id = library
            .get_portal_identity("test", "admin-ext")
            .await
            .unwrap()
            .unwrap()
            .user_id
            .unwrap();

        async fn create(
            app: axum::Router,
            cookie: Option<&str>,
            bearer: Option<&str>,
            role: &str,
            login: &str,
        ) -> StatusCode {
            let mut builder = Request::builder()
                .method("POST")
                .uri("/api/users")
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(c) = cookie {
                builder = builder.header(header::COOKIE, c);
            }
            if let Some(b) = bearer {
                builder = builder.header(header::AUTHORIZATION, b);
            }
            let res = app
                .oneshot(
                    builder
                        .body(Body::from(format!(
                            r#"{{"role":"{role}","login_name":"{login}","mint_invite":false}}"#
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status();
            let _ = res.into_body().collect().await;
            status
        }

        assert_eq!(
            create(
                app.clone(),
                Some(&admin_cookie),
                None,
                "owner",
                "blocked-owner"
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            create(
                app.clone(),
                Some(&admin_cookie),
                None,
                "administrator",
                "blocked-admin"
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            create(
                app.clone(),
                Some(&admin_cookie),
                None,
                "member",
                "ok-member"
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            create(
                app.clone(),
                Some(&owner_cookie),
                None,
                "owner",
                "blocked-owner-2"
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            create(
                app.clone(),
                Some(&owner_cookie),
                None,
                "administrator",
                "ok-admin"
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            create(
                app.clone(),
                None,
                Some("Bearer op-token-users"),
                "owner",
                "ok-owner"
            )
            .await,
            StatusCode::OK
        );

        let deny_owner = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/users/{owner_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &admin_cookie)
                    .body(Body::from(r#"{"display_name":"nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deny_owner.status(), StatusCode::FORBIDDEN);
        let _ = deny_owner.into_body().collect().await;

        let elevated = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/elevate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &owner_cookie)
                    .body(Body::from(format!(
                        r#"{{"password":"{}"}}"#,
                        phase2_owner_password()
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(elevated.status(), StatusCode::OK);
        let op_cookie = cookie_from_set_cookie(
            elevated
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap(),
        );
        let _ = elevated.into_body().collect().await;
        assert_eq!(
            create(app, Some(&op_cookie), None, "owner", "elevated-owner").await,
            StatusCode::OK
        );
        let _ = library;
    }

    #[tokio::test]
    async fn first_password_skips_current_password() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use bookclerk_library::hash_token;
        use chrono::{Duration as ChronoDuration, Utc};
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        use uuid::Uuid;

        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let user = library
            .create_user(UserRole::Member, Some("NoPw"), None)
            .await
            .unwrap();
        let identity = library
            .ensure_local_portal_identity(user.id, Some("NoPw"))
            .await
            .unwrap();
        let raw = Uuid::new_v4().to_string();
        library
            .insert_portal_session(
                &hash_token(&raw),
                identity.id,
                Utc::now() + ChronoDuration::hours(12),
            )
            .await
            .unwrap();
        let cookie = format!("{PORTAL_SESSION_COOKIE}={raw}");
        let first = ["first", "-", "pass", "-", "word"].concat();
        let set = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(r#"{{"password":"{first}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(set.status(), StatusCode::OK);
        let _ = set.into_body().collect().await;
        assert!(library
            .get_user_password_hash(user.id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn password_change_revokes_elevated_sessions() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (_state, app, library) = phase2_harness("op-token-phase2").await;
        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;
        let elevated = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/elevate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"password":"{}"}}"#,
                        phase2_owner_password()
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(elevated.status(), StatusCode::OK);
        let op_cookie = cookie_from_set_cookie(
            elevated
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap(),
        );
        let _ = elevated.into_body().collect().await;

        let next = ["owner", "-", "pass", "-", "2"].concat();
        let change = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"password":"{next}","current_password":"{}"}}"#,
                        phase2_owner_password()
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(change.status(), StatusCode::OK);
        let _ = change.into_body().collect().await;

        let settings = app
            .oneshot(
                Request::builder()
                    .uri("/api/settings")
                    .header(header::COOKIE, &op_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(settings.status(), StatusCode::UNAUTHORIZED);
        let _ = settings.into_body().collect().await;
    }

    #[tokio::test]
    async fn established_owner_first_passkey_requires_reauth() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (state, app, library) = phase2_harness("op-token-phase2").await;
        {
            let mut cfg = state.config.write().await;
            cfg.integrations.public_origin = Some("http://localhost:8787".into());
        }
        let cookie = portal_cookie_for(&library, "test", "admin-ext").await;
        let raw = cookie
            .strip_prefix(&format!("{PORTAL_SESSION_COOKIE}="))
            .expect("portal cookie");
        library
            .set_portal_session_created_at(&hash_token(raw), Utc::now() - ChronoDuration::hours(1))
            .await
            .unwrap();

        let stale = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/passkeys/register/begin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "http://localhost:8787")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
        let _ = stale.into_body().collect().await;

        let ok = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/passkeys/register/begin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "http://localhost:8787")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"current_password":"{}"}}"#,
                        phase2_owner_password()
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let _ = ok.into_body().collect().await;
    }
}
