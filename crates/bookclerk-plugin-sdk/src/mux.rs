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
//! alive. Credit and close records for a generation stay reserved until the
//! writer drops both, so a blocked writer cannot accumulate them after the
//! map slot is released. A new Open is refused while either record vector or
//! that reservation table is already at the cap. Each slot has a generation
//! so dropping an old stream cannot remove a replacement that reused the id.
//! Window updates saturate at [`INITIAL_WINDOW`] and are coalesced until the
//! writer accepts them, so a full control queue cannot drop receive credit.
//! `Close` is written only after Data already accepted for that same stream.
//! Window updates and other streams' control frames are not stuck behind one
//! unread stream. A stream flush or shutdown waits until that stream's
//! accepted output is written. [`Mux::shutdown`] and dropping the last mux
//! cancel both tasks without draining queued frames.
//!
//! Frame layout (big-endian):
//! ```text
//! u32 length_of_rest | u8 type | u32 conn_id | payload
//! ```
//! Types: `Open=1`, `Data=2`, `Close=3`, `Window=4`. Data payloads are capped
//! at 1 MiB. A `Window` frame carries a `u32` credit (bytes the peer may
//! send). Each stream starts with [`INITIAL_WINDOW`] bytes of send credit.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, Notify};

use crate::{Result, SdkError};

/// Mux reader and writer tasks that have not finished in this process.
static LIVE_MUX_TASKS: AtomicUsize = AtomicUsize::new(0);

/// In-process mux reader and writer tasks that have not returned.
#[must_use]
pub fn live_mux_task_count() -> usize {
    LIVE_MUX_TASKS.load(Ordering::SeqCst)
}

struct LiveMuxTask;

impl Drop for LiveMuxTask {
    fn drop(&mut self) {
        LIVE_MUX_TASKS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn begin_mux_task() -> LiveMuxTask {
    LIVE_MUX_TASKS.fetch_add(1, Ordering::SeqCst);
    LiveMuxTask
}

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
///
/// `Wake` carries no frame. It asks the writer to flush coalesced window
/// credit and any `Close` that is no longer waiting on queued Data.
enum Control {
    /// A local `Open` is waiting in the writer's pending-open queue.
    Open,
    Close(u32),
    Wake,
}

/// One outbound Data frame. `Close` is not queued here: the writer emits it
/// after every accepted Data frame for that stream has been written.
struct OutData {
    id: u32,
    generation: u64,
    payload: Vec<u8>,
    /// Data frames for this stream accepted into the outbound queue.
    outbound_queued: Arc<AtomicUsize>,
    /// Set when the local stream wants `Close` after those frames.
    close_pending: Arc<AtomicBool>,
}

/// A local `Open` that must be on the wire before that generation's Data or Close.
struct PendingOpen {
    id: u32,
    generation: u64,
}

/// Local generation ordering. Peer-initiated streams are not listed.
struct LocalGen {
    id: u32,
    generation: u64,
    open_written: bool,
    close_written: bool,
}

/// Receive credit the writer has not yet turned into a `Window` frame.
struct CreditHold {
    id: u32,
    generation: u64,
    pending: Arc<AtomicU32>,
    alive: Arc<AtomicBool>,
    /// `pending` was taken and the `Window` frame has not finished yet.
    emitting: bool,
}

/// A `Close` that must wait until `outbound_queued` is zero.
struct CloseHold {
    id: u32,
    generation: u64,
    outbound_queued: Arc<AtomicUsize>,
    close_pending: Arc<AtomicBool>,
    /// Set once the `Close` frame has been written.
    close_flag: Arc<AtomicBool>,
    /// The `Close` frame was taken and has not finished yet.
    emitting: bool,
}

/// One admitted generation until the writer has finished its credit and close.
///
/// The map slot can disappear on drop while this record remains. Admission
/// fails when the table is already at [`MAX_LIVE_STREAMS`].
struct BookRecord {
    generation: u64,
    credit_done: bool,
    close_done: bool,
}

/// Credit and close accounting the writer flushes. Kept off [`Shared`] so the
/// writer can hold it without keeping the control-channel sender alive.
struct WriterState {
    credits: Mutex<Vec<CreditHold>>,
    closes: Mutex<Vec<CloseHold>>,
    records: Mutex<Vec<BookRecord>>,
    pending_opens: Mutex<VecDeque<PendingOpen>>,
    local_gens: Mutex<Vec<LocalGen>>,
    /// Wakes the reader when a `Close` has been written.
    admit_wake: Arc<Notify>,
    /// First writer failure. Later flushes and shutdowns surface it.
    fault: Mutex<Option<String>>,
    flush_wakers: Mutex<Vec<Waker>>,
    writer_stopped: AtomicBool,
    /// Forced cancellation flag shared with the mux task owner.
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    credit_hwm: AtomicUsize,
    close_hwm: AtomicUsize,
    record_hwm: AtomicUsize,
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
    writer_state: Arc<WriterState>,
    /// Streams sitting in the accept channel, not yet returned by [`Mux::accept`].
    accept_queued: AtomicUsize,
    accept_hwm: AtomicUsize,
    map_hwm: AtomicUsize,
    /// Peer opens waiting until an older `Close` for the same id is written.
    deferred_opens: Mutex<VecDeque<u32>>,
    /// Wakes the reader after a `Close` so a deferred open can be admitted.
    admit_wake: Arc<Notify>,
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
    pending_credit: Arc<AtomicU32>,
    credit_alive: Arc<AtomicBool>,
}

/// Reader and writer lifetime shared by every clone of a [`Mux`].
///
/// The tasks hold the stop flag, not this struct. Dropping the last clone sets
/// the flag so those tasks exit instead of detaching. [`Mux::shutdown`] sets
/// the same flag while other clones still exist.
struct MuxTasks {
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    remaining: Arc<AtomicUsize>,
    finished: Arc<Notify>,
    joins: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for MuxTasks {
    fn drop(&mut self) {
        cancel_mux_tasks(self);
    }
}

fn cancel_mux_tasks(tasks: &MuxTasks) {
    tasks.stop.store(true, Ordering::SeqCst);
    tasks.wake.notify_waiters();
    let joins = std::mem::take(
        &mut *tasks
            .joins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for join in joins {
        join.abort();
    }
}

struct FinishTask {
    remaining: Arc<AtomicUsize>,
    finished: Arc<Notify>,
}

impl Drop for FinishTask {
    fn drop(&mut self) {
        finish_task(&self.remaining, &self.finished);
    }
}

/// One multiplexed endpoint. Cheap to clone (shared tasks + id allocator).
///
/// Clones share one task owner. Dropping a clone does not stop the reader or
/// writer while another clone remains. Dropping the last clone, or calling
/// [`Mux::shutdown`], cancels both tasks even if the peer is still connected.
#[derive(Clone)]
pub struct Mux {
    shared: Arc<Shared>,
    next_id: Arc<AtomicU32>,
    accept_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<MuxStream>>>,
    tasks: Arc<MuxTasks>,
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
    pending_credit: Arc<AtomicU32>,
    outbound_queued: Arc<AtomicUsize>,
    close_pending: Arc<AtomicBool>,
    /// Set when this stream's `Close` frame has been written.
    close_flag: Arc<AtomicBool>,
    /// `Open` was queued, so drop must send `Close`.
    announced: bool,
    /// Slot was inserted, so drop must remove it.
    inserted: bool,
    /// Local `Close` was queued. Inbound EOF does not set this.
    closed: bool,
    /// The peer stopped delivering bytes. Independent of outbound `Close`.
    read_eof: bool,
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
        let admit_wake = Arc::new(Notify::new());
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let writer_state = Arc::new(WriterState {
            credits: Mutex::new(Vec::new()),
            closes: Mutex::new(Vec::new()),
            records: Mutex::new(Vec::new()),
            pending_opens: Mutex::new(VecDeque::new()),
            local_gens: Mutex::new(Vec::new()),
            admit_wake: Arc::clone(&admit_wake),
            fault: Mutex::new(None),
            flush_wakers: Mutex::new(Vec::new()),
            writer_stopped: AtomicBool::new(false),
            stop: Arc::clone(&stop),
            wake: Arc::clone(&wake),
            credit_hwm: AtomicUsize::new(0),
            close_hwm: AtomicUsize::new(0),
            record_hwm: AtomicUsize::new(0),
        });
        let shared = Arc::new(Shared {
            map: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            aggregate: AtomicUsize::new(0),
            control_tx: control_tx.clone(),
            data_tx: data_tx.clone(),
            data_wakers: Arc::clone(&data_wakers),
            writer_state: Arc::clone(&writer_state),
            accept_queued: AtomicUsize::new(0),
            accept_hwm: AtomicUsize::new(0),
            map_hwm: AtomicUsize::new(0),
            deferred_opens: Mutex::new(VecDeque::new()),
            admit_wake,
        });
        let task_owner = Arc::new(MuxTasks {
            stop: Arc::clone(&stop),
            wake: Arc::clone(&wake),
            remaining: Arc::new(AtomicUsize::new(2)),
            finished: Arc::new(Notify::new()),
            joins: Mutex::new(Vec::new()),
        });
        let writer_stop = Arc::clone(&task_owner.stop);
        let writer_wake = Arc::clone(&task_owner.wake);
        let writer_remaining = Arc::clone(&task_owner.remaining);
        let writer_finished = Arc::clone(&task_owner.finished);
        let writer_join = tokio::spawn(async move {
            let _live = begin_mux_task();
            let _finish = FinishTask {
                remaining: writer_remaining,
                finished: writer_finished,
            };
            writer_task(
                writer,
                writer_state,
                control_rx,
                data_rx,
                data_wakers,
                writer_stop,
                writer_wake,
            )
            .await;
        });
        let reader_shared = Arc::clone(&shared);
        let reader_stop = Arc::clone(&task_owner.stop);
        let reader_wake = Arc::clone(&task_owner.wake);
        let reader_remaining = Arc::clone(&task_owner.remaining);
        let reader_finished = Arc::clone(&task_owner.finished);
        let reader_join = tokio::spawn(async move {
            let _live = begin_mux_task();
            let _finish = FinishTask {
                remaining: reader_remaining,
                finished: reader_finished,
            };
            let peers = ClosePeers(Arc::clone(&reader_shared));
            if let Err(err) = reader_task(
                reader,
                Arc::clone(&reader_shared),
                accept_tx,
                id_base,
                reader_stop,
                reader_wake,
            )
            .await
            {
                if !is_mux_stopped(&err) {
                    tracing::debug!(error = %err, "mux reader stopped");
                }
            }
            drop(peers);
        });
        task_owner
            .joins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([writer_join, reader_join]);
        Self {
            shared,
            next_id: Arc::new(AtomicU32::new(id_base)),
            accept_rx: Arc::new(tokio::sync::Mutex::new(accept_rx)),
            tasks: task_owner,
        }
    }

    /// Stop the reader and writer even if other clones of this mux still exist.
    ///
    /// The peer does not have to hang up. [`Self::closed`] resolves when both
    /// tasks have left their loops and dropped the transport.
    pub fn shutdown(&self) {
        cancel_mux_tasks(&self.tasks);
    }

    /// Wait until the reader and writer tasks have finished.
    pub async fn closed(&self) {
        loop {
            let notified = self.tasks.finished.notified();
            if self.tasks.remaining.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

fn finish_task(remaining: &AtomicUsize, finished: &Notify) {
    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
        finished.notify_waiters();
    }
}

async fn wait_stop(stop: &AtomicBool, wake: &Notify) {
    loop {
        let notified = wake.notified();
        if stop.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

fn is_mux_stopped(err: &SdkError) -> bool {
    matches!(err, SdkError::Message(msg) if msg == "mux stopped")
}

/// Closes every peer slot when the reader task is aborted or returns.
struct ClosePeers(Arc<Shared>);

impl Drop for ClosePeers {
    fn drop(&mut self) {
        self.0.close_all_peers();
    }
}

impl Mux {
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
        {
            let mut gens = lock_local_gens(&self.shared.writer_state.local_gens);
            gens.push(LocalGen {
                id,
                generation,
                open_written: false,
                close_written: false,
            });
            lock_pending_opens(&self.shared.writer_state.pending_opens)
                .push_back(PendingOpen { id, generation });
        }
        stream.announced = true;
        self.shared
            .control_tx
            .send(Control::Open)
            .await
            .map_err(|_| SdkError::message("mux writer gone"))?;
        Ok(stream)
    }

    /// Wait for the next logical stream opened by the peer.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the reader task exits (peer hangup or bad frame).
    pub async fn accept(&self) -> Result<MuxStream> {
        let stream = self
            .accept_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| SdkError::message("mux closed"))?;
        self.shared.accept_queued.fetch_sub(1, Ordering::SeqCst);
        Ok(stream)
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
        let mut records = lock_records(&self.writer_state.records);
        let mut credits = lock_credits(&self.writer_state.credits);
        let closes_len = lock_closes(&self.writer_state.closes).len();
        if records.len() >= MAX_LIVE_STREAMS
            || credits.len() >= MAX_LIVE_STREAMS
            || closes_len >= MAX_LIVE_STREAMS
        {
            return None;
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        slot.generation = generation;
        let pending = Arc::clone(&slot.pending_credit);
        let alive = Arc::clone(&slot.credit_alive);
        map.insert(id, slot);
        self.map_hwm.fetch_max(map.len(), Ordering::Relaxed);
        records.push(BookRecord {
            generation,
            credit_done: false,
            close_done: false,
        });
        self.writer_state
            .record_hwm
            .fetch_max(records.len(), Ordering::Relaxed);
        credits.push(CreditHold {
            id,
            generation,
            pending,
            alive,
            emitting: false,
        });
        self.writer_state
            .credit_hwm
            .fetch_max(credits.len(), Ordering::Relaxed);
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
            slot.credit_alive.store(false, Ordering::SeqCst);
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
        let pending_credit = Arc::new(AtomicU32::new(0));
        let credit_alive = Arc::new(AtomicBool::new(true));
        let outbound_queued = Arc::new(AtomicUsize::new(0));
        let close_pending = Arc::new(AtomicBool::new(false));
        let close_flag = Arc::new(AtomicBool::new(false));
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
            pending_credit: Arc::clone(&pending_credit),
            outbound_queued: Arc::clone(&outbound_queued),
            close_pending: Arc::clone(&close_pending),
            close_flag,
            announced: false,
            inserted: false,
            closed: false,
            read_eof: false,
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
                pending_credit,
                credit_alive,
            },
        )
    }

    /// Remember `n` bytes of receive credit. The writer coalesces this into a
    /// `Window` frame. A full control queue keeps the credit here.
    fn grant_window(&self, n: u32) {
        if n == 0 {
            return;
        }
        self.pending_credit.fetch_add(n, Ordering::AcqRel);
        let _ = self.shared.control_tx.try_send(Control::Wake);
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
        // `read_eof` is inbound only. A peer Close still needs this side's
        // Close, or the generation's reservation stays charged.
        let mut queued_close = false;
        if self.announced && !self.closed {
            self.queue_close();
            self.closed = true;
            queued_close = true;
        } else if self.inserted && !self.announced {
            // No Close record will be queued. The admission permit ends when
            // the writer drops the credit hold.
            mark_book_close_done(&self.shared.writer_state, self.generation);
        }
        if self.inserted && !queued_close {
            // Close was already queued. Wake the writer so it can drop the
            // credit hold now that `credit_alive` is false.
            let _ = self.shared.control_tx.try_send(Control::Wake);
        }
    }
}

impl MuxStream {
    /// Ask the writer to send `Close` after Data already queued for this stream.
    fn queue_close(&self) {
        if self
            .close_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        {
            let mut closes = lock_closes(&self.shared.writer_state.closes);
            closes.push(CloseHold {
                id: self.id,
                generation: self.generation,
                outbound_queued: Arc::clone(&self.outbound_queued),
                close_pending: Arc::clone(&self.close_pending),
                close_flag: Arc::clone(&self.close_flag),
                emitting: false,
            });
            self.shared
                .writer_state
                .close_hwm
                .fetch_max(closes.len(), Ordering::Relaxed);
        }
        let _ = self.shared.control_tx.try_send(Control::Wake);
    }
}

fn lock_map(map: &Mutex<HashMap<u32, Slot>>) -> std::sync::MutexGuard<'_, HashMap<u32, Slot>> {
    map.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_credits(credits: &Mutex<Vec<CreditHold>>) -> std::sync::MutexGuard<'_, Vec<CreditHold>> {
    credits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_closes(closes: &Mutex<Vec<CloseHold>>) -> std::sync::MutexGuard<'_, Vec<CloseHold>> {
    closes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_records(records: &Mutex<Vec<BookRecord>>) -> std::sync::MutexGuard<'_, Vec<BookRecord>> {
    records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn note_accept_queued(shared: &Shared) {
    let now = shared.accept_queued.fetch_add(1, Ordering::SeqCst) + 1;
    shared.accept_hwm.fetch_max(now, Ordering::Relaxed);
}

fn lock_pending_opens(
    opens: &Mutex<VecDeque<PendingOpen>>,
) -> std::sync::MutexGuard<'_, VecDeque<PendingOpen>> {
    opens
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_local_gens(gens: &Mutex<Vec<LocalGen>>) -> std::sync::MutexGuard<'_, Vec<LocalGen>> {
    gens.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_deferred(deferred: &Mutex<VecDeque<u32>>) -> std::sync::MutexGuard<'_, VecDeque<u32>> {
    deferred
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// True when this id still has a local generation or `Close` the writer has not finished.
fn id_waits_for_close(shared: &Shared, id: u32) -> bool {
    lock_closes(&shared.writer_state.closes)
        .iter()
        .any(|hold| hold.id == id)
        || lock_local_gens(&shared.writer_state.local_gens)
            .iter()
            .any(|gen| gen.id == id && !gen.close_written)
}

/// Admits one deferred peer open. `Ok(true)` means the caller should loop.
///
/// # Errors
///
/// Returns [`SdkError`] when the accept channel is closed or the id is a duplicate.
fn admit_deferred_opens(
    shared: &Arc<Shared>,
    accept_tx: &mpsc::UnboundedSender<MuxStream>,
) -> Result<bool> {
    let Some(id) = lock_deferred(&shared.deferred_opens).front().copied() else {
        return Ok(false);
    };
    if id_waits_for_close(shared, id) {
        return Ok(false);
    }
    lock_deferred(&shared.deferred_opens).pop_front();
    if lock_map(&shared.map).contains_key(&id) {
        return Err(SdkError::message("mux duplicate stream id"));
    }
    if !enqueue_peer_open(shared, accept_tx, id)? {
        let _ = shared.control_tx.try_send(Control::Close(id));
    }
    Ok(true)
}

/// Queues an accepted peer stream.
///
/// `Ok(false)` means the cap refused the id. The caller sends `Close`.
///
/// # Errors
///
/// Returns [`SdkError`] when the accept channel is closed.
fn enqueue_peer_open(
    shared: &Arc<Shared>,
    accept_tx: &mpsc::UnboundedSender<MuxStream>,
    id: u32,
) -> Result<bool> {
    let (mut stream, slot) = MuxStream::pair(id, Arc::clone(shared));
    let Some(generation) = shared.try_insert(id, slot) else {
        return Ok(false);
    };
    stream.generation = generation;
    stream.inserted = true;
    stream.announced = true;
    note_accept_queued(shared);
    if accept_tx.send(stream).is_err() {
        shared.accept_queued.fetch_sub(1, Ordering::SeqCst);
        return Err(SdkError::message("mux accept closed"));
    }
    Ok(true)
}

/// Records that the writer has finished this generation's credit or close.
///
/// The reservation stays until both are done, which is what admission counts.
fn mark_book(state: &WriterState, generation: u64, credit: bool) {
    let mut records = lock_records(&state.records);
    let Some(record) = records
        .iter_mut()
        .find(|record| record.generation == generation)
    else {
        return;
    };
    if credit {
        record.credit_done = true;
    } else {
        record.close_done = true;
    }
    records.retain(|record| !(record.credit_done && record.close_done));
}

fn mark_book_credit_done(state: &WriterState, generation: u64) {
    mark_book(state, generation, true);
}

fn mark_book_close_done(state: &WriterState, generation: u64) {
    mark_book(state, generation, false);
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

/// Reads `buf` unless forced cancellation wins first.
///
/// # Errors
///
/// Returns [`SdkError`] when the peer closes early, the read fails, or the mux
/// is stopped.
async fn read_exact_unless_stopped<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
    stop: &AtomicBool,
    wake: &Notify,
) -> Result<()> {
    tokio::select! {
        biased;
        () = wait_stop(stop, wake) => Err(SdkError::message("mux stopped")),
        result = reader.read_exact(buf) => result.map(|_| ()).map_err(io_err),
    }
}

/// Discards `n` bytes unless forced cancellation wins first.
///
/// # Errors
///
/// Returns [`SdkError`] when the peer closes early, the read fails, or the mux
/// is stopped.
async fn discard_unless_stopped<R: AsyncRead + Unpin>(
    reader: &mut R,
    n: usize,
    stop: &AtomicBool,
    wake: &Notify,
) -> Result<()> {
    tokio::select! {
        biased;
        () = wait_stop(stop, wake) => Err(SdkError::message("mux stopped")),
        result = discard(reader, n) => result,
    }
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
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
) -> Result<()> {
    loop {
        if admit_deferred_opens(&shared, &accept_tx)? {
            continue;
        }
        let waiting = !lock_deferred(&shared.deferred_opens).is_empty();
        let header = tokio::select! {
            biased;
            () = wait_stop(&stop, &wake) => return Ok(()),
            () = shared.admit_wake.notified(), if waiting => continue,
            result = read_header(&mut reader) => result?,
        };
        match header.typ {
            TYPE_OPEN => {
                if header.payload_len != 0 || !peer_id_allowed(id_base, header.conn_id) {
                    return Err(SdkError::message("mux rejected open"));
                }
                if lock_map(&shared.map).contains_key(&header.conn_id) {
                    return Err(SdkError::message("mux duplicate stream id"));
                }
                if id_waits_for_close(&shared, header.conn_id) {
                    let mut deferred = lock_deferred(&shared.deferred_opens);
                    if deferred.len() >= MAX_LIVE_STREAMS {
                        let _ = shared.control_tx.try_send(Control::Close(header.conn_id));
                    } else {
                        deferred.push_back(header.conn_id);
                    }
                    continue;
                }
                if !enqueue_peer_open(&shared, &accept_tx, header.conn_id)? {
                    let _ = shared.control_tx.try_send(Control::Close(header.conn_id));
                }
            }
            TYPE_DATA => {
                if !admit_data(&shared, header.conn_id, header.payload_len) {
                    discard_unless_stopped(&mut reader, header.payload_len, &stop, &wake).await?;
                    shared.close_peer(header.conn_id);
                    continue;
                }
                let mut payload = vec![0u8; header.payload_len];
                read_exact_unless_stopped(&mut reader, &mut payload, &stop, &wake).await?;
                enqueue_data(&shared, header.conn_id, payload);
            }
            TYPE_CLOSE => {
                if header.payload_len != 0 {
                    discard_unless_stopped(&mut reader, header.payload_len, &stop, &wake).await?;
                }
                shared.close_peer(header.conn_id);
            }
            TYPE_WINDOW => {
                if header.payload_len != 4 {
                    return Err(SdkError::message("mux window payload must be 4 bytes"));
                }
                let mut bytes = [0u8; 4];
                read_exact_unless_stopped(&mut reader, &mut bytes, &stop, &wake).await?;
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

/// Drops the data queue and wakes writers when the writer task ends or is aborted.
struct WriterExit {
    data_rx: Option<mpsc::Receiver<OutData>>,
    data_wakers: Arc<Mutex<Vec<Waker>>>,
    state: Arc<WriterState>,
}

impl Drop for WriterExit {
    fn drop(&mut self) {
        self.data_rx.take();
        wake_data_writers(&self.data_wakers);
        if lock_fault(&self.state.fault).is_none() {
            fail_writer(&self.state, &SdkError::message("mux stopped"));
        } else {
            wake_flush_wakers(&self.state);
        }
        stop_writer(&self.state);
    }
}

async fn writer_task<W: AsyncWrite + Unpin>(
    mut writer: W,
    state: Arc<WriterState>,
    mut control_rx: mpsc::Receiver<Control>,
    data_rx: mpsc::Receiver<OutData>,
    data_wakers: Arc<Mutex<Vec<Waker>>>,
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
) {
    let mut exit = WriterExit {
        data_rx: Some(data_rx),
        data_wakers,
        state: Arc::clone(&state),
    };
    let mut data_open = true;
    let mut held: Option<OutData> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let flushed = flush_outbound(&mut writer, &state).await;
        if let Err(err) = flushed {
            fail_writer(&state, &err);
            break;
        }
        if let Some(data) = held.take() {
            if local_payload_ready(&state, data.generation) {
                if let Err(err) = emit_data(&mut writer, &state, data).await {
                    fail_writer(&state, &err);
                    break;
                }
            } else {
                held = Some(data);
            }
        }
        if flushed.unwrap_or(false) {
            continue;
        }
        tokio::select! {
            biased;
            () = wait_stop(&stop, &wake) => break,
            ctrl = control_rx.recv() => {
                let Some(ctrl) = ctrl else {
                    break;
                };
                // `Open` is written from the pending-open queue so it stays ahead of Close.
                if let Control::Close(id) = ctrl {
                    if let Err(err) =
                        write_unless_stopped(&mut writer, &state, TYPE_CLOSE, id, &[]).await
                    {
                        fail_writer(&state, &err);
                        break;
                    }
                }
            }
            data = recv_out_data(exit.data_rx.as_mut()), if data_open && held.is_none() => {
                let Some(data) = data else {
                    data_open = false;
                    continue;
                };
                wake_data_writers(&exit.data_wakers);
                if local_payload_ready(&state, data.generation) {
                    if let Err(err) = emit_data(&mut writer, &state, data).await {
                        fail_writer(&state, &err);
                        break;
                    }
                } else {
                    held = Some(data);
                }
            }
        }
    }
}

async fn recv_out_data(rx: Option<&mut mpsc::Receiver<OutData>>) -> Option<OutData> {
    match rx {
        Some(rx) => rx.recv().await,
        None => None,
    }
}

/// Peer streams are always ready. A local stream waits until its `Open` is written.
fn local_payload_ready(state: &WriterState, generation: u64) -> bool {
    let gens = lock_local_gens(&state.local_gens);
    match gens.iter().find(|gen| gen.generation == generation) {
        None => true,
        Some(gen) => gen.open_written,
    }
}

fn note_close_written(state: &WriterState, generation: u64) {
    {
        let closes = lock_closes(&state.closes);
        if let Some(hold) = closes.iter().find(|hold| hold.generation == generation) {
            hold.close_flag.store(true, Ordering::Release);
        }
    }
    {
        let mut gens = lock_local_gens(&state.local_gens);
        if let Some(gen) = gens.iter_mut().find(|gen| gen.generation == generation) {
            gen.close_written = true;
        }
        gens.retain(|gen| !gen.close_written);
    }
    state.admit_wake.notify_waiters();
    wake_flush_wakers(state);
}

fn lock_flush_wakers(wakers: &Mutex<Vec<Waker>>) -> std::sync::MutexGuard<'_, Vec<Waker>> {
    wakers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wake_flush_wakers(state: &WriterState) {
    let parked = std::mem::take(&mut *lock_flush_wakers(&state.flush_wakers));
    for waker in parked {
        waker.wake();
    }
}

fn stream_incomplete_error(state: &WriterState, done: bool) -> Option<std::io::Error> {
    if done {
        return None;
    }
    if let Some(message) = lock_fault(&state.fault).clone() {
        return Some(std::io::Error::other(message));
    }
    if state.writer_stopped.load(Ordering::Acquire) {
        return Some(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "mux writer gone",
        ));
    }
    None
}

fn lock_fault(fault: &Mutex<Option<String>>) -> std::sync::MutexGuard<'_, Option<String>> {
    fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn fail_writer(state: &WriterState, err: &SdkError) {
    {
        let mut fault = lock_fault(&state.fault);
        if fault.is_none() {
            *fault = Some(err.to_string());
        }
    }
    wake_flush_wakers(state);
}

fn stop_writer(state: &WriterState) {
    state.writer_stopped.store(true, Ordering::Release);
    wake_flush_wakers(state);
}

fn open_is_eligible(open: &PendingOpen, gens: &[LocalGen]) -> bool {
    !gens
        .iter()
        .any(|gen| gen.id == open.id && gen.generation < open.generation && !gen.close_written)
}

/// Writes the oldest eligible local `Open`, if there is one.
///
/// # Errors
///
/// Returns [`SdkError`] when the frame cannot be written.
async fn write_one_pending_open<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &WriterState,
) -> Result<bool> {
    let next = {
        let pending = lock_pending_opens(&state.pending_opens);
        let gens = lock_local_gens(&state.local_gens);
        pending.front().and_then(|open| {
            open_is_eligible(open, &gens).then_some(PendingOpen {
                id: open.id,
                generation: open.generation,
            })
        })
    };
    let Some(open) = next else {
        return Ok(false);
    };
    write_unless_stopped(writer, state, TYPE_OPEN, open.id, &[]).await?;
    if lock_pending_opens(&state.pending_opens)
        .front()
        .is_some_and(|item| item.generation == open.generation)
    {
        lock_pending_opens(&state.pending_opens).pop_front();
    } else {
        lock_pending_opens(&state.pending_opens).retain(|item| item.generation != open.generation);
    }
    if let Some(gen) = lock_local_gens(&state.local_gens)
        .iter_mut()
        .find(|gen| gen.generation == open.generation)
    {
        gen.open_written = true;
    }
    Ok(true)
}

/// Writes every queued `Open`, then the window credit and `Close` frames that
/// already have that `Open` on the wire.
///
/// # Errors
///
/// Returns [`SdkError`] when a frame cannot be written. Credit and close
/// accounting is restored first.
async fn flush_outbound<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &WriterState,
) -> Result<bool> {
    let mut wrote = false;
    loop {
        let opened = write_one_pending_open(writer, state).await?;
        let rest = flush_credits_and_closes(writer, state).await?;
        if opened || rest {
            wrote = true;
            wake_flush_wakers(state);
        } else {
            break;
        }
    }
    Ok(wrote)
}

/// Writes window credit and ready `Close` frames that already have their `Open`.
///
/// # Errors
///
/// Returns [`SdkError`] when a frame cannot be written. Credit and close
/// accounting is restored first.
async fn flush_credits_and_closes<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &WriterState,
) -> Result<bool> {
    let mut wrote = false;
    let credits = take_credits(state);
    for credit in credits {
        if !credit_still_current(state, &credit) {
            discard_emitting_credit(state, credit.generation);
            continue;
        }
        if let Err(err) = write_unless_stopped(
            writer,
            state,
            TYPE_WINDOW,
            credit.id,
            &credit.n.to_be_bytes(),
        )
        .await
        {
            restore_emitting_credit(state, &credit);
            return Err(err);
        }
        finish_emitting_credit(state, &credit);
        wrote = true;
    }
    let closes = take_ready_closes(state);
    for close in closes {
        if let Err(err) = write_unless_stopped(writer, state, TYPE_CLOSE, close.id, &[]).await {
            restore_emitting_close(state, close.generation);
            return Err(err);
        }
        finish_emitting_close(state, close.generation);
        wrote = true;
    }
    Ok(wrote)
}

struct TakenCredit {
    id: u32,
    generation: u64,
    n: u32,
}

fn take_credits(state: &WriterState) -> Vec<TakenCredit> {
    let mut finished = Vec::new();
    let out = {
        let mut holds = lock_credits(&state.credits);
        let mut out = Vec::new();
        let mut index = 0;
        while index < holds.len() {
            if holds[index].emitting {
                index += 1;
                continue;
            }
            if !local_payload_ready(state, holds[index].generation) {
                index += 1;
                continue;
            }
            let n = holds[index].pending.swap(0, Ordering::AcqRel);
            if n > 0 {
                holds[index].emitting = true;
                out.push(TakenCredit {
                    id: holds[index].id,
                    generation: holds[index].generation,
                    n,
                });
                index += 1;
                continue;
            }
            let keep = holds[index].alive.load(Ordering::Acquire)
                || holds[index].pending.load(Ordering::Acquire) > 0;
            if keep {
                index += 1;
            } else {
                finished.push(holds[index].generation);
                holds.remove(index);
            }
        }
        out
    };
    for generation in finished {
        mark_book_credit_done(state, generation);
    }
    out
}

fn restore_emitting_credit(state: &WriterState, credit: &TakenCredit) {
    let mut holds = lock_credits(&state.credits);
    if let Some(hold) = holds
        .iter_mut()
        .find(|hold| hold.generation == credit.generation)
    {
        hold.emitting = false;
        hold.pending.fetch_add(credit.n, Ordering::AcqRel);
    }
}

fn finish_emitting_credit(state: &WriterState, credit: &TakenCredit) {
    let done = {
        let mut holds = lock_credits(&state.credits);
        let Some(pos) = holds
            .iter()
            .position(|hold| hold.generation == credit.generation)
        else {
            return;
        };
        holds[pos].emitting = false;
        let drop_it = !holds[pos].alive.load(Ordering::Acquire)
            && holds[pos].pending.load(Ordering::Acquire) == 0;
        if drop_it {
            holds.remove(pos);
        }
        drop_it
    };
    if done {
        mark_book_credit_done(state, credit.generation);
    }
}

fn discard_emitting_credit(state: &WriterState, generation: u64) {
    let removed = {
        let mut holds = lock_credits(&state.credits);
        let before = holds.len();
        holds.retain(|hold| hold.generation != generation);
        holds.len() != before
    };
    if removed {
        mark_book_credit_done(state, generation);
    }
}

/// True when no newer live stream has taken this id.
///
/// Credit for a stream that is already gone is still written unless a
/// replacement generation is registered. That replacement must not inherit
/// the old stream's window.
fn credit_still_current(state: &WriterState, credit: &TakenCredit) -> bool {
    let holds = lock_credits(&state.credits);
    !holds.iter().any(|hold| {
        hold.id == credit.id
            && hold.generation != credit.generation
            && hold.alive.load(Ordering::Acquire)
    })
}

struct ReadyClose {
    id: u32,
    generation: u64,
}

fn take_ready_closes(state: &WriterState) -> Vec<ReadyClose> {
    let mut finished = Vec::new();
    let ready = {
        let mut closes = lock_closes(&state.closes);
        let mut ready = Vec::new();
        let mut index = 0;
        while index < closes.len() {
            if closes[index].emitting {
                index += 1;
                continue;
            }
            if !local_payload_ready(state, closes[index].generation) {
                index += 1;
                continue;
            }
            if !closes[index].close_pending.load(Ordering::Acquire) {
                finished.push(closes[index].generation);
                closes.remove(index);
                continue;
            }
            if closes[index].outbound_queued.load(Ordering::Acquire) > 0 {
                index += 1;
                continue;
            }
            if closes[index].close_pending.swap(false, Ordering::AcqRel) {
                closes[index].emitting = true;
                ready.push(ReadyClose {
                    id: closes[index].id,
                    generation: closes[index].generation,
                });
            }
            index += 1;
        }
        ready
    };
    for generation in finished {
        mark_book_close_done(state, generation);
        note_close_written(state, generation);
    }
    ready
}

fn restore_emitting_close(state: &WriterState, generation: u64) {
    let mut closes = lock_closes(&state.closes);
    if let Some(hold) = closes.iter_mut().find(|hold| hold.generation == generation) {
        hold.emitting = false;
        hold.close_pending.store(true, Ordering::SeqCst);
    }
}

fn finish_emitting_close(state: &WriterState, generation: u64) {
    let removed = {
        let mut closes = lock_closes(&state.closes);
        if let Some(hold) = closes.iter().find(|hold| hold.generation == generation) {
            hold.close_flag.store(true, Ordering::Release);
        }
        let before = closes.len();
        closes.retain(|hold| hold.generation != generation);
        closes.len() != before
    };
    if removed {
        mark_book_close_done(state, generation);
        note_close_written(state, generation);
    }
}

/// Writes one data frame and records a trailing `Close` when that was the last one.
///
/// # Errors
///
/// Returns [`SdkError`] when the underlying write fails.
async fn emit_data<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &WriterState,
    data: OutData,
) -> Result<()> {
    let generation = data.generation;
    if write_out_data(writer, state, data).await? {
        note_close_written(state, generation);
    } else {
        wake_flush_wakers(state);
    }
    Ok(())
}

/// Writes one queued Data frame, then `Close` if this was the last accepted
/// frame and the stream has already shut down.
///
/// # Errors
///
/// Returns [`SdkError`] when the underlying write or flush fails.
async fn write_out_data<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &WriterState,
    data: OutData,
) -> Result<bool> {
    write_unless_stopped(writer, state, TYPE_DATA, data.id, &data.payload).await?;
    let left = data.outbound_queued.fetch_sub(1, Ordering::AcqRel);
    if left == 1 && data.close_pending.swap(false, Ordering::AcqRel) {
        if let Err(err) = write_unless_stopped(writer, state, TYPE_CLOSE, data.id, &[]).await {
            data.close_pending.store(true, Ordering::SeqCst);
            return Err(err);
        }
        return Ok(true);
    }
    Ok(false)
}

/// Writes one frame unless forced cancellation wins first.
///
/// # Errors
///
/// Returns [`SdkError`] when the underlying write fails or the mux is stopped.
async fn write_unless_stopped<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &WriterState,
    typ: u8,
    conn_id: u32,
    payload: &[u8],
) -> Result<()> {
    tokio::select! {
        biased;
        () = wait_stop(&state.stop, &state.wake) => Err(SdkError::message("mux stopped")),
        result = write_raw(writer, typ, conn_id, payload) => result,
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
                self.read_eof = true;
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
        // `read_eof` and `peer_gone` reject further writes. `closed` is the
        // outbound Close, which is queued separately in `poll_shutdown`.
        if self.closed || self.read_eof || self.peer_gone.load(Ordering::SeqCst) {
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
        self.outbound_queued.fetch_add(1, Ordering::AcqRel);
        let frame = OutData {
            id: self.id,
            generation: self.generation,
            payload: buf[..n].to_vec(),
            outbound_queued: Arc::clone(&self.outbound_queued),
            close_pending: Arc::clone(&self.close_pending),
        };
        let queued = match self.shared.data_tx.try_send(frame) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.outbound_queued.fetch_sub(1, Ordering::AcqRel);
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "mux writer gone",
                )));
            }
            Err(mpsc::error::TrySendError::Full(frame)) => {
                lock_data_wakers(&self.shared.data_wakers).push(cx.waker().clone());
                match self.shared.data_tx.try_send(frame) {
                    Ok(()) => true,
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        self.outbound_queued.fetch_sub(1, Ordering::AcqRel);
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "mux writer gone",
                        )));
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.outbound_queued.fetch_sub(1, Ordering::AcqRel);
                        return Poll::Pending;
                    }
                }
            }
        };
        if queued {
            self.send_credit.fetch_sub(n as u32, Ordering::AcqRel);
            self.in_flight.fetch_add(n as u32, Ordering::AcqRel);
            Poll::Ready(Ok(n))
        } else {
            Poll::Pending
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let done = self.outbound_queued.load(Ordering::Acquire) == 0;
        if let Some(err) = stream_incomplete_error(&self.shared.writer_state, done) {
            return Poll::Ready(Err(err));
        }
        if done {
            return Poll::Ready(Ok(()));
        }
        lock_flush_wakers(&self.shared.writer_state.flush_wakers).push(cx.waker().clone());
        let done = self.outbound_queued.load(Ordering::Acquire) == 0;
        if let Some(err) = stream_incomplete_error(&self.shared.writer_state, done) {
            return Poll::Ready(Err(err));
        }
        if done {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if !self.closed {
            if self.announced {
                self.queue_close();
            }
            self.closed = true;
        }
        let close_done = !self.announced || self.close_flag.load(Ordering::Acquire);
        let done = self.outbound_queued.load(Ordering::Acquire) == 0 && close_done;
        if let Some(err) = stream_incomplete_error(&self.shared.writer_state, done) {
            return Poll::Ready(Err(err));
        }
        if done {
            return Poll::Ready(Ok(()));
        }
        lock_flush_wakers(&self.shared.writer_state.flush_wakers).push(cx.waker().clone());
        let close_done = !self.announced || self.close_flag.load(Ordering::Acquire);
        let done = self.outbound_queued.load(Ordering::Acquire) == 0 && close_done;
        if let Some(err) = stream_incomplete_error(&self.shared.writer_state, done) {
            return Poll::Ready(Err(err));
        }
        if done {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
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

    fn bookkeeping_idle(mux: &Mux) -> bool {
        lock_map(&mux.shared.map).is_empty()
            && lock_records(&mux.shared.writer_state.records).is_empty()
            && lock_credits(&mux.shared.writer_state.credits).is_empty()
            && lock_closes(&mux.shared.writer_state.closes).is_empty()
            && lock_local_gens(&mux.shared.writer_state.local_gens).is_empty()
            && lock_pending_opens(&mux.shared.writer_state.pending_opens).is_empty()
    }

    async fn wait_bookkeeping_idle(mux: &Mux) {
        let idle = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !bookkeeping_idle(mux) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            idle.is_ok(),
            "bookkeeping did not return to baseline: map {} records {} credits {} closes {}",
            lock_map(&mux.shared.map).len(),
            lock_records(&mux.shared.writer_state.records).len(),
            lock_credits(&mux.shared.writer_state.credits).len(),
            lock_closes(&mux.shared.writer_state.closes).len(),
        );
    }

    /// Peer Close, read-to-end, and drop must retire the generation. Otherwise
    /// the 33rd open is rejected while the writer is healthy.
    #[tokio::test]
    async fn peer_close_read_to_end_drop_releases_admission() {
        let (client, server) = pair();
        for i in 0..(MAX_LIVE_STREAMS + 4) {
            let mut local = client
                .open()
                .await
                .unwrap_or_else(|err| panic!("open {i} after peer EOF drops: {err}"));
            let mut remote = server.accept().await.expect("accept");
            remote.shutdown().await.expect("peer close");
            let mut buf = Vec::new();
            local.read_to_end(&mut buf).await.expect("read eof");
            assert!(buf.is_empty(), "peer close carried payload");
            drop(local);
            drop(remote);
            wait_bookkeeping_idle(&client).await;
            wait_bookkeeping_idle(&server).await;
        }
        let mut local = client.open().await.expect("open after the cap");
        let mut remote = server.accept().await.expect("accept after the cap");
        local.write_all(b"ok").await.expect("write");
        local.shutdown().await.expect("local shutdown");
        let mut buf = Vec::new();
        remote.read_to_end(&mut buf).await.expect("echo read");
        assert_eq!(buf, b"ok");
        drop(local);
        drop(remote);
        wait_bookkeeping_idle(&client).await;
        wait_bookkeeping_idle(&server).await;
    }

    /// Reading EOF must not make `shutdown` wait for a Close that was never queued.
    #[tokio::test]
    async fn peer_eof_then_shutdown_completes_and_releases_admission() {
        let (client, server) = pair();
        let mut local = client.open().await.expect("open");
        let mut remote = server.accept().await.expect("accept");
        remote.shutdown().await.expect("peer close");
        let mut buf = Vec::new();
        local.read_to_end(&mut buf).await.expect("read eof");
        let shutdown = tokio::time::timeout(std::time::Duration::from_secs(2), local.shutdown())
            .await
            .expect("shutdown hung after peer EOF");
        shutdown.expect("shutdown");
        drop(local);
        drop(remote);
        wait_bookkeeping_idle(&client).await;
        wait_bookkeeping_idle(&server).await;
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

    /// Accepting and dropping peer streams while the writer never runs must
    /// not grow credit records, close records, queues, or tasks.
    #[tokio::test]
    async fn accept_drop_churn_with_blocked_writer_bounds_bookkeeping() {
        let gate = Arc::new(AtomicBool::new(false));
        let waker_slot = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (server_io, peer) = duplex(256 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = Mux::server(
            sr,
            GateWriter {
                inner: sw,
                gate: Arc::clone(&gate),
                waker_slot: Arc::clone(&waker_slot),
                log,
                buf: Vec::new(),
            },
        );
        let consumer = server.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_consumer = Arc::clone(&stop);
        let accept_task = tokio::spawn(async move {
            loop {
                if stop_consumer.load(Ordering::SeqCst) {
                    break;
                }
                match tokio::time::timeout(std::time::Duration::from_millis(20), consumer.accept())
                    .await
                {
                    Ok(Ok(stream)) => drop(stream),
                    Ok(Err(_)) => break,
                    Err(_) => continue,
                }
            }
        });
        let (_pr, mut pw) = tokio::io::split(peer);
        let opens = MAX_LIVE_STREAMS * 8;
        for i in 0..opens {
            let id = (i as u32) * 2 + 1;
            write_open(&mut pw, id).await;
            assert!(
                lock_map(&server.shared.map).len() <= MAX_LIVE_STREAMS,
                "map grew past the cap"
            );
            assert!(
                lock_credits(&server.shared.writer_state.credits).len() <= MAX_LIVE_STREAMS,
                "credit records grew past the cap"
            );
            assert!(
                lock_closes(&server.shared.writer_state.closes).len() <= MAX_LIVE_STREAMS,
                "close records grew past the cap"
            );
            assert!(
                lock_records(&server.shared.writer_state.records).len() <= MAX_LIVE_STREAMS,
                "bookkeeping reservations grew past the cap"
            );
            assert!(
                server.shared.accept_queued.load(Ordering::SeqCst) <= MAX_LIVE_STREAMS,
                "accept queue grew past the cap"
            );
            assert_eq!(
                server.tasks.remaining.load(Ordering::SeqCst),
                2,
                "mux spawned extra transport tasks"
            );
            let control_queued =
                server.shared.control_tx.max_capacity() - server.shared.control_tx.capacity();
            let data_queued =
                server.shared.data_tx.max_capacity() - server.shared.data_tx.capacity();
            assert!(control_queued <= WRITER_CONTROL_QUEUE);
            assert!(data_queued <= WRITER_DATA_QUEUE);
        }
        let drained = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let map_len = lock_map(&server.shared.map).len();
                let waiting = server.shared.accept_queued.load(Ordering::SeqCst);
                if map_len == 0 && waiting == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            drained.is_ok(),
            "accepted streams were not dropped; map={} accept={}",
            lock_map(&server.shared.map).len(),
            server.shared.accept_queued.load(Ordering::SeqCst)
        );
        assert!(
            server
                .shared
                .writer_state
                .credit_hwm
                .load(Ordering::Relaxed)
                >= 1
        );
        assert!(server.shared.writer_state.close_hwm.load(Ordering::Relaxed) >= 1);
        assert_eq!(
            server
                .shared
                .writer_state
                .record_hwm
                .load(Ordering::Relaxed),
            MAX_LIVE_STREAMS,
            "blocked writer did not hold the bookkeeping cap"
        );
        assert!(
            server
                .shared
                .writer_state
                .credit_hwm
                .load(Ordering::Relaxed)
                <= MAX_LIVE_STREAMS
        );
        assert!(server.shared.writer_state.close_hwm.load(Ordering::Relaxed) <= MAX_LIVE_STREAMS);
        assert!(server.shared.map_hwm.load(Ordering::Relaxed) <= MAX_LIVE_STREAMS);
        assert!(server.shared.accept_hwm.load(Ordering::Relaxed) <= MAX_LIVE_STREAMS);
        assert_eq!(
            server.shared.control_tx.max_capacity(),
            WRITER_CONTROL_QUEUE
        );
        assert_eq!(server.shared.data_tx.max_capacity(), WRITER_DATA_QUEUE);
        assert!(lock_credits(&server.shared.writer_state.credits).len() <= MAX_LIVE_STREAMS);
        assert!(lock_closes(&server.shared.writer_state.closes).len() <= MAX_LIVE_STREAMS);
        assert!(lock_records(&server.shared.writer_state.records).len() <= MAX_LIVE_STREAMS);
        assert_eq!(server.tasks.remaining.load(Ordering::SeqCst), 2);
        assert!(
            server
                .shared
                .writer_state
                .record_hwm
                .load(Ordering::Relaxed)
                > 0,
            "churn never admitted a stream"
        );
        let _ = gate;
        let _ = waker_slot;
        stop.store(true, Ordering::SeqCst);
        accept_task.abort();
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
                .try_send(OutData {
                    id: id as u32,
                    generation: 0,
                    payload: vec![0xAB],
                    outbound_queued: Arc::new(AtomicUsize::new(1)),
                    close_pending: Arc::new(AtomicBool::new(false)),
                })
                .unwrap();
        }
        let pending = Arc::new(AtomicU32::new(1));
        let state = Arc::new(WriterState {
            credits: Mutex::new(vec![CreditHold {
                id: 7,
                generation: 1,
                pending: Arc::clone(&pending),
                alive: Arc::new(AtomicBool::new(true)),
                emitting: false,
            }]),
            closes: Mutex::new(Vec::new()),
            records: Mutex::new(Vec::new()),
            pending_opens: Mutex::new(VecDeque::new()),
            local_gens: Mutex::new(Vec::new()),
            admit_wake: Arc::new(Notify::new()),
            fault: Mutex::new(None),
            flush_wakers: Mutex::new(Vec::new()),
            writer_stopped: AtomicBool::new(false),
            stop: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            credit_hwm: AtomicUsize::new(0),
            close_hwm: AtomicUsize::new(0),
            record_hwm: AtomicUsize::new(0),
        });
        let log = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter {
            log: Arc::clone(&log),
            buf: Vec::new(),
        };
        let task = tokio::spawn(writer_task(
            writer,
            Arc::clone(&state),
            control_rx,
            data_rx,
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Notify::new()),
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

    #[tokio::test]
    async fn coalesced_window_survives_a_blocked_writer() {
        let (client_io, server_io) = duplex(INITIAL_WINDOW as usize + 256 * 1024);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let gate = Arc::new(AtomicBool::new(true));
        let waker_slot = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = Mux::client(cr, cw);
        let server = Mux::server(
            sr,
            GateWriter {
                inner: sw,
                gate: Arc::clone(&gate),
                waker_slot: Arc::clone(&waker_slot),
                log: Arc::clone(&log),
                buf: Vec::new(),
            },
        );
        let mut sent = client.open().await.expect("open");
        let mut recv = server.accept().await.expect("accept");
        let payload = vec![0x11u8; INITIAL_WINDOW as usize];
        let send = tokio::spawn(async move {
            sent.write_all(&payload).await.expect("fill window");
            sent
        });
        let mut one = [0u8; 1];
        recv.read_exact(&mut one).await.expect("first byte");
        gate.store(false, Ordering::SeqCst);
        let mut rest = vec![0u8; INITIAL_WINDOW as usize - 1];
        recv.read_exact(&mut rest).await.expect("rest of window");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let blocked = window_credit(&log);
        assert!(
            blocked < INITIAL_WINDOW,
            "writer delivered every window while it should have been blocked: {blocked}"
        );
        gate.store(true, Ordering::SeqCst);
        if let Some(waker) = waker_slot.lock().unwrap().take() {
            waker.wake();
        }
        let credited = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if window_credit(&log) >= INITIAL_WINDOW {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            credited.is_ok(),
            "coalesced credit never arrived: {}",
            window_credit(&log)
        );
        let mut sent = send.await.expect("join writer");
        let extra = vec![0x22u8; 4096];
        tokio::time::timeout(std::time::Duration::from_secs(2), sent.write_all(&extra))
            .await
            .expect("later payload must flow after credit")
            .expect("write extra");
        let mut got = vec![0u8; extra.len()];
        recv.read_exact(&mut got).await.expect("read extra");
        assert_eq!(got, extra);
    }

    #[tokio::test]
    async fn open_then_immediate_drop_writes_open_before_close() {
        let gate = Arc::new(AtomicBool::new(false));
        let waker_slot = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (local, remote) = duplex(64 * 1024);
        let (lr, lw) = tokio::io::split(local);
        let (mut rr, _rw) = tokio::io::split(remote);
        let client = Mux::client(
            lr,
            GateWriter {
                inner: lw,
                gate: Arc::clone(&gate),
                waker_slot: Arc::clone(&waker_slot),
                log: Arc::clone(&log),
                buf: Vec::new(),
            },
        );
        let stream = client.open().await.expect("open");
        drop(stream);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "blocked writer emitted frames before the gate opened: {:?}",
            log.lock().unwrap()
        );
        gate.store(true, Ordering::SeqCst);
        if let Some(waker) = waker_slot.lock().unwrap().take() {
            waker.wake();
        }
        let frames = read_frames_until_close(&mut rr).await;
        assert_eq!(
            frames.iter().map(|(typ, _, _)| *typ).collect::<Vec<_>>(),
            vec![TYPE_OPEN, TYPE_CLOSE],
            "immediate drop must be Open then Close: {frames:?}"
        );
        assert_eq!(frames[0].1, frames[1].1);
    }

    #[tokio::test]
    async fn shutdown_before_accept_writes_open_data_close_then_eof() {
        let (local, remote) = duplex(64 * 1024);
        let (lr, lw) = tokio::io::split(local);
        let (mut rr, _rw) = tokio::io::split(remote);
        let client = Mux::client(lr, lw);
        let mut stream = client.open().await.expect("open");
        let payload = b"hello-ordered";
        stream.write_all(payload).await.expect("write");
        stream.shutdown().await.expect("shutdown");
        drop(stream);
        let frames = read_frames_until_close(&mut rr).await;
        let types: Vec<u8> = frames.iter().map(|(typ, _, _)| *typ).collect();
        assert_eq!(
            types,
            vec![TYPE_OPEN, TYPE_DATA, TYPE_CLOSE],
            "shutdown before accept must be Open, Data, Close: {frames:?}"
        );
        let data: Vec<u8> = frames
            .iter()
            .filter(|(typ, _, _)| *typ == TYPE_DATA)
            .flat_map(|(_, _, payload)| payload.clone())
            .collect();
        assert_eq!(data, payload);
        drop(client);
        let mut tail = [0u8; 1];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), rr.read(&mut tail))
            .await
            .expect("eof timed out")
            .expect("read after close");
        assert_eq!(n, 0, "peer did not observe EOF after mux shutdown");
    }

    #[tokio::test]
    async fn flush_and_shutdown_wait_until_the_blocked_writer_finishes() {
        let gate = Arc::new(AtomicBool::new(false));
        let waker_slot = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (local, remote) = duplex(64 * 1024);
        let (lr, lw) = tokio::io::split(local);
        let (mut rr, _rw) = tokio::io::split(remote);
        let client = Mux::client(
            lr,
            GateWriter {
                inner: lw,
                gate: Arc::clone(&gate),
                waker_slot: Arc::clone(&waker_slot),
                log: Arc::clone(&log),
                buf: Vec::new(),
            },
        );
        let mut stream = client.open().await.expect("open");
        let payload = b"flush-me";
        stream.write_all(payload).await.expect("queue");
        let flush_blocked =
            tokio::time::timeout(std::time::Duration::from_millis(40), stream.flush()).await;
        assert!(
            flush_blocked.is_err(),
            "flush returned before the blocked writer finished"
        );
        let mut stream = stream;
        let shutdown_blocked =
            tokio::time::timeout(std::time::Duration::from_millis(40), stream.shutdown()).await;
        assert!(
            shutdown_blocked.is_err(),
            "shutdown returned before Close was written"
        );
        gate.store(true, Ordering::SeqCst);
        if let Some(waker) = waker_slot.lock().unwrap().take() {
            waker.wake();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.shutdown())
            .await
            .expect("shutdown timed out")
            .expect("shutdown");
        let frames = read_frames_until_close(&mut rr).await;
        let data: Vec<u8> = frames
            .iter()
            .filter(|(typ, _, _)| *typ == TYPE_DATA)
            .flat_map(|(_, _, payload)| payload.clone())
            .collect();
        assert_eq!(data, payload);
        assert_eq!(
            frames.iter().map(|(typ, _, _)| *typ).collect::<Vec<_>>(),
            vec![TYPE_OPEN, TYPE_DATA, TYPE_CLOSE]
        );
    }

    #[tokio::test]
    async fn flush_returns_an_injected_writer_error() {
        let (local, _remote) = duplex(4096);
        let (lr, _lw) = tokio::io::split(local);
        let client = Mux::client(lr, BoomWriter);
        let mut stream = client.open().await.expect("open queued");
        stream.write_all(b"x").await.expect("queue");
        let err = tokio::time::timeout(std::time::Duration::from_secs(2), stream.flush())
            .await
            .expect("flush timed out")
            .expect_err("flush hid the writer error");
        assert!(
            err.to_string().contains("injected write failure"),
            "flush error was {err}"
        );
        let err = tokio::time::timeout(std::time::Duration::from_secs(2), stream.shutdown())
            .await
            .expect("shutdown timed out")
            .expect_err("shutdown hid the writer error");
        assert!(
            err.to_string().contains("injected write failure"),
            "shutdown error was {err}"
        );
    }

    #[tokio::test]
    async fn queued_open_during_forced_cancel_emits_nothing() {
        let gate = Arc::new(AtomicBool::new(false));
        let waker_slot = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (local, remote) = duplex(64 * 1024);
        let (lr, lw) = tokio::io::split(local);
        let (mut rr, _rw) = tokio::io::split(remote);
        let client = Mux::client(
            lr,
            GateWriter {
                inner: lw,
                gate: Arc::clone(&gate),
                waker_slot: Arc::clone(&waker_slot),
                log: Arc::clone(&log),
                buf: Vec::new(),
            },
        );
        let observed = client.clone();
        let stream = client.open().await.expect("open");
        drop(stream);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        client.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(2), observed.closed())
            .await
            .expect("forced cancel did not finish the tasks");
        gate.store(true, Ordering::SeqCst);
        if let Some(waker) = waker_slot.lock().unwrap().take() {
            waker.wake();
        }
        assert!(
            log.lock().unwrap().is_empty(),
            "forced cancel drained queued Open/Close: {:?}",
            log.lock().unwrap()
        );
        let mut tail = [0u8; 1];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), rr.read(&mut tail))
            .await
            .expect("eof timed out")
            .expect("read");
        assert_eq!(n, 0, "peer did not observe EOF after forced cancel");
        drop(client);
    }

    #[tokio::test]
    async fn reused_id_is_not_admitted_until_the_previous_close_is_written() {
        let gate = Arc::new(AtomicBool::new(false));
        let waker_slot = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (local, remote) = duplex(64 * 1024);
        let (lr, lw) = tokio::io::split(local);
        let (_rr, mut rw) = tokio::io::split(remote);
        let server = Mux::server(
            lr,
            GateWriter {
                inner: lw,
                gate: Arc::clone(&gate),
                waker_slot: Arc::clone(&waker_slot),
                log: Arc::clone(&log),
                buf: Vec::new(),
            },
        );
        write_open(&mut rw, 1).await;
        let first = server.accept().await.expect("first");
        drop(first);
        write_open(&mut rw, 1).await;
        let early =
            tokio::time::timeout(std::time::Duration::from_millis(80), server.accept()).await;
        assert!(
            early.is_err(),
            "reused id was admitted before the previous Close was written"
        );
        gate.store(true, Ordering::SeqCst);
        if let Some(waker) = waker_slot.lock().unwrap().take() {
            waker.wake();
        }
        let second = tokio::time::timeout(std::time::Duration::from_secs(2), server.accept())
            .await
            .expect("second open was not admitted after Close")
            .expect("accept");
        assert_eq!(second.id(), 1);
        let frames = log.lock().unwrap().clone();
        let close_at = frames
            .iter()
            .position(|(typ, id, _)| *typ == TYPE_CLOSE && *id == 1)
            .expect("close frame");
        assert_eq!(
            close_at, 0,
            "a frame preceded Close for the retired generation: {frames:?}"
        );
        drop(second);
    }

    async fn read_frames_until_close(
        reader: &mut (impl AsyncRead + Unpin),
    ) -> Vec<(u8, u32, Vec<u8>)> {
        let mut frames = Vec::new();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let header = read_header(reader).await.expect("frame header");
                let mut payload = vec![0u8; header.payload_len];
                if header.payload_len > 0 {
                    reader.read_exact(&mut payload).await.expect("payload");
                }
                let typ = header.typ;
                frames.push((typ, header.conn_id, payload));
                if typ == TYPE_CLOSE {
                    break;
                }
            }
        })
        .await;
        assert!(finished.is_ok(), "timed out waiting for Close: {frames:?}");
        frames
    }

    #[tokio::test]
    async fn close_stays_behind_accepted_data_when_the_writer_is_blocked() {
        let gate = Arc::new(AtomicBool::new(false));
        let waker_slot = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (reader_io, hold) = duplex(64);
        let (reader, _hold_write) = tokio::io::split(reader_io);
        let _hold = hold;
        let client = Mux::client(
            reader,
            GateWriter {
                inner: tokio::io::sink(),
                gate: Arc::clone(&gate),
                waker_slot: Arc::clone(&waker_slot),
                log: Arc::clone(&log),
                buf: Vec::new(),
            },
        );
        let mut stream = client.open().await.expect("open");
        let payload = b"hello-exact-payload";
        stream.write_all(payload).await.expect("queue data");
        let shutdown = tokio::spawn(async move {
            stream
                .shutdown()
                .await
                .expect("shutdown completes after the writer runs");
            stream
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "blocked writer emitted frames early: {:?}",
            log.lock().unwrap()
        );
        gate.store(true, Ordering::SeqCst);
        if let Some(waker) = waker_slot.lock().unwrap().take() {
            waker.wake();
        }
        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let frames = log.lock().unwrap().clone();
                if frames.iter().any(|(typ, _, _)| *typ == TYPE_CLOSE) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(ready.is_ok(), "close was not written");
        let frames = log.lock().unwrap().clone();
        let data: Vec<u8> = frames
            .iter()
            .filter(|(typ, _, _)| *typ == TYPE_DATA)
            .flat_map(|(_, _, payload)| payload.clone())
            .collect();
        assert_eq!(data, payload);
        let last_data = frames
            .iter()
            .rposition(|(typ, _, _)| *typ == TYPE_DATA)
            .expect("data frame");
        let close_at = frames
            .iter()
            .position(|(typ, _, _)| *typ == TYPE_CLOSE)
            .expect("close frame");
        assert!(
            last_data < close_at,
            "Close moved ahead of accepted Data: {frames:?}"
        );
        shutdown.await.expect("shutdown task");
    }

    type FrameLog = (u8, u32, Vec<u8>);

    #[tokio::test]
    async fn dropping_the_mux_finishes_tasks_while_the_peer_stays_idle() {
        let (client_io, server_io) = duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let reader_gone = Arc::new(AtomicBool::new(false));
        let writer_gone = Arc::new(AtomicBool::new(false));
        let reader_started = Arc::new(AtomicBool::new(false));
        let server = Mux::server(
            IdleReader {
                inner: sr,
                started: Arc::clone(&reader_started),
                gone: Arc::clone(&reader_gone),
            },
            IdleWriter {
                inner: sw,
                gone: Arc::clone(&writer_gone),
            },
        );
        let client = Mux::client(cr, cw);
        let started = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !reader_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(started.is_ok(), "mux reader never polled the idle peer");
        let observed = server.clone();
        drop(server);
        // The client clone is still alive, so this drop is not the last owner.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !reader_gone.load(Ordering::SeqCst),
            "dropping one clone stopped the reader"
        );
        observed.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(2), observed.closed())
            .await
            .expect("mux tasks did not finish");
        assert!(
            reader_gone.load(Ordering::SeqCst),
            "reader still holds the transport"
        );
        assert!(
            writer_gone.load(Ordering::SeqCst),
            "writer still holds the transport"
        );
        drop(client);
    }

    #[tokio::test]
    async fn stream_shutdown_still_writes_queued_data() {
        let (client, server) = pair();
        let mut outgoing = client.open().await.expect("open");
        let mut incoming = server.accept().await.expect("accept");
        outgoing.write_all(b"hello").await.expect("queue");
        outgoing.shutdown().await.expect("graceful shutdown");
        drop(outgoing);
        drop(client);
        let mut buf = [0u8; 5];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            incoming.read_exact(&mut buf),
        )
        .await
        .expect("accepted data was not written by stream shutdown")
        .expect("read");
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn forced_cancel_mid_data_finishes_while_the_frame_stays_partial() {
        forced_cancel_while_stalled(partial_frame(TYPE_DATA, 1, &[0xAB; 32], true), true).await;
    }

    #[tokio::test]
    async fn forced_cancel_mid_window_finishes_while_the_frame_stays_partial() {
        let credit = 1u32.to_be_bytes();
        forced_cancel_while_stalled(partial_frame(TYPE_WINDOW, 1, &credit, true), true).await;
    }

    #[tokio::test]
    async fn forced_cancel_mid_discard_finishes_while_the_frame_stays_partial() {
        forced_cancel_while_stalled(partial_frame(TYPE_DATA, 1, &[0u8; 64], false), false).await;
    }

    async fn forced_cancel_while_stalled(script: Vec<u8>, accept_open: bool) {
        let stalled = Arc::new(AtomicBool::new(false));
        let gone = Arc::new(AtomicBool::new(false));
        let server = Mux::server(
            StallAfter {
                data: script,
                pos: 0,
                stalled: Arc::clone(&stalled),
                gone: Arc::clone(&gone),
            },
            tokio::io::sink(),
        );
        let accepted = if accept_open {
            Some(
                tokio::time::timeout(std::time::Duration::from_secs(2), server.accept())
                    .await
                    .expect("accept timed out")
                    .expect("accept"),
            )
        } else {
            None
        };
        let saw_stall = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !stalled.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            saw_stall.is_ok(),
            "reader never blocked inside the partial payload"
        );
        server.shutdown();
        drop(accepted);
        tokio::time::timeout(std::time::Duration::from_secs(2), server.closed())
            .await
            .expect("forced cancel left a task running");
        assert!(
            gone.load(Ordering::SeqCst),
            "reader still holds the transport"
        );
    }

    fn partial_frame(typ: u8, id: u32, payload: &[u8], with_open: bool) -> Vec<u8> {
        let mut script = Vec::new();
        if with_open {
            script.extend(frame_bytes(TYPE_OPEN, id, &[]));
        }
        let frame = frame_bytes(typ, id, payload);
        let keep = 4 + 5 + 1;
        script.extend_from_slice(&frame[..keep.min(frame.len())]);
        script
    }

    fn frame_bytes(typ: u8, id: u32, payload: &[u8]) -> Vec<u8> {
        let len = (1 + 4 + payload.len()) as u32;
        let mut buf = Vec::with_capacity(4 + len as usize);
        buf.extend_from_slice(&len.to_be_bytes());
        buf.push(typ);
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(payload);
        buf
    }

    #[tokio::test]
    async fn forced_cancel_with_saturated_output_wakes_waiters_without_draining() {
        let gate = Arc::new(AtomicBool::new(false));
        let waker_slot = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (local, remote) = duplex(64 * 1024);
        let (lr, lw) = tokio::io::split(local);
        let (mut rr, _rw) = tokio::io::split(remote);
        let client = Mux::client(
            lr,
            GateWriter {
                inner: lw,
                gate: Arc::clone(&gate),
                waker_slot: Arc::clone(&waker_slot),
                log: Arc::clone(&log),
                buf: Vec::new(),
            },
        );
        let mut stream = client.open().await.expect("open");
        stream
            .write_all(b"queued-not-drained")
            .await
            .expect("queue");
        let shutdown = tokio::spawn(async move { stream.shutdown().await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "writer emitted frames while the gate was closed"
        );
        client.shutdown();
        let err = tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown waiter hung after forced cancel")
            .expect("shutdown task")
            .expect_err("forced cancel reported a successful shutdown");
        assert!(
            err.to_string().contains("mux stopped") || err.to_string().contains("mux writer"),
            "waiter error was {err}"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), client.closed())
            .await
            .expect("tasks did not finish");
        gate.store(true, Ordering::SeqCst);
        if let Some(waker) = waker_slot.lock().unwrap().take() {
            waker.wake();
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "forced cancel drained after the gate opened: {:?}",
            log.lock().unwrap()
        );
        let mut tail = [0u8; 1];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), rr.read(&mut tail))
            .await
            .expect("eof timed out")
            .expect("read");
        assert_eq!(n, 0, "peer did not observe EOF");
    }

    fn window_credit(log: &Mutex<Vec<FrameLog>>) -> u32 {
        log.lock()
            .unwrap()
            .iter()
            .filter(|(typ, _, payload)| *typ == TYPE_WINDOW && payload.len() == 4)
            .map(|(_, _, payload)| u32::from_be_bytes(payload.as_slice().try_into().unwrap()))
            .fold(0u32, u32::saturating_add)
    }

    /// Yields a scripted prefix, then stays pending until the mux drops it.
    struct StallAfter {
        data: Vec<u8>,
        pos: usize,
        stalled: Arc<AtomicBool>,
        gone: Arc<AtomicBool>,
    }

    impl Drop for StallAfter {
        fn drop(&mut self) {
            self.gone.store(true, Ordering::SeqCst);
        }
    }

    impl AsyncRead for StallAfter {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.pos >= self.data.len() {
                self.stalled.store(true, Ordering::SeqCst);
                return Poll::Pending;
            }
            let n = buf.remaining().min(self.data.len() - self.pos);
            buf.put_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    struct IdleReader<R> {
        inner: R,
        started: Arc<AtomicBool>,
        gone: Arc<AtomicBool>,
    }

    impl<R> Drop for IdleReader<R> {
        fn drop(&mut self) {
            self.gone.store(true, Ordering::SeqCst);
        }
    }

    impl<R: AsyncRead + Unpin> AsyncRead for IdleReader<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.started.store(true, Ordering::SeqCst);
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    struct IdleWriter<W> {
        inner: W,
        gone: Arc<AtomicBool>,
    }

    impl<W> Drop for IdleWriter<W> {
        fn drop(&mut self) {
            self.gone.store(true, Ordering::SeqCst);
        }
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for IdleWriter<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// Writer that fails every write with a stable error string.
    struct BoomWriter;

    impl AsyncWrite for BoomWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::other("injected write failure")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Writer that records mux frames and can refuse them until a gate opens.
    struct GateWriter<W> {
        inner: W,
        gate: Arc<AtomicBool>,
        waker_slot: Arc<Mutex<Option<Waker>>>,
        log: Arc<Mutex<Vec<FrameLog>>>,
        buf: Vec<u8>,
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for GateWriter<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if !self.gate.load(Ordering::SeqCst) {
                *self.waker_slot.lock().unwrap() = Some(cx.waker().clone());
                if !self.gate.load(Ordering::SeqCst) {
                    return Poll::Pending;
                }
            }
            let n = {
                let inner = Pin::new(&mut self.inner);
                match inner.poll_write(cx, buf) {
                    Poll::Ready(Ok(n)) => n,
                    other => return other,
                }
            };
            self.buf.extend_from_slice(&buf[..n]);
            loop {
                if self.buf.len() < 4 {
                    break;
                }
                let len = u32::from_be_bytes(self.buf[..4].try_into().unwrap()) as usize;
                if self.buf.len() < 4 + len {
                    break;
                }
                let typ = self.buf[4];
                let id = u32::from_be_bytes(self.buf[5..9].try_into().unwrap());
                let payload = self.buf[9..4 + len].to_vec();
                self.log.lock().unwrap().push((typ, id, payload));
                self.buf.drain(..4 + len);
            }
            Poll::Ready(Ok(n))
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if !self.gate.load(Ordering::SeqCst) {
                *self.waker_slot.lock().unwrap() = Some(cx.waker().clone());
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
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
