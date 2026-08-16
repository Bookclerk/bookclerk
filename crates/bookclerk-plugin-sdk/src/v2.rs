//! ABI v2 guest helpers (`describe` / streams / `JobHandler`).

#![allow(clippy::missing_docs_in_private_items)]

pub use bookclerk_plugin_abi::v2::{
    byte_source_from_async_read, connect_plugin, negotiate_rpc_features,
    pull_byte_source_to_writer, serve_plugin, serve_plugin_stdio, stream_copy_keys, ByteRange,
    Cancellation, ContentSource, ContentSourceClient, ContentSourceContext, CopyResult, Database,
    DatabaseClient, DatabaseContext, DatabaseSession, Destination, DestinationClient,
    DestinationContext, DestinationServer, DomainEvent, EventResult, ExecResult, HealthOk,
    Integration, IntegrationClient, IntegrationContext, JobCheckpoint, JobHandler,
    JobHandlerContext, JobInvocation, JobInvocationLease, JobOutcome, ListOptions, ListPage,
    NeverCancel, ObjectInfo, ObjectMetadata, OidcClientTemplate, PluginClient, PluginDescribe,
    PluginRoot, PluginServer, ProgressSink, PutResult, QueryPage, ReadResult, ScalarLimits, Source,
    SourceClient, SourceContext, SourceServer, Statement, StreamCopyHandler, StreamCopySpec,
    Transaction, WorkerContext, WriteOptions, ABI_MAJOR, ABI_MINOR, ENVELOPE_VERSION,
    FEATURE_SCALAR_LIMITS, FEATURE_STORAGE_COPY, FEATURE_STREAMS, MAX_LIST_PAGE, MAX_SCALAR_BYTES,
    MAX_STREAM_WINDOW_BYTES, PRODUCT_API_VERSION,
};

mod json;
pub use json::{decode as decode_json, encode as encode_json, page_rows};

use std::sync::Arc;

use crate::error::{Result, SdkError};

/// Serves a v2 [`PluginRoot`] on stdin/stdout (Cap'n Proto RPC).
///
/// Must run on a current-thread tokio runtime inside a `LocalSet` (this helper
/// creates the `LocalSet`). Abort is capability drop / stream cancel.
///
/// # Errors
///
/// Returns [`SdkError`] when the vat fails.
pub async fn serve(plugin: impl PluginRoot + 'static) -> Result<()> {
    serve_v2(plugin).await
}

/// Serves a v2 [`PluginRoot`] on stdin/stdout (Cap'n Proto RPC).
///
/// Must run on a current-thread tokio runtime inside a `LocalSet` (this helper
/// creates the `LocalSet`). Abort is capability drop / stream cancel.
///
/// # Errors
///
/// Returns [`SdkError`] when the vat fails.
pub async fn serve_v2(plugin: impl PluginRoot + 'static) -> Result<()> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(serve_plugin_stdio(
            Arc::new(plugin),
            MAX_STREAM_WINDOW_BYTES,
        ))
        .await
        .map_err(|err| SdkError::message(err.to_string()))
}
