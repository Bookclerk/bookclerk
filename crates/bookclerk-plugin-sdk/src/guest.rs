//! Low-level guest-side Workers RPC helper over tokio stdin/stdout.
//!
//! Audience: advanced plugin authors who need a raw `(method, params) → Value`
//! dispatch loop. Prefer [`crate::BookclerkPluginGuest`] +
//! [`crate::BookclerkPlugin`] for typed native guests — that path parses
//! camelCase Workers RPC params into ABI DTOs and maps methods to trait
//! methods.
//!
//! Framing: one JSON [`bookclerk_plugin_abi::RpcRequest`] / response object per
//! newline, capped at [`crate::protocol::MAX_RPC_LINE_BYTES`].

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use bookclerk_plugin_abi::{methods, PluginError, PluginErrorCode, RpcRequest, RpcResponse};

use crate::error::{Result, SdkError};
use crate::protocol::MAX_RPC_LINE_BYTES;

/// Guest-side helper: read requests from stdin, write responses to stdout.
///
/// Prefer [`crate::BookclerkPluginGuest`] + [`crate::BookclerkPlugin`] for new
/// plugins (typed method dispatch). This type is the low-level raw-handler loop
/// used by thin adapters and tests.
pub struct PluginGuest;

impl PluginGuest {
    /// Runs a simple dispatch loop on tokio stdin/stdout (Workers RPC framing).
    ///
    /// Each non-empty line is deserialized as an RPC request. The handler
    /// receives the wire method name and JSON params. A `shutdown` method
    /// always ends the loop after the response is written; handler errors on
    /// `shutdown` are coerced to a successful null result so the host can exit
    /// cleanly.
    ///
    /// # Arguments
    ///
    /// * `handler` - Async closure invoked per request. Return `Ok(Value)` for
    ///   the RPC `result`, or `Err(String)` which becomes an
    ///   [`PluginErrorCode::Internal`] wire error (except on `shutdown`).
    ///
    /// # Returns
    ///
    /// `Ok(())` when stdin EOF is reached or after a successful `shutdown`
    /// response flush.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] on I/O failures, oversize frames
    /// ([`MAX_RPC_LINE_BYTES`]), or response JSON serialization errors.
    /// Malformed request lines are logged and skipped (no response).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use bookclerk_plugin_sdk::PluginGuest;
    /// use serde_json::{json, Value};
    ///
    /// # async fn demo() -> bookclerk_plugin_sdk::Result<()> {
    /// PluginGuest::serve(|method, _params| async move {
    ///     if method == "health" {
    ///         Ok(json!({ "ok": true }))
    ///     } else {
    ///         Err(format!("unsupported {method}"))
    ///     }
    /// })
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
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
