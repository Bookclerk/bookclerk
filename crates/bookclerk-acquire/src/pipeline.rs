//! Acquire pipeline: fetch (plain) → package / metadata → storage.
//!
//! DRM decrypt happens inside content-source plugins; this crate never sees keys.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bookclerk_config::{DestinationId, FileTimestampMode, MultiDestinationMode};
use bookclerk_enrich::{asin_identifier, chapter_info_from_layout, MetadataProviders};
use bookclerk_library::{AcquireStatus, CopyState, FileOwner, FileProbe, LibraryStore};
use bookclerk_media::{
    align_chapter_starts_async, bookclerk_tool_tag, encode_to_mp3, encode_to_opus, fixup_audiobook,
    package_m4b_from_mp3, parse_mp4, track_duration_ms, ChapterAlignOptions, FixupRequest,
    OpusEncodeOptions, PackageM4bRequest,
};
use bookclerk_plugin_abi::MetadataChaptersParams;
use bookclerk_source::{ContentSource, DownloadOptions, FetchOptions, PlainFetch};
use bookclerk_storage::{ObjectMeta, StorageBackend};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cue::{
    apply_start_map_to_chapter_tree, chapters_from_catalog_info_for_plain_audio,
    rebase_chapter_tree_for_plain_audio, write_cue, FlatChapter,
};
use crate::destinations::{AcquireDestination, AcquireDestinations};
use crate::error::{AcquireError, Result};
use crate::library_copies::{
    fetch_library_copy, from_store, CopyWants, LibraryCopyListing, LibraryCopySource, LibraryFetch,
};
use crate::naming::{
    audio_basename, chapter_storage_key_with_folder, sidecar_key, storage_key_with_contexts,
    NamingContext,
};
use crate::reconcile::{find_existing_for_request, StorageIndex};
use crate::record::{recording_copy_destinations, recording_destinations};
use crate::split::split_audio_by_chapters;

/// Request to acquire a single title.
#[derive(Debug, Clone)]
pub struct AcquireRequest {
    /// Download product id (Audible ASIN / Libro ISBN).
    pub asin: String,
    /// The account holding (`holdings.id`) the title is acquired for,
    /// when known: acquire state is written under it. Without one the
    /// state is written to the title `asin` names on `account_id`.
    pub holding_id: Option<String>,
    /// Which store owns this title (plugin id: `audible`, `libro`, …).
    pub source: String,
    /// Store account id that owns this title.
    pub account_id: String,
    /// Display title used for naming and metadata.
    pub title: String,
    /// Author list for naming templates and tags.
    pub authors: Option<String>,
    /// Narrator list for naming templates and tags.
    pub narrators: Option<String>,
    /// Series name for naming templates, when present.
    pub series: Option<String>,
    /// Series position string for naming templates, when present.
    pub series_index: Option<String>,
    /// Source download options (quality, chapter prefs, …).
    pub options: DownloadOptions,
    /// Bookclerk files directory root (`BOOKCLERK_FILES_DIR`).
    pub files_dir: PathBuf,
    /// Scratch directory for encrypted + decrypted temps.
    pub cache_dir: PathBuf,
    /// When true, download even if matching media already exists in storage.
    pub force: bool,
    /// When set, only write prepared audio to these destinations
    /// (`output.multi_destination = refetch_missing`).
    pub write_destinations: Option<Vec<DestinationId>>,
    /// What the destination filters know about the book. When set, the
    /// acquire writes only to the destinations whose filter takes it
    /// ([`AcquireDestinations::for_book`]) and fails before fetching when
    /// none does. `None` writes to every destination given: a host
    /// acquiring for an owner gets destinations the owner already chose.
    pub placement: Option<bookclerk_config::PlacementFacts>,
    /// Durable daemon job id that owns this acquire's scratch directory.
    pub job_id: Option<String>,
    /// Refuse a new scratch dir when registered temps already exceed this.
    pub temp_quota_bytes: Option<u64>,
    /// The `<call number>` to name the title by instead of its edition in
    /// `library`: a host acquiring for a shared node names the title as the
    /// owner does, since its scratch library's edition ids are its own.
    pub call_number: Option<String>,
    /// Likewise for the title's podcast parent, whose folder an episode
    /// may be saved in.
    pub parent_call_number: Option<String>,
    /// Metadata providers asked for the plain-audio catalog and chapter
    /// overlay. The default has none, so plain audio keeps its store tags.
    pub metadata: AcquireMetadata,
    /// The libraries asked for the copy they already hold of the title
    /// before it is fetched from its store. The default asks none.
    pub library_copies: LibraryCopySource,
    /// The destination this request names files for, which translates the
    /// host keys its naming templates build. The default keeps host keys;
    /// the per-destination requests set it for each destination.
    pub destination_keys: crate::keys::DestinationKeys,
}

/// Turns a cover URL a metadata provider named into an image the acquire can
/// embed, without the acquire itself requesting the URL.
///
/// The daemon implements this with its cover service: a `covers` plugin
/// downloads the image and the confined media worker re-encodes it, so what
/// reaches the tagger is a JPEG the host produced.
#[async_trait::async_trait]
pub trait CoverImages: Send + Sync {
    /// The full-size JPEG for `url`, or `None` when it cannot be fetched or
    /// is not an image.
    async fn full_cover(&self, url: &str) -> Option<Vec<u8>>;
}

/// The deployed metadata providers an acquire asks for the plain-audio
/// overlay (catalog tags, cover and chapter layout of an enriched ASIN), and
/// what turns the overlay's cover URL into an image.
#[derive(Clone)]
pub struct AcquireMetadata {
    /// Providers asked for the overlay record.
    providers: Arc<dyn MetadataProviders>,
    /// Fetches the record's cover; without one the overlay has no cover.
    covers: Option<Arc<dyn CoverImages>>,
}

impl AcquireMetadata {
    /// Wraps the providers an acquire should ask.
    ///
    /// # Arguments
    ///
    /// * `providers` - Usually the daemon's or CLI's integration registry.
    #[must_use]
    pub fn new(providers: Arc<dyn MetadataProviders>) -> Self {
        Self {
            providers,
            covers: None,
        }
    }

    /// The same providers, with `covers` fetching the overlay's cover.
    #[must_use]
    pub fn with_covers(mut self, covers: Arc<dyn CoverImages>) -> Self {
        self.covers = Some(covers);
        self
    }

    /// The wrapped providers.
    #[must_use]
    pub fn providers(&self) -> &dyn MetadataProviders {
        self.providers.as_ref()
    }
}

impl Default for AcquireMetadata {
    fn default() -> Self {
        Self::new(Arc::new(bookclerk_enrich::NoMetadataProviders))
    }
}

impl std::fmt::Debug for AcquireMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AcquireMetadata(..)")
    }
}

/// Result after a successful acquire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireResult {
    /// Primary product id (Audible ASIN, ISBN, …) for this job.
    pub asin: String,
    /// Object-storage key written for the primary audio artifact.
    pub storage_key: String,
    /// All object keys written during this acquire (audio + sidecars).
    #[serde(default)]
    pub written_keys: Vec<String>,
    /// True when an existing file was matched and no download ran.
    pub matched_existing: bool,
    /// True when the audio was taken from the copy a library this install
    /// joined already held, instead of fetched from the store.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_library: bool,
    /// How many times this acquire downloaded the title (or its companion
    /// PDF) from the store: 0 for a copy matched in storage or taken from a
    /// joined library, 1 for a store download (even one a better copy from
    /// a joined library replaced afterwards), 2 when that library copy then
    /// failed packaging and the store was asked again. Download pacing
    /// counts each against the account (`bookclerk_library::pacing`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub store_fetches: u32,
    /// Each destination this acquire wrote or found the audio on, and the
    /// ones whose writes failed; recorded as the edition's copies
    /// (`edition_copies`) once the acquire returns. Empty when nothing was
    /// written or checked per destination (an edition already stored
    /// everywhere, or a companion PDF).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copies: Vec<CopyOutcome>,
}

/// Whether a count is zero, so serde leaves [`AcquireResult::store_fetches`]
/// out of a result that never reached the store.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `skip_serializing_if` passes a reference
fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// What one destination ended up with in one acquire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyOutcome {
    /// The destination's id.
    pub destination: DestinationId,
    /// Whether the destination now holds the audio.
    pub result: CopyResult,
}

/// Whether a destination holds an acquire's audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CopyResult {
    /// The destination holds the complete files.
    Stored {
        /// The edition's primary audio key there.
        key: String,
    },
    /// The destination's writes failed; the copy is retried later.
    Failed {
        /// The last write's error.
        error: String,
    },
}

/// Run the acquire pipeline for one book.
///
/// # Arguments
///
/// * `library` - Library store for status updates.
/// * `source` - Content-source plugin used to fetch the title.
/// * `destinations` - Output backends to write packaged audio into.
/// * `req` - Title identity, account, and download options.
///
/// # Returns
///
/// [`AcquireResult`] describing written keys and match status.
///
/// # Errors
///
/// Returns [`AcquireError`] when fetch, package, storage, or library updates fail.
pub async fn acquire_book(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    acquire_book_indexed(library, destinations, req, None, source).await
}

/// Acquire with an optional pre-built [`StorageIndex`] (avoids re-listing storage
/// when liberating many titles). On success, newly written keys are inserted into
/// the index so later books in the same batch can match them.
///
/// Fetch always goes through [`ContentSource::fetch_title`] (Plain only;
/// Audible decrypts Adrm/CENC inside the plugin before returning).
///
/// Every key the run writes on a destination (audio, chapter or part
/// files, sidecars, the companion PDF) is recorded for the book's edition
/// in the library's `edition_files`, with its role, just before it is
/// written, so a host upload of another edition is never granted it.
///
/// A book whose edition another book has already acquired (the same
/// edition bought on two accounts, say) is marked acquired at that book's
/// key without fetching it again.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn acquire_book_indexed(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    mut req: AcquireRequest,
    index: Option<&mut StorageIndex>,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    let placed = placed_destinations(destinations, &req)?;
    let destinations = placed.as_ref().unwrap_or(destinations);
    let owner = file_owner_of_request(library, &req).await?;
    pin_call_number(&mut req, owner.as_ref());
    req.library_copies = req.library_copies.for_one_acquire();
    let recorded = recording_destinations(
        library,
        destinations,
        owner.as_ref(),
        &req.source,
        req.library_copies.stored_from(),
    );
    let copies: Vec<CopyTarget> = destinations
        .copy_plan()
        .into_iter()
        .map(|(copy, targets)| CopyTarget {
            destinations: recording_copy_destinations(
                library,
                &targets,
                owner.as_ref(),
                &req.source,
                req.library_copies.stored_from(),
                &copy,
            ),
            copy,
        })
        .collect();
    // The edition the copies are recorded for (its `FileOwner` id).
    let edition = owner.as_ref().map(FileOwner::id);
    let stored = match edition {
        Some(edition) => stored_copies(library, edition, destinations).await?,
        None => StoredCopies::Untracked,
    };
    let result =
        acquire_book_recorded(library, &recorded, &copies, req, index, source, &stored).await?;
    if let Some(edition) = edition {
        record_copies(library, edition, &result.copies).await?;
    }
    Ok(result)
}

/// Which destinations of an acquire's target set already hold its edition.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StoredCopies {
    /// The title has no library row, so no copies are recorded for it.
    Untracked,
    /// The destinations with a stored copy (`edition_copies`), each with
    /// the edition's primary audio key there, the destinations whose copy
    /// failed for good, and whether those two cover every destination in
    /// the target set.
    Tracked {
        /// Destinations holding the edition, with their keys.
        present: Vec<(DestinationId, String)>,
        /// Destinations whose copy is `failed`: an acquire leaves them
        /// alone until a forced one tries them again.
        held: Vec<DestinationId>,
        /// True when every destination in the target set holds the
        /// edition or is held.
        complete: bool,
    },
}

impl StoredCopies {
    /// Whether every destination in the target set holds the edition (an
    /// untracked title counts as complete, as before copies were
    /// recorded).
    fn complete(&self) -> bool {
        match self {
            Self::Untracked => true,
            Self::Tracked { complete, .. } => *complete,
        }
    }

    /// Whether `destination`'s copy failed for good, so an unforced
    /// acquire writes nothing there.
    fn is_held(&self, destination: &DestinationId) -> bool {
        match self {
            Self::Untracked => false,
            Self::Tracked { held, .. } => held.contains(destination),
        }
    }

    /// The key a stored copy of the edition has on `destination`.
    fn key_on(&self, destination: &DestinationId) -> Option<&str> {
        match self {
            Self::Untracked => None,
            Self::Tracked { present, .. } => present
                .iter()
                .find(|(id, _)| id == destination)
                .map(|(_, key)| key.as_str()),
        }
    }
}

/// The stored copies of `edition` on the destinations in `destinations`.
///
/// Copies count only while the edition is acquired: a storage match that
/// found the files gone marks the edition not acquired, and its copy rows
/// must not then stand in for the files.
///
/// # Errors
///
/// Returns an error when a read fails.
async fn stored_copies(
    library: &LibraryStore,
    edition: &str,
    destinations: &AcquireDestinations,
) -> Result<StoredCopies> {
    let acquired = library
        .edition_acquisition(edition)
        .await?
        .is_some_and(|acquisition| acquisition.status == AcquireStatus::Acquired);
    if !acquired {
        return Ok(StoredCopies::Tracked {
            present: Vec::new(),
            held: Vec::new(),
            complete: false,
        });
    }
    let copies = library.copies_of_editions(&[edition.to_owned()]).await?;
    let present: Vec<(DestinationId, String)> = destinations
        .items
        .iter()
        .filter_map(|dest| {
            copies
                .iter()
                .find(|copy| {
                    copy.destination == dest.id.as_str() && copy.state == CopyState::Stored
                })
                .and_then(|copy| copy.storage_key.clone())
                .map(|key| (dest.id.clone(), key))
        })
        .collect();
    let held: Vec<DestinationId> = destinations
        .items
        .iter()
        .filter(|dest| {
            copies
                .iter()
                .any(|copy| copy.destination == dest.id.as_str() && copy.state == CopyState::Failed)
        })
        .map(|dest| dest.id.clone())
        .collect();
    let complete = present.len() + held.len() == destinations.items.len();
    Ok(StoredCopies::Tracked {
        present,
        held,
        complete,
    })
}

/// Records each destination's outcome as `edition`'s copy there: stored
/// with its key, or pending with the error and a retry delay.
///
/// # Errors
///
/// Returns an error when a write fails.
async fn record_copies(
    library: &LibraryStore,
    edition: &str,
    copies: &[CopyOutcome],
) -> Result<()> {
    let now = chrono::Utc::now();
    for copy in copies {
        match &copy.result {
            CopyResult::Stored { key } => {
                library
                    .set_copy_stored(edition, copy.destination.as_str(), key)
                    .await?;
            }
            CopyResult::Failed { error } => {
                let state = library
                    .set_copy_pending(edition, copy.destination.as_str(), error, now)
                    .await?;
                tracing::warn!(
                    edition,
                    destination = %copy.destination,
                    state = state.as_str(),
                    %error,
                    "a destination's copy failed"
                );
            }
        }
    }
    Ok(())
}

/// One copy of the book an acquire makes (`output.copies`) and the
/// destinations that receive it, each recording its audio as that copy's
/// ([`AcquireDestinations::copy_plan`]).
struct CopyTarget {
    /// The copy.
    copy: bookclerk_config::OutputCopy,
    /// The destinations that receive it, with options that package it.
    destinations: AcquireDestinations,
}

/// The target set of `req`: the destinations whose filter takes the book
/// when the request carries its [`AcquireRequest::placement`], else `None`
/// (every destination given).
///
/// # Errors
///
/// Returns [`AcquireError::NoDestinationAccepts`] when no destination takes
/// the book.
fn placed_destinations(
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
) -> Result<Option<AcquireDestinations>> {
    req.placement
        .as_ref()
        .map(|facts| destinations.for_book(facts))
        .transpose()
}

/// Names the whole acquire with the call number of the edition `owner`
/// read once at its start, so no later naming step re-reads the edition
/// and, on a failed read, names a file differently from the others.
/// A call number the request already carries (an owner's, for a host) is
/// kept.
fn pin_call_number(req: &mut AcquireRequest, owner: Option<&FileOwner>) {
    if req.call_number.is_some() {
        return;
    }
    match owner {
        Some(FileOwner::Edition(edition)) => {
            req.call_number = bookclerk_library::call_number(edition)
        }
        None => {}
    }
}

/// [`acquire_book_indexed`] against destinations whose writes are recorded
/// for the title ([`recording_destinations`]).
///
/// # Errors
///
/// Returns an error when fetch, package, storage, or library updates fail.
async fn acquire_book_recorded(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    copies: &[CopyTarget],
    mut req: AcquireRequest,
    mut index: Option<&mut StorageIndex>,
    source: &dyn ContentSource,
    stored: &StoredCopies,
) -> Result<AcquireResult> {
    req.options = destinations.primary_destination().options.clone();
    req.destination_keys =
        crate::keys::DestinationKeys::of(destinations.primary_destination().backend.as_ref());
    tracing::info!(
        asin = %req.asin,
        source = %req.source,
        title = %req.title,
        force = req.force,
        output = ?req.options.effective_output(),
        "acquire requested"
    );

    if req.options.wants_split_by_size() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "output=split_mp3_by_size is not implemented yet (split_mp3_max_mb={}); \
             use split_mp3_by_chapter or single_mp3",
            req.options.split_mp3_max_mb
        )));
    }
    if copies.is_empty() {
        // Some destination takes the book, but every one refuses every
        // copy the library makes.
        return Err(AcquireError::NoDestinationAccepts);
    }

    // Destinations that already hold the title while the fetch writes only
    // the ones that lack it ([`MultiDestinationMode::RefetchMissing`]).
    let mut already_stored = Vec::new();
    if !req.force && !req.options.overwrite_existing {
        // An edition some destination in the target set lacks is checked
        // per destination below instead, so the missing copies are written.
        let acquired_title = if stored.complete() {
            resolve_title(library, &req).await
        } else {
            None
        };
        if let Some(title) = acquired_title {
            // The edition was acquired by this title: its files are this
            // title's own. Another title's copy is checked before it is
            // shared ([`share_acquired_sibling`]).
            if title.status == AcquireStatus::Acquired && title.acquired_here() {
                let primary_key = title.storage_key.clone().unwrap_or_default();
                tracing::info!(
                    asin = %req.asin,
                    key = %primary_key,
                    "skipping download — title already acquired"
                );
                let fetched_pdf =
                    resume_missing_companion_pdf(library, destinations, &req, source).await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: Vec::new(),
                    matched_existing: true,
                    from_library: false,
                    store_fetches: u32::from(fetched_pdf),
                    copies: Vec::new(),
                });
            }
            let backends: Vec<(&str, &dyn StorageBackend)> = destinations
                .items
                .iter()
                .map(|item| (item.id.as_str(), item.backend.as_ref()))
                .collect();
            if let Some(primary_key) = share_acquired_sibling(library, &backends, &title).await? {
                tracing::info!(
                    asin = %req.asin,
                    key = %primary_key,
                    "skipping download — another book of this edition is already acquired"
                );
                let fetched_pdf =
                    resume_missing_companion_pdf(library, destinations, &req, source).await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: Vec::new(),
                    matched_existing: true,
                    from_library: false,
                    store_fetches: u32::from(fetched_pdf),
                    copies: Vec::new(),
                });
            }
        }
        // Before accepting media already at the planned name: a file there
        // may be another edition's (two editions whose short ids collide),
        // and adopting it would point this edition at that recording.
        check_edition_paths(library, destinations, &req).await?;
        match plan_existing_destinations(library, destinations, &req, index.as_deref(), stored)
            .await?
        {
            ExistingPlan::Skip {
                primary_key,
                present,
            } => {
                tracing::info!(
                    asin = %req.asin,
                    key = %primary_key,
                    "skipping download — matched existing acquired media"
                );
                record_status(
                    library,
                    &req,
                    AcquireStatus::Acquired,
                    Some(&primary_key),
                    None,
                )
                .await?;
                let fetched_pdf =
                    resume_missing_companion_pdf(library, destinations, &req, source).await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: Vec::new(),
                    matched_existing: true,
                    from_library: false,
                    store_fetches: u32::from(fetched_pdf),
                    copies: stored_outcomes(&present),
                });
            }
            ExistingPlan::SyncMissing {
                primary_key,
                source_id,
                source_key,
                missing,
                present,
            } => {
                tracing::info!(
                    asin = %req.asin,
                    from = ?source_id,
                    missing = missing.len(),
                    "syncing existing media to missing destinations (no store fetch)"
                );
                let synced = sync_missing_destinations(
                    destinations,
                    &source_id,
                    &source_key,
                    &missing,
                    &req.files_dir.join("stage-journal"),
                )
                .await?;
                let written: Vec<String> = synced
                    .iter()
                    .filter_map(|outcome| match &outcome.result {
                        CopyResult::Stored { key } => Some(key.clone()),
                        CopyResult::Failed { .. } => None,
                    })
                    .collect();
                let mut copies = stored_outcomes(&present);
                copies.extend(synced);
                if let Some(idx) = index.as_mut() {
                    for key in &written {
                        idx.insert_key(key.clone());
                    }
                }
                record_status(
                    library,
                    &req,
                    AcquireStatus::Acquired,
                    Some(&primary_key),
                    None,
                )
                .await?;
                let fetched_pdf =
                    resume_missing_companion_pdf(library, destinations, &req, source).await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: written,
                    matched_existing: true,
                    from_library: false,
                    store_fetches: u32::from(fetched_pdf),
                    copies,
                });
            }
            ExistingPlan::Fetch {
                only_ids: Some(ids),
                present,
            } => {
                tracing::info!(
                    asin = %req.asin,
                    destinations = ?ids,
                    "re-fetching into missing destinations only"
                );
                req.write_destinations = Some(ids);
                already_stored = present;
            }
            ExistingPlan::Fetch {
                only_ids: None,
                present: _,
            } => {
                // Full acquire into every destination (refetch_all, or nothing present).
            }
        }
    }

    if req.force || req.options.overwrite_existing {
        // The existing-media path above has already checked.
        check_edition_paths(library, destinations, &req).await?;
    }

    record_status(library, &req, AcquireStatus::Queued, None, None).await?;

    let result = match run_pipeline(library, copies, &req, source).await {
        Ok(mut result) => {
            result.copies.extend(stored_outcomes(&already_stored));
            if let Some(idx) = index.as_mut() {
                idx.insert_key(result.storage_key.clone());
                for key in &result.written_keys {
                    idx.insert_key(key.clone());
                }
            }
            record_status(
                library,
                &req,
                AcquireStatus::Acquired,
                Some(&result.storage_key),
                None,
            )
            .await?;
            Ok(result)
        }
        // A fetch that only filled missing destinations failed while others
        // hold the title: the title stays acquired at their key, and the
        // failure counts as an attempt on each copy it was to write.
        // A store that asked to slow down is answered as the failure it is,
        // so the job pauses the account; the title stays acquired.
        Err(err) if !already_stored.is_empty() => {
            let primary_key = already_stored
                .iter()
                .find(|(id, _)| *id == destinations.primary)
                .or_else(|| already_stored.first())
                .map(|(_, key)| key.clone())
                .unwrap_or_default();
            library
                .set_acquire_status(
                    status_key(&req),
                    &req.account_id,
                    AcquireStatus::Acquired,
                    Some(&primary_key),
                    None,
                )
                .await?;
            if matches!(
                err,
                AcquireError::Source(bookclerk_source::SourceError::RateLimited(_))
            ) {
                return Err(err);
            }
            let error = err.to_string();
            let mut copies = stored_outcomes(&already_stored);
            copies.extend(
                req.write_destinations
                    .iter()
                    .flatten()
                    .map(|id| CopyOutcome {
                        destination: id.clone(),
                        result: CopyResult::Failed {
                            error: error.clone(),
                        },
                    }),
            );
            tracing::warn!(
                asin = %req.asin,
                %error,
                "writing the missing destinations failed; they are retried in the background"
            );
            Ok(AcquireResult {
                asin: req.asin.clone(),
                storage_key: primary_key,
                written_keys: Vec::new(),
                matched_existing: true,
                from_library: false,
                // The failed fetch counts once against the account's pace,
                // as a failed acquire does.
                store_fetches: 1,
                copies,
            })
        }
        Err(err) => {
            let message = err.to_string();
            let _ = record_status(library, &req, AcquireStatus::Error, None, Some(&message)).await;
            Err(err)
        }
    };
    let work_dir = req.cache_dir.join("acquire").join(status_key(&req));
    cleanup_work_dir(library, req.job_id.as_deref(), &work_dir).await;
    result
}

/// The request's key for logs and scratch paths: its account holding when
/// known, otherwise the store product id.
fn status_key(req: &AcquireRequest) -> &str {
    req.holding_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .unwrap_or(&req.asin)
}

/// Records `req`'s acquire state: under its account holding when known
/// ([`LibraryStore::set_holding_acquire_status`]), otherwise on the title
/// its product id names.
///
/// # Errors
///
/// Returns the library's error when the write fails or the title is gone.
async fn record_status(
    library: &LibraryStore,
    req: &AcquireRequest,
    status: AcquireStatus,
    storage_key: Option<&str>,
    error_message: Option<&str>,
) -> bookclerk_library::Result<()> {
    match req.holding_id.as_deref().filter(|id| !id.is_empty()) {
        Some(holding) => {
            library
                .set_holding_acquire_status(holding, status, storage_key, error_message)
                .await
        }
        None => {
            library
                .set_acquire_status(
                    &req.asin,
                    &req.account_id,
                    status,
                    storage_key,
                    error_message,
                )
                .await
        }
    }
}

/// Records `req`'s companion PDF state, keyed as [`record_status`] keys
/// the audio state.
///
/// # Errors
///
/// Returns the library's error when the write fails or the title is gone.
async fn record_pdf_status(
    library: &LibraryStore,
    req: &AcquireRequest,
    status: AcquireStatus,
    pdf_storage_key: Option<&str>,
) -> bookclerk_library::Result<()> {
    match req.holding_id.as_deref().filter(|id| !id.is_empty()) {
        Some(holding) => {
            library
                .set_holding_pdf_status(holding, status, pdf_storage_key)
                .await
        }
        None => {
            library
                .set_pdf_status(&req.asin, &req.account_id, status, pdf_storage_key)
                .await
        }
    }
}

/// Initial scratch reservation held until the real directory size is known.
const INITIAL_TEMP_RESERVE_BYTES: u64 = 256 * 1024 * 1024;

/// Enforces the job temp quota, creates `work_dir`, and reserves it on the job.
async fn prepare_work_dir(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
) -> Result<()> {
    let quota = req.temp_quota_bytes.unwrap_or(u64::MAX);
    let remaining = remaining_temp_budget(library, req, work_dir).await?;
    if remaining == 0 {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded (0 bytes remaining)"
        )));
    }
    if let Some(job_id) = req.job_id.as_deref() {
        let reserve = INITIAL_TEMP_RESERVE_BYTES.min(remaining);
        library
            .reserve_job_temp_path(job_id, &work_dir.to_string_lossy(), reserve, quota)
            .await?;
        tokio::fs::create_dir_all(work_dir).await?;
        return Ok(());
    }
    if quota != u64::MAX {
        let used = scratch_usage(&req.cache_dir).await;
        if used >= quota {
            return Err(AcquireError::Other(anyhow::anyhow!(
                "acquire scratch quota exceeded ({used} >= {quota} bytes)"
            )));
        }
    }
    tokio::fs::create_dir_all(work_dir).await?;
    Ok(())
}

/// Expands the path reservation to the on-disk size, or fails when over quota.
async fn enforce_work_dir_quota(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
) -> Result<()> {
    let Some(quota) = req.temp_quota_bytes else {
        return Ok(());
    };
    let used = dir_size(work_dir).await;
    if let Some(job_id) = req.job_id.as_deref() {
        library
            .reserve_job_temp_path(job_id, &work_dir.to_string_lossy(), used.max(1), quota)
            .await?;
        return Ok(());
    }
    let total = scratch_usage(&req.cache_dir).await;
    if total > quota {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded ({total} > {quota} bytes)"
        )));
    }
    Ok(())
}

/// Best-effort size of `{cache}/acquire` and `{cache}/acquire-pdf`.
async fn scratch_usage(cache_dir: &Path) -> u64 {
    let mut total = 0u64;
    for name in ["acquire", "acquire-pdf"] {
        total = total.saturating_add(dir_size(&cache_dir.join(name)).await);
    }
    total
}

/// Recursive directory size; missing paths count as zero.
async fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Bytes still allowed for `work_dir` under the job or cache-wide quota.
///
/// The work directory's own bytes are left out of the used total on both
/// paths (its reservation on a job, its size on disk without one), because
/// every caller compares the remainder with `dir_size(work_dir)`: counting
/// them in both places would fail a work directory over half the quota.
async fn remaining_temp_budget(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
) -> Result<u64> {
    let Some(quota) = req.temp_quota_bytes else {
        return Ok(u64::MAX);
    };
    if let Some(job_id) = req.job_id.as_deref() {
        let used = library.reserved_temp_bytes().await?;
        let this = library
            .list_job_temp_paths(job_id)
            .await?
            .into_iter()
            .find(|row| row.path == work_dir.to_string_lossy())
            .map(|row| row.reserved_bytes)
            .unwrap_or(0);
        return Ok(quota.saturating_sub(used.saturating_sub(this)));
    }
    let used = scratch_usage(&req.cache_dir).await;
    let this = if counted_in_scratch(&req.cache_dir, work_dir) {
        dir_size(work_dir).await
    } else {
        0
    };
    Ok(quota.saturating_sub(used.saturating_sub(this)))
}

/// Bytes a new file written into `work_dir` may take: the remaining budget
/// less what the work directory already holds.
///
/// [`remaining_temp_budget`] leaves the work directory out of the used total
/// because its callers measure the whole directory; a single new download
/// (a companion PDF, a library's copy) is measured alone, so it is capped
/// at what is left beside the files already there.
async fn remaining_for_new_file(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
) -> Result<u64> {
    let budget = remaining_temp_budget(library, req, work_dir).await?;
    if budget == u64::MAX {
        return Ok(budget);
    }
    Ok(budget.saturating_sub(dir_size(work_dir).await))
}

/// Whether `work_dir` lies under a directory [`scratch_usage`] counts.
fn counted_in_scratch(cache_dir: &Path, work_dir: &Path) -> bool {
    ["acquire", "acquire-pdf"]
        .iter()
        .any(|name| work_dir.starts_with(cache_dir.join(name)))
}

/// Fetches a title while a watchdog cancels the source if the cache exceeds quota.
async fn fetch_title_enforcing_quota(
    library: &LibraryStore,
    req: &AcquireRequest,
    source: &dyn ContentSource,
    work_dir: &Path,
) -> Result<PlainFetch> {
    let remaining = remaining_temp_budget(library, req, work_dir).await?;
    if remaining == 0 {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded (0 bytes remaining)"
        )));
    }
    let bounded = remaining != u64::MAX;
    let cancel = Arc::new(AtomicBool::new(false));
    let opts = FetchOptions {
        download: req.options.clone(),
        cache_dir: work_dir.to_path_buf(),
        files_dir: req.files_dir.clone(),
        max_cache_bytes: bounded.then_some(remaining),
        cancel: bounded.then(|| cancel.clone()),
    };
    let scope = bookclerk_source::source_scope(library, source);
    let fetch_fut = source.fetch_title(&scope, &req.account_id, &req.asin, &opts);
    let fetch = if bounded {
        let watchdog = async {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if dir_size(work_dir).await > remaining {
                    cancel.store(true, Ordering::SeqCst);
                    break;
                }
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
            }
        };
        tokio::select! {
            result = fetch_fut => result?,
            () = watchdog => {
                return Err(AcquireError::Other(anyhow::anyhow!(
                    "acquire scratch quota exceeded during fetch ({remaining} byte budget)"
                )));
            }
        }
    } else {
        fetch_fut.await?
    };
    if cancel.load(Ordering::SeqCst) || dir_size(work_dir).await > remaining {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded during fetch ({remaining} byte budget)"
        )));
    }
    Ok(fetch)
}

/// Runs a temp-producing stage and fails if `work_dir` exceeds the remaining budget.
///
/// A 50 ms watchdog cancels the stage when growth is incremental. A post-check
/// catches a single write that overshoots the budget before the next poll.
async fn run_stage_enforcing_quota<T, F>(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
    fut: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let remaining = remaining_temp_budget(library, req, work_dir).await?;
    if remaining == 0 {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded (0 bytes remaining)"
        )));
    }
    if remaining == u64::MAX {
        return fut.await;
    }
    let result = tokio::select! {
        result = fut => result,
        () = quota_watchdog(work_dir, remaining) => {
            Err(AcquireError::Other(anyhow::anyhow!(
                "acquire scratch quota exceeded during packaging ({remaining} byte budget)"
            )))
        }
    };
    if dir_size(work_dir).await > remaining {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded during packaging ({remaining} byte budget)"
        )));
    }
    result
}

/// Polls `work_dir` until it exceeds `remaining` bytes.
async fn quota_watchdog(work_dir: &Path, remaining: u64) {
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if dir_size(work_dir).await > remaining {
            break;
        }
    }
}

/// Streams an HTTP body to `path`, aborting once `max_bytes` would be exceeded.
async fn write_http_body_capped(
    response: reqwest::Response,
    path: &Path,
    max_bytes: u64,
) -> Result<u64> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut written = 0u64;
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| AcquireError::Other(anyhow::anyhow!("download body failed: {err}")))?
    {
        let next = written.saturating_add(chunk.len() as u64);
        if next > max_bytes {
            let _ = tokio::fs::remove_file(path).await;
            return Err(AcquireError::Other(anyhow::anyhow!(
                "acquire scratch quota exceeded while streaming ({max_bytes} byte budget)"
            )));
        }
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        written = next;
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    Ok(written)
}

/// Removes a scratch directory and unregisters that path after it is gone.
async fn cleanup_work_dir(library: &LibraryStore, job_id: Option<&str>, work_dir: &Path) {
    let gone = match tokio::fs::remove_dir_all(work_dir).await {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            tracing::warn!(
                path = %work_dir.display(),
                error = %err,
                "failed to clean acquire cache dir"
            );
            false
        }
    };
    if gone {
        if let Some(job_id) = job_id {
            let _ = library
                .unregister_job_temp_path(job_id, &work_dir.to_string_lossy())
                .await;
        }
    }
}

#[derive(Debug)]
/// Whether acquire can skip, copy between destinations, or must fetch from the store.
enum ExistingPlan {
    /// Every destination already has the title — skip acquire.
    Skip {
        /// Primary library key for the already-acquired title.
        primary_key: String,
        /// Each destination with the key the title was found at there.
        present: Vec<(DestinationId, String)>,
    },
    /// Copy from a present destination into missing ones (no store fetch).
    SyncMissing {
        /// Storage key already present on the primary (or first present) destination.
        primary_key: String,
        /// Destination that already holds the title and will be copied from.
        source_id: DestinationId,
        /// Object key on `source_id` to copy into missing destinations.
        source_key: String,
        /// Destinations that lack the title, paired with the planned target key.
        missing: Vec<(DestinationId, String)>,
        /// Each destination that already holds the title, with its key there.
        present: Vec<(DestinationId, String)>,
    },
    /// Run the full acquire pipeline (`only_ids` limits writes when set).
    Fetch {
        /// When set, restrict the fetch write to these destinations; `None` writes all.
        only_ids: Option<Vec<DestinationId>>,
        /// Each destination that already holds the title, with its key
        /// there (empty when the fetch rewrites every destination).
        present: Vec<(DestinationId, String)>,
    },
}

/// Inspects each destination for an existing object and chooses skip, sync, or fetch.
async fn plan_existing_destinations(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    index: Option<&StorageIndex>,
    stored: &StoredCopies,
) -> Result<ExistingPlan> {
    if destinations.len() == 1 {
        let dest = destinations.primary_destination();
        if let Some(key) = stored_key_present(stored, dest).await {
            return Ok(ExistingPlan::Skip {
                present: vec![(dest.id.clone(), key.to_owned())],
                primary_key: key.to_owned(),
            });
        }
        let dest_req = request_for_destination(req, dest);
        let owned_index;
        let lookup = match index {
            Some(idx) => idx,
            None => {
                owned_index = StorageIndex::from_storage(dest.backend.as_ref()).await?;
                &owned_index
            }
        };
        return Ok(
            match find_existing_for_request(lookup, dest.backend.as_ref(), library, &dest_req)
                .await?
            {
                Some(primary_key) => ExistingPlan::Skip {
                    present: vec![(dest.id.clone(), primary_key.clone())],
                    primary_key,
                },
                None => ExistingPlan::Fetch {
                    only_ids: None,
                    present: Vec::new(),
                },
            },
        );
    }

    let mut present: Vec<(DestinationId, String)> = Vec::new();
    let mut missing_ids: Vec<DestinationId> = Vec::new();
    let mut primary_key = None;
    // A destination whose copy failed for good is neither checked nor
    // written: it waits for a forced acquire.
    let mut held = false;

    for dest in &destinations.items {
        if stored.is_held(&dest.id) {
            held = true;
            continue;
        }
        // A recorded stored copy whose key is still there needs no listing.
        if let Some(key) = stored_key_present(stored, dest).await {
            if dest.id == destinations.primary {
                primary_key = Some(key.to_owned());
            }
            present.push((dest.id.clone(), key.to_owned()));
            continue;
        }
        let dest_req = request_for_destination(req, dest);
        // A destination that cannot be listed now is unavailable: it is
        // treated as lacking the title, so the write to it fails on its own
        // and is retried, rather than failing the whole acquire here.
        let found = match StorageIndex::from_storage(dest.backend.as_ref()).await {
            Ok(dest_index) => {
                find_existing_for_request(&dest_index, dest.backend.as_ref(), library, &dest_req)
                    .await
            }
            Err(err) => Err(err),
        };
        let found = match found {
            Ok(found) => found,
            Err(err) => {
                tracing::warn!(
                    destination = %dest.id,
                    error = %err,
                    "destination unavailable while checking for the title"
                );
                None
            }
        };
        if let Some(key) = found {
            if dest.id == destinations.primary {
                primary_key = Some(key.clone());
            }
            present.push((dest.id.clone(), key));
        } else {
            missing_ids.push(dest.id.clone());
        }
    }

    let ext = present
        .first()
        .and_then(|(_, k)| k.rsplit_once('.').map(|(_, e)| e))
        .unwrap_or("m4b");
    let mut missing: Vec<(DestinationId, String)> = Vec::new();
    for id in missing_ids {
        let Some(dest) = destinations.destination(&id) else {
            continue;
        };
        let dest_req = request_for_destination(req, dest);
        missing.push((id, planned_storage_key_for(library, &dest_req, ext).await));
    }

    if missing.is_empty() {
        let primary_key = primary_key
            .or_else(|| present.first().map(|(_, k)| k.clone()))
            .unwrap_or_default();
        return Ok(ExistingPlan::Skip {
            primary_key,
            present,
        });
    }
    // The write set when the fetch writes every destination it checked:
    // all of them, or only the missing ones when some are held.
    let missing_only = |missing: Vec<(DestinationId, String)>| {
        held.then(|| missing.into_iter().map(|(id, _)| id).collect())
    };
    if present.is_empty() {
        return Ok(ExistingPlan::Fetch {
            only_ids: missing_only(missing),
            present,
        });
    }

    let primary_key = primary_key
        .clone()
        .or_else(|| present.first().map(|(_, k)| k.clone()))
        .unwrap_or_else(|| missing[0].1.clone());

    match destinations.multi_destination {
        // A copy between destinations moves one file: an edition stored as
        // several audio files (parts or chapters) is fetched again for the
        // destinations that lack it instead.
        MultiDestinationMode::SyncMissing
            if !single_audio_file(library, req, &present[0].0).await? =>
        {
            Ok(ExistingPlan::Fetch {
                only_ids: Some(missing.into_iter().map(|(k, _)| k).collect()),
                present,
            })
        }
        MultiDestinationMode::SyncMissing => {
            let (source_id, source_key) = present[0].clone();
            Ok(ExistingPlan::SyncMissing {
                primary_key,
                source_id,
                source_key,
                missing,
                present,
            })
        }
        MultiDestinationMode::RefetchMissing => Ok(ExistingPlan::Fetch {
            only_ids: Some(missing.into_iter().map(|(k, _)| k).collect()),
            present,
        }),
        MultiDestinationMode::RefetchAll if held => {
            let mut ids: Vec<DestinationId> = present.into_iter().map(|(id, _)| id).collect();
            ids.extend(missing.into_iter().map(|(id, _)| id));
            Ok(ExistingPlan::Fetch {
                only_ids: Some(ids),
                present: Vec::new(),
            })
        }
        MultiDestinationMode::RefetchAll => Ok(ExistingPlan::Fetch {
            only_ids: None,
            present: Vec::new(),
        }),
    }
}

/// The key of `dest`'s stored copy in `stored`, when the object is still
/// there. A copy whose object is gone, or a destination that cannot answer,
/// is checked by listing instead.
async fn stored_key_present<'a>(
    stored: &'a StoredCopies,
    dest: &AcquireDestination,
) -> Option<&'a str> {
    let key = stored.key_on(&dest.id)?;
    matches!(dest.backend.exists(key).await, Ok(true)).then_some(key)
}

/// Whether `req`'s edition is stored as at most one audio file on
/// `destination` (`edition_files`), so a copy of its primary key copies
/// all of its audio.
///
/// # Errors
///
/// Returns an error when the read fails.
async fn single_audio_file(
    library: &LibraryStore,
    req: &AcquireRequest,
    destination: &DestinationId,
) -> Result<bool> {
    let Some(owner) = file_owner_of_request(library, req).await? else {
        return Ok(true);
    };
    Ok(library
        .audio_keys_of(destination.as_str(), &owner)
        .await?
        .len()
        <= 1)
}

/// Retries per destination `put` before the acquire write is treated as failed.
const DEST_WRITE_ATTEMPTS: u32 = 3;

/// Copies an existing object from one destination into every destination
/// that lacks it, all at once.
///
/// Each target's transfer retries on its own; a target that still fails is
/// reported as a [`CopyResult::Failed`] outcome rather than failing the
/// acquire, since the source destination already holds the title.
///
/// # Returns
///
/// One outcome per entry of `missing`, in order.
///
/// # Errors
///
/// Returns an error when a destination is not configured or the source
/// object cannot be read.
async fn sync_missing_destinations(
    destinations: &AcquireDestinations,
    source_id: &DestinationId,
    source_key: &str,
    missing: &[(DestinationId, String)],
    stage_journal: &Path,
) -> Result<Vec<CopyOutcome>> {
    let source = destinations.destination(source_id).ok_or_else(|| {
        AcquireError::Other(anyhow::anyhow!(
            "sync source destination {:?} is not configured",
            source_id
        ))
    })?;
    let probe = source.backend.probe(source_key).await?;
    let mut meta = probe.meta;
    if meta.content_length.is_none() {
        meta.content_length = Some(probe.size);
    }
    if meta.content_type.is_none() {
        meta.content_type = probe.content_type.or_else(|| {
            source_key
                .rsplit_once('.')
                .map(|(_, ext)| content_type_for_ext(ext).to_string())
        });
    }

    // The stored media is the main output's copy; a destination whose copy
    // rules refuse it (one that takes only an extra copy) is not given it.
    let main = destinations
        .primary_destination()
        .options
        .copies_to_make()
        .into_iter()
        .next();
    let mut targets = Vec::with_capacity(missing.len());
    for (id, target_key) in missing {
        let dest = destinations.destination(id).ok_or_else(|| {
            AcquireError::Other(anyhow::anyhow!(
                "sync target destination {:?} is not configured",
                id
            ))
        })?;
        if main
            .as_ref()
            .is_some_and(|copy| !dest.filter.accepts_copy(copy))
        {
            continue;
        }
        targets.push((dest, target_key));
    }
    let options = bookclerk_storage::TransferOptions {
        max_attempts: DEST_WRITE_ATTEMPTS,
        stage_journal_dir: Some(stage_journal.to_path_buf()),
        ..bookclerk_storage::TransferOptions::default()
    };
    let outcomes = futures::future::join_all(targets.into_iter().map(|(dest, target_key)| {
        let meta = meta.clone();
        let options = &options;
        async move {
            let result = match bookclerk_storage::transfer_object(
                source.backend.as_ref(),
                source_key,
                dest.backend.as_ref(),
                target_key,
                meta,
                options,
            )
            .await
            {
                Ok(_) => CopyResult::Stored {
                    key: target_key.clone(),
                },
                Err(err) => {
                    tracing::warn!(
                        destination = %dest.id,
                        key = %target_key,
                        error = %err,
                        "copy to a missing destination failed; it is retried in the background"
                    );
                    CopyResult::Failed {
                        error: err.to_string(),
                    }
                }
            };
            CopyOutcome {
                destination: dest.id.clone(),
                result,
            }
        }
    }))
    .await;
    Ok(outcomes)
}

/// A [`CopyResult::Stored`] outcome for each destination in `present`.
fn stored_outcomes(present: &[(DestinationId, String)]) -> Vec<CopyOutcome> {
    present
        .iter()
        .map(|(destination, key)| CopyOutcome {
            destination: destination.clone(),
            result: CopyResult::Stored { key: key.clone() },
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
/// How prepared audio files are named when written to storage.
enum AudioKeyPlan {
    /// One packaged file using the title-level file template.
    Single,
    /// One file per chapter using the chapter-file template.
    SplitChapters,
    /// One file per source part, with part index baked into the product id.
    PlainParts,
}

#[derive(Debug, Clone)]
/// Local audio file ready to upload, with the title and extension used for naming.
struct PreparedAudioFile {
    /// Absolute path of the packaged or source audio on disk.
    path: PathBuf,
    /// Chapter or part title substituted into naming templates.
    title: String,
    /// File extension without a leading dot (`m4b`, `mp3`, …).
    ext: String,
}

#[derive(Debug, Clone)]
/// First object key written to one destination during this acquire.
struct DestinationStoredKey {
    /// Destination that received the write (`local`, `s3`, …).
    id: DestinationId,
    /// Object key of the first file written to that destination.
    key: String,
}

#[derive(Debug, Clone)]
/// A destination whose writes failed this acquire.
struct DestinationFailure {
    /// The destination that was not written.
    id: DestinationId,
    /// The last write's error, for the copy's record.
    error: String,
}

#[derive(Debug, Clone)]
/// Keys written this acquire, plus the primary destination's first key for status.
struct StoredKeys {
    /// First key on the primary destination (or the first successful write).
    primary_key: String,
    /// Per-destination first keys used for sidecar writes and result reporting.
    keys: Vec<DestinationStoredKey>,
    /// Destinations in the write set that still lack the files.
    failed: Vec<DestinationFailure>,
}

impl StoredKeys {
    /// First object key written on each destination, in destination order.
    fn all_keys(&self) -> Vec<String> {
        self.keys.iter().map(|stored| stored.key.clone()).collect()
    }

    /// Each destination's outcome, for the edition's copy records.
    fn copies(&self) -> Vec<CopyOutcome> {
        let stored = self.keys.iter().map(|stored| CopyOutcome {
            destination: stored.id.clone(),
            result: CopyResult::Stored {
                key: stored.key.clone(),
            },
        });
        let failed = self.failed.iter().map(|failed| CopyOutcome {
            destination: failed.id.clone(),
            result: CopyResult::Failed {
                error: failed.error.clone(),
            },
        });
        stored.chain(failed).collect()
    }
}

/// Clones the acquire request with this destination's naming and download options.
fn request_for_destination(
    req: &AcquireRequest,
    destination: &AcquireDestination,
) -> AcquireRequest {
    let mut dest_req = req.clone();
    dest_req.options = destination.options.clone();
    dest_req.destination_keys = crate::keys::DestinationKeys::of(destination.backend.as_ref());
    dest_req
}

/// Writes prepared audio to every destination in the write set at the same
/// time, retrying each destination independently.
///
/// The acquire succeeds as soon as one destination holds the complete
/// files: a destination whose writes still fail after
/// [`DEST_WRITE_ATTEMPTS`] is reported in [`StoredKeys::failed`] and
/// retried in the background (storage destinations ADR, decision 3), while
/// the destinations that stored the files keep them.
///
/// # Errors
///
/// Returns the first destination's last error when no destination stored
/// the files, or an error when there is nothing to write.
async fn store_prepared_audio_files(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    files: &[PreparedAudioFile],
    plan: AudioKeyPlan,
) -> Result<StoredKeys> {
    if files.is_empty() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "no audio files were prepared for storage"
        )));
    }

    let targets = destinations.items.iter().filter(|dest| {
        req.write_destinations
            .as_deref()
            .is_none_or(|only| only.is_empty() || only.contains(&dest.id))
    });
    let outcomes = futures::future::join_all(
        targets.map(|dest| store_prepared_on_destination(library, dest, req, files, plan)),
    )
    .await;

    let mut primary_key = None;
    let mut keys = Vec::new();
    let mut failed = Vec::new();
    let mut first_err = None;
    for (id, outcome) in outcomes {
        match outcome {
            Ok(key) => {
                if id == destinations.primary {
                    primary_key = Some(key.clone());
                }
                keys.push(DestinationStoredKey { id, key });
            }
            Err(err) => {
                failed.push(DestinationFailure {
                    id,
                    error: err.to_string(),
                });
                first_err.get_or_insert(err);
            }
        }
    }
    if keys.is_empty() {
        return Err(first_err.unwrap_or_else(|| {
            AcquireError::Other(anyhow::anyhow!("no destination is in the write set"))
        }));
    }
    if !failed.is_empty() {
        tracing::warn!(
            asin = %req.asin,
            stored = keys.len(),
            failed = ?failed.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            "stored on some destinations; the rest are retried in the background"
        );
    }

    let primary_key = primary_key
        .or_else(|| keys.first().map(|stored| stored.key.clone()))
        .unwrap_or_default();
    Ok(StoredKeys {
        primary_key,
        keys,
        failed,
    })
}

/// Writes prepared audio to one destination, up to [`DEST_WRITE_ATTEMPTS`]
/// times.
///
/// # Returns
///
/// The destination's id with the first key written there, or with the last
/// attempt's error.
async fn store_prepared_on_destination(
    library: &LibraryStore,
    dest: &AcquireDestination,
    req: &AcquireRequest,
    files: &[PreparedAudioFile],
    plan: AudioKeyPlan,
) -> (DestinationId, Result<String>) {
    let dest_req = request_for_destination(req, dest);
    let mut last_err = None;
    for attempt in 1..=DEST_WRITE_ATTEMPTS {
        match write_prepared_files_to_destination(library, dest, &dest_req, files, plan).await {
            Ok((first_key, written_keys)) => {
                apply_storage_timestamps(dest.backend.as_ref(), library, &dest_req, &written_keys)
                    .await;
                return (dest.id.clone(), Ok(first_key));
            }
            Err(err) => {
                tracing::warn!(
                    destination = %dest.id,
                    asin = %req.asin,
                    attempt,
                    %err,
                    "destination write failed; retrying"
                );
                last_err = Some(err);
            }
        }
    }
    let err = last_err.unwrap_or_else(|| {
        AcquireError::Other(anyhow::anyhow!("destination {} was not written", dest.id))
    });
    (dest.id.clone(), Err(err))
}

/// Uploads each prepared file to one destination and returns the first key plus all keys written.
async fn write_prepared_files_to_destination(
    library: &LibraryStore,
    dest: &AcquireDestination,
    dest_req: &AcquireRequest,
    files: &[PreparedAudioFile],
    plan: AudioKeyPlan,
) -> Result<(String, Vec<String>)> {
    let mut first_key = None;
    let mut written_keys = Vec::new();
    // Resolved once for every file: each resolve reads the book, its
    // edition and up to four name probes, and a split book has hundreds
    // of files.
    let contexts = naming_contexts(library, dest_req).await;
    for (idx, file) in files.iter().enumerate() {
        let key = planned_key_for_prepared_file(dest_req, &contexts, file, idx, plan).await;
        let meta = object_meta_for(
            library,
            dest_req,
            &file.title,
            content_type_for_ext(&file.ext),
            tokio::fs::metadata(&file.path).await.ok().map(|m| m.len()),
        )
        .await;
        dest.backend.put_file(&key, &file.path, meta).await?;
        if first_key.is_none() {
            first_key = Some(key.clone());
        }
        written_keys.push(key);
    }
    Ok((first_key.unwrap_or_default(), written_keys))
}

/// Storage key for one prepared file under the single, chapter, or part
/// naming plan, from the request's folder and file naming `contexts`
/// ([`naming_contexts`]), translated by the request's destination
/// ([`AcquireRequest::destination_keys`]).
async fn planned_key_for_prepared_file(
    req: &AcquireRequest,
    contexts: &(NamingContext, NamingContext),
    file: &PreparedAudioFile,
    idx: usize,
    plan: AudioKeyPlan,
) -> String {
    match plan {
        AudioKeyPlan::Single => {
            let templates = req.options.naming_templates();
            let host_key = storage_key_with_contexts(
                &contexts.0,
                &contexts.1,
                Some(templates.folder.as_str()),
                Some(templates.file.as_str()),
                &file.ext,
                &req.options.replacement_characters,
                req.options.path_limits,
            );
            req.destination_keys.translate(host_key).await
        }
        AudioKeyPlan::SplitChapters => planned_chapter_storage_key(req, contexts, idx, file).await,
        AudioKeyPlan::PlainParts => planned_plain_part_storage_key(req, contexts, idx, file).await,
    }
}

/// Chapter-file storage key using 1-based index and the chapter title.
async fn planned_chapter_storage_key(
    req: &AcquireRequest,
    (folder_ctx, file_ctx): &(NamingContext, NamingContext),
    idx: usize,
    file: &PreparedAudioFile,
) -> String {
    let templates = req.options.naming_templates();
    let host_key = chapter_storage_key_with_folder(
        folder_ctx,
        file_ctx,
        Some(templates.folder.as_str()),
        Some(templates.chapter_file.as_str()),
        &req.options.replacement_characters,
        idx + 1,
        &file.title,
        &file.ext,
        req.options.path_limits,
    );
    req.destination_keys.translate(host_key).await
}

/// Part-file storage key with a `-pNNN` product-id suffix so parts do not collide.
async fn planned_plain_part_storage_key(
    req: &AcquireRequest,
    (folder_ctx, file_ctx): &(NamingContext, NamingContext),
    idx: usize,
    file: &PreparedAudioFile,
) -> String {
    let mut part_ctx = file_ctx.clone();
    part_ctx.title = format!("{} — {}", req.title, file.title);
    part_ctx.asin = format!("{}-p{:03}", req.asin, idx + 1);
    // The default file templates name a title by `<call number>`, not
    // `<asin>`, so the part number goes on the id too: two parts with one
    // title must not share a name.
    part_ctx.call_number = part_ctx
        .call_number
        .map(|id| format!("{id}-p{:03}", idx + 1));
    part_ctx.chapter_number = Some(u32::try_from(idx + 1).unwrap_or(1));
    part_ctx.chapter_title = Some(file.title.clone());
    let templates = req.options.naming_templates();
    let host_key = storage_key_with_contexts(
        folder_ctx,
        &part_ctx,
        Some(templates.folder.as_str()),
        Some(templates.file.as_str()),
        &file.ext,
        &req.options.replacement_characters,
        req.options.path_limits,
    );
    req.destination_keys.translate(host_key).await
}

/// Marks the title as downloading, then fetches and stores it from the content source.
async fn run_pipeline(
    library: &LibraryStore,
    copies: &[CopyTarget],
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    record_status(library, req, AcquireStatus::Downloading, None, None).await?;

    run_source_pipeline(library, copies, req, source).await
}

/// Fetches the title into the acquire cache once and stores each copy the
/// library makes of it ([`store_copies`]).
async fn run_source_pipeline(
    library: &LibraryStore,
    copies: &[CopyTarget],
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    let work_dir = req.cache_dir.join("acquire").join(status_key(req));
    prepare_work_dir(library, req, &work_dir).await?;

    let offer = offered_copy(library, req).await;
    let taken = match &offer {
        Some(offer) if offer.same_store => {
            take_library_copy(library, req, &work_dir, offer, CopyCover::Take).await
        }
        Some(_) | None => None,
    };
    let mut store_fetches = u32::from(taken.is_none());
    let (fetch, library_pdf, from_library) = match taken {
        Some(taken) => (taken.fetch, taken.pdf, true),
        None => {
            let own = fetch_title_enforcing_quota(library, req, source, &work_dir).await?;
            // A copy from another store is compared with the download it
            // would replace, so the store is always asked first. The
            // libraries are asked again once the download is here: what
            // they offered before it may no longer be downloadable after
            // a long store download.
            let offer = match offer {
                Some(_) => offered_copy(library, req).await,
                None => None,
            };
            match offer.filter(|offer| !offer.same_store) {
                Some(offer) => {
                    match replace_with_better_copy(library, req, &work_dir, &offer, own).await {
                        Compared::Library(better) => (better, None, true),
                        Compared::Own(own) => (own, None, false),
                    }
                }
                None => (own, None, false),
            }
        }
    };
    enforce_work_dir_quota(library, req, &work_dir).await?;

    check_copy_folders(library, copies, req).await?;
    let mut progress = CopiesProgress::default();
    let stored = run_stage_enforcing_quota(library, req, &work_dir, async {
        store_copies_from(
            library,
            copies,
            req,
            &work_dir,
            &fetch,
            library_pdf.as_deref(),
            &mut progress,
        )
        .await
    })
    .await;
    let stored = match stored {
        Err(AcquireError::Media(err)) if from_library => {
            drop(fetch);
            // The rest comes from the store, so this acquire asks it (again,
            // when its first download was replaced by the library's copy).
            store_fetches += 1;
            store_rest_from_store(library, copies, req, source, &work_dir, &mut progress, &err)
                .await
        }
        stored => stored,
    };
    if stored.is_ok() {
        remove_copies_scratch(copies, &work_dir).await;
        enforce_work_dir_quota(library, req, &work_dir).await?;
    }
    stored?;
    let mut result = finish_copies(progress.result)?;
    // True when any copy was stored from the library's audio: copies made
    // before a fallback to the store keep it.
    result.from_library = from_library && progress.library_copies > 0;
    result.store_fetches = store_fetches;
    Ok(result)
}

/// Stores the copies still to make from the title's own store download,
/// after the media worker failed on a library's copy
/// ([`AcquireError::Media`]): its tags read, so it was taken, but its audio
/// does not package or decode. A retry would ask the library again and get
/// the same copy, so the acquire fetches from the store instead.
///
/// A media failure stops a copy before any of its files reach a
/// destination, so the copy that failed and every copy after it are made
/// from the store download; copies already stored keep the library's audio,
/// which was recorded as taken before anything was stored. The library's
/// copy leaves scratch before the store download starts, so the two never
/// hold the scratch quota together.
///
/// # Errors
///
/// Returns the store fetch's failure, or the first failure storing a copy
/// from it.
async fn store_rest_from_store(
    library: &LibraryStore,
    copies: &[CopyTarget],
    req: &AcquireRequest,
    source: &dyn ContentSource,
    work_dir: &Path,
    progress: &mut CopiesProgress,
    err: &bookclerk_media::MediaError,
) -> Result<()> {
    tracing::warn!(
        id = %status_key(req),
        error = %err,
        stored = progress.next,
        "the library's copy failed packaging; storing the rest from the store"
    );
    // The library's copy and whatever its packaging left go; the store
    // download starts in an empty work directory.
    remove_scratch_dir(work_dir, "failed to clean acquire cache dir").await;
    tokio::fs::create_dir_all(work_dir).await?;
    req.library_copies.stored_from().clear();
    progress.from_store = true;
    let own = fetch_title_enforcing_quota(library, req, source, work_dir).await?;
    enforce_work_dir_quota(library, req, work_dir).await?;
    run_stage_enforcing_quota(library, req, work_dir, async {
        store_copies_from(library, copies, req, work_dir, &own, None, progress).await
    })
    .await
}

/// Name of the directory, inside an acquire's scratch directory, that a
/// library's copy is downloaded into.
const LIBRARY_COPY_DIR: &str = "library-copy";

/// How far an acquire got storing its copies ([`store_copies_from`]).
#[derive(Debug, Default)]
struct CopiesProgress {
    /// The acquire's result so far: the first copy that wrote a key, with
    /// every key written ([`fold_copy_result`]).
    result: Option<AcquireResult>,
    /// Index of the next copy to store.
    next: usize,
    /// How many copies were stored from a library's audio.
    library_copies: usize,
    /// True once the copies are made from the title's own store download
    /// after a library's copy failed packaging ([`store_rest_from_store`]).
    from_store: bool,
}

/// Packages and stores each copy of the fetched title, in order, from the
/// one download in `work_dir` ([`store_plain_fetch`] per copy, against the
/// copy's own destinations and options).
///
/// With one copy (the default), the copy is made in `work_dir` itself, as
/// before copies existed. With several, each is made in its own
/// subdirectory, removed once stored, so the download stays for the next
/// copy. The result is the first copy's, with every copy's written keys:
/// the first copy's primary key is the title's.
///
/// # Errors
///
/// Returns the first copy's failure; copies already stored stay stored.
#[cfg(test)]
async fn store_copies(
    library: &LibraryStore,
    copies: &[CopyTarget],
    req: &AcquireRequest,
    work_dir: &Path,
    fetch: PlainFetch,
    library_pdf: Option<&Path>,
) -> Result<AcquireResult> {
    check_copy_folders(library, copies, req).await?;
    let mut progress = CopiesProgress::default();
    store_copies_from(
        library,
        copies,
        req,
        work_dir,
        &fetch,
        library_pdf,
        &mut progress,
    )
    .await?;
    remove_copies_scratch(copies, work_dir).await;
    finish_copies(progress.result)
}

/// Stores the copies from `progress.next` on, from `fetch`, advancing
/// `progress` past each copy stored (see [`store_copies`] for where each
/// copy is made).
///
/// # Errors
///
/// Returns the first copy's failure; `progress` then names that copy as
/// the next, and copies already stored stay stored.
async fn store_copies_from(
    library: &LibraryStore,
    copies: &[CopyTarget],
    req: &AcquireRequest,
    work_dir: &Path,
    fetch: &PlainFetch,
    library_pdf: Option<&Path>,
    progress: &mut CopiesProgress,
) -> Result<()> {
    let several = copies.len() > 1;
    while let Some(target) = copies.get(progress.next) {
        let at = progress.next;
        let mut copy_req = req.clone();
        copy_req.options = target.destinations.primary_destination().options.clone();
        copy_req.destination_keys = crate::keys::DestinationKeys::of(
            target.destinations.primary_destination().backend.as_ref(),
        );
        // The first copy stores and records the title's companion PDF as an
        // acquire of one copy does; a later copy's PDF goes only to the
        // destinations no earlier copy reached ([`store_extra_copy_pdf`]).
        let extra_pdf = at > 0 && copy_req.options.download_pdf;
        if extra_pdf {
            copy_req.options.download_pdf = false;
        }
        let copy_dir = if several {
            let dir = work_dir.join(format!("copy-{at}"));
            tokio::fs::create_dir_all(&dir).await?;
            dir
        } else {
            work_dir.to_path_buf()
        };
        tracing::info!(
            id = %status_key(req),
            copy = %target.copy,
            destinations = target.destinations.len(),
            "storing a copy"
        );
        // Boxed: the packaging future is large, and an acquire already runs
        // deep in its caller's stack.
        let stored = Box::pin(store_plain_fetch(
            library,
            &target.destinations,
            &copy_req,
            &copy_dir,
            fetch.clone(),
            library_pdf,
        ))
        .await;
        let stored = match stored {
            Ok(stored) => stored,
            // A later copy no destination stored, once an earlier one is:
            // the acquire keeps what is stored, and the copy is retried on
            // each destination that was to receive it. A packaging failure
            // still stops here, so a library's copy falls back to the store.
            Err(err)
                if !matches!(err, AcquireError::Media(_))
                    && progress
                        .result
                        .as_ref()
                        .is_some_and(|result| !result.storage_key.is_empty()) =>
            {
                let error = err.to_string();
                tracing::warn!(
                    id = %status_key(req),
                    copy = %target.copy,
                    %error,
                    "a copy was not stored; it is retried in the background"
                );
                let copies = target
                    .destinations
                    .items
                    .iter()
                    .filter(|dest| {
                        copy_req
                            .write_destinations
                            .as_deref()
                            .is_none_or(|only| only.is_empty() || only.contains(&dest.id))
                    })
                    .map(|dest| CopyOutcome {
                        destination: dest.id.clone(),
                        result: CopyResult::Failed {
                            error: error.clone(),
                        },
                    })
                    .collect();
                fold_copy_result(
                    &mut progress.result,
                    AcquireResult {
                        asin: req.asin.clone(),
                        storage_key: String::new(),
                        written_keys: Vec::new(),
                        matched_existing: false,
                        from_library: false,
                        // The caller sets the count once every copy is made.
                        store_fetches: 0,
                        copies,
                    },
                );
                if several {
                    remove_scratch_dir(&copy_dir, "failed to clean a copy's scratch dir").await;
                }
                progress.next += 1;
                continue;
            }
            Err(err) => return Err(err),
        };
        let mut stored = stored;
        if extra_pdf {
            copy_req.options.download_pdf = true;
            let served = &copies[..at];
            stored.written_keys.extend(
                store_extra_copy_pdf(
                    library,
                    &target.destinations,
                    served,
                    &copy_req,
                    &copy_dir,
                    CompanionPdf::of(fetch.pdf_url.as_deref(), library_pdf),
                )
                .await,
            );
        }
        // Some packaging paths leave their scratch behind; a finished
        // copy's files are stored, so they go before the next copy is made
        // and several copies never hold the scratch quota together.
        if several {
            remove_scratch_dir(&copy_dir, "failed to clean a copy's scratch dir").await;
        }
        fold_copy_result(&mut progress.result, stored);
        if !progress.from_store {
            progress.library_copies += 1;
        }
        progress.next += 1;
    }
    Ok(())
}

/// Removes what an acquire of several copies left in scratch: the work
/// directory with the download every copy was made from. With one copy
/// the work directory is the copy's, and the caller cleans it.
async fn remove_copies_scratch(copies: &[CopyTarget], work_dir: &Path) {
    if copies.len() > 1 {
        remove_scratch_dir(work_dir, "failed to clean acquire cache dir").await;
    }
}

/// Removes `dir`, logging `what` when it exists and cannot be removed.
async fn remove_scratch_dir(dir: &Path, what: &str) {
    if let Err(err) = tokio::fs::remove_dir_all(dir).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %dir.display(), error = %err, "{what}");
        }
    }
}

/// The acquire's result once every copy is stored: refused when no copy
/// was stored or none wrote a key (every destination that was to be
/// written refuses every copy, as with `refetch_missing` choosing a
/// destination whose copy rules take none), so an acquire never records an
/// empty storage key.
///
/// # Errors
///
/// Returns [`AcquireError::NoDestinationAccepts`] in both cases.
fn finish_copies(result: Option<AcquireResult>) -> Result<AcquireResult> {
    result
        .filter(|result| !result.storage_key.is_empty())
        .ok_or(AcquireError::NoDestinationAccepts)
}

/// Folds one stored copy's result into the acquire's: the first copy that
/// writes a key names the acquire, and every other key it writes joins its
/// written keys. A copy whose destinations were all skipped writes no key,
/// so it never names the acquire over a copy that did.
///
/// Each destination's outcome joins the acquire's: a destination that
/// failed any copy counts as failed, so its copy is retried.
fn fold_copy_result(result: &mut Option<AcquireResult>, stored: AcquireResult) {
    match result.as_mut() {
        None => *result = Some(stored),
        // A copy whose destinations were all skipped writes no key; the
        // first copy that does write names the acquire instead.
        Some(first) if first.storage_key.is_empty() && !stored.storage_key.is_empty() => {
            let mut stored = stored;
            stored.written_keys.append(&mut first.written_keys);
            let earlier = std::mem::take(&mut first.copies);
            merge_copy_outcomes(&mut stored.copies, earlier);
            *first = stored;
        }
        Some(first) => {
            if !stored.storage_key.is_empty() {
                first.written_keys.push(stored.storage_key);
            }
            first.written_keys.extend(stored.written_keys);
            merge_copy_outcomes(&mut first.copies, stored.copies);
        }
    }
}

/// Adds `more` to `outcomes`, one outcome per destination: a destination
/// already listed keeps its outcome unless `more` failed there.
fn merge_copy_outcomes(outcomes: &mut Vec<CopyOutcome>, more: Vec<CopyOutcome>) {
    for outcome in more {
        match outcomes
            .iter_mut()
            .find(|known| known.destination == outcome.destination)
        {
            Some(known) => {
                if matches!(outcome.result, CopyResult::Failed { .. }) {
                    *known = outcome;
                }
            }
            None => outcomes.push(outcome),
        }
    }
}

/// Refuses an acquire whose copies would share a folder: two copies whose
/// planned title folders meet, on one destination or across several.
///
/// Each copy beside the main output sits under a folder named after it,
/// so this holds unless a destination's own template copies a label, or a
/// tight path limit shortens two labels alike. Two copies sharing a folder
/// would overwrite each other's files, and the library could no longer
/// tell the main copy's files from another copy's by their key, so the
/// acquire stops before anything is stored.
///
/// # Errors
///
/// Returns [`AcquireError::Other`] naming the folder and both copies.
async fn check_copy_folders(
    library: &LibraryStore,
    copies: &[CopyTarget],
    req: &AcquireRequest,
) -> Result<()> {
    if copies.len() < 2 {
        return Ok(());
    }
    let mut owners: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (at, target) in copies.iter().enumerate() {
        for dest in &target.destinations.items {
            let copy_req = request_for_destination(req, dest);
            for folder in planned_title_folders(library, &copy_req).await {
                let owner = *owners.entry(folder.clone()).or_insert(at);
                if owner != at {
                    return Err(AcquireError::Other(anyhow::anyhow!(
                        "the copies {} and {} would share the folder `{folder}` \
                         (destination {}); give the destinations templates or path \
                         limits that keep each copy's folder apart",
                        copies[owner].copy,
                        target.copy,
                        dest.id
                    )));
                }
            }
        }
    }
    Ok(())
}

/// True when any copy the request makes is split by chapter, so the audio
/// it starts from must be MP4 (the chapter splitter refuses MP3).
fn splits_any_copy(req: &AcquireRequest) -> bool {
    req.options.wants_split_by_chapter()
        || req
            .options
            .copies_to_make()
            .iter()
            .any(|copy| copy.format == bookclerk_config::CopyFormat::Mp3Chapters)
}

/// True when every copy the request makes keeps the files as the store
/// delivered them (`original`), so nothing is decoded or repackaged.
fn keeps_delivered(req: &AcquireRequest) -> bool {
    req.options
        .copies_to_make()
        .iter()
        .all(|copy| copy.format == bookclerk_config::CopyFormat::Original)
}

/// True when an MP3 the store delivers is encoded again at the request's
/// quality instead of stored as delivered: the request makes MP3 and the
/// library keeps several copies (`output.copies`), so each copy must hold
/// the audio its quality names. A library with one copy keeps a store's
/// MP3 as delivered.
fn reencodes_mp3(req: &AcquireRequest) -> bool {
    req.options.wants_mp3() && !req.options.copies.is_empty()
}

/// Encodes each part of `plain` to MP3 at the request's quality, under
/// `work_dir`, for a request that [`reencodes_mp3`].
///
/// # Errors
///
/// Returns the encoder's error when a part does not encode.
async fn encode_plain_parts(
    req: &AcquireRequest,
    work_dir: &Path,
    mut plain: PlainFetch,
) -> Result<PlainFetch> {
    let dir = work_dir.join("encoded");
    std::fs::create_dir_all(&dir)?;
    for (index, part) in plain.parts.iter_mut().enumerate() {
        let out = dir.join(format!("part-{index:04}.mp3"));
        encode_to_mp3(
            &part.path,
            &out,
            &req.options.lame,
            req.options.max_sample_rate,
            None,
        )
        .await?;
        part.path = out;
    }
    Ok(plain)
}

/// A copy a library offered of the request's title.
struct OfferedCopy {
    /// The copy.
    copy: Box<dyn crate::LibraryCopy>,
    /// The store the library downloaded it from.
    store: String,
    /// True when that is the title's own store.
    same_store: bool,
}

/// The copy one of the libraries the request asks
/// ([`AcquireRequest::library_copies`]) offers of its title, when the
/// library knows which store the copy came from.
async fn offered_copy(library: &LibraryStore, req: &AcquireRequest) -> Option<OfferedCopy> {
    let copies = req.library_copies.copies()?;
    let title = resolve_title(library, req).await?;
    let copy = copies
        .copy_of(LibraryCopyListing {
            store: &title.source,
            plugin_key: &title.plugin_key,
            account_id: &title.account_id,
            marketplace: &title.marketplace,
            product_id: &title.product_id,
        })
        .await?;
    let store = copy
        .source_store()
        .map(str::trim)
        .filter(|store| !store.is_empty())?
        .to_owned();
    let same_store = from_store(Some(&store), &title.source);
    Some(OfferedCopy {
        copy,
        store,
        same_store,
    })
}

/// Downloads the copy a library offered into scratch and, for a copy from
/// the title's own store, records it as taken
/// ([`crate::LibraryCopy::taken`], before anything is stored).
///
/// A copy that cannot be downloaded whole within the scratch budget, read
/// by the confined tag reader, or recorded as taken is not used: what the
/// attempt left in `work_dir` is removed and the caller uses the store's
/// download.
///
/// # Returns
///
/// The copy's files, or `None` to use the store's download.
async fn take_library_copy(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
    offer: &OfferedCopy,
    cover: CopyCover,
) -> Option<LibraryFetch> {
    // The work directory already holds the store's download when a copy
    // from another store is compared.
    let budget = remaining_for_new_file(library, req, work_dir).await.ok()?;
    let dir = work_dir.join(LIBRARY_COPY_DIR);
    let media = req.library_copies.media();
    let wants = CopyWants {
        // A copy from another store replaces a download that already
        // carries the store's own PDF.
        pdf: offer.same_store && req.options.download_pdf,
        // Any copy split by chapter needs MP4 audio to split, whichever
        // copy is the main output.
        mp4_audio_only: splits_any_copy(req),
        skip_cover: matches!(cover, CopyCover::Skip),
    };
    let taken = match fetch_library_copy(offer.copy.as_ref(), &dir, budget, &media, wants).await {
        Ok(taken) => taken,
        Err(err) => {
            tracing::warn!(
                id = %status_key(req),
                error = %err,
                "the library's copy could not be taken; using the store's download"
            );
            remove_library_copy(&dir).await;
            return None;
        }
    };
    if offer.same_store {
        return record_taken(req, offer, &dir).await.then_some(taken);
    }
    // A copy from another store is recorded by the caller, once this
    // install's own probe of the file agrees that it is the better copy.
    Some(taken)
}

/// Whether an acquire downloads the cover of a library's copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyCover {
    /// The acquire has no cover of its own.
    Take,
    /// The acquire keeps its own store download's cover.
    Skip,
}

/// True when this install could store audio like `probe` under the
/// request's output options, going by what a library says of its copy.
///
/// Output that keeps the files as delivered stores anything. Every other
/// output decodes or repackages the audio in the media worker, which must
/// open that kind of file ([`bookclerk_media::capabilities`]). The encoding
/// itself is checked on the downloaded file ([`output_can_decode`]).
fn output_can_use(req: &AcquireRequest, probe: &FileProbe) -> bool {
    keeps_delivered(req) || bookclerk_media::capabilities::reads_content_type(&probe.content_type)
}

/// True when the request's output can be made from audio encoded as
/// `codec`: any audio when the files are kept as delivered, and otherwise
/// only what the media worker decodes ([`bookclerk_media::capabilities`]).
fn output_can_decode(req: &AcquireRequest, codec: Option<bookclerk_tags::AudioCodec>) -> bool {
    keeps_delivered(req) || codec.is_some_and(bookclerk_media::capabilities::decodes)
}

/// Records the offered copy as taken, before anything of it is stored: a
/// store step that fails part way can leave the library's audio on a
/// destination, and that audio must already be known as the library's.
///
/// # Returns
///
/// False when the record could not be written; the copy's files are then
/// removed from scratch (`dir`) and it is not used.
async fn record_taken(req: &AcquireRequest, offer: &OfferedCopy, dir: &Path) -> bool {
    if let Err(err) = offer.copy.taken().await {
        tracing::warn!(
            id = %status_key(req),
            error = %err,
            "the library's copy could not be recorded as taken; using the store's download"
        );
        remove_library_copy(dir).await;
        return false;
    }
    tracing::info!(
        id = %status_key(req),
        store = %offer.store,
        "taking the copy the library holds"
    );
    true
}

/// Which audio an acquire stores after comparing its own store download
/// with a library's copy from another store.
enum Compared {
    /// The store's download: it is at least as good, or the library's
    /// copy could not be used.
    Own(PlainFetch),
    /// The library's audio, with the store download's companion PDF and
    /// cover (the library's cover only when the store sent none).
    Library(PlainFetch),
}

/// The audio file of a fetch whose probe stands for the whole: the
/// single file, or the first part.
fn primary_audio(fetch: &PlainFetch) -> Option<&Path> {
    fetch
        .m4b_path
        .as_deref()
        .or_else(|| fetch.parts.first().map(|part| part.path.as_path()))
}

/// Probes `path` in the confined media worker.
///
/// # Returns
///
/// The probe and the audio's encoding (`None` when the tag reader does
/// not know it), or `None` when the file cannot be read as audio.
async fn probe_scratch_audio(
    media: &bookclerk_media::MediaPool,
    path: &Path,
) -> Option<(FileProbe, Option<bookclerk_tags::AudioCodec>)> {
    let size = tokio::fs::metadata(path).await.ok()?.len();
    let record = media
        .read_tags(size, &bookclerk_media::FileRange::new(path))
        .await
        .ok()?;
    let probe = bookclerk_media::AudioProbe::from_record(size, &record);
    Some((
        FileProbe {
            content_type: probe.content_type.to_owned(),
            bitrate_kbps: probe
                .bitrate_kbps
                .map(|kbps| kbps.min(FileProbe::MAX_BITRATE_KBPS)),
            lossless: probe.lossless,
            source_store: None,
        },
        record.codec,
    ))
}

/// Compares the title's own store download with the copy a library holds
/// from another store, once, and keeps the better of the two
/// ([`FileProbe::is_better_than`]): lossless over lossy, then the higher
/// bitrate.
///
/// The library's copy is downloaded only when the library's own probe says
/// it is better, and replaces the store download only when this install's
/// probe of the downloaded file agrees and the request's output can use
/// that kind of audio ([`output_can_use`], [`output_can_decode`]). In every other case (equal copies,
/// a download that cannot be probed, a copy that does not arrive or cannot
/// be recorded as taken) the store download is kept.
async fn replace_with_better_copy(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
    offer: &OfferedCopy,
    own: PlainFetch,
) -> Compared {
    let media = req.library_copies.media();
    let Some(own_audio) = primary_audio(&own).map(Path::to_path_buf) else {
        return Compared::Own(own);
    };
    let Some((own_probe, _)) = probe_scratch_audio(&media, &own_audio).await else {
        return Compared::Own(own);
    };
    let said = offer.copy.probe();
    if !said.is_better_than(&own_probe) || !output_can_use(req, &said) {
        tracing::debug!(
            id = %status_key(req),
            copy_store = %offer.store,
            "the library's copy from another store is no better, or this output cannot use it; keeping the store's download"
        );
        return Compared::Own(own);
    }
    let cover = if own.cover_path.is_some() {
        CopyCover::Skip
    } else {
        CopyCover::Take
    };
    let Some(taken) = take_library_copy(library, req, work_dir, offer, cover).await else {
        return Compared::Own(own);
    };
    let dir = work_dir.join(LIBRARY_COPY_DIR);
    let theirs = match primary_audio(&taken.fetch) {
        Some(path) => probe_scratch_audio(&media, path).await,
        None => None,
    };
    if !theirs.is_some_and(|(theirs, codec)| {
        theirs.is_better_than(&own_probe)
            && output_can_use(req, &theirs)
            && output_can_decode(req, codec)
    }) {
        tracing::warn!(
            id = %status_key(req),
            copy_store = %offer.store,
            "the library's copy is not the better copy it was said to be; keeping the store's download"
        );
        remove_library_copy(&dir).await;
        return Compared::Own(own);
    }
    // Both downloads are in scratch now. If another job took the room
    // meanwhile, the copy goes and the store's download stays.
    if let Err(err) = enforce_work_dir_quota(library, req, work_dir).await {
        tracing::warn!(
            id = %status_key(req),
            error = %err,
            "no scratch room for the library's copy; keeping the store's download"
        );
        remove_library_copy(&dir).await;
        return Compared::Own(own);
    }
    if !record_taken(req, offer, &dir).await {
        return Compared::Own(own);
    }
    // The store's audio is not stored: free its scratch space now.
    for path in own
        .m4b_path
        .iter()
        .chain(own.parts.iter().map(|part| &part.path))
    {
        let _ = tokio::fs::remove_file(path).await;
    }
    req.library_copies.stored_from().set(&offer.store);
    // Only the audio is replaced: the store's cover stays, and the
    // library's (downloaded only then) stands in when the store sent none.
    let mut better = taken.fetch;
    if own.cover_path.is_some() {
        better.cover_path = own.cover_path;
    }
    better.pdf_url = own.pdf_url;
    Compared::Library(better)
}

/// Removes what a library copy left in scratch, so the store's download
/// is packaged with only its own files.
async fn remove_library_copy(dir: &Path) {
    if let Err(err) = tokio::fs::remove_dir_all(dir).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %err, "could not remove the library copy from scratch");
        }
    }
}

/// Packages or stores a source `PlainFetch` according to output options and chapter overlay.
async fn store_plain_fetch(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    work_dir: &Path,
    plain: PlainFetch,
    library_pdf: Option<&Path>,
) -> Result<AcquireResult> {
    let want_mp3 = req.options.wants_mp3();
    let want_opus = req.options.wants_opus();
    let multi = plain.parts.len() > 1;
    let audible_overlay_possible = plain_source_has_audible_asin(library, req).await;

    // No-op output: keep store-delivered bytes (no remux/transcode).
    if req.options.is_noop_output() {
        if multi {
            return store_plain_parts(library, destinations, req, work_dir, plain, library_pdf)
                .await;
        }
        let path = plain
            .m4b_path
            .clone()
            .or_else(|| plain.parts.first().map(|p| p.path.clone()))
            .ok_or_else(|| {
                AcquireError::Other(anyhow::anyhow!(
                    "source `{}` returned no audio for {}",
                    req.source,
                    req.asin
                ))
            })?;
        return store_plain_parts(
            library,
            destinations,
            req,
            work_dir,
            PlainFetch {
                parts: vec![bookclerk_source::PlainAudioPart {
                    path,
                    title: None,
                    duration_ms: None,
                }],
                m4b_path: None,
                cover_path: plain.cover_path,
                chapters: plain.chapters,
                pdf_url: plain.pdf_url,
            },
            library_pdf,
        )
        .await;
    }

    // Multi-part "split by chapter" without enrichment: store parts as-is.
    // When an Audible ASIN enrichment is available, package first so we can embed /
    // split by the literary chapter tree instead of track-boundary placeholders.
    if multi && req.options.wants_split_by_chapter() && !audible_overlay_possible {
        let plain = if reencodes_mp3(req) {
            encode_plain_parts(req, work_dir, plain).await?
        } else {
            plain
        };
        return store_plain_parts(library, destinations, req, work_dir, plain, library_pdf).await;
    }

    let mut chapters = plain.chapters.clone();
    let mut replace_chapters = false;
    let mut acquired_path = if let Some(m4b) = plain.m4b_path.clone() {
        m4b
    } else if plain.parts.is_empty() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "source `{}` returned no audio for {}",
            req.source,
            req.asin
        )));
    } else if plain.parts.len() == 1 && (want_mp3 || want_opus) && !audible_overlay_possible {
        plain.parts[0].path.clone()
    } else {
        // Package MP3/M4A part(s) into M4B (single-file M4B target, multi→single, or
        // Audible chapter overlay that needs a contiguous timeline before chapter split).
        let titles: Vec<String> = plain
            .parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                p.title
                    .clone()
                    .unwrap_or_else(|| format!("Chapter {}", i + 1))
            })
            .collect();
        let out = work_dir.join(format!("{}.m4b", status_key(req)));
        let (outcome, packaged_chapters) = package_m4b_from_mp3(PackageM4bRequest {
            parts: plain.parts.iter().map(|p| p.path.clone()).collect(),
            output: out,
            chapter_titles: titles,
        })
        .await?;
        if chapters.is_empty() {
            chapters = packaged_chapters;
        }
        outcome.output
    };

    // Store track markers (Chirp/Libro) are not literary chapters. When the row
    // has an enriched Audible ASIN, overlay Audible's chapter tree and shift
    // timestamps past Audible brand intro/outro (absent from plain audio).
    let mut catalog = PlainAudibleCatalog::default();
    let mut plain_chapter_tree: Option<Value> = None;
    if audible_overlay_possible {
        let plain_duration = probe_audio_duration_ms(&acquired_path);
        catalog = fetch_plain_catalog_overlay(library, req, work_dir).await;
        if let Some((overlaid, tree)) =
            overlay_audible_chapters_for_plain(library, req, plain_duration).await
        {
            let audible_asin = resolve_title(library, req)
                .await
                .and_then(|t| t.asin.clone());
            tracing::info!(
                id = %status_key(req),
                source = %req.source,
                audible_asin = ?audible_asin,
                chapters = overlaid.len(),
                plain_duration_ms = ?plain_duration,
                "overlaying Audible chapter tree onto plain audio"
            );
            // Local ±5s speech-band snap: cheap (decode only small windows),
            // places markers up to 2s before the spoken-title onset without
            // crossing prior vocal energy (helps with music beds too).
            let aligned = align_chapter_starts_async(
                &acquired_path,
                &overlaid,
                ChapterAlignOptions::default(),
            )
            .await;
            let mut start_map = std::collections::HashMap::new();
            for ((_, old), (_, new)) in overlaid.iter().zip(aligned.iter()) {
                start_map.insert(*old, *new);
            }
            plain_chapter_tree = Some(apply_start_map_to_chapter_tree(&tree, &start_map));
            chapters = aligned;
            replace_chapters = true;
        }
    }

    let flat_chapters: Vec<FlatChapter> = chapters
        .iter()
        .map(|(title, start_ms)| FlatChapter {
            title: title.clone(),
            start_ms: *start_ms,
        })
        .collect();
    let will_split = req.options.wants_split_by_chapter() && flat_chapters.len() > 1;

    if want_mp3 && !will_split {
        let ext = acquired_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !ext.eq_ignore_ascii_case("mp3") || reencodes_mp3(req) {
            let mut mp3_out = work_dir.join(format!("{}.mp3", status_key(req)));
            if mp3_out == acquired_path {
                mp3_out = work_dir.join(format!("{}.encoded.mp3", status_key(req)));
            }
            encode_to_mp3(
                &acquired_path,
                &mp3_out,
                &req.options.lame,
                req.options.max_sample_rate,
                None,
            )
            .await?;
            acquired_path = mp3_out;
        }
    }
    if want_opus {
        // Like MP3 (`reencodes_mp3`): a store's Opus file is kept as
        // delivered unless the library keeps several copies, which must
        // each hold the audio their quality names.
        let ext = acquired_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !ext.eq_ignore_ascii_case("opus") || !req.options.copies.is_empty() {
            let mut opus_out = work_dir.join(format!("{}.opus", status_key(req)));
            if opus_out == acquired_path {
                opus_out = work_dir.join(format!("{}.encoded.opus", status_key(req)));
            }
            encode_to_opus(
                &acquired_path,
                &opus_out,
                OpusEncodeOptions {
                    bitrate_kbps: req.options.opus.kbps,
                    mono: req.options.opus.mono,
                },
                None,
            )
            .await?;
            acquired_path = opus_out;
        }
    }

    let ext = if will_split && want_mp3 {
        "mp3".to_string()
    } else {
        acquired_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_else(|| fallback_audio_ext(&req.options))
            .to_string()
    };

    // Prefer Audible catalog cover when enrichment matched an ASIN.
    let cover_path = catalog.cover_path.clone().or(plain.cover_path.clone());
    if req.options.fixup_metadata && !will_split {
        let fixed = work_dir.join(format!("{}.fixed.{}", status_key(req), ext));
        match fixup_audiobook(
            build_fixup_request(
                library,
                req,
                acquired_path.clone(),
                fixed.clone(),
                cover_path.clone(),
                chapters,
                None,
                replace_chapters,
                &catalog,
            )
            .await,
        )
        .await
        {
            Ok(outcome) => acquired_path = outcome.output,
            Err(err) => {
                tracing::warn!(
                    id = %status_key(req),
                    error = %err,
                    "metadata fixup failed; storing pre-fixup audio"
                );
            }
        }
    }

    let stored_keys = if will_split {
        let total_ms = probe_audio_duration_ms(&acquired_path)
            .or_else(|| {
                flat_chapters
                    .last()
                    .map(|c| c.start_ms.saturating_add(600_000))
            })
            .unwrap_or(3_600_000);
        let split_dir = work_dir.join("chapters");
        let (folder_ctx, file_ctx) = naming_contexts(library, req).await;
        let split_chapters = split_audio_by_chapters(
            &acquired_path,
            &split_dir,
            &flat_chapters,
            total_ms,
            &folder_ctx,
            &file_ctx,
            &req.options,
            "m4b",
        )
        .await?;
        let mut prepared = Vec::new();
        for (idx, ch) in split_chapters.into_iter().enumerate() {
            let mut chapter_path = ch.path;
            let mut chapter_ext = "m4b".to_string();
            if want_mp3 {
                let mp3_path = chapter_path.with_extension("mp3");
                encode_to_mp3(
                    &chapter_path,
                    &mp3_path,
                    &req.options.lame,
                    req.options.max_sample_rate,
                    None,
                )
                .await?;
                chapter_path = mp3_path;
                chapter_ext = "mp3".to_string();
            }
            if req.options.fixup_metadata {
                let fixed = chapter_path.with_extension(format!("fixed.{}", ext));
                let chapter_chapters = vec![(ch.title.clone(), 0u64)];
                match fixup_audiobook(
                    build_fixup_request(
                        library,
                        req,
                        chapter_path.clone(),
                        fixed.clone(),
                        cover_path.clone(),
                        chapter_chapters,
                        Some(format!("{} — {}", req.title, ch.title)),
                        true,
                        &catalog,
                    )
                    .await,
                )
                .await
                {
                    Ok(outcome) => chapter_path = outcome.output,
                    Err(err) => {
                        tracing::warn!(
                            id = %status_key(req),
                            chapter = idx + 1,
                            error = %err,
                            "chapter metadata fixup failed"
                        );
                    }
                }
            }
            prepared.push(PreparedAudioFile {
                path: chapter_path,
                title: ch.title,
                ext: chapter_ext,
            });
        }
        store_prepared_audio_files(
            library,
            destinations,
            req,
            &prepared,
            AudioKeyPlan::SplitChapters,
        )
        .await?
    } else {
        let prepared = [PreparedAudioFile {
            path: acquired_path.clone(),
            title: req.title.clone(),
            ext: ext.clone(),
        }];
        store_prepared_audio_files(library, destinations, req, &prepared, AudioKeyPlan::Single)
            .await?
    };

    if let Some(cover) = cover_path.as_ref() {
        for stored in &stored_keys.keys {
            if let Some(dest) = destinations.destination(&stored.id) {
                let dest_req = request_for_destination(req, dest);
                if dest_req.options.download_cover {
                    let cover_key = sidecar_key(&stored.key, "jpg");
                    let asin_str = object_asin_for(library, &dest_req).await;
                    let meta =
                        sidecar_meta(asin_str.as_str(), &dest_req.title, "image/jpeg", cover).await;
                    if let Err(err) = dest.backend.put_file(&cover_key, cover, meta).await {
                        tracing::warn!(id = %status_key(req), error = %err, "cover store failed");
                    }
                }
            }
        }
    }

    // Flat sidecars for players/tools that ignore embedded Nero/QuickTime chapters;
    // also persist the provider's nested chapter tree (timestamp-adjusted) when available.
    for stored in &stored_keys.keys {
        if let Some(dest) = destinations.destination(&stored.id) {
            let dest_req = request_for_destination(req, dest);
            let asin_str = object_asin_for(library, &dest_req).await;
            store_flat_chapter_sidecars(
                dest.backend.as_ref(),
                &dest_req,
                &stored.key,
                work_dir,
                &flat_chapters,
                asin_str.as_str(),
            )
            .await;
            if let Some(tree) = plain_chapter_tree.as_ref() {
                let asin_str2 = object_asin_for(library, &dest_req).await;
                store_chapter_tree_sidecar(
                    dest.backend.as_ref(),
                    &dest_req,
                    &stored.key,
                    work_dir,
                    tree,
                    asin_str2.as_str(),
                )
                .await;
            }
        }
    }

    let mut written_keys = stored_keys.all_keys();
    written_keys.extend(
        store_companion_pdf_sidecars(
            library,
            destinations,
            req,
            work_dir,
            CompanionPdf::of(plain.pdf_url.as_deref(), library_pdf),
        )
        .await?,
    );

    if let Err(err) = tokio::fs::remove_dir_all(work_dir).await {
        tracing::warn!(
            path = %work_dir.display(),
            error = %err,
            "failed to clean acquire cache dir"
        );
    }

    Ok(AcquireResult {
        asin: req.asin.clone(),
        storage_key: stored_keys.primary_key.clone(),
        written_keys,
        matched_existing: false,
        from_library: false,
        store_fetches: 0,
        copies: stored_keys.copies(),
    })
}

/// Stores a multi-part plain fetch as separate files and optional cover sidecars.
async fn store_plain_parts(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    work_dir: &Path,
    plain: PlainFetch,
    library_pdf: Option<&Path>,
) -> Result<AcquireResult> {
    let mut prepared = Vec::new();
    for (idx, part) in plain.parts.iter().enumerate() {
        let ext = part
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp3")
            .to_string();
        let title = part
            .title
            .clone()
            .unwrap_or_else(|| format!("Part {}", idx + 1));
        prepared.push(PreparedAudioFile {
            path: part.path.clone(),
            title,
            ext,
        });
    }

    let stored_keys = store_prepared_audio_files(
        library,
        destinations,
        req,
        &prepared,
        AudioKeyPlan::PlainParts,
    )
    .await?;

    if let Some(cover) = plain.cover_path.as_ref() {
        for stored in &stored_keys.keys {
            if let Some(dest) = destinations.destination(&stored.id) {
                let dest_req = request_for_destination(req, dest);
                if dest_req.options.download_cover {
                    let cover_key = sidecar_key(&stored.key, "jpg");
                    let asin_str = object_asin_for(library, &dest_req).await;
                    let meta =
                        sidecar_meta(asin_str.as_str(), &dest_req.title, "image/jpeg", cover).await;
                    if let Err(err) = dest.backend.put_file(&cover_key, cover, meta).await {
                        tracing::warn!(id = %status_key(req), error = %err, "cover store failed");
                    }
                }
            }
        }
    }

    let mut written_keys = stored_keys.all_keys();
    written_keys.extend(
        store_companion_pdf_sidecars(
            library,
            destinations,
            req,
            work_dir,
            CompanionPdf::of(plain.pdf_url.as_deref(), library_pdf),
        )
        .await?,
    );

    Ok(AcquireResult {
        asin: req.asin.clone(),
        storage_key: stored_keys.primary_key.clone(),
        written_keys,
        matched_existing: false,
        from_library: false,
        store_fetches: 0,
        copies: stored_keys.copies(),
    })
}

/// Download + store companion PDF sidecars when `output.download_pdf` is enabled.
///
/// A PDF a library's copy carried ([`CompanionPdf::Library`]) is stored
/// from its scratch file instead of downloaded; this runs before the
/// acquire's scratch directory is removed, so the file is still there.
///
/// Soft-fails (logs + returns `Ok(empty)`) when the URL is missing, HTTP fails,
/// or the remaining scratch budget cannot hold the body so audio acquire still
/// succeeds. Quota and write failures record [`AcquireStatus::Error`] on the
/// companion PDF so a later existing-media acquire can resume the PDF-only
/// side effect. Skips when a PDF is already marked acquired unless `req.force`.
/// Uses the planned single-book audio key per destination (not every chapter
/// file when splitting).
///
/// # Errors
///
/// Returns [`AcquireError`] when the remaining-budget lookup fails.
async fn store_companion_pdf_sidecars(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    work_dir: &Path,
    pdf: CompanionPdf<'_>,
) -> Result<Vec<String>> {
    if !req.options.download_pdf {
        return Ok(Vec::new());
    }
    let pdf_url = match pdf {
        CompanionPdf::Library(path) => {
            return Ok(store_library_companion_pdf(library, destinations, req, path).await);
        }
        CompanionPdf::Store(url) => url.map(str::trim).filter(|s| !s.is_empty()),
    };
    let Some(pdf_url) = pdf_url else {
        return Ok(Vec::new());
    };

    // A failed reuse check is a companion PDF failure; the audio this
    // runs after is already stored and keeps its status.
    if !req.force {
        match stored_edition_pdf(library, destinations, req).await {
            Ok(Some(_)) => return Ok(Vec::new()),
            Ok(None) => {}
            Err(err) => {
                record_companion_pdf_failure(library, req, &err).await;
                return Ok(Vec::new());
            }
        }
    }

    let remaining = remaining_for_new_file(library, req, work_dir).await?;
    if remaining == 0 {
        record_companion_pdf_failure(
            library,
            req,
            &AcquireError::Other(anyhow::anyhow!(
                "acquire scratch quota exceeded (0 bytes remaining)"
            )),
        )
        .await;
        return Ok(Vec::new());
    }
    let pdf_path = work_dir.join(format!("{}.pdf", req.asin));
    let client = download_http_client();
    let response = match client.get(pdf_url).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => ok,
            Err(err) => {
                tracing::warn!(id = %status_key(req), error = %err, "PDF download failed");
                return Ok(Vec::new());
            }
        },
        Err(err) => {
            tracing::warn!(id = %status_key(req), error = %err, "PDF download failed");
            return Ok(Vec::new());
        }
    };
    if let Err(err) = write_http_body_capped(response, &pdf_path, remaining).await {
        let _ = tokio::fs::remove_file(&pdf_path).await;
        record_companion_pdf_failure(library, req, &err).await;
        return Ok(Vec::new());
    }

    Ok(put_companion_pdf(library, destinations, req, &pdf_path).await)
}

/// Stores the companion PDF beside a copy other than the first, on each of
/// its destinations that no earlier copy of the acquire (`served`) wrote
/// to: a destination that takes only an extra copy (an MP3-only phone)
/// gets the PDF in that copy's folder, and one that also takes an earlier
/// copy keeps the one PDF it already has.
///
/// The title's recorded PDF key stays the first copy's: nothing is
/// recorded here. A destination already holding the PDF at the copy's key
/// is skipped unless the request is forced. The store's PDF is downloaded
/// only when some destination needs it; failures are logged and the copy's
/// audio keeps its status.
///
/// # Returns
///
/// The keys written.
async fn store_extra_copy_pdf(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    served: &[CopyTarget],
    req: &AcquireRequest,
    copy_dir: &Path,
    pdf: CompanionPdf<'_>,
) -> Vec<String> {
    let mut wanted = Vec::new();
    for dest in &destinations.items {
        let reached = served
            .iter()
            .any(|earlier| earlier.destinations.destination(&dest.id).is_some());
        if reached {
            continue;
        }
        let dest_req = request_for_destination(req, dest);
        if !dest_req.options.download_pdf {
            continue;
        }
        let pdf_key = sidecar_key(&planned_storage_key(library, &dest_req).await, "pdf");
        if !req.force && dest.backend.exists(&pdf_key).await.unwrap_or(false) {
            continue;
        }
        wanted.push((dest, dest_req, pdf_key));
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    let downloaded;
    let pdf_path = match pdf {
        CompanionPdf::Library(path) => path,
        CompanionPdf::Store(url) => {
            let Some(url) = url.map(str::trim).filter(|url| !url.is_empty()) else {
                return Vec::new();
            };
            downloaded = copy_dir.join(format!("{}.pdf", req.asin));
            if let Err(err) = download_copy_pdf(library, req, copy_dir, url, &downloaded).await {
                tracing::warn!(id = %status_key(req), error = %err, "copy PDF download failed");
                return Vec::new();
            }
            downloaded.as_path()
        }
    };
    let mut written = Vec::new();
    for (dest, dest_req, pdf_key) in wanted {
        let asin_str = object_asin_for(library, &dest_req).await;
        let meta = sidecar_meta(&asin_str, &dest_req.title, "application/pdf", pdf_path).await;
        match dest.backend.put_file(&pdf_key, pdf_path, meta).await {
            Ok(()) => written.push(pdf_key),
            Err(err) => {
                tracing::warn!(id = %status_key(req), error = %err, "copy PDF store failed");
            }
        }
    }
    written
}

/// Downloads the store's companion PDF at `url` to `path` in `copy_dir`,
/// within the scratch left beside what the work directory holds.
///
/// # Errors
///
/// Returns the budget lookup's, the request's or the capped write's
/// failure; a partial file is removed.
async fn download_copy_pdf(
    library: &LibraryStore,
    req: &AcquireRequest,
    copy_dir: &Path,
    url: &str,
    path: &Path,
) -> Result<()> {
    // Packaging may have removed the copy's scratch directory already.
    tokio::fs::create_dir_all(copy_dir).await?;
    let remaining = remaining_for_new_file(library, req, copy_dir).await?;
    let response = download_http_client()
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| AcquireError::Other(anyhow::anyhow!("PDF download failed: {err}")))?;
    if let Err(err) = write_http_body_capped(response, path, remaining).await {
        let _ = tokio::fs::remove_file(path).await;
        return Err(err);
    }
    Ok(())
}

/// Where an acquire's companion PDF comes from.
#[derive(Debug, Clone, Copy)]
enum CompanionPdf<'a> {
    /// The URL the store gave with the title, when it gave one.
    Store(Option<&'a str>),
    /// The file a library's copy carried, already in the acquire's
    /// scratch directory.
    Library(&'a Path),
}

impl<'a> CompanionPdf<'a> {
    /// The library's file when the acquire took one, else the store's URL.
    fn of(store_url: Option<&'a str>, library_pdf: Option<&'a Path>) -> Self {
        library_pdf.map_or(Self::Store(store_url), Self::Library)
    }
}

/// Stores the companion PDF a library's copy carried, as
/// [`store_companion_pdf_sidecars`] stores a downloaded one.
///
/// Nothing is written when companion PDFs are off, or when the edition
/// already stores one and the request is not forced. A failure is recorded
/// on the companion PDF, never on the audio stored before it.
///
/// # Returns
///
/// The keys written.
async fn store_library_companion_pdf(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    pdf_path: &Path,
) -> Vec<String> {
    if !req.options.download_pdf {
        return Vec::new();
    }
    if !req.force {
        match stored_edition_pdf(library, destinations, req).await {
            Ok(Some(_)) => return Vec::new(),
            Ok(None) => {}
            Err(err) => {
                record_companion_pdf_failure(library, req, &err).await;
                return Vec::new();
            }
        }
    }
    put_companion_pdf(library, destinations, req, pdf_path).await
}

/// Writes the companion PDF at `pdf_path` beside the planned audio key of
/// every destination and marks the PDF acquired under the primary
/// destination's key (the first written one when the primary write
/// failed). A destination whose write fails is logged and skipped.
///
/// # Returns
///
/// The keys written.
async fn put_companion_pdf(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    pdf_path: &Path,
) -> Vec<String> {
    let mut primary_pdf_key = None;
    let mut written = Vec::new();
    for dest in &destinations.items {
        let dest_req = request_for_destination(req, dest);
        let audio_key = planned_storage_key(library, &dest_req).await;
        let pdf_key = sidecar_key(&audio_key, "pdf");
        let asin_str = object_asin_for(library, &dest_req).await;
        let meta = sidecar_meta(&asin_str, &dest_req.title, "application/pdf", pdf_path).await;
        match dest.backend.put_file(&pdf_key, pdf_path, meta).await {
            Ok(()) => {
                if dest.id == destinations.primary {
                    primary_pdf_key = Some(pdf_key.clone());
                }
                written.push(pdf_key);
            }
            Err(err) => {
                tracing::warn!(id = %status_key(req), error = %err, "PDF store failed");
            }
        }
    }

    if let Some(pdf_key) = primary_pdf_key.or_else(|| written.first().cloned()) {
        if let Err(err) =
            record_pdf_status(library, req, AcquireStatus::Acquired, Some(&pdf_key)).await
        {
            tracing::warn!(id = %status_key(req), error = %err, "PDF status update failed");
        }
    }

    written
}

/// Records [`AcquireStatus::Error`] for a companion PDF without failing audio acquire.
async fn record_companion_pdf_failure(
    library: &LibraryStore,
    req: &AcquireRequest,
    err: &AcquireError,
) {
    tracing::warn!(
        id = %status_key(req),
        error = %err,
        "companion PDF failed; audio acquire continues"
    );
    if let Err(status_err) = record_pdf_status(library, req, AcquireStatus::Error, None).await {
        tracing::warn!(
            id = %status_key(req),
            error = %status_err,
            "PDF error status update failed"
        );
    }
}

/// Retries a missing companion PDF after audio is already stored.
///
/// Dedicated [`acquire_pdf_only`] still fails hard when PDF is the primary job.
/// This path swallows PDF errors so an existing-media acquire cannot flip the
/// book back to [`AcquireStatus::Error`].
///
/// # Returns
///
/// Whether the PDF was fetched from the store, or a fetch was attempted
/// and failed after it may have reached the store: either counts toward
/// the account's download pace ([`AcquireResult::store_fetches`]).
///
/// # Errors
///
/// Returns the store's `RateLimited` refusal, after recording it on
/// `pdf_status`, so the job pauses the account. Any other failure (checking
/// for an already stored PDF, downloading or storing it) is only recorded
/// on `pdf_status`.
async fn resume_missing_companion_pdf(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<bool> {
    if !req.options.download_pdf {
        return Ok(false);
    }
    if !req.force {
        match stored_edition_pdf(library, destinations, req).await {
            Ok(Some(_)) => return Ok(false),
            Ok(None) => {}
            Err(err) => {
                record_companion_pdf_failure(library, req, &err).await;
                return Ok(false);
            }
        }
    }
    match acquire_pdf_only_recorded(library, destinations, req, source).await {
        Ok(result) => Ok(result.store_fetches > 0),
        Err(err) => {
            record_companion_pdf_failure(library, req, &err).await;
            // The store asking the account to slow down is the job's to
            // act on (a cool-down), so it is not swallowed with the rest.
            if matches!(
                err,
                AcquireError::Source(bookclerk_source::SourceError::RateLimited(_))
            ) {
                return Err(err);
            }
            Ok(true)
        }
    }
}

/// The key of the companion PDF the request's edition already stores, when
/// the request's title may use it.
///
/// A PDF this title acquired is its own. One another title of the edition
/// acquired is used once it is present on the primary destination, and is
/// then recorded for this title too, as [`share_acquired_sibling`] shares
/// the audio.
///
/// # Errors
///
/// Returns an error when the primary destination cannot be checked or the
/// status write fails.
async fn stored_edition_pdf(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
) -> Result<Option<String>> {
    let Some(title) = resolve_title(library, req).await else {
        return Ok(None);
    };
    let Some(key) = title
        .pdf_storage_key
        .clone()
        .filter(|key| title.pdf_status == AcquireStatus::Acquired && !key.is_empty())
    else {
        return Ok(None);
    };
    if title.pdf_acquired_here() {
        return Ok(Some(key));
    }
    let primary = destinations.primary_destination();
    let primary = (primary.id.as_str(), primary.backend.as_ref());
    if !sibling_pdf_is_present(Some(&primary), Some(&key)).await? {
        return Ok(None);
    }
    library
        .set_holding_pdf_status(&title.holding_id, AcquireStatus::Acquired, Some(&key))
        .await?;
    Ok(Some(key))
}

/// Persist nested `chapters.tree.json` (provider layout with adjusted timestamps).
async fn store_chapter_tree_sidecar(
    storage: &dyn StorageBackend,
    req: &AcquireRequest,
    audio_key: &str,
    work_dir: &Path,
    tree: &Value,
    asin: &str,
) {
    if !req.options.chapter_json_tree() {
        return;
    }
    let json_path = work_dir.join(format!("{}.chapters.tree.json", req.asin));
    match tokio::fs::write(
        &json_path,
        serde_json::to_vec_pretty(tree).unwrap_or_default(),
    )
    .await
    {
        Ok(()) => {
            let key = sidecar_key(audio_key, "chapters.tree.json");
            let meta = sidecar_meta(asin, &req.title, "application/json", &json_path).await;
            if let Err(err) = storage.put_file(&key, &json_path, meta).await {
                tracing::warn!(
                    asin = %req.asin,
                    key = %key,
                    error = %err,
                    "tree chapter json store failed"
                );
            }
        }
        Err(err) => {
            tracing::warn!(asin = %req.asin, error = %err, "tree chapter json write failed");
        }
    }
}

/// Write flat `.cue` / `chapters.flat.json` sidecars from the embedded marker list.
async fn store_flat_chapter_sidecars(
    storage: &dyn StorageBackend,
    req: &AcquireRequest,
    audio_key: &str,
    work_dir: &Path,
    flat_chapters: &[FlatChapter],
    asin: &str,
) {
    if flat_chapters.is_empty() {
        return;
    }

    if req.options.create_cue {
        let cue_path = work_dir.join(format!("{}.cue", req.asin));
        let performer = req.authors.as_deref().unwrap_or("Unknown Author");
        match write_cue(
            &cue_path,
            &audio_basename(audio_key),
            performer,
            &req.title,
            flat_chapters,
        ) {
            Ok(()) => {
                let key = sidecar_key(audio_key, "cue");
                let meta = sidecar_meta(asin, &req.title, "application/x-cue", &cue_path).await;
                if let Err(err) = storage.put_file(&key, &cue_path, meta).await {
                    tracing::warn!(asin = %req.asin, key = %key, error = %err, "cue store failed");
                }
            }
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "cue write failed");
            }
        }
    }

    if req.options.chapter_json_flat() {
        let json_path = work_dir.join(format!("{}.chapters.flat.json", req.asin));
        let payload = serde_json::json!({
            "layout": "flat",
            "chapters": flat_chapters.iter().map(|c| {
                serde_json::json!({
                    "title": c.title,
                    "startOffsetMs": c.start_ms,
                })
            }).collect::<Vec<_>>(),
        });
        match tokio::fs::write(
            &json_path,
            serde_json::to_vec_pretty(&payload).unwrap_or_default(),
        )
        .await
        {
            Ok(()) => {
                let key = sidecar_key(audio_key, "chapters.flat.json");
                let meta = sidecar_meta(asin, &req.title, "application/json", &json_path).await;
                if let Err(err) = storage.put_file(&key, &json_path, meta).await {
                    tracing::warn!(
                        asin = %req.asin,
                        key = %key,
                        error = %err,
                        "flat chapter json store failed"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "flat chapter json write failed");
            }
        }
    }
}

/// Object metadata (content type, length, ASIN, timestamps) for a stored audio file.
async fn object_meta_for(
    library: &LibraryStore,
    req: &AcquireRequest,
    title: &str,
    content_type: &str,
    content_length: Option<u64>,
) -> ObjectMeta {
    let title_view = resolve_title(library, req).await;
    let created = resolve_timestamp(req.options.creation_time, title_view.as_ref());
    let modified = resolve_timestamp(req.options.last_write_time, title_view.as_ref());
    ObjectMeta {
        content_type: Some(content_type.into()),
        content_length,
        asin: Some(
            title_view
                .as_ref()
                .and_then(|t| t.asin.clone())
                .unwrap_or_else(|| req.asin.clone()),
        ),
        title: Some(title.to_string()),
        creation_time: created.map(system_time_rfc3339),
        last_write_time: modified.map(system_time_rfc3339),
        ..Default::default()
    }
}

/// Prefer enriched Audible ASIN for S3 object metadata when present; otherwise
/// the acquire product id (Audible ASIN or Libro ISBN).
async fn object_asin_for(library: &LibraryStore, req: &AcquireRequest) -> String {
    resolve_title(library, req)
        .await
        .and_then(|t| t.asin)
        .unwrap_or_else(|| req.asin.clone())
}

/// Formats a `SystemTime` as RFC 3339 UTC, using the Unix epoch when the instant is invalid.
fn system_time_rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339()
}

/// Object metadata for a cover or chapter sidecar, without creation or modified timestamps.
async fn sidecar_meta(asin: &str, title: &str, content_type: &str, path: &Path) -> ObjectMeta {
    let content_length = tokio::fs::metadata(path).await.ok().map(|m| m.len());
    ObjectMeta {
        content_type: Some(content_type.into()),
        content_length,
        asin: Some(asin.to_string()),
        title: Some(title.to_string()),
        creation_time: None,
        last_write_time: None,
        ..Default::default()
    }
}

/// MIME type for a well-known audio extension; unknown extensions become `application/octet-stream`.
fn content_type_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "m4a" | "m4b" => "audio/mp4",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "aaxc" | "aax" | "cenc" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

/// Default audio extension when the source path has none (`mp3`, `opus`, or `m4b`).
fn fallback_audio_ext(options: &DownloadOptions) -> &'static str {
    if options.wants_mp3() {
        "mp3"
    } else if options.wants_opus() {
        "opus"
    } else {
        "m4b"
    }
}

#[derive(Debug, Clone, Default)]
/// Optional provider catalog overlay applied to plain-audio metadata and tags.
struct PlainAudibleCatalog {
    /// Catalog title used for tags when the store title is empty or less specific.
    title: Option<String>,
    /// Comma-joined author names from the provider record.
    authors: Option<String>,
    /// Comma-joined narrator names from the provider record.
    narrators: Option<String>,
    /// Primary series name from the provider record.
    series: Option<String>,
    /// Series position string from the provider record (may be non-integer).
    series_index: Option<String>,
    /// Subtitle from the provider record, when present.
    subtitle: Option<String>,
    /// Publisher display name from the provider record.
    publisher: Option<String>,
    /// ISBN when the provider record carries one.
    isbn: Option<String>,
    /// Semicolon-joined genre/tag names from the provider record.
    categories: Option<String>,
    /// Four-digit publication year derived from the provider's release date.
    year: Option<String>,
    /// Long description or summary from the provider record.
    description: Option<String>,
    /// Content language from the provider record (for example `english`).
    language: Option<String>,
    /// Downloaded cover image used for tagging when the source omitted one.
    cover_path: Option<PathBuf>,
}

/// True when the library row has an enrichment Audible ASIN distinct from the store product id.
async fn plain_source_has_audible_asin(library: &LibraryStore, req: &AcquireRequest) -> bool {
    resolve_title(library, req)
        .await
        .and_then(|t| {
            let asin = t.asin.as_deref()?;
            // Audible-native titles have asin == product_id; enrichment ASINs differ.
            if asin == t.product_id.as_str() {
                None
            } else {
                Some(())
            }
        })
        .is_some()
}

/// Fetch provider catalog extras for plain acquire (chapters fetched separately).
async fn fetch_plain_catalog_overlay(
    library: &LibraryStore,
    req: &AcquireRequest,
    work_dir: &Path,
) -> PlainAudibleCatalog {
    let Some(title) = resolve_title(library, req).await else {
        return PlainAudibleCatalog::default();
    };
    let Some(audible_asin) = title.asin.clone() else {
        return PlainAudibleCatalog::default();
    };
    let region = title_region(&title);
    let Some(record) =
        bookclerk_enrich::enrichment_for_asin(req.metadata.providers(), &audible_asin, region)
            .await
    else {
        tracing::debug!(
            audible_asin = %audible_asin,
            "no metadata provider record for plain acquire overlay"
        );
        return PlainAudibleCatalog::default();
    };
    let cover_path = match (record.cover_url.as_deref(), req.metadata.covers.as_deref()) {
        (Some(url), Some(covers)) => save_overlay_cover(covers, url, &audible_asin, work_dir).await,
        _ => None,
    };
    PlainAudibleCatalog {
        title: Some(record.title),
        authors: record.authors,
        narrators: record.narrators,
        series: record.series,
        series_index: record.series_index,
        subtitle: record.subtitle,
        publisher: record.publisher,
        isbn: record.isbn,
        categories: record.categories,
        year: record.published_at.as_deref().and_then(release_year),
        description: record.description,
        language: record.language,
        cover_path,
    }
}

/// Marketplace of a title, `us` when blank.
fn title_region(title: &bookclerk_library::AcquireTitle) -> &str {
    if title.marketplace.trim().is_empty() {
        "us"
    } else {
        title.marketplace.as_str()
    }
}

/// Four-digit year at the start of a provider's release date.
fn release_year(date: &str) -> Option<String> {
    let year = date.trim().get(..4)?;
    year.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| year.to_string())
}

/// Saves the provider's cover, as the host's cover service re-encoded it,
/// into the work directory.
///
/// The URL is plugin output, so the host never requests it: `covers` has a
/// `covers` plugin download it and the confined media worker turn it into a
/// JPEG.
///
/// # Arguments
///
/// * `covers` - The host's cover service.
/// * `url` - The provider's cover URL.
/// * `audible_asin` - ASIN naming the saved file.
/// * `work_dir` - The title's scratch directory.
///
/// # Returns
///
/// The saved path, or `None` when there is no image or it cannot be written.
async fn save_overlay_cover(
    covers: &dyn CoverImages,
    url: &str,
    audible_asin: &str,
    work_dir: &Path,
) -> Option<PathBuf> {
    let Some(jpeg) = covers.full_cover(url).await else {
        tracing::debug!(%url, "provider cover unavailable; overlay keeps the store cover");
        return None;
    };
    let path = work_dir.join(format!("{audible_asin}.provider-cover.jpg"));
    match tokio::fs::write(&path, &jpeg).await {
        Ok(()) => Some(path),
        Err(err) => {
            tracing::debug!(error = %err, "provider cover not saved");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// Builds a media fix-up request, preferring provider overlay fields over the library row.
async fn build_fixup_request(
    library: &LibraryStore,
    req: &AcquireRequest,
    input: PathBuf,
    output: PathBuf,
    cover: Option<PathBuf>,
    chapters: Vec<(String, u64)>,
    title_override: Option<String>,
    replace_chapters: bool,
    catalog: &PlainAudibleCatalog,
) -> FixupRequest {
    let book = resolve_title(library, req).await;
    let year = catalog.year.clone().or_else(|| {
        book.as_ref()
            .and_then(|b| b.published_at)
            .map(|dt| dt.format("%Y").to_string())
    });
    let asin = book.as_ref().and_then(|b| b.asin.clone());
    let isbn = catalog.isbn.clone().or_else(|| {
        book.as_ref().and_then(|b| {
            b.isbn.clone().or_else(|| {
                // product_id is the store-native id; use it as ISBN when it is not the ASIN.
                if b.asin.as_deref() != Some(b.product_id.as_str()) {
                    Some(b.product_id.clone())
                } else {
                    None
                }
            })
        })
    });
    let title = title_override
        .unwrap_or_else(|| catalog.title.clone().unwrap_or_else(|| req.title.clone()));
    FixupRequest {
        input,
        output,
        title,
        author: catalog.authors.clone().or_else(|| req.authors.clone()),
        narrator: catalog.narrators.clone().or_else(|| req.narrators.clone()),
        cover,
        chapters,
        replace_chapters,
        subtitle: catalog
            .subtitle
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.subtitle.clone())),
        publisher: catalog
            .publisher
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.publisher.clone())),
        year,
        genre: catalog
            .categories
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.categories.clone())),
        series: catalog
            .series
            .clone()
            .or_else(|| req.series.clone())
            .or_else(|| book.as_ref().and_then(|b| b.series.clone())),
        series_index: catalog
            .series_index
            .clone()
            .or_else(|| req.series_index.clone())
            .or_else(|| book.as_ref().and_then(|b| b.series_index.clone())),
        asin,
        isbn,
        description: catalog.description.clone(),
        language: catalog.language.clone(),
        tool: Some(bookclerk_tool_tag()),
    }
}

/// Applies configured creation/modified times to written keys; timestamp failures are logged only.
async fn apply_storage_timestamps(
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    req: &AcquireRequest,
    keys: &[String],
) {
    let title = resolve_title(library, req).await;
    let created = resolve_timestamp(req.options.creation_time, title.as_ref());
    let modified = resolve_timestamp(req.options.last_write_time, title.as_ref());
    if created.is_none() && modified.is_none() {
        return;
    }
    for key in keys {
        if let Err(err) = storage.touch_file(key, created, modified).await {
            tracing::warn!(asin = %req.asin, key = %key, error = %err, "file timestamp update failed");
        }
    }
}

/// Resolves a file-timestamp mode to a `SystemTime` (`now`, purchase time, or publication time).
fn resolve_timestamp(
    mode: FileTimestampMode,
    book: Option<&bookclerk_library::AcquireTitle>,
) -> Option<SystemTime> {
    match mode {
        FileTimestampMode::Now => Some(SystemTime::now()),
        FileTimestampMode::Purchased => book.and_then(|b| b.purchased_at).map(|dt| {
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp().max(0) as u64)
        }),
        FileTimestampMode::Published => book.and_then(|b| b.published_at).map(|dt| {
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp().max(0) as u64)
        }),
    }
}

/// The library model's view of the request's title
/// ([`LibraryStore::acquire_title`]), found through its library row
/// ([`resolve_book`]) while requests still name one; `None` when the row is
/// missing or its listing is not resolved yet.
async fn resolve_title(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Option<bookclerk_library::AcquireTitle> {
    if let Some(holding) = req.holding_id.as_deref().filter(|id| !id.is_empty()) {
        return match library.acquire_title_of_holding(holding).await {
            Ok(title) => title,
            Err(err) => {
                tracing::warn!(asin = %req.asin, error = %err, "could not read the title from the library model");
                None
            }
        };
    }
    let book = resolve_book(library, req).await?;
    match library
        .acquire_title(
            &book.plugin_key,
            &book.account_id,
            &book.marketplace,
            &book.product_id,
        )
        .await
    {
        Ok(title) => title,
        Err(err) => {
            tracing::warn!(asin = %req.asin, error = %err, "could not read the title from the library model");
            None
        }
    }
}

/// Loads the library row by UUID first, then by product id and account.
async fn resolve_book(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Option<bookclerk_library::BookRecord> {
    if let Some(holding) = req.holding_id.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(Some(title)) = library.acquire_title_of_holding(holding).await {
            if let Some(uuid) = title.book_uuid.as_deref() {
                if let Ok(Some(b)) = library.get_book_by_uuid(uuid).await {
                    return Some(b);
                }
            }
        }
    }
    library
        .get_book(&req.asin, &req.account_id)
        .await
        .ok()
        .flatten()
}

/// When liberating plain audio that was enriched with an Audible ASIN, ask the
/// metadata providers for Audible's chapter layout (the Audnexus plugin) and
/// rebase starts for missing brand intro/outro.
///
/// `plain_audio_duration_ms` is the probed duration of the plain file (no brand
/// segments). When the layout omits runtime, it reconstructs the Audible
/// timeline so outro chapters can still be trimmed.
async fn overlay_audible_chapters_for_plain(
    library: &LibraryStore,
    req: &AcquireRequest,
    plain_audio_duration_ms: Option<u64>,
) -> Option<(Vec<(String, u64)>, Value)> {
    let title = resolve_title(library, req).await?;
    let audible_asin = title.asin.clone()?;
    let region = title_region(&title).to_string();
    let answer = req
        .metadata
        .providers()
        .chapters(&MetadataChaptersParams {
            identifiers: vec![asin_identifier(&audible_asin, &region)],
            marketplace: Some(region),
        })
        .await;
    for failure in &answer.failures {
        tracing::warn!(
            audible_asin = %audible_asin,
            provider = %failure.provider,
            error = %failure.message,
            "chapter layout lookup failed for plain-audio overlay"
        );
    }
    let Some(found) = answer.found else {
        tracing::debug!(
            audible_asin = %audible_asin,
            "no provider chapter layout for plain-audio overlay"
        );
        return None;
    };
    let info = chapter_info_from_layout(&found.chapters);
    let chapters = chapters_from_catalog_info_for_plain_audio(
        &info,
        req.options.combine_nested_chapter_titles,
        req.options.merge_opening_and_end_credits,
        req.options.strip_unabridged,
        req.options.strip_audible_brand_audio,
        plain_audio_duration_ms,
    );
    if chapters.is_empty() {
        None
    } else {
        let tree = rebase_chapter_tree_for_plain_audio(&info, plain_audio_duration_ms);
        Some((chapters, tree))
    }
}

/// Probes MP4-family duration in milliseconds; returns `None` for other containers or parse failure.
fn probe_audio_duration_ms(path: &Path) -> Option<u64> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "m4b" | "m4a" | "mp4" | "aaxc" | "aax") {
        if let Ok(mp4) = parse_mp4(path) {
            let ms = track_duration_ms(&mp4.audio);
            if ms > 0 {
                return Some(ms);
            }
        }
    }
    None
}

/// Builds a naming context from the request, filling gaps from the library row.
async fn base_naming_ctx(library: &LibraryStore, req: &AcquireRequest) -> NamingContext {
    let book = resolve_title(library, req).await;
    let call_number = match (&req.call_number, &book) {
        (Some(id), _) => Some(id.clone()),
        (None, Some(title)) => bookclerk_library::call_number(&title.edition_id),
        (None, None) => None,
    };
    NamingContext {
        call_number,
        asin: book
            .as_ref()
            .map(|b| b.asin_or_isbn().to_string())
            .unwrap_or_else(|| req.asin.clone()),
        title: req.title.clone(),
        subtitle: book.as_ref().and_then(|b| b.subtitle.clone()),
        authors: req.authors.clone(),
        narrators: req.narrators.clone(),
        series: req
            .series
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.series.clone())),
        series_index: req
            .series_index
            .clone()
            .or_else(|| book.as_ref().and_then(|b| b.series_index.clone())),
        series_asin: book.as_ref().and_then(|b| b.series_product_id.clone()),
        year_published: book
            .as_ref()
            .and_then(|b| b.published_at)
            .map(|dt| dt.year()),
        account_id: Some(req.account_id.clone()),
        locale: book.as_ref().map(|b| b.marketplace.clone()),
        publisher: book.as_ref().and_then(|b| b.publisher.clone()),
        categories: book.as_ref().and_then(|b| b.categories.clone()),
        length_minutes: book.as_ref().and_then(|b| b.length_minutes),
        is_abridged: book.as_ref().is_some_and(|b| b.is_abridged),
        content_kind: book.as_ref().map(|b| b.content_kind.to_string()),
        ..Default::default()
    }
}

/// Folder naming context: when saving podcasts to the parent folder, evaluate
/// the folder template against the podcast parent (classic Libation behavior).
async fn base_folder_naming_ctx(
    library: &LibraryStore,
    req: &AcquireRequest,
    episode: NamingContext,
) -> NamingContext {
    if !req.options.save_podcasts_to_parent_folder {
        return episode;
    }
    let kind = episode.content_kind.as_deref().unwrap_or("");
    if !bookclerk_library::is_episode(kind) {
        return episode;
    }
    let Some(parent_asin) = episode.series_asin.as_deref() else {
        return episode;
    };
    let parent = match library
        .acquire_titles_named(Some(&req.account_id), parent_asin)
        .await
    {
        Ok(titles) => titles.into_iter().next(),
        Err(err) => {
            tracing::warn!(asin = %req.asin, error = %err, "could not read the podcast parent");
            None
        }
    };
    let Some(parent) = parent else {
        return episode;
    };
    NamingContext {
        asin: parent.asin_or_isbn().to_string(),
        title: parent.title.clone(),
        subtitle: parent.subtitle.clone(),
        authors: parent.authors.clone(),
        narrators: parent.narrators.clone(),
        series: parent.series.clone(),
        series_index: None,
        series_asin: parent.series_product_id.clone(),
        year_published: parent.published_at.map(|dt| dt.year()),
        account_id: Some(parent.account_id.clone()),
        locale: Some(parent.marketplace.clone()),
        publisher: parent.publisher.clone(),
        categories: parent.categories.clone(),
        length_minutes: parent.length_minutes,
        is_abridged: parent.is_abridged,
        content_kind: Some(parent.content_kind.to_string()),
        call_number: match &req.parent_call_number {
            Some(id) => Some(id.clone()),
            None => bookclerk_library::call_number(&parent.edition_id),
        },
        ..Default::default()
    }
}

/// The short Bookclerk id of the edition `book_uuid` resolves to, for
/// `<call number>` ([`bookclerk_library::call_number`]).
///
/// # Returns
///
/// `None` when the title has no edition.
///
/// # Errors
///
/// Returns an error when the edition cannot be read.
pub async fn call_number_of(
    library: &LibraryStore,
    book_uuid: &str,
) -> bookclerk_library::Result<Option<String>> {
    Ok(library
        .edition_of_book(book_uuid)
        .await?
        .and_then(|edition| bookclerk_library::call_number(&edition)))
}

/// The folder and file naming contexts of `req`. Both carry the edition's
/// `<call number>`, which the templates must write
/// ([`bookclerk_naming::require_call_number`]), so two editions of one work
/// never share a path.
async fn naming_contexts(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> (NamingContext, NamingContext) {
    let file = base_naming_ctx(library, req).await;
    let folder = base_folder_naming_ctx(library, req, file.clone()).await;
    (folder, file)
}

/// Planned audio storage key for `ext`, honoring folder/file templates and
/// `save_podcasts_to_parent_folder`: the host key the templates build,
/// translated by the request's destination
/// ([`AcquireRequest::destination_keys`]).
pub async fn planned_storage_key_for(
    library: &LibraryStore,
    req: &AcquireRequest,
    ext: &str,
) -> String {
    let host_key =
        planned_storage_key_with_rules(library, req, ext, &req.options.replacement_characters)
            .await;
    req.destination_keys.translate(host_key).await
}

/// The host key [`planned_storage_key_for`] translates, built with an
/// explicit replacement-rule set and not translated (used by reconcile to
/// probe wildcard patterns across sanitization profiles).
pub async fn planned_storage_key_with_rules(
    library: &LibraryStore,
    req: &AcquireRequest,
    ext: &str,
    replacement_rules: &[bookclerk_config::ReplacementRule],
) -> String {
    let templates = req.options.naming_templates();
    let (folder_ctx, file_ctx) = naming_contexts(library, req).await;
    storage_key_with_contexts(
        &folder_ctx,
        &file_ctx,
        Some(templates.folder.as_str()),
        Some(templates.file.as_str()),
        ext,
        replacement_rules,
        req.options.path_limits,
    )
}

/// Compute the storage key that would be used (for dry-run / set-status).
///
/// Uses the library row (when present) so podcast episodes honor
/// `save_podcasts_to_parent_folder` the same way as a real acquire.
pub async fn planned_storage_key(library: &LibraryStore, req: &AcquireRequest) -> String {
    planned_storage_key_for(library, req, fallback_audio_ext(&req.options)).await
}

/// The folders an acquire of `req` writes the title's files under on a
/// destination with `req.options`: the folder of its planned audio key,
/// which its sidecars, chapter files and parts share. When podcast
/// episodes are saved to the parent's folder, the episode's own folder is
/// included too, since a library without the parent's row names it so. An
/// empty string is the destination's root (a template with no folder).
pub async fn planned_title_folders(library: &LibraryStore, req: &AcquireRequest) -> Vec<String> {
    /// The folder part of a storage key.
    fn folder_of(key: &str) -> String {
        key.rsplit_once('/')
            .map_or_else(String::new, |(folder, _)| folder.to_string())
    }
    let ext = fallback_audio_ext(&req.options);
    let mut folders = vec![folder_of(&planned_storage_key_for(library, req, ext).await)];
    if req.options.save_podcasts_to_parent_folder {
        let mut own = req.clone();
        own.options.save_podcasts_to_parent_folder = false;
        let folder = folder_of(&planned_storage_key_for(library, &own, ext).await);
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders
}

/// In a [`PlannedTitleNames`] pattern: one or more characters of a file
/// name (no `/`), where the source's part or chapter title goes; the
/// marker the owner's upload scope matches
/// ([`bookclerk_library::control_plane::TitleNames`]).
pub const NAME_WILDCARD: char = bookclerk_library::control_plane::UPLOAD_NAME_WILDCARD;

/// In a [`PlannedTitleNames`] pattern: one or more ASCII digits, where a
/// part or chapter number goes (the owner's upload scope matches it too).
pub const NUMBER_WILDCARD: char = bookclerk_library::control_plane::UPLOAD_NUMBER_WILDCARD;

/// Stands in for a part or chapter title when a pattern is rendered: plain
/// letters, which every replacement rule keeps.
const TITLE_SENTINEL: &str = "Qzxvpartqzxv";

/// Stands in for a part or chapter number when a pattern is rendered: too
/// long for any zero padding to change it.
const NUMBER_SENTINEL: u32 = 987_654_321;

/// The names an acquire of one title writes on one destination, as the
/// owner plans them, to bound a host's upload of that title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTitleNames {
    /// The folders the title's files go under ([`planned_title_folders`]).
    pub folders: Vec<String>,
    /// True when the folder template has a component of the title's own
    /// (its id, title or subtitle), so no other title's files go there and
    /// any name under it is the title's. False for a folder several titles
    /// share (`<author>`), the destination's root, or a podcast parent's
    /// folder: there only the names below are the title's.
    pub own_folder: bool,
    /// The planned audio key of a single-file acquire, without its
    /// extension (one per folder variant): the title's audio is this stem
    /// with any audio extension, and its sidecars this stem, a `.` and a
    /// suffix.
    pub audio_stems: Vec<String>,
    /// Part and chapter keys without their extension, with
    /// [`NAME_WILDCARD`] and [`NUMBER_WILDCARD`] where the source's part or
    /// chapter title and number go. Those titles are not known before the
    /// acquire runs, so they are matched rather than listed; a template
    /// with no part or chapter field yields a literal name.
    pub part_patterns: Vec<String>,
}

/// The title's planned names on the destination `req.options` describe:
/// [`PlannedTitleNames`].
pub async fn planned_title_names(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> PlannedTitleNames {
    /// The folder part of a storage key.
    fn folder_of(key: &str) -> &str {
        key.rsplit_once('/').map_or("", |(folder, _)| folder)
    }
    /// `key` without its last extension.
    fn stem_of(key: &str) -> String {
        let name_at = key.rfind('/').map_or(0, |at| at + 1);
        match key[name_at..].rfind('.') {
            Some(dot) if dot > 0 => key[..name_at + dot].to_string(),
            _ => key.to_string(),
        }
    }
    /// `text` with every ASCII-case-insensitive match of `needle` (ASCII)
    /// replaced by `with`: a template's case flags (`[u]`, `[l]`) change
    /// a sentinel's case, never its length.
    fn replace_ignoring_case(text: &str, needle: &str, with: &str) -> String {
        let folded = text.to_ascii_lowercase();
        let needle = needle.to_ascii_lowercase();
        let mut out = String::with_capacity(text.len());
        let mut from = 0;
        while let Some(at) = folded[from..].find(&needle) {
            out.push_str(&text[from..from + at]);
            out.push_str(with);
            from += at + needle.len();
        }
        out.push_str(&text[from..]);
        out
    }
    /// A rendered key's stem with its sentinels turned into wildcards. The
    /// number may come out grouped (`#,##0`), so each grouped form is
    /// matched too; a form the template changes beyond that stays literal
    /// and the upload is refused rather than widened.
    fn pattern_of(key: &str) -> String {
        let mut pattern =
            replace_ignoring_case(&stem_of(key), TITLE_SENTINEL, &NAME_WILDCARD.to_string());
        let number = NUMBER_SENTINEL.to_string();
        let wildcard = NUMBER_WILDCARD.to_string();
        for grouped in [
            number.clone(),
            "987,654,321".to_string(),
            "987.654.321".to_string(),
            "987 654 321".to_string(),
        ] {
            pattern = pattern.replace(&grouped, &wildcard);
        }
        pattern
    }
    let ext = fallback_audio_ext(&req.options);
    let mut variants = vec![req.clone()];
    if req.options.save_podcasts_to_parent_folder {
        let mut own = req.clone();
        own.options.save_podcasts_to_parent_folder = false;
        variants.push(own);
    }
    let templates = req.options.naming_templates();
    let rules = &req.options.replacement_characters;
    let mut names = PlannedTitleNames {
        folders: planned_title_folders(library, req).await,
        own_folder: false,
        audio_stems: Vec::new(),
        part_patterns: Vec::new(),
    };
    let mut own_folder = true;
    for variant in &variants {
        let (folder_ctx, file_ctx) = naming_contexts(library, variant).await;
        let key = storage_key_with_contexts(
            &folder_ctx,
            &file_ctx,
            Some(templates.folder.as_str()),
            Some(templates.file.as_str()),
            ext,
            rules,
            variant.options.path_limits,
        );
        // The folder is the title's own only when every other title probed
        // (other product and call number, title and subtitle) would be filed
        // elsewhere: one
        // wholly different, and one that only extends this title's fields
        // (`Dune: Messiah: …`), which a template using part of a field
        // (`<title short>`, a leading slice) files in the same folder.
        let probes: [fn(&str) -> String; 2] = [
            |_| TITLE_SENTINEL.to_string(),
            |field| format!("{field}: {TITLE_SENTINEL}"),
        ];
        let filed_apart = probes.iter().all(|probe| {
            let mut other = folder_ctx.clone();
            other.asin = probe(&folder_ctx.asin);
            other.call_number = folder_ctx.call_number.as_deref().map(probe);
            other.title = probe(&folder_ctx.title);
            other.subtitle = Some(probe(folder_ctx.subtitle.as_deref().unwrap_or_default()));
            let other_key = storage_key_with_contexts(
                &other,
                &file_ctx,
                Some(templates.folder.as_str()),
                Some(templates.file.as_str()),
                ext,
                rules,
                variant.options.path_limits,
            );
            folder_of(&key) != folder_of(&other_key)
        });
        // A podcast parent's folder holds all its episodes: never one
        // title's own, however its template reads.
        let parents_folder = folder_ctx.asin != file_ctx.asin;
        own_folder &= !parents_folder && !folder_of(&key).is_empty() && filed_apart;
        let stem = stem_of(&variant.destination_keys.translate(key.clone()).await);
        if !names.audio_stems.contains(&stem) {
            names.audio_stems.push(stem);
        }
        // Plain parts: the file template with the part's title and a
        // `-pNNN` product and call number (as `planned_plain_part_storage_key`
        // names them).
        let mut part_ctx = file_ctx.clone();
        part_ctx.title = format!("{} — {TITLE_SENTINEL}", variant.title);
        part_ctx.asin = format!("{}-p{NUMBER_SENTINEL}", variant.asin);
        part_ctx.call_number = part_ctx
            .call_number
            .map(|id| format!("{id}-p{NUMBER_SENTINEL}"));
        part_ctx.chapter_number = Some(NUMBER_SENTINEL);
        part_ctx.chapter_title = Some(TITLE_SENTINEL.into());
        let part = storage_key_with_contexts(
            &folder_ctx,
            &part_ctx,
            Some(templates.folder.as_str()),
            Some(templates.file.as_str()),
            ext,
            rules,
            variant.options.path_limits,
        );
        // Split chapters: the chapter-file template.
        let chapter = chapter_storage_key_with_folder(
            &folder_ctx,
            &file_ctx,
            Some(templates.folder.as_str()),
            Some(templates.chapter_file.as_str()),
            rules,
            NUMBER_SENTINEL as usize,
            TITLE_SENTINEL,
            ext,
            variant.options.path_limits,
        );
        let part = variant.destination_keys.translate(part).await;
        let chapter = variant.destination_keys.translate(chapter).await;
        for pattern in [pattern_of(&part), pattern_of(&chapter)] {
            if !names.part_patterns.contains(&pattern) {
                names.part_patterns.push(pattern);
            }
        }
    }
    names.own_folder = own_folder;
    names
}

/// Download and store companion PDF only (classic `acquire --pdf`).
///
/// Fetches via [`ContentSource::fetch_title`]; requires `PlainFetch.pdf_url`, then HTTP GETs that URL.
/// Each PDF key written is recorded for the book's edition in `edition_files`.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn acquire_pdf_only(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    let placed = placed_destinations(destinations, req)?;
    let destinations = placed.as_ref().unwrap_or(destinations);
    let owner = file_owner_of_request(library, req).await?;
    let mut req = req.clone();
    pin_call_number(&mut req, owner.as_ref());
    let recorded = recording_destinations(
        library,
        destinations,
        owner.as_ref(),
        &req.source,
        req.library_copies.stored_from(),
    );
    acquire_pdf_only_recorded(library, &recorded, &req, source).await
}

/// Refuses an acquire whose files could land on another edition's: every
/// destination's templates must write `<call number>`
/// ([`bookclerk_naming::require_call_number`]), and the planned audio name
/// must not already hold another edition's file (two edition ids that share
/// their short id). Nothing is fetched or overwritten when either fails.
///
/// # Errors
///
/// Returns [`AcquireError::Other`] naming the destination whose templates
/// lack the tag, or the planned key another edition holds.
async fn check_edition_paths(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
) -> Result<()> {
    let owner = owner_of_request(library, req)
        .await?
        .and_then(|(owner, _)| owner);
    for item in &destinations.items {
        let dest_req = request_for_destination(req, item);
        let templates = dest_req.options.naming_templates();
        bookclerk_naming::require_call_number(
            &templates.folder,
            &templates.file,
            &templates.chapter_file,
        )
        .map_err(|err| {
            AcquireError::Other(anyhow::anyhow!(
                "{err} (the {} destination); fix it in Settings → Output",
                item.id
            ))
        })?;
        let Some(owner) = &owner else {
            continue;
        };
        let (folder_ctx, file_ctx) = naming_contexts(library, &dest_req).await;
        let key = dest_req
            .destination_keys
            .translate(planned_audio_key(&dest_req, &folder_ctx, &file_ctx))
            .await;
        if name_held_elsewhere(library, &key, owner).await? {
            return Err(AcquireError::Other(anyhow::anyhow!(
                "another edition's files already use {key}; nothing was written"
            )));
        }
    }
    Ok(())
}

/// The audio key `req` plans with these naming contexts, on its fallback
/// audio extension.
fn planned_audio_key(req: &AcquireRequest, folder: &NamingContext, file: &NamingContext) -> String {
    let templates = req.options.naming_templates();
    storage_key_with_contexts(
        folder,
        file,
        Some(templates.folder.as_str()),
        Some(templates.file.as_str()),
        fallback_audio_ext(&req.options),
        &req.options.replacement_characters,
        req.options.path_limits,
    )
}

/// True when another owner's file already lives under the name of the
/// planned `key` ([`LibraryStore::name_held_by_another_owner`]). The
/// written key can differ from the planned one in its extension and in a
/// part suffix, so the probe is the key without its extension.
///
/// # Errors
///
/// Returns an error when the read fails, so the acquire stops rather than
/// risk writing over another edition's files.
async fn name_held_elsewhere(library: &LibraryStore, key: &str, owner: &FileOwner) -> Result<bool> {
    let stem = key
        .rsplit_once('.')
        .filter(|(stem, ext)| !ext.contains('/') && !stem.ends_with('/'))
        .map_or(key, |(stem, _)| stem);
    Ok(library.name_held_by_another_owner(stem, owner).await?)
}

/// Marks `book` acquired at the files of its edition, when another book
/// of the edition acquired it (`edition_acquisitions`) and every
/// destination holds the copy, so the edition is not fetched again. The edition's companion PDF is shared too
/// when that book has one.
///
/// A destination holds the copy when the sibling's primary key there is
/// the edition's own (unrecorded, as storage matching adopts it, or
/// recorded for this edition, never another's: a listing that moved
/// editions leaves its old files with the old one) and every audio key
/// recorded for the edition there (parts, chapters) is present.
///
/// # Arguments
///
/// * `destinations` - Each destination's id and
///   backend.
/// * `book` - The book to mark acquired.
///
/// # Returns
///
/// The shared primary key; `None` when the book has no edition yet, or no
/// other book of its edition is acquired with a copy every destination
/// holds (a destination deployed since, another node's layout, or a file
/// removed since). The acquire then plans as usual and syncs or fetches
/// what is missing.
///
/// # Errors
///
/// Returns an error when a library lookup or write, or a backend probe,
/// fails.
pub async fn share_acquired_sibling(
    library: &LibraryStore,
    destinations: &[(&str, &dyn StorageBackend)],
    title: &bookclerk_library::AcquireTitle,
) -> Result<Option<String>> {
    let edition = title.edition_id.clone();
    // The edition records which holding acquired it (`edition_acquisitions`).
    let Some(acquisition) = library.edition_acquisition(&edition).await? else {
        return Ok(None);
    };
    if acquisition.status != AcquireStatus::Acquired || acquisition.holding_id == title.holding_id {
        return Ok(None);
    }
    let Some(key) = acquisition
        .storage_key
        .as_deref()
        .filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };
    let owner = FileOwner::Edition(edition);
    // The primary key names the primary destination's file; another
    // destination may name the same audio differently, so it is judged by
    // the keys the edition recorded there.
    for (index, (destination, backend)) in destinations.iter().enumerate() {
        let recorded = library.audio_keys_of(destination, &owner).await?;
        if index == 0 {
            let holder = library.edition_file(destination, key).await?;
            if holder.is_some_and(|file| file.owner != owner) {
                return Ok(None);
            }
            if !backend.exists(key).await? {
                return Ok(None);
            }
        } else if recorded.is_empty() && !backend.exists(key).await? {
            return Ok(None);
        }
        for audio in recorded {
            if !backend.exists(&audio).await? {
                return Ok(None);
            }
        }
    }
    library
        .set_holding_acquire_status(&title.holding_id, AcquireStatus::Acquired, Some(key), None)
        .await?;
    // The edition's companion PDF is shared too when it is on the primary
    // destination.
    let pdf_key = acquisition
        .pdf_storage_key
        .as_deref()
        .filter(|key| acquisition.pdf_status == AcquireStatus::Acquired && !key.is_empty());
    if pdf_key.is_some() && sibling_pdf_is_present(destinations.first(), pdf_key).await? {
        library
            .set_holding_pdf_status(&title.holding_id, AcquireStatus::Acquired, pdf_key)
            .await?;
    }
    Ok(Some(key.to_string()))
}

/// True when the sibling's companion PDF `key` is present on the primary
/// destination, whose key the book row records, so the sharing title can
/// record it as acquired.
///
/// # Errors
///
/// Returns an error when the destination cannot be checked.
async fn sibling_pdf_is_present(
    primary: Option<&(&str, &dyn StorageBackend)>,
    key: Option<&str>,
) -> Result<bool> {
    let (Some((_, backend)), Some(key)) = (primary, key.filter(|key| !key.is_empty())) else {
        return Ok(false);
    };
    backend.exists(key).await.map_err(Into::into)
}

/// The owner the files of `req`'s book are recorded under: its edition.
///
/// # Returns
///
/// The book's [`FileOwner::Edition`]; `None` when the title has no library
/// row at all, so there is nothing to record the files for.
///
/// # Errors
///
/// Returns an error when the library lookup fails, and refuses a book whose
/// row has no edition yet (a listing the resolver refused or has not
/// resolved): its files would be written without an ownership record and
/// named without a call number, so it waits for a scan to resolve it.
async fn file_owner_of_request(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Result<Option<FileOwner>> {
    match owner_of_request(library, req).await? {
        None => Ok(None),
        Some((Some(owner), _)) => Ok(Some(owner)),
        Some((None, title)) => Err(AcquireError::Other(anyhow::anyhow!(
            "title {title} has no edition yet; rescan its account before acquiring it"
        ))),
    }
}

/// The edition that owns `req`'s files, found through its account holding
/// when known, otherwise through the title its product id names.
///
/// # Returns
///
/// `None` when the title is not in the library; otherwise the owner
/// (`None` when the title has no edition yet) and the id it was found by.
///
/// # Errors
///
/// Returns an error when the library lookup fails.
async fn owner_of_request(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Result<Option<(Option<FileOwner>, String)>> {
    if let Some(holding) = req.holding_id.as_deref().filter(|id| !id.is_empty()) {
        return Ok(library
            .file_owner_of_holding(holding)
            .await?
            .map(|owner| (Some(owner), holding.to_string())));
    }
    let Some(book) = resolve_book(library, req).await else {
        return Ok(None);
    };
    Ok(Some((
        library.file_owner_of_book(&book.uuid).await?,
        book.uuid,
    )))
}

/// [`acquire_pdf_only`] against destinations whose writes are already
/// recorded for the title ([`recording_destinations`]).
///
/// # Errors
///
/// Returns an error when the fetch, the download or a write fails.
async fn acquire_pdf_only_recorded(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    let primary_req = request_for_destination(req, destinations.primary_destination());

    if !primary_req.force {
        if let Some(key) = stored_edition_pdf(library, destinations, &primary_req).await? {
            return Ok(AcquireResult {
                asin: primary_req.asin.clone(),
                storage_key: key,
                written_keys: Vec::new(),
                matched_existing: true,
                from_library: false,
                store_fetches: 0,
                copies: Vec::new(),
            });
        }
    }

    // The companion PDF is named by the same templates as the audio, so it
    // gets the same refusals before anything is fetched or written.
    check_edition_paths(library, destinations, req).await?;

    let work_dir = primary_req
        .cache_dir
        .join("acquire-pdf")
        .join(&primary_req.asin);
    prepare_work_dir(library, &primary_req, &work_dir).await?;
    let result =
        download_and_store_companion_pdf(library, destinations, &primary_req, source, &work_dir)
            .await;
    cleanup_work_dir(library, primary_req.job_id.as_deref(), &work_dir).await;
    result
}

/// Fetch, cap, and store the companion PDF after scratch is reserved.
async fn download_and_store_companion_pdf(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    primary_req: &AcquireRequest,
    source: &dyn ContentSource,
    work_dir: &Path,
) -> Result<AcquireResult> {
    let mut download_opts = primary_req.options.clone();
    download_opts.download_pdf = true;
    let mut pdf_req = primary_req.clone();
    pdf_req.options = download_opts;
    let fetch = fetch_title_enforcing_quota(library, &pdf_req, source, work_dir).await?;

    let pdf_url = fetch.pdf_url;
    let Some(pdf_url) = pdf_url else {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "no companion PDF available for {}",
            primary_req.asin
        )));
    };

    let pdf_path = work_dir.join(format!("{}.pdf", primary_req.asin));
    let remaining = remaining_for_new_file(library, primary_req, work_dir).await?;
    if remaining == 0 {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded (0 bytes remaining)"
        )));
    }
    let client = download_http_client();
    let response = client
        .get(&pdf_url)
        .send()
        .await
        .map_err(|e| AcquireError::Other(anyhow::anyhow!("PDF download failed: {e}")))?
        .error_for_status()
        .map_err(|e| AcquireError::Other(anyhow::anyhow!("PDF download failed: {e}")))?;
    write_http_body_capped(response, &pdf_path, remaining).await?;
    enforce_work_dir_quota(library, primary_req, work_dir).await?;

    let asin_str = object_asin_for(library, primary_req).await;
    let mut primary_pdf_key = None;
    let mut written_keys = Vec::new();
    for dest in &destinations.items {
        let dest_req = request_for_destination(primary_req, dest);
        let audio_key = planned_storage_key(library, &dest_req).await;
        let pdf_key = sidecar_key(&audio_key, "pdf");
        let meta = sidecar_meta(&asin_str, &dest_req.title, "application/pdf", &pdf_path).await;
        dest.backend.put_file(&pdf_key, &pdf_path, meta).await?;
        if dest.id == destinations.primary {
            primary_pdf_key = Some(pdf_key.clone());
        }
        written_keys.push(pdf_key);
    }
    let pdf_key = primary_pdf_key
        .or_else(|| written_keys.first().cloned())
        .unwrap_or_default();
    record_pdf_status(
        library,
        primary_req,
        AcquireStatus::Acquired,
        Some(&pdf_key),
    )
    .await?;

    Ok(AcquireResult {
        asin: primary_req.asin.clone(),
        storage_key: pdf_key,
        written_keys,
        matched_existing: false,
        from_library: false,
        store_fetches: 1,
        copies: Vec::new(),
    })
}

/// The HTTP client companion downloads (cover PDFs) share.
///
/// Built once per process and cloned, so downloads reuse one connection
/// pool instead of handshaking per file. It holds no cookies and no default
/// headers.
fn download_http_client() -> reqwest::Client {
    static SHARED: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    SHARED.get_or_init(reqwest::Client::new).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{BookRecord, EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload};

    /// The library model's acquire view of a stored title.
    async fn view_of(store: &LibraryStore, book: &BookRecord) -> bookclerk_library::AcquireTitle {
        store
            .acquire_title(
                &book.plugin_key,
                &book.account_id,
                &book.marketplace,
                &book.product_id,
            )
            .await
            .unwrap()
            .unwrap()
    }
    /// Answers every URL containing `ok` with a fixed JPEG.
    struct FixedCovers;

    #[async_trait::async_trait]
    impl CoverImages for FixedCovers {
        async fn full_cover(&self, url: &str) -> Option<Vec<u8>> {
            url.contains("ok").then(|| b"\xFF\xD8\xFFjpeg".to_vec())
        }
    }

    #[tokio::test]
    async fn overlay_covers_come_from_the_cover_service() {
        let tmp = tempfile::tempdir().unwrap();
        let path = save_overlay_cover(
            &FixedCovers,
            "https://img.example/ok.jpg",
            "B00TEST001",
            tmp.path(),
        )
        .await
        .expect("saved");
        assert_eq!(path, tmp.path().join("B00TEST001.provider-cover.jpg"));
        assert_eq!(std::fs::read(&path).unwrap(), b"\xFF\xD8\xFFjpeg");
        assert_eq!(
            save_overlay_cover(
                &FixedCovers,
                "https://img.example/no.jpg",
                "B00TEST002",
                tmp.path()
            )
            .await,
            None
        );
        // A missing work directory is a write failure, not a panic.
        assert_eq!(
            save_overlay_cover(
                &FixedCovers,
                "https://img.example/ok.jpg",
                "B00TEST003",
                &tmp.path().join("gone")
            )
            .await,
            None
        );
        let metadata = AcquireMetadata::default();
        assert!(metadata.covers.is_none());
        assert!(metadata.with_covers(Arc::new(FixedCovers)).covers.is_some());
    }

    #[test]
    fn release_years_need_four_leading_digits() {
        assert_eq!(release_year(" 2021-05-04").as_deref(), Some("2021"));
        assert_eq!(release_year("2021").as_deref(), Some("2021"));
        assert_eq!(release_year("21-05"), None);
        assert_eq!(release_year("May 2021"), None);
    }

    async fn test_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    async fn job_id(store: &LibraryStore) -> String {
        let created = store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Acquire,
                payload: JobPayload::default(),
                priority: 0,
                max_attempts: 3,
                max_pending: 8,
                run_after: None,
            })
            .await
            .unwrap();
        match created {
            EnqueueOutcome::Created { id } => id,
            other => panic!("expected created: {other:?}"),
        }
    }

    fn dummy_req(cache: &Path, job_id: Option<String>, quota: Option<u64>) -> AcquireRequest {
        AcquireRequest {
            asin: "B00TEST".into(),
            holding_id: None,
            source: "audible".into(),
            account_id: "user-1".into(),
            title: "Test".into(),
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            options: DownloadOptions::default(),
            files_dir: cache.to_path_buf(),
            cache_dir: cache.to_path_buf(),
            force: false,
            write_destinations: None,
            placement: None,
            job_id,
            temp_quota_bytes: quota,
            call_number: None,
            parent_call_number: None,
            metadata: AcquireMetadata::default(),
            library_copies: LibraryCopySource::default(),
            destination_keys: crate::keys::DestinationKeys::default(),
        }
    }

    #[tokio::test]
    async fn cleanup_unregisters_only_after_successful_delete() {
        let store = test_store().await;
        let id = job_id(&store).await;
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = tmp.path().join("work");
        tokio::fs::write(&work_dir, b"not-a-dir").await.unwrap();
        store
            .reserve_job_temp_path(&id, &work_dir.to_string_lossy(), 8, 1024)
            .await
            .unwrap();
        cleanup_work_dir(&store, Some(&id), &work_dir).await;
        let still = store.list_job_temp_paths(&id).await.unwrap();
        assert_eq!(still.len(), 1);

        tokio::fs::remove_file(&work_dir).await.unwrap();
        cleanup_work_dir(&store, Some(&id), &work_dir).await;
        assert!(store.list_job_temp_paths(&id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn oversized_artifact_fails_quota_expand() {
        let store = test_store().await;
        let id = job_id(&store).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        let work_dir = cache.join("acquire").join("B00TEST");
        let req = dummy_req(cache, Some(id.clone()), Some(16));
        prepare_work_dir(&store, &req, &work_dir).await.unwrap();
        tokio::fs::write(work_dir.join("big.bin"), vec![0u8; 64])
            .await
            .unwrap();
        assert!(enforce_work_dir_quota(&store, &req, &work_dir)
            .await
            .is_err());
    }

    struct QuotaBurstSource {
        chunk: usize,
        chunks: usize,
    }

    #[async_trait::async_trait]
    impl ContentSource for QuotaBurstSource {
        fn id(&self) -> &str {
            "quota-burst"
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            bookclerk_source::PortalAuthMode::Password
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            bookclerk_source::SourceBrand {
                id: "quota-burst",
                name: "Quota Burst",
                bg: "#000000",
                fg: "#ffffff",
                accent: "#000000",
                icon_url: "",
            }
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Ok(Vec::new())
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Ok(bookclerk_source::ScanSummary::default())
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            opts: &FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            for i in 0..self.chunks {
                tokio::fs::write(
                    opts.cache_dir.join(format!("chunk-{i}")),
                    vec![0u8; self.chunk],
                )
                .await?;
                opts.enforce_cache_budget()?;
                tokio::task::yield_now().await;
            }
            Ok(PlainFetch {
                parts: Vec::new(),
                m4b_path: None,
                cover_path: None,
                chapters: Vec::new(),
                pdf_url: None,
            })
        }
    }

    #[tokio::test]
    async fn fetch_stops_when_source_crosses_quota_mid_write() {
        let store = test_store().await;
        let id = job_id(&store).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        let work_dir = cache.join("acquire").join("B00TEST");
        let req = dummy_req(cache, Some(id), Some(20));
        prepare_work_dir(&store, &req, &work_dir).await.unwrap();
        let err = fetch_title_enforcing_quota(
            &store,
            &req,
            &QuotaBurstSource {
                chunk: 16,
                chunks: 4,
            },
            &work_dir,
        )
        .await
        .expect_err("quota must fail mid-fetch");
        assert!(err.to_string().contains("quota"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn packaging_stage_fails_when_work_dir_crosses_quota() {
        let store = test_store().await;
        let id = job_id(&store).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        let work_dir = cache.join("acquire").join("B00TEST");
        let req = dummy_req(cache, Some(id), Some(20));
        prepare_work_dir(&store, &req, &work_dir).await.unwrap();
        let err = run_stage_enforcing_quota(&store, &req, &work_dir, async {
            tokio::fs::write(work_dir.join("packaged.m4b"), vec![0u8; 64]).await?;
            Ok(())
        })
        .await
        .expect_err("quota must fail after an oversized packaging write");
        assert!(err.to_string().contains("quota"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn without_a_job_the_work_dir_counts_once_against_the_quota() {
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        let work_dir = cache.join("acquire").join("B00TEST");
        let req = dummy_req(cache, None, Some(100));
        prepare_work_dir(&store, &req, &work_dir).await.unwrap();
        tokio::fs::write(work_dir.join("fetched.mp3"), vec![0u8; 60])
            .await
            .unwrap();

        // 60 bytes of a 100-byte quota leave the whole directory 100 bytes,
        // and a new file 40.
        assert_eq!(
            remaining_temp_budget(&store, &req, &work_dir)
                .await
                .unwrap(),
            100
        );
        assert_eq!(
            remaining_for_new_file(&store, &req, &work_dir)
                .await
                .unwrap(),
            40
        );

        // A stage that leaves 90 bytes fits; one that leaves 110 does not.
        run_stage_enforcing_quota(&store, &req, &work_dir, async {
            tokio::fs::write(work_dir.join("packaged.m4b"), vec![0u8; 30]).await?;
            Ok(())
        })
        .await
        .expect("a work directory over half the quota still fits");
        let err = run_stage_enforcing_quota(&store, &req, &work_dir, async {
            tokio::fs::write(work_dir.join("more.m4b"), vec![0u8; 20]).await?;
            Ok(())
        })
        .await
        .expect_err("total scratch over the quota must fail");
        assert!(err.to_string().contains("quota"), "unexpected error: {err}");

        // Another acquire's scratch still counts against this one.
        let other = cache.join("acquire").join("B00OTHER");
        tokio::fs::create_dir_all(&other).await.unwrap();
        tokio::fs::write(other.join("x"), vec![0u8; 15])
            .await
            .unwrap();
        assert_eq!(
            remaining_temp_budget(&store, &req, &work_dir)
                .await
                .unwrap(),
            85
        );

        // A directory outside the counted scratch is not subtracted.
        assert!(!counted_in_scratch(cache, &cache.join("elsewhere")));
        assert!(counted_in_scratch(
            cache,
            &cache.join("acquire-pdf").join("x")
        ));
    }

    async fn serve_http_body(body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, header.as_bytes()).await;
            let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, &body).await;
        });
        format!("http://{addr}/file.pdf")
    }

    async fn serve_http_body_loop(
        body: Vec<u8>,
        hits: Arc<std::sync::atomic::AtomicUsize>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                hits.fetch_add(1, Ordering::SeqCst);
                let mut buf = vec![0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, header.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, &body).await;
            }
        });
        format!("http://{addr}/file.pdf")
    }

    #[tokio::test]
    async fn streamed_download_stops_at_remaining_byte_cap() {
        let url = serve_http_body(vec![0u8; 64]).await;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.pdf");
        let response = reqwest::Client::new().get(&url).send().await.unwrap();
        let err = write_http_body_capped(response, &path, 16)
            .await
            .expect_err("stream must stop at the remaining-byte cap");
        assert!(err.to_string().contains("quota"), "unexpected error: {err}");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn companion_pdf_sidecar_stops_at_remaining_byte_cap() {
        let store = test_store().await;
        let id = job_id(&store).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        let work_dir = cache.join("acquire").join("B00TEST");
        let dest_root = tmp.path().join("dest");
        let url = serve_http_body(vec![0u8; 64]).await;
        let mut req = dummy_req(cache, Some(id), Some(16));
        req.options.download_pdf = true;
        prepare_work_dir(&store, &req, &work_dir).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = counting_destinations(&dest_root, puts.clone());
        let written = store_companion_pdf_sidecars(
            &store,
            &destinations,
            &req,
            &work_dir,
            CompanionPdf::Store(Some(&url)),
        )
        .await
        .expect("companion PDF quota must soft-fail so audio acquire can succeed");
        assert!(written.is_empty());
        assert!(!work_dir.join("B00TEST.pdf").exists());
        assert_eq!(puts.load(Ordering::SeqCst), 0);
    }

    struct CountingSource {
        fetches: Arc<std::sync::atomic::AtomicUsize>,
        pdf_url: Option<String>,
    }

    #[async_trait::async_trait]
    impl ContentSource for CountingSource {
        fn id(&self) -> &str {
            "crash-src"
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            bookclerk_source::PortalAuthMode::Password
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            bookclerk_source::SourceBrand {
                id: "crash-src",
                name: "Crash",
                bg: "#000000",
                fg: "#ffffff",
                accent: "#000000",
                icon_url: "",
            }
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Ok(Vec::new())
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Ok(bookclerk_source::ScanSummary::default())
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            opts: &FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let path = opts.cache_dir.join("crash.m4b");
            tokio::fs::write(&path, b"fake-m4b").await?;
            Ok(PlainFetch {
                parts: vec![bookclerk_source::PlainAudioPart {
                    path,
                    title: Some("Crash".into()),
                    duration_ms: Some(1_000),
                }],
                m4b_path: None,
                cover_path: None,
                chapters: Vec::new(),
                pdf_url: self.pdf_url.clone(),
            })
        }
    }

    /// The copy a library offers of the request's title, downloaded and
    /// recorded as the pipeline does for a copy from the title's store.
    async fn fetch_from_library(
        store: &LibraryStore,
        req: &AcquireRequest,
        work_dir: &Path,
    ) -> Option<LibraryFetch> {
        let offer = offered_copy(store, req).await?;
        take_library_copy(store, req, work_dir, &offer, CopyCover::Take).await
    }

    /// A request for book `uuid`'s holding that asks `copies` before the
    /// store and reads what it downloads in this process.
    async fn copy_req(
        store: &LibraryStore,
        cache: &Path,
        job: String,
        product_id: &str,
        uuid: &str,
        copies: Arc<crate::library_copies::tests::OneCopy>,
    ) -> AcquireRequest {
        let mut req = edition_req(cache, job, product_id, "user-1");
        req.holding_id = Some(holding_of(store, uuid).await);
        req.library_copies = LibraryCopySource::new(copies)
            .read_by(Arc::new(bookclerk_media::MediaPool::in_process()));
        req
    }

    #[tokio::test]
    async fn a_title_is_taken_from_the_copy_its_library_holds_of_the_same_store() {
        use crate::library_copies::tests::{MemoryCopy, OneCopy};
        use bookclerk_library::FileRole;

        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dest = tmp.path().join("dest");
        let destinations = title_named_destinations(&dest, puts.clone());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let audio = bookclerk_tags::fixtures::tagged_m4b();

        // The library downloaded its copy from this title's store: the
        // store is not asked, and the library's bytes are what is stored.
        let uuid = edition_book(&store, "B00COPY", "user-1", "Twin", None).await;
        let copy = MemoryCopy::new(
            Some("audible"),
            vec![
                ("Twin.m4b", FileRole::Audio, audio.clone()),
                ("Twin.cue", FileRole::Sidecar, b"FILE".to_vec()),
            ],
        );
        let (copy_fetches, taken) = (copy.fetched.clone(), copy.taken.clone());
        let copies = OneCopy::of(copy);
        let req = copy_req(
            &store,
            &cache,
            job.clone(),
            "B00COPY",
            &uuid,
            copies.clone(),
        )
        .await;
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(result.from_library);
        assert_eq!(result.store_fetches, 0);
        assert!(!result.matched_existing);
        assert_eq!(fetches.load(Ordering::SeqCst), 0, "the store is not asked");
        assert_eq!(copy_fetches.load(Ordering::SeqCst), 1, "only the audio");
        assert_eq!(taken.load(Ordering::SeqCst), 1);
        assert_eq!(
            *copies.asked.lock().unwrap(),
            [(
                "audible".to_string(),
                "us".to_string(),
                "B00COPY".to_string()
            )]
        );
        assert_eq!(
            std::fs::read(dest.join(&result.storage_key)).unwrap(),
            audio
        );
        let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
        assert_eq!(book.acquire_status, AcquireStatus::Acquired);
        // The result says where the audio came from.
        assert_eq!(
            serde_json::to_value(&result).unwrap()["from_library"],
            serde_json::json!(true)
        );

        // A copy the library downloaded from another store, or from a
        // store it does not know, is left alone: this title's own store
        // is asked, and nothing is downloaded from the library. What a
        // library offers from another store is asked for again once the
        // store's download is here, since the first answer may have
        // lapsed by then.
        for (n, theirs) in [Some("libro"), None].into_iter().enumerate() {
            let product = format!("B00OTHER{n}");
            let uuid = edition_book(&store, &product, "user-1", "Twin", None).await;
            let copy = MemoryCopy::new(theirs, vec![("Twin.m4b", FileRole::Audio, audio.clone())]);
            let (copy_fetches, taken) = (copy.fetched.clone(), copy.taken.clone());
            let before = fetches.load(Ordering::SeqCst);
            let copies = OneCopy::of(copy);
            let req = copy_req(&store, &cache, job.clone(), &product, &uuid, copies.clone()).await;
            let result = acquire_book(&store, &destinations, req, &source)
                .await
                .unwrap();
            assert!(!result.from_library);
            assert_eq!(fetches.load(Ordering::SeqCst), before + 1);
            assert_eq!(
                copies.asked.lock().unwrap().len(),
                if theirs.is_some() { 2 } else { 1 }
            );
            assert_eq!(copy_fetches.load(Ordering::SeqCst), 0);
            assert_eq!(taken.load(Ordering::SeqCst), 0);
            assert!(
                serde_json::to_value(&result)
                    .unwrap()
                    .get("from_library")
                    .is_none(),
                "a store fetch serializes as it always did"
            );
        }

        // A copy that does not arrive falls back to the store, and what
        // arrived of it is gone before the store's files are packaged.
        let uuid = edition_book(&store, "B00CUT", "user-1", "Twin", None).await;
        let mut copy = MemoryCopy::new(
            Some("audible"),
            vec![("Twin.m4b", FileRole::Audio, audio.clone())],
        );
        copy.failing = Some(0);
        let first_taken = copy.taken.clone();
        let before = fetches.load(Ordering::SeqCst);
        let req = copy_req(
            &store,
            &cache,
            job.clone(),
            "B00CUT",
            &uuid,
            OneCopy::of(copy),
        )
        .await;
        let work_dir = cache.join("acquire").join(&uuid);
        prepare_work_dir(&store, &req, &work_dir).await.unwrap();
        assert!(fetch_from_library(&store, &req, &work_dir).await.is_none());
        assert!(!work_dir.join(LIBRARY_COPY_DIR).exists());
        assert_eq!(first_taken.load(Ordering::SeqCst), 0);
        let mut copy = MemoryCopy::new(
            Some("audible"),
            vec![("Twin.m4b", FileRole::Audio, audio.clone())],
        );
        copy.failing = Some(0);
        let taken = copy.taken.clone();
        let req = copy_req(
            &store,
            &cache,
            job.clone(),
            "B00CUT",
            &uuid,
            OneCopy::of(copy),
        )
        .await;
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(!result.from_library);
        assert_eq!(result.store_fetches, 1);
        assert_eq!(fetches.load(Ordering::SeqCst), before + 1);
        assert_eq!(taken.load(Ordering::SeqCst), 0);

        // No library holds a copy: the store, as before.
        let uuid = edition_book(&store, "B00NONE", "user-1", "Twin", None).await;
        let none = Arc::new(OneCopy::default());
        let before = fetches.load(Ordering::SeqCst);
        let req = copy_req(&store, &cache, job, "B00NONE", &uuid, none.clone()).await;
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(!result.from_library);
        assert_eq!(result.store_fetches, 1);
        assert_eq!(fetches.load(Ordering::SeqCst), before + 1);
        assert_eq!(none.asked.lock().unwrap().len(), 1);
    }

    /// A copy is recorded as the library's before anything is stored, and
    /// one that cannot be recorded is not used: the title comes from the
    /// store and nothing of the copy stays in scratch.
    #[tokio::test]
    async fn a_copy_that_cannot_be_recorded_as_taken_is_not_used() {
        use crate::library_copies::tests::{MemoryCopy, OneCopy};
        use bookclerk_library::FileRole;

        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dest = tmp.path().join("dest");
        let destinations = title_named_destinations(&dest, puts.clone());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let audio = bookclerk_tags::fixtures::tagged_m4b();
        let uuid = edition_book(&store, "B00REC", "user-1", "Twin", None).await;
        let copy_of = || {
            let mut copy = MemoryCopy::new(
                Some("audible"),
                vec![("Twin.m4b", FileRole::Audio, audio.clone())],
            );
            copy.refuses_taken = true;
            copy
        };

        // The record is asked for once the copy is in scratch and before
        // any destination is written.
        let copy = copy_of();
        let taken = copy.taken.clone();
        let req = copy_req(
            &store,
            &cache,
            job.clone(),
            "B00REC",
            &uuid,
            OneCopy::of(copy),
        )
        .await;
        let work_dir = cache.join("acquire").join(&uuid);
        prepare_work_dir(&store, &req, &work_dir).await.unwrap();
        assert!(fetch_from_library(&store, &req, &work_dir).await.is_none());
        assert_eq!(taken.load(Ordering::SeqCst), 1);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
        assert!(!work_dir.join(LIBRARY_COPY_DIR).exists());

        let req = copy_req(&store, &cache, job, "B00REC", &uuid, OneCopy::of(copy_of())).await;
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(!result.from_library);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(dest.join(&result.storage_key)).unwrap(),
            b"fake-m4b",
            "the store's audio is what was stored"
        );
    }

    /// With companion PDFs on, a title whose audio was taken from the
    /// library gets the PDF the library holds too, and the store is not
    /// asked at all.
    #[tokio::test]
    async fn a_title_taken_from_the_library_gets_the_librarys_pdf() {
        use crate::library_copies::tests::{MemoryCopy, OneCopy};
        use bookclerk_library::FileRole;

        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dest = tmp.path().join("dest");
        let mut destinations = title_named_destinations(&dest, puts);
        for item in &mut destinations.items {
            item.options.download_pdf = true;
            // The packaging path removes the acquire's scratch directory
            // when it is done: the PDF must be stored before that.
            item.options.format = bookclerk_config::OutputFormat::EnrichedM4b;
        }
        let job = job_id(&store).await;
        let audio = bookclerk_tags::fixtures::tagged_m4b();
        let uuid = edition_book(&store, "B00PDF", "user-1", "Twin", None).await;
        // The store would give another PDF: it must not be the one stored.
        let pdf_source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: Some(serve_http_body(b"%PDF-1.4 from the store".to_vec()).await),
        };
        let copy = MemoryCopy::new(
            Some("audible"),
            vec![
                ("Twin.m4b", FileRole::Audio, audio.clone()),
                ("Twin.pdf", FileRole::Pdf, b"%PDF-1.4 companion".to_vec()),
            ],
        );
        let mut req = copy_req(&store, &cache, job, "B00PDF", &uuid, OneCopy::of(copy)).await;
        req.options.download_pdf = true;
        let result = acquire_book(&store, &destinations, req, &pdf_source)
            .await
            .unwrap();
        assert!(result.from_library);
        let scratch: Vec<_> = std::fs::read_dir(cache.join("acquire"))
            .map(|dir| dir.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default();
        assert!(scratch.is_empty(), "scratch was cleaned: {scratch:?}");
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            0,
            "the store is not asked, for the audio or for the PDF"
        );
        assert_eq!(
            std::fs::read(dest.join(&result.storage_key)).unwrap(),
            audio
        );
        let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
        assert_eq!(book.pdf_status, AcquireStatus::Acquired);
        let pdf_key = book.pdf_storage_key.expect("the PDF was stored");
        assert!(result.written_keys.contains(&pdf_key), "{result:?}");
        assert_eq!(
            std::fs::read(dest.join(pdf_key)).unwrap(),
            b"%PDF-1.4 companion"
        );
    }

    /// With companion PDFs off, the library's PDF is neither downloaded
    /// nor stored.
    #[tokio::test]
    async fn a_librarys_pdf_is_left_alone_when_companion_pdfs_are_off() {
        use crate::library_copies::tests::{MemoryCopy, OneCopy};
        use bookclerk_library::FileRole;

        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dest = tmp.path().join("dest");
        let destinations = title_named_destinations(&dest, puts);
        let job = job_id(&store).await;
        let uuid = edition_book(&store, "B00NOPDF", "user-1", "Twin", None).await;
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let copy = MemoryCopy::new(
            Some("audible"),
            vec![
                (
                    "Twin.m4b",
                    FileRole::Audio,
                    bookclerk_tags::fixtures::tagged_m4b(),
                ),
                ("Twin.pdf", FileRole::Pdf, b"%PDF-1.4 companion".to_vec()),
            ],
        );
        let copy_fetches = copy.fetched.clone();
        let mut req = copy_req(&store, &cache, job, "B00NOPDF", &uuid, OneCopy::of(copy)).await;
        req.options.download_pdf = false;
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(result.from_library);
        assert_eq!(copy_fetches.load(Ordering::SeqCst), 1, "the audio alone");
        let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
        assert_eq!(book.pdf_storage_key, None);
    }

    /// A library's copy from another store is compared with the title's
    /// own store download once: the better of the two is stored, and the
    /// library's word for its copy is checked against the file it sent.
    #[tokio::test]
    async fn a_copy_from_another_store_replaces_the_download_only_when_better() {
        use crate::library_copies::tests::{MemoryCopy, OneCopy};
        use bookclerk_library::FileRole;
        use bookclerk_tags::fixtures::{tagged_flac, tagged_mp3};

        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let job = job_id(&store).await;
        let uuid = edition_book(&store, "B00CMP", "user-1", "Twin", None).await;
        let work_dir = cache.join("acquire").join(&uuid);

        // The title's own download: a lossy MP3 with a cover and a PDF.
        let own = |work_dir: &Path| {
            let audio = work_dir.join("own.mp3");
            let cover = work_dir.join("own.jpg");
            std::fs::write(&audio, tagged_mp3()).unwrap();
            std::fs::write(&cover, b"jpeg").unwrap();
            PlainFetch {
                parts: vec![bookclerk_source::PlainAudioPart {
                    path: audio,
                    title: None,
                    duration_ms: None,
                }],
                m4b_path: None,
                cover_path: Some(cover),
                chapters: Vec::new(),
                pdf_url: Some("https://store.example/twin.pdf".into()),
            }
        };
        // What the library says of its copy, and the file it sends.
        let holding = holding_of(&store, &uuid).await;
        let compare = |claims: (Option<u32>, bool), audio: Vec<u8>, name: &'static str| {
            let mut copy = MemoryCopy::new(Some("libro"), vec![(name, FileRole::Audio, audio)]);
            copy.claims = claims;
            let (fetched, taken) = (copy.fetched.clone(), copy.taken.clone());
            let mut req = edition_req(&cache, job.clone(), "B00CMP", "user-1");
            req.holding_id = Some(holding.clone());
            req.library_copies = LibraryCopySource::new(OneCopy::of(copy))
                .read_by(Arc::new(bookclerk_media::MediaPool::in_process()));
            // Files are kept as delivered, so a FLAC copy can be stored.
            req.options.format = bookclerk_config::OutputFormat::None;
            (req, fetched, taken)
        };

        // A lossless copy beats the lossy download: it is recorded as
        // taken and stored as the other store's, with the download's
        // cover and PDF, and the download's audio leaves scratch.
        let (req, fetched, taken) = compare((Some(700), true), tagged_flac(), "Twin.flac");
        prepare_work_dir(&store, &req, &work_dir).await.unwrap();
        let offer = offered_copy(&store, &req).await.unwrap();
        assert!(!offer.same_store);
        let own_fetch = own(&work_dir);
        let Compared::Library(better) =
            replace_with_better_copy(&store, &req, &work_dir, &offer, own_fetch).await
        else {
            panic!("the lossless copy is kept");
        };
        assert_eq!(
            better.parts[0].path,
            work_dir.join(LIBRARY_COPY_DIR).join("000.flac")
        );
        assert_eq!(better.cover_path, Some(work_dir.join("own.jpg")));
        assert_eq!(
            better.pdf_url.as_deref(),
            Some("https://store.example/twin.pdf")
        );
        assert!(!work_dir.join("own.mp3").exists());
        assert_eq!(taken.load(Ordering::SeqCst), 1);
        assert_eq!(fetched.load(Ordering::SeqCst), 1);
        assert_eq!(
            req.library_copies.stored_from().get().as_deref(),
            Some("libro")
        );
        remove_library_copy(&work_dir.join(LIBRARY_COPY_DIR)).await;

        // Only the audio is replaced: the download's cover stays when the
        // library sends one too, and the library's stands in when the
        // store sent none.
        for own_cover in [true, false] {
            let mut copy = MemoryCopy::new(
                Some("libro"),
                vec![
                    ("Twin.flac", FileRole::Audio, tagged_flac()),
                    ("cover.png", FileRole::Cover, b"png".to_vec()),
                ],
            );
            copy.claims = (Some(700), true);
            let copy_fetches = copy.fetched.clone();
            let mut req = copy_req(
                &store,
                &cache,
                job.clone(),
                "B00CMP",
                &uuid,
                OneCopy::of(copy),
            )
            .await;
            req.options.format = bookclerk_config::OutputFormat::None;
            let offer = offered_copy(&store, &req).await.unwrap();
            let mut own_fetch = own(&work_dir);
            if !own_cover {
                own_fetch.cover_path = None;
            }
            let Compared::Library(better) =
                replace_with_better_copy(&store, &req, &work_dir, &offer, own_fetch).await
            else {
                panic!("the lossless copy is kept");
            };
            let cover = better.cover_path.unwrap();
            if own_cover {
                assert_eq!(cover, work_dir.join("own.jpg"));
                // The library's cover is not even downloaded.
                assert_eq!(copy_fetches.load(Ordering::SeqCst), 1);
            } else {
                assert_eq!(copy_fetches.load(Ordering::SeqCst), 2);
                assert!(cover.starts_with(work_dir.join(LIBRARY_COPY_DIR)));
                assert_eq!(std::fs::read(&cover).unwrap(), b"png");
            }
            remove_library_copy(&work_dir.join(LIBRARY_COPY_DIR)).await;
        }

        // A copy the library says is no better is not even downloaded.
        let (req, fetched, taken) = compare((None, false), tagged_flac(), "Twin.flac");
        let offer = offered_copy(&store, &req).await.unwrap();
        let own_fetch = own(&work_dir);
        assert!(matches!(
            replace_with_better_copy(&store, &req, &work_dir, &offer, own_fetch).await,
            Compared::Own(_)
        ));
        assert_eq!(fetched.load(Ordering::SeqCst), 0);
        assert_eq!(taken.load(Ordering::SeqCst), 0);
        assert!(work_dir.join("own.mp3").exists());
        assert_eq!(req.library_copies.stored_from().get(), None);

        // A copy said to be lossless that is the same lossy file: this
        // install's own probe decides, and the download is kept.
        let (req, fetched, taken) = compare((Some(900), true), tagged_mp3(), "Twin.mp3");
        let offer = offered_copy(&store, &req).await.unwrap();
        let own_fetch = own(&work_dir);
        assert!(matches!(
            replace_with_better_copy(&store, &req, &work_dir, &offer, own_fetch).await,
            Compared::Own(_)
        ));
        assert_eq!(fetched.load(Ordering::SeqCst), 1);
        assert_eq!(taken.load(Ordering::SeqCst), 0);
        assert!(!work_dir.join(LIBRARY_COPY_DIR).exists());
        assert!(work_dir.join("own.mp3").exists());
        assert_eq!(req.library_copies.stored_from().get(), None);

        // A better copy that does not fit in scratch beside the download
        // is not downloaded: the budget counts what the download holds.
        let flac_len = u64::try_from(tagged_flac().len()).unwrap();
        let (mut req, fetched, taken) = compare((Some(700), true), tagged_flac(), "Twin.flac");
        let own_fetch = own(&work_dir);
        req.temp_quota_bytes = Some(dir_size(&work_dir).await + flac_len - 1);
        assert!(req.job_id.is_some());
        let offer = offered_copy(&store, &req).await.unwrap();
        assert!(matches!(
            replace_with_better_copy(&store, &req, &work_dir, &offer, own_fetch).await,
            Compared::Own(_)
        ));
        assert_eq!(fetched.load(Ordering::SeqCst), 0);
        assert_eq!(taken.load(Ordering::SeqCst), 0);
        assert!(work_dir.join("own.mp3").exists());

        // A lossless copy is used by an output that repackages the audio
        // too: the media worker decodes FLAC.
        let (mut req, fetched, _) = compare((Some(700), true), tagged_flac(), "Twin.flac");
        req.options.format = bookclerk_config::OutputFormat::EnrichedM4b;
        let offer = offered_copy(&store, &req).await.unwrap();
        let own_fetch = own(&work_dir);
        assert!(matches!(
            replace_with_better_copy(&store, &req, &work_dir, &offer, own_fetch).await,
            Compared::Library(_)
        ));
        assert_eq!(fetched.load(Ordering::SeqCst), 1);
        remove_library_copy(&work_dir.join(LIBRARY_COPY_DIR)).await;

        // A lossy MP4 copy is used by an output that repackages audio
        // only when its encoding is one the media worker decodes: the
        // same file with an E-AC-3 sample entry is downloaded, read and
        // dropped, and the store's download is kept.
        let bigger_m4b = |entry: &[u8; 4]| {
            let mut file = bookclerk_tags::fixtures::tagged_m4b();
            let at = file.windows(4).position(|w| w == b"mp4a").unwrap();
            file[at..at + 4].copy_from_slice(entry);
            file.extend(bookclerk_tags::fixtures::mp4_box(
                b"free",
                // Enough to give the 90 s fixture a higher bitrate than
                // the download's.
                &vec![0; 2 << 20],
            ));
            file
        };
        for (entry, used) in [(b"mp4a", true), (b"ec-3", false)] {
            let (mut req, fetched, _) = compare((Some(900), false), bigger_m4b(entry), "Twin.m4b");
            req.options.format = bookclerk_config::OutputFormat::EnrichedM4b;
            let offer = offered_copy(&store, &req).await.unwrap();
            let own_fetch = own(&work_dir);
            let compared =
                replace_with_better_copy(&store, &req, &work_dir, &offer, own_fetch).await;
            assert_eq!(matches!(compared, Compared::Library(_)), used, "{entry:?}");
            assert_eq!(fetched.load(Ordering::SeqCst), 1);
            assert_eq!(work_dir.join("own.mp3").exists(), !used);
            remove_library_copy(&work_dir.join(LIBRARY_COPY_DIR)).await;
        }

        // A better copy that cannot be recorded as taken is not used.
        let mut copy = MemoryCopy::new(
            Some("libro"),
            vec![("Twin.flac", FileRole::Audio, tagged_flac())],
        );
        copy.claims = (Some(700), true);
        copy.refuses_taken = true;
        let mut req = copy_req(
            &store,
            &cache,
            job.clone(),
            "B00CMP",
            &uuid,
            OneCopy::of(copy),
        )
        .await;
        req.options.format = bookclerk_config::OutputFormat::None;
        let offer = offered_copy(&store, &req).await.unwrap();
        let own_fetch = own(&work_dir);
        assert!(matches!(
            replace_with_better_copy(&store, &req, &work_dir, &offer, own_fetch).await,
            Compared::Own(_)
        ));
        assert!(!work_dir.join(LIBRARY_COPY_DIR).exists());
        assert_eq!(req.library_copies.stored_from().get(), None);

        // A download this install cannot probe is kept as it is.
        let (req, fetched, _) = compare((Some(700), true), tagged_flac(), "Twin.flac");
        let offer = offered_copy(&store, &req).await.unwrap();
        let mut unreadable = own(&work_dir);
        std::fs::write(&unreadable.parts[0].path, b"not audio").unwrap();
        unreadable.cover_path = None;
        assert!(matches!(
            replace_with_better_copy(&store, &req, &work_dir, &offer, unreadable).await,
            Compared::Own(_)
        ));
        assert_eq!(fetched.load(Ordering::SeqCst), 0);
    }

    struct CountingBackend {
        inner: bookclerk_storage::LocalFsBackend,
        puts: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl StorageBackend for CountingBackend {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                puts: self.puts.clone(),
            })
        }

        async fn put(
            &self,
            key: &str,
            data: bytes::Bytes,
            meta: ObjectMeta,
        ) -> bookclerk_storage::Result<()> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.inner.put(key, data, meta).await
        }

        async fn put_file(
            &self,
            key: &str,
            path: &Path,
            meta: ObjectMeta,
        ) -> bookclerk_storage::Result<()> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.inner.put_file(key, path, meta).await
        }

        async fn get(&self, key: &str) -> bookclerk_storage::Result<bytes::Bytes> {
            self.inner.get(key).await
        }

        async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
            self.inner.exists(key).await
        }

        async fn list(
            &self,
            prefix: &str,
        ) -> bookclerk_storage::Result<Vec<bookclerk_storage::ObjectInfo>> {
            self.inner.list(prefix).await
        }

        async fn probe(
            &self,
            key: &str,
        ) -> bookclerk_storage::Result<bookclerk_storage::ObjectProbe> {
            self.inner.probe(key).await
        }

        async fn copy(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
            self.inner.copy(from, to).await
        }

        async fn delete(&self, key: &str) -> bookclerk_storage::Result<()> {
            self.inner.delete(key).await
        }

        async fn list_page(
            &self,
            prefix: &str,
            cursor: Option<&str>,
            limit: u32,
        ) -> bookclerk_storage::Result<bookclerk_storage::ListPage> {
            self.inner.list_page(prefix, cursor, limit).await
        }

        async fn get_stream(
            &self,
            key: &str,
            range: Option<bookclerk_storage::ByteRange>,
        ) -> bookclerk_storage::Result<(
            bookclerk_storage::ObjectProbe,
            std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        )> {
            self.inner.get_stream(key, range).await
        }

        async fn put_stream(
            &self,
            key: &str,
            body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            meta: ObjectMeta,
        ) -> bookclerk_storage::Result<bookclerk_storage::PutStreamResult> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.inner.put_stream(key, body, meta).await
        }
    }

    fn counting_destinations(
        root: &Path,
        puts: Arc<std::sync::atomic::AtomicUsize>,
    ) -> AcquireDestinations {
        counting_destinations_with(root, puts, false)
    }

    fn counting_destinations_with(
        root: &Path,
        puts: Arc<std::sync::atomic::AtomicUsize>,
        download_pdf: bool,
    ) -> AcquireDestinations {
        let options = DownloadOptions {
            format: bookclerk_config::OutputFormat::None,
            fixup_metadata: false,
            download_pdf,
            ..DownloadOptions::default()
        };
        AcquireDestinations {
            items: vec![AcquireDestination {
                id: DestinationId::new("local").unwrap(),
                backend: Box::new(CountingBackend {
                    inner: bookclerk_storage::LocalFsBackend::new(root.to_path_buf()).unwrap(),
                    puts,
                }),
                options,
                filter: bookclerk_config::DestinationFilter::default(),
            }],
            primary: DestinationId::new("local").unwrap(),
            multi_destination: MultiDestinationMode::RefetchAll,
        }
    }

    #[tokio::test]
    async fn restart_after_destination_write_does_not_repeat_put() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("library.db");
        let dest_root = tmp.path().join("dest");
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let book_uuid;
        let job_id;
        {
            let store = bookclerk_plugin_database_sqlite::open_store(&db_path)
                .await
                .unwrap();
            store
                .upsert_account("audible", "user-1", "us", None, true, "audible")
                .await
                .unwrap();
            let book = store
                .upsert_book(&bookclerk_library::NewBook::minimal(
                    "B00CRASH", "user-1", "us", "Crash",
                ))
                .await
                .unwrap();
            book_uuid = book.uuid.clone();
            let created = store
                .enqueue_job(EnqueueJobSpec {
                    kind: JobKind::Acquire,
                    payload: JobPayload {
                        account: Some("user-1".into()),
                        title: Some(book.uuid.clone()),
                        ..Default::default()
                    },
                    priority: 0,
                    max_attempts: 3,
                    max_pending: 8,
                    run_after: None,
                })
                .await
                .unwrap();
            let EnqueueOutcome::Created { id } = created else {
                panic!("expected created: {created:?}");
            };
            job_id = id.clone();
            let claimed = store
                .claim_next_job(
                    bookclerk_library::JobResourceClass::Network,
                    "worker-crash",
                    60,
                    "op-crash-1",
                )
                .await
                .unwrap()
                .expect("claim");
            assert_eq!(claimed.id, id);

            let book_uuid_for_record = book.uuid.clone();
            let mut req = dummy_req(&cache, Some(id.clone()), None);
            req.asin = "B00CRASH".into();
            req.holding_id = Some(holding_of(&store, &book.uuid).await);
            req.title = "Crash".into();
            req.options.format = bookclerk_config::OutputFormat::None;
            req.options.fixup_metadata = false;
            let destinations = counting_destinations(&dest_root, puts.clone());
            let first = acquire_book(
                &store,
                &destinations,
                req,
                &CountingSource {
                    fetches: fetches.clone(),
                    pdf_url: None,
                },
            )
            .await
            .expect("first acquire must write the destination");
            assert!(!first.matched_existing);
            assert_eq!(puts.load(Ordering::SeqCst), 1);
            // The written key is recorded for the book's edition on its
            // destination, with its role.
            let recorded = store
                .edition_file("local", &first.storage_key)
                .await
                .unwrap()
                .expect("the acquire records the key it wrote");
            let owner = store
                .file_owner_of_book(&book_uuid_for_record)
                .await
                .unwrap()
                .expect("the upsert resolves the book to an edition");
            assert_eq!(recorded.owner, owner);
            assert_eq!(recorded.writer, bookclerk_library::ACQUIRE_WRITER);
            assert_eq!(fetches.load(Ordering::SeqCst), 1);

            // Crash window: destination succeeded and the book is acquired, but
            // `complete_job` never ran. Expire the lease so restart can reclaim.
            assert!(store
                .heartbeat_job(&claimed.fence().expect("fence"), 0, None)
                .await
                .unwrap());
        }

        let store = bookclerk_plugin_database_sqlite::open_store(&db_path)
            .await
            .unwrap();
        assert_eq!(store.reclaim_expired_leases().await.unwrap(), 1);
        let retry = store
            .claim_next_job(
                bookclerk_library::JobResourceClass::Network,
                "worker-retry",
                60,
                "op-crash-2",
            )
            .await
            .unwrap()
            .expect("retry claim");
        assert_eq!(retry.id, job_id);
        assert_eq!(retry.attempt_count, 2);

        let mut req = dummy_req(&cache, Some(job_id.clone()), None);
        req.asin = "B00CRASH".into();
        req.holding_id = Some(holding_of(&store, &book_uuid).await);
        req.title = "Crash".into();
        req.options.format = bookclerk_config::OutputFormat::None;
        req.options.fixup_metadata = false;
        let destinations = counting_destinations(&dest_root, puts.clone());
        let second = acquire_book(
            &store,
            &destinations,
            req,
            &CountingSource {
                fetches: fetches.clone(),
                pdf_url: None,
            },
        )
        .await
        .expect("retry acquire must skip the already-written destination");
        assert!(second.matched_existing);
        assert_eq!(puts.load(Ordering::SeqCst), 1);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert!(store
            .complete_job(&retry.fence().expect("fence"), Some("acquired=1"))
            .await
            .unwrap());
    }

    /// The account holding of book `uuid`, which acquire requests name.
    async fn holding_of(store: &LibraryStore, uuid: &str) -> String {
        store
            .title_holding_of_book(uuid)
            .await
            .unwrap()
            .expect("the upsert resolves the book to a holding")
            .holding_id
    }

    /// Upserts `product_id` as `title` on `account` (creating the account)
    /// and returns the book's uuid.
    async fn edition_book(
        store: &LibraryStore,
        product_id: &str,
        account: &str,
        title: &str,
        narrators: Option<&str>,
    ) -> String {
        store
            .upsert_account("audible", account, "us", None, true, "audible")
            .await
            .unwrap();
        let mut book = bookclerk_library::NewBook::minimal(product_id, account, "us", title);
        book.narrators = narrators.map(str::to_owned);
        store.upsert_book(&book).await.unwrap().uuid
    }

    /// A request for `product_id` on `account` named by `<title>`
    /// alone, so two editions of one title plan the same key.
    fn edition_req(cache: &Path, job: String, product_id: &str, account: &str) -> AcquireRequest {
        let mut req = dummy_req(cache, Some(job), None);
        req.asin = product_id.into();
        req.account_id = account.into();
        // No holding: written by product id and account, as a request
        // without a known holding is.
        req.title = "Twin".into();
        req
    }

    /// Destinations writing to `root` with titles named
    /// `<title> [<call number>]/<title>`.
    fn title_named_destinations(
        root: &Path,
        puts: Arc<std::sync::atomic::AtomicUsize>,
    ) -> AcquireDestinations {
        let mut destinations = counting_destinations(root, puts);
        for item in &mut destinations.items {
            item.options.folder_template = Some("<title> [<call number>]".into());
            item.options.file_template = Some("<title>".into());
        }
        destinations
    }

    /// The `<call number>` of `uuid`'s edition.
    async fn short_id(store: &LibraryStore, uuid: &str) -> String {
        store
            .edition_of_book(uuid)
            .await
            .unwrap()
            .map(|edition| bookclerk_library::call_number(&edition).unwrap())
            .unwrap()
    }

    /// A store that sends real audio: an M4B the media worker decodes,
    /// with a cover and, when set, a companion PDF.
    struct RealAudioSource {
        /// Fetches asked for.
        fetches: Arc<std::sync::atomic::AtomicUsize>,
        /// The companion PDF's address.
        pdf_url: Option<String>,
        /// True when the store sends a lossy MP3 of lower bitrate than the
        /// M4B ([`real_m4b`]) instead of the M4B itself.
        mp3: bool,
    }

    /// Writes four seconds of real AAC audio as an M4B at `path` (about
    /// 98 kb/s by the probe's size over duration).
    fn real_m4b(path: &Path) {
        bookclerk_media::package_m4b_from_pcm(
            &vec![0i16; 48_000 * 2 * 4],
            48_000,
            2,
            path,
            &[("One".to_string(), 0)],
        )
        .unwrap();
    }

    /// An M4B whose tags and probe read like [`real_m4b`]'s but whose
    /// audio cannot be decoded: its chunk offsets point past the file's end.
    fn undecodable_m4b(dir: &Path) -> Vec<u8> {
        let path = dir.join("undecodable.m4b");
        real_m4b(&path);
        let mut bytes = std::fs::read(&path).unwrap();
        let stco = bytes.windows(4).position(|w| w == b"stco").unwrap();
        let count = u32::from_be_bytes(bytes[stco + 8..stco + 12].try_into().unwrap());
        for at in 0..usize::try_from(count).unwrap() {
            let offset = stco + 12 + at * 4;
            bytes[offset..offset + 4].copy_from_slice(&0x7FFF_0000u32.to_be_bytes());
        }
        bytes
    }

    #[async_trait::async_trait]
    impl ContentSource for RealAudioSource {
        fn id(&self) -> &str {
            "audible"
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            bookclerk_source::PortalAuthMode::Password
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            bookclerk_source::SourceBrand {
                id: "audible",
                name: "Real",
                bg: "#000000",
                fg: "#ffffff",
                accent: "#000000",
                icon_url: "",
            }
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Ok(Vec::new())
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Ok(bookclerk_source::ScanSummary::default())
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            opts: &FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let m4b = opts.cache_dir.join("store.m4b");
            real_m4b(&m4b);
            let cover = opts.cache_dir.join("store.jpg");
            tokio::fs::write(&cover, b"store jpeg").await?;
            let (m4b_path, parts) = if self.mp3 {
                let mp3 = opts.cache_dir.join("store.mp3");
                let lame = bookclerk_config::LameConfig {
                    target: "bitrate".into(),
                    constant_bitrate: true,
                    bitrate_kbps: 32,
                    ..bookclerk_config::LameConfig::default()
                };
                encode_to_mp3(&m4b, &mp3, &lame, Some(22_050), None)
                    .await
                    .map_err(|err| bookclerk_source::SourceError::api(err.to_string()))?;
                tokio::fs::remove_file(&m4b).await?;
                let part = bookclerk_source::PlainAudioPart {
                    path: mp3,
                    title: None,
                    duration_ms: None,
                };
                (None, vec![part])
            } else {
                (Some(m4b), Vec::new())
            };
            Ok(PlainFetch {
                parts,
                m4b_path,
                cover_path: Some(cover),
                chapters: Vec::new(),
                pdf_url: self.pdf_url.clone(),
            })
        }
    }

    /// Runs the media worker in this process for the rest of the test
    /// binary, so real audio encodes without the confined helper.
    fn media_in_process() {
        bookclerk_media::replace_pool(bookclerk_media::MediaPool::new(
            bookclerk_media::MediaPoolConfig {
                confinement: bookclerk_media::Confinement::Off,
                ..bookclerk_media::MediaPoolConfig::default()
            },
        ));
    }

    #[tokio::test]
    async fn a_library_copy_that_does_not_package_is_stored_from_the_store() {
        use crate::library_copies::tests::{MemoryCopy, OneCopy, QueuedCopies};
        use bookclerk_library::FileRole;

        media_in_process();
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dest = tmp.path().join("dest");
        let mut destinations = title_named_destinations(&dest, puts.clone());
        // The output re-encodes the audio, so a copy whose samples do not
        // decode fails in the media worker.
        for item in &mut destinations.items {
            item.options.format = bookclerk_config::OutputFormat::SingleMp3;
        }
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = RealAudioSource {
            fetches: fetches.clone(),
            pdf_url: None,
            mp3: false,
        };
        let job = job_id(&store).await;

        // A copy from the title's own store whose tags read but whose
        // audio is not audio: it is taken, fails packaging, and the store
        // is asked instead.
        let uuid = edition_book(&store, "B00BAD", "user-1", "Twin", None).await;
        let copy = MemoryCopy::new(
            Some("audible"),
            vec![(
                "Twin.m4b",
                FileRole::Audio,
                bookclerk_tags::fixtures::tagged_m4b(),
            )],
        );
        let taken = copy.taken.clone();
        let mut req = copy_req(
            &store,
            &cache,
            job.clone(),
            "B00BAD",
            &uuid,
            OneCopy::of(copy),
        )
        .await;
        req.options = destinations.primary_destination().options.clone();
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .expect("the store's download is stored");
        assert_eq!(taken.load(Ordering::SeqCst), 1, "the copy was taken first");
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "then the store was asked"
        );
        assert_eq!(result.store_fetches, 1, "one store download to count");
        assert!(!result.from_library);
        assert!(result.storage_key.ends_with(".mp3"), "{result:?}");
        let probe = store
            .edition_file_probe("local", &result.storage_key)
            .await
            .unwrap()
            .expect("the stored audio was probed");
        assert_eq!(probe.content_type, "audio/mpeg");
        assert_eq!(probe.source_store.as_deref(), Some("audible"));
        let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
        assert_eq!(book.acquire_status, AcquireStatus::Acquired);

        // A copy from another store that beats the store's MP3 by its
        // probe, and so replaces it, but whose audio does not decode: the
        // store is asked again, since its download left scratch when the
        // copy won.
        let source = RealAudioSource {
            fetches: fetches.clone(),
            pdf_url: None,
            mp3: true,
        };
        edition_book(&store, "B00BADOTHER", "user-1", "Twin", None).await;
        let bad = || {
            let mut copy = MemoryCopy::new(
                Some("libro"),
                vec![("Twin.m4b", FileRole::Audio, undecodable_m4b(tmp.path()))],
            );
            copy.claims = (Some(98), false);
            copy
        };
        // Asked once before the store download and once after it.
        let (first, second) = (bad(), bad());
        let taken = second.taken.clone();
        let mut req = edition_req(&cache, job, "B00BADOTHER", "user-1");
        req.library_copies = LibraryCopySource::new(QueuedCopies::of(vec![first, second]))
            .read_by(Arc::new(bookclerk_media::MediaPool::in_process()));
        req.options = destinations.primary_destination().options.clone();
        let before = fetches.load(Ordering::SeqCst);
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .expect("the store's download is stored");
        assert_eq!(
            taken.load(Ordering::SeqCst),
            1,
            "the copy won and was taken"
        );
        assert_eq!(fetches.load(Ordering::SeqCst), before + 2, "fetched again");
        assert_eq!(result.store_fetches, 2, "both store downloads count");
        assert!(!result.from_library);
        let probe = store
            .edition_file_probe("local", &result.storage_key)
            .await
            .unwrap()
            .expect("the stored audio was probed");
        assert_eq!(
            probe.source_store.as_deref(),
            Some("audible"),
            "the file is the store's, not the other store's"
        );
        let scratch: Vec<_> = std::fs::read_dir(cache.join("acquire"))
            .map(|dir| dir.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default();
        assert!(scratch.is_empty(), "scratch was cleaned: {scratch:?}");
    }

    /// A library's copy from another store that beats the store's download
    /// is stored through the whole acquire: the libraries are asked again
    /// once the download is here, the second answer is the copy taken, and
    /// the store's cover and companion PDF are kept beside its audio.
    #[tokio::test]
    async fn a_better_copy_from_another_store_is_stored_in_place_of_the_download() {
        use crate::library_copies::tests::{MemoryCopy, QueuedCopies};
        use bookclerk_library::FileRole;

        media_in_process();
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dest = tmp.path().join("dest");
        let mut destinations = title_named_destinations(&dest, puts);
        for item in &mut destinations.items {
            item.options.download_pdf = true;
            item.options.download_cover = true;
            item.options.format = bookclerk_config::OutputFormat::EnrichedM4b;
        }
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = RealAudioSource {
            fetches: fetches.clone(),
            pdf_url: Some(serve_http_body(b"%PDF-1.4 from the store".to_vec()).await),
            mp3: true,
        };
        let job = job_id(&store).await;
        let uuid = edition_book(&store, "B00WIN", "user-1", "Twin", None).await;

        // The first answer, before the download, is never downloaded: what
        // a library offered then may have lapsed.
        let mut first = MemoryCopy::new(
            Some("libro"),
            vec![("Old.m4b", FileRole::Audio, b"lapsed".to_vec())],
        );
        first.failing = Some(0);
        let first_fetches = first.fetched.clone();
        let m4b = tmp.path().join("library.m4b");
        real_m4b(&m4b);
        let mut second = MemoryCopy::new(
            Some("libro"),
            vec![
                ("Twin.m4b", FileRole::Audio, std::fs::read(&m4b).unwrap()),
                ("cover.png", FileRole::Cover, b"library png".to_vec()),
            ],
        );
        second.claims = (Some(98), false);
        let (second_fetches, taken) = (second.fetched.clone(), second.taken.clone());
        let copies = QueuedCopies::of(vec![first, second]);
        let mut req = edition_req(&cache, job, "B00WIN", "user-1");
        req.library_copies = LibraryCopySource::new(copies.clone())
            .read_by(Arc::new(bookclerk_media::MediaPool::in_process()));
        req.options = destinations.primary_destination().options.clone();

        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(result.from_library);
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "the store is asked once");
        assert_eq!(copies.asks(), 2, "asked before and after the download");
        assert_eq!(first_fetches.load(Ordering::SeqCst), 0);
        assert_eq!(
            second_fetches.load(Ordering::SeqCst),
            1,
            "only the audio: the store sent a cover"
        );
        assert_eq!(taken.load(Ordering::SeqCst), 1);

        // The stored audio is the library's AAC, not the store's MP3, and
        // its file row names the store the library downloaded it from.
        let probe = store
            .edition_file_probe("local", &result.storage_key)
            .await
            .unwrap()
            .expect("the stored audio was probed");
        assert_eq!(probe.content_type, "audio/mp4");
        assert_eq!(probe.source_store.as_deref(), Some("libro"));

        // The store's cover and companion PDF are kept.
        let cover_key = sidecar_key(&result.storage_key, "jpg");
        assert_eq!(std::fs::read(dest.join(cover_key)).unwrap(), b"store jpeg");
        let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
        assert_eq!(book.acquire_status, AcquireStatus::Acquired);
        let pdf_key = book.pdf_storage_key.expect("the store's PDF was stored");
        assert_eq!(
            std::fs::read(dest.join(pdf_key)).unwrap(),
            b"%PDF-1.4 from the store"
        );
    }

    #[tokio::test]
    async fn written_audio_is_probed_onto_its_file_row() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00PROBE", "user-1", "Probed", None).await;
        let owner = store.file_owner_of_book(&uuid).await.unwrap().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = counting_destinations(&tmp.path().join("dest"), puts);
        let media = Arc::new(bookclerk_media::MediaPool::in_process());
        let recorded = crate::record::recording_destinations_probed_by(
            &store,
            &destinations,
            Some(&owner),
            "audible",
            &crate::library_copies::StoredFrom::default(),
            Arc::clone(&media),
        );
        let backend = &recorded.primary_destination().backend;

        let audio = tmp.path().join("book.m4b");
        tokio::fs::write(&audio, bookclerk_tags::fixtures::tagged_m4b())
            .await
            .unwrap();
        backend
            .put_file("Probed/Probed.m4b", &audio, ObjectMeta::default())
            .await
            .unwrap();
        let probe = store
            .edition_file_probe("local", "Probed/Probed.m4b")
            .await
            .unwrap()
            .expect("the written audio was probed");
        assert_eq!(probe.content_type, "audio/mp4");
        assert!(probe.bitrate_kbps.is_some());
        assert!(!probe.lossless);
        assert_eq!(probe.source_store.as_deref(), Some("audible"));

        // A sidecar is recorded but never probed, and audio the reader
        // cannot parse is stored with no probe instead of failing the write.
        let cue = tmp.path().join("book.cue");
        tokio::fs::write(&cue, b"FILE").await.unwrap();
        backend
            .put_file("Probed/Probed.cue", &cue, ObjectMeta::default())
            .await
            .unwrap();
        let junk = tmp.path().join("part.mp3");
        tokio::fs::write(&junk, b"not audio").await.unwrap();
        backend
            .put_file("Probed/Probed - 02.mp3", &junk, ObjectMeta::default())
            .await
            .unwrap();
        for key in ["Probed/Probed.cue", "Probed/Probed - 02.mp3"] {
            assert!(store.edition_file("local", key).await.unwrap().is_some());
            assert_eq!(store.edition_file_probe("local", key).await.unwrap(), None);
        }

        // A plugin id that is not a plain store id leaves the source out.
        // (Each write gets its own source file: the local backend hard-links
        // what it is given.)
        let again = tmp.path().join("again.m4b");
        tokio::fs::write(&again, bookclerk_tags::fixtures::tagged_m4b())
            .await
            .unwrap();
        tokio::fs::remove_file(tmp.path().join("dest/Probed/Probed.m4b"))
            .await
            .unwrap();
        let odd = crate::record::recording_destinations_probed_by(
            &store,
            &destinations,
            Some(&owner),
            "Odd Store!",
            &crate::library_copies::StoredFrom::default(),
            Arc::clone(&media),
        );
        odd.primary_destination()
            .backend
            .put_file("Probed/Probed.m4b", &again, ObjectMeta::default())
            .await
            .unwrap();
        let probe = store
            .edition_file_probe("local", "Probed/Probed.m4b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(probe.source_store, None);

        // Audio that came from a library's copy of another store names
        // that store, not the title's own.
        let stored_from = crate::library_copies::StoredFrom::default();
        let replaced = crate::record::recording_destinations_probed_by(
            &store,
            &destinations,
            Some(&owner),
            "audible",
            &stored_from,
            media,
        );
        stored_from.set("libro");
        replaced
            .primary_destination()
            .backend
            .put_file("Probed/Probed.m4b", &again, ObjectMeta::default())
            .await
            .unwrap();
        let probe = store
            .edition_file_probe("local", "Probed/Probed.m4b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(probe.source_store.as_deref(), Some("libro"));

        // With no confined worker where one is required (the default pool),
        // the file is still written and recorded, just not probed.
        let unconfined = recording_destinations(
            &store,
            &destinations,
            Some(&owner),
            "audible",
            &crate::library_copies::StoredFrom::default(),
        );
        unconfined
            .primary_destination()
            .backend
            .put_file("Probed/Probed - 03.m4b", &again, ObjectMeta::default())
            .await
            .unwrap();
        assert!(store
            .edition_file("local", "Probed/Probed - 03.m4b")
            .await
            .unwrap()
            .is_some());
    }

    /// What the destination filters see of an Audible book held by user
    /// 3's `user-1` account.
    fn audible_placement() -> bookclerk_config::PlacementFacts {
        bookclerk_config::PlacementFacts {
            patrons: vec![3],
            storefront: "audible".into(),
            account: "user-1".into(),
            media_kind: bookclerk_config::PlacementMediaKind::Book,
        }
    }

    /// A filter of one rule on the storefront.
    fn storefront_filter(
        value: &str,
        effect: bookclerk_config::FilterEffect,
    ) -> bookclerk_config::DestinationFilter {
        bookclerk_config::DestinationFilter {
            rules: vec![bookclerk_config::FilterRule {
                field: bookclerk_config::FilterField::Storefront,
                value: value.into(),
                effect,
            }],
        }
    }

    #[tokio::test]
    async fn a_book_no_destination_takes_fails_before_it_is_fetched() {
        let store = test_store().await;
        edition_book(&store, "B00NONE", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations = title_named_destinations(&tmp.path().join("dest"), puts.clone());
        destinations.items[0].filter =
            storefront_filter("audible", bookclerk_config::FilterEffect::Exclude);
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: Some("https://example.invalid/book.pdf".into()),
        };
        let job = job_id(&store).await;

        let mut req = edition_req(&cache, job.clone(), "B00NONE", "user-1");
        req.placement = Some(audible_placement());
        let err = acquire_book(&store, &destinations, req.clone(), &source)
            .await
            .unwrap_err();
        assert!(matches!(err, AcquireError::NoDestinationAccepts), "{err}");
        let err = acquire_pdf_only(&store, &destinations, &req, &source)
            .await
            .unwrap_err();
        assert!(matches!(err, AcquireError::NoDestinationAccepts), "{err}");
        assert_eq!(fetches.load(Ordering::SeqCst), 0, "nothing is fetched");
        assert_eq!(puts.load(Ordering::SeqCst), 0, "nothing is written");

        // A request without placement facts writes to every destination
        // given, as a host acquiring for an owner does.
        req.placement = None;
        acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(puts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_book_is_written_only_to_the_destinations_that_take_it() {
        let store = test_store().await;
        edition_book(&store, "B00SOME", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let refused_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let taken_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // The primary takes only Libro.fm books; the second takes the rest.
        let mut destinations =
            title_named_destinations(&tmp.path().join("libro"), refused_puts.clone());
        destinations.items[0].filter =
            storefront_filter("libro", bookclerk_config::FilterEffect::Include);
        let mut second = title_named_destinations(&tmp.path().join("rest"), taken_puts.clone())
            .items
            .remove(0);
        second.id = DestinationId::new("rest").unwrap();
        second.filter = storefront_filter("libro", bookclerk_config::FilterEffect::Exclude);
        destinations.items.push(second);
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;

        let mut req = edition_req(&cache, job, "B00SOME", "user-1");
        req.placement = Some(audible_placement());
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert_eq!(refused_puts.load(Ordering::SeqCst), 0);
        assert_eq!(taken_puts.load(Ordering::SeqCst), 1);
        assert!(store
            .edition_file("rest", &result.storage_key)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .edition_file("local", &result.storage_key)
            .await
            .unwrap()
            .is_none());
    }

    /// A copy from its `format:quality` name.
    fn output_copy(raw: &str) -> bookclerk_config::OutputCopy {
        bookclerk_config::OutputCopy::parse(raw).unwrap()
    }

    /// A filter of one rule on a copy's format.
    fn format_filter(
        value: &str,
        effect: bookclerk_config::FilterEffect,
    ) -> bookclerk_config::DestinationFilter {
        bookclerk_config::DestinationFilter {
            rules: vec![bookclerk_config::FilterRule {
                field: bookclerk_config::FilterField::Format,
                value: value.into(),
                effect,
            }],
        }
    }

    #[tokio::test]
    async fn sync_skips_a_destination_that_refuses_the_main_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations = title_named_destinations(&tmp.path().join("local"), puts.clone());
        destinations.items[0].options.copies = vec![output_copy("mp3_chapters:64k-mono")];
        let mut phone = title_named_destinations(&tmp.path().join("phone"), puts.clone())
            .items
            .remove(0);
        phone.id = DestinationId::new("phone").unwrap();
        phone.filter = format_filter("mp3_chapters", bookclerk_config::FilterEffect::Include);
        destinations.items.push(phone);
        let local = destinations.items[0].id.clone();
        destinations.items[0]
            .backend
            .put(
                "Book/Book.m4b",
                bytes::Bytes::from_static(b"audio"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let journal = tmp.path().join("journal");
        let missing = [(
            DestinationId::new("phone").unwrap(),
            "Book/Book.m4b".to_string(),
        )];
        let written =
            sync_missing_destinations(&destinations, &local, "Book/Book.m4b", &missing, &journal)
                .await
                .unwrap();
        assert!(written.is_empty(), "the phone takes only the MP3 copy");

        destinations.items[1].filter = bookclerk_config::DestinationFilter::default();
        let written =
            sync_missing_destinations(&destinations, &local, "Book/Book.m4b", &missing, &journal)
                .await
                .unwrap();
        assert_eq!(
            written.len(),
            1,
            "a destination taking every copy is synced"
        );
    }

    #[test]
    fn a_destination_that_failed_any_copy_counts_as_failed() {
        let outcome = |id: &str, failed: bool| CopyOutcome {
            destination: DestinationId::new(id).unwrap(),
            result: if failed {
                CopyResult::Failed {
                    error: "offline".into(),
                }
            } else {
                CopyResult::Stored {
                    key: format!("{id}.m4b"),
                }
            },
        };
        let mut outcomes = vec![outcome("local", false), outcome("phone", true)];
        merge_copy_outcomes(
            &mut outcomes,
            vec![
                outcome("local", true),
                outcome("phone", false),
                outcome("cloud", false),
            ],
        );
        assert_eq!(
            outcomes,
            vec![
                outcome("local", true),
                outcome("phone", true),
                outcome("cloud", false)
            ]
        );
    }

    #[test]
    fn the_first_copy_that_writes_names_the_acquire() {
        let stored = |key: &str, written: &[&str]| AcquireResult {
            asin: "B00TEST".into(),
            storage_key: key.into(),
            written_keys: written.iter().map(|key| (*key).to_string()).collect(),
            matched_existing: false,
            from_library: false,
            store_fetches: 0,
            copies: Vec::new(),
        };
        let mut result = None;
        fold_copy_result(&mut result, stored("", &["skipped.json"]));
        fold_copy_result(&mut result, stored("mp3/a.mp3", &["mp3/b.mp3"]));
        fold_copy_result(&mut result, stored("m4b/a.m4b", &[]));
        let unwritten = Some(stored("", &["skipped.json"]));
        assert!(matches!(
            finish_copies(unwritten),
            Err(AcquireError::NoDestinationAccepts)
        ));
        assert!(finish_copies(None).is_err());
        let result = finish_copies(result).unwrap();
        assert_eq!(result.storage_key, "mp3/a.mp3");
        assert_eq!(
            result.written_keys,
            ["mp3/b.mp3", "skipped.json", "m4b/a.m4b"]
        );
    }

    #[test]
    fn a_chapter_copy_beside_the_main_output_needs_mp4_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = dummy_req(tmp.path(), None, None);
        req.options.format = bookclerk_config::OutputFormat::SingleMp3;
        req.options.split_files_by_chapter = false;
        assert!(!splits_any_copy(&req));
        req.options.copies = vec![output_copy("mp3_chapters:64k-mono")];
        assert!(splits_any_copy(&req));
    }

    #[tokio::test]
    async fn copies_that_would_share_a_folder_are_refused_before_storing() {
        let store = test_store().await;
        edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations = title_named_destinations(&tmp.path().join("local"), puts.clone());
        destinations.items[0].options.copies = vec![output_copy("mp3:128k")];
        let plan = destinations.copy_plan();
        // A destination template that already names the label, as the
        // main copy's folder: the extra copy's folder would be the same.
        let label = plan[1].1.items[0].options.folder_template.clone().unwrap();
        let mut copies: Vec<CopyTarget> = plan
            .into_iter()
            .map(|(copy, destinations)| CopyTarget { copy, destinations })
            .collect();
        copies[0].destinations.items[0].options.folder_template = Some(label);
        let job = job_id(&store).await;
        let mut req = edition_req(&cache, job, "B00SAME", "user-1");
        req.options = destinations.primary_destination().options.clone();
        let fetch = PlainFetch {
            parts: Vec::new(),
            m4b_path: None,
            cover_path: None,
            chapters: Vec::new(),
            pdf_url: None,
        };
        let err = store_copies(&store, &copies, &req, tmp.path(), fetch, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("would share the folder"), "{err}");
        assert_eq!(puts.load(Ordering::SeqCst), 0, "nothing is stored");

        copies.truncate(1);
        check_copy_folders(&store, &copies, &req).await.unwrap();
    }

    /// With companion PDFs on, a destination that takes only an extra copy
    /// gets the PDF beside that copy, in its folder; one that takes the main
    /// copy too keeps the one PDF beside the main copy, and the title's
    /// recorded PDF stays the main copy's.
    #[tokio::test]
    async fn a_destination_with_only_an_extra_copy_gets_the_pdf_beside_it() {
        media_in_process();
        let store = test_store().await;
        let uuid = edition_book(&store, "B00PDFCOPY", "user-1", "Twin", None).await;
        let owner = store.file_owner_of_book(&uuid).await.unwrap().unwrap();
        let id = short_id(&store, &uuid).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations = title_named_destinations(&tmp.path().join("local"), puts.clone());
        destinations.items[0].options.copies = vec![output_copy("mp3:64k")];
        let mut phone = title_named_destinations(&tmp.path().join("phone"), puts)
            .items
            .remove(0);
        phone.id = DestinationId::new("phone").unwrap();
        phone.filter = format_filter("mp3", bookclerk_config::FilterEffect::Include);
        destinations.items.push(phone);
        for item in &mut destinations.items {
            item.options.download_pdf = true;
        }
        let copies: Vec<CopyTarget> = destinations
            .copy_plan()
            .into_iter()
            .map(|(copy, targets)| CopyTarget {
                destinations: recording_copy_destinations(
                    &store,
                    &targets,
                    Some(&owner),
                    "audible",
                    &crate::library_copies::StoredFrom::default(),
                    &copy,
                ),
                copy,
            })
            .collect();
        assert_eq!(copies.len(), 2);
        let job = job_id(&store).await;
        let mut req = edition_req(&cache, job, "B00PDFCOPY", "user-1");
        req.options = destinations.primary_destination().options.clone();
        pin_call_number(&mut req, Some(&owner));
        let work_dir = cache.join("acquire").join(&uuid);
        tokio::fs::create_dir_all(&work_dir).await.unwrap();
        let m4b = work_dir.join("store.m4b");
        real_m4b(&m4b);
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetch = PlainFetch {
            parts: Vec::new(),
            m4b_path: Some(m4b),
            cover_path: None,
            chapters: Vec::new(),
            pdf_url: Some(serve_http_body_loop(b"%PDF-1.4 companion".to_vec(), hits.clone()).await),
        };

        let result = store_copies(&store, &copies, &req, &work_dir, fetch, None)
            .await
            .unwrap();
        let main_pdf = format!("Twin [{id}]/Twin.pdf");
        let copy_pdf = format!("mp3 64k/Twin [{id}]/Twin.pdf");
        let local = tmp.path().join("local");
        let phone = tmp.path().join("phone");
        assert_eq!(
            std::fs::read(local.join(&main_pdf)).unwrap(),
            b"%PDF-1.4 companion"
        );
        assert!(
            !local.join(&copy_pdf).exists(),
            "a destination with the main copy keeps one PDF"
        );
        assert_eq!(
            std::fs::read(phone.join(&copy_pdf)).unwrap(),
            b"%PDF-1.4 companion",
            "the phone gets the PDF beside its copy"
        );
        assert!(!phone.join(&main_pdf).exists());
        assert!(result.written_keys.contains(&copy_pdf), "{result:?}");
        let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
        assert_eq!(book.pdf_storage_key.as_deref(), Some(main_pdf.as_str()));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "once per copy that needs it"
        );

        // Acquired again, the phone already holds its PDF: nothing is
        // downloaded for it, and the main copy's PDF is reused.
        let work_dir = cache.join("acquire").join(&uuid);
        tokio::fs::create_dir_all(&work_dir).await.unwrap();
        let m4b = work_dir.join("store.m4b");
        real_m4b(&m4b);
        let url = serve_http_body_loop(b"%PDF-1.4 companion".to_vec(), hits.clone()).await;
        let fetch = PlainFetch {
            parts: Vec::new(),
            m4b_path: Some(m4b),
            cover_path: None,
            chapters: Vec::new(),
            pdf_url: Some(url),
        };
        store_copies(&store, &copies, &req, &work_dir, fetch, None)
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2, "no PDF is fetched again");
    }

    /// A later copy that no destination stores leaves the main output
    /// stored: the acquire succeeds and the copy is pending where it was to
    /// go.
    #[tokio::test]
    async fn a_copy_no_destination_stores_is_retried_while_the_main_one_stays() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00COPY", "user-1", "Twin", None).await;
        let owner = store.file_owner_of_book(&uuid).await.unwrap().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let local_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let phone_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // The library keeps the store's files and an MP3 copy per chapter;
        // `local` takes every copy, `phone` only the MP3 one.
        let mut destinations =
            title_named_destinations(&tmp.path().join("local"), local_puts.clone());
        destinations.items[0].options.copies = vec![output_copy("mp3_chapters:64k-mono")];
        let mut phone = title_named_destinations(&tmp.path().join("phone"), phone_puts.clone())
            .items
            .remove(0);
        phone.id = DestinationId::new("phone").unwrap();
        phone.filter = format_filter("mp3_chapters", bookclerk_config::FilterEffect::Include);
        destinations.items.push(phone);
        // `local` keeps only the store's files, and every write to `phone`
        // fails.
        destinations.items[0].filter =
            format_filter("original", bookclerk_config::FilterEffect::Include);
        std::fs::remove_dir_all(tmp.path().join("phone")).ok();
        std::fs::write(tmp.path().join("phone"), b"not a folder").unwrap();

        let plan = destinations.copy_plan();
        let copies: Vec<CopyTarget> = plan
            .into_iter()
            .map(|(copy, targets)| CopyTarget {
                destinations: recording_copy_destinations(
                    &store,
                    &targets,
                    Some(&owner),
                    "audible",
                    &crate::library_copies::StoredFrom::default(),
                    &copy,
                ),
                copy,
            })
            .collect();
        let job = job_id(&store).await;
        let mut req = edition_req(&cache, job, "B00COPY", "user-1");
        req.options = destinations.primary_destination().options.clone();
        pin_call_number(&mut req, Some(&owner));
        let work_dir = cache.join("acquire").join(&uuid);
        tokio::fs::create_dir_all(&work_dir).await.unwrap();
        // The MP3 copy is encoded again from the store's MP3 parts, so the
        // parts are real audio and this test runs the codec in-process.
        bookclerk_media::replace_pool(bookclerk_media::MediaPool::new(
            bookclerk_media::MediaPoolConfig {
                confinement: bookclerk_media::Confinement::Off,
                ..bookclerk_media::MediaPoolConfig::default()
            },
        ));
        let source = work_dir.join("source.m4b");
        bookclerk_media::package_m4b_from_pcm(
            &vec![0i16; 48_000 * 2],
            48_000,
            2,
            &source,
            &[("One".to_string(), 0)],
        )
        .unwrap();
        let delivered = bookclerk_config::LameConfig {
            target: "bitrate".into(),
            constant_bitrate: true,
            bitrate_kbps: 192,
            ..bookclerk_config::LameConfig::default()
        };
        let mut parts = Vec::new();
        let mut delivered_bytes = Vec::new();
        for n in 1..=2 {
            let path = work_dir.join(format!("part{n}.mp3"));
            encode_to_mp3(&source, &path, &delivered, None, None)
                .await
                .unwrap();
            delivered_bytes.push(std::fs::read(&path).unwrap());
            parts.push(bookclerk_source::PlainAudioPart {
                path,
                title: Some(format!("Chapter {n}")),
                duration_ms: Some(1_000),
            });
        }
        let fetch = PlainFetch {
            parts,
            m4b_path: None,
            cover_path: None,
            chapters: Vec::new(),
            pdf_url: None,
        };

        let result = store_copies(&store, &copies, &req, &work_dir, fetch, None)
            .await
            .expect("the main output is stored");
        assert!(!result.storage_key.is_empty());
        let outcomes: Vec<(&str, bool)> = result
            .copies
            .iter()
            .map(|copy| {
                (
                    copy.destination.as_str(),
                    matches!(copy.result, CopyResult::Stored { .. }),
                )
            })
            .collect();
        assert_eq!(outcomes, [("local", true), ("phone", false)]);
    }

    #[tokio::test]
    async fn each_copy_is_stored_and_recorded_as_its_own() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00COPY", "user-1", "Twin", None).await;
        let owner = store.file_owner_of_book(&uuid).await.unwrap().unwrap();
        let id = short_id(&store, &uuid).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let local_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let phone_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // The library keeps the store's files and an MP3 copy per chapter;
        // `local` takes every copy, `phone` only the MP3 one.
        let mut destinations =
            title_named_destinations(&tmp.path().join("local"), local_puts.clone());
        destinations.items[0].options.copies = vec![output_copy("mp3_chapters:64k-mono")];
        let mut phone = title_named_destinations(&tmp.path().join("phone"), phone_puts.clone())
            .items
            .remove(0);
        phone.id = DestinationId::new("phone").unwrap();
        phone.filter = format_filter("mp3_chapters", bookclerk_config::FilterEffect::Include);
        destinations.items.push(phone);

        let plan = destinations.copy_plan();
        let shape: Vec<(String, Vec<&str>)> = plan
            .iter()
            .map(|(copy, targets)| {
                (
                    copy.to_string(),
                    targets.items.iter().map(|dest| dest.id.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("original:original".to_string(), vec!["local"]),
                ("mp3_chapters:64k-mono".to_string(), vec!["local", "phone"]),
            ]
        );
        let mp3 = &plan[1].1;
        assert_eq!(mp3.primary.as_str(), "local");
        assert!(mp3.items[0].options.wants_split_by_chapter());
        assert_eq!(mp3.items[0].options.lame.bitrate_kbps, 64);
        for dest in &mp3.items {
            assert_eq!(
                dest.options.folder_template.as_deref(),
                Some("mp3_chapters 64k-mono/<title> [<call number>]"),
                "a copy other than the main output gets a folder of its own"
            );
            assert_eq!(
                dest.options.copies,
                [output_copy("mp3_chapters:64k-mono")],
                "every copy request carries the library's copy list"
            );
        }
        assert_eq!(
            plan[0].1.items[0].options.folder_template.as_deref(),
            Some("<title> [<call number>]"),
            "the main output's copy keeps the plain names"
        );

        let copies: Vec<CopyTarget> = plan
            .into_iter()
            .map(|(copy, targets)| CopyTarget {
                destinations: recording_copy_destinations(
                    &store,
                    &targets,
                    Some(&owner),
                    "audible",
                    &crate::library_copies::StoredFrom::default(),
                    &copy,
                ),
                copy,
            })
            .collect();
        let job = job_id(&store).await;
        let mut req = edition_req(&cache, job, "B00COPY", "user-1");
        req.options = destinations.primary_destination().options.clone();
        pin_call_number(&mut req, Some(&owner));
        let work_dir = cache.join("acquire").join(&uuid);
        tokio::fs::create_dir_all(&work_dir).await.unwrap();
        // The MP3 copy is encoded again from the store's MP3 parts, so the
        // parts are real audio and this test runs the codec in-process.
        bookclerk_media::replace_pool(bookclerk_media::MediaPool::new(
            bookclerk_media::MediaPoolConfig {
                confinement: bookclerk_media::Confinement::Off,
                ..bookclerk_media::MediaPoolConfig::default()
            },
        ));
        let source = work_dir.join("source.m4b");
        bookclerk_media::package_m4b_from_pcm(
            &vec![0i16; 48_000 * 2],
            48_000,
            2,
            &source,
            &[("One".to_string(), 0)],
        )
        .unwrap();
        let delivered = bookclerk_config::LameConfig {
            target: "bitrate".into(),
            constant_bitrate: true,
            bitrate_kbps: 192,
            ..bookclerk_config::LameConfig::default()
        };
        let mut parts = Vec::new();
        let mut delivered_bytes = Vec::new();
        for n in 1..=2 {
            let path = work_dir.join(format!("part{n}.mp3"));
            encode_to_mp3(&source, &path, &delivered, None, None)
                .await
                .unwrap();
            delivered_bytes.push(std::fs::read(&path).unwrap());
            parts.push(bookclerk_source::PlainAudioPart {
                path,
                title: Some(format!("Chapter {n}")),
                duration_ms: Some(1_000),
            });
        }
        let fetch = PlainFetch {
            parts,
            m4b_path: None,
            cover_path: None,
            chapters: Vec::new(),
            pdf_url: None,
        };

        let result = store_copies(&store, &copies, &req, &work_dir, fetch, None)
            .await
            .unwrap();
        let plain = format!("Twin [{id}]/");
        let copy_folder = format!("mp3_chapters 64k-mono/Twin [{id}]/");
        assert!(
            result.storage_key.starts_with(&plain),
            "the main copy names the title: {}",
            result.storage_key
        );
        assert_eq!(local_puts.load(Ordering::SeqCst), 4, "two parts per copy");
        assert_eq!(phone_puts.load(Ordering::SeqCst), 2, "the MP3 copy only");
        assert!(
            result
                .written_keys
                .iter()
                .any(|key| key.starts_with(&copy_folder)),
            "{:?}",
            result.written_keys
        );
        assert_eq!(
            store
                .edition_file_copy("local", &result.storage_key)
                .await
                .unwrap(),
            Some(output_copy("original:original"))
        );
        let mut local_copy_keys = 0;
        for entry in walk(&tmp.path().join("local")) {
            let key = entry
                .strip_prefix(tmp.path().join("local"))
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if key.starts_with(&copy_folder) && !key.ends_with(".json") {
                local_copy_keys += 1;
                assert_eq!(
                    store.edition_file_copy("local", &key).await.unwrap(),
                    Some(output_copy("mp3_chapters:64k-mono")),
                    "{key}"
                );
            }
        }
        assert_eq!(local_copy_keys, 2);
        let phone_keys: Vec<String> = walk(&tmp.path().join("phone"))
            .into_iter()
            .map(|path| {
                path.strip_prefix(tmp.path().join("phone"))
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .filter(|key| !key.ends_with(".json"))
            .collect();
        assert_eq!(phone_keys.len(), 2, "{phone_keys:?}");
        for key in &phone_keys {
            assert!(key.starts_with(&copy_folder), "{key}");
            let bytes = std::fs::read(tmp.path().join("phone").join(key)).unwrap();
            assert!(
                !delivered_bytes.contains(&bytes),
                "the 64k mono copy is encoded again, not the store's 192k MP3: {key}"
            );
            assert_eq!(
                mp3_frame_shape(&bytes),
                Some((64, true)),
                "the copy holds 64 kbit/s mono audio: {key}"
            );
            assert_eq!(
                store.edition_file_copy("phone", key).await.unwrap(),
                Some(output_copy("mp3_chapters:64k-mono"))
            );
        }
        assert!(!work_dir.exists(), "the scratch copies are cleaned up");
    }

    /// The bitrate in kbit/s and whether the audio is mono, read from the
    /// first MPEG audio frame header of an MP3 (after any ID3v2 tag).
    fn mp3_frame_shape(bytes: &[u8]) -> Option<(u32, bool)> {
        let mut at = 0;
        if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
            let size = bytes[6..10]
                .iter()
                .fold(0usize, |size, b| (size << 7) | usize::from(b & 0x7f));
            at = 10 + size;
        }
        while at + 4 <= bytes.len() {
            let h = &bytes[at..at + 4];
            if h[0] == 0xff && h[1] & 0xe0 == 0xe0 && h[1] & 0x06 == 0x02 {
                let mpeg1 = h[1] & 0x18 == 0x18;
                let index = usize::from(h[2] >> 4);
                let table: [u32; 15] = if mpeg1 {
                    [
                        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
                    ]
                } else {
                    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160]
                };
                return Some((*table.get(index)?, h[3] >> 6 == 3));
            }
            at += 1;
        }
        None
    }

    /// Every file under `root`, recursively.
    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    #[tokio::test]
    async fn a_book_whose_every_copy_is_refused_fails_before_it_is_fetched() {
        let store = test_store().await;
        edition_book(&store, "B00NOCOPY", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations = title_named_destinations(&tmp.path().join("dest"), puts.clone());
        destinations.items[0].filter =
            format_filter("mp3", bookclerk_config::FilterEffect::Include);
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let mut req = edition_req(&cache, job, "B00NOCOPY", "user-1");
        req.placement = Some(audible_placement());
        let err = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap_err();
        assert!(matches!(err, AcquireError::NoDestinationAccepts), "{err}");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_library_with_one_copy_names_it_on_the_files_it_writes() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00ONE", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&tmp.path().join("dest"), puts.clone());
        let source = CountingSource {
            fetches: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00ONE", "user-1");
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        let id = short_id(&store, &uuid).await;
        assert!(result.storage_key.starts_with(&format!("Twin [{id}]/")));
        assert_eq!(puts.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .edition_file_copy("local", &result.storage_key)
                .await
                .unwrap(),
            Some(output_copy("original:original"))
        );
    }

    /// The copies `store` records for `uuid`'s edition, as
    /// `(destination, state, attempts)`.
    async fn copy_states(store: &LibraryStore, uuid: &str) -> Vec<(String, CopyState, u32)> {
        let edition = store.edition_of_book(uuid).await.unwrap().unwrap();
        store
            .copies_of_editions(&[edition])
            .await
            .unwrap()
            .into_iter()
            .map(|copy| (copy.destination, copy.state, copy.attempts))
            .collect()
    }

    #[tokio::test]
    async fn a_destination_that_fails_leaves_a_pending_copy_that_the_next_acquire_fills() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00SOME", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let local_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations =
            title_named_destinations(&tmp.path().join("local"), local_puts.clone());
        let second_root = tmp.path().join("second");
        let mut second = title_named_destinations(&second_root, second_puts.clone())
            .items
            .remove(0);
        second.id = DestinationId::new("second").unwrap();
        destinations.items.push(second);
        destinations.multi_destination = MultiDestinationMode::SyncMissing;
        // The second destination is unavailable: its root is a file, so
        // every write under it fails.
        std::fs::remove_dir_all(&second_root).unwrap();
        std::fs::write(&second_root, b"not a folder").unwrap();
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };

        let job = job_id(&store).await;
        let req = edition_req(&cache, job.clone(), "B00SOME", "user-1");
        let result = acquire_book(&store, &destinations, req, &source)
            .await
            .expect("the first stored copy is enough");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert!(
            tokio::fs::try_exists(tmp.path().join("local").join(&result.storage_key))
                .await
                .unwrap()
        );
        assert_eq!(
            copy_states(&store, &uuid).await,
            vec![
                ("local".to_owned(), CopyState::Stored, 0),
                ("second".to_owned(), CopyState::Pending, 1),
            ]
        );

        // Once the destination is back, the next acquire copies the title
        // there from the stored copy, though the title is acquired already.
        std::fs::remove_file(&second_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        let req = edition_req(&cache, job.clone(), "B00SOME", "user-1");
        let again = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(again.matched_existing);
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "no second fetch");
        let Some(CopyResult::Stored { key }) = again
            .copies
            .iter()
            .find(|copy| copy.destination.as_str() == "second")
            .map(|copy| copy.result.clone())
        else {
            panic!("{again:?}");
        };
        assert_eq!(
            std::fs::read(second_root.join(&key)).unwrap(),
            std::fs::read(tmp.path().join("local").join(&result.storage_key)).unwrap()
        );
        assert_eq!(
            copy_states(&store, &uuid).await,
            vec![
                ("local".to_owned(), CopyState::Stored, 0),
                ("second".to_owned(), CopyState::Stored, 0),
            ]
        );

        // With every copy stored, a third acquire fetches and writes nothing.
        let req = edition_req(&cache, job, "B00SOME", "user-1");
        let puts_before = second_puts.load(Ordering::SeqCst);
        assert!(
            acquire_book(&store, &destinations, req, &source)
                .await
                .unwrap()
                .matched_existing
        );
        assert_eq!(second_puts.load(Ordering::SeqCst), puts_before);
    }

    /// A copy that failed for good waits for a forced acquire: an acquire
    /// that fills another destination's missing copy leaves it alone.
    #[tokio::test]
    async fn a_failed_copy_is_left_alone_while_another_destination_is_filled() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00SOME", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failed_puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let local_only = title_named_destinations(&tmp.path().join("local"), Arc::default());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job.clone(), "B00SOME", "user-1");
        let first = acquire_book(&store, &local_only, req, &source)
            .await
            .unwrap();
        let edition = store.edition_of_book(&uuid).await.unwrap().unwrap();
        let now = chrono::Utc::now();
        for _ in 0..bookclerk_library::MAX_COPY_ATTEMPTS {
            store
                .set_copy_pending(&edition, "broken", "offline", now)
                .await
                .unwrap();
        }

        let mut destinations = title_named_destinations(&tmp.path().join("local"), Arc::default());
        for (id, puts) in [("broken", failed_puts.clone()), ("fresh", Arc::default())] {
            let mut extra = title_named_destinations(&tmp.path().join(id), puts)
                .items
                .remove(0);
            extra.id = DestinationId::new(id).unwrap();
            destinations.items.push(extra);
        }
        destinations.multi_destination = MultiDestinationMode::SyncMissing;
        let req = edition_req(&cache, job, "B00SOME", "user-1");
        let again = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(again.matched_existing);
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "no second fetch");
        assert_eq!(failed_puts.load(Ordering::SeqCst), 0, "nothing written");
        let Some(CopyResult::Stored { key }) = again
            .copies
            .iter()
            .find(|copy| copy.destination.as_str() == "fresh")
            .map(|copy| copy.result.clone())
        else {
            panic!("{again:?}");
        };
        assert_eq!(
            std::fs::read(tmp.path().join("fresh").join(&key)).unwrap(),
            std::fs::read(tmp.path().join("local").join(&first.storage_key)).unwrap()
        );
        assert_eq!(
            copy_states(&store, &uuid).await,
            vec![
                (
                    "broken".to_owned(),
                    CopyState::Failed,
                    bookclerk_library::MAX_COPY_ATTEMPTS
                ),
                ("fresh".to_owned(), CopyState::Stored, 0),
                ("local".to_owned(), CopyState::Stored, 0),
            ]
        );
    }

    /// A refetch that only fills a missing destination and fails there
    /// leaves the title acquired and counts an attempt on that copy.
    #[tokio::test]
    async fn a_failed_refetch_into_a_missing_destination_counts_an_attempt() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00SOME", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let local_only = title_named_destinations(&tmp.path().join("local"), Arc::default());
        let job = job_id(&store).await;
        let req = edition_req(&cache, job.clone(), "B00SOME", "user-1");
        let first = acquire_book(&store, &local_only, req, &source)
            .await
            .unwrap();

        let mut destinations = title_named_destinations(&tmp.path().join("local"), Arc::default());
        let broken_root = tmp.path().join("broken");
        let mut broken = title_named_destinations(&broken_root, Arc::default())
            .items
            .remove(0);
        broken.id = DestinationId::new("broken").unwrap();
        destinations.items.push(broken);
        destinations.multi_destination = MultiDestinationMode::RefetchMissing;
        std::fs::remove_dir_all(&broken_root).unwrap();
        std::fs::write(&broken_root, b"not a folder").unwrap();

        let req = edition_req(&cache, job, "B00SOME", "user-1");
        let again = acquire_book(&store, &destinations, req, &source)
            .await
            .expect("the stored copy keeps the title acquired");
        assert!(again.matched_existing);
        assert_eq!(again.storage_key, first.storage_key);
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            2,
            "refetched for the missing copy"
        );
        assert_eq!(
            copy_states(&store, &uuid).await,
            vec![
                ("broken".to_owned(), CopyState::Pending, 1),
                ("local".to_owned(), CopyState::Stored, 0),
            ]
        );
        let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
        assert_eq!(book.acquire_status, AcquireStatus::Acquired);
        assert_eq!(again.store_fetches, 1, "the failed refetch asked the store");
    }

    /// A refetch into a missing destination that the store refuses for
    /// downloading too much hands the refusal to the job, so the account
    /// pauses, and the title stays acquired at the copy it has.
    #[tokio::test]
    async fn a_throttled_refetch_into_a_missing_destination_reaches_the_job() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00SOME", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let source = ThrottlesAfterFirst(CountingSource {
            fetches: Arc::default(),
            pdf_url: None,
        });
        let local_only = title_named_destinations(&tmp.path().join("local"), Arc::default());
        let job = job_id(&store).await;
        let req = edition_req(&cache, job.clone(), "B00SOME", "user-1");
        acquire_book(&store, &local_only, req, &source)
            .await
            .unwrap();

        let mut destinations = title_named_destinations(&tmp.path().join("local"), Arc::default());
        let mut second = title_named_destinations(&tmp.path().join("second"), Arc::default())
            .items
            .remove(0);
        second.id = DestinationId::new("second").unwrap();
        destinations.items.push(second);
        destinations.multi_destination = MultiDestinationMode::RefetchMissing;

        let req = edition_req(&cache, job, "B00SOME", "user-1");
        let err = acquire_book(&store, &destinations, req, &source)
            .await
            .expect_err("the throttled refetch is the job's to act on");
        assert!(
            matches!(
                err,
                AcquireError::Source(bookclerk_source::SourceError::RateLimited(_))
            ),
            "unexpected error: {err}"
        );
        let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
        assert_eq!(book.acquire_status, AcquireStatus::Acquired);
    }

    #[tokio::test]
    async fn an_acquire_fails_when_no_destination_stores_the_title() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00SOME", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let root = tmp.path().join("local");
        let destinations = title_named_destinations(&root, Arc::default());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::write(&root, b"not a folder").unwrap();
        let source = CountingSource {
            fetches: Arc::default(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00SOME", "user-1");
        assert!(acquire_book(&store, &destinations, req, &source)
            .await
            .is_err());
        assert!(copy_states(&store, &uuid).await.is_empty());
    }

    #[tokio::test]
    async fn a_book_of_an_acquired_edition_shares_its_files_without_a_fetch() {
        let store = test_store().await;
        // One edition bought on two accounts.
        let first_uuid = edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00SAME", "user-2", "Twin", None).await;
        let edition = store.edition_of_book(&first_uuid).await.unwrap();
        assert!(edition.is_some());
        assert_eq!(store.edition_of_book(&second_uuid).await.unwrap(), edition);
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&tmp.path().join("dest"), puts.clone());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;

        let req = edition_req(&cache, job.clone(), "B00SAME", "user-1");
        let first = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(!first.matched_existing);
        let req = edition_req(&cache, job.clone(), "B00SAME", "user-2");
        let second = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(second.matched_existing);
        assert_eq!(second.storage_key, first.storage_key);
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "the edition is fetched once"
        );
        assert_eq!(puts.load(Ordering::SeqCst), 1);
        let shared = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        assert_eq!(shared.acquire_status, AcquireStatus::Acquired);
        assert_eq!(
            shared.storage_key.as_deref(),
            Some(first.storage_key.as_str())
        );
        // The edition records the copy once, under the title that fetched it.
        let edition = edition.unwrap();
        let acquired = store.edition_acquisition(&edition).await.unwrap().unwrap();
        assert_eq!(acquired.status, AcquireStatus::Acquired);
        assert_eq!(
            acquired.storage_key.as_deref(),
            Some(first.storage_key.as_str())
        );
        assert_eq!(
            Some(acquired.holding_id.clone()),
            store
                .title_holding_of_book(&first_uuid)
                .await
                .unwrap()
                .map(|t| t.holding_id)
        );
        // A sibling title's failure leaves the edition acquired; the title
        // that fetched it can demote it.
        store
            .set_acquire_status(
                &second_uuid,
                "user-2",
                AcquireStatus::Error,
                None,
                Some("x"),
            )
            .await
            .unwrap();
        let kept = store.edition_acquisition(&edition).await.unwrap().unwrap();
        assert_eq!(kept, acquired);
        store
            .set_acquire_status(
                &first_uuid,
                "user-1",
                AcquireStatus::NotAcquired,
                None,
                None,
            )
            .await
            .unwrap();
        let demoted = store.edition_acquisition(&edition).await.unwrap().unwrap();
        assert_eq!(demoted.status, AcquireStatus::NotAcquired);
        assert_eq!(demoted.storage_key, None);
    }

    #[tokio::test]
    async fn another_edition_of_a_title_gets_its_own_folder() {
        let store = test_store().await;
        // Two editions (two products) of one title.
        let first_uuid = edition_book(&store, "B00ONE", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00TWO", "user-1", "Twin", Some("Ada")).await;
        assert_ne!(
            store.edition_of_book(&first_uuid).await.unwrap(),
            store.edition_of_book(&second_uuid).await.unwrap()
        );
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&tmp.path().join("dest"), puts.clone());
        let source = CountingSource {
            fetches: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        for (product, uuid) in [("B00ONE", &first_uuid), ("B00TWO", &second_uuid)] {
            let req = edition_req(&cache, job.clone(), product, "user-1");
            let acquired = acquire_book(&store, &destinations, req, &source)
                .await
                .unwrap();
            assert!(!acquired.matched_existing, "{product}");
            let short = short_id(&store, uuid).await;
            assert!(
                acquired
                    .storage_key
                    .starts_with(&format!("Twin [{short}]/Twin")),
                "{}",
                acquired.storage_key
            );
            let owner = store.file_owner_of_book(uuid).await.unwrap().unwrap();
            let recorded = store
                .edition_file("local", &acquired.storage_key)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(recorded.owner, owner, "{product}");
            assert_eq!(recorded.role, bookclerk_library::FileRole::Audio);
        }
        assert_eq!(
            puts.load(Ordering::SeqCst),
            2,
            "no edition overwrote another"
        );
    }

    #[test]
    fn a_copy_the_worker_cannot_open_is_used_only_as_delivered() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = dummy_req(tmp.path(), None, None);
        req.options.format = bookclerk_config::OutputFormat::EnrichedM4b;
        let probe = |content_type: &str| FileProbe {
            content_type: content_type.into(),
            bitrate_kbps: Some(128),
            lossless: false,
            source_store: None,
        };
        assert!(output_can_use(&req, &probe("audio/ogg")));
        assert!(!output_can_use(&req, &probe("audio/x-matroska")));
        req.options.format = bookclerk_config::OutputFormat::None;
        assert!(output_can_use(&req, &probe("audio/x-matroska")));
    }

    #[tokio::test]
    async fn two_parts_with_one_title_get_two_names() {
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut req = dummy_req(tmp.path(), None, None);
        req.call_number = Some("hd4j63pc".into());
        let part = |n: &str| PreparedAudioFile {
            path: tmp.path().join(n),
            title: "Introduction".into(),
            ext: "mp3".into(),
        };
        let contexts = naming_contexts(&store, &req).await;
        let first = planned_plain_part_storage_key(&req, &contexts, 0, &part("a.mp3")).await;
        let second = planned_plain_part_storage_key(&req, &contexts, 1, &part("b.mp3")).await;
        assert_ne!(first, second, "parts with one title share {first}");
        assert!(first.contains("hd4j63pc-p001"), "{first}");
        assert!(second.contains("hd4j63pc-p002"), "{second}");
    }

    #[tokio::test]
    async fn another_editions_file_at_the_planned_name_is_not_adopted() {
        let store = test_store().await;
        edition_book(&store, "B00ONE", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00TWO", "user-1", "Twin", Some("Ada")).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&tmp.path().join("dest"), puts.clone());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        // Two editions whose short ids collide plan one name.
        let mut first = edition_req(&cache, job.clone(), "B00ONE", "user-1");
        first.call_number = Some("samesame".into());
        acquire_book(&store, &destinations, first, &source)
            .await
            .unwrap();
        let mut second = edition_req(&cache, job, "B00TWO", "user-1");
        second.call_number = Some("samesame".into());
        let refused = acquire_book(&store, &destinations, second, &source).await;
        assert!(
            refused.is_err(),
            "the second edition adopted the first's file"
        );
        let book = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        assert_ne!(book.acquire_status, AcquireStatus::Acquired);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_padded_product_id_still_finds_its_edition() {
        let store = test_store().await;
        let uuid = edition_book(&store, " B00PAD ", "user-1", "Padded", None).await;
        assert!(
            store.file_owner_of_book(&uuid).await.unwrap().is_some(),
            "the title row's raw product id finds the trimmed listing"
        );
        let edition = store.edition_of_book(&uuid).await.unwrap().unwrap();
        assert!(
            store
                .books_of_edition(&edition)
                .await
                .unwrap()
                .contains(&uuid),
            "the edition finds the padded row back"
        );
    }

    #[tokio::test]
    async fn a_book_without_an_edition_acquires_nothing() {
        let store = test_store().await;
        // A title over the resolver's 4 KiB text limit is stored but never gets
        // an edition.
        let long_title = "T".repeat(8 * 1024);
        let uuid = edition_book(&store, "B00LONG", "user-1", &long_title, None).await;
        assert!(store.file_owner_of_book(&uuid).await.unwrap().is_none());
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&tmp.path().join("dest"), puts.clone());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00LONG", "user-1");
        let refused = acquire_book(&store, &destinations, req, &source).await;
        assert!(refused.is_err(), "an unresolved title must not acquire");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn templates_without_the_call_number_acquire_nothing() {
        let store = test_store().await;
        edition_book(&store, "B00ONE", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations = counting_destinations(&tmp.path().join("dest"), puts.clone());
        destinations.items[0].options.folder_template = Some("<title>".into());
        destinations.items[0].options.file_template = Some("<title>".into());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00ONE", "user-1");
        let err = acquire_book(&store, &destinations, req.clone(), &source)
            .await
            .expect_err("a template that can share a path is refused");
        assert!(err.to_string().contains("<call number>"), "{err}");
        assert!(err.to_string().contains("local"), "{err}");
        // The companion PDF alone is refused the same way.
        let err = acquire_pdf_only(&store, &destinations, &req, &source)
            .await
            .expect_err("a PDF-only acquire with such a template is refused");
        assert!(err.to_string().contains("<call number>"), "{err}");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_name_another_edition_holds_is_never_overwritten() {
        let store = test_store().await;
        edition_book(&store, "B00ONE", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00TWO", "user-1", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&tmp.path().join("dest"), puts.clone());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        // Another edition's file at the first edition's planned name (two
        // edition ids that share their short id).
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00ONE", "user-1");
        let mut planned_req = req.clone();
        planned_req.options = destinations.primary_destination().options.clone();
        let planned = planned_storage_key(&store, &planned_req).await;
        let other = store
            .file_owner_of_book(&second_uuid)
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .record_edition_file(
                "local",
                &planned,
                &other,
                bookclerk_library::FileRole::Audio,
            )
            .await
            .unwrap());
        let err = acquire_book(&store, &destinations, req, &source)
            .await
            .expect_err("another edition's name is refused");
        assert!(err.to_string().contains("another edition"), "{err}");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_sibling_whose_file_is_gone_is_fetched_again() {
        let store = test_store().await;
        edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        edition_book(&store, "B00SAME", "user-2", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let dest = tmp.path().join("dest");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&dest, puts.clone());
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job.clone(), "B00SAME", "user-1");
        let first = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        tokio::fs::remove_file(dest.join(&first.storage_key))
            .await
            .unwrap();
        let req = edition_req(&cache, job, "B00SAME", "user-2");
        let second = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(!second.matched_existing, "a missing file is not shared");
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert!(dest.join(&second.storage_key).exists());
    }

    #[tokio::test]
    async fn a_sibling_missing_a_part_is_not_shared() {
        let store = test_store().await;
        let first_uuid = edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00SAME", "user-2", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let dest = tmp.path().join("dest");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&dest, puts);
        let source = CountingSource {
            fetches: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00SAME", "user-1");
        let first = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        let backends: Vec<(&str, &dyn StorageBackend)> = destinations
            .items
            .iter()
            .map(|item| (item.id.as_str(), item.backend.as_ref()))
            .collect();
        let second = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        // The edition also recorded a part file, since removed.
        let owner = store
            .file_owner_of_book(&first_uuid)
            .await
            .unwrap()
            .unwrap();
        let folder = first.storage_key.rsplit_once('/').unwrap().0;
        assert!(store
            .record_edition_file(
                "local",
                &format!("{folder}/Twin — Part 2.m4b"),
                &owner,
                bookclerk_library::FileRole::Part,
            )
            .await
            .unwrap());
        assert_eq!(
            share_acquired_sibling(&store, &backends, &view_of(&store, &second).await)
                .await
                .unwrap(),
            None,
            "a copy missing a part is not shared"
        );
    }

    #[tokio::test]
    async fn a_sibling_named_differently_per_destination_is_shared() {
        let store = test_store().await;
        edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00SAME", "user-2", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let local = tmp.path().join("local");
        let archive = tmp.path().join("archive");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations = title_named_destinations(&local, puts.clone());
        let mut second_destination = title_named_destinations(&archive, puts).items.remove(0);
        second_destination.id = DestinationId::new("s3").unwrap();
        second_destination.options.folder_template = Some("Archive/<title> [<call number>]".into());
        destinations.items.push(second_destination);
        let source = CountingSource {
            fetches: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00SAME", "user-1");
        let first = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(
            !archive.join(&first.storage_key).exists(),
            "the archive names the file differently"
        );
        let backends: Vec<(&str, &dyn StorageBackend)> = destinations
            .items
            .iter()
            .map(|item| (item.id.as_str(), item.backend.as_ref()))
            .collect();
        let second = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        assert_eq!(
            share_acquired_sibling(&store, &backends, &view_of(&store, &second).await)
                .await
                .unwrap(),
            Some(first.storage_key),
            "each destination is judged by its own recorded keys"
        );
    }

    #[tokio::test]
    async fn a_sibling_pdf_that_is_gone_is_not_shared() {
        let store = test_store().await;
        let first_uuid = edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00SAME", "user-2", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let dest = tmp.path().join("dest");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&dest, puts);
        let source = CountingSource {
            fetches: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00SAME", "user-1");
        let first = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        // The sibling's row says its PDF is stored, but the file is gone.
        store
            .set_pdf_status(
                &first_uuid,
                "user-1",
                AcquireStatus::Acquired,
                Some("Twin/gone.pdf"),
            )
            .await
            .unwrap();
        let backends: Vec<(&str, &dyn StorageBackend)> = destinations
            .items
            .iter()
            .map(|item| (item.id.as_str(), item.backend.as_ref()))
            .collect();
        let second = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        assert_eq!(
            share_acquired_sibling(&store, &backends, &view_of(&store, &second).await)
                .await
                .unwrap(),
            Some(first.storage_key),
            "the audio is still shared"
        );
        let second = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        assert_eq!(second.acquire_status, AcquireStatus::Acquired);
        assert_ne!(
            second.pdf_status,
            AcquireStatus::Acquired,
            "a missing PDF is not recorded as acquired"
        );
    }

    #[tokio::test]
    async fn an_editions_stored_pdf_is_used_by_another_title() {
        let store = test_store().await;
        let first_uuid = edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00SAME", "user-2", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let dest = tmp.path().join("dest");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let destinations = title_named_destinations(&dest, puts);
        let job = job_id(&store).await;
        let first_req = edition_req(&cache, job.clone(), "B00SAME", "user-1");
        let second_req = edition_req(&cache, job, "B00SAME", "user-2");

        // Nothing stored yet.
        assert_eq!(
            stored_edition_pdf(&store, &destinations, &second_req)
                .await
                .unwrap(),
            None
        );
        // The first title's PDF is recorded but missing: not used.
        store
            .set_pdf_status(
                &first_uuid,
                "user-1",
                AcquireStatus::Acquired,
                Some("Twin/twin.pdf"),
            )
            .await
            .unwrap();
        assert_eq!(
            stored_edition_pdf(&store, &destinations, &second_req)
                .await
                .unwrap(),
            None
        );
        // Its own title uses it without a probe; another title once it is
        // on the primary destination, and records it.
        assert_eq!(
            stored_edition_pdf(&store, &destinations, &first_req)
                .await
                .unwrap()
                .as_deref(),
            Some("Twin/twin.pdf")
        );
        destinations
            .primary_destination()
            .backend
            .put(
                "Twin/twin.pdf",
                bytes::Bytes::from_static(b"%PDF-1.4"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            stored_edition_pdf(&store, &destinations, &second_req)
                .await
                .unwrap()
                .as_deref(),
            Some("Twin/twin.pdf")
        );
        let second = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        assert_eq!(second.pdf_status, AcquireStatus::Acquired);
        assert_eq!(second.pdf_storage_key.as_deref(), Some("Twin/twin.pdf"));
    }

    #[tokio::test]
    async fn oversized_companion_pdf_soft_fails_and_retries_on_existing_audio() {
        let store = test_store().await;
        store
            .upsert_account("audible", "user-1", "us", None, true, "audible")
            .await
            .unwrap();
        let book = store
            .upsert_book(&bookclerk_library::NewBook::minimal(
                "B00PDFQ",
                "user-1",
                "us",
                "Pdf Quota",
            ))
            .await
            .unwrap();
        let id = job_id(&store).await;
        let tmp = tempfile::tempdir().unwrap();
        let dest_root = tmp.path().join("dest");
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pdf_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url = serve_http_body_loop(vec![0u8; 64], pdf_hits.clone()).await;

        let mut req = dummy_req(&cache, Some(id), Some(16));
        req.asin = "B00PDFQ".into();
        req.holding_id = Some(holding_of(&store, &book.uuid).await);
        req.title = "Pdf Quota".into();
        let destinations = counting_destinations_with(&dest_root, puts.clone(), true);
        let source = CountingSource {
            fetches: fetches.clone(),
            pdf_url: Some(url),
        };
        let first = acquire_book(&store, &destinations, req.clone(), &source)
            .await
            .expect("audio acquire must succeed when companion PDF exceeds quota");
        assert!(!first.matched_existing);
        assert_eq!(puts.load(Ordering::SeqCst), 1);
        assert_eq!(pdf_hits.load(Ordering::SeqCst), 1);
        let after_first = store
            .get_book("B00PDFQ", "user-1")
            .await
            .unwrap()
            .expect("book after first acquire");
        assert_eq!(after_first.acquire_status, AcquireStatus::Acquired);
        assert_eq!(after_first.pdf_status, AcquireStatus::Error);

        let second = acquire_book(&store, &destinations, req, &source)
            .await
            .expect("retry must keep acquired audio and resume the companion PDF");
        assert!(second.matched_existing);
        assert_eq!(
            second.store_fetches, 1,
            "the resumed PDF went to the store, so it counts toward the pace"
        );
        assert_eq!(puts.load(Ordering::SeqCst), 1);
        assert_eq!(pdf_hits.load(Ordering::SeqCst), 2);
        let after_retry = store
            .get_book("B00PDFQ", "user-1")
            .await
            .unwrap()
            .expect("book after retry");
        assert_eq!(after_retry.acquire_status, AcquireStatus::Acquired);
        assert_eq!(after_retry.pdf_status, AcquireStatus::Error);
    }

    /// A store that answers its first fetch as [`CountingSource`] does
    /// and asks the account to slow down on every later one.
    struct ThrottlesAfterFirst(CountingSource);

    #[async_trait::async_trait]
    impl ContentSource for ThrottlesAfterFirst {
        fn id(&self) -> &str {
            self.0.id()
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            self.0.portal_auth_mode()
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            self.0.portal_brand()
        }

        async fn login(
            &self,
            scope: &bookclerk_library::SourceScope,
            opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            self.0.login(scope, opts).await
        }

        async fn list_accounts(
            &self,
            scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            self.0.list_accounts(scope).await
        }

        async fn scan(
            &self,
            scope: &bookclerk_library::SourceScope,
            opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            self.0.scan(scope, opts).await
        }

        async fn fetch_title(
            &self,
            scope: &bookclerk_library::SourceScope,
            account_id: &str,
            title_id: &str,
            opts: &FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            if self.0.fetches.load(Ordering::SeqCst) > 0 {
                return Err(bookclerk_source::SourceError::RateLimited(None));
            }
            self.0.fetch_title(scope, account_id, title_id, opts).await
        }
    }

    /// A store that refuses the resumed companion PDF for downloading too
    /// much hands that refusal to the job, so the account pauses, while
    /// the acquired audio stays acquired.
    #[tokio::test]
    async fn a_throttled_companion_pdf_resume_reaches_the_job() {
        let store = test_store().await;
        store
            .upsert_account("audible", "user-1", "us", None, true, "audible")
            .await
            .unwrap();
        let book = store
            .upsert_book(&bookclerk_library::NewBook::minimal(
                "B00PDFT",
                "user-1",
                "us",
                "Pdf Throttle",
            ))
            .await
            .unwrap();
        let id = job_id(&store).await;
        let tmp = tempfile::tempdir().unwrap();
        let dest_root = tmp.path().join("dest");
        let cache = tmp.path().join("cache");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pdf_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url = serve_http_body_loop(vec![0u8; 64], pdf_hits.clone()).await;

        let mut req = dummy_req(&cache, Some(id), Some(16));
        req.asin = "B00PDFT".into();
        req.holding_id = Some(holding_of(&store, &book.uuid).await);
        req.title = "Pdf Throttle".into();
        let destinations = counting_destinations_with(&dest_root, puts.clone(), true);
        let source = ThrottlesAfterFirst(CountingSource {
            fetches: fetches.clone(),
            pdf_url: Some(url),
        });
        acquire_book(&store, &destinations, req.clone(), &source)
            .await
            .expect("audio acquire succeeds when the companion PDF is too large");

        let err = acquire_book(&store, &destinations, req, &source)
            .await
            .expect_err("the throttled PDF resume is the job's to act on");
        assert!(
            matches!(
                err,
                AcquireError::Source(bookclerk_source::SourceError::RateLimited(_))
            ),
            "unexpected error: {err}"
        );
        let after = store
            .get_book("B00PDFT", "user-1")
            .await
            .unwrap()
            .expect("book after retry");
        assert_eq!(after.acquire_status, AcquireStatus::Acquired);
        assert_eq!(after.pdf_status, AcquireStatus::Error);
        assert_eq!(puts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn acquire_pdf_only_unregisters_scratch_after_fetch_quota_failure() {
        let store = test_store().await;
        let id = job_id(&store).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let dest_root = tmp.path().join("dest");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut req = dummy_req(&cache, Some(id.clone()), Some(20));
        req.options.download_pdf = true;
        let destinations = counting_destinations(&dest_root, puts);
        let err = acquire_pdf_only(
            &store,
            &destinations,
            &req,
            &QuotaBurstSource {
                chunk: 16,
                chunks: 4,
            },
        )
        .await
        .expect_err("PDF-only fetch must fail when scratch quota is exceeded");
        assert!(err.to_string().contains("quota"), "unexpected error: {err}");
        assert!(
            store.list_job_temp_paths(&id).await.unwrap().is_empty(),
            "failed PDF job must unregister acquire-pdf scratch"
        );
        assert!(!cache.join("acquire-pdf").join("B00TEST").exists());
    }

    /// A request for `title` by "Ann Author" whose destination names
    /// files with `folder`, `<title> [<asin>]` and a chapter template
    /// carrying the title, number and chapter title.
    fn upload_naming_req(cache: &Path, asin: &str, title: &str, folder: &str) -> AcquireRequest {
        let mut req = dummy_req(cache, None, None);
        req.asin = asin.into();
        req.title = title.into();
        req.authors = Some("Ann Author".into());
        req.options.folder_template = Some(folder.into());
        req.options.file_template = Some("<title> [<asin>]".into());
        req.options.chapter_file_template = Some("<title> - <ch#> - <chapter title>".into());
        req
    }

    /// The upload scope's planned names: a folder several titles share
    /// (`<first author>`) and the root are not the title's own, and name
    /// its audio stem plus part and chapter patterns; a folder with the
    /// title in it is the title's own.
    #[tokio::test]
    async fn upload_planned_title_names_follow_the_folder_template() {
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let shared = upload_naming_req(tmp.path(), "B00ONE", "One", "<first author>");
        let names = planned_title_names(&store, &shared).await;
        assert_eq!(
            names,
            PlannedTitleNames {
                folders: vec!["Ann Author".into()],
                own_folder: false,
                audio_stems: vec!["Ann Author/One [B00ONE]".into()],
                part_patterns: vec![
                    format!("Ann Author/One — {NAME_WILDCARD} [B00ONE-p{NUMBER_WILDCARD}]"),
                    format!("Ann Author/One - {NUMBER_WILDCARD} - {NAME_WILDCARD}"),
                ],
            }
        );
        let root = upload_naming_req(tmp.path(), "B00ONE", "One", "");
        let names = planned_title_names(&store, &root).await;
        assert_eq!(names.folders, vec![String::new()]);
        assert!(!names.own_folder, "the root is shared by every title");
        assert_eq!(names.audio_stems, vec!["One [B00ONE]".to_string()]);
        assert_eq!(
            names.part_patterns,
            vec![
                format!("One — {NAME_WILDCARD} [B00ONE-p{NUMBER_WILDCARD}]"),
                format!("One - {NUMBER_WILDCARD} - {NAME_WILDCARD}"),
            ]
        );
        let own = upload_naming_req(tmp.path(), "B00ONE", "One", "<first author>/<title>");
        let names = planned_title_names(&store, &own).await;
        assert_eq!(names.folders, vec!["Ann Author/One".to_string()]);
        assert!(names.own_folder, "a folder named for the title is its own");
        // So is one named only for the edition.
        let mut by_id = upload_naming_req(tmp.path(), "B00ONE", "One", "<call number>");
        by_id.call_number = Some("hd4j63pc".into());
        let names = planned_title_names(&store, &by_id).await;
        assert_eq!(names.folders, vec!["hd4j63pc".to_string()]);
        assert!(
            names.own_folder,
            "a folder named for the edition is its own"
        );
    }

    /// Destination double that stores each `:` of a host key as `_`, like
    /// a store whose keys cannot hold colons.
    #[derive(Clone)]
    struct ColonlessBackend {
        /// Backend the double stores into.
        inner: bookclerk_storage::LocalFsBackend,
    }

    #[async_trait::async_trait]
    impl StorageBackend for ColonlessBackend {
        fn name(&self) -> &'static str {
            "colonless"
        }

        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(self.clone())
        }

        async fn put(
            &self,
            key: &str,
            data: bytes::Bytes,
            meta: ObjectMeta,
        ) -> bookclerk_storage::Result<()> {
            self.inner.put(key, data, meta).await
        }

        async fn put_file(
            &self,
            key: &str,
            path: &Path,
            meta: ObjectMeta,
        ) -> bookclerk_storage::Result<()> {
            self.inner.put_file(key, path, meta).await
        }

        async fn get(&self, key: &str) -> bookclerk_storage::Result<bytes::Bytes> {
            self.inner.get(key).await
        }

        async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
            self.inner.exists(key).await
        }

        async fn list(
            &self,
            prefix: &str,
        ) -> bookclerk_storage::Result<Vec<bookclerk_storage::ObjectInfo>> {
            self.inner.list(prefix).await
        }

        async fn probe(
            &self,
            key: &str,
        ) -> bookclerk_storage::Result<bookclerk_storage::ObjectProbe> {
            self.inner.probe(key).await
        }

        async fn copy(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
            self.inner.copy(from, to).await
        }

        async fn delete(&self, key: &str) -> bookclerk_storage::Result<()> {
            self.inner.delete(key).await
        }

        async fn list_page(
            &self,
            prefix: &str,
            cursor: Option<&str>,
            limit: u32,
        ) -> bookclerk_storage::Result<bookclerk_storage::ListPage> {
            self.inner.list_page(prefix, cursor, limit).await
        }

        async fn get_stream(
            &self,
            key: &str,
            range: Option<bookclerk_storage::ByteRange>,
        ) -> bookclerk_storage::Result<(
            bookclerk_storage::ObjectProbe,
            std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        )> {
            self.inner.get_stream(key, range).await
        }

        async fn put_stream(
            &self,
            key: &str,
            body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            meta: ObjectMeta,
        ) -> bookclerk_storage::Result<bookclerk_storage::PutStreamResult> {
            self.inner.put_stream(key, body, meta).await
        }

        async fn translate_keys(
            &self,
            keys: &[bookclerk_storage::key_syntax::HostKey],
        ) -> bookclerk_storage::Result<Vec<String>> {
            Ok(keys
                .iter()
                .map(|key| key.as_str().replace(':', "_"))
                .collect())
        }
    }

    /// Planned keys and upload names are in the keys the destination
    /// stores: the template renders a host key and the destination
    /// translates it.
    #[tokio::test]
    async fn planned_names_are_the_keys_the_destination_stores() {
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut req = upload_naming_req(tmp.path(), "B00ONE", "Dune: Messiah", "<first author>");
        let host_key = planned_storage_key_for(&store, &req, "m4b").await;
        assert_eq!(host_key, "Ann Author/Dune: Messiah [B00ONE].m4b");
        req.destination_keys = crate::keys::DestinationKeys::of(&ColonlessBackend {
            inner: bookclerk_storage::LocalFsBackend::new(tmp.path().to_path_buf()).unwrap(),
        });
        assert_eq!(
            planned_storage_key_for(&store, &req, "m4b").await,
            "Ann Author/Dune_ Messiah [B00ONE].m4b"
        );
        let names = planned_title_names(&store, &req).await;
        assert_eq!(
            names.audio_stems,
            vec!["Ann Author/Dune_ Messiah [B00ONE]".to_string()]
        );
        assert!(
            names
                .part_patterns
                .iter()
                .all(|pattern| pattern.starts_with("Ann Author/Dune_ Messiah")),
            "{:?}",
            names.part_patterns
        );
    }

    /// A folder that uses only part of the title (`<title short>`, a
    /// leading slice) is shared by every title that part matches, so it
    /// is not the title's own.
    #[tokio::test]
    async fn a_folder_from_part_of_the_title_is_not_the_titles_own() {
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        for folder in ["<first author>/<title short>", "<first author>/<title[3]>"] {
            let req = upload_naming_req(tmp.path(), "B00TWO", "Dune: Messiah", folder);
            let names = planned_title_names(&store, &req).await;
            assert!(!names.own_folder, "{folder} is shared with other titles");
        }
    }

    /// A template that changes the case of a part or chapter title still
    /// yields a pattern with its wildcards, not a literal no file matches.
    #[tokio::test]
    async fn part_patterns_survive_a_templates_case_flags() {
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut req = upload_naming_req(tmp.path(), "B00ONE", "One", "<first author>");
        req.options.chapter_file_template = Some("<title> - <ch#> - <chapter title[u]>".into());
        let names = planned_title_names(&store, &req).await;
        assert!(
            names.part_patterns.contains(&format!(
                "Ann Author/One - {NUMBER_WILDCARD} - {NAME_WILDCARD}"
            )),
            "{:?}",
            names.part_patterns
        );
    }
}
