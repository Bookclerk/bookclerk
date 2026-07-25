//! Shared JSON-RPC method names and payload types (api_version = 1).

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
    pub auth_credential_suffixes: Vec<String>,
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

/// Login params for source plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginParams {
    pub files_dir: String,
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

/// Scan params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanParams {
    pub files_dir: String,
    pub library_db: String,
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default = "default_page")]
    pub page_size: u32,
    #[serde(default = "default_true")]
    pub import_episodes: bool,
    #[serde(default = "default_true")]
    pub import_plus_titles: bool,
}

fn default_page() -> u32 {
    50
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
}

/// Fetch-title params. Plugin writes media under `cache_dir` and returns paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchTitleParams {
    pub files_dir: String,
    pub account_id: String,
    pub title_id: String,
    pub cache_dir: String,
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

/// Method names (keep stable).
pub mod methods {
    pub const HANDSHAKE: &str = "handshake";
    pub const HEALTH: &str = "health";
    pub const DIAGNOSE: &str = "diagnose";
    pub const START: &str = "start";
    pub const ON_EVENT: &str = "on_event";
    pub const SCAN_LIBRARY: &str = "scan_library";
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
