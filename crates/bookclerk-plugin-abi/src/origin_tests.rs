//! Tests for the `ORIGIN` binding and `ingress` entrypoint checks.

use super::*;

/// A header field.
fn header(name: &str, value: &str) -> OriginHeader {
    OriginHeader {
        name: name.into(),
        value: value.into(),
    }
}

/// A request with `target` and `headers`.
fn request(target: &str, headers: Vec<OriginHeader>) -> OriginRequest {
    OriginRequest {
        method: "GET".into(),
        target: target.into(),
        headers,
        client_address: Some("203.0.113.9".into()),
    }
}

#[test]
fn a_plain_request_passes_without_forwarding_headers() {
    let checked = check_origin_request(&request(
        "/api/status?x=1",
        vec![
            header("Accept", "application/json"),
            header("Host", "evil.example"),
            header("X-Forwarded-For", "10.0.0.1"),
            header("Cookie", "a=b; c=d"),
        ],
    ))
    .unwrap();
    assert_eq!(checked.method, "GET");
    assert_eq!(checked.target, "/api/status?x=1");
    assert_eq!(
        checked.headers,
        vec![
            header("Accept", "application/json"),
            header("Cookie", "a=b; c=d")
        ]
    );
    assert_eq!(checked.client, Some("203.0.113.9".parse().unwrap()));
}

#[test]
fn a_missing_client_address_is_none() {
    let mut req = request("/", Vec::new());
    req.client_address = None;
    assert_eq!(check_origin_request(&req).unwrap().client, None);
    req.client_address = Some(String::new());
    assert_eq!(check_origin_request(&req).unwrap().client, None);
    req.client_address = Some("2001:db8::1".into());
    assert!(check_origin_request(&req).unwrap().client.is_some());
}

#[test]
fn bad_requests_are_refused() {
    let mut bad_method = request("/", Vec::new());
    bad_method.method = "GE T".into();
    let mut long_method = request("/", Vec::new());
    long_method.method = "A".repeat(MAX_ORIGIN_METHOD_BYTES + 1);
    let mut bad_client = request("/", Vec::new());
    bad_client.client_address = Some("localhost".into());
    let too_many = request(
        "/",
        (0..=MAX_ORIGIN_HEADERS)
            .map(|i| header(&format!("x-{i}"), "1"))
            .collect(),
    );
    let too_big = request(
        "/",
        vec![header(
            "x-big",
            &"a".repeat(MAX_ORIGIN_HEADER_BYTES as usize),
        )],
    );
    for refused in [
        bad_method,
        long_method,
        bad_client,
        too_many,
        too_big,
        request("api/status", Vec::new()),
        request("//evil.example/", Vec::new()),
        request("https://evil.example/", Vec::new()),
        request("/a b", Vec::new()),
        request("/a#frag", Vec::new()),
        request("/caf\u{e9}", Vec::new()),
        request(
            &format!("/{}", "a".repeat(MAX_ORIGIN_TARGET_BYTES as usize)),
            Vec::new(),
        ),
        request("/", vec![header("Transfer-Encoding", "chunked")]),
        request("/", vec![header("Connection", "close")]),
        request("/", vec![header("Upgrade", "websocket")]),
        request("/", vec![header("bad name", "1")]),
        request("/", vec![header("", "1")]),
        request("/", vec![header("x", "a\r\nInjected: 1")]),
        request("/", vec![header("x", "caf\u{e9}")]),
    ] {
        assert!(check_origin_request(&refused).is_err(), "{refused:?}");
    }
}

#[test]
fn response_heads_are_checked() {
    assert!(check_origin_response(200, &[header("Set-Cookie", "a=b")]).is_ok());
    assert!(check_origin_response(99, &[]).is_err());
    assert!(check_origin_response(600, &[]).is_err());
    assert!(check_origin_response(200, &[header("Transfer-Encoding", "chunked")]).is_err());
}

#[test]
fn only_an_https_host_name_is_an_ingress_origin() {
    let origin = |text: &str| {
        parse_ingress_origin(&IngressStarted {
            origin: text.into(),
        })
    };
    let ok = origin("https://library.example.workers.dev").unwrap();
    assert_eq!(ok.as_str(), "https://library.example.workers.dev");
    assert_eq!(ok.host(), "library.example.workers.dev");
    assert_eq!(ok.to_string(), "https://library.example.workers.dev");
    for refused in [
        "http://library.example",
        "https://library.example/",
        "https://library.example:8443",
        "https://library.example/path",
        "https://user@library.example",
        "https://Library.example",
        "https://library.example.",
        "https://localhost",
        "https://192.168.1.10",
        "https://-bad.example",
        "https://bad-.example",
        "https://a..example",
        "https://",
        "library.example",
        "",
    ] {
        assert!(origin(refused).is_err(), "{refused}");
    }
    let long = format!("https://{}.example", "a.".repeat(130));
    assert!(origin(&long).is_err());
    let label = format!("https://{}.example", "a".repeat(64));
    assert!(origin(&label).is_err());
}
