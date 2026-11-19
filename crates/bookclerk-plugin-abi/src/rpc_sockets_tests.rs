//! Round trips of the socket binding and handler over local capabilities.

use std::cell::RefCell;
use std::sync::Arc;

use super::*;
use crate::{PluginError, PluginErrorCode};

/// Host double that records what reached it.
#[derive(Default)]
struct RecordingSockets {
    /// Calls in arrival order, as short strings.
    calls: RefCell<Vec<String>>,
}

#[async_trait::async_trait(?Send)]
impl Sockets for RecordingSockets {
    async fn connect(&self, request: SocketConnectRequest) -> Result<SocketInfo> {
        self.calls
            .borrow_mut()
            .push(format!("connect {}", request.url));
        Ok(SocketInfo {
            socket: "s-1".into(),
            url: request.url,
            protocol: request.protocols.first().cloned(),
            attachment: request.attachment,
        })
    }

    async fn send(&self, socket: String, frame: SocketFrame) -> Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("send {socket} {frame:?}"));
        Ok(())
    }

    async fn close(&self, socket: String, code: u16, reason: String) -> Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("close {socket} {code} {reason}"));
        Ok(())
    }

    async fn set_attachment(&self, socket: String, attachment: Vec<u8>) -> Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("attach {socket} {}", attachment.len()));
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SocketInfo>> {
        Err(PluginError::unavailable("broker restarting"))
    }
}

fn client_over(host: Arc<RecordingSockets>) -> SocketsClient {
    SocketsClient::new(capnp_rpc::new_client(SocketsServer { inner: host }))
}

#[tokio::test(flavor = "current_thread")]
async fn guest_calls_reach_the_host_binding() {
    let host = Arc::new(RecordingSockets::default());
    let client = client_over(Arc::clone(&host));
    let info = client
        .connect(SocketConnectRequest {
            url: "wss://abs.example/socket.io/".into(),
            protocols: vec!["chat".into()],
            attachment: b"cursor".to_vec(),
            ..SocketConnectRequest::default()
        })
        .await
        .expect("connect");
    assert_eq!(info.socket, "s-1");
    assert_eq!(info.protocol.as_deref(), Some("chat"));
    assert_eq!(info.attachment, b"cursor");
    client
        .send("s-1".into(), SocketFrame::Text("40".into()))
        .await
        .expect("send");
    client
        .set_attachment("s-1".into(), vec![1, 2, 3])
        .await
        .expect("attach");
    client
        .close("s-1".into(), 0, "done".into())
        .await
        .expect("close");
    let err = client.list().await.expect_err("list error passes through");
    assert_eq!(err.code, PluginErrorCode::Unavailable);
    assert_eq!(
        *host.calls.borrow(),
        vec![
            "connect wss://abs.example/socket.io/".to_string(),
            "send s-1 Text(\"40\")".to_string(),
            "attach s-1 3".to_string(),
            "close s-1 1000 done".to_string(),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_host_server_refuses_what_the_checks_refuse() {
    let host = Arc::new(RecordingSockets::default());
    // Call the server through a raw capability so the guest-side checks in
    // `SocketsClient` are skipped and only the server's own checks apply.
    let raw: sockets_capnp::Client = capnp_rpc::new_client(SocketsServer {
        inner: Arc::clone(&host) as Arc<dyn Sockets>,
    });
    let mut req = raw.connect_request();
    req.get()
        .init_request()
        .set_url("https://not-a-socket.example");
    let reply = req.send().promise.await.expect("rpc");
    let outcome =
        read_socket_connect_reply(reply.get().expect("reply").get_result().expect("result"))
            .expect("decode");
    assert_eq!(
        outcome.expect_err("refused").code,
        PluginErrorCode::InvalidParams
    );

    let mut req = raw.send_request();
    req.get().set_socket("../escape");
    req.get().init_frame().set_text("x");
    let reply = req.send().promise.await.expect("rpc");
    let err =
        read_unit(reply.get().expect("reply").get_result().expect("result")).expect_err("bad id");
    assert_eq!(err.code, PluginErrorCode::InvalidParams);

    let mut req = raw.close_request();
    req.get().set_socket("s-1");
    req.get().set_code(1006);
    let reply = req.send().promise.await.expect("rpc");
    assert!(read_unit(reply.get().expect("reply").get_result().expect("result")).is_err());
    assert!(host.calls.borrow().is_empty(), "nothing reached the host");
}

/// Guest handler that records deliveries.
#[derive(Default)]
struct RecordingHandler {
    /// Deliveries in arrival order.
    seen: RefCell<Vec<String>>,
}

#[async_trait::async_trait(?Send)]
impl SocketHandler for RecordingHandler {
    async fn message(&self, event: SocketMessage) -> Result<SocketMessageOk> {
        let poll_events = event.frame == SocketFrame::Text("poll".into());
        self.seen
            .borrow_mut()
            .push(format!("message {} {:?}", event.socket, event.frame));
        Ok(SocketMessageOk { poll_events })
    }

    async fn closed(&self, event: SocketClosed) -> Result<()> {
        self.seen.borrow_mut().push(format!(
            "closed {} {} {}",
            event.socket, event.code, event.clean
        ));
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn host_deliveries_reach_the_guest_handler() {
    let guest = Arc::new(RecordingHandler::default());
    let client = SocketHandlerClient::new(capnp_rpc::new_client(SocketHandlerServer {
        inner: Arc::clone(&guest) as Arc<dyn SocketHandler>,
    }));
    let quiet = client
        .message(SocketMessage {
            socket: "s-1".into(),
            frame: SocketFrame::Binary(vec![9]),
            attachment: Vec::new(),
        })
        .await
        .expect("message");
    assert!(!quiet.poll_events);
    let asks = client
        .message(SocketMessage {
            socket: "s-1".into(),
            frame: SocketFrame::Text("poll".into()),
            attachment: Vec::new(),
        })
        .await
        .expect("message");
    assert!(asks.poll_events, "the guest's ask reaches the host");
    client
        .closed(SocketClosed {
            socket: "s-1".into(),
            code: 1006,
            reason: None,
            clean: false,
            attachment: Vec::new(),
        })
        .await
        .expect("closed");
    let err = client
        .message(SocketMessage {
            socket: "s-1".into(),
            frame: SocketFrame::Text(String::new()),
            attachment: vec![0; 4096],
        })
        .await
        .expect_err("oversized attachment");
    assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
    assert_eq!(
        *guest.seen.borrow(),
        vec![
            "message s-1 Binary([9])".to_string(),
            "message s-1 Text(\"poll\")".to_string(),
            "closed s-1 1006 false".to_string(),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_handler_that_ignores_messages_says_so() {
    struct Bare;
    impl SocketHandler for Bare {}
    let client = SocketHandlerClient::new(capnp_rpc::new_client(SocketHandlerServer {
        inner: Arc::new(Bare),
    }));
    let err = client
        .message(SocketMessage {
            socket: "s-1".into(),
            ..SocketMessage::default()
        })
        .await
        .expect_err("unsupported");
    assert_eq!(err.code, PluginErrorCode::Unsupported);
    client
        .closed(SocketClosed {
            socket: "s-1".into(),
            ..SocketClosed::default()
        })
        .await
        .expect("closed is ignored by default");
}

#[tokio::test(flavor = "current_thread")]
async fn the_guest_client_refuses_before_sending() {
    let host = Arc::new(RecordingSockets::default());
    let client = client_over(Arc::clone(&host));
    let bad_url = client
        .connect(SocketConnectRequest {
            url: "ws://127.1/".into(),
            ..SocketConnectRequest::default()
        })
        .await
        .expect_err("non-canonical host");
    assert_eq!(bad_url.code, PluginErrorCode::InvalidParams);
    let bad_id = client
        .send("a/b".into(), SocketFrame::Text("x".into()))
        .await
        .expect_err("bad id");
    assert_eq!(bad_id.code, PluginErrorCode::InvalidParams);
    let big = client
        .send("s-1".into(), SocketFrame::Binary(vec![0; 262_145]))
        .await
        .expect_err("oversized frame");
    assert_eq!(big.code, PluginErrorCode::PayloadTooLarge);
    let heavy = client
        .set_attachment("s-1".into(), vec![0; 2049])
        .await
        .expect_err("oversized attachment");
    assert_eq!(heavy.code, PluginErrorCode::PayloadTooLarge);
    let code = client
        .close("s-1".into(), 1015, String::new())
        .await
        .expect_err("reserved code");
    assert_eq!(code.code, PluginErrorCode::InvalidParams);
    assert!(host.calls.borrow().is_empty(), "nothing reached the host");
}

#[tokio::test(flavor = "current_thread")]
async fn the_host_server_refuses_oversized_attachments() {
    let host = Arc::new(RecordingSockets::default());
    let raw: sockets_capnp::Client = capnp_rpc::new_client(SocketsServer {
        inner: Arc::clone(&host) as Arc<dyn Sockets>,
    });
    let mut req = raw.set_attachment_request();
    req.get().set_socket("s-1");
    req.get().set_attachment(&[0; 2049]);
    let reply = req.send().promise.await.expect("rpc");
    let err = read_unit(reply.get().expect("reply").get_result().expect("result"))
        .expect_err("oversized");
    assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
    assert!(host.calls.borrow().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn listing_returns_the_hosts_sockets() {
    struct Listing;
    #[async_trait::async_trait(?Send)]
    impl Sockets for Listing {
        async fn connect(&self, _request: SocketConnectRequest) -> Result<SocketInfo> {
            Err(PluginError::unsupported("connect"))
        }
        async fn send(&self, _socket: String, _frame: SocketFrame) -> Result<()> {
            Err(PluginError::unsupported("send"))
        }
        async fn close(&self, _socket: String, _code: u16, _reason: String) -> Result<()> {
            Err(PluginError::unsupported("close"))
        }
        async fn set_attachment(&self, _socket: String, _attachment: Vec<u8>) -> Result<()> {
            Err(PluginError::unsupported("set_attachment"))
        }
        async fn list(&self) -> Result<Vec<SocketInfo>> {
            Ok(vec![SocketInfo {
                socket: "s-2".into(),
                url: "ws://10.0.0.5:13378/".into(),
                protocol: None,
                attachment: b"a".to_vec(),
            }])
        }
    }
    let client = SocketsClient::new(capnp_rpc::new_client(SocketsServer {
        inner: Arc::new(Listing),
    }));
    let sockets = client.list().await.expect("list");
    assert_eq!(sockets.len(), 1);
    assert_eq!(sockets[0].socket, "s-2");
    assert_eq!(sockets[0].protocol, None);
    let err = client
        .send("s-2".into(), SocketFrame::Text("x".into()))
        .await
        .expect_err("host error passes through");
    assert_eq!(err.code, PluginErrorCode::Unsupported);
}

#[tokio::test(flavor = "current_thread")]
async fn the_guest_server_refuses_bad_deliveries() {
    let guest = Arc::new(RecordingHandler::default());
    // A raw capability skips the host client's checks, so only the guest
    // server's own checks apply.
    let raw: socket_handler_capnp::Client = capnp_rpc::new_client(SocketHandlerServer {
        inner: Arc::clone(&guest) as Arc<dyn SocketHandler>,
    });
    let mut req = raw.message_request();
    {
        let mut event = req.get().init_event();
        event.set_socket("../s");
        event.init_frame().set_text("x");
    }
    let reply = req.send().promise.await.expect("rpc");
    let err = read_socket_message_reply(reply.get().expect("reply").get_result().expect("result"))
        .expect("decodes")
        .expect_err("bad id");
    assert_eq!(err.code, PluginErrorCode::InvalidParams);

    let mut req = raw.closed_request();
    {
        let mut event = req.get().init_event();
        event.set_socket("s-1");
        event.set_attachment(&[0; 2049]);
    }
    let reply = req.send().promise.await.expect("rpc");
    let err = read_unit(reply.get().expect("reply").get_result().expect("result"))
        .expect_err("oversized attachment");
    assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
    assert!(guest.seen.borrow().is_empty(), "nothing reached the guest");

    let host_side = SocketHandlerClient::new(raw);
    let err = host_side
        .closed(SocketClosed {
            socket: String::new(),
            ..SocketClosed::default()
        })
        .await
        .expect_err("the host client checks the id too");
    assert_eq!(err.code, PluginErrorCode::InvalidParams);
}

#[test]
fn socket_messages_round_trip_through_the_codec() {
    let request = SocketConnectRequest {
        url: "wss://abs.example/".into(),
        protocols: vec!["chat".into()],
        attachment: vec![1],
        ..SocketConnectRequest::default()
    };
    let mut message = capnp::message::Builder::new_default();
    write_socket_connect_request(message.init_root(), &request).expect("encode");
    let bytes = capnp::serialize::write_message_to_words(&message);
    assert_eq!(
        roundtrip_socket_message(SocketCodec::ConnectRequest, &bytes).expect("decode"),
        SocketWireMessage::ConnectRequest(request)
    );

    let event = SocketMessage {
        socket: "s-1".into(),
        frame: SocketFrame::Binary(vec![7]),
        attachment: Vec::new(),
    };
    let mut message = capnp::message::Builder::new_default();
    write_socket_message(message.init_root(), &event).expect("encode");
    let bytes = capnp::serialize::write_message_to_words(&message);
    assert_eq!(
        roundtrip_socket_message(SocketCodec::Message, &bytes).expect("decode"),
        SocketWireMessage::Message(event)
    );

    assert!(matches!(
        roundtrip_socket_message(SocketCodec::Message, &[1, 2, 3]),
        Err(RoundTripFailure::Rejected(_))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn host_and_broker_serve_each_other_over_one_link() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (host_end, broker_end) = tokio::io::duplex(64 * 1024);
            let broker_sockets = Arc::new(RecordingSockets::default());
            let handler_slot: Rc<RefCell<Option<SocketHandlerClient>>> = Rc::default();
            let (broker_read, broker_write) = tokio::io::split(broker_end);
            let served = Arc::clone(&broker_sockets);
            let slot = Rc::clone(&handler_slot);
            let broker = tokio::task::spawn_local(serve_socket_broker(
                broker_read,
                broker_write,
                move |handler| {
                    *slot.borrow_mut() = Some(handler);
                    served as Arc<dyn Sockets>
                },
            ));

            let host_handler = Arc::new(RecordingHandler::default());
            let (host_read, host_write) = tokio::io::split(host_end);
            let (sockets, rpc) = connect_socket_broker(
                host_read,
                host_write,
                Arc::clone(&host_handler) as Arc<dyn SocketHandler>,
            );
            let rpc = tokio::task::spawn_local(rpc);

            // Host to broker: a guest's call forwarded through the link.
            let info = sockets
                .connect(SocketConnectRequest {
                    url: "wss://abs.example.com/socket.io/".into(),
                    ..SocketConnectRequest::default()
                })
                .await
                .expect("connect through the link");
            assert_eq!(info.socket, "s-1");
            assert_eq!(
                *broker_sockets.calls.borrow(),
                vec!["connect wss://abs.example.com/socket.io/".to_string()]
            );

            // Broker to host: a delivery through the host's bootstrap.
            let handler = handler_slot
                .borrow()
                .clone()
                .expect("broker got the handler");
            handler
                .message(SocketMessage {
                    socket: "s-1".into(),
                    frame: SocketFrame::Text("42".into()),
                    attachment: Vec::new(),
                })
                .await
                .expect("deliver through the link");
            assert_eq!(
                *host_handler.seen.borrow(),
                vec!["message s-1 Text(\"42\")".to_string()]
            );

            drop((sockets, handler));
            rpc.abort();
            let _ = broker.await;
        })
        .await;
}
