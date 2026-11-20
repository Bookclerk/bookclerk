//! One part of an uploaded file: `PUT /api/node/v1/uploads/{id}/files/{n}`
//! carrying `Content-Range: bytes <first>-<last>/<size>`.
//!
//! A relay in front of a library (an ingress plugin's Cloudflare Worker,
//! for one) caps request bodies, so a host sends a file larger than
//! [`UPLOAD_PART_BYTES`] as consecutive parts, each its own request, in
//! order. A file that fits is sent whole, without the header. The owner
//! streams the parts of one file into a single write per destination, so a
//! file is still never staged (see `docs/adr/ingress-plugins.md`).
//!
//! An interrupted file resumes: the host asks where the owner is with an
//! empty `PUT` carrying `Content-Range: bytes */<size>`
//! ([`UploadRange::Status`]), and sends the rest from the offset the owner
//! answers. Parts need not fall on [`UPLOAD_PART_BYTES`] boundaries.
//!
//! The header comes from the holder of an upload token, so
//! [`parse_upload_range`] accepts exactly one spelling of each form and
//! refuses everything else.

/// Largest part a host sends (64 MiB): under the request-body caps of the
/// relays an install can sit behind (100 MB on Cloudflare's Free plan).
pub const UPLOAD_PART_BYTES: u64 = 64 << 20;

/// Most decimal digits in one number of the header; 19 digits always fit
/// a `u64`.
const MAX_DIGITS: usize = 19;

/// A part of a declared file: at least one byte, inside the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadPart {
    /// Offset of the part's first byte in the file.
    pub offset: u64,
    /// Bytes in the part; at least 1.
    pub length: u64,
}

impl UploadPart {
    /// Offset just past the part's last byte.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.length)
    }

    /// Whether the part ends the file of `size` bytes.
    #[must_use]
    pub fn is_last(&self, size: u64) -> bool {
        self.end() == size
    }

    /// The `Content-Range` value naming this part of a file of `size`
    /// bytes, as [`parse_upload_range`] reads it.
    #[must_use]
    pub fn content_range(&self, size: u64) -> String {
        format!(
            "bytes {}-{}/{size}",
            self.offset,
            self.end().saturating_sub(1)
        )
    }
}

/// The parts a host sends a file of `size` bytes in, each at most
/// `part_bytes` (at least 1) long, in order; empty when the file fits in
/// one request (`size <= part_bytes`), which is then sent whole.
#[must_use]
pub fn upload_parts(size: u64, part_bytes: u64) -> Vec<UploadPart> {
    let part_bytes = part_bytes.max(1);
    if size <= part_bytes {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut offset = 0u64;
    while offset < size {
        let length = part_bytes.min(size - offset);
        parts.push(UploadPart { offset, length });
        offset += length;
    }
    parts
}

/// What a `Content-Range` on an upload `PUT` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadRange {
    /// The body is this part of the file.
    Part(UploadPart),
    /// No body: answer with the offset the owner needs next
    /// (`bytes */<size>`).
    Status,
}

impl UploadRange {
    /// The status query's `Content-Range` value for a file of `size`
    /// bytes.
    #[must_use]
    pub fn status_header(size: u64) -> String {
        format!("bytes */{size}")
    }
}

/// One number of the header: 1 to [`MAX_DIGITS`] ASCII digits with no
/// leading zero (except `0` itself).
fn number(text: &[u8]) -> Option<u64> {
    if text.is_empty() || text.len() > MAX_DIGITS || (text.len() > 1 && text[0] == b'0') {
        return None;
    }
    text.iter().try_fold(0u64, |sum, byte| {
        byte.is_ascii_digit()
            .then(|| sum * 10 + u64::from(byte - b'0'))
    })
}

/// Reads a `Content-Range` header value on an upload `PUT` of a declared
/// file of `size` bytes.
///
/// # Arguments
///
/// * `header` - the header value as sent.
/// * `size` - the file's declared size from the upload's manifest.
///
/// # Returns
///
/// [`UploadRange::Part`] for exactly `bytes <first>-<last>/<size>`, with
/// canonical decimal numbers, `first <= last < size` and the declared size;
/// [`UploadRange::Status`] for exactly `bytes */<size>` with the declared
/// size; `None` for anything else, including a part of an empty file
/// (which is always sent whole).
#[must_use]
pub fn parse_upload_range(header: &[u8], size: u64) -> Option<UploadRange> {
    let spec = header.strip_prefix(b"bytes ")?;
    if let Some(total) = spec.strip_prefix(b"*/") {
        return (number(total)? == size).then_some(UploadRange::Status);
    }
    let dash = spec.iter().position(|byte| *byte == b'-')?;
    let slash = spec.iter().position(|byte| *byte == b'/')?;
    if slash < dash {
        return None;
    }
    let first = number(&spec[..dash])?;
    let last = number(&spec[dash + 1..slash])?;
    let total = number(&spec[slash + 1..])?;
    if total != size || first > last || last >= size {
        return None;
    }
    Some(UploadRange::Part(UploadPart {
        offset: first,
        length: last - first + 1,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_cover_the_file_in_order() {
        assert!(upload_parts(10, 10).is_empty());
        assert!(upload_parts(0, 10).is_empty());
        let parts = upload_parts(25, 10);
        assert_eq!(
            parts,
            vec![
                UploadPart {
                    offset: 0,
                    length: 10
                },
                UploadPart {
                    offset: 10,
                    length: 10
                },
                UploadPart {
                    offset: 20,
                    length: 5
                },
            ]
        );
        assert!(parts[2].is_last(25));
        assert!(!parts[1].is_last(25));
        for part in parts {
            assert_eq!(
                parse_upload_range(part.content_range(25).as_bytes(), 25),
                Some(UploadRange::Part(part))
            );
        }
    }

    #[test]
    fn only_the_canonical_spelling_is_a_part() {
        assert_eq!(
            parse_upload_range(b"bytes 0-9/25", 25),
            Some(UploadRange::Part(UploadPart {
                offset: 0,
                length: 10
            }))
        );
        assert_eq!(
            parse_upload_range(b"bytes */25", 25),
            Some(UploadRange::Status)
        );
        assert_eq!(UploadRange::status_header(25), "bytes */25");
        for refused in [
            &b"bytes 0-9/26"[..],
            b"bytes 0-25/25",
            b"bytes 9-0/25",
            b"bytes 00-9/25",
            b"bytes 0-9/025",
            b"bytes=0-9/25",
            b"Bytes 0-9/25",
            b"bytes  0-9/25",
            b"bytes 0-9",
            b"bytes 0/9-25",
            b"bytes 0-9/*",
            b"bytes */26",
            b"bytes */025",
            b"bytes * /25",
            b"bytes -9/25",
            b"bytes 0-+9/25",
            b"bytes 0-0/0",
            b"",
        ] {
            assert_eq!(parse_upload_range(refused, 25), None, "{refused:?}");
        }
        assert_eq!(parse_upload_range(b"bytes 0-0/0", 0), None);
        assert_eq!(
            parse_upload_range(b"bytes */0", 0),
            Some(UploadRange::Status)
        );
    }
}
