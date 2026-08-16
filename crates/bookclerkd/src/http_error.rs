//! Branded HTTP error responses (HTML or JSON) for the daemon control plane.

use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// Rewrite empty 4xx/5xx responses into Bookclerk-branded HTML or JSON.
///
/// Negotiation prefers the request `Content-Type`, then `Accept`, then whether
/// the path looks like an API route (`/api/…` and legacy control-plane paths).
/// Responses that already set `Content-Type` (JSON handlers, static files, SPA)
/// are left unchanged.
pub async fn brand_error_responses(req: Request, next: Next) -> Response {
    let prefer_json = wants_json(req.headers(), req.uri().path());
    let res = next.run(req).await;
    let status = res.status();
    if !status.is_client_error() && !status.is_server_error() {
        return res;
    }
    if res.headers().contains_key(header::CONTENT_TYPE) {
        return res;
    }
    ErrorBody::new(status).into_response_for(prefer_json)
}

#[derive(Debug, Clone)]
/// Branded 4xx/5xx payload (slug, message, status) before HTML/JSON encoding.
struct ErrorBody {
    /// HTTP status written on the rewritten response.
    status: StatusCode,
    /// Stable machine slug (`unauthorized`, `not_found`, …).
    code: &'static str,
    /// Operator-facing explanation shown in HTML or JSON.
    message: &'static str,
}

impl ErrorBody {
    /// Maps `status` to a slug and operator-facing message via [`describe`].
    fn new(status: StatusCode) -> Self {
        let (code, message) = describe(status);
        Self {
            status,
            code,
            message,
        }
    }

    /// Encodes this body as JSON or branded HTML according to `prefer_json`.
    fn into_response_for(self, prefer_json: bool) -> Response {
        if prefer_json {
            (
                self.status,
                Json(ErrorJson {
                    error: self.code,
                    message: self.message,
                    status: self.status.as_u16(),
                }),
            )
                .into_response()
        } else {
            (self.status, Html(render_html(self.status, self.message))).into_response()
        }
    }
}

/// Branded HTML error for browser document routes (`GET /invite`, …).
///
/// # Arguments
///
/// * `status` - HTTP status written on the response.
/// * `message` - Operator-facing explanation shown in the page body.
///
/// # Returns
///
/// HTML response with `Content-Type: text/html`.
pub fn document_error(status: StatusCode, message: &'static str) -> Response {
    ErrorBody {
        status,
        code: describe(status).0,
        message,
    }
    .into_response_for(false)
}

#[derive(Debug, Serialize)]
/// JSON error object (`error`, `message`, numeric `status`) for API clients.
struct ErrorJson {
    /// Machine slug (same as [`ErrorBody::code`]).
    error: &'static str,
    /// Operator-facing explanation.
    message: &'static str,
    /// Numeric HTTP status (duplicates the response status for clients).
    status: u16,
}

/// Maps common statuses to a slug and Bookclerk-branded message.
fn describe(status: StatusCode) -> (&'static str, &'static str) {
    match status {
        StatusCode::BAD_REQUEST => ("bad_request", "The request could not be understood."),
        StatusCode::UNAUTHORIZED => (
            "unauthorized",
            "Operator authentication is required. Sign in with your operator token.",
        ),
        StatusCode::FORBIDDEN => ("forbidden", "You do not have access to this resource."),
        StatusCode::NOT_FOUND => ("not_found", "That page or resource was not found."),
        StatusCode::GONE => ("gone", "This resource is no longer available."),
        StatusCode::METHOD_NOT_ALLOWED => ("method_not_allowed", "This method is not allowed."),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => (
            "unsupported_media_type",
            "Send JSON with Content-Type: application/json.",
        ),
        StatusCode::TOO_MANY_REQUESTS => {
            ("too_many_requests", "Too many requests. Try again shortly.")
        }
        StatusCode::INTERNAL_SERVER_ERROR => (
            "internal_error",
            "Something went wrong on the Bookclerk daemon.",
        ),
        StatusCode::BAD_GATEWAY => ("bad_gateway", "An upstream service failed."),
        StatusCode::SERVICE_UNAVAILABLE => (
            "service_unavailable",
            "The Bookclerk daemon is not ready to handle this request.",
        ),
        StatusCode::GATEWAY_TIMEOUT => ("gateway_timeout", "The upstream request timed out."),
        other => (
            status_code_slug(other),
            other.canonical_reason().unwrap_or("Request failed"),
        ),
    }
}

/// Fallback kebab slug for statuses not listed in [`describe`].
fn status_code_slug(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        405 => "method_not_allowed",
        408 => "request_timeout",
        409 => "conflict",
        410 => "gone",
        413 => "payload_too_large",
        415 => "unsupported_media_type",
        422 => "unprocessable_entity",
        429 => "too_many_requests",
        500 => "internal_error",
        501 => "not_implemented",
        502 => "bad_gateway",
        503 => "service_unavailable",
        504 => "gateway_timeout",
        _ => "error",
    }
}

/// Prefer JSON when the client looks like an API consumer.
fn wants_json(headers: &HeaderMap, path: &str) -> bool {
    if media_prefers_json(headers.get(header::CONTENT_TYPE)) {
        return true;
    }
    if media_prefers_html(headers.get(header::CONTENT_TYPE)) {
        return false;
    }
    match accept_preference(headers.get(header::ACCEPT)) {
        Some(true) => return true,
        Some(false) => return false,
        None => {}
    }
    path_looks_like_api(path)
}

/// True for `/api/…`, `/health`, and legacy control-plane paths.
fn path_looks_like_api(path: &str) -> bool {
    path == "/status"
        || path == "/scan"
        || path == "/acquire"
        || path == "/jobs"
        || path == "/health"
        || path.starts_with("/api/")
        || path.starts_with("/status/")
        || path.starts_with("/integrations/")
}

/// True when a `Content-Type` / media token is JSON or `+json`.
fn media_prefers_json(value: Option<&HeaderValue>) -> bool {
    let Some(raw) = value.and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let media = raw
        .split(';')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_ascii_lowercase();
    media == "application/json" || media.ends_with("+json")
}

/// True when a media token is `text/html` or XHTML.
fn media_prefers_html(value: Option<&HeaderValue>) -> bool {
    let Some(raw) = value.and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let media = raw
        .split(';')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_ascii_lowercase();
    media == "text/html" || media == "application/xhtml+xml"
}

/// `Some(true)` = prefer JSON, `Some(false)` = prefer HTML, `None` = no signal.
fn accept_preference(value: Option<&HeaderValue>) -> Option<bool> {
    let raw = value.and_then(|v| v.to_str().ok())?;
    if raw.trim() == "*/*" {
        return None;
    }
    let mut best_json: Option<f32> = None;
    let mut best_html: Option<f32> = None;
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut media = part;
        let mut q = 1.0_f32;
        if let Some((m, params)) = part.split_once(';') {
            media = m.trim();
            for param in params.split(';') {
                let param = param.trim();
                if let Some(v) = param.strip_prefix("q=") {
                    if let Ok(parsed) = v.trim().parse::<f32>() {
                        q = parsed;
                    }
                }
            }
        }
        let media = media.trim().to_ascii_lowercase();
        if media == "application/json" || media.ends_with("+json") {
            best_json = Some(best_json.map_or(q, |b| b.max(q)));
        } else if media == "text/html" || media == "application/xhtml+xml" {
            best_html = Some(best_html.map_or(q, |b| b.max(q)));
        }
    }
    match (best_json, best_html) {
        (Some(j), Some(h)) => Some(j >= h),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    }
}

/// Full wordmark (same asset as the login page), inlined so errors work without `ui/dist`.
const LOGO_SVG: &str = include_str!("../../../assets/brand/svg/bookclerk-logo.svg");

/// Inlines the wordmark and branded CSS so errors work without `ui/dist`.
fn render_html(status: StatusCode, message: &str) -> String {
    let code = status.as_u16();
    let reason = html_escape(status.canonical_reason().unwrap_or("Error"));
    let message = html_escape(message);
    let logo = LOGO_SVG
        .trim_start_matches("\u{feff}")
        .trim_start()
        .strip_prefix("<?xml version=\"1.0\" encoding=\"utf-8\"?>")
        .unwrap_or(LOGO_SVG)
        .trim_start();
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<meta name="theme-color" content="#0B3553" media="(prefers-color-scheme: light)"/>
<meta name="theme-color" content="#121c26" media="(prefers-color-scheme: dark)"/>
<title>{code} {reason} · Bookclerk</title>
<style>
:root {{
  --ink: #0b3553;
  --ink-soft: #1a4a6b;
  --teal: #3d7f7a;
  --brick: #c84a34;
  --parchment: #f3e5c6;
  --fold: #e7d3ac;
  --paper: #fbf7ee;
  --font-display: "Literata", "Iowan Old Style", "Palatino Linotype", Palatino, serif;
  --font-sans: "Source Sans 3", "Segoe UI", Candara, sans-serif;
}}
html {{ color-scheme: light; }}
@media (prefers-color-scheme: dark) {{
  :root {{
    --ink: #f3e5c6;
    --ink-soft: #d4c4a0;
    --teal: #5aa8a2;
    --brick: #e06a52;
    --parchment: #1a2834;
    --fold: #243240;
    --paper: #121c26;
  }}
  html {{ color-scheme: dark; }}
}}
* {{ box-sizing: border-box; }}
html, body {{ height: 100%; }}
body {{
  margin: 0;
  font-family: var(--font-sans);
  color: var(--ink);
  background:
    radial-gradient(1200px 600px at 10% -10%, #fff9ee 0%, transparent 55%),
    radial-gradient(900px 500px at 100% 0%, #e8f0ef 0%, transparent 50%),
    linear-gradient(180deg, var(--paper) 0%, #f0e6d4 100%);
  background-attachment: fixed;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 2rem 1.25rem;
}}
@media (prefers-color-scheme: dark) {{
  body {{
    background:
      radial-gradient(1200px 600px at 10% -10%, #1a2834 0%, transparent 55%),
      radial-gradient(900px 500px at 100% 0%, #1a3a38 0%, transparent 50%),
      linear-gradient(180deg, var(--paper) 0%, #0d1620 100%);
  }}
}}
main {{
  width: min(28rem, 100%);
  animation: fadeUp 420ms ease-out;
}}
/* Match LoginPage: full-width wordmark (`w-full` + `mb-8`). */
.brand {{
  margin: 0 0 2rem;
}}
.brand svg {{
  display: block;
  width: 100%;
  height: auto;
}}
.brand .bookclerk-wordmark {{
  fill: currentColor;
}}
.status {{
  font-size: 0.8rem;
  font-weight: 600;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  color: var(--brick);
  margin: 0 0 0.5rem;
}}
h1 {{
  font-family: var(--font-display);
  font-size: 1.85rem;
  font-weight: 700;
  letter-spacing: -0.02em;
  margin: 0 0 0.65rem;
  line-height: 1.2;
}}
p {{
  margin: 0;
  font-size: 1rem;
  line-height: 1.5;
  color: color-mix(in srgb, var(--ink) 72%, transparent);
}}
.actions {{
  display: flex;
  flex-wrap: wrap;
  gap: 0.65rem;
  margin-top: 1.75rem;
}}
a {{
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 0.55rem 1rem;
  border-radius: 0.4rem;
  font-weight: 600;
  font-size: 0.95rem;
  text-decoration: none;
}}
a.primary {{
  background: var(--ink);
  color: var(--parchment);
}}
a.primary:hover {{ background: var(--ink-soft); }}
a.secondary {{
  background: color-mix(in srgb, var(--fold) 70%, var(--paper));
  color: var(--ink);
  border: 1px solid color-mix(in srgb, var(--fold) 80%, var(--ink));
}}
a.secondary:hover {{ background: var(--fold); }}
@keyframes fadeUp {{
  from {{ opacity: 0; transform: translateY(10px); }}
  to {{ opacity: 1; transform: translateY(0); }}
}}
</style>
</head>
<body>
<main>
  <div class="brand">{logo}</div>
  <p class="status">{code} · {reason}</p>
  <h1>{reason}</h1>
  <p>{message}</p>
  <div class="actions">
    <a class="primary" href="/">Open library</a>
    <a class="secondary" href="javascript:history.back()">Go back</a>
  </div>
</main>
</body>
</html>
"##
    )
}

/// Escapes `& < > " '` so status text cannot break the error HTML.
fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    async fn forbidden() -> StatusCode {
        StatusCode::FORBIDDEN
    }

    fn app() -> Router {
        Router::new()
            .route("/api/secret", get(forbidden))
            .route("/page", get(forbidden))
            .layer(axum::middleware::from_fn(brand_error_responses))
    }

    #[tokio::test]
    async fn api_path_errors_are_json_by_default() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/api/secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "forbidden");
        assert_eq!(json["status"], 403);
    }

    #[tokio::test]
    async fn content_type_json_forces_json_on_html_path() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/page")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[tokio::test]
    async fn accept_html_serves_branded_page() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/page")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(res
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        let body = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
            .unwrap();
        assert!(body.contains("Bookclerk"));
        assert!(body.contains("viewBox=\"0 0 1800 600\""));
        assert!(body.contains("403"));
        assert!(body.contains("--ink: #0b3553"));
        assert!(body.contains("prefers-color-scheme: dark"));
        assert!(body.contains("--ink: #f3e5c6"));
        assert!(body.contains(".brand svg"));
        assert!(body.contains(".brand .bookclerk-wordmark"));
        assert!(body.contains("width: 100%"));
    }

    #[tokio::test]
    async fn document_error_uses_custom_message() {
        async fn gone() -> Response {
            document_error(StatusCode::GONE, "This invite link has already been used.")
        }
        let app = Router::new().route("/invite", get(gone));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/invite")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::GONE);
        let body = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
            .unwrap();
        assert!(body.contains("already been used"));
        assert!(body.contains("410"));
    }

    #[tokio::test]
    async fn existing_json_body_is_not_rewritten() {
        async fn already_json() -> impl IntoResponse {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "custom", "keep": true })),
            )
        }
        let app = Router::new()
            .route("/api/custom", get(already_json))
            .layer(axum::middleware::from_fn(brand_error_responses));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/custom")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["keep"], true);
        assert_eq!(json["error"], "custom");
    }
}
