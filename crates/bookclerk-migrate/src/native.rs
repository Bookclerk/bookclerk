//! Native Bookclerk portable backup (`.tar.gz` archive).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use crate::error::{MigrateError, Result};

/// Wraps an operator-facing message as [`MigrateError::Source`].
fn err(msg: impl Into<String>) -> MigrateError {
    MigrateError::Source(msg.into())
}

/// Current native backup format version.
pub const NATIVE_BACKUP_FORMAT_VERSION: u32 = 1;

/// Manifest stored at `manifest.json` inside the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeBackupManifest {
    /// Archive format version written at export; importers warn when newer than supported.
    pub format_version: u32,
    /// RFC 3339 UTC timestamp when the archive was created.
    pub created_at: String,
    /// Bookclerk version string recorded for support / compatibility notes.
    pub bookclerk_version: String,
    /// Logical paths packaged (`config.toml`, `library.db`, optional `plugins/**/plugin.toml`).
    pub included: Vec<String>,
}

/// Options for exporting a native backup.
#[derive(Debug, Clone)]
pub struct NativeExportOptions {
    /// Bookclerk or Libation files directory root for this operation.
    pub files_dir: PathBuf,
    /// Destination archive path. Absolute, or relative to the process cwd.
    /// This is an operator choice and is not jailed under [`Self::files_dir`].
    pub dest: PathBuf,
    /// Bookclerk version string recorded in the backup manifest.
    pub bookclerk_version: String,
    /// Include `plugins/**/plugin.toml` (not plugin binaries).
    pub include_plugin_manifests: bool,
    /// Include `cache/` (large; off by default).
    pub include_cache: bool,
    /// Include `logs/` (off by default).
    pub include_logs: bool,
    /// Include `plugin-databases/` (SQLite binding files; plugin DDL is not migrated).
    pub include_plugin_databases: bool,
}

/// Options for importing a native backup.
#[derive(Debug, Clone)]
pub struct NativeImportOptions {
    /// Path to the native backup `.tar.zst` (or similar) archive.
    pub archive: PathBuf,
    /// Destination Bookclerk files directory for import.
    pub dest_files_dir: PathBuf,
    /// When true, overwrite existing data instead of failing on conflict.
    pub force: bool,
    /// When true, report what would change without writing files.
    pub dry_run: bool,
}

/// Summary of a native export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeExportSummary {
    /// Path to the native backup `.tar.zst` (or similar) archive.
    pub archive: String,
    /// Count of files packaged or restored.
    pub files: usize,
    /// Logical paths included in the archive.
    pub included: Vec<String>,
}

/// Summary of a native import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeImportSummary {
    /// Count of files packaged or restored.
    pub files: usize,
    /// Native backup format version actually read from the archive.
    pub format_version: u32,
    /// Non-fatal warnings collected during the run (operator-facing).
    pub warnings: Vec<String>,
}

/// Export Bookclerk files-dir essentials into a `.tar.gz` archive.
///
/// # Arguments
///
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `NativeExportSummary` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn export_native(opts: NativeExportOptions) -> Result<NativeExportSummary> {
    // `files_dir` contains collected entries. `dest` is the operator-selected
    // archive location (absolute, or cwd-relative) and is not joined onto it.
    let files_root = std::fs::canonicalize(&opts.files_dir).map_err(|source| {
        err(format!(
            "could not canonicalize files dir {}: {source}",
            opts.files_dir.display()
        ))
    })?;
    let dest = if opts.dest.is_absolute() {
        opts.dest.clone()
    } else {
        std::env::current_dir()
            .map_err(|source| err(format!("current directory: {source}")))?
            .join(&opts.dest)
    };
    reject_empty_or_nul(&dest)?;
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|source| err(format!("create {}: {source}", parent.display())))?;
        }
    }
    let file = File::create(&dest)
        .map_err(|source| err(format!("create {}: {source}", dest.display())))?;
    let enc = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut builder = tar::Builder::new(enc);

    let mut included = Vec::new();
    let mut file_count = 0usize;
    let mut entries: Vec<(String, PathBuf)> = Vec::new();

    push_if_exists(&files_root, "config.toml", &mut entries, &mut included)?;
    push_if_exists(&files_root, "library.db", &mut entries, &mut included)?;

    if opts.include_plugin_manifests {
        let plugins = files_root.join("plugins");
        collect_plugin_tomls(&files_root, &plugins, &mut entries, &mut included)?;
    }
    if opts.include_cache {
        let cache = files_root.join("cache");
        collect_dir(
            &files_root,
            &cache,
            "cache",
            &mut entries,
            &mut included,
            true,
        )?;
    }
    if opts.include_logs {
        let logs = files_root.join("logs");
        collect_dir(
            &files_root,
            &logs,
            "logs",
            &mut entries,
            &mut included,
            true,
        )?;
    }
    if opts.include_plugin_databases {
        let plugin_databases = files_root.join("plugin-databases");
        collect_dir(
            &files_root,
            &plugin_databases,
            "plugin-databases",
            &mut entries,
            &mut included,
            true,
        )?;
    }

    let manifest = NativeBackupManifest {
        format_version: NATIVE_BACKUP_FORMAT_VERSION,
        created_at: chrono::Utc::now().to_rfc3339(),
        bookclerk_version: opts.bookclerk_version,
        included: included.clone(),
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| err(format!("manifest json: {e}")))?;
    {
        let mut header = tar::Header::new_gnu();
        header.set_path("manifest.json")?;
        header.set_size(manifest_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, manifest_bytes.as_slice())?;
        file_count += 1;
    }

    for (arc_name, path) in entries {
        let mut f = File::open(&path)
            .map_err(|source| err(format!("open {}: {source}", path.display())))?;
        let mut header = tar::Header::new_gnu();
        header.set_path(&arc_name)?;
        let meta = f.metadata()?;
        header.set_metadata(&meta);
        header.set_cksum();
        builder.append(&header, &mut f)?;
        file_count += 1;
    }

    let enc = builder
        .into_inner()
        .map_err(|e| err(format!("tar finish: {e}")))?;
    enc.finish().map_err(|e| err(format!("gzip finish: {e}")))?;

    Ok(NativeExportSummary {
        archive: dest.display().to_string(),
        files: file_count,
        included,
    })
}

/// Restore a native backup archive into `dest_files_dir`.
///
/// # Arguments
///
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `NativeImportSummary` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn import_native(opts: NativeImportOptions) -> Result<NativeImportSummary> {
    let file = File::open(&opts.archive)
        .map_err(|source| err(format!("open {}: {source}", opts.archive.display())))?;
    let dec = GzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(dec);

    let mut summary = NativeImportSummary::default();
    let mut manifest: Option<NativeBackupManifest> = None;

    if !opts.dry_run {
        std::fs::create_dir_all(&opts.dest_files_dir)?;
    }

    for entry in archive
        .entries()
        .map_err(|e| err(format!("tar entries: {e}")))?
    {
        let mut entry = entry.map_err(|e| err(format!("tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| err(format!("tar path: {e}")))?
            .to_path_buf();
        let name = path.to_string_lossy().to_string();
        if !is_safe_archive_path(&path) {
            summary
                .warnings
                .push(format!("skipped unsafe path `{name}`"));
            continue;
        }

        if name == "manifest.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            match serde_json::from_slice::<NativeBackupManifest>(&buf) {
                Ok(m) => {
                    summary.format_version = m.format_version;
                    if m.format_version > NATIVE_BACKUP_FORMAT_VERSION {
                        summary.warnings.push(format!(
                            "backup format_version {} newer than supported {}",
                            m.format_version, NATIVE_BACKUP_FORMAT_VERSION
                        ));
                    }
                    manifest = Some(m);
                }
                Err(e) => summary.warnings.push(format!("invalid manifest.json: {e}")),
            }
            summary.files += 1;
            continue;
        }

        let dest = opts.dest_files_dir.join(&path);
        if dest.exists() && !opts.force {
            summary.warnings.push(format!(
                "skip existing {} (pass --force to overwrite)",
                dest.display()
            ));
            continue;
        }
        if opts.dry_run {
            summary.files += 1;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&dest)
            .map_err(|source| err(format!("create {}: {source}", dest.display())))?;
        std::io::copy(&mut entry, &mut out)?;
        summary.files += 1;
    }

    if manifest.is_none() {
        summary
            .warnings
            .push("manifest.json missing from archive".into());
    }
    Ok(summary)
}

/// Reject absolute paths and `..` / prefix components before joining into dest.
#[must_use]
fn is_safe_archive_path(path: &Path) -> bool {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return false;
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Rejects empty paths and interior NULs before filesystem access.
fn reject_empty_or_nul(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(err("refusing empty path"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if path.as_os_str().as_bytes().contains(&0) {
            return Err(err(format!("refusing path with NUL: {}", path.display())));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if path.as_os_str().encode_wide().any(|c| c == 0) {
            return Err(err(format!("refusing path with NUL: {}", path.display())));
        }
    }
    Ok(())
}

/// Requires `path` to stay under `root` after canonicalize + `starts_with`.
///
/// Missing leaves: canonicalize the nearest existing parent and rejoin. No
/// raw-path fallback.
fn require_under_walk_root(root: &Path, path: &Path) -> Result<PathBuf> {
    reject_empty_or_nul(root)?;
    reject_empty_or_nul(path)?;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(err(format!("refusing path with '..': {}", path.display())));
    }
    let root_norm = std::fs::canonicalize(root).map_err(|source| {
        err(format!(
            "could not canonicalize walk root {}: {source}",
            root.display()
        ))
    })?;
    let path_norm = match std::fs::canonicalize(path) {
        Ok(c) => c,
        Err(err_canon) => {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(err(format!(
                        "refusing dangling or unresolvable symlink {}: {err_canon}",
                        path.display()
                    )));
                }
                Ok(_) => {
                    return Err(err(format!(
                        "could not canonicalize existing path {}: {err_canon}",
                        path.display()
                    )));
                }
                Err(meta_err) if meta_err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(err(format!(
                        "could not stat path {}: {meta_err}",
                        path.display()
                    )));
                }
                Err(_) => {}
            }
            let mut suffix = Vec::new();
            let mut cursor = path.to_path_buf();
            loop {
                match std::fs::canonicalize(&cursor) {
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
                                return Err(err(format!(
                                    "refusing dangling or unresolvable symlink {}: {canon_err}",
                                    cursor.display()
                                )));
                            }
                            Ok(_) => {
                                return Err(err(format!(
                                    "could not canonicalize path {}: {canon_err}",
                                    cursor.display()
                                )));
                            }
                            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                                return Err(err(format!(
                                    "could not stat path {}: {e}",
                                    cursor.display()
                                )));
                            }
                            Err(_) => {}
                        }
                        let name = cursor
                            .file_name()
                            .ok_or_else(|| {
                                err(format!("path has no file name: {}", path.display()))
                            })?
                            .to_os_string();
                        suffix.push(name);
                        match cursor.parent() {
                            Some(parent) if !parent.as_os_str().is_empty() => {
                                cursor = parent.to_path_buf();
                            }
                            _ => {
                                return Err(err(format!(
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
        return Err(err(format!(
            "path {} escapes walk root {}",
            path_norm.display(),
            root_norm.display()
        )));
    }
    Ok(path_norm)
}

/// Adds `rel` to the export list when that file exists under the files dir.
fn push_if_exists(
    root: &Path,
    rel: &str,
    entries: &mut Vec<(String, PathBuf)>,
    included: &mut Vec<String>,
) -> Result<()> {
    reject_empty_or_nul(root)?;
    let path = root.join(rel);
    require_under_walk_root(root, &path)?;
    if path.is_file() {
        entries.push((rel.to_string(), path));
        included.push(rel.to_string());
    }
    Ok(())
}

/// Walks a directory into archive entries (`cache/`, `logs/`); missing dirs are skipped.
///
/// A genuinely missing optional root is skipped before canonicalization. A
/// dangling symlink is not "missing" and is rejected by [`require_under_walk_root`].
/// Symlink directory entries (including a symlinked optional root) are refused
/// before canonicalize so a self-loop cannot re-enter and inflate the walk.
/// `walk_root` is the existing files directory, not the optional child.
fn collect_dir(
    walk_root: &Path,
    dir: &Path,
    arc_prefix: &str,
    entries: &mut Vec<(String, PathBuf)>,
    included: &mut Vec<String>,
    recursive: bool,
) -> Result<()> {
    if optional_export_dir_absent(dir)? {
        return Ok(());
    }
    let dir_meta = std::fs::symlink_metadata(dir)
        .map_err(|source| err(format!("stat export dir {}: {source}", dir.display())))?;
    if dir_meta.file_type().is_symlink() {
        return Err(err(format!(
            "refusing symlink export directory: {}",
            dir.display()
        )));
    }
    let dir_canon = require_under_walk_root(walk_root, dir)?;
    if !dir_meta.is_dir() {
        return Ok(());
    }
    included.push(format!("{arc_prefix}/"));
    for entry in std::fs::read_dir(&dir_canon)? {
        let entry = entry?;
        let entry_path = entry.path();
        let file_type = entry.file_type().map_err(|source| {
            err(format!(
                "stat export entry {}: {source}",
                entry_path.display()
            ))
        })?;
        if file_type.is_symlink() {
            return Err(err(format!(
                "refusing symlink export entry: {}",
                entry_path.display()
            )));
        }
        let path = require_under_walk_root(walk_root, &entry_path)?;
        let name = entry.file_name();
        let arc_name = format!("{arc_prefix}/{}", name.to_string_lossy());
        if file_type.is_file() {
            entries.push((arc_name, path));
        } else if recursive && file_type.is_dir() {
            // Recurse on the directory entry path (not a resolved alias).
            collect_dir(walk_root, &entry_path, &arc_name, entries, included, true)?;
        }
    }
    Ok(())
}

/// True when `path` does not exist. Symlinks, including dangling ones, are present.
fn optional_export_dir_absent(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(source) => Err(err(format!("stat {}: {source}", path.display()))),
    }
}

/// Collects `plugins/**/plugin.toml` only (binaries stay out of the portable backup).
///
/// A missing `plugins/` directory is skipped. `files_root` is the containment root.
fn collect_plugin_tomls(
    files_root: &Path,
    plugins_root: &Path,
    entries: &mut Vec<(String, PathBuf)>,
    included: &mut Vec<String>,
) -> Result<()> {
    if optional_export_dir_absent(plugins_root)? {
        return Ok(());
    }
    let plugins_meta = std::fs::symlink_metadata(plugins_root).map_err(|source| {
        err(format!(
            "stat plugins dir {}: {source}",
            plugins_root.display()
        ))
    })?;
    if plugins_meta.file_type().is_symlink() {
        return Err(err(format!(
            "refusing symlink plugins directory: {}",
            plugins_root.display()
        )));
    }
    let plugins_root = require_under_walk_root(files_root, plugins_root)?;
    if !plugins_meta.is_dir() {
        return Ok(());
    }
    included.push("plugins/**/plugin.toml".into());
    let root_toml =
        require_under_walk_root(plugins_root.as_path(), &plugins_root.join("plugin.toml"))?;
    if root_toml.is_file() {
        entries.push(("plugins/plugin.toml".into(), root_toml));
    }
    for entry in std::fs::read_dir(&plugins_root)? {
        let entry = entry?;
        let entry_path = entry.path();
        let file_type = entry.file_type().map_err(|source| {
            err(format!(
                "stat plugin entry {}: {source}",
                entry_path.display()
            ))
        })?;
        if file_type.is_symlink() {
            return Err(err(format!(
                "refusing symlink plugin entry: {}",
                entry_path.display()
            )));
        }
        if !file_type.is_dir() {
            continue;
        }
        let path = require_under_walk_root(plugins_root.as_path(), &entry_path)?;
        let toml = require_under_walk_root(plugins_root.as_path(), &path.join("plugin.toml"))?;
        if toml.is_file() {
            let name = entry.file_name();
            entries.push((
                format!("plugins/{}/plugin.toml", name.to_string_lossy()),
                toml,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_and_absolute() {
        assert!(is_safe_archive_path(Path::new("config.toml")));
        assert!(is_safe_archive_path(Path::new("Accounts/a.auth")));
        assert!(is_safe_archive_path(Path::new("foo..bar")));
        assert!(!is_safe_archive_path(Path::new("../etc/passwd")));
        assert!(!is_safe_archive_path(Path::new(
            "Accounts/../../etc/passwd"
        )));
        assert!(!is_safe_archive_path(Path::new("/etc/passwd")));
        assert!(!is_safe_archive_path(Path::new("")));
    }

    #[test]
    fn roundtrip_native_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("files");
        std::fs::create_dir_all(files.join("plugins/example")).unwrap();
        std::fs::write(files.join("config.toml"), b"library.auto_acquire = false\n").unwrap();
        std::fs::write(files.join("library.db"), b"sqlite-placeholder").unwrap();
        std::fs::write(
            files.join("plugins/example/plugin.toml"),
            b"id = \"example\"\n",
        )
        .unwrap();

        let archive = tmp.path().join("backup.tar.gz");
        let summary = export_native(NativeExportOptions {
            files_dir: files.clone(),
            dest: archive.clone(),
            bookclerk_version: "0.1.0-test".into(),
            include_plugin_manifests: true,
            include_cache: false,
            include_logs: false,
            include_plugin_databases: false,
        })
        .unwrap();
        assert!(summary.files >= 3);
        assert!(
            archive.is_file(),
            "archive must be written outside files_dir"
        );
        assert!(!files.join("backup.tar.gz").exists());

        let dest = tmp.path().join("restored");
        let imp = import_native(NativeImportOptions {
            archive,
            dest_files_dir: dest.clone(),
            force: true,
            dry_run: false,
        })
        .unwrap();
        assert!(imp.files >= 3);
        assert_eq!(
            std::fs::read_to_string(dest.join("config.toml")).unwrap(),
            "library.auto_acquire = false\n"
        );
        assert!(dest.join("plugins/example/plugin.toml").is_file());
    }

    #[test]
    fn export_skips_missing_optional_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("files");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("config.toml"), b"library.auto_acquire = false\n").unwrap();
        let archive = tmp.path().join("backup.tar.gz");
        let summary = export_native(NativeExportOptions {
            files_dir: files,
            dest: archive.clone(),
            bookclerk_version: "0.1.0-test".into(),
            include_plugin_manifests: true,
            include_cache: true,
            include_logs: true,
            include_plugin_databases: true,
        })
        .expect("missing optional dirs are skipped");
        assert!(archive.is_file());
        assert!(summary.files >= 1);
        assert!(!summary.included.iter().any(|p| p.starts_with("cache")));
        assert!(!summary.included.iter().any(|p| p.starts_with("logs")));
        assert!(!summary
            .included
            .iter()
            .any(|p| p.starts_with("plugin-databases")));
        assert!(!summary.included.iter().any(|p| p.starts_with("plugins")));
    }

    #[cfg(unix)]
    #[test]
    fn export_rejects_dangling_optional_directory_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("files");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("config.toml"), b"x\n").unwrap();
        std::os::unix::fs::symlink(files.join("missing-cache-target"), files.join("cache"))
            .unwrap();
        let err = export_native(NativeExportOptions {
            files_dir: files,
            dest: tmp.path().join("backup.tar.gz"),
            bookclerk_version: "0.1.0-test".into(),
            include_plugin_manifests: false,
            include_cache: true,
            include_logs: false,
            include_plugin_databases: false,
        })
        .expect_err("dangling cache symlink");
        let msg = format!("{err}");
        assert!(
            msg.contains("symlink") || msg.contains("dangling"),
            "expected dangling-symlink refusal, got {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn export_rejects_cache_symlink_self_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("files");
        let cache = files.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(files.join("config.toml"), b"x\n").unwrap();
        std::os::unix::fs::symlink(&cache, cache.join("loop")).unwrap();
        let err = export_native(NativeExportOptions {
            files_dir: files,
            dest: tmp.path().join("backup.tar.gz"),
            bookclerk_version: "0.1.0-test".into(),
            include_plugin_manifests: false,
            include_cache: true,
            include_logs: false,
            include_plugin_databases: false,
        })
        .expect_err("self-loop cache symlink");
        let msg = format!("{err}");
        assert!(
            msg.contains("symlink"),
            "expected symlink refusal, got {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn export_rejects_two_directory_cache_symlink_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("files");
        let a = files.join("cache/a");
        let b = files.join("cache/b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(files.join("config.toml"), b"x\n").unwrap();
        std::os::unix::fs::symlink(&b, a.join("to-b")).unwrap();
        std::os::unix::fs::symlink(&a, b.join("to-a")).unwrap();
        let err = export_native(NativeExportOptions {
            files_dir: files,
            dest: tmp.path().join("backup.tar.gz"),
            bookclerk_version: "0.1.0-test".into(),
            include_plugin_manifests: false,
            include_cache: true,
            include_logs: false,
            include_plugin_databases: false,
        })
        .expect_err("two-dir cache symlink loop");
        assert!(format!("{err}").contains("symlink"));
    }
}
