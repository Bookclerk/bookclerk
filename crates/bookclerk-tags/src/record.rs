//! What [`crate::TagReader`] reports for one file, and the limits a record
//! from an untrusted worker is checked against.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    ByteSpan, MAX_CHAPTERS, MAX_CHAPTER_TITLE_BYTES, MAX_ENTRIES, MAX_KEY_BYTES, MAX_VALUE_BYTES,
    REGION_HASH_VERSION,
};

/// Longest running time a record may claim: 10,000 hours, in milliseconds.
///
/// Far beyond any real audiobook; a larger value means a corrupt header.
pub const MAX_DURATION_MS: u64 = 10_000 * 3_600 * 1_000;

/// The metadata one file carries in its own container.
///
/// Values are reported as the file holds them, without normalisation: the
/// resolver decides what they mean. Entries keep the file's order, and a
/// field may appear more than once (several artists, several genres).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagRecord {
    /// The container the file was read as.
    pub container: Container,
    /// Text values, in file order; at most [`MAX_ENTRIES`].
    pub entries: Vec<TagEntry>,
    /// Chapters, by start time; at most [`MAX_CHAPTERS`].
    pub chapters: Vec<Chapter>,
    /// Where [`Self::chapters`] came from; `None` exactly when there are none.
    pub chapter_source: Option<ChapterSource>,
    /// The running time, when the container states or implies one.
    pub duration: Option<Duration>,
    /// How the audio is encoded, when the container names an encoding the
    /// reader knows; `None` for an MP4 file with no audio track or with one
    /// in another encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<AudioCodec>,
    /// The embedded cover picture, when there is one.
    pub cover: Option<EmbeddedCover>,
    /// Digest of the bytes that hold the metadata.
    pub region: RegionDigest,
    /// True when entries, chapters or values were cut to fit the limits, so
    /// the record is not everything the file holds.
    pub truncated: bool,
}

/// The container formats the reader understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    /// ISO base media (`.m4b`, `.m4a`, `.mp4`): `ilst` items, Nero `chpl`
    /// and QuickTime chapter tracks.
    Mp4,
    /// MPEG audio (`.mp3`): an ID3v2 tag at the start, or failing that an
    /// ID3v1 tag at the end.
    Mp3,
    /// FLAC: Vorbis comments, pictures and `STREAMINFO`.
    Flac,
}

/// The audio encodings the reader tells apart.
///
/// The encoding decides whether a file is a lossless copy of its source,
/// which is what lets a library keep the better of two copies of one
/// edition (library data model ADR, section 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    /// MPEG-4 AAC (`mp4a` sample entry), lossy.
    Aac,
    /// Apple Lossless (`alac` sample entry).
    Alac,
    /// FLAC, in its own container or in MP4 (`fLaC` sample entry).
    Flac,
    /// MPEG audio layer I, II or III, lossy.
    Mp3,
    /// Opus in MP4 (`Opus` sample entry), lossy.
    Opus,
    /// Dolby Digital Plus (`ec-3` sample entry), lossy.
    Eac3,
    /// Dolby AC-4 (`ac-4` sample entry), lossy.
    Ac4,
}

impl AudioCodec {
    /// True when decoding gives back the source audio bit for bit.
    #[must_use]
    pub fn is_lossless(self) -> bool {
        match self {
            Self::Alac | Self::Flac => true,
            Self::Aac | Self::Mp3 | Self::Opus | Self::Eac3 | Self::Ac4 => false,
        }
    }

    /// The codec an MP4 audio sample entry of type `format` holds, when the
    /// reader knows it.
    #[must_use]
    pub fn from_sample_entry(format: [u8; 4]) -> Option<Self> {
        match &format {
            b"mp4a" => Some(Self::Aac),
            b"alac" => Some(Self::Alac),
            b"fLaC" => Some(Self::Flac),
            b".mp3" => Some(Self::Mp3),
            b"Opus" => Some(Self::Opus),
            b"ec-3" => Some(Self::Eac3),
            b"ac-4" => Some(Self::Ac4),
            _ => None,
        }
    }
}

impl Container {
    /// The media type files of this container are stored and served as.
    #[must_use]
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Mp4 => "audio/mp4",
            Self::Mp3 => "audio/mpeg",
            Self::Flac => "audio/flac",
        }
    }
}

/// One text value from the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagEntry {
    /// Which field the value belongs to.
    pub field: TagField,
    /// The value; at most [`MAX_VALUE_BYTES`] bytes, never containing NUL.
    pub value: String,
}

/// The field a [`TagEntry`] fills.
///
/// The named variants are the fields every supported container has a
/// standard slot for. Everything else keeps the container's own key, so no
/// value is dropped for want of a name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TagField {
    /// Track or book title (`©nam`, `TIT2`, `TITLE`).
    Title,
    /// Subtitle (`TIT3`, `SUBTITLE`).
    Subtitle,
    /// Album, which audiobook tools use for the book title (`©alb`, `TALB`, `ALBUM`).
    Album,
    /// Artist, usually the author (`©ART`, `TPE1`, `ARTIST`).
    Artist,
    /// Album artist (`aART`, `TPE2`, `ALBUMARTIST`).
    AlbumArtist,
    /// Composer, which audiobook tools use for the narrator (`©wrt`, `TCOM`, `COMPOSER`).
    Composer,
    /// Genre (`©gen`, `TCON`, `GENRE`).
    Genre,
    /// Release date or year, as written (`©day`, `TDRC`/`TYER`, `DATE`).
    Date,
    /// Comment (`©cmt`, `COMM`, `COMMENT`).
    Comment,
    /// Short description (`desc`, `DESCRIPTION`).
    Description,
    /// Long description (`ldes`).
    LongDescription,
    /// Publisher (`©pub`, `TPUB`, `PUBLISHER`, `ORGANIZATION`).
    Publisher,
    /// Copyright (`cprt`, `TCOP`, `COPYRIGHT`).
    Copyright,
    /// Language (`TLAN`, `LANGUAGE`).
    Language,
    /// Track number, as `n` or `n/total` (`trkn`, `TRCK`, `TRACKNUMBER`).
    Track,
    /// Disc number, as `n` or `n/total` (`disk`, `TPOS`, `DISCNUMBER`).
    Disc,
    /// Grouping (`©grp`, `GRP1`/`TIT1`, `GROUPING`).
    Grouping,
    /// A user-defined value: an MP4 `----` item, an ID3 `TXXX` frame, or a
    /// Vorbis comment key with no named field.
    Freeform {
        /// The MP4 `mean` (such as `com.apple.iTunes` or
        /// `com.pilabor.tone`); `None` for ID3 and Vorbis.
        namespace: Option<String>,
        /// The item name, `TXXX` description or comment key.
        name: String,
    },
    /// A standard container field with no named variant, by its native key
    /// (an MP4 atom such as `©too`, an ID3 frame id such as `TENC`).
    Native {
        /// The atom or frame id.
        id: String,
    },
}

impl TagField {
    /// The keys this field carries, for the length checks.
    fn keys(&self) -> impl Iterator<Item = &str> {
        let (first, second) = match self {
            Self::Freeform { namespace, name } => (namespace.as_deref(), Some(name.as_str())),
            Self::Native { id } => (Some(id.as_str()), None),
            Self::Title
            | Self::Subtitle
            | Self::Album
            | Self::Artist
            | Self::AlbumArtist
            | Self::Composer
            | Self::Genre
            | Self::Date
            | Self::Comment
            | Self::Description
            | Self::LongDescription
            | Self::Publisher
            | Self::Copyright
            | Self::Language
            | Self::Track
            | Self::Disc
            | Self::Grouping => (None, None),
        };
        first.into_iter().chain(second)
    }
}

/// One chapter mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chapter {
    /// Start, in milliseconds from the beginning of the file.
    pub start_ms: u64,
    /// Title; at most [`MAX_CHAPTER_TITLE_BYTES`] bytes, never containing NUL.
    pub title: String,
}

/// Which structure a file's chapters were read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChapterSource {
    /// A QuickTime chapter text track (what Apple players and Bookclerk use).
    Mp4Track,
    /// A Nero `chpl` box, read when there is no chapter track.
    Mp4Nero,
    /// ID3v2 `CHAP` frames.
    Id3Chap,
    /// Vorbis `CHAPTERnnn` / `CHAPTERnnnNAME` comments.
    VorbisComment,
}

/// A running time and how it was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Duration {
    /// Milliseconds; at most [`MAX_DURATION_MS`].
    pub ms: u64,
    /// Whether the container states it or it was estimated.
    pub precision: DurationPrecision,
}

/// How a [`Duration`] was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurationPrecision {
    /// Stated by the container (MP4 `mvhd`, FLAC `STREAMINFO`, an MP3
    /// Xing/Info/VBRI frame count), or every MP3 frame was counted.
    Exact,
    /// Worked out from the MP3 frames in the first 64 KiB and the audio
    /// size: right for constant bit rate files, close for variable ones.
    Estimated,
}

/// Where the embedded cover is and what it is, without its bytes.
///
/// The host fetches [`Self::span`] itself and hands it to the cover worker,
/// so a page of tag replies never carries pictures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedCover {
    /// The picture format, sniffed from its first bytes (not the declared
    /// MIME type).
    pub format: PictureFormat,
    /// Picture size in bytes.
    pub len: u64,
    /// SHA-256 of the picture bytes.
    pub sha256: Sha256Digest,
    /// Where the picture bytes lie in the file, when they are stored as is
    /// (not inside an unsynchronised ID3 tag).
    pub span: Option<ByteSpan>,
}

/// The format of an embedded picture, from its signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PictureFormat {
    /// JPEG (`FF D8 FF`).
    Jpeg,
    /// PNG.
    Png,
    /// WebP (`RIFF....WEBP`).
    Webp,
    /// GIF.
    Gif,
    /// BMP (`BM`).
    Bmp,
    /// None of the above.
    Unknown,
}

impl PictureFormat {
    /// The format whose signature `bytes` starts with.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Self {
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Self::Jpeg
        } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Self::Png
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Self::Webp
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Self::Gif
        } else if bytes.starts_with(b"BM") {
            Self::Bmp
        } else {
            Self::Unknown
        }
    }
}

/// Digest of a file's metadata region.
///
/// The region is the bytes that hold tags and chapters and nothing else: an
/// MP4's `udta` and `meta` boxes, its chapter track tables and the chapter
/// title samples; an MP3's ID3v2 and ID3v1 tags; a FLAC's Vorbis comment,
/// picture and cue sheet blocks. Re-encoding the audio or moving it leaves
/// the digest alone unless the tags move with it; editing a tag changes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegionDigest {
    /// How the digest was computed; [`REGION_HASH_VERSION`] today. A later
    /// version means stored digests are recomputed, not compared.
    pub version: u16,
    /// SHA-256 over each region span in file order, each as its length (a
    /// big-endian `u64`) followed by its bytes.
    pub sha256: Sha256Digest,
    /// Total bytes the digest covers.
    pub len: u64,
}

/// A SHA-256 digest, written as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest(pub [u8; 32]);

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({})", hex::encode(self.0))
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() != 64 || text.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(serde::de::Error::custom(
                "digest must be 64 lowercase hex digits",
            ));
        }
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(&text, &mut bytes).map_err(serde::de::Error::custom)?;
        Ok(Self(bytes))
    }
}

/// Why a [`TagRecord`] from the worker was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// More entries or chapters than the limits allow.
    #[error("record has too many {0}")]
    TooMany(&'static str),
    /// A key, value or title is empty where it may not be, too long, or
    /// contains NUL.
    #[error("record has a bad {0}")]
    BadText(&'static str),
    /// Chapters are not in start order.
    #[error("chapters are out of order")]
    ChapterOrder,
    /// `chapter_source` does not agree with whether there are chapters.
    #[error("chapter source does not match the chapters")]
    ChapterSource,
    /// A duration over [`MAX_DURATION_MS`].
    #[error("duration is out of range")]
    Duration,
    /// A codec the container cannot hold (an MP3 or FLAC file always holds
    /// its own).
    #[error("codec does not match the container")]
    Codec,
    /// A cover span outside the file or of the wrong length.
    #[error("cover span is outside the file")]
    CoverSpan,
    /// A region digest that covers more bytes than the file has.
    #[error("region digest covers more than the file")]
    RegionLen,
    /// A region digest of another version.
    #[error("region digest version {0} is not {REGION_HASH_VERSION}")]
    RegionVersion(u16),
}

/// Checks `text` against a byte limit and the NUL rule.
fn check_text(
    text: &str,
    max: usize,
    empty_ok: bool,
    what: &'static str,
) -> Result<(), RecordError> {
    if text.len() > max || text.contains('\0') || (!empty_ok && text.is_empty()) {
        return Err(RecordError::BadText(what));
    }
    Ok(())
}

impl TagRecord {
    /// Checks a record that came from the worker against the reader's
    /// limits, so a compromised worker cannot hand the host more than an
    /// honest reader would.
    ///
    /// # Arguments
    ///
    /// * `file_size` - Size of the file the record describes, in bytes.
    ///
    /// # Errors
    ///
    /// The first [`RecordError`] found.
    pub fn validate(&self, file_size: u64) -> Result<(), RecordError> {
        if self.entries.len() > MAX_ENTRIES {
            return Err(RecordError::TooMany("entries"));
        }
        for entry in &self.entries {
            check_text(&entry.value, MAX_VALUE_BYTES, true, "value")?;
            for key in entry.field.keys() {
                check_text(key, MAX_KEY_BYTES, false, "key")?;
            }
        }
        if self.chapters.len() > MAX_CHAPTERS {
            return Err(RecordError::TooMany("chapters"));
        }
        for chapter in &self.chapters {
            check_text(
                &chapter.title,
                MAX_CHAPTER_TITLE_BYTES,
                true,
                "chapter title",
            )?;
        }
        if self
            .chapters
            .windows(2)
            .any(|pair| pair[0].start_ms > pair[1].start_ms)
        {
            return Err(RecordError::ChapterOrder);
        }
        if self.chapters.is_empty() != self.chapter_source.is_none() {
            return Err(RecordError::ChapterSource);
        }
        if self.duration.is_some_and(|d| d.ms > MAX_DURATION_MS)
            || self
                .chapters
                .iter()
                .any(|chapter| chapter.start_ms > MAX_DURATION_MS)
        {
            return Err(RecordError::Duration);
        }
        if let Some(cover) = &self.cover {
            if let Some(span) = cover.span {
                if span.len != cover.len || span.end().is_none_or(|end| end > file_size) {
                    return Err(RecordError::CoverSpan);
                }
            }
            if cover.len > file_size {
                return Err(RecordError::CoverSpan);
            }
        }
        let codec_fits = match (self.container, self.codec) {
            (Container::Mp3, Some(AudioCodec::Mp3)) | (Container::Flac, Some(AudioCodec::Flac)) => {
                true
            }
            (Container::Mp4, _) => true,
            (Container::Mp3 | Container::Flac, _) => false,
        };
        if !codec_fits {
            return Err(RecordError::Codec);
        }
        if self.region.version != REGION_HASH_VERSION {
            return Err(RecordError::RegionVersion(self.region.version));
        }
        if self.region.len > file_size {
            return Err(RecordError::RegionLen);
        }
        Ok(())
    }
}
