//! Shared oracle for the `egress_relay_head` fuzz target and the
//! `bookclerk-workerd` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The launcher's egress relay reads request heads from workerd (which runs
//! the plugin's code) and response heads from whatever server the plugin
//! reached. Byte 0 selects the form: even is a request head, odd a response
//! head; the rest is the head. The invariants, checked against `httparse`
//! as an independent HTTP/1.1 parser:
//!
//! - parsing never panics;
//! - an accepted request is one `httparse` reads as a complete request with
//!   the same method; a `CONNECT` names a valid authority, every other
//!   method an absolute `http(s)://` URL whose host and port match the
//!   tunnel target; a `CONNECT` whose host ends in the STARTTLS suffix
//!   becomes a framed tunnel to the name before it, and no other does;
//! - the rewritten request head is itself a complete request to `httparse`,
//!   in origin form, with exactly one `host` header naming the target, a
//!   `connection` header of `close` or `upgrade`, and none of the hop-by-hop
//!   or proxy headers the relay strips; every other header survives;
//! - an accepted response is one `httparse` reads with the same status, and
//!   the head handed back is complete to `httparse`; a final head (not `1xx`)
//!   carries exactly one `connection: close`.

use bookclerk_workerd::egress_relay::{
    parse_request_head, parse_response_head, RelayRequest, Scheme,
};
use bookclerk_workerd::egress_starttls::STARTTLS_HOST_SUFFIX;

/// Headers the relay never forwards from workerd.
const STRIPPED: [&str; 6] = [
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "expect",
];

/// Runs every invariant on one input.
pub fn check(data: &[u8]) {
    let Some((&selector, head)) = data.split_first() else {
        return;
    };
    if selector % 2 == 0 {
        check_request(head);
    } else {
        check_response(head);
    }
}

fn check_request(head: &[u8]) {
    let Ok(parsed) = parse_request_head(head) else {
        return;
    };
    let mut headers = [httparse::EMPTY_HEADER; 160];
    let mut original = httparse::Request::new(&mut headers);
    let status = original
        .parse(head)
        .expect("httparse rejects an accepted request");
    assert!(status.is_complete(), "accepted request is incomplete");
    assert_eq!(
        status.unwrap(),
        head.len(),
        "accepted head has trailing bytes"
    );
    let method = original.method.expect("method");
    let path = original.path.expect("target");
    match parsed {
        RelayRequest::Tunnel(target) => {
            assert_eq!(method, "CONNECT");
            assert!(!target.host.is_empty());
            assert_ne!(target.port, 0);
            assert_eq!(target.host, target.host.to_ascii_lowercase());
            assert!(path.eq_ignore_ascii_case(&target.authority()));
            assert!(!target.host.ends_with(STARTTLS_HOST_SUFFIX));
        }
        RelayRequest::FramedTunnel(target) => {
            assert_eq!(method, "CONNECT");
            assert!(!target.host.is_empty() && !target.host.ends_with('.'));
            assert_ne!(target.port, 0);
            assert_eq!(target.host, target.host.to_ascii_lowercase());
            let dialed = format!("{}{STARTTLS_HOST_SUFFIX}:{}", target.host, target.port);
            assert!(path.eq_ignore_ascii_case(&dialed), "{path} vs {dialed}");
        }
        RelayRequest::Forward {
            scheme,
            target,
            head: rewritten,
        } => {
            assert_ne!(method, "CONNECT");
            let lower = path.to_ascii_lowercase();
            let (prefix, default_port) = match scheme {
                Scheme::Http => ("http://", 80),
                Scheme::Https => ("https://", 443),
            };
            assert!(lower.starts_with(prefix), "{path}");
            let rest = &lower[prefix.len()..];
            let authority = rest.split(['/', '?']).next().unwrap_or("");
            let expected = if target.port == default_port {
                [
                    target.authority(),
                    target.authority().replace(&format!(":{default_port}"), ""),
                ]
            } else {
                [target.authority(), target.authority()]
            };
            assert!(
                expected.iter().any(|candidate| candidate == authority)
                    || authority == format!("{}:{}", host_in_url(&target.host), target.port)
                    || (target.port == default_port && authority == host_in_url(&target.host)),
                "url authority {authority:?} vs target {target:?}"
            );
            check_rewritten(&rewritten, method, &target.authority(), original.headers);
        }
    }
}

/// `host` as it appears in a URL: IPv6 literals in brackets.
fn host_in_url(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn check_rewritten(
    rewritten: &[u8],
    method: &str,
    authority: &str,
    original: &[httparse::Header<'_>],
) {
    let mut headers = [httparse::EMPTY_HEADER; 160];
    let mut request = httparse::Request::new(&mut headers);
    let status = request
        .parse(rewritten)
        .expect("httparse rejects the rewritten head");
    assert!(status.is_complete());
    assert_eq!(status.unwrap(), rewritten.len());
    assert_eq!(request.method, Some(method));
    let path = request.path.expect("path");
    assert!(
        path.starts_with('/'),
        "rewritten target is not origin form: {path}"
    );
    let named = |name: &str| {
        request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .collect::<Vec<_>>()
    };
    let hosts = named("host");
    assert_eq!(hosts.len(), 1, "exactly one host header");
    assert!(
        hosts[0].value.eq_ignore_ascii_case(authority.as_bytes())
            || hosts[0].value.eq_ignore_ascii_case(
                authority
                    .trim_end_matches(":80")
                    .trim_end_matches(":443")
                    .as_bytes()
            )
    );
    let connection = named("connection");
    assert_eq!(connection.len(), 1, "exactly one connection header");
    assert!(
        connection[0].value == b"close" || connection[0].value == b"upgrade",
        "connection header"
    );
    for stripped in STRIPPED.iter().skip(2) {
        assert!(named(stripped).is_empty(), "{stripped} survived");
    }
    for header in original {
        let lower = header.name.to_ascii_lowercase();
        if STRIPPED.contains(&lower.as_str()) {
            continue;
        }
        assert!(
            request
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case(header.name) && h.value == header.value),
            "header {} was dropped",
            header.name
        );
    }
}

fn check_response(head: &[u8]) {
    let Ok(parsed) = parse_response_head(head) else {
        return;
    };
    let mut headers = [httparse::EMPTY_HEADER; 160];
    let mut original = httparse::Response::new(&mut headers);
    let status = original
        .parse(head)
        .expect("httparse rejects an accepted response");
    assert!(status.is_complete());
    assert_eq!(original.code, Some(parsed.status));

    let mut headers = [httparse::EMPTY_HEADER; 160];
    let mut handed = httparse::Response::new(&mut headers);
    let status = handed
        .parse(&parsed.head)
        .expect("httparse rejects the head handed to workerd");
    assert!(status.is_complete());
    assert_eq!(status.unwrap(), parsed.head.len());
    assert_eq!(handed.code, Some(parsed.status));
    if parsed.status >= 200 {
        let connection: Vec<_> = handed
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("connection"))
            .collect();
        assert_eq!(
            connection.len(),
            1,
            "final head needs one connection header"
        );
        assert_eq!(connection[0].value, b"close");
    }
}
