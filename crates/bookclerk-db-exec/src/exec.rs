//! Shared SeaORM execute helpers used by typed [`crate::typed`] execution.

#![allow(clippy::missing_docs_in_private_items)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bookclerk_plugin_abi::{DbCapabilities, DbPlanStatementKind, SqlTypeEnv};
use futures::TryStreamExt;
use sea_orm::{ConnectionTrait, DbErr, ExecResult, QueryResult, Statement, StreamTrait, Value};
use serde_json::Value as JsonValue;

use crate::proxy_txn::{
    consume_atomic_interrupt, note_query_row, AtomicInterruptKind, AtomicInterruptPhase,
};

/// Real engine the adapter opened.
///
/// Hosts must not construct this from a SeaORM proxy connection. Physical
/// lowering ([`crate::lower_canonical_sql`]) happens only against this identity.
/// Unknown SeaORM backends fail closed — never default to SQLite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalEngine {
    /// SQLite physical engine (rusqlite / files).
    Sqlite,
    /// PostgreSQL 16+ physical engine.
    Postgres,
    /// Cloudflare D1 (sqlite-family SQL, HTTP batch).
    D1,
}

/// Fail closed on SeaORM backends that are not BookclerkSQL engines.
///
/// # Panics
///
/// Always panics: MySQL and unknown SeaORM backends are not BookclerkSQL engines.
#[inline]
pub(crate) fn reject_unknown_seaorm_backend(backend: sea_orm::DatabaseBackend) -> ! {
    panic!("BookclerkSQL adapters do not support SeaORM backend {backend:?}")
}

impl PhysicalEngine {
    /// SQLite physical engine.
    #[must_use]
    pub const fn sqlite() -> Self {
        Self::Sqlite
    }

    /// PostgreSQL physical engine.
    #[must_use]
    pub const fn postgres() -> Self {
        Self::Postgres
    }

    /// Cloudflare D1 physical engine.
    #[must_use]
    pub const fn d1() -> Self {
        Self::D1
    }

    /// Adapter-only: the backend of a connection this adapter opened.
    ///
    /// # Panics
    ///
    /// Panics on SeaORM backends that are not BookclerkSQL engines (including
    /// MySQL and future unknown variants).
    #[must_use]
    pub fn from_adapter_backend(backend: sea_orm::DatabaseBackend) -> Self {
        match backend {
            sea_orm::DatabaseBackend::Postgres => Self::Postgres,
            sea_orm::DatabaseBackend::Sqlite => Self::Sqlite,
            other => reject_unknown_seaorm_backend(other),
        }
    }

    /// SeaORM backend used for physical SQL and typed binds.
    #[must_use]
    pub const fn backend(self) -> sea_orm::DatabaseBackend {
        match self {
            Self::Postgres => sea_orm::DatabaseBackend::Postgres,
            Self::Sqlite | Self::D1 => sea_orm::DatabaseBackend::Sqlite,
        }
    }

    /// Timing source label for this physical engine.
    #[must_use]
    pub const fn timing_source(self) -> &'static str {
        match self {
            Self::Postgres => "postgres_txn",
            Self::D1 => "d1_txn",
            Self::Sqlite => "sqlite_txn",
        }
    }
}

/// How leftover host SQL reaches a SeaORM connection.
///
/// [`Self::Canonical`] is sqlite-shaped transport with no physical lowering
/// (production plugin-host proxy). [`Self::InProcess`] lowers exactly once
/// using the engine of the opened adapter connection (native sqlite / postgres
/// tests). Host/library code must not name [`PhysicalEngine`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SqlExecTarget {
    /// Canonical `?` SQL. Adapters lower after RPC.
    #[default]
    Canonical,
    /// In-process adapter connection. Infer the engine from the connection
    /// and lower once.
    InProcess,
}

impl SqlExecTarget {
    /// True when leftover SQL is physically lowered by the adapter SDK.
    #[must_use]
    pub const fn lowers(self) -> bool {
        matches!(self, Self::InProcess)
    }
}

/// Session-level cancel / deadline for one atomic attempt (not hashed).
#[derive(Clone, Default)]
pub struct AtomicSession {
    /// Host RPC cancel flag, when the guest shares a process with the host.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Guest-visible deadline (`deadlineUnixMs` on the wire).
    pub deadline_unix_ms: Option<u64>,
    /// Extra catalog snapshot merged before typing host-authored SQL
    /// (canonical host schema). Empty for plugin-binding execute.
    pub type_env: SqlTypeEnv,
}

impl AtomicSession {
    /// Builds session control from a wire `deadlineUnixMs`.
    #[must_use]
    pub fn from_deadline(deadline_unix_ms: Option<u64>) -> Self {
        Self {
            cancel: None,
            deadline_unix_ms,
            type_env: SqlTypeEnv::new(),
        }
    }

    /// Attaches a host cancel flag (job fence) to this session.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Option<Arc<AtomicBool>>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Merges `env` (typically the canonical host schema) into this session so
    /// host DML is typed against library tables, not the plugin catalog.
    #[must_use]
    pub fn with_type_env(mut self, env: SqlTypeEnv) -> Self {
        self.type_env = env;
        self
    }

    /// Checks cancel / deadline / test inject at `phase`.
    ///
    /// # Errors
    ///
    /// Returns when the session is cancelled, past deadline, or a test inject fires.
    pub(crate) fn check(&self, phase: AtomicInterruptPhase) -> Result<(), DbErr> {
        if let Some(kind) = consume_atomic_interrupt(phase) {
            return Err(interrupt_err(phase, kind));
        }
        let cancelled = self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst));
        let expired = self.deadline_unix_ms.is_some_and(|ms| unix_now_ms() >= ms);
        if cancelled {
            return Err(interrupt_err(phase, AtomicInterruptKind::Cancel));
        }
        if expired {
            return Err(interrupt_err(phase, AtomicInterruptKind::Deadline));
        }
        Ok(())
    }
}

/// Per-statement result bounds. Row/byte `0` means unlimited at execute time.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecCaps {
    /// Maximum rows a statement may return (`0` = unlimited).
    pub max_result_rows: u32,
    /// Maximum JSON bytes of one statement's rows (`0` = unlimited).
    pub max_result_bytes: u32,
    /// Maximum UTF-8 / blob bytes of one cell (`0` = unlimited).
    pub max_cell_bytes: u32,
    /// Maximum encoded bytes of the whole atomic reply (`0` = unlimited).
    pub max_atomic_result_bytes: u32,
}

impl From<u32> for ExecCaps {
    fn from(max_result_rows: u32) -> Self {
        Self {
            max_result_rows,
            max_result_bytes: 0,
            max_cell_bytes: 0,
            max_atomic_result_bytes: 0,
        }
    }
}

impl ExecCaps {
    /// Copies negotiated capability limits into the executor.
    #[must_use]
    pub fn from_capabilities(caps: &DbCapabilities) -> Self {
        Self {
            max_result_rows: caps.max_result_rows,
            max_result_bytes: caps.max_result_bytes,
            max_cell_bytes: caps.max_cell_bytes,
            max_atomic_result_bytes: caps.max_atomic_result_bytes,
        }
    }
}

/// Maps a session interrupt onto a guest-classifiable [`DbErr`].
fn interrupt_err(phase: AtomicInterruptPhase, kind: AtomicInterruptKind) -> DbErr {
    let around_commit = matches!(phase, AtomicInterruptPhase::AroundCommit);
    let msg = match (around_commit, kind) {
        (true, _) => "database commit failed: session interrupt at commit",
        (false, AtomicInterruptKind::Cancel) => "cancelled: atomic session cancelled",
        (false, AtomicInterruptKind::Deadline) => "deadline_exceeded: atomic deadline elapsed",
    };
    DbErr::Custom(msg.into())
}

/// Current unix time in milliseconds.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Remaining milliseconds until `deadline_unix_ms` (`None` if unlimited).
pub(crate) fn remaining_deadline_ms(deadline_unix_ms: Option<u64>) -> Option<u64> {
    let dl = deadline_unix_ms?;
    Some(dl.saturating_sub(unix_now_ms()).max(1))
}

/// Wraps a host-tagged [`bookclerk_plugin_abi::DbPlanStatementKind::Select`] so the engine stops
/// after `cap + 1` rows. Callers must not pass DML `RETURNING` SQL.
#[must_use]
pub fn cap_query_sql(sql: &str, max_result_rows: u32) -> String {
    if max_result_rows == 0 {
        return sql.to_string();
    }
    let n = u64::from(max_result_rows) + 1;
    let inner = sql.trim().trim_end_matches(';');
    format!("SELECT * FROM ({inner}) AS _bc_cap LIMIT {n}")
}

/// Collects at most `max_result_rows + 1` engine rows (no JSON conversion).
///
/// PostgreSQL is streamed. SQLite goes through the rusqlite proxy, which stops
/// the cursor at the same cap (and records positional column metadata).
///
/// # Errors
///
/// Returns when the engine stream/query fails.
pub(crate) async fn collect_capped_query_results<C>(
    conn: &C,
    stmt: Statement,
    max_result_rows: u32,
) -> Result<Vec<QueryResult>, DbErr>
where
    C: ConnectionTrait + StreamTrait,
{
    let stop_after = row_stop_after(max_result_rows);
    if ConnectionTrait::get_database_backend(conn) == sea_orm::DatabaseBackend::Postgres {
        let stream = conn.stream_raw(stmt).await?;
        futures::pin_mut!(stream);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await? {
            let _ = note_query_row();
            rows.push(row);
            if rows.len() >= stop_after {
                break;
            }
        }
        return Ok(rows);
    }
    let rows = conn.query_all_raw(stmt).await?;
    Ok(rows.into_iter().take(stop_after).collect())
}

/// Uniform `rowsAffected` by host-authored kind.
pub(crate) fn rows_affected_for_kind(kind: DbPlanStatementKind, returned_rows: usize) -> u64 {
    match kind {
        DbPlanStatementKind::Select => 0,
        DbPlanStatementKind::Returning => u64::try_from(returned_rows).unwrap_or(u64::MAX),
        DbPlanStatementKind::Execute => 0,
    }
}

/// Fetch one extra row past a positive cap so overflow can fail closed.
fn row_stop_after(max_result_rows: u32) -> usize {
    if max_result_rows == 0 {
        usize::MAX
    } else {
        usize::try_from(max_result_rows)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    }
}

/// UTF-8 / JSON length of one cell used for `maxCellBytes`.
pub fn json_cell_utf8_len(v: &JsonValue) -> usize {
    match v {
        JsonValue::String(s) => s.len(),
        JsonValue::Array(_) | JsonValue::Object(_) => v.to_string().len(),
        _ => 0,
    }
}

/// Adds `extra` encoded JSON bytes toward `max_result_bytes`.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when the running total would exceed the cap.
pub fn note_encoded_result_bytes(
    used: &mut usize,
    extra: usize,
    max_result_bytes: u32,
) -> Result<(), DbErr> {
    if max_result_bytes == 0 {
        return Ok(());
    }
    *used = used.saturating_add(extra);
    let cap = usize::try_from(max_result_bytes).unwrap_or(usize::MAX);
    if *used > cap {
        return Err(DbErr::Custom(format!(
            "query result is {used} bytes; maxResultBytes is {max_result_bytes}"
        )));
    }
    Ok(())
}

/// Fails closed when `n` exceeds a positive `max_result_rows` cap.
pub(crate) fn exceeds_result_row_cap(n: usize, max_result_rows: u32) -> bool {
    if max_result_rows == 0 {
        return false;
    }
    let cap = usize::try_from(max_result_rows).unwrap_or(usize::MAX);
    n > cap
}

/// Maps a SeaORM cell onto JSON for plan interpretation.
#[must_use]
pub fn sea_value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Bool(Some(b)) => JsonValue::Bool(*b),
        Value::TinyInt(Some(n)) => JsonValue::from(*n),
        Value::SmallInt(Some(n)) => JsonValue::from(*n),
        Value::Int(Some(n)) => JsonValue::from(*n),
        Value::BigInt(Some(n)) => JsonValue::from(*n),
        Value::TinyUnsigned(Some(n)) => JsonValue::from(*n),
        Value::SmallUnsigned(Some(n)) => JsonValue::from(*n),
        Value::Unsigned(Some(n)) => JsonValue::from(*n),
        Value::BigUnsigned(Some(n)) => JsonValue::from(*n),
        Value::Float(Some(n)) => JsonValue::from(f64::from(*n)),
        Value::Double(Some(n)) => JsonValue::from(*n),
        Value::String(Some(s)) => JsonValue::String(s.clone()),
        Value::Char(Some(c)) => JsonValue::String(c.to_string()),
        Value::Bytes(Some(b)) => JsonValue::String(crate::bytes_to_b64_string(b)),
        _ => JsonValue::Null,
    }
}

/// Encoded JSON length of a SeaORM proxy row (keys, numbers, punctuation).
#[must_use]
pub fn encoded_proxy_row_len<'a>(
    values: impl IntoIterator<Item = (&'a String, &'a Value)>,
) -> usize {
    let mut map = serde_json::Map::new();
    for (name, value) in values {
        map.insert(name.clone(), sea_value_to_json(value));
    }
    JsonValue::Object(map).to_string().len()
}

/// Executes canonical SQL on a physical engine the adapter opened.
///
/// Does **not** run host semantic desugars. Callers must pass already-canonical
/// SQL (`?` placeholders). Host/library code must not call this helper.
///
/// # Errors
///
/// Returns when the engine rejects the lowered statement.
pub async fn execute_physical_sql<C>(
    engine: PhysicalEngine,
    db: &C,
    sql: &str,
    values: impl IntoIterator<Item = Value>,
) -> Result<ExecResult, DbErr>
where
    C: ConnectionTrait,
{
    let backend = engine.backend();
    let lowered = crate::lower_canonical_sql(backend, sql);
    db.execute_raw(Statement::from_sql_and_values(backend, lowered, values))
        .await
}

/// Queries canonical SQL on a physical engine the adapter opened.
///
/// # Errors
///
/// Returns when the engine rejects the lowered statement.
pub async fn query_physical_sql<C>(
    engine: PhysicalEngine,
    db: &C,
    sql: &str,
    values: impl IntoIterator<Item = Value>,
) -> Result<Vec<QueryResult>, DbErr>
where
    C: ConnectionTrait,
{
    query_physical_sql_typed(engine, db, sql, None, values).await
}

/// [`query_physical_sql`] with an optional hash-bound proof (TEXT collate /
/// INTEGER overflow on Postgres).
///
/// # Errors
///
/// Returns when the proof is not bound to `sql` or the engine rejects the work.
pub async fn query_physical_sql_typed<C>(
    engine: PhysicalEngine,
    db: &C,
    sql: &str,
    proof: Option<&bookclerk_plugin_abi::ResolvedStatement>,
    values: impl IntoIterator<Item = Value>,
) -> Result<Vec<QueryResult>, DbErr>
where
    C: ConnectionTrait,
{
    let backend = engine.backend();
    let lowered = crate::lower_canonical_sql_typed(backend, sql, proof)
        .map_err(|err| DbErr::Custom(err.to_string()))?;
    db.query_all_raw(Statement::from_sql_and_values(backend, lowered, values))
        .await
}

#[cfg(test)]
mod tests {
    use super::cap_query_sql;
    use bookclerk_plugin_abi::DbPlanStatementKind;
    use sea_orm::Value;

    #[test]
    fn cap_query_sql_wraps_readonly_select() {
        let sql = cap_query_sql("SELECT x FROM t", 5);
        assert!(sql.contains("LIMIT 6"), "{sql}");
        assert!(sql.contains("AS _bc_cap"), "{sql}");
    }

    #[test]
    fn only_select_kind_requests_limit_wrap() {
        assert!(DbPlanStatementKind::Select.wrap_select_limit());
        assert!(!DbPlanStatementKind::Returning.wrap_select_limit());
        assert!(!DbPlanStatementKind::Execute.wrap_select_limit());
    }

    #[test]
    fn encoded_proxy_row_counts_keys_and_numbers() {
        use super::{encoded_proxy_row_len, sea_value_to_json};
        let alias = format!("c00_{}", "x".repeat(40));
        let values = [(alias.clone(), Value::BigInt(Some(1)))];
        let nbytes = encoded_proxy_row_len(values.iter().map(|(k, v)| (k, v)));
        let mut map = serde_json::Map::new();
        map.insert(alias.clone(), sea_value_to_json(&values[0].1));
        assert_eq!(nbytes, serde_json::Value::Object(map).to_string().len());
        assert!(
            nbytes > alias.len(),
            "JSON punctuation and the numeric cell must count: {nbytes} vs alias {}",
            alias.len()
        );
    }

    #[test]
    fn sql_exec_target_separates_canonical_proxy_from_in_process() {
        use super::{PhysicalEngine, SqlExecTarget};
        assert!(!SqlExecTarget::Canonical.lowers());
        assert!(SqlExecTarget::InProcess.lowers());
        assert_eq!(PhysicalEngine::postgres().timing_source(), "postgres_txn");
        assert_eq!(PhysicalEngine::sqlite().timing_source(), "sqlite_txn");
        assert_eq!(PhysicalEngine::d1().timing_source(), "d1_txn");
        assert_eq!(
            PhysicalEngine::d1().backend(),
            sea_orm::DatabaseBackend::Sqlite
        );
    }

    #[test]
    fn canonical_sql_keeps_question_marks_until_postgres_lower() {
        use sea_orm::DatabaseBackend;
        let sql = "SELECT id FROM t WHERE a = ? AND b IN (?, ?)";
        assert_eq!(
            crate::lower_canonical_sql(DatabaseBackend::Postgres, sql),
            "SELECT id FROM t WHERE a = $1 AND b IN ($2, $3)"
        );
        assert_eq!(
            crate::lower_canonical_sql(DatabaseBackend::Sqlite, sql),
            sql
        );
    }
}
