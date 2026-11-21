//! Shared oracle for the `upload_part` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The `Content-Range` header of an upload `PUT`
//! (`/api/node/v1/uploads/{id}/files/{n}`) comes from whoever holds an
//! upload token, and [`parse_upload_range`] decides from it which bytes of
//! a declared file the request body is, or that the request only asks
//! where the owner is. The input is the file's declared
//! size (the first eight bytes, big endian; missing bytes are zero)
//! followed by the header as sent.
//!
//! Whatever the header is:
//!
//! - a part lies inside the file: at least one byte, starting before the
//!   end and ending at or before it, so the owner never writes past the
//!   declared size;
//! - a part is given only to a header that names it back exactly
//!   ([`UploadPart::content_range`] gives the header as sent), and a status
//!   query only to `bytes */<size>` ([`UploadRange::status_header`]), so
//!   there is one spelling of each;
//! - the answer equals the one worked out here by a second reading of the
//!   header (digits folded into a wide integer rather than parsed), so the
//!   parser and the oracle would have to be wrong the same way.

use bookclerk_node_proto::{parse_upload_range, UploadPart, UploadRange};

/// Largest input worth checking: the size and a header far longer than
/// any part the parser reads.
pub const MAX_INPUT_BYTES: usize = 8 + 256;

/// A canonical run of 1 to 19 ASCII digits as a wide integer; `None` for
/// anything else.
fn digits(text: &[u8]) -> Option<u128> {
    if text.is_empty() || text.len() > 19 || (text.len() > 1 && text[0] == b'0') {
        return None;
    }
    text.iter().try_fold(0u128, |sum, byte| {
        byte.is_ascii_digit()
            .then(|| sum * 10 + u128::from(byte - b'0'))
    })
}

/// What the header should be read as, read a second way.
fn expected(header: &[u8], size: u64) -> Option<UploadRange> {
    let text = std::str::from_utf8(header).ok()?;
    let spec = text.strip_prefix("bytes ")?;
    let (range, total) = spec.split_once('/')?;
    if range == "*" {
        return (digits(total.as_bytes())? == u128::from(size)).then_some(UploadRange::Status);
    }
    let (first, last) = range.split_once('-')?;
    let (first, last, total) = (
        digits(first.as_bytes())?,
        digits(last.as_bytes())?,
        digits(total.as_bytes())?,
    );
    if total != u128::from(size) || first > last || last >= total {
        return None;
    }
    Some(UploadRange::Part(UploadPart {
        offset: u64::try_from(first).ok()?,
        length: u64::try_from(last - first + 1).ok()?,
    }))
}

/// What one input was read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// A part of the file.
    Part,
    /// A status query.
    Status,
    /// Refused.
    Refused,
}

/// Checks one input; answers what the header was read as.
///
/// # Panics
///
/// Panics when an invariant above does not hold.
pub fn check(data: &[u8]) -> Read {
    if data.len() > MAX_INPUT_BYTES {
        return Read::Refused;
    }
    let mut size_bytes = [0u8; 8];
    let head = data.len().min(8);
    size_bytes[..head].copy_from_slice(&data[..head]);
    let size = u64::from_be_bytes(size_bytes);
    let header = &data[head..];
    let got = parse_upload_range(header, size);
    assert_eq!(
        got,
        expected(header, size),
        "header {header:?}, size {size}"
    );
    match got {
        None => Read::Refused,
        Some(UploadRange::Status) => {
            assert_eq!(
                UploadRange::status_header(size).as_bytes(),
                header,
                "a second spelling of a status query"
            );
            Read::Status
        }
        Some(UploadRange::Part(part)) => {
            assert!(part.length >= 1, "an empty part");
            assert!(part.offset < size, "a part starting at or past the end");
            assert!(part.end() <= size, "a part ending past the end");
            assert_eq!(
                part.content_range(size).as_bytes(),
                header,
                "a second spelling of a part"
            );
            Read::Part
        }
    }
}
