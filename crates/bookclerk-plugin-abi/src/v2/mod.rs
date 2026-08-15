//! Plugin ABI v2: object-capability classes, transferable byte streams, jobs.

#![allow(clippy::missing_docs_in_private_items)]

mod features;
mod jobs;
mod limits;
mod roles;
mod rpc;
mod types;

/// Generated Cap'n Proto RPC interfaces (`schema/plugin_v2.capnp`).
pub use crate::plugin_v2_capnp;

pub use features::{
    negotiate_rpc_features, RpcFeature, FEATURE_SCALAR_LIMITS, FEATURE_STORAGE_COPY,
    FEATURE_STREAMS,
};
pub use jobs::{read_all, stream_copy_keys, StreamCopyHandler, StreamCopySpec};
pub use limits::{
    ScalarLimits, ABI_MAJOR, ABI_MINOR, MAX_LIST_PAGE, MAX_SCALAR_BYTES, MAX_STREAM_WINDOW_BYTES,
    PRODUCT_API_VERSION,
};
pub use roles::{
    ByteRange, Cancellation, ContentSource, ContentSourceContext, Database, DatabaseContext,
    DatabaseSession, Destination, Integration, IntegrationContext, JobHandler, JobHandlerContext,
    NeverCancel, PluginRoot, ProgressSink, ReadResult, Source, Transaction,
};
pub use rpc::{
    byte_source_from_async_read, connect_plugin, pull_byte_source_to_writer, serve_plugin,
    serve_plugin_stdio, ContentSourceClient, DatabaseClient, DestinationClient, DestinationServer,
    IntegrationClient, PluginClient, PluginServer, SourceClient, SourceServer,
};
pub use types::{
    CopyResult, DestinationContext, DomainEvent, EventResult, ExecResult, ExtensibleConfig,
    HealthOk, JobCheckpoint, JobInvocation, JobInvocationLease, JobOutcome, ListOptions, ListPage,
    ObjectInfo, ObjectMetadata, PluginDescribe, PutResult, QueryPage, ScalarLimitsDto,
    SourceContext, Statement, WorkerContext, WriteOptions, ENVELOPE_VERSION, MAX_CHECKPOINT_BYTES,
};
