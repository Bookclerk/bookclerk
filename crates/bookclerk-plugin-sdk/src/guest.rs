//! Guest-side Workers RPC helper (stdio).

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use bookclerk_plugin_abi::{methods, PluginError, PluginErrorCode, RpcRequest, RpcResponse};

use crate::error::{Result, SdkError};
use crate::protocol::MAX_RPC_LINE_BYTES;

/// Guest-side helper: read requests from stdin, write responses to stdout.
///
/// Prefer [`crate::BookclerkPluginGuest`] + [`crate::BookclerkPlugin`] for new
/// plugins (typed method dispatch). This type is the low-level raw-handler loop.
pub struct PluginGuest;

impl PluginGuest {
    /// Run a simple dispatch loop on tokio stdin/stdout (Workers RPC framing).
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
            let req: RpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(%err, "invalid request");
                    continue;
                }
            };
            let is_shutdown = req.method == methods::shutdown::NAME;
            let outcome = match handler(req.method.clone(), req.params.unwrap_or(Value::Null)).await
            {
                Ok(result) => Ok(result),
                Err(_) if is_shutdown => Ok(Value::Null),
                Err(message) => Err(message),
            };
            let resp = match outcome {
                Ok(result) => RpcResponse {
                    id: req.id,
                    result: Some(result),
                    error: None,
                },
                Err(message) => RpcResponse {
                    id: req.id,
                    result: None,
                    error: Some(PluginError {
                        code: PluginErrorCode::Internal,
                        message,
                        details: None,
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
    let n = reader.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_RPC_LINE_BYTES {
        return Err(SdkError::message(format!(
            "RPC line exceeds max size ({MAX_RPC_LINE_BYTES} bytes)"
        )));
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}
