//! Frames the host and `bookclerk-media-worker pcm` exchange over the
//! worker's stdio.
//!
//! One worker process decodes one audio file, from a start position to its
//! end, for a player. The worker opens nothing: it asks the host for the
//! byte ranges its demuxer reads and answers with decoded PCM. Each frame is
//! a four-byte big-endian length, then that many bytes of body, which start
//! with a one-byte tag:
//!
//! | Direction | Frame | Body after the tag |
//! | --- | --- | --- |
//! | host → worker | `Open` (0) | `size: u64`, `start_ms: u64` |
//! | host → worker | `Data` (1) | `offset: u64`, then the span's bytes |
//! | worker → host | `Need` (0) | `offset: u64`, `len: u32` |
//! | worker → host | `Format` (1) | `sample_rate: u32`, `channels: u8`, `start_ms: u64`, `duration_ms: u64` |
//! | worker → host | `Pcm` (2) | interleaved signed 16-bit samples, little-endian |
//! | worker → host | `End` (3) | nothing |
//! | worker → host | `Failed` (4) | a UTF-8 message of at most [`MAX_ERROR_BYTES`] |
//!
//! Integers in headers are big-endian; samples are little-endian, the
//! byte order every platform the worker ships on uses natively. A
//! `duration_ms` of [`UNKNOWN_DURATION`] means the container states none.
//!
//! The worker parses untrusted files, so the host treats it as compromised:
//! [`ReplyReader`] checks every length and field of each frame, and
//! [`ReplyOrder`] checks the stream as a whole (one `Format` before any
//! `Pcm`, whole frames of samples, nothing after `End` or `Failed`). The
//! host still checks each `Need` against the file it is reading.

use crate::decode::MAX_SAMPLE_RATE_HZ;

/// First worker argument that selects the PCM decoder.
pub const PCM_WORKER_ARG: &str = "pcm";

/// Longest error message a reply carries, in bytes.
pub const MAX_ERROR_BYTES: usize = 1024;

/// Most bytes one `Need` asks for (1 MiB).
pub const MAX_NEED_BYTES: u32 = 1024 * 1024;

/// Most samples (not frames) one `Pcm` frame carries: a FLAC block at its
/// largest (65 535 frames) in stereo fits.
pub const MAX_PCM_SAMPLES: usize = 2 * 65_536;

/// Largest request body: a tag, an offset and one span's bytes.
pub const MAX_REQUEST_BODY: usize = 9 + MAX_NEED_BYTES as usize;

/// Largest reply body: a tag and a `Pcm` frame at its largest.
pub const MAX_REPLY_BODY: usize = 1 + 2 * MAX_PCM_SAMPLES;

/// The `duration_ms` a `Format` frame carries when the container states
/// no duration.
pub const UNKNOWN_DURATION: u64 = u64::MAX;

/// Longest position or duration a `Format` frame may state, in
/// milliseconds: sixty days, the playback core's own limit.
pub const MAX_POSITION_MS: u64 = 60 * 24 * 60 * 60 * 1000;

/// Request tag of [`PcmRequest::Open`].
const OPEN: u8 = 0;
/// Request tag of [`PcmRequest::Data`].
const DATA: u8 = 1;
/// Reply tag of [`PcmReply::Need`].
const NEED: u8 = 0;
/// Reply tag of [`PcmReply::Format`].
const FORMAT: u8 = 1;
/// Reply tag of [`PcmReply::Pcm`].
const PCM: u8 = 2;
/// Reply tag of [`PcmReply::End`].
const END: u8 = 3;
/// Reply tag of [`PcmReply::Failed`].
const FAILED: u8 = 4;

/// Why a frame, or the stream of frames, could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PcmWireError {
    /// The body ended before a field it promised.
    #[error("frame is truncated")]
    Truncated,
    /// The body has bytes after its last field.
    #[error("frame has trailing bytes")]
    Trailing,
    /// The body is over the frame's size cap.
    #[error("frame is too large")]
    TooLarge,
    /// The tag byte is not one this direction uses.
    #[error("unknown frame tag {0}")]
    UnknownTag(u8),
    /// A `Need` for no bytes, more than [`MAX_NEED_BYTES`], or a span that
    /// overflows.
    #[error("bad span")]
    BadSpan,
    /// A `Format` with a rate outside 1 to [`MAX_SAMPLE_RATE_HZ`], a channel
    /// count other than 1 or 2, or a position or duration over
    /// [`MAX_POSITION_MS`].
    #[error("bad format")]
    BadFormat,
    /// A `Pcm` frame with no samples, more than [`MAX_PCM_SAMPLES`], or an
    /// odd byte count.
    #[error("bad samples")]
    BadSamples,
    /// An error message that is not UTF-8 or is too long.
    #[error("bad error message")]
    BadMessage,
    /// A frame out of order: `Pcm` before `Format`, a second `Format`, a
    /// partial frame of samples, or anything after `End` or `Failed`.
    #[error("frame out of order: {0}")]
    OutOfOrder(&'static str),
}

/// A frame the host sends the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcmRequest {
    /// Start decoding a file of `size` bytes from `start_ms`. The first
    /// frame on the worker's stdin, and only once.
    Open {
        /// The file's size in bytes.
        size: u64,
        /// Where to start, in milliseconds from the start of the file.
        start_ms: u64,
    },
    /// The bytes of the span the worker last asked for.
    Data {
        /// The span's offset.
        offset: u64,
        /// The span's bytes.
        bytes: Vec<u8>,
    },
}

/// The layout of the PCM that follows, and where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmHeader {
    /// Samples per second per channel, in Hz; 1 to [`MAX_SAMPLE_RATE_HZ`].
    pub sample_rate: u32,
    /// Interleaved channels in every `Pcm` frame: 1 or 2.
    pub channels: u8,
    /// Where the first sample sits in the file, in milliseconds: the
    /// asked-for start, or the packet boundary before it.
    pub start_ms: u64,
    /// The file's length in milliseconds, when the container states one.
    pub duration_ms: Option<u64>,
}

/// A frame the worker sends the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcmReply {
    /// Send this span of the file.
    Need {
        /// The span's offset.
        offset: u64,
        /// Its length in bytes: 1 to [`MAX_NEED_BYTES`].
        len: u32,
    },
    /// The PCM layout; sent once, before the first `Pcm`.
    Format(PcmHeader),
    /// Decoded samples, interleaved, a whole number of frames.
    Pcm(Vec<i16>),
    /// The file decoded to its end; the worker exits.
    End,
    /// The file could not be decoded; the worker exits.
    Failed {
        /// Why, for logs; at most [`MAX_ERROR_BYTES`].
        message: String,
    },
}

/// Reads a frame's length prefix.
///
/// # Returns
///
/// The body length in bytes.
#[must_use]
pub fn body_len(prefix: [u8; 4]) -> usize {
    usize::try_from(u32::from_be_bytes(prefix)).unwrap_or(usize::MAX)
}

/// Prefixes `body` with its length.
///
/// # Errors
///
/// [`PcmWireError::TooLarge`] when the body does not fit a `u32` length.
fn framed(body: Vec<u8>) -> Result<Vec<u8>, PcmWireError> {
    let len = u32::try_from(body.len()).map_err(|_| PcmWireError::TooLarge)?;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Encodes a request as a whole frame, length prefix included.
///
/// # Errors
///
/// [`PcmWireError::TooLarge`] when a `Data` span is over
/// [`MAX_NEED_BYTES`].
pub fn encode_request(request: &PcmRequest) -> Result<Vec<u8>, PcmWireError> {
    let mut body = Vec::new();
    match request {
        PcmRequest::Open { size, start_ms } => {
            body.push(OPEN);
            body.extend_from_slice(&size.to_be_bytes());
            body.extend_from_slice(&start_ms.to_be_bytes());
        }
        PcmRequest::Data { offset, bytes } => {
            if bytes.len() > MAX_REQUEST_BODY - 9 {
                return Err(PcmWireError::TooLarge);
            }
            body.reserve(9 + bytes.len());
            body.push(DATA);
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(bytes);
        }
    }
    framed(body)
}

/// Decodes a request body (the frame without its length prefix).
///
/// # Errors
///
/// A [`PcmWireError`] for a body that is too large, truncated, has
/// trailing bytes or an unknown tag.
pub fn decode_request(body: &[u8]) -> Result<PcmRequest, PcmWireError> {
    if body.len() > MAX_REQUEST_BODY {
        return Err(PcmWireError::TooLarge);
    }
    let mut reader = Fields(body);
    let request = match reader.u8()? {
        OPEN => PcmRequest::Open {
            size: reader.u64()?,
            start_ms: reader.u64()?,
        },
        DATA => PcmRequest::Data {
            offset: reader.u64()?,
            bytes: reader.rest().to_vec(),
        },
        tag => return Err(PcmWireError::UnknownTag(tag)),
    };
    reader.finish()?;
    Ok(request)
}

/// Encodes a reply as a whole frame, length prefix included.
///
/// An error message is cut to [`MAX_ERROR_BYTES`] at a character boundary.
///
/// # Errors
///
/// [`PcmWireError::BadSamples`] for a `Pcm` frame with no samples or more
/// than [`MAX_PCM_SAMPLES`].
pub fn encode_reply(reply: &PcmReply) -> Result<Vec<u8>, PcmWireError> {
    let mut body = Vec::new();
    match reply {
        PcmReply::Need { offset, len } => {
            body.push(NEED);
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(&len.to_be_bytes());
        }
        PcmReply::Format(header) => {
            body.push(FORMAT);
            body.extend_from_slice(&header.sample_rate.to_be_bytes());
            body.push(header.channels);
            body.extend_from_slice(&header.start_ms.to_be_bytes());
            body.extend_from_slice(&header.duration_ms.unwrap_or(UNKNOWN_DURATION).to_be_bytes());
        }
        PcmReply::Pcm(samples) => {
            if samples.is_empty() || samples.len() > MAX_PCM_SAMPLES {
                return Err(PcmWireError::BadSamples);
            }
            body.reserve(1 + 2 * samples.len());
            body.push(PCM);
            for sample in samples {
                body.extend_from_slice(&sample.to_le_bytes());
            }
        }
        PcmReply::End => body.push(END),
        PcmReply::Failed { message } => {
            body.push(FAILED);
            body.extend_from_slice(cut(message, MAX_ERROR_BYTES).as_bytes());
        }
    }
    framed(body)
}

/// Decodes a reply body (the frame without its length prefix) and checks
/// each field against its limit.
///
/// # Errors
///
/// A [`PcmWireError`] for a body that is too large, truncated, has
/// trailing bytes or an unknown tag, or carries a field out of bounds.
pub fn decode_reply(body: &[u8]) -> Result<PcmReply, PcmWireError> {
    if body.len() > MAX_REPLY_BODY {
        return Err(PcmWireError::TooLarge);
    }
    let mut reader = Fields(body);
    let reply = match reader.u8()? {
        NEED => {
            let offset = reader.u64()?;
            let len = reader.u32()?;
            if len == 0 || len > MAX_NEED_BYTES || offset.checked_add(u64::from(len)).is_none() {
                return Err(PcmWireError::BadSpan);
            }
            PcmReply::Need { offset, len }
        }
        FORMAT => {
            let sample_rate = reader.u32()?;
            let channels = reader.u8()?;
            let start_ms = reader.u64()?;
            let duration = reader.u64()?;
            let duration_ms = (duration != UNKNOWN_DURATION).then_some(duration);
            if !(1..=MAX_SAMPLE_RATE_HZ).contains(&sample_rate)
                || !(1..=2).contains(&channels)
                || start_ms > MAX_POSITION_MS
                || duration_ms.is_some_and(|ms| ms > MAX_POSITION_MS)
            {
                return Err(PcmWireError::BadFormat);
            }
            PcmReply::Format(PcmHeader {
                sample_rate,
                channels,
                start_ms,
                duration_ms,
            })
        }
        PCM => {
            let bytes = reader.rest();
            if bytes.is_empty() || bytes.len() % 2 != 0 || bytes.len() / 2 > MAX_PCM_SAMPLES {
                return Err(PcmWireError::BadSamples);
            }
            PcmReply::Pcm(
                bytes
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect(),
            )
        }
        END => PcmReply::End,
        FAILED => {
            let bytes = reader.rest();
            if bytes.len() > MAX_ERROR_BYTES {
                return Err(PcmWireError::BadMessage);
            }
            let message = std::str::from_utf8(bytes)
                .map_err(|_| PcmWireError::BadMessage)?
                .to_owned();
            PcmReply::Failed { message }
        }
        tag => return Err(PcmWireError::UnknownTag(tag)),
    };
    reader.finish()?;
    Ok(reply)
}

/// Splits the worker's output into checked replies, fed bytes as they
/// arrive (sans IO: the host's reader task and the fuzz target drive the
/// same code).
#[derive(Debug, Default)]
pub struct ReplyReader {
    /// Bytes received and not yet taken as a whole frame.
    pending: Vec<u8>,
    /// Set once a frame failed; every later call returns the same error.
    broken: Option<PcmWireError>,
}

impl ReplyReader {
    /// Appends bytes read from the worker.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.broken.is_none() {
            self.pending.extend_from_slice(bytes);
        }
    }

    /// Bytes held that are not yet a whole frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }

    /// Takes the next whole reply, if one has arrived.
    ///
    /// # Returns
    ///
    /// `None` until a whole frame is buffered.
    ///
    /// # Errors
    ///
    /// [`PcmWireError::TooLarge`] as soon as a length prefix is over
    /// [`MAX_REPLY_BODY`], or what [`decode_reply`] returns. The reader is
    /// then broken and returns that error from every later call.
    pub fn next_reply(&mut self) -> Result<Option<PcmReply>, PcmWireError> {
        if let Some(err) = &self.broken {
            return Err(err.clone());
        }
        let Some(prefix) = self.pending.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = body_len(*prefix);
        if len > MAX_REPLY_BODY {
            return Err(self.fail(PcmWireError::TooLarge));
        }
        let Some(body) = self.pending.get(4..4 + len) else {
            return Ok(None);
        };
        let decoded = decode_reply(body);
        self.pending.drain(..4 + len);
        decoded.map(Some).map_err(|err| self.fail(err))
    }

    /// Breaks the reader with `err` and returns it.
    fn fail(&mut self, err: PcmWireError) -> PcmWireError {
        self.pending = Vec::new();
        self.broken = Some(err.clone());
        err
    }
}

/// Where a reply stream is: the rules [`ReplyOrder::check`] enforces on top
/// of each frame's own checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplyOrder {
    /// No `Format` yet: only `Need`, `Format` or `Failed` may come.
    #[default]
    Opening,
    /// Streaming samples in the stated layout.
    Streaming(PcmHeader),
    /// `End` or `Failed` came; nothing more may.
    Done,
}

impl ReplyOrder {
    /// Checks that `reply` may come next and moves past it.
    ///
    /// # Errors
    ///
    /// [`PcmWireError::OutOfOrder`] for `Pcm` or `End` before `Format`, a
    /// second `Format`, samples that are not a whole number of frames, or
    /// any frame after `End` or `Failed`. The order is then
    /// [`ReplyOrder::Done`].
    pub fn check(&mut self, reply: &PcmReply) -> Result<(), PcmWireError> {
        let next = match (*self, reply) {
            (Self::Done, _) => Err("a frame after the end"),
            (_, PcmReply::Failed { .. }) => Ok(Self::Done),
            (Self::Opening, PcmReply::Need { .. }) => Ok(Self::Opening),
            (Self::Opening, PcmReply::Format(header)) => Ok(Self::Streaming(*header)),
            (Self::Opening, PcmReply::Pcm(_) | PcmReply::End) => Err("samples before the format"),
            (Self::Streaming(_), PcmReply::Format(_)) => Err("a second format"),
            (Self::Streaming(header), PcmReply::Need { .. }) => Ok(Self::Streaming(header)),
            (Self::Streaming(header), PcmReply::Pcm(samples)) => {
                if samples.len() % usize::from(header.channels) == 0 {
                    Ok(Self::Streaming(header))
                } else {
                    Err("a partial frame of samples")
                }
            }
            (Self::Streaming(_), PcmReply::End) => Ok(Self::Done),
        };
        match next {
            Ok(next) => {
                *self = next;
                Ok(())
            }
            Err(why) => {
                *self = Self::Done;
                Err(PcmWireError::OutOfOrder(why))
            }
        }
    }
}

/// `text` cut to at most `max` bytes at a character boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A cursor over a frame body's fields.
struct Fields<'a>(
    /// The bytes not read yet.
    &'a [u8],
);

impl<'a> Fields<'a> {
    /// Takes `N` bytes.
    fn take<const N: usize>(&mut self) -> Result<[u8; N], PcmWireError> {
        let (head, tail) = self
            .0
            .split_first_chunk::<N>()
            .ok_or(PcmWireError::Truncated)?;
        self.0 = tail;
        Ok(*head)
    }

    /// Takes one byte.
    fn u8(&mut self) -> Result<u8, PcmWireError> {
        Ok(self.take::<1>()?[0])
    }

    /// Takes a big-endian `u32`.
    fn u32(&mut self) -> Result<u32, PcmWireError> {
        Ok(u32::from_be_bytes(self.take()?))
    }

    /// Takes a big-endian `u64`.
    fn u64(&mut self) -> Result<u64, PcmWireError> {
        Ok(u64::from_be_bytes(self.take()?))
    }

    /// Takes everything left.
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    /// Checks that nothing is left.
    fn finish(&self) -> Result<(), PcmWireError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(PcmWireError::Trailing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> PcmHeader {
        PcmHeader {
            sample_rate: 44_100,
            channels: 2,
            start_ms: 1_500,
            duration_ms: Some(60_000),
        }
    }

    #[test]
    fn requests_and_replies_round_trip() {
        for request in [
            PcmRequest::Open {
                size: 9,
                start_ms: 7,
            },
            PcmRequest::Data {
                offset: 3,
                bytes: vec![1, 2, 3],
            },
        ] {
            let frame = encode_request(&request).unwrap();
            assert_eq!(body_len(*frame.first_chunk().unwrap()), frame.len() - 4);
            assert_eq!(decode_request(&frame[4..]).unwrap(), request);
        }
        for reply in [
            PcmReply::Need {
                offset: 10,
                len: MAX_NEED_BYTES,
            },
            PcmReply::Format(header()),
            PcmReply::Format(PcmHeader {
                duration_ms: None,
                ..header()
            }),
            PcmReply::Pcm(vec![i16::MIN, -1, 0, i16::MAX]),
            PcmReply::End,
            PcmReply::Failed {
                message: "nope".into(),
            },
        ] {
            let frame = encode_reply(&reply).unwrap();
            assert_eq!(decode_reply(&frame[4..]).unwrap(), reply);
        }
    }

    #[test]
    fn replies_out_of_bounds_are_refused() {
        let need = |offset: u64, len: u32| {
            let mut body = vec![NEED];
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(&len.to_be_bytes());
            decode_reply(&body)
        };
        assert_eq!(need(0, 0), Err(PcmWireError::BadSpan));
        assert_eq!(need(0, MAX_NEED_BYTES + 1), Err(PcmWireError::BadSpan));
        assert_eq!(need(u64::MAX, 1), Err(PcmWireError::BadSpan));

        let format = |rate: u32, channels: u8, start: u64, duration: u64| {
            let mut body = vec![FORMAT];
            body.extend_from_slice(&rate.to_be_bytes());
            body.push(channels);
            body.extend_from_slice(&start.to_be_bytes());
            body.extend_from_slice(&duration.to_be_bytes());
            decode_reply(&body)
        };
        assert!(format(44_100, 2, 0, UNKNOWN_DURATION).is_ok());
        assert_eq!(format(0, 2, 0, 0), Err(PcmWireError::BadFormat));
        assert_eq!(
            format(MAX_SAMPLE_RATE_HZ + 1, 2, 0, 0),
            Err(PcmWireError::BadFormat)
        );
        assert_eq!(format(44_100, 3, 0, 0), Err(PcmWireError::BadFormat));
        assert_eq!(format(44_100, 0, 0, 0), Err(PcmWireError::BadFormat));
        assert_eq!(
            format(44_100, 1, MAX_POSITION_MS + 1, 0),
            Err(PcmWireError::BadFormat)
        );

        assert_eq!(decode_reply(&[PCM]), Err(PcmWireError::BadSamples));
        assert_eq!(decode_reply(&[PCM, 1]), Err(PcmWireError::BadSamples));
        assert_eq!(decode_reply(&[END, 0]), Err(PcmWireError::Trailing));
        assert_eq!(decode_reply(&[9]), Err(PcmWireError::UnknownTag(9)));
        assert_eq!(decode_reply(&[]), Err(PcmWireError::Truncated));
        assert_eq!(decode_reply(&[FAILED, 0xff]), Err(PcmWireError::BadMessage));
        assert_eq!(
            encode_reply(&PcmReply::Pcm(vec![0; MAX_PCM_SAMPLES + 1])),
            Err(PcmWireError::BadSamples)
        );
    }

    #[test]
    fn long_messages_are_cut_at_a_character_boundary() {
        let frame = encode_reply(&PcmReply::Failed {
            message: "é".repeat(MAX_ERROR_BYTES),
        })
        .unwrap();
        let PcmReply::Failed { message } = decode_reply(&frame[4..]).unwrap() else {
            panic!("not a failure");
        };
        assert_eq!(message.len(), MAX_ERROR_BYTES);
    }

    #[test]
    fn the_reader_reassembles_split_frames_and_breaks_on_a_bad_one() {
        let mut bytes = encode_reply(&PcmReply::Format(header())).unwrap();
        bytes.extend(encode_reply(&PcmReply::Pcm(vec![1, 2])).unwrap());
        let mut reader = ReplyReader::default();
        let mut replies = Vec::new();
        for byte in &bytes {
            reader.push(std::slice::from_ref(byte));
            while let Some(reply) = reader.next_reply().unwrap() {
                replies.push(reply);
            }
        }
        assert_eq!(
            replies,
            vec![PcmReply::Format(header()), PcmReply::Pcm(vec![1, 2])]
        );
        assert_eq!(reader.buffered(), 0);

        let huge = u32::try_from(MAX_REPLY_BODY + 1).unwrap().to_be_bytes();
        reader.push(&huge);
        assert_eq!(reader.next_reply(), Err(PcmWireError::TooLarge));
        reader.push(&encode_reply(&PcmReply::End).unwrap());
        assert_eq!(reader.next_reply(), Err(PcmWireError::TooLarge));
    }

    #[test]
    fn the_order_admits_one_format_whole_frames_and_nothing_after_the_end() {
        let mut order = ReplyOrder::default();
        order.check(&PcmReply::Need { offset: 0, len: 1 }).unwrap();
        assert!(ReplyOrder::default()
            .check(&PcmReply::Pcm(vec![1]))
            .is_err());
        assert!(ReplyOrder::default().check(&PcmReply::End).is_err());
        order.check(&PcmReply::Format(header())).unwrap();
        assert!(order.clone().check(&PcmReply::Format(header())).is_err());
        assert!(order.clone().check(&PcmReply::Pcm(vec![1, 2, 3])).is_err());
        order.check(&PcmReply::Pcm(vec![1, 2])).unwrap();
        order.check(&PcmReply::Need { offset: 5, len: 1 }).unwrap();
        order.check(&PcmReply::End).unwrap();
        assert_eq!(order, ReplyOrder::Done);
        assert!(order.check(&PcmReply::End).is_err());
        let mut failed = ReplyOrder::default();
        failed
            .check(&PcmReply::Failed {
                message: String::new(),
            })
            .unwrap();
        assert_eq!(failed, ReplyOrder::Done);
    }
}
