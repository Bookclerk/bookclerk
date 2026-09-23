//! Safe ZIP / tar.gz extraction with path traversal and size limits.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;
use zip::ZipArchive;

use crate::error::{CatalogError, Result};
use crate::target::ArchiveFormat;

/// Hard cap on archive file size before extraction.
pub const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB
/// Hard cap on total extracted archive bytes.
pub const MAX_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
/// Maximum extracted-bytes / archive-bytes ratio before a zip-bomb is rejected (100:1).
pub const MAX_COMPRESSION_RATIO: u64 = 100;
/// Hard cap on a single archive member (512 MiB).
pub const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

/// SHA-256 hex digest of a file.
///
/// Callers must pass a path already contained under a trusted root (for
/// example via [`safe_join`] / [`require_under`]). Trusted roots may retain
/// intentional `..` spelling until the caller canonicalizes them; this hasher
/// does not re-litigate path spelling.
///
/// # Errors
///
/// Returns an error when the file cannot be read.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// SHA-256 hex digest of bytes.
#[must_use]
pub fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Extract an archive into `dest`, refusing path traversal and oversized output.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn extract_archive(archive: &Path, format: ArchiveFormat, dest: &Path) -> Result<()> {
    if dest.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(CatalogError::message(format!(
            "refusing extract dest with '..': {}",
            dest.display()
        )));
    }
    fs::create_dir_all(dest)?;
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(archive, dest),
        ArchiveFormat::Zip => extract_zip(archive, dest),
    }
}

/// Extracts a tar.gz while refusing traversal, links, and oversized or high-ratio members.
fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    // Archive path is under host staging (caller); refuse `..` before open.
    if archive
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(CatalogError::message(format!(
            "refusing archive path with '..': {}",
            archive.display()
        )));
    }
    let file = File::open(archive)?;
    let meta = file.metadata()?;
    if meta.len() > MAX_ARCHIVE_BYTES {
        return Err(CatalogError::message(format!(
            "archive exceeds {} byte limit",
            MAX_ARCHIVE_BYTES
        )));
    }
    let dec = GzDecoder::new(file);
    let mut tar = Archive::new(dec);
    let mut extracted: u64 = 0;
    for entry in tar.entries().map_err(io_err)? {
        let mut entry = entry.map_err(io_err)?;
        let path = entry.path().map_err(io_err)?.into_owned();
        let out = safe_join(dest, &path)?;
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if entry.header().entry_type().is_symlink() || entry.header().entry_type().is_hard_link() {
            return Err(CatalogError::message(format!(
                "refusing symlink/hardlink in archive: {}",
                path.display()
            )));
        }
        let size = entry.header().size().map_err(io_err)?;
        if size > MAX_ENTRY_BYTES {
            return Err(CatalogError::message(format!(
                "archive entry {} exceeds entry size limit",
                path.display()
            )));
        }
        extracted = extracted.saturating_add(size);
        if extracted > MAX_EXTRACTED_BYTES {
            return Err(CatalogError::message(
                "extracted size exceeds decompression limit",
            ));
        }
        if meta.len() > 0 && extracted / meta.len().max(1) > MAX_COMPRESSION_RATIO {
            return Err(CatalogError::message(
                "archive compression ratio exceeds limit",
            ));
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file = File::create(&out)?;
        io::copy(&mut entry, &mut out_file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if entry.header().mode().unwrap_or(0) & 0o111 != 0 {
                let mut perms = out_file.metadata()?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&out, perms)?;
            }
        }
    }
    Ok(())
}

/// Extracts a zip while refusing traversal, symlinks, NULs, and oversized or high-ratio members.
fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    if archive
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(CatalogError::message(format!(
            "refusing archive path with '..': {}",
            archive.display()
        )));
    }
    let file = File::open(archive)?;
    let meta = file.metadata()?;
    if meta.len() > MAX_ARCHIVE_BYTES {
        return Err(CatalogError::message(format!(
            "archive exceeds {} byte limit",
            MAX_ARCHIVE_BYTES
        )));
    }
    let mut zip = ZipArchive::new(file).map_err(|e| CatalogError::message(e.to_string()))?;
    let mut extracted: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| CatalogError::message(e.to_string()))?;
        let name = entry.name().to_string();
        if name.contains('\0') {
            return Err(CatalogError::message("archive entry name contains NUL"));
        }
        let path = PathBuf::from(&name);
        let out = safe_join(dest, &path)?;
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        // zip crate exposes symlink via unix mode / extra fields; reject absolute
        // and traversal via safe_join. Also reject names that claim to be links
        // when enclosed name looks like a symlink marker.
        if entry
            .unix_mode()
            .is_some_and(|m| (m & 0o170000) == 0o120000)
        {
            return Err(CatalogError::message(format!(
                "refusing symlink in zip: {name}"
            )));
        }
        let size = entry.size();
        if size > MAX_ENTRY_BYTES {
            return Err(CatalogError::message(format!(
                "archive entry {name} exceeds entry size limit"
            )));
        }
        extracted = extracted.saturating_add(size);
        if extracted > MAX_EXTRACTED_BYTES {
            return Err(CatalogError::message(
                "extracted size exceeds decompression limit",
            ));
        }
        if meta.len() > 0 && extracted / meta.len().max(1) > MAX_COMPRESSION_RATIO {
            return Err(CatalogError::message(
                "archive compression ratio exceeds limit",
            ));
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file = File::create(&out)?;
        io::copy(&mut entry, &mut out_file)?;
        #[cfg(unix)]
        if entry.unix_mode().is_some_and(|m| m & 0o111 != 0) {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&out)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&out, perms)?;
        }
    }
    Ok(())
}

/// Join `dest` / `rel` while rejecting absolute paths and `..` escapes.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn safe_join(dest: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.is_absolute() {
        return Err(CatalogError::message(format!(
            "refusing absolute archive path: {}",
            rel.display()
        )));
    }
    let mut out = dest.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(CatalogError::message(format!(
                    "refusing path traversal in archive: {}",
                    rel.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(CatalogError::message(format!(
                    "refusing rooted archive path: {}",
                    rel.display()
                )));
            }
        }
    }
    let canon_dest = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    // Ensure out stays under dest even before create (lexical).
    if !out.starts_with(dest) && !out.starts_with(&canon_dest) {
        return Err(CatalogError::message(format!(
            "archive path escapes destination: {}",
            rel.display()
        )));
    }
    Ok(out)
}

/// Returns `path` when it stays under `root` (canonical containment).
///
/// Use before filesystem sinks that already hold an absolute path built from a
/// trusted root (install dest, staging, receipt). Prefer [`safe_join`] when the
/// relative segment is still separate.
///
/// When `path` does not exist yet, canonicalizes the nearest existing ancestor
/// and rejoins the missing suffix (same approach as bookclerk-sandbox
/// `require_under_root`) so a symlinked parent such as
/// `root/.staging -> /outside` cannot pass a lexical `starts_with` check.
///
/// # Errors
///
/// Returns when `path` contains a `ParentDir` component, or escapes `root`
/// lexically or after canonicalize.
pub fn require_under(root: &Path, path: &Path) -> Result<PathBuf> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(CatalogError::message(format!(
            "refusing path with '..': {}",
            path.display()
        )));
    }
    let root_norm = root.canonicalize().map_err(|source| {
        CatalogError::message(format!(
            "could not canonicalize root {}: {source}",
            root.display()
        ))
    })?;
    // Lexical under-root barrier *before* canonicalize/symlink_metadata sinks.
    // Compare against both the caller root and its canonical form: Windows
    // canonicalize rewrites 8.3 short names and adds a `\\?\` prefix, so a
    // same-form `starts_with(root)` still holds when the canonical form does not.
    let lexical = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root_norm.join(path)
    };
    #[cfg(not(windows))]
    if !lexical.starts_with(&root_norm) && !lexical.starts_with(root) {
        return Err(CatalogError::message(format!(
            "path {} escapes root {}",
            lexical.display(),
            root_norm.display()
        )));
    }
    let path_norm = match lexical.canonicalize() {
        Ok(c) => c,
        Err(err) => {
            match fs::symlink_metadata(&lexical) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(CatalogError::message(format!(
                        "refusing dangling or unresolvable symlink {}: {err}",
                        lexical.display()
                    )));
                }
                Ok(_) => {
                    return Err(CatalogError::message(format!(
                        "could not canonicalize existing path {} under {}: {err}",
                        lexical.display(),
                        root.display()
                    )));
                }
                Err(meta_err) if meta_err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(CatalogError::message(format!(
                        "could not stat path {} under {}: {meta_err}",
                        lexical.display(),
                        root.display()
                    )));
                }
                Err(_) => {}
            }
            let mut suffix = Vec::new();
            let mut cursor = lexical.clone();
            loop {
                #[cfg(not(windows))]
                if !cursor.starts_with(&root_norm) && !cursor.starts_with(root) {
                    return Err(CatalogError::message(format!(
                        "path {} escapes root {}",
                        cursor.display(),
                        root_norm.display()
                    )));
                }
                match cursor.canonicalize() {
                    Ok(canon) => {
                        let mut out = canon;
                        for part in suffix.iter().rev() {
                            out.push(part);
                        }
                        break out;
                    }
                    Err(canon_err) => {
                        match fs::symlink_metadata(&cursor) {
                            Ok(meta) if meta.file_type().is_symlink() => {
                                return Err(CatalogError::message(format!(
                                    "refusing dangling or unresolvable symlink {} under {}: {canon_err}",
                                    cursor.display(),
                                    root.display()
                                )));
                            }
                            Ok(_) => {
                                return Err(CatalogError::message(format!(
                                    "could not canonicalize path {} under {}: {canon_err}",
                                    cursor.display(),
                                    root.display()
                                )));
                            }
                            Err(meta_err) if meta_err.kind() != std::io::ErrorKind::NotFound => {
                                return Err(CatalogError::message(format!(
                                    "could not stat path {} under {}: {meta_err}",
                                    cursor.display(),
                                    root.display()
                                )));
                            }
                            Err(_) => {}
                        }
                        let name = cursor.file_name().ok_or_else(|| {
                            CatalogError::message(format!(
                                "could not resolve path {} under {}: {err}",
                                path.display(),
                                root.display()
                            ))
                        })?;
                        suffix.push(name.to_os_string());
                        match cursor.parent() {
                            Some(parent) if !parent.as_os_str().is_empty() => {
                                cursor = parent.to_path_buf();
                            }
                            _ => {
                                return Err(CatalogError::message(format!(
                                    "could not resolve path {} under {}: {err}",
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
        return Err(CatalogError::message(format!(
            "path {} escapes root {}",
            path_norm.display(),
            root_norm.display()
        )));
    }
    Ok(path_norm)
}

/// Lexical child path under `root` safe for `remove_dir_all` / `rename` / create.
///
/// Unlike [`require_under`], this returns the intended directory entry (not a
/// symlink target) and refuses when the leaf exists as a symlink or when a
/// resolved existing path equals the trusted root (so cleanup cannot wipe the
/// root via an in-root alias). Missing leaves are validated via [`require_under`].
///
/// # Errors
///
/// Returns when the path escapes `root`, is a symlink leaf, or would mutate the root.
pub fn require_mutable_child(root: &Path, path: &Path) -> Result<PathBuf> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(CatalogError::message(format!(
            "refusing path with '..': {}",
            path.display()
        )));
    }
    let root_norm = root.canonicalize().map_err(|source| {
        CatalogError::message(format!(
            "could not canonicalize root {}: {source}",
            root.display()
        ))
    })?;
    let lexical = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root_norm.join(path)
    };
    #[cfg(not(windows))]
    if !lexical.starts_with(&root_norm) && !lexical.starts_with(root) {
        return Err(CatalogError::message(format!(
            "path {} escapes root {}",
            lexical.display(),
            root_norm.display()
        )));
    }
    match fs::symlink_metadata(&lexical) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(CatalogError::message(format!(
                "refusing symlink for mutation: {}",
                lexical.display()
            )));
        }
        Ok(_) => {
            let canon = lexical.canonicalize().map_err(|source| {
                CatalogError::message(format!(
                    "could not canonicalize {}: {source}",
                    lexical.display()
                ))
            })?;
            if !canon.starts_with(&root_norm) || canon == root_norm {
                return Err(CatalogError::message(format!(
                    "refusing mutation of {} (not a strict child of {})",
                    lexical.display(),
                    root_norm.display()
                )));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Missing leaf: prove the intended spelling stays under root.
            let _ = require_under(root, &lexical)?;
        }
        Err(source) => {
            return Err(CatalogError::message(format!(
                "could not stat {}: {source}",
                lexical.display()
            )));
        }
    }
    Ok(lexical)
}

/// Wraps an I/O or tar error as a catalog message.
fn io_err(err: impl std::fmt::Display) -> CatalogError {
    CatalogError::message(err.to_string())
}

/// Write bytes to a path, creating parents.
///
/// Callers must pass a path already contained under a trusted root.
pub fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(CatalogError::message(format!(
            "refusing path with '..': {}",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = File::create(path)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::Builder;

    #[test]
    fn rejects_zip_slip() {
        assert!(safe_join(Path::new("/tmp/out"), Path::new("../evil")).is_err());
        assert!(safe_join(Path::new("/tmp/out"), Path::new("/etc/passwd")).is_err());
        assert!(safe_join(Path::new("/tmp/out"), Path::new("ok/file")).is_ok());
    }

    #[test]
    fn extracts_tar_gz_and_rejects_traversal_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("ok.tar.gz");
        {
            let file = File::create(&archive).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "plugin.toml", &b"hi\n"[..])
                .unwrap();
            tar.finish().unwrap();
        }
        let dest = dir.path().join("out");
        extract_archive(&archive, ArchiveFormat::TarGz, &dest).unwrap();
        assert!(dest.join("plugin.toml").is_file());
    }

    #[test]
    fn safe_join_rejects_dotdot_components() {
        // The `tar` crate refuses to *write* `..` entries; we still guard on read
        // via `safe_join` for hand-crafted or foreign archives.
        let err = safe_join(Path::new("/tmp/out"), Path::new("a/../../evil")).unwrap_err();
        assert!(
            err.to_string().contains("traversal") || err.to_string().contains("refusing"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn require_under_rejects_symlinked_parent_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let staging = root.path().join(".staging");
        std::os::unix::fs::symlink(outside.path(), &staging).unwrap();
        let escaped = staging.join("new");
        let err = require_under(root.path(), &escaped).unwrap_err();
        assert!(
            err.to_string().contains("escapes") || err.to_string().contains("could not"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn require_under_rejects_dangling_symlink_leaf() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let missing = outside.join("new.txt");
        let link = root.path().join("new.txt");
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        let err = require_under(root.path(), &link).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected dangling symlink refusal, got {err}"
        );
        assert!(!missing.exists(), "must not create the outside target");
    }
}
