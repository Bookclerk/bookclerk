//! Alias uniqueness preflight over the configured discovery universe.
//!
//! The catalog installer only sees `$FILES_DIR/plugins` plus the install
//! ledger. Runtime discovery also searches `BOOKCLERK_PLUGIN_DIRS`. This
//! module applies the same search-root order before any install mutation.
//!
//! Callers must hold [`PluginMutationLock`] for this host `$FILES_DIR` for
//! the entire preflight → catalog install → health → commit/rollback
//! transaction. The lock is host-local and does not coordinate with other
//! `$FILES_DIR` values or a shared Bookclerk database.

use std::path::{Path, PathBuf};

use bookclerk_config::Config;
use bookclerk_library::BOOKCLERK_SCHEMA_NAMESPACE;
use bookclerk_plugin_abi::PRODUCT_API_VERSION;
use bookclerk_plugin_catalog::{
    BookclerkPackageManifest, CatalogError, InstallOptions, InstallOutcome, InstallReceipt,
    Installer, PackageCoordinate, PluginKey, PluginMutationLock,
};

use crate::discover::plugin_search_dirs;
use crate::manifest::PluginManifest;
use crate::{PluginError, Result};

/// Occupancy (PluginKey, alias, path) under every configured discovery root.
///
/// Walks [`plugin_search_dirs`] without hashing payloads. A receipt PluginKey
/// is preferred; otherwise [`PluginKey::from_install_path`].
///
/// # Errors
///
/// Returns when a `plugin.toml` cannot be read or parsed, or a PluginKey
/// cannot be formed for an occupant.
pub fn configured_alias_occupants(config: &Config) -> Result<Vec<(PluginKey, String, PathBuf)>> {
    alias_occupants_in_dirs(&plugin_search_dirs(config))
}

/// Rejects installing `incoming_alias` when a different PluginKey already owns
/// it anywhere in the configured discovery universe.
///
/// Same PluginKey remains eligible for update/replace at the **same** alias.
/// `--replace` cannot seize a foreign alias or rename an installed alias.
///
/// # Errors
///
/// Returns [`PluginError`] when the alias is occupied by another PluginKey or
/// occupancy cannot be read.
pub fn reject_configured_alias_collision(
    config: &Config,
    incoming_key: &PluginKey,
    incoming_alias: &str,
) -> Result<()> {
    reject_alias_collision_in_dirs(&plugin_search_dirs(config), incoming_key, incoming_alias)
}

/// Catalog install after [`reject_configured_alias_collision`].
///
/// `lock` must already cover `opts.plugins_root` so alias preflight and the
/// catalog transaction share one host-local mutation lock.
///
/// # Errors
///
/// Returns a catalog error when the lock does not cover this plugins root,
/// preflight rejects the alias, or install fails.
pub fn install_from_manifest_with_configured_aliases(
    config: &Config,
    lock: &PluginMutationLock,
    manifest: &BookclerkPackageManifest,
    coordinate: &PackageCoordinate,
    opts: &InstallOptions,
) -> bookclerk_plugin_catalog::Result<InstallOutcome> {
    lock.require_plugins_root(&opts.plugins_root)?;
    preflight_install_alias(
        config,
        coordinate,
        &manifest.runtime().id,
        &opts.plugins_root,
    )?;
    Installer::install_from_manifest_with_lock(lock, manifest, coordinate, opts)
}

/// Local-archive install after [`reject_configured_alias_collision`].
///
/// `lock` must already cover `opts.plugins_root`.
///
/// # Errors
///
/// Returns a catalog error when the lock does not cover this plugins root,
/// preflight rejects the alias, or install fails.
pub fn install_local_archive_with_configured_aliases(
    config: &Config,
    lock: &PluginMutationLock,
    archive: &Path,
    manifest: &BookclerkPackageManifest,
    opts: &InstallOptions,
) -> bookclerk_plugin_catalog::Result<InstallOutcome> {
    lock.require_plugins_root(&opts.plugins_root)?;
    let coordinate = Installer::local_archive_coordinate(archive, manifest);
    preflight_install_alias(
        config,
        &coordinate,
        &manifest.runtime().id,
        &opts.plugins_root,
    )?;
    Installer::install_local_archive_with_lock(lock, archive, manifest, opts)
}

/// Resolves the incoming PluginKey and rejects a foreign alias occupant.
fn preflight_install_alias(
    config: &Config,
    coordinate: &PackageCoordinate,
    runtime_id: &str,
    plugins_root: &Path,
) -> bookclerk_plugin_catalog::Result<()> {
    let key = Installer::plugin_key_for(coordinate, runtime_id, plugins_root)?;
    reject_configured_alias_collision(config, &key, runtime_id)
        .map_err(|err| CatalogError::message(err.to_string()))
}

/// Same order as [`plugin_search_dirs`]: extra roots first, then `$FILES_DIR/plugins`.
pub(crate) fn reject_alias_collision_in_dirs(
    dirs: &[PathBuf],
    incoming_key: &PluginKey,
    incoming_alias: &str,
) -> Result<()> {
    let occupants = alias_occupants_in_dirs(dirs)?;
    let matches: Vec<_> = occupants
        .iter()
        .filter(|(_, alias, _)| alias.eq_ignore_ascii_case(incoming_alias))
        .collect();
    if matches.is_empty() {
        return Ok(());
    }
    let foreign_or_dup = matches.len() > 1 || matches.iter().any(|(key, _, _)| key != incoming_key);
    if foreign_or_dup {
        let details: Vec<_> = matches
            .iter()
            .map(|(key, _, path)| format!("`{key}` at {}", path.display()))
            .collect();
        return Err(PluginError::message(format!(
            "plugin alias `{incoming_alias}` is already occupied; \
             a different PluginKey cannot take this alias (`--replace` updates the existing PluginKey only), \
             and the same PluginKey must not appear in more than one discovery root: {}",
            details.join("; ")
        )));
    }
    Ok(())
}

/// Occupancy walk of each configured discovery root.
fn alias_occupants_in_dirs(dirs: &[PathBuf]) -> Result<Vec<(PluginKey, String, PathBuf)>> {
    let mut out = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        occupancy_in_dir(dir, &mut out)?;
    }
    Ok(out)
}

/// Occupancy walk of `$dir/plugin.toml` or `$dir/<name>/plugin.toml`.
fn occupancy_in_dir(dir: &Path, out: &mut Vec<(PluginKey, String, PathBuf)>) -> Result<()> {
    let root_manifest = dir.join("plugin.toml");
    if root_manifest.is_file() {
        push_alias_occupant(&root_manifest, dir, out)?;
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(path = %dir.display(), %err, "cannot read plugin directory");
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("plugin.toml");
        if manifest_path.is_file() {
            push_alias_occupant(&manifest_path, &path, out)?;
        }
    }
    Ok(())
}

/// Parses one occupant (receipt PluginKey when present).
fn push_alias_occupant(
    manifest_path: &Path,
    root: &Path,
    out: &mut Vec<(PluginKey, String, PathBuf)>,
) -> Result<()> {
    let text = std::fs::read_to_string(manifest_path)?;
    let manifest = PluginManifest::parse(&text)?;
    if manifest.id == BOOKCLERK_SCHEMA_NAMESPACE {
        return Ok(());
    }
    if manifest.api_version > PRODUCT_API_VERSION {
        return Ok(());
    }
    let receipt = InstallReceipt::load(root).ok();
    let alias = manifest.id.clone();
    if let Some(row) = receipt.as_ref() {
        if !row.runtime.id.eq_ignore_ascii_case(&alias) {
            return Err(PluginError::message(format!(
                "plugin at {}: install receipt alias `{}` does not match plugin.toml alias `{alias}`",
                root.display(),
                row.runtime.id
            )));
        }
    }
    let key = receipt
        .and_then(|row| row.plugin_key().ok())
        .or_else(|| PluginKey::from_install_path(root, &alias).ok())
        .ok_or_else(|| {
            PluginError::message(format!(
                "plugin alias `{alias}` at {} has no PluginKey",
                root.display()
            ))
        })?;
    out.push((key, alias, root.to_path_buf()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use bookclerk_plugin_catalog::{PluginKind, RegistrySource};

    /// Sets the Unix execute bit so occupancy treats the stub as a command.
    fn chmod_exec(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    /// Writes a minimal `plugin.toml` occupant (no payload hash / receipt).
    fn write_occupant(dir: &Path, id: &str) {
        fs::create_dir_all(dir).unwrap();
        let bin = dir.join("bin");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        chmod_exec(&bin);
        fs::write(
            dir.join("plugin.toml"),
            format!(
                "api_version = 3\nid = \"{id}\"\nruntime = \"native\"\ncommand = \"./bin\"\n\
                 entrypoints = [\"cli\"]\n[capabilities.network]\nmode = \"deny\"\n"
            ),
        )
        .unwrap();
    }

    /// Package manifest used only to compute the incoming PluginKey.
    fn dummy_manifest(id: &str) -> BookclerkPackageManifest {
        BookclerkPackageManifest {
            schema_version: 1,
            protocol: None,
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: id.into(),
            display_name: None,
            description: None,
            coordinate: None,
            artifacts: vec![],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        }
    }

    #[test]
    fn files_dir_plugins_foreign_alias_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_occupant(&tmp.path().join("plugins").join("echo"), "echo");
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let incoming =
            PluginKey::from_install_path(&tmp.path().join("other.tar.gz"), "echo").unwrap();
        let err = reject_configured_alias_collision(&cfg, &incoming, "echo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already occupied"), "{err}");
        assert!(err.contains("PluginKey"), "{err}");
    }

    #[test]
    fn extra_discovery_dir_foreign_alias_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let extra = tmp.path().join("extra");
        write_occupant(&extra.join("echo"), "echo");
        let files = tmp.path().join("files");
        fs::create_dir_all(files.join("plugins")).unwrap();
        let incoming =
            PluginKey::from_install_path(&tmp.path().join("incoming.tar.gz"), "echo").unwrap();
        let dirs = vec![extra, files.join("plugins")];
        let err = reject_alias_collision_in_dirs(&dirs, &incoming, "echo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already occupied"), "{err}");
        assert!(err.contains("--replace"), "{err}");
    }

    #[test]
    fn same_plugin_key_update_is_permitted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("plugins").join("echo");
        write_occupant(&root, "echo");
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let key = PluginKey::from_install_path(&root, "echo").unwrap();
        reject_configured_alias_collision(&cfg, &key, "echo").unwrap();
    }

    #[test]
    fn replace_does_not_override_foreign_plugin_key() {
        let tmp = tempfile::tempdir().unwrap();
        write_occupant(&tmp.path().join("plugins").join("echo"), "echo");
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let incoming =
            PluginKey::from_install_path(&tmp.path().join("seizure.tar.gz"), "echo").unwrap();
        let err = reject_configured_alias_collision(&cfg, &incoming, "ECHO")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already occupied"), "{err}");
        assert!(err.contains("--replace"), "{err}");
    }

    #[test]
    fn preflight_reject_does_not_mutate_install_tree_or_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let occupant = tmp.path().join("plugins").join("echo");
        write_occupant(&occupant, "echo");
        let ledger = tmp.path().join("install-ledger.json");
        fs::write(&ledger, b"{\"schema_version\":1,\"artifacts\":[]}").unwrap();
        let cfg = Config {
            paths: Some(bookclerk_config::Paths::from_files_dir(
                tmp.path().to_path_buf(),
            )),
            ..Config::default()
        };
        let archive = tmp.path().join("incoming.tar.gz");
        fs::write(&archive, b"not-an-archive").unwrap();
        let manifest = dummy_manifest("echo");
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let opts = InstallOptions {
            plugins_root: tmp.path().join("plugins"),
            trust: bookclerk_plugin_catalog::TrustPolicy::allow_unverified_publisher(),
            ..Default::default()
        };
        let lock = PluginMutationLock::acquire(tmp.path()).unwrap();
        let err =
            install_from_manifest_with_configured_aliases(&cfg, &lock, &manifest, &coord, &opts)
                .unwrap_err()
                .to_string();
        assert!(err.contains("already occupied"), "{err}");
        assert_eq!(
            fs::read(&ledger).unwrap(),
            b"{\"schema_version\":1,\"artifacts\":[]}"
        );
        assert!(occupant.join("plugin.toml").is_file());
        assert!(!tmp.path().join("plugins").join(".staging").is_dir());
        let mut kids: Vec<_> = fs::read_dir(tmp.path().join("plugins"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        kids.sort();
        assert_eq!(kids, vec!["echo".to_string()]);
    }
}
