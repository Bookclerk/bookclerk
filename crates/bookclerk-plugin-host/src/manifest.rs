//! `plugin.toml` schema — install-time metadata shipped with a plugin.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::CliSchema;

/// Which Bookclerk surface a plugin implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Source,
    Integration,
    Output,
    Database,
}

impl PluginKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Integration => "integration",
            Self::Output => "output",
            Self::Database => "database",
        }
    }
}

/// What a plugin needs from the network.
///
/// A plugin declares this; the host grants it. Nothing here can widen the
/// filesystem allowlist, which the host derives on its own — a manifest ships
/// with the plugin, so anything it can ask for is something a hostile plugin can
/// ask for too. Unrestricted access is deliberately not expressible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkNeed {
    /// No network at all. For a plugin that only transforms what it is handed.
    None,
    /// Outbound connections only. The default, and all a storefront needs to
    /// call an API and download a file.
    #[default]
    Outbound,
    /// Outbound plus a local callback listener on a kernel-assigned port.
    ///
    /// Only for an interactive OAuth login that receives its authorization code
    /// over loopback, which is how Audible's sign-in works.
    Listen,
}

/// `[sandbox]` — what a plugin needs from its jail.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxManifest {
    /// Network reachability this plugin needs.
    pub network: NetworkNeed,
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
    /// Wire framing name. Absent means [`bookclerk_plugin_sdk::PROTOCOL_NAME`].
    #[serde(default)]
    pub protocol: Option<String>,
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
    /// Optional CLI schema for help without spawning (handshake / `cli.describe` win at invoke).
    #[serde(default)]
    pub cli: Option<CliSchema>,
    /// What this plugin needs from its jail. Omitted means outbound-only.
    #[serde(default)]
    pub sandbox: SandboxManifest,
    /// Public site / API URLs this plugin talks to.
    ///
    /// Informational (UI brand / docs) — not a sandbox host allowlist. The first
    /// parseable URL's host is used to build a Google favicon URL for Settings.
    #[serde(default)]
    pub outbound_urls: Vec<String>,
}

impl PluginManifest {
    /// Google `s2/favicons` URL derived from the first usable [`Self::outbound_urls`] entry.
    #[must_use]
    pub fn google_favicon_url(&self) -> Option<String> {
        self.outbound_urls
            .iter()
            .find_map(|raw| host_from_outbound_url(raw))
            .map(|host| format!("https://www.google.com/s2/favicons?domain={host}&sz=128"))
    }

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
        if let Some(protocol) = m.protocol.as_deref() {
            if protocol != bookclerk_plugin_sdk::PROTOCOL_NAME {
                return Err(crate::PluginError::message(format!(
                    "plugin.toml: unsupported `protocol` {protocol:?}; only {:?} is supported",
                    bookclerk_plugin_sdk::PROTOCOL_NAME
                )));
            }
        }
        Ok(m)
    }
}

/// Extract a hostname suitable for Google's favicon service.
fn host_from_outbound_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let rest = with_scheme.split_once("://")?.1;
    let host_port = rest.split(['/', '?', '#']).next()?.trim();
    if host_port.is_empty() {
        return None;
    }
    let host = host_port
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(host_port);
    // Drop brackets / port from `[::1]:443` or `example.com:443`.
    let host = if let Some(inner) = host.strip_prefix('[') {
        inner.split(']').next().unwrap_or(inner)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    let host = host.trim().trim_start_matches("www.").to_ascii_lowercase();
    if host.is_empty() || host.contains(' ') {
        return None;
    }
    Some(host)
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
        assert!(m.cli.is_none());
        assert!(m.protocol.is_none());
        // A manifest that says nothing about the network gets the narrowest
        // grant a storefront can actually work with.
        assert_eq!(m.sandbox.network, NetworkNeed::Outbound);
    }

    #[test]
    fn unsupported_protocol_is_rejected() {
        let err = PluginManifest::parse(
            r#"
api_version = 1
id = "echo"
kind = "integration"
command = "./echo-integration"
protocol = "something-else"
"#,
        )
        .expect_err("unsupported protocol must fail");
        assert!(
            err.to_string().contains("unsupported `protocol`"),
            "got: {err}"
        );
    }

    #[test]
    fn known_protocol_is_accepted() {
        let m = PluginManifest::parse(
            r#"
api_version = 1
id = "echo"
kind = "integration"
command = "./echo-integration"
protocol = "jsonrpc-stdio-v1"
"#,
        )
        .unwrap();
        assert_eq!(m.protocol.as_deref(), Some("jsonrpc-stdio-v1"));
    }

    #[test]
    fn parse_sandbox_network_need() {
        let m = PluginManifest::parse(
            r#"
api_version = 1
id = "audible"
kind = "source"
command = "./bookclerk-plugin-source-audible"

[sandbox]
network = "listen"
"#,
        )
        .unwrap();
        assert_eq!(m.sandbox.network, NetworkNeed::Listen);
    }

    /// A typo in a security-relevant field must not read as a default.
    #[test]
    fn unknown_sandbox_keys_are_rejected() {
        let err = PluginManifest::parse(
            r#"
api_version = 1
id = "sneaky"
kind = "source"
command = "./x"

[sandbox]
netwrok = "listen"
"#,
        )
        .expect_err("unknown key must fail");
        assert!(err.to_string().contains("netwrok"), "got: {err}");
    }

    #[test]
    fn unknown_network_values_are_rejected() {
        let err = PluginManifest::parse(
            r#"
api_version = 1
id = "sneaky"
kind = "source"
command = "./x"

[sandbox]
network = "full"
"#,
        )
        .expect_err("`full` must not be expressible from a manifest");
        assert!(err.to_string().contains("full"), "got: {err}");
    }

    #[test]
    fn outbound_urls_drive_google_favicon() {
        let m = PluginManifest::parse(
            r#"
api_version = 1
id = "audible"
kind = "source"
command = "./x"
outbound_urls = ["https://www.audible.com/", "https://api.audible.com"]
"#,
        )
        .unwrap();
        assert_eq!(
            m.google_favicon_url().as_deref(),
            Some("https://www.google.com/s2/favicons?domain=audible.com&sz=128")
        );
    }

    #[test]
    fn host_from_outbound_url_accepts_bare_hosts() {
        assert_eq!(
            host_from_outbound_url("chirpbooks.com"),
            Some("chirpbooks.com".into())
        );
        assert_eq!(host_from_outbound_url(""), None);
    }

    #[test]
    fn parse_cli_schema() {
        let m = PluginManifest::parse(
            r#"
api_version = 1
id = "echo"
kind = "integration"
command = "./echo-integration"

[cli]
[[cli.commands]]
name = "ping"
about = "Probe echo plugin"
[[cli.commands.args]]
name = "message"
long = "message"
kind = "string"
default = "hi"
"#,
        )
        .unwrap();
        let cli = m.cli.expect("cli");
        assert_eq!(cli.commands.len(), 1);
        assert_eq!(cli.commands[0].name, "ping");
        assert_eq!(cli.commands[0].args.len(), 1);
        assert_eq!(cli.commands[0].args[0].name, "message");
    }
}
