//! `plugin.toml` schema — install-time metadata shipped with a plugin.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which Bookclerk surface a plugin implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Source,
    Integration,
    Output,
}

impl PluginKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Integration => "integration",
            Self::Output => "output",
        }
    }
}

/// On-disk plugin descriptor (`plugin.toml`).
///
/// Installed by the plugin (or its installer) under a search root. User settings
/// live in the main `config.toml` under `[sources.<id>]` / `[integrations.<id>]`
/// and are passed at handshake — not stored here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Protocol version this plugin speaks (`1` today).
    pub api_version: u32,
    /// Stable plugin id (must match `[sources.<id>]` / `[integrations.<id>]`).
    pub id: String,
    /// Human-facing name (fallback if handshake omits `display_name`).
    #[serde(default)]
    pub name: Option<String>,
    /// Plugin kind.
    pub kind: PluginKind,
    /// Executable to spawn (absolute, or relative to the manifest directory).
    pub command: PathBuf,
    /// Extra argv after `command`.
    #[serde(default)]
    pub args: Vec<String>,
}

impl PluginManifest {
    /// Parse manifest TOML text.
    pub fn parse(text: &str) -> crate::Result<Self> {
        let m: Self = toml::from_str(text)?;
        if m.id.trim().is_empty() {
            return Err(crate::PluginError::message("plugin.toml: `id` is required"));
        }
        if m.api_version == 0 {
            return Err(crate::PluginError::message(
                "plugin.toml: `api_version` must be >= 1",
            ));
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_integration() {
        let m = PluginManifest::parse(
            r#"
api_version = 1
id = "echo"
kind = "integration"
command = "./echo-integration"
"#,
        )
        .unwrap();
        assert_eq!(m.id, "echo");
        assert_eq!(m.kind, PluginKind::Integration);
        assert!(m.args.is_empty());
    }
}
