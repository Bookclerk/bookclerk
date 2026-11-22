//! Cap'n Proto glue for host-served inbound webhooks: the `Webhooks`
//! binding the host serves and the guest calls, and the `WebhookHandler`
//! entrypoint the guest serves and the host calls.
//!
//! Both directions apply the checks in [`crate::webhooks`] before anything
//! crosses the wire, so a guest cannot hand the host a malformed pull or
//! ack, and the host never hands a guest an oversized delivery.

use std::rc::Rc;
use std::sync::Arc;

use super::from_capnp;
use super::sockets_glue::{read_unit, write_unit};
use crate::generated::RoundTripFailure;
use crate::generated::{
    read_webhook_ack_request, read_webhook_delivered, read_webhook_delivery,
    read_webhook_pull_reply, read_webhook_pull_request, read_webhook_url_reply,
    write_webhook_ack_request, write_webhook_delivered, write_webhook_delivery,
    write_webhook_pull_reply, write_webhook_pull_request, write_webhook_url_reply,
    WebhookAckRequest, WebhookDelivered, WebhookDelivery, WebhookPullPage, WebhookPullRequest,
    WebhookUrl,
};
use crate::plugin_capnp::{webhook_handler as webhook_handler_capnp, webhooks as webhooks_capnp};
use crate::webhooks::{
    check_webhook_ack, check_webhook_delivered, check_webhook_name, check_webhook_page,
    check_webhook_pull,
};
use crate::{Result, WebhookHandler, Webhooks};

/// Host-side server for the [`Webhooks`] binding handed to a guest on
/// `Bindings.webhooks`. Every request is checked here before it reaches
/// the host's store, and every page before it leaves.
pub(super) struct WebhooksServer {
    /// Host implementation (the delivery store).
    pub(super) inner: Arc<dyn Webhooks>,
}

impl webhooks_capnp::Server for WebhooksServer {
    async fn url(
        self: Rc<Self>,
        params: webhooks_capnp::UrlParams,
        mut results: webhooks_capnp::UrlResults,
    ) -> capnp::Result<()> {
        let name = super::text_of(params.get()?.get_name()?);
        let outcome = match check_webhook_name(&name) {
            Ok(()) => self.inner.url(name).await.map(|url| WebhookUrl { url }),
            Err(err) => Err(err),
        };
        write_webhook_url_reply(results.get().init_result(), &outcome)
    }

    async fn pull(
        self: Rc<Self>,
        params: webhooks_capnp::PullParams,
        mut results: webhooks_capnp::PullResults,
    ) -> capnp::Result<()> {
        let request = read_webhook_pull_request(params.get()?.get_request()?)?;
        let outcome = match check_webhook_pull(&request) {
            Ok(pull) => self.inner.pull(request).await.and_then(|page| {
                check_webhook_page(&page, pull.batch_size)?;
                Ok(page)
            }),
            Err(err) => Err(err),
        };
        write_webhook_pull_reply(results.get().init_result(), &outcome)
    }

    async fn ack(
        self: Rc<Self>,
        params: webhooks_capnp::AckParams,
        mut results: webhooks_capnp::AckResults,
    ) -> capnp::Result<()> {
        let request = read_webhook_ack_request(params.get()?.get_request()?)?;
        let outcome = match check_webhook_ack(&request) {
            Ok(()) => self.inner.ack(request).await,
            Err(err) => Err(err),
        };
        write_unit(results.get().init_result(), outcome);
        Ok(())
    }
}

/// Guest-side client for the host [`Webhooks`] binding.
#[derive(Clone)]
pub struct WebhooksClient {
    /// The host's `Webhooks` capability.
    client: webhooks_capnp::Client,
}

impl WebhooksClient {
    /// Wraps a `Webhooks` capability client.
    #[must_use]
    pub fn new(client: webhooks_capnp::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait(?Send)]
impl Webhooks for WebhooksClient {
    async fn url(&self, name: String) -> Result<Option<String>> {
        check_webhook_name(&name)?;
        let mut req = self.client.url_request();
        req.get().set_name(&name);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_webhook_url_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|url| url.url.filter(|url| !url.is_empty()))
    }

    async fn pull(&self, request: WebhookPullRequest) -> Result<WebhookPullPage> {
        let pull = check_webhook_pull(&request)?;
        let mut req = self.client.pull_request();
        write_webhook_pull_request(req.get().init_request(), &request).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let page = read_webhook_pull_reply(reply_result!(reply)).map_err(from_capnp)??;
        check_webhook_page(&page, pull.batch_size)?;
        Ok(page)
    }

    async fn ack(&self, request: WebhookAckRequest) -> Result<()> {
        check_webhook_ack(&request)?;
        let mut req = self.client.ack_request();
        write_webhook_ack_request(req.get().init_request(), &request).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_unit(reply_result!(reply))
    }
}

/// Serves a guest's [`WebhookHandler`] as the Cap'n Proto `WebhookHandler`.
pub(super) struct WebhookHandlerServer {
    /// The guest implementation every call forwards to.
    pub(super) inner: Arc<dyn WebhookHandler>,
}

impl webhook_handler_capnp::Server for WebhookHandlerServer {
    async fn delivered(
        self: Rc<Self>,
        params: webhook_handler_capnp::DeliveredParams,
        mut results: webhook_handler_capnp::DeliveredResults,
    ) -> capnp::Result<()> {
        let event = read_webhook_delivered(params.get()?.get_event()?)?;
        let outcome = match check_webhook_delivered(&event) {
            Ok(()) => self.inner.delivered(event).await,
            Err(err) => Err(err),
        };
        write_unit(results.get().init_result(), outcome);
        Ok(())
    }
}

/// Host-side client for a guest's [`WebhookHandler`].
#[derive(Clone)]
pub struct WebhookHandlerClient {
    /// The guest's `WebhookHandler` capability.
    client: webhook_handler_capnp::Client,
}

impl WebhookHandlerClient {
    /// Wraps a `WebhookHandler` capability client.
    #[must_use]
    pub fn new(client: webhook_handler_capnp::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait(?Send)]
impl WebhookHandler for WebhookHandlerClient {
    async fn delivered(&self, event: WebhookDelivered) -> Result<()> {
        check_webhook_delivered(&event)?;
        let mut req = self.client.delivered_request();
        write_webhook_delivered(req.get().init_event(), &event).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_unit(reply_result!(reply))
    }
}

/// Which webhook Cap'n Proto struct [`roundtrip_webhook_message`] decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookCodec {
    /// `WebhookPullRequest`, which the host decodes from a guest.
    PullRequest,
    /// `WebhookAckRequest`, which the host decodes from a guest.
    AckRequest,
    /// `WebhookDelivery`, which a guest decodes from the host.
    Delivery,
}

/// A value [`roundtrip_webhook_message`] decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookWireMessage {
    /// A decoded `WebhookPullRequest`.
    PullRequest(WebhookPullRequest),
    /// A decoded `WebhookAckRequest`.
    AckRequest(WebhookAckRequest),
    /// A decoded `WebhookDelivery`.
    Delivery(WebhookDelivery),
}

/// Decodes the root of `$reader` with `$read`, writes it back with
/// `$write`, reads it again and checks the two decoded values match;
/// evaluates to the decoded value or a [`RoundTripFailure`].
macro_rules! roundtrip {
    ($reader:expr, $what:literal, $read:ident, $write:ident) => {{
        let codec_failure = |step: &str, err: &dyn std::fmt::Display| {
            RoundTripFailure::Codec(format!(concat!($what, " {} failed: {}"), step, err))
        };
        let first = $read($reader.get_root()?)?;
        let mut message = capnp::message::Builder::new_default();
        $write(message.init_root(), &first).map_err(|err| codec_failure("re-encode", &err))?;
        let again = $read(
            message
                .get_root_as_reader()
                .map_err(|err| codec_failure("re-read", &err))?,
        )
        .map_err(|err| codec_failure("re-decode", &err))?;
        if first != again {
            return Err(RoundTripFailure::Codec(
                concat!("round trip changed the decoded ", $what).to_string(),
            ));
        }
        first
    }};
}

/// Decodes one unpacked single-segment webhook message with the codec the
/// RPC server and client use, then checks `read(write(v)) == v`. The
/// `webhook_rpc` fuzz target and its replay test drive this, so they reach
/// the same decoder a peer's bytes do; the caller then applies the
/// [`crate::webhooks`] checks to the value.
///
/// # Arguments
///
/// * `codec` - Which struct the message holds.
/// * `bytes` - The message, as `capnp::serialize` writes it.
///
/// # Returns
///
/// The decoded value.
///
/// # Errors
///
/// [`RoundTripFailure::Rejected`] when the bytes do not decode;
/// [`RoundTripFailure::Codec`] when a decoded value does not survive `write`
/// then `read` intact.
pub fn roundtrip_webhook_message(
    codec: WebhookCodec,
    bytes: &[u8],
) -> std::result::Result<WebhookWireMessage, RoundTripFailure> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)?;
    Ok(match codec {
        WebhookCodec::PullRequest => WebhookWireMessage::PullRequest(roundtrip!(
            reader,
            "WebhookPullRequest",
            read_webhook_pull_request,
            write_webhook_pull_request
        )),
        WebhookCodec::AckRequest => WebhookWireMessage::AckRequest(roundtrip!(
            reader,
            "WebhookAckRequest",
            read_webhook_ack_request,
            write_webhook_ack_request
        )),
        WebhookCodec::Delivery => WebhookWireMessage::Delivery(roundtrip!(
            reader,
            "WebhookDelivery",
            read_webhook_delivery,
            write_webhook_delivery
        )),
    })
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
#[path = "rpc_webhooks_tests.rs"]
mod tests;
