//! Guest-side JSON-RPC helper (stdio).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{Result, SdkError};
use crate::protocol::{methods, MAX_RPC_LINE_BYTES};

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
        let mut reader = BufReader::new(stdin);
        let mut stdout = tokio::io::stdout();
        loop {
            let line = match read_rpc_line(&mut reader).await? {
                Some(line) => line,
                None => break,
            };
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
            let is_shutdown = req.method == methods::SHUTDOWN;
            // Call the handler first so a guest can flush work; shutdown still
            // acks even when the handler does not implement the method.
            let outcome = match handler(req.method.clone(), req.params.unwrap_or(Value::Null)).await
            {
                Ok(result) => Ok(result),
                Err(_) if is_shutdown => Ok(Value::Null),
                Err(message) => Err(message),
            };
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
            if is_shutdown {
                break;
            }
        }
        Ok(())
    }
}

/// Read one newline-delimited RPC line, rejecting oversize frames.
async fn read_rpc_line<R: AsyncBufRead + AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let end = pos + 1;
            if buf.len() + end > MAX_RPC_LINE_BYTES {
                return Err(SdkError::message(format!(
                    "RPC line exceeds MAX_RPC_LINE_BYTES ({MAX_RPC_LINE_BYTES})"
                )));
            }
            buf.extend_from_slice(&available[..end]);
            reader.consume(end);
            break;
        }
        if buf.len() + available.len() > MAX_RPC_LINE_BYTES {
            return Err(SdkError::message(format!(
                "RPC line exceeds MAX_RPC_LINE_BYTES ({MAX_RPC_LINE_BYTES})"
            )));
        }
        let n = available.len();
        buf.extend_from_slice(available);
        reader.consume(n);
    }
    // Strip trailing newline (and optional CR).
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|err| SdkError::message(format!("RPC line is not valid UTF-8: {err}")))
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
