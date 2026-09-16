//! Unix-domain listeners that stay under `sockaddr_un` limits.
//!
//! Cargo sets `TMPDIR` to workspace `.tmp` (see `.cargo/config.toml`).
//! On GitHub Actions that prefix is already ~42 bytes
//! (`/home/runner/work/bookclerk/bookclerk/.tmp`). Plugin scratch adds
//! `plugin-state/<fs_id>/tmp/<workerd-leaf>/sockets.sock`, which overflows
//! Linux's 108-byte `sun_path` (std rejects the path *before* `bind(2)` with
//! "path must be shorter than SUN_LEN"). Connect has the same limit, so a
//! `/proc/self/fd` bind trick is not enough for peers that still pass the
//! absolute path.
//!
//! Linux GRANTED (workerd ↔ launcher) stays `unix-abstract:` — workerd is not
//! inside the nested native Landlock domain. The native TCP proxy cannot: a
//! pathname under `state_dir` named as `/proc/self/fd/{n}/sockets.sock` stays
//! under `sun_path`; Isolation::Off guests inherit directory fd `{n}`.
//! Other Unix: bind a pathname under `state_dir` and pass a **relative**
//! `unix:granted.sock` to workerd (cwd is `state_dir`).
//!
//! Do not fall back to `std::env::temp_dir()`: inside `bookclerk-workerd` that
//! is the guest scratch (`TMPDIR`), which is the same overflowing prefix.

#![allow(clippy::missing_docs_in_private_items)]

use std::fs::File;
use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

use rand::RngCore;

#[cfg(not(target_os = "linux"))]
use std::sync::Mutex;

/// Prefix kept for SDK tests and non-Linux fallbacks.
///
/// Production Linux `BOOKCLERK_SOCKET_PROXY` is a `/proc/self/fd/` pathname, not
/// this prefix.
pub const SOCKET_PROXY_ABSTRACT_PREFIX: &str = "abstract:";

/// Bound native TCP proxy and the directory fd the nested guest must inherit.
pub struct BoundSocketProxy {
    /// Value for [`crate::socket_proxy::SOCKET_PROXY_ENV`].
    pub spec: String,
    /// Listener the launcher serves HTTP CONNECT on.
    pub listener: UnixListener,
    /// Open `state_dir`; keep until the nested guest is spawned, `O_CLOEXEC` off.
    pub inherit_dir: Option<File>,
}

/// Pathname leaf for the GRANTED channel on non-Linux Unix.
pub const GRANTED_SOCK_FILE: &str = "granted.sock";

/// Pathname leaf for the native TCP proxy on non-Linux Unix.
pub const SOCKET_PROXY_SOCK_FILE: &str = "sockets.sock";

/// workerd / KJ address for a relative GRANTED pathname (cwd = state dir).
pub const GRANTED_RELATIVE_ADDR: &str = "unix:granted.sock";

/// Serializes `chdir` around pathname binds that overflow `sun_path`.
#[cfg(not(target_os = "linux"))]
static CHDIR_BIND: Mutex<()> = Mutex::new(());

/// Clears `FD_CLOEXEC` so a spawned child inherits `fd`.
///
/// Used for the nested guest's socket-proxy directory fd.
///
/// # Errors
///
/// Returns when `fcntl` `F_GETFD` / `F_SETFD` fails.
#[allow(unsafe_code)]
pub fn clear_cloexec(fd: std::os::fd::RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
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

/// Binds the native-behind-workerd TCP proxy.
///
/// Linux: pathname socket in `state_dir`, advertised as `/proc/self/fd/{n}/…`
/// so `sun_path` cannot overflow Cargo's long `TMPDIR`. The nested guest
/// inherits directory fd `{n}` (see [`BoundSocketProxy::inherit_dir`]).
/// Other Unix: absolute pathname (callers keep the leaf short).
///
/// # Errors
///
/// Returns an I/O error when `state_dir` cannot be opened or the listener
/// cannot be bound.
pub fn bind_socket_proxy(state_dir: &Path) -> io::Result<BoundSocketProxy> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join(SOCKET_PROXY_SOCK_FILE);
        let _ = std::fs::remove_file(&path);
        let dir = File::open(state_dir)?;
        let spec = format!("/proc/self/fd/{}/{SOCKET_PROXY_SOCK_FILE}", dir.as_raw_fd());
        let listener = UnixListener::bind(&spec)?;
        Ok(BoundSocketProxy {
            spec,
            listener,
            inherit_dir: Some(dir),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path = state_dir.join(SOCKET_PROXY_SOCK_FILE);
        let listener = bind_pathname(&path)?;
        Ok(BoundSocketProxy {
            spec: path.to_string_lossy().into_owned(),
            listener,
            inherit_dir: None,
        })
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

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn bind_socket_proxy_uses_proc_fd_pathname() {
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().expect("tmpdir");
        let proxy = bind_socket_proxy(dir.path()).expect("proxy");
        assert!(
            proxy.spec.starts_with("/proc/self/fd/"),
            "BOOKCLERK_SOCKET_PROXY must be a /proc/self/fd pathname, got {}",
            proxy.spec
        );
        assert!(
            !proxy.spec.starts_with(SOCKET_PROXY_ABSTRACT_PREFIX),
            "abstract SOCKET_PROXY is the wrong shape: {}",
            proxy.spec
        );
        assert!(
            proxy.spec.len() < 108,
            "proc-fd proxy path must stay under sun_path: {}",
            proxy.spec
        );
        let inherit = proxy.inherit_dir.as_ref().expect("inherit dir fd");
        assert_eq!(
            proxy.spec,
            format!(
                "/proc/self/fd/{}/{SOCKET_PROXY_SOCK_FILE}",
                inherit.as_raw_fd()
            )
        );
        let _client = UnixStream::connect(&proxy.spec).expect("connect via proc fd");
        let (_server, _) = proxy.listener.accept().expect("accept");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bind_socket_proxy_survives_github_actions_sun_len() {
        let base = tempfile::tempdir().expect("tmpdir");
        let long = base
            .path()
            .join("gha-sunlen")
            .join("x".repeat(48))
            .join("plugin-state/pk-6fbbb9ca1420cebb/tmp")
            .join("we940186f8b9f");
        std::fs::create_dir_all(&long).expect("mkdir");
        let absolute = long.join(SOCKET_PROXY_SOCK_FILE);
        assert!(
            absolute.as_os_str().len() >= 108,
            "fixture must overflow sun_path: {} ({})",
            absolute.display(),
            absolute.as_os_str().len()
        );
        let proxy = bind_socket_proxy(&long).expect("proxy on overflowing state_dir");
        assert!(
            proxy.spec.len() < 108,
            "advertised spec overflowed sun_path: {}",
            proxy.spec
        );
        let _client = UnixStream::connect(&proxy.spec).expect("connect");
        let (_server, _) = proxy.listener.accept().expect("accept");
    }

    #[test]
    fn github_actions_cargo_tmpdir_overflows_sockaddr_un() {
        // Path from CI on 4043a3c8 (postgres LIKE spawn):
        // bind socket proxy .../.tmp/.tmpOjHB9L/plugin-state/pk-…/tmp/we…/sockets.sock
        let granted = Path::new("/home/runner/work/bookclerk/bookclerk/.tmp/.tmpOjHB9L")
            .join("plugin-state/pk-6fbbb9ca1420cebb/tmp")
            .join("we940186f8b9f")
            .join(SOCKET_PROXY_SOCK_FILE);
        let len = granted.to_string_lossy().len();
        assert!(
            len >= 108,
            "fixture must model the GHA overflow ({len} bytes): {}",
            granted.display()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_process_connects_through_inherited_dir_fd() {
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};
        use std::thread;

        let dir = tempfile::tempdir().expect("tmpdir");
        let proxy = bind_socket_proxy(dir.path()).expect("proxy");
        let inherit = proxy.inherit_dir.as_ref().expect("dir");
        clear_cloexec(inherit.as_raw_fd()).expect("clear CLOEXEC");
        let spec = proxy.spec.clone();
        let listener = proxy.listener.try_clone().expect("clone listener");
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept");
        });
        let output = Command::new("python3")
            .args([
                "-c",
                &format!("import socket; s=socket.socket(socket.AF_UNIX); s.connect({spec:?})"),
            ])
            .stdin(Stdio::null())
            .output()
            .expect("python3");
        assert!(
            output.status.success(),
            "child connect {spec}: status={:?} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        server.join().expect("server");
    }

    #[cfg(target_os = "linux")]
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
