//! `plugin.toml` types: manifest root and nested capability / workerd tables.
//!
//! These structs are the serde projection of the install descriptor. Unknown
//! keys are rejected (`deny_unknown_fields`) so typos fail at parse time.
//! Semantic rules that cannot be expressed in serde alone live in
//! [`PluginManifest::validate`]. Product narrative: `docs/plugins.md`.

use std::path::PathBuf;

use bookclerk_plugin_abi::CliSchema;
pub use bookclerk_plugin_abi::{Entrypoint, EventConsumerSpec, PluginCapabilities};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Every entrypoint in declaration order (mirrors the Cap'n Proto enum).
pub const ALL_ENTRYPOINTS: [Entrypoint; 6] = [
    Entrypoint::Storefront,
    Entrypoint::Storage,
    Entrypoint::DatabaseAdapter,
    Entrypoint::RemoteLibrary,
    Entrypoint::Cli,
    Entrypoint::Oidc,
];

/// Handler family an [`Entrypoint`] belongs to.
///
/// # Returns
///
/// The [`PluginFamily`] whose `config.toml` prefix and API group hold plugins
/// exporting this entrypoint.
#[must_use]
pub fn entrypoint_family(entrypoint: Entrypoint) -> PluginFamily {
    match entrypoint {
        Entrypoint::Storefront => PluginFamily::Source,
        Entrypoint::Storage => PluginFamily::Output,
        Entrypoint::DatabaseAdapter => PluginFamily::Database,
        Entrypoint::RemoteLibrary | Entrypoint::Cli | Entrypoint::Oidc => PluginFamily::Integration,
    }
}

/// Handler family — how the daemon, CLI, and UI group plugins and which
/// `config.toml` prefix holds their settings.
///
/// Derived from the exported [`Entrypoint`]s and declared triggers (see
/// [`PluginManifest::families`]); never written to `plugin.toml`. Wire values
/// are lowercase (`source`, `integration`, `output`, `database`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginFamily {
    /// Storefront / library source (`Storefront` entrypoint).
    Source,
    /// Event consumer, job runner, remote library, CLI, or OIDC bridge.
    Integration,
    /// Destination / output backend (`Storage` entrypoint).
    Output,
    /// Library database backend (`DatabaseAdapter` entrypoint).
    Database,
}

impl PluginFamily {
    /// Every family in settings-prefix priority order.
    pub const ALL: [Self; 4] = [
        Self::Database,
        Self::Source,
        Self::Output,
        Self::Integration,
    ];

    /// Returns the lowercase wire name used in API paths and config prefixes.
    ///
    /// # Returns
    ///
    /// One of `"source"`, `"integration"`, `"output"`, or `"database"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Integration => "integration",
            Self::Output => "output",
            Self::Database => "database",
        }
    }

    /// Parses a wire name produced by [`Self::as_str`].
    ///
    /// # Returns
    ///
    /// `Some(family)` for a known name; `None` otherwise.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == name)
    }
}

/// Guest runtime selection (`runtime` key in `plugin.toml`).
///
/// Defaults to [`PluginRuntimeKind::Native`] when omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PluginRuntimeKind {
    /// OS binary speaking the Workers RPC ABI, spawned through `bookclerk-jail`.
    ///
    /// Requires a non-empty `command`. Must not declare
    /// `capabilities.network.domains` (native outbound is coarse jail
    /// networking with no hostname filter).
    #[default]
    Native,
    /// Author modules loaded by first-party `bookclerk-workerd` (one jail +
    /// one isolate per plugin).
    ///
    /// Requires a `[workerd]` table. Outbound mode requires non-empty
    /// `capabilities.network.domains`.
    Workerd,
}

/// Network mode declared in `[capabilities.network]`.
///
/// Defaults to [`NetworkMode::Deny`]. See `docs/plugins.md` for native vs
/// workerd semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// No outbound network (default). Jail and isolate both refuse guest
    /// fetches / coarse outbound.
    #[default]
    Deny,
    /// Coarse jail outbound (native) or workerd isolate egress.
    ///
    /// For **workerd**, pair with `domains` (isolate hostname allowlist;
    /// required and validated). For **native**, do **not** set `domains` —
    /// the OS jail cannot filter by hostname; outbound means open internet
    /// (plus oauth listen when `bindings.oauth` is set).
    Outbound,
}

/// `[capabilities.network]` — mode plus optional workerd host allowlist.
///
/// Defaults to deny with an empty domain list. Unknown keys are rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkCapabilities {
    /// Whether the guest may open outbound connections / isolate fetches.
    pub mode: NetworkMode,
    /// Workerd-only: initial-request host allowlist for isolate egress.
    ///
    /// Entries are exact hosts or `*.` prefix wildcards; validated with IDNA
    /// ToASCII at [`PluginManifest::validate`]. Must be empty for
    /// `runtime = "native"` and non-empty for workerd + outbound.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
}

impl Default for NetworkCapabilities {
    fn default() -> Self {
        Self {
            mode: NetworkMode::Deny,
            domains: vec![],
        }
    }
}

/// Maximum named database bindings one plugin may declare.
pub const MAX_DATABASE_BINDINGS: usize = 8;

/// Maximum length of one database binding name.
pub const MAX_DATABASE_BINDING_NAME_LEN: usize = 32;

/// True when `name` is a valid Workers-style binding name (`[A-Z][A-Z0-9_]*`).
#[must_use]
pub fn is_valid_database_binding_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_DATABASE_BINDING_NAME_LEN
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_uppercase() || (i > 0 && c.is_ascii_digit()))
        && name.starts_with(|c: char| c.is_ascii_uppercase())
}

/// Host bindings a plugin declared, resolved from the v3 manifest tables.
///
/// Derived view produced by [`PluginManifest::bindings`]; `[vars]`,
/// `[secrets]`, `[[kv_namespaces]]`, `[work_fs]`, `[oauth]`, and
/// `[[databases]]` each map onto one flag or list here so host code can gate
/// jail layout, consent, and env injection uniformly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingCapabilities {
    /// Guest may read plugin config delivered by the host (`[vars]`).
    pub config: bool,
    /// Guest may read sealed secrets / credentials via host bindings (`[secrets]`).
    pub secrets: bool,
    /// Guest may use per-plugin key/value storage (`[[kv_namespaces]]`).
    pub plugin_kv: bool,
    /// Guest may use host-mediated work filesystem (`[work_fs]`).
    pub work_fs: bool,
    /// Guest needs an OAuth-style callback tunnel (`[oauth]`).
    ///
    /// With native outbound, this upgrades jail network need to
    /// [`JailNetworkNeed::Listen`].
    pub oauth: bool,
    /// Named plugin-owned database bindings (`[[databases]] binding = "DB"`).
    pub databases: Vec<String>,
}

/// One `[[databases]]` row — an isolated plugin-owned database binding.
///
/// Each name binds a database provisioned by the active database adapter —
/// separate from the Bookclerk library and from every other plugin. Names
/// must be `[A-Z][A-Z0-9_]*`, unique, and at most
/// [`MAX_DATABASE_BINDING_NAME_LEN`] chars; the operator consents to each
/// binding before enable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DatabaseBindingManifest {
    /// Binding name exposed on `env` (Workers-style, e.g. `"DB"`).
    pub binding: String,
}

/// A named host binding (`[secrets]`, `[work_fs]`, `[oauth]`, `[[kv_namespaces]]`).
///
/// The `binding` key names the property on `env`; it defaults per table
/// (`SECRETS`, `WORK_FS`, `OAUTH`, `KV`) when omitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct NamedBinding {
    /// Binding name exposed on `env`; empty means the table default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub binding: String,
}

impl NamedBinding {
    /// Resolves the binding name, falling back to `default`.
    ///
    /// # Returns
    ///
    /// The author's `binding` when non-empty, else `default`.
    #[must_use]
    pub fn name_or<'a>(&'a self, default: &'a str) -> &'a str {
        if self.binding.is_empty() {
            default
        } else {
            &self.binding
        }
    }
}

/// Default `binding` for `[secrets]`.
pub const DEFAULT_SECRETS_BINDING: &str = "SECRETS";
/// Default `binding` for `[work_fs]`.
pub const DEFAULT_WORK_FS_BINDING: &str = "WORK_FS";
/// Default `binding` for `[oauth]`.
pub const DEFAULT_OAUTH_BINDING: &str = "OAUTH";
/// Default `binding` for `[[kv_namespaces]]`.
pub const DEFAULT_KV_BINDING: &str = "KV";
/// Default `binding` for `[[events.producers]]`.
pub const DEFAULT_EVENTS_BINDING: &str = "EVENTS";
/// Binding name the host uses for operator config (`[vars]`).
pub const CONFIG_BINDING: &str = "CONFIG";

/// `[triggers]` — handlers on the default entrypoint the host invokes.
///
/// Mirrors `wrangler.jsonc` `triggers`; Bookclerk's triggers are durable
/// commands (`job(controller)`) rather than crons. Event triggers live under
/// `[[events.consumers]]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TriggersManifest {
    /// Command types the default entrypoint's `job(controller)` handles
    /// (snake_case, e.g. `stream_copy`). Empty means no job trigger.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
}

impl TriggersManifest {
    /// True when no trigger is declared (omit `[triggers]`).
    fn is_default(&self) -> bool {
        self.jobs.is_empty()
    }
}

/// Full `[capabilities]` table required on every manifest.
///
/// Only `network` remains here in v3 (consent has no Wrangler analogue);
/// bindings and triggers moved to top-level Wrangler-shaped tables.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesManifest {
    /// Network mode and optional workerd domain allowlist.
    pub network: NetworkCapabilities,
}

/// One `[[events.consumers]]` row — a durable domain-event trigger for the
/// default entrypoint's `event(batch)` handler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventConsumer {
    /// Versioned event type (`book_acquired`, …).
    #[serde(rename = "type")]
    pub event_type: String,
    /// Schema versions this guest can consume (default `[1]`).
    #[serde(default = "default_schema_versions")]
    pub schema_versions: Vec<u32>,
    /// Whether `EventResult::Suspended` is supported for this type.
    #[serde(default)]
    pub supports_suspend: bool,
    /// Concurrency class copied onto deliveries (default `"network"`).
    #[serde(default = "default_resource_class")]
    pub resource_class: String,
    /// Optional host-owned payload object filter (top-level key equality).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<serde_json::Value>,
    /// Redelivery attempts before dead-lettering; `None` uses the host default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
}

impl Eq for EventConsumer {}

/// One `[[events.producers]]` row — an event type the plugin may publish
/// through its `EVENTS` binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EventProducer {
    /// Event type the plugin publishes (snake_case).
    #[serde(rename = "type")]
    pub event_type: String,
    /// Publisher binding name on `env` (default `EVENTS`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub binding: String,
}

impl EventProducer {
    /// Resolves the publisher binding name (default [`DEFAULT_EVENTS_BINDING`]).
    #[must_use]
    pub fn binding_name(&self) -> &str {
        if self.binding.is_empty() {
            DEFAULT_EVENTS_BINDING
        } else {
            &self.binding
        }
    }
}

/// Default `[1]` when a consumer omits `schema_versions`.
fn default_schema_versions() -> Vec<u32> {
    vec![1]
}

/// Default `"network"` when a consumer omits `resource_class`.
fn default_resource_class() -> String {
    "network".into()
}

/// `[events]` — consumers (triggers) and producers (publish grants).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct EventsManifest {
    /// Event types delivered to the default entrypoint's `event(batch)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<EventConsumer>,
    /// Event types the plugin may publish.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub producers: Vec<EventProducer>,
}

impl EventsManifest {
    /// True when neither consumers nor producers are declared (omit `[events]`).
    fn is_default(&self) -> bool {
        self.consumers.is_empty() && self.producers.is_empty()
    }
}

/// Serde skip predicate for the optional `[vars]` table.
fn vars_is_none(v: &Option<std::collections::BTreeMap<String, toml::Value>>) -> bool {
    v.is_none()
}

/// `[workerd]` — WorkerCode-equivalent isolate configuration.
///
/// Required when [`PluginRuntimeKind::Workerd`]. Maps closely to Cloudflare
/// Worker module config consumed by `bookclerk-workerd`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerdRuntimeManifest {
    /// Cloudflare compatibility date (`YYYY-MM-DD`); required and non-empty.
    pub compatibility_date: String,
    /// Compatibility flags (e.g. `python_workers` for Pyodide guests).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compatibility_flags: Vec<String>,
    /// Entrypoint module filename relative to the modules tree (e.g. `index.js`).
    pub main_module: String,
    /// Directory containing worker modules (default `"modules"`).
    #[serde(default = "default_modules_dir")]
    pub modules_dir: String,
    /// Named export used as the Worker entrypoint (default `"default"`).
    #[serde(default = "default_entrypoint")]
    pub entrypoint: String,
    /// Optional CPU / subrequest budgets (host-clamped; see [`WorkerdLimits`]).
    #[serde(default, skip_serializing_if = "WorkerdLimits::is_default")]
    pub limits: WorkerdLimits,
}

/// Default worker modules directory (`modules`) when `[workerd]` omits it.
fn default_modules_dir() -> String {
    "modules".into()
}

/// Default named export (`default`) used as the Worker entrypoint.
fn default_entrypoint() -> String {
    "default".into()
}

/// Optional workerd resource limits under `[workerd.limits]`.
///
/// Local workerd does **not** Cap'n Proto-emit `cpuMs` / `subRequests`. Bookclerk
/// clamps these values, injects `subrequests` into egress policy JSON via
/// [`crate::EgressPolicy::from_manifest`], and maps `cpu_ms` onto jail Spec CPU
/// rate (plus memory/process ceilings) for OS enforcement.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerdLimits {
    /// Soft CPU budget in milliseconds (unset/`0` → [`Self::DEFAULT_CPU_MS`],
    /// then clamped to [`Self::MAX_CPU_MS`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u32>,
    /// Soft outbound fetch budget (unset/`0` → [`Self::DEFAULT_SUBREQUESTS`],
    /// then clamped to [`Self::MAX_SUBREQUESTS`]). Injected into
    /// `EGRESS_POLICY.subrequests` for workerd guests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subrequests: Option<u32>,
}

/// Concrete limits after applying host defaults and hard caps.
///
/// Produced by [`WorkerdLimits::effective`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveWorkerdLimits {
    /// Effective CPU budget in milliseconds.
    pub cpu_ms: u32,
    /// Effective outbound fetch / subrequest budget.
    pub subrequests: u32,
}

impl WorkerdLimits {
    /// Default CPU budget when unset or `0` (matches Echo examples): 30_000 ms.
    pub const DEFAULT_CPU_MS: u32 = 30_000;
    /// Default outbound fetch budget when unset or `0` (matches Echo examples): 50.
    pub const DEFAULT_SUBREQUESTS: u32 = 50;
    /// Hard host cap for CPU budget (ms): 120_000.
    pub const MAX_CPU_MS: u32 = 120_000;
    /// Hard host cap for outbound fetch budget: 1_000.
    pub const MAX_SUBREQUESTS: u32 = 1_000;

    /// True when both limit fields are unset (omit `[workerd.limits]`).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Resolves concrete limits: unset/`0` → defaults, then clamp to hard caps.
    ///
    /// # Returns
    ///
    /// [`EffectiveWorkerdLimits`] with `cpu_ms` and `subrequests` in range
    /// `[default, max]` (or exactly the author value when already in range).
    #[must_use]
    pub fn effective(&self) -> EffectiveWorkerdLimits {
        EffectiveWorkerdLimits {
            cpu_ms: clamp_limit(self.cpu_ms, Self::DEFAULT_CPU_MS, Self::MAX_CPU_MS),
            subrequests: clamp_limit(
                self.subrequests,
                Self::DEFAULT_SUBREQUESTS,
                Self::MAX_SUBREQUESTS,
            ),
        }
    }

    /// Alias for [`Self::effective`].
    #[must_use]
    pub fn clamp(&self) -> EffectiveWorkerdLimits {
        self.effective()
    }
}

/// Treats unset/`0` as `default`, then caps at the host hard maximum.
fn clamp_limit(raw: Option<u32>, default: u32, max: u32) -> u32 {
    match raw {
        None | Some(0) => default,
        Some(n) => n.min(max),
    }
}

/// One module entry in `[[modules]]` (workerd script packages).
///
/// Used for packaging, Python detection ([`crate::manifest_needs_python`]),
/// and documenting the isolate module graph alongside `[workerd].main_module`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleSpec {
    /// Module name as known to the isolate (often matches the filename).
    pub name: String,
    /// Path relative to the plugin package root (or modules dir).
    pub path: String,
    /// Module type string (TOML key `type`; default `"js"`). Use `"python"`
    /// for Pyodide modules.
    #[serde(default = "default_module_type")]
    #[serde(rename = "type")]
    pub module_type: String,
}

/// Default `[[modules]]` type (`js`) when the TOML `type` key is omitted.
fn default_module_type() -> String {
    "js".into()
}

/// On-disk plugin descriptor (`plugin.toml`, `api_version = 3`).
///
/// Root table for install / discovery, shaped after `wrangler.jsonc`: the
/// plugin exports named [`Entrypoint`]s, declares triggers for its default
/// entrypoint (`[triggers]`, `[[events.consumers]]`), and lists the host
/// bindings it expects on `env` (`[vars]`, `[secrets]`, `[[kv_namespaces]]`,
/// `[work_fs]`, `[oauth]`, `[[databases]]`, `[[events.producers]]`). Parse
/// with [`Self::parse`] (deserialize + [`Self::validate`]). Unknown keys are
/// rejected.
///
/// # Validation highlights
///
/// - `api_version` must equal [`bookclerk_plugin_abi::PRODUCT_API_VERSION`]
/// - `id` must pass [`crate::validate_plugin_id`]
/// - at least one entrypoint or trigger must be declared
/// - native requires `command`; workerd requires `[workerd]` with date + main
/// - `domains` forbidden on native; required for workerd + outbound
/// - optional `logo` must pass [`crate::validate_logo`]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// ABI / schema version. Must equal [`bookclerk_plugin_abi::PRODUCT_API_VERSION`].
    pub api_version: u32,
    /// Globally unique plugin id (`[a-z0-9_]{2,32}` grammar).
    pub id: String,
    /// Optional human-readable display name for Settings / Accounts UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional semver (or free-form) package version string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Settings / UI logo: `https://…` URL or relative image path under the
    /// plugin root. Validated by [`crate::validate_logo`] when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// Guest runtime (`native` default, or `workerd`). Selects the backend
    /// behind `bookclerk-workerd`; it does not change the ABI.
    #[serde(default)]
    pub runtime: PluginRuntimeKind,
    /// Native executable path relative to the install root (required when
    /// `runtime = "native"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<PathBuf>,
    /// Extra argv passed after `command` for native guests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Named entrypoints this plugin exports (`["storefront", "cli"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoints: Vec<Entrypoint>,
    /// Workerd isolate config (required when `runtime = "workerd"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workerd: Option<WorkerdRuntimeManifest>,
    /// Optional module list for workerd packages (`[[modules]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<ModuleSpec>,
    /// Default-entrypoint triggers other than events (`[triggers]`).
    #[serde(default, skip_serializing_if = "TriggersManifest::is_default")]
    pub triggers: TriggersManifest,
    /// Event consumers (triggers) and producers (publish grants).
    #[serde(default, skip_serializing_if = "EventsManifest::is_default")]
    pub events: EventsManifest,
    /// Isolated plugin-owned database bindings (`[[databases]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub databases: Vec<DatabaseBindingManifest>,
    /// Operator config binding (`[vars]`). Keys are defaults the host may
    /// override from `config.toml`; an empty table still declares the binding.
    #[serde(default, skip_serializing_if = "vars_is_none")]
    pub vars: Option<std::collections::BTreeMap<String, toml::Value>>,
    /// Sealed secrets binding (`[secrets]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<NamedBinding>,
    /// Per-plugin key/value storage bindings (`[[kv_namespaces]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kv_namespaces: Vec<NamedBinding>,
    /// Host-mediated work filesystem binding (`[work_fs]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_fs: Option<NamedBinding>,
    /// OAuth callback tunnel binding (`[oauth]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<NamedBinding>,
    /// Declared network capability (consent has no Wrangler analogue).
    pub capabilities: CapabilitiesManifest,
    /// Optional CLI schema advertised to `bookclerk plugins <id>` (from
    /// `bookclerk-plugin-abi::CliSchema`). Requires the `Cli` entrypoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliSchema>,
    /// Optional Bookclerk-as-IdP client templates (`[[oidc.clients]]`).
    #[serde(default, skip_serializing_if = "OidcManifest::is_empty")]
    pub oidc: OidcManifest,
}

impl Eq for PluginManifest {}

/// `[[oidc.clients]]` install-time OIDC AS templates.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OidcManifest {
    /// Client templates materialized when this plugin is installed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients: Vec<OidcClientToml>,
}

impl OidcManifest {
    /// True when no clients are declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }
}

/// One `[[oidc.clients]]` row (snake_case TOML).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OidcClientToml {
    /// OAuth `client_id`.
    pub client_id: String,
    /// Operator-facing card title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    /// Path appended to the plugin origin.
    pub callback_path: String,
    /// Public PKCE when true (default).
    #[serde(default = "default_true")]
    pub public_client: bool,
    /// Scopes for first materialization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_scopes: Vec<String>,
    /// Whether new rows may issue refresh tokens (default true).
    #[serde(default = "default_true")]
    pub issue_refresh_token: bool,
    /// Dotted config key for the player origin.
    pub origin_config_key: String,
}

/// Serde default for `public_client` and `issue_refresh_token`.
fn default_true() -> bool {
    true
}

impl PluginManifest {
    /// Resolves the declared host bindings into one flat view.
    ///
    /// # Returns
    ///
    /// [`BindingCapabilities`] with one flag per named-binding table and the
    /// `[[databases]]` binding names in declaration order.
    #[must_use]
    pub fn bindings(&self) -> BindingCapabilities {
        BindingCapabilities {
            config: self.vars.is_some(),
            secrets: self.secrets.is_some(),
            plugin_kv: !self.kv_namespaces.is_empty(),
            work_fs: self.work_fs.is_some(),
            oauth: self.oauth.is_some(),
            databases: self.databases.iter().map(|d| d.binding.clone()).collect(),
        }
    }

    /// True when the plugin exports `entrypoint`.
    #[must_use]
    pub fn has_entrypoint(&self, entrypoint: Entrypoint) -> bool {
        self.entrypoints.contains(&entrypoint)
    }

    /// True when the default entrypoint has an `event(batch)` trigger.
    #[must_use]
    pub fn consumes_events(&self) -> bool {
        !self.events.consumers.is_empty()
    }

    /// True when the default entrypoint has a `job(controller)` trigger.
    #[must_use]
    pub fn runs_jobs(&self) -> bool {
        !self.triggers.jobs.is_empty()
    }

    /// True when the plugin declares any publishable event type.
    #[must_use]
    pub fn produces_events(&self) -> bool {
        !self.events.producers.is_empty()
    }

    /// Handler families this plugin belongs to, in
    /// [`PluginFamily::ALL`] priority order.
    ///
    /// A plugin joins the `Source` / `Output` / `Database` families through
    /// the matching named entrypoint and the `Integration` family through
    /// event consumers, job triggers, `RemoteLibrary`, `Cli`, or `Oidc`.
    ///
    /// # Returns
    ///
    /// Deduplicated families; never empty for a validated manifest.
    #[must_use]
    pub fn families(&self) -> Vec<PluginFamily> {
        let mut out = Vec::new();
        for family in PluginFamily::ALL {
            let member = match family {
                PluginFamily::Source => self.has_entrypoint(Entrypoint::Storefront),
                PluginFamily::Output => self.has_entrypoint(Entrypoint::Storage),
                PluginFamily::Database => self.has_entrypoint(Entrypoint::DatabaseAdapter),
                PluginFamily::Integration => {
                    self.consumes_events()
                        || self.runs_jobs()
                        || self.has_entrypoint(Entrypoint::RemoteLibrary)
                        || self.has_entrypoint(Entrypoint::Oidc)
                        || (self.has_entrypoint(Entrypoint::Cli)
                            && !self.has_entrypoint(Entrypoint::Storefront)
                            && !self.has_entrypoint(Entrypoint::Storage)
                            && !self.has_entrypoint(Entrypoint::DatabaseAdapter))
                }
            };
            if member {
                out.push(family);
            }
        }
        out
    }

    /// Primary handler family: the first of [`Self::families`].
    ///
    /// Picks the `config.toml` settings prefix (`database.*` > `sources.*` >
    /// `output.*` > `integrations.*`) and the API grouping.
    ///
    /// # Returns
    ///
    /// The highest-priority family; `Integration` for a manifest with no
    /// entrypoint (only reachable before validation).
    #[must_use]
    pub fn primary_family(&self) -> PluginFamily {
        self.families()
            .into_iter()
            .next()
            .unwrap_or(PluginFamily::Integration)
    }

    /// Event types this plugin may publish (from `[[events.producers]]`).
    #[must_use]
    pub fn producer_types(&self) -> Vec<String> {
        self.events
            .producers
            .iter()
            .map(|p| p.event_type.clone())
            .collect()
    }

    /// Typed capability declaration this manifest implies.
    ///
    /// Guests return exactly this value from `describe()` so the host's
    /// widening check (manifest vs describe vs grant) is a plain equality
    /// comparison. Native Rust guests typically call it on the manifest
    /// embedded with `include_str!("../plugin.toml")`.
    ///
    /// # Returns
    ///
    /// Entrypoints, event consumers/producers, job types, database binding
    /// names, and named binding names (`CONFIG`, `SECRETS`, `WORK_FS`,
    /// `OAUTH`, KV and event bindings) in manifest order.
    #[must_use]
    pub fn capabilities(&self) -> PluginCapabilities {
        PluginCapabilities {
            entrypoints: self.entrypoints.clone(),
            consumes: self
                .events
                .consumers
                .iter()
                .map(|c| EventConsumerSpec {
                    event_type: c.event_type.clone(),
                    schema_versions: c.schema_versions.clone(),
                    supports_suspend: c.supports_suspend,
                })
                .collect(),
            produces: self.producer_types(),
            jobs: self.triggers.jobs.clone(),
            databases: self.databases.iter().map(|d| d.binding.clone()).collect(),
            bindings: self.binding_names(),
        }
    }

    /// Names of the non-database bindings this manifest exposes on `env`.
    ///
    /// # Returns
    ///
    /// `CONFIG` (when `[vars]` is present), the resolved `[secrets]`,
    /// `[work_fs]`, `[oauth]` names, every `[[kv_namespaces]]` binding, and
    /// every distinct `[[events.producers]]` binding, in that order.
    #[must_use]
    pub fn binding_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.vars.is_some() {
            names.push(CONFIG_BINDING.to_string());
        }
        if let Some(b) = &self.secrets {
            names.push(b.name_or(DEFAULT_SECRETS_BINDING).to_string());
        }
        if let Some(b) = &self.work_fs {
            names.push(b.name_or(DEFAULT_WORK_FS_BINDING).to_string());
        }
        if let Some(b) = &self.oauth {
            names.push(b.name_or(DEFAULT_OAUTH_BINDING).to_string());
        }
        for kv in &self.kv_namespaces {
            names.push(kv.name_or(DEFAULT_KV_BINDING).to_string());
        }
        for producer in &self.events.producers {
            let name = producer.binding_name().to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// Returns domains for workerd network consent UI.
    ///
    /// IDNA-normalizes author domains and includes Pyodide CDN hosts when this
    /// is a Python + outbound guest. On normalization failure, falls back to
    /// filtering author domains individually (invalid entries dropped).
    ///
    /// # Returns
    ///
    /// Deduplicated host patterns suitable for operator consent display.
    #[must_use]
    pub fn consent_domains(&self) -> Vec<String> {
        crate::egress::consent_domains_for(self).unwrap_or_else(|_| {
            self.capabilities
                .network
                .domains
                .iter()
                .filter_map(|d| crate::egress::normalize_domain_pattern(d))
                .collect()
        })
    }

    /// Classifies the optional `logo` field after validation.
    ///
    /// # Returns
    ///
    /// `Ok(None)` when `logo` is unset; `Ok(Some(kind))` when valid.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] from [`crate::validate_logo`] when `logo` is set but
    /// invalid.
    pub fn logo_kind(&self) -> Result<Option<crate::LogoKind>> {
        match self.logo.as_deref() {
            None => Ok(None),
            Some(raw) => Ok(Some(crate::validate_logo(raw)?)),
        }
    }

    /// Parses and validates a `plugin.toml` document.
    ///
    /// Deserializes with `toml::from_str` then runs [`Self::validate`].
    ///
    /// # Arguments
    ///
    /// * `text` - Full UTF-8 contents of `plugin.toml`.
    ///
    /// # Returns
    ///
    /// A semantically validated [`PluginManifest`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::TomlDe`] on malformed TOML, or [`Error::Message`] from
    /// [`Self::validate`].
    ///
    /// # Examples
    ///
    /// ```
    /// use bookclerk_plugin_manifest::PluginManifest;
    ///
    /// let m = PluginManifest::parse(r#"
    /// api_version = 3
    /// id = "echo"
    /// runtime = "native"
    /// command = "./echo"
    /// entrypoints = ["cli"]
    ///
    /// [capabilities.network]
    /// mode = "deny"
    /// "#).unwrap();
    /// assert_eq!(m.id, "echo");
    /// ```
    pub fn parse(text: &str) -> Result<Self> {
        let m: Self = toml::from_str(text)?;
        m.validate()?;
        Ok(m)
    }

    /// Runs semantic validation after deserialize.
    ///
    /// Checks id grammar, `api_version`, logo, runtime-specific required
    /// fields, and network domain rules (native forbid / workerd outbound
    /// require / IDNA).
    ///
    /// # Returns
    ///
    /// `Ok(())` when all rules pass.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Message`] describing the first failed rule.
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(Error::message("plugin.toml: `id` is required"));
        }
        // Validate the raw id (non-lossy): do not trim before grammar checks.
        crate::validate_plugin_id(&self.id)
            .map_err(|e| Error::message(format!("plugin.toml: {e}")))?;
        if self.api_version != bookclerk_plugin_abi::PRODUCT_API_VERSION {
            return Err(Error::message(format!(
                "plugin.toml: `api_version` must be {}",
                bookclerk_plugin_abi::PRODUCT_API_VERSION
            )));
        }
        if let Some(logo) = self.logo.as_deref() {
            let _ = crate::validate_logo(logo)?;
        }
        for (i, client) in self.oidc.clients.iter().enumerate() {
            if client.client_id.trim().is_empty() {
                return Err(Error::message(format!(
                    "plugin.toml: oidc.clients[{i}].client_id is required"
                )));
            }
            if client.callback_path.trim().is_empty() || !client.callback_path.starts_with('/') {
                return Err(Error::message(format!(
                    "plugin.toml: oidc.clients[{i}].callback_path must start with `/`"
                )));
            }
            if client.origin_config_key.trim().is_empty() {
                return Err(Error::message(format!(
                    "plugin.toml: oidc.clients[{i}].origin_config_key is required"
                )));
            }
        }
        match self.runtime {
            PluginRuntimeKind::Native => {
                if self
                    .command
                    .as_ref()
                    .is_none_or(|c| c.as_os_str().is_empty())
                {
                    return Err(Error::message(
                        "plugin.toml: `command` is required when runtime = \"native\"",
                    ));
                }
            }
            PluginRuntimeKind::Workerd => {
                let Some(w) = self.workerd.as_ref() else {
                    return Err(Error::message(
                        "plugin.toml: `[workerd]` / runtime.workerd is required when runtime = \"workerd\"",
                    ));
                };
                if w.compatibility_date.trim().is_empty() {
                    return Err(Error::message(
                        "plugin.toml: workerd.compatibility_date is required",
                    ));
                }
                if w.main_module.trim().is_empty() {
                    return Err(Error::message(
                        "plugin.toml: workerd.main_module is required",
                    ));
                }
            }
        }
        if self.runtime == PluginRuntimeKind::Native
            && !self.capabilities.network.domains.is_empty()
        {
            return Err(Error::message(
                "plugin.toml: capabilities.network.domains is only valid for runtime = \"workerd\" \
                 (native outbound is coarse jail networking with no hostname filter — omit domains)",
            ));
        }
        if self.runtime == PluginRuntimeKind::Workerd
            && self.capabilities.network.mode == NetworkMode::Outbound
            && self.capabilities.network.domains.is_empty()
        {
            return Err(Error::message(
                "plugin.toml: capabilities.network.domains is required when runtime = \"workerd\" \
                 and mode = \"outbound\"",
            ));
        }
        for domain in &self.capabilities.network.domains {
            if crate::egress::normalize_domain_pattern(domain).is_none() {
                return Err(Error::message(format!(
                    "plugin.toml: capabilities.network.domains entry `{domain}` is not a valid \
                     hostname (IDNA ToASCII failed or percent-encoded host)"
                )));
            }
        }
        if self.entrypoints.is_empty() && !self.consumes_events() && !self.runs_jobs() {
            return Err(Error::message(
                "plugin.toml: declare at least one of `entrypoints`, `[[events.consumers]]`, \
                 or `[triggers].jobs`",
            ));
        }
        {
            let mut seen = std::collections::HashSet::new();
            for entrypoint in &self.entrypoints {
                if !seen.insert(*entrypoint) {
                    return Err(Error::message(format!(
                        "plugin.toml: entrypoints entry `{}` is duplicated",
                        entrypoint.wire_name()
                    )));
                }
            }
        }
        if self.cli.is_some() && !self.has_entrypoint(Entrypoint::Cli) {
            return Err(Error::message(
                "plugin.toml: `[cli]` requires `\"cli\"` in `entrypoints`",
            ));
        }
        if !self.oidc.is_empty() && !self.has_entrypoint(Entrypoint::Oidc) {
            return Err(Error::message(
                "plugin.toml: `[[oidc.clients]]` requires `\"oidc\"` in `entrypoints`",
            ));
        }
        {
            if self.databases.len() > MAX_DATABASE_BINDINGS {
                return Err(Error::message(format!(
                    "plugin.toml: [[databases]] lists {} bindings; max is {MAX_DATABASE_BINDINGS}",
                    self.databases.len()
                )));
            }
            let mut seen = std::collections::HashSet::new();
            for db in &self.databases {
                let name = db.binding.as_str();
                if !is_valid_database_binding_name(name) {
                    return Err(Error::message(format!(
                        "plugin.toml: [[databases]] binding `{name}` must be \
                         `[A-Z][A-Z0-9_]*` and at most {MAX_DATABASE_BINDING_NAME_LEN} chars"
                    )));
                }
                if !seen.insert(name) {
                    return Err(Error::message(format!(
                        "plugin.toml: [[databases]] binding `{name}` is duplicated"
                    )));
                }
            }
            let mut named: Vec<(&str, &str)> = Vec::new();
            if let Some(b) = &self.secrets {
                named.push(("secrets", b.name_or(DEFAULT_SECRETS_BINDING)));
            }
            if let Some(b) = &self.work_fs {
                named.push(("work_fs", b.name_or(DEFAULT_WORK_FS_BINDING)));
            }
            if let Some(b) = &self.oauth {
                named.push(("oauth", b.name_or(DEFAULT_OAUTH_BINDING)));
            }
            for b in &self.kv_namespaces {
                named.push(("kv_namespaces", b.name_or(DEFAULT_KV_BINDING)));
            }
            for p in &self.events.producers {
                named.push(("events.producers", p.binding_name()));
            }
            for (table, name) in &named {
                if !is_valid_database_binding_name(name) {
                    return Err(Error::message(format!(
                        "plugin.toml: [{table}] binding `{name}` must be `[A-Z][A-Z0-9_]*`"
                    )));
                }
                if *name == CONFIG_BINDING || seen.contains(name) {
                    return Err(Error::message(format!(
                        "plugin.toml: [{table}] binding `{name}` collides with another binding"
                    )));
                }
            }
            for (table, name) in &named {
                if *table != "events.producers" && !seen.insert(name) {
                    return Err(Error::message(format!(
                        "plugin.toml: [{table}] binding `{name}` collides with another binding"
                    )));
                }
            }
        }
        for (i, job) in self.triggers.jobs.iter().enumerate() {
            if !is_snake_case_type(job) {
                return Err(Error::message(format!(
                    "plugin.toml: triggers.jobs[{i}] `{job}` must be snake_case `[a-z][a-z0-9_]*`"
                )));
            }
        }
        for (i, sub) in self.events.consumers.iter().enumerate() {
            if sub.event_type.trim().is_empty() {
                return Err(Error::message(format!(
                    "plugin.toml: events.consumers[{i}].type is required"
                )));
            }
            if !is_snake_case_type(&sub.event_type) {
                return Err(Error::message(format!(
                    "plugin.toml: events.consumers[{i}].type `{}` must be \
                     snake_case `[a-z][a-z0-9_]*`",
                    sub.event_type
                )));
            }
            if sub.schema_versions.is_empty() {
                return Err(Error::message(format!(
                    "plugin.toml: events.consumers[{i}].schema_versions must not be empty"
                )));
            }
            let class = sub.resource_class.trim();
            if !class.is_empty() && class != "network" {
                return Err(Error::message(format!(
                    "plugin.toml: events.consumers[{i}].resource_class \
                     `{class}` is not supported (only `network`)"
                )));
            }
        }
        {
            let mut seen = std::collections::HashSet::new();
            for (i, producer) in self.events.producers.iter().enumerate() {
                if !is_snake_case_type(&producer.event_type) {
                    return Err(Error::message(format!(
                        "plugin.toml: events.producers[{i}].type `{}` must be \
                         snake_case `[a-z][a-z0-9_]*`",
                        producer.event_type
                    )));
                }
                if !seen.insert(producer.event_type.as_str()) {
                    return Err(Error::message(format!(
                        "plugin.toml: events.producers[{i}].type `{}` is duplicated",
                        producer.event_type
                    )));
                }
            }
        }
        Ok(())
    }

    /// Resolves the process path to spawn for this guest.
    ///
    /// # Returns
    ///
    /// `Some(command)` for native guests; `None` for workerd (the host
    /// resolves the `bookclerk-workerd` helper beside itself).
    #[must_use]
    pub fn spawn_command(&self) -> Option<&PathBuf> {
        match self.runtime {
            PluginRuntimeKind::Native => self.command.as_ref(),
            PluginRuntimeKind::Workerd => None, // host resolves bookclerk-workerd
        }
    }

    /// Maps manifest network + oauth binding to OS jail network policy.
    ///
    /// Native guests get coarse jail outbound ([`JailNetworkNeed::Outbound`])
    /// when `mode = "outbound"`; **hostname allowlists are not supported** on
    /// native (see `domains` / workerd). With `bindings.oauth`, native guests
    /// also need loopback listen for the host OAuth callback tunnel
    /// ([`JailNetworkNeed::Listen`]). Workerd always needs loopback
    /// listen/connect to its Cloudflare child (domain policy is enforced
    /// inside the isolate when `domains` are set).
    ///
    /// # Returns
    ///
    /// The jail network need the host should apply when spawning the guest.
    #[must_use]
    pub fn jail_network_need(&self) -> JailNetworkNeed {
        if self.runtime == PluginRuntimeKind::Workerd {
            return JailNetworkNeed::Listen;
        }
        match self.capabilities.network.mode {
            NetworkMode::Deny => JailNetworkNeed::None,
            NetworkMode::Outbound if self.oauth.is_some() => JailNetworkNeed::Listen,
            NetworkMode::Outbound => JailNetworkNeed::Outbound,
        }
    }
}

/// True for `[a-z][a-z0-9_]*` event / command type names.
fn is_snake_case_type(name: &str) -> bool {
    !name.is_empty()
        && name.chars().enumerate().all(|(j, c)| {
            if j == 0 {
                c.is_ascii_lowercase()
            } else {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
            }
        })
}

/// OS jail network capability derived from manifest network + oauth binding.
///
/// Produced by [`PluginManifest::jail_network_need`]. Distinct from isolate
/// hostname allowlists ([`crate::EgressPolicy`]), which apply only inside
/// workerd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JailNetworkNeed {
    /// No IP sockets (`mode = "deny"` on native).
    None,
    /// Native `outbound` without OAuth — coarse jail outbound (no hostname filter).
    Outbound,
    /// Native `outbound` + `bindings.oauth`, or any workerd guest (loopback
    /// listen/connect to the Cloudflare child / OAuth callback tunnel).
    Listen,
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    /// Minimal v3 native manifest body; callers append tables.
    fn native(extra: &str) -> String {
        format!(
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./echo"
entrypoints = ["remoteLibrary"]
[capabilities.network]
mode = "deny"
{extra}
"#
        )
    }

    #[test]
    fn workerd_limits_unset_and_zero_use_defaults() {
        assert_eq!(
            WorkerdLimits::default().effective(),
            EffectiveWorkerdLimits {
                cpu_ms: WorkerdLimits::DEFAULT_CPU_MS,
                subrequests: WorkerdLimits::DEFAULT_SUBREQUESTS,
            }
        );
        assert_eq!(
            WorkerdLimits {
                cpu_ms: Some(0),
                subrequests: Some(0),
            }
            .clamp(),
            EffectiveWorkerdLimits {
                cpu_ms: WorkerdLimits::DEFAULT_CPU_MS,
                subrequests: WorkerdLimits::DEFAULT_SUBREQUESTS,
            }
        );
    }

    #[test]
    fn workerd_limits_over_cap_are_clamped() {
        let over = WorkerdLimits {
            cpu_ms: Some(WorkerdLimits::MAX_CPU_MS + 1),
            subrequests: Some(WorkerdLimits::MAX_SUBREQUESTS + 50),
        }
        .effective();
        assert_eq!(over.cpu_ms, WorkerdLimits::MAX_CPU_MS);
        assert_eq!(over.subrequests, WorkerdLimits::MAX_SUBREQUESTS);
    }

    #[test]
    fn workerd_limits_in_range_pass_through() {
        let mid = WorkerdLimits {
            cpu_ms: Some(45_000),
            subrequests: Some(100),
        }
        .effective();
        assert_eq!(mid.cpu_ms, 45_000);
        assert_eq!(mid.subrequests, 100);
    }

    #[test]
    fn parse_workerd_echo() {
        let m = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
version = "1.0.0"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"

[capabilities.network]
mode = "deny"

[vars]
"#,
        )
        .unwrap();
        assert_eq!(m.runtime, PluginRuntimeKind::Workerd);
        assert_eq!(m.capabilities.network.mode, NetworkMode::Deny);
        assert!(m.bindings().config);
        assert_eq!(m.families(), vec![PluginFamily::Integration]);
    }

    #[test]
    fn families_follow_entrypoints_and_triggers() {
        let m = PluginManifest::parse(&native("")).unwrap();
        assert_eq!(m.primary_family(), PluginFamily::Integration);

        let storefront = PluginManifest::parse(
            r#"
api_version = 3
id = "shop"
runtime = "native"
command = "./shop"
entrypoints = ["storefront", "cli"]
[capabilities.network]
mode = "outbound"
"#,
        )
        .unwrap();
        assert_eq!(storefront.families(), vec![PluginFamily::Source]);
        assert_eq!(storefront.primary_family(), PluginFamily::Source);

        let consumer = PluginManifest::parse(
            r#"
api_version = 3
id = "abs"
runtime = "native"
command = "./abs"
entrypoints = ["storage"]
[capabilities.network]
mode = "deny"
[[events.consumers]]
type = "book_acquired"
"#,
        )
        .unwrap();
        assert_eq!(
            consumer.families(),
            vec![PluginFamily::Output, PluginFamily::Integration]
        );
        assert_eq!(consumer.primary_family(), PluginFamily::Output);

        let db = PluginManifest::parse(
            r#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./sqlite"
entrypoints = ["databaseAdapter", "storefront"]
[capabilities.network]
mode = "deny"
"#,
        )
        .unwrap();
        assert_eq!(db.primary_family(), PluginFamily::Database);
    }

    #[test]
    fn manifest_without_entrypoints_or_triggers_is_rejected() {
        let err = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./echo"
[capabilities.network]
mode = "deny"
"#,
        )
        .expect_err("no entrypoint");
        assert!(err.to_string().contains("entrypoints"), "{err}");

        let jobs_only = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./echo"
[triggers]
jobs = ["stream_copy"]
[capabilities.network]
mode = "deny"
"#,
        )
        .unwrap();
        assert!(jobs_only.runs_jobs());
        assert_eq!(jobs_only.families(), vec![PluginFamily::Integration]);
    }

    #[test]
    fn duplicate_entrypoints_and_unknown_names_are_rejected() {
        let dup = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./echo"
entrypoints = ["cli", "cli"]
[capabilities.network]
mode = "deny"
"#,
        )
        .expect_err("duplicate entrypoint");
        assert!(dup.to_string().contains("duplicated"), "{dup}");
        let unknown = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./echo"
entrypoints = ["integration"]
[capabilities.network]
mode = "deny"
"#,
        )
        .expect_err("lowercase kind names are not entrypoints");
        assert!(unknown.to_string().contains("integration"), "{unknown}");
    }

    #[test]
    fn cli_and_oidc_tables_require_their_entrypoints() {
        let err = PluginManifest::parse(&native(
            r#"
[cli]
commands = []
"#,
        ))
        .expect_err("cli without Cli entrypoint");
        assert!(err.to_string().contains("cli"), "{err}");

        let err = PluginManifest::parse(&native(
            r#"
[[oidc.clients]]
client_id = "echo-player"
callback_path = "/auth/openid/callback"
origin_config_key = "integrations.echo.base_url"
"#,
        ))
        .expect_err("oidc without Oidc entrypoint");
        assert!(err.to_string().contains("oidc"), "{err}");
    }

    #[test]
    fn parse_oidc_clients() {
        let m = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
version = "1.0.0"
runtime = "native"
command = "./echo"
entrypoints = ["oidc"]

[capabilities.network]
mode = "deny"

[vars]

[[oidc.clients]]
client_id = "echo-player"
display_name = "Echo Player"
callback_path = "/auth/openid/callback"
origin_config_key = "integrations.echo.base_url"
"#,
        )
        .unwrap();
        assert_eq!(m.oidc.clients.len(), 1);
        assert_eq!(m.oidc.clients[0].client_id, "echo-player");
        assert!(m.oidc.clients[0].public_client);
        assert!(m.oidc.clients[0].issue_refresh_token);
    }

    #[test]
    fn oidc_callback_path_must_be_absolute() {
        let err = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./echo"
entrypoints = ["oidc"]

[capabilities.network]
mode = "deny"

[[oidc.clients]]
client_id = "echo-player"
callback_path = "auth/callback"
origin_config_key = "integrations.echo.base_url"
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("callback_path must start with `/`"),
            "{err}"
        );
    }

    #[test]
    fn api_version_2_is_rejected() {
        let err = PluginManifest::parse(
            r#"
api_version = 2
id = "echo"
runtime = "native"
command = "./echo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
"#,
        )
        .expect_err("api_version 2 is removed");
        assert!(err.to_string().contains("must be 3"), "{err}");
    }

    #[test]
    fn legacy_kind_and_methods_keys_are_rejected() {
        let err = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
kind = "integration"
runtime = "native"
command = "./echo"
[capabilities.network]
mode = "deny"
"#,
        )
        .expect_err("kind removed");
        assert!(err.to_string().contains("kind"), "{err}");
        let err = PluginManifest::parse(&native(
            r#"
[capabilities.methods]
list = ["health"]
"#,
        ))
        .expect_err("methods removed");
        assert!(err.to_string().contains("methods"), "{err}");
        let err = PluginManifest::parse(&native(
            r#"
[capabilities.bindings]
config = true
"#,
        ))
        .expect_err("bindings removed");
        assert!(err.to_string().contains("bindings"), "{err}");
    }

    #[test]
    fn event_consumers_parse_and_default_schema() {
        let m = PluginManifest::parse(&native(
            r#"
[[events.consumers]]
type = "book_acquired"
supports_suspend = true
"#,
        ))
        .unwrap();
        assert_eq!(m.events.consumers.len(), 1);
        assert_eq!(m.events.consumers[0].event_type, "book_acquired");
        assert_eq!(m.events.consumers[0].schema_versions, vec![1]);
        assert!(m.events.consumers[0].supports_suspend);
        assert_eq!(m.events.consumers[0].resource_class, "network");
        assert!(m.events.consumers[0].filter.is_none());
        assert!(m.events.consumers[0].max_retries.is_none());
        assert!(m.consumes_events());
    }

    #[test]
    fn event_consumers_parse_resource_class_and_filter() {
        let m = PluginManifest::parse(&native(
            r#"
[[events.consumers]]
type = "book_acquired"
resource_class = "network"
filter = { source = "audible" }
max_retries = 3
"#,
        ))
        .unwrap();
        let sub = &m.events.consumers[0];
        assert_eq!(sub.resource_class, "network");
        assert_eq!(sub.max_retries, Some(3));
        let filter = sub.filter.as_ref().and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            filter.get("source").and_then(|v| v.as_str()),
            Some("audible")
        );
    }

    #[test]
    fn event_consumers_reject_unknown_resource_class() {
        for class in ["cpu", "netwrok"] {
            let err = PluginManifest::parse(&native(&format!(
                r#"
[[events.consumers]]
type = "book_acquired"
resource_class = "{class}"
"#
            )))
            .expect_err("unsupported resource_class");
            assert!(err.to_string().contains("resource_class"), "{err}");
        }
    }

    #[test]
    fn event_producers_default_binding_and_reject_duplicates() {
        let m = PluginManifest::parse(&native(
            r#"
[[events.producers]]
type = "echo_seen"
[[events.producers]]
type = "echo_done"
binding = "OUT"
"#,
        ))
        .unwrap();
        assert_eq!(m.events.producers[0].binding_name(), DEFAULT_EVENTS_BINDING);
        assert_eq!(m.events.producers[1].binding_name(), "OUT");
        assert_eq!(m.producer_types(), vec!["echo_seen", "echo_done"]);
        assert!(m.produces_events());

        let dup = PluginManifest::parse(&native(
            r#"
[[events.producers]]
type = "echo_seen"
[[events.producers]]
type = "echo_seen"
"#,
        ))
        .expect_err("duplicate producer");
        assert!(dup.to_string().contains("duplicated"), "{dup}");
        let camel = PluginManifest::parse(&native(
            r#"
[[events.producers]]
type = "EchoSeen"
"#,
        ))
        .expect_err("camelCase producer type");
        assert!(camel.to_string().contains("snake_case"), "{camel}");
    }

    #[test]
    fn workerd_outbound_requires_domains() {
        let err = PluginManifest::parse(
            r#"
api_version = 3
id = "xx"
runtime = "workerd"
entrypoints = ["storefront"]
[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
[capabilities.network]
mode = "outbound"
"#,
        )
        .expect_err("domains required for workerd outbound");
        assert!(err.to_string().contains("domains"), "{err}");
    }

    #[test]
    fn database_bindings_validate_names_and_uniqueness() {
        let manifest = |rows: &[&str]| {
            let tables: String = rows
                .iter()
                .map(|name| format!("[[databases]]\nbinding = \"{name}\"\n"))
                .collect();
            PluginManifest::parse(&native(&tables))
        };
        let ok = manifest(&["DB", "CACHE_2"]).expect("valid binding names");
        assert_eq!(ok.bindings().databases, vec!["DB", "CACHE_2"]);
        let bad = manifest(&["db"]).expect_err("lowercase rejected");
        assert!(bad.to_string().contains("A-Z"), "{bad}");
        let dup = manifest(&["DB", "DB"]).expect_err("duplicates rejected");
        assert!(dup.to_string().contains("duplicated"), "{dup}");
        let digit = manifest(&["1DB"]).expect_err("leading digit rejected");
        assert!(digit.to_string().contains("A-Z"), "{digit}");
        let many = manifest(&["A1", "A2", "A3", "A4", "A5", "A6", "A7", "A8", "A9"])
            .expect_err("over max bindings");
        assert!(many.to_string().contains("max is"), "{many}");
    }

    #[test]
    fn named_bindings_default_and_collide() {
        let m = PluginManifest::parse(&native(
            r#"
[secrets]
[work_fs]
[oauth]
binding = "LOGIN"
[[kv_namespaces]]
binding = "KV"
[[databases]]
binding = "DB"
"#,
        ))
        .unwrap();
        let b = m.bindings();
        assert!(b.secrets && b.work_fs && b.oauth && b.plugin_kv && !b.config);
        assert_eq!(b.databases, vec!["DB"]);
        assert_eq!(
            m.oauth.as_ref().unwrap().name_or(DEFAULT_OAUTH_BINDING),
            "LOGIN"
        );
        assert_eq!(
            m.secrets.as_ref().unwrap().name_or(DEFAULT_SECRETS_BINDING),
            "SECRETS"
        );

        let collide = PluginManifest::parse(&native(
            r#"
[secrets]
binding = "DB"
[[databases]]
binding = "DB"
"#,
        ))
        .expect_err("secrets collides with database binding");
        assert!(collide.to_string().contains("collides"), "{collide}");
        let reserved = PluginManifest::parse(&native(
            r#"
[secrets]
binding = "CONFIG"
"#,
        ))
        .expect_err("CONFIG is reserved for [vars]");
        assert!(reserved.to_string().contains("collides"), "{reserved}");
    }

    #[test]
    fn leftover_migration_plan_field_is_rejected() {
        let err = PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]
migration_plan = "migrations.toml"
[capabilities.network]
mode = "deny"
[[databases]]
binding = "DB"
"#,
        )
        .expect_err("static migration_plan is not a plugin.toml field");
        let msg = err.to_string();
        assert!(
            msg.contains("migration_plan") || msg.to_lowercase().contains("unknown"),
            "{err}"
        );
    }

    #[test]
    fn native_outbound_forbids_domains() {
        let err = PluginManifest::parse(
            r#"
api_version = 3
id = "audible"
runtime = "native"
command = "./bookclerk-plugin-source-audible"
entrypoints = ["storefront"]
[capabilities.network]
mode = "outbound"
domains = ["api.audible.com", "www.amazon.com"]
[oauth]
[secrets]
"#,
        )
        .expect_err("domains forbidden on native");
        assert!(err.to_string().contains("only valid for runtime"), "{err}");
    }

    #[test]
    fn native_outbound_without_domains_ok() {
        let m = PluginManifest::parse(
            r#"
api_version = 3
id = "audible"
runtime = "native"
command = "./bookclerk-plugin-source-audible"
entrypoints = ["storefront"]
[capabilities.network]
mode = "outbound"
[oauth]
[secrets]
"#,
        )
        .unwrap();
        assert_eq!(
            m.jail_network_need(),
            JailNetworkNeed::Listen,
            "native outbound + oauth needs loopback listen"
        );
        assert!(m.logo_kind().unwrap().is_none());
    }

    #[test]
    fn parse_echo_native_rust_fixture() {
        let raw = include_str!("../../../examples/plugins-echo-native-rust/plugin.toml");
        let m = PluginManifest::parse(raw).expect("echo native rust plugin.toml");
        assert_eq!(m.id, "echo_native_rust");
        assert!(m.cli.is_some());
        assert!(m.has_entrypoint(Entrypoint::Cli));
    }

    #[test]
    fn workerd_outbound_with_domains_ok() {
        let m = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "workerd"
entrypoints = ["remoteLibrary"]
[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]
[vars]
"#,
        )
        .unwrap();
        assert!(m.logo.is_none());
        assert_eq!(m.consent_domains(), vec!["api.example.com".to_string()]);
    }

    #[test]
    fn logo_https_ok() {
        let m = PluginManifest::parse(
            r#"
api_version = 3
id = "audible"
runtime = "native"
command = "./bin"
entrypoints = ["storefront"]
logo = "https://www.google.com/s2/favicons?domain=audible.com&sz=128"
[capabilities.network]
mode = "outbound"
"#,
        )
        .unwrap();
        assert!(matches!(
            m.logo_kind().unwrap(),
            Some(crate::LogoKind::RemoteUrl(_))
        ));
    }

    #[test]
    fn logo_relative_path_ok() {
        let m = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./bin"
entrypoints = ["cli"]
logo = "assets/logo.png"
[capabilities.network]
mode = "deny"
"#,
        )
        .unwrap();
        assert!(matches!(
            m.logo_kind().unwrap(),
            Some(crate::LogoKind::EmbeddedPath(_))
        ));
    }

    #[test]
    fn logo_javascript_rejected() {
        let err = PluginManifest::parse(
            r#"
api_version = 3
id = "echo"
runtime = "native"
command = "./bin"
entrypoints = ["cli"]
logo = "javascript:alert(1)"
[capabilities.network]
mode = "deny"
"#,
        )
        .expect_err("javascript logo");
        assert!(err.to_string().contains("logo"), "{err}");
    }

    #[test]
    fn id_with_leading_or_trailing_whitespace_rejected() {
        for padded in [" echo", "echo "] {
            let toml = format!(
                r#"
api_version = 3
id = "{padded}"
runtime = "native"
command = "./bin"
entrypoints = ["cli"]
[capabilities.network]
mode = "deny"
"#
            );
            let err = PluginManifest::parse(&toml).expect_err(padded);
            assert!(
                err.to_string().contains("whitespace"),
                "padded id `{padded}`: {err}"
            );
        }
    }

    #[test]
    fn entrypoint_names_roundtrip() {
        for e in ALL_ENTRYPOINTS {
            assert_eq!(Entrypoint::from_wire_name(e.wire_name()), Some(e));
        }
        assert_eq!(Entrypoint::from_wire_name("Storefront"), None);
        for f in PluginFamily::ALL {
            assert_eq!(PluginFamily::parse(f.as_str()), Some(f));
        }
    }
}
