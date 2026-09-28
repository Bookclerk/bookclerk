//! Host-created IPC links delivered by descriptor or handle inheritance.
//!
//! Native-behind-workerd siblings never open each other by name. The host
//! creates the guest RPC link and the multiplexed socket-proxy link, keeps
//! every end `CLOEXEC` / non-inheritable, and delivers the child ends.
//! On Unix both links are duplex sockets. On Windows each link is two
//! unidirectional pipes so a pending read cannot lock a write. Guest RPC
//! keeps synchronous guest ends (Rust std aborts on overlapped stdin). The
//! proxy mux uses overlapped ends on both peers.
//!
//! - **Unix:** [`inherit_fd_at`] from `Command::pre_exec` (`dup2` onto a fixed
//!   child fd, then close the source in that child). macOS has no
//!   `SOCK_CLOEXEC`; [`with_fd_spawn_lock`] covers allocation through
//!   `FD_CLOEXEC` and every in-process spawn. Concurrent spawns cannot inherit
//!   another session's link.
//! - **Windows:** [`duplicate_handle_into`] the jail process, then one bounded
//!   JSON [`JailHandoff`] line on the jail's stdin. The jail marks only those
//!   duplicates inheritable and puts them on `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
//!   Named pipes are created with a DACL for the creating user, SYSTEM, and
//!   Administrators. The guest uses the inherited handle, not the pipe name.
//!
//! Children name an inherited end with [`LinkSpec`] (`fd:3` / `handle:123`).

#![allow(unsafe_code)] // Unix socketpair/pipe/dup2; Windows CreateNamedPipe/DuplicateHandle.
#![allow(clippy::missing_docs_in_private_items)]

use std::fmt;
use std::io;

use serde::{Deserialize, Serialize};

/// Env var naming the guest Cap'n Proto link for `bookclerk-workerd`.
///
/// Unix: one duplex socket, `fd:<n>`. Windows: the gateway's overlapped read
/// half (guest stdout → gateway), `handle:<n>`. The write half is
/// [`GATEWAY_GUEST_RPC_WRITE_ENV`].
pub const GATEWAY_GUEST_RPC_ENV: &str = "BOOKCLERK_GATEWAY_GUEST_RPC";
/// Windows-only env var naming the gateway's overlapped write half of guest RPC.
///
/// `handle:<n>` writes into the guest's synchronous stdin. Absent on Unix,
/// where [`GATEWAY_GUEST_RPC_ENV`] is a full-duplex socket.
pub const GATEWAY_GUEST_RPC_WRITE_ENV: &str = "BOOKCLERK_GATEWAY_GUEST_RPC_WRITE";
/// Env var naming the multiplexed CONNECT-proxy link for `bookclerk-workerd`.
///
/// Unix: one duplex socket, `fd:<n>`. Windows: the gateway's overlapped read
/// half, `handle:<n>`. The write half is [`GATEWAY_PROXY_WRITE_ENV`].
pub const GATEWAY_PROXY_ENV: &str = "BOOKCLERK_GATEWAY_PROXY";
/// Windows-only env var naming the gateway's overlapped write half of the proxy.
///
/// Absent on Unix, where [`GATEWAY_PROXY_ENV`] is a full-duplex socket.
pub const GATEWAY_PROXY_WRITE_ENV: &str = "BOOKCLERK_GATEWAY_PROXY_WRITE";
/// Env var naming the socket-proxy link for a native guest (SDK `net::connect`).
///
/// Unix: one duplex socket, `fd:<n>`. Windows product spawns name the guest's
/// overlapped read half here; the write half is [`SOCKET_PROXY_WRITE_ENV`].
pub const SOCKET_PROXY_ENV: &str = "BOOKCLERK_SOCKET_PROXY";
/// Windows-only env var naming the guest's overlapped write half of the proxy.
///
/// The SDK still accepts a single `handle:<n>` duplex when this is unset
/// (unit tests). Product spawns set both halves so a pending read cannot lock
/// a write on the same pipe.
pub const SOCKET_PROXY_WRITE_ENV: &str = "BOOKCLERK_SOCKET_PROXY_WRITE";
/// When `1`, `bookclerk-jail` reads one [`JailHandoff`] JSON line from stdin.
pub const JAIL_HANDOFF_ENV: &str = "BOOKCLERK_JAIL_HANDOFF";
/// Host-chosen workerd session directory (`0700`, under `$TMPDIR`).
pub const WORKERD_STATE_DIR_ENV: &str = "BOOKCLERK_WORKERD_STATE_DIR";

/// Child fd the gateway uses for the guest RPC socket (`BOOKCLERK_GATEWAY_GUEST_RPC`).
pub const GATEWAY_RPC_FD: i32 = 3;
/// Child fd the gateway uses for the proxy mux (`BOOKCLERK_GATEWAY_PROXY`).
pub const GATEWAY_PROXY_FD: i32 = 4;
/// Child fd the native guest uses for the proxy mux (`BOOKCLERK_SOCKET_PROXY`).
pub const GUEST_PROXY_FD: i32 = 3;
/// Test-only extra endpoint in the native guest. Production preserve lists omit it.
pub const TEST_EXTRA_ENDPOINT_FD: i32 = 4;

/// Serialize macOS descriptor allocation with in-process `Command` spawns.
///
/// macOS has no `SOCK_CLOEXEC` or `pipe2`. Socket and pipe creation in this
/// crate holds the lock from allocation until `FD_CLOEXEC` is set. Every
/// `Command::spawn` in that process must hold it for the `spawn` call, or a
/// concurrent `fork` inherits the new descriptor. Linux `SOCK_CLOEXEC` /
/// `pipe2` are atomic, so this is a no-op there. Do not create a link while
/// already inside the lock: the mutex is not reentrant.
pub fn with_fd_spawn_lock<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(target_os = "macos")]
    {
        let _guard = fd_spawn_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f()
    }
    #[cfg(not(target_os = "macos"))]
    {
        f()
    }
}

/// Process-wide lock for [`with_fd_spawn_lock`].
#[cfg(target_os = "macos")]
fn fd_spawn_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

/// How a confined child names an inherited duplex or pipe end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkSpec {
    /// Unix inherited file descriptor (`fd:3`).
    Fd(i32),
    /// Windows inherited handle value (`handle:18446744073709551615`).
    Handle(u64),
}

/// Why a [`LinkSpec`] string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkSpecError {
    /// Missing `fd:` / `handle:` prefix or empty remainder.
    #[error("link spec must be `fd:<n>` or `handle:<n>`, got `{0}`")]
    Unrecognized(String),
    /// Prefix present but the number did not parse.
    #[error("link spec `{0}` has a non-numeric suffix")]
    BadNumber(String),
}

impl LinkSpec {
    /// Parse `fd:<n>` or `handle:<n>`.
    ///
    /// # Errors
    ///
    /// Returns [`LinkSpecError`] when the prefix is missing or the number is
    /// not a decimal `i32` / `u64`.
    pub fn parse(spec: &str) -> Result<Self, LinkSpecError> {
        if let Some(rest) = spec.strip_prefix("fd:") {
            let n = rest
                .parse()
                .map_err(|_| LinkSpecError::BadNumber(spec.to_string()))?;
            return Ok(Self::Fd(n));
        }
        if let Some(rest) = spec.strip_prefix("handle:") {
            let n = rest
                .parse()
                .map_err(|_| LinkSpecError::BadNumber(spec.to_string()))?;
            return Ok(Self::Handle(n));
        }
        Err(LinkSpecError::Unrecognized(spec.to_string()))
    }

    /// `fd:<n>` form used on Unix.
    #[must_use]
    pub fn fd(n: i32) -> Self {
        Self::Fd(n)
    }

    /// `handle:<n>` form used on Windows.
    #[must_use]
    pub fn handle(n: u64) -> Self {
        Self::Handle(n)
    }
}

impl fmt::Display for LinkSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fd(n) => write!(f, "fd:{n}"),
            Self::Handle(n) => write!(f, "handle:{n}"),
        }
    }
}

impl std::str::FromStr for LinkSpec {
    type Err = LinkSpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// One-shot Windows jail stdin line. Bounded JSON; never a general broker.
///
/// The host derives every handle from values it just duplicated with
/// `duplicate_handle_into` (Windows). The jail is the sole spawner and marks
/// them inheritable immediately before `CreateProcess`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JailHandoff {
    /// Wire version. Only `1` is accepted.
    pub v: u32,
    /// Guest stdin handle (RPC read end), when the jail should not create a pipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<u64>,
    /// Guest stdout handle (RPC write end), when the jail should not create a pipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<u64>,
    /// Extra inheritable handles exported as `env=handle:<value>`.
    #[serde(default)]
    pub extra: Vec<JailHandoffExtra>,
}

/// One extra inherited handle named in the child environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JailHandoffExtra {
    /// Environment variable the child should see (`BOOKCLERK_SOCKET_PROXY`, …).
    pub env: String,
    /// Handle value in the jail process (already duplicated).
    pub handle: u64,
}

impl JailHandoff {
    /// Current wire version.
    pub const VERSION: u32 = 1;

    /// Maximum accepted JSON line length (8 KiB). Larger input is a protocol error.
    pub const MAX_LINE_BYTES: usize = 8 * 1024;

    /// Encode as a single JSON line (no trailing newline; the caller writes `\n`).
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] when serde cannot encode the handoff.
    pub fn to_line(&self) -> io::Result<String> {
        serde_json::to_string(self).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    /// Parse one JSON line. Rejects `v != 1` and oversize input.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] when the line is too long, not JSON, or
    /// the version is not [`Self::VERSION`].
    pub fn from_line(line: &str) -> io::Result<Self> {
        if line.len() > Self::MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jail handoff line exceeds 8 KiB",
            ));
        }
        let parsed: Self = serde_json::from_str(line.trim())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if parsed.v != Self::VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported jail handoff version {}", parsed.v),
            ));
        }
        Ok(parsed)
    }
}

/// One end of a host-created duplex IPC link (socketpair / overlapped pipe).
#[derive(Debug)]
pub struct DuplexLink {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    #[cfg(not(any(unix, windows)))]
    _priv: (),
}

impl DuplexLink {
    /// Create a connected duplex pair. Both ends stay non-inheritable / `CLOEXEC`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the kernel cannot allocate the pair.
    pub fn pair() -> io::Result<(Self, Self)> {
        #[cfg(unix)]
        {
            unix::socketpair()
        }
        #[cfg(windows)]
        {
            windows::duplex_pair()
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "DuplexLink requires Unix or Windows",
            ))
        }
    }

    /// Duplicate this end. Both copies stay `CLOEXEC` / non-inheritable.
    ///
    /// Used so guest stdin and stdout can share one duplex without racing
    /// `dup` in the caller.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the kernel cannot duplicate the descriptor.
    pub fn try_clone(&self) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                fd: self.fd.try_clone()?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                handle: self.handle.try_clone()?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = self;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "DuplexLink requires Unix or Windows",
            ))
        }
    }

    /// Unix raw fd (still owned by `self`).
    #[cfg(unix)]
    #[must_use]
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }

    /// Consume this end as an [`std::os::fd::OwnedFd`].
    #[cfg(unix)]
    #[must_use]
    pub fn into_owned_fd(self) -> std::os::fd::OwnedFd {
        self.fd
    }

    /// Windows raw handle (still owned by `self`).
    #[cfg(windows)]
    #[must_use]
    pub fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::AsRawHandle;
        self.handle.as_raw_handle()
    }

    /// Consume this end as an [`std::os::windows::io::OwnedHandle`].
    #[cfg(windows)]
    #[must_use]
    pub fn into_owned_handle(self) -> std::os::windows::io::OwnedHandle {
        self.handle
    }

    /// Handle value for [`LinkSpec::Handle`] / [`JailHandoff`].
    #[cfg(windows)]
    #[must_use]
    pub fn handle_value(&self) -> u64 {
        self.as_raw_handle() as usize as u64
    }

    /// Mark this end inheritable. Product hosts never call this (they
    /// `DuplicateHandle` into `bookclerk-jail` instead). Unconfined tests pass
    /// a duplicate on `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `SetHandleInformation` fails.
    #[cfg(windows)]
    pub fn set_inheritable(&self, inherit: bool) -> io::Result<()> {
        windows::set_handle_inheritable(self.as_raw_handle(), inherit)
    }
}

/// Host and guest ends of a pair of unidirectional stdio pipes (RPC).
#[derive(Debug)]
pub struct StdioEnds {
    /// Parent writes this into the child's stdin.
    pub host_stdin: DuplexHalf,
    /// Parent reads this from the child's stdout.
    pub host_stdout: DuplexHalf,
    /// Child stdin read end.
    pub guest_stdin: DuplexHalf,
    /// Child stdout write end.
    pub guest_stdout: DuplexHalf,
}

/// One unidirectional pipe end (stdio).
#[derive(Debug)]
pub struct DuplexHalf {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    #[cfg(not(any(unix, windows)))]
    _priv: (),
}

impl StdioEnds {
    /// Two unidirectional pipes: host stdin-write / stdout-read, guest the opposite.
    ///
    /// Windows: host ends are overlapped; guest ends are synchronous (Rust std
    /// `Command` convention). All DACLs are owner-only.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when a pipe cannot be created.
    pub fn pair() -> io::Result<Self> {
        #[cfg(unix)]
        {
            unix::stdio_pair()
        }
        #[cfg(windows)]
        {
            windows::stdio_pair(false)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "StdioEnds requires Unix or Windows",
            ))
        }
    }

    /// Two unidirectional pipes whose guest ends are overlapped.
    ///
    /// The socket-proxy mux reads and writes at the same time from both
    /// peers. A single duplex pipe deadlocks that mux on Windows. Each end
    /// here is only read or only written, and every handle is overlapped so
    /// Tokio can wrap it. Guest stdio RPC uses [`pair`] instead, because Rust
    /// std aborts on an overlapped stdin.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when a pipe cannot be created.
    #[cfg(windows)]
    pub fn pair_overlapped() -> io::Result<Self> {
        windows::stdio_pair(true)
    }
}

impl DuplexHalf {
    /// Unix raw fd.
    #[cfg(unix)]
    #[must_use]
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }

    /// Consume as [`std::os::fd::OwnedFd`].
    #[cfg(unix)]
    #[must_use]
    pub fn into_owned_fd(self) -> std::os::fd::OwnedFd {
        self.fd
    }

    /// Windows raw handle.
    #[cfg(windows)]
    #[must_use]
    pub fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::AsRawHandle;
        self.handle.as_raw_handle()
    }

    /// Consume as [`std::os::windows::io::OwnedHandle`].
    #[cfg(windows)]
    #[must_use]
    pub fn into_owned_handle(self) -> std::os::windows::io::OwnedHandle {
        self.handle
    }

    /// Handle value for [`JailHandoff`].
    #[cfg(windows)]
    #[must_use]
    pub fn handle_value(&self) -> u64 {
        self.as_raw_handle() as usize as u64
    }

    /// Mark this end inheritable.
    ///
    /// Product hosts duplicate into `bookclerk-jail` instead of calling this.
    /// The authenticated-endpoint fixture uses it so a test child can inherit
    /// the pipe when `inherit` is set and cannot when it is cleared.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `SetHandleInformation` fails.
    #[cfg(windows)]
    pub fn set_inheritable(&self, inherit: bool) -> io::Result<()> {
        windows::set_handle_inheritable(self.as_raw_handle(), inherit)
    }
}

/// `dup2` `src` onto `dest` and clear `FD_CLOEXEC` on the destination.
///
/// Call from `Command::pre_exec` only. `src` stays `CLOEXEC` in the parent.
/// The child must close `src` when it differs from `dest` so the pre-dup
/// descriptor does not survive `exec`. After `dup2` the child sees `dest`
/// without `CLOEXEC`; the guest sets `FD_CLOEXEC` again when it adopts that
/// descriptor.
///
/// # Errors
///
/// Returns an I/O error when `dup2` or `fcntl` fails.
///
/// # Safety
///
/// Must run in the child after `fork` and before `exec`. `src` must be open.
#[cfg(unix)]
pub fn inherit_fd_at(src: std::os::fd::RawFd, dest: std::os::fd::RawFd) -> io::Result<()> {
    unix::inherit_fd_at(src, dest)
}

/// Duplicate `handle` into `target` without making it inheritable there.
///
/// The jail later marks the duplicate inheritable and lists it for
/// `CreateProcess`. The host never sets `HANDLE_FLAG_INHERIT` on its own copy.
///
/// # Errors
///
/// Returns an I/O error when `DuplicateHandle` fails.
#[cfg(windows)]
pub fn duplicate_handle_into(
    handle: std::os::windows::io::RawHandle,
    target: std::os::windows::io::RawHandle,
) -> io::Result<u64> {
    windows::duplicate_handle_into(handle, target)
}

/// Duplicate `handle` into this process. The copy is not inheritable.
///
/// # Errors
///
/// Returns an I/O error when `DuplicateHandle` fails.
#[cfg(windows)]
pub fn duplicate_handle_local(handle: std::os::windows::io::RawHandle) -> io::Result<u64> {
    windows::duplicate_handle_local(handle)
}

/// Owned duplicate of `handle` in this process. The copy is not inheritable.
///
/// # Errors
///
/// Returns an I/O error when `DuplicateHandle` fails.
#[cfg(windows)]
pub fn duplicate_owned_handle(
    handle: std::os::windows::io::RawHandle,
) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
    let value = duplicate_handle_local(handle)?;
    Ok(unsafe { OwnedHandle::from_raw_handle(value as usize as RawHandle) })
}

#[cfg(unix)]
#[allow(unsafe_code)] // socketpair, pipe, dup2, fcntl.
#[allow(clippy::missing_docs_in_private_items)]
mod unix {
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};

    use super::{DuplexHalf, DuplexLink, StdioEnds};

    pub(super) fn socketpair() -> io::Result<(DuplexLink, DuplexLink)> {
        allocate_cloexec(|| {
            let mut fds = [0 as RawFd; 2];
            // Linux: SOCK_CLOEXEC. macOS: SOCK_STREAM then FD_CLOEXEC (no
            // SOCK_CLOEXEC). The spawn lock covers that window.
            #[cfg(target_os = "linux")]
            let ty = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
            #[cfg(not(target_os = "linux"))]
            let ty = libc::SOCK_STREAM;
            let rc = unsafe { libc::socketpair(libc::AF_UNIX, ty, 0, fds.as_mut_ptr()) };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Err(err) = set_cloexec(fds[0]).and_then(|()| set_cloexec(fds[1])) {
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(err);
            }
            Ok((
                DuplexLink {
                    fd: unsafe { OwnedFd::from_raw_fd(fds[0]) },
                },
                DuplexLink {
                    fd: unsafe { OwnedFd::from_raw_fd(fds[1]) },
                },
            ))
        })
    }

    fn set_cloexec(fd: RawFd) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0 as RawFd; 2];
        // `pipe2` is Linux-only; macOS gets `pipe` + `FD_CLOEXEC` under the
        // spawn lock held by [`stdio_pair`].
        #[cfg(target_os = "linux")]
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        #[cfg(not(target_os = "linux"))]
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if let Err(err) = set_cloexec(fds[0]).and_then(|()| set_cloexec(fds[1])) {
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(err);
        }
        Ok((unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
            OwnedFd::from_raw_fd(fds[1])
        }))
    }

    /// Hold the macOS spawn lock across both pipes so a `fork` cannot split them.
    fn allocate_cloexec<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(target_os = "macos")]
        {
            super::with_fd_spawn_lock(f)
        }
        #[cfg(not(target_os = "macos"))]
        {
            f()
        }
    }

    pub(super) fn stdio_pair() -> io::Result<StdioEnds> {
        allocate_cloexec(|| {
            let (guest_stdin, host_stdin) = pipe_cloexec()?;
            let (host_stdout, guest_stdout) = pipe_cloexec()?;
            Ok(StdioEnds {
                host_stdin: DuplexHalf { fd: host_stdin },
                host_stdout: DuplexHalf { fd: host_stdout },
                guest_stdin: DuplexHalf { fd: guest_stdin },
                guest_stdout: DuplexHalf { fd: guest_stdout },
            })
        })
    }

    pub(super) fn inherit_fd_at(src: RawFd, dest: RawFd) -> io::Result<()> {
        if unsafe { libc::dup2(src, dest) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = unsafe { libc::fcntl(dest, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(dest, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
#[allow(clippy::missing_docs_in_private_items)]
mod windows {
    use std::io;
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr;

    use rand::RngCore;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, DuplicateHandle, SetHandleInformation, HANDLE, HANDLE_FLAGS,
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    use super::{DuplexHalf, DuplexLink, StdioEnds};

    fn random_pipe_name(kind: &str) -> String {
        let mut nonce = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut nonce);
        format!(r"\\.\pipe\bc-{kind}-{}", hex_encode(&nonce))
    }

    fn hex_encode(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// First-instance pipe with a creator/SYSTEM/Administrators DACL.
    fn create_named_pipe(
        name_w: &[u16],
        open_mode: FILE_FLAGS_AND_ATTRIBUTES,
    ) -> io::Result<HANDLE> {
        let mut security = crate::platform::windows_pipe::NamedPipeSecurity::for_creator()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let server = unsafe {
            CreateNamedPipeW(
                PCWSTR(name_w.as_ptr()),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                16 * 1024,
                16 * 1024,
                0,
                Some(security.as_security_attributes()),
            )
        };
        if server.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(server)
    }

    fn close_if_valid(handle: HANDLE) {
        if !handle.is_invalid() && handle != HANDLE::default() && handle != INVALID_HANDLE_VALUE {
            let _ = unsafe { CloseHandle(handle) };
        }
    }

    fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
        if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CreateNamedPipe/CreateFile returned an invalid handle",
            ));
        }
        // Owner-only DACL is the process default; do not mark inheritable.
        unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).map_err(
                |err| {
                    close_if_valid(handle);
                    io::Error::other(err)
                },
            )?;
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(handle.0 as RawHandle) })
    }

    /// True when `err` is the Win32 code the `windows` crate stored on `BOOL::ok`.
    ///
    /// That wrapper reads `GetLastError` and then calls `GetErrorInfo`, which can
    /// replace the thread's last-error. A later `GetLastError` is not the pipe
    /// status.
    fn is_win32(err: &windows::core::Error, code: u32) -> bool {
        err.code() == windows::core::HRESULT::from_win32(code)
    }

    /// Complete `ConnectNamedPipe` after the client `CreateFile` has connected.
    ///
    /// A synchronous server may pass a null overlapped pointer. An overlapped
    /// server must pass a real `OVERLAPPED`: a null pointer can report success
    /// while the instance is still listening, and later `ReadFile`/`WriteFile`
    /// then wait forever. `ERROR_PIPE_CONNECTED` means the client already
    /// attached, which is success.
    fn finish_connect(server: HANDLE, server_overlapped: bool) -> io::Result<()> {
        use windows::Win32::Foundation::{ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
        use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

        if !server_overlapped {
            return match unsafe { ConnectNamedPipe(server, None) } {
                Ok(()) => Ok(()),
                Err(err) if is_win32(&err, ERROR_PIPE_CONNECTED.0) => Ok(()),
                Err(err) => Err(io::Error::other(err)),
            };
        }

        let event =
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(io::Error::other)?;
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..OVERLAPPED::default()
        };
        let connected = unsafe { ConnectNamedPipe(server, Some(ptr::addr_of_mut!(overlapped))) };
        match connected {
            Ok(()) => {
                close_if_valid(event);
                return Ok(());
            }
            Err(err) if is_win32(&err, ERROR_PIPE_CONNECTED.0) => {
                close_if_valid(event);
                return Ok(());
            }
            Err(err) if is_win32(&err, ERROR_IO_PENDING.0) => {}
            Err(err) => {
                close_if_valid(event);
                return Err(io::Error::other(err));
            }
        }
        // The client handle already exists, so this should complete immediately.
        // Bound the wait so a stuck instance cannot wedge the host. Cancel
        // before returning: `overlapped` must not be dropped while I/O is pending.
        let wait = unsafe { WaitForSingleObject(event, 5_000) };
        if wait != WAIT_OBJECT_0 {
            let _ = unsafe { CancelIoEx(server, Some(ptr::addr_of!(overlapped))) };
            let mut transferred = 0u32;
            let _ = unsafe { GetOverlappedResult(server, &overlapped, &mut transferred, true) };
            close_if_valid(event);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "overlapped ConnectNamedPipe did not complete",
            ));
        }
        let mut transferred = 0u32;
        let result = unsafe { GetOverlappedResult(server, &overlapped, &mut transferred, false) };
        close_if_valid(event);
        match result {
            Ok(()) => Ok(()),
            Err(err) if is_win32(&err, ERROR_PIPE_CONNECTED.0) => Ok(()),
            Err(err) => Err(io::Error::other(err)),
        }
    }

    /// Duplex overlapped pipe; both ends `FILE_FLAG_OVERLAPPED`, max 1 instance.
    pub(super) fn duplex_pair() -> io::Result<(DuplexLink, DuplexLink)> {
        let name = random_pipe_name("l");
        let name_w = wide(&name);
        let server = create_named_pipe(
            &name_w,
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
        )?;
        let client = unsafe {
            CreateFileW(
                PCWSTR(name_w.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_GENERIC_READ.0
                    | windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        let client = match client {
            Ok(h) => h,
            Err(err) => {
                close_if_valid(server);
                return Err(io::Error::other(err));
            }
        };
        if let Err(err) = finish_connect(server, true) {
            close_if_valid(server);
            close_if_valid(client);
            return Err(err);
        }
        Ok((
            DuplexLink {
                handle: owned(server)?,
            },
            DuplexLink {
                handle: owned(client)?,
            },
        ))
    }

    fn unidirectional(
        host_writes: bool,
        server_overlapped: bool,
    ) -> io::Result<(OwnedHandle, OwnedHandle)> {
        let name = random_pipe_name(if host_writes { "si" } else { "so" });
        let name_w = wide(&name);
        let access = if host_writes {
            PIPE_ACCESS_INBOUND
        } else {
            PIPE_ACCESS_OUTBOUND
        };
        // Server is the guest end. Host end (CreateFile) is always overlapped.
        // Guest stdio keeps the server synchronous. The proxy mux sets
        // `server_overlapped` so both Tokio peers can use the pipe, one
        // direction per handle.
        let mode = if server_overlapped {
            access | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED
        } else {
            access | FILE_FLAG_FIRST_PIPE_INSTANCE
        };
        let server = create_named_pipe(&name_w, mode)?;
        let desired = if host_writes {
            windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0
        } else {
            windows::Win32::Storage::FileSystem::FILE_GENERIC_READ.0
        };
        let client = unsafe {
            CreateFileW(
                PCWSTR(name_w.as_ptr()),
                desired,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        let client = match client {
            Ok(h) => h,
            Err(err) => {
                close_if_valid(server);
                return Err(io::Error::other(err));
            }
        };
        if let Err(err) = finish_connect(server, server_overlapped) {
            close_if_valid(server);
            close_if_valid(client);
            return Err(err);
        }
        // server = guest, client = host (always overlapped)
        Ok((owned(client)?, owned(server)?))
    }

    pub(super) fn stdio_pair(server_overlapped: bool) -> io::Result<StdioEnds> {
        let (host_stdin, guest_stdin) = unidirectional(true, server_overlapped)?;
        let (host_stdout, guest_stdout) = unidirectional(false, server_overlapped)?;
        Ok(StdioEnds {
            host_stdin: DuplexHalf { handle: host_stdin },
            host_stdout: DuplexHalf {
                handle: host_stdout,
            },
            guest_stdin: DuplexHalf {
                handle: guest_stdin,
            },
            guest_stdout: DuplexHalf {
                handle: guest_stdout,
            },
        })
    }

    pub(super) fn set_handle_inheritable(handle: RawHandle, inherit: bool) -> io::Result<()> {
        let flags = if inherit {
            HANDLE_FLAG_INHERIT
        } else {
            HANDLE_FLAGS(0)
        };
        unsafe {
            SetHandleInformation(HANDLE(handle), HANDLE_FLAG_INHERIT.0, flags)
                .map_err(io::Error::other)
        }
    }

    pub(super) fn duplicate_handle_local(handle: RawHandle) -> io::Result<u64> {
        duplicate_handle_into(handle, unsafe { GetCurrentProcess() }.0 as RawHandle)
    }

    pub(super) fn duplicate_handle_into(handle: RawHandle, target: RawHandle) -> io::Result<u64> {
        let mut dest = HANDLE::default();
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(handle),
                HANDLE(target),
                &mut dest,
                0,
                false,
                windows::Win32::Foundation::DUPLICATE_SAME_ACCESS,
            )
            .map_err(io::Error::other)?;
        }
        Ok(dest.0 as usize as u64)
    }

    /// Anonymous open of the pipe name fails; the inherited handle still carries bytes.
    #[cfg(test)]
    pub(super) fn creator_dacl_blocks_anonymous_and_keeps_the_handle() -> io::Result<()> {
        use std::io::{Read, Write};
        use std::os::windows::io::FromRawHandle;
        use windows::core::PWSTR;
        use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_FILE_OBJECT,
        };
        use windows::Win32::Security::{
            ImpersonateAnonymousToken, RevertToSelf, DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR,
        };
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        };
        use windows::Win32::System::Threading::GetCurrentThread;

        let name = random_pipe_name("dacl");
        let name_w = wide(&name);
        let server =
            create_named_pipe(&name_w, PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE)?;
        let mut psd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        let info = unsafe {
            GetSecurityInfo(
                server,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                Some(&mut psd),
            )
        };
        if info.0 != 0 {
            close_if_valid(server);
            return Err(io::Error::other(format!("GetSecurityInfo {info:?}")));
        }
        let mut sddl_wide = PWSTR::null();
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                psd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut sddl_wide,
                None,
            )
        };
        let sddl = if converted.is_ok() && !sddl_wide.is_null() {
            unsafe { sddl_wide.to_string().unwrap_or_default() }
        } else {
            String::new()
        };
        if !sddl_wide.is_null() {
            use windows::Win32::Foundation::{LocalFree, HLOCAL};
            unsafe {
                let _ = LocalFree(Some(HLOCAL(sddl_wide.as_ptr().cast())));
            }
        }
        if !psd.0.is_null() {
            use windows::Win32::Foundation::{LocalFree, HLOCAL};
            unsafe {
                let _ = LocalFree(Some(HLOCAL(psd.0)));
            }
        }
        for forbidden in ["WD", "AN", "AC", "S-1-1-0", "S-1-5-7", "S-1-15-2"] {
            if sddl.contains(forbidden) {
                close_if_valid(server);
                return Err(io::Error::other(format!(
                    "pipe DACL grants {forbidden}: {sddl}"
                )));
            }
        }
        let names_system = sddl.contains("SY") || sddl.contains("S-1-5-18");
        let names_admins = sddl.contains("BA") || sddl.contains("S-1-5-32-544");
        if !names_system || !names_admins {
            close_if_valid(server);
            return Err(io::Error::other(format!(
                "pipe DACL must name SYSTEM and Administrators: {sddl}"
            )));
        }

        struct RevertOnDrop;
        impl Drop for RevertOnDrop {
            fn drop(&mut self) {
                unsafe {
                    let _ = RevertToSelf();
                }
            }
        }
        unsafe {
            ImpersonateAnonymousToken(GetCurrentThread()).map_err(|err| {
                close_if_valid(server);
                io::Error::other(format!("ImpersonateAnonymousToken: {err}"))
            })?;
        }
        let _revert = RevertOnDrop;
        let anonymous = unsafe {
            CreateFileW(
                PCWSTR(name_w.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        drop(_revert);
        match anonymous {
            Ok(handle) => {
                close_if_valid(handle);
                close_if_valid(server);
                return Err(io::Error::other("anonymous token opened the pipe by name"));
            }
            Err(err) => {
                if !is_win32(&err, ERROR_ACCESS_DENIED.0) {
                    close_if_valid(server);
                    return Err(io::Error::other(format!(
                        "anonymous open failed with {err}, expected ACCESS_DENIED"
                    )));
                }
            }
        }

        let client = unsafe {
            CreateFileW(
                PCWSTR(name_w.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|err| {
            close_if_valid(server);
            io::Error::other(format!("creator open: {err}"))
        })?;
        if let Err(err) = finish_connect(server, false) {
            close_if_valid(server);
            close_if_valid(client);
            return Err(err);
        }
        let mut server_file = unsafe { std::fs::File::from_raw_handle(server.0 as RawHandle) };
        let mut client_file = unsafe { std::fs::File::from_raw_handle(client.0 as RawHandle) };
        client_file
            .write_all(b"mux")
            .map_err(|err| io::Error::other(format!("inherited client write: {err}")))?;
        let mut buf = [0u8; 3];
        server_file.read_exact(&mut buf)?;
        if &buf != b"mux" {
            return Err(io::Error::other(
                "inherited handle did not carry the mux bytes",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_spec_round_trips() {
        assert_eq!(LinkSpec::parse("fd:3").unwrap(), LinkSpec::Fd(3));
        assert_eq!(LinkSpec::parse("handle:42").unwrap(), LinkSpec::Handle(42));
        assert_eq!(LinkSpec::fd(4).to_string(), "fd:4");
        assert_eq!(LinkSpec::handle(7).to_string(), "handle:7");
        assert!(matches!(
            LinkSpec::parse("/tmp/sockets.sock"),
            Err(LinkSpecError::Unrecognized(_))
        ));
        assert!(matches!(
            LinkSpec::parse("fd:nope"),
            Err(LinkSpecError::BadNumber(_))
        ));
    }

    #[test]
    fn jail_handoff_round_trips_and_rejects_bad_version() {
        let handoff = JailHandoff {
            v: 1,
            stdin: Some(11),
            stdout: Some(12),
            extra: vec![JailHandoffExtra {
                env: SOCKET_PROXY_ENV.into(),
                handle: 13,
            }],
        };
        let line = handoff.to_line().expect("encode");
        assert!(line.len() < JailHandoff::MAX_LINE_BYTES);
        assert_eq!(JailHandoff::from_line(&line).expect("decode"), handoff);
        let err = JailHandoff::from_line(r#"{"v":2}"#).expect_err("v=2");
        assert!(err.to_string().contains("version"), "{err}");
        let huge = "x".repeat(JailHandoff::MAX_LINE_BYTES + 1);
        assert!(JailHandoff::from_line(&huge).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn duplex_link_echoes() {
        use std::io::{Read, Write};

        let (a, b) = DuplexLink::pair().expect("pair");
        for fd in [a.as_raw_fd(), b.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(
                flags >= 0 && flags & libc::FD_CLOEXEC != 0,
                "link ends must be CLOEXEC before any spawn"
            );
        }
        let mut a = std::fs::File::from(a.into_owned_fd());
        let mut b = std::fs::File::from(b.into_owned_fd());
        a.write_all(b"ping").expect("write");
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"ping");
    }

    #[cfg(unix)]
    #[test]
    fn stdio_ends_are_unidirectional() {
        use std::io::{Read, Write};

        let ends = StdioEnds::pair().expect("stdio");
        let mut host_in = std::fs::File::from(ends.host_stdin.into_owned_fd());
        let mut guest_in = std::fs::File::from(ends.guest_stdin.into_owned_fd());
        host_in.write_all(b"rpc").expect("host stdin");
        let mut buf = [0u8; 3];
        guest_in.read_exact(&mut buf).expect("guest stdin");
        assert_eq!(&buf, b"rpc");

        let mut guest_out = std::fs::File::from(ends.guest_stdout.into_owned_fd());
        let mut host_out = std::fs::File::from(ends.host_stdout.into_owned_fd());
        guest_out.write_all(b"ok!").expect("guest stdout");
        host_out.read_exact(&mut buf).expect("host stdout");
        assert_eq!(&buf, b"ok!");
    }

    #[cfg(unix)]
    #[test]
    fn inherit_fd_at_clears_cloexec_on_the_destination() {
        use std::io::{Read, Write};
        use std::os::fd::FromRawFd;

        let (a, b) = DuplexLink::pair().expect("pair");
        let dest = 64;
        inherit_fd_at(a.as_raw_fd(), dest).expect("dup2");
        drop(a);
        let mut dest_file = unsafe { std::fs::File::from_raw_fd(dest) };
        let mut peer = std::fs::File::from(b.into_owned_fd());
        peer.write_all(b"hi").expect("write");
        let mut buf = [0u8; 2];
        dest_file.read_exact(&mut buf).expect("read dest");
        assert_eq!(&buf, b"hi");
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_dacl_rejects_anonymous_and_keeps_inherited_bytes() {
        windows::creator_dacl_blocks_anonymous_and_keeps_the_handle().expect("creator DACL");
    }
}
