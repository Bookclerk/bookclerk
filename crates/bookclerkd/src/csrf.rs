//! CSRF Origin checks for cookie-authenticated mutating `/api/*` requests.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::api::AppState;
use crate::auth::{PORTAL_SESSION_COOKIE, SESSION_COOKIE};

/// Require `Origin` (or `Referer`) matching `integrations.public_origin` host,
/// or same-origin `Host`, for cookie-authenticated state-changing `/api/*`
/// requests. Exempts login / redeem / password / bootstrap paths and the
/// Apple `form_post` OIDC callback (cross-site POST; bound by the
/// `bookclerk_oidc_tx` cookie + `state`, not Origin).
pub async fn require_csrf_for_cookie_api(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if !is_mutating(req.method()) {
        return Ok(next.run(req).await);
    }
    let path = req.uri().path();
    if !path.starts_with("/api/") {
        return Ok(next.run(req).await);
    }
    if is_csrf_exempt(path) {
        return Ok(next.run(req).await);
    }
    if !has_session_cookie(req.headers()) {
        // Bearer / anonymous — CSRF does not apply.
        return Ok(next.run(req).await);
    }
    let cfg = state.config.read().await;
    let public_origin = cfg.integrations.public_origin.clone();
    drop(cfg);
    if origin_ok(req.headers(), public_origin.as_deref()) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

/// True for POST, PUT, PATCH, and DELETE (state-changing methods).
fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// True for login, redeem, bootstrap, and IdP callback paths that cannot send Origin.
fn is_csrf_exempt(path: &str) -> bool {
    matches!(
        path,
        "/api/auth/login"
            | "/api/auth/password"
            | "/api/auth/bootstrap"
            | "/api/auth/tray-handoff"
            | "/api/auth/tray-handoff/prepare"
            | "/api/portal/redeem"
            | "/api/portal/login/integration"
            | "/api/auth/oidc/callback"
            | "/api/auth/passkeys/login/begin"
            | "/api/auth/passkeys/login/finish"
    )
}

/// True when the request carries an operator or portal session cookie (CSRF applies).
fn has_session_cookie(headers: &axum::http::HeaderMap) -> bool {
    let Some(cookie) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    cookie.split(';').any(|part| {
        let part = part.trim();
        part.starts_with(&format!("{SESSION_COOKIE}="))
            || part.starts_with(&format!("{PORTAL_SESSION_COOKIE}="))
    })
}

/// Accepts Origin/Referer matching `public_origin`, or same-host when origin is unset.
fn origin_ok(headers: &axum::http::HeaderMap, public_origin: Option<&str>) -> bool {
    let host_header = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        return origin_matches(origin, public_origin, host_header.as_deref());
    }
    if let Some(referer) = headers.get(header::REFERER).and_then(|v| v.to_str().ok()) {
        return origin_matches(referer, public_origin, host_header.as_deref());
    }
    // No Origin/Referer: allow same-process tools that send Host only when
    // public_origin is unset (loopback / direct). When public_origin is set,
    // require an explicit Origin.
    if public_origin.is_none() {
        return true;
    }
    false
}

/// Compares the Origin/Referer host to `public_origin`, or to `Host` when unset.
fn origin_matches(origin_or_url: &str, public_origin: Option<&str>, host: Option<&str>) -> bool {
    let origin_host = host_from_url(origin_or_url);
    if let Some(pub_origin) = public_origin {
        if let Some(expected) = host_from_url(pub_origin) {
            return origin_host.as_deref() == Some(expected.as_str());
        }
    }
    // public_origin unset → same-origin Host check.
    match (origin_host.as_deref(), host) {
        (Some(o), Some(h)) => hosts_equal(o, h),
        _ => false,
    }
}

/// Host\[:port\] from an http(s) URL or bare host; `None` for other schemes.
fn host_from_url(url: &str) -> Option<String> {
    let url = url.trim();
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let hostport = rest.split('/').next()?.split('?').next()?;
        return Some(hostport.to_ascii_lowercase());
    }
    // Bare host:port
    if url.contains("://") {
        return None;
    }
    Some(url.split('/').next()?.to_ascii_lowercase())
}

/// Case-insensitive host\[:port\] equality.
fn hosts_equal(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use axum::http::HeaderValue;

    #[test]
    fn public_origin_requires_matching_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("bookclerk.example"));
        assert!(!origin_ok(&headers, Some("https://bookclerk.example"),));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://bookclerk.example"),
        );
        assert!(origin_ok(&headers, Some("https://bookclerk.example")));
    }

    #[test]
    fn unset_public_origin_allows_same_host() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8787"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8787"));
        assert!(origin_ok(&headers, None));
    }

    #[test]
    fn oidc_callback_is_csrf_exempt() {
        assert!(is_csrf_exempt("/api/auth/oidc/callback"));
        assert!(is_csrf_exempt("/api/auth/tray-handoff"));
        assert!(is_csrf_exempt("/api/auth/tray-handoff/prepare"));
        assert!(!is_csrf_exempt("/api/auth/logout"));
    }
}
