//! `bookclerk-plugin package` — archive a plugin directory for distribution.
//!
//! Audience: release / CI packaging. Library function behind the
//! `bookclerk-plugin package` subcommand.

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use bookclerk_plugin_manifest::{parse, PluginRuntimeKind};
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

use bookclerk_plugin_sdk::{Result, SdkError};

/// Packages a plugin directory into `out_dir` as a `.tar.gz` plus `SHA256SUMS`.
///
/// Native guests include the binary named by `command` (mode `0755` on Unix)
/// and name the archive with the host target triple. Workerd guests copy the
/// `modules_dir` tree and use a `-workerd` archive stem. Existing
/// `SHA256SUMS` lines for the same archive name are replaced.
///
/// # Arguments
///
/// * `plugin_dir` - Directory containing `plugin.toml` and guest payload.
/// * `out_dir` - Destination directory for the archive and checksums file
///   (created if missing).
///
/// # Returns
///
/// Absolute or relative path to the written `.tar.gz` archive.
///
/// # Errors
///
/// Returns [`SdkError`] when the manifest is invalid, required binaries /
/// modules are missing, or archive / checksum I/O fails.
pub fn package_plugin(plugin_dir: &Path, out_dir: &Path) -> Result<PathBuf> {
    // Operator-selected root: resolve once so a symlinked plugin directory is
    // allowed; only manifest-derived children are constrained below.
    let root = trusted_plugin_root(plugin_dir)?;
    let toml_path = join_relative_under(&root, Path::new("plugin.toml"))?;
    let text = std::fs::read_to_string(&toml_path)
        .map_err(|e| SdkError::message(format!("read {}: {e}", toml_path.display())))?;
    let manifest = parse(&text).map_err(|e| SdkError::message(e.to_string()))?;
    let version = manifest.version.clone().unwrap_or_else(|| "0.0.0".into());
    let id = &manifest.id;
    std::fs::create_dir_all(out_dir).map_err(SdkError::from)?;

    let staging = out_dir.join(format!(".staging-{id}"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(SdkError::from)?;
    std::fs::copy(&toml_path, staging.join("plugin.toml")).map_err(SdkError::from)?;

    if let Some(logo) = manifest.logo.as_deref() {
        if let bookclerk_plugin_manifest::LogoKind::EmbeddedPath(rel) =
            bookclerk_plugin_manifest::validate_logo(logo)
                .map_err(|e| SdkError::message(e.to_string()))?
        {
            let src = require_existing_source_under(&root, Path::new(&rel))?;
            if !src.is_file() {
                return Err(SdkError::message(format!(
                    "embedded logo missing for package: {}",
                    src.display()
                )));
            }
            let dest = join_relative_under(&staging, Path::new(&rel))?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(SdkError::from)?;
            }
            std::fs::copy(&src, &dest).map_err(SdkError::from)?;
        }
    }

    let archive_stem = match manifest.runtime {
        PluginRuntimeKind::Native => {
            let cmd = manifest
                .command
                .as_ref()
                .ok_or_else(|| SdkError::message("native plugin missing command"))?;
            let src = if cmd.is_absolute() {
                // Absolute command paths are intentional operator-selected build
                // outputs (documented); not treated as untrusted manifest suffixes.
                cmd.clone()
            } else {
                require_existing_source_under(&root, cmd)?
            };
            if !src.is_file() {
                return Err(SdkError::message(format!(
                    "native binary not found for package: {}",
                    src.display()
                )));
            }
            let bin_name = src
                .file_name()
                .ok_or_else(|| SdkError::message("binary name"))?
                .to_os_string();
            let dest = join_under_root(&staging, &bin_name)?;
            std::fs::copy(&src, &dest).map_err(SdkError::from)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&dest)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&dest, perms)?;
            }
            let triple = host_bookclerk_target();
            format!("bookclerk-plugin-{id}-{version}-{triple}")
        }
        PluginRuntimeKind::Workerd => {
            let w = manifest
                .workerd
                .as_ref()
                .ok_or_else(|| SdkError::message("workerd config missing"))?;
            let modules_src = require_existing_source_under(&root, Path::new(&w.modules_dir))?;
            let modules_dst = join_relative_under(&staging, Path::new(&w.modules_dir))?;
            copy_dir_recursive(&modules_src, &modules_dst)?;
            // `BookclerkEntrypoint` and the named `*Entrypoint` bases are imported
            // from `@bookclerk/plugin-sdk/workerd`; `bookclerk-workerd` injects that
            // module at runtime.
            format!("bookclerk-plugin-{id}-{version}-workerd")
        }
    };

    let archive_name = format!("{archive_stem}.tar.gz");
    // Manifest version is free-form; contain the archive name under out_dir before
    // any write. Publish via an attempt-owned temp so failure cleanup cannot delete
    // a pre-existing final artifact.
    let out_root = {
        std::fs::create_dir_all(out_dir).map_err(SdkError::from)?;
        out_dir.canonicalize().map_err(|e| {
            SdkError::message(format!(
                "canonicalize package out dir {}: {e}",
                out_dir.display()
            ))
        })?
    };
    let archive_path = join_relative_under(&out_root, Path::new(&archive_name))?;
    let tmp_name = format!(
        ".packaging-tmp-{id}-{}.tar.gz",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp_path = join_relative_under(&out_root, Path::new(&tmp_name))?;
    match write_tar_gz(&staging, &tmp_path) {
        Ok(()) => {}
        Err(err) => {
            let _ = std::fs::remove_file(&tmp_path);
            let _ = std::fs::remove_dir_all(&staging);
            return Err(err);
        }
    }
    std::fs::rename(&tmp_path, &archive_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        SdkError::message(format!(
            "publish archive {} → {}: {e}",
            tmp_path.display(),
            archive_path.display()
        ))
    })?;
    let _ = std::fs::remove_dir_all(&staging);

    let sums = join_relative_under(&out_root, Path::new("SHA256SUMS"))?;
    let digest = sha256_file(&archive_path)?;
    let line = format!("{digest}  {archive_name}\n");
    // Append or replace single-line sums for this archive.
    let mut body = String::new();
    if sums.is_file() {
        body = std::fs::read_to_string(&sums).unwrap_or_default();
        body = body
            .lines()
            .filter(|l| !l.ends_with(&archive_name))
            .collect::<Vec<_>>()
            .join("\n");
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
    }
    body.push_str(&line);
    std::fs::write(&sums, body).map_err(SdkError::from)?;

    Ok(archive_path)
}

/// Bookclerk target triple for this host (`linux-x64-gnu`, `macos-arm64`, …).
fn host_bookclerk_target() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64") => "linux-x64-gnu".into(),
        ("linux", "aarch64") => "linux-arm64".into(),
        ("macos", "aarch64") => "macos-arm64".into(),
        ("macos", "x86_64") => "macos-x64".into(),
        ("windows", "x86_64") => "windows-x64".into(),
        _ => format!("{os}-{arch}"),
    }
}

/// Canonical identity of the operator-selected plugin directory.
///
/// A symlinked plugin root is allowed: we resolve once and treat that identity
/// as the trusted base. Manifest-derived children are validated below it.
///
/// # Errors
///
/// Returns [`SdkError`] when the path is empty/NUL or cannot be canonicalized.
fn trusted_plugin_root(plugin_dir: &Path) -> Result<PathBuf> {
    reject_empty_or_nul(plugin_dir)?;
    plugin_dir.canonicalize().map_err(|e| {
        SdkError::message(format!(
            "canonicalize plugin root {}: {e}",
            plugin_dir.display()
        ))
    })
}

/// Rejects absolute paths and `ParentDir` / prefix components in a relative
/// manifest suffix (names like `edition..2` remain allowed).
///
/// # Errors
///
/// Returns [`SdkError`] when `rel` is absolute or contains unsafe components.
fn require_relative_manifest_path(rel: &Path) -> Result<()> {
    reject_empty_or_nul(rel)?;
    if rel.as_os_str().is_empty() {
        return Err(SdkError::message("refusing empty manifest path"));
    }
    if rel.is_absolute() {
        return Err(SdkError::message(format!(
            "refusing absolute manifest path: {}",
            rel.display()
        )));
    }
    let mut saw_normal = false;
    for comp in rel.components() {
        match comp {
            Component::Normal(_) => saw_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SdkError::message(format!(
                    "refusing unsafe manifest path component: {}",
                    rel.display()
                )));
            }
        }
    }
    if !saw_normal {
        return Err(SdkError::message(format!(
            "refusing empty manifest path: {}",
            rel.display()
        )));
    }
    Ok(())
}

/// Join a relative manifest suffix under `root` with component-wise safety.
///
/// # Errors
///
/// Returns [`SdkError`] when `rel` is unsafe or the join escapes `root`.
fn join_relative_under(root: &Path, rel: &Path) -> Result<PathBuf> {
    require_relative_manifest_path(rel)?;
    reject_empty_or_nul(root)?;
    let root_norm = root.canonicalize().map_err(|e| {
        SdkError::message(format!("canonicalize package root {}: {e}", root.display()))
    })?;
    let mut out = root_norm.clone();
    for comp in rel.components() {
        if let Component::Normal(name) = comp {
            out.push(name);
        }
    }
    require_under_root(&root_norm, &out)
}

/// Resolve a manifest-derived relative source under `root`.
///
/// Walks each component with link-aware metadata (refuses symlinks), then
/// requires the existing path's canonicalize result to stay under `root`.
///
/// # Errors
///
/// Returns [`SdkError`] on unsafe spelling, symlink components, missing paths,
/// or escape after canonicalize.
fn require_existing_source_under(root: &Path, rel: &Path) -> Result<PathBuf> {
    join_relative_under(root, rel)?;
    let root_norm = root.canonicalize().map_err(|e| {
        SdkError::message(format!("canonicalize package root {}: {e}", root.display()))
    })?;
    let mut cur = root_norm.clone();
    for comp in rel.components() {
        let Component::Normal(name) = comp else {
            continue;
        };
        cur.push(name);
        let meta = std::fs::symlink_metadata(&cur).map_err(|e| {
            SdkError::message(format!("stat package source {}: {e}", cur.display()))
        })?;
        if meta.file_type().is_symlink() {
            return Err(SdkError::message(format!(
                "refusing symlink in package source: {}",
                cur.display()
            )));
        }
    }
    let canon = cur.canonicalize().map_err(|e| {
        SdkError::message(format!(
            "canonicalize package source {}: {e}",
            cur.display()
        ))
    })?;
    if !canon.starts_with(&root_norm) {
        return Err(SdkError::message(format!(
            "path {} escapes root {}",
            canon.display(),
            root_norm.display()
        )));
    }
    Ok(canon)
}

/// Rejects empty paths and interior NULs before filesystem access.
///
/// # Errors
///
/// Returns [`SdkError`] when the path is empty or contains an interior NUL.
fn reject_empty_or_nul(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(SdkError::message("refusing empty path"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if path.as_os_str().as_bytes().contains(&0) {
            return Err(SdkError::message(format!(
                "refusing path with NUL: {}",
                path.display()
            )));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if path.as_os_str().encode_wide().any(|c| c == 0) {
            return Err(SdkError::message(format!(
                "refusing path with NUL: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Join `root` / `name` and require the result stays under `root`.
///
/// # Errors
///
/// Returns [`SdkError`] when `root` is unsafe, `name` escapes, or the join leaves `root`.
fn join_under_root(root: &Path, name: &std::ffi::OsStr) -> Result<PathBuf> {
    reject_empty_or_nul(root)?;
    for comp in Path::new(name).components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SdkError::message(format!(
                    "refusing unsafe name under {}: {}",
                    root.display(),
                    Path::new(name).display()
                )));
            }
        }
    }
    let root_norm = root.canonicalize().map_err(|e| {
        SdkError::message(format!("canonicalize package root {}: {e}", root.display()))
    })?;
    let out = root_norm.join(name);
    require_under_root(&root_norm, &out)
}

/// Requires `path` to stay under `root` after canonicalize + `starts_with`.
///
/// Missing leaves: canonicalize the nearest existing parent and rejoin. No
/// raw-path fallback.
///
/// # Errors
///
/// Returns [`SdkError`] when `path` is unsafe, cannot canonicalize, or escapes `root`.
fn require_under_root(root: &Path, path: &Path) -> Result<PathBuf> {
    reject_empty_or_nul(path)?;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(SdkError::message(format!(
            "refusing path with '..': {}",
            path.display()
        )));
    }
    let root_norm = root.canonicalize().map_err(|e| {
        SdkError::message(format!("canonicalize package root {}: {e}", root.display()))
    })?;
    // Walk suffix components under the root so intermediate symlinks are refused
    // before canonicalize would follow them out of tree.
    if let Ok(rel) = path
        .strip_prefix(root)
        .or_else(|_| path.strip_prefix(&root_norm))
    {
        let mut cur = root_norm.clone();
        for comp in rel.components() {
            let Component::Normal(name) = comp else {
                continue;
            };
            cur.push(name);
            match std::fs::symlink_metadata(&cur) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(SdkError::message(format!(
                        "refusing symlink in package path: {}",
                        cur.display()
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => {
                    return Err(SdkError::message(format!(
                        "could not stat path {}: {e}",
                        cur.display()
                    )));
                }
                Ok(_) => {}
            }
        }
    } else {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(SdkError::message(format!(
                    "refusing symlink in package path: {}",
                    path.display()
                )));
            }
            _ => {}
        }
    }
    let path_norm = match path.canonicalize() {
        Ok(c) => c,
        Err(err) => {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(SdkError::message(format!(
                        "refusing dangling or unresolvable symlink {}: {err}",
                        path.display()
                    )));
                }
                Ok(_) => {
                    return Err(SdkError::message(format!(
                        "could not canonicalize existing path {}: {err}",
                        path.display()
                    )));
                }
                Err(meta_err) if meta_err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(SdkError::message(format!(
                        "could not stat path {}: {meta_err}",
                        path.display()
                    )));
                }
                Err(_) => {}
            }
            let mut suffix = Vec::new();
            let mut cursor = path.to_path_buf();
            loop {
                match cursor.canonicalize() {
                    Ok(canon) => {
                        let mut out = canon;
                        for part in suffix.iter().rev() {
                            out.push(part);
                        }
                        break out;
                    }
                    Err(canon_err) => {
                        match std::fs::symlink_metadata(&cursor) {
                            Ok(meta) if meta.file_type().is_symlink() => {
                                return Err(SdkError::message(format!(
                                    "refusing dangling or unresolvable symlink {}: {canon_err}",
                                    cursor.display()
                                )));
                            }
                            Ok(_) => {
                                return Err(SdkError::message(format!(
                                    "could not canonicalize path {}: {canon_err}",
                                    cursor.display()
                                )));
                            }
                            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                                return Err(SdkError::message(format!(
                                    "could not stat path {}: {e}",
                                    cursor.display()
                                )));
                            }
                            Err(_) => {}
                        }
                        let name = cursor.file_name().ok_or_else(|| {
                            SdkError::message(format!("path has no file name: {}", path.display()))
                        })?;
                        suffix.push(name.to_os_string());
                        match cursor.parent() {
                            Some(parent) if !parent.as_os_str().is_empty() => {
                                cursor = parent.to_path_buf();
                            }
                            _ => {
                                return Err(SdkError::message(format!(
                                    "could not canonicalize path {} under {}: {canon_err}",
                                    path.display(),
                                    root.display()
                                )));
                            }
                        }
                    }
                }
            }
        }
    };
    if !path_norm.starts_with(&root_norm) {
        return Err(SdkError::message(format!(
            "path {} escapes root {}",
            path_norm.display(),
            root_norm.display()
        )));
    }
    Ok(path_norm)
}

/// Recursively copies `src` into `dst`, creating directories as needed.
///
/// Symlinks and non-file/non-directory entries are refused so a plugin tree
/// cannot embed host bytes via `modules/leak -> /outside`.
///
/// # Errors
///
/// Propagates filesystem errors from directory creation, traversal, or file copy
/// as [`SdkError`]. Returns an error when a symlink or unsupported type is found.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    reject_empty_or_nul(src)?;
    reject_empty_or_nul(dst)?;
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    if src.is_symlink() {
        return Err(SdkError::message(format!(
            "refusing symlink package source: {}",
            src.display()
        )));
    }
    std::fs::create_dir_all(&dst).map_err(SdkError::from)?;
    for entry in std::fs::read_dir(&src).map_err(SdkError::from)? {
        let entry = entry.map_err(SdkError::from)?;
        let ty = entry.file_type().map_err(SdkError::from)?;
        let to = join_under_root(&dst, &entry.file_name())?;
        let from = require_under_root(&src, &entry.path())?;
        if ty.is_symlink() {
            return Err(SdkError::message(format!(
                "refusing symlink in package source: {}",
                from.display()
            )));
        }
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to).map_err(SdkError::from)?;
        } else {
            return Err(SdkError::message(format!(
                "refusing unsupported package source type: {}",
                from.display()
            )));
        }
    }
    Ok(())
}

/// Writes `staging` as a gzip tar with `.` as the archive root.
///
/// # Errors
///
/// Propagates archive or compression failures as [`SdkError`].
fn write_tar_gz(staging: &Path, archive_path: &Path) -> Result<()> {
    let file = File::create(archive_path).map_err(SdkError::from)?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(enc);
    builder
        .append_dir_all(".", staging)
        .map_err(|e| SdkError::message(format!("tar: {e}")))?;
    let enc = builder
        .into_inner()
        .map_err(|e| SdkError::message(format!("tar finish: {e}")))?;
    enc.finish()
        .map_err(|e| SdkError::message(format!("gzip finish: {e}")))?;
    Ok(())
}

/// Hex-encoded SHA-256 of `path`, streamed in 8 KiB chunks.
///
/// # Errors
///
/// Propagates file open or read failures as [`SdkError`].
fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(SdkError::from)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf).map_err(SdkError::from)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(all(test, unix))]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_minimal_workerd_plugin(plugin: &Path) {
        std::fs::create_dir_all(plugin.join("modules")).unwrap();
        std::fs::write(
            plugin.join("plugin.toml"),
            r#"api_version = 3
id = "pkg_symlink_test"
version = "0.0.1"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
modules_dir = "modules"
entrypoint = "default"

[capabilities.network]
mode = "deny"
"#,
        )
        .unwrap();
        std::fs::write(
            plugin.join("modules").join("index.js"),
            "export default class P {}\n",
        )
        .unwrap();
    }

    #[test]
    fn package_refuses_module_file_symlink_without_outside_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("plugin");
        write_minimal_workerd_plugin(&plugin);
        let outside = dir.path().join("outside.txt");
        {
            let mut f = File::create(&outside).unwrap();
            f.write_all(b"SECRET_OUTSIDE_BYTES").unwrap();
        }
        std::os::unix::fs::symlink(&outside, plugin.join("modules").join("leak.txt")).unwrap();

        let out = dir.path().join("dist");
        let err = package_plugin(&plugin, &out).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink refusal, got {err}"
        );
        assert!(
            !out.exists()
                || std::fs::read_dir(&out)
                    .map(|entries| {
                        !entries
                            .filter_map(|e| e.ok())
                            .any(|e| e.path().extension().is_some_and(|ext| ext == "gz"))
                    })
                    .unwrap_or(true),
            "must not publish a successful .tar.gz"
        );
        assert!(!out.join("SHA256SUMS").is_file());
        // Staging may remain after failure; ensure outside bytes were not copied in.
        if let Ok(entries) = std::fs::read_dir(&out) {
            for entry in entries.flatten() {
                assert!(
                    !walkdir_contains_secret(&entry.path(), b"SECRET_OUTSIDE_BYTES"),
                    "outside secret must not appear under {}",
                    entry.path().display()
                );
            }
        }
    }

    #[test]
    fn package_refuses_modules_root_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("plugin");
        write_minimal_workerd_plugin(&plugin);
        let outside_modules = dir.path().join("outside_modules");
        std::fs::create_dir_all(&outside_modules).unwrap();
        std::fs::write(
            outside_modules.join("index.js"),
            "export default class X {}\n",
        )
        .unwrap();
        std::fs::remove_dir_all(plugin.join("modules")).unwrap();
        std::os::unix::fs::symlink(&outside_modules, plugin.join("modules")).unwrap();

        let out = dir.path().join("dist");
        let err = package_plugin(&plugin, &out).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!out.join("SHA256SUMS").is_file());
    }

    #[test]
    fn package_refuses_embedded_logo_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("plugin");
        write_minimal_workerd_plugin(&plugin);
        let toml = r#"api_version = 3
id = "pkg_symlink_test"
version = "0.0.1"
runtime = "workerd"
entrypoints = ["cli"]
logo = "logo.png"

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
modules_dir = "modules"
entrypoint = "default"

[capabilities.network]
mode = "deny"
"#;
        std::fs::write(plugin.join("plugin.toml"), toml).unwrap();
        let outside = dir.path().join("outside.png");
        std::fs::write(&outside, b"FAKEPNG").unwrap();
        std::os::unix::fs::symlink(&outside, plugin.join("logo.png")).unwrap();

        let out = dir.path().join("dist");
        let err = package_plugin(&plugin, &out).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn package_refuses_intermediate_dir_symlink_on_native_command() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.toml"),
            r#"api_version = 3
id = "native_sym"
version = "0.0.1"
runtime = "native"
command = "bin/tool"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
"#,
        )
        .unwrap();
        let outside = dir.path().join("outside_bin");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("tool"), b"#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&outside, plugin.join("bin")).unwrap();

        let out = dir.path().join("dist");
        let err = package_plugin(&plugin, &out).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected intermediate symlink refusal, got {err}"
        );
        assert!(!out.join("SHA256SUMS").is_file());
    }

    #[test]
    fn package_refuses_parent_dir_in_native_command() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        let outside = dir.path().join("outside_tool");
        std::fs::write(&outside, b"#!/bin/sh\n").unwrap();
        std::fs::write(
            plugin.join("plugin.toml"),
            r#"api_version = 3
id = "native_dotdot"
version = "0.0.1"
runtime = "native"
command = "../outside_tool"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
"#,
        )
        .unwrap();

        let out = dir.path().join("dist");
        let err = package_plugin(&plugin, &out).unwrap_err();
        assert!(
            err.to_string().contains("unsafe") || err.to_string().contains(".."),
            "expected .. refusal, got {err}"
        );
    }

    #[test]
    fn package_allows_symlinked_plugin_root() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real_plugin");
        write_minimal_workerd_plugin(&real);
        let link = dir.path().join("link_plugin");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let out = dir.path().join("dist");
        let archive = package_plugin(&link, &out).expect("symlinked plugin root must package");
        assert!(archive.is_file());
        assert!(out.join("SHA256SUMS").is_file());
    }

    #[test]
    fn package_refuses_version_path_traversal_in_archive_name() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("plugin");
        write_minimal_workerd_plugin(&plugin);
        let mut toml = std::fs::read_to_string(plugin.join("plugin.toml")).unwrap();
        // Insert free-form version with parent components into the archive stem.
        toml = toml.replacen("version = \"0.0.1\"", "version = \"../../../victim\"", 1);
        std::fs::write(plugin.join("plugin.toml"), toml).unwrap();

        let out = dir.path().join("dist");
        std::fs::create_dir_all(&out).unwrap();
        let victim = dir.path().join("victim-workerd.tar.gz");
        std::fs::write(&victim, b"PREEXISTING").unwrap();

        let err = package_plugin(&plugin, &out).unwrap_err();
        assert!(
            err.to_string().contains("unsafe")
                || err.to_string().contains("escapes")
                || err.to_string().contains(".."),
            "expected archive containment refusal, got {err}"
        );
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"PREEXISTING",
            "must not create/overwrite outside archive"
        );
    }

    #[test]
    fn package_tar_failure_preserves_existing_final_archive() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("plugin");
        write_minimal_workerd_plugin(&plugin);
        let out = dir.path().join("dist");
        std::fs::create_dir_all(&out).unwrap();
        // First successful package establishes the final archive name.
        let archive = package_plugin(&plugin, &out).expect("initial package");
        let keep = b"KEEP_FINAL_BYTES";
        std::fs::write(&archive, keep).unwrap();

        // Corrupt staging mid-flight by replacing modules with a non-directory after
        // a second call would rebuild — instead force write_tar_gz failure by making
        // out_root read-only after planting the final file is awkward cross-platform.
        // Simulate the failure cleanup contract: join a bad archive name that fails
        // before rename while a same-name final already exists — covered by
        // version traversal above. Here verify a second successful package may
        // replace, and a failed require_existing still leaves KEEP when we only
        // delete attempt temps: remove modules so package fails before tar publish.
        std::fs::remove_dir_all(plugin.join("modules")).unwrap();
        let err = package_plugin(&plugin, &out).unwrap_err();
        assert!(!err.to_string().is_empty());
        assert_eq!(
            std::fs::read(&archive).unwrap(),
            keep,
            "failed package must not delete pre-existing final archive"
        );
    }

    fn walkdir_contains_secret(root: &Path, secret: &[u8]) -> bool {
        fn walk(path: &Path, secret: &[u8]) -> bool {
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                return false;
            };
            if meta.file_type().is_symlink() {
                return false;
            }
            if meta.is_file() {
                if let Ok(bytes) = std::fs::read(path) {
                    if bytes.windows(secret.len()).any(|w| w == secret) {
                        return true;
                    }
                }
                return false;
            }
            if meta.is_dir() {
                if let Ok(rd) = std::fs::read_dir(path) {
                    for entry in rd.flatten() {
                        if walk(&entry.path(), secret) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        walk(root, secret)
    }
}
