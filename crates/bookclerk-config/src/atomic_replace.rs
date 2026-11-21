//! Atomic same-directory file replace for `config.toml` and the other
//! bootstrap files written beside it (`node-identity.json`, `master.key`).
//!
//! `std::fs::rename` replaces an existing destination on Unix and on Windows.
//! Rust's Windows implementation uses `MoveFileExW`, with a
//! `SetFileInformationByHandle` fallback. This helper's distinction is an
//! explicit `MOVEFILE_WRITE_THROUGH` policy, so a power loss after a successful
//! config write cannot leave a cached rename unpublished. It does not delete
//! `to` before the rename.
//!
//! [`atomic-write-file`](https://docs.rs/atomic-write-file) was considered; it
//! does not guarantee `MOVEFILE_WRITE_THROUGH`. Unique staging names live in
//! `settings::staging_toml_path`.

#![allow(unsafe_code)] // Windows `MoveFileExW` FFI in [`replace_file_windows`]

use std::path::Path;

/// Replace `to` with `from`, both in the same directory. `to` may already
/// exist; on Windows this goes through `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` (see the module
/// docs), retrying briefly while another replace of `to` settles. The moved
/// file keeps its own permissions, so a `0600` staging file
/// lands as a `0600` target.
///
/// # Errors
///
/// Returns an I/O error when the platform replace operation fails.
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
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
    let mut attempt = 0u32;
    loop {
        // SAFETY: `src` and `dst` are NUL-terminated wide paths that outlive the call.
        let ok = unsafe {
            MoveFileExW(
                src.as_ptr(),
                dst.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok != 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        attempt += 1;
        if attempt >= REPLACE_ATTEMPTS || !replace_is_transient(&err) {
            return Err(err);
        }
        std::thread::sleep(std::time::Duration::from_millis(u64::from(attempt) * 10));
    }
}

/// How many times [`replace_file_windows`] tries before giving up: about
/// half a second of backoff in all.
#[cfg(windows)]
const REPLACE_ATTEMPTS: u32 = 10;

/// Whether a failed `MoveFileExW` is worth retrying.
///
/// Windows refuses to replace a file while another rename onto it is still
/// settling, or while a reader holds it open without delete sharing, with
/// access-denied or a sharing violation. Both clear on their own; Unix renames
/// never see them.
#[cfg(windows)]
fn replace_is_transient(err: &std::io::Error) -> bool {
    /// `ERROR_ACCESS_DENIED`.
    const ACCESS_DENIED: i32 = 5;
    /// `ERROR_SHARING_VIOLATION`.
    const SHARING_VIOLATION: i32 = 32;
    matches!(err.raw_os_error(), Some(ACCESS_DENIED | SHARING_VIOLATION))
}
