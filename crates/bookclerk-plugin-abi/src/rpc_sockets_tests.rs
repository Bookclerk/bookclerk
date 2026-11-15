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
    async fn message(&self, event: SocketMessage) -> Result<()> {
        self.seen
            .borrow_mut()
            .push(format!("message {} {:?}", event.socket, event.frame));
        Ok(())
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
    client
        .message(SocketMessage {
            socket: "s-1".into(),
            frame: SocketFrame::Binary(vec![9]),
            attachment: Vec::new(),
        })
        .await
        .expect("message");
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
