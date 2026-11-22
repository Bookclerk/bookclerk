//! Round trips of the webhook binding and handler over local capabilities.

use std::cell::RefCell;
use std::sync::Arc;

use super::*;
use crate::generated::WebhookHeader;
use crate::{PluginError, PluginErrorCode};

/// Host double that records what reached it and returns `deliveries`
/// deliveries per pull.
#[derive(Default)]
struct RecordingWebhooks {
    /// Calls in arrival order, as short strings.
    calls: RefCell<Vec<String>>,
    /// Deliveries each pull returns, whatever the batch size.
    deliveries: usize,
}

fn delivery(lease: &str) -> WebhookDelivery {
    WebhookDelivery {
        lease_id: lease.into(),
        received_at_unix_ms: 1_700_000_000_000,
        attempt: 1,
        headers: vec![WebhookHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        body: br#"{"event":"b2:ObjectCreated"}"#.to_vec(),
    }
}

#[async_trait::async_trait(?Send)]
impl Webhooks for RecordingWebhooks {
    async fn url(&self, name: String) -> Result<Option<String>> {
        self.calls.borrow_mut().push(format!("url {name}"));
        if name == "off" {
            return Ok(None);
        }
        Ok(Some(format!("https://library.example/hooks/t/{name}")))
    }

    async fn pull(&self, request: WebhookPullRequest) -> Result<WebhookPullPage> {
        self.calls.borrow_mut().push(format!(
            "pull {} {} {}",
            request.name, request.batch_size, request.visibility_timeout_ms
        ));
        Ok(WebhookPullPage {
            deliveries: (0..self.deliveries)
                .map(|i| delivery(&format!("l{i}")))
                .collect(),
            missed: true,
        })
    }

    async fn ack(&self, request: WebhookAckRequest) -> Result<()> {
        self.calls.borrow_mut().push(format!(
            "ack {} {:?} {:?}",
            request.name, request.acks, request.retries
        ));
        Err(PluginError::unavailable("store busy"))
    }
}

fn client_over(host: Arc<RecordingWebhooks>) -> WebhooksClient {
    WebhooksClient::new(capnp_rpc::new_client(WebhooksServer { inner: host }))
}

#[tokio::test(flavor = "current_thread")]
async fn guest_calls_reach_the_host_binding() {
    let host = Arc::new(RecordingWebhooks {
        deliveries: 2,
        ..RecordingWebhooks::default()
    });
    let client = client_over(Arc::clone(&host));
    assert_eq!(
        client.url("events".into()).await.expect("url").as_deref(),
        Some("https://library.example/hooks/t/events")
    );
    assert_eq!(client.url("off".into()).await.expect("url"), None);
    let page = client
        .pull(WebhookPullRequest {
            name: "events".into(),
            batch_size: 10,
            visibility_timeout_ms: 0,
        })
        .await
        .expect("pull");
    assert_eq!(page.deliveries, vec![delivery("l0"), delivery("l1")]);
    assert!(page.missed);
    let err = client
        .ack(WebhookAckRequest {
            name: "events".into(),
            acks: vec!["l0".into()],
            retries: vec!["l1".into()],
        })
        .await
        .expect_err("ack error passes through");
    assert_eq!(err.code, PluginErrorCode::Unavailable);
    assert_eq!(
        *host.calls.borrow(),
        vec![
            "url events".to_string(),
            "url off".to_string(),
            "pull events 10 0".to_string(),
            "ack events [\"l0\"] [\"l1\"]".to_string(),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_host_server_refuses_what_the_checks_refuse() {
    let host = Arc::new(RecordingWebhooks::default());
    // Call the server through a raw capability so the guest-side checks in
    // `WebhooksClient` are skipped and only the server's own checks apply.
    let raw: webhooks_capnp::Client = capnp_rpc::new_client(WebhooksServer {
        inner: Arc::clone(&host) as Arc<dyn Webhooks>,
    });
    let mut req = raw.url_request();
    req.get().set_name("../escape");
    let reply = req.send().promise.await.expect("rpc");
    let outcome = read_webhook_url_reply(reply.get().expect("reply").get_result().expect("result"))
        .expect("decode");
    assert_eq!(
        outcome.expect_err("refused").code,
        PluginErrorCode::InvalidParams
    );

    let mut req = raw.pull_request();
    {
        let mut r = req.get().init_request();
        r.set_name("events");
        r.set_batch_size(crate::limits::MAX_WEBHOOK_PULL_BATCH + 1);
    }
    let reply = req.send().promise.await.expect("rpc");
    let outcome =
        read_webhook_pull_reply(reply.get().expect("reply").get_result().expect("result"))
            .expect("decode");
    assert!(outcome.is_err());

    let mut req = raw.ack_request();
    {
        let mut r = req.get().init_request();
        r.set_name("events");
        r.reborrow().init_acks(1).set(0, "same");
        r.init_retries(1).set(0, "same");
    }
    let reply = req.send().promise.await.expect("rpc");
    assert!(read_unit(reply.get().expect("reply").get_result().expect("result")).is_err());
    assert!(host.calls.borrow().is_empty(), "nothing reached the host");
}

#[tokio::test(flavor = "current_thread")]
async fn an_oversized_page_never_reaches_the_guest() {
    let host = Arc::new(RecordingWebhooks {
        deliveries: 3,
        ..RecordingWebhooks::default()
    });
    let client = client_over(host);
    let err = client
        .pull(WebhookPullRequest {
            name: "events".into(),
            batch_size: 2,
            visibility_timeout_ms: 1_000,
        })
        .await
        .expect_err("more deliveries than asked for");
    assert_eq!(err.code, PluginErrorCode::InvalidParams);
}

/// Guest handler that records deliveries.
#[derive(Default)]
struct RecordingHandler {
    /// Webhook names and waiting counts, in arrival order.
    seen: RefCell<Vec<(String, u32)>>,
}

#[async_trait::async_trait(?Send)]
impl WebhookHandler for RecordingHandler {
    async fn delivered(&self, event: WebhookDelivered) -> Result<()> {
        self.seen.borrow_mut().push((event.name, event.waiting));
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn host_wakes_reach_the_guest_handler() {
    let guest = Arc::new(RecordingHandler::default());
    let client = WebhookHandlerClient::new(capnp_rpc::new_client(WebhookHandlerServer {
        inner: Arc::clone(&guest) as Arc<dyn WebhookHandler>,
    }));
    client
        .delivered(WebhookDelivered {
            name: "events".into(),
            waiting: 3,
        })
        .await
        .expect("delivered");
    assert!(client
        .delivered(WebhookDelivered {
            name: "Bad Name".into(),
            waiting: 1,
        })
        .await
        .is_err());
    assert_eq!(*guest.seen.borrow(), vec![("events".to_string(), 3)]);
}

#[test]
fn the_wire_round_trip_keeps_each_codec() {
    let mut message = capnp::message::Builder::new_default();
    write_webhook_delivery(message.init_root(), &delivery("l0")).expect("write");
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize");
    assert_eq!(
        roundtrip_webhook_message(WebhookCodec::Delivery, &bytes).expect("decode"),
        WebhookWireMessage::Delivery(delivery("l0"))
    );
    let ack = WebhookAckRequest {
        name: "events".into(),
        acks: vec!["a".into()],
        retries: Vec::new(),
    };
    let mut message = capnp::message::Builder::new_default();
    write_webhook_ack_request(message.init_root(), &ack).expect("write");
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize");
    assert_eq!(
        roundtrip_webhook_message(WebhookCodec::AckRequest, &bytes).expect("decode"),
        WebhookWireMessage::AckRequest(ack)
    );
    assert!(roundtrip_webhook_message(WebhookCodec::PullRequest, b"short").is_err());
}
