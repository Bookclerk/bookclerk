//! Production RPC-host plugin-migration apply: progress is independent of retries.
//!
//! [`apply_registered_plugin_migrations`] is the control flow used by
//! [`super::database::ExternalDatabase::ensure_plugin_migrations`]. Tests in
//! this module exercise that helper directly (not only the SeaORM library
//! walk).

use std::time::Duration;

use async_trait::async_trait;
use bookclerk_library::{
    next_pending_plugin_migration, plugin_apply_statements, plugin_journal_has_entry,
    require_history_prefix, PluginMigrationHistory, PluginMigrationSequence, ProvenPluginMigration,
    MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS,
};
use bookclerk_plugin_sdk::{
    DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
};

use crate::{PluginError, Result as PluginResult};

/// Host that can re-read the durable journal and execute one atomic apply unit.
#[async_trait]
pub(crate) trait PluginMigrationApplyHost: Send + Sync {
    /// Load host-private `plugin_migrations` rows for this binding.
    async fn load_plugin_migration_history(&self) -> PluginResult<PluginMigrationHistory>;
    /// Execute one short atomic unit (slot + ops + journal append).
    ///
    /// `applied_prefix` is the durable journal length / expected ordinal used
    /// to stamp the binding catalog (`plugin_binding_type_env`).
    async fn execute_plugin_migration_apply(
        &self,
        operation_id: String,
        statements: Vec<String>,
        applied_prefix: usize,
    ) -> PluginResult<()>;
}

/// Host-private execute operation id: ordinal + opaque id + retry attempt.
///
/// The retry number is not a plugin-visible migration version.
#[must_use]
pub(crate) fn plugin_migrate_operation_id(
    owner: &str,
    binding: &str,
    ordinal: usize,
    migration_id: &str,
    attempt: usize,
) -> String {
    let id = {
        let mut end = migration_id.len().min(64);
        while end > 0 && !migration_id.is_char_boundary(end) {
            end -= 1;
        }
        &migration_id[..end]
    };
    format!("plugin-migrate-{owner}-{binding}-ord{ordinal}-{id}-try{attempt}")
}

/// Typed execute envelope matching production binding schema apply.
#[must_use]
pub(crate) fn plugin_migration_apply_request(
    operation_id: String,
    sqls: Vec<String>,
) -> ExecuteRequest {
    ExecuteRequest {
        operation_id,
        request_hash: String::new(),
        deadline_unix_ms: 0,
        statements: sqls
            .into_iter()
            .map(|sql| TypedDbStatement {
                sql,
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Discard,
            })
            .collect(),
    }
}

/// True when a binding migration batch may be retried after re-reading the ledger.
#[must_use]
pub(crate) fn plugin_schema_apply_retryable(err: &PluginError) -> bool {
    if err.is_ambiguous_transport() {
        return true;
    }
    matches!(
        bookclerk_db_exec::classify_db_err_message(&err.to_string()),
        bookclerk_db_exec::DbErrorClass::Unavailable | bookclerk_db_exec::DbErrorClass::Conflict
    )
}

/// Outer durable-progress loop used by the production RPC host.
///
/// Retry accounting is per expected migration (inner loop), not per
/// registration length.
///
/// # Errors
///
/// Returns when prefix verification fails, a migration exhausts retries, or
/// apply fails permanently.
pub(crate) async fn apply_registered_plugin_migrations<H>(
    host: &H,
    owner: &str,
    binding: &str,
    registered: &PluginMigrationSequence,
) -> PluginResult<()>
where
    H: PluginMigrationApplyHost,
{
    loop {
        let history = host.load_plugin_migration_history().await?;
        let Some((ordinal, migration)) = next_pending_plugin_migration(&history, registered)
            .map_err(|err| PluginError::message(err.to_string()))?
        else {
            return Ok(());
        };
        apply_one_registered_plugin_migration(host, owner, binding, ordinal, migration, registered)
            .await?;
    }
}

/// Inner bounded retry loop for exactly one expected migration.
async fn apply_one_registered_plugin_migration<H>(
    host: &H,
    owner: &str,
    binding: &str,
    ordinal: usize,
    migration: &ProvenPluginMigration,
    registered: &PluginMigrationSequence,
) -> PluginResult<()>
where
    H: PluginMigrationApplyHost,
{
    let mut delay_ms = 20u64;
    let ordinal_i64 = i64::try_from(ordinal).unwrap_or(i64::MAX);
    for attempt in 0..MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS {
        let history = host.load_plugin_migration_history().await?;
        if plugin_journal_has_entry(&history, ordinal_i64, &migration.id, &migration.checksum) {
            return Ok(());
        }
        require_history_prefix(&history, registered)
            .map_err(|err| PluginError::message(err.to_string()))?;
        if history.len() != ordinal {
            return Ok(());
        }
        let stmts = plugin_apply_statements(ordinal_i64, migration);
        let operation_id =
            plugin_migrate_operation_id(owner, binding, ordinal, &migration.id, attempt);
        match host
            .execute_plugin_migration_apply(operation_id, stmts, ordinal)
            .await
        {
            Ok(()) => return Ok(()),
            Err(err) => match host.load_plugin_migration_history().await {
                Ok(observed)
                    if plugin_journal_has_entry(
                        &observed,
                        ordinal_i64,
                        &migration.id,
                        &migration.checksum,
                    ) =>
                {
                    return Ok(());
                }
                Ok(observed) => {
                    require_history_prefix(&observed, registered)
                        .map_err(|e| PluginError::message(e.to_string()))?;
                    if observed.len() != ordinal {
                        return Ok(());
                    }
                    if plugin_schema_apply_retryable(&err)
                        && attempt + 1 < MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(2).min(250);
                        continue;
                    }
                    if plugin_schema_apply_retryable(&err) {
                        return Err(PluginError::message(format!(
                            "plugin migration `{}` for `{owner}/{binding}` exhausted retries \
                                 at ordinal {ordinal}",
                            migration.id
                        )));
                    }
                    return Err(err);
                }
                Err(_)
                    if plugin_schema_apply_retryable(&err)
                        && attempt + 1 < MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS =>
                {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = delay_ms.saturating_mul(2).min(250);
                    continue;
                }
                Err(_) => return Err(err),
            },
        }
    }
    Err(PluginError::message(format!(
        "plugin migration `{}` for `{owner}/{binding}` exhausted retries at ordinal {ordinal}",
        migration.id
    )))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use bookclerk_library::{
        apply_binding_bootstrap, load_plugin_migration_history, next_pending_plugin_migration,
        plugin_binding_type_env, prove_plugin_migration_sequence, PluginMigrationSequence,
    };
    use bookclerk_plugin_abi::{PluginMigration, PluginMigrationOp};
    use sea_orm::DatabaseConnection;

    #[derive(Clone, Copy)]
    enum ApplyScript {
        Commit,
        FailRetryable,
        CommitThenAmbiguous,
    }

    struct ScriptedApplyHost {
        db: DatabaseConnection,
        registered: PluginMigrationSequence,
        script: Mutex<VecDeque<ApplyScript>>,
        executed_ids: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl PluginMigrationApplyHost for ScriptedApplyHost {
        async fn load_plugin_migration_history(&self) -> PluginResult<PluginMigrationHistory> {
            load_plugin_migration_history(&self.db)
                .await
                .map_err(|err| PluginError::message(err.to_string()))
        }

        async fn execute_plugin_migration_apply(
            &self,
            operation_id: String,
            statements: Vec<String>,
            applied_prefix: usize,
        ) -> PluginResult<()> {
            self.executed_ids.lock().unwrap().push(operation_id.clone());
            let behavior = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ApplyScript::Commit);
            match behavior {
                ApplyScript::FailRetryable => {
                    Err(PluginError::unavailable("injected retryable apply failure"))
                }
                ApplyScript::Commit => {
                    commit_apply(
                        &self.db,
                        operation_id,
                        statements,
                        &self.registered,
                        applied_prefix,
                    )
                    .await
                }
                ApplyScript::CommitThenAmbiguous => {
                    commit_apply(
                        &self.db,
                        operation_id,
                        statements,
                        &self.registered,
                        applied_prefix,
                    )
                    .await?;
                    Err(PluginError::unavailable(
                        "lost reply after durable apply commit",
                    ))
                }
            }
        }
    }

    async fn commit_apply(
        db: &DatabaseConnection,
        operation_id: String,
        statements: Vec<String>,
        registered: &PluginMigrationSequence,
        applied_prefix: usize,
    ) -> PluginResult<()> {
        let req = plugin_migration_apply_request(operation_id, statements);
        let catalog = plugin_binding_type_env(registered, applied_prefix);
        bookclerk_library::sql_plan::execute_typed_on_binding(
            db,
            &req,
            "plugin_migrate_txn",
            0,
            &catalog,
        )
        .await
        .map_err(|err| PluginError::message(err.to_string()))?;
        Ok(())
    }

    async fn binding_db() -> DatabaseConnection {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .unwrap();
        apply_binding_bootstrap(&db).await.unwrap();
        db
    }

    fn schema(sql: &str) -> PluginMigrationOp {
        PluginMigrationOp::Schema(sql.to_string())
    }

    fn n_table_migs(n: usize) -> Vec<PluginMigration> {
        (0..n)
            .map(|i| PluginMigration {
                id: format!("m{i:03}"),
                operations: vec![schema(&format!(
                    "CREATE TABLE IF NOT EXISTS t{i} (id INTEGER PRIMARY KEY NOT NULL)"
                ))],
            })
            .collect()
    }

    fn seq(migrations: Vec<PluginMigration>) -> PluginMigrationSequence {
        prove_plugin_migration_sequence(migrations).expect("prove")
    }

    fn assert_history_matches(
        history: &PluginMigrationHistory,
        registered: &PluginMigrationSequence,
    ) {
        assert_eq!(history.len(), registered.migrations.len());
        for (i, entry) in history.entries.iter().enumerate() {
            assert_eq!(entry.ordinal, i64::try_from(i).unwrap());
            assert_eq!(entry.migration_id, registered.migrations[i].id);
            assert_eq!(entry.checksum, registered.migrations[i].checksum);
        }
    }

    #[tokio::test]
    async fn rpc_host_applies_more_than_eight_migrations_from_empty() {
        let db = binding_db().await;
        let n = 16;
        assert!(n > MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS);
        let registered = seq(n_table_migs(n));
        let host = ScriptedApplyHost {
            db: db.clone(),
            registered: registered.clone(),
            script: Mutex::new(VecDeque::new()),
            executed_ids: Mutex::new(Vec::new()),
        };
        apply_registered_plugin_migrations(&host, "echo_sql", "DB", &registered)
            .await
            .expect("long sequence");
        let history = load_plugin_migration_history(&db).await.unwrap();
        assert_history_matches(&history, &registered);
        let executed = host.executed_ids.lock().unwrap();
        assert_eq!(executed.len(), n);
        assert!(
            executed.iter().all(|id| !id.contains("exhausted")),
            "{executed:?}"
        );
        for (i, id) in executed.iter().enumerate() {
            assert!(id.contains(&format!("ord{i}-")), "{id}");
            assert!(id.contains("-try0"), "{id}");
        }
    }

    #[tokio::test]
    async fn rpc_host_retries_do_not_consume_later_migration_capacity() {
        let db = binding_db().await;
        let n = 10;
        assert!(n > MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS);
        let registered = seq(n_table_migs(n));
        let mut script = VecDeque::new();
        script.push_back(ApplyScript::Commit); // m000
        script.push_back(ApplyScript::FailRetryable);
        script.push_back(ApplyScript::FailRetryable);
        script.push_back(ApplyScript::Commit); // m001 after two retries
        let host = ScriptedApplyHost {
            db: db.clone(),
            registered: registered.clone(),
            script: Mutex::new(script),
            executed_ids: Mutex::new(Vec::new()),
        };
        apply_registered_plugin_migrations(&host, "echo_sql", "DB", &registered)
            .await
            .expect("retries must not cap later migrations");
        let history = load_plugin_migration_history(&db).await.unwrap();
        assert_history_matches(&history, &registered);
        let executed = host.executed_ids.lock().unwrap();
        let m001: Vec<_> = executed
            .iter()
            .filter(|id| id.contains("ord1-m001"))
            .cloned()
            .collect();
        assert_eq!(m001.len(), 3, "{executed:?}");
        assert!(m001[0].contains("-try0"), "{m001:?}");
        assert!(m001[1].contains("-try1"), "{m001:?}");
        assert!(m001[2].contains("-try2"), "{m001:?}");
        assert_eq!(executed.len(), n + 2);
    }

    #[tokio::test]
    async fn rpc_host_retry_exhaustion_is_per_migration() {
        let db = binding_db().await;
        let registered = seq(n_table_migs(10));
        let mut script = VecDeque::new();
        script.push_back(ApplyScript::Commit); // m000
        for _ in 0..MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS {
            script.push_back(ApplyScript::FailRetryable);
        }
        let host = ScriptedApplyHost {
            db: db.clone(),
            registered: registered.clone(),
            script: Mutex::new(script),
            executed_ids: Mutex::new(Vec::new()),
        };
        let err = apply_registered_plugin_migrations(&host, "echo_sql", "DB", &registered)
            .await
            .expect_err("bounded retries");
        let msg = err.to_string();
        assert!(msg.contains("m001"), "{msg}");
        assert!(msg.contains("exhausted retries"), "{msg}");
        assert!(msg.contains("ordinal 1"), "{msg}");
        assert!(!msg.contains("remaining suffix"), "{msg}");
        let history = load_plugin_migration_history(&db).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history.entries[0].migration_id, "m000");
        let executed = host.executed_ids.lock().unwrap();
        assert_eq!(executed.len(), 1 + MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS);
        assert!(executed.iter().any(|id| id.contains("ord0-m000")));
        assert!(
            executed.iter().all(|id| !id.contains("ord2-")),
            "{executed:?}"
        );
        assert!(next_pending_plugin_migration(&history, &registered)
            .unwrap()
            .is_some_and(|(ord, m)| ord == 1 && m.id == "m001"));
    }

    #[tokio::test]
    async fn rpc_host_ambiguous_completion_advances_without_replay() {
        let db = binding_db().await;
        let registered = seq(n_table_migs(3));
        let mut script = VecDeque::new();
        script.push_back(ApplyScript::CommitThenAmbiguous);
        let host = ScriptedApplyHost {
            db: db.clone(),
            registered: registered.clone(),
            script: Mutex::new(script),
            executed_ids: Mutex::new(Vec::new()),
        };
        apply_registered_plugin_migrations(&host, "echo_sql", "DB", &registered)
            .await
            .expect("ambiguous completion");
        let history = load_plugin_migration_history(&db).await.unwrap();
        assert_history_matches(&history, &registered);
        let executed = host.executed_ids.lock().unwrap();
        let m000: Vec<_> = executed
            .iter()
            .filter(|id| id.contains("ord0-m000"))
            .cloned()
            .collect();
        assert_eq!(
            m000.len(),
            1,
            "must not replay committed migration: {executed:?}"
        );
        assert_eq!(executed.len(), 3);
    }

    #[test]
    fn operation_id_includes_ordinal_id_and_attempt() {
        let id = plugin_migrate_operation_id("echo", "DB", 4, "create-notes", 2);
        assert_eq!(id, "plugin-migrate-echo-DB-ord4-create-notes-try2");
    }
}
