//! Shared oracle for the `upload_part` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The `Content-Range` header of an upload part (`PUT
//! /api/node/v1/uploads/{id}/files/{n}`) comes from whoever holds an
//! upload token, and [`parse_upload_part`] decides from it which bytes of
//! a declared file the request body is. The input is the file's declared
//! size (the first eight bytes, big endian; missing bytes are zero)
//! followed by the header as sent.
//!
//! Whatever the header is:
//!
//! - a part lies inside the file: at least one byte, starting before the
//!   end and ending at or before it, so the owner never writes past the
//!   declared size;
//! - a part is given only to a header that names it back exactly
//!   ([`UploadPart::content_range`] gives the header as sent), so there is
//!   one spelling of each part;
//! - the answer equals the one worked out here by a second reading of the
//!   header (digits folded into a wide integer rather than parsed), so the
//!   parser and the oracle would have to be wrong the same way.

use bookclerk_node_proto::{parse_upload_part, UploadPart};

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

/// The part the header should be read as, read a second way.
fn expected(header: &[u8], size: u64) -> Option<UploadPart> {
    let text = std::str::from_utf8(header).ok()?;
    let spec = text.strip_prefix("bytes ")?;
    let (range, total) = spec.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let (first, last, total) = (
        digits(first.as_bytes())?,
        digits(last.as_bytes())?,
        digits(total.as_bytes())?,
    );
    if total != u128::from(size) || first > last || last >= total {
        return None;
    }
    Some(UploadPart {
        offset: u64::try_from(first).ok()?,
        length: u64::try_from(last - first + 1).ok()?,
    })
}

/// Checks one input; answers whether the header was read as a part.
///
/// # Panics
///
/// Panics when an invariant above does not hold.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let mut size_bytes = [0u8; 8];
    let head = data.len().min(8);
    size_bytes[..head].copy_from_slice(&data[..head]);
    let size = u64::from_be_bytes(size_bytes);
    let header = &data[head..];
    let got = parse_upload_part(header, size);
    assert_eq!(
        got,
        expected(header, size),
        "header {header:?}, size {size}"
    );
    let Some(part) = got else {
        return false;
    };
    assert!(part.length >= 1, "an empty part");
    assert!(part.offset < size, "a part starting at or past the end");
    assert!(part.end() <= size, "a part ending past the end");
    assert_eq!(
        part.content_range(size).as_bytes(),
        header,
        "a second spelling of a part"
    );
    true
}
