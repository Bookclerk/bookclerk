//! Guest-side JSON-RPC helper (stdio).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::Result;

/// Guest-side helper: read requests from stdin, write responses to stdout.
///
/// Third-party Rust plugins depend on this crate and call [`PluginGuest::serve`].
pub struct PluginGuest;

impl PluginGuest {
    /// Run a simple dispatch loop on tokio stdin/stdout.
    pub async fn serve<F, Fut>(mut handler: F) -> Result<()>
    where
        F: FnMut(String, Value) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<Value, String>>,
    {
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let req: GuestRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(%err, "invalid request");
                    continue;
                }
            };
            let outcome = handler(req.method.clone(), req.params.unwrap_or(Value::Null)).await;
            let resp = match outcome {
                Ok(result) => GuestResponse {
                    jsonrpc: "2.0",
                    id: req.id,
                    result: Some(result),
                    error: None,
                },
                Err(message) => GuestResponse {
                    jsonrpc: "2.0",
                    id: req.id,
                    result: None,
                    error: Some(GuestError {
                        code: -32000,
                        message,
                    }),
                },
            };
            let mut out = serde_json::to_string(&resp)?;
            out.push('\n');
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct GuestRequest {
    id: Option<u64>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct GuestResponse {
    jsonrpc: &'static str,
    id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<GuestError>,
}

#[derive(Debug, Serialize)]
struct GuestError {
    code: i64,
    message: String,
}
