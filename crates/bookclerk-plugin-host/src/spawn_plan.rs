//! How one discovered plugin is launched.
//!
//! Every product spawn goes through the **workerd front door**: the jail execs
//! `bookclerk-workerd`, which owns the control plane (`describe` / `open`
//! policy / `shutdown`) and either loads the author's isolate (`runtime =
//! "workerd"`) or spawns the native Cap'n Proto guest itself and forwards every
//! entrypoint family typed (`runtime = "native"`). The manifest `runtime` only
//! selects the backend behind the isolate; it never selects the transport.
//!
//! Speaking Cap'n Proto to a native guest directly — the host connecting to
//! the guest's stdio with no `bookclerk-workerd` in between — exists only as
//! [`SpawnTransport::DirectNativeDiagnostic`] for tests and diagnostics. No
//! `[plugins].isolation` mode selects it: isolation decides whether the OS jail
//! may be skipped, not whether the front door may be.

use std::path::{Path, PathBuf};

use crate::discover::DiscoveredPlugin;
use crate::manifest::PluginRuntimeKind;
use crate::{PluginError, Result};

/// Override for the `bookclerk-workerd` launcher path (tests / packaging).
pub const WORKERD_LAUNCHER_ENV: &str = "BOOKCLERK_PLUGIN_WORKERD";
/// Override for the pinned Cloudflare `workerd` binary, honoured by
/// `bookclerk-workerd` and forwarded to it by the host.
pub const WORKERD_BIN_ENV: &str = "BOOKCLERK_WORKERD_BIN";
/// Environment variable naming the native backend `bookclerk-workerd` fronts.
pub const NATIVE_BACKEND_ENV: &str = "BOOKCLERK_NATIVE_BACKEND";

/// Which transport a spawn uses to reach the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpawnTransport {
    /// Product path: `bookclerk-workerd` fronts every guest, native or isolate.
    #[default]
    WorkerdFrontDoor,
    /// Host ↔ native Cap'n Proto on the guest's own stdio, no isolate.
    ///
    /// Diagnostic only: shell-probe jail tests and transport benchmarks. A
    /// `runtime = "workerd"` manifest cannot use it (there is no native guest
    /// to talk to) and the product binaries never select it.
    DirectNativeDiagnostic,
}

/// Process tree the jail will contain, as reported in [`crate::ExecutorIdentity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuestRuntimeKind {
    /// `bookclerk-workerd` + the pinned `workerd` running author modules.
    Workerd,
    /// `bookclerk-workerd` + `workerd` (control plane) + the native guest.
    NativeBehindWorkerd,
    /// The native guest alone, spoken to directly (diagnostic transport).
    NativeDirect,
}

impl GuestRuntimeKind {
    /// Stable label used in executor identities and logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Workerd => "workerd",
            Self::NativeBehindWorkerd => "native-behind-workerd",
            Self::NativeDirect => "native-direct",
        }
    }

    /// Fixed jail occupancy of the launcher tree before any guest-owned
    /// children or threads.
    ///
    /// `NativeDirect`: `bookclerk-jail` execs the guest (1). `Workerd`:
    /// `bookclerk-workerd` plus the `workerd` child (2). `NativeBehindWorkerd`:
    /// launcher, `workerd`, and the native guest (3).
    #[must_use]
    pub fn process_overhead(self) -> u32 {
        match self {
            Self::NativeDirect => 1,
            Self::Workerd => 2,
            Self::NativeBehindWorkerd => 3,
        }
    }

    /// True when `bookclerk-workerd` is the program the jail execs.
    #[must_use]
    pub fn fronted_by_workerd(self) -> bool {
        !matches!(self, Self::NativeDirect)
    }

    /// Runtime kind a manifest resolves to under `transport`.
    #[must_use]
    pub fn for_manifest(runtime: PluginRuntimeKind, transport: SpawnTransport) -> Self {
        match (runtime, transport) {
            (PluginRuntimeKind::Workerd, _) => Self::Workerd,
            (PluginRuntimeKind::Native, SpawnTransport::WorkerdFrontDoor) => {
                Self::NativeBehindWorkerd
            }
            (PluginRuntimeKind::Native, SpawnTransport::DirectNativeDiagnostic) => {
                Self::NativeDirect
            }
        }
    }
}

/// Resolved `bookclerk-workerd` launcher and the pinned `workerd` beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerdFrontDoor {
    /// `bookclerk-workerd` executable the jail execs.
    pub launcher: PathBuf,
    /// Pinned Cloudflare `workerd` the launcher spawns.
    pub workerd_bin: PathBuf,
}

impl WorkerdFrontDoor {
    /// Locates the front door for this host process.
    ///
    /// Launcher: [`WORKERD_LAUNCHER_ENV`] when set (must be a file), else
    /// beside the current executable (or its `deps/` parent for cargo test
    /// binaries), else on `PATH`. `workerd`: [`WORKERD_BIN_ENV`] when set
    /// (must be a file), else beside the launcher.
    ///
    /// # Errors
    ///
    /// Returns an error naming what was looked for and where when either
    /// binary is missing.
    pub fn locate() -> Result<Self> {
        let launcher = locate_launcher()?;
        let workerd_bin = locate_workerd_bin(&launcher)?;
        Ok(Self {
            launcher,
            workerd_bin,
        })
    }

    /// Locates the front door looking **only** in `dir` (no environment
    /// overrides, no `PATH`). A test seam for missing-launcher planning.
    ///
    /// # Errors
    ///
    /// Returns an error when `dir` lacks `bookclerk-workerd` or `workerd`.
    pub fn locate_in(dir: &Path) -> Result<Self> {
        let launcher = dir.join(launcher_bin_name());
        if !launcher.is_file() {
            return Err(PluginError::message(format!(
                "{} not found in {}",
                launcher_bin_name(),
                dir.display()
            )));
        }
        let workerd_bin = dir.join(cloudflare_workerd_bin_name());
        if !workerd_bin.is_file() {
            return Err(PluginError::message(format!(
                "pinned `{}` not found beside {}",
                cloudflare_workerd_bin_name(),
                launcher.display()
            )));
        }
        Ok(Self {
            launcher,
            workerd_bin,
        })
    }
}

/// Everything the spawn path needs to start one guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    /// Program the jail execs: `bookclerk-workerd`, or the native command on
    /// the diagnostic transport.
    pub launcher: PathBuf,
    /// Arguments after `launcher` (manifest `args`; diagnostic transport only).
    pub args: Vec<String>,
    /// Native executable `bookclerk-workerd` fronts, exported as
    /// [`NATIVE_BACKEND_ENV`]. `None` for author isolates and direct native.
    pub native_backend: Option<PathBuf>,
    /// Pinned `workerd` the launcher will spawn (must stay readable in the jail).
    pub workerd_bin: Option<PathBuf>,
    /// Process tree this plan produces.
    pub runtime: GuestRuntimeKind,
}

impl SpawnPlan {
    /// Resolves the launch for `plugin` on `transport` using the host's front door.
    ///
    /// # Errors
    ///
    /// Fails when the front door is required but `bookclerk-workerd` or the
    /// pinned `workerd` cannot be found, when a native manifest declares `args`
    /// (the launcher spawns the backend with none), or when a `workerd` manifest
    /// asks for the diagnostic transport.
    pub fn resolve(plugin: &DiscoveredPlugin, transport: SpawnTransport) -> Result<Self> {
        Self::resolve_with(plugin, transport, WorkerdFrontDoor::locate)
    }

    /// [`Self::resolve`] with an injected front-door locator (called only when
    /// the plan needs `bookclerk-workerd`).
    ///
    /// # Errors
    ///
    /// Same as [`Self::resolve`]; locator failures are wrapped in the
    /// front-door refusal message.
    pub fn resolve_with(
        plugin: &DiscoveredPlugin,
        transport: SpawnTransport,
        locate: impl FnOnce() -> Result<WorkerdFrontDoor>,
    ) -> Result<Self> {
        let id = &plugin.manifest.id;
        let runtime = GuestRuntimeKind::for_manifest(plugin.manifest.runtime, transport);
        match runtime {
            GuestRuntimeKind::NativeDirect => Ok(Self {
                launcher: plugin.command.clone(),
                args: plugin.manifest.args.clone(),
                native_backend: None,
                workerd_bin: None,
                runtime,
            }),
            GuestRuntimeKind::Workerd => {
                if transport == SpawnTransport::DirectNativeDiagnostic {
                    return Err(PluginError::message(format!(
                        "plugin `{id}` is a workerd isolate; the direct native diagnostic \
                         transport has no native guest to connect to"
                    )));
                }
                let front_door = locate().map_err(|err| front_door_unavailable(id, &err))?;
                Ok(Self {
                    launcher: front_door.launcher,
                    args: Vec::new(),
                    native_backend: None,
                    workerd_bin: Some(front_door.workerd_bin),
                    runtime,
                })
            }
            GuestRuntimeKind::NativeBehindWorkerd => {
                if !plugin.manifest.args.is_empty() {
                    return Err(PluginError::message(format!(
                        "plugin `{id}`: manifest `args` are not supported behind the \
                         bookclerk-workerd front door (the launcher starts `command` with no \
                         arguments); wrap them in the executable instead"
                    )));
                }
                let front_door = locate().map_err(|err| front_door_unavailable(id, &err))?;
                Ok(Self {
                    launcher: front_door.launcher,
                    args: Vec::new(),
                    native_backend: Some(plugin.command.clone()),
                    workerd_bin: Some(front_door.workerd_bin),
                    runtime,
                })
            }
        }
    }

    /// True when `bookclerk-workerd` is the program the jail execs.
    #[must_use]
    pub fn fronted_by_workerd(&self) -> bool {
        self.runtime.fronted_by_workerd()
    }

    /// Executable paths the jail must let the launcher tree read and exec.
    #[must_use]
    pub fn executable_reads(&self) -> Vec<PathBuf> {
        let mut reads = vec![self.launcher.clone()];
        reads.extend(self.native_backend.iter().cloned());
        reads.extend(self.workerd_bin.iter().cloned());
        reads
    }
}

/// Refusal for a product spawn whose front door is missing, in every isolation mode.
fn front_door_unavailable(id: &str, err: &PluginError) -> PluginError {
    PluginError::message(format!(
        "refusing to start plugin `{id}`: every plugin runs behind the bookclerk-workerd \
         front door and it is unavailable ({err}). Ship `bookclerk-workerd` and the pinned \
         Cloudflare `workerd` beside the host binary (`cargo build-app --platform` / \
         `cargo ensure-workerd`), or point {WORKERD_LAUNCHER_ENV} / {WORKERD_BIN_ENV} at them. \
         [plugins].isolation does not relax this; direct native spawn is diagnostic-only"
    ))
}

/// `bookclerk-workerd` filename for this platform.
fn launcher_bin_name() -> &'static str {
    if cfg!(windows) {
        "bookclerk-workerd.exe"
    } else {
        "bookclerk-workerd"
    }
}

/// Filename of the pinned Cloudflare `workerd` binary (`workerd.exe` on Windows).
pub(crate) fn cloudflare_workerd_bin_name() -> &'static str {
    if cfg!(windows) {
        "workerd.exe"
    } else {
        "workerd"
    }
}

/// Finds `bookclerk-workerd`: env override, beside the host executable (or its
/// `deps/` parent), then `PATH`.
pub(crate) fn locate_launcher() -> Result<PathBuf> {
    let name = launcher_bin_name();
    if let Some(path) = std::env::var_os(WORKERD_LAUNCHER_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(PluginError::message(format!(
            "{WORKERD_LAUNCHER_ENV} points at {}, which is not a file",
            path.display()
        )));
    }
    let mut searched = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
            searched.push(dir.to_path_buf());
            // An integration test binary runs from `target/<profile>/deps`, one
            // level below where cargo puts the launcher.
            if dir.file_name().is_some_and(|last| last == "deps") {
                if let Some(parent) = dir.parent() {
                    let candidate = parent.join(name);
                    if candidate.is_file() {
                        return Ok(candidate);
                    }
                    searched.push(parent.to_path_buf());
                }
            }
        }
    }
    if let Some(path) = which_in_path(name) {
        return Ok(path);
    }
    Err(PluginError::message(format!(
        "{name} not found beside the host binary ({}), on PATH, or via {WORKERD_LAUNCHER_ENV}",
        searched
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Finds the pinned `workerd`: [`WORKERD_BIN_ENV`] when set, else beside `launcher`.
fn locate_workerd_bin(launcher: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(WORKERD_BIN_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(PluginError::message(format!(
            "{WORKERD_BIN_ENV} points at {}, which is not a file",
            path.display()
        )));
    }
    let candidate = launcher
        .parent()
        .map(|dir| dir.join(cloudflare_workerd_bin_name()))
        .unwrap_or_default();
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(PluginError::message(format!(
        "pinned Cloudflare `{}` not found beside {} (run `cargo ensure-workerd`)",
        cloudflare_workerd_bin_name(),
        launcher.display()
    )))
}

/// Returns the first `PATH` entry that contains a file named `name`.
fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Native manifest at `root` with `./guest` as its command.
    fn native_plugin(root: &Path, args: &str) -> DiscoveredPlugin {
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").expect("write guest");
        let manifest = crate::PluginManifest::parse(&format!(
            r#"
api_version = 3
id = "probe"
version = "0.0.0"
runtime = "native"
command = "./guest"
{args}
entrypoints = ["remoteLibrary"]

[capabilities.network]
mode = "deny"
"#
        ))
        .expect("test manifest");
        DiscoveredPlugin {
            manifest,
            root: root.to_path_buf(),
            command,
        }
    }

    /// Fake front door: empty `bookclerk-workerd` + `workerd` files in `dir`.
    fn fake_front_door(dir: &Path) -> WorkerdFrontDoor {
        std::fs::write(dir.join(launcher_bin_name()), b"").expect("launcher");
        std::fs::write(dir.join(cloudflare_workerd_bin_name()), b"").expect("workerd");
        WorkerdFrontDoor::locate_in(dir).expect("fake front door")
    }

    #[test]
    fn default_transport_fronts_a_native_manifest_with_workerd() {
        let install = tempfile::tempdir().expect("tempdir");
        let helpers = tempfile::tempdir().expect("tempdir");
        let plugin = native_plugin(install.path(), "");
        let front_door = fake_front_door(helpers.path());

        let plan = SpawnPlan::resolve_with(&plugin, SpawnTransport::WorkerdFrontDoor, || {
            Ok(front_door.clone())
        })
        .expect("plan");
        assert_eq!(plan.launcher, front_door.launcher);
        assert_eq!(
            plan.native_backend.as_deref(),
            Some(plugin.command.as_path())
        );
        assert_eq!(
            plan.workerd_bin.as_deref(),
            Some(front_door.workerd_bin.as_path())
        );
        assert_eq!(plan.runtime, GuestRuntimeKind::NativeBehindWorkerd);
        assert_eq!(plan.runtime.label(), "native-behind-workerd");
        assert!(plan.fronted_by_workerd());
        assert!(plan.args.is_empty());
        assert!(plan.executable_reads().contains(&plugin.command));
    }

    #[test]
    fn diagnostic_transport_yields_the_raw_native_command() {
        let install = tempfile::tempdir().expect("tempdir");
        let plugin = native_plugin(install.path(), r#"args = ["--probe"]"#);
        let plan = SpawnPlan::resolve_with(&plugin, SpawnTransport::DirectNativeDiagnostic, || {
            panic!("direct native must not look for the front door")
        })
        .expect("plan");
        assert_eq!(plan.launcher, plugin.command);
        assert_eq!(plan.args, vec!["--probe".to_string()]);
        assert_eq!(plan.native_backend, None);
        assert_eq!(plan.workerd_bin, None);
        assert_eq!(plan.runtime, GuestRuntimeKind::NativeDirect);
        assert_eq!(plan.runtime.label(), "native-direct");
        assert!(!plan.fronted_by_workerd());
    }

    #[test]
    fn missing_front_door_is_a_hard_error_naming_the_launcher() {
        let install = tempfile::tempdir().expect("tempdir");
        let empty = tempfile::tempdir().expect("tempdir");
        let plugin = native_plugin(install.path(), "");
        let err = SpawnPlan::resolve_with(&plugin, SpawnTransport::WorkerdFrontDoor, || {
            WorkerdFrontDoor::locate_in(empty.path())
        })
        .expect_err("must refuse");
        let message = err.to_string();
        assert!(
            message.contains("refusing to start plugin `probe`"),
            "{message}"
        );
        assert!(
            message.contains("bookclerk-workerd front door"),
            "{message}"
        );
        assert!(message.contains("diagnostic-only"), "{message}");
    }

    #[test]
    fn missing_pinned_workerd_is_a_hard_error() {
        let install = tempfile::tempdir().expect("tempdir");
        let helpers = tempfile::tempdir().expect("tempdir");
        std::fs::write(helpers.path().join(launcher_bin_name()), b"").expect("launcher");
        let plugin = native_plugin(install.path(), "");
        let err = SpawnPlan::resolve_with(&plugin, SpawnTransport::WorkerdFrontDoor, || {
            WorkerdFrontDoor::locate_in(helpers.path())
        })
        .expect_err("must refuse");
        assert!(err.to_string().contains("pinned"), "{err}");
    }

    #[test]
    fn native_args_cannot_cross_the_front_door() {
        let install = tempfile::tempdir().expect("tempdir");
        let helpers = tempfile::tempdir().expect("tempdir");
        let plugin = native_plugin(install.path(), r#"args = ["--probe"]"#);
        let front_door = fake_front_door(helpers.path());
        let err = SpawnPlan::resolve_with(&plugin, SpawnTransport::WorkerdFrontDoor, || {
            Ok(front_door.clone())
        })
        .expect_err("must refuse");
        assert!(err.to_string().contains("`args`"), "{err}");
    }

    #[test]
    fn workerd_manifest_rejects_the_diagnostic_transport() {
        use crate::manifest::WorkerdRuntimeManifest;

        let install = tempfile::tempdir().expect("tempdir");
        let mut plugin = native_plugin(install.path(), "");
        plugin.manifest.runtime = PluginRuntimeKind::Workerd;
        plugin.manifest.command = None;
        plugin.manifest.workerd = Some(WorkerdRuntimeManifest {
            compatibility_date: "2026-08-01".into(),
            compatibility_flags: vec![],
            main_module: "index.js".into(),
            modules_dir: "modules".into(),
            entrypoint: "default".into(),
            limits: Default::default(),
        });
        let err = SpawnPlan::resolve_with(&plugin, SpawnTransport::DirectNativeDiagnostic, || {
            panic!("must fail before locating")
        })
        .expect_err("must refuse");
        assert!(err.to_string().contains("workerd isolate"), "{err}");
    }

    #[test]
    fn process_overhead_counts_every_launcher_tree_member() {
        assert_eq!(GuestRuntimeKind::NativeDirect.process_overhead(), 1);
        assert_eq!(GuestRuntimeKind::Workerd.process_overhead(), 2);
        assert_eq!(GuestRuntimeKind::NativeBehindWorkerd.process_overhead(), 3);
        assert_eq!(
            GuestRuntimeKind::for_manifest(PluginRuntimeKind::Native, SpawnTransport::default()),
            GuestRuntimeKind::NativeBehindWorkerd
        );
    }
}
