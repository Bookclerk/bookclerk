//! A [`Policy`] in a form that survives a process boundary.
//!
//! Self-confinement is enough for a binary Bookclerk ships: the media worker
//! jails itself before it looks at a single byte of media, and the only code
//! that could skip the call is our own. A plugin guest is different. Its binary
//! is the untrusted thing, so asking it to confine itself asks the attacker to
//! cooperate.
//!
//! So the host decides the jail and a launcher applies it, and the two are
//! different processes. [`Spec`] is what travels between them — JSON in an
//! environment variable, read by `bookclerk-jail` before it hands control to
//! the guest.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Enforcement, NetPolicy, Policy};

/// Descriptor the host leaves open for the fetch-directory side channel.
pub const PLUGIN_FD_CHANNEL: i32 = 3;

/// Environment variable naming [`PLUGIN_FD_CHANNEL`].
pub const PLUGIN_FD_CHANNEL_ENV: &str = "BOOKCLERK_PLUGIN_FD_CHANNEL";

/// Environment variable carrying the JSON [`Spec`] to `bookclerk-jail`.
///
/// The launcher drops it before handing off, so a guest never sees the shape of
/// its own jail and cannot pass it on to something it spawns.
pub const SPEC_ENV: &str = "BOOKCLERK_JAIL_SPEC";

/// A serializable confinement request.
///
/// Mirrors [`Policy`], which is a builder and deliberately does not expose its
/// unresolved fields. Paths are resolved by the launcher, in the launcher's own
/// view of the filesystem, rather than being canonicalized here — the host and
/// the jailed process must agree on physical paths, and only the process that
/// applies the policy can confirm they do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Diagnostics label, echoed in the launcher's report.
    pub label: String,
    /// Paths the confined process may read.
    #[serde(default)]
    pub reads: Vec<PathBuf>,
    /// Paths the confined process may read and write.
    #[serde(default)]
    pub writes: Vec<PathBuf>,
    /// Network reachability.
    #[serde(default)]
    pub net: NetPolicy,
    /// Whether `execve` is permitted.
    ///
    /// A launcher that has to `exec` the real program cannot deny this to
    /// itself. What it can rely on is that the restrictions are inherited and
    /// irreversible, so whatever the guest goes on to exec is confined the same
    /// way, and `no_new_privs` has already neutralized setuid.
    #[serde(default)]
    pub allow_exec: bool,
    /// Whether to include the platform's read-only system set.
    #[serde(default = "default_system_paths")]
    pub system_paths: bool,
    /// What to do when a layer does not engage.
    #[serde(default)]
    pub enforcement: Enforcement,
    /// Descriptors the host wired up deliberately and the launcher must not
    /// close before the hand-off — typically a Unix socket for passing a fetch
    /// directory one RPC at a time.
    #[serde(default)]
    pub preserve_fds: Vec<i32>,
    /// Pre-created AppContainer profile moniker (Windows).
    ///
    /// When set, `bookclerk-jail` attaches to this host-owned profile and does
    /// not delete it. When absent, the jail creates a unique per-launch profile
    /// and deletes it after the guest (and its Job Object) exits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_profile_name: Option<String>,
    /// Soft memory ceiling in bytes (Job Object / cgroup v2 `memory.max`).
    ///
    /// `None` keeps platform defaults (Windows label heuristics; Linux applies
    /// no cgroup memory limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Cap on concurrent processes in the jail (Job active-process / `pids.max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_processes: Option<u32>,
    /// CPU hard-cap as a percent of one CPU (1–100).
    ///
    /// Windows: Job Object CPU rate. Linux: cgroup v2 `cpu.max` quota for a
    /// 100 ms period. macOS Seatbelt cannot enforce this (see docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_rate_percent: Option<u32>,
}

fn default_system_paths() -> bool {
    true
}

impl Spec {
    /// Start a spec with nothing granted.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            reads: Vec::new(),
            writes: Vec::new(),
            net: NetPolicy::default(),
            allow_exec: false,
            system_paths: true,
            enforcement: Enforcement::default(),
            preserve_fds: Vec::new(),
            windows_profile_name: None,
            memory_bytes: None,
            active_processes: None,
            cpu_rate_percent: None,
        }
    }

    /// Build the [`Policy`] this spec describes.
    #[must_use]
    pub fn policy(&self) -> Policy {
        Policy::new(self.label.clone())
            .reads(self.reads.clone())
            .writes(self.writes.clone())
            .net(self.net)
            .allow_exec(self.allow_exec)
            .system_paths(self.system_paths)
            .enforcement(self.enforcement)
            .memory_bytes(self.memory_bytes)
            .active_processes(self.active_processes)
            .cpu_rate_percent(self.cpu_rate_percent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let spec = Spec {
            label: "plugin:audible".into(),
            reads: vec![PathBuf::from("/opt/plugins/audible")],
            writes: vec![PathBuf::from("/var/lib/bookclerk/cache")],
            net: NetPolicy::OutboundListen,
            allow_exec: true,
            system_paths: true,
            enforcement: Enforcement::Required,
            preserve_fds: Vec::new(),
            windows_profile_name: None,
            memory_bytes: Some(512 * 1024 * 1024),
            active_processes: Some(8),
            cpu_rate_percent: Some(80),
        };
        let json = serde_json::to_string(&spec).expect("encode");
        assert_eq!(
            serde_json::from_str::<Spec>(&json).expect("decode"),
            spec,
            "json: {json}"
        );
    }

    /// The wire form is read by a separate binary that may be a different build
    /// than the host, so the names are part of the contract.
    #[test]
    fn policy_names_are_stable_on_the_wire() {
        let json = serde_json::to_string(&Spec {
            net: NetPolicy::OutboundListen,
            enforcement: Enforcement::BestEffort,
            ..Spec::new("probe")
        })
        .expect("encode");
        assert!(json.contains("\"outbound-listen\""), "{json}");
        assert!(json.contains("\"best-effort\""), "{json}");
    }

    #[test]
    fn a_bare_spec_grants_nothing_but_keeps_the_system_set() {
        let json = r#"{"label":"minimal"}"#;
        let spec: Spec = serde_json::from_str(json).expect("decode");
        assert!(spec.reads.is_empty());
        assert!(spec.writes.is_empty());
        assert_eq!(spec.net, NetPolicy::Deny);
        assert!(!spec.allow_exec);
        // A process with no system paths cannot even run its dynamic loader,
        // so the omitted default has to be the permissive one.
        assert!(spec.system_paths);
        assert_eq!(spec.enforcement, Enforcement::Required);
    }

    #[test]
    fn spec_becomes_the_policy_it_describes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = Spec {
            writes: vec![dir.path().to_path_buf()],
            net: NetPolicy::OutboundListen,
            system_paths: false,
            ..Spec::new("plugin:libro")
        };
        let policy = spec.policy();
        assert_eq!(policy.label(), "plugin:libro");
        assert_eq!(policy.net_policy(), NetPolicy::OutboundListen);
        assert_eq!(policy.resolved_writes().len(), 1);
        assert!(policy.resolved_reads().is_empty());
    }
}
