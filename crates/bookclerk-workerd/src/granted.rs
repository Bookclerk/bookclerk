//! Host-granted Source/Destination HTTP reverse channel for workerd JobHandler.
//!
//! HTTP accept/read/write runs on the multi-thread runtime (`tokio::spawn`) so
//! it is not starved by the Cap'n Proto vat `LocalSet`. Stub calls (`Source`,
//! `Destination`, `ProgressSink`) stay on the vat thread via a command channel.

#![allow(clippy::missing_docs_in_private_items)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use anyhow::{bail, Context as _, Result};
use bookclerk_plugin_abi::v2::{Destination, ObjectMetadata, ProgressSink, Source, WriteOptions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// One JobHandler invocation's granted stubs (vat-thread only).
pub struct GrantedSlot {
    /// Host-granted input source.
    pub input: Option<Box<dyn Source>>,
    /// Host-granted output destination.
    pub output: Option<Box<dyn Destination>>,
    /// Progress sink.
    pub progress: Option<Box<dyn ProgressSink>>,
    /// Absolute expiry; dispatch fails closed after this instant.
    pub expires: std::time::Instant,
    /// Whether `open` is permitted on this grant.
    pub allow_open: bool,
    /// Whether `put` is permitted on this grant.
    pub allow_put: bool,
    /// Whether `progress` is permitted on this grant.
    pub allow_progress: bool,
}

/// Invocation-id → granted stubs.
pub type GrantedTable = Rc<RefCell<HashMap<String, GrantedSlot>>>;

enum GrantedCmd {
    Open {
        invocation: String,
        key: String,
        resp: oneshot::Sender<Result<OpenOk, String>>,
    },
    Put {
        invocation: String,
        key: String,
        options: WriteOptions,
        body_rx: mpsc::Receiver<Vec<u8>>,
        resp: oneshot::Sender<Result<PutOk, String>>,
    },
    Progress {
        invocation: String,
        percent: f32,
        message: String,
        resp: oneshot::Sender<Result<(), String>>,
    },
}

struct OpenOk {
    meta: ObjectMetadata,
    body_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
}

struct PutOk {
    key: String,
    bytes_written: u64,
    etag: Option<String>,
}

/// Starts the Send HTTP accept loop and the vat-thread command dispatcher.
///
/// Must be called from a `LocalSet` (dispatcher is `spawn_local`).
pub fn spawn_granted<L>(listener: L, token: String, table: GrantedTable)
where
    L: GrantedListener + Send + 'static,
    L::Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(32);
    tokio::task::spawn_local(dispatch_granted(rx, table));
    tokio::spawn(async move {
        if let Err(err) = serve_granted_http(listener, token, tx).await {
            tracing::warn!(error = %err, "granted server exited");
        }
    });
}

async fn dispatch_granted(mut rx: mpsc::Receiver<GrantedCmd>, table: GrantedTable) {
    while let Some(cmd) = rx.recv().await {
        let table = Rc::clone(&table);
        tokio::task::spawn_local(async move {
            match cmd {
                GrantedCmd::Open {
                    invocation,
                    key,
                    resp,
                } => {
                    let _ = resp.send(dispatch_open(&table, invocation, key).await);
                }
                GrantedCmd::Put {
                    invocation,
                    key,
                    options,
                    body_rx,
                    resp,
                } => {
                    let _ =
                        resp.send(dispatch_put(&table, invocation, key, options, body_rx).await);
                }
                GrantedCmd::Progress {
                    invocation,
                    percent,
                    message,
                    resp,
                } => {
                    let _ =
                        resp.send(dispatch_progress(&table, invocation, percent, message).await);
                }
            }
        });
    }
}

fn take_slot_source(table: &GrantedTable, invocation: &str) -> Result<Box<dyn Source>, String> {
    let mut table = table.borrow_mut();
    let slot = table
        .get_mut(invocation)
        .ok_or_else(|| "unknown or revoked grant".to_string())?;
    if slot.expires <= std::time::Instant::now() {
        table.remove(invocation);
        return Err("grant expired".into());
    }
    if !slot.allow_open {
        return Err("open not permitted on this grant".into());
    }
    slot.input
        .take()
        .ok_or_else(|| "source already in use".to_string())
}

async fn dispatch_open(
    table: &GrantedTable,
    invocation: String,
    key: String,
) -> Result<OpenOk, String> {
    let input = take_slot_source(table, &invocation)?;
    let opened = input.open(&key).await.map_err(|err| err.to_string())?;
    {
        let mut table = table.borrow_mut();
        if let Some(slot) = table.get_mut(&invocation) {
            slot.input = Some(input);
        }
    }
    let (tx, rx) = mpsc::channel(4);
    let mut body = opened.body;
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
    Ok(OpenOk {
        meta: opened.meta,
        body_rx: rx,
    })
}

async fn dispatch_put(
    table: &GrantedTable,
    invocation: String,
    key: String,
    options: WriteOptions,
    body_rx: mpsc::Receiver<Vec<u8>>,
) -> Result<PutOk, String> {
    let output = {
        let mut table = table.borrow_mut();
        let slot = table
            .get_mut(&invocation)
            .ok_or_else(|| "unknown or revoked grant".to_string())?;
        if slot.expires <= std::time::Instant::now() {
            table.remove(&invocation);
            return Err("grant expired".into());
        }
        if !slot.allow_put {
            return Err("put not permitted on this grant".into());
        }
        slot.output
            .take()
            .ok_or_else(|| "destination already in use".to_string())?
    };
    let body: Pin<Box<dyn AsyncRead + Send>> = Box::pin(ChannelReader {
        rx: body_rx,
        buf: Vec::new(),
    });
    let put = output
        .put(&key, body, options)
        .await
        .map_err(|err| err.to_string())?;
    {
        let mut table = table.borrow_mut();
        if let Some(slot) = table.get_mut(&invocation) {
            slot.output = Some(output);
        }
    }
    Ok(PutOk {
        key: put.key,
        bytes_written: put.bytes_written,
        etag: put.etag,
    })
}

async fn dispatch_progress(
    table: &GrantedTable,
    invocation: String,
    percent: f32,
    message: String,
) -> Result<(), String> {
    let progress = {
        let mut table = table.borrow_mut();
        let slot = table
            .get_mut(&invocation)
            .ok_or_else(|| "unknown or revoked grant".to_string())?;
        if slot.expires <= std::time::Instant::now() {
            table.remove(&invocation);
            return Err("grant expired".into());
        }
        if !slot.allow_progress {
            return Err("progress not permitted on this grant".into());
        }
        slot.progress
            .take()
            .ok_or_else(|| "progress already in use".to_string())?
    };
    progress
        .report(percent, &message)
        .await
        .map_err(|err| err.to_string())?;
    {
        let mut table = table.borrow_mut();
        if let Some(slot) = table.get_mut(&invocation) {
            slot.progress = Some(progress);
        }
    }
    Ok(())
}

struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.buf.is_empty() {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => self.buf = chunk,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = self.buf.len().min(buf.remaining());
        buf.put_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Poll::Ready(Ok(()))
    }
}

async fn serve_granted_http<L>(
    listener: L,
    token: String,
    cmds: mpsc::Sender<GrantedCmd>,
) -> Result<()>
where
    L: GrantedListener,
    L::Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tracing::info!("granted HTTP reverse channel listening");
    loop {
        let stream = match listener.accept().await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(error = %err, "granted accept failed");
                continue;
            }
        };
        let token = token.clone();
        let cmds = cmds.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_conn(stream, &token, cmds).await {
                tracing::warn!(error = %err, "granted HTTP request failed");
            }
        });
    }
}

/// Accept loop abstraction (unix or tcp).
pub trait GrantedListener {
    /// Accepted connection type.
    type Stream;
    /// Accept one connection.
    fn accept(&self) -> impl std::future::Future<Output = Result<Self::Stream>> + Send;
}

impl GrantedListener for tokio::net::UnixListener {
    type Stream = tokio::net::UnixStream;
    async fn accept(&self) -> Result<Self::Stream> {
        Ok(self.accept().await?.0)
    }
}

impl GrantedListener for tokio::net::TcpListener {
    type Stream = tokio::net::TcpStream;
    async fn accept(&self) -> Result<Self::Stream> {
        Ok(self.accept().await?.0)
    }
}

/// Isolate-wide `BRIDGE_TOKEN` must not claim a per-invocation grant slot.
fn grant_bearer_is_unusable(provided: &str, bridge_token: &str) -> bool {
    provided.is_empty() || provided == bridge_token
}

async fn handle_conn<S>(stream: S, token: &str, cmds: mpsc::Sender<GrantedCmd>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = reader;
    let (method, path, headers, prefix) = read_request_head(&mut reader).await?;
    tracing::debug!(method = %method, path = %path, "granted HTTP request");
    let auth = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let (path_only, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
    let key = query_param(query, "key").unwrap_or_default();
    let provided = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .unwrap_or("");
    // Per-invocation grant tokens are the capability. An isolate-wide
    // BRIDGE_TOKEN cannot claim another principal's live slot.
    if grant_bearer_is_unusable(provided, token) {
        write_status(&mut writer, 401, "unauthorized").await?;
        return Ok(());
    }
    let invocation = provided.to_string();

    if method == "GET" && path_only == "/open" {
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(GrantedCmd::Open {
            invocation,
            key: key.clone(),
            resp: resp_tx,
        })
        .await
        .context("granted dispatch closed")?;
        let opened = resp_rx
            .await
            .context("granted open dropped")?
            .map_err(anyhow::Error::msg)?;
        let mut head = format!(
            "HTTP/1.1 200 OK\r\nconnection: close\r\nx-bookclerk-key: {}\r\nx-bookclerk-size: {}\r\ntransfer-encoding: chunked\r\n",
            opened.meta.key, opened.meta.size
        );
        if let Some(ct) = &opened.meta.content_type {
            head.push_str(&format!(
                "x-bookclerk-content-type: {ct}\r\ncontent-type: {ct}\r\n"
            ));
        }
        head.push_str("\r\n");
        writer.write_all(head.as_bytes()).await?;
        let mut body_rx = opened.body_rx;
        while let Some(chunk) = body_rx.recv().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(error = %err, "granted source body failed; aborting HTTP body");
                    return Err(anyhow::anyhow!("granted source body failed: {err}"));
                }
            };
            if chunk.is_empty() {
                continue;
            }
            let hdr = format!("{:x}\r\n", chunk.len());
            writer.write_all(hdr.as_bytes()).await?;
            writer.write_all(&chunk).await?;
            writer.write_all(b"\r\n").await?;
        }
        writer.write_all(b"0\r\n\r\n").await?;
        writer.flush().await?;
        return Ok(());
    }

    if method == "PUT" && path_only == "/put" {
        let content_type = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());
        let content_length = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok());
        let (body_tx, body_rx) = mpsc::channel::<Vec<u8>>(4);
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(GrantedCmd::Put {
            invocation,
            key,
            options: WriteOptions {
                content_type,
                content_length,
                sha256: None,
                commit_token: None,
                stage_only: false,
            },
            body_rx,
            resp: resp_tx,
        })
        .await
        .context("granted dispatch closed")?;
        pump_http_body(&mut reader, &headers, prefix, &body_tx).await?;
        drop(body_tx);
        let put = resp_rx
            .await
            .context("granted put dropped")?
            .map_err(anyhow::Error::msg)?;
        let json = serde_json::json!({
            "key": put.key,
            "bytesWritten": put.bytes_written,
            "etag": put.etag,
        });
        let payload = serde_json::to_vec(&json)?;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            payload.len()
        );
        writer.write_all(resp.as_bytes()).await?;
        writer.write_all(&payload).await?;
        writer.flush().await?;
        return Ok(());
    }

    if method == "POST" && path_only == "/progress" {
        let rest = read_content(&mut reader, &headers, prefix).await?;
        let value: serde_json::Value = serde_json::from_slice(&rest).unwrap_or_default();
        let percent = value
            .get("percent")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0) as f32;
        let message = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(GrantedCmd::Progress {
            invocation,
            percent,
            message,
            resp: resp_tx,
        })
        .await
        .context("granted dispatch closed")?;
        resp_rx
            .await
            .context("granted progress dropped")?
            .map_err(anyhow::Error::msg)?;
        write_status(&mut writer, 200, "ok").await?;
        return Ok(());
    }

    write_status(&mut writer, 404, "not found").await?;
    Ok(())
}

pub(crate) async fn pump_http_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    headers: &[(String, String)],
    prefix: Vec<u8>,
    body_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let chunked = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
        .map(|(_, v)| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    if chunked {
        let mut buf = prefix;
        loop {
            let size = loop {
                if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                    let line = buf.drain(..pos).collect::<Vec<_>>();
                    buf.drain(..2);
                    let line = std::str::from_utf8(&line).unwrap_or("0");
                    break usize::from_str_radix(line.trim(), 16).unwrap_or(0);
                }
                let mut tmp = [0u8; 64];
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    bail!("truncated chunked body");
                }
                buf.extend_from_slice(&tmp[..n]);
            };
            if size == 0 {
                return Ok(());
            }
            while buf.len() < size {
                let mut tmp = vec![0u8; (size - buf.len()).min(64 * 1024)];
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    bail!("truncated chunked body");
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let chunk: Vec<u8> = buf.drain(..size.min(buf.len())).collect();
            if !chunk.is_empty() {
                body_tx
                    .send(chunk)
                    .await
                    .context("granted put body closed")?;
            }
            while buf.len() < 2 {
                let mut tmp = [0u8; 2];
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    bail!("truncated chunked body");
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            if buf.len() >= 2 {
                buf.drain(..2);
            }
        }
    }
    if let Some(len) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
    {
        if prefix.len() > len {
            bail!("body exceeded Content-Length");
        }
        let mut remaining = len.saturating_sub(prefix.len());
        if !prefix.is_empty() {
            body_tx
                .send(prefix)
                .await
                .context("granted put body closed")?;
        }
        let mut buf = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let want = remaining.min(buf.len());
            let n = stream.read(&mut buf[..want]).await?;
            if n == 0 {
                bail!("early EOF before Content-Length");
            }
            remaining -= n;
            body_tx
                .send(buf[..n].to_vec())
                .await
                .context("granted put body closed")?;
        }
        return Ok(());
    }
    if !prefix.is_empty() {
        body_tx
            .send(prefix)
            .await
            .context("granted put body closed")?;
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        body_tx
            .send(buf[..n].to_vec())
            .await
            .context("granted put body closed")?;
    }
    Ok(())
}

async fn read_request_head<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4) {
            let head = std::str::from_utf8(&buf[..idx]).context("request headers")?;
            let mut lines = head.split("\r\n");
            let req = lines.next().unwrap_or("");
            let mut parts = req.split_whitespace();
            let method = parts.next().unwrap_or("GET").to_string();
            let path = parts.next().unwrap_or("/").to_string();
            let mut headers = Vec::new();
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                if let Some((k, v)) = line.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            return Ok((method, path, headers, buf[idx..].to_vec()));
        }
        if buf.len() > 64 * 1024 {
            bail!("request headers too large");
        }
    }
    bail!("eof before request headers")
}

async fn read_content<S: AsyncRead + Unpin>(
    stream: &mut S,
    headers: &[(String, String)],
    mut prefix: Vec<u8>,
) -> Result<Vec<u8>> {
    if let Some(len) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
    {
        while prefix.len() < len {
            let mut tmp = vec![0u8; (len - prefix.len()).min(64 * 1024)];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            prefix.extend_from_slice(&tmp[..n]);
        }
        prefix.truncate(len);
    }
    Ok(prefix)
}

fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k == name {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn write_status<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    body: &str,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn expired_grant_is_revoked() {
        let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
        table.borrow_mut().insert(
            "g1".into(),
            GrantedSlot {
                input: None,
                output: None,
                progress: None,
                expires: Instant::now() - Duration::from_secs(1),
                allow_open: true,
                allow_put: true,
                allow_progress: true,
            },
        );
        let err = match take_slot_source(&table, "g1") {
            Ok(_) => panic!("expired grant must fail"),
            Err(err) => err,
        };
        assert!(err.contains("expired") || err.contains("revoked") || err.contains("unknown"));
        assert!(table.borrow().get("g1").is_none());
    }

    #[test]
    fn unknown_grant_fails_closed() {
        let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
        let err = match take_slot_source(&table, "missing") {
            Ok(_) => panic!("missing grant must fail"),
            Err(err) => err,
        };
        assert!(err.contains("unknown") || err.contains("revoked"));
    }

    #[test]
    fn bridge_token_cannot_claim_a_grant_slot() {
        assert!(grant_bearer_is_unusable("", "bridge"));
        assert!(grant_bearer_is_unusable("bridge", "bridge"));
        assert!(!grant_bearer_is_unusable("grant-abc", "bridge"));
    }

    #[test]
    fn retained_grant_fails_after_teardown() {
        let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
        table.borrow_mut().insert(
            "g-live".into(),
            GrantedSlot {
                input: None,
                output: None,
                progress: None,
                expires: Instant::now() + Duration::from_secs(60),
                allow_open: true,
                allow_put: true,
                allow_progress: true,
            },
        );
        table.borrow_mut().remove("g-live");
        let err = match take_slot_source(&table, "g-live") {
            Ok(_) => panic!("revoked grant must fail"),
            Err(err) => err,
        };
        assert!(err.contains("unknown") || err.contains("revoked"));
    }
}
