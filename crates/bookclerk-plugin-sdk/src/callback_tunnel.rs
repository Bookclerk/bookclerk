//! Multiplexed byte tunnel for host-owned OAuth callback listeners.
//!
//! Audience: source plugin authors that need a browser redirect target during
//! interactive login (e.g. Audible OAuth) while the guest is jailed and cannot
//! bind loopback itself. The host binds the browser-facing TCP socket (works
//! under AppContainer loopback isolation) and forwards each accepted connection
//! over a duplex IPC stream. The guest runs its HTTP stack on
//! [`TunnelStream`] values from [`TunnelGuest::accept`] — no guest `listen`
//! required.
//!
//! Frame layout (big-endian):
//! ```text
//! u32 length_of_rest | u8 type | u32 conn_id | payload
//! ```
//! Types: `Open=1`, `Data=2`, `Close=3`. Payload per Data frame is capped at 1 MiB.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, Mutex};

use crate::{Result, SdkError};

const TYPE_OPEN: u8 = 1;
const TYPE_DATA: u8 = 2;
const TYPE_CLOSE: u8 = 3;
const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;

enum OutFrame {
    Open(u32),
    Data(u32, Vec<u8>),
    Close(u32),
}

/// Host end of the callback tunnel: opens logical connections toward the guest.
///
/// Construct with [`TunnelHost::new`] over the duplex IPC halves shared with
/// [`TunnelGuest`]. Call [`TunnelHost::open`] once per accepted browser TCP
/// connection, then `tokio::io::copy_bidirectional` between the TCP socket and
/// the returned [`TunnelStream`].
pub struct TunnelHost {
    out_tx: mpsc::UnboundedSender<OutFrame>,
    next_id: AtomicU32,
    inbound: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Guest end of the callback tunnel: accepts logical connections from the host.
///
/// Construct with [`TunnelGuest::new`] on the peer IPC halves. Loop on
/// [`TunnelGuest::accept`] and feed each [`TunnelStream`] into the guest HTTP
/// server (same `AsyncRead` + `AsyncWrite` surface as a TCP stream).
pub struct TunnelGuest {
    accept_rx: mpsc::UnboundedReceiver<TunnelStream>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// One multiplexed logical connection implementing `AsyncRead` + `AsyncWrite`.
///
/// Connection ids are assigned by the host ([`TunnelHost::open`]) and echoed in
/// Open/Data/Close frames. Writes larger than the 1 MiB frame payload cap are
/// split into multiple Data frames (partial write semantics).
pub struct TunnelStream {
    id: u32,
    out_tx: mpsc::UnboundedSender<OutFrame>,
    data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    read_buf: Vec<u8>,
    closed: bool,
}

impl TunnelStream {
    /// Stable connection id shared with the peer for this logical stream.
    ///
    /// Assigned by [`TunnelHost::open`] starting at `1` and included in every
    /// frame for this stream. Useful for logging correlating browser sockets.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }
}

impl TunnelHost {
    /// Spawns reader/writer tasks on the host half of a duplex IPC link.
    ///
    /// Keeps background tasks alive for the lifetime of `self`. Dropping the
    /// host stops accepting new opens; in-flight streams observe EOF when the
    /// peer closes.
    ///
    /// # Arguments
    ///
    /// * `reader` - Async half that receives guest→host frames.
    /// * `writer` - Async half that sends host→guest frames.
    ///
    /// # Returns
    ///
    /// Ready [`TunnelHost`] that can [`Self::open`] logical connections.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<OutFrame>();
        let inbound: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(writer_task(writer, out_rx)));
        let inbound_task = Arc::clone(&inbound);
        tasks.push(tokio::spawn(async move {
            let mut reader = reader;
            while let Ok(frame) = read_frame(&mut reader).await {
                match frame {
                    Frame::Data { conn_id, payload } => {
                        let map = inbound_task.lock().await;
                        if let Some(tx) = map.get(&conn_id) {
                            let _ = tx.send(payload);
                        }
                    }
                    Frame::Close { conn_id } => {
                        inbound_task.lock().await.remove(&conn_id);
                    }
                    Frame::Open { .. } => {}
                }
            }
            // Drop senders so TunnelStream readers observe EOF instead of hanging.
            inbound_task.lock().await.clear();
        }));
        Self {
            out_tx,
            next_id: AtomicU32::new(1),
            inbound,
            _tasks: tasks,
        }
    }

    /// Opens one logical connection toward the guest and returns its stream.
    ///
    /// Allocates a new connection id, registers an inbound data channel, and
    /// sends an Open frame so [`TunnelGuest::accept`] can yield the peer stream.
    ///
    /// # Returns
    ///
    /// Host-side [`TunnelStream`] ready for bidirectional I/O with the guest.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the background writer task has exited (IPC
    /// closed) so the Open frame cannot be queued.
    pub async fn open(&self) -> Result<TunnelStream> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.inbound.lock().await.insert(id, tx);
        self.out_tx
            .send(OutFrame::Open(id))
            .map_err(|_| SdkError::message("callback tunnel writer gone"))?;
        Ok(TunnelStream {
            id,
            out_tx: self.out_tx.clone(),
            data_rx: rx,
            read_buf: Vec::new(),
            closed: false,
        })
    }
}

impl TunnelGuest {
    /// Spawns reader/writer tasks on the guest half of a duplex IPC link.
    ///
    /// # Arguments
    ///
    /// * `reader` - Async half that receives host→guest frames.
    /// * `writer` - Async half that sends guest→host frames.
    ///
    /// # Returns
    ///
    /// Ready [`TunnelGuest`] whose [`Self::accept`] yields streams opened by
    /// the host.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<OutFrame>();
        let (accept_tx, accept_rx) = mpsc::unbounded_channel();
        let streams: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(writer_task(writer, out_rx)));
        let streams_task = Arc::clone(&streams);
        tasks.push(tokio::spawn(async move {
            let mut reader = reader;
            while let Ok(frame) = read_frame(&mut reader).await {
                match frame {
                    Frame::Open { conn_id } => {
                        let (tx, rx) = mpsc::unbounded_channel();
                        streams_task.lock().await.insert(conn_id, tx);
                        let stream = TunnelStream {
                            id: conn_id,
                            out_tx: out_tx.clone(),
                            data_rx: rx,
                            read_buf: Vec::new(),
                            closed: false,
                        };
                        if accept_tx.send(stream).is_err() {
                            break;
                        }
                    }
                    Frame::Data { conn_id, payload } => {
                        let map = streams_task.lock().await;
                        if let Some(tx) = map.get(&conn_id) {
                            let _ = tx.send(payload);
                        }
                    }
                    Frame::Close { conn_id } => {
                        streams_task.lock().await.remove(&conn_id);
                    }
                }
            }
            // Drop senders so TunnelStream readers observe EOF instead of hanging.
            streams_task.lock().await.clear();
        }));
        Self {
            accept_rx,
            _tasks: tasks,
        }
    }

    /// Waits for the next logical connection opened by [`TunnelHost::open`].
    ///
    /// # Returns
    ///
    /// Guest-side [`TunnelStream`] for one browser callback connection.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the accept channel closes because the IPC
    /// reader task exited (peer hangup or frame error).
    pub async fn accept(&mut self) -> Result<TunnelStream> {
        self.accept_rx
            .recv()
            .await
            .ok_or_else(|| SdkError::message("callback tunnel closed"))
    }
}

async fn writer_task<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut out_rx: mpsc::UnboundedReceiver<OutFrame>,
) {
    while let Some(frame) = out_rx.recv().await {
        let (typ, conn_id, payload) = match frame {
            OutFrame::Open(id) => (TYPE_OPEN, id, Vec::new()),
            OutFrame::Data(id, p) => (TYPE_DATA, id, p),
            OutFrame::Close(id) => (TYPE_CLOSE, id, Vec::new()),
        };
        if write_raw(&mut writer, typ, conn_id, &payload)
            .await
            .is_err()
        {
            break;
        }
    }
}

enum Frame {
    Open { conn_id: u32 },
    Data { conn_id: u32, payload: Vec<u8> },
    Close { conn_id: u32 },
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame> {
    let len = reader.read_u32().await.map_err(io_err)?;
    if len < 5 || (len as usize) > MAX_FRAME_PAYLOAD + 5 {
        return Err(SdkError::message(format!(
            "callback tunnel bad frame length {len}"
        )));
    }
    let mut rest = vec![0u8; len as usize];
    reader.read_exact(&mut rest).await.map_err(io_err)?;
    let typ = rest[0];
    let conn_id = u32::from_be_bytes(rest[1..5].try_into().unwrap());
    let payload = rest[5..].to_vec();
    match typ {
        TYPE_OPEN => Ok(Frame::Open { conn_id }),
        TYPE_DATA => Ok(Frame::Data { conn_id, payload }),
        TYPE_CLOSE => Ok(Frame::Close { conn_id }),
        other => Err(SdkError::message(format!(
            "callback tunnel unknown frame type {other}"
        ))),
    }
}

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

impl AsyncRead for TunnelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.read_buf.is_empty() {
            let n = buf.remaining().min(self.read_buf.len());
            buf.put_slice(&self.read_buf[..n]);
            self.read_buf.drain(..n);
            return Poll::Ready(Ok(()));
        }
        if self.closed {
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut self.data_rx).poll_recv(cx) {
            Poll::Ready(Some(chunk)) => {
                let n = buf.remaining().min(chunk.len());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.read_buf.extend_from_slice(&chunk[n..]);
                }
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

impl AsyncWrite for TunnelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.closed {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "tunnel closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Cap each Data frame so read_frame's MAX_FRAME_PAYLOAD check cannot
        // reject a large copy_bidirectional chunk; report a partial write.
        let n = buf.len().min(MAX_FRAME_PAYLOAD);
        match self.out_tx.send(OutFrame::Data(self.id, buf[..n].to_vec())) {
            Ok(()) => Poll::Ready(Ok(n)),
            Err(_) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "tunnel writer gone",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if !self.closed {
            let _ = self.out_tx.send(OutFrame::Close(self.id));
            self.closed = true;
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn host_open_guest_accept_echo() {
        let (a, b) = duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let host = TunnelHost::new(ar, aw);
        let mut guest = TunnelGuest::new(br, bw);

        let mut host_stream = host.open().await.expect("open");
        let mut guest_stream = guest.accept().await.expect("accept");

        host_stream.write_all(b"hello").await.expect("write");
        let mut buf = [0u8; 5];
        guest_stream.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"hello");

        guest_stream.write_all(b"world").await.expect("write");
        let mut buf2 = [0u8; 5];
        host_stream.read_exact(&mut buf2).await.expect("read");
        assert_eq!(&buf2, b"world");
    }
}
