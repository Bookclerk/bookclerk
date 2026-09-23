//! Evaluate an installed plugin tree into [`PluginInstallIdentity`].

use std::path::Path;

use bookclerk_plugin_manifest::PluginManifest;

use crate::error::{CatalogError, Result};
use crate::identity::{
    is_platform_plugin_key, platform_artifact, ArtifactIdentity, PluginInstallIdentity, PluginKey,
    PluginProvenance, PLATFORM_PRODUCT,
};
use crate::ledger::{entry_matches, record_install, InstallLedger};
use crate::payload::{manifest_sha256, payload_root_sha256};
use crate::receipt::InstallReceipt;

/// Evaluate identity + provenance for an install directory.
///
/// Equivalent to [`evaluate_install_in`] with no host files directory. Without
/// a files-dir ledger, platform trust cannot be established.
///
/// # Errors
///
/// Returns an error when the payload cannot be hashed, `plugin.toml` is
/// missing, a receipt is malformed, or a present receipt is not recorded in
/// the host ledger.
pub fn evaluate_install(root: &Path, manifest: &PluginManifest) -> Result<PluginInstallIdentity> {
    evaluate_install_in(root, manifest, None)
}

/// Evaluate identity + provenance, consulting the host install ledger under
/// `files_dir` when provided.
///
/// Receipt fields are **not** a trust anchor. [`PluginProvenance::PlatformBundled`]
/// requires a matching [`InstallLedger`] row written by host tooling, a known
/// platform [`PluginKey`], and exact current payload digests.
///
/// A genuinely absent receipt is [`PluginProvenance::LocalDevelopment`]. Any
/// other receipt/hash failure fails closed.
///
/// # Errors
///
/// Returns an error when hashing fails, a receipt is unreadable or malformed,
/// the stored PluginKey is invalid, or a present receipt has no matching
/// host ledger entry.
pub fn evaluate_install_in(
    root: &Path,
    manifest: &PluginManifest,
    files_dir: Option<&Path>,
) -> Result<PluginInstallIdentity> {
    // Resolve the trusted install identity once so intentional `..` spelling in
    // `files_dir` / discovery roots does not fail hashing, and so payload walks
    // use the same canonical tree.
    let root_buf = root.canonicalize().map_err(|source| {
        CatalogError::message(format!(
            "could not canonicalize plugin root {}: {source}",
            root.display()
        ))
    })?;
    let root = root_buf.as_path();
    let manifest_sha = manifest_sha256(root)?;
    let payload_sha = payload_root_sha256(root)?;
    let version = manifest.version.clone().unwrap_or_else(|| "0.0.0".into());

    match InstallReceipt::load(root) {
        Err(err) if err.is_receipt_not_found() => {
            let plugin_key = PluginKey::from_install_path(root, &manifest.id)?;
            let artifact = ArtifactIdentity {
                plugin_key: plugin_key.clone(),
                version,
                manifest_sha256: manifest_sha,
                payload_root_sha256: payload_sha,
                archive_sha256: None,
            };
            Ok(PluginInstallIdentity {
                plugin_key,
                alias: manifest.id.clone(),
                artifact,
                provenance: PluginProvenance::LocalDevelopment,
            })
        }
        Err(err) => Err(err),
        Ok(receipt) => {
            let plugin_key = receipt.plugin_key()?;
            require_key_derived_install_path(root, &plugin_key)?;
            if !receipt.runtime.id.eq_ignore_ascii_case(&manifest.id) {
                return Err(CatalogError::message(format!(
                    "install receipt alias `{}` does not match plugin.toml alias `{}` at {}",
                    receipt.runtime.id,
                    manifest.id,
                    root.display()
                )));
            }
            let hashes_match = receipt.manifest_sha256.eq_ignore_ascii_case(&manifest_sha)
                && receipt
                    .payload_root_sha256
                    .eq_ignore_ascii_case(&payload_sha);
            let ledger_entry = match files_dir {
                Some(dir) => InstallLedger::load(dir)?.get(&plugin_key).cloned(),
                None => None,
            };
            let provenance = if !hashes_match {
                PluginProvenance::Modified
            } else {
                let Some(entry) = ledger_entry.as_ref() else {
                    return Err(CatalogError::message(format!(
                        "install receipt for `{plugin_key}` is not recorded in the host install ledger"
                    )));
                };
                if !entry_matches(entry, &plugin_key, &manifest_sha, &payload_sha) {
                    PluginProvenance::Modified
                } else if entry.provenance == PluginProvenance::PlatformBundled
                    && is_platform_plugin_key(&plugin_key)
                    && platform_artifact(&plugin_key.package(), manifest.id.as_str()).is_some()
                {
                    PluginProvenance::PlatformBundled
                } else {
                    PluginProvenance::VerifiedInstalled
                }
            };
            let artifact = ArtifactIdentity {
                plugin_key: plugin_key.clone(),
                version,
                manifest_sha256: manifest_sha,
                payload_root_sha256: payload_sha,
                archive_sha256: Some(receipt.archive_sha256.clone()).filter(|s| !s.is_empty()),
            };
            Ok(PluginInstallIdentity {
                plugin_key,
                alias: manifest.id.clone(),
                artifact,
                provenance,
            })
        }
    }
}

/// For `pk-*` install directories, require `basename == key.fs_id()`.
fn require_key_derived_install_path(root: &Path, key: &PluginKey) -> Result<()> {
    let Some(name) = root.file_name().and_then(|n| n.to_str()) else {
        return Err(CatalogError::message(format!(
            "install directory {} has a non-Unicode name; refusing to trust receipt PluginKey",
            root.display()
        )));
    };
    if name.starts_with("pk-") && name != key.fs_id() {
        return Err(CatalogError::message(format!(
            "install directory {} does not match receipt PluginKey filesystem id `{}`",
            root.display(),
            key.fs_id()
        )));
    }
    Ok(())
}

/// Build a host-owned platform-bundled receipt **and** ledger row.
///
/// Only [`crate::identity::PLATFORM_ARTIFACTS`] may be stamped this way.
/// `files_dir` is the Bookclerk files directory that owns `install-ledger.json`.
///
/// # Errors
///
/// Returns an error when `package_name`/`manifest.id` are not a known
/// platform pair, hashing fails, or the ledger cannot be written.
pub fn stamp_platform_receipt(
    root: &Path,
    files_dir: &Path,
    package_name: &str,
    manifest: &PluginManifest,
    version: &str,
) -> Result<InstallReceipt> {
    let Some(spec) = platform_artifact(package_name, &manifest.id) else {
        return Err(CatalogError::message(format!(
            "refusing platform receipt for `{package_name}` id `{}` (not a Bookclerk platform artifact)",
            manifest.id
        )));
    };
    let plugin_key = spec.plugin_key();
    let manifest_sha = manifest_sha256(root)?;
    let payload_sha = payload_root_sha256(root)?;
    let receipt = InstallReceipt::platform_bundled(
        plugin_key.clone(),
        version,
        manifest,
        manifest_sha.clone(),
        payload_sha.clone(),
        PLATFORM_PRODUCT,
        package_name,
    );
    receipt.store(root)?;
    record_install(
        files_dir,
        &plugin_key,
        package_name,
        &manifest.id,
        &manifest_sha,
        &payload_sha,
        PluginProvenance::PlatformBundled,
    )?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::write_file;
    use crate::identity::PLUGIN_KEY_FS_ID_HEX_CHARS;

    fn write_sqlite_tree(root: &Path) {
        write_file(
            &root.join("plugin.toml"),
            br#"
api_version = 3
id = "sqlite"
runtime = "native"
command = "./guest"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"
"#,
        )
        .unwrap();
        write_file(&root.join("guest"), b"#!/bin/sh\n").unwrap();
    }

    fn parse_sqlite(root: &Path) -> PluginManifest {
        let text = std::fs::read_to_string(root.join("plugin.toml")).unwrap();
        PluginManifest::parse(&text).unwrap()
    }

    fn files_plugin_root(files: &Path) -> std::path::PathBuf {
        let key = PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite").unwrap();
        let dest = files.join("plugins").join(key.fs_id());
        std::fs::create_dir_all(&dest).unwrap();
        dest
    }

    #[test]
    fn missing_receipt_is_local_development() {
        let dir = tempfile::tempdir().unwrap();
        write_sqlite_tree(dir.path());
        let manifest = parse_sqlite(dir.path());
        let identity = evaluate_install(dir.path(), &manifest).unwrap();
        assert_eq!(identity.provenance, PluginProvenance::LocalDevelopment);
        assert_eq!(identity.alias(), "sqlite");
        assert!(!is_platform_plugin_key(&identity.plugin_key));
    }

    #[test]
    fn stamped_platform_receipt_is_trusted_until_modified() {
        let files = tempfile::tempdir().unwrap();
        let root = files_plugin_root(files.path());
        write_sqlite_tree(&root);
        let manifest = parse_sqlite(&root);
        stamp_platform_receipt(
            &root,
            files.path(),
            "bookclerk-plugin-database-sqlite",
            &manifest,
            "0.1.0",
        )
        .unwrap();
        let identity = evaluate_install_in(&root, &manifest, Some(files.path())).unwrap();
        assert_eq!(identity.provenance, PluginProvenance::PlatformBundled);
        assert!(identity.provenance.grants_platform_defaults());
        assert_eq!(
            identity.plugin_key.fs_id().len(),
            3 + PLUGIN_KEY_FS_ID_HEX_CHARS
        );

        write_file(&root.join("guest"), b"#!/bin/sh\n#pwn\n").unwrap();
        let identity = evaluate_install_in(&root, &manifest, Some(files.path())).unwrap();
        assert_eq!(identity.provenance, PluginProvenance::Modified);
        assert!(!identity.provenance.grants_platform_defaults());
    }

    #[test]
    fn pk_directory_mismatching_receipt_fs_id_fails_closed() {
        let files = tempfile::tempdir().unwrap();
        let key = PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite").unwrap();
        let wrong = files
            .path()
            .join("plugins")
            .join("pk-ffffffffffffffffffffffffffffffff");
        std::fs::create_dir_all(&wrong).unwrap();
        write_sqlite_tree(&wrong);
        let manifest = parse_sqlite(&wrong);
        stamp_platform_receipt(
            &wrong,
            files.path(),
            "bookclerk-plugin-database-sqlite",
            &manifest,
            "0.1.0",
        )
        .unwrap();
        // Move receipt+ledger to claim the real key while sitting under a foreign pk-* leaf.
        let err = evaluate_install_in(&wrong, &manifest, Some(files.path()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not match receipt PluginKey filesystem id")
                || err.contains(&key.fs_id()),
            "{err}"
        );
    }

    #[test]
    fn forged_platform_receipt_without_ledger_is_not_trusted() {
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("plugins").join("evil-sqlite");
        std::fs::create_dir_all(&root).unwrap();
        write_sqlite_tree(&root);
        let manifest = parse_sqlite(&root);
        let plugin_key = PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite").unwrap();
        let manifest_sha = manifest_sha256(&root).unwrap();
        let payload_sha = payload_root_sha256(&root).unwrap();
        let receipt = InstallReceipt::platform_bundled(
            plugin_key,
            "0.1.0",
            &manifest,
            manifest_sha,
            payload_sha,
            PLATFORM_PRODUCT,
            "bookclerk-plugin-database-sqlite",
        );
        receipt.store(&root).unwrap();

        let err = evaluate_install_in(&root, &manifest, Some(files.path())).unwrap_err();
        assert!(
            err.to_string().contains("install ledger"),
            "forged receipt must not establish platform trust: {err}"
        );
        let identity = evaluate_install(&root, &manifest).unwrap_err();
        assert!(identity.to_string().contains("install ledger"));
    }

    #[test]
    fn malformed_receipt_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        write_sqlite_tree(dir.path());
        write_file(&dir.path().join("receipt.json"), b"{not-json").unwrap();
        let manifest = parse_sqlite(dir.path());
        let err = evaluate_install(dir.path(), &manifest).unwrap_err();
        assert!(
            !err.is_receipt_not_found(),
            "malformed receipt must not look absent: {err}"
        );
    }

    #[test]
    fn ledger_planted_inside_plugin_tree_cannot_establish_platform_trust() {
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("plugins").join("evil-sqlite");
        std::fs::create_dir_all(&root).unwrap();
        write_sqlite_tree(&root);
        let plugin_key = PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite").unwrap();
        record_install(
            &root,
            &plugin_key,
            "bookclerk-plugin-database-sqlite",
            "sqlite",
            "deadbeef",
            "deadbeef",
            PluginProvenance::PlatformBundled,
        )
        .unwrap();
        let manifest = parse_sqlite(&root);
        let manifest_sha = manifest_sha256(&root).unwrap();
        let payload_sha = payload_root_sha256(&root).unwrap();
        InstallReceipt::platform_bundled(
            plugin_key,
            "0.1.0",
            &manifest,
            manifest_sha,
            payload_sha,
            PLATFORM_PRODUCT,
            "bookclerk-plugin-database-sqlite",
        )
        .store(&root)
        .unwrap();
        assert!(root.join("install-ledger.json").is_file());
        let err = evaluate_install_in(&root, &manifest, Some(files.path())).unwrap_err();
        assert!(
            err.to_string().contains("install ledger"),
            "plugin-adjacent ledger must not be the trust anchor: {err}"
        );
    }

    #[test]
    fn fake_sqlite_id_cannot_stamp_platform_receipt_with_wrong_package() {
        let files = tempfile::tempdir().unwrap();
        let root = files_plugin_root(files.path());
        write_sqlite_tree(&root);
        let manifest = parse_sqlite(&root);
        let err = stamp_platform_receipt(&root, files.path(), "evil-sqlite", &manifest, "1.0.0")
            .unwrap_err();
        assert!(
            err.to_string().contains("not a Bookclerk platform"),
            "{err}"
        );
    }
}
