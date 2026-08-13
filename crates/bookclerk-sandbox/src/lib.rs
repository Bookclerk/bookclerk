//! Portable, unprivileged process confinement.
//!
//! # Audience
//!
//! Host binaries that confine media workers and plugin guests (`bookclerk-jail`,
//! `bookclerk-media-worker`). Guest plugins never link this crate; the host
//! imposes the jail. See `docs/plugins.md#the-guest-jail` and `docs/media.md`.
//!
//! Bookclerk confines code by what it may touch, not by which uid it runs as.
//! Every backend here works without root, without setuid helpers, and without
//! user namespaces or bind mounts:
//!
//! - **Linux** — Landlock filesystem allowlist plus a seccomp-bpf deny list.
//! - **macOS** — Seatbelt (`sandbox_init`) with a deny-default SBPL profile.
//! - **Windows** — AppContainer applied at `CreateProcess`; see
//!   [`spawn`]. A process cannot confine *itself* on Windows, so
//!   [`Policy::confine_current_process`] reports the filesystem layer as
//!   [`LayerStatus::Unsupported`] there. Children are confined by
//!   [`spawn::run_appcontainer`] (used by `bookclerk-jail`).
//!
//! Callers pick the failure mode with [`Enforcement`]. `Required` turns a
//! backend that cannot enforce into an error, which is what production paths
//! use so a missing jail can never pass silently.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod platform;
mod spec;

pub use platform::BACKEND;
pub use spec::{Spec, PLUGIN_FD_CHANNEL, PLUGIN_FD_CHANNEL_ENV, SPEC_ENV};

/// Windows AppContainer spawn API ([`plan_appcontainer`](spawn::plan_appcontainer),
/// [`run_appcontainer`](spawn::run_appcontainer)).
///
/// Windows AppContainer planning / launch helpers re-exported on every OS so
/// callers can depend on a stable `bookclerk_sandbox::spawn` path. Non-Windows
/// builds keep the planning helpers and return a clear error from launch/ACL
/// entry points.
pub mod spawn {
    pub use crate::platform::windows_pipe::NamedPipeSecurity;
    pub use crate::platform::windows_spawn::{
        grant_path_access, is_os_managed_path, plan_appcontainer, profile_name_for_label,
        run_appcontainer, unique_profile_moniker, AclGrant, AppContainerLaunch,
        AppContainerSession,
    };

    #[cfg(windows)]
    pub use crate::platform::windows_spawn::dacl_mentions_sid;

    /// Former name of [`run_appcontainer`]; kept as a thin alias for callers.
    pub use run_appcontainer as spawn_appcontainer;
}

/// What to do when a confinement layer cannot be enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Enforcement {
    /// Fail the call when any requested layer does not engage.
    #[default]
    Required,
    /// Apply what the host supports and report the rest as unsupported.
    BestEffort,
    /// Apply nothing. Used only by the documented opt-out.
    Disabled,
}

/// Network reachability granted to the confined process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetPolicy {
    /// No IP sockets at all. Media workers only touch local files.
    #[default]
    Deny,
    /// Outbound connections allowed, inbound listeners refused. Enough for a
    /// storefront plugin that only fetches over HTTPS.
    Outbound,
    /// Outbound connections plus a listener on a kernel-assigned port.
    ///
    /// This exists for one flow: an OAuth login that sends the authorization
    /// code back to a short-lived local callback server. The grant is narrower
    /// than [`Full`](Self::Full) in that no fixed port can be claimed, so a
    /// confined process cannot stand up a service on a port anything else would
    /// know to connect to.
    ///
    /// What "kernel-assigned" buys differs by backend: Landlock rules are
    /// per-port, so Linux allows binds within `ip_local_port_range` and refuses
    /// every fixed port, while Seatbelt filters by address and restricts the
    /// listener to loopback. Neither confines it to both at once.
    OutboundListen,
    /// Unrestricted. The daemon binds its own control-plane listener.
    Full,
}

/// A confinement request: an allowlist of paths plus a few coarse switches.
///
/// Paths that do not exist when the policy is applied are skipped, because a
/// Landlock rule on a missing path is an error rather than a wider grant.
#[derive(Debug, Clone)]
pub struct Policy {
    label: String,
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
    net: NetPolicy,
    allow_exec: bool,
    system_paths: bool,
    enforcement: Enforcement,
    memory_bytes: Option<u64>,
    active_processes: Option<u32>,
    cpu_rate_percent: Option<u32>,
}

/// Number of logical CPUs visible to this process (at least 1).
#[must_use]
pub fn host_logical_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1)
}

/// Maximum Spec / grant CPU rate: 100% × logical CPUs (one-core units).
///
/// Example: 8 logical CPUs → `800` (eight full cores of CFS/Job bandwidth).
#[must_use]
pub fn host_cpu_rate_max() -> u32 {
    host_logical_cpus().saturating_mul(100)
}

/// Optional OS resource ceilings carried on a [`Policy`] / [`Spec`].
///
/// Windows applies these (merged with label heuristics) via a Job Object.
/// Linux applies them best-effort via cgroup v2 when at least one field is set.
/// macOS Seatbelt has no equivalent — see [`Policy::has_resource_limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceLimits {
    /// Process memory ceiling in bytes.
    pub memory_bytes: Option<u64>,
    /// CPU hard-cap as a percent of **one logical CPU** (1..=host cores×100).
    ///
    /// Values above 100 request multi-core bandwidth (200 ≈ two cores). Linux
    /// writes cgroup v2 `cpu.max` as `period * percent / 100`. Windows Job
    /// `CpuRate` is a share of **all** processors, so [`windows_job_cpu_rate`]
    /// scales by logical CPU count. Threads share one quota pool.
    pub cpu_rate_percent: Option<u32>,
    /// Maximum concurrent processes in the jail.
    pub active_processes: Option<u32>,
}

/// Map a percent-of-one-CPU hard cap onto a Windows Job Object `CpuRate`.
///
/// Job `CpuRate` is “cycles per 10 000” of **machine** capacity. Bookclerk’s
/// [`ResourceLimits::cpu_rate_percent`] is percent of **one** logical CPU, so
/// this divides by `logical_cpus` (at least 1). Result is clamped to `1..=10000`
/// because `CpuRate = 0` is rejected by the API.
///
/// # Examples
///
/// - 80% of one core on an 8-CPU host → 10% of the machine → `1000`
/// - 400% (four cores) on an 8-CPU host → 50% of the machine → `5000`
#[must_use]
pub fn windows_job_cpu_rate(percent_of_one_cpu: u32, logical_cpus: u32) -> u32 {
    let cores = logical_cpus.max(1);
    let max_pct = cores.saturating_mul(100);
    let pct = percent_of_one_cpu.clamp(1, max_pct);
    ((u64::from(pct) * 100) / u64::from(cores))
        .clamp(1, 10_000)
        .try_into()
        .unwrap_or(10_000)
}

impl ResourceLimits {
    /// Whether any limit was requested.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.memory_bytes.is_none()
            && self.cpu_rate_percent.is_none()
            && self.active_processes.is_none()
    }
}

/// Label-based Job defaults used when a [`Spec`] leaves resource fields unset.
///
/// Plugin guests are capped tightly; labels containing `"media"` get more
/// headroom (matching the historical Windows Job heuristics).
#[must_use]
pub fn label_resource_defaults(label: &str) -> ResourceLimits {
    let media = label.to_ascii_lowercase().contains("media");
    if media {
        ResourceLimits {
            memory_bytes: Some(2 * 1024 * 1024 * 1024),
            cpu_rate_percent: None,
            active_processes: Some(64),
        }
    } else {
        ResourceLimits {
            memory_bytes: Some(512 * 1024 * 1024),
            cpu_rate_percent: Some(80),
            active_processes: Some(8),
        }
    }
}

impl Policy {
    /// Start a deny-default policy. `label` appears in diagnostics only.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            reads: Vec::new(),
            writes: Vec::new(),
            net: NetPolicy::Deny,
            allow_exec: false,
            system_paths: true,
            enforcement: Enforcement::Required,
            memory_bytes: None,
            active_processes: None,
            cpu_rate_percent: None,
        }
    }

    /// Grant read access to `path` and everything beneath it.
    #[must_use]
    pub fn read(mut self, path: impl Into<PathBuf>) -> Self {
        self.reads.push(path.into());
        self
    }

    /// Grant read and write access to `path` and everything beneath it.
    #[must_use]
    pub fn write(mut self, path: impl Into<PathBuf>) -> Self {
        self.writes.push(path.into());
        self
    }

    /// Grant read access to each of `paths`.
    #[must_use]
    pub fn reads<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.reads.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Grant read and write access to each of `paths`.
    #[must_use]
    pub fn writes<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.writes.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Set network reachability. Defaults to [`NetPolicy::Deny`].
    #[must_use]
    pub fn net(mut self, net: NetPolicy) -> Self {
        self.net = net;
        self
    }

    /// Allow `execve` of binaries inside the read allowlist. Defaults to false.
    #[must_use]
    pub fn allow_exec(mut self, allow: bool) -> Self {
        self.allow_exec = allow;
        self
    }

    /// Include the platform's read-only system set (shared libraries, the CA
    /// bundle, resolver configuration). Defaults to true; a worker that needs
    /// nothing but its own scratch directory can turn it off.
    #[must_use]
    pub fn system_paths(mut self, include: bool) -> Self {
        self.system_paths = include;
        self
    }

    /// Set the failure mode. Defaults to [`Enforcement::Required`].
    #[must_use]
    pub fn enforcement(mut self, enforcement: Enforcement) -> Self {
        self.enforcement = enforcement;
        self
    }

    /// Soft memory ceiling in bytes (`None` = platform default / unset).
    #[must_use]
    pub fn memory_bytes(mut self, bytes: Option<u64>) -> Self {
        self.memory_bytes = bytes;
        self
    }

    /// Cap on concurrent processes (`None` = platform default / unset).
    #[must_use]
    pub fn active_processes(mut self, n: Option<u32>) -> Self {
        self.active_processes = n;
        self
    }

    /// CPU hard-cap percent of one logical CPU (`None` = platform default / unset).
    ///
    /// Clamped to `1..=`[`host_cpu_rate_max`] (100 × logical CPUs).
    #[must_use]
    pub fn cpu_rate_percent(mut self, percent: Option<u32>) -> Self {
        let max = host_cpu_rate_max();
        self.cpu_rate_percent = percent.map(|p| p.clamp(1, max));
        self
    }

    /// Diagnostics label supplied to [`Self::new`] (logs / doctor output only).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Spec-provided resource ceilings (before label heuristics).
    #[must_use]
    pub fn resource_limits(&self) -> ResourceLimits {
        ResourceLimits {
            memory_bytes: self.memory_bytes,
            cpu_rate_percent: self.cpu_rate_percent,
            active_processes: self.active_processes,
        }
    }

    /// Whether the Spec/Policy asked for any OS resource ceiling.
    #[must_use]
    pub fn has_resource_limits(&self) -> bool {
        !self.resource_limits().is_empty()
    }

    /// Job Object limits: Spec fields override [`label_resource_defaults`].
    ///
    /// Always returns concrete ceilings suitable for Windows Jobs. Linux only
    /// applies cgroup limits when [`Self::has_resource_limits`] is true.
    #[must_use]
    pub fn resolved_job_limits(&self) -> ResourceLimits {
        let defaults = label_resource_defaults(&self.label);
        ResourceLimits {
            memory_bytes: self.memory_bytes.or(defaults.memory_bytes),
            cpu_rate_percent: self.cpu_rate_percent.or(defaults.cpu_rate_percent),
            active_processes: self.active_processes.or(defaults.active_processes),
        }
    }

    /// Read allowlist, including the platform system set when enabled.
    ///
    /// Missing paths are filtered out and the rest are resolved to their
    /// physical (canonical) location.
    #[must_use]
    pub fn resolved_reads(&self) -> Vec<PathBuf> {
        let system = self
            .system_paths
            .then(platform::system_read_paths)
            .unwrap_or(&[]);
        resolve_all(
            system
                .iter()
                .map(Path::new)
                .chain(self.reads.iter().map(PathBuf::as_path)),
        )
    }

    /// Write allowlist, including the few system paths that have to be writable
    /// when the system set is enabled.
    ///
    /// Missing paths are filtered out and the rest are resolved to their
    /// physical (canonical) location.
    #[must_use]
    pub fn resolved_writes(&self) -> Vec<PathBuf> {
        let system = self
            .system_paths
            .then(platform::system_write_paths)
            .unwrap_or(&[]);
        resolve_all(
            system
                .iter()
                .map(Path::new)
                .chain(self.writes.iter().map(PathBuf::as_path)),
        )
    }

    /// Network reachability granted by this policy.
    #[must_use]
    pub fn net_policy(&self) -> NetPolicy {
        self.net
    }

    /// Whether `execve` is permitted.
    #[must_use]
    pub fn exec_allowed(&self) -> bool {
        self.allow_exec
    }

    /// What to do when a requested layer cannot engage.
    #[must_use]
    pub fn enforcement_mode(&self) -> Enforcement {
        self.enforcement
    }

    /// Confine the calling process. Restrictions are irreversible and are
    /// inherited by children, which can only narrow them further.
    ///
    /// Call this at the top of `main`, before spawning threads or a runtime.
    /// It allocates, so it is *not* async-signal-safe and must not be used from
    /// `Command::pre_exec` in a threaded parent. Child processes we control
    /// confine themselves at startup instead, which closes the window between
    /// `exec` and the jail taking effect.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::NotEnforced`] when [`Enforcement::Required`] was
    /// requested and a layer did not engage, and [`SandboxError::Backend`] when
    /// a backend call fails outright.
    pub fn confine_current_process(&self) -> Result<Report, SandboxError> {
        if self.enforcement == Enforcement::Disabled {
            return Ok(Report::disabled(&self.label));
        }
        let report = platform::confine_current_process(self)?;
        if self.enforcement == Enforcement::Required {
            report.require_enforced()?;
        }
        Ok(report)
    }
}

/// Resolve `path` to the location the kernel will actually check.
///
/// Backends match on physical paths, so a rule naming a symlink covers nothing.
/// macOS is where this bites hardest: `TMPDIR` lives under `/var/folders`,
/// `/var` is a symlink to `/private/var`, and a Seatbelt rule written against
/// the `/var` spelling silently matches no file. Landlock resolves at
/// rule-add time and so is already equivalent, but resolving up front keeps
/// every backend seeing the same set.
///
/// Returns `None` when the path does not exist, since a rule naming a missing
/// path is an error rather than a wider grant.
fn resolve(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().map(strip_verbatim)
}

/// Drop the `\\?\` prefix Windows canonicalization adds.
///
/// Most Win32 path APIs reject verbatim paths. Nothing consumes these on
/// Windows today, but leaving the prefix in place would be a trap for the
/// spawn-side AppContainer work. A no-op everywhere else.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let stripped = path
        .to_string_lossy()
        .strip_prefix(r"\\?\")
        .map(PathBuf::from);
    stripped.unwrap_or(path)
}

/// Resolve every path, dropping the ones that do not exist and collapsing
/// entries that resolve to the same place.
fn resolve_all<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        if let Some(resolved) = resolve(path) {
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        }
    }
    out
}

/// Outcome for one confinement layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerStatus {
    /// The layer engaged fully.
    Enforced,
    /// The layer engaged, but the host does not support every requested
    /// restriction. Carries what was lost.
    Partial(String),
    /// This platform draws its boundaries differently and has no separate
    /// mechanism here, but the restrictions this layer stands for are carried
    /// by another layer in the same report. Nothing was lost.
    ///
    /// macOS is the case that motivates this: Seatbelt gates operation classes
    /// rather than syscall numbers, so there is no seccomp equivalent to
    /// report — while `(deny default)` in the profile already refuses `exec`
    /// and the network. Treating that as a failure would make
    /// [`Enforcement::Required`] impossible to satisfy on macOS.
    ///
    /// A backend must not use this to paper over a restriction that genuinely
    /// is not in effect; that is [`Unsupported`](Self::Unsupported).
    NotApplicable(String),
    /// The host has no mechanism for this layer and the restriction is *not*
    /// in effect. Fails [`Enforcement::Required`].
    Unsupported(String),
    /// The policy did not ask for this layer.
    NotRequested,
}

impl LayerStatus {
    /// Whether this layer is itself providing protection.
    ///
    /// False for [`NotApplicable`](Self::NotApplicable): the protection exists,
    /// but a different layer in the report is the one supplying it.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Enforced | Self::Partial(_))
    }

    /// Whether this status means a requested restriction is missing.
    #[must_use]
    pub fn is_gap(&self) -> bool {
        matches!(self, Self::Unsupported(_))
    }
}

impl fmt::Display for LayerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enforced => write!(f, "enforced"),
            Self::Partial(detail) => write!(f, "partial ({detail})"),
            Self::NotApplicable(detail) => write!(f, "n/a ({detail})"),
            Self::Unsupported(detail) => write!(f, "unsupported ({detail})"),
            Self::NotRequested => write!(f, "not requested"),
        }
    }
}

/// What actually engaged when a [`Policy`] was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The policy's diagnostics label.
    pub label: String,
    /// Backend name, e.g. `landlock+seccomp`.
    pub backend: &'static str,
    /// Filesystem allowlist (Landlock / Seatbelt / AppContainer) status.
    pub filesystem: LayerStatus,
    /// Syscall restriction (seccomp / Seatbelt operation classes) status.
    pub syscall: LayerStatus,
    /// Network restriction status for the requested [`NetPolicy`].
    pub network: LayerStatus,
    /// Memory / CPU / process ceilings (Job Object / cgroup v2).
    pub resources: LayerStatus,
}

impl Report {
    fn disabled(label: &str) -> Self {
        Self {
            label: label.to_string(),
            backend: "disabled",
            filesystem: LayerStatus::NotRequested,
            syscall: LayerStatus::NotRequested,
            network: LayerStatus::NotRequested,
            resources: LayerStatus::NotRequested,
        }
    }

    /// Whether the filesystem allowlist — the layer that protects `master.key`
    /// and `library.db` — is active.
    #[must_use]
    pub fn is_confined(&self) -> bool {
        self.filesystem.is_active()
    }

    fn require_enforced(&self) -> Result<(), SandboxError> {
        for (layer, status) in [
            ("filesystem", &self.filesystem),
            ("syscall", &self.syscall),
            ("network", &self.network),
            ("resources", &self.resources),
        ] {
            if let LayerStatus::Unsupported(detail) = status {
                return Err(SandboxError::NotEnforced {
                    label: self.label.clone(),
                    layer,
                    detail: detail.clone(),
                });
            }
        }
        // A report where nothing engaged at all must never pass as enforced,
        // whatever the individual layers claim. This is the backstop against a
        // backend marking every layer `NotApplicable`.
        if !self.is_confined() {
            return Err(SandboxError::NotEnforced {
                label: self.label.clone(),
                layer: "filesystem",
                detail: format!("no layer engaged: {}", self.summary()),
            });
        }
        Ok(())
    }

    /// One-line summary for startup logs and `bookclerk doctor`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} [{}]: filesystem={}, syscall={}, network={}, resources={}",
            self.label, self.backend, self.filesystem, self.syscall, self.network, self.resources
        )
    }
}

/// What this host can enforce, without applying anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Backend name for this platform (e.g. `landlock+seccomp`).
    pub backend: &'static str,
    /// Whether a process can confine *itself* (Landlock / Seatbelt).
    ///
    /// False on Windows: isolation is granted only at `CreateProcess`. Media
    /// workers that self-confine check this field.
    pub filesystem: bool,
    /// Whether a child can be confined at spawn (AppContainer).
    ///
    /// True on Windows once AppContainer launch is wired. Plugin jails that
    /// start guests through `bookclerk-jail` check this (or [`Self::filesystem`]).
    pub spawn_filesystem: bool,
    /// Whether syscall filtering (seccomp / equivalent) is available.
    pub syscall: bool,
    /// Whether network restriction can be applied on this host.
    pub network: bool,
    /// Human-readable detail, e.g. the Landlock ABI level found.
    pub detail: String,
}

impl Capabilities {
    /// Whether this host can confine a guest somehow (self-confine or spawn-time).
    #[must_use]
    pub fn can_confine_guest(&self) -> bool {
        self.filesystem || self.spawn_filesystem
    }
}

/// Probe what this host supports. Applies nothing; safe to call at any time.
#[must_use]
pub fn capabilities() -> Capabilities {
    platform::capabilities()
}

/// Confinement failure when applying or requiring a [`Policy`].
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// A required layer did not engage under [`Enforcement::Required`].
    #[error("{label}: {layer} confinement not enforced ({detail})")]
    NotEnforced {
        /// Diagnostics label from the policy that failed.
        label: String,
        /// Layer name (`filesystem`, `syscall`, or `network`).
        layer: &'static str,
        /// Why the layer did not engage.
        detail: String,
    },
    /// A backend call failed outright (kernel / API error).
    #[error("{label}: {backend} failed: {detail}")]
    Backend {
        /// Diagnostics label from the policy that failed.
        label: String,
        /// Backend name that returned the error.
        backend: &'static str,
        /// Underlying backend error text.
        detail: String,
    },
}

impl SandboxError {
    /// Only the backends that actually call into the kernel can produce this.
    /// Windows reports every layer up front and the fallback backend does
    /// nothing at all, so neither has a failure path to build an error from.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn backend(label: &str, detail: impl fmt::Display) -> Self {
        Self::Backend {
            label: label.to_string(),
            backend: BACKEND,
            detail: detail.to_string(),
        }
    }
}

/// Resolve a path for use in an allowlist, creating it if absent.
///
/// Landlock and Seatbelt both need the path to exist when the rule is added, so
/// callers that will write into a scratch directory should create it first.
///
/// # Errors
///
/// Propagates directory-creation failures.
pub fn ensure_dir(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(path)?;
    // Canonicalize so a symlinked allowlist entry matches the resolved path the
    // kernel sees; a rule on the link alone would not cover the target. Same
    // spelling as `resolve`, so a scratch directory built here compares equal
    // to the allowlist entry derived from it.
    std::fs::canonicalize(path).map(strip_verbatim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_enforcement_short_circuits() {
        let report = Policy::new("test")
            .enforcement(Enforcement::Disabled)
            .confine_current_process()
            .expect("disabled policy always succeeds");
        assert_eq!(report.backend, "disabled");
        assert!(!report.is_confined());
    }

    #[test]
    fn missing_paths_are_filtered_out() {
        let policy = Policy::new("test")
            .system_paths(false)
            .read("/definitely/not/a/real/path/for/bookclerk")
            .write("/also/not/real");
        assert!(policy.resolved_reads().is_empty());
        assert!(policy.resolved_writes().is_empty());
    }

    /// `/dev/null` is in the read set as well, but a read-only grant makes an
    /// ordinary output redirect fail, so the system set has to widen it.
    #[cfg(unix)]
    #[test]
    fn the_system_set_makes_dev_null_writable() {
        let with = Policy::new("test");
        assert!(
            with.resolved_writes()
                .iter()
                .any(|path| path == Path::new("/dev/null")),
            "expected /dev/null among {:?}",
            with.resolved_writes()
        );

        let without = Policy::new("test").system_paths(false);
        assert!(without.resolved_writes().is_empty());
    }

    #[test]
    fn existing_paths_survive_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = Policy::new("test").system_paths(false).write(dir.path());
        let resolved = policy.resolved_writes();

        // Compared by physical location rather than by spelling: Windows
        // canonicalization returns a `\\?\` path and the allowlist strips that
        // prefix, so the two spellings are equal without being identical.
        assert_eq!(resolved.len(), 1, "expected one entry, got {resolved:?}");
        assert_eq!(
            std::fs::canonicalize(&resolved[0]).expect("canonicalize entry"),
            std::fs::canonicalize(dir.path()).expect("canonicalize dir"),
        );
    }

    #[test]
    fn duplicate_paths_collapse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = Policy::new("test")
            .system_paths(false)
            .write(dir.path())
            .write(dir.path());
        assert_eq!(policy.resolved_writes().len(), 1);
    }

    /// Backends match physical paths, so an allowlist entry has to be resolved
    /// or the rule covers nothing. Two spellings of the same directory must
    /// also collapse to one entry.
    #[cfg(unix)]
    #[test]
    fn symlinked_entries_resolve_to_their_target_and_dedupe() {
        let root = tempfile::tempdir().expect("tempdir");
        let real = root.path().join("real");
        std::fs::create_dir(&real).expect("create dir");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let policy = Policy::new("test")
            .system_paths(false)
            .write(&link)
            .write(&real);
        assert_eq!(
            policy.resolved_writes(),
            vec![std::fs::canonicalize(&real).expect("canonicalize")]
        );
    }

    #[test]
    fn required_enforcement_rejects_unsupported_layer() {
        let report = Report {
            label: "test".into(),
            backend: "none",
            filesystem: LayerStatus::Unsupported("no backend".into()),
            syscall: LayerStatus::NotRequested,
            network: LayerStatus::NotRequested,
            resources: LayerStatus::NotRequested,
        };
        let err = report.require_enforced().expect_err("should reject");
        assert!(matches!(
            err,
            SandboxError::NotEnforced {
                layer: "filesystem",
                ..
            }
        ));
    }

    /// The macOS shape. Seatbelt has no syscall-number filter and never will,
    /// so reporting that as a gap made `Required` unsatisfiable there — the
    /// media worker refused every job on macOS.
    #[test]
    fn required_enforcement_accepts_a_layer_the_platform_covers_differently() {
        let report = Report {
            label: "media-worker:encode_mp3".into(),
            backend: "seatbelt",
            filesystem: LayerStatus::Partial("profile is broader than landlock".into()),
            syscall: LayerStatus::NotApplicable("seatbelt gates operations".into()),
            network: LayerStatus::Enforced,
            resources: LayerStatus::NotApplicable(
                "Seatbelt has no memory/CPU/pids controls".into(),
            ),
        };
        report
            .require_enforced()
            .expect("a platform without a separate syscall filter still confines");
    }

    /// `NotApplicable` must not become a way to report a jail that is not
    /// there: a report where nothing engaged fails regardless.
    #[test]
    fn required_enforcement_rejects_a_report_where_no_layer_engaged() {
        let report = Report {
            label: "test".into(),
            backend: "pretend",
            filesystem: LayerStatus::NotApplicable("nothing to see here".into()),
            syscall: LayerStatus::NotApplicable("nor here".into()),
            network: LayerStatus::NotApplicable("nor here".into()),
            resources: LayerStatus::NotApplicable("nor here".into()),
        };
        let err = report
            .require_enforced()
            .expect_err("an entirely inactive report is not confinement");
        assert!(matches!(err, SandboxError::NotEnforced { .. }));
    }

    #[test]
    fn not_applicable_is_neither_active_nor_a_gap() {
        let status = LayerStatus::NotApplicable("covered elsewhere".into());
        assert!(!status.is_active());
        assert!(!status.is_gap());
        assert!(LayerStatus::Unsupported("missing".into()).is_gap());
        assert!(LayerStatus::Enforced.is_active());
    }

    #[test]
    fn capabilities_probe_does_not_panic() {
        let caps = capabilities();
        assert!(!caps.backend.is_empty());
    }

    #[test]
    fn label_resource_defaults_distinguish_media_from_plugins() {
        let plugin = label_resource_defaults("plugin:echo");
        assert_eq!(plugin.memory_bytes, Some(512 * 1024 * 1024));
        assert_eq!(plugin.cpu_rate_percent, Some(80));
        assert_eq!(plugin.active_processes, Some(8));

        let media = label_resource_defaults("media-worker:encode_mp3");
        assert_eq!(media.memory_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(media.cpu_rate_percent, None);
        assert_eq!(media.active_processes, Some(64));
    }

    #[test]
    fn resolved_job_limits_prefer_spec_fields_over_label_heuristics() {
        let policy = Policy::new("plugin:echo")
            .memory_bytes(Some(64 * 1024 * 1024))
            .active_processes(Some(3))
            .cpu_rate_percent(Some(25));
        let limits = policy.resolved_job_limits();
        assert_eq!(limits.memory_bytes, Some(64 * 1024 * 1024));
        assert_eq!(limits.active_processes, Some(3));
        assert_eq!(limits.cpu_rate_percent, Some(25));

        // Unset fields still fall back to plugin heuristics.
        let partial = Policy::new("plugin:echo").memory_bytes(Some(128 * 1024 * 1024));
        let merged = partial.resolved_job_limits();
        assert_eq!(merged.memory_bytes, Some(128 * 1024 * 1024));
        assert_eq!(merged.cpu_rate_percent, Some(80));
        assert_eq!(merged.active_processes, Some(8));

        // No Spec fields → full label defaults (Windows Job path).
        let unset = Policy::new("plugin:echo").resolved_job_limits();
        assert_eq!(unset, label_resource_defaults("plugin:echo"));
    }

    #[test]
    fn cpu_rate_percent_clamps_to_host_max() {
        let max = host_cpu_rate_max();
        assert_eq!(
            Policy::new("x")
                .cpu_rate_percent(Some(0))
                .resource_limits()
                .cpu_rate_percent,
            Some(1)
        );
        assert_eq!(
            Policy::new("x")
                .cpu_rate_percent(Some(max.saturating_add(50)))
                .resource_limits()
                .cpu_rate_percent,
            Some(max)
        );
        if max >= 200 {
            assert_eq!(
                Policy::new("x")
                    .cpu_rate_percent(Some(200))
                    .resource_limits()
                    .cpu_rate_percent,
                Some(200)
            );
        }
    }

    #[test]
    fn windows_job_cpu_rate_scales_by_logical_cpus() {
        // 80% of one core on 8 CPUs → 10% of machine → CpuRate 1000.
        assert_eq!(windows_job_cpu_rate(80, 8), 1_000);
        assert_eq!(windows_job_cpu_rate(100, 1), 10_000);
        assert_eq!(windows_job_cpu_rate(100, 4), 2_500);
        // Four cores on eight → 50% of machine.
        assert_eq!(windows_job_cpu_rate(400, 8), 5_000);
        // Never emit 0 (API rejects it).
        assert_eq!(windows_job_cpu_rate(1, 128), 1);
        assert_eq!(windows_job_cpu_rate(50, 0), 5_000); // cores treated as 1
                                                        // Cap at full machine.
        assert_eq!(windows_job_cpu_rate(10_000, 4), 10_000);
    }
}
