//! Shared oracle for the `origin_request` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one thing a relay `ingress` plugin hands the host. Byte 0
//! modulo 4 selects the form: 0 is the body of a `POST /origin/fetch` on the
//! isolate bridge (a framed head, then the request body), 1 is a request
//! head's JSON, 2 is a response head's JSON, and 3 is the origin an ingress
//! plugin reports from `start` (lossy UTF-8). The invariants:
//!
//! - splitting, decoding and checking never panic;
//! - [`split_origin_frame`] waits for more bytes exactly while the frame is
//!   incomplete, refuses exactly the declared lengths past
//!   [`MAX_ORIGIN_FRAME_HEAD_BYTES`], and otherwise returns the declared
//!   head and every byte after it, so the request body starts at the right
//!   byte and a guest cannot make the host buffer without bound;
//! - a request head is accepted exactly when the independent restatement of
//!   the method, target, header and client-address rules below holds; an
//!   accepted head keeps its method, target and client address, drops
//!   exactly the `host` and forwarding fields, and frames back to itself;
//! - a response head is accepted exactly when the restated status and
//!   header rules hold;
//! - an ingress origin is accepted exactly when the restated host-name
//!   rules hold, and a WHATWG URL parser (the `url` crate) reads every
//!   accepted origin as `https`, the same host, no port, user info, path,
//!   query or fragment.

use std::net::IpAddr;

use bookclerk_plugin_abi::origin::{
    decode_origin_request_frame, decode_origin_response_frame, encode_origin_frame,
    parse_ingress_origin, split_origin_frame, OriginResponseHead, MAX_ORIGIN_FRAME_HEAD_BYTES,
};
use bookclerk_plugin_abi::{
    IngressStarted, OriginHeader, OriginRequest, MAX_INGRESS_ORIGIN_BYTES, MAX_ORIGIN_HEADERS,
    MAX_ORIGIN_HEADER_BYTES, MAX_ORIGIN_TARGET_BYTES,
};

/// Largest input worth parsing: a whole frame at its limit plus some body.
pub const MAX_INPUT_BYTES: usize = MAX_ORIGIN_FRAME_HEAD_BYTES + 64 * 1024;

/// Header fields that describe one connection, restated (lowercase).
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Request fields the host drops, restated (lowercase).
const IGNORED: [&str; 6] = [
    "host",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
];

/// Longest method the host accepts, restated.
const MAX_METHOD_BYTES: usize = 32;

/// RFC 9110 `tchar`, restated.
fn tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// True when `text` is a non-empty token.
fn token(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(tchar)
}

/// True when `name` is in `list`, ignoring ASCII case.
fn listed(name: &str, list: &[&str]) -> bool {
    let lower = name.to_ascii_lowercase();
    list.contains(&lower.as_str())
}

/// The header rules, restated.
fn headers_ok(headers: &[OriginHeader]) -> bool {
    if headers.len() > MAX_ORIGIN_HEADERS as usize {
        return false;
    }
    let total: usize = headers.iter().map(|h| h.name.len() + h.value.len()).sum();
    total <= MAX_ORIGIN_HEADER_BYTES as usize
        && headers.iter().all(|h| {
            token(&h.name)
                && !listed(&h.name, &HOP_BY_HOP)
                && h.value
                    .bytes()
                    .all(|b| b == b'\t' || (b' '..=b'~').contains(&b))
        })
}

/// The request rules, restated; `Some(client)` when the head is acceptable.
fn request_ok(request: &OriginRequest) -> Option<Option<IpAddr>> {
    let method_ok = request.method.len() <= MAX_METHOD_BYTES && token(&request.method);
    let t = request.target.as_bytes();
    let target_ok = t.len() <= MAX_ORIGIN_TARGET_BYTES as usize
        && t.first() == Some(&b'/')
        && t.get(1) != Some(&b'/')
        && t.iter().all(|&b| (b'!'..=b'~').contains(&b) && b != b'#');
    if !(method_ok && target_ok && headers_ok(&request.headers)) {
        return None;
    }
    match request.client_address.as_deref() {
        None | Some("") => Some(None),
        Some(text) => text.parse::<IpAddr>().ok().map(Some),
    }
}

/// Checks one request head's JSON against the restated rules.
// Oracle: a failed expectation is the finding the fuzzer reports.
#[allow(clippy::expect_used)]
fn check_request_json(json: &[u8]) {
    let decoded = decode_origin_request_frame(json);
    let Ok(request) = serde_json::from_slice::<OriginRequest>(json) else {
        assert!(
            decoded.is_err(),
            "a head that is not an OriginRequest passed"
        );
        return;
    };
    let expected = request_ok(&request);
    let checked = match (decoded, expected) {
        (Ok(checked), Some(client)) => {
            assert_eq!(checked.client, client, "client address changed");
            checked
        }
        (Err(_), None) => return,
        (Ok(_), None) => panic!("a head breaking a rule passed: {request:?}"),
        (Err(err), Some(_)) => panic!("an acceptable head was refused: {err:?} {request:?}"),
    };
    assert_eq!(checked.method, request.method);
    assert_eq!(checked.target, request.target);
    let kept: Vec<&OriginHeader> = request
        .headers
        .iter()
        .filter(|h| !listed(&h.name, &IGNORED))
        .collect();
    assert_eq!(checked.headers.iter().collect::<Vec<_>>(), kept);
    // What the host passes on frames back to the same checked head.
    let again = OriginRequest {
        method: checked.method.clone(),
        target: checked.target.clone(),
        headers: checked.headers.clone(),
        client_address: checked.client.map(|ip| ip.to_string()),
    };
    let frame = encode_origin_frame(&again).expect("an accepted head frames");
    let (head, rest) = split_origin_frame(&frame)
        .expect("own frame splits")
        .expect("own frame is whole");
    assert!(rest.is_empty());
    assert_eq!(
        decode_origin_request_frame(head).expect("own frame decodes"),
        checked
    );
}

/// Checks a framed request body against the restated split rules.
// Oracle: a failed expectation is the finding the fuzzer reports.
#[allow(clippy::expect_used)]
fn check_frame(buf: &[u8]) {
    let split = split_origin_frame(buf);
    let Some(prefix) = buf.get(..4) else {
        assert!(matches!(split, Ok(None)), "a partial prefix did not wait");
        return;
    };
    let len = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    if len > MAX_ORIGIN_FRAME_HEAD_BYTES {
        assert!(split.is_err(), "an oversized head length was accepted");
        return;
    }
    if buf.len() < 4 + len {
        assert!(
            matches!(split, Ok(None)),
            "an incomplete frame did not wait"
        );
        return;
    }
    let (head, rest) = split.expect("whole frame").expect("whole frame splits");
    assert_eq!(head, &buf[4..4 + len]);
    assert_eq!(rest, &buf[4 + len..]);
    check_request_json(head);
}

/// Checks one response head's JSON against the restated rules.
fn check_response_json(json: &[u8]) {
    let decoded = decode_origin_response_frame(json);
    let Ok(head) = serde_json::from_slice::<OriginResponseHead>(json) else {
        assert!(
            decoded.is_err(),
            "a head that is not a response head passed"
        );
        return;
    };
    let ok = (100..=599).contains(&head.status) && headers_ok(&head.headers);
    assert_eq!(
        decoded.is_ok(),
        ok,
        "response head verdict differs: {head:?}"
    );
}

/// Checks an ingress origin against the restated rules and a URL parser.
fn check_origin(text: &str) {
    let parsed = parse_ingress_origin(&IngressStarted {
        origin: text.to_owned(),
    });
    let host = text.strip_prefix("https://");
    let ok = host.is_some_and(|host| {
        let labels: Vec<&str> = host.split('.').collect();
        host.len() <= MAX_INGRESS_ORIGIN_BYTES as usize
            && labels.len() >= 2
            && labels.iter().all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.bytes()
                        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
            })
            && labels
                .last()
                .and_then(|top| top.bytes().next())
                .is_some_and(|b| b.is_ascii_lowercase())
    });
    assert_eq!(
        parsed.is_ok(),
        ok,
        "ingress origin verdict differs: {text:?}"
    );
    let (Ok(origin), Some(host)) = (parsed, host) else {
        return;
    };
    assert_eq!(origin.host(), host);
    match url::Url::parse(text) {
        Ok(url) => {
            assert_eq!(url.scheme(), "https");
            assert_eq!(
                url.host_str(),
                Some(host),
                "the URL parser reads another host"
            );
            assert_eq!(url.port(), None);
            assert!(url.username().is_empty() && url.password().is_none());
            assert_eq!(url.path(), "/");
            assert!(url.query().is_none() && url.fragment().is_none());
        }
        // A label shaped like punycode that does not decode is refused by
        // the URL parser's IDNA step; the host still names one DNS host.
        Err(_) => assert!(
            host.split('.').any(|l| l.starts_with("xn--")),
            "the URL parser refused an accepted origin: {text:?}"
        ),
    }
}

/// Runs every invariant on one input.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&form, rest)) = data.split_first() else {
        return;
    };
    match form % 4 {
        0 => check_frame(rest),
        1 => check_request_json(rest),
        2 => check_response_json(rest),
        _ => check_origin(&String::from_utf8_lossy(rest)),
    }
}
