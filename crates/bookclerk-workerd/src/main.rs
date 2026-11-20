//! `bookclerk-workerd` — one jailed workerd isolate per plugin.
//!
//! Speaks the same Workers RPC stdio ABI as native guests. Loads author modules
//! via a pinned Cloudflare `workerd` binary, applies domain-allowlisted egress
//! (redirect hops allowed). A `compatibility_date` newer than the bundled pin
//! warns and loads at the pin. `[[kv_namespaces]]` and `[queues]` are served
//! through the granted channel (`/kv/*`, `/queues/send`).
//!
//! Under Linux Landlock `OutboundListen`, only `bind(port=0)` is allowed — the
//! launcher binds the bridge RPC socket itself and passes it to workerd via
//! `--socket-fd`. Windows has no `--socket-fd`, so there workerd binds
//! `127.0.0.1:0` and reports the port on `--control-fd`
//! (`bookclerk_workerd::control`). The adapter-private `GRANTED` capability channel uses a Linux
//! abstract unix socket (or a relative `unix:granted.sock` under `$TMPDIR` on
//! other Unix), or an already-bound loopback TCP listener on Windows
//! (AppContainer-friendly).
//!
//! Author `modules/` stay in the read-only install root (Cap'n Proto
//! `/modules/…` embeds + `--import-path`). `$TMPDIR` only holds generated
//! bridge assets, config, and sockets.

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::unwrap_used
)]

mod manifest_env;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bookclerk_plugin_manifest::PluginManifest;
use bookclerk_workerd::bridge_addr::BridgeAddr;
use bookclerk_workerd::config::{self, ListenSpec};
use bookclerk_workerd::control;
use bookclerk_workerd::egress::EgressProxy;
use bookclerk_workerd::ensure::ensure_workerd;
use bookclerk_workerd::generate_bridge_token;
use bookclerk_workerd::grant::OperatorGrantEnv;
use bookclerk_workerd::pin::{binary_name, WORKERD_RELEASE_TAG};
use bookclerk_workerd::python_snapshot;
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

    if std::env::var(bookclerk_workerd::socket_broker::SOCKET_BROKER_ENV).as_deref() == Ok("1") {
        return run_socket_broker(&manifest).await;
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

/// Runs as the plugin's socket broker on stdio until the host hangs up.
///
/// The broker dials under the same effective egress policy the front door
/// enforces (manifest narrowed by the operator grant the host injected).
async fn run_socket_broker(manifest: &PluginManifest) -> Result<()> {
    if !manifest.capabilities().sockets {
        bail!(
            "plugin `{}` declares no [sockets]; refusing broker mode",
            manifest.id
        );
    }
    let grant = OperatorGrantEnv::from_env();
    let egress = grant.apply_egress(manifest, EgressProxy::from_manifest(manifest));
    info!(
        plugin = %manifest.id,
        mode = ?egress.mode(),
        domains = ?egress.allowed_initial_hosts(),
        "starting socket broker"
    );
    let tunnels = bookclerk_workerd::egress_relay::host_link_from_env().await?;
    bookclerk_workerd::socket_broker::serve_stdio(egress.policy().clone(), tunnels)
        .await
        .map_err(|err| anyhow::anyhow!("socket broker: {err}"))
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
    // A killed launcher never removes its state directory; clear the ones
    // this plugin's dead sessions left before allocating a new one.
    bookclerk_workerd::state_sweep::sweep_abandoned(root);
    let state_dir = config::workerd_state_dir(root)?;
    let _state_cleanup = RemoveDirOnDrop(state_dir.clone());
    // Declared after the cleanup guard so the lock is released before the
    // directory is removed (Windows refuses to delete an open file).
    let _state_owner = bookclerk_workerd::state_sweep::claim(&state_dir)?;
    let bridge_token = generate_bridge_token();

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

    // The isolate's only way out: the relay hands each tunnel to the host's
    // check. Without a host link (network denied) there is no relay and the
    // egress worker is `blocked`.
    let relay = start_egress_relay(&state_dir).await?;
    let egress = &match relay.as_ref() {
        Some(relay) => egress.clone().with_relay(relay.addr.clone()),
        None => egress.clone(),
    };

    // Python isolates try their snapshot starts in order; every other
    // isolate starts once, plainly.
    let snapshot_plan = if manifest.declares_python() {
        python_snapshot::SnapshotPlan::from_env()
    } else {
        python_snapshot::SnapshotPlan::default()
    };
    let (mut child, rpc_addr) = snapshot_plan
        .start_in_order(|start| {
            let (granted_addr, bridge_token, state_dir) = (
                granted_addr.as_str(),
                bridge_token.as_str(),
                state_dir.as_path(),
            );
            async move {
                let (listen, known, rpc_listener) = bridge_rpc_socket(state_dir)?;
                let generated = config::materialize(
                    root,
                    manifest,
                    egress,
                    limits,
                    listen,
                    Some(granted_addr),
                    bridge_token,
                    Some(state_dir),
                )?;
                let (mut cmd, spawn_bin) = workerd_serve_command(workerd_bin, &generated, root)?;
                cmd.args(start.serve_args());
                start_isolate(
                    &mut cmd,
                    &spawn_bin,
                    &generated.listen,
                    known,
                    rpc_listener,
                    bridge_token,
                )
                .await
            }
        })
        .await?;

    let result = mediate_bridge(
        rpc_addr,
        bridge_token.clone(),
        #[cfg(unix)]
        granted_unix,
        #[cfg(not(unix))]
        granted_tcp,
        manifest.capabilities(),
        manifest
            .sockets
            .as_ref()
            .map_or(bookclerk_plugin_manifest::DEFAULT_SOCKETS_BINDING, |b| {
                b.name_or(bookclerk_plugin_manifest::DEFAULT_SOCKETS_BINDING)
            })
            .to_string(),
    )
    .await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

/// The running egress relay for one isolate session.
struct EgressRelay {
    /// Workerd `external` address the relay listens on.
    addr: String,
    /// The accept loop; aborted when the session ends.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for EgressRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the egress relay when the host delivered a proxy link
/// ([`bookclerk_workerd::egress_relay::host_link_from_env`]).
///
/// # Arguments
///
/// * `state_dir` - The session's state directory (Unix socket home).
///
/// # Returns
///
/// `None` when no link was delivered (the plugin's network is denied).
///
/// # Errors
///
/// When the link spec is malformed, the challenge can't be written, or the
/// relay listener can't be bound.
async fn start_egress_relay(state_dir: &Path) -> Result<Option<EgressRelay>> {
    let Some(mux) = bookclerk_workerd::egress_relay::host_link_from_env().await? else {
        return Ok(None);
    };
    let tls = bookclerk_workerd::socket_broker::default_tls()?;

    #[cfg(unix)]
    let (addr, task) = {
        let (addr, listener) = bookclerk_workerd::unix_bind::bind_egress(state_dir)
            .with_context(|| format!("bind egress relay under {}", state_dir.display()))?;
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        let task = tokio::spawn(async move {
            let accept = || {
                let listener = &listener;
                async move { listener.accept().await.map(|(stream, _)| stream) }
            };
            if let Err(err) = bookclerk_workerd::egress_relay::serve(accept, mux, tls).await {
                warn!(error = %err, "egress relay stopped");
            }
        });
        (addr, task)
    };
    #[cfg(not(unix))]
    let (addr, task) = {
        let _ = state_dir;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind egress relay loopback")?;
        let addr = format!("127.0.0.1:{}", listener.local_addr()?.port());
        let task = tokio::spawn(async move {
            let accept = || {
                let listener = &listener;
                async move { listener.accept().await.map(|(stream, _)| stream) }
            };
            if let Err(err) = bookclerk_workerd::egress_relay::serve(accept, mux, tls).await {
                warn!(error = %err, "egress relay stopped");
            }
        });
        (addr, task)
    };
    Ok(Some(EgressRelay { addr, task }))
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

    let (listen, known, rpc_listener) = bridge_rpc_socket(&state_dir)?;

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
    let (mut child, rpc_addr) =
        spawn_and_find_bridge(&mut cmd, &spawn_bin, &generated.listen, known, rpc_listener).await?;
    wait_for_bridge(&rpc_addr, &bridge_token)
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
        rpc_addr,
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
    addr: BridgeAddr,
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
        addr,
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
    addr: BridgeAddr,
    token: String,
    #[cfg(unix)] granted_unix: Option<std::os::unix::net::UnixListener>,
    #[cfg(not(unix))] granted_tcp: Option<std::net::TcpListener>,
    capabilities: bookclerk_plugin_abi::PluginCapabilities,
    sockets_binding: String,
) -> Result<()> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use bookclerk_workerd::bridge_http::BridgeHttp;
    use bookclerk_workerd::bridge_stdio::{mediate_bridge_stdio, Backend};
    use bookclerk_workerd::granted::{spawn_granted, GrantedTable};

    let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
    let http = BridgeHttp {
        addr,
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
            mediate_bridge_stdio(
                http,
                table,
                capabilities,
                Backend::Author { sockets_binding },
            )
            .await
        })
        .await
}

/// How long workerd gets to start listening and answer the bridge `/health`.
const BRIDGE_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// The bridge RPC listener the launcher binds for workerd to inherit.
#[cfg(unix)]
type RpcListener = std::os::unix::net::UnixListener;

/// Windows binds nothing here; workerd binds and reports its own port.
#[cfg(not(unix))]
type RpcListener = std::convert::Infallible;

/// Plans the bridge RPC socket for one workerd spawn.
///
/// On Unix the launcher binds a Unix socket (abstract on Linux, a pathname in
/// the session directory elsewhere) and hands the listener to workerd with
/// `--socket-fd`. Neither process opens an IP socket, so the jail denies them
/// all.
///
/// # Arguments
///
/// * `state_dir` - The session's state directory.
///
/// # Returns
///
/// The listen mode for the generated config, the address the launcher
/// connects to, and the listener workerd inherits.
///
/// # Errors
///
/// When the bind fails or its descriptor cannot be made inheritable.
#[cfg(unix)]
fn bridge_rpc_socket(
    state_dir: &Path,
) -> Result<(ListenSpec, Option<BridgeAddr>, Option<RpcListener>)> {
    use std::os::fd::AsRawFd;
    let (addr, listener) = bookclerk_workerd::unix_bind::bind_rpc(state_dir)
        .with_context(|| format!("bind bridge RPC socket under {}", state_dir.display()))?;
    bookclerk_workerd::unix_bind::clear_cloexec(listener.as_raw_fd())
        .context("clear CLOEXEC on bridge RPC socket")?;
    Ok((ListenSpec::Inherited, Some(addr), Some(listener)))
}

/// Plans the bridge RPC socket for one workerd spawn.
///
/// Windows has no `--socket-fd`, so workerd binds `127.0.0.1:0` itself and
/// reports the port it got ([`ListenSpec::ReportedTcp`]). Reserving a port
/// here and releasing it for workerd to rebind raced every other bind on the
/// host and failed with `WSAEACCES` (10013) whenever another security context
/// took the port in between (issue #781). The AppContainer's
/// `privateNetworkClientServer` capability covers the in-jail loopback bind.
///
/// # Arguments
///
/// * `state_dir` - Unused; the signature matches the Unix variant.
///
/// # Returns
///
/// [`ListenSpec::ReportedTcp`], no known address and no listener.
///
/// # Errors
///
/// Never; the signature matches the Unix variant.
#[cfg(not(unix))]
fn bridge_rpc_socket(
    state_dir: &Path,
) -> Result<(ListenSpec, Option<BridgeAddr>, Option<RpcListener>)> {
    let _ = state_dir;
    Ok((ListenSpec::ReportedTcp, None, None))
}

/// Spawns workerd and learns where its bridge RPC socket listens.
///
/// # Arguments
///
/// * `cmd` - `workerd serve` command from [`workerd_serve_command`].
/// * `spawn_bin` - Executable path, for error messages.
/// * `listen` - Listen mode the config was generated with.
/// * `known` - The bridge address when the launcher bound it (Unix).
/// * `rpc_listener` - Listener workerd inherits with `--socket-fd` (Unix);
///   closed here once the child holds it.
///
/// # Returns
///
/// The running child, with its output forwarded to tracing, and the bridge
/// RPC address.
///
/// # Errors
///
/// When the spawn fails, or when workerd exits or stays silent for
/// [`BRIDGE_READY_TIMEOUT`] before reporting a [`ListenSpec::ReportedTcp`]
/// port (a failed bind makes workerd exit, so this fails fast).
async fn spawn_and_find_bridge(
    cmd: &mut tokio::process::Command,
    spawn_bin: &Path,
    listen: &ListenSpec,
    known: Option<BridgeAddr>,
    rpc_listener: Option<RpcListener>,
) -> Result<(Child, BridgeAddr)> {
    #[cfg(unix)]
    if let Some(ref listener) = rpc_listener {
        use std::os::fd::AsRawFd;
        cmd.arg(format!(
            "--socket-fd={}={}",
            control::RPC_SOCKET,
            listener.as_raw_fd()
        ));
    }
    if listen.reports_port() {
        // Stdout is the control channel; structured logging (on with this
        // listen mode) keeps isolate output from posing as a control line.
        cmd.arg("--control-fd=1");
    }

    let mut child = spawn_workerd_process(cmd, spawn_bin)?;
    // workerd now owns the listening socket; close our copy after spawn.
    // (On Windows the listener type is uninhabited, so there is nothing to
    // close.)
    #[cfg(unix)]
    drop(rpc_listener);
    #[cfg(windows)]
    let _ = rpc_listener;

    control::forward_lines(child.stderr.take());
    if let Some(addr) = known {
        control::forward_lines(child.stdout.take());
        return Ok((child, addr));
    }
    let port = control::bridge_port(listen, child.stdout.take(), BRIDGE_READY_TIMEOUT).await?;
    Ok((child, BridgeAddr::Tcp(port)))
}

/// Spawns workerd and waits until its bridge answers `/health`.
///
/// # Returns
///
/// The running child and the bridge RPC address.
///
/// # Errors
///
/// Returns an error when workerd cannot be spawned, reports no bridge port,
/// or its bridge never becomes healthy; the child is killed first.
async fn start_isolate(
    cmd: &mut tokio::process::Command,
    spawn_bin: &Path,
    listen: &ListenSpec,
    known: Option<BridgeAddr>,
    rpc_listener: Option<RpcListener>,
    bridge_token: &str,
) -> Result<(Child, BridgeAddr)> {
    let (mut child, rpc_addr) =
        spawn_and_find_bridge(cmd, spawn_bin, listen, known, rpc_listener).await?;
    if let Err(err) = wait_for_bridge(&rpc_addr, bridge_token)
        .await
        .context("workerd bridge /health did not become ready")
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(err);
    }
    Ok((child, rpc_addr))
}

/// Polls bridge `/health` for up to [`BRIDGE_READY_TIMEOUT`] before mediating RPC.
async fn wait_for_bridge(addr: &BridgeAddr, token: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + BRIDGE_READY_TIMEOUT;
    loop {
        match bookclerk_workerd::bridge_http::check_health(addr, token).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                if tokio::time::Instant::now() > deadline {
                    bail!("timeout waiting for bridge /health on {addr}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}
