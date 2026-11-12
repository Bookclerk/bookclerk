//! Cross-origin and browser-hardening headers for every response the daemon
//! sends.
//!
//! The daemon's web app, API, covers and downloads belong to one origin and
//! carry per-user data behind a session cookie or bearer token, so every
//! response is closed to other origins, and says so with explicit headers
//! rather than leaving each browser to assume a policy from a missing one:
//!
//! | Header | Value | Effect |
//! | --- | --- | --- |
//! | `Cross-Origin-Resource-Policy` | `same-origin` | No other site can load a response as an image, script, style or media. |
//! | `Cross-Origin-Opener-Policy` | `same-origin` | A page that opens the app, or that it opens, gets no window handle. |
//! | `Cross-Origin-Embedder-Policy` | `require-corp` | The app's pages load only resources that opt in; they load only their own. |
//! | `X-Frame-Options` | `DENY` | No framing, beside the CSP's `frame-ancestors 'none'`. |
//! | `X-Content-Type-Options` | `nosniff` | Browsers keep to the declared `Content-Type`. |
//! | `Referrer-Policy` | `strict-origin-when-cross-origin`, unless the handler chose a stricter one | Other sites see at most the origin, never a path. |
//! | `Origin-Agent-Cluster` | `?1` | The origin gets its own agent cluster. |
//! | `X-Permitted-Cross-Domain-Policies` | `none` | No legacy cross-domain policy files. |
//! | `X-XSS-Protection` | `0` | Turns off old browsers' XSS filter, which could leak page content; the CSP is what stops injected script. |
//!
//! No response carries an `Access-Control-*` grant: [`apply`] removes any a
//! handler set, so scripts on other origins can never read an answer.
//! Clients that are not browsers (the CLI, other nodes, integrations) do not
//! apply these headers and are unaffected.

use axum::extract::Request;
use axum::http::header::{self, HeaderName};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// Headers [`apply`] sets on every response, replacing any value a handler
/// chose.
pub(crate) const CLOSED_HEADERS: &[(&str, &str)] = &[
    ("cross-origin-resource-policy", "same-origin"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-embedder-policy", "require-corp"),
    ("x-frame-options", "DENY"),
    ("x-content-type-options", "nosniff"),
    ("origin-agent-cluster", "?1"),
    ("x-permitted-cross-domain-policies", "none"),
    ("x-xss-protection", "0"),
];

/// `Referrer-Policy` for responses whose handler chose none. A handler may
/// set a stricter one, such as the tray hand-off's `no-referrer`, which
/// [`apply`] keeps.
pub(crate) const DEFAULT_REFERRER_POLICY: &str = "strict-origin-when-cross-origin";

/// CORS grants no response may carry.
const CORS_GRANTS: &[HeaderName] = &[
    header::ACCESS_CONTROL_ALLOW_ORIGIN,
    header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
    header::ACCESS_CONTROL_ALLOW_METHODS,
    header::ACCESS_CONTROL_ALLOW_HEADERS,
    header::ACCESS_CONTROL_EXPOSE_HEADERS,
    header::ACCESS_CONTROL_MAX_AGE,
];

/// Gives every response [`CLOSED_HEADERS`], a [`DEFAULT_REFERRER_POLICY`]
/// unless its handler set one, and strips any CORS grant. A WebSocket
/// upgrade (`101`) passes through untouched.
///
/// # Arguments
///
/// * `req` - The incoming request.
/// * `next` - The rest of the stack.
///
/// # Returns
///
/// The inner response with the closed headers set.
pub(crate) async fn apply(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    if res.status() == StatusCode::SWITCHING_PROTOCOLS {
        return res;
    }
    let headers = res.headers_mut();
    for (name, value) in CLOSED_HEADERS {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    if !headers.contains_key(header::REFERRER_POLICY) {
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static(DEFAULT_REFERRER_POLICY),
        );
    }
    for name in CORS_GRANTS {
        headers.remove(name);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    /// A router whose one route answers with a CORS grant and a permissive
    /// CORP, wrapped in [`apply`].
    fn app() -> Router {
        Router::new()
            .route(
                "/",
                get(|| async {
                    (
                        [
                            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                            (header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true"),
                            (
                                HeaderName::from_static("cross-origin-resource-policy"),
                                "cross-origin",
                            ),
                            (header::REFERRER_POLICY, "no-referrer"),
                        ],
                        "ok",
                    )
                }),
            )
            .layer(axum::middleware::from_fn(apply))
    }

    #[tokio::test]
    async fn closes_a_response_and_strips_cors_grants() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::ORIGIN, "https://elsewhere.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        for (name, value) in CLOSED_HEADERS {
            assert_eq!(res.headers().get(*name).unwrap(), value, "{name}");
            assert_eq!(res.headers().get_all(*name).iter().count(), 1, "{name}");
        }
        for name in CORS_GRANTS {
            assert!(res.headers().get(name).is_none(), "{name}");
        }
        assert_eq!(
            res.headers().get(header::REFERRER_POLICY).unwrap(),
            "no-referrer",
            "a handler's own policy stays"
        );
    }

    #[tokio::test]
    async fn sets_the_default_referrer_policy_when_none_was_chosen() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(apply));
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            res.headers().get(header::REFERRER_POLICY).unwrap(),
            DEFAULT_REFERRER_POLICY
        );
    }

    #[test]
    fn every_header_name_is_valid_lowercase() {
        for (name, value) in CLOSED_HEADERS {
            assert_eq!(*name, name.to_ascii_lowercase());
            assert!(HeaderName::from_bytes(name.as_bytes()).is_ok(), "{name}");
            assert!(HeaderValue::from_str(value).is_ok(), "{value}");
        }
    }
}
