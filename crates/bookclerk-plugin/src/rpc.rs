//! JSON-RPC 2.0 over newline-delimited stdio.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

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

/// Host-side client that owns a plugin child process.
pub struct PluginClient {
    id: String,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next_id: AtomicU64,
    handshake: HandshakeResult,
}

impl PluginClient {
    /// Spawn `command` with `args`, working directory `cwd`, then handshake.
    pub async fn spawn(
        id: &str,
        command: &Path,
        args: &[String],
        cwd: &Path,
        config_table: Value,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            // Do not inherit host secrets (BOOKCLERK_AUTH_PASSWORD, AWS keys,
            // operator token, DB URLs, …). Allowlist only non-sensitive vars.
            .env_clear();
        for (key, value) in std::env::vars() {
            if plugin_env_allowed(&key) {
                cmd.env(key, value);
            }
        }
        cmd.env("BOOKCLERK_PLUGIN_ID", id);
        let mut child = cmd.spawn()?;

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

    /// Call a JSON-RPC method and deserialize the result.
    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let value = self.call_raw(method, params).await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value> {
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
        "XDG_RUNTIME_DIR",
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
