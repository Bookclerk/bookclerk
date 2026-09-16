//! Host-mediated plugin binding schema lifecycle.
//!
//! Each isolated binding database records durable [`SchemaState`] in
//! `bookclerk_schema_migrations` (same table shape as the library). Host bootstrap
//! ([`crate::migrations::binding_bootstrap_ops`]) is the unreleased binding
//! schema. Progression is ordered, host-mediated BookclerkSQL only — no
//! `pg_dump`, `VACUUM INTO`, D1 REST migrate, or other backend-native escape
//! hatch.
//!
//! Retry matches library schema apply: uniqueness / duplicate-object /
//! unavailable after re-read of durable state. Concurrent openers that miss
//! the marker retry the same unit.
//!
//! Plugin-owned tables are admitted BookclerkSQL through startup
//! [`crate::migrations::prove_plugin_migration_sequence`] /
//! [`crate::migrations::apply_plugin_migrations`] and do **not** bump
//! host-owned bootstrap [`SchemaState`]. Restore writes captured rows
//! (including `bookclerk_plugin_migrations`) and does **not** run plugin migrations
//! inside the restore transaction.

use std::time::Duration;

use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, ExecuteRequest, SqlTypeEnv, TypedDbStatement,
};
use sea_orm::DatabaseConnection;

use crate::error::{LibraryError, Result};
use crate::host_schema::{current_schema_state, ensure_schema_migrations};
use crate::migrations::{
    binding_bootstrap_ops, binding_bootstrap_statements, binding_unreleased_checksum,
    prove_migration_ops, unreleased_state_marker_sql, BINDING_SCHEMA_VERSION,
    SCHEMA_MIGRATIONS_DDL,
};
use crate::schema_state::SchemaState;
use crate::sql_plan::execute_typed_on_binding;

/// Statements to apply for `state`, or `None` when the binding already matches.
///
/// # Errors
///
/// Returns when `state` is a checksum/base mismatch or an unexpected frozen
/// binding (this binary has no frozen binding plan).
pub fn binding_bootstrap_plan(state: &SchemaState) -> Result<Option<Vec<String>>> {
    match state {
        SchemaState::Uninitialized => {
            let mut stmts = vec![SCHEMA_MIGRATIONS_DDL.to_string()];
            stmts.extend(binding_bootstrap_statements().iter().cloned());
            stmts.push(unreleased_state_marker_sql(
                &binding_unreleased_checksum(),
                BINDING_SCHEMA_VERSION,
            ));
            Ok(Some(stmts))
        }
        SchemaState::Unreleased {
            base_version,
            checksum,
        } => {
            let expected = binding_unreleased_checksum();
            if *base_version != BINDING_SCHEMA_VERSION {
                return Err(LibraryError::Schema(format!(
                    "binding unreleased@base{base_version} is not this binary's binding base \
                     {BINDING_SCHEMA_VERSION}"
                )));
            }
            if checksum != &expected {
                return Err(LibraryError::Schema(format!(
                    "binding unreleased checksum {checksum} does not match this binary \
                     ({expected}); restore the binding or drop it — Bookclerk will not \
                     reshape a plugin binding in place"
                )));
            }
            Ok(None)
        }
        SchemaState::Frozen { version, checksum } => Err(LibraryError::Schema(format!(
            "binding is frozen@{version}+{checksum}; this binary has no frozen binding plan"
        ))),
    }
}

/// Applies host-owned binding bootstrap when the binding is uninitialized.
///
/// Matching [`SchemaState::Unreleased`] is a no-op. Restore that rewrites
/// `bookclerk_schema_migrations` as ordinary rows will take this no-op path and will not
/// re-run plugin-owned DDL.
///
/// # Errors
///
/// Returns when schema apply or the binding state machine fails closed.
pub async fn apply_binding_bootstrap(db: &DatabaseConnection) -> Result<()> {
    ensure_schema_migrations(db).await?;
    let state = current_schema_state(db).await?;
    let Some(stmts) = binding_bootstrap_plan(&state)? else {
        return Ok(());
    };
    prove_migration_ops(binding_bootstrap_ops())?;
    let mut delay_ms = 20u64;
    for attempt in 0..8 {
        match run_binding_batch(db, stmts.clone()).await {
            Ok(()) => return Ok(()),
            Err(err) => match current_schema_state(db).await {
                Ok(SchemaState::Unreleased { checksum, .. })
                    if checksum == binding_unreleased_checksum() =>
                {
                    return Ok(());
                }
                Ok(_) | Err(_) => {
                    if attempt + 1 < 8 && err.is_schema_apply_retryable() {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(2).min(250);
                        continue;
                    }
                    return Err(err);
                }
            },
        }
    }
    Err(LibraryError::Schema(
        "binding bootstrap apply exhausted retries".into(),
    ))
}

/// Runs binding bootstrap statements as one typed atomic unit.
async fn run_binding_batch(db: &DatabaseConnection, stmts: Vec<String>) -> Result<()> {
    if stmts.is_empty() {
        return Ok(());
    }
    let req = ExecuteRequest {
        operation_id: "binding-bootstrap".into(),
        request_hash: String::new(),
        deadline_unix_ms: 0,
        statements: stmts
            .into_iter()
            .map(|sql| TypedDbStatement {
                sql,
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            })
            .collect(),
    };
    execute_typed_on_binding(db, &req, "schema_txn", 0, &SqlTypeEnv::new()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_schema::current_schema_state;

    #[test]
    fn matching_unreleased_is_noop() {
        let checksum = binding_unreleased_checksum();
        let state = SchemaState::Unreleased {
            base_version: BINDING_SCHEMA_VERSION,
            checksum,
        };
        assert!(binding_bootstrap_plan(&state).unwrap().is_none());
    }

    #[test]
    fn checksum_mismatch_fails_closed() {
        let state = SchemaState::Unreleased {
            base_version: BINDING_SCHEMA_VERSION,
            checksum: "deadbeef".repeat(8),
        };
        let err = binding_bootstrap_plan(&state).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn uninitialized_emits_ops_and_marker() {
        let stmts = binding_bootstrap_plan(&SchemaState::Uninitialized)
            .unwrap()
            .expect("apply");
        assert!(stmts.iter().any(|s| s.contains("bookclerk_receipts")));
        assert!(stmts
            .iter()
            .any(|s| s.contains("bookclerk_schema_migrations")));
        assert!(stmts
            .iter()
            .any(|s| s.contains("INSERT INTO bookclerk_schema_migrations")));
        assert!(!stmts
            .iter()
            .any(|s| s.contains("VACUUM") || s.contains("pg_dump")));
    }

    #[tokio::test]
    async fn apply_is_idempotent_and_does_not_rerun_on_match() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .unwrap();
        apply_binding_bootstrap(&db).await.unwrap();
        let first = current_schema_state(&db).await.unwrap();
        apply_binding_bootstrap(&db).await.unwrap();
        let second = current_schema_state(&db).await.unwrap();
        assert_eq!(first, second);
        match first {
            SchemaState::Unreleased {
                base_version,
                checksum,
            } => {
                assert_eq!(base_version, BINDING_SCHEMA_VERSION);
                assert_eq!(checksum, binding_unreleased_checksum());
            }
            other => panic!("expected unreleased binding, got {other}"),
        }
    }
}
