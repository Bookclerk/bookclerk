//! GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.
//!
//! Rust projection of the `rust-generated` section of `plugin.capnp`: owned
//! DTOs for every typed method payload plus `write_*` / `read_*` codecs over
//! the capnpc builders and readers. `$optional` scalars are `Option<T>`
//! (wire zero value = `None`); `{ ok, err }` reply unions are
//! `Result<T, PluginError>`.

#![allow(clippy::too_many_lines)]

use serde::{Deserialize, Serialize};

use crate::plugin_capnp;

/// Cap'n Proto list lengths are `u32`.
///
/// # Errors
///
/// Returns when `len` exceeds `u32::MAX`.
fn list_len(len: usize) -> capnp::Result<u32> {
    u32::try_from(len)
        .map_err(|_| capnp::Error::failed(format!("list length {len} exceeds UInt32")))
}

/// `$optional` text: empty means absent.
///
/// # Errors
///
/// Returns when the Cap'n Proto text is not valid UTF-8.
fn opt_text(t: capnp::text::Reader<'_>) -> capnp::Result<Option<String>> {
    let s = t.to_str()?;
    Ok(if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    })
}

/// `$optional` data: empty means absent.
fn opt_data(d: &[u8]) -> Option<Vec<u8>> {
    if d.is_empty() {
        None
    } else {
        Some(d.to_vec())
    }
}

/// `$optional` number: zero means absent.
fn opt_num<T: Default + PartialEq>(v: T) -> Option<T> {
    if v == T::default() {
        None
    } else {
        Some(v)
    }
}

/// Owned strings of a `List(Text)`.
///
/// # Errors
///
/// Returns when an element is not valid UTF-8.
fn read_text_list(list: capnp::text_list::Reader<'_>) -> capnp::Result<Vec<String>> {
    list.iter().map(|t| Ok(t?.to_string()?)).collect()
}

/// Named entrypoint a plugin exports (Cloudflare Workers named-entrypoint
/// analogue). Each value is a capability the host calls over RPC; triggers on
/// the default entrypoint (`event`, `job`) are declared separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Entrypoint {
    /// Storefront: login, scan, fetch, catalog search (`ContentSource`).
    #[default]
    Storefront,
    /// Object storage destination (`Destination`).
    Storage,
    /// Library database adapter (`Database`).
    DatabaseAdapter,
    /// Remote-library lifecycle: start/stop, scanLibrary, syncListening,
    /// pollEvents (`RemoteLibrary`).
    RemoteLibrary,
    /// Guest CLI (`PluginCli.describe` / `PluginCli.invoke`).
    Cli,
    /// OIDC bridge: relying-party client templates and `authenticateUser`.
    Oidc,
}

impl Entrypoint {
    /// Wire name of this enumerant (matches the TypeScript / Python SDK literal).
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Storefront => "storefront",
            Self::Storage => "storage",
            Self::DatabaseAdapter => "databaseAdapter",
            Self::RemoteLibrary => "remoteLibrary",
            Self::Cli => "cli",
            Self::Oidc => "oidc",
        }
    }

    /// Parse a wire name; `None` for unknown strings.
    #[must_use]
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "storefront" => Some(Self::Storefront),
            "storage" => Some(Self::Storage),
            "databaseAdapter" => Some(Self::DatabaseAdapter),
            "remoteLibrary" => Some(Self::RemoteLibrary),
            "cli" => Some(Self::Cli),
            "oidc" => Some(Self::Oidc),
            _ => None,
        }
    }
}

impl From<Entrypoint> for plugin_capnp::Entrypoint {
    fn from(value: Entrypoint) -> Self {
        match value {
            Entrypoint::Storefront => Self::Storefront,
            Entrypoint::Storage => Self::Storage,
            Entrypoint::DatabaseAdapter => Self::DatabaseAdapter,
            Entrypoint::RemoteLibrary => Self::RemoteLibrary,
            Entrypoint::Cli => Self::Cli,
            Entrypoint::Oidc => Self::Oidc,
        }
    }
}

impl From<plugin_capnp::Entrypoint> for Entrypoint {
    fn from(value: plugin_capnp::Entrypoint) -> Self {
        match value {
            plugin_capnp::Entrypoint::Storefront => Self::Storefront,
            plugin_capnp::Entrypoint::Storage => Self::Storage,
            plugin_capnp::Entrypoint::DatabaseAdapter => Self::DatabaseAdapter,
            plugin_capnp::Entrypoint::RemoteLibrary => Self::RemoteLibrary,
            plugin_capnp::Entrypoint::Cli => Self::Cli,
            plugin_capnp::Entrypoint::Oidc => Self::Oidc,
        }
    }
}

/// Portal Accounts connect mode for storefronts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PortalAuthMode {
    /// Guest did not declare a mode; the host assumes password login.
    #[default]
    Unspecified,
    /// Email / password login through the Accounts UI.
    Password,
    /// Browser OAuth through `loginStart` / `loginComplete`.
    Oauth,
}

impl PortalAuthMode {
    /// Wire name of this enumerant (matches the TypeScript / Python SDK literal).
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Password => "password",
            Self::Oauth => "oauth",
        }
    }

    /// Parse a wire name; `None` for unknown strings.
    #[must_use]
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "unspecified" => Some(Self::Unspecified),
            "password" => Some(Self::Password),
            "oauth" => Some(Self::Oauth),
            _ => None,
        }
    }
}

impl From<PortalAuthMode> for plugin_capnp::PortalAuthMode {
    fn from(value: PortalAuthMode) -> Self {
        match value {
            PortalAuthMode::Unspecified => Self::Unspecified,
            PortalAuthMode::Password => Self::Password,
            PortalAuthMode::Oauth => Self::Oauth,
        }
    }
}

impl From<plugin_capnp::PortalAuthMode> for PortalAuthMode {
    fn from(value: plugin_capnp::PortalAuthMode) -> Self {
        match value {
            plugin_capnp::PortalAuthMode::Unspecified => Self::Unspecified,
            plugin_capnp::PortalAuthMode::Password => Self::Password,
            plugin_capnp::PortalAuthMode::Oauth => Self::Oauth,
        }
    }
}

/// Value kind for a `CliArgSpec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CliArgKind {
    /// Free-form string argument (default).
    #[default]
    String,
    /// Boolean flag ("true" / "false").
    Bool,
    /// Integer argument.
    Int,
    /// Filesystem path argument.
    Path,
}

impl CliArgKind {
    /// Wire name of this enumerant (matches the TypeScript / Python SDK literal).
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Path => "path",
        }
    }

    /// Parse a wire name; `None` for unknown strings.
    #[must_use]
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "string" => Some(Self::String),
            "bool" => Some(Self::Bool),
            "int" => Some(Self::Int),
            "path" => Some(Self::Path),
            _ => None,
        }
    }
}

impl From<CliArgKind> for plugin_capnp::CliArgKind {
    fn from(value: CliArgKind) -> Self {
        match value {
            CliArgKind::String => Self::String,
            CliArgKind::Bool => Self::Bool,
            CliArgKind::Int => Self::Int,
            CliArgKind::Path => Self::Path,
        }
    }
}

impl From<plugin_capnp::CliArgKind> for CliArgKind {
    fn from(value: plugin_capnp::CliArgKind) -> Self {
        match value {
            plugin_capnp::CliArgKind::String => Self::String,
            plugin_capnp::CliArgKind::Bool => Self::Bool,
            plugin_capnp::CliArgKind::Int => Self::Int,
            plugin_capnp::CliArgKind::Path => Self::Path,
        }
    }
}

/// Catalog search ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogSort {
    /// Storefront relevance ranking (default).
    #[default]
    Relevance,
    /// Most popular first.
    Popularity,
    /// Highest rated first.
    Rating,
    /// Alphabetical by title.
    Title,
    /// Alphabetical by author.
    Author,
}

impl CatalogSort {
    /// Wire name of this enumerant (matches the TypeScript / Python SDK literal).
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Popularity => "popularity",
            Self::Rating => "rating",
            Self::Title => "title",
            Self::Author => "author",
        }
    }

    /// Parse a wire name; `None` for unknown strings.
    #[must_use]
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "relevance" => Some(Self::Relevance),
            "popularity" => Some(Self::Popularity),
            "rating" => Some(Self::Rating),
            "title" => Some(Self::Title),
            "author" => Some(Self::Author),
            _ => None,
        }
    }
}

impl From<CatalogSort> for plugin_capnp::CatalogSort {
    fn from(value: CatalogSort) -> Self {
        match value {
            CatalogSort::Relevance => Self::Relevance,
            CatalogSort::Popularity => Self::Popularity,
            CatalogSort::Rating => Self::Rating,
            CatalogSort::Title => Self::Title,
            CatalogSort::Author => Self::Author,
        }
    }
}

impl From<plugin_capnp::CatalogSort> for CatalogSort {
    fn from(value: plugin_capnp::CatalogSort) -> Self {
        match value {
            plugin_capnp::CatalogSort::Relevance => Self::Relevance,
            plugin_capnp::CatalogSort::Popularity => Self::Popularity,
            plugin_capnp::CatalogSort::Rating => Self::Rating,
            plugin_capnp::CatalogSort::Title => Self::Title,
            plugin_capnp::CatalogSort::Author => Self::Author,
        }
    }
}

/// Catalog search facet restricting which field the query matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogField {
    /// Match any field.
    #[default]
    Any,
    /// Match author names.
    Author,
    /// Match narrator names.
    Narrator,
    /// Match series names.
    Series,
    /// Match genre / category labels.
    Genre,
}

impl CatalogField {
    /// Wire name of this enumerant (matches the TypeScript / Python SDK literal).
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Author => "author",
            Self::Narrator => "narrator",
            Self::Series => "series",
            Self::Genre => "genre",
        }
    }

    /// Parse a wire name; `None` for unknown strings.
    #[must_use]
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "any" => Some(Self::Any),
            "author" => Some(Self::Author),
            "narrator" => Some(Self::Narrator),
            "series" => Some(Self::Series),
            "genre" => Some(Self::Genre),
            _ => None,
        }
    }
}

impl From<CatalogField> for plugin_capnp::CatalogField {
    fn from(value: CatalogField) -> Self {
        match value {
            CatalogField::Any => Self::Any,
            CatalogField::Author => Self::Author,
            CatalogField::Narrator => Self::Narrator,
            CatalogField::Series => Self::Series,
            CatalogField::Genre => Self::Genre,
        }
    }
}

impl From<plugin_capnp::CatalogField> for CatalogField {
    fn from(value: plugin_capnp::CatalogField) -> Self {
        match value {
            plugin_capnp::CatalogField::Any => Self::Any,
            plugin_capnp::CatalogField::Author => Self::Author,
            plugin_capnp::CatalogField::Narrator => Self::Narrator,
            plugin_capnp::CatalogField::Series => Self::Series,
            plugin_capnp::CatalogField::Genre => Self::Genre,
        }
    }
}

/// Whether an edition is abridged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Abridgement {
    /// The storefront did not say.
    #[default]
    Unknown,
    /// Unabridged edition.
    Unabridged,
    /// Abridged edition.
    Abridged,
}

impl Abridgement {
    /// Wire name of this enumerant (matches the TypeScript / Python SDK literal).
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Unabridged => "unabridged",
            Self::Abridged => "abridged",
        }
    }

    /// Parse a wire name; `None` for unknown strings.
    #[must_use]
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "unknown" => Some(Self::Unknown),
            "unabridged" => Some(Self::Unabridged),
            "abridged" => Some(Self::Abridged),
            _ => None,
        }
    }
}

impl From<Abridgement> for plugin_capnp::Abridgement {
    fn from(value: Abridgement) -> Self {
        match value {
            Abridgement::Unknown => Self::Unknown,
            Abridgement::Unabridged => Self::Unabridged,
            Abridgement::Abridged => Self::Abridged,
        }
    }
}

impl From<plugin_capnp::Abridgement> for Abridgement {
    fn from(value: plugin_capnp::Abridgement) -> Self {
        match value {
            plugin_capnp::Abridgement::Unknown => Self::Unknown,
            plugin_capnp::Abridgement::Unabridged => Self::Unabridged,
            plugin_capnp::Abridgement::Abridged => Self::Abridged,
        }
    }
}

/// One declared event consumer: a `\[\[events.consumers\]\]` row the default
/// entrypoint's `event(batch)` handler accepts.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventConsumerSpec {
    /// Versioned event type (snake_case, e.g. `book_acquired`).
    #[serde(default)]
    pub event_type: String,
    /// Schema versions the guest can consume; never empty.
    #[serde(default)]
    pub schema_versions: Vec<u32>,
    /// Whether `EventResult.suspended` is supported for this type.
    #[serde(default)]
    pub supports_suspend: bool,
}

/// Typed capability declaration returned by `describe()`. The host compares
/// it with `plugin.toml` and the operator grant; widening is rejected at spawn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCapabilities {
    /// Named entrypoints the guest exports.
    #[serde(default)]
    pub entrypoints: Vec<Entrypoint>,
    /// Event types the default entrypoint consumes (`event(batch)` trigger).
    #[serde(default)]
    pub consumes: Vec<EventConsumerSpec>,
    /// Event types the guest may publish through its `EVENTS` binding.
    #[serde(default)]
    pub produces: Vec<String>,
    /// Command types the default entrypoint runs (`job(controller)` trigger).
    #[serde(default)]
    pub jobs: Vec<String>,
    /// Plugin-owned database binding names (`\[\[databases\]\]`).
    #[serde(default)]
    pub databases: Vec<String>,
    /// Other named bindings the guest expects on `env` (`CONFIG`, `SECRETS`,
    /// `WORK_FS`, `OAUTH`, `KV`, `EVENTS`, `\[\[queues.producers\]\]` names, ...).
    #[serde(default)]
    pub bindings: Vec<String>,
    /// Queues the default entrypoint consumes (`queue(batch)` trigger).
    #[serde(default)]
    pub queue_consumers: Vec<String>,
    /// Queues the guest may send to through its `\[\[queues.producers\]\]` bindings.
    #[serde(default)]
    pub queue_producers: Vec<String>,
}

/// Portal brand crossing the RPC boundary. Distinct from `plugin.toml`
/// `logo`: `iconUrl` is the live URL or data URI the SPA renders.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Brand {
    /// Brand id (often matches the plugin id); empty means "no brand".
    #[serde(default)]
    pub id: String,
    /// Display name shown next to the brand swatch.
    #[serde(default)]
    pub name: String,
    /// Background CSS color (hex or named).
    #[serde(default)]
    pub bg: String,
    /// Foreground CSS color for text on `bg`.
    #[serde(default)]
    pub fg: String,
    /// Accent CSS color for highlights / CTAs.
    #[serde(default)]
    pub accent: String,
    /// Icon URL or data URI for the portal.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// One discoverable config option group advertised for sources.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    /// Config key under the plugin's `config.toml` table.
    #[serde(default)]
    pub key: String,
    /// Operator-facing label for the option group.
    #[serde(default)]
    pub label: String,
    /// Allowed selectable values for this key.
    #[serde(default)]
    pub values: Vec<ConfigOptionValue>,
}

/// One selectable value under a `ConfigOption`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionValue {
    /// Value written to config when selected.
    #[serde(default)]
    pub id: String,
    /// Operator-facing label for this value.
    #[serde(default)]
    pub label: String,
}

/// Declared plugin CLI surface (`PluginCli.describe` / `describe().cli`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliSchema {
    /// Commands exposed as `bookclerk plugins <id> <command> ...`.
    #[serde(default)]
    pub commands: Vec<CliCommandSpec>,
}

/// One plugin CLI command under `CliSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandSpec {
    /// Command verb after the plugin id (for example "ping").
    #[serde(default)]
    pub name: String,
    /// Short help text for `--help`.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Argument / flag specs for this command.
    #[serde(default)]
    pub args: Vec<CliArgSpec>,
}

/// One CLI argument or flag under a `CliCommandSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliArgSpec {
    /// Internal arg name used as `CliArg.name` on invoke.
    #[serde(default)]
    pub name: String,
    /// Long flag without leading dashes (e.g. "message" -> `--message`).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<String>,
    /// Short flag character (e.g. "m" -> `-m`).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    /// Parsed value kind.
    #[serde(default)]
    pub kind: CliArgKind,
    /// When true, the host rejects invoke if the arg is missing.
    #[serde(default)]
    pub required: bool,
    /// Default string form when the operator omits the arg.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Help text for this arg.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// When true, the arg is positional rather than a flagged option.
    #[serde(default)]
    pub positional: bool,
}

/// One named argument value passed to `PluginCli.invoke`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliArg {
    /// Arg name matching a `CliArgSpec.name`.
    #[serde(default)]
    pub name: String,
    /// String form of the value (the guest parses per `CliArgSpec.kind`).
    #[serde(default)]
    pub value: String,
}

/// Params of `PluginCli.invoke`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliInvokeParams {
    /// Command name matching a `CliCommandSpec.name`.
    #[serde(default)]
    pub command: String,
    /// Named argument values.
    #[serde(default)]
    pub args: Vec<CliArg>,
}

/// Result of `PluginCli.invoke`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliInvokeResult {
    /// Process-style exit code (0 = success).
    #[serde(default)]
    pub exit_code: i32,
    /// Captured standard output text.
    #[serde(default)]
    pub stdout: String,
    /// Captured standard error text.
    #[serde(default)]
    pub stderr: String,
    /// Structured payload for machine consumers; empty `mediaType` when absent.
    #[serde(default)]
    pub payload: crate::ExtensibleConfig,
}

/// Author-facing database adapter configuration carried in
/// `DatabaseContext.adapter`. This is the generic bootstrap mechanism for
/// third-party adapters: the operator's granted `\[database.<id>\]` table plus
/// the scoped writable data dir. First-party host-managed adapters receive
/// host-private connect params in `DatabaseContext.config` instead.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseAdapterConfig {
    /// Scoped writable directory for this plugin (`.../plugins/<id>/data`).
    #[serde(default)]
    pub plugin_data_dir: String,
    /// Granted plugin settings (operator `\[database.<id>\]` table) as
    /// `application/json`; `{}` when the operator configured nothing.
    #[serde(default)]
    pub settings: crate::ExtensibleConfig,
    /// Named plugin database binding this open serves; empty for the primary
    /// library open. Adapters advertising `DbCapabilities.pluginDatabases` must
    /// serve each binding from its own isolated database.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// Host-issued opaque instance id for this (owner plugin, binding) pair.
    /// Collision-resistant and stable across re-opens. Empty for the primary
    /// library open. Third-party adapters must key isolated databases on this
    /// value rather than `binding` alone (two plugins may both declare `DB`).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// When true, open an existing binding unit and do not provision a missing
    /// one (read-only backup capture). False lets the adapter create the unit.
    #[serde(default)]
    pub open_existing: bool,
}

/// Operator-facing diagnostic lines printed by `bookclerk plugins diagnose`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseResult {
    /// Human-readable probe lines.
    #[serde(default)]
    pub lines: Vec<String>,
}

/// Source account metadata returned from login and stored by the host.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceAccount {
    /// Stable account id within this source plugin.
    #[serde(default)]
    pub account_id: String,
    /// Source plugin id (the host forces this to the guest's install id).
    #[serde(default)]
    pub source: String,
    /// Storefront marketplace / region code (for example `us`, `uk`).
    #[serde(default)]
    pub marketplace: String,
    /// Operator-facing label.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// When true, bare / scheduled scans include this account. Explicit CLI
    /// `--account` bypasses this flag.
    #[serde(default)]
    pub scan_enabled: bool,
}

/// Params of `ContentSource.login` and `ContentSource.loginStart`. Password
/// sources fill email/password; OAuth sources use callback / external fields.
/// There is no files-dir root or library DB path -- only `pluginDataDir`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginParams {
    /// Scoped writable directory for this plugin only (`.../plugins/<id>/data`).
    #[serde(default)]
    pub plugin_data_dir: String,
    /// Marketplace / locale for the storefront; empty means the guest default.
    #[serde(default)]
    pub marketplace: String,
    /// Operator label stored on the account row.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Account email / username for password logins; empty for pure OAuth.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Account password for password logins; never logged; empty for OAuth.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// When true, overwrite an existing sealed credential for this account.
    #[serde(default)]
    pub force: bool,
    /// Bind address for OAuth callback servers (`host:port`). Ignored when
    /// `callbackIpc` is set (host owns the TCP listener).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_bind: Option<String>,
    /// Host-owned callback IPC endpoint the guest must connect to. When set
    /// (with `callbackPublicBase`), the guest must not bind a TCP listener.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_ipc: Option<String>,
    /// Public base URL for the host TCP listener, e.g. `http://127.0.0.1:12345`.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_public_base: Option<String>,
    /// When true, use external / paste-redirect OAuth instead of a local
    /// callback server.
    #[serde(default)]
    pub external: bool,
    /// Pre-supplied OAuth redirect URL (paste flow).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_url: Option<String>,
    /// Prefer QR output when the guest supports it.
    #[serde(default)]
    pub show_qr: bool,
    /// Seconds to wait for OAuth callback capture; guest default when 0.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Store-specific knobs as `application/json`; guests may ignore unknowns.
    #[serde(default)]
    pub extra: crate::ExtensibleConfig,
}

/// Result of `ContentSource.login` / `loginComplete`: account metadata plus
/// opaque credentials for the host to seal into `encrypted_secrets`
/// (`provider = plugin id`). Guests never write secrets into the library DB.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResult {
    /// Account row fields for the host to upsert.
    #[serde(default)]
    pub account: SourceAccount,
    /// Opaque credential blob the host seals; empty when login only refreshed
    /// metadata. Guests choose the encoding (typically JSON bytes).
    /// `None` when absent (wire zero value).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::json_bytes::opt_b64"
    )]
    pub credentials: Option<Vec<u8>>,
}

/// Result of `ContentSource.loginStart` (interactive OAuth). The operator
/// opens `url`; `loginComplete` later uses `sessionId`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStartResult {
    /// Opaque session id for `loginComplete`.
    #[serde(default)]
    pub session_id: String,
    /// Browser URL the operator should open to complete OAuth.
    #[serde(default)]
    pub url: String,
}

/// Params of `ContentSource.loginComplete`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCompleteParams {
    /// Session id previously returned by `loginStart`.
    #[serde(default)]
    pub session_id: String,
}

/// Host-sealed credentials for one account, delivered on `scan`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCredential {
    /// Account id the blob belongs to.
    #[serde(default)]
    pub account_id: String,
    /// Opaque credential bytes exactly as the guest returned them at login.
    #[serde(default, with = "crate::json_bytes::b64")]
    pub credentials: Vec<u8>,
}

/// Params of `ContentSource.scan`. The host injects sealed credentials so the
/// plugin does not need a private credential store under `pluginDataDir`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanParams {
    /// Scoped plugin data directory.
    #[serde(default)]
    pub plugin_data_dir: String,
    /// Account ids to scan; empty means all scan-enabled accounts.
    #[serde(default)]
    pub accounts: Vec<String>,
    /// Storefront page size; the host always sends an explicit value.
    #[serde(default)]
    pub page_size: u32,
    /// When true, import podcast/episode-style rows.
    #[serde(default)]
    pub import_episodes: bool,
    /// When true, import Plus/catalog entitlement titles.
    #[serde(default)]
    pub import_plus_titles: bool,
    /// Host-loaded credential blobs for the requested accounts.
    #[serde(default)]
    pub credentials: Vec<AccountCredential>,
}

/// One library title returned by `ContentSource.scan`. The host upserts these
/// rows and forces `source` to the plugin id.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanBook {
    /// Account that owns this library entry.
    #[serde(default)]
    pub account_id: String,
    /// Storefront product / SKU id.
    #[serde(default)]
    pub product_id: String,
    /// Primary title string.
    #[serde(default)]
    pub title: String,
    /// Marketplace / region when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketplace: Option<String>,
    /// Amazon ASIN when the storefront exposes one.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// ISBN when the storefront exposes one.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// Comma- or guest-formatted author list.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<String>,
    /// Comma- or guest-formatted narrator list.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrators: Option<String>,
    /// Series name when applicable.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// Series index / sequence label.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_index: Option<String>,
    /// Content classification (e.g. `book` vs `episode`).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_kind: Option<String>,
    /// Publisher name when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Runtime in whole minutes when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_minutes: Option<i64>,
    /// Subtitle when distinct from `title`.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
}

/// Summary result of `ContentSource.scan`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    /// Number of accounts touched during the scan.
    #[serde(default)]
    pub accounts: u32,
    /// Count of titles the guest expects the host to upsert; may mirror
    /// `books.length`.
    #[serde(default)]
    pub books_upserted: u32,
    /// Number of storefront pages fetched.
    #[serde(default)]
    pub pages: u32,
    /// Accounts skipped because `scanEnabled` was false.
    #[serde(default)]
    pub skipped_disabled: u32,
    /// Titles for the host to upsert. Prefer this over plugin-side DB writes.
    #[serde(default)]
    pub books: Vec<ScanBook>,
}

/// Fetch-relevant acquire knobs the host forwards so external load matches
/// in-process. Packaging / naming knobs stay host-side.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchOptions {
    /// Prefer Widevine/CENC download when the store offers it.
    #[serde(default)]
    pub widevine: bool,
    /// Prefer xHE-AAC on the Widevine path when offered.
    #[serde(default)]
    pub xhe_aac: bool,
    /// Local Widevine `.wvd` path granted to the guest.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widevine_cdm_path: Option<String>,
    /// Remote L3 CDM provider URL; empty means the classic default, `off`
    /// disables remote provisioning.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widevine_cdm_provider: Option<String>,
    /// When true, download a cover image alongside audio.
    #[serde(default)]
    pub download_cover: bool,
    /// When true, download a companion PDF when the store exposes one.
    #[serde(default)]
    pub download_pdf: bool,
    /// Cover image size request (`500`, `1215`, or `native`).
    #[serde(default)]
    pub cover_size: String,
    /// Preferred chapter API layout when fetching (`tree` or `flat`).
    #[serde(default)]
    pub chapter_layout: String,
    /// When true, trim Audible brand intro/outro from the remux window.
    #[serde(default)]
    pub strip_audible_brand_audio: bool,
    /// When true, download clips/bookmarks sidecars when offered.
    #[serde(default)]
    pub download_clips_bookmarks: bool,
    /// When true, keep the encrypted download in storage.
    #[serde(default)]
    pub retain_aax_file: bool,
    /// Fetch speed cap in KB/s (`0` = unlimited).
    #[serde(default)]
    pub download_speed_limit_kbps: u32,
    /// When true, persist raw catalog API JSON as `metadata.json`.
    #[serde(default)]
    pub save_metadata_json: bool,
}

/// Params of `ContentSource.fetchTitle`. The plugin writes media under
/// `cacheDir` and returns plain (DRM-free) paths. The host injects
/// credentials; guests must not open `library.db` or `master.key`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchTitleParams {
    /// Scoped plugin data directory.
    #[serde(default)]
    pub plugin_data_dir: String,
    /// Account whose credentials apply.
    #[serde(default)]
    pub account_id: String,
    /// Library / storefront title id to download.
    #[serde(default)]
    pub title_id: String,
    /// Absolute path the guest should write media into (jail-granted TMPDIR).
    #[serde(default)]
    pub cache_dir: String,
    /// Host-loaded credential blob for this account; empty when unavailable.
    /// `None` when absent (wire zero value).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::json_bytes::opt_b64"
    )]
    pub credentials: Option<Vec<u8>>,
    /// Granted `\[sources.<id>\]` table as `application/json`.
    #[serde(default)]
    pub source_config: crate::ExtensibleConfig,
    /// Fetch-relevant acquire options.
    #[serde(default)]
    pub fetch: FetchOptions,
}

/// One plain audio part written under the cache directory.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlainPart {
    /// Absolute path to the part file under `cacheDir`.
    #[serde(default)]
    pub path: String,
    /// Part title (disc / chapter label).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Duration of this part in milliseconds when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// One chapter marker of a fetched title.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterMarker {
    /// Chapter title.
    #[serde(default)]
    pub title: String,
    /// Chapter start offset in milliseconds from the beginning of the title.
    #[serde(default)]
    pub start_ms: u64,
}

/// Plain (DRM-free) fetch result. Sources always return decrypted media; DRM
/// guests decrypt before responding.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlainFetch {
    /// Ordered audio part files written under the cache directory.
    #[serde(default)]
    pub parts: Vec<PlainPart>,
    /// Single M4B path when the guest assembled one.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m4b_path: Option<String>,
    /// Cover image path under the cache directory.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_path: Option<String>,
    /// Chapter markers; empty when unknown.
    #[serde(default)]
    pub chapters: Vec<ChapterMarker>,
    /// Companion PDF download URL when the store exposes one.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_url: Option<String>,
}

/// Success payload of `ContentSource.listAccounts`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceAccounts {
    /// Accounts the guest knows about.
    #[serde(default)]
    pub accounts: Vec<SourceAccount>,
}

/// Params of `ContentSource.searchCatalog`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchCatalogParams {
    /// Free-text search query.
    #[serde(default)]
    pub query: String,
    /// Storefront region / marketplace code; empty means the guest default.
    #[serde(default)]
    pub region: String,
    /// Maximum hits to return; the host always sends an explicit value.
    #[serde(default)]
    pub limit: u32,
    /// 1-based page for storefronts that page.
    #[serde(default)]
    pub page: u32,
    /// Sort order.
    #[serde(default)]
    pub sort: CatalogSort,
    /// Facet restriction.
    #[serde(default)]
    pub field: CatalogField,
    /// Preferred content language (soft-prioritize; e.g. `en`).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// Params of `ContentSource.expandCandidates`. Seed fields identify a known
/// title; the guest returns related catalog hits.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandCandidatesParams {
    /// Source plugin id hint when expanding across storefronts.
    #[serde(default)]
    pub source: String,
    /// Seed storefront product id.
    #[serde(default)]
    pub product_id: String,
    /// Seed title text.
    #[serde(default)]
    pub title: String,
    /// Seed authors string.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<String>,
    /// Seed narrators string.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrators: Option<String>,
    /// Seed series name.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// Seed series ASIN when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_asin: Option<String>,
    /// Seed Amazon ASIN.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// Seed ISBN.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// Storefront region / marketplace code.
    #[serde(default)]
    pub region: String,
    /// Maximum candidates to return; the host always sends an explicit value.
    #[serde(default)]
    pub limit: u32,
}

/// Params of `ContentSource.purchaseHint`. At least one identity field
/// (`productId` / `asin` / `isbn` / title+authors) should be set; guests may
/// return `invalid_params` when none are usable.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseHintParams {
    /// Storefront product id when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_id: Option<String>,
    /// Title text for fuzzy lookup.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Authors string for fuzzy lookup.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<String>,
    /// Amazon ASIN when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// ISBN when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// Storefront region / marketplace code.
    #[serde(default)]
    pub region: String,
    /// When true, guests should include live price fields when available.
    #[serde(default)]
    pub with_price: bool,
}

/// Params of `ContentSource.listDeals`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListDealsParams {
    /// Maximum number of deals to return; guest default when 0.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Params of `ContentSource.catalogDetail`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogDetailParams {
    /// Store product id (Libro ISBN or ISBN-slug).
    #[serde(default)]
    pub product_id: String,
    /// ISBN when it differs from `productId`.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
}

/// One catalog / candidate hit returned by `searchCatalog`,
/// `expandCandidates`, `listDeals`, and `catalogDetail`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogHit {
    /// Storefront product / SKU id.
    #[serde(default)]
    pub product_id: String,
    /// Primary title.
    #[serde(default)]
    pub title: String,
    /// Authors string when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<String>,
    /// Narrators string when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrators: Option<String>,
    /// Series name when applicable.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// Series index / sequence label.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_index: Option<String>,
    /// Amazon ASIN when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// ISBN when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// Storefront product page URL.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Cover image URL.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// Hit origin label (plugin id or storefront name).
    #[serde(default)]
    pub origin: String,
    /// Subtitle when distinct from `title`.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Long description / blurb when fetched.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Publisher name when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Runtime in whole minutes.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_minutes: Option<i64>,
    /// Publication date string as provided by the storefront.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Category / genre labels as a single string when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<String>,
    /// Content language code when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Current price in minor units (cents).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cents: Option<i64>,
    /// ISO currency code for `priceCents`.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Pre-formatted price for display.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_label: Option<String>,
    /// Aggregate rating when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_overall: Option<f64>,
    /// Number of ratings when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_count: Option<i64>,
    /// Whether the edition is abridged when the storefront says so.
    #[serde(default)]
    pub abridgement: Abridgement,
}

/// Success payload of the catalog list methods.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogHits {
    /// Hits in storefront order.
    #[serde(default)]
    pub hits: Vec<CatalogHit>,
}

/// Success payload of `ContentSource.catalogDetail`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogDetail {
    /// False when the product is unknown to the storefront (`hit` is empty).
    #[serde(default)]
    pub found: bool,
    /// Full catalog record when `found`.
    #[serde(default)]
    pub hit: CatalogHit,
}

/// Purchase hint for SPA / CLI purchase deep-links.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseHint {
    /// Storefront product id.
    #[serde(default)]
    pub product_id: String,
    /// Title when resolved.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Purchase or product-page URL.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Current price in minor units.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cents: Option<i64>,
    /// ISO currency code for price fields.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Pre-formatted current price.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_label: Option<String>,
    /// List / MSRP price in minor units.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_cents: Option<i64>,
    /// Pre-formatted list price.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price_label: Option<String>,
    /// Member / Plus price in minor units.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_cents: Option<i64>,
    /// Pre-formatted member price.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_price_label: Option<String>,
}

/// Success payload of `ContentSource.purchaseHint`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseHintResult {
    /// False when the guest could not resolve the title (`hint` is empty).
    #[serde(default)]
    pub found: bool,
    /// Purchase hint when `found`.
    #[serde(default)]
    pub hint: PurchaseHint,
}

/// Params of `RemoteLibrary.scanLibrary` (remote library sync).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanLibraryParams {
    /// When true, force a full rescan even if the guest would otherwise
    /// incremental-sync.
    #[serde(default)]
    pub force: bool,
}

/// Params of `Oidc.authenticateUser`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateUserParams {
    /// Integration username / login id.
    #[serde(default)]
    pub username: String,
    /// Integration password; never logged by the host.
    #[serde(default)]
    pub password: String,
}

/// One external user observed by an integration. The host may mint claim
/// tickets without exposing portal details to the guest.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalUser {
    /// Integration provider id (often the plugin id).
    #[serde(default)]
    pub provider: String,
    /// Provider-scoped user id.
    #[serde(default)]
    pub external_user_id: String,
    /// Display name for UI.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Ephemeral remote token (e.g. ABS JWT). Guest-to-host only; never
    /// persisted.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
}

/// Success payload of `RemoteLibrary.pollEvents`: signals for the host to kick
/// off workflows.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPollResult {
    /// Newly observed external users since the last poll.
    #[serde(default)]
    pub users: Vec<ExternalUser>,
}

/// One listening-progress row. The host upserts into `listening_progress`
/// tagged with the plugin id; plugins never open the library DB.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListeningProgress {
    /// Provider-scoped user id.
    #[serde(default)]
    pub external_user_id: String,
    /// Provider-scoped item / library id.
    #[serde(default)]
    pub external_item_id: String,
    /// Bookclerk identity row id when already linked.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<i64>,
    /// Title text when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Authors string when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<String>,
    /// Amazon ASIN when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// ISBN when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// Fractional progress in `0.0..=1.0` when the provider reports it.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    /// Current playback position in seconds.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_time_seconds: Option<f64>,
    /// Total duration in seconds when known.
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    /// When true, the provider marks the item finished.
    #[serde(default)]
    pub is_finished: bool,
    /// Last listen timestamp as unix milliseconds (UTC).
    /// `None` when absent (wire zero value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_listened_at_unix_ms: Option<u64>,
}

/// Success payload of `RemoteLibrary.syncListening`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncListeningResult {
    /// Progress snapshots to upsert.
    #[serde(default)]
    pub items: Vec<ListeningProgress>,
}

/// Guest-published domain event (`EventPublisher.publish`). The host stamps
/// `eventId`, `source` (the plugin id), and `accountId` from the invocation;
/// guests cannot forge either.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginEvent {
    /// Snake_case event type; must be listed in `\[\[events.producers\]\]`.
    #[serde(default)]
    pub event_type: String,
    /// Schema version of `payload`, owned by the event type.
    #[serde(default)]
    pub schema_version: u32,
    /// Producer idempotency key, unique per (account, source, eventType) in the
    /// outbox; a repeat returns `PublishOk.duplicate = true` with the earlier id.
    /// Empty publishes unconditionally.
    #[serde(default)]
    pub deduplication_key: String,
    /// Encoded event payload; at most `maxEventPayloadBytes`.
    #[serde(default, with = "crate::json_bytes::b64")]
    pub payload: Vec<u8>,
    /// When the producer observed the fact; zero means "now" on the host clock.
    #[serde(default)]
    pub occurred_at_unix_ms: u64,
    /// Trace correlation id; empty inherits `Invocation.correlationId`.
    #[serde(default)]
    pub correlation_id: String,
    /// Id of the event or command that caused this one; empty inherits
    /// `Invocation.causationId`.
    #[serde(default)]
    pub causation_id: String,
}

/// Success payload of `EventPublisher.publish`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishOk {
    /// Outbox event id (new or the earlier row when `duplicate`).
    #[serde(default)]
    pub event_id: String,
    /// True when `deduplicationKey` matched an existing outbox row.
    #[serde(default)]
    pub duplicate: bool,
}

/// Identity of one `PluginWorker.open` invocation. The host issues ids; guests
/// echo `correlationId` / `causationId` onto published events.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invocation {
    /// Unique host-issued invocation id.
    #[serde(default)]
    pub id: String,
    /// Account scope; empty for operator / host-wide invocations.
    #[serde(default)]
    pub account_id: String,
    /// UTC Unix milliseconds; zero when the invocation has no deadline. The host
    /// fence is authoritative.
    #[serde(default)]
    pub deadline_unix_ms: u64,
    /// Trace correlation id; empty when none.
    #[serde(default)]
    pub correlation_id: String,
    /// Id of the event or command that caused this invocation; empty when none.
    #[serde(default)]
    pub causation_id: String,
}

/// One message on a Workers-style queue, delivered by `QueueConsumer.queue`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueMessage {
    /// Outbox row id; stable across redeliveries.
    #[serde(default)]
    pub id: String,
    /// When the producer sent the message (UTC Unix milliseconds).
    #[serde(default)]
    pub timestamp_unix_ms: u64,
    /// Encoded body; at most `maxEventPayloadBytes`.
    #[serde(default, with = "crate::json_bytes::b64")]
    pub body: Vec<u8>,
    /// Body encoding: `json`, `text`, or `bytes`.
    #[serde(default)]
    pub content_type: String,
    /// Delivery counter, starting at 1.
    #[serde(default)]
    pub attempts: u32,
    /// Producer plugin id; empty when the host sent it.
    #[serde(default)]
    pub source: String,
}

/// One `QueueConsumer.queue` delivery: messages from one queue in send order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueBatch {
    /// Queue the messages were sent to.
    #[serde(default)]
    pub queue: String,
    /// Messages in delivery order; the reply carries one `QueueResult` per entry.
    #[serde(default)]
    pub messages: Vec<QueueMessage>,
}

/// Redeliver a queue message later.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRetry {
    /// Seconds before redelivery; zero applies the consumer's `retry_delay`.
    #[serde(default)]
    pub delay_seconds: u32,
    /// Short human reason.
    #[serde(default)]
    pub reason: String,
}

/// Per-message outcome inside a `QueueConsumer.queue` batch reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum QueueResult {
    /// Handled; the host marks the message delivered.
    Ack,
    /// Redeliver later.
    Retry(QueueRetry),
}

/// Success payload of `QueueConsumer.queue`: exactly one entry per
/// `QueueBatch.messages` entry, in order. A short list is a host-side error
/// and the missing tail is retried.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueBatchOk {
    /// Per-message outcomes in delivery order.
    #[serde(default)]
    pub results: Vec<QueueResult>,
}

/// One message to send through a `\[\[queues.producers\]\]` binding.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSendMessage {
    /// Encoded body; at most `maxEventPayloadBytes`.
    #[serde(default, with = "crate::json_bytes::b64")]
    pub body: Vec<u8>,
    /// Body encoding: `json` (default when empty), `text`, or `bytes`.
    #[serde(default)]
    pub content_type: String,
    /// Seconds before the message may be delivered; zero means now.
    #[serde(default)]
    pub delay_seconds: u32,
}

/// Success payload of `QueueProducer.send` / `sendBatch`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSendOk {
    /// Outbox row ids in send order.
    #[serde(default)]
    pub message_ids: Vec<String>,
}

/// Expiration and metadata for `KvNamespace.put`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvPutOptions {
    /// Absolute expiry (UTC Unix milliseconds); zero means none.
    #[serde(default)]
    pub expiration_unix_ms: u64,
    /// Relative expiry in seconds from the write; zero means none. Ignored when
    /// `expirationUnixMs` is set.
    #[serde(default)]
    pub expiration_ttl_seconds: u64,
    /// Opaque JSON metadata text, at most `maxKvMetadataBytes`; empty means none.
    #[serde(default)]
    pub metadata: String,
}

/// Success payload of `KvNamespace.get`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvGetOk {
    /// True when the key exists and has not expired.
    #[serde(default)]
    pub found: bool,
    /// Stored value; empty when not found.
    #[serde(default, with = "crate::json_bytes::b64")]
    pub value: Vec<u8>,
    /// Stored metadata JSON text; empty when none.
    #[serde(default)]
    pub metadata: String,
    /// Absolute expiry (UTC Unix milliseconds); zero means none.
    #[serde(default)]
    pub expiration_unix_ms: u64,
}

/// Success payload of `KvNamespace.put`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvPutOk {
    /// Effective absolute expiry (UTC Unix milliseconds); zero means none.
    #[serde(default)]
    pub expiration_unix_ms: u64,
}

/// Success payload of `KvNamespace.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvDeleteOk {
    /// True when a live value existed before the delete.
    #[serde(default)]
    pub existed: bool,
}

/// Page request for `KvNamespace.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvListOptions {
    /// Only keys starting with this prefix; empty means all.
    #[serde(default)]
    pub prefix: String,
    /// Opaque cursor from a previous page; empty means the first page.
    #[serde(default)]
    pub cursor: String,
    /// Most keys to return; zero means the host default, at most `maxListPage`.
    #[serde(default)]
    pub limit: u32,
}

/// One key on a `KvNamespace.list` page.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvListKey {
    /// Key name.
    #[serde(default)]
    pub key: String,
    /// Stored metadata JSON text; empty when none.
    #[serde(default)]
    pub metadata: String,
    /// Absolute expiry (UTC Unix milliseconds); zero means none.
    #[serde(default)]
    pub expiration_unix_ms: u64,
}

/// Success payload of `KvNamespace.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvListOk {
    /// Keys in lexical order.
    #[serde(default)]
    pub keys: Vec<KvListKey>,
    /// True when no further page exists.
    #[serde(default)]
    pub list_complete: bool,
    /// Cursor for the next page; empty when `listComplete`.
    #[serde(default)]
    pub cursor: String,
}

/// Encode a [`EventConsumerSpec`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_event_consumer_spec(
    mut b: plugin_capnp::event_consumer_spec::Builder<'_>,
    v: &EventConsumerSpec,
) -> capnp::Result<()> {
    b.set_event_type(&v.event_type);
    {
        let mut items = b
            .reborrow()
            .init_schema_versions(list_len(v.schema_versions.len())?);
        for (i, item) in v.schema_versions.iter().enumerate() {
            items.set(list_len(i)?, *item);
        }
    }
    b.set_supports_suspend(v.supports_suspend);
    Ok(())
}

/// Decode a [`EventConsumerSpec`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_event_consumer_spec(
    r: plugin_capnp::event_consumer_spec::Reader<'_>,
) -> capnp::Result<EventConsumerSpec> {
    Ok(EventConsumerSpec {
        event_type: r.get_event_type()?.to_string()?,
        schema_versions: r.get_schema_versions()?.iter().collect::<Vec<_>>(),
        supports_suspend: r.get_supports_suspend(),
    })
}

/// Encode a [`PluginCapabilities`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_plugin_capabilities(
    mut b: plugin_capnp::plugin_capabilities::Builder<'_>,
    v: &PluginCapabilities,
) -> capnp::Result<()> {
    {
        let mut items = b
            .reborrow()
            .init_entrypoints(list_len(v.entrypoints.len())?);
        for (i, item) in v.entrypoints.iter().enumerate() {
            items.set(list_len(i)?, (*item).into());
        }
    }
    {
        let mut items = b.reborrow().init_consumes(list_len(v.consumes.len())?);
        for (i, item) in v.consumes.iter().enumerate() {
            write_event_consumer_spec(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    {
        let mut items = b.reborrow().init_produces(list_len(v.produces.len())?);
        for (i, item) in v.produces.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    {
        let mut items = b.reborrow().init_jobs(list_len(v.jobs.len())?);
        for (i, item) in v.jobs.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    {
        let mut items = b.reborrow().init_databases(list_len(v.databases.len())?);
        for (i, item) in v.databases.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    {
        let mut items = b.reborrow().init_bindings(list_len(v.bindings.len())?);
        for (i, item) in v.bindings.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    {
        let mut items = b
            .reborrow()
            .init_queue_consumers(list_len(v.queue_consumers.len())?);
        for (i, item) in v.queue_consumers.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    {
        let mut items = b
            .reborrow()
            .init_queue_producers(list_len(v.queue_producers.len())?);
        for (i, item) in v.queue_producers.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    Ok(())
}

/// Decode a [`PluginCapabilities`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_plugin_capabilities(
    r: plugin_capnp::plugin_capabilities::Reader<'_>,
) -> capnp::Result<PluginCapabilities> {
    Ok(PluginCapabilities {
        entrypoints: r
            .get_entrypoints()?
            .iter()
            .map(|item| item.map(Into::into))
            .collect::<Result<Vec<_>, _>>()?,
        consumes: r
            .get_consumes()?
            .iter()
            .map(|item| read_event_consumer_spec(item))
            .collect::<capnp::Result<Vec<_>>>()?,
        produces: read_text_list(r.get_produces()?)?,
        jobs: read_text_list(r.get_jobs()?)?,
        databases: read_text_list(r.get_databases()?)?,
        bindings: read_text_list(r.get_bindings()?)?,
        queue_consumers: read_text_list(r.get_queue_consumers()?)?,
        queue_producers: read_text_list(r.get_queue_producers()?)?,
    })
}

/// Encode a [`Brand`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_brand(mut b: plugin_capnp::brand::Builder<'_>, v: &Brand) -> capnp::Result<()> {
    b.set_id(&v.id);
    b.set_name(&v.name);
    b.set_bg(&v.bg);
    b.set_fg(&v.fg);
    b.set_accent(&v.accent);
    if let Some(value) = &v.icon_url {
        b.set_icon_url(value);
    }
    Ok(())
}

/// Decode a [`Brand`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_brand(r: plugin_capnp::brand::Reader<'_>) -> capnp::Result<Brand> {
    Ok(Brand {
        id: r.get_id()?.to_string()?,
        name: r.get_name()?.to_string()?,
        bg: r.get_bg()?.to_string()?,
        fg: r.get_fg()?.to_string()?,
        accent: r.get_accent()?.to_string()?,
        icon_url: opt_text(r.get_icon_url()?)?,
    })
}

/// Encode a [`ConfigOption`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_config_option(
    mut b: plugin_capnp::config_option::Builder<'_>,
    v: &ConfigOption,
) -> capnp::Result<()> {
    b.set_key(&v.key);
    b.set_label(&v.label);
    {
        let mut items = b.reborrow().init_values(list_len(v.values.len())?);
        for (i, item) in v.values.iter().enumerate() {
            write_config_option_value(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`ConfigOption`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_config_option(
    r: plugin_capnp::config_option::Reader<'_>,
) -> capnp::Result<ConfigOption> {
    Ok(ConfigOption {
        key: r.get_key()?.to_string()?,
        label: r.get_label()?.to_string()?,
        values: r
            .get_values()?
            .iter()
            .map(|item| read_config_option_value(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a [`ConfigOptionValue`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_config_option_value(
    mut b: plugin_capnp::config_option_value::Builder<'_>,
    v: &ConfigOptionValue,
) -> capnp::Result<()> {
    b.set_id(&v.id);
    b.set_label(&v.label);
    Ok(())
}

/// Decode a [`ConfigOptionValue`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_config_option_value(
    r: plugin_capnp::config_option_value::Reader<'_>,
) -> capnp::Result<ConfigOptionValue> {
    Ok(ConfigOptionValue {
        id: r.get_id()?.to_string()?,
        label: r.get_label()?.to_string()?,
    })
}

/// Encode a [`CliSchema`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_cli_schema(
    mut b: plugin_capnp::cli_schema::Builder<'_>,
    v: &CliSchema,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_commands(list_len(v.commands.len())?);
        for (i, item) in v.commands.iter().enumerate() {
            write_cli_command_spec(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`CliSchema`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_cli_schema(r: plugin_capnp::cli_schema::Reader<'_>) -> capnp::Result<CliSchema> {
    Ok(CliSchema {
        commands: r
            .get_commands()?
            .iter()
            .map(|item| read_cli_command_spec(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a [`CliCommandSpec`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_cli_command_spec(
    mut b: plugin_capnp::cli_command_spec::Builder<'_>,
    v: &CliCommandSpec,
) -> capnp::Result<()> {
    b.set_name(&v.name);
    if let Some(value) = &v.about {
        b.set_about(value);
    }
    {
        let mut items = b.reborrow().init_args(list_len(v.args.len())?);
        for (i, item) in v.args.iter().enumerate() {
            write_cli_arg_spec(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`CliCommandSpec`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_cli_command_spec(
    r: plugin_capnp::cli_command_spec::Reader<'_>,
) -> capnp::Result<CliCommandSpec> {
    Ok(CliCommandSpec {
        name: r.get_name()?.to_string()?,
        about: opt_text(r.get_about()?)?,
        args: r
            .get_args()?
            .iter()
            .map(|item| read_cli_arg_spec(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a [`CliArgSpec`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_cli_arg_spec(
    mut b: plugin_capnp::cli_arg_spec::Builder<'_>,
    v: &CliArgSpec,
) -> capnp::Result<()> {
    b.set_name(&v.name);
    if let Some(value) = &v.long {
        b.set_long(value);
    }
    if let Some(value) = &v.short {
        b.set_short(value);
    }
    b.set_kind(v.kind.into());
    b.set_required(v.required);
    if let Some(value) = &v.default {
        b.set_default(value);
    }
    if let Some(value) = &v.about {
        b.set_about(value);
    }
    b.set_positional(v.positional);
    Ok(())
}

/// Decode a [`CliArgSpec`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_cli_arg_spec(r: plugin_capnp::cli_arg_spec::Reader<'_>) -> capnp::Result<CliArgSpec> {
    Ok(CliArgSpec {
        name: r.get_name()?.to_string()?,
        long: opt_text(r.get_long()?)?,
        short: opt_text(r.get_short()?)?,
        kind: r.get_kind()?.into(),
        required: r.get_required(),
        default: opt_text(r.get_default()?)?,
        about: opt_text(r.get_about()?)?,
        positional: r.get_positional(),
    })
}

/// Encode a [`CliArg`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_cli_arg(mut b: plugin_capnp::cli_arg::Builder<'_>, v: &CliArg) -> capnp::Result<()> {
    b.set_name(&v.name);
    b.set_value(&v.value);
    Ok(())
}

/// Decode a [`CliArg`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_cli_arg(r: plugin_capnp::cli_arg::Reader<'_>) -> capnp::Result<CliArg> {
    Ok(CliArg {
        name: r.get_name()?.to_string()?,
        value: r.get_value()?.to_string()?,
    })
}

/// Encode a [`CliInvokeParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_cli_invoke_params(
    mut b: plugin_capnp::cli_invoke_params::Builder<'_>,
    v: &CliInvokeParams,
) -> capnp::Result<()> {
    b.set_command(&v.command);
    {
        let mut items = b.reborrow().init_args(list_len(v.args.len())?);
        for (i, item) in v.args.iter().enumerate() {
            write_cli_arg(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`CliInvokeParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_cli_invoke_params(
    r: plugin_capnp::cli_invoke_params::Reader<'_>,
) -> capnp::Result<CliInvokeParams> {
    Ok(CliInvokeParams {
        command: r.get_command()?.to_string()?,
        args: r
            .get_args()?
            .iter()
            .map(|item| read_cli_arg(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a [`CliInvokeResult`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_cli_invoke_result(
    mut b: plugin_capnp::cli_invoke_result::Builder<'_>,
    v: &CliInvokeResult,
) -> capnp::Result<()> {
    b.set_exit_code(v.exit_code);
    b.set_stdout(&v.stdout);
    b.set_stderr(&v.stderr);
    crate::rpc::write_extensible_config(b.reborrow().init_payload(), &v.payload);
    Ok(())
}

/// Decode a [`CliInvokeResult`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_cli_invoke_result(
    r: plugin_capnp::cli_invoke_result::Reader<'_>,
) -> capnp::Result<CliInvokeResult> {
    Ok(CliInvokeResult {
        exit_code: r.get_exit_code(),
        stdout: r.get_stdout()?.to_string()?,
        stderr: r.get_stderr()?.to_string()?,
        payload: crate::rpc::read_extensible_config(r.get_payload()?),
    })
}

/// Encode a `CliSchemaReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_cli_schema_reply(
    b: plugin_capnp::cli_schema_reply::Builder<'_>,
    v: &Result<CliSchema, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_cli_schema(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `CliSchemaReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_cli_schema_reply(
    r: plugin_capnp::cli_schema_reply::Reader<'_>,
) -> capnp::Result<Result<CliSchema, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::cli_schema_reply::Ok(ok) => Ok(Ok(read_cli_schema(ok?)?)),
        plugin_capnp::cli_schema_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a `CliInvokeReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_cli_invoke_reply(
    b: plugin_capnp::cli_invoke_reply::Builder<'_>,
    v: &Result<CliInvokeResult, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_cli_invoke_result(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `CliInvokeReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_cli_invoke_reply(
    r: plugin_capnp::cli_invoke_reply::Reader<'_>,
) -> capnp::Result<Result<CliInvokeResult, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::cli_invoke_reply::Ok(ok) => Ok(Ok(read_cli_invoke_result(ok?)?)),
        plugin_capnp::cli_invoke_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`DatabaseAdapterConfig`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_database_adapter_config(
    mut b: plugin_capnp::database_adapter_config::Builder<'_>,
    v: &DatabaseAdapterConfig,
) -> capnp::Result<()> {
    b.set_plugin_data_dir(&v.plugin_data_dir);
    crate::rpc::write_extensible_config(b.reborrow().init_settings(), &v.settings);
    if let Some(value) = &v.binding {
        b.set_binding(value);
    }
    if let Some(value) = &v.instance_id {
        b.set_instance_id(value);
    }
    b.set_open_existing(v.open_existing);
    Ok(())
}

/// Decode a [`DatabaseAdapterConfig`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_database_adapter_config(
    r: plugin_capnp::database_adapter_config::Reader<'_>,
) -> capnp::Result<DatabaseAdapterConfig> {
    Ok(DatabaseAdapterConfig {
        plugin_data_dir: r.get_plugin_data_dir()?.to_string()?,
        settings: crate::rpc::read_extensible_config(r.get_settings()?),
        binding: opt_text(r.get_binding()?)?,
        instance_id: opt_text(r.get_instance_id()?)?,
        open_existing: r.get_open_existing(),
    })
}

/// Encode a [`DiagnoseResult`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_diagnose_result(
    mut b: plugin_capnp::diagnose_result::Builder<'_>,
    v: &DiagnoseResult,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_lines(list_len(v.lines.len())?);
        for (i, item) in v.lines.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    Ok(())
}

/// Decode a [`DiagnoseResult`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_diagnose_result(
    r: plugin_capnp::diagnose_result::Reader<'_>,
) -> capnp::Result<DiagnoseResult> {
    Ok(DiagnoseResult {
        lines: read_text_list(r.get_lines()?)?,
    })
}

/// Encode a `DiagnoseReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_diagnose_reply(
    b: plugin_capnp::diagnose_reply::Builder<'_>,
    v: &Result<DiagnoseResult, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_diagnose_result(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `DiagnoseReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_diagnose_reply(
    r: plugin_capnp::diagnose_reply::Reader<'_>,
) -> capnp::Result<Result<DiagnoseResult, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::diagnose_reply::Ok(ok) => Ok(Ok(read_diagnose_result(ok?)?)),
        plugin_capnp::diagnose_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`SourceAccount`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_source_account(
    mut b: plugin_capnp::source_account::Builder<'_>,
    v: &SourceAccount,
) -> capnp::Result<()> {
    b.set_account_id(&v.account_id);
    b.set_source(&v.source);
    b.set_marketplace(&v.marketplace);
    if let Some(value) = &v.label {
        b.set_label(value);
    }
    b.set_scan_enabled(v.scan_enabled);
    Ok(())
}

/// Decode a [`SourceAccount`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_source_account(
    r: plugin_capnp::source_account::Reader<'_>,
) -> capnp::Result<SourceAccount> {
    Ok(SourceAccount {
        account_id: r.get_account_id()?.to_string()?,
        source: r.get_source()?.to_string()?,
        marketplace: r.get_marketplace()?.to_string()?,
        label: opt_text(r.get_label()?)?,
        scan_enabled: r.get_scan_enabled(),
    })
}

/// Encode a [`LoginParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_login_params(
    mut b: plugin_capnp::login_params::Builder<'_>,
    v: &LoginParams,
) -> capnp::Result<()> {
    b.set_plugin_data_dir(&v.plugin_data_dir);
    b.set_marketplace(&v.marketplace);
    if let Some(value) = &v.label {
        b.set_label(value);
    }
    if let Some(value) = &v.email {
        b.set_email(value);
    }
    if let Some(value) = &v.password {
        b.set_password(value);
    }
    b.set_force(v.force);
    if let Some(value) = &v.callback_bind {
        b.set_callback_bind(value);
    }
    if let Some(value) = &v.callback_ipc {
        b.set_callback_ipc(value);
    }
    if let Some(value) = &v.callback_public_base {
        b.set_callback_public_base(value);
    }
    b.set_external(v.external);
    if let Some(value) = &v.response_url {
        b.set_response_url(value);
    }
    b.set_show_qr(v.show_qr);
    if let Some(value) = v.timeout_secs {
        b.set_timeout_secs(value);
    }
    crate::rpc::write_extensible_config(b.reborrow().init_extra(), &v.extra);
    Ok(())
}

/// Decode a [`LoginParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_login_params(r: plugin_capnp::login_params::Reader<'_>) -> capnp::Result<LoginParams> {
    Ok(LoginParams {
        plugin_data_dir: r.get_plugin_data_dir()?.to_string()?,
        marketplace: r.get_marketplace()?.to_string()?,
        label: opt_text(r.get_label()?)?,
        email: opt_text(r.get_email()?)?,
        password: opt_text(r.get_password()?)?,
        force: r.get_force(),
        callback_bind: opt_text(r.get_callback_bind()?)?,
        callback_ipc: opt_text(r.get_callback_ipc()?)?,
        callback_public_base: opt_text(r.get_callback_public_base()?)?,
        external: r.get_external(),
        response_url: opt_text(r.get_response_url()?)?,
        show_qr: r.get_show_qr(),
        timeout_secs: opt_num(r.get_timeout_secs()),
        extra: crate::rpc::read_extensible_config(r.get_extra()?),
    })
}

/// Encode a [`LoginResult`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_login_result(
    mut b: plugin_capnp::login_result::Builder<'_>,
    v: &LoginResult,
) -> capnp::Result<()> {
    write_source_account(b.reborrow().init_account(), &v.account)?;
    if let Some(value) = &v.credentials {
        b.set_credentials(value);
    }
    Ok(())
}

/// Decode a [`LoginResult`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_login_result(r: plugin_capnp::login_result::Reader<'_>) -> capnp::Result<LoginResult> {
    Ok(LoginResult {
        account: read_source_account(r.get_account()?)?,
        credentials: opt_data(r.get_credentials()?),
    })
}

/// Encode a [`LoginStartResult`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_login_start_result(
    mut b: plugin_capnp::login_start_result::Builder<'_>,
    v: &LoginStartResult,
) -> capnp::Result<()> {
    b.set_session_id(&v.session_id);
    b.set_url(&v.url);
    Ok(())
}

/// Decode a [`LoginStartResult`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_login_start_result(
    r: plugin_capnp::login_start_result::Reader<'_>,
) -> capnp::Result<LoginStartResult> {
    Ok(LoginStartResult {
        session_id: r.get_session_id()?.to_string()?,
        url: r.get_url()?.to_string()?,
    })
}

/// Encode a [`LoginCompleteParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_login_complete_params(
    mut b: plugin_capnp::login_complete_params::Builder<'_>,
    v: &LoginCompleteParams,
) -> capnp::Result<()> {
    b.set_session_id(&v.session_id);
    Ok(())
}

/// Decode a [`LoginCompleteParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_login_complete_params(
    r: plugin_capnp::login_complete_params::Reader<'_>,
) -> capnp::Result<LoginCompleteParams> {
    Ok(LoginCompleteParams {
        session_id: r.get_session_id()?.to_string()?,
    })
}

/// Encode a `LoginReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_login_reply(
    b: plugin_capnp::login_reply::Builder<'_>,
    v: &Result<LoginResult, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_login_result(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `LoginReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_login_reply(
    r: plugin_capnp::login_reply::Reader<'_>,
) -> capnp::Result<Result<LoginResult, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::login_reply::Ok(ok) => Ok(Ok(read_login_result(ok?)?)),
        plugin_capnp::login_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a `LoginStartReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_login_start_reply(
    b: plugin_capnp::login_start_reply::Builder<'_>,
    v: &Result<LoginStartResult, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_login_start_result(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `LoginStartReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_login_start_reply(
    r: plugin_capnp::login_start_reply::Reader<'_>,
) -> capnp::Result<Result<LoginStartResult, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::login_start_reply::Ok(ok) => Ok(Ok(read_login_start_result(ok?)?)),
        plugin_capnp::login_start_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`AccountCredential`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_account_credential(
    mut b: plugin_capnp::account_credential::Builder<'_>,
    v: &AccountCredential,
) -> capnp::Result<()> {
    b.set_account_id(&v.account_id);
    b.set_credentials(&v.credentials);
    Ok(())
}

/// Decode a [`AccountCredential`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_account_credential(
    r: plugin_capnp::account_credential::Reader<'_>,
) -> capnp::Result<AccountCredential> {
    Ok(AccountCredential {
        account_id: r.get_account_id()?.to_string()?,
        credentials: r.get_credentials()?.to_vec(),
    })
}

/// Encode a [`ScanParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_scan_params(
    mut b: plugin_capnp::scan_params::Builder<'_>,
    v: &ScanParams,
) -> capnp::Result<()> {
    b.set_plugin_data_dir(&v.plugin_data_dir);
    {
        let mut items = b.reborrow().init_accounts(list_len(v.accounts.len())?);
        for (i, item) in v.accounts.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    b.set_page_size(v.page_size);
    b.set_import_episodes(v.import_episodes);
    b.set_import_plus_titles(v.import_plus_titles);
    {
        let mut items = b
            .reborrow()
            .init_credentials(list_len(v.credentials.len())?);
        for (i, item) in v.credentials.iter().enumerate() {
            write_account_credential(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`ScanParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_scan_params(r: plugin_capnp::scan_params::Reader<'_>) -> capnp::Result<ScanParams> {
    Ok(ScanParams {
        plugin_data_dir: r.get_plugin_data_dir()?.to_string()?,
        accounts: read_text_list(r.get_accounts()?)?,
        page_size: r.get_page_size(),
        import_episodes: r.get_import_episodes(),
        import_plus_titles: r.get_import_plus_titles(),
        credentials: r
            .get_credentials()?
            .iter()
            .map(|item| read_account_credential(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a [`ScanBook`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_scan_book(
    mut b: plugin_capnp::scan_book::Builder<'_>,
    v: &ScanBook,
) -> capnp::Result<()> {
    b.set_account_id(&v.account_id);
    b.set_product_id(&v.product_id);
    b.set_title(&v.title);
    if let Some(value) = &v.marketplace {
        b.set_marketplace(value);
    }
    if let Some(value) = &v.asin {
        b.set_asin(value);
    }
    if let Some(value) = &v.isbn {
        b.set_isbn(value);
    }
    if let Some(value) = &v.authors {
        b.set_authors(value);
    }
    if let Some(value) = &v.narrators {
        b.set_narrators(value);
    }
    if let Some(value) = &v.series {
        b.set_series(value);
    }
    if let Some(value) = &v.series_index {
        b.set_series_index(value);
    }
    if let Some(value) = &v.content_kind {
        b.set_content_kind(value);
    }
    if let Some(value) = &v.publisher {
        b.set_publisher(value);
    }
    if let Some(value) = v.length_minutes {
        b.set_length_minutes(value);
    }
    if let Some(value) = &v.subtitle {
        b.set_subtitle(value);
    }
    Ok(())
}

/// Decode a [`ScanBook`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_scan_book(r: plugin_capnp::scan_book::Reader<'_>) -> capnp::Result<ScanBook> {
    Ok(ScanBook {
        account_id: r.get_account_id()?.to_string()?,
        product_id: r.get_product_id()?.to_string()?,
        title: r.get_title()?.to_string()?,
        marketplace: opt_text(r.get_marketplace()?)?,
        asin: opt_text(r.get_asin()?)?,
        isbn: opt_text(r.get_isbn()?)?,
        authors: opt_text(r.get_authors()?)?,
        narrators: opt_text(r.get_narrators()?)?,
        series: opt_text(r.get_series()?)?,
        series_index: opt_text(r.get_series_index()?)?,
        content_kind: opt_text(r.get_content_kind()?)?,
        publisher: opt_text(r.get_publisher()?)?,
        length_minutes: opt_num(r.get_length_minutes()),
        subtitle: opt_text(r.get_subtitle()?)?,
    })
}

/// Encode a [`ScanSummary`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_scan_summary(
    mut b: plugin_capnp::scan_summary::Builder<'_>,
    v: &ScanSummary,
) -> capnp::Result<()> {
    b.set_accounts(v.accounts);
    b.set_books_upserted(v.books_upserted);
    b.set_pages(v.pages);
    b.set_skipped_disabled(v.skipped_disabled);
    {
        let mut items = b.reborrow().init_books(list_len(v.books.len())?);
        for (i, item) in v.books.iter().enumerate() {
            write_scan_book(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`ScanSummary`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_scan_summary(r: plugin_capnp::scan_summary::Reader<'_>) -> capnp::Result<ScanSummary> {
    Ok(ScanSummary {
        accounts: r.get_accounts(),
        books_upserted: r.get_books_upserted(),
        pages: r.get_pages(),
        skipped_disabled: r.get_skipped_disabled(),
        books: r
            .get_books()?
            .iter()
            .map(|item| read_scan_book(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a `ScanReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_scan_reply(
    b: plugin_capnp::scan_reply::Builder<'_>,
    v: &Result<ScanSummary, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_scan_summary(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `ScanReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_scan_reply(
    r: plugin_capnp::scan_reply::Reader<'_>,
) -> capnp::Result<Result<ScanSummary, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::scan_reply::Ok(ok) => Ok(Ok(read_scan_summary(ok?)?)),
        plugin_capnp::scan_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`FetchOptions`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_fetch_options(
    mut b: plugin_capnp::fetch_options::Builder<'_>,
    v: &FetchOptions,
) -> capnp::Result<()> {
    b.set_widevine(v.widevine);
    b.set_xhe_aac(v.xhe_aac);
    if let Some(value) = &v.widevine_cdm_path {
        b.set_widevine_cdm_path(value);
    }
    if let Some(value) = &v.widevine_cdm_provider {
        b.set_widevine_cdm_provider(value);
    }
    b.set_download_cover(v.download_cover);
    b.set_download_pdf(v.download_pdf);
    b.set_cover_size(&v.cover_size);
    b.set_chapter_layout(&v.chapter_layout);
    b.set_strip_audible_brand_audio(v.strip_audible_brand_audio);
    b.set_download_clips_bookmarks(v.download_clips_bookmarks);
    b.set_retain_aax_file(v.retain_aax_file);
    b.set_download_speed_limit_kbps(v.download_speed_limit_kbps);
    b.set_save_metadata_json(v.save_metadata_json);
    Ok(())
}

/// Decode a [`FetchOptions`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_fetch_options(
    r: plugin_capnp::fetch_options::Reader<'_>,
) -> capnp::Result<FetchOptions> {
    Ok(FetchOptions {
        widevine: r.get_widevine(),
        xhe_aac: r.get_xhe_aac(),
        widevine_cdm_path: opt_text(r.get_widevine_cdm_path()?)?,
        widevine_cdm_provider: opt_text(r.get_widevine_cdm_provider()?)?,
        download_cover: r.get_download_cover(),
        download_pdf: r.get_download_pdf(),
        cover_size: r.get_cover_size()?.to_string()?,
        chapter_layout: r.get_chapter_layout()?.to_string()?,
        strip_audible_brand_audio: r.get_strip_audible_brand_audio(),
        download_clips_bookmarks: r.get_download_clips_bookmarks(),
        retain_aax_file: r.get_retain_aax_file(),
        download_speed_limit_kbps: r.get_download_speed_limit_kbps(),
        save_metadata_json: r.get_save_metadata_json(),
    })
}

/// Encode a [`FetchTitleParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_fetch_title_params(
    mut b: plugin_capnp::fetch_title_params::Builder<'_>,
    v: &FetchTitleParams,
) -> capnp::Result<()> {
    b.set_plugin_data_dir(&v.plugin_data_dir);
    b.set_account_id(&v.account_id);
    b.set_title_id(&v.title_id);
    b.set_cache_dir(&v.cache_dir);
    if let Some(value) = &v.credentials {
        b.set_credentials(value);
    }
    crate::rpc::write_extensible_config(b.reborrow().init_source_config(), &v.source_config);
    write_fetch_options(b.reborrow().init_fetch(), &v.fetch)?;
    Ok(())
}

/// Decode a [`FetchTitleParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_fetch_title_params(
    r: plugin_capnp::fetch_title_params::Reader<'_>,
) -> capnp::Result<FetchTitleParams> {
    Ok(FetchTitleParams {
        plugin_data_dir: r.get_plugin_data_dir()?.to_string()?,
        account_id: r.get_account_id()?.to_string()?,
        title_id: r.get_title_id()?.to_string()?,
        cache_dir: r.get_cache_dir()?.to_string()?,
        credentials: opt_data(r.get_credentials()?),
        source_config: crate::rpc::read_extensible_config(r.get_source_config()?),
        fetch: read_fetch_options(r.get_fetch()?)?,
    })
}

/// Encode a [`PlainPart`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_plain_part(
    mut b: plugin_capnp::plain_part::Builder<'_>,
    v: &PlainPart,
) -> capnp::Result<()> {
    b.set_path(&v.path);
    if let Some(value) = &v.title {
        b.set_title(value);
    }
    if let Some(value) = v.duration_ms {
        b.set_duration_ms(value);
    }
    Ok(())
}

/// Decode a [`PlainPart`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_plain_part(r: plugin_capnp::plain_part::Reader<'_>) -> capnp::Result<PlainPart> {
    Ok(PlainPart {
        path: r.get_path()?.to_string()?,
        title: opt_text(r.get_title()?)?,
        duration_ms: opt_num(r.get_duration_ms()),
    })
}

/// Encode a [`ChapterMarker`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_chapter_marker(
    mut b: plugin_capnp::chapter_marker::Builder<'_>,
    v: &ChapterMarker,
) -> capnp::Result<()> {
    b.set_title(&v.title);
    b.set_start_ms(v.start_ms);
    Ok(())
}

/// Decode a [`ChapterMarker`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_chapter_marker(
    r: plugin_capnp::chapter_marker::Reader<'_>,
) -> capnp::Result<ChapterMarker> {
    Ok(ChapterMarker {
        title: r.get_title()?.to_string()?,
        start_ms: r.get_start_ms(),
    })
}

/// Encode a [`PlainFetch`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_plain_fetch(
    mut b: plugin_capnp::plain_fetch::Builder<'_>,
    v: &PlainFetch,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_parts(list_len(v.parts.len())?);
        for (i, item) in v.parts.iter().enumerate() {
            write_plain_part(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    if let Some(value) = &v.m4b_path {
        b.set_m4b_path(value);
    }
    if let Some(value) = &v.cover_path {
        b.set_cover_path(value);
    }
    {
        let mut items = b.reborrow().init_chapters(list_len(v.chapters.len())?);
        for (i, item) in v.chapters.iter().enumerate() {
            write_chapter_marker(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    if let Some(value) = &v.pdf_url {
        b.set_pdf_url(value);
    }
    Ok(())
}

/// Decode a [`PlainFetch`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_plain_fetch(r: plugin_capnp::plain_fetch::Reader<'_>) -> capnp::Result<PlainFetch> {
    Ok(PlainFetch {
        parts: r
            .get_parts()?
            .iter()
            .map(|item| read_plain_part(item))
            .collect::<capnp::Result<Vec<_>>>()?,
        m4b_path: opt_text(r.get_m4b_path()?)?,
        cover_path: opt_text(r.get_cover_path()?)?,
        chapters: r
            .get_chapters()?
            .iter()
            .map(|item| read_chapter_marker(item))
            .collect::<capnp::Result<Vec<_>>>()?,
        pdf_url: opt_text(r.get_pdf_url()?)?,
    })
}

/// Encode a `FetchTitleReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_fetch_title_reply(
    b: plugin_capnp::fetch_title_reply::Builder<'_>,
    v: &Result<PlainFetch, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_plain_fetch(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `FetchTitleReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_fetch_title_reply(
    r: plugin_capnp::fetch_title_reply::Reader<'_>,
) -> capnp::Result<Result<PlainFetch, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::fetch_title_reply::Ok(ok) => Ok(Ok(read_plain_fetch(ok?)?)),
        plugin_capnp::fetch_title_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`SourceAccounts`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_source_accounts(
    mut b: plugin_capnp::source_accounts::Builder<'_>,
    v: &SourceAccounts,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_accounts(list_len(v.accounts.len())?);
        for (i, item) in v.accounts.iter().enumerate() {
            write_source_account(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`SourceAccounts`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_source_accounts(
    r: plugin_capnp::source_accounts::Reader<'_>,
) -> capnp::Result<SourceAccounts> {
    Ok(SourceAccounts {
        accounts: r
            .get_accounts()?
            .iter()
            .map(|item| read_source_account(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a `SourceAccountsReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_source_accounts_reply(
    b: plugin_capnp::source_accounts_reply::Builder<'_>,
    v: &Result<SourceAccounts, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_source_accounts(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `SourceAccountsReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_source_accounts_reply(
    r: plugin_capnp::source_accounts_reply::Reader<'_>,
) -> capnp::Result<Result<SourceAccounts, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::source_accounts_reply::Ok(ok) => Ok(Ok(read_source_accounts(ok?)?)),
        plugin_capnp::source_accounts_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`SearchCatalogParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_search_catalog_params(
    mut b: plugin_capnp::search_catalog_params::Builder<'_>,
    v: &SearchCatalogParams,
) -> capnp::Result<()> {
    b.set_query(&v.query);
    b.set_region(&v.region);
    b.set_limit(v.limit);
    b.set_page(v.page);
    b.set_sort(v.sort.into());
    b.set_field(v.field.into());
    if let Some(value) = &v.language {
        b.set_language(value);
    }
    Ok(())
}

/// Decode a [`SearchCatalogParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_search_catalog_params(
    r: plugin_capnp::search_catalog_params::Reader<'_>,
) -> capnp::Result<SearchCatalogParams> {
    Ok(SearchCatalogParams {
        query: r.get_query()?.to_string()?,
        region: r.get_region()?.to_string()?,
        limit: r.get_limit(),
        page: r.get_page(),
        sort: r.get_sort()?.into(),
        field: r.get_field()?.into(),
        language: opt_text(r.get_language()?)?,
    })
}

/// Encode a [`ExpandCandidatesParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_expand_candidates_params(
    mut b: plugin_capnp::expand_candidates_params::Builder<'_>,
    v: &ExpandCandidatesParams,
) -> capnp::Result<()> {
    b.set_source(&v.source);
    b.set_product_id(&v.product_id);
    b.set_title(&v.title);
    if let Some(value) = &v.authors {
        b.set_authors(value);
    }
    if let Some(value) = &v.narrators {
        b.set_narrators(value);
    }
    if let Some(value) = &v.series {
        b.set_series(value);
    }
    if let Some(value) = &v.series_asin {
        b.set_series_asin(value);
    }
    if let Some(value) = &v.asin {
        b.set_asin(value);
    }
    if let Some(value) = &v.isbn {
        b.set_isbn(value);
    }
    b.set_region(&v.region);
    b.set_limit(v.limit);
    Ok(())
}

/// Decode a [`ExpandCandidatesParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_expand_candidates_params(
    r: plugin_capnp::expand_candidates_params::Reader<'_>,
) -> capnp::Result<ExpandCandidatesParams> {
    Ok(ExpandCandidatesParams {
        source: r.get_source()?.to_string()?,
        product_id: r.get_product_id()?.to_string()?,
        title: r.get_title()?.to_string()?,
        authors: opt_text(r.get_authors()?)?,
        narrators: opt_text(r.get_narrators()?)?,
        series: opt_text(r.get_series()?)?,
        series_asin: opt_text(r.get_series_asin()?)?,
        asin: opt_text(r.get_asin()?)?,
        isbn: opt_text(r.get_isbn()?)?,
        region: r.get_region()?.to_string()?,
        limit: r.get_limit(),
    })
}

/// Encode a [`PurchaseHintParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_purchase_hint_params(
    mut b: plugin_capnp::purchase_hint_params::Builder<'_>,
    v: &PurchaseHintParams,
) -> capnp::Result<()> {
    if let Some(value) = &v.product_id {
        b.set_product_id(value);
    }
    if let Some(value) = &v.title {
        b.set_title(value);
    }
    if let Some(value) = &v.authors {
        b.set_authors(value);
    }
    if let Some(value) = &v.asin {
        b.set_asin(value);
    }
    if let Some(value) = &v.isbn {
        b.set_isbn(value);
    }
    b.set_region(&v.region);
    b.set_with_price(v.with_price);
    Ok(())
}

/// Decode a [`PurchaseHintParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_purchase_hint_params(
    r: plugin_capnp::purchase_hint_params::Reader<'_>,
) -> capnp::Result<PurchaseHintParams> {
    Ok(PurchaseHintParams {
        product_id: opt_text(r.get_product_id()?)?,
        title: opt_text(r.get_title()?)?,
        authors: opt_text(r.get_authors()?)?,
        asin: opt_text(r.get_asin()?)?,
        isbn: opt_text(r.get_isbn()?)?,
        region: r.get_region()?.to_string()?,
        with_price: r.get_with_price(),
    })
}

/// Encode a [`ListDealsParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_list_deals_params(
    mut b: plugin_capnp::list_deals_params::Builder<'_>,
    v: &ListDealsParams,
) -> capnp::Result<()> {
    if let Some(value) = v.limit {
        b.set_limit(value);
    }
    Ok(())
}

/// Decode a [`ListDealsParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_list_deals_params(
    r: plugin_capnp::list_deals_params::Reader<'_>,
) -> capnp::Result<ListDealsParams> {
    Ok(ListDealsParams {
        limit: opt_num(r.get_limit()),
    })
}

/// Encode a [`CatalogDetailParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_catalog_detail_params(
    mut b: plugin_capnp::catalog_detail_params::Builder<'_>,
    v: &CatalogDetailParams,
) -> capnp::Result<()> {
    b.set_product_id(&v.product_id);
    if let Some(value) = &v.isbn {
        b.set_isbn(value);
    }
    Ok(())
}

/// Decode a [`CatalogDetailParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_catalog_detail_params(
    r: plugin_capnp::catalog_detail_params::Reader<'_>,
) -> capnp::Result<CatalogDetailParams> {
    Ok(CatalogDetailParams {
        product_id: r.get_product_id()?.to_string()?,
        isbn: opt_text(r.get_isbn()?)?,
    })
}

/// Encode a [`CatalogHit`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_catalog_hit(
    mut b: plugin_capnp::catalog_hit::Builder<'_>,
    v: &CatalogHit,
) -> capnp::Result<()> {
    b.set_product_id(&v.product_id);
    b.set_title(&v.title);
    if let Some(value) = &v.authors {
        b.set_authors(value);
    }
    if let Some(value) = &v.narrators {
        b.set_narrators(value);
    }
    if let Some(value) = &v.series {
        b.set_series(value);
    }
    if let Some(value) = &v.series_index {
        b.set_series_index(value);
    }
    if let Some(value) = &v.asin {
        b.set_asin(value);
    }
    if let Some(value) = &v.isbn {
        b.set_isbn(value);
    }
    if let Some(value) = &v.url {
        b.set_url(value);
    }
    if let Some(value) = &v.cover_url {
        b.set_cover_url(value);
    }
    b.set_origin(&v.origin);
    if let Some(value) = &v.subtitle {
        b.set_subtitle(value);
    }
    if let Some(value) = &v.description {
        b.set_description(value);
    }
    if let Some(value) = &v.publisher {
        b.set_publisher(value);
    }
    if let Some(value) = v.length_minutes {
        b.set_length_minutes(value);
    }
    if let Some(value) = &v.published_at {
        b.set_published_at(value);
    }
    if let Some(value) = &v.categories {
        b.set_categories(value);
    }
    if let Some(value) = &v.language {
        b.set_language(value);
    }
    if let Some(value) = v.price_cents {
        b.set_price_cents(value);
    }
    if let Some(value) = &v.currency {
        b.set_currency(value);
    }
    if let Some(value) = &v.price_label {
        b.set_price_label(value);
    }
    if let Some(value) = v.rating_overall {
        b.set_rating_overall(value);
    }
    if let Some(value) = v.rating_count {
        b.set_rating_count(value);
    }
    b.set_abridgement(v.abridgement.into());
    Ok(())
}

/// Decode a [`CatalogHit`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_catalog_hit(r: plugin_capnp::catalog_hit::Reader<'_>) -> capnp::Result<CatalogHit> {
    Ok(CatalogHit {
        product_id: r.get_product_id()?.to_string()?,
        title: r.get_title()?.to_string()?,
        authors: opt_text(r.get_authors()?)?,
        narrators: opt_text(r.get_narrators()?)?,
        series: opt_text(r.get_series()?)?,
        series_index: opt_text(r.get_series_index()?)?,
        asin: opt_text(r.get_asin()?)?,
        isbn: opt_text(r.get_isbn()?)?,
        url: opt_text(r.get_url()?)?,
        cover_url: opt_text(r.get_cover_url()?)?,
        origin: r.get_origin()?.to_string()?,
        subtitle: opt_text(r.get_subtitle()?)?,
        description: opt_text(r.get_description()?)?,
        publisher: opt_text(r.get_publisher()?)?,
        length_minutes: opt_num(r.get_length_minutes()),
        published_at: opt_text(r.get_published_at()?)?,
        categories: opt_text(r.get_categories()?)?,
        language: opt_text(r.get_language()?)?,
        price_cents: opt_num(r.get_price_cents()),
        currency: opt_text(r.get_currency()?)?,
        price_label: opt_text(r.get_price_label()?)?,
        rating_overall: opt_num(r.get_rating_overall()),
        rating_count: opt_num(r.get_rating_count()),
        abridgement: r.get_abridgement()?.into(),
    })
}

/// Encode a [`CatalogHits`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_catalog_hits(
    mut b: plugin_capnp::catalog_hits::Builder<'_>,
    v: &CatalogHits,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_hits(list_len(v.hits.len())?);
        for (i, item) in v.hits.iter().enumerate() {
            write_catalog_hit(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`CatalogHits`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_catalog_hits(r: plugin_capnp::catalog_hits::Reader<'_>) -> capnp::Result<CatalogHits> {
    Ok(CatalogHits {
        hits: r
            .get_hits()?
            .iter()
            .map(|item| read_catalog_hit(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a `CatalogHitsReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_catalog_hits_reply(
    b: plugin_capnp::catalog_hits_reply::Builder<'_>,
    v: &Result<CatalogHits, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_catalog_hits(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `CatalogHitsReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_catalog_hits_reply(
    r: plugin_capnp::catalog_hits_reply::Reader<'_>,
) -> capnp::Result<Result<CatalogHits, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::catalog_hits_reply::Ok(ok) => Ok(Ok(read_catalog_hits(ok?)?)),
        plugin_capnp::catalog_hits_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`CatalogDetail`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_catalog_detail(
    mut b: plugin_capnp::catalog_detail::Builder<'_>,
    v: &CatalogDetail,
) -> capnp::Result<()> {
    b.set_found(v.found);
    write_catalog_hit(b.reborrow().init_hit(), &v.hit)?;
    Ok(())
}

/// Decode a [`CatalogDetail`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_catalog_detail(
    r: plugin_capnp::catalog_detail::Reader<'_>,
) -> capnp::Result<CatalogDetail> {
    Ok(CatalogDetail {
        found: r.get_found(),
        hit: read_catalog_hit(r.get_hit()?)?,
    })
}

/// Encode a `CatalogDetailReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_catalog_detail_reply(
    b: plugin_capnp::catalog_detail_reply::Builder<'_>,
    v: &Result<CatalogDetail, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_catalog_detail(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `CatalogDetailReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_catalog_detail_reply(
    r: plugin_capnp::catalog_detail_reply::Reader<'_>,
) -> capnp::Result<Result<CatalogDetail, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::catalog_detail_reply::Ok(ok) => Ok(Ok(read_catalog_detail(ok?)?)),
        plugin_capnp::catalog_detail_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`PurchaseHint`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_purchase_hint(
    mut b: plugin_capnp::purchase_hint::Builder<'_>,
    v: &PurchaseHint,
) -> capnp::Result<()> {
    b.set_product_id(&v.product_id);
    if let Some(value) = &v.title {
        b.set_title(value);
    }
    if let Some(value) = &v.url {
        b.set_url(value);
    }
    if let Some(value) = v.price_cents {
        b.set_price_cents(value);
    }
    if let Some(value) = &v.currency {
        b.set_currency(value);
    }
    if let Some(value) = &v.price_label {
        b.set_price_label(value);
    }
    if let Some(value) = v.list_price_cents {
        b.set_list_price_cents(value);
    }
    if let Some(value) = &v.list_price_label {
        b.set_list_price_label(value);
    }
    if let Some(value) = v.member_price_cents {
        b.set_member_price_cents(value);
    }
    if let Some(value) = &v.member_price_label {
        b.set_member_price_label(value);
    }
    Ok(())
}

/// Decode a [`PurchaseHint`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_purchase_hint(
    r: plugin_capnp::purchase_hint::Reader<'_>,
) -> capnp::Result<PurchaseHint> {
    Ok(PurchaseHint {
        product_id: r.get_product_id()?.to_string()?,
        title: opt_text(r.get_title()?)?,
        url: opt_text(r.get_url()?)?,
        price_cents: opt_num(r.get_price_cents()),
        currency: opt_text(r.get_currency()?)?,
        price_label: opt_text(r.get_price_label()?)?,
        list_price_cents: opt_num(r.get_list_price_cents()),
        list_price_label: opt_text(r.get_list_price_label()?)?,
        member_price_cents: opt_num(r.get_member_price_cents()),
        member_price_label: opt_text(r.get_member_price_label()?)?,
    })
}

/// Encode a [`PurchaseHintResult`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_purchase_hint_result(
    mut b: plugin_capnp::purchase_hint_result::Builder<'_>,
    v: &PurchaseHintResult,
) -> capnp::Result<()> {
    b.set_found(v.found);
    write_purchase_hint(b.reborrow().init_hint(), &v.hint)?;
    Ok(())
}

/// Decode a [`PurchaseHintResult`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_purchase_hint_result(
    r: plugin_capnp::purchase_hint_result::Reader<'_>,
) -> capnp::Result<PurchaseHintResult> {
    Ok(PurchaseHintResult {
        found: r.get_found(),
        hint: read_purchase_hint(r.get_hint()?)?,
    })
}

/// Encode a `PurchaseHintReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_purchase_hint_reply(
    b: plugin_capnp::purchase_hint_reply::Builder<'_>,
    v: &Result<PurchaseHintResult, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_purchase_hint_result(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `PurchaseHintReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_purchase_hint_reply(
    r: plugin_capnp::purchase_hint_reply::Reader<'_>,
) -> capnp::Result<Result<PurchaseHintResult, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::purchase_hint_reply::Ok(ok) => Ok(Ok(read_purchase_hint_result(ok?)?)),
        plugin_capnp::purchase_hint_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`ScanLibraryParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_scan_library_params(
    mut b: plugin_capnp::scan_library_params::Builder<'_>,
    v: &ScanLibraryParams,
) -> capnp::Result<()> {
    b.set_force(v.force);
    Ok(())
}

/// Decode a [`ScanLibraryParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_scan_library_params(
    r: plugin_capnp::scan_library_params::Reader<'_>,
) -> capnp::Result<ScanLibraryParams> {
    Ok(ScanLibraryParams {
        force: r.get_force(),
    })
}

/// Encode a [`AuthenticateUserParams`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_authenticate_user_params(
    mut b: plugin_capnp::authenticate_user_params::Builder<'_>,
    v: &AuthenticateUserParams,
) -> capnp::Result<()> {
    b.set_username(&v.username);
    b.set_password(&v.password);
    Ok(())
}

/// Decode a [`AuthenticateUserParams`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_authenticate_user_params(
    r: plugin_capnp::authenticate_user_params::Reader<'_>,
) -> capnp::Result<AuthenticateUserParams> {
    Ok(AuthenticateUserParams {
        username: r.get_username()?.to_string()?,
        password: r.get_password()?.to_string()?,
    })
}

/// Encode a [`ExternalUser`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_external_user(
    mut b: plugin_capnp::external_user::Builder<'_>,
    v: &ExternalUser,
) -> capnp::Result<()> {
    b.set_provider(&v.provider);
    b.set_external_user_id(&v.external_user_id);
    if let Some(value) = &v.display_name {
        b.set_display_name(value);
    }
    if let Some(value) = &v.access_token {
        b.set_access_token(value);
    }
    Ok(())
}

/// Decode a [`ExternalUser`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_external_user(
    r: plugin_capnp::external_user::Reader<'_>,
) -> capnp::Result<ExternalUser> {
    Ok(ExternalUser {
        provider: r.get_provider()?.to_string()?,
        external_user_id: r.get_external_user_id()?.to_string()?,
        display_name: opt_text(r.get_display_name()?)?,
        access_token: opt_text(r.get_access_token()?)?,
    })
}

/// Encode a `ExternalUserReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_external_user_reply(
    b: plugin_capnp::external_user_reply::Builder<'_>,
    v: &Result<ExternalUser, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_external_user(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `ExternalUserReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_external_user_reply(
    r: plugin_capnp::external_user_reply::Reader<'_>,
) -> capnp::Result<Result<ExternalUser, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::external_user_reply::Ok(ok) => Ok(Ok(read_external_user(ok?)?)),
        plugin_capnp::external_user_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`EventPollResult`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_event_poll_result(
    mut b: plugin_capnp::event_poll_result::Builder<'_>,
    v: &EventPollResult,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_users(list_len(v.users.len())?);
        for (i, item) in v.users.iter().enumerate() {
            write_external_user(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`EventPollResult`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_event_poll_result(
    r: plugin_capnp::event_poll_result::Reader<'_>,
) -> capnp::Result<EventPollResult> {
    Ok(EventPollResult {
        users: r
            .get_users()?
            .iter()
            .map(|item| read_external_user(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a `EventPollReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_event_poll_reply(
    b: plugin_capnp::event_poll_reply::Builder<'_>,
    v: &Result<EventPollResult, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_event_poll_result(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `EventPollReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_event_poll_reply(
    r: plugin_capnp::event_poll_reply::Reader<'_>,
) -> capnp::Result<Result<EventPollResult, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::event_poll_reply::Ok(ok) => Ok(Ok(read_event_poll_result(ok?)?)),
        plugin_capnp::event_poll_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`ListeningProgress`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_listening_progress(
    mut b: plugin_capnp::listening_progress::Builder<'_>,
    v: &ListeningProgress,
) -> capnp::Result<()> {
    b.set_external_user_id(&v.external_user_id);
    b.set_external_item_id(&v.external_item_id);
    if let Some(value) = v.identity_id {
        b.set_identity_id(value);
    }
    if let Some(value) = &v.title {
        b.set_title(value);
    }
    if let Some(value) = &v.authors {
        b.set_authors(value);
    }
    if let Some(value) = &v.asin {
        b.set_asin(value);
    }
    if let Some(value) = &v.isbn {
        b.set_isbn(value);
    }
    if let Some(value) = v.progress {
        b.set_progress(value);
    }
    if let Some(value) = v.current_time_seconds {
        b.set_current_time_seconds(value);
    }
    if let Some(value) = v.duration_seconds {
        b.set_duration_seconds(value);
    }
    b.set_is_finished(v.is_finished);
    if let Some(value) = v.last_listened_at_unix_ms {
        b.set_last_listened_at_unix_ms(value);
    }
    Ok(())
}

/// Decode a [`ListeningProgress`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_listening_progress(
    r: plugin_capnp::listening_progress::Reader<'_>,
) -> capnp::Result<ListeningProgress> {
    Ok(ListeningProgress {
        external_user_id: r.get_external_user_id()?.to_string()?,
        external_item_id: r.get_external_item_id()?.to_string()?,
        identity_id: opt_num(r.get_identity_id()),
        title: opt_text(r.get_title()?)?,
        authors: opt_text(r.get_authors()?)?,
        asin: opt_text(r.get_asin()?)?,
        isbn: opt_text(r.get_isbn()?)?,
        progress: opt_num(r.get_progress()),
        current_time_seconds: opt_num(r.get_current_time_seconds()),
        duration_seconds: opt_num(r.get_duration_seconds()),
        is_finished: r.get_is_finished(),
        last_listened_at_unix_ms: opt_num(r.get_last_listened_at_unix_ms()),
    })
}

/// Encode a [`SyncListeningResult`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_sync_listening_result(
    mut b: plugin_capnp::sync_listening_result::Builder<'_>,
    v: &SyncListeningResult,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_items(list_len(v.items.len())?);
        for (i, item) in v.items.iter().enumerate() {
            write_listening_progress(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`SyncListeningResult`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_sync_listening_result(
    r: plugin_capnp::sync_listening_result::Reader<'_>,
) -> capnp::Result<SyncListeningResult> {
    Ok(SyncListeningResult {
        items: r
            .get_items()?
            .iter()
            .map(|item| read_listening_progress(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a `SyncListeningReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_sync_listening_reply(
    b: plugin_capnp::sync_listening_reply::Builder<'_>,
    v: &Result<SyncListeningResult, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_sync_listening_result(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `SyncListeningReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_sync_listening_reply(
    r: plugin_capnp::sync_listening_reply::Reader<'_>,
) -> capnp::Result<Result<SyncListeningResult, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::sync_listening_reply::Ok(ok) => Ok(Ok(read_sync_listening_result(ok?)?)),
        plugin_capnp::sync_listening_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`PluginEvent`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_plugin_event(
    mut b: plugin_capnp::plugin_event::Builder<'_>,
    v: &PluginEvent,
) -> capnp::Result<()> {
    b.set_event_type(&v.event_type);
    b.set_schema_version(v.schema_version);
    b.set_deduplication_key(&v.deduplication_key);
    b.set_payload(&v.payload);
    b.set_occurred_at_unix_ms(v.occurred_at_unix_ms);
    b.set_correlation_id(&v.correlation_id);
    b.set_causation_id(&v.causation_id);
    Ok(())
}

/// Decode a [`PluginEvent`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_plugin_event(r: plugin_capnp::plugin_event::Reader<'_>) -> capnp::Result<PluginEvent> {
    Ok(PluginEvent {
        event_type: r.get_event_type()?.to_string()?,
        schema_version: r.get_schema_version(),
        deduplication_key: r.get_deduplication_key()?.to_string()?,
        payload: r.get_payload()?.to_vec(),
        occurred_at_unix_ms: r.get_occurred_at_unix_ms(),
        correlation_id: r.get_correlation_id()?.to_string()?,
        causation_id: r.get_causation_id()?.to_string()?,
    })
}

/// Encode a [`PublishOk`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_publish_ok(
    mut b: plugin_capnp::publish_ok::Builder<'_>,
    v: &PublishOk,
) -> capnp::Result<()> {
    b.set_event_id(&v.event_id);
    b.set_duplicate(v.duplicate);
    Ok(())
}

/// Decode a [`PublishOk`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_publish_ok(r: plugin_capnp::publish_ok::Reader<'_>) -> capnp::Result<PublishOk> {
    Ok(PublishOk {
        event_id: r.get_event_id()?.to_string()?,
        duplicate: r.get_duplicate(),
    })
}

/// Encode a `PublishReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_publish_reply(
    b: plugin_capnp::publish_reply::Builder<'_>,
    v: &Result<PublishOk, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_publish_ok(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `PublishReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_publish_reply(
    r: plugin_capnp::publish_reply::Reader<'_>,
) -> capnp::Result<Result<PublishOk, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::publish_reply::Ok(ok) => Ok(Ok(read_publish_ok(ok?)?)),
        plugin_capnp::publish_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`Invocation`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_invocation(
    mut b: plugin_capnp::invocation::Builder<'_>,
    v: &Invocation,
) -> capnp::Result<()> {
    b.set_id(&v.id);
    b.set_account_id(&v.account_id);
    b.set_deadline_unix_ms(v.deadline_unix_ms);
    b.set_correlation_id(&v.correlation_id);
    b.set_causation_id(&v.causation_id);
    Ok(())
}

/// Decode a [`Invocation`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_invocation(r: plugin_capnp::invocation::Reader<'_>) -> capnp::Result<Invocation> {
    Ok(Invocation {
        id: r.get_id()?.to_string()?,
        account_id: r.get_account_id()?.to_string()?,
        deadline_unix_ms: r.get_deadline_unix_ms(),
        correlation_id: r.get_correlation_id()?.to_string()?,
        causation_id: r.get_causation_id()?.to_string()?,
    })
}

/// Encode a [`QueueMessage`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_queue_message(
    mut b: plugin_capnp::queue_message::Builder<'_>,
    v: &QueueMessage,
) -> capnp::Result<()> {
    b.set_id(&v.id);
    b.set_timestamp_unix_ms(v.timestamp_unix_ms);
    b.set_body(&v.body);
    b.set_content_type(&v.content_type);
    b.set_attempts(v.attempts);
    b.set_source(&v.source);
    Ok(())
}

/// Decode a [`QueueMessage`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_queue_message(
    r: plugin_capnp::queue_message::Reader<'_>,
) -> capnp::Result<QueueMessage> {
    Ok(QueueMessage {
        id: r.get_id()?.to_string()?,
        timestamp_unix_ms: r.get_timestamp_unix_ms(),
        body: r.get_body()?.to_vec(),
        content_type: r.get_content_type()?.to_string()?,
        attempts: r.get_attempts(),
        source: r.get_source()?.to_string()?,
    })
}

/// Encode a [`QueueBatch`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_queue_batch(
    mut b: plugin_capnp::queue_batch::Builder<'_>,
    v: &QueueBatch,
) -> capnp::Result<()> {
    b.set_queue(&v.queue);
    {
        let mut items = b.reborrow().init_messages(list_len(v.messages.len())?);
        for (i, item) in v.messages.iter().enumerate() {
            write_queue_message(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`QueueBatch`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_queue_batch(r: plugin_capnp::queue_batch::Reader<'_>) -> capnp::Result<QueueBatch> {
    Ok(QueueBatch {
        queue: r.get_queue()?.to_string()?,
        messages: r
            .get_messages()?
            .iter()
            .map(|item| read_queue_message(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a [`QueueRetry`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_queue_retry(
    mut b: plugin_capnp::queue_retry::Builder<'_>,
    v: &QueueRetry,
) -> capnp::Result<()> {
    b.set_delay_seconds(v.delay_seconds);
    b.set_reason(&v.reason);
    Ok(())
}

/// Decode a [`QueueRetry`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_queue_retry(r: plugin_capnp::queue_retry::Reader<'_>) -> capnp::Result<QueueRetry> {
    Ok(QueueRetry {
        delay_seconds: r.get_delay_seconds(),
        reason: r.get_reason()?.to_string()?,
    })
}

/// Encode a `QueueResult` union.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_queue_result(
    mut b: plugin_capnp::queue_result::Builder<'_>,
    v: &QueueResult,
) -> capnp::Result<()> {
    match v {
        QueueResult::Ack => b.set_ack(()),
        QueueResult::Retry(value) => {
            write_queue_retry(b.init_retry(), value)?;
        }
    }
    Ok(())
}

/// Decode a `QueueResult` union.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_queue_result(r: plugin_capnp::queue_result::Reader<'_>) -> capnp::Result<QueueResult> {
    Ok(match r.which()? {
        plugin_capnp::queue_result::Ack(()) => QueueResult::Ack,
        plugin_capnp::queue_result::Retry(value) => QueueResult::Retry(read_queue_retry(value?)?),
    })
}

/// Encode a [`QueueBatchOk`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_queue_batch_ok(
    mut b: plugin_capnp::queue_batch_ok::Builder<'_>,
    v: &QueueBatchOk,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_results(list_len(v.results.len())?);
        for (i, item) in v.results.iter().enumerate() {
            write_queue_result(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    Ok(())
}

/// Decode a [`QueueBatchOk`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_queue_batch_ok(
    r: plugin_capnp::queue_batch_ok::Reader<'_>,
) -> capnp::Result<QueueBatchOk> {
    Ok(QueueBatchOk {
        results: r
            .get_results()?
            .iter()
            .map(|item| read_queue_result(item))
            .collect::<capnp::Result<Vec<_>>>()?,
    })
}

/// Encode a `QueueBatchReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_queue_batch_reply(
    b: plugin_capnp::queue_batch_reply::Builder<'_>,
    v: &Result<QueueBatchOk, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_queue_batch_ok(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `QueueBatchReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_queue_batch_reply(
    r: plugin_capnp::queue_batch_reply::Reader<'_>,
) -> capnp::Result<Result<QueueBatchOk, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::queue_batch_reply::Ok(ok) => Ok(Ok(read_queue_batch_ok(ok?)?)),
        plugin_capnp::queue_batch_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`QueueSendMessage`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_queue_send_message(
    mut b: plugin_capnp::queue_send_message::Builder<'_>,
    v: &QueueSendMessage,
) -> capnp::Result<()> {
    b.set_body(&v.body);
    b.set_content_type(&v.content_type);
    b.set_delay_seconds(v.delay_seconds);
    Ok(())
}

/// Decode a [`QueueSendMessage`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_queue_send_message(
    r: plugin_capnp::queue_send_message::Reader<'_>,
) -> capnp::Result<QueueSendMessage> {
    Ok(QueueSendMessage {
        body: r.get_body()?.to_vec(),
        content_type: r.get_content_type()?.to_string()?,
        delay_seconds: r.get_delay_seconds(),
    })
}

/// Encode a [`QueueSendOk`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_queue_send_ok(
    mut b: plugin_capnp::queue_send_ok::Builder<'_>,
    v: &QueueSendOk,
) -> capnp::Result<()> {
    {
        let mut items = b
            .reborrow()
            .init_message_ids(list_len(v.message_ids.len())?);
        for (i, item) in v.message_ids.iter().enumerate() {
            items.set(list_len(i)?, item);
        }
    }
    Ok(())
}

/// Decode a [`QueueSendOk`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_queue_send_ok(
    r: plugin_capnp::queue_send_ok::Reader<'_>,
) -> capnp::Result<QueueSendOk> {
    Ok(QueueSendOk {
        message_ids: read_text_list(r.get_message_ids()?)?,
    })
}

/// Encode a `QueueSendReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_queue_send_reply(
    b: plugin_capnp::queue_send_reply::Builder<'_>,
    v: &Result<QueueSendOk, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_queue_send_ok(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `QueueSendReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_queue_send_reply(
    r: plugin_capnp::queue_send_reply::Reader<'_>,
) -> capnp::Result<Result<QueueSendOk, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::queue_send_reply::Ok(ok) => Ok(Ok(read_queue_send_ok(ok?)?)),
        plugin_capnp::queue_send_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`KvPutOptions`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_kv_put_options(
    mut b: plugin_capnp::kv_put_options::Builder<'_>,
    v: &KvPutOptions,
) -> capnp::Result<()> {
    b.set_expiration_unix_ms(v.expiration_unix_ms);
    b.set_expiration_ttl_seconds(v.expiration_ttl_seconds);
    b.set_metadata(&v.metadata);
    Ok(())
}

/// Decode a [`KvPutOptions`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_kv_put_options(
    r: plugin_capnp::kv_put_options::Reader<'_>,
) -> capnp::Result<KvPutOptions> {
    Ok(KvPutOptions {
        expiration_unix_ms: r.get_expiration_unix_ms(),
        expiration_ttl_seconds: r.get_expiration_ttl_seconds(),
        metadata: r.get_metadata()?.to_string()?,
    })
}

/// Encode a [`KvGetOk`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_kv_get_ok(
    mut b: plugin_capnp::kv_get_ok::Builder<'_>,
    v: &KvGetOk,
) -> capnp::Result<()> {
    b.set_found(v.found);
    b.set_value(&v.value);
    b.set_metadata(&v.metadata);
    b.set_expiration_unix_ms(v.expiration_unix_ms);
    Ok(())
}

/// Decode a [`KvGetOk`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_kv_get_ok(r: plugin_capnp::kv_get_ok::Reader<'_>) -> capnp::Result<KvGetOk> {
    Ok(KvGetOk {
        found: r.get_found(),
        value: r.get_value()?.to_vec(),
        metadata: r.get_metadata()?.to_string()?,
        expiration_unix_ms: r.get_expiration_unix_ms(),
    })
}

/// Encode a `KvGetReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_kv_get_reply(
    b: plugin_capnp::kv_get_reply::Builder<'_>,
    v: &Result<KvGetOk, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_kv_get_ok(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `KvGetReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_kv_get_reply(
    r: plugin_capnp::kv_get_reply::Reader<'_>,
) -> capnp::Result<Result<KvGetOk, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::kv_get_reply::Ok(ok) => Ok(Ok(read_kv_get_ok(ok?)?)),
        plugin_capnp::kv_get_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`KvPutOk`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_kv_put_ok(
    mut b: plugin_capnp::kv_put_ok::Builder<'_>,
    v: &KvPutOk,
) -> capnp::Result<()> {
    b.set_expiration_unix_ms(v.expiration_unix_ms);
    Ok(())
}

/// Decode a [`KvPutOk`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_kv_put_ok(r: plugin_capnp::kv_put_ok::Reader<'_>) -> capnp::Result<KvPutOk> {
    Ok(KvPutOk {
        expiration_unix_ms: r.get_expiration_unix_ms(),
    })
}

/// Encode a [`KvDeleteOk`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_kv_delete_ok(
    mut b: plugin_capnp::kv_delete_ok::Builder<'_>,
    v: &KvDeleteOk,
) -> capnp::Result<()> {
    b.set_existed(v.existed);
    Ok(())
}

/// Decode a [`KvDeleteOk`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_kv_delete_ok(r: plugin_capnp::kv_delete_ok::Reader<'_>) -> capnp::Result<KvDeleteOk> {
    Ok(KvDeleteOk {
        existed: r.get_existed(),
    })
}

/// Encode a `KvPutReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_kv_put_reply(
    b: plugin_capnp::kv_put_reply::Builder<'_>,
    v: &Result<KvPutOk, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_kv_put_ok(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `KvPutReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_kv_put_reply(
    r: plugin_capnp::kv_put_reply::Reader<'_>,
) -> capnp::Result<Result<KvPutOk, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::kv_put_reply::Ok(ok) => Ok(Ok(read_kv_put_ok(ok?)?)),
        plugin_capnp::kv_put_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a `KvDeleteReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_kv_delete_reply(
    b: plugin_capnp::kv_delete_reply::Builder<'_>,
    v: &Result<KvDeleteOk, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_kv_delete_ok(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `KvDeleteReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_kv_delete_reply(
    r: plugin_capnp::kv_delete_reply::Reader<'_>,
) -> capnp::Result<Result<KvDeleteOk, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::kv_delete_reply::Ok(ok) => Ok(Ok(read_kv_delete_ok(ok?)?)),
        plugin_capnp::kv_delete_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Encode a [`KvListOptions`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_kv_list_options(
    mut b: plugin_capnp::kv_list_options::Builder<'_>,
    v: &KvListOptions,
) -> capnp::Result<()> {
    b.set_prefix(&v.prefix);
    b.set_cursor(&v.cursor);
    b.set_limit(v.limit);
    Ok(())
}

/// Decode a [`KvListOptions`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_kv_list_options(
    r: plugin_capnp::kv_list_options::Reader<'_>,
) -> capnp::Result<KvListOptions> {
    Ok(KvListOptions {
        prefix: r.get_prefix()?.to_string()?,
        cursor: r.get_cursor()?.to_string()?,
        limit: r.get_limit(),
    })
}

/// Encode a [`KvListKey`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_kv_list_key(
    mut b: plugin_capnp::kv_list_key::Builder<'_>,
    v: &KvListKey,
) -> capnp::Result<()> {
    b.set_key(&v.key);
    b.set_metadata(&v.metadata);
    b.set_expiration_unix_ms(v.expiration_unix_ms);
    Ok(())
}

/// Decode a [`KvListKey`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_kv_list_key(r: plugin_capnp::kv_list_key::Reader<'_>) -> capnp::Result<KvListKey> {
    Ok(KvListKey {
        key: r.get_key()?.to_string()?,
        metadata: r.get_metadata()?.to_string()?,
        expiration_unix_ms: r.get_expiration_unix_ms(),
    })
}

/// Encode a [`KvListOk`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.
pub fn write_kv_list_ok(
    mut b: plugin_capnp::kv_list_ok::Builder<'_>,
    v: &KvListOk,
) -> capnp::Result<()> {
    {
        let mut items = b.reborrow().init_keys(list_len(v.keys.len())?);
        for (i, item) in v.keys.iter().enumerate() {
            write_kv_list_key(items.reborrow().get(list_len(i)?), item)?;
        }
    }
    b.set_list_complete(v.list_complete);
    b.set_cursor(&v.cursor);
    Ok(())
}

/// Decode a [`KvListOk`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.
pub fn read_kv_list_ok(r: plugin_capnp::kv_list_ok::Reader<'_>) -> capnp::Result<KvListOk> {
    Ok(KvListOk {
        keys: r
            .get_keys()?
            .iter()
            .map(|item| read_kv_list_key(item))
            .collect::<capnp::Result<Vec<_>>>()?,
        list_complete: r.get_list_complete(),
        cursor: r.get_cursor()?.to_string()?,
    })
}

/// Encode a `KvListReply` union from a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when a nested field cannot be encoded.
pub fn write_kv_list_reply(
    b: plugin_capnp::kv_list_reply::Builder<'_>,
    v: &Result<KvListOk, crate::PluginError>,
) -> capnp::Result<()> {
    match v {
        Ok(ok) => write_kv_list_ok(b.init_ok(), ok),
        Err(err) => {
            crate::rpc::write_error(b.init_err(), err);
            Ok(())
        }
    }
}

/// Decode a `KvListReply` union into a typed result.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the union is unset or a field is malformed.
pub fn read_kv_list_reply(
    r: plugin_capnp::kv_list_reply::Reader<'_>,
) -> capnp::Result<Result<KvListOk, crate::PluginError>> {
    match r.which()? {
        plugin_capnp::kv_list_reply::Ok(ok) => Ok(Ok(read_kv_list_ok(ok?)?)),
        plugin_capnp::kv_list_reply::Err(err) => Ok(Err(crate::rpc::read_error(err?))),
    }
}

/// Number of root structs [`read_any_message`] can decode; selectors are
/// `0..READ_ANY_COUNT` in schema order.
pub const READ_ANY_COUNT: u8 = 82;

/// Decodes `bytes` as an unpacked single-segment Cap'n Proto message whose
/// root is the struct `selector` names, through that struct's `read_*`
/// codec, and drops the result.
///
/// Harness entry point for the `abi_decode` fuzz target and its corpus
/// replay: every generated decoder runs with a traversal budget derived
/// from the input size and a fixed nesting limit, so a small buffer cannot
/// expand into a large graph.
///
/// # Errors
///
/// Returns the Cap'n Proto error the framing or the codec reports, or a
/// failure for a selector at or past [`READ_ANY_COUNT`].
pub fn read_any_message(selector: u8, bytes: &[u8]) -> capnp::Result<()> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)?;
    match selector {
        0 => {
            let _ = read_event_consumer_spec(
                reader.get_root::<plugin_capnp::event_consumer_spec::Reader<'_>>()?,
            )?;
        }
        1 => {
            let _ = read_plugin_capabilities(
                reader.get_root::<plugin_capnp::plugin_capabilities::Reader<'_>>()?,
            )?;
        }
        2 => {
            let _ = read_brand(reader.get_root::<plugin_capnp::brand::Reader<'_>>()?)?;
        }
        3 => {
            let _ =
                read_config_option(reader.get_root::<plugin_capnp::config_option::Reader<'_>>()?)?;
        }
        4 => {
            let _ = read_config_option_value(
                reader.get_root::<plugin_capnp::config_option_value::Reader<'_>>()?,
            )?;
        }
        5 => {
            let _ = read_cli_schema(reader.get_root::<plugin_capnp::cli_schema::Reader<'_>>()?)?;
        }
        6 => {
            let _ = read_cli_command_spec(
                reader.get_root::<plugin_capnp::cli_command_spec::Reader<'_>>()?,
            )?;
        }
        7 => {
            let _ =
                read_cli_arg_spec(reader.get_root::<plugin_capnp::cli_arg_spec::Reader<'_>>()?)?;
        }
        8 => {
            let _ = read_cli_arg(reader.get_root::<plugin_capnp::cli_arg::Reader<'_>>()?)?;
        }
        9 => {
            let _ = read_cli_invoke_params(
                reader.get_root::<plugin_capnp::cli_invoke_params::Reader<'_>>()?,
            )?;
        }
        10 => {
            let _ = read_cli_invoke_result(
                reader.get_root::<plugin_capnp::cli_invoke_result::Reader<'_>>()?,
            )?;
        }
        11 => {
            let _ = read_cli_schema_reply(
                reader.get_root::<plugin_capnp::cli_schema_reply::Reader<'_>>()?,
            )?;
        }
        12 => {
            let _ = read_cli_invoke_reply(
                reader.get_root::<plugin_capnp::cli_invoke_reply::Reader<'_>>()?,
            )?;
        }
        13 => {
            let _ = read_database_adapter_config(
                reader.get_root::<plugin_capnp::database_adapter_config::Reader<'_>>()?,
            )?;
        }
        14 => {
            let _ = read_diagnose_result(
                reader.get_root::<plugin_capnp::diagnose_result::Reader<'_>>()?,
            )?;
        }
        15 => {
            let _ = read_diagnose_reply(
                reader.get_root::<plugin_capnp::diagnose_reply::Reader<'_>>()?,
            )?;
        }
        16 => {
            let _ = read_source_account(
                reader.get_root::<plugin_capnp::source_account::Reader<'_>>()?,
            )?;
        }
        17 => {
            let _ =
                read_login_params(reader.get_root::<plugin_capnp::login_params::Reader<'_>>()?)?;
        }
        18 => {
            let _ =
                read_login_result(reader.get_root::<plugin_capnp::login_result::Reader<'_>>()?)?;
        }
        19 => {
            let _ = read_login_start_result(
                reader.get_root::<plugin_capnp::login_start_result::Reader<'_>>()?,
            )?;
        }
        20 => {
            let _ = read_login_complete_params(
                reader.get_root::<plugin_capnp::login_complete_params::Reader<'_>>()?,
            )?;
        }
        21 => {
            let _ = read_login_reply(reader.get_root::<plugin_capnp::login_reply::Reader<'_>>()?)?;
        }
        22 => {
            let _ = read_login_start_reply(
                reader.get_root::<plugin_capnp::login_start_reply::Reader<'_>>()?,
            )?;
        }
        23 => {
            let _ = read_account_credential(
                reader.get_root::<plugin_capnp::account_credential::Reader<'_>>()?,
            )?;
        }
        24 => {
            let _ = read_scan_params(reader.get_root::<plugin_capnp::scan_params::Reader<'_>>()?)?;
        }
        25 => {
            let _ = read_scan_book(reader.get_root::<plugin_capnp::scan_book::Reader<'_>>()?)?;
        }
        26 => {
            let _ =
                read_scan_summary(reader.get_root::<plugin_capnp::scan_summary::Reader<'_>>()?)?;
        }
        27 => {
            let _ = read_scan_reply(reader.get_root::<plugin_capnp::scan_reply::Reader<'_>>()?)?;
        }
        28 => {
            let _ =
                read_fetch_options(reader.get_root::<plugin_capnp::fetch_options::Reader<'_>>()?)?;
        }
        29 => {
            let _ = read_fetch_title_params(
                reader.get_root::<plugin_capnp::fetch_title_params::Reader<'_>>()?,
            )?;
        }
        30 => {
            let _ = read_plain_part(reader.get_root::<plugin_capnp::plain_part::Reader<'_>>()?)?;
        }
        31 => {
            let _ = read_chapter_marker(
                reader.get_root::<plugin_capnp::chapter_marker::Reader<'_>>()?,
            )?;
        }
        32 => {
            let _ = read_plain_fetch(reader.get_root::<plugin_capnp::plain_fetch::Reader<'_>>()?)?;
        }
        33 => {
            let _ = read_fetch_title_reply(
                reader.get_root::<plugin_capnp::fetch_title_reply::Reader<'_>>()?,
            )?;
        }
        34 => {
            let _ = read_source_accounts(
                reader.get_root::<plugin_capnp::source_accounts::Reader<'_>>()?,
            )?;
        }
        35 => {
            let _ = read_source_accounts_reply(
                reader.get_root::<plugin_capnp::source_accounts_reply::Reader<'_>>()?,
            )?;
        }
        36 => {
            let _ = read_search_catalog_params(
                reader.get_root::<plugin_capnp::search_catalog_params::Reader<'_>>()?,
            )?;
        }
        37 => {
            let _ = read_expand_candidates_params(
                reader.get_root::<plugin_capnp::expand_candidates_params::Reader<'_>>()?,
            )?;
        }
        38 => {
            let _ = read_purchase_hint_params(
                reader.get_root::<plugin_capnp::purchase_hint_params::Reader<'_>>()?,
            )?;
        }
        39 => {
            let _ = read_list_deals_params(
                reader.get_root::<plugin_capnp::list_deals_params::Reader<'_>>()?,
            )?;
        }
        40 => {
            let _ = read_catalog_detail_params(
                reader.get_root::<plugin_capnp::catalog_detail_params::Reader<'_>>()?,
            )?;
        }
        41 => {
            let _ = read_catalog_hit(reader.get_root::<plugin_capnp::catalog_hit::Reader<'_>>()?)?;
        }
        42 => {
            let _ =
                read_catalog_hits(reader.get_root::<plugin_capnp::catalog_hits::Reader<'_>>()?)?;
        }
        43 => {
            let _ = read_catalog_hits_reply(
                reader.get_root::<plugin_capnp::catalog_hits_reply::Reader<'_>>()?,
            )?;
        }
        44 => {
            let _ = read_catalog_detail(
                reader.get_root::<plugin_capnp::catalog_detail::Reader<'_>>()?,
            )?;
        }
        45 => {
            let _ = read_catalog_detail_reply(
                reader.get_root::<plugin_capnp::catalog_detail_reply::Reader<'_>>()?,
            )?;
        }
        46 => {
            let _ =
                read_purchase_hint(reader.get_root::<plugin_capnp::purchase_hint::Reader<'_>>()?)?;
        }
        47 => {
            let _ = read_purchase_hint_result(
                reader.get_root::<plugin_capnp::purchase_hint_result::Reader<'_>>()?,
            )?;
        }
        48 => {
            let _ = read_purchase_hint_reply(
                reader.get_root::<plugin_capnp::purchase_hint_reply::Reader<'_>>()?,
            )?;
        }
        49 => {
            let _ = read_scan_library_params(
                reader.get_root::<plugin_capnp::scan_library_params::Reader<'_>>()?,
            )?;
        }
        50 => {
            let _ = read_authenticate_user_params(
                reader.get_root::<plugin_capnp::authenticate_user_params::Reader<'_>>()?,
            )?;
        }
        51 => {
            let _ =
                read_external_user(reader.get_root::<plugin_capnp::external_user::Reader<'_>>()?)?;
        }
        52 => {
            let _ = read_external_user_reply(
                reader.get_root::<plugin_capnp::external_user_reply::Reader<'_>>()?,
            )?;
        }
        53 => {
            let _ = read_event_poll_result(
                reader.get_root::<plugin_capnp::event_poll_result::Reader<'_>>()?,
            )?;
        }
        54 => {
            let _ = read_event_poll_reply(
                reader.get_root::<plugin_capnp::event_poll_reply::Reader<'_>>()?,
            )?;
        }
        55 => {
            let _ = read_listening_progress(
                reader.get_root::<plugin_capnp::listening_progress::Reader<'_>>()?,
            )?;
        }
        56 => {
            let _ = read_sync_listening_result(
                reader.get_root::<plugin_capnp::sync_listening_result::Reader<'_>>()?,
            )?;
        }
        57 => {
            let _ = read_sync_listening_reply(
                reader.get_root::<plugin_capnp::sync_listening_reply::Reader<'_>>()?,
            )?;
        }
        58 => {
            let _ =
                read_plugin_event(reader.get_root::<plugin_capnp::plugin_event::Reader<'_>>()?)?;
        }
        59 => {
            let _ = read_publish_ok(reader.get_root::<plugin_capnp::publish_ok::Reader<'_>>()?)?;
        }
        60 => {
            let _ =
                read_publish_reply(reader.get_root::<plugin_capnp::publish_reply::Reader<'_>>()?)?;
        }
        61 => {
            let _ = read_invocation(reader.get_root::<plugin_capnp::invocation::Reader<'_>>()?)?;
        }
        62 => {
            let _ =
                read_queue_message(reader.get_root::<plugin_capnp::queue_message::Reader<'_>>()?)?;
        }
        63 => {
            let _ = read_queue_batch(reader.get_root::<plugin_capnp::queue_batch::Reader<'_>>()?)?;
        }
        64 => {
            let _ = read_queue_retry(reader.get_root::<plugin_capnp::queue_retry::Reader<'_>>()?)?;
        }
        65 => {
            let _ =
                read_queue_result(reader.get_root::<plugin_capnp::queue_result::Reader<'_>>()?)?;
        }
        66 => {
            let _ = read_queue_batch_ok(
                reader.get_root::<plugin_capnp::queue_batch_ok::Reader<'_>>()?,
            )?;
        }
        67 => {
            let _ = read_queue_batch_reply(
                reader.get_root::<plugin_capnp::queue_batch_reply::Reader<'_>>()?,
            )?;
        }
        68 => {
            let _ = read_queue_send_message(
                reader.get_root::<plugin_capnp::queue_send_message::Reader<'_>>()?,
            )?;
        }
        69 => {
            let _ =
                read_queue_send_ok(reader.get_root::<plugin_capnp::queue_send_ok::Reader<'_>>()?)?;
        }
        70 => {
            let _ = read_queue_send_reply(
                reader.get_root::<plugin_capnp::queue_send_reply::Reader<'_>>()?,
            )?;
        }
        71 => {
            let _ = read_kv_put_options(
                reader.get_root::<plugin_capnp::kv_put_options::Reader<'_>>()?,
            )?;
        }
        72 => {
            let _ = read_kv_get_ok(reader.get_root::<plugin_capnp::kv_get_ok::Reader<'_>>()?)?;
        }
        73 => {
            let _ =
                read_kv_get_reply(reader.get_root::<plugin_capnp::kv_get_reply::Reader<'_>>()?)?;
        }
        74 => {
            let _ = read_kv_put_ok(reader.get_root::<plugin_capnp::kv_put_ok::Reader<'_>>()?)?;
        }
        75 => {
            let _ =
                read_kv_delete_ok(reader.get_root::<plugin_capnp::kv_delete_ok::Reader<'_>>()?)?;
        }
        76 => {
            let _ =
                read_kv_put_reply(reader.get_root::<plugin_capnp::kv_put_reply::Reader<'_>>()?)?;
        }
        77 => {
            let _ = read_kv_delete_reply(
                reader.get_root::<plugin_capnp::kv_delete_reply::Reader<'_>>()?,
            )?;
        }
        78 => {
            let _ = read_kv_list_options(
                reader.get_root::<plugin_capnp::kv_list_options::Reader<'_>>()?,
            )?;
        }
        79 => {
            let _ = read_kv_list_key(reader.get_root::<plugin_capnp::kv_list_key::Reader<'_>>()?)?;
        }
        80 => {
            let _ = read_kv_list_ok(reader.get_root::<plugin_capnp::kv_list_ok::Reader<'_>>()?)?;
        }
        81 => {
            let _ =
                read_kv_list_reply(reader.get_root::<plugin_capnp::kv_list_reply::Reader<'_>>()?)?;
        }
        _ => {
            return Err(capnp::Error::failed(format!(
                "unknown read_any selector {selector}"
            )))
        }
    }
    Ok(())
}

/// Why [`roundtrip_any_message`] could not show `read(write(v)) == v`.
#[derive(Debug)]
pub enum RoundTripFailure {
    /// The input bytes did not decode; expected for arbitrary input and
    /// not a codec defect.
    Rejected(capnp::Error),
    /// A value that did decode failed to re-encode, to decode again, or
    /// came back different: a defect in a generated codec pair.
    Codec(String),
}

impl From<capnp::Error> for RoundTripFailure {
    fn from(err: capnp::Error) -> Self {
        Self::Rejected(err)
    }
}

/// Decodes `bytes` as the struct `selector` names (as [`read_any_message`]
/// does), writes the value back through that struct's `write_*` codec,
/// decodes the result and requires it to equal the first value.
///
/// Harness entry point for the `abi_roundtrip` fuzz target and its corpus
/// replay. The first decode runs under the same traversal and nesting
/// budget as [`read_any_message`]; the re-decode runs on bytes this crate
/// produced from an already bounded value, so it uses the default budget.
/// Equality is the DTO's `PartialEq`; because `NaN != NaN`, two values
/// that `PartialEq` separates still count as equal when their `Debug`
/// renderings agree, which is a field-by-field comparison that prints every
/// NaN the same way and keeps `-0.0` apart from `0.0`.
///
/// # Errors
///
/// [`RoundTripFailure::Rejected`] when the input does not decode or the
/// selector is at or past [`READ_ANY_COUNT`]; [`RoundTripFailure::Codec`]
/// when the decoded value does not survive `write_*` then `read_*` intact.
pub fn roundtrip_any_message(selector: u8, bytes: &[u8]) -> Result<(), RoundTripFailure> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)?;
    match selector {
        0 => {
            let first = read_event_consumer_spec(
                reader.get_root::<plugin_capnp::event_consumer_spec::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_event_consumer_spec(
                message.init_root::<plugin_capnp::event_consumer_spec::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_event_consumer_spec(
                    again.get_root::<plugin_capnp::event_consumer_spec::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        1 => {
            let first = read_plugin_capabilities(
                reader.get_root::<plugin_capnp::plugin_capabilities::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_plugin_capabilities(
                message.init_root::<plugin_capnp::plugin_capabilities::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_plugin_capabilities(
                    again.get_root::<plugin_capnp::plugin_capabilities::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        2 => {
            let first = read_brand(reader.get_root::<plugin_capnp::brand::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_brand(
                message.init_root::<plugin_capnp::brand::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| read_brand(again.get_root::<plugin_capnp::brand::Reader<'_>>()?))
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        3 => {
            let first =
                read_config_option(reader.get_root::<plugin_capnp::config_option::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_config_option(
                message.init_root::<plugin_capnp::config_option::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_config_option(again.get_root::<plugin_capnp::config_option::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        4 => {
            let first = read_config_option_value(
                reader.get_root::<plugin_capnp::config_option_value::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_config_option_value(
                message.init_root::<plugin_capnp::config_option_value::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_config_option_value(
                    again.get_root::<plugin_capnp::config_option_value::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        5 => {
            let first =
                read_cli_schema(reader.get_root::<plugin_capnp::cli_schema::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_cli_schema(
                message.init_root::<plugin_capnp::cli_schema::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_cli_schema(again.get_root::<plugin_capnp::cli_schema::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        6 => {
            let first = read_cli_command_spec(
                reader.get_root::<plugin_capnp::cli_command_spec::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_cli_command_spec(
                message.init_root::<plugin_capnp::cli_command_spec::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_cli_command_spec(
                    again.get_root::<plugin_capnp::cli_command_spec::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        7 => {
            let first =
                read_cli_arg_spec(reader.get_root::<plugin_capnp::cli_arg_spec::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_cli_arg_spec(
                message.init_root::<plugin_capnp::cli_arg_spec::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_cli_arg_spec(again.get_root::<plugin_capnp::cli_arg_spec::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        8 => {
            let first = read_cli_arg(reader.get_root::<plugin_capnp::cli_arg::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_cli_arg(
                message.init_root::<plugin_capnp::cli_arg::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| read_cli_arg(again.get_root::<plugin_capnp::cli_arg::Reader<'_>>()?))
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        9 => {
            let first = read_cli_invoke_params(
                reader.get_root::<plugin_capnp::cli_invoke_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_cli_invoke_params(
                message.init_root::<plugin_capnp::cli_invoke_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_cli_invoke_params(
                    again.get_root::<plugin_capnp::cli_invoke_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        10 => {
            let first = read_cli_invoke_result(
                reader.get_root::<plugin_capnp::cli_invoke_result::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_cli_invoke_result(
                message.init_root::<plugin_capnp::cli_invoke_result::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_cli_invoke_result(
                    again.get_root::<plugin_capnp::cli_invoke_result::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        11 => {
            let first = read_cli_schema_reply(
                reader.get_root::<plugin_capnp::cli_schema_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_cli_schema_reply(
                message.init_root::<plugin_capnp::cli_schema_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_cli_schema_reply(
                    again.get_root::<plugin_capnp::cli_schema_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        12 => {
            let first = read_cli_invoke_reply(
                reader.get_root::<plugin_capnp::cli_invoke_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_cli_invoke_reply(
                message.init_root::<plugin_capnp::cli_invoke_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_cli_invoke_reply(
                    again.get_root::<plugin_capnp::cli_invoke_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        13 => {
            let first = read_database_adapter_config(
                reader.get_root::<plugin_capnp::database_adapter_config::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_database_adapter_config(
                message.init_root::<plugin_capnp::database_adapter_config::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_database_adapter_config(
                    again.get_root::<plugin_capnp::database_adapter_config::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        14 => {
            let first = read_diagnose_result(
                reader.get_root::<plugin_capnp::diagnose_result::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_diagnose_result(
                message.init_root::<plugin_capnp::diagnose_result::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_diagnose_result(again.get_root::<plugin_capnp::diagnose_result::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        15 => {
            let first = read_diagnose_reply(
                reader.get_root::<plugin_capnp::diagnose_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_diagnose_reply(
                message.init_root::<plugin_capnp::diagnose_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_diagnose_reply(again.get_root::<plugin_capnp::diagnose_reply::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        16 => {
            let first = read_source_account(
                reader.get_root::<plugin_capnp::source_account::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_source_account(
                message.init_root::<plugin_capnp::source_account::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_source_account(again.get_root::<plugin_capnp::source_account::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        17 => {
            let first =
                read_login_params(reader.get_root::<plugin_capnp::login_params::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_login_params(
                message.init_root::<plugin_capnp::login_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_login_params(again.get_root::<plugin_capnp::login_params::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        18 => {
            let first =
                read_login_result(reader.get_root::<plugin_capnp::login_result::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_login_result(
                message.init_root::<plugin_capnp::login_result::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_login_result(again.get_root::<plugin_capnp::login_result::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        19 => {
            let first = read_login_start_result(
                reader.get_root::<plugin_capnp::login_start_result::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_login_start_result(
                message.init_root::<plugin_capnp::login_start_result::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_login_start_result(
                    again.get_root::<plugin_capnp::login_start_result::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        20 => {
            let first = read_login_complete_params(
                reader.get_root::<plugin_capnp::login_complete_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_login_complete_params(
                message.init_root::<plugin_capnp::login_complete_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_login_complete_params(
                    again.get_root::<plugin_capnp::login_complete_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        21 => {
            let first =
                read_login_reply(reader.get_root::<plugin_capnp::login_reply::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_login_reply(
                message.init_root::<plugin_capnp::login_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_login_reply(again.get_root::<plugin_capnp::login_reply::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        22 => {
            let first = read_login_start_reply(
                reader.get_root::<plugin_capnp::login_start_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_login_start_reply(
                message.init_root::<plugin_capnp::login_start_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_login_start_reply(
                    again.get_root::<plugin_capnp::login_start_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        23 => {
            let first = read_account_credential(
                reader.get_root::<plugin_capnp::account_credential::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_account_credential(
                message.init_root::<plugin_capnp::account_credential::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_account_credential(
                    again.get_root::<plugin_capnp::account_credential::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        24 => {
            let first =
                read_scan_params(reader.get_root::<plugin_capnp::scan_params::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_scan_params(
                message.init_root::<plugin_capnp::scan_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_scan_params(again.get_root::<plugin_capnp::scan_params::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        25 => {
            let first = read_scan_book(reader.get_root::<plugin_capnp::scan_book::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_scan_book(
                message.init_root::<plugin_capnp::scan_book::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_scan_book(again.get_root::<plugin_capnp::scan_book::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        26 => {
            let first =
                read_scan_summary(reader.get_root::<plugin_capnp::scan_summary::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_scan_summary(
                message.init_root::<plugin_capnp::scan_summary::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_scan_summary(again.get_root::<plugin_capnp::scan_summary::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        27 => {
            let first =
                read_scan_reply(reader.get_root::<plugin_capnp::scan_reply::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_scan_reply(
                message.init_root::<plugin_capnp::scan_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_scan_reply(again.get_root::<plugin_capnp::scan_reply::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        28 => {
            let first =
                read_fetch_options(reader.get_root::<plugin_capnp::fetch_options::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_fetch_options(
                message.init_root::<plugin_capnp::fetch_options::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_fetch_options(again.get_root::<plugin_capnp::fetch_options::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        29 => {
            let first = read_fetch_title_params(
                reader.get_root::<plugin_capnp::fetch_title_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_fetch_title_params(
                message.init_root::<plugin_capnp::fetch_title_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_fetch_title_params(
                    again.get_root::<plugin_capnp::fetch_title_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        30 => {
            let first =
                read_plain_part(reader.get_root::<plugin_capnp::plain_part::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_plain_part(
                message.init_root::<plugin_capnp::plain_part::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_plain_part(again.get_root::<plugin_capnp::plain_part::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        31 => {
            let first = read_chapter_marker(
                reader.get_root::<plugin_capnp::chapter_marker::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_chapter_marker(
                message.init_root::<plugin_capnp::chapter_marker::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_chapter_marker(again.get_root::<plugin_capnp::chapter_marker::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        32 => {
            let first =
                read_plain_fetch(reader.get_root::<plugin_capnp::plain_fetch::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_plain_fetch(
                message.init_root::<plugin_capnp::plain_fetch::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_plain_fetch(again.get_root::<plugin_capnp::plain_fetch::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        33 => {
            let first = read_fetch_title_reply(
                reader.get_root::<plugin_capnp::fetch_title_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_fetch_title_reply(
                message.init_root::<plugin_capnp::fetch_title_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_fetch_title_reply(
                    again.get_root::<plugin_capnp::fetch_title_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        34 => {
            let first = read_source_accounts(
                reader.get_root::<plugin_capnp::source_accounts::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_source_accounts(
                message.init_root::<plugin_capnp::source_accounts::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_source_accounts(again.get_root::<plugin_capnp::source_accounts::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        35 => {
            let first = read_source_accounts_reply(
                reader.get_root::<plugin_capnp::source_accounts_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_source_accounts_reply(
                message.init_root::<plugin_capnp::source_accounts_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_source_accounts_reply(
                    again.get_root::<plugin_capnp::source_accounts_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        36 => {
            let first = read_search_catalog_params(
                reader.get_root::<plugin_capnp::search_catalog_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_search_catalog_params(
                message.init_root::<plugin_capnp::search_catalog_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_search_catalog_params(
                    again.get_root::<plugin_capnp::search_catalog_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        37 => {
            let first = read_expand_candidates_params(
                reader.get_root::<plugin_capnp::expand_candidates_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_expand_candidates_params(
                message.init_root::<plugin_capnp::expand_candidates_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_expand_candidates_params(
                    again.get_root::<plugin_capnp::expand_candidates_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        38 => {
            let first = read_purchase_hint_params(
                reader.get_root::<plugin_capnp::purchase_hint_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_purchase_hint_params(
                message.init_root::<plugin_capnp::purchase_hint_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_purchase_hint_params(
                    again.get_root::<plugin_capnp::purchase_hint_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        39 => {
            let first = read_list_deals_params(
                reader.get_root::<plugin_capnp::list_deals_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_list_deals_params(
                message.init_root::<plugin_capnp::list_deals_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_list_deals_params(
                    again.get_root::<plugin_capnp::list_deals_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        40 => {
            let first = read_catalog_detail_params(
                reader.get_root::<plugin_capnp::catalog_detail_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_catalog_detail_params(
                message.init_root::<plugin_capnp::catalog_detail_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_catalog_detail_params(
                    again.get_root::<plugin_capnp::catalog_detail_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        41 => {
            let first =
                read_catalog_hit(reader.get_root::<plugin_capnp::catalog_hit::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_catalog_hit(
                message.init_root::<plugin_capnp::catalog_hit::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_catalog_hit(again.get_root::<plugin_capnp::catalog_hit::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        42 => {
            let first =
                read_catalog_hits(reader.get_root::<plugin_capnp::catalog_hits::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_catalog_hits(
                message.init_root::<plugin_capnp::catalog_hits::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_catalog_hits(again.get_root::<plugin_capnp::catalog_hits::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        43 => {
            let first = read_catalog_hits_reply(
                reader.get_root::<plugin_capnp::catalog_hits_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_catalog_hits_reply(
                message.init_root::<plugin_capnp::catalog_hits_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_catalog_hits_reply(
                    again.get_root::<plugin_capnp::catalog_hits_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        44 => {
            let first = read_catalog_detail(
                reader.get_root::<plugin_capnp::catalog_detail::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_catalog_detail(
                message.init_root::<plugin_capnp::catalog_detail::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_catalog_detail(again.get_root::<plugin_capnp::catalog_detail::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        45 => {
            let first = read_catalog_detail_reply(
                reader.get_root::<plugin_capnp::catalog_detail_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_catalog_detail_reply(
                message.init_root::<plugin_capnp::catalog_detail_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_catalog_detail_reply(
                    again.get_root::<plugin_capnp::catalog_detail_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        46 => {
            let first =
                read_purchase_hint(reader.get_root::<plugin_capnp::purchase_hint::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_purchase_hint(
                message.init_root::<plugin_capnp::purchase_hint::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_purchase_hint(again.get_root::<plugin_capnp::purchase_hint::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        47 => {
            let first = read_purchase_hint_result(
                reader.get_root::<plugin_capnp::purchase_hint_result::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_purchase_hint_result(
                message.init_root::<plugin_capnp::purchase_hint_result::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_purchase_hint_result(
                    again.get_root::<plugin_capnp::purchase_hint_result::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        48 => {
            let first = read_purchase_hint_reply(
                reader.get_root::<plugin_capnp::purchase_hint_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_purchase_hint_reply(
                message.init_root::<plugin_capnp::purchase_hint_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_purchase_hint_reply(
                    again.get_root::<plugin_capnp::purchase_hint_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        49 => {
            let first = read_scan_library_params(
                reader.get_root::<plugin_capnp::scan_library_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_scan_library_params(
                message.init_root::<plugin_capnp::scan_library_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_scan_library_params(
                    again.get_root::<plugin_capnp::scan_library_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        50 => {
            let first = read_authenticate_user_params(
                reader.get_root::<plugin_capnp::authenticate_user_params::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_authenticate_user_params(
                message.init_root::<plugin_capnp::authenticate_user_params::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_authenticate_user_params(
                    again.get_root::<plugin_capnp::authenticate_user_params::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        51 => {
            let first =
                read_external_user(reader.get_root::<plugin_capnp::external_user::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_external_user(
                message.init_root::<plugin_capnp::external_user::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_external_user(again.get_root::<plugin_capnp::external_user::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        52 => {
            let first = read_external_user_reply(
                reader.get_root::<plugin_capnp::external_user_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_external_user_reply(
                message.init_root::<plugin_capnp::external_user_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_external_user_reply(
                    again.get_root::<plugin_capnp::external_user_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        53 => {
            let first = read_event_poll_result(
                reader.get_root::<plugin_capnp::event_poll_result::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_event_poll_result(
                message.init_root::<plugin_capnp::event_poll_result::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_event_poll_result(
                    again.get_root::<plugin_capnp::event_poll_result::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        54 => {
            let first = read_event_poll_reply(
                reader.get_root::<plugin_capnp::event_poll_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_event_poll_reply(
                message.init_root::<plugin_capnp::event_poll_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_event_poll_reply(
                    again.get_root::<plugin_capnp::event_poll_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        55 => {
            let first = read_listening_progress(
                reader.get_root::<plugin_capnp::listening_progress::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_listening_progress(
                message.init_root::<plugin_capnp::listening_progress::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_listening_progress(
                    again.get_root::<plugin_capnp::listening_progress::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        56 => {
            let first = read_sync_listening_result(
                reader.get_root::<plugin_capnp::sync_listening_result::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_sync_listening_result(
                message.init_root::<plugin_capnp::sync_listening_result::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_sync_listening_result(
                    again.get_root::<plugin_capnp::sync_listening_result::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        57 => {
            let first = read_sync_listening_reply(
                reader.get_root::<plugin_capnp::sync_listening_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_sync_listening_reply(
                message.init_root::<plugin_capnp::sync_listening_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_sync_listening_reply(
                    again.get_root::<plugin_capnp::sync_listening_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        58 => {
            let first =
                read_plugin_event(reader.get_root::<plugin_capnp::plugin_event::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_plugin_event(
                message.init_root::<plugin_capnp::plugin_event::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_plugin_event(again.get_root::<plugin_capnp::plugin_event::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        59 => {
            let first =
                read_publish_ok(reader.get_root::<plugin_capnp::publish_ok::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_publish_ok(
                message.init_root::<plugin_capnp::publish_ok::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_publish_ok(again.get_root::<plugin_capnp::publish_ok::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        60 => {
            let first =
                read_publish_reply(reader.get_root::<plugin_capnp::publish_reply::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_publish_reply(
                message.init_root::<plugin_capnp::publish_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_publish_reply(again.get_root::<plugin_capnp::publish_reply::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        61 => {
            let first =
                read_invocation(reader.get_root::<plugin_capnp::invocation::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_invocation(
                message.init_root::<plugin_capnp::invocation::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_invocation(again.get_root::<plugin_capnp::invocation::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        62 => {
            let first =
                read_queue_message(reader.get_root::<plugin_capnp::queue_message::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_message(
                message.init_root::<plugin_capnp::queue_message::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_message(again.get_root::<plugin_capnp::queue_message::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        63 => {
            let first =
                read_queue_batch(reader.get_root::<plugin_capnp::queue_batch::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_batch(
                message.init_root::<plugin_capnp::queue_batch::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_batch(again.get_root::<plugin_capnp::queue_batch::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        64 => {
            let first =
                read_queue_retry(reader.get_root::<plugin_capnp::queue_retry::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_retry(
                message.init_root::<plugin_capnp::queue_retry::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_retry(again.get_root::<plugin_capnp::queue_retry::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        65 => {
            let first =
                read_queue_result(reader.get_root::<plugin_capnp::queue_result::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_result(
                message.init_root::<plugin_capnp::queue_result::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_result(again.get_root::<plugin_capnp::queue_result::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        66 => {
            let first = read_queue_batch_ok(
                reader.get_root::<plugin_capnp::queue_batch_ok::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_batch_ok(
                message.init_root::<plugin_capnp::queue_batch_ok::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_batch_ok(again.get_root::<plugin_capnp::queue_batch_ok::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        67 => {
            let first = read_queue_batch_reply(
                reader.get_root::<plugin_capnp::queue_batch_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_batch_reply(
                message.init_root::<plugin_capnp::queue_batch_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_batch_reply(
                    again.get_root::<plugin_capnp::queue_batch_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        68 => {
            let first = read_queue_send_message(
                reader.get_root::<plugin_capnp::queue_send_message::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_send_message(
                message.init_root::<plugin_capnp::queue_send_message::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_send_message(
                    again.get_root::<plugin_capnp::queue_send_message::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        69 => {
            let first =
                read_queue_send_ok(reader.get_root::<plugin_capnp::queue_send_ok::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_send_ok(
                message.init_root::<plugin_capnp::queue_send_ok::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_send_ok(again.get_root::<plugin_capnp::queue_send_ok::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        70 => {
            let first = read_queue_send_reply(
                reader.get_root::<plugin_capnp::queue_send_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_queue_send_reply(
                message.init_root::<plugin_capnp::queue_send_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_queue_send_reply(
                    again.get_root::<plugin_capnp::queue_send_reply::Reader<'_>>()?,
                )
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        71 => {
            let first = read_kv_put_options(
                reader.get_root::<plugin_capnp::kv_put_options::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_put_options(
                message.init_root::<plugin_capnp::kv_put_options::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_put_options(again.get_root::<plugin_capnp::kv_put_options::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        72 => {
            let first = read_kv_get_ok(reader.get_root::<plugin_capnp::kv_get_ok::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_get_ok(
                message.init_root::<plugin_capnp::kv_get_ok::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_get_ok(again.get_root::<plugin_capnp::kv_get_ok::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        73 => {
            let first =
                read_kv_get_reply(reader.get_root::<plugin_capnp::kv_get_reply::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_get_reply(
                message.init_root::<plugin_capnp::kv_get_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_get_reply(again.get_root::<plugin_capnp::kv_get_reply::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        74 => {
            let first = read_kv_put_ok(reader.get_root::<plugin_capnp::kv_put_ok::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_put_ok(
                message.init_root::<plugin_capnp::kv_put_ok::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_put_ok(again.get_root::<plugin_capnp::kv_put_ok::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        75 => {
            let first =
                read_kv_delete_ok(reader.get_root::<plugin_capnp::kv_delete_ok::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_delete_ok(
                message.init_root::<plugin_capnp::kv_delete_ok::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_delete_ok(again.get_root::<plugin_capnp::kv_delete_ok::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        76 => {
            let first =
                read_kv_put_reply(reader.get_root::<plugin_capnp::kv_put_reply::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_put_reply(
                message.init_root::<plugin_capnp::kv_put_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_put_reply(again.get_root::<plugin_capnp::kv_put_reply::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        77 => {
            let first = read_kv_delete_reply(
                reader.get_root::<plugin_capnp::kv_delete_reply::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_delete_reply(
                message.init_root::<plugin_capnp::kv_delete_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_delete_reply(again.get_root::<plugin_capnp::kv_delete_reply::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        78 => {
            let first = read_kv_list_options(
                reader.get_root::<plugin_capnp::kv_list_options::Reader<'_>>()?,
            )?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_list_options(
                message.init_root::<plugin_capnp::kv_list_options::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_list_options(again.get_root::<plugin_capnp::kv_list_options::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        79 => {
            let first =
                read_kv_list_key(reader.get_root::<plugin_capnp::kv_list_key::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_list_key(
                message.init_root::<plugin_capnp::kv_list_key::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_list_key(again.get_root::<plugin_capnp::kv_list_key::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        80 => {
            let first =
                read_kv_list_ok(reader.get_root::<plugin_capnp::kv_list_ok::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_list_ok(
                message.init_root::<plugin_capnp::kv_list_ok::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_list_ok(again.get_root::<plugin_capnp::kv_list_ok::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        81 => {
            let first =
                read_kv_list_reply(reader.get_root::<plugin_capnp::kv_list_reply::Reader<'_>>()?)?;
            let mut message = capnp::message::Builder::new_default();
            write_kv_list_reply(
                message.init_root::<plugin_capnp::kv_list_reply::Builder<'_>>(),
                &first,
            )
            .map_err(|err| RoundTripFailure::Codec(format!("re-encode failed: {err}")))?;
            let encoded = capnp::serialize::write_message_to_words(&message);
            let again = capnp::serialize::read_message(
                &mut std::io::Cursor::new(&encoded),
                capnp::message::ReaderOptions::new(),
            )
            .and_then(|again| {
                read_kv_list_reply(again.get_root::<plugin_capnp::kv_list_reply::Reader<'_>>()?)
            })
            .map_err(|err| RoundTripFailure::Codec(format!("re-decode failed: {err}")))?;
            if first != again && format!("{first:?}") != format!("{again:?}") {
                return Err(RoundTripFailure::Codec(format!(
                    "round trip changed the value: {first:?} != {again:?}"
                )));
            }
        }
        _ => {
            return Err(
                capnp::Error::failed(format!("unknown round-trip selector {selector}")).into(),
            )
        }
    }
    Ok(())
}
