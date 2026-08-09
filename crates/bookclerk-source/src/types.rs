//! Shared types for content sources.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::options::DownloadOptions;

/// One allowed value for a [`SourceConfigOption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigOptionValue {
    /// Wire / TOML id (`high`, `m4b`, `web`, …).
    pub id: &'static str,
    /// Human label.
    pub label: &'static str,
}

/// One source-native config knob under `[sources.<id>]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceConfigOption {
    /// TOML key (`bitrate`, `container`, `access`).
    pub key: &'static str,
    /// Human label.
    pub label: &'static str,
    /// Allowed values.
    pub values: &'static [ConfigOptionValue],
}

/// Account discovered or created by a content source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceAccount {
    pub account_id: String,
    /// Canonical plugin id (`audible`, `libro`, …).
    pub source: String,
    pub marketplace: String,
    pub label: Option<String>,
    pub scan_enabled: bool,
}

/// Options for interactive / CLI login.
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    pub marketplace: String,
    pub label: Option<String>,
    /// Email/password sources; ignored for OAuth.
    pub email: Option<String>,
    /// Email/password sources; ignored for OAuth.
    pub password: Option<String>,
    pub force: bool,
    /// Optional OAuth callback bind (`host:port`) for portal / reverse-proxy use.
    pub callback_bind: Option<String>,
    /// External / paste-redirect OAuth instead of a local callback server.
    pub external: bool,
    /// Pre-supplied OAuth redirect URL (with [`Self::external`]).
    pub response_url: Option<String>,
    /// Emit a terminal QR for the authorize URL when the source supports it.
    pub show_qr: bool,
    /// Seconds to wait for OAuth callback capture (source-defined default when `None`).
    pub timeout_secs: Option<u64>,
    /// Store-specific knobs (`audible_username`, `ascii_qr`, …). Guests may ignore unknowns.
    pub extra: serde_json::Value,
}

/// Options for [`crate::ContentSource::import_credentials`].
#[derive(Debug, Clone, Default)]
pub struct ImportCredentialsOptions {
    /// Treat input as classic Libation `AccountsSettings.json`.
    pub libation_accounts: bool,
    /// Import mkb79 / audible-cli legacy auth JSON.
    pub mkb79: bool,
    /// Destination display label / filename stem.
    pub label: Option<String>,
    /// Overwrite an existing credential.
    pub force: bool,
}

/// Progress events for interactive OAuth login (portal / CLI).
#[derive(Debug, Clone)]
pub enum OAuthProgress {
    /// Browser URL the operator should open (optional pre-rendered QR text).
    LoginUrl { url: String, qr: Option<String> },
    /// Local callback server is listening (SSH port-forward hint).
    CallbackListening { addr: String },
    /// Waiting for the OAuth redirect / callback.
    WaitingForCallback,
    /// Login finished for this account id.
    Completed { account_id: String },
}

/// Options for a library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Limit to specific account nicknames / ids.
    pub accounts: Vec<String>,
    pub page_size: u32,
    /// Import podcast episodes — consumed by plugins that support it.
    pub import_episodes: bool,
    /// Import catalog Plus / non-owned titles — consumed by plugins that support it.
    pub import_plus_titles: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            page_size: 50,
            import_episodes: true,
            import_plus_titles: true,
        }
    }
}

/// Summary of a scan run.
#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub accounts: usize,
    pub books_upserted: usize,
    pub pages: u32,
    pub skipped_disabled: usize,
}

impl ScanSummary {
    /// Merge another source's summary into this one.
    pub fn merge(&mut self, other: &Self) {
        self.accounts += other.accounts;
        self.books_upserted += other.books_upserted;
        self.pages += other.pages;
        self.skipped_disabled += other.skipped_disabled;
    }
}

/// Options passed to [`crate::ContentSource::fetch_title`].
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub download: DownloadOptions,
    pub cache_dir: PathBuf,
    /// Root files directory (`BOOKCLERK_FILES_DIR`). Used for CDM / Widevine path
    /// resolution by Audible. Non-auth operations only; auth is loaded from the
    /// [`bookclerk_library::LibraryStore`] passed directly to the trait method.
    pub files_dir: PathBuf,
}

/// One DRM-free audio part (chapter file or single book).
#[derive(Debug, Clone)]
pub struct PlainAudioPart {
    pub path: PathBuf,
    pub title: Option<String>,
    /// Duration in milliseconds when known.
    pub duration_ms: Option<u64>,
}

/// DRM-free fetch result. Sources that use DRM decrypt inside the plugin and
/// return clear media here — the host never sees ciphertext or keys.
#[derive(Debug, Clone)]
pub struct PlainFetch {
    pub parts: Vec<PlainAudioPart>,
    /// Pre-built M4B from the store / plugin when available.
    pub m4b_path: Option<PathBuf>,
    pub cover_path: Option<PathBuf>,
    pub chapters: Vec<(String, u64)>,
    /// Companion PDF download URL when the store exposes one.
    pub pdf_url: Option<String>,
}

/// Result of fetching a title for acquire (always clear media).
///
/// Historically a dual Encrypted/Plain enum; DRM is plugin-owned now, so this
/// is an alias of [`PlainFetch`]. Prefer `PlainFetch` in new code.
pub type SourceFetch = PlainFetch;

/// Neutral catalog / candidate hit (no store crate types).
///
/// Identity fields are required; bibliographic extras and list price are filled
/// when a storefront’s search/expand payload already includes them (avoids a
/// second Audnexus / purchase-hint round-trip for Discover detail).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CatalogHit {
    pub product_id: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub url: Option<String>,
    /// Public cover image URL when the storefront provides one.
    #[serde(default)]
    pub cover_url: Option<String>,
    /// How this was found (`related`, `series`, `author`, `search`, `top_deals`, …).
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub length_minutes: Option<i64>,
    /// ISO-8601 / store release date string when known.
    #[serde(default)]
    pub published_at: Option<String>,
    /// Genre / subject labels (`;`-separated).
    #[serde(default)]
    pub categories: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    /// List / deal price from the same catalog payload (optional).
    #[serde(default)]
    pub price_cents: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub price_label: Option<String>,
    /// Community overall rating (0–5) when the storefront returned it.
    #[serde(default)]
    pub rating_overall: Option<f64>,
    /// Number of star ratings backing [`Self::rating_overall`] when known.
    #[serde(default)]
    pub rating_count: Option<i64>,
    /// `Some(true)` when the storefront marks the title abridged; `Some(false)`
    /// for unabridged; `None` when unknown.
    #[serde(default)]
    pub is_abridged: Option<bool>,
}

impl CatalogHit {
    /// Decode HTML entities in human-readable metadata fields.
    ///
    /// Storefronts (notably Libro.fm) sometimes return `Memory&#39;s Blade`-style
    /// titles; normalize before discover merge / UI display.
    ///
    /// No-op (no allocations) when no field contains `&`.
    #[must_use]
    pub fn decode_html_entities(mut self) -> Self {
        bookclerk_library::decode_html_entities_in_place(&mut self.title);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.authors);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.narrators);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.series);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.subtitle);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.description);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.publisher);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.categories);
        bookclerk_library::decode_html_entities_in_place(&mut self.origin);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.price_label);
        // Identifiers / URLs are left alone (must stay wire-exact).
        self
    }
}

/// Seed for related / series / author expansion.
#[derive(Debug, Clone)]
pub struct ExpandSeed {
    /// Source id of the seed title (`chirp`, `audible`, …).
    pub source: String,
    pub product_id: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    /// Parent Audible series ASIN when known (from library metadata).
    pub series_asin: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    /// Marketplace / catalog region (`us`, `uk`, …).
    pub region: String,
}

/// URL + optional live price for one storefront edition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcePurchaseHint {
    pub product_id: String,
    pub title: Option<String>,
    pub url: Option<String>,
    /// Primary / best known sell price in minor units (prefer member when dual).
    pub price_cents: Option<i64>,
    pub currency: Option<String>,
    pub price_label: Option<String>,
    /// Non-member / list / retail price when the storefront shows dual pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_cents: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_label: Option<String>,
    /// Member / subscriber price when distinct from list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_cents: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_label: Option<String>,
}

impl SourcePurchaseHint {
    /// Decode HTML entities in human-readable fields (no-op when no `&`).
    #[must_use]
    pub fn decode_html_entities(mut self) -> Self {
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.title);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.price_label);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.list_price_label);
        bookclerk_library::decode_html_entities_opt_in_place(&mut self.member_price_label);
        self
    }
}

/// Optional role / facet for [`crate::ContentSource::search_catalog`].
///
/// Free-text Discover typeahead leaves this unset. Meta links (author, narrator,
/// series, genre) set it so storefronts that support scoped params (e.g. Audible
/// `author=` / `narrator=`) can return the right catalog slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSearchField {
    Author,
    Narrator,
    Series,
    Genre,
}

impl CatalogSearchField {
    /// Parse a wire / query-string value (`author`, `narrators`, …).
    #[must_use]
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "author" | "authors" => Some(Self::Author),
            "narrator" | "narrators" => Some(Self::Narrator),
            "series" => Some(Self::Series),
            "genre" | "genres" => Some(Self::Genre),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Author => "author",
            Self::Narrator => "narrator",
            Self::Series => "series",
            Self::Genre => "genre",
        }
    }
}

/// Sort mode for [`crate::ContentSource::search_catalog`].
///
/// Storefronts that lack a matching upstream sort ignore this (caller merges).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSearchSort {
    #[default]
    Relevance,
    /// Audible `BestSellers` when available.
    Popularity,
    /// Audible `AvgRating` when available.
    Rating,
    Title,
    Author,
    /// Catalog list/deal `price_cents` (host re-rank; sparse on some stores).
    Price,
    /// `length_minutes` (host re-rank).
    Length,
}

/// Ascending / descending for catalog search host re-rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSortDir {
    Asc,
    #[default]
    Desc,
}

impl CatalogSortDir {
    #[must_use]
    pub fn from_wire(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "asc" | "ascending" => Self::Asc,
            _ => Self::Desc,
        }
    }

    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    /// Default direction when the client omits `sort_dir`.
    #[must_use]
    pub fn default_for_sort(sort: CatalogSearchSort) -> Self {
        match sort {
            CatalogSearchSort::Title | CatalogSearchSort::Author | CatalogSearchSort::Length => {
                Self::Asc
            }
            CatalogSearchSort::Relevance
            | CatalogSearchSort::Popularity
            | CatalogSearchSort::Rating
            | CatalogSearchSort::Price => Self::Desc,
        }
    }
}

impl CatalogSearchSort {
    #[must_use]
    pub fn from_wire(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "popularity" | "bestsellers" | "best_sellers" => Self::Popularity,
            "rating" | "avgrating" | "avg_rating" => Self::Rating,
            "title" => Self::Title,
            "author" | "authors" => Self::Author,
            "price" => Self::Price,
            "length" | "runtime" | "duration" => Self::Length,
            _ => Self::Relevance,
        }
    }

    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Popularity => "popularity",
            Self::Rating => "rating",
            Self::Title => "title",
            Self::Author => "author",
            Self::Price => "price",
            Self::Length => "length",
        }
    }

    /// Audible `/1.0/catalog/products` `products_sort_by` value.
    #[must_use]
    pub fn audible_products_sort_by(self) -> &'static str {
        match self {
            Self::Relevance | Self::Price | Self::Length => "Relevance",
            Self::Popularity => "BestSellers",
            Self::Rating => "AvgRating",
            Self::Title => "Title",
            // No author sort upstream — keep relevance and let the host re-rank.
            Self::Author => "Relevance",
        }
    }

    /// Audible `/1.0/catalog/search` `sort` (`sort_option_id`) value.
    ///
    /// Prefer this endpoint for Discover keyword browse — `/catalog/products`
    /// keyword/title filters often omit major titles that still resolve by ASIN.
    #[must_use]
    pub fn audible_catalog_search_sort(self) -> &'static str {
        match self {
            Self::Relevance | Self::Author | Self::Price | Self::Length => "relevancerank",
            Self::Popularity => "popularity-rank",
            Self::Rating => "review-rank",
            Self::Title => "title-asc-rank",
        }
    }
}

/// Options for [`crate::ContentSource::search_catalog`].
#[derive(Debug, Clone)]
pub struct CatalogSearchOpts {
    pub query: String,
    pub region: String,
    pub limit: usize,
    /// 1-based page index for storefronts that support paging (default 1).
    pub page: u32,
    /// Preferred upstream / merge sort.
    pub sort: CatalogSearchSort,
    /// When set, prefer storefront-native search for this facet.
    pub field: Option<CatalogSearchField>,
    /// Preferred content language (BCP-47 primary or display name); soft-prioritize.
    pub language: Option<String>,
}

impl Default for CatalogSearchOpts {
    fn default() -> Self {
        Self {
            query: String::new(),
            region: String::new(),
            limit: 0,
            page: 1,
            sort: CatalogSearchSort::Relevance,
            field: None,
            language: None,
        }
    }
}

/// Options for [`crate::ContentSource::purchase_hint`].
#[derive(Debug, Clone, Default)]
pub struct PurchaseHintOpts {
    pub product_id: Option<String>,
    pub title: Option<String>,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub region: String,
    /// When true, resolve live price if the source can.
    pub with_price: bool,
}
