//! Shared host/egress allowlist policy (workerd JS + native mediator).
//!
//! Semantics match `bookclerk-workerd` bridge `egress.js`:
//! - **initial** request hosts must match `domains` (`*` prefix wildcards)
//! - hosts and patterns are IDNA ToASCII–normalized; percent-encoded hosts fail closed
//! - **redirect** hops after an allowed initial request do not re-check domains
//!   (intentional — redirect hops stay free)
//! - hop count is capped by `max_redirects`

use serde::{Deserialize, Serialize};

use crate::types::{
    NetworkCapabilities, NetworkMode, PluginManifest, PluginRuntimeKind, WorkerdRuntimeManifest,
};

/// Default redirect budget (matches workerd policy injection).
pub const DEFAULT_MAX_REDIRECTS: u32 = 10;

/// Hosts Pyodide / Cloudflare Python Workers commonly fetch on first boot
/// (runtime index + micropip). Included in consent + egress when a workerd
/// guest is Python + `outbound`. See Cloudflare Python packages docs.
pub const PYODIDE_EGRESS_HOSTS: &[&str] =
    &["cdn.jsdelivr.net", "pypi.org", "files.pythonhosted.org"];

/// Initial-host allowlist with redirect-follow semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EgressPolicy {
    pub mode: NetworkMode,
    pub domains: Vec<String>,
    #[serde(default = "default_max_redirects")]
    pub max_redirects: u32,
    /// Max outbound `fetch` calls (initial + redirect hops that actually fetch).
    ///
    /// When present and finite, the workerd egress bridge enforces this counter.
    /// Absent means unlimited (tests / native mediator). Workerd guests always
    /// inject the clamped `[workerd].limits.subrequests` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subrequests: Option<u32>,
}

fn default_max_redirects() -> u32 {
    DEFAULT_MAX_REDIRECTS
}

impl EgressPolicy {
    /// Build a policy from network capabilities, normalizing domains (fail closed → deny).
    #[must_use]
    pub fn from_network(caps: &NetworkCapabilities) -> Self {
        Self::try_from_network(caps).unwrap_or_else(|_| Self::deny())
    }

    /// Like [`from_network`] but returns an error when any allowlist entry is invalid.
    pub fn try_from_network(caps: &NetworkCapabilities) -> Result<Self, String> {
        let domains = normalize_domain_list(&caps.domains)?;
        Ok(Self {
            mode: caps.mode,
            domains,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            subrequests: None,
        })
    }

    /// Policy from a plugin manifest, including Pyodide hosts for Python+outbound
    /// and clamped `[workerd].limits.subrequests` when a workerd table is present.
    #[must_use]
    pub fn from_manifest(manifest: &PluginManifest) -> Self {
        Self::try_from_manifest(manifest).unwrap_or_else(|_| Self::deny())
    }

    /// Like [`from_manifest`] but fails closed on invalid allowlist entries.
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

    /// Attach a clamped subrequest budget (typically from [`crate::WorkerdLimits::effective`]).
    #[must_use]
    pub fn with_subrequests(mut self, subrequests: u32) -> Self {
        self.subrequests = Some(subrequests);
        self
    }

    #[must_use]
    pub fn deny() -> Self {
        Self {
            mode: NetworkMode::Deny,
            domains: Vec::new(),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            subrequests: None,
        }
    }

    #[must_use]
    pub fn mode(&self) -> NetworkMode {
        self.mode
    }

    #[must_use]
    pub fn domains(&self) -> &[String] {
        &self.domains
    }

    #[must_use]
    pub fn max_redirects(&self) -> u32 {
        self.max_redirects
    }

    /// Whether a *direct* (non-redirect) request host is permitted.
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

    /// Redirect hops are allowed without re-checking the domain allowlist.
    #[must_use]
    pub fn allows_redirect(&self, _host: &str, hop_index: u32) -> bool {
        match self.mode {
            NetworkMode::Deny => false,
            NetworkMode::Outbound => hop_index < self.max_redirects,
        }
    }

    /// Wire form injected into workerd `EGRESS_POLICY` and the native helper.
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

/// True when the manifest declares a Python workerd guest (main module, modules,
/// or `python_workers` compatibility flag).
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

fn workerd_declares_python(w: &WorkerdRuntimeManifest) -> bool {
    w.main_module.to_ascii_lowercase().ends_with(".py")
        || w.compatibility_flags.iter().any(|f| f == "python_workers")
}

/// Append Pyodide CDN hosts when `needs_python` and mode is outbound (deduped).
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

/// Consent / egress domain list for a manifest (normalized + Pyodide when needed).
pub fn consent_domains_for(manifest: &PluginManifest) -> Result<Vec<String>, String> {
    let base = normalize_domain_list(&manifest.capabilities.network.domains)?;
    Ok(with_python_runtime_hosts(
        manifest_needs_python(manifest),
        manifest.capabilities.network.mode,
        &base,
    ))
}

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

/// IDNA ToASCII hostname. Rejects percent-encoding and failed IDNA (fail closed).
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

/// Normalize an allowlist pattern (`*.example.com` or exact host) via IDNA.
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

/// Hostname / allowlist pattern match (`*.example.com` or exact).
///
/// Both sides are IDNA-normalized; invalid hosts/patterns never match.
#[must_use]
pub fn host_matches(host: &str, pattern: &str) -> bool {
    let (Some(host), Some(pattern)) = (normalize_hostname(host), normalize_domain_pattern(pattern))
    else {
        return false;
    };
    host_matches_normalized(&host, &pattern)
}

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
api_version = 1
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
api_version = 1
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
api_version = 1
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
api_version = 1
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
