//! Shared JSON-RPC method names and payload types (api_version = 1).
//!
//! # Trust boundary
//!
//! External plugins are **untrusted** relative to the host. The host must never
//! hand them `library.db`, `master.key`, or the Bookclerk files-dir root. Plugins
//! receive only a scoped `plugin_data_dir` / `cache_dir`, and credentials are
//! host-mediated (login returns a blob the host seals; scan and fetch receive
//! that blob from the host). Scan returns book DTOs for the host to upsert.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current host↔plugin protocol version.
pub const PLUGIN_API_VERSION: u32 = 1;

/// Result of `handshake`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HandshakeResult {
    pub api_version: u32,
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Source plugins: oauth | password
    #[serde(default)]
    pub portal_auth_mode: Option<String>,
    #[serde(default)]
    pub password_env_var: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub sort_key: Option<u32>,
    #[serde(default)]
    pub brand: Option<BrandDto>,
    #[serde(default)]
    pub config_options: Vec<ConfigOptionDto>,
    /// Optional CLI command schema (also available via [`methods::CLI_DESCRIBE`]).
    #[serde(default)]
    pub cli: Option<CliSchema>,
}

/// Declared CLI surface for a plugin (`cli.describe` / handshake `cli` / `plugin.toml`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CliSchema {
    #[serde(default)]
    pub commands: Vec<CliCommandSpec>,
}

/// One plugin subcommand under `bookclerk plugins <id> <name>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliCommandSpec {
    pub name: String,
    #[serde(default)]
    pub about: Option<String>,
    #[serde(default)]
    pub args: Vec<CliArgSpec>,
}

/// Argument kind for declarative plugin CLI args.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CliArgKind {
    #[default]
    String,
    Bool,
    Int,
    Path,
}

/// One argument on a plugin CLI command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliArgSpec {
    pub name: String,
    #[serde(default)]
    pub long: Option<String>,
    #[serde(default)]
    pub short: Option<char>,
    #[serde(default)]
    pub kind: CliArgKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub about: Option<String>,
    /// When true, accept as a positional value (still keyed by `name` in invoke args).
    #[serde(default)]
    pub positional: bool,
}

/// Params for [`methods::CLI_INVOKE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliInvokeParams {
    pub command: String,
    #[serde(default)]
    pub args: serde_json::Map<String, Value>,
}

/// Result of [`methods::CLI_INVOKE`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CliInvokeResult {
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub json: Option<Value>,
}

/// Portal brand crossing the RPC boundary (owned strings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrandDto {
    pub id: String,
    pub name: String,
    pub bg: String,
    pub fg: String,
    pub accent: String,
    pub icon_url: String,
}

/// Config option discovery for sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigOptionDto {
    pub key: String,
    pub label: String,
    pub values: Vec<ConfigOptionValueDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigOptionValueDto {
    pub id: String,
    pub label: String,
}

/// Serializable health payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthDto {
    pub id: String,
    pub enabled: bool,
    pub ok: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Book-acquired event payload (host → plugin).
///
/// `book` is opaque JSON shaped like the host library row; guests may deserialize
/// a subset without depending on `bookclerk-library`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookAcquiredDto {
    pub book: Value,
    pub storage_key: String,
    #[serde(default)]
    pub absolute_path: Option<String>,
}

/// Source account DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceAccountDto {
    pub account_id: String,
    pub source: String,
    pub marketplace: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default = "default_true")]
    pub scan_enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Login params for source plugins (no files-dir root / DB path).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub callback_bind: Option<String>,
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
pub struct LoginResultDto {
    pub account: SourceAccountDto,
    /// Opaque JSON credential blob. Host seals into `encrypted_secrets`
    /// (`provider = plugin id`). Never written by the plugin into the library DB.
    #[serde(default)]
    pub credentials: Option<Value>,
}

/// Result of [`methods::LOGIN_START`] (interactive OAuth).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStartResultDto {
    /// Opaque session id for [`methods::LOGIN_COMPLETE`].
    pub session_id: String,
    /// Browser URL the operator should open.
    pub url: String,
}

/// Params for [`methods::LOGIN_COMPLETE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginCompleteParams {
    pub session_id: String,
}

/// Params for [`methods::CREDENTIALS_UPDATE`] — guest-requested credential write-back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialsUpdateParams {
    pub account_id: String,
    /// Replacement opaque credential JSON for the host to re-seal.
    pub credentials: Value,
}

/// One external user observed by an integration (ABS-only concerns; host workflows).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalUserDto {
    pub provider: String,
    pub external_user_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Result of [`methods::EVENT_POLL`] — signals for the host to kick off workflows.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventPollResultDto {
    #[serde(default)]
    pub users: Vec<ExternalUserDto>,
}

/// Scan params — no library DB path.
///
/// Host injects sealed credentials (same mediation as `fetch_title`) so the
/// plugin does not need a private credential store under `plugin_data_dir`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

fn default_page() -> u32 {
    50
}

/// One library title returned by an external source `scan` (host upserts).
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Params for [`methods::SEARCH_CATALOG`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchCatalogParams {
    pub query: String,
    #[serde(default)]
    pub region: String,
    #[serde(default = "default_catalog_limit")]
    pub limit: usize,
}

fn default_catalog_limit() -> usize {
    20
}

/// Params for [`methods::EXPAND_CANDIDATES`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    pub origin: String,
}

/// Wire form of [`SourcePurchaseHint`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct SyncListeningResultDto {
    #[serde(default)]
    pub items: Vec<ListeningProgressDto>,
}

/// Wire DTO for one listening-progress row (serde-compatible with the host
/// `ListeningProgressSnapshot` shape).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
pub struct ObjectInfoDto {
    pub key: String,
    pub size: u64,
}

/// Probe result for output plugins.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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
pub struct LocalGetParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub key: String,
}

/// Params for key-scoped local output methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalKeyParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub key: String,
}

/// Params for [`methods::LIST`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalListParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub prefix: String,
}

/// Params for [`methods::COPY`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalCopyParams {
    #[serde(flatten)]
    pub ctx: OutputLocalContextDto,
    pub from: String,
    pub to: String,
}

/// Params for [`methods::TOUCH_FILE`] (local filesystem output).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct GetParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub key: String,
}

/// Result of [`methods::GET`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetResultDto {
    pub data_base64: String,
}

/// Params for key-scoped output methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub key: String,
}

/// Params for [`methods::LIST`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub prefix: String,
}

/// Params for [`methods::COPY`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub from: String,
    pub to: String,
}

/// Params for [`methods::TOUCH_FILE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchFileParams {
    #[serde(flatten)]
    pub ctx: OutputS3ContextDto,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

/// Method names (keep stable).
pub mod methods {
    pub const HANDSHAKE: &str = "handshake";
    pub const HEALTH: &str = "health";
    pub const DIAGNOSE: &str = "diagnose";
    pub const START: &str = "start";
    pub const ON_EVENT: &str = "on_event";
    /// Integration → host signal poll ([`EventPollResultDto`]).
    pub const EVENT_POLL: &str = "event_poll";
    pub const SCAN_LIBRARY: &str = "scan_library";
    /// Return [`SyncListeningResultDto`] for the host to upsert.
    pub const SYNC_LISTENING: &str = "sync_listening";
    pub const AUTHENTICATE_USER: &str = "authenticate_user";
    pub const LOGIN: &str = "login";
    /// Begin interactive OAuth ([`LoginStartResultDto`]).
    pub const LOGIN_START: &str = "login.start";
    /// Finish interactive OAuth ([`LoginCompleteParams`] → [`LoginResultDto`]).
    pub const LOGIN_COMPLETE: &str = "login.complete";
    /// Guest credential write-back request (host re-seals); optional capability.
    pub const CREDENTIALS_UPDATE: &str = "credentials.update";
    pub const LIST_ACCOUNTS: &str = "list_accounts";
    pub const SCAN: &str = "scan";
    pub const FETCH_TITLE: &str = "fetch_title";
    /// Public catalog typeahead ([`SearchCatalogParams`] → `Vec<CatalogHitDto>`).
    pub const SEARCH_CATALOG: &str = "search_catalog";
    /// Expand related / series / author candidates ([`ExpandCandidatesParams`]).
    pub const EXPAND_CANDIDATES: &str = "expand_candidates";
    /// Purchase / catalog URL (+ optional price) ([`PurchaseHintParams`]).
    pub const PURCHASE_HINT: &str = "purchase_hint";
    /// Current deals / promos (`{ "limit": N }` → `Vec<CatalogHitDto>`).
    pub const LIST_DEALS: &str = "list_deals";
    /// Return [`CliSchema`] (authoritative at invoke time when capability `cli` is set).
    pub const CLI_DESCRIBE: &str = "cli.describe";
    /// Run a declared plugin CLI command ([`CliInvokeParams`] → [`CliInvokeResult`]).
    pub const CLI_INVOKE: &str = "cli.invoke";
    /// Write bytes under a key ([`PutParams`]).
    pub const PUT: &str = "put";
    /// Stream a local file from the side channel ([`PutFileParams`]).
    pub const PUT_FILE: &str = "put_file";
    /// Read an object ([`GetParams`] → [`GetResultDto`]).
    pub const GET: &str = "get";
    /// Probe object metadata ([`KeyParams`] → [`ObjectProbeDto`]).
    pub const PROBE: &str = "probe";
    /// True when the object exists ([`KeyParams`] → `{ "exists": bool }`).
    pub const EXISTS: &str = "exists";
    /// List objects under a prefix ([`ListParams`] → `Vec<ObjectInfoDto>`).
    pub const LIST: &str = "list";
    /// Server-side copy ([`CopyParams`]).
    pub const COPY: &str = "copy";
    /// Delete an object ([`KeyParams`]).
    pub const DELETE: &str = "delete";
    /// Best-effort logical timestamp update ([`TouchFileParams`]).
    pub const TOUCH_FILE: &str = "touch_file";
    /// Open the library database ([`DbConnectParams`]; SQLite receives fd 3).
    pub const DB_CONNECT: &str = "db.connect";
    /// Ping the database connection.
    pub const DB_PING: &str = "db.ping";
    /// Run a query ([`StatementDto`] → [`QueryResultDto`]).
    pub const DB_QUERY: &str = "db.query";
    /// Execute a statement ([`StatementDto`] → [`ExecResultDto`]).
    pub const DB_EXECUTE: &str = "db.execute";
}
