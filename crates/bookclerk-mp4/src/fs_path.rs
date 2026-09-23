//! Empty/NUL checks before MP4 filesystem sinks.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::error::{Mp4Error, Result};

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
/// These MP4 APIs take caller-selected filesystem paths (not untrusted
/// suffixes under a trusted root). Do **not** reject lexical `..` or convert
/// through UTF-8 lossy; containment belongs at call sites that have a root.
///
/// # Errors
///
/// Returns [`Mp4Error`] when the path is empty or contains an interior NUL.
pub(crate) fn validated(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(Mp4Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing empty path",
        )));
    }
    if os_contains_nul(path.as_os_str()) {
        return Err(Mp4Error::container(format!(
            "refusing path with interior NUL: {}",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}
