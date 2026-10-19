//! Host-owned schema application after a database guest connects.
//!
//! The host reads [`crate::SchemaState`] (`Uninitialized` / `Unreleased` /
//! `Frozen`) and applies remaining canonical schema as atomic units (each
//! frozen plan step, then the unreleased pack). A frozen database newer than
//! this binary fails closed. Versions live in `bookclerk_schema_migrations` rows.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, QueryResult};
#[cfg(test)]
use sea_orm::{DbBackend, Statement};

use crate::backup::{backup_library, BackupReason, BackupRequest, SchemaBackupOpts};
use crate::control_plane::{
    acquire_lease_on, load_lease_on, release_lease_on, renew_lease_on, ClusterLease,
    MIGRATION_LEASE,
};
use crate::error::{LibraryError, Result};
#[cfg(test)]
use crate::migrations::MigrationOp;
use crate::migrations::{
    frozen_marker_delete_sql, frozen_marker_sql, host_migration_plan, sql_string_literal,
    unreleased_checksum, unreleased_ops, unreleased_state_marker_sql, HostMigrationStep,
    BOOKCLERK_SCHEMA_NAMESPACE, CLUSTER_LEASES_DDL, SCHEMA_MIGRATIONS_DDL, SCHEMA_VERSION,
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
/// A walk that writes anything (backup or DDL) runs under the
/// [`MIGRATION_LEASE`] cluster lease, like a start that migrates: it waits
/// up to [`MIGRATION_LEASE_WAIT`] for another migrator, renews the lease
/// while it works, and checks it before every DDL batch. A no-op walk only
/// checks the database and takes no lease.
///
/// # Errors
///
/// Returns [`LibraryError`] when the walk cannot start, a backup fails, a
/// batch fails, another migrator holds the lease past the wait, or the lease
/// is lost mid-walk.
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
    ensure_schema_migrations(db).await?;
    let state = current_schema_state(db).await?;
    let planned =
        crate::schema_walk::plan_schema_walk_from_state(&host_migration_plan(), &state, target);
    if planned.map_or(true, |walk| walk.is_noop()) {
        // Nothing to write (or a walk that cannot start): check only, without
        // creating `cluster_leases` or taking the lease.
        let walk = prepare_schema_change(db, target, &opts).await?;
        apply_walk_batch(db, &walk, &mut run_batch).await?;
        return Ok(walk);
    }
    run_under_migration_lease(
        db,
        &mut run_batch,
        LeasedMigration::WalkTo {
            target,
            opts: &opts,
        },
    )
    .await?
    .ok_or_else(|| {
        LibraryError::Schema("schema walk under the migration lease returned no walk".into())
    })
}

/// How long a schema migration lease lasts without renewal. The migrator
/// renews it every third of this while it works.
pub const MIGRATION_LEASE_TTL: Duration = Duration::from_secs(120);

/// How long a starter waits for another starter's migration before it fails
/// closed. Longer than [`MIGRATION_LEASE_TTL`], so a migrator that died is
/// replaced by a waiter rather than failing every waiter.
pub const MIGRATION_LEASE_WAIT: Duration = Duration::from_secs(300);

/// True when `state` needs DDL from this binary (as opposed to a check that
/// passes or fails without writing).
fn schema_needs_migration(state: &SchemaState) -> bool {
    match state {
        SchemaState::Uninitialized => true,
        SchemaState::Unreleased { .. } => false,
        SchemaState::Frozen { version, .. } => {
            let max_plan = host_migration_plan()
                .iter()
                .map(|s| s.version)
                .max()
                .unwrap_or(0);
            // A newer frozen database is refused by the check, not migrated.
            *version <= max_plan && (*version < max_plan || !unreleased_ops().is_empty())
        }
    }
}

/// True when `state` is exactly the schema this binary applies: a start
/// against it neither migrates nor fails closed.
#[must_use]
pub fn schema_state_compatible(state: &SchemaState) -> bool {
    match state {
        SchemaState::Uninitialized => false,
        SchemaState::Unreleased {
            base_version,
            checksum,
        } => *base_version == SCHEMA_VERSION && *checksum == unreleased_checksum(),
        SchemaState::Frozen { version, .. } => {
            unreleased_ops().is_empty()
                && host_migration_plan().iter().map(|s| s.version).max() == Some(*version)
        }
    }
}

/// A holder id unique to this migration attempt: the process plus a nonce.
fn migration_holder() -> String {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!("pid{}-{}", std::process::id(), &nonce[..12])
}

/// Reconciles durable [`SchemaState`] with this binary's current canonical schema.
///
/// A database that already matches is only checked. One that needs DDL is
/// migrated under the [`MIGRATION_LEASE`] cluster lease, so concurrent
/// starters against one database produce exactly one migrator: the others
/// poll until the schema matches (then check it like any start), take the
/// lease over if the migrator's lease expires, and fail closed after
/// [`MIGRATION_LEASE_WAIT`]. Every DDL batch is fenced on the lease.
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
    if !schema_needs_migration(&state) {
        return reconcile_state_once(db, opts, run_batch, state).await;
    }
    run_under_migration_lease(db, run_batch, LeasedMigration::Reconcile(opts))
        .await
        .map(|_| ())
}

/// What a holder of the [`MIGRATION_LEASE`] runs.
#[derive(Debug, Clone, Copy)]
enum LeasedMigration<'a> {
    /// Reconcile with this binary's schema (a start).
    Reconcile(&'a SchemaApplyOptions),
    /// Walk toward `target` (`bookclerk db migrate`).
    WalkTo {
        /// Frozen revision to walk to.
        target: i64,
        /// Backup options for the walk.
        opts: &'a SchemaApplyOptions,
    },
}

/// Creates `cluster_leases`, takes the [`MIGRATION_LEASE`] (waiting up to
/// [`MIGRATION_LEASE_WAIT`] for another migrator), runs `what` under it, and
/// releases it. Returns the walk for [`LeasedMigration::WalkTo`].
///
/// A start that waits stops waiting once the schema matches and checks it
/// like any start; a walk keeps waiting for the lease.
async fn run_under_migration_lease<F, Fut>(
    db: &DatabaseConnection,
    run_batch: &mut F,
    what: LeasedMigration<'_>,
) -> Result<Option<SchemaWalk>>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    // The lease table is part of the pack being applied; create it first.
    // Concurrent `CREATE TABLE IF NOT EXISTS` can still collide on
    // PostgreSQL's catalog; the loser retries and finds the table. An
    // ambiguous failure (D1 lost the commit reply) is settled by reading the
    // table: if it is there, the statement committed.
    let mut attempt = 0;
    loop {
        match run_batch(vec![CLUSTER_LEASES_DDL.to_string()]).await {
            Ok(()) => break,
            Err(_) if load_lease_on(db, MIGRATION_LEASE).await.is_ok() => break,
            Err(err) if err.is_schema_apply_retryable() => {
                attempt += 1;
                if attempt >= 8 {
                    return Err(err);
                }
                tokio::time::sleep(Duration::from_millis(20 * attempt)).await;
            }
            Err(err) => return Err(err),
        }
    }
    let holder = migration_holder();
    let deadline = tokio::time::Instant::now() + migration_lease_wait();
    let mut delay = Duration::from_millis(50);
    loop {
        if let Some(lease) =
            acquire_lease_on(db, MIGRATION_LEASE, &holder, migration_lease_ttl()).await?
        {
            let result = migrate_under_lease(db, run_batch, &lease, what).await;
            if let Err(err) = release_lease_on(db, &lease).await {
                tracing::warn!(error = %err, "schema migration lease release failed");
            }
            return result;
        }
        if let LeasedMigration::Reconcile(opts) = what {
            let state = current_schema_state(db).await?;
            if !schema_needs_migration(&state) {
                return reconcile_state_once(db, opts, run_batch, state)
                    .await
                    .map(|()| None);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let current = load_lease_on(db, MIGRATION_LEASE).await?;
            return Err(LibraryError::Schema(format!(
                "another starter is migrating this database ({}); gave up after {}s. \
                 Retry once it finishes",
                current.map_or_else(
                    || "lease holder unknown".to_string(),
                    |lease| format!("lease holder {} token {}", lease.holder, lease.token)
                ),
                migration_lease_wait().as_secs()
            )));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(1));
    }
}

/// Runs `what` while holding `lease`: re-reads the state (a previous holder
/// may have finished), fences each DDL batch on the lease, and renews the
/// lease in the background so a long pre-migrate backup or DDL batch does
/// not lose it. Once the renewer has given the lease up as lost, the next
/// DDL batch is refused.
async fn migrate_under_lease<F, Fut>(
    db: &DatabaseConnection,
    run_batch: &mut F,
    lease: &ClusterLease,
    what: LeasedMigration<'_>,
) -> Result<Option<SchemaWalk>>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let ttl = migration_lease_ttl();
    let lost = AtomicBool::new(false);
    let work = async {
        let lost = &lost;
        let mut fenced = |stmts: Vec<String>| {
            let fence = renew_lease_on(db, lease, ttl);
            let batch = run_batch(stmts);
            async move {
                if lost.load(Ordering::SeqCst) || fence.await?.is_none() {
                    return Err(LibraryError::Schema(format!(
                        "lost the schema migration lease (token {}) before a DDL batch; \
                         another starter may have taken over",
                        lease.token
                    )));
                }
                batch.await
            }
        };
        match what {
            LeasedMigration::Reconcile(opts) => {
                let state = current_schema_state(db).await?;
                reconcile_state_once(db, opts, &mut fenced, state).await?;
                Ok(None)
            }
            LeasedMigration::WalkTo { target, opts } => {
                let walk = prepare_schema_change(db, target, opts).await?;
                apply_walk_batch(db, &walk, &mut fenced).await?;
                Ok(Some(walk))
            }
        }
    };
    let renew = async {
        keep_migration_lease(|| renew_lease_on(db, lease, ttl), ttl, &lost).await;
        // The lease is gone: stop renewing; the fence refuses the next batch.
        std::future::pending::<()>().await;
    };
    tokio::select! {
        result = work => result,
        () = renew => unreachable!("renewal never completes"),
    }
}

/// Renews a migration lease every third of `ttl` until it is lost, then sets
/// `lost` and returns.
///
/// A renewal error is retried on the next tick: the lease is still ours
/// until it expires. The lease counts as lost once a renewal finds it taken
/// or expired, or once renewals have failed for a full `ttl` since the last
/// one that succeeded (so it may have expired and been taken over).
async fn keep_migration_lease<R, RFut>(mut renew: R, ttl: Duration, lost: &AtomicBool)
where
    R: FnMut() -> RFut,
    RFut: Future<Output = Result<Option<ClusterLease>>>,
{
    let mut renewed_at = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(ttl / 3).await;
        let attempt = tokio::time::Instant::now();
        match renew().await {
            Ok(Some(_)) => renewed_at = attempt,
            Ok(None) => {
                tracing::warn!("schema migration lease lost; refusing further DDL");
                break;
            }
            Err(err) if renewed_at.elapsed() >= ttl => {
                tracing::warn!(
                    error = %err,
                    "schema migration lease not renewed for a full TTL; refusing further DDL"
                );
                break;
            }
            Err(err) => {
                tracing::warn!(error = %err, "schema migration lease renewal failed; retrying");
            }
        }
    }
    lost.store(true, Ordering::SeqCst);
}

/// The migration lease TTL ([`MIGRATION_LEASE_TTL`]).
#[cfg(not(test))]
fn migration_lease_ttl() -> Duration {
    MIGRATION_LEASE_TTL
}

/// The bounded wait for another migrator ([`MIGRATION_LEASE_WAIT`]).
#[cfg(not(test))]
fn migration_lease_wait() -> Duration {
    MIGRATION_LEASE_WAIT
}

#[cfg(test)]
thread_local! {
    /// Test override of `(ttl, wait)` for the migration lease.
    static MIGRATION_LEASE_TIMING: std::cell::Cell<Option<(Duration, Duration)>> =
        const { std::cell::Cell::new(None) };
}

/// The migration lease TTL ([`MIGRATION_LEASE_TTL`]).
#[cfg(test)]
fn migration_lease_ttl() -> Duration {
    MIGRATION_LEASE_TIMING
        .with(std::cell::Cell::get)
        .map_or(MIGRATION_LEASE_TTL, |(ttl, _)| ttl)
}

/// The bounded wait for another migrator ([`MIGRATION_LEASE_WAIT`]).
#[cfg(test)]
fn migration_lease_wait() -> Duration {
    MIGRATION_LEASE_TIMING
        .with(std::cell::Cell::get)
        .map_or(MIGRATION_LEASE_WAIT, |(_, wait)| wait)
}

/// Checks `state` against this binary and applies what it is missing.
async fn reconcile_state_once<F, Fut>(
    db: &DatabaseConnection,
    opts: &SchemaApplyOptions,
    run_batch: &mut F,
    state: SchemaState,
) -> Result<()>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
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
        assert!(current_canonical_schema().contains("adapter_plugin_key"));
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
            .expect("lower")
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
    async fn postgres_concurrent_apply_host_schema_has_one_migrator() {
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
        let packs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (a, b) = tokio::join!(
            counted_apply(&db1, packs.clone()),
            counted_apply(&db2, packs.clone()),
        );
        a.expect("first apply");
        b.expect("second apply");
        // The migration lease admits exactly one migrator; the other waits.
        assert_eq!(packs.load(std::sync::atomic::Ordering::SeqCst), 1);
        let lease = load_lease_on(&db1, MIGRATION_LEASE).await.unwrap().unwrap();
        assert_eq!(lease.expires_at_ms, 0, "released: {lease:?}");
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
            "INSERT INTO books (uuid, plugin_key, source, account_id, product_id, marketplace, title, created_at, updated_at) \
             VALUES ('u', 'audible', 'audible', 'missing', 'p', 'us', 't', 't', 't')",
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

    /// Applies the host schema on `db`, counting full-pack batches in `packs`.
    async fn counted_apply(
        db: &DatabaseConnection,
        packs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<()> {
        let exec = db.clone();
        apply_host_schema_with_batch(db, move |stmts| {
            let exec = exec.clone();
            let packs = packs.clone();
            async move {
                if stmts.len() > 1 {
                    packs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                run_atomic_ddl(&exec, "schema-apply", stmts).await
            }
        })
        .await
    }

    async fn two_file_connections(
        dir: &std::path::Path,
    ) -> (DatabaseConnection, DatabaseConnection) {
        let path = dir.join("library.db");
        let a = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
        let b = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
        (a, b)
    }

    #[tokio::test]
    async fn concurrent_starters_produce_exactly_one_migrator() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = two_file_connections(dir.path()).await;
        let packs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (left, right) = tokio::join!(
            counted_apply(&a, packs.clone()),
            counted_apply(&b, packs.clone())
        );
        left.expect("starter a");
        right.expect("starter b");
        assert_eq!(packs.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            current_schema_state(&b).await.unwrap(),
            SchemaState::Unreleased { .. }
        ));
        // The migrator released its lease; the row records who it was.
        let lease = load_lease_on(&a, MIGRATION_LEASE).await.unwrap().unwrap();
        assert!(lease.holder.starts_with("pid"), "{lease:?}");
        assert_eq!(lease.expires_at_ms, 0);
        // A matching database is only checked: no lease, no batch.
        counted_apply(&a, packs.clone()).await.unwrap();
        assert_eq!(packs.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            load_lease_on(&a, MIGRATION_LEASE).await.unwrap().unwrap(),
            lease
        );
    }

    #[tokio::test]
    async fn a_waiting_starter_takes_over_a_dead_migrators_lease() {
        MIGRATION_LEASE_TIMING.with(|t| {
            t.set(Some((Duration::from_millis(200), Duration::from_secs(5))));
        });
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = two_file_connections(dir.path()).await;
        ensure_schema_migrations(&a).await.unwrap();
        run_atomic_ddl(&a, "lease-table", vec![CLUSTER_LEASES_DDL.to_string()])
            .await
            .unwrap();
        // A starter took the lease, then died before applying anything.
        acquire_lease_on(&a, MIGRATION_LEASE, "dead", Duration::from_millis(200))
            .await
            .unwrap()
            .unwrap();
        let packs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        counted_apply(&b, packs.clone()).await.expect("takeover");
        assert_eq!(packs.load(std::sync::atomic::Ordering::SeqCst), 1);
        let lease = load_lease_on(&a, MIGRATION_LEASE).await.unwrap().unwrap();
        assert_eq!(lease.token, 2, "{lease:?}");
        assert_ne!(lease.holder, "dead");
        MIGRATION_LEASE_TIMING.with(|t| t.set(None));
    }

    #[tokio::test]
    async fn a_starter_fails_closed_while_another_holds_the_migration_lease() {
        MIGRATION_LEASE_TIMING.with(|t| {
            t.set(Some((Duration::from_secs(60), Duration::from_millis(300))));
        });
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = two_file_connections(dir.path()).await;
        ensure_schema_migrations(&a).await.unwrap();
        run_atomic_ddl(&a, "lease-table", vec![CLUSTER_LEASES_DDL.to_string()])
            .await
            .unwrap();
        acquire_lease_on(&a, MIGRATION_LEASE, "busy", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        let packs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let err = counted_apply(&b, packs.clone())
            .await
            .expect_err("bounded wait");
        assert!(
            err.to_string().contains("another starter is migrating"),
            "{err}"
        );
        assert!(err.to_string().contains("busy"), "{err}");
        assert_eq!(packs.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            current_schema_state(&b).await.unwrap(),
            SchemaState::Uninitialized
        );
        MIGRATION_LEASE_TIMING.with(|t| t.set(None));
    }

    #[tokio::test]
    async fn a_migrator_that_lost_its_lease_runs_no_ddl() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = two_file_connections(dir.path()).await;
        ensure_schema_migrations(&a).await.unwrap();
        run_atomic_ddl(&a, "lease-table", vec![CLUSTER_LEASES_DDL.to_string()])
            .await
            .unwrap();
        let stale = acquire_lease_on(&a, MIGRATION_LEASE, "stale", Duration::from_millis(50))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        acquire_lease_on(&b, MIGRATION_LEASE, "new", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        let packs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = packs.clone();
        let exec = a.clone();
        let mut run = move |stmts: Vec<String>| {
            let counter = counter.clone();
            let exec = exec.clone();
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                run_atomic_ddl(&exec, "schema-apply", stmts).await
            }
        };
        let opts = SchemaApplyOptions::default();
        let err = migrate_under_lease(&a, &mut run, &stale, LeasedMigration::Reconcile(&opts))
            .await
            .expect_err("fenced");
        assert!(
            err.to_string().contains("lost the schema migration lease"),
            "{err}"
        );
        assert_eq!(packs.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            current_schema_state(&a).await.unwrap(),
            SchemaState::Uninitialized
        );
    }

    #[tokio::test]
    async fn db_migrate_runs_under_the_migration_lease() {
        static PLAN: &[HostMigrationStep] = &[HostMigrationStep {
            version: 1,
            steps: &[MigrationOp::Schema(
                "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
            )],
            down: None,
            introduced_in: "0.0.0",
        }];
        let _plan = crate::migrations::override_host_migration_plan(PLAN);
        MIGRATION_LEASE_TIMING.with(|t| {
            t.set(Some((Duration::from_secs(60), Duration::from_millis(300))));
        });
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = two_file_connections(dir.path()).await;
        ensure_schema_migrations(&a).await.unwrap();
        run_atomic_ddl(&a, "lease-table", vec![CLUSTER_LEASES_DDL.to_string()])
            .await
            .unwrap();
        let busy = acquire_lease_on(&a, MIGRATION_LEASE, "busy", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        let batches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let run = |db: &DatabaseConnection| {
            let counter = batches.clone();
            let exec = db.clone();
            move |stmts: Vec<String>| {
                let counter = counter.clone();
                let exec = exec.clone();
                async move {
                    // `CREATE TABLE IF NOT EXISTS cluster_leases` runs before
                    // the lease; count only the walk's own batches.
                    if stmts != [CLUSTER_LEASES_DDL] {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    run_atomic_ddl(&exec, "schema-migrate", stmts).await
                }
            }
        };
        // Another migrator holds the lease: the walk waits, then fails closed
        // without a batch.
        let err = migrate_host_schema_to_with_batch(&b, 1, SchemaApplyOptions::default(), run(&b))
            .await
            .expect_err("lease held elsewhere");
        assert!(
            err.to_string().contains("another starter is migrating"),
            "{err}"
        );
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            current_schema_state(&b).await.unwrap(),
            SchemaState::Uninitialized
        );
        // Once it is free, the walk takes the lease, applies, and releases it.
        release_lease_on(&a, &busy).await.unwrap();
        let walk = migrate_host_schema_to_with_batch(&b, 1, SchemaApplyOptions::default(), run(&b))
            .await
            .expect("walk");
        assert_eq!(walk.stopped_at, 1);
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            current_schema_state(&a).await.unwrap(),
            SchemaState::Frozen { version: 1, .. }
        ));
        let lease = load_lease_on(&a, MIGRATION_LEASE).await.unwrap().unwrap();
        assert!(lease.holder.starts_with("pid"), "{lease:?}");
        assert_eq!((lease.token, lease.expires_at_ms), (2, 0));
        MIGRATION_LEASE_TIMING.with(|t| t.set(None));
    }

    #[tokio::test]
    async fn the_lease_renewer_retries_errors_and_stops_only_once_the_lease_is_lost() {
        let ttl = Duration::from_millis(300);
        let lease = ClusterLease {
            name: MIGRATION_LEASE.into(),
            holder: "h".into(),
            token: 1,
            expires_at_ms: i64::MAX,
        };
        let transient = || Err(LibraryError::Other(anyhow::anyhow!("database busy")));
        // Transient errors are retried; a renewal that finds the lease taken
        // stops the renewer.
        let mut script = vec![Ok(None), Ok(Some(lease.clone())), transient(), transient()];
        let calls = std::cell::Cell::new(0_usize);
        let lost = AtomicBool::new(false);
        keep_migration_lease(
            || {
                calls.set(calls.get() + 1);
                std::future::ready(script.pop().expect("renewed after the lease was lost"))
            },
            ttl,
            &lost,
        )
        .await;
        assert_eq!(calls.get(), 4);
        assert!(lost.load(Ordering::SeqCst));
        // Renewals that keep failing for a full TTL count as lost.
        let calls = std::cell::Cell::new(0_usize);
        let lost = AtomicBool::new(false);
        keep_migration_lease(
            || {
                calls.set(calls.get() + 1);
                std::future::ready(transient())
            },
            ttl,
            &lost,
        )
        .await;
        assert!(lost.load(Ordering::SeqCst));
        assert!((2..=4).contains(&calls.get()), "{}", calls.get());
    }
}
