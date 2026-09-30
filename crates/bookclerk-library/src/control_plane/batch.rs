//! Canonical SQL batches for control-plane writes.
//!
//! Statements stay SQLite-shaped. Adapters lower them. This module does not
//! branch on engine identity.

use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, DbRow, DbValue, ExecuteReply, ExecuteRequest,
    TypedDbStatement,
};

use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Runs one host-authored canonical batch as a single transaction.
///
/// Production stores send the stamped envelope through [`crate::TypedAtomicExec`].
/// In-process tests execute it on the opened connection.
///
/// # Errors
///
/// Returns an error when typecheck, transport, or the engine rejects the batch.
pub(super) async fn execute_host_batch(
    store: &LibraryStore,
    req: ExecuteRequest,
) -> Result<ExecuteReply> {
    store.execute_host_batch(req).await
}

/// Execute-kind statement. Only `rowsAffected` is required.
pub(super) fn exec(sql: &str, parameters: Vec<DbValue>) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.to_string(),
        parameters,
        kind: DbPlanStatementKind::Execute,
        max_rows: 0,
        result_selection: DbResultSelection::AffectedRows,
    }
}

/// Read-only `SELECT` that returns at most `max_rows` rows.
pub(super) fn query(sql: &str, parameters: Vec<DbValue>, max_rows: u32) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.to_string(),
        parameters,
        kind: DbPlanStatementKind::Select,
        max_rows,
        result_selection: DbResultSelection::Rows,
    }
}

/// Text bind.
pub(super) fn text(value: &str) -> DbValue {
    DbValue::Text(value.to_string())
}

/// Integer bind.
pub(super) fn int(value: i64) -> DbValue {
    DbValue::Int64(value)
}

/// Text cell at `index`, or an error when the cell is missing or not text.
pub(super) fn text_cell(row: &DbRow, index: usize) -> Result<String> {
    match row.values.get(index) {
        Some(DbValue::Text(value)) => Ok(value.clone()),
        Some(other) => Err(LibraryError::Other(anyhow::anyhow!(
            "expected text configuration cell, got {other:?}"
        ))),
        None => Err(LibraryError::Other(anyhow::anyhow!(
            "missing configuration result cell {index}"
        ))),
    }
}

/// First statement's affected-row count, or 0 when the reply is empty.
pub(super) fn rows_affected(reply: &ExecuteReply, index: usize) -> u64 {
    reply
        .statements
        .get(index)
        .map(|stmt| stmt.rows_affected)
        .unwrap_or(0)
}

/// Rows from one statement.
pub(super) fn rows(reply: &ExecuteReply, index: usize) -> &[DbRow] {
    reply
        .statements
        .get(index)
        .map(|stmt| stmt.rows.as_slice())
        .unwrap_or(&[])
}

/// Builds an [`ExecuteRequest`] with an empty hash (the host stamps it).
pub(super) fn request(operation_id: &str, statements: Vec<TypedDbStatement>) -> ExecuteRequest {
    ExecuteRequest {
        operation_id: operation_id.to_string(),
        request_hash: String::new(),
        statements,
        deadline_unix_ms: 0,
    }
}
