//! Minimal external integration plugin for Libation.
//!
//! Speaks newline-delimited JSON-RPC on stdio. Install `plugin.toml` + this
//! binary under `$LIBATION_FILES_DIR/plugins/echo/` and enable in config:
//!
//! ```toml
//! [integrations.echo]
//! enabled = true
//! ```

use libation_plugin::{methods, HandshakeResult, HealthDto, PluginGuest, PLUGIN_API_VERSION};
use serde_json::json;

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
                ],
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
            other => Err(format!("method not found: {other}")),
        }
    })
    .await?;
    Ok(())
}
