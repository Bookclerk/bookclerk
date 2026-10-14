//! Plugin host (`[plugins]`) and portal (`[integrations]`) configuration.
//!
//! Plugin enablement and settings do not live here. A plugin is enabled by a
//! `plugin_deployments` row for a host, and its settings are the plugin
//! instance document validated against the `[[settings]]` the plugin
//! declares in `plugin.toml` (`bookclerk plugins deploy|config`, the
//! Settings page). `[integrations]` keeps only the portal knobs. See
//! `docs/plugins.md`.

use serde::{Deserialize, Serialize};

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
    /// When true, community packages without independent publisher authenticity
    /// may be installed without `--allow-unverified-publisher` (digests are still
    /// required). Bookclerk does not verify publisher signatures.
    #[serde(default)]
    pub allow_unverified_publisher: bool,
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
pub fn node_logical_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1)
}

/// Host CPU rate ceiling in one-core percent units (`logical_cpus × 100`).
#[must_use]
pub fn host_cpu_rate_max() -> u32 {
    node_logical_cpus().saturating_mul(100)
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
        if let Ok(value) = std::env::var("BOOKCLERK_PLUGIN_ALLOW_UNVERIFIED_PUBLISHER") {
            if let Some(b) = parse_bool_loose(&value) {
                self.allow_unverified_publisher = b;
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

/// Connect-portal settings under `[integrations]`.
///
/// Integration plugins themselves (Audiobookshelf, …) are deployed plugin
/// instances; their settings live on the instance, not here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IntegrationsConfig {
    /// Hours a connect-portal claim ticket remains valid (default 72).
    pub claim_ticket_ttl_hours: u64,
    /// Public HTTPS origin for portal redirects when behind a reverse proxy
    /// (e.g. `https://bookclerk.example.com`). `None` accepts only a bound
    /// loopback `Origin` / `Host`; non-loopback hosts must set this pin.
    pub public_origin: Option<String>,
    /// Hours a portal browser session cookie remains valid (default 12).
    pub portal_session_ttl_hours: u64,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self {
            claim_ticket_ttl_hours: 72,
            public_origin: None,
            portal_session_ttl_hours: 12,
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
