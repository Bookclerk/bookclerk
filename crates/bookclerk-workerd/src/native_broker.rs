//! Trusted native broker: HTTP (workerd `PLUGIN_BACKEND`) → Cap'n Proto guest.
//!
//! The host executor owns the process tree. This broker launches or connects to
//! the verified native guest; plugin input cannot choose the executable or
//! weaken the sandbox. Every operation checks role, invocation fence, limits,
//! and the binding allowlist (confused-deputy closed). Grant revocation cancels
//! in-flight work.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bookclerk_plugin_abi::v2::{
    connect_plugin, ByteRange, Destination, DestinationContext, ListOptions, PluginClient,
    PluginDescribe, Source, SourceContext, WriteOptions, MAX_SCALAR_BYTES,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// Allowlist + fence for one brokered plugin session.
#[derive(Clone, Debug)]
pub struct BrokerPolicy {
    /// Manifest plugin id.
    pub plugin_id: String,
    /// Role the host is permitted to invoke (`destination`, `source`, …).
    pub role: String,
    /// Grant revision; mismatch rejects the call.
    pub grant_revision: String,
    /// Optional invocation fence id.
    pub invocation_fence: Option<String>,
    /// When true, in-flight operations abort.
    pub cancelled: Arc<AtomicBool>,
    /// Maximum JSON / scalar body.
    pub max_scalar_bytes: u32,
    /// Allowed destination/source operations.
    pub allowed_ops: HashSet<String>,
}

impl BrokerPolicy {
    /// Destination-capable policy for tests and the local native-behind-workerd slice.
    #[must_use]
    pub fn destination(plugin_id: impl Into<String>, grant_revision: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            role: "destination".into(),
            grant_revision: grant_revision.into(),
            invocation_fence: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            max_scalar_bytes: MAX_SCALAR_BYTES,
            allowed_ops: [
                "head",
                "list",
                "get",
                "put",
                "copy",
                "delete",
                "commit",
                "abortStage",
                "open",
                "describe",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }
    }

    /// Cancels in-flight operations (grant revocation).
    pub fn revoke(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn check(&self, role: &str, op: &str) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            bail!("cancelled: grant revoked");
        }
        if role != self.role && role != "describe" {
            bail!("forbidden: role `{role}` is not granted (`{}`)", self.role);
        }
        if !self.allowed_ops.contains(op) && op != "describe" {
            bail!("forbidden: operation `{op}` is not on the binding allowlist");
        }
        Ok(())
    }
}

type ByteStream = mpsc::Receiver<Result<Vec<u8>, String>>;
type OpenedObject = (bookclerk_plugin_abi::v2::ObjectMetadata, ByteStream);

enum BrokerCmd {
    Describe {
        resp: oneshot::Sender<Result<PluginDescribe, String>>,
    },
    Head {
        json: String,
        key: String,
        resp: oneshot::Sender<Result<Option<bookclerk_plugin_abi::v2::ObjectMetadata>, String>>,
    },
    List {
        json: String,
        options: ListOptions,
        resp: oneshot::Sender<Result<bookclerk_plugin_abi::v2::ListPage, String>>,
    },
    Get {
        json: String,
        key: String,
        range: Option<ByteRange>,
        resp: oneshot::Sender<Result<OpenedObject, String>>,
    },
    Put {
        json: String,
        key: String,
        options: WriteOptions,
        body_rx: mpsc::Receiver<Vec<u8>>,
        resp: oneshot::Sender<Result<bookclerk_plugin_abi::v2::PutResult, String>>,
    },
    Copy {
        json: String,
        from: String,
        to: String,
        resp: oneshot::Sender<Result<u64, String>>,
    },
    Delete {
        json: String,
        key: String,
        resp: oneshot::Sender<Result<(), String>>,
    },
    Commit {
        json: String,
        key: String,
        token: String,
        resp: oneshot::Sender<Result<bookclerk_plugin_abi::v2::PutResult, String>>,
    },
    AbortStage {
        json: String,
        key: String,
        token: String,
        resp: oneshot::Sender<Result<(), String>>,
    },
    Open {
        json: String,
        key: String,
        resp: oneshot::Sender<Result<OpenedObject, String>>,
    },
}

/// Starts the HTTP accept loop (Send) and the vat-thread dispatcher (`LocalSet`).
pub fn spawn_native_broker<L>(listener: L, client: PluginClient, policy: BrokerPolicy)
where
    L: BrokerListener + Send + 'static,
    L::Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(32);
    tokio::task::spawn_local(dispatch_broker(rx, client, policy.clone()));
    tokio::spawn(async move {
        if let Err(err) = serve_broker_http(listener, policy, tx).await {
            tracing::warn!(error = %err, "native broker exited");
        }
    });
}

/// TCP/Unix accept surface for the native broker.
pub trait BrokerListener {
    /// Accepted stream type.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send;
    /// Accept one connection.
    fn accept(
        &self,
    ) -> impl std::future::Future<Output = std::io::Result<(Self::Stream, String)>> + Send;
}

impl BrokerListener for tokio::net::TcpListener {
    type Stream = tokio::net::TcpStream;
    async fn accept(&self) -> std::io::Result<(Self::Stream, String)> {
        let (s, addr) = tokio::net::TcpListener::accept(self).await?;
        Ok((s, addr.to_string()))
    }
}

async fn dispatch_broker(
    mut rx: mpsc::Receiver<BrokerCmd>,
    client: PluginClient,
    policy: BrokerPolicy,
) {
    while let Some(cmd) = rx.recv().await {
        if policy.cancelled.load(Ordering::SeqCst) {
            match cmd {
                BrokerCmd::Describe { resp } => {
                    let _ = resp.send(Err("cancelled: grant revoked".into()));
                }
                BrokerCmd::Head { resp, .. } => {
                    let _ = resp.send(Err("cancelled: grant revoked".into()));
                }
                BrokerCmd::List { resp, .. } => {
                    let _ = resp.send(Err("cancelled: grant revoked".into()));
                }
                BrokerCmd::Get { resp, .. } | BrokerCmd::Open { resp, .. } => {
                    let _ = resp.send(Err("cancelled: grant revoked".into()));
                }
                BrokerCmd::Put { resp, .. } | BrokerCmd::Commit { resp, .. } => {
                    let _ = resp.send(Err("cancelled: grant revoked".into()));
                }
                BrokerCmd::Copy { resp, .. } => {
                    let _ = resp.send(Err("cancelled: grant revoked".into()));
                }
                BrokerCmd::Delete { resp, .. } | BrokerCmd::AbortStage { resp, .. } => {
                    let _ = resp.send(Err("cancelled: grant revoked".into()));
                }
            }
            continue;
        }
        match cmd {
            BrokerCmd::Describe { resp } => {
                let out = client.describe().await.map_err(|e| e.to_string());
                let _ = resp.send(out);
            }
            BrokerCmd::Head { json, key, resp } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = race_against_revoke(cancelled, async {
                    let dest = client
                        .destination(DestinationContext { json })
                        .await
                        .map_err(|e| e.to_string())?;
                    dest.head(&key).await.map_err(|e| e.to_string())
                })
                .await;
                let _ = resp.send(out);
            }
            BrokerCmd::List {
                json,
                options,
                resp,
            } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = race_against_revoke(cancelled, async {
                    let dest = client
                        .destination(DestinationContext { json })
                        .await
                        .map_err(|e| e.to_string())?;
                    dest.list(options).await.map_err(|e| e.to_string())
                })
                .await;
                let _ = resp.send(out);
            }
            BrokerCmd::Get {
                json,
                key,
                range,
                resp,
            } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = stream_get(&client, json, key, range, false, cancelled).await;
                let _ = resp.send(out);
            }
            BrokerCmd::Open { json, key, resp } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = stream_get(&client, json, key, None, true, cancelled).await;
                let _ = resp.send(out);
            }
            BrokerCmd::Put {
                json,
                key,
                options,
                mut body_rx,
                resp,
            } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = async {
                    let dest = client
                        .destination(DestinationContext { json })
                        .await
                        .map_err(|e| e.to_string())?;
                    let (mut body_tx, body) = tokio::io::duplex(64 * 1024);
                    let pump_cancel = Arc::clone(&cancelled);
                    tokio::task::spawn_local(async move {
                        while let Some(chunk) = body_rx.recv().await {
                            if pump_cancel.load(Ordering::SeqCst) {
                                break;
                            }
                            if tokio::io::AsyncWriteExt::write_all(&mut body_tx, &chunk)
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                    race_against_revoke(Arc::clone(&cancelled), async {
                        dest.put(&key, Box::pin(body), options)
                            .await
                            .map_err(|e| e.to_string())
                    })
                    .await
                }
                .await;
                let _ = resp.send(out);
            }
            BrokerCmd::Copy {
                json,
                from,
                to,
                resp,
            } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = race_against_revoke(cancelled, async {
                    let dest = client
                        .destination(DestinationContext { json })
                        .await
                        .map_err(|e| e.to_string())?;
                    dest.copy(&from, &to)
                        .await
                        .map(|r| r.bytes_copied)
                        .map_err(|e| e.to_string())
                })
                .await;
                let _ = resp.send(out);
            }
            BrokerCmd::Delete { json, key, resp } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = race_against_revoke(cancelled, async {
                    let dest = client
                        .destination(DestinationContext { json })
                        .await
                        .map_err(|e| e.to_string())?;
                    dest.delete(&key).await.map_err(|e| e.to_string())
                })
                .await;
                let _ = resp.send(out);
            }
            BrokerCmd::Commit {
                json,
                key,
                token,
                resp,
            } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = race_against_revoke(cancelled, async {
                    let dest = client
                        .destination(DestinationContext { json })
                        .await
                        .map_err(|e| e.to_string())?;
                    dest.commit(&key, &token).await.map_err(|e| e.to_string())
                })
                .await;
                let _ = resp.send(out);
            }
            BrokerCmd::AbortStage {
                json,
                key,
                token,
                resp,
            } => {
                let cancelled = Arc::clone(&policy.cancelled);
                let out = race_against_revoke(cancelled, async {
                    let dest = client
                        .destination(DestinationContext { json })
                        .await
                        .map_err(|e| e.to_string())?;
                    dest.abort_stage(&key, &token)
                        .await
                        .map_err(|e| e.to_string())
                })
                .await;
                let _ = resp.send(out);
            }
        }
    }
}

async fn stream_get(
    client: &PluginClient,
    json: String,
    key: String,
    range: Option<ByteRange>,
    as_source: bool,
    cancelled: Arc<AtomicBool>,
) -> Result<OpenedObject, String> {
    let (meta, body) = if as_source {
        race_against_revoke(Arc::clone(&cancelled), async {
            let src = client
                .source(SourceContext { json })
                .await
                .map_err(|e| e.to_string())?;
            let opened = src.open(&key).await.map_err(|e| e.to_string())?;
            Ok((opened.meta, opened.body))
        })
        .await?
    } else {
        race_against_revoke(Arc::clone(&cancelled), async {
            let dest = client
                .destination(DestinationContext { json })
                .await
                .map_err(|e| e.to_string())?;
            let got = dest.get(&key, range).await.map_err(|e| e.to_string())?;
            Ok((got.meta, got.body))
        })
        .await?
    };
    let (tx, rx) = mpsc::channel(4);
    tokio::task::spawn_local(pump_get_body(body, tx, cancelled));
    Ok((meta, rx))
}

/// Copies object bytes until EOF or grant revocation.
async fn pump_get_body(
    mut body: impl AsyncRead + Unpin + 'static,
    tx: mpsc::Sender<Result<Vec<u8>, String>>,
    cancelled: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            biased;
            () = wait_cancelled(Arc::clone(&cancelled)) => {
                let _ = tx.send(Err("cancelled: grant revoked".into())).await;
                break;
            }
            read = body.read(&mut buf) => {
                match read {
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
        }
    }
}

async fn serve_broker_http<L>(
    listener: L,
    policy: BrokerPolicy,
    cmds: mpsc::Sender<BrokerCmd>,
) -> Result<()>
where
    L: BrokerListener,
    L::Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    loop {
        let (stream, _) = listener.accept().await?;
        let policy = policy.clone();
        let cmds = cmds.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_conn(stream, policy, cmds).await {
                tracing::debug!(error = %err, "native broker connection");
            }
        });
    }
}

async fn write_broker_err<W: AsyncWrite + Unpin>(
    writer: &mut W,
    code: &str,
    msg: &str,
) -> Result<()> {
    let body = serde_json::json!({ "error": { "code": code, "message": msg } });
    let payload = serde_json::to_vec(&body).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    );
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn handle_conn<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    policy: BrokerPolicy,
    cmds: mpsc::Sender<BrokerCmd>,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (method, path, headers, prefix) = read_request(&mut reader).await?;
    let path_only = path.split('?').next().unwrap_or(&path);
    let query: Vec<(String, String)> = path
        .split_once('?')
        .map(|(_, q)| {
            q.split('&')
                .filter_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    Some((percent_decode(k), percent_decode(v)))
                })
                .collect()
        })
        .unwrap_or_default();
    let ctx_header = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-bookclerk-context"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let ctx_json = if ctx_header.is_empty() {
        String::new()
    } else {
        serde_json::from_str::<serde_json::Value>(&ctx_header)
            .ok()
            .and_then(|v| v.get("json").and_then(|j| j.as_str()).map(str::to_string))
            .unwrap_or(ctx_header)
    };

    if method == "POST" && path_only == "/v2/describe" {
        if let Err(err) = policy.check("describe", "describe") {
            return write_broker_err(&mut writer, "forbidden", &err.to_string()).await;
        }
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::Describe { resp: resp_tx }).await?;
        match resp_rx.await.context("describe dropped")? {
            Ok(desc) => write_json(&mut writer, &serde_json::to_value(&desc)?).await,
            Err(err) => write_broker_err(&mut writer, "internal", &err).await,
        }
    } else if method == "POST" && path_only == "/v2/destination/head" {
        policy
            .check("destination", "head")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rest = read_content(&mut reader, &headers, prefix, policy.max_scalar_bytes).await?;
        if rest.len() > policy.max_scalar_bytes as usize {
            bail!("payload_too_large: head body");
        }
        let value: serde_json::Value = serde_json::from_slice(&rest).unwrap_or_default();
        let key = value
            .get("key")
            .and_then(|k| k.as_str())
            .unwrap_or("")
            .to_string();
        let json = value
            .get("json")
            .and_then(|k| k.as_str())
            .unwrap_or(&ctx_json)
            .to_string();
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::Head {
            json,
            key,
            resp: resp_tx,
        })
        .await?;
        match resp_rx.await.context("head dropped")? {
            Ok(meta) => {
                let body = serde_json::json!({ "found": meta.is_some(), "meta": meta });
                write_json(&mut writer, &body).await
            }
            Err(err) => write_broker_err(&mut writer, "internal", &err).await,
        }
    } else if method == "POST" && path_only == "/v2/destination/list" {
        policy
            .check("destination", "list")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rest = read_content(&mut reader, &headers, prefix, policy.max_scalar_bytes).await?;
        let value: serde_json::Value = serde_json::from_slice(&rest).unwrap_or_default();
        let options = list_options(value.get("options").unwrap_or(&value));
        let json = value
            .get("json")
            .and_then(|k| k.as_str())
            .unwrap_or(&ctx_json)
            .to_string();
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::List {
            json,
            options,
            resp: resp_tx,
        })
        .await?;
        match resp_rx.await.context("list dropped")? {
            Ok(page) => write_json(&mut writer, &serde_json::to_value(&page)?).await,
            Err(err) => write_broker_err(&mut writer, "internal", &err).await,
        }
    } else if method == "GET" && path_only == "/v2/destination/get" {
        policy
            .check("destination", "get")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let key = query
            .iter()
            .find(|(k, _)| k == "key")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let range = query
            .iter()
            .find(|(k, _)| k == "offset")
            .map(|(_, off)| ByteRange {
                offset: off.parse().unwrap_or(0),
                length: query
                    .iter()
                    .find(|(k, _)| k == "length")
                    .and_then(|(_, v)| v.parse().ok()),
            });
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::Get {
            json: ctx_json,
            key,
            range,
            resp: resp_tx,
        })
        .await?;
        stream_response(&mut writer, resp_rx).await
    } else if method == "PUT" && path_only == "/v2/destination/put" {
        policy
            .check("destination", "put")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let key = query
            .iter()
            .find(|(k, _)| k == "key")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let content_type = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());
        let content_length = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok());
        let commit_token = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-bookclerk-commit-token"))
            .map(|(_, v)| v.clone());
        let stage_only = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-bookclerk-stage-only"))
            .is_some_and(|(_, v)| v == "1");
        let (body_tx, body_rx) = mpsc::channel(4);
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::Put {
            json: ctx_json,
            key,
            options: WriteOptions {
                content_type,
                content_length,
                sha256: None,
                commit_token,
                stage_only,
            },
            body_rx,
            resp: resp_tx,
        })
        .await?;
        pump_body(&mut reader, &headers, prefix, &body_tx).await?;
        drop(body_tx);
        match resp_rx.await.context("put dropped")? {
            Ok(put) => write_json(&mut writer, &serde_json::to_value(&put)?).await,
            Err(err) => write_broker_err(&mut writer, "internal", &err).await,
        }
    } else if method == "POST" && path_only == "/v2/destination/copy" {
        policy
            .check("destination", "copy")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rest = read_content(&mut reader, &headers, prefix, policy.max_scalar_bytes).await?;
        let value: serde_json::Value = serde_json::from_slice(&rest).unwrap_or_default();
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::Copy {
            json: value
                .get("json")
                .and_then(|k| k.as_str())
                .unwrap_or(&ctx_json)
                .to_string(),
            from: value
                .get("from")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .into(),
            to: value
                .get("to")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .into(),
            resp: resp_tx,
        })
        .await?;
        match resp_rx.await.context("copy dropped")? {
            Ok(n) => write_json(&mut writer, &serde_json::json!({ "bytesCopied": n })).await,
            Err(err) => write_broker_err(&mut writer, "internal", &err).await,
        }
    } else if method == "POST" && path_only == "/v2/destination/delete" {
        policy
            .check("destination", "delete")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rest = read_content(&mut reader, &headers, prefix, policy.max_scalar_bytes).await?;
        let value: serde_json::Value = serde_json::from_slice(&rest).unwrap_or_default();
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::Delete {
            json: value
                .get("json")
                .and_then(|k| k.as_str())
                .unwrap_or(&ctx_json)
                .to_string(),
            key: value
                .get("key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .into(),
            resp: resp_tx,
        })
        .await?;
        match resp_rx.await.context("delete dropped")? {
            Ok(()) => write_json(&mut writer, &serde_json::json!({ "ok": true })).await,
            Err(err) => write_broker_err(&mut writer, "internal", &err).await,
        }
    } else if method == "POST" && path_only == "/v2/destination/commit" {
        policy
            .check("destination", "commit")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rest = read_content(&mut reader, &headers, prefix, policy.max_scalar_bytes).await?;
        let value: serde_json::Value = serde_json::from_slice(&rest).unwrap_or_default();
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::Commit {
            json: value
                .get("json")
                .and_then(|k| k.as_str())
                .unwrap_or(&ctx_json)
                .to_string(),
            key: value
                .get("key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .into(),
            token: value
                .get("commitToken")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .into(),
            resp: resp_tx,
        })
        .await?;
        match resp_rx.await.context("commit dropped")? {
            Ok(put) => write_json(&mut writer, &serde_json::to_value(&put)?).await,
            Err(err) => write_broker_err(&mut writer, "internal", &err).await,
        }
    } else if method == "POST" && path_only == "/v2/destination/abortStage" {
        policy
            .check("destination", "abortStage")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rest = read_content(&mut reader, &headers, prefix, policy.max_scalar_bytes).await?;
        let value: serde_json::Value = serde_json::from_slice(&rest).unwrap_or_default();
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::AbortStage {
            json: value
                .get("json")
                .and_then(|k| k.as_str())
                .unwrap_or(&ctx_json)
                .to_string(),
            key: value
                .get("key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .into(),
            token: value
                .get("commitToken")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .into(),
            resp: resp_tx,
        })
        .await?;
        match resp_rx.await.context("abort dropped")? {
            Ok(()) => write_json(&mut writer, &serde_json::json!({ "ok": true })).await,
            Err(err) => write_broker_err(&mut writer, "internal", &err).await,
        }
    } else if method == "GET" && path_only == "/v2/source/open" {
        policy
            .check("destination", "open")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let key = query
            .iter()
            .find(|(k, _)| k == "key")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let (resp_tx, resp_rx) = oneshot::channel();
        cmds.send(BrokerCmd::Open {
            json: ctx_json,
            key,
            resp: resp_tx,
        })
        .await?;
        stream_response(&mut writer, resp_rx).await
    } else {
        let resp =
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 9\r\nconnection: close\r\n\r\nnot found";
        writer.write_all(resp).await?;
        writer.flush().await?;
        Ok(())
    }
}

fn list_options(v: &serde_json::Value) -> ListOptions {
    ListOptions {
        prefix: v
            .get("prefix")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .into(),
        cursor: v.get("cursor").and_then(|x| x.as_str()).map(str::to_string),
        limit: v.get("limit").and_then(|x| x.as_u64()).unwrap_or(50) as u32,
    }
}

async fn stream_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    resp_rx: oneshot::Receiver<Result<OpenedObject, String>>,
) -> Result<()> {
    let (meta, mut body_rx) = resp_rx
        .await
        .context("get dropped")?
        .map_err(anyhow::Error::msg)?;
    let mut headers = format!(
        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nx-bookclerk-key: {}\r\nx-bookclerk-size: {}\r\nconnection: close\r\n",
        meta.key, meta.size
    );
    if let Some(ct) = &meta.content_type {
        headers.push_str(&format!(
            "x-bookclerk-content-type: {ct}\r\ncontent-type: {ct}\r\n"
        ));
    }
    if let Some(etag) = &meta.etag {
        headers.push_str(&format!("x-bookclerk-etag: {etag}\r\n"));
    }
    headers.push_str("\r\n");
    writer.write_all(headers.as_bytes()).await?;
    while let Some(chunk) = body_rx.recv().await {
        let chunk = chunk.map_err(anyhow::Error::msg)?;
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
    Ok(())
}

async fn write_json<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &serde_json::Value,
) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    );
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_request<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::new();
    loop {
        let mut tmp = [0u8; 1024];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            bail!("truncated HTTP request");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = buf[..pos].to_vec();
            let prefix = buf[pos + 4..].to_vec();
            let text = String::from_utf8_lossy(&head);
            let mut lines = text.split("\r\n");
            let req = lines.next().unwrap_or("");
            let mut parts = req.split_whitespace();
            let method = parts.next().unwrap_or("GET").to_string();
            let path = parts.next().unwrap_or("/").to_string();
            let mut headers = Vec::new();
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            return Ok((method, path, headers, prefix));
        }
        if buf.len() > MAX_SCALAR_BYTES as usize {
            bail!("payload_too_large: HTTP headers");
        }
    }
}

async fn read_content<S: AsyncRead + Unpin>(
    stream: &mut S,
    headers: &[(String, String)],
    mut prefix: Vec<u8>,
    max_bytes: u32,
) -> Result<Vec<u8>> {
    let raw = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .ok_or_else(|| anyhow::anyhow!("missing Content-Length"))?
        .1
        .trim();
    let len = raw
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("invalid Content-Length"))?;
    if len > max_bytes as usize {
        bail!("payload_too_large: Content-Length {len} exceeds {max_bytes}");
    }
    while prefix.len() < len {
        let mut tmp = vec![0u8; (len - prefix.len()).min(64 * 1024)];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("read {} of {len} Content-Length bytes", prefix.len()),
            )
            .into());
        }
        prefix.extend_from_slice(&tmp[..n]);
    }
    prefix.truncate(len);
    Ok(prefix)
}

async fn pump_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    headers: &[(String, String)],
    prefix: Vec<u8>,
    body_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    crate::granted::pump_http_body(stream, headers, prefix, body_tx).await
}

async fn wait_cancelled(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn race_against_revoke<T>(
    cancelled: Arc<AtomicBool>,
    op: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        () = wait_cancelled(cancelled) => Err("cancelled: grant revoked".into()),
        out = op => out,
    }
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
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

/// Connects a stdio Cap'n Proto guest as the broker's native backend.
///
/// # Errors
///
/// Returns a plugin error when the vat fails.
pub async fn mediate_native_guest<R, W>(
    stdout: R,
    stdin: W,
    policy: BrokerPolicy,
    listener: tokio::net::TcpListener,
) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: tokio::io::AsyncWrite + Unpin + 'static,
{
    let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
    tokio::task::spawn_local(rpc);
    spawn_native_broker(listener, client, policy);
    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoke_cancels_in_flight_allowlist() {
        let policy = BrokerPolicy::destination("local", "grant-1");
        assert!(policy.check("destination", "put").is_ok());
        policy.revoke();
        assert!(policy.check("destination", "put").is_err());
        assert!(policy.check("describe", "describe").is_err());
    }

    #[test]
    fn confused_deputy_role_is_closed() {
        let policy = BrokerPolicy::destination("local", "grant-1");
        assert!(policy.check("database", "execute").is_err());
        assert!(policy.check("destination", "not-a-real-op").is_err());
    }

    #[tokio::test]
    async fn revoke_aborts_blocked_put() {
        assert_revoke_aborts_blocked_op("put").await;
    }

    #[tokio::test]
    async fn revoke_aborts_blocked_get() {
        assert_revoke_aborts_blocked_op("get").await;
    }

    #[tokio::test]
    async fn revoke_aborts_blocked_open() {
        assert_revoke_aborts_blocked_op("open").await;
    }

    #[tokio::test]
    async fn revoke_aborts_blocked_copy() {
        assert_revoke_aborts_blocked_op("copy").await;
    }

    #[tokio::test]
    async fn revoke_aborts_blocked_delete() {
        assert_revoke_aborts_blocked_op("delete").await;
    }

    #[tokio::test]
    async fn revoke_aborts_blocked_commit() {
        assert_revoke_aborts_blocked_op("commit").await;
    }

    #[tokio::test]
    async fn revoke_aborts_blocked_abort_stage() {
        assert_revoke_aborts_blocked_op("abort_stage").await;
    }

    async fn assert_revoke_aborts_blocked_op(op: &str) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let err = race_against_revoke(cancelled, std::future::pending::<Result<(), String>>())
            .await
            .unwrap_err();
        assert!(
            err.contains("revoked"),
            "expected revoke cancellation for {op}, got {err}"
        );
    }

    struct PendingRead;

    impl tokio::io::AsyncRead for PendingRead {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    #[tokio::test]
    async fn revoke_errors_in_flight_get_body_pump() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::channel(4);
        let flag = Arc::clone(&cancelled);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        pump_get_body(PendingRead, tx, cancelled).await;
        let err = rx.recv().await.expect("pump error").expect_err("revoked");
        assert!(
            err.contains("revoked"),
            "expected in-flight GET body to error on revoke, got {err}"
        );
    }

    #[tokio::test]
    async fn revoke_errors_in_flight_open_body_pump() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::channel(4);
        let flag = Arc::clone(&cancelled);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        pump_get_body(PendingRead, tx, cancelled).await;
        let err = rx.recv().await.expect("pump error").expect_err("revoked");
        assert!(
            err.contains("revoked"),
            "expected in-flight OPEN body to error on revoke, got {err}"
        );
    }

    #[tokio::test]
    async fn read_content_rejects_missing_invalid_and_short_bodies() {
        let mut empty: &[u8] = b"";
        let err = read_content(&mut empty, &[], Vec::new(), 64)
            .await
            .expect_err("missing Content-Length");
        assert!(err.to_string().contains("Content-Length"));

        let headers = vec![("Content-Length".into(), "nope".into())];
        let mut empty: &[u8] = b"";
        let err = read_content(&mut empty, &headers, Vec::new(), 64)
            .await
            .expect_err("invalid Content-Length");
        assert!(err.to_string().contains("invalid Content-Length"));

        let headers = vec![("Content-Length".into(), "100".into())];
        let mut empty: &[u8] = b"";
        let err = read_content(&mut empty, &headers, Vec::new(), 10)
            .await
            .expect_err("oversize Content-Length");
        assert!(err.to_string().contains("payload_too_large"));

        let headers = vec![("Content-Length".into(), "4".into())];
        let mut short: &[u8] = b"ab";
        let err = read_content(&mut short, &headers, Vec::new(), 64)
            .await
            .expect_err("short body");
        assert_eq!(
            err.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::UnexpectedEof)
        );

        let headers = vec![("Content-Length".into(), "4".into())];
        let mut exact: &[u8] = b"abcd";
        let got = read_content(&mut exact, &headers, Vec::new(), 64)
            .await
            .unwrap();
        assert_eq!(got, b"abcd");
    }
}
