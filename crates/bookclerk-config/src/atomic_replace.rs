//! Atomic same-directory file replace for `config.toml`.
//!
//! Unix `rename` replaces the destination. Windows `std::fs::rename` cannot,
//! so this uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`
//! instead of deleting `to` first (a crash between delete and rename would
//! leave no config file).
//!
//! [`atomic-write-file`](https://docs.rs/atomic-write-file) was considered; it
//! does not guarantee `MOVEFILE_WRITE_THROUGH`, which Bookclerk needs so a
//! power-loss after a successful config write cannot leave a cached rename
//! unpublished. Unique staging names live in `settings::staging_toml_path`.

#![allow(unsafe_code)] // Windows `MoveFileExW` FFI in [`replace_file_windows`]

use std::path::Path;

/// Replace `to` with `from`.
pub(crate) fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        replace_file_windows(from, to)
    }
}

#[cfg(windows)]
/// Atomically replaces `to` with `from` via `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`.
///
/// # Errors
///
/// Returns an I/O error when `MoveFileExW` fails.
fn replace_file_windows(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    /// Encodes `path` as a NUL-terminated wide string for Win32 APIs.
    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let src = wide(from);
    let dst = wide(to);
    // SAFETY: `src` and `dst` are NUL-terminated wide paths that outlive the call.
    let ok = unsafe {
        MoveFileExW(
            src.as_ptr(),
            dst.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
