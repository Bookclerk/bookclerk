//! Echo workerd guest logic compiled to Wasm.
//!
//! JS `modules/index.js` loads this module and forwards Workers RPC methods.
//! Identity comes from `plugin.toml` (`api_version = 3`) via the launcher's
//! manifest projection; Wasm still implements health / diagnose / onEvent / CLI.

use bookclerk_plugin_abi::{
    CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams, CliInvokeResult, CliSchema,
    ExtensibleConfig, HealthOk,
};
use serde_json::{json, Value};

/// Describe / health plugin id (`echo_workerd_rust`); must match `plugin.toml`.
const PLUGIN_ID: &str = "echo_workerd_rust";

/// CLI schema advertised at `cliDescribe` (`ping --message`).
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
/// Called from the JS `BookclerkEntrypoint` / `Cli` glue (and unit tests). Unknown methods
/// return `Err`; successful handlers serialize ABI DTOs as JSON text.
///
/// # Arguments
///
/// * `method` - Workers RPC method name (`health`, `cliInvoke`, …).
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
        "describe" => json!({
            "apiVersion": 2,
            "id": PLUGIN_ID,
            "kind": "integration",
            "displayName": "Echo Integration (workerd Rust/Wasm)",
            "rpcFeatures": ["rpc.scalarLimits"],
            "scalarLimits": {
                "maxScalarBytes": 262144,
                "maxStreamWindowBytes": 1048576,
                "maxListPage": 256,
            },
            "supportedRoles": ["integration"],
            "capabilities": ["health", "diagnose", "onEvent", "cli"],
            "cli": cli_schema(),
        }),
        "shutdown" => Value::Null,
        "health" => serde_json::to_value(HealthOk {
            ok: true,
            detail: format!("{PLUGIN_ID}: echo workerd rust wasm plugin ready"),
        })
        .map_err(|e| e.to_string())?,
        "diagnose" => json!(["echo_workerd_rust: ok"]),
        "onEvent" => json!({ "kind": "ack" }),
        "cliDescribe" => serde_json::to_value(cli_schema()).map_err(|e| e.to_string())?,
        "cliInvoke" => {
            let p: CliInvokeParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let out = if p.command != "ping" {
                CliInvokeResult {
                    exit_code: 2,
                    stderr: format!("unknown command {}", p.command),
                    ..CliInvokeResult::default()
                }
            } else {
                let message = p
                    .args
                    .iter()
                    .find(|arg| arg.name == "message")
                    .map_or("hi", |arg| arg.value.as_str());
                CliInvokeResult {
                    exit_code: 0,
                    stdout: format!("pong: {message}\n"),
                    payload: ExtensibleConfig::json(&json!({ "pong": message })),
                    ..CliInvokeResult::default()
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
        let out = dispatch_json(
            "cliInvoke",
            r#"{"command":"ping","args":[{"name":"message","value":"ci"}]}"#,
        )
        .unwrap();
        assert!(out.contains("pong: ci"));
    }

    #[test]
    fn health_detail() {
        let out = dispatch_json("health", "{}").unwrap();
        assert!(out.contains("echo workerd rust wasm plugin ready"));
    }

    #[test]
    fn describe_reports_api_version() {
        let out = dispatch_json("describe", "{}").unwrap();
        assert!(out.contains("\"apiVersion\":2"));
        assert!(out.contains("echo_workerd_rust"));
    }
}
