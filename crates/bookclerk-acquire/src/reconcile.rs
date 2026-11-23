//! Match existing acquired media in storage to library rows.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use bookclerk_config::{key_matches_reconcile_pattern, reconciliation_wildcard_rules};
use bookclerk_library::{AcquireStatus, AcquireTitle, LibraryStore};
use bookclerk_source::DownloadOptions;
use bookclerk_storage::StorageBackend;
#[cfg(test)]
use bookclerk_storage::{LocalFsBackend, ObjectMeta};
#[cfg(test)]
use bytes::Bytes;

use crate::error::Result;
use crate::naming::NamingContext;
use crate::pipeline::{planned_storage_key_for, planned_storage_key_with_rules, AcquireRequest};
use crate::storage_key_with_rules;

/// Index of storage object keys, keyed by ASIN found in the path.
#[derive(Debug, Default, Clone)]
pub struct StorageIndex {
    /// ASIN (uppercase) → candidate storage keys written during this process.
    by_asin: HashMap<String, Vec<String>>,
    /// Keys inserted via [`Self::insert_key`], not a full inventory.
    all_keys: HashMap<String, ()>,
    /// Durable scan generation in `storage_scan_rows`, when one was built.
    scan_id: Option<String>,
}

impl StorageIndex {
    /// Empty overlay.
    ///
    /// This does not list `storage`. Call [`crate::scan_storage`]
    /// when identity matching needs a durable index. Exact keys are resolved
    /// with [`StorageBackend::exists`] at decision time.
    ///
    /// # Errors
    ///
    /// Always returns `Ok`. The storage argument is accepted so older call
    /// sites keep compiling while they move to [`crate::scan_storage`].
    pub async fn from_storage(storage: &dyn StorageBackend) -> Result<Self> {
        let _ = storage.instance_id();
        Ok(Self::default())
    }

    /// Overlay plus a durable scan id.
    #[must_use]
    pub fn with_scan(scan_id: impl Into<String>) -> Self {
        Self {
            scan_id: Some(scan_id.into()),
            ..Self::default()
        }
    }

    /// Durable scan generation, when present.
    #[must_use]
    pub fn scan_id(&self) -> Option<&str> {
        self.scan_id.as_deref()
    }

    /// Insert a storage key into the index (test helper / incremental).
    pub fn insert_key(&mut self, key: String) {
        self.all_keys.insert(key.clone(), ());
        for asin in extract_asins_from_key(&key) {
            self.by_asin.entry(asin).or_default().push(key.clone());
        }
    }

    /// Returns true when `key` is present in the index.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.all_keys.contains_key(key)
    }

    /// Overlay keys matching a reconcile wildcard `pattern`, best media rank first.
    #[must_use]
    pub fn keys_matching_pattern(&self, pattern: &str) -> Vec<String> {
        let mut keys: Vec<String> = self
            .all_keys
            .keys()
            .filter(|key| key_matches_reconcile_pattern(pattern, key))
            .cloned()
            .collect();
        keys.sort_by(|left, right| {
            media_rank(left)
                .cmp(&media_rank(right))
                .then(left.cmp(right))
        });
        keys
    }

    /// First key matching a reconcile wildcard `pattern`, preferring better media types.
    #[must_use]
    pub fn find_key_matching_pattern(&self, pattern: &str) -> Option<&str> {
        self.all_keys
            .keys()
            .filter(|key| key_matches_reconcile_pattern(pattern, key))
            .min_by_key(|key| media_rank(key))
            .map(String::as_str)
    }

    /// Best matching storage key for an ASIN / ISBN token, if any.
    #[must_use]
    pub fn best_key_for_asin(&self, asin: &str) -> Option<&str> {
        let upper = asin.to_ascii_uppercase();
        if let Some(candidates) = self.by_asin.get(&upper) {
            return pick_best_media_key(candidates);
        }
        let isbn = normalize_isbn13(asin)?;
        let candidates = self.by_asin.get(&isbn)?;
        pick_best_media_key(candidates)
    }
}

/// Summary of a reconcile run.
#[derive(Debug, Clone, Default)]
pub struct ReconcileSummary {
    /// Titles matched to existing storage objects.
    pub matched: u32,
    /// Titles cleared from Acquired because no matching file was found.
    pub cleared: u32,
    /// Titles already consistent with storage.
    pub unchanged: u32,
}

/// Options for [`reconcile_library`].
#[derive(Debug, Clone)]
pub struct ReconcileOptions {
    /// Optional account id filter; `None` means all accounts.
    pub account: Option<String>,
    /// When true, books marked Acquired whose file is missing become NotAcquired.
    pub clear_missing: bool,
    /// Limit to these ASINs (empty = all).
    pub asins: Vec<String>,
    /// When true, only mark found files as Acquired (do not clear missing).
    pub only_mark_found: bool,
    /// When true, only clear Acquired rows with no matching file.
    pub only_clear_missing: bool,
    /// Naming prefs (templates, podcast parent folder) for planned-path matching.
    pub download: DownloadOptions,
    /// Lease fence when this reconcile is a phase of a durable job.
    pub fence: Option<bookclerk_library::JobFence>,
    /// Checkpoint to resume, when `fence` is set.
    pub checkpoint: Option<bookclerk_plugin_abi::JobCheckpoint>,
}

impl Default for ReconcileOptions {
    fn default() -> Self {
        Self {
            account: None,
            clear_missing: true,
            asins: Vec::new(),
            only_mark_found: false,
            only_clear_missing: false,
            download: DownloadOptions::default(),
            fence: None,
            checkpoint: None,
        }
    }
}

/// Scan storage and update library acquire status / storage_key for matches.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn reconcile_library(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    options: ReconcileOptions,
) -> Result<ReconcileSummary> {
    let index = crate::storage_scan::scan_storage(
        library,
        storage,
        options.fence.as_ref(),
        options.checkpoint.as_ref(),
        false,
    )
    .await?;
    let mut summary = ReconcileSummary::default();
    let mut walk = TitleWalk::default();
    let mut after = None;
    loop {
        let page = library
            .acquire_titles_page(options.account.as_deref(), after.as_deref(), 64)
            .await?;
        for title in page.titles {
            summary = reconcile_one(
                library, storage, &index, &options, &mut walk, title, summary,
            )
            .await?;
        }
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    if let Some(scan_id) = index.scan_id() {
        let _ = library.storage_scan_delete(scan_id).await;
    }
    Ok(summary)
}

/// What one reconcile or storage-matching walk already wrote, so the
/// edition two accounts hold is matched once, not once per title.
#[derive(Debug, Default)]
pub(crate) struct TitleWalk {
    /// Editions this walk recorded as acquired.
    matched: HashSet<String>,
    /// Editions this walk cleared (their audio was missing): once the
    /// acquiring holding is gone, every title of the edition may clear it,
    /// and the first one does.
    cleared: HashSet<String>,
}

impl TitleWalk {
    /// True when this walk already recorded `title`'s edition as acquired
    /// (the title's view was read before that write).
    pub(crate) fn already_matched(&self, title: &AcquireTitle) -> bool {
        self.matched.contains(&title.edition_id)
    }

    /// Notes that this walk recorded `title`'s edition as acquired.
    pub(crate) fn record_match(&mut self, title: &AcquireTitle) {
        self.matched.insert(title.edition_id.clone());
    }

    /// Notes that this walk cleared `title`'s edition.
    pub(crate) fn record_clear(&mut self, title: &AcquireTitle) {
        self.cleared.insert(title.edition_id.clone());
    }
}

/// True when `title` names any of `ids` (its holding id, store product
/// id, ISBN or ASIN, any case); an empty `ids` names every title.
pub(crate) fn title_is_named(title: &AcquireTitle, ids: &[String]) -> bool {
    ids.is_empty()
        || ids.iter().any(|a| {
            a.eq_ignore_ascii_case(&title.holding_id)
                || a.eq_ignore_ascii_case(&title.product_id)
                || title
                    .isbn
                    .as_ref()
                    .is_some_and(|isbn| a.eq_ignore_ascii_case(isbn))
                || title
                    .asin
                    .as_ref()
                    .is_some_and(|asin| a.eq_ignore_ascii_case(asin))
        })
}

/// What reconcile or storage matching records for a title whose edition's
/// audio was looked for in storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TitleUpdate {
    /// The edition's state already says so (or another title of it is the
    /// one to change it): nothing to write.
    Unchanged,
    /// Record the edition as acquired at this key, by this title.
    Acquired(String),
    /// Record the edition as not acquired, by this title.
    Cleared,
}

/// The write a found key (`Some`) or a miss (`None`) calls for, from the
/// edition's state: an edition is matched once per walk, and only the title
/// that acquired an edition clears it, as only that title's writes change
/// an acquired edition.
pub(crate) fn title_update(
    title: &AcquireTitle,
    walk: &TitleWalk,
    found: Option<&str>,
) -> TitleUpdate {
    match found {
        Some(key) => {
            let current = title.status == AcquireStatus::Acquired
                && title.storage_key.as_deref() == Some(key);
            if current || walk.already_matched(title) {
                TitleUpdate::Unchanged
            } else {
                TitleUpdate::Acquired(key.to_string())
            }
        }
        None => {
            let owner = title.acquired_by.is_none() || title.acquired_here();
            let settled = walk.already_matched(title) || walk.cleared.contains(&title.edition_id);
            if title.status == AcquireStatus::Acquired && owner && !settled {
                TitleUpdate::Cleared
            } else {
                TitleUpdate::Unchanged
            }
        }
    }
}

async fn reconcile_one(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    index: &StorageIndex,
    options: &ReconcileOptions,
    walk: &mut TitleWalk,
    title: AcquireTitle,
    mut summary: ReconcileSummary,
) -> Result<ReconcileSummary> {
    if !title_is_named(&title, &options.asins) {
        return Ok(summary);
    }
    let matched =
        find_existing_for_title(index, storage, library, &title, &options.download).await?;
    if (matched.is_some() && options.only_clear_missing)
        || (matched.is_none() && (options.only_mark_found || !options.clear_missing))
    {
        summary.unchanged += 1;
        return Ok(summary);
    }
    match title_update(&title, walk, matched.as_deref()) {
        TitleUpdate::Unchanged => summary.unchanged += 1,
        TitleUpdate::Acquired(key) => {
            library
                .set_holding_acquire_status(
                    &title.holding_id,
                    AcquireStatus::Acquired,
                    Some(&key),
                    None,
                )
                .await?;
            walk.record_match(&title);
            summary.matched += 1;
            tracing::info!(
                asin = %title.asin_or_isbn(),
                key = %key,
                "matched existing acquired media"
            );
        }
        TitleUpdate::Cleared => {
            let still_there = match title.storage_key.as_deref() {
                Some(key) => exists_here(storage, key).await?,
                None => false,
            };
            if still_there {
                summary.unchanged += 1;
                return Ok(summary);
            }
            library
                .set_holding_acquire_status(
                    &title.holding_id,
                    AcquireStatus::NotAcquired,
                    None,
                    None,
                )
                .await?;
            walk.record_clear(&title);
            summary.cleared += 1;
        }
    }
    Ok(summary)
}

/// Find an existing acquired file for a title (planned path or ASIN-in-path).
///
/// Honors configured folder/file templates and `save_podcasts_to_parent_folder`
/// via the same path planner as acquire.
///
/// # Errors
///
/// Returns a storage or database error from the lookup. A completed miss is
/// `Ok(None)` and is not an error.
pub async fn find_existing_for_title(
    index: &StorageIndex,
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    title: &AcquireTitle,
    download: &DownloadOptions,
) -> Result<Option<String>> {
    // 1. The edition's stored key, confirmed against the backend now.
    if let Some(key) = &title.storage_key {
        if key_present(index, storage, key).await? {
            return Ok(Some(key.clone()));
        }
    }

    let req = request_from_title(title, download);
    if let Some(key) = find_existing_for_request(index, storage, library, &req).await? {
        return Ok(Some(key));
    }

    // Fallback: match any identity token embedded in a storage key.
    for id in [
        Some(title.product_id.as_str()),
        title.asin.as_deref(),
        title.isbn.as_deref(),
        Some(title.asin_or_isbn()),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(key) = best_identity(index, storage, library, id).await? {
            return Ok(Some(key));
        }
    }
    Ok(None)
}

async fn key_present(
    index: &StorageIndex,
    storage: &dyn StorageBackend,
    key: &str,
) -> Result<bool> {
    let _ = index;
    exists_here(storage, key).await
}

/// Whether `key` holds an object on `storage`.
///
/// Reconcile probes keys spelled under every filesystem's rules, so a key
/// this backend refuses to spell (a `:` on a Windows disk) cannot hold
/// anything there and counts as absent instead of failing the pass.
///
/// # Errors
///
/// Returns any other storage error.
async fn exists_here(storage: &dyn StorageBackend, key: &str) -> Result<bool> {
    match storage.exists(key).await {
        Ok(found) => Ok(found),
        Err(bookclerk_storage::StorageError::InvalidKey(_)) => Ok(false),
        Err(err) => Err(crate::error::AcquireError::Storage(err)),
    }
}

async fn best_identity(
    index: &StorageIndex,
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    id: &str,
) -> Result<Option<String>> {
    if let Some(key) = index.best_key_for_asin(id) {
        if key_present(index, storage, key).await? {
            return Ok(Some(key.to_string()));
        }
    }
    if let Some(scan_id) = index.scan_id() {
        return first_live_scan_identity(storage, library, scan_id, id).await;
    }
    stream_identity(storage, id).await
}

async fn first_live_scan_identity(
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    scan_id: &str,
    id: &str,
) -> Result<Option<String>> {
    let mut after: Option<(i64, String)> = None;
    loop {
        let page = library
            .storage_scan_identity_page(
                scan_id,
                id,
                after.as_ref().map(|(rank, key)| (*rank, key.as_str())),
                8,
            )
            .await
            .map_err(|err| {
                crate::error::AcquireError::Other(anyhow::anyhow!("storage scan identity: {err}"))
            })?;
        if page.is_empty() {
            return Ok(None);
        }
        let page_len = page.len();
        for (key, rank) in page {
            if exists_here(storage, &key).await? {
                return Ok(Some(key));
            }
            after = Some((rank, key));
        }
        if page_len < 8 {
            return Ok(None);
        }
    }
}

async fn stream_identity(storage: &dyn StorageBackend, id: &str) -> Result<Option<String>> {
    let mut cursor = None;
    let mut best: Option<(u8, String)> = None;
    let mut hops = 0u32;
    loop {
        hops = hops.saturating_add(1);
        if hops > 1_000_000 {
            return Err(crate::error::AcquireError::Storage(
                bookclerk_storage::StorageError::InvalidCursor(
                    "identity list made no progress".into(),
                ),
            ));
        }
        let page = storage
            .list_page("", cursor.as_deref(), 0)
            .await
            .map_err(crate::error::AcquireError::Storage)?;
        for obj in &page.objects {
            if !extract_asins_from_key(&obj.key)
                .iter()
                .any(|found| found.eq_ignore_ascii_case(id))
            {
                continue;
            }
            let rank = media_rank(&obj.key);
            if rank == 0 {
                return Ok(Some(obj.key.clone()));
            }
            if best.as_ref().is_none_or(|(prev, _)| rank < *prev) {
                best = Some((rank, obj.key.clone()));
            }
        }
        match page.next_cursor {
            Some(next) if cursor.as_deref() == Some(next.as_str()) => {
                return Err(crate::error::AcquireError::Storage(
                    bookclerk_storage::StorageError::InvalidCursor(
                        "identity list cursor did not advance".into(),
                    ),
                ));
            }
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(best.map(|(_, key)| key))
}

/// Same as [`find_existing_for_title`] but for a acquire request before DB status is Acquired.
///
/// Matching strategy:
/// 1. Exact planned path under the *creation* replacement rules
/// 2. Same templates with sanitizable characters as wildcards (cross OS/backend)
/// 3. Template path without podcast-parent rewrite (exact, then wildcard)
/// 4. ASIN token found anywhere in a storage key
///
/// # Errors
///
/// Returns a storage or database error from the lookup. A completed miss is
/// `Ok(None)`.
pub async fn find_existing_for_request(
    index: &StorageIndex,
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Result<Option<String>> {
    let ext = if req.options.wants_mp3() {
        "mp3"
    } else if req.options.wants_opus() {
        "opus"
    } else {
        "m4b"
    };

    // 1. Exact planned path (current creation sanitization).
    if let Some(key) = find_exact_planned(index, storage, library, req, ext).await? {
        return Ok(Some(key));
    }

    // 2. Wildcard planned path — pickup liberations from another OS/backend.
    let wildcard_rules = reconciliation_wildcard_rules(&req.options.replacement_characters);
    if let Some(key) = find_wildcard_planned(index, storage, library, req, &wildcard_rules).await? {
        return Ok(Some(key));
    }

    // 3. When templates differ from profile defaults, probe raw template path without
    // podcast-parent rewriting (older episode layouts).
    if req.options.folder_template.is_some() || req.options.file_template.is_some() {
        let ctx = NamingContext {
            asin: req.asin.clone(),
            title: req.title.clone(),
            authors: req.authors.clone(),
            narrators: req.narrators.clone(),
            series: req.series.clone(),
            series_index: req.series_index.clone(),
            account_id: Some(req.account_id.clone()),
            call_number: req.call_number.clone(),
            ..Default::default()
        };
        for alt in planned_extensions() {
            let key = req
                .destination_keys
                .translate(storage_key_with_rules(
                    &ctx,
                    req.options.folder_template.as_deref(),
                    req.options.file_template.as_deref(),
                    alt,
                    &req.options.replacement_characters,
                ))
                .await;
            if key_present(index, storage, &key).await? {
                return Ok(Some(key));
            }
        }
        for alt in planned_extensions() {
            let pattern = storage_key_with_rules(
                &ctx,
                req.options.folder_template.as_deref(),
                req.options.file_template.as_deref(),
                alt,
                &wildcard_rules,
            );
            if let Some(key) = find_pattern(index, storage, library, &pattern).await? {
                return Ok(Some(key));
            }
        }
    }

    best_identity(index, storage, library, &req.asin).await
}

async fn find_pattern(
    index: &StorageIndex,
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    pattern: &str,
) -> Result<Option<String>> {
    for key in index.keys_matching_pattern(pattern) {
        if exists_here(storage, &key).await? {
            return Ok(Some(key));
        }
    }
    if let Some(scan_id) = index.scan_id() {
        return find_pattern_in_scan(storage, library, scan_id, pattern).await;
    }
    find_pattern_remote(storage, pattern).await
}

async fn find_pattern_in_scan(
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    scan_id: &str,
    pattern: &str,
) -> Result<Option<String>> {
    let prefix = pattern
        .split(bookclerk_config::RECONCILE_WILDCARD)
        .next()
        .unwrap_or("");
    let mut after: Option<String> = None;
    loop {
        let page = library
            .storage_scan_object_page(scan_id, prefix, after.as_deref(), 32)
            .await
            .map_err(|err| {
                crate::error::AcquireError::Other(anyhow::anyhow!("storage scan objects: {err}"))
            })?;
        if page.is_empty() {
            return Ok(None);
        }
        let page_len = page.len();
        for key in page {
            if bookclerk_config::key_matches_reconcile_pattern(pattern, &key)
                && exists_here(storage, &key).await?
            {
                return Ok(Some(key));
            }
            after = Some(key);
        }
        if page_len < 32 {
            return Ok(None);
        }
    }
}

async fn find_pattern_remote(
    storage: &dyn StorageBackend,
    pattern: &str,
) -> Result<Option<String>> {
    let prefix = pattern
        .split(bookclerk_config::RECONCILE_WILDCARD)
        .next()
        .unwrap_or("");
    let mut cursor = None;
    let mut hops = 0u32;
    loop {
        hops = hops.saturating_add(1);
        if hops > 1_000_000 {
            return Err(crate::error::AcquireError::Storage(
                bookclerk_storage::StorageError::InvalidCursor(
                    "reconcile list made no progress".into(),
                ),
            ));
        }
        let page = storage
            .list_page(prefix, cursor.as_deref(), 0)
            .await
            .map_err(crate::error::AcquireError::Storage)?;
        for obj in &page.objects {
            if bookclerk_config::key_matches_reconcile_pattern(pattern, &obj.key) {
                return Ok(Some(obj.key.clone()));
            }
        }
        match page.next_cursor {
            Some(next) if cursor.as_deref() == Some(next.as_str()) => {
                return Err(crate::error::AcquireError::Storage(
                    bookclerk_storage::StorageError::InvalidCursor(
                        "reconcile list cursor did not advance".into(),
                    ),
                ));
            }
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(None)
}

/// Looks up the planned storage key (preferred ext, then alternates) in the index.
async fn find_exact_planned(
    index: &StorageIndex,
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    req: &AcquireRequest,
    preferred_ext: &str,
) -> Result<Option<String>> {
    let planned = planned_storage_key_for(library, req, preferred_ext).await;
    if key_present(index, storage, &planned).await? {
        return Ok(Some(planned));
    }
    for alt in planned_extensions() {
        if *alt == preferred_ext {
            continue;
        }
        let key = planned_storage_key_for(library, req, alt).await;
        if key_present(index, storage, &key).await? {
            return Ok(Some(key));
        }
    }
    Ok(None)
}

/// Matches a planned key with wildcard replacement rules against paged objects.
async fn find_wildcard_planned(
    index: &StorageIndex,
    storage: &dyn StorageBackend,
    library: &LibraryStore,
    req: &AcquireRequest,
    wildcard_rules: &[bookclerk_config::ReplacementRule],
) -> Result<Option<String>> {
    for alt in planned_extensions() {
        let pattern = planned_storage_key_with_rules(library, req, alt, wildcard_rules).await;
        if let Some(key) = find_pattern(index, storage, library, &pattern).await? {
            return Ok(Some(key));
        }
    }
    Ok(None)
}

/// Builds an acquire request from a held title for storage-key
/// reconciliation.
pub(crate) fn request_from_title(
    title: &AcquireTitle,
    download: &DownloadOptions,
) -> AcquireRequest {
    AcquireRequest {
        asin: title.product_id.clone(),
        holding_id: Some(title.holding_id.clone()),
        source: title.source.clone(),
        account_id: title.account_id.clone(),
        title: title.title.clone(),
        authors: title.authors.clone(),
        narrators: title.narrators.clone(),
        series: title.series.clone(),
        series_index: title.series_index.clone(),
        options: download.clone(),
        files_dir: PathBuf::new(),
        cache_dir: PathBuf::new(),
        force: false,
        write_destinations: None,
        placement: None,
        job_id: None,
        temp_quota_bytes: None,
        cancel: None,
        call_number: None,
        parent_call_number: None,
        metadata: crate::AcquireMetadata::default(),
        library_copies: crate::LibraryCopySource::default(),
        destination_keys: crate::DestinationKeys::default(),
    }
}

/// Extensions tried when matching an existing object, packaged formats first.
fn planned_extensions() -> &'static [&'static str] {
    // Prefer packaged outputs first; include plain passthrough containers that
    // Chirp / GraphicAudio may store under noop/`as_is` output.
    &["m4b", "m4a", "mp3", "opus", "flac", "aac", "ogg", "oga"]
}

/// Extract Audible-like ASINs and ISBN-13 tokens from a storage key / file path.
///
/// Matches:
/// - filename stem `B00EXAMPLE.m4b`
/// - bracket form `Title [B00EXAMPLE].m4b` (classic Libation)
/// - path segment containing a standalone ASIN token
/// - ISBN-13 shaped tokens (13 digits, optional hyphens)
#[must_use]
pub fn extract_asins_from_key(key: &str) -> Vec<String> {
    let mut found = Vec::new();
    let normalized = key.replace('\\', "/");

    // Bracket form: [B0...] / [978...]
    for part in normalized.split(['[', ']']) {
        push_id_candidate(&mut found, part);
    }

    for segment in normalized.split('/') {
        let stem = segment.rsplit_once('.').map(|(s, _)| s).unwrap_or(segment);
        // Strip trailing " [ASIN]" already handled; also try whole stem.
        push_id_candidate(&mut found, stem);
        // Tokens separated by space / underscore / hyphen.
        for token in stem.split(|c: char| c.is_whitespace() || c == '_' || c == '-') {
            push_id_candidate(&mut found, token);
        }
    }

    found.sort();
    found.dedup();
    found
}

/// Appends an uppercase ASIN or hyphen-stripped ISBN-13 when `raw` looks like one.
fn push_id_candidate(out: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if looks_like_asin(trimmed) {
        out.push(trimmed.to_ascii_uppercase());
    } else if let Some(isbn) = normalize_isbn13(trimmed) {
        out.push(isbn);
    }
}

/// Audible product ids are typically 10 chars starting with `B`.
fn looks_like_asin(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 10 || bytes.len() > 12 {
        return false;
    }
    if !matches!(bytes[0], b'B' | b'b') {
        return false;
    }
    s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// ISBN-13: exactly 13 digits, optionally separated by hyphens or spaces.
fn normalize_isbn13(s: &str) -> Option<String> {
    if s.chars()
        .any(|c| !c.is_ascii_digit() && c != '-' && !c.is_whitespace())
    {
        return None;
    }
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 13 {
        Some(digits)
    } else {
        None
    }
}

/// Chooses the candidate with the best packaged-format rank (`m4b` over `mp3`, …).
fn pick_best_media_key(candidates: &[String]) -> Option<&str> {
    candidates
        .iter()
        .min_by_key(|key| media_rank(key))
        .map(String::as_str)
}

/// Lower is better: packaged `m4b`/`m4a`/`mp3` beat DRM leftovers (`aaxc`).
pub(crate) fn media_rank(key: &str) -> u8 {
    let ext = key.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "m4b" => 0,
        "m4a" => 1,
        "mp3" => 2,
        "flac" => 3,
        "aac" => 4,
        "ogg" | "oga" | "opus" => 5,
        "aaxc" | "aax" => 9,
        _ => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_asin_from_default_key() {
        let asins = extract_asins_from_key("Author/Title/B00EXAMPLE1.m4b");
        assert_eq!(asins, vec!["B00EXAMPLE1".to_string()]);
    }

    #[test]
    fn extracts_asin_from_bracket_form() {
        let asins = extract_asins_from_key("Author/Some Title [B0D186SQWV].m4b");
        assert!(asins.iter().any(|a| a == "B0D186SQWV"));
    }

    #[test]
    fn prefers_m4b_over_aaxc() {
        let mut index = StorageIndex::default();
        index.insert_key("x/B00EXAMPLE1.aaxc".into());
        index.insert_key("x/B00EXAMPLE1.m4b".into());
        assert_eq!(
            index.best_key_for_asin("B00EXAMPLE1"),
            Some("x/B00EXAMPLE1.m4b")
        );
    }

    #[test]
    fn rejects_short_tokens() {
        assert!(extract_asins_from_key("Author/Book.m4b").is_empty());
    }

    #[test]
    fn extracts_isbn13_from_key() {
        let ids = extract_asins_from_key("Author/Title/9781234567890.m4b");
        assert!(ids.iter().any(|id| id == "9781234567890"));

        let hyphenated = extract_asins_from_key("Author/Title [978-1-234567-89-0].m4b");
        assert!(hyphenated.iter().any(|id| id == "9781234567890"));
    }
}

#[cfg(test)]
mod reconcile_integration {
    use super::*;
    use bookclerk_library::NewBook;
    use bookclerk_storage::{LocalFsBackend, ObjectMeta, StorageBackend};
    use bytes::Bytes;
    use tempfile::tempdir;

    #[tokio::test]
    async fn a_key_the_backend_cannot_spell_is_absent() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().join("books")).unwrap();
        assert!(!exists_here(&backend, "../outside.m4b").await.unwrap());
        assert!(exists_here(&BoomStorage, "a.m4b").await.is_err());
    }

    #[tokio::test]
    async fn matches_existing_file_by_asin_in_path() {
        let dir = tempdir().unwrap();
        let store_root = dir.path().join("books");
        let backend = LocalFsBackend::new(store_root).unwrap();
        backend
            .put(
                "Some Author/Cool Book [B00EXAMPLE1].m4b",
                Bytes::from_static(b"audio"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();

        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        library
            .upsert_book(&NewBook::minimal("B00EXAMPLE1", "acct", "us", "Cool Book"))
            .await
            .unwrap();

        let summary = reconcile_library(
            &library,
            &backend,
            ReconcileOptions {
                account: None,
                clear_missing: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(summary.matched, 1);
        let book = library
            .held_title("B00EXAMPLE1", "acct")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(book.status, AcquireStatus::Acquired);
        assert!(book.storage_key.as_deref().unwrap().contains("B00EXAMPLE1"));
    }

    #[tokio::test]
    async fn matches_configured_folder_file_templates() {
        let dir = tempdir().unwrap();
        let store_root = dir.path().join("books");
        let backend = LocalFsBackend::new(store_root).unwrap();
        backend
            .put(
                "CustomRoot/B00EXAMPLE1.m4b",
                Bytes::from_static(b"audio"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();

        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut book = NewBook::minimal("B00EXAMPLE1", "acct", "us", "Cool Book");
        book.authors = Some("Jane Doe".into());
        library.upsert_book(&book).await.unwrap();

        let download = DownloadOptions {
            folder_template: Some("CustomRoot".into()),
            file_template: Some("<asin>".into()),
            ..Default::default()
        };

        let summary = reconcile_library(
            &library,
            &backend,
            ReconcileOptions {
                download,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(summary.matched, 1);
        let book = library
            .held_title("B00EXAMPLE1", "acct")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            book.storage_key.as_deref(),
            Some("CustomRoot/B00EXAMPLE1.m4b")
        );
    }

    #[tokio::test]
    async fn matches_windows_sanitized_key_under_posix_rules() {
        // File acquired on Windows (colon → underscore) should still match when
        // this host creates paths with POSIX rules (colon kept).
        let dir = tempdir().unwrap();
        let store_root = dir.path().join("books");
        let backend = LocalFsBackend::new(store_root).unwrap();
        backend
            .put(
                "Jane Doe/Hello_ World/B00EXAMPLE1.m4b",
                Bytes::from_static(b"audio"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();

        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut book = NewBook::minimal("B00EXAMPLE1", "acct", "us", "Hello: World");
        book.authors = Some("Jane Doe".into());
        library.upsert_book(&book).await.unwrap();

        let download = DownloadOptions {
            // Creation rules keep ':'; reconcile must still find the Windows key.
            replacement_characters: bookclerk_config::posix_replacement_characters(),
            ..Default::default()
        };

        let summary = reconcile_library(
            &library,
            &backend,
            ReconcileOptions {
                download,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(summary.matched, 1);
        let book = library
            .held_title("B00EXAMPLE1", "acct")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            book.storage_key.as_deref(),
            Some("Jane Doe/Hello_ World/B00EXAMPLE1.m4b")
        );
    }

    #[tokio::test]
    async fn storage_errors_do_not_clear_acquired_status() {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        library
            .upsert_book(&NewBook::minimal("B00EXAMPLE1", "acct", "us", "Cool Book"))
            .await
            .unwrap();
        library
            .set_acquire_status(
                "B00EXAMPLE1",
                "acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some("kept.m4b"),
                None,
            )
            .await
            .unwrap();
        let err = reconcile_library(
            &library,
            &BoomStorage,
            ReconcileOptions {
                clear_missing: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("storage") || err.to_string().contains("boom"),
            "{err}"
        );
        let book = library
            .held_title("B00EXAMPLE1", "acct")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(book.status, bookclerk_library::AcquireStatus::Acquired);
        assert_eq!(book.storage_key.as_deref(), Some("kept.m4b"));
    }

    /// A library where two accounts hold the same Audible product.
    async fn two_holders() -> LibraryStore {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        for account in ["acct", "other"] {
            library
                .upsert_account("audible", account, "us", None, true, "audible")
                .await
                .unwrap();
            library
                .upsert_book(&NewBook::minimal("B00EXAMPLE1", account, "us", "Cool Book"))
                .await
                .unwrap();
        }
        library
    }

    #[tokio::test]
    async fn an_edition_two_accounts_hold_is_matched_once() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().join("books")).unwrap();
        backend
            .put(
                "Some Author/Cool Book [B00EXAMPLE1].m4b",
                Bytes::from_static(b"audio"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let library = two_holders().await;
        let summary = reconcile_library(&library, &backend, ReconcileOptions::default())
            .await
            .unwrap();
        assert_eq!((summary.matched, summary.unchanged), (1, 1));
        let title = library
            .acquire_title("audible", "other", "us", "B00EXAMPLE1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(title.status, AcquireStatus::Acquired);
        assert_eq!(
            title.storage_key.as_deref(),
            Some("Some Author/Cool Book [B00EXAMPLE1].m4b")
        );

        // A second pass finds the edition's state already current.
        let again = reconcile_library(&library, &backend, ReconcileOptions::default())
            .await
            .unwrap();
        assert_eq!((again.matched, again.unchanged), (0, 2));

        // Only the titles a filter names are walked.
        let named = reconcile_library(
            &library,
            &backend,
            ReconcileOptions {
                asins: vec!["b00example1".into()],
                account: Some("other".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!((named.matched, named.unchanged), (0, 1));
        let none = reconcile_library(
            &library,
            &backend,
            ReconcileOptions {
                asins: vec!["B0NOTHERE".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!((none.matched, none.unchanged, none.cleared), (0, 0, 0));
    }

    #[tokio::test]
    async fn only_the_acquiring_title_clears_a_missing_edition() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().join("books")).unwrap();
        let library = two_holders().await;
        library
            .set_acquire_status(
                "B00EXAMPLE1",
                "other",
                AcquireStatus::Acquired,
                Some("gone.m4b"),
                None,
            )
            .await
            .unwrap();
        let summary = reconcile_library(&library, &backend, ReconcileOptions::default())
            .await
            .unwrap();
        // The acquiring title ("other") clears it; "acct" leaves it alone.
        assert_eq!((summary.cleared, summary.unchanged), (1, 1));
        let title = library
            .acquire_title("audible", "acct", "us", "B00EXAMPLE1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(title.status, AcquireStatus::NotAcquired);

        // With clearing off, or only marking found files, a miss is left.
        library
            .set_acquire_status(
                "B00EXAMPLE1",
                "other",
                AcquireStatus::Acquired,
                Some("gone.m4b"),
                None,
            )
            .await
            .unwrap();
        for options in [
            ReconcileOptions {
                clear_missing: false,
                ..Default::default()
            },
            ReconcileOptions {
                only_mark_found: true,
                ..Default::default()
            },
        ] {
            let kept = reconcile_library(&library, &backend, options)
                .await
                .unwrap();
            assert_eq!((kept.cleared, kept.unchanged), (0, 2));
        }
    }

    #[tokio::test]
    async fn a_missing_edition_whose_acquiring_holding_is_gone_is_cleared() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().join("books")).unwrap();
        let library = two_holders().await;
        library
            .set_acquire_status(
                "B00EXAMPLE1",
                "other",
                AcquireStatus::Acquired,
                Some("gone.m4b"),
                None,
            )
            .await
            .unwrap();
        let before = library
            .acquire_title("audible", "other", "us", "B00EXAMPLE1")
            .await
            .unwrap()
            .unwrap();
        // Revoking the acquiring account removes its holding; reconnecting
        // and rescanning links the title again under a new holding.
        library
            .revoke_credentials("audible", "other")
            .await
            .unwrap();
        library
            .mark_connection_active("audible", "other")
            .await
            .unwrap();
        library
            .upsert_book(&NewBook::minimal("B00EXAMPLE1", "other", "us", "Cool Book"))
            .await
            .unwrap();
        let after = library
            .acquire_title("audible", "other", "us", "B00EXAMPLE1")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(before.holding_id, after.holding_id);
        assert_eq!(after.status, AcquireStatus::Acquired);
        assert_eq!(after.acquired_by, None);

        let summary = reconcile_library(&library, &backend, ReconcileOptions::default())
            .await
            .unwrap();
        assert_eq!((summary.cleared, summary.unchanged), (1, 1));
        let title = library
            .acquire_title("audible", "acct", "us", "B00EXAMPLE1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(title.status, AcquireStatus::NotAcquired);
    }

    #[test]
    fn a_title_update_follows_the_editions_state() {
        let title = AcquireTitle {
            holding_id: "h".into(),
            edition_id: "ed".into(),
            listing_id: "l".into(),
            plugin_key: "audible".into(),
            source: "audible".into(),
            account_id: "acct".into(),
            marketplace: "us".into(),
            product_id: "B0".into(),
            asin: None,
            isbn: None,
            title: "T".into(),
            subtitle: None,
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_product_id: None,
            publisher: None,
            categories: None,
            length_minutes: None,
            language: None,
            is_abridged: false,
            content_kind: "book",
            purchased_at: None,
            added_at: None,
            published_at: None,
            status: AcquireStatus::Acquired,
            storage_key: Some("a.m4b".into()),
            acquired_by: Some("h".into()),
            pdf_status: AcquireStatus::NotAcquired,
            pdf_storage_key: None,
            pdf_acquired_by: None,
        };
        let mut walk = TitleWalk::default();
        assert_eq!(
            title_update(&title, &walk, Some("a.m4b")),
            TitleUpdate::Unchanged
        );
        assert_eq!(
            title_update(&title, &walk, Some("b.m4b")),
            TitleUpdate::Acquired("b.m4b".into())
        );
        assert_eq!(title_update(&title, &walk, None), TitleUpdate::Cleared);
        let sibling = AcquireTitle {
            holding_id: "other".into(),
            ..title.clone()
        };
        assert_eq!(title_update(&sibling, &walk, None), TitleUpdate::Unchanged);
        let mut cleared = TitleWalk::default();
        cleared.record_clear(&title);
        assert_eq!(title_update(&title, &cleared, None), TitleUpdate::Unchanged);
        walk.record_match(&title);
        assert_eq!(
            title_update(&sibling, &walk, Some("b.m4b")),
            TitleUpdate::Unchanged
        );
        assert_eq!(title_update(&title, &walk, None), TitleUpdate::Unchanged);
    }

    #[tokio::test]
    async fn deleted_preferred_format_uses_the_next_live_candidate() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().join("books")).unwrap();
        backend
            .put(
                "live.mp3",
                Bytes::from_static(b"mp3"),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .storage_scan_register("scan-candidates", "inst", None)
            .await
            .unwrap();
        for index in 0..8 {
            library
                .storage_scan_put_identity(
                    "scan-candidates",
                    "B00EXAMPLE1",
                    &format!("gone-{index}.m4b"),
                    1,
                    0,
                )
                .await
                .unwrap();
        }
        library
            .storage_scan_put_identity("scan-candidates", "B00EXAMPLE1", "live.mp3", 3, 1)
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        library
            .upsert_book(&NewBook::minimal("B00EXAMPLE1", "acct", "us", "Cool Book"))
            .await
            .unwrap();
        let title = library
            .acquire_title("audible", "acct", "us", "B00EXAMPLE1")
            .await
            .unwrap()
            .unwrap();
        let found = find_existing_for_title(
            &StorageIndex::with_scan("scan-candidates"),
            &backend,
            &library,
            &title,
            &bookclerk_source::DownloadOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(found.as_deref(), Some("live.mp3"));
    }

    #[tokio::test]
    async fn wildcard_lookups_reuse_the_durable_inventory() {
        let dir = tempdir().unwrap();
        let inner = LocalFsBackend::new(dir.path().join("books")).unwrap();
        let lists = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut book = NewBook::minimal("B00EXAMPLE1", "acct", "us", "Hello: World");
        book.authors = Some("Jane Doe".into());
        library.upsert_book(&book).await.unwrap();
        let stored = library
            .held_title("B00EXAMPLE1", "acct")
            .await
            .unwrap()
            .unwrap();
        // The default layout names the file after the edition's call number.
        let edition = library
            .edition_of_holding(&stored.holding_id)
            .await
            .unwrap()
            .unwrap();
        let short = bookclerk_library::call_number(&edition).unwrap();
        let key = format!("Jane Doe/Hello/Hello_ World _{short}_.m4b");
        let key = key.as_str();
        inner
            .put(key, Bytes::from_static(b"audio"), ObjectMeta::default())
            .await
            .unwrap();
        let backend = CountLists {
            inner,
            lists: std::sync::Arc::clone(&lists),
        };
        library
            .storage_scan_register("scan-wild", "inst", None)
            .await
            .unwrap();
        library
            .storage_scan_put_object("scan-wild", key, 5, 0, true)
            .await
            .unwrap();
        let title = library
            .acquire_title("audible", "acct", "us", "B00EXAMPLE1")
            .await
            .unwrap()
            .unwrap();
        let req = request_from_title(
            &title,
            &bookclerk_source::DownloadOptions {
                replacement_characters: bookclerk_config::posix_replacement_characters(),
                ..Default::default()
            },
        );
        let index = StorageIndex::with_scan("scan-wild");
        for _ in 0..2 {
            let found = find_existing_for_request(&index, &backend, &library, &req)
                .await
                .unwrap();
            assert_eq!(found.as_deref(), Some(key));
        }
        assert_eq!(lists.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}

#[cfg(test)]
struct CountLists {
    inner: LocalFsBackend,
    lists: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl StorageBackend for CountLists {
    fn name(&self) -> &'static str {
        "count-lists"
    }
    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(Self {
            inner: self.inner.clone(),
            lists: std::sync::Arc::clone(&self.lists),
        })
    }
    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> bookclerk_storage::Result<()> {
        self.inner.put(key, data, meta).await
    }
    async fn get(&self, key: &str) -> bookclerk_storage::Result<Bytes> {
        self.inner.get(key).await
    }
    async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
        self.inner.exists(key).await
    }
    async fn list(
        &self,
        prefix: &str,
    ) -> bookclerk_storage::Result<Vec<bookclerk_storage::ObjectInfo>> {
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.list(prefix).await
    }
    async fn probe(&self, key: &str) -> bookclerk_storage::Result<bookclerk_storage::ObjectProbe> {
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
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
}

#[cfg(test)]
struct BoomStorage;

#[cfg(test)]
#[async_trait::async_trait]
impl StorageBackend for BoomStorage {
    fn name(&self) -> &'static str {
        "boom"
    }
    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(BoomStorage)
    }
    async fn put(&self, _: &str, _: Bytes, _: ObjectMeta) -> bookclerk_storage::Result<()> {
        Err(bookclerk_storage::StorageError::Io(std::io::Error::other(
            "boom",
        )))
    }
    async fn get(&self, key: &str) -> bookclerk_storage::Result<Bytes> {
        Err(bookclerk_storage::StorageError::NotFound(key.into()))
    }
    async fn exists(&self, _: &str) -> bookclerk_storage::Result<bool> {
        Err(bookclerk_storage::StorageError::Io(std::io::Error::other(
            "boom exists",
        )))
    }
    async fn list(&self, _: &str) -> bookclerk_storage::Result<Vec<bookclerk_storage::ObjectInfo>> {
        Err(bookclerk_storage::StorageError::Io(std::io::Error::other(
            "boom list",
        )))
    }
    async fn probe(&self, key: &str) -> bookclerk_storage::Result<bookclerk_storage::ObjectProbe> {
        Err(bookclerk_storage::StorageError::NotFound(key.into()))
    }
    async fn copy(&self, _: &str, _: &str) -> bookclerk_storage::Result<()> {
        Err(bookclerk_storage::StorageError::Io(std::io::Error::other(
            "boom",
        )))
    }
    async fn delete(&self, _: &str) -> bookclerk_storage::Result<()> {
        Ok(())
    }
    async fn list_page(
        &self,
        _: &str,
        _: Option<&str>,
        _: u32,
    ) -> bookclerk_storage::Result<bookclerk_storage::ListPage> {
        Err(bookclerk_storage::StorageError::Io(std::io::Error::other(
            "boom list",
        )))
    }
    async fn get_stream(
        &self,
        key: &str,
        _: Option<bookclerk_storage::ByteRange>,
    ) -> bookclerk_storage::Result<(
        bookclerk_storage::ObjectProbe,
        std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    )> {
        Err(bookclerk_storage::StorageError::NotFound(key.into()))
    }
    async fn put_stream(
        &self,
        _: &str,
        _: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        _: ObjectMeta,
    ) -> bookclerk_storage::Result<bookclerk_storage::PutStreamResult> {
        Err(bookclerk_storage::StorageError::Io(std::io::Error::other(
            "boom",
        )))
    }
}
