//! Canonical effective authority and live session fencing.
//!
//! Structural capabilities originate in the plugin package. The operator may
//! narrow them but cannot invent entrypoints, consumers, producers, jobs, or
//! host bindings.
//! Network destinations are operator-extensible: explicit host-owned grants
//! may add hostnames beyond the manifest. Guest `describe()` cannot widen
//! either class.
//!
//! ```text
//! effective structural = manifest ∩ operator grant ∩ host policy
//! effective network    = host policy ∩ approved(manifest network ∪ operator additions)
//!                      − operator denials
//! ```
//!
//! `authority_revision` is SHA-256 of the canonical effective policy. Grant
//! changes fence already-running sessions that still hold the old revision.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};

use crate::consent::PluginGrant;
use crate::PluginError;

/// Live plugin sessions keyed by provenance-qualified PluginKey.
struct LiveSession {
    /// Canonical PluginKey for this vat.
    plugin_key: String,
    /// [`authority_revision`] captured at spawn.
    revision: String,
    /// Set when a later grant for this key no longer matches `revision`.
    cancelled: Arc<AtomicBool>,
}

/// Process-wide live session table.
fn live() -> &'static Mutex<Vec<LiveSession>> {
    static LIVE: OnceLock<Mutex<Vec<LiveSession>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Registers a live session and returns its cancel flag.
///
/// Drop the session (or call [`unregister_session`]) when the vat exits.
#[must_use]
pub fn register_session(plugin_key: &str, revision: &str) -> Arc<AtomicBool> {
    let cancelled = Arc::new(AtomicBool::new(false));
    if let Ok(mut guard) = live().lock() {
        guard.push(LiveSession {
            plugin_key: plugin_key.to_string(),
            revision: revision.to_string(),
            cancelled: Arc::clone(&cancelled),
        });
    }
    cancelled
}

/// Drops a previously registered cancel flag from the live set.
pub fn unregister_session(flag: &Arc<AtomicBool>) {
    if let Ok(mut guard) = live().lock() {
        guard.retain(|s| !Arc::ptr_eq(&s.cancelled, flag));
    }
}

/// Fences every live session for `plugin_key` (grant/authority change).
pub fn fence_plugin_key(plugin_key: &str) {
    fence_stale_sessions(plugin_key, "");
}

/// Fences live sessions for `plugin_key` whose revision is not `current_revision`.
///
/// Pass an empty `current_revision` to fence every session for the key.
pub fn fence_stale_sessions(plugin_key: &str, current_revision: &str) {
    if plugin_key.is_empty() {
        return;
    }
    if let Ok(guard) = live().lock() {
        for session in guard.iter() {
            if session.plugin_key == plugin_key
                && (current_revision.is_empty() || session.revision != current_revision)
            {
                session.cancelled.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// True when this cancel flag has been fenced.
#[must_use]
pub fn is_fenced(flag: &Arc<AtomicBool>) -> bool {
    flag.load(Ordering::SeqCst)
}

/// Error when a session's authority revision is no longer current.
pub fn fenced_error() -> PluginError {
    PluginError::message("plugin session fenced: effective authority changed")
}

/// Canonical SHA-256 of security-relevant effective authority (not presentation).
///
/// Includes structural capabilities, network mode, approved domains, operator
/// additions/denials, and bindings. Omits `approved_at` and display aliases
/// except the PluginKey.
#[must_use]
pub fn authority_revision(grant: &PluginGrant) -> String {
    let mut hasher = Sha256::new();
    hasher.update(grant.plugin_key.as_bytes());
    hasher.update(b"\nstructural\n");
    for e in &grant.entrypoints {
        hasher.update(e.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nproducers\n");
    for p in &grant.producers {
        hasher.update(p.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nconsumers\n");
    for c in &grant.consumers {
        hasher.update(c.event_type.as_bytes());
        hasher.update(b"|");
        for v in &c.schema_versions {
            hasher.update(v.to_string().as_bytes());
            hasher.update(b",");
        }
        hasher.update(if c.supports_suspend { b"s1|" } else { b"s0|" });
        if let Some(filter) = &c.filter {
            hasher.update(filter.as_bytes());
        }
        hasher.update(b";");
    }
    hasher.update(b"\njobs\n");
    for j in &grant.jobs {
        hasher.update(j.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nbindings\n");
    for b in &grant.bindings {
        hasher.update(b.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nflags\n");
    for f in &grant.compatibility_flags {
        hasher.update(f.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nnetwork\n");
    hasher.update(grant.network_mode.as_bytes());
    hasher.update(b"\ndomains\n");
    for d in &grant.domains {
        hasher.update(d.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nmanifest_domains\n");
    for d in &grant.manifest_domains {
        hasher.update(d.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\noperator_added\n");
    for d in &grant.operator_added_domains {
        hasher.update(d.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\noperator_denied\n");
    for d in &grant.operator_denied_domains {
        hasher.update(d.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nbudgets\n");
    hasher.update(
        crate::consent::effective_disk_mib(grant.disk_mib)
            .to_string()
            .as_bytes(),
    );
    hasher.update(b"|mem|");
    hasher.update(
        crate::consent::effective_memory_mib(grant.memory_mib)
            .to_string()
            .as_bytes(),
    );
    hasher.update(b"|cpu_ms|");
    if let Some(cpu_ms) = grant.cpu_ms {
        hasher.update(cpu_ms.to_string().as_bytes());
    } else {
        hasher.update(b"unset");
    }
    hasher.update(b"|subrequests|");
    if let Some(subrequests) = grant.subrequests {
        hasher.update(subrequests.to_string().as_bytes());
    } else {
        hasher.update(b"unset");
    }
    hasher.update(b"|cpu_rate|");
    hasher.update(
        crate::consent::effective_cpu_rate_percent(grant.cpu_rate_percent)
            .to_string()
            .as_bytes(),
    );
    hasher.update(b"|extra_proc|");
    hasher.update(
        crate::consent::effective_extra_processes(grant.extra_processes)
            .to_string()
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn grant(domains: &[&str]) -> PluginGrant {
        PluginGrant {
            schema_version: crate::GRANT_SCHEMA_VERSION,
            plugin_key: "path:file:///tmp/demo".into(),
            plugin_id: "demo".into(),
            entrypoints: ["storefront".to_string()].into_iter().collect(),
            producers: BTreeSet::new(),
            consumers: Default::default(),
            jobs: Default::default(),
            network_mode: "outbound".into(),
            domains: domains.iter().map(|s| (*s).to_string()).collect(),
            manifest_domains: domains.iter().map(|s| (*s).to_string()).collect(),
            operator_added_domains: BTreeSet::new(),
            operator_denied_domains: BTreeSet::new(),
            bindings: ["config".to_string()].into_iter().collect(),
            compatibility_flags: BTreeSet::new(),
            cpu_ms: None,
            subrequests: None,
            disk_mib: None,
            memory_mib: None,
            cpu_rate_percent: None,
            extra_processes: None,
            approved_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn metadata_does_not_change_authority_revision() {
        let a = grant(&["a.example"]);
        let mut b = a.clone();
        b.approved_at = "2099-01-01T00:00:00Z".into();
        b.plugin_id = "other-alias".into();
        assert_eq!(authority_revision(&a), authority_revision(&b));
    }

    #[test]
    fn operator_added_domain_changes_revision() {
        let a = grant(&["a.example"]);
        let mut b = a.clone();
        b.domains.insert("b.example".into());
        b.operator_added_domains.insert("b.example".into());
        assert_ne!(authority_revision(&a), authority_revision(&b));
    }

    #[test]
    fn consumer_and_job_changes_alter_authority_revision() {
        let a = grant(&["a.example"]);
        let mut b = a.clone();
        b.consumers.insert(crate::GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: None,
        });
        assert_ne!(authority_revision(&a), authority_revision(&b));
        let mut c = a.clone();
        c.jobs.insert("stream_copy".into());
        assert_ne!(authority_revision(&a), authority_revision(&c));
    }

    #[test]
    fn fence_stale_sessions_keeps_current_revision() {
        let key = "path:file:///tmp/keep#demo";
        let current = register_session(key, "rev-new");
        let stale = register_session(key, "rev-old");
        fence_stale_sessions(key, "rev-new");
        assert!(!is_fenced(&current));
        assert!(is_fenced(&stale));
        unregister_session(&current);
        unregister_session(&stale);
    }

    #[test]
    fn resource_budgets_change_authority_revision() {
        let a = grant(&["a.example"]);
        for mutate in [
            |g: &mut PluginGrant| g.memory_mib = Some(128),
            |g: &mut PluginGrant| g.cpu_rate_percent = Some(25),
            |g: &mut PluginGrant| g.extra_processes = Some(1),
            |g: &mut PluginGrant| g.disk_mib = Some(64),
            |g: &mut PluginGrant| g.subrequests = Some(7),
            |g: &mut PluginGrant| g.cpu_ms = Some(1_000),
        ] {
            let mut b = a.clone();
            mutate(&mut b);
            assert_ne!(
                authority_revision(&a),
                authority_revision(&b),
                "budget mutation must change authority revision"
            );
        }
    }
}
