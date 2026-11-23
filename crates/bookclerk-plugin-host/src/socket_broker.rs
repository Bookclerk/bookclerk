//! The host's side of host-brokered sockets.
//!
//! A [`SocketBroker`] belongs to one deployed plugin instance that declares
//! `[sockets]`, and outlives every guest process of that instance: a guest
//! that sleeps keeps its sockets. On the first `connect` it starts
//! `bookclerk-workerd` in broker mode through the ordinary spawn path, so
//! the broker runs in the plugin's jail under the plugin's covering grant
//! (see `bookclerk_workerd::socket_broker`). The host talks to it over one
//! Cap'n Proto link on a dedicated thread, where the broker serves
//! `Sockets` and the host serves `SocketHandler`.
//!
//! What arrives on a socket goes to the instance's [`SocketSink`], which
//! wakes the guest if it sleeps and calls its `socketHandler` entrypoint.
//! The broker process stops once its last socket closed, so an instance
//! with no open socket runs nothing; if it dies, every socket it held is
//! reported closed with 1006 and the attachment the host last saw.
//!
//! The guest reaches the broker through [`BrokerSockets`], the `SOCKETS`
//! binding a session hands out only when the manifest declares
//! `[sockets]` and the covering grant has the `sockets` token.
//!
//! A relay ingress's `Origin.connect` adds sockets of a second kind: a
//! WebSocket into the library itself ([`LocalSocket`]), which the host
//! holds in process instead of in the broker. They share the instance's id
//! space, socket cap and sink, so the guest cannot tell them apart.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::{Context, Poll};

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_plugin_abi::sockets::{
    check_socket_attachment, check_socket_close, check_socket_frame,
};
use bookclerk_plugin_abi::{
    LimitExceeded, PluginError as AbiError, Result as AbiResult, SocketClosed,
    SocketConnectRequest, SocketFrame, SocketHandler, SocketInfo, SocketMessage, SocketMessageOk,
    Sockets, MAX_ORIGIN_SOCKETS_PER_INSTANCE, MAX_SOCKETS_PER_INSTANCE,
};
use bookclerk_workerd::socket_broker::SOCKET_BROKER_ENV;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};

use crate::spawn_stdio::SpawnedStdio;
use crate::{DiscoveredPlugin, GuestRuntimeKind, SpawnPlan, SpawnTransport};

/// Close code reported for sockets lost when the broker process dies.
const CLOSE_ABNORMAL: u16 = 1006;

/// What a socket delivered: a message, or its close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketEvent {
    /// One message on an open socket.
    Message(SocketMessage),
    /// The socket ended; nothing more arrives on it.
    Closed(SocketClosed),
}

/// Where a plugin instance's socket events go: its guest's `socketHandler`.
#[async_trait]
pub trait SocketSink: Send + Sync {
    /// Hands one event to the guest, starting it if it sleeps.
    ///
    /// A sink that schedules the guest's event poll acts on the guest's
    /// [`SocketMessageOk::poll_events`] itself, polling at once.
    ///
    /// # Returns
    ///
    /// The guest's reply to a message; the default for a close.
    ///
    /// # Errors
    ///
    /// Returns the wake or delivery failure as text. The broker logs it and
    /// carries on: a socket is not closed because one delivery failed.
    async fn deliver_socket_event(
        &self,
        event: SocketEvent,
    ) -> std::result::Result<SocketMessageOk, String>;
}

/// What the library's side of a [`LocalSocket`] delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalInbound {
    /// One message from the library.
    Frame(SocketFrame),
    /// The socket ended; nothing more arrives on it.
    Closed {
        /// WebSocket close code; 1006 when it dropped without one.
        code: u16,
        /// Close reason the library sent; `None` when none.
        reason: Option<String>,
        /// Whether the close handshake completed.
        clean: bool,
    },
}

/// What the guest asks of the library's side of a [`LocalSocket`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalOutbound {
    /// Send one message to the library.
    Frame(SocketFrame),
    /// Close the socket with this code (already checked) and reason.
    Close {
        /// WebSocket close code.
        code: u16,
        /// Close reason, at most 123 bytes.
        reason: String,
    },
}

/// A WebSocket into the library that the host holds for a plugin instance:
/// an `Origin.connect` the library accepted.
///
/// The library's side runs on `runtime` and talks to the host through the
/// two channels. Dropping `outbound` (the instance is retired) tells it to
/// close the socket; it reports every close on `inbound` once, last.
pub struct LocalSocket {
    /// Runtime the host's delivery task runs on; it outlives the guest's
    /// sessions, so a sleeping guest keeps the socket.
    pub runtime: tokio::runtime::Handle,
    /// URL the guest sees for the socket, without credentials.
    pub url: String,
    /// Subprotocol the library chose; `None` when none.
    pub protocol: Option<String>,
    /// Messages and the close for the library.
    pub outbound: mpsc::Sender<LocalOutbound>,
    /// Messages and the close from the library.
    pub inbound: mpsc::Receiver<LocalInbound>,
}

/// An open [`LocalSocket`] as the host tracks it.
struct LocalEntry {
    /// URL the guest sees.
    url: String,
    /// Subprotocol the library chose.
    protocol: Option<String>,
    /// The attachment the guest last set.
    attachment: Vec<u8>,
    /// Where the guest's sends and close go.
    outbound: mpsc::Sender<LocalOutbound>,
}

/// One request for the broker thread.
enum Command {
    /// `Sockets.connect`.
    Connect(SocketConnectRequest, oneshot::Sender<AbiResult<SocketInfo>>),
    /// `Sockets.send`.
    Send(String, SocketFrame, oneshot::Sender<AbiResult<()>>),
    /// `Sockets.close`.
    Close(String, u16, String, oneshot::Sender<AbiResult<()>>),
    /// `Sockets.setAttachment`.
    SetAttachment(String, Vec<u8>, oneshot::Sender<AbiResult<()>>),
    /// `Sockets.list`.
    List(oneshot::Sender<AbiResult<Vec<SocketInfo>>>),
}

/// The running broker: its command queue.
struct Link {
    /// Commands for the broker thread; closed when the thread ends.
    tx: mpsc::UnboundedSender<Command>,
}

/// State shared by every clone of a [`SocketBroker`] and its thread.
struct Inner {
    /// The plugin the broker serves.
    plugin: DiscoveredPlugin,
    /// Host config the broker is spawned with.
    config: Config,
    /// Where events go; set once the instance's guest proxy exists.
    sink: OnceLock<Weak<dyn SocketSink>>,
    /// The running broker, if any. Held across a connect so two connects
    /// do not start two brokers.
    link: tokio::sync::Mutex<Option<Link>>,
    /// Open sockets and the attachment the host last saw for each, keyed
    /// by the broker generation that opened them and the socket id, so the
    /// host can report them closed if their broker dies. Keying by
    /// generation keeps two brokers' sockets apart even if their ids match.
    open: Mutex<BTreeMap<SocketKey, Vec<u8>>>,
    /// Sockets whose close reached the host before their connect answer
    /// did, by broker generation, so the late answer does not resurrect
    /// them in `open`. Lock after `open`.
    closed_early: Mutex<BTreeSet<SocketKey>>,
    /// The last broker generation started; each broker process gets the
    /// next one.
    generation: AtomicU64,
    /// Woken when the sink is installed and when this state is dropped,
    /// so an event waiting for a sink ends either way.
    sink_ready: Arc<tokio::sync::Notify>,
    /// Open sockets into the library, by guest id (`o<n>`). Lock after
    /// `open` when both are held.
    local: Mutex<BTreeMap<String, LocalEntry>>,
    /// Counter for the next local socket id; never reset.
    local_next: AtomicU64,
}

/// A socket as the host tracks it: the broker generation that opened it
/// and the id that broker minted.
type SocketKey = (u64, String);

impl Inner {
    /// Locks the open-socket mirror; a poisoned lock still holds a map.
    fn open(&self) -> std::sync::MutexGuard<'_, BTreeMap<SocketKey, Vec<u8>>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Locks the set of sockets closed before their connect was recorded.
    fn closed_early(&self) -> std::sync::MutexGuard<'_, BTreeSet<SocketKey>> {
        self.closed_early
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a socket broker `generation` opened, unless its close
    /// already came.
    ///
    /// Runs on the link thread as the connect answer arrives, so a caller
    /// that stops waiting still leaves the socket mirrored.
    fn record_open(&self, generation: u64, info: &SocketInfo) {
        let key = (generation, info.socket.clone());
        let mut open = self.open();
        if !self.closed_early().remove(&key) {
            open.insert(key, info.attachment.clone());
        }
    }

    /// Records a socket broker `generation` closed.
    ///
    /// # Returns
    ///
    /// Whether that broker holds no open socket any more.
    fn record_closed(&self, generation: u64, socket: &str) -> bool {
        let key = (generation, socket.to_string());
        let mut open = self.open();
        if open.remove(&key).is_none() {
            self.closed_early().insert(key);
        }
        !open.keys().any(|(owner, _)| *owner == generation)
    }

    /// Whether broker `generation` holds an open socket.
    fn holds(&self, generation: u64) -> bool {
        self.open().keys().any(|(owner, _)| *owner == generation)
    }

    /// Replaces the attachment the host last saw for an open socket of
    /// broker `generation`.
    fn record_attachment(&self, generation: u64, socket: &str, attachment: &[u8]) {
        if let Some(slot) = self.open().get_mut(&(generation, socket.to_string())) {
            slot.clear();
            slot.extend_from_slice(attachment);
        }
    }

    /// Forgets everything broker `generation` held.
    ///
    /// # Returns
    ///
    /// Its sockets that were still open, with their attachments.
    fn take_generation(&self, generation: u64) -> Vec<(String, Vec<u8>)> {
        let mut open = self.open();
        self.closed_early()
            .retain(|(owner, _)| *owner != generation);
        let kept = open.split_off(&(generation.saturating_add(1), String::new()));
        let lost = open.split_off(&(generation, String::new()));
        open.extend(kept);
        lost.into_iter()
            .map(|((_, socket), attachment)| (socket, attachment))
            .collect()
    }

    /// Locks the local socket table; a poisoned lock still holds a map.
    fn local(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, LocalEntry>> {
        self.local
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Refuses one more outbound socket once the instance holds its most.
    ///
    /// # Errors
    ///
    /// `limit_exceeded` at [`MAX_SOCKETS_PER_INSTANCE`].
    fn check_capacity(&self) -> AbiResult<()> {
        let held = self.open().len();
        if held >= MAX_SOCKETS_PER_INSTANCE as usize {
            return Err(AbiError::limit_exceeded(LimitExceeded::new(
                "maxSocketsPerInstance",
                u64::from(MAX_SOCKETS_PER_INSTANCE),
                u64::try_from(held + 1).unwrap_or(u64::MAX),
            )));
        }
        Ok(())
    }

    /// Refuses one more origin socket once the instance holds its most.
    ///
    /// Origin sockets have their own cap, apart from outbound ones, because
    /// each live connection a visitor opens through a public address is one.
    ///
    /// # Errors
    ///
    /// `limit_exceeded` at [`MAX_ORIGIN_SOCKETS_PER_INSTANCE`].
    fn check_local_capacity(&self) -> AbiResult<()> {
        let held = self.local().len();
        if held >= MAX_ORIGIN_SOCKETS_PER_INSTANCE as usize {
            return Err(AbiError::limit_exceeded(LimitExceeded::new(
                "maxOriginSocketsPerInstance",
                u64::from(MAX_ORIGIN_SOCKETS_PER_INSTANCE),
                u64::try_from(held + 1).unwrap_or(u64::MAX),
            )));
        }
        Ok(())
    }

    /// The sink, while the instance's guest proxy is alive.
    fn sink(&self) -> Option<Arc<dyn SocketSink>> {
        self.sink.get().and_then(Weak::upgrade)
    }
}

/// The host's handle on one plugin instance's socket broker.
///
/// Cheap to clone; every clone shares the broker. Dropping the last clone
/// stops the broker process and its sockets.
#[derive(Clone)]
pub struct SocketBroker {
    /// Shared state.
    inner: Arc<Inner>,
}

impl SocketBroker {
    /// A broker handle for `plugin`; nothing starts until the first
    /// `connect`.
    ///
    /// # Arguments
    ///
    /// * `plugin` - The deployed plugin; it must declare `[sockets]`.
    /// * `config` - Host config for the broker's spawn.
    #[must_use]
    pub fn new(plugin: DiscoveredPlugin, config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                plugin,
                config,
                sink: OnceLock::new(),
                link: tokio::sync::Mutex::new(None),
                open: Mutex::new(BTreeMap::new()),
                closed_early: Mutex::new(BTreeSet::new()),
                generation: AtomicU64::new(0),
                sink_ready: Arc::new(tokio::sync::Notify::new()),
                local: Mutex::new(BTreeMap::new()),
                local_next: AtomicU64::new(0),
            }),
        }
    }

    /// Sets where socket events go. Only the first call takes effect.
    ///
    /// The broker keeps a weak reference, so it never keeps a retired
    /// instance's proxy alive.
    pub fn set_sink(&self, sink: Weak<dyn SocketSink>) {
        let _ = self.inner.sink.set(sink);
        self.inner.sink_ready.notify_waiters();
    }

    /// Ids of the sockets the host believes open: broker sockets oldest
    /// id first, then local ones.
    #[must_use]
    pub fn open_sockets(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .inner
            .open()
            .keys()
            .map(|(generation, socket)| guest_socket(*generation, socket))
            .collect();
        ids.extend(self.inner.local().keys().cloned());
        ids
    }

    /// Makes a WebSocket into the library a socket of this instance and
    /// starts delivering what arrives on it.
    ///
    /// # Arguments
    ///
    /// * `socket` - The library's side.
    /// * `attachment` - The guest's initial attachment.
    ///
    /// # Returns
    ///
    /// The socket as the guest sees it.
    ///
    /// # Errors
    ///
    /// `payload_too_large` for an oversized attachment; `limit_exceeded`
    /// when the instance holds its most origin sockets. The socket is closed
    /// either way.
    pub fn adopt_local(&self, socket: LocalSocket, attachment: Vec<u8>) -> AbiResult<SocketInfo> {
        let LocalSocket {
            runtime,
            url,
            protocol,
            outbound,
            inbound,
        } = socket;
        check_socket_attachment(&attachment)?;
        self.inner.check_local_capacity()?;
        let n = self.inner.local_next.fetch_add(1, Ordering::SeqCst) + 1;
        let id = local_socket(n);
        self.inner.local().insert(
            id.clone(),
            LocalEntry {
                url: url.clone(),
                protocol: protocol.clone(),
                attachment: attachment.clone(),
                outbound,
            },
        );
        runtime.spawn(pump_local(Arc::downgrade(&self.inner), id.clone(), inbound));
        Ok(SocketInfo {
            socket: id,
            url,
            protocol,
            attachment,
        })
    }

    /// Refuses when the instance already holds its most origin sockets.
    ///
    /// # Errors
    ///
    /// `limit_exceeded` at [`MAX_ORIGIN_SOCKETS_PER_INSTANCE`].
    pub(crate) fn check_room(&self) -> AbiResult<()> {
        self.inner.check_local_capacity()
    }

    /// The local socket `socket`'s outbound channel, when it is open.
    fn local_outbound(&self, socket: &str) -> Option<mpsc::Sender<LocalOutbound>> {
        self.inner
            .local()
            .get(socket)
            .map(|entry| entry.outbound.clone())
    }

    /// Sends one command to the running broker.
    ///
    /// # Returns
    ///
    /// `None` when no broker runs (or it stopped before answering).
    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<AbiResult<T>>) -> Command,
    ) -> Option<AbiResult<T>> {
        let tx = self.inner.link.lock().await.as_ref()?.tx.clone();
        let (reply, answer) = oneshot::channel();
        tx.send(command(reply)).ok()?;
        answer.await.ok()
    }

    /// Opens a socket, starting the broker when none runs.
    ///
    /// # Errors
    ///
    /// The broker's refusal or dial failure, or `unavailable` when the
    /// broker cannot be started.
    pub async fn connect(&self, request: SocketConnectRequest) -> AbiResult<SocketInfo> {
        // Checked here too so a full instance is refused without a dial.
        self.inner.check_capacity()?;
        // A broker that is stopping after its last close may drop the
        // command; one retry starts a fresh one.
        for _ in 0..2 {
            let tx = {
                let mut link = self.inner.link.lock().await;
                let live = link.as_ref().is_some_and(|l| !l.tx.is_closed());
                if !live {
                    *link = Some(start(&self.inner).await?);
                }
                link.as_ref().map(|l| l.tx.clone())
            };
            let Some(tx) = tx else { continue };
            let (reply, answer) = oneshot::channel();
            if tx.send(Command::Connect(request.clone(), reply)).is_err() {
                continue;
            }
            if let Ok(outcome) = answer.await {
                return outcome;
            }
        }
        Err(AbiError::unavailable("the socket broker stopped"))
    }

    /// Sends one message on an open socket.
    ///
    /// # Errors
    ///
    /// `not_found` when the socket is not open; the broker's refusal.
    pub async fn send(&self, socket: String, frame: SocketFrame) -> AbiResult<()> {
        if let Some(outbound) = self.local_outbound(&socket) {
            check_socket_frame(&frame)?;
            return outbound
                .send(LocalOutbound::Frame(frame))
                .await
                .map_err(|_| not_open(&socket));
        }
        let missing = not_open(&socket);
        self.ask(|reply| Command::Send(socket, frame, reply))
            .await
            .unwrap_or(Err(missing))
    }

    /// Closes a socket; closing an unknown or closed socket is not an error.
    ///
    /// # Errors
    ///
    /// The broker's refusal of the code or reason.
    pub async fn close(&self, socket: String, code: u16, reason: String) -> AbiResult<()> {
        if let Some(outbound) = self.local_outbound(&socket) {
            let code = check_socket_close(code, &reason)?;
            // A socket the library already closed has nothing to close.
            let _ = outbound.send(LocalOutbound::Close { code, reason }).await;
            return Ok(());
        }
        self.ask(|reply| Command::Close(socket, code, reason, reply))
            .await
            .unwrap_or(Ok(()))
    }

    /// Replaces a socket's attachment.
    ///
    /// # Errors
    ///
    /// `not_found` when the socket is not open; the broker's refusal.
    pub async fn set_attachment(&self, socket: String, attachment: Vec<u8>) -> AbiResult<()> {
        if let Some(entry) = self.inner.local().get_mut(&socket) {
            check_socket_attachment(&attachment)?;
            entry.attachment = attachment;
            return Ok(());
        }
        let missing = not_open(&socket);
        let kept = attachment.clone();
        let id = socket.clone();
        let outcome = self
            .ask(|reply| Command::SetAttachment(socket, attachment, reply))
            .await
            .unwrap_or(Err(missing));
        if outcome.is_ok() {
            if let Some((generation, socket)) = broker_socket(&id) {
                self.inner.record_attachment(generation, socket, &kept);
            }
        }
        outcome
    }

    /// The instance's open sockets, oldest first.
    ///
    /// # Errors
    ///
    /// The broker's failure to answer.
    pub async fn list(&self) -> AbiResult<Vec<SocketInfo>> {
        let mut sockets = self.ask(Command::List).await.unwrap_or(Ok(Vec::new()))?;
        sockets.extend(self.inner.local().iter().map(|(id, entry)| SocketInfo {
            socket: id.clone(),
            url: entry.url.clone(),
            protocol: entry.protocol.clone(),
            attachment: entry.attachment.clone(),
        }));
        Ok(sockets)
    }
}

/// The broker a session may hand its guest as `SOCKETS`.
///
/// # Returns
///
/// `broker` when the manifest declares `[sockets]` and the covering grant
/// has the `sockets` token; `None` otherwise, so the binding is absent.
pub(crate) fn granted(
    broker: Option<SocketBroker>,
    manifest: &crate::PluginManifest,
    grant: &crate::PluginGrant,
) -> Option<SocketBroker> {
    broker
        .filter(|_| manifest.sockets.is_some() && crate::require_binding(grant, "sockets").is_ok())
}

/// Refusal for a socket that is not open.
fn not_open(socket: &str) -> AbiError {
    AbiError::not_found(format!("socket `{socket}` is not open"))
}

/// The spawn plan for a broker: the front door alone, never a native
/// sibling, whatever the plugin's runtime.
///
/// # Errors
///
/// The front-door refusal when `bookclerk-workerd` is missing.
fn broker_plan(plugin: &DiscoveredPlugin) -> crate::Result<SpawnPlan> {
    let plan = SpawnPlan::resolve(plugin, SpawnTransport::WorkerdFrontDoor)?;
    Ok(SpawnPlan {
        args: Vec::new(),
        native_backend: None,
        runtime: GuestRuntimeKind::Workerd,
        ..plan
    })
}

/// Starts a broker process and its link thread.
///
/// # Errors
///
/// `unavailable` with the spawn failure.
async fn start(inner: &Arc<Inner>) -> AbiResult<Link> {
    if !inner.plugin.manifest.capabilities().sockets {
        return Err(AbiError::forbidden("the plugin declares no [sockets]"));
    }
    let unavailable = |err: crate::PluginError| {
        AbiError::unavailable(format!("could not start the socket broker: {err}"))
    };
    let plan = broker_plan(&inner.plugin).map_err(unavailable)?;
    let env: [(&str, OsString); 1] = [(SOCKET_BROKER_ENV, OsString::from("1"))];
    let spawned = crate::spawn_stdio::spawn_stdio_guest(
        &inner.plugin,
        &plan,
        &inner.config,
        serde_json::Value::Null,
        &env,
        None,
        None,
    )
    .await
    .map_err(unavailable)?;
    // The session hands out `SOCKETS` only under the `sockets` grant token;
    // the broker refuses to run without it too.
    if let Err(err) = crate::require_binding(&spawned.grant, "sockets") {
        crate::rpc_session::abandon_spawned(spawned);
        return Err(AbiError::forbidden(err.to_string()));
    }
    // Like a guest session, the broker joins the authority table, so a
    // grant change fences it: its process stops and its sockets are
    // reported lost. A change that landed during the spawn fences it here.
    let identity =
        crate::ExecutorIdentity::from_plugin_with_runtime(&inner.plugin, "", plan.runtime)
            .with_overlay_config(&inner.config)
            .with_persisted_and_effective(&spawned.persisted_grant, &spawned.grant);
    if identity.grant_revision.is_empty() {
        crate::rpc_session::abandon_spawned(spawned);
        return Err(AbiError::unavailable(
            "the socket broker spawn is missing an authority revision",
        ));
    }
    let fenced = Arc::new(tokio::sync::Notify::new());
    let fence = {
        let _epoch = crate::authority::lock_grant_epoch();
        if let Err(err) = crate::rpc_session::grant_still_current(
            &spawned.files_dir,
            &inner.plugin,
            &inner.config,
            &identity,
            None,
        ) {
            spawned.cancel.store(true, Ordering::SeqCst);
            crate::rpc_session::abandon_spawned(spawned);
            return Err(AbiError::forbidden(err.to_string()));
        }
        let hook = Arc::clone(&fenced);
        crate::authority::register_session_revisions_on(
            inner.plugin.plugin_key().canonical(),
            &identity.grant_revision,
            &identity.authority_revision,
            Arc::new(move || hook.notify_one()),
            Arc::clone(&spawned.cancel),
        )
    };
    let generation = inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, rx) = mpsc::unbounded_channel();
    let link = LinkRun {
        generation,
        inner: Arc::downgrade(inner),
        fence,
        fenced,
    };
    let name = format!("bc-sockets-{}", inner.plugin.manifest.id);
    let name: String = name.chars().take(15).collect();
    std::thread::Builder::new()
        .name(name)
        .spawn(move || run_link(spawned, rx, link))
        .map_err(|err| AbiError::unavailable(format!("socket broker thread: {err}")))?;
    Ok(Link { tx })
}

/// What one broker's link thread runs with.
struct LinkRun {
    /// The broker's generation, tagging the sockets it opens.
    generation: u64,
    /// Shared broker state; weak so the link never keeps a dropped broker
    /// alive.
    inner: Weak<Inner>,
    /// The broker's cancel flag in the authority table, unregistered when
    /// the link ends.
    fence: Arc<AtomicBool>,
    /// Woken when a grant change fences the broker.
    fenced: Arc<tokio::sync::Notify>,
}

/// The broker's stdio, shared by the link's reader and writer halves so
/// the spawn record stays whole for cleanup.
#[derive(Clone)]
struct Pipes(Rc<RefCell<SpawnedStdio>>);

impl AsyncRead for Pipes {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut spawned = self.0.borrow_mut();
        Pin::new(&mut spawned.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for Pipes {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut spawned = self.0.borrow_mut();
        Pin::new(&mut spawned.stdin).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut spawned = self.0.borrow_mut();
        Pin::new(&mut spawned.stdin).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut spawned = self.0.borrow_mut();
        Pin::new(&mut spawned.stdin).poll_shutdown(cx)
    }
}

/// The host's `SocketHandler` on the link: forwards to the sink.
struct HostHandler {
    /// Shared broker state; weak so the link never keeps a dropped
    /// broker alive.
    inner: Weak<Inner>,
    /// The broker's generation.
    generation: u64,
    /// Set when the last open socket closed, so the link stops the broker.
    idle: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait(?Send)]
impl SocketHandler for HostHandler {
    async fn message(&self, event: SocketMessage) -> AbiResult<SocketMessageOk> {
        {
            let Some(inner) = self.inner.upgrade() else {
                return Ok(SocketMessageOk::default());
            };
            inner.record_attachment(self.generation, &event.socket, &event.attachment);
        }
        let mut event = event;
        event.socket = guest_socket(self.generation, &event.socket);
        deliver(&self.inner, SocketEvent::Message(event)).await;
        // The broker has no events to poll; the guest's own ask travels on
        // the host's delivery to it (see `SocketSink`).
        Ok(SocketMessageOk::default())
    }

    async fn closed(&self, event: SocketClosed) -> AbiResult<()> {
        let now_idle = {
            let Some(inner) = self.inner.upgrade() else {
                return Ok(());
            };
            inner.record_closed(self.generation, &event.socket)
        };
        let mut event = event;
        event.socket = guest_socket(self.generation, &event.socket);
        deliver(&self.inner, SocketEvent::Closed(event)).await;
        if now_idle {
            self.idle.notify_one();
        }
        Ok(())
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Ends any wait for a sink that will now never come.
        self.sink_ready.notify_waiters();
    }
}

/// The instance's sink, once the deployment installed it.
///
/// A guest can open a socket while it starts, before the deployment has
/// the proxy that takes events. The event then waits, holding only a weak
/// reference, until the sink arrives or the deployment gives the broker
/// up (a failed start drops every handle). Meanwhile the broker's delivery
/// queue holds the socket's later messages and its reads pause, so nothing
/// is dropped.
///
/// # Returns
///
/// The sink; `None` when the broker is gone or its guest proxy retired.
async fn sink_when_ready(inner: &Weak<Inner>) -> Option<Arc<dyn SocketSink>> {
    let ready = Arc::clone(&inner.upgrade()?.sink_ready);
    loop {
        let installed = ready.notified();
        tokio::pin!(installed);
        installed.as_mut().enable();
        {
            let inner = inner.upgrade()?;
            if inner.sink.get().is_some() {
                return inner.sink();
            }
        }
        installed.await;
    }
}

/// Hands one event to the sink, logging a failure.
async fn deliver(inner: &Weak<Inner>, event: SocketEvent) {
    let plugin = match inner.upgrade() {
        Some(inner) => inner.plugin.manifest.id.clone(),
        None => return,
    };
    let Some(sink) = sink_when_ready(inner).await else {
        tracing::debug!(%plugin, "socket event with no guest to take it");
        return;
    };
    if let Err(err) = sink.deliver_socket_event(event).await {
        tracing::warn!(%plugin, error = %err, "a plugin did not take a socket event");
    }
}

/// The broker link thread: serves commands until the broker idles, dies,
/// is fenced, or every handle is gone, then tears the process down.
fn run_link(spawned: SpawnedStdio, mut rx: mpsc::UnboundedReceiver<Command>, link: LinkRun) {
    let LinkRun {
        generation,
        inner,
        fence,
        fenced,
    } = link;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::warn!(error = %err, "socket broker runtime");
            crate::authority::unregister_session(&fence);
            crate::rpc_session::abandon_spawned(spawned);
            report_lost(&inner, generation);
            return;
        }
    };
    let pipes = Pipes(Rc::new(RefCell::new(spawned)));
    let held = pipes.clone();
    let after = inner.clone();
    let lost = runtime.block_on(async move {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let idle = Arc::new(tokio::sync::Notify::new());
                let handler: Arc<dyn SocketHandler> = Arc::new(HostHandler {
                    inner: inner.clone(),
                    generation,
                    idle: Arc::clone(&idle),
                });
                let (sockets, rpc) =
                    bookclerk_plugin_abi::connect_socket_broker(pipes.clone(), pipes, handler);
                let mut rpc = tokio::task::spawn_local(rpc);
                let sockets = Rc::new(sockets);
                let mut ended_by_broker = false;
                loop {
                    tokio::select! {
                        command = rx.recv() => match command {
                            Some(command) => {
                                tokio::task::spawn_local(serve(
                                    Rc::clone(&sockets),
                                    inner.clone(),
                                    generation,
                                    command,
                                ));
                            }
                            None => break,
                        },
                        () = idle.notified() => {
                            let still_idle = inner
                                .upgrade()
                                .is_none_or(|inner| !inner.holds(generation));
                            if still_idle {
                                break;
                            }
                        }
                        () = fenced.notified() => {
                            ended_by_broker = true;
                            break;
                        }
                        _ = &mut rpc => {
                            ended_by_broker = true;
                            break;
                        }
                    }
                }
                rx.close();
                drop(sockets);
                rpc.abort();
                ended_by_broker
            })
            .await
    });
    drop(runtime);
    crate::authority::unregister_session(&fence);
    if let Ok(cell) = Rc::try_unwrap(held.0) {
        crate::rpc_session::abandon_spawned(cell.into_inner());
    }
    if lost {
        report_lost(&after, generation);
    } else if let Some(inner) = after.upgrade() {
        // An idle or dropped broker holds nothing; forget its early closes.
        let _ = inner.take_generation(generation);
    }
}

/// Reports every socket broker `generation` held as closed with 1006,
/// after the broker process died or was fenced under them. Sockets a
/// replacement broker opened meanwhile are not touched.
fn report_lost(inner: &Weak<Inner>, generation: u64) {
    let lost = match inner.upgrade() {
        Some(inner) => inner.take_generation(generation),
        None => return,
    };
    if lost.is_empty() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        for (socket, attachment) in lost {
            let event = SocketClosed {
                socket: guest_socket(generation, &socket),
                code: CLOSE_ABNORMAL,
                reason: Some("the socket broker stopped".to_string()),
                clean: false,
                attachment,
            };
            deliver(inner, SocketEvent::Closed(event)).await;
        }
    });
}

/// Runs one command against the broker's `Sockets`, recording a socket
/// it opened before answering the caller.
async fn serve(
    sockets: Rc<bookclerk_plugin_abi::SocketsClient>,
    inner: Weak<Inner>,
    generation: u64,
    command: Command,
) {
    // The broker's id for a guest's socket, when this broker holds it; a
    // socket of an earlier broker is no longer open.
    let held = |guest: &str| -> Option<String> {
        broker_socket(guest)
            .filter(|(owner, _)| *owner == generation)
            .map(|(_, socket)| socket.to_string())
    };
    let to_guest = |mut info: SocketInfo| {
        info.socket = guest_socket(generation, &info.socket);
        info
    };
    match command {
        Command::Connect(request, reply) => {
            let outcome = sockets.connect(request).await;
            if let (Ok(info), Some(inner)) = (&outcome, inner.upgrade()) {
                inner.record_open(generation, info);
            }
            let _ = reply.send(outcome.map(to_guest));
        }
        Command::Send(guest, frame, reply) => {
            let outcome = match held(&guest) {
                Some(socket) => sockets.send(socket, frame).await,
                None => Err(not_open(&guest)),
            };
            let _ = reply.send(outcome);
        }
        Command::Close(guest, code, reason, reply) => {
            let outcome = match held(&guest) {
                Some(socket) => sockets.close(socket, code, reason).await,
                None => Ok(()),
            };
            let _ = reply.send(outcome);
        }
        Command::SetAttachment(guest, attachment, reply) => {
            let outcome = match held(&guest) {
                Some(socket) => sockets.set_attachment(socket, attachment).await,
                None => Err(not_open(&guest)),
            };
            let _ = reply.send(outcome);
        }
        Command::List(reply) => {
            let outcome = sockets
                .list()
                .await
                .map(|infos| infos.into_iter().map(to_guest).collect());
            let _ = reply.send(outcome);
        }
    }
}

/// Delivers what the library sends on local socket `id` until it closes,
/// then forgets the socket and delivers its close.
///
/// Runs on the library's runtime, one event at a time, so the guest sees
/// messages in order; the library stops reading while a delivery waits.
async fn pump_local(inner: Weak<Inner>, id: String, mut inbound: mpsc::Receiver<LocalInbound>) {
    loop {
        let event = match inbound.recv().await {
            Some(LocalInbound::Frame(frame)) => {
                let Some(attachment) = inner
                    .upgrade()
                    .and_then(|inner| inner.local().get(&id).map(|e| e.attachment.clone()))
                else {
                    return;
                };
                SocketEvent::Message(SocketMessage {
                    socket: id.clone(),
                    frame,
                    attachment,
                })
            }
            closed => {
                let (code, reason, clean) = match closed {
                    Some(LocalInbound::Closed {
                        code,
                        reason,
                        clean,
                    }) => (code, reason, clean),
                    _ => (CLOSE_ABNORMAL, None, false),
                };
                let Some(entry) = inner.upgrade().and_then(|inner| inner.local().remove(&id))
                else {
                    return;
                };
                deliver(
                    &inner,
                    SocketEvent::Closed(SocketClosed {
                        socket: id,
                        code,
                        reason,
                        clean,
                        attachment: entry.attachment,
                    }),
                )
                .await;
                return;
            }
        };
        deliver(&inner, event).await;
    }
}

/// The id a guest sees for the host's `n`th local socket. It has no `_`,
/// so [`broker_socket`] never reads it as a broker's.
fn local_socket(n: u64) -> String {
    format!("o{n}")
}

/// The id a guest sees for broker `generation`'s socket `socket`.
///
/// Each broker mints its own ids, and two brokers' ids can match; the
/// generation prefix keeps every id the guest sees unique for the
/// instance's life, as the ABI promises. Broker ids are at most 30 bytes,
/// so the result stays within the 64-byte id limit.
fn guest_socket(generation: u64, socket: &str) -> String {
    format!("g{generation}_{socket}")
}

/// Splits a guest-visible socket id into its broker generation and the
/// broker's own id; `None` for an id this host never minted.
fn broker_socket(guest: &str) -> Option<(u64, &str)> {
    let (generation, socket) = guest.strip_prefix('g')?.split_once('_')?;
    let generation = generation.parse().ok()?;
    (!socket.is_empty()).then_some((generation, socket))
}

/// The `SOCKETS` binding a guest session hands out: the instance's broker.
pub struct BrokerSockets {
    /// The instance's broker.
    broker: SocketBroker,
}

impl BrokerSockets {
    /// The binding for `broker`.
    #[must_use]
    pub fn new(broker: SocketBroker) -> Self {
        Self { broker }
    }
}

#[async_trait::async_trait(?Send)]
impl Sockets for BrokerSockets {
    async fn connect(&self, request: SocketConnectRequest) -> AbiResult<SocketInfo> {
        self.broker.connect(request).await
    }

    async fn send(&self, socket: String, frame: SocketFrame) -> AbiResult<()> {
        self.broker.send(socket, frame).await
    }

    async fn close(&self, socket: String, code: u16, reason: String) -> AbiResult<()> {
        self.broker.close(socket, code, reason).await
    }

    async fn set_attachment(&self, socket: String, attachment: Vec<u8>) -> AbiResult<()> {
        self.broker.set_attachment(socket, attachment).await
    }

    async fn list(&self) -> AbiResult<Vec<SocketInfo>> {
        self.broker.list().await
    }
}

#[cfg(test)]
#[path = "socket_broker_tests.rs"]
pub(crate) mod tests;
