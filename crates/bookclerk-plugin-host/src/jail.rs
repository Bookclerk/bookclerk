//! What a plugin guest may reach, and how the host makes that stick.
//!
//! # The boundary
//!
//! A guest is handed three directories and nothing else:
//!
//! - its own install directory, read-only — the binary and `plugin.toml`
//! - `…/plugin-state/<plugin-key-fs-id>/data`, its private state, also exported as `HOME`
//! - `…/plugin-state/<plugin-key-fs-id>/tmp`, its scratch, exported as `TMPDIR`
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
//! v1 JSON adapters passed a per-call directory over `SCM_RIGHTS` on fd 3; the
//! product ABI does not arm
//! that channel (workerd cannot `recvmsg`; destinations stream).
//!
//! # Why a launcher
//!
//! The guest cannot be asked to confine itself; see the `bookclerk-jail` crate
//! docs for why, and for what permitting `execve` costs.

use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};

use bookclerk_config::{Config, Isolation};
use bookclerk_sandbox::{Enforcement, NetPolicy, Spec};

use crate::discover::DiscoveredPlugin;
use crate::manifest::JailNetworkNeed;
use crate::spawn_plan::{GuestRuntimeKind, SpawnPlan};
use crate::{PluginError, PluginGrant, PluginRuntimeKind, Result};

/// Launcher binary that applies the jail.
const JAIL_BIN_NAME: &str = "bookclerk-jail";
/// Override for the launcher path, folded into `[plugins].jail_bin` by config.
const JAIL_BIN_ENV: &str = "BOOKCLERK_PLUGIN_JAIL";

/// The private directory a plugin keeps state in.
///
/// State is keyed by [`bookclerk_plugin_catalog::PluginKey`] (`plugin-state/<fs_id>/data`), not the
/// display alias. Invalid keys cannot be constructed.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn plugin_data_dir(config: &Config, plugin: &DiscoveredPlugin) -> Result<PathBuf> {
    Ok(plugin_state_root(config, plugin)?.join("data"))
}

/// Scratch space for one plugin, used as its `TMPDIR`.
///
/// Guests inherit `TMPDIR` from the host otherwise, which names a directory
/// outside every jail — so a guest reaching for a temp file would fail on a
/// permission error unrelated to anything it was denied.
fn plugin_scratch_dir(config: &Config, plugin: &DiscoveredPlugin) -> Result<PathBuf> {
    Ok(plugin_state_root(config, plugin)?.join("tmp"))
}

/// Where one plugin's host-managed directories live.
///
/// Distinct from [`DiscoveredPlugin::root`], which is where the plugin is
/// installed and is read-only to the guest.
fn plugin_state_root(config: &Config, plugin: &DiscoveredPlugin) -> Result<PathBuf> {
    Ok(config
        .paths()
        .files_dir
        .join("plugin-state")
        .join(plugin.plugin_key().fs_id()))
}

/// Default host budget for each of `plugin-state/<fs_id>/data` and `…/tmp`.
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
        if !dir.starts_with(root) {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.starts_with(root) {
                continue;
            }
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

/// Directories and start decisions for one plugin session.
///
/// Native-behind-workerd fills [`Self::guest_start`]: the host spawns two
/// sibling jails. Isolates and direct-native use only [`Self::start`].
#[derive(Debug)]
pub(crate) struct GuestJail {
    /// Private state directory, also the native guest's `HOME`.
    pub data: PathBuf,
    /// Scratch directory, the native guest's `TMPDIR`.
    pub scratch: PathBuf,
    /// Host-owned session directory (`0700`) for the gateway's `TMPDIR` /
    /// `BOOKCLERK_WORKERD_STATE_DIR`. `None` when this session has no sibling.
    pub session_dir: Option<PathBuf>,
    /// Child the host speaks Cap'n Proto to (gateway jail, isolate, or direct).
    pub start: Start,
    /// Native sibling jail when [`crate::GuestRuntimeKind::NativeBehindWorkerd`].
    pub guest_start: Option<Start>,
    /// Aggregate session resource ceilings.
    ///
    /// Process-count ceilings (overhead + extra). Linux `pids.max` is
    /// [`session_cgroup_thread_limits`], not this number. The Windows outer Job
    /// rewrites `active_processes` through [`windows_outer_job_limits`].
    #[allow(dead_code)] // read on Windows (`SessionJob`); Linux writes limits at create.
    pub session_limits: bookclerk_sandbox::ResourceLimits,
    /// Isolation mode from config. Required refuses a missing outer Windows Job.
    #[allow(dead_code)] // read on Windows when the outer Job cannot be created
    pub isolation: Isolation,
    /// Linux session cgroup leaf both siblings join.
    ///
    /// Drop removes the leaf (killing leftover members) unless the host has
    /// taken ownership for the life of the vat.
    #[cfg(target_os = "linux")]
    pub session_cgroup: Option<SessionCgroup>,
    /// Private pathname-socket directory for the native guest.
    ///
    /// Drop removes it unless the host has taken ownership for the vat.
    #[cfg(unix)]
    pub guest_ipc: Option<GuestIpcDir>,
    /// AppContainer Package SID of the native guest (callback proxy DACL).
    #[cfg(windows)]
    pub package_sid: Option<String>,
    /// Host-owned AppContainer profile for the Cap'n Proto child.
    #[cfg(windows)]
    pub appcontainer: Option<bookclerk_sandbox::spawn::AppContainerSession>,
    /// Host-owned AppContainer profile for the native sibling.
    #[cfg(windows)]
    pub guest_appcontainer: Option<bookclerk_sandbox::spawn::AppContainerSession>,
}

/// Host-owned Linux cgroup leaf. Drop kills members and removes the directory.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct SessionCgroup {
    /// Leaf path. `None` after Drop has taken it.
    path: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
impl SessionCgroup {
    /// Own `path` until this value is dropped.
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Leaf directory both sibling jails join.
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[cfg(target_os = "linux")]
impl Drop for SessionCgroup {
    fn drop(&mut self) {
        if let Some(dir) = self.path.take() {
            if let Err(err) = bookclerk_sandbox::destroy_session_cgroup(&dir) {
                tracing::warn!(
                    error = %err,
                    path = %dir.display(),
                    "could not remove the session cgroup"
                );
            }
        }
    }
}

/// Mode `0700` directory the guest may use for pathname sockets.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct GuestIpcDir {
    /// Canonical directory. `None` after Drop has taken it.
    path: Option<PathBuf>,
}

#[cfg(unix)]
impl GuestIpcDir {
    /// Own `path` until this value is dropped.
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Directory granted on the guest write list and `unix_socket_dirs`.
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[cfg(unix)]
impl Drop for GuestIpcDir {
    fn drop(&mut self) {
        if let Some(dir) = self.path.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Per-session leaf name: plugin id, host pid, and a nonce.
///
/// Pid alone collides when one host starts two sessions of the same plugin.
///
/// # Arguments
///
/// * `plugin` - The plugin whose filesystem id is part of the leaf name.
#[cfg(target_os = "linux")]
fn session_cgroup_suffix(plugin: &DiscoveredPlugin) -> String {
    static NONCE: AtomicU64 = AtomicU64::new(1);
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{tick}-{n}",
        plugin.plugin_key().fs_id(),
        std::process::id()
    )
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
    ///
    /// `spawn` decides which executables the jail must let the launcher tree
    /// read and exec, the loopback bridge exception, and the process budget.
    pub(crate) fn plan(
        config: &Config,
        plugin: &DiscoveredPlugin,
        spawn: &SpawnPlan,
    ) -> Result<Self> {
        let data = plugin_data_dir(config, plugin)?;
        let scratch = plugin_scratch_dir(config, plugin)?;
        let id = plugin.alias();

        for dir in [&data, &scratch] {
            std::fs::create_dir_all(dir).map_err(|err| {
                PluginError::message(format!("could not create {}: {err}", dir.display()))
            })?;
        }
        // Availability: refuse spawn/reload when state already exceeds the host
        // budget (runaway tmp from a previous session).
        let grant = crate::consent::spawn_grant(&config.paths().files_dir, plugin).ok();
        let disk_budget = crate::consent::effective_disk_budget_bytes(grant.as_ref());
        ensure_plugin_state_within_budget_limit(id, &data, &scratch, disk_budget)?;
        // Fail closed while planning: a missing/unwritable local output root
        // must not become a late, opaque guest IO failure after jail start.
        if is_platform_local_storage(plugin) && config.output.local.enabled {
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
        let siblings = spawn.runtime == crate::GuestRuntimeKind::NativeBehindWorkerd;
        let session_dir = if siblings {
            Some(create_session_dir(&plugin_state_root(config, plugin)?)?)
        } else {
            None
        };
        let mut session_limits = session_resource_limits(plugin, spawn.runtime, grant.as_ref());
        apply_global_jail_resource_overrides(
            &mut session_limits,
            &config.plugins.jail,
            spawn.runtime,
        );
        #[cfg(target_os = "linux")]
        let session_cgroup = if siblings {
            // `pids.max` is the infrastructure thread budget plus the guest's
            // extra allowance. `session_limits` stays the process count the
            // Windows outer Job rewrites; it is not copied into the cgroup.
            let thread_limits = session_cgroup_thread_limits(session_limits, spawn.runtime);
            match bookclerk_sandbox::create_session_cgroup(
                &thread_limits,
                &session_cgroup_suffix(plugin),
            ) {
                Ok(path) => Some(SessionCgroup::new(path)),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "session cgroup unavailable; process-group kill is the fallback and does not cover a descendant that calls setsid"
                    );
                    None
                }
            }
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let cgroup_dir = session_cgroup
            .as_ref()
            .and_then(|cgroup| cgroup.path().map(Path::to_path_buf));
        #[cfg(not(target_os = "linux"))]
        let cgroup_dir = None;
        #[cfg(unix)]
        let guest_ipc = if siblings {
            match bookclerk_sandbox::create_guest_ipc_dir() {
                Ok(path) => Some(GuestIpcDir::new(path)),
                Err(err) => {
                    return Err(PluginError::message(format!(
                        "could not create the guest IPC directory: {err}"
                    )));
                }
            }
        } else {
            None
        };
        #[cfg(unix)]
        let ipc_dir = guest_ipc.as_ref().and_then(GuestIpcDir::path);
        #[cfg(not(unix))]
        let ipc_dir: Option<&Path> = None;
        #[cfg(windows)]
        let mut package_sid = None;
        #[cfg(windows)]
        let mut appcontainer = None;
        #[cfg(windows)]
        let mut guest_appcontainer = None;

        let (start, guest_start) = match isolation {
            Isolation::Off => plan_isolation_off(
                config,
                plugin,
                spawn,
                &data,
                &scratch,
                session_dir.as_deref(),
                grant.as_ref(),
                cgroup_dir.clone(),
                siblings,
                ipc_dir,
            )?,
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
                            match create_windows_profiles(plugin, siblings, isolation) {
                                Ok(profiles) => {
                                    package_sid = profiles
                                        .guest
                                        .as_ref()
                                        .or(profiles.gateway.as_ref())
                                        .map(|s| s.package_sid().to_string());
                                    appcontainer = profiles.gateway;
                                    guest_appcontainer = profiles.guest;
                                }
                                Err(err) if isolation == Isolation::BestEffort => {
                                    return Ok(Self {
                                        data,
                                        scratch,
                                        session_dir,
                                        start: Start::Unconfined {
                                            reason: format!(
                                                "AppContainer profile unavailable: {err}"
                                            ),
                                        },
                                        guest_start: siblings.then(|| Start::Unconfined {
                                            reason: format!(
                                                "AppContainer profile unavailable: {err}"
                                            ),
                                        }),
                                        session_limits,
                                        isolation,
                                        #[cfg(target_os = "linux")]
                                        session_cgroup,
                                        package_sid: None,
                                        appcontainer: None,
                                        guest_appcontainer: None,
                                    });
                                }
                                Err(err) => {
                                    return Err(PluginError::message(format!(
                                        "could not create AppContainer session for `{id}`: {err}"
                                    )));
                                }
                            }
                        }
                        #[cfg(windows)]
                        let gateway_profile =
                            appcontainer.as_ref().map(|s| s.profile_name().to_string());
                        #[cfg(windows)]
                        let guest_profile = guest_appcontainer
                            .as_ref()
                            .map(|s| s.profile_name().to_string());
                        #[cfg(not(windows))]
                        let gateway_profile = None;
                        #[cfg(not(windows))]
                        let guest_profile = None;

                        confined_starts(
                            launcher,
                            plugin,
                            spawn,
                            config,
                            &data,
                            &scratch,
                            session_dir.as_deref(),
                            grant.as_ref(),
                            enforcement,
                            gateway_profile,
                            guest_profile,
                            cgroup_dir,
                            siblings,
                            ipc_dir,
                        )
                    }
                    Err(reason)
                        if isolation == Isolation::BestEffort && !windows_needs_jail(siblings) =>
                    {
                        (
                            Start::Unconfined {
                                reason: reason.clone(),
                            },
                            siblings.then_some(Start::Unconfined { reason }),
                        )
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
            session_dir,
            start,
            guest_start,
            session_limits,
            isolation,
            #[cfg(target_os = "linux")]
            session_cgroup,
            #[cfg(unix)]
            guest_ipc,
            #[cfg(windows)]
            package_sid,
            #[cfg(windows)]
            appcontainer,
            #[cfg(windows)]
            guest_appcontainer,
        })
    }
}

/// Windows native-behind always goes through `bookclerk-jail` (handoff).
fn windows_needs_jail(siblings: bool) -> bool {
    cfg!(windows) && siblings
}

/// Isolation::Off: Unix siblings stay unconfined; Windows siblings still use
/// `bookclerk-jail` with [`Enforcement::Disabled`] so handle handoff works.
#[allow(clippy::too_many_arguments)]
fn plan_isolation_off(
    config: &Config,
    plugin: &DiscoveredPlugin,
    spawn: &SpawnPlan,
    data: &Path,
    scratch: &Path,
    session_dir: Option<&Path>,
    grant: Option<&PluginGrant>,
    cgroup_dir: Option<PathBuf>,
    siblings: bool,
    ipc_dir: Option<&Path>,
) -> Result<(Start, Option<Start>)> {
    let _ = ipc_dir;
    if windows_needs_jail(siblings) {
        let launcher = resolve_launcher(config, Isolation::Off).map_err(|reason| {
            PluginError::message(format!(
                "Windows native-behind-workerd requires `{JAIL_BIN_NAME}` beside the host \
                 even when [plugins].isolation = off ({reason})"
            ))
        })?;
        return Ok(confined_starts(
            launcher,
            plugin,
            spawn,
            config,
            data,
            scratch,
            session_dir,
            grant,
            Enforcement::Disabled,
            None,
            None,
            cgroup_dir,
            siblings,
            ipc_dir,
        ));
    }
    Ok((
        Start::Unconfined {
            reason: "[plugins].isolation = off".to_string(),
        },
        siblings.then(|| Start::Unconfined {
            reason: "[plugins].isolation = off".to_string(),
        }),
    ))
}

/// Grant `dir` as the guest's only pathname-socket directory.
///
/// The directory is also a write so the guest can create the socket node.
/// `/tmp` and the gateway session directory stay off both lists.
fn apply_guest_ipc(spec: &mut Spec, dir: &Path) {
    if !spec.writes.iter().any(|path| path == dir) {
        spec.writes.push(dir.to_path_buf());
    }
    spec.unix_socket_dirs = Some(vec![dir.to_path_buf()]);
}

/// Confined gateway (and optional guest) starts sharing one session cgroup.
#[allow(clippy::too_many_arguments)]
fn confined_starts(
    launcher: PathBuf,
    plugin: &DiscoveredPlugin,
    spawn: &SpawnPlan,
    config: &Config,
    data: &Path,
    scratch: &Path,
    session_dir: Option<&Path>,
    grant: Option<&PluginGrant>,
    enforcement: Enforcement,
    gateway_profile: Option<String>,
    guest_profile: Option<String>,
    cgroup_dir: Option<PathBuf>,
    siblings: bool,
    ipc_dir: Option<&Path>,
) -> (Start, Option<Start>) {
    let gateway = Start::Confined {
        launcher: launcher.clone(),
        spec: Box::new(build_spec_with_grant(
            plugin,
            spawn,
            config,
            data,
            scratch,
            session_dir,
            if siblings {
                JailRole::Gateway
            } else {
                JailRole::Combined
            },
            enforcement,
            gateway_profile,
            grant,
            cgroup_dir.clone(),
        )),
    };
    let guest = siblings.then(|| {
        let mut spec = build_spec_with_grant(
            plugin,
            spawn,
            config,
            data,
            scratch,
            session_dir,
            JailRole::Guest,
            enforcement,
            guest_profile,
            grant,
            cgroup_dir,
        );
        if let Some(dir) = ipc_dir {
            apply_guest_ipc(&mut spec, dir);
        }
        Start::Confined {
            launcher,
            spec: Box::new(spec),
        }
    });
    (gateway, guest)
}

/// Create the gateway profile and, for siblings, a distinct guest profile.
#[cfg(windows)]
struct WindowsProfiles {
    /// AppContainer for the Cap'n Proto child (`bookclerk-workerd`).
    gateway: Option<bookclerk_sandbox::spawn::AppContainerSession>,
    /// AppContainer for the native sibling. `None` when this session is a single jail.
    guest: Option<bookclerk_sandbox::spawn::AppContainerSession>,
}

/// Create distinct AppContainer profiles for the gateway and, when `siblings`
/// is set, the native guest.
///
/// Profiles stay host-owned. The jail launcher receives only the profile name.
///
/// # Errors
///
/// Returns an error when Windows refuses to create a profile.
#[cfg(windows)]
fn create_windows_profiles(
    plugin: &DiscoveredPlugin,
    siblings: bool,
    isolation: Isolation,
) -> std::result::Result<WindowsProfiles, bookclerk_sandbox::SandboxError> {
    let _ = isolation;
    let gateway_label = format!("plugin:{}", plugin.plugin_key().fs_id());
    let gateway = bookclerk_sandbox::spawn::AppContainerSession::create(&gateway_label)?;
    let guest = if siblings {
        let guest_label = format!("plugin:{}:guest", plugin.plugin_key().fs_id());
        Some(bookclerk_sandbox::spawn::AppContainerSession::create(
            &guest_label,
        )?)
    } else {
        None
    };
    Ok(WindowsProfiles {
        gateway: Some(gateway),
        guest,
    })
}

/// Host-owned `plugin-state/<fs_id>/session-<nonce>` (0700).
fn create_session_dir(state_root: &Path) -> Result<PathBuf> {
    let nonce = format!(
        "{:x}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let dir = state_root.join(format!("session-{nonce}"));
    std::fs::create_dir_all(&dir).map_err(|err| {
        PluginError::message(format!("could not create {}: {err}", dir.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|err| {
            PluginError::message(format!("could not chmod {}: {err}", dir.display()))
        })?;
    }
    Ok(dir)
}

/// Which filesystem / net / fd contract a jail spec uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JailRole {
    /// Isolate or direct-native: today's single-jail grants.
    Combined,
    /// Native-behind gateway: OutboundListen, session dir, fds 3+4.
    Gateway,
    /// Native-behind guest: Deny, data/tmp, fd 3.
    Guest,
}

/// Build the allowlist for one guest.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_spec(
    plugin: &DiscoveredPlugin,
    spawn: &SpawnPlan,
    config: &Config,
    data: &Path,
    scratch: &Path,
    preserve_fds: Vec<i32>,
    enforcement: Enforcement,
    windows_profile_name: Option<String>,
) -> Spec {
    let _ = preserve_fds;
    build_spec_with_grant(
        plugin,
        spawn,
        config,
        data,
        scratch,
        None,
        JailRole::Combined,
        enforcement,
        windows_profile_name,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
/// Builds a jail `Spec` for [`JailRole`]: install reads, granted writes, net, resources.
fn build_spec_with_grant(
    plugin: &DiscoveredPlugin,
    spawn: &SpawnPlan,
    config: &Config,
    data: &Path,
    scratch: &Path,
    session_dir: Option<&Path>,
    role: JailRole,
    enforcement: Enforcement,
    windows_profile_name: Option<String>,
    grant: Option<&PluginGrant>,
    cgroup_dir: Option<PathBuf>,
) -> Spec {
    let (writes, reads, net, preserve_fds, unix_socket_dirs, label_suffix) = match role {
        JailRole::Combined => {
            let mut writes = vec![data.to_path_buf(), scratch.to_path_buf()];
            if is_platform_local_storage(plugin) && config.output.local.enabled {
                writes.push(resolved_local_output_root(config));
            }
            if is_sqlite_database_plugin(plugin) {
                writes.extend(sqlite_library_paths(config));
                writes.push(plugin_databases_dir(config));
            }
            let mut reads = vec![plugin.root.clone()];
            reads.extend(spawn.executable_reads());
            (
                writes,
                reads,
                jail_net_policy(plugin, spawn, grant),
                Vec::new(),
                None,
                String::new(),
            )
        }
        JailRole::Gateway => {
            let session = session_dir
                .expect("native-behind gateway requires a host session directory")
                .to_path_buf();
            let mut reads = vec![plugin.root.clone()];
            reads.extend(spawn.gateway_executable_reads());
            (
                vec![session.clone()],
                reads,
                NetPolicy::OutboundListen,
                // The CONNECT mux stays in the unsandboxed host. Preserving
                // fd 4 would keep an unrelated descriptor open in the gateway.
                vec![bookclerk_sandbox::GATEWAY_RPC_FD],
                Some(vec![session]),
                ":gateway".to_string(),
            )
        }
        JailRole::Guest => {
            let mut writes = vec![data.to_path_buf(), scratch.to_path_buf()];
            if is_platform_local_storage(plugin) && config.output.local.enabled {
                writes.push(resolved_local_output_root(config));
            }
            if is_sqlite_database_plugin(plugin) {
                writes.extend(sqlite_library_paths(config));
                writes.push(plugin_databases_dir(config));
            }
            let mut reads = vec![plugin.root.clone()];
            reads.extend(spawn.guest_executable_reads());
            (
                writes,
                reads,
                NetPolicy::Deny,
                vec![bookclerk_sandbox::GUEST_PROXY_FD],
                Some(Vec::new()),
                ":guest".to_string(),
            )
        }
    };
    let mut resources = match role {
        JailRole::Gateway => gateway_spec_resource_limits(plugin, grant),
        JailRole::Guest => sibling_guest_spec_resource_limits(plugin, grant),
        JailRole::Combined => guest_spec_resource_limits(plugin, spawn.runtime, grant),
    };
    let override_runtime = match role {
        JailRole::Gateway => crate::GuestRuntimeKind::Workerd,
        JailRole::Guest => crate::GuestRuntimeKind::NativeDirect,
        JailRole::Combined => spawn.runtime,
    };
    apply_global_jail_resource_overrides(&mut resources, &config.plugins.jail, override_runtime);
    if role == JailRole::Gateway {
        // Extra processes belong on the guest / outer session cap, not the gateway.
        resources.active_processes = Some(2);
    }
    // A nested Job CPU rate is a fraction of its parent. The outer session
    // Job keeps the one-core hard cap. Sibling inner specs say `"off"` so the
    // jail does not fill the plugin label default. Standalone Combined and
    // media specs keep their percent. Memory and process limits stay explicit.
    let cpu_rate_percent = if matches!(role, JailRole::Gateway | JailRole::Guest) {
        bookclerk_sandbox::CpuRate::Disabled
    } else {
        match resources.cpu_rate_percent {
            Some(percent) => bookclerk_sandbox::CpuRate::Percent(percent),
            None => bookclerk_sandbox::CpuRate::Unspecified,
        }
    };
    Spec {
        label: format!("plugin:{}{label_suffix}", plugin.plugin_key().fs_id()),
        reads,
        writes,
        net,
        allow_exec: true,
        system_paths: true,
        enforcement,
        preserve_fds,
        windows_profile_name,
        memory_bytes: resources.memory_bytes,
        active_processes: resources.active_processes,
        cpu_rate_percent,
        inherit_handles: Vec::new(),
        cgroup_dir,
        unix_socket_dirs,
    }
}

/// Tightens guest memory/CPU/process ceilings using `[plugins.jail]` without widening filesystem grants.
fn apply_global_jail_resource_overrides(
    resources: &mut bookclerk_sandbox::ResourceLimits,
    jail: &bookclerk_config::PluginsJailConfig,
    runtime: GuestRuntimeKind,
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

/// Maps manifest network need and a deny grant onto Landlock/AppContainer `NetPolicy` (workerd-fronted guests stay `OutboundListen`).
fn jail_net_policy(
    plugin: &DiscoveredPlugin,
    spawn: &SpawnPlan,
    grant: Option<&PluginGrant>,
) -> NetPolicy {
    if spawn.fronted_by_workerd() {
        // Intentional OS-jail exception (see docs/adr/plugin-workers-rpc-workerd.md):
        // `bookclerk-workerd` must `bind(127.0.0.1:0)` for the host↔isolate RPC
        // bridge. Linux Landlock has no loopback-only policy, so
        // `OutboundListen` also permits `connect` for the launcher. Isolate
        // egress (`WORKERD_GRANT_*` → `globalOutbound = blocked` under deny)
        // remains the grant enforcement layer for isolates. Native-behind
        // *guests* use [`JailRole::Guest`] (`NetPolicy::Deny`) and the SDK
        // socket proxy rather than ambient `AF_INET`.
        return NetPolicy::OutboundListen;
    }
    let denied = grant.is_some_and(|g| g.network_mode.eq_ignore_ascii_case("deny"));
    match plugin.manifest.jail_network_need() {
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
/// - `active_processes` = payload overhead + extra budget (default extra 2;
///   native grant `extraProcesses`; workerd isolates use the default extra
///   only). Payload overhead for native-behind-workerd is 3 (gateway pair +
///   guest). The Windows outer Job uses [`windows_outer_job_limits`] (baseline
///   5 plus that extra) and does not feed this payload count to Linux.
/// - `cpu_rate_percent`: **native** from grant `cpuRatePercent` (default 80);
///   **workerd** always uses the host default (80) so isolate budgets stay on
///   `cpu_ms`. `[plugins.jail]` then applies as a per-jail ceiling.
fn guest_spec_resource_limits(
    plugin: &DiscoveredPlugin,
    runtime: GuestRuntimeKind,
    grant: Option<&PluginGrant>,
) -> bookclerk_sandbox::ResourceLimits {
    use crate::consent::{
        active_processes_for, effective_cpu_rate_percent, effective_extra_processes,
        effective_memory_mib,
    };

    let memory_mib = effective_memory_mib(grant.and_then(|g| g.memory_mib));
    let isolate = plugin.manifest.runtime == PluginRuntimeKind::Workerd;
    let extra = if isolate {
        // Workerd process headroom is host-managed (not a per-plugin consent knob).
        effective_extra_processes(None)
    } else {
        effective_extra_processes(grant.and_then(|g| g.extra_processes))
    };

    let cpu_rate = if isolate {
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

/// Gateway jail occupancy: `bookclerk-workerd` + pinned `workerd` (2). Extra
/// processes belong on the guest / session Job.
fn gateway_spec_resource_limits(
    plugin: &DiscoveredPlugin,
    grant: Option<&PluginGrant>,
) -> bookclerk_sandbox::ResourceLimits {
    let mut limits = guest_spec_resource_limits(plugin, crate::GuestRuntimeKind::Workerd, grant);
    limits.active_processes = Some(2);
    limits
}

/// Native sibling occupancy: guest (1) + grant extra.
fn sibling_guest_spec_resource_limits(
    plugin: &DiscoveredPlugin,
    grant: Option<&PluginGrant>,
) -> bookclerk_sandbox::ResourceLimits {
    guest_spec_resource_limits(plugin, crate::GuestRuntimeKind::NativeDirect, grant)
}

/// Linux `pids.max` for one session.
///
/// `payload.active_processes` is a process count. Subtracting
/// [`GuestRuntimeKind::process_overhead`] recovers `extraProcesses`. The
/// result is the infrastructure thread budget plus that extra. It is not
/// the Windows outer cap and not the payload process count.
///
/// Production callers are Linux-only. Other platforms still call this from
/// the sibling-grant unit test, so `--lib` builds there allow the unused
/// warning.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn session_cgroup_thread_limits(
    payload: bookclerk_sandbox::ResourceLimits,
    runtime: crate::GuestRuntimeKind,
) -> bookclerk_sandbox::ResourceLimits {
    use crate::consent::PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT;

    let extra = payload
        .active_processes
        .map(|absolute| absolute.saturating_sub(runtime.process_overhead()))
        .unwrap_or(PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT);
    let mut limits = payload;
    limits.active_processes =
        Some(bookclerk_sandbox::INFRASTRUCTURE_THREAD_BUDGET.saturating_add(extra));
    limits
}

/// Process-count ceilings (overhead + extra). Linux `pids.max` is
/// [`session_cgroup_thread_limits`].
fn session_resource_limits(
    plugin: &DiscoveredPlugin,
    runtime: crate::GuestRuntimeKind,
    grant: Option<&PluginGrant>,
) -> bookclerk_sandbox::ResourceLimits {
    guest_spec_resource_limits(plugin, runtime, grant)
}

/// Windows outer Job limits: same memory and CPU as `payload`, process cap
/// baseline 5 plus the extra budget already applied to that payload cap.
///
/// Sibling inner Jobs omit CPU; this outer Job keeps it. Linux `pids.max`
/// is [`session_cgroup_thread_limits`], not this Windows process count.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_outer_job_limits(
    payload: bookclerk_sandbox::ResourceLimits,
    runtime: crate::GuestRuntimeKind,
) -> bookclerk_sandbox::ResourceLimits {
    use crate::consent::{windows_session_active_processes, PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT};

    let extra = payload
        .active_processes
        .map(|abs| abs.saturating_sub(runtime.process_overhead()))
        .unwrap_or(PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT);
    let mut limits = payload;
    if runtime == crate::GuestRuntimeKind::NativeBehindWorkerd {
        limits.active_processes = Some(windows_session_active_processes(extra));
    }
    limits
}

/// True when this guest is the verified Bookclerk platform SQLite adapter.
pub(crate) fn is_sqlite_database_plugin(plugin: &DiscoveredPlugin) -> bool {
    plugin
        .manifest
        .has_entrypoint(crate::Entrypoint::DatabaseAdapter)
        && crate::first_party_database_kind(plugin)
            == Some(bookclerk_config::DatabasePluginKind::Sqlite)
}

/// True when this guest is the verified Bookclerk platform local storage plugin.
fn is_platform_local_storage(plugin: &DiscoveredPlugin) -> bool {
    plugin.manifest.has_entrypoint(crate::Entrypoint::Storage)
        && crate::is_first_party_local_output(plugin)
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

/// Root of the isolated per-binding database files granted to the sqlite adapter.
fn plugin_databases_dir(config: &Config) -> PathBuf {
    config.paths().files_dir.join("plugin-databases")
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
    // Directory-level grant for named plugin database bindings (a directory
    // rule covers files created later inside it).
    std::fs::create_dir_all(plugin_databases_dir(config))?;
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
        plugin_with_entrypoint(root, id, network, "storefront")
    }

    fn plugin_with_entrypoint(
        root: &Path,
        id: &str,
        network: JailNetworkNeed,
        entrypoint: &str,
    ) -> DiscoveredPlugin {
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").expect("write guest");
        let (network_toml, oauth_toml) = match network {
            JailNetworkNeed::None => ("mode = \"deny\"", ""),
            JailNetworkNeed::Outbound => ("mode = \"outbound\"", ""),
            JailNetworkNeed::Listen => ("mode = \"outbound\"", "[oauth]\n"),
        };
        let toml = format!(
            r#"
api_version = 3
id = "{id}"
version = "0.0.0"
runtime = "native"
command = "./guest"
entrypoints = ["{entrypoint}"]

[capabilities.network]
{network_toml}

{oauth_toml}
"#
        );
        std::fs::write(root.join("plugin.toml"), &toml).expect("write plugin.toml");
        let manifest = crate::PluginManifest::parse(&toml).expect("test manifest");
        DiscoveredPlugin::try_new(manifest, root.to_path_buf(), command, None)
            .expect("test plugin tree must evaluate")
    }

    fn sqlite_plugin_at(files: &Path) -> DiscoveredPlugin {
        let key = bookclerk_plugin_catalog::PluginKey::platform(
            "bookclerk-plugin-database-sqlite",
            "sqlite",
        )
        .unwrap();
        let root = files.join("plugins").join(key.fs_id());
        std::fs::create_dir_all(&root).expect("platform plugin dir");
        let plugin =
            plugin_with_entrypoint(&root, "sqlite", JailNetworkNeed::None, "databaseAdapter");
        bookclerk_plugin_catalog::stamp_platform_receipt(
            &plugin.root,
            files,
            "bookclerk-plugin-database-sqlite",
            &plugin.manifest,
            "0.0.0",
        )
        .expect("stamp platform sqlite");
        DiscoveredPlugin::try_new(plugin.manifest, plugin.root, plugin.command, Some(files))
            .expect("stamped platform sqlite")
    }

    /// Diagnostic-transport plan: the jail execs the native command itself.
    fn direct(plugin: &DiscoveredPlugin) -> SpawnPlan {
        SpawnPlan::resolve_with(
            plugin,
            crate::SpawnTransport::DirectNativeDiagnostic,
            || panic!("direct native never needs the front door"),
        )
        .expect("direct plan")
    }

    /// Front-door plan against fake `bookclerk-workerd` + `workerd` files in `helpers`.
    fn fronted(plugin: &DiscoveredPlugin, helpers: &Path) -> SpawnPlan {
        for name in ["bookclerk-workerd", "workerd", "bookclerk-jail"] {
            let path = helpers.join(name);
            if !path.exists() {
                std::fs::write(&path, b"").expect("fake helper");
            }
        }
        SpawnPlan::resolve_with(plugin, crate::SpawnTransport::WorkerdFrontDoor, || {
            crate::WorkerdFrontDoor::locate_in(helpers)
        })
        .expect("front-door plan")
    }

    /// Manifest flipped to a workerd isolate (no native command).
    fn as_workerd_isolate(plugin: &mut DiscoveredPlugin, limits: crate::manifest::WorkerdLimits) {
        use crate::manifest::WorkerdRuntimeManifest;

        plugin.manifest.runtime = PluginRuntimeKind::Workerd;
        plugin.manifest.command = None;
        plugin.manifest.workerd = Some(WorkerdRuntimeManifest {
            compatibility_date: "2026-08-01".into(),
            compatibility_flags: vec![],
            main_module: "index.js".into(),
            modules_dir: "modules".into(),
            entrypoint: "default".into(),
            limits,
        });
    }

    #[test]
    fn the_allowlist_covers_the_guest_dirs_and_nothing_else() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = plugin_at(install.path(), "libro", JailNetworkNeed::Outbound);

        let spec = build_spec(
            &plugin,
            &direct(&plugin),
            &config,
            &plugin_data_dir(&config, &plugin).unwrap(),
            &plugin_scratch_dir(&config, &plugin).unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
        );

        assert_eq!(
            spec.label,
            format!("plugin:{}", plugin.plugin_key().fs_id())
        );
        assert_eq!(
            spec.net,
            NetPolicy::Outbound,
            "direct native outbound gets coarse jail outbound"
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
        let config = config_at(files.path());
        let plugin = sqlite_plugin_at(files.path());
        ensure_sqlite_library_files(&config).expect("touch sqlite files");

        let spec = build_spec(
            &plugin,
            &direct(&plugin),
            &config,
            &plugin_data_dir(&config, &plugin).unwrap(),
            &plugin_scratch_dir(&config, &plugin).unwrap(),
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
    fn fake_first_party_aliases_do_not_get_host_private_grants() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        config.output.local.enabled = true;
        config.output.local.root = files.path().join("books");
        std::fs::create_dir_all(&config.output.local.root).expect("output root");
        ensure_sqlite_library_files(&config).expect("touch sqlite files");
        let output_root = resolved_local_output_root(&config);
        for (id, entrypoint) in [
            ("sqlite", "databaseAdapter"),
            ("postgres", "databaseAdapter"),
            ("d1", "databaseAdapter"),
            ("local", "storage"),
        ] {
            let root = install.path().join(id);
            std::fs::create_dir_all(&root).expect("alias install dir");
            let plugin = plugin_with_entrypoint(&root, id, JailNetworkNeed::None, entrypoint);
            let spec = build_spec(
                &plugin,
                &direct(&plugin),
                &config,
                &plugin_data_dir(&config, &plugin).unwrap(),
                &plugin_scratch_dir(&config, &plugin).unwrap(),
                Vec::new(),
                Enforcement::Required,
                None,
            );
            for path in sqlite_library_paths(&config) {
                assert!(
                    !spec.writes.contains(&path),
                    "third-party `{id}` must not receive {}",
                    path.display()
                );
            }
            assert!(
                !spec.writes.contains(&plugin_databases_dir(&config)),
                "third-party `{id}` must not receive plugin-databases/"
            );
            assert!(
                !spec.writes.contains(&output_root),
                "third-party `{id}` must not receive local output root"
            );
            assert_eq!(
                plugin.identity.provenance,
                bookclerk_plugin_catalog::PluginProvenance::LocalDevelopment
            );
        }
    }

    #[test]
    fn planning_sqlite_precreates_library_sidecars() {
        let files = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        config.plugins.isolation = Isolation::Off;
        let plugin = sqlite_plugin_at(files.path());

        let _jail = GuestJail::plan(&config, &plugin, &direct(&plugin)).expect("plan");
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
            &direct(&plugin),
            &config,
            &plugin_data_dir(&config, &plugin).unwrap(),
            &plugin_scratch_dir(&config, &plugin).unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
        );
        assert!(!spec.writes.contains(&config.paths().files_dir));
        assert!(!spec.reads.contains(&config.paths().files_dir));
    }

    /// Sibling specs: gateway reads workerd helpers + session dir; guest reads
    /// the backend and writes data/tmp. Neither sees the other's private paths.
    #[test]
    fn a_native_guest_behind_workerd_gets_split_sibling_grants() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let helpers = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = plugin_at(install.path(), "sqlite", JailNetworkNeed::None);
        let plan = fronted(&plugin, helpers.path());
        assert_eq!(plan.runtime, GuestRuntimeKind::NativeBehindWorkerd);
        let data = plugin_data_dir(&config, &plugin).unwrap();
        let scratch = plugin_scratch_dir(&config, &plugin).unwrap();
        let session = files.path().join("session-test");
        std::fs::create_dir_all(&session).expect("session dir");

        let gateway = build_spec_with_grant(
            &plugin,
            &plan,
            &config,
            &data,
            &scratch,
            Some(session.as_path()),
            JailRole::Gateway,
            Enforcement::Required,
            None,
            None,
            None,
        );
        let guest = build_spec_with_grant(
            &plugin,
            &plan,
            &config,
            &data,
            &scratch,
            Some(session.as_path()),
            JailRole::Guest,
            Enforcement::Required,
            None,
            None,
            None,
        );
        for exe in [
            &plan.launcher,
            plan.workerd_bin.as_ref().expect("workerd bin"),
        ] {
            assert!(
                gateway.reads.iter().any(|r| exe.starts_with(r)),
                "{} must be readable in the gateway: {:?}",
                exe.display(),
                gateway.reads
            );
            assert!(
                !guest.reads.iter().any(|r| exe.starts_with(r)),
                "{} must not be readable in the guest: {:?}",
                exe.display(),
                guest.reads
            );
        }
        assert!(
            guest.reads.iter().any(|r| plugin.command.starts_with(r)),
            "backend must be readable in the guest"
        );
        // The install directory is granted to both (plugin.toml); the gateway
        // must not receive an extra explicit grant of the workerd helpers'
        // parent, and the guest must not receive the helper binaries.
        assert_eq!(gateway.net, NetPolicy::OutboundListen);
        assert_eq!(guest.net, NetPolicy::Deny);
        assert_eq!(gateway.active_processes, Some(2));
        // guest 1 + default extra 2
        assert_eq!(guest.active_processes, Some(3));
        // Nested CPU would compound against the outer session Job.
        assert_eq!(
            gateway.cpu_rate_percent,
            bookclerk_sandbox::CpuRate::Disabled
        );
        assert_eq!(guest.cpu_rate_percent, bookclerk_sandbox::CpuRate::Disabled);
        let payload = session_resource_limits(&plugin, plan.runtime, None);
        assert_eq!(payload.active_processes, Some(5), "linux payload stays 3+2");
        let outer = windows_outer_job_limits(payload, plan.runtime);
        assert_eq!(outer.active_processes, Some(7), "windows outer is 5+2");
        assert_eq!(outer.cpu_rate_percent, payload.cpu_rate_percent);
        let threads = session_cgroup_thread_limits(payload, plan.runtime);
        assert_eq!(
            threads.active_processes,
            Some(bookclerk_sandbox::INFRASTRUCTURE_THREAD_BUDGET + 2)
        );
        assert_ne!(threads.active_processes, outer.active_processes);
        assert_ne!(threads.active_processes, payload.active_processes);
        assert_eq!(gateway.writes, vec![session.clone()]);
        assert!(guest.writes.contains(&data));
        assert!(guest.writes.contains(&scratch));
        assert!(!guest.writes.contains(&session));
        assert_eq!(
            gateway.unix_socket_dirs.as_deref(),
            Some([session.clone()].as_slice())
        );
        assert_eq!(guest.unix_socket_dirs.as_deref(), Some([].as_slice()));
        assert!(!gateway.reads.contains(&helpers.path().to_path_buf()));
    }

    /// Production confined guests receive one short IPC directory, not `/tmp`
    /// and not the gateway session directory.
    #[cfg(unix)]
    #[test]
    fn planned_guest_pathname_sockets_are_only_the_private_ipc_dir() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let helpers = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        config.plugins.isolation = Isolation::Required;
        let plugin = plugin_at(install.path(), "sqlite", JailNetworkNeed::None);
        let plan = fronted(&plugin, helpers.path());
        config.plugins.jail_bin = Some(helpers.path().join("bookclerk-jail"));

        let jail = GuestJail::plan(&config, &plugin, &plan).expect("plan");
        let ipc = jail
            .guest_ipc
            .as_ref()
            .and_then(GuestIpcDir::path)
            .expect("guest ipc dir")
            .to_path_buf();
        bookclerk_sandbox::ensure_guest_ipc_fits(&ipc).expect("sockaddr_un");
        assert_ne!(ipc, std::path::Path::new("/tmp"));
        assert_ne!(ipc, std::path::Path::new("/private/tmp"));

        let Start::Confined { spec, .. } = jail.guest_start.as_ref().expect("guest start") else {
            panic!("guest must be confined under required isolation");
        };
        assert_eq!(spec.net, NetPolicy::Deny);
        assert_eq!(
            spec.unix_socket_dirs.as_deref(),
            Some(std::slice::from_ref(&ipc))
        );
        assert!(spec.writes.iter().any(|path| path == &ipc));
        assert!(
            !spec.writes.iter().any(|path| {
                path == std::path::Path::new("/tmp") || path == std::path::Path::new("/private/tmp")
            }),
            "guest writes must not include /tmp: {:?}",
            spec.writes
        );
        let session = jail.session_dir.clone().expect("session dir");
        assert!(!spec.writes.iter().any(|path| path == &session));
        assert!(spec
            .unix_socket_dirs
            .as_ref()
            .is_some_and(|dirs| !dirs.iter().any(|path| path == &session)));

        let Start::Confined { spec: gateway, .. } = &jail.start else {
            panic!("gateway must be confined under required isolation");
        };
        assert_eq!(
            gateway.unix_socket_dirs.as_deref(),
            Some(std::slice::from_ref(&session))
        );
        assert!(!gateway.writes.iter().any(|path| path == &ipc));

        drop(jail);
        assert!(
            !ipc.exists(),
            "dropping the jail removes the guest IPC directory"
        );
    }

    #[test]
    fn one_guest_cannot_reach_another_guests_data() {
        let files = tempfile::tempdir().expect("tempdir");
        let a_install = tempfile::tempdir().expect("tempdir");
        let b_install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let mine_plugin = plugin_at(a_install.path(), "libro", JailNetworkNeed::Outbound);
        let theirs_plugin = plugin_at(b_install.path(), "audible", JailNetworkNeed::Outbound);
        let mine = plugin_data_dir(&config, &mine_plugin).unwrap();
        let theirs = plugin_data_dir(&config, &theirs_plugin).unwrap();
        assert_ne!(mine, theirs);
        assert!(!theirs.starts_with(&mine));
        assert!(!mine.starts_with(&theirs));
        assert!(mine.starts_with(files.path().join("plugin-state")));
        assert!(theirs.starts_with(files.path().join("plugin-state")));
    }

    #[test]
    fn network_need_maps_to_the_matching_policy() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let helpers = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        // Direct native: deny / outbound / outbound+oauth → Deny / Outbound / OutboundListen.
        for (need, expected) in [
            (JailNetworkNeed::None, NetPolicy::Deny),
            (JailNetworkNeed::Outbound, NetPolicy::Outbound),
            (JailNetworkNeed::Listen, NetPolicy::OutboundListen),
        ] {
            let plugin = plugin_at(install.path(), "xx", need);
            let spec = build_spec(
                &plugin,
                &direct(&plugin),
                &config,
                &plugin_data_dir(&config, &plugin).unwrap(),
                &plugin_scratch_dir(&config, &plugin).unwrap(),
                Vec::new(),
                Enforcement::Required,
                None,
            );
            assert_eq!(spec.net, expected, "{need:?}");
        }

        // Workerd needs loopback listen/connect to its Cloudflare child.
        let mut workerd = plugin_at(install.path(), "echo", JailNetworkNeed::None);
        as_workerd_isolate(&mut workerd, Default::default());
        let workerd_plan = fronted(&workerd, helpers.path());
        assert_eq!(workerd_plan.runtime, GuestRuntimeKind::Workerd);
        assert_eq!(
            workerd.manifest.jail_network_need(),
            JailNetworkNeed::Listen
        );
        let spec = build_spec(
            &workerd,
            &workerd_plan,
            &config,
            &plugin_data_dir(&config, &workerd).unwrap(),
            &plugin_scratch_dir(&config, &workerd).unwrap(),
            Vec::new(),
            Enforcement::Required,
            None,
        );
        assert_eq!(spec.net, NetPolicy::OutboundListen);

        // Operator `deny` must not strip the loopback RPC bind; isolate egress
        // still honours the grant via WORKERD_GRANT_NETWORK_MODE.
        let deny = PluginGrant {
            schema_version: crate::GRANT_SCHEMA_VERSION,
            plugin_key: String::new(),
            plugin_id: "echo".into(),
            entrypoints: Default::default(),
            producers: Default::default(),
            consumers: Default::default(),
            jobs: Default::default(),
            network_mode: "deny".into(),
            domains: Default::default(),
            manifest_domains: Default::default(),
            operator_added_domains: Default::default(),
            operator_denied_domains: Default::default(),
            bindings: Default::default(),
            compatibility_flags: Default::default(),
            cpu_ms: None,
            subrequests: None,
            disk_mib: None,
            memory_mib: None,
            cpu_rate_percent: None,
            extra_processes: None,
            approved_at: "2026-01-01T00:00:00Z".into(),
            ..PluginGrant::empty()
        };
        let denied = build_spec_with_grant(
            &workerd,
            &workerd_plan,
            &config,
            &plugin_data_dir(&config, &workerd).unwrap(),
            &plugin_scratch_dir(&config, &workerd).unwrap(),
            None,
            JailRole::Combined,
            Enforcement::Required,
            None,
            Some(&deny),
            None,
        );
        assert_eq!(denied.net, NetPolicy::OutboundListen);

        // Direct native OAuth Listen + stored deny grant stays OS-Deny (no
        // workerd bridge to keep open).
        let native_listen = plugin_at(install.path(), "oauth", JailNetworkNeed::Listen);
        let native_denied = build_spec_with_grant(
            &native_listen,
            &direct(&native_listen),
            &config,
            &plugin_data_dir(&config, &native_listen).unwrap(),
            &plugin_scratch_dir(&config, &native_listen).unwrap(),
            None,
            JailRole::Combined,
            Enforcement::Required,
            None,
            Some(&PluginGrant {
                schema_version: crate::GRANT_SCHEMA_VERSION,
                plugin_key: String::new(),
                plugin_id: "oauth".into(),
                entrypoints: Default::default(),
                producers: Default::default(),
                consumers: Default::default(),
                jobs: Default::default(),
                network_mode: "deny".into(),
                domains: Default::default(),
                manifest_domains: Default::default(),
                operator_added_domains: Default::default(),
                operator_denied_domains: Default::default(),
                bindings: Default::default(),
                compatibility_flags: Default::default(),
                cpu_ms: None,
                subrequests: None,
                disk_mib: None,
                memory_mib: None,
                cpu_rate_percent: None,
                extra_processes: None,
                approved_at: "2026-01-01T00:00:00Z".into(),
                ..PluginGrant::empty()
            }),
            None,
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
            &direct(&native),
            &config,
            &plugin_data_dir(&config, &native).unwrap(),
            &plugin_scratch_dir(&config, &native).unwrap(),
            vec![],
            Enforcement::Required,
            None,
        );
        // Host knobs are ceilings: min(default 512, 256), min(80, host_max for 250),
        // extra min(2, 1) → active = 1 + 1 = 2. 250 clamps to host_max then min with 80.
        assert_eq!(spec.memory_bytes, Some(256 * 1024 * 1024));
        assert_eq!(
            spec.cpu_rate_percent,
            bookclerk_sandbox::CpuRate::Percent(80)
        );
        assert_eq!(spec.active_processes, Some(2));

        // Session aggregate behind the front door stays 3 + 1 = 4; the gateway
        // jail is fixed at 2 and the guest takes 1 + extra.
        let helpers = tempfile::tempdir().expect("tempdir");
        let plan = fronted(&native, helpers.path());
        let session = files.path().join("session-limits");
        std::fs::create_dir_all(&session).expect("session");
        let data = plugin_data_dir(&config, &native).unwrap();
        let scratch = plugin_scratch_dir(&config, &native).unwrap();
        let gateway = build_spec_with_grant(
            &native,
            &plan,
            &config,
            &data,
            &scratch,
            Some(session.as_path()),
            JailRole::Gateway,
            Enforcement::Required,
            None,
            None,
            None,
        );
        let guest = build_spec_with_grant(
            &native,
            &plan,
            &config,
            &data,
            &scratch,
            Some(session.as_path()),
            JailRole::Guest,
            Enforcement::Required,
            None,
            None,
            None,
        );
        assert_eq!(gateway.active_processes, Some(2));
        assert_eq!(guest.active_processes, Some(2));
        assert_eq!(
            gateway.cpu_rate_percent,
            bookclerk_sandbox::CpuRate::Disabled
        );
        assert_eq!(guest.cpu_rate_percent, bookclerk_sandbox::CpuRate::Disabled);
        let mut session = session_resource_limits(&native, plan.runtime, None);
        apply_global_jail_resource_overrides(&mut session, &config.plugins.jail, plan.runtime);
        // Linux payload: 3 + extra 1. Windows outer: 5 + extra 1. CPU stays outer-only.
        assert_eq!(session.active_processes, Some(4));
        assert!(session.cpu_rate_percent.is_some());
        let outer = windows_outer_job_limits(session, plan.runtime);
        assert_eq!(outer.active_processes, Some(6));
        assert_eq!(outer.cpu_rate_percent, session.cpu_rate_percent);
    }

    #[test]
    fn workerd_jail_cpu_uses_host_default_not_cpu_ms_heuristic() {
        use crate::manifest::WorkerdLimits;

        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let helpers = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());

        let native = plugin_at(install.path(), "native", JailNetworkNeed::None);
        let native_spec = build_spec(
            &native,
            &direct(&native),
            &config,
            &plugin_data_dir(&config, &native).unwrap(),
            &plugin_scratch_dir(&config, &native).unwrap(),
            vec![],
            Enforcement::Required,
            None,
        );
        // Direct native: overhead 1 + default extra 2 = 3. Workerd: overhead 2 + extra 2 = 4.
        assert_eq!(native_spec.memory_bytes, Some(512 * 1024 * 1024));
        assert_eq!(native_spec.active_processes, Some(3));
        assert_eq!(
            native_spec.cpu_rate_percent,
            bookclerk_sandbox::CpuRate::Percent(80)
        );

        let native_with_grant = build_spec_with_grant(
            &native,
            &direct(&native),
            &config,
            &plugin_data_dir(&config, &native).unwrap(),
            &plugin_scratch_dir(&config, &native).unwrap(),
            None,
            JailRole::Combined,
            Enforcement::Required,
            None,
            Some(&PluginGrant {
                schema_version: crate::GRANT_SCHEMA_VERSION,
                plugin_key: String::new(),
                plugin_id: "native".into(),
                entrypoints: Default::default(),
                producers: Default::default(),
                consumers: Default::default(),
                jobs: Default::default(),
                network_mode: "deny".into(),
                domains: Default::default(),
                manifest_domains: Default::default(),
                operator_added_domains: Default::default(),
                operator_denied_domains: Default::default(),
                bindings: Default::default(),
                compatibility_flags: Default::default(),
                cpu_ms: None,
                subrequests: None,
                disk_mib: Some(512),
                memory_mib: Some(256),
                cpu_rate_percent: Some(40),
                extra_processes: Some(4),
                approved_at: "2026-01-01T00:00:00Z".into(),
                ..PluginGrant::empty()
            }),
            None,
        );
        assert_eq!(native_with_grant.memory_bytes, Some(256 * 1024 * 1024));
        assert_eq!(
            native_with_grant.cpu_rate_percent,
            bookclerk_sandbox::CpuRate::Percent(40)
        );
        // Clamped by default global extra_processes = 2 → active = 1 + 2 = 3.
        assert_eq!(native_with_grant.active_processes, Some(3));

        let mut workerd = plugin_at(install.path(), "echo", JailNetworkNeed::None);
        as_workerd_isolate(
            &mut workerd,
            WorkerdLimits {
                cpu_ms: Some(15_000),
                subrequests: None,
            },
        );
        let workerd_plan = fronted(&workerd, helpers.path());
        let default_spec = build_spec(
            &workerd,
            &workerd_plan,
            &config,
            &plugin_data_dir(&config, &workerd).unwrap(),
            &plugin_scratch_dir(&config, &workerd).unwrap(),
            vec![],
            Enforcement::Required,
            None,
        );
        assert_eq!(default_spec.memory_bytes, Some(512 * 1024 * 1024));
        // Workerd: overhead 2 + default extra 2 = 4.
        assert_eq!(default_spec.active_processes, Some(4));
        // Workerd isolate budget is cpu_ms; jail CPU stays at host default (80),
        // not the old cpu_ms → rate heuristic (which would have been 40).
        assert_eq!(
            default_spec.cpu_rate_percent,
            bookclerk_sandbox::CpuRate::Percent(80)
        );

        let mut config_ceil = config_at(files.path());
        config_ceil.plugins.jail.cpu_rate_percent = Some(25);
        let capped = build_spec(
            &workerd,
            &workerd_plan,
            &config_ceil,
            &plugin_data_dir(&config_ceil, &workerd).unwrap(),
            &plugin_scratch_dir(&config_ceil, &workerd).unwrap(),
            vec![],
            Enforcement::Required,
            None,
        );
        assert_eq!(
            capped.cpu_rate_percent,
            bookclerk_sandbox::CpuRate::Percent(25)
        );
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
            &direct(&native),
            &config,
            &plugin_data_dir(&config, &native).unwrap(),
            &plugin_scratch_dir(&config, &native).unwrap(),
            None,
            JailRole::Combined,
            Enforcement::Required,
            None,
            Some(&PluginGrant {
                schema_version: crate::GRANT_SCHEMA_VERSION,
                plugin_key: String::new(),
                plugin_id: "native".into(),
                entrypoints: Default::default(),
                producers: Default::default(),
                consumers: Default::default(),
                jobs: Default::default(),
                network_mode: "deny".into(),
                domains: Default::default(),
                manifest_domains: Default::default(),
                operator_added_domains: Default::default(),
                operator_denied_domains: Default::default(),
                bindings: Default::default(),
                compatibility_flags: Default::default(),
                cpu_ms: None,
                subrequests: None,
                disk_mib: Some(512),
                memory_mib: Some(512),
                cpu_rate_percent: Some(want),
                extra_processes: Some(2),
                approved_at: "2026-01-01T00:00:00Z".into(),
                ..PluginGrant::empty()
            }),
            None,
        );
        assert_eq!(
            spec.cpu_rate_percent,
            bookclerk_sandbox::CpuRate::Percent(want)
        );
    }

    /// Hostile / non-grammar ids are rejected (no lossy rewrite). State for a
    /// valid plugin lives under `plugin-state/<fs_id>/`, not the display alias.
    #[test]
    fn invalid_plugin_ids_are_rejected_not_rewritten() {
        for hostile in ["../../etc", "..", ".", "a/b", "/absolute", "a-b", "a__b"] {
            let err = bookclerk_plugin_catalog::PluginKey::from_install_path(
                Path::new("/tmp/x"),
                hostile,
            )
            .expect_err("must reject");
            assert!(
                err.to_string().contains("plugin id") || err.to_string().contains("invalid"),
                "id {hostile:?} got: {err}"
            );
        }
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = plugin_at(install.path(), "echo", JailNetworkNeed::None);
        let data = plugin_data_dir(&config, &plugin).unwrap();
        assert!(data.starts_with(files.path().join("plugin-state")));
        assert!(data.ends_with("data"));
        assert!(!data.to_string_lossy().contains(".."));
        assert!(!data.to_string_lossy().contains("echo"));
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
            &direct(&plugin),
            &config,
            &plugin_data_dir(&config, &plugin).unwrap(),
            &plugin_scratch_dir(&config, &plugin).unwrap(),
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

        let jail = GuestJail::plan(&config, &plugin, &direct(&plugin)).expect("plan");
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

        let err = GuestJail::plan(&config, &plugin, &direct(&plugin)).expect_err("must refuse");
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

        let jail = GuestJail::plan(&config, &plugin, &direct(&plugin)).expect("plan");
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

        let data = plugin_data_dir(&config, &plugin).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        // Grow past the production 512 MiB ceiling with a sparse-ish write that
        // still counts via `metadata().len()` on a regular file.
        let fat = data.join("fat.bin");
        let file = std::fs::File::create(&fat).expect("create");
        file.set_len(PLUGIN_STATE_BUDGET_BYTES + 1)
            .expect("set_len");
        drop(file);

        let err = GuestJail::plan(&config, &plugin, &direct(&plugin)).expect_err("must refuse");
        assert!(err.to_string().contains("state directory"), "got: {err}");
    }
}
