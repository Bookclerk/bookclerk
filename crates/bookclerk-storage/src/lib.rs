//! Storage backends for acquired audiobooks, covers, and PDFs.
//!
//! # Audience
//!
//! Host acquire / library code that writes through [`StorageBackend`]. Guest
//! plugins do not depend on this crate.
//!
//! Product narrative: destination sections in `docs/configuration.md`. Style:
//! `docs/code-documentation.md`.

/// Bounded scalar reads, ranges, and digest helpers.
mod bounded;
/// Error types returned by destination storage backends.
mod error;
mod fanout;
/// On-disk local list index (external sort, O(page) reads).
mod list_index;
mod local;
mod placement;
mod s3;
#[cfg(any(unix, windows))]
mod s3_http;
/// [`StorageBackend`] trait plus object metadata and audio-key helpers.
mod traits;
/// Source-to-destination copy that stays within stream windows.
mod transfer;

pub use bounded::{
    ensure_scalar_len, normalize_range, parse_sha256_hex, read_scalar_body, reject_scalar_hint,
    sha256_field_from_raw, MAX_LIST_PAGE, MAX_SCALAR_OBJECT_BYTES, MAX_SUPPORTED_OBJECT_BYTES,
};
pub use error::{Result, StorageError};
pub use fanout::FanoutBackend;
pub use local::LocalFsBackend;
pub use placement::node_placement_id;
pub use s3::{S3Backend, S3Credentials, S3Settings};
pub use traits::{
    bookclerk_meta_sidecar_key, is_audio_key, ByteRange, ListPage, ObjectInfo, ObjectMeta,
    ObjectProbe, PutStreamResult, StorageBackend, AUDIO_EXTENSIONS,
};
pub use transfer::{transfer_object, TransferOptions, TransferOutcome};

use bookclerk_config::normalize_storage_prefix;

/// Trims and normalizes a destination key prefix (same rules as config).
pub(crate) fn normalize_prefix(prefix: &str) -> String {
    normalize_storage_prefix(prefix)
}
