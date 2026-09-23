//! Empty/NUL checks and single-component joins before DRM filesystem sinks.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::error::{DrmError, Result};

/// True when `s` contains an interior NUL.
fn os_contains_nul(s: &OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().contains(&0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().any(|c| c == 0)
    }
}

/// Rejects empty paths and interior NULs; returns `path` unchanged otherwise.
///
/// Download/output paths are caller-selected under the acquire cache, not
/// untrusted suffixes under a separate root. Do not reject lexical `..` or
/// convert through UTF-8 lossy here.
///
/// # Errors
///
/// Returns [`DrmError::Native`] when the path is empty or contains NUL.
pub(crate) fn validated_fs_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(DrmError::Native("refusing empty path".into()));
    }
    if os_contains_nul(path.as_os_str()) {
        return Err(DrmError::Native(format!(
            "refusing path with interior NUL: {}",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

/// Join a single path component under `cache_dir` for title-scoped work dirs.
///
/// `component` must be one normal path segment (ASINs qualify). When the joined
/// path already exists, containment is enforced with canonicalize + `starts_with`
/// so Default Setup path-injection queries can barrier the flow.
///
/// # Errors
///
/// Returns [`DrmError::Native`] when `cache_dir` / `component` is unsafe or the
/// join escapes the cache root.
pub(crate) fn join_cache_component(cache_dir: &Path, component: &str) -> Result<PathBuf> {
    let cache_dir = validated_fs_path(cache_dir)?;
    if component.is_empty() || component.contains('\0') {
        return Err(DrmError::Native(
            "refusing empty cache path component".into(),
        ));
    }
    // CodeQL DotDotCheck barrier when false.
    if component.contains("..") {
        return Err(DrmError::Native(format!(
            "refusing cache path component with '..': {component}"
        )));
    }
    if component.contains('/') || component.contains('\\') {
        return Err(DrmError::Native(format!(
            "refusing multi-segment cache path component: {component}"
        )));
    }
    let rel = Path::new(component);
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(DrmError::Native(format!(
            "refusing multi-segment cache path component: {component}"
        )));
    }
    let mut normals = rel.components().filter_map(|c| match c {
        Component::Normal(s) => Some(s),
        Component::CurDir => None,
        _ => None,
    });
    let name = match (normals.next(), normals.next()) {
        (Some(name), None) => name,
        _ => {
            return Err(DrmError::Native(format!(
                "refusing multi-segment cache path component: {component}"
            )));
        }
    };

    let root_norm = fs::canonicalize(&cache_dir).map_err(|e| {
        DrmError::Native(format!(
            "could not canonicalize cache root {}: {e}",
            cache_dir.display()
        ))
    })?;
    let joined = root_norm.join(name);
    if let Ok(canon) = fs::canonicalize(&joined) {
        if !canon.starts_with(&root_norm) {
            return Err(DrmError::Native(format!(
                "path {} escapes cache root {}",
                canon.display(),
                root_norm.display()
            )));
        }
        return Ok(canon);
    }
    match fs::symlink_metadata(&joined) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(DrmError::Native(format!(
                "refusing dangling or unresolvable symlink: {}",
                joined.display()
            )));
        }
        Ok(_) => {
            return Err(DrmError::Native(format!(
                "could not canonicalize existing path: {}",
                joined.display()
            )));
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(DrmError::Native(format!(
                "could not stat path {}: {e}",
                joined.display()
            )));
        }
        Err(_) => {}
    }
    if !joined.starts_with(&root_norm) {
        return Err(DrmError::Native(format!(
            "path {} escapes cache root {}",
            joined.display(),
            root_norm.display()
        )));
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_cache_component_rejects_traversal() {
        let root = std::env::temp_dir();
        assert!(join_cache_component(&root, "..").is_err());
        assert!(join_cache_component(&root, "../x").is_err());
        assert!(join_cache_component(&root, "a/b").is_err());
        assert!(join_cache_component(&root, "a\\b").is_err());
    }

    #[test]
    fn join_cache_component_accepts_asin_shape() {
        let root = std::env::temp_dir();
        let out = join_cache_component(&root, "B00EXAMPLE1").expect("asin");
        assert!(out.ends_with("B00EXAMPLE1"));
    }
}
