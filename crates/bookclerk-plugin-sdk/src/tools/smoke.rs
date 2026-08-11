//! Out-of-tree workerd smoke via `bookclerk-workerd` library (no launcher binary).

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use bookclerk_plugin_manifest::{parse, PluginRuntimeKind};
use bookclerk_workerd::config::{self, ListenSpec};
use bookclerk_workerd::egress::EgressProxy;
use bookclerk_workerd::ensure_workerd;
use bookclerk_workerd::notify::generate_bridge_token;
use serde_json::{json, Value};

/// Smoke a `runtime = "workerd"` plugin: ensure pin → materialize → handshake + health.
pub fn smoke_plugin(plugin_dir: &Path) -> Result<String, String> {
    let root = plugin_dir
        .canonicalize()
        .map_err(|e| format!("resolve {}: {e}", plugin_dir.display()))?;
    let toml_path = root.join("plugin.toml");
    let text = std::fs::read_to_string(&toml_path)
        .map_err(|e| format!("read {}: {e}", toml_path.display()))?;
    let manifest = parse(&text).map_err(|e| format!("parse plugin.toml: {e}"))?;
    if manifest.runtime != PluginRuntimeKind::Workerd {
        return Err(format!(
            "smoke requires runtime = \"workerd\" (got {:?})",
            manifest.runtime
        ));
    }

    let cache = default_cache_dir();
    let workerd_bin = ensure_workerd(&cache).map_err(|e| format!("ensure workerd: {e:#}"))?;

    // Unconfined smoke can use TCP; jailed guests inherit a listen FD via the launcher.
    let port = free_loopback_port().map_err(|e| format!("allocate port: {e}"))?;
    let listen = ListenSpec::TcpLoopback(port);
    let egress = EgressProxy::from_manifest(&manifest);
    let bridge_token = generate_bridge_token();
    let generated = config::materialize(&root, &manifest, &egress, listen, None, &bridge_token)
        .map_err(|e| format!("materialize config: {e:#}"))?;
    let base = generated.listen.client_base_url();

    let mut child = Command::new(&workerd_bin)
        .arg("serve")
        .arg(&generated.config_path)
        .arg(format!("--import-path={}", generated.import_path.display()))
        .current_dir(&generated.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("BOOKCLERK_PLUGIN_ROOT", &root)
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", workerd_bin.display()))?;

    let result = (|| {
        wait_for_health(&base, &bridge_token).map_err(|e| format!("health: {e}"))?;
        let rpc_url = format!("{base}/rpc");
        let handshake = post_rpc(
            &rpc_url,
            &json!({
                "id": 1,
                "method": "handshake",
                "params": { "apiVersion": 1, "config": {} }
            }),
            &bridge_token,
        )?;
        let health = post_rpc(
            &rpc_url,
            &json!({
                "id": 2,
                "method": "health",
                "params": {}
            }),
            &bridge_token,
        )?;
        let detail = json!({
            "plugin": manifest.id,
            "listen": base,
            "handshake": handshake,
            "health": health,
        });
        Ok(format!(
            "smoke ok {}\n{}",
            manifest.id,
            serde_json::to_string_pretty(&detail).unwrap_or_default()
        ))
    })();

    kill_child(&mut child);
    result
}

fn default_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("BOOKCLERK_WORKERD_CACHE") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("bookclerk").join("workerd")
}

fn free_loopback_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn wait_for_health(base: &str, token: &str) -> Result<(), String> {
    let url = format!("{base}/health");
    let deadline = Instant::now() + Duration::from_secs(15);
    let auth = format!("Bearer {token}");
    loop {
        match ureq::get(&url).header("Authorization", &auth).call() {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            _ => {
                if Instant::now() > deadline {
                    return Err(format!("timeout waiting for {url}"));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn post_rpc(url: &str, body: &Value, token: &str) -> Result<Value, String> {
    let mut response = ureq::post(url)
        .header("content-type", "application/json")
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(body)
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read bridge body: {e}"))?;
    if !status.is_success() && status.as_u16() != 400 {
        return Err(format!("bridge HTTP {status}: {text}"));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("parse bridge JSON: {e}"))?;
    if let Some(err) = value.get("error") {
        let code = err
            .get("code")
            .and_then(|c| c.as_str())
            .unwrap_or("internal");
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("bridge error");
        let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("?");
        return Err(format!("RPC {method} failed: {code}: {message}"));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
