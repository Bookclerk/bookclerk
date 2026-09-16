//! Cap'n Proto two-party RPC adapters for plugin ABI role classes.
//!
//! Public types remain [`crate::Destination`] / [`crate::ByteRange`] streams.
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

use crate::generated::{
    read_authenticate_user_params, read_brand, read_catalog_detail_params,
    read_catalog_detail_reply, read_catalog_hits_reply, read_cli_invoke_params,
    read_cli_invoke_reply, read_cli_schema, read_cli_schema_reply, read_config_option,
    read_database_adapter_config, read_diagnose_reply, read_event_poll_reply,
    read_expand_candidates_params, read_external_user_reply, read_fetch_title_params,
    read_fetch_title_reply, read_list_deals_params, read_login_complete_params, read_login_params,
    read_login_reply, read_login_start_reply, read_purchase_hint_params, read_purchase_hint_reply,
    read_scan_library_params, read_scan_params, read_scan_reply, read_search_catalog_params,
    read_source_accounts_reply, read_sync_listening_reply, write_authenticate_user_params,
    write_brand, write_catalog_detail_params, write_catalog_detail_reply, write_catalog_hits_reply,
    write_cli_invoke_params, write_cli_invoke_reply, write_cli_schema, write_cli_schema_reply,
    write_config_option, write_database_adapter_config, write_diagnose_reply,
    write_event_poll_reply, write_expand_candidates_params, write_external_user_reply,
    write_fetch_title_params, write_fetch_title_reply, write_list_deals_params,
    write_login_complete_params, write_login_params, write_login_reply, write_login_start_reply,
    write_purchase_hint_params, write_purchase_hint_reply, write_scan_library_params,
    write_scan_params, write_scan_reply, write_search_catalog_params, write_source_accounts_reply,
    write_sync_listening_reply, CatalogDetail, CatalogHits, DiagnoseResult, EventPollResult,
    PurchaseHintResult, SourceAccounts, SyncListeningResult,
};
#[cfg(feature = "host")]
use crate::host_roles::HostAdapterDatabaseSession;
use crate::limits::{
    ScalarLimits, MAX_EVENT_PAYLOAD_BYTES, MAX_LIST_PAGE, MAX_PLUGIN_MIGRATION_OPS,
    MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES, MAX_PLUGIN_MIGRATION_TOTAL_OPS, MAX_SCALAR_BYTES,
    MAX_STREAM_WINDOW_BYTES,
};
use crate::plugin_capnp::{
    adapter_database_session as adapter_database_session_capnp, adapter_session_reply,
    bookclerk_plugin, byte_source, cancellation, content_source as content_source_capnp,
    content_source_reply, copy_reply, database as database_capnp, database_reply, describe_reply,
    destination as dest_iface, destination_reply, domain_event, empty_reply,
    event_result as event_result_capnp, event_result_reply, get_reply,
    guest_database as guest_database_capnp, handle_reply, head_reply, health_reply,
    integration as integration_capnp, integration_reply, job_handler, job_invocation, job_outcome,
    list_reply, object_metadata, oidc_client_template, oidc_clients_reply, open_reply,
    plugin_describe, plugin_error, plugin_migration, plugin_migration_op, plugin_migrations_ok,
    plugin_migrations_reply, progress_sink, pull_reply, put_reply, source as source_capnp,
    source_reply, worker_reply, write_options,
};
#[cfg(feature = "host")]
use crate::plugin_host_capnp::host_adapter_database_session as host_adapter_database_session_capnp;
use crate::roles::{
    AdapterDatabaseSession, ByteRange, Cancellation, ContentSource, ContentSourceContext, Database,
    DatabaseContext, Destination, GuestDatabase, Integration, IntegrationContext, JobHandler,
    JobHandlerContext, NeverCancel, PluginRoot, ProgressSink, ReadResult, Source,
};
use crate::rpc_types::{
    CopyResult, DestinationContext, DomainEvent, EventResult, HealthOk, JobCheckpoint,
    JobInvocation, JobOutcome, ListOptions, ListPage, ObjectInfo, ObjectMetadata,
    OidcClientTemplate, PluginDescribe, PutResult, SourceContext, WorkerContext, WriteOptions,
    MAX_CHECKPOINT_BYTES,
};
use crate::{
    capnp_u32_len, require_plugin_migration_registration, AuthenticateUserParams,
    CatalogDetailParams, CatalogHit, CliInvokeParams, CliInvokeResult, CliSchema,
    ExpandCandidatesParams, ExternalUser, FetchTitleParams, ListDealsParams, ListeningProgress,
    LoginCompleteParams, LoginParams, LoginResult, LoginStartResult, PlainFetch, PluginError,
    PluginMigration, PluginMigrationOp, PurchaseHint, PurchaseHintParams, Result,
    ScanLibraryParams, ScanParams, ScanSummary, SearchCatalogParams, SourceAccount,
    MAX_PLUGIN_MIGRATION_ID_BYTES,
};

pub(super) fn from_capnp(err: impl std::fmt::Display) -> PluginError {
    PluginError::unavailable(err.to_string())
}

pub(super) fn text_of(r: capnp::text::Reader<'_>) -> String {
    r.to_string().unwrap_or_default()
}

pub(super) fn read_extensible_config(
    r: crate::plugin_capnp::extensible_config::Reader<'_>,
) -> crate::ExtensibleConfig {
    crate::ExtensibleConfig {
        schema_version: r.get_schema_version(),
        media_type: r.get_media_type().ok().map(text_of).unwrap_or_default(),
        payload: r.get_payload().ok().map(|d| d.to_vec()).unwrap_or_default(),
    }
}

pub(super) fn write_extensible_config(
    mut b: crate::plugin_capnp::extensible_config::Builder<'_>,
    cfg: &crate::ExtensibleConfig,
) {
    b.set_schema_version(cfg.schema_version);
    b.set_media_type(&cfg.media_type);
    b.set_payload(&cfg.payload);
}

pub(super) fn write_error(mut b: plugin_error::Builder<'_>, err: &PluginError) {
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

fn fill_put_result(mut out: crate::plugin_capnp::put_result::Builder<'_>, put: &PutResult) {
    out.set_key(&put.key);
    out.set_bytes_written(put.bytes_written);
    if let Some(etag) = &put.etag {
        out.set_etag(etag);
    }
    if let Some(sum) = &put.sha256 {
        out.set_sha256(sum);
    }
}

pub(super) fn read_error(r: plugin_error::Reader<'_>) -> PluginError {
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
    fill_text_list(
        b.reborrow()
            .init_supported_roles(u32_len(d.supported_roles.len())?),
        &d.supported_roles,
    );
    fill_text_list(
        b.reborrow()
            .init_capabilities(u32_len(d.capabilities.len())?),
        &d.capabilities,
    );
    b.set_portal_auth_mode(d.portal_auth_mode.into());
    if let Some(env) = &d.password_env_var {
        b.set_password_env_var(env);
    }
    fill_text_list(
        b.reborrow().init_aliases(u32_len(d.aliases.len())?),
        &d.aliases,
    );
    b.set_sort_key(d.sort_key);
    if let Some(brand) = &d.brand {
        write_brand(b.reborrow().init_brand(), brand)?;
    }
    {
        let mut opts = b
            .reborrow()
            .init_config_options(u32_len(d.config_options.len())?);
        for (i, opt) in d.config_options.iter().enumerate() {
            write_config_option(opts.reborrow().get(i as u32), opt)?;
        }
    }
    write_cli_schema(b.reborrow().init_cli(), &d.cli)?;
    Ok(())
}

/// Cap'n Proto list lengths are `u32`.
///
/// # Errors
///
/// Returns when `len` exceeds `u32::MAX`.
fn u32_len(len: usize) -> capnp::Result<u32> {
    u32::try_from(len)
        .map_err(|_| capnp::Error::failed(format!("list length {len} exceeds UInt32")))
}

/// Copy `items` into an already-sized `List(Text)` builder.
fn fill_text_list(mut list: capnp::text_list::Builder<'_>, items: &[String]) {
    for (i, item) in items.iter().enumerate() {
        list.set(i as u32, item);
    }
}

/// Owned strings of a `List(Text)` reader.
///
/// # Errors
///
/// Returns a Cap'n Proto error when an element is not valid UTF-8.
fn read_text_list(list: capnp::text_list::Reader<'_>) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        out.push(text_of(item.map_err(from_capnp)?));
    }
    Ok(out)
}

/// Decode [`PluginDescribe`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns [`PluginError`] when a text or nested field cannot be read.
fn read_describe(m: plugin_describe::Reader<'_>) -> Result<PluginDescribe> {
    let lim = m.get_scalar_limits().map_err(from_capnp)?;
    let brand = if m.has_brand() {
        let brand = read_brand(m.get_brand().map_err(from_capnp)?).map_err(from_capnp)?;
        if brand.id.is_empty() {
            None
        } else {
            Some(brand)
        }
    } else {
        None
    };
    let config_options = {
        let list = m.get_config_options().map_err(from_capnp)?;
        let mut out = Vec::with_capacity(list.len() as usize);
        for item in list.iter() {
            out.push(read_config_option(item).map_err(from_capnp)?);
        }
        out
    };
    let cli = if m.has_cli() {
        read_cli_schema(m.get_cli().map_err(from_capnp)?).map_err(from_capnp)?
    } else {
        CliSchema::default()
    };
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
        rpc_features: read_text_list(m.get_rpc_features().map_err(from_capnp)?)?,
        scalar_limits: crate::rpc_types::ScalarLimitsDto {
            max_scalar_bytes: lim.get_max_scalar_bytes(),
            max_stream_window_bytes: lim.get_max_stream_window_bytes(),
            max_list_page: lim.get_max_list_page(),
        },
        supported_roles: read_text_list(m.get_supported_roles().map_err(from_capnp)?)?,
        capabilities: read_text_list(m.get_capabilities().map_err(from_capnp)?)?,
        portal_auth_mode: m.get_portal_auth_mode().map(Into::into).unwrap_or_default(),
        password_env_var: {
            let v = text_of(m.get_password_env_var().map_err(from_capnp)?);
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        },
        aliases: read_text_list(m.get_aliases().map_err(from_capnp)?)?,
        sort_key: m.get_sort_key(),
        brand,
        config_options,
        cli,
    })
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
    page: crate::plugin_capnp::list_page::Reader<'_>,
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
            config: read_extensible_config(c.get_config()?),
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
            config: read_extensible_config(c.get_config()?),
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
            config: read_extensible_config(c.get_config()?),
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
            config: read_extensible_config(c.get_config()?),
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
            config: read_extensible_config(c.get_config()?),
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
            config: read_extensible_config(c.get_config()?),
            adapter: read_database_adapter_config(c.get_adapter()?)?,
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
        let outcome = self.inner.cli_describe().await;
        write_cli_schema_reply(results.get().init_result(), &outcome)
    }

    async fn cli_invoke(
        self: Rc<Self>,
        params: bookclerk_plugin::CliInvokeParams,
        mut results: bookclerk_plugin::CliInvokeResults,
    ) -> capnp::Result<()> {
        let params = read_cli_invoke_params(params.get()?.get_params()?)?;
        let outcome = self.inner.cli_invoke(params).await;
        write_cli_invoke_reply(results.get().init_result(), &outcome)
    }

    async fn oidc_clients(
        self: Rc<Self>,
        _params: bookclerk_plugin::OidcClientsParams,
        mut results: bookclerk_plugin::OidcClientsResults,
    ) -> capnp::Result<()> {
        let result = results.get().init_result();
        match self.inner.oidc_clients().await {
            Ok(clients) => fill_oidc_clients(result.init_ok(), &clients)?,
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn database_migrations(
        self: Rc<Self>,
        params: bookclerk_plugin::DatabaseMigrationsParams,
        mut results: bookclerk_plugin::DatabaseMigrationsResults,
    ) -> capnp::Result<()> {
        let binding = params
            .get()?
            .get_binding()
            .ok()
            .map(text_of)
            .unwrap_or_default();
        let result = results.get().init_result();
        match self.inner.database_migrations(&binding).await {
            Ok(migrations) => match require_plugin_migration_registration(&migrations) {
                Ok(()) => {
                    if let Err(err) = fill_plugin_migrations(result.init_ok(), &migrations) {
                        return Err(capnp::Error::failed(err.to_string()));
                    }
                }
                Err(err) => write_error(result.init_err(), &err),
            },
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

/// Encode plugin OIDC client templates into a Cap'n Proto `oidcClients` ok payload.
///
/// # Errors
///
/// Returns a Cap'n Proto error when the `clients` list cannot be initialized.
fn fill_oidc_clients(
    mut ok: crate::plugin_capnp::oidc_clients_ok::Builder<'_>,
    clients: &[OidcClientTemplate],
) -> capnp::Result<()> {
    let mut list = ok.reborrow().init_clients(clients.len() as u32);
    for (i, tmpl) in clients.iter().enumerate() {
        fill_oidc_client_template(list.reborrow().get(i as u32), tmpl);
    }
    Ok(())
}

/// Encode one [`OidcClientTemplate`] onto a Cap'n Proto builder.
fn fill_oidc_client_template(mut b: oidc_client_template::Builder<'_>, tmpl: &OidcClientTemplate) {
    b.set_client_id(&tmpl.client_id);
    b.set_display_name(&tmpl.display_name);
    b.set_callback_path(&tmpl.callback_path);
    b.set_public_client(tmpl.public_client);
    {
        let mut scopes = b
            .reborrow()
            .init_default_scopes(tmpl.default_scopes.len() as u32);
        for (i, scope) in tmpl.default_scopes.iter().enumerate() {
            scopes.set(i as u32, scope);
        }
    }
    b.set_issue_refresh_token(tmpl.issue_refresh_token);
    b.set_origin_config_key(&tmpl.origin_config_key);
}

/// Decode one OIDC client template from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns [`PluginError`] when a text or list field cannot be read.
fn read_oidc_client_template(r: oidc_client_template::Reader<'_>) -> Result<OidcClientTemplate> {
    let scopes = r.get_default_scopes().map_err(from_capnp)?;
    let mut default_scopes = Vec::new();
    for scope in scopes.iter() {
        default_scopes.push(scope.map_err(from_capnp)?.to_string().unwrap_or_default());
    }
    Ok(OidcClientTemplate {
        client_id: text_of(r.get_client_id().map_err(from_capnp)?),
        display_name: text_of(r.get_display_name().map_err(from_capnp)?),
        callback_path: text_of(r.get_callback_path().map_err(from_capnp)?),
        public_client: r.get_public_client(),
        default_scopes,
        issue_refresh_token: r.get_issue_refresh_token(),
        origin_config_key: text_of(r.get_origin_config_key().map_err(from_capnp)?),
    })
}

/// Encode plugin-owned migrations into a Cap'n Proto `databaseMigrations` ok payload.
///
/// Callers must already have passed [`require_plugin_migration_registration`].
/// List lengths use checked `u32` conversion.
///
/// # Errors
///
/// Returns [`PluginError::payload_too_large`] when a list length cannot fit in
/// Cap'n Proto `UInt32`.
fn fill_plugin_migrations(
    mut ok: plugin_migrations_ok::Builder<'_>,
    migrations: &[PluginMigration],
) -> Result<()> {
    let n = capnp_u32_len(migrations.len(), "plugin migrations")?;
    let mut list = ok.reborrow().init_migrations(n);
    for (i, migration) in migrations.iter().enumerate() {
        let idx = capnp_u32_len(i, "plugin migration index")?;
        fill_plugin_migration(list.reborrow().get(idx), migration)?;
    }
    Ok(())
}

/// Encode one [`PluginMigration`] onto a Cap'n Proto builder.
///
/// # Errors
///
/// Returns [`PluginError::payload_too_large`] when the operations list length
/// cannot fit in Cap'n Proto `UInt32`.
fn fill_plugin_migration(
    mut b: plugin_migration::Builder<'_>,
    migration: &PluginMigration,
) -> Result<()> {
    b.set_id(&migration.id);
    let n = capnp_u32_len(migration.operations.len(), "plugin migration operations")?;
    let mut ops = b.reborrow().init_operations(n);
    for (i, op) in migration.operations.iter().enumerate() {
        let idx = capnp_u32_len(i, "plugin migration operation index")?;
        let mut slot = ops.reborrow().get(idx);
        match op {
            PluginMigrationOp::Schema(sql) => slot.set_schema(sql),
            PluginMigrationOp::Data(sql) => slot.set_data(sql),
        }
    }
    Ok(())
}

/// Decode plugin-owned migrations from a Cap'n Proto `databaseMigrations` ok payload.
///
/// Rejects oversize lists and SQL text before allocating the corresponding
/// `String` / completing the `Vec`.
///
/// # Errors
///
/// Returns [`PluginError::payload_too_large`] when a list, SQL text, or the
/// aggregate registration exceeds the ABI limits.
fn read_plugin_migrations(r: plugin_migrations_ok::Reader<'_>) -> Result<Vec<PluginMigration>> {
    let list = r.get_migrations().map_err(from_capnp)?;
    if list.len() > MAX_LIST_PAGE {
        return Err(PluginError::payload_too_large(format!(
            "plugin migration count {} exceeds maxListPage ({MAX_LIST_PAGE})",
            list.len()
        )));
    }
    let cap = usize::try_from(list.len()).unwrap_or(0);
    let mut out = Vec::with_capacity(cap);
    let mut total = 0usize;
    let mut total_ops = 0usize;
    let max_reg = usize::try_from(MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES).unwrap_or(0);
    for item in list.iter() {
        out.push(read_plugin_migration(
            item,
            &mut total,
            &mut total_ops,
            max_reg,
        )?);
    }
    require_plugin_migration_registration(&out)?;
    Ok(out)
}

/// Decode one plugin-owned migration from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns [`PluginError`] when the id or `operations` list cannot be read, or
/// a size limit is exceeded.
fn read_plugin_migration(
    r: plugin_migration::Reader<'_>,
    total: &mut usize,
    total_ops: &mut usize,
    max_reg: usize,
) -> Result<PluginMigration> {
    let id_text = r.get_id().map_err(from_capnp)?;
    let id_len = id_text.as_bytes().len();
    if id_len > MAX_PLUGIN_MIGRATION_ID_BYTES {
        return Err(PluginError::payload_too_large(format!(
            "plugin migration id is {id_len} bytes; maximum is {MAX_PLUGIN_MIGRATION_ID_BYTES}"
        )));
    }
    *total = total.saturating_add(id_len);
    if *total > max_reg {
        return Err(PluginError::payload_too_large(format!(
            "plugin migration registration is {total} bytes; exceeds \
             maxPluginMigrationRegistrationBytes ({max_reg})"
        )));
    }
    let id = text_of(id_text);
    let ops = r.get_operations().map_err(from_capnp)?;
    if ops.len() > MAX_PLUGIN_MIGRATION_OPS {
        return Err(PluginError::payload_too_large(format!(
            "plugin migration `{id}` has {} operations; exceeds maxPluginMigrationOps ({MAX_PLUGIN_MIGRATION_OPS})",
            ops.len()
        )));
    }
    let n_ops = usize::try_from(ops.len()).unwrap_or(0);
    *total_ops = total_ops.saturating_add(n_ops);
    if *total_ops > usize::try_from(MAX_PLUGIN_MIGRATION_TOTAL_OPS).unwrap_or(0) {
        return Err(PluginError::payload_too_large(format!(
            "plugin migration registration has {total_ops} operations; exceeds \
             maxPluginMigrationTotalOps ({MAX_PLUGIN_MIGRATION_TOTAL_OPS})"
        )));
    }
    let mut operations = Vec::with_capacity(n_ops);
    let max_sql = usize::try_from(MAX_SCALAR_BYTES).unwrap_or(0);
    for op in ops.iter() {
        operations.push(read_plugin_migration_op(op, &id, total, max_sql, max_reg)?);
    }
    Ok(PluginMigration { id, operations })
}

/// Decode one already-separated plugin migration operation from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns [`PluginError`] when the operation union or SQL text cannot be read,
/// or the SQL / aggregate size limit is exceeded.
fn read_plugin_migration_op(
    r: plugin_migration_op::Reader<'_>,
    id: &str,
    total: &mut usize,
    max_sql: usize,
    max_reg: usize,
) -> Result<PluginMigrationOp> {
    let (is_schema, sql_text) = match r.which().map_err(from_capnp)? {
        plugin_migration_op::Schema(sql) => (true, sql.map_err(from_capnp)?),
        plugin_migration_op::Data(sql) => (false, sql.map_err(from_capnp)?),
    };
    let n = sql_text.as_bytes().len();
    if n > max_sql {
        return Err(PluginError::payload_too_large(format!(
            "plugin migration `{id}` SQL is {n} bytes; exceeds maxScalarBytes ({max_sql})"
        )));
    }
    *total = total.saturating_add(n);
    if *total > max_reg {
        return Err(PluginError::payload_too_large(format!(
            "plugin migration registration is {total} bytes; exceeds \
             maxPluginMigrationRegistrationBytes ({max_reg})"
        )));
    }
    let sql = text_of(sql_text);
    if is_schema {
        Ok(PluginMigrationOp::Schema(sql))
    } else {
        Ok(PluginMigrationOp::Data(sql))
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
        EventResult::Suspended {
            checkpoint_json,
            checkpoint_schema_version,
            wake_at_unix_ms,
            wake_on_event_type,
            wake_on_filter_json,
        } => {
            let mut s = b.init_suspended();
            s.set_checkpoint_json(checkpoint_json);
            s.set_checkpoint_schema_version(*checkpoint_schema_version);
            s.set_wake_at_unix_ms(*wake_at_unix_ms);
            s.set_wake_on_event_type(wake_on_event_type);
            s.set_wake_on_filter_json(wake_on_filter_json);
        }
    }
}

/// Decode a [`DomainEvent`] from Cap'n Proto.
///
/// # Errors
///
/// Returns when a text or data field cannot be read from the message.
fn read_domain_event(r: domain_event::Reader<'_>) -> Result<DomainEvent> {
    let payload = r.get_payload().map_err(from_capnp)?.to_vec();
    if payload.len() > MAX_EVENT_PAYLOAD_BYTES as usize {
        return Err(PluginError::payload_too_large(format!(
            "domain event payload of {} bytes exceeds {MAX_EVENT_PAYLOAD_BYTES}",
            payload.len()
        )));
    }
    let checkpoint_json = text_of(r.get_checkpoint_json().map_err(from_capnp)?);
    if checkpoint_json.len() > MAX_CHECKPOINT_BYTES as usize {
        return Err(PluginError::payload_too_large(format!(
            "checkpoint of {} bytes exceeds {MAX_CHECKPOINT_BYTES}",
            checkpoint_json.len()
        )));
    }
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
        payload,
        checkpoint_json,
        checkpoint_schema_version: r.get_checkpoint_schema_version(),
        invocation_sequence: r.get_invocation_sequence(),
        resume_pending: r.get_resume_pending(),
        source: text_of(r.get_source().map_err(from_capnp)?),
    })
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
        let params = read_login_params(params.get()?.get_params()?)?;
        let outcome = self.inner.login(params).await;
        write_login_reply(results.get().init_result(), &outcome)
    }

    async fn scan(
        self: Rc<Self>,
        params: content_source_capnp::ScanParams,
        mut results: content_source_capnp::ScanResults,
    ) -> capnp::Result<()> {
        let params = read_scan_params(params.get()?.get_params()?)?;
        let outcome = self.inner.scan(params).await;
        write_scan_reply(results.get().init_result(), &outcome)
    }

    async fn fetch_title(
        self: Rc<Self>,
        params: content_source_capnp::FetchTitleParams,
        mut results: content_source_capnp::FetchTitleResults,
    ) -> capnp::Result<()> {
        let params = read_fetch_title_params(params.get()?.get_params()?)?;
        let outcome = self.inner.fetch_title(params).await;
        write_fetch_title_reply(results.get().init_result(), &outcome)
    }

    async fn list_accounts(
        self: Rc<Self>,
        _params: content_source_capnp::ListAccountsParams,
        mut results: content_source_capnp::ListAccountsResults,
    ) -> capnp::Result<()> {
        let outcome = self
            .inner
            .list_accounts()
            .await
            .map(|accounts| SourceAccounts { accounts });
        write_source_accounts_reply(results.get().init_result(), &outcome)
    }

    async fn login_start(
        self: Rc<Self>,
        params: content_source_capnp::LoginStartParams,
        mut results: content_source_capnp::LoginStartResults,
    ) -> capnp::Result<()> {
        let params = read_login_params(params.get()?.get_params()?)?;
        let outcome = self.inner.login_start(params).await;
        write_login_start_reply(results.get().init_result(), &outcome)
    }

    async fn login_complete(
        self: Rc<Self>,
        params: content_source_capnp::LoginCompleteParams,
        mut results: content_source_capnp::LoginCompleteResults,
    ) -> capnp::Result<()> {
        let params = read_login_complete_params(params.get()?.get_params()?)?;
        let outcome = self.inner.login_complete(params).await;
        write_login_reply(results.get().init_result(), &outcome)
    }

    async fn search_catalog(
        self: Rc<Self>,
        params: content_source_capnp::SearchCatalogParams,
        mut results: content_source_capnp::SearchCatalogResults,
    ) -> capnp::Result<()> {
        let params = read_search_catalog_params(params.get()?.get_params()?)?;
        let outcome = self
            .inner
            .search_catalog(params)
            .await
            .map(|hits| CatalogHits { hits });
        write_catalog_hits_reply(results.get().init_result(), &outcome)
    }

    async fn expand_candidates(
        self: Rc<Self>,
        params: content_source_capnp::ExpandCandidatesParams,
        mut results: content_source_capnp::ExpandCandidatesResults,
    ) -> capnp::Result<()> {
        let params = read_expand_candidates_params(params.get()?.get_params()?)?;
        let outcome = self
            .inner
            .expand_candidates(params)
            .await
            .map(|hits| CatalogHits { hits });
        write_catalog_hits_reply(results.get().init_result(), &outcome)
    }

    async fn purchase_hint(
        self: Rc<Self>,
        params: content_source_capnp::PurchaseHintParams,
        mut results: content_source_capnp::PurchaseHintResults,
    ) -> capnp::Result<()> {
        let params = read_purchase_hint_params(params.get()?.get_params()?)?;
        let outcome = self
            .inner
            .purchase_hint(params)
            .await
            .map(|hint| PurchaseHintResult {
                found: hint.is_some(),
                hint: hint.unwrap_or_default(),
            });
        write_purchase_hint_reply(results.get().init_result(), &outcome)
    }

    async fn list_deals(
        self: Rc<Self>,
        params: content_source_capnp::ListDealsParams,
        mut results: content_source_capnp::ListDealsResults,
    ) -> capnp::Result<()> {
        let params = read_list_deals_params(params.get()?.get_params()?)?;
        let outcome = self
            .inner
            .list_deals(params)
            .await
            .map(|hits| CatalogHits { hits });
        write_catalog_hits_reply(results.get().init_result(), &outcome)
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
        let outcome = self
            .inner
            .diagnose()
            .await
            .map(|lines| DiagnoseResult { lines });
        write_diagnose_reply(results.get().init_result(), &outcome)
    }

    async fn catalog_detail(
        self: Rc<Self>,
        params: content_source_capnp::CatalogDetailParams,
        mut results: content_source_capnp::CatalogDetailResults,
    ) -> capnp::Result<()> {
        let params = read_catalog_detail_params(params.get()?.get_params()?)?;
        let outcome = self
            .inner
            .catalog_detail(params)
            .await
            .map(|hit| CatalogDetail {
                found: hit.is_some(),
                hit: hit.unwrap_or_default(),
            });
        write_catalog_detail_reply(results.get().init_result(), &outcome)
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
        let event = match params.get()?.get_event() {
            Ok(r) => match read_domain_event(r) {
                Ok(event) => event,
                Err(err) => {
                    write_error(results.get().init_result().init_err(), &err);
                    return Ok(());
                }
            },
            Err(err) => return Err(capnp::Error::failed(err.to_string())),
        };
        let outcome = match self.inner.on_event(event).await {
            Ok(EventResult::Suspended {
                checkpoint_json, ..
            }) if checkpoint_json.len() > MAX_CHECKPOINT_BYTES as usize => {
                Err(PluginError::payload_too_large(format!(
                    "checkpoint of {} bytes exceeds {MAX_CHECKPOINT_BYTES}",
                    checkpoint_json.len()
                )))
            }
            other => other,
        };
        let result = results.get().init_result();
        match outcome {
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
        let outcome = self
            .inner
            .diagnose()
            .await
            .map(|lines| DiagnoseResult { lines });
        write_diagnose_reply(results.get().init_result(), &outcome)
    }

    async fn scan_library(
        self: Rc<Self>,
        params: integration_capnp::ScanLibraryParams,
        mut results: integration_capnp::ScanLibraryResults,
    ) -> capnp::Result<()> {
        let params = read_scan_library_params(params.get()?.get_params()?)?;
        let mut result = results.get().init_result();
        match self.inner.scan_library(params).await {
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
        let outcome = self
            .inner
            .sync_listening()
            .await
            .map(|items| SyncListeningResult { items });
        write_sync_listening_reply(results.get().init_result(), &outcome)
    }

    async fn authenticate_user(
        self: Rc<Self>,
        params: integration_capnp::AuthenticateUserParams,
        mut results: integration_capnp::AuthenticateUserResults,
    ) -> capnp::Result<()> {
        let params = read_authenticate_user_params(params.get()?.get_params()?)?;
        let outcome = self.inner.authenticate_user(params).await;
        write_external_user_reply(results.get().init_result(), &outcome)
    }

    async fn poll_events(
        self: Rc<Self>,
        _params: integration_capnp::PollEventsParams,
        mut results: integration_capnp::PollEventsResults,
    ) -> capnp::Result<()> {
        let outcome = self
            .inner
            .poll_events()
            .await
            .map(|users| EventPollResult { users });
        write_event_poll_reply(results.get().init_result(), &outcome)
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
                let client: adapter_database_session_capnp::Client =
                    capnp_rpc::new_client(AdapterDatabaseSessionServer {
                        inner: Arc::from(session),
                        #[cfg(feature = "host")]
                        host: self.inner.host_session().map(Arc::from),
                    });
                result.set_ok(client);
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

struct AdapterDatabaseSessionServer {
    inner: Arc<dyn AdapterDatabaseSession>,
    #[cfg(feature = "host")]
    host: Option<Arc<dyn HostAdapterDatabaseSession>>,
}

impl adapter_database_session_capnp::Server for AdapterDatabaseSessionServer {
    async fn capabilities(
        self: Rc<Self>,
        _params: adapter_database_session_capnp::CapabilitiesParams,
        mut results: adapter_database_session_capnp::CapabilitiesResults,
    ) -> capnp::Result<()> {
        crate::db_rpc::write_db_capabilities_reply(
            results.get().init_result(),
            self.inner.capabilities().await,
        );
        Ok(())
    }

    async fn execute(
        self: Rc<Self>,
        params: adapter_database_session_capnp::ExecuteParams,
        mut results: adapter_database_session_capnp::ExecuteResults,
    ) -> capnp::Result<()> {
        let request = params
            .get()?
            .get_request()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                crate::db_rpc::read_adapter_execute_request(r)
                    .map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        crate::db_rpc::write_execute_result_reply(
            results.get().init_result(),
            self.inner.execute(request).await,
        );
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: adapter_database_session_capnp::CloseParams,
        mut results: adapter_database_session_capnp::CloseResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.close().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn bootstrap(
        self: Rc<Self>,
        _params: adapter_database_session_capnp::BootstrapParams,
        mut results: adapter_database_session_capnp::BootstrapResults,
    ) -> capnp::Result<()> {
        crate::db_rpc::write_db_bootstrap_reply(
            results.get().init_result(),
            self.inner.bootstrap().await,
        );
        Ok(())
    }

    async fn export_identity(
        self: Rc<Self>,
        _params: adapter_database_session_capnp::ExportIdentityParams,
        mut results: adapter_database_session_capnp::ExportIdentityResults,
    ) -> capnp::Result<()> {
        crate::db_rpc::write_identity_export_reply(
            results.get().init_result(),
            self.inner.export_identity().await,
        );
        Ok(())
    }

    async fn import_identity(
        self: Rc<Self>,
        params: adapter_database_session_capnp::ImportIdentityParams,
        mut results: adapter_database_session_capnp::ImportIdentityResults,
    ) -> capnp::Result<()> {
        let rows = params
            .get()?
            .get_rows()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                crate::db_rpc::read_identity_list(r)
                    .map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        let mut result = results.get().init_result();
        match self.inner.import_identity(&rows).await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn list_user_relations(
        self: Rc<Self>,
        _params: adapter_database_session_capnp::ListUserRelationsParams,
        mut results: adapter_database_session_capnp::ListUserRelationsResults,
    ) -> capnp::Result<()> {
        crate::db_rpc::write_user_relations_reply(
            results.get().init_result(),
            self.inner.list_user_relations().await,
        );
        Ok(())
    }

    async fn prepare_unit_restore(
        self: Rc<Self>,
        _params: adapter_database_session_capnp::PrepareUnitRestoreParams,
        mut results: adapter_database_session_capnp::PrepareUnitRestoreResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.prepare_unit_restore().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn drop_user_relations(
        self: Rc<Self>,
        params: adapter_database_session_capnp::DropUserRelationsParams,
        mut results: adapter_database_session_capnp::DropUserRelationsResults,
    ) -> capnp::Result<()> {
        let names = params
            .get()?
            .get_names()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                crate::db_rpc::read_text_list(r)
                    .map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        let mut result = results.get().init_result();
        match self.inner.drop_user_relations(&names).await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn assert_restore_constraints(
        self: Rc<Self>,
        _params: adapter_database_session_capnp::AssertRestoreConstraintsParams,
        mut results: adapter_database_session_capnp::AssertRestoreConstraintsResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.assert_restore_constraints().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

#[cfg(feature = "host")]
impl host_adapter_database_session_capnp::Server for AdapterDatabaseSessionServer {
    async fn begin(
        self: Rc<Self>,
        params: host_adapter_database_session_capnp::BeginParams,
        mut results: host_adapter_database_session_capnp::BeginResults,
    ) -> capnp::Result<()> {
        let isolation = params
            .get()?
            .get_isolation()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|iso| {
                crate::db_rpc::read_isolation(iso)
                    .map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        let mut result = results.get().init_result();
        match &self.host {
            Some(host) => match host.begin(isolation).await {
                Ok(txn) => {
                    result.set_ok(crate::host_rpc::new_adapter_transaction_client(Arc::from(
                        txn,
                    )));
                }
                Err(err) => write_error(result.init_err(), &err),
            },
            None => write_error(
                result.init_err(),
                &PluginError::unsupported("interactive adapter transactions"),
            ),
        }
        Ok(())
    }

    async fn execute(
        self: Rc<Self>,
        params: host_adapter_database_session_capnp::ExecuteParams,
        mut results: host_adapter_database_session_capnp::ExecuteResults,
    ) -> capnp::Result<()> {
        let request = params
            .get()?
            .get_request()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                crate::db_rpc::read_adapter_execute_request(r)
                    .map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        crate::db_rpc::write_execute_result_reply(
            results.get().init_result(),
            match &self.host {
                Some(host) => host.execute(request).await,
                None => Err(PluginError::unsupported("host execute not implemented")),
            },
        );
        Ok(())
    }
}

struct GuestDatabaseServer {
    inner: Arc<dyn GuestDatabase>,
}

impl guest_database_capnp::Server for GuestDatabaseServer {
    async fn execute(
        self: Rc<Self>,
        params: guest_database_capnp::ExecuteParams,
        mut results: guest_database_capnp::ExecuteResults,
    ) -> capnp::Result<()> {
        let request = params
            .get()?
            .get_request()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                crate::db_rpc::read_execute_request(r)
                    .map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        crate::db_rpc::write_execute_result_reply(
            results.get().init_result(),
            self.inner.execute(request).await,
        );
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: guest_database_capnp::CloseParams,
        mut results: guest_database_capnp::CloseResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.close().await {
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
        let database: Option<Box<dyn GuestDatabase>> = match p.get_database().ok() {
            Some(client) => Some(Box::new(GuestDatabaseClient { client })),
            None => None,
        };
        let mut databases: Vec<(String, Box<dyn GuestDatabase>)> = Vec::new();
        if p.has_databases() {
            let list = p
                .get_databases()
                .map_err(|err| capnp::Error::failed(err.to_string()))?;
            for entry in list.iter() {
                let name = entry
                    .get_name()
                    .and_then(|t| t.to_str().map_err(capnp::Error::from))
                    .map_err(|err| capnp::Error::failed(err.to_string()))?
                    .to_string();
                let client = entry
                    .get_database()
                    .map_err(|err| capnp::Error::failed(err.to_string()))?;
                databases.push((name, Box::new(GuestDatabaseClient { client })));
            }
        }
        let ctx = JobHandlerContext {
            input: Box::new(SourceClient::new(input, self.window)),
            output: Box::new(DestinationClient::new(output, self.window)),
            progress: Box::new(ProgressArc(progress)),
            database,
            databases,
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

/// Host bootstrap client for a plugin vat.
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
                max_scalar_bytes: crate::limits::MAX_SCALAR_BYTES,
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
    /// Returns a plugin error when the RPC fails or `apiVersion` is not
    /// [`crate::limits::PRODUCT_API_VERSION`].
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
        if m.get_api_version() != crate::limits::PRODUCT_API_VERSION {
            return Err(PluginError::unsupported(format!(
                "unsupported apiVersion {}",
                m.get_api_version()
            )));
        }
        read_describe(m)
    }

    /// Returns a destination capability.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the factory call fails.
    pub async fn destination(&self, ctx: DestinationContext) -> Result<DestinationClient> {
        let mut req = self.client.destination_request();
        {
            let c = req.get().get_context().map_err(from_capnp)?;
            write_extensible_config(c.init_config(), &ctx.config);
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
            let c = req.get().get_context().map_err(from_capnp)?;
            write_extensible_config(c.init_config(), &ctx.config);
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
            write_extensible_config(c.init_config(), &ctx.config);
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
            None,
            Vec::new(),
        )
        .await
    }

    /// [`Self::handle_job`] with an explicit cancellation capability.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the handler fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_job_with_cancel(
        &self,
        handler: job_handler::Client,
        invocation: JobInvocation,
        input: Arc<dyn Source>,
        output: Arc<dyn Destination>,
        progress: Arc<dyn ProgressSink>,
        cancel: Arc<dyn Cancellation>,
        database: Option<Arc<dyn GuestDatabase>>,
        databases: Vec<(String, Arc<dyn GuestDatabase>)>,
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
        if let Some(db) = database {
            req.get()
                .set_database(capnp_rpc::new_client(GuestDatabaseServer { inner: db }));
        }
        if !databases.is_empty() {
            let count = u32::try_from(databases.len())
                .map_err(|_| PluginError::invalid_params("too many database bindings"))?;
            let mut list = req.get().init_databases(count);
            for (i, (name, db)) in databases.into_iter().enumerate() {
                let mut entry = list.reborrow().get(u32::try_from(i).unwrap_or(u32::MAX));
                entry.set_name(&name);
                entry.set_database(capnp_rpc::new_client(GuestDatabaseServer { inner: db }));
            }
        }
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
            let c = req.get().get_context().map_err(from_capnp)?;
            write_extensible_config(c.init_config(), &ctx.config);
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
            let c = req.get().get_context().map_err(from_capnp)?;
            write_extensible_config(c.init_config(), &ctx.config);
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
            // Host-private connect params (postgres URL, D1 token, sqlite path)
            // travel in `config`; third-party adapters get typed `adapter`.
            write_extensible_config(c.reborrow().init_config(), &ctx.config);
            write_database_adapter_config(c.reborrow().init_adapter(), &ctx.adapter)
                .map_err(from_capnp)?;
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

    /// Returns the guest's declared CLI surface.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn cli_describe(&self) -> Result<CliSchema> {
        let req = self.client.cli_describe_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        read_cli_schema_reply(result).map_err(from_capnp)?
    }

    /// Runs one guest CLI command.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn cli_invoke(&self, params: CliInvokeParams) -> Result<CliInvokeResult> {
        let mut req = self.client.cli_invoke_request();
        write_cli_invoke_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        read_cli_invoke_reply(result).map_err(from_capnp)?
    }

    /// Lists plugin-provided OIDC authorization-server client templates.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails. Older guests that lack
    /// `oidcClients` surface `unsupported` / transport errors.
    pub async fn oidc_clients(&self) -> Result<Vec<OidcClientTemplate>> {
        let req = self.client.oidc_clients_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            oidc_clients_reply::Ok(ok) => {
                let ok = ok.map_err(from_capnp)?;
                let list = ok.get_clients().map_err(from_capnp)?;
                let mut out = Vec::new();
                for item in list.iter() {
                    out.push(read_oidc_client_template(item)?);
                }
                Ok(out)
            }
            oidc_clients_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    /// Complete ordered plugin-owned migration sequence for one named binding.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the RPC fails.
    pub async fn database_migrations(&self, binding: &str) -> Result<Vec<PluginMigration>> {
        let mut req = self.client.database_migrations_request();
        req.get().set_binding(binding);
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            plugin_migrations_reply::Ok(ok) => read_plugin_migrations(ok.map_err(from_capnp)?),
            plugin_migrations_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
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

/// `reply.get()?.get_result()?` for any `(result :T)` method response.
macro_rules! reply_result {
    ($reply:expr) => {
        $reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?
    };
}

/// Cap'n Proto client for [`ContentSource`].
pub struct ContentSourceClient {
    client: content_source_capnp::Client,
}

#[async_trait::async_trait(?Send)]
impl ContentSource for ContentSourceClient {
    async fn login(&self, params: LoginParams) -> Result<LoginResult> {
        let mut req = self.client.login_request();
        write_login_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_login_reply(reply_result!(reply)).map_err(from_capnp)?
    }
    async fn scan(&self, params: ScanParams) -> Result<ScanSummary> {
        let mut req = self.client.scan_request();
        write_scan_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_scan_reply(reply_result!(reply)).map_err(from_capnp)?
    }
    async fn fetch_title(&self, params: FetchTitleParams) -> Result<PlainFetch> {
        let mut req = self.client.fetch_title_request();
        write_fetch_title_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_fetch_title_reply(reply_result!(reply)).map_err(from_capnp)?
    }
    async fn list_accounts(&self) -> Result<Vec<SourceAccount>> {
        let req = self.client.list_accounts_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_source_accounts_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.accounts)
    }
    async fn login_start(&self, params: LoginParams) -> Result<LoginStartResult> {
        let mut req = self.client.login_start_request();
        write_login_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_login_start_reply(reply_result!(reply)).map_err(from_capnp)?
    }
    async fn login_complete(&self, params: LoginCompleteParams) -> Result<LoginResult> {
        let mut req = self.client.login_complete_request();
        write_login_complete_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_login_reply(reply_result!(reply)).map_err(from_capnp)?
    }
    async fn search_catalog(&self, params: SearchCatalogParams) -> Result<Vec<CatalogHit>> {
        let mut req = self.client.search_catalog_request();
        write_search_catalog_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_catalog_hits_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.hits)
    }
    async fn expand_candidates(&self, params: ExpandCandidatesParams) -> Result<Vec<CatalogHit>> {
        let mut req = self.client.expand_candidates_request();
        write_expand_candidates_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_catalog_hits_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.hits)
    }
    async fn purchase_hint(&self, params: PurchaseHintParams) -> Result<Option<PurchaseHint>> {
        let mut req = self.client.purchase_hint_request();
        write_purchase_hint_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_purchase_hint_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.found.then_some(ok.hint))
    }
    async fn list_deals(&self, params: ListDealsParams) -> Result<Vec<CatalogHit>> {
        let mut req = self.client.list_deals_request();
        write_list_deals_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_catalog_hits_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.hits)
    }
    async fn catalog_detail(&self, params: CatalogDetailParams) -> Result<Option<CatalogHit>> {
        let mut req = self.client.catalog_detail_request();
        write_catalog_detail_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_catalog_detail_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.found.then_some(ok.hit))
    }
    async fn health(&self) -> Result<HealthOk> {
        let req = self.client.health_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_health_reply(reply_result!(reply))
    }
    async fn diagnose(&self) -> Result<Vec<String>> {
        let req = self.client.diagnose_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_diagnose_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.lines)
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
        read_health_reply(reply_result!(reply))
    }
    async fn on_event(&self, event: DomainEvent) -> Result<EventResult> {
        if event.payload.len() > MAX_EVENT_PAYLOAD_BYTES as usize {
            return Err(PluginError::payload_too_large(format!(
                "domain event payload of {} bytes exceeds {MAX_EVENT_PAYLOAD_BYTES}",
                event.payload.len()
            )));
        }
        if event.checkpoint_json.len() > MAX_CHECKPOINT_BYTES as usize {
            return Err(PluginError::payload_too_large(format!(
                "checkpoint of {} bytes exceeds {MAX_CHECKPOINT_BYTES}",
                event.checkpoint_json.len()
            )));
        }
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
            e.set_checkpoint_json(&event.checkpoint_json);
            e.set_checkpoint_schema_version(event.checkpoint_schema_version);
            e.set_invocation_sequence(event.invocation_sequence);
            e.set_resume_pending(event.resume_pending);
            e.set_source(&event.source);
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
        read_empty(reply_result!(reply))
    }
    async fn stop(&self) -> Result<()> {
        let req = self.client.stop_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(reply_result!(reply))
    }
    async fn diagnose(&self) -> Result<Vec<String>> {
        let req = self.client.diagnose_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_diagnose_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.lines)
    }
    async fn scan_library(&self, params: ScanLibraryParams) -> Result<()> {
        let mut req = self.client.scan_library_request();
        write_scan_library_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(reply_result!(reply))
    }
    async fn sync_listening(&self) -> Result<Vec<ListeningProgress>> {
        let req = self.client.sync_listening_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_sync_listening_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.items)
    }
    async fn authenticate_user(&self, params: AuthenticateUserParams) -> Result<ExternalUser> {
        let mut req = self.client.authenticate_user_request();
        write_authenticate_user_params(req.get().init_params(), &params).map_err(from_capnp)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_external_user_reply(reply_result!(reply)).map_err(from_capnp)?
    }
    async fn poll_events(&self) -> Result<Vec<ExternalUser>> {
        let req = self.client.poll_events_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_event_poll_reply(reply_result!(reply))
            .map_err(from_capnp)?
            .map(|ok| ok.users)
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
    match r.which().map_err(from_capnp)? {
        event_result_capnp::Ack(_) => Ok(EventResult::Ack),
        event_result_capnp::Retry(ok) => {
            let ok = ok.map_err(from_capnp)?;
            Ok(EventResult::Retry {
                retry_at_unix_ms: ok.get_retry_at_unix_ms(),
                reason: text_of(ok.get_reason().map_err(from_capnp)?),
            })
        }
        event_result_capnp::Reject(ok) => Ok(EventResult::Reject {
            reason: text_of(ok.map_err(from_capnp)?.get_reason().map_err(from_capnp)?),
        }),
        event_result_capnp::DeadLetter(ok) => Ok(EventResult::DeadLetter {
            reason: text_of(ok.map_err(from_capnp)?.get_reason().map_err(from_capnp)?),
        }),
        event_result_capnp::Suspended(ok) => {
            let ok = ok.map_err(from_capnp)?;
            let checkpoint_json = text_of(ok.get_checkpoint_json().map_err(from_capnp)?);
            if checkpoint_json.len() > MAX_CHECKPOINT_BYTES as usize {
                return Err(PluginError::payload_too_large(format!(
                    "checkpoint of {} bytes exceeds {MAX_CHECKPOINT_BYTES}",
                    checkpoint_json.len()
                )));
            }
            Ok(EventResult::Suspended {
                checkpoint_json,
                checkpoint_schema_version: ok.get_checkpoint_schema_version(),
                wake_at_unix_ms: ok.get_wake_at_unix_ms(),
                wake_on_event_type: text_of(ok.get_wake_on_event_type().map_err(from_capnp)?),
                wake_on_filter_json: text_of(ok.get_wake_on_filter_json().map_err(from_capnp)?),
            })
        }
    }
}

/// Cap'n Proto client for [`Database`].
pub struct DatabaseClient {
    client: database_capnp::Client,
}

/// Public adapter session plus host-private interactive transaction client.
#[cfg(feature = "host")]
pub struct AdapterSessionHandle {
    /// Typed `capabilities` / `execute` / `close`.
    pub session: Box<dyn AdapterDatabaseSession>,
    /// Host-only `begin` (capability cast on the same session object).
    pub host: crate::host_rpc::HostAdapterDatabaseSessionClient,
}

#[cfg(feature = "host")]
impl DatabaseClient {
    /// Opens a session and host-private transaction client on one capability.
    ///
    /// # Errors
    ///
    /// Returns when `openSession` fails on the guest.
    pub async fn open_session_handle(&self) -> Result<AdapterSessionHandle> {
        let req = self.client.open_session_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            adapter_session_reply::Ok(sess) => {
                let cap_client = sess.map_err(from_capnp)?;
                Ok(AdapterSessionHandle {
                    session: Box::new(AdapterDatabaseSessionClient {
                        client: cap_client.clone(),
                    }),
                    host: crate::host_rpc::HostAdapterDatabaseSessionClient::from_session_client(
                        cap_client,
                    ),
                })
            }
            adapter_session_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Database for DatabaseClient {
    async fn open_session(&self) -> Result<Box<dyn AdapterDatabaseSession>> {
        let req = self.client.open_session_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            adapter_session_reply::Ok(sess) => Ok(Box::new(AdapterDatabaseSessionClient {
                client: sess.map_err(from_capnp)?,
            })),
            adapter_session_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
}

struct AdapterDatabaseSessionClient {
    client: adapter_database_session_capnp::Client,
}

#[async_trait::async_trait(?Send)]
impl AdapterDatabaseSession for AdapterDatabaseSessionClient {
    async fn capabilities(&self) -> Result<crate::DbCapabilities> {
        let req = self.client.capabilities_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_db_capabilities_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn execute(
        &self,
        request: crate::host_envelope::AdapterExecuteRequest,
    ) -> Result<crate::ExecuteReply> {
        let mut req = self.client.execute_request();
        crate::db_rpc::write_adapter_execute_request(req.get().init_request(), &request)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_execute_result_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
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

    async fn bootstrap(&self) -> Result<crate::DbBootstrap> {
        let req = self.client.bootstrap_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_db_bootstrap_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn export_identity(&self) -> Result<Vec<crate::DbIdentityHighWater>> {
        let req = self.client.export_identity_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_identity_export_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn import_identity(&self, rows: &[crate::DbIdentityHighWater]) -> Result<()> {
        let mut req = self.client.import_identity_request();
        {
            let mut list = req.get().init_rows(rows.len() as u32);
            crate::db_rpc::write_identity_list(list.reborrow(), rows);
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn list_user_relations(&self) -> Result<Vec<String>> {
        let req = self.client.list_user_relations_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_user_relations_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn prepare_unit_restore(&self) -> Result<()> {
        let req = self.client.prepare_unit_restore_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn drop_user_relations(&self, names: &[String]) -> Result<()> {
        let mut req = self.client.drop_user_relations_request();
        {
            let mut list = req.get().init_names(names.len() as u32);
            for (i, name) in names.iter().enumerate() {
                list.set(i as u32, name);
            }
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn assert_restore_constraints(&self) -> Result<()> {
        let req = self.client.assert_restore_constraints_request();
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

struct GuestDatabaseClient {
    client: guest_database_capnp::Client,
}

#[async_trait::async_trait(?Send)]
impl GuestDatabase for GuestDatabaseClient {
    async fn execute(&self, request: crate::ExecuteRequest) -> Result<crate::ExecuteReply> {
        let mut req = self.client.execute_request();
        crate::db_rpc::write_execute_request(req.get().init_request(), &request)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_execute_result_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
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
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
mod tests {
    use super::*;
    use crate::{
        ByteRange, Cancellation, CopyResult, Database, DatabaseContext, Destination,
        DestinationContext, DomainEvent, EventResult, ExtensibleConfig, GuestDatabase, HealthOk,
        Integration, IntegrationContext, JobHandler, JobHandlerContext, JobInvocation, JobOutcome,
        ListOptions, ListPage, ObjectInfo, ObjectMetadata, PluginDescribe, PluginRoot,
        ProgressSink, PutResult, ReadResult, ScalarLimits, Source, SourceContext, WorkerContext,
        WriteOptions, FEATURE_SCALAR_LIMITS, FEATURE_STREAMS, MAX_CHECKPOINT_BYTES,
        MAX_EVENT_PAYLOAD_BYTES, MAX_LIST_PAGE, MAX_PLUGIN_MIGRATION_OPS,
        MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES, MAX_PLUGIN_MIGRATION_TOTAL_OPS, MAX_SCALAR_BYTES,
        PRODUCT_API_VERSION,
    };
    use crate::{
        ExecuteRequest, PluginError, PluginErrorCode, PluginMigration, PluginMigrationOp, Result,
    };
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
        migrations: Option<Vec<PluginMigration>>,
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

        async fn oidc_clients(&self) -> Result<Vec<OidcClientTemplate>> {
            Ok(vec![OidcClientTemplate {
                client_id: "abs".into(),
                display_name: "Audiobookshelf".into(),
                callback_path: "/auth/openid/callback".into(),
                public_client: true,
                default_scopes: vec!["openid".into(), "profile".into()],
                issue_refresh_token: true,
                origin_config_key: "integrations.audiobookshelf.base_url".into(),
            }])
        }

        async fn database_migrations(&self, binding: &str) -> Result<Vec<PluginMigration>> {
            if let Some(migrations) = &self.migrations {
                return Ok(migrations.clone());
            }
            if binding != "DB" {
                return Ok(Vec::new());
            }
            Ok(vec![PluginMigration {
                id: "create-notes".into(),
                operations: vec![PluginMigrationOp::Schema(
                    "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)"
                        .into(),
                )],
            }])
        }
    }

    struct CaptureDbPlugin {
        seen: Mutex<Option<DatabaseContext>>,
    }

    #[async_trait::async_trait(?Send)]
    impl PluginRoot for CaptureDbPlugin {
        async fn describe(&self) -> Result<PluginDescribe> {
            Ok(PluginDescribe {
                api_version: PRODUCT_API_VERSION,
                id: "db_ctx".into(),
                kind: "database".into(),
                display_name: None,
                rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
                scalar_limits: ScalarLimits::default().into(),
                supported_roles: vec!["database".into()],
                ..PluginDescribe::default()
            })
        }

        async fn database(&self, context: DatabaseContext) -> Result<Box<dyn Database>> {
            *self.seen.lock().expect("seen") = Some(context);
            Err(PluginError::unsupported("database"))
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

    struct GuestDbProbe;

    #[async_trait::async_trait(?Send)]
    impl GuestDatabase for GuestDbProbe {
        async fn execute(&self, _request: ExecuteRequest) -> Result<crate::ExecuteReply> {
            Err(PluginError::unsupported("execute"))
        }
    }

    struct DbProbeHandler;

    #[async_trait::async_trait(?Send)]
    impl JobHandler for DbProbeHandler {
        async fn handle(
            &self,
            _invocation: JobInvocation,
            mut context: JobHandlerContext,
        ) -> Result<JobOutcome> {
            if context.database.take().is_some() {
                return Err(PluginError::internal(
                    "host library database must not be injected",
                ));
            }
            let Some(named) = context.take_named_database("DB") else {
                return Err(PluginError::internal("named database binding missing"));
            };
            named.close().await?;
            if context.take_named_database("OTHER").is_some() {
                return Err(PluginError::internal("unexpected extra binding"));
            }
            Ok(JobOutcome::Completed {
                message: "named-database-injected".into(),
                bytes_copied: 0,
            })
        }
    }

    struct DbProbePlugin {
        dest: Arc<MemDest>,
    }

    #[async_trait::async_trait(?Send)]
    impl PluginRoot for DbProbePlugin {
        async fn describe(&self) -> Result<PluginDescribe> {
            Ok(PluginDescribe {
                api_version: PRODUCT_API_VERSION,
                id: "db_probe".into(),
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
            Ok(Box::new(DbProbeHandler))
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

    struct EventPlugin;

    #[async_trait::async_trait(?Send)]
    impl PluginRoot for EventPlugin {
        async fn describe(&self) -> Result<PluginDescribe> {
            Ok(PluginDescribe {
                api_version: PRODUCT_API_VERSION,
                id: "event_test".into(),
                kind: "integration".into(),
                rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
                scalar_limits: ScalarLimits::default().into(),
                supported_roles: vec!["integration".into()],
                ..PluginDescribe::default()
            })
        }

        async fn integration(&self, _context: IntegrationContext) -> Result<Box<dyn Integration>> {
            Ok(Box::new(EventIntegration))
        }
    }

    struct EventIntegration;

    #[async_trait::async_trait(?Send)]
    impl Integration for EventIntegration {
        async fn health(&self) -> Result<HealthOk> {
            Ok(HealthOk {
                ok: true,
                detail: "event test".into(),
            })
        }

        async fn on_event(&self, event: DomainEvent) -> Result<EventResult> {
            Ok(match event.event_type.as_str() {
                "test_retry" => EventResult::Retry {
                    retry_at_unix_ms: 9,
                    reason: "retry".into(),
                },
                "test_reject" => EventResult::Reject {
                    reason: "reject".into(),
                },
                "test_dead_letter" => EventResult::DeadLetter {
                    reason: "dead".into(),
                },
                "test_suspend" => EventResult::Suspended {
                    checkpoint_json: r#"{"n":1}"#.into(),
                    checkpoint_schema_version: 1,
                    wake_at_unix_ms: 3,
                    wake_on_event_type: String::new(),
                    wake_on_filter_json: String::new(),
                },
                "test_suspend_huge" => EventResult::Suspended {
                    checkpoint_json: "x".repeat(MAX_CHECKPOINT_BYTES as usize + 1),
                    checkpoint_schema_version: 1,
                    wake_at_unix_ms: 3,
                    wake_on_event_type: String::new(),
                    wake_on_filter_json: String::new(),
                },
                _ => EventResult::Ack,
            })
        }
    }

    fn sample_event(event_type: &str) -> DomainEvent {
        DomainEvent {
            event_id: "e1".into(),
            event_type: event_type.into(),
            schema_version: 1,
            occurred_at_unix_ms: 1,
            deduplication_key: "k".into(),
            delivery_attempt: 1,
            payload: b"{}".to_vec(),
            ..DomainEvent::default()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_result_roundtrip_all_variants_and_oversized_payload() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(64 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                tokio::task::spawn_local(async move {
                    let _ =
                        serve_plugin(Arc::new(EventPlugin), server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let integration = client
                    .integration(IntegrationContext::default())
                    .await
                    .expect("integration");
                assert_eq!(
                    integration
                        .on_event(sample_event("book_acquired"))
                        .await
                        .unwrap(),
                    EventResult::Ack
                );
                assert_eq!(
                    integration
                        .on_event(sample_event("test_retry"))
                        .await
                        .unwrap(),
                    EventResult::Retry {
                        retry_at_unix_ms: 9,
                        reason: "retry".into(),
                    }
                );
                assert_eq!(
                    integration
                        .on_event(sample_event("test_reject"))
                        .await
                        .unwrap(),
                    EventResult::Reject {
                        reason: "reject".into(),
                    }
                );
                assert_eq!(
                    integration
                        .on_event(sample_event("test_dead_letter"))
                        .await
                        .unwrap(),
                    EventResult::DeadLetter {
                        reason: "dead".into(),
                    }
                );
                assert_eq!(
                    integration
                        .on_event(sample_event("test_suspend"))
                        .await
                        .unwrap(),
                    EventResult::Suspended {
                        checkpoint_json: r#"{"n":1}"#.into(),
                        checkpoint_schema_version: 1,
                        wake_at_unix_ms: 3,
                        wake_on_event_type: String::new(),
                        wake_on_filter_json: String::new(),
                    }
                );
                let mut oversized = sample_event("book_acquired");
                oversized.payload = vec![0; MAX_EVENT_PAYLOAD_BYTES as usize + 1];
                let err = integration.on_event(oversized).await.unwrap_err();
                assert_eq!(err.code, crate::PluginErrorCode::PayloadTooLarge);
                let err = integration
                    .on_event(sample_event("test_suspend_huge"))
                    .await
                    .unwrap_err();
                assert_eq!(err.code, crate::PluginErrorCode::PayloadTooLarge);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oidc_clients_roundtrip() {
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
                    migrations: None,
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let clients = client.oidc_clients().await.expect("oidcClients");
                assert_eq!(clients.len(), 1);
                assert_eq!(clients[0].client_id, "abs");
                assert_eq!(clients[0].callback_path, "/auth/openid/callback");
                assert_eq!(
                    clients[0].origin_config_key,
                    "integrations.audiobookshelf.base_url"
                );
                assert_eq!(clients[0].scopes_or_default(), vec!["openid", "profile"]);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_migrations_roundtrip() {
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
                    migrations: None,
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let empty = client
                    .database_migrations("CACHE")
                    .await
                    .expect("databaseMigrations");
                assert!(empty.is_empty());
                let migrations = client
                    .database_migrations("DB")
                    .await
                    .expect("databaseMigrations");
                assert_eq!(migrations.len(), 1);
                assert_eq!(migrations[0].id, "create-notes");
                assert!(matches!(
                    migrations[0].operations.first(),
                    Some(PluginMigrationOp::Schema(sql)) if sql.contains("notes")
                ));
            })
            .await;
    }

    fn schema_mig(id: &str, sql: impl Into<String>) -> PluginMigration {
        PluginMigration {
            id: id.into(),
            operations: vec![PluginMigrationOp::Schema(sql.into())],
        }
    }

    fn n_migrations(n: usize) -> Vec<PluginMigration> {
        (0..n)
            .map(|i| schema_mig(&format!("m{i:03}"), "x"))
            .collect()
    }

    fn n_ops(n: usize) -> PluginMigration {
        PluginMigration {
            id: "ops".into(),
            operations: (0..n)
                .map(|_| PluginMigrationOp::Schema("x".into()))
                .collect(),
        }
    }

    fn encode_then_decode(migrations: &[PluginMigration]) -> Result<Vec<PluginMigration>> {
        require_plugin_migration_registration(migrations)?;
        let mut message = capnp::message::Builder::new_default();
        {
            let ok = message.init_root::<plugin_migrations_ok::Builder>();
            fill_plugin_migrations(ok, migrations)?;
        }
        let reader = message
            .get_root_as_reader::<plugin_migrations_ok::Reader<'_>>()
            .map_err(from_capnp)?;
        read_plugin_migrations(reader)
    }

    fn decode_unbounded(
        fill: impl FnOnce(plugin_migrations_ok::Builder<'_>),
    ) -> Result<Vec<PluginMigration>> {
        let mut message = capnp::message::Builder::new_default();
        {
            let ok = message.init_root::<plugin_migrations_ok::Builder>();
            fill(ok);
        }
        let reader = message
            .get_root_as_reader::<plugin_migrations_ok::Reader<'_>>()
            .map_err(from_capnp)?;
        read_plugin_migrations(reader)
    }

    #[test]
    fn plugin_migrations_roundtrip_at_count_limit() {
        let migrations = n_migrations(MAX_LIST_PAGE as usize);
        let back = encode_then_decode(&migrations).expect("count N");
        assert_eq!(back, migrations);
    }

    #[test]
    fn plugin_migrations_decode_count_plus_one_is_payload_too_large() {
        let n = MAX_LIST_PAGE + 1;
        let err = decode_unbounded(|mut ok| {
            let mut list = ok.reborrow().init_migrations(n);
            for i in 0..n {
                let mut m = list.reborrow().get(i);
                m.set_id(format!("m{i:03}"));
                m.reborrow().init_operations(1).get(0).set_schema("x");
            }
        })
        .expect_err("count N+1");
        assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
        assert!(err.message.contains("maxListPage"), "{err}");
    }

    #[test]
    fn plugin_migrations_roundtrip_at_ops_limit() {
        let migrations = vec![n_ops(MAX_PLUGIN_MIGRATION_OPS as usize)];
        let back = encode_then_decode(&migrations).expect("ops N");
        assert_eq!(back, migrations);
    }

    #[test]
    fn plugin_migrations_decode_ops_plus_one_is_payload_too_large() {
        let n = MAX_PLUGIN_MIGRATION_OPS + 1;
        let err = decode_unbounded(|mut ok| {
            let mut list = ok.reborrow().init_migrations(1);
            let mut m = list.reborrow().get(0);
            m.set_id("ops");
            let mut ops = m.reborrow().init_operations(n);
            for i in 0..n {
                ops.reborrow().get(i).set_schema("x");
            }
        })
        .expect_err("ops N+1");
        assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
        assert!(err.message.contains("maxPluginMigrationOps"), "{err}");
    }

    fn n_ops_spread(total: usize, per_migration: usize) -> Vec<PluginMigration> {
        assert!(per_migration > 0);
        let full = total / per_migration;
        let rem = total % per_migration;
        let mut out = Vec::new();
        for i in 0..full {
            let mut m = n_ops(per_migration);
            m.id = format!("t{i:03}");
            out.push(m);
        }
        if rem > 0 {
            let mut m = n_ops(rem);
            m.id = format!("t{full:03}");
            out.push(m);
        }
        out
    }

    #[test]
    fn plugin_migrations_roundtrip_at_total_ops_limit() {
        let per = MAX_PLUGIN_MIGRATION_OPS as usize / 2;
        let migrations = n_ops_spread(MAX_PLUGIN_MIGRATION_TOTAL_OPS as usize, per);
        let back = encode_then_decode(&migrations).expect("total ops N");
        assert_eq!(back, migrations);
    }

    #[test]
    fn plugin_migrations_decode_total_ops_plus_one_is_payload_too_large() {
        let per = MAX_PLUGIN_MIGRATION_OPS;
        let n = (MAX_PLUGIN_MIGRATION_TOTAL_OPS / per) + 1;
        let err = decode_unbounded(|mut ok| {
            let mut list = ok.reborrow().init_migrations(n);
            for i in 0..n {
                let mut m = list.reborrow().get(i);
                m.set_id(format!("m{i:03}"));
                let mut ops = m.reborrow().init_operations(per);
                for j in 0..per {
                    ops.reborrow().get(j).set_schema("x");
                }
            }
        })
        .expect_err("total ops N+1");
        assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
        assert!(err.message.contains("maxPluginMigrationTotalOps"), "{err}");
    }

    #[test]
    fn plugin_migrations_roundtrip_at_sql_limit() {
        let sql = "x".repeat(MAX_SCALAR_BYTES as usize);
        let migrations = vec![schema_mig("", sql)];
        let back = encode_then_decode(&migrations).expect("sql N");
        assert_eq!(back, migrations);
    }

    #[test]
    fn plugin_migrations_decode_sql_plus_one_is_payload_too_large() {
        let sql = "x".repeat(MAX_SCALAR_BYTES as usize + 1);
        let err = decode_unbounded(|mut ok| {
            let mut list = ok.reborrow().init_migrations(1);
            let mut m = list.reborrow().get(0);
            m.set_id("sql");
            m.reborrow().init_operations(1).get(0).set_schema(&sql);
        })
        .expect_err("sql N+1");
        assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
        assert!(err.message.contains("maxScalarBytes"), "{err}");
    }

    #[test]
    fn plugin_migrations_roundtrip_at_aggregate_limit() {
        let max_reg = MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES as usize;
        let id = "a";
        let sql = "x".repeat(max_reg - id.len());
        let migrations = vec![schema_mig(id, sql)];
        let back = encode_then_decode(&migrations).expect("aggregate N");
        assert_eq!(back, migrations);
    }

    #[test]
    fn plugin_migrations_decode_aggregate_plus_one_is_payload_too_large() {
        let half = (MAX_SCALAR_BYTES as usize) / 2 + 1;
        let sql = "x".repeat(half);
        let err = decode_unbounded(|mut ok| {
            let mut list = ok.reborrow().init_migrations(2);
            for (i, id) in ["a", "b"].iter().enumerate() {
                let mut m = list.reborrow().get(u32::try_from(i).expect("index"));
                m.set_id(id);
                m.reborrow().init_operations(1).get(0).set_schema(&sql);
            }
        })
        .expect_err("aggregate N+1");
        assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
        assert!(
            err.message.contains("maxPluginMigrationRegistrationBytes"),
            "{err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_migrations_rpc_rejects_count_plus_one() {
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
                    migrations: Some(n_migrations(MAX_LIST_PAGE as usize + 1)),
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let err = client
                    .database_migrations("DB")
                    .await
                    .expect_err("oversize registration");
                assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_migrations_rpc_roundtrip_at_count_limit() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(256 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                let registered = n_migrations(MAX_LIST_PAGE as usize);
                let plugin = Arc::new(TestPlugin {
                    dest: Arc::new(MemDest {
                        store: Mutex::new(HashMap::new()),
                    }),
                    migrations: Some(registered.clone()),
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let back = client
                    .database_migrations("DB")
                    .await
                    .expect("count N roundtrip");
                assert_eq!(back, registered);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_migrations_rpc_rejects_total_ops_plus_one() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(256 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                let per = MAX_PLUGIN_MIGRATION_OPS as usize / 2;
                let plugin = Arc::new(TestPlugin {
                    dest: Arc::new(MemDest {
                        store: Mutex::new(HashMap::new()),
                    }),
                    migrations: Some(n_ops_spread(
                        MAX_PLUGIN_MIGRATION_TOTAL_OPS as usize + 1,
                        per,
                    )),
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let err = client
                    .database_migrations("DB")
                    .await
                    .expect_err("oversize total ops");
                assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
                assert!(err.message.contains("maxPluginMigrationTotalOps"), "{err}");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_factory_forwards_extensible_config() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(64 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                let plugin = Arc::new(CaptureDbPlugin {
                    seen: Mutex::new(None),
                });
                let seen = Arc::clone(&plugin);
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let sent = DatabaseContext {
                    config: ExtensibleConfig {
                        schema_version: 1,
                        media_type: "application/vnd.bookclerk.db-connect+json".into(),
                        payload: br#"{"backend":"postgres","url":"postgres://example/db"}"#
                            .to_vec(),
                    },
                    adapter: crate::DatabaseAdapterConfig {
                        plugin_data_dir: "/tmp/plugins/probe/data".into(),
                        binding: Some("DB".into()),
                        ..crate::DatabaseAdapterConfig::default()
                    },
                };
                let err = match client.database(sent.clone()).await {
                    Err(err) => err,
                    Ok(_) => panic!("probe guest must return unsupported, not a session"),
                };
                assert_eq!(err.code, PluginErrorCode::Unsupported);
                let got = seen
                    .seen
                    .lock()
                    .expect("seen")
                    .clone()
                    .expect("guest received DatabaseContext");
                assert_eq!(got.config.schema_version, sent.config.schema_version);
                assert_eq!(got.config.media_type, sent.config.media_type);
                assert_eq!(got.config.payload, sent.config.payload);
                assert_eq!(got.adapter, sent.adapter);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_job_injects_database_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_end, server_end) = duplex(64 * 1024);
                let (server_r, server_w) = tokio::io::split(server_end);
                let (client_r, client_w) = tokio::io::split(client_end);
                let store = Arc::new(MemDest {
                    store: Mutex::new(HashMap::new()),
                });
                let plugin = Arc::new(DbProbePlugin {
                    dest: Arc::clone(&store),
                });
                tokio::task::spawn_local(async move {
                    let _ = serve_plugin(plugin, server_r, server_w, 64 * 1024).await;
                });
                let (client, rpc) = connect_plugin(client_r, client_w, 64 * 1024);
                tokio::task::spawn_local(rpc);
                let handler = client
                    .worker(WorkerContext {
                        job_id: "probe".into(),
                        ..Default::default()
                    })
                    .await
                    .expect("worker");
                let granted = Arc::new(DestClone(Arc::clone(&store)));
                let outcome = client
                    .handle_job_with_cancel(
                        handler,
                        JobInvocation::stream_copy("probe", "{}"),
                        granted.clone() as Arc<dyn Source>,
                        granted as Arc<dyn Destination>,
                        Arc::new(NoopProgress),
                        Arc::new(TestCancel(Arc::new(AtomicBool::new(false)))),
                        None,
                        vec![(
                            "DB".to_string(),
                            Arc::new(GuestDbProbe) as Arc<dyn GuestDatabase>,
                        )],
                    )
                    .await
                    .expect("handle");
                match outcome {
                    JobOutcome::Completed { message, .. } => {
                        assert_eq!(message, "named-database-injected");
                    }
                    other => panic!("expected completed, got {other:?}"),
                }
            })
            .await;
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
                    migrations: None,
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
                    migrations: None,
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
                    migrations: None,
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
                        None,
                        Vec::new(),
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
