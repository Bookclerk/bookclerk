//! Plugin-style `[sources.*]` and `[integrations.*]` configuration.
//!
//! Source plugins are opaque TOML tables under `[sources.<id>]`. Host code
//! never names store-specific knobs — each content-source crate parses its
//! own table at registration time. Integrations remain typed for first-party
//! ABS; additional ids land in opaque tables.
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

fn parse_bool_loose(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Optional third-party integrations under `[integrations]`.
///
/// First-party Audiobookshelf stays typed; additional `[integrations.<id>]`
/// tables (including dynamically discovered plugins) land in [`Self::plugins`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IntegrationsConfig {
    pub portal_base_path: String,
    pub claim_ticket_ttl_hours: u64,
    pub public_origin: Option<String>,
    pub portal_session_ttl_hours: u64,
    pub audiobookshelf: AudiobookshelfConfig,
    /// Opaque tables for external / discovered integration plugins.
    #[serde(flatten)]
    pub plugins: BTreeMap<String, toml::Value>,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self {
            portal_base_path: "/connect".into(),
            claim_ticket_ttl_hours: 72,
            public_origin: None,
            portal_session_ttl_hours: 12,
            audiobookshelf: AudiobookshelfConfig::default(),
            plugins: BTreeMap::new(),
        }
    }
}

impl IntegrationsConfig {
    /// Whether an integration plugin id is enabled.
    ///
    /// Built-in ABS uses its typed flag. External plugins default to **disabled**
    /// unless `[integrations.<id>] enabled = true`.
    #[must_use]
    pub fn is_enabled(&self, integration: &str) -> bool {
        match integration.trim().to_ascii_lowercase().as_str() {
            "audiobookshelf" | "abs" => self.audiobookshelf.enabled,
            other => self
                .plugin_table(other)
                .and_then(|t| t.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }

    /// Borrow an external plugin table when present.
    #[must_use]
    pub fn plugin_table(&self, id: &str) -> Option<&toml::Table> {
        self.plugins.get(id)?.as_table()
    }

    /// Mutable external plugin table (creates empty table if needed).
    pub fn plugin_table_mut(&mut self, id: &str) -> &mut toml::Table {
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
}

/// Audiobookshelf integration settings (`[integrations.audiobookshelf]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AudiobookshelfConfig {
    pub enabled: bool,
    pub base_url: String,
    pub api_key: Option<String>,
    pub library_id: Option<String>,
    pub watch_users: bool,
    pub notify_scan_on_liberate: bool,
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
            notify_scan_on_liberate: true,
            allow_credential_login: true,
        }
    }
}
