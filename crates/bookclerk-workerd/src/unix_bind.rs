//! Unix-domain GRANTED listeners that stay under `sockaddr_un` limits.
//!
//! Cargo sets `TMPDIR` to workspace `.tmp`. PluginKey state dirs
//! (`plugin-state/pk-…/tmp/…/granted.sock`) overflow Linux's 108-byte
//! `sun_path`. Linux therefore binds a `unix-abstract:` name; other Unix
//! binds `granted.sock` under `state_dir` and passes a relative address
//! (workerd's cwd is that directory).

#![allow(clippy::missing_docs_in_private_items)]

use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

use rand::RngCore;

#[cfg(not(target_os = "linux"))]
use std::sync::Mutex;

/// Pathname leaf for the GRANTED channel on non-Linux Unix.
pub const GRANTED_SOCK_FILE: &str = "granted.sock";

/// workerd / KJ address for a relative GRANTED pathname (cwd = state dir).
pub const GRANTED_RELATIVE_ADDR: &str = "unix:granted.sock";

/// Serializes `chdir` around pathname binds that overflow `sun_path`.
#[cfg(not(target_os = "linux"))]
static CHDIR_BIND: Mutex<()> = Mutex::new(());

/// Random Linux abstract name (`bc-{kind}-{16 hex}`), always far under `sun_path`.
#[must_use]
pub fn random_abstract_name(kind: &str) -> String {
    let mut nonce = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut nonce);
    format!("bc-{kind}-{}", hex::encode(nonce))
}

/// Binds the adapter-private GRANTED channel.
///
/// # Errors
///
/// Returns an I/O error when the listener cannot be bound.
pub fn bind_granted(state_dir: &Path) -> io::Result<(String, UnixListener)> {
    #[cfg(target_os = "linux")]
    {
        let _ = state_dir;
        let name = random_abstract_name("g");
        let listener = bind_abstract(name.as_bytes())?;
        Ok((format!("unix-abstract:{name}"), listener))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path = state_dir.join(GRANTED_SOCK_FILE);
        let listener = bind_pathname(&path)?;
        Ok((GRANTED_RELATIVE_ADDR.to_string(), listener))
    }
}

/// Binds a Linux abstract-namespace Unix listener (`sun_path[0] == NUL`).
///
/// # Errors
///
/// Returns an I/O error when the name is empty/too long or `bind(2)` fails.
#[cfg(target_os = "linux")]
pub fn bind_abstract(name: &[u8]) -> io::Result<UnixListener> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;
    let addr = SocketAddr::from_abstract_name(name)?;
    UnixListener::bind_addr(&addr)
}

/// Binds a pathname socket, falling back when the absolute path exceeds `sun_path`.
///
/// # Errors
///
/// Returns an I/O error when the parent cannot be created or `bind(2)` fails.
#[cfg(not(target_os = "linux"))]
pub fn bind_pathname(path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(err) if pathname_too_long(&err, path) => bind_pathname_chdir(path),
        Err(err) => Err(err),
    }
}

#[cfg(not(target_os = "linux"))]
fn pathname_too_long(err: &io::Error, path: &Path) -> bool {
    err.kind() == io::ErrorKind::InvalidInput || path.as_os_str().len() >= 104
}

#[cfg(not(target_os = "linux"))]
fn bind_pathname_chdir(path: &Path) -> io::Result<UnixListener> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "unix socket path has no parent directory",
        )
    })?;
    let leaf = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "unix socket path has no file name",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let _guard = CHDIR_BIND.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::current_dir()?;
    std::env::set_current_dir(parent)?;
    let result = UnixListener::bind(leaf);
    let _ = std::env::set_current_dir(prev);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abstract_names_stay_short() {
        let name = random_abstract_name("g");
        assert!(name.starts_with("bc-g-"), "{name}");
        assert!(name.len() < 48, "unix-abstract address too long: {name}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bind_granted_uses_unix_abstract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (addr, _listener) = bind_granted(dir.path()).expect("bind granted");
        assert!(
            addr.starts_with("unix-abstract:bc-g-"),
            "GRANTED must be unix-abstract, got {addr}"
        );
    }
}
