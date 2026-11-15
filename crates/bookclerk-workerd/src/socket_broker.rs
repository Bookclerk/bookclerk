//! The socket broker: holds a plugin instance's WebSockets while its guest
//! sleeps.
//!
//! The host starts `bookclerk-workerd` in broker mode
//! ([`SOCKET_BROKER_ENV`]) inside the plugin's jail, under the plugin's
//! network grant. The broker serves the plugin ABI's `Sockets` interface to
//! the host on stdio and delivers what arrives through the host's
//! `SocketHandler` (see `bookclerk_plugin_abi::serve_socket_broker`), in the
//! manner of hibernatable Workers WebSockets:
//!
//! - every dial passes the egress policy: the URL's host must be on a
//!   `fetch` domain grant or a TCP grant for that host and port
//!   ([`target_granted`]), and the broker connects only to resolved
//!   addresses the policy allows, with no redirects;
//! - `wss://` uses rustls on the hosts' one provider with Mozilla's roots;
//! - heartbeat frames the guest declared at connect are answered here and
//!   never wake the guest ([`SocketTable::inbound`]);
//! - other messages queue per socket and reach the host one at a time; the
//!   broker stops reading a socket whose queue is full, and keeps answering
//!   heartbeats otherwise while a delivery waits for the guest to wake;
//! - a socket's close reaches the host after its last message, once.
//!
//! The decisions live in [`crate::socket_table`], which the fuzz target
//! drives without I/O.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_plugin_abi::sockets::SocketTarget;
use bookclerk_plugin_abi::{
    PluginError, Result, SocketClosed, SocketConnectRequest, SocketFrame, SocketHandler,
    SocketInfo, SocketMessage, Sockets, MAX_SOCKET_FRAME_BYTES,
};
use bookclerk_plugin_manifest::EgressPolicy;
use futures::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::header::{
    HeaderName, HeaderValue, SEC_WEBSOCKET_PROTOCOL, USER_AGENT,
};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

use crate::socket_table::{Inbound, SocketTable, CLOSE_TOO_BIG};

/// Environment variable that starts `bookclerk-workerd` as the plugin's
/// socket broker (value `1`) instead of its front door.
pub const SOCKET_BROKER_ENV: &str = "BOOKCLERK_SOCKET_BROKER";

/// Longest the broker spends resolving and connecting TCP for one dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest the TLS and WebSocket handshakes may take together.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest the broker waits for the peer's close frame after sending its
/// own.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages that may wait for the guest on one socket before the broker
/// stops reading it.
const DELIVERY_QUEUE: usize = 32;

/// Guest sends that may wait for one socket's writer.
const COMMAND_QUEUE: usize = 16;

/// Close code reported when the connection dropped without a close frame.
const CLOSE_ABNORMAL: u16 = 1006;

/// Close code reported when the peer's close frame carried no code.
const CLOSE_NO_STATUS: u16 = 1005;

/// Close code reported when the broker itself goes away.
const CLOSE_GOING_AWAY: u16 = 1001;

/// A byte stream the broker runs a WebSocket over (plain TCP or TLS).
trait BrokerIo: AsyncRead + AsyncWrite + Unpin {}

impl<T: AsyncRead + AsyncWrite + Unpin> BrokerIo for T {}

/// An open WebSocket as the broker holds it.
type Socket = WebSocketStream<Box<dyn BrokerIo>>;

/// What the guest asked one socket's task to do.
enum Command {
    /// Send this frame.
    Send(SocketFrame),
    /// Close with this code and reason.
    Close(u16, String),
}

/// What one socket's reader hands its delivery task, in order.
enum Delivery {
    /// A message for the guest.
    Message(SocketMessage),
    /// The socket ended; the last item.
    Closed(SocketClosed),
}

/// State every socket task and every guest call shares.
struct Shared {
    /// What the broker knows about each socket.
    table: RefCell<SocketTable>,
    /// Command queue of each open socket's task.
    links: RefCell<HashMap<String, mpsc::Sender<Command>>>,
    /// The plugin's effective egress policy (manifest narrowed by the
    /// operator's grant).
    policy: EgressPolicy,
    /// TLS settings for `wss://`.
    tls: Arc<ClientConfig>,
    /// The host's handler.
    handler: Rc<dyn SocketHandler>,
}

/// The broker's [`Sockets`] for one plugin instance.
pub struct SocketBroker {
    /// State shared with the socket tasks.
    shared: Rc<Shared>,
}

/// TLS settings with Mozilla's roots on the hosts' one rustls provider.
///
/// # Errors
///
/// Returns an internal error when the provider offers no safe protocol
/// version (it always does).
pub fn default_tls() -> Result<Arc<ClientConfig>> {
    let mut config = ClientConfig::builder_with_provider(bookclerk_tls::crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|err| PluginError::internal(format!("TLS settings: {err}")))?
        .with_root_certificates(RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        })
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

impl SocketBroker {
    /// A broker with no sockets.
    ///
    /// Must be used inside a `tokio::task::LocalSet`: each socket runs on
    /// local tasks because the host's handler is a Cap'n Proto capability.
    ///
    /// # Arguments
    ///
    /// * `policy` - The plugin's effective egress policy.
    /// * `tls` - TLS settings for `wss://` ([`default_tls`] in production).
    /// * `handler` - The host's handler for messages and closes.
    /// * `id_prefix` - Prefix for socket ids, unique per broker start so ids
    ///   are not reused across restarts.
    #[must_use]
    pub fn new(
        policy: EgressPolicy,
        tls: Arc<ClientConfig>,
        handler: Rc<dyn SocketHandler>,
        id_prefix: &str,
    ) -> Self {
        Self {
            shared: Rc::new(Shared {
                table: RefCell::new(SocketTable::new(id_prefix)),
                links: RefCell::new(HashMap::new()),
                policy,
                tls,
                handler,
            }),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Sockets for SocketBroker {
    async fn connect(&self, request: SocketConnectRequest) -> Result<SocketInfo> {
        let (id, target) = self.shared.table.borrow_mut().reserve(&request)?;
        let dialed = dial(&self.shared, &request, &target).await;
        let (socket, protocol) = match dialed {
            Ok(done) => done,
            Err(err) => {
                self.shared.table.borrow_mut().abandon(&id);
                return Err(err);
            }
        };
        let info = self
            .shared
            .table
            .borrow_mut()
            .establish(&id, protocol)
            .ok_or_else(|| PluginError::internal("socket vanished during its handshake"))?;
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let (deliveries, delivery_rx) = mpsc::channel(DELIVERY_QUEUE);
        self.shared.links.borrow_mut().insert(id.clone(), commands);
        tokio::task::spawn_local(deliver(Rc::clone(&self.shared.handler), delivery_rx));
        tokio::task::spawn_local(pump(
            Rc::clone(&self.shared),
            id,
            socket,
            command_rx,
            deliveries,
        ));
        Ok(info)
    }

    async fn send(&self, socket: String, frame: SocketFrame) -> Result<()> {
        self.shared.table.borrow().outbound(&socket, &frame)?;
        let link = self.shared.links.borrow().get(&socket).cloned();
        let link = link.ok_or_else(|| not_open(&socket))?;
        link.send(Command::Send(frame))
            .await
            .map_err(|_| not_open(&socket))
    }

    async fn close(&self, socket: String, code: u16, reason: String) -> Result<()> {
        let Some(code) = self
            .shared
            .table
            .borrow()
            .close_request(&socket, code, &reason)?
        else {
            return Ok(());
        };
        let link = self.shared.links.borrow().get(&socket).cloned();
        if let Some(link) = link {
            // A socket that ended meanwhile is closed already: not an error.
            let _ = link.send(Command::Close(code, reason)).await;
        }
        Ok(())
    }

    async fn set_attachment(&self, socket: String, attachment: Vec<u8>) -> Result<()> {
        self.shared
            .table
            .borrow_mut()
            .set_attachment(&socket, attachment)
    }

    async fn list(&self) -> Result<Vec<SocketInfo>> {
        Ok(self.shared.table.borrow().list())
    }
}

/// Refusal for a socket that is not open.
fn not_open(socket: &str) -> PluginError {
    PluginError::not_found(format!("socket `{socket}` is not open"))
}

/// True when the grant covers the socket's target.
///
/// A WebSocket starts as an HTTP request, so a `fetch` domain grant covers
/// it; a TCP grant for the host and port covers it too, since that grant
/// already lets the plugin open the same connection and speak the protocol
/// itself. Either way the addresses are checked afterwards.
#[must_use]
pub fn target_granted(policy: &EgressPolicy, target: &SocketTarget) -> bool {
    policy.allows_initial(&target.host) || policy.allows_tcp(&target.host, target.port)
}

/// Addresses for `target` that the policy allows, after the host check.
///
/// # Errors
///
/// `forbidden` when the host is not an approved domain or every address
/// is outside the grant; `unavailable` when the name does not resolve.
async fn allowed_addresses(
    policy: &EgressPolicy,
    target: &SocketTarget,
) -> Result<Vec<SocketAddr>> {
    if !target_granted(policy, target) {
        return Err(PluginError::forbidden(format!(
            "`{}` is not on the plugin's network grant",
            target.host
        )));
    }
    let resolved: Vec<SocketAddr> = match target.host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, target.port)],
        Err(_) => tokio::net::lookup_host((target.host.as_str(), target.port))
            .await
            .map_err(|err| {
                PluginError::unavailable(format!("could not resolve `{}`: {err}", target.host))
            })?
            .collect(),
    };
    let allowed: Vec<SocketAddr> = resolved
        .into_iter()
        .filter(|addr| policy.allows_ip(addr.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(PluginError::forbidden(format!(
            "`{}` resolves only to addresses outside the plugin's network grant",
            target.host
        )));
    }
    Ok(allowed)
}

/// Connects TCP to the first allowed address that answers.
///
/// # Errors
///
/// `unavailable` with the last connect failure.
async fn connect_tcp(addresses: &[SocketAddr]) -> Result<TcpStream> {
    let mut last = String::from("no address");
    for addr in addresses {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(err) => last = format!("{addr}: {err}"),
        }
    }
    Err(PluginError::unavailable(format!(
        "could not connect: {last}"
    )))
}

/// Builds the upgrade request: the guest's headers, the subprotocols, and
/// the `User-Agent` the policy allows.
///
/// # Errors
///
/// `invalid_params` when the URL or a header does not form a request.
fn upgrade_request(
    policy: &EgressPolicy,
    request: &SocketConnectRequest,
    target: &SocketTarget,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut upgrade = request
        .url
        .as_str()
        .into_client_request()
        .map_err(|err| PluginError::invalid_params(format!("socket URL: {err}")))?;
    let headers = upgrade.headers_mut();
    let mut requested_agent = None;
    for header in &request.headers {
        if header.name.eq_ignore_ascii_case(USER_AGENT.as_str()) {
            requested_agent = Some(header.value.as_str());
            continue;
        }
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|err| PluginError::invalid_params(format!("header name: {err}")))?;
        let value = HeaderValue::from_bytes(header.value.as_bytes())
            .map_err(|err| PluginError::invalid_params(format!("header value: {err}")))?;
        headers.append(name, value);
    }
    let agent = policy.user_agent_for(&target.host, requested_agent);
    let agent = HeaderValue::from_str(&agent)
        .map_err(|err| PluginError::internal(format!("User-Agent: {err}")))?;
    headers.insert(USER_AGENT, agent);
    if !request.protocols.is_empty() {
        let protocols = HeaderValue::from_str(&request.protocols.join(", "))
            .map_err(|err| PluginError::invalid_params(format!("subprotocols: {err}")))?;
        headers.insert(SEC_WEBSOCKET_PROTOCOL, protocols);
    }
    Ok(upgrade)
}

/// Dials, secures and upgrades one socket.
///
/// # Returns
///
/// The open socket and the subprotocol the server chose.
///
/// # Errors
///
/// `forbidden` for an egress refusal; `unavailable` for a network, TLS or
/// handshake failure or a server that refused the upgrade;
/// `deadline_exceeded` when a step timed out.
async fn dial(
    shared: &Shared,
    request: &SocketConnectRequest,
    target: &SocketTarget,
) -> Result<(Socket, Option<String>)> {
    let upgrade = upgrade_request(&shared.policy, request, target)?;
    let connect = async {
        let addresses = allowed_addresses(&shared.policy, target).await?;
        connect_tcp(&addresses).await
    };
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| PluginError::deadline_exceeded("timed out connecting the socket"))??;
    let handshake = async {
        let io: Box<dyn BrokerIo> = if target.secure {
            let name = ServerName::try_from(target.host.clone())
                .map_err(|err| PluginError::invalid_params(format!("TLS name: {err}")))?;
            let tls = tokio_rustls::TlsConnector::from(Arc::clone(&shared.tls))
                .connect(name, tcp)
                .await
                .map_err(|err| PluginError::unavailable(format!("TLS failed: {err}")))?;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_SOCKET_FRAME_BYTES as usize))
            .max_frame_size(Some(MAX_SOCKET_FRAME_BYTES as usize));
        tokio_tungstenite::client_async_with_config(upgrade, io, Some(config))
            .await
            .map_err(|err| match err {
                WsError::Http(response) => PluginError::unavailable(format!(
                    "the server refused the socket with HTTP {}",
                    response.status().as_u16()
                )),
                other => PluginError::unavailable(format!("socket handshake failed: {other}")),
            })
    };
    let (socket, response) = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| PluginError::deadline_exceeded("timed out in the socket handshake"))??;
    let protocol = response
        .headers()
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if let Some(chosen) = &protocol {
        if !request.protocols.iter().any(|p| p == chosen) {
            return Err(PluginError::unavailable(format!(
                "the server chose subprotocol `{chosen}`, which was not requested"
            )));
        }
    }
    Ok((socket, protocol))
}

/// The WebSocket message for a guest frame.
fn to_message(frame: SocketFrame) -> Message {
    match frame {
        SocketFrame::Text(text) => Message::text(text),
        SocketFrame::Binary(bytes) => Message::binary(bytes),
    }
}

/// Sends a close frame and waits briefly for the peer's.
///
/// # Returns
///
/// True when the peer answered with its own close (a clean close).
async fn close_socket(socket: &mut Socket, code: u16, reason: &str) -> bool {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    };
    if socket.close(Some(frame)).await.is_err() {
        return false;
    }
    let drain = async {
        while let Some(next) = socket.next().await {
            match next {
                Ok(Message::Close(_)) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    };
    tokio::time::timeout(CLOSE_TIMEOUT, drain)
        .await
        .unwrap_or(false)
}

/// Runs one open socket: guest commands out, remote frames in, until it
/// ends; then reports the close through the delivery queue.
async fn pump(
    shared: Rc<Shared>,
    id: String,
    mut socket: Socket,
    mut commands: mpsc::Receiver<Command>,
    deliveries: mpsc::Sender<Delivery>,
) {
    let (code, reason, clean) = loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Send(frame)) => {
                    if socket.send(to_message(frame)).await.is_err() {
                        break (CLOSE_ABNORMAL, String::new(), false);
                    }
                }
                Some(Command::Close(code, reason)) => {
                    let clean = close_socket(&mut socket, code, &reason).await;
                    break (code, reason, clean);
                }
                None => {
                    let clean = close_socket(&mut socket, CLOSE_GOING_AWAY, "").await;
                    break (CLOSE_GOING_AWAY, String::new(), clean);
                }
            },
            next = socket.next() => {
                let frame = match next {
                    Some(Ok(Message::Text(text))) => SocketFrame::Text(text.as_str().to_string()),
                    Some(Ok(Message::Binary(bytes))) => SocketFrame::Binary(bytes.to_vec()),
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame.map_or((CLOSE_NO_STATUS, String::new()), |f| {
                            (u16::from(f.code), f.reason.as_str().to_string())
                        });
                        break (code, reason, true);
                    }
                    Some(Err(WsError::Capacity(_))) => {
                        let _ = close_socket(&mut socket, CLOSE_TOO_BIG, "message too big").await;
                        break (CLOSE_TOO_BIG, "message too big".to_string(), false);
                    }
                    Some(Err(_)) | None => break (CLOSE_ABNORMAL, String::new(), false),
                };
                let decision = shared.table.borrow().inbound(&id, frame);
                match decision {
                    Inbound::Reply(reply) => {
                        if socket.send(to_message(reply)).await.is_err() {
                            break (CLOSE_ABNORMAL, String::new(), false);
                        }
                    }
                    Inbound::Deliver(message) => {
                        if deliveries.send(Delivery::Message(message)).await.is_err() {
                            break (CLOSE_GOING_AWAY, String::new(), false);
                        }
                    }
                    Inbound::TooBig => {
                        let _ = close_socket(&mut socket, CLOSE_TOO_BIG, "message too big").await;
                        break (CLOSE_TOO_BIG, "message too big".to_string(), false);
                    }
                    Inbound::Ignore => {}
                }
            }
        }
    };
    shared.links.borrow_mut().remove(&id);
    let closed = shared.table.borrow_mut().closed(&id, code, &reason, clean);
    if let Some(event) = closed {
        let _ = deliveries.send(Delivery::Closed(event)).await;
    }
}

/// Hands one socket's messages, then its close, to the host in order.
async fn deliver(handler: Rc<dyn SocketHandler>, mut queue: mpsc::Receiver<Delivery>) {
    while let Some(item) = queue.recv().await {
        let outcome = match item {
            Delivery::Message(message) => handler.message(message).await,
            Delivery::Closed(event) => {
                let outcome = handler.closed(event).await;
                if let Err(err) = outcome {
                    tracing::warn!(error = %err, "the host did not take a socket close");
                }
                return;
            }
        };
        if let Err(err) = outcome {
            tracing::warn!(error = %err, "the host did not take a socket message");
        }
    }
}

/// Serves the broker on stdio until the host hangs up.
///
/// # Arguments
///
/// * `policy` - The plugin's effective egress policy.
///
/// # Errors
///
/// Returns the TLS setup or link failure.
pub async fn serve_stdio(policy: EgressPolicy) -> Result<()> {
    let tls = default_tls()?;
    let prefix = format!("s{:08x}-", rand::random::<u32>());
    let local = tokio::task::LocalSet::new();
    local
        .run_until(bookclerk_plugin_abi::serve_socket_broker(
            tokio::io::stdin(),
            tokio::io::stdout(),
            move |handler| {
                let handler: Rc<dyn SocketHandler> = Rc::new(handler);
                // The broker lives on this thread's LocalSet; the ABI's
                // server type takes an `Arc` and never sends it elsewhere.
                #[allow(clippy::arc_with_non_send_sync)]
                let broker = Arc::new(SocketBroker::new(policy, tls, handler, &prefix));
                broker as Arc<dyn Sockets>
            },
        ))
        .await
}

#[cfg(test)]
#[path = "socket_broker_tests.rs"]
mod tests;
