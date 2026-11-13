//! Shared oracle for the `window_policy` fuzz target and the
//! `bookclerk-window-policy` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the desktop window's custom scheme: the method, headers
//! and path a page sends, which decide whether the request reaches the
//! router carrying the operator token, and the URLs the window is asked to
//! open. The invariants, restated here from the bytes rather than read from
//! the policy:
//!
//! - **Refusal.** A request is refused as foreign exactly when it carries an
//!   `Origin` whose bytes are not one of the two app origins, or carries no
//!   `Origin`, is not `GET`/`HEAD`/`OPTIONS`, and has a `Referer` that does
//!   not parse as a URL on an app origin. Otherwise it is refused exactly
//!   when its path's first two segments are `api` and `node` (or `api` and
//!   `live`) and nothing else follows them but a `/`.
//! - **Rewrite.** The headers the router sees hold no `Cookie`,
//!   `Proxy-Authorization`, `Forwarded`, `Referer`, `X-Forwarded-*` or
//!   `X-Real-IP`; exactly one `Host: localhost` and one
//!   `Origin: http://localhost`; `Authorization` only when a token was given,
//!   and then exactly `Bearer <token>` (or none, when that is not a valid
//!   header value); and every other header the page sent, unchanged.
//! - **Navigation.** An app URL is always allowed. With no join open nothing
//!   else is. With one open, a loopback `http` URL on the join callback path
//!   returns that path and query (always starting with the callback path, so
//!   the window stays on its own origin), other `https` and loopback `http`
//!   URLs are a sign-in, and the rest is refused.
//! - **Live updates.** The `live` command answers exactly when the calling
//!   webview's label is byte for byte `main` and the page URL parses onto
//!   an app origin; a wrong label is reported before a foreign page.
//! - **Sign-in time.** A join start answer opens the sign-in exactly when it
//!   is a JSON object with a string `authorize_url` and an unsigned
//!   `expires_in_secs`, for that many seconds up to an hour.
//!
//! Input layout: the first byte holds flags (bit 0: a join is open, bit 1:
//! a token is given, bit 2: the `live` caller is labelled `main`, otherwise
//! its label is the method's text); the rest splits on `0x00` into the
//! method, the path, the navigation URL (also the `live` caller's page),
//! the join start body, the token, and then header lines `name: value`.

use bookclerk_window_policy::{
    classify_navigation, join_sign_in_ttl, live_refusal, refusal, rewrite_request_headers,
    LiveRefusal, Navigation, Refusal,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 4096;

/// The two app origins, written out rather than read from the policy.
const APP: [&[u8]; 2] = [b"bookclerk://localhost", b"http://bookclerk.localhost"];

/// The join callback path, written out.
const CALLBACK: &str = "/join/callback";

/// One decoded input.
struct Case<'a> {
    /// Whether a join is open.
    join_open: bool,
    /// The request method.
    method: Method,
    /// The request path.
    path: &'a str,
    /// The URL the window is asked to open.
    navigation: &'a str,
    /// A join start answer's body.
    join_body: &'a [u8],
    /// The operator token, when one is given.
    token: Option<&'a str>,
    /// The headers the page sent.
    headers: HeaderMap,
    /// Whether the `live` caller is labelled `main` rather than with the
    /// method's text.
    main_label: bool,
}

/// Splits `data` into a case, or `None` when it is too short.
fn decode(data: &[u8]) -> Option<Case<'_>> {
    let (&flags, rest) = data.split_first()?;
    let mut parts = rest.split(|&b| b == 0);
    let method = Method::from_bytes(parts.next().unwrap_or_default()).unwrap_or(Method::GET);
    let path = std::str::from_utf8(parts.next().unwrap_or_default()).unwrap_or("/");
    let navigation = std::str::from_utf8(parts.next().unwrap_or_default()).unwrap_or("");
    let join_body = parts.next().unwrap_or_default();
    let token = std::str::from_utf8(parts.next().unwrap_or_default()).unwrap_or("");
    let mut headers = HeaderMap::new();
    for line in parts {
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let (name, value) = (&line[..colon], &line[colon + 1..]);
        let value = value.strip_prefix(b" ").unwrap_or(value);
        if let (Ok(name), Ok(value)) =
            (HeaderName::from_bytes(name), HeaderValue::from_bytes(value))
        {
            headers.append(name, value);
        }
    }
    Some(Case {
        join_open: flags & 1 != 0,
        method,
        path,
        navigation,
        join_body,
        token: (flags & 2 != 0).then_some(token),
        headers,
        main_label: flags & 4 != 0,
    })
}

/// Whether `text` parses as a URL on one of the app origins.
fn on_app_origin(text: &str) -> bool {
    let Ok(url) = url::Url::parse(text) else {
        return false;
    };
    // Compare the serialized origin with the two app origins.
    let origin = format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or(""),
        url.port().map(|p| format!(":{p}")).unwrap_or_default()
    );
    APP.contains(&origin.as_bytes())
}

/// Whether a `Referer` value names an app page. Header text is visible
/// ASCII (or tab); a value with any other byte is not read as a URL.
fn referer_on_app_origin(raw: &[u8]) -> bool {
    raw.iter().all(|&b| b == b'\t' || (0x20..0x7f).contains(&b))
        && std::str::from_utf8(raw).is_ok_and(on_app_origin)
}

/// The oracle's own verdict on a request.
fn expected_refusal(case: &Case<'_>) -> Option<Refusal> {
    let origin = case.headers.get("origin").map(HeaderValue::as_bytes);
    let foreign = match origin {
        Some(origin) => !APP.contains(&origin),
        None => {
            let safe = case.method == Method::GET
                || case.method == Method::HEAD
                || case.method == Method::OPTIONS;
            !safe
                && case
                    .headers
                    .get("referer")
                    .is_some_and(|referer| !referer_on_app_origin(referer.as_bytes()))
        }
    };
    if foreign {
        return Some(Refusal::ForeignOrigin);
    }
    let segments: Vec<&str> = case.path.split('/').collect();
    let refused = |name: &str| {
        segments.first() == Some(&"")
            && segments.get(1) == Some(&"api")
            && segments.get(2) == Some(&name)
    };
    if refused("node") {
        Some(Refusal::NodeApi)
    } else if refused("live") {
        Some(Refusal::LiveSocket)
    } else {
        None
    }
}

/// Whether `url` is a loopback `http` URL, restated.
fn loopback_http(url: &url::Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }
    match url.host_str() {
        Some(host) => {
            host.eq_ignore_ascii_case("localhost")
                || host == "[::1]"
                || host
                    .parse::<std::net::Ipv4Addr>()
                    .is_ok_and(|ip| ip.octets()[0] == 127)
        }
        None => false,
    }
}

/// Checks one input against the policy.
///
/// # Returns
///
/// Whether the request reached the router (was not refused), so the replay
/// test can count both sides.
///
/// # Panics
///
/// When the policy disagrees with the oracle.
// A failed check is the oracle's verdict; `expect` names the broken invariant.
#[allow(clippy::expect_used)]
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let Some(case) = decode(data) else {
        return false;
    };

    // Refusal.
    let got = refusal(&case.method, &case.headers, case.path);
    assert_eq!(got, expected_refusal(&case), "refusal for {:?}", case.path);

    // Rewrite.
    let seen = rewrite_request_headers(&case.headers, case.token);
    for gone in [
        "cookie",
        "proxy-authorization",
        "forwarded",
        "referer",
        "x-real-ip",
    ] {
        assert!(seen.get(gone).is_none(), "{gone} reached the router");
    }
    assert!(
        seen.keys()
            .all(|name| !name.as_str().starts_with("x-forwarded-")),
        "a forwarding header reached the router"
    );
    let host: Vec<&[u8]> = seen
        .get_all("host")
        .iter()
        .map(HeaderValue::as_bytes)
        .collect();
    assert_eq!(host, [b"localhost".as_slice()]);
    let origin: Vec<&[u8]> = seen
        .get_all("origin")
        .iter()
        .map(HeaderValue::as_bytes)
        .collect();
    assert_eq!(origin, [b"http://localhost".as_slice()]);
    let auth: Vec<&[u8]> = seen
        .get_all("authorization")
        .iter()
        .map(HeaderValue::as_bytes)
        .collect();
    match case.token {
        None => assert!(auth.is_empty(), "a token appeared from nowhere"),
        Some(token) => {
            let want = format!("Bearer {token}");
            if HeaderValue::from_str(&want).is_ok() {
                assert_eq!(auth, [want.as_bytes()]);
            } else {
                assert!(auth.is_empty(), "an invalid token was sent");
            }
        }
    }
    let replaced = [
        "authorization",
        "cookie",
        "origin",
        "referer",
        "forwarded",
        "host",
        "proxy-authorization",
        "x-real-ip",
    ];
    for name in case.headers.keys() {
        if replaced.contains(&name.as_str()) || name.as_str().starts_with("x-forwarded-") {
            continue;
        }
        let page: Vec<&HeaderValue> = case.headers.get_all(name).iter().collect();
        let routed: Vec<&HeaderValue> = seen.get_all(name).iter().collect();
        assert_eq!(page, routed, "{name} changed on the way");
    }

    // Navigation.
    let nav = classify_navigation(case.navigation, case.join_open);
    let parsed = url::Url::parse(case.navigation).ok();
    let app = on_app_origin(case.navigation);
    match &nav {
        Navigation::App => assert!(app, "{} is not an app page", case.navigation),
        Navigation::JoinSignIn => {
            assert!(case.join_open && !app, "sign-in with no join open");
            let url = parsed.as_ref().expect("a sign-in URL parses");
            assert!(url.scheme() == "https" || loopback_http(url));
        }
        Navigation::JoinReturn(path_and_query) => {
            assert!(case.join_open && !app, "return with no join open");
            let url = parsed.as_ref().expect("a return URL parses");
            assert!(loopback_http(url) && url.path() == CALLBACK);
            assert!(
                path_and_query.starts_with(CALLBACK),
                "return {path_and_query} leaves the callback path"
            );
            let rest = &path_and_query[CALLBACK.len()..];
            assert!(rest.is_empty() || rest.starts_with('?'));
        }
        Navigation::Refuse => {
            assert!(!app, "an app page was refused");
            if case.join_open {
                if let Some(url) = parsed.as_ref() {
                    assert!(
                        url.scheme() != "https" && !loopback_http(url),
                        "{} was refused during a join",
                        case.navigation
                    );
                }
            }
        }
    }

    // Live updates.
    let label = if case.main_label {
        "main"
    } else {
        case.method.as_str()
    };
    let want = if label.as_bytes() != b"main" {
        Some(LiveRefusal::OtherWebview)
    } else if !app {
        Some(LiveRefusal::ForeignPage)
    } else {
        None
    };
    assert_eq!(
        live_refusal(label, case.navigation),
        want,
        "live for {label:?} on {:?}",
        case.navigation
    );

    // Sign-in time.
    let ttl = join_sign_in_ttl(case.join_body);
    let want = serde_json::from_slice::<serde_json::Value>(case.join_body)
        .ok()
        .and_then(|value| {
            let object = value.as_object()?;
            object.get("authorize_url")?.as_str()?;
            let secs = object.get("expires_in_secs")?.as_u64()?;
            Some(std::time::Duration::from_secs(secs.min(3600)))
        });
    assert_eq!(ttl, want, "sign-in time");

    got.is_none()
}
