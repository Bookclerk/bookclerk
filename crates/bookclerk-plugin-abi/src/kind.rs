//! Kind-specific Workers RPC DTOs (source / integration / output).
//!
//! Field names serialize as **camelCase** on the wire. Tagged enums keep their
//! discriminant rename policy ([`SourceFetchDto`] variant tags stay
//! `snake_case`; database connect tags live in [`crate::db::DbConnectParams`]).
//!
//! | Kind | Typical methods |
//! | --- | --- |
//! | `source` | [`crate::methods::login`], [`crate::methods::scan`], [`crate::methods::fetch_title`], catalog helpers |
//! | `integration` | [`crate::methods::start`], [`crate::methods::poll_events`], [`crate::methods::sync_listening`] |
//! | `output` | [`crate::methods::put`], [`crate::methods::put_file`], [`crate::methods::get`], … |
//!
//! Untagged enums such as [`OutputPutParams`] accept either the local or S3
//! param shape so the host can share one dispatcher across destinations.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default `true` for `scan_enabled`, `import_episodes`, and `import_plus_titles` when omitted.
fn default_true() -> bool {
    true
}

/// Default storefront page size (`50`) for [`ScanParams::page_size`].
fn default_page() -> u32 {
    50
}

/// Default catalog hit cap (`20`) for search and expand requests.
fn default_catalog_limit() -> usize {
    20
}

/// Default 1-based catalog page when the wire field is omitted.
fn default_catalog_page() -> u32 {
    1
}

/// Book-acquired notify payload used by some host→plugin paths (opaque book JSON).
///
/// Prefer [`crate::events::BookAcquiredPayload`] for the typed
/// [`crate::methods::on_event`] envelope. This DTO carries a fuller library-row
/// JSON plus storage location for guests that need the raw book document.
///
/// `book` is opaque JSON shaped like the host library row; guests may deserialize
/// a subset without depending on `bookclerk-library`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookAcquiredDto {
    /// Opaque host library-row JSON for the acquired title.
    pub book: Value,
    /// Destination object key where the primary media was written (wire
    /// `storageKey`).
    pub storage_key: String,
    /// Absolute filesystem path when the destination is local; omitted/`None`
    /// for remote-only backends (wire `absolutePath`).
    #[serde(default)]
    pub absolute_path: Option<String>,
}

/// Source account metadata returned from login / list-accounts and stored by the host.
///
/// Produced by [`crate::methods::login`] / [`crate::methods::login_complete`]
/// inside [`LoginResultDto::account`]. The host upserts the row; guests must
/// not open `library.db`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceAccountDto {
    /// Stable account id within this source plugin (wire `accountId`).
    pub account_id: String,
    /// Source plugin id (host may force this to the guest's install id).
    pub source: String,
    /// Storefront marketplace / region code (for example `us`, `uk`).
    pub marketplace: String,
    /// Operator-facing label; omitted when the guest has none.
    #[serde(default)]
    pub label: Option<String>,
    /// When true, bare/`scheduled` scans include this account (default `true`;
    /// wire `scanEnabled`). Explicit CLI `--account` bypasses this flag.
    #[serde(default = "default_true")]
    pub scan_enabled: bool,
}

/// Params for [`crate::methods::login`] (and alias [`LoginStartParams`]).
///
/// Password sources fill email/password; OAuth sources use callback / external
/// fields. There is no files-dir root or library DB path — only
/// [`Self::plugin_data_dir`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginParams {
    /// Scoped writable directory for this plugin only (`…/plugins/<id>/data`,
    /// wire `pluginDataDir`).
    pub plugin_data_dir: String,
    /// Marketplace / locale for the storefront (default empty → guest default).
    #[serde(default)]
    pub marketplace: String,
    /// Optional operator label stored on the account row.
    #[serde(default)]
    pub label: Option<String>,
    /// Account email / username for password logins; omitted for pure OAuth.
    #[serde(default)]
    pub email: Option<String>,
    /// Account password for password logins; never logged; omitted for OAuth.
    #[serde(default)]
    pub password: Option<String>,
    /// When true, overwrite an existing sealed credential for this account.
    #[serde(default)]
    pub force: bool,
    /// Optional bind address for OAuth callback servers (`host:port`).
    /// Ignored when [`Self::callback_ipc`] is set (host owns the TCP listener).
    #[serde(default)]
    pub callback_bind: Option<String>,
    /// Host-owned callback IPC endpoint the guest must connect to.
    ///
    /// When set (with [`Self::callback_public_base`]), the guest must **not**
    /// bind a TCP listener. The host accepts browser connections and forwards
    /// raw bytes over this duplex IPC (Unix socket path or Windows pipe name).
    #[serde(default)]
    pub callback_ipc: Option<String>,
    /// Public base URL for the host TCP listener, e.g. `http://127.0.0.1:12345`.
    /// Combined with the guest's landing path to form the browser URL
    /// (wire `callbackPublicBase`).
    #[serde(default)]
    pub callback_public_base: Option<String>,
    /// When true, use external / paste-redirect OAuth instead of a local
    /// callback server.
    #[serde(default)]
    pub external: bool,
    /// Pre-supplied OAuth redirect URL (paste flow); omitted otherwise
    /// (wire `responseUrl`).
    #[serde(default)]
    pub response_url: Option<String>,
    /// Prefer QR output when the guest supports it (wire `showQr`).
    #[serde(default)]
    pub show_qr: bool,
    /// Seconds to wait for OAuth callback capture; guest default when `None`
    /// (wire `timeoutSecs`).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Store-specific knobs as a JSON object; guests may ignore unknowns.
    #[serde(default)]
    pub extra: Value,
}

/// Result of [`crate::methods::login`] / [`crate::methods::login_complete`].
///
/// Account metadata plus opaque credentials for the host to seal into
/// `encrypted_secrets` (`provider = plugin id`). Guests never write secrets
/// into the library DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResultDto {
    /// Account row fields for the host to upsert.
    pub account: SourceAccountDto,
    /// Opaque JSON credential blob. Host seals into `encrypted_secrets`.
    /// `None` when login only refreshed metadata.
    #[serde(default)]
    pub credentials: Option<Value>,
}

/// Result of [`crate::methods::login_start`] (interactive OAuth).
///
/// Operator opens [`Self::url`]; later [`crate::methods::login_complete`] uses
/// [`Self::session_id`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStartResultDto {
    /// Opaque session id for [`crate::methods::login_complete`] (wire
    /// `sessionId`).
    pub session_id: String,
    /// Browser URL the operator should open to complete OAuth.
    pub url: String,
}

/// Params for [`crate::methods::login_complete`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCompleteParams {
    /// Session id previously returned by [`LoginStartResultDto::session_id`]
    /// (wire `sessionId`).
    pub session_id: String,
}

/// Params for [`crate::methods::credentials_update`] — guest-requested
/// credential write-back after a silent refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsUpdateParams {
    /// Account whose sealed blob should be replaced (wire `accountId`).
    pub account_id: String,
    /// Replacement opaque credential JSON for the host to re-seal.
    pub credentials: Value,
}

/// One external user observed by an integration (ABS-style concerns; host workflows).
///
/// Returned from [`crate::methods::authenticate_user`] / [`crate::methods::poll_events`]
/// paths; the host may mint claim tickets without exposing portal details to
/// the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalUserDto {
    /// Integration provider id (often the plugin id).
    pub provider: String,
    /// Provider-scoped user id (wire `externalUserId`).
    pub external_user_id: String,
    /// Optional display name for UI.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Ephemeral remote token (e.g. ABS JWT). Guest→host only; never persisted.
    /// Omitted from JSON when absent (wire `accessToken`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
}

/// Result of [`crate::methods::poll_events`] — signals for the host to kick off workflows.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EventPollResultDto {
    /// Newly observed external users since the last poll (default empty).
    #[serde(default)]
    pub users: Vec<ExternalUserDto>,
}

/// Params for [`crate::methods::scan`].
///
/// No library DB path. Host injects sealed credentials (same mediation as
/// [`crate::methods::fetch_title`]) so the plugin does not need a private
/// credential store under `plugin_data_dir`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanParams {
    /// Scoped plugin data directory (wire `pluginDataDir`).
    pub plugin_data_dir: String,
    /// Account ids to scan; empty means all scan-enabled accounts known to the
    /// guest/host pairing.
    #[serde(default)]
    pub accounts: Vec<String>,
    /// Storefront page size (default `50`; wire `pageSize`).
    #[serde(default = "default_page")]
    pub page_size: u32,
    /// When true, import podcast/episode-style rows (default `true`; wire
    /// `importEpisodes`).
    #[serde(default = "default_true")]
    pub import_episodes: bool,
    /// When true, import Plus/catalog entitlement titles (default `true`; wire
    /// `importPlusTitles`).
    #[serde(default = "default_true")]
    pub import_plus_titles: bool,
    /// Host-loaded credential blobs keyed by `account_id` (wire `credentials`).
    ///
    /// Values are the same opaque JSON sealed at login. Empty when the host
    /// has no credentials for the requested accounts.
    #[serde(default)]
    pub credentials: std::collections::BTreeMap<String, Value>,
}

/// One library title returned by an external source [`crate::methods::scan`].
///
/// The host upserts these rows and forces `source` to the plugin id. Prefer
/// returning titles in [`ScanSummaryDto::books`] over plugin-side DB writes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanBookDto {
    /// Account that owns this library entry (wire `accountId`).
    pub account_id: String,
    /// Storefront product / SKU id (wire `productId`).
    pub product_id: String,
    /// Primary title string.
    pub title: String,
    /// Marketplace / region when known.
    #[serde(default)]
    pub marketplace: Option<String>,
    /// Amazon ASIN when the storefront exposes one.
    #[serde(default)]
    pub asin: Option<String>,
    /// ISBN when the storefront exposes one.
    #[serde(default)]
    pub isbn: Option<String>,
    /// Comma- or guest-formatted author list.
    #[serde(default)]
    pub authors: Option<String>,
    /// Comma- or guest-formatted narrator list.
    #[serde(default)]
    pub narrators: Option<String>,
    /// Series name when applicable.
    #[serde(default)]
    pub series: Option<String>,
    /// Series index / sequence label (wire `seriesIndex`).
    #[serde(default)]
    pub series_index: Option<String>,
    /// Content classification (e.g. book vs episode; wire `contentKind`).
    #[serde(default)]
    pub content_kind: Option<String>,
    /// Publisher name when known.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Runtime in whole minutes when known (wire `lengthMinutes`).
    #[serde(default)]
    pub length_minutes: Option<i64>,
    /// Subtitle when distinct from [`Self::title`].
    #[serde(default)]
    pub subtitle: Option<String>,
}

/// Summary result of [`crate::methods::scan`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummaryDto {
    /// Number of accounts touched during the scan.
    #[serde(default)]
    pub accounts: usize,
    /// Count of titles the guest expects the host to upsert (wire
    /// `booksUpserted`); may mirror [`Self::books`].len().
    #[serde(default)]
    pub books_upserted: usize,
    /// Number of storefront pages fetched.
    #[serde(default)]
    pub pages: u32,
    /// Accounts skipped because `scan_enabled` was false (wire
    /// `skippedDisabled`).
    #[serde(default)]
    pub skipped_disabled: usize,
    /// Titles for the host to upsert. Prefer this over plugin-side DB writes.
    #[serde(default)]
    pub books: Vec<ScanBookDto>,
}

/// Params for [`crate::methods::fetch_title`].
///
/// Plugin writes media under [`Self::cache_dir`] and returns plain (DRM-free)
/// paths in [`SourceFetchDto`]. Host injects credentials; guests must not open
/// `library.db` or `master.key`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchTitleParams {
    /// Scoped plugin data directory (wire `pluginDataDir`).
    pub plugin_data_dir: String,
    /// Account whose credentials apply (wire `accountId`).
    pub account_id: String,
    /// Library / storefront title id to download (wire `titleId`).
    pub title_id: String,
    /// Absolute path to the host download cache for this fetch (wire
    /// `cacheDir`). Guests write media here.
    pub cache_dir: String,
    /// Host-loaded credential blob for this account (sealed in DB; plugin never
    /// opens DB). `None` when unavailable.
    #[serde(default)]
    pub credentials: Option<Value>,
    /// Opaque plugin table from `[sources.<id>]` (wire `sourceConfig`).
    #[serde(default)]
    pub source_config: Value,
    /// Host acquire/download options (JSON object matching host
    /// `DownloadOptions`).
    ///
    /// Guests should honor fetch-relevant knobs (`widevine`,
    /// `strip_audible_brand_audio`, `download_cover`, `chapter_layout`, speed
    /// limits, …) so external load matches in-process. Plugin-specific overlays
    /// (e.g. `[sources.audible].bitrate`) still come from
    /// [`Self::source_config`].
    #[serde(default)]
    pub download: Value,
}

/// Plain (DRM-free) fetch result from [`crate::methods::fetch_title`].
///
/// Wire tag is `type` with `snake_case` variants. Sources always return
/// decrypted/plain media — DRM guests decrypt before responding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceFetchDto {
    /// Successfully downloaded plain audio parts (`type: "plain"`).
    #[serde(rename_all = "camelCase")]
    Plain {
        /// Ordered audio part files written under the cache directory.
        parts: Vec<PlainPartDto>,
        /// Optional single M4B path when the guest assembled one (wire
        /// `m4bPath`).
        #[serde(default)]
        m4b_path: Option<String>,
        /// Optional cover image path under the cache directory (wire
        /// `coverPath`).
        #[serde(default)]
        cover_path: Option<String>,
        /// Chapter markers as `(title, start_ms)` pairs; empty when unknown.
        #[serde(default)]
        chapters: Vec<(String, u64)>,
        /// Companion PDF download URL when the store exposes one (wire
        /// `pdfUrl`).
        #[serde(default)]
        pdf_url: Option<String>,
    },
}

/// Params for [`crate::methods::catalog_detail`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CatalogDetailParams {
    /// Store product id (Libro ISBN or ISBN-slug; wire `productId`).
    pub product_id: String,
    /// Optional ISBN when it differs from [`Self::product_id`].
    #[serde(default)]
    pub isbn: Option<String>,
}

/// Params for [`crate::methods::search_catalog`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearchCatalogParams {
    /// Free-text search query.
    pub query: String,
    /// Storefront region / marketplace code (default empty → guest default).
    #[serde(default)]
    pub region: String,
    /// Maximum hits to return (default `20`).
    #[serde(default = "default_catalog_limit")]
    pub limit: usize,
    /// 1-based page for storefronts that page (default `1`).
    #[serde(default = "default_catalog_page")]
    pub page: u32,
    /// Sort key: `relevance` / `popularity` / `rating` / `title` / `author`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Optional facet (`author` / `narrator` / `series` / `genre`).
    #[serde(default)]
    pub field: Option<String>,
    /// Preferred content language (soft-prioritize; e.g. `en`).
    #[serde(default)]
    pub language: Option<String>,
}

/// Params for [`crate::methods::expand_candidates`].
///
/// Seed fields identify a known title; the guest returns related catalog hits.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExpandCandidatesParams {
    /// Source plugin id hint when expanding across storefronts.
    #[serde(default)]
    pub source: String,
    /// Seed storefront product id (wire `productId`).
    #[serde(default)]
    pub product_id: String,
    /// Seed title text.
    #[serde(default)]
    pub title: String,
    /// Seed authors string.
    #[serde(default)]
    pub authors: Option<String>,
    /// Seed narrators string.
    #[serde(default)]
    pub narrators: Option<String>,
    /// Seed series name.
    #[serde(default)]
    pub series: Option<String>,
    /// Seed series ASIN when known (wire `seriesAsin`).
    #[serde(default)]
    pub series_asin: Option<String>,
    /// Seed Amazon ASIN.
    #[serde(default)]
    pub asin: Option<String>,
    /// Seed ISBN.
    #[serde(default)]
    pub isbn: Option<String>,
    /// Storefront region / marketplace code.
    #[serde(default)]
    pub region: String,
    /// Maximum candidates to return (default `20`).
    #[serde(default = "default_catalog_limit")]
    pub limit: usize,
}

/// Params for [`crate::methods::purchase_hint`].
///
/// At least one identity field (`product_id` / `asin` / `isbn` / title+authors)
/// should be set; guests may return [`PluginErrorCode::InvalidParams`](crate::PluginErrorCode::InvalidParams)
/// when none are usable.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseHintParams {
    /// Storefront product id when known (wire `productId`).
    #[serde(default)]
    pub product_id: Option<String>,
    /// Title text for fuzzy lookup.
    #[serde(default)]
    pub title: Option<String>,
    /// Authors string for fuzzy lookup.
    #[serde(default)]
    pub authors: Option<String>,
    /// Amazon ASIN when known.
    #[serde(default)]
    pub asin: Option<String>,
    /// ISBN when known.
    #[serde(default)]
    pub isbn: Option<String>,
    /// Storefront region / marketplace code.
    #[serde(default)]
    pub region: String,
    /// When true, guests should include live price fields when available
    /// (wire `withPrice`).
    #[serde(default)]
    pub with_price: bool,
}

/// Wire form of a catalog / candidate hit.
///
/// Returned by [`crate::methods::search_catalog`],
/// [`crate::methods::expand_candidates`], and
/// [`crate::methods::catalog_detail`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CatalogHitDto {
    /// Storefront product / SKU id (wire `productId`).
    pub product_id: String,
    /// Primary title.
    pub title: String,
    /// Authors string when known.
    #[serde(default)]
    pub authors: Option<String>,
    /// Narrators string when known.
    #[serde(default)]
    pub narrators: Option<String>,
    /// Series name when applicable.
    #[serde(default)]
    pub series: Option<String>,
    /// Series index / sequence label (wire `seriesIndex`).
    #[serde(default)]
    pub series_index: Option<String>,
    /// Amazon ASIN when known.
    #[serde(default)]
    pub asin: Option<String>,
    /// ISBN when known.
    #[serde(default)]
    pub isbn: Option<String>,
    /// Storefront product page URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Cover image URL (wire `coverUrl`).
    #[serde(default)]
    pub cover_url: Option<String>,
    /// Hit origin label (plugin id or storefront name; default empty).
    #[serde(default)]
    pub origin: String,
    /// Subtitle when distinct from [`Self::title`].
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Long description / blurb when fetched.
    #[serde(default)]
    pub description: Option<String>,
    /// Publisher name when known.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Runtime in whole minutes (wire `lengthMinutes`).
    #[serde(default)]
    pub length_minutes: Option<i64>,
    /// Publication date string as provided by the storefront (wire
    /// `publishedAt`).
    #[serde(default)]
    pub published_at: Option<String>,
    /// Category / genre labels as a single string when known.
    #[serde(default)]
    pub categories: Option<String>,
    /// Content language code when known.
    #[serde(default)]
    pub language: Option<String>,
    /// Current price in minor units (cents; wire `priceCents`).
    #[serde(default)]
    pub price_cents: Option<i64>,
    /// ISO currency code for [`Self::price_cents`].
    #[serde(default)]
    pub currency: Option<String>,
    /// Pre-formatted price for display (wire `priceLabel`).
    #[serde(default)]
    pub price_label: Option<String>,
    /// Aggregate rating when known (wire `ratingOverall`).
    #[serde(default)]
    pub rating_overall: Option<f64>,
    /// Number of ratings when known (wire `ratingCount`).
    #[serde(default)]
    pub rating_count: Option<i64>,
    /// Whether the edition is abridged when the storefront says so (wire
    /// `isAbridged`).
    #[serde(default)]
    pub is_abridged: Option<bool>,
}

/// Wire form of a purchase hint from [`crate::methods::purchase_hint`].
///
/// Mirrors host `SourcePurchaseHint` for SPA / CLI purchase deep-links.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseHintDto {
    /// Storefront product id (wire `productId`).
    pub product_id: String,
    /// Title when resolved.
    #[serde(default)]
    pub title: Option<String>,
    /// Purchase or product-page URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Current price in minor units (wire `priceCents`).
    #[serde(default)]
    pub price_cents: Option<i64>,
    /// ISO currency code for price fields.
    #[serde(default)]
    pub currency: Option<String>,
    /// Pre-formatted current price (wire `priceLabel`).
    #[serde(default)]
    pub price_label: Option<String>,
    /// List / MSRP price in minor units (wire `listPriceCents`).
    #[serde(default)]
    pub list_price_cents: Option<i64>,
    /// Pre-formatted list price (wire `listPriceLabel`).
    #[serde(default)]
    pub list_price_label: Option<String>,
    /// Member / Plus price in minor units (wire `memberPriceCents`).
    #[serde(default)]
    pub member_price_cents: Option<i64>,
    /// Pre-formatted member price (wire `memberPriceLabel`).
    #[serde(default)]
    pub member_price_label: Option<String>,
}

/// One plain audio part under [`SourceFetchDto::Plain`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlainPartDto {
    /// Absolute path to the part file under the host cache directory.
    pub path: String,
    /// Optional part title (disc/chapter label).
    #[serde(default)]
    pub title: Option<String>,
    /// Duration of this part in milliseconds when known (wire `durationMs`).
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// Listening / progress rows returned by integration capability
/// [`crate::methods::sync_listening`].
///
/// Host upserts into the generic `listening_progress` table tagged with the
/// plugin id; plugins must not open the library DB themselves.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncListeningResultDto {
    /// Progress snapshots to upsert (default empty).
    #[serde(default)]
    pub items: Vec<ListeningProgressDto>,
}

/// Wire DTO for one listening-progress row (serde-compatible with the host
/// `ListeningProgressSnapshot` shape).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListeningProgressDto {
    /// Provider-scoped user id (wire `externalUserId`).
    pub external_user_id: String,
    /// Provider-scoped item / library id (wire `externalItemId`).
    pub external_item_id: String,
    /// Optional Bookclerk identity row id when already linked (wire
    /// `identityId`).
    #[serde(default)]
    pub identity_id: Option<i64>,
    /// Title text when known.
    #[serde(default)]
    pub title: Option<String>,
    /// Authors string when known.
    #[serde(default)]
    pub authors: Option<String>,
    /// Amazon ASIN when known.
    #[serde(default)]
    pub asin: Option<String>,
    /// ISBN when known.
    #[serde(default)]
    pub isbn: Option<String>,
    /// Fractional progress in `0.0..=1.0` when the provider reports it.
    #[serde(default)]
    pub progress: Option<f64>,
    /// Current playback position in seconds (wire `currentTimeSeconds`).
    #[serde(default)]
    pub current_time_seconds: Option<f64>,
    /// Total duration in seconds when known (wire `durationSeconds`).
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    /// When true, the provider marks the item finished (wire `isFinished`).
    #[serde(default)]
    pub is_finished: bool,
    /// Last listen timestamp (UTC; wire `lastListenedAt`).
    #[serde(default)]
    pub last_listened_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Object metadata for output plugins (mirrors host `bookclerk_storage::ObjectMeta`).
///
/// Attached to [`crate::methods::put`] / [`crate::methods::put_file`] and
/// returned from [`crate::methods::probe`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMetaDto {
    /// MIME type when known (wire `contentType`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Object size in bytes when known (wire `contentLength`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
    /// Amazon ASIN metadata for the stored title when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// Title metadata for the stored object when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Creation timestamp string as understood by the destination (wire
    /// `creationTime`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_time: Option<String>,
    /// Last-write timestamp string as understood by the destination (wire
    /// `lastWriteTime`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_write_time: Option<String>,
}

/// Listing entry for output plugins from [`crate::methods::list`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectInfoDto {
    /// Object key relative to the destination root/prefix.
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
}

/// Probe result for output plugins from [`crate::methods::probe`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectProbeDto {
    /// Object key that was probed.
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// MIME type when known (wire `contentType`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Extended metadata for the object.
    #[serde(default)]
    pub meta: ObjectMetaDto,
}

/// Static AWS-style credentials injected by the host (never read from guest env).
///
/// [`Debug`] redacts secret fields. Present on [`OutputS3ContextDto`] when the
/// host has resolved keys from env or `encrypted_secrets`.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct S3CredentialsDto {
    /// Access key id (wire `accessKeyId`).
    pub access_key_id: String,
    /// Secret access key (wire `secretAccessKey`).
    pub secret_access_key: String,
    /// Optional session token for temporary credentials (wire `sessionToken`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

impl std::fmt::Debug for S3CredentialsDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3CredentialsDto")
            .field("access_key_id", &"***")
            .field("secret_access_key", &"***")
            .field("session_token", &self.session_token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// S3 destination knobs the host injects on every output RPC (flattened into
/// put/get/list params).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputS3ContextDto {
    /// Scoped writable directory for this plugin only (`…/plugins/<id>/data`,
    /// wire `pluginDataDir`).
    pub plugin_data_dir: String,
    /// Target bucket name.
    pub bucket: String,
    /// Key prefix under the bucket (`[output.s3].prefix`).
    pub prefix: String,
    /// AWS region (or compatible).
    pub region: String,
    /// Optional custom endpoint (MinIO / path-style hosts); host may prepend
    /// `https://` for bare hostnames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// When true, force path-style addressing (wire `forcePathStyle`).
    #[serde(default)]
    pub force_path_style: bool,
    /// When absent the guest may use the AWS SDK default provider chain
    /// (unconfined dev only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<S3CredentialsDto>,
}

/// Local filesystem destination knobs the host injects on every output RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputLocalContextDto {
    /// Scoped writable directory for this plugin only (`…/plugins/<id>/data`,
    /// wire `pluginDataDir`).
    pub plugin_data_dir: String,
    /// Library output root (`[output.local].root`).
    pub root: String,
    /// Key prefix under `root` (`[output.local].prefix`).
    pub prefix: String,
}

/// Params for [`crate::methods::put`] against a local filesystem output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalPutParams {
    /// Flattened local destination context (root / prefix / data dir).
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    /// Destination object key under `root`/`prefix`.
    pub key: String,
    /// Base64-encoded object body for small objects (wire `dataBase64`).
    pub data_base64: String,
    /// Optional object metadata (default empty).
    #[serde(default)]
    pub meta: ObjectMetaDto,
}

/// Params for [`crate::methods::put_file`] against a local filesystem output.
///
/// Large files arrive via FD side channel when confined; otherwise
/// [`Self::local_path`] is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalPutFileParams {
    /// Flattened local destination context.
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    /// Destination object key.
    pub key: String,
    /// Optional object metadata (default empty).
    #[serde(default)]
    pub meta: ObjectMetaDto,
    /// Absolute path to the source file when no side channel is wired
    /// (wire `localPath`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

/// Params for [`crate::methods::get`] against a local filesystem output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalGetParams {
    /// Flattened local destination context.
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    /// Object key to read.
    pub key: String,
}

/// Params for key-scoped local output methods (`exists` / `probe` / `delete`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalKeyParams {
    /// Flattened local destination context.
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    /// Object key to operate on.
    pub key: String,
}

/// Params for [`crate::methods::list`] against a local filesystem output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalListParams {
    /// Flattened local destination context.
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    /// Key prefix filter within the destination.
    pub prefix: String,
}

/// Params for [`crate::methods::copy`] against a local filesystem output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCopyParams {
    /// Flattened local destination context.
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    /// Source object key.
    pub from: String,
    /// Destination object key.
    pub to: String,
}

/// Params for [`crate::methods::touch_file`] against a local filesystem output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTouchFileParams {
    /// Flattened local destination context.
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    /// Object key whose timestamps should be updated.
    pub key: String,
    /// Optional creation time string for the destination backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// Optional modification time string for the destination backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

/// Params for [`crate::methods::put`] against an S3-compatible output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutParams {
    /// Flattened S3 destination context (bucket / credentials / …).
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    /// Object key under the bucket prefix.
    pub key: String,
    /// Base64-encoded object body (sidecars and small objects; wire
    /// `dataBase64`).
    pub data_base64: String,
    /// Optional object metadata (default empty).
    #[serde(default)]
    pub meta: ObjectMetaDto,
}

/// Params for [`crate::methods::put_file`] against an S3-compatible output.
///
/// Jailed guests receive the local file over the side channel (`SCM_RIGHTS` on
/// the socket at fd 3) immediately before this RPC. When no side channel is
/// wired (e.g. unconfined / best-effort), the host sets [`Self::local_path`]
/// instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutFileParams {
    /// Flattened S3 destination context.
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    /// Object key under the bucket prefix.
    pub key: String,
    /// Optional object metadata (default empty).
    #[serde(default)]
    pub meta: ObjectMetaDto,
    /// Absolute path to the source file when no side channel is wired
    /// (wire `localPath`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

/// Params for [`crate::methods::get`] against an S3-compatible output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetParams {
    /// Flattened S3 destination context.
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    /// Object key to download.
    pub key: String,
}

/// Result of [`crate::methods::get`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResultDto {
    /// Base64-encoded object body (wire `dataBase64`).
    pub data_base64: String,
}

/// Params for key-scoped S3 output methods (`exists` / `probe` / `delete`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyParams {
    /// Flattened S3 destination context.
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    /// Object key to operate on.
    pub key: String,
}

/// Params for [`crate::methods::list`] against an S3-compatible output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    /// Flattened S3 destination context.
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    /// Key prefix filter within the bucket/prefix.
    pub prefix: String,
}

/// Params for [`crate::methods::copy`] against an S3-compatible output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyParams {
    /// Flattened S3 destination context.
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    /// Source object key.
    pub from: String,
    /// Destination object key.
    pub to: String,
}

/// Params for [`crate::methods::touch_file`] against an S3-compatible output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TouchFileParams {
    /// Flattened S3 destination context.
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    /// Object key whose metadata timestamps should be updated.
    pub key: String,
    /// Optional creation time string for backends that support it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// Optional modification time string for backends that support it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

/// Wire params for [`crate::methods::put`] — local or S3 destination shape.
///
/// Untagged: serde picks [`LocalPutParams`] vs [`PutParams`] by field set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputPutParams {
    /// Local filesystem destination params.
    Local(LocalPutParams),
    /// S3-compatible destination params.
    S3(PutParams),
}

/// Wire params for [`crate::methods::put_file`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputPutFileParams {
    /// Local filesystem destination params.
    Local(LocalPutFileParams),
    /// S3-compatible destination params.
    S3(PutFileParams),
}

/// Wire params for [`crate::methods::get`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputGetParams {
    /// Local filesystem destination params.
    Local(LocalGetParams),
    /// S3-compatible destination params.
    S3(GetParams),
}

/// Wire params for key-scoped output methods (`exists` / `probe` / `delete`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputKeyParams {
    /// Local filesystem destination params.
    Local(LocalKeyParams),
    /// S3-compatible destination params.
    S3(KeyParams),
}

/// Wire params for [`crate::methods::list`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputListParams {
    /// Local filesystem destination params.
    Local(LocalListParams),
    /// S3-compatible destination params.
    S3(ListParams),
}

/// Wire params for [`crate::methods::copy`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputCopyParams {
    /// Local filesystem destination params.
    Local(LocalCopyParams),
    /// S3-compatible destination params.
    S3(CopyParams),
}

/// Wire params for [`crate::methods::touch_file`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputTouchFileParams {
    /// Local filesystem destination params.
    Local(LocalTouchFileParams),
    /// S3-compatible destination params.
    S3(TouchFileParams),
}

/// Result of [`crate::methods::exists`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExistsResultDto {
    /// When true, the object key is present in the destination.
    pub exists: bool,
}

/// Params for [`crate::methods::login_start`] — same shape as [`LoginParams`].
pub type LoginStartParams = LoginParams;

/// Params for [`crate::methods::scan_library`] (integration remote library sync).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanLibraryParams {
    /// When true, force a full rescan even if the guest would otherwise
    /// incremental-sync.
    #[serde(default)]
    pub force: bool,
}

/// Params for [`crate::methods::authenticate_user`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateUserParams {
    /// Integration username / login id.
    pub username: String,
    /// Integration password; never logged by the host.
    pub password: String,
}

/// Params for [`crate::methods::list_deals`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListDealsParams {
    /// Optional maximum number of deals to return; guest default when `None`.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Params for [`crate::methods::list_accounts`] (currently no fields; reserved).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListAccountsParams {}
