//! Host-side conversions between storefront types and the typed plugin ABI.
//!
//! Everything a guest also needs lives in [`bookclerk_storefront::abi`] and is
//! re-exported here. This module adds the conversions that touch host-only
//! types: [`DownloadOptions`] (built from the host configuration) and the
//! library rows a scan record becomes ([`NewBook`], [`ScannedTitle`]).

use std::path::PathBuf;

use bookclerk_library::{NewBook, ScannedTitle};
use bookclerk_plugin_abi as abi;
pub use bookclerk_storefront::abi::*;

use crate::options::DownloadOptions;

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

/// The title a scan record describes: its flat row and its listing.
///
/// The listing is `None` when the record fails listing validation (the
/// flat row is still stored and its own listing derived from it); see
/// [`scan_book_to_new`] for when no title is returned at all.
///
/// # Arguments
///
/// * `plugin_id` - The plugin that produced the record; it owns the listing.
/// * `book` - The scan record as the guest returned it.
#[must_use]
pub fn scanned_title(plugin_id: &str, book: abi::ScanBook) -> Option<ScannedTitle> {
    let mut draft = bookclerk_storefront::listing::scan_book_draft(&book);
    draft.plugin_key = plugin_id.to_string();
    let listing = match bookclerk_resolve::Listing::from_draft(&draft) {
        Ok(listing) => Some(listing),
        Err(err) => {
            tracing::warn!(
                plugin = plugin_id,
                product_id = %book.product_id,
                error = %err,
                "scan record is not a valid listing; using its flat row"
            );
            None
        }
    };
    let book = scan_book_to_new(plugin_id, book)?;
    Some(ScannedTitle { book, listing })
}

/// Maps a guest scan row onto the flat [`NewBook`] row the library stores
/// until it keeps works, expressions and editions, forcing `source` to the
/// plugin id so a guest can never claim another storefront's rows. The
/// owning `plugin_key` is stamped by the [`bookclerk_library::SourceScope`]
/// on write.
///
/// Credits join by role, the first ISBN and ASIN fill the identifier
/// columns (normalised), the first series entry fills the series columns,
/// and the work kind (with whether the record names a parent show) becomes
/// `content_kind`. An episode with no store
/// series id names its show through `parent_product_id`, which fills
/// `series_asin` so the episode is filed under the show.
///
/// The ASIN columns take an ASIN scoped to the book's marketplace, or an
/// unscoped one, before an ASIN from another marketplace.
///
/// # Returns
///
/// `None` for a record with a blank product id, which would overwrite
/// another blank row, for a record that leaves a kind unspecified (the
/// host refuses those), and for an ebook or print edition: the flat table has
/// no way to keep those out of acquire, so they are not stored until the
/// library keeps editions.
#[must_use]
pub fn scan_book_to_new(plugin_id: &str, book: abi::ScanBook) -> Option<NewBook> {
    use bookclerk_storefront::records::{self, CreditRole, IdentifierScheme};
    match book.media_kind {
        abi::MediaKind::Audio => {}
        abi::MediaKind::Unspecified | abi::MediaKind::Ebook | abi::MediaKind::Print => return None,
    }
    if book.expression_kind == abi::ExpressionKind::Unspecified {
        return None;
    }
    let content_kind = records::content_kind(book.work_kind, book.parent_product_id.is_some())?;
    if book.product_id.trim().is_empty() {
        return None;
    }
    let non_blank = |id: &String| !id.trim().is_empty();
    let series = book.series_entries.into_iter().next();
    let series_asin = series
        .as_ref()
        .and_then(|entry| entry.store_series_id.clone())
        .filter(non_blank)
        .or_else(|| {
            (book.work_kind == abi::WorkKind::Podcast)
                .then_some(book.parent_product_id)
                .flatten()
                .filter(non_blank)
        });
    let marketplace = book.marketplace.unwrap_or_else(|| String::from("us"));
    Some(NewBook {
        uuid: None,
        plugin_key: String::new(),
        source: plugin_id.to_string(),
        account_id: book.account_id,
        asin: records::asin_for_marketplace(&book.identifiers, &marketplace),
        isbn: records::identifier_value(&book.identifiers, IdentifierScheme::Isbn),
        marketplace,
        title: book.title,
        authors: records::joined_names(&book.credits, CreditRole::Author),
        narrators: records::joined_names(&book.credits, CreditRole::Narrator),
        series_index: series.as_ref().and_then(|entry| entry.position.clone()),
        series_asin,
        series: series.map(|entry| entry.name),
        purchased_at: book.purchased_at.as_deref().and_then(parse_rfc3339),
        publisher: book.publisher,
        length_minutes: book.length_minutes,
        is_abridged: book.abridgement == abi::Abridgement::Abridged,
        content_kind: content_kind.to_string(),
        categories: book.categories,
        subtitle: book.subtitle,
        published_at: book.published_at.as_deref().and_then(parse_rfc3339),
        product_id: book.product_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scan record with every kind named: a narrated audiobook.
    fn audio_book() -> abi::ScanBook {
        abi::ScanBook {
            work_kind: abi::WorkKind::Book,
            expression_kind: abi::ExpressionKind::Narration,
            media_kind: abi::MediaKind::Audio,
            ..abi::ScanBook::default()
        }
    }

    #[test]
    fn scan_book_refuses_unspecified_kinds() {
        for book in [
            abi::ScanBook {
                work_kind: abi::WorkKind::Unspecified,
                ..audio_book()
            },
            abi::ScanBook {
                expression_kind: abi::ExpressionKind::Unspecified,
                ..audio_book()
            },
            abi::ScanBook {
                media_kind: abi::MediaKind::Unspecified,
                ..audio_book()
            },
        ] {
            let book = abi::ScanBook {
                product_id: "p".into(),
                ..book
            };
            assert!(scan_book_to_new("echo", book).is_none());
        }
    }

    #[test]
    fn scan_book_forces_plugin_source() {
        let book = abi::ScanBook {
            account_id: "a".into(),
            product_id: "p".into(),
            title: "T".into(),
            ..audio_book()
        };
        let new = scan_book_to_new("echo", book).expect("audiobook row");
        assert_eq!(new.source, "echo");
        assert_eq!(new.marketplace, "us");
        assert_eq!(new.content_kind, "book");
    }

    #[test]
    fn scan_book_maps_structured_records_onto_the_flat_row() {
        use crate::records::{self, CreditRole, IdentifierScheme, WorkKind};
        let mut credits = records::credits_from_names(["A One", "A Two"], CreditRole::Author);
        credits.extend(records::credits_from_names(["N One"], CreditRole::Narrator));
        let book = abi::ScanBook {
            account_id: "a".into(),
            product_id: "p".into(),
            title: "T".into(),
            marketplace: Some("uk".into()),
            identifiers: [
                records::identifier(IdentifierScheme::Asin, "B0ASIN", Some("uk")),
                records::identifier(IdentifierScheme::Isbn, "0-14-044913-2", None),
            ]
            .into_iter()
            .flatten()
            .collect(),
            credits,
            series_entries: vec![abi::SeriesEntry {
                name: "S".into(),
                position: Some("2".into()),
                store_series_id: Some("B0SERIES".into()),
            }],
            work_kind: WorkKind::Podcast,
            abridgement: abi::Abridgement::Abridged,
            published_at: Some("2020-01-02T00:00:00Z".into()),
            purchased_at: Some("not a date".into()),
            categories: Some("Fantasy".into()),
            ..audio_book()
        };
        let new = scan_book_to_new("echo", book).expect("audiobook row");
        assert_eq!(new.authors.as_deref(), Some("A One, A Two"));
        assert_eq!(new.narrators.as_deref(), Some("N One"));
        assert_eq!(new.asin.as_deref(), Some("B0ASIN"));
        assert_eq!(new.isbn.as_deref(), Some("9780140449136"));
        assert_eq!(new.series.as_deref(), Some("S"));
        assert_eq!(new.series_index.as_deref(), Some("2"));
        assert_eq!(new.series_asin.as_deref(), Some("B0SERIES"));
        assert_eq!(new.content_kind, "episode");
        assert!(new.is_abridged);
        assert_eq!(new.marketplace, "uk");
        assert_eq!(
            new.published_at.map(rfc3339).as_deref(),
            Some("2020-01-02T00:00:00Z")
        );
        assert_eq!(new.purchased_at, None);
        assert_eq!(new.categories.as_deref(), Some("Fantasy"));
    }

    #[test]
    fn scan_book_skips_editions_that_are_never_acquired() {
        for media_kind in [abi::MediaKind::Ebook, abi::MediaKind::Print] {
            let book = abi::ScanBook {
                product_id: "p".into(),
                media_kind,
                ..audio_book()
            };
            assert!(scan_book_to_new("echo", book).is_none());
        }
        let podcast = abi::ScanBook {
            product_id: "p".into(),
            media_kind: abi::MediaKind::Audio,
            ..audio_book()
        };
        assert!(scan_book_to_new("echo", podcast).is_some());
    }

    #[test]
    fn episode_parent_fills_the_series_id_only_when_none_is_given() {
        let episode = |series_entries| abi::ScanBook {
            product_id: "ep".into(),
            work_kind: abi::WorkKind::Podcast,
            parent_product_id: Some("B0SHOW".into()),
            series_entries,
            ..audio_book()
        };
        let new = scan_book_to_new("echo", episode(Vec::new())).expect("episode row");
        assert_eq!(new.series_asin.as_deref(), Some("B0SHOW"));
        let named = vec![abi::SeriesEntry {
            name: "Show".into(),
            position: None,
            store_series_id: Some("B0OTHER".into()),
        }];
        let new = scan_book_to_new("echo", episode(named)).expect("episode row");
        assert_eq!(new.series_asin.as_deref(), Some("B0OTHER"));
        let book = abi::ScanBook {
            product_id: "b".into(),
            parent_product_id: Some("B0SHOW".into()),
            ..audio_book()
        };
        let new = scan_book_to_new("echo", book).expect("book row");
        assert_eq!(new.series_asin, None);
    }

    #[test]
    fn scan_book_skips_blank_product_ids() {
        let book = abi::ScanBook {
            product_id: "  ".into(),
            ..audio_book()
        };
        assert!(scan_book_to_new("echo", book).is_none());
    }

    #[test]
    fn blank_series_ids_fall_back_to_the_episode_parent() {
        let book = abi::ScanBook {
            product_id: "ep".into(),
            work_kind: abi::WorkKind::Podcast,
            parent_product_id: Some("B0SHOW".into()),
            series_entries: vec![abi::SeriesEntry {
                name: "Show".into(),
                position: None,
                store_series_id: Some(" ".into()),
            }],
            ..audio_book()
        };
        let new = scan_book_to_new("echo", book).expect("episode row");
        assert_eq!(new.series_asin.as_deref(), Some("B0SHOW"));
        let blank_parent = abi::ScanBook {
            product_id: "ep".into(),
            work_kind: abi::WorkKind::Podcast,
            parent_product_id: Some(String::new()),
            ..audio_book()
        };
        let new = scan_book_to_new("echo", blank_parent).expect("episode row");
        assert_eq!(new.series_asin, None);
    }

    #[test]
    fn scan_book_takes_the_asin_of_its_marketplace() {
        use crate::records::{self, IdentifierScheme};
        let book = abi::ScanBook {
            product_id: "p".into(),
            marketplace: Some("uk".into()),
            identifiers: [
                records::identifier(IdentifierScheme::Asin, "B0US", Some("us")),
                records::identifier(IdentifierScheme::Asin, "B0UK", Some("uk")),
            ]
            .into_iter()
            .flatten()
            .collect(),
            ..audio_book()
        };
        let new = scan_book_to_new("echo", book).expect("book row");
        assert_eq!(new.asin.as_deref(), Some("B0UK"));
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
}
