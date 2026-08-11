//! Kind-specific Workers RPC DTOs (source / integration / output).
//!
//! Field names serialize as camelCase on the wire. Tagged enums keep their
//! discriminant rename policy (`SourceFetchDto` variant tags stay
//! `snake_case`; `DbConnectParams` backend tags stay `lowercase`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_true() -> bool {
    true
}

fn default_page() -> u32 {
    50
}

fn default_catalog_limit() -> usize {
    20
}

fn default_catalog_page() -> u32 {
    1
}

/// Book-acquired event payload (host → plugin).
///
/// `book` is opaque JSON shaped like the host library row; guests may deserialize
/// a subset without depending on `bookclerk-library`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookAcquiredDto {
    pub book: Value,
    pub storage_key: String,
    #[serde(default)]
    pub absolute_path: Option<String>,
}

/// Source account DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceAccountDto {
    pub account_id: String,
    pub source: String,
    pub marketplace: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default = "default_true")]
    pub scan_enabled: bool,
}

/// Login params for source plugins (no files-dir root / DB path).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginParams {
    /// Scoped writable directory for this plugin only (`…/plugins/<id>/data`).
    pub plugin_data_dir: String,
    #[serde(default)]
    pub marketplace: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
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
    /// Combined with the guest's landing path to form the browser URL.
    #[serde(default)]
    pub callback_public_base: Option<String>,
    /// External / paste-redirect OAuth instead of a local callback server.
    #[serde(default)]
    pub external: bool,
    /// Pre-supplied OAuth redirect URL.
    #[serde(default)]
    pub response_url: Option<String>,
    /// Prefer QR output when the guest supports it.
    #[serde(default)]
    pub show_qr: bool,
    /// Seconds to wait for OAuth callback capture.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Store-specific knobs; guests may ignore unknowns.
    #[serde(default)]
    pub extra: Value,
}

/// Login result — account metadata plus opaque credentials for the host to seal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResultDto {
    pub account: SourceAccountDto,
    /// Opaque JSON credential blob. Host seals into `encrypted_secrets`
    /// (`provider = plugin id`). Never written by the plugin into the library DB.
    #[serde(default)]
    pub credentials: Option<Value>,
}

/// Result of [`methods::LOGIN_START`] (interactive OAuth).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStartResultDto {
    /// Opaque session id for [`methods::LOGIN_COMPLETE`].
    pub session_id: String,
    /// Browser URL the operator should open.
    pub url: String,
}

/// Params for [`methods::LOGIN_COMPLETE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCompleteParams {
    pub session_id: String,
}

/// Params for [`methods::CREDENTIALS_UPDATE`] — guest-requested credential write-back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsUpdateParams {
    pub account_id: String,
    /// Replacement opaque credential JSON for the host to re-seal.
    pub credentials: Value,
}

/// One external user observed by an integration (ABS-only concerns; host workflows).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalUserDto {
    pub provider: String,
    pub external_user_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Ephemeral remote token (e.g. ABS JWT). Guest→host only; never persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
}

/// Result of [`methods::EVENT_POLL`] — signals for the host to kick off workflows.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EventPollResultDto {
    #[serde(default)]
    pub users: Vec<ExternalUserDto>,
}

/// Scan params — no library DB path.
///
/// Host injects sealed credentials (same mediation as `fetch_title`) so the
/// plugin does not need a private credential store under `plugin_data_dir`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanParams {
    pub plugin_data_dir: String,
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default = "default_page")]
    pub page_size: u32,
    #[serde(default = "default_true")]
    pub import_episodes: bool,
    #[serde(default = "default_true")]
    pub import_plus_titles: bool,
    /// Host-loaded credential blobs keyed by `account_id`.
    ///
    /// Values are the same opaque JSON sealed at `login`. Empty when the host
    /// has no credentials for the requested accounts.
    #[serde(default)]
    pub credentials: std::collections::BTreeMap<String, Value>,
}

/// One library title returned by an external source `scan` (host upserts).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanBookDto {
    pub account_id: String,
    pub product_id: String,
    pub title: String,
    #[serde(default)]
    pub marketplace: Option<String>,
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
    #[serde(default)]
    pub authors: Option<String>,
    #[serde(default)]
    pub narrators: Option<String>,
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub series_index: Option<String>,
    #[serde(default)]
    pub content_kind: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub length_minutes: Option<i64>,
    #[serde(default)]
    pub subtitle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummaryDto {
    #[serde(default)]
    pub accounts: usize,
    #[serde(default)]
    pub books_upserted: usize,
    #[serde(default)]
    pub pages: u32,
    #[serde(default)]
    pub skipped_disabled: usize,
    /// Titles for the host to upsert. Prefer this over plugin-side DB writes.
    #[serde(default)]
    pub books: Vec<ScanBookDto>,
}

/// Fetch-title params. Plugin writes media under `cache_dir` and returns paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchTitleParams {
    pub plugin_data_dir: String,
    pub account_id: String,
    pub title_id: String,
    pub cache_dir: String,
    /// Host-loaded credential blob for this account (sealed in DB; plugin never opens DB).
    #[serde(default)]
    pub credentials: Option<Value>,
    /// Opaque plugin table from `[sources.<id>]`.
    #[serde(default)]
    pub source_config: Value,
    /// Host acquire/download options (JSON object matching host `DownloadOptions`).
    ///
    /// Guests should honor fetch-relevant knobs (`widevine`, `strip_audible_brand_audio`,
    /// `download_cover`, `chapter_layout`, speed limits, …) so external load matches
    /// in-process. Plugin-specific overlays (e.g. `[sources.audible].bitrate`) still
    /// come from [`Self::source_config`].
    #[serde(default)]
    pub download: Value,
}

/// Plain (DRM-free) fetch result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceFetchDto {
    #[serde(rename_all = "camelCase")]
    Plain {
        parts: Vec<PlainPartDto>,
        #[serde(default)]
        m4b_path: Option<String>,
        #[serde(default)]
        cover_path: Option<String>,
        #[serde(default)]
        chapters: Vec<(String, u64)>,
        /// Companion PDF download URL when the store exposes one.
        #[serde(default)]
        pdf_url: Option<String>,
    },
}

/// Params for [`methods::CATALOG_DETAIL`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CatalogDetailParams {
    /// Store product id (Libro ISBN or ISBN-slug).
    pub product_id: String,
    /// Optional ISBN when it differs from [`Self::product_id`].
    #[serde(default)]
    pub isbn: Option<String>,
}

/// Params for [`methods::SEARCH_CATALOG`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearchCatalogParams {
    pub query: String,
    #[serde(default)]
    pub region: String,
    #[serde(default = "default_catalog_limit")]
    pub limit: usize,
    /// 1-based page for storefronts that page (default 1).
    #[serde(default = "default_catalog_page")]
    pub page: u32,
    /// `relevance` / `popularity` / `rating` / `title` / `author`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Optional facet (`author` / `narrator` / `series` / `genre`).
    #[serde(default)]
    pub field: Option<String>,
    /// Preferred content language (soft-prioritize; e.g. `en`).
    #[serde(default)]
    pub language: Option<String>,
}

/// Params for [`methods::EXPAND_CANDIDATES`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExpandCandidatesParams {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub product_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub authors: Option<String>,
    #[serde(default)]
    pub narrators: Option<String>,
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub series_asin: Option<String>,
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
    #[serde(default)]
    pub region: String,
    #[serde(default = "default_catalog_limit")]
    pub limit: usize,
}

/// Params for [`methods::PURCHASE_HINT`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseHintParams {
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub authors: Option<String>,
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub with_price: bool,
}

/// Wire form of a catalog / candidate hit.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CatalogHitDto {
    pub product_id: String,
    pub title: String,
    #[serde(default)]
    pub authors: Option<String>,
    #[serde(default)]
    pub narrators: Option<String>,
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub series_index: Option<String>,
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
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
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub categories: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub price_cents: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub price_label: Option<String>,
    #[serde(default)]
    pub rating_overall: Option<f64>,
    #[serde(default)]
    pub rating_count: Option<i64>,
    #[serde(default)]
    pub is_abridged: Option<bool>,
}

/// Wire form of [`SourcePurchaseHint`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseHintDto {
    pub product_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub price_cents: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub price_label: Option<String>,
    #[serde(default)]
    pub list_price_cents: Option<i64>,
    #[serde(default)]
    pub list_price_label: Option<String>,
    #[serde(default)]
    pub member_price_cents: Option<i64>,
    #[serde(default)]
    pub member_price_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlainPartDto {
    pub path: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// Listening / progress rows returned by integration capability `sync_listening`.
///
/// Host upserts into the generic `listening_progress` table tagged with the
/// plugin id; plugins must not open the library DB themselves.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncListeningResultDto {
    #[serde(default)]
    pub items: Vec<ListeningProgressDto>,
}

/// Wire DTO for one listening-progress row (serde-compatible with the host
/// `ListeningProgressSnapshot` shape).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListeningProgressDto {
    pub external_user_id: String,
    pub external_item_id: String,
    #[serde(default)]
    pub identity_id: Option<i64>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub authors: Option<String>,
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default)]
    pub current_time_seconds: Option<f64>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub is_finished: bool,
    #[serde(default)]
    pub last_listened_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Object metadata for output plugins (mirrors host [`bookclerk_storage::ObjectMeta`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMetaDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_write_time: Option<String>,
}

/// Listing entry for output plugins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectInfoDto {
    pub key: String,
    pub size: u64,
}

/// Probe result for output plugins.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectProbeDto {
    pub key: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub meta: ObjectMetaDto,
}

/// Static AWS-style credentials injected by the host (never read from guest env).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct S3CredentialsDto {
    pub access_key_id: String,
    pub secret_access_key: String,
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

/// S3 destination knobs the host injects on every output RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputS3ContextDto {
    /// Scoped writable directory for this plugin only (`…/plugins/<id>/data`).
    pub plugin_data_dir: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub force_path_style: bool,
    /// When absent the guest may use the AWS SDK default provider chain (unconfined dev only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<S3CredentialsDto>,
}

/// Local filesystem destination knobs the host injects on every output RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputLocalContextDto {
    /// Scoped writable directory for this plugin only (`…/plugins/<id>/data`).
    pub plugin_data_dir: String,
    /// Library output root (`[output.local].root`).
    pub root: String,
    /// Key prefix under `root` (`[output.local].prefix`).
    pub prefix: String,
}

/// Params for [`methods::PUT`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalPutParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub key: String,
    pub data_base64: String,
    #[serde(default)]
    pub meta: ObjectMetaDto,
}

/// Params for [`methods::PUT_FILE`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalPutFileParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub key: String,
    #[serde(default)]
    pub meta: ObjectMetaDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

/// Params for [`methods::GET`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalGetParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub key: String,
}

/// Params for key-scoped local output methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalKeyParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub key: String,
}

/// Params for [`methods::LIST`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalListParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub prefix: String,
}

/// Params for [`methods::COPY`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCopyParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub from: String,
    pub to: String,
}

/// Params for [`methods::TOUCH_FILE`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTouchFileParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

/// Params for [`methods::PUT`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub key: String,
    /// Base64-encoded object body (sidecars and small objects).
    pub data_base64: String,
    #[serde(default)]
    pub meta: ObjectMetaDto,
}

/// Params for [`methods::PUT_FILE`].
///
/// Jailed guests receive the local file over the side channel (`SCM_RIGHTS` on
/// the socket at fd 3) immediately before this RPC. When no side channel is
/// wired (e.g. unconfined / best-effort), the host sets [`Self::local_path`]
/// instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutFileParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub key: String,
    #[serde(default)]
    pub meta: ObjectMetaDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

/// Params for [`methods::GET`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub key: String,
}

/// Result of [`methods::GET`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResultDto {
    pub data_base64: String,
}

/// Params for key-scoped output methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub key: String,
}

/// Params for [`methods::LIST`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub prefix: String,
}

/// Params for [`methods::COPY`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub from: String,
    pub to: String,
}

/// Params for [`methods::TOUCH_FILE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TouchFileParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

/// Wire params for [`methods::PUT`] — local or S3 destination shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputPutParams {
    Local(LocalPutParams),
    S3(PutParams),
}

/// Wire params for [`methods::PUT_FILE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputPutFileParams {
    Local(LocalPutFileParams),
    S3(PutFileParams),
}

/// Wire params for [`methods::GET`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputGetParams {
    Local(LocalGetParams),
    S3(GetParams),
}

/// Wire params for key-scoped output methods (`exists` / `probe` / `delete`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputKeyParams {
    Local(LocalKeyParams),
    S3(KeyParams),
}

/// Wire params for [`methods::LIST`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputListParams {
    Local(LocalListParams),
    S3(ListParams),
}

/// Wire params for [`methods::COPY`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputCopyParams {
    Local(LocalCopyParams),
    S3(CopyParams),
}

/// Wire params for [`methods::TOUCH_FILE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputTouchFileParams {
    Local(LocalTouchFileParams),
    S3(TouchFileParams),
}

/// Result of [`methods::EXISTS`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExistsResultDto {
    pub exists: bool,
}

/// Params for [`crate::methods::login_start`] — same shape as [`LoginParams`].
pub type LoginStartParams = LoginParams;

/// Params for [`crate::methods::scan_library`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanLibraryParams {
    #[serde(default)]
    pub force: bool,
}

/// Params for [`crate::methods::authenticate_user`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateUserParams {
    pub username: String,
    pub password: String,
}

/// Params for [`crate::methods::list_deals`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListDealsParams {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Params for [`crate::methods::list_accounts`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListAccountsParams {}
