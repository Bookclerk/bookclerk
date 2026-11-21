//! The host side of `bookclerk-media-worker pcm`: decoded samples for a
//! player, from a confined worker.
//!
//! A player needs PCM, and decoding is untrusted work, so it never happens
//! in this process. [`crate::MediaPool::open_pcm`] starts one worker per
//! file and start position, jailed to no path and no network, and feeds it
//! the byte ranges its demuxer asks for from a [`RangeSource`] (a local
//! file, a storage plugin's ranged read, or the playback route). The
//! samples come back as a [`PcmStream`].
//!
//! The worker is treated as compromised: every frame is checked by
//! [`ReplyReader`] and [`ReplyOrder`], every `Need` must lie inside the file,
//! the bytes fetched for one stream are capped, and the worker gets a
//! deadline for its format and for each round trip. A seek is a new stream:
//! dropping a [`PcmStream`] kills its worker.
//!
//! With isolation off, playback is refused rather than decoded here: the
//! player has no reason to put a codec next to the master key.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::cover_worker::pathless_spec;
use crate::error::{MediaError, Result};
use crate::pcm_wire::{
    encode_request, PcmHeader, PcmReply, PcmRequest, ReplyOrder, ReplyReader, PCM_WORKER_ARG,
};
use crate::pool::{forward_worker_stderr, worker_env_allowed, Confinement, WorkerLaunch};
use crate::{ByteSpan, RangeSource, WORKER_ENFORCEMENT_ENV};

/// Label of PCM work in errors and logs.
const LABEL: &str = "pcm";

/// Longest the worker may take to state its format, and the host's source
/// to answer one `Need`.
const ROUND_TIMEOUT: Duration = Duration::from_secs(30);

/// Decoded chunks buffered between the worker and the player. The worker
/// waits on its pipe once these are full, so a paused player holds at most
/// this much PCM.
const CHUNKS_AHEAD: usize = 16;

/// Bytes one stream may fetch beyond the file's own size: a demuxer reads
/// some regions twice (an index, then the samples), never the whole file
/// many times over.
const EXTRA_FETCH_BYTES: u64 = 64 * 1024 * 1024;

/// Decoded samples of one file from one start position.
///
/// Dropping it stops the worker.
#[derive(Debug)]
pub struct PcmStream {
    /// The layout and start the worker stated.
    header: PcmHeader,
    /// Chunks of interleaved samples, then the end or an error.
    chunks: mpsc::Receiver<Result<Vec<i16>>>,
    /// The task that drives the worker; aborted (killing it) on drop.
    driver: JoinHandle<()>,
}

impl Drop for PcmStream {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl PcmStream {
    /// The sample rate, channels, actual start and stated duration.
    #[must_use]
    pub fn header(&self) -> PcmHeader {
        self.header
    }

    /// The next chunk of interleaved samples: a whole number of frames in
    /// [`Self::header`]'s layout.
    ///
    /// # Returns
    ///
    /// `None` once the file decoded to its end.
    ///
    /// # Errors
    ///
    /// [`MediaError::Native`] when the worker could not decode the rest,
    /// [`MediaError::Io`] when the source failed, and
    /// [`MediaError::Worker`] when the worker died, stalled or broke the
    /// protocol. The stream ends after an error.
    pub async fn next_chunk(&mut self) -> Option<Result<Vec<i16>>> {
        self.chunks.recv().await
    }
}

/// Where PCM work runs.
#[derive(Debug, Clone)]
pub(crate) enum PcmRunner {
    /// A confined worker per stream.
    Worker {
        /// Worker binary, and the jail on spawn-confined platforms.
        launch: WorkerLaunch,
        /// Required or best-effort.
        confinement: Confinement,
    },
    /// Refuse, with the reason.
    Refuse(String),
}

impl PcmRunner {
    /// Opens a stream of `size` bytes from `source`, starting at `start_ms`.
    pub(crate) async fn open<S: RangeSource + 'static>(
        &self,
        size: u64,
        start_ms: u64,
        source: S,
    ) -> Result<PcmStream> {
        match self {
            Self::Refuse(detail) => Err(MediaError::NotIsolated {
                job: LABEL,
                detail: detail.clone(),
            }),
            Self::Worker {
                launch,
                confinement,
            } => {
                let mut child = spawn(launch, *confinement)?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| worker_error("pcm worker stdin unavailable".into()))?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| worker_error("pcm worker stdout unavailable".into()))?;
                drive(stdin, stdout, Some(child), size, start_ms, source).await
            }
        }
    }
}

/// Starts one worker in PCM mode.
fn spawn(launch: &WorkerLaunch, confinement: Confinement) -> Result<tokio::process::Child> {
    let mut command = match &launch.jail {
        Some(jail) => {
            let mut command = tokio::process::Command::new(jail);
            command.arg("--").arg(&launch.bin);
            command
        }
        None => tokio::process::Command::new(&launch.bin),
    };
    command
        .arg(PCM_WORKER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in std::env::vars() {
        if worker_env_allowed(&key) {
            command.env(key, value);
        }
    }
    command.env(WORKER_ENFORCEMENT_ENV, confinement.as_env_value());
    if launch.jail.is_some() {
        let spec = pathless_spec("media-worker:pcm", confinement);
        command.env(
            bookclerk_sandbox::SPEC_ENV,
            serde_json::to_string(&spec)
                .map_err(|err| worker_error(format!("could not encode jail spec: {err}")))?,
        );
    }
    let mut child = bookclerk_sandbox::with_fd_spawn_lock(|| command.spawn()).map_err(|err| {
        worker_error(format!(
            "could not spawn {} {PCM_WORKER_ARG}: {err}",
            launch.bin.display()
        ))
    })?;
    if let Some(stderr) = child.stderr.take() {
        forward_worker_stderr(LABEL, stderr);
    }
    Ok(child)
}

/// Sends `Open`, then drives the worker on a task until its format
/// arrives, and returns the stream.
///
/// # Arguments
///
/// * `input` - The worker's stdin.
/// * `output` - The worker's stdout.
/// * `child` - The process, killed when the driver ends; `None` in tests
///   that run the worker on a thread.
/// * `size` - The file's size in bytes.
/// * `start_ms` - Where to start.
/// * `source` - Where the file's bytes come from.
///
/// # Errors
///
/// What the worker reports before its format, or a [`MediaError::Worker`]
/// when it breaks the protocol or misses the deadline.
pub(crate) async fn drive<I, O, S>(
    mut input: I,
    output: O,
    child: Option<tokio::process::Child>,
    size: u64,
    start_ms: u64,
    source: S,
) -> Result<PcmStream>
where
    I: AsyncWrite + Unpin + Send + 'static,
    O: AsyncRead + Unpin + Send + 'static,
    S: RangeSource + 'static,
{
    let open = encode_request(&PcmRequest::Open { size, start_ms })
        .map_err(|err| worker_error(format!("could not encode request: {err}")))?;
    write_frame(&mut input, &open).await?;
    let (header_tx, header_rx) = oneshot::channel();
    let (chunk_tx, chunks) = mpsc::channel(CHUNKS_AHEAD);
    let driver = tokio::spawn(async move {
        // Held so the worker dies with the driver (kill_on_drop).
        let _child = child;
        let mut session = Session {
            input,
            output,
            source: Arc::new(source),
            size,
            fetched: 0,
            reader: ReplyReader::default(),
            order: ReplyOrder::default(),
        };
        session.run(header_tx, chunk_tx).await;
    });
    match tokio::time::timeout(ROUND_TIMEOUT, header_rx).await {
        Ok(Ok(Ok(header))) => Ok(PcmStream {
            header,
            chunks,
            driver,
        }),
        Ok(Ok(Err(err))) => {
            driver.abort();
            Err(err)
        }
        Ok(Err(_)) => {
            driver.abort();
            Err(worker_error("pcm worker ended before its format".into()))
        }
        Err(_) => {
            driver.abort();
            Err(worker_error(format!(
                "pcm worker stated no format within {}s",
                ROUND_TIMEOUT.as_secs()
            )))
        }
    }
}

/// One worker's conversation.
struct Session<I, O, S> {
    /// The worker's stdin.
    input: I,
    /// The worker's stdout.
    output: O,
    /// Where the file's bytes come from.
    source: Arc<S>,
    /// The file's size in bytes.
    size: u64,
    /// Bytes fetched so far for this stream.
    fetched: u64,
    /// Splits stdout into checked frames.
    reader: ReplyReader,
    /// Checks the order of frames.
    order: ReplyOrder,
}

/// Where the format goes once the worker states it, or the error before it.
type HeaderSlot = Option<oneshot::Sender<Result<PcmHeader>>>;

impl<I, O, S> Session<I, O, S>
where
    I: AsyncWrite + Unpin,
    O: AsyncRead + Unpin,
    S: RangeSource,
{
    /// Runs until the worker ends, fails, breaks the protocol, or the
    /// player drops the stream.
    async fn run(
        &mut self,
        header: oneshot::Sender<Result<PcmHeader>>,
        chunks: mpsc::Sender<Result<Vec<i16>>>,
    ) {
        let mut header: HeaderSlot = Some(header);
        let outcome = self.pump(&mut header, &chunks).await;
        if let Err(err) = outcome {
            match header.take() {
                Some(slot) => {
                    let _ = slot.send(Err(err));
                }
                None => {
                    let _ = chunks.send(Err(err)).await;
                }
            }
        }
    }

    /// Reads frames and answers them.
    ///
    /// # Returns
    ///
    /// `Ok` at the end of the file or when the player went away.
    async fn pump(
        &mut self,
        header: &mut HeaderSlot,
        chunks: &mpsc::Sender<Result<Vec<i16>>>,
    ) -> Result<()> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            while let Some(reply) = self
                .reader
                .next_reply()
                .map_err(|err| worker_error(format!("pcm worker sent a bad frame: {err}")))?
            {
                self.order
                    .check(&reply)
                    .map_err(|err| worker_error(format!("pcm worker broke the protocol: {err}")))?;
                match reply {
                    PcmReply::Need { offset, len } => self.answer(offset, len).await?,
                    PcmReply::Format(stated) => {
                        if let Some(slot) = header.take() {
                            if slot.send(Ok(stated)).is_err() {
                                return Ok(());
                            }
                        }
                    }
                    PcmReply::Pcm(samples) => {
                        if chunks.send(Ok(samples)).await.is_err() {
                            return Ok(());
                        }
                    }
                    PcmReply::End => return Ok(()),
                    PcmReply::Failed { message } => return Err(MediaError::Native(message)),
                }
            }
            let read = if header.is_some() {
                tokio::time::timeout(ROUND_TIMEOUT, self.output.read(&mut buf))
                    .await
                    .map_err(|_| {
                        worker_error(format!(
                            "pcm worker did not answer within {}s",
                            ROUND_TIMEOUT.as_secs()
                        ))
                    })?
            } else {
                // Streaming: the worker waits on the pipe while the player
                // is paused, so silence here is not a stall.
                self.output.read(&mut buf).await
            };
            let n = read.map_err(|err| worker_error(format!("pcm worker output failed: {err}")))?;
            if n == 0 {
                return Err(worker_error("pcm worker ended before the end".into()));
            }
            self.reader.push(&buf[..n]);
        }
    }

    /// Fetches the span a `Need` names and writes it to the worker.
    ///
    /// # Errors
    ///
    /// [`MediaError::Worker`] for a span outside the file or over the fetch
    /// cap, [`MediaError::Io`] when the source fails or answers short.
    async fn answer(&mut self, offset: u64, len: u32) -> Result<()> {
        let span = ByteSpan {
            offset,
            len: u64::from(len),
        };
        if span.end().is_none_or(|end| end > self.size) {
            return Err(worker_error(
                "pcm worker asked for bytes outside the file".into(),
            ));
        }
        self.fetched = self.fetched.saturating_add(span.len);
        if self.fetched > self.size.saturating_add(EXTRA_FETCH_BYTES) {
            return Err(worker_error(
                "pcm worker asked for far more than the file".into(),
            ));
        }
        let bytes = tokio::time::timeout(ROUND_TIMEOUT, self.source.read_range(span))
            .await
            .map_err(|_| {
                MediaError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the file's storage did not answer in time",
                ))
            })??;
        if u64::try_from(bytes.len()).ok() != Some(span.len) {
            return Err(MediaError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "storage returned {} bytes for a {}-byte range",
                    bytes.len(),
                    span.len
                ),
            )));
        }
        let frame = encode_request(&PcmRequest::Data { offset, bytes })
            .map_err(|err| worker_error(format!("could not encode request: {err}")))?;
        tokio::time::timeout(ROUND_TIMEOUT, write_frame(&mut self.input, &frame))
            .await
            .map_err(|_| worker_error("pcm worker did not take a frame in time".into()))?
    }
}

/// Writes one whole frame.
async fn write_frame(input: &mut (impl AsyncWrite + Unpin), frame: &[u8]) -> Result<()> {
    input
        .write_all(frame)
        .await
        .and(input.flush().await)
        .map_err(|err| worker_error(format!("could not write to pcm worker: {err}")))
}

/// A [`MediaError::Worker`] for PCM work.
fn worker_error(detail: String) -> MediaError {
    MediaError::Worker { job: LABEL, detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pcm_wire::{encode_reply, PcmHeader};

    /// A file held in memory.
    struct Buffer(Vec<u8>);

    impl RangeSource for Buffer {
        async fn read_range(&self, span: ByteSpan) -> std::io::Result<Vec<u8>> {
            let from = usize::try_from(span.offset).map_err(std::io::Error::other)?;
            let to = usize::try_from(span.offset + span.len).map_err(std::io::Error::other)?;
            self.0
                .get(from..to)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| std::io::Error::other("out of range"))
        }
    }

    /// Runs the real worker loop on a thread behind in-memory pipes.
    async fn open_on_thread(file: Vec<u8>, start_ms: u64) -> Result<PcmStream> {
        let (host_in, worker_in) = tokio::io::duplex(1 << 20);
        let (worker_out, host_out) = tokio::io::duplex(1 << 20);
        std::thread::spawn(move || {
            let input = tokio_util_bridge::Reader(worker_in);
            let output = tokio_util_bridge::Writer(worker_out);
            let _ = crate::pcm_worker::serve_pcm(input, output);
        });
        let size = u64::try_from(file.len()).unwrap();
        drive(host_in, host_out, None, size, start_ms, Buffer(file)).await
    }

    /// Blocking adapters over Tokio's in-memory pipes, for the worker's
    /// synchronous loop.
    mod tokio_util_bridge {
        use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

        /// The read half, read synchronously.
        pub struct Reader(pub DuplexStream);
        /// The write half, written synchronously.
        pub struct Writer(pub DuplexStream);

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
        }

        impl std::io::Read for Reader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                runtime().block_on(self.0.read(buf))
            }
        }

        impl std::io::Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                runtime().block_on(self.0.write(buf))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                runtime().block_on(self.0.flush())
            }
        }
    }

    fn wav(rate: u32, seconds: u32) -> Vec<u8> {
        let frames = rate * seconds;
        let data_len = frames * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let sample = i16::try_from(frame % 1000).unwrap();
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_streams_through_a_worker() {
        let mut stream = open_on_thread(wav(8_000, 2), 0).await.unwrap();
        assert_eq!(stream.header().sample_rate, 8_000);
        assert_eq!(stream.header().channels, 1);
        assert_eq!(stream.header().duration_ms, Some(2_000));
        let mut samples = Vec::new();
        while let Some(chunk) = stream.next_chunk().await {
            samples.extend(chunk.unwrap());
        }
        assert_eq!(samples.len(), 16_000);
        assert_eq!(samples[999], 999);
        assert_eq!(samples[1000], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_file_fails_before_the_format() {
        let err = open_on_thread(vec![3; 4096], 0).await.unwrap_err();
        assert!(matches!(err, MediaError::Native(_)), "{err:?}");
    }

    /// Opens a stream against a scripted worker that sends `frames`.
    async fn scripted(frames: Vec<PcmReply>, size: u64) -> Result<Vec<Result<Vec<i16>>>> {
        let (host_in, _worker_in) = tokio::io::duplex(1 << 16);
        let (mut worker_out, host_out) = tokio::io::duplex(1 << 16);
        let mut bytes = Vec::new();
        for frame in &frames {
            bytes.extend(encode_reply(frame).unwrap());
        }
        tokio::spawn(async move {
            let _ = worker_out.write_all(&bytes).await;
            // Keep the pipe open so the end is the frames', not EOF's.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let mut stream = drive(host_in, host_out, None, size, 0, Buffer(vec![0; 64])).await?;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next_chunk().await {
            let failed = chunk.is_err();
            out.push(chunk);
            if failed {
                break;
            }
        }
        Ok(out)
    }

    fn header(channels: u8) -> PcmHeader {
        PcmHeader {
            sample_rate: 8_000,
            channels,
            start_ms: 0,
            duration_ms: None,
        }
    }

    #[tokio::test]
    async fn a_worker_that_breaks_the_protocol_is_stopped() {
        // Samples before the format.
        let err = scripted(vec![PcmReply::Pcm(vec![1])], 64)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::Worker { .. }), "{err:?}");
        // Half a stereo frame.
        let chunks = scripted(
            vec![PcmReply::Format(header(2)), PcmReply::Pcm(vec![1, 2, 3])],
            64,
        )
        .await
        .unwrap();
        assert!(matches!(
            chunks.last(),
            Some(Err(MediaError::Worker { .. }))
        ));
        // A span past the end of the file.
        let err = scripted(vec![PcmReply::Need { offset: 60, len: 8 }], 64)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::Worker { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_failure_after_the_format_ends_the_stream_with_its_message() {
        let chunks = scripted(
            vec![
                PcmReply::Format(header(1)),
                PcmReply::Pcm(vec![5, 6]),
                PcmReply::Failed {
                    message: "broken packet".into(),
                },
            ],
            64,
        )
        .await
        .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].as_ref().unwrap(), &vec![5, 6]);
        assert!(matches!(&chunks[1], Err(MediaError::Native(m)) if m == "broken packet"));
    }

    #[tokio::test]
    async fn a_runner_without_a_worker_refuses() {
        let runner = PcmRunner::Refuse("isolation is off".into());
        let err = runner.open(1, 0, Buffer(vec![0])).await.unwrap_err();
        assert!(matches!(err, MediaError::NotIsolated { .. }));
    }
}
