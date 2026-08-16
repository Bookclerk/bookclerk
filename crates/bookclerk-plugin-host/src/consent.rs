//! Operator permission grants for plugin capabilities.
//!
//! The manifest consent request is a **baseline suggestion**, not a hard
//! ceiling. Operators may widen or narrow domains, bindings, flags, network
//! mode, workerd budgets, and per-plugin disk space. Host hard caps still
//! apply ([`WorkerdLimits`] maxes, [`PLUGIN_STATE_BUDGET_MIB_MAX`], known
//! bindings). Overrides that break plugin functionality are the operator's
//! responsibility — Bookclerk only enforces what the grant records.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::manifest::{PluginManifest, PluginRuntimeKind, WorkerdLimits};
use crate::{PluginError, Result};

/// Filename under `$BOOKCLERK_FILES_DIR` for persisted grants.
pub const GRANTS_FILE: &str = "plugin-grants.json";

/// Env keys consumed by `bookclerk-workerd` (`grant.rs`) at isolate start.
pub const WORKERD_GRANT_NETWORK_MODE_ENV: &str = "BOOKCLERK_WORKERD_GRANT_NETWORK_MODE";
/// Comma-separated grant domain allowlist for workerd egress.
pub const WORKERD_GRANT_DOMAINS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_DOMAINS";
/// Grant CPU budget (ms) for workerd logging / limit narrowing.
pub const WORKERD_GRANT_CPU_MS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_CPU_MS";
/// Grant subrequest budget injected into workerd `EGRESS_POLICY`.
pub const WORKERD_GRANT_SUBREQUESTS_ENV: &str = "BOOKCLERK_WORKERD_GRANT_SUBREQUESTS";

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

/// Fixed jail occupancy for the launcher / primary guest tree.
///
/// Native: `bookclerk-jail` execs the guest (1 PID). Workerd: `bookclerk-workerd`
/// plus the `workerd` child (2 PIDs).
#[must_use]
pub fn jail_process_overhead(runtime: PluginRuntimeKind) -> u32 {
    match runtime {
        PluginRuntimeKind::Native => 1,
        PluginRuntimeKind::Workerd => 2,
    }
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
pub fn active_processes_for(runtime: PluginRuntimeKind, extra: u32) -> u32 {
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

/// One approved grant snapshot for a plugin id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginGrant {
    /// Plugin id from `plugin.toml` (globally unique across kinds).
    pub plugin_id: String,
    /// Plugin kind string (`source`, `integration`, `output`, `database`).
    pub kind: String,
    /// Approved network mode: `deny` or `outbound`.
    pub network_mode: String,
    /// Approved initial outbound domain patterns (**workerd** allowlist only).
    pub domains: BTreeSet<String>,
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
        let path = Self::path(files_dir);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Writes this grant store to `plugin-grants.json` under `files_dir`.
    ///
    /// # Arguments
    ///
    /// * `files_dir` - Bookclerk files directory that owns `plugin-grants.json`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when serialization or the write fails.
    pub fn save(&self, files_dir: &Path) -> Result<()> {
        let path = Self::path(files_dir);
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Returns the grant for `plugin_id`, if one is stored.
    ///
    /// # Arguments
    ///
    /// * `plugin_id` - Plugin id to look up.
    ///
    /// # Returns
    ///
    /// A reference to the matching [`PluginGrant`], or `None`.
    pub fn get(&self, plugin_id: &str) -> Option<&PluginGrant> {
        self.grants.iter().find(|g| g.plugin_id == plugin_id)
    }

    /// Inserts or replaces the grant for `grant.plugin_id`.
    ///
    /// # Arguments
    ///
    /// * `grant` - Full consent snapshot to persist in memory (call [`Self::save`] to flush).
    pub fn upsert(&mut self, grant: PluginGrant) {
        if let Some(existing) = self
            .grants
            .iter_mut()
            .find(|g| g.plugin_id == grant.plugin_id)
        {
            *existing = grant;
        } else {
            self.grants.push(grant);
        }
    }
}

/// Build the consent request surface from a manifest.
#[must_use]
pub fn consent_request(manifest: &PluginManifest) -> PluginGrant {
    let mut bindings = BTreeSet::new();
    let b = &manifest.capabilities.bindings;
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
        plugin_id: manifest.id.clone(),
        kind: manifest.kind.as_str().to_string(),
        network_mode: match manifest.capabilities.network.mode {
            crate::manifest::NetworkMode::Deny => "deny".into(),
            crate::manifest::NetworkMode::Outbound => "outbound".into(),
        },
        domains,
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
        format!("Plugin: {} ({})", grant.plugin_id, grant.kind),
        format!("Network: {}", grant.network_mode),
    ];
    if grant.network_mode == "outbound" && grant.domains.is_empty() {
        lines.push(
            "Native / coarse outbound: OS jail allow-or-deny only (no hostname filter). \
             Domain allowlists are enforced for workerd guests. Jail memory/CPU/process \
             and disk budgets apply to both runtimes."
                .into(),
        );
    }
    if !grant.domains.is_empty() {
        lines.push(format!(
            "Workerd initial outbound domains: {}",
            grant.domains.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
        lines
            .push("Redirect hops after an allowed initial host do not require re-approval.".into());
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
}

/// True when a stored grant is usable for enable/spawn of this plugin id.
///
/// Operator grants are authoritative: presence for the plugin id is enough.
/// Manifest changes no longer invalidate a stored grant (operator responsibility).
#[must_use]
pub fn grant_covers(existing: &PluginGrant, requested: &PluginGrant) -> bool {
    existing.plugin_id == requested.plugin_id
        && (existing.kind.is_empty() || existing.kind.eq_ignore_ascii_case(&requested.kind))
}

/// Spawn/delivery grant: stored approval is authoritative, host-normalized.
///
/// Does **not** intersect domains/bindings/flags with the manifest request, so
/// operator widen/narrow overrides survive spawn. Missing workerd budgets fall
/// back to the request defaults; all numeric limits clamp to host maxes.
#[must_use]
pub fn effective_grant(existing: &PluginGrant, requested: &PluginGrant) -> PluginGrant {
    let network_mode = if existing.network_mode.is_empty() {
        requested.network_mode.clone()
    } else {
        existing.network_mode.clone()
    };
    PluginGrant {
        plugin_id: existing.plugin_id.clone(),
        kind: if existing.kind.is_empty() {
            requested.kind.clone()
        } else {
            existing.kind.clone()
        },
        network_mode,
        domains: existing.domains.clone(),
        bindings: existing.bindings.clone(),
        compatibility_flags: existing.compatibility_flags.clone(),
        cpu_ms: normalize_cpu_ms(existing.cpu_ms.or(requested.cpu_ms)),
        subrequests: normalize_subrequests(existing.subrequests.or(requested.subrequests)),
        disk_mib: Some(effective_disk_mib(existing.disk_mib.or(requested.disk_mib))),
        memory_mib: Some(effective_memory_mib(
            existing.memory_mib.or(requested.memory_mib),
        )),
        cpu_rate_percent: existing
            .cpu_rate_percent
            .or(requested.cpu_rate_percent)
            .map(|v| effective_cpu_rate_percent(Some(v))),
        extra_processes: existing
            .extra_processes
            .or(requested.extra_processes)
            .map(|v| effective_extra_processes(Some(v))),
        approved_at: existing.approved_at.clone(),
    }
}

/// Validates and normalizes an operator-supplied grant against host hard caps.
///
/// The manifest `baseline` supplies identity defaults and suggested values.
/// Operators may **widen or narrow** domains, bindings, flags, network mode,
/// workerd budgets, and disk space. Unknown bindings are rejected; workerd /
/// disk limits clamp to host maximums.
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
    if !approved.kind.is_empty() && !approved.kind.eq_ignore_ascii_case(&baseline.kind) {
        return Err(PluginError::message(format!(
            "grant kind `{}` does not match `{}`",
            approved.kind, baseline.kind
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
        if !KNOWN_HOST_BINDINGS
            .iter()
            .any(|known| binding.eq_ignore_ascii_case(known))
        {
            return Err(PluginError::message(format!(
                "unknown host binding `{binding}`"
            )));
        }
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
        plugin_id: baseline.plugin_id.clone(),
        kind: baseline.kind.clone(),
        network_mode: network_mode.to_ascii_lowercase(),
        domains,
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
pub fn require_grant(files_dir: &Path, manifest: &PluginManifest) -> Result<PluginGrant> {
    let store = PluginGrantStore::load(files_dir)?;
    let requested = consent_request(manifest);
    match store.get(&manifest.id) {
        Some(existing) if grant_covers(existing, &requested) => {
            Ok(effective_grant(existing, &requested))
        }
        Some(_) => Err(PluginError::message(format!(
            "plugin `{}` grant does not match this plugin; re-approve with `bookclerk plugins approve {}`",
            manifest.id, manifest.id
        ))),
        None => Err(PluginError::message(format!(
            "plugin `{}` has no permission grant; run `bookclerk plugins approve {}` first",
            manifest.id, manifest.id
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

/// Handshake `config` payload: non-empty settings only when the grant includes `config`.
#[must_use]
pub fn handshake_config_for_grant(
    grant: &PluginGrant,
    config_table: serde_json::Value,
) -> serde_json::Value {
    if grant_has_binding(grant, "config") {
        config_table
    } else {
        serde_json::json!({})
    }
}

/// Platform guests shipped with the installer (`sqlite`, `local`) skip the
/// consent UX and are enabled by default. Persist a covering grant when the
/// installed manifest stays within the safe platform envelope (deny network,
/// only `config` / `work_fs` bindings).
pub fn ensure_platform_grant(files_dir: &Path, manifest: &PluginManifest) -> Result<PluginGrant> {
    if !is_platform_plugin_id(&manifest.id) {
        return require_grant(files_dir, manifest);
    }
    let requested = consent_request(manifest);
    if !is_safe_platform_request(&requested) {
        return Err(PluginError::message(format!(
            "platform plugin `{}` declares capabilities outside the installer envelope; \
             run `bookclerk plugins approve {}`",
            manifest.id, manifest.id
        )));
    }
    let mut store = PluginGrantStore::load(files_dir)?;
    match store.get(&manifest.id) {
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

/// Resolve the covering grant for spawn: platform auto-grant, else [`require_grant`].
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn spawn_grant(files_dir: &Path, manifest: &PluginManifest) -> Result<PluginGrant> {
    if is_platform_plugin_id(&manifest.id) {
        ensure_platform_grant(files_dir, manifest)
    } else {
        require_grant(files_dir, manifest)
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
}

/// Returns true for installer platform guests (`sqlite`, `local`).
///
/// # Arguments
///
/// * `id` - Plugin id to test (case-insensitive).
///
/// # Returns
///
/// `true` when the id is a built-in platform plugin.
#[must_use]
pub fn is_platform_plugin_id(id: &str) -> bool {
    matches!(id.to_ascii_lowercase().as_str(), "sqlite" | "local")
}

/// True when a platform grant is deny-network with only `config` / `work_fs` bindings.
fn is_safe_platform_request(grant: &PluginGrant) -> bool {
    grant.network_mode == "deny"
        && grant.domains.is_empty()
        && grant.compatibility_flags.is_empty()
        && grant
            .bindings
            .iter()
            .all(|b| b == "config" || b == "work_fs")
}

/// Reject handshake claims that exceed the manifest (and covering grant).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn validate_handshake_capabilities(
    manifest: &PluginManifest,
    grant: &PluginGrant,
    capabilities: &[String],
    portal_auth_mode: Option<&str>,
) -> Result<()> {
    let oauth_mode = portal_auth_mode.is_some_and(|m| m.eq_ignore_ascii_case("oauth"));
    let oauth_methods = capabilities.iter().any(|cap| {
        cap.eq_ignore_ascii_case("loginStart") || cap.eq_ignore_ascii_case("loginComplete")
    });
    if oauth_mode || oauth_methods {
        if !manifest.capabilities.bindings.oauth {
            return Err(PluginError::message(format!(
                "plugin `{}` handshake advertises OAuth without bindings.oauth in plugin.toml",
                manifest.id
            )));
        }
        require_binding(grant, "oauth")?;
    }

    // Lifecycle / entrypoint methods are always implied; kind-specific surfaces
    // in `capabilities.methods` are what consent is meant to bound.
    const CORE_CAPS: &[&str] = &[
        "handshake",
        "shutdown",
        "health",
        "diagnose",
        "start",
        "onEvent",
        "pollEvents",
        "cli",
        "cliDescribe",
        "cliInvoke",
        "oidcClients",
    ];
    let declared = &manifest.capabilities.methods.list;
    if declared.is_empty() {
        return Ok(());
    }
    for cap in capabilities {
        if CORE_CAPS.iter().any(|name| name.eq_ignore_ascii_case(cap)) {
            continue;
        }
        if !declared.iter().any(|name| name.eq_ignore_ascii_case(cap)) {
            return Err(PluginError::message(format!(
                "plugin `{}` handshake advertises capability `{cap}` not listed in \
                 capabilities.methods",
                manifest.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginManifest;

    fn sample_grant(domains: &[&str], bindings: &[&str], flags: &[&str]) -> PluginGrant {
        PluginGrant {
            plugin_id: "demo".into(),
            kind: "source".into(),
            network_mode: "outbound".into(),
            domains: domains.iter().map(|s| (*s).to_string()).collect(),
            bindings: bindings.iter().map(|s| (*s).to_string()).collect(),
            compatibility_flags: flags.iter().map(|s| (*s).to_string()).collect(),
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
    fn grant_covers_allows_operator_domain_subset() {
        let existing = sample_grant(&["a.example"], &["config"], &[]);
        let requested = sample_grant(&["a.example", "b.example"], &["config"], &[]);
        assert!(grant_covers(&existing, &requested));
    }

    #[test]
    fn grant_covers_allows_operator_binding_subset() {
        let existing = sample_grant(&["a.example"], &["config"], &[]);
        let requested = sample_grant(&["a.example"], &["config", "secrets"], &[]);
        assert!(grant_covers(&existing, &requested));
    }

    #[test]
    fn grant_covers_allows_operator_flag_subset() {
        let existing = sample_grant(&[], &[], &["nodejs_compat"]);
        let requested = sample_grant(&[], &[], &["nodejs_compat", "streams_enable_constructors"]);
        assert!(grant_covers(&existing, &requested));
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
    fn handshake_config_omitted_without_config_binding() {
        let grant = sample_grant(&[], &["secrets"], &[]);
        let delivered = handshake_config_for_grant(
            &grant,
            serde_json::json!({ "greeting": "hi", "enabled": true }),
        );
        assert_eq!(delivered, serde_json::json!({}));
        let with_config = sample_grant(&[], &["config"], &[]);
        let kept =
            handshake_config_for_grant(&with_config, serde_json::json!({ "greeting": "hi" }));
        assert_eq!(kept["greeting"], "hi");
    }

    #[test]
    fn require_grant_fails_without_store_entry() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let err = require_grant(dir.path(), &manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no permission grant"), "{err}");
    }

    #[test]
    fn require_grant_succeeds_when_manifest_widens_past_stored_subset() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.network_mode = "deny".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
secrets = true
"#,
        )
        .unwrap();
        let grant = require_grant(dir.path(), &manifest).unwrap();
        assert!(grant_has_binding(&grant, "config"));
        assert!(!grant_has_binding(&grant, "secrets"));
    }

    #[test]
    fn require_grant_keeps_operator_domain_widen_past_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&["old.example"], &["config"], &[]);
        existing.approved_at = "2026-01-01T00:00:00Z".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "demo"
kind = "source"
runtime = "workerd"

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"

[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let grant = require_grant(dir.path(), &manifest).expect("operator widen kept");
        assert!(grant.domains.contains("old.example"));
        assert!(!grant.domains.contains("api.example.com"));
    }

    #[test]
    fn platform_grant_auto_persists_for_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "sqlite"
kind = "database"
runtime = "native"
command = "./sqlite"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
work_fs = true
"#,
        )
        .unwrap();
        let grant = ensure_platform_grant(dir.path(), &manifest).unwrap();
        assert!(grant_has_binding(&grant, "config"));
        assert!(grant_has_binding(&grant, "work_fs"));
        // Second call returns an effective grant (same surface, preserved approved_at).
        let again = spawn_grant(dir.path(), &manifest).unwrap();
        assert_eq!(again.plugin_id, "sqlite");
        assert_eq!(again.approved_at, grant.approved_at);
        assert_eq!(again.bindings, grant.bindings);
    }

    #[test]
    fn platform_grant_replaces_stale_grant_when_manifest_narrows() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config", "work_fs"], &[]);
        existing.plugin_id = "sqlite".into();
        existing.kind = "database".into();
        existing.network_mode = "deny".into();
        existing.approved_at = "2026-02-02T00:00:00Z".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "sqlite"
kind = "database"
runtime = "native"
command = "./sqlite"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let grant = ensure_platform_grant(dir.path(), &manifest).unwrap();
        assert_eq!(
            grant.bindings.iter().cloned().collect::<Vec<_>>(),
            vec!["config".to_string()]
        );
        assert!(!grant_has_binding(&grant, "work_fs"));
    }

    #[test]
    fn platform_grant_fails_outside_installer_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.plugin_id = "sqlite".into();
        existing.kind = "database".into();
        existing.network_mode = "deny".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "sqlite"
kind = "database"
runtime = "native"
command = "./sqlite"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
secrets = true
"#,
        )
        .unwrap();
        let err = ensure_platform_grant(dir.path(), &manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the installer envelope"), "{err}");
    }

    #[test]
    fn effective_grant_preserves_operator_approval_without_intersecting() {
        let existing = sample_grant(
            &["a.example", "b.example"],
            &["config", "secrets"],
            &["nodejs_compat"],
        );
        let requested = sample_grant(&["a.example"], &["config"], &[]);
        let effective = effective_grant(&existing, &requested);
        assert_eq!(effective.plugin_id, existing.plugin_id);
        assert_eq!(effective.kind, existing.kind);
        assert_eq!(effective.approved_at, existing.approved_at);
        assert_eq!(effective.network_mode, existing.network_mode);
        assert_eq!(effective.domains, existing.domains);
        assert_eq!(effective.bindings, existing.bindings);
        assert_eq!(effective.compatibility_flags, existing.compatibility_flags);
        assert_eq!(effective.disk_mib, Some(PLUGIN_STATE_BUDGET_MIB_DEFAULT));
    }

    #[test]
    fn require_grant_keeps_stored_subset_after_repeated_manifest_widening() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.network_mode = "deny".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let narrowed = PluginManifest::parse(
            r#"
api_version = 2
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let effective = require_grant(dir.path(), &narrowed).unwrap();
        assert!(grant_has_binding(&effective, "config"));

        let widened = PluginManifest::parse(
            r#"
api_version = 2
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
plugin_kv = true
"#,
        )
        .unwrap();
        let effective = require_grant(dir.path(), &widened).unwrap();
        assert!(grant_has_binding(&effective, "config"));
        assert!(!grant_has_binding(&effective, "plugin_kv"));
    }

    #[test]
    fn validate_approved_grant_allows_widen_beyond_baseline() {
        let baseline = sample_grant(&["api.example.com"], &["config"], &["nodejs_compat"]);
        let approved = sample_grant(
            &["api.example.com", "extra.example.com"],
            &["config", "secrets"],
            &["nodejs_compat"],
        );
        let grant = validate_approved_grant(&approved, &baseline).expect("widen ok");
        assert!(grant.domains.contains("extra.example.com"));
        assert!(grant.bindings.contains("secrets"));
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
        assert_eq!(normalized.kind, ceiling.kind);
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

    #[test]
    fn validate_handshake_rejects_oauth_without_binding() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let grant = consent_request(&manifest);
        let err = validate_handshake_capabilities(
            &manifest,
            &grant,
            &["loginStart".into()],
            Some("oauth"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("OAuth"), "{err}");
    }

    #[test]
    fn validate_handshake_rejects_undeclared_methods() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "demo"
kind = "integration"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.methods]
list = ["handshake", "health"]
"#,
        )
        .unwrap();
        let grant = consent_request(&manifest);
        let err = validate_handshake_capabilities(
            &manifest,
            &grant,
            &["handshake".into(), "scanLibrary".into()],
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("scanLibrary"), "{err}");
    }

    #[test]
    fn validate_handshake_allows_core_entrypoint_methods() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "demo"
kind = "integration"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.methods]
list = ["handshake", "health", "diagnose", "onEvent", "cli"]
"#,
        )
        .unwrap();
        let grant = consent_request(&manifest);
        validate_handshake_capabilities(
            &manifest,
            &grant,
            &[
                "handshake".into(),
                "health".into(),
                "start".into(),
                "cli".into(),
            ],
            None,
        )
        .unwrap();
    }

    #[test]
    fn python_outbound_consent_includes_pyodide_hosts() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 2
id = "echo_workerd_python"
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

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let grant = consent_request(&manifest);
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
        narrow.kind = grant.kind.clone();
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
}
