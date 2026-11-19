//! Reads the metadata embedded in audiobook files without reading the audio.
//!
//! [`TagReader`] is a sans-IO state machine: it never opens a file. It asks
//! for byte ranges ([`Poll::Need`]), the caller supplies them
//! ([`TagReader::supply`]), and once it has the bytes that hold the tags it
//! answers with a [`TagRecord`]: text entries, chapters, the running time,
//! where the cover picture lies, and a digest of the metadata region. The
//! same code serves a local file, an S3 object read by ranged `GET`, the
//! fuzzer and the unit tests, and only the metadata region (plus a few box
//! headers) is ever fetched.
//!
//! Files on storage are untrusted. The reader runs inside
//! `bookclerk-media-worker tags`, a long-lived worker jailed to no path and
//! no network, which speaks [`wire`] frames with the host. The host checks
//! every record it gets back with [`TagRecord::validate`].
//!
//! Supported containers: MP4 (`ilst` items, QuickTime chapter tracks and
//! Nero `chpl`), MP3 (ID3v2 with `CHAP`, ID3v1, Xing/Info/VBRI frame counts)
//! and FLAC (Vorbis comments, `PICTURE`, `STREAMINFO`).

mod cache;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
mod flac;
mod id3v2;
mod mp4;
mod mpeg;
mod reader;
mod record;
mod text;
pub mod wire;

use serde::{Deserialize, Serialize};

pub use reader::{read_all, Poll, TagReader};
pub use record::{
    AudioCodec, Chapter, ChapterSource, Container, Duration, DurationPrecision, EmbeddedCover,
    PictureFormat, RecordError, RegionDigest, Sha256Digest, TagEntry, TagField, TagRecord,
    MAX_DURATION_MS,
};

/// Version of the region digest this reader computes ([`RegionDigest::version`]).
pub const REGION_HASH_VERSION: u16 = 1;

/// Most text entries a record carries; the rest are dropped and the record
/// marked truncated.
pub const MAX_ENTRIES: usize = 512;

/// Longest value an entry carries, in bytes; longer values are cut at a
/// character boundary and the record marked truncated.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

/// Longest key (freeform name, namespace or native id), in bytes; an entry
/// with a longer key is dropped.
pub const MAX_KEY_BYTES: usize = 256;

/// Most chapters a record carries.
pub const MAX_CHAPTERS: usize = 4096;

/// Longest chapter title, in bytes.
pub const MAX_CHAPTER_TITLE_BYTES: usize = 1024;

/// Most bytes the reader asks for over one file. Real metadata, cover
/// included, is a few megabytes at most.
pub const MAX_FETCH_BYTES: u64 = 16 * 1024 * 1024;

/// Most rounds of [`Poll::Need`] for one file.
pub const MAX_ROUNDS: u32 = 128;

/// Most spans one [`Poll::Need`] asks for.
pub const MAX_SPANS_PER_ROUND: usize = 64;

/// Least the reader asks for at once when it only needs a header, so a walk
/// over small boxes takes few rounds.
pub const MIN_FETCH_BYTES: u64 = 64 * 1024;

/// A range of bytes in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteSpan {
    /// Offset of the first byte.
    pub offset: u64,
    /// Number of bytes.
    pub len: u64,
}

impl ByteSpan {
    /// The offset one past the last byte, or `None` when that overflows.
    #[must_use]
    pub fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.len)
    }
}

/// Why a file's tags could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    /// The file is not a container the reader understands.
    #[error("not an MP4, MP3 or FLAC file")]
    Unsupported,
    /// The container is malformed: a structure runs past its parent or the
    /// file, or a required box is missing.
    #[error("malformed {container}: {detail}")]
    Malformed {
        /// The container being read.
        container: &'static str,
        /// What was wrong.
        detail: &'static str,
    },
    /// The metadata is larger than [`MAX_FETCH_BYTES`] or needs more than
    /// [`MAX_ROUNDS`] rounds.
    #[error("metadata is too large to read")]
    TooLarge,
    /// The caller supplied bytes the reader did not ask for, or the wrong
    /// number of them.
    #[error("supplied bytes do not match the request")]
    Unrequested,
    /// The ID3v2 tag could not be parsed.
    #[error("bad ID3v2 tag: {0}")]
    Id3(String),
}
