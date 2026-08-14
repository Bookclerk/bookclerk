//! `plugin.toml` types: manifest root and nested capability / workerd tables.
//!
//! These structs are the serde projection of the install descriptor. Unknown
//! keys are rejected (`deny_unknown_fields`) so typos fail at parse time.
//! Semantic rules that cannot be expressed in serde alone live in
//! [`PluginManifest::validate`]. Product narrative: `docs/plugins.md`.

use std::path::PathBuf;

use bookclerk_plugin_abi::CliSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Which Bookclerk surface a plugin implements.
///
/// Wire values are lowercase (`source`, `integration`, `output`, `database`).
/// Ids are globally unique across kinds — two plugins cannot share an `id`
/// even if their kinds differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Storefront / library source (scan, acquire, catalog).
    Source,
    /// Side integration (e.g. Audiobookshelf sync, Connect).
    Integration,
    /// Destination / output backend (local filesystem, S3, …).
    Output,
    /// Library database backend (sqlite, postgres, D1, …).
    Database,
}

impl PluginKind {
    /// Returns the lowercase wire name used in TOML and API paths.
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

/// Serde skip predicate: omit a binding flag from TOML when it is `false`.
fn is_false(v: &bool) -> bool {
    !*v
}

/// `[capabilities.bindings]` — host stubs the guest expects at spawn.
///
/// Each flag is omitted from TOML when `false`. Enabling a binding does not
/// grant consent by itself; the operator must still approve network /
/// privileged delivery as documented in `docs/plugins.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BindingCapabilities {
    /// Guest may read plugin config delivered by the host.
    #[serde(skip_serializing_if = "is_false")]
    pub config: bool,
    /// Guest may read sealed secrets / credentials via host bindings.
    #[serde(skip_serializing_if = "is_false")]
    pub secrets: bool,
    /// Guest may use per-plugin key/value storage.
    #[serde(skip_serializing_if = "is_false")]
    pub plugin_kv: bool,
    /// Guest may use the host work filesystem side-channel.
    #[serde(skip_serializing_if = "is_false")]
    pub work_fs: bool,
    /// Guest needs an OAuth-style callback tunnel (host owns the listener).
    ///
    /// With native outbound, this upgrades jail network need to
    /// [`JailNetworkNeed::Listen`].
    #[serde(skip_serializing_if = "is_false")]
    pub oauth: bool,
}

/// `[capabilities.methods]` — declared RPC surface for discovery / consent.
///
/// Lists method names the guest intends to implement; used for operator UI
/// and tooling, not as a hard ABI gate at handshake.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MethodCapabilities {
    /// Workers RPC method names this guest advertises (camelCase wire names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub list: Vec<String>,
}

/// Full `[capabilities]` table required on every manifest.
///
/// `network` is mandatory in TOML; `bindings` and `methods` default to empty
/// / all-false when omitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesManifest {
    /// Network mode and optional workerd domain allowlist.
    pub network: NetworkCapabilities,
    /// Host binding stubs the guest expects.
    #[serde(default, skip_serializing_if = "BindingCapabilities::is_default")]
    pub bindings: BindingCapabilities,
    /// Declared RPC method names for discovery / consent.
    #[serde(default, skip_serializing_if = "MethodCapabilities::is_default")]
    pub methods: MethodCapabilities,
}

impl BindingCapabilities {
    /// True when every binding flag is off (omit the `[capabilities.bindings]` table).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl MethodCapabilities {
    /// True when no RPC method names are declared (omit `[capabilities.methods]`).
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
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

/// On-disk plugin descriptor (`plugin.toml`).
///
/// Root table for install / discovery. Parse with [`Self::parse`] (deserialize
/// + [`Self::validate`]). Unknown keys are rejected.
///
/// # Validation highlights
///
/// - `api_version` must be `1`
/// - `id` must pass [`crate::validate_plugin_id`]
/// - native requires `command`; workerd requires `[workerd]` with date + main
/// - `domains` forbidden on native; required for workerd + outbound
/// - optional `logo` must pass [`crate::validate_logo`]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// ABI / schema version. Must be `1` (validated).
    pub api_version: u32,
    /// Globally unique plugin id (`[a-z0-9_]{2,32}` grammar).
    pub id: String,
    /// Optional human-readable display name for Settings / Accounts UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Which Bookclerk surface this plugin implements.
    pub kind: PluginKind,
    /// Optional semver (or free-form) package version string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Settings / UI logo: `https://…` URL or relative image path under the
    /// plugin root. Validated by [`crate::validate_logo`] when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// Guest runtime (`native` default, or `workerd`).
    #[serde(default)]
    pub runtime: PluginRuntimeKind,
    /// Native executable path relative to the install root (required when
    /// `runtime = "native"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<PathBuf>,
    /// Extra argv passed after `command` for native guests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Workerd isolate config (required when `runtime = "workerd"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workerd: Option<WorkerdRuntimeManifest>,
    /// Optional module list for workerd packages (`[[modules]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<ModuleSpec>,
    /// Declared network, bindings, and methods capabilities.
    pub capabilities: CapabilitiesManifest,
    /// Optional CLI schema advertised to `bookclerk plugins <id>` (from
    /// `bookclerk-plugin-abi::CliSchema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliSchema>,
}

impl PluginManifest {
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
    /// api_version = 1
    /// id = "echo"
    /// kind = "integration"
    /// runtime = "native"
    /// command = "./echo"
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
            NetworkMode::Outbound if self.capabilities.bindings.oauth => JailNetworkNeed::Listen,
            NetworkMode::Outbound => JailNetworkNeed::Outbound,
        }
    }
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
