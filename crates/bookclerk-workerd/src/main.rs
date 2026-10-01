//! `bookclerk-workerd` — one jailed workerd isolate per plugin.
//!
//! Speaks the same Workers RPC stdio ABI as native guests. Loads author modules
//! via a pinned Cloudflare `workerd` binary, applies domain-allowlisted egress
//! (redirect hops allowed). A `compatibility_date` newer than the bundled pin
//! warns and loads at the pin. `[[kv_namespaces]]` and `[queues]` fail load
//! with "not implemented yet".
//!
//! Under Linux Landlock `OutboundListen`, only `bind(port=0)` is allowed — the
//! launcher binds the bridge RPC socket itself and passes it to workerd via
//! `--socket-fd`. The adapter-private `GRANTED` capability channel uses a Linux
//! abstract unix socket (or a relative `unix:granted.sock` under `$TMPDIR` on
//! other Unix), or an already-bound loopback TCP listener on Windows
//! (AppContainer-friendly).
//!
//! Author `modules/` stay in the read-only install root (Cap'n Proto
//! `/modules/…` embeds + `--import-path`). `$TMPDIR` only holds generated
//! bridge assets, config, and sockets.

mod manifest_env;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bookclerk_plugin_manifest::PluginManifest;
use bookclerk_workerd::config::{self, ListenSpec};
use bookclerk_workerd::egress::EgressProxy;
use bookclerk_workerd::ensure::ensure_workerd;
use bookclerk_workerd::generate_bridge_token;
use bookclerk_workerd::grant::OperatorGrantEnv;
use bookclerk_workerd::pin::{binary_name, WORKERD_RELEASE_TAG};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

/// Deletes a per-session workerd state directory when the isolate function returns.
struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
use tracing::{info, warn};

use crate::manifest_env::load_manifest;

/// `DETACHED_PROCESS`. `CREATE_NO_WINDOW` still starts conhost.exe, which
/// consumes a Job active-process slot.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(bookclerk_sandbox::GATEWAY_WORKER_THREADS)
        .max_blocking_threads(bookclerk_sandbox::GATEWAY_MAX_BLOCKING_THREADS)
        .enable_all()
        .thread_name("bookclerk-workerd")
        .build()
        .context("build the bookclerk-workerd runtime")?;
    runtime.block_on(run())
}

/// Gateway process body on the pinned runtime.
async fn run() -> Result<()> {
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
    if let Some(message) = manifest.unimplemented_surface() {
        bail!("{message}");
    }

    if let Ok(rpc_spec) = std::env::var(bookclerk_sandbox::GATEWAY_GUEST_RPC_ENV) {
        if rpc_spec.is_empty() {
            bail!(
                "{} must name an inherited link",
                bookclerk_sandbox::GATEWAY_GUEST_RPC_ENV
            );
        }
        return run_native_behind_workerd(&rpc_spec, &root, &manifest).await;
    }

    let workerd_meta = manifest
        .workerd
        .as_ref()
        .context("bookclerk-workerd requires runtime = \"workerd\" and [workerd] table (or BOOKCLERK_GATEWAY_GUEST_RPC)")?;

    if let Err(err) = bookclerk_plugin_manifest::validate_author_compatibility_date(
        &workerd_meta.compatibility_date,
    ) {
        bail!("{err}");
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
    let beside = std::env::current_exe().ok();
    let beside = beside.as_deref();
    if let Ok(p) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        let path = PathBuf::from(p);
        return bookclerk_sandbox::require_helper_beside_or_absolute(
            &path,
            binary_name(),
            beside,
        )
        .with_context(|| {
            format!(
                "BOOKCLERK_WORKERD_BIN={} is not a usable workerd binary; run `cargo ensure-workerd`",
                path.display()
            )
        });
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(binary_name());
            if candidate.is_file() {
                return bookclerk_sandbox::require_helper_beside_or_absolute(
                    &candidate,
                    binary_name(),
                    Some(&exe),
                )
                .context("validate workerd beside launcher");
            }
            match ensure_workerd(dir) {
                Ok(path) => {
                    return bookclerk_sandbox::require_helper_beside_or_absolute(
                        &path,
                        binary_name(),
                        Some(&exe),
                    )
                    .context("validate ensured workerd binary");
                }
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

/// Builds `workerd serve` after validating the binary and session argv paths.
fn workerd_serve_command(
    workerd_bin: &Path,
    generated: &config::GeneratedConfig,
    root: &Path,
) -> Result<(tokio::process::Command, PathBuf)> {
    let bin = bookclerk_sandbox::require_spawn_executable(workerd_bin)
        .with_context(|| format!("validate workerd binary {}", workerd_bin.display()))?;
    let config_path =
        bookclerk_sandbox::require_under_root(&generated.config_path, &generated.state_dir)
            .context("validate workerd config path under session state_dir")?;
    let import_path = bookclerk_sandbox::require_absolute_spawn_path(&generated.import_path)
        .context("validate workerd --import-path")?;

    // Win32 form: an AppContainer CreateProcess on a `\\?\` path is access-denied
    // even when the same file is readable, and workerd's kj parser aborts when
    // the first component is `?`. Strip the prefix only after the checks above.
    // DETACHED_PROCESS keeps conhost out of the gateway Job's active-process
    // cap (`CREATE_NO_WINDOW` does not). Stdin is a pipe, not `Stdio::null()`:
    // null opens `\\.\NUL`, and an AppContainer token is denied that device
    // (`ERROR_ACCESS_DENIED`) before `CreateProcess` runs.
    // [`spawn_workerd_process`] drops the write end so the child still sees EOF.
    let spawn_bin = bookclerk_sandbox::create_process_path(&bin);
    let config_arg = bookclerk_sandbox::create_process_path(&config_path);
    let import_arg = bookclerk_sandbox::create_process_path(&import_path);
    let cwd = bookclerk_sandbox::create_process_path(&generated.state_dir);
    let plugin_root = bookclerk_sandbox::create_process_path(root);
    let mut cmd = tokio::process::Command::new(&spawn_bin);
    #[cfg(windows)]
    cmd.creation_flags(DETACHED_PROCESS);
    cmd.arg("serve")
        // Unlocks the egress worker's `$experimental` inbound CONNECT handler.
        .arg(bookclerk_workerd::WORKERD_SERVE_EXPERIMENTAL);
    cmd.arg(&config_arg)
        // Cap'n Proto `/modules/…` embeds resolve against the RO install root.
        .arg(format!("--import-path={}", import_arg.display()))
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("BOOKCLERK_PLUGIN_ROOT", &plugin_root)
        .kill_on_drop(true);
    Ok((cmd, spawn_bin))
}

/// Spawns `workerd serve` and closes stdin so the child reads EOF.
///
/// # Errors
///
/// Returns an error when the operating system refuses to create the process.
fn spawn_workerd_process(cmd: &mut tokio::process::Command, spawn_bin: &Path) -> Result<Child> {
    let mut child = bookclerk_sandbox::with_fd_spawn_lock(|| cmd.spawn())
        .with_context(|| format!("spawn {}", spawn_bin.display()))?;
    drop(child.stdin.take());
    Ok(child)
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
    let bridge_token = generate_bridge_token();

    #[cfg(unix)]
    let (listen, rpc_listener) = {
        // Landlock OutboundListen allows bind(0) but not rebinding a concrete
        // ephemeral port — bind here and hand the FD to workerd via --socket-fd.
        let rpc_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind bridge RPC ephemeral loopback")?;
        let rpc_port = rpc_listener.local_addr()?.port();
        clear_cloexec(&rpc_listener).context("clear CLOEXEC on bridge RPC socket")?;
        (
            ListenSpec::InheritedTcp { port: rpc_port },
            Some(rpc_listener),
        )
    };

    #[cfg(not(unix))]
    let (listen, rpc_listener) = {
        // Windows has no `--socket-fd`. Reserve an ephemeral port, release it,
        // and let workerd bind via `--socket-addr` (AppContainer grants
        // privateNetworkClientServer for in-jail loopback).
        let rpc_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind bridge RPC ephemeral loopback")?;
        let rpc_port = rpc_listener.local_addr()?.port();
        drop(rpc_listener);
        (
            ListenSpec::TcpLoopback(rpc_port),
            None::<std::net::TcpListener>,
        )
    };

    #[cfg(unix)]
    let (granted_addr, granted_unix) = {
        let (addr, listener) = bookclerk_workerd::unix_bind::bind_granted(&state_dir)
            .with_context(|| format!("bind granted socket under {}", state_dir.display()))?;
        (addr, Some(listener))
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
        Some(granted_addr.as_str()),
        &bridge_token,
        Some(state_dir.as_path()),
    )?;

    let (mut cmd, spawn_bin) = workerd_serve_command(workerd_bin, &generated, root)?;

    #[cfg(unix)]
    if let Some(ref listener) = rpc_listener {
        use std::os::fd::AsRawFd;
        cmd.arg(format!("--socket-fd=rpc={}", listener.as_raw_fd()));
    }

    let mut child = spawn_workerd_process(&mut cmd, &spawn_bin)?;

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
        manifest.capabilities(),
    )
    .await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

/// Host-owned native-behind-workerd: workerd control plane plus inherited guest links.
///
/// The host spawned the native backend as a sibling jail. This launcher never
/// sees the backend path: [`bookclerk_sandbox::GATEWAY_GUEST_RPC_ENV`] is the
/// Cap'n Proto link. The production host serves the CONNECT mux itself
/// (Windows AppContainers cannot dial host loopback). When
/// [`bookclerk_sandbox::GATEWAY_PROXY_ENV`] is set, this process serves it
/// instead — unjailed conformance spawns. On Windows the RPC link is two
/// unidirectional pipes ([`bookclerk_sandbox::GATEWAY_GUEST_RPC_WRITE_ENV`]
/// is the write half). Plugin input cannot choose either link. Only
/// `describe` / `open` policy and `shutdown` pass through the adapter isolate.
async fn run_native_behind_workerd(
    rpc_spec: &str,
    root: &Path,
    manifest: &PluginManifest,
) -> Result<()> {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use bookclerk_plugin_manifest::WorkerdLimits;
    use bookclerk_workerd::inherited_link::InheritedDuplex;

    let proxy_spec = std::env::var(bookclerk_sandbox::GATEWAY_PROXY_ENV).ok();
    #[cfg(not(windows))]
    let proxy = match proxy_spec.as_deref() {
        Some(spec) => Some(InheritedDuplex::open(spec).context("open inherited proxy link")?),
        None => None,
    };
    #[cfg(windows)]
    let proxy_halves = match proxy_spec.as_deref() {
        Some(spec) => {
            let write_spec = std::env::var(bookclerk_sandbox::GATEWAY_PROXY_WRITE_ENV)
                .with_context(|| {
                    format!(
                        "{} is required on Windows when {} is set",
                        bookclerk_sandbox::GATEWAY_PROXY_WRITE_ENV,
                        bookclerk_sandbox::GATEWAY_PROXY_ENV
                    )
                })?;
            Some(
                InheritedDuplex::open_halves(spec, &write_spec)
                    .context("open inherited proxy pipes")?,
            )
        }
        None => None,
    };
    #[cfg(windows)]
    let (guest_stdout, guest_stdin) = {
        let write_spec = std::env::var(bookclerk_sandbox::GATEWAY_GUEST_RPC_WRITE_ENV)
            .with_context(|| {
                format!(
                    "{} is required on Windows",
                    bookclerk_sandbox::GATEWAY_GUEST_RPC_WRITE_ENV
                )
            })?;
        InheritedDuplex::open_halves(rpc_spec, &write_spec)
            .context("open inherited guest RPC pipes")?
    };
    #[cfg(not(windows))]
    let (guest_stdout, guest_stdin) = InheritedDuplex::open(rpc_spec)
        .context("open inherited guest RPC link")?
        .into_split();

    let workerd_bin = resolve_workerd_binary()?;
    let grant = OperatorGrantEnv::from_env();
    let egress = grant.apply_egress(manifest, EgressProxy::from_manifest(manifest));
    let limits = grant.apply_limits(WorkerdLimits::default().effective());
    info!(
        plugin = %manifest.id,
        workerd = %workerd_bin.display(),
        "starting native-behind-workerd isolate"
    );

    let state_dir = config::host_state_dir()?;
    let bridge_token = generate_bridge_token();

    #[cfg(unix)]
    let (listen, rpc_listener) = {
        let rpc_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind bridge RPC ephemeral loopback")?;
        let rpc_port = rpc_listener.local_addr()?.port();
        clear_cloexec(&rpc_listener).context("clear CLOEXEC on bridge RPC socket")?;
        (
            config::ListenSpec::InheritedTcp { port: rpc_port },
            Some(rpc_listener),
        )
    };

    #[cfg(not(unix))]
    let (listen, rpc_listener) = {
        let rpc_listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind bridge RPC ephemeral loopback")?;
        let rpc_port = rpc_listener.local_addr()?.port();
        drop(rpc_listener);
        (
            config::ListenSpec::TcpLoopback(rpc_port),
            None::<std::net::TcpListener>,
        )
    };

    #[cfg(unix)]
    let (granted_addr, granted_unix) = {
        let (addr, listener) = bookclerk_workerd::unix_bind::bind_granted(&state_dir)
            .with_context(|| format!("bind granted socket under {}", state_dir.display()))?;
        (addr, Some(listener))
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
        manifest,
        &egress,
        limits,
        listen,
        Some(granted_addr.as_str()),
        &bridge_token,
        Some(state_dir.as_path()),
    )?;

    let (mut cmd, spawn_bin) = workerd_serve_command(&workerd_bin, &generated, root)?;

    #[cfg(unix)]
    if let Some(ref listener) = rpc_listener {
        use std::os::fd::AsRawFd;
        cmd.arg(format!("--socket-fd=rpc={}", listener.as_raw_fd()));
    }

    let mut child = spawn_workerd_process(&mut cmd, &spawn_bin)?;
    drop(rpc_listener);
    forward_child_logs(&mut child);
    wait_for_bridge(&generated.listen, &bridge_token)
        .await
        .context("workerd bridge /health did not become ready")?;

    let socket_fence = Arc::new(AtomicBool::new(false));
    #[cfg(not(windows))]
    let proxy_server = if let Some(proxy) = proxy {
        Some(bookclerk_workerd::socket_proxy::spawn_link(
            proxy,
            egress.policy().clone(),
            Arc::clone(&socket_fence),
        )?)
    } else {
        None
    };
    #[cfg(windows)]
    let proxy_server = if let Some((proxy_read, proxy_write)) = proxy_halves {
        Some(bookclerk_workerd::socket_proxy::spawn_halves(
            proxy_read,
            proxy_write,
            egress.policy().clone(),
            Arc::clone(&socket_fence),
        )?)
    } else {
        None
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
        manifest.capabilities(),
    )
    .await;

    socket_fence.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(proxy_server);

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

/// Cap'n Proto stdio with the native guest's vat as the typed data plane.
async fn mediate_native<R, W>(
    port: u16,
    token: String,
    #[cfg(unix)] granted_unix: Option<std::os::unix::net::UnixListener>,
    #[cfg(not(unix))] granted_tcp: Option<std::net::TcpListener>,
    guest_stdout: R,
    guest_stdin: W,
    capabilities: bookclerk_plugin_abi::PluginCapabilities,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + 'static,
    W: tokio::io::AsyncWrite + Unpin + 'static,
{
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use bookclerk_plugin_abi::{connect_plugin, MAX_STREAM_WINDOW_BYTES};
    use bookclerk_workerd::bridge_http::BridgeHttp;
    use bookclerk_workerd::bridge_stdio::{mediate_bridge_stdio, Backend};
    use bookclerk_workerd::granted::{spawn_granted, GrantedTable};

    let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
    let http = BridgeHttp {
        port,
        token: token.clone(),
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let (client, rpc) = connect_plugin(guest_stdout, guest_stdin, MAX_STREAM_WINDOW_BYTES);
            let rpc_task = tokio::task::spawn_local(rpc);
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
            let mediate = mediate_bridge_stdio(http, table, capabilities, Backend::Native(client));
            tokio::select! {
                result = mediate => result,
                rpc = rpc_task => match rpc {
                    Ok(Ok(())) => anyhow::bail!("native guest RPC transport closed"),
                    Ok(Err(err)) => anyhow::bail!("native guest RPC transport: {err}"),
                    Err(err) => anyhow::bail!("native guest RPC task: {err}"),
                },
            }
        })
        .await
}

/// Serves Bookclerk capnp on stdio and granted HTTP on the reverse channel.
async fn mediate_bridge(
    port: u16,
    token: String,
    #[cfg(unix)] granted_unix: Option<std::os::unix::net::UnixListener>,
    #[cfg(not(unix))] granted_tcp: Option<std::net::TcpListener>,
    capabilities: bookclerk_plugin_abi::PluginCapabilities,
) -> Result<()> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use bookclerk_workerd::bridge_http::BridgeHttp;
    use bookclerk_workerd::bridge_stdio::{mediate_bridge_stdio, Backend};
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
            mediate_bridge_stdio(http, table, capabilities, Backend::Author).await
        })
        .await
}

#[cfg(unix)]
/// Clears `FD_CLOEXEC` so workerd inherits the bound RPC listener via `--socket-fd`.
fn clear_cloexec(listener: &std::net::TcpListener) -> Result<()> {
    use std::os::fd::AsRawFd;
    bookclerk_workerd::unix_bind::clear_cloexec(listener.as_raw_fd()).map_err(anyhow::Error::from)
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
