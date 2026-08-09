//! Operator and portal session authentication for the daemon HTTP API.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::Request;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_integrations::portal_identity_from_headers;
use bookclerk_library::{portal_prefs_key, PortalIdentity, OPERATOR_PREFS_KEY};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::timeout;
use uuid::Uuid;

use crate::api::AppState;

pub const SESSION_COOKIE: &str = "bookclerk_operator_session";
const AUTH_DB_TIMEOUT: Duration = Duration::from_secs(3);

/// Peer IP for login throttling (`ConnectInfo` when available, else `"unknown"`).
pub(crate) struct ClientIp(String);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(ConnectInfo(addr)) = parts.extensions.get::<ConnectInfo<SocketAddr>>() {
            return Ok(Self(addr.ip().to_string()));
        }
        Ok(Self("unknown".into()))
    }
}

#[derive(Debug)]
struct LoginThrottleBucket {
    failures: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
}

#[derive(Debug)]
pub struct OperatorAuthState {
    pub token: String,
    pub sessions: Mutex<HashMap<String, Instant>>,
    pub session_ttl: Duration,
    pub enabled: bool,
    login_max_failures: u32,
    login_window: Duration,
    login_lockout: Duration,
    login_attempts: Mutex<HashMap<String, LoginThrottleBucket>>,
}

impl OperatorAuthState {
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
            sessions: Mutex::new(HashMap::new()),
            session_ttl: Duration::from_secs(session_ttl_hours.saturating_mul(3600).max(3600)),
            enabled,
            login_max_failures: login_max_failures.max(1),
            // Failures count within the same period as the lockout by default.
            login_window: login_lockout,
            login_lockout,
            login_attempts: Mutex::new(HashMap::new()),
        }
    }

    fn token_matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.token.as_bytes(), candidate.as_bytes())
    }

    /// `None` = allowed; `Some(retry_after)` = locked out.
    async fn login_throttle_check(&self, client_key: &str) -> Option<Duration> {
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

    async fn record_login_failure(&self, client_key: &str) -> Option<Duration> {
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

    async fn clear_login_failures(&self, client_key: &str) {
        self.login_attempts.lock().await.remove(client_key);
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct AuthMeResponse {
    pub authenticated: bool,
    /// `operator`, `portal`, or omitted when anonymous.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Configured post-auth landing view (`discover` / `library` / `accounts`).
    /// Loaded from per-user DB preferences (not config.toml).
    pub default_view: String,
    /// Whether this session may acquire / scan / manage jobs.
    pub can_acquire: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal: Option<PortalMeInfo>,
}

#[derive(Debug, Serialize)]
pub struct PortalMeInfo {
    pub identity_id: i64,
    pub provider: String,
    pub external_user_id: String,
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    ok: bool,
    role: String,
    default_view: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    ClientIp(client_key): ClientIp,
    Json(body): Json<LoginRequest>,
) -> Result<Response, StatusCode> {
    let auth = state.auth.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
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
    let session_id = Uuid::new_v4().to_string();
    {
        let mut sessions = auth.sessions.lock().await;
        prune_sessions(&mut sessions, auth.session_ttl);
        sessions.insert(session_id.clone(), Instant::now());
    }
    let max_age = auth.session_ttl.as_secs();
    let cookie =
        format!("{SESSION_COOKIE}={session_id}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(LoginResponse {
            ok: true,
            role: String::from("operator"),
            default_view,
        }),
    )
        .into_response())
}

fn too_many_requests(retry_after: Duration) -> Response {
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

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(auth) = state.auth.as_ref() {
        if let Some(session_id) = session_id_from_headers(&headers) {
            auth.sessions.lock().await.remove(&session_id);
        }
    }
    let mut hdrs = HeaderMap::new();
    for cookie in [
        format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
        String::from("bookclerk_portal_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
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

pub async fn me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let Some(auth) = state.auth.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(AuthMeResponse {
                authenticated: false,
                role: None,
                default_view: String::from("discover"),
                can_acquire: false,
                portal: None,
            }),
        );
    };

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
                portal: None,
            }),
        );
    }

    if authorize_operator(auth, &headers).await {
        let library = state.library_snapshot().await;
        let default_view = default_view_for_subject(&library, OPERATOR_PREFS_KEY, None).await;
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(String::from("operator")),
                default_view,
                can_acquire: true,
                portal: None,
            }),
        );
    }

    let library = state.library_snapshot().await;
    if let Some(identity) = timed_portal_identity_from_headers(&library, &headers).await {
        let key = portal_prefs_key(identity.id);
        let default_view = default_view_for_subject(&library, &key, Some(identity.id)).await;
        return (
            StatusCode::OK,
            Json(AuthMeResponse {
                authenticated: true,
                role: Some(String::from("portal")),
                default_view,
                can_acquire: false,
                portal: Some(PortalMeInfo {
                    identity_id: identity.id,
                    provider: identity.provider,
                    external_user_id: identity.external_user_id,
                    label: identity.label,
                }),
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
            portal: None,
        }),
    )
}

pub async fn require_operator_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(auth) = state.auth.as_ref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    if !auth.enabled {
        return Ok(next.run(req).await);
    }
    if authorize_operator(auth, req.headers()).await {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Allow operator sessions **or** portal sessions (Discover / read-only library).
pub async fn require_operator_or_portal_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(auth) = state.auth.as_ref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    if !auth.enabled {
        return Ok(next.run(req).await);
    }
    if authorize_operator(auth, req.headers()).await {
        return Ok(next.run(req).await);
    }
    let library = state.library_snapshot().await;
    if timed_portal_identity_from_headers(&library, req.headers())
        .await
        .is_some()
    {
        return Ok(next.run(req).await);
    }
    Err(StatusCode::UNAUTHORIZED)
}

/// Resolve portal identity when the caller is not an operator.
pub async fn caller_portal_identity(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<PortalIdentity> {
    let auth = state.auth.as_ref()?;
    if !auth.enabled {
        return None;
    }
    if authorize_operator(auth, headers).await {
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
    if let Some(auth) = state.auth.as_ref() {
        if !auth.enabled || authorize_operator(auth, headers).await {
            return Ok((OPERATOR_PREFS_KEY.to_string(), None));
        }
    } else {
        return Ok((OPERATOR_PREFS_KEY.to_string(), None));
    }
    let library = state.library_snapshot().await;
    match timeout(
        AUTH_DB_TIMEOUT,
        portal_identity_from_headers(&library, headers),
    )
    .await
    {
        Ok(Some(identity)) => Ok((portal_prefs_key(identity.id), Some(identity.id))),
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

async fn timed_portal_identity_from_headers(
    library: &bookclerk_library::LibraryStore,
    headers: &HeaderMap,
) -> Option<PortalIdentity> {
    match timeout(
        AUTH_DB_TIMEOUT,
        portal_identity_from_headers(library, headers),
    )
    .await
    {
        Ok(identity) => identity,
        Err(_) => {
            tracing::warn!("portal identity lookup timed out");
            None
        }
    }
}

async fn authorize_operator(auth: &OperatorAuthState, headers: &HeaderMap) -> bool {
    if let Some(token) = bearer_token(headers) {
        if auth.token_matches(token) {
            return true;
        }
    }
    let Some(session_id) = session_id_from_headers(headers) else {
        return false;
    };
    let mut sessions = auth.sessions.lock().await;
    prune_sessions(&mut sessions, auth.session_ttl);
    sessions
        .get(&session_id)
        .is_some_and(|created| created.elapsed() < auth.session_ttl)
}

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

fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn prune_sessions(sessions: &mut HashMap<String, Instant>, ttl: Duration) {
    sessions.retain(|_, created| created.elapsed() < ttl);
}

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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
pub fn normalize_default_view(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "library" => String::from("library"),
        "accounts" => String::from("accounts"),
        "wishlist" => String::from("wishlist"),
        _ => String::from("discover"),
    }
}

#[cfg(test)]
mod tests {
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
}
