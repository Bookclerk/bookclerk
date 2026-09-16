//! Plugin-owned binding migrations: opaque IDs, exact-prefix history, suffix apply.
//!
//! The plugin registers a complete ordered sequence at startup. Bookclerk
//! assigns no semantic meaning to plugin-chosen IDs. Durable
//! [`PLUGIN_MIGRATIONS_TABLE`] rows are a host-private journal; `ordinal` is
//! storage order only.

use std::collections::HashSet;
use std::time::Duration;

use bookclerk_plugin_abi::{
    apply_schema_sql_to_env, authorize_guest_sql_policy, canonical_statements_checksum,
    require_plugin_migration_registration, validate_guest_execute_request_for_policy,
    DbPlanStatementKind, DbResultSelection, DbValue, ExecuteReply, ExecuteRequest, GuestSqlPolicy,
    PluginMigration, PluginMigrationOp, SqlTypeEnv, TypedDbStatement, MAX_LIST_PAGE,
    MAX_PLUGIN_MIGRATION_ID_BYTES, PLUGIN_MIGRATIONS_TABLE,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, StreamTrait};

use super::plan::sql_string_literal;
use super::prove_plan_sql_op;
use crate::error::{LibraryError, Result};
use crate::sql_plan::{execute_typed_on_binding, execute_typed_on_open};

/// Timing label for plugin journal apply (not an adapter identity).
const PLUGIN_TXN_TIMING: &str = "plugin_migrate_txn";

/// Bounded retries for **one** expected plugin migration apply unit.
///
/// Independent of how many migrations are registered. A fresh binding may
/// apply the complete registration; this only caps retries of a single
/// expected `(ordinal, id, checksum)` while durable history is unchanged.
pub const MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS: usize = 8;

/// Per-binding serialization slot for plugin migration apply.
pub const PLUGIN_MIGRATION_SLOT_KEY: &str = "bookclerk_plugin_migrations";

/// Proven, ordered plugin migration sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMigrationSequence {
    /// Registration-order migrations with checksums.
    pub migrations: Vec<ProvenPluginMigration>,
}

/// One proven registered migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenPluginMigration {
    /// Opaque plugin-chosen identity.
    pub id: String,
    /// Ordered operations.
    pub operations: Vec<PluginMigrationOp>,
    /// Deterministic checksum of `(id, classified ops)`.
    pub checksum: String,
}

/// One durable host-private journal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginJournalEntry {
    /// Host-private storage position (`0..n-1`). Not a plugin version.
    pub ordinal: i64,
    /// Opaque plugin-chosen identity.
    pub migration_id: String,
    /// Checksum recorded when this row was applied.
    pub checksum: String,
}

/// Ordered durable plugin migration history.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginMigrationHistory {
    /// Journal rows in ordinal order.
    pub entries: Vec<PluginJournalEntry>,
}

impl PluginMigrationHistory {
    /// SHA-256 digest of the complete ordered `(migration_id, checksum)` list.
    #[must_use]
    pub fn digest(&self) -> String {
        plugin_history_digest(&self.entries)
    }

    /// Number of applied journal rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no plugin migrations have been applied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Operator-facing history label (`history@{n}+{digest}`).
    #[must_use]
    pub fn display(&self) -> String {
        format!("history@{}+{}", self.len(), self.digest())
    }
}

/// SHA-256 of ordered `(migration_id, checksum)` pairs.
#[must_use]
pub fn plugin_history_digest(entries: &[PluginJournalEntry]) -> String {
    let parts: Vec<String> = entries
        .iter()
        .map(|e| format!("{}\n{}", e.migration_id, e.checksum))
        .collect();
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    canonical_statements_checksum(&refs)
}

/// Checksum of one registered migration (id + classified SQL).
#[must_use]
pub fn plugin_migration_checksum(id: &str, operations: &[PluginMigrationOp]) -> String {
    let mut parts = vec![format!("id:{id}")];
    for op in operations {
        let tag = if op.is_schema() { "schema" } else { "data" };
        parts.push(format!("{tag}:{}", op.sql()));
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    canonical_statements_checksum(&refs)
}

/// Proves a complete registration with one evolving [`SqlTypeEnv`].
///
/// Schema operations update the environment. Data operations typecheck against
/// the current environment. The environment is not reset between migrations.
///
/// # Errors
///
/// Returns when resource limits are exceeded, an id is invalid, ids repeat,
/// an operation is empty/malformed, or BookclerkSQL proof fails.
/// Transport/resource validation is
/// [`bookclerk_plugin_abi::require_plugin_migration_registration`];
/// this function is the authoritative semantic proof afterward.
pub fn prove_plugin_migration_sequence(
    migrations: Vec<PluginMigration>,
) -> Result<PluginMigrationSequence> {
    require_plugin_migration_registration(&migrations)
        .map_err(|err| LibraryError::Schema(err.to_string()))?;
    let mut seen = HashSet::new();
    let mut env = SqlTypeEnv::new();
    let mut proven = Vec::with_capacity(migrations.len());
    let mut op_index = 0usize;
    for migration in migrations {
        let id = validate_plugin_migration_id(&migration.id)?;
        if !seen.insert(id.clone()) {
            return Err(LibraryError::Schema(format!(
                "plugin migration id `{id}` is registered more than once"
            )));
        }
        if migration.operations.is_empty() {
            return Err(LibraryError::Schema(format!(
                "plugin migration `{id}` has no operations"
            )));
        }
        for op in &migration.operations {
            prove_plugin_migration_op(op_index, op, &mut env)?;
            op_index += 1;
        }
        let checksum = plugin_migration_checksum(&id, &migration.operations);
        proven.push(ProvenPluginMigration {
            id,
            operations: migration.operations,
            checksum,
        });
    }
    Ok(PluginMigrationSequence { migrations: proven })
}

/// Binding catalog after host bootstrap plus schema ops from the first `applied`
/// registered migrations.
///
/// Plugin apply stamps journal DML and suffix data ops against this evolving
/// environment. `applied` is the durable prefix length (host-private ordinal
/// count), not a plugin version.
#[must_use]
pub fn plugin_binding_type_env(registered: &PluginMigrationSequence, applied: usize) -> SqlTypeEnv {
    let mut env = super::binding_bootstrap_type_env();
    for migration in registered.migrations.iter().take(applied) {
        for op in &migration.operations {
            if op.is_schema() {
                apply_schema_sql_to_env(&mut env, op.sql());
            }
        }
    }
    env
}

/// Proves one registered op: packing, kind, evolving type env, guest grammar,
/// and binding-migration authorization (reserved journal/bookkeeping denied).
fn prove_plugin_migration_op(
    index: usize,
    op: &PluginMigrationOp,
    env: &mut SqlTypeEnv,
) -> Result<()> {
    prove_plan_sql_op(index, op.sql(), op.is_schema(), env)?;
    let req = ExecuteRequest {
        operation_id: format!("prove-plugin-migration-op-{index}"),
        request_hash: String::new(),
        deadline_unix_ms: 0,
        statements: vec![TypedDbStatement {
            sql: op.sql().to_string(),
            parameters: Vec::new(),
            kind: DbPlanStatementKind::Execute,
            max_rows: 0,
            result_selection: DbResultSelection::Discard,
        }],
    };
    let policy = GuestSqlPolicy::binding_migration().with_sql_types(env.clone());
    validate_guest_execute_request_for_policy(&req, &policy)
        .map_err(|err| LibraryError::Schema(format!("plugin migration op {index}: {err}")))?;
    authorize_guest_sql_policy(&req, &policy)
        .map_err(|err| LibraryError::Schema(format!("plugin migration op {index}: {err}")))?;
    Ok(())
}

/// Validates an opaque plugin-chosen migration id.
///
/// # Errors
///
/// Returns when the id is empty, padded, too long, or contains U+0000.
pub fn validate_plugin_migration_id(id: &str) -> Result<String> {
    if id != id.trim() {
        return Err(LibraryError::Schema(
            "plugin migration id must not have leading or trailing whitespace".into(),
        ));
    }
    if id.is_empty() {
        return Err(LibraryError::Schema(
            "plugin migration id must not be empty".into(),
        ));
    }
    if id.len() > MAX_PLUGIN_MIGRATION_ID_BYTES {
        return Err(LibraryError::Schema(format!(
            "plugin migration id is {} bytes; maximum is {MAX_PLUGIN_MIGRATION_ID_BYTES}",
            id.len()
        )));
    }
    if id.contains('\0') {
        return Err(LibraryError::Schema(
            "plugin migration id must not contain U+0000".into(),
        ));
    }
    Ok(id.to_string())
}

/// Fail closed unless durable history is an exact prefix of `registered`.
///
/// # Errors
///
/// Returns when an applied id/checksum/order does not match, or durable history
/// is longer than the current registration (older/incompatible plugin).
pub fn require_history_prefix(
    history: &PluginMigrationHistory,
    registered: &PluginMigrationSequence,
) -> Result<()> {
    if history.len() > registered.migrations.len() {
        return Err(LibraryError::Schema(format!(
            "plugin migration journal has {} applied migrations, longer than the \
             current registration ({}); the installed plugin is older or incompatible",
            history.len(),
            registered.migrations.len()
        )));
    }
    for (i, entry) in history.entries.iter().enumerate() {
        if entry.ordinal != i64::try_from(i).unwrap_or(i64::MAX) {
            return Err(LibraryError::Schema(format!(
                "plugin migration journal ordinal {} is not contiguous at position {i}",
                entry.ordinal
            )));
        }
        let expected = &registered.migrations[i];
        if entry.migration_id != expected.id {
            return Err(LibraryError::Schema(format!(
                "plugin migration journal position {i} id `{}` does not match \
                 registration `{}` (edited, renamed, or reordered)",
                entry.migration_id, expected.id
            )));
        }
        if entry.checksum != expected.checksum {
            return Err(LibraryError::Schema(format!(
                "plugin migration journal position {i} id `{}` checksum {} does not \
                 match registration {} (applied migration was edited)",
                entry.migration_id, entry.checksum, expected.checksum
            )));
        }
    }
    Ok(())
}

/// Pending registered migrations after the durable prefix.
///
/// # Errors
///
/// Returns [`require_history_prefix`] errors.
pub fn pending_plugin_suffix<'a>(
    history: &PluginMigrationHistory,
    registered: &'a PluginMigrationSequence,
) -> Result<&'a [ProvenPluginMigration]> {
    require_history_prefix(history, registered)?;
    Ok(&registered.migrations[history.len()..])
}

/// Next registered migration after a verified durable prefix, if any.
///
/// Uses [`require_history_prefix`] then indexes `registered[history.len()]`
/// rather than rebuilding the pending suffix vector.
///
/// # Errors
///
/// Returns [`require_history_prefix`] errors.
pub fn next_pending_plugin_migration<'a>(
    history: &PluginMigrationHistory,
    registered: &'a PluginMigrationSequence,
) -> Result<Option<(usize, &'a ProvenPluginMigration)>> {
    require_history_prefix(history, registered)?;
    let ordinal = history.len();
    Ok(registered
        .migrations
        .get(ordinal)
        .map(|migration| (ordinal, migration)))
}

/// True when a binding session's captured history digest still matches durable history.
///
/// # Errors
///
/// Returns when the digest differs (peer advanced or rewritten history).
pub fn plugin_history_session_matches(
    expected_digest: &str,
    observed: &PluginMigrationHistory,
) -> Result<()> {
    let found = observed.digest();
    if expected_digest == found {
        return Ok(());
    }
    Err(LibraryError::Schema(format!(
        "plugin migration history advanced under this session (expected {expected_digest}, \
         found {found}); reopen the binding"
    )))
}

/// Typed SELECT used by RPC hosts to read the plugin migration journal.
#[must_use]
pub fn plugin_journal_select_request(
    operation_id: impl Into<String>,
    deadline_unix_ms: u64,
) -> ExecuteRequest {
    ExecuteRequest {
        operation_id: operation_id.into(),
        request_hash: String::new(),
        deadline_unix_ms,
        statements: vec![TypedDbStatement {
            sql: format!(
                "SELECT ordinal, migration_id, checksum FROM {PLUGIN_MIGRATIONS_TABLE} ORDER BY ordinal"
            ),
            parameters: Vec::new(),
            kind: DbPlanStatementKind::Select,
            max_rows: MAX_LIST_PAGE,
            result_selection: DbResultSelection::Rows,
        }],
    }
}

/// Reads the host-private plugin migration journal.
///
/// # Errors
///
/// Returns when the journal cannot be read or ordinals are not contiguous.
pub async fn load_plugin_migration_history(
    db: &DatabaseConnection,
) -> Result<PluginMigrationHistory> {
    load_plugin_migration_history_on(db).await
}

/// Reads the journal on an already-open connection (including a backup capture
/// transaction).
///
/// # Errors
///
/// Returns when the journal cannot be read or ordinals are not contiguous.
pub async fn load_plugin_migration_history_on<C>(conn: &C) -> Result<PluginMigrationHistory>
where
    C: ConnectionTrait + StreamTrait,
{
    let req = plugin_journal_select_request("plugin-journal-select", 0);
    let reply = execute_typed_on_open(
        conn,
        &req,
        super::binding_bootstrap_type_env(),
        MAX_LIST_PAGE,
    )
    .await
    .map_err(|err| {
        LibraryError::Schema(format!("cannot read bookclerk_plugin_migrations: {err}"))
    })?;
    history_from_execute_reply(&reply)
}

/// Parses journal rows from a typed select reply.
///
/// # Errors
///
/// Returns when row shape or ordinals are invalid.
pub fn history_from_execute_reply(reply: &ExecuteReply) -> Result<PluginMigrationHistory> {
    let Some(stmt) = reply.statements.first() else {
        return Ok(PluginMigrationHistory::default());
    };
    let mut entries = Vec::with_capacity(stmt.rows.len());
    for (i, row) in stmt.rows.iter().enumerate() {
        if row.values.len() < 3 {
            return Err(LibraryError::Schema(
                "bookclerk_plugin_migrations row is missing ordinal, migration_id, or checksum"
                    .into(),
            ));
        }
        let ordinal = match &row.values[0] {
            DbValue::Int64(n) => *n,
            other => {
                return Err(LibraryError::Schema(format!(
                    "bookclerk_plugin_migrations.ordinal must be INTEGER, got {other:?}"
                )))
            }
        };
        let migration_id = match &row.values[1] {
            DbValue::Text(s) => s.clone(),
            other => {
                return Err(LibraryError::Schema(format!(
                    "bookclerk_plugin_migrations.migration_id must be TEXT, got {other:?}"
                )))
            }
        };
        let checksum = match &row.values[2] {
            DbValue::Text(s) => s.clone(),
            other => {
                return Err(LibraryError::Schema(format!(
                    "bookclerk_plugin_migrations.checksum must be TEXT, got {other:?}"
                )))
            }
        };
        let expect = i64::try_from(i).unwrap_or(i64::MAX);
        if ordinal != expect {
            return Err(LibraryError::Schema(format!(
                "bookclerk_plugin_migrations ordinal {ordinal} is not contiguous (expected {expect})"
            )));
        }
        entries.push(PluginJournalEntry {
            ordinal,
            migration_id,
            checksum,
        });
    }
    Ok(PluginMigrationHistory { entries })
}

/// Host-authored journal insert for one applied migration.
#[must_use]
pub fn plugin_journal_insert_sql(ordinal: i64, id: &str, checksum: &str) -> String {
    let at = chrono::Utc::now().to_rfc3339().replace('\'', "''");
    format!(
        "INSERT INTO {PLUGIN_MIGRATIONS_TABLE} (ordinal, migration_id, checksum, applied_at) \
         VALUES ({ordinal}, {}, {}, '{at}')",
        sql_string_literal(id),
        sql_string_literal(checksum)
    )
}

/// Portable insert-or-ignore + bump for [`PLUGIN_MIGRATION_SLOT_KEY`].
///
/// These statements run inside the same atomic apply unit as the migration
/// operations and journal append. They do not hold a lock across later
/// independent transactions.
#[must_use]
pub fn plugin_slot_lock_sql() -> Vec<String> {
    let key = sql_string_literal(PLUGIN_MIGRATION_SLOT_KEY);
    vec![
        format!("INSERT OR IGNORE INTO bookclerk_slots (slot_key, bump) VALUES ({key}, 0)"),
        format!("UPDATE bookclerk_slots SET bump = bump + 1 WHERE slot_key = {key}"),
    ]
}

/// One atomic apply unit: serialization slot, registered ops, host journal append.
#[must_use]
pub fn plugin_apply_statements(ordinal: i64, migration: &ProvenPluginMigration) -> Vec<String> {
    let mut stmts = plugin_slot_lock_sql();
    stmts.extend(migration.operations.iter().map(|op| op.sql().to_string()));
    stmts.push(plugin_journal_insert_sql(
        ordinal,
        &migration.id,
        &migration.checksum,
    ));
    stmts
}

/// Remaining atomic apply batches for RPC hosts that cannot run SeaORM apply.
///
/// # Errors
///
/// Returns [`require_history_prefix`] errors.
pub fn remaining_plugin_suffix_batches(
    history: &PluginMigrationHistory,
    registered: &PluginMigrationSequence,
) -> Result<Vec<Vec<String>>> {
    let suffix = pending_plugin_suffix(history, registered)?;
    let start = history.len();
    Ok(suffix
        .iter()
        .enumerate()
        .map(|(i, migration)| {
            let ordinal = i64::try_from(start + i).unwrap_or(i64::MAX);
            plugin_apply_statements(ordinal, migration)
        })
        .collect())
}

/// Applies the pending registered suffix. Idempotent when history already matches.
///
/// Outer loop walks durable history (exact-prefix). Each pending migration is
/// one short atomic unit: serialization-slot mutation + plugin operations +
/// journal append, with a bounded inner retry loop that does **not** consume
/// later migration capacity. There is no lock held across those independent
/// transactions. Concurrent hosts re-read durable history before applying;
/// uniqueness / conflict / an ambiguous reply causes another journal read.
/// Matching `(id, checksum)` at the expected ordinal is success.
/// Contradictory history fails closed.
///
/// # Errors
///
/// Returns when prefix verification fails, apply exhausts retries, or the
/// journal is contradictory.
pub async fn apply_plugin_migrations(
    db: &DatabaseConnection,
    registered: &PluginMigrationSequence,
) -> Result<PluginMigrationHistory> {
    loop {
        let history = load_plugin_migration_history(db).await?;
        let Some((ordinal, migration)) = next_pending_plugin_migration(&history, registered)?
        else {
            return Ok(history);
        };
        let catalog = plugin_binding_type_env(registered, ordinal);
        let ordinal_i64 = i64::try_from(ordinal).unwrap_or(i64::MAX);
        apply_one_plugin_migration(db, ordinal_i64, migration, registered, &catalog).await?;
    }
}

/// Applies one pending journal row (ops + marker) with lost-completion retry.
///
/// # Errors
///
/// Returns when the journal is contradictory, apply is not retryable, or
/// retries are exhausted.
async fn apply_one_plugin_migration(
    db: &DatabaseConnection,
    ordinal: i64,
    migration: &ProvenPluginMigration,
    registered: &PluginMigrationSequence,
    catalog: &SqlTypeEnv,
) -> Result<()> {
    let mut delay_ms = 20u64;
    for attempt in 0..MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS {
        let history = load_plugin_migration_history(db).await?;
        if plugin_journal_has_entry(&history, ordinal, &migration.id, &migration.checksum) {
            return Ok(());
        }
        require_history_prefix(&history, registered)?;
        let expected_len = usize::try_from(ordinal).unwrap_or(usize::MAX);
        if history.len() != expected_len {
            return Ok(());
        }
        let stmts = plugin_apply_statements(ordinal, migration);
        let operation_id = format!("plugin-migrate-ord{ordinal}-try{attempt}");
        match run_plugin_atomic(db, &operation_id, stmts, catalog).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                let history = load_plugin_migration_history(db).await;
                match history {
                    Ok(observed)
                        if plugin_journal_has_entry(
                            &observed,
                            ordinal,
                            &migration.id,
                            &migration.checksum,
                        ) =>
                    {
                        return Ok(());
                    }
                    Ok(observed) => {
                        require_history_prefix(&observed, registered)?;
                        if observed.len() != expected_len {
                            return Ok(());
                        }
                        if err.is_schema_apply_retryable()
                            && attempt + 1 < MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS
                        {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            delay_ms = delay_ms.saturating_mul(2).min(250);
                            continue;
                        }
                        if err.is_schema_apply_retryable() {
                            return Err(LibraryError::Schema(format!(
                                "plugin migration `{}` exhausted retries at ordinal {ordinal}",
                                migration.id
                            )));
                        }
                        return Err(err);
                    }
                    Err(_)
                        if err.is_schema_apply_retryable()
                            && attempt + 1 < MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS =>
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(2).min(250);
                        continue;
                    }
                    Err(_) => return Err(err),
                }
            }
        }
    }
    Err(LibraryError::Schema(format!(
        "plugin migration `{}` exhausted retries at ordinal {ordinal}",
        migration.id
    )))
}

/// True when `history` already contains this `(ordinal, id, checksum)` row.
#[must_use]
pub fn plugin_journal_has_entry(
    history: &PluginMigrationHistory,
    ordinal: i64,
    id: &str,
    checksum: &str,
) -> bool {
    history
        .entries
        .iter()
        .any(|e| e.ordinal == ordinal && e.migration_id == id && e.checksum == checksum)
}

/// Runs `stmts` as one typed binding execute (ops + journal row).
///
/// # Errors
///
/// Returns when typed apply fails (uniqueness/unavailable are retryable at
/// the caller).
async fn run_plugin_atomic(
    db: &DatabaseConnection,
    operation_id: &str,
    stmts: Vec<String>,
    catalog: &SqlTypeEnv,
) -> Result<()> {
    let req = ExecuteRequest {
        operation_id: operation_id.into(),
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
    execute_typed_on_binding(db, &req, PLUGIN_TXN_TIMING, 0, catalog).await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
mod tests {
    use super::*;
    use crate::apply_binding_bootstrap;
    use bookclerk_plugin_abi::{GuestSqlPolicy, MAX_PLUGIN_MIGRATION_OPS};

    fn schema(sql: &str) -> PluginMigrationOp {
        PluginMigrationOp::Schema(sql.to_string())
    }

    fn data(sql: &str) -> PluginMigrationOp {
        PluginMigrationOp::Data(sql.to_string())
    }

    fn mig(id: &str, operations: Vec<PluginMigrationOp>) -> PluginMigration {
        PluginMigration {
            id: id.into(),
            operations,
        }
    }

    fn notes_create() -> PluginMigrationOp {
        schema(
            "CREATE TABLE IF NOT EXISTS notes (\
                id INTEGER PRIMARY KEY, \
                body TEXT NOT NULL\
            )",
        )
    }

    fn seq(migrations: Vec<PluginMigration>) -> PluginMigrationSequence {
        prove_plugin_migration_sequence(migrations).expect("prove")
    }

    #[test]
    fn prove_rejects_count_plus_one_before_semantic_sql() {
        let migrations: Vec<_> = (0..MAX_LIST_PAGE as usize + 1)
            .map(|i| mig(&format!("m{i:03}"), vec![schema("x")]))
            .collect();
        let err = prove_plugin_migration_sequence(migrations).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("payload_too_large") || msg.contains("maxListPage"),
            "{err}"
        );
    }

    #[test]
    fn prove_runs_at_ops_limit() {
        let extra = MAX_PLUGIN_MIGRATION_OPS as usize - 1;
        let mut ops = vec![notes_create()];
        for i in 0..extra {
            ops.push(data(&format!(
                "INSERT INTO notes (id, body) VALUES ({i}, 'x')"
            )));
        }
        seq(vec![mig("bulk", ops)]);
    }

    #[test]
    fn each_apply_batch_is_slot_ops_journal() {
        let registered = seq(vec![mig("create-notes", vec![notes_create()])]);
        let batches =
            remaining_plugin_suffix_batches(&PluginMigrationHistory::default(), &registered)
                .unwrap();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert!(batch.len() >= 4, "{batch:?}");
        assert!(
            batch[0].contains("bookclerk_slots") && batch[0].contains("INSERT OR IGNORE"),
            "{}",
            batch[0]
        );
        assert!(
            batch[1].contains("bookclerk_slots") && batch[1].contains("UPDATE"),
            "{}",
            batch[1]
        );
        assert!(batch[2].to_ascii_uppercase().contains("CREATE TABLE"));
        assert!(
            batch.last().unwrap().contains(PLUGIN_MIGRATIONS_TABLE),
            "{}",
            batch.last().unwrap()
        );
    }

    async fn binding_db() -> DatabaseConnection {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .unwrap();
        apply_binding_bootstrap(&db).await.unwrap();
        db
    }

    #[test]
    fn opaque_ids_need_no_numeric_meaning() {
        let registered = seq(vec![
            mig("create-notes", vec![notes_create()]),
            mig(
                "20240101T000000Z",
                vec![schema(
                    "CREATE INDEX IF NOT EXISTS notes_body ON notes(body)",
                )],
            ),
            mig(
                "550e8400-e29b-41d4-a716-446655440000",
                vec![data("INSERT INTO notes (id, body) VALUES (1, 'seed')")],
            ),
        ]);
        assert_eq!(registered.migrations.len(), 3);
        assert_eq!(registered.migrations[0].id, "create-notes");
    }

    #[test]
    fn evolving_env_second_migration_indexes_first_table() {
        seq(vec![
            mig("a", vec![notes_create()]),
            mig(
                "b",
                vec![schema(
                    "CREATE INDEX IF NOT EXISTS notes_body ON notes(body)",
                )],
            ),
        ]);
    }

    #[test]
    fn plugin_binding_type_env_keeps_bootstrap_and_prefix_schema() {
        let registered = seq(vec![
            mig("a", vec![notes_create()]),
            mig(
                "b",
                vec![data("INSERT INTO notes (id, body) VALUES (1, 'seed')")],
            ),
        ]);
        let before = plugin_binding_type_env(&registered, 0);
        assert!(before.has_table(PLUGIN_MIGRATIONS_TABLE));
        assert!(!before.has_table("notes"));
        let after_create = plugin_binding_type_env(&registered, 1);
        assert!(after_create.has_table("notes"));
        assert_eq!(
            after_create.column_type("notes", "body"),
            Some(bookclerk_plugin_abi::SqlType::Text)
        );
    }

    #[test]
    fn three_dependent_migrations_prove() {
        seq(vec![
            mig("a", vec![notes_create()]),
            mig(
                "b",
                vec![schema(
                    "CREATE TABLE IF NOT EXISTS tags (\
                    id INTEGER PRIMARY KEY, \
                    note_id INTEGER NOT NULL\
                )",
                )],
            ),
            mig(
                "c",
                vec![data("INSERT INTO tags (id, note_id) VALUES (1, 1)")],
            ),
        ]);
    }

    #[test]
    fn invalid_sql_fails_before_use() {
        let err = prove_plugin_migration_sequence(vec![mig(
            "bad",
            vec![schema(
                "CREATE TABLE IF NOT EXISTS nope (id INTEGER PRIMARY KEY, x DATETIME)",
            )],
        )])
        .unwrap_err();
        assert!(
            err.to_string().contains("typecheck")
                || err.to_string().contains("DATETIME")
                || err.to_string().to_lowercase().contains("not allowed")
                || err.to_string().to_lowercase().contains("type"),
            "{err}"
        );
    }

    #[test]
    fn registration_cannot_name_journal() {
        let err = prove_plugin_migration_sequence(vec![mig(
            "evil",
            vec![schema(
                "CREATE TABLE IF NOT EXISTS bookclerk_plugin_migrations (\
                    ordinal INTEGER PRIMARY KEY NOT NULL, \
                    migration_id TEXT NOT NULL, \
                    checksum TEXT NOT NULL, \
                    applied_at TEXT NOT NULL\
                )",
            )],
        )])
        .unwrap_err();
        assert!(
            err.to_string().contains("bookclerk_plugin_migrations")
                || err.to_string().to_lowercase().contains("reserved"),
            "{err}"
        );
    }

    #[test]
    fn noncontiguous_numeric_looking_ids_are_opaque() {
        seq(vec![
            mig("10", vec![notes_create()]),
            mig(
                "3",
                vec![schema(
                    "CREATE INDEX IF NOT EXISTS notes_body ON notes(body)",
                )],
            ),
        ]);
    }

    fn n_table_migs(n: usize) -> Vec<PluginMigration> {
        (0..n)
            .map(|i| {
                mig(
                    &format!("m{i:03}"),
                    vec![schema(&format!(
                        "CREATE TABLE IF NOT EXISTS t{i} (id INTEGER PRIMARY KEY NOT NULL)"
                    ))],
                )
            })
            .collect()
    }

    #[test]
    fn next_pending_indexes_registration_after_prefix() {
        let registered = seq(n_table_migs(3));
        let empty = PluginMigrationHistory::default();
        let (ordinal, migration) = next_pending_plugin_migration(&empty, &registered)
            .unwrap()
            .expect("first");
        assert_eq!(ordinal, 0);
        assert_eq!(migration.id, "m000");
        let history = PluginMigrationHistory {
            entries: vec![PluginJournalEntry {
                ordinal: 0,
                migration_id: registered.migrations[0].id.clone(),
                checksum: registered.migrations[0].checksum.clone(),
            }],
        };
        let (ordinal, migration) = next_pending_plugin_migration(&history, &registered)
            .unwrap()
            .expect("second");
        assert_eq!(ordinal, 1);
        assert_eq!(migration.id, "m001");
        let done = PluginMigrationHistory {
            entries: registered
                .migrations
                .iter()
                .enumerate()
                .map(|(i, m)| PluginJournalEntry {
                    ordinal: i64::try_from(i).unwrap(),
                    migration_id: m.id.clone(),
                    checksum: m.checksum.clone(),
                })
                .collect(),
        };
        assert!(next_pending_plugin_migration(&done, &registered)
            .unwrap()
            .is_none());
    }

    #[test]
    fn second_migration_without_prior_table_fails() {
        let err = prove_plugin_migration_sequence(vec![mig(
            "idx-only",
            vec![schema(
                "CREATE INDEX IF NOT EXISTS notes_body ON notes(body)",
            )],
        )])
        .unwrap_err();
        assert!(
            err.to_string().contains("typecheck") || err.to_string().contains("notes"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn reregister_identical_history_is_noop() {
        let db = binding_db().await;
        let registered = seq(vec![mig("create-notes", vec![notes_create()])]);
        let first = apply_plugin_migrations(&db, &registered).await.unwrap();
        let second = apply_plugin_migrations(&db, &registered).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert_eq!(first.entries[0].migration_id, "create-notes");
        assert_eq!(first.digest(), second.digest());
    }

    #[tokio::test]
    async fn sixteen_migrations_apply_from_empty_beyond_retry_cap() {
        let db = binding_db().await;
        let n = MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS * 2;
        assert!(n > MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS);
        let registered = seq(n_table_migs(n));
        let history = apply_plugin_migrations(&db, &registered).await.unwrap();
        assert_eq!(history.len(), n);
        for (i, entry) in history.entries.iter().enumerate() {
            assert_eq!(entry.ordinal, i64::try_from(i).unwrap());
            assert_eq!(entry.migration_id, registered.migrations[i].id);
            assert_eq!(entry.checksum, registered.migrations[i].checksum);
        }
    }

    #[tokio::test]
    async fn append_runs_only_suffix() {
        let db = binding_db().await;
        let v1 = seq(vec![mig("create-notes", vec![notes_create()])]);
        apply_plugin_migrations(&db, &v1).await.unwrap();
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "INSERT INTO notes (id, body) VALUES (1, 'kept')",
            ),
        )
        .await
        .unwrap();
        let v2 = seq(vec![
            mig("create-notes", vec![notes_create()]),
            mig(
                "idx",
                vec![schema(
                    "CREATE INDEX IF NOT EXISTS notes_body ON notes(body)",
                )],
            ),
        ]);
        let after = apply_plugin_migrations(&db, &v2).await.unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after.entries[1].migration_id, "idx");
        let rows = sea_orm::ConnectionTrait::query_all_raw(
            &db,
            sea_orm::Statement::from_string(sea_orm::DbBackend::Sqlite, "SELECT body FROM notes"),
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn edit_applied_sql_fails() {
        let db = binding_db().await;
        let original = seq(vec![mig("create-notes", vec![notes_create()])]);
        apply_plugin_migrations(&db, &original).await.unwrap();
        let edited = seq(vec![mig(
            "create-notes",
            vec![schema(
                "CREATE TABLE IF NOT EXISTS notes (\
                    id INTEGER PRIMARY KEY, \
                    body TEXT NOT NULL, \
                    extra INTEGER\
                )",
            )],
        )]);
        let err = apply_plugin_migrations(&db, &edited).await.unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
    }

    #[tokio::test]
    async fn rename_applied_id_fails() {
        let db = binding_db().await;
        let original = seq(vec![mig("create-notes", vec![notes_create()])]);
        apply_plugin_migrations(&db, &original).await.unwrap();
        let renamed = seq(vec![mig("notes-v1", vec![notes_create()])]);
        let err = apply_plugin_migrations(&db, &renamed).await.unwrap_err();
        assert!(
            err.to_string().contains("does not match") || err.to_string().contains("renamed"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn reorder_applied_fails() {
        let db = binding_db().await;
        let original = seq(vec![
            mig("a", vec![notes_create()]),
            mig(
                "b",
                vec![schema(
                    "CREATE TABLE IF NOT EXISTS tags (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
                )],
            ),
        ]);
        apply_plugin_migrations(&db, &original).await.unwrap();
        let reordered = seq(vec![
            mig(
                "b",
                vec![schema(
                    "CREATE TABLE IF NOT EXISTS tags (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
                )],
            ),
            mig("a", vec![notes_create()]),
        ]);
        let err = apply_plugin_migrations(&db, &reordered).await.unwrap_err();
        assert!(
            err.to_string().contains("does not match") || err.to_string().contains("reordered"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn remove_applied_fails() {
        let db = binding_db().await;
        let original = seq(vec![
            mig("a", vec![notes_create()]),
            mig(
                "b",
                vec![schema(
                    "CREATE TABLE IF NOT EXISTS tags (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
                )],
            ),
        ]);
        apply_plugin_migrations(&db, &original).await.unwrap();
        let removed = seq(vec![mig("a", vec![notes_create()])]);
        let err = apply_plugin_migrations(&db, &removed).await.unwrap_err();
        assert!(err.to_string().contains("longer than"), "{err}");
    }

    #[tokio::test]
    async fn two_nodes_racing_suffix_converge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db");
        let a = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
        apply_binding_bootstrap(&a).await.unwrap();
        let b = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
        let registered = seq(vec![mig("create-notes", vec![notes_create()])]);
        let (ra, rb) = tokio::join!(
            apply_plugin_migrations(&a, &registered),
            apply_plugin_migrations(&b, &registered)
        );
        let ha = ra.expect("node a");
        let hb = rb.expect("node b");
        assert_eq!(ha.digest(), hb.digest());
        assert_eq!(ha.len(), 1);
    }

    #[tokio::test]
    async fn lost_completion_retries_when_journal_absent() {
        let db = binding_db().await;
        let registered = seq(vec![mig("create-notes", vec![notes_create()])]);
        let batch =
            remaining_plugin_suffix_batches(&PluginMigrationHistory::default(), &registered)
                .unwrap()
                .into_iter()
                .next()
                .expect("one suffix");
        let expanded =
            bookclerk_db_exec::expand_host_schema_batch(sea_orm::DatabaseBackend::Sqlite, &batch)
                .unwrap_or(batch);
        let skip = u32::try_from(expanded.len().saturating_sub(1)).unwrap_or(0);
        crate::inject_atomic_interrupt_after(
            crate::AtomicInterruptPhase::BetweenStatements,
            crate::AtomicInterruptKind::Cancel,
            skip,
        );
        let err = apply_plugin_migrations(&db, &registered)
            .await
            .expect_err("interrupt before journal");
        assert!(err.to_string().to_lowercase().contains("cancel"), "{err}");
        let before = load_plugin_migration_history(&db).await.unwrap();
        assert!(before.is_empty());
        let after = apply_plugin_migrations(&db, &registered).await.unwrap();
        assert_eq!(after.len(), 1);
    }

    #[tokio::test]
    async fn ambiguous_completion_reread_recognizes_commit() {
        let db = binding_db().await;
        let registered = seq(vec![mig("create-notes", vec![notes_create()])]);
        let batch =
            remaining_plugin_suffix_batches(&PluginMigrationHistory::default(), &registered)
                .unwrap()
                .into_iter()
                .next()
                .expect("one suffix");
        run_plugin_atomic(
            &db,
            "lost-reply",
            batch,
            &plugin_binding_type_env(&registered, 0),
        )
        .await
        .expect("durable apply whose reply the caller never saw");
        let after = apply_plugin_migrations(&db, &registered).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after.entries[0].migration_id, "create-notes");
    }

    #[tokio::test]
    async fn stale_session_fails_after_peer_advances() {
        let db = binding_db().await;
        let v1 = seq(vec![mig("create-notes", vec![notes_create()])]);
        let expected = apply_plugin_migrations(&db, &v1).await.unwrap();
        let v2 = seq(vec![
            mig("create-notes", vec![notes_create()]),
            mig(
                "idx",
                vec![schema(
                    "CREATE INDEX IF NOT EXISTS notes_body ON notes(body)",
                )],
            ),
        ]);
        apply_plugin_migrations(&db, &v2).await.unwrap();
        let observed = load_plugin_migration_history(&db).await.unwrap();
        let err = plugin_history_session_matches(&expected.digest(), &observed).unwrap_err();
        assert!(err.to_string().contains("history advanced"), "{err}");
    }

    #[tokio::test]
    async fn restore_older_history_then_suffix_applies() {
        use crate::backup::capture::capture_plugin_unit;
        use crate::backup::repository::BackupRepository;
        use crate::backup::restore::restore_backup_unit;
        use crate::backup::{CanonicalExportOpts, CanonicalRestoreKind, CanonicalRestoreOpts};

        let src = binding_db().await;
        let v1 = seq(vec![mig("create-notes", vec![notes_create()])]);
        let src_history = apply_plugin_migrations(&src, &v1).await.unwrap();
        let files = tempfile::tempdir().unwrap();
        let repo = BackupRepository::open(files.path()).unwrap();
        let unit = capture_plugin_unit(
            &src,
            &repo,
            &CanonicalExportOpts::default(),
            "echo_sql",
            "notes",
            "sqlite",
        )
        .await
        .unwrap();
        assert_eq!(unit.plugin_schema_namespace.as_deref(), Some("echo_sql"));
        assert_eq!(
            unit.plugin_schema_checksum.as_deref(),
            Some(src_history.digest().as_str())
        );
        assert_eq!(
            unit.plugin_schema_state.as_deref(),
            Some(src_history.display().as_str())
        );
        let restored = binding_db().await;
        restore_backup_unit(
            &restored,
            &repo,
            &unit,
            CanonicalRestoreKind::PluginBinding,
            &CanonicalRestoreOpts::default(),
            false,
        )
        .await
        .unwrap();
        let history = load_plugin_migration_history(&restored).await.unwrap();
        assert_eq!(history.len(), 1);
        let v2 = seq(vec![
            mig("create-notes", vec![notes_create()]),
            mig(
                "idx",
                vec![schema(
                    "CREATE INDEX IF NOT EXISTS notes_body ON notes(body)",
                )],
            ),
        ]);
        let after = apply_plugin_migrations(&restored, &v2).await.unwrap();
        assert_eq!(after.len(), 2);
    }

    #[tokio::test]
    async fn restore_history_newer_than_plugin_fails() {
        let db = binding_db().await;
        let v2 = seq(vec![
            mig("create-notes", vec![notes_create()]),
            mig(
                "idx",
                vec![schema(
                    "CREATE INDEX IF NOT EXISTS notes_body ON notes(body)",
                )],
            ),
        ]);
        apply_plugin_migrations(&db, &v2).await.unwrap();
        let v1 = seq(vec![mig("create-notes", vec![notes_create()])]);
        let err = apply_plugin_migrations(&db, &v1).await.unwrap_err();
        assert!(err.to_string().contains("longer than"), "{err}");
    }

    #[tokio::test]
    async fn binding_owned_rejects_schema_and_journal() {
        let db = binding_db().await;
        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
        let env = bookclerk_db_exec::load_sql_type_env(&db)
            .await
            .expect("binding catalog");
        let policy = GuestSqlPolicy::binding_owned().with_sql_types(env);
        let ddl = ExecuteRequest {
            operation_id: "ddl".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![TypedDbStatement {
                sql: "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)".into(),
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
        };
        let err = crate::execute_guest_atomic_with(ddl, &caps, &policy, |_| async {
            Err(bookclerk_plugin_abi::PluginError::internal(
                "binding_owned must reject schema DDL before dispatch",
            ))
        })
        .await
        .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("schema")
                || err.to_string().to_lowercase().contains("ddl")
                || err.to_string().to_lowercase().contains("create"),
            "{err}"
        );

        let journal = ExecuteRequest {
            operation_id: "j".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![TypedDbStatement {
                sql: format!("SELECT ordinal FROM {PLUGIN_MIGRATIONS_TABLE}"),
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Select,
                max_rows: 8,
                result_selection: DbResultSelection::Rows,
            }],
        };
        let err = crate::execute_guest_atomic_with(journal, &caps, &policy, |_| async {
            Err(bookclerk_plugin_abi::PluginError::internal(
                "binding_owned must reject journal SQL before dispatch",
            ))
        })
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("bookclerk_plugin_migrations")
                || err.to_string().to_lowercase().contains("reserved")
                || err.to_string().to_lowercase().contains("unauthorized"),
            "{err}"
        );
    }

    #[test]
    fn bookclerk_namespace_unused_for_plugin_history_display() {
        let history = PluginMigrationHistory::default();
        assert!(history.display().starts_with("history@0+"));
        assert!(!history.display().contains("bookclerk"));
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
    async fn apply_is_idempotent_on_postgres() {
        if std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
        {
            return;
        }
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap();
        let db_name = format!("plreg_{}", uuid::Uuid::new_v4().as_simple());
        let admin = sea_orm::Database::connect(url.as_str())
            .await
            .expect("admin");
        let backend = sea_orm::ConnectionTrait::get_database_backend(&admin);
        sea_orm::ConnectionTrait::execute_raw(
            &admin,
            sea_orm::Statement::from_string(backend, format!("CREATE DATABASE {db_name}")),
        )
        .await
        .expect("create db");
        let (base, query) = match url.split_once('?') {
            Some((base, q)) => (base, Some(q)),
            None => (url.as_str(), None),
        };
        let trimmed = base.trim_end_matches('/');
        let slash = trimmed.rfind('/').expect("url path");
        let db_url = match query {
            Some(q) => format!("{}/{db_name}?{q}", &trimmed[..slash]),
            None => format!("{}/{db_name}", &trimmed[..slash]),
        };
        let db = sea_orm::Database::connect(&db_url).await.expect("connect");
        apply_binding_bootstrap(&db).await.unwrap();
        let registered = seq(vec![mig("create-notes", vec![notes_create()])]);
        let first = apply_plugin_migrations(&db, &registered).await.unwrap();
        let second = apply_plugin_migrations(&db, &registered).await.unwrap();
        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.len(), 1);
    }
}
