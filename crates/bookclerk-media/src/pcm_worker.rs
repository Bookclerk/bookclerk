//! The worker side of `bookclerk-media-worker pcm`: decodes one audio file
//! to PCM for a player, reading its bytes from the host.
//!
//! The file may live on any destination, so the worker opens nothing. Its
//! demuxer reads through [`HostRanges`], which asks the host for each block
//! it needs with a `Need` frame and waits for the `Data` answer (see
//! [`crate::pcm_wire`]). Decoding goes through the decode front
//! ([`crate::decode`]), so the player reads every format the media worker
//! reads, folded to at most stereo. Samples go out in `Pcm` frames as they
//! are decoded; the pipe's own backpressure paces the worker to the
//! player, which reads only as fast as it plays.
//!
//! Asked for a `Sample` instead, the worker decodes from the start position
//! for the sample's length only, folds it to mono and encodes it as MP3
//! ([`crate::mp3::Mp3Stream`]), sending `Encoded` frames: the bytes a book's
//! details play are never the stored file's own bytes.

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, PoisonError};

use symphonia::core::formats::{SeekMode, SeekTo};
use symphonia::core::io::MediaSource;
use symphonia::core::units::Time;

use crate::decode::{open_audio, OutChannels, Primed};
use crate::error::{MediaError, Result};
use crate::mp3::Mp3Stream;
use crate::pcm_wire::{
    body_len, decode_request, encode_reply, PcmHeader, PcmReply, PcmRequest, MAX_ENCODED_BYTES,
    MAX_NEED_BYTES, MAX_PCM_SAMPLES, MAX_POSITION_MS, MAX_REQUEST_BODY, MAX_SAMPLE_MS,
    MAX_SAMPLE_OUTPUT_HZ,
};

/// Bytes asked for at a time: large enough that a sequential read costs
/// few round trips, small enough that a seek wastes little.
const BLOCK_BYTES: u32 = 256 * 1024;

const _: () = assert!(BLOCK_BYTES <= MAX_NEED_BYTES);

/// The worker's stdio, shared by the demuxer's reads and the sample writes.
struct Pipes<R, W> {
    /// Frames from the host.
    input: R,
    /// Frames to the host.
    output: W,
}

impl<R: Read, W: Write> Pipes<R, W> {
    /// Writes one reply frame and flushes it.
    fn send(&mut self, reply: &PcmReply) -> std::io::Result<()> {
        let frame = encode_reply(reply).map_err(std::io::Error::other)?;
        self.output.write_all(&frame)?;
        self.output.flush()
    }

    /// Reads one request frame.
    ///
    /// # Errors
    ///
    /// An I/O error when stdin ends, a frame is over [`MAX_REQUEST_BODY`],
    /// or it does not decode.
    fn receive(&mut self) -> std::io::Result<PcmRequest> {
        let mut prefix = [0u8; 4];
        self.input.read_exact(&mut prefix)?;
        let len = body_len(prefix);
        if len > MAX_REQUEST_BODY {
            return Err(std::io::Error::other(format!(
                "request of {len} bytes is too large"
            )));
        }
        let mut body = vec![0u8; len];
        self.input.read_exact(&mut body)?;
        decode_request(&body).map_err(std::io::Error::other)
    }
}

/// Stdio shared between the demuxer and the decode loop.
type Shared<R, W> = Arc<Mutex<Pipes<R, W>>>;

/// Locks the pipes, recovering them from a panicked holder (the decode
/// loop ends the stream on any error anyway).
fn lock<R, W>(pipes: &Shared<R, W>) -> std::sync::MutexGuard<'_, Pipes<R, W>> {
    pipes.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The file, read from the host one block at a time.
struct HostRanges<R, W> {
    /// The pipes to the host.
    pipes: Shared<R, W>,
    /// The file's size in bytes.
    size: u64,
    /// The read position.
    position: u64,
    /// The last block received: its offset and bytes.
    block: (u64, Vec<u8>),
}

impl<R: Read, W: Write> HostRanges<R, W> {
    /// Asks the host for the block at `offset` and keeps it.
    ///
    /// # Errors
    ///
    /// An I/O error when the pipes fail, or the host answers with another
    /// frame, another offset or another length.
    fn fetch(&mut self, offset: u64) -> std::io::Result<()> {
        let left = self.size.saturating_sub(offset);
        let len = u32::try_from(left.min(u64::from(BLOCK_BYTES))).unwrap_or(BLOCK_BYTES);
        let mut pipes = lock(&self.pipes);
        pipes.send(&PcmReply::Need { offset, len })?;
        match pipes.receive()? {
            PcmRequest::Data { offset: got, bytes }
                if got == offset && u64::try_from(bytes.len()).ok() == Some(u64::from(len)) =>
            {
                self.block = (offset, bytes);
                Ok(())
            }
            _ => Err(std::io::Error::other("host answered with another span")),
        }
    }
}

impl<R: Read, W: Write> Read for HostRanges<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.position >= self.size {
            return Ok(0);
        }
        let (start, bytes) = &self.block;
        let inside = self.position >= *start
            && self.position < start.saturating_add(u64::try_from(bytes.len()).unwrap_or(0));
        if !inside {
            self.fetch(self.position)?;
        }
        let (start, bytes) = &self.block;
        let from = usize::try_from(self.position - start).map_err(std::io::Error::other)?;
        let available = bytes.get(from..).unwrap_or_default();
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.position += u64::try_from(n).map_err(std::io::Error::other)?;
        Ok(n)
    }
}

impl<R, W> Seek for HostRanges<R, W> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.size.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        let target =
            target.ok_or_else(|| std::io::Error::other("seek before the start of the file"))?;
        self.position = target;
        Ok(target)
    }
}

impl<R: Read + Send + Sync, W: Write + Send + Sync> MediaSource for HostRanges<R, W> {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.size)
    }
}

/// Serves one PCM decode over `input` and `output` (the worker's stdin and
/// stdout): reads `Open`, then streams `Format`, `Pcm` frames and `End`, or
/// `Failed` when the file cannot be decoded. Opened with `Sample` instead,
/// streams `Format`, `Encoded` frames and `End`.
///
/// # Arguments
///
/// * `input` - Request frames from the host.
/// * `output` - Reply frames to the host.
///
/// # Errors
///
/// Why the stream broke: the host sent no `Open` or `Sample`, or a pipe
/// failed. A file
/// that cannot be decoded is not an error here: the host is told with
/// `Failed`.
pub fn serve_pcm<R, W>(input: R, output: W) -> std::result::Result<(), String>
where
    R: Read + Send + Sync + 'static,
    W: Write + Send + Sync + 'static,
{
    let pipes = Arc::new(Mutex::new(Pipes { input, output }));
    let opening = lock(&pipes).receive().map_err(|err| err.to_string())?;
    let outcome = match opening {
        PcmRequest::Open { size, start_ms } => decode(&pipes, size, start_ms.min(MAX_POSITION_MS)),
        PcmRequest::Sample {
            size,
            start_ms,
            length_ms,
        } => sample(
            &pipes,
            size,
            start_ms.min(MAX_POSITION_MS),
            length_ms.min(MAX_SAMPLE_MS),
        ),
        PcmRequest::Data { .. } => return Err("the first request was not Open or Sample".into()),
    };
    let last = match outcome {
        Ok(()) => PcmReply::End,
        Err(err) => PcmReply::Failed {
            message: err.to_string(),
        },
    };
    let sent = lock(&pipes).send(&last).map_err(|err| err.to_string());
    sent
}

/// A file opened through the host and positioned at the asked-for start.
struct Positioned {
    /// The decoder, at the start.
    opened: crate::decode::OpenedAudio,
    /// Where decoding starts, in milliseconds: the asked-for start or the
    /// packet boundary before it.
    actual_ms: u64,
    /// The file's length in milliseconds, when its container states one.
    duration_ms: Option<u64>,
}

/// Opens the file through the host and seeks to `start_ms`.
///
/// # Errors
///
/// What the decode front returns, or a failed seek.
fn open_at<R, W>(pipes: &Shared<R, W>, size: u64, start_ms: u64) -> Result<Positioned>
where
    R: Read + Send + Sync + 'static,
    W: Write + Send + Sync + 'static,
{
    let source = HostRanges {
        pipes: Arc::clone(pipes),
        size,
        position: 0,
        block: (0, Vec::new()),
    };
    let mut opened = open_audio(Box::new(source), start_ms > 0)?;
    let track = opened
        .format
        .tracks()
        .iter()
        .find(|track| track.id == opened.track_id)
        .cloned();
    let time_base = track.as_ref().and_then(|track| track.time_base);
    let duration_ms = track
        .as_ref()
        .and_then(|track| track.duration)
        .zip(time_base)
        .and_then(|(duration, base)| base.calc_duration(duration))
        .and_then(|time| u64::try_from(time.as_millis()).ok())
        .filter(|ms| *ms <= MAX_POSITION_MS);
    let mut actual_ms = 0;
    if start_ms > 0 {
        let seeked = opened
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from_millis_u64(start_ms),
                    track_id: Some(opened.track_id),
                },
            )
            .map_err(|err| MediaError::Native(format!("seek failed: {err}")))?;
        opened.decoder.reset();
        // The primed packet is from the start of the file, not the seek.
        opened.primed = Primed::default();
        actual_ms = time_base
            .and_then(|base| base.calc_time(seeked.actual_ts))
            .and_then(|time| u64::try_from(time.as_millis()).ok())
            .unwrap_or(start_ms)
            .min(MAX_POSITION_MS);
    }
    Ok(Positioned {
        opened,
        actual_ms,
        duration_ms,
    })
}

/// Opens the file through the host, seeks to `start_ms` and streams its
/// samples.
///
/// # Errors
///
/// What the decode front returns, a failed seek, or a pipe failure.
fn decode<R, W>(pipes: &Shared<R, W>, size: u64, start_ms: u64) -> Result<()>
where
    R: Read + Send + Sync + 'static,
    W: Write + Send + Sync + 'static,
{
    let Positioned {
        mut opened,
        actual_ms,
        duration_ms,
    } = open_at(pipes, size, start_ms)?;
    let format = opened.pcm_format(OutChannels::SourceUpToStereo);
    let channels = u8::try_from(format.channels).unwrap_or(2);
    send(
        pipes,
        &PcmReply::Format(PcmHeader {
            sample_rate: format.sample_rate,
            channels,
            start_ms: actual_ms,
            duration_ms,
        }),
    )?;
    // Whole frames per message, so the host never sees half of one.
    let per_frame = usize::from(channels).max(1);
    let chunk = MAX_PCM_SAMPLES - MAX_PCM_SAMPLES % per_frame;
    opened.decode_each(OutChannels::SourceUpToStereo, |pcm| {
        for part in pcm.chunks(chunk) {
            send(pipes, &PcmReply::Pcm(part.to_vec()))?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Opens the file through the host, seeks to `start_ms`, and streams
/// `length_ms` of it folded to mono and encoded as MP3.
///
/// # Errors
///
/// What the decode front or the encoder returns, a failed seek, or a pipe
/// failure.
fn sample<R, W>(pipes: &Shared<R, W>, size: u64, start_ms: u64, length_ms: u64) -> Result<()>
where
    R: Read + Send + Sync + 'static,
    W: Write + Send + Sync + 'static,
{
    let Positioned {
        mut opened,
        actual_ms,
        duration_ms,
    } = open_at(pipes, size, start_ms)?;
    let source_hz = opened.pcm_format(OutChannels::Mono).sample_rate;
    let (mut encoder, encoded_hz) = Mp3Stream::new(source_hz, MAX_SAMPLE_OUTPUT_HZ)?;
    send(
        pipes,
        &PcmReply::Format(PcmHeader {
            sample_rate: encoded_hz,
            channels: 1,
            start_ms: actual_ms,
            duration_ms,
        }),
    )?;
    // Frames still to encode, at the source rate.
    let mut left = usize::try_from(length_ms.saturating_mul(u64::from(source_hz)) / 1000)
        .unwrap_or(usize::MAX);
    opened.decode_while(OutChannels::Mono, |pcm| {
        let take = pcm.len().min(left);
        left -= take;
        send_encoded(pipes, &encoder.push(&pcm[..take])?)?;
        Ok(left > 0)
    })?;
    send_encoded(pipes, &encoder.finish()?)
}

/// Sends `bytes` as `Encoded` frames of at most [`MAX_ENCODED_BYTES`];
/// nothing when it is empty.
///
/// # Errors
///
/// [`MediaError::Io`] when the pipe to the host fails.
fn send_encoded<R: Read, W: Write>(pipes: &Shared<R, W>, bytes: &[u8]) -> Result<()> {
    for part in bytes.chunks(MAX_ENCODED_BYTES) {
        send(pipes, &PcmReply::Encoded(part.to_vec()))?;
    }
    Ok(())
}

/// Sends one reply.
///
/// # Errors
///
/// [`MediaError::Io`] when the pipe to the host fails.
fn send<R: Read, W: Write>(pipes: &Shared<R, W>, reply: &PcmReply) -> Result<()> {
    lock(pipes).send(reply).map_err(MediaError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pcm_wire;
    use std::io::Cursor;
    use std::sync::mpsc;

    /// A pipe end fed from a channel, so a test can play the host.
    struct ChannelReader {
        /// Bytes still to hand out.
        pending: Cursor<Vec<u8>>,
        /// Where more come from.
        rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    }

    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                let n = self.pending.read(buf)?;
                if n > 0 || buf.is_empty() {
                    return Ok(n);
                }
                match self.rx.lock().unwrap().recv() {
                    Ok(bytes) => self.pending = Cursor::new(bytes),
                    Err(_) => return Ok(0),
                }
            }
        }
    }

    /// A pipe end that forwards each write to a channel.
    struct ChannelWriter(Mutex<mpsc::Sender<Vec<u8>>>);

    impl Write for ChannelWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .send(buf.to_vec())
                .map_err(std::io::Error::other)?;
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A 16-bit PCM WAV of `seconds` of a 440 Hz tone.
    fn wav(rate: u32, channels: u16, seconds: u32) -> Vec<u8> {
        let frames = rate * seconds;
        let data_len = frames * u32::from(channels) * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let phase = f64::from(frame) * 440.0 * std::f64::consts::TAU / f64::from(rate);
            let sample = (phase.sin() * 8000.0) as i16;
            for _ in 0..channels {
                out.extend_from_slice(&sample.to_le_bytes());
            }
        }
        out
    }

    /// Plays the host for one decode of `file` from `start_ms`.
    fn run(file: Vec<u8>, start_ms: u64) -> Vec<PcmReply> {
        let (to_worker, worker_rx) = mpsc::channel();
        let (worker_tx, from_worker) = mpsc::channel();
        let input = ChannelReader {
            pending: Cursor::new(Vec::new()),
            rx: Mutex::new(worker_rx),
        };
        let output = ChannelWriter(Mutex::new(worker_tx));
        let worker = std::thread::spawn(move || serve_pcm(input, output));
        let size = u64::try_from(file.len()).unwrap();
        to_worker
            .send(encode_request(&PcmRequest::Open { size, start_ms }))
            .unwrap();
        let mut reader = pcm_wire::ReplyReader::default();
        let mut order = pcm_wire::ReplyOrder::default();
        let mut replies = Vec::new();
        'outer: while let Ok(bytes) = from_worker.recv() {
            reader.push(&bytes);
            while let Some(reply) = reader.next_reply().unwrap() {
                order.check(&reply).unwrap();
                if let PcmReply::Need { offset, len } = reply {
                    let from = usize::try_from(offset).unwrap();
                    let to = from + usize::try_from(len).unwrap();
                    assert!(to <= file.len(), "asked past the end");
                    to_worker
                        .send(encode_request(&PcmRequest::Data {
                            offset,
                            bytes: file[from..to].to_vec(),
                        }))
                        .unwrap();
                    continue;
                }
                let last = matches!(reply, PcmReply::End | PcmReply::Failed { .. });
                replies.push(reply);
                if last {
                    break 'outer;
                }
            }
        }
        worker.join().unwrap().unwrap();
        replies
    }

    /// `encode_request` that cannot fail in these tests.
    fn encode_request(request: &PcmRequest) -> Vec<u8> {
        pcm_wire::encode_request(request).unwrap()
    }

    fn samples(replies: &[PcmReply]) -> usize {
        replies
            .iter()
            .map(|reply| match reply {
                PcmReply::Pcm(samples) => samples.len(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn a_whole_file_decodes_through_host_ranges() {
        let replies = run(wav(22_050, 2, 3), 0);
        let PcmReply::Format(header) = replies[0] else {
            panic!("no format first: {replies:?}");
        };
        assert_eq!(header.sample_rate, 22_050);
        assert_eq!(header.channels, 2);
        assert_eq!(header.start_ms, 0);
        assert_eq!(header.duration_ms, Some(3_000));
        assert_eq!(samples(&replies), 22_050 * 3 * 2);
        assert_eq!(replies.last(), Some(&PcmReply::End));
    }

    #[test]
    fn a_start_position_skips_ahead() {
        let replies = run(wav(8_000, 1, 4), 2_500);
        let PcmReply::Format(header) = replies[0] else {
            panic!("no format first: {replies:?}");
        };
        assert!(
            header.start_ms <= 2_500 && header.start_ms >= 2_400,
            "{header:?}"
        );
        let expected = (4_000 - header.start_ms) * 8;
        let got = u64::try_from(samples(&replies)).unwrap();
        assert!(got.abs_diff(expected) <= 1_152, "{got} vs {expected}");
    }

    #[test]
    fn garbage_fails_with_a_message() {
        let replies = run(vec![7; 5000], 0);
        assert!(
            matches!(replies.as_slice(), [PcmReply::Failed { .. }]),
            "{replies:?}"
        );
    }

    #[test]
    fn a_host_that_answers_another_span_ends_the_decode() {
        let (to_worker, worker_rx) = mpsc::channel();
        let (worker_tx, from_worker) = mpsc::channel::<Vec<u8>>();
        let input = ChannelReader {
            pending: Cursor::new(Vec::new()),
            rx: Mutex::new(worker_rx),
        };
        let worker =
            std::thread::spawn(move || serve_pcm(input, ChannelWriter(Mutex::new(worker_tx))));
        to_worker
            .send(encode_request(&PcmRequest::Open {
                size: 100,
                start_ms: 0,
            }))
            .unwrap();
        let mut reader = pcm_wire::ReplyReader::default();
        let mut last = None;
        while let Ok(bytes) = from_worker.recv() {
            reader.push(&bytes);
            while let Some(reply) = reader.next_reply().unwrap() {
                if matches!(reply, PcmReply::Need { .. }) {
                    to_worker
                        .send(encode_request(&PcmRequest::Data {
                            offset: 50,
                            bytes: vec![0; 3],
                        }))
                        .unwrap();
                } else {
                    last = Some(reply);
                }
            }
        }
        assert!(matches!(last, Some(PcmReply::Failed { .. })), "{last:?}");
        worker.join().unwrap().unwrap();
    }
}
