//! `POST /origin/fetch` on the granted channel: the `ORIGIN` binding of a
//! relay `ingress` plugin.
//!
//! The adapter isolate sends one request per `env.ORIGIN.fetch`. Its body is
//! a framed head ([`bookclerk_plugin_abi::origin::encode_origin_frame`]: a
//! length prefix and the [`OriginRequest`] as JSON) followed by the request
//! body. The reply is `200` with a framed [`OriginResponseHead`] followed by
//! the library's response body, both streamed: a request body is never
//! buffered here and the response starts as soon as its head is ready. A
//! refusal before the library sees the request answers with the plugin
//! error instead.
//!
//! [`OriginRequest`]: bookclerk_plugin_abi::OriginRequest

use std::pin::Pin;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use bookclerk_plugin_abi::origin::{
    check_origin_response, decode_origin_request_frame, encode_origin_frame, split_origin_frame,
    CheckedOriginRequest, OriginResponseHead,
};
use bookclerk_plugin_abi::{Origin, PluginError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use super::{
    body_framing, pump_http_body, write_plugin_error, write_status, ChannelReader, GrantedCmd,
    GrantedTable,
};

/// What the vat side hands back for one origin request: the response head
/// and, when the response has one, its body chunks.
pub(super) struct OriginOpened {
    /// Status and header fields, already checked.
    pub(super) head: OriginResponseHead,
    /// Body chunks, or the reason the body broke.
    pub(super) body_rx: Option<mpsc::Receiver<Result<Vec<u8>, String>>>,
}

/// Serves `POST /origin/fetch` for one connection.
///
/// # Errors
///
/// An I/O error on the connection, or a broken response body (the
/// connection is dropped so the isolate sees a truncated body, never a
/// clean end).
pub(super) async fn serve<R, W>(
    mut reader: R,
    writer: &mut W,
    headers: Vec<(String, String)>,
    prefix: Vec<u8>,
    invocation: String,
    cmds: mpsc::Sender<GrantedCmd>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    if let Err(err) = body_framing(&headers) {
        write_status(writer, 400, &format!("malformed body: {err}")).await?;
        return Ok(());
    }
    // The whole HTTP body (frame and request body) is decoded off the
    // connection by its own task, so the response can start before the
    // request body ends.
    let (raw_tx, mut raw_rx) = mpsc::channel::<Result<Vec<u8>, String>>(4);
    tokio::spawn(async move {
        if let Err(err) = pump_http_body(&mut reader, &headers, prefix, &raw_tx).await {
            let _ = raw_tx.send(Err(err.to_string())).await;
        }
    });
    let mut buffered = Vec::new();
    let (request, rest) = loop {
        match split_origin_frame(&buffered) {
            Ok(Some((json, rest))) => match decode_origin_request_frame(json) {
                Ok(request) => break (request, rest.to_vec()),
                Err(err) => {
                    write_plugin_error(writer, &err).await?;
                    return Ok(());
                }
            },
            Ok(None) => {}
            Err(err) => {
                write_plugin_error(writer, &err).await?;
                return Ok(());
            }
        }
        match raw_rx.recv().await {
            Some(Ok(chunk)) => buffered.extend_from_slice(&chunk),
            Some(Err(_)) | None => {
                write_plugin_error(
                    writer,
                    &PluginError::invalid_params("the origin request ended inside its head"),
                )
                .await?;
                return Ok(());
            }
        }
    };
    // The request body: what followed the frame, then the rest of the
    // stream as it arrives.
    let (body_tx, body_rx) = mpsc::channel::<Result<Vec<u8>, String>>(4);
    tokio::spawn(async move {
        if !rest.is_empty() && body_tx.send(Ok(rest)).await.is_err() {
            return;
        }
        while let Some(chunk) = raw_rx.recv().await {
            if body_tx.send(chunk).await.is_err() {
                return;
            }
        }
    });
    let (resp_tx, resp_rx) = oneshot::channel();
    cmds.send(GrantedCmd::OriginFetch {
        invocation,
        request,
        body_rx,
        resp: resp_tx,
    })
    .await
    .context("granted dispatch closed")?;
    let opened = match resp_rx.await.context("granted origin fetch dropped")? {
        Ok(opened) => opened,
        Err(err) => {
            write_plugin_error(writer, &err).await?;
            return Ok(());
        }
    };
    let frame = encode_origin_frame(&opened.head).map_err(anyhow::Error::msg)?;
    writer
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
              transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        )
        .await?;
    write_chunk(writer, &frame).await?;
    if let Some(mut body_rx) = opened.body_rx {
        while let Some(chunk) = body_rx.recv().await {
            let chunk =
                chunk.map_err(|err| anyhow::anyhow!("origin response body failed: {err}"))?;
            write_chunk(writer, &chunk).await?;
        }
    }
    writer.write_all(b"0\r\n\r\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Writes one HTTP chunk; an empty one is skipped (it would end the body).
async fn write_chunk<W: AsyncWrite + Unpin>(writer: &mut W, chunk: &[u8]) -> Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    writer
        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
        .await?;
    writer.write_all(chunk).await?;
    writer.write_all(b"\r\n").await?;
    Ok(())
}

/// Vat side of one origin request: the slot must carry the `ORIGIN`
/// binding; the library's response head is checked before it goes back.
///
/// # Errors
///
/// `unauthorized` for an unknown, revoked or expired grant, `forbidden`
/// for a grant without `ORIGIN`, or the library's refusal.
pub(super) async fn dispatch(
    table: &GrantedTable,
    invocation: String,
    request: CheckedOriginRequest,
    body_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
) -> Result<OriginOpened, PluginError> {
    let origin: Rc<dyn Origin> = {
        let mut table = table.borrow_mut();
        let slot = table.get_mut(&invocation).ok_or_else(|| {
            PluginError::new(
                bookclerk_plugin_abi::PluginErrorCode::Unauthorized,
                "unknown or revoked grant",
            )
        })?;
        if slot.expires <= std::time::Instant::now() {
            table.remove(&invocation);
            return Err(PluginError::new(
                bookclerk_plugin_abi::PluginErrorCode::Unauthorized,
                "grant expired",
            ));
        }
        slot.origin
            .clone()
            .ok_or_else(|| PluginError::forbidden("ORIGIN is not granted to this invocation"))?
    };
    let body: Pin<Box<dyn AsyncRead + Send>> = Box::pin(ChannelReader {
        rx: body_rx,
        buf: Vec::new(),
    });
    let reply = origin.fetch(request, Some(body)).await?;
    check_origin_response(reply.status, &reply.headers)?;
    let body_rx = reply.body.map(|mut body| {
        let (tx, rx) = mpsc::channel(4);
        tokio::task::spawn_local(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match body.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err.to_string())).await;
                        break;
                    }
                }
            }
        });
        rx
    });
    Ok(OriginOpened {
        head: OriginResponseHead {
            status: reply.status,
            headers: reply.headers,
        },
        body_rx,
    })
}

#[cfg(test)]
#[path = "granted_origin_tests.rs"]
mod tests;
