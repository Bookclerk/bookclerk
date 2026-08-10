//! Shared host/egress allowlist policy (workerd JS + native mediator).
//!
//! Semantics match `bookclerk-workerd` bridge `egress.js`:
//! - **initial** request hosts must match `domains` (`*` prefix wildcards)
//! - **redirect** hops after an allowed initial request do not re-check domains
//! - hop count is capped by `max_redirects`

use serde::{Deserialize, Serialize};

use crate::types::{NetworkCapabilities, NetworkMode, PluginManifest};

/// Default redirect budget (matches workerd policy injection).
pub const DEFAULT_MAX_REDIRECTS: u32 = 10;

/// Initial-host allowlist with redirect-follow semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EgressPolicy {
    pub mode: NetworkMode,
    pub domains: Vec<String>,
    #[serde(default = "default_max_redirects")]
    pub max_redirects: u32,
}

fn default_max_redirects() -> u32 {
    DEFAULT_MAX_REDIRECTS
}

impl EgressPolicy {
    #[must_use]
    pub fn from_network(caps: &NetworkCapabilities) -> Self {
        Self {
            mode: caps.mode,
            domains: caps.domains.clone(),
            max_redirects: DEFAULT_MAX_REDIRECTS,
        }
    }

    #[must_use]
    pub fn from_manifest(manifest: &PluginManifest) -> Self {
        Self::from_network(&manifest.capabilities.network)
    }

    #[must_use]
    pub fn deny() -> Self {
        Self {
            mode: NetworkMode::Deny,
            domains: Vec::new(),
            max_redirects: DEFAULT_MAX_REDIRECTS,
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
            NetworkMode::Outbound => self.domains.iter().any(|d| host_matches(host, d)),
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

/// Hostname / allowlist pattern match (`*.example.com` or exact).
#[must_use]
pub fn host_matches(host: &str, pattern: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
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
        };
        assert!(policy.allows_initial("api.example.com"));
        assert!(policy.allows_initial("a.cdn.example.com"));
        assert!(policy.allows_initial("cdn.example.com"));
        assert!(!policy.allows_initial("evil.com"));
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
    fn policy_json_shape() {
        let policy = EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["libro.fm".into()],
            max_redirects: 10,
        };
        let v = policy.to_policy_json();
        assert_eq!(v["mode"], "outbound");
        assert_eq!(v["maxRedirects"], 10);
        assert_eq!(v["domains"][0], "libro.fm");
    }
}
