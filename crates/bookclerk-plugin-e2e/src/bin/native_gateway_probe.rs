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
        let port: u16 = arg(&params, "port")
            .parse()
            .map_err(|err| PluginError::invalid_params(format!("port: {err}")))?;
        let outcome = match arg(&params, "op") {
            "connect" => match mediated(host, port, arg(&params, "payload")).await {
                Ok(echo) => serde_json::json!({ "ok": true, "echo": echo }),
                Err(error) => serde_json::json!({ "ok": false, "error": error }),
            },
            "ambient" => match ambient(host, port) {
                Ok(()) => serde_json::json!({ "ok": true }),
                Err(error) => serde_json::json!({ "ok": false, "error": error }),
            },
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
    serve(Root).await?;
    Ok(())
}
