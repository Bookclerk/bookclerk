//! What a plugin guest may reach, and how the host makes that stick.
//!
//! # The boundary
//!
//! A guest is handed three directories and nothing else:
//!
//! - its own install directory, read-only — the binary and `plugin.toml`
//! - `…/plugins/<id>/data`, its private state, also exported as `HOME`
//! - `…/plugins/<id>/tmp`, its scratch, exported as `TMPDIR`
//! - the download cache root, where fetched media is staged for the host
//!
//! That leaves out everything that matters: `master.key`, `library.db`, the
//! operator token, the output library, the config file, and every other plugin's
//! data directory. None of it is a loss, because the host already mediates the
//! things a guest would otherwise need those for — credentials arrive as RPC
//! parameters and scan results go back the same way, so a guest has never had a
//! reason to open the database.
//!
//! # Why the cache root and not one book's directory
//!
//! A guest is long-lived: one process per plugin, serving every call for the
//! life of the daemon. The `cache_dir` a fetch works in arrives as an RPC
//! parameter, so it is not known when the jail is built, and confinement cannot
//! be narrowed later. The grant is therefore the cache root rather than one
//! book's subdirectory. A media job, which gets exactly one book and one output,
//! is confined far more tightly — see `docs/media.md`.
//!
//! # Why a launcher
//!
//! The guest cannot be asked to confine itself; see the `bookclerk-jail` crate
//! docs for why, and for what permitting `execve` costs.

use std::path::{Path, PathBuf};

use bookclerk_config::{Config, Isolation};
use bookclerk_sandbox::{Enforcement, NetPolicy, Spec};

use crate::discover::DiscoveredPlugin;
use crate::manifest::NetworkNeed;
use crate::{PluginError, Result};

/// Launcher binary that applies the jail.
const JAIL_BIN_NAME: &str = "bookclerk-jail";
/// Override for the launcher path, folded into `[plugins].jail_bin` by config.
const JAIL_BIN_ENV: &str = "BOOKCLERK_PLUGIN_JAIL";

/// The private directory a plugin keeps state in.
#[must_use]
pub fn plugin_data_dir(config: &Config, plugin_id: &str) -> PathBuf {
    plugin_state_root(config, plugin_id).join("data")
}

/// Scratch space for one plugin, used as its `TMPDIR`.
///
/// Guests inherit `TMPDIR` from the host otherwise, which names a directory
/// outside every jail — so a guest reaching for a temp file would fail on a
/// permission error unrelated to anything it was denied.
#[must_use]
fn plugin_scratch_dir(config: &Config, plugin_id: &str) -> PathBuf {
    plugin_state_root(config, plugin_id).join("tmp")
}

/// Where one plugin's host-managed directories live.
///
/// Distinct from [`DiscoveredPlugin::root`], which is where the plugin is
/// installed and is read-only to the guest.
fn plugin_state_root(config: &Config, plugin_id: &str) -> PathBuf {
    config
        .paths()
        .files_dir
        .join("plugins")
        .join(sanitize_id(plugin_id).as_ref())
}

/// How a guest will be started.
#[derive(Debug)]
pub(crate) enum Start {
    /// Through the launcher, which applies `spec` and then becomes the guest.
    Confined { launcher: PathBuf, spec: Box<Spec> },
    /// Directly, with no jail. Only reachable when the operator turned isolation
    /// off, or asked for best-effort on a host that cannot confine.
    Unconfined { reason: String },
}

/// A guest's directories plus the decision about how to start it.
#[derive(Debug)]
pub(crate) struct GuestJail {
    /// Private state directory, also the guest's `HOME`.
    pub data: PathBuf,
    /// Scratch directory, the guest's `TMPDIR`.
    pub scratch: PathBuf,
    pub start: Start,
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
        let data = plugin_data_dir(config, id);
        let scratch = plugin_scratch_dir(config, id);
        let cache = config.download_cache_dir();

        for dir in [&data, &scratch, &cache] {
            std::fs::create_dir_all(dir).map_err(|err| {
                PluginError::message(format!("could not create {}: {err}", dir.display()))
            })?;
        }

        let isolation = config.plugins.isolation;
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
                    Ok(launcher) => Start::Confined {
                        launcher,
                        spec: Box::new(build_spec(plugin, &data, &scratch, &cache, enforcement)),
                    },
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
        })
    }
}

/// Build the allowlist for one guest.
fn build_spec(
    plugin: &DiscoveredPlugin,
    data: &Path,
    scratch: &Path,
    cache: &Path,
    enforcement: Enforcement,
) -> Spec {
    Spec {
        label: format!("plugin:{}", plugin.manifest.id),
        // The install directory covers `plugin.toml` and, in the usual layout,
        // the binary. A manifest may name an absolute `command` elsewhere, so
        // grant that too rather than relying on the two coinciding.
        reads: vec![plugin.root.clone(), plugin.command.clone()],
        writes: vec![
            data.to_path_buf(),
            scratch.to_path_buf(),
            cache.to_path_buf(),
        ],
        net: match plugin.manifest.sandbox.network {
            NetworkNeed::None => NetPolicy::Deny,
            NetworkNeed::Outbound => NetPolicy::Outbound,
            NetworkNeed::Listen => NetPolicy::OutboundListen,
        },
        // The launcher has to exec the guest to hand over. See the
        // `bookclerk-jail` crate docs on why this is close to free.
        allow_exec: true,
        system_paths: true,
        enforcement,
    }
}

/// Locate the launcher: the configured path, then [`JAIL_BIN_ENV`], then beside
/// the current executable.
fn resolve_launcher(config: &Config, isolation: Isolation) -> std::result::Result<PathBuf, String> {
    if isolation == Isolation::Required {
        let caps = bookclerk_sandbox::capabilities();
        if !caps.filesystem {
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

/// Keep a plugin id from escaping its own directory.
///
/// Ids come from a `plugin.toml` that an attacker may have written, and they are
/// used as a path component. `../../..` would otherwise place a guest's writable
/// data directory wherever it liked.
fn sanitize_id(id: &str) -> std::borrow::Cow<'_, str> {
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && id != "."
        && id != ".."
    {
        return std::borrow::Cow::Borrowed(id);
    }
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.trim_matches('_').is_empty() {
        out = "plugin".to_string();
    }
    std::borrow::Cow::Owned(out)
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

    fn plugin_at(root: &Path, id: &str, network: NetworkNeed) -> DiscoveredPlugin {
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").expect("write guest");
        DiscoveredPlugin {
            manifest: crate::PluginManifest {
                api_version: 1,
                id: id.to_string(),
                name: None,
                kind: crate::PluginKind::Source,
                command: PathBuf::from("./guest"),
                args: vec![],
                cli: None,
                sandbox: crate::manifest::SandboxManifest { network },
            },
            root: root.to_path_buf(),
            command,
        }
    }

    #[test]
    fn the_allowlist_covers_the_guest_dirs_and_nothing_else() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = plugin_at(install.path(), "libro", NetworkNeed::Outbound);

        let spec = build_spec(
            &plugin,
            &plugin_data_dir(&config, "libro"),
            &plugin_scratch_dir(&config, "libro"),
            &config.download_cache_dir(),
            Enforcement::Required,
        );

        assert_eq!(spec.label, "plugin:libro");
        assert_eq!(spec.net, NetPolicy::Outbound);
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

    /// The files dir is the parent of both the cache and every plugin's data
    /// directory, so granting it would hand over the database and the key.
    #[test]
    fn the_files_dir_itself_is_never_granted() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugin = plugin_at(install.path(), "libro", NetworkNeed::Outbound);
        let spec = build_spec(
            &plugin,
            &plugin_data_dir(&config, "libro"),
            &plugin_scratch_dir(&config, "libro"),
            &config.download_cache_dir(),
            Enforcement::Required,
        );
        assert!(!spec.writes.contains(&config.paths().files_dir));
        assert!(!spec.reads.contains(&config.paths().files_dir));
    }

    #[test]
    fn one_guest_cannot_reach_another_guests_data() {
        let files = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let mine = plugin_data_dir(&config, "libro");
        let theirs = plugin_data_dir(&config, "audible");
        assert!(!theirs.starts_with(&mine));
        assert!(!mine.starts_with(&theirs));
    }

    #[test]
    fn network_need_maps_to_the_matching_policy() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        for (need, expected) in [
            (NetworkNeed::None, NetPolicy::Deny),
            (NetworkNeed::Outbound, NetPolicy::Outbound),
            (NetworkNeed::Listen, NetPolicy::OutboundListen),
        ] {
            let plugin = plugin_at(install.path(), "x", need);
            let spec = build_spec(
                &plugin,
                &plugin_data_dir(&config, "x"),
                &plugin_scratch_dir(&config, "x"),
                &config.download_cache_dir(),
                Enforcement::Required,
            );
            assert_eq!(spec.net, expected, "{need:?}");
        }
    }

    /// A manifest is written by whoever shipped the plugin, so a hostile id must
    /// not be able to place a writable directory outside the plugins tree.
    #[test]
    fn a_traversing_plugin_id_stays_inside_the_plugins_tree() {
        let files = tempfile::tempdir().expect("tempdir");
        let config = config_at(files.path());
        let plugins_root = files.path().join("plugins");
        for hostile in ["../../etc", "..", ".", "a/b", "/absolute"] {
            let data = plugin_data_dir(&config, hostile);
            assert!(
                data.starts_with(&plugins_root),
                "id {hostile:?} escaped to {}",
                data.display()
            );
        }
    }

    #[test]
    fn planning_creates_the_directories_it_grants() {
        let files = tempfile::tempdir().expect("tempdir");
        let install = tempfile::tempdir().expect("tempdir");
        let mut config = config_at(files.path());
        // Keep this about directory creation rather than about locating the
        // launcher, which the enforcement tests cover.
        config.plugins.isolation = Isolation::Off;
        let plugin = plugin_at(install.path(), "libro", NetworkNeed::Outbound);

        let jail = GuestJail::plan(&config, &plugin).expect("plan");
        assert!(jail.data.is_dir(), "{}", jail.data.display());
        assert!(jail.scratch.is_dir(), "{}", jail.scratch.display());
        assert!(config.download_cache_dir().is_dir());
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
        let plugin = plugin_at(install.path(), "libro", NetworkNeed::Outbound);

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
        let plugin = plugin_at(install.path(), "libro", NetworkNeed::Outbound);

        let jail = GuestJail::plan(&config, &plugin).expect("plan");
        match jail.start {
            Start::Unconfined { reason } => {
                assert!(reason.contains("not a file"), "got: {reason}")
            }
            other => panic!("expected a fallback, got {other:?}"),
        }
    }
}
