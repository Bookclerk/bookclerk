//! `plugin.toml` types.

use std::path::PathBuf;

use bookclerk_plugin_abi::CliSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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

/// Guest runtime selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PluginRuntimeKind {
    /// OS binary speaking the Workers RPC ABI.
    #[default]
    Native,
    /// Bookclerk-shipped `bookclerk-workerd` + author modules.
    Workerd,
}

/// Network mode declared in `[capabilities.network]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// No outbound network.
    #[default]
    Deny,
    /// Coarse jail outbound (native) or workerd isolate egress.
    ///
    /// For **workerd**, pair with `domains` (isolate hostname allowlist).
    /// For **native**, do **not** set `domains` — the OS jail cannot filter by
    /// hostname; outbound means open internet (plus oauth listen when bound).
    Outbound,
}

/// `[capabilities.network]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkCapabilities {
    pub mode: NetworkMode,
    /// Workerd-only: initial-request host allowlist for isolate egress.
    /// Must be empty for `runtime = "native"` (rejected by [`PluginManifest::validate`]).
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

fn is_false(v: &bool) -> bool {
    !*v
}

/// `[capabilities.bindings]` — host stubs the guest expects.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BindingCapabilities {
    #[serde(skip_serializing_if = "is_false")]
    pub config: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub secrets: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub plugin_kv: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub work_fs: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub oauth: bool,
}

/// `[capabilities.methods]` — declared RPC surface for discovery/consent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MethodCapabilities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub list: Vec<String>,
}

/// Full `[capabilities]` table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesManifest {
    pub network: NetworkCapabilities,
    #[serde(default, skip_serializing_if = "BindingCapabilities::is_default")]
    pub bindings: BindingCapabilities,
    #[serde(default, skip_serializing_if = "MethodCapabilities::is_default")]
    pub methods: MethodCapabilities,
}

impl BindingCapabilities {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl MethodCapabilities {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `[workerd]` — WorkerCode-equivalent fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerdRuntimeManifest {
    pub compatibility_date: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compatibility_flags: Vec<String>,
    pub main_module: String,
    #[serde(default = "default_modules_dir")]
    pub modules_dir: String,
    #[serde(default = "default_entrypoint")]
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "WorkerdLimits::is_default")]
    pub limits: WorkerdLimits,
}

fn default_modules_dir() -> String {
    "modules".into()
}

fn default_entrypoint() -> String {
    "default".into()
}

/// Optional workerd resource limits (host clamps).
///
/// Local workerd does **not** Cap'n Proto-emit `cpuMs` / `subRequests`. Bookclerk
/// clamps these values, injects `subrequests` into egress policy JSON, and maps
/// `cpu_ms` onto jail Spec CPU rate (plus memory/process ceilings) for OS
/// enforcement.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerdLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subrequests: Option<u32>,
}

/// Concrete limits after applying host defaults and hard caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveWorkerdLimits {
    pub cpu_ms: u32,
    pub subrequests: u32,
}

impl WorkerdLimits {
    /// Default CPU budget when unset or `0` (matches Echo examples).
    pub const DEFAULT_CPU_MS: u32 = 30_000;
    /// Default outbound fetch budget when unset or `0` (matches Echo examples).
    pub const DEFAULT_SUBREQUESTS: u32 = 50;
    /// Hard host cap for CPU budget (ms).
    pub const MAX_CPU_MS: u32 = 120_000;
    /// Hard host cap for outbound fetch budget.
    pub const MAX_SUBREQUESTS: u32 = 1_000;

    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Resolve concrete limits: unset/`0` → defaults, then clamp to hard caps.
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

fn clamp_limit(raw: Option<u32>, default: u32, max: u32) -> u32 {
    match raw {
        None | Some(0) => default,
        Some(n) => n.min(max),
    }
}

/// One module in `[[modules]]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleSpec {
    pub name: String,
    pub path: String,
    #[serde(default = "default_module_type")]
    #[serde(rename = "type")]
    pub module_type: String,
}

fn default_module_type() -> String {
    "js".into()
}

/// On-disk plugin descriptor (`plugin.toml`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// ABI version (`1`).
    pub api_version: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: PluginKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Settings / UI logo: `https://…` URL or relative image path under the plugin root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(default)]
    pub runtime: PluginRuntimeKind,
    /// Native executable (required when `runtime = native`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Workerd isolate config (required when `runtime = workerd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workerd: Option<WorkerdRuntimeManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<ModuleSpec>,
    pub capabilities: CapabilitiesManifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliSchema>,
}

impl PluginManifest {
    /// Domains used for workerd network consent UI (IDNA-normalized; includes
    /// Pyodide CDN hosts when this is a Python + outbound guest).
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

    /// Validated logo classification, if `logo` is set.
    pub fn logo_kind(&self) -> Result<Option<crate::LogoKind>> {
        match self.logo.as_deref() {
            None => Ok(None),
            Some(raw) => Ok(Some(crate::validate_logo(raw)?)),
        }
    }

    /// Parse and validate manifest TOML.
    pub fn parse(text: &str) -> Result<Self> {
        let m: Self = toml::from_str(text)?;
        m.validate()?;
        Ok(m)
    }

    /// Semantic validation after deserialize.
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(Error::message("plugin.toml: `id` is required"));
        }
        // Validate the raw id (non-lossy): do not trim before grammar checks.
        crate::validate_plugin_id(&self.id)
            .map_err(|e| Error::message(format!("plugin.toml: {e}")))?;
        if self.api_version != 1 {
            return Err(Error::message("plugin.toml: `api_version` must be 1"));
        }
        if let Some(logo) = self.logo.as_deref() {
            let _ = crate::validate_logo(logo)?;
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
        Ok(())
    }

    /// Resolve the process to spawn (native command or bookclerk-workerd).
    #[must_use]
    pub fn spawn_command(&self) -> Option<&PathBuf> {
        match self.runtime {
            PluginRuntimeKind::Native => self.command.as_ref(),
            PluginRuntimeKind::Workerd => None, // host resolves bookclerk-workerd
        }
    }

    /// Map manifest network + oauth binding to jail network policy.
    ///
    /// Native guests get coarse jail outbound (`Outbound`) when
    /// `mode = "outbound"`; **hostname allowlists are not supported** on
    /// native (see `domains` / workerd). With `bindings.oauth`, native guests
    /// also need loopback listen for the host OAuth callback tunnel (`Listen`).
    /// Workerd always needs loopback listen/connect to its Cloudflare child
    /// (domain policy is enforced inside the isolate when `domains` are set).
    #[must_use]
    pub fn jail_network_need(&self) -> JailNetworkNeed {
        if self.runtime == PluginRuntimeKind::Workerd {
            return JailNetworkNeed::Listen;
        }
        match self.capabilities.network.mode {
            NetworkMode::Deny => JailNetworkNeed::None,
            NetworkMode::Outbound if self.capabilities.bindings.oauth => JailNetworkNeed::Listen,
            NetworkMode::Outbound => JailNetworkNeed::Outbound,
        }
    }
}

/// Map manifest network + oauth binding to jail network policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JailNetworkNeed {
    /// No IP sockets (`mode = "deny"`).
    None,
    /// Native `outbound` without OAuth — coarse jail outbound.
    Outbound,
    /// Native `outbound` + `bindings.oauth`, or any workerd guest (loopback
    /// listen/connect to the Cloudflare child / OAuth callback).
    Listen,
}

#[cfg(test)]
mod tests {
    use super::*;

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
api_version = 1
id = "echo"
kind = "integration"
version = "1.0.0"
runtime = "workerd"

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        assert_eq!(m.runtime, PluginRuntimeKind::Workerd);
        assert_eq!(m.capabilities.network.mode, NetworkMode::Deny);
    }

    #[test]
    fn workerd_outbound_requires_domains() {
        let err = PluginManifest::parse(
            r#"
api_version = 1
id = "xx"
kind = "source"
runtime = "workerd"
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
    fn native_outbound_forbids_domains() {
        let err = PluginManifest::parse(
            r#"
api_version = 1
id = "audible"
kind = "source"
runtime = "native"
command = "./bookclerk-plugin-source-audible"
[capabilities.network]
mode = "outbound"
domains = ["api.audible.com", "www.amazon.com"]
[capabilities.bindings]
oauth = true
secrets = true
"#,
        )
        .expect_err("domains forbidden on native");
        assert!(err.to_string().contains("only valid for runtime"), "{err}");
    }

    #[test]
    fn native_outbound_without_domains_ok() {
        let m = PluginManifest::parse(
            r#"
api_version = 1
id = "audible"
kind = "source"
runtime = "native"
command = "./bookclerk-plugin-source-audible"
[capabilities.network]
mode = "outbound"
[capabilities.bindings]
oauth = true
secrets = true
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
    }

    #[test]
    fn workerd_outbound_with_domains_ok() {
        let m = PluginManifest::parse(
            r#"
api_version = 1
id = "echo"
kind = "integration"
runtime = "workerd"
[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]
[capabilities.bindings]
config = true
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
api_version = 1
id = "audible"
kind = "source"
runtime = "native"
command = "./bin"
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
api_version = 1
id = "echo"
kind = "integration"
runtime = "native"
command = "./bin"
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
api_version = 1
id = "echo"
kind = "integration"
runtime = "native"
command = "./bin"
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
api_version = 1
id = "{padded}"
kind = "integration"
runtime = "native"
command = "./bin"
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
    fn workerd_limits_effective_defaults_and_caps() {
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
            .effective(),
            EffectiveWorkerdLimits {
                cpu_ms: WorkerdLimits::DEFAULT_CPU_MS,
                subrequests: WorkerdLimits::DEFAULT_SUBREQUESTS,
            }
        );
        let capped = WorkerdLimits {
            cpu_ms: Some(500_000),
            subrequests: Some(9_999),
        }
        .effective();
        assert_eq!(capped.cpu_ms, WorkerdLimits::MAX_CPU_MS);
        assert_eq!(capped.subrequests, WorkerdLimits::MAX_SUBREQUESTS);
        let mid = WorkerdLimits {
            cpu_ms: Some(12_000),
            subrequests: Some(10),
        }
        .effective();
        assert_eq!(mid.cpu_ms, 12_000);
        assert_eq!(mid.subrequests, 10);
    }
}
