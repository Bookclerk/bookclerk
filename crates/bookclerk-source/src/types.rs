//! Shared DTOs for content-source auth, scan, fetch, and catalog APIs.
//!
//! # Audience
//!
//! Host job runners and [`crate::ContentSource`] implementors.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::options::DownloadOptions;

/// One allowed value for a [`SourceConfigOption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigOptionValue {
    /// Wire / TOML id (`high`, `m4b`, `web`, …).
    pub id: &'static str,
    /// Operator-facing display name for this choice in Settings / CLI help.
    pub label: &'static str,
}

/// One source-native config knob under `[sources.<id>]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceConfigOption {
    /// TOML key (`bitrate`, `container`, `access`).
    pub key: &'static str,
    /// Operator-facing display name for this knob in Settings.
    pub label: &'static str,
    /// Closed set of allowed wire values for [`Self::key`].
    pub values: &'static [ConfigOptionValue],
}

/// Account discovered or created by a content source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceAccount {
    /// Stable account id stored in the library (often email or store user id).
    pub account_id: String,
    /// Canonical plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Store marketplace / region code (`us`, `uk`, …).
    pub marketplace: String,
    /// Optional operator-facing nickname for CLI / UI selection.
    pub label: Option<String>,
    /// When false, scheduled / bare scans skip this account (explicit
    /// `--account` still includes it).
    pub scan_enabled: bool,
}

/// Options for interactive / CLI login.
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    /// Target marketplace / region for login (`us`, `uk`, …); empty = plugin default.
    pub marketplace: String,
    /// Optional nickname stored with the new account credentials.
    pub label: Option<String>,
    /// Email/password sources; ignored for OAuth.
    pub email: Option<String>,
    /// Email/password sources; ignored for OAuth.
    pub password: Option<String>,
    /// When true, overwrite an existing credential blob for the same account.
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
    LoginUrl {
        /// Absolute HTTPS URL for the browser / QR payload.
        url: String,
        /// Optional pre-rendered QR text for terminal UIs.
        qr: Option<String>,
    },
    /// Local callback server is listening (SSH port-forward hint).
    CallbackListening {
        /// Listen address (host:port) of the local redirect receiver.
        addr: String,
    },
    /// Waiting for the OAuth redirect / callback.
    WaitingForCallback,
    /// Login finished; credentials are persisted for `account_id`.
    Completed {
        /// Account id stored after a successful login.
        account_id: String,
    },
}

/// Options for a library scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Limit to specific account nicknames / ids.
    pub accounts: Vec<String>,
    /// Upstream library page size (titles per HTTP page; default 50).
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
    /// Number of accounts successfully scanned in this run.
    pub accounts: usize,
    /// Library rows inserted or updated.
    pub books_upserted: usize,
    /// Upstream library pages fetched across all accounts.
    pub pages: u32,
    /// Accounts skipped because [`crate::SourceAccount::scan_enabled`] was false.
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
    /// Packaging / naming knobs for this acquire (from `[output]` + destination).
    pub download: DownloadOptions,
    /// Absolute directory for scratch downloads for this fetch.
    pub cache_dir: PathBuf,
    /// Root files directory (`BOOKCLERK_FILES_DIR`). Used for CDM / Widevine path
    /// resolution by Audible. Non-auth operations only; auth is loaded from the
    /// [`bookclerk_library::LibraryStore`] passed directly to the trait method.
    pub files_dir: PathBuf,
}

/// One DRM-free audio part (chapter file or single book).
#[derive(Debug, Clone)]
pub struct PlainAudioPart {
    /// Absolute path to a clear (DRM-free) audio file on disk.
    pub path: PathBuf,
    /// Optional chapter / part title for packaging and metadata.
    pub title: Option<String>,
    /// Duration in milliseconds when known.
    pub duration_ms: Option<u64>,
}

/// DRM-free fetch result. Sources that use DRM decrypt inside the plugin and
/// return clear media here — the host never sees ciphertext or keys.
#[derive(Debug, Clone)]
pub struct PlainFetch {
    /// Ordered clear audio parts (single file or per-chapter downloads).
    pub parts: Vec<PlainAudioPart>,
    /// Pre-built M4B from the store / plugin when available.
    pub m4b_path: Option<PathBuf>,
    /// Absolute path to a downloaded cover image when available.
    pub cover_path: Option<PathBuf>,
    /// Chapter titles paired with start offsets in milliseconds.
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
    /// Store-native product / title id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Display title for the work.
    pub title: String,
    /// Author names (`;`-separated when multiple).
    pub authors: Option<String>,
    /// Narrator names (`;`-separated when multiple).
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    pub series: Option<String>,
    /// Position within the series (e.g. `"1"`, `"1-5"`).
    pub series_index: Option<String>,
    /// Amazon ASIN when known (may match [`Self::product_id`] for Audible).
    pub asin: Option<String>,
    /// ISBN-10/13 when known.
    pub isbn: Option<String>,
    /// Canonical storefront product URL when available.
    pub url: Option<String>,
    /// Public cover image URL when the storefront provides one.
    #[serde(default)]
    pub cover_url: Option<String>,
    /// How this was found (`related`, `series`, `author`, `search`, `top_deals`, …).
    #[serde(default)]
    pub origin: String,
    /// Subtitle / secondary title line when the storefront provides one.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Marketing / synopsis text from the storefront.
    #[serde(default)]
    pub description: Option<String>,
    /// Publisher imprint name when known.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports duration.
    #[serde(default)]
    pub length_minutes: Option<i64>,
    /// ISO-8601 / store release date string when known.
    #[serde(default)]
    pub published_at: Option<String>,
    /// Genre / subject labels (`;`-separated).
    #[serde(default)]
    pub categories: Option<String>,
    /// Content language (BCP-47 or storefront display name).
    #[serde(default)]
    pub language: Option<String>,
    /// List / deal price from the same catalog payload (optional).
    #[serde(default)]
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for [`Self::price_cents`] when priced.
    #[serde(default)]
    pub currency: Option<String>,
    /// Pre-formatted price string for UI (e.g. `$14.95`).
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
    /// Store-native product id of the owned / seed title.
    pub product_id: String,
    /// Display title used for related/series/author expansion queries.
    pub title: String,
    /// Author names from the seed library row.
    pub authors: Option<String>,
    /// Narrator names from the seed library row.
    pub narrators: Option<String>,
    /// Series name from the seed library row.
    pub series: Option<String>,
    /// Parent Audible series ASIN when known (from library metadata).
    pub series_asin: Option<String>,
    /// Amazon ASIN when known for Audible-related expansion.
    pub asin: Option<String>,
    /// ISBN when known for cross-store matching.
    pub isbn: Option<String>,
    /// Marketplace / catalog region (`us`, `uk`, …).
    pub region: String,
}

/// URL + optional live price for one storefront edition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcePurchaseHint {
    /// Store-native product id for the hinted edition.
    pub product_id: String,
    /// Display title when the storefront returns one with the hint.
    pub title: Option<String>,
    /// Deep link to buy / open the title on the storefront.
    pub url: Option<String>,
    /// Primary / best known sell price in minor units (prefer member when dual).
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for the primary price.
    pub currency: Option<String>,
    /// Pre-formatted primary price string for UI.
    pub price_label: Option<String>,
    /// Non-member / list / retail price when the storefront shows dual pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_cents: Option<i64>,
    /// Pre-formatted non-member / list price for dual-price storefronts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_label: Option<String>,
    /// Member / subscriber price when distinct from list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_cents: Option<i64>,
    /// Pre-formatted member / subscriber price when distinct from list.
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
    /// Scope search to titles by this author.
    Author,
    /// Scope search to titles narrated by this person.
    Narrator,
    /// Scope search to titles in this series.
    Series,
    /// Scope search to this genre / category facet.
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

    /// Serialize to the portal / query-string wire id.
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
    /// Storefront relevance ranking (default).
    #[default]
    Relevance,
    /// Audible `BestSellers` when available.
    Popularity,
    /// Audible `AvgRating` when available.
    Rating,
    /// Alphabetical by title (host or upstream).
    Title,
    /// Alphabetical by primary author (often host re-rank).
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
    /// Ascending order.
    Asc,
    /// Descending order (default for popularity / rating / price).
    #[default]
    Desc,
}

impl CatalogSortDir {
    /// Parse a wire / query-string direction (`asc` / `desc`; unknown → [`Self::Desc`]).
    #[must_use]
    pub fn from_wire(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "asc" | "ascending" => Self::Asc,
            _ => Self::Desc,
        }
    }

    /// Serialize to the portal / query-string wire id.
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
    /// Parse a wire / query-string sort id (unknown → [`Self::Relevance`]).
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

    /// Serialize to the portal / query-string wire id.
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
    /// Free-text or facet search string.
    pub query: String,
    /// Marketplace / catalog region (`us`, `uk`, …); empty = plugin default.
    pub region: String,
    /// Maximum hits to return (`0` = plugin default).
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
    /// Preferred store-native product id when already known.
    pub product_id: Option<String>,
    /// Title used to disambiguate when `product_id` is absent.
    pub title: Option<String>,
    /// Author names used to disambiguate catalog lookups.
    pub authors: Option<String>,
    /// Amazon ASIN used for Audible / cross-store hints.
    pub asin: Option<String>,
    /// ISBN used for cross-store hints.
    pub isbn: Option<String>,
    /// Marketplace / catalog region (`us`, `uk`, …); empty = plugin default.
    pub region: String,
    /// When true, resolve live price if the source can.
    pub with_price: bool,
}
