//! Host-mediated Cloudflare-style SQL binding over typed `execute`.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bookclerk_plugin_abi::{
    encoded_execute_request_bytes, guest_statement_kind, DbPlanStatementKind, DbResultSelection,
    DbTiming, DbValue, ExecuteReply, ExecuteRequest, PluginError, Result, StatementResult,
    TypedDbStatement,
};
use bookclerk_plugin_abi::{GuestDatabase, JobHandlerContext};

static OP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Explicit retry identity: reuse both `operationId` and `requestHash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryToken {
    /// Caller-chosen idempotency key.
    pub operation_id: String,
    /// Canonical Cap'n request hash stamped by the host.
    pub request_hash: String,
}

/// Cloudflare [`D1Result::meta`](https://developers.cloudflare.com/d1/worker-api/return-object/) shape.
#[derive(Debug, Clone, PartialEq)]
pub struct D1Meta {
    /// Engine-reported SQL duration in milliseconds.
    pub duration: f64,
    /// Rows changed by the statement (`rowsAffected`).
    pub changes: u64,
    /// Last inserted row id when the adapter exposes it (else `0`).
    pub last_row_id: i64,
    /// `true` when `changes > 0`.
    pub changed_db: bool,
    /// Rows returned to the guest for this statement.
    pub rows_read: u32,
    /// Rows written (`changes` for DML).
    pub rows_written: u64,
}

/// Cloudflare [`D1Result`](https://developers.cloudflare.com/d1/worker-api/return-object/) projection.
#[derive(Debug, Clone, PartialEq)]
pub struct D1Result {
    /// Always `true` on success; errors are returned as `Result::Err`.
    pub success: bool,
    /// Row maps for selects; `None` when not applicable (DML).
    pub results: Option<Vec<HashMap<String, DbValue>>>,
    /// Cloudflare-shaped timing and change metadata.
    pub meta: D1Meta,
}

/// Cloudflare [`D1ExecResult`](https://developers.cloudflare.com/d1/worker-api/return-object/).
#[derive(Debug, Clone, PartialEq)]
pub struct D1ExecResult {
    /// Number of statements executed (one per non-empty `exec` line).
    pub count: u32,
    /// Total duration in milliseconds.
    pub duration: f64,
}

/// Options for [`DatabaseBinding`].
#[derive(Debug, Clone, Default)]
pub struct DatabaseBindingOptions {
    /// Negotiated `maxRequestBytes` (`0` = unlimited).
    pub max_request_bytes: u32,
    /// Default `maxRows` for [`PreparedStatement::all`] (`0` = host adapter cap).
    pub max_result_rows: u32,
    /// Default retry token.
    pub retry: Option<RetryToken>,
    /// Guest-visible deadline (unix ms).
    pub deadline_unix_ms: u64,
}

#[derive(Clone, Copy)]
struct TerminalIntent {
    selection: DbResultSelection,
    max_rows: u32,
}

/// Host-mediated typed SQL surface for plugin guests.
#[derive(Clone)]
pub struct DatabaseBinding {
    session: Arc<dyn GuestDatabase>,
    options: DatabaseBindingOptions,
}

impl DatabaseBinding {
    /// Wraps a host-granted [`GuestDatabase`].
    #[must_use]
    pub fn from_session(session: Arc<dyn GuestDatabase>) -> Self {
        Self::from_session_with(session, DatabaseBindingOptions::default())
    }

    /// Wraps a host-granted session with request-budget knobs.
    #[must_use]
    pub fn from_session_with(
        session: Arc<dyn GuestDatabase>,
        options: DatabaseBindingOptions,
    ) -> Self {
        Self { session, options }
    }

    /// Takes [`JobHandlerContext::database`] when the invocation includes one.
    ///
    /// Production jobs do **not** inject the host library here. Durable plugin
    /// state uses [`Self::take_named_from_job_context`].
    #[must_use]
    pub fn take_from_job_context(ctx: &mut JobHandlerContext) -> Option<Self> {
        ctx.database
            .take()
            .map(|db| Self::from_session(Arc::from(db)))
    }

    /// Takes the named plugin database binding `name` from a job context.
    ///
    /// Named bindings come from `plugin.toml` `capabilities.bindings.databases`
    /// after operator approval: each is an isolated plugin-owned database
    /// (full DML plus idempotent `CREATE`/`DROP` `TABLE`/`INDEX` with
    /// `IF [NOT] EXISTS`), physically separate from the Bookclerk library and
    /// from every other plugin.
    #[must_use]
    pub fn take_named_from_job_context(ctx: &mut JobHandlerContext, name: &str) -> Option<Self> {
        ctx.take_named_database(name)
            .map(|db| Self::from_session(Arc::from(db)))
    }

    /// Prepare one canonical-SQL statement (`?` placeholders).
    #[must_use]
    pub fn prepare(&self, sql: impl Into<String>) -> PreparedStatement {
        PreparedStatement {
            binding: self.clone(),
            sql: sql.into(),
            parameters: Vec::new(),
            intent: Some(default_terminal_intent(&self.options)),
        }
    }

    /// Run prepared statements as one typed atomic batch.
    ///
    /// # Errors
    ///
    /// Returns when a statement is missing terminal intent, the encoded
    /// request exceeds `maxRequestBytes`, or `execute` fails.
    pub async fn batch(
        &self,
        statements: Vec<PreparedStatement>,
        retry: Option<RetryToken>,
    ) -> Result<Vec<D1Result>> {
        let reply = self.batch_reply(statements, retry).await?;
        Ok(execute_reply_to_d1_results(&reply))
    }

    /// Execute raw SQL without bind parameters (Cloudflare `D1Database::exec`).
    ///
    /// Bookclerk accepts one or more newline-separated statements like D1.
    /// Each non-empty line is executed as one prepared statement in a single
    /// typed atomic batch.
    ///
    /// # Errors
    ///
    /// Returns when the query is empty or `batch` fails.
    pub async fn exec(
        &self,
        query: impl Into<String>,
        retry: Option<RetryToken>,
    ) -> Result<D1ExecResult> {
        let queries = split_exec_queries(&query.into());
        if queries.is_empty() {
            return Err(PluginError::invalid_params("exec query is empty"));
        }
        let mut prepared = Vec::with_capacity(queries.len());
        for sql in &queries {
            prepared.push(self.prepare(sql.clone()));
        }
        let results = self.batch(prepared, retry).await?;
        let duration: f64 = results.iter().map(|r| r.meta.duration).sum();
        Ok(D1ExecResult {
            count: u32::try_from(queries.len()).unwrap_or(u32::MAX),
            duration,
        })
    }

    /// Internal typed-batch transport. Prefer [`Self::prepare`].
    ///
    /// # Errors
    ///
    /// Returns when the batch is empty, the encoded request exceeds the
    /// negotiated cap, or `execute` fails.
    pub async fn execute(
        &self,
        batch: Vec<TypedDbStatement>,
        retry: Option<RetryToken>,
    ) -> Result<ExecuteReply> {
        if batch.is_empty() {
            return Err(PluginError::invalid_params(
                "execute statements must be non-empty",
            ));
        }
        let token = retry.or_else(|| self.options.retry.clone());
        let (operation_id, request_hash) = match token {
            Some(t) => (t.operation_id, t.request_hash),
            None => (new_operation_id(), String::new()),
        };
        let request = ExecuteRequest {
            operation_id,
            request_hash,
            statements: batch,
            deadline_unix_ms: self.options.deadline_unix_ms,
        };
        let encoded = encoded_execute_request_bytes(&request)?;
        let cap = self.options.max_request_bytes;
        if cap > 0 && encoded.len() > cap as usize {
            return Err(PluginError::payload_too_large(format!(
                "atomic request is {} bytes; guest maxRequestBytes is {cap}",
                encoded.len()
            )));
        }
        self.session.execute(request).await
    }

    /// # Errors
    ///
    /// Returns when a statement is missing terminal intent or `execute` fails.
    async fn batch_reply(
        &self,
        statements: Vec<PreparedStatement>,
        retry: Option<RetryToken>,
    ) -> Result<ExecuteReply> {
        let mut typed = Vec::with_capacity(statements.len());
        for stmt in &statements {
            typed.push(stmt.as_typed()?);
        }
        self.execute(typed, retry).await
    }
}

/// Cloudflare-style prepared statement.
#[derive(Clone)]
pub struct PreparedStatement {
    binding: DatabaseBinding,
    sql: String,
    parameters: Vec<DbValue>,
    intent: Option<TerminalIntent>,
}

impl PreparedStatement {
    /// Replace bound parameters.
    #[must_use]
    pub fn bind(mut self, values: Vec<DbValue>) -> Self {
        self.parameters = values;
        self
    }

    /// Mark DML intent for [`DatabaseBinding::batch`].
    #[must_use]
    pub fn as_run(mut self) -> Self {
        self.intent = Some(TerminalIntent {
            selection: DbResultSelection::AffectedRows,
            max_rows: 0,
        });
        self
    }

    /// Mark `maxRows = 1` row intent for [`DatabaseBinding::batch`].
    ///
    /// Preserves the caller SQL; the host enforces the one-row bound at execution.
    #[must_use]
    pub fn as_first(mut self) -> Self {
        self.intent = Some(TerminalIntent {
            selection: DbResultSelection::Rows,
            max_rows: 1,
        });
        self
    }

    /// Mark row-returning intent for [`DatabaseBinding::batch`].
    #[must_use]
    pub fn as_all(mut self) -> Self {
        self.intent = Some(TerminalIntent {
            selection: DbResultSelection::Rows,
            max_rows: self.binding.options.max_result_rows,
        });
        self
    }

    /// Execute as Cloudflare `run()` (functionally equivalent to [`Self::all`]).
    ///
    /// # Errors
    ///
    /// Returns when `execute` fails.
    pub async fn run(self, retry: Option<RetryToken>) -> Result<D1Result> {
        self.all(retry).await
    }

    /// First row as a name→value map, or `None` (Cloudflare `first()` without `colName`).
    ///
    /// # Errors
    ///
    /// Returns when `execute` fails.
    pub async fn first(self, retry: Option<RetryToken>) -> Result<Option<Vec<(String, DbValue)>>> {
        let bound = self.as_first();
        let binding = bound.binding.clone();
        let reply = binding.batch_reply(vec![bound], retry).await?;
        let Some(result) = reply.statements.into_iter().next() else {
            return Ok(None);
        };
        let Some(row) = row_map_from_statement(&result) else {
            return Ok(None);
        };
        Ok(Some(row.into_iter().collect()))
    }

    /// One column from the first row, or `None` when no row exists.
    ///
    /// # Errors
    ///
    /// Returns when `execute` fails or the named column is absent from the row
    /// (Cloudflare `first(colName)` throws `D1_ERROR` in that case).
    pub async fn first_column(
        self,
        col_name: &str,
        retry: Option<RetryToken>,
    ) -> Result<Option<DbValue>> {
        let bound = self.as_first();
        let binding = bound.binding.clone();
        let reply = binding.batch_reply(vec![bound], retry).await?;
        let Some(result) = reply.statements.into_iter().next() else {
            return Ok(None);
        };
        let Some(row) = row_map_from_statement(&result) else {
            return Ok(None);
        };
        Ok(Some(column_value_from_row(&row, col_name)?))
    }

    /// Positional cell values per row (Cloudflare `raw()`).
    ///
    /// # Errors
    ///
    /// Returns when `execute` fails.
    pub async fn raw(self, retry: Option<RetryToken>) -> Result<Vec<Vec<DbValue>>> {
        let bound = self.as_all();
        let binding = bound.binding.clone();
        let reply = binding.batch_reply(vec![bound], retry).await?;
        let Some(result) = reply.statements.into_iter().next() else {
            return Ok(Vec::new());
        };
        Ok(result.rows.into_iter().map(|r| r.values).collect())
    }

    /// Execute as a row-returning query. Returns a Cloudflare-shaped [`D1Result`].
    ///
    /// # Errors
    ///
    /// Returns when `execute` fails.
    pub async fn all(self, retry: Option<RetryToken>) -> Result<D1Result> {
        let bound = self.as_all();
        let binding = bound.binding.clone();
        let reply = binding.batch_reply(vec![bound], retry).await?;
        let stmt = reply
            .statements
            .into_iter()
            .next()
            .ok_or_else(|| PluginError::internal("execute reply missing statement result"))?;
        Ok(statement_result_to_d1_result(&stmt, &reply.timing))
    }

    /// # Errors
    ///
    /// Returns [`PluginError::invalid_params`] when terminal intent is missing.
    fn as_typed(&self) -> Result<TypedDbStatement> {
        let intent = self
            .intent
            .unwrap_or_else(|| default_terminal_intent(&self.binding.options));
        let kind = match intent.selection {
            DbResultSelection::AffectedRows | DbResultSelection::Discard => {
                DbPlanStatementKind::Execute
            }
            _ => guest_statement_kind(&self.sql),
        };
        Ok(TypedDbStatement {
            sql: self.sql.clone(),
            parameters: self.parameters.clone(),
            kind,
            max_rows: intent.max_rows,
            result_selection: intent.selection,
        })
    }
}

fn default_terminal_intent(options: &DatabaseBindingOptions) -> TerminalIntent {
    TerminalIntent {
        selection: DbResultSelection::Rows,
        max_rows: options.max_result_rows,
    }
}

fn split_exec_queries(query: &str) -> Vec<String> {
    query
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.trim_end_matches(';').trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Resolves a column value from a row map, matching case-insensitively.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `col_name` is absent from the row.
fn column_value_from_row(row: &HashMap<String, DbValue>, col_name: &str) -> Result<DbValue> {
    if let Some(v) = row.get(col_name) {
        return Ok(v.clone());
    }
    let lower = col_name.to_ascii_lowercase();
    for (k, v) in row {
        if k.eq_ignore_ascii_case(&lower) {
            return Ok(v.clone());
        }
    }
    Err(PluginError::invalid_params(format!(
        "column {col_name} not found in first() result"
    )))
}

/// Maps one typed statement result to Cloudflare [`D1Result`].
#[must_use]
pub fn statement_result_to_d1_result(stmt: &StatementResult, timing: &DbTiming) -> D1Result {
    let changes = stmt.rows_affected;
    let duration_ms = timing.db_execution_us as f64 / 1000.0;
    let results = if stmt.columns.is_empty() {
        None
    } else {
        Some(
            stmt.rows
                .iter()
                .map(|row| {
                    stmt.columns
                        .iter()
                        .zip(&row.values)
                        .map(|(c, v)| (c.name.clone(), v.clone()))
                        .collect()
                })
                .collect(),
        )
    };
    D1Result {
        success: true,
        results,
        meta: D1Meta {
            duration: duration_ms,
            changes,
            last_row_id: 0,
            changed_db: changes > 0,
            rows_read: stmt.rows.len() as u32,
            rows_written: changes,
        },
    }
}

/// Maps a typed execute reply to Cloudflare [`D1Result`] per statement.
#[must_use]
pub fn execute_reply_to_d1_results(reply: &ExecuteReply) -> Vec<D1Result> {
    reply
        .statements
        .iter()
        .map(|stmt| statement_result_to_d1_result(stmt, &reply.timing))
        .collect()
}

fn row_map_from_statement(result: &StatementResult) -> Option<HashMap<String, DbValue>> {
    let row = result.rows.first()?;
    Some(
        result
            .columns
            .iter()
            .zip(&row.values)
            .map(|(c, v)| (c.name.clone(), v.clone()))
            .collect(),
    )
}

fn new_operation_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("op-{now}-{}", OP_SEQ.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::GuestDatabase;
    use bookclerk_plugin_abi::PluginErrorCode;
    use std::sync::Mutex;

    struct RecordingSession {
        last: Mutex<Option<ExecuteRequest>>,
    }

    #[async_trait::async_trait(?Send)]
    impl GuestDatabase for RecordingSession {
        async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteReply> {
            let n = request.statements.len();
            *self.last.lock().expect("lock") = Some(request);
            Ok(ExecuteReply {
                operation_id: "op".into(),
                statements: (0..n)
                    .map(|_| bookclerk_plugin_abi::StatementResult {
                        rows: vec![bookclerk_plugin_abi::DbRow {
                            values: vec![DbValue::Int64(1)],
                        }],
                        columns: vec![bookclerk_plugin_abi::DbColumn {
                            name: "n".into(),
                            db_type: bookclerk_plugin_abi::DbType::Int64,
                        }],
                        rows_affected: 1,
                    })
                    .collect(),
                timing: bookclerk_plugin_abi::DbTiming {
                    attempt_elapsed_us: 0,
                    db_execution_us: 0,
                    db_timing_source: "test".into(),
                },
            })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn d1_result_projection_and_first_column() {
        let session = Arc::new(RecordingSession {
            last: Mutex::new(None),
        });
        let binding = DatabaseBinding::from_session(session.clone());
        let all = binding.prepare("SELECT n FROM t").all(None).await.unwrap();
        assert!(all.success);
        assert_eq!(all.meta.rows_read, 1);
        let results = all.results.expect("results");
        assert_eq!(results[0].get("n"), Some(&DbValue::Int64(1)));

        let col = binding
            .prepare("SELECT n FROM t")
            .first_column("n", None)
            .await
            .unwrap()
            .expect("column");
        assert_eq!(col, DbValue::Int64(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_run_and_batch_stamp_terminal_intent() {
        let session = Arc::new(RecordingSession {
            last: Mutex::new(None),
        });
        let binding = DatabaseBinding::from_session(session.clone());
        let row = binding
            .prepare("SELECT n FROM t")
            .first(None)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row[0].0, "n");
        let req = session.last.lock().expect("lock").clone().expect("req");
        assert_eq!(req.statements[0].max_rows, 1);
        assert_eq!(req.statements[0].result_selection, DbResultSelection::Rows);
        assert_eq!(req.statements[0].sql, "SELECT n FROM t");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_and_all_stamp_rows_selection_for_select_and_dml() {
        let session = Arc::new(RecordingSession {
            last: Mutex::new(None),
        });
        let binding = DatabaseBinding::from_session(session.clone());
        binding
            .prepare("INSERT INTO t VALUES (?)")
            .bind(vec![DbValue::Int64(1)])
            .run(None)
            .await
            .unwrap();
        let req = session.last.lock().expect("lock").clone().expect("req");
        assert_eq!(req.statements[0].max_rows, 0);
        assert_eq!(req.statements[0].result_selection, DbResultSelection::Rows);
        assert_eq!(req.statements[0].kind, DbPlanStatementKind::Execute);

        binding.prepare("SELECT n FROM t").run(None).await.unwrap();
        let req = session.last.lock().expect("lock").clone().expect("req");
        assert_eq!(req.statements[0].result_selection, DbResultSelection::Rows);
        assert_eq!(req.statements[0].kind, DbPlanStatementKind::Select);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn batch_accepts_default_intent_and_optional_overrides() {
        let session = Arc::new(RecordingSession {
            last: Mutex::new(None),
        });
        let binding = DatabaseBinding::from_session(session.clone());
        binding
            .batch(vec![binding.prepare("SELECT n FROM t")], None)
            .await
            .unwrap();
        let req = session.last.lock().expect("lock").clone().expect("req");
        assert_eq!(req.statements[0].result_selection, DbResultSelection::Rows);

        binding
            .batch(
                vec![
                    binding
                        .prepare("INSERT INTO t VALUES (?)")
                        .bind(vec![DbValue::Int64(1)])
                        .as_run(),
                    binding.prepare("SELECT n FROM t").as_all(),
                ],
                None,
            )
            .await
            .unwrap();
        let req = session.last.lock().expect("lock").clone().expect("req");
        assert_eq!(
            req.statements[0].result_selection,
            DbResultSelection::AffectedRows
        );
        assert_eq!(req.statements[1].result_selection, DbResultSelection::Rows);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exec_runs_newline_separated_queries() {
        let session = Arc::new(RecordingSession {
            last: Mutex::new(None),
        });
        let binding = DatabaseBinding::from_session(session.clone());
        let out = binding
            .exec("INSERT INTO t VALUES (1)\nSELECT 1", None)
            .await
            .unwrap();
        assert_eq!(out.count, 2);
        let req = session.last.lock().expect("lock").clone().expect("req");
        assert_eq!(req.statements.len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_column_errors_when_column_missing() {
        let session = Arc::new(RecordingSession {
            last: Mutex::new(None),
        });
        let binding = DatabaseBinding::from_session(session.clone());
        let err = binding
            .prepare("SELECT 1 AS n")
            .first_column("missing", None)
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
    }

    #[test]
    fn dml_run_canonical_hash_matches_host_classification() {
        use bookclerk_plugin_abi::canonical_execute_request_hash;
        let req = ExecuteRequest {
            operation_id: String::new(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO t VALUES (?)".into(),
                parameters: vec![DbValue::Int64(1)],
                kind: guest_statement_kind("INSERT INTO t VALUES (?)"),
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        assert_eq!(req.statements[0].kind, DbPlanStatementKind::Execute);
        let hash = canonical_execute_request_hash(&req).expect("hash");
        assert_eq!(hash.len(), 64);
        assert_eq!(
            guest_statement_kind("INSERT INTO t VALUES (?) RETURNING id"),
            DbPlanStatementKind::Returning
        );
        let _ = hash;
    }
}
