//! Authoritative Bookclerk plugin ABI (`api_version` 2).
//!
//! Version 2 is the product object-capability ABI (Cap'n Proto RPC, role
//! classes, transferred byte streams). JSON DTOs in this crate describe the
//! payloads carried inside `Text` fields of that ABI (`describe().metadataJson`,
//! role `paramsJson`, `cliInvoke` params/results) — never a transport of their
//! own.
//!
//! # Audience
//!
//! - **Guest authors** — implement the Cap'n Proto roles against these DTOs
//!   (via `bookclerk-plugin-sdk`, `@bookclerk/plugin-sdk`, or a language binding
//!   generated from the same schema).
//! - **Host / SDK maintainers** — drive role capabilities, seal credentials,
//!   and upsert library rows without depending on store-specific crates.
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
//! enums, and the "JSON payload contracts" section that types the JSON carried
//! in `Text` fields. Types here are the Rust projection; the TypeScript and
//! Python SDK projections are generated from the same schema by
//! `scripts/gen-plugin-abi.py`, which also drift-checks this crate. Wire DTO
//! fields serialize as **camelCase**.
//!
//! Install manifests validate against [`PLUGIN_TOML_SCHEMA_JSON`]
//! (`schema/plugin-toml.json`).
//!
//! # Versioning
//!
//! [`PRODUCT_API_VERSION`] is `2` (object-capability Cap'n Proto / Workers
//! RPC). Product spawn requires `plugin.toml` `api_version = 2`. There is no
//! `protocol` key.
//!
//! # Modules
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`methods`] | Capability / consent method name constants (`login`, `onEvent`, …) |
//! | [`types`] | Shared DTOs (identity metadata, health, CLI) |
//! | [`kind`] | Kind-specific DTOs (source / integration / output) |
//! | [`db`] | Host-private database connect params (feature `host`) |
//! | [`error`] | [`PluginError`] / [`PluginErrorCode`] |
//! | [`plugin_capnp`] | Generated Cap'n Proto RPC interfaces |

pub mod db;
pub mod db_execute;
mod db_rpc;
pub mod db_value;
pub mod error;
mod features;
pub mod guest_sql;
#[cfg(feature = "host")]
pub(crate) mod host_envelope;
#[cfg(feature = "host")]
mod host_roles;
#[cfg(feature = "host")]
mod host_rpc;
mod jobs;
pub mod kind;
mod limits;
pub mod methods;
mod roles;
mod rpc;
mod rpc_types;
mod sdk_wire;
mod sql_overflow;
mod sql_proof;
mod sql_text;
pub mod sql_types;
pub mod types;

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

#[cfg(test)]
mod wire_fixtures;

#[cfg(feature = "host")]
pub use db::{connect_params_from_context, database_context_from_params, DbConnectParams};
pub use db::{
    database_adapter_config_from_context, database_context_from_adapter_config,
    DATABASE_ADAPTER_CONFIG_MEDIA_TYPE,
};
#[cfg(feature = "host")]
pub use db_execute::lowered_statement_preflight_len_proven;
pub use db_execute::{
    lowered_statement_preflight_len, lowered_statement_upper_bound_len, sql_payload_bytes,
    sql_payload_exceeds, DbBootstrap, DbCapabilities, DbColumn, DbPlanStatementKind,
    DbResultSelection, DbRow, DbTiming, ExecuteReply, ExecuteRequest, StatementResult,
    TypedDbStatement, D1_MAX_BINDS, D1_MAX_FUNCTION_ARGS, D1_MAX_PAYLOAD_BYTES,
    D1_MAX_SCHEMA_COLUMNS, D1_MAX_SQL_STATEMENT_BYTES, FIRST_PARTY_MAX_RESULT_BYTES,
    FIRST_PARTY_MAX_RESULT_ROWS, FIRST_PARTY_MAX_STATEMENTS, HOST_MIN_BINDS, HOST_MIN_CELL_BYTES,
    HOST_MIN_FUNCTION_ARGS, HOST_MIN_PATTERN_BYTES, HOST_MIN_PAYLOAD_BYTES, HOST_MIN_RESULT_BYTES,
    HOST_MIN_RESULT_ROWS, HOST_MIN_SCHEMA_COLUMNS, HOST_MIN_STATEMENTS, POSTGRES_MAX_BINDS,
    POSTGRES_MAX_FUNCTION_ARGS, POSTGRES_MAX_SCHEMA_COLUMNS, SQLITE_MAX_BINDS,
    SQLITE_MAX_FUNCTION_ARGS, SQLITE_MAX_PATTERN_BYTES, SQLITE_MAX_SCHEMA_COLUMNS,
    SQL_CONTRACT_VERSION,
};
pub use db_value::{db_type_from_declared, normalize_db_value_for_column, DbType, DbValue};
pub use error::{PluginError, PluginErrorCode, Result};
pub use guest_sql::{
    authorize_guest_sql_policy, guest_statement_kind, parse_guest_sql_refs,
    returning_single_row_proven, statement_is_ddl, validate_guest_execute_request,
    validate_guest_execute_request_for_policy, validate_sql_v1_grammar, GuestSqlPolicy,
    GuestSqlRefs,
};
#[cfg(feature = "host")]
pub use host_envelope::{GuestReceiptPersist, HostExecuteEnvelope};
pub use kind::*;
pub use methods::METHOD_NAMES;
#[cfg(feature = "host")]
pub use sql_overflow::{apply_integer_overflow, OverflowDialect};
#[cfg(feature = "host")]
pub use sql_proof::{
    assert_proof_matches_sql, IntegerArithKind, IntegerArithSite, PhysicalAccess,
    ResolvedAssignment, ResolvedStatement, SchemaAction, SqlSpan, TextCollateSite,
    PHYSICAL_STAR_COLUMN,
};
pub use sql_text::{
    canonical_statements_checksum, glob_expanded_like_pattern_bytes, like_pattern_sources,
    require_function_args_within, require_like_patterns_within, require_portable_text,
    require_portable_text_binds, sql_v1_function_calls, sql_v1_helper_is_chunkable,
    sql_v1_pack_statements, sqlite_family_like_divmod_insert_upper_bound,
    sqlite_family_mechanical_len_upper_bound, text_contains_nul, LikePatternSrc, SqlFnCall,
    D1_PHYSICAL_LIKE_GLOB_PATTERN_BYTES, D1_PORTABLE_LIKE_PATTERN_BYTES,
    LIKE_GLOB_PATTERN_WRAP_BYTES, LIKE_GLOB_REWRITE_OVERHEAD, LIKE_GLOB_WRAP_PREFIX,
    LIKE_GLOB_WRAP_SUFFIX, LIKE_TO_GLOB_KEYWORD_EXTRA, SQLITE_FAMILY_DIV_MOD_NULLIF_EXTRA,
    SQLITE_FAMILY_INSERT_OR_IGNORE_MAX_EXTRA,
};
pub use sql_types::{
    apply_schema_action_to_env, apply_schema_sql_to_env, catalog_companions,
    catalog_page_statement, parse_create_table_schema, parse_drop_table_name,
    postgres_identity_function_name, postgres_identity_object_digest,
    postgres_identity_trigger_name, require_sql_v1_helper_arity, reserved_catalog_relation_missing,
    split_sql_statements, sql_catalog_create_table_sql, sql_catalog_page_rows,
    sql_host_bookkeeping_type_env, sql_type_env_from_canonical_ddl,
    sql_type_env_from_canonical_statements, sql_v1_helper_arity, sql_v1_helper_arity_ok,
    sql_v1_ident_in_bounds, statement_sql_hash, typecheck_execute_request, CreateTableSchema,
    SqlType, SqlTypeEnv, INSERT_SELECT_WRAP_ALIAS, POSTGRES_IDENT_FN_PREFIX,
    POSTGRES_IDENT_TRIGGER_PREFIX, SQL_CATALOG_TABLE, SQL_IDENTITY_TABLE, SQL_SCHEMA_TABLE,
    SQL_V1_MAX_IDENT_BYTES,
};
#[cfg(feature = "host")]
pub use sql_types::{
    catalog_companions_for_action, sql_schema_create_table_sql, typecheck_execute_request_proofs,
};
pub use types::*;

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
    ScalarLimits, MAX_EVENT_PAYLOAD_BYTES, MAX_LIST_PAGE, MAX_SCALAR_BYTES,
    MAX_STREAM_WINDOW_BYTES, PRODUCT_API_VERSION,
};
pub use roles::{
    AdapterDatabaseSession, ByteRange, Cancellation, ContentSource, ContentSourceContext, Database,
    DatabaseContext, Destination, GuestDatabase, Integration, IntegrationContext, JobHandler,
    JobHandlerContext, NeverCancel, PluginRoot, ProgressSink, ReadResult, Source,
};
#[cfg(feature = "host")]
pub use rpc::AdapterSessionHandle;
pub use rpc::{
    byte_source_from_async_read, connect_plugin, pull_byte_source_to_writer, serve_plugin,
    serve_plugin_stdio, ContentSourceClient, DatabaseClient, DestinationClient, DestinationServer,
    IntegrationClient, PluginClient, PluginServer, SourceClient, SourceServer,
};
pub use rpc_types::{
    CopyResult, DestinationContext, DomainEvent, EventResult, ExtensibleConfig, HealthOk,
    JobCheckpoint, JobInvocation, JobInvocationLease, JobOutcome, ListOptions, ListPage,
    ObjectInfo, ObjectMetadata, OidcClientTemplate, PluginDescribe, PutResult, QueryPage,
    ScalarLimitsDto, SourceContext, WorkerContext, WriteOptions, MAX_CHECKPOINT_BYTES,
};

/// Embedded JSON Schema for install `plugin.toml` files (shared with language
/// SDK author tools and `bookclerk plugins` validation).
pub const PLUGIN_TOML_SCHEMA_JSON: &str = include_str!("../schema/plugin-toml.json");

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn metadata_roundtrip_camel_case() {
        let meta = PluginMetadata {
            api_version: PRODUCT_API_VERSION,
            id: "echo".into(),
            kind: "integration".into(),
            capabilities: vec!["health".into()],
            ..PluginMetadata::default()
        };
        let v = serde_json::to_value(&meta).unwrap();
        assert!(v.get("apiVersion").is_some());
        assert!(v.get("api_version").is_none());
        let back: PluginMetadata = serde_json::from_value(v).unwrap();
        assert_eq!(back.id, "echo");
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
