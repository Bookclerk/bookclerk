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
use uuid::Uuid;

use crate::{PluginError, Result};

/// Active host callback proxy for one OAuth session.
pub struct CallbackProxy {
    pub public_base: String,
    pub ipc_endpoint: String,
    bind_addr: SocketAddr,
    _cleanup: Option<PathBuf>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl CallbackProxy {
    /// Bind browser TCP + IPC, spawn accept/forward loop.
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

#[cfg(unix)]
async fn start_unix(
    tcp: TcpListener,
    bound: SocketAddr,
    public_base: String,
    scratch: &Path,
) -> Result<CallbackProxy> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let path = scratch.join(format!("oauth-cb-{}.sock", Uuid::new_v4()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|err| {
        PluginError::message(format!("callback Unix bind {}: {err}", path.display()))
    })?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
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

    let mut server = if let Some(sid) = package_sid {
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
