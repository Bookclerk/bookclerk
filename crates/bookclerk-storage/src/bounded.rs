//! Small-object and stream bounds shared by every storage backend.
//!
//! The numbers are the plugin ABI maxima ([`bookclerk_plugin_sdk::MAX_SCALAR_BYTES`],
//! [`bookclerk_plugin_sdk::MAX_LIST_PAGE`]). Negotiated sessions may use a lower
//! scalar cap; they must not raise these. HEAD / `content-length` is an early
//! rejection hint. Callers still count bytes actually read and treat `limit + 1`
//! as [`StorageError::PayloadTooLarge`] rather than a truncated success.

#![allow(clippy::missing_docs_in_private_items)]

use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::{Result, StorageError};
use crate::traits::ByteRange;

/// ABI scalar ceiling. Audiobook bodies must not use [`crate::StorageBackend::get`] /
/// [`crate::StorageBackend::put`].
pub const MAX_SCALAR_OBJECT_BYTES: u64 = bookclerk_plugin_sdk::MAX_SCALAR_BYTES as u64;

/// ABI list page ceiling.
pub const MAX_LIST_PAGE: u32 = bookclerk_plugin_sdk::MAX_LIST_PAGE;

/// Largest object this process will upload or server-copy.
///
/// S3 allows 10,000 parts. Parts are [`crate::s3::MULTIPART_PART_SIZE`] (8 MiB),
/// so the application ceiling is 80 GiB. AWS's own 5 TiB object maximum is wider;
/// requests above this ceiling fail before any part buffer is filled.
pub const MAX_SUPPORTED_OBJECT_BYTES: u64 =
    crate::s3::MULTIPART_PART_SIZE as u64 * crate::s3::S3_MAX_PARTS as u64;

/// AWS `CopyObject` inclusive ceiling. Larger copies use `UploadPartCopy`.
pub const COPY_OBJECT_MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// How a ranged read is applied after rejecting ambiguous internal lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadSpan {
    /// Bytes from `offset` through EOF.
    ToEnd {
        /// First byte to return.
        offset: u64,
    },
    /// Exactly `length` bytes starting at `offset`.
    Exact {
        /// First byte to return.
        offset: u64,
        /// Number of bytes to return. Always non-zero.
        length: u64,
    },
}

/// Normalizes an internal range.
///
/// `None` reads the whole object. [`ByteRange::length`] `None` reads through
/// EOF (the Cap'n Proto wire uses integer `0` for that case and decodes it to
/// `None` before it reaches a backend). `Some(0)` is rejected so local and S3
/// cannot disagree about an empty window versus "to end".
///
/// `object_size`, when known, is the whole object. Offsets past that size, a
/// window that would extend past it, and `offset + length` overflow are errors.
///
/// # Errors
///
/// Returns [`StorageError::InvalidKey`] for an ambiguous, overflowing, or
/// unsatisfiable range.
pub fn normalize_range(
    range: Option<ByteRange>,
    object_size: Option<u64>,
) -> Result<Option<ReadSpan>> {
    let Some(range) = range else {
        return Ok(None);
    };
    let span = match range.length {
        Some(0) => {
            return Err(StorageError::InvalidKey(
                "byte range length 0 is ambiguous; omit length to read through EOF".into(),
            ));
        }
        None => ReadSpan::ToEnd {
            offset: range.offset,
        },
        Some(length) => {
            let end = range.offset.checked_add(length).ok_or_else(|| {
                StorageError::InvalidKey("byte range offset + length overflows u64".into())
            })?;
            if let Some(size) = object_size {
                if end > size {
                    return Err(StorageError::InvalidKey(format!(
                        "byte range {end} extends past object size {size}"
                    )));
                }
            }
            ReadSpan::Exact {
                offset: range.offset,
                length,
            }
        }
    };
    if let Some(size) = object_size {
        let offset = match span {
            ReadSpan::ToEnd { offset } | ReadSpan::Exact { offset, .. } => offset,
        };
        if offset > size {
            return Err(StorageError::InvalidKey(format!(
                "byte range offset {offset} is past object size {size}"
            )));
        }
    }
    Ok(Some(span))
}

/// Rejects a scalar put whose buffer already exceeds `limit`.
///
/// # Errors
///
/// Returns [`StorageError::PayloadTooLarge`] when `len` is above `limit`.
pub fn ensure_scalar_len(len: usize, limit: u64) -> Result<()> {
    if len as u64 > limit {
        return Err(StorageError::PayloadTooLarge(format!(
            "scalar object of {len} bytes exceeds {limit} (use get_stream/put_stream)"
        )));
    }
    Ok(())
}

/// Early-reject when a HEAD/stat hint already exceeds `limit`.
///
/// This does not allocate `hint` bytes. The body must still be counted.
///
/// # Errors
///
/// Returns [`StorageError::PayloadTooLarge`] when `hint` is above `limit`.
pub fn reject_scalar_hint(hint: u64, limit: u64) -> Result<()> {
    if hint > limit {
        return Err(StorageError::PayloadTooLarge(format!(
            "scalar object hint of {hint} bytes exceeds {limit} (use get_stream)"
        )));
    }
    Ok(())
}

/// Reads at most `limit + 1` bytes.
///
/// A body longer than `limit` returns [`StorageError::PayloadTooLarge`] and
/// stops. The allocation is capped at `limit + 1` regardless of any advertised
/// content length.
///
/// # Errors
///
/// Returns [`StorageError::PayloadTooLarge`] or [`StorageError::Io`].
pub async fn read_scalar_body(
    mut reader: Pin<Box<dyn AsyncRead + Send>>,
    limit: u64,
) -> Result<bytes::Bytes> {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX.saturating_sub(1));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8 * 1024];
    loop {
        if buf.len() > limit {
            return Err(StorageError::PayloadTooLarge(format!(
                "scalar body exceeded {limit} bytes"
            )));
        }
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        let room = (limit + 1).saturating_sub(buf.len());
        if n > room {
            buf.extend_from_slice(&tmp[..room]);
            return Err(StorageError::PayloadTooLarge(format!(
                "scalar body exceeded {limit} bytes"
            )));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    if buf.len() > limit {
        return Err(StorageError::PayloadTooLarge(format!(
            "scalar body exceeded {limit} bytes"
        )));
    }
    Ok(bytes::Bytes::from(buf))
}

/// Validates a raw SHA-256 field (ABI `Data`, 32 bytes) into hex.
///
/// Empty means "not provided". Any other length is an integrity error.
///
/// # Errors
///
/// Returns [`StorageError::Integrity`] when the digest is not 32 bytes.
pub fn sha256_field_from_raw(bytes: Option<&[u8]>) -> Result<Option<String>> {
    match bytes {
        None | Some([]) => Ok(None),
        Some(raw) if raw.len() == 32 => Ok(Some(hex::encode(raw))),
        Some(raw) => Err(StorageError::Integrity(format!(
            "sha256 must be 32 bytes, got {}",
            raw.len()
        ))),
    }
}

/// Parses a hex SHA-256 (64 hex chars) into raw bytes.
///
/// # Errors
///
/// Returns [`StorageError::Integrity`] when the shape is wrong.
pub fn parse_sha256_hex(value: &str) -> Result<[u8; 32]> {
    let raw = hex::decode(value.trim())
        .map_err(|_| StorageError::Integrity(format!("sha256 is not hex: {value}")))?;
    if raw.len() != 32 {
        return Err(StorageError::Integrity(format!(
            "sha256 must be 32 bytes, got {}",
            raw.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&raw);
    Ok(out)
}

/// Deletes `path` on drop unless [`TempGuard::disarm`] ran.
pub struct TempGuard {
    path: std::path::PathBuf,
    armed: bool,
}

impl TempGuard {
    /// Arms deletion of `path`.
    pub fn arm(path: std::path::PathBuf) -> Self {
        Self { path, armed: true }
    }

    /// Leaves the file in place (after a successful rename away from `path`).
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Clamps a requested page size into `1..=MAX_LIST_PAGE`. `0` selects the max.
#[must_use]
pub fn clamp_page_limit(limit: u32) -> usize {
    let requested = if limit == 0 { MAX_LIST_PAGE } else { limit };
    requested.clamp(1, MAX_LIST_PAGE) as usize
}

/// True when a single `CopyObject` is within the AWS 5 GiB ceiling.
#[must_use]
pub fn copy_uses_single_request(size: u64) -> bool {
    size <= COPY_OBJECT_MAX_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn scalar_read_accepts_limit_and_rejects_limit_plus_one() {
        let limit = 32u64;
        let ok = read_scalar_body(Box::pin(Cursor::new(vec![1u8; 32])), limit)
            .await
            .unwrap();
        assert_eq!(ok.len(), 32);
        let err = read_scalar_body(Box::pin(Cursor::new(vec![1u8; 33])), limit)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::PayloadTooLarge(_)));
    }

    #[test]
    fn hint_does_not_require_allocating_the_advertised_size() {
        let err = reject_scalar_hint(u64::MAX, MAX_SCALAR_OBJECT_BYTES).unwrap_err();
        assert!(matches!(err, StorageError::PayloadTooLarge(_)));
        assert!(reject_scalar_hint(MAX_SCALAR_OBJECT_BYTES, MAX_SCALAR_OBJECT_BYTES).is_ok());
    }

    #[test]
    fn zero_length_range_is_rejected_and_wire_none_means_to_end() {
        let err = normalize_range(
            Some(ByteRange {
                offset: 0,
                length: Some(0),
            }),
            Some(10),
        )
        .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
        assert_eq!(
            normalize_range(
                Some(ByteRange {
                    offset: 4,
                    length: None,
                }),
                Some(10),
            )
            .unwrap(),
            Some(ReadSpan::ToEnd { offset: 4 })
        );
        assert!(normalize_range(None, Some(0)).unwrap().is_none());
    }

    #[test]
    fn range_overflow_and_past_end_fail() {
        let err = normalize_range(
            Some(ByteRange {
                offset: u64::MAX,
                length: Some(1),
            }),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
        let err = normalize_range(
            Some(ByteRange {
                offset: 8,
                length: Some(4),
            }),
            Some(10),
        )
        .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
        let err = normalize_range(
            Some(ByteRange {
                offset: 11,
                length: None,
            }),
            Some(10),
        )
        .unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[test]
    fn copy_boundary_is_five_gib() {
        assert!(copy_uses_single_request(COPY_OBJECT_MAX_BYTES));
        assert!(!copy_uses_single_request(COPY_OBJECT_MAX_BYTES + 1));
        assert!(copy_uses_single_request(0));
    }

    #[test]
    fn sha256_shape_is_enforced() {
        assert!(sha256_field_from_raw(None).unwrap().is_none());
        assert!(sha256_field_from_raw(Some(&[])).unwrap().is_none());
        assert!(sha256_field_from_raw(Some(&[1, 2, 3])).is_err());
        let raw = [7u8; 32];
        let hex = sha256_field_from_raw(Some(&raw)).unwrap().unwrap();
        assert_eq!(parse_sha256_hex(&hex).unwrap(), raw);
    }
}
