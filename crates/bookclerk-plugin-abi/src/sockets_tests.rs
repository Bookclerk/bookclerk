//! Unit tests for the socket checks.

use super::*;
use crate::generated::SocketHeader;

fn request(url: &str) -> SocketConnectRequest {
    SocketConnectRequest {
        url: url.into(),
        ..SocketConnectRequest::default()
    }
}

fn text(value: &str) -> SocketFrame {
    SocketFrame::Text(value.into())
}

#[test]
fn urls_parse_into_their_targets() {
    let target = parse_socket_url("wss://ABS.example.com/socket.io/?EIO=4").expect("wss");
    assert_eq!(
        target,
        SocketTarget {
            secure: true,
            host: "abs.example.com".into(),
            port: 443,
        }
    );
    let target = parse_socket_url("ws://10.0.0.5:13378/socket.io").expect("ws");
    assert_eq!((target.secure, target.port), (false, 13378));
    let target = parse_socket_url("ws://[::1]:8080").expect("v6");
    assert_eq!((target.host.as_str(), target.port), ("::1", 8080));
    let target = parse_socket_url("ws://192.168.1.10:8080").expect("v4");
    assert_eq!(target.host, "192.168.1.10");
    assert!(parse_socket_url("ws://host-1.example_2.com").is_ok());
    let target = parse_socket_url("WS://host").expect("case");
    assert_eq!(target.port, 80);
}

#[test]
fn bad_urls_are_refused() {
    for url in [
        "",
        "https://example.com",
        "wss://",
        "wss://user:pass@example.com",
        "wss://example.com/#frag",
        "wss://exa%6dple.com",
        "wss://example.com:0",
        "wss://example.com:070",
        "wss://example.com:65536",
        "wss://example.com:+1",
        "wss://::1",
        "wss://[::1",
        "wss://[::1]x",
        "wss://example.com/ path",
        "wss://[example.com]",
        "wss://[]",
        "wss://exa\\mple.com",
        "wss://exa\u{e9}mple.com",
        "wss://example.com:",
        "ws://127.1",
        "ws://0x7f.0.0.1",
        "ws://example.0x10",
        "ws://010.0.0.1",
        "ws://1.2.3.4.",
    ] {
        assert!(parse_socket_url(url).is_err(), "{url:?} should be refused");
    }
    let long = format!("wss://example.com/{}", "a".repeat(2048));
    assert!(parse_socket_url(&long).is_err());
}

#[test]
fn connect_requests_check_headers_protocols_and_attachments() {
    let mut ok = request("wss://example.com");
    ok.protocols = vec!["chat".into()];
    ok.headers = vec![SocketHeader {
        name: "Authorization".into(),
        value: "Bearer secret".into(),
    }];
    assert!(validate_socket_connect(&ok).is_ok());

    let mut forbidden = ok.clone();
    forbidden.headers[0].name = "Sec-WebSocket-Key".into();
    assert!(validate_socket_connect(&forbidden).is_err());
    forbidden.headers[0].name = "Host".into();
    assert!(validate_socket_connect(&forbidden).is_err());

    let mut injected = ok.clone();
    injected.headers[0].value = "x\r\nX-Evil: 1".into();
    let err = validate_socket_connect(&injected).expect_err("crlf");
    assert!(
        !err.to_string().contains("x\r\n"),
        "the value is never echoed: {err}"
    );

    let mut protocol = ok.clone();
    protocol.protocols = vec!["two words".into()];
    assert!(validate_socket_connect(&protocol).is_err());

    let mut too_many = ok.clone();
    too_many.headers = (0..17)
        .map(|i| SocketHeader {
            name: format!("x-{i}"),
            value: String::new(),
        })
        .collect();
    assert!(validate_socket_connect(&too_many).is_err());

    let mut attachment = ok;
    attachment.attachment = vec![0; 2049];
    assert!(validate_socket_connect(&attachment).is_err());
}

#[test]
fn heartbeats_match_exactly_and_are_bounded() {
    let pairs = vec![SocketAutoResponse {
        request: text("2"),
        response: text("3"),
    }];
    assert!(check_auto_responses(&pairs).is_ok());
    assert_eq!(auto_response_for(&text("2"), &pairs), Some(&text("3")));
    assert_eq!(auto_response_for(&text("2 "), &pairs), None);
    assert_eq!(
        auto_response_for(&SocketFrame::Binary(b"2".to_vec()), &pairs),
        None,
        "a binary frame never matches a text heartbeat"
    );
    let empty = vec![SocketAutoResponse {
        request: text(""),
        response: text("3"),
    }];
    assert!(check_auto_responses(&empty).is_err());
    let five = vec![pairs[0].clone(); 5];
    assert!(check_auto_responses(&five).is_err());
}

#[test]
fn close_codes_and_ids_are_checked() {
    assert_eq!(check_socket_close(0, "").expect("default"), 1000);
    assert_eq!(check_socket_close(4001, "bye").expect("app"), 4001);
    for code in [999, 1005, 1006, 1015, 2000, 5000] {
        assert!(check_socket_close(code, "").is_err(), "{code}");
    }
    assert!(check_socket_close(1000, &"r".repeat(124)).is_err());
    assert!(check_socket_id("s-1_A").is_ok());
    assert!(check_socket_id("").is_err());
    assert!(check_socket_id("a/b").is_err());
    assert!(check_socket_id(&"a".repeat(65)).is_err());
}

#[test]
fn frames_and_attachments_are_capped() {
    assert!(check_socket_frame(&text("hello")).is_ok());
    assert!(check_socket_frame(&SocketFrame::Binary(vec![0; 262_145])).is_err());
    assert!(check_socket_attachment(&[0; 2048]).is_ok());
    assert!(check_socket_attachment(&[0; 2049]).is_err());
}

#[test]
fn connect_requests_refuse_each_list_and_value_bound() {
    let base = request("wss://example.com");

    let mut protocols = base.clone();
    protocols.protocols = (0..17).map(|i| format!("p{i}")).collect();
    assert!(validate_socket_connect(&protocols).is_err());

    let mut name = base.clone();
    name.headers = vec![SocketHeader {
        name: "bad name".into(),
        value: String::new(),
    }];
    assert!(validate_socket_connect(&name).is_err());

    let mut long = base.clone();
    long.headers = vec![SocketHeader {
        name: "X-Long".into(),
        value: "v".repeat(2049),
    }];
    assert!(validate_socket_connect(&long).is_err());

    let mut tab = base.clone();
    tab.headers = vec![SocketHeader {
        name: "X-Tab".into(),
        value: "a\tb".into(),
    }];
    assert!(validate_socket_connect(&tab).is_ok(), "a tab is allowed");

    let mut heartbeat = base;
    heartbeat.auto_responses = vec![SocketAutoResponse {
        request: text("2"),
        response: SocketFrame::Binary(vec![0; 257]),
    }];
    assert!(validate_socket_connect(&heartbeat).is_err());
}
