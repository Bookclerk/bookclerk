//! Frames the host and the cover worker exchange over the worker's stdio.
//!
//! The worker is long-lived and handles many covers at once, so each request
//! carries an id and replies may come back in any order. Every frame is a
//! four-byte big-endian length followed by that many bytes of body:
//!
//! | Frame | Body |
//! | --- | --- |
//! | Request | `id: u64`, then the cover bytes (at most [`MAX_INPUT_BYTES`]) |
//! | Reply, ok | `id: u64`, `0u8`, `width: u32`, `height: u32`, then for each [`CoverSize`] in order a `u32` length and that many JPEG bytes |
//! | Reply, error | `id: u64`, `1u8`, then a UTF-8 message of at most [`MAX_ERROR_BYTES`] |
//!
//! All integers are big-endian. The host reads replies from a process that
//! decoded untrusted bytes, so [`decode_reply`] checks every length, and
//! accepts an ok reply only when each image starts with the JPEG signature.

use crate::{sniff, CoverFormat, CoverSet, CoverSize, MAX_INPUT_BYTES};

/// Longest error message a reply carries, in bytes.
pub const MAX_ERROR_BYTES: usize = 1024;

/// Largest request body: an id and a cover.
pub const MAX_REQUEST_BODY: usize = 8 + MAX_INPUT_BYTES;

/// Largest reply body. A JPEG of a cover that fits the decode limits is far
/// smaller than this; a longer frame means the worker is broken.
pub const MAX_REPLY_BODY: usize = 64 * 1024 * 1024;

/// Reply tag of a processed cover.
const TAG_OK: u8 = 0;

/// Reply tag of a refused cover.
const TAG_ERR: u8 = 1;

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
    /// The reply tag is neither ok nor error.
    #[error("unknown reply tag {0}")]
    UnknownTag(u8),
    /// An error message is not UTF-8 or is too long.
    #[error("bad error message")]
    BadMessage,
    /// An ok reply carries an image that is not a JPEG, or zero dimensions.
    #[error("reply image is not a JPEG")]
    NotJpeg,
}

/// One cover the host asks the worker to process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Id the reply echoes.
    pub id: u64,
    /// The untrusted cover bytes.
    pub cover: Vec<u8>,
}

/// The worker's answer to one [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The cover was processed.
    Ok {
        /// The request's id.
        id: u64,
        /// The JPEGs.
        set: CoverSet,
    },
    /// The cover was refused.
    Err {
        /// The request's id.
        id: u64,
        /// Why, for logs; at most [`MAX_ERROR_BYTES`].
        message: String,
    },
}

impl Reply {
    /// The id of the request this answers.
    #[must_use]
    pub fn id(&self) -> u64 {
        match self {
            Self::Ok { id, .. } | Self::Err { id, .. } => *id,
        }
    }
}

/// Reads a frame's length prefix.
///
/// # Arguments
///
/// * `prefix` - The four bytes before the body.
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
/// [`WireError::TooLarge`] for a cover over [`MAX_INPUT_BYTES`].
pub fn encode_request(request: &Request) -> Result<Vec<u8>, WireError> {
    if request.cover.len() > MAX_INPUT_BYTES {
        return Err(WireError::TooLarge);
    }
    let mut body = Vec::with_capacity(8 + request.cover.len());
    body.extend_from_slice(&request.id.to_be_bytes());
    body.extend_from_slice(&request.cover);
    framed(body)
}

/// Decodes a request body (the bytes after the length prefix).
///
/// # Errors
///
/// [`WireError::Truncated`] without a full id, [`WireError::TooLarge`] over
/// [`MAX_REQUEST_BODY`].
pub fn decode_request(body: &[u8]) -> Result<Request, WireError> {
    if body.len() > MAX_REQUEST_BODY {
        return Err(WireError::TooLarge);
    }
    let mut reader = Reader(body);
    let id = reader.u64()?;
    Ok(Request {
        id,
        cover: reader.0.to_vec(),
    })
}

/// Encodes a reply as a whole frame, length prefix included.
///
/// An error message is cut to [`MAX_ERROR_BYTES`] at a character boundary.
///
/// # Errors
///
/// [`WireError::TooLarge`] when an image does not fit a `u32` length or the
/// body is over [`MAX_REPLY_BODY`].
pub fn encode_reply(reply: &Reply) -> Result<Vec<u8>, WireError> {
    let mut body = Vec::new();
    body.extend_from_slice(&reply.id().to_be_bytes());
    match reply {
        Reply::Ok { set, .. } => {
            body.push(TAG_OK);
            body.extend_from_slice(&set.width.to_be_bytes());
            body.extend_from_slice(&set.height.to_be_bytes());
            for size in CoverSize::ALL {
                let image = set.get(size);
                let len = u32::try_from(image.len()).map_err(|_| WireError::TooLarge)?;
                body.extend_from_slice(&len.to_be_bytes());
                body.extend_from_slice(image);
            }
        }
        Reply::Err { message, .. } => {
            body.push(TAG_ERR);
            body.extend_from_slice(truncate(message, MAX_ERROR_BYTES).as_bytes());
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
/// tag, a bad message, zero dimensions, or an image without the JPEG
/// signature.
pub fn decode_reply(body: &[u8]) -> Result<Reply, WireError> {
    if body.len() > MAX_REPLY_BODY {
        return Err(WireError::TooLarge);
    }
    let mut reader = Reader(body);
    let id = reader.u64()?;
    match reader.u8()? {
        TAG_OK => {
            let width = reader.u32()?;
            let height = reader.u32()?;
            if width == 0 || height == 0 {
                return Err(WireError::NotJpeg);
            }
            let mut images = Vec::with_capacity(CoverSize::ALL.len());
            for _ in CoverSize::ALL {
                let len = usize::try_from(reader.u32()?).map_err(|_| WireError::TooLarge)?;
                let image = reader.take(len)?;
                if sniff(image) != Some(CoverFormat::Jpeg) {
                    return Err(WireError::NotJpeg);
                }
                images.push(image.to_vec());
            }
            if !reader.0.is_empty() {
                return Err(WireError::Trailing);
            }
            let mut images = images.into_iter();
            let (Some(thumb), Some(card), Some(full)) =
                (images.next(), images.next(), images.next())
            else {
                return Err(WireError::Truncated);
            };
            Ok(Reply::Ok {
                id,
                set: CoverSet {
                    width,
                    height,
                    thumb,
                    card,
                    full,
                },
            })
        }
        TAG_ERR => {
            if reader.0.len() > MAX_ERROR_BYTES {
                return Err(WireError::BadMessage);
            }
            let message = std::str::from_utf8(reader.0)
                .map_err(|_| WireError::BadMessage)?
                .to_string();
            Ok(Reply::Err { id, message })
        }
        other => Err(WireError::UnknownTag(other)),
    }
}

/// `text` cut to at most `max` bytes at a character boundary.
fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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

    /// The next big-endian `u32`.
    fn u32(&mut self) -> Result<u32, WireError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(|_| WireError::Truncated)?;
        Ok(u32::from_be_bytes(bytes))
    }

    /// The next big-endian `u64`.
    fn u64(&mut self) -> Result<u64, WireError> {
        let bytes: [u8; 8] = self.take(8)?.try_into().map_err(|_| WireError::Truncated)?;
        Ok(u64::from_be_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];

    fn set() -> CoverSet {
        CoverSet {
            width: 10,
            height: 20,
            thumb: JPEG.to_vec(),
            card: [JPEG, &[9]].concat(),
            full: [JPEG, &[8, 8]].concat(),
        }
    }

    fn body(frame: &[u8]) -> &[u8] {
        let len = body_len(frame[..4].try_into().expect("prefix"));
        assert_eq!(len, frame.len() - 4);
        &frame[4..]
    }

    #[test]
    fn requests_round_trip() {
        let request = Request {
            id: 7,
            cover: vec![1, 2, 3],
        };
        let frame = encode_request(&request).expect("encode");
        assert_eq!(decode_request(body(&frame)), Ok(request));
        assert_eq!(decode_request(&[0; 7]), Err(WireError::Truncated));
        assert_eq!(
            encode_request(&Request {
                id: 1,
                cover: vec![0; MAX_INPUT_BYTES + 1]
            }),
            Err(WireError::TooLarge)
        );
        assert_eq!(
            decode_request(&vec![0; MAX_REQUEST_BODY + 1]),
            Err(WireError::TooLarge)
        );
    }

    #[test]
    fn replies_round_trip() {
        for reply in [
            Reply::Ok { id: 3, set: set() },
            Reply::Err {
                id: u64::MAX,
                message: "nope".into(),
            },
        ] {
            let frame = encode_reply(&reply).expect("encode");
            assert_eq!(decode_reply(body(&frame)), Ok(reply));
        }
    }

    #[test]
    fn long_messages_are_cut_at_a_character() {
        let reply = Reply::Err {
            id: 1,
            message: "é".repeat(MAX_ERROR_BYTES),
        };
        let frame = encode_reply(&reply).expect("encode");
        let Ok(Reply::Err { message, .. }) = decode_reply(body(&frame)) else {
            panic!("error reply");
        };
        assert_eq!(message.len(), MAX_ERROR_BYTES);
    }

    #[test]
    fn decode_reply_refuses_malformed_bodies() {
        let good = encode_reply(&Reply::Ok { id: 1, set: set() }).expect("encode");
        let good = body(&good).to_vec();
        for cut in 0..good.len() {
            assert!(decode_reply(&good[..cut]).is_err(), "cut at {cut}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(decode_reply(&trailing), Err(WireError::Trailing));
        let mut not_jpeg = good.clone();
        // First image byte follows id, tag, width, height and its length.
        not_jpeg[8 + 1 + 4 + 4 + 4] = 0;
        assert_eq!(decode_reply(&not_jpeg), Err(WireError::NotJpeg));
        let mut zero = good;
        zero[9..13].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(decode_reply(&zero), Err(WireError::NotJpeg));
        let mut tag = 1u64.to_be_bytes().to_vec();
        tag.push(2);
        assert_eq!(decode_reply(&tag), Err(WireError::UnknownTag(2)));
        let mut bad_text = 1u64.to_be_bytes().to_vec();
        bad_text.extend_from_slice(&[TAG_ERR, 0xFF]);
        assert_eq!(decode_reply(&bad_text), Err(WireError::BadMessage));
        let mut long = 1u64.to_be_bytes().to_vec();
        long.push(TAG_ERR);
        long.extend(std::iter::repeat_n(b'a', MAX_ERROR_BYTES + 1));
        assert_eq!(decode_reply(&long), Err(WireError::BadMessage));
    }
}
