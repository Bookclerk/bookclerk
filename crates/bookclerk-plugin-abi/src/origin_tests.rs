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

#[test]
fn frames_round_trip_and_wait_for_the_whole_head() {
    let req = request("/api/status", vec![header("Accept", "*/*")]);
    let mut framed = encode_origin_frame(&req).unwrap();
    framed.extend_from_slice(b"body bytes");
    for cut in 0..ORIGIN_FRAME_PREFIX_BYTES + 10 {
        assert_eq!(split_origin_frame(&framed[..cut]).unwrap(), None, "{cut}");
    }
    let (json, rest) = split_origin_frame(&framed).unwrap().unwrap();
    assert_eq!(rest, b"body bytes");
    let checked = decode_origin_request_frame(json).unwrap();
    assert_eq!(checked.target, "/api/status");

    let head = OriginResponseHead {
        status: 204,
        headers: vec![header("Set-Cookie", "a=b")],
    };
    let framed = encode_origin_frame(&head).unwrap();
    let (json, rest) = split_origin_frame(&framed).unwrap().unwrap();
    assert!(rest.is_empty());
    assert_eq!(decode_origin_response_frame(json).unwrap(), head);
}

#[test]
fn oversized_or_malformed_frames_are_refused() {
    let len = u32::try_from(MAX_ORIGIN_FRAME_HEAD_BYTES + 1).unwrap();
    assert!(split_origin_frame(&len.to_be_bytes()).is_err());
    assert!(decode_origin_request_frame(b"not json").is_err());
    assert!(decode_origin_request_frame(br#"{"method":"GET","target":"x"}"#).is_err());
    assert!(decode_origin_response_frame(br#"{"status":42,"headers":[]}"#).is_err());
}

#[test]
fn an_upgrade_keeps_its_subprotocols_and_drops_the_handshake() {
    let checked = check_origin_connect(&request(
        "/api/node/v1/socket",
        vec![
            header("Sec-WebSocket-Protocol", "bookclerk.v1"),
            header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            header("Sec-WebSocket-Version", "13"),
            header("Sec-WebSocket-Extensions", "permessage-deflate"),
            header("Cookie", "s=1"),
            header("X-Forwarded-For", "10.0.0.1"),
        ],
    ))
    .unwrap();
    assert_eq!(checked.method, "GET");
    assert_eq!(
        checked.headers,
        vec![
            header("Sec-WebSocket-Protocol", "bookclerk.v1"),
            header("Cookie", "s=1"),
        ]
    );
}

#[test]
fn an_upgrade_must_be_a_get_without_connection_fields() {
    let mut post = request("/socket", vec![]);
    post.method = "POST".into();
    assert!(check_origin_connect(&post)
        .unwrap_err()
        .message
        .contains("GET"));
    for name in ["Upgrade", "Connection"] {
        let err = check_origin_connect(&request("/socket", vec![header(name, "websocket")]))
            .unwrap_err();
        assert!(err.message.contains("connection"), "{name}: {err:?}");
    }
    assert!(check_origin_connect(&request("socket", vec![])).is_err());
}

#[test]
fn refusals_are_checked() {
    let refusal = |status: u16, body: usize| OriginRefusal {
        status,
        headers: vec![header("Content-Type", "application/json")],
        body: vec![b'x'; body],
    };
    check_origin_refusal(&refusal(403, 10)).unwrap();
    check_origin_refusal(&refusal(200, MAX_ORIGIN_REFUSAL_BODY_BYTES as usize)).unwrap();
    for status in [0, 101, 199, 600] {
        assert!(check_origin_refusal(&refusal(status, 0)).is_err(), "{status}");
    }
    assert!(check_origin_refusal(&refusal(403, MAX_ORIGIN_REFUSAL_BODY_BYTES as usize + 1))
        .unwrap_err()
        .message
        .contains("body"));
    let mut hop = refusal(403, 0);
    hop.headers.push(header("Transfer-Encoding", "chunked"));
    assert!(check_origin_refusal(&hop).is_err());
}
