//! Host-owned OAuth callback TCP listener + IPC byte tunnel to the guest.
//!
//! Browser → host `TcpListener` → multiplexed tunnel → guest LoginServer.
//! Required on Windows AppContainer (host↔guest loopback is blocked); used on
//! all OSes for a uniform plugin contract.

#![cfg_attr(windows, allow(unsafe_code))] // CreateNamedPipe SECURITY_ATTRIBUTES

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use bookclerk_plugin_sdk::TunnelHost;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
#[cfg(windows)]
use uuid::Uuid;

use crate::{PluginError, Result};

/// Active host callback proxy for one OAuth session.
pub struct CallbackProxy {
    /// Browser-facing origin (`http://127.0.0.1:<port>`) advertised to the guest.
    pub public_base: String,
    /// Unix socket path or Windows pipe name the guest LoginServer connects to.
    pub ipc_endpoint: String,
    /// Actual TCP address of the host callback listener (may be port 0 resolved).
    bind_addr: SocketAddr,
    /// Unix socket path removed on drop; `None` on Windows named pipes.
    _cleanup: Option<PathBuf>,
    /// Accept/forward task aborted when the proxy is dropped.
    join: Option<tokio::task::JoinHandle<()>>,
}

impl CallbackProxy {
    /// Bind the browser TCP listener and the guest IPC endpoint, then spawn
    /// the accept/forward loop.
    ///
    /// # Errors
    ///
    /// Returns when `callback_bind` is not a socket address, the TCP listener
    /// cannot bind, the scratch directory cannot be created, or the platform
    /// IPC endpoint cannot be created.
    pub async fn start(
        callback_bind: Option<&str>,
        scratch: &Path,
        package_sid: Option<&str>,
    ) -> Result<Self> {
        let tcp_addr: SocketAddr = callback_bind
            .unwrap_or("127.0.0.1:0")
            .parse()
            .map_err(|err| PluginError::message(format!("callback_bind: {err}")))?;
        let tcp = TcpListener::bind(tcp_addr)
            .await
            .map_err(|err| PluginError::message(format!("callback TCP bind {tcp_addr}: {err}")))?;
        let bound = tcp
            .local_addr()
            .map_err(|err| PluginError::message(format!("callback TCP local_addr: {err}")))?;
        let host = if bound.ip().is_unspecified() {
            "127.0.0.1".to_string()
        } else {
            bound.ip().to_string()
        };
        let public_base = format!("http://{host}:{}", bound.port());

        std::fs::create_dir_all(scratch).map_err(|err| {
            PluginError::message(format!("callback IPC scratch {}: {err}", scratch.display()))
        })?;

        #[cfg(unix)]
        {
            let _ = package_sid;
            start_unix(tcp, bound, public_base, scratch).await
        }
        #[cfg(windows)]
        {
            start_windows(tcp, bound, public_base, package_sid).await
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (tcp, public_base, package_sid);
            Err(PluginError::message(
                "callback IPC unsupported on this platform",
            ))
        }
    }

    #[must_use]
    /// Bound TCP address the browser (and guest) should use for the callback.
    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }
}

impl Drop for CallbackProxy {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            join.abort();
        }
        if let Some(path) = self._cleanup.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Longest socket file the guest IPC directory is validated to hold.
///
/// [`bookclerk_sandbox::ensure_guest_ipc_fits`] budgets for `.s.PGSQL.65535`.
/// A callback name longer than that can miss `sockaddr_un` on macOS even when
/// the directory itself was accepted.
#[cfg(unix)]
const GUEST_IPC_LONGEST_SOCKET_NAME: &str = ".s.PGSQL.65535";

/// How many distinct filenames to try before the callback bind fails.
#[cfg(unix)]
const CALLBACK_BIND_ATTEMPTS: usize = 8;

/// Mixes a process-local counter into each callback socket filename.
#[cfg(unix)]
static CALLBACK_SOCKET_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// `path` does not fit in `sockaddr_un`. The message names the length and the directory.
#[cfg(unix)]
fn unix_socket_path_too_long(path: &Path) -> Option<String> {
    let capacity = if cfg!(target_os = "macos") {
        bookclerk_sandbox::MACOS_SUN_PATH_CAPACITY
    } else {
        108
    };
    let len = path.as_os_str().len();
    if len >= capacity {
        let dir = path.parent().unwrap_or(path);
        Some(format!(
            "callback socket path length {len} does not fit sockaddr_un capacity {capacity}; directory {} is too long",
            dir.display()
        ))
    } else {
        None
    }
}

/// Returns one fresh `cb` + 8 hex-digit filename.
///
/// The result is 10 bytes, which is within [`GUEST_IPC_LONGEST_SOCKET_NAME`].
#[cfg(unix)]
fn callback_socket_name() -> String {
    use std::sync::atomic::Ordering;

    let n = CALLBACK_SOCKET_NONCE.fetch_add(1, Ordering::Relaxed);
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let token = n.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ tick ^ u64::from(std::process::id());
    let name = format!("cb{:08x}", token as u32);
    debug_assert_eq!(name.len(), 10);
    debug_assert!(name.len() <= GUEST_IPC_LONGEST_SOCKET_NAME.len());
    name
}

/// Yields [`CALLBACK_BIND_ATTEMPTS`] filenames from [`callback_socket_name`].
#[cfg(unix)]
fn callback_socket_names() -> impl Iterator<Item = String> {
    std::iter::repeat_with(callback_socket_name).take(CALLBACK_BIND_ATTEMPTS)
}

/// `err` means this filename is already present, so another candidate is required.
#[cfg(unix)]
fn unix_socket_name_occupied(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AlreadyExists
    )
}

/// Binds one mode `0600` callback socket under `scratch`.
///
/// Each `names` entry is a file in that directory. An occupied candidate is
/// left in place. The filename must be no longer than
/// [`GUEST_IPC_LONGEST_SOCKET_NAME`].
///
/// # Errors
///
/// Returns when a candidate is not a single path segment within the guest IPC
/// filename budget, the absolute path does not fit `sockaddr_un`, the bind
/// fails for a reason other than an occupied name, or every candidate is
/// occupied.
#[cfg(unix)]
fn bind_callback_socket(
    scratch: &Path,
    names: impl IntoIterator<Item = String>,
) -> Result<(tokio::net::UnixListener, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let mut tried = 0usize;
    let mut last_occupied: Option<(PathBuf, std::io::Error)> = None;
    for name in names {
        tried += 1;
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.len() > GUEST_IPC_LONGEST_SOCKET_NAME.len()
            || name.contains('/')
            || name.contains('\\')
        {
            return Err(PluginError::message(format!(
                "callback socket name `{name}` exceeds the guest IPC filename budget of {GUEST_IPC_LONGEST_SOCKET_NAME}"
            )));
        }
        let path = scratch.join(&name);
        if let Some(err) = unix_socket_path_too_long(&path) {
            return Err(PluginError::message(err));
        }
        match UnixListener::bind(&path) {
            Ok(listener) => {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                return Ok((listener, path));
            }
            Err(err) if unix_socket_name_occupied(&err) => {
                // Leave the existing inode in place. Unlinking it would
                // disconnect the proxy that already bound this name.
                last_occupied = Some((path, err));
            }
            Err(err) => {
                return Err(PluginError::message(format!(
                    "callback Unix bind {}: {err}",
                    path.display()
                )));
            }
        }
    }
    Err(match last_occupied {
        Some((path, err)) => PluginError::message(format!(
            "callback Unix bind in {} failed after {tried} occupied names (last {}): {err}",
            scratch.display(),
            path.display()
        )),
        None => PluginError::message(format!(
            "callback Unix bind in {} had no socket name candidates",
            scratch.display()
        )),
    })
}

#[cfg(unix)]
/// Binds a 0600 unix socket under `scratch` and forwards accepted TCP streams through it.
async fn start_unix(
    tcp: TcpListener,
    bound: SocketAddr,
    public_base: String,
    scratch: &Path,
) -> Result<CallbackProxy> {
    let (listener, path) = bind_callback_socket(scratch, callback_socket_names())?;
    let ipc_endpoint = path.display().to_string();
    let join = tokio::spawn(async move {
        let Ok((ipc, _)) = listener.accept().await else {
            tracing::warn!("callback Unix accept failed");
            return;
        };
        run_forward_loop(tcp, ipc).await;
    });

    Ok(CallbackProxy {
        public_base,
        ipc_endpoint,
        bind_addr: bound,
        _cleanup: Some(path),
        join: Some(join),
    })
}

/// Starts the Windows named-pipe OAuth callback forwarder.
///
/// # Errors
///
/// Returns when the pipe cannot be created or the AppContainer DACL cannot
/// be applied.
#[cfg(windows)]
async fn start_windows(
    tcp: TcpListener,
    bound: SocketAddr,
    public_base: String,
    package_sid: Option<&str>,
) -> Result<CallbackProxy> {
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

    let name = format!(r"\\.\pipe\bookclerk-oauth-{}", Uuid::new_v4().simple());
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .pipe_mode(PipeMode::Byte);

    let server = if let Some(sid) = package_sid {
        // Package SID DACL + Low mandatory label so the AppContainer guest can
        // open the pipe; default CreateNamedPipe DACLs deny Package SIDs.
        let mut sec = bookclerk_sandbox::spawn::NamedPipeSecurity::for_app_container(sid)
            .map_err(|err| PluginError::message(format!("callback pipe ACL for {sid}: {err}")))?;
        // SAFETY: `sec` owns a valid SECURITY_ATTRIBUTES until this block ends;
        // CreateNamedPipe copies the descriptor onto the pipe object.
        unsafe { options.create_with_security_attributes_raw(&name, sec.as_mut_ptr()) }
    } else {
        options.create(&name)
    }
    .map_err(|err| PluginError::message(format!("callback pipe create {name}: {err}")))?;
    let ipc_endpoint = name;
    let join = tokio::spawn(async move {
        if tokio::time::timeout(Duration::from_secs(120), server.connect())
            .await
            .is_err()
        {
            tracing::warn!("callback pipe connect timed out");
            return;
        }
        run_forward_loop(tcp, server).await;
    });

    Ok(CallbackProxy {
        public_base,
        ipc_endpoint,
        bind_addr: bound,
        _cleanup: None,
        join: Some(join),
    })
}

/// Accepts browser TCP connections and copies each bidirectionally over the guest tunnel.
async fn run_forward_loop<S>(tcp: TcpListener, ipc: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let (reader, writer) = tokio::io::split(ipc);
    let host_tunnel = TunnelHost::new(reader, writer);
    loop {
        let Ok((mut tcp_stream, _)) = tcp.accept().await else {
            break;
        };
        let Ok(mut tunnel_stream) = host_tunnel.open().await else {
            let _ = tcp_stream.shutdown().await;
            continue;
        };
        tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut tcp_stream, &mut tunnel_stream).await;
            let _ = tunnel_stream.shutdown().await;
        });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use bookclerk_plugin_sdk::TunnelGuest;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        bind_callback_socket, callback_socket_name, CallbackProxy, GUEST_IPC_LONGEST_SOCKET_NAME,
    };

    /// Generated callback filenames stay within the macOS socket-name budget.
    #[test]
    fn callback_socket_names_stay_within_the_guest_ipc_budget() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            let name = callback_socket_name();
            assert!(
                name.len() <= GUEST_IPC_LONGEST_SOCKET_NAME.len(),
                "{name} is longer than {GUEST_IPC_LONGEST_SOCKET_NAME}"
            );
            assert_eq!(name.len(), 10, "{name}");
            assert!(name.starts_with("cb"), "{name}");
            assert!(
                seen.insert(name.clone()),
                "duplicate callback socket name {name}"
            );
        }
    }

    /// A filename past `.s.PGSQL.65535` is rejected before any bind.
    #[test]
    fn callback_socket_rejects_a_name_past_the_filename_budget() {
        let dir = tempfile::tempdir().expect("dir");
        let occupied = dir.path().join("cb00000001");
        std::fs::write(&occupied, b"keep").expect("occupy");
        let long = "c".repeat(GUEST_IPC_LONGEST_SOCKET_NAME.len() + 1);
        let err = bind_callback_socket(dir.path(), [long, "cb00000002".to_string()])
            .expect_err("long name");
        assert!(err.to_string().contains("filename budget"), "{err}");
        assert_eq!(std::fs::read(&occupied).expect("occupied file"), b"keep");
        assert!(!dir.path().join("cb00000002").exists());
    }

    /// An absolute path that cannot fit `sockaddr_un` fails with the length and directory.
    #[test]
    fn callback_socket_rejects_a_directory_that_cannot_fit_sockaddr_un() {
        let dir = PathBuf::from(format!("/tmp/{}", "a".repeat(120)));
        let err = bind_callback_socket(&dir, std::iter::once("cb00000001".to_string()))
            .expect_err("long directory");
        let text = err.to_string();
        assert!(text.contains("too long"), "{text}");
        assert!(text.contains(&dir.display().to_string()), "{text}");
    }

    /// Binding skips an occupied name and leaves that listener's inode in place.
    #[tokio::test]
    async fn occupied_callback_socket_name_is_left_in_place() {
        let dir = tempfile::tempdir().expect("dir");
        let taken = dir.path().join("cb00000001");
        let listener = tokio::net::UnixListener::bind(&taken).expect("occupy");
        let accept = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("original accept");
            let mut buf = [0u8; 4];
            sock.read_exact(&mut buf).await.expect("original read");
            assert_eq!(&buf, b"keep");
        });

        let (bound, path) = bind_callback_socket(
            dir.path(),
            ["cb00000001".to_string(), "cb00000002".to_string()],
        )
        .expect("retry bind");
        assert_eq!(path, dir.path().join("cb00000002"));
        let mode = std::fs::metadata(&path)
            .expect("new socket")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let mut client = tokio::net::UnixStream::connect(&taken)
            .await
            .expect("connect occupied socket");
        client
            .write_all(b"keep")
            .await
            .expect("write occupied socket");
        tokio::time::timeout(Duration::from_secs(5), accept)
            .await
            .expect("occupied socket owner timed out")
            .expect("accept task");
        drop(bound);
        drop(client);
    }

    /// Two proxies in one directory own distinct sockets, and either drop leaves the other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_live_unix_callback_proxies_keep_distinct_endpoints() {
        let dir = tempfile::tempdir().expect("dir");
        for drop_first in [true, false] {
            let (first, second) = tokio::join!(
                CallbackProxy::start(None, dir.path(), None),
                CallbackProxy::start(None, dir.path(), None),
            );
            let first = first.expect("first proxy");
            let second = second.expect("second proxy");
            assert_ne!(first.ipc_endpoint, second.ipc_endpoint);

            let first_path = PathBuf::from(&first.ipc_endpoint);
            let second_path = PathBuf::from(&second.ipc_endpoint);
            assert_eq!(first_path.parent(), Some(dir.path()));
            assert_eq!(second_path.parent(), Some(dir.path()));
            for path in [&first_path, &second_path] {
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .expect("name");
                assert!(
                    name.len() <= GUEST_IPC_LONGEST_SOCKET_NAME.len(),
                    "{name} exceeds {GUEST_IPC_LONGEST_SOCKET_NAME}"
                );
                let mode = std::fs::metadata(path)
                    .expect("socket")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "{}", path.display());
            }

            let mut first_guest = attach_guest(&first.ipc_endpoint).await;
            let mut second_guest = attach_guest(&second.ipc_endpoint).await;
            exchange(&first, &mut first_guest, b"owner-a").await;
            exchange(&second, &mut second_guest, b"owner-b").await;

            if drop_first {
                drop(first);
                assert!(!first_path.exists(), "dropped proxy left its socket behind");
                assert!(second_path.exists(), "survivor socket was removed");
                exchange(&second, &mut second_guest, b"alive-b").await;
                drop(second);
                assert!(!second_path.exists());
            } else {
                drop(second);
                assert!(
                    !second_path.exists(),
                    "dropped proxy left its socket behind"
                );
                assert!(first_path.exists(), "survivor socket was removed");
                exchange(&first, &mut first_guest, b"alive-a").await;
                drop(first);
                assert!(!first_path.exists());
            }
        }
    }

    /// Connects a guest tunnel to `endpoint`.
    async fn attach_guest(endpoint: &str) -> TunnelGuest {
        let stream = tokio::net::UnixStream::connect(endpoint)
            .await
            .unwrap_or_else(|err| panic!("guest connect {endpoint}: {err}"));
        let (reader, writer) = tokio::io::split(stream);
        TunnelGuest::new(reader, writer)
    }

    /// Checks that bytes written to `proxy`'s browser listener arrive on `guest`.
    async fn exchange(proxy: &CallbackProxy, guest: &mut TunnelGuest, payload: &[u8]) {
        let tcp = tokio::net::TcpStream::connect(proxy.bind_addr());
        let accepted = guest.accept();
        let (tcp, accepted) = tokio::join!(tcp, accepted);
        let mut tcp =
            tcp.unwrap_or_else(|err| panic!("browser connect {}: {err}", proxy.bind_addr));
        let mut stream = accepted.unwrap_or_else(|err| panic!("guest accept: {err}"));
        tcp.write_all(payload)
            .await
            .unwrap_or_else(|err| panic!("browser write: {err}"));
        let mut got = vec![0u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut got))
            .await
            .unwrap_or_else(|_| panic!("guest read timed out for {payload:?}"))
            .unwrap_or_else(|err| panic!("guest read: {err}"));
        assert_eq!(got, payload);
        stream
            .write_all(payload)
            .await
            .unwrap_or_else(|err| panic!("guest write: {err}"));
        let mut back = vec![0u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut back))
            .await
            .unwrap_or_else(|_| panic!("browser read timed out for {payload:?}"))
            .unwrap_or_else(|err| panic!("browser read: {err}"));
        assert_eq!(back, payload);
    }
}
