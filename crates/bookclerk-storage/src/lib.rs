//! Storage backends for acquired audiobooks, covers, and PDFs.
//!
//! # Audience
//!
//! Host acquire / library code that writes through [`StorageBackend`]. Guest
//! plugins do not depend on this crate.
//!
//! Product narrative: destination sections in `docs/server/configuration.md`. Style:
//! `docs/contributing/code-documentation.md`.

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

/// Bounded scalar reads, ranges, and digest helpers.
mod bounded;
/// Error types returned by destination storage backends.
mod error;
mod fanout;
pub mod fs_kind;
/// Serving a backend's intent operations from a storage guest.
pub mod guest;
/// Intent operations (moves, batches, conditional writes, change feeds) and
/// their fallbacks.
pub mod intent;
pub mod key_syntax;
/// On-disk local list index (external sort, O(page) reads).
mod list_index;
mod local;
#[cfg(feature = "watch")]
mod local_watch;
mod placement;
pub mod r2_events;
#[cfg(feature = "s3")]
mod r2_queue;
#[cfg(feature = "s3")]
mod s3;
pub mod s3_events;
#[cfg(feature = "s3")]
pub mod s3_guest;
#[cfg(all(feature = "s3", any(unix, windows)))]
mod s3_http;
#[cfg(feature = "s3")]
mod s3_queue;
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
pub use intent::{
    ChangeFeedPage, ConditionalPutResult, HintedListPage, KeyResult, ListRequest, MoveOptions,
    MovePrefixPage, MoveResult, ObjectChange, ObjectChangeKind, StorageCapabilities,
    WriteCondition,
};
pub use local::LocalFsBackend;
pub use placement::node_placement_id;
#[cfg(feature = "s3")]
pub use r2_queue::{CloudflareAccountId, CloudflareApiToken, CloudflareQueueId};
#[cfg(feature = "s3")]
pub use s3::{S3Backend, S3Credentials, S3Settings};
#[cfg(feature = "s3")]
pub use s3_queue::QueueUrl;
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
