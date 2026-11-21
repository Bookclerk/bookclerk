//! What the desktop window's custom scheme lets a page reach.
//!
//! The desktop window runs the host in-process and hands every request its
//! webview makes on the app scheme to the router, adding its own credential
//! itself (`bookclerkd::embedded`). That makes the scheme handler an
//! authorization boundary over bytes the host did not produce: the method,
//! headers and path a page sent, and the URLs the window is asked to open.
//! This crate is that boundary's policy, kept free of I/O so the
//! `window_policy` fuzz target and the unit tests drive the same code the
//! window does:
//!
//! - [`refusal`]: a request from a page that is not the app's (by `Origin`,
//!   or for a write by `Referer`) is refused, and so are the node API and
//!   the live socket, before routing.
//! - [`rewrite_request_headers`]: the headers the router sees. The page's
//!   credentials, origins and forwarding headers are dropped; `Host`,
//!   `Origin` ([`EMBEDDED_ORIGIN`]) and the window's own [`Credential`] are
//!   set: the patron session from this OS user's profile, or the operator
//!   token before the library has an Owner.
//! - [`classify_navigation`]: the window stays on the app's pages, except
//!   for a library's sign-in while one started from the window is open: a
//!   join (this install becomes a branch) or a library card (this install
//!   signs in as a clerk).
//! - [`sign_in_ttl`]: how long that sign-in stays open, from the start
//!   route's answer.

use std::time::Duration;

use http::{header, HeaderMap, HeaderName, HeaderValue, Method};

/// The `Host` every embedded request is routed under.
pub const EMBEDDED_HOST: &str = "localhost";

/// The origin the router sees every embedded request come from: loopback,
/// so a sign-in's return address is one the library accepts.
pub const EMBEDDED_ORIGIN: &str = "http://localhost";

/// Origins of the window's own pages: the custom scheme, and the form
/// WebView2 (Windows) gives it.
pub const APP_ORIGINS: [&str; 2] = ["bookclerk://localhost", "http://bookclerk.localhost"];

/// The route that starts a join; its answer opens the sign-in window.
pub const JOIN_START_PATH: &str = "/api/config/join/start";

/// The route that starts a library card sign-in; its answer opens the
/// sign-in window.
pub const CLERK_START_PATH: &str = "/api/config/clerk/start";

/// The routes whose successful `POST` opens a library's sign-in in the
/// window.
pub const SIGN_IN_START_PATHS: [&str; 2] = [JOIN_START_PATH, CLERK_START_PATH];

/// The paths a library sends the window back to after its sign-in, on a
/// loopback `http` address: the join's and the library card's callback.
pub const SIGN_IN_CALLBACK_PATHS: [&str; 2] = [
    bookclerk_node_proto::JOIN_CALLBACK_PATH,
    bookclerk_node_proto::CLERK_CALLBACK_PATH,
];

/// The node API's path prefix: other nodes reach it over TLS, never the
/// window.
pub const NODE_API_PATH: &str = "/api/node";

/// The live WebSocket's path, which a custom scheme cannot carry. The same
/// value as `bookclerkd`'s `live::LIVE_PATH` (a test there checks).
pub const LIVE_PATH: &str = "/api/live";

/// Longest sign-in a start route may open, in seconds, whatever its answer
/// says.
pub const MAX_SIGN_IN_SECS: u64 = 3600;

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

/// Why a request was refused before it reached the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A node API path: other nodes reach it over TLS, never the window.
    NodeApi,
    /// The live WebSocket, which a custom scheme cannot carry.
    LiveSocket,
    /// A request a page from another origin sent.
    ForeignOrigin,
}

/// Where the window may go, as [`classify_navigation`] decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Navigation {
    /// One of the app's own pages.
    App,
    /// A library's sign-in page, while a sign-in is open.
    SignIn,
    /// The library sending the window back after sign-in. The window opens
    /// this path and query on the app scheme instead; it always starts with
    /// one of [`SIGN_IN_CALLBACK_PATHS`].
    SignInReturn(String),
    /// Anywhere else: refused.
    Refuse,
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
/// `Some` with the reason for a refused request, `None` otherwise. An
/// origin refusal wins over a path one.
#[must_use]
pub fn refusal(method: &Method, headers: &HeaderMap, path: &str) -> Option<Refusal> {
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
fn from_another_origin(method: &Method, headers: &HeaderMap) -> bool {
    if let Some(origin) = headers.get(header::ORIGIN) {
        return !origin
            .to_str()
            .is_ok_and(|origin| APP_ORIGINS.contains(&origin));
    }
    let writes = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if !writes {
        return false;
    }
    headers.get(header::REFERER).is_some_and(|referer| {
        !referer
            .to_str()
            .is_ok_and(|referer| url::Url::parse(referer).is_ok_and(|url| is_app_url(&url)))
    })
}

/// Refuses paths the window must not reach.
///
/// # Arguments
///
/// * `path` - The request path, undecoded (as the router matches it).
///
/// # Returns
///
/// `Some` with the reason for a refused path, `None` otherwise. A prefix
/// matches whole segments only (`/api/nodes` is not the node API).
fn refused_path(path: &str) -> Option<Refusal> {
    let under = |prefix: &str| {
        path == prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    if under(NODE_API_PATH) {
        Some(Refusal::NodeApi)
    } else if under(LIVE_PATH) {
        Some(Refusal::LiveSocket)
    } else {
        None
    }
}

/// Whether `url` is one of the app's own pages: on one of [`APP_ORIGINS`],
/// with no other port.
fn is_app_url(url: &url::Url) -> bool {
    let host = match url.scheme() {
        "bookclerk" => "localhost",
        "http" => "bookclerk.localhost",
        _ => return false,
    };
    url.host_str() == Some(host) && url.port().is_none()
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
/// * `sign_in_open` - Whether a library's sign-in is under way.
///
/// # Returns
///
/// The app's own pages always; the library's return to one of
/// [`SIGN_IN_CALLBACK_PATHS`] on a loopback `http` address (any host and
/// port) as a [`Navigation::SignInReturn`] while a sign-in is open; other
/// `https` pages, or loopback `http` ones, only while a sign-in is open.
#[must_use]
pub fn classify_navigation(raw: &str, sign_in_open: bool) -> Navigation {
    let Ok(url) = url::Url::parse(raw) else {
        return Navigation::Refuse;
    };
    if is_app_url(&url) {
        return Navigation::App;
    }
    if !sign_in_open {
        return Navigation::Refuse;
    }
    if is_loopback_http(&url) && SIGN_IN_CALLBACK_PATHS.contains(&url.path()) {
        let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        return Navigation::SignInReturn(format!("{}{query}", url.path()));
    }
    if url.scheme() == "https" || is_loopback_http(&url) {
        return Navigation::SignIn;
    }
    Navigation::Refuse
}

/// The label of the desktop app's one window, the only webview the `live`
/// command answers.
pub const MAIN_WINDOW_LABEL: &str = "main";

/// Why the desktop window's `live` command refused a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveRefusal {
    /// A webview other than the main window called it.
    OtherWebview,
    /// The calling page is not one of the app's own pages, such as a
    /// library's sign-in page.
    ForeignPage,
}

/// Decides whether a page may start the window's live updates, which carry
/// the operator's feed.
///
/// # Arguments
///
/// * `webview_label` - The calling webview's label.
/// * `page_url` - The URL of the page that called, as the webview reports
///   it.
///
/// # Returns
///
/// `None` when the main window's app page called; otherwise why it was
/// refused. A wrong webview wins over a foreign page.
#[must_use]
pub fn live_refusal(webview_label: &str, page_url: &str) -> Option<LiveRefusal> {
    if webview_label != MAIN_WINDOW_LABEL {
        return Some(LiveRefusal::OtherWebview);
    }
    match url::Url::parse(page_url) {
        Ok(url) if is_app_url(&url) => None,
        _ => Some(LiveRefusal::ForeignPage),
    }
}

/// How long the window may show the library's sign-in, from a successful
/// start route's JSON answer (a join's or a library card's).
///
/// # Arguments
///
/// * `body` - The answer's body.
///
/// # Returns
///
/// `expires_in_secs`, at most [`MAX_SIGN_IN_SECS`], when the body is a
/// JSON object with a string `authorize_url` and an unsigned integer
/// `expires_in_secs`; `None` otherwise.
#[must_use]
pub fn sign_in_ttl(body: &[u8]) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value.get("authorize_url")?.as_str()?;
    let secs = value
        .get("expires_in_secs")?
        .as_u64()?
        .min(MAX_SIGN_IN_SECS);
    Some(Duration::from_secs(secs))
}

/// The credential the window presents for a request: the page never holds
/// one, so the embedded host chooses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential<'a> {
    /// No credential: operator auth is off, or nobody is signed in yet and
    /// the page should show the sign-in screen.
    None,
    /// The operator's bearer token: the library has no Owner yet (first
    /// run, or a computer that is only a clerk of another library).
    Operator(&'a str),
    /// The `Cookie` header the window's own session jar holds: the patron
    /// this OS user's profile names, and an elevated operator session when
    /// that patron elevated.
    Cookies(&'a str),
}

/// The headers the router sees for an embedded request: the page's own,
/// less credentials, origins and forwarding headers, plus `Host`, `Origin`
/// ([`EMBEDDED_ORIGIN`]) and the window's own credential.
///
/// # Arguments
///
/// * `page` - The headers the webview sent.
/// * `credential` - What the window presents: nothing, the operator's
///   bearer token, or its session jar's `Cookie` header.
///
/// # Returns
///
/// The rewritten headers. A token or cookie text that is not a valid header
/// value is left out, so the request goes unauthenticated (fail closed);
/// the caller can tell from the missing `Authorization` or `Cookie`.
#[must_use]
pub fn rewrite_request_headers(page: &HeaderMap, credential: Credential<'_>) -> HeaderMap {
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
    let (name, text) = match credential {
        Credential::None => return headers,
        Credential::Operator(token) => (header::AUTHORIZATION, format!("Bearer {token}")),
        Credential::Cookies(cookies) => (header::COOKIE, cookies.to_owned()),
    };
    if let Ok(mut value) = HeaderValue::from_str(&text) {
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Headers from `pairs`.
    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn requests_from_the_app_pass_and_others_are_refused() {
        let app = headers(&[("origin", "bookclerk://localhost")]);
        assert_eq!(refusal(&Method::POST, &app, "/api/jobs"), None);
        let windows = headers(&[("origin", "http://bookclerk.localhost")]);
        assert_eq!(refusal(&Method::POST, &windows, "/api/jobs"), None);
        for origin in ["https://library.example", "null", "bookclerk://localhost:1"] {
            let foreign = headers(&[("origin", origin)]);
            assert_eq!(
                refusal(&Method::GET, &foreign, "/api/jobs"),
                Some(Refusal::ForeignOrigin),
                "{origin}"
            );
        }
        let referred = headers(&[("referer", "https://library.example/join/approve")]);
        assert_eq!(refusal(&Method::GET, &referred, "/api/jobs"), None);
        assert_eq!(
            refusal(&Method::POST, &referred, "/api/jobs"),
            Some(Refusal::ForeignOrigin)
        );
        let app_referer = headers(&[("referer", "bookclerk://localhost/settings")]);
        assert_eq!(refusal(&Method::DELETE, &app_referer, "/api/jobs"), None);
        assert_eq!(refusal(&Method::POST, &HeaderMap::new(), "/api/jobs"), None);
    }

    #[test]
    fn node_api_and_live_socket_match_whole_segments() {
        let none = HeaderMap::new();
        assert_eq!(
            refusal(&Method::GET, &none, "/api/node"),
            Some(Refusal::NodeApi)
        );
        assert_eq!(
            refusal(&Method::GET, &none, "/api/node/v1/enroll"),
            Some(Refusal::NodeApi)
        );
        assert_eq!(
            refusal(&Method::GET, &none, "/api/live"),
            Some(Refusal::LiveSocket)
        );
        assert_eq!(refusal(&Method::GET, &none, "/api/nodes"), None);
        assert_eq!(refusal(&Method::GET, &none, "/api/lively"), None);
        let foreign = headers(&[("origin", "https://evil.example")]);
        assert_eq!(
            refusal(&Method::GET, &foreign, "/api/node"),
            Some(Refusal::ForeignOrigin)
        );
    }

    #[test]
    fn the_rewrite_drops_page_credentials_and_adds_the_token() {
        let page = headers(&[
            ("cookie", "bookclerk_operator_session=stolen"),
            ("authorization", "Bearer page"),
            ("proxy-authorization", "Basic x"),
            ("origin", "bookclerk://localhost"),
            ("referer", "bookclerk://localhost/"),
            ("forwarded", "for=203.0.113.9"),
            ("x-forwarded-for", "203.0.113.9"),
            ("x-real-ip", "203.0.113.9"),
            ("host", "evil.example"),
            ("accept", "application/json"),
        ]);
        let seen = rewrite_request_headers(&page, Credential::Operator("op-token"));
        let mut lines: Vec<String> = seen
            .iter()
            .map(|(name, value)| format!("{name}={}", value.to_str().unwrap()))
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            [
                "accept=application/json",
                "authorization=Bearer op-token",
                "host=localhost",
                "origin=http://localhost",
            ]
        );
        assert!(seen.get(header::AUTHORIZATION).unwrap().is_sensitive());
        let off = rewrite_request_headers(&page, Credential::None);
        assert!(off.get(header::AUTHORIZATION).is_none());
        assert!(off.get(header::COOKIE).is_none());
        let bad = rewrite_request_headers(&page, Credential::Operator("line\nbreak"));
        assert!(bad.get(header::AUTHORIZATION).is_none());
    }

    #[test]
    fn the_rewrite_sends_the_jar_cookies_instead_of_the_page_cookies() {
        let page = headers(&[
            ("cookie", "bookclerk_portal_session=stolen"),
            ("authorization", "Bearer page"),
        ]);
        let jar = "bookclerk_portal_session=mine; bookclerk_operator_session=elevated";
        let seen = rewrite_request_headers(&page, Credential::Cookies(jar));
        let cookies: Vec<&str> = seen
            .get_all(header::COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(cookies, [jar]);
        assert!(seen.get(header::COOKIE).unwrap().is_sensitive());
        assert!(seen.get(header::AUTHORIZATION).is_none());
        let bad = rewrite_request_headers(&page, Credential::Cookies("a=b\r\nx: y"));
        assert!(bad.get(header::COOKIE).is_none());
    }

    #[test]
    fn the_window_stays_on_the_app_unless_a_sign_in_is_open() {
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
            classify_navigation("http://bookclerk.localhost:8080/", false),
            Navigation::Refuse
        );
        assert_eq!(
            classify_navigation("https://library.example/join/approve", false),
            Navigation::Refuse
        );
        assert_eq!(
            classify_navigation("https://library.example/join/approve", true),
            Navigation::SignIn
        );
        assert_eq!(
            classify_navigation("http://localhost/join/callback?code=c&state=s", true),
            Navigation::SignInReturn("/join/callback?code=c&state=s".into())
        );
        assert_eq!(
            classify_navigation("http://127.0.0.1:8787/join/callback", true),
            Navigation::SignInReturn("/join/callback".into())
        );
        assert_eq!(
            classify_navigation("http://[::1]/", true),
            Navigation::SignIn
        );
        assert_eq!(
            classify_navigation("http://evil.example/join/callback", true),
            Navigation::Refuse
        );
        assert_eq!(
            classify_navigation("http://localhost:8787/clerk/callback?code=c&state=s", true),
            Navigation::SignInReturn("/clerk/callback?code=c&state=s".into())
        );
        assert_eq!(
            classify_navigation("http://localhost/clerk/callback?code=c", false),
            Navigation::Refuse
        );
        assert_eq!(
            classify_navigation("http://localhost/clerk/callbacks", true),
            Navigation::SignIn
        );
    }

    #[test]
    fn sign_in_ttl_comes_from_a_start_answer_only() {
        let started =
            br#"{"authorize_url":"https://l.example/join/approve","expires_in_secs":600}"#;
        assert_eq!(sign_in_ttl(started), Some(Duration::from_secs(600)));
        let long = br#"{"authorize_url":"x","expires_in_secs":999999}"#;
        assert_eq!(sign_in_ttl(long), Some(Duration::from_secs(3600)));
        assert_eq!(sign_in_ttl(br#"{"expires_in_secs":600}"#), None);
        assert_eq!(sign_in_ttl(b"not json"), None);
    }

    #[test]
    fn live_updates_answer_only_the_main_window_on_an_app_page() {
        assert_eq!(live_refusal("main", "bookclerk://localhost/library"), None);
        assert_eq!(live_refusal("main", "http://bookclerk.localhost/"), None);
        assert_eq!(
            live_refusal("join", "bookclerk://localhost/"),
            Some(LiveRefusal::OtherWebview)
        );
        assert_eq!(
            live_refusal("Main", "https://library.example/login"),
            Some(LiveRefusal::OtherWebview)
        );
        for page in [
            "https://library.example/login",
            "http://localhost:8787/join/callback",
            "http://bookclerk.localhost:8080/",
            "not a url",
            "",
        ] {
            assert_eq!(
                live_refusal("main", page),
                Some(LiveRefusal::ForeignPage),
                "{page}"
            );
        }
    }
}
