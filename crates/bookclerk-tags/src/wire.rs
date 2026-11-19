//! Frames the host and `bookclerk-media-worker tags` exchange over the
//! worker's stdio.
//!
//! The worker holds a [`crate::TagReader`] per open file and handles many
//! files at once; every frame names its file by an id the host picks. Each
//! frame is a four-byte big-endian length, then that many bytes of body,
//! which start with a one-byte tag:
//!
//! | Direction | Frame | Body after the tag |
//! | --- | --- | --- |
//! | host → worker | `Open` (0) | `id: u64`, `size: u64` |
//! | host → worker | `Data` (1) | `id: u64`, `offset: u64`, then the span's bytes |
//! | host → worker | `Close` (2) | `id: u64` |
//! | worker → host | `Need` (0) | `id: u64`, `count: u16`, then `count` × (`offset: u64`, `len: u64`) |
//! | worker → host | `Done` (1) | `id: u64`, then the [`TagRecord`] as JSON |
//! | worker → host | `Failed` (2) | `id: u64`, then a UTF-8 message of at most [`MAX_ERROR_BYTES`] |
//!
//! All integers are big-endian. The worker reads untrusted files, so the
//! host treats it as compromised: [`decode_reply`] checks every length and
//! count, and the host still checks each span against the file and each
//! record with [`TagRecord::validate`].

use crate::{ByteSpan, TagRecord, MAX_FETCH_BYTES, MAX_SPANS_PER_ROUND};

/// Longest error message a reply carries, in bytes.
pub const MAX_ERROR_BYTES: usize = 1024;

/// Largest request body: a tag, an id, an offset and one span's bytes
/// (at most [`MAX_FETCH_BYTES`]).
pub const MAX_REQUEST_BODY: usize = 17 + 16 * 1024 * 1024;

/// Largest reply body. A record at every limit is under 40 MiB of JSON.
pub const MAX_REPLY_BODY: usize = 48 * 1024 * 1024;

/// Request tag of [`Request::Open`].
const OPEN: u8 = 0;
/// Request tag of [`Request::Data`].
const DATA: u8 = 1;
/// Request tag of [`Request::Close`].
const CLOSE: u8 = 2;
/// Reply tag of [`Reply::Need`].
const NEED: u8 = 0;
/// Reply tag of [`Reply::Done`].
const DONE: u8 = 1;
/// Reply tag of [`Reply::Failed`].
const FAILED: u8 = 2;

/// Why a frame body could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
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
    /// A `Need` with no spans or more than [`MAX_SPANS_PER_ROUND`], or an
    /// empty or overflowing span.
    #[error("bad span list")]
    BadSpans,
    /// An error message that is not UTF-8 or is too long.
    #[error("bad error message")]
    BadMessage,
    /// A `Done` whose record is not valid JSON for a [`TagRecord`].
    #[error("bad record: {0}")]
    BadRecord(String),
}

/// A frame the host sends the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Start reading a file of `size` bytes as `id`.
    Open {
        /// The file's id; unique among open files.
        id: u64,
        /// The file's size in bytes.
        size: u64,
    },
    /// The bytes of one span the worker asked for.
    Data {
        /// The file's id.
        id: u64,
        /// The span's offset.
        offset: u64,
        /// The span's bytes.
        bytes: Vec<u8>,
    },
    /// Forget a file (the host gave up on it).
    Close {
        /// The file's id.
        id: u64,
    },
}

/// A frame the worker sends the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Send these spans of the file.
    Need {
        /// The file's id.
        id: u64,
        /// One to [`MAX_SPANS_PER_ROUND`] non-empty spans.
        spans: Vec<ByteSpan>,
    },
    /// The file's record; the worker has forgotten the file.
    Done {
        /// The file's id.
        id: u64,
        /// What the file holds.
        record: Box<TagRecord>,
    },
    /// The file could not be read; the worker has forgotten it.
    Failed {
        /// The file's id.
        id: u64,
        /// Why, for logs; at most [`MAX_ERROR_BYTES`].
        message: String,
    },
}

impl Reply {
    /// The id of the file this reply is about.
    #[must_use]
    pub fn id(&self) -> u64 {
        match self {
            Self::Need { id, .. } | Self::Done { id, .. } | Self::Failed { id, .. } => *id,
        }
    }
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
/// [`WireError::TooLarge`] when the body does not fit a `u32` length.
fn framed(body: Vec<u8>) -> Result<Vec<u8>, WireError> {
    let len = u32::try_from(body.len()).map_err(|_| WireError::TooLarge)?;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Encodes a request as a whole frame, length prefix included.
///
/// # Errors
///
/// [`WireError::TooLarge`] when the body is over [`MAX_REQUEST_BODY`].
pub fn encode_request(request: &Request) -> Result<Vec<u8>, WireError> {
    let mut body = Vec::new();
    match request {
        Request::Open { id, size } => {
            body.push(OPEN);
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&size.to_be_bytes());
        }
        Request::Data { id, offset, bytes } => {
            if bytes.len() > MAX_REQUEST_BODY - 17 {
                return Err(WireError::TooLarge);
            }
            body.reserve(17 + bytes.len());
            body.push(DATA);
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(bytes);
        }
        Request::Close { id } => {
            body.push(CLOSE);
            body.extend_from_slice(&id.to_be_bytes());
        }
    }
    framed(body)
}

/// Decodes a request body (the bytes after the length prefix).
///
/// # Errors
///
/// A [`WireError`] for a truncated, oversized or trailing body or an
/// unknown tag.
pub fn decode_request(body: &[u8]) -> Result<Request, WireError> {
    if body.len() > MAX_REQUEST_BODY {
        return Err(WireError::TooLarge);
    }
    let mut reader = Reader(body);
    let tag = reader.u8()?;
    let id = reader.u64()?;
    let request = match tag {
        OPEN => Request::Open {
            id,
            size: reader.u64()?,
        },
        DATA => {
            let offset = reader.u64()?;
            return Ok(Request::Data {
                id,
                offset,
                bytes: reader.0.to_vec(),
            });
        }
        CLOSE => Request::Close { id },
        other => return Err(WireError::UnknownTag(other)),
    };
    reader.end()?;
    Ok(request)
}

/// Encodes a reply as a whole frame, length prefix included.
///
/// An error message is cut to [`MAX_ERROR_BYTES`] at a character boundary.
///
/// # Errors
///
/// [`WireError::BadSpans`] for a span list the host would refuse,
/// [`WireError::BadRecord`] when the record cannot be serialized, and
/// [`WireError::TooLarge`] over [`MAX_REPLY_BODY`].
pub fn encode_reply(reply: &Reply) -> Result<Vec<u8>, WireError> {
    let mut body = Vec::new();
    match reply {
        Reply::Need { id, spans } => {
            check_spans(spans)?;
            let count = u16::try_from(spans.len()).map_err(|_| WireError::BadSpans)?;
            body.push(NEED);
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&count.to_be_bytes());
            for span in spans {
                body.extend_from_slice(&span.offset.to_be_bytes());
                body.extend_from_slice(&span.len.to_be_bytes());
            }
        }
        Reply::Done { id, record } => {
            body.push(DONE);
            body.extend_from_slice(&id.to_be_bytes());
            serde_json::to_writer(&mut body, record)
                .map_err(|err| WireError::BadRecord(err.to_string()))?;
        }
        Reply::Failed { id, message } => {
            body.push(FAILED);
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(crate::text::cut(message, MAX_ERROR_BYTES).as_bytes());
        }
    }
    if body.len() > MAX_REPLY_BODY {
        return Err(WireError::TooLarge);
    }
    framed(body)
}

/// Decodes a reply body (the bytes after the length prefix).
///
/// # Errors
///
/// A [`WireError`] for a truncated, oversized or trailing body, an unknown
/// tag, a bad span list, a bad message or a record that does not parse.
pub fn decode_reply(body: &[u8]) -> Result<Reply, WireError> {
    if body.len() > MAX_REPLY_BODY {
        return Err(WireError::TooLarge);
    }
    let mut reader = Reader(body);
    let tag = reader.u8()?;
    let id = reader.u64()?;
    match tag {
        NEED => {
            let count = usize::from(reader.u16()?);
            if count == 0 || count > MAX_SPANS_PER_ROUND {
                return Err(WireError::BadSpans);
            }
            let mut spans = Vec::with_capacity(count);
            for _ in 0..count {
                spans.push(ByteSpan {
                    offset: reader.u64()?,
                    len: reader.u64()?,
                });
            }
            reader.end()?;
            check_spans(&spans)?;
            Ok(Reply::Need { id, spans })
        }
        DONE => {
            let record: TagRecord = serde_json::from_slice(reader.0).map_err(|err| {
                WireError::BadRecord(crate::text::cut(&err.to_string(), 200).into())
            })?;
            Ok(Reply::Done {
                id,
                record: Box::new(record),
            })
        }
        FAILED => {
            if reader.0.len() > MAX_ERROR_BYTES {
                return Err(WireError::BadMessage);
            }
            let message = std::str::from_utf8(reader.0)
                .map_err(|_| WireError::BadMessage)?
                .to_string();
            Ok(Reply::Failed { id, message })
        }
        other => Err(WireError::UnknownTag(other)),
    }
}

/// Checks a `Need` span list: one to [`MAX_SPANS_PER_ROUND`] spans, each
/// non-empty, no longer than [`MAX_FETCH_BYTES`], and not overflowing.
fn check_spans(spans: &[ByteSpan]) -> Result<(), WireError> {
    if spans.is_empty() || spans.len() > MAX_SPANS_PER_ROUND {
        return Err(WireError::BadSpans);
    }
    if spans
        .iter()
        .any(|span| span.len == 0 || span.len > MAX_FETCH_BYTES || span.end().is_none())
    {
        return Err(WireError::BadSpans);
    }
    Ok(())
}

/// A cursor over a frame body.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// The next `len` bytes.
    fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        if self.0.len() < len {
            return Err(WireError::Truncated);
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }

    /// The next byte.
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    /// The next big-endian `u16`.
    fn u16(&mut self) -> Result<u16, WireError> {
        let bytes: [u8; 2] = self.take(2)?.try_into().map_err(|_| WireError::Truncated)?;
        Ok(u16::from_be_bytes(bytes))
    }

    /// The next big-endian `u64`.
    fn u64(&mut self) -> Result<u64, WireError> {
        let bytes: [u8; 8] = self.take(8)?.try_into().map_err(|_| WireError::Truncated)?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// Fails unless the body is used up.
    fn end(&self) -> Result<(), WireError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(WireError::Trailing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_all;

    fn body(frame: &[u8]) -> &[u8] {
        let len = body_len(frame[..4].try_into().expect("prefix"));
        assert_eq!(len, frame.len() - 4);
        &frame[4..]
    }

    #[test]
    fn a_request_fits_the_largest_span() {
        assert_eq!(
            u64::try_from(MAX_REQUEST_BODY - 17).expect("fits"),
            MAX_FETCH_BYTES
        );
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            Request::Open { id: 1, size: 99 },
            Request::Data {
                id: 2,
                offset: 7,
                bytes: vec![1, 2, 3],
            },
            Request::Close { id: u64::MAX },
        ] {
            let frame = encode_request(&request).expect("encode");
            assert_eq!(decode_request(body(&frame)), Ok(request));
        }
        assert_eq!(decode_request(&[OPEN, 0, 0]), Err(WireError::Truncated));
        assert_eq!(decode_request(&[9; 9]), Err(WireError::UnknownTag(9)));
        let mut trailing = body(&encode_request(&Request::Close { id: 1 }).expect("e")).to_vec();
        trailing.push(0);
        assert_eq!(decode_request(&trailing), Err(WireError::Trailing));
    }

    #[test]
    fn replies_round_trip() {
        let record = read_all(&crate::fixtures::tagged_flac()).expect("read");
        for reply in [
            Reply::Need {
                id: 3,
                spans: vec![ByteSpan { offset: 0, len: 10 }],
            },
            Reply::Done {
                id: 4,
                record: Box::new(record),
            },
            Reply::Failed {
                id: 5,
                message: "nope".into(),
            },
        ] {
            let frame = encode_reply(&reply).expect("encode");
            assert_eq!(decode_reply(body(&frame)), Ok(reply));
        }
    }

    #[test]
    fn bad_replies_are_refused() {
        let need = |count: u16, spans: &[(u64, u64)]| {
            let mut out = vec![NEED];
            out.extend_from_slice(&1u64.to_be_bytes());
            out.extend_from_slice(&count.to_be_bytes());
            for (offset, len) in spans {
                out.extend_from_slice(&offset.to_be_bytes());
                out.extend_from_slice(&len.to_be_bytes());
            }
            out
        };
        assert_eq!(decode_reply(&need(0, &[])), Err(WireError::BadSpans));
        assert_eq!(decode_reply(&need(1, &[(0, 0)])), Err(WireError::BadSpans));
        assert_eq!(
            decode_reply(&need(1, &[(u64::MAX, 2)])),
            Err(WireError::BadSpans)
        );
        assert_eq!(decode_reply(&need(2, &[(0, 1)])), Err(WireError::Truncated));
        let mut long = vec![FAILED];
        long.extend_from_slice(&1u64.to_be_bytes());
        long.extend(std::iter::repeat_n(b'a', MAX_ERROR_BYTES + 1));
        assert_eq!(decode_reply(&long), Err(WireError::BadMessage));
        let mut json = vec![DONE];
        json.extend_from_slice(&1u64.to_be_bytes());
        json.extend_from_slice(b"{}");
        assert!(matches!(decode_reply(&json), Err(WireError::BadRecord(_))));
        assert_eq!(
            encode_reply(&Reply::Need {
                id: 1,
                spans: vec![]
            }),
            Err(WireError::BadSpans)
        );
    }
}
