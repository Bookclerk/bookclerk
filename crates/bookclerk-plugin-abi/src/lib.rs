//! Authoritative Bookclerk plugin ABI (`api_version` 3).
//!
//! Version 3 is the capability-driven object-capability ABI (Cap'n Proto RPC,
//! transferred byte streams). The guest root is [`PluginWorker`]:
//! `describe()` advertises typed [`PluginCapabilities`], then
//! `open(invocation, bindings)` returns the exported [`Entrypoints`]
//! (`storefront`, `storage`, `databaseAdapter`, `remoteLibrary`, `cli`,
//! `oidc`, plus the `eventConsumer` / `jobRunner` triggers). Host-granted
//! [`Bindings`] carry `CONFIG` / `SECRETS`, the `EVENTS` publisher, and named
//! `[[databases]]` sessions — the Workers `env` idiom.
//!
//! # Audience
//!
//! - **Guest authors** — implement [`PluginWorker`] and the entrypoint traits
//!   against these DTOs (via `bookclerk-plugin-sdk`, `@bookclerk/plugin-sdk`,
//!   or a language binding generated from the same schema).
//! - **Host / SDK maintainers** — drive entrypoint capabilities, seal
//!   credentials, and upsert library rows without depending on store-specific
//!   crates.
//!
//! Product narrative (jail, consent, install layout) lives in
//! [`docs/plugins.md`](https://github.com/bookclerk/bookclerk/blob/main/docs/plugins.md).
//! This crate is the typed wire contract only.
//!
//! # Schema
//!
//! The Cap'n Proto schema at
//! [`schema/plugin.capnp`](https://github.com/bookclerk/bookclerk/blob/main/crates/bookclerk-plugin-abi/schema/plugin.capnp)
//! is the single source of truth: RPC interfaces, product constants, database
//! enums, and the "Typed method payloads" section. Types here are the Rust
//! projection; the TypeScript and
//! Python SDK projections are generated from the same schema by
//! `scripts/gen-plugin-abi.py`, which also drift-checks this crate. Wire DTO
//! fields serialize as **camelCase**.
//!
//! Install manifests validate against [`PLUGIN_TOML_SCHEMA_JSON`]
//! (`schema/plugin-toml.json`).
//!
//! # Versioning
//!
//! [`PRODUCT_API_VERSION`] is `3` (capability-driven Cap'n Proto / Workers
//! RPC). Product spawn requires `plugin.toml` `api_version = 3`. There is no
//! `protocol` key.
//!
//! # Modules
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`methods`] | Capability / consent method name constants (`login`, `event`, …) |
//! | Crate-root DTOs | [`PluginDescribe`], health, CLI, destination objects |
//! | [`kind`] | Kind-specific DTOs (source / integration / output) |
//! | [`db`] | Host-private database connect params (feature `host`) |
//! | [`error`] | [`PluginError`] / [`PluginErrorCode`] |
//! | [`guest_sql`] | SQL-v1 grammar / guest admission |
//! | [`sql_desugar`] | Host-only semantic desugars (`NULLS`, `NULLIF`) |
//! | [`plugin_capnp`] | Generated Cap'n Proto RPC interfaces |

pub mod backup_ops;
pub mod db;
pub mod db_execute;
mod db_rpc;
pub mod db_value;
pub mod error;
mod features;
pub mod generated;
pub mod guest_sql;
pub(crate) mod host_envelope;
#[cfg(feature = "host")]
mod host_roles;
#[cfg(feature = "host")]
mod host_rpc;
mod jobs;
pub mod json_bytes;
pub mod kind;
mod limits;
pub mod methods;
mod plugin_migration;
mod roles;
mod rpc;
mod rpc_types;
mod sdk_wire;
pub mod sql_desugar;
mod sql_overflow;
mod sql_proof;
mod sql_text;
pub mod sql_types;

/// Generated Cap'n Proto RPC interfaces (`schema/plugin.capnp`).
///
/// Included at crate root because `capnpc` emits `crate::plugin_capnp` paths.
#[allow(
    dead_code,
    missing_docs,
    unused_imports,
    unused_parens,
    clippy::all,
    clippy::pedantic,
    rustdoc::all,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::missing_docs_in_private_items
)]
pub mod plugin_capnp {
    include!(concat!(env!("OUT_DIR"), "/plugin_capnp.rs"));
}

/// Host-private Cap'n Proto RPC interfaces (`schema/plugin_host.capnp`).
#[cfg(feature = "host")]
#[allow(
    dead_code,
    missing_docs,
    unused_imports,
    unused_parens,
    clippy::all,
    clippy::pedantic,
    rustdoc::all,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::missing_docs_in_private_items
)]
pub mod plugin_host_capnp {
    include!(concat!(env!("OUT_DIR"), "/plugin_host_capnp.rs"));
}

pub use backup_ops::{AdapterBackupOps, SharedAdapterBackupOps};
pub use db::{binding_values_from_adapter_config, database_adapter_config_from_bindings};
#[cfg(feature = "host")]
pub use db::{binding_values_from_params, connect_params_from_bindings, DbConnectParams};
#[cfg(feature = "host")]
pub use db_execute::lowered_statement_preflight_len_proven;
pub use db_execute::{
    lowered_statement_preflight_len, lowered_statement_upper_bound_len, sql_payload_bytes,
    sql_payload_exceeds, DbBootstrap, DbCapabilities, DbColumn, DbIdentityHighWater,
    DbPlanStatementKind, DbResultSelection, DbRow, DbTiming, ExecuteReply, ExecuteRequest,
    IsolationReq, StatementResult, TypedDbStatement, D1_MAX_BINDS, D1_MAX_FUNCTION_ARGS,
    D1_MAX_PAYLOAD_BYTES, D1_MAX_SCHEMA_COLUMNS, D1_MAX_SQL_STATEMENT_BYTES,
    FIRST_PARTY_MAX_RESULT_BYTES, FIRST_PARTY_MAX_RESULT_ROWS, FIRST_PARTY_MAX_STATEMENTS,
    HOST_MIN_BINDS, HOST_MIN_CELL_BYTES, HOST_MIN_FUNCTION_ARGS, HOST_MIN_PATTERN_BYTES,
    HOST_MIN_PAYLOAD_BYTES, HOST_MIN_RESULT_BYTES, HOST_MIN_RESULT_ROWS, HOST_MIN_SCHEMA_COLUMNS,
    HOST_MIN_STATEMENTS, POSTGRES_MAX_BINDS, POSTGRES_MAX_FUNCTION_ARGS,
    POSTGRES_MAX_SCHEMA_COLUMNS, SQLITE_MAX_BINDS, SQLITE_MAX_FUNCTION_ARGS,
    SQLITE_MAX_PATTERN_BYTES, SQLITE_MAX_SCHEMA_COLUMNS, SQL_CONTRACT_VERSION,
};
pub use db_value::{db_type_from_declared, normalize_db_value_for_column, DbType, DbValue};
pub use error::{PluginError, PluginErrorCode, Result};
pub use generated::*;
pub use guest_sql::{
    authorize_guest_sql_policy, guest_statement_kind, is_reserved_binding_name,
    parse_guest_sql_refs, returning_single_row_proven, statement_is_ddl,
    validate_guest_execute_request, validate_guest_execute_request_for_policy,
    validate_sql_v1_grammar, GuestSqlPolicy, GuestSqlRefs, BOOKCLERK_RESERVED_PREFIX,
};
#[cfg(feature = "host")]
pub use host_envelope::GuestReceiptPersist;
pub use host_envelope::{AdapterExecuteRequest, CanonicalExecuteRequest, UnresolvedExecuteRequest};
pub use kind::*;
pub use methods::METHOD_NAMES;
pub use plugin_migration::{
    capnp_u32_len, plugin_migration_registration_bytes, require_plugin_migration_json_lists,
    require_plugin_migration_registration, PluginMigration, PluginMigrationOp,
    MAX_PLUGIN_MIGRATION_ID_BYTES, PLUGIN_MIGRATIONS_TABLE,
};
pub use sql_desugar::{desugar_canonical_sql, desugar_execute_request};
#[cfg(feature = "host")]
pub use sql_overflow::{apply_integer_overflow, OverflowDialect};
#[cfg(feature = "host")]
pub use sql_proof::{
    assert_proof_matches_sql, IntegerArithKind, IntegerArithSite, PhysicalAccess,
    ResolvedAssignment, ResolvedStatement, SchemaAction, SqlSpan, TextCollateSite,
    PHYSICAL_STAR_COLUMN,
};
pub use sql_text::{
    admitted_bookclerk_sql_samples, canonical_statements_checksum,
    glob_expanded_like_pattern_bytes, like_pattern_sources, require_function_args_within,
    require_like_patterns_within, require_portable_text, require_portable_text_binds,
    sql_v1_function_calls, sql_v1_helper_is_chunkable, sql_v1_pack_statements,
    sqlite_family_like_divmod_insert_upper_bound, sqlite_family_mechanical_len_upper_bound,
    text_contains_nul, LikePatternSrc, SqlFnCall, D1_PHYSICAL_LIKE_GLOB_PATTERN_BYTES,
    D1_PORTABLE_LIKE_PATTERN_BYTES, LIKE_GLOB_PATTERN_WRAP_BYTES, LIKE_GLOB_REWRITE_OVERHEAD,
    LIKE_GLOB_WRAP_PREFIX, LIKE_GLOB_WRAP_SUFFIX, LIKE_TO_GLOB_KEYWORD_EXTRA,
    SQLITE_FAMILY_DIV_MOD_NULLIF_EXTRA, SQLITE_FAMILY_INSERT_OR_IGNORE_MAX_EXTRA,
};
pub use sql_types::{
    apply_schema_action_to_env, apply_schema_sql_to_env, catalog_companions,
    catalog_page_statement, parse_create_index_sql, parse_create_table_schema,
    parse_drop_index_name, parse_drop_table_name, postgres_identity_function_name,
    postgres_identity_object_digest, postgres_identity_trigger_name, require_sql_v1_helper_arity,
    reserved_catalog_relation_missing, sql_catalog_create_table_sql, sql_catalog_page_rows,
    sql_ddl_create_table_sql, sql_host_bookkeeping_type_env, sql_type_env_from_canonical_ddl,
    sql_type_env_from_canonical_statements, sql_v1_helper_arity, sql_v1_helper_arity_ok,
    sql_v1_ident_in_bounds, statement_sql_hash, typecheck_create_index_sql,
    typecheck_execute_request, ColumnReference, CreateIndexSchema, CreateTableSchema, SqlType,
    SqlTypeEnv, INSERT_SELECT_WRAP_ALIAS, POSTGRES_IDENT_FN_PREFIX, POSTGRES_IDENT_TRIGGER_PREFIX,
    SQL_CATALOG_TABLE, SQL_DDL_TABLE, SQL_IDENTITY_TABLE, SQL_SCHEMA_TABLE,
    SQL_V1_JSON_OBJECT_MAX_ARGS, SQL_V1_MAX_IDENT_BYTES,
};
#[cfg(feature = "host")]
pub use sql_types::{
    catalog_companions_for_action, sql_schema_create_table_sql, typecheck_execute_request_proofs,
};

#[cfg(feature = "host")]
pub use host_roles::{AdapterTransaction, HostAdapterDatabaseSession};
#[cfg(feature = "host")]
pub use host_rpc::HostAdapterDatabaseSessionClient;

pub use db_rpc::{
    canonical_execute_request_hash, decode_db_value_bytes, decode_execute_request_bytes,
    decode_execute_result_reply_bytes, encoded_db_value_bytes, encoded_execute_reply_bytes,
    encoded_execute_request_bytes, encoded_execute_result_reply_bytes,
    encoded_statement_result_bytes,
};
pub use features::{
    negotiate_rpc_features, RpcFeature, FEATURE_SCALAR_LIMITS, FEATURE_STORAGE_COPY,
    FEATURE_STREAMS,
};
pub use jobs::{read_all, stream_copy_keys, StreamCopyHandler, StreamCopySpec};
pub use limits::{
    ScalarLimits, MAX_EVENT_PAYLOAD_BYTES, MAX_LIST_PAGE, MAX_PLUGIN_MIGRATION_OPS,
    MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES, MAX_PLUGIN_MIGRATION_TOTAL_OPS, MAX_SCALAR_BYTES,
    MAX_STREAM_WINDOW_BYTES, PRODUCT_API_VERSION,
};
pub use roles::{
    AdapterDatabaseSession, BindingValues, Bindings, ByteRange, Cancellation, ContentSource,
    Database, Destination, Entrypoints, EventConsumer, EventPublisher, GuestDatabase,
    JobController, JobRunner, NeverCancel, Oidc, PluginCli, PluginWorker, ProgressSink, ReadResult,
    RemoteLibrary, Source,
};
#[cfg(feature = "host")]
pub use rpc::AdapterSessionHandle;
pub use rpc::{
    byte_source_from_async_read, connect_plugin, pull_byte_source_to_writer, serve_plugin,
    serve_plugin_stdio, ContentSourceClient, DatabaseClient, DestinationClient, DestinationServer,
    EventConsumerClient, EventPublisherClient, HostBindings, JobRunnerClient, OidcClient,
    OpenedEntrypoints, PluginCliClient, PluginClient, PluginServer, RemoteLibraryClient,
    SourceClient, SourceServer,
};
pub use rpc_types::{
    CopyResult, DomainEvent, EventResult, ExtensibleConfig, HealthOk, JobCheckpoint, JobInvocation,
    JobInvocationLease, JobOutcome, ListOptions, ListPage, ObjectInfo, ObjectMetadata,
    OidcClientTemplate, PluginDescribe, PutResult, QueryPage, ScalarLimitsDto, WriteOptions,
    JSON_MEDIA_TYPE, MAX_CHECKPOINT_BYTES,
};

/// Embedded JSON Schema for install `plugin.toml` files (shared with language
/// SDK author tools and `bookclerk plugins` validation).
pub const PLUGIN_TOML_SCHEMA_JSON: &str = include_str!("../schema/plugin-toml.json");

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn describe_roundtrip_camel_case() {
        let describe = PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "echo".into(),
            capabilities: PluginCapabilities {
                entrypoints: vec![Entrypoint::Cli],
                consumes: vec![EventConsumerSpec {
                    event_type: "book_acquired".into(),
                    schema_versions: vec![1],
                    supports_suspend: true,
                }],
                ..PluginCapabilities::default()
            },
            ..PluginDescribe::default()
        };
        let v = serde_json::to_value(&describe).unwrap();
        assert!(v.get("apiVersion").is_some());
        assert!(v.get("api_version").is_none());
        assert!(v.get("metadataJson").is_none());
        assert_eq!(v["capabilities"]["entrypoints"][0], "cli");
        let back: PluginDescribe = serde_json::from_value(v).unwrap();
        assert_eq!(back.id, "echo");
        assert!(back.has_entrypoint(Entrypoint::Cli));
        assert!(back.consumes_event("book_acquired"));
        assert!(!back.runs_job("book_acquired"));
    }

    #[test]
    fn capnp_schema_has_no_legacy_json_row_fields() {
        let schema = include_str!("../schema/plugin.capnp");
        assert!(
            !schema.contains("valuesJson"),
            "plugin.capnp must not contain valuesJson"
        );
        assert!(
            !schema.contains("rowsJson"),
            "plugin.capnp must not contain rowsJson"
        );
    }
}
