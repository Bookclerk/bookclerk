//! Host-owned schema application after a database guest connects.
//!
//! Guests open a connection and ping. The host reads the current version and
//! applies each remaining step from [`crate::migrations::host_migration_plan`]
//! as **one** atomic unit (DDL + version marker last). Marker kind (schema
//! capability flags) selects only the versioning mechanic (`PRAGMA
//! user_version`, `schema_migrations` row, or one HTTP `{ "batch": [...] }`
//! per version). Canonical DDL lives in the host plan; the live connection
//! backend chooses adapter-edge lowering via
//! [`bookclerk_db_exec::expand_host_schema_batch`] at execution time.
//!
//! TODO(#squash): collapse the long migration chain to a single baseline version.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use bookclerk_plugin_abi::DbPlanStatementKind;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

use crate::error::{LibraryError, Result};
use crate::migrations::{host_migration_plan, HostMigrationStep};
use crate::sql_plan::{execute_statements_on, DbAtomicPlan, DbPlanStatement};

/// Timing label for host schema apply (not an adapter identity).
const SCHEMA_TXN_TIMING: &str = "schema_txn";

/// Canonical schema apply batch: host DDL followed by the version marker.
///
/// Production and test-only executors must consume this same representation.
/// Adapters lower and split the pack at execution
/// ([`bookclerk_db_exec::expand_host_schema_batch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaBatch {
    /// Ordered SQL strings; the last statement is the version marker.
    pub statements: Vec<String>,
}

impl SchemaBatch {
    /// Builds a batch from one canonical DDL pack plus a trailing marker.
    #[must_use]
    pub fn from_ddl_and_marker(ddl: impl Into<String>, marker: impl Into<String>) -> Self {
        Self {
            statements: vec![ddl.into(), marker.into()],
        }
    }

    /// SQLite `PRAGMA user_version` marker last.
    #[must_use]
    pub fn with_pragma_marker(ddl: &str, version: i64) -> Self {
        Self::from_ddl_and_marker(ddl, format!("PRAGMA user_version = {version}"))
    }

    /// `schema_migrations` row marker last.
    #[must_use]
    pub fn with_row_marker(ddl: &str, version: i64) -> Self {
        Self::from_ddl_and_marker(
            ddl,
            format!("INSERT INTO schema_migrations (version) VALUES ({version})"),
        )
    }
}

/// Which versioning mechanic the host should use.
///
/// Flags choose **how** versions are stored and applied, not which SQL pack
/// to emit. Canonical Bookclerk SQL is [`crate::migrations::migration_sql`].
/// Adapters lower canonical DDL for the live connection backend at execution
/// (see [`bookclerk_db_exec::expand_host_schema_batch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSchemaKind {
    /// `PRAGMA user_version` on an interactive SQLite-family connection.
    PragmaMarker,
    /// `schema_migrations` rows without requiring atomic HTTP batches.
    RowMarker,
    /// `schema_migrations` plus one atomic batch per version (D1-style).
    AtomicBatchMarker,
}

impl HostSchemaKind {
    /// Selects a schema **apply mechanic** from typed
    /// [`bookclerk_plugin_abi::DbCapabilities`] versioning flags.
    ///
    /// Plugin identity, `dialect`, and `sqlFamily` are not consulted. SQL text
    /// is chosen from the live connection backend when applying (canonical
    /// SQLite pack, or the Postgres adapter-edge pack in `bookclerk-db-exec`),
    /// not from these flags. A conforming adapter may use any plugin id as
    /// long as it advertises exactly one of:
    ///
    /// - `pragmaUserVersion` (`PRAGMA user_version` marker)
    /// - `schemaMigrations` without `atomicSchemaBatch` (row marker)
    /// - `schemaMigrations` + `atomicSchemaBatch` (atomic batch apply)
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Other`] when the flags are missing, mixed, or
    /// contradictory.
    pub fn from_db_capabilities(caps: &bookclerk_plugin_abi::DbCapabilities) -> Result<Self> {
        let kind = if caps.pragma_user_version
            && !caps.schema_migrations
            && !caps.atomic_schema_batch
        {
            Self::PragmaMarker
        } else if caps.schema_migrations && caps.atomic_schema_batch && !caps.pragma_user_version {
            Self::AtomicBatchMarker
        } else if caps.schema_migrations && !caps.atomic_schema_batch && !caps.pragma_user_version {
            Self::RowMarker
        } else {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "database guest schema flags are not a known versioning contract \
                 (pragmaUserVersion={}, schemaMigrations={}, atomicSchemaBatch={})",
                caps.pragma_user_version,
                caps.schema_migrations,
                caps.atomic_schema_batch
            )));
        };
        kind.advertised_db_capabilities_match(caps)?;
        Ok(kind)
    }

    /// Checks typed capability flags against this marker kind.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Other`] when the guest advertised a different
    /// versioning scheme than this kind requires.
    pub fn advertised_db_capabilities_match(
        self,
        caps: &bookclerk_plugin_abi::DbCapabilities,
    ) -> Result<()> {
        let ok = match self {
            Self::PragmaMarker => caps.pragma_user_version && !caps.atomic_schema_batch,
            Self::AtomicBatchMarker => caps.schema_migrations && caps.atomic_schema_batch,
            Self::RowMarker => {
                caps.schema_migrations && !caps.atomic_schema_batch && !caps.pragma_user_version
            }
        };
        if ok {
            Ok(())
        } else {
            Err(LibraryError::Other(anyhow::anyhow!(
                "database plugin advertised schema flags do not match {:?}",
                self
            )))
        }
    }
}

/// Applies pending host-authored DDL from the canonical migration plan.
///
/// # Errors
///
/// Returns [`LibraryError`] when a version read or DDL statement fails.
pub async fn apply_host_schema(db: &DatabaseConnection, kind: HostSchemaKind) -> Result<()> {
    match kind {
        HostSchemaKind::PragmaMarker => apply_sqlite_user_version(db).await,
        HostSchemaKind::RowMarker | HostSchemaKind::AtomicBatchMarker => {
            let backend = db.get_database_backend();
            apply_schema_migrations_from_plan(
                db,
                backend,
                SCHEMA_TXN_TIMING,
                &host_migration_plan(),
            )
            .await
        }
    }
}

/// Applies schema using `run_batch` (typed `executeAtomic`) for each version.
///
/// # Errors
///
/// Returns [`LibraryError`] when a version read, DDL statement, or batch fails.
pub async fn apply_host_schema_with_batch<F, Fut>(
    db: &DatabaseConnection,
    kind: HostSchemaKind,
    mut run_batch: F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    match kind {
        HostSchemaKind::PragmaMarker => {
            apply_sqlite_user_version_with_batch(db, &mut run_batch).await
        }
        HostSchemaKind::RowMarker | HostSchemaKind::AtomicBatchMarker => {
            let backend = db.get_database_backend();
            apply_schema_migrations_with_batch(db, backend, &host_migration_plan(), &mut run_batch)
                .await
        }
    }
}

/// True when schema apply should retry after re-reading durable state.
///
/// [`LibraryError::Unavailable`] covers locks, deadlocks, and lost commits.
/// [`LibraryError::Conflict`] covers concurrent `CREATE TABLE IF NOT EXISTS`
/// catalog uniqueness (Postgres `23505` on `pg_type`) and a peer inserting the
/// version marker before this migrator observes it. Callers re-read the
/// version first; retrying the idempotent DDL+marker batch is not a blind
/// re-apply.
fn is_schema_retryable(err: &LibraryError) -> bool {
    matches!(
        err,
        LibraryError::Unavailable(_) | LibraryError::Conflict(_)
    )
}

/// Applies pending SQLite `PRAGMA user_version` migrations via `run_batch`.
///
/// # Errors
///
/// Returns when a version read or batch fails.
async fn apply_sqlite_user_version_with_batch<F, Fut>(
    db: &DatabaseConnection,
    run_batch: &mut F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let backend = DbBackend::Sqlite;
    exec_sql(db, backend, "PRAGMA foreign_keys = OFF").await?;
    let steps = host_migration_plan();
    for step in &steps {
        apply_one_sqlite_version_with_batch(db, step.version, step.canonical, run_batch).await?;
    }
    exec_sql(db, backend, "PRAGMA foreign_keys = ON").await?;
    Ok(())
}

/// Applies one SQLite schema version as a `run_batch` transaction.
///
/// # Errors
///
/// Returns when the batch fails.
async fn apply_one_sqlite_version_with_batch<F, Fut>(
    db: &DatabaseConnection,
    version: i64,
    schema: &str,
    run_batch: &mut F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let batch = SchemaBatch::with_pragma_marker(schema, version);
    let mut delay_ms = 20u64;
    let mut last_applied_err = None;
    for attempt in 0..8 {
        if version <= sqlite_user_version(db).await? {
            return Ok(());
        }
        match run_batch(batch.statements.clone()).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if version <= sqlite_user_version(db).await? {
                    return Ok(());
                }
                if attempt + 1 < 8 && is_schema_retryable(&err) {
                    last_applied_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = delay_ms.saturating_mul(2).min(250);
                    continue;
                }
                return Err(err);
            }
        }
    }
    if version <= sqlite_user_version(db).await? {
        return Ok(());
    }
    Err(last_applied_err.expect("sqlite schema version retry"))
}

/// Applies pending `schema_migrations` versions via `run_batch`.
///
/// # Errors
///
/// Returns when a version read or batch fails.
async fn apply_schema_migrations_with_batch<F, Fut>(
    db: &DatabaseConnection,
    backend: DbBackend,
    steps: &[HostMigrationStep],
    run_batch: &mut F,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    ensure_schema_migrations(db, backend).await?;
    for step in steps {
        let version = step.version;
        let schema = step.canonical;
        let batch = SchemaBatch::with_row_marker(schema, version);
        let mut delay_ms = 20u64;
        let mut last_err = None;
        for attempt in 0..8 {
            if schema_versions_applied(db, backend)
                .await?
                .contains(&version)
            {
                break;
            }
            match run_batch(batch.statements.clone()).await {
                Ok(()) => break,
                Err(err) => {
                    if schema_versions_applied(db, backend)
                        .await?
                        .contains(&version)
                    {
                        break;
                    }
                    if attempt + 1 < 8 && is_schema_retryable(&err) {
                        last_err = Some(err);
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(2).min(250);
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        if !schema_versions_applied(db, backend)
            .await?
            .contains(&version)
        {
            if let Some(err) = last_err {
                return Err(err);
            }
        }
    }
    Ok(())
}

/// Applies remaining `PRAGMA user_version` steps (file SQLite).
async fn apply_sqlite_user_version(db: &DatabaseConnection) -> Result<()> {
    let backend = DbBackend::Sqlite;
    exec_sql(db, backend, "PRAGMA foreign_keys = OFF").await?;
    let steps = host_migration_plan();
    for step in &steps {
        apply_one_sqlite_version(db, backend, step.version, step.canonical).await?;
    }
    exec_sql(db, backend, "PRAGMA foreign_keys = ON").await?;
    Ok(())
}

/// Applies one file-SQLite version, recovering when a peer committed DDL first.
async fn apply_one_sqlite_version(
    db: &DatabaseConnection,
    backend: DbBackend,
    version: i64,
    schema: &str,
) -> Result<()> {
    let mut delay_ms = 20u64;
    let mut last_applied_err = None;
    for attempt in 0..8 {
        if version <= sqlite_user_version(db).await? {
            return Ok(());
        }
        let batch = SchemaBatch::with_pragma_marker(schema, version);
        match run_atomic_ddl(
            db,
            backend,
            SCHEMA_TXN_TIMING,
            &format!("migrate-sqlite-{version}"),
            batch.statements,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                if version <= sqlite_user_version(db).await? {
                    return Ok(());
                }
                if attempt + 1 < 8 && is_schema_retryable(&err) {
                    last_applied_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = delay_ms.saturating_mul(2).min(250);
                    continue;
                }
                return Err(err);
            }
        }
    }
    if version <= sqlite_user_version(db).await? {
        return Ok(());
    }
    Err(last_applied_err.expect("sqlite schema version retry"))
}

/// Reads `PRAGMA user_version`.
async fn sqlite_user_version(db: &DatabaseConnection) -> Result<i64> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA user_version",
        ))
        .await
        .map_err(LibraryError::Orm)?;
    let Some(row) = rows.first() else {
        return Ok(0);
    };
    if let Ok(v) = row.try_get::<i64>("", "user_version") {
        return Ok(v);
    }
    if let Ok(v) = row.try_get_by_index::<i64>(0) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get_by_index::<i32>(0) {
        return Ok(i64::from(v));
    }
    Ok(0)
}

/// Applies pending `schema_migrations` versions from the canonical host plan.
async fn apply_schema_migrations_from_plan(
    db: &DatabaseConnection,
    backend: DbBackend,
    timing: &str,
    steps: &[HostMigrationStep],
) -> Result<()> {
    ensure_schema_migrations(db, backend).await?;
    for step in steps {
        let schema = step.canonical;
        apply_one_schema_migration(db, backend, timing, step.version, schema).await?;
    }
    Ok(())
}

/// Applies one `schema_migrations` version, recovering from a concurrent peer.
async fn apply_one_schema_migration(
    db: &DatabaseConnection,
    backend: DbBackend,
    timing: &str,
    version: i64,
    schema: &str,
) -> Result<()> {
    let mut delay_ms = 20u64;
    let mut last_err = None;
    for attempt in 0..8 {
        if schema_versions_applied(db, backend)
            .await?
            .contains(&version)
        {
            return Ok(());
        }
        let batch = SchemaBatch::with_row_marker(schema, version);
        match run_atomic_ddl(
            db,
            backend,
            timing,
            &format!("migrate-{timing}-{version}"),
            batch.statements,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                if schema_versions_applied(db, backend)
                    .await?
                    .contains(&version)
                {
                    return Ok(());
                }
                if attempt + 1 < 8 && is_schema_retryable(&err) {
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = delay_ms.saturating_mul(2).min(250);
                    continue;
                }
                return Err(err);
            }
        }
    }
    if schema_versions_applied(db, backend)
        .await?
        .contains(&version)
    {
        return Ok(());
    }
    Err(last_err.expect("schema_migrations version retry"))
}

/// `CREATE TABLE IF NOT EXISTS schema_migrations`.
async fn ensure_schema_migrations(db: &DatabaseConnection, backend: DbBackend) -> Result<()> {
    let mut delay_ms = 20u64;
    let mut last_err = None;
    for attempt in 0..8 {
        match exec_sql(
            db,
            backend,
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)",
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) if attempt + 1 < 8 && is_schema_retryable(&err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = delay_ms.saturating_mul(2).min(250);
            }
            Err(err) => return Err(err),
        }
    }
    match exec_sql(
        db,
        backend,
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)",
    )
    .await
    {
        Ok(()) => Ok(()),
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
    backend: DbBackend,
    timing: &str,
    operation_id: &str,
    stmts: Vec<String>,
) -> Result<()> {
    if stmts.is_empty() {
        return Ok(());
    }
    let stmts = bookclerk_db_exec::expand_host_schema_batch(backend, &stmts).unwrap_or(stmts);
    let plan = DbAtomicPlan {
        statements: stmts
            .into_iter()
            .map(|sql| DbPlanStatement::new(sql, Vec::new(), DbPlanStatementKind::Execute))
            .collect(),
        outcome_index: 0,
        payload_index: None,
        prior_receipt_index: None,
        receipt_select_index: None,
    };
    execute_statements_on(db, &plan, operation_id, timing, 0).await?;
    Ok(())
}

/// Loads `schema_migrations.version` rows.
async fn schema_versions_applied(
    db: &DatabaseConnection,
    backend: DbBackend,
) -> Result<HashSet<i64>> {
    let rows = db
        .query_all_raw(Statement::from_string(
            backend,
            "SELECT version FROM schema_migrations",
        ))
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            row.try_get::<i64>("", "version")
                .ok()
                .or_else(|| row.try_get_by_index::<i64>(0).ok())
                .or_else(|| row.try_get::<i32>("", "version").ok().map(i64::from))
                .or_else(|| row.try_get_by_index::<i32>(0).ok().map(i64::from))
        })
        .collect())
}

/// Executes one SQL string.
async fn exec_sql(db: &DatabaseConnection, backend: DbBackend, sql: &str) -> Result<()> {
    db.execute_raw(Statement::from_string(backend, sql.to_string()))
        .await
        .map_err(LibraryError::from_db_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::DbCapabilities;

    #[test]
    fn schema_retryable_covers_conflict_and_unavailable() {
        assert!(is_schema_retryable(&LibraryError::Unavailable(
            "SQLITE_BUSY".into()
        )));
        assert!(is_schema_retryable(&LibraryError::Conflict(
            "duplicate key".into()
        )));
        assert!(!is_schema_retryable(&LibraryError::Orm(
            sea_orm::DbErr::Custom("syntax error".into())
        )));
    }

    #[test]
    fn schema_batch_keeps_version_marker_last() {
        let batch = SchemaBatch::with_pragma_marker("CREATE TABLE t (id INTEGER)", 3);
        assert_eq!(batch.statements.len(), 2);
        assert_eq!(batch.statements.last().unwrap(), "PRAGMA user_version = 3");
        let rows = SchemaBatch::with_row_marker("CREATE TABLE t (id INTEGER)", 2);
        assert_eq!(
            rows.statements.last().unwrap(),
            "INSERT INTO schema_migrations (version) VALUES (2)"
        );
    }

    #[test]
    fn host_migration_plan_starts_with_greenfield_baseline() {
        use crate::migrations::{
            greenfield_baseline_canonical, host_migration_plan, migration_sql,
        };
        let plan = host_migration_plan();
        assert_eq!(plan[0].version, 1);
        assert_eq!(plan[0].canonical, greenfield_baseline_canonical());
        assert!(plan[0].canonical.contains(migration_sql()[0]));
        // Versions are contiguous starting at 1.
        for (i, step) in plan.iter().enumerate() {
            assert_eq!(step.version, i as i64 + 1);
        }
    }

    #[test]
    fn from_db_capabilities_selects_kind_from_flags_not_identity() {
        assert_eq!(
            HostSchemaKind::from_db_capabilities(&DbCapabilities::advertised_sqlite()).unwrap(),
            HostSchemaKind::PragmaMarker
        );
        assert_eq!(
            HostSchemaKind::from_db_capabilities(&DbCapabilities::advertised_postgres()).unwrap(),
            HostSchemaKind::RowMarker
        );
        assert_eq!(
            HostSchemaKind::from_db_capabilities(&DbCapabilities::advertised_d1()).unwrap(),
            HostSchemaKind::AtomicBatchMarker
        );

        let mut mixed = DbCapabilities::advertised_sqlite();
        mixed.schema_migrations = true;
        assert!(HostSchemaKind::from_db_capabilities(&mixed).is_err());
        let mut none = DbCapabilities::advertised_sqlite();
        none.pragma_user_version = false;
        assert!(HostSchemaKind::from_db_capabilities(&none).is_err());
        assert!(HostSchemaKind::PragmaMarker
            .advertised_db_capabilities_match(&none)
            .is_err());
    }

    #[tokio::test]
    async fn schema_migrations_on_sqlite_uses_canonical_sql_not_postgres_pack() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        apply_host_schema(&db, HostSchemaKind::RowMarker)
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
            "canonical SQLITE_SCHEMA must create books"
        );
    }

    #[tokio::test]
    async fn atomic_batch_marker_applies_canonical_plan() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        let db_batch = db.clone();
        apply_host_schema_with_batch(&db, HostSchemaKind::AtomicBatchMarker, move |stmts| {
            let db_batch = db_batch.clone();
            async move {
                run_atomic_ddl(
                    &db_batch,
                    db_batch.get_database_backend(),
                    "sqlite_txn",
                    "atomic-batch",
                    stmts,
                )
                .await
            }
        })
        .await
        .expect("atomic batch schema");
        let plan = host_migration_plan();
        let applied = schema_versions_applied(&db, DbBackend::Sqlite)
            .await
            .unwrap();
        let last = plan.last().map(|s| s.version).unwrap_or(0);
        assert!(
            applied.contains(&last),
            "last plan version {last}, applied={applied:?}"
        );
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
            "SELECT slot_key FROM db_serialization_slots LIMIT 1",
        ))
        .await
        .expect("serialization slots table");
    }

    #[tokio::test]
    async fn sqlite_crash_before_version_marker_rolls_back_and_retries() {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .expect("unmigrated sqlite");
        let ddl = bookclerk_db_exec::split_schema_statements(host_migration_plan()[0].canonical)
            .len() as u32;
        crate::inject_atomic_interrupt_after(
            crate::AtomicInterruptPhase::BetweenStatements,
            crate::AtomicInterruptKind::Cancel,
            ddl,
        );
        let err = apply_host_schema(&db, HostSchemaKind::PragmaMarker)
            .await
            .expect_err("interrupt before version marker");
        assert!(err.to_string().to_lowercase().contains("cancel"), "{err}");
        assert_eq!(sqlite_user_version(&db).await.unwrap(), 0);
        apply_host_schema(&db, HostSchemaKind::PragmaMarker)
            .await
            .expect("retry after crash");
        assert!(sqlite_user_version(&db).await.unwrap() > 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_concurrent_apply_host_schema_both_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        let db1 = bookclerk_plugin_database_sqlite::open(&path)
            .await
            .expect("open 1");
        let db2 = bookclerk_plugin_database_sqlite::open(&path)
            .await
            .expect("open 2");
        let (a, b) = tokio::join!(
            apply_host_schema(&db1, HostSchemaKind::PragmaMarker),
            apply_host_schema(&db2, HostSchemaKind::PragmaMarker),
        );
        a.expect("first apply");
        b.expect("second apply");
        assert!(sqlite_user_version(&db1).await.unwrap() > 0);
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
        let canonical = host_migration_plan()[0].canonical;
        let ddl = bookclerk_db_exec::expand_host_schema_batch(
            DbBackend::Postgres,
            &[
                canonical.to_string(),
                "INSERT INTO schema_migrations (version) VALUES (1)".to_string(),
            ],
        )
        .expect("postgres schema batch")
        .len()
        .saturating_sub(1) as u32;
        crate::inject_atomic_interrupt_after(
            crate::AtomicInterruptPhase::BetweenStatements,
            crate::AtomicInterruptKind::Cancel,
            ddl,
        );
        let err = apply_host_schema(&db, HostSchemaKind::RowMarker)
            .await
            .expect_err("interrupt");
        assert!(err.to_string().to_lowercase().contains("cancel"), "{err}");
        apply_host_schema(&db, HostSchemaKind::RowMarker)
            .await
            .expect("retry");
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
        let (a, b) = tokio::join!(
            apply_host_schema(&db1, HostSchemaKind::RowMarker),
            apply_host_schema(&db2, HostSchemaKind::RowMarker),
        );
        a.expect("first apply");
        b.expect("second apply");
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
}
