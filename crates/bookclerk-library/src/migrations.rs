//! Host library schema: frozen plan steps plus the unreleased development pack.
//!
//! Bookclerk has **not** frozen a production v1 schema. [`host_migration_plan`]
//! is empty until a release cut. Live schema lives in [`unreleased_ops`] (already
//! separated [`MigrationOp`]s). Fresh databases apply those ops (frozen ups +
//! unreleased) and persist [`crate::SchemaState::Unreleased`] with `base_version`
//! equal to [`SCHEMA_VERSION`] (`0` today: no frozen revisions). Fresh init
//! records each frozen plan step's checksum before the unreleased marker; when
//! the unreleased bucket is empty the database ends
//! [`crate::SchemaState::Frozen`]. Joined SQL ([`unreleased_sql`],
//! [`current_canonical_schema`]) is derived for diagnostics/export/tests.
//! Adapters lower canonical DDL at the execution edge
//! ([`bookclerk_db_exec::expand_host_schema_batch`]).

use bookclerk_plugin_abi::{
    apply_schema_sql_to_env, canonical_statements_checksum, sql_v1_pack_statements,
    statement_is_ddl, typecheck_execute_request, DbPlanStatementKind, DbResultSelection,
    ExecuteRequest, SqlTypeEnv, TypedDbStatement,
};

use std::sync::OnceLock;

use crate::error::{LibraryError, Result};

#[cfg(test)]
use std::cell::Cell;

/// Final SQLite DDL for a fresh Bookclerk library database.
///
/// SeaORM entities in [`crate::entities`] mirror these columns exactly. All
/// integer columns map to `i64`, reals to `f64`, blobs to `Vec<u8>`, and text
/// (including RFC 3339 timestamps) to `String`.
#[must_use]
pub fn latest_schema_sqlite() -> &'static str {
    current_canonical_schema()
}

/// Highest **frozen** schema version this binary knows (`0` while the plan is empty).
///
/// This is not a discriminator for uninitialized vs unreleased. Use
/// [`crate::SchemaState`].
pub const SCHEMA_VERSION: i64 = 0;

/// Live development DDL derived from [`unreleased_ops`] (joined with `;\n`).
///
/// Apply, checksum, and backup use the op list. This script is diagnostics,
/// export, and `execute_batch` tests only.
#[must_use]
pub fn unreleased_sql() -> &'static str {
    static SQL: OnceLock<String> = OnceLock::new();
    SQL.get_or_init(|| join_op_sql(unreleased_ops())).as_str()
}

/// Host bookkeeping table created before applying plan versions.
///
/// `namespace` separates Bookclerk-owned ledger rows (`bookclerk`) from any
/// leftover rows. Plugin-owned history uses the separate `bookclerk_plugin_migrations`
/// journal, not this table.
pub const SCHEMA_MIGRATIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS bookclerk_schema_migrations (
        namespace TEXT NOT NULL,
        version INTEGER NOT NULL,
        state TEXT NOT NULL,
        checksum TEXT NOT NULL,
        app_version TEXT NOT NULL,
        applied_at TEXT NOT NULL,
        PRIMARY KEY (namespace, state, version)
    )";

/// One admitted BookclerkSQL statement in a host migration apply unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOp {
    /// Schema mutation (`CREATE` / `DROP` / `ALTER` / index).
    Schema(&'static str),
    /// Data backfill (`INSERT` / `UPDATE` / `DELETE`).
    Data(&'static str),
}

impl MigrationOp {
    /// Canonical BookclerkSQL for this op.
    #[must_use]
    pub const fn sql(self) -> &'static str {
        match self {
            Self::Schema(sql) | Self::Data(sql) => sql,
        }
    }

    /// True when this op is admitted schema DDL.
    #[must_use]
    pub const fn is_schema(self) -> bool {
        matches!(self, Self::Schema(_))
    }

    /// True when this op is admitted DML / backfill.
    #[must_use]
    pub const fn is_data(self) -> bool {
        matches!(self, Self::Data(_))
    }
}

mod binding_ops;
mod engine;
mod plan;
mod plugin;
mod unreleased_ops;

pub use engine::{
    apply_migration_plan, downgrade_migration_plan, remaining_upgrade_batches,
    schema_session_matches, ApplyDirection,
};
pub use plan::{
    frozen_marker_delete_sql, frozen_marker_sql, schema_slot_key, sql_string_literal,
    unreleased_marker_sql_in, validate_schema_namespace, MigrationPlan, MigrationStep, PlanOp,
    BOOKCLERK_SCHEMA_NAMESPACE,
};
pub use plugin::{
    apply_plugin_migrations, history_from_execute_reply, load_plugin_migration_history,
    load_plugin_migration_history_on, next_pending_plugin_migration, pending_plugin_suffix,
    plugin_apply_statements, plugin_binding_type_env, plugin_history_digest,
    plugin_history_session_matches, plugin_journal_has_entry, plugin_journal_select_request,
    plugin_migration_checksum, prove_plugin_migration_sequence, remaining_plugin_suffix_batches,
    require_history_prefix, PluginJournalEntry, PluginMigrationHistory, PluginMigrationSequence,
    ProvenPluginMigration, MAX_PLUGIN_MIGRATION_APPLY_ATTEMPTS, PLUGIN_MIGRATION_SLOT_KEY,
};

/// One host-owned schema version in the canonical Bookclerk migration plan.
///
/// Each version is recorded as a `bookclerk_schema_migrations` row (adapters
/// must advertise `schemaMigrations`). The live connection backend lowers
/// each [`MigrationOp`] at the adapter boundary (Postgres) or applies it
/// verbatim (SQLite / D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostMigrationStep {
    /// `bookclerk_schema_migrations.version` for this step.
    pub version: i64,
    /// Ordered schema and data ops for this version (the `up`).
    pub steps: &'static [MigrationOp],
    /// Reverse ops when this step is reversible; `None` means restore a backup.
    pub down: Option<&'static [MigrationOp]>,
    /// First Bookclerk semver that shipped this step.
    pub introduced_in: &'static str,
}

impl HostMigrationStep {
    /// SHA-256 hex digest of length-prefixed `up` (and `down` when present).
    #[must_use]
    pub fn checksum(&self) -> String {
        migration_ops_checksum(self.steps, self.down)
    }

    /// True when [`Self::down`] is present so CLI rollback can apply this step.
    #[must_use]
    pub fn reversible(&self) -> bool {
        self.down.is_some()
    }

    /// Concatenated up SQL (tests / derived views). Boundaries are the op list.
    #[must_use]
    pub fn up_sql(&self) -> String {
        self.steps
            .iter()
            .map(|op| op.sql())
            .collect::<Vec<_>>()
            .join(";\n")
    }
}

/// Oldest frozen schema version this binary can run.
///
/// Derived from [`host_migration_plan`]: `None` while the plan is empty
/// (no frozen schema versions exist). Once frozen steps exist this is the
/// first retained step, not a separately synchronized constant.
#[must_use]
pub fn min_supported_schema_version() -> Option<i64> {
    host_migration_plan().first().map(|step| step.version)
}

/// Oldest frozen version in `plan` (`None` when `plan` is empty).
#[must_use]
pub fn min_supported_schema_version_in(plan: &[HostMigrationStep]) -> Option<i64> {
    plan.first().map(|step| step.version)
}

/// SHA-256 of length-prefixed migration ops (not a joined script).
#[must_use]
pub fn migration_ops_checksum(ups: &[MigrationOp], down: Option<&[MigrationOp]>) -> String {
    let up: Vec<&str> = ups.iter().map(|op| op.sql()).collect();
    let down_sql: Option<Vec<&str>> = down.map(|ops| ops.iter().map(|op| op.sql()).collect());
    let down_refs: Option<Vec<&str>> = down_sql.as_ref().map(|v| v.to_vec());
    migration_statements_checksum(&up, down_refs.as_deref())
}

/// SHA-256 of canonical up statements, plus down statements when reversible.
///
/// # Errors
///
/// Returns when `canonical` or `down` is not a BookclerkSQL statement list.
/// Parser failure is never treated as a single opaque statement.
pub fn migration_sql_checksum(canonical: &str, down: Option<&str>) -> Result<String> {
    let ups = pack_migration_sql(canonical)?;
    let downs = match down {
        Some(sql) => Some(pack_migration_sql(sql)?),
        None => None,
    };
    let up_refs: Vec<&str> = ups.iter().map(String::as_str).collect();
    let down_owned: Option<Vec<&str>> = downs
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect());
    Ok(migration_statements_checksum(
        &up_refs,
        down_owned.as_deref(),
    ))
}

/// Packs `sql` with the SQL-v1 lexer. Empty input yields no statements.
///
/// # Errors
///
/// Returns when `sql` is not a BookclerkSQL statement list.
pub fn pack_migration_sql(sql: &str) -> Result<Vec<String>> {
    sql_v1_pack_statements(sql).map_err(|err| {
        LibraryError::Schema(format!(
            "migration SQL is not a BookclerkSQL statement list: {err}"
        ))
    })
}

/// Length-prefixed checksum of `ups`, then `-- down` and down statements.
fn migration_statements_checksum(ups: &[&str], down: Option<&[&str]>) -> String {
    let mut parts = Vec::with_capacity(ups.len().saturating_add(1));
    parts.extend(ups.iter().copied());
    if let Some(down) = down {
        parts.push("-- down");
        parts.extend(down.iter().copied());
    }
    canonical_statements_checksum(&parts)
}

/// Joins already-separated ops with `;\n` for diagnostics/export.
#[must_use]
pub fn join_op_sql(ops: &[MigrationOp]) -> String {
    ops.iter()
        .map(|op| op.sql())
        .collect::<Vec<_>>()
        .join(";\n")
}

/// Highest frozen binding-bootstrap version (`0` until a binding freeze).
pub const BINDING_SCHEMA_VERSION: i64 = 0;

/// Host-owned bootstrap ops applied inside every isolated plugin binding.
///
/// # Panics
///
/// Panics when a bootstrap op is not one admitted BookclerkSQL statement of
/// the declared [`MigrationOp`] kind.
#[must_use]
pub fn binding_bootstrap_ops() -> &'static [MigrationOp] {
    static PROVEN: OnceLock<()> = OnceLock::new();
    PROVEN.get_or_init(|| {
        prove_migration_ops(binding_ops::BINDING_BOOTSTRAP_OPS).unwrap_or_else(|err| {
            panic!("BINDING_BOOTSTRAP_OPS failed BookclerkSQL proof: {err}");
        });
    });
    binding_ops::BINDING_BOOTSTRAP_OPS
}

/// Derived diagnostic SQL for [`binding_bootstrap_ops`].
#[must_use]
pub fn binding_bootstrap_sql() -> &'static str {
    static SQL: OnceLock<String> = OnceLock::new();
    SQL.get_or_init(|| join_op_sql(binding_bootstrap_ops()))
        .as_str()
}

/// Ordered canonical statements for [`binding_bootstrap_ops`].
#[must_use]
pub fn binding_bootstrap_statements() -> &'static [String] {
    static STMTS: OnceLock<Vec<String>> = OnceLock::new();
    STMTS
        .get_or_init(|| ops_to_statements(binding_bootstrap_ops()))
        .as_slice()
}

/// Type environment implied by [`binding_bootstrap_ops`] (journal, slots, catalog).
///
/// Host-private binding bookkeeping lives here so plugin apply can typecheck
/// journal DML without using the host library catalog.
#[must_use]
pub fn binding_bootstrap_type_env() -> SqlTypeEnv {
    let mut env = SqlTypeEnv::new();
    for op in binding_bootstrap_ops() {
        if op.is_schema() {
            apply_schema_sql_to_env(&mut env, op.sql());
        }
    }
    env
}

/// SHA-256 of [`binding_bootstrap_ops`] (length-prefixed statement list).
#[must_use]
pub fn binding_unreleased_checksum() -> String {
    migration_ops_checksum(binding_bootstrap_ops(), None)
}

/// Frozen ups concatenated with [`unreleased_ops`] (derived diagnostic SQL).
///
/// While the production plan is empty this is exactly [`unreleased_sql`]. After
/// a release cut it is frozen step SQL plus whatever is again unreleased — do
/// not assume it equals [`unreleased_sql`] forever. Test plan overrides must
/// not feed this `OnceLock` (`production_host_migration_plan` only).
#[must_use]
pub fn current_canonical_schema() -> &'static str {
    if production_host_migration_plan().is_empty() {
        return unreleased_sql();
    }
    static SQL: OnceLock<String> = OnceLock::new();
    SQL.get_or_init(|| current_canonical_statements().join(";\n"))
        .as_str()
}

/// Ordered canonical statements for [`current_canonical_schema`].
#[must_use]
pub fn current_canonical_statements() -> &'static [String] {
    static STMTS: OnceLock<Vec<String>> = OnceLock::new();
    STMTS
        .get_or_init(|| {
            let mut out = Vec::new();
            for step in production_host_migration_plan() {
                for op in step.steps {
                    out.push(op.sql().to_string());
                }
            }
            out.extend(unreleased_statements().iter().cloned());
            out
        })
        .as_slice()
}

/// Live unreleased host schema ops (source of truth).
///
/// # Panics
///
/// Panics when an unreleased op is not one admitted BookclerkSQL statement of
/// the declared [`MigrationOp`] kind.
#[must_use]
pub fn unreleased_ops() -> &'static [MigrationOp] {
    static PROVEN: OnceLock<()> = OnceLock::new();
    PROVEN.get_or_init(|| {
        prove_migration_ops(unreleased_ops::UNRELEASED_OPS).unwrap_or_else(|err| {
            panic!("UNRELEASED_OPS failed BookclerkSQL proof: {err}");
        });
    });
    unreleased_ops::UNRELEASED_OPS
}

/// Ordered statements in [`unreleased_ops`].
#[must_use]
pub fn unreleased_statements() -> &'static [String] {
    static STMTS: OnceLock<Vec<String>> = OnceLock::new();
    STMTS
        .get_or_init(|| ops_to_statements(unreleased_ops()))
        .as_slice()
}

/// SHA-256 of [`unreleased_ops`] (empty string when empty).
#[must_use]
pub fn unreleased_checksum() -> String {
    migration_ops_checksum(unreleased_ops(), None)
}

/// `INSERT` for an unreleased `bookclerk_schema_migrations` row in the Bookclerk namespace.
#[must_use]
pub fn unreleased_state_marker_sql(checksum: &str, base_version: i64) -> String {
    unreleased_marker_sql_in(BOOKCLERK_SCHEMA_NAMESPACE, checksum, base_version)
}

/// Proves each op is exactly one BookclerkSQL statement of the declared kind.
///
/// Schema ops must be admitted DDL (`CREATE` / `ALTER` / `DROP`). Data ops must
/// be admitted DML. Both go through the SQL-v1 typechecker; Schema ops also
/// update `env` so later Data/index ops see prior `CREATE TABLE`.
///
/// # Errors
///
/// Returns when packing, kind, or typechecking fails.
pub fn prove_migration_ops(ops: &[MigrationOp]) -> Result<()> {
    let mut env = SqlTypeEnv::new();
    for (index, op) in ops.iter().enumerate() {
        prove_migration_op(index, *op, &mut env)?;
    }
    Ok(())
}

/// Packs `sql` and requires exactly one statement.
///
/// # Errors
///
/// Returns when packing fails or `sql` is not a single statement.
pub fn require_single_migration_statement(sql: &str) -> Result<String> {
    let packed = pack_migration_sql(sql)?;
    match packed.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(LibraryError::Schema(
            "migration op is empty after packing".into(),
        )),
        _ => Err(LibraryError::Schema(format!(
            "migration op packed into {} statements; each MigrationOp must be one statement",
            packed.len()
        ))),
    }
}

/// Proves one op: single packed statement, Schema/Data kind, SQL-v1 typecheck.
fn prove_migration_op(index: usize, op: MigrationOp, env: &mut SqlTypeEnv) -> Result<()> {
    prove_plan_sql_op(index, op.sql(), op.is_schema(), env)
}

/// Proves one op: single packed statement, Schema/Data kind, SQL-v1 typecheck.
pub(crate) fn prove_plan_sql_op(
    index: usize,
    sql: &str,
    is_schema: bool,
    env: &mut SqlTypeEnv,
) -> Result<()> {
    let packed = require_single_migration_statement(sql)
        .map_err(|err| LibraryError::Schema(format!("migration op {index}: {err}")))?;
    if packed != sql.trim() {
        return Err(LibraryError::Schema(format!(
            "migration op {index} is not already a single BookclerkSQL statement"
        )));
    }
    let is_ddl = statement_is_ddl(sql);
    if is_schema && !is_ddl {
        return Err(LibraryError::Schema(format!(
            "migration op {index} Schema variant is not admitted DDL"
        )));
    }
    if !is_schema && is_ddl {
        return Err(LibraryError::Schema(format!(
            "migration op {index} Data variant is admitted DDL"
        )));
    }
    let req = ExecuteRequest {
        operation_id: format!("prove-migration-op-{index}"),
        request_hash: String::new(),
        deadline_unix_ms: 0,
        statements: vec![TypedDbStatement {
            sql: sql.to_string(),
            parameters: Vec::new(),
            kind: DbPlanStatementKind::Execute,
            max_rows: 0,
            result_selection: DbResultSelection::Discard,
        }],
    };
    typecheck_execute_request(&req, env).map_err(|err| {
        LibraryError::Schema(format!("migration op {index} failed typecheck: {err}"))
    })?;
    if is_schema {
        apply_schema_sql_to_env(env, sql);
    }
    Ok(())
}

/// Copies each op's canonical SQL into an owned statement list.
fn ops_to_statements(ops: &[MigrationOp]) -> Vec<String> {
    ops.iter().map(|op| op.sql().to_string()).collect()
}

/// Host table names declared by [`current_canonical_schema`], plus
/// `bookclerk_schema_migrations`.
#[must_use]
pub fn current_canonical_table_names() -> Vec<String> {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES
        .get_or_init(|| {
            let mut names = table_names_from_statements(current_canonical_statements());
            if !names.iter().any(|n| n == "bookclerk_schema_migrations") {
                names.push("bookclerk_schema_migrations".into());
            }
            names
        })
        .clone()
}

/// Parses `CREATE TABLE` names from already-separated canonical statements.
fn table_names_from_statements(stmts: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    for stmt in stmts {
        let trimmed = stmt.trim();
        let lower = trimmed.to_ascii_lowercase();
        let rest = if let Some(rest) = lower.strip_prefix("create table") {
            rest.trim()
        } else {
            continue;
        };
        let rest = rest
            .strip_prefix("if not exists")
            .map(str::trim)
            .unwrap_or(rest);
        let name = rest
            .split(|c: char| c.is_whitespace() || c == '(')
            .find(|part| !part.is_empty())
            .unwrap_or("");
        if !name.is_empty() {
            names.push(name.trim_matches('"').trim_matches('`').to_string());
        }
    }
    names
}

/// Column types implied by [`current_canonical_schema`].
#[must_use]
pub fn host_sql_type_env() -> bookclerk_plugin_abi::SqlTypeEnv {
    bookclerk_plugin_abi::sql_type_env_from_canonical_statements(current_canonical_statements())
}

/// Frozen host migration steps. Empty until a release cut copies
/// [`unreleased_ops`] into version 1.
#[must_use]
pub fn host_migration_plan() -> Vec<HostMigrationStep> {
    #[cfg(test)]
    {
        if let Some(plan) = HOST_PLAN_OVERRIDE.with(Cell::get) {
            return plan.to_vec();
        }
    }
    production_host_migration_plan()
}

/// Production frozen plan (empty until a release cut). Test overrides must not
/// feed [`current_canonical_schema`]'s `OnceLock`.
fn production_host_migration_plan() -> Vec<HostMigrationStep> {
    Vec::new()
}

#[cfg(test)]
thread_local! {
    static HOST_PLAN_OVERRIDE: Cell<Option<&'static [HostMigrationStep]>> = const { Cell::new(None) };
}

/// Test-only: [`host_migration_plan`] returns `plan` until the guard drops.
#[cfg(test)]
pub(crate) struct HostPlanOverrideGuard;

#[cfg(test)]
impl Drop for HostPlanOverrideGuard {
    fn drop(&mut self) {
        HOST_PLAN_OVERRIDE.with(|cell| cell.set(None));
    }
}

/// Installs a test-only frozen plan for [`host_migration_plan`] until drop.
#[cfg(test)]
pub(crate) fn override_host_migration_plan(
    plan: &'static [HostMigrationStep],
) -> HostPlanOverrideGuard {
    HOST_PLAN_OVERRIDE.with(|cell| cell.set(Some(plan)));
    HostPlanOverrideGuard
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn apply_current_schema(conn: &Connection) {
        conn.execute_batch(current_canonical_schema()).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    }

    fn insert_domain_event(conn: &Connection, id: &str, account_id: &str, dedup_key: &str) {
        conn.execute(
            "INSERT INTO domain_events (
                id, event_type, schema_version, occurred_at, account_id, source,
                correlation_id, causation_id, dedup_key, payload, ordering_key,
                dispatch_state, created_at
            ) VALUES (
                ?1, 'book_acquired', 1, '2026-01-01T00:00:00+00:00', ?2,
                'audible', '', '', ?3, '{}', '', 'pending',
                '2026-01-01T00:00:00+00:00'
            )",
            rusqlite::params![id, account_id, dedup_key],
        )
        .unwrap();
    }

    fn insert_delivery(conn: &Connection, id: &str, event_id: &str) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO event_deliveries (
                id, event_id, plugin_id, idempotency_key, state, run_after,
                created_at, updated_at
            ) VALUES (
                ?1, ?2, 'echo', ?1, 'pending', '2026-01-01T00:00:00+00:00',
                '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'
            )",
            rusqlite::params![id, event_id],
        )
    }

    #[test]
    fn current_schema_rejects_orphan_event_deliveries_with_foreign_keys_on() {
        let conn = Connection::open_in_memory().unwrap();
        apply_current_schema(&conn);
        let orphan = insert_delivery(&conn, "evt-missing:echo", "evt-missing");
        assert!(
            orphan.is_err(),
            "event_deliveries.event_id must reference domain_events(id)"
        );
        insert_domain_event(&conn, "evt-1", "acct", "book_acquired:u1");
        insert_delivery(&conn, "evt-1:echo", "evt-1").unwrap();
        let deliveries: i64 = conn
            .query_row("SELECT COUNT(*) FROM event_deliveries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(deliveries, 1);
    }

    #[test]
    fn current_schema_domain_events_unique_is_namespaced() {
        let conn = Connection::open_in_memory().unwrap();
        apply_current_schema(&conn);
        insert_domain_event(&conn, "evt-1", "acct", "book_acquired:u1");
        insert_domain_event(&conn, "evt-2", "other", "book_acquired:u1");
        let dup = conn.execute(
            "INSERT INTO domain_events (
                id, event_type, schema_version, occurred_at, account_id, source,
                correlation_id, causation_id, dedup_key, payload, ordering_key,
                dispatch_state, created_at
            ) VALUES (
                'evt-3', 'book_acquired', 1, '2026-01-01T00:00:00+00:00', 'acct',
                'audible', '', '', 'book_acquired:u1', '{}', '', 'pending',
                '2026-01-01T00:00:00+00:00'
            )",
            [],
        );
        assert!(
            dup.is_err(),
            "UNIQUE(account_id, source, event_type, dedup_key) must hold"
        );
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM domain_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn host_migration_plan_is_empty_until_a_release_cut() {
        assert!(host_migration_plan().is_empty());
        assert_eq!(SCHEMA_VERSION, 0);
        assert_eq!(min_supported_schema_version(), None);
        assert!(!unreleased_sql().trim().is_empty());
        assert!(unreleased_sql().contains("plugin_databases"));
        assert!(unreleased_sql().contains("dispatch_snapshot_json"));
        assert!(unreleased_sql().contains("bookclerk_slots"));
        assert_eq!(current_canonical_statements(), unreleased_statements());
        assert_eq!(current_canonical_schema(), unreleased_sql());
        assert!(
            unreleased_ops()
                .iter()
                .any(|op| op.is_data() && op.sql().contains("event_outbox_stats")),
            "seed INSERT must be MigrationOp::Data"
        );
        prove_migration_ops(unreleased_ops()).expect("unreleased ops must typecheck");
        prove_migration_ops(binding_bootstrap_ops()).expect("binding ops must typecheck");
        assert_eq!(unreleased_checksum().len(), 64);
        let tables = current_canonical_table_names();
        assert!(tables.contains(&"books".into()));
        assert!(tables.contains(&"bookclerk_schema_migrations".into()));
        assert!(tables.contains(&"plugin_databases".into()));
    }

    #[test]
    fn unreleased_checksum_is_stable() {
        let a = unreleased_checksum();
        let b = unreleased_checksum();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn postgres_lowering_of_baseline_is_mechanically_complete() {
        let lowered = current_canonical_statements()
            .iter()
            .map(|stmt| bookclerk_db_exec::lower_canonical_ddl_to_postgres(stmt))
            .collect::<Vec<_>>()
            .join(";\n");
        // No SQLite-isms may survive the mechanical lowering; the CI Postgres
        // sidecar applies this exact output (`postgres_test_store`).
        for token in [
            "AUTOINCREMENT",
            " INTEGER",
            " BLOB",
            " REAL",
            "INSERT OR IGNORE",
        ] {
            assert!(
                !lowered.contains(token),
                "sqlite-ism `{token}` survived postgres lowering"
            );
        }
        assert!(lowered.contains("BIGINT PRIMARY KEY"), "identity ids");
        assert!(
            lowered.contains("ON CONFLICT DO NOTHING"),
            "insert-or-ignore"
        );
        assert!(lowered.contains(" BYTEA"), "blob columns");
        assert!(lowered.contains(" DOUBLE PRECISION"), "real columns");
        assert!(lowered.contains("UNIQUE(account_id, source, event_type, dedup_key)"));
        // Word-boundary safety: string literals stay untouched.
        assert!(lowered.contains(r#"'["openid","profile","email"]'"#));
        assert!(!lowered.contains("domain_events_v27"));
    }

    #[test]
    fn host_sql_type_env_seeds_library_tables() {
        let env = host_sql_type_env();
        assert!(
            env.has_table("accounts"),
            "expected accounts in {:?}",
            env.iter().map(|(t, _, _)| t).collect::<Vec<_>>()
        );
        assert!(
            env.column_type("portal_identities", "user_id").is_some(),
            "ALTER ADD COLUMN user_id must land in the host type env"
        );
        assert!(
            env.has_table("domain_events"),
            "rebuild RENAME must restore domain_events"
        );
        assert!(
            !env.has_table("domain_events_v27"),
            "v27 rebuild table must be renamed away"
        );
        let req = bookclerk_plugin_abi::ExecuteRequest {
            operation_id: "host-order".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![bookclerk_plugin_abi::TypedDbStatement {
                sql: "SELECT title FROM books ORDER BY title".into(),
                parameters: Vec::new(),
                kind: bookclerk_plugin_abi::DbPlanStatementKind::Select,
                max_rows: 8,
                result_selection: bookclerk_plugin_abi::DbResultSelection::Rows,
            }],
        };
        let proofs = bookclerk_plugin_abi::typecheck_execute_request_proofs(&req, &env)
            .expect("host ORDER BY TEXT must typecheck against the canonical host schema");
        assert!(
            !proofs[0].text_collate_sites.is_empty(),
            "host TEXT ORDER BY must record collate sites: {:?}",
            proofs[0]
        );
        let mut working = env.clone();
        for stmt in current_canonical_statements() {
            bookclerk_plugin_abi::apply_schema_sql_to_env(&mut working, stmt);
            if bookclerk_plugin_abi::statement_is_ddl(stmt) {
                continue;
            }
            let upper = stmt.trim().to_ascii_uppercase();
            if upper.starts_with("PRAGMA ") || upper.starts_with("ALTER ") {
                continue;
            }
            let one = bookclerk_plugin_abi::ExecuteRequest {
                operation_id: "host-ddl-dml".into(),
                request_hash: String::new(),
                deadline_unix_ms: 0,
                statements: vec![bookclerk_plugin_abi::TypedDbStatement {
                    sql: stmt.clone(),
                    parameters: Vec::new(),
                    kind: bookclerk_plugin_abi::DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: bookclerk_plugin_abi::DbResultSelection::Discard,
                }],
            };
            bookclerk_plugin_abi::typecheck_execute_request_proofs(&one, &working)
                .unwrap_or_else(|err| panic!("host schema DML failed on `{stmt}`: {err}"));
        }
    }

    #[test]
    fn schema_vs_data_is_enforced() {
        let err =
            prove_migration_ops(&[MigrationOp::Schema("INSERT INTO accounts (id) VALUES (1)")])
                .expect_err("Schema INSERT");
        assert!(err.to_string().contains("not admitted DDL"), "{err}");
        let err =
            prove_migration_ops(&[MigrationOp::Data("CREATE TABLE t (id INTEGER PRIMARY KEY)")])
                .expect_err("Data CREATE");
        assert!(err.to_string().contains("admitted DDL"), "{err}");
        let err = prove_migration_ops(&[MigrationOp::Schema(
            "CREATE TABLE t (id INTEGER PRIMARY KEY); CREATE TABLE u (id INTEGER PRIMARY KEY)",
        )])
        .expect_err("multi-statement Schema");
        assert!(
            err.to_string().contains("packed into") || err.to_string().contains("single"),
            "{err}"
        );
    }

    #[test]
    fn migration_sql_checksum_fails_closed_on_packer_error() {
        let err = migration_sql_checksum("SELECT 'unterminated", None).expect_err("unterminated");
        assert!(
            err.to_string()
                .contains("not a BookclerkSQL statement list"),
            "{err}"
        );
        let err = migration_sql_checksum("CREATE TABLE t (id INTEGER PRIMARY KEY) /* ", None)
            .expect_err("unterminated comment");
        assert!(
            err.to_string()
                .contains("not a BookclerkSQL statement list"),
            "{err}"
        );
        let err = prove_migration_ops(&[MigrationOp::Schema("CREATE TABLE t (id INTEGER /* ")])
            .expect_err("static Schema must not pack_or_single");
        assert!(
            err.to_string().contains("migration op") || err.to_string().contains("BookclerkSQL"),
            "{err}"
        );
        let ok = migration_sql_checksum("CREATE TABLE t (id INTEGER PRIMARY KEY)", None)
            .expect("one statement");
        assert_eq!(ok.len(), 64);
        let two = migration_sql_checksum(
            "CREATE TABLE t (id INTEGER PRIMARY KEY); CREATE TABLE u (id INTEGER PRIMARY KEY)",
            None,
        )
        .expect("two packed statements checksum as a list");
        assert_ne!(ok, two);
    }

    #[test]
    fn unreleased_statements_match_ops_without_repacking() {
        let from_ops: Vec<&str> = unreleased_ops().iter().map(|op| op.sql()).collect();
        let stmts: Vec<&str> = unreleased_statements().iter().map(String::as_str).collect();
        assert_eq!(from_ops, stmts);
        let binding: Vec<&str> = binding_bootstrap_ops().iter().map(|op| op.sql()).collect();
        let binding_stmts: Vec<&str> = binding_bootstrap_statements()
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(binding, binding_stmts);
    }
}
