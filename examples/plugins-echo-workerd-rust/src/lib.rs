//! Echo workerd guest logic compiled to Wasm.
//!
//! JS `modules/index.js` loads this module and forwards Workers RPC methods.
//! Types come from [`bookclerk_plugin_abi`] — the same ABI as
//! `bookclerk_plugin_sdk::BookclerkPlugin`.

use bookclerk_plugin_abi::{
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    DiagnoseResult, HandshakeResult, HealthResult, API_VERSION,
};
use serde_json::{json, Value};

const PLUGIN_ID: &str = "echo_workerd_rust";

fn cli_schema() -> CliSchema {
    CliSchema {
        commands: vec![CliCommandSpec {
            name: "ping".into(),
            about: Some("Probe echo plugin".into()),
            args: vec![CliArgSpec {
                name: "message".into(),
                long: Some("message".into()),
                short: None,
                kind: CliArgKind::String,
                required: false,
                default: Some("hi".into()),
                about: Some("Message to echo".into()),
                positional: false,
            }],
        }],
    }
}

/// Dispatches one Workers RPC method and returns the JSON result payload.
///
/// Called from the JS `WorkerEntrypoint` glue (and unit tests). Unknown methods
/// return `Err`; successful handlers serialize ABI DTOs as JSON text.
///
/// # Arguments
///
/// * `method` - Workers RPC method name (`handshake`, `health`, `cliInvoke`, …).
/// * `params_json` - JSON params object, or empty/`null` when the method takes none.
///
/// # Returns
///
/// JSON text of the method result (may be the literal `null`).
///
/// # Errors
///
/// Returns an error string when params fail to parse, serialization fails, or
/// the method is unsupported.
pub fn dispatch_json(method: &str, params_json: &str) -> Result<String, String> {
    let params: Value = if params_json.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(params_json).map_err(|e| e.to_string())?
    };
    let result = match method {
        "handshake" => serde_json::to_value(HandshakeResult {
            api_version: API_VERSION,
            id: PLUGIN_ID.into(),
            kind: "integration".into(),
            display_name: Some("Echo Integration (workerd Rust/Wasm)".into()),
            capabilities: vec![
                "health".into(),
                "diagnose".into(),
                "onEvent".into(),
                "cli".into(),
            ],
            cli: Some(cli_schema()),
            ..HandshakeResult::default()
        })
        .map_err(|e| e.to_string())?,
        "shutdown" => Value::Null,
        "health" => serde_json::to_value(HealthResult {
            ok: true,
            id: Some(PLUGIN_ID.into()),
            enabled: Some(true),
            detail: Some("echo workerd rust wasm plugin ready".into()),
        })
        .map_err(|e| e.to_string())?,
        "diagnose" => serde_json::to_value(DiagnoseResult {
            lines: vec!["echo_workerd_rust: ok".into()],
        })
        .map_err(|e| e.to_string())?,
        "onEvent" => Value::Null,
        "cliDescribe" => serde_json::to_value(cli_schema()).map_err(|e| e.to_string())?,
        "cliInvoke" => {
            let p: CliInvokeParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let out = if p.command != "ping" {
                CliInvokeResult {
                    exit_code: 2,
                    stdout: String::new(),
                    stderr: format!("unknown command {}", p.command),
                    json: None,
                }
            } else {
                let message = p
                    .args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("hi");
                CliInvokeResult {
                    exit_code: 0,
                    stdout: format!("pong: {message}\n"),
                    stderr: String::new(),
                    json: Some(json!({ "pong": message })),
                }
            };
            serde_json::to_value(out).map_err(|e| e.to_string())?
        }
        other => return Err(format!("unsupported method: {other}")),
    };
    serde_json::to_string(&result).map_err(|e| e.to_string())
}

#[cfg(target_arch = "wasm32")]
mod wasm_api {
    use super::dispatch_json;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
    }

    /// Forwards a Workers RPC call into [`super::dispatch_json`].
    ///
    /// # Arguments
    ///
    /// * `method` - Workers RPC method name.
    /// * `params_json` - JSON params (may be empty).
    ///
    /// # Errors
    ///
    /// Maps dispatch failures to [`JsError`].
    #[wasm_bindgen]
    pub fn dispatch(method: &str, params_json: &str) -> Result<String, JsError> {
        dispatch_json(method, params_json).map_err(|e| JsError::new(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_roundtrip() {
        let out =
            dispatch_json("cliInvoke", r#"{"command":"ping","args":{"message":"ci"}}"#).unwrap();
        assert!(out.contains("pong: ci"));
    }

    #[test]
    fn health_detail() {
        let out = dispatch_json("health", "{}").unwrap();
        assert!(out.contains("echo workerd rust wasm plugin ready"));
    }
}
