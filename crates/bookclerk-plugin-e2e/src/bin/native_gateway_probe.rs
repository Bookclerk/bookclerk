//! Test-only native guest for the `native_gateway` smoke (never staged or shipped).
//!
//! Exports one `probe` CLI command. `op=connect` dials through the SDK socket
//! proxy (`bookclerk_plugin_sdk::net::connect`) and echoes `payload`;
//! `op=ambient` tries a direct `std::net` TCP connect, which the nested
//! deny-network jail must refuse. Results are one JSON object on stdout.

#![allow(clippy::missing_docs_in_private_items)]

use std::time::Duration;

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    manifest_capabilities, serve, Bindings, CliArgKind, CliArgSpec, CliCommandSpec,
    CliInvokeParams, CliInvokeResult, CliSchema, ConnectOptions, Entrypoints, Invocation,
    PluginCli, PluginDescribe, PluginError, PluginWorker, ScalarLimits, SocketAddress,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PLUGIN_ID: &str = "native_gateway_probe";
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const AMBIENT_TIMEOUT: Duration = Duration::from_secs(3);

fn arg_spec(name: &str) -> CliArgSpec {
    CliArgSpec {
        name: name.into(),
        long: Some(name.into()),
        short: None,
        kind: CliArgKind::String,
        required: false,
        default: None,
        about: None,
        positional: false,
    }
}

fn cli_schema() -> CliSchema {
    CliSchema {
        commands: vec![CliCommandSpec {
            name: "probe".into(),
            about: Some("Network probe".into()),
            args: ["op", "host", "port", "payload"]
                .into_iter()
                .map(arg_spec)
                .collect(),
        }],
    }
}

/// The installed `plugin.toml` beside this executable: ports are chosen at
/// test time, so describe() cannot embed the manifest at compile time.
fn installed_manifest() -> Result<String, PluginError> {
    let exe = std::env::current_exe()
        .map_err(|err| PluginError::internal(format!("current_exe: {err}")))?;
    let path = exe
        .parent()
        .ok_or_else(|| PluginError::internal("executable has no parent"))?
        .join("plugin.toml");
    std::fs::read_to_string(&path)
        .map_err(|err| PluginError::internal(format!("read {}: {err}", path.display())))
}

struct Root;

#[async_trait(?Send)]
impl PluginWorker for Root {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        if let Ok(ms) = std::env::var("BOOKCLERK_PROBE_DESCRIBE_DELAY_MS") {
            if let Ok(ms) = ms.parse::<u64>() {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
        }
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: PLUGIN_ID.into(),
            display_name: Some("Native gateway probe".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(&installed_manifest()?)?,
            cli: cli_schema(),
            ..PluginDescribe::default()
        })
    }

    async fn open(
        &self,
        _invocation: Invocation,
        _bindings: Bindings,
    ) -> Result<Entrypoints, PluginError> {
        Ok(Entrypoints {
            cli: Some(Box::new(Probe)),
            ..Entrypoints::default()
        })
    }
}

struct Probe;

fn arg<'a>(params: &'a CliInvokeParams, name: &str) -> &'a str {
    params
        .args
        .iter()
        .find(|a| a.name == name)
        .map_or("", |a| a.value.as_str())
}

async fn mediated(host: &str, port: u16, payload: &str) -> Result<String, String> {
    let address = SocketAddress {
        hostname: host.into(),
        port,
    };
    let mut socket = tokio::time::timeout(
        IO_TIMEOUT,
        bookclerk_plugin_sdk::net::connect(address, ConnectOptions::default()),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|err| err.to_string())?;
    let stream = socket.stream();
    let round_trip = async {
        stream.write_all(payload.as_bytes()).await?;
        stream.flush().await?;
        let mut echoed = vec![0_u8; payload.len()];
        stream.read_exact(&mut echoed).await?;
        Ok::<_, std::io::Error>(echoed)
    };
    let echoed = tokio::time::timeout(IO_TIMEOUT, round_trip)
        .await
        .map_err(|_| "round trip timed out".to_string())?
        .map_err(|err| format!("round trip: {err}"))?;
    String::from_utf8(echoed).map_err(|err| format!("echo not utf-8: {err}"))
}

async fn hold_until_eof(host: &str, port: u16, payload: &str) -> Result<(), String> {
    let address = SocketAddress {
        hostname: host.into(),
        port,
    };
    let mut socket = tokio::time::timeout(
        IO_TIMEOUT,
        bookclerk_plugin_sdk::net::connect(address, ConnectOptions::default()),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|err| err.to_string())?;
    let stream = socket.stream();
    stream
        .write_all(payload.as_bytes())
        .await
        .map_err(|err| format!("write: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| format!("flush: {err}"))?;
    let mut echoed = vec![0_u8; payload.len()];
    tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut echoed))
        .await
        .map_err(|_| "echo timed out".to_string())?
        .map_err(|err| format!("echo: {err}"))?;
    let mut extra = [0_u8; 8];
    let _ = stream.read(&mut extra).await;
    Ok(())
}

/// Spawn parked threads until `limit` or the kernel refuses another task.
///
/// `pids.max` counts threads. A refusal is the cgroup cap, reported as an
/// error string; the caller decides whether that is enforcement.
fn exhaust_threads(limit: &str) -> serde_json::Value {
    let limit: u32 = limit.parse().unwrap_or(0);
    let mut created = 0_u32;
    let mut error = None;
    let mut handles = Vec::new();
    for _ in 0..limit {
        match std::thread::Builder::new()
            .name("bookclerk-probe-park".into())
            .spawn(|| {
                std::thread::park();
            }) {
            Ok(handle) => {
                created += 1;
                handles.push(handle);
            }
            Err(err) => {
                error = Some(err.to_string());
                break;
            }
        }
    }
    // Detach the parked threads. They keep their `pids.max` slots until the
    // session is torn down.
    drop(handles);
    serde_json::json!({
        "ok": error.is_some(),
        "created": created,
        "error": error,
    })
}

/// Fork a sleeper that stays in this process group. `exec` is denied, so the
/// child only calls `pause`. Windows observes the jail's child instead.
fn spawn_pause_descendant() -> serde_json::Value {
    #[cfg(unix)]
    {
        let pid = spawn_pause_child();
        if pid > 0 {
            serde_json::json!({ "ok": true, "pid": pid })
        } else {
            serde_json::json!({ "ok": false, "error": format!("fork returned {pid}") })
        }
    }
    #[cfg(not(unix))]
    {
        serde_json::json!({
            "ok": false,
            "error": "the Windows jail process keeps the probe as its child",
        })
    }
}

#[cfg(unix)]
#[allow(unsafe_code)] // `fork` + `pause`; the child never returns into the runtime.
fn spawn_pause_child() -> i32 {
    extern "C" {
        fn fork() -> i32;
        fn pause() -> i32;
    }
    unsafe {
        let pid = fork();
        if pid == 0 {
            loop {
                pause();
            }
        }
        pid
    }
}

fn ambient(host: &str, port: u16) -> Result<(), String> {
    let addr: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|err| format!("bad address: {err}"))?;
    std::net::TcpStream::connect_timeout(&addr, AMBIENT_TIMEOUT)
        .map(drop)
        .map_err(|err| format!("{:?}: {err}", err.kind()))
}

#[async_trait(?Send)]
impl PluginCli for Probe {
    async fn describe(&self) -> Result<CliSchema, PluginError> {
        Ok(cli_schema())
    }

    async fn invoke(&self, params: CliInvokeParams) -> Result<CliInvokeResult, PluginError> {
        if params.command != "probe" {
            return Ok(CliInvokeResult {
                exit_code: 2,
                stderr: format!("unknown command {}", params.command),
                ..CliInvokeResult::default()
            });
        }
        let host = arg(&params, "host");
        let port: u16 = arg(&params, "port").parse().unwrap_or(0);
        let outcome = match arg(&params, "op") {
            "connect" => match mediated(host, port, arg(&params, "payload")).await {
                Ok(echo) => serde_json::json!({ "ok": true, "echo": echo }),
                Err(error) => serde_json::json!({ "ok": false, "error": error }),
            },
            "ambient" => match ambient(host, port) {
                Ok(()) => serde_json::json!({ "ok": true }),
                Err(error) => serde_json::json!({ "ok": false, "error": error }),
            },
            "env_keys" => {
                let mut keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
                keys.sort();
                let proxy = std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV).ok();
                serde_json::json!({ "ok": true, "keys": keys, "socket_proxy": proxy })
            }
            "hold" => match hold_until_eof(host, port, arg(&params, "payload")).await {
                Ok(()) => serde_json::json!({ "ok": true, "eof": true }),
                Err(error) => serde_json::json!({ "ok": false, "error": error }),
            },
            "block" => {
                let ms = arg(&params, "payload").parse::<u64>().unwrap_or(30_000);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                serde_json::json!({ "ok": true })
            }
            "descendant" => spawn_pause_descendant(),
            "exhaust_threads" => exhaust_threads(arg(&params, "payload")),
            "read_path" => {
                let path = arg(&params, "payload");
                match std::fs::read(path) {
                    Ok(bytes) => serde_json::json!({
                        "ok": true,
                        "len": bytes.len(),
                    }),
                    Err(err) => serde_json::json!({
                        "ok": false,
                        "error": format!("{:?}: {err}", err.kind()),
                    }),
                }
            }
            other => {
                return Err(PluginError::invalid_params(format!("unknown op `{other}`")));
            }
        };
        Ok(CliInvokeResult {
            exit_code: 0,
            stdout: outcome.to_string(),
            ..CliInvokeResult::default()
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("BOOKCLERK_PROBE_EXIT").is_some() {
        return Ok(());
    }
    serve(Root).await?;
    Ok(())
}
