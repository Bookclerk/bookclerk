//! Cap'n Proto glue for host-brokered sockets: the `Sockets` binding the
//! host serves and the guest calls, and the `SocketHandler` entrypoint the
//! guest serves and the host calls.
//!
//! Both directions apply the checks in [`crate::sockets`] before anything
//! crosses the wire, so a guest cannot push an oversized frame through the
//! host and the host never hands a guest one.

use std::rc::Rc;
use std::sync::Arc;

use super::{empty_reply, from_capnp, read_error, text_of, write_error};
use crate::generated::RoundTripFailure;
use crate::generated::{
    read_socket_closed, read_socket_connect_reply, read_socket_connect_request, read_socket_frame,
    read_socket_list_reply, read_socket_message, read_socket_message_reply, write_socket_closed,
    write_socket_connect_reply, write_socket_connect_request, write_socket_frame,
    write_socket_list_reply, write_socket_message, write_socket_message_reply, SocketClosed,
    SocketConnectRequest, SocketFrame, SocketInfo, SocketList, SocketMessage, SocketMessageOk,
};
use crate::plugin_capnp::{socket_handler as socket_handler_capnp, sockets as sockets_capnp};
use crate::sockets::{
    check_socket_attachment, check_socket_close, check_socket_frame, check_socket_id,
    validate_socket_connect,
};
use crate::{PluginError, Result, SocketHandler, Sockets};

/// Checks a frame and attachment the host is about to deliver to the guest.
///
/// # Errors
///
/// The first failing id, frame or attachment check.
fn check_delivery(socket: &str, frame: Option<&SocketFrame>, attachment: &[u8]) -> Result<()> {
    check_socket_id(socket)?;
    if let Some(frame) = frame {
        check_socket_frame(frame)?;
    }
    check_socket_attachment(attachment)
}

/// Writes a unit outcome into an `EmptyReply` builder.
fn write_unit(mut result: crate::plugin_capnp::empty_reply::Builder<'_>, outcome: Result<()>) {
    match outcome {
        Ok(()) => result.set_ok(()),
        Err(err) => write_error(result.init_err(), &err),
    }
}

/// Reads an `EmptyReply` into a unit outcome.
///
/// # Errors
///
/// The error the reply carried, or the decode error.
fn read_unit(result: crate::plugin_capnp::empty_reply::Reader<'_>) -> Result<()> {
    match result.which().map_err(from_capnp)? {
        empty_reply::Ok(()) => Ok(()),
        empty_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

/// Host-side server for the [`Sockets`] binding handed to a guest on
/// `Bindings.sockets`. Every request is checked here before it reaches the
/// broker.
pub(super) struct SocketsServer {
    /// Host implementation (the broker link).
    pub(super) inner: Arc<dyn Sockets>,
}

impl sockets_capnp::Server for SocketsServer {
    async fn connect(
        self: Rc<Self>,
        params: sockets_capnp::ConnectParams,
        mut results: sockets_capnp::ConnectResults,
    ) -> capnp::Result<()> {
        let request = read_socket_connect_request(params.get()?.get_request()?)?;
        let outcome = match validate_socket_connect(&request) {
            Ok(_) => self.inner.connect(request).await,
            Err(err) => Err(err),
        };
        write_socket_connect_reply(results.get().init_result(), &outcome)
    }

    async fn send(
        self: Rc<Self>,
        params: sockets_capnp::SendParams,
        mut results: sockets_capnp::SendResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let socket = text_of(p.get_socket()?);
        let frame = read_socket_frame(p.get_frame()?)?;
        let outcome = match check_socket_id(&socket).and_then(|()| check_socket_frame(&frame)) {
            Ok(()) => self.inner.send(socket, frame).await,
            Err(err) => Err(err),
        };
        write_unit(results.get().init_result(), outcome);
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        params: sockets_capnp::CloseParams,
        mut results: sockets_capnp::CloseResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let socket = text_of(p.get_socket()?);
        let reason = text_of(p.get_reason()?);
        let outcome = match check_socket_id(&socket)
            .and_then(|()| check_socket_close(p.get_code(), &reason))
        {
            Ok(code) => self.inner.close(socket, code, reason).await,
            Err(err) => Err(err),
        };
        write_unit(results.get().init_result(), outcome);
        Ok(())
    }

    async fn set_attachment(
        self: Rc<Self>,
        params: sockets_capnp::SetAttachmentParams,
        mut results: sockets_capnp::SetAttachmentResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let socket = text_of(p.get_socket()?);
        let attachment = p.get_attachment()?.to_vec();
        let outcome =
            match check_socket_id(&socket).and_then(|()| check_socket_attachment(&attachment)) {
                Ok(()) => self.inner.set_attachment(socket, attachment).await,
                Err(err) => Err(err),
            };
        write_unit(results.get().init_result(), outcome);
        Ok(())
    }

    async fn list(
        self: Rc<Self>,
        _params: sockets_capnp::ListParams,
        mut results: sockets_capnp::ListResults,
    ) -> capnp::Result<()> {
        let outcome = self
            .inner
            .list()
            .await
            .map(|sockets| SocketList { sockets });
        write_socket_list_reply(results.get().init_result(), &outcome)
    }
}

/// Guest-side client for the host [`Sockets`] binding.
#[derive(Clone)]
pub struct SocketsClient {
    /// The host's `Sockets` capability.
    client: sockets_capnp::Client,
}

impl SocketsClient {
    /// Wraps a `Sockets` capability client.
    #[must_use]
    pub fn new(client: sockets_capnp::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait(?Send)]
impl Sockets for SocketsClient {
    async fn connect(&self, request: SocketConnectRequest) -> Result<SocketInfo> {
        validate_socket_connect(&request)?;
        let mut req = self.client.connect_request();
        write_socket_connect_request(req.get().init_request(), &request).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_socket_connect_reply(reply_result!(reply)).map_err(from_capnp)?
    }

    async fn send(&self, socket: String, frame: SocketFrame) -> Result<()> {
        check_socket_id(&socket)?;
        check_socket_frame(&frame)?;
        let mut req = self.client.send_request();
        {
            let mut p = req.get();
            p.set_socket(&socket);
            write_socket_frame(p.init_frame(), &frame).map_err(from_capnp)?;
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_unit(reply_result!(reply))
    }

    async fn close(&self, socket: String, code: u16, reason: String) -> Result<()> {
        check_socket_id(&socket)?;
        let code = check_socket_close(code, &reason)?;
        let mut req = self.client.close_request();
        {
            let mut p = req.get();
            p.set_socket(&socket);
            p.set_code(code);
            p.set_reason(&reason);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_unit(reply_result!(reply))
    }

    async fn set_attachment(&self, socket: String, attachment: Vec<u8>) -> Result<()> {
        check_socket_id(&socket)?;
        check_socket_attachment(&attachment)?;
        let mut req = self.client.set_attachment_request();
        {
            let mut p = req.get();
            p.set_socket(&socket);
            p.set_attachment(&attachment);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_unit(reply_result!(reply))
    }

    async fn list(&self) -> Result<Vec<SocketInfo>> {
        let req = self.client.list_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_socket_list_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|list| list.sockets)
    }
}

/// Serves a guest's [`SocketHandler`] as the Cap'n Proto `SocketHandler`.
pub(super) struct SocketHandlerServer {
    /// The guest implementation every method forwards to.
    pub(super) inner: Arc<dyn SocketHandler>,
}

impl socket_handler_capnp::Server for SocketHandlerServer {
    async fn message(
        self: Rc<Self>,
        params: socket_handler_capnp::MessageParams,
        mut results: socket_handler_capnp::MessageResults,
    ) -> capnp::Result<()> {
        let event = read_socket_message(params.get()?.get_event()?)?;
        let outcome = match check_delivery(&event.socket, Some(&event.frame), &event.attachment) {
            Ok(()) => self.inner.message(event).await,
            Err(err) => Err(err),
        };
        write_socket_message_reply(results.get().init_result(), &outcome)
    }

    async fn closed(
        self: Rc<Self>,
        params: socket_handler_capnp::ClosedParams,
        mut results: socket_handler_capnp::ClosedResults,
    ) -> capnp::Result<()> {
        let event = read_socket_closed(params.get()?.get_event()?)?;
        let outcome = match check_delivery(&event.socket, None, &event.attachment) {
            Ok(()) => self.inner.closed(event).await,
            Err(err) => Err(err),
        };
        write_unit(results.get().init_result(), outcome);
        Ok(())
    }
}

/// Host-side client for a guest's [`SocketHandler`].
#[derive(Clone)]
pub struct SocketHandlerClient {
    /// The guest's `SocketHandler` capability.
    client: socket_handler_capnp::Client,
}

impl SocketHandlerClient {
    /// Wraps a `SocketHandler` capability client.
    #[must_use]
    pub fn new(client: socket_handler_capnp::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait(?Send)]
impl SocketHandler for SocketHandlerClient {
    async fn message(&self, event: SocketMessage) -> Result<SocketMessageOk> {
        check_delivery(&event.socket, Some(&event.frame), &event.attachment)?;
        let mut req = self.client.message_request();
        write_socket_message(req.get().init_event(), &event).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_socket_message_reply(reply_result!(reply)).map_err(from_capnp)?
    }

    async fn closed(&self, event: SocketClosed) -> Result<()> {
        check_delivery(&event.socket, None, &event.attachment)?;
        let mut req = self.client.closed_request();
        write_socket_closed(req.get().init_event(), &event).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_unit(reply_result!(reply))
    }
}

/// Which socket Cap'n Proto struct [`roundtrip_socket_message`] decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketCodec {
    /// `SocketConnectRequest`, which the host decodes from a guest.
    ConnectRequest,
    /// `SocketMessage`, which a guest decodes from the host (and the host
    /// decodes from the broker before delivery).
    Message,
}

/// A value [`roundtrip_socket_message`] decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketWireMessage {
    /// A decoded `SocketConnectRequest`.
    ConnectRequest(SocketConnectRequest),
    /// A decoded `SocketMessage`.
    Message(SocketMessage),
}

/// Decodes one unpacked single-segment `SocketConnectRequest` or
/// `SocketMessage` with the codec the RPC server and client use, then checks
/// `read(write(v)) == v`. The `socket_request` fuzz target and its replay
/// test drive this, so they reach the same decoder a peer's bytes do; the
/// caller then applies [`crate::sockets`] checks to the value.
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
pub fn roundtrip_socket_message(
    codec: SocketCodec,
    bytes: &[u8],
) -> std::result::Result<SocketWireMessage, RoundTripFailure> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)?;
    let codec_failure = |what: &str, err: &dyn std::fmt::Display| {
        RoundTripFailure::Codec(format!("{what} failed: {err}"))
    };
    let mut message = capnp::message::Builder::new_default();
    match codec {
        SocketCodec::ConnectRequest => {
            let first = read_socket_connect_request(reader.get_root()?)?;
            write_socket_connect_request(message.init_root(), &first)
                .map_err(|err| codec_failure("re-encode", &err))?;
            let again = read_socket_connect_request(
                message
                    .get_root_as_reader()
                    .map_err(|err| codec_failure("re-read", &err))?,
            )
            .map_err(|err| codec_failure("re-decode", &err))?;
            if first != again {
                return Err(RoundTripFailure::Codec(
                    "round trip changed the decoded SocketConnectRequest".to_string(),
                ));
            }
            Ok(SocketWireMessage::ConnectRequest(first))
        }
        SocketCodec::Message => {
            let first = read_socket_message(reader.get_root()?)?;
            write_socket_message(message.init_root(), &first)
                .map_err(|err| codec_failure("re-encode", &err))?;
            let again = read_socket_message(
                message
                    .get_root_as_reader()
                    .map_err(|err| codec_failure("re-read", &err))?,
            )
            .map_err(|err| codec_failure("re-decode", &err))?;
            if first != again {
                return Err(RoundTripFailure::Codec(
                    "round trip changed the decoded SocketMessage".to_string(),
                ));
            }
            Ok(SocketWireMessage::Message(first))
        }
    }
}

/// Serves the socket broker's side of the host-to-broker link.
///
/// The broker is a separate jailed process that holds a plugin instance's
/// sockets while its guest sleeps. On this one connection each side serves
/// the other: the broker's bootstrap capability is `Sockets` (the host
/// forwards a guest's calls to it), and the host's is `SocketHandler` (the
/// broker delivers messages and closes through it). The broker is the
/// two-party server side; [`connect_socket_broker`] is the host's half.
///
/// Must run inside a `tokio::task::LocalSet`. Returns when the host hangs
/// up.
///
/// # Arguments
///
/// * `reader` / `writer` - The link to the host.
/// * `make` - Builds the broker's [`Sockets`] from the host's handler.
///
/// # Errors
///
/// Returns an internal error when the link fails.
pub async fn serve_socket_broker<R, W>(
    reader: R,
    writer: W,
    make: impl FnOnce(SocketHandlerClient) -> Arc<dyn Sockets>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + 'static,
    W: tokio::io::AsyncWrite + Unpin + 'static,
{
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    let network = capnp_rpc::twoparty::VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    // The broker's capability is created after the host's handler is known,
    // so the bootstrap is a local promise resolved right below.
    let (fulfil, resolved) = tokio::sync::oneshot::channel::<sockets_capnp::Client>();
    let sockets_client: sockets_capnp::Client = capnp_rpc::new_future_client(async move {
        resolved
            .await
            .map_err(|_| capnp::Error::failed("the socket broker did not start".to_string()))
    });
    let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), Some(sockets_client.client));
    let host: socket_handler_capnp::Client =
        rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Client);
    let inner = make(SocketHandlerClient::new(host));
    let served: sockets_capnp::Client = capnp_rpc::new_client(SocketsServer { inner });
    let _ = fulfil.send(served);
    rpc_system
        .await
        .map_err(|err| PluginError::internal(err.to_string()))
}

/// Connects the host's side of the host-to-broker link (see
/// [`serve_socket_broker`]).
///
/// Must run inside a `tokio::task::LocalSet`. The returned [`capnp_rpc::RpcSystem`] must
/// be spawned with `tokio::task::spawn_local`; the link lasts as long as it
/// runs.
///
/// # Arguments
///
/// * `reader` / `writer` - The link to the broker.
/// * `handler` - Where the broker delivers messages and closes.
///
/// # Returns
///
/// The broker's [`Sockets`] and the link's RPC system.
pub fn connect_socket_broker<R, W>(
    reader: R,
    writer: W,
    handler: Arc<dyn SocketHandler>,
) -> (
    SocketsClient,
    capnp_rpc::RpcSystem<capnp_rpc::rpc_twoparty_capnp::Side>,
)
where
    R: tokio::io::AsyncRead + Unpin + 'static,
    W: tokio::io::AsyncWrite + Unpin + 'static,
{
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    let network = capnp_rpc::twoparty::VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        capnp_rpc::rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let served: socket_handler_capnp::Client =
        capnp_rpc::new_client(SocketHandlerServer { inner: handler });
    let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), Some(served.client));
    let broker: sockets_capnp::Client =
        rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
    (SocketsClient::new(broker), rpc_system)
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
#[path = "rpc_sockets_tests.rs"]
mod tests;
