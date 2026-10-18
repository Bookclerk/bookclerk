//! Grant algebra for owner plugins on a host node (H-4 design, section 5;
//! step 8).
//!
//! An owner cluster deploys a plugin to a host with the grant the
//! cluster's operator approved (the *request*). The host's own operator
//! approves it before anything runs and may narrow it; the stored approval
//! is the request as reviewed plus the *consent* (`library
//! control_plane::remote_consents`). This module decides:
//!
//! - [`remote_request_within`]: whether a request stays inside what was
//!   reviewed. A new version that asks for anything more (a domain, a
//!   binding, network at all, a larger budget) needs approval again.
//! - [`remote_consent`]: the consent an operator gives, the request minus
//!   what they deny. Nothing outside the request can be added, so a host
//!   can narrow but never widen.
//! - [`narrow_remote_grant`]: the grant the guest runs with, the current
//!   request intersected with the consent. Denied network destinations are
//!   recorded as operator denials, so the spawn path's re-evaluation of
//!   the manifest never brings them back.
//! - [`remote_platform_auto_approval`]: a Bookclerk platform plugin is
//!   approved without asking only when this host's own provenance check
//!   says the owner's artifact is the bundled one and the request stays
//!   inside the platform envelope.
//!
//! Every comparison is strict: an unparsable grant, an unknown network
//! mode, or a missing field reads as wider, never narrower.

use std::collections::BTreeSet;

use crate::consent::{
    consumers_cover, effective_cpu_rate_percent, effective_disk_mib, effective_extra_processes,
    effective_memory_mib, GrantedEventConsumer, PluginGrant,
};
use crate::Result;

/// What a host operator takes out of an owner plugin's request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteNarrowing {
    /// Outbound domain patterns to deny.
    pub deny_domains: BTreeSet<String>,
    /// Host bindings to deny (`secrets`, `oauth`, `kv`, ...).
    pub deny_bindings: BTreeSet<String>,
    /// Turn outbound network off altogether.
    pub deny_network: bool,
}

/// True when `mode` is outbound (anything else is read as deny).
fn outbound(mode: &str) -> bool {
    mode.eq_ignore_ascii_case("outbound")
}

/// True when `wider` asks for a larger budget than `ceiling`, comparing
/// the effective (defaulted, clamped) values.
fn budget_wider(ceiling: &PluginGrant, wider: &PluginGrant) -> bool {
    let unset_or = |value: Option<u32>| value.unwrap_or(u32::MAX);
    effective_memory_mib(wider.memory_mib) > effective_memory_mib(ceiling.memory_mib)
        || effective_disk_mib(wider.disk_mib) > effective_disk_mib(ceiling.disk_mib)
        || unset_or(wider.cpu_ms) > unset_or(ceiling.cpu_ms)
        || unset_or(wider.subrequests) > unset_or(ceiling.subrequests)
        || wider
            .cpu_rate_percent
            .map(|v| effective_cpu_rate_percent(Some(v)))
            > ceiling
                .cpu_rate_percent
                .map(|v| effective_cpu_rate_percent(Some(v)))
        || wider
            .extra_processes
            .map(|v| effective_extra_processes(Some(v)))
            > ceiling
                .extra_processes
                .map(|v| effective_extra_processes(Some(v)))
}

/// True when `requested` asks for nothing outside `reviewed`: the same
/// plugin, every capability set a subset, network no wider (outbound only
/// if reviewed outbound), undeclared redirects only if reviewed, and no
/// larger budget.
#[must_use]
pub fn remote_request_within(reviewed: &PluginGrant, requested: &PluginGrant) -> bool {
    reviewed.plugin_key == requested.plugin_key
        && (!outbound(&requested.network_mode) || outbound(&reviewed.network_mode))
        && requested.domains.is_subset(&reviewed.domains)
        && requested.tcp.is_subset(&reviewed.tcp)
        && requested.address_cidrs.is_subset(&reviewed.address_cidrs)
        && (!requested.allow_undeclared_public_redirects
            || reviewed.allow_undeclared_public_redirects)
        && requested.bindings.is_subset(&reviewed.bindings)
        && requested
            .compatibility_flags
            .is_subset(&reviewed.compatibility_flags)
        && requested.entrypoints.is_subset(&reviewed.entrypoints)
        && requested.producers.is_subset(&reviewed.producers)
        && requested.jobs.is_subset(&reviewed.jobs)
        && consumers_cover(&reviewed.consumers, &requested.consumers)
        && !budget_wider(reviewed, requested)
}

/// The consent an operator gives to `requested` with `narrowing` applied.
///
/// # Errors
///
/// Returns an error when the narrowing names a domain or binding the
/// request does not ask for (it would deny nothing, and a typo must not
/// read as a narrowing that did not happen).
pub fn remote_consent(requested: &PluginGrant, narrowing: &RemoteNarrowing) -> Result<PluginGrant> {
    let unknown_domains: Vec<_> = narrowing
        .deny_domains
        .difference(&requested.domains)
        .cloned()
        .collect();
    if !unknown_domains.is_empty() {
        return Err(crate::PluginError::message(format!(
            "the request does not ask for {}",
            unknown_domains.join(", ")
        )));
    }
    let unknown_bindings: Vec<_> = narrowing
        .deny_bindings
        .difference(&requested.bindings)
        .cloned()
        .collect();
    if !unknown_bindings.is_empty() {
        return Err(crate::PluginError::message(format!(
            "the request does not ask for the binding(s) {}",
            unknown_bindings.join(", ")
        )));
    }
    let mut consent = requested.clone();
    for domain in &narrowing.deny_domains {
        consent.domains.remove(domain);
    }
    for binding in &narrowing.deny_bindings {
        consent.bindings.remove(binding);
    }
    if narrowing.deny_network {
        consent.network_mode = "deny".into();
        consent.domains.clear();
        consent.tcp.clear();
        consent.address_cidrs.clear();
        consent.allow_undeclared_public_redirects = false;
    }
    consent.approved_at = chrono::Utc::now().to_rfc3339();
    Ok(consent)
}

/// `a ∩ b` of two capability sets.
fn both<T: Clone + Ord>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> BTreeSet<T> {
    a.intersection(b).cloned().collect()
}

/// The smaller of two optional budgets, each defaulted by `effective`.
fn min_budget(a: Option<u32>, b: Option<u32>, effective: fn(Option<u32>) -> u32) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        _ => Some(effective(a).min(effective(b))),
    }
}

/// The smaller of two optional limits where unset means the host default
/// (kept unset only when both are).
fn min_limit(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

/// The grant a guest runs with: `requested ∩ consent`.
///
/// Every structural set and network destination is intersected; network
/// is outbound only when both are; undeclared redirects only when both
/// allow; budgets take the smaller. Destinations the request asks for but
/// the result lacks are recorded as operator denials, so the spawn path's
/// manifest re-evaluation ([`crate::effective_grant`]) keeps them out.
#[must_use]
pub fn narrow_remote_grant(requested: &PluginGrant, consent: &PluginGrant) -> PluginGrant {
    let network_mode = if outbound(&requested.network_mode) && outbound(&consent.network_mode) {
        "outbound".to_string()
    } else {
        "deny".to_string()
    };
    let deny = network_mode == "deny";
    let domains = if deny {
        BTreeSet::new()
    } else {
        both(&requested.domains, &consent.domains)
    };
    let tcp = if deny {
        BTreeSet::new()
    } else {
        both(&requested.tcp, &consent.tcp)
    };
    let address_cidrs = if deny {
        BTreeSet::new()
    } else {
        both(&requested.address_cidrs, &consent.address_cidrs)
    };
    let manifest_domains = if requested.manifest_domains.is_empty() {
        requested.domains.clone()
    } else {
        requested.manifest_domains.clone()
    };
    let denied_domains: BTreeSet<String> = manifest_domains
        .union(&requested.domains)
        .chain(requested.operator_denied_domains.iter())
        .filter(|domain| !domains.contains(*domain))
        .cloned()
        .collect();
    let denied_tcp = requested
        .manifest_tcp
        .union(&requested.tcp)
        .chain(requested.operator_denied_tcp.iter())
        .filter(|grant| !tcp.contains(*grant))
        .cloned()
        .collect();
    let denied_cidrs = requested
        .manifest_cidrs
        .union(&requested.address_cidrs)
        .chain(requested.operator_denied_cidrs.iter())
        .filter(|cidr| !address_cidrs.contains(*cidr))
        .cloned()
        .collect();
    let redirects = !deny
        && requested.allow_undeclared_public_redirects
        && consent.allow_undeclared_public_redirects;
    let consumers: BTreeSet<GrantedEventConsumer> = requested
        .consumers
        .iter()
        .filter(|req| consent.consumers.iter().any(|got| got.covers(req)))
        .cloned()
        .collect();
    PluginGrant {
        schema_version: requested.schema_version,
        plugin_key: requested.plugin_key.clone(),
        plugin_id: requested.plugin_id.clone(),
        entrypoints: both(&requested.entrypoints, &consent.entrypoints),
        producers: both(&requested.producers, &consent.producers),
        consumers,
        jobs: both(&requested.jobs, &consent.jobs),
        network_mode,
        operator_added_domains: both(&requested.operator_added_domains, &domains),
        operator_denied_domains: denied_domains,
        domains,
        manifest_domains,
        operator_added_tcp: both(&requested.operator_added_tcp, &tcp),
        operator_denied_tcp: denied_tcp,
        tcp,
        manifest_tcp: requested.manifest_tcp.clone(),
        operator_added_cidrs: both(&requested.operator_added_cidrs, &address_cidrs),
        operator_denied_cidrs: denied_cidrs,
        address_cidrs,
        manifest_cidrs: requested.manifest_cidrs.clone(),
        allow_undeclared_public_redirects: redirects,
        operator_allow_undeclared_public_redirects: Some(redirects),
        bindings: both(&requested.bindings, &consent.bindings),
        compatibility_flags: both(&requested.compatibility_flags, &consent.compatibility_flags),
        cpu_ms: min_limit(requested.cpu_ms, consent.cpu_ms),
        subrequests: min_limit(requested.subrequests, consent.subrequests),
        disk_mib: min_budget(requested.disk_mib, consent.disk_mib, effective_disk_mib),
        memory_mib: min_budget(
            requested.memory_mib,
            consent.memory_mib,
            effective_memory_mib,
        ),
        cpu_rate_percent: min_limit(requested.cpu_rate_percent, consent.cpu_rate_percent),
        extra_processes: min_limit(requested.extra_processes, consent.extra_processes),
        approved_at: consent.approved_at.clone(),
    }
}

/// How this host stands on one owner plugin's request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteConsentState {
    /// Approved and the request is within what was reviewed: the guest
    /// runs with this grant.
    Consented(Box<PluginGrant>),
    /// Never approved (or revoked): waiting for the host's operator.
    Pending,
    /// Approved once, but the request now asks for more than was reviewed.
    NeedsReapproval,
    /// The request or the stored approval does not parse; nothing runs.
    Invalid,
}

impl RemoteConsentState {
    /// The grant to run with, when consented.
    #[must_use]
    pub fn grant(&self) -> Option<&PluginGrant> {
        match self {
            Self::Consented(grant) => Some(grant),
            _ => None,
        }
    }

    /// Stable text for observations and listings.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Consented(_) => "consented",
            Self::Pending => "awaiting_host_consent",
            Self::NeedsReapproval => "awaiting_host_reapproval",
            Self::Invalid => "invalid_grant",
        }
    }
}

/// Parses a grant document, refusing one for another plugin.
fn parse_grant(json: &str, plugin_key: &str) -> Option<PluginGrant> {
    serde_json::from_str::<PluginGrant>(json)
        .ok()
        .filter(|grant| grant.plugin_key == plugin_key)
}

/// Evaluates a stored consent row's grants: the current request
/// (`requested_json`), the request as reviewed and the consent (both
/// `None` while unapproved).
#[must_use]
pub fn remote_consent_state(
    plugin_key: &str,
    requested_json: &str,
    reviewed_json: Option<&str>,
    consent_json: Option<&str>,
) -> RemoteConsentState {
    let Some(requested) = parse_grant(requested_json, plugin_key) else {
        return RemoteConsentState::Invalid;
    };
    let (Some(reviewed), Some(consent)) = (reviewed_json, consent_json) else {
        return RemoteConsentState::Pending;
    };
    let (Some(reviewed), Some(consent)) = (
        parse_grant(reviewed, plugin_key),
        parse_grant(consent, plugin_key),
    ) else {
        return RemoteConsentState::Invalid;
    };
    // A consent wider than what was reviewed is not one this module wrote.
    if !remote_request_within(&reviewed, &consent) {
        return RemoteConsentState::Invalid;
    }
    if !remote_request_within(&reviewed, &requested) {
        return RemoteConsentState::NeedsReapproval;
    }
    RemoteConsentState::Consented(Box::new(narrow_remote_grant(&requested, &consent)))
}

/// True when a platform plugin's request may be approved without asking:
/// the key is a Bookclerk platform key, this host's own install of that key
/// is platform-bundled with the same payload digest as the owner's
/// artifact (`local_bundled_payload`, from this host's provenance check),
/// and the request stays inside the platform envelope (deny network, only
/// `config` / `work_fs`).
#[must_use]
pub fn remote_platform_auto_approval(
    requested: &PluginGrant,
    owner_payload_root_sha256: &str,
    local_bundled_payload: Option<&str>,
) -> bool {
    let Ok(key) = bookclerk_plugin_catalog::PluginKey::parse(&requested.plugin_key) else {
        return false;
    };
    bookclerk_plugin_catalog::is_platform_plugin_key(&key)
        && local_bundled_payload.is_some_and(|local| {
            !owner_payload_root_sha256.is_empty() && local == owner_payload_root_sha256
        })
        && crate::consent::is_platform_envelope(requested)
}

/// Approves the request recorded from `cluster_id` for `plugin_key`, as
/// the host's operator reviewed it: `expected_version`, when given, must be
/// the recorded version, and the consent is the recorded request minus
/// `narrowing` ([`remote_consent`]). The write is a compare-and-swap on the
/// recorded request, so a request that changed after it was read is
/// refused rather than approved unseen.
///
/// # Errors
///
/// Returns an error when nothing is recorded, the version differs, the
/// recorded request does not parse or the narrowing names something it
/// does not ask for, or the store refuses the write (including a request
/// that changed in between).
pub async fn approve_remote_request(
    store: &bookclerk_library::LibraryStore,
    cluster_id: &str,
    plugin_key: &str,
    expected_version: Option<&str>,
    narrowing: &RemoteNarrowing,
    approved_by: &str,
) -> Result<bookclerk_library::control_plane::RemotePluginConsent> {
    use bookclerk_library::control_plane::{
        approve_remote_plugin, load_remote_plugin_consent, RemoteApproval,
    };
    let lib = |err: bookclerk_library::LibraryError| crate::PluginError::message(err.to_string());
    let row = load_remote_plugin_consent(store, cluster_id, plugin_key)
        .await
        .map_err(lib)?
        .ok_or_else(|| {
            crate::PluginError::message(format!(
                "cluster {cluster_id} has not asked to run {plugin_key} here"
            ))
        })?;
    if let Some(expected) = expected_version {
        if expected != row.requested_version {
            return Err(crate::PluginError::message(format!(
                "the cluster asks for version {}, not {expected}",
                row.requested_version
            )));
        }
    }
    let requested = serde_json::from_str::<PluginGrant>(&row.requested_grant_json)
        .ok()
        .filter(|grant| grant.plugin_key == plugin_key)
        .ok_or_else(|| crate::PluginError::message("the recorded request does not parse"))?;
    let consent = remote_consent(&requested, narrowing)?;
    let grant_json = serde_json::to_string(&consent)
        .map_err(|err| crate::PluginError::message(err.to_string()))?;
    approve_remote_plugin(
        store,
        &RemoteApproval {
            cluster_id,
            plugin_key,
            version: &row.requested_version,
            reviewed_request_json: &row.requested_grant_json,
            grant_json: &grant_json,
            approved_by,
        },
    )
    .await
    .map_err(lib)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_manifest::TcpGrant;

    fn requested() -> PluginGrant {
        PluginGrant {
            plugin_key: "path:file:///tmp/store#store".into(),
            plugin_id: "store".into(),
            entrypoints: ["storefront".to_string()].into(),
            network_mode: "outbound".into(),
            domains: [
                "api.store.example".to_string(),
                "cdn.store.example".to_string(),
            ]
            .into(),
            manifest_domains: [
                "api.store.example".to_string(),
                "cdn.store.example".to_string(),
            ]
            .into(),
            tcp: [TcpGrant {
                host: "db.store.example".into(),
                ports: vec![5432],
            }]
            .into(),
            bindings: ["config".to_string(), "secrets".to_string()].into(),
            memory_mib: Some(256),
            ..PluginGrant::empty()
        }
    }

    fn json(grant: &PluginGrant) -> String {
        serde_json::to_string(grant).unwrap()
    }

    #[test]
    fn a_host_narrows_but_never_widens() {
        let request = requested();
        let consent = remote_consent(
            &request,
            &RemoteNarrowing {
                deny_domains: ["cdn.store.example".to_string()].into(),
                ..RemoteNarrowing::default()
            },
        )
        .unwrap();
        let effective = narrow_remote_grant(&request, &consent);
        assert_eq!(effective.domains, ["api.store.example".to_string()].into());
        assert!(effective
            .operator_denied_domains
            .contains("cdn.store.example"));
        assert!(remote_request_within(&request, &effective));
        // The spawn path re-evaluates the manifest; the denial holds.
        let respawn = crate::effective_grant(&effective, &request);
        assert!(
            !respawn.domains.contains("cdn.store.example"),
            "{respawn:?}"
        );

        // A consent cannot add what the request lacks.
        let mut wider = consent.clone();
        wider.domains.insert("evil.example".into());
        wider.bindings.insert("oauth".into());
        let effective = narrow_remote_grant(&request, &wider);
        assert!(!effective.domains.contains("evil.example"));
        assert!(!effective.bindings.contains("oauth"));
        assert!(matches!(
            remote_consent_state(
                &request.plugin_key,
                &json(&request),
                Some(&json(&request)),
                Some(&json(&wider))
            ),
            RemoteConsentState::Invalid
        ));

        // A narrowing that names nothing requested is refused.
        assert!(remote_consent(
            &request,
            &RemoteNarrowing {
                deny_domains: ["typo.example".to_string()].into(),
                ..RemoteNarrowing::default()
            }
        )
        .is_err());

        // Network off clears every destination.
        let off = remote_consent(
            &request,
            &RemoteNarrowing {
                deny_network: true,
                deny_bindings: ["secrets".to_string()].into(),
                ..RemoteNarrowing::default()
            },
        )
        .unwrap();
        let effective = narrow_remote_grant(&request, &off);
        assert_eq!(effective.network_mode, "deny");
        assert!(effective.domains.is_empty() && effective.tcp.is_empty());
        assert!(!effective.bindings.contains("secrets"));
        assert_eq!(
            crate::effective_grant(&effective, &request)
                .egress_policy()
                .mode,
            bookclerk_plugin_manifest::NetworkMode::Deny
        );
    }

    #[test]
    fn a_wider_version_needs_approval_again() {
        let reviewed = requested();
        let consent = remote_consent(&reviewed, &RemoteNarrowing::default()).unwrap();
        let state = |request: &PluginGrant| {
            remote_consent_state(
                &reviewed.plugin_key,
                &json(request),
                Some(&json(&reviewed)),
                Some(&json(&consent)),
            )
        };
        assert!(state(&reviewed).grant().is_some());

        // Narrower or equal: carried forward.
        let mut narrower = reviewed.clone();
        narrower.domains.remove("cdn.store.example");
        let carried = state(&narrower);
        assert_eq!(
            carried.grant().unwrap().domains,
            ["api.store.example".to_string()].into()
        );

        let mut wider_domain = reviewed.clone();
        wider_domain.domains.insert("tracker.example".into());
        let mut wider_binding = reviewed.clone();
        wider_binding.bindings.insert("oauth".into());
        let mut wider_budget = reviewed.clone();
        wider_budget.memory_mib = Some(4096);
        let mut redirects = reviewed.clone();
        redirects.allow_undeclared_public_redirects = true;
        let mut other_plugin = reviewed.clone();
        other_plugin.plugin_key = "path:file:///tmp/other#other".into();
        for wider in [wider_domain, wider_binding, wider_budget, redirects] {
            assert_eq!(state(&wider), RemoteConsentState::NeedsReapproval);
        }
        assert_eq!(state(&other_plugin), RemoteConsentState::Invalid);

        // Unapproved and malformed rows run nothing.
        assert_eq!(
            remote_consent_state(&reviewed.plugin_key, &json(&reviewed), None, None),
            RemoteConsentState::Pending
        );
        assert_eq!(
            remote_consent_state(&reviewed.plugin_key, "{", None, None),
            RemoteConsentState::Invalid
        );
        let mut outbound_from_deny = reviewed.clone();
        let mut deny_reviewed = reviewed.clone();
        deny_reviewed.network_mode = "deny".into();
        deny_reviewed.domains.clear();
        deny_reviewed.tcp.clear();
        outbound_from_deny.domains.clear();
        outbound_from_deny.tcp.clear();
        assert!(!remote_request_within(&deny_reviewed, &outbound_from_deny));
    }

    #[test]
    fn platform_plugins_auto_approve_only_when_bundled_here() {
        let platform = PluginGrant {
            plugin_key: "platform:bookclerk/bookclerk-plugin-database-sqlite".into(),
            plugin_id: "sqlite".into(),
            bindings: ["config".to_string()].into(),
            ..PluginGrant::empty()
        };
        let key = bookclerk_plugin_catalog::PluginKey::parse(&platform.plugin_key).unwrap();
        assert!(bookclerk_plugin_catalog::is_platform_plugin_key(&key));
        assert!(remote_platform_auto_approval(&platform, "abc", Some("abc")));
        assert!(!remote_platform_auto_approval(
            &platform,
            "abc",
            Some("def")
        ));
        assert!(!remote_platform_auto_approval(&platform, "abc", None));
        assert!(!remote_platform_auto_approval(&platform, "", Some("")));
        let mut networked = platform.clone();
        networked.network_mode = "outbound".into();
        networked.domains.insert("x.example".into());
        assert!(!remote_platform_auto_approval(
            &networked,
            "abc",
            Some("abc")
        ));
        let store = requested();
        assert!(!remote_platform_auto_approval(&store, "abc", Some("abc")));
    }
}
