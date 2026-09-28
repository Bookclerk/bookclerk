//! Short private directory for guest pathname sockets.
//!
//! macOS `sockaddr_un.sun_path` is 104 bytes, including the NUL. The guest
//! Postgres mediator binds `{dir}/.s.PGSQL.<port>` there, and the host binds
//! the OAuth callback socket in the same directory. `/tmp` itself is not
//! granted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// macOS `sun_path` capacity. Rust rejects a path whose length is greater than
/// or equal to this value.
pub const MACOS_SUN_PATH_CAPACITY: usize = 104;

/// Longest socket file the guest must create in the IPC directory.
const LONGEST_SOCKET_NAME: &str = ".s.PGSQL.65535";

/// Create a mode `0700` directory under `/tmp` whose longest guest socket fits
/// in a macOS `sockaddr_un`.
///
/// The returned path is canonical, so a `/tmp` symlink (`/private/tmp` on
/// macOS) is what the jail profile and the guest both see.
///
/// # Errors
///
/// Returns when `/tmp` cannot hold the directory, or when the canonical path
/// plus `.s.PGSQL.65535` does not fit. The error includes the path length and
/// the directory that was too long.
pub fn create_guest_ipc_dir() -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;

    static NONCE: AtomicU64 = AtomicU64::new(1);
    let parent = Path::new("/tmp");
    for _ in 0..8 {
        let n = NONCE.fetch_add(1, Ordering::Relaxed);
        let tick = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        let token = n.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ tick ^ u64::from(std::process::id());
        let dir = parent.join(format!("bc-{:08x}", token as u32));
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "could not create guest IPC directory {}: {err}",
                    dir.display()
                ));
            }
        }
        if let Err(err) = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)) {
            let _ = std::fs::remove_dir(&dir);
            return Err(format!(
                "could not set mode 0700 on guest IPC directory {}: {err}",
                dir.display()
            ));
        }
        let canonical = match std::fs::canonicalize(&dir) {
            Ok(path) => path,
            Err(err) => {
                let _ = std::fs::remove_dir(&dir);
                return Err(format!(
                    "could not canonicalize guest IPC directory {}: {err}",
                    dir.display()
                ));
            }
        };
        if let Err(err) = ensure_guest_ipc_fits(&canonical) {
            let _ = std::fs::remove_dir(&canonical);
            return Err(err);
        }
        return Ok(canonical);
    }
    Err("could not allocate a unique guest IPC directory under /tmp".into())
}

/// Reject `dir` when `{dir}/.s.PGSQL.65535` does not fit in a macOS `sockaddr_un`.
///
/// # Errors
///
/// The message includes the socket path length and `dir`.
pub fn ensure_guest_ipc_fits(dir: &Path) -> Result<(), String> {
    let socket = dir.join(LONGEST_SOCKET_NAME);
    let len = socket.as_os_str().len();
    if len >= MACOS_SUN_PATH_CAPACITY {
        return Err(format!(
            "guest IPC socket path length {len} does not fit sockaddr_un capacity {MACOS_SUN_PATH_CAPACITY}; directory {} is too long",
            dir.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_that_cannot_fit_names_the_length_and_the_path() {
        let dir = PathBuf::from(format!("/tmp/{}", "a".repeat(120)));
        let err = ensure_guest_ipc_fits(&dir).expect_err("long directory");
        let socket = dir.join(LONGEST_SOCKET_NAME);
        assert!(err.contains(&socket.as_os_str().len().to_string()), "{err}");
        assert!(err.contains(&dir.display().to_string()), "{err}");
        assert!(err.contains("too long"), "{err}");
    }

    #[test]
    fn created_directory_is_private_and_fits_sockaddr_un() {
        let dir = create_guest_ipc_dir().expect("create");
        let meta = std::fs::metadata(&dir).expect("metadata");
        let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
        assert_eq!(mode & 0o777, 0o700, "mode {mode:o}");
        assert_ne!(dir, Path::new("/tmp"));
        assert_ne!(dir, Path::new("/private/tmp"));
        ensure_guest_ipc_fits(&dir).expect("fits");
        assert!(
            !dir.ends_with("plugin-state"),
            "the IPC directory is not the plugin scratch tree"
        );
        let _ = std::fs::remove_dir(&dir);
    }
}
