//! Workers RPC over newline-delimited stdio (identical native / workerd framing).

#![cfg_attr(unix, allow(unsafe_code))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use bookclerk_config::Config;
use bookclerk_sandbox::{PLUGIN_FD_CHANNEL, PLUGIN_FD_CHANNEL_ENV};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex, MutexGuard};

use crate::consent::{
    handshake_config_for_grant, require_binding, spawn_grant, validate_handshake_capabilities,
    PluginGrant,
};
use crate::discover::DiscoveredPlugin;
use crate::jail::{GuestJail, Start};
use crate::protocol::{
    methods, HandshakeResult, HOST_API_VERSION_MAX, HOST_API_VERSION_MIN, MAX_RPC_LINE_BYTES,
    PLUGIN_API_VERSION,
};
use crate::{PluginError, Result};

/// Default wait for one RPC round-trip before the host gives up.
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug, Serialize)]
struct Request {
    id: u64,
    method: String,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct Response {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<AbiRpcError>,
}

/// Wire error (matches [`bookclerk_plugin_abi::PluginError`] JSON).
#[derive(Debug, Deserialize)]
struct AbiRpcError {
    #[serde(default)]
    #[allow(dead_code)]
    code: Option<String>,
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
    /// Covering operator grant checked at spawn and privilege delivery.
    grant: PluginGrant,
    /// Serializes side-channel / ACL setup with the matching JSON-RPC write so
    /// concurrent calls cannot reorder FDs or revoke grants early.
    call_gate: Mutex<()>,
    /// Set after a serious timeout or protocol violation; client must be dropped
    /// and the plugin restarted under operator control.
    quarantined: Arc<AtomicBool>,
    /// Plugin scratch directory (`…/plugins/<id>/tmp`) for callback IPC sockets.
    scratch: PathBuf,
    /// Host end of the fetch-directory side channel, when the guest is jailed.
    #[cfg(unix)]
    fd_channel: Option<std::os::unix::net::UnixStream>,
    /// AppContainer Package SID for per-op ACL grants (Windows confined guests).
    #[cfg(windows)]
    package_sid: Option<String>,
    /// Host-owned AppContainer profile; must outlive the jailed child.
    ///
    /// Declared after `child` so the child (and its Job Object tree) is killed
    /// before DeleteAppContainerProfile runs.
    #[cfg(windows)]
    _appcontainer: Option<bookclerk_sandbox::spawn::AppContainerSession>,
}

impl PluginClient {
    /// Scratch directory for this guest (`TMPDIR` inside the jail).
    #[must_use]
    pub fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    /// AppContainer package SID when the guest is confined on Windows.
    #[must_use]
    pub fn package_sid(&self) -> Option<&str> {
        #[cfg(windows)]
        {
            self.package_sid.as_deref()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Spawn `plugin` inside its jail, then handshake.
    ///
    /// Takes the whole [`DiscoveredPlugin`] and [`Config`] rather than a command
    /// line so there is no way to start a guest without deciding how it is
    /// confined. `config_table` is the plugin's own settings, sent at handshake
    /// only when the covering grant includes the `config` binding.
    ///
    /// # Errors
    ///
    /// Fails when no covering consent grant exists, when the jail cannot be
    /// applied and `[plugins].isolation` is `required`, when the guest does not
    /// answer the handshake, or when handshake claims exceed the manifest/grant.
    pub async fn spawn(
        plugin: &DiscoveredPlugin,
        config: &Config,
        config_table: Value,
    ) -> Result<Self> {
        let id = plugin.manifest.id.as_str();
        let grant = spawn_grant(&config.paths().files_dir, &plugin.manifest)?;
        let handshake_config = handshake_config_for_grant(&grant, config_table);
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

        let child = Arc::new(Mutex::new(child));
        let quarantined = Arc::new(AtomicBool::new(false));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();
        let quarantine_flag = Arc::clone(&quarantined);
        let child_reader = Arc::clone(&child);
        let reader_plugin_id = id.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match read_rpc_line(&mut reader, &mut buf, MAX_RPC_LINE_BYTES).await {
                    Ok(None) => break,
                    Ok(Some(())) => {}
                    Err(err) => {
                        tracing::warn!(
                            plugin = %reader_plugin_id,
                            %err,
                            "plugin response stream closed or exceeded MAX_RPC_LINE_BYTES"
                        );
                        // Oversized / corrupt framing is a protocol violation —
                        // fail pending waiters, quarantine, and kill the guest.
                        quarantine_flag.store(true, Ordering::SeqCst);
                        {
                            let mut map = pending_reader.lock().await;
                            for (_, tx) in map.drain() {
                                let _ = tx.send(Err(PluginError::message(format!(
                                    "plugin `{reader_plugin_id}` protocol violation: {err}"
                                ))));
                            }
                        }
                        let mut child = child_reader.lock().await;
                        let _ = child.start_kill();
                        break;
                    }
                }
                let line = match std::str::from_utf8(&buf) {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!(%err, "plugin returned non-UTF8 JSON-RPC line");
                        continue;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Response>(line) {
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
            child,
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
            grant: grant.clone(),
            call_gate: Mutex::new(()),
            quarantined,
            scratch: jail.scratch.clone(),
            #[cfg(unix)]
            fd_channel: jail.fd_channel,
            #[cfg(windows)]
            package_sid: jail.package_sid,
            #[cfg(windows)]
            _appcontainer: jail.appcontainer,
        };

        let hs: HandshakeResult = client
            .call(
                methods::HANDSHAKE,
                serde_json::json!({
                    "apiVersion": PLUGIN_API_VERSION,
                    "config": handshake_config,
                }),
            )
            .await?;
        if hs.api_version < HOST_API_VERSION_MIN || hs.api_version > HOST_API_VERSION_MAX {
            return Err(PluginError::message(format!(
                "plugin `{id}` handshake api_version {} is outside supported range \
                 {HOST_API_VERSION_MIN}..={HOST_API_VERSION_MAX}",
                hs.api_version
            )));
        }
        if hs.id != id {
            tracing::warn!(
                manifest_id = %id,
                handshake_id = %hs.id,
                "plugin handshake id differs from manifest id; using manifest id"
            );
        }
        validate_handshake_capabilities(
            &plugin.manifest,
            &grant,
            &hs.capabilities,
            hs.portal_auth_mode.as_deref(),
        )?;
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

    /// Covering consent grant from spawn (bindings gate privileged delivery).
    #[must_use]
    pub fn grant(&self) -> &PluginGrant {
        &self.grant
    }

    /// Fail closed when a delivery site needs a binding this guest was not granted.
    pub fn require_binding(&self, name: &str) -> Result<()> {
        require_binding(&self.grant, name)
    }

    pub fn has_capability(&self, cap: &str) -> bool {
        self.handshake
            .capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case(cap))
    }

    /// Whether the host can pass fetch/upload descriptors over the side channel.
    ///
    /// Unix: `SCM_RIGHTS` on fd 3. Windows uses path-in-params plus temporary
    /// AppContainer ACL grants instead ([`Self::has_acl_grants`]).
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

    /// Whether the host can temporarily ACL paths for a confined Windows guest.
    #[must_use]
    pub fn has_acl_grants(&self) -> bool {
        #[cfg(windows)]
        {
            self.package_sid.is_some()
        }
        #[cfg(not(windows))]
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
        // Hold the call gate across side-channel/ACL setup, the request write,
        // and the response wait so concurrent Unix FD passes stay ordered with
        // their requests and Windows ACL grants are not revoked early.
        let _gate: MutexGuard<'_, ()> = self.call_gate.lock().await;

        // Hold ACL grants until the RPC returns (Windows AppContainer).
        #[cfg(windows)]
        let mut _acl_guards: Vec<crate::windows_acl::AclGuard> = Vec::new();

        if let Some(side) = side {
            // FD / ACL side passes are work_fs privileges — require the binding.
            require_binding(&self.grant, "work_fs")?;
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
            #[cfg(windows)]
            if let Some(sid) = self.package_sid.as_deref() {
                match side {
                    SidePass::FetchDir(dir) if method == methods::FETCH_TITLE => {
                        let _ = std::fs::create_dir_all(dir);
                        _acl_guards.push(crate::windows_acl::grant_path_for_guest(sid, dir, true)?);
                    }
                    SidePass::UploadFile(path) if method == methods::PUT_FILE => {
                        _acl_guards
                            .push(crate::windows_acl::grant_path_for_guest(sid, path, false)?);
                    }
                    SidePass::DbFile(path) if method == methods::DB_CONNECT => {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                            _acl_guards
                                .push(crate::windows_acl::grant_path_for_guest(sid, parent, true)?);
                        }
                        if path.exists() {
                            _acl_guards
                                .push(crate::windows_acl::grant_path_for_guest(sid, path, true)?);
                        }
                    }
                    SidePass::FetchDir(_) | SidePass::UploadFile(_) | SidePass::DbFile(_) => {}
                }
            }
            #[cfg(not(any(unix, windows)))]
            let _ = side;
        }
        self.call_raw_inner(method, params).await
    }

    async fn call_raw_inner(&self, method: &str, params: Value) -> Result<Value> {
        if self.quarantined.load(Ordering::SeqCst) {
            return Err(PluginError::message(format!(
                "plugin `{}` is quarantined after a prior timeout or protocol violation; \
                 restart the plugin before retrying `{method}`",
                self.id
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, tx);
        }
        let req = Request {
            id,
            method: method.to_string(),
            params,
        };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        if line.len() > MAX_RPC_LINE_BYTES {
            let mut map = self.pending.lock().await;
            map.remove(&id);
            return Err(PluginError::message(format!(
                "plugin `{}` request for `{method}` exceeds MAX_RPC_LINE_BYTES ({MAX_RPC_LINE_BYTES})",
                self.id
            )));
        }
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(PluginError::message(format!(
                "plugin `{}` closed while waiting for `{method}`",
                self.id
            ))),
            Err(_) => {
                let mut map = self.pending.lock().await;
                map.remove(&id);
                self.quarantine_and_kill(&format!(
                    "timed out after {}s waiting for `{method}`",
                    RPC_TIMEOUT.as_secs()
                ))
                .await;
                Err(PluginError::message(format!(
                    "plugin `{}` timed out after {}s waiting for `{method}` \
                     (guest killed and quarantined)",
                    self.id,
                    RPC_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Kill the guest and refuse further RPCs until this client is dropped.
    async fn quarantine_and_kill(&self, reason: &str) {
        self.quarantined.store(true, Ordering::SeqCst);
        tracing::error!(
            plugin = %self.id,
            %reason,
            "quarantining plugin guest after serious failure"
        );
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
    }

    /// Whether this client was killed after a timeout or protocol violation.
    #[must_use]
    pub fn is_quarantined(&self) -> bool {
        self.quarantined.load(Ordering::SeqCst)
    }

    /// Ask the guest to shut down gracefully (optional method).
    ///
    /// Missing / unsupported methods are ignored; [`Drop`] still kills the child.
    pub async fn shutdown(&self) {
        let _ = self
            .call_optional::<Value>(methods::SHUTDOWN, Value::Null)
            .await;
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

    /// Run `diagnose`, accepting either `{ "lines": [...] }` or a bare string array.
    pub async fn diagnose(&self) -> Result<Vec<String>> {
        let value: Value = self
            .call(methods::DIAGNOSE, Value::Object(Default::default()))
            .await?;
        Ok(parse_diagnose_lines(value))
    }
}

fn parse_diagnose_lines(value: Value) -> Vec<String> {
    if let Some(arr) = value.as_array() {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    if let Some(lines) = value.get("lines").and_then(|v| v.as_array()) {
        return lines
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    vec![value.to_string()]
}

impl Drop for PluginClient {
    fn drop(&mut self) {
        // kill_on_drop is set; best-effort kill if still running.
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

/// Read one newline-delimited RPC line, refusing payloads over `max` bytes.
async fn read_rpc_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<()>> {
    loop {
        let (done, used) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(()))
                };
            }
            if let Some(i) = memchr_newline(available) {
                let take = i; // exclude newline
                if buf.len().saturating_add(take) > max {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("RPC line exceeds {max} bytes"),
                    ));
                }
                buf.extend_from_slice(&available[..take]);
                (true, i + 1)
            } else {
                if buf.len().saturating_add(available.len()) > max {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("RPC line exceeds {max} bytes"),
                    ));
                }
                buf.extend_from_slice(available);
                (false, available.len())
            }
        };
        reader.consume(used);
        if done {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(()));
        }
    }
}

fn memchr_newline(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&b| b == b'\n')
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
        "PATHEXT",
        "HOME",
        "USER",
        "USERNAME",
        "USERPROFILE",
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
        // Windows AppContainer launch / DLL load.
        "SystemRoot",
        "SystemDrive",
        "windir",
        "LOCALAPPDATA",
        "ComSpec",
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
