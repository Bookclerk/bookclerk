//! Deterministic immutable payload identity for an installed plugin tree.
//!
//! [`payload_root_sha256`] hashes packaged files. Host-owned install receipts
//! (`receipt.json` and its temp/backup names) are excluded. Mutable plugin
//! state lives outside the install tree under `plugin-state/`.
//!
//! The digest proves: these installed bytes match the artifact recorded for this
//! provenance-qualified package. It does **not** prove publisher identity.

use std::fs;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{CatalogError, Result};
use crate::extract::sha256_file;

/// Directory / file names excluded from the immutable payload (host-owned
/// receipts only). Packaged `data/` and `tmp/` are hashed: mutable plugin
/// state lives under `$FILES_DIR/plugin-state/<PluginKey fs-id>/`, not inside
/// the install tree.
pub const PAYLOAD_SKIP_NAMES: &[&str] = &["receipt.json", "receipt.json.bak", "receipt.json.tmp"];

/// SHA-256 of `plugin.toml` at `plugin_root`.
///
/// # Errors
///
/// Returns an error when the file cannot be read.
pub fn manifest_sha256(plugin_root: &Path) -> Result<String> {
    sha256_file(&plugin_root.join("plugin.toml"))
}

/// Deterministic payload root: sorted relative path + type + content hash.
///
/// Encoding (UTF-8, `\n`-terminated records, paths use `/`):
///
/// - file: `f\t{rel}\t{sha256}`
/// - directory: `d\t{rel}` (children listed separately)
///
/// Symlinks and path traversal (`..`) are rejected. The root directory itself
/// is not listed; an empty tree hashes the empty byte string.
///
/// # Errors
///
/// Returns an error on I/O failure, traversal, or a symlink in the payload.
pub fn payload_root_sha256(plugin_root: &Path) -> Result<String> {
    let mut records = Vec::new();
    collect_payload_records(plugin_root, plugin_root, &mut records)?;
    records.sort();
    let mut hasher = Sha256::new();
    for rec in &records {
        hasher.update(rec.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Collects sorted payload records under `dir` (relative to `root`).
fn collect_payload_records(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| CatalogError::message(format!("read {}: {e}", dir.display())))?
        .map(|e| {
            e.map(|e| e.path())
                .map_err(|err| CatalogError::message(err.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            CatalogError::message(format!("non-UTF-8 payload path {}", path.display()))
        })?;
        if dir == root && PAYLOAD_SKIP_NAMES.contains(&name) {
            continue;
        }
        let rel = path.strip_prefix(root).map_err(|_| {
            CatalogError::message(format!(
                "payload path {} escaped {}",
                path.display(),
                root.display()
            ))
        })?;
        let rel_str = normalize_rel(rel)?;
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(CatalogError::message(format!(
                "refusing symlink in plugin payload: {rel_str}"
            )));
        }
        if meta.is_dir() {
            if !rel_str.is_empty() {
                out.push(format!("d\t{rel_str}"));
            }
            collect_payload_records(root, &path, out)?;
        } else if meta.is_file() {
            let digest = sha256_file(&path)?;
            out.push(format!("f\t{rel_str}\t{digest}"));
        } else {
            return Err(CatalogError::message(format!(
                "refusing special file in plugin payload: {rel_str}"
            )));
        }
    }
    Ok(())
}

/// Normalizes a relative payload path to `/`-separated UTF-8 without traversal.
fn normalize_rel(rel: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => {
                let s = c.to_str().ok_or_else(|| {
                    CatalogError::message("non-UTF-8 path component in plugin payload")
                })?;
                if s == ".." || s.contains('\0') || s.contains('/') || s.contains('\\') {
                    return Err(CatalogError::message(format!(
                        "refusing ambiguous payload path {}",
                        rel.display()
                    )));
                }
                parts.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CatalogError::message(format!(
                    "refusing traversal/absolute payload path {}",
                    rel.display()
                )));
            }
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::write_file;

    #[test]
    fn payload_hash_is_order_independent_and_skips_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(&root.join("plugin.toml"), b"id = \"echo\"\n").unwrap();
        write_file(&root.join("bin/guest"), b"#!/bin/sh\n").unwrap();
        write_file(&root.join("data/secret"), b"nope").unwrap();
        write_file(&root.join("tmp/scratch"), b"nope").unwrap();
        write_file(&root.join("receipt.json"), b"{}").unwrap();

        let a = payload_root_sha256(root).unwrap();
        let manifest = manifest_sha256(root).unwrap();
        assert_eq!(manifest.len(), 64);

        // Reverse-create equivalent tree, including formerly skipped packaged paths.
        let dir2 = tempfile::tempdir().unwrap();
        let root2 = dir2.path();
        write_file(&root2.join("bin/guest"), b"#!/bin/sh\n").unwrap();
        write_file(&root2.join("plugin.toml"), b"id = \"echo\"\n").unwrap();
        write_file(&root2.join("data/secret"), b"nope").unwrap();
        write_file(&root2.join("tmp/scratch"), b"nope").unwrap();
        let b = payload_root_sha256(root2).unwrap();
        assert_eq!(a, b);

        // Mutating a packaged file under data/ changes identity; receipt does not.
        write_file(&root.join("receipt.json"), b"{changed}").unwrap();
        assert_eq!(a, payload_root_sha256(root).unwrap());
        write_file(&root.join("data/secret"), b"changed").unwrap();
        assert_ne!(a, payload_root_sha256(root).unwrap());
    }

    #[test]
    fn payload_hash_covers_packaged_data_and_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(&root.join("plugin.toml"), b"id = \"echo\"\n").unwrap();
        write_file(&root.join("data/keep"), b"one").unwrap();
        let before = payload_root_sha256(root).unwrap();
        write_file(&root.join("data/keep"), b"two").unwrap();
        assert_ne!(before, payload_root_sha256(root).unwrap());
        write_file(&root.join("tmp/scratch"), b"x").unwrap();
        assert_ne!(before, payload_root_sha256(root).unwrap());
    }

    #[test]
    fn payload_hash_rejects_dotdot_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("plugin.toml"), b"x").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", dir.path().join("link")).unwrap();
            let err = payload_root_sha256(dir.path()).unwrap_err();
            assert!(err.to_string().contains("symlink"), "{err}");
        }
        let err = normalize_rel(Path::new("../evil")).unwrap_err();
        assert!(err.to_string().contains("traversal") || err.to_string().contains("refusing"));
    }
}
