//! Host-internal SQL plan intermediate representation.
//!
//! Generic atomic batches compiled by `bookclerk-library` and executed by
//! database guests. These types are **not** part of the public plugin ABI;
//! guests receive typed [`bookclerk_plugin_abi::ExecuteRequest`] on the wire.

use bookclerk_plugin_abi::DbPlanStatementKind;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// One parameterized statement in a host-authored [`DbAtomicPlan`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DbPlanStatement {
    /// Dialect-specific SQL (SQLite `?` or Postgres `$1`).
    pub sql: String,
    /// Ordered JSON binds (null, bool, number, string, `b64:` blobs, or
    /// [`sea_null`] objects so Postgres can distinguish BYTEA/INTEGER nulls).
    #[serde(default)]
    pub binds: Vec<JsonValue>,
    /// Whether the guest should collect rows or only `rowsAffected`.
    #[serde(default)]
    pub kind: DbPlanStatementKind,
    /// Proven upper bound on rows this statement returns (`0` = unproven).
    #[serde(default)]
    pub max_rows: u32,
}

impl DbPlanStatement {
    /// Statement with an unproven row bound (`maxRows = 0`).
    #[must_use]
    pub fn new(sql: impl Into<String>, binds: Vec<JsonValue>, kind: DbPlanStatementKind) -> Self {
        Self {
            sql: sql.into(),
            binds,
            kind,
            max_rows: 0,
        }
    }
}

/// JSON object key for a typed SQL null (`{"$sea_null": "Bytes"}`).
pub const SEA_NULL_KEY: &str = "$sea_null";

/// Wire JSON for a typed null bind of SeaORM `kind` (`Bytes`, `BigInt`, `String`, …).
#[must_use]
pub fn sea_null(kind: &str) -> JsonValue {
    let mut map = serde_json::Map::new();
    map.insert(SEA_NULL_KEY.into(), JsonValue::String(kind.to_string()));
    JsonValue::Object(map)
}

/// Returns the `$sea_null` kind when `v` is a typed-null object.
#[must_use]
pub fn sea_null_kind(v: &JsonValue) -> Option<&str> {
    v.get(SEA_NULL_KEY)?.as_str()
}

/// Generic atomic batch: ordered SQL with outcome/receipt selectors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DbAtomicPlan {
    /// Statements run as one SQL transaction, in order.
    pub statements: Vec<DbPlanStatement>,
    /// Index of the application-status `SELECT` when receipts are not used.
    #[serde(default)]
    pub outcome_index: u32,
    /// Index of a payload `SELECT` when the op returns a JSON record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_index: Option<u32>,
    /// Index of the receipt `SELECT` immediately after prune (replay detect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_receipt_index: Option<u32>,
    /// Index of the final receipt `SELECT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_select_index: Option<u32>,
}

/// Host-generated idempotency envelope for in-process atomic execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DbAtomicRequest {
    /// Caller-chosen idempotency key (UUID). Retries must reuse the same id.
    pub operation_id: String,
    /// SHA-256 hex of the idempotency-relevant request; compared on receipt replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_hash: Option<String>,
    /// Generic statement plan. Guests fail closed when this is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<DbAtomicPlan>,
    /// Guest-visible deadline (unix ms). Transport metadata; not hashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
}

impl DbAtomicRequest {
    /// Envelope for a host-authored generic plan.
    #[must_use]
    pub fn with_plan(
        operation_id: impl Into<String>,
        request_hash: impl Into<String>,
        plan: DbAtomicPlan,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            request_hash: Some(request_hash.into()),
            plan: Some(plan),
            deadline_unix_ms: None,
        }
    }
}

/// Optional engine timing for atomic execution. Not part of the idempotency hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DbAtomicTiming {
    /// Monotonic duration of this plugin-handler attempt.
    pub attempt_elapsed_us: u64,
    /// Engine-reported SQL/transaction time when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_execution_us: Option<u64>,
    /// How `db_execution_us` was measured (`d1_sql_duration`, `sqlite_txn`, `postgres_txn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_timing_source: Option<String>,
}

/// Rows produced by one statement in a guest atomic batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DbPlanStmtExecResult {
    /// Result-set rows (empty for DML without `RETURNING`).
    pub rows: Vec<JsonValue>,
    /// Engine-reported rows affected (wire `rowsAffected`).
    pub rows_affected: u64,
}

/// Generic guest result of one atomic batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DbPlanExecResult {
    /// Echo of the request `operationId`.
    pub operation_id: String,
    /// Per-statement rows and `rowsAffected`, in plan order.
    pub statements: Vec<DbPlanStmtExecResult>,
    /// Handler/engine timing. Not hashed for idempotency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<DbAtomicTiming>,
}

/// Sentinel SQL for the host-internal atomic proxy transport.
pub const DB_ATOMIC_SENTINEL: &str = "bookclerk.atomic";

/// Sentinel SQL for capability negotiation (host-internal).
pub const DB_CAPABILITIES_SENTINEL: &str = "bookclerk.capabilities";
