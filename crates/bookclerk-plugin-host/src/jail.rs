//! What a plugin guest may reach, and how the host makes that stick.
//!
//! # The boundary
//!
//! A guest is handed three directories and nothing else:
//!
//! - its own install directory, read-only — the binary and `plugin.toml`
//! - `…/plugins/<id>/data`, its private state, also exported as `HOME`
//! - `…/plugins/<id>/tmp`, its scratch, exported as `TMPDIR`
//!
//! Fetch scratch is a subdirectory of that `tmp` (the host passes it as
//! `cache_dir` on `fetchTitle`). Destinations ingest bytes over Cap'n Proto
//! streams, so they never need a host path inside the jail.
//!
//! That leaves out everything that matters: `master.key`, the operator token,
//! the output library, the config file, the download cache root, and every
//! other plugin's data directory. The **sqlite** database guest is the
//! exception: it gets file-level write grants for `library.db` and its
//! `-wal`/`-shm`/`-journal` sidecars — never the files-dir parent (which would
//! expose `master.key`). Other guests never see the database; credentials and
//! scan results stay on RPC.
//!
//! # Why fetch scratch lives under plugin `tmp` rather than the cache root
//!
//! A guest is long-lived: one process per plugin, serving every call for the
//! life of the daemon. Filesystem confinement is fixed at spawn and cannot
//! grow a new host-cache directory per `fetchTitle`. Granting the whole cache
//! would let one plugin read or overwrite every other fetch's scratch. Plugin
//! `tmp` is already in the spawn allowlist and is this guest's principal only.
//! v1 passed a per-call directory over `SCM_RIGHTS` on fd 3; v2 does not arm
//! that channel (workerd cannot `recvmsg`; destinations stream).
//!
//! # Why a launcher
//!
//! The guest cannot be asked to confine itself; see the `bookclerk-jail` crate
//! docs for why, and for what permitting `execve` costs.

use std::path::{Path, PathBuf};

use bookclerk_config::{Config, Isolation};
use bookclerk_sandbox::{Enforcement, NetPolicy, Spec};

use crate::discover::DiscoveredPlugin;
use crate::manifest::JailNetworkNeed;
use crate::{PluginError, PluginGrant, PluginRuntimeKind, Result};

/// Launcher binary that applies the jail.
const JAIL_BIN_NAME: &str = "bookclerk-jail";
/// Override for the launcher path, folded into `[plugins].jail_bin` by config.
const JAIL_BIN_ENV: &str = "BOOKCLERK_PLUGIN_JAIL";

/// The private directory a plugin keeps state in.
///
/// `plugin_id` must satisfy [`bookclerk_plugin_manifest::validate_plugin_id`];
/// invalid ids are rejected (no lossy rewriting).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn plugin_data_dir(config: &Config, plugin_id: &str) -> Result<PathBuf> {
    Ok(plugin_state_root(config, plugin_id)?.join("data"))
}

/// Scratch space for one plugin, used as its `TMPDIR`.
///
/// Guests inherit `TMPDIR` from the host otherwise, which names a directory
/// outside every jail — so a guest reaching for a temp file would fail on a
/// permission error unrelated to anything it was denied.
fn plugin_scratch_dir(config: &Config, plugin_id: &str) -> Result<PathBuf> {
    Ok(plugin_state_root(config, plugin_id)?.join("tmp"))
}

/// Where one plugin's host-managed directories live.
///
/// Distinct from [`DiscoveredPlugin::root`], which is where the plugin is
/// installed and is read-only to the guest.
fn plugin_state_root(config: &Config, plugin_id: &str) -> Result<PathBuf> {
    Ok(config
        .paths()
        .files_dir
        .join("plugins")
        .join(validated_plugin_id(plugin_id)?))
}

/// Default host budget for each of `plugins/<id>/data` and `plugins/<id>/tmp`.
///
/// Checked at jail plan (spawn/reload) so a guest whose `data`/`tmp` already
/// exceeds the budget cannot start. Operators may raise or lower this per plugin
/// via consent `diskMib`, still clamped to [`crate::consent::PLUGIN_STATE_BUDGET_MIB_MAX`].
pub(crate) const PLUGIN_STATE_BUDGET_BYTES: u64 =
    (crate::consent::PLUGIN_STATE_BUDGET_MIB_DEFAULT as u64) * 1024 * 1024;

/// Shallow recursive size used for availability budgets (best-effort).
///
/// Uses `symlink_metadata` so a guest cannot force the host to walk outside
/// `data/` / `tmp/` via directory symlinks.
fn dir_size_bytes(root: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                // Count the symlink node itself; do not follow.
                total = total.saturating_add(meta.len());
            } else if ft.is_dir() {
                stack.push(path);
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// Fail closed when `data` or `scratch` exceeds the default host disk budget.
#[allow(dead_code)] // retained for callers/tests that want the default ceiling
pub(crate) fn ensure_plugin_state_within_budget(
    plugin_id: &str,
    data: &Path,
    scratch: &Path,
) -> Result<()> {
    ensure_plugin_state_within_budget_limit(plugin_id, data, scratch, PLUGIN_STATE_BUDGET_BYTES)
}

/// Same as [`ensure_plugin_state_within_budget`] with an explicit byte limit
/// (tests use a tiny ceiling so they need not grow past 512 MiB).
pub(crate) fn ensure_plugin_state_within_budget_limit(
    plugin_id: &str,
    data: &Path,
    scratch: &Path,
    limit_bytes: u64,
) -> Result<()> {
    for dir in [data, scratch] {
        // Missing dirs count as empty; plan creates them before this runs.
        let used = match dir_size_bytes(dir) {
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => {
                return Err(PluginError::message(format!(
                    "could not measure plugin `{plugin_id}` state directory {}: {err}",
                    dir.display()
                )));
            }
        };
        if used > limit_bytes {
            tracing::error!(
                plugin = %plugin_id,
                path = %dir.display(),
                used_bytes = used,
                limit_bytes,
                "plugin state directory exceeds host disk budget"
            );
            return Err(PluginError::message(format!(
                "plugin `{plugin_id}` state directory {} is {used} bytes \
                 (limit {limit_bytes}); clear it before reload",
                dir.display()
            )));
        }
    }
    Ok(())
}

/// How a guest will be started.
#[derive(Debug)]
pub(crate) enum Start {
    /// Through the launcher, which applies `spec` and then becomes the guest.
    Confined {
        /// Path to the `bookclerk-jail` launcher binary.
        launcher: PathBuf,
        /// Confinement policy applied before `exec`.
        spec: Box<Spec>,
    },
    /// Directly, with no jail. Only reachable when the operator turned isolation
    /// off, or asked for best-effort on a host that cannot confine.
    Unconfined {
        /// Operator-visible reason confinement was skipped.
        reason: String,
    },
}

/// A guest's directories plus the decision about how to start it.
#[derive(Debug)]
pub(crate) struct GuestJail {
    /// Private state directory, also the guest's `HOME`.
    pub data: PathBuf,
    /// Scratch directory, the guest's `TMPDIR`.
    pub scratch: PathBuf,
    /// Confined launcher + spec, or an unconfined start with the skip reason.
    pub start: Start,
    /// AppContainer Package SID (SDDL) when the guest will run confined on Windows.
    #[cfg(windows)]
    pub package_sid: Option<String>,
    /// Host-owned AppContainer profile; deleted when the plugin client drops.
    #[cfg(windows)]
    pub appcontainer: Option<bookclerk_sandbox::spawn::AppContainerSession>,
}

impl GuestJail {
    /// Decide how `plugin` will run, creating the directories it is granted.
    ///
    /// # Errors
    ///
    /// Returns an error when `[plugins].isolation` is `required` and the jail
    /// cannot be applied — a missing launcher, or a host with no backend. The
    /// caller skips the plugin, which is the point: a storefront guest parses
    /// hostile input, so running it unconfined is worse than not running it.
    pub(crate) fn plan(config: &Config, plugin: &DiscoveredPlugin) -> Result<Self> {
        let id = &plugin.manifest.id;
        let data = plugin_data_dir(config, id)?;
        let scratch = plugin_scratch_dir(config, id)?;

        for dir in [&data, &scratch] {
            std::fs::create_dir_all(dir).map_err(|err| {
                PluginError::message(format!("could not create {}: {err}", dir.display()))
            })?;
        }
        // Availability: refuse spawn/reload when state already exceeds the host
        // budget (runaway tmp from a previous session).
        let grant = crate::consent::spawn_grant(&config.paths().files_dir, &plugin.manifest).ok();
        let disk_budget = crate::consent::effective_disk_budget_bytes(grant.as_ref());
        ensure_plugin_state_within_budget_limit(id, &data, &scratch, disk_budget)?;
        // Fail closed while planning: a missing/unwritable local output root
        // must not become a late, opaque guest IO failure after jail start.
        if plugin.manifest.kind == crate::PluginKind::Output
            && plugin.manifest.id == "local"
            && config.output.local.enabled
        {
            let root = resolved_local_output_root(config);
            std::fs::create_dir_all(&root).map_err(|err| {
                PluginError::message(format!(
                    "could not create local output root {}: {err}",
                    root.display()
                ))
            })?;
        }
        // SQLite needs the DB + journal sidecars to exist before the confinement
        // backend attaches file rules: Landlock opens each path with O_PATH at
        // confine time; AppContainer ACLs are likewise set on existing paths.
        if is_sqlite_database_plugin(plugin) {
            ensure_sqlite_library_files(config).map_err(|err| {
                PluginError::message(format!("could not prepare sqlite library files: {err}"))
            })?;
        }

        let isolation = config.plugins.isolation;
        #[cfg(windows)]
        let mut package_sid = None;
        #[cfg(windows)]
        let mut appcontainer = None;
        let start = match isolation {
            Isolation::Off => Start::Unconfined {
                reason: "[plugins].isolation = off".to_string(),
            },
            Isolation::Required | Isolation::BestEffort => {
                let enforcement = if isolation == Isolation::Required {
                    Enforcement::Required
                } else {
                    Enforcement::BestEffort
                };
                match resolve_launcher(config, isolation) {
                    Ok(launcher) => {
                        #[cfg(windows)]
                        {
                            let label = format!("plugin:{id}");
                            match bookclerk_sandbox::spawn::AppContainerSession::create(&label) {
                                Ok(session) => {
                                    package_sid = Some(session.package_sid().to_string());
                                    appcontainer = Some(session);
                                }
                                Err(err) if isolation == Isolation::BestEffort => {
                                    return Ok(Self {
                                        data,
                                        scratch,
                                        start: Start::Unconfined {
                                            reason: format!(
                                                "AppContainer profile unavailable: {err}"
                                            ),
                                        },
                                        package_sid: None,
                                        appcontainer: None,
                                    });
                                }
                                Err(err) => {
                                    return Err(PluginError::message(format!(
                                        "could not create AppContainer session for `{id}`: {err}"
                                    )));
                                }
                            }
                        }
                        // v2 does not pass per-RPC descriptors; fetch scratch is
                        // plugin `tmp` and sqlite paths are spawn-time grants.
                        let preserve_fds: Vec<i32> = Vec::new();

                        #[cfg(windows)]
                        let windows_profile_name =
                            appcontainer.as_ref().map(|s| s.profile_name().to_string());
                        #[cfg(not(windows))]
                        let windows_profile_name = None;

                        Start::Confined {
                            launcher,
                            spec: Box::new(build_spec_with_grant(
                                plugin,
                                config,
                                &data,
                                &scratch,
                                preserve_fds,
                                enforcement,
                                windows_profile_name,
                                grant.as_ref(),
                            )),
                        }
                    }
                    Err(reason) if isolation == Isolation::BestEffort => {
                        Start::Unconfined { reason }
                    }
                    Err(reason) => {
                        return Err(PluginError::message(format!(
                            "refusing to run plugin `{id}` unconfined: {reason}. \
                             Set [plugins].isolation = \"best-effort\" to allow it anyway"
                        )))
                    }
                }
            }
        };

        Ok(Self {
            data,
            scratch,
            start,
            #[cfg(windows)]
            package_sid,
            #[cfg(windows)]
            appcontainer,
        })
    }
}

/// Build the allowlist for one guest.
#[cfg(test)]
fn build_spec(
    plugin: &DiscoveredPlugin,
    config: &Config,
    data: &Path,
    scratch: &Path,
    preserve_fds: Vec<i32>,
    enforcement: Enforcement,
    windows_profile_name: Option<String>,
) -> Spec {
    build_spec_with_grant(
        plugin,
        config,
        data,
        scratch,
        preserve_fds,
        enforcement,
        windows_profile_name,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
/// Builds a jail `Spec`: install/command reads, data/tmp (and granted output/SQLite) writes, and grant-derived net/resources.
fn build_spec_with_grant(
    plugin: &DiscoveredPlugin,
    config: &Config,
    data: &Path,
    scratch: &Path,
    preserve_fds: Vec<i32>,
    enforcement: Enforcement,
    windows_profile_name: Option<String>,
    grant: Option<&PluginGrant>,
) -> Spec {
    let mut writes = vec![data.to_path_buf(), scratch.to_path_buf()];
    // Local output writes under `[output.local].root`; grant only that tree.
    // Require kind == Output so a non-output plugin cannot claim id "local".
    if plugin.manifest.kind == crate::PluginKind::Output
        && plugin.manifest.id == "local"
        && config.output.local.enabled
    {
        // Directory creation happens in [`GuestJail::plan`] (hard error).
        writes.push(resolved_local_output_root(config));
    }
    // File-level grants only — never the files-dir parent (see module docs).
    if is_sqlite_database_plugin(plugin) {
        writes.extend(sqlite_library_paths(config));
    }
    let mut resources = guest_spec_resource_limits(plugin, grant);
    // Global jail knobs only override resource ceilings. Guest filesystems remain
    // install read-only plus host-managed data/tmp grants, not free-form paths.
    apply_global_jail_resource_overrides(
        &mut resources,
        &config.plugins.jail,
        plugin.manifest.runtime,
    );
    Spec {
        label: format!("plugin:{}", plugin.manifest.id),
        // The install directory covers `plugin.toml` and, in the usual layout,
        // the binary. A manifest may name an absolute `command` elsewhere, so
        // grant that too rather than relying on the two coinciding.
        reads: {
            let mut reads = vec![plugin.root.clone(), plugin.command.clone()];
            // workerd guests also exec the pinned Cloudflare `workerd` beside
            // `bookclerk-workerd` (or BOOKCLERK_WORKERD_BIN).
            if plugin.manifest.runtime == crate::PluginRuntimeKind::Workerd {
                if let Some(parent) = plugin.command.parent() {
                    reads.push(parent.join(cloudflare_workerd_bin_name()));
                }
                if let Ok(override_bin) = std::env::var("BOOKCLERK_WORKERD_BIN") {
                    reads.push(PathBuf::from(override_bin));
                }
            }
            reads
        },
        writes,
        net: jail_net_policy(plugin, grant),
        // The launcher has to exec the guest to hand over. See the
        // `bookclerk-jail` crate docs on why this is close to free.
        // On Windows, `allow_exec` is not separately enforceable at CreateProcess;
        // path ACLs and low integrity remain the boundary (see windows_spawn docs).
        allow_exec: true,
        system_paths: true,
        enforcement,
        preserve_fds,
        windows_profile_name,
        memory_bytes: resources.memory_bytes,
        active_processes: resources.active_processes,
        cpu_rate_percent: resources.cpu_rate_percent,
    }
}

/// Tightens guest memory/CPU/process ceilings using `[plugins.jail]` without widening filesystem grants.
fn apply_global_jail_resource_overrides(
    resources: &mut bookclerk_sandbox::ResourceLimits,
    jail: &bookclerk_config::PluginsJailConfig,
    runtime: PluginRuntimeKind,
) {
    use crate::consent::{
        active_processes_for, effective_extra_processes, jail_process_overhead,
        PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT,
    };

    let host_max = bookclerk_sandbox::host_cpu_rate_max();
    if let Some(memory_mib) = jail.memory_mib {
        let host = memory_mib.saturating_mul(1024 * 1024);
        resources.memory_bytes = Some(match resources.memory_bytes {
            Some(guest) => guest.min(host),
            None => host,
        });
    }
    // Always clamp Spec CPU to physical host max (one-core units).
    if let Some(cpu) = resources.cpu_rate_percent {
        resources.cpu_rate_percent = Some(cpu.clamp(1, host_max));
    }
    if let Some(cpu_rate_percent) = jail.cpu_rate_percent {
        let ceiling = cpu_rate_percent.clamp(1, host_max);
        resources.cpu_rate_percent = Some(match resources.cpu_rate_percent {
            Some(guest) => guest.min(ceiling),
            None => ceiling,
        });
    }
    if let Some(global_extra) = jail.extra_processes {
        let ceiling = effective_extra_processes(Some(global_extra));
        let overhead = jail_process_overhead(runtime);
        let current_extra = match resources.active_processes {
            Some(abs) => abs.saturating_sub(overhead),
            None => PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT,
        };
        let extra = current_extra.min(ceiling);
        resources.active_processes = Some(active_processes_for(runtime, extra));
    }
}

/// Maps manifest network need and a deny grant onto Landlock/AppContainer `NetPolicy` (workerd Listen stays `OutboundListen`).
fn jail_net_policy(plugin: &DiscoveredPlugin, grant: Option<&PluginGrant>) -> NetPolicy {
    let denied = grant.is_some_and(|g| g.network_mode.eq_ignore_ascii_case("deny"));
    match plugin.manifest.jail_network_need() {
        JailNetworkNeed::Listen if plugin.manifest.runtime == PluginRuntimeKind::Workerd => {
            // Intentional OS-jail exception (see docs/adr/plugin-workers-rpc-workerd.md):
            // `bookclerk-workerd` must `bind(127.0.0.1:0)` for the host↔isolate RPC
            // bridge. Linux Landlock has no loopback-only policy, so `OutboundListen`
            // also permits `connect`. Isolate egress (`WORKERD_GRANT_NETWORK_MODE` →
            // `globalOutbound = blocked` under deny) remains the grant enforcement
            // layer. Native Listen guests never take this branch.
            NetPolicy::OutboundListen
        }
        JailNetworkNeed::Listen => {
            if denied {
                NetPolicy::Deny
            } else {
                NetPolicy::OutboundListen
            }
        }
        JailNetworkNeed::None => NetPolicy::Deny,
        JailNetworkNeed::Outbound => {
            if denied {
                NetPolicy::Deny
            } else {
                NetPolicy::Outbound
            }
        }
    }
}

/// Map grant (and host defaults) onto jail Spec resource fields.
///
/// Applies to **native and workerd** confined guests:
///
/// - `memory_bytes` from grant `memoryMib` (default 512 MiB)
/// - `active_processes` = overhead(runtime) + extra budget (default extra 2;
///   native grant `extraProcesses`; workerd uses default extra only)
/// - `cpu_rate_percent`: **native** from grant `cpuRatePercent` (default 80);
///   **workerd** always uses the host default (80) so isolate budgets stay on
///   `cpu_ms`. `[plugins.jail]` then applies as a per-jail ceiling.
fn guest_spec_resource_limits(
    plugin: &DiscoveredPlugin,
    grant: Option<&PluginGrant>,
) -> bookclerk_sandbox::ResourceLimits {
    use crate::consent::{
        active_processes_for, effective_cpu_rate_percent, effective_extra_processes,
        effective_memory_mib,
    };

    let memory_mib = effective_memory_mib(grant.and_then(|g| g.memory_mib));
    let runtime = plugin.manifest.runtime;
    let extra = if runtime == PluginRuntimeKind::Workerd {
        // Workerd process headroom is host-managed (not a per-plugin consent knob).
        effective_extra_processes(None)
    } else {
        effective_extra_processes(grant.and_then(|g| g.extra_processes))
    };

    let cpu_rate = if runtime == PluginRuntimeKind::Workerd {
        // Isolate-facing budget is cpu_ms; jail CPU is host per-jail policy only.
        effective_cpu_rate_percent(None)
    } else {
        effective_cpu_rate_percent(grant.and_then(|g| g.cpu_rate_percent))
    };

    bookclerk_sandbox::ResourceLimits {
        memory_bytes: Some(u64::from(memory_mib).saturating_mul(1024 * 1024)),
        active_processes: Some(active_processes_for(runtime, extra)),
        cpu_rate_percent: Some(cpu_rate),
    }
}

/// True when this guest is the `sqlite` database plugin and may be granted `library.db` sidecars.
fn is_sqlite_database_plugin(plugin: &DiscoveredPlugin) -> bool {
    plugin.manifest.kind == crate::PluginKind::Database
        && plugin.manifest.id.eq_ignore_ascii_case("sqlite")
}

/// Filename of the pinned Cloudflare `workerd` binary (`workerd.exe` on Windows).
fn cloudflare_workerd_bin_name() -> &'static str {
    if cfg!(windows) {
        "workerd.exe"
    } else {
        "workerd"
    }
}

/// `library.db` plus the journal sidecars SQLite opens beside it.
///
/// The library connection uses `PRAGMA journal_mode=TRUNCATE` so commits
/// truncate `*-journal` instead of unlinking it (Landlock would deny
/// `RemoveFile` on the files-dir parent; AppContainer ACLs apply the same
/// constraint on Windows). `-wal`/`-shm` are included for completeness if a
/// connection ever switches to WAL.
fn sqlite_library_paths(config: &Config) -> Vec<PathBuf> {
    let db = config.database.sqlite_path(&config.paths().files_dir);
    let wal = {
        let mut s = db.as_os_str().to_os_string();
        s.push("-wal");
        PathBuf::from(s)
    };
    let shm = {
        let mut s = db.as_os_str().to_os_string();
        s.push("-shm");
        PathBuf::from(s)
    };
    let journal = {
        let mut s = db.as_os_str().to_os_string();
        s.push("-journal");
        PathBuf::from(s)
    };
    vec![db, wal, shm, journal]
}

/// Touch the SQLite DB and sidecars so the confinement backend can attach
/// per-file rules (Landlock opens each path with `O_PATH`; AppContainer ACLs
/// are set on existing paths).
fn ensure_sqlite_library_files(config: &Config) -> std::io::Result<()> {
    let paths = sqlite_library_paths(config);
    if let Some(parent) = paths[0].parent() {
        std::fs::create_dir_all(parent)?;
    }
    for path in &paths {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
    }
    Ok(())
}

/// Locate the launcher: the configured path, then [`JAIL_BIN_ENV`], then beside
/// the current executable.
fn resolve_launcher(config: &Config, isolation: Isolation) -> std::result::Result<PathBuf, String> {
    if isolation == Isolation::Required {
        let caps = bookclerk_sandbox::capabilities();
        // Linux/macOS self-confine (`filesystem`); Windows AppContainer at
        // CreateProcess (`spawn_filesystem`).
        if !caps.can_confine_guest() {
            return Err(format!(
                "this host cannot confine a process ({}) [{}]",
                caps.detail, caps.backend
            ));
        }
    }

    if let Some(path) = config.plugins.jail_bin.as_deref() {
        // Config folds the environment variable into `jail_bin` before we see
        // it, so name both rather than pointing at a config.toml that may never
        // have mentioned this path.
        return check_launcher(path, "plugins.jail_bin (or BOOKCLERK_PLUGIN_JAIL)");
    }
    if let Some(path) = std::env::var_os(JAIL_BIN_ENV) {
        return check_launcher(Path::new(&path), JAIL_BIN_ENV);
    }

    let exe = std::env::current_exe()
        .map_err(|err| format!("could not locate the current executable: {err}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let name = format!("{JAIL_BIN_NAME}{}", std::env::consts::EXE_SUFFIX);
    if dir.join(&name).is_file() {
        return Ok(dir.join(name));
    }
    // An integration test binary runs from `target/<profile>/deps`, one level
    // below where cargo puts the launcher.
    if dir.file_name().is_some_and(|last| last == "deps") {
        if let Some(parent) = dir.parent() {
            if parent.join(&name).is_file() {
                return Ok(parent.join(name));
            }
        }
    }
    Err(format!(
        "{JAIL_BIN_NAME} not found in {} and {JAIL_BIN_ENV} is unset",
        dir.display()
    ))
}

/// Accepts `path` as the jail launcher when it is a file; otherwise returns a source-labeled error.
fn check_launcher(path: &Path, source: &str) -> std::result::Result<PathBuf, String> {
    if path.is_file() {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "{source} points at {}, which is not a file",
            path.display()
        ))
    }
}

/// Absolute `[output.local].root`, joined to `files_dir` when the config path is relative.
fn resolved_local_output_root(config: &Config) -> PathBuf {
    let root = &config.output.local.root;
    if root.is_absolute() {
        root.clone()
    } else {
        config.paths().files_dir.join(root)
    }
}

/// Accepted plugin ids are used as path segments with no rewriting.
///
/// Invalid ids are rejected — lossy mapping (e.g. `/` → `_`) is forbidden so
/// distinct raw ids cannot collide after sanitization.
fn validated_plugin_id(id: &str) -> Result<&str> {
    crate::registry::validate_plugin_id(id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_config::Paths;

    fn config_at(files: &Path) -> Config {
        Config {
            paths: Some(Paths::from_files_dir(files.to_path_buf())),
            ..Default::default()
        }
    }

    fn plugin_at(root: &Path, id: &str, network: JailNetworkNeed) -> DiscoveredPlugin {
        use crate::manifest::{
            BindingCapabilities, CapabilitiesManifest, NetworkCapabilities, NetworkMode,
            PluginRuntimeKind,
        };
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").expect("write guest");
        let (mode, oauth) = match network {
            JailNetworkNeed::None => (NetworkMode::Deny, false),
            JailNetworkNeed::Outbound => (NetworkMode::Outbound, false),
            JailNetworkNeed::Listen => (NetworkMode::Outbound, true),
        };
        let domains = if mode == NetworkMode::Outbound {
            vec!["example.com".into()]
        } else {
            vec![]
        };
        DiscoveredPlugin {
            manifest: crate::PluginManifest {
                api_version: 2,
                id: id.to_string(),
                name: None,
                kind: crate::PluginKind::Source,
                version: Some("0.0.0".into()),
                logo: None,
                runtime: PluginRuntimeKind::Native,
                command: Some(PathBuf::from("./guest")),
                args: vec![],
                workerd: None,
                modules: vec![],
                capabilities: CapabilitiesManifest {
                    network: NetworkCapabilities { mode, domains },
                    bindings: BindingCapabilities {
                        oauth,
                        ..BindingCapabilities::default()
                    },
                    methods: Default::default(),
                    events: Default::default(),
                },
                cli: None,
                oidc: Default::default(),
            },
            root: root.to_path_buf(),
            command,
        }
    }

    fn sqlite_plugin_at(root: &Path) -> DiscoveredPlugin {
        let mut plugin = plugin_at(root, "sqlite", JailNetworkNeed::None);
        plugin.manifest.kind = crate::PluginKind::Database;
        plugin
    }

    #[test]
    fn the_allowlist_covers_the_guest_dirs_and_nothing_else() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = plugin_at(install.path(), "libro", JailNetworkNeed::Outbound);

        let spec = build_spec(
            &plugin,
            &config,
            &plugin_data_dir(&config, "libro").unwrap(),
            &plugin_scratch_dir(&config, "libro").unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
        );

        assert_eq!(spec.label, "plugin:libro");
        assert_eq!(
            spec.net,
            NetPolicy::Outbound,
            "native outbound gets coarse jail outbound"
        );
        assert!(spec.allow_exec, "the launcher has to exec the guest");

        // Nothing that matters is writable.
        let paths = config.paths();
        for forbidden in [
            paths.library_db.clone(),
            paths.files_dir.join("master.key"),
            paths.config_file.clone(),
        ] {
            assert!(
                !spec.writes.iter().any(|w| forbidden.starts_with(w)),
                "{} is under a writable grant: {:?}",
                forbidden.display(),
                spec.writes
            );
            assert!(
                !spec.reads.iter().any(|r| forbidden.starts_with(r)),
                "{} is under a readable grant: {:?}",
                forbidden.display(),
                spec.reads
            );
        }
    }

    /// The sqlite guest needs file-level write grants for the DB and journal
    /// sidecars on all platforms — without handing over the files-dir parent
    /// (`master.key`, config).
    #[test]
    fn sqlite_guest_gets_library_db_files_but_not_secrets() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = sqlite_plugin_at(install.path());
        ensure_sqlite_library_files(&config).expect("touch sqlite files");

        let spec = build_spec(
            &plugin,
            &config,
            &plugin_data_dir(&config, "sqlite").unwrap(),
            &plugin_scratch_dir(&config, "sqlite").unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
        );

        let db_files = sqlite_library_paths(&config);
        for path in &db_files {
            assert!(
                spec.writes.contains(path),
                "missing write grant for {}",
                path.display()
            );
        }

        let paths = config.paths();
        for forbidden in [
            paths.files_dir.join("master.key"),
            paths.config_file.clone(),
        ] {
            assert!(
                !spec.writes.iter().any(|w| forbidden.starts_with(w)),
                "{} is under a writable grant: {:?}",
                forbidden.display(),
                spec.writes
            );
        }
        // Parent directory grant would expose secrets; only the files themselves.
        assert!(!spec.writes.contains(&paths.files_dir));
    }

    #[test]
    fn planning_sqlite_precreates_library_sidecars() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        config.plugins.isolation = Isolation::Off;
        let plugin = sqlite_plugin_at(install.path());

        let _jail = GuestJail::plan(&config, &plugin).expect("plan");
        for path in sqlite_library_paths(&config) {
            assert!(path.is_file(), "expected {}", path.display());
        }
    }

    /// The files dir is the parent of both the cache and every plugin's data
    /// directory, so granting it would hand over the database and the key.
    #[test]
    fn the_files_dir_itself_is_never_granted() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = plugin_at(install.path(), "libro", JailNetworkNeed::Outbound);
        let spec = build_spec(
            &plugin,
            &config,
            &plugin_data_dir(&config, "libro").unwrap(),
            &plugin_scratch_dir(&config, "libro").unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
        );
        assert!(!spec.writes.contains(&config.paths().files_dir));
        assert!(!spec.reads.contains(&config.paths().files_dir));
    }

    #[test]
    fn one_guest_cannot_reach_another_guests_data() {
        let files = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let mine = plugin_data_dir(&config, "libro").unwrap();
        let theirs = plugin_data_dir(&config, "audible").unwrap();
        assert!(!theirs.starts_with(&mine));
        assert!(!mine.starts_with(&theirs));
    }

    #[test]
    fn network_need_maps_to_the_matching_policy() {
        use crate::manifest::{PluginRuntimeKind, WorkerdRuntimeManifest};

        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        // Native: deny / outbound / outbound+oauth → Deny / Outbound / OutboundListen.
        for (need, expected) in [
            (JailNetworkNeed::None, NetPolicy::Deny),
            (JailNetworkNeed::Outbound, NetPolicy::Outbound),
            (JailNetworkNeed::Listen, NetPolicy::OutboundListen),
        ] {
            let plugin = plugin_at(install.path(), "xx", need);
            let spec = build_spec(
                &plugin,
                &config,
                &plugin_data_dir(&config, "xx").unwrap(),
                &plugin_scratch_dir(&config, "xx").unwrap(),
                Vec::new(),
                Enforcement::Required,
                None,
            );
            assert_eq!(spec.net, expected, "{need:?}");
        }

        // Workerd needs loopback listen/connect to its Cloudflare child.
        let mut workerd = plugin_at(install.path(), "echo", JailNetworkNeed::None);
        workerd.manifest.runtime = PluginRuntimeKind::Workerd;
        workerd.manifest.command = None;
        workerd.manifest.workerd = Some(WorkerdRuntimeManifest {
            compatibility_date: "2026-08-01".into(),
            compatibility_flags: vec![],
            main_module: "index.js".into(),
            modules_dir: "modules".into(),
            entrypoint: "default".into(),
            limits: Default::default(),
        });
        assert_eq!(
            workerd.manifest.jail_network_need(),
            JailNetworkNeed::Listen
        );
        let spec = build_spec(
            &workerd,
            &config,
            &plugin_data_dir(&config, "echo").unwrap(),
            &plugin_scratch_dir(&config, "echo").unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
        );
        assert_eq!(spec.net, NetPolicy::OutboundListen);

        // Operator `deny` must not strip the loopback RPC bind; isolate egress
        // still honours the grant via WORKERD_GRANT_NETWORK_MODE.
        let deny = PluginGrant {
            plugin_id: "echo".into(),
            kind: "integration".into(),
            network_mode: "deny".into(),
            domains: Default::default(),
            bindings: Default::default(),
            compatibility_flags: Default::default(),
            cpu_ms: None,
            subrequests: None,
            disk_mib: None,
            memory_mib: None,
            cpu_rate_percent: None,
            extra_processes: None,
            approved_at: "2026-01-01T00:00:00Z".into(),
        };
        let denied = build_spec_with_grant(
            &workerd,
            &config,
            &plugin_data_dir(&config, "echo").unwrap(),
            &plugin_scratch_dir(&config, "echo").unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
            Some(&deny),
        );
        assert_eq!(denied.net, NetPolicy::OutboundListen);

        // Native OAuth Listen + stored deny grant stays OS-Deny (no workerd
        // bridge exception).
        let native_listen = plugin_at(install.path(), "oauth", JailNetworkNeed::Listen);
        let native_denied = build_spec_with_grant(
            &native_listen,
            &config,
            &plugin_data_dir(&config, "oauth").unwrap(),
            &plugin_scratch_dir(&config, "oauth").unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
            Some(&PluginGrant {
                plugin_id: "oauth".into(),
                kind: "source".into(),
                network_mode: "deny".into(),
                domains: Default::default(),
                bindings: Default::default(),
                compatibility_flags: Default::default(),
                cpu_ms: None,
                subrequests: None,
                disk_mib: None,
                memory_mib: None,
                cpu_rate_percent: None,
                extra_processes: None,
                approved_at: "2026-01-01T00:00:00Z".into(),
            }),
        );
        assert_eq!(native_denied.net, NetPolicy::Deny);
    }

    #[test]
    fn global_jail_limits_ceiling_native_guest_resources() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        config.plugins.jail.memory_mib = Some(256);
        config.plugins.jail.cpu_rate_percent = Some(250);
        config.plugins.jail.extra_processes = Some(1);
        let native = plugin_at(install.path(), "native", JailNetworkNeed::None);
        let spec = build_spec(
            &native,
            &config,
            &plugin_data_dir(&config, "native").unwrap(),
            &plugin_scratch_dir(&config, "native").unwrap(),
            vec![],
            Enforcement::Required,
            None,
        );
        // Host knobs are ceilings: min(default 512, 256), min(80, host_max for 250),
        // extra min(2, 1) → active = 1 + 1 = 2. 250 clamps to host_max then min with 80.
        assert_eq!(spec.memory_bytes, Some(256 * 1024 * 1024));
        assert_eq!(spec.cpu_rate_percent, Some(80));
        assert_eq!(spec.active_processes, Some(2));
    }

    #[test]
    fn workerd_jail_cpu_uses_host_default_not_cpu_ms_heuristic() {
        use crate::manifest::{PluginRuntimeKind, WorkerdLimits, WorkerdRuntimeManifest};

        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());

        let native = plugin_at(install.path(), "native", JailNetworkNeed::None);
        let native_spec = build_spec(
            &native,
            &config,
            &plugin_data_dir(&config, "native").unwrap(),
            &plugin_scratch_dir(&config, "native").unwrap(),
            vec![],
            Enforcement::Required,
            None,
        );
        // Native: overhead 1 + default extra 2 = 3. Workerd: overhead 2 + extra 2 = 4.
        assert_eq!(native_spec.memory_bytes, Some(512 * 1024 * 1024));
        assert_eq!(native_spec.active_processes, Some(3));
        assert_eq!(native_spec.cpu_rate_percent, Some(80));

        let native_with_grant = build_spec_with_grant(
            &native,
            &config,
            &plugin_data_dir(&config, "native").unwrap(),
            &plugin_scratch_dir(&config, "native").unwrap(),
            vec![],
            Enforcement::Required,
            None,
            Some(&PluginGrant {
                plugin_id: "native".into(),
                kind: "integration".into(),
                network_mode: "deny".into(),
                domains: Default::default(),
                bindings: Default::default(),
                compatibility_flags: Default::default(),
                cpu_ms: None,
                subrequests: None,
                disk_mib: Some(512),
                memory_mib: Some(256),
                cpu_rate_percent: Some(40),
                extra_processes: Some(4),
                approved_at: "2026-01-01T00:00:00Z".into(),
            }),
        );
        assert_eq!(native_with_grant.memory_bytes, Some(256 * 1024 * 1024));
        assert_eq!(native_with_grant.cpu_rate_percent, Some(40));
        // Clamped by default global extra_processes = 2 → active = 1 + 2 = 3.
        assert_eq!(native_with_grant.active_processes, Some(3));

        let mut workerd = plugin_at(install.path(), "echo", JailNetworkNeed::None);
        workerd.manifest.runtime = PluginRuntimeKind::Workerd;
        workerd.manifest.command = None;
        workerd.manifest.workerd = Some(WorkerdRuntimeManifest {
            compatibility_date: "2026-08-01".into(),
            compatibility_flags: vec![],
            main_module: "index.js".into(),
            modules_dir: "modules".into(),
            entrypoint: "default".into(),
            limits: WorkerdLimits {
                cpu_ms: Some(15_000),
                subrequests: None,
            },
        });
        let default_spec = build_spec(
            &workerd,
            &config,
            &plugin_data_dir(&config, "echo").unwrap(),
            &plugin_scratch_dir(&config, "echo").unwrap(),
            vec![],
            Enforcement::Required,
            None,
        );
        assert_eq!(default_spec.memory_bytes, Some(512 * 1024 * 1024));
        // Workerd: overhead 2 + default extra 2 = 4.
        assert_eq!(default_spec.active_processes, Some(4));
        // Workerd isolate budget is cpu_ms; jail CPU stays at host default (80),
        // not the old cpu_ms → rate heuristic (which would have been 40).
        assert_eq!(default_spec.cpu_rate_percent, Some(80));

        let mut config_ceil = config_at(files.path());
        config_ceil.plugins.jail.cpu_rate_percent = Some(25);
        let capped = build_spec(
            &workerd,
            &config_ceil,
            &plugin_data_dir(&config_ceil, "echo").unwrap(),
            &plugin_scratch_dir(&config_ceil, "echo").unwrap(),
            vec![],
            Enforcement::Required,
            None,
        );
        assert_eq!(capped.cpu_rate_percent, Some(25));
    }

    #[test]
    fn native_grant_may_request_multi_core_cpu_rate() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        let native = plugin_at(install.path(), "native", JailNetworkNeed::None);
        let host_max = bookclerk_sandbox::host_cpu_rate_max();
        let want = host_max.clamp(80, 200);
        // Default `[plugins.jail].cpu_rate_percent` is 80; raise the host ceiling
        // so a native grant can actually request more than one-core-default.
        config.plugins.jail.cpu_rate_percent = Some(host_max);
        let spec = build_spec_with_grant(
            &native,
            &config,
            &plugin_data_dir(&config, "native").unwrap(),
            &plugin_scratch_dir(&config, "native").unwrap(),
            vec![],
            Enforcement::Required,
            None,
            Some(&PluginGrant {
                plugin_id: "native".into(),
                kind: "integration".into(),
                network_mode: "deny".into(),
                domains: Default::default(),
                bindings: Default::default(),
                compatibility_flags: Default::default(),
                cpu_ms: None,
                subrequests: None,
                disk_mib: Some(512),
                memory_mib: Some(512),
                cpu_rate_percent: Some(want),
                extra_processes: Some(2),
                approved_at: "2026-01-01T00:00:00Z".into(),
            }),
        );
        assert_eq!(spec.cpu_rate_percent, Some(want));
    }

    /// Hostile / non-grammar ids are rejected (no lossy rewrite). Path
    /// containment for valid ids remains: state lives under `plugins/<id>/`.
    #[test]
    fn invalid_plugin_ids_are_rejected_not_rewritten() {
        let files = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        for hostile in ["../../etc", "..", ".", "a/b", "/absolute", "a-b", "a__b"] {
            let err = plugin_data_dir(&config, hostile).expect_err("must reject");
            assert!(
                err.to_string().contains("plugin id"),
                "id {hostile:?} got: {err}"
            );
        }
        // Valid id is identity under plugins/.
        let data = plugin_data_dir(&config, "echo").unwrap();
        assert!(data.starts_with(files.path().join("plugins").join("echo")));
        assert!(!data.to_string_lossy().contains(".."));
    }

    /// The download cache root is never granted; fetch scratch is plugin `tmp`.
    #[test]
    fn the_cache_root_is_never_granted() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = plugin_at(install.path(), "libro", JailNetworkNeed::Outbound);
        let spec = build_spec(
            &plugin,
            &config,
            &plugin_data_dir(&config, "libro").unwrap(),
            &plugin_scratch_dir(&config, "libro").unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
        );
        assert!(!spec
            .writes
            .iter()
            .any(|path| path == &config.download_cache_dir()));
    }

    #[test]
    fn planning_creates_the_directories_it_grants() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        // Keep this about directory creation rather than about locating the
        // launcher, which the enforcement tests cover.
        config.plugins.isolation = Isolation::Off;
        let plugin = plugin_at(install.path(), "libro", JailNetworkNeed::Outbound);

        let jail = GuestJail::plan(&config, &plugin).expect("plan");
        assert!(jail.data.is_dir(), "{}", jail.data.display());
        assert!(jail.scratch.is_dir(), "{}", jail.scratch.display());
        assert!(matches!(jail.start, Start::Unconfined { .. }));
    }

    /// `required` must not degrade into an unconfined guest.
    #[test]
    fn required_isolation_refuses_when_the_launcher_is_missing() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        config.plugins.isolation = Isolation::Required;
        config.plugins.jail_bin = Some(files.path().join("no-such-launcher"));
        let plugin = plugin_at(install.path(), "libro", JailNetworkNeed::Outbound);

        let err = GuestJail::plan(&config, &plugin).expect_err("must refuse");
        assert!(err.to_string().contains("refusing to run"), "got: {err}");
    }

    #[test]
    fn best_effort_falls_back_and_says_why() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        config.plugins.isolation = Isolation::BestEffort;
        config.plugins.jail_bin = Some(files.path().join("no-such-launcher"));
        let plugin = plugin_at(install.path(), "libro", JailNetworkNeed::Outbound);

        let jail = GuestJail::plan(&config, &plugin).expect("plan");
        match jail.start {
            Start::Unconfined { reason } => {
                assert!(reason.contains("not a file"), "got: {reason}")
            }
            other => panic!("expected a fallback, got {other:?}"),
        }
    }

    #[test]
    fn state_budget_allows_empty_dirs_and_refuses_growth_past_limit() {
        let root = tempfile::tempdir().expect("tempdir");
        let data = root.path().join("data");
        let scratch = root.path().join("tmp");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();

        ensure_plugin_state_within_budget_limit("echo", &data, &scratch, 64).expect("empty ok");

        std::fs::write(data.join("fat.bin"), vec![0u8; 100]).expect("write");
        let err = ensure_plugin_state_within_budget_limit("echo", &data, &scratch, 64)
            .expect_err("must refuse over budget");
        assert!(err.to_string().contains("state directory"), "got: {err}");
        assert!(err.to_string().contains("limit 64"), "got: {err}");

        // Clear growth → subsequent check succeeds again (reload path).
        std::fs::remove_file(data.join("fat.bin")).unwrap();
        ensure_plugin_state_within_budget_limit("echo", &data, &scratch, 64).expect("cleared");
    }

    #[test]
    fn plan_refuses_when_existing_state_exceeds_budget() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        config.plugins.isolation = Isolation::Off;
        let plugin = plugin_at(install.path(), "libro", JailNetworkNeed::Outbound);

        let data = plugin_data_dir(&config, "libro").unwrap();
        std::fs::create_dir_all(&data).unwrap();
        // Grow past the production 512 MiB ceiling with a sparse-ish write that
        // still counts via `metadata().len()` on a regular file.
        let fat = data.join("fat.bin");
        let file = std::fs::File::create(&fat).expect("create");
        file.set_len(PLUGIN_STATE_BUDGET_BYTES + 1)
            .expect("set_len");
        drop(file);

        let err = GuestJail::plan(&config, &plugin).expect_err("must refuse");
        assert!(err.to_string().contains("state directory"), "got: {err}");
    }
}
