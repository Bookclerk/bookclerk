//! Unix-domain listeners that stay under `sockaddr_un` limits.
//!
//! Cargo sets `TMPDIR` to workspace `.tmp` (see `.cargo/config.toml`).
//! On GitHub Actions that prefix is already ~42 bytes
//! (`/home/runner/work/bookclerk/bookclerk/.tmp`). Plugin scratch plus a
//! workerd leaf used to overflow Linux's 108-byte `sun_path`.
//!
//! Linux GRANTED (workerd ↔ launcher) stays `unix-abstract:` — workerd is a
//! child of this launcher, not a sibling guest. Other Unix: bind a pathname
//! under the host-chosen session dir and pass a **relative** `unix:granted.sock`
//! to workerd (cwd is that directory).
//!
//! Native-behind-workerd no longer binds a named socket-proxy listener. The
//! host delivers an inherited duplex (`fd:` / `handle:`) and
//! [`crate::socket_proxy::spawn_link`] multiplexes CONNECT streams over it.
//!
//! Do not fall back to `std::env::temp_dir()`: inside `bookclerk-workerd` that
//! is the process scratch (`TMPDIR`).

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

/// Clears `FD_CLOEXEC` so a spawned child inherits `fd`.
///
/// Used for the workerd `--socket-fd` RPC listener.
///
/// # Errors
///
/// Returns when `fcntl` `F_GETFD` / `F_SETFD` fails.
#[allow(unsafe_code)]
pub fn clear_cloexec(fd: std::os::fd::RawFd) -> io::Result<()> {
    set_fd_cloexec(fd, false)
}

/// Sets `FD_CLOEXEC` so a later `workerd` spawn cannot keep a sibling link open.
///
/// # Errors
///
/// Returns when `fcntl` `F_GETFD` / `F_SETFD` fails.
#[allow(unsafe_code)]
pub fn set_cloexec(fd: std::os::fd::RawFd) -> io::Result<()> {
    set_fd_cloexec(fd, true)
}

#[allow(unsafe_code)]
fn set_fd_cloexec(fd: std::os::fd::RawFd, cloexec: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let next = if cloexec {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, next) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Random Linux abstract name (`bc-{kind}-{16 hex}`), always far under `sun_path`.
#[must_use]
pub fn random_abstract_name(kind: &str) -> String {
    let mut nonce = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut nonce);
    format!("bc-{kind}-{}", hex::encode(nonce))
}

/// Binds the adapter-private GRANTED channel.
///
/// Linux returns a `unix-abstract:` address for workerd `external`. Other Unix
/// binds [`GRANTED_SOCK_FILE`] under `state_dir` and returns
/// [`GRANTED_RELATIVE_ADDR`] (workerd's cwd is that directory).
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
/// On Linux the fallback binds via `/proc/self/fd/{dirfd}/name` so the inode
/// still lands at `path`. Peers that `connect()` must use a short name (relative
/// path, `/proc/self/fd`, or abstract) — the absolute path remains too long.
///
/// # Errors
///
/// Returns an I/O error when the parent cannot be created or `bind(2)` fails.
pub fn bind_pathname(path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(err) if pathname_too_long(&err, path) => bind_pathname_short_addr(path),
        Err(err) => Err(err),
    }
}

fn pathname_too_long(err: &io::Error, path: &Path) -> bool {
    err.kind() == io::ErrorKind::InvalidInput || path.as_os_str().len() >= sun_path_capacity()
}

/// Usable `sun_path` bytes including the trailing NUL that Rust's std accounts for.
fn sun_path_capacity() -> usize {
    // Linux `sizeof(sun_path)` is 108; macOS is 104. std rejects `len >=` that.
    if cfg!(target_os = "macos") {
        104
    } else {
        108
    }
}

fn bind_pathname_short_addr(path: &Path) -> io::Result<UnixListener> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "unix socket path has no parent directory",
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "unix socket path has no file name",
        )
    })?;
    std::fs::create_dir_all(parent)?;

    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let dir = std::fs::File::open(parent)?;
        let proc_path = format!(
            "/proc/self/fd/{}/{}",
            dir.as_raw_fd(),
            Path::new(name).display()
        );
        UnixListener::bind(&proc_path)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _guard = CHDIR_BIND
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cwd = std::env::current_dir()?;
        std::env::set_current_dir(parent)?;
        let result = UnixListener::bind(name);
        let restore = std::env::set_current_dir(&cwd);
        match (result, restore) {
            (Ok(listener), Ok(())) => Ok(listener),
            (Ok(_), Err(err)) => Err(err),
            (Err(err), _) => Err(err),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn abstract_listener_accepts_connect() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;

        let name = random_abstract_name("t");
        assert!(
            name.len() < 40,
            "abstract name must stay far under sun_path: {name}"
        );
        let listener = bind_abstract(name.as_bytes()).expect("bind abstract");
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).expect("addr");
        let _client = UnixStream::connect_addr(&addr).expect("connect abstract");
        let (_server, _) = listener.accept().expect("accept");
    }

    #[test]
    fn bind_granted_uses_unix_abstract() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (addr, _listener) = bind_granted(dir.path()).expect("granted");
        assert!(
            addr.starts_with("unix-abstract:bc-g-"),
            "workerd GRANTED must be unix-abstract, got {addr}"
        );
        assert!(addr.len() < 48, "unix-abstract address too long: {addr}");
    }

    #[test]
    fn pathname_bind_survives_sockaddr_un_overflow() {
        use std::os::fd::AsRawFd;

        let base = tempfile::tempdir().expect("tmpdir");
        let long = base.path().join("x".repeat(120));
        std::fs::create_dir_all(&long).expect("mkdir");
        let sock = long.join(GRANTED_SOCK_FILE);
        assert!(
            sock.as_os_str().len() >= 108,
            "test path should overflow sun_path: {} ({})",
            sock.display(),
            sock.as_os_str().len()
        );
        assert!(
            UnixListener::bind(&sock).is_err(),
            "std bind must reject the overflowing absolute path"
        );
        let listener = bind_pathname(&sock).expect("bind via /proc/self/fd");
        let dir = std::fs::File::open(&long).expect("open dir");
        let proc_path = format!("/proc/self/fd/{}/{GRANTED_SOCK_FILE}", dir.as_raw_fd());
        let _client = UnixStream::connect(&proc_path).expect("connect via dirfd");
        let (_server, _) = listener.accept().expect("accept");
    }
}
