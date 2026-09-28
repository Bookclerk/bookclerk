//! Operator permission grants for plugin capabilities.
//!
//! Structural authority (entrypoints, producers, event consumers, host
//! bindings, named databases, jobs) originates in the plugin package. The
//! operator may **narrow** it but cannot invent a structural capability the
//! manifest did not declare. Empty stored structural sets grant no authority
//! for every persisted document version, including omitted/`0` legacy JSON.
//! Newly requested structural capabilities stay pending until explicitly
//! approved; they are never inherited from a newer manifest.
//!
//! A stored grant remains usable when a later package version adds optional
//! structural capabilities; the extras stay pending until the operator
//! consents. `describe()` that claims an ungranted capability is rejected.
//!
//! Network authority is operator-extensible: the operator may add hostnames
//! the author omitted. Guest `describe()` remains refinement only.
//!
//! ```text
//! effective structural = manifest ∩ operator grant ∩ host policy
//! effective network    = host policy ∩ approved(manifest network ∪ operator additions)
//!                      − operator denials
//! ```

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use bookclerk_config::{resolve_postgres_url, Config, DatabasePluginKind};
use bookclerk_plugin_abi::{PluginCapabilities, PortalAuthMode};
use bookclerk_plugin_manifest::{
    is_public_internet, is_restricted_hostname, normalize_domain_pattern, EgressPolicy,
    NetworkMode, TcpGrant,
};

use crate::manifest::{PluginManifest, PluginRuntimeKind, WorkerdLimits};
use crate::spawn_plan::GuestRuntimeKind;
use crate::{PluginError, Result};
use bookclerk_plugin_manifest::EventConsumer;

/// Filename under `$BOOKCLERK_FILES_DIR` for persisted grants.
pub const GRANTS_FILE: &str = "plugin-grants.json";
/// Current persisted [`PluginGrant`] document version.
///
/// This constant versions the on-disk grant representation. Missing / `0` JSON
/// is a legacy document. Empty structural sets are fail-closed at **every**
/// schema version: they approve nothing and do not acquire capabilities merely
/// because a newer manifest requests them.
pub const GRANT_SCHEMA_VERSION: u32 = 2;

/// Env keys consumed by `bookclerk-workerd` (`grant.rs`) at isolate start.
pub const WORKERD_GRANT_NETWORK_MODE_ENV: &str = "BOOKCLERK_WORKERD_GRANT_NETWORK_MODE";
/// Comma-separated grant domain allowlist for workerd egress.
pub const WORKERD_GRANT_DOMAINS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_DOMAINS";
/// Grant CPU budget (ms) for workerd logging / limit narrowing.
pub const WORKERD_GRANT_CPU_MS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_CPU_MS";
/// Grant subrequest budget injected into workerd `EGRESS_POLICY`.
pub const WORKERD_GRANT_SUBREQUESTS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_SUBREQUESTS";
/// Canonical JSON [`EgressPolicy`] (preferred over piecemeal domain env vars).
pub const WORKERD_GRANT_POLICY_ENV: &str = "BOOKCLERK_WORKERD_GRANT_POLICY";

/// Default per-plugin `data/` and `tmp/` disk budget (MiB each).
pub const PLUGIN_STATE_BUDGET_MIB_DEFAULT: u32 = 512;
/// Host hard cap for per-plugin disk budget overrides (MiB).
pub const PLUGIN_STATE_BUDGET_MIB_MAX: u32 = 4096;

/// Default jail Spec memory ceiling (MiB) for confined guests.
pub const PLUGIN_JAIL_MEMORY_MIB_DEFAULT: u32 = 512;
/// Host hard cap for per-plugin jail memory overrides (MiB).
pub const PLUGIN_JAIL_MEMORY_MIB_MAX: u32 = 4096;
/// Default jail Spec CPU rate percent for confined guests (one-core units).
pub const PLUGIN_JAIL_CPU_RATE_DEFAULT: u32 = 80;
/// Absolute ceiling used only when host CPU detection is unavailable in docs.
///
/// Prefer [`host_cpu_rate_max`] at runtime (`logical_cpus × 100`).
pub const PLUGIN_JAIL_CPU_RATE_MAX: u32 = 100;
/// Default jail CPU as cores (`PLUGIN_JAIL_CPU_RATE_DEFAULT` / 100).
pub const PLUGIN_JAIL_CPU_CORES_DEFAULT: f64 = 0.80;
/// Default **extra** processes/threads beyond launcher overhead.
pub const PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT: u32 = 2;
/// Host hard cap for per-plugin extra process budget.
pub const PLUGIN_JAIL_EXTRA_PROCESSES_MAX: u32 = 62;
/// Absolute OS jail PID ceiling (`overhead + extra`, inclusive).
pub const PLUGIN_JAIL_ACTIVE_PROCESSES_MAX: u32 = 64;
/// Windows outer session Job processes before the granted extra budget.
///
/// Both `bookclerk-jail` supervisors, `bookclerk-workerd`, the pinned
/// `workerd`, and the native guest. Payload overhead stays 3
/// ([`GuestRuntimeKind::process_overhead`]); this baseline is only the outer
/// Windows Job. Linux `pids.max` counts threads and must not use this number.
pub const WINDOWS_SESSION_PROCESS_BASELINE: u32 = 5;

/// Fixed payload occupancy for the launcher / primary guest tree.
///
/// Workerd isolate: `bookclerk-workerd` plus the `workerd` child (2).
/// Native behind workerd: those two plus the native guest (3). Direct native
/// (diagnostic transport): the guest (1). The Windows outer Job adds the two
/// jail supervisors on top of the native-behind payload; see
/// [`WINDOWS_SESSION_PROCESS_BASELINE`].
#[must_use]
pub fn jail_process_overhead(runtime: GuestRuntimeKind) -> u32 {
    runtime.process_overhead()
}

/// Windows outer session Job active-process cap: baseline 5 plus `extra`.
///
/// `extra` is clamped to [`PLUGIN_JAIL_EXTRA_PROCESSES_MAX`]. The sum is
/// clamped to [`PLUGIN_JAIL_ACTIVE_PROCESSES_MAX`]. Inner jail caps are not
/// inflated by this baseline: the gateway stays at 2 and the guest stays at
/// 1 plus the same extra budget.
#[must_use]
pub fn windows_session_active_processes(extra: u32) -> u32 {
    let extra = extra.min(PLUGIN_JAIL_EXTRA_PROCESSES_MAX);
    WINDOWS_SESSION_PROCESS_BASELINE
        .saturating_add(extra)
        .min(PLUGIN_JAIL_ACTIVE_PROCESSES_MAX)
}

/// Clamp an operator/manifest extra-process budget (`0..=`[`PLUGIN_JAIL_EXTRA_PROCESSES_MAX`]).
#[must_use]
pub fn effective_extra_processes(value: Option<u32>) -> u32 {
    value
        .unwrap_or(PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT)
        .min(PLUGIN_JAIL_EXTRA_PROCESSES_MAX)
}

/// Absolute Spec `active_processes` from runtime overhead + extra budget.
#[must_use]
pub fn active_processes_for(runtime: GuestRuntimeKind, extra: u32) -> u32 {
    let overhead = jail_process_overhead(runtime);
    let extra = extra.min(PLUGIN_JAIL_EXTRA_PROCESSES_MAX);
    (overhead.saturating_add(extra)).min(PLUGIN_JAIL_ACTIVE_PROCESSES_MAX)
}

/// Convert Spec/config percent-of-one-core (100 = 1.00 core) to cores (2 d.p.).
#[must_use]
pub fn percent_to_cores(percent: u32) -> f64 {
    (f64::from(percent) / 100.0 * 100.0).round() / 100.0
}

/// Convert cores to Spec/config percent-of-one-core, rounded to 0.01 core.
#[must_use]
pub fn cores_to_percent(cores: f64) -> u32 {
    if !cores.is_finite() || cores <= 0.0 {
        return 0;
    }
    let pct = (cores * 100.0).round();
    if pct < 1.0 {
        1
    } else if pct > f64::from(u32::MAX) {
        u32::MAX
    } else {
        pct as u32
    }
}

/// Host max jail CPU in cores (`logical_cpus` as a 2 d.p. float).
#[must_use]
pub fn host_cpu_cores_max() -> f64 {
    percent_to_cores(host_cpu_rate_max())
}

/// Clamp operator/manifest cores into `0.01..=`[`host_cpu_cores_max`].
#[must_use]
pub fn effective_cpu_cores(value: Option<f64>) -> f64 {
    let max = host_cpu_cores_max();
    let raw = value.unwrap_or(PLUGIN_JAIL_CPU_CORES_DEFAULT);
    if !raw.is_finite() {
        return PLUGIN_JAIL_CPU_CORES_DEFAULT.min(max);
    }
    let stepped = (raw * 100.0).round() / 100.0;
    stepped.clamp(0.01, max)
}

/// Host binding names operators may grant (widen or narrow).
pub const KNOWN_HOST_BINDINGS: &[&str] = &["config", "secrets", "plugin_kv", "work_fs", "oauth"];

/// Grant-entry prefix for named plugin database bindings (`database:<NAME>`).
pub const DATABASE_BINDING_PREFIX: &str = "database:";

/// The binding name when a grant entry is a named database binding.
#[must_use]
pub fn database_binding_name(binding: &str) -> Option<&str> {
    binding.strip_prefix(DATABASE_BINDING_PREFIX)
}

/// Consented database binding names on a grant, in manifest-set order.
#[must_use]
pub fn granted_database_bindings(grant: &PluginGrant) -> Vec<String> {
    grant
        .bindings
        .iter()
        .filter_map(|b| database_binding_name(b).map(str::to_string))
        .collect()
}

/// One approved event-consumer subscription (structural authority).
///
/// Identity includes the event type plus the security-relevant subscription
/// properties that change what the plugin receives: schema versions, suspend
/// support, and the host-owned payload filter. Operational knobs
/// (`resource_class`, `max_retries`) are **not** part of authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct GrantedEventConsumer {
    /// Versioned event type (`book_acquired`, …).
    pub event_type: String,
    /// Schema versions this grant may deliver (sorted, unique).
    pub schema_versions: Vec<u32>,
    /// Whether `EventResult::Suspended` is authorized for this type.
    pub supports_suspend: bool,
    /// Canonical JSON of the host-owned payload filter, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

impl GrantedEventConsumer {
    /// Structural consumer snapshot from a `[[events.consumers]]` row.
    #[must_use]
    pub fn from_manifest(consumer: &EventConsumer) -> Self {
        let mut schema_versions = consumer.schema_versions.clone();
        schema_versions.sort_unstable();
        schema_versions.dedup();
        Self {
            event_type: consumer.event_type.clone(),
            schema_versions,
            supports_suspend: consumer.supports_suspend,
            filter: canonical_event_filter(&consumer.filter),
        }
    }

    /// True when `requested` is the same type with equal-or-narrower schemas,
    /// equal-or-narrower suspend, and the same filter.
    #[must_use]
    pub fn covers(&self, requested: &Self) -> bool {
        self.event_type == requested.event_type
            && requested
                .schema_versions
                .iter()
                .all(|v| self.schema_versions.contains(v))
            && (!requested.supports_suspend || self.supports_suspend)
            && self.filter == requested.filter
    }

    /// Intersection used when a stored grant already covers the current manifest.
    #[must_use]
    pub fn intersect(&self, requested: &Self) -> Option<Self> {
        if self.event_type != requested.event_type || self.filter != requested.filter {
            return None;
        }
        let schema_versions: Vec<u32> = requested
            .schema_versions
            .iter()
            .copied()
            .filter(|v| self.schema_versions.contains(v))
            .collect();
        if schema_versions.is_empty() && !requested.schema_versions.is_empty() {
            return None;
        }
        Some(Self {
            event_type: self.event_type.clone(),
            schema_versions,
            supports_suspend: self.supports_suspend && requested.supports_suspend,
            filter: self.filter.clone(),
        })
    }
}

/// Minified JSON for an event filter; `null` / missing → `None`.
#[must_use]
pub fn canonical_event_filter(filter: &Option<serde_json::Value>) -> Option<String> {
    filter.as_ref().and_then(|value| {
        if value.is_null() {
            None
        } else {
            serde_json::to_string(value).ok()
        }
    })
}

/// True when every requested consumer is covered by some approved consumer.
#[must_use]
pub fn consumers_cover(
    existing: &BTreeSet<GrantedEventConsumer>,
    requested: &BTreeSet<GrantedEventConsumer>,
) -> bool {
    requested
        .iter()
        .all(|req| existing.iter().any(|got| got.covers(req)))
}

/// Intersect stored consumers with the current manifest request.
fn intersect_consumers(
    existing: &BTreeSet<GrantedEventConsumer>,
    requested: &BTreeSet<GrantedEventConsumer>,
) -> BTreeSet<GrantedEventConsumer> {
    requested
        .iter()
        .filter_map(|req| {
            existing
                .iter()
                .find(|got| got.event_type == req.event_type && got.filter == req.filter)
                .and_then(|got| got.intersect(req))
        })
        .collect()
}

/// Consumers declared by `plugin.toml` as structural grant entries.
#[must_use]
pub fn granted_consumers_from_manifest(
    manifest: &PluginManifest,
) -> BTreeSet<GrantedEventConsumer> {
    manifest
        .events
        .consumers
        .iter()
        .map(GrantedEventConsumer::from_manifest)
        .collect()
}

/// Job trigger types declared by `[triggers].jobs`.
#[must_use]
pub fn granted_jobs_from_manifest(manifest: &PluginManifest) -> BTreeSet<String> {
    manifest.triggers.jobs.iter().cloned().collect()
}

/// One approved grant snapshot for a provenance-qualified plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginGrant {
    /// Persisted grant document version.
    ///
    /// `0` (or omitted JSON) is a legacy document. Empty structural sets
    /// approve nothing at every version; a newer manifest cannot acquire
    /// entrypoints, producers, consumers, jobs, or bindings by request alone.
    #[serde(default)]
    pub schema_version: u32,
    /// Canonical [`bookclerk_plugin_catalog::PluginKey`].
    #[serde(default)]
    pub plugin_key: String,
    /// Display / CLI alias from `plugin.toml` (not security authoritative).
    pub plugin_id: String,
    /// Approved named entrypoints (wire names: `storefront`, `storage`,
    /// `databaseAdapter`, `remoteLibrary`, `cli`, `oidc`).
    #[serde(default)]
    pub entrypoints: BTreeSet<String>,
    /// Approved event types the plugin may publish through its events binding.
    #[serde(default)]
    pub producers: BTreeSet<String>,
    /// Approved event-consumer subscriptions (type + schema + suspend + filter).
    #[serde(default)]
    pub consumers: BTreeSet<GrantedEventConsumer>,
    /// Approved `[triggers].jobs` types the plugin may handle.
    #[serde(default)]
    pub jobs: BTreeSet<String>,
    /// Approved network mode: `deny` or `outbound`.
    pub network_mode: String,
    /// Approved initial outbound domain patterns (**workerd** allowlist).
    ///
    /// Effective set: manifest request ∪ operator additions − operator denials.
    pub domains: BTreeSet<String>,
    /// Network destinations requested by the installed manifest (not operator-added).
    #[serde(default)]
    pub manifest_domains: BTreeSet<String>,
    /// Operator-added destinations beyond the current manifest request.
    ///
    /// Survive upgrades of the same [`Self::plugin_key`]. Do not transfer across provenance.
    #[serde(default)]
    pub operator_added_domains: BTreeSet<String>,
    /// Operator-denied destinations (even if the manifest still lists them).
    #[serde(default)]
    pub operator_denied_domains: BTreeSet<String>,
    /// Effective raw TCP grants (manifest ∪ operator additions − denials).
    #[serde(default)]
    pub tcp: BTreeSet<TcpGrant>,
    /// TCP grants requested by the installed manifest.
    #[serde(default)]
    pub manifest_tcp: BTreeSet<TcpGrant>,
    /// Operator-added TCP grants; survive upgrades of the same PluginKey.
    #[serde(default)]
    pub operator_added_tcp: BTreeSet<TcpGrant>,
    /// Operator-denied TCP grants (even if the current manifest still lists them).
    ///
    /// Same PluginKey upgrades must not silently restore a TCP host the operator
    /// removed. Host spawn overlays (postgres URL, D1, ABS, S3) may still add
    /// destinations from operator config after this set is applied.
    #[serde(default)]
    pub operator_denied_tcp: BTreeSet<TcpGrant>,
    /// Effective CIDR grants beyond the public Internet.
    #[serde(default)]
    pub address_cidrs: BTreeSet<String>,
    /// CIDRs requested by the installed manifest.
    #[serde(default)]
    pub manifest_cidrs: BTreeSet<String>,
    /// Operator-added CIDRs; survive upgrades of the same PluginKey.
    #[serde(default)]
    pub operator_added_cidrs: BTreeSet<String>,
    /// Operator-denied CIDRs (even if the current manifest still lists them).
    ///
    /// Same PluginKey upgrades must not silently restore an address-space grant
    /// the operator removed. Host spawn overlays (loopback postgres/ABS) may
    /// still add CIDRs from operator config after this set is applied.
    #[serde(default)]
    pub operator_denied_cidrs: BTreeSet<String>,
    /// Effective undeclared-public-redirect permission.
    #[serde(default)]
    pub allow_undeclared_public_redirects: bool,
    /// Explicit operator override for undeclared public redirects (`None` = follow manifest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_allow_undeclared_public_redirects: Option<bool>,
    /// Approved host binding names (`config`, `secrets`, `oauth`, …).
    pub bindings: BTreeSet<String>,
    /// Approved workerd compatibility flags from the consent snapshot.
    pub compatibility_flags: BTreeSet<String>,
    /// Optional workerd CPU budget override in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u32>,
    /// Optional workerd outbound fetch / subrequest budget override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subrequests: Option<u32>,
    /// Optional per-plugin disk budget for `data/` and `tmp/` (MiB each).
    ///
    /// Applies to **native and workerd** guests. Unset →
    /// [`PLUGIN_STATE_BUDGET_MIB_DEFAULT`]; clamped to
    /// [`PLUGIN_STATE_BUDGET_MIB_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_mib: Option<u32>,
    /// Optional jail Spec memory ceiling (MiB) for **native and workerd**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
    /// Optional jail Spec CPU rate as percent of one logical CPU for **native**
    /// guests (`1..=`[`host_cpu_rate_max`]; values above 100 request multi-core
    /// bandwidth). Workerd guests omit this and use `cpu_ms` plus the host jail
    /// CPU ceiling instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_rate_percent: Option<u32>,
    /// Optional **extra** process/thread budget beyond launcher overhead (**native**).
    ///
    /// Workerd guests omit this; the host applies default extra headroom only.
    /// Absolute Spec `active_processes` = overhead(runtime) + this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_processes: Option<u32>,
    /// RFC 3339 time when the operator approved this grant.
    pub approved_at: String,
}

impl PluginGrant {
    /// True when this grant authorizes delivering `event_type` at `schema_version`.
    #[must_use]
    pub fn allows_event_consumer(&self, event_type: &str, schema_version: u32) -> bool {
        self.consumers
            .iter()
            .any(|c| c.event_type == event_type && c.schema_versions.contains(&schema_version))
    }

    /// True when this grant authorizes handling `job_type`.
    #[must_use]
    pub fn allows_job(&self, job_type: &str) -> bool {
        self.jobs.iter().any(|j| j == job_type)
    }

    /// Empty grant used as a struct-update base for tests and new fields.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: 0,
            plugin_key: String::new(),
            plugin_id: String::new(),
            entrypoints: BTreeSet::new(),
            producers: BTreeSet::new(),
            consumers: BTreeSet::new(),
            jobs: BTreeSet::new(),
            network_mode: "deny".into(),
            domains: BTreeSet::new(),
            manifest_domains: BTreeSet::new(),
            operator_added_domains: BTreeSet::new(),
            operator_denied_domains: BTreeSet::new(),
            tcp: BTreeSet::new(),
            manifest_tcp: BTreeSet::new(),
            operator_added_tcp: BTreeSet::new(),
            operator_denied_tcp: BTreeSet::new(),
            address_cidrs: BTreeSet::new(),
            manifest_cidrs: BTreeSet::new(),
            operator_added_cidrs: BTreeSet::new(),
            operator_denied_cidrs: BTreeSet::new(),
            allow_undeclared_public_redirects: false,
            operator_allow_undeclared_public_redirects: None,
            bindings: BTreeSet::new(),
            compatibility_flags: BTreeSet::new(),
            cpu_ms: None,
            subrequests: None,
            disk_mib: None,
            memory_mib: None,
            cpu_rate_percent: None,
            extra_processes: None,
            approved_at: String::new(),
        }
    }

    /// Canonical typed network policy for workerd and the native socket proxy.
    #[must_use]
    pub fn egress_policy(&self) -> EgressPolicy {
        let mode = if self.network_mode.eq_ignore_ascii_case("outbound") {
            NetworkMode::Outbound
        } else {
            NetworkMode::Deny
        };
        let mut tcp: Vec<TcpGrant> = self.tcp.iter().cloned().collect();
        tcp.sort();
        EgressPolicy {
            mode,
            domains: self.domains.iter().cloned().collect(),
            max_redirects: bookclerk_plugin_manifest::DEFAULT_MAX_REDIRECTS,
            subrequests: self.subrequests,
            allow_undeclared_public_redirects: self.allow_undeclared_public_redirects,
            tcp,
            address_cidrs: self.address_cidrs.iter().cloned().collect(),
        }
    }
}

/// On-disk grant store.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginGrantStore {
    /// Persisted per-plugin grant snapshots.
    #[serde(default)]
    pub grants: Vec<PluginGrant>,
}

impl PluginGrantStore {
    /// Absolute path to `plugin-grants.json` under `files_dir`.
    pub fn path(files_dir: &Path) -> PathBuf {
        files_dir.join(GRANTS_FILE)
    }

    /// Loads grants from disk, or an empty store when the file is missing.
    ///
    /// Operator-selected `files_dir` may contain `..` spelling (resolved via
    /// canonicalize). Escaping or dangling leaf symlinks at `plugin-grants.json`
    /// are hard errors; a true missing file yields an empty store.
    ///
    /// # Arguments
    ///
    /// * `files_dir` - Bookclerk files directory that owns `plugin-grants.json`.
    ///
    /// # Returns
    ///
    /// Parsed [`PluginGrantStore`], defaulting when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the file cannot be read or parsed.
    pub fn load(files_dir: &Path) -> Result<Self> {
        let root = match files_dir.canonicalize() {
            Ok(root) => root,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(PluginError::message(format!(
                    "could not canonicalize files_dir {}: {err}",
                    files_dir.display()
                )));
            }
        };
        let path = bookclerk_plugin_catalog::require_under(&root, &Self::path(&root))
            .map_err(|err| PluginError::message(err.to_string()))?;
        match std::fs::read_to_string(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(PluginError::message(format!(
                "read {}: {err}",
                path.display()
            ))),
            Ok(text) => Ok(serde_json::from_str(&text)?),
        }
    }

    /// Writes this grant store to `plugin-grants.json` under `files_dir`.
    ///
    /// Uses unique same-dir `create_new` staging and Unix `rename` / Windows
    /// `MoveFileExW` so a leaf symlink at the final path is replaced as a
    /// directory entry (not followed). Cleanup removes only staging this call
    /// created.
    ///
    /// # Arguments
    ///
    /// * `files_dir` - Bookclerk files directory that owns `plugin-grants.json`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when serialization or the write fails.
    pub fn save(&self, files_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(files_dir)?;
        let root = files_dir.canonicalize().map_err(|err| {
            PluginError::message(format!(
                "could not canonicalize files_dir {}: {err}",
                files_dir.display()
            ))
        })?;
        let path = root.join(GRANTS_FILE);
        if !path.starts_with(&root) {
            return Err(PluginError::message("plugin-grants path escaped files_dir"));
        }
        let text = serde_json::to_string_pretty(self)?;
        write_grants_atomic(&root, &path, text.as_bytes())?;
        crate::authority::notify_grants_changed();
        Ok(())
    }

    /// Returns the grant whose canonical PluginKey matches `plugin_key`.
    ///
    /// Does not fall back to display aliases — spawn and privilege checks must
    /// not inherit another provenance's grant.
    pub fn get_by_plugin_key(&self, plugin_key: &str) -> Option<&PluginGrant> {
        if plugin_key.is_empty() {
            return None;
        }
        self.grants
            .iter()
            .find(|g| !g.plugin_key.is_empty() && g.plugin_key == plugin_key)
    }

    /// Returns the grant for `plugin_key` (canonical), or an unambiguous alias.
    ///
    /// Display / CLI helper only. Spawn, overlay, and other privilege checks
    /// must call [`Self::get_by_plugin_key`] so an alias cannot inherit another
    /// provenance's grant.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - Canonical PluginKey text, or a display alias.
    ///
    /// # Returns
    ///
    /// A reference to the matching [`PluginGrant`], or `None`.
    pub fn get(&self, plugin_key: &str) -> Option<&PluginGrant> {
        if let Some(g) = self.get_by_plugin_key(plugin_key) {
            return Some(g);
        }
        let alias_hits: Vec<_> = self
            .grants
            .iter()
            .filter(|g| g.plugin_id == plugin_key)
            .collect();
        match alias_hits.as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }

    /// Inserts or replaces the grant for `grant.plugin_key`.
    ///
    /// Incoming grants with a PluginKey match only that key: they never
    /// overwrite a different provenance, and never collapse onto a keyless
    /// same-alias legacy row. A keyless incoming grant (legacy files / tests)
    /// matches only other keyless rows by alias so it cannot replace a
    /// provenance-qualified grant.
    ///
    /// # Arguments
    ///
    /// * `grant` - Full consent snapshot to persist in memory (call [`Self::save`] to flush).
    pub fn upsert(&mut self, grant: PluginGrant) {
        let idx = if grant.plugin_key.is_empty() {
            self.grants
                .iter()
                .position(|g| g.plugin_key.is_empty() && g.plugin_id == grant.plugin_id)
        } else {
            self.grants
                .iter()
                .position(|g| g.plugin_key == grant.plugin_key)
        };
        if !grant.plugin_key.is_empty() {
            let revision = crate::authority::grant_revision(&grant);
            crate::authority::fence_stale_grant_revisions(&grant.plugin_key, &revision);
        }
        if let Some(i) = idx {
            self.grants[i] = grant;
        } else {
            self.grants.push(grant);
        }
    }
}

/// Unique same-directory staging path for an atomic grants replace.
fn staging_grants_path(dir: &Path, dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(GRANTS_FILE);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    dir.join(format!(
        ".{name}.tmp-{}-{nonce:016x}-{n}",
        std::process::id()
    ))
}

/// Replace `to` with staging file `from` without following a leaf symlink at `to`.
fn replace_grants_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        replace_grants_windows(from, to)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // MoveFileExW FFI — same boundary as bookclerk-config.
/// Atomically replaces `to` with `from` via `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`.
///
/// # Errors
///
/// Returns an I/O error when `MoveFileExW` fails.
fn replace_grants_windows(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let src = wide(from);
    let dst = wide(to);
    // SAFETY: `src` and `dst` are NUL-terminated wide paths that outlive the call.
    let ok = unsafe {
        MoveFileExW(
            src.as_ptr(),
            dst.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Write `bytes` to `dest` via same-dir temp + atomic replace.
fn write_grants_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    if !dest.starts_with(dir) {
        return Err(PluginError::message(
            "plugin-grants destination escapes files_dir",
        ));
    }
    let staging = staging_grants_path(dir, dest);
    if !staging.starts_with(dir) {
        return Err(PluginError::message(
            "plugin-grants staging path escapes files_dir",
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .map_err(|err| {
            PluginError::message(format!(
                "create plugin-grants staging {}: {err}",
                staging.display()
            ))
        })?;
    if let Err(err) = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })() {
        let _ = std::fs::remove_file(&staging);
        return Err(PluginError::message(format!(
            "write plugin-grants staging {}: {err}",
            staging.display()
        )));
    }
    match replace_grants_entry(&staging, dest) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&staging);
            Err(PluginError::message(format!(
                "replace plugin-grants {}: {err}",
                dest.display()
            )))
        }
    }
}

/// Build the consent request surface from a manifest and its PluginKey.
#[must_use]
pub fn consent_request(
    manifest: &PluginManifest,
    plugin_key: &bookclerk_plugin_catalog::PluginKey,
) -> PluginGrant {
    let mut grant = consent_request_alias(manifest);
    grant.plugin_key = plugin_key.canonical().to_string();
    grant
}

/// Build the consent request surface from a manifest (alias only; tests).
#[must_use]
pub fn consent_request_alias(manifest: &PluginManifest) -> PluginGrant {
    let mut bindings = BTreeSet::new();
    let b = manifest.bindings();
    if b.config {
        bindings.insert("config".into());
    }
    if b.secrets {
        bindings.insert("secrets".into());
    }
    if b.plugin_kv {
        bindings.insert("plugin_kv".into());
    }
    if b.work_fs {
        bindings.insert("work_fs".into());
    }
    if b.oauth {
        bindings.insert("oauth".into());
    }
    for name in &b.databases {
        bindings.insert(format!("{DATABASE_BINDING_PREFIX}{name}"));
    }
    let flags = manifest
        .workerd
        .as_ref()
        .map(|w| w.compatibility_flags.iter().cloned().collect())
        .unwrap_or_default();
    let domains: BTreeSet<String> = bookclerk_plugin_manifest::consent_domains_for(manifest)
        .unwrap_or_else(|_| {
            manifest
                .capabilities
                .network
                .domains
                .iter()
                .filter_map(|d| bookclerk_plugin_manifest::normalize_domain_pattern(d))
                .collect::<Vec<_>>()
        })
        .into_iter()
        .collect();
    let net = &manifest.capabilities.network;
    let tcp: BTreeSet<TcpGrant> = net
        .tcp
        .iter()
        .filter_map(|t| {
            let host = bookclerk_plugin_manifest::normalize_domain_pattern(&t.host)?;
            let mut ports = t.ports.clone();
            ports.sort_unstable();
            ports.dedup();
            Some(TcpGrant { host, ports })
        })
        .collect();
    let address_cidrs: BTreeSet<String> = net
        .address_cidrs
        .iter()
        .filter_map(|raw| {
            bookclerk_plugin_manifest::CidrGrant::parse(raw)
                .ok()
                .map(|g| format!("{}/{}", g.network, g.prefix))
        })
        .collect();
    let (cpu_ms, subrequests) = if manifest.runtime == PluginRuntimeKind::Workerd {
        let effective = manifest
            .workerd
            .as_ref()
            .map(|workerd| workerd.limits.clone())
            .unwrap_or_default()
            .effective();
        (Some(effective.cpu_ms), Some(effective.subrequests))
    } else {
        (None, None)
    };
    PluginGrant {
        schema_version: GRANT_SCHEMA_VERSION,
        plugin_key: String::new(),
        plugin_id: manifest.id.clone(),
        entrypoints: manifest
            .entrypoints
            .iter()
            .map(|e| e.wire_name().to_string())
            .collect(),
        producers: manifest.producer_types().into_iter().collect(),
        consumers: granted_consumers_from_manifest(manifest),
        jobs: granted_jobs_from_manifest(manifest),
        network_mode: match manifest.capabilities.network.mode {
            crate::manifest::NetworkMode::Deny => "deny".into(),
            crate::manifest::NetworkMode::Outbound => "outbound".into(),
        },
        domains: domains.clone(),
        manifest_domains: domains,
        operator_added_domains: BTreeSet::new(),
        operator_denied_domains: BTreeSet::new(),
        tcp: tcp.clone(),
        manifest_tcp: tcp,
        operator_added_tcp: BTreeSet::new(),
        operator_denied_tcp: BTreeSet::new(),
        address_cidrs: address_cidrs.clone(),
        manifest_cidrs: address_cidrs,
        operator_added_cidrs: BTreeSet::new(),
        operator_denied_cidrs: BTreeSet::new(),
        allow_undeclared_public_redirects: net.allow_undeclared_public_redirects,
        operator_allow_undeclared_public_redirects: None,
        bindings,
        compatibility_flags: flags,
        cpu_ms,
        subrequests,
        disk_mib: Some(PLUGIN_STATE_BUDGET_MIB_DEFAULT),
        memory_mib: Some(PLUGIN_JAIL_MEMORY_MIB_DEFAULT),
        // Native: tunable jail CPU. Workerd: isolate `cpu_ms` + host jail ceiling.
        cpu_rate_percent: if manifest.runtime == PluginRuntimeKind::Workerd {
            None
        } else {
            Some(PLUGIN_JAIL_CPU_RATE_DEFAULT)
        },
        // Native: operator-tunable extra budget. Workerd: host default headroom only.
        extra_processes: if manifest.runtime == PluginRuntimeKind::Workerd {
            None
        } else {
            Some(PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT)
        },
        approved_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Human-readable consent lines for CLI/UI.
#[must_use]
pub fn consent_summary(grant: &PluginGrant) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Plugin: {} (entrypoints: {})",
            grant.plugin_id,
            if grant.entrypoints.is_empty() {
                "none".to_string()
            } else {
                grant
                    .entrypoints
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
        format!("Network: {}", grant.network_mode),
    ];
    if !grant.producers.is_empty() {
        lines.push(format!(
            "Publishes events: {}",
            grant
                .producers
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !grant.consumers.is_empty() {
        let names: Vec<String> = grant
            .consumers
            .iter()
            .map(|c| {
                format!(
                    "{} (schemas {}; suspend={})",
                    c.event_type,
                    c.schema_versions
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                    c.supports_suspend
                )
            })
            .collect();
        lines.push(format!("Consumes events: {}", names.join("; ")));
    }
    if !grant.jobs.is_empty() {
        lines.push(format!(
            "Job triggers: {}",
            grant.jobs.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if grant.network_mode == "outbound" && grant.domains.is_empty() && grant.tcp.is_empty() {
        lines.push(
            "Outbound mode with no fetch hosts or TCP grants: the guest cannot fetch or \
             connect until the operator adds destinations. Ambient internet sockets are \
             not a grant."
                .into(),
        );
    }
    if !grant.domains.is_empty() {
        lines.push(format!(
            "Workerd initial outbound domains: {}",
            grant.domains.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
        if !grant.operator_added_domains.is_empty() {
            lines.push(format!(
                "Operator-added destinations: {}",
                grant
                    .operator_added_domains
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines.push(
            "Redirect destinations must stay within this allowlist unless the operator grants undeclared public redirects; address-space policy still applies."
                .into(),
        );
        let pyodide = bookclerk_plugin_manifest::PYODIDE_EGRESS_HOSTS;
        let has_pyodide = pyodide
            .iter()
            .any(|h| grant.domains.iter().any(|d| d.eq_ignore_ascii_case(h)));
        if has_pyodide {
            lines.push(format!(
                "Python runtime hosts (Pyodide/CDN): {}",
                pyodide.join(", ")
            ));
        }
    }
    if !grant.tcp.is_empty() {
        let listed: Vec<String> = grant
            .tcp
            .iter()
            .map(|t| {
                let ports = t
                    .ports
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}:[{}]", t.host, ports)
            })
            .collect();
        lines.push(format!("Raw TCP connect: {}", listed.join(", ")));
    }
    if !grant.address_cidrs.is_empty() {
        lines.push(format!(
            "Address-space CIDRs (in addition to the public Internet): {}",
            grant
                .address_cidrs
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if grant.allow_undeclared_public_redirects {
        lines.push(
            "Allow redirects to undeclared public destinations (address-space policy still applies)."
                .into(),
        );
    }
    if !grant.bindings.is_empty() {
        lines.push(format!(
            "Host bindings: {}",
            grant
                .bindings
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let databases = granted_database_bindings(grant);
    if !databases.is_empty() {
        lines.push(format!(
            "Plugin databases (isolated, plugin-owned): {}",
            databases.join(", ")
        ));
    }
    if !grant.compatibility_flags.is_empty() {
        lines.push(format!(
            "Compatibility flags: {}",
            grant
                .compatibility_flags
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(cpu_ms) = grant.cpu_ms {
        lines.push(format!("Workerd CPU limit: {cpu_ms} ms"));
    }
    if let Some(subrequests) = grant.subrequests {
        lines.push(format!("Workerd subrequest limit: {subrequests}"));
    }
    let disk = effective_disk_mib(grant.disk_mib);
    lines.push(format!(
        "Plugin disk budget: {disk} MiB each for data/ and tmp/ (host max {PLUGIN_STATE_BUDGET_MIB_MAX} MiB)"
    ));
    let memory = effective_memory_mib(grant.memory_mib);
    let proc_line = match grant.extra_processes {
        Some(_) => {
            let extra = effective_extra_processes(grant.extra_processes);
            format!(
                "{extra} additional processes/threads \
                 (host max extra {PLUGIN_JAIL_EXTRA_PROCESSES_MAX})"
            )
        }
        None => "process headroom host-managed (workerd launcher + isolate)".into(),
    };
    let cpu_line = match grant.cpu_rate_percent {
        Some(rate) => format!(
            "{} cores (native jail; host max {})",
            format_cpu_cores(percent_to_cores(effective_cpu_rate_percent(Some(rate)))),
            format_cpu_cores(host_cpu_cores_max())
        ),
        None if grant.cpu_ms.is_some() => format!(
            "CPU rate from host jail settings (workerd isolate budget is cpu_ms; host max {} cores)",
            format_cpu_cores(host_cpu_cores_max())
        ),
        None => format!(
            "{} cores (default; host max {})",
            format_cpu_cores(PLUGIN_JAIL_CPU_CORES_DEFAULT),
            format_cpu_cores(host_cpu_cores_max())
        ),
    };
    lines.push(format!(
        "Jail resources: {memory} MiB memory, {cpu_line}, {proc_line} \
         (host max memory {PLUGIN_JAIL_MEMORY_MIB_MAX} MiB)"
    ));
    lines.push(
        "Operator overrides may widen or narrow the manifest request; host hard caps still \
         apply. Bookclerk does not guarantee plugin behaviour if overrides remove capabilities \
         the guest needs."
            .into(),
    );
    lines
}

/// Returns true when a stored network mode can satisfy a requested network mode.
///
/// # Arguments
///
/// * `existing` - Network mode already approved by the operator.
/// * `requested` - Network mode requested by the current plugin manifest.
///
/// # Returns
///
/// `true` when the modes match, or when an existing `deny` grant is stricter
/// than a requested `outbound` grant.
#[must_use]
pub fn network_compatible(existing: &str, requested: &str) -> bool {
    existing.eq_ignore_ascii_case(requested)
        || (existing.eq_ignore_ascii_case("deny") && requested.eq_ignore_ascii_case("outbound"))
}

/// True when an existing grant stays within the manifest request surface.
///
/// Used for **platform auto-grants** (installer envelope). Operator approvals
/// are not limited to this subset — see [`validate_approved_grant`].
#[must_use]
pub fn grant_within_ceiling(existing: &PluginGrant, requested: &PluginGrant) -> bool {
    network_compatible(&existing.network_mode, &requested.network_mode)
        && existing.domains.is_subset(&requested.domains)
        && existing.bindings.is_subset(&requested.bindings)
        && existing
            .compatibility_flags
            .is_subset(&requested.compatibility_flags)
        && consumers_cover(&requested.consumers, &existing.consumers)
        && existing.jobs.is_subset(&requested.jobs)
        && existing.entrypoints.is_subset(&requested.entrypoints)
        && existing.producers.is_subset(&requested.producers)
}

/// True when a stored grant is usable for enable/spawn of this PluginKey.
///
/// Identity match is enough. Extra capabilities on a newer manifest stay
/// pending until the operator approves them; [`effective_grant`] intersects.
#[must_use]
pub fn grant_covers(existing: &PluginGrant, requested: &PluginGrant) -> bool {
    if !existing.plugin_key.is_empty() || !requested.plugin_key.is_empty() {
        return existing.plugin_key == requested.plugin_key;
    }
    existing.plugin_id == requested.plugin_id
}

/// Structural capabilities the current manifest requests that the stored grant
/// does not yet authorize.
#[must_use]
pub fn pending_structural(existing: &PluginGrant, requested: &PluginGrant) -> PendingStructural {
    let entrypoints = requested
        .entrypoints
        .difference(&existing.entrypoints)
        .cloned()
        .collect();
    let producers = requested
        .producers
        .difference(&existing.producers)
        .cloned()
        .collect();
    let jobs = requested.jobs.difference(&existing.jobs).cloned().collect();
    let bindings = requested
        .bindings
        .difference(&existing.bindings)
        .cloned()
        .collect();
    let compatibility_flags = requested
        .compatibility_flags
        .difference(&existing.compatibility_flags)
        .cloned()
        .collect();
    let consumers = requested
        .consumers
        .iter()
        .filter(|req| !existing.consumers.iter().any(|got| got.covers(req)))
        .cloned()
        .collect();
    PendingStructural {
        entrypoints,
        producers,
        consumers,
        jobs,
        bindings,
        compatibility_flags,
    }
}

/// Newly requested structural capabilities awaiting operator consent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingStructural {
    /// Entrypoints declared by the current manifest but not granted.
    pub entrypoints: BTreeSet<String>,
    /// Event producers declared by the current manifest but not granted.
    pub producers: BTreeSet<String>,
    /// Event consumers declared by the current manifest but not granted.
    pub consumers: BTreeSet<GrantedEventConsumer>,
    /// Job triggers declared by the current manifest but not granted.
    pub jobs: BTreeSet<String>,
    /// Host / database bindings declared by the current manifest but not granted.
    pub bindings: BTreeSet<String>,
    /// Compatibility flags declared by the current manifest but not granted.
    pub compatibility_flags: BTreeSet<String>,
}

impl PendingStructural {
    /// True when the current manifest introduces no unapproved structural caps.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entrypoints.is_empty()
            && self.producers.is_empty()
            && self.consumers.is_empty()
            && self.jobs.is_empty()
            && self.bindings.is_empty()
            && self.compatibility_flags.is_empty()
    }

    /// CLI/UI lines for capabilities waiting on consent.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut lines = vec!["Awaiting consent (not automatically approved):".into()];
        if !self.entrypoints.is_empty() {
            lines.push(format!(
                "  entrypoints: {}",
                self.entrypoints
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.producers.is_empty() {
            lines.push(format!(
                "  event producers: {}",
                self.producers
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.consumers.is_empty() {
            lines.push(format!(
                "  event consumers: {}",
                self.consumers
                    .iter()
                    .map(|c| c.event_type.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.jobs.is_empty() {
            lines.push(format!(
                "  jobs: {}",
                self.jobs.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.bindings.is_empty() {
            lines.push(format!(
                "  bindings: {}",
                self.bindings.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.compatibility_flags.is_empty() {
            lines.push(format!(
                "  compatibility flags: {}",
                self.compatibility_flags
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines
    }
}

/// Reconstructs operator network additions/denials relative to the current manifest.
fn classified_operator_network(
    existing: &PluginGrant,
    requested: &PluginGrant,
) -> (BTreeSet<String>, BTreeSet<String>) {
    if !existing.operator_added_domains.is_empty() || !existing.operator_denied_domains.is_empty() {
        return (
            existing.operator_added_domains.clone(),
            existing.operator_denied_domains.clone(),
        );
    }
    let manifest = if requested.manifest_domains.is_empty() {
        &requested.domains
    } else {
        &requested.manifest_domains
    };
    let added = existing.domains.difference(manifest).cloned().collect();
    // Legacy grants without explicit denials: do not treat "not yet stored"
    // manifest hosts as denied — those re-evaluate from the new package.
    let denied = existing.operator_denied_domains.clone();
    (added, denied)
}

/// Effective outbound domains: current manifest ∪ operator additions − denials.
fn merge_network_domains(existing: &PluginGrant, requested: &PluginGrant) -> BTreeSet<String> {
    let (added, denied) = classified_operator_network(existing, requested);
    let mut domains = if requested.manifest_domains.is_empty() {
        requested.domains.clone()
    } else {
        requested.manifest_domains.clone()
    };
    domains.extend(added);
    for d in denied {
        domains.remove(&d);
    }
    domains
}

/// Reconstructs operator TCP additions/denials relative to the current manifest.
fn classified_operator_tcp(
    existing: &PluginGrant,
    requested: &PluginGrant,
) -> (BTreeSet<TcpGrant>, BTreeSet<TcpGrant>) {
    if !existing.operator_added_tcp.is_empty() || !existing.operator_denied_tcp.is_empty() {
        return (
            existing.operator_added_tcp.clone(),
            existing.operator_denied_tcp.clone(),
        );
    }
    let added = existing
        .tcp
        .difference(&requested.manifest_tcp)
        .cloned()
        .collect();
    // Legacy grants without explicit TCP denials: new package hosts re-evaluate.
    let denied = existing.operator_denied_tcp.clone();
    (added, denied)
}

/// Operator-added TCP grants relative to the current manifest.
fn merge_operator_tcp(existing: &PluginGrant, requested: &PluginGrant) -> BTreeSet<TcpGrant> {
    classified_operator_tcp(existing, requested).0
}

/// Effective TCP: current manifest ∪ operator additions − operator denials.
fn merge_tcp(existing: &PluginGrant, requested: &PluginGrant) -> BTreeSet<TcpGrant> {
    let (added, denied) = classified_operator_tcp(existing, requested);
    let mut tcp = requested.manifest_tcp.clone();
    tcp.extend(added);
    for grant in denied {
        tcp.remove(&grant);
    }
    tcp
}

/// Reconstructs operator CIDR additions/denials relative to the current manifest.
fn classified_operator_cidrs(
    existing: &PluginGrant,
    requested: &PluginGrant,
) -> (BTreeSet<String>, BTreeSet<String>) {
    if !existing.operator_added_cidrs.is_empty() || !existing.operator_denied_cidrs.is_empty() {
        return (
            existing.operator_added_cidrs.clone(),
            existing.operator_denied_cidrs.clone(),
        );
    }
    let added = existing
        .address_cidrs
        .difference(&requested.manifest_cidrs)
        .cloned()
        .collect();
    // Legacy grants without explicit CIDR denials: new package CIDRs re-evaluate.
    let denied = existing.operator_denied_cidrs.clone();
    (added, denied)
}

/// Effective CIDRs: current manifest ∪ operator additions − operator denials.
fn merge_cidrs(existing: &PluginGrant, requested: &PluginGrant) -> BTreeSet<String> {
    let (added, denied) = classified_operator_cidrs(existing, requested);
    let mut cidrs = requested.manifest_cidrs.clone();
    cidrs.extend(added);
    for cidr in denied {
        cidrs.remove(&cidr);
    }
    cidrs
}

/// Spawn/delivery grant: stored approval is authoritative, host-normalized.
///
/// Structural capabilities stay at the stored operator snapshot (narrowed).
/// Network destinations re-evaluate the current manifest plus persisted
/// operator additions/denials.
#[must_use]
pub fn effective_grant(existing: &PluginGrant, requested: &PluginGrant) -> PluginGrant {
    let network_mode = if existing.network_mode.is_empty() {
        requested.network_mode.clone()
    } else {
        existing.network_mode.clone()
    };
    let (operator_added_domains, operator_denied_domains) =
        classified_operator_network(existing, requested);
    let (operator_added_cidrs, operator_denied_cidrs) =
        classified_operator_cidrs(existing, requested);
    PluginGrant {
        schema_version: existing.schema_version,
        plugin_key: if existing.plugin_key.is_empty() {
            requested.plugin_key.clone()
        } else {
            existing.plugin_key.clone()
        },
        plugin_id: if existing.plugin_id.is_empty() {
            requested.plugin_id.clone()
        } else {
            existing.plugin_id.clone()
        },
        entrypoints: intersect_structural(&existing.entrypoints, &requested.entrypoints),
        producers: intersect_structural(&existing.producers, &requested.producers),
        consumers: intersect_consumers(&existing.consumers, &requested.consumers),
        jobs: intersect_structural(&existing.jobs, &requested.jobs),
        network_mode,
        domains: merge_network_domains(existing, requested),
        manifest_domains: if requested.manifest_domains.is_empty() {
            requested.domains.clone()
        } else {
            requested.manifest_domains.clone()
        },
        operator_added_domains,
        operator_denied_domains,
        tcp: merge_tcp(existing, requested),
        manifest_tcp: requested.manifest_tcp.clone(),
        operator_added_tcp: merge_operator_tcp(existing, requested),
        operator_denied_tcp: classified_operator_tcp(existing, requested).1,
        address_cidrs: merge_cidrs(existing, requested),
        manifest_cidrs: requested.manifest_cidrs.clone(),
        operator_added_cidrs,
        operator_denied_cidrs,
        allow_undeclared_public_redirects: existing
            .operator_allow_undeclared_public_redirects
            .unwrap_or(requested.allow_undeclared_public_redirects),
        operator_allow_undeclared_public_redirects: existing
            .operator_allow_undeclared_public_redirects,
        bindings: intersect_structural(&existing.bindings, &requested.bindings),
        compatibility_flags: intersect_structural(
            &existing.compatibility_flags,
            &requested.compatibility_flags,
        ),
        // Budgets are operator/stored authority: never auto-widen from a newer
        // manifest when the stored grant left a field unset.
        cpu_ms: normalize_cpu_ms(existing.cpu_ms),
        subrequests: normalize_subrequests(existing.subrequests),
        disk_mib: Some(effective_disk_mib(existing.disk_mib)),
        memory_mib: Some(effective_memory_mib(existing.memory_mib)),
        cpu_rate_percent: existing
            .cpu_rate_percent
            .map(|v| effective_cpu_rate_percent(Some(v))),
        extra_processes: existing
            .extra_processes
            .map(|v| effective_extra_processes(Some(v))),
        approved_at: existing.approved_at.clone(),
    }
}

/// Intersects stored approval with the current manifest request.
///
/// Empty stored sets yield an empty result for every grant schema version.
/// Newly requested members stay pending until the operator approves them.
///
/// # Arguments
///
/// * `existing` - Stored operator-approved set.
/// * `requested` - Current manifest request.
fn intersect_structural<T: Clone + Ord>(
    existing: &BTreeSet<T>,
    requested: &BTreeSet<T>,
) -> BTreeSet<T> {
    existing.intersection(requested).cloned().collect()
}

/// Validates and normalizes an operator-supplied grant against host hard caps.
///
/// The manifest `baseline` supplies identity defaults and suggested values.
/// Operators may **narrow** structural capabilities (entrypoints, producers,
/// host bindings, flags) and **widen or narrow** network destinations.
/// Inventing a structural capability the package did not declare is rejected.
/// Workerd / disk limits clamp to host maximums.
///
/// # Arguments
///
/// * `approved` - Operator-supplied grant draft.
/// * `baseline` - Consent request generated from the current manifest.
///
/// # Returns
///
/// A normalized grant with plugin identity and a fresh `approved_at`.
///
/// # Errors
///
/// Returns [`PluginError`] when identity mismatches, network mode is invalid,
/// a binding is unknown, or a domain pattern cannot be normalized.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
pub fn validate_approved_grant(
    approved: &PluginGrant,
    baseline: &PluginGrant,
) -> Result<PluginGrant> {
    if !approved.plugin_id.is_empty() && approved.plugin_id != baseline.plugin_id {
        return Err(PluginError::message(format!(
            "grant plugin id `{}` does not match `{}`",
            approved.plugin_id, baseline.plugin_id
        )));
    }
    if let Some(extra) = approved
        .entrypoints
        .difference(&baseline.entrypoints)
        .next()
    {
        return Err(PluginError::message(format!(
            "grant entrypoint `{extra}` is not declared by plugin.toml"
        )));
    }
    if let Some(extra) = approved.producers.difference(&baseline.producers).next() {
        return Err(PluginError::message(format!(
            "grant event producer `{extra}` is not declared by plugin.toml"
        )));
    }
    if !consumers_cover(&baseline.consumers, &approved.consumers) {
        return Err(PluginError::message(
            "grant event consumer is not declared by plugin.toml (structural authority)",
        ));
    }
    if let Some(extra) = approved.jobs.difference(&baseline.jobs).next() {
        return Err(PluginError::message(format!(
            "grant job trigger `{extra}` is not declared by plugin.toml (structural authority)"
        )));
    }
    let network_mode = if approved.network_mode.is_empty() {
        baseline.network_mode.clone()
    } else {
        approved.network_mode.clone()
    };
    if !network_mode.eq_ignore_ascii_case("deny") && !network_mode.eq_ignore_ascii_case("outbound")
    {
        return Err(PluginError::message(format!(
            "invalid network mode `{network_mode}` (expected deny or outbound)"
        )));
    }
    for binding in &approved.bindings {
        if let Some(name) = database_binding_name(binding) {
            if !bookclerk_plugin_manifest::is_valid_database_binding_name(name) {
                return Err(PluginError::message(format!(
                    "invalid database binding name `{name}` (expected [A-Z][A-Z0-9_]*)"
                )));
            }
            if !baseline.bindings.contains(binding) {
                return Err(PluginError::message(format!(
                    "grant database binding `{binding}` is not declared by plugin.toml"
                )));
            }
            continue;
        }
        if !KNOWN_HOST_BINDINGS
            .iter()
            .any(|known| binding.eq_ignore_ascii_case(known))
        {
            return Err(PluginError::message(format!(
                "unknown host binding `{binding}`"
            )));
        }
        let known = KNOWN_HOST_BINDINGS
            .iter()
            .find(|known| binding.eq_ignore_ascii_case(known))
            .copied()
            .unwrap_or(binding.as_str());
        if !baseline
            .bindings
            .iter()
            .any(|b| b.eq_ignore_ascii_case(known) || b == binding)
        {
            return Err(PluginError::message(format!(
                "grant binding `{binding}` is not declared by plugin.toml (structural authority)"
            )));
        }
    }
    if let Some(extra) = approved
        .compatibility_flags
        .difference(&baseline.compatibility_flags)
        .next()
    {
        return Err(PluginError::message(format!(
            "grant compatibility flag `{extra}` is not declared by plugin.toml (structural authority)"
        )));
    }
    let mut domains = BTreeSet::new();
    for raw in &approved.domains {
        let Some(normalized) = bookclerk_plugin_manifest::normalize_domain_pattern(raw) else {
            return Err(PluginError::message(format!(
                "invalid domain pattern `{raw}`"
            )));
        };
        domains.insert(normalized);
    }
    let bindings = approved
        .bindings
        .iter()
        .map(|b| {
            if database_binding_name(b).is_some() {
                // Preserve the case-sensitive binding name after the prefix.
                return b.clone();
            }
            KNOWN_HOST_BINDINGS
                .iter()
                .find(|known| b.eq_ignore_ascii_case(known))
                .map(|known| (*known).to_string())
                .unwrap_or_else(|| b.to_ascii_lowercase())
        })
        .collect();
    let compatibility_flags = approved
        .compatibility_flags
        .iter()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect();
    let cpu_ms = approved
        .cpu_ms
        .or(baseline.cpu_ms)
        .map(|raw| normalize_cpu_ms(Some(raw)).expect("Some input returns Some"));
    let subrequests = approved
        .subrequests
        .or(baseline.subrequests)
        .map(|raw| normalize_subrequests(Some(raw)).expect("Some input returns Some"));
    let disk_mib = Some(effective_disk_mib(approved.disk_mib.or(baseline.disk_mib)));
    let memory_mib = Some(effective_memory_mib(
        approved.memory_mib.or(baseline.memory_mib),
    ));
    let cpu_rate_percent = approved
        .cpu_rate_percent
        .or(baseline.cpu_rate_percent)
        .map(|v| effective_cpu_rate_percent(Some(v)));
    let extra_processes = approved
        .extra_processes
        .or(baseline.extra_processes)
        .map(|v| effective_extra_processes(Some(v)));
    Ok(PluginGrant {
        schema_version: GRANT_SCHEMA_VERSION,
        plugin_key: baseline.plugin_key.clone(),
        plugin_id: baseline.plugin_id.clone(),
        entrypoints: approved.entrypoints.clone(),
        producers: approved.producers.clone(),
        consumers: approved.consumers.clone(),
        jobs: approved.jobs.clone(),
        network_mode: network_mode.to_ascii_lowercase(),
        domains: domains.clone(),
        manifest_domains: baseline.domains.clone(),
        operator_added_domains: domains.difference(&baseline.domains).cloned().collect(),
        operator_denied_domains: baseline.domains.difference(&domains).cloned().collect(),
        tcp: approved.tcp.clone(),
        manifest_tcp: baseline.manifest_tcp.clone(),
        operator_added_tcp: approved
            .tcp
            .difference(&baseline.manifest_tcp)
            .cloned()
            .collect(),
        operator_denied_tcp: baseline
            .manifest_tcp
            .difference(&approved.tcp)
            .cloned()
            .collect(),
        address_cidrs: approved.address_cidrs.clone(),
        manifest_cidrs: baseline.manifest_cidrs.clone(),
        operator_added_cidrs: approved
            .address_cidrs
            .difference(&baseline.manifest_cidrs)
            .cloned()
            .collect(),
        operator_denied_cidrs: baseline
            .manifest_cidrs
            .difference(&approved.address_cidrs)
            .cloned()
            .collect(),
        allow_undeclared_public_redirects: approved.allow_undeclared_public_redirects,
        operator_allow_undeclared_public_redirects: if approved.allow_undeclared_public_redirects
            != baseline.allow_undeclared_public_redirects
        {
            Some(approved.allow_undeclared_public_redirects)
        } else {
            approved.operator_allow_undeclared_public_redirects
        },
        bindings,
        compatibility_flags,
        cpu_ms,
        subrequests,
        disk_mib,
        memory_mib,
        cpu_rate_percent,
        extra_processes,
        approved_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Clamps an optional CPU budget through [`WorkerdLimits::effective`].
fn normalize_cpu_ms(value: Option<u32>) -> Option<u32> {
    value.map(|cpu_ms| {
        WorkerdLimits {
            cpu_ms: Some(cpu_ms),
            subrequests: None,
        }
        .effective()
        .cpu_ms
    })
}

/// Clamps an optional subrequest budget through [`WorkerdLimits::effective`].
fn normalize_subrequests(value: Option<u32>) -> Option<u32> {
    value.map(|subrequests| {
        WorkerdLimits {
            cpu_ms: None,
            subrequests: Some(subrequests),
        }
        .effective()
        .subrequests
    })
}

/// Resolved disk budget in MiB (default + host clamp).
#[must_use]
pub fn effective_disk_mib(value: Option<u32>) -> u32 {
    value
        .unwrap_or(PLUGIN_STATE_BUDGET_MIB_DEFAULT)
        .clamp(1, PLUGIN_STATE_BUDGET_MIB_MAX)
}

/// Resolved jail Spec memory ceiling in MiB.
#[must_use]
pub fn effective_memory_mib(value: Option<u32>) -> u32 {
    value
        .unwrap_or(PLUGIN_JAIL_MEMORY_MIB_DEFAULT)
        .clamp(1, PLUGIN_JAIL_MEMORY_MIB_MAX)
}

/// Resolved jail Spec CPU rate as percent of one logical CPU.
///
/// Clamped to `1..=`[`host_cpu_rate_max`] (100 × logical CPUs).
#[must_use]
pub fn effective_cpu_rate_percent(value: Option<u32>) -> u32 {
    value
        .unwrap_or(PLUGIN_JAIL_CPU_RATE_DEFAULT)
        .clamp(1, host_cpu_rate_max())
}

/// Host CPU rate ceiling in one-core percent units (`logical_cpus × 100`).
#[must_use]
pub fn host_cpu_rate_max() -> u32 {
    bookclerk_sandbox::host_cpu_rate_max()
}

/// Logical CPUs visible to this process (at least 1).
#[must_use]
pub fn host_logical_cpus() -> u32 {
    bookclerk_sandbox::host_logical_cpus()
}

/// Format cores for operator-facing text (`0.80`).
#[must_use]
pub fn format_cpu_cores(cores: f64) -> String {
    format!("{:.2}", effective_cpu_cores(Some(cores)))
}

/// Resolved disk budget in bytes for `data/` and `tmp/` checks.
#[must_use]
pub fn effective_disk_budget_bytes(grant: Option<&PluginGrant>) -> u64 {
    u64::from(effective_disk_mib(grant.and_then(|g| g.disk_mib))) * 1024 * 1024
}

/// Require an operator grant before enable **or** every external spawn.
///
/// # Arguments
///
/// * `files_dir` - Bookclerk files directory containing `plugin-grants.json`.
/// * `manifest` - Plugin manifest whose id must have a stored grant.
///
/// # Returns
///
/// Effective [`PluginGrant`] (operator approval, host-normalized).
///
/// # Errors
///
/// Returns [`PluginError`] when no grant exists for the plugin id.
pub fn require_grant(
    files_dir: &Path,
    plugin: &crate::discover::DiscoveredPlugin,
) -> Result<PluginGrant> {
    let store = PluginGrantStore::load(files_dir)?;
    let requested = consent_request(&plugin.manifest, plugin.plugin_key());
    let existing = store.get_by_plugin_key(plugin.plugin_key().canonical());
    match existing {
        Some(existing) if grant_covers(existing, &requested) => {
            Ok(effective_grant(existing, &requested))
        }
        Some(_) => Err(PluginError::message(format!(
            "plugin `{}` grant does not match this plugin; re-approve with `bookclerk plugins approve {}`",
            plugin.alias(), plugin.alias()
        ))),
        None => Err(PluginError::message(format!(
            "plugin `{}` has no permission grant; run `bookclerk plugins approve {}` first",
            plugin.alias(), plugin.alias()
        ))),
    }
}

/// True when `grant.bindings` contains `name` (case-insensitive).
#[must_use]
pub fn grant_has_binding(grant: &PluginGrant, name: &str) -> bool {
    grant
        .bindings
        .iter()
        .any(|binding| binding.eq_ignore_ascii_case(name))
}

/// Maps a `describe()` env binding name onto the persisted grant token.
///
/// Host bindings (`CONFIG`, `[secrets]`, `[work_fs]`, `[oauth]`, KV) are
/// authorized through [`PluginGrant::bindings`]. Producer `EVENTS` names are
/// authorized through [`PluginGrant::producers`] instead.
fn grant_binding_for_described(manifest: &PluginManifest, env_name: &str) -> Option<&'static str> {
    use bookclerk_plugin_manifest::{
        CONFIG_BINDING, DEFAULT_KV_BINDING, DEFAULT_OAUTH_BINDING, DEFAULT_SECRETS_BINDING,
        DEFAULT_WORK_FS_BINDING,
    };
    if env_name == CONFIG_BINDING {
        return Some("config");
    }
    if manifest
        .secrets
        .as_ref()
        .is_some_and(|binding| env_name == binding.name_or(DEFAULT_SECRETS_BINDING))
    {
        return Some("secrets");
    }
    if manifest
        .work_fs
        .as_ref()
        .is_some_and(|binding| env_name == binding.name_or(DEFAULT_WORK_FS_BINDING))
    {
        return Some("work_fs");
    }
    if manifest
        .oauth
        .as_ref()
        .is_some_and(|binding| env_name == binding.name_or(DEFAULT_OAUTH_BINDING))
    {
        return Some("oauth");
    }
    if manifest
        .kv_namespaces
        .iter()
        .any(|kv| env_name == kv.name_or(DEFAULT_KV_BINDING))
    {
        return Some("plugin_kv");
    }
    None
}

/// Fail closed when a delivery site needs a binding the covering grant lacks.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn require_binding(grant: &PluginGrant, name: &str) -> Result<()> {
    if grant_has_binding(grant, name) {
        Ok(())
    } else {
        Err(PluginError::message(format!(
            "plugin `{}` grant lacks binding `{name}`; re-approve with `bookclerk plugins approve {}`",
            grant.plugin_id, grant.plugin_id
        )))
    }
}

/// Spawn `config` payload: non-empty settings only when the grant includes `config`.
#[must_use]
pub fn spawn_config_for_grant(
    grant: &PluginGrant,
    config_table: serde_json::Value,
) -> serde_json::Value {
    if grant_has_binding(grant, "config") {
        config_table
    } else {
        serde_json::json!({})
    }
}

/// Verified Bookclerk platform artifacts skip the consent UX when the
/// installed payload still matches the host-stamped receipt and the request
/// stays in the installer envelope (deny network, only `config` / `work_fs`).
pub fn ensure_platform_grant(
    files_dir: &Path,
    plugin: &crate::discover::DiscoveredPlugin,
) -> Result<PluginGrant> {
    if !plugin.identity.provenance.grants_platform_defaults()
        || !bookclerk_plugin_catalog::is_platform_plugin_key(plugin.plugin_key())
    {
        return require_grant(files_dir, plugin);
    }
    let requested = consent_request(&plugin.manifest, plugin.plugin_key());
    if !is_safe_platform_request(&requested) {
        return Err(PluginError::message(format!(
            "platform plugin `{}` declares capabilities outside the installer envelope; \
             run `bookclerk plugins approve {}`",
            plugin.alias(),
            plugin.alias()
        )));
    }
    let mut store = PluginGrantStore::load(files_dir)?;
    match store.get_by_plugin_key(plugin.plugin_key().canonical()) {
        Some(existing) if grant_within_ceiling(existing, &requested) => {
            Ok(effective_grant(existing, &requested))
        }
        Some(_) | None => {
            store.upsert(requested.clone());
            store.save(files_dir)?;
            Ok(requested)
        }
    }
}

/// Resolve the covering grant for spawn: verified platform auto-grant, else [`require_grant`].
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn spawn_grant(
    files_dir: &Path,
    plugin: &crate::discover::DiscoveredPlugin,
) -> Result<PluginGrant> {
    if plugin.identity.provenance.grants_platform_defaults()
        && bookclerk_plugin_catalog::is_platform_plugin_key(plugin.plugin_key())
    {
        ensure_platform_grant(files_dir, plugin)
    } else {
        require_grant(files_dir, plugin)
    }
}

/// Injects effective-grant overrides for `bookclerk-workerd` into a guest command.
///
/// Must be called after `env_clear` (explicit `BOOKCLERK_*` is blocked from
/// inheritance). Domains are always set (possibly empty) so a subset approval
/// cannot fall back to the full manifest allowlist.
///
/// # Arguments
///
/// * `cmd` - Guest process command being prepared for spawn.
/// * `grant` - Effective covering grant for this plugin.
pub fn inject_workerd_grant_env(cmd: &mut Command, grant: &PluginGrant) {
    cmd.env(WORKERD_GRANT_NETWORK_MODE_ENV, &grant.network_mode);
    let domains = grant.domains.iter().cloned().collect::<Vec<_>>().join(",");
    cmd.env(WORKERD_GRANT_DOMAINS_ENV, domains);
    if let Some(cpu_ms) = grant.cpu_ms {
        cmd.env(WORKERD_GRANT_CPU_MS_ENV, cpu_ms.to_string());
    }
    if let Some(subrequests) = grant.subrequests {
        cmd.env(WORKERD_GRANT_SUBREQUESTS_ENV, subrequests.to_string());
    }
    if let Ok(policy) = serde_json::to_string(&grant.egress_policy()) {
        cmd.env(WORKERD_GRANT_POLICY_ENV, policy);
    }
}

/// Overlay TCP + CIDR implied by host configuration (not persisted).
///
/// Not operator-invented structural authority. The operator already configured
/// the destination URL; spawn injects matching `EgressPolicy` so the nested
/// native guest can `connect()` it. Covers:
///
/// - active Postgres `[database.postgres].url`
/// - active D1 `[database.d1].api_base`
/// - Audiobookshelf `[integrations.audiobookshelf].base_url`
/// - S3 `[output.s3].endpoint` (custom/MinIO)
///
/// Occupancy is uniquified with [`crate::resolve_plugin_slot`]: a bare alias
/// overlays only the unique occupant, and a PluginKey never overlays a twin.
///
/// No-op unless the covering grant is outbound.
///
/// # Arguments
///
/// * `grant` - Effective covering grant (mutated in place).
/// * `plugin` - Guest being spawned.
/// * `config` - Host config (URL already env-applied).
/// * `discovered` - Installs used to uniquify occupancy (must include `plugin`).
pub fn overlay_host_implied_network(
    grant: &mut PluginGrant,
    plugin: &crate::discover::DiscoveredPlugin,
    config: &Config,
    discovered: &[crate::discover::DiscoveredPlugin],
) {
    if !grant.network_mode.eq_ignore_ascii_case("outbound") {
        return;
    }
    overlay_postgres_url(grant, plugin, config, discovered);
    overlay_d1_api_base(grant, plugin, config, discovered);
    overlay_audiobookshelf_url(grant, plugin, config, discovered);
    overlay_s3_endpoint(grant, plugin, config, discovered);
}

/// SHA-256 of host config that participates in implied network overlays.
///
/// Changing a Postgres URL, D1 API origin, S3 endpoint, or Audiobookshelf URL
/// must mint a new executor `configuration_revision` and fence live sessions.
#[must_use]
pub fn host_overlay_config_digest(config: &Config) -> String {
    let mut hasher = Sha256::new();
    hasher.update(config.database.plugin.as_bytes());
    hasher.update(b"\npostgres\n");
    hasher.update(
        config
            .database
            .postgres
            .url
            .as_deref()
            .unwrap_or("")
            .as_bytes(),
    );
    hasher.update(b"\n");
    if let Some(path) = &config.database.postgres.url_file {
        hasher.update(path.to_string_lossy().as_bytes());
    }
    hasher.update(b"\nd1\n");
    hasher.update(config.database.d1.api_base.as_bytes());
    hasher.update(b"\n");
    hasher.update(config.database.d1.account_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(config.database.d1.database_id.as_bytes());
    hasher.update(b"\ns3\n");
    hasher.update(config.output.s3.plugin.as_bytes());
    hasher.update(b"\n");
    hasher.update(
        config
            .output
            .s3
            .endpoint
            .as_deref()
            .unwrap_or("")
            .as_bytes(),
    );
    hasher.update(b"\nabs\n");
    hasher.update(config.integrations.occupancy("audiobookshelf").as_bytes());
    hasher.update(b"\n");
    hasher.update(config.integrations.audiobookshelf().base_url.as_bytes());
    hex::encode(hasher.finalize())
}

/// Recomputes effective overlay authority and fences sessions that no longer match.
///
/// Host-config destination changes do not touch `plugin-grants.json`, so the
/// grant watcher cannot see them. Config reload must call this.
pub fn reconcile_host_overlay_authority(config: &Config) {
    let discovered = match crate::discover_plugins(config) {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(error = %err, "cannot reconcile overlay authority without discovery");
            return;
        }
    };
    let store = match PluginGrantStore::load(&config.paths().files_dir) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(error = %err, "cannot reconcile overlay authority without grants");
            return;
        }
    };
    for (key, session_rev) in crate::authority::live_authority_snapshot() {
        let Some(plugin) = discovered
            .iter()
            .find(|plugin| plugin.plugin_key().canonical() == key)
        else {
            continue;
        };
        let Some(persisted) = store.get_by_plugin_key(&key) else {
            crate::authority::fence_stale_sessions(&key, "");
            continue;
        };
        let mut effective = persisted.clone();
        overlay_host_implied_network(&mut effective, plugin, config, &discovered);
        let current = crate::authority::authority_revision(&effective);
        if current != session_rev {
            crate::authority::fence_stale_sessions(&key, &current);
        }
    }
}

/// True when `plugin` is the unique occupant of `spec` among `discovered`.
fn overlay_unique_occupant(
    plugin: &crate::discover::DiscoveredPlugin,
    spec: &str,
    discovered: &[crate::discover::DiscoveredPlugin],
) -> bool {
    matches!(
        crate::discover::resolve_plugin_slot(discovered, spec),
        Ok(Some(occupant)) if occupant.plugin_key() == plugin.plugin_key()
    )
}

/// TCP host/port a Postgres URL would dial (`None` for Unix-socket URLs).
///
/// Query `host` / `hostaddr` / `port` override the URL authority. A `host`
/// that starts with `/` is a libpq directory for `.s.PGSQL.{port}`. Copied
/// into the host so production does not link the postgres adapter crate.
#[must_use]
pub(crate) fn postgres_tcp_target(url: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(url).ok()?;
    let mut host = parsed
        .host_str()
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    let mut port = parsed.port().unwrap_or(5432);
    let mut unix_socket = parsed
        .host_str()
        .is_some_and(|h| h.starts_with('/') || h.starts_with("%2F") || h.starts_with("%2f"));
    for (key, value) in parsed.query_pairs() {
        match &*key {
            "host" if value.starts_with('/') => unix_socket = true,
            "host" => host = Some(value.into_owned()),
            "hostaddr" => host = Some(value.into_owned()),
            "port" => port = value.parse().ok()?,
            _ => {}
        }
    }
    if unix_socket {
        return None;
    }
    Some((host?, port))
}

/// Overlay TCP for the active Postgres URL (host-owned, not persisted).
fn overlay_postgres_url(
    grant: &mut PluginGrant,
    plugin: &crate::discover::DiscoveredPlugin,
    config: &Config,
    discovered: &[crate::discover::DiscoveredPlugin],
) {
    if !overlay_database_slot_matches(plugin, config, DatabasePluginKind::Postgres, discovered) {
        return;
    }
    let Ok(url) = resolve_postgres_url(config) else {
        return;
    };
    let Some((host, port)) = postgres_tcp_target(&url) else {
        return;
    };
    overlay_tcp_host(grant, &host, port);
}

/// Overlay TCP for `[database.d1].api_base` when this guest is the active D1 plugin.
fn overlay_d1_api_base(
    grant: &mut PluginGrant,
    plugin: &crate::discover::DiscoveredPlugin,
    config: &Config,
    discovered: &[crate::discover::DiscoveredPlugin],
) {
    if !overlay_database_slot_matches(plugin, config, DatabasePluginKind::D1, discovered) {
        return;
    }
    overlay_http_url(grant, &config.database.d1.api_base, 443);
}

/// True when `plugin` is the selected `[database].plugin` occupant of `kind`.
///
/// A provenance-qualified PluginKey matches only that key. Kind tokens
/// (`postgres`, `pg`, `d1`) resolve through [`crate::resolve_plugin_slot`] so
/// corrupt duplicate aliases fail closed instead of inheriting the operator URL.
fn overlay_database_slot_matches(
    plugin: &crate::discover::DiscoveredPlugin,
    config: &Config,
    kind: DatabasePluginKind,
    discovered: &[crate::discover::DiscoveredPlugin],
) -> bool {
    if crate::first_party_database_kind(plugin) != Some(kind) {
        return false;
    }
    let spec = config.database.plugin.trim();
    if spec.is_empty() {
        return false;
    }
    if bookclerk_plugin_catalog::PluginKey::parse(spec).is_ok() {
        return overlay_unique_occupant(plugin, spec, discovered);
    }
    let kind_id = DatabasePluginKind::parse(spec).map(DatabasePluginKind::as_str);
    overlay_unique_occupant(plugin, kind_id.unwrap_or(spec), discovered)
}

/// Overlay TCP for `[integrations.audiobookshelf].base_url`.
fn overlay_audiobookshelf_url(
    grant: &mut PluginGrant,
    plugin: &crate::discover::DiscoveredPlugin,
    config: &Config,
    discovered: &[crate::discover::DiscoveredPlugin],
) {
    if !plugin.alias().eq_ignore_ascii_case("audiobookshelf") {
        return;
    }
    let spec = crate::occupancy_spec(
        config.integrations.occupancy("audiobookshelf"),
        "audiobookshelf",
    );
    if !overlay_unique_occupant(plugin, spec, discovered) {
        return;
    }
    overlay_http_url(grant, &config.integrations.audiobookshelf().base_url, 443);
}

/// Overlay TCP for `[output.s3].endpoint` (MinIO / custom). Default AWS hosts
/// come from the S3 guest manifest `tcp` list.
fn overlay_s3_endpoint(
    grant: &mut PluginGrant,
    plugin: &crate::discover::DiscoveredPlugin,
    config: &Config,
    discovered: &[crate::discover::DiscoveredPlugin],
) {
    if !plugin.alias().eq_ignore_ascii_case("s3") {
        return;
    }
    let spec = crate::occupancy_spec(&config.output.s3.plugin, "s3");
    if !overlay_unique_occupant(plugin, spec, discovered) {
        return;
    }
    let Some(endpoint) = config.output.s3.endpoint.as_deref() else {
        return;
    };
    overlay_http_url(grant, endpoint, 443);
}

/// Parses an HTTP(S) URL or bare host and grants TCP + implied CIDRs.
fn overlay_http_url(grant: &mut PluginGrant, raw: &str, default_port: u16) {
    let Some((host, port)) = tcp_target_from_http_url(raw, default_port) else {
        return;
    };
    overlay_tcp_host(grant, &host, port);
}

/// Grants TCP for `host:port` plus loopback/private CIDRs when `host` is not public.
fn overlay_tcp_host(grant: &mut PluginGrant, host: &str, port: u16) {
    add_tcp_grant(grant, host, port);
    for cidr in implied_cidrs_for_host(host) {
        grant.address_cidrs.insert(cidr);
    }
}

/// Host + port for an HTTP(S) URL or a scheme-less host[:port].
fn tcp_target_from_http_url(raw: &str, default_port: u16) -> Option<(String, u16)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(trimmed)
        .ok()
        .or_else(|| url::Url::parse(&format!("https://{trimmed}")).ok())?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(default_port);
    Some((host, port))
}

/// Inserts `host:port` into the effective TCP grant set (merging ports).
fn add_tcp_grant(grant: &mut PluginGrant, host: &str, port: u16) {
    let Some(host) = normalize_domain_pattern(host) else {
        return;
    };
    let mut ports = grant
        .tcp
        .iter()
        .find(|t| t.host == host)
        .map(|t| t.ports.clone())
        .unwrap_or_default();
    grant.tcp.retain(|t| t.host != host);
    if !ports.contains(&port) {
        ports.push(port);
        ports.sort_unstable();
    }
    grant.tcp.insert(TcpGrant { host, ports });
}

/// CIDRs the socket proxy needs beyond the public Internet for `host`.
fn implied_cidrs_for_host(host: &str) -> Vec<String> {
    if is_restricted_hostname(host) {
        return vec!["127.0.0.1/32".into(), "::1/128".into()];
    }
    let Ok(ip) = host.parse::<IpAddr>() else {
        return Vec::new();
    };
    if is_public_internet(ip) {
        return Vec::new();
    }
    match ip {
        IpAddr::V4(_) => vec![format!("{ip}/32")],
        IpAddr::V6(_) => vec![format!("{ip}/128")],
    }
}

/// Canonical SHA-256 of a grant's security-relevant fields (not presentation).
#[must_use]
pub fn grant_revision(grant: &PluginGrant) -> String {
    crate::authority::grant_revision(grant)
}

/// True when a platform grant is deny-network with only `config` / `work_fs` bindings.
fn is_safe_platform_request(grant: &PluginGrant) -> bool {
    grant.network_mode == "deny"
        && grant.domains.is_empty()
        && grant.tcp.is_empty()
        && grant.address_cidrs.is_empty()
        && !grant.allow_undeclared_public_redirects
        && grant.compatibility_flags.is_empty()
        && grant
            .bindings
            .iter()
            .all(|b| b == "config" || b == "work_fs")
}

/// Reject `describe()` capability claims that exceed the manifest and the
/// covering grant.
///
/// The guest's typed [`PluginCapabilities`] must not widen what
/// `plugin.toml` declares: no extra entrypoints, event consumers, producers,
/// job types, database bindings, or named bindings. Every advertised
/// entrypoint and producer must also be present in the operator grant, and
/// OAuth storefronts need the `oauth` binding.
///
/// # Arguments
///
/// * `manifest` - Installed manifest for the plugin.
/// * `grant` - Effective covering grant.
/// * `described` - Capability block the guest returned from `describe()`.
/// * `portal_auth_mode` - Storefront connect mode from `describe()`.
///
/// # Errors
///
/// Returns an error naming the first widening capability or missing grant.
pub fn validate_described_capabilities(
    manifest: &PluginManifest,
    grant: &PluginGrant,
    described: &PluginCapabilities,
    portal_auth_mode: PortalAuthMode,
) -> Result<()> {
    let declared = manifest.capabilities();
    let id = &manifest.id;

    for entrypoint in &described.entrypoints {
        if !declared.entrypoints.contains(entrypoint) {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe() exports entrypoint `{}` not declared in plugin.toml",
                entrypoint.wire_name()
            )));
        }
        if !grant.entrypoints.contains(entrypoint.wire_name()) {
            return Err(PluginError::message(format!(
                "plugin `{id}` grant lacks entrypoint `{}`; re-approve with \
                 `bookclerk plugins approve {id}`",
                entrypoint.wire_name()
            )));
        }
    }
    for consumer in &described.consumes {
        let Some(manifest_consumer) = declared
            .consumes
            .iter()
            .find(|c| c.event_type == consumer.event_type)
        else {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe() consumes event `{}` not declared in plugin.toml",
                consumer.event_type
            )));
        };
        if consumer
            .schema_versions
            .iter()
            .any(|v| !manifest_consumer.schema_versions.contains(v))
            || (consumer.supports_suspend && !manifest_consumer.supports_suspend)
        {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe() widens event consumer `{}` beyond plugin.toml",
                consumer.event_type
            )));
        }
        if !grant.consumers.iter().any(|c| {
            c.event_type == consumer.event_type
                && consumer
                    .schema_versions
                    .iter()
                    .all(|v| c.schema_versions.contains(v))
                && (!consumer.supports_suspend || c.supports_suspend)
        }) {
            return Err(PluginError::message(format!(
                "plugin `{id}` grant lacks event consumer `{}`; re-approve with \
                 `bookclerk plugins approve {id}`",
                consumer.event_type
            )));
        }
    }
    for producer in &described.produces {
        if !declared.produces.contains(producer) {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe() produces event `{producer}` not declared in plugin.toml"
            )));
        }
        if !grant.producers.contains(producer) {
            return Err(PluginError::message(format!(
                "plugin `{id}` grant lacks event producer `{producer}`; re-approve with \
                 `bookclerk plugins approve {id}`"
            )));
        }
    }
    for job in &described.jobs {
        if !declared.jobs.contains(job) {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe() runs job `{job}` not declared in plugin.toml"
            )));
        }
        if !grant.jobs.contains(job) {
            return Err(PluginError::message(format!(
                "plugin `{id}` grant lacks job trigger `{job}`; re-approve with \
                 `bookclerk plugins approve {id}`"
            )));
        }
    }
    for database in &described.databases {
        if !declared.databases.contains(database) {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe() binds database `{database}` not declared in plugin.toml"
            )));
        }
        require_binding(grant, &format!("{DATABASE_BINDING_PREFIX}{database}"))?;
    }
    for binding in &described.bindings {
        if !declared.bindings.contains(binding) {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe() expects binding `{binding}` not declared in plugin.toml"
            )));
        }
        if let Some(grant_name) = grant_binding_for_described(manifest, binding) {
            require_binding(grant, grant_name)?;
        }
    }
    if portal_auth_mode == PortalAuthMode::Oauth {
        if !manifest.bindings().oauth {
            return Err(PluginError::message(format!(
                "plugin `{id}` describe() advertises OAuth without `[oauth]` in plugin.toml"
            )));
        }
        require_binding(grant, "oauth")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::DiscoveredPlugin;
    use crate::manifest::PluginManifest;
    use bookclerk_plugin_catalog::PluginProvenance;

    fn discovered(root: &std::path::Path, toml: &str) -> DiscoveredPlugin {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("plugin.toml"), toml).unwrap();
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").unwrap();
        let manifest = PluginManifest::parse(toml).unwrap();
        DiscoveredPlugin::try_new(manifest, root.to_path_buf(), command, None)
            .expect("test plugin tree must evaluate")
    }

    fn first_party_database(
        root: &std::path::Path,
        package: &str,
        id: &str,
        toml: &str,
    ) -> DiscoveredPlugin {
        let mut plugin = discovered(root, toml);
        let key = bookclerk_plugin_catalog::PluginKey::platform(package, id).unwrap();
        plugin.identity.plugin_key = key.clone();
        plugin.identity.artifact.plugin_key = key;
        plugin.identity.provenance = PluginProvenance::VerifiedInstalled;
        plugin
    }

    fn stamped_sqlite(files: &std::path::Path) -> DiscoveredPlugin {
        let key = bookclerk_plugin_catalog::PluginKey::platform(
            "bookclerk-plugin-database-sqlite",
            "sqlite",
        )
        .unwrap();
        let root = files.join("plugins").join(key.fs_id());
        let plugin = discovered(
            &root,
            r#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"

[vars]

[work_fs]
"#,
        );
        bookclerk_plugin_catalog::stamp_platform_receipt(
            &plugin.root,
            files,
            "bookclerk-plugin-database-sqlite",
            &plugin.manifest,
            "0.0.0",
        )
        .expect("stamp");
        DiscoveredPlugin::try_new(plugin.manifest, plugin.root, plugin.command, Some(files))
            .expect("stamped platform plugin")
    }

    fn sample_grant(domains: &[&str], bindings: &[&str], flags: &[&str]) -> PluginGrant {
        PluginGrant {
            schema_version: GRANT_SCHEMA_VERSION,
            plugin_key: String::new(),
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
            bindings: bindings.iter().map(|s| (*s).to_string()).collect(),
            compatibility_flags: flags.iter().map(|s| (*s).to_string()).collect(),
            cpu_ms: None,
            subrequests: None,
            disk_mib: None,
            memory_mib: None,
            cpu_rate_percent: None,
            extra_processes: None,
            approved_at: "2026-01-01T00:00:00Z".into(),
            ..PluginGrant::empty()
        }
    }

    #[test]
    fn grant_store_absent_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = PluginGrantStore::load(dir.path()).unwrap();
        assert!(store.grants.is_empty());
    }

    #[test]
    fn grant_store_round_trip_under_parent_dir_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("files");
        std::fs::create_dir_all(&nested).unwrap();
        let with_dotdot = nested.join("..").join("files");
        let mut store = PluginGrantStore::default();
        store.upsert(sample_grant(&["a.example"], &["config"], &[]));
        store.save(&with_dotdot).unwrap();
        let loaded = PluginGrantStore::load(&with_dotdot).unwrap();
        assert_eq!(loaded.grants.len(), 1);
        assert!(loaded.grants[0].domains.contains("a.example"));
    }

    #[cfg(unix)]
    #[test]
    fn grant_store_load_refuses_escaping_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("stolen.json");
        std::fs::write(&victim, b"{\"grants\":[]}").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(GRANTS_FILE)).unwrap();
        let err = PluginGrantStore::load(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("escape")
                || err.to_string().contains("symlink")
                || err.to_string().contains("refusing")
                || err.to_string().contains("path"),
            "{err}"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"{\"grants\":[]}");
    }

    #[cfg(unix)]
    #[test]
    fn grant_store_load_refuses_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("missing.json"),
            dir.path().join(GRANTS_FILE),
        )
        .unwrap();
        let err = PluginGrantStore::load(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("symlink")
                || err.to_string().contains("dangling")
                || err.to_string().contains("refusing")
                || err.to_string().contains("canonicalize"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn grant_store_save_replaces_final_symlink_without_touching_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("grants-victim");
        std::fs::write(&victim, b"keep-grants").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(GRANTS_FILE)).unwrap();
        let mut store = PluginGrantStore::default();
        store.upsert(sample_grant(&["b.example"], &["config"], &[]));
        store.save(dir.path()).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep-grants");
        let loaded = PluginGrantStore::load(dir.path()).unwrap();
        assert!(loaded.grants[0].domains.contains("b.example"));
    }

    #[cfg(unix)]
    #[test]
    fn grant_store_save_does_not_follow_temp_symlink_name() {
        // Unique staging names mean a fixed `.json.tmp` plant is not used; planting a
        // symlink at a guessed staging path must not be truncated/followed via create_new.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("tmp-victim");
        std::fs::write(&victim, b"tmp-keep").unwrap();
        // Pre-plant the legacy fixed temp name used by the old writer.
        let legacy_tmp = dir.path().join("plugin-grants.json.tmp");
        std::os::unix::fs::symlink(&victim, &legacy_tmp).unwrap();
        let mut store = PluginGrantStore::default();
        store.upsert(sample_grant(&["c.example"], &["config"], &[]));
        store.save(dir.path()).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"tmp-keep");
        assert!(
            std::fs::symlink_metadata(&legacy_tmp)
                .unwrap()
                .file_type()
                .is_symlink(),
            "legacy temp plant must remain untouched"
        );
        let loaded = PluginGrantStore::load(dir.path()).unwrap();
        assert!(loaded.grants[0].domains.contains("c.example"));
    }

    #[test]
    fn grant_covers_allows_operator_domain_subset() {
        let existing = sample_grant(&["a.example"], &["config"], &[]);
        let requested = sample_grant(&["a.example", "b.example"], &["config"], &[]);
        assert!(grant_covers(&existing, &requested));
    }

    #[test]
    fn grant_covers_allows_new_structural_caps_as_pending() {
        let existing = sample_grant(&["a.example"], &["config"], &[]);
        let requested = sample_grant(&["a.example"], &["config", "secrets"], &[]);
        assert!(grant_covers(&existing, &requested));
        let pending = pending_structural(&existing, &requested);
        assert!(pending.bindings.contains("secrets"));
        let effective = effective_grant(&existing, &requested);
        assert!(!effective.bindings.contains("secrets"));
        assert!(effective.bindings.contains("config"));
    }

    #[test]
    fn grant_covers_keeps_identity_when_manifest_adds_consumer() {
        let existing = sample_grant(&[], &["config"], &[]);
        let mut requested = existing.clone();
        requested.consumers.insert(GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: None,
        });
        assert!(grant_covers(&existing, &requested));
        let pending = pending_structural(&existing, &requested);
        assert_eq!(pending.consumers.len(), 1);
        let effective = effective_grant(&existing, &requested);
        assert!(effective.consumers.is_empty());
        let approved = requested.clone();
        assert!(grant_covers(&approved, &requested));
        assert!(pending_structural(&approved, &requested).is_empty());
    }

    #[test]
    fn grant_covers_keeps_identity_when_manifest_adds_job() {
        let existing = sample_grant(&[], &["config"], &[]);
        let mut requested = existing.clone();
        requested.jobs.insert("stream_copy".into());
        assert!(grant_covers(&existing, &requested));
        assert!(pending_structural(&existing, &requested)
            .jobs
            .contains("stream_copy"));
        assert!(effective_grant(&existing, &requested).jobs.is_empty());
        assert!(grant_covers(&requested, &requested));
    }

    #[test]
    fn adding_consumer_or_job_changes_authority_revision() {
        let base = sample_grant(&[], &["config"], &[]);
        let mut with_consumer = base.clone();
        with_consumer.consumers.insert(GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: true,
            filter: None,
        });
        assert_ne!(grant_revision(&base), grant_revision(&with_consumer));
        let mut with_job = base.clone();
        with_job.jobs.insert("stream_copy".into());
        assert_ne!(grant_revision(&base), grant_revision(&with_job));
        assert_ne!(grant_revision(&with_consumer), grant_revision(&with_job));
    }

    #[test]
    fn validate_approved_grant_rejects_invented_consumer_and_job() {
        let baseline = sample_grant(&[], &["config"], &[]);
        let mut steal = baseline.clone();
        steal.consumers.insert(GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: None,
        });
        let err = validate_approved_grant(&steal, &baseline)
            .unwrap_err()
            .to_string();
        assert!(err.contains("consumer"), "{err}");
        let mut steal_job = baseline.clone();
        steal_job.jobs.insert("stream_copy".into());
        let err = validate_approved_grant(&steal_job, &baseline)
            .unwrap_err()
            .to_string();
        assert!(err.contains("job trigger"), "{err}");
    }

    #[test]
    fn consent_request_includes_consumers_and_jobs() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[[events.consumers]]
type = "book_acquired"
schema_versions = [1]
supports_suspend = true

[triggers]
jobs = ["stream_copy"]
"#,
        )
        .unwrap();
        let grant = consent_request_alias(&manifest);
        assert!(grant
            .consumers
            .iter()
            .any(|c| c.event_type == "book_acquired"
                && c.schema_versions == vec![1]
                && c.supports_suspend));
        assert!(grant.jobs.contains("stream_copy"));
        let summary = consent_summary(&grant);
        assert!(
            summary.iter().any(|l| l.contains("Consumes events")),
            "{summary:?}"
        );
        assert!(
            summary.iter().any(|l| l.contains("Job triggers")),
            "{summary:?}"
        );
    }

    #[test]
    fn grant_covers_treats_new_flag_as_pending() {
        let existing = sample_grant(&[], &[], &["nodejs_compat"]);
        let requested = sample_grant(&[], &[], &["nodejs_compat", "streams_enable_constructors"]);
        assert!(grant_covers(&existing, &requested));
        assert!(pending_structural(&existing, &requested)
            .compatibility_flags
            .contains("streams_enable_constructors"));
        assert!(!effective_grant(&existing, &requested)
            .compatibility_flags
            .contains("streams_enable_constructors"));
    }

    #[test]
    fn grant_covers_allows_stored_capabilities_beyond_manifest_baseline() {
        let existing = sample_grant(
            &["a.example", "b.example"],
            &["config", "secrets"],
            &["nodejs_compat"],
        );
        let requested = sample_grant(&["a.example"], &["config"], &[]);
        assert!(grant_covers(&existing, &requested));
        assert!(!grant_within_ceiling(&existing, &requested));
    }

    #[test]
    fn grant_covers_allows_deny_when_request_is_outbound() {
        let mut existing = sample_grant(&[], &[], &[]);
        existing.network_mode = "deny".into();
        let requested = sample_grant(&[], &[], &[]);
        assert!(grant_covers(&existing, &requested));
    }

    #[test]
    fn overlay_implies_loopback_tcp_from_postgres_url() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = first_party_database(
            dir.path(),
            "bookclerk-plugin-database-postgres",
            "postgres",
            r#"
api_version = 3
id = "postgres"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        let mut config = Config::default();
        config.database.plugin = "postgres".into();
        config.database.postgres.url =
            Some("postgres://postgres:postgres@localhost:5432/postgres".into());
        let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
        overlay_host_implied_network(&mut grant, &plugin, &config, std::slice::from_ref(&plugin));
        let policy = grant.egress_policy();
        assert!(
            policy.allows_tcp("localhost", 5432),
            "implied localhost TCP: {policy:?}"
        );
        assert!(grant.address_cidrs.contains("127.0.0.1/32"));
        assert!(grant.address_cidrs.contains("::1/128"));
    }

    #[test]
    fn overlay_public_host_needs_tcp_not_cidr() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = first_party_database(
            dir.path(),
            "bookclerk-plugin-database-postgres",
            "postgres",
            r#"
api_version = 3
id = "postgres"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        let mut config = Config::default();
        config.database.plugin = "postgres".into();
        config.database.postgres.url = Some("postgres://bookclerk@db.example.com:5432/db".into());
        let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
        overlay_host_implied_network(&mut grant, &plugin, &config, std::slice::from_ref(&plugin));
        assert!(grant.egress_policy().allows_tcp("db.example.com", 5432));
        assert!(grant.address_cidrs.is_empty());
    }

    #[test]
    fn overlay_skips_when_sqlite_is_active() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = first_party_database(
            dir.path(),
            "bookclerk-plugin-database-postgres",
            "postgres",
            r#"
api_version = 3
id = "postgres"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        let mut config = Config::default();
        config.database.plugin = "sqlite".into();
        config.database.postgres.url = Some("postgres://localhost/db".into());
        let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
        overlay_host_implied_network(&mut grant, &plugin, &config, std::slice::from_ref(&plugin));
        assert!(grant.tcp.is_empty());
        assert!(grant.address_cidrs.is_empty());
    }

    #[test]
    fn overlay_d1_api_base_implies_tcp() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = first_party_database(
            dir.path(),
            "bookclerk-plugin-database-d1",
            "d1",
            r#"
api_version = 3
id = "d1"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        let mut config = Config::default();
        config.database.plugin = "d1".into();
        config.database.d1.api_base = "https://api.cloudflare.com/client/v4".into();
        let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
        overlay_host_implied_network(&mut grant, &plugin, &config, std::slice::from_ref(&plugin));
        assert!(grant.egress_policy().allows_tcp("api.cloudflare.com", 443));
    }

    #[test]
    fn overlay_third_party_postgres_alias_does_not_get_host_url() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            dir.path(),
            r#"
api_version = 3
id = "postgres"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        assert!(crate::first_party_database_kind(&plugin).is_none());
        let mut config = Config::default();
        config.database.plugin = "postgres".into();
        config.database.postgres.url =
            Some("postgres://postgres:postgres@localhost:5432/postgres".into());
        let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
        overlay_host_implied_network(&mut grant, &plugin, &config, std::slice::from_ref(&plugin));
        assert!(
            grant.tcp.is_empty() && grant.address_cidrs.is_empty(),
            "third-party postgres alias must not inherit host URL overlay: {grant:?}"
        );
    }

    #[test]
    fn overlay_third_party_d1_alias_does_not_get_api_base() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            dir.path(),
            r#"
api_version = 3
id = "d1"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        assert!(crate::first_party_database_kind(&plugin).is_none());
        let mut config = Config::default();
        config.database.plugin = "d1".into();
        config.database.d1.api_base = "https://api.cloudflare.com/client/v4".into();
        let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
        overlay_host_implied_network(&mut grant, &plugin, &config, std::slice::from_ref(&plugin));
        assert!(
            grant.tcp.is_empty(),
            "third-party d1 alias must not inherit host D1 API overlay"
        );
    }

    #[test]
    fn postgres_tcp_target_parses_host_port_and_skips_unix() {
        assert_eq!(
            postgres_tcp_target("postgres://u:p@db.example.com:6543/library"),
            Some(("db.example.com".into(), 6543))
        );
        assert_eq!(
            postgres_tcp_target("postgres://localhost/db"),
            Some(("localhost".into(), 5432))
        );
        assert!(postgres_tcp_target("postgres://%2Fvar%2Frun%2Fpostgresql/db").is_none());
        assert!(postgres_tcp_target("postgres://ignored/db?host=/var/run/postgresql").is_none());
    }

    #[test]
    fn overlay_audiobookshelf_loopback_implies_cidrs() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            dir.path(),
            r#"
api_version = 3
id = "audiobookshelf"
runtime = "native"
command = "./guest"
entrypoints = ["remoteLibrary"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        let mut config = Config::default();
        config
            .integrations
            .set_audiobookshelf_string("base_url", "http://127.0.0.1:13378");
        let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
        overlay_host_implied_network(&mut grant, &plugin, &config, std::slice::from_ref(&plugin));
        assert!(grant.egress_policy().allows_tcp("127.0.0.1", 13378));
        assert!(grant.address_cidrs.contains("127.0.0.1/32"));
    }

    #[test]
    fn overlay_s3_custom_endpoint_implies_tcp() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            dir.path(),
            r#"
api_version = 3
id = "s3"
runtime = "native"
command = "./guest"
entrypoints = ["storage"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        let mut config = Config::default();
        config.output.s3.endpoint = Some("http://127.0.0.1:9000".into());
        let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
        overlay_host_implied_network(&mut grant, &plugin, &config, std::slice::from_ref(&plugin));
        assert!(grant.egress_policy().allows_tcp("127.0.0.1", 9000));
        assert!(grant.address_cidrs.contains("127.0.0.1/32"));
    }

    #[test]
    /// Invalid installation state (defense in depth): PluginKey occupancy must
    /// not overlay host TCP onto a different PluginKey that reused the alias.
    fn overlay_plugin_key_spec_does_not_grant_alias_twin() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let toml = r#"
api_version = 3
id = "postgres"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#;
        let real = first_party_database(
            a.path(),
            "bookclerk-plugin-database-postgres",
            "postgres",
            toml,
        );
        let twin = discovered(b.path(), toml);
        assert_eq!(real.alias(), twin.alias());
        assert_ne!(real.plugin_key(), twin.plugin_key());
        let mut config = Config::default();
        config.database.plugin = real.plugin_key().canonical().to_string();
        config.database.postgres.url =
            Some("postgres://postgres:postgres@localhost:5432/postgres".into());

        let among = [real.clone(), twin.clone()];
        let mut grant_real = consent_request(&real.manifest, real.plugin_key());
        overlay_host_implied_network(&mut grant_real, &real, &config, &among);
        assert!(
            grant_real.egress_policy().allows_tcp("localhost", 5432),
            "selected PluginKey must still get the host postgres overlay"
        );

        let mut grant_twin = consent_request(&twin.manifest, twin.plugin_key());
        overlay_host_implied_network(&mut grant_twin, &twin, &config, &among);
        assert!(
            grant_twin.tcp.is_empty() && grant_twin.address_cidrs.is_empty(),
            "corrupt duplicate alias must not inherit the operator postgres URL overlay"
        );
    }

    #[test]
    /// Invalid installation state (defense in depth): a bare alias occupancy
    /// grants neither twin when two PluginKeys share that alias.
    fn overlay_bare_alias_does_not_grant_when_twins_exist() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let toml = r#"
api_version = 3
id = "postgres"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#;
        let real = discovered(a.path(), toml);
        let twin = discovered(b.path(), toml);
        let among = [real.clone(), twin.clone()];
        let mut config = Config::default();
        config.database.plugin = "postgres".into();
        config.database.postgres.url =
            Some("postgres://postgres:postgres@localhost:5432/postgres".into());

        let mut grant_real = consent_request(&real.manifest, real.plugin_key());
        overlay_host_implied_network(&mut grant_real, &real, &config, &among);
        let mut grant_twin = consent_request(&twin.manifest, twin.plugin_key());
        overlay_host_implied_network(&mut grant_twin, &twin, &config, &among);
        assert!(
            grant_real.tcp.is_empty() && grant_twin.tcp.is_empty(),
            "corrupt duplicate alias occupancy must not overlay host TCP onto either install"
        );
    }

    #[test]
    /// Invalid installation state (defense in depth): S3 PluginKey occupancy
    /// must not overlay host TCP onto an alias twin.
    fn overlay_s3_plugin_key_spec_does_not_grant_alias_twin() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let toml = r#"
api_version = 3
id = "s3"
runtime = "native"
command = "./guest"
entrypoints = ["storage"]

[capabilities.network]
mode = "outbound"

[vars]
"#;
        let real = discovered(a.path(), toml);
        let twin = discovered(b.path(), toml);
        assert_eq!(real.alias(), twin.alias());
        assert_ne!(real.plugin_key(), twin.plugin_key());
        let mut config = Config::default();
        config.output.s3.plugin = real.plugin_key().canonical().to_string();
        config.output.s3.endpoint = Some("http://127.0.0.1:9000".into());

        let among = [real.clone(), twin.clone()];
        let mut grant_real = consent_request(&real.manifest, real.plugin_key());
        overlay_host_implied_network(&mut grant_real, &real, &config, &among);
        assert!(
            grant_real.egress_policy().allows_tcp("127.0.0.1", 9000),
            "selected PluginKey must still get the host S3 endpoint overlay"
        );

        let mut grant_twin = consent_request(&twin.manifest, twin.plugin_key());
        overlay_host_implied_network(&mut grant_twin, &twin, &config, &among);
        assert!(
            grant_twin.tcp.is_empty() && grant_twin.address_cidrs.is_empty(),
            "corrupt duplicate alias must not inherit the operator S3 endpoint overlay"
        );
    }

    #[test]
    /// Invalid installation state (defense in depth): ABS PluginKey occupancy
    /// must not overlay host TCP onto an alias twin.
    fn overlay_audiobookshelf_plugin_key_spec_does_not_grant_alias_twin() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let toml = r#"
api_version = 3
id = "audiobookshelf"
runtime = "native"
command = "./guest"
entrypoints = ["remoteLibrary"]

[capabilities.network]
mode = "outbound"

[vars]
"#;
        let real = discovered(a.path(), toml);
        let twin = discovered(b.path(), toml);
        let mut config = Config::default();
        config
            .integrations
            .set_audiobookshelf_string("plugin", real.plugin_key().canonical().to_string());
        config
            .integrations
            .set_audiobookshelf_string("base_url", "http://127.0.0.1:13378");

        let among = [real.clone(), twin.clone()];
        let mut grant_real = consent_request(&real.manifest, real.plugin_key());
        overlay_host_implied_network(&mut grant_real, &real, &config, &among);
        assert!(grant_real.egress_policy().allows_tcp("127.0.0.1", 13378));

        let mut grant_twin = consent_request(&twin.manifest, twin.plugin_key());
        overlay_host_implied_network(&mut grant_twin, &twin, &config, &among);
        assert!(
            grant_twin.tcp.is_empty() && grant_twin.address_cidrs.is_empty(),
            "corrupt duplicate alias must not inherit the operator Audiobookshelf URL overlay"
        );
    }

    #[test]
    fn postgres_host_overlay_changes_authority_not_persisted_grant() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = first_party_database(
            dir.path(),
            "bookclerk-plugin-database-postgres",
            "postgres",
            r#"
api_version = 3
id = "postgres"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "outbound"

[vars]
"#,
        );
        let mut config = Config::default();
        config.database.plugin = plugin.plugin_key().canonical().to_string();
        config.database.postgres.url =
            Some("postgres://postgres:postgres@127.0.0.1:5432/library".into());
        let persisted = consent_request(&plugin.manifest, plugin.plugin_key());
        assert!(persisted.tcp.is_empty(), "manifest has no fixed TCP host");
        let grant_rev = grant_revision(&persisted);
        let mut effective = persisted.clone();
        overlay_host_implied_network(
            &mut effective,
            &plugin,
            &config,
            std::slice::from_ref(&plugin),
        );
        assert!(
            effective.egress_policy().allows_tcp("127.0.0.1", 5432),
            "config Postgres URL must overlay TCP"
        );
        assert_eq!(
            grant_revision(&persisted),
            grant_rev,
            "persisted grant digest is taken from the stored object, not the overlay"
        );
        assert_ne!(
            crate::authority::authority_revision(&persisted),
            crate::authority::authority_revision(&effective)
        );
        let overlay_before = host_overlay_config_digest(&config);
        config.database.postgres.url =
            Some("postgres://postgres:postgres@10.0.0.8:5432/library".into());
        assert_ne!(overlay_before, host_overlay_config_digest(&config));
        let mut moved = persisted.clone();
        overlay_host_implied_network(&mut moved, &plugin, &config, std::slice::from_ref(&plugin));
        assert!(moved.egress_policy().allows_tcp("10.0.0.8", 5432));
        assert_ne!(
            crate::authority::authority_revision(&effective),
            crate::authority::authority_revision(&moved),
            "destination change must invalidate effective runtime authority"
        );
    }

    #[test]
    fn grant_revision_changes_with_authority_not_metadata() {
        let a = sample_grant(&["a.example"], &["config"], &[]);
        let mut b = a.clone();
        b.approved_at = "2099-01-01T00:00:00Z".into();
        assert_eq!(grant_revision(&a), grant_revision(&b));
        b.domains.insert("b.example".into());
        assert_ne!(grant_revision(&a), grant_revision(&b));
        assert!(!grant_revision(&a).is_empty());
        assert_eq!(grant_revision(&a).len(), 64);
    }

    #[test]
    fn grant_covers_rejects_mismatched_plugin_keys() {
        let mut a = sample_grant(&["a.example"], &["config"], &[]);
        let mut b = a.clone();
        a.plugin_key = "platform:bookclerk/bookclerk-plugin-database-sqlite".into();
        b.plugin_key = "path:file:///tmp/evil".into();
        a.plugin_id = "sqlite".into();
        b.plugin_id = "sqlite".into();
        assert!(!grant_covers(&a, &b));
    }

    #[test]
    fn validate_approved_grant_allows_domain_widen() {
        let baseline = sample_grant(&["a.example"], &["config"], &[]);
        let approved = sample_grant(&["a.example", "b.example"], &["config"], &[]);
        let grant = validate_approved_grant(&approved, &baseline).expect("widen ok");
        assert!(grant.domains.contains("a.example"));
        assert!(grant.domains.contains("b.example"));
    }

    #[test]
    fn validate_approved_grant_rejects_unknown_binding() {
        let baseline = sample_grant(&[], &["config"], &[]);
        let approved = sample_grant(&[], &["config", "not_a_real_binding"], &[]);
        let err = validate_approved_grant(&approved, &baseline)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown host binding"), "{err}");
    }

    #[test]
    fn validate_approved_grant_clamps_workerd_limits() {
        let mut ceiling = sample_grant(&["a.example"], &["config"], &["nodejs_compat"]);
        ceiling.cpu_ms = Some(WorkerdLimits::MAX_CPU_MS);
        ceiling.subrequests = Some(50);
        let mut approved = sample_grant(&["a.example"], &["config"], &["nodejs_compat"]);
        approved.cpu_ms = Some(WorkerdLimits::MAX_CPU_MS + 50);
        approved.subrequests = Some(40);
        let grant = validate_approved_grant(&approved, &ceiling).unwrap();
        assert_eq!(grant.cpu_ms, Some(WorkerdLimits::MAX_CPU_MS));
        assert_eq!(grant.subrequests, Some(40));
        assert!(!grant.approved_at.is_empty());
    }

    #[test]
    fn grant_has_binding_is_case_insensitive() {
        let grant = sample_grant(&[], &["config", "Secrets"], &[]);
        assert!(grant_has_binding(&grant, "config"));
        assert!(grant_has_binding(&grant, "CONFIG"));
        assert!(grant_has_binding(&grant, "secrets"));
        assert!(!grant_has_binding(&grant, "oauth"));
    }

    #[test]
    fn require_binding_fails_closed() {
        let grant = sample_grant(&[], &["config"], &[]);
        assert!(require_binding(&grant, "config").is_ok());
        let err = require_binding(&grant, "secrets").unwrap_err().to_string();
        assert!(err.contains("lacks binding `secrets`"), "{err}");
    }

    #[test]
    fn consent_request_carries_named_database_bindings() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[[databases]]
binding = "DB"

[[databases]]
binding = "CACHE"
"#,
        )
        .expect("manifest");
        let grant = consent_request_alias(&manifest);
        assert!(grant.bindings.contains("database:DB"));
        assert!(grant.bindings.contains("database:CACHE"));
        assert_eq!(granted_database_bindings(&grant), vec!["CACHE", "DB"]);
        let summary = consent_summary(&grant).join("\n");
        assert!(summary.contains("Plugin databases"), "{summary}");
    }

    #[test]
    fn approved_database_bindings_validate_names_and_keep_case() {
        let baseline = sample_grant(&[], &["database:DB_2", "config"], &[]);
        let approved = sample_grant(&[], &["database:DB_2", "config"], &[]);
        let grant = validate_approved_grant(&approved, &baseline).expect("valid binding grant");
        assert!(grant.bindings.contains("database:DB_2"));
        assert_eq!(granted_database_bindings(&grant), vec!["DB_2"]);

        let bad = sample_grant(&[], &["database:not-upper"], &[]);
        let err = validate_approved_grant(&bad, &baseline)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid database binding name"), "{err}");
    }

    #[test]
    fn spawn_config_omitted_without_config_binding() {
        let grant = sample_grant(&[], &["secrets"], &[]);
        let delivered = spawn_config_for_grant(
            &grant,
            serde_json::json!({ "greeting": "hi", "enabled": true }),
        );
        assert_eq!(delivered, serde_json::json!({}));
        let with_config = sample_grant(&[], &["config"], &[]);
        let kept = spawn_config_for_grant(&with_config, serde_json::json!({ "greeting": "hi" }));
        assert_eq!(kept["greeting"], "hi");
    }

    #[test]
    fn require_grant_fails_without_store_entry() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            &dir.path().join("install"),
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./guest"
entrypoints = ["storefront"]

[capabilities.network]
mode = "deny"

[vars]
"#,
        );
        let err = require_grant(dir.path(), &plugin).unwrap_err().to_string();
        assert!(err.contains("no permission grant"), "{err}");
    }

    #[test]
    fn require_grant_requires_reapprove_for_new_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            &dir.path().join("install"),
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./guest"
entrypoints = ["storefront"]

[capabilities.network]
mode = "deny"

[vars]

[secrets]
"#,
        );
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.network_mode = "deny".into();
        existing.plugin_key = plugin.plugin_key().canonical().to_string();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let effective = require_grant(dir.path(), &plugin).unwrap();
        assert!(grant_has_binding(&effective, "config"));
        assert!(!grant_has_binding(&effective, "secrets"));
        let err = require_binding(&effective, "secrets")
            .unwrap_err()
            .to_string();
        assert!(err.contains("secrets"), "{err}");
    }

    #[test]
    fn require_grant_keeps_operator_domain_widen_past_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            &dir.path().join("install"),
            r#"
api_version = 3
id = "demo"
runtime = "workerd"
entrypoints = ["storefront"]

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"

[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]

[vars]
"#,
        );
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&["old.example"], &["config"], &[]);
        existing.approved_at = "2026-01-01T00:00:00Z".into();
        existing.plugin_key = plugin.plugin_key().canonical().to_string();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let grant = require_grant(dir.path(), &plugin).expect("operator widen kept");
        assert!(grant.domains.contains("old.example"));
        assert!(
            grant.domains.contains("api.example.com"),
            "new manifest hosts re-evaluate on the same PluginKey"
        );
        assert!(grant.operator_added_domains.contains("old.example"));
    }

    #[test]
    fn platform_grant_auto_persists_for_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = stamped_sqlite(dir.path());
        assert_eq!(
            plugin.identity.provenance,
            PluginProvenance::PlatformBundled
        );
        let grant = ensure_platform_grant(dir.path(), &plugin).unwrap();
        assert!(grant_has_binding(&grant, "config"));
        assert!(grant_has_binding(&grant, "work_fs"));
        assert!(!grant.plugin_key.is_empty());
        let again = spawn_grant(dir.path(), &plugin).unwrap();
        assert_eq!(again.plugin_id, "sqlite");
        assert_eq!(again.approved_at, grant.approved_at);
        assert_eq!(again.bindings, grant.bindings);
        assert_eq!(again.plugin_key, plugin.plugin_key().canonical());
    }

    #[test]
    fn fake_sqlite_id_does_not_receive_platform_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            &dir.path().join("install"),
            r#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"

[vars]

[work_fs]
"#,
        );
        assert_eq!(
            plugin.identity.provenance,
            PluginProvenance::LocalDevelopment
        );
        assert!(!plugin.identity.provenance.grants_platform_defaults());
        let err = ensure_platform_grant(dir.path(), &plugin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no permission grant"), "{err}");
    }

    #[test]
    fn platform_grant_replaces_stale_grant_when_manifest_narrows() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            &dir.path().join("install"),
            r#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"

[vars]
"#,
        );
        bookclerk_plugin_catalog::stamp_platform_receipt(
            &plugin.root,
            dir.path(),
            "bookclerk-plugin-database-sqlite",
            &plugin.manifest,
            "0.0.0",
        )
        .unwrap();
        let plugin = DiscoveredPlugin::try_new(
            plugin.manifest,
            plugin.root,
            plugin.command,
            Some(dir.path()),
        )
        .unwrap();

        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config", "work_fs"], &[]);
        existing.plugin_key = plugin.plugin_key().canonical().to_string();
        existing.plugin_id = "sqlite".into();
        existing.entrypoints = ["databaseAdapter".to_string()].into_iter().collect();
        existing.network_mode = "deny".into();
        existing.approved_at = "2026-02-02T00:00:00Z".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let grant = ensure_platform_grant(dir.path(), &plugin).unwrap();
        assert_eq!(
            grant.bindings.iter().cloned().collect::<Vec<_>>(),
            vec!["config".to_string()]
        );
        assert!(!grant_has_binding(&grant, "work_fs"));
    }

    #[test]
    fn platform_grant_fails_outside_installer_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered(
            &dir.path().join("install"),
            r#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"

[vars]

[secrets]
"#,
        );
        bookclerk_plugin_catalog::stamp_platform_receipt(
            &plugin.root,
            dir.path(),
            "bookclerk-plugin-database-sqlite",
            &plugin.manifest,
            "0.0.0",
        )
        .unwrap();
        let plugin = DiscoveredPlugin::try_new(
            plugin.manifest,
            plugin.root,
            plugin.command,
            Some(dir.path()),
        )
        .unwrap();
        let err = ensure_platform_grant(dir.path(), &plugin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the installer envelope"), "{err}");
    }

    #[test]
    fn effective_grant_intersects_structural_and_keeps_operator_network() {
        let existing = sample_grant(
            &["a.example", "b.example"],
            &["config", "secrets"],
            &["nodejs_compat"],
        );
        let requested = sample_grant(&["a.example"], &["config"], &[]);
        let effective = effective_grant(&existing, &requested);
        assert_eq!(effective.plugin_id, existing.plugin_id);
        assert_eq!(effective.entrypoints, existing.entrypoints);
        assert_eq!(effective.approved_at, existing.approved_at);
        assert_eq!(effective.network_mode, existing.network_mode);
        assert!(effective.domains.contains("a.example"));
        assert!(effective.domains.contains("b.example"));
        assert!(effective.operator_added_domains.contains("b.example"));
        assert_eq!(
            effective.bindings.iter().cloned().collect::<Vec<_>>(),
            vec!["config".to_string()]
        );
        assert!(effective.compatibility_flags.is_empty());
        assert_eq!(effective.disk_mib, Some(PLUGIN_STATE_BUDGET_MIB_DEFAULT));
    }

    #[test]
    fn effective_grant_keeps_operator_tcp_denials_across_manifest_upgrade() {
        let denied = TcpGrant {
            host: "old.example.com".into(),
            ports: vec![443],
        };
        let added = TcpGrant {
            host: "extra.example.com".into(),
            ports: vec![443],
        };
        let mut existing = sample_grant(&["api.example.com"], &["config"], &[]);
        existing.tcp.insert(added.clone());
        existing.operator_added_tcp.insert(added.clone());
        existing.operator_denied_tcp.insert(denied.clone());
        existing.manifest_tcp.insert(denied.clone());

        let mut requested = sample_grant(&["api.example.com"], &["config"], &[]);
        requested.tcp.insert(denied.clone());
        requested.manifest_tcp.insert(denied.clone());
        requested.tcp.insert(TcpGrant {
            host: "new.example.com".into(),
            ports: vec![443],
        });
        requested.manifest_tcp.insert(TcpGrant {
            host: "new.example.com".into(),
            ports: vec![443],
        });

        let effective = effective_grant(&existing, &requested);
        assert!(
            !effective.tcp.contains(&denied),
            "operator TCP denials must survive a same-PluginKey package upgrade"
        );
        assert!(effective.tcp.contains(&added));
        assert!(effective.tcp.iter().any(|t| t.host == "new.example.com"));
        assert!(effective.operator_denied_tcp.contains(&denied));
    }

    #[test]
    fn effective_grant_keeps_operator_cidr_denials_across_manifest_upgrade() {
        let denied = "10.0.0.0/8".to_string();
        let added = "192.168.0.0/16".to_string();
        let mut existing = sample_grant(&["api.example.com"], &["config"], &[]);
        existing.address_cidrs.insert(added.clone());
        existing.operator_added_cidrs.insert(added.clone());
        existing.operator_denied_cidrs.insert(denied.clone());
        existing.manifest_cidrs.insert(denied.clone());

        let mut requested = sample_grant(&["api.example.com"], &["config"], &[]);
        requested.address_cidrs.insert(denied.clone());
        requested.manifest_cidrs.insert(denied.clone());
        requested.address_cidrs.insert("172.16.0.0/12".into());
        requested.manifest_cidrs.insert("172.16.0.0/12".into());

        let effective = effective_grant(&existing, &requested);
        assert!(
            !effective.address_cidrs.contains(&denied),
            "operator CIDR denials must survive a same-PluginKey package upgrade"
        );
        assert!(effective.address_cidrs.contains(&added));
        assert!(effective.address_cidrs.contains("172.16.0.0/12"));
        assert!(effective.operator_denied_cidrs.contains(&denied));
    }

    #[test]
    fn effective_grant_keeps_operator_domain_denials_across_manifest_upgrade() {
        let denied = "old.example.com".to_string();
        let added = "extra.example.com".to_string();
        let mut existing = sample_grant(&["api.example.com"], &["config"], &[]);
        existing.domains.insert(added.clone());
        existing.operator_added_domains.insert(added.clone());
        existing.operator_denied_domains.insert(denied.clone());
        existing.manifest_domains.insert(denied.clone());

        let mut requested = sample_grant(&["api.example.com"], &["config"], &[]);
        requested.domains.insert(denied.clone());
        requested.manifest_domains.insert(denied.clone());
        requested.domains.insert("new.example.com".into());
        requested.manifest_domains.insert("new.example.com".into());

        let effective = effective_grant(&existing, &requested);
        assert!(
            !effective.domains.contains(&denied),
            "operator fetch-host denials must survive a same-PluginKey package upgrade"
        );
        assert!(effective.domains.contains(&added));
        assert!(effective.domains.contains("new.example.com"));
        assert!(effective.operator_denied_domains.contains(&denied));
    }

    #[test]
    fn require_grant_does_not_inherit_across_plugin_keys() {
        let dir = tempfile::tempdir().unwrap();
        let platformish = discovered(
            &dir.path().join("a"),
            r#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"
"#,
        );
        let impostor = discovered(
            &dir.path().join("b"),
            r#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"
"#,
        );
        assert_ne!(
            platformish.plugin_key().canonical(),
            impostor.plugin_key().canonical()
        );
        let mut store = PluginGrantStore::default();
        let mut existing = consent_request(&platformish.manifest, platformish.plugin_key());
        existing.approved_at = "2026-01-01T00:00:00Z".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();
        let err = require_grant(dir.path(), &impostor)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no permission grant"), "{err}");
    }

    #[test]
    fn get_by_plugin_key_does_not_fall_back_to_alias() {
        let mut store = PluginGrantStore::default();
        let mut grant = sample_grant(&[], &["config"], &[]);
        grant.plugin_id = "local".into();
        grant.plugin_key = "platform:bookclerk/bookclerk-plugin-destination-local#local".into();
        store.upsert(grant);
        assert!(store.get_by_plugin_key("local").is_none());
        assert!(store.get("local").is_some(), "CLI alias helper still works");
        assert!(store
            .get_by_plugin_key("platform:bookclerk/bookclerk-plugin-destination-local#local")
            .is_some());
    }

    #[test]
    fn get_alias_is_ambiguous_when_plugin_key_twins_exist() {
        let mut store = PluginGrantStore::default();
        let mut a = sample_grant(&[], &["config"], &[]);
        a.plugin_id = "local".into();
        a.plugin_key = "platform:bookclerk/bookclerk-plugin-destination-local#local".into();
        let mut b = sample_grant(&[], &["config"], &[]);
        b.plugin_id = "local".into();
        b.plugin_key = "path:file:///tmp/evil#local".into();
        store.upsert(a);
        store.upsert(b);
        assert!(store.get("local").is_none());
        assert!(store
            .get_by_plugin_key("platform:bookclerk/bookclerk-plugin-destination-local#local")
            .is_some());
        assert!(store
            .get_by_plugin_key("path:file:///tmp/evil#local")
            .is_some());
    }

    #[test]
    /// Invalid installation state (defense in depth): a keyed grant must not
    /// replace a keyless same-alias legacy row.
    fn upsert_keyed_grant_does_not_replace_keyless_alias_twin() {
        let mut store = PluginGrantStore::default();
        let mut keyless = sample_grant(&[], &["config"], &[]);
        keyless.plugin_id = "local".into();
        store.upsert(keyless);
        let mut keyed = sample_grant(&[], &["secrets"], &[]);
        keyed.plugin_id = "local".into();
        keyed.plugin_key = "platform:bookclerk/bookclerk-plugin-destination-local#local".into();
        store.upsert(keyed);
        assert_eq!(store.grants.len(), 2);
        assert!(store
            .grants
            .iter()
            .any(|g| g.plugin_key.is_empty() && grant_has_binding(g, "config")));
        assert!(grant_has_binding(
            store
                .get_by_plugin_key("platform:bookclerk/bookclerk-plugin-destination-local#local")
                .expect("keyed grant"),
            "secrets"
        ));
    }

    #[test]
    /// Invalid installation state (defense in depth): a keyless grant must not
    /// replace a provenance-qualified same-alias row.
    fn upsert_keyless_grant_does_not_replace_keyed_same_alias() {
        let mut store = PluginGrantStore::default();
        let mut keyed = sample_grant(&[], &["config"], &[]);
        keyed.plugin_id = "local".into();
        keyed.plugin_key = "platform:bookclerk/bookclerk-plugin-destination-local#local".into();
        store.upsert(keyed);
        let mut keyless = sample_grant(&[], &["secrets"], &[]);
        keyless.plugin_id = "local".into();
        store.upsert(keyless);
        assert_eq!(store.grants.len(), 2);
        assert!(grant_has_binding(
            store
                .get_by_plugin_key("platform:bookclerk/bookclerk-plugin-destination-local#local")
                .expect("keyed grant"),
            "config"
        ));
        assert!(store
            .grants
            .iter()
            .any(|g| g.plugin_key.is_empty() && grant_has_binding(g, "secrets")));
    }

    #[test]
    fn upsert_replaces_existing_row_for_the_same_plugin_key() {
        let mut store = PluginGrantStore::default();
        let key = "platform:bookclerk/bookclerk-plugin-destination-local#local";
        let mut first = sample_grant(&[], &["config"], &[]);
        first.plugin_id = "local".into();
        first.plugin_key = key.into();
        store.upsert(first);
        let mut second = sample_grant(&[], &["secrets"], &[]);
        second.plugin_id = "local".into();
        second.plugin_key = key.into();
        store.upsert(second);
        assert_eq!(store.grants.len(), 1);
        assert!(grant_has_binding(
            store.get_by_plugin_key(key).expect("replaced"),
            "secrets"
        ));
    }

    #[test]
    fn upsert_fences_live_session_for_plugin_key() {
        let _lock = crate::authority::test_live_lock();
        let key = "path:file:///tmp/demo";
        let flag = crate::authority::register_session(key, "old-revision");
        let mut store = PluginGrantStore::default();
        let mut grant = sample_grant(&["a.example"], &["config"], &[]);
        grant.plugin_key = key.into();
        store.upsert(grant);
        assert!(crate::authority::is_fenced(&flag));
        crate::authority::unregister_session(&flag);
    }

    #[test]
    fn require_grant_keeps_stored_subset_after_repeated_manifest_widening() {
        let dir = tempfile::tempdir().unwrap();
        let narrowed = discovered(
            &dir.path().join("install"),
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./guest"
entrypoints = ["storefront"]

[capabilities.network]
mode = "deny"

[vars]
"#,
        );
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.network_mode = "deny".into();
        existing.plugin_key = narrowed.plugin_key().canonical().to_string();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let effective = require_grant(dir.path(), &narrowed).unwrap();
        assert!(grant_has_binding(&effective, "config"));

        let widened = discovered(
            &dir.path().join("install"),
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./guest"
entrypoints = ["storefront"]

[capabilities.network]
mode = "deny"

[vars]

[[kv_namespaces]]
binding = "KV"
"#,
        );
        let effective = require_grant(dir.path(), &widened).unwrap();
        assert!(grant_has_binding(&effective, "config"));
        assert!(!grant_has_binding(&effective, "plugin_kv"));
        let requested = consent_request(&widened.manifest, widened.plugin_key());
        let pending = pending_structural(&effective, &requested);
        assert!(
            pending.bindings.contains("plugin_kv"),
            "new KV binding must stay pending: {pending:?}"
        );
    }

    #[test]
    fn validate_approved_grant_allows_network_widen_not_structural() {
        let baseline = sample_grant(&["api.example.com"], &["config"], &["nodejs_compat"]);
        let approved = sample_grant(
            &["api.example.com", "extra.example.com"],
            &["config"],
            &["nodejs_compat"],
        );
        let grant = validate_approved_grant(&approved, &baseline).expect("network widen ok");
        assert!(grant.domains.contains("extra.example.com"));
        assert!(grant.operator_added_domains.contains("extra.example.com"));
        assert_eq!(grant.manifest_domains, baseline.domains);

        let steal = sample_grant(
            &["api.example.com"],
            &["config", "secrets"],
            &["nodejs_compat"],
        );
        let err = validate_approved_grant(&steal, &baseline)
            .unwrap_err()
            .to_string();
        assert!(err.contains("structural"), "{err}");
    }

    #[test]
    fn validate_approved_grant_normalizes_limits_and_preserves_subset() {
        let mut ceiling = sample_grant(&["api.example.com"], &["config", "secrets"], &[]);
        ceiling.cpu_ms = Some(60_000);
        ceiling.subrequests = Some(500);

        let mut approved = sample_grant(&["api.example.com"], &["config"], &[]);
        approved.cpu_ms = Some(15_000);
        approved.subrequests = Some(25);

        let normalized = validate_approved_grant(&approved, &ceiling).unwrap();
        assert_eq!(normalized.plugin_id, ceiling.plugin_id);
        assert_eq!(normalized.entrypoints, ceiling.entrypoints);
        assert_eq!(normalized.cpu_ms, Some(15_000));
        assert_eq!(normalized.subrequests, Some(25));
        assert!(grant_has_binding(&normalized, "config"));
        assert!(!grant_has_binding(&normalized, "secrets"));
        assert_ne!(normalized.approved_at, approved.approved_at);
    }

    #[test]
    fn validate_approved_grant_clamps_limits_to_host_max_not_baseline() {
        let mut baseline = sample_grant(&[], &[], &[]);
        baseline.cpu_ms = Some(30_000);
        baseline.subrequests = Some(50);
        let mut approved = baseline.clone();
        approved.cpu_ms = Some(60_000);
        approved.subrequests = Some(200);
        approved.disk_mib = Some(PLUGIN_STATE_BUDGET_MIB_MAX + 50);
        let grant = validate_approved_grant(&approved, &baseline).expect("host clamp");
        assert_eq!(grant.cpu_ms, Some(60_000));
        assert_eq!(grant.subrequests, Some(200));
        assert_eq!(grant.disk_mib, Some(PLUGIN_STATE_BUDGET_MIB_MAX));
    }

    fn caps(entrypoints: &[bookclerk_plugin_abi::Entrypoint]) -> PluginCapabilities {
        PluginCapabilities {
            entrypoints: entrypoints.to_vec(),
            ..PluginCapabilities::default()
        }
    }

    #[test]
    fn validate_describe_rejects_oauth_without_binding() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["storefront"]

[capabilities.network]
mode = "deny"

[vars]
"#,
        )
        .unwrap();
        let grant = consent_request_alias(&manifest);
        let err = validate_described_capabilities(
            &manifest,
            &grant,
            &caps(&[bookclerk_plugin_abi::Entrypoint::Storefront]),
            PortalAuthMode::Oauth,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("OAuth"), "{err}");
    }

    #[test]
    fn validate_describe_rejects_undeclared_entrypoints_and_events() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[[events.consumers]]
type = "book_acquired"
schema_versions = [1]
"#,
        )
        .unwrap();
        let grant = consent_request_alias(&manifest);
        let err = validate_described_capabilities(
            &manifest,
            &grant,
            &caps(&[
                bookclerk_plugin_abi::Entrypoint::Cli,
                bookclerk_plugin_abi::Entrypoint::RemoteLibrary,
            ]),
            PortalAuthMode::Unspecified,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("remoteLibrary"), "{err}");

        let widened_consumer = PluginCapabilities {
            consumes: vec![bookclerk_plugin_abi::EventConsumerSpec {
                event_type: "book_acquired".into(),
                schema_versions: vec![1, 2],
                supports_suspend: false,
            }],
            ..caps(&[bookclerk_plugin_abi::Entrypoint::Cli])
        };
        let err = validate_described_capabilities(
            &manifest,
            &grant,
            &widened_consumer,
            PortalAuthMode::Unspecified,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("widens event consumer"), "{err}");

        let undeclared_producer = PluginCapabilities {
            produces: vec!["book_acquired".into()],
            ..caps(&[bookclerk_plugin_abi::Entrypoint::Cli])
        };
        let err = validate_described_capabilities(
            &manifest,
            &grant,
            &undeclared_producer,
            PortalAuthMode::Unspecified,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("produces event"), "{err}");
    }

    #[test]
    fn validate_describe_accepts_manifest_capabilities() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli", "remoteLibrary"]

[capabilities.network]
mode = "deny"

[vars]

[[databases]]
binding = "DB"

[[events.consumers]]
type = "book_acquired"
schema_versions = [1]
supports_suspend = true

[[events.producers]]
type = "demo_pinged"
"#,
        )
        .unwrap();
        let grant = consent_request_alias(&manifest);
        assert!(grant.producers.contains("demo_pinged"));
        assert!(grant.entrypoints.contains("remoteLibrary"));
        validate_described_capabilities(
            &manifest,
            &grant,
            &manifest.capabilities(),
            PortalAuthMode::Unspecified,
        )
        .unwrap();

        // Same PluginKey still covers enable/spawn; the missing producer stays
        // pending and describe() cannot claim it.
        let mut narrowed = grant.clone();
        narrowed.producers.clear();
        assert!(grant_covers(&narrowed, &grant));
        let pending = pending_structural(&narrowed, &grant);
        assert!(pending.producers.contains("demo_pinged"));
        let err = validate_described_capabilities(
            &manifest,
            &narrowed,
            &manifest.capabilities(),
            PortalAuthMode::Unspecified,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("grant lacks event producer"), "{err}");
    }

    #[test]
    fn validate_describe_rejects_ungranted_host_binding() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[vars]

[secrets]
"#,
        )
        .unwrap();
        let requested = consent_request_alias(&manifest);
        assert!(grant_has_binding(&requested, "config"));
        assert!(grant_has_binding(&requested, "secrets"));
        let mut granted_a = requested.clone();
        granted_a.bindings.remove("secrets");
        let described_a = PluginCapabilities {
            entrypoints: vec![bookclerk_plugin_abi::Entrypoint::Cli],
            bindings: vec!["CONFIG".into()],
            ..PluginCapabilities::default()
        };
        validate_described_capabilities(
            &manifest,
            &granted_a,
            &described_a,
            PortalAuthMode::Unspecified,
        )
        .expect("describe of granted config only must succeed");
        let err = validate_described_capabilities(
            &manifest,
            &granted_a,
            &manifest.capabilities(),
            PortalAuthMode::Unspecified,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("grant lacks binding `secrets`"),
            "describe of ungranted secrets must fail: {err}"
        );
    }

    #[test]
    fn python_outbound_consent_includes_pyodide_hosts() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "echo_workerd_python"
runtime = "workerd"
entrypoints = ["cli"]

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

[vars]
"#,
        )
        .unwrap();
        let grant = consent_request_alias(&manifest);
        assert!(grant.domains.contains("api.example.com"));
        for host in bookclerk_plugin_manifest::PYODIDE_EGRESS_HOSTS {
            assert!(
                grant.domains.iter().any(|d| d == *host),
                "missing Pyodide host {host} in {:?}",
                grant.domains
            );
        }
        let summary = consent_summary(&grant);
        assert!(
            summary.iter().any(|l| l.contains("Python runtime hosts")),
            "{summary:?}"
        );
        assert_eq!(grant.cpu_ms, Some(WorkerdLimits::DEFAULT_CPU_MS));
        assert_eq!(grant.subrequests, Some(WorkerdLimits::DEFAULT_SUBREQUESTS));
        assert!(
            summary.iter().any(|l| l.contains("Workerd CPU limit")),
            "{summary:?}"
        );
        assert!(
            summary
                .iter()
                .any(|l| l.contains("Workerd subrequest limit")),
            "{summary:?}"
        );
        // Narrow stored grant for the same plugin id remains usable.
        let mut narrow = sample_grant(&["api.example.com"], &["config"], &["python_workers"]);
        narrow.plugin_id = grant.plugin_id.clone();
        narrow.entrypoints = grant.entrypoints.clone();
        assert!(grant_covers(&narrow, &grant));
        assert!(grant_within_ceiling(&narrow, &grant));
    }

    #[test]
    fn workerd_grant_env_keys_match_launcher_contract() {
        // Keep in lockstep with bookclerk_workerd::grant constants (stringly
        // coupled across crates; no shared dep either direction).
        assert_eq!(
            WORKERD_GRANT_NETWORK_MODE_ENV,
            "BOOKCLERK_WORKERD_GRANT_NETWORK_MODE"
        );
        assert_eq!(WORKERD_GRANT_DOMAINS_ENV, "BOOKCLERK_WORKERD_GRANT_DOMAINS");
        assert_eq!(WORKERD_GRANT_CPU_MS_ENV, "BOOKCLERK_WORKERD_GRANT_CPU_MS");
        assert_eq!(
            WORKERD_GRANT_SUBREQUESTS_ENV,
            "BOOKCLERK_WORKERD_GRANT_SUBREQUESTS"
        );
    }

    #[test]
    fn cpu_ms_and_subrequests_round_trip_on_grant() {
        let mut grant = sample_grant(&[], &[], &[]);
        grant.cpu_ms = Some(12_000);
        grant.subrequests = Some(75);
        let encoded = serde_json::to_string(&grant).unwrap();
        assert!(encoded.contains("cpuMs"));
        assert!(encoded.contains("subrequests"));
        let decoded: PluginGrant = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.cpu_ms, Some(12_000));
        assert_eq!(decoded.subrequests, Some(75));
    }

    #[test]
    fn explicit_empty_consumers_and_jobs_stay_empty() {
        let mut approved = sample_grant(&[], &["config"], &[]);
        approved.schema_version = GRANT_SCHEMA_VERSION;
        approved.consumers.clear();
        approved.jobs.clear();
        let mut baseline = approved.clone();
        baseline.consumers.insert(GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: None,
        });
        baseline.jobs.insert("stream_copy".into());
        let grant = validate_approved_grant(&approved, &baseline).unwrap();
        assert!(grant.consumers.is_empty());
        assert!(grant.jobs.is_empty());
        assert_eq!(grant.schema_version, GRANT_SCHEMA_VERSION);
        let effective = effective_grant(&grant, &baseline);
        assert!(effective.consumers.is_empty());
        assert!(effective.jobs.is_empty());
    }

    #[test]
    fn legacy_empty_structural_sets_do_not_inherit_manifest() {
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.schema_version = 0;
        existing.consumers.clear();
        existing.jobs.clear();
        existing.entrypoints.clear();
        existing.bindings.clear();
        existing.producers.clear();
        let mut requested = existing.clone();
        requested.schema_version = GRANT_SCHEMA_VERSION;
        requested.entrypoints.insert("storefront".into());
        requested.bindings.insert("secrets".into());
        requested.consumers.insert(GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: None,
        });
        requested.jobs.insert("stream_copy".into());
        let effective = effective_grant(&existing, &requested);
        assert!(effective.entrypoints.is_empty());
        assert!(effective.consumers.is_empty());
        assert!(effective.jobs.is_empty());
        assert!(effective.bindings.is_empty());
        let pending = pending_structural(&existing, &requested);
        assert!(pending.entrypoints.contains("storefront"));
        assert_eq!(pending.consumers.len(), 1);
        assert!(pending.jobs.contains("stream_copy"));
        assert!(pending.bindings.contains("secrets"));
    }

    #[test]
    fn effective_grant_does_not_auto_widen_unset_budgets() {
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.cpu_ms = None;
        existing.disk_mib = Some(64);
        let mut requested = existing.clone();
        requested.cpu_ms = Some(60_000);
        requested.disk_mib = Some(512);
        let effective = effective_grant(&existing, &requested);
        assert_eq!(effective.cpu_ms, None);
        assert_eq!(effective.disk_mib, Some(effective_disk_mib(Some(64))));
    }

    #[test]
    fn manifest_ab_grant_a_is_effective_a_only() {
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.entrypoints = ["cli".into()].into_iter().collect();
        let mut requested = existing.clone();
        requested.entrypoints = ["cli".into(), "storefront".into()].into_iter().collect();
        requested.consumers.insert(GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: None,
        });
        let effective = effective_grant(&existing, &requested);
        assert_eq!(
            effective.entrypoints,
            ["cli".to_string()].into_iter().collect()
        );
        assert!(effective.consumers.is_empty());
        let pending = pending_structural(&existing, &requested);
        assert!(pending.entrypoints.contains("storefront"));
        assert_eq!(pending.consumers.len(), 1);

        let manifest = PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli", "storefront"]

[capabilities.network]
mode = "deny"
"#,
        )
        .unwrap();
        let described_ok = PluginCapabilities {
            entrypoints: vec![bookclerk_plugin_abi::Entrypoint::Cli],
            ..PluginCapabilities::default()
        };
        validate_described_capabilities(
            &manifest,
            &effective,
            &described_ok,
            PortalAuthMode::Unspecified,
        )
        .unwrap();
        let described_extra = PluginCapabilities {
            entrypoints: vec![
                bookclerk_plugin_abi::Entrypoint::Cli,
                bookclerk_plugin_abi::Entrypoint::Storefront,
            ],
            ..PluginCapabilities::default()
        };
        let err = validate_described_capabilities(
            &manifest,
            &effective,
            &described_extra,
            PortalAuthMode::Unspecified,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("storefront"), "{err}");
    }

    #[test]
    fn omitted_schema_version_json_is_legacy() {
        let json = r#"{
            "pluginId":"demo",
            "networkMode":"deny",
            "domains":[],
            "bindings":["config"],
            "compatibilityFlags":[],
            "approvedAt":"2026-01-01T00:00:00Z"
        }"#;
        let grant: PluginGrant = serde_json::from_str(json).unwrap();
        assert_eq!(grant.schema_version, 0);
    }

    #[test]
    fn windows_outer_job_adds_supervisors_without_changing_payload_overhead() {
        assert_eq!(GuestRuntimeKind::NativeBehindWorkerd.process_overhead(), 3);
        assert_eq!(WINDOWS_SESSION_PROCESS_BASELINE, 5);
        // extras 0, 1, and the default of 2. The absolute max clamps 5+62.
        assert_eq!(windows_session_active_processes(0), 5);
        assert_eq!(windows_session_active_processes(1), 6);
        assert_eq!(
            windows_session_active_processes(PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT),
            7
        );
        assert_eq!(
            windows_session_active_processes(PLUGIN_JAIL_EXTRA_PROCESSES_MAX),
            PLUGIN_JAIL_ACTIVE_PROCESSES_MAX
        );
        assert_eq!(
            active_processes_for(GuestRuntimeKind::NativeDirect, 0),
            1,
            "guest inner cap is the guest plus extras, not the outer baseline"
        );
        assert_eq!(
            active_processes_for(
                GuestRuntimeKind::Workerd,
                PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT
            ),
            4
        );
    }
}
