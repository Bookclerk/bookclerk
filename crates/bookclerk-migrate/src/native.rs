//! Native Bookclerk portable backup (`.tar.gz` archive).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use crate::error::{MigrateError, Result};

fn err(msg: impl Into<String>) -> MigrateError {
    MigrateError::Source(msg.into())
}

/// Current native backup format version.
pub const NATIVE_BACKUP_FORMAT_VERSION: u32 = 1;

/// Manifest stored at `manifest.json` inside the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeBackupManifest {
    pub format_version: u32,
    pub created_at: String,
    pub bookclerk_version: String,
    pub included: Vec<String>,
}

/// Options for exporting a native backup.
#[derive(Debug, Clone)]
pub struct NativeExportOptions {
    pub files_dir: PathBuf,
    pub dest: PathBuf,
    pub bookclerk_version: String,
    /// Include `plugins/**/plugin.toml` (not plugin binaries).
    pub include_plugin_manifests: bool,
    /// Include `cache/` (large; off by default).
    pub include_cache: bool,
    /// Include `logs/` (off by default).
    pub include_logs: bool,
}

/// Options for importing a native backup.
#[derive(Debug, Clone)]
pub struct NativeImportOptions {
    pub archive: PathBuf,
    pub dest_files_dir: PathBuf,
    pub force: bool,
    pub dry_run: bool,
}

/// Summary of a native export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeExportSummary {
    pub archive: String,
    pub files: usize,
    pub included: Vec<String>,
}

/// Summary of a native import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeImportSummary {
    pub files: usize,
    pub format_version: u32,
    pub warnings: Vec<String>,
}

/// Export Bookclerk files-dir essentials into a `.tar.gz` archive.
pub fn export_native(opts: NativeExportOptions) -> Result<NativeExportSummary> {
    if let Some(parent) = opts.dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(&opts.dest)
        .map_err(|source| err(format!("create {}: {source}", opts.dest.display())))?;
    let enc = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut builder = tar::Builder::new(enc);

    let mut included = Vec::new();
    let mut file_count = 0usize;
    let mut entries: Vec<(String, PathBuf)> = Vec::new();

    push_if_exists(&opts.files_dir, "config.toml", &mut entries, &mut included);
    push_if_exists(&opts.files_dir, "library.db", &mut entries, &mut included);

    if opts.include_plugin_manifests {
        collect_plugin_tomls(&opts.files_dir.join("plugins"), &mut entries, &mut included)?;
    }
    if opts.include_cache {
        collect_dir(
            &opts.files_dir.join("cache"),
            "cache",
            &mut entries,
            &mut included,
            true,
        )?;
    }
    if opts.include_logs {
        collect_dir(
            &opts.files_dir.join("logs"),
            "logs",
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
        archive: opts.dest.display().to_string(),
        files: file_count,
        included,
    })
}

/// Restore a native backup archive into `dest_files_dir`.
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

fn push_if_exists(
    root: &Path,
    rel: &str,
    entries: &mut Vec<(String, PathBuf)>,
    included: &mut Vec<String>,
) {
    let path = root.join(rel);
    if path.is_file() {
        entries.push((rel.to_string(), path));
        included.push(rel.to_string());
    }
}

fn collect_dir(
    dir: &Path,
    arc_prefix: &str,
    entries: &mut Vec<(String, PathBuf)>,
    included: &mut Vec<String>,
    recursive: bool,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    included.push(format!("{arc_prefix}/"));
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let arc_name = format!("{arc_prefix}/{}", name.to_string_lossy());
        if path.is_file() {
            entries.push((arc_name, path));
        } else if recursive && path.is_dir() {
            collect_dir(&path, &arc_name, entries, included, true)?;
        }
    }
    Ok(())
}

fn collect_plugin_tomls(
    plugins_root: &Path,
    entries: &mut Vec<(String, PathBuf)>,
    included: &mut Vec<String>,
) -> Result<()> {
    if !plugins_root.is_dir() {
        return Ok(());
    }
    included.push("plugins/**/plugin.toml".into());
    let root_toml = plugins_root.join("plugin.toml");
    if root_toml.is_file() {
        entries.push(("plugins/plugin.toml".into(), root_toml));
    }
    for entry in std::fs::read_dir(plugins_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let toml = path.join("plugin.toml");
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
        })
        .unwrap();
        assert!(summary.files >= 3);

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
}
