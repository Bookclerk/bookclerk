//! Unit tests for the `/sockets/*` route parser and reply encoder.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use bookclerk_plugin_abi::{PluginErrorCode, MAX_SOCKET_ATTACHMENT_BYTES};

/// Parses `body` (a JSON value) as operation `op`.
fn parse(op: &str, body: &serde_json::Value) -> Result<SocketCall, PluginError> {
    parse_socket_call(op, &serde_json::to_vec(body).unwrap())
}

#[test]
fn connect_carries_every_field_and_decodes_base64() {
    let call = parse(
        "connect",
        &serde_json::json!({
            "url": "wss://abs.example:13378/socket.io/?EIO=4&transport=websocket",
            "protocols": ["chat"],
            "headers": [{ "name": "Authorization", "value": "Bearer t" }],
            "autoResponses": [
                { "request": { "text": "2" }, "response": { "text": "3" } },
                { "request": { "binary": "AAE=" }, "response": { "binary": "AgM=" } }
            ],
            "attachment": "aGk="
        }),
    )
    .unwrap();
    let SocketCall::Connect(request) = call else {
        panic!("expected connect");
    };
    assert_eq!(request.protocols, vec!["chat".to_string()]);
    assert_eq!(request.headers[0].name, "Authorization");
    assert_eq!(
        request.auto_responses[0].request,
        SocketFrame::Text("2".into())
    );
    assert_eq!(
        request.auto_responses[1].response,
        SocketFrame::Binary(vec![2, 3])
    );
    assert_eq!(request.attachment, b"hi");
}

#[test]
fn connect_defaults_the_optional_fields() {
    let call = parse(
        "connect",
        &serde_json::json!({ "url": "ws://127.0.0.1:9/" }),
    )
    .unwrap();
    assert_eq!(
        call,
        SocketCall::Connect(SocketConnectRequest {
            url: "ws://127.0.0.1:9/".into(),
            ..SocketConnectRequest::default()
        })
    );
}

#[test]
fn connect_applies_the_abi_checks() {
    let refused = [
        serde_json::json!({ "url": "https://abs.example/" }),
        serde_json::json!({ "url": "wss://abs.example/", "headers": [{ "name": "Host", "value": "x" }] }),
        serde_json::json!({ "url": "wss://abs.example/", "protocols": ["has space"] }),
        serde_json::json!({
            "url": "wss://abs.example/",
            "autoResponses": [{ "request": { "text": "" }, "response": { "text": "3" } }]
        }),
    ];
    for body in refused {
        let err = parse("connect", &body).unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{body}");
    }
    let big = bookclerk_plugin_abi::json_bytes::encode(&vec![
        0;
        MAX_SOCKET_ATTACHMENT_BYTES as usize
            + 1
    ]);
    let err = parse(
        "connect",
        &serde_json::json!({ "url": "wss://abs.example/", "attachment": big }),
    )
    .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
}

#[test]
fn unknown_fields_and_malformed_json_are_refused() {
    for (op, body) in [
        (
            "connect",
            br#"{"url":"wss://a.example/","extra":1}"#.as_slice(),
        ),
        (
            "send",
            br#"{"socket":"g1_a","frame":{"text":"x","binary":""}}"#.as_slice(),
        ),
        (
            "send",
            br#"{"socket":"g1_a","frame":{"binary":"not base64!"}}"#.as_slice(),
        ),
        ("list", br#"{"socket":"g1_a"}"#.as_slice()),
        ("list", b"[]".as_slice()),
        ("send", br#"["g1_a",{"text":"x"}]"#.as_slice()),
        ("close", b"not json".as_slice()),
        ("attachment", br#"{"socket":"g1_a"}"#.as_slice()),
    ] {
        let err = parse_socket_call(op, body).unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{op}");
    }
}

#[test]
fn send_checks_the_id_and_the_frame_size() {
    let ok = parse(
        "send",
        &serde_json::json!({ "socket": "g2_ab-1", "frame": { "binary": "AQID" } }),
    )
    .unwrap();
    assert_eq!(
        ok,
        SocketCall::Send {
            socket: "g2_ab-1".into(),
            frame: SocketFrame::Binary(vec![1, 2, 3]),
        }
    );
    let err = parse(
        "send",
        &serde_json::json!({ "socket": "../x", "frame": { "text": "hi" } }),
    )
    .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::InvalidParams);
    let huge = "x".repeat(MAX_SOCKET_FRAME_BYTES as usize + 1);
    let err = parse(
        "send",
        &serde_json::json!({ "socket": "g1_a", "frame": { "text": huge } }),
    )
    .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
}

#[test]
fn close_maps_zero_to_normal_and_refuses_reserved_codes() {
    let call = parse("close", &serde_json::json!({ "socket": "g1_a" })).unwrap();
    assert_eq!(
        call,
        SocketCall::Close {
            socket: "g1_a".into(),
            code: 1000,
            reason: String::new(),
        }
    );
    let err = parse(
        "close",
        &serde_json::json!({ "socket": "g1_a", "code": 1006 }),
    )
    .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::InvalidParams);
}

#[test]
fn attachment_and_list_parse() {
    assert_eq!(
        parse(
            "attachment",
            &serde_json::json!({ "socket": "g1_a", "attachment": "" })
        )
        .unwrap(),
        SocketCall::SetAttachment {
            socket: "g1_a".into(),
            attachment: Vec::new(),
        }
    );
    assert_eq!(
        parse("list", &serde_json::json!({})).unwrap(),
        SocketCall::List
    );
}

#[test]
fn an_unknown_operation_is_not_found() {
    let err = parse("open", &serde_json::json!({})).unwrap_err();
    assert_eq!(err.code, PluginErrorCode::NotFound);
    assert!(!SOCKET_ROUTE_OPS.contains(&"open"));
}

#[test]
fn replies_encode_as_the_sdk_reads_them() {
    let info = SocketInfo {
        socket: "g1_a".into(),
        url: "wss://abs.example/".into(),
        protocol: Some("chat".into()),
        attachment: b"hi".to_vec(),
    };
    let connected: serde_json::Value =
        serde_json::from_slice(&socket_reply_json(&SocketReply::Connected(info.clone())).unwrap())
            .unwrap();
    assert_eq!(
        connected,
        serde_json::json!({
            "socket": "g1_a",
            "url": "wss://abs.example/",
            "protocol": "chat",
            "attachment": "aGk="
        })
    );
    assert_eq!(socket_reply_json(&SocketReply::Done).unwrap(), b"{}");
    let listed: serde_json::Value =
        serde_json::from_slice(&socket_reply_json(&SocketReply::Listed(vec![info])).unwrap())
            .unwrap();
    assert_eq!(listed["sockets"][0]["socket"], "g1_a");
}

#[test]
fn route_frames_round_trip_with_abi_frames() {
    for frame in [
        SocketFrame::Text("t".into()),
        SocketFrame::Binary(vec![0, 255]),
    ] {
        let route = RouteFrame::from(frame.clone());
        let json = serde_json::to_vec(&route).unwrap();
        let back: RouteFrame = serde_json::from_slice(&json).unwrap();
        assert_eq!(SocketFrame::from(back), frame);
    }
}

#[test]
fn the_body_cap_fits_a_largest_frame() {
    let frame = RouteSend {
        socket: "g1_a".into(),
        frame: RouteFrame::Binary(vec![0xff; MAX_SOCKET_FRAME_BYTES as usize]),
    };
    let body = serde_json::to_vec(&frame).unwrap();
    assert!(body.len() <= MAX_SOCKET_ROUTE_BODY_BYTES as usize);
}
