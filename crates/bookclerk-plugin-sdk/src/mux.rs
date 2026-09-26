//! Bidirectional multiplexed byte streams with per-stream credit windows.
//!
//! Used for the native-behind-workerd socket proxy (guest [`Mux::open`],
//! gateway [`Mux::accept`]) and the OAuth callback tunnel (host opens, guest
//! accepts). Either side may initiate; connection ids from a client are odd
//! and from a server are even so they cannot collide.
//!
//! The peer is untrusted. Frame length is rejected before a payload buffer is
//! allocated. Each stream buffers at most [`MAX_BUFFERED_PER_STREAM`] unread
//! bytes, the connection buffers at most [`MAX_AGGREGATE_BUFFERED`], and at
//! most [`MAX_LIVE_STREAMS`] stream objects exist. That cap covers streams
//! waiting in the accept queue and streams the peer has already closed: a
//! `Close` frame does not free the slot while the [`MuxStream`] is still
//! alive. Each slot has a generation so dropping an old stream cannot remove
//! a replacement that reused the id. Window updates saturate at
//! [`INITIAL_WINDOW`]. Data frames use a bounded queue; `Window` and `Close`
//! travel on a separate queue that the writer drains first.
//!
//! Frame layout (big-endian):
//! ```text
//! u32 length_of_rest | u8 type | u32 conn_id | payload
//! ```
//! Types: `Open=1`, `Data=2`, `Close=3`, `Window=4`. Data payloads are capped
//! at 1 MiB. A `Window` frame carries a `u32` credit (bytes the peer may
//! send). Each stream starts with [`INITIAL_WINDOW`] bytes of send credit.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;

use crate::{Result, SdkError};

/// Frame type byte for opening a multiplexed connection.
pub const TYPE_OPEN: u8 = 1;
/// Frame type byte for payload bytes on an open connection.
pub const TYPE_DATA: u8 = 2;
/// Frame type byte for closing a multiplexed connection.
pub const TYPE_CLOSE: u8 = 3;
/// Frame type byte for a credit-window update.
pub const TYPE_WINDOW: u8 = 4;
/// Maximum Data-frame payload in bytes (1 MiB); larger writes are split.
pub const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;
/// Initial per-stream send credit, in bytes.
pub const INITIAL_WINDOW: u32 = 64 * 1024;
/// Unread bytes kept for one stream. Further Data is discarded and the stream
/// is closed so the reader can still take `Window` and `Close` for others.
pub const MAX_BUFFERED_PER_STREAM: usize = INITIAL_WINDOW as usize;
/// Unread bytes kept across every stream on one mux.
pub const MAX_AGGREGATE_BUFFERED: usize = 1024 * 1024;
/// Live local and peer streams, including those not yet accepted.
pub const MAX_LIVE_STREAMS: usize = 32;
/// Outbound Data frames waiting on the writer. The peer not reading cannot
/// grow this queue.
const WRITER_DATA_QUEUE: usize = 4;
/// Outbound Open, Window, and Close frames. Drained before Data.
const WRITER_CONTROL_QUEUE: usize = 64;

/// Client ids are odd (`1, 3, 5, …`).
const CLIENT_ID_BASE: u32 = 1;
/// Server ids are even (`2, 4, 6, …`).
const SERVER_ID_BASE: u32 = 2;

/// Control frames. These are not queued behind Data.
enum Control {
    Open(u32),
    Close(u32),
    Window(u32, u32),
}

/// One outbound Data-queue item. `Close` stays behind `Bytes` already queued
/// for this stream; `Window` uses the control queue so it is not blocked.
enum OutData {
    Bytes { id: u32, payload: Vec<u8> },
    Close(u32),
}

/// Shared tables for the reader task and every [`MuxStream`].
struct Shared {
    map: Mutex<HashMap<u32, Slot>>,
    /// Next generation handed out by [`Shared::try_insert`].
    next_generation: AtomicU64,
    aggregate: AtomicUsize,
    control_tx: mpsc::Sender<Control>,
    data_tx: mpsc::Sender<OutData>,
    /// Writers parked because the Data queue is full.
    data_wakers: Arc<Mutex<Vec<Waker>>>,
}

/// Per-connection inbound slot used by the reader task.
///
/// The slot stays in the map until the [`MuxStream`] that owns it is dropped.
/// `data_tx` is taken when the peer closes so the stream observes EOF without
/// freeing the id for reuse.
struct Slot {
    generation: u64,
    data_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    buffered: Arc<AtomicUsize>,
    send_credit: Arc<AtomicU32>,
    in_flight: Arc<AtomicU32>,
    send_waker: Arc<Mutex<Option<Waker>>>,
    peer_gone: Arc<AtomicBool>,
}

/// One multiplexed endpoint. Cheap to clone (shared tasks + id allocator).
#[derive(Clone)]
pub struct Mux {
    shared: Arc<Shared>,
    next_id: Arc<AtomicU32>,
    accept_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<MuxStream>>>,
    _tasks: Arc<Vec<tokio::task::JoinHandle<()>>>,
}

/// One multiplexed logical connection implementing `AsyncRead` + `AsyncWrite`.
pub struct MuxStream {
    id: u32,
    /// Generation stored in the map when this stream was inserted.
    generation: u64,
    shared: Arc<Shared>,
    data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    read_buf: Vec<u8>,
    send_credit: Arc<AtomicU32>,
    in_flight: Arc<AtomicU32>,
    send_waker: Arc<Mutex<Option<Waker>>>,
    buffered: Arc<AtomicUsize>,
    peer_gone: Arc<AtomicBool>,
    /// `Open` was queued, so drop must send `Close`.
    announced: bool,
    /// Slot was inserted, so drop must remove it.
    inserted: bool,
    closed: bool,
}

impl MuxStream {
    /// Stable connection id shared with the peer for this logical stream.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }
}

impl Mux {
    /// Client endpoint (odd connection ids). Socket-proxy guests and OAuth hosts.
    pub fn client<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::new(reader, writer, CLIENT_ID_BASE)
    }

    /// Server endpoint (even connection ids). Socket-proxy gateway and OAuth guests.
    pub fn server<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::new(reader, writer, SERVER_ID_BASE)
    }

    fn new<R, W>(reader: R, writer: W, id_base: u32) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (control_tx, control_rx) = mpsc::channel(WRITER_CONTROL_QUEUE);
        let (data_tx, data_rx) = mpsc::channel(WRITER_DATA_QUEUE);
        let (accept_tx, accept_rx) = mpsc::unbounded_channel();
        let data_wakers = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Shared {
            map: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            aggregate: AtomicUsize::new(0),
            control_tx: control_tx.clone(),
            data_tx: data_tx.clone(),
            data_wakers: Arc::clone(&data_wakers),
        });
        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(writer_task(
            writer,
            control_rx,
            data_rx,
            data_wakers,
        )));
        let reader_shared = Arc::clone(&shared);
        tasks.push(tokio::spawn(async move {
            if let Err(err) =
                reader_task(reader, Arc::clone(&reader_shared), accept_tx, id_base).await
            {
                tracing::debug!(error = %err, "mux reader stopped");
            }
            reader_shared.close_all_peers();
        }));
        Self {
            shared,
            next_id: Arc::new(AtomicU32::new(id_base)),
            accept_rx: Arc::new(tokio::sync::Mutex::new(accept_rx)),
            _tasks: Arc::new(tasks),
        }
    }

    /// Open a logical stream toward the peer.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the writer task has exited or the live-stream
    /// cap is already full.
    pub async fn open(&self) -> Result<MuxStream> {
        let id = self.next_id.fetch_add(2, Ordering::Relaxed);
        let (mut stream, slot) = MuxStream::pair(id, Arc::clone(&self.shared));
        let Some(generation) = self.shared.try_insert(id, slot) else {
            return Err(SdkError::message("mux stream cap"));
        };
        stream.generation = generation;
        stream.inserted = true;
        self.shared
            .control_tx
            .send(Control::Open(id))
            .await
            .map_err(|_| SdkError::message("mux writer gone"))?;
        stream.announced = true;
        Ok(stream)
    }

    /// Wait for the next logical stream opened by the peer.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the reader task exits (peer hangup or bad frame).
    pub async fn accept(&self) -> Result<MuxStream> {
        self.accept_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| SdkError::message("mux closed"))
    }
}

impl Shared {
    /// Insert `slot` unless `id` is taken or the live-object cap is full.
    ///
    /// The returned generation is stored on the [`MuxStream`]. A later drop
    /// removes the map entry only when it still carries this generation.
    fn try_insert(&self, id: u32, mut slot: Slot) -> Option<u64> {
        let mut map = lock_map(&self.map);
        if map.len() >= MAX_LIVE_STREAMS || map.contains_key(&id) {
            return None;
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        slot.generation = generation;
        map.insert(id, slot);
        Some(generation)
    }

    /// End inbound delivery for `id` without freeing the stream object.
    ///
    /// The peer may reuse an id only after the [`MuxStream`] drops and
    /// [`Self::release_stream`] removes a matching generation.
    fn close_peer(&self, id: u32) {
        let mut map = lock_map(&self.map);
        let Some(slot) = map.get_mut(&id) else {
            return;
        };
        slot.peer_gone.store(true, Ordering::SeqCst);
        slot.data_tx.take();
        let waker = lock_waker(&slot.send_waker).take();
        drop(map);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn close_all_peers(&self) {
        let ids: Vec<u32> = lock_map(&self.map).keys().copied().collect();
        for id in ids {
            self.close_peer(id);
        }
    }

    /// Remove the slot only when it is still the generation `stream` inserted.
    fn release_stream(&self, id: u32, generation: u64) {
        let mut map = lock_map(&self.map);
        let matches = map
            .get(&id)
            .is_some_and(|slot| slot.generation == generation);
        if !matches {
            return;
        }
        if let Some(slot) = map.remove(&id) {
            slot.peer_gone.store(true, Ordering::SeqCst);
        }
    }
}

impl MuxStream {
    fn pair(id: u32, shared: Arc<Shared>) -> (Self, Slot) {
        let (inbound_tx, data_rx) = mpsc::unbounded_channel();
        let send_credit = Arc::new(AtomicU32::new(INITIAL_WINDOW));
        let in_flight = Arc::new(AtomicU32::new(0));
        let send_waker = Arc::new(Mutex::new(None));
        let buffered = Arc::new(AtomicUsize::new(0));
        let peer_gone = Arc::new(AtomicBool::new(false));
        let stream = Self {
            id,
            generation: 0,
            shared,
            data_rx,
            read_buf: Vec::new(),
            send_credit: Arc::clone(&send_credit),
            in_flight: Arc::clone(&in_flight),
            send_waker: Arc::clone(&send_waker),
            buffered: Arc::clone(&buffered),
            peer_gone: Arc::clone(&peer_gone),
            announced: false,
            inserted: false,
            closed: false,
        };
        (
            stream,
            Slot {
                generation: 0,
                data_tx: Some(inbound_tx),
                buffered,
                send_credit,
                in_flight,
                send_waker,
                peer_gone,
            },
        )
    }

    fn grant_window(&self, n: u32) {
        if n == 0 || self.closed {
            return;
        }
        let _ = self.shared.control_tx.try_send(Control::Window(self.id, n));
    }

    fn note_consumed(&self, n: usize) {
        if n == 0 {
            return;
        }
        sub_atomic(&self.buffered, n);
        sub_atomic(&self.shared.aggregate, n);
        self.grant_window(u32::try_from(n).unwrap_or(u32::MAX));
    }
}

impl Drop for MuxStream {
    fn drop(&mut self) {
        let left = self.buffered.swap(0, Ordering::AcqRel);
        sub_atomic(&self.shared.aggregate, left);
        if self.inserted {
            self.shared.release_stream(self.id, self.generation);
        }
        if self.announced && !self.closed {
            self.queue_close();
            self.closed = true;
        }
    }
}

impl MuxStream {
    fn queue_close(&self) {
        if self
            .shared
            .data_tx
            .try_send(OutData::Close(self.id))
            .is_err()
        {
            // The data queue is full of another stream's frames. `Close` still
            // has the control path so it is not stuck behind that unread data.
            let _ = self.shared.control_tx.try_send(Control::Close(self.id));
        }
    }
}

fn lock_map(map: &Mutex<HashMap<u32, Slot>>) -> std::sync::MutexGuard<'_, HashMap<u32, Slot>> {
    map.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_waker(waker: &Mutex<Option<Waker>>) -> std::sync::MutexGuard<'_, Option<Waker>> {
    waker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn sub_atomic(counter: &AtomicUsize, n: usize) {
    if n == 0 {
        return;
    }
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
        Some(cur.saturating_sub(n))
    });
}

/// Returns send credit only for bytes the peer could have consumed.
///
/// `add` is capped by in-flight bytes and the result never exceeds
/// [`INITIAL_WINDOW`]. A window that arrives before those bytes were sent, or
/// a second window for the same bytes, does not raise the allowance.
fn grant_send_credit(credit: &AtomicU32, in_flight: &AtomicU32, add: u32) {
    if add == 0 {
        return;
    }
    let grant = loop {
        let cur = in_flight.load(Ordering::Acquire);
        let take = cur.min(add);
        if take == 0 {
            return;
        }
        if in_flight
            .compare_exchange(cur, cur - take, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break take;
        }
    };
    let _ = credit.fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
        let next = cur.saturating_add(grant).min(INITIAL_WINDOW);
        (next != cur).then_some(next)
    });
}

fn peer_id_allowed(local_base: u32, id: u32) -> bool {
    if id == 0 {
        return false;
    }
    (local_base % 2 == 1) != (id % 2 == 1)
}

struct Header {
    typ: u8,
    conn_id: u32,
    payload_len: usize,
}

/// Reads the length and the fixed header. An illegal length returns before any
/// payload buffer is allocated.
///
/// # Errors
///
/// Returns [`SdkError`] when the peer closes early, the read fails, or the
/// length is shorter than the fixed header or longer than [`MAX_FRAME_PAYLOAD`].
async fn read_header<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Header> {
    let len = reader.read_u32().await.map_err(io_err)?;
    if len < 5 || (len as usize) > MAX_FRAME_PAYLOAD + 5 {
        return Err(SdkError::message(format!("mux bad frame length {len}")));
    }
    let mut head = [0u8; 5];
    reader.read_exact(&mut head).await.map_err(io_err)?;
    let conn_id = u32::from_be_bytes([head[1], head[2], head[3], head[4]]);
    Ok(Header {
        typ: head[0],
        conn_id,
        payload_len: len as usize - 5,
    })
}

/// Consumes `n` payload bytes through a fixed stack buffer.
///
/// # Errors
///
/// Returns [`SdkError`] when the peer closes before `n` bytes arrive or the
/// read fails.
async fn discard<R: AsyncRead + Unpin>(reader: &mut R, mut n: usize) -> Result<()> {
    let mut buf = [0u8; 8192];
    while n > 0 {
        let chunk = n.min(buf.len());
        reader.read_exact(&mut buf[..chunk]).await.map_err(io_err)?;
        n -= chunk;
    }
    Ok(())
}

/// Demultiplexes frames until the peer closes or a frame is rejected.
///
/// # Errors
///
/// Returns [`SdkError`] when a frame header or payload cannot be read, or when
/// an Open uses a duplicate id, the wrong parity, or a closed accept channel.
async fn reader_task<R: AsyncRead + Unpin>(
    mut reader: R,
    shared: Arc<Shared>,
    accept_tx: mpsc::UnboundedSender<MuxStream>,
    id_base: u32,
) -> Result<()> {
    loop {
        let header = read_header(&mut reader).await?;
        match header.typ {
            TYPE_OPEN => {
                if header.payload_len != 0 || !peer_id_allowed(id_base, header.conn_id) {
                    return Err(SdkError::message("mux rejected open"));
                }
                if lock_map(&shared.map).contains_key(&header.conn_id) {
                    return Err(SdkError::message("mux duplicate stream id"));
                }
                let (mut stream, slot) = MuxStream::pair(header.conn_id, Arc::clone(&shared));
                let Some(generation) = shared.try_insert(header.conn_id, slot) else {
                    // Cap is full or the id is still owned by a stream object.
                    // Refuse without blocking the reader on the accept queue.
                    let _ = shared.control_tx.try_send(Control::Close(header.conn_id));
                    continue;
                };
                stream.generation = generation;
                stream.inserted = true;
                stream.announced = true;
                if accept_tx.send(stream).is_err() {
                    return Err(SdkError::message("mux accept closed"));
                }
            }
            TYPE_DATA => {
                if !admit_data(&shared, header.conn_id, header.payload_len) {
                    discard(&mut reader, header.payload_len).await?;
                    shared.close_peer(header.conn_id);
                    continue;
                }
                let mut payload = vec![0u8; header.payload_len];
                reader.read_exact(&mut payload).await.map_err(io_err)?;
                enqueue_data(&shared, header.conn_id, payload);
            }
            TYPE_CLOSE => {
                if header.payload_len != 0 {
                    discard(&mut reader, header.payload_len).await?;
                }
                shared.close_peer(header.conn_id);
            }
            TYPE_WINDOW => {
                if header.payload_len != 4 {
                    return Err(SdkError::message("mux window payload must be 4 bytes"));
                }
                let mut bytes = [0u8; 4];
                reader.read_exact(&mut bytes).await.map_err(io_err)?;
                let credit = u32::from_be_bytes(bytes);
                let map = lock_map(&shared.map);
                if let Some(slot) = map.get(&header.conn_id) {
                    grant_send_credit(&slot.send_credit, &slot.in_flight, credit);
                    if let Some(waker) = lock_waker(&slot.send_waker).take() {
                        waker.wake();
                    }
                }
            }
            other => {
                return Err(SdkError::message(format!("mux unknown frame type {other}")));
            }
        }
    }
}

/// True when `payload_len` fits the per-stream and aggregate caps.
///
/// A payload larger than the per-stream window is refused here, before the
/// caller allocates it.
fn admit_data(shared: &Shared, id: u32, payload_len: usize) -> bool {
    if payload_len == 0 {
        return true;
    }
    if payload_len > MAX_BUFFERED_PER_STREAM {
        return false;
    }
    let map = lock_map(&shared.map);
    let Some(slot) = map.get(&id) else {
        return false;
    };
    if slot.peer_gone.load(Ordering::Acquire) || slot.data_tx.is_none() {
        return false;
    }
    let cur = slot.buffered.load(Ordering::Acquire);
    let agg = shared.aggregate.load(Ordering::Acquire);
    cur.saturating_add(payload_len) <= MAX_BUFFERED_PER_STREAM
        && agg.saturating_add(payload_len) <= MAX_AGGREGATE_BUFFERED
}

fn enqueue_data(shared: &Shared, id: u32, payload: Vec<u8>) {
    if payload.is_empty() {
        return;
    }
    let len = payload.len();
    let map = lock_map(&shared.map);
    let Some(slot) = map.get(&id) else {
        return;
    };
    if slot.peer_gone.load(Ordering::Acquire) || slot.data_tx.is_none() {
        return;
    }
    let cur = slot.buffered.load(Ordering::Acquire);
    let agg = shared.aggregate.load(Ordering::Acquire);
    if cur.saturating_add(len) > MAX_BUFFERED_PER_STREAM
        || agg.saturating_add(len) > MAX_AGGREGATE_BUFFERED
    {
        drop(map);
        shared.close_peer(id);
        return;
    }
    slot.buffered.fetch_add(len, Ordering::AcqRel);
    shared.aggregate.fetch_add(len, Ordering::AcqRel);
    let sent = slot
        .data_tx
        .as_ref()
        .is_some_and(|tx| tx.send(payload).is_ok());
    if !sent {
        sub_atomic(&slot.buffered, len);
        sub_atomic(&shared.aggregate, len);
    }
}

fn lock_data_wakers(wakers: &Mutex<Vec<Waker>>) -> std::sync::MutexGuard<'_, Vec<Waker>> {
    wakers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wake_data_writers(wakers: &Mutex<Vec<Waker>>) {
    let parked = std::mem::take(&mut *lock_data_wakers(wakers));
    for waker in parked {
        waker.wake();
    }
}

async fn writer_task<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut control_rx: mpsc::Receiver<Control>,
    mut data_rx: mpsc::Receiver<OutData>,
    data_wakers: Arc<Mutex<Vec<Waker>>>,
) {
    let mut data_open = true;
    loop {
        tokio::select! {
            biased;
            ctrl = control_rx.recv() => {
                let Some(ctrl) = ctrl else {
                    if data_open {
                        while let Some(data) = data_rx.recv().await {
                            wake_data_writers(&data_wakers);
                            if write_out_data(&mut writer, data).await.is_err() {
                                break;
                            }
                        }
                    }
                    break;
                };
                let (typ, conn_id, payload) = match ctrl {
                    Control::Open(id) => (TYPE_OPEN, id, Vec::new()),
                    Control::Close(id) => (TYPE_CLOSE, id, Vec::new()),
                    Control::Window(id, credit) => {
                        (TYPE_WINDOW, id, credit.to_be_bytes().to_vec())
                    }
                };
                if write_raw(&mut writer, typ, conn_id, &payload).await.is_err() {
                    break;
                }
            }
            data = data_rx.recv(), if data_open => {
                let Some(data) = data else {
                    data_open = false;
                    continue;
                };
                wake_data_writers(&data_wakers);
                if write_out_data(&mut writer, data).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Writes one queued Data or Close frame.
///
/// # Errors
///
/// Returns [`SdkError`] when the underlying write or flush fails.
async fn write_out_data<W: AsyncWrite + Unpin>(writer: &mut W, data: OutData) -> Result<()> {
    match data {
        OutData::Bytes { id, payload } => write_raw(writer, TYPE_DATA, id, &payload).await,
        OutData::Close(id) => write_raw(writer, TYPE_CLOSE, id, &[]).await,
    }
}

/// Writes one length-prefixed mux frame and flushes it.
///
/// # Errors
///
/// Returns [`SdkError`] when the underlying write or flush fails.
async fn write_raw<W: AsyncWrite + Unpin>(
    writer: &mut W,
    typ: u8,
    conn_id: u32,
    payload: &[u8],
) -> Result<()> {
    let len = (1 + 4 + payload.len()) as u32;
    let mut buf = Vec::with_capacity(4 + len as usize);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(typ);
    buf.extend_from_slice(&conn_id.to_be_bytes());
    buf.extend_from_slice(payload);
    writer.write_all(&buf).await.map_err(io_err)?;
    writer.flush().await.map_err(io_err)?;
    Ok(())
}

fn io_err(err: std::io::Error) -> SdkError {
    SdkError::message(err.to_string())
}

impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.read_buf.is_empty() {
            let n = buf.remaining().min(self.read_buf.len());
            buf.put_slice(&self.read_buf[..n]);
            self.read_buf.drain(..n);
            self.note_consumed(n);
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut self.data_rx).poll_recv(cx) {
            Poll::Ready(Some(chunk)) => {
                let n = buf.remaining().min(chunk.len());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.read_buf.extend_from_slice(&chunk[n..]);
                }
                self.note_consumed(n);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => {
                self.closed = true;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.closed || self.peer_gone.load(Ordering::SeqCst) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "mux closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let credit = self.send_credit.load(Ordering::Acquire);
        if credit == 0 {
            let mut slot = lock_waker(&self.send_waker);
            *slot = Some(cx.waker().clone());
            if self.send_credit.load(Ordering::Acquire) == 0 {
                return Poll::Pending;
            }
        }
        let credit = self.send_credit.load(Ordering::Acquire);
        if credit == 0 {
            return Poll::Pending;
        }
        let n = buf.len().min(MAX_FRAME_PAYLOAD).min(credit as usize);
        let frame = OutData::Bytes {
            id: self.id,
            payload: buf[..n].to_vec(),
        };
        match self.shared.data_tx.try_send(frame) {
            Ok(()) => {
                self.send_credit.fetch_sub(n as u32, Ordering::AcqRel);
                self.in_flight.fetch_add(n as u32, Ordering::AcqRel);
                Poll::Ready(Ok(n))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "mux writer gone",
            ))),
            Err(mpsc::error::TrySendError::Full(frame)) => {
                lock_data_wakers(&self.shared.data_wakers).push(cx.waker().clone());
                match self.shared.data_tx.try_send(frame) {
                    Ok(()) => {
                        self.send_credit.fetch_sub(n as u32, Ordering::AcqRel);
                        self.in_flight.fetch_add(n as u32, Ordering::AcqRel);
                        Poll::Ready(Ok(n))
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(
                        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "mux writer gone"),
                    )),
                    Err(mpsc::error::TrySendError::Full(_)) => Poll::Pending,
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if !self.closed {
            if self.announced {
                self.queue_close();
            }
            self.closed = true;
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    fn pair() -> (Mux, Mux) {
        let (a, b) = duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        (Mux::client(ar, aw), Mux::server(br, bw))
    }

    async fn write_open(writer: &mut (impl AsyncWrite + Unpin), id: u32) {
        write_raw(writer, TYPE_OPEN, id, &[]).await.unwrap();
    }

    #[tokio::test]
    async fn client_open_server_accept_echo() {
        let (client, server) = pair();
        let mut c = client.open().await.expect("open");
        let mut s = server.accept().await.expect("accept");
        c.write_all(b"hello").await.expect("write");
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"hello");
        s.write_all(b"world").await.expect("write");
        let mut buf2 = [0u8; 5];
        c.read_exact(&mut buf2).await.expect("read");
        assert_eq!(&buf2, b"world");
    }

    #[tokio::test]
    async fn server_can_open_toward_the_client() {
        let (client, server) = pair();
        let mut s = server.open().await.expect("server open");
        let mut c = client.accept().await.expect("client accept");
        s.write_all(b"up").await.expect("write");
        let mut buf = [0u8; 2];
        c.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"up");
    }

    #[tokio::test]
    async fn two_streams_backpressure_does_not_stall_the_other() {
        let (client, server) = pair();
        let mut a = client.open().await.expect("open a");
        let mut b = client.open().await.expect("open b");
        let mut sa = server.accept().await.expect("accept a");
        let mut sb = server.accept().await.expect("accept b");

        let payload = vec![0xABu8; INITIAL_WINDOW as usize + 8 * 1024];
        let payload_b = vec![0xCDu8; 4096];

        let writer_a = tokio::spawn(async move {
            a.write_all(&payload).await.expect("write a");
            a.shutdown().await.expect("shutdown a");
            payload.len()
        });
        let writer_b = tokio::spawn(async move {
            b.write_all(&payload_b).await.expect("write b");
            b.shutdown().await.expect("shutdown b");
            payload_b.len()
        });

        let mut got_b = Vec::new();
        sb.read_to_end(&mut got_b).await.expect("read b");
        assert_eq!(got_b.len(), 4096);
        assert!(got_b.iter().all(|x| *x == 0xCD));
        writer_b.await.expect("join b");

        let mut got_a = Vec::new();
        sa.read_to_end(&mut got_a).await.expect("read a");
        assert_eq!(got_a.len(), INITIAL_WINDOW as usize + 8 * 1024);
        writer_a.await.expect("join a");
    }

    #[tokio::test]
    async fn oversize_length_is_rejected_before_a_payload_buffer() {
        let (server_io, mut peer) = duplex(64);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(sr, sw);
        peer.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(
            server.accept().await.is_err(),
            "oversize length must stop the reader"
        );
    }

    #[tokio::test]
    async fn excess_credit_data_is_not_buffered() {
        let (server_io, peer) = duplex(MAX_BUFFERED_PER_STREAM + 64 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(sr, sw);
        let (mut pr, mut pw) = tokio::io::split(peer);
        tokio::spawn(async move {
            let _ = pr.read_to_end(&mut Vec::new()).await;
        });
        write_open(&mut pw, 1).await;
        let mut stream = server.accept().await.expect("accept");
        let ok = vec![0x11u8; MAX_BUFFERED_PER_STREAM];
        write_raw(&mut pw, TYPE_DATA, 1, &ok).await.unwrap();
        let extra = vec![0x22u8; 4096];
        write_raw(&mut pw, TYPE_DATA, 1, &extra).await.unwrap();
        let mut got = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read_to_end(&mut got),
        )
        .await
        .expect("read finished")
        .unwrap();
        assert_eq!(got.len(), MAX_BUFFERED_PER_STREAM);
        assert!(got.iter().all(|b| *b == 0x11));
    }

    #[tokio::test]
    async fn open_flood_stops_at_the_live_stream_cap() {
        let (server_io, peer) = duplex(256 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(sr, sw);
        let (_pr, mut pw) = tokio::io::split(peer);
        for i in 0..(MAX_LIVE_STREAMS + 8) {
            write_open(&mut pw, (i as u32) * 2 + 1).await;
        }
        let mut n = 0;
        while let Ok(Ok(_)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), server.accept()).await
        {
            n += 1;
        }
        assert_eq!(n, MAX_LIVE_STREAMS);
    }

    #[tokio::test]
    async fn open_close_flood_with_accept_paused_stays_at_the_cap() {
        let (server_io, peer) = duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(sr, sw);
        let (mut pr, mut pw) = tokio::io::split(peer);
        let drain = tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                match pr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        let opens = MAX_LIVE_STREAMS * 8;
        for i in 0..opens {
            let id = (i as u32) * 2 + 1;
            write_open(&mut pw, id).await;
            write_raw(&mut pw, TYPE_CLOSE, id, &[]).await.unwrap();
        }
        drop(pw);
        let settled = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if lock_map(&server.shared.map).len() == MAX_LIVE_STREAMS {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "occupancy never settled at the cap; map={}",
            lock_map(&server.shared.map).len()
        );
        assert_eq!(lock_map(&server.shared.map).len(), MAX_LIVE_STREAMS);
        let mut n = 0usize;
        let mut held = Vec::new();
        while let Ok(Ok(stream)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), server.accept()).await
        {
            n += 1;
            held.push(stream);
        }
        assert_eq!(n, MAX_LIVE_STREAMS, "accept queue grew past the live cap");
        assert_eq!(lock_map(&server.shared.map).len(), MAX_LIVE_STREAMS);
        drop(held);
        assert_eq!(lock_map(&server.shared.map).len(), 0);
        drain.abort();
    }

    #[tokio::test]
    async fn dropping_a_stale_stream_does_not_retire_the_reused_id() {
        let (server_io, _peer) = duplex(4096);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(sr, sw);
        let (mut stale, _old_slot) = MuxStream::pair(1, Arc::clone(&server.shared));
        stale.generation = 1;
        stale.inserted = true;
        let (mut live, mut slot) = MuxStream::pair(1, Arc::clone(&server.shared));
        slot.generation = 2;
        live.generation = 2;
        live.inserted = true;
        lock_map(&server.shared.map).insert(1, slot);
        drop(stale);
        assert_eq!(
            lock_map(&server.shared.map)
                .get(&1)
                .map(|slot| slot.generation),
            Some(2),
            "dropping the old stream removed the replacement"
        );
        {
            let map = lock_map(&server.shared.map);
            map.get(&1)
                .expect("replacement slot")
                .data_tx
                .as_ref()
                .expect("replacement inbound")
                .send(b"ok".to_vec())
                .expect("send");
        }
        let mut buf = [0u8; 2];
        live.read_exact(&mut buf)
            .await
            .expect("replacement still reads");
        assert_eq!(&buf, b"ok");
    }

    #[tokio::test]
    async fn duplicate_and_wrong_parity_ids_are_rejected() {
        let (server_io, peer) = duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(sr, sw);
        let (_pr, mut pw) = tokio::io::split(peer);
        write_open(&mut pw, 2).await;
        assert!(
            server.accept().await.is_err(),
            "even id is not a client open"
        );

        let (server_io, peer) = duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(sr, sw);
        let (_pr, mut pw) = tokio::io::split(peer);
        write_open(&mut pw, 1).await;
        let first = server.accept().await.expect("first");
        assert_eq!(first.id(), 1);
        write_open(&mut pw, 1).await;
        assert!(
            server.accept().await.is_err(),
            "duplicate id must stop the reader"
        );
    }

    #[tokio::test]
    async fn window_overflow_does_not_raise_send_credit() {
        let (client_io, peer) = duplex(1024 * 1024);
        let (cr, cw) = tokio::io::split(client_io);
        let client = Mux::client(cr, cw);
        let (mut pr, mut pw) = tokio::io::split(peer);
        let counted = Arc::new(AtomicUsize::new(0));
        let counted_task = Arc::clone(&counted);
        let (window_tx, window_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut window_tx = Some(window_tx);
            loop {
                let header = match read_header(&mut pr).await {
                    Ok(header) => header,
                    Err(_) => break,
                };
                if header.typ == TYPE_OPEN {
                    write_raw(
                        &mut pw,
                        TYPE_WINDOW,
                        header.conn_id,
                        &u32::MAX.to_be_bytes(),
                    )
                    .await
                    .ok();
                    if let Some(tx) = window_tx.take() {
                        let _ = tx.send(());
                    }
                }
                if header.typ == TYPE_DATA {
                    counted_task.fetch_add(header.payload_len, Ordering::SeqCst);
                }
                if header.payload_len > 0 && discard(&mut pr, header.payload_len).await.is_err() {
                    break;
                }
            }
        });
        let mut stream = client.open().await.expect("open");
        window_rx.await.expect("window sent");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let payload = vec![0x5Au8; INITIAL_WINDOW as usize + 32 * 1024];
        let mut written = 0usize;
        let write = async {
            while written < payload.len() {
                let n = stream.write(&payload[written..]).await?;
                written += n;
            }
            Ok::<(), std::io::Error>(())
        };
        let timed = tokio::time::timeout(std::time::Duration::from_millis(400), write).await;
        assert!(timed.is_err(), "write must block at the initial window");
        assert!(
            written <= INITIAL_WINDOW as usize,
            "wrote {written} after a saturating window"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            counted.load(Ordering::SeqCst) <= INITIAL_WINDOW as usize,
            "peer observed more than the initial window"
        );
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_bounds_the_writer_queue() {
        let (client_io, peer) = duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let client = Mux::client(cr, cw);
        // Hold the peer end so the socket stays open and unread.
        let _peer = peer;
        let mut stream = client.open().await.expect("open");
        let payload = vec![0x7Eu8; INITIAL_WINDOW as usize + 64 * 1024];
        let mut written = 0usize;
        let write = async {
            while written < payload.len() {
                let n = stream.write(&payload[written..]).await?;
                written += n;
            }
            Ok::<(), std::io::Error>(())
        };
        let timed = tokio::time::timeout(std::time::Duration::from_millis(400), write).await;
        assert!(
            timed.is_err(),
            "write must stall when the peer does not read"
        );
        assert!(
            written <= INITIAL_WINDOW as usize,
            "queued {written} bytes for an unread peer"
        );
    }

    #[tokio::test]
    async fn window_is_not_stuck_behind_queued_data() {
        let (control_tx, control_rx) = mpsc::channel(WRITER_CONTROL_QUEUE);
        let (data_tx, data_rx) = mpsc::channel(WRITER_DATA_QUEUE);
        for id in 0..WRITER_DATA_QUEUE {
            data_tx
                .try_send(OutData::Bytes {
                    id: id as u32,
                    payload: vec![0xAB],
                })
                .unwrap();
        }
        control_tx.try_send(Control::Window(7, 1)).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter {
            log: Arc::clone(&log),
            buf: Vec::new(),
        };
        let task = tokio::spawn(writer_task(
            writer,
            control_rx,
            data_rx,
            Arc::new(Mutex::new(Vec::new())),
        ));
        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if log.lock().unwrap().len() > WRITER_DATA_QUEUE {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(ready.is_ok(), "writer did not drain control and data");
        let frames = log.lock().unwrap().clone();
        drop(control_tx);
        drop(data_tx);
        task.await.unwrap();
        assert_eq!(
            frames.first().copied(),
            Some(TYPE_WINDOW),
            "Window must be written before queued Data: {frames:?}"
        );
        assert!(frames.contains(&TYPE_DATA));
    }

    #[tokio::test]
    async fn aggregate_cap_discards_further_data_without_stopping_an_earlier_stream() {
        let streams = MAX_AGGREGATE_BUFFERED / MAX_BUFFERED_PER_STREAM;
        let (server_io, peer) = duplex(MAX_AGGREGATE_BUFFERED + 256 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(sr, sw);
        let (mut pr, mut pw) = tokio::io::split(peer);
        tokio::spawn(async move {
            let _ = pr.read_to_end(&mut Vec::new()).await;
        });
        let payload = vec![0x44u8; MAX_BUFFERED_PER_STREAM];
        for i in 0..streams {
            let id = (i as u32) * 2 + 1;
            write_open(&mut pw, id).await;
            write_raw(&mut pw, TYPE_DATA, id, &payload).await.unwrap();
        }
        let extra_id = (streams as u32) * 2 + 1;
        write_open(&mut pw, extra_id).await;
        write_raw(&mut pw, TYPE_DATA, extra_id, &[0x55])
            .await
            .unwrap();
        let mut accepted = Vec::new();
        for _ in 0..=streams {
            accepted.push(server.accept().await.expect("accept"));
        }
        let mut extra = accepted.pop().unwrap();
        let mut earlier = accepted.remove(0);
        let mut one = [0u8; 1];
        earlier.read_exact(&mut one).await.unwrap();
        assert_eq!(one[0], 0x44);
        let mut extra_buf = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            extra.read_to_end(&mut extra_buf),
        )
        .await
        .expect("extra stream closed")
        .unwrap();
        assert!(extra_buf.is_empty(), "aggregate overflow was buffered");
    }

    /// Records frame types from complete mux frames.
    struct LogWriter {
        log: Arc<Mutex<Vec<u8>>>,
        buf: Vec<u8>,
    }

    impl AsyncWrite for LogWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.buf.extend_from_slice(buf);
            loop {
                if self.buf.len() < 4 {
                    break;
                }
                let len = u32::from_be_bytes(self.buf[..4].try_into().unwrap()) as usize;
                if self.buf.len() < 4 + len {
                    break;
                }
                let typ = self.buf[4];
                self.log.lock().unwrap().push(typ);
                self.buf.drain(..4 + len);
            }
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}
