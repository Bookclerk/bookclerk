//! Reference Echo integration — native Rust guest.
//!
//! Speaks the Workers RPC ABI via [`BookclerkPlugin`] / [`BookclerkPluginGuest`].
//!
//! ```bash
//! bookclerk plugins approve echo_native_rust --yes
//! bookclerk plugins enable echo_native_rust
//! bookclerk plugins echo_native_rust ping --message hi
//! ```

use async_trait::async_trait;
use bookclerk_plugin_abi::{
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    DiagnoseResult, HandshakeParams, HandshakeResult, HealthResult, HostToPluginEvent, PluginError,
    API_VERSION,
};
use bookclerk_plugin_sdk::{BookclerkPlugin, BookclerkPluginGuest};

const PLUGIN_ID: &str = "echo_native_rust";

fn cli_schema() -> CliSchema {
    CliSchema {
        commands: vec![CliCommandSpec {
            name: "ping".into(),
            about: Some("Probe echo plugin".into()),
            args: vec![CliArgSpec {
                name: "message".into(),
                long: Some("message".into()),
                short: Some('m'),
                kind: CliArgKind::String,
                required: false,
                default: Some("hi".into()),
                about: Some("Message to echo".into()),
                positional: false,
            }],
        }],
    }
}

struct EchoPlugin;

#[async_trait]
impl BookclerkPlugin for EchoPlugin {
    async fn handshake(&self, _params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        Ok(HandshakeResult {
            api_version: API_VERSION,
            id: PLUGIN_ID.into(),
            kind: "integration".into(),
            display_name: Some("Echo Integration (native Rust)".into()),
            capabilities: vec![
                "health".into(),
                "diagnose".into(),
                "onEvent".into(),
                "start".into(),
                "cli".into(),
            ],
            cli: Some(cli_schema()),
            ..HandshakeResult::default()
        })
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            id: Some(PLUGIN_ID.into()),
            enabled: Some(true),
            detail: Some("echo_native_rust ready".into()),
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult {
            lines: vec!["echo_native_rust diagnose: ok".into()],
        })
    }

    async fn on_event(&self, event: HostToPluginEvent) -> Result<(), PluginError> {
        eprintln!("echo_native_rust event: {event:?}");
        Ok(())
    }

    async fn start(&self) -> Result<(), PluginError> {
        Ok(())
    }

    async fn cli_describe(&self) -> Result<CliSchema, PluginError> {
        Ok(cli_schema())
    }

    async fn cli_invoke(&self, params: CliInvokeParams) -> Result<CliInvokeResult, PluginError> {
        if params.command != "ping" {
            return Ok(CliInvokeResult {
                exit_code: 2,
                stdout: String::new(),
                stderr: format!("unknown command {}", params.command),
                json: None,
            });
        }
        let message = params
            .args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("hi");
        Ok(CliInvokeResult {
            exit_code: 0,
            stdout: format!("pong: {message}\n"),
            stderr: String::new(),
            json: None,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(EchoPlugin).await?;
    Ok(())
}
