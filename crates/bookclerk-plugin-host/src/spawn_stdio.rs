//! Shared jailed-child spawn for Cap'n Proto `api_version = 3` stdio guests.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use bookclerk_config::Config;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::consent::{inject_workerd_grant_env, spawn_config_for_grant, spawn_grant, PluginGrant};
use crate::discover::DiscoveredPlugin;
use crate::jail::{GuestJail, Start};
use crate::spawn_plan::{
    SpawnPlan, NATIVE_BACKEND_ENV, NESTED_JAIL_BIN_ENV, NESTED_NATIVE_JAIL_ENV, WORKERD_BIN_ENV,
};
use crate::{PluginError, Result};

/// Jailed plugin child with stdio pipes (describe not yet called).
pub(crate) struct SpawnedStdio {
    /// Plugin id from the manifest.
    pub id: String,
    /// Child process; killed on drop of the session that owns it.
    pub child: Child,
    /// Guest stdin (host writes RPC / capnp).
    pub stdin: ChildStdin,
    /// Guest stdout (host reads RPC / capnp).
    pub stdout: ChildStdout,
    /// Covering operator grant.
    pub grant: PluginGrant,
    /// Spawn config JSON or destination context extras.
    pub spawn_config: Value,
    /// Guest HOME / data directory.
    pub data: PathBuf,
    /// Guest TMPDIR / scratch directory.
    pub scratch: PathBuf,
    /// AppContainer package SID.
    #[cfg(windows)]
    pub package_sid: Option<String>,
    /// Host-owned AppContainer profile.
    #[cfg(windows)]
    pub appcontainer: Option<bookclerk_sandbox::spawn::AppContainerSession>,
    /// Last lines of guest stderr (workerd + native child), for spawn failures.
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Files dir used to re-read `plugin-grants.json` before returning a session.
    pub files_dir: PathBuf,
}

/// Spawns the jailed guest with piped stdio. Caller performs Cap'n Proto connect.
///
/// `plan` names the program the jail execs (`bookclerk-workerd` on the product
/// path) and, for a native backend, the executable the launcher must front.
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
    extra_env: &[(&str, std::ffi::OsString)],
) -> Result<SpawnedStdio> {
    let id = plugin.manifest.id.clone();
    let grant = spawn_grant(&config.paths().files_dir, plugin)?;
    let spawn_config = spawn_config_for_grant(&grant, config_table);
    let jail = GuestJail::plan(config, plugin, plan)?;

    let mut cmd = match &jail.start {
        Start::Confined { launcher, .. } => {
            tracing::debug!(
                plugin = %id,
                launcher = %launcher.display(),
                program = %plan.launcher.display(),
                runtime = plan.runtime.label(),
                "starting plugin guest under a jail"
            );
            let mut cmd = Command::new(launcher);
            cmd.arg("--").arg(&plan.launcher).args(&plan.args);
            cmd
        }
        Start::Unconfined { reason } => {
            tracing::warn!(
                plugin = %id,
                %reason,
                runtime = plan.runtime.label(),
                "starting plugin guest WITHOUT a jail; it can reach everything \
                 this user can"
            );
            let mut cmd = Command::new(&plan.launcher);
            cmd.args(&plan.args);
            cmd
        }
    };

    cmd.current_dir(&plugin.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in std::env::vars_os() {
        if crate::rpc::plugin_env_allowed(&key.to_string_lossy()) {
            cmd.env(key, value);
        }
    }
    cmd.env("BOOKCLERK_PLUGIN_ID", &id);
    // `bookclerk-workerd` reads the manifest from here (falling back to cwd).
    cmd.env("BOOKCLERK_PLUGIN_ROOT", &plugin.root);
    cmd.env("BOOKCLERK_PLUGIN_TOML", plugin.root.join("plugin.toml"));
    if plan.fronted_by_workerd() {
        inject_workerd_grant_env(&mut cmd, &grant);
        if let Some(backend) = &plan.native_backend {
            cmd.env(NATIVE_BACKEND_ENV, backend);
            // Isolation::Required (and any other confined start) nested-jails
            // the native backend. Isolation::Off keeps ambient AF_INET so the
            // postgres LIKE CI vector is unchanged on this branch.
            if matches!(&jail.start, Start::Confined { .. }) {
                cmd.env(NESTED_NATIVE_JAIL_ENV, "1");
            }
            if let Some(jail_bin) = plan.nested_jail_helper() {
                cmd.env(NESTED_JAIL_BIN_ENV, jail_bin);
            }
        }
        // The launcher resolves `workerd` beside itself unless told otherwise;
        // the jail already grants whichever the plan resolved.
        if let Some(workerd_bin) = &plan.workerd_bin {
            cmd.env(WORKERD_BIN_ENV, workerd_bin);
        }
    }
    for key in ["TMPDIR", "TEMP", "TMP"] {
        cmd.env(key, &jail.scratch);
    }
    cmd.env("HOME", &jail.data);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    if let Start::Confined { spec, .. } = &jail.start {
        cmd.env(
            bookclerk_sandbox::SPEC_ENV,
            serde_json::to_string(spec.as_ref()).map_err(|err| {
                PluginError::message(format!("could not encode the jail spec: {err}"))
            })?,
        );
    }

    let mut child = cmd.spawn()?;

    let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(stderr) = child.stderr.take() {
        forward_guest_stderr(id.clone(), stderr, Arc::clone(&stderr_tail));
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| PluginError::message("plugin stdin missing"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PluginError::message("plugin stdout missing"))?;

    Ok(SpawnedStdio {
        id,
        child,
        stdin,
        stdout,
        grant,
        spawn_config,
        data: jail.data,
        scratch: jail.scratch,
        #[cfg(windows)]
        package_sid: jail.package_sid,
        #[cfg(windows)]
        appcontainer: jail.appcontainer,
        stderr_tail,
        files_dir: config.paths().files_dir.clone(),
    })
}

/// Lines of guest stderr retained for spawn/describe failure messages.
const STDERR_TAIL_LINES: usize = 40;

/// Re-emits each guest stderr line through tracing so `bookclerkd` JSON logs
/// stay structured (jail summaries used to land as raw `eprintln!` on the
/// inherited daemon stderr). ANSI from guest formatters is stripped so JSON
/// does not encode CSI as `\u001b`. Also keeps a short ring for panic text:
/// tests often have no tracing subscriber, so workerd/guest logs were invisible.
fn forward_guest_stderr(plugin: String, stderr: ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = bookclerk_config::strip_ansi_escapes(&line);
            if line.is_empty() {
                continue;
            }
            if let Ok(mut buf) = tail.lock() {
                if buf.len() >= STDERR_TAIL_LINES {
                    buf.pop_front();
                }
                buf.push_back(line.to_string());
            }
            tracing::info!(plugin = %plugin, "{line}");
        }
    });
}

/// Guest process status plus captured stderr, for describe/spawn failures.
pub(crate) fn spawn_failure_detail(
    child: &mut Child,
    stderr_tail: &Arc<Mutex<VecDeque<String>>>,
) -> String {
    let status = match child.try_wait() {
        Ok(Some(st)) => format!("guest exited: {st}"),
        Ok(None) => "guest still running".into(),
        Err(e) => format!("guest wait error: {e}"),
    };
    let stderr = stderr_tail_text(stderr_tail);
    if stderr.is_empty() {
        status
    } else {
        format!("{status}\n--- guest stderr ---\n{stderr}")
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

fn stderr_tail_text(tail: &Arc<Mutex<VecDeque<String>>>) -> String {
    tail.lock()
        .map(|buf| buf.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}
