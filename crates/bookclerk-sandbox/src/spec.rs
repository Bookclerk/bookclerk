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

use crate::{CpuRate, Enforcement, NetPolicy, Policy};

/// Reserved descriptor number for a native SCM_RIGHTS side channel (fd 3).
///
/// Plugin hosts do **not** leave this open or set [`PLUGIN_FD_CHANNEL_ENV`].
/// Kept so a future host-selected native shortcut can reuse the same number.
pub const PLUGIN_FD_CHANNEL: i32 = 3;

/// Environment variable naming [`PLUGIN_FD_CHANNEL`].
///
/// Hosts do not set this; guests must not wait on `recvmsg` just because
/// they see fd 3 inherited from a parent.
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
    /// Network reachability granted to the confined process.
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
    /// CPU hard-cap as a percent of **one logical CPU** (`1..=` host cores×100).
    ///
    /// Linux: cgroup v2 `cpu.max` quota for a 100 ms period (`quota = period ×
    /// percent / 100`; values above 100 request multiple cores). Windows: Job
    /// Object hard cap, scaled by logical CPU count so the same percent means
    /// one-core bandwidth (see [`crate::windows_job_cpu_rate`]). macOS Seatbelt
    /// cannot enforce this (see docs).
    ///
    /// Omitted and `null` are unspecified (the label default). A number is a
    /// percent. `"off"` disables the rate so a nested Job is not filled with
    /// the plugin default.
    #[serde(default, skip_serializing_if = "CpuRate::is_unspecified")]
    pub cpu_rate_percent: CpuRate,
    /// Windows handles the jail must put on the child's inherit list.
    ///
    /// The host never marks these inheritable itself. It duplicates them into
    /// `bookclerk-jail` with `duplicate_handle_into` and names them here / in
    /// the stdin handoff line so the jail can add them to
    /// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherit_handles: Vec<u64>,
    /// Linux cgroup v2 leaf both sibling jails join (session aggregate limits).
    ///
    /// When set, the jail moves into this directory instead of creating
    /// `bookclerk-<pid>`. The host writes the ceilings; the jail must not
    /// overwrite them with a per-process budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_dir: Option<PathBuf>,
    /// macOS Seatbelt pathname Unix-socket directories.
    ///
    /// `None` (default, omitted on the wire) keeps the historical rule: bind
    /// and connect under every writable path. `Some(dirs)` allows UDS only in
    /// those directories (`Some([])` grants none). The native-behind-workerd
    /// gateway sets the host-chosen session directory. `build_spec_with_grant`
    /// starts the Deny guest at `Some([])`; the host then replaces that with
    /// the guest's private IPC directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_socket_dirs: Option<Vec<PathBuf>>,
}

/// Default for [`Spec::system_paths`]: include system library/loader paths.
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
            cpu_rate_percent: CpuRate::Unspecified,
            inherit_handles: Vec::new(),
            cgroup_dir: None,
            unix_socket_dirs: None,
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
            .cpu_rate(self.cpu_rate_percent)
            .unix_socket_dirs(self.unix_socket_dirs.clone())
            .cgroup_dir(self.cgroup_dir.clone())
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
            cpu_rate_percent: CpuRate::Percent(80),
            inherit_handles: vec![42],
            cgroup_dir: Some(PathBuf::from("/sys/fs/cgroup/bookclerk-session")),
            unix_socket_dirs: Some(vec![PathBuf::from("/tmp/session")]),
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
    fn cpu_rate_wire_keeps_omitted_null_number_and_off_distinct() {
        let omitted = serde_json::from_str::<Spec>(r#"{"label":"probe"}"#).expect("omitted");
        assert_eq!(omitted.cpu_rate_percent, CpuRate::Unspecified);
        assert!(!serde_json::to_string(&Spec::new("probe"))
            .expect("encode")
            .contains("cpu_rate_percent"));

        let null = serde_json::from_str::<Spec>(r#"{"label":"probe","cpu_rate_percent":null}"#)
            .expect("null");
        assert_eq!(null.cpu_rate_percent, CpuRate::Unspecified);

        let percent = serde_json::from_str::<Spec>(r#"{"label":"probe","cpu_rate_percent":40}"#)
            .expect("number");
        assert_eq!(percent.cpu_rate_percent, CpuRate::Percent(40));

        let off = serde_json::from_str::<Spec>(r#"{"label":"probe","cpu_rate_percent":"off"}"#)
            .expect("off");
        assert_eq!(off.cpu_rate_percent, CpuRate::Disabled);
        let encoded = serde_json::to_value(&off).expect("encode off");
        assert_eq!(encoded["cpu_rate_percent"], "off");

        let resolved = off.policy().resolved_job_limits();
        assert_eq!(resolved.cpu_rate_percent, None);
        assert_eq!(
            Spec::new("plugin:echo")
                .policy()
                .resolved_job_limits()
                .cpu_rate_percent,
            Some(80)
        );
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
        assert!(spec.inherit_handles.is_empty());
        assert_eq!(spec.cgroup_dir, None);
        assert_eq!(spec.unix_socket_dirs, None);
    }

    #[test]
    fn omitted_sibling_fields_stay_backward_compatible() {
        let spec: Spec = serde_json::from_str(
            r#"{"label":"probe","reads":[],"writes":[],"net":"deny","allow_exec":true}"#,
        )
        .expect("decode");
        assert!(spec.inherit_handles.is_empty());
        assert_eq!(spec.cgroup_dir, None);
        assert_eq!(spec.unix_socket_dirs, None);
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
