//! Operator consent grant overrides delivered to `bookclerk-workerd` via env.
//!
//! The plugin host injects these after resolving the effective grant at spawn.
//! Values always clamp to host [`WorkerdLimits`] caps and never widen beyond
//! the manifest-derived egress policy / limits.

use bookclerk_plugin_manifest::{
    manifest_needs_python, normalize_domain_pattern, with_python_runtime_hosts,
    EffectiveWorkerdLimits, EgressPolicy, NetworkMode, PluginManifest, WorkerdLimits,
};

use crate::egress::EgressProxy;

/// Env: network mode override (`deny` or `outbound`).
///
/// Must stay aligned with `bookclerk_plugin_host::consent::WORKERD_GRANT_*_ENV`.
pub const GRANT_NETWORK_MODE_ENV: &str = "BOOKCLERK_WORKERD_GRANT_NETWORK_MODE";
/// Env: comma-separated initial-host domain patterns (grant subset).
pub const GRANT_DOMAINS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_DOMAINS";
/// Env: workerd CPU budget override in milliseconds.
pub const GRANT_CPU_MS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_CPU_MS";
/// Env: workerd subrequest budget override.
pub const GRANT_SUBREQUESTS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_SUBREQUESTS";

/// Optional operator grant fields parsed from the process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatorGrantEnv {
    /// Stricter-or-equal network mode from the stored grant.
    pub network_mode: Option<NetworkMode>,
    /// Domain allowlist subset (`Some` even when empty).
    pub domains: Option<Vec<String>>,
    /// CPU budget override (milliseconds).
    pub cpu_ms: Option<u32>,
    /// Subrequest budget override.
    pub subrequests: Option<u32>,
}

impl OperatorGrantEnv {
    /// Reads grant override env vars set by the plugin host at spawn.
    ///
    /// Missing vars leave the corresponding field unset (manifest policy wins).
    /// Invalid mode / non-numeric limits are ignored (fail open to manifest).
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            network_mode: std::env::var(GRANT_NETWORK_MODE_ENV)
                .ok()
                .as_deref()
                .and_then(parse_network_mode),
            domains: std::env::var(GRANT_DOMAINS_ENV).ok().map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            }),
            cpu_ms: parse_u32_env(GRANT_CPU_MS_ENV),
            subrequests: parse_u32_env(GRANT_SUBREQUESTS_ENV),
        }
    }

    /// Returns true when no grant override env vars were present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.network_mode.is_none()
            && self.domains.is_none()
            && self.cpu_ms.is_none()
            && self.subrequests.is_none()
    }

    /// Narrows a manifest-derived egress proxy to the operator grant.
    ///
    /// Domain patterns are IDNA-normalized; invalid entries are dropped.
    /// Python + outbound guests still receive Pyodide CDN hosts.
    #[must_use]
    pub fn apply_egress(&self, manifest: &PluginManifest, egress: EgressProxy) -> EgressProxy {
        let mut policy = egress.policy().clone();

        if let Some(mode) = self.network_mode {
            if mode == NetworkMode::Deny {
                let mut deny = EgressPolicy::deny();
                if let Some(sub) = self.clamped_subrequests() {
                    deny.subrequests = Some(match policy.subrequests {
                        Some(existing) => existing.min(sub),
                        None => sub,
                    });
                } else if let Some(existing) = policy.subrequests {
                    deny.subrequests = Some(existing);
                }
                return EgressProxy::from_policy(deny);
            }
            policy.mode = mode;
        }

        if let Some(ref domains) = self.domains {
            let normalized: Vec<String> = domains
                .iter()
                .filter_map(|d| normalize_domain_pattern(d))
                .collect();
            policy.domains = with_python_runtime_hosts(
                manifest_needs_python(manifest),
                policy.mode,
                &normalized,
            );
        }

        if let Some(sub) = self.clamped_subrequests() {
            policy.subrequests = Some(match policy.subrequests {
                Some(existing) => existing.min(sub),
                None => sub,
            });
        }

        EgressProxy::from_policy(policy)
    }

    /// Narrows effective workerd limits to the operator grant (host-capped).
    ///
    /// When a grant override is present it is authoritative within
    /// [`WorkerdLimits`] hard maxes and may be **higher** than the manifest
    /// defaults.
    #[must_use]
    pub fn apply_limits(&self, mut limits: EffectiveWorkerdLimits) -> EffectiveWorkerdLimits {
        if let Some(cpu) = self.clamped_cpu_ms() {
            limits.cpu_ms = cpu;
        }
        if let Some(sub) = self.clamped_subrequests() {
            limits.subrequests = sub;
        }
        limits
    }

    /// Grant CPU budget in milliseconds, clamped to [`WorkerdLimits`] hard maxes.
    fn clamped_cpu_ms(&self) -> Option<u32> {
        self.cpu_ms.map(|cpu_ms| {
            WorkerdLimits {
                cpu_ms: Some(cpu_ms),
                subrequests: None,
            }
            .effective()
            .cpu_ms
        })
    }

    /// Grant subrequest budget, clamped to [`WorkerdLimits`] hard maxes.
    fn clamped_subrequests(&self) -> Option<u32> {
        self.subrequests.map(|subrequests| {
            WorkerdLimits {
                cpu_ms: None,
                subrequests: Some(subrequests),
            }
            .effective()
            .subrequests
        })
    }
}

/// Parses `deny` or `outbound`; any other string is ignored (manifest policy wins).
fn parse_network_mode(raw: &str) -> Option<NetworkMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "deny" => Some(NetworkMode::Deny),
        "outbound" => Some(NetworkMode::Outbound),
        _ => None,
    }
}

/// Parses a non-empty env var as `u32`; missing or non-numeric values are ignored.
fn parse_u32_env(key: &str) -> Option<u32> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_manifest::parse;

    fn sample_manifest(domains: &[&str]) -> PluginManifest {
        let domain_lines = domains
            .iter()
            .map(|d| format!("  \"{d}\""))
            .collect::<Vec<_>>()
            .join(",\n");
        parse(&format!(
            r#"
api_version = 2
id = "echo"
kind = "integration"
runtime = "workerd"

[workerd]
compatibility_date = "2024-09-23"
main_module = "plugin.js"
modules_dir = "modules"

[workerd.limits]
cpu_ms = 30000
subrequests = 50

[capabilities.network]
mode = "outbound"
domains = [
{domain_lines}
]
"#
        ))
        .expect("valid test manifest")
    }

    #[test]
    fn apply_egress_narrows_domains_and_subrequests() {
        let manifest = sample_manifest(&["api.example.com", "cdn.example.com"]);
        let base = EgressProxy::from_manifest(&manifest);
        let grant = OperatorGrantEnv {
            network_mode: Some(NetworkMode::Outbound),
            domains: Some(vec!["api.example.com".into()]),
            cpu_ms: Some(15_000),
            subrequests: Some(10),
        };
        let narrowed = grant.apply_egress(&manifest, base);
        assert_eq!(narrowed.mode(), NetworkMode::Outbound);
        assert_eq!(
            narrowed.allowed_initial_hosts(),
            &["api.example.com".to_string()]
        );
        assert_eq!(narrowed.policy().subrequests, Some(10));
        let limits = grant.apply_limits(manifest.workerd.as_ref().unwrap().limits.effective());
        assert_eq!(limits.cpu_ms, 15_000);
        assert_eq!(limits.subrequests, 10);
    }

    #[test]
    fn apply_egress_deny_clears_domains() {
        let manifest = sample_manifest(&["api.example.com"]);
        let base = EgressProxy::from_manifest(&manifest);
        let grant = OperatorGrantEnv {
            network_mode: Some(NetworkMode::Deny),
            domains: Some(vec![]),
            cpu_ms: None,
            subrequests: Some(5),
        };
        let narrowed = grant.apply_egress(&manifest, base);
        assert_eq!(narrowed.mode(), NetworkMode::Deny);
        assert!(narrowed.allowed_initial_hosts().is_empty());
        assert_eq!(narrowed.policy().subrequests, Some(5));
    }

    #[test]
    fn apply_limits_honors_grant_widen_within_host_max() {
        let grant = OperatorGrantEnv {
            network_mode: None,
            domains: None,
            cpu_ms: Some(60_000),
            subrequests: Some(200),
        };
        let limits = EffectiveWorkerdLimits {
            cpu_ms: 10_000,
            subrequests: 20,
        };
        let applied = grant.apply_limits(limits);
        assert_eq!(applied.cpu_ms, 60_000);
        assert_eq!(applied.subrequests, 200);
    }

    #[test]
    fn apply_limits_never_exceeds_host_max() {
        let grant = OperatorGrantEnv {
            network_mode: None,
            domains: None,
            cpu_ms: Some(WorkerdLimits::MAX_CPU_MS + 50),
            subrequests: Some(WorkerdLimits::MAX_SUBREQUESTS + 50),
        };
        let limits = EffectiveWorkerdLimits {
            cpu_ms: 10_000,
            subrequests: 20,
        };
        let applied = grant.apply_limits(limits);
        assert_eq!(applied.cpu_ms, WorkerdLimits::MAX_CPU_MS);
        assert_eq!(applied.subrequests, WorkerdLimits::MAX_SUBREQUESTS);
    }
}
