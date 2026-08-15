//! Cap'n Proto two-party RPC adapters for ABI v2 role classes.
//!
//! Public types remain [`crate::v2::Destination`] / [`crate::v2::ByteRange`] streams.
//! Capability table indexes stay inside capnp-rpc. Method results are typed
//! success/error unions; SDKs map `err` onto [`PluginError`].

#![allow(clippy::missing_docs_in_private_items)]
#![allow(clippy::arc_with_non_send_sync)] // capnp stubs are `!Send`; vat is LocalSet.

use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::limits::{ScalarLimits, MAX_LIST_PAGE, MAX_STREAM_WINDOW_BYTES};
use super::plugin_v2_capnp::{
    bookclerk_plugin, byte_source, cancellation, content_source as content_source_capnp,
    content_source_reply, copy_reply, database as database_capnp, database_reply,
    database_session as database_session_capnp, describe_reply, destination as dest_iface,
    destination_reply, domain_event, empty_reply, event_result as event_result_capnp,
    event_result_reply, exec_reply, get_reply, handle_reply, head_reply, health_reply,
    integration as integration_capnp, integration_reply, job_handler, job_invocation, job_outcome,
    json_reply, list_reply, object_metadata, open_reply, plugin_describe, plugin_error,
    progress_sink, pull_reply, put_reply, query_reply, session_reply, source as source_capnp,
    source_reply, transaction as transaction_capnp, transaction_reply, worker_reply, write_options,
};
use super::roles::{
    ByteRange, Cancellation, ContentSource, ContentSourceContext, Database, DatabaseContext,
    DatabaseSession, Destination, Integration, IntegrationContext, JobHandler, JobHandlerContext,
    NeverCancel, PluginRoot, ProgressSink, ReadResult, Source, Transaction,
};
use super::types::{
    CopyResult, DestinationContext, DomainEvent, EventResult, ExecResult, HealthOk, JobCheckpoint,
    JobInvocation, JobOutcome, ListOptions, ListPage, ObjectInfo, ObjectMetadata, PluginDescribe,
    PutResult, QueryPage, SourceContext, Statement, WorkerContext, WriteOptions, ENVELOPE_VERSION,
    MAX_CHECKPOINT_BYTES,
};
use crate::{PluginError, Result};

fn from_capnp(err: impl std::fmt::Display) -> PluginError {
    PluginError::unavailable(err.to_string())
}

fn text_of(r: capnp::text::Reader<'_>) -> String {
    r.to_string().unwrap_or_default()
}

fn write_error(mut b: plugin_error::Builder<'_>, err: &PluginError) {
    b.set_code(err.wire_str());
    b.set_message(&err.message);
}

fn read_write_options(o: write_options::Reader<'_>) -> WriteOptions {
    WriteOptions {
        content_type: {
            let t = o.get_content_type().ok().map(text_of).unwrap_or_default();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        },
        content_length: {
            let n = o.get_content_length();
            if n == 0 {
                None
            } else {
                Some(n)
            }
        },
        sha256: o.get_sha256().ok().and_then(
            |d| {
                if d.is_empty() {
                    None
                } else {
                    Some(d.to_vec())
                }
            },
        ),
        commit_token: {
            let t = o.get_commit_token().ok().map(text_of).unwrap_or_default();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        },
        stage_only: o.get_stage_only(),
    }
}

fn fill_write_options(mut o: write_options::Builder<'_>, options: &WriteOptions) {
    if let Some(ct) = &options.content_type {
        o.set_content_type(ct);
    }
    if let Some(n) = options.content_length {
        o.set_content_length(n);
    }
    if let Some(sum) = &options.sha256 {
        o.set_sha256(sum);
    }
    if let Some(token) = &options.commit_token {
        o.set_commit_token(token);
    }
    o.set_stage_only(options.stage_only);
}

fn fill_put_result(mut out: super::plugin_v2_capnp::put_result::Builder<'_>, put: &PutResult) {
    out.set_key(&put.key);
    out.set_bytes_written(put.bytes_written);
    if let Some(etag) = &put.etag {
        out.set_etag(etag);
    }
    if let Some(sum) = &put.sha256 {
        out.set_sha256(sum);
    }
}

fn read_error(r: plugin_error::Reader<'_>) -> PluginError {
    let code = r.get_code().ok().map(text_of).unwrap_or_default();
    let message = r.get_message().ok().map(text_of).unwrap_or_default();
    PluginError::from_wire(&code, message)
}

fn fill_metadata(mut b: object_metadata::Builder<'_>, meta: &ObjectMetadata) {
    b.set_key(&meta.key);
    b.set_size(meta.size);
    if let Some(ct) = &meta.content_type {
        b.set_content_type(ct);
    }
    if let Some(etag) = &meta.etag {
        b.set_etag(etag);
    }
    if let Some(sum) = &meta.sha256 {
        b.set_sha256(sum);
    }
}

/// Decode object metadata from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns [`PluginError`] when a text or data field cannot be read.
fn read_metadata(r: object_metadata::Reader<'_>) -> Result<ObjectMetadata> {
    Ok(ObjectMetadata {
        key: text_of(r.get_key().map_err(from_capnp)?),
        size: r.get_size(),
        content_type: {
            let t = text_of(r.get_content_type().map_err(from_capnp)?);
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        },
        etag: {
            let t = text_of(r.get_etag().map_err(from_capnp)?);
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        },
        sha256: {
            let d = r.get_sha256().map_err(from_capnp)?;
            if d.is_empty() {
                None
            } else {
                Some(d.to_vec())
            }
        },
    })
}

/// Encode [`PluginDescribe`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto encoding error when a text or nested field cannot be
/// set.
fn fill_describe(mut b: plugin_describe::Builder<'_>, d: &PluginDescribe) -> capnp::Result<()> {
    b.set_api_version(d.api_version);
    b.set_id(&d.id);
    b.set_kind(&d.kind);
    if let Some(name) = &d.display_name {
        b.set_display_name(name);
    }
    {
        let mut feats = b.reborrow().init_rpc_features(d.rpc_features.len() as u32);
        for (i, f) in d.rpc_features.iter().enumerate() {
            feats.set(i as u32, f);
        }
    }
    let mut lim = b.reborrow().get_scalar_limits()?;
    lim.set_max_scalar_bytes(d.scalar_limits.max_scalar_bytes);
    lim.set_max_stream_window_bytes(d.scalar_limits.max_stream_window_bytes);
    lim.set_max_list_page(d.scalar_limits.max_list_page);
    b.set_abi_major(if d.abi_major == 0 {
        d.api_version
    } else {
        d.abi_major
    });
    b.set_abi_minor(d.abi_minor);
    {
        let mut roles = b
            .reborrow()
            .init_supported_roles(d.supported_roles.len() as u32);
        for (i, role) in d.supported_roles.iter().enumerate() {
            roles.set(i as u32, role);
        }
    }
    if !d.metadata_json.is_empty() {
        b.set_metadata_json(&d.metadata_json);
    }
    Ok(())
}

/// Encode a [`JobOutcome`] union onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto encoding error when a nested field cannot be set.
fn fill_job_outcome(b: job_outcome::Builder<'_>, outcome: &JobOutcome) -> capnp::Result<()> {
    match outcome {
        JobOutcome::Completed {
            message,
            bytes_copied,
        } => {
            let mut c = b.init_completed();
            c.set_message(message);
            c.set_bytes_copied(*bytes_copied);
        }
        JobOutcome::Retryable {
            message,
            retry_after_unix_ms,
        } => {
            let mut c = b.init_retryable();
            c.set_message(message);
            c.set_retry_after_unix_ms(retry_after_unix_ms.unwrap_or(0));
        }
        JobOutcome::Rejected { message } => {
            b.init_rejected().set_message(message);
        }
        JobOutcome::Cancelled { message } => {
            b.init_cancelled().set_message(message);
        }
        JobOutcome::Suspended {
            checkpoint,
            wake_at_unix_ms,
        } => {
            let mut c = b.init_suspended();
            c.set_checkpoint_json(&checkpoint.json);
            c.set_checkpoint_schema_version(checkpoint.schema_version);
            c.set_wake_at_unix_ms(*wake_at_unix_ms);
        }
    }
    Ok(())
}

/// Decode a [`JobOutcome`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns [`PluginError`] when the union is unknown or a nested field cannot
/// be read.
fn read_job_outcome(r: job_outcome::Reader<'_>) -> Result<JobOutcome> {
    match r.which().map_err(from_capnp)? {
        job_outcome::Completed(c) => {
            let c = c.map_err(from_capnp)?;
            Ok(JobOutcome::Completed {
                message: text_of(c.get_message().map_err(from_capnp)?),
                bytes_copied: c.get_bytes_copied(),
            })
        }
        job_outcome::Retryable(c) => {
            let c = c.map_err(from_capnp)?;
            let retry = c.get_retry_after_unix_ms();
            Ok(JobOutcome::Retryable {
                message: text_of(c.get_message().map_err(from_capnp)?),
                retry_after_unix_ms: if retry == 0 { None } else { Some(retry) },
            })
        }
        job_outcome::Rejected(c) => {
            let c = c.map_err(from_capnp)?;
            Ok(JobOutcome::Rejected {
                message: text_of(c.get_message().map_err(from_capnp)?),
            })
        }
        job_outcome::Cancelled(c) => {
            let c = c.map_err(from_capnp)?;
            Ok(JobOutcome::Cancelled {
                message: text_of(c.get_message().map_err(from_capnp)?),
            })
        }
        job_outcome::Suspended(c) => {
            let c = c.map_err(from_capnp)?;
            let json = text_of(c.get_checkpoint_json().map_err(from_capnp)?);
            if json.len() > MAX_CHECKPOINT_BYTES as usize {
                return Err(PluginError::payload_too_large(format!(
                    "checkpoint of {} bytes exceeds {MAX_CHECKPOINT_BYTES}",
                    json.len()
                )));
            }
            Ok(JobOutcome::Suspended {
                checkpoint: JobCheckpoint {
                    schema_version: c.get_checkpoint_schema_version(),
                    json,
                },
                wake_at_unix_ms: c.get_wake_at_unix_ms(),
            })
        }
    }
}

/// Encode a [`JobInvocation`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns a Cap'n Proto encoding error when a text or nested field cannot be
/// set.
fn fill_invocation(
    mut b: job_invocation::Builder<'_>,
    invocation: &JobInvocation,
) -> capnp::Result<()> {
    b.set_envelope_version(invocation.envelope_version);
    b.set_payload_schema_version(invocation.payload_schema_version);
    b.set_invocation_id(&invocation.invocation_id);
    b.set_command_type(&invocation.command_type);
    b.set_payload_json(&invocation.payload_json);
    b.set_idempotency_key(&invocation.idempotency_key);
    b.set_attempt(invocation.attempt);
    b.set_correlation_id(&invocation.correlation_id);
    if let Some(c) = &invocation.causation_id {
        b.set_causation_id(c);
    }
    b.set_deadline_unix_ms(invocation.deadline_unix_ms);
    if let Some(cp) = &invocation.checkpoint {
        b.set_checkpoint_json(&cp.json);
        b.set_checkpoint_schema_version(cp.schema_version);
    }
    b.set_invocation_sequence(invocation.invocation_sequence);
    if let Some(step) = &invocation.step_id {
        b.set_step_id(step);
    }
    Ok(())
}

/// Decode a [`JobInvocation`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns [`PluginError`] when a required field cannot be read.
fn read_invocation(r: job_invocation::Reader<'_>) -> Result<JobInvocation> {
    let envelope_version = r.get_envelope_version();
    if envelope_version != 0 && envelope_version != ENVELOPE_VERSION {
        return Err(PluginError::unsupported(format!(
            "unsupported job envelope version {envelope_version}"
        )));
    }
    let checkpoint_json = text_of(r.get_checkpoint_json().map_err(from_capnp)?);
    if checkpoint_json.len() > MAX_CHECKPOINT_BYTES as usize {
        return Err(PluginError::payload_too_large(format!(
            "checkpoint of {} bytes exceeds {MAX_CHECKPOINT_BYTES}",
            checkpoint_json.len()
        )));
    }
    let checkpoint = if checkpoint_json.is_empty() && r.get_checkpoint_schema_version() == 0 {
        None
    } else {
        Some(JobCheckpoint {
            schema_version: r.get_checkpoint_schema_version(),
            json: checkpoint_json,
        })
    };
    let causation = text_of(r.get_causation_id().map_err(from_capnp)?);
    Ok(JobInvocation {
        envelope_version: if envelope_version == 0 {
            ENVELOPE_VERSION
        } else {
            envelope_version
        },
        payload_schema_version: r.get_payload_schema_version().max(1),
        invocation_id: text_of(r.get_invocation_id().map_err(from_capnp)?),
        command_type: text_of(r.get_command_type().map_err(from_capnp)?),
        payload_json: text_of(r.get_payload_json().map_err(from_capnp)?),
        idempotency_key: text_of(r.get_idempotency_key().map_err(from_capnp)?),
        attempt: r.get_attempt().max(1),
        correlation_id: text_of(r.get_correlation_id().map_err(from_capnp)?),
        causation_id: if causation.is_empty() {
            None
        } else {
            Some(causation)
        },
        deadline_unix_ms: r.get_deadline_unix_ms(),
        checkpoint,
        invocation_sequence: r.get_invocation_sequence(),
        step_id: {
            let s = text_of(r.get_step_id().map_err(from_capnp)?);
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        },
    })
}

/// Wraps an [`AsyncRead`] as a `ByteSource` capability (pull window = flow control).
pub struct ReadByteSource {
    reader: Arc<Mutex<Pin<Box<dyn AsyncRead + Send>>>>,
    window: u32,
}

impl ReadByteSource {
    /// Builds a source that yields at most `window` bytes per pull.
    #[must_use]
    pub fn new(reader: Pin<Box<dyn AsyncRead + Send>>, window: u32) -> Self {
        Self {
            reader: Arc::new(Mutex::new(reader)),
            window: window.clamp(1, MAX_STREAM_WINDOW_BYTES),
        }
    }
}

impl byte_source::Server for ReadByteSource {
    async fn pull(
        self: Rc<Self>,
        params: byte_source::PullParams,
        mut results: byte_source::PullResults,
    ) -> capnp::Result<()> {
        let max = params.get()?.get_max_bytes();
        let n = (max.min(self.window).max(1)) as usize;
        let mut buf = vec![0u8; n];
        let mut guard = self.reader.lock().await;
        let result = results.get().init_result();
        match AsyncReadExt::read(&mut *guard, &mut buf).await {
            Ok(read) => {
                buf.truncate(read);
                let mut ok = result.init_ok();
                ok.set_chunk(&buf);
                ok.set_done(read == 0);
            }
            Err(err) => {
                write_error(
                    result.init_err(),
                    &PluginError::internal(format!("byte source read: {err}")),
                );
            }
        }
        Ok(())
    }
}

/// Returns a `ByteSource` client that pulls from `reader`.
#[must_use]
pub fn byte_source_from_async_read(
    reader: Pin<Box<dyn AsyncRead + Send>>,
    window: u32,
) -> byte_source::Client {
    capnp_rpc::new_client(ReadByteSource::new(reader, window))
}

/// Pulls `source` into `writer` using bounded windows.
///
/// A failed `ByteSource.pull` is returned as [`PluginError`] — never as a clean
/// EOF. Destinations must abort rather than commit.
///
/// # Errors
///
/// Returns a plugin error when the stream or writer fails.
pub async fn pull_byte_source_to_writer<W: AsyncWrite + Unpin>(
    source: byte_source::Client,
    writer: &mut W,
    window: u32,
) -> Result<u64> {
    let window = window.clamp(1, MAX_STREAM_WINDOW_BYTES);
    let mut total = 0u64;
    loop {
        let mut req = source.pull_request();
        req.get().set_max_bytes(window);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            pull_reply::Ok(ok) => {
                let ok = ok.map_err(from_capnp)?;
                let chunk = ok.get_chunk().map_err(from_capnp)?;
                if !chunk.is_empty() {
                    writer.write_all(chunk).await.map_err(|err| {
                        PluginError::internal(format!("stream write failed: {err}"))
                    })?;
                    total += chunk.len() as u64;
                }
                if ok.get_done() || chunk.is_empty() {
                    break;
                }
            }
            pull_reply::Err(err) => {
                return Err(read_error(err.map_err(from_capnp)?));
            }
        }
    }
    writer
        .flush()
        .await
        .map_err(|err| PluginError::internal(format!("stream flush failed: {err}")))?;
    Ok(total)
}

enum ByteChunk {
    Data(Vec<u8>),
    Eof,
    Err(std::io::Error),
}

struct ByteSourceAsyncRead {
    rx: mpsc::Receiver<ByteChunk>,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl AsyncRead for ByteSourceAsyncRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.done {
            return Poll::Ready(Ok(()));
        }
        if self.pos < self.buf.len() {
            let n = (self.buf.len() - self.pos).min(buf.remaining());
            buf.put_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            if self.pos >= self.buf.len() {
                self.buf.clear();
                self.pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        match self.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) | Poll::Ready(Some(ByteChunk::Eof)) => {
                self.done = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Some(ByteChunk::Err(err))) => {
                self.done = true;
                Poll::Ready(Err(err))
            }
            Poll::Ready(Some(ByteChunk::Data(chunk))) => {
                self.buf = chunk;
                self.pos = 0;
                self.poll_read(cx, buf)
            }
        }
    }
}

fn async_read_from_byte_source(
    client: byte_source::Client,
    window: u32,
) -> Pin<Box<dyn AsyncRead + Send>> {
    let (tx, rx) = mpsc::channel(4);
    tokio::task::spawn_local(async move {
        let window = window.clamp(1, MAX_STREAM_WINDOW_BYTES);
        loop {
            let mut req = client.pull_request();
            req.get().set_max_bytes(window);
            let chunk = match req.send().promise.await {
                Ok(reply) => match reply.get() {
                    Ok(r) => match r.get_result() {
                        Ok(result) => match result.which() {
                            Ok(pull_reply::Ok(ok)) => match ok {
                                Ok(ok) => match ok.get_chunk() {
                                    Ok(bytes) => {
                                        let done = ok.get_done() || bytes.is_empty();
                                        if bytes.is_empty() {
                                            ByteChunk::Eof
                                        } else {
                                            let data = bytes.to_vec();
                                            if done {
                                                if tx.send(ByteChunk::Data(data)).await.is_err() {
                                                    return;
                                                }
                                                ByteChunk::Eof
                                            } else {
                                                ByteChunk::Data(data)
                                            }
                                        }
                                    }
                                    Err(err) => ByteChunk::Err(std::io::Error::other(err)),
                                },
                                Err(err) => ByteChunk::Err(std::io::Error::other(err)),
                            },
                            Ok(pull_reply::Err(err)) => {
                                let pe = err
                                    .map(read_error)
                                    .unwrap_or_else(|e| PluginError::unavailable(e.to_string()));
                                ByteChunk::Err(std::io::Error::other(pe))
                            }
                            Err(err) => ByteChunk::Err(std::io::Error::other(err)),
                        },
                        Err(err) => ByteChunk::Err(std::io::Error::other(err)),
                    },
                    Err(err) => ByteChunk::Err(std::io::Error::other(err)),
                },
                Err(err) => ByteChunk::Err(std::io::Error::other(err)),
            };
            let eof = matches!(chunk, ByteChunk::Eof | ByteChunk::Err(_));
            if tx.send(chunk).await.is_err() {
                return;
            }
            if eof {
                return;
            }
        }
    });
    Box::pin(ByteSourceAsyncRead {
        rx,
        buf: Vec::new(),
        pos: 0,
        done: false,
    })
}

/// Decode a [`ListPage`] and reject pages larger than `max`.
///
/// # Errors
///
/// Returns [`PluginError`] when the page exceeds `max` or a nested object
/// cannot be read.
fn decode_list_page(
    page: super::plugin_v2_capnp::list_page::Reader<'_>,
    max: u32,
) -> Result<ListPage> {
    let list = page.get_objects().map_err(from_capnp)?;
    if list.len() > max {
        return Err(PluginError::payload_too_large(format!(
            "list page of {} objects exceeds {max}",
            list.len()
        )));
    }
    let mut objects = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        objects.push(ObjectInfo {
            key: text_of(item.get_key().map_err(from_capnp)?),
            size: item.get_size(),
        });
    }
    let cursor = text_of(page.get_next_cursor().map_err(from_capnp)?);
    Ok(ListPage {
        objects,
        next_cursor: if cursor.is_empty() {
            None
        } else {
            Some(cursor)
        },
    })
}

/// Cap'n Proto server wrapping a [`Destination`] trait object.
pub struct DestinationServer {
    inner: Arc<dyn Destination>,
    window: u32,
}

impl DestinationServer {
    /// Serves `inner` with the given stream window.
    #[must_use]
    pub fn new(inner: Arc<dyn Destination>, window: u32) -> Self {
        Self {
            inner,
            window: window.clamp(1, MAX_STREAM_WINDOW_BYTES),
        }
    }
}

impl dest_iface::Server for DestinationServer {
    async fn head(
        self: Rc<Self>,
        params: dest_iface::HeadParams,
        mut results: dest_iface::HeadResults,
    ) -> capnp::Result<()> {
        let key = params.get()?.get_key()?.to_string().unwrap_or_default();
        let result = results.get().init_result();
        match self.inner.head(&key).await {
            Ok(Some(meta)) => {
                let mut ok = result.init_ok();
                ok.set_found(true);
                fill_metadata(ok.get_meta()?, &meta);
            }
            Ok(None) => {
                result.init_ok().set_found(false);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn list(
        self: Rc<Self>,
        params: dest_iface::ListParams,
        mut results: dest_iface::ListResults,
    ) -> capnp::Result<()> {
        let options = {
            let o = params.get()?.get_options()?;
            ListOptions {
                prefix: o.get_prefix().ok().map(text_of).unwrap_or_default(),
                cursor: {
                    let c = o.get_cursor().ok().map(text_of).unwrap_or_default();
                    if c.is_empty() {
                        None
                    } else {
                        Some(c)
                    }
                },
                limit: o.get_limit(),
            }
        };
        let result = results.get().init_result();
        match self.inner.list(options).await {
            Ok(page) => {
                let mut out = result.init_ok();
                if let Some(c) = &page.next_cursor {
                    out.set_next_cursor(c);
                }
                let mut list = out.init_objects(page.objects.len() as u32);
                for (i, obj) in page.objects.iter().enumerate() {
                    let mut item = list.reborrow().get(i as u32);
                    item.set_key(&obj.key);
                    item.set_size(obj.size);
                }
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn get(
        self: Rc<Self>,
        params: dest_iface::GetParams,
        mut results: dest_iface::GetResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let key = p.get_key().ok().map(text_of).unwrap_or_default();
        let range = p.get_options().ok().and_then(|o| {
            o.get_range().ok().map(|r| ByteRange {
                offset: r.get_offset(),
                length: {
                    let n = r.get_length();
                    if n == 0 {
                        None
                    } else {
                        Some(n)
                    }
                },
            })
        });
        let result = results.get().init_result();
        match self.inner.get(&key, range).await {
            Ok(read) => {
                let mut ok = result.init_ok();
                fill_metadata(ok.reborrow().get_meta()?, &read.meta);
                ok.set_body(byte_source_from_async_read(read.body, self.window));
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn put(
        self: Rc<Self>,
        params: dest_iface::PutParams,
        mut results: dest_iface::PutResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let key = p.get_key().ok().map(text_of).unwrap_or_default();
        let body = p.get_body().ok();
        let options = p
            .get_options()
            .ok()
            .map(read_write_options)
            .unwrap_or_default();
        let result = results.get().init_result();
        let Some(body) = body else {
            write_error(
                result.init_err(),
                &PluginError::invalid_params("put missing body stream"),
            );
            return Ok(());
        };
        let reader = async_read_from_byte_source(body, self.window);
        match self.inner.put(&key, reader, options).await {
            Ok(put) => fill_put_result(result.init_ok(), &put),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn commit(
        self: Rc<Self>,
        params: dest_iface::CommitParams,
        mut results: dest_iface::CommitResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let key = p.get_key().ok().map(text_of).unwrap_or_default();
        let token = p.get_commit_token().ok().map(text_of).unwrap_or_default();
        let result = results.get().init_result();
        match self.inner.commit(&key, &token).await {
            Ok(put) => fill_put_result(result.init_ok(), &put),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn abort_stage(
        self: Rc<Self>,
        params: dest_iface::AbortStageParams,
        mut results: dest_iface::AbortStageResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let key = p.get_key().ok().map(text_of).unwrap_or_default();
        let token = p.get_commit_token().ok().map(text_of).unwrap_or_default();
        let mut result = results.get().init_result();
        match self.inner.abort_stage(&key, &token).await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn copy(
        self: Rc<Self>,
        params: dest_iface::CopyParams,
        mut results: dest_iface::CopyResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let from = p.get_from().ok().map(text_of).unwrap_or_default();
        let to = p.get_to().ok().map(text_of).unwrap_or_default();
        let result = results.get().init_result();
        match self.inner.copy(&from, &to).await {
            Ok(copy) => result.init_ok().set_bytes_copied(copy.bytes_copied),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn delete(
        self: Rc<Self>,
        params: dest_iface::DeleteParams,
        mut results: dest_iface::DeleteResults,
    ) -> capnp::Result<()> {
        let key = params.get()?.get_key()?.to_string().unwrap_or_default();
        let mut result = results.get().init_result();
        match self.inner.delete(&key).await {
            Ok(()) => {
                result.set_ok(());
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

/// Host-side [`Destination`] over a capnp destination stub.
#[derive(Clone)]
pub struct DestinationClient {
    client: dest_iface::Client,
    window: u32,
    max_list_page: u32,
}

impl DestinationClient {
    /// Wraps a capnp destination client.
    #[must_use]
    pub fn new(client: dest_iface::Client, window: u32) -> Self {
        Self {
            client,
            window: window.clamp(1, MAX_STREAM_WINDOW_BYTES),
            max_list_page: MAX_LIST_PAGE,
        }
    }

    /// Applies negotiated list-page cardinality.
    #[must_use]
    pub fn with_max_list_page(mut self, max_list_page: u32) -> Self {
        self.max_list_page = max_list_page.max(1);
        self
    }
}

#[async_trait::async_trait(?Send)]
impl Destination for DestinationClient {
    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>> {
        let mut req = self.client.head_request();
        req.get().set_key(key);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            head_reply::Ok(ok) => {
                let ok = ok.map_err(from_capnp)?;
                if !ok.get_found() {
                    return Ok(None);
                }
                Ok(Some(read_metadata(ok.get_meta().map_err(from_capnp)?)?))
            }
            head_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    async fn list(&self, options: ListOptions) -> Result<ListPage> {
        let mut req = self.client.list_request();
        {
            let mut o = req.get().get_options().map_err(from_capnp)?;
            o.set_prefix(&options.prefix);
            if let Some(c) = &options.cursor {
                o.set_cursor(c);
            }
            o.set_limit(options.limit);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            list_reply::Ok(page) => decode_list_page(page.map_err(from_capnp)?, self.max_list_page),
            list_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<ReadResult> {
        let mut req = self.client.get_request();
        req.get().set_key(key);
        if let Some(range) = range {
            let o = req.get().get_options().map_err(from_capnp)?;
            let mut r = o.get_range().map_err(from_capnp)?;
            r.set_offset(range.offset);
            r.set_length(range.length.unwrap_or(0));
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            get_reply::Ok(ok) => {
                let ok = ok.map_err(from_capnp)?;
                Ok(ReadResult {
                    meta: read_metadata(ok.get_meta().map_err(from_capnp)?)?,
                    body: async_read_from_byte_source(
                        ok.get_body().map_err(from_capnp)?,
                        self.window,
                    ),
                })
            }
            get_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> Result<PutResult> {
        let mut req = self.client.put_request();
        req.get().set_key(key);
        req.get()
            .set_body(byte_source_from_async_read(body, self.window));
        {
            let o = req.get().get_options().map_err(from_capnp)?;
            fill_write_options(o, &options);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            put_reply::Ok(r) => {
                let r = r.map_err(from_capnp)?;
                Ok(PutResult {
                    key: text_of(r.get_key().map_err(from_capnp)?),
                    bytes_written: r.get_bytes_written(),
                    etag: {
                        let t = text_of(r.get_etag().map_err(from_capnp)?);
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    },
                    sha256: {
                        let d = r.get_sha256().map_err(from_capnp)?;
                        if d.is_empty() {
                            None
                        } else {
                            Some(d.to_vec())
                        }
                    },
                })
            }
            put_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    async fn copy(&self, from: &str, to: &str) -> Result<CopyResult> {
        let mut req = self.client.copy_request();
        req.get().set_from(from);
        req.get().set_to(to);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            copy_reply::Ok(r) => Ok(CopyResult {
                bytes_copied: r.map_err(from_capnp)?.get_bytes_copied(),
            }),
            copy_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut req = self.client.delete_request();
        req.get().set_key(key);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            empty_reply::Ok(()) => Ok(()),
            empty_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    async fn commit(&self, key: &str, commit_token: &str) -> Result<PutResult> {
        let mut req = self.client.commit_request();
        req.get().set_key(key);
        req.get().set_commit_token(commit_token);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            put_reply::Ok(r) => {
                let r = r.map_err(from_capnp)?;
                Ok(PutResult {
                    key: text_of(r.get_key().map_err(from_capnp)?),
                    bytes_written: r.get_bytes_written(),
                    etag: {
                        let t = text_of(r.get_etag().map_err(from_capnp)?);
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    },
                    sha256: {
                        let d = r.get_sha256().map_err(from_capnp)?;
                        if d.is_empty() {
                            None
                        } else {
                            Some(d.to_vec())
                        }
                    },
                })
            }
            put_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    async fn abort_stage(&self, key: &str, commit_token: &str) -> Result<()> {
        let mut req = self.client.abort_stage_request();
        req.get().set_key(key);
        req.get().set_commit_token(commit_token);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            empty_reply::Ok(()) => Ok(()),
            empty_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
}

/// Cap'n Proto server wrapping a [`Source`].
pub struct SourceServer {
    inner: Arc<dyn Source>,
    window: u32,
}

impl SourceServer {
    /// Serves `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn Source>, window: u32) -> Self {
        Self {
            inner,
            window: window.clamp(1, MAX_STREAM_WINDOW_BYTES),
        }
    }
}

impl source_capnp::Server for SourceServer {
    async fn open(
        self: Rc<Self>,
        params: source_capnp::OpenParams,
        mut results: source_capnp::OpenResults,
    ) -> capnp::Result<()> {
        let key = params.get()?.get_key()?.to_string().unwrap_or_default();
        let result = results.get().init_result();
        match self.inner.open(&key).await {
            Ok(read) => {
                let mut ok = result.init_ok();
                fill_metadata(ok.reborrow().get_meta()?, &read.meta);
                ok.set_body(byte_source_from_async_read(read.body, self.window));
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

/// Host-side [`Source`] over a capnp source stub.
#[derive(Clone)]
pub struct SourceClient {
    client: source_capnp::Client,
    window: u32,
}

impl SourceClient {
    /// Wraps a capnp source client.
    #[must_use]
    pub fn new(client: source_capnp::Client, window: u32) -> Self {
        Self {
            client,
            window: window.clamp(1, MAX_STREAM_WINDOW_BYTES),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Source for SourceClient {
    async fn open(&self, key: &str) -> Result<ReadResult> {
        let mut req = self.client.open_request();
        req.get().set_key(key);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            open_reply::Ok(ok) => {
                let ok = ok.map_err(from_capnp)?;
                Ok(ReadResult {
                    meta: read_metadata(ok.get_meta().map_err(from_capnp)?)?,
                    body: async_read_from_byte_source(
                        ok.get_body().map_err(from_capnp)?,
                        self.window,
                    ),
                })
            }
            open_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
}

struct ProgressServer {
    inner: Arc<dyn ProgressSink>,
}

impl progress_sink::Server for ProgressServer {
    async fn report(
        self: Rc<Self>,
        params: progress_sink::ReportParams,
        mut results: progress_sink::ReportResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let percent = p.get_percent();
        let message = p.get_message().ok().map(text_of).unwrap_or_default();
        let mut result = results.get().init_result();
        match self.inner.report(percent, &message).await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

struct CancellationServer {
    inner: Arc<dyn Cancellation>,
}

impl cancellation::Server for CancellationServer {
    async fn poll(
        self: Rc<Self>,
        _params: cancellation::PollParams,
        mut results: cancellation::PollResults,
    ) -> capnp::Result<()> {
        match self.inner.poll().await {
            Ok(cancelled) => {
                results.get().set_cancelled(cancelled);
                Ok(())
            }
            Err(err) => Err(capnp::Error::failed(format!("cancellation poll: {err}"))),
        }
    }
}

struct CancellationClient {
    client: cancellation::Client,
}

#[async_trait::async_trait(?Send)]
impl Cancellation for CancellationClient {
    async fn poll(&self) -> Result<bool> {
        let req = self.client.poll_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        Ok(reply.get().map_err(from_capnp)?.get_cancelled())
    }
}

/// Cap'n Proto server wrapping [`PluginRoot`].
pub struct PluginServer {
    inner: Arc<dyn PluginRoot>,
    window: u32,
}

impl PluginServer {
    /// Serves `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn PluginRoot>, window: u32) -> Self {
        Self {
            inner,
            window: window.clamp(1, MAX_STREAM_WINDOW_BYTES),
        }
    }
}

impl bookclerk_plugin::Server for PluginServer {
    async fn describe(
        self: Rc<Self>,
        _params: bookclerk_plugin::DescribeParams,
        mut results: bookclerk_plugin::DescribeResults,
    ) -> capnp::Result<()> {
        let result = results.get().init_result();
        match self.inner.describe().await {
            Ok(d) => fill_describe(result.init_ok(), &d)?,
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn destination(
        self: Rc<Self>,
        params: bookclerk_plugin::DestinationParams,
        mut results: bookclerk_plugin::DestinationResults,
    ) -> capnp::Result<()> {
        let c = params.get()?.get_context()?;
        let ctx = DestinationContext {
            json: c.get_json().ok().map(text_of).unwrap_or_default(),
        };
        let mut result = results.get().init_result();
        match self.inner.destination(ctx).await {
            Ok(dest) => {
                let client: dest_iface::Client =
                    capnp_rpc::new_client(DestinationServer::new(Arc::from(dest), self.window));
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn source(
        self: Rc<Self>,
        params: bookclerk_plugin::SourceParams,
        mut results: bookclerk_plugin::SourceResults,
    ) -> capnp::Result<()> {
        let c = params.get()?.get_context()?;
        let ctx = SourceContext {
            json: c.get_json().ok().map(text_of).unwrap_or_default(),
        };
        let mut result = results.get().init_result();
        match self.inner.source(ctx).await {
            Ok(src) => {
                let client: source_capnp::Client =
                    capnp_rpc::new_client(SourceServer::new(Arc::from(src), self.window));
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn worker(
        self: Rc<Self>,
        params: bookclerk_plugin::WorkerParams,
        mut results: bookclerk_plugin::WorkerResults,
    ) -> capnp::Result<()> {
        let c = params.get()?.get_context()?;
        let ctx = WorkerContext {
            job_id: c.get_job_id().ok().map(text_of).unwrap_or_default(),
            json: c.get_json().ok().map(text_of).unwrap_or_default(),
        };
        let mut result = results.get().init_result();
        match self.inner.worker(ctx).await {
            Ok(handler) => {
                let client: job_handler::Client =
                    capnp_rpc::new_client(JobHandlerServer::new(Arc::from(handler), self.window));
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn shutdown(
        self: Rc<Self>,
        _params: bookclerk_plugin::ShutdownParams,
        mut results: bookclerk_plugin::ShutdownResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.shutdown().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn content_source(
        self: Rc<Self>,
        params: bookclerk_plugin::ContentSourceParams,
        mut results: bookclerk_plugin::ContentSourceResults,
    ) -> capnp::Result<()> {
        let c = params.get()?.get_context()?;
        let ctx = ContentSourceContext {
            json: c.get_json().ok().map(text_of).unwrap_or_default(),
        };
        let mut result = results.get().init_result();
        match self.inner.content_source(ctx).await {
            Ok(role) => {
                let client: content_source_capnp::Client =
                    capnp_rpc::new_client(ContentSourceServer {
                        inner: Arc::from(role),
                    });
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn integration(
        self: Rc<Self>,
        params: bookclerk_plugin::IntegrationParams,
        mut results: bookclerk_plugin::IntegrationResults,
    ) -> capnp::Result<()> {
        let c = params.get()?.get_context()?;
        let ctx = IntegrationContext {
            json: c.get_json().ok().map(text_of).unwrap_or_default(),
        };
        let mut result = results.get().init_result();
        match self.inner.integration(ctx).await {
            Ok(role) => {
                let client: integration_capnp::Client = capnp_rpc::new_client(IntegrationServer {
                    inner: Arc::from(role),
                });
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn database(
        self: Rc<Self>,
        params: bookclerk_plugin::DatabaseParams,
        mut results: bookclerk_plugin::DatabaseResults,
    ) -> capnp::Result<()> {
        let c = params.get()?.get_context()?;
        let ctx = DatabaseContext {
            json: c.get_json().ok().map(text_of).unwrap_or_default(),
        };
        let mut result = results.get().init_result();
        match self.inner.database(ctx).await {
            Ok(role) => {
                let client: database_capnp::Client = capnp_rpc::new_client(DatabaseServer {
                    inner: Arc::from(role),
                });
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn cli_describe(
        self: Rc<Self>,
        _params: bookclerk_plugin::CliDescribeParams,
        mut results: bookclerk_plugin::CliDescribeResults,
    ) -> capnp::Result<()> {
        let result = results.get().init_result();
        write_json_reply(result, self.inner.cli_describe().await);
        Ok(())
    }

    async fn cli_invoke(
        self: Rc<Self>,
        params: bookclerk_plugin::CliInvokeParams,
        mut results: bookclerk_plugin::CliInvokeResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        let result = results.get().init_result();
        write_json_reply(result, self.inner.cli_invoke(&json).await);
        Ok(())
    }
}

fn write_json_reply(result: json_reply::Builder<'_>, outcome: Result<String>) {
    match outcome {
        Ok(json) => result.init_ok().set_json(&json),
        Err(err) => write_error(result.init_err(), &err),
    }
}

fn write_health_reply(result: health_reply::Builder<'_>, outcome: Result<HealthOk>) {
    match outcome {
        Ok(h) => {
            let mut ok = result.init_ok();
            ok.set_ok(h.ok);
            ok.set_detail(&h.detail);
        }
        Err(err) => write_error(result.init_err(), &err),
    }
}

fn write_event_result(b: event_result_capnp::Builder<'_>, result: &EventResult) {
    match result {
        EventResult::Ack => {
            b.init_ack();
        }
        EventResult::Retry {
            retry_at_unix_ms,
            reason,
        } => {
            let mut r = b.init_retry();
            r.set_retry_at_unix_ms(*retry_at_unix_ms);
            r.set_reason(reason);
        }
        EventResult::Reject { reason } => {
            b.init_reject().set_reason(reason);
        }
        EventResult::DeadLetter { reason } => {
            b.init_dead_letter().set_reason(reason);
        }
    }
}

/// Decode a [`DomainEvent`] from Cap'n Proto.
///
/// # Errors
///
/// Returns when a text or data field cannot be read from the message.
fn read_domain_event(r: domain_event::Reader<'_>) -> Result<DomainEvent> {
    Ok(DomainEvent {
        event_id: text_of(r.get_event_id().map_err(from_capnp)?),
        event_type: text_of(r.get_event_type().map_err(from_capnp)?),
        schema_version: r.get_schema_version(),
        occurred_at_unix_ms: r.get_occurred_at_unix_ms(),
        account_id: text_of(r.get_account_id().map_err(from_capnp)?),
        correlation_id: text_of(r.get_correlation_id().map_err(from_capnp)?),
        causation_id: text_of(r.get_causation_id().map_err(from_capnp)?),
        deduplication_key: text_of(r.get_deduplication_key().map_err(from_capnp)?),
        delivery_attempt: r.get_delivery_attempt(),
        payload: r.get_payload().map_err(from_capnp)?.to_vec(),
    })
}

/// Decode a SQL [`Statement`] from Cap'n Proto.
///
/// # Errors
///
/// Returns when a text field cannot be read from the message.
fn read_statement(r: super::plugin_v2_capnp::statement::Reader<'_>) -> Result<Statement> {
    Ok(Statement {
        sql: text_of(r.get_sql().map_err(from_capnp)?),
        values_json: text_of(r.get_values_json().map_err(from_capnp)?),
    })
}

fn write_exec_reply(result: exec_reply::Builder<'_>, outcome: Result<ExecResult>) {
    match outcome {
        Ok(exec) => {
            let mut ok = result.init_ok();
            ok.set_last_insert_id(exec.last_insert_id);
            ok.set_rows_affected(exec.rows_affected);
        }
        Err(err) => write_error(result.init_err(), &err),
    }
}

fn write_query_reply(result: query_reply::Builder<'_>, outcome: Result<QueryPage>) {
    match outcome {
        Ok(page) => {
            let mut ok = result.init_ok();
            ok.set_rows_json(&page.rows_json);
            if let Some(c) = &page.next_cursor {
                ok.set_next_cursor(c);
            }
        }
        Err(err) => write_error(result.init_err(), &err),
    }
}

struct ContentSourceServer {
    inner: Arc<dyn ContentSource>,
}

impl content_source_capnp::Server for ContentSourceServer {
    async fn login(
        self: Rc<Self>,
        params: content_source_capnp::LoginParams,
        mut results: content_source_capnp::LoginResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(results.get().init_result(), self.inner.login(&json).await);
        Ok(())
    }

    async fn scan(
        self: Rc<Self>,
        params: content_source_capnp::ScanParams,
        mut results: content_source_capnp::ScanResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(results.get().init_result(), self.inner.scan(&json).await);
        Ok(())
    }

    async fn fetch_title(
        self: Rc<Self>,
        params: content_source_capnp::FetchTitleParams,
        mut results: content_source_capnp::FetchTitleResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.fetch_title(&json).await,
        );
        Ok(())
    }

    async fn list_accounts(
        self: Rc<Self>,
        _params: content_source_capnp::ListAccountsParams,
        mut results: content_source_capnp::ListAccountsResults,
    ) -> capnp::Result<()> {
        write_json_reply(
            results.get().init_result(),
            self.inner.list_accounts().await,
        );
        Ok(())
    }

    async fn login_start(
        self: Rc<Self>,
        params: content_source_capnp::LoginStartParams,
        mut results: content_source_capnp::LoginStartResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.login_start(&json).await,
        );
        Ok(())
    }

    async fn login_complete(
        self: Rc<Self>,
        params: content_source_capnp::LoginCompleteParams,
        mut results: content_source_capnp::LoginCompleteResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.login_complete(&json).await,
        );
        Ok(())
    }

    async fn search_catalog(
        self: Rc<Self>,
        params: content_source_capnp::SearchCatalogParams,
        mut results: content_source_capnp::SearchCatalogResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.search_catalog(&json).await,
        );
        Ok(())
    }

    async fn expand_candidates(
        self: Rc<Self>,
        params: content_source_capnp::ExpandCandidatesParams,
        mut results: content_source_capnp::ExpandCandidatesResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.expand_candidates(&json).await,
        );
        Ok(())
    }

    async fn purchase_hint(
        self: Rc<Self>,
        params: content_source_capnp::PurchaseHintParams,
        mut results: content_source_capnp::PurchaseHintResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.purchase_hint(&json).await,
        );
        Ok(())
    }

    async fn list_deals(
        self: Rc<Self>,
        params: content_source_capnp::ListDealsParams,
        mut results: content_source_capnp::ListDealsResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.list_deals(&json).await,
        );
        Ok(())
    }

    async fn health(
        self: Rc<Self>,
        _params: content_source_capnp::HealthParams,
        mut results: content_source_capnp::HealthResults,
    ) -> capnp::Result<()> {
        write_health_reply(results.get().init_result(), self.inner.health().await);
        Ok(())
    }

    async fn diagnose(
        self: Rc<Self>,
        _params: content_source_capnp::DiagnoseParams,
        mut results: content_source_capnp::DiagnoseResults,
    ) -> capnp::Result<()> {
        write_json_reply(results.get().init_result(), self.inner.diagnose().await);
        Ok(())
    }

    async fn catalog_detail(
        self: Rc<Self>,
        params: content_source_capnp::CatalogDetailParams,
        mut results: content_source_capnp::CatalogDetailResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.catalog_detail(&json).await,
        );
        Ok(())
    }
}

struct IntegrationServer {
    inner: Arc<dyn Integration>,
}

impl integration_capnp::Server for IntegrationServer {
    async fn health(
        self: Rc<Self>,
        _params: integration_capnp::HealthParams,
        mut results: integration_capnp::HealthResults,
    ) -> capnp::Result<()> {
        write_health_reply(results.get().init_result(), self.inner.health().await);
        Ok(())
    }

    async fn on_event(
        self: Rc<Self>,
        params: integration_capnp::OnEventParams,
        mut results: integration_capnp::OnEventResults,
    ) -> capnp::Result<()> {
        let event = params
            .get()?
            .get_event()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                read_domain_event(r).map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        let result = results.get().init_result();
        match self.inner.on_event(event).await {
            Ok(ev) => write_event_result(result.init_ok(), &ev),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn start(
        self: Rc<Self>,
        _params: integration_capnp::StartParams,
        mut results: integration_capnp::StartResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.start().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn stop(
        self: Rc<Self>,
        _params: integration_capnp::StopParams,
        mut results: integration_capnp::StopResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.stop().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn diagnose(
        self: Rc<Self>,
        _params: integration_capnp::DiagnoseParams,
        mut results: integration_capnp::DiagnoseResults,
    ) -> capnp::Result<()> {
        write_json_reply(results.get().init_result(), self.inner.diagnose().await);
        Ok(())
    }

    async fn scan_library(
        self: Rc<Self>,
        params: integration_capnp::ScanLibraryParams,
        mut results: integration_capnp::ScanLibraryResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        let mut result = results.get().init_result();
        match self.inner.scan_library(&json).await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn sync_listening(
        self: Rc<Self>,
        _params: integration_capnp::SyncListeningParams,
        mut results: integration_capnp::SyncListeningResults,
    ) -> capnp::Result<()> {
        write_json_reply(
            results.get().init_result(),
            self.inner.sync_listening().await,
        );
        Ok(())
    }

    async fn authenticate_user(
        self: Rc<Self>,
        params: integration_capnp::AuthenticateUserParams,
        mut results: integration_capnp::AuthenticateUserResults,
    ) -> capnp::Result<()> {
        let json = params
            .get()?
            .get_params_json()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        write_json_reply(
            results.get().init_result(),
            self.inner.authenticate_user(&json).await,
        );
        Ok(())
    }

    async fn poll_events(
        self: Rc<Self>,
        _params: integration_capnp::PollEventsParams,
        mut results: integration_capnp::PollEventsResults,
    ) -> capnp::Result<()> {
        write_json_reply(results.get().init_result(), self.inner.poll_events().await);
        Ok(())
    }
}

struct DatabaseServer {
    inner: Arc<dyn Database>,
}

impl database_capnp::Server for DatabaseServer {
    async fn open_session(
        self: Rc<Self>,
        _params: database_capnp::OpenSessionParams,
        mut results: database_capnp::OpenSessionResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.open_session().await {
            Ok(session) => {
                let client: database_session_capnp::Client =
                    capnp_rpc::new_client(DatabaseSessionServer {
                        inner: Arc::from(session),
                    });
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

struct DatabaseSessionServer {
    inner: Arc<dyn DatabaseSession>,
}

impl database_session_capnp::Server for DatabaseSessionServer {
    async fn execute(
        self: Rc<Self>,
        params: database_session_capnp::ExecuteParams,
        mut results: database_session_capnp::ExecuteResults,
    ) -> capnp::Result<()> {
        let stmt = params
            .get()?
            .get_statement()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| read_statement(r).map_err(|err| capnp::Error::failed(err.to_string())))?;
        write_exec_reply(results.get().init_result(), self.inner.execute(stmt).await);
        Ok(())
    }

    async fn query(
        self: Rc<Self>,
        params: database_session_capnp::QueryParams,
        mut results: database_session_capnp::QueryResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let stmt = p
            .get_statement()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| read_statement(r).map_err(|err| capnp::Error::failed(err.to_string())))?;
        let cursor = p.get_cursor().ok().map(text_of).unwrap_or_default();
        let limit = p.get_limit();
        write_query_reply(
            results.get().init_result(),
            self.inner.query(stmt, &cursor, limit).await,
        );
        Ok(())
    }

    async fn begin(
        self: Rc<Self>,
        _params: database_session_capnp::BeginParams,
        mut results: database_session_capnp::BeginResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.begin().await {
            Ok(txn) => {
                let client: transaction_capnp::Client = capnp_rpc::new_client(TransactionServer {
                    inner: Arc::from(txn),
                });
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: database_session_capnp::CloseParams,
        mut results: database_session_capnp::CloseResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.close().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

struct TransactionServer {
    inner: Arc<dyn Transaction>,
}

impl transaction_capnp::Server for TransactionServer {
    async fn execute(
        self: Rc<Self>,
        params: transaction_capnp::ExecuteParams,
        mut results: transaction_capnp::ExecuteResults,
    ) -> capnp::Result<()> {
        let stmt = params
            .get()?
            .get_statement()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| read_statement(r).map_err(|err| capnp::Error::failed(err.to_string())))?;
        write_exec_reply(results.get().init_result(), self.inner.execute(stmt).await);
        Ok(())
    }

    async fn query(
        self: Rc<Self>,
        params: transaction_capnp::QueryParams,
        mut results: transaction_capnp::QueryResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let stmt = p
            .get_statement()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| read_statement(r).map_err(|err| capnp::Error::failed(err.to_string())))?;
        let cursor = p.get_cursor().ok().map(text_of).unwrap_or_default();
        let limit = p.get_limit();
        write_query_reply(
            results.get().init_result(),
            self.inner.query(stmt, &cursor, limit).await,
        );
        Ok(())
    }

    async fn commit(
        self: Rc<Self>,
        _params: transaction_capnp::CommitParams,
        mut results: transaction_capnp::CommitResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.commit().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn rollback(
        self: Rc<Self>,
        _params: transaction_capnp::RollbackParams,
        mut results: transaction_capnp::RollbackResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.rollback().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

struct JobHandlerServer {
    inner: Arc<dyn JobHandler>,
    window: u32,
}

impl JobHandlerServer {
    fn new(inner: Arc<dyn JobHandler>, window: u32) -> Self {
        Self { inner, window }
    }
}

impl job_handler::Server for JobHandlerServer {
    async fn handle(
        self: Rc<Self>,
        params: job_handler::HandleParams,
        mut results: job_handler::HandleResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let invocation = p
            .get_invocation()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                read_invocation(r).map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        let input = p
            .get_input()
            .ok()
            .ok_or_else(|| capnp::Error::failed("missing input source".into()))?;
        let output = p
            .get_output()
            .ok()
            .ok_or_else(|| capnp::Error::failed("missing output destination".into()))?;
        let progress: Arc<dyn ProgressSink> = match p.get_progress().ok() {
            Some(client) => Arc::new(ProgressClient { client }),
            None => Arc::new(NullProgress),
        };
        let cancel: Box<dyn Cancellation> = match p.get_cancel().ok() {
            Some(client) => Box::new(CancellationClient { client }),
            None => Box::new(NeverCancel),
        };
        let ctx = JobHandlerContext {
            input: Box::new(SourceClient::new(input, self.window)),
            output: Box::new(DestinationClient::new(output, self.window)),
            progress: Box::new(ProgressArc(progress)),
            cancel,
        };
        let result = results.get().init_result();
        match self.inner.handle(invocation, ctx).await {
            Ok(outcome) => fill_job_outcome(result.init_ok(), &outcome)?,
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

struct NullProgress;

#[async_trait::async_trait(?Send)]
impl ProgressSink for NullProgress {
    async fn report(&self, _percent: f32, _message: &str) -> Result<()> {
        Ok(())
    }
}

struct ProgressClient {
    client: progress_sink::Client,
}

#[async_trait::async_trait(?Send)]
impl ProgressSink for ProgressClient {
    async fn report(&self, percent: f32, message: &str) -> Result<()> {
        let mut req = self.client.report_request();
        req.get().set_percent(percent);
        req.get().set_message(message);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            empty_reply::Ok(()) => Ok(()),
            empty_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
}

struct ProgressArc(Arc<dyn ProgressSink>);

#[async_trait::async_trait(?Send)]
impl ProgressSink for ProgressArc {
    async fn report(&self, percent: f32, message: &str) -> Result<()> {
        self.0.report(percent, message).await
    }
}

/// Host bootstrap client for a v2 plugin vat.
#[derive(Clone)]
pub struct PluginClient {
    client: bookclerk_plugin::Client,
    /// Negotiated stream window.
    pub window: u32,
    /// Negotiated scalar / list limits.
    pub limits: ScalarLimits,
}

impl PluginClient {
    /// Wraps a bootstrap client.
    #[must_use]
    pub fn new(client: bookclerk_plugin::Client, window: u32) -> Self {
        let window = window.clamp(1, MAX_STREAM_WINDOW_BYTES);
        Self {
            client,
            window,
            limits: ScalarLimits {
                max_scalar_bytes: super::limits::MAX_SCALAR_BYTES,
                max_stream_window_bytes: window,
                max_list_page: MAX_LIST_PAGE,
            },
        }
    }

    /// Applies negotiated limits (stream window + list page).
    #[must_use]
    pub fn with_limits(mut self, limits: ScalarLimits) -> Self {
        self.limits = limits;
        self.window = limits
            .max_stream_window_bytes
            .clamp(1, MAX_STREAM_WINDOW_BYTES);
        self
    }

    /// Calls `describe`.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails or the version is not 2.
    pub async fn describe(&self) -> Result<PluginDescribe> {
        let req = self.client.describe_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        let m = match result.which().map_err(from_capnp)? {
            describe_reply::Ok(m) => m.map_err(from_capnp)?,
            describe_reply::Err(err) => return Err(read_error(err.map_err(from_capnp)?)),
        };
        if m.get_api_version() != super::PRODUCT_API_VERSION {
            return Err(PluginError::unsupported(format!(
                "unsupported apiVersion {}",
                m.get_api_version()
            )));
        }
        let feats = m.get_rpc_features().map_err(from_capnp)?;
        let mut rpc_features = Vec::new();
        for f in feats.iter() {
            rpc_features.push(f.map_err(from_capnp)?.to_string().unwrap_or_default());
        }
        let lim = m.get_scalar_limits().map_err(from_capnp)?;
        Ok(PluginDescribe {
            api_version: m.get_api_version(),
            id: text_of(m.get_id().map_err(from_capnp)?),
            kind: text_of(m.get_kind().map_err(from_capnp)?),
            display_name: {
                let n = text_of(m.get_display_name().map_err(from_capnp)?);
                if n.is_empty() {
                    None
                } else {
                    Some(n)
                }
            },
            rpc_features,
            scalar_limits: super::types::ScalarLimitsDto {
                max_scalar_bytes: lim.get_max_scalar_bytes(),
                max_stream_window_bytes: lim.get_max_stream_window_bytes(),
                max_list_page: lim.get_max_list_page(),
            },
            abi_major: {
                let n = m.get_abi_major();
                if n == 0 {
                    m.get_api_version()
                } else {
                    n
                }
            },
            abi_minor: m.get_abi_minor(),
            supported_roles: {
                let roles = m.get_supported_roles().map_err(from_capnp)?;
                let mut out = Vec::new();
                for role in roles.iter() {
                    out.push(role.map_err(from_capnp)?.to_string().unwrap_or_default());
                }
                out
            },
            metadata_json: text_of(m.get_metadata_json().map_err(from_capnp)?),
        })
    }

    /// Returns a destination capability.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory call fails.
    pub async fn destination(&self, ctx: DestinationContext) -> Result<DestinationClient> {
        let mut req = self.client.destination_request();
        {
            let mut c = req.get().get_context().map_err(from_capnp)?;
            c.set_json(&ctx.json);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            destination_reply::Ok(dest) => Ok(DestinationClient::new(
                dest.map_err(from_capnp)?,
                self.window,
            )
            .with_max_list_page(self.limits.max_list_page)),
            destination_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    /// Returns a source capability.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory call fails.
    pub async fn source(&self, ctx: SourceContext) -> Result<SourceClient> {
        let mut req = self.client.source_request();
        {
            let mut c = req.get().get_context().map_err(from_capnp)?;
            c.set_json(&ctx.json);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            source_reply::Ok(src) => Ok(SourceClient::new(src.map_err(from_capnp)?, self.window)),
            source_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    /// Returns a job handler capability.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory call fails.
    pub async fn worker(&self, ctx: WorkerContext) -> Result<job_handler::Client> {
        let mut req = self.client.worker_request();
        {
            let mut c = req.get().get_context().map_err(from_capnp)?;
            c.set_job_id(&ctx.job_id);
            c.set_json(&ctx.json);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            worker_reply::Ok(handler) => handler.map_err(from_capnp),
            worker_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    /// Invokes `JobHandler.handle` with host-granted source/destination stubs.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the handler fails.
    pub async fn handle_job(
        &self,
        handler: job_handler::Client,
        invocation: JobInvocation,
        input: Arc<dyn Source>,
        output: Arc<dyn Destination>,
        progress: Arc<dyn ProgressSink>,
    ) -> Result<JobOutcome> {
        self.handle_job_with_cancel(
            handler,
            invocation,
            input,
            output,
            progress,
            Arc::new(NeverCancel),
        )
        .await
    }

    /// [`Self::handle_job`] with an explicit cancellation capability.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the handler fails.
    pub async fn handle_job_with_cancel(
        &self,
        handler: job_handler::Client,
        invocation: JobInvocation,
        input: Arc<dyn Source>,
        output: Arc<dyn Destination>,
        progress: Arc<dyn ProgressSink>,
        cancel: Arc<dyn Cancellation>,
    ) -> Result<JobOutcome> {
        let mut req = handler.handle_request();
        fill_invocation(req.get().get_invocation().map_err(from_capnp)?, &invocation)
            .map_err(from_capnp)?;
        req.get()
            .set_input(capnp_rpc::new_client(SourceServer::new(input, self.window)));
        req.get()
            .set_output(capnp_rpc::new_client(DestinationServer::new(
                output,
                self.window,
            )));
        req.get()
            .set_progress(capnp_rpc::new_client(ProgressServer { inner: progress }));
        req.get()
            .set_cancel(capnp_rpc::new_client(CancellationServer { inner: cancel }));
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            handle_reply::Ok(o) => read_job_outcome(o.map_err(from_capnp)?),
            handle_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    /// Returns a storefront content-source capability.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory call fails.
    pub async fn content_source(&self, ctx: ContentSourceContext) -> Result<ContentSourceClient> {
        let mut req = self.client.content_source_request();
        {
            let mut c = req.get().get_context().map_err(from_capnp)?;
            c.set_json(&ctx.json);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            content_source_reply::Ok(src) => Ok(ContentSourceClient {
                client: src.map_err(from_capnp)?,
            }),
            content_source_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    /// Returns an integration capability.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory call fails.
    pub async fn integration(&self, ctx: IntegrationContext) -> Result<IntegrationClient> {
        let mut req = self.client.integration_request();
        {
            let mut c = req.get().get_context().map_err(from_capnp)?;
            c.set_json(&ctx.json);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            integration_reply::Ok(src) => Ok(IntegrationClient {
                client: src.map_err(from_capnp)?,
            }),
            integration_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    /// Returns a database factory.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory call fails.
    pub async fn database(&self, ctx: DatabaseContext) -> Result<DatabaseClient> {
        let mut req = self.client.database_request();
        {
            let mut c = req.get().get_context().map_err(from_capnp)?;
            c.set_json(&ctx.json);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            database_reply::Ok(src) => Ok(DatabaseClient {
                client: src.map_err(from_capnp)?,
            }),
            database_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    /// Returns the guest CLI schema JSON.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn cli_describe(&self) -> Result<String> {
        let req = self.client.cli_describe_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    /// Invokes a guest CLI command (`CliInvokeParams` JSON).
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn cli_invoke(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.cli_invoke_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
}

/// Decode a JSON success/error union.
///
/// # Errors
///
/// Returns the nested [`PluginError`] or a Cap'n Proto read failure.
fn read_json_reply(result: json_reply::Reader<'_>) -> Result<String> {
    match result.which().map_err(from_capnp)? {
        json_reply::Ok(ok) => {
            let ok = ok.map_err(from_capnp)?;
            Ok(text_of(ok.get_json().map_err(from_capnp)?))
        }
        json_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

/// Decode a health success/error union.
///
/// # Errors
///
/// Returns the nested [`PluginError`] or a Cap'n Proto read failure.
fn read_health_reply(result: health_reply::Reader<'_>) -> Result<HealthOk> {
    match result.which().map_err(from_capnp)? {
        health_reply::Ok(ok) => {
            let ok = ok.map_err(from_capnp)?;
            Ok(HealthOk {
                ok: ok.get_ok(),
                detail: text_of(ok.get_detail().map_err(from_capnp)?),
            })
        }
        health_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

/// Cap'n Proto client for [`ContentSource`].
pub struct ContentSourceClient {
    client: content_source_capnp::Client,
}

#[async_trait::async_trait(?Send)]
impl ContentSource for ContentSourceClient {
    async fn login(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.login_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn scan(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.scan_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn fetch_title(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.fetch_title_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn list_accounts(&self) -> Result<String> {
        let req = self.client.list_accounts_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn login_start(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.login_start_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn login_complete(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.login_complete_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn search_catalog(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.search_catalog_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn expand_candidates(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.expand_candidates_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn purchase_hint(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.purchase_hint_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn list_deals(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.list_deals_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn catalog_detail(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.catalog_detail_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn health(&self) -> Result<HealthOk> {
        let req = self.client.health_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_health_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn diagnose(&self) -> Result<String> {
        let req = self.client.diagnose_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
}

/// Cap'n Proto client for [`Integration`].
pub struct IntegrationClient {
    client: integration_capnp::Client,
}

#[async_trait::async_trait(?Send)]
impl Integration for IntegrationClient {
    async fn health(&self) -> Result<HealthOk> {
        let req = self.client.health_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_health_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn on_event(&self, event: DomainEvent) -> Result<EventResult> {
        let mut req = self.client.on_event_request();
        {
            let mut e = req.get().get_event().map_err(from_capnp)?;
            e.set_event_id(&event.event_id);
            e.set_event_type(&event.event_type);
            e.set_schema_version(event.schema_version);
            e.set_occurred_at_unix_ms(event.occurred_at_unix_ms);
            e.set_account_id(&event.account_id);
            e.set_correlation_id(&event.correlation_id);
            e.set_causation_id(&event.causation_id);
            e.set_deduplication_key(&event.deduplication_key);
            e.set_delivery_attempt(event.delivery_attempt);
            e.set_payload(&event.payload);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            event_result_reply::Ok(ok) => read_event_result(ok.map_err(from_capnp)?),
            event_result_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
    async fn start(&self) -> Result<()> {
        let req = self.client.start_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn stop(&self) -> Result<()> {
        let req = self.client.stop_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn diagnose(&self) -> Result<String> {
        let req = self.client.diagnose_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn scan_library(&self, params_json: &str) -> Result<()> {
        let mut req = self.client.scan_library_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn sync_listening(&self) -> Result<String> {
        let req = self.client.sync_listening_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn authenticate_user(&self, params_json: &str) -> Result<String> {
        let mut req = self.client.authenticate_user_request();
        req.get().set_params_json(params_json);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn poll_events(&self) -> Result<String> {
        let req = self.client.poll_events_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_json_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
}

/// Decode an empty success/error union.
///
/// # Errors
///
/// Returns the nested [`PluginError`] or a Cap'n Proto read failure.
fn read_empty(result: empty_reply::Reader<'_>) -> Result<()> {
    match result.which().map_err(from_capnp)? {
        empty_reply::Ok(()) => Ok(()),
        empty_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

/// Decode an [`EventResult`] union.
///
/// # Errors
///
/// Returns when the union or a nested text field cannot be read.
fn read_event_result(r: event_result_capnp::Reader<'_>) -> Result<EventResult> {
    Ok(match r.which().map_err(from_capnp)? {
        event_result_capnp::Ack(_) => EventResult::Ack,
        event_result_capnp::Retry(ok) => {
            let ok = ok.map_err(from_capnp)?;
            EventResult::Retry {
                retry_at_unix_ms: ok.get_retry_at_unix_ms(),
                reason: text_of(ok.get_reason().map_err(from_capnp)?),
            }
        }
        event_result_capnp::Reject(ok) => EventResult::Reject {
            reason: text_of(ok.map_err(from_capnp)?.get_reason().map_err(from_capnp)?),
        },
        event_result_capnp::DeadLetter(ok) => EventResult::DeadLetter {
            reason: text_of(ok.map_err(from_capnp)?.get_reason().map_err(from_capnp)?),
        },
    })
}

/// Cap'n Proto client for [`Database`].
pub struct DatabaseClient {
    client: database_capnp::Client,
}

#[async_trait::async_trait(?Send)]
impl Database for DatabaseClient {
    async fn open_session(&self) -> Result<Box<dyn DatabaseSession>> {
        let req = self.client.open_session_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            session_reply::Ok(sess) => Ok(Box::new(DatabaseSessionClient {
                client: sess.map_err(from_capnp)?,
            })),
            session_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
}

struct DatabaseSessionClient {
    client: database_session_capnp::Client,
}

fn write_statement(mut b: super::plugin_v2_capnp::statement::Builder<'_>, statement: &Statement) {
    b.set_sql(&statement.sql);
    b.set_values_json(&statement.values_json);
}

/// Decode an execute success/error union.
///
/// # Errors
///
/// Returns the nested [`PluginError`] or a Cap'n Proto read failure.
fn read_exec_reply(result: exec_reply::Reader<'_>) -> Result<ExecResult> {
    match result.which().map_err(from_capnp)? {
        exec_reply::Ok(ok) => {
            let ok = ok.map_err(from_capnp)?;
            Ok(ExecResult {
                last_insert_id: ok.get_last_insert_id(),
                rows_affected: ok.get_rows_affected(),
            })
        }
        exec_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

/// Decode a query-page success/error union.
///
/// # Errors
///
/// Returns the nested [`PluginError`] or a Cap'n Proto read failure.
fn read_query_reply(result: query_reply::Reader<'_>) -> Result<QueryPage> {
    match result.which().map_err(from_capnp)? {
        query_reply::Ok(ok) => {
            let ok = ok.map_err(from_capnp)?;
            let cursor = text_of(ok.get_next_cursor().map_err(from_capnp)?);
            Ok(QueryPage {
                rows_json: text_of(ok.get_rows_json().map_err(from_capnp)?),
                next_cursor: if cursor.is_empty() {
                    None
                } else {
                    Some(cursor)
                },
            })
        }
        query_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

#[async_trait::async_trait(?Send)]
impl DatabaseSession for DatabaseSessionClient {
    async fn execute(&self, statement: Statement) -> Result<ExecResult> {
        let mut req = self.client.execute_request();
        write_statement(req.get().get_statement().map_err(from_capnp)?, &statement);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_exec_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn query(&self, statement: Statement, cursor: &str, limit: u32) -> Result<QueryPage> {
        let mut req = self.client.query_request();
        write_statement(req.get().get_statement().map_err(from_capnp)?, &statement);
        req.get().set_cursor(cursor);
        req.get().set_limit(limit);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_query_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        let req = self.client.begin_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            transaction_reply::Ok(txn) => Ok(Box::new(TransactionClient {
                client: txn.map_err(from_capnp)?,
            })),
            transaction_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
    async fn close(&self) -> Result<()> {
        let req = self.client.close_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
}

struct TransactionClient {
    client: transaction_capnp::Client,
}

#[async_trait::async_trait(?Send)]
impl Transaction for TransactionClient {
    async fn execute(&self, statement: Statement) -> Result<ExecResult> {
        let mut req = self.client.execute_request();
        write_statement(req.get().get_statement().map_err(from_capnp)?, &statement);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_exec_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn query(&self, statement: Statement, cursor: &str, limit: u32) -> Result<QueryPage> {
        let mut req = self.client.query_request();
        write_statement(req.get().get_statement().map_err(from_capnp)?, &statement);
        req.get().set_cursor(cursor);
        req.get().set_limit(limit);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_query_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn commit(&self) -> Result<()> {
        let req = self.client.commit_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
    async fn rollback(&self) -> Result<()> {
        let req = self.client.rollback_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
}

/// Serves `plugin` as the bootstrap object on a two-party vat over `reader`/`writer`.
///
/// Must run inside a `tokio::task::LocalSet`.
///
/// # Errors
///
/// Returns a plugin error when the vat fails.
pub async fn serve_plugin<R, W>(
    plugin: Arc<dyn PluginRoot>,
    reader: R,
    writer: W,
    window: u32,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + 'static,
    W: tokio::io::AsyncWrite + Unpin + 'static,
{
    let window = window.clamp(1, MAX_STREAM_WINDOW_BYTES);
    let client: bookclerk_plugin::Client = capnp_rpc::new_client(PluginServer::new(plugin, window));
    let network = twoparty::VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let rpc_system = RpcSystem::new(Box::new(network), Some(client.client));
    rpc_system
        .await
        .map_err(|err| PluginError::internal(err.to_string()))
}

/// Serves `plugin` on stdin/stdout. Must run on a current-thread runtime + `LocalSet`.
///
/// # Errors
///
/// Returns a plugin error when the vat fails.
pub async fn serve_plugin_stdio(plugin: Arc<dyn PluginRoot>, window: u32) -> Result<()> {
    serve_plugin(plugin, tokio::io::stdin(), tokio::io::stdout(), window).await
}

/// Connects as the client side of a two-party vat.
///
/// Must run inside a `tokio::task::LocalSet`. The returned [`RpcSystem`] must be
/// spawned with `tokio::task::spawn_local`.
pub fn connect_plugin<R, W>(
    reader: R,
    writer: W,
    window: u32,
) -> (PluginClient, RpcSystem<rpc_twoparty_capnp::Side>)
where
    R: tokio::io::AsyncRead + Unpin + 'static,
    W: tokio::io::AsyncWrite + Unpin + 'static,
{
    let network = twoparty::VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let mut rpc_system = RpcSystem::new(Box::new(network), None);
    let client: bookclerk_plugin::Client = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);
    (
        PluginClient::new(client, window.clamp(1, MAX_STREAM_WINDOW_BYTES)),
        rpc_system,
    )
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::v2::{
        ByteRange, Cancellation, CopyResult, Destination, DestinationContext, JobHandler,
        JobHandlerContext, JobInvocation, JobOutcome, ListOptions, ListPage, ObjectInfo,
        ObjectMetadata, PluginDescribe, PluginRoot, ProgressSink, PutResult, ReadResult,
        ScalarLimits, Source, SourceContext, WorkerContext, WriteOptions, FEATURE_SCALAR_LIMITS,
        FEATURE_STREAMS, MAX_LIST_PAGE, PRODUCT_API_VERSION,
    };
    use crate::{PluginError, PluginErrorCode, Result};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::io::duplex;

    struct MemDest {
        store: Mutex<HashMap<String, Vec<u8>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl Destination for MemDest {
        async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>> {
            if key == "internal-msg" {
                return Err(PluginError::internal("object not_found in cache"));
            }
            if key == "unknown-code" {
                return Err(PluginError::from_wire("future_retry_policy", "try later"));
            }
            let store = self.store.lock().expect("lock");
            Ok(store.get(key).map(|v| ObjectMetadata {
                key: key.into(),
                size: v.len() as u64,
                ..Default::default()
            }))
        }

        async fn list(&self, _options: ListOptions) -> Result<ListPage> {
            Ok(ListPage {
                objects: (0..MAX_LIST_PAGE + 2)
                    .map(|i| ObjectInfo {
                        key: format!("k{i}"),
                        size: 1,
                    })
                    .collect(),
                next_cursor: None,
            })
        }

        async fn get(&self, key: &str, _range: Option<ByteRange>) -> Result<ReadResult> {
            if key == "fail-mid" {
                return Ok(ReadResult {
                    meta: ObjectMetadata {
                        key: key.into(),
                        size: 100,
                        ..Default::default()
                    },
                    body: Box::pin(FailAfter { remain: 8 }),
                });
            }
            let store = self.store.lock().expect("lock");
            let data = store
                .get(key)
                .cloned()
                .ok_or_else(|| PluginError::not_found(format!("missing {key}")))?;
            let size = data.len() as u64;
            Ok(ReadResult {
                meta: ObjectMetadata {
                    key: key.into(),
                    size,
                    ..Default::default()
                },
                body: Box::pin(std::io::Cursor::new(data)),
            })
        }

        async fn put(
            &self,
            key: &str,
            mut body: Pin<Box<dyn AsyncRead + Send>>,
            options: WriteOptions,
        ) -> Result<PutResult> {
            let mut buf = Vec::new();
            body.read_to_end(&mut buf)
                .await
                .map_err(|err| PluginError::internal(err.to_string()))?;
            if let Some(len) = options.content_length {
                if buf.len() as u64 != len {
                    return Err(PluginError::invalid_params(format!(
                        "content-length {len} got {}",
                        buf.len()
                    )));
                }
            }
            let n = buf.len() as u64;
            self.store.lock().expect("lock").insert(key.into(), buf);
            Ok(PutResult {
                key: key.into(),
                bytes_written: n,
                ..Default::default()
            })
        }

        async fn copy(&self, _from: &str, _to: &str) -> Result<CopyResult> {
            Err(PluginError::unsupported("copy"))
        }

        async fn delete(&self, key: &str) -> Result<()> {
            self.store.lock().expect("lock").remove(key);
            Ok(())
        }
    }

    struct FailAfter {
        remain: usize,
    }

    impl AsyncRead for FailAfter {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.remain == 0 {
                return Poll::Ready(Err(std::io::Error::other("source exploded")));
            }
            let n = self.remain.min(buf.remaining()).min(4);
            buf.put_slice(&vec![1u8; n]);
            self.remain -= n;
            Poll::Ready(Ok(()))
        }
    }

    struct TestPlugin {
        dest: Arc<MemDest>,
    }

    #[async_trait::async_trait(?Send)]
    impl PluginRoot for TestPlugin {
        async fn describe(&self) -> Result<PluginDescribe> {
            Ok(PluginDescribe {
                api_version: PRODUCT_API_VERSION,
                id: "native_test".into(),
                kind: "output".into(),
                display_name: None,
                rpc_features: vec![FEATURE_SCALAR_LIMITS.into(), FEATURE_STREAMS.into()],
                scalar_limits: ScalarLimits::default().into(),
                ..PluginDescribe::default()
            })
        }

        async fn destination(&self, _context: DestinationContext) -> Result<Box<dyn Destination>> {
            Ok(Box::new(DestClone(Arc::clone(&self.dest))))
        }

        async fn source(&self, _context: SourceContext) -> Result<Box<dyn Source>> {
            Err(PluginError::unsupported("source"))
        }

        async fn worker(&self, _context: WorkerContext) -> Result<Box<dyn JobHandler>> {
            Err(PluginError::unsupported("worker"))
        }
    }

    struct DestClone(Arc<MemDest>);

    #[async_trait::async_trait(?Send)]
    impl Destination for DestClone {
        async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>> {
            self.0.head(key).await
        }
        async fn list(&self, options: ListOptions) -> Result<ListPage> {
            self.0.list(options).await
        }
        async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<ReadResult> {
            self.0.get(key, range).await
        }
        async fn put(
            &self,
            key: &str,
            body: Pin<Box<dyn AsyncRead + Send>>,
            options: WriteOptions,
        ) -> Result<PutResult> {
            self.0.put(key, body, options).await
        }
        async fn copy(&self, from: &str, to: &str) -> Result<CopyResult> {
            self.0.copy(from, to).await
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.0.delete(key).await
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Source for DestClone {
        async fn open(&self, key: &str) -> Result<ReadResult> {
            Destination::get(&*self.0, key, None).await
        }
    }

    struct TestCancel(Arc<AtomicBool>);

    #[async_trait::async_trait(?Send)]
    impl Cancellation for TestCancel {
        async fn poll(&self) -> Result<bool> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }

    struct NoopProgress;

    #[async_trait::async_trait(?Send)]
    impl ProgressSink for NoopProgress {
        async fn report(&self, _percent: f32, _message: &str) -> Result<()> {
            Ok(())
        }
    }

    struct SlowHandler;

    #[async_trait::async_trait(?Send)]
    impl JobHandler for SlowHandler {
        async fn handle(
            &self,
            _invocation: JobInvocation,
            context: JobHandlerContext,
        ) -> Result<JobOutcome> {
            for _ in 0..200 {
                if context.cancel.poll().await? {
                    return Ok(JobOutcome::Cancelled {
                        message: "fence lost".into(),
                    });
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            context
                .output
                .put(
                    "from-a",
                    Box::pin(std::io::Cursor::new(b"attempt-a".to_vec())),
                    WriteOptions::default(),
                )
                .await?;
            Ok(JobOutcome::Completed {
                message: "committed".into(),
                bytes_copied: 9,
            })
        }
    }

    struct LeasePlugin {
        dest: Arc<MemDest>,
    }

    #[async_trait::async_trait(?Send)]
    impl PluginRoot for LeasePlugin {
        async fn describe(&self) -> Result<PluginDescribe> {
            Ok(PluginDescribe {
                api_version: PRODUCT_API_VERSION,
                id: "native_test".into(),
                kind: "output".into(),
                display_name: None,
                rpc_features: vec![FEATURE_SCALAR_LIMITS.into(), FEATURE_STREAMS.into()],
                scalar_limits: ScalarLimits::default().into(),
                ..PluginDescribe::default()
            })
        }

        async fn destination(&self, _context: DestinationContext) -> Result<Box<dyn Destination>> {
            Ok(Box::new(DestClone(Arc::clone(&self.dest))))
        }

        async fn source(&self, _context: SourceContext) -> Result<Box<dyn Source>> {
            Ok(Box::new(DestClone(Arc::clone(&self.dest))))
        }

        async fn worker(&self, _context: WorkerContext) -> Result<Box<dyn JobHandler>> {
            Ok(Box::new(SlowHandler))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_error_preserves_internal_when_message_says_not_found() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(64 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                let plugin = Arc::new(TestPlugin {
                    dest: Arc::new(MemDest {
                        store: Mutex::new(HashMap::new()),
                    }),
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let dest = client
                    .destination(DestinationContext::default())
                    .await
                    .expect("dest");
                let err = dest.head("internal-msg").await.expect_err("must fail");
                assert_eq!(err.code, PluginErrorCode::Internal);
                assert!(err.message.contains("not_found"));
                let unknown = dest.head("unknown-code").await.expect_err("unknown");
                assert_eq!(unknown.code, PluginErrorCode::Unknown);
                assert_eq!(unknown.wire_str(), "future_retry_policy");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_page_overflow_is_payload_too_large() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(256 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                let plugin = Arc::new(TestPlugin {
                    dest: Arc::new(MemDest {
                        store: Mutex::new(HashMap::new()),
                    }),
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let dest = client
                    .destination(DestinationContext::default())
                    .await
                    .expect("dest");
                let err = dest
                    .list(ListOptions::default())
                    .await
                    .expect_err("oversize page");
                assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mid_stream_source_failure_is_not_eof() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(64 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                let plugin = Arc::new(TestPlugin {
                    dest: Arc::new(MemDest {
                        store: Mutex::new(HashMap::new()),
                    }),
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let dest = client
                    .destination(DestinationContext::default())
                    .await
                    .expect("dest");
                dest.put(
                    "keep",
                    Box::pin(std::io::Cursor::new(b"original".to_vec())),
                    WriteOptions::default(),
                )
                .await
                .expect("seed");
                dest.put(
                    "keep",
                    Box::pin(FailAfter { remain: 8 }),
                    WriteOptions {
                        content_length: Some(100),
                        ..Default::default()
                    },
                )
                .await
                .expect_err("failing put must not publish");
                let got = dest.get("fail-mid", None).await.expect("open fail-mid");
                let mut body = got.body;
                let mut buf = Vec::new();
                let err = body.read_to_end(&mut buf).await.expect_err("must not eof");
                assert_ne!(err.kind(), std::io::ErrorKind::UnexpectedEof);
                let keep = dest.get("keep", None).await.expect("keep");
                let mut out = Vec::new();
                let mut body = keep.body;
                body.read_to_end(&mut out).await.unwrap();
                assert_eq!(out, b"original");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lease_loss_stops_attempt_a_before_commit() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(64 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                let store = Arc::new(MemDest {
                    store: Mutex::new(HashMap::new()),
                });
                let plugin = Arc::new(LeasePlugin {
                    dest: Arc::clone(&store),
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let handler = client
                    .worker(WorkerContext {
                        job_id: "lease".into(),
                        ..Default::default()
                    })
                    .await
                    .expect("worker");
                let granted = Arc::new(DestClone(Arc::clone(&store)));
                let flag = Arc::new(AtomicBool::new(false));
                let cancel_flag = Arc::clone(&flag);
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    cancel_flag.store(true, Ordering::SeqCst);
                });
                let outcome = tokio::time::timeout(
                    Duration::from_secs(10),
                    client.handle_job_with_cancel(
                        handler,
                        JobInvocation::stream_copy("lease", "{}"),
                        granted.clone() as Arc<dyn Source>,
                        granted as Arc<dyn Destination>,
                        Arc::new(NoopProgress),
                        Arc::new(TestCancel(flag)),
                    ),
                )
                .await
                .expect("lease-loss timed out")
                .expect("handle");
                match outcome {
                    JobOutcome::Cancelled { message } => {
                        assert!(message.contains("fence"), "{message}");
                    }
                    other => panic!("expected cancelled, got {other:?}"),
                }
                assert!(
                    store.store.lock().expect("lock").get("from-a").is_none(),
                    "attempt A must not commit after fence loss"
                );
            })
            .await;
    }
}
