//! Host egress policy for workerd guests (wraps shared [`EgressPolicy`]).

use bookclerk_plugin_manifest::{EgressPolicy, NetworkMode, PluginManifest};

/// Initial-host allowlist with redirect-follow semantics.
///
/// Thin wrapper kept for call sites; core matching lives in
/// [`bookclerk_plugin_manifest::EgressPolicy`].
#[derive(Debug, Clone)]
pub struct EgressProxy {
    inner: EgressPolicy,
}

impl EgressProxy {
    #[must_use]
    pub fn from_manifest(manifest: &PluginManifest) -> Self {
        Self {
            inner: EgressPolicy::from_manifest(manifest),
        }
    }

    #[must_use]
    pub fn from_policy(inner: EgressPolicy) -> Self {
        Self { inner }
    }

    #[must_use]
    pub fn policy(&self) -> &EgressPolicy {
        &self.inner
    }

    /// Network mode from the plugin manifest.
    #[must_use]
    pub fn mode(&self) -> NetworkMode {
        self.inner.mode()
    }

    /// Maximum redirect hops after an allowed initial request.
    #[must_use]
    pub fn max_redirects(&self) -> u32 {
        self.inner.max_redirects()
    }

    #[must_use]
    pub fn allowed_initial_hosts(&self) -> &[String] {
        self.inner.domains()
    }

    /// Whether a *direct* (non-redirect) request host is permitted.
    #[must_use]
    pub fn allows_initial_host(&self, host: &str) -> bool {
        self.inner.allows_initial(host)
    }

    /// Redirect hops are allowed without re-checking the domain allowlist.
    #[must_use]
    pub fn allows_redirect_hop(&self, host: &str, hop_index: u32) -> bool {
        self.inner.allows_redirect(host, hop_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_and_redirect_policy() {
        let proxy = EgressProxy::from_policy(EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["api.example.com".into(), "*.cdn.example.com".into()],
            max_redirects: 5,
            subrequests: None,
        });
        assert!(proxy.allows_initial_host("api.example.com"));
        assert!(proxy.allows_initial_host("a.cdn.example.com"));
        assert!(!proxy.allows_initial_host("evil.com"));
        assert!(proxy.allows_redirect_hop("evil.com", 1));
        assert!(!proxy.allows_redirect_hop("evil.com", 5));
    }
}
