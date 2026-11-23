//! Unit tests for [`super::SocketTable`].

use super::*;
use bookclerk_plugin_abi::sockets::check_socket_id;
use bookclerk_plugin_abi::{PluginErrorCode, MAX_SOCKET_FRAME_BYTES};

/// A valid connect request for `url` with one Socket.io heartbeat pair.
fn request(url: &str) -> SocketConnectRequest {
    SocketConnectRequest {
        url: url.into(),
        auto_responses: vec![SocketAutoResponse {
            request: SocketFrame::Text("2".into()),
            response: SocketFrame::Text("3".into()),
        }],
        attachment: b"cursor-1".to_vec(),
        ..SocketConnectRequest::default()
    }
}

/// A table with one open socket; returns the table and its id.
fn open_one() -> (SocketTable, String) {
    let mut table = SocketTable::new("t-");
    let (id, target) = table
        .reserve(&request("wss://abs.example.com/socket.io/"))
        .unwrap();
    assert_eq!(target.scheme, SocketScheme::Wss);
    assert_eq!(target.port, 443);
    assert!(table.establish(&id, Some("chat".into())).is_some());
    (table, id)
}

#[test]
fn ids_are_valid_unique_and_prefixed() {
    let mut table = SocketTable::new("bad prefix!/");
    let (a, _) = table.reserve(&request("ws://h.example/")).unwrap();
    assert!(table.abandon(&a));
    let (b, _) = table.reserve(&request("ws://h.example/")).unwrap();
    assert_ne!(a, b, "an abandoned id is not reused");
    assert!(a.starts_with("badprefix"));
    check_socket_id(&a).unwrap();
    let long = SocketTable::new(&"x".repeat(200));
    assert_eq!(long.prefix.len(), MAX_ID_PREFIX_BYTES);
}

#[test]
fn reserve_refuses_bad_requests_and_the_ninth_socket() {
    let mut table = SocketTable::new("s");
    assert!(table.reserve(&request("http://h.example/")).is_err());
    assert!(table.is_empty());
    for _ in 0..MAX_SOCKETS_PER_INSTANCE {
        table.reserve(&request("ws://h.example/")).unwrap();
    }
    let err = table.reserve(&request("ws://h.example/")).unwrap_err();
    assert_eq!(err.code, PluginErrorCode::LimitExceeded);
    assert_eq!(table.len(), MAX_SOCKETS_PER_INSTANCE as usize);
}

#[test]
fn pending_sockets_are_hidden_and_inert() {
    let mut table = SocketTable::new("s");
    let (id, _) = table.reserve(&request("ws://h.example/")).unwrap();
    assert!(table.list().is_empty());
    assert_eq!(
        table.inbound(&id, SocketFrame::Text("x".into())),
        Inbound::Ignore
    );
    assert!(table.outbound(&id, &SocketFrame::Text("x".into())).is_err());
    assert!(table.set_attachment(&id, vec![1]).is_err());
    assert_eq!(table.close_request(&id, 0, "").unwrap(), None);
    assert!(table.closed(&id, 1000, "", true).is_none());
    assert!(table.establish(&id, None).is_some());
    assert!(
        table.establish(&id, None).is_none(),
        "establish happens once"
    );
    assert!(!table.abandon(&id), "an open socket is not abandoned");
}

#[test]
fn heartbeats_are_answered_and_other_frames_delivered() {
    let (table, id) = open_one();
    assert_eq!(
        table.inbound(&id, SocketFrame::Text("2".into())),
        Inbound::Reply(SocketFrame::Text("3".into()))
    );
    // Same bytes as a binary frame is not the declared heartbeat.
    assert!(matches!(
        table.inbound(&id, SocketFrame::Binary(b"2".to_vec())),
        Inbound::Deliver(_)
    ));
    let Inbound::Deliver(message) = table.inbound(&id, SocketFrame::Text("42[\"x\"]".into()))
    else {
        panic!("expected a delivery");
    };
    assert_eq!(message.socket, id);
    assert_eq!(message.attachment, b"cursor-1");
}

#[test]
fn oversized_frames_close_the_socket() {
    let (table, id) = open_one();
    let big = vec![0_u8; MAX_SOCKET_FRAME_BYTES as usize + 1];
    assert_eq!(
        table.inbound(&id, SocketFrame::Binary(big)),
        Inbound::TooBig
    );
    let at_cap = vec![0_u8; MAX_SOCKET_FRAME_BYTES as usize];
    assert!(matches!(
        table.inbound(&id, SocketFrame::Binary(at_cap)),
        Inbound::Deliver(_)
    ));
}

#[test]
fn attachments_follow_the_socket_until_it_closes() {
    let (mut table, id) = open_one();
    table.set_attachment(&id, b"cursor-2".to_vec()).unwrap();
    assert!(table.set_attachment(&id, vec![0; 4096]).is_err());
    assert_eq!(table.list()[0].attachment, b"cursor-2");
    assert_eq!(table.list()[0].protocol.as_deref(), Some("chat"));
    let closed = table.closed(&id, 1001, "going away", true).unwrap();
    assert_eq!(closed.attachment, b"cursor-2");
    assert_eq!(closed.reason.as_deref(), Some("going away"));
    assert!(table.closed(&id, 1001, "", true).is_none(), "reported once");
    assert_eq!(
        table.inbound(&id, SocketFrame::Text("x".into())),
        Inbound::Ignore
    );
    assert!(table.is_empty());
}

#[test]
fn close_requests_check_code_and_tolerate_unknown_sockets() {
    let (table, id) = open_one();
    assert_eq!(table.close_request(&id, 0, "").unwrap(), Some(1000));
    assert_eq!(table.close_request("nope", 1000, "").unwrap(), None);
    assert!(table.close_request(&id, 1005, "").is_err());
    assert!(table.close_request(&id, 1000, &"r".repeat(200)).is_err());
}

#[test]
fn a_tls_stream_takes_binary_frames_only() {
    let mut table = SocketTable::new("s");
    let req = SocketConnectRequest {
        url: "tls://connect.example.com:443".into(),
        protocols: vec!["x-tunnel".into()],
        ..SocketConnectRequest::default()
    };
    let (id, target) = table.reserve(&req).unwrap();
    assert_eq!(target.scheme, SocketScheme::Tls);
    assert!(table.establish(&id, Some("x-tunnel".into())).is_some());
    let err = table
        .outbound(&id, &SocketFrame::Text("x".into()))
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::InvalidParams);
    assert!(table.outbound(&id, &SocketFrame::Binary(vec![1])).is_ok());
}
