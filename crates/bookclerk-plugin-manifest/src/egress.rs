//! Shared host/egress allowlist policy (workerd JS bridge + native mediator).
//!
//! Semantics match `bookclerk-workerd` bridge `egress.js` and the product rules
//! in `docs/plugins.md`:
//!
//! - **Initial** request hosts must match [`EgressPolicy::domains`] (`*.`
//!   prefix wildcards). Hosts and patterns are IDNA ToASCII–normalized;
//!   percent-encoded hosts fail closed.
//! - **Redirect** hops after an allowed initial request do **not** re-check
//!   domains (intentional — operators approve the initial allowlist only).
//! - Hop count is capped by [`EgressPolicy::max_redirects`] (wire
//!   `maxRedirects`).
//! - **Python + outbound** workerd guests also receive
//!   [`PYODIDE_EGRESS_HOSTS`] in consent and egress lists.
//!
//! Native guests never get hostname filtering; see [`crate::NetworkMode`] and
//! [`crate::PluginManifest::validate`].

use serde::{Deserialize, Serialize};

use crate::types::{
    NetworkCapabilities, NetworkMode, PluginManifest, PluginRuntimeKind, WorkerdRuntimeManifest,
};

/// Default redirect budget injected when building policy from network caps.
///
/// Matches workerd policy injection (`maxRedirects = 10`). Wire JSON uses
/// camelCase `maxRedirects`.
pub const DEFAULT_MAX_REDIRECTS: u32 = 10;

/// Hosts Pyodide / Cloudflare Python Workers commonly fetch on first boot
/// (runtime index + micropip).
///
/// Included in consent UI lists and [`EgressPolicy`] domain allowlists when a
/// workerd guest is Python **and** [`NetworkMode::Outbound`]. See Cloudflare
/// Python packages docs. Values:
/// `cdn.jsdelivr.net`, `pypi.org`, `files.pythonhosted.org`.
pub const PYODIDE_EGRESS_HOSTS: &[&str] =
    &["cdn.jsdelivr.net", "pypi.org", "files.pythonhosted.org"];

/// Initial-host allowlist with redirect-follow and optional subrequest budget.
///
/// Serialized as camelCase for injection into workerd `EGRESS_POLICY` and the
/// native helper (`mode`, `domains`, `maxRedirects`, optional `subrequests`).
///
/// # Matching rules
///
/// - [`Self::allows_initial`] — hostname must match an allowlist entry after
///   IDNA normalization (`*.example.com` or exact host).
/// - [`Self::allows_redirect`] — after an allowed initial fetch, hops are
///   permitted solely by index `< max_redirects` (domain not re-checked).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EgressPolicy {
    /// Network mode (`deny` or `outbound`). Wire name: `mode`.
    pub mode: NetworkMode,

    /// IDNA-normalized initial-request host patterns (`*.cdn.example.com` or
    /// exact hosts). Empty when mode is deny. Wire name: `domains`.
    pub domains: Vec<String>,

    /// Maximum redirect hops after an allowed initial request (0-based hop
    /// index must be `< max_redirects`). Wire name: `maxRedirects`. Defaults
    /// to [`DEFAULT_MAX_REDIRECTS`].
    #[serde(default = "default_max_redirects")]
    pub max_redirects: u32,

    /// Max outbound `fetch` calls (initial request plus redirect hops that
    /// actually fetch).
    ///
    /// When present and finite, the workerd egress bridge enforces this
    /// counter **per egress invocation**. Absent means unlimited (tests /
    /// native mediator). Workerd guests always inject the clamped
    /// `[workerd].limits.subrequests` value via [`Self::from_manifest`].
    /// Wire name: `subrequests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subrequests: Option<u32>,
}

/// Serde default for [`EgressPolicy::max_redirects`] when the wire field is omitted (`10`).
fn default_max_redirects() -> u32 {
    DEFAULT_MAX_REDIRECTS
}

impl EgressPolicy {
    /// Builds a policy from network capabilities, normalizing domains.
    ///
    /// Invalid allowlist entries cause a fail-closed [`Self::deny`] policy
    /// (prefer [`Self::try_from_network`] when you need the error).
    ///
    /// # Arguments
    ///
    /// * `caps` - `[capabilities.network]` table from the manifest.
    ///
    /// # Returns
    ///
    /// An outbound or deny policy with normalized domains and
    /// [`DEFAULT_MAX_REDIRECTS`]. `subrequests` is unset.
    #[must_use]
    pub fn from_network(caps: &NetworkCapabilities) -> Self {
        Self::try_from_network(caps).unwrap_or_else(|_| Self::deny())
    }

    /// Like [`Self::from_network`] but returns an error when any allowlist
    /// entry fails IDNA / percent-encoding checks.
    ///
    /// # Arguments
    ///
    /// * `caps` - `[capabilities.network]` table from the manifest.
    ///
    /// # Returns
    ///
    /// Policy with deduplicated, IDNA-normalized domain patterns.
    ///
    /// # Errors
    ///
    /// Returns a string describing the first invalid domain pattern.
    pub fn try_from_network(caps: &NetworkCapabilities) -> Result<Self, String> {
        let domains = normalize_domain_list(&caps.domains)?;
        Ok(Self {
            mode: caps.mode,
            domains,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            subrequests: None,
        })
    }

    /// Builds policy from a full plugin manifest.
    ///
    /// Appends [`PYODIDE_EGRESS_HOSTS`] for Python + outbound workerd guests
    /// and sets `subrequests` from clamped `[workerd].limits` when present.
    /// Invalid allowlist entries fail closed to [`Self::deny`].
    ///
    /// # Arguments
    ///
    /// * `manifest` - Validated (or at least deserialized) plugin descriptor.
    ///
    /// # Returns
    ///
    /// Egress policy ready for workerd injection or consent materialization.
    #[must_use]
    pub fn from_manifest(manifest: &PluginManifest) -> Self {
        Self::try_from_manifest(manifest).unwrap_or_else(|_| Self::deny())
    }

    /// Like [`Self::from_manifest`] but fails closed on invalid allowlist entries.
    ///
    /// # Arguments
    ///
    /// * `manifest` - Plugin descriptor providing network caps and optional
    ///   `[workerd]` limits.
    ///
    /// # Returns
    ///
    /// Policy with Python runtime hosts (when applicable) and clamped
    /// subrequest budget when `[workerd]` is present.
    ///
    /// # Errors
    ///
    /// Returns a string describing the first invalid domain in
    /// `capabilities.network.domains`.
    pub fn try_from_manifest(manifest: &PluginManifest) -> Result<Self, String> {
        let mut policy = Self::try_from_network(&manifest.capabilities.network)?;
        policy.domains = with_python_runtime_hosts(
            manifest_needs_python(manifest),
            policy.mode,
            &policy.domains,
        );
        if let Some(workerd) = manifest.workerd.as_ref() {
            policy.subrequests = Some(workerd.limits.effective().subrequests);
        }
        Ok(policy)
    }

    /// Attaches a clamped subrequest budget (typically from
    /// [`crate::WorkerdLimits::effective`]).
    ///
    /// # Arguments
    ///
    /// * `subrequests` - Maximum fetch count for the workerd egress bridge.
    ///
    /// # Returns
    ///
    /// `self` with `subrequests` set to `Some(subrequests)`.
    #[must_use]
    pub fn with_subrequests(mut self, subrequests: u32) -> Self {
        self.subrequests = Some(subrequests);
        self
    }

    /// Returns a deny-all policy (no domains, default redirect budget, no
    /// subrequest cap).
    ///
    /// Used when allowlist normalization fails (`from_*` helpers) or when
    /// network mode is deny.
    #[must_use]
    pub fn deny() -> Self {
        Self {
            mode: NetworkMode::Deny,
            domains: Vec::new(),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            subrequests: None,
        }
    }

    /// Returns the configured [`NetworkMode`].
    #[must_use]
    pub fn mode(&self) -> NetworkMode {
        self.mode
    }

    /// Returns the IDNA-normalized domain allowlist (may include Pyodide hosts).
    #[must_use]
    pub fn domains(&self) -> &[String] {
        &self.domains
    }

    /// Returns the redirect hop budget (`maxRedirects` on the wire).
    #[must_use]
    pub fn max_redirects(&self) -> u32 {
        self.max_redirects
    }

    /// Returns whether a *direct* (non-redirect) request host is permitted.
    ///
    /// Deny mode always returns `false`. Outbound mode normalizes `host` via
    /// [`normalize_hostname`] and matches against [`Self::domains`].
    ///
    /// # Arguments
    ///
    /// * `host` - Request hostname (may include a trailing dot; Unicode or
    ///   Punycode). Percent-encoded hosts never match.
    ///
    /// # Returns
    ///
    /// `true` when mode is outbound and the host matches an allowlist entry.
    #[must_use]
    pub fn allows_initial(&self, host: &str) -> bool {
        match self.mode {
            NetworkMode::Deny => false,
            NetworkMode::Outbound => {
                let Some(host) = normalize_hostname(host) else {
                    return false;
                };
                self.domains
                    .iter()
                    .any(|d| host_matches_normalized(&host, d))
            }
        }
    }

    /// Returns whether a redirect hop is allowed without re-checking domains.
    ///
    /// Deny mode always returns `false`. Outbound mode allows hops while
    /// `hop_index < max_redirects`. The `host` argument is intentionally
    /// unused — redirect targets are not re-allowlisted.
    ///
    /// # Arguments
    ///
    /// * `_host` - Redirect target hostname (ignored by design).
    /// * `hop_index` - Zero-based redirect hop index after the initial request.
    ///
    /// # Returns
    ///
    /// `true` when mode is outbound and the hop is within budget.
    #[must_use]
    pub fn allows_redirect(&self, _host: &str, hop_index: u32) -> bool {
        match self.mode {
            NetworkMode::Deny => false,
            NetworkMode::Outbound => hop_index < self.max_redirects,
        }
    }

    /// Serializes this policy to the JSON value injected as workerd
    /// `EGRESS_POLICY` (and the native helper wire form).
    ///
    /// On unlikely serialize failure, returns a deny-shaped object with
    /// `maxRedirects` set to [`DEFAULT_MAX_REDIRECTS`].
    ///
    /// # Returns
    ///
    /// CamelCase JSON (`mode`, `domains`, `maxRedirects`, optional
    /// `subrequests`).
    #[must_use]
    pub fn to_policy_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| {
            serde_json::json!({
                "mode": "deny",
                "domains": [],
                "maxRedirects": DEFAULT_MAX_REDIRECTS,
            })
        })
    }
}

/// Returns `true` when the manifest declares a Python workerd guest.
///
/// Detection (any of):
/// - `[workerd].main_module` ends with `.py`
/// - `compatibility_flags` contains `python_workers`
/// - any `[[modules]]` entry has `type = "python"` or a `.py` name/path
///
/// Native runtimes always return `false`.
///
/// # Arguments
///
/// * `manifest` - Plugin descriptor to inspect.
///
/// # Returns
///
/// `true` when Pyodide CDN hosts should be merged into consent/egress lists
/// for outbound guests.
#[must_use]
pub fn manifest_needs_python(manifest: &PluginManifest) -> bool {
    if manifest.runtime != PluginRuntimeKind::Workerd {
        return false;
    }
    if let Some(w) = manifest.workerd.as_ref() {
        if workerd_declares_python(w) {
            return true;
        }
    }
    manifest.modules.iter().any(|m| {
        m.module_type.eq_ignore_ascii_case("python")
            || m.name.to_ascii_lowercase().ends_with(".py")
            || m.path.to_ascii_lowercase().ends_with(".py")
    })
}

/// True when the workerd runtime names a `.py` main module or `python_workers` flag.
fn workerd_declares_python(w: &WorkerdRuntimeManifest) -> bool {
    w.main_module.to_ascii_lowercase().ends_with(".py")
        || w.compatibility_flags.iter().any(|f| f == "python_workers")
}

/// Appends [`PYODIDE_EGRESS_HOSTS`] when `needs_python` and mode is outbound.
///
/// Existing entries that already match a Pyodide host (case-insensitive) are
/// not duplicated.
///
/// # Arguments
///
/// * `needs_python` - Typically from [`manifest_needs_python`].
/// * `mode` - Network mode; hosts are only appended for
///   [`NetworkMode::Outbound`].
/// * `base` - Already-normalized author allowlist.
///
/// # Returns
///
/// A new `Vec` with base domains plus any missing Pyodide hosts.
#[must_use]
pub fn with_python_runtime_hosts(
    needs_python: bool,
    mode: NetworkMode,
    base: &[String],
) -> Vec<String> {
    let mut domains = base.to_vec();
    if needs_python && mode == NetworkMode::Outbound {
        for host in PYODIDE_EGRESS_HOSTS {
            if !domains.iter().any(|d| d.eq_ignore_ascii_case(host)) {
                domains.push((*host).to_string());
            }
        }
    }
    domains
}

/// Builds the consent / egress domain list for a manifest.
///
/// Normalizes `capabilities.network.domains` via IDNA and appends Pyodide
/// hosts when [`manifest_needs_python`] and mode is outbound.
///
/// # Arguments
///
/// * `manifest` - Plugin descriptor providing network caps and runtime hints.
///
/// # Returns
///
/// Deduplicated, IDNA-normalized host patterns for operator consent UI and
/// materialization.
///
/// # Errors
///
/// Returns a string when any declared domain fails
/// [`normalize_domain_pattern`].
pub fn consent_domains_for(manifest: &PluginManifest) -> Result<Vec<String>, String> {
    let base = normalize_domain_list(&manifest.capabilities.network.domains)?;
    Ok(with_python_runtime_hosts(
        manifest_needs_python(manifest),
        manifest.capabilities.network.mode,
        &base,
    ))
}

/// IDNA-normalizes and deduplicates allowlist patterns; any invalid entry fails the list.
///
/// # Errors
///
/// Returns a string when any domain fails [`normalize_domain_pattern`].
fn normalize_domain_list(domains: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(domains.len());
    for d in domains {
        let n = normalize_domain_pattern(d)
            .ok_or_else(|| format!("invalid network domain `{d}` (IDNA/percent-encoding)"))?;
        if !out.iter().any(|e| e == &n) {
            out.push(n);
        }
    }
    Ok(out)
}

/// Normalizes a hostname with IDNA ToASCII (fail closed).
///
/// Trims ASCII whitespace and a single trailing dot. Rejects empty strings
/// and any host containing `%` (percent-encoding bypass).
///
/// # Arguments
///
/// * `host` - Raw request or allowlist hostname.
///
/// # Returns
///
/// ASCII/Punycode hostname, or `None` when IDNA fails or the input is
/// percent-encoded / empty.
#[must_use]
pub fn normalize_hostname(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() || host.contains('%') {
        return None;
    }
    idna::domain_to_ascii_cow(host.as_bytes(), idna::AsciiDenyList::URL)
        .ok()
        .map(|c| c.into_owned())
}

/// Normalizes an allowlist pattern (`*.example.com` or exact host) via IDNA.
///
/// The `*.` prefix is preserved; only the suffix is ToASCII-normalized.
/// Percent-encoded patterns and empty patterns return `None`.
///
/// # Arguments
///
/// * `pattern` - Author-supplied domain entry from
///   `capabilities.network.domains`.
///
/// # Returns
///
/// Normalized pattern suitable for storage on [`EgressPolicy::domains`], or
/// `None` on failure.
#[must_use]
pub fn normalize_domain_pattern(pattern: &str) -> Option<String> {
    let pattern = pattern.trim().trim_end_matches('.');
    if pattern.is_empty() || pattern.contains('%') {
        return None;
    }
    if let Some(rest) = pattern.strip_prefix("*.") {
        let ascii = normalize_hostname(rest)?;
        Some(format!("*.{ascii}"))
    } else {
        normalize_hostname(pattern)
    }
}

/// True when an IDNA-normalized request host is covered by an allowlist pattern.
///
/// Both sides are IDNA-normalized first; invalid hosts or patterns never
/// match. Wildcard patterns (`*.cdn.example.com`) match the bare suffix and
/// any subdomain (`cdn.example.com`, `a.cdn.example.com`) but not
/// `notcdn.example.com`.
///
/// # Arguments
///
/// * `host` - Request hostname to test.
/// * `pattern` - Exact host or `*.` prefix wildcard pattern.
///
/// # Returns
///
/// `true` when both normalize successfully and the host is covered by the
/// pattern.
///
/// # Examples
///
/// ```
/// use bookclerk_plugin_manifest::host_matches;
///
/// assert!(host_matches("API.Example.COM", "api.example.com"));
/// assert!(host_matches("www.cdn.example.com", "*.cdn.example.com"));
/// assert!(!host_matches("evil.com", "api.example.com"));
/// ```
#[must_use]
pub fn host_matches(host: &str, pattern: &str) -> bool {
    let (Some(host), Some(pattern)) = (normalize_hostname(host), normalize_domain_pattern(pattern))
    else {
        return false;
    };
    host_matches_normalized(&host, &pattern)
}

/// Exact or `*.suffix` match on already-normalized host and pattern strings.
fn host_matches_normalized(host: &str, pattern: &str) -> bool {
    // `host` and policy `domains` are already IDNA-normalized by callers
    // (`allows_initial` / `try_from_network`). Do not re-run IDNA here.
    if let Some(rest) = pattern.strip_prefix("*.") {
        host == rest || host.ends_with(&format!(".{rest}"))
    } else {
        host == pattern
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_and_redirect_policy() {
        let policy = EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["api.example.com".into(), "*.cdn.example.com".into()],
            max_redirects: 5,
            subrequests: None,
        };
        assert!(policy.allows_initial("api.example.com"));
        assert!(policy.allows_initial("a.cdn.example.com"));
        assert!(policy.allows_initial("cdn.example.com"));
        assert!(!policy.allows_initial("evil.com"));
        // Redirect hops stay free (intentional — no per-hop re-allowlist).
        assert!(policy.allows_redirect("evil.com", 1));
        assert!(!policy.allows_redirect("evil.com", 5));
        assert!(!EgressPolicy::deny().allows_initial("api.example.com"));
    }

    #[test]
    fn host_matches_exact_and_wildcard() {
        assert!(host_matches("API.Example.COM", "api.example.com"));
        assert!(host_matches("www.cdn.example.com.", "*.cdn.example.com"));
        assert!(!host_matches("evil.example.com", "api.example.com"));
        assert!(!host_matches("notcdn.example.com", "*.cdn.example.com"));
    }

    #[test]
    fn idna_unicode_and_punycode_forms_match() {
        // bücher.de → xn--bcher-kva.de
        assert!(host_matches("bücher.de", "xn--bcher-kva.de"));
        assert!(host_matches("xn--bcher-kva.de", "bücher.de"));
        assert!(host_matches("WWW.bücher.de.", "*.xn--bcher-kva.de"));
        let policy = EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["xn--bcher-kva.de".into()],
            max_redirects: 10,
            subrequests: None,
        };
        assert!(policy.allows_initial("bücher.de"));
        assert!(policy.allows_initial("xn--bcher-kva.de"));
    }

    #[test]
    fn percent_encoded_host_denied() {
        assert!(normalize_hostname("evil%2ecom").is_none());
        assert!(normalize_hostname("%65vil.com").is_none());
        assert!(!host_matches("evil%2ecom", "evil.com"));
        let policy = EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["evil.com".into()],
            max_redirects: 10,
            subrequests: None,
        };
        assert!(!policy.allows_initial("evil%2ecom"));
        assert!(normalize_domain_pattern("api%2eexample.com").is_none());
    }

    #[test]
    fn trailing_dot_and_case_still_match() {
        assert!(host_matches("API.Example.COM.", "api.example.com"));
        assert!(host_matches("api.example.com", "API.Example.COM."));
        let policy = EgressPolicy::try_from_network(&NetworkCapabilities {
            mode: NetworkMode::Outbound,
            domains: vec!["API.Example.COM.".into()],
        })
        .unwrap();
        assert_eq!(policy.domains, vec!["api.example.com".to_string()]);
        assert!(policy.allows_initial("api.example.com."));
    }

    #[test]
    fn invalid_allowlist_fails_closed_at_policy_build() {
        let err = EgressPolicy::try_from_network(&NetworkCapabilities {
            mode: NetworkMode::Outbound,
            domains: vec!["ok.example.com".into(), "bad%2eexample.com".into()],
        })
        .expect_err("percent-encoded pattern");
        assert!(err.contains("bad%2eexample.com"), "{err}");
        let denied = EgressPolicy::from_network(&NetworkCapabilities {
            mode: NetworkMode::Outbound,
            domains: vec!["bad%2eexample.com".into()],
        });
        assert_eq!(denied.mode, NetworkMode::Deny);
        assert!(!denied.allows_initial("bad.example.com"));
    }

    #[test]
    fn policy_json_shape() {
        let policy = EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["libro.fm".into()],
            max_redirects: 10,
            subrequests: None,
        };
        let v = policy.to_policy_json();
        assert_eq!(v["mode"], "outbound");
        assert_eq!(v["maxRedirects"], 10);
        assert_eq!(v["domains"][0], "libro.fm");
        assert!(v.get("subrequests").is_none());
    }

    #[test]
    fn policy_json_includes_subrequests_when_set() {
        let policy = EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["api.example.com".into()],
            max_redirects: 10,
            subrequests: Some(50),
        };
        let v = policy.to_policy_json();
        assert_eq!(v["subrequests"], 50);
    }

    #[test]
    fn workerd_manifest_injects_clamped_subrequests() {
        let toml = r#"
api_version = 2
id = "limits_demo"
kind = "integration"
runtime = "workerd"
[workerd]
compatibility_date = "2026-08-01"
main_module = "plugin.js"
[workerd.limits]
cpu_ms = 30000
subrequests = 50
[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]
"#;
        let manifest = PluginManifest::parse(toml).unwrap();
        let policy = EgressPolicy::from_manifest(&manifest);
        assert_eq!(policy.subrequests, Some(50));
        let over = r#"
api_version = 2
id = "limits_over"
kind = "integration"
runtime = "workerd"
[workerd]
compatibility_date = "2026-08-01"
main_module = "plugin.js"
[workerd.limits]
subrequests = 99999
[capabilities.network]
mode = "deny"
"#;
        let over_manifest = PluginManifest::parse(over).unwrap();
        let over_policy = EgressPolicy::from_manifest(&over_manifest);
        assert_eq!(
            over_policy.subrequests,
            Some(crate::WorkerdLimits::MAX_SUBREQUESTS)
        );
    }

    #[test]
    fn python_outbound_includes_pyodide_hosts() {
        let toml = r#"
api_version = 2
id = "py_demo"
kind = "integration"
runtime = "workerd"
[workerd]
compatibility_date = "2026-08-01"
compatibility_flags = ["python_workers"]
main_module = "plugin.py"
[[modules]]
name = "plugin.py"
path = "plugin.py"
type = "python"
[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]
"#;
        let manifest = PluginManifest::parse(toml).unwrap();
        assert!(manifest_needs_python(&manifest));
        let domains = consent_domains_for(&manifest).unwrap();
        assert!(domains.iter().any(|d| d == "api.example.com"));
        for host in PYODIDE_EGRESS_HOSTS {
            assert!(
                domains.iter().any(|d| d == *host),
                "missing Pyodide host {host} in {domains:?}"
            );
        }
        let policy = EgressPolicy::from_manifest(&manifest);
        assert!(policy.allows_initial("cdn.jsdelivr.net"));
        assert!(policy.allows_redirect("evil.com", 1));
    }

    #[test]
    fn js_outbound_omits_pyodide_hosts() {
        let toml = r#"
api_version = 2
id = "js_demo"
kind = "integration"
runtime = "workerd"
[workerd]
compatibility_date = "2026-08-01"
main_module = "plugin.js"
[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]
"#;
        let manifest = PluginManifest::parse(toml).unwrap();
        assert!(!manifest_needs_python(&manifest));
        let domains = consent_domains_for(&manifest).unwrap();
        assert_eq!(domains, vec!["api.example.com".to_string()]);
    }
}
