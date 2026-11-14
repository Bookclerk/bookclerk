//! Acquire pipeline: fetch (plain) → package / metadata → storage.
//!
//! DRM decrypt happens inside content-source plugins; this crate never sees keys.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bookclerk_config::{FileTimestampMode, MultiDestinationMode, OutputBackendKind};
use bookclerk_enrich::{asin_identifier, chapter_info_from_layout, MetadataProviders};
use bookclerk_library::{AcquireStatus, BookRecord, FileOwner, LibraryStore};
use bookclerk_media::{
    align_chapter_starts_async, bookclerk_tool_tag, encode_to_mp3, fixup_audiobook,
    package_m4b_from_mp3, parse_mp4, track_duration_ms, ChapterAlignOptions, FixupRequest,
    PackageM4bRequest,
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
use crate::destinations::{destination_id, AcquireDestination, AcquireDestinations};
use crate::error::{AcquireError, Result};
use crate::naming::{
    audio_basename, chapter_storage_key_with_folder, sidecar_key, storage_key_with_contexts,
    NamingContext,
};
use crate::reconcile::{find_existing_for_request, StorageIndex};
use crate::record::recording_destinations;
use crate::split::split_audio_by_chapters;

/// Request to acquire a single title.
#[derive(Debug, Clone)]
pub struct AcquireRequest {
    /// Download product id (Audible ASIN / Libro ISBN).
    pub asin: String,
    /// Stable library UUID when known (preferred for status updates).
    pub book_uuid: Option<String>,
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
    /// When set, only write prepared audio to these destination kinds
    /// (`output.multi_destination = refetch_missing`).
    pub write_destinations: Option<Vec<OutputBackendKind>>,
    /// Durable daemon job id that owns this acquire's scratch directory.
    pub job_id: Option<String>,
    /// Refuse a new scratch dir when registered temps already exceed this.
    pub temp_quota_bytes: Option<u64>,
    /// The `<library id>` to name the title by instead of its edition in
    /// `library`: a host acquiring for an owner node names the title as the
    /// owner does, since its scratch library's edition ids are its own.
    pub library_id: Option<String>,
    /// Likewise for the title's podcast parent, whose folder an episode
    /// may be saved in.
    pub parent_library_id: Option<String>,
    /// Metadata providers asked for the plain-audio catalog and chapter
    /// overlay. The default has none, so plain audio keeps its store tags.
    pub metadata: AcquireMetadata,
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
    let owner = file_owner_of_request(library, &req).await?;
    pin_library_id(&mut req, owner.as_ref());
    let recorded = recording_destinations(library, destinations, owner.as_ref());
    acquire_book_recorded(library, &recorded, req, index, source).await
}

/// Names the whole acquire with the library id of the edition `owner`
/// read once at its start, so no later naming step re-reads the edition
/// and, on a failed read, names a file differently from the others.
/// A library id the request already carries (an owner's, for a host) is
/// kept.
fn pin_library_id(req: &mut AcquireRequest, owner: Option<&FileOwner>) {
    if req.library_id.is_some() {
        return;
    }
    match owner {
        Some(FileOwner::Edition(edition)) => {
            req.library_id = bookclerk_library::library_id(edition)
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
    mut req: AcquireRequest,
    mut index: Option<&mut StorageIndex>,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    req.options = destinations.primary_destination().options.clone();
    tracing::info!(
        asin = %req.asin,
        source = %req.source,
        title = %req.title,
        force = req.force,
        output = ?req.options.effective_output(),
        "acquire requested"
    );

    if req.options.wants_opus() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "output=opus is not implemented yet; use enriched_m4b, single_mp3, \
             split_mp3_by_chapter, or none"
        )));
    }
    if req.options.wants_split_by_size() {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "output=split_mp3_by_size is not implemented yet (split_mp3_max_mb={}); \
             use split_mp3_by_chapter or single_mp3",
            req.options.split_mp3_max_mb
        )));
    }

    if !req.force && !req.options.overwrite_existing {
        if let Some(book) = resolve_book(library, &req).await {
            if book.acquire_status == AcquireStatus::Acquired {
                let primary_key = book.storage_key.clone().unwrap_or_default();
                tracing::info!(
                    asin = %req.asin,
                    key = %primary_key,
                    "skipping download — title already acquired"
                );
                resume_missing_companion_pdf(library, destinations, &req, source).await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: Vec::new(),
                    matched_existing: true,
                });
            }
            let backends: Vec<(&str, &dyn StorageBackend)> = destinations
                .items
                .iter()
                .map(|item| (destination_id(item.kind), item.backend.as_ref()))
                .collect();
            if let Some(primary_key) = share_acquired_sibling(library, &backends, &book).await? {
                tracing::info!(
                    asin = %req.asin,
                    key = %primary_key,
                    "skipping download — another book of this edition is already acquired"
                );
                resume_missing_companion_pdf(library, destinations, &req, source).await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: Vec::new(),
                    matched_existing: true,
                });
            }
        }
        // Before accepting media already at the planned name: a file there
        // may be another edition's (two editions whose short ids collide),
        // and adopting it would point this edition at that recording.
        check_edition_paths(library, destinations, &req).await?;
        match plan_existing_destinations(library, destinations, &req, index.as_deref()).await? {
            ExistingPlan::Skip { primary_key } => {
                tracing::info!(
                    asin = %req.asin,
                    key = %primary_key,
                    "skipping download — matched existing acquired media"
                );
                library
                    .set_acquire_status(
                        status_key(&req),
                        &req.account_id,
                        AcquireStatus::Acquired,
                        Some(&primary_key),
                        None,
                    )
                    .await?;
                resume_missing_companion_pdf(library, destinations, &req, source).await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: Vec::new(),
                    matched_existing: true,
                });
            }
            ExistingPlan::SyncMissing {
                primary_key,
                source_kind,
                source_key,
                missing,
            } => {
                tracing::info!(
                    asin = %req.asin,
                    from = ?source_kind,
                    missing = missing.len(),
                    "syncing existing media to missing destinations (no store fetch)"
                );
                let written = sync_missing_destinations(
                    destinations,
                    source_kind,
                    &source_key,
                    &missing,
                    &req.files_dir.join("stage-journal"),
                )
                .await?;
                if let Some(idx) = index.as_mut() {
                    for key in &written {
                        idx.insert_key(key.clone());
                    }
                }
                library
                    .set_acquire_status(
                        status_key(&req),
                        &req.account_id,
                        AcquireStatus::Acquired,
                        Some(&primary_key),
                        None,
                    )
                    .await?;
                resume_missing_companion_pdf(library, destinations, &req, source).await?;
                return Ok(AcquireResult {
                    asin: req.asin,
                    storage_key: primary_key,
                    written_keys: written,
                    matched_existing: true,
                });
            }
            ExistingPlan::Fetch {
                only_kinds: Some(kinds),
            } => {
                tracing::info!(
                    asin = %req.asin,
                    destinations = ?kinds,
                    "re-fetching into missing destinations only"
                );
                req.write_destinations = Some(kinds);
            }
            ExistingPlan::Fetch { only_kinds: None } => {
                // Full acquire into every destination (refetch_all, or nothing present).
            }
        }
    }

    if req.force || req.options.overwrite_existing {
        // The existing-media path above has already checked.
        check_edition_paths(library, destinations, &req).await?;
    }

    library
        .set_acquire_status(
            status_key(&req),
            &req.account_id,
            AcquireStatus::Queued,
            None,
            None,
        )
        .await?;

    let result = match run_pipeline(library, destinations, &req, source).await {
        Ok(result) => {
            if let Some(idx) = index.as_mut() {
                idx.insert_key(result.storage_key.clone());
                for key in &result.written_keys {
                    idx.insert_key(key.clone());
                }
            }
            library
                .set_acquire_status(
                    status_key(&req),
                    &req.account_id,
                    AcquireStatus::Acquired,
                    Some(&result.storage_key),
                    None,
                )
                .await?;
            Ok(result)
        }
        Err(err) => {
            let message = err.to_string();
            let _ = library
                .set_acquire_status(
                    status_key(&req),
                    &req.account_id,
                    AcquireStatus::Error,
                    None,
                    Some(&message),
                )
                .await;
            Err(err)
        }
    };
    let work_dir = req.cache_dir.join("acquire").join(status_key(&req));
    cleanup_work_dir(library, req.job_id.as_deref(), &work_dir).await;
    result
}

/// Library status row key: book UUID when known, otherwise the store product id.
fn status_key(req: &AcquireRequest) -> &str {
    req.book_uuid.as_deref().unwrap_or(&req.asin)
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
    Ok(quota.saturating_sub(scratch_usage(&req.cache_dir).await))
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
    },
    /// Copy from a present destination into missing ones (no store fetch).
    SyncMissing {
        /// Storage key already present on the primary (or first present) destination.
        primary_key: String,
        /// Destination that already holds the title and will be copied from.
        source_kind: OutputBackendKind,
        /// Object key on `source_kind` to copy into missing destinations.
        source_key: String,
        /// Destinations that lack the title, paired with the planned target key.
        missing: Vec<(OutputBackendKind, String)>,
    },
    /// Run the full acquire pipeline (`only_kinds` limits writes when set).
    Fetch {
        /// When set, restrict the fetch write to these destination kinds; `None` writes all.
        only_kinds: Option<Vec<OutputBackendKind>>,
    },
}

/// Inspects each destination for an existing object and chooses skip, sync, or fetch.
async fn plan_existing_destinations(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    index: Option<&StorageIndex>,
) -> Result<ExistingPlan> {
    if destinations.len() == 1 {
        let dest = destinations.primary_destination();
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
                Some(primary_key) => ExistingPlan::Skip { primary_key },
                None => ExistingPlan::Fetch { only_kinds: None },
            },
        );
    }

    let mut present: Vec<(OutputBackendKind, String)> = Vec::new();
    let mut missing_kinds: Vec<OutputBackendKind> = Vec::new();
    let mut primary_key = None;

    for dest in &destinations.items {
        let dest_req = request_for_destination(req, dest);
        let dest_index = StorageIndex::from_storage(dest.backend.as_ref()).await?;
        if let Some(key) =
            find_existing_for_request(&dest_index, dest.backend.as_ref(), library, &dest_req)
                .await?
        {
            if dest.kind == destinations.primary {
                primary_key = Some(key.clone());
            }
            present.push((dest.kind, key));
        } else {
            missing_kinds.push(dest.kind);
        }
    }

    let ext = present
        .first()
        .and_then(|(_, k)| k.rsplit_once('.').map(|(_, e)| e))
        .unwrap_or("m4b");
    let mut missing: Vec<(OutputBackendKind, String)> = Vec::new();
    for kind in missing_kinds {
        let Some(dest) = destinations.destination(kind) else {
            continue;
        };
        let dest_req = request_for_destination(req, dest);
        missing.push((kind, planned_storage_key_for(library, &dest_req, ext).await));
    }

    if missing.is_empty() {
        let primary_key = primary_key
            .or_else(|| present.first().map(|(_, k)| k.clone()))
            .unwrap_or_default();
        return Ok(ExistingPlan::Skip { primary_key });
    }
    if present.is_empty() {
        return Ok(ExistingPlan::Fetch { only_kinds: None });
    }

    let primary_key = primary_key
        .clone()
        .or_else(|| present.first().map(|(_, k)| k.clone()))
        .unwrap_or_else(|| missing[0].1.clone());

    match destinations.multi_destination {
        MultiDestinationMode::SyncMissing => {
            let (source_kind, source_key) = present[0].clone();
            Ok(ExistingPlan::SyncMissing {
                primary_key,
                source_kind,
                source_key,
                missing,
            })
        }
        MultiDestinationMode::RefetchMissing => Ok(ExistingPlan::Fetch {
            only_kinds: Some(missing.into_iter().map(|(k, _)| k).collect()),
        }),
        MultiDestinationMode::RefetchAll => Ok(ExistingPlan::Fetch { only_kinds: None }),
    }
}

/// Retries per destination `put` before the acquire write is treated as failed.
const DEST_WRITE_ATTEMPTS: u32 = 3;

/// Copies an existing object from one destination into destinations that lack it.
async fn sync_missing_destinations(
    destinations: &AcquireDestinations,
    source_kind: OutputBackendKind,
    source_key: &str,
    missing: &[(OutputBackendKind, String)],
    stage_journal: &Path,
) -> Result<Vec<String>> {
    let source = destinations.destination(source_kind).ok_or_else(|| {
        AcquireError::Other(anyhow::anyhow!(
            "sync source destination {:?} is not configured",
            source_kind
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

    let mut written = Vec::new();
    for (kind, target_key) in missing {
        let dest = destinations.destination(*kind).ok_or_else(|| {
            AcquireError::Other(anyhow::anyhow!(
                "sync target destination {:?} is not configured",
                kind
            ))
        })?;
        bookclerk_storage::transfer_object(
            source.backend.as_ref(),
            source_key,
            dest.backend.as_ref(),
            target_key,
            meta.clone(),
            &bookclerk_storage::TransferOptions {
                max_attempts: DEST_WRITE_ATTEMPTS,
                stage_journal_dir: Some(stage_journal.to_path_buf()),
                ..bookclerk_storage::TransferOptions::default()
            },
        )
        .await
        .map_err(AcquireError::Storage)?;
        written.push(target_key.clone());
    }
    Ok(written)
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
    /// Destination backend that received the write (`local`, `s3`, …).
    kind: OutputBackendKind,
    /// Object key of the first file written to that destination.
    key: String,
}

#[derive(Debug, Clone)]
/// Keys written this acquire, plus the primary destination's first key for status.
struct StoredKeys {
    /// First key on the primary destination (or the first successful write).
    primary_key: String,
    /// Per-destination first keys used for sidecar writes and result reporting.
    keys: Vec<DestinationStoredKey>,
}

impl StoredKeys {
    /// First object key written on each destination, in destination order.
    fn all_keys(&self) -> Vec<String> {
        self.keys.iter().map(|stored| stored.key.clone()).collect()
    }
}

/// Clones the acquire request with this destination's naming and download options.
fn request_for_destination(
    req: &AcquireRequest,
    destination: &AcquireDestination,
) -> AcquireRequest {
    let mut dest_req = req.clone();
    dest_req.options = destination.options.clone();
    dest_req
}

/// Writes prepared audio to every enabled destination, retrying each destination independently.
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

    let mut primary_key = None;
    let mut keys = Vec::new();
    for dest in &destinations.items {
        if let Some(filter) = req.write_destinations.as_deref() {
            if !filter.is_empty() && !filter.contains(&dest.kind) {
                continue;
            }
        }
        let dest_req = request_for_destination(req, dest);
        let mut last_err = None;
        let mut stored = None;
        for attempt in 1..=DEST_WRITE_ATTEMPTS {
            match write_prepared_files_to_destination(library, dest, &dest_req, files, plan).await {
                Ok((first_key, written_keys)) => {
                    apply_storage_timestamps(
                        dest.backend.as_ref(),
                        library,
                        &dest_req,
                        &written_keys,
                    )
                    .await;
                    if dest.kind == destinations.primary {
                        primary_key = Some(first_key.clone());
                    }
                    stored = Some(DestinationStoredKey {
                        kind: dest.kind,
                        key: first_key,
                    });
                    last_err = None;
                    break;
                }
                Err(err) => {
                    tracing::warn!(
                        destination = ?dest.kind,
                        asin = %req.asin,
                        attempt,
                        %err,
                        "destination write failed; retrying"
                    );
                    last_err = Some(err);
                }
            }
        }
        if let Some(err) = last_err {
            // Retain successful writes on other destinations; fail this book.
            return Err(err);
        }
        if let Some(stored) = stored {
            keys.push(stored);
        }
    }

    let primary_key = primary_key
        .or_else(|| keys.first().map(|stored| stored.key.clone()))
        .unwrap_or_default();
    Ok(StoredKeys { primary_key, keys })
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
    for (idx, file) in files.iter().enumerate() {
        let key = planned_key_for_prepared_file(library, dest_req, file, idx, plan).await;
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

/// Storage key for one prepared file under the single, chapter, or part naming plan.
async fn planned_key_for_prepared_file(
    library: &LibraryStore,
    req: &AcquireRequest,
    file: &PreparedAudioFile,
    idx: usize,
    plan: AudioKeyPlan,
) -> String {
    match plan {
        AudioKeyPlan::Single => planned_storage_key_for(library, req, &file.ext).await,
        AudioKeyPlan::SplitChapters => planned_chapter_storage_key(library, req, idx, file).await,
        AudioKeyPlan::PlainParts => planned_plain_part_storage_key(library, req, idx, file).await,
    }
}

/// Chapter-file storage key using 1-based index and the chapter title.
async fn planned_chapter_storage_key(
    library: &LibraryStore,
    req: &AcquireRequest,
    idx: usize,
    file: &PreparedAudioFile,
) -> String {
    let templates = req.options.naming_templates();
    let (folder_ctx, file_ctx) = naming_contexts(library, req).await;
    chapter_storage_key_with_folder(
        &folder_ctx,
        &file_ctx,
        Some(templates.folder.as_str()),
        Some(templates.chapter_file.as_str()),
        &req.options.replacement_characters,
        idx + 1,
        &file.title,
        &file.ext,
        req.options.path_limits,
    )
}

/// Part-file storage key with a `-pNNN` product-id suffix so parts do not collide.
async fn planned_plain_part_storage_key(
    library: &LibraryStore,
    req: &AcquireRequest,
    idx: usize,
    file: &PreparedAudioFile,
) -> String {
    let (folder_ctx, file_ctx) = naming_contexts(library, req).await;
    let mut part_ctx = file_ctx;
    part_ctx.title = format!("{} — {}", req.title, file.title);
    part_ctx.asin = format!("{}-p{:03}", req.asin, idx + 1);
    // The default file templates name a title by `<library id>`, not
    // `<asin>`, so the part number goes on the id too: two parts with one
    // title must not share a name.
    part_ctx.library_id = part_ctx
        .library_id
        .map(|id| format!("{id}-p{:03}", idx + 1));
    part_ctx.chapter_number = Some(u32::try_from(idx + 1).unwrap_or(1));
    part_ctx.chapter_title = Some(file.title.clone());
    let templates = req.options.naming_templates();
    storage_key_with_contexts(
        &folder_ctx,
        &part_ctx,
        Some(templates.folder.as_str()),
        Some(templates.file.as_str()),
        &file.ext,
        &req.options.replacement_characters,
        req.options.path_limits,
    )
}

/// Marks the title as downloading, then fetches and stores it from the content source.
async fn run_pipeline(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    library
        .set_acquire_status(
            status_key(req),
            &req.account_id,
            AcquireStatus::Downloading,
            None,
            None,
        )
        .await?;

    run_source_pipeline(library, destinations, req, source).await
}

/// Fetches the title into the acquire cache and stores the resulting plain audio.
async fn run_source_pipeline(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<AcquireResult> {
    let work_dir = req.cache_dir.join("acquire").join(status_key(req));
    prepare_work_dir(library, req, &work_dir).await?;

    let fetch = fetch_title_enforcing_quota(library, req, source, &work_dir).await?;
    enforce_work_dir_quota(library, req, &work_dir).await?;

    let result = run_stage_enforcing_quota(library, req, &work_dir, async {
        store_plain_fetch(library, destinations, req, &work_dir, fetch).await
    })
    .await;
    if result.is_ok() {
        enforce_work_dir_quota(library, req, &work_dir).await?;
    }
    result
}

/// Packages or stores a source `PlainFetch` according to output options and chapter overlay.
async fn store_plain_fetch(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    work_dir: &Path,
    plain: PlainFetch,
) -> Result<AcquireResult> {
    let want_mp3 = req.options.wants_mp3();
    let multi = plain.parts.len() > 1;
    let audible_overlay_possible = plain_source_has_audible_asin(library, req).await;

    // No-op output: keep store-delivered bytes (no remux/transcode).
    if req.options.is_noop_output() {
        if multi {
            return store_plain_parts(library, destinations, req, work_dir, plain).await;
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
        )
        .await;
    }

    // Multi-part "split by chapter" without enrichment: store parts as-is.
    // When an Audible ASIN enrichment is available, package first so we can embed /
    // split by the literary chapter tree instead of track-boundary placeholders.
    if multi && req.options.wants_split_by_chapter() && !audible_overlay_possible {
        return store_plain_parts(library, destinations, req, work_dir, plain).await;
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
    } else if plain.parts.len() == 1 && want_mp3 && !audible_overlay_possible {
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
            let audible_asin = resolve_book(library, req)
                .await
                .and_then(|b| b.asin.clone());
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
        if !ext.eq_ignore_ascii_case("mp3") {
            let mp3_out = work_dir.join(format!("{}.mp3", status_key(req)));
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
            if let Some(dest) = destinations.destination(stored.kind) {
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
        if let Some(dest) = destinations.destination(stored.kind) {
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
            plain.pdf_url.as_deref(),
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
    })
}

/// Stores a multi-part plain fetch as separate files and optional cover sidecars.
async fn store_plain_parts(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    work_dir: &Path,
    plain: PlainFetch,
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
            if let Some(dest) = destinations.destination(stored.kind) {
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
            plain.pdf_url.as_deref(),
        )
        .await?,
    );

    Ok(AcquireResult {
        asin: req.asin.clone(),
        storage_key: stored_keys.primary_key.clone(),
        written_keys,
        matched_existing: false,
    })
}

/// Download + store companion PDF sidecars when `output.download_pdf` is enabled.
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
    pdf_url: Option<&str>,
) -> Result<Vec<String>> {
    if !req.options.download_pdf {
        return Ok(Vec::new());
    }
    let Some(pdf_url) = pdf_url.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };

    if !req.force {
        if let Some(book) = resolve_book(library, req).await {
            if book.pdf_status == AcquireStatus::Acquired && book.pdf_storage_key.is_some() {
                return Ok(Vec::new());
            }
        }
    }

    let remaining = remaining_temp_budget(library, req, work_dir).await?;
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
    let client = reqwest::Client::new();
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

    let mut primary_pdf_key = None;
    let mut written = Vec::new();
    for dest in &destinations.items {
        let dest_req = request_for_destination(req, dest);
        let audio_key = planned_storage_key(library, &dest_req).await;
        let pdf_key = sidecar_key(&audio_key, "pdf");
        let asin_str = object_asin_for(library, &dest_req).await;
        let meta = sidecar_meta(&asin_str, &dest_req.title, "application/pdf", &pdf_path).await;
        match dest.backend.put_file(&pdf_key, &pdf_path, meta).await {
            Ok(()) => {
                if dest.kind == destinations.primary {
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
        if let Err(err) = library
            .set_pdf_status(
                &req.asin,
                &req.account_id,
                AcquireStatus::Acquired,
                Some(&pdf_key),
            )
            .await
        {
            tracing::warn!(id = %status_key(req), error = %err, "PDF status update failed");
        }
    }

    Ok(written)
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
    if let Err(status_err) = library
        .set_pdf_status(&req.asin, &req.account_id, AcquireStatus::Error, None)
        .await
    {
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
/// # Errors
///
/// Returns [`AcquireError`] only when the library lookup for the existing PDF
/// status fails. Download and store failures are recorded on `pdf_status`.
async fn resume_missing_companion_pdf(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    req: &AcquireRequest,
    source: &dyn ContentSource,
) -> Result<()> {
    if !req.options.download_pdf {
        return Ok(());
    }
    if !req.force {
        if let Some(book) = resolve_book(library, req).await {
            if companion_pdf_is_stored(&book) {
                return Ok(());
            }
        }
    }
    match acquire_pdf_only_recorded(library, destinations, req, source).await {
        Ok(_) => Ok(()),
        Err(err) => {
            record_companion_pdf_failure(library, req, &err).await;
            Ok(())
        }
    }
}

/// True when the library row already has a stored companion PDF.
fn companion_pdf_is_stored(book: &BookRecord) -> bool {
    book.pdf_status == AcquireStatus::Acquired
        && book
            .pdf_storage_key
            .as_deref()
            .is_some_and(|key| !key.is_empty())
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
    let book = resolve_book(library, req).await;
    let created = resolve_timestamp(req.options.creation_time, book.as_ref());
    let modified = resolve_timestamp(req.options.last_write_time, book.as_ref());
    ObjectMeta {
        content_type: Some(content_type.into()),
        content_length,
        asin: Some(object_asin_for(library, req).await),
        title: Some(title.to_string()),
        creation_time: created.map(system_time_rfc3339),
        last_write_time: modified.map(system_time_rfc3339),
        ..Default::default()
    }
}

/// Prefer enriched Audible ASIN for S3 object metadata when present; otherwise
/// the acquire product id (Audible ASIN or Libro ISBN).
async fn object_asin_for(library: &LibraryStore, req: &AcquireRequest) -> String {
    resolve_book(library, req)
        .await
        .and_then(|b| b.audible_asin().map(str::to_string))
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
        "ogg" | "oga" => "audio/ogg",
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
    resolve_book(library, req)
        .await
        .and_then(|b| {
            let asin = b.asin.as_deref()?;
            // Audible-native rows set asin == product_id; enrichment ASINs differ.
            if asin == b.product_id.as_str() {
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
    let Some(book) = resolve_book(library, req).await else {
        return PlainAudibleCatalog::default();
    };
    let Some(audible_asin) = book.audible_asin().map(str::to_string) else {
        return PlainAudibleCatalog::default();
    };
    let region = book_region(&book);
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

/// Marketplace of a library row, `us` when blank.
fn book_region(book: &BookRecord) -> &str {
    if book.marketplace.trim().is_empty() {
        "us"
    } else {
        book.marketplace.as_str()
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
    let book = resolve_book(library, req).await;
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
    let book = resolve_book(library, req).await;
    let created = resolve_timestamp(req.options.creation_time, book.as_ref());
    let modified = resolve_timestamp(req.options.last_write_time, book.as_ref());
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
    book: Option<&bookclerk_library::BookRecord>,
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

/// Loads the library row by UUID first, then by product id and account.
async fn resolve_book(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Option<bookclerk_library::BookRecord> {
    if let Some(uuid) = req.book_uuid.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(Some(b)) = library.get_book_by_uuid(uuid).await {
            return Some(b);
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
    let book = resolve_book(library, req).await?;
    let audible_asin = book.audible_asin()?.to_string();
    let region = book_region(&book).to_string();
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
    let book = resolve_book(library, req).await;
    let library_id = match (&req.library_id, &book) {
        (Some(id), _) => Some(id.clone()),
        (None, Some(book)) => library_id_or_none(library, &book.uuid).await,
        (None, None) => None,
    };
    NamingContext {
        library_id,
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
        series_asin: book.as_ref().and_then(|b| b.series_asin.clone()),
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
        content_kind: book.as_ref().map(|b| b.content_kind.clone()),
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
    let Ok(Some(parent)) = library.get_book(parent_asin, &req.account_id).await else {
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
        series_asin: parent.series_asin.clone(),
        year_published: parent.published_at.map(|dt| dt.year()),
        account_id: Some(parent.account_id.clone()),
        locale: Some(parent.marketplace.clone()),
        publisher: parent.publisher.clone(),
        categories: parent.categories.clone(),
        length_minutes: parent.length_minutes,
        is_abridged: parent.is_abridged,
        content_kind: Some(parent.content_kind.clone()),
        library_id: match &req.parent_library_id {
            Some(id) => Some(id.clone()),
            None => library_id_or_none(library, &parent.uuid).await,
        },
        ..Default::default()
    }
}

/// The short Bookclerk id of the edition `book_uuid` resolves to, for
/// `<library id>` ([`bookclerk_library::library_id`]).
///
/// # Returns
///
/// `None` when the title has no edition.
///
/// # Errors
///
/// Returns an error when the edition cannot be read.
pub async fn library_id_of(
    library: &LibraryStore,
    book_uuid: &str,
) -> bookclerk_library::Result<Option<String>> {
    Ok(library
        .edition_of_book(book_uuid)
        .await?
        .and_then(|edition| bookclerk_library::library_id(&edition)))
}

/// [`library_id_of`] for naming outside an acquire's pinned id
/// ([`pin_library_id`]): a planned key a caller only compares (a preview,
/// a scan pattern). A failed read is logged and names by product id; an
/// acquire never reaches here for its own title, whose id it pinned.
async fn library_id_or_none(library: &LibraryStore, book_uuid: &str) -> Option<String> {
    library_id_of(library, book_uuid)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "could not read a title's edition for naming");
            None
        })
}

/// The folder and file naming contexts of `req`. Both carry the edition's
/// `<library id>`, which the templates must write
/// ([`bookclerk_naming::require_library_id`]), so two editions of one work
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
/// `save_podcasts_to_parent_folder`.
pub async fn planned_storage_key_for(
    library: &LibraryStore,
    req: &AcquireRequest,
    ext: &str,
) -> String {
    planned_storage_key_with_rules(library, req, ext, &req.options.replacement_characters).await
}

/// Like [`planned_storage_key_for`] but with an explicit replacement-rule set
/// (used by reconcile to probe wildcard patterns across sanitization profiles).
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
        // (other product and library id, title and subtitle) would be filed
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
            other.library_id = folder_ctx.library_id.as_deref().map(probe);
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
        let stem = stem_of(&key);
        if !names.audio_stems.contains(&stem) {
            names.audio_stems.push(stem);
        }
        // Plain parts: the file template with the part's title and a
        // `-pNNN` product and library id (as `planned_plain_part_storage_key`
        // names them).
        let mut part_ctx = file_ctx.clone();
        part_ctx.title = format!("{} — {TITLE_SENTINEL}", variant.title);
        part_ctx.asin = format!("{}-p{NUMBER_SENTINEL}", variant.asin);
        part_ctx.library_id = part_ctx
            .library_id
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
    let owner = file_owner_of_request(library, req).await?;
    let mut req = req.clone();
    pin_library_id(&mut req, owner.as_ref());
    let recorded = recording_destinations(library, destinations, owner.as_ref());
    acquire_pdf_only_recorded(library, &recorded, &req, source).await
}

/// Refuses an acquire whose files could land on another edition's: every
/// destination's templates must write `<library id>`
/// ([`bookclerk_naming::require_library_id`]), and the planned audio name
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
    let owner = match resolve_book(library, req).await {
        Some(book) => library.file_owner_of_book(&book.uuid).await?,
        None => None,
    };
    for item in &destinations.items {
        let dest_req = request_for_destination(req, item);
        let templates = dest_req.options.naming_templates();
        bookclerk_naming::require_library_id(
            &templates.folder,
            &templates.file,
            &templates.chapter_file,
        )
        .map_err(|err| {
            AcquireError::Other(anyhow::anyhow!(
                "{err} (the {} destination); fix it in Settings → Output",
                destination_id(item.kind)
            ))
        })?;
        let Some(owner) = &owner else {
            continue;
        };
        let (folder_ctx, file_ctx) = naming_contexts(library, &dest_req).await;
        let key = planned_audio_key(&dest_req, &folder_ctx, &file_ctx);
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
/// * `destinations` - Each destination's id ([`destination_id`]) and
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
    book: &BookRecord,
) -> Result<Option<String>> {
    let Some(edition) = library.edition_of_book(&book.uuid).await? else {
        return Ok(None);
    };
    // The edition records which title acquired it (`edition_acquisitions`).
    let Some(acquisition) = library.edition_acquisition(&edition).await? else {
        return Ok(None);
    };
    if acquisition.status != AcquireStatus::Acquired || acquisition.title_uuid == book.uuid {
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
        .set_acquire_status(
            &book.uuid,
            &book.account_id,
            AcquireStatus::Acquired,
            Some(key),
            None,
        )
        .await?;
    // The companion PDF stays per title: shared from the acquiring title
    // when it has one on the primary destination.
    if let Some(sibling) = library.get_book_by_uuid(&acquisition.title_uuid).await? {
        if companion_pdf_is_stored(&sibling)
            && sibling_pdf_is_present(destinations.first(), sibling.pdf_storage_key.as_deref())
                .await?
        {
            library
                .set_pdf_status(
                    &book.uuid,
                    &book.account_id,
                    AcquireStatus::Acquired,
                    sibling.pdf_storage_key.as_deref(),
                )
                .await?;
        }
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
/// named without a library id, so it waits for a scan to resolve it.
async fn file_owner_of_request(
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Result<Option<FileOwner>> {
    let Some(book) = resolve_book(library, req).await else {
        return Ok(None);
    };
    match library.file_owner_of_book(&book.uuid).await? {
        Some(owner) => Ok(Some(owner)),
        None => Err(AcquireError::Other(anyhow::anyhow!(
            "title {} has no edition yet; rescan its account before acquiring it",
            book.uuid
        ))),
    }
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
        if let Some(book) = resolve_book(library, &primary_req).await {
            if companion_pdf_is_stored(&book) {
                if let Some(key) = book.pdf_storage_key {
                    return Ok(AcquireResult {
                        asin: primary_req.asin.clone(),
                        storage_key: key,
                        written_keys: Vec::new(),
                        matched_existing: true,
                    });
                }
            }
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
    let remaining = remaining_temp_budget(library, primary_req, work_dir).await?;
    if remaining == 0 {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded (0 bytes remaining)"
        )));
    }
    let client = reqwest::Client::new();
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
        if dest.kind == destinations.primary {
            primary_pdf_key = Some(pdf_key.clone());
        }
        written_keys.push(pdf_key);
    }
    let pdf_key = primary_pdf_key
        .or_else(|| written_keys.first().cloned())
        .unwrap_or_default();
    library
        .set_pdf_status(
            &primary_req.asin,
            &primary_req.account_id,
            AcquireStatus::Acquired,
            Some(&pdf_key),
        )
        .await?;

    Ok(AcquireResult {
        asin: primary_req.asin.clone(),
        storage_key: pdf_key,
        written_keys,
        matched_existing: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload};

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
            book_uuid: None,
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
            job_id,
            temp_quota_bytes: quota,
            library_id: None,
            parent_library_id: None,
            metadata: AcquireMetadata::default(),
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
        let written =
            store_companion_pdf_sidecars(&store, &destinations, &req, &work_dir, Some(&url))
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
                kind: OutputBackendKind::Local,
                backend: Box::new(CountingBackend {
                    inner: bookclerk_storage::LocalFsBackend::new(root.to_path_buf()).unwrap(),
                    puts,
                }),
                options,
            }],
            primary: OutputBackendKind::Local,
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
            req.book_uuid = Some(book.uuid);
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
        req.book_uuid = Some(book_uuid);
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

    /// A request for `uuid` (`product_id` on `account`) named by `<title>`
    /// alone, so two editions of one title plan the same key.
    fn edition_req(
        cache: &Path,
        job: String,
        product_id: &str,
        account: &str,
        uuid: &str,
    ) -> AcquireRequest {
        let mut req = dummy_req(cache, Some(job), None);
        req.asin = product_id.into();
        req.account_id = account.into();
        req.book_uuid = Some(uuid.into());
        req.title = "Twin".into();
        req
    }

    /// Destinations writing to `root` with titles named
    /// `<title> [<library id>]/<title>`.
    fn title_named_destinations(
        root: &Path,
        puts: Arc<std::sync::atomic::AtomicUsize>,
    ) -> AcquireDestinations {
        let mut destinations = counting_destinations(root, puts);
        for item in &mut destinations.items {
            item.options.folder_template = Some("<title> [<library id>]".into());
            item.options.file_template = Some("<title>".into());
        }
        destinations
    }

    /// The `<library id>` of `uuid`'s edition.
    async fn short_id(store: &LibraryStore, uuid: &str) -> String {
        store
            .edition_of_book(uuid)
            .await
            .unwrap()
            .map(|edition| bookclerk_library::library_id(&edition).unwrap())
            .unwrap()
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

        let req = edition_req(&cache, job.clone(), "B00SAME", "user-1", &first_uuid);
        let first = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        assert!(!first.matched_existing);
        let req = edition_req(&cache, job.clone(), "B00SAME", "user-2", &second_uuid);
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
        assert_eq!(acquired.title_uuid, first_uuid);
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
            let req = edition_req(&cache, job.clone(), product, "user-1", uuid);
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

    #[tokio::test]
    async fn two_parts_with_one_title_get_two_names() {
        let store = test_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut req = dummy_req(tmp.path(), None, None);
        req.library_id = Some("hd4j63pc".into());
        let part = |n: &str| PreparedAudioFile {
            path: tmp.path().join(n),
            title: "Introduction".into(),
            ext: "mp3".into(),
        };
        let first = planned_plain_part_storage_key(&store, &req, 0, &part("a.mp3")).await;
        let second = planned_plain_part_storage_key(&store, &req, 1, &part("b.mp3")).await;
        assert_ne!(first, second, "parts with one title share {first}");
        assert!(first.contains("hd4j63pc-p001"), "{first}");
        assert!(second.contains("hd4j63pc-p002"), "{second}");
    }

    #[tokio::test]
    async fn another_editions_file_at_the_planned_name_is_not_adopted() {
        let store = test_store().await;
        let first_uuid = edition_book(&store, "B00ONE", "user-1", "Twin", None).await;
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
        let mut first = edition_req(&cache, job.clone(), "B00ONE", "user-1", &first_uuid);
        first.library_id = Some("samesame".into());
        acquire_book(&store, &destinations, first, &source)
            .await
            .unwrap();
        let mut second = edition_req(&cache, job, "B00TWO", "user-1", &second_uuid);
        second.library_id = Some("samesame".into());
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
        let req = edition_req(&cache, job, "B00LONG", "user-1", &uuid);
        let refused = acquire_book(&store, &destinations, req, &source).await;
        assert!(refused.is_err(), "an unresolved title must not acquire");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn templates_without_the_library_id_acquire_nothing() {
        let store = test_store().await;
        let uuid = edition_book(&store, "B00ONE", "user-1", "Twin", None).await;
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
        let req = edition_req(&cache, job, "B00ONE", "user-1", &uuid);
        let err = acquire_book(&store, &destinations, req.clone(), &source)
            .await
            .expect_err("a template that can share a path is refused");
        assert!(err.to_string().contains("<library id>"), "{err}");
        assert!(err.to_string().contains("local"), "{err}");
        // The companion PDF alone is refused the same way.
        let err = acquire_pdf_only(&store, &destinations, &req, &source)
            .await
            .expect_err("a PDF-only acquire with such a template is refused");
        assert!(err.to_string().contains("<library id>"), "{err}");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_name_another_edition_holds_is_never_overwritten() {
        let store = test_store().await;
        let first_uuid = edition_book(&store, "B00ONE", "user-1", "Twin", None).await;
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
        let req = edition_req(&cache, job, "B00ONE", "user-1", &first_uuid);
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
        let first_uuid = edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00SAME", "user-2", "Twin", None).await;
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
        let req = edition_req(&cache, job.clone(), "B00SAME", "user-1", &first_uuid);
        let first = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        tokio::fs::remove_file(dest.join(&first.storage_key))
            .await
            .unwrap();
        let req = edition_req(&cache, job, "B00SAME", "user-2", &second_uuid);
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
        let req = edition_req(&cache, job, "B00SAME", "user-1", &first_uuid);
        let first = acquire_book(&store, &destinations, req, &source)
            .await
            .unwrap();
        let backends: Vec<(&str, &dyn StorageBackend)> = destinations
            .items
            .iter()
            .map(|item| (destination_id(item.kind), item.backend.as_ref()))
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
            share_acquired_sibling(&store, &backends, &second)
                .await
                .unwrap(),
            None,
            "a copy missing a part is not shared"
        );
    }

    #[tokio::test]
    async fn a_sibling_named_differently_per_destination_is_shared() {
        let store = test_store().await;
        let first_uuid = edition_book(&store, "B00SAME", "user-1", "Twin", None).await;
        let second_uuid = edition_book(&store, "B00SAME", "user-2", "Twin", None).await;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let local = tmp.path().join("local");
        let archive = tmp.path().join("archive");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut destinations = title_named_destinations(&local, puts.clone());
        let mut second_destination = title_named_destinations(&archive, puts).items.remove(0);
        second_destination.kind = OutputBackendKind::S3;
        second_destination.options.folder_template = Some("Archive/<title> [<library id>]".into());
        destinations.items.push(second_destination);
        let source = CountingSource {
            fetches: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pdf_url: None,
        };
        let job = job_id(&store).await;
        let req = edition_req(&cache, job, "B00SAME", "user-1", &first_uuid);
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
            .map(|item| (destination_id(item.kind), item.backend.as_ref()))
            .collect();
        let second = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        assert_eq!(
            share_acquired_sibling(&store, &backends, &second)
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
        let req = edition_req(&cache, job, "B00SAME", "user-1", &first_uuid);
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
            .map(|item| (destination_id(item.kind), item.backend.as_ref()))
            .collect();
        let second = store.get_book_by_uuid(&second_uuid).await.unwrap().unwrap();
        assert_eq!(
            share_acquired_sibling(&store, &backends, &second)
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
        req.book_uuid = Some(book.uuid);
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
        let mut by_id = upload_naming_req(tmp.path(), "B00ONE", "One", "<library id>");
        by_id.library_id = Some("hd4j63pc".into());
        let names = planned_title_names(&store, &by_id).await;
        assert_eq!(names.folders, vec!["hd4j63pc".to_string()]);
        assert!(
            names.own_folder,
            "a folder named for the edition is its own"
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
