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
use bookclerk_plugin_abi::{
    PluginError as AbiError, Result as AbiResult, SocketClosed, SocketConnectRequest, SocketFrame,
    SocketHandler, SocketInfo, SocketMessage, Sockets,
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
    /// # Errors
    ///
    /// Returns the wake or delivery failure as text. The broker logs it and
    /// carries on: a socket is not closed because one delivery failed.
    async fn deliver_socket_event(&self, event: SocketEvent) -> std::result::Result<(), String>;
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
    /// Network authority from the instance document (see
    /// [`crate::SessionServices::deployed_endpoint`]).
    deployed_endpoint: Option<Option<String>>,
    /// Where events go; set once the instance's guest proxy exists.
    sink: OnceLock<Weak<dyn SocketSink>>,
    /// The running broker, if any. Held across a connect so two connects
    /// do not start two brokers.
    link: tokio::sync::Mutex<Option<Link>>,
    /// Open sockets, so the host can report them closed if their broker
    /// dies.
    open: Mutex<BTreeMap<String, Opened>>,
    /// Sockets whose close reached the host before their connect answer
    /// did, by broker generation, so the late answer does not resurrect
    /// them in `open`. Lock after `open`.
    closed_early: Mutex<BTreeSet<(u64, String)>>,
    /// The last broker generation started; each broker process gets the
    /// next one.
    generation: AtomicU64,
    /// Woken when the sink is installed and when this state is dropped,
    /// so an event waiting for a sink ends either way.
    sink_ready: Arc<tokio::sync::Notify>,
}

/// One socket the host believes open.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Opened {
    /// The broker generation holding it; only that broker's loss reports
    /// it.
    generation: u64,
    /// The attachment the host last saw.
    attachment: Vec<u8>,
}

impl Inner {
    /// Locks the open-socket mirror; a poisoned lock still holds a map.
    fn open(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Opened>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Locks the set of sockets closed before their connect was recorded.
    fn closed_early(&self) -> std::sync::MutexGuard<'_, BTreeSet<(u64, String)>> {
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
        let mut open = self.open();
        if !self
            .closed_early()
            .remove(&(generation, info.socket.clone()))
        {
            let opened = Opened {
                generation,
                attachment: info.attachment.clone(),
            };
            open.insert(info.socket.clone(), opened);
        }
    }

    /// Records a socket broker `generation` closed.
    ///
    /// # Returns
    ///
    /// Whether that broker holds no open socket any more.
    fn record_closed(&self, generation: u64, socket: &str) -> bool {
        let mut open = self.open();
        match open.get(socket) {
            Some(opened) if opened.generation == generation => {
                open.remove(socket);
            }
            Some(_) => {}
            None => {
                self.closed_early().insert((generation, socket.to_string()));
            }
        }
        !open.values().any(|opened| opened.generation == generation)
    }

    /// Whether broker `generation` holds an open socket.
    fn holds(&self, generation: u64) -> bool {
        self.open()
            .values()
            .any(|opened| opened.generation == generation)
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
        let lost: Vec<String> = open
            .iter()
            .filter(|(_, opened)| opened.generation == generation)
            .map(|(socket, _)| socket.clone())
            .collect();
        lost.into_iter()
            .filter_map(|socket| {
                let opened = open.remove(&socket)?;
                Some((socket, opened.attachment))
            })
            .collect()
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
    /// * `deployed_endpoint` - The instance document's network authority,
    ///   as the guest's own spawn receives it.
    #[must_use]
    pub fn new(
        plugin: DiscoveredPlugin,
        config: Config,
        deployed_endpoint: Option<Option<String>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                plugin,
                config,
                deployed_endpoint,
                sink: OnceLock::new(),
                link: tokio::sync::Mutex::new(None),
                open: Mutex::new(BTreeMap::new()),
                closed_early: Mutex::new(BTreeSet::new()),
                generation: AtomicU64::new(0),
                sink_ready: Arc::new(tokio::sync::Notify::new()),
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

    /// Ids of the sockets the host believes open, oldest id first.
    #[must_use]
    pub fn open_sockets(&self) -> Vec<String> {
        self.inner.open().keys().cloned().collect()
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
        let missing = not_open(&socket);
        let kept = attachment.clone();
        let id = socket.clone();
        let outcome = self
            .ask(|reply| Command::SetAttachment(socket, attachment, reply))
            .await
            .unwrap_or(Err(missing));
        if outcome.is_ok() {
            if let Some(opened) = self.inner.open().get_mut(&id) {
                opened.attachment = kept;
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
        self.ask(Command::List).await.unwrap_or(Ok(Vec::new()))
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
        inner.deployed_endpoint.as_ref().map(|e| e.as_deref()),
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
            inner.deployed_endpoint.as_ref().map(|e| e.as_deref()),
        ) {
            spawned.cancel.store(true, Ordering::SeqCst);
            crate::rpc_session::abandon_spawned(spawned);
            return Err(AbiError::forbidden(err.to_string()));
        }
        let hook = Arc::clone(&fenced);
        crate::authority::register_session_revisions_on_with_deployed_endpoint(
            inner.plugin.plugin_key().canonical(),
            &identity.grant_revision,
            &identity.authority_revision,
            Arc::new(move || hook.notify_one()),
            Arc::clone(&spawned.cancel),
            inner.deployed_endpoint.clone(),
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
    async fn message(&self, event: SocketMessage) -> AbiResult<()> {
        {
            let Some(inner) = self.inner.upgrade() else {
                return Ok(());
            };
            let mut open = inner.open();
            if let Some(opened) = open.get_mut(&event.socket) {
                if opened.generation == self.generation {
                    opened.attachment.clone_from(&event.attachment);
                }
            }
        }
        deliver(&self.inner, SocketEvent::Message(event)).await;
        Ok(())
    }

    async fn closed(&self, event: SocketClosed) -> AbiResult<()> {
        let now_idle = {
            let Some(inner) = self.inner.upgrade() else {
                return Ok(());
            };
            inner.record_closed(self.generation, &event.socket)
        };
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
                socket,
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
    match command {
        Command::Connect(request, reply) => {
            let outcome = sockets.connect(request).await;
            if let (Ok(info), Some(inner)) = (&outcome, inner.upgrade()) {
                inner.record_open(generation, info);
            }
            let _ = reply.send(outcome);
        }
        Command::Send(socket, frame, reply) => {
            let _ = reply.send(sockets.send(socket, frame).await);
        }
        Command::Close(socket, code, reason, reply) => {
            let _ = reply.send(sockets.close(socket, code, reason).await);
        }
        Command::SetAttachment(socket, attachment, reply) => {
            let _ = reply.send(sockets.set_attachment(socket, attachment).await);
        }
        Command::List(reply) => {
            let _ = reply.send(sockets.list().await);
        }
    }
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
mod tests;
