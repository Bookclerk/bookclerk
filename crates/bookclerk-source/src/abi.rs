//! Conversions between host storefront types and the typed Cap'n Proto plugin
//! ABI ([`bookclerk_plugin_abi`]).
//!
//! # Audience
//!
//! The plugin host (`ExternalSource`) and every native storefront guest. Both
//! sides speak the same generated structs, so the mapping lives here once
//! instead of being copied into each plugin crate.
//!
//! Wire optionality follows the ABI convention: empty text / zero numbers mean
//! "absent" and project to `Option::None` on this side.

use std::path::PathBuf;

use bookclerk_library::NewBook;
use bookclerk_plugin_abi as abi;

use crate::options::DownloadOptions;
use crate::types::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, CatalogSearchSort, ExpandSeed, LoginOptions,
    PlainAudioPart, PlainFetch, PurchaseHintOpts, SourceAccount, SourcePurchaseHint,
};

/// `limit` sent for [`crate::ContentSource::list_deals`] when the caller omits one.
pub const DEFAULT_LIST_DEALS_LIMIT: u32 = 20;

/// Host-side registry sort key for guests that leave `sortKey` at zero.
pub const DEFAULT_EXTERNAL_SORT_KEY: u32 = 200;

/// Saturating `usize` → `u32` for wire counters.
fn count_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Saturating `u32` → `usize` for wire counters.
fn count_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

impl From<SourceAccount> for abi::SourceAccount {
    fn from(account: SourceAccount) -> Self {
        Self {
            account_id: account.account_id,
            source: account.source,
            marketplace: account.marketplace,
            label: account.label,
            scan_enabled: account.scan_enabled,
        }
    }
}

impl From<abi::SourceAccount> for SourceAccount {
    fn from(account: abi::SourceAccount) -> Self {
        Self {
            account_id: account.account_id,
            source: account.source,
            marketplace: account.marketplace,
            label: account.label,
            scan_enabled: account.scan_enabled,
        }
    }
}

impl From<PlainAudioPart> for abi::PlainPart {
    fn from(part: PlainAudioPart) -> Self {
        Self {
            path: part.path.display().to_string(),
            title: part.title,
            duration_ms: part.duration_ms,
        }
    }
}

impl From<abi::PlainPart> for PlainAudioPart {
    fn from(part: abi::PlainPart) -> Self {
        Self {
            path: PathBuf::from(part.path),
            title: part.title,
            duration_ms: part.duration_ms,
        }
    }
}

impl From<PlainFetch> for abi::PlainFetch {
    fn from(plain: PlainFetch) -> Self {
        Self {
            parts: plain.parts.into_iter().map(Into::into).collect(),
            m4b_path: plain.m4b_path.map(|p| p.display().to_string()),
            cover_path: plain.cover_path.map(|p| p.display().to_string()),
            chapters: plain
                .chapters
                .into_iter()
                .map(|(title, start_ms)| abi::ChapterMarker { title, start_ms })
                .collect(),
            pdf_url: plain.pdf_url,
        }
    }
}

impl From<abi::PlainFetch> for PlainFetch {
    fn from(plain: abi::PlainFetch) -> Self {
        Self {
            parts: plain.parts.into_iter().map(Into::into).collect(),
            m4b_path: plain.m4b_path.map(PathBuf::from),
            cover_path: plain.cover_path.map(PathBuf::from),
            chapters: plain
                .chapters
                .into_iter()
                .map(|marker| (marker.title, marker.start_ms))
                .collect(),
            pdf_url: plain.pdf_url,
        }
    }
}

/// Tri-state abridgement flag ↔ the host's `Option<bool>`.
#[must_use]
pub fn abridgement_from_flag(is_abridged: Option<bool>) -> abi::Abridgement {
    match is_abridged {
        None => abi::Abridgement::Unknown,
        Some(false) => abi::Abridgement::Unabridged,
        Some(true) => abi::Abridgement::Abridged,
    }
}

/// Host `Option<bool>` view of the wire tri-state abridgement flag.
#[must_use]
pub fn abridgement_to_flag(abridgement: abi::Abridgement) -> Option<bool> {
    match abridgement {
        abi::Abridgement::Unknown => None,
        abi::Abridgement::Unabridged => Some(false),
        abi::Abridgement::Abridged => Some(true),
    }
}

impl From<CatalogHit> for abi::CatalogHit {
    fn from(hit: CatalogHit) -> Self {
        Self {
            product_id: hit.product_id,
            title: hit.title,
            authors: hit.authors,
            narrators: hit.narrators,
            series: hit.series,
            series_index: hit.series_index,
            asin: hit.asin,
            isbn: hit.isbn,
            url: hit.url,
            cover_url: hit.cover_url,
            origin: hit.origin,
            subtitle: hit.subtitle,
            description: hit.description,
            publisher: hit.publisher,
            length_minutes: hit.length_minutes,
            published_at: hit.published_at,
            categories: hit.categories,
            language: hit.language,
            price_cents: hit.price_cents,
            currency: hit.currency,
            price_label: hit.price_label,
            rating_overall: hit.rating_overall,
            rating_count: hit.rating_count,
            abridgement: abridgement_from_flag(hit.is_abridged),
        }
    }
}

impl From<abi::CatalogHit> for CatalogHit {
    fn from(hit: abi::CatalogHit) -> Self {
        Self {
            product_id: hit.product_id,
            title: hit.title,
            authors: hit.authors,
            narrators: hit.narrators,
            series: hit.series,
            series_index: hit.series_index,
            asin: hit.asin,
            isbn: hit.isbn,
            url: hit.url,
            cover_url: hit.cover_url,
            origin: hit.origin,
            subtitle: hit.subtitle,
            description: hit.description,
            publisher: hit.publisher,
            length_minutes: hit.length_minutes,
            published_at: hit.published_at,
            categories: hit.categories,
            language: hit.language,
            price_cents: hit.price_cents,
            currency: hit.currency,
            price_label: hit.price_label,
            rating_overall: hit.rating_overall,
            rating_count: hit.rating_count,
            is_abridged: abridgement_to_flag(hit.abridgement),
        }
    }
}

impl From<SourcePurchaseHint> for abi::PurchaseHint {
    fn from(hint: SourcePurchaseHint) -> Self {
        Self {
            product_id: hint.product_id,
            title: hint.title,
            url: hint.url,
            price_cents: hint.price_cents,
            currency: hint.currency,
            price_label: hint.price_label,
            list_price_cents: hint.list_price_cents,
            list_price_label: hint.list_price_label,
            member_price_cents: hint.member_price_cents,
            member_price_label: hint.member_price_label,
        }
    }
}

impl From<abi::PurchaseHint> for SourcePurchaseHint {
    fn from(hint: abi::PurchaseHint) -> Self {
        Self {
            product_id: hint.product_id,
            title: hint.title,
            url: hint.url,
            price_cents: hint.price_cents,
            currency: hint.currency,
            price_label: hint.price_label,
            list_price_cents: hint.list_price_cents,
            list_price_label: hint.list_price_label,
            member_price_cents: hint.member_price_cents,
            member_price_label: hint.member_price_label,
        }
    }
}

impl From<CatalogSearchSort> for abi::CatalogSort {
    /// Host-only re-rank modes (`price`, `length`) have no upstream sort; they
    /// travel as `relevance` and the host re-ranks the merged result.
    fn from(sort: CatalogSearchSort) -> Self {
        match sort {
            CatalogSearchSort::Relevance | CatalogSearchSort::Price | CatalogSearchSort::Length => {
                Self::Relevance
            }
            CatalogSearchSort::Popularity => Self::Popularity,
            CatalogSearchSort::Rating => Self::Rating,
            CatalogSearchSort::Title => Self::Title,
            CatalogSearchSort::Author => Self::Author,
        }
    }
}

impl From<abi::CatalogSort> for CatalogSearchSort {
    fn from(sort: abi::CatalogSort) -> Self {
        match sort {
            abi::CatalogSort::Relevance => Self::Relevance,
            abi::CatalogSort::Popularity => Self::Popularity,
            abi::CatalogSort::Rating => Self::Rating,
            abi::CatalogSort::Title => Self::Title,
            abi::CatalogSort::Author => Self::Author,
        }
    }
}

/// Wire facet for an optional host search field (`None` → `any`).
#[must_use]
pub fn catalog_field_to_abi(field: Option<CatalogSearchField>) -> abi::CatalogField {
    match field {
        None => abi::CatalogField::Any,
        Some(CatalogSearchField::Author) => abi::CatalogField::Author,
        Some(CatalogSearchField::Narrator) => abi::CatalogField::Narrator,
        Some(CatalogSearchField::Series) => abi::CatalogField::Series,
        Some(CatalogSearchField::Genre) => abi::CatalogField::Genre,
    }
}

/// Host search field for a wire facet (`any` → `None`).
#[must_use]
pub fn catalog_field_from_abi(field: abi::CatalogField) -> Option<CatalogSearchField> {
    match field {
        abi::CatalogField::Any => None,
        abi::CatalogField::Author => Some(CatalogSearchField::Author),
        abi::CatalogField::Narrator => Some(CatalogSearchField::Narrator),
        abi::CatalogField::Series => Some(CatalogSearchField::Series),
        abi::CatalogField::Genre => Some(CatalogSearchField::Genre),
    }
}

impl From<&CatalogSearchOpts> for abi::SearchCatalogParams {
    fn from(opts: &CatalogSearchOpts) -> Self {
        Self {
            query: opts.query.clone(),
            region: opts.region.clone(),
            limit: count_u32(opts.limit),
            page: opts.page.max(1),
            sort: opts.sort.into(),
            field: catalog_field_to_abi(opts.field),
            language: opts.language.clone(),
        }
    }
}

impl From<abi::SearchCatalogParams> for CatalogSearchOpts {
    fn from(params: abi::SearchCatalogParams) -> Self {
        Self {
            query: params.query,
            region: params.region,
            limit: count_usize(params.limit),
            page: params.page.max(1),
            sort: params.sort.into(),
            field: catalog_field_from_abi(params.field),
            language: params.language,
        }
    }
}

/// Builds `expandCandidates` params from a seed plus the result cap.
#[must_use]
pub fn expand_candidates_params(seed: &ExpandSeed, limit: usize) -> abi::ExpandCandidatesParams {
    abi::ExpandCandidatesParams {
        source: seed.source.clone(),
        product_id: seed.product_id.clone(),
        title: seed.title.clone(),
        authors: seed.authors.clone(),
        narrators: seed.narrators.clone(),
        series: seed.series.clone(),
        series_asin: seed.series_asin.clone(),
        asin: seed.asin.clone(),
        isbn: seed.isbn.clone(),
        region: seed.region.clone(),
        limit: count_u32(limit),
    }
}

/// Splits `expandCandidates` params into the host seed and result cap.
#[must_use]
pub fn expand_seed_from_params(params: abi::ExpandCandidatesParams) -> (ExpandSeed, usize) {
    let limit = count_usize(params.limit);
    (
        ExpandSeed {
            source: params.source,
            product_id: params.product_id,
            title: params.title,
            authors: params.authors,
            narrators: params.narrators,
            series: params.series,
            series_asin: params.series_asin,
            asin: params.asin,
            isbn: params.isbn,
            region: params.region,
        },
        limit,
    )
}

impl From<&PurchaseHintOpts> for abi::PurchaseHintParams {
    fn from(opts: &PurchaseHintOpts) -> Self {
        Self {
            product_id: opts.product_id.clone(),
            title: opts.title.clone(),
            authors: opts.authors.clone(),
            asin: opts.asin.clone(),
            isbn: opts.isbn.clone(),
            region: opts.region.clone(),
            with_price: opts.with_price,
        }
    }
}

impl From<abi::PurchaseHintParams> for PurchaseHintOpts {
    fn from(params: abi::PurchaseHintParams) -> Self {
        Self {
            product_id: params.product_id,
            title: params.title,
            authors: params.authors,
            asin: params.asin,
            isbn: params.isbn,
            region: params.region,
            with_price: params.with_price,
        }
    }
}

/// Builds guest login params. `plugin_data_dir` is the jail-granted data
/// directory; the host fills `callback_ipc` / `callback_public_base` after it
/// starts the OAuth callback proxy.
#[must_use]
pub fn login_params(plugin_data_dir: String, opts: LoginOptions) -> abi::LoginParams {
    abi::LoginParams {
        plugin_data_dir,
        marketplace: opts.marketplace,
        label: opts.label,
        email: opts.email,
        password: opts.password,
        force: opts.force,
        callback_bind: opts.callback_bind,
        callback_ipc: None,
        callback_public_base: None,
        external: opts.external,
        response_url: opts.response_url,
        show_qr: opts.show_qr,
        timeout_secs: opts.timeout_secs,
        extra: abi::ExtensibleConfig::json(&opts.extra),
    }
}

impl From<abi::LoginParams> for LoginOptions {
    /// Guest view of host login params; the callback IPC / public base are
    /// transport knobs consumed by the guest's OAuth server, not options.
    fn from(params: abi::LoginParams) -> Self {
        Self {
            marketplace: params.marketplace,
            label: params.label,
            email: params.email,
            password: params.password,
            force: params.force,
            callback_bind: params.callback_bind,
            external: params.external,
            response_url: params.response_url,
            show_qr: params.show_qr,
            timeout_secs: params.timeout_secs,
            extra: params.extra.json_value().unwrap_or(serde_json::Value::Null),
        }
    }
}

/// Wire tri-state for the remote CDM provider (`None` = classic default,
/// `Some("off")` / `Some("")` = disabled, otherwise a URL).
fn widevine_cdm_provider_to_abi(provider: Option<&str>) -> Option<String> {
    match provider {
        None => None,
        Some(value) if value.trim().is_empty() => Some("off".into()),
        Some(value) => Some(value.to_string()),
    }
}

impl From<&DownloadOptions> for abi::FetchOptions {
    /// Only fetch-time knobs cross the ABI; packaging / naming stays host-side.
    fn from(download: &DownloadOptions) -> Self {
        Self {
            widevine: download.widevine,
            xhe_aac: download.xhe_aac,
            widevine_cdm_path: download
                .widevine_cdm
                .as_ref()
                .map(|p| p.display().to_string()),
            widevine_cdm_provider: widevine_cdm_provider_to_abi(
                download.widevine_cdm_provider.as_deref(),
            ),
            download_cover: download.download_cover,
            download_pdf: download.download_pdf,
            cover_size: download.cover_size.clone(),
            chapter_layout: download.chapter_layout.clone(),
            strip_audible_brand_audio: download.strip_audible_brand_audio,
            download_clips_bookmarks: download.download_clips_bookmarks,
            retain_aax_file: download.retain_aax_file,
            download_speed_limit_kbps: download.download_speed_limit_kbps,
            save_metadata_json: download.save_metadata_json,
        }
    }
}

impl DownloadOptions {
    /// Guest-side reconstruction: host defaults overlaid with the fetch knobs
    /// the host sent in [`abi::FetchTitleParams::fetch`].
    #[must_use]
    pub fn from_fetch_options(fetch: &abi::FetchOptions) -> Self {
        let mut options = Self::default();
        options.apply_fetch_options(fetch);
        options
    }

    /// Overlays wire fetch knobs onto these options (packaging knobs untouched).
    pub fn apply_fetch_options(&mut self, fetch: &abi::FetchOptions) {
        self.widevine = fetch.widevine;
        self.xhe_aac = fetch.xhe_aac;
        self.widevine_cdm = fetch.widevine_cdm_path.as_deref().map(PathBuf::from);
        self.widevine_cdm_provider = fetch.widevine_cdm_provider.clone();
        self.download_cover = fetch.download_cover;
        self.download_pdf = fetch.download_pdf;
        if !fetch.cover_size.is_empty() {
            self.cover_size = fetch.cover_size.clone();
        }
        if !fetch.chapter_layout.is_empty() {
            self.chapter_layout = fetch.chapter_layout.clone();
        }
        self.strip_audible_brand_audio = fetch.strip_audible_brand_audio;
        self.download_clips_bookmarks = fetch.download_clips_bookmarks;
        self.retain_aax_file = fetch.retain_aax_file;
        self.download_speed_limit_kbps = fetch.download_speed_limit_kbps;
        self.save_metadata_json = fetch.save_metadata_json;
    }
}

/// Guest scan row for a library [`NewBook`] the storefront produced.
#[must_use]
pub fn scan_book_from_new(book: NewBook) -> abi::ScanBook {
    abi::ScanBook {
        account_id: book.account_id,
        product_id: book.product_id,
        title: book.title,
        marketplace: Some(book.marketplace),
        asin: book.asin,
        isbn: book.isbn,
        authors: book.authors,
        narrators: book.narrators,
        series: book.series,
        series_index: book.series_index,
        content_kind: Some(book.content_kind),
        publisher: book.publisher,
        length_minutes: book.length_minutes,
        subtitle: book.subtitle,
    }
}

/// Maps a guest scan row onto [`NewBook`], forcing `source` to the plugin id
/// so a guest can never claim another storefront's rows.
#[must_use]
pub fn scan_book_to_new(plugin_id: &str, book: abi::ScanBook) -> NewBook {
    NewBook {
        uuid: None,
        product_id: book.product_id,
        source: plugin_id.to_string(),
        account_id: book.account_id,
        asin: book.asin,
        isbn: book.isbn,
        marketplace: book.marketplace.unwrap_or_else(|| String::from("us")),
        title: book.title,
        authors: book.authors,
        narrators: book.narrators,
        series: book.series,
        series_index: book.series_index,
        series_asin: None,
        purchased_at: None,
        publisher: book.publisher,
        length_minutes: book.length_minutes,
        is_abridged: false,
        content_kind: book.content_kind.unwrap_or_else(|| String::from("book")),
        categories: None,
        subtitle: book.subtitle,
        published_at: None,
    }
}

/// Builds a guest scan summary from the books a scan produced.
#[must_use]
pub fn scan_summary(books: Vec<NewBook>, accounts: usize, pages: u32) -> abi::ScanSummary {
    abi::ScanSummary {
        accounts: count_u32(accounts),
        books_upserted: count_u32(books.len()),
        pages,
        skipped_disabled: 0,
        books: books.into_iter().map(scan_book_from_new).collect(),
    }
}

/// Host counters from a guest scan summary (`books_upserted` is the host's
/// own upsert count when it upserted rows itself).
#[must_use]
pub fn scan_summary_from_abi(summary: &abi::ScanSummary, upserted: usize) -> crate::ScanSummary {
    crate::ScanSummary {
        accounts: count_usize(summary.accounts),
        books_upserted: if upserted > 0 {
            upserted
        } else {
            count_usize(summary.books_upserted)
        },
        pages: summary.pages,
        skipped_disabled: count_usize(summary.skipped_disabled),
    }
}

/// Encodes a credential document as the opaque bytes the ABI carries.
///
/// # Errors
///
/// Returns when `credentials` cannot be serialized.
pub fn credentials_to_bytes(credentials: &serde_json::Value) -> crate::Result<Vec<u8>> {
    serde_json::to_vec(credentials).map_err(|err| crate::SourceError::api(err.to_string()))
}

/// Decodes an opaque credential blob into the JSON document guests store.
///
/// # Errors
///
/// Returns [`crate::SourceError::Auth`] when the bytes are not JSON.
pub fn credentials_from_bytes(bytes: &[u8]) -> crate::Result<serde_json::Value> {
    serde_json::from_slice(bytes)
        .map_err(|err| crate::SourceError::Auth(format!("invalid credential blob: {err}")))
}

/// Host-issued per-account credential list for `scan`.
#[must_use]
pub fn account_credentials(
    credentials: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> Vec<abi::AccountCredential> {
    credentials
        .into_iter()
        .map(|(account_id, credentials)| abi::AccountCredential {
            account_id,
            credentials,
        })
        .collect()
}

/// Guest view of the per-account credential list as JSON documents.
///
/// # Errors
///
/// Returns [`crate::SourceError::Auth`] when any blob is not JSON.
pub fn account_credentials_json(
    credentials: &[abi::AccountCredential],
) -> crate::Result<std::collections::BTreeMap<String, serde_json::Value>> {
    credentials
        .iter()
        .map(|entry| {
            credentials_from_bytes(&entry.credentials).map(|json| (entry.account_id.clone(), json))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_fetch_roundtrips_through_wire() {
        let plain = PlainFetch {
            parts: vec![PlainAudioPart {
                path: PathBuf::from("/tmp/a.m4a"),
                title: Some("Part 1".into()),
                duration_ms: Some(1234),
            }],
            m4b_path: Some(PathBuf::from("/tmp/book.m4b")),
            cover_path: None,
            chapters: vec![("Ch 1".into(), 0), ("Ch 2".into(), 5000)],
            pdf_url: Some("https://cdn.example/book.pdf".into()),
        };
        let wire: abi::PlainFetch = plain.clone().into();
        assert_eq!(wire.chapters[1].start_ms, 5000);
        let back: PlainFetch = wire.into();
        assert_eq!(back.chapters, plain.chapters);
        assert_eq!(back.m4b_path, plain.m4b_path);
        assert_eq!(back.parts[0].path, plain.parts[0].path);
        assert_eq!(back.pdf_url, plain.pdf_url);
    }

    #[test]
    fn abridgement_is_tri_state() {
        for flag in [None, Some(true), Some(false)] {
            assert_eq!(abridgement_to_flag(abridgement_from_flag(flag)), flag);
        }
        let hit = CatalogHit {
            product_id: "p".into(),
            title: "T".into(),
            is_abridged: Some(false),
            ..CatalogHit::default()
        };
        let wire: abi::CatalogHit = hit.into();
        assert_eq!(wire.abridgement, abi::Abridgement::Unabridged);
    }

    #[test]
    fn search_params_map_host_only_sorts_to_relevance() {
        let opts = CatalogSearchOpts {
            query: "dune".into(),
            sort: CatalogSearchSort::Price,
            field: Some(CatalogSearchField::Narrator),
            limit: 25,
            page: 0,
            ..CatalogSearchOpts::default()
        };
        let params = abi::SearchCatalogParams::from(&opts);
        assert_eq!(params.sort, abi::CatalogSort::Relevance);
        assert_eq!(params.field, abi::CatalogField::Narrator);
        assert_eq!(params.page, 1);
        let back = CatalogSearchOpts::from(params);
        assert_eq!(back.limit, 25);
        assert_eq!(back.field, Some(CatalogSearchField::Narrator));
    }

    #[test]
    fn scan_book_forces_plugin_source() {
        let book = abi::ScanBook {
            account_id: "a".into(),
            product_id: "p".into(),
            title: "T".into(),
            ..abi::ScanBook::default()
        };
        let new = scan_book_to_new("echo", book);
        assert_eq!(new.source, "echo");
        assert_eq!(new.marketplace, "us");
        assert_eq!(new.content_kind, "book");
    }

    #[test]
    fn fetch_options_carry_cdm_provider_tri_state() {
        let mut download = DownloadOptions {
            widevine_cdm_provider: Some(String::new()),
            ..DownloadOptions::default()
        };
        let fetch = abi::FetchOptions::from(&download);
        assert_eq!(fetch.widevine_cdm_provider.as_deref(), Some("off"));
        download.widevine_cdm_provider = None;
        assert!(abi::FetchOptions::from(&download)
            .widevine_cdm_provider
            .is_none());
        download.cover_size = "1215".into();
        let fetch = abi::FetchOptions::from(&download);
        let rebuilt = DownloadOptions::from_fetch_options(&fetch);
        assert_eq!(rebuilt.cover_size, "1215");
    }

    #[test]
    fn login_params_wrap_extra_as_json_config() {
        let opts = LoginOptions {
            marketplace: "uk".into(),
            extra: serde_json::json!({ "ascii_qr": true }),
            ..LoginOptions::default()
        };
        let params = login_params("/tmp/data".into(), opts);
        assert_eq!(params.extra.media_type, abi::JSON_MEDIA_TYPE);
        let back = LoginOptions::from(params);
        assert_eq!(back.marketplace, "uk");
        assert_eq!(back.extra["ascii_qr"], true);
    }

    #[test]
    fn account_credentials_roundtrip_as_json() {
        let creds = account_credentials([(
            "a1".to_string(),
            credentials_to_bytes(&serde_json::json!({ "token": "t" })).unwrap(),
        )]);
        let json = account_credentials_json(&creds).unwrap();
        assert_eq!(json["a1"]["token"], "t");
        credentials_from_bytes(b"not json").expect_err("must reject non-JSON blobs");
    }
}
