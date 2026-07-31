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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// How this was found (`related`, `series`, `author`, `search`, `top_deals`, …).
    pub origin: String,
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
    pub price_cents: Option<i64>,
    pub currency: Option<String>,
    pub price_label: Option<String>,
}

/// Options for [`crate::ContentSource::search_catalog`].
#[derive(Debug, Clone, Default)]
pub struct CatalogSearchOpts {
    pub query: String,
    pub region: String,
    pub limit: usize,
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
