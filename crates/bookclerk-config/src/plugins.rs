//! Plugin-style `[sources.*]` and `[integrations.*]` configuration.
//!
//! Source plugins are opaque TOML tables under `[sources.<id>]`. Host code
//! never names store-specific knobs — each content-source crate parses its
//! own table at registration time. Integrations use the same opaque-table
//! pattern; first-party ABS is read via [`IntegrationsConfig::audiobookshelf`].
//!
//! External (subprocess) plugins are *discovered* via `plugin.toml` under
//! plugin search dirs; these tables hold enablement and opaque knobs passed
//! at handshake. See `docs/plugins.md`.
//!
//! ```toml
//! [sources.audible]
//! enabled = true
//! bitrate = "high"
//!
//! [sources.libro]
//! enabled = true
//! container = "m4b"
//!
//! [integrations.audiobookshelf]
//! enabled = false
//!
//! [integrations.echo]
//! enabled = true
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-content-source plugins under `[sources]`.
///
/// Each key is a plugin id (`audible`, `libro`, …); values are opaque tables
/// owned by that plugin (`enabled`, bitrate/container/access, …).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(transparent)]
pub struct SourcesConfig {
    /// Opaque `[sources.<id>]` tables keyed by plugin id (`audible`, `libro`, …).
    pub plugins: BTreeMap<String, toml::Value>,
}

impl SourcesConfig {
    /// Borrow a plugin table when present and well-formed.
    #[must_use]
    pub fn table(&self, id: &str) -> Option<&toml::Table> {
        self.plugins.get(id)?.as_table()
    }

    /// Mutable plugin table, creating an empty table if needed.
    pub fn table_mut(&mut self, id: &str) -> &mut toml::Table {
        let entry = self
            .plugins
            .entry(id.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        match entry {
            toml::Value::Table(t) => t,
            _ => unreachable!("plugin entry forced to table"),
        }
    }

    /// Whether a content source id should be registered / scanned.
    ///
    /// Missing tables default to enabled (`true`) so first-party sources work
    /// out of the box before the user writes a `[sources.*]` section.
    #[must_use]
    pub fn is_enabled(&self, source: &str) -> bool {
        self.table(source)
            .and_then(|t| t.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    /// Set `enabled` on a plugin table.
    pub fn set_enabled(&mut self, source: &str, enabled: bool) {
        self.table_mut(source)
            .insert("enabled".into(), toml::Value::Boolean(enabled));
    }

    /// Set a string-valued plugin knob (`bitrate`, `container`, …).
    pub fn set_string(&mut self, source: &str, key: &str, value: impl Into<String>) {
        self.table_mut(source)
            .insert(key.into(), toml::Value::String(value.into()));
    }

    /// Read a string-valued plugin knob.
    #[must_use]
    pub fn get_string(&self, source: &str, key: &str) -> Option<&str> {
        self.table(source)?.get(key)?.as_str()
    }

    /// Apply a dotted override `sources.<id>.<key>=value` (bool or string).
    pub fn apply_dotted_override(&mut self, remainder: &str, value: &str) -> bool {
        let Some((id, key)) = remainder.split_once('.') else {
            return false;
        };
        if id.is_empty() || key.is_empty() {
            return false;
        }
        if key == "enabled" {
            if let Some(b) = parse_bool_loose(value) {
                self.set_enabled(id, b);
                return true;
            }
            return false;
        }
        if let Some(b) = parse_bool_loose(value) {
            self.table_mut(id)
                .insert(key.into(), toml::Value::Boolean(b));
        } else {
            self.set_string(id, key, value.trim());
        }
        true
    }
}

/// Parses common truthy/falsey strings (`1`/`true`/`yes`/`on`); `None` when unrecognized.
fn parse_bool_loose(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// One configured plugin registry source (`[[plugins.registries]]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginRegistryEntry {
    /// Adapter kind: `static`, `cargo`, `npm`, or `pypi`.
    pub kind: String,
    /// Index / registry URL (required for `static`; optional overrides for others).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Optional operator-facing name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Optional global resource ceilings for plugin guest jails under `[plugins.jail]`.
///
/// These override label defaults when building a guest jail spec.
/// Guest filesystem access remains install read-only plus host-managed data/tmp —
/// not a free-form path widen.
///
/// `cpu_rate_percent` is percent of **one logical CPU** (100 = one core) and is a
/// **per-jail ceiling** only (`min(grant, this, host_max)`). Quotas are rate
/// limits, not reservations: if many plugins’ ceilings sum above host capacity,
/// the OS scheduler shares CPU among runnable guests. Bookclerk does not keep a
/// cumulative Σ ledger or throttle later spawns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsJailConfig {
    /// Soft memory ceiling in mebibytes (mapped to Spec `memory_bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    /// Per-jail CPU rate ceiling (one-core percent units). Defaults to 80.
    #[serde(default = "default_jail_cpu_rate_percent")]
    pub cpu_rate_percent: Option<u32>,
    /// Ceiling on **extra** processes/threads beyond launcher overhead. Defaults to 2.
    #[serde(default = "default_jail_extra_processes")]
    pub extra_processes: Option<u32>,
}

/// Default per-jail CPU ceiling: 80% of one logical core.
fn default_jail_cpu_rate_percent() -> Option<u32> {
    Some(80)
}

/// Default extra process/thread budget beyond jail launcher overhead (2).
fn default_jail_extra_processes() -> Option<u32> {
    Some(2)
}

impl Default for PluginsJailConfig {
    fn default() -> Self {
        Self {
            memory_mib: None,
            cpu_rate_percent: Some(80),
            extra_processes: Some(2),
        }
    }
}

/// `[plugins]` — how external plugin guests are run.
///
/// Separate from `[sources.*]` / `[integrations.*]`, which say *which* plugins
/// to load and pass them their own knobs. This says how the host runs whichever
/// ones it loads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PluginsConfig {
    /// How strictly guest processes must be confined.
    pub isolation: crate::Isolation,
    /// Explicit path to `bookclerk-jail`. Normally left unset, in which case the
    /// launcher is found beside the running executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jail_bin: Option<std::path::PathBuf>,
    /// Optional global jail resource ceilings for all plugin guests.
    #[serde(default, skip_serializing_if = "PluginsJailConfig::is_default")]
    pub jail: PluginsJailConfig,
    /// Federated discovery sources (static indexes, cargo, npm, pypi).
    ///
    /// Search order is list order; default when empty is crates.io only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registries: Vec<PluginRegistryEntry>,
    /// When true, unsigned community plugins may be installed without
    /// `--allow-unsigned` (digests are still required).
    #[serde(default)]
    pub allow_unsigned: bool,
}

impl PluginsJailConfig {
    /// True when jail ceilings match [`Self::default`] so serde can omit `[plugins.jail]`.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Clamp CPU / extra-process ceilings into host-safe ranges when set.
    pub fn clamp(&mut self) {
        if let Some(cpu) = self.cpu_rate_percent {
            self.cpu_rate_percent = Some(cpu.clamp(1, host_cpu_rate_max()));
        }
        if let Some(extra) = self.extra_processes {
            self.extra_processes = Some(extra.min(62));
        }
    }
}

/// Logical CPUs visible to this process (at least 1).
#[must_use]
pub fn host_logical_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1)
}

/// Host CPU rate ceiling in one-core percent units (`logical_cpus × 100`).
#[must_use]
pub fn host_cpu_rate_max() -> u32 {
    host_logical_cpus().saturating_mul(100)
}

impl PluginsConfig {
    /// Apply `BOOKCLERK_PLUGIN_*` environment overrides.
    ///
    /// Environment wins over TOML, matching every other section.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(value) = std::env::var("BOOKCLERK_PLUGIN_ISOLATION") {
            if let Some(isolation) = crate::Isolation::parse(&value) {
                self.isolation = isolation;
            }
        }
        if let Ok(value) = std::env::var("BOOKCLERK_PLUGIN_JAIL") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                self.jail_bin = Some(std::path::PathBuf::from(trimmed));
            }
        }
        if let Ok(value) = std::env::var("BOOKCLERK_PLUGIN_ALLOW_UNSIGNED") {
            if let Some(b) = parse_bool_loose(&value) {
                self.allow_unsigned = b;
            }
        }
        if let Ok(value) = std::env::var("BOOKCLERK_PLUGIN_JAIL_MEMORY_MIB") {
            if let Ok(memory_mib) = value.trim().parse::<u64>() {
                self.jail.memory_mib = Some(memory_mib);
            }
        }
        if let Ok(value) = std::env::var("BOOKCLERK_PLUGIN_JAIL_CPU_RATE_PERCENT") {
            if let Ok(cpu) = value.trim().parse::<u32>() {
                self.jail.cpu_rate_percent = Some(cpu.clamp(1, host_cpu_rate_max()));
            }
        }
        if let Ok(value) = std::env::var("BOOKCLERK_PLUGIN_JAIL_EXTRA_PROCESSES") {
            if let Ok(extra) = value.trim().parse::<u32>() {
                self.jail.extra_processes = Some(extra.min(62));
            }
        }
        self.jail.clamp();
    }
}

/// Optional third-party integrations under `[integrations]`.
///
/// Typed portal knobs live alongside opaque `[integrations.<id>]` tables in
/// [`Self::plugins`] (including first-party Audiobookshelf).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IntegrationsConfig {
    /// Hours a connect-portal claim ticket remains valid (default 72).
    pub claim_ticket_ttl_hours: u64,
    /// Public HTTPS origin for portal redirects when behind a reverse proxy
    /// (e.g. `https://bookclerk.example.com`). `None` derives from the request.
    pub public_origin: Option<String>,
    /// Hours a portal browser session cookie remains valid (default 12).
    pub portal_session_ttl_hours: u64,
    /// Opaque tables for integration plugins (including `audiobookshelf`).
    #[serde(flatten)]
    pub plugins: BTreeMap<String, toml::Value>,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self {
            claim_ticket_ttl_hours: 72,
            public_origin: None,
            portal_session_ttl_hours: 12,
            plugins: BTreeMap::new(),
        }
    }
}

impl IntegrationsConfig {
    /// Canonical table id for Audiobookshelf (`audiobookshelf`; `abs` is an alias).
    fn abs_table_id(integration: &str) -> Option<&'static str> {
        match integration.trim().to_ascii_lowercase().as_str() {
            "audiobookshelf" | "abs" => Some("audiobookshelf"),
            _ => None,
        }
    }

    /// Parse `[integrations.audiobookshelf]` from the opaque plugins map.
    #[must_use]
    pub fn audiobookshelf(&self) -> AudiobookshelfConfig {
        self.plugin_table("audiobookshelf")
            .and_then(|t| toml::Value::Table(t.clone()).try_into().ok())
            .unwrap_or_default()
    }

    /// Set a string field on `[integrations.audiobookshelf]`.
    pub fn set_audiobookshelf_string(&mut self, key: &str, value: impl Into<String>) {
        self.plugin_table_mut("audiobookshelf")
            .insert(key.into(), toml::Value::String(value.into()));
    }

    /// Set a boolean field on `[integrations.audiobookshelf]`.
    pub fn set_audiobookshelf_bool(&mut self, key: &str, value: bool) {
        self.plugin_table_mut("audiobookshelf")
            .insert(key.into(), toml::Value::Boolean(value));
    }

    /// Whether an integration plugin id is enabled.
    ///
    /// Missing tables default to **disabled** unless
    /// `[integrations.<id>] enabled = true`. `abs` aliases `audiobookshelf`.
    #[must_use]
    pub fn is_enabled(&self, integration: &str) -> bool {
        let id = Self::abs_table_id(integration).unwrap_or(integration);
        self.plugin_table(id)
            .and_then(|t| t.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Borrow an opaque plugin table when present.
    #[must_use]
    pub fn plugin_table(&self, id: &str) -> Option<&toml::Table> {
        let id = Self::abs_table_id(id).unwrap_or(id);
        self.plugins.get(id)?.as_table()
    }

    /// Mutable opaque plugin table (creates empty table if needed).
    ///
    /// `abs` writes to the `audiobookshelf` table.
    pub fn plugin_table_mut(&mut self, id: &str) -> &mut toml::Table {
        let id = Self::abs_table_id(id).unwrap_or(id);
        let entry = self
            .plugins
            .entry(id.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        match entry {
            toml::Value::Table(t) => t,
            _ => unreachable!("plugin entry forced to table"),
        }
    }

    /// Set `enabled` for an integration id (opaque plugin table).
    pub fn set_enabled(&mut self, integration: &str, enabled: bool) {
        let id = Self::abs_table_id(integration).unwrap_or(integration);
        self.plugin_table_mut(id)
            .insert("enabled".into(), toml::Value::Boolean(enabled));
    }
}

/// Audiobookshelf integration settings (`[integrations.audiobookshelf]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AudiobookshelfConfig {
    /// When true, ABS sync / notify hooks are active (default false).
    pub enabled: bool,
    /// Audiobookshelf server origin without trailing slash (e.g. `https://abs.example`).
    pub base_url: String,
    /// ABS API token; registered for log redaction when set.
    pub api_key: Option<String>,
    /// Target ABS library UUID when the server has multiple libraries.
    pub library_id: Option<String>,
    /// When true, poll ABS users for listening-progress sync.
    pub watch_users: bool,
    /// When true, ask ABS to rescan after a successful acquire.
    pub notify_scan_on_acquire: bool,
    /// When true, allow signing in to the Bookclerk portal with ABS credentials.
    pub allow_credential_login: bool,
}

impl Default for AudiobookshelfConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            api_key: None,
            library_id: None,
            watch_users: true,
            notify_scan_on_acquire: true,
            allow_credential_login: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn plugins_jail_toml_round_trips() {
        let config: Config = toml::from_str(
            r#"
[plugins.jail]
memory_mib = 768
cpu_rate_percent = 40
extra_processes = 6
"#,
        )
        .expect("parse");
        assert_eq!(config.plugins.jail.memory_mib, Some(768));
        assert_eq!(config.plugins.jail.cpu_rate_percent, Some(40));
        assert_eq!(config.plugins.jail.extra_processes, Some(6));
        let encoded = toml::to_string(&config.plugins).expect("encode");
        assert!(
            encoded.contains("memory_mib = 768"),
            "expected jail memory in {encoded}"
        );
    }

    #[test]
    fn plugins_jail_defaults_cpu_80_and_extra_2() {
        let jail = PluginsJailConfig::default();
        assert_eq!(jail.cpu_rate_percent, Some(80));
        assert_eq!(jail.extra_processes, Some(2));
        assert_eq!(jail.memory_mib, None);
    }

    #[test]
    fn plugins_jail_cpu_clamps_to_host_max() {
        let max = host_cpu_rate_max();
        let mut jail = PluginsJailConfig {
            cpu_rate_percent: Some(max.saturating_add(50)),
            ..Default::default()
        };
        jail.clamp();
        assert_eq!(jail.cpu_rate_percent, Some(max));
        jail.cpu_rate_percent = Some(0);
        jail.clamp();
        assert_eq!(jail.cpu_rate_percent, Some(1));
    }
}
