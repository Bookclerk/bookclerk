//! Chapters of an edition, resolved from claims
//! (`docs/adr/metadata-claims-and-file-sync.md`, decision 1; chapters are
//! its first field, `docs/adr/edition-chapters.md`).
//!
//! Every source of chapters adds a claim and none writes the chapters
//! directly: a store's markers, a metadata provider's layout, Bookclerk's
//! own chapters aligned to the audio during acquire, and an
//! Administrator's edit. One resolver, a pure function of an edition's
//! claims ([`resolve_chapters`]), picks the list the edition plays, and
//! the pick is written to `edition_chapters`.
//!
//! A chapter list is stored once, named by the SHA-256 of its canonical
//! encoding ([`ChapterDigest`]): its entries go to `chapter_list_entries`
//! in as many batches as the database plugin needs, then one batch writes
//! the `chapter_lists` row that marks the list complete, the claim and
//! nothing else. The resolution follows in its own batch, guarded on the
//! claim revision it read, so a resolution made from older claims never
//! replaces a newer one. A change publishes `edition_updated` for the
//! edition with `fields: ["chapters"]`.

use std::fmt;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use sha2::{Digest as _, Sha256};

use super::{now_str, LibraryStore};
use crate::control_plane::{batch, read_paged};
use crate::entities::{chapter_list_entries, chapter_lists, edition_chapters, metadata_claims};
use crate::error::{EntityKind, LibraryError, Result};
use crate::store::write_batches::WriteBatches;

/// Most chapters one list holds, the same cap as a metadata file's
/// (`maxMetadataFileChapters` in `plugin.capnp`).
pub const MAX_CHAPTERS: usize = 2000;

/// Longest chapter title stored, in UTF-8 bytes. A machine source's longer
/// title is cut at a character boundary; an edit's is refused.
pub const MAX_CHAPTER_TITLE_BYTES: usize = 512;

/// Deepest nesting stored; 0 is a top-level chapter. A machine source's
/// deeper chapter is lifted to this depth; an edit's is refused.
pub const MAX_CHAPTER_DEPTH: u32 = 7;

/// Latest chapter start stored, in milliseconds: sixty days, the playback
/// core's longest position ([`crate::MAX_PLAYBACK_MS`]).
pub const MAX_CHAPTER_START_MS: u64 = crate::store::playback::MAX_PLAYBACK_MS;

/// How many times a resolution is computed again when a newer claim lands
/// while it is being written.
const RESOLVE_ATTEMPTS: usize = 4;

/// The `subject_kind` of an edition's claims.
const SUBJECT_EDITION: &str = "edition";

/// The `field` of chapter claims.
const FIELD_CHAPTERS: &str = "chapters";

/// One chapter as a source states it, before it is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterInput {
    /// Chapter title.
    pub title: String,
    /// Start, in milliseconds from the start of the edition's timeline.
    pub start_ms: u64,
    /// Nesting depth; 0 for a top-level chapter.
    pub depth: u32,
}

/// One chapter of a checked list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct Chapter {
    /// Chapter title; never empty, at most 512 bytes, no control
    /// characters.
    #[schema(example = "Chapter 1: The Boy Who Lived")]
    pub title: String,
    /// Start, in milliseconds from the start of the edition's timeline.
    #[schema(example = 33_000)]
    pub start_ms: u64,
    /// Nesting depth; 0 for a top-level chapter, at most 7. A chapter at
    /// depth n + 1 is nested under the nearest earlier chapter at depth n.
    #[schema(example = 0)]
    pub depth: u32,
}

/// One chapter as a player seeks between them: where it starts and ends
/// on the edition's timeline ([`ChapterList::spans`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct ChapterSpan {
    /// Chapter title.
    #[schema(example = "Chapter 1: The Boy Who Lived")]
    pub title: String,
    /// Start, in milliseconds from the start of the edition's timeline.
    #[schema(example = 33_000)]
    pub start_ms: u64,
    /// End, in milliseconds: the next chapter's start, or the end of the
    /// timeline for the last chapter; never before `start_ms`.
    #[schema(example = 1_215_000)]
    pub end_ms: u64,
    /// Nesting depth; 0 for a top-level chapter. A chapter at depth n + 1
    /// is nested under the nearest earlier chapter at depth n.
    #[schema(example = 0)]
    pub depth: u32,
}

/// How strictly [`ChapterList::new`] reads its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapterCheck {
    /// A person's edit: anything outside the rules is refused, so what is
    /// stored is exactly what they sent.
    Strict,
    /// A machine source: control characters are dropped, titles trimmed
    /// and cut to [`MAX_CHAPTER_TITLE_BYTES`], an empty title becomes
    /// "Chapter n", and a depth that skips a level or passes
    /// [`MAX_CHAPTER_DEPTH`] is lifted. Starts out of order and too many
    /// chapters are still refused, because no repair keeps their meaning.
    Lenient,
}

/// A checked chapter list, in pre-order with starts never decreasing.
///
/// Every list satisfies: at most [`MAX_CHAPTERS`] chapters; each title
/// non-empty, at most [`MAX_CHAPTER_TITLE_BYTES`] bytes, free of control
/// characters and of leading or trailing white space; each start at most
/// [`MAX_CHAPTER_START_MS`] and no earlier than the one before; the first
/// depth 0 and each later depth at most one more than the one before and
/// at most [`MAX_CHAPTER_DEPTH`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChapterList(Vec<Chapter>);

impl ChapterList {
    /// Checks `input` into a list.
    ///
    /// # Arguments
    ///
    /// * `input` - The chapters in pre-order.
    /// * `check` - Whether to refuse or repair what breaks a rule.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] naming the first chapter that
    /// breaks a rule [`ChapterCheck`] does not repair.
    pub fn new(input: Vec<ChapterInput>, check: ChapterCheck) -> Result<Self> {
        if input.len() > MAX_CHAPTERS {
            return Err(LibraryError::Invalid(format!(
                "a chapter list holds at most {MAX_CHAPTERS} chapters"
            )));
        }
        let mut out: Vec<Chapter> = Vec::with_capacity(input.len());
        for (index, chapter) in input.into_iter().enumerate() {
            let number = index + 1;
            let title = match check {
                ChapterCheck::Strict => strict_title(&chapter.title, number)?,
                ChapterCheck::Lenient => lenient_title(&chapter.title, number),
            };
            if chapter.start_ms > MAX_CHAPTER_START_MS {
                return Err(LibraryError::Invalid(format!(
                    "chapter {number} starts after {MAX_CHAPTER_START_MS} ms"
                )));
            }
            let previous = out.last();
            if previous.is_some_and(|p| chapter.start_ms < p.start_ms) {
                return Err(LibraryError::Invalid(format!(
                    "chapter {number} starts before the chapter above it"
                )));
            }
            let allowed = previous.map_or(0, |p| p.depth.saturating_add(1).min(MAX_CHAPTER_DEPTH));
            let depth = match check {
                ChapterCheck::Strict if chapter.depth > allowed => {
                    return Err(LibraryError::Invalid(format!(
                        "chapter {number} is at depth {}, at most {allowed} is allowed there",
                        chapter.depth
                    )));
                }
                ChapterCheck::Strict => chapter.depth,
                ChapterCheck::Lenient => chapter.depth.min(allowed),
            };
            out.push(Chapter {
                title,
                start_ms: chapter.start_ms,
                depth,
            });
        }
        Ok(Self(out))
    }

    /// The chapters, in pre-order.
    #[must_use]
    pub fn chapters(&self) -> &[Chapter] {
        &self.0
    }

    /// True when the list has no chapters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The chapters as spans a player can seek between: in pre-order, each
    /// ending where the next one starts, as Audiobookshelf flattens a
    /// tree, and the last ending at `end_ms`.
    ///
    /// # Arguments
    ///
    /// * `end_ms` - Where the timeline ends, in milliseconds. `None`, or a
    ///   value before the last chapter's start, ends the last chapter at
    ///   its own start.
    ///
    /// # Returns
    ///
    /// One span per chapter, starts non-decreasing, every end at or after
    /// its start.
    #[must_use]
    pub fn spans(&self, end_ms: Option<u64>) -> Vec<ChapterSpan> {
        self.0
            .iter()
            .enumerate()
            .map(|(index, chapter)| {
                let end = self
                    .0
                    .get(index + 1)
                    .map_or(end_ms.unwrap_or(0), |next| next.start_ms)
                    .max(chapter.start_ms);
                ChapterSpan {
                    title: chapter.title.clone(),
                    start_ms: chapter.start_ms,
                    end_ms: end,
                    depth: chapter.depth,
                }
            })
            .collect()
    }

    /// The list's digest: SHA-256 over each chapter's start (8 bytes, big
    /// endian), depth (4 bytes), title length (4 bytes) and title bytes,
    /// in order.
    #[must_use]
    pub fn digest(&self) -> ChapterDigest {
        let mut hash = Sha256::new();
        hash.update(b"bookclerk-chapters-v1\0");
        for chapter in &self.0 {
            hash.update(chapter.start_ms.to_be_bytes());
            hash.update(chapter.depth.to_be_bytes());
            // A title is at most MAX_CHAPTER_TITLE_BYTES, so its length
            // always fits; the fallback is never taken.
            let len = u32::try_from(chapter.title.len()).unwrap_or(u32::MAX);
            hash.update(len.to_be_bytes());
            hash.update(chapter.title.as_bytes());
        }
        ChapterDigest(hex::encode(hash.finalize()))
    }
}

/// An edit's title, refused when it breaks a rule.
fn strict_title(title: &str, number: usize) -> Result<String> {
    if title.chars().any(char::is_control) {
        return Err(LibraryError::Invalid(format!(
            "chapter {number}'s title has a control character"
        )));
    }
    if title.trim() != title || title.is_empty() {
        return Err(LibraryError::Invalid(format!(
            "chapter {number}'s title is empty or starts or ends with white space"
        )));
    }
    if title.len() > MAX_CHAPTER_TITLE_BYTES {
        return Err(LibraryError::Invalid(format!(
            "chapter {number}'s title is over {MAX_CHAPTER_TITLE_BYTES} bytes"
        )));
    }
    Ok(title.to_string())
}

/// A machine source's title, repaired: control characters dropped, white
/// space trimmed, cut at a character boundary, "Chapter n" when nothing is
/// left.
fn lenient_title(title: &str, number: usize) -> String {
    let clean: String = title.chars().filter(|c| !c.is_control()).collect();
    let trimmed = clean.trim();
    let mut end = trimmed.len().min(MAX_CHAPTER_TITLE_BYTES);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let cut = trimmed[..end].trim_end();
    if cut.is_empty() {
        format!("Chapter {number}")
    } else {
        cut.to_string()
    }
}

/// Lower-case hex SHA-256 naming one stored chapter list
/// ([`ChapterList::digest`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChapterDigest(String);

impl ChapterDigest {
    /// Reads a stored digest, refusing anything but 64 lower-case hex
    /// characters.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        (stored.len() == 64
            && stored
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| Self(stored.to_string()))
    }

    /// The digest as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChapterDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The kind of source a chapter claim came from (`source_kind`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ChapterSourceKind {
    /// An Administrator's edit in Bookclerk.
    Person,
    /// A change found outside Bookclerk: a file it did not write, or an
    /// edit in an integrated tool. Nothing records these yet.
    OutsideEdit,
    /// Bookclerk's own chapters, aligned to the audio during acquire.
    Processed,
    /// A metadata provider's chapter layout.
    Provider,
    /// The markers a store returned with the audio.
    Listing,
    /// Chapters read from a file as first imported. Nothing records these
    /// yet.
    File,
    /// The titles of the parts a store delivered, one chapter per part.
    PartNames,
}

impl ChapterSourceKind {
    /// Every kind, in the order the stored values are documented.
    pub const ALL: [Self; 7] = [
        Self::Person,
        Self::OutsideEdit,
        Self::Processed,
        Self::Provider,
        Self::Listing,
        Self::File,
        Self::PartNames,
    ];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::OutsideEdit => "outside_edit",
            Self::Processed => "processed",
            Self::Provider => "provider",
            Self::Listing => "listing",
            Self::File => "file",
            Self::PartNames => "part_names",
        }
    }

    /// Reads a stored spelling.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == stored)
    }
}

/// Where a machine source ranks within its kind (`quality`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimQuality {
    /// Not ranked within its kind: a person's edit, an outside edit,
    /// Bookclerk's own processing, a file, part names.
    None,
    /// A provider found the edition by a strong identifier and marks the
    /// offsets exact.
    StrongAccurate,
    /// A provider found the edition by a strong identifier; the offsets
    /// are estimates.
    StrongEstimated,
    /// A provider found the edition by a weak identifier or a search.
    Weak,
    /// A store's markers that it says are chapters.
    Chapters,
    /// A store's markers that it says are track or part boundaries.
    Tracks,
    /// A store's markers it says nothing about.
    Unknown,
}

impl ClaimQuality {
    /// Every quality, in the order the stored values are documented.
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::StrongAccurate,
        Self::StrongEstimated,
        Self::Weak,
        Self::Chapters,
        Self::Tracks,
        Self::Unknown,
    ];

    /// The stored spelling; empty for [`Self::None`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::StrongAccurate => "strong_accurate",
            Self::StrongEstimated => "strong_estimated",
            Self::Weak => "weak",
            Self::Chapters => "chapters",
            Self::Tracks => "tracks",
            Self::Unknown => "unknown",
        }
    }

    /// Reads a stored spelling.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.as_str() == stored)
    }
}

/// Who states a chapter claim, as a writer names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChapterSource {
    /// An Administrator's edit. One per edition: a newer edit replaces it.
    Person {
        /// The Administrator (`users.id`); `None` for the operator.
        user_id: Option<i64>,
    },
    /// Bookclerk's chapters aligned to the audio during acquire.
    Processed {
        /// The claim whose titles and starts were aligned.
        input: Option<ChapterDigest>,
    },
    /// A metadata provider's layout.
    Provider {
        /// The provider's PluginKey.
        plugin: String,
        /// How it found the edition and whether its offsets are exact.
        quality: ProviderQuality,
    },
    /// A store's markers.
    Listing {
        /// The storefront's PluginKey.
        plugin: String,
        /// What the store says the markers are.
        kind: MarkerKind,
    },
    /// The titles of a store's parts.
    PartNames {
        /// The storefront's PluginKey.
        plugin: String,
    },
}

/// How a provider found the edition, and how exact its offsets are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderQuality {
    /// Found by a strong identifier; offsets marked exact.
    StrongAccurate,
    /// Found by a strong identifier; offsets estimated.
    StrongEstimated,
    /// Found by a weak identifier or a search.
    Weak,
}

/// What a store says its markers are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    /// Literary chapters.
    Chapters,
    /// Track or part boundaries.
    Tracks,
    /// The store did not say.
    Unknown,
}

impl ChapterSource {
    /// The stored `(source_kind, source_id, quality)` of this source.
    fn key(&self) -> (ChapterSourceKind, &str, ClaimQuality) {
        match self {
            Self::Person { .. } => (ChapterSourceKind::Person, "", ClaimQuality::None),
            Self::Processed { .. } => (ChapterSourceKind::Processed, "", ClaimQuality::None),
            Self::Provider { plugin, quality } => (
                ChapterSourceKind::Provider,
                plugin.as_str(),
                match quality {
                    ProviderQuality::StrongAccurate => ClaimQuality::StrongAccurate,
                    ProviderQuality::StrongEstimated => ClaimQuality::StrongEstimated,
                    ProviderQuality::Weak => ClaimQuality::Weak,
                },
            ),
            Self::Listing { plugin, kind } => (
                ChapterSourceKind::Listing,
                plugin.as_str(),
                match kind {
                    MarkerKind::Chapters => ClaimQuality::Chapters,
                    MarkerKind::Tracks => ClaimQuality::Tracks,
                    MarkerKind::Unknown => ClaimQuality::Unknown,
                },
            ),
            Self::PartNames { plugin } => (
                ChapterSourceKind::PartNames,
                plugin.as_str(),
                ClaimQuality::None,
            ),
        }
    }
}

/// One stored chapter claim, without its list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterClaim {
    /// The kind of source.
    pub source_kind: ChapterSourceKind,
    /// Which source of that kind (a PluginKey; empty for a person's edit
    /// and for Bookclerk's processing).
    pub source_id: String,
    /// Where a machine source ranks within its kind.
    pub quality: ClaimQuality,
    /// The claimed list.
    pub digest: ChapterDigest,
    /// How many chapters the list has; `None` while its write is
    /// unfinished.
    pub chapter_count: Option<u64>,
    /// Length of the timeline the list was measured on, in milliseconds.
    pub runtime_ms: Option<u64>,
    /// For a processed claim, the list it was derived from.
    pub input_digest: Option<ChapterDigest>,
    /// The edition's claim counter when this claim last changed.
    pub revision: i64,
    /// When an Administrator pinned it (RFC 3339).
    pub pinned_at: Option<String>,
    /// The person whose edit it is.
    pub created_by_user_id: Option<i64>,
    /// When it last changed (RFC 3339).
    pub updated_at: String,
}

/// Where a machine claim ranks for chapters: lower wins
/// (`docs/adr/edition-chapters.md`, decision 2).
/// `None` for a claim that is not a machine source.
fn machine_rank(claim: &ChapterClaim) -> Option<u8> {
    match claim.source_kind {
        ChapterSourceKind::Person | ChapterSourceKind::OutsideEdit => None,
        ChapterSourceKind::Processed => Some(0),
        ChapterSourceKind::Provider => Some(match claim.quality {
            ClaimQuality::StrongAccurate => 1,
            ClaimQuality::None
            | ClaimQuality::StrongEstimated
            | ClaimQuality::Weak
            | ClaimQuality::Chapters
            | ClaimQuality::Tracks
            | ClaimQuality::Unknown => 4,
        }),
        ChapterSourceKind::Listing => Some(match claim.quality {
            ClaimQuality::Chapters => 2,
            ClaimQuality::None
            | ClaimQuality::StrongAccurate
            | ClaimQuality::StrongEstimated
            | ClaimQuality::Weak
            | ClaimQuality::Unknown => 5,
            ClaimQuality::Tracks => 6,
        }),
        ChapterSourceKind::File => Some(3),
        ChapterSourceKind::PartNames => Some(7),
    }
}

/// Picks the claim an edition's chapters resolve to.
///
/// In order: the claim an Administrator pinned; the newest edit made in
/// Bookclerk; the newest outside edit; then the best machine source by
/// the chapters machine ranking (`docs/adr/edition-chapters.md`, decision
/// 2), ties broken by source id. A claim whose list write is
/// unfinished is never picked, and an empty list is picked only from a
/// person, who cleared the chapters on purpose. The answer depends only on
/// the claims, never on the order they are given in.
#[must_use]
pub fn resolve_chapters(claims: &[ChapterClaim]) -> Option<&ChapterClaim> {
    let complete = || claims.iter().filter(|c| c.chapter_count.is_some());
    let filled = |c: &&ChapterClaim| c.chapter_count.is_some_and(|n| n > 0);
    let newest = |kind: ChapterSourceKind, allow_empty: bool| {
        complete()
            .filter(|c| c.source_kind == kind)
            .filter(|c| allow_empty || filled(c))
            .max_by(|a, b| {
                a.revision
                    .cmp(&b.revision)
                    .then_with(|| b.source_id.cmp(&a.source_id))
            })
    };
    if let Some(pinned) = complete()
        .filter(|c| c.pinned_at.is_some())
        .filter(|c| c.source_kind == ChapterSourceKind::Person || filled(c))
        .max_by(|a, b| {
            a.revision
                .cmp(&b.revision)
                .then_with(|| b.source_id.cmp(&a.source_id))
        })
    {
        return Some(pinned);
    }
    if let Some(edit) = newest(ChapterSourceKind::Person, true) {
        return Some(edit);
    }
    if let Some(edit) = newest(ChapterSourceKind::OutsideEdit, false) {
        return Some(edit);
    }
    complete()
        .filter(filled)
        .filter_map(|c| machine_rank(c).map(|rank| (rank, c)))
        .min_by(|(ra, a), (rb, b)| {
            ra.cmp(rb)
                .then_with(|| a.source_kind.as_str().cmp(b.source_kind.as_str()))
                .then_with(|| a.source_id.cmp(&b.source_id))
        })
        .map(|(_, c)| c)
}

/// A claim to record ([`LibraryStore::record_chapter_claim`]).
#[derive(Debug, Clone)]
pub struct ChapterClaimWrite {
    /// Who states it.
    pub source: ChapterSource,
    /// The chapters.
    pub list: ChapterList,
    /// Length of the timeline the chapters were measured on, in
    /// milliseconds, when known.
    pub runtime_ms: Option<u64>,
}

/// The chapters an edition resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionChapters {
    /// The chapters; empty when an Administrator cleared them.
    pub list: ChapterList,
    /// The winning claim's source kind.
    pub source_kind: ChapterSourceKind,
    /// The winning claim's source id.
    pub source_id: String,
    /// Length of the timeline the list was measured on, in milliseconds.
    pub runtime_ms: Option<u64>,
}

/// What a chapter write changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChaptersOutcome {
    /// The edition's chapters now come from another list.
    Changed,
    /// The claims changed but the edition plays the same list.
    Unchanged,
}

/// Maps a stored claim row, with its list's chapter count when the list
/// is complete. `None` for a row whose stored values are not ours.
fn map_claim(row: metadata_claims::Model, count: Option<u64>) -> Option<ChapterClaim> {
    Some(ChapterClaim {
        source_kind: ChapterSourceKind::parse(&row.source_kind)?,
        source_id: row.source_id,
        quality: ClaimQuality::parse(&row.quality)?,
        digest: ChapterDigest::parse(&row.value_digest)?,
        chapter_count: count,
        runtime_ms: row.runtime_ms.and_then(|ms| u64::try_from(ms).ok()),
        input_digest: row.input_digest.as_deref().and_then(ChapterDigest::parse),
        revision: row.revision,
        pinned_at: row.pinned_at,
        created_by_user_id: row.created_by_user_id,
        updated_at: row.updated_at,
    })
}

/// A non-negative stored integer from a `u64`; values past `i64::MAX`
/// cannot occur under the list's caps and are stored as `i64::MAX`.
fn stored(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The SQL that names the edition's next claim revision, binding the
/// edition id once.
const NEXT_REVISION: &str = "(SELECT COALESCE(MAX(revision), 0) + 1 FROM metadata_claims \
     WHERE subject_kind = 'edition' AND subject_id = ? AND field = 'chapters')";

impl LibraryStore {
    /// Records one source's chapters for an edition and resolves the
    /// edition's chapters again.
    ///
    /// The list is stored first when it is new. The claim then replaces
    /// the source's earlier one; a claim that states the same list and
    /// runtime as before changes nothing, so recording the same chapters
    /// on every acquire costs one read.
    ///
    /// # Arguments
    ///
    /// * `edition_id` - The edition (`editions.id`).
    /// * `write` - The claim.
    ///
    /// # Returns
    ///
    /// Whether the edition's chapters changed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] for an unknown edition, or the
    /// database error.
    pub async fn record_chapter_claim(
        &self,
        edition_id: &str,
        write: &ChapterClaimWrite,
    ) -> Result<ChaptersOutcome> {
        self.require_edition(edition_id).await?;
        let digest = write.list.digest();
        let (kind, source_id, quality) = write.source.key();
        let existing = metadata_claims::Entity::find_by_id((
            SUBJECT_EDITION.to_string(),
            edition_id.to_string(),
            FIELD_CHAPTERS.to_string(),
            kind.as_str().to_string(),
            source_id.to_string(),
            String::new(),
        ))
        .one(&self.db)
        .await
        .map_err(LibraryError::Orm)?;
        let input = match &write.source {
            ChapterSource::Processed { input } => input.as_ref(),
            ChapterSource::Person { .. }
            | ChapterSource::Provider { .. }
            | ChapterSource::Listing { .. }
            | ChapterSource::PartNames { .. } => None,
        };
        let user = match &write.source {
            ChapterSource::Person { user_id } => *user_id,
            ChapterSource::Processed { .. }
            | ChapterSource::Provider { .. }
            | ChapterSource::Listing { .. }
            | ChapterSource::PartNames { .. } => None,
        };
        let runtime = write.runtime_ms.map(stored);
        let same = existing.as_ref().is_some_and(|row| {
            row.value_digest == digest.as_str()
                && row.runtime_ms == runtime
                && row.quality == quality.as_str()
                && row.input_digest.as_deref() == input.map(ChapterDigest::as_str)
                && row.created_by_user_id == user
        });
        if same && self.chapter_list_complete(&digest).await? {
            return Ok(ChaptersOutcome::Unchanged);
        }
        self.store_chapter_list(&write.list, &digest).await?;
        let now = now_str();
        let statements = vec![
            batch::exec(
                "INSERT OR IGNORE INTO chapter_lists (digest, chapter_count, created_at) \
                 VALUES (?, ?, ?)",
                vec![
                    batch::text(digest.as_str()),
                    batch::int(stored(
                        u64::try_from(write.list.chapters().len()).unwrap_or(0),
                    )),
                    batch::text(&now),
                ],
            ),
            // The source's earlier claim is replaced; when it had none,
            // the update matches nothing and the insert adds it.
            batch::exec(
                &format!(
                    "UPDATE metadata_claims SET quality = ?, value_digest = ?, runtime_ms = ?, \
                     input_digest = ?, created_by_user_id = ?, updated_at = ?, \
                     revision = {NEXT_REVISION} \
                     WHERE subject_kind = 'edition' AND subject_id = ? AND field = 'chapters' \
                     AND source_kind = ? AND source_id = ? AND locale = ''"
                ),
                vec![
                    batch::text(quality.as_str()),
                    batch::text(digest.as_str()),
                    batch::int_opt(runtime),
                    batch::text_opt(input.map(ChapterDigest::as_str)),
                    batch::int_opt(user),
                    batch::text(&now),
                    batch::text(edition_id),
                    batch::text(edition_id),
                    batch::text(kind.as_str()),
                    batch::text(source_id),
                ],
            ),
            batch::exec(
                &format!(
                    "INSERT OR IGNORE INTO metadata_claims (subject_kind, subject_id, field, \
                     source_kind, source_id, locale, quality, value_digest, runtime_ms, \
                     input_digest, revision, created_by_user_id, updated_at) \
                     SELECT 'edition', ?, 'chapters', ?, ?, '', ?, ?, ?, ?, {NEXT_REVISION}, ?, ?"
                ),
                vec![
                    batch::text(edition_id),
                    batch::text(kind.as_str()),
                    batch::text(source_id),
                    batch::text(quality.as_str()),
                    batch::text(digest.as_str()),
                    batch::int_opt(runtime),
                    batch::text_opt(input.map(ChapterDigest::as_str)),
                    batch::text(edition_id),
                    batch::int_opt(user),
                    batch::text(&now),
                ],
            ),
        ];
        let operation_id = format!("chapter-claim-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        self.resolve_edition_chapters(edition_id).await
    }

    /// Removes an Administrator's edit of an edition's chapters, so they
    /// resolve from the other sources again.
    ///
    /// # Returns
    ///
    /// Whether the edition's chapters changed; [`ChaptersOutcome::Unchanged`]
    /// when there was no edit.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] for an unknown edition, or the
    /// database error.
    pub async fn remove_chapter_edit(&self, edition_id: &str) -> Result<ChaptersOutcome> {
        self.require_edition(edition_id).await?;
        let statements = vec![
            // The newest remaining claim moves past the edit's revision
            // first, so the edition's highest revision still grows and a
            // resolution made while the edit existed cannot be written.
            batch::exec(
                &format!(
                    "UPDATE metadata_claims SET revision = {NEXT_REVISION} \
                     WHERE subject_kind = 'edition' AND subject_id = ? AND field = 'chapters' \
                     AND source_kind <> 'person' AND revision = (SELECT MAX(revision) \
                     FROM metadata_claims WHERE subject_kind = 'edition' AND subject_id = ? \
                     AND field = 'chapters' AND source_kind <> 'person')"
                ),
                vec![
                    batch::text(edition_id),
                    batch::text(edition_id),
                    batch::text(edition_id),
                ],
            ),
            batch::exec(
                "DELETE FROM metadata_claims WHERE subject_kind = 'edition' AND subject_id = ? \
                 AND field = 'chapters' AND source_kind = 'person'",
                vec![batch::text(edition_id)],
            ),
        ];
        let operation_id = format!("chapter-edit-remove-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        self.resolve_edition_chapters(edition_id).await
    }

    /// Pins one of an edition's chapter claims, so it wins over every
    /// other source, or removes the pin.
    ///
    /// At most one claim is pinned: pinning one unpins the rest in the same
    /// batch. A claim that is pinned or unpinned moves to the edition's
    /// next revision.
    ///
    /// # Arguments
    ///
    /// * `edition_id` - The edition.
    /// * `target` - The claim to pin, by source kind and source id;
    ///   `None` removes the pin.
    /// * `user_id` - The Administrator; `None` for the operator.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] for an unknown edition or a
    /// claim it does not have, or the database error.
    pub async fn pin_chapter_claim(
        &self,
        edition_id: &str,
        target: Option<(ChapterSourceKind, &str)>,
        user_id: Option<i64>,
    ) -> Result<ChaptersOutcome> {
        self.require_edition(edition_id).await?;
        let now = now_str();
        let mut statements = vec![batch::exec(
            &format!(
                "UPDATE metadata_claims SET pinned_at = NULL, pinned_by_user_id = NULL, \
                 revision = {NEXT_REVISION} \
                 WHERE subject_kind = 'edition' AND subject_id = ? AND field = 'chapters' \
                 AND pinned_at IS NOT NULL"
            ),
            vec![batch::text(edition_id), batch::text(edition_id)],
        )];
        if let Some((kind, source_id)) = target {
            let exists = metadata_claims::Entity::find_by_id((
                SUBJECT_EDITION.to_string(),
                edition_id.to_string(),
                FIELD_CHAPTERS.to_string(),
                kind.as_str().to_string(),
                source_id.to_string(),
                String::new(),
            ))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some();
            if !exists {
                return Err(LibraryError::not_found(
                    EntityKind::MetadataClaim,
                    format!("{}:{source_id}", kind.as_str()),
                ));
            }
            statements.push(batch::exec(
                &format!(
                    "UPDATE metadata_claims SET pinned_at = ?, pinned_by_user_id = ?, \
                     revision = {NEXT_REVISION} \
                     WHERE subject_kind = 'edition' AND subject_id = ? AND field = 'chapters' \
                     AND source_kind = ? AND source_id = ? AND locale = ''"
                ),
                vec![
                    batch::text(&now),
                    batch::int_opt(user_id),
                    batch::text(edition_id),
                    batch::text(edition_id),
                    batch::text(kind.as_str()),
                    batch::text(source_id),
                ],
            ));
        }
        let operation_id = format!("chapter-pin-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        self.resolve_edition_chapters(edition_id).await
    }

    /// An edition's chapter claims, newest first, each with its list's
    /// chapter count.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn chapter_claims(&self, edition_id: &str) -> Result<Vec<ChapterClaim>> {
        use metadata_claims::Column as C;
        let rows = read_paged(
            self,
            metadata_claims::Entity::find()
                .filter(C::SubjectKind.eq(SUBJECT_EDITION))
                .filter(C::SubjectId.eq(edition_id))
                .filter(C::Field.eq(FIELD_CHAPTERS)),
            None,
        )
        .await?;
        let mut claims = Vec::with_capacity(rows.len());
        for row in rows {
            let count = match ChapterDigest::parse(&row.value_digest) {
                Some(digest) => self.chapter_list_count(&digest).await?,
                None => None,
            };
            if let Some(claim) = map_claim(row, count) {
                claims.push(claim);
            }
        }
        claims.sort_by(|a, b| {
            b.revision
                .cmp(&a.revision)
                .then_with(|| a.source_kind.as_str().cmp(b.source_kind.as_str()))
                .then_with(|| a.source_id.cmp(&b.source_id))
        });
        Ok(claims)
    }

    /// The chapters an edition resolved to; `None` when it has none.
    ///
    /// # Errors
    ///
    /// Returns the read error, or [`LibraryError::Other`] when the stored
    /// list is missing or no longer passes its own checks.
    pub async fn edition_chapters(&self, edition_id: &str) -> Result<Option<EditionChapters>> {
        let Some(row) = edition_chapters::Entity::find_by_id(edition_id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let (Some(digest), Some(source_kind)) = (
            ChapterDigest::parse(&row.digest),
            ChapterSourceKind::parse(&row.source_kind),
        ) else {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "edition {edition_id}'s resolved chapters are not readable"
            )));
        };
        let list = self.chapter_list(&digest).await?.ok_or_else(|| {
            LibraryError::Other(anyhow::anyhow!(
                "edition {edition_id}'s chapter list {digest} is missing"
            ))
        })?;
        Ok(Some(EditionChapters {
            list,
            source_kind,
            source_id: row.source_id,
            runtime_ms: row.runtime_ms.and_then(|ms| u64::try_from(ms).ok()),
        }))
    }

    /// A stored chapter list; `None` when no complete list has this
    /// digest.
    ///
    /// # Errors
    ///
    /// Returns the read error, or [`LibraryError::Other`] when the entries
    /// do not match the list's row or its digest.
    pub async fn chapter_list(&self, digest: &ChapterDigest) -> Result<Option<ChapterList>> {
        let Some(count) = self.chapter_list_count(digest).await? else {
            return Ok(None);
        };
        use chapter_list_entries::Column as E;
        let rows = read_paged(
            self,
            chapter_list_entries::Entity::find().filter(E::Digest.eq(digest.as_str())),
            None,
        )
        .await?;
        let damaged = || LibraryError::Other(anyhow::anyhow!("chapter list {digest} is damaged"));
        if u64::try_from(rows.len()).ok() != Some(count) {
            return Err(damaged());
        }
        let mut input = Vec::with_capacity(rows.len());
        for row in rows {
            input.push(ChapterInput {
                title: row.title,
                start_ms: u64::try_from(row.start_ms).map_err(|_| damaged())?,
                depth: u32::try_from(row.depth).map_err(|_| damaged())?,
            });
        }
        let list = ChapterList::new(input, ChapterCheck::Strict).map_err(|_| damaged())?;
        if list.digest() != *digest {
            return Err(damaged());
        }
        Ok(Some(list))
    }

    /// Fails with [`LibraryError::NotFound`] unless the edition exists.
    async fn require_edition(&self, edition_id: &str) -> Result<()> {
        crate::entities::editions::Entity::find_by_id(edition_id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|_| ())
            .ok_or_else(|| LibraryError::not_found(EntityKind::Edition, edition_id))
    }

    /// The chapter count of a complete stored list; `None` when there is
    /// none.
    async fn chapter_list_count(&self, digest: &ChapterDigest) -> Result<Option<u64>> {
        Ok(
            chapter_lists::Entity::find_by_id(digest.as_str().to_string())
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
                .and_then(|row| u64::try_from(row.chapter_count).ok()),
        )
    }

    /// True when a complete list with this digest is stored.
    async fn chapter_list_complete(&self, digest: &ChapterDigest) -> Result<bool> {
        Ok(self.chapter_list_count(digest).await?.is_some())
    }

    /// Writes a list's entries in as many batches as the database plugin
    /// needs; nothing when a complete list with the digest is stored. The
    /// `chapter_lists` row is written later, with the claim, and marks the
    /// list complete. Entries are written with `INSERT OR IGNORE`, so an
    /// interrupted write is finished by the next one.
    async fn store_chapter_list(&self, list: &ChapterList, digest: &ChapterDigest) -> Result<()> {
        if self.chapter_list_complete(digest).await? {
            return Ok(());
        }
        let mut batches = WriteBatches::new(self.db_capabilities(), "chapter list entry");
        for (position, chapter) in list.chapters().iter().enumerate() {
            let statement = batch::exec(
                "INSERT OR IGNORE INTO chapter_list_entries \
                 (digest, position, start_ms, depth, title) VALUES (?, ?, ?, ?, ?)",
                vec![
                    batch::text(digest.as_str()),
                    batch::int(stored(u64::try_from(position).unwrap_or(u64::MAX))),
                    batch::int(stored(chapter.start_ms)),
                    batch::int(i64::from(chapter.depth)),
                    batch::text(&chapter.title),
                ],
            );
            if let Some(full) = batches.push(statement)? {
                self.run_chapter_batch(full).await?;
            }
        }
        if let Some(last) = batches.finish() {
            self.run_chapter_batch(last).await?;
        }
        Ok(())
    }

    /// Runs one batch of chapter list entries.
    async fn run_chapter_batch(
        &self,
        statements: Vec<bookclerk_plugin_abi::TypedDbStatement>,
    ) -> Result<()> {
        let operation_id = format!("chapter-list-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await
            .map(|_| ())
    }

    /// Resolves an edition's chapters from its claims and writes the
    /// result, publishing `edition_updated` when the list changed.
    ///
    /// The write is guarded on the highest claim revision the resolution
    /// read; when a newer claim landed in between, the resolution is
    /// computed again.
    ///
    /// # Errors
    ///
    /// Returns the database error, or [`LibraryError::Conflict`] when
    /// claims kept changing for every attempt.
    pub async fn resolve_edition_chapters(&self, edition_id: &str) -> Result<ChaptersOutcome> {
        for _ in 0..RESOLVE_ATTEMPTS {
            let claims = self.chapter_claims(edition_id).await?;
            let revision = claims.iter().map(|c| c.revision).max().unwrap_or(0);
            let current = edition_chapters::Entity::find_by_id(edition_id.to_string())
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            let winner = resolve_chapters(&claims);
            let changed = match (&current, winner) {
                (None, None) => false,
                (Some(row), Some(w)) => row.digest != w.digest.as_str(),
                (None, Some(_)) | (Some(_), None) => true,
            };
            let now = now_str();
            let guard = "(SELECT COALESCE(MAX(revision), 0) FROM metadata_claims \
                 WHERE subject_kind = 'edition' AND subject_id = ? AND field = 'chapters') = ?";
            let mut statements = Vec::new();
            if changed {
                statements.push(chapters_event(edition_id, revision, &now, guard));
            }
            match winner {
                Some(w) => {
                    statements.push(batch::exec(
                        &format!(
                            "INSERT OR IGNORE INTO edition_chapters (edition_id, digest, \
                             source_kind, source_id, runtime_ms, revision, updated_at) \
                             SELECT ?, ?, ?, ?, ?, ?, ? WHERE {guard}"
                        ),
                        vec![
                            batch::text(edition_id),
                            batch::text(w.digest.as_str()),
                            batch::text(w.source_kind.as_str()),
                            batch::text(&w.source_id),
                            batch::int_opt(w.runtime_ms.map(stored)),
                            batch::int(revision),
                            batch::text(&now),
                            batch::text(edition_id),
                            batch::int(revision),
                        ],
                    ));
                    statements.push(batch::exec(
                        &format!(
                            "UPDATE edition_chapters SET digest = ?, source_kind = ?, \
                             source_id = ?, runtime_ms = ?, revision = ?, updated_at = ? \
                             WHERE edition_id = ? AND {guard}"
                        ),
                        vec![
                            batch::text(w.digest.as_str()),
                            batch::text(w.source_kind.as_str()),
                            batch::text(&w.source_id),
                            batch::int_opt(w.runtime_ms.map(stored)),
                            batch::int(revision),
                            batch::text(&now),
                            batch::text(edition_id),
                            batch::text(edition_id),
                            batch::int(revision),
                        ],
                    ));
                }
                None => statements.push(batch::exec(
                    &format!("DELETE FROM edition_chapters WHERE edition_id = ? AND {guard}"),
                    vec![
                        batch::text(edition_id),
                        batch::text(edition_id),
                        batch::int(revision),
                    ],
                )),
            }
            // A last statement that matches exactly when the guard held,
            // so the reply says whether the resolution was applied.
            statements.push(batch::query(
                &format!("SELECT 1 AS applied WHERE {guard}"),
                vec![batch::text(edition_id), batch::int(revision)],
                1,
            ));
            let last = statements.len() - 1;
            let operation_id = format!("chapter-resolve-{}", uuid::Uuid::new_v4());
            let reply = self
                .execute_host_batch(batch::request(&operation_id, statements))
                .await?;
            if !batch::rows(&reply, last).is_empty() {
                return Ok(if changed {
                    ChaptersOutcome::Changed
                } else {
                    ChaptersOutcome::Unchanged
                });
            }
        }
        Err(LibraryError::Conflict(format!(
            "edition {edition_id}'s chapter claims kept changing while they were resolved"
        )))
    }
}

/// The outbox insert of the `edition_updated` event a chapter change
/// publishes, guarded on the resolution's revision being the newest.
fn chapters_event(
    edition_id: &str,
    revision: i64,
    now: &str,
    guard: &str,
) -> bookclerk_plugin_abi::TypedDbStatement {
    // Every applied change is its own event; a retried batch is replayed
    // from its receipt, not sent again.
    let dedup_key = format!(
        "edition_updated:{edition_id}:chapters:{revision}:{}",
        uuid::Uuid::new_v4()
    );
    let payload =
        serde_json::json!({ "editionId": edition_id, "fields": ["chapters"] }).to_string();
    batch::exec(
        &format!(
            "INSERT OR IGNORE INTO domain_events (\
                id, event_type, schema_version, occurred_at, account_id, source, \
                correlation_id, causation_id, dedup_key, payload, ordering_key, \
                dispatch_state, created_at, wake_pending\
             ) SELECT ?, 'edition_updated', 1, ?, '', 'host', ?, '', ?, ?, ?, 'pending', ?, 1 \
             WHERE NOT EXISTS (\
                SELECT 1 FROM domain_events \
                WHERE queue_name = 'core.events' AND account_id = '' AND source = 'host' \
                  AND event_type = 'edition_updated' AND dedup_key = ?\
             ) AND {guard}"
        ),
        vec![
            batch::text(&uuid::Uuid::new_v4().to_string()),
            batch::text(now),
            batch::text(edition_id),
            batch::text(&dedup_key),
            batch::text(&payload),
            batch::text(edition_id),
            batch::text(now),
            batch::text(&dedup_key),
            batch::text(edition_id),
            batch::int(revision),
        ],
    )
}

impl fmt::Display for ChapterSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
#[path = "chapters_tests.rs"]
mod tests;
