//! Host-mediated Cloudflare-style SQL binding over typed `execute`.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bookclerk_plugin_abi::{
    encoded_execute_request_bytes, guest_statement_kind, DbCapabilities, DbColumn,
    DbPlanStatementKind, DbResultSelection, DbRow, DbTiming, DbValue, ExecuteReply, ExecuteRequest,
    ExecuteStreamBody, ExecuteStreamDecoder, ExecuteStreamFrame, ExecuteStreamLimits,
    LimitExceeded, PluginError, PluginErrorCode, Result, StatementResult, TypedDbStatement,
};
use bookclerk_plugin_abi::{Bindings, GuestDatabase};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Mutex;

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

/// Caps [`DatabaseBinding::chunk`] splits a batch by when the session's
/// limits are not to be used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChunkCaps {
    /// Statements per request (`0` = no cap).
    pub max_statements: u32,
    /// Encoded request bytes per request (`0` = no cap).
    pub max_request_bytes: u32,
}

/// Host-mediated typed SQL surface for plugin guests.
///
/// Row-returning reads go through `GuestDatabase.executeStreamed` whenever
/// the session advertises a streamed budget (`limits()`, fetched once per
/// binding), so a result past the inline caps still arrives whole; sessions
/// that do not stream, or answer `unsupported`, are read inline.
#[derive(Clone)]
pub struct DatabaseBinding {
    session: Arc<dyn GuestDatabase>,
    options: DatabaseBindingOptions,
    /// The session's limits, fetched once per binding; the lock is held
    /// across the fetch so overlapping first reads share it.
    limits: Arc<Mutex<LimitsCache>>,
}

/// What the binding knows about the session's limits.
#[derive(Debug, Clone)]
enum LimitsCache {
    /// `GuestDatabase.limits` has not been called yet.
    Unfetched,
    /// The call failed or the host predates it: reads go inline until an
    /// explicit [`DatabaseBinding::limits`] succeeds.
    Failed,
    /// The session's advertised capabilities.
    Ready(DbCapabilities),
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
        Self {
            session,
            options,
            limits: Arc::new(Mutex::new(LimitsCache::Unfetched)),
        }
    }

    /// Takes the named plugin database binding `name` from the `open`
    /// [`Bindings`].
    ///
    /// Named bindings come from `plugin.toml` `[[databases]]` after operator
    /// approval: each is an isolated plugin-owned database (full DML plus
    /// idempotent `CREATE`/`DROP` `TABLE`/`INDEX` with `IF [NOT] EXISTS`),
    /// physically separate from the Bookclerk library and from every other
    /// plugin. Production invocations never inject the host library.
    #[must_use]
    pub fn take_named_from_bindings(bindings: &mut Bindings, name: &str) -> Option<Self> {
        bindings
            .take_named_database(name)
            .map(|db| Self::from_session(Arc::from(db)))
    }

    /// The negotiated limits of the granted session (`DbCapabilities`).
    ///
    /// Use it to size a batch or a read before sending: `max_request_bytes`,
    /// `max_statements`, `max_result_bytes`, `max_result_rows` and the rest.
    ///
    /// # Errors
    ///
    /// Returns `unsupported` when the host session predates
    /// `GuestDatabase.limits`.
    pub async fn limits(&self) -> Result<DbCapabilities> {
        if let Some(caps) = self.session_limits().await {
            return Ok(caps);
        }
        // The cached fetch failed: let the host's own error surface, and
        // keep a later success so reads use the session's budget.
        let caps = self.session.limits().await?;
        *self.limits.lock().await = LimitsCache::Ready(caps.clone());
        Ok(caps)
    }

    /// The session's limits, fetched once per binding; `None` when the host
    /// predates `GuestDatabase.limits` or the call failed (read inline).
    async fn session_limits(&self) -> Option<DbCapabilities> {
        let mut cache = self.limits.lock().await;
        if matches!(*cache, LimitsCache::Unfetched) {
            *cache = match self.session.limits().await {
                Ok(caps) => LimitsCache::Ready(caps),
                Err(_) => LimitsCache::Failed,
            };
        }
        match &*cache {
            LimitsCache::Ready(caps) => Some(caps.clone()),
            LimitsCache::Unfetched | LimitsCache::Failed => None,
        }
    }

    /// Splits `statements` into batches that each fit one request.
    ///
    /// `for batch in db.chunk(stmts, None).await? { db.batch(batch, None).await?; }`.
    /// Each batch is atomic; the loop is not. The caps come from the
    /// session's limits (`max_statements`, and the tighter of the session's
    /// and the binding's `max_request_bytes`) unless `caps` is given; zero
    /// caps given explicitly mean one batch. A single statement larger than
    /// the request cap is yielded alone, so [`Self::batch`] reports the
    /// typed error. Batches are sized with the UUID-shaped retry id the
    /// binding mints; a caller-chosen retry id that is longer leaves that
    /// much less room.
    ///
    /// # Errors
    ///
    /// Returns `unsupported` when neither the session's limits nor `caps`
    /// are available, and [`PluginError::invalid_params`] when a statement
    /// is missing terminal intent.
    pub async fn chunk(
        &self,
        statements: Vec<PreparedStatement>,
        caps: Option<ChunkCaps>,
    ) -> Result<Vec<Vec<PreparedStatement>>> {
        let explicit = caps.is_some();
        let caps = match caps {
            Some(caps) => caps,
            // The session's request cap and the binding's own both apply to
            // `batch()`, so a batch is sized to the tighter one.
            None => match self.session_limits().await {
                Some(limits) => ChunkCaps {
                    max_statements: limits.max_statements,
                    max_request_bytes: tighter_cap(
                        limits.max_request_bytes,
                        self.options.max_request_bytes,
                    ),
                },
                None => ChunkCaps {
                    max_statements: 0,
                    max_request_bytes: self.options.max_request_bytes,
                },
            },
        };
        // Explicit zero caps mean "one batch"; only missing caps are an error.
        if !explicit && caps.max_statements == 0 && caps.max_request_bytes == 0 {
            return Err(PluginError::unsupported(
                "chunk needs the session limits (GuestDatabase.limits) or explicit caps",
            ));
        }
        let mut typed = Vec::with_capacity(statements.len());
        for stmt in &statements {
            typed.push(stmt.as_typed()?);
        }
        let runs = chunk_indexes(&typed, caps.max_statements, caps.max_request_bytes)?;
        let mut slots: Vec<Option<PreparedStatement>> = statements.into_iter().map(Some).collect();
        Ok(runs
            .into_iter()
            .map(|run| {
                run.into_iter()
                    // Every index is yielded exactly once by `chunk_indexes`.
                    .filter_map(|i| slots.get_mut(i).and_then(Option::take))
                    .collect()
            })
            .collect())
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
        let (request, session_caps) = self.request_for(batch, retry).await?;
        let Some(caps) = session_caps.filter(DbCapabilities::streams_results) else {
            return reply_for(&request, self.session.execute(request.clone()).await?);
        };
        // An `unsupported` answer (an older host, or a session that advertised
        // streaming it cannot serve) falls back to the inline read.
        let streamed = match self.session.execute_streamed(request.clone()).await {
            Ok(stream) => {
                stream
                    .collect(ExecuteStreamLimits::from_capabilities(&caps))
                    .await
            }
            Err(err) => Err(err),
        };
        match streamed {
            Err(err) if err.code == PluginErrorCode::Unsupported => {
                reply_for(&request, self.session.execute(request.clone()).await?)
            }
            other => reply_for(&request, other?),
        }
    }

    /// The request `batch` is sent as, and the session's limits.
    ///
    /// The retry token is `retry`, else the binding's default, else a fresh
    /// operation id. The encoded request is held to the tighter of the
    /// binding's and the session's `max_request_bytes`, so an oversized
    /// request fails locally with the typed error the host would raise (the
    /// same cap `chunk()` splits to).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::invalid_params`] when the batch is empty or
    /// cannot be encoded, and a typed `limit_exceeded` (`maxRequestBytes`)
    /// when the encoded request is over the cap.
    async fn request_for(
        &self,
        batch: Vec<TypedDbStatement>,
        retry: Option<RetryToken>,
    ) -> Result<(ExecuteRequest, Option<DbCapabilities>)> {
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
        let session_caps = self.session_limits().await;
        let cap = tighter_cap(
            self.options.max_request_bytes,
            session_caps
                .as_ref()
                .map_or(0, |caps| caps.max_request_bytes),
        );
        if cap > 0 && encoded.len() > cap as usize {
            return Err(PluginError::limit_exceeded(LimitExceeded::new(
                "maxRequestBytes",
                u64::from(cap),
                u64::try_from(encoded.len()).unwrap_or(u64::MAX),
            )));
        }
        Ok((request, session_caps))
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

    /// Rows one at a time from the same single-snapshot streamed read as
    /// [`Self::all`], without holding the whole result.
    ///
    /// ```ignore
    /// let mut rows = db.prepare("SELECT id, path FROM edition_files").iterate(None);
    /// while let Some(row) = rows.next().await {
    ///     let row = row?;
    ///     // ...
    /// }
    /// ```
    ///
    /// Nothing is sent until the first [`RowStream::next`]. On a session
    /// that streams, rows are decoded as their frames arrive and each is
    /// handed out once, so memory stays at one read window however large
    /// the result is. The read still fails closed (`limit_exceeded`) past
    /// the session's streamed budget, after the rows that fit were handed
    /// out. On a session that does not stream, or answers `unsupported`
    /// before any row arrived, the rows come from one inline read like
    /// [`Self::all`]. Dropping the stream stops reading it.
    #[must_use]
    pub fn iterate(self, retry: Option<RetryToken>) -> RowStream {
        let statement = self.as_all();
        let binding = statement.binding.clone();
        RowStream {
            binding,
            state: RowStreamState::Unopened {
                statement: Box::new(statement),
                retry,
            },
        }
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

/// Bytes [`RowStream`] reads from a streamed reply per step.
const ROW_STREAM_READ_BYTES: usize = 64 * 1024;

/// Rows of one read, handed out one at a time
/// ([`PreparedStatement::iterate`]).
///
/// Each row is a column name → value map, the shape [`D1Result::results`]
/// holds. The stream ends (`None`) after the last row, or after the first
/// error it yields.
pub struct RowStream {
    /// The binding the read goes through.
    binding: DatabaseBinding,
    /// Where the read is.
    state: RowStreamState,
}

impl std::fmt::Debug for RowStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            RowStreamState::Unopened { .. } => "unopened",
            RowStreamState::Inline { .. } => "inline",
            RowStreamState::Streamed(_) => "streamed",
            RowStreamState::Done => "done",
        };
        f.debug_struct("RowStream").field("state", &state).finish()
    }
}

/// Where a [`RowStream`] is.
enum RowStreamState {
    /// Nothing sent yet.
    Unopened {
        /// The read, with row-returning intent.
        statement: Box<PreparedStatement>,
        /// Retry identity for the read.
        retry: Option<RetryToken>,
    },
    /// Rows of an inline reply, handed out in order.
    Inline {
        /// Result columns.
        columns: Vec<DbColumn>,
        /// Rows not yet handed out.
        rows: std::vec::IntoIter<DbRow>,
    },
    /// A streamed reply being decoded.
    Streamed(Box<StreamedRows>),
    /// Every row was handed out, or an error was.
    Done,
}

/// The frames of a streamed reply in flight.
enum StreamedBody {
    /// Frames already in memory, read from `at`.
    Inline {
        /// Every frame.
        bytes: Vec<u8>,
        /// Bytes already fed to the decoder.
        at: usize,
    },
    /// Frames read on demand.
    Reader(Pin<Box<dyn AsyncRead + Send>>),
}

/// A streamed read: the decoder, which keeps no rows, and the rows it
/// handed out that the caller has not taken yet (at most one read window).
struct StreamedRows {
    /// The request, kept to check the reply and for the inline fallback.
    request: ExecuteRequest,
    /// The frames.
    body: StreamedBody,
    /// Read buffer for a reader body.
    buf: Vec<u8>,
    /// Streaming decoder; `None` once it has finished.
    decoder: Option<ExecuteStreamDecoder>,
    /// Columns of the statement whose rows are arriving.
    columns: Vec<DbColumn>,
    /// Rows decoded but not yet handed out.
    pending: VecDeque<DbRow>,
    /// Rows handed out so far, held to the statement's `max_rows`.
    yielded: usize,
}

/// What one step of a streamed read produced.
enum StreamStep {
    /// More rows may be pending; look again.
    Continue,
    /// The stream ended cleanly.
    End,
}

impl RowStream {
    /// The next row, `None` once the read is over.
    ///
    /// # Errors
    ///
    /// Yields (once, then `None`) the error the read fails with: a typed
    /// `limit_exceeded` past the session's budget, the adapter's error, an
    /// `unavailable` when the stream breaks off, or `invalid_params` when
    /// the reply does not answer the request or carries more rows than the
    /// statement's `max_rows`.
    pub async fn next(&mut self) -> Option<Result<HashMap<String, DbValue>>> {
        loop {
            match std::mem::replace(&mut self.state, RowStreamState::Done) {
                RowStreamState::Done => return None,
                RowStreamState::Unopened { statement, retry } => {
                    match self.open(&statement, retry).await {
                        Ok(state) => self.state = state,
                        Err(err) => return Some(Err(err)),
                    }
                }
                RowStreamState::Inline { columns, mut rows } => {
                    let row = rows.next()?;
                    let map = row_map(&columns, row);
                    self.state = RowStreamState::Inline { columns, rows };
                    return Some(Ok(map));
                }
                RowStreamState::Streamed(mut streamed) => {
                    if let Some(row) = streamed.pending.pop_front() {
                        streamed.yielded = streamed.yielded.saturating_add(1);
                        if let Err(err) = check_max_rows(&streamed.request, streamed.yielded) {
                            return Some(Err(err));
                        }
                        let map = row_map(&streamed.columns, row);
                        self.state = RowStreamState::Streamed(streamed);
                        return Some(Ok(map));
                    }
                    match streamed.step().await {
                        Ok(StreamStep::Continue) => {
                            self.state = RowStreamState::Streamed(streamed);
                        }
                        Ok(StreamStep::End) => return None,
                        // Nothing was handed out yet, so the read can still
                        // be served inline, as `all()` would be.
                        Err(err)
                            if err.code == PluginErrorCode::Unsupported
                                && streamed.yielded == 0 =>
                        {
                            match self.inline(streamed.request).await {
                                Ok(state) => self.state = state,
                                Err(err) => return Some(Err(err)),
                            }
                        }
                        Err(err) => return Some(Err(err)),
                    }
                }
            }
        }
    }

    /// Sends the read: streamed when the session advertises a streamed
    /// budget and serves it, inline otherwise.
    ///
    /// # Errors
    ///
    /// Returns the request's own errors (see [`DatabaseBinding::execute`])
    /// and the session's error for the read.
    async fn open(
        &self,
        statement: &PreparedStatement,
        retry: Option<RetryToken>,
    ) -> Result<RowStreamState> {
        let (request, caps) = self
            .binding
            .request_for(vec![statement.as_typed()?], retry)
            .await?;
        let Some(caps) = caps.filter(DbCapabilities::streams_results) else {
            return self.inline(request).await;
        };
        let stream = match self.binding.session.execute_streamed(request.clone()).await {
            Ok(stream) => stream,
            Err(err) if err.code == PluginErrorCode::Unsupported => {
                return self.inline(request).await;
            }
            Err(err) => return Err(err),
        };
        let body = match stream.into_body() {
            ExecuteStreamBody::Inline(bytes) => StreamedBody::Inline { bytes, at: 0 },
            ExecuteStreamBody::Reader(reader) => StreamedBody::Reader(reader),
        };
        Ok(RowStreamState::Streamed(Box::new(StreamedRows {
            request,
            body,
            buf: Vec::new(),
            decoder: Some(ExecuteStreamDecoder::streaming(
                ExecuteStreamLimits::from_capabilities(&caps),
            )),
            columns: Vec::new(),
            pending: VecDeque::new(),
            yielded: 0,
        })))
    }

    /// Reads `request` inline and hands its rows out.
    ///
    /// # Errors
    ///
    /// Returns the session's error, or [`PluginError::invalid_params`] when
    /// the reply does not answer `request`.
    async fn inline(&self, request: ExecuteRequest) -> Result<RowStreamState> {
        let reply = reply_for(
            &request,
            self.binding.session.execute(request.clone()).await?,
        )?;
        let Some(result) = reply.statements.into_iter().next() else {
            return Ok(RowStreamState::Done);
        };
        Ok(RowStreamState::Inline {
            columns: result.columns,
            rows: result.rows.into_iter(),
        })
    }
}

impl StreamedRows {
    /// Feeds the decoder one read window, queueing the rows it completes.
    ///
    /// # Errors
    ///
    /// Returns the decoder's error (a typed `limit_exceeded`, a malformed
    /// or out-of-sequence frame, or the adapter's `err` frame),
    /// [`PluginError::unavailable`] when the reader fails or the stream
    /// ends early, and [`PluginError::invalid_params`] when the head does
    /// not answer the request.
    async fn step(&mut self) -> Result<StreamStep> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(StreamStep::End);
        };
        // The body is read to its end even after the final frame, so bytes
        // trailing it are refused as `collect()` refuses them.
        let chunk: &[u8] = match &mut self.body {
            StreamedBody::Inline { bytes, at } => {
                let start = *at;
                let end = bytes.len().min(start.saturating_add(ROW_STREAM_READ_BYTES));
                *at = end;
                &bytes[start..end]
            }
            StreamedBody::Reader(reader) => {
                self.buf.resize(ROW_STREAM_READ_BYTES, 0);
                let n = reader.read(&mut self.buf).await.map_err(|err| {
                    PluginError::unavailable(format!("streamed reply read failed: {err}"))
                })?;
                &self.buf[..n]
            }
        };
        if chunk.is_empty() {
            // The body is exhausted: the decoder reports whether the reply
            // was whole.
            let Some(decoder) = self.decoder.take() else {
                return Ok(StreamStep::End);
            };
            decoder.finish()?;
            return Ok(StreamStep::End);
        }
        for frame in decoder.push_frames(chunk)? {
            match frame {
                ExecuteStreamFrame::Head {
                    operation_id,
                    statement_count,
                } => check_stream_head(&self.request, &operation_id, statement_count)?,
                ExecuteStreamFrame::Statement { columns, .. } => self.columns = columns,
                ExecuteStreamFrame::Rows(rows) => self.pending.extend(rows),
                // The decoder records the outcome; `finish` reports it.
                ExecuteStreamFrame::End(_) | ExecuteStreamFrame::Err(_) => {}
            }
        }
        Ok(StreamStep::Continue)
    }
}

/// Checks a streamed head answers `request`: the same operation id (a
/// request sent without one takes the id the host minted) and one result
/// per statement.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the head is for another
/// request.
fn check_stream_head(
    request: &ExecuteRequest,
    operation_id: &str,
    statement_count: u32,
) -> Result<()> {
    if !request.operation_id.is_empty() && operation_id != request.operation_id {
        return Err(PluginError::invalid_params(format!(
            "reply answers operation {operation_id}, not {}",
            request.operation_id
        )));
    }
    if usize::try_from(statement_count).ok() != Some(request.statements.len()) {
        return Err(PluginError::invalid_params(format!(
            "reply carries {statement_count} results for {} statements",
            request.statements.len()
        )));
    }
    Ok(())
}

/// Checks `yielded` rows are within the first statement's `max_rows`.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] past a non-zero `max_rows`.
fn check_max_rows(request: &ExecuteRequest, yielded: usize) -> Result<()> {
    let Some(statement) = request.statements.first() else {
        return Ok(());
    };
    let max_rows = usize::try_from(statement.max_rows).unwrap_or(usize::MAX);
    if statement.max_rows > 0 && yielded > max_rows {
        return Err(PluginError::invalid_params(format!(
            "reply carries more than {max_rows} rows for statement 0, which asked for at most {max_rows}"
        )));
    }
    Ok(())
}

/// One row as a column name → value map, taking the row's values.
fn row_map(columns: &[DbColumn], row: DbRow) -> HashMap<String, DbValue> {
    columns
        .iter()
        .map(|c| c.name.clone())
        .zip(row.values)
        .collect()
}

/// The smallest statement a request can carry, used to measure the others.
fn probe_statement() -> TypedDbStatement {
    TypedDbStatement {
        sql: String::new(),
        parameters: Vec::new(),
        kind: DbPlanStatementKind::Select,
        max_rows: 0,
        result_selection: DbResultSelection::Rows,
    }
}

/// A UUID-shaped `operationId`, the widest the binding mints.
const RETRY_ID_PLACEHOLDER: &str = "00000000-0000-4000-8000-000000000000";

/// Encoded bytes of a request carrying `statements`.
///
/// # Errors
///
/// Returns when the request cannot be encoded, such as TEXT holding U+0000.
fn request_bytes(
    operation_id: &str,
    request_hash: &str,
    statements: &[TypedDbStatement],
) -> Result<usize> {
    Ok(encoded_execute_request_bytes(&ExecuteRequest {
        operation_id: operation_id.to_string(),
        request_hash: request_hash.to_string(),
        statements: statements.to_vec(),
        deadline_unix_ms: 0,
    })?
    .len())
}

/// The smaller of two caps where `0` means "no cap".
fn tighter_cap(a: u32, b: u32) -> u32 {
    match (a, b) {
        (0, b) => b,
        (a, 0) => a,
        (a, b) => a.min(b),
    }
}

/// `reply` once it is checked to answer `request`: the same operation id
/// (a request sent without one takes the id the host minted), one result
/// per statement, and no result with more rows than its statement's
/// `max_rows` asked for.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the reply is for another
/// request or a result carries more rows than its statement allowed.
fn reply_for(request: &ExecuteRequest, reply: ExecuteReply) -> Result<ExecuteReply> {
    if !request.operation_id.is_empty() && reply.operation_id != request.operation_id {
        return Err(PluginError::invalid_params(format!(
            "reply answers operation {}, not {}",
            reply.operation_id, request.operation_id
        )));
    }
    if reply.statements.len() != request.statements.len() {
        return Err(PluginError::invalid_params(format!(
            "reply carries {} results for {} statements",
            reply.statements.len(),
            request.statements.len()
        )));
    }
    for (i, (statement, result)) in request.statements.iter().zip(&reply.statements).enumerate() {
        let max_rows = usize::try_from(statement.max_rows).unwrap_or(usize::MAX);
        if statement.max_rows > 0 && result.rows.len() > max_rows {
            return Err(PluginError::invalid_params(format!(
                "reply carries {} rows for statement {i}, which asked for at most {max_rows}",
                result.rows.len()
            )));
        }
    }
    Ok(reply)
}

/// Greedy split of `typed` into index runs that each fit one request.
///
/// Sizes are estimated per statement against one probe statement and each
/// run is verified by one exact encode; a run the estimate let through is
/// halved until it fits, and a lone statement over the cap is yielded on
/// its own. `0` lifts a cap.
///
/// # Errors
///
/// Returns when a statement cannot be encoded (U+0000 in SQL or TEXT).
pub fn chunk_indexes(
    typed: &[TypedDbStatement],
    max_statements: u32,
    max_request_bytes: u32,
) -> Result<Vec<Vec<usize>>> {
    if typed.is_empty() {
        return Ok(Vec::new());
    }
    let probe = probe_statement();
    let retry_hash = "0".repeat(64);
    let probe_request = request_bytes("", "", std::slice::from_ref(&probe))?;
    let probe_statement = request_bytes("", "", &[probe.clone(), probe.clone()])? - probe_request;
    let base = request_bytes(
        RETRY_ID_PLACEHOLDER,
        &retry_hash,
        std::slice::from_ref(&probe),
    )? - probe_statement;
    let mut sizes = Vec::with_capacity(typed.len());
    for stmt in typed {
        sizes.push(request_bytes("", "", &[probe.clone(), stmt.clone()])? - probe_request);
    }
    let bytes_cap = usize::try_from(max_request_bytes).unwrap_or(usize::MAX);
    let statements_cap = usize::try_from(max_statements).unwrap_or(usize::MAX);
    let mut fits = |run: &[usize]| -> Result<bool> {
        if max_request_bytes == 0 {
            return Ok(true);
        }
        let statements: Vec<TypedDbStatement> = run.iter().map(|&i| typed[i].clone()).collect();
        Ok(request_bytes(RETRY_ID_PLACEHOLDER, &retry_hash, &statements)? <= bytes_cap)
    };
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    let mut used = base;
    for (i, &size) in sizes.iter().enumerate() {
        let over_statements = max_statements > 0 && run.len() >= statements_cap;
        let over_bytes = max_request_bytes > 0 && !run.is_empty() && used + size > bytes_cap;
        if over_statements || over_bytes {
            split_to_fit(std::mem::take(&mut run), &mut fits, &mut out)?;
            used = base;
        }
        run.push(i);
        used += size;
    }
    if !run.is_empty() {
        split_to_fit(run, &mut fits, &mut out)?;
    }
    Ok(out)
}

/// Appends `run` to `out` as runs that each satisfy `fits`, in order.
///
/// A run the per-statement estimate let through is halved until `fits`
/// accepts it; a lone index is appended without asking, since it cannot be
/// split further.
///
/// # Errors
///
/// Returns the error `fits` returns, which is an encoding failure.
fn split_to_fit(
    run: Vec<usize>,
    fits: &mut impl FnMut(&[usize]) -> Result<bool>,
    out: &mut Vec<Vec<usize>>,
) -> Result<()> {
    let mut pending = vec![run];
    while let Some(run) = pending.pop() {
        if run.len() <= 1 || fits(&run)? {
            out.push(run);
            continue;
        }
        let half = run.len().div_ceil(2);
        let (left, right) = run.split_at(half);
        // Pushed right first so the left half comes out first.
        pending.push(right.to_vec());
        pending.push(left.to_vec());
    }
    Ok(())
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
            let operation_id = request.operation_id.clone();
            *self.last.lock().expect("lock") = Some(request);
            Ok(ExecuteReply {
                operation_id,
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

    /// A session that answers `limits` with the sqlite advertisement.
    struct LimitsSession;

    #[async_trait::async_trait(?Send)]
    impl GuestDatabase for LimitsSession {
        async fn execute(&self, _request: ExecuteRequest) -> Result<ExecuteReply> {
            Err(PluginError::unsupported("execute"))
        }

        async fn limits(&self) -> Result<DbCapabilities> {
            Ok(DbCapabilities::advertised_sqlite())
        }
    }

    /// What a streaming session answers `executeStreamed` with.
    enum StreamAnswer {
        /// Frames of a reply with this many rows.
        Rows(usize),
        /// A typed error before any frame.
        Err(PluginErrorCode),
        /// A stream whose `err` frame is `unsupported`.
        UnsupportedFrame,
        /// Frames of a reply with this many rows, behind a reader.
        Reader(usize),
        /// Frames of a reply with this many rows, cut before the end frame
        /// is whole.
        Truncated(usize),
        /// Frames of a reply with this many rows, then trailing bytes.
        Trailing(usize),
        /// Frames whose head and statement arrive, then an `unsupported`
        /// error frame after this many rows.
        UnsupportedAfter(usize),
        /// A reader that fails.
        BrokenReader,
    }

    /// A reader that always fails.
    struct FailingReader;

    impl AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("wire cut")))
        }
    }

    /// A session that advertises `caps` and counts every call.
    struct StreamingSession {
        caps: DbCapabilities,
        answer: StreamAnswer,
        inline: AtomicU64,
        streamed: AtomicU64,
        limits: AtomicU64,
    }

    impl StreamingSession {
        fn new(caps: DbCapabilities, answer: StreamAnswer) -> Arc<Self> {
            Arc::new(Self {
                caps,
                answer,
                inline: AtomicU64::new(0),
                streamed: AtomicU64::new(0),
                limits: AtomicU64::new(0),
            })
        }

        fn counts(&self) -> (u64, u64, u64) {
            (
                self.inline.load(Ordering::SeqCst),
                self.streamed.load(Ordering::SeqCst),
                self.limits.load(Ordering::SeqCst),
            )
        }
    }

    /// A reply with one result per requested statement: statement `s` has
    /// `rows` rows whose ids start at `s * 100`, so a dropped or reordered
    /// result is visible.
    fn reply_with_rows(request: &ExecuteRequest, rows: usize) -> ExecuteReply {
        ExecuteReply {
            operation_id: request.operation_id.clone(),
            statements: (0..request.statements.len())
                .map(|s| bookclerk_plugin_abi::StatementResult {
                    rows: (0..rows)
                        .map(|i| bookclerk_plugin_abi::DbRow {
                            values: vec![
                                DbValue::Int64(i64::try_from(s * 100 + i).expect("small")),
                                DbValue::Text(format!("Title {i}")),
                            ],
                        })
                        .collect(),
                    columns: vec![
                        bookclerk_plugin_abi::DbColumn {
                            name: "id".into(),
                            db_type: bookclerk_plugin_abi::DbType::Int64,
                        },
                        bookclerk_plugin_abi::DbColumn {
                            name: "title".into(),
                            db_type: bookclerk_plugin_abi::DbType::Text,
                        },
                    ],
                    rows_affected: 0,
                })
                .collect(),
            timing: bookclerk_plugin_abi::DbTiming {
                attempt_elapsed_us: 1,
                db_execution_us: 1,
                db_timing_source: "test".into(),
            },
        }
    }

    #[async_trait::async_trait(?Send)]
    impl GuestDatabase for StreamingSession {
        async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteReply> {
            self.inline.fetch_add(1, Ordering::SeqCst);
            Ok(reply_with_rows(&request, 1))
        }

        async fn limits(&self) -> Result<DbCapabilities> {
            self.limits.fetch_add(1, Ordering::SeqCst);
            Ok(self.caps.clone())
        }

        async fn execute_streamed(
            &self,
            request: ExecuteRequest,
        ) -> Result<bookclerk_plugin_abi::ExecuteStream> {
            self.streamed.fetch_add(1, Ordering::SeqCst);
            match &self.answer {
                StreamAnswer::Rows(rows) => bookclerk_plugin_abi::ExecuteStream::from_reply(
                    &reply_with_rows(&request, *rows),
                ),
                StreamAnswer::Err(code) => Err(PluginError::new(*code, "streamed answer")),
                StreamAnswer::UnsupportedFrame => {
                    Ok(bookclerk_plugin_abi::ExecuteStream::from_frames(
                        bookclerk_plugin_abi::encode_execute_stream_error(
                            &PluginError::unsupported("no stream"),
                        )?,
                    ))
                }
                StreamAnswer::Reader(rows) => {
                    Ok(bookclerk_plugin_abi::ExecuteStream::reader(Box::pin(
                        std::io::Cursor::new(bookclerk_plugin_abi::encode_execute_stream(
                            &reply_with_rows(&request, *rows),
                        )?),
                    )))
                }
                StreamAnswer::Truncated(rows) => {
                    let mut frames = bookclerk_plugin_abi::encode_execute_stream(
                        &reply_with_rows(&request, *rows),
                    )?;
                    frames.truncate(frames.len() - 3);
                    Ok(bookclerk_plugin_abi::ExecuteStream::inline(frames))
                }
                StreamAnswer::Trailing(rows) => {
                    let mut frames = bookclerk_plugin_abi::encode_execute_stream(
                        &reply_with_rows(&request, *rows),
                    )?;
                    frames.extend_from_slice(&[0u8; 8]);
                    Ok(bookclerk_plugin_abi::ExecuteStream::inline(frames))
                }
                StreamAnswer::UnsupportedAfter(rows) => {
                    let mut frames = bookclerk_plugin_abi::encode_execute_stream(
                        &reply_with_rows(&request, *rows),
                    )?;
                    let end = bookclerk_plugin_abi::encoded_execute_stream_frame_bytes(
                        &ExecuteStreamFrame::End(reply_with_rows(&request, 0).timing),
                    )?;
                    frames.truncate(frames.len() - end.len());
                    frames.extend(bookclerk_plugin_abi::encode_execute_stream_error(
                        &PluginError::unsupported("gave up"),
                    )?);
                    Ok(bookclerk_plugin_abi::ExecuteStream::inline(frames))
                }
                StreamAnswer::BrokenReader => Ok(bookclerk_plugin_abi::ExecuteStream::reader(
                    Box::pin(FailingReader),
                )),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reads_stream_when_the_session_advertises_a_budget() {
        let session =
            StreamingSession::new(DbCapabilities::advertised_sqlite(), StreamAnswer::Rows(7));
        let binding = DatabaseBinding::from_session(session.clone());
        let result = binding
            .prepare("SELECT id, title FROM books")
            .all(None)
            .await
            .expect("streamed read");
        let rows = result.results.expect("rows");
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[6].get("title"), Some(&DbValue::Text("Title 6".into())));
        binding
            .prepare("SELECT id, title FROM books")
            .raw(None)
            .await
            .expect("second read");
        assert_eq!(session.counts(), (0, 2, 1), "limits are fetched once");
        assert_eq!(
            binding.limits().await.expect("cached"),
            DbCapabilities::advertised_sqlite()
        );
        assert_eq!(session.counts().2, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inline_sessions_and_unsupported_answers_read_inline() {
        let mut inline_only = DbCapabilities::advertised_sqlite();
        inline_only.max_streamed_result_bytes = 0;
        inline_only.max_streamed_result_rows = 0;
        for (caps, answer, expected) in [
            (inline_only, StreamAnswer::Rows(3), (1, 0, 1)),
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::Err(PluginErrorCode::Unsupported),
                (1, 1, 1),
            ),
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::UnsupportedFrame,
                (1, 1, 1),
            ),
        ] {
            let session = StreamingSession::new(caps, answer);
            let binding = DatabaseBinding::from_session(session.clone());
            let result = binding
                .prepare("SELECT id FROM books")
                .all(None)
                .await
                .expect("inline read");
            assert_eq!(result.results.expect("rows").len(), 1);
            assert_eq!(session.counts(), expected);
        }

        // Every other streamed failure is the caller's.
        let session = StreamingSession::new(
            DbCapabilities::advertised_sqlite(),
            StreamAnswer::Err(PluginErrorCode::Unavailable),
        );
        let err = DatabaseBinding::from_session(session.clone())
            .prepare("SELECT 1")
            .all(None)
            .await
            .expect_err("unavailable surfaces");
        assert_eq!(err.code, PluginErrorCode::Unavailable);
        assert_eq!(session.counts().0, 0);
    }

    /// A rewrite of a reply before it returns to the binding.
    type Tamper = fn(ExecuteReply) -> ExecuteReply;

    /// A session whose replies are rewritten by `tamper` before they
    /// return, on every path: inline, streamed, and the inline fallback
    /// behind an `unsupported` stream.
    struct TamperingSession {
        caps: DbCapabilities,
        stream: bool,
        tamper: Tamper,
    }

    #[async_trait::async_trait(?Send)]
    impl GuestDatabase for TamperingSession {
        async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteReply> {
            Ok((self.tamper)(reply_with_rows(&request, 2)))
        }

        async fn limits(&self) -> Result<DbCapabilities> {
            Ok(self.caps.clone())
        }

        async fn execute_streamed(
            &self,
            request: ExecuteRequest,
        ) -> Result<bookclerk_plugin_abi::ExecuteStream> {
            if !self.stream {
                return Err(PluginError::unsupported("no stream"));
            }
            bookclerk_plugin_abi::ExecuteStream::from_reply(&(self.tamper)(reply_with_rows(
                &request, 2,
            )))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replies_must_answer_the_request_on_every_path() {
        let mut inline_only = DbCapabilities::advertised_sqlite();
        inline_only.max_streamed_result_bytes = 0;
        inline_only.max_streamed_result_rows = 0;
        let paths: [(DbCapabilities, bool); 3] = [
            (inline_only, false),
            (DbCapabilities::advertised_sqlite(), true),
            (DbCapabilities::advertised_sqlite(), false),
        ];
        let tampers: [(&str, Tamper); 4] = [
            ("another operation", |mut reply| {
                reply.operation_id = "someone-else".into();
                reply
            }),
            ("a missing result", |mut reply| {
                reply.statements.pop();
                reply
            }),
            ("an extra result", |mut reply| {
                let extra = reply.statements[0].clone();
                reply.statements.push(extra);
                reply
            }),
            ("too many rows for first()", |reply| reply),
        ];
        for (caps, stream) in &paths {
            for (what, tamper) in &tampers {
                let binding = DatabaseBinding::from_session(Arc::new(TamperingSession {
                    caps: caps.clone(),
                    stream: *stream,
                    tamper: *tamper,
                }));
                let outcome = if *what == "too many rows for first()" {
                    // The session answers two rows where `first()` asked for one.
                    binding
                        .prepare("SELECT id FROM books")
                        .first(None)
                        .await
                        .map(|_| ())
                } else {
                    binding
                        .prepare("SELECT id FROM books")
                        .all(None)
                        .await
                        .map(|_| ())
                };
                let err = outcome
                    .err()
                    .unwrap_or_else(|| panic!("{what} must be refused (stream = {stream})"));
                assert_eq!(err.code, PluginErrorCode::InvalidParams, "{what}");
            }
            // An honest reply passes on the same path.
            let binding = DatabaseBinding::from_session(Arc::new(TamperingSession {
                caps: caps.clone(),
                stream: *stream,
                tamper: |reply| reply,
            }));
            let rows = binding
                .prepare("SELECT id FROM books")
                .all(None)
                .await
                .expect("honest reply")
                .results
                .expect("rows");
            assert_eq!(rows.len(), 2);
        }

        // A request sent without an operation id takes the one the host minted.
        let minted = ExecuteReply {
            operation_id: "host-minted".into(),
            statements: Vec::new(),
            timing: DbTiming {
                attempt_elapsed_us: 0,
                db_execution_us: 0,
                db_timing_source: "test".into(),
            },
        };
        let anonymous = ExecuteRequest {
            operation_id: String::new(),
            request_hash: String::new(),
            statements: Vec::new(),
            deadline_unix_ms: 0,
        };
        assert_eq!(
            reply_for(&anonymous, minted.clone())
                .expect("minted id accepted")
                .operation_id,
            "host-minted"
        );
        let named = ExecuteRequest {
            operation_id: "mine".into(),
            ..anonymous
        };
        assert!(reply_for(&named, minted).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_streamed_reply_past_the_budget_fails_closed() {
        let mut tiny = DbCapabilities::advertised_sqlite();
        tiny.max_streamed_result_bytes = 4096;
        tiny.max_streamed_result_rows = 4;
        let session = StreamingSession::new(tiny, StreamAnswer::Rows(9));
        let err = DatabaseBinding::from_session(session)
            .prepare("SELECT id FROM books")
            .all(None)
            .await
            .expect_err("nine rows over a cap of four");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        let limit = err.limit().expect("typed limit");
        assert_eq!(
            (limit.limit.as_str(), limit.cap),
            ("maxStreamedResultRows", 4)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chunk_splits_to_the_session_caps_or_explicit_ones() {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_statements = 2;
        let session = StreamingSession::new(caps, StreamAnswer::Rows(0));
        let binding = DatabaseBinding::from_session(session);
        let stmts: Vec<PreparedStatement> = (0..5)
            .map(|i| {
                binding
                    .prepare("INSERT INTO t VALUES (?)")
                    .bind(vec![DbValue::Int64(i)])
            })
            .collect();
        let batches = binding.chunk(stmts.clone(), None).await.expect("chunk");
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![2, 2, 1]
        );
        for batch in batches {
            let n = batch.len();
            let results = binding.batch(batch, None).await.expect("each batch runs");
            assert_eq!(results.len(), n, "one result per statement");
        }
        // A streamed batch answers each statement in order.
        let reader = DatabaseBinding::from_session(StreamingSession::new(
            DbCapabilities::advertised_sqlite(),
            StreamAnswer::Rows(2),
        ));
        let reads = reader
            .batch(
                vec![
                    reader.prepare("SELECT id FROM a"),
                    reader.prepare("SELECT id FROM b"),
                ],
                None,
            )
            .await
            .expect("two reads");
        assert_eq!(reads.len(), 2);
        let first = |r: &crate::db_binding::D1Result| {
            r.results.as_ref().expect("rows")[0].get("id").cloned()
        };
        assert_eq!(first(&reads[0]), Some(DbValue::Int64(0)));
        assert_eq!(first(&reads[1]), Some(DbValue::Int64(100)));

        // Explicit zero caps mean one batch; the binding's own request cap
        // tightens the session's.
        let one = binding
            .chunk(stmts.clone(), Some(ChunkCaps::default()))
            .await
            .expect("explicit zero caps");
        assert_eq!(one.len(), 1);
        let tight = DatabaseBinding::from_session_with(
            StreamingSession::new(DbCapabilities::advertised_sqlite(), StreamAnswer::Rows(0)),
            DatabaseBindingOptions {
                max_request_bytes: 220,
                ..DatabaseBindingOptions::default()
            },
        );
        let tightened = tight.chunk(stmts.clone(), None).await.expect("chunk");
        assert!(tightened.len() > 1, "the binding cap splits the batch");
        for batch in tightened {
            tight.batch(batch, None).await.expect("each batch fits");
        }

        let legacy = DatabaseBinding::from_session(Arc::new(RecordingSession {
            last: Mutex::new(None),
        }));
        let explicit = legacy
            .chunk(
                stmts.clone(),
                Some(ChunkCaps {
                    max_statements: 3,
                    max_request_bytes: 0,
                }),
            )
            .await
            .expect("explicit caps");
        assert_eq!(
            explicit.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![3, 2]
        );
        let err = match legacy.chunk(stmts, None).await {
            Ok(_) => panic!("no limits and no caps must fail"),
            Err(err) => err,
        };
        assert_eq!(err.code, PluginErrorCode::Unsupported);
    }

    #[test]
    fn chunk_indexes_fit_the_byte_cap_and_cover_every_statement_once() {
        let stmt = |value: String| TypedDbStatement {
            sql: "INSERT INTO t (v) VALUES (?)".into(),
            parameters: vec![DbValue::Text(value)],
            kind: DbPlanStatementKind::Execute,
            max_rows: 0,
            result_selection: DbResultSelection::AffectedRows,
        };
        let mut typed: Vec<TypedDbStatement> =
            (0..30).map(|i| stmt("x".repeat((i * 37) % 200))).collect();
        typed.insert(10, stmt("y".repeat(2000)));
        let cap = 600u32;
        let runs = chunk_indexes(&typed, 0, cap).expect("split");
        assert_eq!(
            runs.iter().flatten().copied().collect::<Vec<_>>(),
            (0..typed.len()).collect::<Vec<_>>()
        );
        for run in &runs {
            if run.len() > 1 {
                let statements: Vec<_> = run.iter().map(|&i| typed[i].clone()).collect();
                let bytes = request_bytes(RETRY_ID_PLACEHOLDER, &"0".repeat(64), &statements)
                    .expect("encode");
                assert!(bytes <= cap as usize, "run {run:?} encodes to {bytes}");
            }
        }
        assert_eq!(
            runs.iter().find(|run| run.contains(&10)),
            Some(&vec![10]),
            "an oversized statement is yielded alone"
        );
        assert!(
            runs.len() < typed.len(),
            "runs batch more than one statement"
        );
        assert!(chunk_indexes(&[], 5, 100).expect("empty").is_empty());
        assert_eq!(
            chunk_indexes(&typed[..3], 0, 0).expect("no caps"),
            vec![vec![0, 1, 2]]
        );
        assert_eq!(
            chunk_indexes(&typed[..5], 2, 0).expect("statement cap"),
            vec![vec![0, 1], vec![2, 3], vec![4]]
        );
    }

    #[test]
    fn split_to_fit_halves_a_run_the_estimate_let_through() {
        // Runs of three or more do not fit, so the eight indexes come out as
        // ordered pairs and an odd run splits with the larger half first.
        let mut pairs = |run: &[usize]| Ok(run.len() <= 2);
        let mut out = Vec::new();
        split_to_fit((0..8).collect(), &mut pairs, &mut out).expect("split");
        assert_eq!(out, vec![vec![0, 1], vec![2, 3], vec![4, 5], vec![6, 7]]);
        let mut out = Vec::new();
        split_to_fit((0..5).collect(), &mut pairs, &mut out).expect("split");
        assert_eq!(out, vec![vec![0, 1], vec![2], vec![3, 4]]);

        // A lone index is never asked whether it fits; an encoding failure
        // from the check surfaces.
        let mut failing =
            |_: &[usize]| -> Result<bool> { Err(PluginError::invalid_params("cannot encode")) };
        let mut out = Vec::new();
        split_to_fit(vec![7], &mut failing, &mut out).expect("lone index");
        assert_eq!(out, vec![vec![7]]);
        assert!(split_to_fit(vec![1, 2], &mut failing, &mut out).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn limits_come_from_the_session_or_are_unsupported() {
        let binding = DatabaseBinding::from_session(Arc::new(LimitsSession));
        let caps = binding.limits().await.expect("limits");
        assert_eq!(caps, DbCapabilities::advertised_sqlite());

        // A session that predates `GuestDatabase.limits` keeps the default.
        let legacy = DatabaseBinding::from_session(Arc::new(RecordingSession {
            last: Mutex::new(None),
        }));
        let err = legacy.limits().await.expect_err("default is unsupported");
        assert_eq!(err.code, PluginErrorCode::Unsupported);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn over_cap_request_is_a_typed_limit_error_before_execute() {
        let session = Arc::new(RecordingSession {
            last: Mutex::new(None),
        });
        let binding = DatabaseBinding::from_session_with(
            session.clone(),
            DatabaseBindingOptions {
                max_request_bytes: 16,
                ..DatabaseBindingOptions::default()
            },
        );
        let err = binding
            .prepare("SELECT n FROM t")
            .all(None)
            .await
            .expect_err("request is over 16 bytes");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        let limit = err.limit().expect("typed limit");
        assert_eq!(limit.limit, "maxRequestBytes");
        assert_eq!(limit.cap, 16);
        assert!(limit.used > 16, "{limit:?}");
        assert!(
            session.last.lock().expect("lock").is_none(),
            "an over-cap request never reaches the session"
        );
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

    /// Every row `stream` hands out, then the error it ended with, if any;
    /// checks the stream stays ended after that.
    async fn drain(mut stream: RowStream) -> (Vec<HashMap<String, DbValue>>, Option<PluginError>) {
        let mut rows = Vec::new();
        let mut failure = None;
        while let Some(row) = stream.next().await {
            match row {
                Ok(row) => rows.push(row),
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
        }
        assert!(stream.next().await.is_none(), "the stream stays ended");
        assert_eq!(format!("{stream:?}"), r#"RowStream { state: "done" }"#);
        (rows, failure)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn iterate_hands_out_streamed_rows_in_order() {
        // Inline frames and a reader body several read windows long.
        for answer in [StreamAnswer::Rows(300), StreamAnswer::Reader(6000)] {
            let expected = match answer {
                StreamAnswer::Rows(n) | StreamAnswer::Reader(n) => n,
                _ => unreachable!(),
            };
            let session = StreamingSession::new(DbCapabilities::advertised_sqlite(), answer);
            let binding = DatabaseBinding::from_session(session.clone());
            let stream = binding.prepare("SELECT id, title FROM books").iterate(None);
            assert_eq!(format!("{stream:?}"), r#"RowStream { state: "unopened" }"#);
            assert_eq!(session.counts(), (0, 0, 0), "nothing is sent before next()");
            let (rows, failure) = drain(stream).await;
            assert!(failure.is_none(), "{failure:?}");
            assert_eq!(rows.len(), expected);
            for (i, row) in rows.iter().enumerate() {
                assert_eq!(
                    row.get("id"),
                    Some(&DbValue::Int64(i64::try_from(i).expect("small")))
                );
                assert_eq!(row.get("title"), Some(&DbValue::Text(format!("Title {i}"))));
            }
            assert_eq!(session.counts(), (0, 1, 1));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn iterate_reads_inline_when_the_session_does_not_stream() {
        let mut inline_only = DbCapabilities::advertised_sqlite();
        inline_only.max_streamed_result_bytes = 0;
        inline_only.max_streamed_result_rows = 0;
        for (caps, answer, expected) in [
            (inline_only, StreamAnswer::Rows(3), (1, 0, 1)),
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::Err(PluginErrorCode::Unsupported),
                (1, 1, 1),
            ),
            // An `unsupported` err frame before any row still falls back.
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::UnsupportedFrame,
                (1, 1, 1),
            ),
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::UnsupportedAfter(0),
                (1, 1, 1),
            ),
        ] {
            let session = StreamingSession::new(caps, answer);
            let mut stream = DatabaseBinding::from_session(session.clone())
                .prepare("SELECT id, title FROM books")
                .iterate(None);
            let first = stream.next().await.expect("one row").expect("inline row");
            assert_eq!(first.get("id"), Some(&DbValue::Int64(0)));
            assert_eq!(format!("{stream:?}"), r#"RowStream { state: "inline" }"#);
            let (rest, failure) = drain(stream).await;
            assert!(rest.is_empty() && failure.is_none());
            assert_eq!(session.counts(), expected);
        }

        // A session whose limits fail reads inline too.
        let session = Arc::new(RecordingSession {
            last: Mutex::new(None),
        });
        let (rows, failure) = drain(
            DatabaseBinding::from_session(session.clone())
                .prepare("SELECT n FROM t")
                .iterate(None),
        )
        .await;
        assert!(failure.is_none());
        assert_eq!(rows.len(), 1);
        let sent = session.last.lock().expect("lock").clone().expect("sent");
        assert_eq!(sent.statements[0].result_selection, DbResultSelection::Rows);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn iterate_surfaces_stream_failures_once() {
        let mut tiny = DbCapabilities::advertised_sqlite();
        tiny.max_streamed_result_bytes = 4096;
        tiny.max_streamed_result_rows = 4;
        let cases = [
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::Err(PluginErrorCode::Unavailable),
                PluginErrorCode::Unavailable,
                0,
            ),
            (
                tiny,
                StreamAnswer::Rows(9),
                PluginErrorCode::LimitExceeded,
                0,
            ),
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::Truncated(5),
                PluginErrorCode::Unavailable,
                5,
            ),
            (
                DbCapabilities::advertised_sqlite(),
                // Refused with the window that carried them, rows included.
                StreamAnswer::Trailing(2),
                PluginErrorCode::InvalidParams,
                0,
            ),
            // Once rows were handed out the read cannot restart behind them.
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::UnsupportedAfter(3),
                PluginErrorCode::Unsupported,
                3,
            ),
            (
                DbCapabilities::advertised_sqlite(),
                StreamAnswer::BrokenReader,
                PluginErrorCode::Unavailable,
                0,
            ),
        ];
        for (caps, answer, code, handed) in cases {
            let session = StreamingSession::new(caps, answer);
            let (rows, failure) = drain(
                DatabaseBinding::from_session(session.clone())
                    .prepare("SELECT id, title FROM books")
                    .iterate(None),
            )
            .await;
            let err = failure.expect("the read fails");
            assert_eq!(err.code, code, "{err}");
            assert_eq!(rows.len(), handed, "{err}");
            assert_eq!(session.counts().0, 0, "no inline retry: {err}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn iterate_holds_the_reply_to_the_request() {
        let tampers: [(&str, Tamper); 2] = [
            ("another operation", |mut reply| {
                reply.operation_id = "someone-else".into();
                reply
            }),
            ("an extra result", |mut reply| {
                let extra = reply.statements[0].clone();
                reply.statements.push(extra);
                reply
            }),
        ];
        for stream in [true, false] {
            for (what, tamper) in &tampers {
                let binding = DatabaseBinding::from_session(Arc::new(TamperingSession {
                    caps: DbCapabilities::advertised_sqlite(),
                    stream,
                    tamper: *tamper,
                }));
                let (rows, failure) =
                    drain(binding.prepare("SELECT id FROM books").iterate(None)).await;
                let err = failure.unwrap_or_else(|| panic!("{what} must be refused"));
                assert_eq!(err.code, PluginErrorCode::InvalidParams, "{what}");
                assert!(rows.is_empty(), "{what}");
            }
        }

        // A streamed reply with more rows than the statement asked for stops
        // at the cap.
        let session =
            StreamingSession::new(DbCapabilities::advertised_sqlite(), StreamAnswer::Rows(3));
        let binding = DatabaseBinding::from_session_with(
            session,
            DatabaseBindingOptions {
                max_result_rows: 2,
                ..DatabaseBindingOptions::default()
            },
        );
        let (rows, failure) = drain(binding.prepare("SELECT id FROM books").iterate(None)).await;
        assert_eq!(rows.len(), 2);
        let err = failure.expect("a third row");
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        assert!(err.message.contains("at most 2"), "{err}");

        // A head without an operation id check: the request always carries
        // one, but a minted id is accepted for an anonymous request.
        let anonymous = ExecuteRequest {
            operation_id: String::new(),
            request_hash: String::new(),
            statements: Vec::new(),
            deadline_unix_ms: 0,
        };
        check_stream_head(&anonymous, "host-minted", 0).expect("minted id accepted");
        check_max_rows(&anonymous, 10).expect("no statement, no cap");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn iterate_refuses_an_oversized_request_before_sending() {
        let session =
            StreamingSession::new(DbCapabilities::advertised_sqlite(), StreamAnswer::Rows(1));
        let binding = DatabaseBinding::from_session_with(
            session.clone(),
            DatabaseBindingOptions {
                max_request_bytes: 64,
                ..DatabaseBindingOptions::default()
            },
        );
        let (rows, failure) = drain(
            binding
                .prepare("SELECT id FROM books WHERE title = ?")
                .bind(vec![DbValue::Text("x".repeat(512))])
                .iterate(None),
        )
        .await;
        assert!(rows.is_empty());
        let err = failure.expect("over the request cap");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded, "{err}");
        assert_eq!(session.counts().1, 0, "nothing was sent");
    }
}
