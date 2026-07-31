//! JSON-RPC 2.0 over newline-delimited stdio.

#![cfg_attr(unix, allow(unsafe_code))]

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bookclerk_config::Config;
use bookclerk_sandbox::{PLUGIN_FD_CHANNEL, PLUGIN_FD_CHANNEL_ENV};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

use crate::discover::DiscoveredPlugin;
use crate::jail::{GuestJail, Start};
use crate::protocol::{methods, HandshakeResult, PLUGIN_API_VERSION};
use crate::{PluginError, Result};

#[derive(Debug, Serialize)]
struct Request {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<u64>,
    result: Option<Value>,
    error: Option<RpcErrorObject>,
}

#[derive(Debug, Deserialize)]
struct RpcErrorObject {
    message: String,
}

enum SidePass<'a> {
    FetchDir(&'a Path),
    UploadFile(&'a Path),
    DbFile(&'a Path),
}

/// Host-side client that owns a plugin child process.
pub struct PluginClient {
    id: String,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next_id: AtomicU64,
    handshake: HandshakeResult,
    /// Host end of the fetch-directory side channel, when the guest is jailed.
    #[cfg(unix)]
    fd_channel: Option<std::os::unix::net::UnixStream>,
}

impl PluginClient {
    /// Spawn `plugin` inside its jail, then handshake.
    ///
    /// Takes the whole [`DiscoveredPlugin`] and [`Config`] rather than a command
    /// line so there is no way to start a guest without deciding how it is
    /// confined. `config_table` is the plugin's own settings, sent at handshake.
    ///
    /// # Errors
    ///
    /// Fails when the jail cannot be applied and `[plugins].isolation` is
    /// `required`, and when the guest does not answer the handshake.
    pub async fn spawn(
        plugin: &DiscoveredPlugin,
        config: &Config,
        config_table: Value,
    ) -> Result<Self> {
        let id = plugin.manifest.id.as_str();
        let jail = GuestJail::plan(config, plugin)?;

        let mut cmd = match &jail.start {
            Start::Confined { launcher, .. } => {
                tracing::debug!(
                    plugin = %id,
                    launcher = %launcher.display(),
                    "starting plugin guest under a jail"
                );
                let mut cmd = Command::new(launcher);
                // `--` keeps a guest path that looks like an option from being
                // read as one.
                cmd.arg("--")
                    .arg(&plugin.command)
                    .args(&plugin.manifest.args);
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
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            // Do not inherit host secrets (BOOKCLERK_AUTH_PASSWORD, AWS keys,
            // operator token, DB URLs, …). Allowlist only non-sensitive vars.
            .env_clear();
        for (key, value) in std::env::vars_os() {
            if plugin_env_allowed(&key.to_string_lossy()) {
                cmd.env(key, value);
            }
        }
        cmd.env("BOOKCLERK_PLUGIN_ID", id);
        // Redirect the directories a program reaches for without being told.
        // Inherited values name paths outside the jail, so a guest writing a
        // temp file would fail on a permission error unrelated to its own work.
        for key in ["TMPDIR", "TEMP", "TMP"] {
            cmd.env(key, &jail.scratch);
        }
        cmd.env("HOME", &jail.data);
        if let Start::Confined { spec, .. } = &jail.start {
            cmd.env(
                bookclerk_sandbox::SPEC_ENV,
                serde_json::to_string(spec.as_ref()).map_err(|err| {
                    PluginError::message(format!("could not encode the jail spec: {err}"))
                })?,
            );
            #[cfg(unix)]
            if jail.guest_channel_raw.is_some() {
                cmd.env(PLUGIN_FD_CHANNEL_ENV, PLUGIN_FD_CHANNEL.to_string());
            }
        }

        #[cfg(unix)]
        if let Some(guest_raw) = jail.guest_channel_raw {
            unsafe {
                cmd.pre_exec(move || {
                    if guest_raw != PLUGIN_FD_CHANNEL {
                        if libc::dup2(guest_raw, PLUGIN_FD_CHANNEL) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        libc::close(guest_raw);
                    }
                    Ok(())
                });
            }
        }

        let mut child = cmd.spawn()?;

        #[cfg(unix)]
        if let Some(guest_raw) = jail.guest_channel_raw {
            // SAFETY: the child owns its copy; the parent's is unused after spawn.
            unsafe {
                libc::close(guest_raw);
            }
        }

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PluginError::message("plugin stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PluginError::message("plugin stdout missing"))?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Response>(&line) {
                    Ok(resp) => {
                        if let Some(id) = resp.id {
                            let mut map = pending_reader.lock().await;
                            if let Some(tx) = map.remove(&id) {
                                let outcome = if let Some(err) = resp.error {
                                    Err(PluginError::message(err.message))
                                } else {
                                    Ok(resp.result.unwrap_or(Value::Null))
                                };
                                let _ = tx.send(outcome);
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%err, line = %line, "plugin returned invalid JSON-RPC");
                    }
                }
            }
        });

        let client = Self {
            id: id.to_string(),
            child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            handshake: HandshakeResult {
                api_version: 0,
                id: id.to_string(),
                kind: String::new(),
                display_name: None,
                capabilities: vec![],
                portal_auth_mode: None,
                password_env_var: None,
                aliases: vec![],
                sort_key: None,
                brand: None,
                config_options: vec![],
                cli: None,
            },
            #[cfg(unix)]
            fd_channel: jail.fd_channel,
        };

        let hs: HandshakeResult = client
            .call(
                methods::HANDSHAKE,
                serde_json::json!({
                    "api_version": PLUGIN_API_VERSION,
                    "config": config_table,
                }),
            )
            .await?;
        if hs.id != id {
            tracing::warn!(
                manifest_id = %id,
                handshake_id = %hs.id,
                "plugin handshake id differs from manifest id; using manifest id"
            );
        }
        let mut client = client;
        client.handshake = hs;
        Ok(client)
    }

    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn handshake(&self) -> &HandshakeResult {
        &self.handshake
    }

    pub fn has_capability(&self, cap: &str) -> bool {
        self.handshake
            .capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case(cap))
    }

    /// Whether the host can pass fetch/upload descriptors over the side channel.
    #[must_use]
    pub fn has_side_channel(&self) -> bool {
        #[cfg(unix)]
        {
            self.fd_channel.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Call a JSON-RPC method and deserialize the result.
    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let value = self.call_raw(method, params).await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value> {
        self.call_raw_with_side_pass(method, params, None).await
    }

    /// Like [`Self::call_raw`], but passes an open fetch work directory first when
    /// the guest is jailed.
    pub async fn call_raw_with_fetch_dir(
        &self,
        method: &str,
        params: Value,
        fetch_dir: Option<&Path>,
    ) -> Result<Value> {
        self.call_raw_with_side_pass(method, params, fetch_dir.map(SidePass::FetchDir))
            .await
    }

    /// Like [`Self::call_raw`], but passes an open local file before `put_file`.
    pub async fn call_raw_with_upload_file(
        &self,
        method: &str,
        params: Value,
        upload_path: &Path,
    ) -> Result<Value> {
        self.call_raw_with_side_pass(method, params, Some(SidePass::UploadFile(upload_path)))
            .await
    }

    /// Like [`Self::call_raw`], but passes an open database file before `db.connect`.
    pub async fn call_raw_with_db_file(
        &self,
        method: &str,
        params: Value,
        db_path: &Path,
    ) -> Result<Value> {
        self.call_raw_with_side_pass(method, params, Some(SidePass::DbFile(db_path)))
            .await
    }

    async fn call_raw_with_side_pass(
        &self,
        method: &str,
        params: Value,
        side: Option<SidePass<'_>>,
    ) -> Result<Value> {
        if let Some(side) = side {
            #[cfg(unix)]
            if let Some(channel) = self.fd_channel.as_ref() {
                match side {
                    SidePass::FetchDir(dir) if method == methods::FETCH_TITLE => {
                        crate::fd_pass::send_fetch_dir(channel, dir)?;
                    }
                    SidePass::UploadFile(path) if method == methods::PUT_FILE => {
                        crate::fd_pass::send_upload_file(channel, path)?;
                    }
                    SidePass::DbFile(path) if method == methods::DB_CONNECT => {
                        crate::fd_pass::send_database_file(channel, path)?;
                    }
                    SidePass::FetchDir(_) | SidePass::UploadFile(_) | SidePass::DbFile(_) => {}
                }
            }
            let _ = side;
        }
        self.call_raw_inner(method, params).await
    }

    async fn call_raw_inner(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, tx);
        }
        let req = Request {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }
        match rx.await {
            Ok(outcome) => outcome,
            Err(_) => Err(PluginError::message(format!(
                "plugin `{}` closed while waiting for `{method}`",
                self.id
            ))),
        }
    }

    /// Notify-style call that ignores a missing method (optional capability).
    pub async fn call_optional<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Option<T>> {
        match self.call::<T>(method, params).await {
            Ok(v) => Ok(Some(v)),
            Err(PluginError::Message(msg))
                if msg.contains("method not found") || msg.contains("unsupported") =>
            {
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Resolve CLI schema: `cli.describe` when capable, else handshake `cli`.
    pub async fn cli_describe(&self) -> Result<crate::protocol::CliSchema> {
        use crate::protocol::CliSchema;
        if self.has_capability("cli") {
            if let Some(schema) = self
                .call_optional::<CliSchema>(
                    methods::CLI_DESCRIBE,
                    Value::Object(Default::default()),
                )
                .await?
            {
                return Ok(schema);
            }
        }
        Ok(self.handshake.cli.clone().unwrap_or_default())
    }

    /// Invoke a declared plugin CLI command.
    pub async fn cli_invoke(
        &self,
        params: crate::protocol::CliInvokeParams,
    ) -> Result<crate::protocol::CliInvokeResult> {
        if !self.has_capability("cli") {
            return Err(PluginError::message(format!(
                "plugin `{}` does not advertise the `cli` capability",
                self.id
            )));
        }
        self.call(methods::CLI_INVOKE, serde_json::to_value(params)?)
            .await
    }
}

impl Drop for PluginClient {
    fn drop(&mut self) {
        // kill_on_drop is set; best-effort kill if still running.
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

/// Env keys safe to inherit into a plugin child.
///
/// Explicitly excludes Bookclerk/AWS/Cloudflare secrets and DB URLs.
///
/// `HOME` and the temp-directory variables are listed because a guest needs
/// *some* value for them, but the inherited one names a path outside the jail.
/// [`PluginClient::spawn`] overwrites all four with the guest's own directories
/// after this filter runs. `XDG_RUNTIME_DIR` is absent for the same reason and
/// has no per-guest equivalent to point at.
fn plugin_env_allowed(key: &str) -> bool {
    const ALLOW: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "TMPDIR",
        "TEMP",
        "TMP",
        "TERM",
        "COLORTERM",
        "RUST_BACKTRACE",
        "NO_COLOR",
        "FORCE_COLOR",
    ];
    if ALLOW.iter().any(|k| key.eq_ignore_ascii_case(k)) {
        return true;
    }
    // Block anything that looks like a secret or Bookclerk bootstrap var.
    let upper = key.to_ascii_uppercase();
    if upper.starts_with("BOOKCLERK_")
        || upper.starts_with("AWS_")
        || upper.starts_with("CLOUDFLARE_")
        || upper.contains("PASSWORD")
        || upper.contains("SECRET")
        || upper.contains("TOKEN")
        || upper.contains("API_KEY")
        || upper.contains("DATABASE_URL")
    {
        return false;
    }
    false
}

#[cfg(test)]
mod env_tests {
    use super::plugin_env_allowed;

    #[test]
    fn allows_path_blocks_secrets() {
        assert!(plugin_env_allowed("PATH"));
        assert!(plugin_env_allowed("HOME"));
        assert!(!plugin_env_allowed("BOOKCLERK_AUTH_PASSWORD"));
        assert!(!plugin_env_allowed("BOOKCLERK_OPERATOR_TOKEN"));
        assert!(!plugin_env_allowed("AWS_SECRET_ACCESS_KEY"));
        assert!(!plugin_env_allowed("CLOUDFLARE_API_TOKEN"));
        assert!(!plugin_env_allowed("BOOKCLERK_DATABASE_POSTGRES_URL"));
    }
}
