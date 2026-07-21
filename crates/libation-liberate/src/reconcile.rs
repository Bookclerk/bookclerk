//! Match existing liberated media in storage to library rows.

use std::collections::HashMap;
use std::path::PathBuf;

use libation_audible::DownloadOptions;
use libation_config::{
    key_matches_reconcile_pattern, reconciliation_wildcard_rules, DownloadFormat,
};
use libation_library::{BookRecord, LiberateStatus, LibraryStore};
use libation_storage::StorageBackend;

use crate::error::Result;
use crate::naming::{default_storage_key, NamingContext};
use crate::pipeline::{planned_storage_key_for, planned_storage_key_with_rules, LiberateRequest};
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
    pub matched: u32,
    pub cleared: u32,
    pub unchanged: u32,
}

/// Options for [`reconcile_library`] and [`set_download_status`].
#[derive(Debug, Clone)]
pub struct ReconcileOptions {
    pub account: Option<String>,
    /// When true, books marked Liberated whose file is missing become NotLiberated.
    pub clear_missing: bool,
    /// Limit to these ASINs (empty = all).
    pub asins: Vec<String>,
    /// When true, only mark found files as Liberated (do not clear missing).
    pub only_mark_found: bool,
    /// When true, only clear Liberated rows with no matching file.
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

/// Scan storage and update library liberate status / storage_key for matches.
pub async fn reconcile_library(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    options: ReconcileOptions,
) -> Result<ReconcileSummary> {
    let index = StorageIndex::from_storage(storage).await?;
    let books = library.list_books(options.account.as_deref())?;
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
        let matched = find_existing_for_book(&index, library, &book, &options.download);
        match matched {
            Some(key) => {
                if options.only_clear_missing {
                    summary.unchanged += 1;
                    continue;
                }
                let needs_update = book.liberate_status != LiberateStatus::Liberated
                    || book.storage_key.as_deref() != Some(key.as_str());
                if needs_update {
                    library.set_liberate_status(
                        book.title_id(),
                        &book.account_id,
                        LiberateStatus::Liberated,
                        Some(&key),
                        None,
                    )?;
                    summary.matched += 1;
                    tracing::info!(
                        asin = %book.asin_or_isbn(),
                        key = %key,
                        "matched existing liberated media"
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
                if options.clear_missing && book.liberate_status == LiberateStatus::Liberated {
                    library.set_liberate_status(
                        book.title_id(),
                        &book.account_id,
                        LiberateStatus::NotLiberated,
                        None,
                        None,
                    )?;
                    summary.cleared += 1;
                } else {
                    summary.unchanged += 1;
                }
            }
        }
    }

    Ok(summary)
}

/// Find an existing liberated file for a book (planned path or ASIN-in-path).
///
/// Honors configured folder/file templates and `save_podcasts_to_parent_folder`
/// via the same path planner as liberate.
#[must_use]
pub fn find_existing_for_book(
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
    if let Some(key) = find_existing_for_request(index, library, &req) {
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

/// Same as [`find_existing_for_book`] but for a liberate request before DB status is Liberated.
///
/// Matching strategy:
/// 1. Exact planned path under the *creation* replacement rules
/// 2. Same templates with sanitizable characters as wildcards (cross OS/backend)
/// 3. Default Author/Title/ASIN layout (exact, then wildcard)
/// 4. Template path without podcast-parent rewrite (exact, then wildcard)
/// 5. ASIN token found anywhere in a storage key
#[must_use]
pub fn find_existing_for_request(
    index: &StorageIndex,
    library: &LibraryStore,
    req: &LiberateRequest,
) -> Option<String> {
    let ext = match req.options.format {
        DownloadFormat::M4b => "m4b",
        DownloadFormat::Mp3 => "mp3",
    };

    // 1. Exact planned path (current creation sanitization).
    if let Some(key) = find_exact_planned(index, library, req, ext) {
        return Some(key);
    }

    // 2. Wildcard planned path — pickup liberations from another OS/backend.
    let wildcard_rules = reconciliation_wildcard_rules(&req.options.replacement_characters);
    if let Some(key) = find_wildcard_planned(index, library, req, &wildcard_rules) {
        return Some(key.to_string());
    }

    // 3. Default Author/Title/ASIN layout for older files.
    for alt in planned_extensions() {
        let key = default_storage_key(req.authors.as_deref(), &req.title, &req.asin, alt);
        if index.contains_key(&key) {
            return Some(key);
        }
    }
    for alt in planned_extensions() {
        let pattern = storage_key_with_rules(
            &NamingContext {
                asin: req.asin.clone(),
                title: req.title.clone(),
                authors: req.authors.clone(),
                ..Default::default()
            },
            None,
            None,
            alt,
            &wildcard_rules,
        );
        if let Some(key) = index.find_key_matching_pattern(&pattern) {
            return Some(key.to_string());
        }
    }

    // 4. When templates differ from defaults, probe raw template path without
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

fn find_exact_planned(
    index: &StorageIndex,
    library: &LibraryStore,
    req: &LiberateRequest,
    preferred_ext: &str,
) -> Option<String> {
    let planned = planned_storage_key_for(library, req, preferred_ext);
    if index.contains_key(&planned) {
        return Some(planned);
    }
    for alt in planned_extensions() {
        if *alt == preferred_ext {
            continue;
        }
        let key = planned_storage_key_for(library, req, alt);
        if index.contains_key(&key) {
            return Some(key);
        }
    }
    None
}

fn find_wildcard_planned<'a>(
    index: &'a StorageIndex,
    library: &LibraryStore,
    req: &LiberateRequest,
    wildcard_rules: &[libation_config::ReplacementRule],
) -> Option<&'a str> {
    for alt in planned_extensions() {
        let pattern = planned_storage_key_with_rules(library, req, alt, wildcard_rules);
        if let Some(key) = index.find_key_matching_pattern(&pattern) {
            return Some(key);
        }
    }
    None
}

fn request_from_book(book: &BookRecord, download: &DownloadOptions) -> LiberateRequest {
    LiberateRequest {
        asin: book.download_product_id().to_string(),
        book_uuid: Some(book.uuid.clone()),
        source: libation_source::SourceKind::parse(&book.source)
            .unwrap_or(libation_source::SourceKind::Audible),
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
        preloaded_license: None,
    }
}

fn planned_extensions() -> &'static [&'static str] {
    &["m4b", "m4a", "mp3"]
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
        "aaxc" | "aax" => 9,
        _ => 5,
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
    use bytes::Bytes;
    use libation_library::{LibraryStore, NewBook};
    use libation_storage::{LocalFsBackend, ObjectMeta, StorageBackend};
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

        let library = LibraryStore::open_in_memory().unwrap();
        library.upsert_account("acct", "us", None, true).unwrap();
        library
            .upsert_book(&NewBook::minimal("B00EXAMPLE1", "acct", "us", "Cool Book"))
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
        let book = library.get_book("B00EXAMPLE1", "acct").unwrap().unwrap();
        assert_eq!(book.liberate_status, LiberateStatus::Liberated);
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

        let library = LibraryStore::open_in_memory().unwrap();
        library.upsert_account("acct", "us", None, true).unwrap();
        let mut book = NewBook::minimal("B00EXAMPLE1", "acct", "us", "Cool Book");
        book.authors = Some("Jane Doe".into());
        library.upsert_book(&book).unwrap();

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
        let book = library.get_book("B00EXAMPLE1", "acct").unwrap().unwrap();
        assert_eq!(
            book.storage_key.as_deref(),
            Some("CustomRoot/B00EXAMPLE1.m4b")
        );
    }

    #[tokio::test]
    async fn matches_windows_sanitized_key_under_posix_rules() {
        // File liberated on Windows (colon → underscore) should still match when
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

        let library = LibraryStore::open_in_memory().unwrap();
        library.upsert_account("acct", "us", None, true).unwrap();
        let mut book = NewBook::minimal("B00EXAMPLE1", "acct", "us", "Hello: World");
        book.authors = Some("Jane Doe".into());
        library.upsert_book(&book).unwrap();

        let download = DownloadOptions {
            // Creation rules keep ':'; reconcile must still find the Windows key.
            replacement_characters: libation_config::posix_replacement_characters(),
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
        let book = library.get_book("B00EXAMPLE1", "acct").unwrap().unwrap();
        assert_eq!(
            book.storage_key.as_deref(),
            Some("Jane Doe/Hello_ World/B00EXAMPLE1.m4b")
        );
    }
}
