//! Round trips through the `Origin` and `Ingress` glue.

use std::cell::RefCell;

use tokio::io::AsyncReadExt;

use super::*;

/// An `Origin` that answers with what it was asked, and the request body.
struct Echo {
    /// The last checked request it saw.
    seen: RefCell<Option<CheckedOriginRequest>>,
    /// Status and extra header to answer with.
    answer: (u16, OriginHeader),
}

#[async_trait::async_trait(?Send)]
impl Origin for Echo {
    async fn fetch(
        &self,
        request: CheckedOriginRequest,
        body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    ) -> Result<OriginReply> {
        *self.seen.borrow_mut() = Some(request);
        Ok(OriginReply {
            status: self.answer.0,
            headers: vec![self.answer.1.clone()],
            body,
        })
    }
}

/// A header field.
fn header(name: &str, value: &str) -> OriginHeader {
    OriginHeader {
        name: name.into(),
        value: value.into(),
    }
}

/// A client wired straight to an [`OriginServer`] around `inner`.
fn client(inner: Arc<Echo>) -> OriginClient {
    OriginClient::new(capnp_rpc::new_client(OriginServer { inner, window: 4 }), 4)
}

/// A checked request for `target`.
fn checked(target: &str, headers: Vec<OriginHeader>) -> CheckedOriginRequest {
    CheckedOriginRequest {
        method: "PUT".into(),
        target: target.into(),
        headers,
        client: Some("198.51.100.7".parse().unwrap()),
    }
}

/// Runs `test` on a local set, as the capnp clients need.
fn local<F: std::future::Future<Output = ()>>(test: F) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, test);
}

#[test]
fn a_request_and_its_body_cross_both_ways() {
    local(async {
        let echo = Arc::new(Echo {
            seen: RefCell::new(None),
            answer: (201, header("Set-Cookie", "a=b")),
        });
        let origin = client(Arc::clone(&echo));
        let body: Pin<Box<dyn AsyncRead + Send>> = Box::pin(&b"part of an audio file"[..]);
        let reply = origin
            .fetch(
                checked(
                    "/api/node/v1/uploads/u/files/0",
                    vec![header("Content-Range", "bytes 0-9/20")],
                ),
                Some(body),
            )
            .await
            .unwrap();
        assert_eq!(reply.status, 201);
        assert_eq!(reply.headers, vec![header("Set-Cookie", "a=b")]);
        let mut echoed = Vec::new();
        reply.body.unwrap().read_to_end(&mut echoed).await.unwrap();
        assert_eq!(echoed, b"part of an audio file");
        let seen = echo.seen.borrow().clone().unwrap();
        assert_eq!(seen.target, "/api/node/v1/uploads/u/files/0");
        assert_eq!(seen.client, Some("198.51.100.7".parse().unwrap()));
    });
}

#[test]
fn a_bad_head_never_reaches_the_library() {
    local(async {
        let echo = Arc::new(Echo {
            seen: RefCell::new(None),
            answer: (200, header("X", "1")),
        });
        let origin = client(Arc::clone(&echo));
        let refused = origin
            .fetch(checked("http://evil.example/", Vec::new()), None)
            .await;
        assert!(refused.is_err());
        assert!(echo.seen.borrow().is_none());
    });
}

#[test]
fn a_bad_response_head_is_refused() {
    local(async {
        let echo = Arc::new(Echo {
            seen: RefCell::new(None),
            answer: (200, header("Transfer-Encoding", "chunked")),
        });
        let refused = client(echo).fetch(checked("/", Vec::new()), None).await;
        assert!(refused.is_err());
    });
}

/// An ingress plugin that reports `origin`.
struct Provider {
    /// What `start` reports.
    origin: String,
}

#[async_trait::async_trait(?Send)]
impl Ingress for Provider {
    async fn start(&self) -> Result<IngressStarted> {
        Ok(IngressStarted {
            origin: self.origin.clone(),
        })
    }

    async fn health(&self) -> Result<IngressHealth> {
        Ok(IngressHealth {
            serving: false,
            detail: Some("the relay is not reachable".into()),
        })
    }

    async fn stop(&self) -> Result<()> {
        Err(PluginError::unavailable("stop failed"))
    }
}

/// A host client wired to an [`IngressServer`] reporting `origin`.
fn ingress(origin: &str) -> IngressClient {
    IngressClient::new(capnp_rpc::new_client(IngressServer {
        inner: Arc::new(Provider {
            origin: origin.into(),
        }),
    }))
}

#[test]
fn the_host_checks_the_origin_a_provider_reports() {
    local(async {
        let good = ingress("https://library.example.workers.dev");
        assert_eq!(
            good.start_checked().await.unwrap().as_str(),
            "https://library.example.workers.dev"
        );
        let health = good.health().await.unwrap();
        assert!(!health.serving);
        assert_eq!(health.detail.as_deref(), Some("the relay is not reachable"));
        assert!(good.stop().await.is_err());
        assert!(ingress("http://library.example")
            .start_checked()
            .await
            .is_err());
        assert!(ingress("https://127.0.0.1").start_checked().await.is_err());
    });
}

#[test]
fn no_origin_refuses() {
    local(async {
        assert!(NoOrigin
            .fetch(checked("/", Vec::new()), None)
            .await
            .is_err());
    });
}

/// An `Origin` that upgrades `/socket` and answers anything else with the
/// refusal it holds.
struct Upgrader {
    /// The last request and attachment it saw.
    seen: RefCell<Option<(CheckedOriginRequest, Vec<u8>)>>,
    /// What it answers for a target other than `/socket`.
    refusal: crate::generated::OriginRefusal,
}

#[async_trait::async_trait(?Send)]
impl Origin for Upgrader {
    async fn fetch(
        &self,
        _request: CheckedOriginRequest,
        _body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    ) -> Result<OriginReply> {
        Err(PluginError::internal("not used"))
    }

    async fn connect(
        &self,
        request: CheckedOriginRequest,
        attachment: Vec<u8>,
    ) -> Result<OriginConnected> {
        let upgrade = request.target == "/socket";
        *self.seen.borrow_mut() = Some((request, attachment.clone()));
        Ok(if upgrade {
            OriginConnected::Socket(crate::generated::SocketInfo {
                socket: "o1-a".into(),
                url: "https://library.example/socket".into(),
                protocol: Some("bookclerk.v1".into()),
                attachment,
            })
        } else {
            OriginConnected::Refused(self.refusal.clone())
        })
    }
}

/// A client wired to an [`OriginServer`] around an [`Upgrader`].
fn upgrader(refusal_status: u16) -> (Arc<Upgrader>, OriginClient) {
    let inner = Arc::new(Upgrader {
        seen: RefCell::new(None),
        refusal: crate::generated::OriginRefusal {
            status: refusal_status,
            headers: vec![header("Content-Type", "text/plain")],
            body: b"sign in first".to_vec(),
        },
    });
    let origin = OriginClient::new(
        capnp_rpc::new_client(OriginServer {
            inner: Arc::clone(&inner) as Arc<dyn Origin>,
            window: 4,
        }),
        4,
    );
    (inner, origin)
}

/// A checked upgrade request for `target`.
fn upgrade(target: &str, headers: Vec<OriginHeader>) -> CheckedOriginRequest {
    CheckedOriginRequest {
        method: "GET".into(),
        ..checked(target, headers)
    }
}

#[test]
fn an_accepted_upgrade_answers_its_socket() {
    local(async {
        let (inner, origin) = upgrader(401);
        let connected = origin
            .connect(
                upgrade(
                    "/socket",
                    vec![
                        header("Sec-WebSocket-Protocol", "bookclerk.v1"),
                        header("Sec-WebSocket-Key", "guest-chosen"),
                    ],
                ),
                b"pair:7".to_vec(),
            )
            .await
            .unwrap();
        let OriginConnected::Socket(info) = connected else {
            panic!("expected a socket, got {connected:?}");
        };
        assert_eq!(info.socket, "o1-a");
        assert_eq!(info.protocol.as_deref(), Some("bookclerk.v1"));
        assert_eq!(info.attachment, b"pair:7");
        let (seen, attachment) = inner.seen.borrow_mut().take().unwrap();
        // The host writes its own handshake fields.
        assert_eq!(
            seen.headers,
            vec![header("Sec-WebSocket-Protocol", "bookclerk.v1")]
        );
        assert_eq!(seen.client, Some("198.51.100.7".parse().unwrap()));
        assert_eq!(attachment, b"pair:7");
    });
}

#[test]
fn a_refused_upgrade_answers_the_library_response() {
    local(async {
        let (_, origin) = upgrader(401);
        let connected = origin
            .connect(upgrade("/elsewhere", Vec::new()), Vec::new())
            .await
            .unwrap();
        let OriginConnected::Refused(refusal) = connected else {
            panic!("expected a refusal, got {connected:?}");
        };
        assert_eq!(refusal.status, 401);
        assert_eq!(refusal.body, b"sign in first");
    });
}

#[test]
fn bad_upgrades_and_bad_refusals_are_refused() {
    local(async {
        let (inner, origin) = upgrader(401);
        let post = checked("/socket", Vec::new());
        assert_eq!(
            origin.connect(post, Vec::new()).await.unwrap_err().code,
            crate::PluginErrorCode::InvalidParams
        );
        let big = vec![0u8; MAX_SOCKET_ATTACHMENT_BYTES as usize + 1];
        assert!(origin
            .connect(upgrade("/socket", Vec::new()), big)
            .await
            .unwrap_err()
            .message
            .contains("attachment"));
        assert!(inner.seen.borrow().is_none(), "nothing reached the library");

        let (_, origin) = upgrader(101);
        assert!(origin
            .connect(upgrade("/elsewhere", Vec::new()), Vec::new())
            .await
            .unwrap_err()
            .message
            .contains("101"));
    });
}

#[test]
fn an_origin_without_upgrades_says_so() {
    local(async {
        let echo = Arc::new(Echo {
            seen: RefCell::new(None),
            answer: (200, header("X", "y")),
        });
        let err = client(echo)
            .connect(upgrade("/socket", Vec::new()), Vec::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, crate::PluginErrorCode::Unavailable);
    });
}
