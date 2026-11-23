//! The socket broker: holds a plugin instance's WebSockets and raw TLS
//! streams while its guest sleeps.
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
//! - a `tls://host:port` stream needs a TCP grant for its host and port,
//!   offers the guest's ALPN ids, checks the server against the guest's own
//!   trust anchors when it sends any (else Mozilla's roots), and carries bytes: each read becomes a
//!   binary message and each binary frame the guest sends is written as is;
//!   the peer ending the stream is a clean close (1000);
//! - heartbeat frames the guest declared at connect are answered here and
//!   never wake the guest ([`SocketTable::inbound`]);
//! - other messages queue per socket and reach the host one at a time; the
//!   broker stops reading a socket whose queue is full, and keeps answering
//!   heartbeats otherwise while a delivery waits for the guest to wake;
//! - a socket's close reaches the host after its last message, once.
//!
//! The decisions live in [`crate::socket_table`], which the fuzz target
//! drives without I/O.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_plugin_abi::sockets::{SocketTarget, SOCKET_CLOSE_NORMAL};
use bookclerk_plugin_abi::{
    LimitExceeded, PluginError, Result, SocketClosed, SocketConnectRequest, SocketFrame,
    SocketHandler, SocketInfo, SocketMessage, Sockets, MAX_SOCKETS_PER_INSTANCE,
    MAX_SOCKET_FRAME_BYTES,
};
use bookclerk_plugin_manifest::EgressPolicy;
use futures::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
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

/// Longest a whole close may take: sending the broker's close frame and
/// waiting for the peer's, or flushing the reply to the peer's close.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest one frame write may take before the broker drops a peer that
/// stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Most bytes one read of a `tls://` stream hands the guest as a message.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// A byte stream the broker runs a WebSocket over (plain TCP or TLS).
trait BrokerIo: AsyncRead + AsyncWrite + Unpin {}

impl<T: AsyncRead + AsyncWrite + Unpin> BrokerIo for T {}

/// An open WebSocket as the broker holds it.
type Socket = WebSocketStream<Box<dyn BrokerIo>>;

/// An open connection: a WebSocket, or a `tls://` byte stream.
enum Conn {
    /// A WebSocket.
    Ws(Box<Socket>),
    /// A raw TLS byte stream.
    Stream(Box<dyn BrokerIo>),
}

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
    /// The host's proxy link. In production every dial is a tunnel the host
    /// checks and dials, because the broker's jail has no IP sockets. `None`
    /// only in unjailed tests, which dial directly.
    tunnels: Option<Arc<bookclerk_plugin_sdk::mux::Mux>>,
    /// The host's handler.
    handler: Rc<dyn SocketHandler>,
    /// Sockets that ended but whose delivery task still runs (a message or
    /// the close is still with the host). They count against the
    /// per-instance limit, so a host that is slow to take deliveries
    /// cannot collect unbounded delivery work.
    lingering: Cell<usize>,
    /// How many times a socket's delivery queue was full and the broker
    /// paused reading it; tests use it to reach that state for certain.
    queue_full: Cell<u64>,
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
                tunnels: None,
                handler,
                lingering: Cell::new(0),
                queue_full: Cell::new(0),
            }),
        }
    }
}

impl SocketBroker {
    /// Sends every dial through the host's proxy link as a tunnel.
    ///
    /// # Arguments
    ///
    /// * `tunnels` - The host proxy link.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    ///
    /// # Panics
    ///
    /// Never: the shared state has no other owner until the broker serves.
    #[must_use]
    pub fn with_tunnels(mut self, tunnels: Arc<bookclerk_plugin_sdk::mux::Mux>) -> Self {
        if let Some(shared) = Rc::get_mut(&mut self.shared) {
            shared.tunnels = Some(tunnels);
        }
        self
    }
}

/// A pending socket's reservation: abandoned when `connect` ends before the
/// socket is open, whether its dial failed or the caller cancelled it.
struct Reservation {
    /// The broker's state.
    shared: Rc<Shared>,
    /// The reserved socket id.
    id: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Leaves an open socket alone.
        self.shared.table.borrow_mut().abandon(&self.id);
    }
}

/// Counts one socket's delivery task among the lingering ones once its
/// socket ended, until the task finishes.
struct DeliveryTask {
    /// The broker's state.
    shared: Rc<Shared>,
    /// Whether the socket ended and this task was counted.
    counted: Cell<bool>,
}

impl Drop for DeliveryTask {
    fn drop(&mut self) {
        if self.counted.get() {
            self.shared
                .lingering
                .set(self.shared.lingering.get().saturating_sub(1));
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Sockets for SocketBroker {
    async fn connect(&self, request: SocketConnectRequest) -> Result<SocketInfo> {
        let held = self.shared.table.borrow().len() + self.shared.lingering.get();
        if held >= MAX_SOCKETS_PER_INSTANCE as usize {
            return Err(PluginError::limit_exceeded(LimitExceeded::new(
                "maxSocketsPerInstance",
                u64::from(MAX_SOCKETS_PER_INSTANCE),
                u64::try_from(held + 1).unwrap_or(u64::MAX),
            )));
        }
        let (id, target) = self.shared.table.borrow_mut().reserve(&request)?;
        let reservation = Reservation {
            shared: Rc::clone(&self.shared),
            id: id.clone(),
        };
        let (conn, protocol) = dial(&self.shared, &request, &target).await?;
        let info = self
            .shared
            .table
            .borrow_mut()
            .establish(&id, protocol)
            .ok_or_else(|| PluginError::internal("socket vanished during its handshake"))?;
        drop(reservation);
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let (deliveries, delivery_rx) = mpsc::channel(DELIVERY_QUEUE);
        self.shared.links.borrow_mut().insert(id.clone(), commands);
        let task = Rc::new(DeliveryTask {
            shared: Rc::clone(&self.shared),
            counted: Cell::new(false),
        });
        tokio::task::spawn_local(deliver(Rc::clone(&task), delivery_rx));
        let shared = Rc::clone(&self.shared);
        match conn {
            Conn::Ws(socket) => {
                tokio::task::spawn_local(pump(shared, id, *socket, command_rx, deliveries, task));
            }
            Conn::Stream(stream) => {
                tokio::task::spawn_local(pump_stream(
                    shared, id, stream, command_rx, deliveries, task,
                ));
            }
        }
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
/// A WebSocket starts as an HTTP request, so a `fetch` domain grant that
/// grants the target's port covers it (an entry with no port grants 80 and
/// 443, as for `fetch`); a TCP grant for the host and port covers it too, since that grant
/// already lets the plugin open the same connection and speak the protocol
/// itself. A `tls://` stream is raw TCP, so only a TCP grant covers it.
/// Either way the addresses are checked afterwards.
#[must_use]
pub fn target_granted(policy: &EgressPolicy, target: &SocketTarget) -> bool {
    let tcp = policy.allows_tcp(&target.host, target.port);
    if target.scheme.is_stream() {
        return tcp;
    }
    policy.allows_fetch(&target.host, target.port) || tcp
}

/// The typed refusal for a target the grant does not cover, naming the
/// pending network setting whose value names it, when one does.
fn not_granted(policy: &EgressPolicy, target: &SocketTarget) -> PluginError {
    let destination = if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let setting = policy
        .pending_setting_for(&target.host, target.port)
        .map(|pending| pending.setting.as_str());
    PluginError::network_not_granted(&destination, setting)
}

/// Addresses for `target` that the policy allows, after the host check.
///
/// # Errors
///
/// `network_not_granted` when the host is not an approved domain,
/// `forbidden` when every address is outside the grant; `unavailable` when the name does not resolve.
async fn allowed_addresses(
    policy: &EgressPolicy,
    target: &SocketTarget,
) -> Result<Vec<SocketAddr>> {
    if !target_granted(policy, target) {
        return Err(not_granted(policy, target));
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
        .filter(|addr| policy.allows_ip_for(&target.host, addr.ip()))
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

/// Dials, secures and upgrades one socket (or, for `tls://`, dials and
/// secures one stream).
///
/// # Returns
///
/// The open connection and the subprotocol the server chose (the
/// negotiated ALPN id for a stream).
///
/// # Errors
///
/// `invalid_params` for an unusable `tls://` trust anchor; `forbidden` for
/// an egress refusal; `unavailable` for a network, TLS or
/// handshake failure or a server that refused the upgrade;
/// `deadline_exceeded` when a step timed out.
async fn dial(
    shared: &Shared,
    request: &SocketConnectRequest,
    target: &SocketTarget,
) -> Result<(Conn, Option<String>)> {
    if target.scheme.is_stream() {
        // Settle the TLS settings first, so a bad trust anchor costs no dial.
        let config = stream_tls(shared, request)?;
        let stream = dial_tcp(shared, target).await?;
        return secure_stream(config, request, target, stream).await;
    }
    let stream = dial_tcp(shared, target).await?;
    let upgrade = upgrade_request(&shared.policy, request, target)?;
    let handshake = async {
        let io: Box<dyn BrokerIo> = if target.scheme.uses_tls() {
            let name = ServerName::try_from(target.host.clone())
                .map_err(|err| PluginError::invalid_params(format!("TLS name: {err}")))?;
            let tls = tokio_rustls::TlsConnector::from(Arc::clone(&shared.tls))
                .connect(name, stream)
                .await
                .map_err(|err| PluginError::unavailable(format!("TLS failed: {err}")))?;
            Box::new(tls)
        } else {
            stream
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
    Ok((Conn::Ws(Box::new(socket)), protocol))
}

/// Secures a `tls://` stream with the settings [`stream_tls`] chose.
///
/// # Returns
///
/// The stream and the negotiated ALPN id, if any.
///
/// # Errors
///
/// `unavailable` for a TLS failure or an ALPN id the guest did not offer;
/// `deadline_exceeded` when the handshake timed out.
async fn secure_stream(
    config: ClientConfig,
    request: &SocketConnectRequest,
    target: &SocketTarget,
    stream: Box<dyn BrokerIo>,
) -> Result<(Conn, Option<String>)> {
    let name = ServerName::try_from(target.host.clone())
        .map_err(|err| PluginError::invalid_params(format!("TLS name: {err}")))?;
    let handshake = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, stream);
    let tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| PluginError::deadline_exceeded("timed out in the TLS handshake"))?
        .map_err(|err| PluginError::unavailable(format!("TLS failed: {err}")))?;
    let protocol = tls
        .get_ref()
        .1
        .alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned());
    if let Some(chosen) = &protocol {
        if !request.protocols.iter().any(|p| p == chosen) {
            return Err(PluginError::unavailable(
                "the server chose an ALPN protocol that was not offered",
            ));
        }
    }
    Ok((Conn::Stream(Box::new(tls)), protocol))
}

/// TLS settings for a `tls://` stream: Mozilla's roots, or only the guest's
/// trust anchors when it sent any, offering the guest's ALPN ids.
///
/// # Errors
///
/// `invalid_params` when an anchor is not a usable root certificate.
fn stream_tls(shared: &Shared, request: &SocketConnectRequest) -> Result<ClientConfig> {
    let mut config = if request.trust_anchors.is_empty() {
        (*shared.tls).clone()
    } else {
        anchored_tls(&request.trust_anchors)?
    };
    config.alpn_protocols = request
        .protocols
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect();
    Ok(config)
}

/// TLS settings that trust only the guest's own roots, for a `tls://`
/// service that signs its endpoint with a private CA.
///
/// # Errors
///
/// `invalid_params` when an anchor is not a usable root certificate.
fn anchored_tls(anchors: &[Vec<u8>]) -> Result<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for der in anchors {
        roots
            .add(CertificateDer::from(der.clone()))
            .map_err(|err| PluginError::invalid_params(format!("trust anchor: {err}")))?;
    }
    Ok(
        ClientConfig::builder_with_provider(bookclerk_tls::crypto_provider())
            .with_safe_default_protocol_versions()
            .map_err(|err| PluginError::internal(format!("TLS settings: {err}")))?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Connects TCP for one socket, through the host's tunnel in production.
///
/// # Errors
///
/// `network_not_granted` or `forbidden` for an egress refusal;
/// `unavailable` for a network failure; `deadline_exceeded` after
/// [`CONNECT_TIMEOUT`].
async fn dial_tcp(shared: &Shared, target: &SocketTarget) -> Result<Box<dyn BrokerIo>> {
    let connect = async {
        if let Some(tunnels) = &shared.tunnels {
            // Same refusal text as a direct dial; the host checks again and
            // resolves and checks every address itself.
            if !target_granted(&shared.policy, target) {
                return Err(not_granted(&shared.policy, target));
            }
            let tunnel_target = crate::egress_relay::TunnelTarget {
                host: target.host.to_ascii_lowercase(),
                port: target.port,
            };
            let stream = crate::egress_relay::open_tunnel(tunnels.as_ref(), &tunnel_target)
                .await
                .map_err(|err| PluginError::unavailable(format!("could not connect: {err}")))?;
            return Ok(Box::new(stream) as Box<dyn BrokerIo>);
        }
        let addresses = allowed_addresses(&shared.policy, target).await?;
        Ok(Box::new(connect_tcp(&addresses).await?) as Box<dyn BrokerIo>)
    };
    tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| PluginError::deadline_exceeded("timed out connecting the socket"))?
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
    let close = async {
        if socket.close(Some(frame)).await.is_err() {
            return false;
        }
        while let Some(next) = socket.next().await {
            match next {
                Ok(Message::Close(_)) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    };
    tokio::time::timeout(CLOSE_TIMEOUT, close)
        .await
        .unwrap_or(false)
}

/// Sends one frame, giving up after [`WRITE_TIMEOUT`].
///
/// # Returns
///
/// False when the write failed or the peer stopped reading.
async fn write(socket: &mut Socket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

/// Runs one open socket: guest commands out, remote frames in, until it
/// ends; then reports the close through the delivery queue.
///
/// A full delivery queue stops reading the socket but never stops serving
/// the guest's commands, so a handler that sends while its own messages
/// wait cannot stall the socket.
async fn pump(
    shared: Rc<Shared>,
    id: String,
    mut socket: Socket,
    mut commands: mpsc::Receiver<Command>,
    deliveries: mpsc::Sender<Delivery>,
    task: Rc<DeliveryTask>,
) {
    // A message waiting for room in the delivery queue.
    let mut held: Option<Delivery> = None;
    let (code, reason, clean) = loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Send(frame)) => {
                    if !write(&mut socket, to_message(frame)).await {
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
            permit = deliveries.reserve(), if held.is_some() => match (permit, held.take()) {
                (Ok(permit), Some(item)) => permit.send(item),
                (Ok(_), None) => {}
                (Err(_), _) => break (CLOSE_GOING_AWAY, String::new(), false),
            },
            next = socket.next(), if held.is_none() => {
                let frame = match next {
                    Some(Ok(Message::Text(text))) => SocketFrame::Text(text.as_str().to_string()),
                    Some(Ok(Message::Binary(bytes))) => SocketFrame::Binary(bytes.to_vec()),
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame.map_or((CLOSE_NO_STATUS, String::new()), |f| {
                            (u16::from(f.code), f.reason.as_str().to_string())
                        });
                        // tungstenite queued the reply; send it before
                        // calling the close clean.
                        let replied = matches!(
                            tokio::time::timeout(CLOSE_TIMEOUT, socket.flush()).await,
                            Ok(Ok(()) | Err(WsError::ConnectionClosed))
                        );
                        break (code, reason, replied);
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
                        if !write(&mut socket, to_message(reply)).await {
                            break (CLOSE_ABNORMAL, String::new(), false);
                        }
                    }
                    Inbound::Deliver(message) => {
                        match deliveries.try_send(Delivery::Message(message)) {
                            Ok(()) => {}
                            Err(TrySendError::Full(item)) => {
                                shared.queue_full.set(shared.queue_full.get() + 1);
                                held = Some(item);
                            }
                            Err(TrySendError::Closed(_)) => {
                                break (CLOSE_GOING_AWAY, String::new(), false);
                            }
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
    // Guest sends waiting on this socket fail now rather than wait on a
    // pump that no longer serves them.
    drop(commands);
    drop(socket);
    finish(
        shared,
        id,
        Ended {
            code,
            reason,
            clean,
        },
        held,
        deliveries,
        task,
    )
    .await;
}

/// How a socket ended, as its close reports it.
struct Ended {
    /// Close code reported to the guest.
    code: u16,
    /// Close reason reported to the guest.
    reason: String,
    /// Whether the close handshake (or stream shutdown) completed.
    clean: bool,
}

/// Frees an ended socket's slot and hands its last held message, then its
/// close, to the delivery task.
async fn finish(
    shared: Rc<Shared>,
    id: String,
    ended: Ended,
    held: Option<Delivery>,
    deliveries: mpsc::Sender<Delivery>,
    task: Rc<DeliveryTask>,
) {
    let Ended {
        code,
        reason,
        clean,
    } = ended;
    shared.links.borrow_mut().remove(&id);
    let closed = shared.table.borrow_mut().closed(&id, code, &reason, clean);
    // The socket's slot is free, but its delivery task still holds work
    // until the host took it.
    task.counted.set(true);
    shared.lingering.set(shared.lingering.get() + 1);
    drop(task);
    if let Some(item) = held {
        if deliveries.send(item).await.is_err() {
            return;
        }
    }
    if let Some(event) = closed {
        let _ = deliveries.send(Delivery::Closed(event)).await;
    }
}

/// Ends a `tls://` stream the guest closed: TLS close_notify, then waits
/// for the peer to end its side.
///
/// # Returns
///
/// True when the peer ended the stream within [`CLOSE_TIMEOUT`].
async fn close_stream(stream: &mut Box<dyn BrokerIo>) -> bool {
    let drained = async {
        if stream.shutdown().await.is_err() {
            return false;
        }
        let mut sink = vec![0_u8; STREAM_CHUNK_BYTES];
        loop {
            match stream.read(&mut sink).await {
                Ok(0) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    };
    matches!(tokio::time::timeout(CLOSE_TIMEOUT, drained).await, Ok(true))
}

/// Writes bytes to a `tls://` stream and flushes them.
///
/// # Returns
///
/// False when the write failed or the peer stopped reading.
async fn write_stream(stream: &mut Box<dyn BrokerIo>, bytes: &[u8]) -> bool {
    let written = async {
        stream.write_all(bytes).await?;
        stream.flush().await
    };
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, written).await,
        Ok(Ok(()))
    )
}

/// Runs one open `tls://` stream as [`pump`] runs a WebSocket: each read
/// becomes a binary message, each binary frame the guest sends is written
/// as is, and the peer ending the stream is a clean close (1000).
async fn pump_stream(
    shared: Rc<Shared>,
    id: String,
    mut stream: Box<dyn BrokerIo>,
    mut commands: mpsc::Receiver<Command>,
    deliveries: mpsc::Sender<Delivery>,
    task: Rc<DeliveryTask>,
) {
    let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
    let mut held: Option<Delivery> = None;
    let (code, reason, clean) = loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Send(SocketFrame::Binary(bytes))) => {
                    if !write_stream(&mut stream, &bytes).await {
                        break (CLOSE_ABNORMAL, String::new(), false);
                    }
                }
                // The table refuses text on a stream before it is queued.
                Some(Command::Send(SocketFrame::Text(_))) => {}
                Some(Command::Close(code, reason)) => {
                    let clean = close_stream(&mut stream).await;
                    break (code, reason, clean);
                }
                None => {
                    let clean = close_stream(&mut stream).await;
                    break (CLOSE_GOING_AWAY, String::new(), clean);
                }
            },
            permit = deliveries.reserve(), if held.is_some() => match (permit, held.take()) {
                (Ok(permit), Some(item)) => permit.send(item),
                (Ok(_), None) => {}
                (Err(_), _) => break (CLOSE_GOING_AWAY, String::new(), false),
            },
            read = stream.read(&mut buffer), if held.is_none() => {
                let frame = match read {
                    Ok(0) => break (SOCKET_CLOSE_NORMAL, String::new(), true),
                    Ok(n) => SocketFrame::Binary(buffer.get(..n).unwrap_or_default().to_vec()),
                    Err(_) => break (CLOSE_ABNORMAL, String::new(), false),
                };
                let decision = shared.table.borrow().inbound(&id, frame);
                match decision {
                    // A stream declares no heartbeats, so nothing is
                    // answered here; a reply would be written as is.
                    Inbound::Reply(SocketFrame::Binary(reply)) => {
                        if !write_stream(&mut stream, &reply).await {
                            break (CLOSE_ABNORMAL, String::new(), false);
                        }
                    }
                    Inbound::Reply(SocketFrame::Text(_)) | Inbound::Ignore => {}
                    Inbound::Deliver(message) => {
                        match deliveries.try_send(Delivery::Message(message)) {
                            Ok(()) => {}
                            Err(TrySendError::Full(item)) => {
                                shared.queue_full.set(shared.queue_full.get() + 1);
                                held = Some(item);
                            }
                            Err(TrySendError::Closed(_)) => {
                                break (CLOSE_GOING_AWAY, String::new(), false);
                            }
                        }
                    }
                    Inbound::TooBig => {
                        let _ = close_stream(&mut stream).await;
                        break (CLOSE_TOO_BIG, "message too big".to_string(), false);
                    }
                }
            }
        }
    };
    drop(commands);
    drop(stream);
    finish(
        shared,
        id,
        Ended {
            code,
            reason,
            clean,
        },
        held,
        deliveries,
        task,
    )
    .await;
}

/// Hands one socket's messages, then its close, to the host in order.
async fn deliver(task: Rc<DeliveryTask>, mut queue: mpsc::Receiver<Delivery>) {
    let handler = Rc::clone(&task.shared.handler);
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
/// * `tunnels` - The host's CONNECT link. When set, every socket is dialed
///   through it, so the broker's own jail needs no network; `None` dials
///   directly (diagnostic spawns only).
///
/// # Errors
///
/// Returns the TLS setup or link failure.
pub async fn serve_stdio(
    policy: EgressPolicy,
    tunnels: Option<Arc<bookclerk_plugin_sdk::mux::Mux>>,
) -> Result<()> {
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
                let broker = SocketBroker::new(policy, tls, handler, &prefix);
                let broker = match tunnels {
                    Some(tunnels) => broker.with_tunnels(tunnels),
                    None => broker,
                };
                #[allow(clippy::arc_with_non_send_sync)]
                let broker = Arc::new(broker);
                broker as Arc<dyn Sockets>
            },
        ))
        .await
}

#[cfg(test)]
#[path = "socket_broker_tests.rs"]
mod tests;
