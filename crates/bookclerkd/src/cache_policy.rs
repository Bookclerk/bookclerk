//! `Cache-Control` for every response the daemon sends.
//!
//! Each response falls in one class, and the [`apply`] middleware gives it
//! that class's header unless the handler chose one itself:
//!
//! | Class | `Cache-Control` |
//! | --- | --- |
//! | HTML documents | `no-cache` (`/invite`: `no-store, no-cache, must-revalidate`) |
//! | Hashed web app files under `/assets/` | [`IMMUTABLE_ASSET`] |
//! | Other web app files (icons, manifest, fonts) | [`REVALIDATE`] |
//! | Everything else (API JSON, errors including HTML error pages, legacy routes) | [`NO_STORE`] |
//!
//! Handlers with their own policy set the header themselves: cover and
//! image routes ([`crate::covers`]), plugin logos and profile pictures. Image
//! handlers also answer `If-None-Match` through [`image_response`], so a
//! browser revalidating its copy gets `304 Not Modified` without the body.
//!
//! The middleware runs outside the branded-error layer, so an error page the
//! daemon rewrites still gets [`NO_STORE`].

use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bookclerk_cover::http_cache::{if_none_match, EntityTag};

/// `Cache-Control` for web app files whose name carries a hash of their
/// contents (Vite's `/assets/` output): a change is a new URL, so browsers
/// and proxies may keep these for a year.
pub(crate) const IMMUTABLE_ASSET: &str = "public, max-age=31536000, immutable";

/// `Cache-Control` for web app files under a fixed name (icons, the web
/// manifest, fonts, `theme-init.js`): kept, but checked with the server
/// (`Last-Modified`) before each use, so a rebuilt `ui/dist` shows at once.
pub(crate) const REVALIDATE: &str = "no-cache";

/// `Cache-Control` for everything without a policy of its own: API JSON is
/// per-user and changes often, so nothing may store it.
pub(crate) const NO_STORE: &str = "no-store";

/// `Cache-Control` for the HTML of `/invite`, whose URL can turn from the
/// live app into a `410` after the invite is redeemed.
const INVITE_DOCUMENT: &str = "no-store, no-cache, must-revalidate";

/// Path prefix of the web app's hashed build output.
const HASHED_ASSET_PREFIX: &str = "/assets/";

/// Marks a response as a file served from the web app's `ui/dist`, so
/// [`apply`] can tell it from an API answer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StaticFile;

/// Tags a `ui/dist` file response with [`StaticFile`]; layer it on the
/// static file service.
///
/// # Arguments
///
/// * `response` - The static file service's response.
///
/// # Returns
///
/// The same response, tagged.
pub(crate) async fn mark_static(mut response: Response) -> Response {
    response.extensions_mut().insert(StaticFile);
    response
}

/// The response class [`apply`] picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// An HTML document; `invite` says whether it is `/invite`.
    Document {
        /// Whether the request was for `/invite`.
        invite: bool,
    },
    /// The handler set its own `Cache-Control`.
    Chosen,
    /// A successful hashed web app file.
    HashedAsset,
    /// A successful web app file under a fixed name.
    StaticFile,
    /// Anything else.
    Uncached,
}

/// Picks the class of a response.
///
/// An error status (`4xx` or `5xx`) is [`Class::Uncached`] even when it is
/// an HTML page, such as the branded `404` a browser gets, so no cache keeps
/// it; only `/invite` keeps its own policy, since its `410` is the page.
///
/// # Arguments
///
/// * `path` - The request path.
/// * `response` - The response about to be sent.
fn classify(path: &str, response: &Response) -> Class {
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    let invite = path == "/invite";
    let status = response.status();
    let error = status.is_client_error() || status.is_server_error();
    if is_html && (invite || !error) {
        return Class::Document { invite };
    }
    if error {
        return Class::Uncached;
    }
    if response.headers().contains_key(header::CACHE_CONTROL) {
        return Class::Chosen;
    }
    let served_file = response.extensions().get::<StaticFile>().is_some()
        && (status.is_success() || status == StatusCode::NOT_MODIFIED);
    if !served_file {
        return Class::Uncached;
    }
    if path.starts_with(HASHED_ASSET_PREFIX) {
        Class::HashedAsset
    } else {
        Class::StaticFile
    }
}

/// Sets `Cache-Control` on every response by its class (see the module
/// docs).
///
/// HTML documents always get `no-cache` so a rebuilt `ui/dist/index.html`
/// is used on the next navigation; `/invite` is `no-store` (with
/// `Pragma: no-cache`) because the same URL can change from the live app to
/// a `410` after redeem.
pub(crate) async fn apply(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let mut res = next.run(req).await;
    let value = match classify(&path, &res) {
        Class::Document { invite: true } => {
            res.headers_mut()
                .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
            INVITE_DOCUMENT
        }
        Class::Document { invite: false } => REVALIDATE,
        Class::Chosen => return res,
        Class::HashedAsset => IMMUTABLE_ASSET,
        Class::StaticFile => REVALIDATE,
        Class::Uncached => NO_STORE,
    };
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    res
}

/// A `200` carrying `body` with a strong `ETag`, or `304 Not Modified` when
/// the request's `If-None-Match` already names that tag.
///
/// # Arguments
///
/// * `request` - The request's headers.
/// * `body` - The image bytes.
/// * `content_type` - `Content-Type` of the body.
/// * `cache_control` - `Cache-Control` for both answers.
///
/// # Returns
///
/// The `200` or `304` response.
pub(crate) fn image_response(
    request: &HeaderMap,
    body: Vec<u8>,
    content_type: &'static str,
    cache_control: &'static str,
) -> Response {
    let tag = EntityTag::of(&body);
    let etag = HeaderValue::from_str(&tag.header_value())
        .unwrap_or_else(|_| HeaderValue::from_static("\"\""));
    let current = request
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .any(|value| if_none_match(value.as_bytes(), &tag));
    if current {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(cache_control),
                ),
            ],
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(cache_control),
            ),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    /// A router with one route per class, behind [`apply`].
    fn app() -> Router {
        let static_files = Router::new()
            .route("/assets/app-1a2b3c.js", get(|| async { "js" }))
            .route("/favicon.ico", get(|| async { "ico" }))
            .route(
                "/missing.png",
                get(|| async { StatusCode::NOT_FOUND.into_response() }),
            )
            .layer(axum::middleware::map_response(mark_static));
        Router::new()
            .route("/api/status", get(|| async { axum::Json("ok") }))
            .route("/assets/not-a-file", get(|| async { "api-ish" }))
            .route("/", get(|| async { axum::response::Html("<p>app</p>") }))
            .route(
                "/invite",
                get(|| async { axum::response::Html("<p>invite</p>") }),
            )
            .route(
                "/gone",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        axum::response::Html("<p>branded 404</p>"),
                    )
                }),
            )
            .route(
                "/api/own",
                get(|| async { ([(header::CACHE_CONTROL, "private, max-age=60")], "own") }),
            )
            .merge(static_files)
            .layer(axum::middleware::from_fn(apply))
    }

    async fn headers_of(path: &str) -> (StatusCode, HeaderMap) {
        let res = app()
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("response");
        (res.status(), res.headers().clone())
    }

    async fn cache_control(path: &str) -> String {
        let (_, headers) = headers_of(path).await;
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    #[tokio::test]
    async fn every_class_gets_its_header() {
        assert_eq!(cache_control("/api/status").await, NO_STORE);
        assert_eq!(cache_control("/").await, REVALIDATE);
        assert_eq!(cache_control("/invite").await, INVITE_DOCUMENT);
        assert_eq!(
            cache_control("/assets/app-1a2b3c.js").await,
            IMMUTABLE_ASSET
        );
        assert_eq!(cache_control("/favicon.ico").await, REVALIDATE);
        assert_eq!(cache_control("/api/own").await, "private, max-age=60");
    }

    #[tokio::test]
    async fn invite_also_sends_pragma() {
        let (_, headers) = headers_of("/invite").await;
        assert_eq!(headers[header::PRAGMA], "no-cache");
    }

    #[tokio::test]
    async fn only_files_from_ui_dist_are_cached() {
        // A route under `/assets/` that is not a static file, a missing
        // static file and an unknown path are not stored.
        assert_eq!(cache_control("/assets/not-a-file").await, NO_STORE);
        assert_eq!(cache_control("/missing.png").await, NO_STORE);
        assert_eq!(cache_control("/nowhere").await, NO_STORE);
        // An HTML error page is not a document anyone may keep.
        assert_eq!(cache_control("/gone").await, NO_STORE);
    }

    #[test]
    fn an_image_carries_its_tag_and_answers_304_when_current() {
        let body = b"jpeg".to_vec();
        let tag = EntityTag::of(&body).header_value();

        let fresh = image_response(&HeaderMap::new(), body.clone(), "image/jpeg", "private");
        assert_eq!(fresh.status(), StatusCode::OK);
        assert_eq!(fresh.headers()[header::ETAG], tag.as_str());
        assert_eq!(fresh.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(fresh.headers()[header::CACHE_CONTROL], "private");

        let mut stale = HeaderMap::new();
        stale.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"old\""));
        let changed = image_response(&stale, body.clone(), "image/jpeg", "private");
        assert_eq!(changed.status(), StatusCode::OK);

        let mut current = HeaderMap::new();
        current.append(header::IF_NONE_MATCH, HeaderValue::from_static("\"old\""));
        current.append(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&format!("W/{tag}")).expect("header"),
        );
        let same = image_response(&current, body, "image/jpeg", "private");
        assert_eq!(same.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(same.headers()[header::ETAG], tag.as_str());
        assert_eq!(same.headers()[header::CACHE_CONTROL], "private");
        assert!(!same.headers().contains_key(header::CONTENT_TYPE));
    }
}
