//! Run a generic [`DbAtomicPlan`] on a SeaORM connection (one native transaction).

use super::host_ir::{DbAtomicPlan, DbPlanExecResult};

use crate::atomic_ops::DbAtomicResult;
use crate::error::{LibraryError, Result};
use crate::sql_plan::interpret::interpret_exec;

pub use bookclerk_db_exec::AtomicSession;

/// Executes `plan` as one transaction and interprets receipt/outcome rows.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a statement fails. Application statuses
/// are returned as [`DbAtomicResult`], not errors.
pub async fn execute_plan_on(
    db: &sea_orm::DatabaseConnection,
    plan: &DbAtomicPlan,
    expected_hash: &str,
    operation_id: &str,
    timing_source: &str,
) -> Result<DbAtomicResult> {
    execute_plan_on_capped(db, plan, expected_hash, operation_id, timing_source, 0).await
}

/// Like [`execute_plan_on`], failing when a statement returns more than `max_result_rows`.
///
/// `max_result_rows` of `0` means unlimited.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a statement fails or exceeds the row cap.
pub async fn execute_plan_on_capped(
    db: &sea_orm::DatabaseConnection,
    plan: &DbAtomicPlan,
    expected_hash: &str,
    operation_id: &str,
    timing_source: &str,
    max_result_rows: u32,
) -> Result<DbAtomicResult> {
    let exec =
        execute_statements_on(db, plan, operation_id, timing_source, max_result_rows).await?;
    Ok(interpret_exec(plan, &exec, expected_hash))
}

/// Executes `plan` as one transaction and returns generic statement results.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a statement fails.
pub async fn execute_statements_on(
    db: &sea_orm::DatabaseConnection,
    plan: &DbAtomicPlan,
    operation_id: &str,
    timing_source: &str,
    max_result_rows: u32,
) -> Result<DbPlanExecResult> {
    bookclerk_db_exec::execute_statements_on_session(
        db,
        plan,
        operation_id,
        timing_source,
        max_result_rows,
        AtomicSession::default().with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .map_err(LibraryError::from_db_err)
}

/// [`execute_statements_on`] with session cancel / deadline checks.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when a statement fails or the session is interrupted.
pub async fn execute_statements_on_session(
    db: &sea_orm::DatabaseConnection,
    plan: &DbAtomicPlan,
    operation_id: &str,
    timing_source: &str,
    max_result_rows: u32,
    session: AtomicSession,
) -> Result<DbPlanExecResult> {
    bookclerk_db_exec::execute_statements_on_session(
        db,
        plan,
        operation_id,
        timing_source,
        max_result_rows,
        session.with_type_env(crate::migrations::host_sql_type_env()),
    )
    .await
    .map_err(LibraryError::from_db_err)
}
