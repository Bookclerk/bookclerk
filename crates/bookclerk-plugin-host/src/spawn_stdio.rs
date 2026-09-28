//! Shared jailed-child spawn for Cap'n Proto `api_version = 3` stdio guests.

#![allow(clippy::missing_docs_in_private_items)]
#![cfg_attr(unix, allow(unsafe_code))] // `Command::pre_exec` + `inherit_fd_at` (dup2).

#[cfg(test)]
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bookclerk_config::Config;
#[cfg(unix)]
use bookclerk_sandbox::DuplexLink;
#[cfg(any(windows, test))]
use bookclerk_sandbox::GATEWAY_GUEST_RPC_WRITE_ENV;
#[cfg(test)]
use bookclerk_sandbox::GATEWAY_PROXY_ENV;
use bookclerk_sandbox::{
    join_capped_lines, push_capped_line, redact_capped, spawn_diag_stderr_enabled, truncate_utf8,
    with_fd_spawn_lock, GATEWAY_GUEST_RPC_ENV, SOCKET_PROXY_ENV, SPAWN_DIAG_MAX_LINES,
    SPAWN_DIAG_RECORD_BYTES, SPAWN_DIAG_TOTAL_BYTES, WORKERD_STATE_DIR_ENV,
};
#[cfg(windows)]
use bookclerk_sandbox::{DuplexHalf, StdioEnds};
#[cfg(windows)]
use bookclerk_sandbox::{JailHandoff, JailHandoffExtra, JAIL_HANDOFF_ENV};
#[cfg(unix)]
use bookclerk_sandbox::{GATEWAY_RPC_FD, GUEST_PROXY_FD};
use serde_json::Value;
#[cfg(windows)]
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::consent::{inject_workerd_grant_env, spawn_config_for_grant, spawn_grant, PluginGrant};
use crate::discover::DiscoveredPlugin;
use crate::jail::{GuestJail, Start};
use crate::spawn_plan::{SpawnPlan, WORKERD_BIN_ENV};
use crate::{PluginError, Result};

/// Set to `1` or `true` in the host process to give each proxy a channel tag.
///
/// The variable is not copied into the guest. The tag is written under the
/// gateway session directory as [`TEST_CHANNEL_TAG_FILE`] and answered only
/// by that session's proxy. Production leaves it unset.
pub const TEST_CHANNEL_IDENT_ENV: &str = "BOOKCLERK_TEST_CHANNEL_IDENT";

/// Session-directory file holding the tag when [`TEST_CHANNEL_IDENT_ENV`] is set.
pub const TEST_CHANNEL_TAG_FILE: &str = "channel-tag";

/// Host-process description of one extra endpoint to add to the guest handoff.
///
/// Unix: `fd:<n>` of a socket in this process. Windows:
/// `handle:<read>,handle:<write>` of the guest pipe ends in this process.
/// The variable is not copied into the guest. Production leaves it unset, so
/// the guest preserve list stays [`bookclerk_sandbox::GUEST_PROXY_FD`] only.
/// Setting it transfers the endpoint. It does not, by itself, publish the
/// candidate metadata the guest uses to observe that slot.
pub const TEST_INJECT_EXTRA_ENDPOINT_ENV: &str = "BOOKCLERK_TEST_INJECT_EXTRA_ENDPOINT";

/// Host-process request to publish candidate-endpoint observation metadata.
///
/// `1` or `true` names the fixed Unix slot, and on Windows the handles from
/// [`TEST_INJECT_EXTRA_ENDPOINT_ENV`]. A Windows `handle:<read>,handle:<write>`
/// value names that candidate without transferring it. The variable is not
/// copied into the guest. Unset means the guest reports `not-run`, which is
/// not proof the endpoint is absent.
pub const TEST_OBSERVE_EXTRA_ENDPOINT_ENV: &str = "BOOKCLERK_TEST_OBSERVE_EXTRA_ENDPOINT";

static SPAWN_DIAG: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static SPAWN_DIAG_SEQ: AtomicU64 = AtomicU64::new(1);

/// Record one secret-free startup line.
///
/// The line is a `bookclerk::spawn` tracing event and a byte-capped ring
/// entry. Raw stderr is written only when [`spawn_diag_stderr_enabled`] is
/// set (`BOOKCLERK_SPAWN_DIAG=1|true|stderr`). Session challenges are stripped.
/// Plugin stderr is not stored here; the per-session tail is separate.
pub fn note_spawn_stage(message: &str) {
    let line = push_stage(message);
    emit_structured_stage(&line);
}

/// Bounded startup log for the current process.
///
/// A deadline handler prints this before exiting. The snapshot is a second
/// capped copy of the ring, not a join of unbounded strings.
#[must_use]
pub fn recent_spawn_diagnostics() -> String {
    SPAWN_DIAG
        .lock()
        .map(|ring| join_capped_lines(&ring, SPAWN_DIAG_TOTAL_BYTES))
        .unwrap_or_default()
}

fn push_stage(message: &str) -> String {
    // Classify a token that crosses the cap before any of its bytes are stored.
    let message = redact_capped(message, SPAWN_DIAG_RECORD_BYTES);
    let seq = SPAWN_DIAG_SEQ.fetch_add(1, Ordering::Relaxed);
    let line = format!("bookclerk-spawn: stage {seq}: {message}");
    if let Ok(mut ring) = SPAWN_DIAG.lock() {
        push_capped_line(
            &mut ring,
            &line,
            SPAWN_DIAG_RECORD_BYTES,
            SPAWN_DIAG_TOTAL_BYTES,
            SPAWN_DIAG_MAX_LINES,
        );
    }
    line
}

fn emit_structured_stage(line: &str) {
    tracing::info!(target: "bookclerk::spawn", "{line}");
    if spawn_diag_stderr_enabled() {
        eprintln!("{line}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

/// Jailed plugin child with stdio pipes (describe not yet called).
pub(crate) struct SpawnedStdio {
    /// Provenance-qualified PluginKey (canonical text).
    pub id: String,
    /// Manifest display alias (`plugin.toml` `id`).
    pub alias: String,
    /// Child the host speaks Cap'n Proto to (gateway / isolate / direct).
    pub child: Child,
    /// Native sibling when this session is native-behind-workerd.
    pub guest: Option<Child>,
    /// Guest stdin (host writes RPC / capnp) — the gateway / primary child.
    pub stdin: ChildStdin,
    /// Guest stdout (host reads RPC / capnp).
    pub stdout: ChildStdout,
    /// Covering **effective** grant (persisted ∩ overlays).
    pub grant: PluginGrant,
    /// Persisted operator grant before host overlays.
    pub persisted_grant: PluginGrant,
    /// Spawn config JSON or destination context extras.
    pub spawn_config: Value,
    /// Guest HOME / data directory.
    pub data: PathBuf,
    /// Guest TMPDIR / scratch directory.
    pub scratch: PathBuf,
    /// Host-owned gateway session directory (removed when the vat drops).
    pub session_dir: Option<PathBuf>,
    /// Native-behind gateway pid (the Cap'n Proto child).
    pub gateway_pid: Option<u32>,
    /// Native guest pid, or the single child when there is no sibling.
    pub guest_pid: Option<u32>,
    /// AppContainer package SID of the native guest (callback proxy).
    #[cfg(windows)]
    pub package_sid: Option<String>,
    /// Host-owned AppContainer profile for the Cap'n Proto child.
    #[cfg(windows)]
    #[allow(dead_code)]
    pub appcontainer: Option<bookclerk_sandbox::spawn::AppContainerSession>,
    /// Host-owned AppContainer profile for the native sibling.
    #[cfg(windows)]
    #[allow(dead_code)]
    pub guest_appcontainer: Option<bookclerk_sandbox::spawn::AppContainerSession>,
    /// Windows session Job (`KILL_ON_JOB_CLOSE`) covering both jails.
    #[cfg(windows)]
    #[allow(dead_code)]
    pub session_job: Option<bookclerk_sandbox::SessionJob>,
    /// Best-effort spawn continued with no outer Job because that kernel
    /// feature is unsupported. Required isolation fails before this is set.
    #[cfg(windows)]
    #[allow(dead_code)] // recorded on the session; Required isolation never sets it
    pub outer_job_unsupported: bool,
    /// Cancel flag shared by the host proxy, initial describe, and RPCs.
    pub cancel: Arc<AtomicBool>,
    /// Host CONNECT proxy. Drop cancels its tasks.
    pub proxy: Option<bookclerk_workerd::socket_proxy::ProxyServer>,
    /// Pid and start time captured when each sibling was spawned.
    ///
    /// Cleanup signals that process group only while the start time still
    /// matches. The leader must still be a zombie: reaping it drops the start
    /// time, and a recycled pid is not signalled.
    pub identities: SiblingIdentities,
    /// Linux cgroup leaf. Drop kills members and removes the directory.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)] // ownership is the Drop impl; nothing else reads the path
    pub session_cgroup: Option<crate::jail::SessionCgroup>,
    /// Guest pathname-socket directory. Drop removes it.
    #[cfg(unix)]
    #[allow(dead_code)] // ownership is the Drop impl; the session clones the path
    pub guest_ipc: Option<crate::jail::GuestIpcDir>,
    /// Host-owned ACL rollback for this session's package SIDs.
    #[cfg(windows)]
    #[allow(dead_code)] // Drop revokes; the vat owns the journal by holding it
    pub acl_journal: AclJournal,
    /// Last lines of guest + gateway stderr, for spawn failures.
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Files dir used to re-read `plugin-grants.json` before returning a session.
    pub files_dir: PathBuf,
}

/// Child processes plus the session cancel token, proxy, and recorded identities.
struct SpawnedParts {
    child: Child,
    guest: Option<Child>,
    stdin: ChildStdin,
    stdout: ChildStdout,
    gateway_pid: Option<u32>,
    guest_pid: Option<u32>,
    session_job: Option<WindowsSessionJob>,
    cancel: Arc<AtomicBool>,
    proxy: Option<bookclerk_workerd::socket_proxy::ProxyServer>,
    identities: SiblingIdentities,
}

/// Removes `session_dir` unless [`Self::disarm`] is called after a successful spawn.
struct SessionDirGuard(Option<PathBuf>);

impl SessionDirGuard {
    fn disarm(&mut self) -> Option<PathBuf> {
        self.0.take()
    }
}

impl Drop for SessionDirGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Spawns the jailed guest with piped stdio. Caller performs Cap'n Proto connect.
///
/// `plan` names the program the jail execs (`bookclerk-workerd` on the product
/// path) and, for a native backend, the sibling the host starts beside it.
///
/// # Errors
///
/// Fails when no covering grant exists, the jail cannot be applied, or the
/// process cannot be started.
pub(crate) async fn spawn_stdio_guest(
    plugin: &DiscoveredPlugin,
    plan: &SpawnPlan,
    config: &Config,
    config_table: Value,
    extra_env: &[(&str, OsString)],
) -> Result<SpawnedStdio> {
    let id = plugin.plugin_key().canonical().to_string();
    note_spawn_stage(&format!("spawn begin plugin={id}"));
    tokio::task::yield_now().await;
    let alias = plugin.manifest.id.clone();
    let persisted_grant = spawn_grant(&config.paths().files_dir, plugin)?;
    let grant = effective_spawn_grant(&persisted_grant, plugin, config);
    let spawn_config = spawn_config_for_grant(&grant, config_table);
    // Unix takes the cgroup and guest IPC directory out of the jail. Windows
    // only moves fields, so `mut` is unused there.
    #[cfg_attr(windows, allow(unused_mut))]
    let mut jail = GuestJail::plan(config, plugin, plan)?;
    note_spawn_stage(&format!("spawn planned plugin={id}"));
    let mut session_guard = SessionDirGuard(jail.session_dir.clone());
    let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
    #[cfg(windows)]
    let acl_journal = build_acl_journal(&jail, plan);

    let spawned = if jail.guest_start.is_some() {
        spawn_siblings(
            plugin,
            plan,
            &jail,
            &grant,
            extra_env,
            &id,
            Arc::clone(&stderr_tail),
        )
        .await
    } else {
        spawn_single(
            plugin,
            plan,
            &jail,
            &grant,
            extra_env,
            &id,
            Arc::clone(&stderr_tail),
        )
        .await
    };

    let parts = match spawned {
        Ok(parts) => parts,
        Err(err) => return Err(err),
    };
    let session_dir = session_guard.disarm();
    #[cfg(not(windows))]
    let _ = &parts.session_job;
    #[cfg(windows)]
    let outer_job_unsupported = jail.guest_start.is_some() && parts.session_job.is_none();
    #[cfg(target_os = "linux")]
    let session_cgroup = jail.session_cgroup.take();
    #[cfg(unix)]
    let guest_ipc = jail.guest_ipc.take();

    Ok(SpawnedStdio {
        id,
        alias,
        child: parts.child,
        guest: parts.guest,
        stdin: parts.stdin,
        stdout: parts.stdout,
        grant,
        persisted_grant,
        spawn_config,
        data: jail.data,
        scratch: jail.scratch,
        session_dir,
        gateway_pid: parts.gateway_pid,
        guest_pid: parts.guest_pid,
        #[cfg(windows)]
        package_sid: jail.package_sid,
        #[cfg(windows)]
        appcontainer: jail.appcontainer,
        #[cfg(windows)]
        guest_appcontainer: jail.guest_appcontainer,
        #[cfg(windows)]
        session_job: parts.session_job,
        #[cfg(windows)]
        outer_job_unsupported,
        cancel: parts.cancel,
        proxy: parts.proxy,
        identities: parts.identities,
        #[cfg(target_os = "linux")]
        session_cgroup,
        #[cfg(unix)]
        guest_ipc,
        #[cfg(windows)]
        acl_journal,
        stderr_tail,
        files_dir: config.paths().files_dir.clone(),
    })
}

/// Single-child spawn (workerd isolate or diagnostic direct native).
#[allow(clippy::too_many_arguments, unused_variables)]
async fn spawn_single(
    plugin: &DiscoveredPlugin,
    plan: &SpawnPlan,
    jail: &GuestJail,
    grant: &PluginGrant,
    extra_env: &[(&str, OsString)],
    id: &str,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
) -> Result<SpawnedParts> {
    tracing::debug!(
        plugin = %id,
        program = %plan.launcher.display(),
        runtime = plan.runtime.label(),
        "starting plugin guest"
    );
    let mut cmd = command_for_start(&jail.start, &plan.launcher, &plan.args);
    apply_common_env(&mut cmd, plugin, id);
    if plan.fronted_by_workerd() {
        inject_workerd_grant_env(&mut cmd, grant);
        if let Some(workerd_bin) = &plan.workerd_bin {
            cmd.env(WORKERD_BIN_ENV, workerd_bin);
        }
    }
    apply_temp_and_home(&mut cmd, &jail.scratch, &jail.data);
    for (key, value) in extra_env {
        cmd.env(*key, value);
    }
    apply_spec_env(&mut cmd, &jail.start)?;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = with_fd_spawn_lock(|| cmd.spawn())?;
    let identities = sibling_identities(Some(&child), None);
    if let Some(stderr) = child.stderr.take() {
        forward_guest_stderr(id.to_string(), "guest", stderr, Arc::clone(&stderr_tail));
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| PluginError::message("plugin stdin missing"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PluginError::message("plugin stdout missing"))?;
    let pid = child.id();
    Ok(SpawnedParts {
        child,
        guest: None,
        stdin,
        stdout,
        gateway_pid: None,
        guest_pid: pid,
        session_job: None,
        cancel: Arc::new(AtomicBool::new(false)),
        proxy: None,
        identities,
    })
}

/// Host-spawned gateway + native guest joined by inherited duplex links.
#[allow(clippy::too_many_arguments)]
async fn spawn_siblings(
    plugin: &DiscoveredPlugin,
    plan: &SpawnPlan,
    jail: &GuestJail,
    grant: &PluginGrant,
    extra_env: &[(&str, OsString)],
    id: &str,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
) -> Result<SpawnedParts> {
    let backend = plan.native_backend.as_ref().ok_or_else(|| {
        PluginError::message("native-behind-workerd spawn is missing the backend path")
    })?;
    let guest_start = jail.guest_start.as_ref().ok_or_else(|| {
        PluginError::message("native-behind-workerd jail plan is missing the guest start")
    })?;
    let session_dir = jail.session_dir.as_ref().ok_or_else(|| {
        PluginError::message("native-behind-workerd jail plan is missing the session directory")
    })?;

    // Outer Job before links, the proxy, or either sibling. A required failure
    // returns with nothing left running; the caller drops profiles and the
    // session directory.
    #[cfg(windows)]
    let session_job = match prepare_windows_session_job(jail, plan.runtime) {
        Ok(job) => job,
        Err(err) => return Err(err),
    };
    #[cfg(not(windows))]
    let session_job: Option<WindowsSessionJob> = None;

    // Unix links are socketpairs. Windows uses two unidirectional pipes per
    // link so a pending read cannot lock a write on the same pipe. Guest RPC
    // ends stay synchronous (Rust std aborts on overlapped stdin). Proxy ends
    // are overlapped on both peers because both wrap them in Tokio.
    #[cfg(unix)]
    let (rpc_gateway, rpc_guest) = DuplexLink::pair()
        .map_err(|err| PluginError::message(format!("could not create guest RPC link: {err}")))?;
    #[cfg(unix)]
    let (proxy_gateway, proxy_guest) = DuplexLink::pair().map_err(|err| {
        PluginError::message(format!("could not create socket-proxy link: {err}"))
    })?;
    #[cfg(windows)]
    let rpc_pipes = StdioEnds::pair()
        .map_err(|err| PluginError::message(format!("could not create guest RPC pipes: {err}")))?;
    #[cfg(windows)]
    let proxy_pipes = StdioEnds::pair_overlapped().map_err(|err| {
        PluginError::message(format!("could not create socket-proxy pipes: {err}"))
    })?;

    tracing::debug!(
        plugin = %id,
        gateway = %plan.launcher.display(),
        guest = %backend.display(),
        session = %session_dir.display(),
        "starting native-behind-workerd siblings"
    );

    let mut gateway_cmd = command_for_start(&jail.start, &plan.launcher, &plan.args);
    apply_common_env(&mut gateway_cmd, plugin, id);
    inject_workerd_grant_env(&mut gateway_cmd, grant);
    if let Some(workerd_bin) = &plan.workerd_bin {
        gateway_cmd.env(WORKERD_BIN_ENV, workerd_bin);
    }
    apply_temp_and_home(&mut gateway_cmd, session_dir, session_dir);
    gateway_cmd.env(WORKERD_STATE_DIR_ENV, session_dir);
    apply_spec_env(&mut gateway_cmd, &jail.start)?;
    gateway_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut guest_cmd = command_for_start(guest_start, backend, &[]);
    apply_common_env(&mut guest_cmd, plugin, id);
    apply_temp_and_home(&mut guest_cmd, &jail.scratch, &jail.data);
    #[cfg(unix)]
    if let Some(dir) = jail.guest_ipc.as_ref().and_then(|ipc| ipc.path()) {
        guest_cmd.env(bookclerk_plugin_sdk::GUEST_IPC_DIR_ENV, dir);
    }
    apply_spec_env(&mut guest_cmd, guest_start)?;
    for (key, value) in extra_env {
        guest_cmd.env(*key, value);
    }
    guest_cmd.stderr(Stdio::piped()).kill_on_drop(true);

    // The unsandboxed host serves the CONNECT mux. A jailed gateway cannot
    // dial the host's loopback on Windows (no machine-wide exemption), and
    // the same host-side check is the policy boundary on every OS.
    // The cancel flag exists before either sibling so a revoke during startup
    // reaches the proxy and the initial describe. The challenge is delivered
    // only in the guest environment; the fd number is not the session identity.
    let cancel = Arc::new(AtomicBool::new(false));
    let challenge = new_session_challenge();
    guest_cmd.env(
        bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV,
        hex::encode(challenge),
    );
    // Host-only. The guest learns the tag by asking the proxy, not from env.
    let channel_tag = prepare_channel_tag(session_dir)?;
    #[cfg(unix)]
    let proxy = serve_host_socket_proxy(
        proxy_gateway,
        grant.egress_policy(),
        Arc::clone(&cancel),
        challenge,
        channel_tag.as_deref(),
    )?;
    #[cfg(windows)]
    let proxy = serve_host_socket_proxy(
        proxy_pipes.host_stdout,
        proxy_pipes.host_stdin,
        grant.egress_policy(),
        Arc::clone(&cancel),
        challenge,
        channel_tag.as_deref(),
    )?;

    #[cfg(unix)]
    {
        inherit_unix_gateway(&mut gateway_cmd, &rpc_gateway);
        inherit_unix_guest(&mut guest_cmd, rpc_guest, &proxy_guest)?;
    }
    #[cfg(windows)]
    {
        guest_cmd.stdin(Stdio::piped()).stdout(Stdio::null());
        gateway_cmd.env(JAIL_HANDOFF_ENV, "1");
        guest_cmd.env(JAIL_HANDOFF_ENV, "1");
    }

    let mut gateway = with_fd_spawn_lock(|| gateway_cmd.spawn()).map_err(|err| {
        PluginError::message(format!("could not start gateway for `{id}`: {err}"))
    })?;
    #[cfg(unix)]
    let gateway_identity = ProcessIdentity::capture(gateway.id());
    // Created before the handoff so the jail cannot finish CreateProcess
    // first and miss the event.
    #[cfg(windows)]
    let jail_ready = {
        let pid = gateway.id().ok_or_else(|| {
            PluginError::message(format!("gateway for `{id}` did not report a pid"))
        })?;
        bookclerk_sandbox::JailReady::create(pid).map_err(|err| {
            PluginError::message(format!("could not create the jail-ready event: {err}"))
        })?
    };
    if let Some(stderr) = gateway.stderr.take() {
        forward_guest_stderr(id.to_string(), "gateway", stderr, Arc::clone(&stderr_tail));
    }

    #[cfg(windows)]
    {
        // Longer than the jail's `Local\bookclerk-dacl-tx` wait (120s) so a
        // launch queued behind other grants is not killed while it still holds
        // or waits for that mutex. The e2e spawn deadline is longer than this.
        const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
        // Join the session Job before the handoff. The jail blocks on that
        // line, so its child is created only after the jail is already in the Job.
        if let Some(job) = session_job.as_ref() {
            assign_job(job, &gateway)?;
        }
        let gateway_pid = gateway.id().unwrap_or(0);
        note_spawn_stage(&format!(
            "handoff gateway begin pid={gateway_pid} plugin={id}"
        ));
        if let Err(err) =
            windows_handoff_gateway(&mut gateway, &rpc_pipes.host_stdout, &rpc_pipes.host_stdin)
                .await
        {
            let _ = gateway.kill().await;
            return Err(err);
        }
        note_spawn_stage(&format!(
            "handoff gateway wrote pid={gateway_pid} plugin={id}"
        ));
        let ready_deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        let mut last_ready_note = tokio::time::Instant::now();
        note_spawn_stage(&format!(
            "jail-ready wait begin pid={gateway_pid} plugin={id}"
        ));
        loop {
            if last_ready_note.elapsed() >= std::time::Duration::from_secs(15) {
                let status = match gateway.try_wait() {
                    Ok(Some(code)) => format!("exited:{code}"),
                    Ok(None) => "running".to_string(),
                    Err(err) => format!("wait-error:{err}"),
                };
                note_spawn_stage(&format!(
                    "jail-ready still waiting pid={gateway_pid} status={status} plugin={id}"
                ));
                last_ready_note = tokio::time::Instant::now();
            }
            match jail_ready.is_signaled() {
                Ok(true) => break,
                Ok(false) => {}
                Err(err) => {
                    let _ = gateway.kill().await;
                    return Err(PluginError::message(format!(
                        "jail-ready wait failed for `{id}`: {err}\n{}",
                        spawn_failure_detail(&mut gateway, None, &stderr_tail)
                    )));
                }
            }
            if gateway
                .try_wait()
                .map_err(|err| PluginError::message(format!("gateway wait: {err}")))?
                .is_some()
            {
                return Err(PluginError::message(format!(
                    "gateway for `{id}` exited before its child started\n{}",
                    spawn_failure_detail(&mut gateway, None, &stderr_tail)
                )));
            }
            if tokio::time::Instant::now() >= ready_deadline {
                let _ = gateway.kill().await;
                return Err(PluginError::message(format!(
                    "gateway for `{id}` did not start its child within {}s\n{}",
                    READY_TIMEOUT.as_secs(),
                    spawn_failure_detail(&mut gateway, None, &stderr_tail)
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        note_spawn_stage(&format!(
            "jail-ready signaled pid={gateway_pid} plugin={id}"
        ));
    }

    let guest_spawn = with_fd_spawn_lock(|| guest_cmd.spawn());
    let mut guest = match guest_spawn {
        Ok(child) => child,
        Err(err) => {
            #[cfg(unix)]
            if let Some(identity) = gateway_identity.as_ref() {
                identity.kill_if_same();
            }
            let _ = gateway.kill().await;
            return Err(PluginError::message(format!(
                "could not start native guest for `{id}`: {err}\n{}",
                spawn_failure_detail(&mut gateway, None, &stderr_tail)
            )));
        }
    };
    if let Some(stderr) = guest.stderr.take() {
        forward_guest_stderr(id.to_string(), "guest", stderr, Arc::clone(&stderr_tail));
    }

    #[cfg(windows)]
    {
        let guest_pid = guest.id().unwrap_or(0);
        note_spawn_stage(&format!("handoff guest begin pid={guest_pid} plugin={id}"));
        if let Some(job) = session_job.as_ref() {
            if let Err(err) = assign_job(job, &guest) {
                let _ = gateway.kill().await;
                let _ = guest.kill().await;
                return Err(err);
            }
        }
        if let Err(err) = windows_handoff_guest(
            &mut guest,
            &rpc_pipes.guest_stdin,
            &rpc_pipes.guest_stdout,
            &proxy_pipes.guest_stdin,
            &proxy_pipes.guest_stdout,
        )
        .await
        {
            let _ = gateway.kill().await;
            let _ = guest.kill().await;
            return Err(err);
        }
        note_spawn_stage(&format!("handoff guest wrote pid={guest_pid} plugin={id}"));
    }
    #[cfg(unix)]
    {
        let _ = (rpc_gateway, proxy_guest);
    }

    #[cfg(unix)]
    let identities = SiblingIdentities {
        gateway: gateway_identity,
        guest: ProcessIdentity::capture(guest.id()),
    };
    #[cfg(not(unix))]
    let identities = SiblingIdentities::default();
    let stdin = match gateway.stdin.take() {
        Some(stdin) => stdin,
        None => {
            identities.kill_matching();
            let _ = gateway.kill().await;
            let _ = guest.kill().await;
            return Err(PluginError::message("gateway stdin missing"));
        }
    };
    let stdout = match gateway.stdout.take() {
        Some(stdout) => stdout,
        None => {
            identities.kill_matching();
            let _ = gateway.kill().await;
            let _ = guest.kill().await;
            return Err(PluginError::message("gateway stdout missing"));
        }
    };
    let gateway_pid = gateway.id();
    let guest_pid = guest.id();
    note_spawn_stage(&format!(
        "siblings started gateway_pid={} guest_pid={} plugin={id}",
        gateway_pid.unwrap_or(0),
        guest_pid.unwrap_or(0)
    ));
    Ok(SpawnedParts {
        child: gateway,
        guest: Some(guest),
        stdin,
        stdout,
        gateway_pid,
        guest_pid,
        session_job,
        cancel,
        proxy: Some(proxy),
        identities,
    })
}

/// `bookclerk-jail -- program args`, or `program args` when unconfined.
fn command_for_start(start: &Start, program: &std::path::Path, args: &[String]) -> Command {
    match start {
        Start::Confined { launcher, .. } => {
            let mut cmd = Command::new(launcher);
            cmd.arg("--").arg(program).args(args);
            // New process group so shutdown can signal the jail and its
            // grandchildren (pinned `workerd`) together. A lone SIGKILL of the
            // jail skips its `Drop` and leaves `workerd` holding the session dir.
            #[cfg(unix)]
            cmd.process_group(0);
            cmd
        }
        Start::Unconfined { reason } => {
            tracing::warn!(
                %reason,
                program = %program.display(),
                "starting plugin process WITHOUT a jail; it can reach everything this user can"
            );
            let mut cmd = Command::new(program);
            cmd.args(args);
            #[cfg(unix)]
            cmd.process_group(0);
            cmd
        }
    }
}

/// SIGKILL `pid`'s process group. No-op when the group is already gone.
///
/// Spawn uses `process_group(0)`, so `pid` is the group leader. Callers must
/// confirm the pid still names that leader ([`ProcessIdentity::kill_if_same`]);
/// a recycled pid must not be signalled. A descendant that has called `setsid`
/// is in a new session and is outside this group. Without a delegated cgroup,
/// this path does not own that descendant.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Start time of a process, used to detect pid reuse.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessStart {
    /// Linux `/proc/<pid>/stat` field 22, in clock ticks since boot.
    #[cfg(target_os = "linux")]
    LinuxTicks(u64),
    /// macOS `proc_bsdinfo` start timeval.
    #[cfg(target_os = "macos")]
    MacMicros { sec: u64, usec: u64 },
}

/// Pid plus the start time observed when the child was spawned.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub(crate) struct ProcessIdentity {
    pid: u32,
    start: ProcessStart,
}

#[cfg(unix)]
impl ProcessIdentity {
    /// Read the start time before any wait. `None` when the pid or the start
    /// time cannot be read; callers then must not signal the process group.
    pub(crate) fn capture(pid: Option<u32>) -> Option<Self> {
        let pid = pid?;
        let start = read_process_start(pid)?;
        Some(Self { pid, start })
    }

    fn still_same(&self) -> bool {
        match read_process_start(self.pid) {
            Some(start) => start == self.start,
            None => false,
        }
    }

    /// SIGKILL the process group only while `pid` still has this start time.
    ///
    /// The leader may already be a zombie. Reaping it first makes
    /// [`Self::still_same`] fail, and the rest of the group is left alive.
    pub(crate) fn kill_if_same(&self) {
        if !self.still_same() {
            tracing::warn!(
                pid = self.pid,
                "refusing to signal a process group whose start time no longer matches"
            );
            return;
        }
        kill_process_group(self.pid);
    }

    /// Same pid with a start time that cannot match a live process.
    #[cfg(test)]
    fn with_bogus_start(mut self) -> Self {
        self.start = match self.start {
            #[cfg(target_os = "linux")]
            ProcessStart::LinuxTicks(ticks) => ProcessStart::LinuxTicks(ticks.wrapping_add(1)),
            #[cfg(target_os = "macos")]
            ProcessStart::MacMicros { sec, usec } => ProcessStart::MacMicros {
                sec: sec.wrapping_add(1),
                usec,
            },
        };
        self
    }
}

/// Leaders recorded at spawn. Empty on Windows, where the session Job is the tree.
#[derive(Clone, Default)]
pub(crate) struct SiblingIdentities {
    #[cfg(unix)]
    pub gateway: Option<ProcessIdentity>,
    #[cfg(unix)]
    pub guest: Option<ProcessIdentity>,
}

impl SiblingIdentities {
    /// Signal each recorded group whose start time still matches.
    pub(crate) fn kill_matching(&self) {
        #[cfg(unix)]
        {
            if let Some(identity) = &self.gateway {
                identity.kill_if_same();
            }
            if let Some(identity) = &self.guest {
                identity.kill_if_same();
            }
        }
    }
}

/// True when `pid` is a child that has exited and has not been reaped.
///
/// `waitid` with `WNOWAIT` leaves the zombie in place so
/// [`ProcessIdentity::kill_if_same`] can still read its start time. A later
/// `wait` collects it. A descendant that called `setsid` is not in this
/// process group; this function does not claim that descendant.
#[cfg(unix)]
pub(crate) fn exited_without_reaping(pid: u32) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if rc != 0 {
        return false;
    }
    unsafe { info.assume_init().si_pid() == pid as libc::pid_t }
}

/// Capture identities for leaders that exist right now.
fn sibling_identities(gateway: Option<&Child>, guest: Option<&Child>) -> SiblingIdentities {
    #[cfg(unix)]
    {
        SiblingIdentities {
            gateway: gateway.and_then(|child| ProcessIdentity::capture(child.id())),
            guest: guest.and_then(|child| ProcessIdentity::capture(child.id())),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (gateway, guest);
        SiblingIdentities::default()
    }
}

/// Linux starttime: the 20th whitespace token after the last `)` in `/proc/pid/stat`.
#[cfg(target_os = "linux")]
fn linux_start_ticks(stat: &str) -> Option<u64> {
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn read_process_start(pid: u32) -> Option<ProcessStart> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    linux_start_ticks(&text).map(ProcessStart::LinuxTicks)
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [u8; 16],
    pbi_name: [u8; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

#[cfg(target_os = "macos")]
const PROC_PIDTBSDINFO: i32 = 3;

#[cfg(target_os = "macos")]
#[link(name = "proc")]
extern "C" {
    fn proc_pidinfo(
        pid: i32,
        flavor: i32,
        arg: u64,
        buffer: *mut ProcBsdInfo,
        buffersize: i32,
    ) -> i32;
}

#[cfg(target_os = "macos")]
fn read_process_start(pid: u32) -> Option<ProcessStart> {
    const _: () = assert!(std::mem::offset_of!(ProcBsdInfo, pbi_start_tvsec) == 120);
    let mut info = std::mem::MaybeUninit::<ProcBsdInfo>::zeroed();
    let wrote = unsafe {
        proc_pidinfo(
            pid as i32,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr(),
            std::mem::size_of::<ProcBsdInfo>() as i32,
        )
    };
    if wrote < std::mem::size_of::<ProcBsdInfo>() as i32 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid {
        return None;
    }
    Some(ProcessStart::MacMicros {
        sec: info.pbi_start_tvsec,
        usec: info.pbi_start_tvusec,
    })
}

/// Host journal of package SIDs and paths. Drop revokes only those SIDs.
#[cfg(windows)]
pub(crate) struct AclJournal {
    entries: Vec<bookclerk_sandbox::spawn::AclJournalEntry>,
}

#[cfg(windows)]
impl Drop for AclJournal {
    fn drop(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if let Err(err) = bookclerk_sandbox::spawn::revoke_acl_journal(&self.entries) {
            tracing::warn!(error = %err, "host ACL journal revoke failed");
        }
        self.entries.clear();
    }
}

/// Paths the jail will ACE, taken from the specs and profiles already built.
#[cfg(windows)]
fn build_acl_journal(jail: &GuestJail, plan: &crate::spawn_plan::SpawnPlan) -> AclJournal {
    let mut entries = Vec::new();
    if let (Start::Confined { spec, .. }, Some(session)) = (&jail.start, jail.appcontainer.as_ref())
    {
        entries.extend(bookclerk_sandbox::spawn::plan_acl_journal(
            spec,
            session.package_sid(),
            Some(plan.launcher.as_path()),
            &session.profile_directories(),
        ));
    }
    if let (Some(Start::Confined { spec, .. }), Some(session)) =
        (&jail.guest_start, jail.guest_appcontainer.as_ref())
    {
        entries.extend(bookclerk_sandbox::spawn::plan_acl_journal(
            spec,
            session.package_sid(),
            plan.native_backend.as_deref(),
            &session.profile_directories(),
        ));
    }
    AclJournal { entries }
}

/// Allowlisted host env plus `BOOKCLERK_PLUGIN_*`.
fn apply_common_env(cmd: &mut Command, plugin: &DiscoveredPlugin, id: &str) {
    cmd.current_dir(&plugin.root).env_clear();
    for (key, value) in std::env::vars_os() {
        if crate::rpc::plugin_env_allowed(&key.to_string_lossy()) {
            cmd.env(key, value);
        }
    }
    cmd.env("BOOKCLERK_PLUGIN_ID", id);
    cmd.env("BOOKCLERK_PLUGIN_ROOT", &plugin.root);
    cmd.env("BOOKCLERK_PLUGIN_TOML", plugin.root.join("plugin.toml"));
}

fn apply_temp_and_home(cmd: &mut Command, tmp: &std::path::Path, home: &std::path::Path) {
    for key in ["TMPDIR", "TEMP", "TMP"] {
        cmd.env(key, tmp);
    }
    cmd.env("HOME", home);
}

/// 32 random bytes the guest must write before the proxy mux starts.
fn channel_ident_requested(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true"))
}

/// Write a unique tag for this session when the test env is set.
fn prepare_channel_tag(session_dir: &std::path::Path) -> Result<Option<String>> {
    write_channel_tag(
        session_dir,
        channel_ident_requested(std::env::var(TEST_CHANNEL_IDENT_ENV).ok().as_deref()),
    )
}

fn write_channel_tag(session_dir: &std::path::Path, enabled: bool) -> Result<Option<String>> {
    if !enabled {
        return Ok(None);
    }
    let tag = format!("ch-{}", uuid::Uuid::new_v4().simple());
    std::fs::write(session_dir.join(TEST_CHANNEL_TAG_FILE), &tag)
        .map_err(|err| PluginError::message(format!("could not record the channel tag: {err}")))?;
    Ok(Some(tag))
}

fn new_session_challenge() -> [u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN] {
    let mut out = [0u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    out[..16].copy_from_slice(first.as_bytes());
    out[16..].copy_from_slice(second.as_bytes());
    out
}

fn apply_spec_env(cmd: &mut Command, start: &Start) -> Result<()> {
    if let Start::Confined { spec, .. } = start {
        cmd.env(
            bookclerk_sandbox::SPEC_ENV,
            serde_json::to_string(spec.as_ref()).map_err(|err| {
                PluginError::message(format!("could not encode the jail spec: {err}"))
            })?,
        );
    }
    Ok(())
}

#[cfg(unix)]
fn inherit_unix_gateway(cmd: &mut Command, rpc: &DuplexLink) {
    let rpc_fd = rpc.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            bookclerk_sandbox::inherit_fd_at(rpc_fd, GATEWAY_RPC_FD)?;
            if rpc_fd != GATEWAY_RPC_FD {
                libc::close(rpc_fd);
            }
            Ok(())
        });
    }
    cmd.env(GATEWAY_GUEST_RPC_ENV, format!("fd:{GATEWAY_RPC_FD}"));
}

/// Serve the guest CONNECT mux in this process.
///
/// `link` is the host end of the inherited proxy. The guest holds the other
/// end. Dialing here reaches host loopback; the gateway AppContainer cannot.
#[cfg(unix)]
fn serve_host_socket_proxy(
    link: DuplexLink,
    policy: bookclerk_plugin_manifest::EgressPolicy,
    fence: Arc<AtomicBool>,
    challenge: [u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN],
    channel_tag: Option<&str>,
) -> Result<bookclerk_workerd::socket_proxy::ProxyServer> {
    use std::os::unix::net::UnixStream;
    let std_stream = UnixStream::from(link.into_owned_fd());
    std_stream
        .set_nonblocking(true)
        .map_err(|err| PluginError::message(format!("host socket proxy nonblocking: {err}")))?;
    let stream = tokio::net::UnixStream::from_std(std_stream)
        .map_err(|err| PluginError::message(format!("host socket proxy wrap: {err}")))?;
    let started = match channel_tag {
        Some(tag) => bookclerk_workerd::socket_proxy::spawn_link_with_challenge_tag(
            stream, policy, fence, challenge, tag,
        ),
        None => bookclerk_workerd::socket_proxy::spawn_link_with_challenge(
            stream, policy, fence, challenge,
        ),
    };
    started.map_err(|err| PluginError::message(format!("host socket proxy failed to start: {err}")))
}

/// Serve the guest CONNECT mux on two unidirectional overlapped pipes.
#[cfg(windows)]
#[allow(unsafe_code)] // NamedPipeClient::from_raw_handle takes the inherited pipe.
fn serve_host_socket_proxy(
    read: DuplexHalf,
    write: DuplexHalf,
    policy: bookclerk_plugin_manifest::EgressPolicy,
    fence: Arc<AtomicBool>,
    challenge: [u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN],
    channel_tag: Option<&str>,
) -> Result<bookclerk_workerd::socket_proxy::ProxyServer> {
    use std::os::windows::io::IntoRawHandle;
    let read = unsafe {
        tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle(
            read.into_owned_handle().into_raw_handle(),
        )
    }
    .map_err(|err| PluginError::message(format!("host proxy read pipe: {err}")))?;
    let write = unsafe {
        tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle(
            write.into_owned_handle().into_raw_handle(),
        )
    }
    .map_err(|err| PluginError::message(format!("host proxy write pipe: {err}")))?;
    let started = match channel_tag {
        Some(tag) => bookclerk_workerd::socket_proxy::spawn_halves_with_challenge_tag(
            read, write, policy, fence, challenge, tag,
        ),
        None => bookclerk_workerd::socket_proxy::spawn_halves_with_challenge(
            read, write, policy, fence, challenge,
        ),
    };
    started.map_err(|err| PluginError::message(format!("host socket proxy failed to start: {err}")))
}

#[cfg(unix)]
fn inherit_unix_guest(cmd: &mut Command, rpc: DuplexLink, proxy: &DuplexLink) -> Result<()> {
    let rpc_in = rpc
        .try_clone()
        .map_err(|err| PluginError::message(format!("clone guest RPC end: {err}")))?;
    cmd.stdin(Stdio::from(rpc_in.into_owned_fd()));
    cmd.stdout(Stdio::from(rpc.into_owned_fd()));
    let proxy_fd = proxy.as_raw_fd();
    let extra_fd = injected_unix_fd()?;
    if extra_fd == Some(proxy_fd) {
        return Err(PluginError::message(
            "test extra endpoint fd is the configured proxy socket",
        ));
    }
    unsafe {
        cmd.pre_exec(move || {
            inherit_guest_proxy_and_extra(proxy_fd, extra_fd)?;
            Ok(())
        });
    }
    cmd.env(SOCKET_PROXY_ENV, format!("fd:{GUEST_PROXY_FD}"));
    if matches!(observe_kind()?, ObserveKind::Slot) {
        cmd.env(
            bookclerk_plugin_sdk::TEST_EXTRA_CANDIDATE_ENV,
            format!("fd:{}", bookclerk_sandbox::TEST_EXTRA_ENDPOINT_FD),
        );
    }
    if extra_fd.is_some() {
        cmd.env(
            bookclerk_plugin_sdk::TEST_EXTRA_ENDPOINT_ENV,
            format!("fd:{}", bookclerk_sandbox::TEST_EXTRA_ENDPOINT_FD),
        );
    }
    Ok(())
}

/// `fd:<n>` from [`TEST_INJECT_EXTRA_ENDPOINT_ENV`], or `None` when unset.
///
/// A Windows `handle:` value and a descriptor below 3 are errors. The number
/// may equal the guest proxy slot: [`inherit_guest_proxy_and_extra`] copies
/// the socket before that slot is overwritten. The same descriptor as the
/// configured proxy is rejected later. Production leaves the variable unset.
#[cfg(unix)]
fn injected_unix_fd() -> Result<Option<i32>> {
    match std::env::var(TEST_INJECT_EXTRA_ENDPOINT_ENV) {
        Ok(value) => parse_injected_unix_fd(&value),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(PluginError::message(format!(
            "could not read {TEST_INJECT_EXTRA_ENDPOINT_ENV}: {err}"
        ))),
    }
}

/// Parse a Unix inject spec. Empty is "do not inject".
#[cfg(any(unix, test))]
fn parse_injected_unix_fd(value: &str) -> Result<Option<i32>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let Some(rest) = value.strip_prefix("fd:") else {
        return Err(PluginError::message(format!(
            "{TEST_INJECT_EXTRA_ENDPOINT_ENV} must be fd:<n> on unix"
        )));
    };
    let fd: i32 = rest.trim().parse().map_err(|_| {
        PluginError::message(format!(
            "{TEST_INJECT_EXTRA_ENDPOINT_ENV} has no file descriptor"
        ))
    })?;
    if fd < 3 {
        return Err(PluginError::message(format!(
            "{TEST_INJECT_EXTRA_ENDPOINT_ENV} fd {fd} collides with stdio"
        )));
    }
    Ok(Some(fd))
}

/// Place the proxy at `proxy_slot` and, when set, the extra socket at `extra_slot`.
///
/// Both objects are copied above the slots first. A source whose number is the
/// proxy slot is therefore still the extra socket after the proxy move.
#[cfg(unix)]
fn inherit_guest_proxy_and_extra(
    proxy_fd: std::os::fd::RawFd,
    extra_fd: Option<std::os::fd::RawFd>,
) -> std::io::Result<()> {
    inherit_mapped_fds(
        proxy_fd,
        GUEST_PROXY_FD,
        extra_fd,
        bookclerk_sandbox::TEST_EXTRA_ENDPOINT_FD,
    )
}

/// Copy `proxy_fd` onto `proxy_slot` and optional `extra_fd` onto `extra_slot`.
#[cfg(unix)]
fn inherit_mapped_fds(
    proxy_fd: std::os::fd::RawFd,
    proxy_slot: std::os::fd::RawFd,
    extra_fd: Option<std::os::fd::RawFd>,
    extra_slot: std::os::fd::RawFd,
) -> std::io::Result<()> {
    if extra_fd == Some(proxy_fd) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "extra fd is the proxy fd",
        ));
    }
    if proxy_slot == extra_slot {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "proxy slot and extra slot are the same descriptor",
        ));
    }
    let floor = proxy_slot.max(extra_slot).saturating_add(1);
    let proxy_copy = dup_cloexec_at_least(proxy_fd, floor)?;
    let extra_copy = match extra_fd {
        Some(src) => match dup_cloexec_at_least(src, floor) {
            Ok(copy) => Some(copy),
            Err(err) => {
                unsafe { libc::close(proxy_copy) };
                return Err(err);
            }
        },
        None => None,
    };
    let installed: std::io::Result<()> = (|| {
        bookclerk_sandbox::inherit_fd_at(proxy_copy, proxy_slot)?;
        if let Some(copy) = extra_copy {
            bookclerk_sandbox::inherit_fd_at(copy, extra_slot)?;
        }
        Ok(())
    })();
    unsafe { libc::close(proxy_copy) };
    if let Some(copy) = extra_copy {
        unsafe { libc::close(copy) };
    }
    installed?;
    close_unless_installed(proxy_fd, proxy_slot, extra_slot, extra_fd.is_some());
    if let Some(src) = extra_fd {
        close_unless_installed(src, proxy_slot, extra_slot, true);
    }
    Ok(())
}

/// Duplicate `fd` at or above `min`, with `FD_CLOEXEC` set on the copy.
#[cfg(unix)]
fn dup_cloexec_at_least(
    fd: std::os::fd::RawFd,
    min: std::os::fd::RawFd,
) -> std::io::Result<std::os::fd::RawFd> {
    let start = min.max(3);
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, start) };
    if copy < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(copy)
    }
}

/// Close a pre-move descriptor that is not one of the installed slots.
#[cfg(unix)]
fn close_unless_installed(
    fd: std::os::fd::RawFd,
    proxy_slot: std::os::fd::RawFd,
    extra_slot: std::os::fd::RawFd,
    extra_live: bool,
) {
    if fd == proxy_slot || (extra_live && fd == extra_slot) {
        return;
    }
    unsafe { libc::close(fd) };
}

/// `handle:<read>,handle:<write>` from [`TEST_INJECT_EXTRA_ENDPOINT_ENV`].
///
/// Empty is "do not inject". A Unix `fd:` value, a missing half, or the same
/// handle twice is an error.
#[cfg(any(windows, test))]
fn parse_injected_handle_pair(value: &str) -> Result<Option<(u64, u64)>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let Some((read, write)) = value.split_once(',') else {
        return Err(PluginError::message(format!(
            "{TEST_INJECT_EXTRA_ENDPOINT_ENV} must be handle:<read>,handle:<write>"
        )));
    };
    let read = parse_handle_token(read)?;
    let write = parse_handle_token(write)?;
    if read == write {
        return Err(PluginError::message(format!(
            "{TEST_INJECT_EXTRA_ENDPOINT_ENV} read and write handles are the same value"
        )));
    }
    Ok(Some((read, write)))
}

/// One `handle:<n>` token.
#[cfg(any(windows, test))]
fn parse_handle_token(token: &str) -> Result<u64> {
    let Some(rest) = token.trim().strip_prefix("handle:") else {
        return Err(PluginError::message(format!(
            "{TEST_INJECT_EXTRA_ENDPOINT_ENV} must be handle:<read>,handle:<write>"
        )));
    };
    rest.trim().parse().map_err(|_| {
        PluginError::message(format!(
            "{TEST_INJECT_EXTRA_ENDPOINT_ENV} has no handle value"
        ))
    })
}

/// Guest pipe ends in this process, when the test inject env is set.
#[cfg(windows)]
fn injected_windows_handles() -> Result<Option<(u64, u64)>> {
    match std::env::var(TEST_INJECT_EXTRA_ENDPOINT_ENV) {
        Ok(value) => parse_injected_handle_pair(&value),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(PluginError::message(format!(
            "could not read {TEST_INJECT_EXTRA_ENDPOINT_ENV}: {err}"
        ))),
    }
}

/// Whether the host asked the guest to classify a known candidate slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObserveKind {
    /// No candidate metadata. The guest must report `not-run`.
    Off,
    /// Publish the fixed Unix slot, or the injected Windows handles.
    Slot,
    /// Windows candidate handles in this process. Do not transfer them unless
    /// [`TEST_INJECT_EXTRA_ENDPOINT_ENV`] names the same pair.
    #[cfg(windows)]
    Handles(u64, u64),
}

/// Parse [`TEST_OBSERVE_EXTRA_ENDPOINT_ENV`].
fn parse_observe_request(value: Option<&str>) -> Result<ObserveKind> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(ObserveKind::Off);
    };
    if value == "1" || value == "true" {
        return Ok(ObserveKind::Slot);
    }
    parse_observe_platform(value)
}

/// Observation metadata is not implemented on this platform.
#[cfg(not(any(unix, windows)))]
fn parse_observe_platform(value: &str) -> Result<ObserveKind> {
    Err(PluginError::message(format!(
        "{TEST_OBSERVE_EXTRA_ENDPOINT_ENV} is not supported on this platform, got {value}"
    )))
}

/// Unix observation metadata is the fixed slot, not a handle spec.
#[cfg(unix)]
fn parse_observe_platform(value: &str) -> Result<ObserveKind> {
    Err(PluginError::message(format!(
        "{TEST_OBSERVE_EXTRA_ENDPOINT_ENV} must be 1 on unix, got {value}"
    )))
}

/// Windows may name the candidate handles without transferring them.
#[cfg(windows)]
fn parse_observe_platform(value: &str) -> Result<ObserveKind> {
    match parse_injected_handle_pair(value)? {
        Some((read, write)) => Ok(ObserveKind::Handles(read, write)),
        None => Err(PluginError::message(format!(
            "{TEST_OBSERVE_EXTRA_ENDPOINT_ENV} must be 1 or handle:<read>,handle:<write>"
        ))),
    }
}

/// Host observation request. Unset is [`ObserveKind::Off`].
fn observe_kind() -> Result<ObserveKind> {
    match std::env::var(TEST_OBSERVE_EXTRA_ENDPOINT_ENV) {
        Ok(value) => parse_observe_request(Some(&value)),
        Err(std::env::VarError::NotPresent) => Ok(ObserveKind::Off),
        Err(err) => Err(PluginError::message(format!(
            "could not read {TEST_OBSERVE_EXTRA_ENDPOINT_ENV}: {err}"
        ))),
    }
}

/// How a Windows test extra endpoint is duplicated into the jail.
#[cfg(windows)]
struct WindowsExtraPlan {
    /// Guest pipe ends in this process.
    read: u64,
    write: u64,
    /// Keep duplicating until the jail value is outside the loader's range.
    raise: bool,
    /// Place the duplicates on the guest inherit list.
    transfer: bool,
    /// Publish candidate metadata for those jail values.
    candidate: bool,
}

/// Separate transfer permission from candidate metadata.
#[cfg(windows)]
fn windows_extra_plan() -> Result<Option<WindowsExtraPlan>> {
    let inject = injected_windows_handles()?;
    let observe = observe_kind()?;
    match (inject, observe) {
        (None, ObserveKind::Off) => Ok(None),
        (None, ObserveKind::Slot) => Err(PluginError::message(format!(
            "{TEST_OBSERVE_EXTRA_ENDPOINT_ENV} is 1 but {TEST_INJECT_EXTRA_ENDPOINT_ENV} is unset; \
             Windows observation needs handle:<read>,handle:<write>"
        ))),
        (Some((read, write)), ObserveKind::Off) => Ok(Some(WindowsExtraPlan {
            read,
            write,
            raise: false,
            transfer: true,
            candidate: false,
        })),
        (Some((read, write)), ObserveKind::Slot) => Ok(Some(WindowsExtraPlan {
            read,
            write,
            raise: false,
            transfer: true,
            candidate: true,
        })),
        (Some((read, write)), ObserveKind::Handles(observed_read, observed_write)) => {
            if (read, write) != (observed_read, observed_write) {
                return Err(PluginError::message(
                    "observation metadata names a different endpoint than the transfer",
                ));
            }
            Ok(Some(WindowsExtraPlan {
                read,
                write,
                raise: false,
                transfer: true,
                candidate: true,
            }))
        }
        (None, ObserveKind::Handles(read, write)) => Ok(Some(WindowsExtraPlan {
            read,
            write,
            raise: true,
            transfer: false,
            candidate: true,
        })),
    }
}

/// `DuplicateHandle` source from a numeric handle in this process.
#[cfg(windows)]
fn raw_handle_from_u64(value: u64) -> std::os::windows::io::RawHandle {
    value as usize as std::os::windows::io::RawHandle
}

#[cfg(windows)]
async fn windows_handoff_gateway(
    child: &mut Child,
    rpc_read: &DuplexHalf,
    rpc_write: &DuplexHalf,
) -> Result<()> {
    let target = process_handle(child)?;
    // Read half is guest → gateway. Write half is gateway → guest.
    // The CONNECT mux stays in the host; this jail only receives guest RPC.
    let rpc_read_h = bookclerk_sandbox::duplicate_handle_into(rpc_read.as_raw_handle(), target)
        .map_err(|err| PluginError::message(format!("DuplicateHandle gateway RPC read: {err}")))?;
    let rpc_write_h = bookclerk_sandbox::duplicate_handle_into(rpc_write.as_raw_handle(), target)
        .map_err(|err| {
        PluginError::message(format!("DuplicateHandle gateway RPC write: {err}"))
    })?;
    let handoff = JailHandoff {
        v: JailHandoff::VERSION,
        stdin: None,
        stdout: None,
        extra: vec![
            JailHandoffExtra::inherited(GATEWAY_GUEST_RPC_ENV, rpc_read_h),
            JailHandoffExtra::inherited(GATEWAY_GUEST_RPC_WRITE_ENV, rpc_write_h),
        ],
    };
    write_handoff_line(child, &handoff).await
}

#[cfg(windows)]
async fn windows_handoff_guest(
    child: &mut Child,
    rpc_stdin: &DuplexHalf,
    rpc_stdout: &DuplexHalf,
    proxy_read: &DuplexHalf,
    proxy_write: &DuplexHalf,
) -> Result<()> {
    // Build the handoff before any await. A raw process handle held across
    // `.await` makes this future `!Send`.
    let handoff = guest_jail_handoff(child, rpc_stdin, rpc_stdout, proxy_read, proxy_write)?;
    write_handoff_line(child, &handoff).await?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.shutdown().await;
    }
    Ok(())
}

/// Duplicate guest handles into the jail. Sync so the process handle is not
/// captured by the spawn future.
#[cfg(windows)]
fn guest_jail_handoff(
    child: &Child,
    rpc_stdin: &DuplexHalf,
    rpc_stdout: &DuplexHalf,
    proxy_read: &DuplexHalf,
    proxy_write: &DuplexHalf,
) -> Result<JailHandoff> {
    let target = process_handle(child)?;
    // Separate pipes. Duplicating one duplex end for both stdio handles lets a
    // synchronous read lock the write, and the inherit list also rejects a
    // repeated handle value.
    let rpc_in = bookclerk_sandbox::duplicate_handle_into(rpc_stdin.as_raw_handle(), target)
        .map_err(|err| PluginError::message(format!("DuplicateHandle guest RPC stdin: {err}")))?;
    let rpc_out = bookclerk_sandbox::duplicate_handle_into(rpc_stdout.as_raw_handle(), target)
        .map_err(|err| PluginError::message(format!("DuplicateHandle guest RPC stdout: {err}")))?;
    let proxy_read_h = bookclerk_sandbox::duplicate_handle_into(proxy_read.as_raw_handle(), target)
        .map_err(|err| PluginError::message(format!("DuplicateHandle guest proxy read: {err}")))?;
    let proxy_write_h =
        bookclerk_sandbox::duplicate_handle_into(proxy_write.as_raw_handle(), target).map_err(
            |err| PluginError::message(format!("DuplicateHandle guest proxy write: {err}")),
        )?;
    let mut extra = vec![
        JailHandoffExtra::inherited(SOCKET_PROXY_ENV, proxy_read_h),
        JailHandoffExtra::inherited(bookclerk_sandbox::SOCKET_PROXY_WRITE_ENV, proxy_write_h),
    ];
    if let Some(plan) = windows_extra_plan()? {
        let WindowsExtraPlan {
            read,
            write,
            raise,
            transfer,
            candidate,
        } = plan;
        let duplicate = |value: u64, what: &str| {
            let raw = raw_handle_from_u64(value);
            let duplicated = if raise {
                bookclerk_sandbox::duplicate_handle_into_at_least(raw, target, 0x4000)
            } else {
                bookclerk_sandbox::duplicate_handle_into(raw, target)
            };
            duplicated.map_err(|err| {
                PluginError::message(format!("DuplicateHandle test extra {what}: {err}"))
            })
        };
        let read_h = duplicate(read, "read")?;
        let write_h = duplicate(write, "write")?;
        if transfer {
            extra.push(JailHandoffExtra::inherited(
                bookclerk_plugin_sdk::TEST_EXTRA_ENDPOINT_ENV,
                read_h,
            ));
            extra.push(JailHandoffExtra::inherited(
                bookclerk_plugin_sdk::TEST_EXTRA_ENDPOINT_WRITE_ENV,
                write_h,
            ));
        }
        if candidate {
            extra.push(JailHandoffExtra::named_only(
                bookclerk_plugin_sdk::TEST_EXTRA_CANDIDATE_ENV,
                read_h,
            ));
            extra.push(JailHandoffExtra::named_only(
                bookclerk_plugin_sdk::TEST_EXTRA_CANDIDATE_WRITE_ENV,
                write_h,
            ));
        }
    }
    Ok(JailHandoff {
        v: JailHandoff::VERSION,
        stdin: Some(rpc_in),
        stdout: Some(rpc_out),
        extra,
    })
}

#[cfg(windows)]
async fn write_handoff_line(child: &mut Child, handoff: &JailHandoff) -> Result<()> {
    let line = handoff
        .to_line()
        .map_err(|err| PluginError::message(format!("encode jail handoff: {err}")))?;
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| PluginError::message("jail stdin missing for handoff"))?;
    write_all_noted(stdin, line.as_bytes(), "handoff write").await?;
    write_all_noted(stdin, b"\n", "handoff newline").await?;
    wait_noted(stdin.flush(), "handoff flush")
        .await
        .map_err(|err| PluginError::message(format!("flush jail handoff: {err}")))?;
    Ok(())
}

/// Poll `write` until it finishes, logging every 15s so a full pipe is visible.
#[cfg(windows)]
async fn write_all_noted(
    stdin: &mut tokio::process::ChildStdin,
    bytes: &[u8],
    stage: &str,
) -> Result<()> {
    let write = stdin.write_all(bytes);
    wait_noted(write, stage)
        .await
        .map_err(|err| PluginError::message(format!("{stage}: {err}")))
}

/// Drive `fut` and record `stage still waiting` while it is pending.
#[cfg(windows)]
async fn wait_noted<F, T>(fut: F, stage: &str) -> std::io::Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    let mut fut = std::pin::pin!(fut);
    let started = tokio::time::Instant::now();
    loop {
        tokio::select! {
            result = fut.as_mut() => return result,
            () = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
                note_spawn_stage(&format!(
                    "{stage} still waiting elapsed_ms={}",
                    started.elapsed().as_millis()
                ));
            }
        }
    }
}

#[cfg(windows)]
fn assign_job(job: &bookclerk_sandbox::SessionJob, child: &Child) -> Result<()> {
    job.assign(process_handle(child)?)
        .map_err(|err| PluginError::message(format!("AssignProcessToJobObject: {err}")))
}

#[cfg(windows)]
fn process_handle(child: &Child) -> Result<std::os::windows::io::RawHandle> {
    child
        .raw_handle()
        .ok_or_else(|| PluginError::message("jail process handle is gone"))
}

#[cfg(windows)]
type WindowsSessionJob = bookclerk_sandbox::SessionJob;
#[cfg(not(windows))]
type WindowsSessionJob = ();

/// Why creating the outer Windows session Job failed.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OuterJobFailure {
    /// The kernel does not implement the Job feature that was requested.
    Unsupported,
    /// Create or configure failed for a reason other than missing support.
    Failed,
}

/// What the host does with an outer-Job outcome.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OuterJobDecision {
    /// Job exists and siblings may be assigned to it.
    Present,
    /// Best-effort / off continued and recorded that there is no outer Job.
    AbsentUnsupported,
    /// Required, or a failure that is not an explicit lack of support.
    FailClosed,
}

/// Required isolation fails closed. Best-effort may continue only when the
/// missing piece is explicitly unsupported.
#[cfg_attr(not(windows), allow(dead_code))]
fn decide_outer_job(
    isolation: bookclerk_config::Isolation,
    failure: Option<OuterJobFailure>,
) -> OuterJobDecision {
    match failure {
        None => OuterJobDecision::Present,
        Some(OuterJobFailure::Unsupported)
            if matches!(
                isolation,
                bookclerk_config::Isolation::BestEffort | bookclerk_config::Isolation::Off
            ) =>
        {
            OuterJobDecision::AbsentUnsupported
        }
        Some(_) => OuterJobDecision::FailClosed,
    }
}

/// Create the outer session Job, or fail before either sibling starts.
#[cfg(windows)]
fn prepare_windows_session_job(
    jail: &GuestJail,
    runtime: crate::GuestRuntimeKind,
) -> Result<Option<bookclerk_sandbox::SessionJob>> {
    let limits = crate::jail::windows_outer_job_limits(jail.session_limits, runtime);
    match bookclerk_sandbox::SessionJob::create(&limits) {
        Ok(job) => Ok(Some(job)),
        Err(err) => {
            let failure = if err.kind() == std::io::ErrorKind::Unsupported {
                OuterJobFailure::Unsupported
            } else {
                OuterJobFailure::Failed
            };
            match decide_outer_job(jail.isolation, Some(failure)) {
                OuterJobDecision::Present => Err(PluginError::message(
                    "outer session Job decision was Present after a create failure",
                )),
                OuterJobDecision::AbsentUnsupported => {
                    tracing::warn!(
                        isolation = jail.isolation.as_str(),
                        error = %err,
                        "outer session Job is unsupported; continuing with no outer job"
                    );
                    Ok(None)
                }
                OuterJobDecision::FailClosed => Err(PluginError::message(format!(
                    "could not create the outer session Job ({err}); no sibling was started"
                ))),
            }
        }
    }
}

/// Keys the host must never place on the native sibling.
#[cfg(test)]
fn guest_env_forbidden_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.starts_with("BOOKCLERK_WORKERD_GRANT_")
        || upper.starts_with("BOOKCLERK_JAIL_")
        || upper == GATEWAY_GUEST_RPC_ENV
        || upper == GATEWAY_GUEST_RPC_WRITE_ENV
        || upper == GATEWAY_PROXY_ENV
        || upper == bookclerk_sandbox::GATEWAY_PROXY_WRITE_ENV
        || upper == WORKERD_STATE_DIR_ENV
        || upper == "BOOKCLERK_NATIVE_BACKEND"
        || upper == "BOOKCLERK_NESTED_NATIVE_JAIL"
        || upper == "BOOKCLERK_NESTED_JAIL_ENFORCEMENT"
        || upper == "BOOKCLERK_NESTED_AC_PROFILE"
        || upper == "BOOKCLERK_NESTED_AC_SID"
}

/// Guest environment keys after the host allowlist + curated bootstrap.
///
/// Used by the env-contract unit test. `extra` is applied last (sqlite / S3).
#[cfg(test)]
fn curated_guest_env_keys(extra: &[(&str, OsString)]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy().into_owned();
        if crate::rpc::plugin_env_allowed(&name) {
            keys.insert(name);
        }
    }
    for key in [
        "BOOKCLERK_PLUGIN_ID",
        "BOOKCLERK_PLUGIN_ROOT",
        "BOOKCLERK_PLUGIN_TOML",
        "HOME",
        "TMPDIR",
        "TEMP",
        "TMP",
        SOCKET_PROXY_ENV,
    ] {
        keys.insert(key.to_string());
    }
    for (key, _) in extra {
        keys.insert((*key).to_string());
    }
    keys.retain(|k| !guest_env_forbidden_key(k));
    keys
}

/// Lines of guest stderr retained for spawn/describe failure messages.
const STDERR_TAIL_LINES: usize = 40;
/// Bytes kept from one guest stderr line, including the sibling tag.
const STDERR_TAIL_LINE_BYTES: usize = 512;
/// Bytes kept across the per-session tail. Entry count is not the budget.
const STDERR_TAIL_TOTAL_BYTES: usize = 8 * 1024;
/// Marker appended when a guest line is longer than the cap.
const STDERR_TRUNCATED: &str = "[truncated]";

/// Re-emits each guest stderr line through tracing so `bookclerkd` JSON logs
/// stay structured. ANSI from guest formatters is stripped so JSON
/// does not encode CSI as `\u001b`. A byte-capped per-session tail keeps
/// panic text. Untrusted guest text is not copied into the process-wide
/// stage ring. Gateway lines that our jail already marked `bookclerk-spawn:`
/// are the exception: they are host diagnostics and stay in that ring.
fn forward_guest_stderr(
    plugin: String,
    tag: &'static str,
    stderr: ChildStderr,
    tail: Arc<Mutex<VecDeque<String>>>,
) {
    tokio::spawn(async move {
        drain_guest_stderr(&plugin, tag, stderr, tail.as_ref()).await;
    });
}

/// Read guest stderr in capped chunks until EOF.
///
/// `BufReader::lines` would allocate the whole line before a newline. This
/// keeps at most [`STDERR_TAIL_LINE_BYTES`] of each line and still reads the
/// rest so a noisy sibling cannot stall the pipe.
async fn drain_guest_stderr<R>(plugin: &str, tag: &str, reader: R, tail: &Mutex<VecDeque<String>>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = GuestLineReader::new(reader);
    while let Ok(Some(line)) = reader.next_line().await {
        let line = bookclerk_config::strip_ansi_escapes(&line);
        if line.is_empty() {
            continue;
        }
        if tag == "gateway" && line.contains("bookclerk-spawn:") {
            let _ = push_stage(&format!("[{tag}] {line}"));
        }
        push_stderr_tail(tail, &format!("[{tag}] {line}"));
        emit_guest_stderr_record(plugin, tag, &line);
    }
}

fn emit_guest_stderr_record(plugin: &str, tag: &str, line: &str) {
    let line = truncate_utf8(line, STDERR_TAIL_LINE_BYTES);
    tracing::info!(target: "bookclerk::spawn", plugin, sibling = tag, "{line}");
    if spawn_diag_stderr_enabled() {
        eprintln!("bookclerk-spawn: [{tag}] {line}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

fn push_stderr_tail(tail: &Mutex<VecDeque<String>>, line: &str) {
    let Ok(mut buf) = tail.lock() else {
        return;
    };
    push_capped_line(
        &mut buf,
        line,
        STDERR_TAIL_LINE_BYTES,
        STDERR_TAIL_TOTAL_BYTES,
        STDERR_TAIL_LINES,
    );
}

/// Chunked line reader with a hard cap and a discard-until-newline drain.
struct GuestLineReader<R> {
    inner: R,
    pending: VecDeque<u8>,
}

impl<R> GuestLineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pending: VecDeque::new(),
        }
    }
}

impl<R> GuestLineReader<R>
where
    R: AsyncRead + Unpin,
{
    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut saw = false;
        loop {
            if self.pending.is_empty() {
                let mut buf = [0_u8; 4096];
                let n = self.inner.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                self.pending.extend(buf[..n].iter().copied());
            }
            while let Some(byte) = self.pending.pop_front() {
                if byte == b'\n' {
                    return Ok(Some(bounded_guest_line(&kept, truncated)));
                }
                saw = true;
                if byte == b'\r' {
                    continue;
                }
                if kept.len() < STDERR_TAIL_LINE_BYTES {
                    kept.push(byte);
                } else {
                    truncated = true;
                }
            }
        }
        if !saw {
            return Ok(None);
        }
        Ok(Some(bounded_guest_line(&kept, truncated)))
    }
}

fn bounded_guest_line(bytes: &[u8], truncated: bool) -> String {
    let valid = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            let up = err.valid_up_to();
            std::str::from_utf8(&bytes[..up]).unwrap_or("")
        }
    };
    if !truncated {
        return valid.to_string();
    }
    let room = STDERR_TAIL_LINE_BYTES.saturating_sub(STDERR_TRUNCATED.len());
    let mut out = truncate_utf8(valid, room).to_string();
    out.push_str(STDERR_TRUNCATED);
    out
}

/// Guest process status plus captured stderr, for describe/spawn failures.
pub(crate) fn spawn_failure_detail(
    child: &mut Child,
    guest: Option<&mut Child>,
    stderr_tail: &Arc<Mutex<VecDeque<String>>>,
) -> String {
    let mut parts = vec![child_status("gateway", child)];
    if let Some(guest) = guest {
        parts.push(child_status("guest", guest));
    }
    let status = parts.join("; ");
    let stderr = stderr_tail_text(stderr_tail);
    if stderr.is_empty() {
        status
    } else {
        format!("{status}\n--- guest stderr ---\n{stderr}")
    }
}

fn child_status(tag: &str, child: &mut Child) -> String {
    // Do not reap here. A later group kill still needs the leader's start time.
    #[cfg(unix)]
    {
        match child.id() {
            Some(pid) if exited_without_reaping(pid) => format!("{tag} exited"),
            Some(_) => format!("{tag} still running"),
            None => format!("{tag} already reaped"),
        }
    }
    #[cfg(not(unix))]
    {
        match child.try_wait() {
            Ok(Some(st)) => format!("{tag} exited: {st}"),
            Ok(None) => format!("{tag} still running"),
            Err(e) => format!("{tag} wait error: {e}"),
        }
    }
}

/// Attaches [`spawn_failure_detail`] without losing the ABI error class.
pub(crate) fn with_spawn_detail(err: PluginError, extra: String) -> PluginError {
    match err {
        PluginError::Unavailable(message) => {
            PluginError::unavailable(format!("{message}; {extra}"))
        }
        PluginError::Message(message) => PluginError::message(format!("{message}; {extra}")),
        PluginError::Abi { code, message } => {
            PluginError::from_abi(Some(&code), format!("{message}; {extra}"))
        }
        other => PluginError::message(format!("{other}; {extra}")),
    }
}

/// Applies host-implied network overlays to a persisted spawn grant.
pub(crate) fn effective_spawn_grant(
    persisted: &PluginGrant,
    plugin: &DiscoveredPlugin,
    config: &Config,
) -> PluginGrant {
    let mut grant = persisted.clone();
    crate::consent::overlay_host_implied_network(
        &mut grant,
        plugin,
        config,
        &overlay_discovered_plugins(config, plugin),
    );
    grant
}

/// Occupancy list used to uniquify host overlays, always including `plugin`.
///
/// Must **not** call [`crate::discover_plugins`]: that re-hashes every staged
/// payload (debug guest binaries are hundreds of MiB) on each spawn.
pub(crate) fn overlay_discovered_plugins(
    config: &Config,
    plugin: &DiscoveredPlugin,
) -> Vec<DiscoveredPlugin> {
    match crate::discover::discover_occupancy_plugins(config) {
        Ok(mut list) => {
            list.retain(|found| found.root != plugin.root);
            list.push(plugin.clone());
            list
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "overlay occupancy scan failed; skipping host-implied network overlay"
            );
            Vec::new()
        }
    }
}

fn stderr_tail_text(tail: &Arc<Mutex<VecDeque<String>>>) -> String {
    tail.lock()
        .map(|buf| join_capped_lines(&buf, STDERR_TAIL_TOTAL_BYTES))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_endpoint_specs_reject_the_other_platform_and_collisions() {
        assert!(parse_injected_unix_fd("").unwrap().is_none());
        assert!(parse_injected_unix_fd("   ").unwrap().is_none());
        assert_eq!(parse_injected_unix_fd("fd:15").unwrap(), Some(15));
        assert_eq!(parse_injected_unix_fd("fd:4").unwrap(), Some(4));
        assert_eq!(parse_injected_unix_fd("fd:3").unwrap(), Some(3));
        assert!(parse_injected_unix_fd("fd:2").is_err());
        assert!(parse_injected_unix_fd("fd:0").is_err());
        assert!(parse_injected_unix_fd("handle:10,handle:12").is_err());
        assert!(parse_injected_unix_fd("fd:nope").is_err());

        assert!(parse_injected_handle_pair("").unwrap().is_none());
        assert_eq!(
            parse_injected_handle_pair("handle:10,handle:12").unwrap(),
            Some((10, 12))
        );
        assert_eq!(
            parse_injected_handle_pair("  handle:10 , handle:12 ").unwrap(),
            Some((10, 12))
        );
        assert!(parse_injected_handle_pair("handle:10,handle:10").is_err());
        assert!(parse_injected_handle_pair("fd:4").is_err());
        assert!(parse_injected_handle_pair("handle:10").is_err());
        assert!(parse_injected_handle_pair("handle:10,fd:4").is_err());
    }

    /// A parent descriptor numbered like the guest proxy slot must still be the
    /// extra socket after the proxy is installed on that number.
    #[cfg(unix)]
    #[test]
    fn extra_socket_kept_when_its_number_is_the_proxy_slot() {
        use std::io::{Read, Write};
        use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
        use std::os::unix::net::UnixStream;

        fn stream_pair() -> (UnixStream, RawFd) {
            let (host, guest) = UnixStream::pair().expect("socketpair");
            host.set_nonblocking(true).expect("host nonblocking");
            (host, guest.into_raw_fd())
        }

        let proxy_slot = std::fs::File::open("/dev/null")
            .expect("reserve proxy slot")
            .into_raw_fd();
        let extra_slot = std::fs::File::open("/dev/null")
            .expect("reserve extra slot")
            .into_raw_fd();
        let (mut proxy_host, mut proxy_guest) = stream_pair();
        let (mut extra_host, extra_guest) = stream_pair();
        let extra_src = if extra_guest == proxy_slot {
            extra_guest
        } else {
            let rc = unsafe { libc::dup2(extra_guest, proxy_slot) };
            assert!(
                rc >= 0,
                "dup2 extra onto proxy slot: {}",
                std::io::Error::last_os_error()
            );
            unsafe { libc::close(extra_guest) };
            proxy_slot
        };
        if proxy_guest == proxy_slot || proxy_guest == extra_slot {
            let away = dup_cloexec_at_least(proxy_guest, proxy_slot.max(extra_slot) + 1)
                .expect("move proxy off a slot");
            unsafe { libc::close(proxy_guest) };
            proxy_guest = away;
        }

        inherit_mapped_fds(proxy_guest, proxy_slot, Some(extra_src), extra_slot)
            .expect("install proxy and extra");

        proxy_host.write_all(b"P").expect("write proxy");
        extra_host.write_all(b"E").expect("write extra");
        let mut proxy_end = unsafe { UnixStream::from_raw_fd(proxy_slot) };
        let mut extra_end = unsafe { UnixStream::from_raw_fd(extra_slot) };
        proxy_end
            .set_nonblocking(true)
            .expect("proxy slot nonblocking");
        extra_end
            .set_nonblocking(true)
            .expect("extra slot nonblocking");
        let mut buf = [0u8; 1];
        proxy_end.read_exact(&mut buf).expect("read proxy slot");
        assert_eq!(buf, [b'P']);
        extra_end.read_exact(&mut buf).expect("read extra slot");
        assert_eq!(buf, [b'E']);
    }

    #[test]
    fn observe_metadata_is_not_transfer_permission() {
        assert_eq!(parse_observe_request(None).unwrap(), ObserveKind::Off);
        assert_eq!(parse_observe_request(Some("")).unwrap(), ObserveKind::Off);
        assert_eq!(parse_observe_request(Some("1")).unwrap(), ObserveKind::Slot);
        assert_eq!(
            parse_observe_request(Some(" true ")).unwrap(),
            ObserveKind::Slot
        );
        assert!(parse_observe_request(Some("fd:4")).is_err());
        #[cfg(not(windows))]
        assert!(parse_observe_request(Some("handle:10,handle:12")).is_err());
        #[cfg(windows)]
        assert_eq!(
            parse_observe_request(Some("handle:10,handle:12")).unwrap(),
            ObserveKind::Handles(10, 12)
        );
    }

    #[test]
    fn channel_tag_file_is_written_only_when_requested() {
        assert!(!channel_ident_requested(None));
        assert!(!channel_ident_requested(Some("")));
        assert!(!channel_ident_requested(Some("0")));
        assert!(channel_ident_requested(Some("1")));
        assert!(channel_ident_requested(Some("true")));
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(write_channel_tag(dir.path(), false).unwrap().is_none());
        assert!(!dir.path().join(TEST_CHANNEL_TAG_FILE).exists());
        let tag = write_channel_tag(dir.path(), true).unwrap().expect("tag");
        assert!(tag.starts_with("ch-"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(TEST_CHANNEL_TAG_FILE)).unwrap(),
            tag
        );
    }

    #[test]
    fn spawn_diagnostics_redact_session_challenges() {
        let challenge = "ab".repeat(32);
        assert_eq!(challenge.len(), 64);
        let line = bookclerk_sandbox::redact_diagnostic_text(&format!(
            "café plugin=probe BOOKCLERK_SESSION_CHALLENGE={challenge} tail"
        ));
        assert!(!line.contains(&challenge));
        assert!(line.contains("[redacted]"));
        assert!(line.contains("[redacted-env]"));
        assert!(line.contains("tail"));
        assert!(line.contains("café"));
        assert!(!line.contains('Ã'));
        assert!(!line.contains('©'));
    }

    #[test]
    fn spawn_diagnostics_redact_tokens_that_cross_the_cap() {
        let secret = "ab".repeat(32);
        let across = format!("{}{secret}", "x".repeat(449));
        assert_eq!(across.len(), 513);
        assert_stage_redaction(&across, &secret, true);

        let before = format!("before {secret} after");
        assert_stage_redaction(&before, &secret, true);

        let after_secret = "ef".repeat(32);
        let after = format!("{}{after_secret}", "x".repeat(SPAWN_DIAG_RECORD_BYTES));
        assert_stage_redaction(&after, &after_secret, false);

        let env = format!("{}BOOKCLERK_SESSION_CHALLENGE", "n".repeat(500));
        assert_stage_redaction(&env, "BOOKCLERK_SESSION_CHALLENGE", true);

        let multi_secret = "12".repeat(32);
        let multibyte = format!("{}{multi_secret}", "café".repeat(100));
        assert_stage_redaction(&multibyte, &multi_secret, true);

        let body63 = format!("{}c", "ab".repeat(31));
        let hex63 = format!("{}{body63}", "q".repeat(449));
        let event = capture_stage_event(&hex63);
        assert!(event.contains(&body63), "{event}");
        assert!(!event.contains("[redacted]"), "{event}");
    }

    fn assert_stage_redaction(message: &str, secret: &str, expect_marker: bool) {
        let direct = redact_capped(message, SPAWN_DIAG_RECORD_BYTES);
        assert!(direct.len() <= SPAWN_DIAG_RECORD_BYTES);
        assert!(!direct.contains(&secret[..secret.len().min(16)]));
        let event = capture_stage_event(message);
        assert!(
            event.contains(&direct),
            "stage event dropped the capped redaction\nevent: {event}\ndirect: {direct}"
        );
        let prefix = &secret[..secret.len().min(16)];
        assert!(
            !event.contains(prefix),
            "stage event kept a challenge prefix ({} bytes)",
            event.len()
        );
        let snap = recent_spawn_diagnostics();
        assert!(snap.len() <= SPAWN_DIAG_TOTAL_BYTES);
        assert!(
            !snap.contains(prefix),
            "stage snapshot kept a challenge prefix ({} bytes)",
            snap.len()
        );
        assert!(!event.contains('Ã'), "{event}");
        assert!(!snap.contains('Ã'));
        if expect_marker {
            assert!(
                event.contains("[redacted"),
                "stage event dropped the marker: {event}"
            );
        } else {
            assert!(!event.contains("[redacted"), "{event}");
        }
    }

    fn capture_stage_event(message: &str) -> String {
        for _ in 0..40 {
            let text = capture_stage_event_once(message);
            if !text.is_empty() {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        String::new()
    }

    fn capture_stage_event_once(message: &str) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log buf").extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new("bookclerk=info"))
            .with_writer(move || Buf(Arc::clone(&sink)))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            // Other tests emit this callsite with no subscriber, which caches
            // the event as disabled for the process. Rebuild while this
            // subscriber is installed so the line is actually recorded.
            tracing::callsite::rebuild_interest_cache();
            note_spawn_stage(message);
        });
        let text = String::from_utf8(bytes.lock().expect("log buf").clone()).expect("utf-8 log");
        text
    }

    #[test]
    fn spawn_diagnostics_snapshot_is_byte_bounded() {
        for _ in 0..200 {
            note_spawn_stage(&format!("café-{}", "x".repeat(4_000)));
        }
        let snap = recent_spawn_diagnostics();
        assert!(snap.len() <= SPAWN_DIAG_TOTAL_BYTES);
        assert!(snap.contains("café"));
        assert!(!snap.contains(&"x".repeat(1_000)));
        assert!(!snap.contains('Ã'));
    }

    #[test]
    fn json_logging_is_structured_and_filtered() {
        let info = capture_spawn_logs("bookclerk=info", "json-café-info");
        let warn = capture_spawn_logs("bookclerk=warn", "json-café-warn");
        assert!(
            info.lines()
                .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()),
            "stderr sink was not JSON: {info}"
        );
        assert!(info.contains("json-café-info"), "{info}");
        assert!(info.contains("café"));
        assert!(!info.contains('Ã'));
        assert!(
            !warn.contains("json-café-warn"),
            "warn filter kept an info spawn line: {warn}"
        );
    }

    fn capture_spawn_logs(filter: &str, token: &str) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log buf").extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(move || Buf(Arc::clone(&sink)))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            note_spawn_stage(token);
            emit_guest_stderr_record("probe", "guest", token);
        });
        let text = String::from_utf8(bytes.lock().expect("log buf").clone()).expect("utf-8 log");
        text
    }

    #[tokio::test]
    async fn guest_stderr_is_byte_capped_and_leaves_the_stage_ring() {
        use tokio::io::AsyncWriteExt;

        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let token = "guest-café-token";
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let tail_task = Arc::clone(&tail);
        let drained = tokio::spawn(async move {
            drain_guest_stderr("probe", "guest", client, tail_task.as_ref()).await;
        });
        let chunk = vec![b'B'; 8192];
        server.write_all(token.as_bytes()).await.expect("token");
        let mut written = 0usize;
        while written < 2_000_000 {
            server.write_all(&chunk).await.expect("pad");
            written += chunk.len();
        }
        server
            .write_all(b"\nafter-newline\n")
            .await
            .expect("tail line");
        drop(server);
        drained.await.expect("drain");
        let text = stderr_tail_text(&tail);
        assert!(text.len() <= STDERR_TAIL_TOTAL_BYTES, "{}", text.len());
        assert!(text.contains(token), "{text}");
        assert!(text.contains("café"));
        assert!(text.contains("after-newline"), "{text}");
        assert!(!text.contains(&"B".repeat(1_000)));
        assert!(!text.contains('Ã'));
        for line in text.lines() {
            assert!(line.len() <= STDERR_TAIL_LINE_BYTES, "{}", line.len());
        }
        drop(tail);
        let stages = recent_spawn_diagnostics();
        assert!(!stages.contains(token));
        assert!(!stages.contains("after-newline"));
    }

    #[test]
    fn guest_env_contract_excludes_gateway_secrets() {
        let extra = [("BOOKCLERK_SQLITE_PATH", OsString::from("/tmp/library.db"))];
        let keys = curated_guest_env_keys(&extra);
        assert!(keys.contains("BOOKCLERK_PLUGIN_ID"));
        assert!(keys.contains("BOOKCLERK_PLUGIN_ROOT"));
        assert!(keys.contains("BOOKCLERK_SOCKET_PROXY"));
        assert!(keys.contains("BOOKCLERK_SQLITE_PATH"));
        assert!(keys.contains("HOME"));
        assert!(keys.contains("TMPDIR"));
        for key in &keys {
            assert!(
                !guest_env_forbidden_key(key),
                "guest env must not contain {key}"
            );
        }
        assert!(!keys.contains(GATEWAY_GUEST_RPC_ENV));
        assert!(!keys.contains(GATEWAY_GUEST_RPC_WRITE_ENV));
        assert!(!keys.contains(GATEWAY_PROXY_ENV));
        assert!(!keys.contains(WORKERD_STATE_DIR_ENV));
        assert!(!keys.contains(bookclerk_sandbox::SPEC_ENV));
        assert!(!keys
            .iter()
            .any(|k| k.starts_with("BOOKCLERK_WORKERD_GRANT_")));
        assert!(!keys.iter().any(|k| k.starts_with("BOOKCLERK_JAIL_")));
    }

    #[test]
    fn outer_job_fails_closed_unless_the_gap_is_explicitly_unsupported() {
        use bookclerk_config::Isolation;
        assert_eq!(
            decide_outer_job(Isolation::Required, None),
            OuterJobDecision::Present
        );
        assert_eq!(
            decide_outer_job(Isolation::Required, Some(OuterJobFailure::Unsupported)),
            OuterJobDecision::FailClosed
        );
        assert_eq!(
            decide_outer_job(Isolation::Required, Some(OuterJobFailure::Failed)),
            OuterJobDecision::FailClosed
        );
        assert_eq!(
            decide_outer_job(Isolation::BestEffort, Some(OuterJobFailure::Failed)),
            OuterJobDecision::FailClosed
        );
        assert_eq!(
            decide_outer_job(Isolation::BestEffort, Some(OuterJobFailure::Unsupported)),
            OuterJobDecision::AbsentUnsupported
        );
        assert_eq!(
            decide_outer_job(Isolation::Off, Some(OuterJobFailure::Unsupported)),
            OuterJobDecision::AbsentUnsupported
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_starttime_is_the_token_after_the_comm_field() {
        // Field 22 is the 20th whitespace token after the last ')'.
        let stat = "9 (comm with) parens) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 99";
        assert_eq!(linux_start_ticks(stat), Some(4242));
    }

    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            true
        } else {
            std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
    }

    #[cfg(unix)]
    #[test]
    fn mismatched_start_time_does_not_signal_the_group() {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        cmd.process_group(0);
        let mut child = with_fd_spawn_lock(|| cmd.spawn()).expect("sleep");
        let pid = child.id();
        let identity = ProcessIdentity::capture(Some(pid)).expect("start time");
        assert!(identity.still_same());
        identity.clone().with_bogus_start().kill_if_same();
        assert!(
            process_alive(pid),
            "a mismatched start time must not signal the live pid"
        );
        identity.kill_if_same();
        let _ = child.wait();
        assert!(
            !process_alive(pid),
            "matching identity must reap the leader"
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_group_kill_reaps_a_descendant_of_the_leader() {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & echo $!; wait");
        cmd.process_group(0);
        cmd.stdout(std::process::Stdio::piped());
        let mut child = with_fd_spawn_lock(|| cmd.spawn()).expect("sh");
        let leader = ProcessIdentity::capture(Some(child.id())).expect("leader");
        let stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut line)
            .expect("descendant pid");
        let descendant: u32 = line.trim().parse().expect("pid");
        assert!(process_alive(descendant));
        leader.kill_if_same();
        let _ = child.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && process_alive(descendant) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !process_alive(descendant),
            "a descendant that stays in the leader's process group must exit"
        );
        assert!(!process_alive(leader.pid));
    }

    /// Leader exit must be visible without a reap, so a later group kill still
    /// sees the start time and the same-group descendant dies.
    #[cfg(unix)]
    #[test]
    fn observing_leader_exit_still_kills_the_same_group_descendant() {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("trap '' HUP; sleep 60 & echo $!; exit");
        cmd.process_group(0);
        cmd.stdout(std::process::Stdio::piped());
        let mut child = with_fd_spawn_lock(|| cmd.spawn()).expect("sh");
        let leader = ProcessIdentity::capture(Some(child.id())).expect("leader");
        let stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut line)
            .expect("descendant pid");
        let descendant: u32 = line.trim().parse().expect("pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !exited_without_reaping(leader.pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "leader did not exit while its descendant stayed up"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            exited_without_reaping(leader.pid),
            "a second observation must not reap the leader"
        );
        assert!(
            leader.still_same(),
            "the zombie leader must keep the start time captured at spawn"
        );
        assert!(
            process_alive(descendant),
            "descendant must still be alive when the leader is only observed"
        );
        leader.kill_if_same();
        let _ = child.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && process_alive(descendant) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !process_alive(descendant),
            "observing leader exit must still let the group kill reach the descendant"
        );
        assert!(!process_alive(leader.pid));
    }

    /// `setsid` leaves the leader's process group. A delegated cgroup still
    /// contains that descendant; without one, group kill does not.
    #[cfg(target_os = "linux")]
    #[test]
    fn setsid_descendant_is_contained_only_by_a_delegated_cgroup() {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("setsid sleep 30 & echo $!; wait");
        cmd.process_group(0);
        cmd.stdout(std::process::Stdio::piped());
        let mut child = with_fd_spawn_lock(|| cmd.spawn()).expect("sh");
        let leader = ProcessIdentity::capture(Some(child.id())).expect("leader");
        let stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut line)
            .expect("setsid pid");
        let escaped: u32 = line.trim().parse().expect("pid");
        assert!(process_alive(escaped));
        let detached = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{escaped}/stat")).unwrap_or_default();
            let pgrp = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(2))
                .and_then(|token| token.parse::<u32>().ok());
            if pgrp == Some(escaped) {
                break;
            }
            assert!(
                std::time::Instant::now() < detached,
                "setsid descendant did not leave the leader's process group"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        leader.kill_if_same();
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            process_alive(escaped),
            "process-group kill must not be treated as covering setsid"
        );

        let limits = bookclerk_sandbox::ResourceLimits {
            memory_bytes: None,
            cpu_rate_percent: None,
            active_processes: Some(32),
        };
        let suffix = format!(
            "setsid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        );
        match bookclerk_sandbox::create_session_cgroup(&limits, &suffix) {
            Ok(dir) => {
                if let Err(err) = std::fs::write(dir.join("cgroup.procs"), format!("{escaped}")) {
                    eprintln!(
                        "could not move the setsid descendant into {}: {err}. \
                         process-group kill is the fallback and does not cover setsid",
                        dir.display()
                    );
                    unsafe {
                        libc::kill(escaped as i32, libc::SIGKILL);
                    }
                    let _ = bookclerk_sandbox::destroy_session_cgroup(&dir);
                    return;
                }
                bookclerk_sandbox::destroy_session_cgroup(&dir)
                    .expect("cgroup destroy reaps members and removes the leaf");
                assert!(
                    !process_alive(escaped),
                    "a delegated cgroup must reap the setsid descendant"
                );
                assert!(!dir.exists(), "the leaf must be removed");
            }
            Err(err) => {
                eprintln!(
                    "delegated cgroup unavailable ({err}); process-group kill is the fallback \
                     and does not cover a descendant that calls setsid"
                );
                unsafe {
                    libc::kill(escaped as i32, libc::SIGKILL);
                }
            }
        }
    }

    /// The guest's stdin and stdout are its RPC end. Fd 3 is its proxy end.
    /// The gateway RPC end and the host proxy end stay in the parent.
    /// Socketpair ends have distinct inodes, so the parent reads `/proc/<pid>/fd`
    /// while the child is still alive.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn guest_stdio_does_not_include_the_gateway_rpc_end() {
        fn socket_id(link: &DuplexLink) -> String {
            std::fs::read_link(format!("/proc/self/fd/{}", link.as_raw_fd()))
                .expect("socket link")
                .to_string_lossy()
                .into_owned()
        }
        fn child_sockets(pid: u32) -> String {
            let dir = std::fs::read_dir(format!("/proc/{pid}/fd")).expect("child fds");
            let mut names = Vec::new();
            for entry in dir.flatten() {
                if let Ok(target) = std::fs::read_link(entry.path()) {
                    names.push(target.to_string_lossy().into_owned());
                }
            }
            names.join("\n")
        }
        /// `CLOEXEC` closes descriptors at `exec`, not at `fork`. `/proc/<pid>/fd`
        /// before `exec` still lists the parent's sockets.
        async fn wait_until_exec_sleep(pid: u32) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                if comm.trim() == "sleep" {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "child {pid} did not exec sleep (comm {comm:?})"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        fn count_fd(text: &str, id: &str) -> usize {
            text.lines().filter(|line| *line == id).count()
        }

        let (rpc_gateway, rpc_guest) = DuplexLink::pair().expect("rpc pair");
        let (proxy_host, proxy_guest) = DuplexLink::pair().expect("proxy pair");
        let rpc_guest_id = socket_id(&rpc_guest);
        let rpc_gateway_id = socket_id(&rpc_gateway);
        let proxy_guest_id = socket_id(&proxy_guest);
        let proxy_host_id = socket_id(&proxy_host);

        let mut cmd = Command::new("sleep");
        cmd.arg("30").stderr(Stdio::null());
        inherit_unix_guest(&mut cmd, rpc_guest, &proxy_guest).expect("inherit guest");
        let mut child = with_fd_spawn_lock(|| cmd.spawn()).expect("spawn guest shape");
        let pid = child.id().expect("guest pid");
        wait_until_exec_sleep(pid).await;
        let text = child_sockets(pid);
        let _ = child.start_kill();
        let _ = child.wait().await;
        assert_eq!(
            count_fd(&text, &proxy_guest_id),
            1,
            "proxy fd should be the dup2 destination only\n{text}"
        );
        assert_eq!(
            count_fd(&text, &rpc_guest_id),
            2,
            "guest RPC end belongs on stdin and stdout\n{text}"
        );
        assert!(
            count_fd(&text, &rpc_gateway_id) == 0 && count_fd(&text, &proxy_host_id) == 0,
            "gateway ends leaked into the guest\n{text}"
        );
        drop((rpc_gateway, proxy_host));

        let (rpc_gateway, rpc_guest) = DuplexLink::pair().expect("rpc pair");
        let (proxy_host, proxy_guest) = DuplexLink::pair().expect("proxy pair");
        let ids = [
            socket_id(&rpc_gateway),
            socket_id(&rpc_guest),
            socket_id(&proxy_host),
            socket_id(&proxy_guest),
        ];
        let mut unrelated = Command::new("sleep");
        unrelated
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = with_fd_spawn_lock(|| unrelated.spawn()).expect("unrelated");
        let pid = child.id().expect("unrelated pid");
        wait_until_exec_sleep(pid).await;
        let text = child_sockets(pid);
        let _ = child.start_kill();
        let _ = child.wait().await;
        for id in &ids {
            assert!(
                count_fd(&text, id) == 0,
                "unrelated child inherited {id}\n{text}"
            );
        }
        drop((rpc_gateway, rpc_guest, proxy_host, proxy_guest));
    }
}
