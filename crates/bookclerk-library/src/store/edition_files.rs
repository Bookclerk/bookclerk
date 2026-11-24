//! Every file key an edition has, per output destination (`edition_files`).
//!
//! Files belong to editions, not to the account rows that hold them
//! (library data model ADR, `docs/adr/library-data-model.md`,
//! section 3): two accounts holding one edition share its files, and an
//! acquire of one of them serves both. Library rows record only a title's
//! primary audio and PDF keys, but an acquire also writes chapter or part
//! files and sidecars, and a naming template may put several titles in one
//! folder. This table records each of those keys against the edition that
//! wrote it, so a host upload can be refused a key another edition holds on
//! that destination, an acquire can give a second edition its own path, and
//! an aborted upload's copy is deleted only while the key is still that
//! upload's.
//!
//! An acquire run on this node records each key just before it writes the
//! file ([`LibraryStore::record_edition_file`]; the file is the edition's
//! from then on, whichever edition had it before). A host upload claims
//! every key of its manifest when it is granted
//! ([`LibraryStore::claim_edition_file`]), and only keys no other edition
//! holds (a Host job's upload of a book may take over keys a branch's
//! catalog upload wrote); a grant refused part way gives back what it
//! claimed ([`LibraryStore::release_edition_file`]).
//!
//! A title a branch shares here is a node holding of an edition, so its
//! files are that edition's like any other.

use sea_orm::sea_query::{Expr, OnConflict, Query};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder};

use super::{now_str, LibraryStore};
use crate::entities::{
    edition_acquisitions, edition_files, editions, holdings, node_upload_sessions, store_listings,
};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;

/// The `writer` of a key an acquire on this node wrote without a run of
/// its own (a branch's import of its owner's title), and the prefix of
/// every acquire run's own writer ([`AcquireWriter`]).
pub const ACQUIRE_WRITER: &str = "acquire";

/// The `writer` one acquire run records its keys under:
/// `acquire:<32 hex>`, unique to the run.
///
/// Each run has its own, so a write that fails can release exactly the
/// record this run made for it ([`LibraryStore::release_run_edition_file`])
/// and never a concurrent run's record of the same key. It is never a host
/// upload id, so no upload's claim mistakes a run's key for its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireWriter(String);

impl AcquireWriter {
    /// A writer no other run has.
    #[must_use]
    pub fn for_run() -> Self {
        Self(format!(
            "{ACQUIRE_WRITER}:{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// The stored `writer` value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when `writer` is an acquire's: [`ACQUIRE_WRITER`] or a run's
    /// own writer.
    #[must_use]
    pub fn is_acquire(writer: &str) -> bool {
        writer == ACQUIRE_WRITER
            || writer
                .strip_prefix(ACQUIRE_WRITER)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|run| !run.is_empty())
    }
}

/// What recording a key an acquire run is about to write did
/// ([`LibraryStore::record_run_edition_file`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedFile {
    /// The key was unrecorded: this run's record is new, and is this
    /// run's to release when its write fails.
    New,
    /// The key was already the owner's (or a stale holder's) and is now
    /// recorded for this run: the earlier file may still be there, so a
    /// failed write leaves the record.
    TakenOver,
    /// Another live edition holds the key; nothing changed.
    Refused,
}

/// Who a recorded file key belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileOwner {
    /// An edition (`editions.id`): every holding of it shares the file.
    Edition(String),
}

impl FileOwner {
    /// The stored `owner_kind`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Edition(_) => "edition",
        }
    }

    /// The stored `owner_id`.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Edition(id) => id,
        }
    }

    /// The owner a stored row names; `None` for an unknown `owner_kind`.
    fn from_row(kind: &str, id: String) -> Option<Self> {
        match kind {
            "edition" => Some(Self::Edition(id)),
            _ => None,
        }
    }
}

/// What a recorded file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileRole {
    /// The edition's primary audio file.
    Audio,
    /// Another audio file of the edition: a part or a chapter file.
    Part,
    /// A chapter file of a title split by chapter.
    Chapter,
    /// The companion PDF.
    Pdf,
    /// Cover art.
    Cover,
    /// Any other file beside the audio (`.cue`, chapter JSON, metadata).
    Sidecar,
    /// A transcript or lyrics file.
    Transcript,
    /// An EPUB.
    Epub,
}

impl FileRole {
    /// Every role, in stored order (the `edition_files.role` CHECK list).
    pub const ALL: [Self; 8] = [
        Self::Audio,
        Self::Part,
        Self::Chapter,
        Self::Pdf,
        Self::Cover,
        Self::Sidecar,
        Self::Transcript,
        Self::Epub,
    ];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Part => "part",
            Self::Chapter => "chapter",
            Self::Pdf => "pdf",
            Self::Cover => "cover",
            Self::Sidecar => "sidecar",
            Self::Transcript => "transcript",
            Self::Epub => "epub",
        }
    }

    /// The role a stored spelling names; [`Self::Sidecar`] for an unknown
    /// one, the role that gates nothing.
    #[must_use]
    pub fn parse(stored: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|role| role.as_str() == stored)
            .unwrap_or(Self::Sidecar)
    }

    /// The role of the file at `key`, from its extension.
    ///
    /// # Arguments
    ///
    /// * `key` - The storage key.
    /// * `primary` - The edition's primary audio key, when known: another
    ///   audio file is then a [`Self::Part`]. Without it every audio file
    ///   is [`Self::Audio`].
    #[must_use]
    pub fn for_key(key: &str, primary: Option<&str>) -> Self {
        let name = key.rsplit('/').next().unwrap_or(key);
        let ext = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "m4b" | "m4a" | "mp3" | "mp4" | "aac" | "flac" | "ogg" | "oga" | "opus" | "wav" => {
                match primary {
                    Some(primary) if primary != key => Self::Part,
                    _ => Self::Audio,
                }
            }
            "pdf" => Self::Pdf,
            "epub" => Self::Epub,
            "jpg" | "jpeg" | "png" | "webp" | "gif" => Self::Cover,
            "vtt" | "srt" | "lrc" => Self::Transcript,
            _ => Self::Sidecar,
        }
    }
}

/// Who holds a recorded file key, and what the file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionFile {
    /// The edition (or branch title) the file belongs to.
    pub owner: FileOwner,
    /// The host upload id that claimed the key last, or an acquire's
    /// writer ([`AcquireWriter`]).
    pub writer: String,
    /// What the file is.
    pub role: FileRole,
    /// Lowercase hex SHA-256 of the content, when known (host uploads).
    pub sha256: Option<String>,
}

/// What a probe found in one stored audio file, and where the copy came
/// from (`edition_files`).
///
/// The same values decide which of two copies of one edition is kept
/// ([`FileProbe::is_better_than`]), wherever the comparison runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProbe {
    /// Media type of the file (`audio/mp4`, `audio/mpeg`, `audio/flac`).
    pub content_type: String,
    /// Average audio bitrate in kilobits per second; `None` when the file
    /// states no running time to divide its size by.
    pub bitrate_kbps: Option<u32>,
    /// True for a lossless encoding (FLAC, ALAC).
    pub lossless: bool,
    /// Plain id of the storefront the copy was downloaded from, when known.
    pub source_store: Option<String>,
}

impl FileProbe {
    /// Longest media type a probe may carry, in bytes.
    pub const MAX_CONTENT_TYPE_BYTES: usize = 64;

    /// Highest bitrate a probe may carry, in kilobits per second: above any
    /// audiobook encoding, so a forged value cannot win by size alone.
    pub const MAX_BITRATE_KBPS: u32 = 20_000;

    /// True when a copy with this probe should replace one with `other`
    /// (library data model ADR, section 3): a lossless copy beats a lossy
    /// one, and within the same kind the higher bitrate wins. A copy whose
    /// bitrate is unknown never replaces one whose bitrate is known, and
    /// equal copies do not replace each other.
    #[must_use]
    pub fn is_better_than(&self, other: &Self) -> bool {
        if self.lossless != other.lossless {
            return self.lossless;
        }
        match (self.bitrate_kbps, other.bitrate_kbps) {
            (Some(mine), Some(theirs)) => mine > theirs,
            (Some(_), None) => true,
            (None, Some(_) | None) => false,
        }
    }

    /// True when the values are ones an honest probe can produce: a short
    /// media type of visible ASCII, a bitrate within
    /// [`Self::MAX_BITRATE_KBPS`], and a storefront that is a store's plain
    /// id (lowercase letters, digits, `_`, `-` and `.`, as node frames name
    /// stores).
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let type_ok = !self.content_type.is_empty()
            && self.content_type.len() <= Self::MAX_CONTENT_TYPE_BYTES
            && self.content_type.bytes().all(|b| b.is_ascii_graphic());
        let store_ok = self.source_store.as_deref().is_none_or(|store| {
            !store.is_empty()
                && store.len() <= Self::MAX_CONTENT_TYPE_BYTES
                && store
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
        });
        type_ok
            && store_ok
            && self
                .bitrate_kbps
                .is_none_or(|kbps| kbps > 0 && kbps <= Self::MAX_BITRATE_KBPS)
    }
}

/// What a probe found about the audio stream of one file, besides its
/// [`FileProbe`]: the encoding, channels, sample rate and the running time
/// a player sees.
///
/// Each value is `None` when the file's headers do not state it; nothing
/// is decoded to find them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioStream {
    /// The audio encoding.
    pub codec: Option<bookclerk_tags::AudioCodec>,
    /// Channels, sample rate and, for AAC, the profile.
    pub format: bookclerk_tags::AudioFormat,
    /// The playable running time in milliseconds, as the container states
    /// it after edit lists and encoder priming; never zero
    /// ([`Self::is_well_formed`]).
    pub duration_ms: Option<u64>,
}

impl AudioStream {
    /// True when every stated value is in range: channels and a sample
    /// rate above zero and within the tag reader's limits, an AAC profile
    /// only on AAC, and a running time above zero that a database integer
    /// holds and [`bookclerk_tags::MAX_DURATION_MS`] allows.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let format = self.format;
        format
            .channels
            .is_none_or(|channels| (1..=bookclerk_tags::MAX_CHANNELS).contains(&channels))
            && format
                .sample_rate_hz
                .is_none_or(|rate| (1..=bookclerk_tags::MAX_SAMPLE_RATE_HZ).contains(&rate))
            && (format.aac_profile.is_none() || self.codec == Some(bookclerk_tags::AudioCodec::Aac))
            && self
                .duration_ms
                .is_none_or(|ms| (1..=bookclerk_tags::MAX_DURATION_MS).contains(&ms))
    }
}

/// One audio file of an edition as [`LibraryStore::edition_audio_files`]
/// reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedAudioFile {
    /// Output destination id (`local`, `s3`).
    pub destination: String,
    /// Storage key on that destination.
    pub storage_key: String,
    /// Primary audio, a part or a chapter file.
    pub role: FileRole,
    /// The library copy (`output.copies`) the file belongs to; `None` for
    /// an upload or a copy this build cannot read.
    pub copy: Option<bookclerk_config::OutputCopy>,
    /// What the last probe found; `None` until the file has been probed.
    pub probe: Option<FileProbe>,
    /// What the last probe found about the stream; all `None` until the
    /// file has been probed, or for a probe from before streams were
    /// stored.
    pub stream: AudioStream,
}

/// The row of an audio file read back into a [`ProbedAudioFile`]. Stored
/// values this build cannot read (an unknown codec name, a negative count)
/// read as unknown.
fn probed_audio_file(row: edition_files::Model) -> ProbedAudioFile {
    let probe = row.probed_at.as_ref().and_then(|_| {
        Some(FileProbe {
            content_type: row.content_type.clone()?,
            bitrate_kbps: row.bitrate_kbps.and_then(|kbps| u32::try_from(kbps).ok()),
            lossless: row.lossless != 0,
            source_store: row.source_store.clone(),
        })
    });
    let codec = row
        .codec
        .as_deref()
        .and_then(bookclerk_tags::AudioCodec::parse);
    let stream = AudioStream {
        codec,
        format: bookclerk_tags::AudioFormat {
            channels: row
                .channels
                .and_then(|channels| u8::try_from(channels).ok()),
            sample_rate_hz: row.sample_rate_hz.and_then(|rate| u32::try_from(rate).ok()),
            aac_profile: row
                .aac_profile
                .as_deref()
                .and_then(bookclerk_tags::AacProfile::parse),
        },
        duration_ms: row
            .duration_ms
            .and_then(|ms| u64::try_from(ms).ok())
            .filter(|&ms| ms > 0),
    };
    ProbedAudioFile {
        copy: row.copy_format.as_deref().and_then(|format| {
            Some(bookclerk_config::OutputCopy {
                format: bookclerk_config::CopyFormat::parse(format)?,
                quality: bookclerk_config::CopyQuality::parse(row.copy_quality.as_deref()?)?,
            })
        }),
        role: FileRole::parse(&row.role),
        destination: row.destination,
        storage_key: row.storage_key,
        probe,
        stream,
    }
}

/// Clears the stream columns of an `edition_files` update, as a new claim
/// of a key clears its probe: the old file's stream says nothing about the
/// new one.
trait ClearStream {
    /// The update with every [`AudioStream`] column set to null.
    fn clear_stream(self) -> Self;
}

impl ClearStream for sea_orm::UpdateMany<edition_files::Entity> {
    fn clear_stream(self) -> Self {
        self.col_expr(
            edition_files::Column::Codec,
            Expr::value(Option::<String>::None),
        )
        .col_expr(
            edition_files::Column::AacProfile,
            Expr::value(Option::<String>::None),
        )
        .col_expr(
            edition_files::Column::Channels,
            Expr::value(Option::<i64>::None),
        )
        .col_expr(
            edition_files::Column::SampleRateHz,
            Expr::value(Option::<i64>::None),
        )
        .col_expr(
            edition_files::Column::DurationMs,
            Expr::value(Option::<i64>::None),
        )
    }
}

/// Forgets what a record said of the copy at its key, for a new copy
/// written there: its probe is stored once known.
trait ForgetCopy {
    /// The update with the probe, the [`AudioStream`] and the copy's
    /// format and quality cleared.
    fn forget_copy(self) -> Self;
}

impl ForgetCopy for sea_orm::UpdateMany<edition_files::Entity> {
    fn forget_copy(self) -> Self {
        self.col_expr(edition_files::Column::Lossless, Expr::value(0))
            .col_expr(
                edition_files::Column::BitrateKbps,
                Expr::value(Option::<i64>::None),
            )
            .col_expr(
                edition_files::Column::ContentType,
                Expr::value(Option::<String>::None),
            )
            .col_expr(
                edition_files::Column::SourceStore,
                Expr::value(Option::<String>::None),
            )
            .col_expr(
                edition_files::Column::ProbedAt,
                Expr::value(Option::<String>::None),
            )
            .clear_stream()
            .col_expr(
                edition_files::Column::CopyFormat,
                Expr::value(Option::<String>::None),
            )
            .col_expr(
                edition_files::Column::CopyQuality,
                Expr::value(Option::<String>::None),
            )
    }
}

/// An edition's acquire state (`edition_acquisitions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionAcquisition {
    /// The edition id.
    pub edition_id: String,
    /// Where the edition's acquire stands; a status this build does not
    /// know reads as [`AcquireStatus::NotAcquired`].
    pub status: AcquireStatus,
    /// Primary destination key of the edition's audio once acquired.
    pub storage_key: Option<String>,
    /// Last acquire failure message.
    pub error_message: Option<String>,
    /// The account holding (`holdings.id`) whose title's write set the
    /// state.
    pub holding_id: String,
    /// RFC 3339 time of that write.
    pub updated_at: String,
    /// Where the edition's companion PDF acquire stands; a status this
    /// build does not know reads as [`AcquireStatus::NotAcquired`].
    pub pdf_status: AcquireStatus,
    /// Destination key of the companion PDF once acquired.
    pub pdf_storage_key: Option<String>,
    /// The account holding (`holdings.id`) whose title's write set the PDF
    /// state; `None` until a PDF write happens.
    pub pdf_holding_id: Option<String>,
}

impl From<edition_acquisitions::Model> for EditionAcquisition {
    fn from(row: edition_acquisitions::Model) -> Self {
        Self {
            status: AcquireStatus::parse(&row.status).unwrap_or_default(),
            edition_id: row.edition_id,
            storage_key: row.storage_key,
            error_message: row.error_message,
            holding_id: row.holding_id,
            updated_at: row.updated_at,
            pdf_status: AcquireStatus::parse(&row.pdf_status).unwrap_or_default(),
            pdf_storage_key: row.pdf_storage_key,
            pdf_holding_id: row.pdf_holding_id,
        }
    }
}

/// The edition a library title sells and the account holding it is held
/// by: the key its acquire writes are recorded under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleHolding {
    /// The edition the title's listing sells.
    pub edition_id: String,
    /// The account's holding of that listing (`holdings.id`).
    pub holding_id: String,
}

/// A row read back into an [`EditionFile`]; `None` for a row whose
/// `owner_kind` this build does not know, which holds its key against
/// every claim.
fn edition_file(row: edition_files::Model) -> Option<EditionFile> {
    Some(EditionFile {
        owner: FileOwner::from_row(&row.owner_kind, row.owner_id)?,
        writer: row.writer,
        role: FileRole::parse(&row.role),
        sha256: row.sha256,
    })
}

/// The condition matching a row that is still exactly `file`.
fn still(file: &EditionFile) -> Condition {
    Condition::all()
        .add(edition_files::Column::OwnerKind.eq(file.owner.kind()))
        .add(edition_files::Column::OwnerId.eq(file.owner.id()))
        .add(edition_files::Column::Writer.eq(file.writer.as_str()))
}

impl LibraryStore {
    /// An edition's acquire state; `None` when no title of it was ever
    /// queued or acquired.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn edition_acquisition(
        &self,
        edition_id: &str,
    ) -> Result<Option<EditionAcquisition>> {
        Ok(
            edition_acquisitions::Entity::find_by_id(edition_id.to_string())
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?
                .map(EditionAcquisition::from),
        )
    }

    /// The edition holding `holding_id` (`holdings.id`) holds: the one its
    /// listing sells; `None` when the holding or its listing is gone.
    ///
    /// # Errors
    ///
    /// Returns an error when a read fails.
    pub async fn edition_of_holding(&self, holding_id: &str) -> Result<Option<String>> {
        Ok(self
            .file_owner_of_holding(holding_id)
            .await?
            .map(|FileOwner::Edition(edition_id)| edition_id))
    }

    /// The file owner of a node holding (`holdings.id`, a title a branch
    /// shares here): the edition its listing sells.
    ///
    /// # Errors
    ///
    /// Returns an error when a read fails.
    pub async fn file_owner_of_holding(&self, holding_id: &str) -> Result<Option<FileOwner>> {
        let Some(holding) = super::holdings::find_holdings()
            .filter(holdings::Column::Id.eq(holding_id))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(store_listings::Entity::find_by_id(holding.listing_id)
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(|listing| FileOwner::Edition(listing.edition_id)))
    }

    /// Records that `owner` writes `key` on `destination` (an acquire run
    /// on this node, just before the write).
    ///
    /// The record is taken when the key is unrecorded, already `owner`'s,
    /// or held by an owner no longer live (a removed edition, or a title a
    /// branch shares here, whose keys an acquire on this node takes over).
    /// Another live edition's key is refused, changing nothing: two
    /// editions whose short ids collide can plan one chapter or sidecar
    /// name the audio-name check does not see, and the write must not
    /// replace that edition's file. Each record is one conditional write,
    /// so of two acquires racing for one key only one wins.
    ///
    /// # Returns
    ///
    /// `true` when the key is now `owner`'s; `false` when another live
    /// edition holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    pub async fn record_edition_file(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        role: FileRole,
    ) -> Result<bool> {
        self.record_edition_copy_file(destination, key, owner, role, None)
            .await
    }

    /// [`Self::record_edition_file`] for a file of one library copy
    /// (`output.copies`): the row also names the copy's format and quality,
    /// or none when `copy` is `None`.
    ///
    /// # Returns
    ///
    /// `true` when the key is now `owner`'s; `false` when another live
    /// edition holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    pub async fn record_edition_copy_file(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        role: FileRole,
        copy: Option<&bookclerk_config::OutputCopy>,
    ) -> Result<bool> {
        let recorded = self
            .record_file_as(destination, key, owner, role, copy, ACQUIRE_WRITER)
            .await?;
        Ok(recorded != RecordedFile::Refused)
    }

    /// [`Self::record_edition_copy_file`] for one acquire run, recorded
    /// under the run's own `writer`.
    ///
    /// # Returns
    ///
    /// Whether the record is new (the run's to release when its write
    /// fails, [`Self::release_run_edition_file`]), taken over from the
    /// owner's earlier record, or refused because another live edition
    /// holds the key.
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    pub async fn record_run_edition_file(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        role: FileRole,
        copy: Option<&bookclerk_config::OutputCopy>,
        writer: &AcquireWriter,
    ) -> Result<RecordedFile> {
        self.record_file_as(destination, key, owner, role, copy, writer.as_str())
            .await
    }

    /// Releases the record an acquire run made of `key` on `destination`
    /// ([`RecordedFile::New`]) after its write failed, so the key is free
    /// again for a host upload of the same title.
    ///
    /// Only a record still naming `writer` goes: a concurrent run or an
    /// upload that recorded the key since keeps its record.
    ///
    /// # Returns
    ///
    /// `true` when the record was removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn release_run_edition_file(
        &self,
        destination: &str,
        key: &str,
        writer: &AcquireWriter,
    ) -> Result<bool> {
        let removed = edition_files::Entity::delete_many()
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::StorageKey.eq(key))
            .filter(edition_files::Column::Writer.eq(writer.as_str()))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(removed.rows_affected > 0)
    }

    /// Records `key` for `owner` under `writer`, as
    /// [`Self::record_run_edition_file`] describes.
    async fn record_file_as(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        role: FileRole,
        copy: Option<&bookclerk_config::OutputCopy>,
        writer: &str,
    ) -> Result<RecordedFile> {
        let now = now_str();
        let copy_format = copy.map(|copy| copy.format.as_str().to_string());
        let copy_quality = copy.map(|copy| copy.quality.to_string());
        let inserted = edition_files::Entity::insert(edition_files::ActiveModel {
            destination: Set(destination.to_string()),
            storage_key: Set(key.to_string()),
            owner_kind: Set(owner.kind().to_string()),
            owner_id: Set(owner.id().to_string()),
            role: Set(role.as_str().to_string()),
            lossless: Set(0),
            bitrate_kbps: Set(None),
            content_type: Set(None),
            source_store: Set(None),
            probed_at: Set(None),
            sha256: Set(None),
            copy_format: Set(copy_format.clone()),
            copy_quality: Set(copy_quality.clone()),
            codec: Set(None),
            aac_profile: Set(None),
            channels: Set(None),
            sample_rate_hz: Set(None),
            duration_ms: Set(None),
            writer: Set(writer.to_string()),
            written_at: Set(now.clone()),
        })
        .on_conflict(
            OnConflict::columns([
                edition_files::Column::Destination,
                edition_files::Column::StorageKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(self.db())
        .await
        .map_err(LibraryError::Orm)?;
        if inserted > 0 {
            return Ok(RecordedFile::New);
        }
        let live_edition = Condition::all()
            .add(edition_files::Column::OwnerKind.eq("edition"))
            .add(
                edition_files::Column::OwnerId.in_subquery(
                    Query::select()
                        .column(editions::Column::Id)
                        .from(editions::Entity)
                        .to_owned(),
                ),
            );
        let mine = Condition::all()
            .add(edition_files::Column::OwnerKind.eq(owner.kind()))
            .add(edition_files::Column::OwnerId.eq(owner.id()));
        let recorded = edition_files::Entity::update_many()
            .col_expr(edition_files::Column::OwnerKind, Expr::value(owner.kind()))
            .col_expr(edition_files::Column::OwnerId, Expr::value(owner.id()))
            .col_expr(edition_files::Column::Role, Expr::value(role.as_str()))
            .col_expr(edition_files::Column::Lossless, Expr::value(0))
            .col_expr(
                edition_files::Column::BitrateKbps,
                Expr::value(Option::<i64>::None),
            )
            .col_expr(
                edition_files::Column::ContentType,
                Expr::value(Option::<String>::None),
            )
            .col_expr(
                edition_files::Column::SourceStore,
                Expr::value(Option::<String>::None),
            )
            .col_expr(
                edition_files::Column::ProbedAt,
                Expr::value(Option::<String>::None),
            )
            .clear_stream()
            .col_expr(
                edition_files::Column::Sha256,
                Expr::value(Option::<String>::None),
            )
            .col_expr(edition_files::Column::CopyFormat, Expr::value(copy_format))
            .col_expr(
                edition_files::Column::CopyQuality,
                Expr::value(copy_quality),
            )
            .col_expr(edition_files::Column::Writer, Expr::value(writer))
            .col_expr(edition_files::Column::WrittenAt, Expr::value(now))
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::StorageKey.eq(key))
            .filter(Condition::any().add(mine).add(live_edition.not()))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(if recorded.rows_affected > 0 {
            RecordedFile::TakenOver
        } else {
            RecordedFile::Refused
        })
    }

    /// Claims `key` on `destination` for `owner`, with `writer` (a host
    /// upload id) as its latest writer.
    ///
    /// The claim succeeds when the key is unrecorded, already the owner's
    /// (any account's row of the same edition), or recorded for an owner
    /// that holds no key against this claim; answers false, changing
    /// nothing, when another edition holds it. An edition holds its keys
    /// against every claim while it exists. Each claim
    /// is one conditional write, so of two uploads of different editions
    /// racing for one key only one wins.
    ///
    /// `expected` is the record the caller read before
    /// ([`Self::edition_file`]; `None` when unrecorded), which a refused
    /// claim's release restores: the claim also answers false, changing
    /// nothing, when the record is no longer that one (an acquire recorded
    /// the key in between), so a release never removes a record the claim
    /// did not replace.
    ///
    /// # Arguments
    ///
    /// * `destination`, `key` - The file's place.
    /// * `owner` - Who claims it.
    /// * `writer` - The host upload id.
    /// * `role`, `sha256` - What the file is and its declared digest.
    /// * `expected` - The record read before the claim.
    /// * `title` - What the upload writes for: the
    ///   account holding (`holdings.id`) of a job upload, or the node holding
    ///   (`holdings.id`) of a catalog upload. A key its edition already
    ///   holds is taken over only when this upload or an earlier upload for
    ///   the same title (or, for a catalog upload, an earlier catalog upload
    ///   of the same branch) wrote it, never one an acquire or another
    ///   account's upload wrote: the edition's files are shared, so one node cannot
    ///   replace them. A job upload may also take over a key a branch's
    ///   catalog upload wrote (`catalog` false), as an acquire on this node
    ///   would replace a delivered copy with its own.
    /// * `catalog` - True for a catalog upload's claim.
    /// * `upgrade` - True when the upload is a better copy of `owner`'s
    ///   edition than the library holds: it takes over any key of that
    ///   edition, whoever wrote it, though still never another edition's.
    ///   An existing record keeps describing its file (only its writer
    ///   changes), since the better copy is moved onto the key only at its
    ///   commit ([`Self::describe_uploaded_file`]).
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn claim_edition_file(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        writer: &str,
        role: FileRole,
        sha256: Option<&str>,
        expected: Option<&EditionFile>,
        title: &str,
        catalog: bool,
        upgrade: bool,
    ) -> Result<bool> {
        let now = now_str();
        // Only a key read as unrecorded is inserted: a claim expecting a
        // record that has since gone must change nothing, so its retry
        // reads the key afresh.
        if expected.is_none() {
            edition_files::Entity::insert(edition_files::ActiveModel {
                destination: Set(destination.to_string()),
                storage_key: Set(key.to_string()),
                owner_kind: Set(owner.kind().to_string()),
                owner_id: Set(owner.id().to_string()),
                role: Set(role.as_str().to_string()),
                lossless: Set(0),
                bitrate_kbps: Set(None),
                content_type: Set(None),
                source_store: Set(None),
                probed_at: Set(None),
                sha256: Set(sha256.map(str::to_string)),
                copy_format: Set(None),
                copy_quality: Set(None),
                codec: Set(None),
                aac_profile: Set(None),
                channels: Set(None),
                sample_rate_hz: Set(None),
                duration_ms: Set(None),
                writer: Set(writer.to_string()),
                written_at: Set(now.clone()),
            })
            .on_conflict(
                OnConflict::columns([
                    edition_files::Column::Destination,
                    edition_files::Column::StorageKey,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        }
        // A holder is live while its edition is in the library; only a
        // holder that is not is stale.
        let live = Condition::all()
            .add(edition_files::Column::OwnerKind.eq("edition"))
            .add(
                edition_files::Column::OwnerId.in_subquery(
                    Query::select()
                        .column(editions::Column::Id)
                        .from(editions::Entity)
                        .to_owned(),
                ),
            );
        let uploads_for = |cond: Condition| {
            edition_files::Column::Writer.in_subquery(
                Query::select()
                    .column(node_upload_sessions::Column::UploadId)
                    .from(node_upload_sessions::Entity)
                    .cond_where(cond)
                    .to_owned(),
            )
        };
        let mut writers = Condition::any()
            .add(edition_files::Column::Writer.eq(writer))
            .add(uploads_for(
                Condition::all().add(node_upload_sessions::Column::HoldingId.eq(title)),
            ));
        if catalog {
            // The same branch's other catalog uploads: a title it shares
            // again after a pause is held under a new id, and its earlier
            // delivery is still its own.
            writers = writers.add(uploads_for(
                Condition::all()
                    .add(node_upload_sessions::Column::JobId.eq(bookclerk_node_proto::CATALOG_ITEM))
                    .add(
                        node_upload_sessions::Column::NodeId.in_subquery(
                            Query::select()
                                .column(node_upload_sessions::Column::NodeId)
                                .from(node_upload_sessions::Entity)
                                .and_where(node_upload_sessions::Column::UploadId.eq(writer))
                                .to_owned(),
                        ),
                    ),
            ));
        } else {
            writers = writers.add(uploads_for(Condition::all().add(
                node_upload_sessions::Column::JobId.eq(bookclerk_node_proto::CATALOG_ITEM),
            )));
        }
        if upgrade {
            // A better copy of the edition replaces its files, whoever
            // wrote them (`writer` is never null, so this takes any).
            writers = writers.add(edition_files::Column::Writer.is_not_null());
        }
        let mine = Condition::all()
            .add(edition_files::Column::OwnerKind.eq(owner.kind()))
            .add(edition_files::Column::OwnerId.eq(owner.id()))
            .add(writers);
        let claim = edition_files::Entity::update_many()
            .col_expr(edition_files::Column::Writer, Expr::value(writer))
            .col_expr(edition_files::Column::WrittenAt, Expr::value(now));
        // A better copy is staged beside the key and moved onto it only at
        // its commit, so until then the record keeps describing the file
        // still there; the commit describes the new one
        // ([`Self::describe_uploaded_file`]).
        let claim = if upgrade && expected.is_some() {
            claim
        } else {
            claim
                .col_expr(edition_files::Column::OwnerKind, Expr::value(owner.kind()))
                .col_expr(edition_files::Column::OwnerId, Expr::value(owner.id()))
                .col_expr(edition_files::Column::Role, Expr::value(role.as_str()))
                .col_expr(
                    edition_files::Column::Sha256,
                    Expr::value(sha256.map(str::to_string)),
                )
                .forget_copy()
        };
        let claimed = claim
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::StorageKey.eq(key))
            .filter(Condition::any().add(mine).add(live.not()))
            // Unrecorded before: only the row the insert above created.
            .filter(match expected {
                Some(prior) => still(prior),
                None => Condition::all()
                    .add(edition_files::Column::OwnerKind.eq(owner.kind()))
                    .add(edition_files::Column::OwnerId.eq(owner.id()))
                    .add(edition_files::Column::Writer.eq(writer)),
            })
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(claimed.rows_affected == 1)
    }

    /// Gives back a claim `writer` made on `key` on `destination`: the
    /// record returns to `prior` (who held it before the claim), or is
    /// removed when `prior` is `None` (the claim created it). Changes
    /// nothing once someone else has written the key since (its writer is
    /// no longer `writer`), so a release never takes a key from a later
    /// claim or acquire.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn release_edition_file(
        &self,
        destination: &str,
        key: &str,
        writer: &str,
        prior: Option<&EditionFile>,
    ) -> Result<()> {
        let ours = Condition::all()
            .add(edition_files::Column::Destination.eq(destination))
            .add(edition_files::Column::StorageKey.eq(key))
            .add(edition_files::Column::Writer.eq(writer));
        match prior {
            None => {
                edition_files::Entity::delete_many()
                    .filter(ours)
                    .exec(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
            }
            Some(prior) => {
                edition_files::Entity::update_many()
                    .col_expr(
                        edition_files::Column::OwnerKind,
                        Expr::value(prior.owner.kind()),
                    )
                    .col_expr(
                        edition_files::Column::OwnerId,
                        Expr::value(prior.owner.id()),
                    )
                    .col_expr(
                        edition_files::Column::Role,
                        Expr::value(prior.role.as_str()),
                    )
                    .col_expr(
                        edition_files::Column::Sha256,
                        Expr::value(prior.sha256.clone()),
                    )
                    .col_expr(
                        edition_files::Column::Writer,
                        Expr::value(prior.writer.as_str()),
                    )
                    // The probe on the row is the given-back upload's, and
                    // the earlier copy's was cleared by its claim: unknown
                    // again until the file is probed.
                    .col_expr(edition_files::Column::Lossless, Expr::value(0))
                    .col_expr(
                        edition_files::Column::BitrateKbps,
                        Expr::value(Option::<i64>::None),
                    )
                    .col_expr(
                        edition_files::Column::ContentType,
                        Expr::value(Option::<String>::None),
                    )
                    .col_expr(
                        edition_files::Column::SourceStore,
                        Expr::value(Option::<String>::None),
                    )
                    .col_expr(
                        edition_files::Column::ProbedAt,
                        Expr::value(Option::<String>::None),
                    )
                    .clear_stream()
                    .filter(ours)
                    .exec(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
            }
        }
        Ok(())
    }

    /// How many keys `writer` (a host upload id) holds, on every
    /// destination together. An upload id is unique and claims keys for one
    /// owner only, so this is how many keys that upload's title still holds
    /// through it.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn count_edition_files_by(&self, writer: &str) -> Result<u64> {
        use sea_orm::PaginatorTrait as _;
        edition_files::Entity::find()
            .filter(edition_files::Column::Writer.eq(writer))
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)
    }

    /// How many keys `writer` (a host upload id) is recorded as the latest
    /// writer of under `owner`: compared with
    /// [`Self::count_edition_files_by`], it says whether every key the
    /// upload holds is still filed under that owner.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn count_edition_files_by_owner(
        &self,
        writer: &str,
        owner: &FileOwner,
    ) -> Result<u64> {
        use sea_orm::PaginatorTrait as _;
        edition_files::Entity::find()
            .filter(edition_files::Column::Writer.eq(writer))
            .filter(edition_files::Column::OwnerKind.eq(owner.kind()))
            .filter(edition_files::Column::OwnerId.eq(owner.id()))
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)
    }

    /// True when a live edition other than `edition` holds `key` on
    /// `destination`, so writing there would replace that edition's file.
    ///
    /// A key that is unrecorded, recorded for `edition`, or recorded for an
    /// edition no longer in the library is free. With `edition` `None`
    /// (the title has no edition here), any live edition's record counts.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn key_held_by_another_edition(
        &self,
        destination: &str,
        key: &str,
        edition: Option<&str>,
    ) -> Result<bool> {
        let Some(row) =
            edition_files::Entity::find_by_id((destination.to_string(), key.to_string()))
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        // A row of an owner kind this build does not know holds its key
        // against every writer, as it does against every claim.
        if row.owner_kind != "edition" {
            return Ok(true);
        }
        if edition == Some(row.owner_id.as_str()) {
            return Ok(false);
        }
        let live = editions::Entity::find_by_id(row.owner_id)
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(live.is_some())
    }

    /// The edition a store's listing sells; `None` while the listing is
    /// not in this library.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The store's PluginKey.
    /// * `marketplace` - The store's marketplace (any case; blank for one).
    /// * `product_id` - The store's product id.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn edition_of_listing(
        &self,
        plugin_key: &str,
        marketplace: &str,
        product_id: &str,
    ) -> Result<Option<String>> {
        let marketplace = bookclerk_resolve::stored_marketplace(marketplace);
        let product_id = bookclerk_resolve::stored_product_id(product_id);
        Ok(store_listings::Entity::find()
            .filter(store_listings::Column::PluginKey.eq(plugin_key))
            .filter(store_listings::Column::Marketplace.eq(marketplace.as_str()))
            .filter(store_listings::Column::ProductId.eq(product_id.as_str()))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(|listing| listing.edition_id))
    }

    /// Who holds `key` on `destination`; `None` when it is unrecorded.
    ///
    /// A row naming an owner kind this build does not know reads as held
    /// by an edition no claim matches, so it is never taken over.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn edition_file(&self, destination: &str, key: &str) -> Result<Option<EditionFile>> {
        let row = edition_files::Entity::find_by_id((destination.to_string(), key.to_string()))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.map(|row| {
            let writer = row.writer.clone();
            edition_file(row).unwrap_or(EditionFile {
                owner: FileOwner::Edition(String::new()),
                writer,
                role: FileRole::Sidecar,
                sha256: None,
            })
        }))
    }

    /// Stores what a probe found in `owner`'s file at `key` on
    /// `destination`, stamped with the time of the write.
    ///
    /// The row is changed only while `owner` still holds the key, so a
    /// probe that finishes after another edition took the key over
    /// describes nothing.
    ///
    /// # Returns
    ///
    /// `true` when the row was `owner`'s and now carries `probe`.
    ///
    /// # Errors
    ///
    /// Returns an error when `probe` is not well formed
    /// ([`FileProbe::is_well_formed`]) or the write fails.
    pub async fn set_edition_file_probe(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        probe: &FileProbe,
    ) -> Result<bool> {
        let held = Condition::all()
            .add(edition_files::Column::OwnerKind.eq(owner.kind()))
            .add(edition_files::Column::OwnerId.eq(owner.id()));
        self.write_file_probe(destination, key, held, probe).await
    }

    /// Describes the file the committed upload `writer` moved onto `key` on
    /// `destination` as a new copy: its digest is `sha256`, and what the
    /// record said of the copy it replaced (probe, stream, format) is
    /// cleared. A better copy of an edition claims its keys without
    /// touching their records ([`Self::claim_edition_file`]), so the commit
    /// calls this before storing the new probe.
    ///
    /// The row is changed only while `writer` is still the upload that
    /// claimed the key last.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn describe_uploaded_file(
        &self,
        destination: &str,
        key: &str,
        writer: &str,
        sha256: &str,
    ) -> Result<()> {
        edition_files::Entity::update_many()
            .col_expr(edition_files::Column::Sha256, Expr::value(sha256))
            .forget_copy()
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::StorageKey.eq(key))
            .filter(edition_files::Column::Writer.eq(writer))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Stores what a probe found in the file the upload `writer` wrote at
    /// `key` on `destination`, as [`Self::set_edition_file_probe`] does for
    /// an owner.
    ///
    /// The row is changed only while `writer` is still the upload that
    /// claimed the key last, so a committed upload never describes a file
    /// a later write replaced.
    ///
    /// # Returns
    ///
    /// `true` when the row was `writer`'s and now carries `probe`.
    ///
    /// # Errors
    ///
    /// Returns an error when `probe` is not well formed or the write fails.
    pub async fn set_uploaded_file_probe(
        &self,
        destination: &str,
        key: &str,
        writer: &str,
        probe: &FileProbe,
    ) -> Result<bool> {
        let written = Condition::all().add(edition_files::Column::Writer.eq(writer));
        self.write_file_probe(destination, key, written, probe)
            .await
    }

    /// Writes `probe` on the row of `key` on `destination` when it also
    /// matches `holder`.
    async fn write_file_probe(
        &self,
        destination: &str,
        key: &str,
        holder: Condition,
        probe: &FileProbe,
    ) -> Result<bool> {
        if !probe.is_well_formed() {
            return Err(LibraryError::Invalid(
                "a file probe has a malformed media type, store or bitrate".to_string(),
            ));
        }
        let updated = edition_files::Entity::update_many()
            .col_expr(
                edition_files::Column::ContentType,
                Expr::value(probe.content_type.clone()),
            )
            .col_expr(
                edition_files::Column::BitrateKbps,
                Expr::value(probe.bitrate_kbps.map(i64::from)),
            )
            .col_expr(
                edition_files::Column::Lossless,
                Expr::value(i64::from(probe.lossless)),
            )
            .col_expr(
                edition_files::Column::SourceStore,
                Expr::value(probe.source_store.clone()),
            )
            .col_expr(edition_files::Column::ProbedAt, Expr::value(now_str()))
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::StorageKey.eq(key))
            .filter(holder)
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(updated.rows_affected > 0)
    }

    /// Stores what a probe found about the audio stream of `owner`'s file
    /// at `key` on `destination` ([`AudioStream`]), beside the probe
    /// [`Self::set_edition_file_probe`] stores.
    ///
    /// The row is changed only while `owner` still holds the key, as for
    /// the probe.
    ///
    /// # Returns
    ///
    /// `true` when the row was `owner`'s and now carries `stream`.
    ///
    /// # Errors
    ///
    /// Returns an error when `stream` is not well formed
    /// ([`AudioStream::is_well_formed`]) or the write fails.
    pub async fn set_edition_file_stream(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        stream: &AudioStream,
    ) -> Result<bool> {
        if !stream.is_well_formed() {
            return Err(LibraryError::Invalid(
                "an audio stream has a zero or out-of-range value".to_string(),
            ));
        }
        let updated = edition_files::Entity::update_many()
            .col_expr(
                edition_files::Column::Codec,
                Expr::value(stream.codec.map(|codec| codec.as_str().to_owned())),
            )
            .col_expr(
                edition_files::Column::AacProfile,
                Expr::value(
                    stream
                        .format
                        .aac_profile
                        .map(|profile| profile.as_str().to_owned()),
                ),
            )
            .col_expr(
                edition_files::Column::Channels,
                Expr::value(stream.format.channels.map(i64::from)),
            )
            .col_expr(
                edition_files::Column::SampleRateHz,
                Expr::value(stream.format.sample_rate_hz.map(i64::from)),
            )
            .col_expr(
                edition_files::Column::DurationMs,
                Expr::value(stream.duration_ms.and_then(|ms| i64::try_from(ms).ok())),
            )
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::StorageKey.eq(key))
            .filter(edition_files::Column::OwnerKind.eq(owner.kind()))
            .filter(edition_files::Column::OwnerId.eq(owner.id()))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(updated.rows_affected > 0)
    }

    /// Every audio file (primary audio, parts and chapters) recorded for
    /// edition `edition_id`, on every destination, with what the last probe
    /// of each found ([`ProbedAudioFile`]), in destination then key order.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn edition_audio_files(&self, edition_id: &str) -> Result<Vec<ProbedAudioFile>> {
        let owner = FileOwner::Edition(edition_id.to_owned());
        let rows = edition_files::Entity::find()
            .filter(edition_files::Column::OwnerKind.eq(owner.kind()))
            .filter(edition_files::Column::OwnerId.eq(owner.id()))
            .filter(edition_files::Column::Role.is_in([
                FileRole::Audio.as_str(),
                FileRole::Part.as_str(),
                FileRole::Chapter.as_str(),
            ]))
            .order_by_asc(edition_files::Column::Destination)
            .order_by_asc(edition_files::Column::StorageKey)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows.into_iter().map(probed_audio_file).collect())
    }

    /// What the last probe found in the file at `key` on `destination`;
    /// `None` when the key is unrecorded or was never probed.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn edition_file_probe(
        &self,
        destination: &str,
        key: &str,
    ) -> Result<Option<FileProbe>> {
        let row = edition_files::Entity::find_by_id((destination.to_string(), key.to_string()))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.and_then(|row| {
            row.probed_at.as_ref()?;
            Some(FileProbe {
                content_type: row.content_type?,
                bitrate_kbps: row.bitrate_kbps.and_then(|kbps| u32::try_from(kbps).ok()),
                lossless: row.lossless != 0,
                source_store: row.source_store,
            })
        }))
    }

    /// The library copy (`output.copies`) the file at `key` on
    /// `destination` belongs to; `None` when the key is unrecorded, belongs
    /// to no one copy, or names a copy this build cannot read.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn edition_file_copy(
        &self,
        destination: &str,
        key: &str,
    ) -> Result<Option<bookclerk_config::OutputCopy>> {
        let row = edition_files::Entity::find_by_id((destination.to_string(), key.to_string()))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.and_then(|row| {
            Some(bookclerk_config::OutputCopy {
                format: bookclerk_config::CopyFormat::parse(row.copy_format.as_deref()?)?,
                quality: bookclerk_config::CopyQuality::parse(row.copy_quality.as_deref()?)?,
            })
        }))
    }

    /// The audio keys (primary audio, parts and chapters) recorded for
    /// `owner` on `destination`, in key order: every file a copy of the
    /// edition's recording there needs.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn audio_keys_of(&self, destination: &str, owner: &FileOwner) -> Result<Vec<String>> {
        let rows = edition_files::Entity::find()
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::OwnerKind.eq(owner.kind()))
            .filter(edition_files::Column::OwnerId.eq(owner.id()))
            .filter(edition_files::Column::Role.is_in([
                FileRole::Audio.as_str(),
                FileRole::Part.as_str(),
                FileRole::Chapter.as_str(),
            ]))
            .order_by_asc(edition_files::Column::StorageKey)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows.into_iter().map(|row| row.storage_key).collect())
    }

    /// True when another live edition's file is recorded, on any
    /// destination, at the name `stem` (a planned key without its
    /// extension): the key `stem` itself, `stem` with an extension in the
    /// same folder (`stem.cue`), or one of its part files
    /// (`stem — Part 1.m4b`). Another edition's files already live under
    /// that name, so an acquire of `owner` is refused rather than overwrite
    /// them. A longer name that only starts with `stem` (`stem Peaks.m4b`)
    /// is not the same name. A branch title's file, or a file of an edition
    /// no longer in the library, does not count: an acquire takes those
    /// keys over, as [`LibraryStore::claim_edition_file`] does.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn name_held_by_another_owner(&self, stem: &str, owner: &FileOwner) -> Result<bool> {
        use sea_orm::sea_query::LikeExpr;
        use sea_orm::PaginatorTrait as _;
        let base = like_escaped(stem);
        let with_extension = Condition::all()
            .add(
                edition_files::Column::StorageKey
                    .like(LikeExpr::new(format!("{base}.%")).escape('\\')),
            )
            .add(
                edition_files::Column::StorageKey
                    .not_like(LikeExpr::new(format!("{base}.%/%")).escape('\\')),
            );
        let part = Condition::all()
            .add(
                edition_files::Column::StorageKey
                    .like(LikeExpr::new(format!("{base} — %")).escape('\\')),
            )
            .add(
                edition_files::Column::StorageKey
                    .not_like(LikeExpr::new(format!("{base} — %/%")).escape('\\')),
            );
        let named = Condition::any()
            .add(edition_files::Column::StorageKey.eq(stem))
            .add(with_extension)
            .add(part);
        let mine = Condition::all()
            .add(edition_files::Column::OwnerKind.eq(owner.kind()))
            .add(edition_files::Column::OwnerId.eq(owner.id()));
        let live_edition = Condition::all()
            .add(edition_files::Column::OwnerKind.eq("edition"))
            .add(
                edition_files::Column::OwnerId.in_subquery(
                    Query::select()
                        .column(editions::Column::Id)
                        .from(editions::Entity)
                        .to_owned(),
                ),
            );
        let others = edition_files::Entity::find()
            .filter(named)
            .filter(mine.not())
            .filter(live_edition)
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(others > 0)
    }
}

/// `text` with the `LIKE` wildcards (`%`, `_`) and the escape character
/// itself escaped with `\`, for a pattern matched with that escape.
pub(crate) fn like_escaped(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_the_extension() {
        assert_eq!(FileRole::for_key("A/T/T.m4b", None), FileRole::Audio);
        assert_eq!(
            FileRole::for_key("A/T/T - 02.mp3", Some("A/T/T - 01.mp3")),
            FileRole::Part
        );
        assert_eq!(FileRole::for_key("A/T/T.PDF", None), FileRole::Pdf);
        assert_eq!(FileRole::for_key("A/T/cover.jpg", None), FileRole::Cover);
        assert_eq!(FileRole::for_key("A/T/T.vtt", None), FileRole::Transcript);
        assert_eq!(FileRole::for_key("A/T/T.epub", None), FileRole::Epub);
        assert_eq!(FileRole::for_key("A/T/T.cue", None), FileRole::Sidecar);
        assert_eq!(FileRole::for_key("A/T/README", None), FileRole::Sidecar);
    }

    #[test]
    fn the_better_copy_is_lossless_then_higher_bitrate() {
        let probe = |lossless, bitrate_kbps| FileProbe {
            content_type: "audio/mp4".into(),
            bitrate_kbps,
            lossless,
            source_store: None,
        };
        // Lossless beats lossy whatever the bitrates say.
        assert!(probe(true, Some(700)).is_better_than(&probe(false, Some(900))));
        assert!(!probe(false, Some(900)).is_better_than(&probe(true, Some(700))));
        assert!(probe(true, None).is_better_than(&probe(false, Some(64))));
        // Within a kind the higher bitrate wins, and equals keep what is there.
        assert!(probe(false, Some(128)).is_better_than(&probe(false, Some(64))));
        assert!(!probe(false, Some(64)).is_better_than(&probe(false, Some(128))));
        assert!(!probe(false, Some(64)).is_better_than(&probe(false, Some(64))));
        // An unknown bitrate never replaces a known one.
        assert!(probe(false, Some(32)).is_better_than(&probe(false, None)));
        assert!(!probe(false, None).is_better_than(&probe(false, Some(32))));
        assert!(!probe(false, None).is_better_than(&probe(false, None)));
    }

    #[test]
    fn stored_spellings_round_trip() {
        for role in FileRole::ALL {
            assert_eq!(FileRole::parse(role.as_str()), role);
        }
        assert_eq!(FileRole::parse("unknown"), FileRole::Sidecar);
        assert_eq!(
            FileOwner::from_row("edition", "e1".into()),
            Some(FileOwner::Edition("e1".into()))
        );
        assert_eq!(FileOwner::from_row("x", "e1".into()), None);
    }
}
