//! Portable COUNT+mutate serialization via `bookclerk_slots`.

use sea_orm::{ConnectionTrait, StreamTrait};

use crate::error::Result;

/// Slot key for job admit / claim / scratch-quota serialization.
pub const JOB_QUEUE_SLOT: &str = "job-queue";

/// Slot key that serializes in-flight event deliveries for one plugin class.
#[must_use]
pub fn event_inflight_slot(plugin_id: &str, resource_class: &str) -> String {
    let class = if resource_class.trim().is_empty() {
        crate::EVENT_RESOURCE_CLASS_NETWORK
    } else {
        resource_class.trim()
    };
    format!("event-inflight:{plugin_id}:{class}")
}

/// Inserts the slot row if missing, then bumps it (transaction-scoped write lock).
///
/// Canonical SQL uses `?` placeholders. Host transport never lowers; adapters
/// realize `INSERT OR IGNORE` at the execute edge. This helper must run on the
/// caller's transaction, not a nested `BEGIN`. `in_process` of `false` is
/// canonical leftover SQL (production proxy). `true` physically lowers once
/// on the opened adapter connection.
///
/// # Errors
///
/// Returns [`crate::LibraryError::Orm`] when either statement fails.
pub async fn lock_serialization_slot<C>(db: &C, in_process: bool, slot_key: &str) -> Result<()>
where
    C: ConnectionTrait + StreamTrait,
{
    const INSERT: &str = "INSERT OR IGNORE INTO bookclerk_slots (slot_key, bump) \
         SELECT ?, 0 WHERE NOT EXISTS (\
            SELECT 1 FROM bookclerk_slots WHERE slot_key = ?\
         )";
    const BUMP: &str = "UPDATE bookclerk_slots SET bump = bump + 1 WHERE slot_key = ?";
    let env = crate::migrations::host_sql_type_env();
    crate::sql_plan::execute_sql_on(
        in_process,
        db,
        INSERT,
        [slot_key.into(), slot_key.into()],
        env.clone(),
    )
    .await?;
    crate::sql_plan::execute_sql_on(in_process, db, BUMP, [slot_key.into()], env).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bookclerk_plugin_abi::{
        typecheck_execute_request_proofs, DbPlanStatementKind, DbResultSelection, DbValue,
        ExecuteRequest, TypedDbStatement,
    };

    fn exec_stmt(sql: &str, binds: usize) -> TypedDbStatement {
        TypedDbStatement {
            sql: sql.to_string(),
            parameters: vec![DbValue::Text("job-queue".into()); binds],
            kind: DbPlanStatementKind::Execute,
            max_rows: 0,
            result_selection: DbResultSelection::AffectedRows,
        }
    }

    #[test]
    fn serialization_slot_sql_typechecks_against_host_env() {
        const INSERT: &str = "INSERT OR IGNORE INTO bookclerk_slots (slot_key, bump) \
         SELECT ?, 0 WHERE NOT EXISTS (\
            SELECT 1 FROM bookclerk_slots WHERE slot_key = ?\
         )";
        const BUMP: &str = "UPDATE bookclerk_slots SET bump = bump + 1 WHERE slot_key = ?";
        let env = crate::migrations::host_sql_type_env();
        let req = ExecuteRequest {
            operation_id: "slot".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![exec_stmt(INSERT, 2), exec_stmt(BUMP, 1)],
        };
        typecheck_execute_request_proofs(&req, &env).expect("slot SQL must typecheck");
    }
}
