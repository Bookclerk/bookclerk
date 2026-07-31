//! Minimal external integration plugin for Bookclerk.
//!
//! Speaks newline-delimited JSON-RPC on stdio. Install `plugin.toml` + this
//! binary under `$BOOKCLERK_FILES_DIR/plugins/echo/` and enable in config:
//!
//! ```toml
//! [integrations.echo]
//! enabled = true
//! ```
//!
//! Declares a sample CLI command:
//!
//! ```bash
//! bookclerk plugins echo ping --message hi
//! ```

use bookclerk_plugin_sdk::{
    methods, CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    HandshakeResult, HealthDto, PluginGuest, PLUGIN_API_VERSION,
};
use serde_json::{json, Value};

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    PluginGuest::serve(|method, params| async move {
        match method.as_str() {
            methods::HANDSHAKE => Ok(serde_json::to_value(HandshakeResult {
                api_version: PLUGIN_API_VERSION,
                id: "echo".into(),
                kind: "integration".into(),
                display_name: Some("Echo Integration".into()),
                capabilities: vec![
                    "health".into(),
                    "diagnose".into(),
                    "on_event".into(),
                    "start".into(),
                    "cli".into(),
                ],
                cli: Some(cli_schema()),
                ..HandshakeResult::default()
            })
            .unwrap()),
            methods::HEALTH => Ok(serde_json::to_value(HealthDto {
                id: "echo".into(),
                enabled: true,
                ok: true,
                detail: Some("echo plugin ready".into()),
            })
            .unwrap()),
            methods::DIAGNOSE => Ok(json!([
                "echo plugin diagnose: ok",
                format!("params={params}")
            ])),
            methods::START => Ok(json!({ "ok": true })),
            methods::ON_EVENT => {
                eprintln!("echo-integration event: {params}");
                Ok(json!({ "ok": true }))
            }
            methods::CLI_DESCRIBE => Ok(serde_json::to_value(cli_schema()).unwrap()),
            methods::CLI_INVOKE => {
                let invoke: CliInvokeParams = serde_json::from_value(params)
                    .map_err(|err| format!("invalid cli.invoke params: {err}"))?;
                if invoke.command != "ping" {
                    return Err(format!("unknown command: {}", invoke.command));
                }
                let message = invoke
                    .args
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("hi");
                Ok(serde_json::to_value(CliInvokeResult {
                    exit_code: 0,
                    stdout: format!("pong: {message}\n"),
                    stderr: String::new(),
                    json: Some(json!({ "pong": message })),
                })
                .unwrap())
            }
            other => Err(format!("method not found: {other}")),
        }
    })
    .await?;
    Ok(())
}
