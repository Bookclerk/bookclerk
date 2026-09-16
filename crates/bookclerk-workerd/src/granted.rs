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
use bookclerk_plugin_abi::decode_execute_request_bytes;
use bookclerk_plugin_abi::{
    authorize_guest_sql_policy, canonical_execute_request_hash, encoded_execute_request_bytes,
    guest_statement_kind, validate_guest_execute_request, GuestSqlPolicy, PluginError,
};
use bookclerk_plugin_abi::{
    encoded_execute_result_reply_bytes, Destination, GuestDatabase, ObjectMetadata, ProgressSink,
    Source, WriteOptions,
};
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
    /// Host-mediated typed SQL session, when this invocation is granted one.
    pub database: Option<Rc<dyn GuestDatabase>>,
    /// Whether `execute` is permitted on this grant.
    pub allow_database: bool,
    /// Host-issued table/column/function allowlist for guest SQL.
    pub sql_policy: GuestSqlPolicy,
    /// Negotiated `maxRequestBytes`. Zero is fail-closed (empty body
    /// only), never unlimited; grants with a database must store `1..=MAX_SCALAR_BYTES`.
    pub max_request_bytes: u32,
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
    Execute {
        invocation: String,
        request_bytes: Vec<u8>,
        resp: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    AtomicBudget {
        invocation: String,
        resp: oneshot::Sender<Result<u32, String>>,
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
                GrantedCmd::Execute {
                    invocation,
                    request_bytes,
                    resp,
                } => {
                    let _ = resp.send(dispatch_execute(&table, invocation, request_bytes).await);
                }
                GrantedCmd::AtomicBudget { invocation, resp } => {
                    let _ = resp.send(dispatch_atomic_budget(&table, invocation));
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

fn dispatch_atomic_budget(table: &GrantedTable, invocation: String) -> Result<u32, String> {
    let mut table = table.borrow_mut();
    let slot = table
        .get_mut(&invocation)
        .ok_or_else(|| "unknown or revoked grant".to_string())?;
    if slot.expires <= std::time::Instant::now() {
        table.remove(&invocation);
        return Err("grant expired".into());
    }
    if !slot.allow_database {
        return Err("database not permitted on this grant".into());
    }
    if slot.database.is_none() {
        return Err("database not granted".into());
    }
    Ok(slot.max_request_bytes)
}

/// Authorizes a guest typed batch on the granted HTTP path.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] or [`PluginError::payload_too_large`]
/// when the SQL is outside the guest grammar or the encoded request exceeds
/// `max_bytes`.
fn authorize_granted_request(
    req: &mut bookclerk_plugin_abi::ExecuteRequest,
    max_bytes: u32,
    policy: &GuestSqlPolicy,
) -> Result<(), PluginError> {
    validate_guest_execute_request(req)?;
    authorize_guest_sql_policy(req, policy)?;
    for stmt in &mut req.statements {
        stmt.kind = guest_statement_kind(&stmt.sql);
    }
    if req.operation_id.is_empty() {
        req.operation_id = format!("{:032x}", rand::random::<u128>());
    }
    let computed = canonical_execute_request_hash(req)?;
    if !req.request_hash.is_empty() && req.request_hash != computed {
        return Err(PluginError::invalid_params(
            "retry token requestHash does not match the canonical request",
        ));
    }
    req.request_hash = computed;
    if max_bytes > 0 {
        let n = encoded_execute_request_bytes(req)?.len();
        if n > max_bytes as usize {
            return Err(PluginError::payload_too_large(format!(
                "atomic request is {n} bytes; guest maxRequestBytes is {max_bytes}"
            )));
        }
    }
    Ok(())
}

async fn dispatch_execute(
    table: &GrantedTable,
    invocation: String,
    request_bytes: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let mut req = decode_execute_request_bytes(&request_bytes).map_err(|err| err.to_string())?;
    let (db, cap, policy) = {
        let mut table = table.borrow_mut();
        let slot = table
            .get_mut(&invocation)
            .ok_or_else(|| "unknown or revoked grant".to_string())?;
        if slot.expires <= std::time::Instant::now() {
            table.remove(&invocation);
            return Err("grant expired".into());
        }
        if !slot.allow_database {
            return Err("database not permitted on this grant".into());
        }
        let db = slot
            .database
            .clone()
            .ok_or_else(|| "database not granted".to_string())?;
        (db, slot.max_request_bytes, slot.sql_policy.clone())
    };
    if let Err(err) = authorize_granted_request(&mut req, cap, &policy) {
        return encoded_execute_result_reply_bytes(Err(err)).map_err(|err| err.to_string());
    }
    let outcome = db.execute(req).await;
    encoded_execute_result_reply_bytes(outcome).map_err(|err| err.to_string())
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
        let rest = read_content(&mut reader, &headers, prefix, 64 * 1024).await?;
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

    if method == "POST" && path_only == "/db/execute" {
        let (budget_tx, budget_rx) = oneshot::channel();
        cmds.send(GrantedCmd::AtomicBudget {
            invocation: invocation.clone(),
            resp: budget_tx,
        })
        .await
        .context("granted dispatch closed")?;
        let cap = match budget_rx.await.context("granted budget dropped")? {
            Ok(cap) => cap,
            Err(_) => {
                write_status(&mut writer, 401, "unauthorized").await?;
                return Ok(());
            }
        };
        if declared_content_length_over_cap(&headers, cap) {
            write_status(&mut writer, 413, "payload too large").await?;
            return Ok(());
        }
        if transfer_encoding_is_chunked(&headers) {
            write_status(&mut writer, 400, "chunked execute bodies are not supported").await?;
            return Ok(());
        }
        let rest = match read_content(&mut reader, &headers, prefix, cap).await {
            Ok(body) => body,
            Err(err) if err.to_string().contains("payload_too_large") => {
                write_status(&mut writer, 413, "payload too large").await?;
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(GrantedCmd::Execute {
            invocation,
            request_bytes: rest,
            resp: resp_tx,
        })
        .await
        .context("granted dispatch closed")?;
        let payload = resp_rx
            .await
            .context("granted execute dropped")?
            .map_err(anyhow::Error::msg)?;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            payload.len()
        );
        writer.write_all(resp.as_bytes()).await?;
        writer.write_all(&payload).await?;
        writer.flush().await?;
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
    max_bytes: u32,
) -> Result<Vec<u8>> {
    let cap = max_bytes as usize;
    if let Some(len) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
    {
        if len > cap {
            bail!("payload_too_large: Content-Length {len} exceeds {cap}");
        }
        while prefix.len() < len {
            let mut tmp = vec![0u8; (len - prefix.len()).min(64 * 1024)];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            prefix.extend_from_slice(&tmp[..n]);
            if prefix.len() > cap {
                bail!("payload_too_large: body exceeded {cap}");
            }
        }
        prefix.truncate(len);
        return Ok(prefix);
    }
    let chunked = transfer_encoding_is_chunked(headers);
    if chunked {
        let mut body = Vec::new();
        if !prefix.is_empty() {
            body.extend_from_slice(&prefix);
        }
        // Remaining chunk-encoded bytes after the header prefix are not fully
        // parsed here; count whatever was already buffered then read until EOF
        // with a hard cap so a guest cannot grow an unbounded Vec.
        loop {
            if body.len() > cap {
                bail!("payload_too_large: body exceeded {cap}");
            }
            let mut tmp = [0u8; 64 * 1024];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        if body.len() > cap {
            bail!("payload_too_large: body exceeded {cap}");
        }
        return Ok(body);
    }
    while prefix.len() <= cap {
        let mut tmp = [0u8; 64 * 1024];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        prefix.extend_from_slice(&tmp[..n]);
        if prefix.len() > cap {
            bail!("payload_too_large: body exceeded {cap}");
        }
    }
    if prefix.len() > cap {
        bail!("payload_too_large: body exceeded {cap}");
    }
    Ok(prefix)
}

fn transfer_encoding_is_chunked(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
        .map(|(_, v)| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
}

fn declared_content_length_over_cap(headers: &[(String, String)], cap: u32) -> bool {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .is_some_and(|len| len > cap as usize)
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
        413 => "Payload Too Large",
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
                database: None,
                allow_database: false,
                sql_policy: GuestSqlPolicy::deny_all(),
                max_request_bytes: 0,
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
                database: None,
                allow_database: false,
                sql_policy: GuestSqlPolicy::deny_all(),
                max_request_bytes: 0,
            },
        );
        table.borrow_mut().remove("g-live");
        let err = match take_slot_source(&table, "g-live") {
            Ok(_) => panic!("revoked grant must fail"),
            Err(err) => err,
        };
        assert!(err.contains("unknown") || err.contains("revoked"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_grant_is_required_for_execute() {
        use bookclerk_plugin_abi::{
            encoded_execute_request_bytes, DbPlanStatementKind, DbResultSelection, ExecuteRequest,
            TypedDbStatement,
        };
        let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
        table.borrow_mut().insert(
            "g-db".into(),
            GrantedSlot {
                input: None,
                output: None,
                progress: None,
                expires: Instant::now() + Duration::from_secs(60),
                allow_open: true,
                allow_put: true,
                allow_progress: true,
                database: None,
                allow_database: false,
                sql_policy: GuestSqlPolicy::deny_all(),
                max_request_bytes: 0,
            },
        );
        let req = ExecuteRequest {
            operation_id: "op-db".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 1".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let bytes = encoded_execute_request_bytes(&req).expect("encode");
        let err = dispatch_execute(&table, "g-db".into(), bytes)
            .await
            .expect_err("missing database grant must fail");
        assert!(
            err.contains("not permitted") || err.contains("not granted"),
            "{err}"
        );
    }

    #[test]
    fn declared_content_length_over_cap_is_rejected_before_read() {
        assert!(declared_content_length_over_cap(
            &[("content-length".into(), "16".into())],
            0
        ));
        assert!(!declared_content_length_over_cap(
            &[("content-length".into(), "0".into())],
            0
        ));
        assert!(!declared_content_length_over_cap(
            &[("content-length".into(), "16".into())],
            16
        ));
        assert!(declared_content_length_over_cap(
            &[("Content-Length".into(), "17".into())],
            16
        ));
    }

    struct FlagSession {
        called: std::cell::Cell<bool>,
    }

    #[async_trait::async_trait(?Send)]
    impl GuestDatabase for FlagSession {
        async fn execute(
            &self,
            _request: bookclerk_plugin_abi::ExecuteRequest,
        ) -> bookclerk_plugin_abi::Result<bookclerk_plugin_abi::ExecuteReply> {
            self.called.set(true);
            Err(PluginError::internal("execute must not run"))
        }
    }

    fn guest_sql_bytes(
        sql: &str,
        parameters: Vec<bookclerk_plugin_abi::DbValue>,
        kind: bookclerk_plugin_abi::DbPlanStatementKind,
        result_selection: bookclerk_plugin_abi::DbResultSelection,
        max_rows: u32,
    ) -> Vec<u8> {
        use bookclerk_plugin_abi::{
            encoded_execute_request_bytes, ExecuteRequest, TypedDbStatement,
        };
        encoded_execute_request_bytes(&ExecuteRequest {
            operation_id: "op-policy".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters,
                kind,
                max_rows,
                result_selection,
            }],
            deadline_unix_ms: 0,
        })
        .expect("encode")
    }

    fn grant_flag_session() -> (GrantedTable, Rc<FlagSession>) {
        grant_flag_session_with_policy(GuestSqlPolicy::allow_tables(["books"]))
    }

    fn grant_flag_session_with_policy(policy: GuestSqlPolicy) -> (GrantedTable, Rc<FlagSession>) {
        let session = Rc::new(FlagSession {
            called: std::cell::Cell::new(false),
        });
        let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
        table.borrow_mut().insert(
            "g-db".into(),
            GrantedSlot {
                input: None,
                output: None,
                progress: None,
                expires: Instant::now() + Duration::from_secs(60),
                allow_open: true,
                allow_put: true,
                allow_progress: true,
                database: Some(session.clone()),
                allow_database: true,
                sql_policy: policy,
                max_request_bytes: 0,
            },
        );
        (table, session)
    }

    async fn assert_granted_policy_rejects(bytes: Vec<u8>, needle: &str) {
        use bookclerk_plugin_abi::decode_execute_result_reply_bytes;
        let (table, session) = grant_flag_session();
        let payload = dispatch_execute(&table, "g-db".into(), bytes)
            .await
            .expect("policy rejection is a Cap'n err reply");
        let err = decode_execute_result_reply_bytes(&payload).expect_err("must fail closed");
        assert!(err.to_string().contains(needle), "{err}");
        assert!(
            !session.called.get(),
            "execute must not run for unauthorized SQL"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn guest_sql_policy_never_dispatches_execute() {
        use bookclerk_plugin_abi::{DbPlanStatementKind, DbResultSelection, DbValue};
        assert_granted_policy_rejects(
            guest_sql_bytes(
                "DROP TABLE books",
                vec![],
                DbPlanStatementKind::Execute,
                DbResultSelection::AffectedRows,
                0,
            ),
            "disallowed",
        )
        .await;
        assert_granted_policy_rejects(
            guest_sql_bytes(
                "SELECT token FROM encrypted_secrets",
                vec![],
                DbPlanStatementKind::Select,
                DbResultSelection::Rows,
                0,
            ),
            "unauthorized",
        )
        .await;
        assert_granted_policy_rejects(
            guest_sql_bytes(
                "SELECT ? FROM books WHERE id = ?",
                vec![DbValue::Int64(1)],
                DbPlanStatementKind::Select,
                DbResultSelection::Rows,
                1,
            ),
            "placeholder",
        )
        .await;
        assert_granted_policy_rejects(
            guest_sql_bytes(
                "SELECT id FROM jobs",
                vec![],
                DbPlanStatementKind::Select,
                DbResultSelection::Rows,
                1,
            ),
            "unauthorized table",
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_authoritative_policy_defers_table_scope_to_session() {
        use bookclerk_plugin_abi::{
            decode_execute_result_reply_bytes, DbPlanStatementKind, DbResultSelection,
        };
        let (table, session) = grant_flag_session_with_policy(GuestSqlPolicy::host_authoritative());
        let payload = dispatch_execute(
            &table,
            "g-db".into(),
            guest_sql_bytes(
                "SELECT id FROM jobs",
                vec![],
                DbPlanStatementKind::Select,
                DbResultSelection::Rows,
                1,
            ),
        )
        .await
        .expect("broker defers table scope to the host session");
        decode_execute_result_reply_bytes(&payload)
            .expect_err("FlagSession still returns an error");
        assert!(
            session.called.get(),
            "host-authoritative grants must dispatch execute"
        );
        let (table, session) = grant_flag_session_with_policy(GuestSqlPolicy::host_authoritative());
        let payload = dispatch_execute(
            &table,
            "g-db".into(),
            guest_sql_bytes(
                "DROP TABLE books",
                vec![],
                DbPlanStatementKind::Execute,
                DbResultSelection::AffectedRows,
                0,
            ),
        )
        .await
        .expect("grammar rejection is a Cap'n err reply");
        let err = decode_execute_result_reply_bytes(&payload).expect_err("DDL must fail closed");
        assert!(err.to_string().contains("disallowed"), "{err}");
        assert!(
            !session.called.get(),
            "DDL must not dispatch even when the broker defers table scope"
        );
    }

    #[tokio::test]
    async fn over_cap_content_length_never_dispatches_execute() {
        let (cmds_tx, mut cmds_rx) = mpsc::channel(8);
        let dispatched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&dispatched);
        tokio::spawn(async move {
            while let Some(cmd) = cmds_rx.recv().await {
                match cmd {
                    GrantedCmd::AtomicBudget { resp, .. } => {
                        let _ = resp.send(Ok(16));
                    }
                    GrantedCmd::Execute { resp, .. } => {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = resp.send(Err("must not dispatch".into()));
                    }
                    _ => {}
                }
            }
        });
        let (client, server) = tokio::io::duplex(4096);
        let serve = tokio::spawn(async move { handle_conn(server, "bridge", cmds_tx).await });
        let body = "x".repeat(32);
        let req = format!(
            "POST /db/execute HTTP/1.1\r\nAuthorization: Bearer grant-1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (mut reader, mut writer) = tokio::io::split(client);
        writer.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 256];
        let n = tokio::time::timeout(Duration::from_secs(2), reader.read(&mut buf))
            .await
            .expect("http response")
            .unwrap();
        let head = String::from_utf8_lossy(&buf[..n]);
        assert!(head.contains("413"), "{head}");
        drop(writer);
        let _ = serve.await;
        assert!(
            !dispatched.load(std::sync::atomic::Ordering::SeqCst),
            "execute must not be dispatched for an over-cap Content-Length"
        );
    }

    #[tokio::test]
    async fn chunked_execute_is_rejected_before_dispatch() {
        let (cmds_tx, mut cmds_rx) = mpsc::channel(8);
        let dispatched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&dispatched);
        tokio::spawn(async move {
            while let Some(cmd) = cmds_rx.recv().await {
                match cmd {
                    GrantedCmd::AtomicBudget { resp, .. } => {
                        let _ = resp.send(Ok(4096));
                    }
                    GrantedCmd::Execute { resp, .. } => {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = resp.send(Err("must not dispatch".into()));
                    }
                    _ => {}
                }
            }
        });
        let (client, server) = tokio::io::duplex(4096);
        let serve = tokio::spawn(async move { handle_conn(server, "bridge", cmds_tx).await });
        // Real HTTP chunk framing. Concatenating these bytes as a Cap'n payload
        // would be accepted by a decoder that never dechunks.
        let req = "POST /db/execute HTTP/1.1\r\n\
Authorization: Bearer grant-1\r\n\
Transfer-Encoding: chunked\r\n\
\r\n\
5\r\nhello\r\n0\r\n\r\n";
        let (mut reader, mut writer) = tokio::io::split(client);
        writer.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 256];
        let n = tokio::time::timeout(Duration::from_secs(2), reader.read(&mut buf))
            .await
            .expect("http response")
            .unwrap();
        let head = String::from_utf8_lossy(&buf[..n]);
        assert!(head.contains("400"), "{head}");
        drop(writer);
        let _ = serve.await;
        assert!(
            !dispatched.load(std::sync::atomic::Ordering::SeqCst),
            "execute must not be dispatched for a chunked body"
        );
    }
}
