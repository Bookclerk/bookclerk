//! Broker tests against a local WebSocket server.

use super::*;
use bookclerk_plugin_abi::{PluginErrorCode, SocketAutoResponse, SocketHeader, SocketMessageOk};
use bookclerk_plugin_manifest::NetworkMode;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

/// What the host handler double saw.
#[derive(Default)]
struct Seen {
    /// Messages and closes in arrival order.
    events: RefCell<Vec<String>>,
    /// Wakes a test waiting for the next event.
    notify: tokio::sync::Notify,
}

/// Host handler double.
struct Recorder(Rc<Seen>);

#[async_trait::async_trait(?Send)]
impl SocketHandler for Recorder {
    async fn message(&self, event: SocketMessage) -> Result<SocketMessageOk> {
        self.0.events.borrow_mut().push(format!(
            "message {:?} {}",
            event.frame,
            String::from_utf8_lossy(&event.attachment)
        ));
        self.0.notify.notify_one();
        Ok(SocketMessageOk::default())
    }

    async fn closed(&self, event: SocketClosed) -> Result<()> {
        self.0.events.borrow_mut().push(format!(
            "closed {} {} {}",
            event.code,
            event.reason.unwrap_or_default(),
            event.clean
        ));
        self.0.notify.notify_one();
        Ok(())
    }
}

/// Waits until the handler has seen `count` events.
async fn wait_for(seen: &Seen, count: usize) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while seen.events.borrow().len() < count {
            seen.notify.notified().await;
        }
    })
    .await
    .expect("events in time");
    seen.events.borrow().clone()
}

/// A policy that allows loopback by address.
fn loopback_policy() -> EgressPolicy {
    EgressPolicy {
        mode: NetworkMode::Outbound,
        domains: vec!["127.0.0.1".into()],
        address_cidrs: vec!["127.0.0.0/8".into()],
        ..EgressPolicy::deny()
    }
}

/// A broker over `policy` and a fresh recorder.
fn new_broker(policy: EgressPolicy) -> (SocketBroker, Rc<Seen>) {
    let seen = Rc::new(Seen::default());
    let handler: Rc<dyn SocketHandler> = Rc::new(Recorder(Rc::clone(&seen)));
    let broker = SocketBroker::new(policy, default_tls().expect("tls"), handler, "t-");
    (broker, seen)
}

/// What the test server saw from the broker.
#[derive(Debug, Default)]
struct ServerLog {
    /// Request headers of the upgrade, lowercased names.
    headers: Vec<(String, String)>,
    /// Messages received, in order.
    received: Vec<String>,
    /// Close code received, if any.
    close: Option<u16>,
}

/// Starts a one-connection WebSocket server that runs `script` on the
/// socket; returns its port and the log it fills.
async fn serve_once<F, Fut>(
    protocol: Option<&'static str>,
    script: F,
) -> (u16, tokio::task::JoinHandle<ServerLog>)
where
    F: FnOnce(WebSocketStream<TcpStream>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ServerLog> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let headers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = Arc::clone(&headers);
        // tungstenite's handshake callback fixes the large error type.
        #[allow(clippy::result_large_err)]
        let callback = move |request: &Request, mut response: Response| {
            let mut seen = record.lock().expect("lock");
            for (name, value) in request.headers() {
                seen.push((
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                ));
            }
            if let Some(protocol) = protocol {
                response
                    .headers_mut()
                    .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(protocol));
            }
            Ok(response)
        };
        let socket = tokio_tungstenite::accept_hdr_async(tcp, callback)
            .await
            .expect("server handshake");
        let mut log = script(socket).await;
        log.headers = headers.lock().expect("lock").clone();
        log
    });
    (port, task)
}

/// Reads until the next text or close, recording it.
async fn read_one(socket: &mut WebSocketStream<TcpStream>, log: &mut ServerLog) -> bool {
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(text) => {
                log.received.push(text.as_str().to_string());
                return true;
            }
            Message::Close(frame) => {
                log.close = frame.map(|f| u16::from(f.code));
                return false;
            }
            _ => {}
        }
    }
    false
}

/// A connect request for the local server.
fn request(port: u16) -> SocketConnectRequest {
    SocketConnectRequest {
        url: format!("ws://127.0.0.1:{port}/socket.io/?EIO=4&transport=websocket"),
        protocols: vec!["chat".into()],
        headers: vec![
            SocketHeader {
                name: "Authorization".into(),
                value: "Bearer token".into(),
            },
            SocketHeader {
                name: "User-Agent".into(),
                value: "guest-agent".into(),
            },
        ],
        auto_responses: vec![SocketAutoResponse {
            request: SocketFrame::Text("2".into()),
            response: SocketFrame::Text("3".into()),
        }],
        attachment: b"cursor".to_vec(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn heartbeats_stay_in_the_broker_and_messages_reach_the_host() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (port, server) = serve_once(Some("chat"), |mut socket| async move {
                let mut log = ServerLog::default();
                socket.send(Message::text("2")).await.expect("ping");
                assert!(read_one(&mut socket, &mut log).await, "heartbeat answer");
                socket
                    .send(Message::text("42[\"item\"]"))
                    .await
                    .expect("event");
                assert!(read_one(&mut socket, &mut log).await, "guest send");
                assert!(!read_one(&mut socket, &mut log).await, "guest close");
                // Keep polling so the close reply goes out.
                while socket.next().await.is_some() {}
                log
            })
            .await;
            let (broker, seen) = new_broker(loopback_policy());
            let info = broker.connect(request(port)).await.expect("connect");
            assert_eq!(info.protocol.as_deref(), Some("chat"));
            assert_eq!(info.attachment, b"cursor");
            assert!(info.socket.starts_with("t-"));

            let events = wait_for(&seen, 1).await;
            assert_eq!(events, vec!["message Text(\"42[\\\"item\\\"]\") cursor"]);

            broker
                .set_attachment(info.socket.clone(), b"cursor-2".to_vec())
                .await
                .expect("attach");
            broker
                .send(info.socket.clone(), SocketFrame::Text("40".into()))
                .await
                .expect("send");
            broker
                .close(info.socket.clone(), 0, "bye".into())
                .await
                .expect("close");
            let events = wait_for(&seen, 2).await;
            assert_eq!(events[1], "closed 1000 bye true");
            assert!(broker.list().await.expect("list").is_empty());

            let log = server.await.expect("server");
            assert_eq!(log.received, vec!["3".to_string(), "40".to_string()]);
            assert_eq!(log.close, Some(1000));
            let header = |name: &str| {
                log.headers
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            };
            assert_eq!(header("authorization").as_deref(), Some("Bearer token"));
            assert_eq!(header("sec-websocket-protocol").as_deref(), Some("chat"));
            assert_eq!(
                header("user-agent"),
                Some(loopback_policy().user_agent_for("127.0.0.1", Some("guest-agent")))
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_remote_close_reaches_the_host_once_with_the_attachment() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (port, server) = serve_once(None, |mut socket| async move {
                socket
                    .close(Some(CloseFrame {
                        code: CloseCode::Away,
                        reason: "restart".into(),
                    }))
                    .await
                    .expect("close");
                // The broker answers the close before reporting it clean.
                let mut log = ServerLog::default();
                while let Some(Ok(message)) = socket.next().await {
                    if let Message::Close(frame) = message {
                        log.close = frame.map(|f| u16::from(f.code));
                    }
                }
                log
            })
            .await;
            let (broker, seen) = new_broker(loopback_policy());
            let mut req = request(port);
            req.protocols.clear();
            let info = broker.connect(req).await.expect("connect");
            assert_eq!(info.protocol, None);
            let events = wait_for(&seen, 1).await;
            assert_eq!(events, vec!["closed 1001 restart true"]);
            assert!(broker.list().await.expect("list").is_empty());
            let err = broker
                .send(info.socket, SocketFrame::Text("late".into()))
                .await
                .expect_err("closed socket");
            assert_eq!(err.code, PluginErrorCode::NotFound);
            let log = server.await.expect("server");
            assert_eq!(log.close, Some(1001), "the close was answered");
        })
        .await;
}

/// Host handler double that, on its first message, sends `replies` frames
/// back through the broker before returning.
struct Replier {
    /// The broker to send through, set once it exists.
    broker: RefCell<Option<Rc<SocketBroker>>>,
    /// Frames to send from the first message.
    replies: usize,
    /// What it saw.
    seen: Rc<Seen>,
}

#[async_trait::async_trait(?Send)]
impl SocketHandler for Replier {
    async fn message(&self, event: SocketMessage) -> Result<SocketMessageOk> {
        let first = self.seen.events.borrow().is_empty();
        self.seen.events.borrow_mut().push("message".into());
        self.seen.notify.notify_one();
        let broker = self.broker.borrow().clone();
        if let (true, Some(broker)) = (first, broker) {
            // Send only once the broker found this socket's delivery queue
            // full behind this call.
            tokio::time::timeout(Duration::from_secs(10), async {
                while broker.shared.queue_full.get() == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|_| PluginError::internal("the delivery queue never filled"))?;
            for n in 0..self.replies {
                broker
                    .send(event.socket.clone(), SocketFrame::Text(format!("r{n}")))
                    .await?;
            }
        }
        Ok(SocketMessageOk::default())
    }

    async fn closed(&self, _event: SocketClosed) -> Result<()> {
        self.seen.events.borrow_mut().push("closed".into());
        self.seen.notify.notify_one();
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_handler_that_sends_while_both_queues_are_full_still_progresses() {
    const MESSAGES: usize = DELIVERY_QUEUE + 8;
    const REPLIES: usize = COMMAND_QUEUE + 4;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (port, server) = serve_once(None, |mut socket| async move {
                for n in 0..MESSAGES {
                    socket
                        .send(Message::text(format!("m{n}")))
                        .await
                        .expect("message");
                }
                let mut log = ServerLog::default();
                while log.received.len() < REPLIES {
                    if !read_one(&mut socket, &mut log).await {
                        break;
                    }
                }
                socket.close(None).await.expect("close");
                while socket.next().await.is_some() {}
                log
            })
            .await;
            let seen = Rc::new(Seen::default());
            let replier = Rc::new(Replier {
                broker: RefCell::new(None),
                replies: REPLIES,
                seen: Rc::clone(&seen),
            });
            let handler: Rc<dyn SocketHandler> = replier.clone();
            let broker = Rc::new(SocketBroker::new(
                loopback_policy(),
                default_tls().expect("tls"),
                handler,
                "t-",
            ));
            *replier.broker.borrow_mut() = Some(Rc::clone(&broker));
            let mut req = request(port);
            req.protocols.clear();
            req.auto_responses.clear();
            broker.connect(req).await.expect("connect");
            let events = wait_for(&seen, MESSAGES + 1).await;
            assert_eq!(events.last().map(String::as_str), Some("closed"));
            let log = server.await.expect("server");
            assert_eq!(log.received.len(), REPLIES);
            *replier.broker.borrow_mut() = None;
        })
        .await;
}

/// Host handler double whose `message` waits until the test opens it.
struct Stuck {
    /// Opens held messages.
    gate: Rc<tokio::sync::Notify>,
    /// What it saw.
    seen: Rc<Seen>,
}

#[async_trait::async_trait(?Send)]
impl SocketHandler for Stuck {
    async fn message(&self, _event: SocketMessage) -> Result<SocketMessageOk> {
        self.seen.events.borrow_mut().push("message".into());
        self.seen.notify.notify_one();
        self.gate.notified().await;
        Ok(SocketMessageOk::default())
    }

    async fn closed(&self, _event: SocketClosed) -> Result<()> {
        self.seen.events.borrow_mut().push("closed".into());
        self.seen.notify.notify_one();
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_closed_socket_counts_against_the_limit_until_its_deliveries_finish() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (port, server) = serve_once(None, |mut socket| async move {
                socket.send(Message::text("one")).await.expect("message");
                socket.close(None).await.expect("close");
                while socket.next().await.is_some() {}
                ServerLog::default()
            })
            .await;
            let seen = Rc::new(Seen::default());
            let gate = Rc::new(tokio::sync::Notify::new());
            let handler: Rc<dyn SocketHandler> = Rc::new(Stuck {
                gate: Rc::clone(&gate),
                seen: Rc::clone(&seen),
            });
            let broker = SocketBroker::new(
                loopback_policy(),
                default_tls().expect("tls"),
                handler,
                "t-",
            );
            let mut req = request(port);
            req.protocols.clear();
            broker.connect(req).await.expect("connect");
            wait_for(&seen, 1).await;
            server.await.expect("server");
            // The transport ended, but the close still waits behind the
            // stuck message.
            tokio::time::timeout(Duration::from_secs(10), async {
                while !broker.shared.table.borrow().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("slot freed");
            assert_eq!(broker.shared.lingering.get(), 1);
            broker
                .shared
                .lingering
                .set(MAX_SOCKETS_PER_INSTANCE as usize);
            let err = broker.connect(request(1)).await.expect_err("limit");
            assert_eq!(err.code, PluginErrorCode::LimitExceeded);
            broker.shared.lingering.set(1);
            gate.notify_one();
            wait_for(&seen, 2).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while broker.shared.lingering.get() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("delivery task finished");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_connect_frees_its_slot() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Accepts TCP but never answers the upgrade.
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let hold = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.expect("accept");
                tokio::time::sleep(Duration::from_secs(5)).await;
                drop(tcp);
            });
            let (broker, _seen) = new_broker(loopback_policy());
            let cancelled =
                tokio::time::timeout(Duration::from_millis(300), broker.connect(request(port)))
                    .await;
            assert!(cancelled.is_err(), "the connect was still dialing");
            assert!(broker.shared.table.borrow().is_empty());
            hold.abort();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_messages_close_the_socket_with_1009() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (port, server) = serve_once(None, |mut socket| async move {
                let big = vec![7_u8; MAX_SOCKET_FRAME_BYTES as usize + 1];
                let _ = socket.send(Message::binary(big)).await;
                let mut log = ServerLog::default();
                let _ = read_one(&mut socket, &mut log).await;
                log
            })
            .await;
            let (broker, seen) = new_broker(loopback_policy());
            let mut req = request(port);
            req.protocols.clear();
            broker.connect(req).await.expect("connect");
            let events = wait_for(&seen, 1).await;
            assert_eq!(events, vec!["closed 1009 message too big false"]);
            let log = server.await.expect("server");
            // The broker sends a 1009 close, then drops the connection with
            // the rest of the oversized message unread. Windows resets such
            // a connection, which can discard the close frame before the
            // server reads it; any close the server does see is 1009.
            assert!(
                matches!(log.close, None | Some(1009)),
                "close {:?}",
                log.close
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_subprotocol_the_guest_did_not_ask_for_fails_the_connect() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (port, _server) =
                serve_once(Some("other"), |_socket| async move { ServerLog::default() }).await;
            let (broker, _seen) = new_broker(loopback_policy());
            let err = broker
                .connect(request(port))
                .await
                .expect_err("bad protocol");
            assert_eq!(err.code, PluginErrorCode::Unavailable);
            assert!(broker.list().await.expect("list").is_empty());
            assert!(broker.shared.table.borrow().is_empty(), "no pending leak");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_network_grant_decides_every_dial() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Host not on the grant.
            let policy = EgressPolicy {
                domains: vec!["abs.example.com".into()],
                ..loopback_policy()
            };
            let (broker, _seen) = new_broker(policy);
            let err = broker.connect(request(9)).await.expect_err("not granted");
            assert_eq!(err.code, PluginErrorCode::Forbidden);

            // Loopback named, but no address grant for it.
            let policy = EgressPolicy {
                address_cidrs: Vec::new(),
                ..loopback_policy()
            };
            let (broker, _seen) = new_broker(policy);
            let err = broker.connect(request(9)).await.expect_err("no cidr");
            assert_eq!(err.code, PluginErrorCode::Forbidden);

            // A TCP grant for the host and port covers the socket too.
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            drop(listener);
            let policy = EgressPolicy {
                domains: Vec::new(),
                tcp: vec![bookclerk_plugin_manifest::TcpGrant {
                    host: "127.0.0.1".into(),
                    ports: vec![port],
                }],
                ..loopback_policy()
            };
            assert!(target_granted(
                &policy,
                &SocketTarget {
                    secure: false,
                    host: "127.0.0.1".into(),
                    port,
                }
            ));
            assert!(!target_granted(
                &policy,
                &SocketTarget {
                    secure: false,
                    host: "127.0.0.1".into(),
                    port: port.wrapping_add(1),
                }
            ));
            let (broker, _seen) = new_broker(policy);
            let err = broker
                .connect(request(port))
                .await
                .expect_err("closed port");
            assert_eq!(
                err.code,
                PluginErrorCode::Unavailable,
                "granted, then refused by the OS"
            );

            // Deny mode refuses everything.
            let (broker, _seen) = new_broker(EgressPolicy::deny());
            let err = broker.connect(request(9)).await.expect_err("deny");
            assert_eq!(err.code, PluginErrorCode::Forbidden);
            assert!(broker.shared.table.borrow().is_empty(), "no pending leak");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_connection_frees_its_slot() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Bind then drop, so the port is very likely closed.
            let port = {
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                listener.local_addr().expect("addr").port()
            };
            let (broker, _seen) = new_broker(loopback_policy());
            let err = broker.connect(request(port)).await.expect_err("refused");
            assert_eq!(err.code, PluginErrorCode::Unavailable);
            assert!(broker.shared.table.borrow().is_empty());
            // Unknown sockets: close is fine, send and attach are not.
            broker
                .close("t-99".into(), 0, String::new())
                .await
                .expect("close");
            assert!(broker
                .send("t-99".into(), SocketFrame::Text("x".into()))
                .await
                .is_err());
            assert!(broker
                .set_attachment("t-99".into(), Vec::new())
                .await
                .is_err());
        })
        .await;
}
