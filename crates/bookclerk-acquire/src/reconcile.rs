//! Match existing acquired media in storage to library rows.

use std::collections::HashMap;
use std::path::PathBuf;

use bookclerk_config::{key_matches_reconcile_pattern, reconciliation_wildcard_rules};
use bookclerk_library::{AcquireStatus, BookRecord, LibraryStore};
use bookclerk_source::DownloadOptions;
use bookclerk_storage::StorageBackend;

use crate::error::Result;
use crate::naming::NamingContext;
use crate::pipeline::{planned_storage_key_for, planned_storage_key_with_rules, AcquireRequest};
use crate::storage_key_with_rules;

/// Index of storage object keys, keyed by ASIN found in the path.
#[derive(Debug, Default, Clone)]
pub struct StorageIndex {
    /// ASIN (uppercase) → candidate storage keys.
    by_asin: HashMap<String, Vec<String>>,
    /// All keys (for planned-key exact checks).
    all_keys: HashMap<String, ()>,
}

impl StorageIndex {
    /// Build an index by listing the storage backend.
    pub async fn from_storage(storage: &dyn StorageBackend) -> Result<Self> {
        let objects = storage.list("").await?;
        let mut index = Self::default();
        for obj in objects {
            index.insert_key(obj.key);
        }
        Ok(index)
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
        }
    }
}

/// Scan storage and update library acquire status / storage_key for matches.
pub async fn reconcile_library(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    options: ReconcileOptions,
) -> Result<ReconcileSummary> {
    let index = StorageIndex::from_storage(storage).await?;
    let books = library.list_books(options.account.as_deref()).await?;
    let mut summary = ReconcileSummary::default();

    for book in books {
        if !options.asins.is_empty()
            && !options.asins.iter().any(|a| {
                a.eq_ignore_ascii_case(&book.uuid)
                    || a.eq_ignore_ascii_case(&book.product_id)
                    || book
                        .isbn
                        .as_ref()
                        .is_some_and(|isbn| a.eq_ignore_ascii_case(isbn))
                    || book
                        .asin
                        .as_ref()
                        .is_some_and(|asin| a.eq_ignore_ascii_case(asin))
            })
        {
            continue;
        }
        let matched = find_existing_for_book(&index, library, &book, &options.download).await;
        match matched {
            Some(key) => {
                if options.only_clear_missing {
                    summary.unchanged += 1;
                    continue;
                }
                let needs_update = book.acquire_status != AcquireStatus::Acquired
                    || book.storage_key.as_deref() != Some(key.as_str());
                if needs_update {
                    library
                        .set_acquire_status(
                            book.title_id(),
                            &book.account_id,
                            AcquireStatus::Acquired,
                            Some(&key),
                            None,
                        )
                        .await?;
                    summary.matched += 1;
                    tracing::info!(
                        asin = %book.asin_or_isbn(),
                        key = %key,
                        "matched existing acquired media"
                    );
                } else {
                    summary.unchanged += 1;
                }
            }
            None => {
                if options.only_mark_found {
                    summary.unchanged += 1;
                    continue;
                }
                if options.clear_missing && book.acquire_status == AcquireStatus::Acquired {
                    library
                        .set_acquire_status(
                            book.title_id(),
                            &book.account_id,
                            AcquireStatus::NotAcquired,
                            None,
                            None,
                        )
                        .await?;
                    summary.cleared += 1;
                } else {
                    summary.unchanged += 1;
                }
            }
        }
    }

    Ok(summary)
}

/// Find an existing acquired file for a book (planned path or ASIN-in-path).
///
/// Honors configured folder/file templates and `save_podcasts_to_parent_folder`
/// via the same path planner as acquire.
pub async fn find_existing_for_book(
    index: &StorageIndex,
    library: &LibraryStore,
    book: &BookRecord,
    download: &DownloadOptions,
) -> Option<String> {
    // 1. Exact stored key.
    if let Some(key) = &book.storage_key {
        if index.contains_key(key) {
            return Some(key.clone());
        }
    }

    let req = request_from_book(book, download);
    if let Some(key) = find_existing_for_request(index, library, &req).await {
        return Some(key);
    }

    // Fallback: match any identity token embedded in a storage key.
    for id in [
        Some(book.product_id.as_str()),
        book.asin.as_deref(),
        book.isbn.as_deref(),
        Some(book.asin_or_isbn()),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(key) = index.best_key_for_asin(id) {
            return Some(key.to_string());
        }
    }
    None
}

/// Same as [`find_existing_for_book`] but for a acquire request before DB status is Acquired.
///
/// Matching strategy:
/// 1. Exact planned path under the *creation* replacement rules
/// 2. Same templates with sanitizable characters as wildcards (cross OS/backend)
/// 3. Template path without podcast-parent rewrite (exact, then wildcard)
/// 4. ASIN token found anywhere in a storage key
pub async fn find_existing_for_request(
    index: &StorageIndex,
    library: &LibraryStore,
    req: &AcquireRequest,
) -> Option<String> {
    let ext = if req.options.wants_mp3() {
        "mp3"
    } else if req.options.wants_opus() {
        "opus"
    } else {
        "m4b"
    };

    // 1. Exact planned path (current creation sanitization).
    if let Some(key) = find_exact_planned(index, library, req, ext).await {
        return Some(key);
    }

    // 2. Wildcard planned path — pickup liberations from another OS/backend.
    let wildcard_rules = reconciliation_wildcard_rules(&req.options.replacement_characters);
    if let Some(key) = find_wildcard_planned(index, library, req, &wildcard_rules).await {
        return Some(key.to_string());
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
            ..Default::default()
        };
        for alt in planned_extensions() {
            let key = storage_key_with_rules(
                &ctx,
                req.options.folder_template.as_deref(),
                req.options.file_template.as_deref(),
                alt,
                &req.options.replacement_characters,
            );
            if index.contains_key(&key) {
                return Some(key);
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
            if let Some(key) = index.find_key_matching_pattern(&pattern) {
                return Some(key.to_string());
            }
        }
    }

    index.best_key_for_asin(&req.asin).map(str::to_string)
}

async fn find_exact_planned(
    index: &StorageIndex,
    library: &LibraryStore,
    req: &AcquireRequest,
    preferred_ext: &str,
) -> Option<String> {
    let planned = planned_storage_key_for(library, req, preferred_ext).await;
    if index.contains_key(&planned) {
        return Some(planned);
    }
    for alt in planned_extensions() {
        if *alt == preferred_ext {
            continue;
        }
        let key = planned_storage_key_for(library, req, alt).await;
        if index.contains_key(&key) {
            return Some(key);
        }
    }
    None
}

async fn find_wildcard_planned<'a>(
    index: &'a StorageIndex,
    library: &LibraryStore,
    req: &AcquireRequest,
    wildcard_rules: &[bookclerk_config::ReplacementRule],
) -> Option<&'a str> {
    for alt in planned_extensions() {
        let pattern = planned_storage_key_with_rules(library, req, alt, wildcard_rules).await;
        if let Some(key) = index.find_key_matching_pattern(&pattern) {
            return Some(key);
        }
    }
    None
}

pub(crate) fn request_from_book(book: &BookRecord, download: &DownloadOptions) -> AcquireRequest {
    AcquireRequest {
        asin: book.download_product_id().to_string(),
        book_uuid: Some(book.uuid.clone()),
        source: book.source.clone(),
        account_id: book.account_id.clone(),
        title: book.title.clone(),
        authors: book.authors.clone(),
        narrators: book.narrators.clone(),
        series: book.series.clone(),
        series_index: book.series_index.clone(),
        options: download.clone(),
        files_dir: PathBuf::new(),
        cache_dir: PathBuf::new(),
        force: false,
        write_destinations: None,
    }
}

fn planned_extensions() -> &'static [&'static str] {
    // Prefer packaged outputs first; include plain passthrough containers that
    // Chirp / GraphicAudio may store under noop/`as_is` output.
    &["m4b", "m4a", "mp3", "flac", "aac", "ogg", "oga"]
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

fn pick_best_media_key(candidates: &[String]) -> Option<&str> {
    candidates
        .iter()
        .min_by_key(|key| media_rank(key))
        .map(String::as_str)
}

fn media_rank(key: &str) -> u8 {
    let ext = key.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "m4b" => 0,
        "m4a" => 1,
        "mp3" => 2,
        "flac" => 3,
        "aac" => 4,
        "ogg" | "oga" => 5,
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
            .upsert_account("acct", "us", None, true, "audible")
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
            .get_book("B00EXAMPLE1", "acct")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(book.acquire_status, AcquireStatus::Acquired);
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
            .upsert_account("acct", "us", None, true, "audible")
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
            .get_book("B00EXAMPLE1", "acct")
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
            .upsert_account("acct", "us", None, true, "audible")
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
            .get_book("B00EXAMPLE1", "acct")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            book.storage_key.as_deref(),
            Some("Jane Doe/Hello_ World/B00EXAMPLE1.m4b")
        );
    }
}
