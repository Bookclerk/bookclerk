//! Host-owned schema application after a database guest connects.
//!
//! The host reads [`crate::SchemaState`] (`Uninitialized` / `Unreleased` /
//! `Frozen`) and applies remaining canonical schema as atomic units (each
//! frozen plan step, then the unreleased pack). A frozen database newer than
//! this binary fails closed. Versions live in `bookclerk_schema_migrations` rows.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, QueryResult};
#[cfg(test)]
use sea_orm::{DbBackend, Statement};

use crate::backup::{backup_library, BackupReason, BackupRequest, SchemaBackupOpts};
use crate::error::{LibraryError, Result};
#[cfg(test)]
use crate::migrations::MigrationOp;
use crate::migrations::{
    frozen_marker_delete_sql, frozen_marker_sql, host_migration_plan, sql_string_literal,
    unreleased_checksum, unreleased_ops, unreleased_state_marker_sql, HostMigrationStep,
    BOOKCLERK_SCHEMA_NAMESPACE, SCHEMA_MIGRATIONS_DDL, SCHEMA_VERSION,
};
use crate::schema_state::{SchemaState, SCHEMA_STATE_FROZEN, SCHEMA_STATE_UNRELEASED};
use crate::schema_walk::SchemaWalk;
use crate::sql_plan::execute_typed_on;

/// Canonical schema apply batch: already-separate host statements plus marker.
///
/// Adapters lower each statement at execution; they must not re-split SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaBatch {
    /// Ordered SQL strings; the last statement is the version/state marker.
    pub statements: Vec<String>,
}

impl SchemaBatch {
    /// Builds a batch from an ordered statement list plus a trailing marker.
    #[must_use]
    pub fn from_statements_and_marker(
        stmts: impl IntoIterator<Item = impl Into<String>>,
        marker: impl Into<String>,
    ) -> Self {
        let mut statements: Vec<String> = stmts.into_iter().map(Into::into).collect();
        statements.push(marker.into());
        Self { statements }
    }

    /// Unreleased development pack plus `bookclerk_schema_migrations` unreleased row.
    #[must_use]
    pub fn unreleased(stmts: impl IntoIterator<Item = impl Into<String>>, checksum: &str) -> Self {
        Self::from_statements_and_marker(
            stmts,
            unreleased_state_marker_sql(checksum, SCHEMA_VERSION),
        )
    }
}

/// Options for [`apply_host_schema_with_options`].
#[derive(Debug, Clone, Default)]
pub struct SchemaApplyOptions {
    /// When set, write a recovery point before applying DDL to a non-empty database.
    pub backup: Option<SchemaBackupOpts>,
}

/// Requires the `schemaMigrations` versioning contract from typed
/// [`bookclerk_plugin_abi::DbCapabilities`].
///
/// The host always records versions in canonical `bookclerk_schema_migrations`
/// rows; canonical Bookclerk SQL is [`crate::migrations::current_canonical_schema`]
/// and adapters lower it for the live connection backend at execution. Plugin
/// identity and diagnostic engine names are not consulted.
///
/// # Errors
///
/// Returns [`LibraryError::Other`] when `schemaMigrations` is missing.
pub fn require_schema_migrations(caps: &bookclerk_plugin_abi::DbCapabilities) -> Result<()> {
    if caps.schema_migrations {
        Ok(())
    } else {
        Err(LibraryError::Other(anyhow::anyhow!(
            "database guest schema flags are not a known versioning contract \
             (schemaMigrations={})",
            caps.schema_migrations
        )))
    }
}

/// Applies pending host-authored DDL from the canonical migration plan.
///
/// In-process adapters (sqlite tests, native postgres tests) infer the engine
/// from the opened connection. Production plugin-host uses
/// [`apply_host_schema_with_batch_opts`] so adapters lower canonical DDL after
/// RPC.
///
/// # Errors
///
/// Returns [`LibraryError`] when a version read or DDL statement fails, the
/// database is newer than this binary, or a frozen checksum does not match.
pub async fn apply_host_schema(db: &DatabaseConnection) -> Result<()> {
    apply_host_schema_with_options(db, SchemaApplyOptions::default()).await
}

/// Applies pending host schema with optional in-place backups.
///
/// # Errors
///
/// Returns [`LibraryError`] when a version read, backup, or DDL statement fails.
pub async fn apply_host_schema_with_options(
    db: &DatabaseConnection,
    opts: SchemaApplyOptions,
) -> Result<()> {
    let exec = db.clone();
    apply_host_schema_with_batch_opts(db, opts, move |stmts| {
        let exec = exec.clone();
        async move { run_atomic_ddl(&exec, "schema-apply", stmts).await }
    })
    .await
}

/// Applies schema using `run_batch` (typed `executeAtomic`) for each version.
///
/// # Errors
///
/// Returns [`LibraryError`] when a version read, DDL statement, or batch fails.
pub async fn apply_host_schema_with_batch<F, Fut>(
    db: &DatabaseConnection,
    run_batch: F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    apply_host_schema_with_batch_opts(db, SchemaApplyOptions::default(), run_batch).await
}

/// Applies schema with backups using `run_batch` for each version.
///
/// # Errors
///
/// Returns [`LibraryError`] when a version read, backup, DDL, or batch fails.
pub async fn apply_host_schema_with_batch_opts<F, Fut>(
    db: &DatabaseConnection,
    opts: SchemaApplyOptions,
    mut run_batch: F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    reconcile_schema_state(db, &opts, &mut run_batch).await
}

/// Migrates toward `target`, including reversible downs for CLI rollback.
///
/// # Errors
///
/// Returns [`LibraryError`] when the walk cannot start, a backup fails, or DDL fails.
/// A blocked downgrade still applies every reversible step and returns the walk
/// (the caller treats `blocked` / `stopped_at != target` as failure).
pub async fn migrate_host_schema_to(
    db: &DatabaseConnection,
    target: i64,
    opts: SchemaApplyOptions,
) -> Result<SchemaWalk> {
    let exec = db.clone();
    migrate_host_schema_to_with_batch(db, target, opts, move |stmts| {
        let exec = exec.clone();
        async move { run_atomic_ddl(&exec, "schema-migrate", stmts).await }
    })
    .await
}

/// Migrates toward `target` using `run_batch` (typed `executeAtomic`) per version.
///
/// # Errors
///
/// Returns [`LibraryError`] when the walk cannot start, a backup fails, or a batch fails.
pub async fn migrate_host_schema_to_with_batch<F, Fut>(
    db: &DatabaseConnection,
    target: i64,
    opts: SchemaApplyOptions,
    mut run_batch: F,
) -> Result<SchemaWalk>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let walk = prepare_schema_change(db, target, &opts).await?;
    apply_walk_batch(db, &walk, &mut run_batch).await?;
    Ok(walk)
}

/// Reconciles durable [`SchemaState`] with this binary's current canonical schema.
async fn reconcile_schema_state<F, Fut>(
    db: &DatabaseConnection,
    opts: &SchemaApplyOptions,
    run_batch: &mut F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    ensure_schema_migrations(db).await?;
    let state = current_schema_state(db).await?;
    match state {
        SchemaState::Uninitialized => apply_unreleased_pack(db, run_batch).await,
        SchemaState::Unreleased {
            base_version,
            checksum,
        } => {
            let expected = unreleased_checksum();
            if base_version != SCHEMA_VERSION {
                return Err(LibraryError::Schema(format!(
                    "unreleased@base{base_version}+{checksum} is not based on this binary's \
                     frozen line (base {SCHEMA_VERSION}); recreate the database \
                     (`cargo reset --yes`)"
                )));
            }
            if checksum == expected {
                if base_version > 0 {
                    verify_applied_checksums(db, &host_migration_plan(), base_version).await?;
                }
                Ok(())
            } else {
                Err(LibraryError::Schema(format!(
                    "unreleased schema checksum {checksum} does not match this binary ({expected}); \
                     recreate the database (`cargo reset --yes`) — Bookclerk will not reshape \
                     an unreleased development schema in place"
                )))
            }
        }
        SchemaState::Frozen { version, checksum } => {
            let plan = host_migration_plan();
            let max_plan = plan.iter().map(|s| s.version).max().unwrap_or(0);
            if version > max_plan {
                return Err(LibraryError::Schema(format!(
                    "database is frozen@{version}+{checksum}, newer than this binary \
                     (frozen plan ends at {max_plan}); run a newer Bookclerk binary, \
                     or restore a backup captured with a binary that knows that freeze"
                )));
            }
            let walk = prepare_schema_change(db, SCHEMA_VERSION, opts).await?;
            apply_walk_batch(db, &walk, run_batch).await?;
            if !unreleased_ops().is_empty() {
                apply_unreleased_bucket(db, run_batch).await?;
            }
            Ok(())
        }
    }
}

/// Reads explicit [`SchemaState`] in the Bookclerk host/bootstrap namespace.
///
/// An empty `bookclerk_schema_migrations` table is uninitialized; leftover SQLite
/// `PRAGMA user_version` is ignored.
///
/// # Errors
///
/// Returns [`LibraryError::Schema`] on malformed, partial, or contradictory markers.
pub async fn current_schema_state(db: &DatabaseConnection) -> Result<SchemaState> {
    current_schema_state_in(db, BOOKCLERK_SCHEMA_NAMESPACE).await
}

/// Reads explicit [`SchemaState`] for one ledger `namespace`.
///
/// Host library and binding bootstrap use [`BOOKCLERK_SCHEMA_NAMESPACE`].
/// Frozen checksums are verified against [`host_migration_plan`] only for
/// the Bookclerk namespace. Plugin-owned history uses `bookclerk_plugin_migrations`,
/// not this ledger.
///
/// # Errors
///
/// Returns [`LibraryError::Schema`] on malformed, partial, or contradictory markers.
pub async fn current_schema_state_in(
    db: &DatabaseConnection,
    namespace: &str,
) -> Result<SchemaState> {
    let host_ns = namespace == BOOKCLERK_SCHEMA_NAMESPACE;
    let rows = match query_schema_migration_rows(db, namespace).await {
        Ok(rows) => rows,
        Err(_) => {
            if host_ns && host_tables_present(db).await? {
                // A peer may hold a schema lock, or have just committed. Re-read
                // before treating "tables without a readable marker" as durable.
                if let Ok(rows) = query_schema_migration_rows(db, namespace).await {
                    if let Some(state) = schema_state_from_migration_rows(rows, host_ns)? {
                        return Ok(state);
                    }
                }
                return Err(LibraryError::Schema(
                    "database has host tables but no readable bookclerk_schema_migrations state; \
                     recreate the database (`cargo reset --yes`)"
                        .into(),
                ));
            }
            return Ok(SchemaState::Uninitialized);
        }
    };

    if let Some(state) = schema_state_from_migration_rows(rows, host_ns)? {
        return Ok(state);
    }
    if host_ns && host_tables_present(db).await? {
        // Concurrent apply: the empty SELECT can lose to a peer COMMIT that
        // writes host tables and the marker together. Re-read the marker
        // before fail-closed.
        if let Ok(rows) = query_schema_migration_rows(db, namespace).await {
            if let Some(state) = schema_state_from_migration_rows(rows, host_ns)? {
                return Ok(state);
            }
        }
        return Err(LibraryError::Schema(
            "host tables exist without a schema state marker; recreate (`cargo reset --yes`)"
                .into(),
        ));
    }
    Ok(SchemaState::Uninitialized)
}

/// Reads [`SchemaState`] from `bookclerk_schema_migrations` on an already-open connection
/// (including a backup capture transaction).
///
/// # Errors
///
/// Returns when the marker table is missing, unreadable, or has no state row.
pub(crate) async fn schema_state_from_conn<C>(conn: &C) -> Result<SchemaState>
where
    C: ConnectionTrait,
{
    schema_state_from_conn_in(conn, BOOKCLERK_SCHEMA_NAMESPACE).await
}

/// Reads [`SchemaState`] for `namespace` on an already-open connection.
///
/// # Errors
///
/// Returns when the marker table is missing, unreadable, or has no state row.
pub(crate) async fn schema_state_from_conn_in<C>(conn: &C, namespace: &str) -> Result<SchemaState>
where
    C: ConnectionTrait,
{
    let rows = conn
        .query_all_raw(bookclerk_db_exec::canonical_statement(
            schema_migrations_select_sql(namespace),
            std::iter::empty::<sea_orm::Value>(),
        ))
        .await
        .map_err(|err| {
            LibraryError::Schema(format!(
                "backup cannot re-read bookclerk_schema_migrations inside the capture transaction: {err}"
            ))
        })?;
    match schema_state_from_migration_rows(rows, namespace == BOOKCLERK_SCHEMA_NAMESPACE)? {
        Some(state) => Ok(state),
        None if namespace == BOOKCLERK_SCHEMA_NAMESPACE => Err(LibraryError::Schema(
            "backup capture found bookclerk_schema_migrations without a state marker \
             for namespace `bookclerk`"
                .into(),
        )),
        None => Ok(SchemaState::Uninitialized),
    }
}

/// Loads namespaced `bookclerk_schema_migrations` version/state/checksum rows.
async fn query_schema_migration_rows(
    db: &DatabaseConnection,
    namespace: &str,
) -> std::result::Result<Vec<QueryResult>, sea_orm::DbErr> {
    let sql = schema_migrations_select_sql(namespace);
    bookclerk_db_exec::query_canonical(db, &sql, std::iter::empty::<sea_orm::Value>()).await
}

/// `SELECT` for one ledger namespace.
fn schema_migrations_select_sql(namespace: &str) -> String {
    format!(
        "SELECT version, state, checksum FROM bookclerk_schema_migrations WHERE namespace = {}",
        sql_string_literal(namespace)
    )
}

/// Postgres `INTEGER` is int4 (`i32`); SQLite INTEGER affinity is `i64`.
fn schema_migration_version(row: &QueryResult) -> Option<i64> {
    row.try_get::<i64>("", "version")
        .ok()
        .or_else(|| row.try_get_by_index::<i64>(0).ok())
        .or_else(|| row.try_get::<i32>("", "version").ok().map(i64::from))
        .or_else(|| row.try_get_by_index::<i32>(0).ok().map(i64::from))
}

/// Interprets `bookclerk_schema_migrations` rows. `Ok(None)` means the table exists but
/// has no unreleased or frozen marker.
///
/// When `verify_host_plan` is true, frozen checksums are checked against
/// [`host_migration_plan`]. Non-bookclerk namespaces skip that host-plan
/// check (legacy rows only; plugin history is `bookclerk_plugin_migrations`).
/// engine verifies the installed plugin plan.
fn schema_state_from_migration_rows(
    rows: Vec<QueryResult>,
    verify_host_plan: bool,
) -> Result<Option<SchemaState>> {
    let mut unreleased = None;
    let mut frozen: Vec<(i64, String)> = Vec::new();
    for row in rows {
        let version = schema_migration_version(&row).ok_or_else(|| {
            LibraryError::Schema("bookclerk_schema_migrations row is missing version".into())
        })?;
        let state = row
            .try_get::<String>("", "state")
            .ok()
            .or_else(|| row.try_get_by_index::<String>(1).ok())
            .unwrap_or_default();
        let checksum = row
            .try_get::<String>("", "checksum")
            .ok()
            .or_else(|| row.try_get_by_index::<String>(2).ok())
            .unwrap_or_default();
        if checksum.is_empty() {
            return Err(LibraryError::Schema(
                "bookclerk_schema_migrations row is missing a checksum".into(),
            ));
        }
        match state.as_str() {
            SCHEMA_STATE_UNRELEASED => {
                if unreleased.is_some() {
                    return Err(LibraryError::Schema(
                        "bookclerk_schema_migrations has multiple unreleased rows".into(),
                    ));
                }
                unreleased = Some((version, checksum));
            }
            SCHEMA_STATE_FROZEN => frozen.push((version, checksum)),
            "" => {
                return Err(LibraryError::Schema(
                    "bookclerk_schema_migrations row is missing state; unsupported old metadata — \
                     recreate (`cargo reset --yes`)"
                        .into(),
                ));
            }
            other => {
                return Err(LibraryError::Schema(format!(
                    "unrecognized bookclerk_schema_migrations.state `{other}`"
                )));
            }
        }
    }

    if let Some((base_version, checksum)) = unreleased {
        if base_version < 0 {
            return Err(LibraryError::Schema(format!(
                "unreleased base version {base_version} is invalid"
            )));
        }
        let max_frozen = frozen.iter().map(|(v, _)| *v).max();
        match max_frozen {
            None if base_version != 0 => {
                return Err(LibraryError::Schema(format!(
                    "unreleased@base{base_version} has no frozen base row; \
                     recreate (`cargo reset --yes`)"
                )));
            }
            Some(frozen_version) if frozen_version != base_version => {
                return Err(LibraryError::Schema(format!(
                    "unreleased@base{base_version} does not match frozen@{frozen_version}; \
                     recreate (`cargo reset --yes`)"
                )));
            }
            _ => {}
        }
        let found: HashMap<i64, String> = frozen.into_iter().collect();
        if verify_host_plan {
            verify_frozen_checksums(&host_migration_plan(), &found, base_version)?;
        }
        return Ok(Some(SchemaState::Unreleased {
            base_version,
            checksum,
        }));
    }
    if let Some((version, checksum)) = frozen.iter().cloned().max_by_key(|(v, _)| *v) {
        if version < 1 {
            return Err(LibraryError::Schema(format!(
                "frozen schema version {version} is invalid"
            )));
        }
        let found: HashMap<i64, String> = frozen.into_iter().collect();
        if verify_host_plan {
            verify_frozen_checksums(&host_migration_plan(), &found, version)?;
        }
        return Ok(Some(SchemaState::Frozen { version, checksum }));
    }
    Ok(None)
}

/// True when a concurrent apply can look like a durable missing marker.
fn is_schema_marker_visibility_race(err: &LibraryError) -> bool {
    match err {
        LibraryError::Schema(msg) => {
            msg.contains("without a schema state marker")
                || msg.contains("no readable bookclerk_schema_migrations state")
        }
        _ => false,
    }
}

/// True when a host table (`books`) exists without relying on schema state.
async fn host_tables_present(db: &DatabaseConnection) -> Result<bool> {
    crate::backup::util::table_exists(db, "books").await
}

/// `INSERT` for the unreleased `bookclerk_schema_migrations` row (no `PRAGMA user_version`).
///
/// `base_version` is the highest frozen revision this pack sits on (`0` before
/// any freeze). It is stored in `bookclerk_schema_migrations.version` and is not an
/// identity for uninitialized databases.
fn unreleased_marker_sql(checksum: &str, base_version: i64) -> String {
    unreleased_state_marker_sql(checksum, base_version)
}

/// Applies frozen plan steps, then the unreleased pack, on a fresh database.
///
/// When `unreleased` is empty, the database ends [`SchemaState::Frozen`] at the
/// last plan version (or stays uninitialized if the plan is also empty). When
/// the unreleased bucket is non-empty, frozen checksums are recorded first.
async fn apply_unreleased_pack<F, Fut>(db: &DatabaseConnection, run_batch: &mut F) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    apply_fresh_schema(
        db,
        run_batch,
        &host_migration_plan(),
        unreleased_ops(),
        SCHEMA_VERSION,
    )
    .await
}

/// Testable fresh-init apply: frozen steps then optional unreleased marker.
pub(crate) async fn apply_fresh_schema<F, Fut>(
    db: &DatabaseConnection,
    run_batch: &mut F,
    plan: &[HostMigrationStep],
    unreleased: &[crate::migrations::MigrationOp],
    schema_version: i64,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    ensure_schema_migrations(db).await?;
    for step in plan {
        crate::migrations::prove_migration_ops(step.steps)?;
        if let Some(down) = step.down {
            crate::migrations::prove_migration_ops(down)?;
        }
        apply_one_schema_migration_with_batch(db, step, run_batch).await?;
    }
    if unreleased.is_empty() {
        return Ok(());
    }
    crate::migrations::prove_migration_ops(unreleased)?;
    apply_unreleased_ops(
        db,
        run_batch,
        unreleased,
        &crate::migrations::migration_ops_checksum(unreleased, None),
        schema_version,
    )
    .await
}

/// Fresh-init helper for tests using the default atomic DDL runner.
#[cfg(test)]
pub(crate) async fn apply_fresh_schema_sqlite(
    db: &DatabaseConnection,
    plan: &[HostMigrationStep],
    unreleased: &[crate::migrations::MigrationOp],
    schema_version: i64,
) -> Result<()> {
    let exec = db.clone();
    let mut run_batch = move |stmts: Vec<String>| {
        let exec = exec.clone();
        async move { run_atomic_ddl(&exec, "schema-apply", stmts).await }
    };
    apply_fresh_schema(db, &mut run_batch, plan, unreleased, schema_version).await
}

/// Applies only [`crate::migrations::unreleased_ops`] after frozen ups.
async fn apply_unreleased_bucket<F, Fut>(db: &DatabaseConnection, run_batch: &mut F) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    apply_unreleased_ops(
        db,
        run_batch,
        unreleased_ops(),
        &unreleased_checksum(),
        SCHEMA_VERSION,
    )
    .await
}

/// Applies one unreleased op list plus the checksum marker.
async fn apply_unreleased_ops<F, Fut>(
    db: &DatabaseConnection,
    run_batch: &mut F,
    ops: &[crate::migrations::MigrationOp],
    checksum: &str,
    base_version: i64,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let batch = SchemaBatch::from_statements_and_marker(
        ops.iter().map(|op| op.sql().to_string()),
        unreleased_marker_sql(checksum, base_version),
    );
    let stmts = batch.statements;
    let mut delay_ms = 20u64;
    for attempt in 0..8 {
        match run_batch(stmts.clone()).await {
            Ok(()) => return Ok(()),
            Err(err) => match current_schema_state(db).await {
                Ok(SchemaState::Unreleased {
                    checksum: applied, ..
                }) if applied == checksum => return Ok(()),
                Err(state_err) if is_schema_marker_visibility_race(&state_err) => {
                    if attempt + 1 < 8 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(2).min(250);
                        continue;
                    }
                    return Err(state_err);
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
        "unreleased schema apply exhausted retries".into(),
    ))
}

/// Reads explicit [`SchemaState`], verifies frozen checksums, backs up, and plans a walk.
async fn prepare_schema_change(
    db: &DatabaseConnection,
    target: i64,
    opts: &SchemaApplyOptions,
) -> Result<SchemaWalk> {
    ensure_schema_migrations(db).await?;
    let state = current_schema_state(db).await?;
    let plan = host_migration_plan();
    if let SchemaState::Frozen { version, .. } = &state {
        verify_applied_checksums(db, &host_migration_plan(), *version).await?;
    }
    let walk = crate::schema_walk::plan_schema_walk_from_state(&plan, &state, target)?;
    if !walk.is_noop() {
        if let Some(snap) = opts.backup.as_ref() {
            if !matches!(state, SchemaState::Uninitialized) {
                let req = BackupRequest {
                    files_dir: snap.files_dir.clone(),
                    schema_state: state.clone(),
                    reason: BackupReason::PreMigrate,
                    to_version: walk.stopped_at,
                    include_plugin_databases: snap.include_plugin_databases,
                    consistent_backup_read: snap.consistent_backup_read,
                    backend_at_capture: snap.backend_at_capture.clone(),
                    max_result_rows: snap.max_result_rows.max(1),
                    max_result_bytes: snap.max_result_bytes.max(1),
                    max_atomic_result_bytes: snap.max_atomic_result_bytes.max(1),
                    plugin_units: snap.plugin_units.clone(),
                    adapter: snap.adapter.clone(),
                    in_process: false,
                };
                backup_library(db, &req).await?;
            }
        }
    }
    Ok(walk)
}

/// Applies a prepared walk using a guest `run_batch` closure.
async fn apply_walk_batch<F, Fut>(
    db: &DatabaseConnection,
    walk: &SchemaWalk,
    run_batch: &mut F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    for step in &walk.ups {
        apply_one_schema_migration_with_batch(db, step, run_batch).await?;
    }
    for step in &walk.downs {
        let stmts = down_statements(step);
        run_batch(stmts).await?;
    }
    Ok(())
}

/// Frozen revision when the database is [`SchemaState::Frozen`]; otherwise `0`.
///
/// This is **not** a discriminator for uninitialized vs unreleased. Prefer
/// [`current_schema_state`]. `0` means “no frozen revision,” never “applied
/// development schema.”
///
/// # Errors
///
/// Returns when the state query fails or the marker is malformed.
pub async fn current_schema_version(db: &DatabaseConnection) -> Result<i64> {
    Ok(current_schema_state(db)
        .await?
        .frozen_version()
        .unwrap_or(0))
}

/// Fails closed when this binary cannot interpret the target database's
/// schema history well enough to drop every unknown table before restore.
///
/// Uninitialized targets are replaceable. Frozen history newer than this
/// binary, unreleased checksums this binary does not know, and unreadable
/// markers are not.
///
/// # Errors
///
/// Returns [`LibraryError::Schema`] when the target is newer or uninterpretable.
pub async fn ensure_restore_target_is_replaceable(db: &DatabaseConnection) -> Result<()> {
    let state = current_schema_state(db).await?;
    let plan = host_migration_plan();
    match state {
        SchemaState::Uninitialized => Ok(()),
        SchemaState::Frozen { version, checksum } => {
            let max_plan = plan.iter().map(|s| s.version).max().unwrap_or(0);
            if version > max_plan {
                return Err(LibraryError::Schema(format!(
                    "restore target is frozen@{version}+{checksum}, newer than this binary \
                     (frozen plan ends at {max_plan}); run a newer Bookclerk binary"
                )));
            }
            verify_applied_checksums(db, &plan, version).await
        }
        SchemaState::Unreleased {
            base_version,
            checksum,
        } => {
            if base_version > SCHEMA_VERSION {
                return Err(LibraryError::Schema(format!(
                    "restore target is unreleased@base{base_version}+{checksum}, newer than \
                     this binary (base {SCHEMA_VERSION}); run a newer Bookclerk binary"
                )));
            }
            let expected = unreleased_checksum();
            if checksum != expected {
                return Err(LibraryError::Schema(format!(
                    "restore target unreleased checksum {checksum} is not this binary \
                     ({expected}); reset or recreate the database — this binary cannot \
                     list unknown tables to drop"
                )));
            }
            if base_version > 0 {
                verify_applied_checksums(db, &plan, base_version).await?;
            }
            Ok(())
        }
    }
}

/// Canonical DDL plus the `bookclerk_schema_migrations` insert.
fn version_marker_statements(step: &HostMigrationStep) -> Vec<String> {
    let mut stmts: Vec<String> = step.steps.iter().map(|op| op.sql().to_string()).collect();
    stmts.push(schema_migrations_insert(step));
    stmts
}

/// Reverse DDL plus deletion of this step's `bookclerk_schema_migrations` row.
fn down_statements(step: &HostMigrationStep) -> Vec<String> {
    let mut stmts = Vec::new();
    if let Some(down) = step.down {
        stmts.extend(down.iter().map(|op| op.sql().to_string()));
    }
    stmts.push(frozen_marker_delete_sql(
        BOOKCLERK_SCHEMA_NAMESPACE,
        step.version,
    ));
    stmts
}

/// `INSERT` for `bookclerk_schema_migrations` including checksum, app version, and timestamp.
fn schema_migrations_insert(step: &HostMigrationStep) -> String {
    frozen_marker_sql(BOOKCLERK_SCHEMA_NAMESPACE, step.version, &step.checksum())
}

/// Refuses when a stored checksum is missing, unreadable, or does not match
/// this binary's frozen SQL.
pub(crate) async fn verify_applied_checksums<C>(
    conn: &C,
    plan: &[HostMigrationStep],
    current: i64,
) -> Result<()>
where
    C: ConnectionTrait,
{
    if current <= 0 {
        return Ok(());
    }
    let rows = conn
        .query_all_raw(bookclerk_db_exec::canonical_statement(
            schema_migrations_select_sql(BOOKCLERK_SCHEMA_NAMESPACE),
            std::iter::empty::<sea_orm::Value>(),
        ))
        .await
        .map_err(|err| {
            LibraryError::Schema(format!(
                "schema version {current} is applied but checksum metadata is unreadable: {err}"
            ))
        })?;
    let mut found_by_version = HashMap::new();
    for row in rows {
        let state = row
            .try_get::<String>("", "state")
            .ok()
            .or_else(|| row.try_get_by_index::<String>(1).ok())
            .unwrap_or_default();
        if state != SCHEMA_STATE_FROZEN {
            continue;
        }
        let version = schema_migration_version(&row).unwrap_or(0);
        let checksum = row
            .try_get::<String>("", "checksum")
            .ok()
            .or_else(|| row.try_get_by_index::<String>(2).ok())
            .unwrap_or_default();
        found_by_version.insert(version, checksum);
    }
    verify_frozen_checksums(plan, &found_by_version, current)
}

/// Refuses when frozen `bookclerk_schema_migrations` rows through `through_version` are
/// missing or do not match this binary's plan.
pub(crate) fn verify_frozen_checksums(
    plan: &[HostMigrationStep],
    found_by_version: &HashMap<i64, String>,
    through_version: i64,
) -> Result<()> {
    if through_version <= 0 {
        return Ok(());
    }
    for step in plan.iter().filter(|s| s.version <= through_version) {
        let expected = step.checksum();
        let found = found_by_version
            .get(&step.version)
            .map(String::as_str)
            .unwrap_or("");
        if found.is_empty() {
            return Err(LibraryError::Schema(format!(
                "schema version {} is applied but checksum metadata is missing; \
                 restore a backup or recreate the library",
                step.version
            )));
        }
        if found != expected {
            return Err(LibraryError::Schema(format!(
                "schema version {} checksum mismatch (database {found}, binary {expected}); \
                 the frozen migration was edited",
                step.version
            )));
        }
    }
    Ok(())
}

/// Applies one `bookclerk_schema_migrations` step via `run_batch`.
async fn apply_one_schema_migration_with_batch<F, Fut>(
    db: &DatabaseConnection,
    step: &HostMigrationStep,
    run_batch: &mut F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let version = step.version;
    let mut delay_ms = 20u64;
    let mut last_err = None;
    for attempt in 0..8 {
        if schema_versions_applied(db).await?.contains(&version) {
            return Ok(());
        }
        let stmts = version_marker_statements(step);
        match run_batch(stmts).await {
            Ok(()) => return Ok(()),
            Err(err)
                if matches!(
                    err,
                    LibraryError::Conflict(_) | LibraryError::Unavailable(_)
                ) =>
            {
                if schema_versions_applied(db).await?.contains(&version) {
                    return Ok(());
                }
                if attempt + 1 < 8 && matches!(err, LibraryError::Unavailable(_)) {
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = delay_ms.saturating_mul(2).min(250);
                    continue;
                }
                if matches!(err, LibraryError::Conflict(_)) {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }
            Err(err) => return Err(err),
        }
    }
    if schema_versions_applied(db).await?.contains(&version) {
        return Ok(());
    }
    Err(last_err.expect("bookclerk_schema_migrations version retry"))
}

/// `CREATE TABLE IF NOT EXISTS bookclerk_schema_migrations`.
pub(crate) async fn ensure_schema_migrations(db: &DatabaseConnection) -> Result<()> {
    let mut delay_ms = 20u64;
    let mut last_err = None;
    for attempt in 0..8 {
        match bookclerk_db_exec::execute_canonical(db, SCHEMA_MIGRATIONS_DDL, [])
            .await
            .map_err(LibraryError::from_db_err)
        {
            Ok(_) => return Ok(()),
            Err(err)
                if attempt + 1 < 8
                    && matches!(
                        err,
                        LibraryError::Conflict(_) | LibraryError::Unavailable(_)
                    ) =>
            {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = delay_ms.saturating_mul(2).min(250);
            }
            Err(err) => return Err(err),
        }
    }
    match bookclerk_db_exec::execute_canonical(db, SCHEMA_MIGRATIONS_DDL, [])
        .await
        .map_err(LibraryError::from_db_err)
    {
        Ok(_) => Ok(()),
        Err(_) if last_err.is_some() => {
            // Peer created the table; a follow-up SELECT in the migrator confirms it.
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Runs `stmts` as one generic atomic execute plan (version marker last).
async fn run_atomic_ddl(
    db: &DatabaseConnection,
    operation_id: &str,
    stmts: Vec<String>,
) -> Result<()> {
    if stmts.is_empty() {
        return Ok(());
    }
    // Canonical pack + marker; `execute_typed_on` expands companions once.
    let req = ExecuteRequest {
        operation_id: operation_id.to_string(),
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
    execute_typed_on(db, &req, 0).await?;
    Ok(())
}

/// Loads `bookclerk_schema_migrations.version` rows.
async fn schema_versions_applied(db: &DatabaseConnection) -> Result<HashSet<i64>> {
    let rows = bookclerk_db_exec::query_canonical(
        db,
        &format!(
            "SELECT version FROM bookclerk_schema_migrations WHERE namespace = {}",
            sql_string_literal(BOOKCLERK_SCHEMA_NAMESPACE)
        ),
        std::iter::empty::<sea_orm::Value>(),
    )
    .await
    .map_err(LibraryError::Orm)?;
    Ok(rows.iter().filter_map(schema_migration_version).collect())
}

/// Executes one SQL string.
#[cfg(test)]
async fn exec_sql(db: &DatabaseConnection, backend: DbBackend, sql: &str) -> Result<()> {
    db.execute_raw(Statement::from_string(backend, sql.to_string()))
        .await
        .map_err(LibraryError::from_db_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::unreleased_checksum;
    use bookclerk_plugin_abi::DbCapabilities;

    #[test]
    fn schema_marker_visibility_race_matches_fail_closed_wording() {
        assert!(is_schema_marker_visibility_race(&LibraryError::Schema(
            "host tables exist without a schema state marker; recreate (`cargo reset --yes`)"
                .into()
        )));
        assert!(is_schema_marker_visibility_race(&LibraryError::Schema(
            "database has host tables but no readable bookclerk_schema_migrations state; \
             recreate the database (`cargo reset --yes`)"
                .into()
        )));
        assert!(!is_schema_marker_visibility_race(&LibraryError::Schema(
            "unreleased schema checksum abc does not match this binary (def)".into()
        )));
    }

    #[test]
    fn current_canonical_schema_is_unreleased_while_plan_empty() {
        use crate::migrations::{
            current_canonical_schema, current_canonical_statements, host_migration_plan,
            unreleased_sql, unreleased_statements,
        };
        assert!(host_migration_plan().is_empty());
        assert_eq!(current_canonical_statements(), unreleased_statements());
        assert_eq!(current_canonical_schema(), unreleased_sql());
        assert!(current_canonical_schema().contains("plugin_databases"));
        assert!(!current_canonical_schema().contains("domain_events_v27"));
    }

    #[test]
    fn require_schema_migrations_reads_flags_not_identity() {
        require_schema_migrations(&DbCapabilities::advertised_sqlite()).unwrap();
        require_schema_migrations(&DbCapabilities::advertised_postgres()).unwrap();
        require_schema_migrations(&DbCapabilities::advertised_d1()).unwrap();

        let mut none = DbCapabilities::advertised_sqlite();
        none.schema_migrations = false;
        assert!(require_schema_migrations(&none).is_err());
    }

    #[test]
    fn verify_frozen_checksums_rejects_missing_and_tampered_rows() {
        const V1: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY)";
        const V1_OPS: &[MigrationOp] = &[MigrationOp::Schema(V1)];
        let plan = [HostMigrationStep {
            version: 1,
            steps: V1_OPS,
            down: None,
            introduced_in: "0.0.0",
        }];
        let mut found = HashMap::new();
        found.insert(1, plan[0].checksum());
        verify_frozen_checksums(&plan, &found, 1).expect("matching frozen row");
        found.insert(1, "deadbeef".repeat(8));
        let err = verify_frozen_checksums(&plan, &found, 1).expect_err("tampered");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        found.clear();
        let err = verify_frozen_checksums(&plan, &found, 1).expect_err("missing");
        assert!(err.to_string().contains("missing"), "{err}");
        verify_frozen_checksums(&plan, &HashMap::new(), 0).expect("pre-v1");
    }

    #[tokio::test]
    async fn fresh_init_synthetic_v1_empty_unreleased_is_frozen() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("sqlite");
        const V1: &str =
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)";
        const V1_OPS: &[MigrationOp] = &[MigrationOp::Schema(V1)];
        let plan = [HostMigrationStep {
            version: 1,
            steps: V1_OPS,
            down: None,
            introduced_in: "0.0.0",
        }];
        let exec = db.clone();
        let mut run_batch = move |stmts: Vec<String>| {
            let exec = exec.clone();
            async move { run_atomic_ddl(&exec, "schema-apply", stmts).await }
        };
        apply_fresh_schema(&db, &mut run_batch, &plan, &[], 1)
            .await
            .expect("fresh frozen");
        let state = current_schema_state(&db).await.expect("state");
        match state {
            SchemaState::Frozen { version, checksum } => {
                assert_eq!(version, 1);
                assert_eq!(checksum, plan[0].checksum());
            }
            other => panic!("expected Frozen@1, got {other}"),
        }
    }

    #[tokio::test]
    async fn fresh_init_synthetic_v1_then_unreleased_records_frozen_history() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("sqlite");
        const V1: &str =
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)";
        const EXTRA: &str = "CREATE TABLE extra_dev (id INTEGER PRIMARY KEY)";
        const V1_OPS: &[MigrationOp] = &[MigrationOp::Schema(V1)];
        const EXTRA_OPS: &[MigrationOp] = &[MigrationOp::Schema(EXTRA)];
        let plan = [HostMigrationStep {
            version: 1,
            steps: V1_OPS,
            down: None,
            introduced_in: "0.0.0",
        }];
        let exec = db.clone();
        let mut run_batch = move |stmts: Vec<String>| {
            let exec = exec.clone();
            async move { run_atomic_ddl(&exec, "schema-apply", stmts).await }
        };
        apply_fresh_schema(&db, &mut run_batch, &plan, EXTRA_OPS, 1)
            .await
            .expect("fresh unreleased");
        let state = current_schema_state(&db).await.expect("state");
        match state {
            SchemaState::Unreleased {
                base_version,
                checksum,
            } => {
                assert_eq!(base_version, 1);
                assert_eq!(
                    checksum,
                    crate::migrations::migration_ops_checksum(EXTRA_OPS, None)
                );
            }
            other => panic!("expected Unreleased@base1, got {other}"),
        }
        let rows = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT version, state, checksum FROM bookclerk_schema_migrations ORDER BY state, version",
            ))
            .await
            .expect("rows");
        assert_eq!(rows.len(), 2, "frozen@1 plus unreleased marker");
        let mut saw_frozen = false;
        for row in &rows {
            let state: String = row.try_get("", "state").unwrap();
            let version: i64 = row.try_get("", "version").unwrap();
            let checksum: String = row.try_get("", "checksum").unwrap();
            if state == SCHEMA_STATE_FROZEN {
                saw_frozen = true;
                assert_eq!(version, 1);
                assert_eq!(checksum, plan[0].checksum());
            }
        }
        assert!(saw_frozen);
    }

    #[tokio::test]
    async fn schema_migrations_on_sqlite_uses_canonical_sql_not_postgres_pack() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        apply_host_schema(&db)
            .await
            .expect("canonical sqlite pack on sqlite backend");
        let cols = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA table_info(books)",
            ))
            .await
            .expect("table_info");
        assert!(
            !cols.is_empty(),
            "canonical unreleased host schema must create books"
        );
    }

    #[tokio::test]
    async fn atomic_batch_marker_applies_canonical_plan() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        let db_batch = db.clone();
        apply_host_schema_with_batch(&db, move |stmts| {
            let db_batch = db_batch.clone();
            async move { run_atomic_ddl(&db_batch, "atomic-batch", stmts).await }
        })
        .await
        .expect("atomic batch schema");
        let state = current_schema_state(&db).await.unwrap();
        assert!(matches!(state, SchemaState::Unreleased { .. }), "{state}");
        let cols = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA table_info(domain_events)",
            ))
            .await
            .unwrap();
        let names: Vec<String> = cols
            .iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect();
        assert!(
            names.iter().any(|n| n == "dispatch_snapshot_json"),
            "dispatch snapshot column missing: {names:?}"
        );
        db.query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT slot_key FROM bookclerk_slots LIMIT 1",
        ))
        .await
        .expect("serialization slots table");
    }

    #[tokio::test]
    async fn sqlite_crash_before_version_marker_rolls_back_and_retries() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        let ddl = crate::migrations::current_canonical_statements().len() as u32;
        crate::inject_atomic_interrupt_after(
            crate::AtomicInterruptPhase::BetweenStatements,
            crate::AtomicInterruptKind::Cancel,
            ddl,
        );
        let err = apply_host_schema(&db)
            .await
            .expect_err("interrupt before version marker");
        assert!(err.to_string().to_lowercase().contains("cancel"), "{err}");
        assert_eq!(
            current_schema_state(&db).await.unwrap(),
            SchemaState::Uninitialized
        );
        apply_host_schema(&db).await.expect("retry after crash");
        assert!(matches!(
            current_schema_state(&db).await.unwrap(),
            SchemaState::Unreleased { .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_concurrent_apply_host_schema_both_ok() {
        // Repeat: CI caught a TOCTOU where the loser saw host tables before the
        // winner's unreleased marker became visible on the same connection.
        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("lib.db");
            let db1 = bookclerk_plugin_database_sqlite::open(&path)
                .await
                .expect("open 1");
            let db2 = bookclerk_plugin_database_sqlite::open(&path)
                .await
                .expect("open 2");
            let (a, b) = tokio::join!(apply_host_schema(&db1), apply_host_schema(&db2),);
            a.unwrap_or_else(|err| panic!("round {round} first apply: {err}"));
            b.unwrap_or_else(|err| panic!("round {round} second apply: {err}"));
            assert!(
                matches!(
                    current_schema_state(&db1).await.unwrap(),
                    SchemaState::Unreleased { .. }
                ),
                "round {round}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
    async fn postgres_crash_before_version_marker_rolls_back_and_retries() {
        if std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
        {
            return;
        }
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap();
        let db_name = format!("mig_{}", uuid::Uuid::new_v4().as_simple());
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
        let mut batch: Vec<String> = crate::migrations::current_canonical_statements().to_vec();
        batch.push(unreleased_marker_sql(
            &unreleased_checksum(),
            SCHEMA_VERSION,
        ));
        let ddl = bookclerk_db_exec::expand_host_schema_batch(DbBackend::Postgres, &batch)
            .expect("postgres schema batch")
            .len()
            .saturating_sub(1) as u32;
        crate::inject_atomic_interrupt_after(
            crate::AtomicInterruptPhase::BetweenStatements,
            crate::AtomicInterruptKind::Cancel,
            ddl,
        );
        let err = apply_host_schema(&db).await.expect_err("interrupt");
        assert!(err.to_string().to_lowercase().contains("cancel"), "{err}");
        apply_host_schema(&db).await.expect("retry");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
    async fn postgres_concurrent_apply_host_schema_both_ok() {
        if std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
        {
            return;
        }
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap();
        let db_name = format!("migc_{}", uuid::Uuid::new_v4().as_simple());
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
        let db1 = sea_orm::Database::connect(&db_url).await.expect("c1");
        let db2 = sea_orm::Database::connect(&db_url).await.expect("c2");
        let (a, b) = tokio::join!(apply_host_schema(&db1), apply_host_schema(&db2),);
        a.expect("first apply");
        b.expect("second apply");
    }

    #[tokio::test]
    async fn uninitialized_applies_unreleased_and_second_start_is_noop() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        assert_eq!(
            current_schema_state(&db).await.unwrap(),
            SchemaState::Uninitialized
        );
        apply_host_schema(&db).await.expect("apply unreleased");
        let first = current_schema_state(&db).await.unwrap();
        assert!(matches!(first, SchemaState::Unreleased { .. }), "{first}");
        assert_eq!(
            first.display(),
            format!("unreleased@base0+{}", unreleased_checksum())
        );
        assert_eq!(first.unreleased_base_version(), Some(0));
        assert_eq!(first.frozen_version(), None);
        apply_host_schema(&db).await.expect("idempotent");
        let second = current_schema_state(&db).await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn unreleased_checksum_mismatch_fails_closed_with_reset() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        apply_host_schema(&db).await.expect("apply");
        exec_sql(
            &db,
            DbBackend::Sqlite,
            "UPDATE bookclerk_schema_migrations SET checksum = 'deadbeef' WHERE state = 'unreleased'",
        )
        .await
        .expect("tamper");
        let err = apply_host_schema(&db).await.expect_err("checksum");
        assert!(
            err.to_string().contains("does not match this binary"),
            "{err}"
        );
        assert!(err.to_string().contains("cargo reset --yes"), "{err}");
    }

    #[tokio::test]
    async fn frozen_newer_than_empty_plan_fails_closed() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        apply_host_schema(&db).await.expect("apply");
        exec_sql(
            &db,
            DbBackend::Sqlite,
            "DELETE FROM bookclerk_schema_migrations WHERE state = 'unreleased'",
        )
        .await
        .expect("drop unreleased");
        exec_sql(
            &db,
            DbBackend::Sqlite,
            "INSERT INTO bookclerk_schema_migrations (namespace, version, state, checksum, app_version, applied_at) \
             VALUES ('bookclerk', 99, 'frozen', 'abc', 'test', 't')",
        )
        .await
        .expect("fake frozen");
        let err = apply_host_schema(&db).await.expect_err("newer frozen");
        assert!(err.to_string().contains("newer than this binary"), "{err}");
        assert!(err.to_string().contains("frozen@99"), "{err}");
    }

    #[tokio::test]
    async fn unreleased_base_must_match_frozen_history() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        apply_host_schema(&db).await.expect("apply");
        exec_sql(
            &db,
            DbBackend::Sqlite,
            "UPDATE bookclerk_schema_migrations SET version = 1 WHERE state = 'unreleased'",
        )
        .await
        .expect("bump base");
        let err = current_schema_state(&db)
            .await
            .expect_err("base without frozen");
        assert!(err.to_string().contains("no frozen base row"), "{err}");

        exec_sql(
            &db,
            DbBackend::Sqlite,
            "INSERT INTO bookclerk_schema_migrations (namespace, version, state, checksum, app_version, applied_at) \
             VALUES ('bookclerk', 1, 'frozen', 'f1', 'test', 't')",
        )
        .await
        .expect("frozen v1");
        let state = current_schema_state(&db).await.unwrap();
        assert_eq!(
            state,
            SchemaState::Unreleased {
                base_version: 1,
                checksum: unreleased_checksum(),
            }
        );

        exec_sql(
            &db,
            DbBackend::Sqlite,
            "UPDATE bookclerk_schema_migrations SET version = 0 WHERE state = 'unreleased'",
        )
        .await
        .expect("reset base 0");
        let err = current_schema_state(&db)
            .await
            .expect_err("base 0 with frozen v1");
        assert!(err.to_string().contains("does not match frozen@1"), "{err}");
    }

    #[test]
    fn foreign_key_is_not_schema_apply_retryable() {
        let err = LibraryError::from_db_err(sea_orm::DbErr::Custom(
            "SQLITE_CONSTRAINT_FOREIGNKEY (787)".into(),
        ));
        assert!(!err.is_schema_apply_retryable(), "{err}");
        let unique = LibraryError::from_db_err(sea_orm::DbErr::Custom(
            "SQLITE_CONSTRAINT_UNIQUE (2067)".into(),
        ));
        assert!(unique.is_schema_apply_retryable(), "{unique}");
        let check = LibraryError::from_db_err(sea_orm::DbErr::Custom(
            "SQLITE_CONSTRAINT_CHECK (275)".into(),
        ));
        assert!(!check.is_schema_apply_retryable(), "{check}");
        let notnull = LibraryError::from_db_err(sea_orm::DbErr::Custom(
            "SQLITE_CONSTRAINT_NOTNULL (1299)".into(),
        ));
        assert!(!notnull.is_schema_apply_retryable(), "{notnull}");
    }

    #[tokio::test]
    async fn contradictory_and_malformed_markers_fail_closed() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        apply_host_schema(&db).await.expect("apply");

        exec_sql(
            &db,
            DbBackend::Sqlite,
            "UPDATE bookclerk_schema_migrations SET checksum = '' WHERE state = 'unreleased'",
        )
        .await
        .expect("empty checksum");
        let err = current_schema_state(&db).await.expect_err("empty checksum");
        assert!(err.to_string().contains("missing a checksum"), "{err}");

        exec_sql(
            &db,
            DbBackend::Sqlite,
            &format!(
                "UPDATE bookclerk_schema_migrations SET checksum = '{}' WHERE state = 'unreleased'",
                unreleased_checksum()
            ),
        )
        .await
        .expect("restore checksum");
        exec_sql(
            &db,
            DbBackend::Sqlite,
            "INSERT INTO bookclerk_schema_migrations (namespace, version, state, checksum, app_version, applied_at) \
             VALUES ('bookclerk', 1, 'unreleased', 'other', 'test', 't')",
        )
        .await
        .expect("second unreleased");
        let err = current_schema_state(&db)
            .await
            .expect_err("multiple unreleased");
        assert!(err.to_string().contains("multiple unreleased"), "{err}");

        exec_sql(
            &db,
            DbBackend::Sqlite,
            "DELETE FROM bookclerk_schema_migrations WHERE checksum = 'other'",
        )
        .await
        .expect("drop extra");
        exec_sql(
            &db,
            DbBackend::Sqlite,
            "UPDATE bookclerk_schema_migrations SET state = 'weird' WHERE state = 'unreleased'",
        )
        .await
        .expect("unknown state");
        let err = current_schema_state(&db).await.expect_err("unknown state");
        assert!(err.to_string().contains("unrecognized"), "{err}");
    }

    #[tokio::test]
    async fn leftover_pragma_does_not_block_schema_migrations_apply() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        exec_sql(&db, DbBackend::Sqlite, "PRAGMA user_version = 99")
            .await
            .expect("bump");
        apply_host_schema(&db)
            .await
            .expect("pragma is adapter-private");
        assert!(matches!(
            current_schema_state(&db).await.expect("state"),
            SchemaState::Unreleased { .. } | SchemaState::Frozen { .. }
        ));
    }

    #[tokio::test]
    async fn host_tables_without_marker_fail_closed() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        apply_host_schema(&db).await.expect("apply");
        exec_sql(
            &db,
            DbBackend::Sqlite,
            "DELETE FROM bookclerk_schema_migrations",
        )
        .await
        .expect("drop marker");
        let err = apply_host_schema(&db).await.expect_err("malformed");
        assert!(
            err.to_string().contains("without a schema state marker")
                || err.to_string().contains("recreate"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced_after_unreleased_apply() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        apply_host_schema(&db).await.expect("apply");
        let err = exec_sql(
            &db,
            DbBackend::Sqlite,
            "INSERT INTO books (uuid, source, account_id, product_id, marketplace, title, created_at, updated_at) \
             VALUES ('u', 'audible', 'missing', 'p', 'us', 't', 't', 't')",
        )
        .await
        .expect_err("fk");
        let s = err.to_string().to_lowercase();
        assert!(
            s.contains("foreign") || s.contains("constraint") || s.contains("787"),
            "{err}"
        );
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
    async fn postgres_sqlx_unique_violation_is_conflict() {
        if std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
        {
            return;
        }
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap();
        let db = sea_orm::Database::connect(url.as_str())
            .await
            .expect("connect");
        let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
        let table = format!("uniq_{}", uuid::Uuid::new_v4().as_simple());
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY)"),
            ),
        )
        .await
        .expect("create table");
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                format!("INSERT INTO {table} (id) VALUES (1)"),
            ),
        )
        .await
        .expect("insert");
        let err = sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                format!("INSERT INTO {table} (id) VALUES (1)"),
            ),
        )
        .await
        .expect_err("duplicate key");
        let _ = sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(backend, format!("DROP TABLE IF EXISTS {table}")),
        )
        .await;
        assert_eq!(
            bookclerk_db_exec::classify_db_err(&err),
            bookclerk_db_exec::DbErrorClass::Conflict,
            "display={err} debug={err:?}"
        );
        assert!(
            matches!(LibraryError::from_db_err(err), LibraryError::Conflict(_)),
            "typed 23505 must map to Conflict even when Display omits the SQLSTATE"
        );
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
    async fn postgres_sqlx_fk_is_not_schema_apply_retryable() {
        if std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
        {
            return;
        }
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap();
        let db = sea_orm::Database::connect(url.as_str())
            .await
            .expect("connect");
        let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
        let suffix = uuid::Uuid::new_v4().as_simple().to_string();
        let parent = format!("fk_p_{suffix}");
        let child = format!("fk_c_{suffix}");
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                format!("CREATE TABLE {parent} (id INTEGER PRIMARY KEY)"),
            ),
        )
        .await
        .expect("parent");
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                format!(
                    "CREATE TABLE {child} (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES {parent}(id))"
                ),
            ),
        )
        .await
        .expect("child");
        let err = sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                format!("INSERT INTO {child} (id, parent_id) VALUES (1, 99)"),
            ),
        )
        .await
        .expect_err("fk");
        let _ = sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(backend, format!("DROP TABLE IF EXISTS {child}")),
        )
        .await;
        let _ = sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(backend, format!("DROP TABLE IF EXISTS {parent}")),
        )
        .await;
        assert_eq!(
            bookclerk_db_exec::classify_db_err(&err),
            bookclerk_db_exec::DbErrorClass::Other,
            "display={err} debug={err:?}"
        );
        assert!(!LibraryError::from_db_err(err).is_schema_apply_retryable());
    }
}
