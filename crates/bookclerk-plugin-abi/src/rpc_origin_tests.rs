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
