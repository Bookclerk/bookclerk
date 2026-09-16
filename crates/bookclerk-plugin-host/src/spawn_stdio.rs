//! Shared jailed-child spawn for Cap'n Proto `api_version = 2` stdio guests.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::PathBuf;
use std::process::Stdio;

use bookclerk_config::Config;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::consent::{inject_workerd_grant_env, spawn_config_for_grant, spawn_grant, PluginGrant};
use crate::discover::DiscoveredPlugin;
use crate::jail::{GuestJail, Start};
use crate::manifest::PluginRuntimeKind;
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
}

/// Spawns the jailed guest with piped stdio. Caller performs Cap'n Proto connect.
///
/// # Errors
///
/// Fails when no covering grant exists, the jail cannot be applied, or the
/// process cannot be started.
pub(crate) async fn spawn_stdio_guest(
    plugin: &DiscoveredPlugin,
    config: &Config,
    config_table: Value,
    extra_env: &[(&str, std::ffi::OsString)],
) -> Result<SpawnedStdio> {
    let id = plugin.manifest.id.clone();
    let grant = spawn_grant(&config.paths().files_dir, &plugin.manifest)?;
    let spawn_config = spawn_config_for_grant(&grant, config_table);
    let jail = GuestJail::plan(config, plugin)?;

    let mut cmd = match &jail.start {
        Start::Confined { launcher, .. } => {
            tracing::debug!(
                plugin = %id,
                launcher = %launcher.display(),
                "starting plugin guest under a jail"
            );
            let mut cmd = Command::new(launcher);
            cmd.arg("--")
                .arg(&plugin.command)
                .args(&plugin.manifest.args);
            cmd.env("BOOKCLERK_PLUGIN_ROOT", &plugin.root);
            cmd.env("BOOKCLERK_PLUGIN_TOML", plugin.root.join("plugin.toml"));
            cmd
        }
        Start::Unconfined { reason } => {
            tracing::warn!(
                plugin = %id,
                %reason,
                "starting plugin guest WITHOUT a jail; it can reach everything \
                 this user can"
            );
            let mut cmd = Command::new(&plugin.command);
            cmd.args(&plugin.manifest.args);
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
    if plugin.manifest.runtime == PluginRuntimeKind::Workerd {
        inject_workerd_grant_env(&mut cmd, &grant);
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

    if let Some(stderr) = child.stderr.take() {
        forward_guest_stderr(id.clone(), stderr);
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
    })
}

/// Re-emits each guest stderr line through tracing so `bookclerkd` JSON logs
/// stay structured (jail summaries used to land as raw `eprintln!` on the
/// inherited daemon stderr). ANSI from guest formatters is stripped so JSON
/// does not encode CSI as `\u001b`.
fn forward_guest_stderr(plugin: String, stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = bookclerk_config::strip_ansi_escapes(&line);
            if line.is_empty() {
                continue;
            }
            tracing::info!(plugin = %plugin, "{line}");
        }
    });
}
