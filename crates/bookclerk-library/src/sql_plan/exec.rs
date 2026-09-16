//! Run a typed [`ExecuteRequest`] on a SeaORM connection (one native transaction).

use bookclerk_db_exec::{ExecCaps, GuestReceiptPersist};
use bookclerk_plugin_abi::ExecuteRequest;

use crate::atomic_ops::DbAtomicResult;
use crate::error::{LibraryError, Result};
use crate::sql_plan::interpret::interpret_typed_exec;
use crate::sql_plan::CompiledAtomic;

pub use bookclerk_db_exec::AtomicSession;

/// Executes a compiled named atomic as one transaction and interprets the reply.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a statement fails. Application statuses
/// are returned as [`DbAtomicResult`], not errors.
pub async fn execute_compiled_on(
    db: &sea_orm::DatabaseConnection,
    compiled: CompiledAtomic,
    timing_source: &str,
) -> Result<DbAtomicResult> {
    execute_compiled_on_capped(db, compiled, timing_source, 0).await
}

/// Like [`execute_compiled_on`], failing when a statement returns more than `max_result_rows`.
///
/// `max_result_rows` of `0` means unlimited.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a statement fails or exceeds the row cap.
pub async fn execute_compiled_on_capped(
    db: &sea_orm::DatabaseConnection,
    compiled: CompiledAtomic,
    timing_source: &str,
    max_result_rows: u32,
) -> Result<DbAtomicResult> {
    let hash = compiled.expected_hash.clone();
    let reply = execute_typed_on(db, &compiled.request, timing_source, max_result_rows).await?;
    Ok(interpret_typed_exec(&compiled, &reply, &hash))
}

/// Executes a typed request as one transaction.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a statement fails.
pub async fn execute_typed_on(
    db: &sea_orm::DatabaseConnection,
    req: &ExecuteRequest,
    timing_source: &str,
    max_result_rows: u32,
) -> Result<bookclerk_plugin_abi::ExecuteReply> {
    execute_typed_on_session(
        db,
        req,
        timing_source,
        max_result_rows,
        AtomicSession::default(),
    )
    .await
}

/// [`execute_typed_on`] with session cancel / deadline checks.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a statement fails or the session is interrupted.
pub async fn execute_typed_on_session(
    db: &sea_orm::DatabaseConnection,
    req: &ExecuteRequest,
    timing_source: &str,
    max_result_rows: u32,
    session: AtomicSession,
) -> Result<bookclerk_plugin_abi::ExecuteReply> {
    bookclerk_db_exec::execute_typed_on_session(
        db,
        req,
        GuestReceiptPersist::default(),
        timing_source,
        ExecCaps::from(max_result_rows),
        session.with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .map_err(LibraryError::from_db_err)
}
