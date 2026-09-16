//! Typed Cap'n database data-plane (`ExecuteRequest` / `ExecuteReply`) and
//! control-plane (`DbCapabilities`) mirrors of `plugin.capnp`.
//!
//! Hosts call `AdapterDatabaseSession.capabilities` and
//! `AdapterDatabaseSession.execute`. Granted job SQL uses `GuestDatabase.execute`.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::db_value::{DbType, DbValue};
use crate::sql_overflow::{apply_integer_overflow, OverflowDialect};
use crate::sql_proof::ResolvedStatement;
use crate::sql_text::{
    require_function_args_within, require_like_patterns_within, require_portable_text,
    require_portable_text_binds, D1_PORTABLE_LIKE_PATTERN_BYTES,
};
use crate::sql_types::SqlTypeEnv;
use crate::MAX_SCALAR_BYTES;

/// SQLite family bind cap advertised by the platform sqlite guest.
pub const SQLITE_MAX_BINDS: u32 = 32_766;

/// PostgreSQL bind cap advertised by the optional postgres guest.
pub const POSTGRES_MAX_BINDS: u32 = 65_535;

/// Cloudflare D1 bound-parameter limit.
///
/// <https://developers.cloudflare.com/d1/platform/limits/>
pub const D1_MAX_BINDS: u32 = 100;

/// Cloudflare D1 physical SQL statement length (bytes) after adapter lowering.
pub const D1_MAX_SQL_STATEMENT_BYTES: u32 = 100_000;

/// Cloudflare D1 maximum arguments to one physical SQL function.
pub const D1_MAX_FUNCTION_ARGS: u32 = 32;

/// SQLite default `SQLITE_MAX_FUNCTION_ARG`.
pub const SQLITE_MAX_FUNCTION_ARGS: u32 = 127;

/// PostgreSQL maximum function arguments.
pub const POSTGRES_MAX_FUNCTION_ARGS: u32 = 100;

/// Cloudflare D1 maximum columns per table.
pub const D1_MAX_SCHEMA_COLUMNS: u32 = 100;

/// SQLite default `SQLITE_MAX_COLUMN`.
pub const SQLITE_MAX_SCHEMA_COLUMNS: u32 = 2_000;

/// PostgreSQL maximum columns per table.
pub const POSTGRES_MAX_SCHEMA_COLUMNS: u32 = 1_600;

/// SQLite default `SQLITE_MAX_LIKE_PATTERN_LENGTH`.
pub const SQLITE_MAX_PATTERN_BYTES: u32 = 50_000;

/// Host refuses guests that cannot accept at least this many function args.
pub const HOST_MIN_FUNCTION_ARGS: u32 = 8;

/// Host refuses guests whose schema-column cap is below the library schema.
///
/// The widest host table (`books`) currently has 41 columns.
pub const HOST_MIN_SCHEMA_COLUMNS: u32 = 64;

/// Host refuses guests that cannot prove a LIKE pattern of this many bytes.
pub const HOST_MIN_PATTERN_BYTES: u32 = 8;

/// D1 / first-party batch statement cap (D1 HTTP batch is 100 queries).
pub const FIRST_PARTY_MAX_STATEMENTS: u32 = 100;

/// First-party row cap advertised for one query statement.
pub const FIRST_PARTY_MAX_RESULT_ROWS: u32 = 1_000;

/// Host refuses guests that cannot bind at least this many parameters.
pub const HOST_MIN_BINDS: u32 = 32;

/// Host refuses guests that cannot run at least this many statements per batch.
pub const HOST_MIN_STATEMENTS: u32 = 40;

/// Host refuses guests that do not bound result rows (`0` is unspecified).
pub const HOST_MIN_RESULT_ROWS: u32 = 1;

/// Host refuses guests that do not bound encoded statement payload bytes.
pub const HOST_MIN_PAYLOAD_BYTES: u32 = 1024;

/// `?` → `unhex(?)` for D1 BLOB binds.
const D1_UNHEX_PLACEHOLDER_EXTRA: usize = 7;

/// `cap_query_sql` wrap using a 10-digit LIMIT (`u32::MAX + 1`).
const D1_QUERY_CAP_WRAP_MAX_EXTRA: usize =
    "SELECT * FROM (".len() + ") AS _bc_cap LIMIT ".len() + 10;

/// Standardized Bookclerk sqlite-family lowering upper bound for a canonical
/// statement of `canonical_len` bytes: mechanical LIKE / `NULLIF` /
/// `INSERT OR IGNORE`, the query LIMIT wrap, and `unhex(?)` for every
/// advertised sqlite-family bind slot.
///
/// This length-only formula does **not** include proof-directed INTEGER
/// overflow CASE wraps: a worst-case `1+1+…` payload of
/// [`HOST_MIN_PAYLOAD_BYTES`] already exceeds [`D1_MAX_SQL_STATEMENT_BYTES`]
/// after wrapping. Overflow is charged by proven preflight from
/// `integer_arith_sites` (or a typecheck of literal-only SQL in
/// [`DbCapabilities::admit_statement`]). Hosts compare the proven bound to
/// [`DbCapabilities::max_lowered_statement_bytes`], not engine identity.
#[must_use]
pub const fn lowered_statement_upper_bound_len(canonical_len: usize) -> usize {
    crate::sql_text::sqlite_family_like_divmod_insert_upper_bound(canonical_len)
        .saturating_add(D1_QUERY_CAP_WRAP_MAX_EXTRA)
        .saturating_add((D1_MAX_BINDS as usize).saturating_mul(D1_UNHEX_PLACEHOLDER_EXTRA))
}

/// Deterministic lowered-statement preflight without a typed proof.
///
/// Mechanical sqlite-family lowering plus the query LIMIT wrap and `unhex(?)`
/// for each bind. INTEGER overflow wraps are omitted unless the caller uses
/// proven preflight with `integer_arith_sites`.
///
/// # Errors
///
/// Returns [`crate::PluginError::invalid_params`] when the pack lexer rejects
/// `sql`.
pub fn lowered_statement_preflight_len(sql: &str, bind_count: usize) -> crate::Result<usize> {
    lowered_statement_preflight(sql, bind_count, None)
}

/// [`lowered_statement_preflight_len`] after applying sqlite-family INTEGER
/// overflow wraps from `proof` (innermost-first, then mechanical `/` `%`
/// NULLIF / LIKE→GLOB / `INSERT OR IGNORE`).
///
/// # Errors
///
/// Returns when `proof` does not validate against `sql`, overflow wrapping
/// fails, or the pack lexer rejects the post-overflow SQL.
#[cfg(feature = "host")]
pub fn lowered_statement_preflight_len_proven(
    sql: &str,
    bind_count: usize,
    proof: Option<&crate::ResolvedStatement>,
) -> crate::Result<usize> {
    if let Some(proof) = proof {
        proof.validate_for(sql)?;
    }
    lowered_statement_preflight(sql, bind_count, proof)
}

/// Apply overflow wraps when `proof` has INTEGER sites, then the mechanical bound.
///
/// # Errors
///
/// Returns [`crate::PluginError::internal`] when overflow wrapping fails, or
/// [`crate::PluginError::invalid_params`] when the pack lexer rejects the
/// post-overflow SQL.
fn lowered_statement_preflight(
    sql: &str,
    bind_count: usize,
    proof: Option<&ResolvedStatement>,
) -> crate::Result<usize> {
    let after_overflow = match proof {
        Some(proof) if !proof.integer_arith_sites.is_empty() => apply_integer_overflow(
            OverflowDialect::SqliteFamily,
            sql,
            &proof.integer_arith_sites,
        )?,
        _ => sql.to_string(),
    };
    let binds = bind_count.min(D1_MAX_BINDS as usize);
    Ok(
        crate::sql_text::sqlite_family_mechanical_len_upper_bound(&after_overflow)?
            .saturating_add(D1_QUERY_CAP_WRAP_MAX_EXTRA)
            .saturating_add(binds.saturating_mul(D1_UNHEX_PLACEHOLDER_EXTRA)),
    )
}

/// Largest canonical payload that [`lowered_statement_upper_bound_len`] still
/// proves against [`D1_MAX_SQL_STATEMENT_BYTES`].
const fn proven_d1_max_payload_bytes() -> u32 {
    let physical = D1_MAX_SQL_STATEMENT_BYTES as usize;
    let mut n = HOST_MIN_PAYLOAD_BYTES as usize;
    let mut best = n;
    while n <= physical {
        if lowered_statement_upper_bound_len(n) <= physical {
            best = n;
            n += 1;
        } else {
            break;
        }
    }
    best as u32
}

/// Canonical SQL+binds cap advertised for D1.
///
/// Largest `n >= `[`HOST_MIN_PAYLOAD_BYTES`] such that
/// [`lowered_statement_upper_bound_len`]`(n) <= `[`D1_MAX_SQL_STATEMENT_BYTES`].
/// Host admission uses generic [`DbCapabilities::max_payload_bytes`] plus
/// [`DbCapabilities::max_lowered_statement_bytes`]; it does not name D1.
pub const D1_MAX_PAYLOAD_BYTES: u32 = proven_d1_max_payload_bytes();

/// Host refuses guests that do not bound JSON bytes of one statement's rows.
pub const HOST_MIN_RESULT_BYTES: u32 = 4_096;

/// Host refuses guests that do not bound one result cell (`0` is unspecified).
pub const HOST_MIN_CELL_BYTES: u32 = 1_024;

/// First-party JSON-byte budget for one statement's rows and for one atomic
/// request/result scalar. Must stay at or below [`crate::MAX_SCALAR_BYTES`].
pub const FIRST_PARTY_MAX_RESULT_BYTES: u32 = MAX_SCALAR_BYTES;

/// Bookclerk SQL contract version advertised by first-party adapters.
///
/// This integer is a **monotonic backward-compatible language/semantic level**.
/// Every guarantee in version *N* remains valid in *N+1*; guests advertise the
/// highest version they implement and hosts require
/// `sqlContractVersion >= SQL_CONTRACT_VERSION`. That `>=` check is only sound
/// while each successor is a superset. A non-superset redesign needs a **new
/// contract identity** and a plugin ABI break (`apiVersion`), not merely
/// incrementing this scalar.
pub const SQL_CONTRACT_VERSION: u32 = 1;

/// Bootstrap-only SeaORM proxy metadata returned by `AdapterDatabaseSession.bootstrap`.
///
/// Not part of the typed [`DbCapabilities`] plane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DbBootstrap {
    /// SQL family for SeaORM proxy bootstrap (`sqlite` or `postgres`).
    pub sql_family: String,
    /// Engine dialect name (`sqlite`, `postgres`, or `postgresql`).
    pub dialect: String,
}

impl DbBootstrap {
    /// Bootstrap metadata for a sqlite-family connection.
    #[must_use]
    pub fn sqlite() -> Self {
        Self {
            sql_family: "sqlite".into(),
            dialect: "sqlite".into(),
        }
    }

    /// Bootstrap metadata for a postgres-family connection.
    #[must_use]
    pub fn postgres() -> Self {
        Self {
            sql_family: "postgres".into(),
            dialect: "postgres".into(),
        }
    }

    /// SeaORM proxy backend failure from bootstrap metadata (`dialect` / `sqlFamily`).
    #[must_use]
    pub fn backend_failure_reason(&self) -> Option<String> {
        let family = self.sql_family.to_ascii_lowercase();
        if !family.is_empty() {
            if family != "sqlite" && family != "postgres" {
                return Some(format!(
                    "database guest sqlFamily {:?} is not sqlite or postgres (SQL-like backends only)",
                    self.sql_family
                ));
            }
            if !self.dialect.is_empty() && !dialect_matches_sql_family(&self.dialect, &family) {
                return Some(format!(
                    "database guest dialect {:?} does not match sqlFamily {:?}",
                    self.dialect, self.sql_family
                ));
            }
            return None;
        }
        let dialect = self.dialect.to_ascii_lowercase();
        if dialect.is_empty() {
            return Some("database guest dialect is required for SeaORM proxy bootstrap".into());
        }
        if dialect == "sqlite"
            || dialect == "postgres"
            || dialect == "postgresql"
            || dialect == "pg"
        {
            return None;
        }
        Some(format!(
            "database guest dialect {:?} is not sqlite or postgres (SQL-like backends only)",
            self.dialect
        ))
    }
}

/// True when SeaORM `dialect` names the same SQL family as `sql_family`.
fn dialect_matches_sql_family(dialect: &str, sql_family: &str) -> bool {
    match sql_family {
        "sqlite" => dialect.eq_ignore_ascii_case("sqlite"),
        "postgres" => {
            dialect.eq_ignore_ascii_case("postgres") || dialect.eq_ignore_ascii_case("postgresql")
        }
        _ => false,
    }
}

/// How a guest should run one statement inside an atomic plan.
///
/// `Select` versus `Returning` is explicit so adapters never reparse SQL to
/// decide whether `SELECT * FROM (…)` wrapping is valid. Matches Cap'n
/// `DbStatementKind` (`execute` | `select` | `returning`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum DbPlanStatementKind {
    /// Statement is DML; only `rowsAffected` is required.
    Execute,
    /// Read-only `SELECT` / read-only `WITH` CTE. May be wrapped with `LIMIT`.
    Select,
    /// DML that returns rows (`INSERT`/`UPDATE`/`DELETE … RETURNING`), or
    /// row-producing introspection (`PRAGMA`, schema reads) that must **not**
    /// be rewritten as a subquery.
    #[default]
    Returning,
}

impl DbPlanStatementKind {
    /// True when the guest must collect `rows` (not only `rowsAffected`).
    #[must_use]
    pub const fn collects_rows(self) -> bool {
        !matches!(self, Self::Execute)
    }

    /// True when the guest may wrap SQL as `SELECT * FROM (sql) LIMIT cap+1`.
    #[must_use]
    pub const fn wrap_select_limit(self) -> bool {
        matches!(self, Self::Select)
    }
}

/// How the guest should return results for one statement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum DbResultSelection {
    /// Drop rows and `rowsAffected`.
    Discard,
    /// Return `rowsAffected` only.
    #[default]
    AffectedRows,
    /// Return positional rows plus column metadata.
    Rows,
}

/// One column in a typed result set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DbColumn {
    /// Column name.
    pub name: String,
    /// Declared / inferred type.
    pub db_type: DbType,
}

/// One positional result row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DbRow {
    /// Cells in column order.
    pub values: Vec<DbValue>,
}

/// One statement in a typed [`ExecuteRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TypedDbStatement {
    /// Canonical Bookclerk SQL (`?` placeholders).
    pub sql: String,
    /// Ordered typed binds.
    pub parameters: Vec<DbValue>,
    /// Host-authored kind (adapters must not reparse SQL).
    pub kind: DbPlanStatementKind,
    /// Proven row upper bound (`0` = unproven).
    pub max_rows: u32,
    /// Which result fields the caller needs.
    pub result_selection: DbResultSelection,
}

/// Typed atomic batch. Every request is a non-empty ordered statement list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteRequest {
    /// Caller-chosen idempotency key.
    pub operation_id: String,
    /// SHA-256 hex of the idempotency-relevant request; empty when omitted.
    pub request_hash: String,
    /// Ordered statements (must be non-empty).
    pub statements: Vec<TypedDbStatement>,
    /// Guest-visible deadline (unix ms). Zero means omitted.
    pub deadline_unix_ms: u64,
}

/// Result of one statement in [`ExecuteReply`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatementResult {
    /// Positional rows (empty when discarded).
    pub rows: Vec<DbRow>,
    /// Column metadata matching [`Self::rows`] cell order.
    pub columns: Vec<DbColumn>,
    /// Engine-reported rows affected.
    pub rows_affected: u64,
}

impl StatementResult {
    /// Builds a row-bearing result and rejects width / name errors.
    ///
    /// # Errors
    ///
    /// Returns when a row length differs from `columns.len()` or a column name
    /// is duplicated.
    pub fn from_rows(columns: Vec<DbColumn>, rows: Vec<DbRow>) -> Result<Self, String> {
        let stmt = Self {
            rows,
            columns,
            rows_affected: 0,
        };
        stmt.validate_positional()?;
        Ok(stmt)
    }

    /// Builds an affected-rows-only result.
    #[must_use]
    pub fn from_affected(rows_affected: u64) -> Self {
        Self {
            rows: Vec::new(),
            columns: Vec::new(),
            rows_affected,
        }
    }

    /// Rejects duplicate column names and row widths that do not match `columns`.
    ///
    /// # Errors
    ///
    /// Returns when a row has the wrong cell count or two columns share a name.
    pub fn validate_positional(&self) -> Result<(), String> {
        let mut seen = HashSet::with_capacity(self.columns.len());
        for col in &self.columns {
            if !seen.insert(col.name.as_str()) {
                return Err(format!("duplicate result column name `{}`", col.name));
            }
        }
        let width = self.columns.len();
        for (i, row) in self.rows.iter().enumerate() {
            if row.values.len() != width {
                return Err(format!(
                    "result row {i} has {} values; columns has {width}",
                    row.values.len()
                ));
            }
        }
        Ok(())
    }
}

/// Engine timing on [`ExecuteReply`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DbTiming {
    /// Monotonic duration of this handler attempt.
    pub attempt_elapsed_us: u64,
    /// Engine-reported SQL/transaction time when available (`0` = omitted).
    pub db_execution_us: u64,
    /// How `db_execution_us` was measured.
    pub db_timing_source: String,
}

/// Typed atomic reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteReply {
    /// Echo of the request `operationId`.
    pub operation_id: String,
    /// Per-statement results, in plan order.
    pub statements: Vec<StatementResult>,
    /// Handler/engine timing.
    pub timing: DbTiming,
}

impl ExecuteReply {
    /// Rejects positional errors on every statement result.
    ///
    /// # Errors
    ///
    /// Returns when any statement has a row-width or duplicate-name error.
    pub fn validate_positional(&self) -> Result<(), String> {
        for (i, stmt) in self.statements.iter().enumerate() {
            stmt.validate_positional()
                .map_err(|err| format!("statement {i}: {err}"))?;
        }
        Ok(())
    }
}

/// Semantic SQL-contract advertisement (`AdapterDatabaseSession.capabilities`).
///
/// Bootstrap metadata (`sql_family`, `diagnostic_engine`, SeaORM `dialect`) is
/// negotiated separately via [`DbBootstrap`] — not on this typed capability plane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DbCapabilities {
    /// Bookclerk SQL contract version.
    pub sql_contract_version: u32,
    /// Guest can run a bounded statement list as one SQL transaction.
    pub atomic_batch: bool,
    /// Guest SQL supports `RETURNING`.
    pub returning: bool,
    /// Guest reports `rowsAffected`.
    pub affected_rows: bool,
    /// Guest versions schema with a `schema_migrations` table.
    pub schema_migrations: bool,
    /// Guest versions schema with `PRAGMA user_version`.
    pub pragma_user_version: bool,
    /// Each schema version must be applied as one atomic batch.
    pub atomic_schema_batch: bool,
    /// Guest honors RPC/session cancellation.
    pub cancellation: bool,
    /// Guest can fill [`DbTiming::db_execution_us`].
    pub timing: bool,
    /// Maximum bound parameters per statement.
    pub max_binds: u32,
    /// Maximum statements in one atomic batch.
    pub max_statements: u32,
    /// Maximum rows a query statement may return.
    pub max_result_rows: u32,
    /// Maximum UTF-8 bytes of SQL plus binds per statement.
    pub max_payload_bytes: u32,
    /// Maximum encoded bytes of one statement's result rows.
    pub max_result_bytes: u32,
    /// Maximum UTF-8 / blob bytes of one result cell.
    pub max_cell_bytes: u32,
    /// Maximum encoded bytes of one [`ExecuteRequest`].
    pub max_request_bytes: u32,
    /// Maximum encoded bytes of one [`ExecuteReply`].
    pub max_atomic_result_bytes: u32,
    /// Adapter can open additional isolated sessions for plugin-owned
    /// database bindings (per-binding file / schema / database).
    #[serde(default)]
    pub plugin_databases: bool,
    /// Maximum arguments in one physical function call after adapter hiding.
    /// `0` is unspecified.
    #[serde(default)]
    pub max_function_args: u32,
    /// Maximum columns in one `CREATE TABLE`. `0` is unspecified.
    #[serde(default)]
    pub max_schema_columns: u32,
    /// Maximum UTF-8 bytes of a BookclerkSQL `LIKE` pattern value.
    /// `0` is unspecified.
    #[serde(default)]
    pub max_pattern_bytes: u32,
    /// Maximum UTF-8 bytes of sqlite-family lowered SQL for one statement.
    /// `0` is unspecified (host does not enforce a lowered-size ceiling).
    #[serde(default)]
    pub max_lowered_statement_bytes: u32,
}

impl DbCapabilities {
    /// True when this guest meets the host's compiled minimum SQL contract.
    #[must_use]
    pub fn meets_host_minimums(&self) -> bool {
        self.capability_failure_reason_opt().is_none()
    }

    /// Operator-facing reason when [`Self::meets_host_minimums`] is false.
    #[must_use]
    pub fn capability_failure_reason(&self) -> String {
        self.capability_failure_reason_opt()
            .unwrap_or_else(|| "database guest failed capability negotiation".into())
    }

    /// Failure reason, or `None` when the guest meets host minima.
    fn capability_failure_reason_opt(&self) -> Option<String> {
        if !self.affected_rows {
            return Some("database guest does not advertise affectedRows".into());
        }
        if !self.cancellation {
            return Some("database guest does not advertise cancellation".into());
        }
        if !self.atomic_batch {
            return Some("database guest does not advertise atomicBatch".into());
        }
        if !self.returning {
            return Some(
                "database guest does not advertise returning (host plans require RETURNING)".into(),
            );
        }
        if self.max_binds < HOST_MIN_BINDS {
            return Some(format!(
                "database guest maxBinds {} is below host minimum {HOST_MIN_BINDS}",
                self.max_binds
            ));
        }
        if self.max_statements < HOST_MIN_STATEMENTS {
            return Some(format!(
                "database guest maxStatements {} is below host minimum {HOST_MIN_STATEMENTS}",
                self.max_statements
            ));
        }
        if self.max_result_rows < HOST_MIN_RESULT_ROWS {
            return Some(format!(
                "database guest maxResultRows {} is below host minimum {HOST_MIN_RESULT_ROWS}",
                self.max_result_rows
            ));
        }
        if self.max_payload_bytes < HOST_MIN_PAYLOAD_BYTES
            || self.max_payload_bytes > MAX_SCALAR_BYTES
        {
            return Some(format!(
                "database guest maxPayloadBytes {} must be between {HOST_MIN_PAYLOAD_BYTES} and {MAX_SCALAR_BYTES}",
                self.max_payload_bytes
            ));
        }
        if self.max_result_bytes < HOST_MIN_RESULT_BYTES {
            return Some(format!(
                "database guest maxResultBytes {} is below host minimum {HOST_MIN_RESULT_BYTES}",
                self.max_result_bytes
            ));
        }
        if self.max_cell_bytes < HOST_MIN_CELL_BYTES {
            return Some(format!(
                "database guest maxCellBytes {} is below host minimum {HOST_MIN_CELL_BYTES}",
                self.max_cell_bytes
            ));
        }
        if self.max_request_bytes < HOST_MIN_RESULT_BYTES
            || self.max_request_bytes > MAX_SCALAR_BYTES
        {
            return Some(format!(
                "database guest maxRequestBytes {} must be between {HOST_MIN_RESULT_BYTES} and {MAX_SCALAR_BYTES}",
                self.max_request_bytes
            ));
        }
        if self.max_atomic_result_bytes < HOST_MIN_RESULT_BYTES
            || self.max_atomic_result_bytes > MAX_SCALAR_BYTES
        {
            return Some(format!(
                "database guest maxAtomicResultBytes {} must be between {HOST_MIN_RESULT_BYTES} and {MAX_SCALAR_BYTES}",
                self.max_atomic_result_bytes
            ));
        }
        if self.max_result_bytes > self.max_atomic_result_bytes {
            return Some(format!(
                "database guest maxResultBytes {} exceeds maxAtomicResultBytes {}",
                self.max_result_bytes, self.max_atomic_result_bytes
            ));
        }
        if self.sql_contract_version < SQL_CONTRACT_VERSION {
            return Some(format!(
                "database guest sqlContractVersion {} is below host minimum {SQL_CONTRACT_VERSION}",
                self.sql_contract_version
            ));
        }
        if self.max_function_args < HOST_MIN_FUNCTION_ARGS {
            return Some(format!(
                "database guest maxFunctionArgs {} is below host minimum {HOST_MIN_FUNCTION_ARGS}",
                self.max_function_args
            ));
        }
        if self.max_schema_columns < HOST_MIN_SCHEMA_COLUMNS {
            return Some(format!(
                "database guest maxSchemaColumns {} is below host minimum {HOST_MIN_SCHEMA_COLUMNS}",
                self.max_schema_columns
            ));
        }
        if self.max_pattern_bytes < HOST_MIN_PATTERN_BYTES {
            return Some(format!(
                "database guest maxPatternBytes {} is below host minimum {HOST_MIN_PATTERN_BYTES}",
                self.max_pattern_bytes
            ));
        }
        None
    }

    /// First-party SQLite capability advertisement (`PRAGMA user_version` marker).
    #[must_use]
    pub fn advertised_sqlite() -> Self {
        Self {
            sql_contract_version: SQL_CONTRACT_VERSION,
            atomic_batch: true,
            returning: true,
            affected_rows: true,
            schema_migrations: false,
            pragma_user_version: true,
            atomic_schema_batch: false,
            cancellation: true,
            timing: true,
            max_binds: SQLITE_MAX_BINDS,
            max_statements: FIRST_PARTY_MAX_STATEMENTS,
            max_result_rows: FIRST_PARTY_MAX_RESULT_ROWS,
            max_payload_bytes: MAX_SCALAR_BYTES,
            max_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
            max_cell_bytes: MAX_SCALAR_BYTES,
            max_request_bytes: MAX_SCALAR_BYTES,
            max_atomic_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
            plugin_databases: true,
            max_function_args: SQLITE_MAX_FUNCTION_ARGS,
            max_schema_columns: SQLITE_MAX_SCHEMA_COLUMNS,
            max_pattern_bytes: SQLITE_MAX_PATTERN_BYTES,
            max_lowered_statement_bytes: 0,
        }
    }

    /// First-party Cloudflare D1 capability advertisement
    /// (`schema_migrations` rows, one atomic HTTP batch per schema version).
    #[must_use]
    pub fn advertised_d1() -> Self {
        Self {
            schema_migrations: true,
            pragma_user_version: false,
            atomic_schema_batch: true,
            max_binds: D1_MAX_BINDS,
            max_payload_bytes: D1_MAX_PAYLOAD_BYTES,
            max_function_args: D1_MAX_FUNCTION_ARGS,
            max_schema_columns: D1_MAX_SCHEMA_COLUMNS,
            max_pattern_bytes: D1_PORTABLE_LIKE_PATTERN_BYTES,
            max_lowered_statement_bytes: D1_MAX_SQL_STATEMENT_BYTES,
            ..Self::advertised_sqlite()
        }
    }

    /// First-party PostgreSQL capability advertisement (`schema_migrations` rows).
    #[must_use]
    pub fn advertised_postgres() -> Self {
        Self {
            schema_migrations: true,
            pragma_user_version: false,
            atomic_schema_batch: false,
            max_binds: POSTGRES_MAX_BINDS,
            max_function_args: POSTGRES_MAX_FUNCTION_ARGS,
            max_schema_columns: POSTGRES_MAX_SCHEMA_COLUMNS,
            ..Self::advertised_sqlite()
        }
    }

    /// Rejects BookclerkSQL that exceeds this advertisement before dispatch.
    ///
    /// Literal INTEGER arithmetic typechecks against an empty catalog so dense
    /// `1+1+…` contributes overflow wrap cost. Statements that need a real
    /// catalog must use [`Self::admit_statement_proven`] / [`Self::admit_proven_execute`]
    /// after the host has a [`ResolvedStatement`].
    ///
    /// Lowered-size admission compares the standardized Bookclerk lowering
    /// upper bound to [`Self::max_lowered_statement_bytes`] (`0` skips that
    /// check). Canonical `maxPayloadBytes` is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`crate::PluginError::invalid_params`] when a statement exceeds
    /// schema-column, LIKE-pattern, TEXT-domain, or lowered-statement limits.
    pub fn admit_statement(&self, sql: &str, parameters: &[crate::DbValue]) -> crate::Result<()> {
        let proof = admit_literal_proof(sql, parameters);
        self.admit_statement_proven(sql, parameters, proof.as_ref())
    }

    /// [`Self::admit_statement`] using a host-resolved proof when present.
    ///
    /// # Errors
    ///
    /// Returns [`crate::PluginError::invalid_params`] when a statement exceeds
    /// negotiated limits, or [`crate::PluginError::internal`] when overflow
    /// wrapping fails.
    pub fn admit_statement_proven(
        &self,
        sql: &str,
        parameters: &[crate::DbValue],
        proof: Option<&ResolvedStatement>,
    ) -> crate::Result<()> {
        require_portable_text(sql)?;
        require_portable_text_binds(parameters)?;
        require_like_patterns_within(sql, parameters, self.max_pattern_bytes)?;
        require_function_args_within(sql, self.max_function_args)?;
        if let Some(schema) = crate::sql_types::parse_create_table_schema(sql) {
            let n = u32::try_from(schema.columns.len()).unwrap_or(u32::MAX);
            if self.max_schema_columns > 0 && n > self.max_schema_columns {
                return Err(crate::PluginError::invalid_params(format!(
                    "CREATE TABLE has {n} columns; guest maxSchemaColumns is {}",
                    self.max_schema_columns
                )));
            }
        }
        let pre = lowered_statement_preflight(sql, parameters.len(), proof)?;
        if self.max_lowered_statement_bytes > 0 {
            let cap = self.max_lowered_statement_bytes as usize;
            if pre > cap {
                return Err(crate::PluginError::invalid_params(format!(
                    "lowered SQL preflight is {pre} bytes; advertised maxLoweredStatementBytes is {cap}"
                )));
            }
        }
        Ok(())
    }

    /// Rejects a resolved batch whose proven lowered size exceeds this advertisement.
    ///
    /// Call after the host has 1:1 [`ResolvedStatement`] proofs and before
    /// adapter dispatch. Does not inspect engine identity.
    ///
    /// # Errors
    ///
    /// Returns when `proofs` is the wrong length, a proof does not bind, or a
    /// statement fails [`Self::admit_statement_proven`].
    pub fn admit_proven_execute(
        &self,
        req: &ExecuteRequest,
        proofs: &[ResolvedStatement],
    ) -> crate::Result<()> {
        if proofs.len() != req.statements.len() {
            return Err(crate::PluginError::invalid_params(format!(
                "resolved proofs must match statement count ({} proofs, {} statements)",
                proofs.len(),
                req.statements.len()
            )));
        }
        for (stmt, proof) in req.statements.iter().zip(proofs.iter()) {
            proof.validate_for(&stmt.sql)?;
            self.admit_statement_proven(&stmt.sql, &stmt.parameters, Some(proof))?;
        }
        Ok(())
    }
}

/// Typecheck `sql` against an empty catalog so literal INTEGER arithmetic
/// (dense `1+1+…`) contributes overflow wrap cost at admission.
fn admit_literal_proof(sql: &str, parameters: &[crate::DbValue]) -> Option<ResolvedStatement> {
    let req = ExecuteRequest {
        operation_id: "admit".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.to_string(),
            parameters: parameters.to_vec(),
            kind: DbPlanStatementKind::Execute,
            max_rows: 0,
            result_selection: DbResultSelection::Discard,
        }],
        deadline_unix_ms: 0,
    };
    crate::sql_types::typecheck_execute_request_resolved(&req, &SqlTypeEnv::new())
        .ok()?
        .into_iter()
        .next()
}

/// UTF-8 bytes of SQL text plus JSON binds (ordinary query/execute payload).
#[must_use]
pub fn sql_payload_bytes(sql: &str, values_json: &str) -> usize {
    sql.len().saturating_add(values_json.len())
}

/// True when ordinary-path SQL+binds exceed the negotiated payload cap.
///
/// The effective cap is `min(max_payload_bytes, MAX_SCALAR_BYTES)`. A cap of
/// `0` fails closed (any non-empty payload exceeds it).
#[must_use]
pub fn sql_payload_exceeds(sql: &str, values_json: &str, max_payload_bytes: u32) -> bool {
    let cap = usize::try_from(max_payload_bytes.min(MAX_SCALAR_BYTES)).unwrap_or(0);
    sql_payload_bytes(sql, values_json) > cap
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn payload_cap_is_scalar_ceiling() {
        assert!(sql_payload_exceeds("SELECT 1", "[]", 0));
        assert!(!sql_payload_exceeds("SELECT 1", "[]", 64));
        let big = "x".repeat(MAX_SCALAR_BYTES as usize);
        assert!(sql_payload_exceeds(&big, "[]", MAX_SCALAR_BYTES));
        assert!(!sql_payload_exceeds("SELECT 1", "[]", MAX_SCALAR_BYTES + 1));
    }

    #[test]
    fn capabilities_reject_missing_cancellation() {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.cancellation = false;
        assert!(!caps.meets_host_minimums());
        assert!(caps.capability_failure_reason().contains("cancellation"));
    }

    #[test]
    fn capabilities_reject_missing_affected_rows() {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.affected_rows = false;
        assert!(!caps.meets_host_minimums());
        assert!(caps.capability_failure_reason().contains("affectedRows"));
    }

    #[test]
    fn advertised_presets_meet_host_minimums() {
        for caps in [
            DbCapabilities::advertised_sqlite(),
            DbCapabilities::advertised_postgres(),
            DbCapabilities::advertised_d1(),
        ] {
            assert!(
                caps.meets_host_minimums(),
                "{}",
                caps.capability_failure_reason()
            );
            assert_eq!(caps.sql_contract_version, SQL_CONTRACT_VERSION);
            assert_ne!(caps.pragma_user_version, caps.schema_migrations);
        }
        assert_eq!(
            DbCapabilities::advertised_sqlite().max_binds,
            SQLITE_MAX_BINDS
        );
        assert_eq!(
            DbCapabilities::advertised_postgres().max_binds,
            POSTGRES_MAX_BINDS
        );
        assert_eq!(DbCapabilities::advertised_d1().max_binds, D1_MAX_BINDS);
        assert_eq!(
            DbCapabilities::advertised_d1().max_payload_bytes,
            D1_MAX_PAYLOAD_BYTES
        );
        assert_eq!(
            DbCapabilities::advertised_d1().max_function_args,
            D1_MAX_FUNCTION_ARGS
        );
        assert_eq!(
            DbCapabilities::advertised_d1().max_schema_columns,
            D1_MAX_SCHEMA_COLUMNS
        );
        assert_eq!(
            DbCapabilities::advertised_d1().max_pattern_bytes,
            D1_PORTABLE_LIKE_PATTERN_BYTES
        );
        assert!(DbCapabilities::advertised_d1().atomic_schema_batch);
        assert!(!DbCapabilities::advertised_postgres().atomic_schema_batch);
    }

    #[test]
    fn capabilities_fail_closed_without_returning_or_bounds() {
        let mut no_returning = DbCapabilities::advertised_sqlite();
        no_returning.returning = false;
        assert!(!no_returning.meets_host_minimums());
        assert!(no_returning
            .capability_failure_reason()
            .contains("returning"));

        let mut zero_rows = DbCapabilities::advertised_sqlite();
        zero_rows.max_result_rows = 0;
        assert!(!zero_rows.meets_host_minimums());
        assert!(zero_rows
            .capability_failure_reason()
            .contains("maxResultRows"));

        let mut zero_payload = DbCapabilities::advertised_sqlite();
        zero_payload.max_payload_bytes = 0;
        assert!(!zero_payload.meets_host_minimums());
        assert!(zero_payload
            .capability_failure_reason()
            .contains("maxPayloadBytes"));

        let mut zero_result = DbCapabilities::advertised_sqlite();
        zero_result.max_result_bytes = 0;
        assert!(!zero_result.meets_host_minimums());
        assert!(zero_result
            .capability_failure_reason()
            .contains("maxResultBytes"));

        let mut zero_cell = DbCapabilities::advertised_sqlite();
        zero_cell.max_cell_bytes = 0;
        assert!(!zero_cell.meets_host_minimums());
        assert!(zero_cell
            .capability_failure_reason()
            .contains("maxCellBytes"));

        let mut zero_request = DbCapabilities::advertised_sqlite();
        zero_request.max_request_bytes = 0;
        assert!(!zero_request.meets_host_minimums());
        assert!(zero_request
            .capability_failure_reason()
            .contains("maxRequestBytes"));

        let mut over_scalar = DbCapabilities::advertised_sqlite();
        over_scalar.max_atomic_result_bytes = MAX_SCALAR_BYTES + 1;
        assert!(!over_scalar.meets_host_minimums());
        assert!(over_scalar
            .capability_failure_reason()
            .contains("maxAtomicResultBytes"));

        let mut zero_fn = DbCapabilities::advertised_sqlite();
        zero_fn.max_function_args = 0;
        assert!(!zero_fn.meets_host_minimums());
        assert!(zero_fn
            .capability_failure_reason()
            .contains("maxFunctionArgs"));

        let mut zero_cols = DbCapabilities::advertised_sqlite();
        zero_cols.max_schema_columns = 0;
        assert!(!zero_cols.meets_host_minimums());
        assert!(zero_cols
            .capability_failure_reason()
            .contains("maxSchemaColumns"));

        let mut zero_pat = DbCapabilities::advertised_sqlite();
        zero_pat.max_pattern_bytes = 0;
        assert!(!zero_pat.meets_host_minimums());
        assert!(zero_pat
            .capability_failure_reason()
            .contains("maxPatternBytes"));
    }

    #[test]
    fn admit_statement_enforces_pattern_and_column_n_plus_one() {
        let mut caps = DbCapabilities::advertised_d1();
        let n = caps.max_pattern_bytes as usize;
        let ok_pat = "a".repeat(n);
        let sql_ok = format!("SELECT * FROM t WHERE x LIKE '{ok_pat}'");
        caps.admit_statement(&sql_ok, &[]).expect("N pattern");
        let sql_over = format!("SELECT * FROM t WHERE x LIKE '{}'", "a".repeat(n + 1));
        let err = caps.admit_statement(&sql_over, &[]).unwrap_err();
        assert!(err.to_string().contains("maxPatternBytes"), "{err}");

        caps.max_schema_columns = 2;
        caps.admit_statement("CREATE TABLE t (a INTEGER, b TEXT)", &[])
            .expect("N columns");
        let err = caps
            .admit_statement("CREATE TABLE t (a INTEGER, b TEXT, c REAL)", &[])
            .unwrap_err();
        assert!(err.to_string().contains("maxSchemaColumns"), "{err}");

        let err = caps.admit_statement("SELECT '\0'", &[]).unwrap_err();
        assert!(err.to_string().contains("U+0000"), "{err}");
        caps.admit_statement("SELECT ?", &[DbValue::Bytes(vec![0])])
            .expect("BLOB NUL");
        let err = caps
            .admit_statement("SELECT ?", &[DbValue::Text("a\0b".into())])
            .unwrap_err();
        assert!(err.to_string().contains("U+0000"), "{err}");
    }

    #[test]
    fn advertised_d1_payload_is_proven_physical_bound() {
        const { assert!(D1_MAX_PAYLOAD_BYTES >= HOST_MIN_PAYLOAD_BYTES) };
        const {
            assert!(
                lowered_statement_upper_bound_len(D1_MAX_PAYLOAD_BYTES as usize)
                    <= D1_MAX_SQL_STATEMENT_BYTES as usize
            )
        };
        const {
            assert!(
                lowered_statement_upper_bound_len(D1_MAX_PAYLOAD_BYTES as usize + 1)
                    > D1_MAX_SQL_STATEMENT_BYTES as usize
            )
        };
        const {
            assert!(lowered_statement_upper_bound_len(25_000) > D1_MAX_SQL_STATEMENT_BYTES as usize)
        };
        let sql =
            "SELECT json_extract(ifnull(body, '{}'), '$.k') FROM t WHERE x LIKE '[%]_?*' AND 1/2";
        let pre = lowered_statement_preflight_len(sql, 0).expect("preflight");
        assert!(pre <= lowered_statement_upper_bound_len(sql.len()), "{pre}");
        assert!(pre <= D1_MAX_SQL_STATEMENT_BYTES as usize, "{pre}");
        assert_eq!(
            DbCapabilities::advertised_d1().max_lowered_statement_bytes,
            D1_MAX_SQL_STATEMENT_BYTES
        );
        assert_eq!(
            DbCapabilities::advertised_sqlite().max_lowered_statement_bytes,
            0
        );
    }

    fn dense_add_sql(adds: usize) -> String {
        let mut sql = String::from("SELECT 1");
        for _ in 0..adds {
            sql.push_str("+1");
        }
        sql
    }

    fn dense_sub_sql(ops: usize) -> String {
        let mut sql = String::from("SELECT 1");
        for _ in 0..ops {
            sql.push_str("-1");
        }
        sql
    }

    fn dense_mul_sql(ops: usize) -> String {
        let mut sql = String::from("SELECT 1");
        for _ in 0..ops {
            sql.push_str("*1");
        }
        sql
    }

    fn nested_abs_add_sql(depth: usize) -> String {
        let mut expr = String::from("1");
        for _ in 0..depth {
            expr = format!("abs({expr}+1)");
        }
        format!("SELECT {expr}")
    }

    fn mixed_like_json_add_sql(adds: usize) -> String {
        let mut sql = String::from("SELECT json_extract('{}', '$.k'), 1");
        for _ in 0..adds {
            sql.push_str("+1");
        }
        sql.push_str(" WHERE 'x' LIKE 'x'");
        sql
    }

    fn max_d1_admitted(build: impl Fn(usize) -> String, params: &[DbValue], cap: usize) -> usize {
        let caps = DbCapabilities::advertised_d1();
        let mut lo = 0usize;
        let mut hi = 1usize;
        while hi < cap {
            let sql = build(hi);
            if sql.len() > caps.max_payload_bytes as usize
                || caps.admit_statement(&sql, params).is_err()
            {
                break;
            }
            lo = hi;
            hi = (hi.saturating_mul(2)).min(cap);
        }
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            let sql = build(mid);
            if sql.len() <= caps.max_payload_bytes as usize
                && caps.admit_statement(&sql, params).is_ok()
            {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn assert_overflow_n_plus_one(
        label: &str,
        build: impl Fn(usize) -> String,
        params: &[DbValue],
        cap: usize,
    ) {
        let caps = DbCapabilities::advertised_d1();
        let n_ok = max_d1_admitted(&build, params, cap);
        assert!(n_ok >= 1, "{label}: expected at least one admitted chain");
        let sql_ok = build(n_ok);
        let proof = admit_literal_proof(&sql_ok, params);
        assert!(
            proof
                .as_ref()
                .is_some_and(|p| !p.integer_arith_sites.is_empty()),
            "{label}: admitted SQL must typecheck with arith sites"
        );
        let pre_ok =
            lowered_statement_preflight(&sql_ok, params.len(), proof.as_ref()).expect("pre N");
        let ceiling = caps.max_lowered_statement_bytes as usize;
        assert!(
            pre_ok <= ceiling,
            "{label} N preflight {pre_ok} > {ceiling}"
        );
        assert!(
            pre_ok <= D1_MAX_SQL_STATEMENT_BYTES as usize,
            "{label} N preflight {pre_ok}"
        );

        let sql_over = build(n_ok + 1);
        let mechanical = lowered_statement_preflight_len(&sql_over, params.len()).expect("mech");
        assert!(
            mechanical <= D1_MAX_SQL_STATEMENT_BYTES as usize,
            "{label}: N+1 must still fit the mechanical-only bound ({mechanical})"
        );
        let err = caps.admit_statement(&sql_over, params).unwrap_err();
        assert!(
            err.to_string().contains("maxLoweredStatementBytes"),
            "{label}: first over-limit case must be admission/preflight, got {err}"
        );
    }

    #[test]
    fn d1_overflow_chains_fail_closed_at_portable_admission() {
        assert_overflow_n_plus_one("add", dense_add_sql, &[], 800);
        assert_overflow_n_plus_one("sub", dense_sub_sql, &[], 800);
        assert_overflow_n_plus_one("mul", dense_mul_sql, &[], 800);
        assert_overflow_n_plus_one("like+json+add", mixed_like_json_add_sql, &[], 800);
        let blob = [DbValue::Bytes(vec![0, 1, 2])];
        assert_overflow_n_plus_one(
            "add+blob",
            |n| format!("{}, ?", dense_add_sql(n)),
            &blob,
            800,
        );
    }

    #[test]
    fn d1_nested_overflow_sites_are_charged_before_mechanical() {
        let caps = DbCapabilities::advertised_d1();
        let sql = nested_abs_add_sql(6);
        caps.admit_statement(&sql, &[])
            .expect("shallow nested abs/add must admit");
        let proof = admit_literal_proof(&sql, &[]).expect("typecheck");
        assert!(
            proof.integer_arith_sites.len() >= 12,
            "each abs(+1) records abs and add: {:?}",
            proof.integer_arith_sites.len()
        );
        let mechanical = lowered_statement_preflight_len(&sql, 0).expect("mech");
        let proven = lowered_statement_preflight(&sql, 0, Some(&proof)).expect("proven");
        assert!(
            proven > mechanical,
            "nested overflow wraps must exceed mechanical-only ({proven} vs {mechanical})"
        );
        assert!(proven <= caps.max_lowered_statement_bytes as usize);
    }

    fn integer_column_env() -> SqlTypeEnv {
        let mut env = SqlTypeEnv::new();
        env.insert_table("t", vec![("n".into(), crate::sql_types::SqlType::Integer)]);
        env
    }

    fn dense_column_add_sql(adds: usize) -> String {
        let mut sql = String::from("SELECT n");
        for _ in 0..adds {
            sql.push_str("+n");
        }
        sql.push_str(" FROM t");
        sql
    }

    fn column_add_request(sql: &str) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: "col".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: sql.to_string(),
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        }
    }

    #[test]
    fn lowered_size_n_plus_one_uses_integer_column_proofs() {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_lowered_statement_bytes = D1_MAX_SQL_STATEMENT_BYTES;
        let env = integer_column_env();
        let mut lo = 0usize;
        let mut hi = 1usize;
        while hi < 800 {
            let sql = dense_column_add_sql(hi);
            let req = column_add_request(&sql);
            let Ok(proofs) = crate::sql_types::typecheck_execute_request_resolved(&req, &env)
            else {
                break;
            };
            if sql.len() > caps.max_payload_bytes as usize
                || caps.admit_proven_execute(&req, &proofs).is_err()
            {
                break;
            }
            lo = hi;
            hi = (hi.saturating_mul(2)).min(800);
        }
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            let sql = dense_column_add_sql(mid);
            let req = column_add_request(&sql);
            let ok = sql.len() <= caps.max_payload_bytes as usize
                && crate::sql_types::typecheck_execute_request_resolved(&req, &env)
                    .ok()
                    .and_then(|proofs| caps.admit_proven_execute(&req, &proofs).ok())
                    .is_some();
            if ok {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        assert!(lo >= 1, "expected an admitted INTEGER column chain");

        let sql_ok = dense_column_add_sql(lo);
        let req_ok = column_add_request(&sql_ok);
        let proofs_ok = crate::sql_types::typecheck_execute_request_resolved(&req_ok, &env)
            .expect("typecheck N");
        assert!(
            !proofs_ok[0].integer_arith_sites.is_empty(),
            "column arithmetic must produce overflow sites"
        );
        caps.admit_statement(&sql_ok, &[])
            .expect("canonical empty-catalog admit still succeeds (no column types)");
        caps.admit_proven_execute(&req_ok, &proofs_ok)
            .expect("N proven lowered size");

        let sql_over = dense_column_add_sql(lo + 1);
        let req_over = column_add_request(&sql_over);
        let proofs_over = crate::sql_types::typecheck_execute_request_resolved(&req_over, &env)
            .expect("typecheck N+1");
        assert!(
            sql_over.len() <= caps.max_payload_bytes as usize,
            "N+1 canonical SQL must still fit maxPayloadBytes ({})",
            sql_over.len()
        );
        caps.admit_statement(&sql_over, &[])
            .expect("empty-catalog admit must not charge column INTEGER overflow");
        let err = caps
            .admit_proven_execute(&req_over, &proofs_over)
            .expect_err("N+1 proven lowered size");
        assert!(
            err.to_string().contains("maxLoweredStatementBytes"),
            "host proven admission must reject without engine identity, got {err}"
        );
        let mechanical = lowered_statement_preflight_len(&sql_over, 0).expect("mech");
        assert!(
            mechanical <= D1_MAX_SQL_STATEMENT_BYTES as usize,
            "N+1 mechanical-only still fits ({mechanical})"
        );
    }

    #[test]
    fn bootstrap_backend_failure_reason_rejects_non_sql_families() {
        let mut bootstrap = DbBootstrap::sqlite();
        bootstrap.sql_family = "mystery".into();
        let reason = bootstrap.backend_failure_reason().expect("reject");
        assert!(reason.contains("sqlFamily"), "{reason}");

        let mut mismatch = DbBootstrap::sqlite();
        mismatch.dialect = "postgres".into();
        let reason = mismatch.backend_failure_reason().expect("reject");
        assert!(reason.contains("does not match"), "{reason}");

        assert!(DbBootstrap::sqlite().backend_failure_reason().is_none());
        assert!(DbBootstrap::postgres().backend_failure_reason().is_none());
        let empty = DbBootstrap::default();
        assert!(empty.backend_failure_reason().is_some());
    }

    #[test]
    fn duplicate_column_names_are_rejected() {
        let stmt = StatementResult {
            columns: vec![
                DbColumn {
                    name: "id".into(),
                    db_type: DbType::Int64,
                },
                DbColumn {
                    name: "id".into(),
                    db_type: DbType::Text,
                },
            ],
            rows: vec![DbRow {
                values: vec![DbValue::Int64(1), DbValue::Text("x".into())],
            }],
            rows_affected: 1,
        };
        let err = stmt.validate_positional().unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn row_width_mismatch_is_rejected() {
        let stmt = StatementResult {
            columns: vec![DbColumn {
                name: "id".into(),
                db_type: DbType::Int64,
            }],
            rows: vec![DbRow {
                values: vec![DbValue::Int64(1), DbValue::Int64(2)],
            }],
            rows_affected: 1,
        };
        let err = stmt.validate_positional().unwrap_err();
        assert!(err.contains("values"), "{err}");
    }

    #[test]
    fn capnp_db_value_goldens_roundtrip() {
        use crate::{decode_db_value_bytes, encoded_db_value_bytes};
        let cases = [
            DbValue::Int64(i64::MIN),
            DbValue::Int64(i64::MAX),
            DbValue::Text("b64:AAAA".into()),
            DbValue::Bytes(vec![0, 1, 2]),
            DbValue::Null(DbType::Bytes),
            DbValue::Boolean(true),
        ];
        for v in cases {
            let bytes = encoded_db_value_bytes(&v).unwrap();
            let back = decode_db_value_bytes(&bytes).unwrap();
            assert_eq!(back, v);
        }
        for sample in [
            "汉语",
            "日本語",
            "한국어",
            "𠀀",
            "مرحبا",
            "שלום",
            "नमस्ते",
            "สวัสดี",
            "γειά",
            "привет",
            "🎵🦀",
            "café",
            "cafe\u{0301}",
        ] {
            let v = DbValue::Text(sample.into());
            let back = decode_db_value_bytes(&encoded_db_value_bytes(&v).unwrap()).unwrap();
            assert_eq!(back, v, "{sample}");
        }
        assert_ne!("café".as_bytes(), "cafe\u{0301}".as_bytes());
        encoded_db_value_bytes(&DbValue::Text("a\0b".into())).expect_err("TEXT NUL");
        encoded_db_value_bytes(&DbValue::Bytes(vec![0])).expect("BLOB NUL");
        let text = encoded_db_value_bytes(&DbValue::Text("b64:AAAA".into())).unwrap();
        let blob = encoded_db_value_bytes(&DbValue::Bytes(vec![0, 1, 2])).unwrap();
        assert_ne!(text, blob);
        assert_eq!(
            hex::encode(encoded_db_value_bytes(&DbValue::Int64(i64::MIN)).unwrap()),
            "00000000040000000000000002000100000002000000000000000000000000800000000000000000"
        );
        assert_eq!(
            hex::encode(encoded_db_value_bytes(&DbValue::Int64(i64::MAX)).unwrap()),
            "000000000400000000000000020001000000020000000000ffffffffffffff7f0000000000000000"
        );
        assert_eq!(
            hex::encode(encoded_db_value_bytes(&DbValue::Text("b64:AAAA".into())).unwrap()),
            "0000000006000000000000000200010000000400000000000000000000000000010000004a0000006236343a414141410000000000000000"
        );
        assert_eq!(
            hex::encode(encoded_db_value_bytes(&DbValue::Bytes(vec![0, 1, 2])).unwrap()),
            "0000000005000000000000000200010000000500000000000000000000000000010000001a0000000001020000000000"
        );
        assert_eq!(
            hex::encode(encoded_db_value_bytes(&DbValue::Boolean(true)).unwrap()),
            "00000000040000000000000002000100010001000000000000000000000000000000000000000000"
        );
        assert_eq!(
            hex::encode(encoded_db_value_bytes(&DbValue::Null(DbType::Bytes)).unwrap()),
            "00000000040000000000000002000100050000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn execute_request_struct_excludes_host_plan_selectors() {
        let req = ExecuteRequest {
            operation_id: "op".into(),
            request_hash: "abc".into(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 1".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let _ = (
            &req.operation_id,
            &req.request_hash,
            &req.statements,
            req.deadline_unix_ms,
        );
        use crate::{decode_execute_request_bytes, encoded_execute_request_bytes};
        let bytes = encoded_execute_request_bytes(&req).unwrap();
        let back = decode_execute_request_bytes(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn capnp_execute_request_roundtrip() {
        use crate::{decode_execute_request_bytes, encoded_execute_request_bytes};
        let req = ExecuteRequest {
            operation_id: "op".into(),
            request_hash: "abc".into(),
            statements: vec![TypedDbStatement {
                sql: "SELECT ?".into(),
                parameters: vec![
                    DbValue::Int64(i64::MIN),
                    DbValue::Text("b64:not-bytes".into()),
                    DbValue::Bytes(vec![0xff]),
                ],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let bytes = encoded_execute_request_bytes(&req).unwrap();
        let back = decode_execute_request_bytes(&bytes).unwrap();
        assert_eq!(back.operation_id, req.operation_id);
        assert_eq!(back.statements, req.statements);
    }

    #[test]
    fn public_statement_kind_matches_capnp_ordinals() {
        use crate::{decode_execute_request_bytes, encoded_execute_request_bytes};
        // Cap'n `DbStatementKind`: execute@0, select@1, returning@2.
        for kind in [
            DbPlanStatementKind::Execute,
            DbPlanStatementKind::Select,
            DbPlanStatementKind::Returning,
        ] {
            let req = ExecuteRequest {
                operation_id: "op".into(),
                request_hash: String::new(),
                statements: vec![TypedDbStatement {
                    sql: "SELECT 1".into(),
                    parameters: vec![],
                    kind,
                    max_rows: 0,
                    result_selection: DbResultSelection::Rows,
                }],
                deadline_unix_ms: 0,
            };
            let bytes = encoded_execute_request_bytes(&req).unwrap();
            let back = decode_execute_request_bytes(&bytes).unwrap();
            assert_eq!(back.statements[0].kind, kind);
        }
        assert_eq!(
            serde_json::to_string(&DbPlanStatementKind::Returning).unwrap(),
            "\"returning\""
        );
    }

    #[test]
    fn decode_execute_request_rejects_multi_segment_messages() {
        use crate::{decode_execute_request_bytes, encoded_execute_request_bytes};
        let req = ExecuteRequest {
            operation_id: "op".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 1".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let mut bytes = encoded_execute_request_bytes(&req).unwrap();
        assert_eq!(bytes[0], 0, "fixture must be a single-segment stream");
        bytes[0] = 1;
        let err = decode_execute_request_bytes(&bytes).unwrap_err();
        assert!(err.to_string().contains("multi-segment"), "{err}");
    }

    #[test]
    fn decode_execute_request_rejects_truncated_messages() {
        use crate::decode_execute_request_bytes;
        let err = decode_execute_request_bytes(&[0, 0, 0]).unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }
}
