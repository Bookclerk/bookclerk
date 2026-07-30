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

/// Book-acquired event params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookAcquiredDto {
    pub book: bookclerk_library::BookRecord,
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
    },
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
    pub items: Vec<bookclerk_integrations::ListeningProgressSnapshot>,
}

/// Method names (keep stable).
pub mod methods {
    pub const HANDSHAKE: &str = "handshake";
    pub const HEALTH: &str = "health";
    pub const DIAGNOSE: &str = "diagnose";
    pub const START: &str = "start";
    pub const ON_EVENT: &str = "on_event";
    pub const SCAN_LIBRARY: &str = "scan_library";
    /// Return [`SyncListeningResultDto`] for the host to upsert.
    pub const SYNC_LISTENING: &str = "sync_listening";
    pub const AUTHENTICATE_USER: &str = "authenticate_user";
    pub const LOGIN: &str = "login";
    pub const LIST_ACCOUNTS: &str = "list_accounts";
    pub const SCAN: &str = "scan";
    pub const FETCH_TITLE: &str = "fetch_title";
    /// Return [`CliSchema`] (authoritative at invoke time when capability `cli` is set).
    pub const CLI_DESCRIBE: &str = "cli.describe";
    /// Run a declared plugin CLI command ([`CliInvokeParams`] → [`CliInvokeResult`]).
    pub const CLI_INVOKE: &str = "cli.invoke";
}
