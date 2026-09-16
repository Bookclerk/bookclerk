//! `bookclerk-workerd` — one jailed workerd isolate per plugin.
//!
//! Speaks the same Workers RPC stdio ABI as native guests. Loads author modules
//! via a pinned Cloudflare `workerd` binary, applies domain-allowlisted egress
//! (redirect hops allowed), and warns when `compatibility_date` is newer than
//! the bundled knowledge date.
//!
//! Under Linux Landlock `OutboundListen`, only `bind(port=0)` is allowed — the
//! launcher binds the bridge RPC socket itself and passes it to workerd via
//! `--socket-fd` (same inherited-FD pattern as the plugin fetch-directory
//! channel). `HOST.notify` uses a unix socket under `$TMPDIR` on Unix, or an
//! already-bound loopback TCP listener on Windows (AppContainer-friendly).
//!
//! Author `modules/` stay in the read-only install root (Cap'n Proto
//! `/modules/…` embeds + `--import-path`). `$TMPDIR` only holds generated
//! bridge assets, config, and sockets.

#![cfg_attr(unix, allow(unsafe_code))] // fcntl clear CLOEXEC for --socket-fd

mod manifest_env;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bookclerk_plugin_manifest::PluginManifest;
use bookclerk_workerd::config::{self, ListenSpec};
use bookclerk_workerd::egress::EgressProxy;
use bookclerk_workerd::ensure::ensure_workerd;
use bookclerk_workerd::grant::OperatorGrantEnv;
use bookclerk_workerd::notify::{
    self, event_type_for_log, generate_bridge_token, parse_notify_http, push_notify_event,
    NOTIFY_ACCEPT_LIMIT, NOTIFY_MAX_BODY,
};
use bookclerk_workerd::pin::{binary_name, BUNDLED_WORKERD_COMPAT_DATE, WORKERD_RELEASE_TAG};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::Semaphore;

/// Deletes a per-session workerd state directory when the isolate function returns.
struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
use tracing::{info, warn};

use crate::manifest_env::load_manifest;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let root = plugin_root()?;
    let manifest = load_manifest(&root)?;

    if let Some(backend) = std::env::var_os("BOOKCLERK_NATIVE_BACKEND") {
        let backend = PathBuf::from(backend);
        if !backend.is_file() {
            bail!(
                "BOOKCLERK_NATIVE_BACKEND={} is not a file",
                backend.display()
            );
        }
        return run_native_behind_workerd(&backend, &root, &manifest).await;
    }

    let workerd_meta = manifest
        .workerd
        .as_ref()
        .context("bookclerk-workerd requires runtime = \"workerd\" and [workerd] table (or BOOKCLERK_NATIVE_BACKEND)")?;

    if workerd_meta.compatibility_date.as_str() > BUNDLED_WORKERD_COMPAT_DATE {
        warn!(
            plugin = %manifest.id,
            plugin_date = %workerd_meta.compatibility_date,
            bundled = BUNDLED_WORKERD_COMPAT_DATE,
            "plugin compatibility_date is newer than this Bookclerk build; continuing (Wrangler-like warn)"
        );
    }

    let modules_dir = root.join(&workerd_meta.modules_dir);
    let main_module = modules_dir.join(&workerd_meta.main_module);
    if !main_module.is_file() {
        bail!(
            "main module not found at {} (plugin root {})",
            main_module.display(),
            root.display()
        );
    }

    let workerd_bin = resolve_workerd_binary()?;
    let grant = OperatorGrantEnv::from_env();
    let egress = grant.apply_egress(&manifest, EgressProxy::from_manifest(&manifest));
    let limits = grant.apply_limits(workerd_meta.limits.effective());
    info!(
        plugin = %manifest.id,
        main = %main_module.display(),
        workerd = %workerd_bin.display(),
        pin = WORKERD_RELEASE_TAG,
        mode = ?egress.mode(),
        domains = ?egress.allowed_initial_hosts(),
        cpu_ms = limits.cpu_ms,
        subrequests = limits.subrequests,
        grant_overrides = !grant.is_empty(),
        "starting workerd plugin isolate"
    );

    run_isolate(&workerd_bin, &root, &manifest, &egress, limits).await
}

/// Plugin install directory from `BOOKCLERK_PLUGIN_ROOT`, else the process cwd.
fn plugin_root() -> Result<PathBuf> {
    if let Ok(root) = std::env::var("BOOKCLERK_PLUGIN_ROOT") {
        return Ok(PathBuf::from(root));
    }
    Ok(std::env::current_dir()?)
}

/// Locates the pinned `workerd` binary (`BOOKCLERK_WORKERD_BIN`, beside the launcher, or ensure).
fn resolve_workerd_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "BOOKCLERK_WORKERD_BIN={} is not a file; run `cargo ensure-workerd` (or build-app/dev)",
            path.display()
        );
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(binary_name());
            if candidate.is_file() {
                return Ok(candidate);
            }
            match ensure_workerd(dir) {
                Ok(path) => return Ok(path),
                Err(err) => {
                    warn!(
                        error = %err,
                        "ensure_workerd beside launcher failed; workerd binary required"
                    );
                }
            }
        }
    }
    bail!(
        "workerd binary not found (pin {WORKERD_RELEASE_TAG}). \
         Run `cargo ensure-workerd` or `cargo build-app --platform` / `cargo dev` first."
    )
}

/// Materializes config, spawns workerd, mediates host stdio ↔ bridge HTTP, then kills the child.
async fn run_isolate(
    workerd_bin: &Path,
    root: &Path,
    manifest: &PluginManifest,
    egress: &EgressProxy,
    limits: bookclerk_plugin_manifest::EffectiveWorkerdLimits,
) -> Result<()> {
    let state_dir = config::workerd_state_dir(root)?;
    let _state_cleanup = RemoveDirOnDrop(state_dir.clone());
    let notify_events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let bridge_token = generate_bridge_token();
    let notify_sem = Arc::new(Semaphore::new(NOTIFY_ACCEPT_LIMIT));

    #[cfg(unix)]
    let (listen, rpc_listener, notify_addr, notify_task) = {
        // Landlock OutboundListen allows bind(0) but not rebinding a concrete
        // ephemeral port — bind here and hand the FD to workerd via --socket-fd.
        let rpc_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind bridge RPC ephemeral loopback")?;
        let rpc_port = rpc_listener.local_addr()?.port();
        clear_cloexec(&rpc_listener).context("clear CLOEXEC on bridge RPC socket")?;
        let notify_sock = state_dir.join("notify.sock");
        let _ = std::fs::remove_file(&notify_sock);
        let notify_addr = format!("unix:{}", notify_sock.display());
        let notify_task = spawn_notify_unix(
            notify_sock,
            Arc::clone(&notify_events),
            bridge_token.clone(),
            Arc::clone(&notify_sem),
        );
        (
            ListenSpec::InheritedTcp { port: rpc_port },
            Some(rpc_listener),
            Some(notify_addr),
            Some(notify_task),
        )
    };

    #[cfg(not(unix))]
    let (listen, rpc_listener, notify_addr, notify_task) = {
        // Windows has no `--socket-fd`. Reserve an ephemeral port, release it,
        // and let workerd bind via `--socket-addr` (AppContainer grants
        // privateNetworkClientServer for in-jail loopback). Notify keeps the
        // already-bound listener (same from_std pattern as OAuth IPC).
        let rpc_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind bridge RPC ephemeral loopback")?;
        let rpc_port = rpc_listener.local_addr()?.port();
        drop(rpc_listener);
        let notify_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind HOST.notify ephemeral loopback")?;
        let notify_port = notify_listener.local_addr()?.port();
        let notify_addr = format!("127.0.0.1:{notify_port}");
        let notify_task = spawn_notify_tcp(
            notify_listener,
            Arc::clone(&notify_events),
            bridge_token.clone(),
            Arc::clone(&notify_sem),
        );
        (
            ListenSpec::TcpLoopback(rpc_port),
            None::<std::net::TcpListener>,
            Some(notify_addr),
            Some(notify_task),
        )
    };

    #[cfg(unix)]
    let (granted_addr, granted_unix) = {
        let granted_sock = state_dir.join("granted.sock");
        let _ = std::fs::remove_file(&granted_sock);
        let listener = std::os::unix::net::UnixListener::bind(&granted_sock)
            .with_context(|| format!("bind granted socket {}", granted_sock.display()))?;
        (format!("unix:{}", granted_sock.display()), Some(listener))
    };
    #[cfg(not(unix))]
    let (granted_addr, granted_tcp) = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind granted-capability ephemeral loopback")?;
        let port = listener.local_addr()?.port();
        (format!("127.0.0.1:{port}"), Some(listener))
    };

    let generated = config::materialize(
        root,
        manifest,
        egress,
        limits,
        listen,
        notify_addr.as_deref(),
        Some(granted_addr.as_str()),
        &bridge_token,
        Some(state_dir.as_path()),
    )?;

    let mut cmd = tokio::process::Command::new(workerd_bin);
    cmd.arg("serve")
        .arg(&generated.config_path)
        // Cap'n Proto `/modules/…` embeds resolve against the RO install root.
        .arg(format!("--import-path={}", generated.import_path.display()))
        .current_dir(&generated.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("BOOKCLERK_PLUGIN_ROOT", root)
        .kill_on_drop(true);

    #[cfg(unix)]
    if let Some(ref listener) = rpc_listener {
        use std::os::fd::AsRawFd;
        cmd.arg(format!("--socket-fd=rpc={}", listener.as_raw_fd()));
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", workerd_bin.display()))?;

    // workerd now owns the listening socket; close our copy after spawn.
    drop(rpc_listener);

    forward_child_logs(&mut child);

    wait_for_bridge(&generated.listen, &bridge_token)
        .await
        .context("workerd bridge /health did not become ready")?;

    let result = mediate_bridge(
        generated.listen.port(),
        bridge_token.clone(),
        #[cfg(unix)]
        granted_unix,
        #[cfg(not(unix))]
        granted_tcp,
    )
    .await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    if let Some(task) = notify_task {
        task.abort();
    }
    let buffered = notify_events.lock().map(|g| g.len()).unwrap_or(0);
    if buffered > 0 {
        info!(
            plugin = %manifest.id,
            events = buffered,
            "HOST.notify reverse-channel events buffered this session"
        );
    }
    result
}

/// Host-owned native-behind-workerd: workerd + trusted broker + verified native guest.
///
/// `BOOKCLERK_NATIVE_BACKEND` names the verified executable. Plugin input cannot
/// choose it or weaken the sandbox. Direct Cap'n Proto remains a host-selected
/// fallback, never plugin-selectable.
async fn run_native_behind_workerd(
    backend: &Path,
    root: &Path,
    manifest: &PluginManifest,
) -> Result<()> {
    use bookclerk_plugin_manifest::WorkerdLimits;

    let workerd_bin = resolve_workerd_binary()?;
    let grant = OperatorGrantEnv::from_env();
    let egress = grant.apply_egress(manifest, EgressProxy::from_manifest(manifest));
    let limits = grant.apply_limits(WorkerdLimits::default().effective());
    info!(
        plugin = %manifest.id,
        backend = %backend.display(),
        workerd = %workerd_bin.display(),
        "starting native-behind-workerd isolate"
    );

    let mut guest = tokio::process::Command::new(backend)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn native guest {}", backend.display()))?;
    let guest_stdin = guest.stdin.take().context("native guest stdin")?;
    let guest_stdout = guest.stdout.take().context("native guest stdout")?;

    let broker_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind native broker loopback")?;
    let backend_addr = format!("127.0.0.1:{}", broker_listener.local_addr()?.port());

    let state_dir = config::workerd_state_dir(root)?;
    let _state_cleanup = RemoveDirOnDrop(state_dir.clone());
    let notify_events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let bridge_token = generate_bridge_token();
    let notify_sem = Arc::new(Semaphore::new(NOTIFY_ACCEPT_LIMIT));

    #[cfg(unix)]
    let (listen, rpc_listener, notify_addr, notify_task) = {
        let rpc_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind bridge RPC ephemeral loopback")?;
        let rpc_port = rpc_listener.local_addr()?.port();
        clear_cloexec(&rpc_listener).context("clear CLOEXEC on bridge RPC socket")?;
        let notify_sock = state_dir.join("notify.sock");
        let _ = std::fs::remove_file(&notify_sock);
        let notify_addr = format!("unix:{}", notify_sock.display());
        let notify_task = spawn_notify_unix(
            notify_sock,
            Arc::clone(&notify_events),
            bridge_token.clone(),
            Arc::clone(&notify_sem),
        );
        (
            config::ListenSpec::InheritedTcp { port: rpc_port },
            Some(rpc_listener),
            Some(notify_addr),
            Some(notify_task),
        )
    };

    #[cfg(not(unix))]
    let (listen, rpc_listener, notify_addr, notify_task) = {
        let rpc_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind bridge RPC ephemeral loopback")?;
        let rpc_port = rpc_listener.local_addr()?.port();
        drop(rpc_listener);
        let notify_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind HOST.notify ephemeral loopback")?;
        let notify_port = notify_listener.local_addr()?.port();
        let notify_addr = format!("127.0.0.1:{notify_port}");
        let notify_task = spawn_notify_tcp(
            notify_listener,
            Arc::clone(&notify_events),
            bridge_token.clone(),
            Arc::clone(&notify_sem),
        );
        (
            config::ListenSpec::TcpLoopback(rpc_port),
            None::<std::net::TcpListener>,
            Some(notify_addr),
            Some(notify_task),
        )
    };

    #[cfg(unix)]
    let (granted_addr, granted_unix) = {
        let granted_sock = state_dir.join("granted.sock");
        let _ = std::fs::remove_file(&granted_sock);
        let listener = std::os::unix::net::UnixListener::bind(&granted_sock)
            .with_context(|| format!("bind granted socket {}", granted_sock.display()))?;
        (format!("unix:{}", granted_sock.display()), Some(listener))
    };
    #[cfg(not(unix))]
    let (granted_addr, granted_tcp) = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind granted-capability ephemeral loopback")?;
        let port = listener.local_addr()?.port();
        (format!("127.0.0.1:{port}"), Some(listener))
    };

    let generated = config::materialize_native_backend(
        root,
        &egress,
        limits,
        listen,
        notify_addr.as_deref(),
        Some(granted_addr.as_str()),
        &backend_addr,
        &bridge_token,
        Some(state_dir.as_path()),
    )?;

    let mut cmd = tokio::process::Command::new(&workerd_bin);
    cmd.arg("serve")
        .arg(&generated.config_path)
        .arg(format!("--import-path={}", generated.import_path.display()))
        .current_dir(&generated.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("BOOKCLERK_PLUGIN_ROOT", root)
        .kill_on_drop(true);

    #[cfg(unix)]
    if let Some(ref listener) = rpc_listener {
        use std::os::fd::AsRawFd;
        cmd.arg(format!("--socket-fd=rpc={}", listener.as_raw_fd()));
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", workerd_bin.display()))?;
    drop(rpc_listener);
    forward_child_logs(&mut child);
    wait_for_bridge(&generated.listen, &bridge_token)
        .await
        .context("workerd bridge /health did not become ready")?;

    let plugin_id = manifest.id.clone();
    let policy = match manifest.kind {
        bookclerk_plugin_manifest::PluginKind::Integration => {
            bookclerk_workerd::native_broker::BrokerPolicy::integration(plugin_id, "1")
        }
        _ => bookclerk_workerd::native_broker::BrokerPolicy::destination(plugin_id, "1"),
    };
    let result = mediate_native(
        generated.listen.port(),
        bridge_token.clone(),
        #[cfg(unix)]
        granted_unix,
        #[cfg(not(unix))]
        granted_tcp,
        guest_stdout,
        guest_stdin,
        broker_listener,
        policy,
    )
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = guest.kill().await;
    let _ = guest.wait().await;
    if let Some(task) = notify_task {
        task.abort();
    }
    result
}

/// Cap'n Proto stdio plus a native broker feeding `PLUGIN_BACKEND`.
async fn mediate_native(
    port: u16,
    token: String,
    #[cfg(unix)] granted_unix: Option<std::os::unix::net::UnixListener>,
    #[cfg(not(unix))] granted_tcp: Option<std::net::TcpListener>,
    guest_stdout: tokio::process::ChildStdout,
    guest_stdin: tokio::process::ChildStdin,
    broker_listener: tokio::net::TcpListener,
    policy: bookclerk_workerd::native_broker::BrokerPolicy,
) -> Result<()> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use bookclerk_plugin_abi::connect_plugin;
    use bookclerk_workerd::bridge_http::BridgeHttp;
    use bookclerk_workerd::bridge_stdio::mediate_bridge_stdio;
    use bookclerk_workerd::granted::{spawn_granted, GrantedTable};
    use bookclerk_workerd::native_broker::spawn_native_broker;

    let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
    let http = BridgeHttp {
        port,
        token: token.clone(),
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let (client, rpc) = connect_plugin(guest_stdout, guest_stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            spawn_native_broker(broker_listener, client, policy);
            #[cfg(unix)]
            {
                let std_listener = granted_unix.context("missing granted unix listener")?;
                std_listener.set_nonblocking(true)?;
                let listener = tokio::net::UnixListener::from_std(std_listener)?;
                spawn_granted(listener, token, Rc::clone(&table));
            }
            #[cfg(not(unix))]
            {
                let std_listener = granted_tcp.context("missing granted TCP listener")?;
                std_listener.set_nonblocking(true)?;
                let listener = tokio::net::TcpListener::from_std(std_listener)?;
                spawn_granted(listener, token, Rc::clone(&table));
            }
            mediate_bridge_stdio(http, table).await
        })
        .await
}

/// Serves Bookclerk capnp on stdio and granted HTTP on the reverse channel.
async fn mediate_bridge(
    port: u16,
    token: String,
    #[cfg(unix)] granted_unix: Option<std::os::unix::net::UnixListener>,
    #[cfg(not(unix))] granted_tcp: Option<std::net::TcpListener>,
) -> Result<()> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use bookclerk_workerd::bridge_http::BridgeHttp;
    use bookclerk_workerd::bridge_stdio::mediate_bridge_stdio;
    use bookclerk_workerd::granted::{spawn_granted, GrantedTable};

    let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
    let http = BridgeHttp {
        port,
        token: token.clone(),
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            #[cfg(unix)]
            {
                let std_listener = granted_unix.context("missing granted unix listener")?;
                std_listener.set_nonblocking(true)?;
                let listener = tokio::net::UnixListener::from_std(std_listener)?;
                spawn_granted(listener, token, Rc::clone(&table));
            }
            #[cfg(not(unix))]
            {
                let std_listener = granted_tcp.context("missing granted TCP listener")?;
                std_listener.set_nonblocking(true)?;
                let listener = tokio::net::TcpListener::from_std(std_listener)?;
                spawn_granted(listener, token, Rc::clone(&table));
            }
            mediate_bridge_stdio(http, table).await
        })
        .await
}

#[cfg(unix)]
/// Clears `FD_CLOEXEC` so workerd inherits the bound RPC listener via `--socket-fd`.
fn clear_cloexec(listener: &std::net::TcpListener) -> Result<()> {
    use std::os::fd::AsRawFd;
    let fd = listener.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        bail!("F_GETFD failed: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        bail!(
            "F_SETFD clear CLOEXEC failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(unix)]
/// Accepts `HOST.notify` connections on a unix socket under the guest `$TMPDIR`.
fn spawn_notify_unix(
    path: PathBuf,
    events: Arc<Mutex<Vec<serde_json::Value>>>,
    token: String,
    sem: Arc<Semaphore>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let listener = match tokio::net::UnixListener::bind(&path) {
            Ok(l) => l,
            Err(err) => {
                warn!(
                    error = %err,
                    path = %path.display(),
                    "HOST.notify unix listener failed to bind"
                );
                return;
            }
        };
        info!(path = %path.display(), "HOST.notify reverse channel listening");
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let Ok(permit) = sem.clone().acquire_owned().await else {
                continue;
            };
            let events = Arc::clone(&events);
            let token = token.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(err) = handle_notify_connection(&mut stream, &events, &token).await {
                    warn!(error = %err, "HOST.notify request failed");
                }
            });
        }
    })
}

#[cfg(not(unix))]
fn spawn_notify_tcp(
    std_listener: std::net::TcpListener,
    events: Arc<Mutex<Vec<serde_json::Value>>>,
    token: String,
    sem: Arc<Semaphore>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Keep the already-bound port-0 socket (do not rebind a concrete port).
        if let Err(err) = std_listener.set_nonblocking(true) {
            warn!(error = %err, "HOST.notify set_nonblocking failed");
            return;
        }
        let listener = match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(err) => {
                warn!(error = %err, "HOST.notify from_std failed");
                return;
            }
        };
        info!("HOST.notify reverse channel listening");
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let Ok(permit) = sem.clone().acquire_owned().await else {
                continue;
            };
            let events = Arc::clone(&events);
            let token = token.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(err) = handle_notify_connection(&mut stream, &events, &token).await {
                    warn!(error = %err, "HOST.notify request failed");
                }
            });
        }
    })
}

/// Parses one notify HTTP request, buffers the event, and writes a short HTTP reply.
async fn handle_notify_connection<S>(
    stream: &mut S,
    events: &Mutex<Vec<serde_json::Value>>,
    token: &str,
) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let raw = read_notify_http(stream).await?;
    if raw.is_empty() {
        return Ok(());
    }
    let (status, reason, body) = match parse_notify_http(&raw, token) {
        Ok((event, size)) => {
            info!(
                event_type = ?event_type_for_log(&event),
                size,
                "HOST.notify"
            );
            match push_notify_event(events, event) {
                Ok(true) => {
                    warn!(
                        cap = notify::NOTIFY_EVENT_CAP,
                        "HOST.notify event buffer full; dropped oldest"
                    );
                }
                Ok(false) => {}
                Err(err) => warn!(error = %err, "HOST.notify buffer push failed"),
            }
            (200u16, "OK", "ok")
        }
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("unauthorized") {
                warn!("HOST.notify unauthorized");
                (401, "Unauthorized", "unauthorized")
            } else {
                warn!(error = %err, "HOST.notify bad request");
                (400, "Bad Request", "bad request")
            }
        }
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

/// Read one HTTP request for notify, capped so headers + body stay within
/// `NOTIFY_MAX_BODY` plus a modest header allowance.
async fn read_notify_http<S>(stream: &mut S) -> Result<String>
where
    S: AsyncReadExt + Unpin,
{
    const HEADER_ALLOWANCE: usize = 8192;
    let max_total = NOTIFY_MAX_BODY + HEADER_ALLOWANCE;
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > max_total {
            bail!("notify request too large");
        }
        if find_header_end(&buf).is_some() {
            // Prefer to have the full body when Content-Length is already present.
            if let Some(need) = content_length_needed(&buf) {
                let header_end = find_header_end(&buf).unwrap();
                let have = buf.len().saturating_sub(header_end);
                if have >= need.min(NOTIFY_MAX_BODY + 1) {
                    break;
                }
                // Still need more body bytes — keep reading unless oversized.
                if need > NOTIFY_MAX_BODY {
                    break;
                }
            } else {
                break;
            }
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Byte offset after the HTTP header terminator (`\r\n\r\n` or `\n\n`).
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

/// Parsed `Content-Length` from notify headers, when present.
fn content_length_needed(buf: &[u8]) -> Option<usize> {
    let end = find_header_end(buf)?;
    let headers = std::str::from_utf8(&buf[..end]).ok()?;
    for line in headers.lines().skip(1) {
        let line = line.trim_end_matches('\r');
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Forwards workerd stdout/stderr lines through tracing (JSON when the parent is bookclerkd).
fn forward_child_logs(child: &mut Child) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "workerd", "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "workerd", "{line}");
            }
        });
    }
}

/// Polls bridge `/health` for up to 30s before mediating RPC.
async fn wait_for_bridge(listen: &ListenSpec, token: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match bridge_get(listen, "/health", token).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                if tokio::time::Instant::now() > deadline {
                    bail!("timeout waiting for bridge /health on {:?}", listen);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Blocking ureq GET/POST to the loopback bridge, authenticated with the session token.
async fn bridge_http(
    listen: &ListenSpec,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    token: &str,
) -> Result<String> {
    let port = listen.port();
    let url = format!("http://127.0.0.1:{port}{path}");
    let url_owned = url.clone();
    let method = method.to_string();
    let auth = format!("Bearer {token}");
    tokio::task::spawn_blocking(move || {
        let mut response = match method.as_str() {
            "GET" => ureq::get(&url_owned)
                .header("Authorization", &auth)
                .call()
                .with_context(|| url_owned.clone())?,
            "POST" => {
                let body = body.unwrap_or_default();
                ureq::post(&url_owned)
                    .header("content-type", "application/json")
                    .header("Authorization", &auth)
                    .send(body)
                    .with_context(|| format!("POST {url_owned}"))?
            }
            other => bail!("unsupported method {other}"),
        };
        let status = response.status();
        let text = response
            .body_mut()
            .read_to_string()
            .context("read bridge body")?;
        if method == "GET" {
            if !status.is_success() {
                bail!("HTTP {status}");
            }
            return Ok(text);
        }
        if !status.is_success() && status.as_u16() != 400 {
            bail!("bridge HTTP {status}: {text}");
        }
        Ok(text)
    })
    .await?
}

/// GET a bridge path (used for `/health`).
async fn bridge_get(listen: &ListenSpec, path: &str, token: &str) -> Result<String> {
    bridge_http(listen, "GET", path, None, token).await
}
