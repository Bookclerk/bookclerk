//! Generic D1 HTTP `batch()` executor for host-authored SQL plans.
//!
//! The guest does not parse Bookclerk operation names. Production and tests
//! use [`D1Proxy::run_typed_atomic`] (`ExecuteRequest`).

use std::time::Duration;

use bookclerk_db_exec::DbPlanStatementKind;
use bookclerk_plugin_abi::{
    lowered_statement_preflight_len_proven, DbCapabilities, D1_MAX_SQL_STATEMENT_BYTES,
};
use bookclerk_plugin_sdk::{
    encoded_execute_reply_bytes, encoded_statement_result_bytes, DbColumn, DbResultSelection,
    DbRow, DbTiming, DbType, DbValue, ExecuteReply, ExecuteRequest, PluginError, StatementResult,
    TypedDbStatement,
};
use sea_orm::DbErr;
use serde_json::Value as JsonValue;

use super::d1::D1Proxy;

/// Collapses adapter-private companions (and host-schema identity extras).
fn collapse_d1_wire(
    groups: &[usize],
    statements: Vec<bookclerk_plugin_abi::StatementResult>,
) -> Vec<bookclerk_plugin_abi::StatementResult> {
    bookclerk_db_exec::collapse_companion_groups(groups, statements)
}

/// One statement in a D1 HTTP batch body.
pub(crate) type SqlStmt = (String, Vec<JsonValue>);

/// Outcome of the guest-receipt stub INSERT sent as its own D1 HTTP batch.
enum GuestReceiptClaim {
    /// `rows_affected = 1`: this host owns the operation and may run guest SQL.
    Won {
        /// Claim INSERT result appended onto the wrap so finalize still sees the stub.
        stub: StatementResult,
    },
    /// `rows_affected = 0` or a unique conflict: another host already claimed.
    Lost,
}

/// True when the claim INSERT hit a unique/PK conflict (treat as lost claim).
fn is_claim_constraint(err: &DbErr) -> bool {
    let t = err.to_string().to_ascii_lowercase();
    t.contains("sqlite_constraint")
        || t.contains("unique constraint")
        || (t.contains("unique") && t.contains("db_atomic_receipts"))
}

/// True when the guest-receipt wrap contains ungated DDL (CREATE/DROP).
fn wrap_has_ungated_ddl(statements: &[TypedDbStatement]) -> bool {
    let end = statements
        .len()
        .saturating_sub(bookclerk_db_exec::GUEST_RECEIPT_STUB_SUFFIX);
    statements
        .iter()
        .skip(bookclerk_db_exec::GUEST_RECEIPT_WRAP_PREFIX)
        .take(end.saturating_sub(bookclerk_db_exec::GUEST_RECEIPT_WRAP_PREFIX))
        .any(|s| bookclerk_plugin_abi::statement_is_ddl(&s.sql))
}

/// Strips the host wrap's DML receipt gate after D1 has claimed the operation.
fn ungate_claimed_guest_write(sql_stmt: &mut SqlStmt, typed: &mut TypedDbStatement) {
    let stripped = bookclerk_db_exec::strip_guest_receipt_write_gate(&sql_stmt.0);
    if stripped == sql_stmt.0 {
        return;
    }
    sql_stmt.0 = stripped;
    let _ = sql_stmt.1.pop();
    typed.sql = sql_stmt.0.clone();
    let _ = typed.parameters.pop();
}

/// Maximum D1 HTTP batch attempts, including retries after ambiguous responses.
const ATOMIC_HTTP_ATTEMPTS: usize = 3;

/// True when any statement is DDL (`CREATE` / `ALTER` / `DROP`), which
/// invalidates the declared-type cache.
fn sql_is_ddl(sql: &str) -> bool {
    let t = sql.trim_start();
    ["CREATE", "ALTER", "DROP"]
        .iter()
        .any(|verb| t.len() >= verb.len() && t[..verb.len()].eq_ignore_ascii_case(verb))
}

/// Cache key for a parsed table reference (quotes stripped, lowercased).
fn table_cache_key(name: &str) -> String {
    name.trim()
        .trim_matches('"')
        .trim_matches('`')
        .trim_matches('[')
        .trim_matches(']')
        .to_ascii_lowercase()
}

/// True when every typed statement is a read (`Select`).
fn typed_is_read_only(statements: &[TypedDbStatement]) -> bool {
    statements
        .iter()
        .all(|s| matches!(s.kind, DbPlanStatementKind::Select))
}

/// True when the batch includes host `db_atomic_receipts` gating (named atomics).
fn typed_is_receipt_gated(statements: &[TypedDbStatement]) -> bool {
    statements
        .iter()
        .any(|s| s.sql.to_ascii_lowercase().contains("db_atomic_receipts"))
}

impl D1Proxy {
    /// Runs a typed [`ExecuteRequest`] as one D1 HTTP batch.
    ///
    /// [`DbValue::Bytes`] parameters are stored as true BLOBs: the placeholder
    /// is rewritten to `unhex(?)` with hex text (JSON has no binary scalar).
    /// BLOB result cells arrive as JSON byte arrays and decode back to
    /// [`DbValue::Bytes`]; text is never re-encoded, so a [`DbValue::Text`]
    /// cell cannot masquerade as bytes. After HTTP success, encode/size
    /// failures are ambiguous (`unavailable`).
    ///
    /// Ambiguous (possibly committed) failures are retried only when the
    /// request is receipt-gated or read-only; an unwrapped mutation returns
    /// the ambiguous error without resubmitting.
    ///
    /// # Errors
    ///
    /// Returns when the batch is rejected, HTTP fails, or the reply cannot be
    /// encoded after commit.
    pub async fn run_typed_atomic(
        &self,
        req: &ExecuteRequest,
        guest_receipt: bookclerk_plugin_abi::GuestReceiptPersist,
        proofs: &[bookclerk_plugin_abi::ResolvedStatement],
    ) -> std::result::Result<ExecuteReply, DbErr> {
        let started = std::time::Instant::now();
        let deadline = (req.deadline_unix_ms > 0).then_some(req.deadline_unix_ms);
        check_d1_session(
            bookclerk_db_exec::AtomicInterruptPhase::BeforeBegin,
            deadline,
        )?;
        let req = req.clone();
        bookclerk_plugin_abi::AdapterExecuteRequest {
            request: req.clone(),
            guest_receipt: guest_receipt.clone(),
            proofs: proofs.to_vec(),
            isolation: bookclerk_plugin_abi::IsolationReq::AtomicBatch,
        }
        .require_proofs()
        .map_err(|err| DbErr::Custom(err.to_string()))?;
        // Host schema batches travel canonical; this adapter edge splits the
        // pack for the SQLite family and collapses results back to the wire
        // request shape after parsing.
        let wire_len = req.statements.len();
        let (expanded, schema_groups) =
            bookclerk_db_exec::expand_host_schema_execute_request_grouped(
                sea_orm::DatabaseBackend::Sqlite,
                &req,
            );
        let host_schema = expanded
            .statements
            .last()
            .is_some_and(|s| bookclerk_db_exec::is_host_schema_version_marker(&s.sql));
        let (expanded, companion_groups) = if host_schema {
            (expanded, schema_groups)
        } else {
            bookclerk_db_exec::expand_binding_execute_request(
                sea_orm::DatabaseBackend::Sqlite,
                &expanded,
            )
        };
        let req = &expanded;
        if req.statements.iter().any(|s| sql_is_ddl(&s.sql)) {
            self.clear_table_types();
        }
        reject_unbounded_returning_typed(&req.statements)?;
        // Retrying after an ambiguous (possibly committed) response is
        // exactly-once only when the request is receipt-gated (replay detects
        // the prior commit) or provably read-only. An unwrapped mutation must
        // surface the ambiguity instead of resubmitting.
        let retry_safe = !guest_receipt.is_absent()
            || typed_is_read_only(&req.statements)
            || typed_is_receipt_gated(&req.statements);
        let d1_caps = DbCapabilities::advertised_d1();
        let cap = d1_caps.max_result_rows;
        if proofs.len() != wire_len {
            return Err(DbErr::Custom(
                "host execute envelope proofs must match statement count".into(),
            ));
        }
        let expanded_proofs = proofs_for_expanded(proofs, &companion_groups)?;
        if expanded_proofs.len() != req.statements.len() {
            return Err(DbErr::Custom(
                "host execute envelope proofs must match statement count".into(),
            ));
        }
        let statements: Vec<SqlStmt> = req
            .statements
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (sql, binds) = d1_typed_statement(&s.sql, &s.parameters, expanded_proofs[i])?;
                let sql = if s.kind.wrap_select_limit() {
                    bookclerk_db_exec::cap_query_sql(&sql, cap)
                } else {
                    sql
                };
                Ok((sql, binds))
            })
            .collect::<Result<Vec<_>, DbErr>>()?;
        // D1 cannot skip later statements in the same HTTP batch based on
        // earlier results. Claim the receipt stub INSERT first so two hosts
        // cannot both run ungated guest DDL. Do not send prune as its own
        // committing HTTP — that would consume lost-reply tests.
        let needs_claim = !guest_receipt.is_absent()
            && req.statements.len() > bookclerk_db_exec::GUEST_RECEIPT_WRAP_PREFIX
            && statements
                .last()
                .is_some_and(|(sql, _)| bookclerk_db_exec::is_guest_receipt_stub_insert(sql))
            && wrap_has_ungated_ddl(&req.statements);
        let mut last_err = None;
        let mut claim = None;
        if needs_claim {
            for attempt in 0..ATOMIC_HTTP_ATTEMPTS {
                check_d1_session(
                    bookclerk_db_exec::AtomicInterruptPhase::BeforeBegin,
                    deadline,
                )?;
                let timeout = d1_http_timeout(deadline)?;
                match self
                    .claim_guest_receipt_stub(req, &statements, timeout, deadline, started)
                    .await
                {
                    Ok(c) => {
                        claim = Some(c);
                        break;
                    }
                    Err(err)
                        if is_ambiguous_d1(&err)
                            && retry_safe
                            && attempt + 1 < ATOMIC_HTTP_ATTEMPTS =>
                    {
                        sleep_before_d1_retry_bounded(attempt, None, deadline).await?;
                        last_err = Some(err);
                    }
                    Err(err) => return Err(err),
                }
            }
            let Some(c) = claim else {
                return Err(last_err.unwrap_or_else(|| ambiguous_d1("exhausted claim retries")));
            };
            match c {
                GuestReceiptClaim::Lost => {
                    let timeout = d1_http_timeout(deadline)?;
                    return self
                        .replay_lost_guest_receipt_claim(
                            req,
                            &statements,
                            timeout,
                            deadline,
                            started,
                            &companion_groups,
                            &guest_receipt,
                            cap,
                        )
                        .await;
                }
                GuestReceiptClaim::Won { stub } => {
                    #[cfg(test)]
                    if self.consume_fail_after_won_claim() {
                        return Err(DbErr::Custom(
                            "unavailable: guest receipt claimed; DDL not started".into(),
                        ));
                    }
                    let timeout = d1_http_timeout(deadline)?;
                    return self
                        .run_claimed_guest_ddl(
                            req,
                            &statements,
                            stub,
                            timeout,
                            deadline,
                            started,
                            &companion_groups,
                            &guest_receipt,
                            cap,
                        )
                        .await;
                }
            }
        }
        for attempt in 0..ATOMIC_HTTP_ATTEMPTS {
            check_d1_session(
                bookclerk_db_exec::AtomicInterruptPhase::BeforeBegin,
                deadline,
            )?;
            let timeout = d1_http_timeout(deadline)?;
            let raw = match self.run_batch_with_timeout(&statements, timeout).await {
                Ok(value) => {
                    check_d1_session(
                        bookclerk_db_exec::AtomicInterruptPhase::AroundCommit,
                        deadline,
                    )?;
                    value
                }
                Err(err)
                    if err.is_retryable() && retry_safe && attempt + 1 < ATOMIC_HTTP_ATTEMPTS =>
                {
                    sleep_before_d1_retry_bounded(attempt, err.retry_after(), deadline).await?;
                    last_err = Some(DbErr::from(err));
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            match parse_typed_batch(req, &raw, started) {
                Ok(mut reply) => {
                    self.normalize_reply_from_declared(req, &mut reply, timeout)
                        .await?;
                    reply.statements = collapse_d1_wire(&companion_groups, reply.statements);
                    if !guest_receipt.is_absent() {
                        // Guest-receipt finalize needs statement results, so D1 runs a
                        // follow-up HTTP batch after the main batch commits. Same-batch
                        // finalize would require provider support for dependent SQL.
                        let hint = &guest_receipt;
                        let finalize = bookclerk_db_exec::guest_receipt_finalize_stmts(
                            &reply,
                            usize::try_from(hint.guest_statement_len).unwrap_or(usize::MAX),
                            &hint.guest_request_hash,
                        )?;
                        if !finalize.is_empty() {
                            let fin_stmts: Vec<SqlStmt> = finalize
                                .iter()
                                .map(|s| {
                                    let (sql, binds) =
                                        d1_typed_statement(&s.sql, &s.parameters, None)?;
                                    let sql = if s.kind.wrap_select_limit() {
                                        bookclerk_db_exec::cap_query_sql(&sql, cap)
                                    } else {
                                        sql
                                    };
                                    Ok((sql, binds))
                                })
                                .collect::<Result<Vec<_>, DbErr>>()?;
                            self.run_batch_with_timeout(&fin_stmts, timeout).await?;
                        }
                    }
                    return Ok(reply);
                }
                Err(err)
                    if is_ambiguous_d1(&err)
                        && retry_safe
                        && attempt + 1 < ATOMIC_HTTP_ATTEMPTS =>
                {
                    sleep_before_d1_retry_bounded(attempt, None, deadline).await?;
                    last_err = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_err.unwrap_or_else(|| ambiguous_d1("exhausted retries")))
    }

    /// INSERT-only claim of the guest-receipt stub (one committing HTTP).
    async fn claim_guest_receipt_stub(
        &self,
        req: &ExecuteRequest,
        statements: &[SqlStmt],
        timeout: Duration,
        deadline: Option<u64>,
        started: std::time::Instant,
    ) -> std::result::Result<GuestReceiptClaim, DbErr> {
        #[cfg(test)]
        self.maybe_pause_claim().await;
        let stub_idx = statements.len().saturating_sub(1);
        let raw = match self
            .run_batch_with_timeout(&statements[stub_idx..=stub_idx], timeout)
            .await
        {
            Ok(value) => {
                check_d1_session(
                    bookclerk_db_exec::AtomicInterruptPhase::AroundCommit,
                    deadline,
                )?;
                value
            }
            Err(err) => return Err(err.into()),
        };
        let claim_req = ExecuteRequest {
            operation_id: req.operation_id.clone(),
            request_hash: req.request_hash.clone(),
            statements: vec![req.statements[stub_idx].clone()],
            deadline_unix_ms: req.deadline_unix_ms,
        };
        match parse_typed_batch(&claim_req, &raw, started) {
            Ok(reply) => {
                let won = reply
                    .statements
                    .first()
                    .is_some_and(|s| s.rows_affected > 0);
                if won {
                    Ok(GuestReceiptClaim::Won {
                        stub: reply
                            .statements
                            .into_iter()
                            .next()
                            .unwrap_or_else(|| StatementResult::from_affected(1)),
                    })
                } else {
                    Ok(GuestReceiptClaim::Lost)
                }
            }
            Err(err) if is_claim_constraint(&err) => Ok(GuestReceiptClaim::Lost),
            Err(err) => Err(err),
        }
    }

    /// Prior-receipt SELECT after a lost claim; pads skipped guest work.
    ///
    /// A [`bookclerk_db_exec::GUEST_RECEIPT_STATUS_CLAIMED`] row with the same
    /// guest hash is an in-flight claim (crash between stub INSERT and DDL).
    /// Resume ungated DDL instead of treating the empty payload as result-lost.
    #[allow(clippy::too_many_arguments)]
    async fn replay_lost_guest_receipt_claim(
        &self,
        req: &ExecuteRequest,
        statements: &[SqlStmt],
        timeout: Duration,
        deadline: Option<u64>,
        started: std::time::Instant,
        companion_groups: &[usize],
        guest_receipt: &bookclerk_plugin_abi::GuestReceiptPersist,
        cap: u32,
    ) -> std::result::Result<ExecuteReply, DbErr> {
        let peek_idx = 1;
        let raw = match self
            .run_batch_with_timeout(&statements[peek_idx..=peek_idx], timeout)
            .await
        {
            Ok(value) => {
                check_d1_session(
                    bookclerk_db_exec::AtomicInterruptPhase::AroundCommit,
                    deadline,
                )?;
                value
            }
            Err(err) => return Err(err.into()),
        };
        let peek_req = ExecuteRequest {
            operation_id: req.operation_id.clone(),
            request_hash: req.request_hash.clone(),
            statements: vec![req.statements[peek_idx].clone()],
            deadline_unix_ms: req.deadline_unix_ms,
        };
        let peek = parse_typed_batch(&peek_req, &raw, started)?;
        if let Some(prior) = peek.statements.first() {
            if bookclerk_db_exec::prior_receipt_should_resume_guest(
                prior,
                &guest_receipt.guest_request_hash,
            ) {
                return self
                    .run_claimed_guest_ddl(
                        req,
                        statements,
                        StatementResult::from_affected(0),
                        timeout,
                        deadline,
                        started,
                        companion_groups,
                        guest_receipt,
                        cap,
                    )
                    .await;
            }
        }
        let mut reply = peek;
        let mut out = vec![StatementResult::from_affected(0)];
        out.extend(std::mem::take(&mut reply.statements));
        bookclerk_db_exec::pad_skipped_guest_results(&mut out, req.statements.len());
        reply.statements = collapse_d1_wire(companion_groups, out);
        Ok(reply)
    }

    /// After a won claim, run prune + ungated guest SQL (DDL and DML) and mark
    /// the receipt `applied` in the same HTTP, then reassemble the wrap shape
    /// for unwrap/finalize.
    ///
    /// The host wrap still gates DML with [`bookclerk_db_exec::GUEST_RECEIPT_WRITE_GATE`].
    /// After the claim stub commits, that predicate would be false on first
    /// execution, so this path strips it. Resume only for `claimed` rows;
    /// `applied` means guest DML already ran.
    #[allow(clippy::too_many_arguments)]
    async fn run_claimed_guest_ddl(
        &self,
        req: &ExecuteRequest,
        statements: &[SqlStmt],
        stub: StatementResult,
        timeout: Duration,
        deadline: Option<u64>,
        started: std::time::Instant,
        companion_groups: &[usize],
        guest_receipt: &bookclerk_plugin_abi::GuestReceiptPersist,
        cap: u32,
    ) -> std::result::Result<ExecuteReply, DbErr> {
        let stub_idx = statements.len().saturating_sub(1);
        let prefix = bookclerk_db_exec::GUEST_RECEIPT_WRAP_PREFIX;
        let mut rest_sql: Vec<SqlStmt> = Vec::with_capacity(stub_idx.saturating_sub(1));
        let mut rest_req_stmts = Vec::with_capacity(stub_idx.saturating_sub(1));
        rest_sql.push(statements[0].clone());
        rest_req_stmts.push(req.statements[0].clone());
        for (mut sql_stmt, mut typed) in statements[prefix..stub_idx]
            .iter()
            .cloned()
            .zip(req.statements[prefix..stub_idx].iter().cloned())
        {
            ungate_claimed_guest_write(&mut sql_stmt, &mut typed);
            rest_sql.push(sql_stmt);
            rest_req_stmts.push(typed);
        }
        let applied = bookclerk_db_exec::guest_receipt_applied_stmt(&req.operation_id);
        let (applied_sql, applied_binds) =
            d1_typed_statement(&applied.sql, &applied.parameters, None)?;
        rest_sql.push((applied_sql, applied_binds));
        rest_req_stmts.push(applied);
        let rest_req = ExecuteRequest {
            operation_id: req.operation_id.clone(),
            request_hash: req.request_hash.clone(),
            statements: rest_req_stmts,
            deadline_unix_ms: req.deadline_unix_ms,
        };
        let raw = self
            .run_batch_with_timeout(&rest_sql, timeout)
            .await
            .map_err(DbErr::from)?;
        check_d1_session(
            bookclerk_db_exec::AtomicInterruptPhase::AroundCommit,
            deadline,
        )?;
        let mut reply = parse_typed_batch(&rest_req, &raw, started)?;
        // Applied-mark is adapter-private; unwrap still sees prune + guest + stub.
        let _applied = reply.statements.pop();
        let mut assembled = Vec::with_capacity(req.statements.len());
        assembled.push(
            reply
                .statements
                .first()
                .cloned()
                .unwrap_or_else(|| StatementResult::from_affected(0)),
        );
        assembled.push(StatementResult::from_affected(0));
        assembled.extend(reply.statements.into_iter().skip(1));
        assembled.push(stub);
        reply.statements = assembled;
        self.normalize_reply_from_declared(req, &mut reply, timeout)
            .await?;
        reply.statements = collapse_d1_wire(companion_groups, reply.statements);
        let finalize = bookclerk_db_exec::guest_receipt_finalize_stmts(
            &reply,
            usize::try_from(guest_receipt.guest_statement_len).unwrap_or(usize::MAX),
            &guest_receipt.guest_request_hash,
        )?;
        if !finalize.is_empty() {
            let fin_stmts: Vec<SqlStmt> = finalize
                .iter()
                .map(|s| {
                    let (sql, binds) = d1_typed_statement(&s.sql, &s.parameters, None)?;
                    let sql = if s.kind.wrap_select_limit() {
                        bookclerk_db_exec::cap_query_sql(&sql, cap)
                    } else {
                        sql
                    };
                    Ok((sql, binds))
                })
                .collect::<Result<Vec<_>, DbErr>>()?;
            self.run_batch_with_timeout(&fin_stmts, timeout).await?;
        }
        Ok(reply)
    }

    /// Declared-type normalization of a typed reply.
    ///
    /// D1's JSON channel carries no column metadata. Declared types come from
    /// `pragma_table_info` on tables each `Rows` statement references (cached
    /// per table, cleared on DDL). Only SELECT items that are proven direct
    /// column references (`col` / `table.col`, with or without `AS alias`)
    /// are normalized; aliases that remap a different column keep the engine
    /// type. A failed metadata fetch is **unavailable**, not a silent
    /// unnormalized reply.
    async fn normalize_reply_from_declared(
        &self,
        req: &ExecuteRequest,
        reply: &mut ExecuteReply,
        timeout: Duration,
    ) -> std::result::Result<(), DbErr> {
        use std::collections::{BTreeSet, HashMap, HashSet};

        let mut per_stmt: Vec<Option<Vec<String>>> = Vec::with_capacity(req.statements.len());
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        for stmt in &req.statements {
            if stmt.result_selection != DbResultSelection::Rows {
                per_stmt.push(None);
                continue;
            }
            match bookclerk_plugin_abi::parse_guest_sql_refs(&stmt.sql) {
                Ok(refs) => {
                    let tables: Vec<String> =
                        refs.tables.iter().map(|t| table_cache_key(t)).collect();
                    wanted.extend(tables.iter().cloned());
                    per_stmt.push(Some(tables));
                }
                Err(_) => per_stmt.push(None),
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        let missing: Vec<String> = wanted
            .iter()
            .filter(|t| self.cached_table_types(t).is_none())
            .cloned()
            .collect();
        if !missing.is_empty() {
            let stmts: Vec<SqlStmt> = missing
                .iter()
                .map(|t| {
                    (
                        "SELECT name, type FROM pragma_table_info(?)".to_string(),
                        vec![JsonValue::String(t.clone())],
                    )
                })
                .collect();
            let raw = self
                .run_batch_with_timeout(&stmts, timeout)
                .await
                .map_err(|err| {
                    DbErr::Custom(format!(
                        "unavailable: declared types for {} could not be loaded: {}",
                        missing.join(","),
                        DbErr::from(err)
                    ))
                })?;
            let Some(arr) = raw.get("result").and_then(JsonValue::as_array) else {
                return Err(DbErr::Custom(
                    "unavailable: declared-type pragma reply missing result array".into(),
                ));
            };
            for (table, entry) in missing.iter().zip(arr) {
                let mut columns = HashMap::new();
                if let Some(rows) = entry.get("results").and_then(JsonValue::as_array) {
                    for row in rows {
                        let name = row.get("name").and_then(JsonValue::as_str);
                        let decl = row.get("type").and_then(JsonValue::as_str);
                        if let (Some(name), Some(decl)) = (name, decl) {
                            columns.insert(
                                name.to_ascii_lowercase(),
                                bookclerk_plugin_abi::db_type_from_declared(decl),
                            );
                        }
                    }
                }
                self.store_table_types(table.clone(), columns);
            }
            for table in &missing {
                if self.cached_table_types(table).is_none() {
                    return Err(DbErr::Custom(format!(
                        "unavailable: declared types for `{table}` were not cached after pragma"
                    )));
                }
            }
        }
        for (i, tables) in per_stmt.iter().enumerate() {
            let Some(tables) = tables else { continue };
            let mut map: HashMap<String, DbType> = HashMap::new();
            let mut conflicted: HashSet<String> = HashSet::new();
            for table in tables {
                let Some(columns) = self.cached_table_types(table) else {
                    continue;
                };
                for (name, ty) in columns {
                    if conflicted.contains(&name) {
                        continue;
                    }
                    match map.get(&name) {
                        None => {
                            map.insert(name, ty);
                        }
                        Some(prev) if *prev != ty => {
                            map.remove(&name);
                            conflicted.insert(name);
                        }
                        Some(_) => {}
                    }
                }
            }
            if map.is_empty() {
                continue;
            }
            let Some(sql) = req.statements.get(i).map(|s| s.sql.as_str()) else {
                continue;
            };
            let Some(stmt) = reply.statements.get_mut(i) else {
                continue;
            };
            apply_proven_declared_types(stmt, sql, &map);
        }
        Ok(())
    }
}

/// HTTP timeout for one D1 batch, capped by the guest-visible deadline.
fn d1_http_timeout(deadline_unix_ms: Option<u64>) -> std::result::Result<Duration, DbErr> {
    match deadline_unix_ms {
        Some(dl) => {
            let now = d1_unix_now_ms();
            if now >= dl {
                return Err(DbErr::Custom(
                    "deadline_exceeded: atomic deadline elapsed".into(),
                ));
            }
            Ok(Duration::from_millis(dl - now).min(super::d1::D1_REQUEST_TIMEOUT))
        }
        None => Ok(super::d1::D1_REQUEST_TIMEOUT),
    }
}

/// Checks cancel/deadline inject plus a guest-visible `deadlineUnixMs`.
fn check_d1_session(
    phase: bookclerk_db_exec::AtomicInterruptPhase,
    deadline_unix_ms: Option<u64>,
) -> std::result::Result<(), DbErr> {
    use bookclerk_db_exec::{AtomicInterruptKind, AtomicInterruptPhase};
    let kind = bookclerk_db_exec::consume_atomic_interrupt(phase);
    let expired = deadline_unix_ms.is_some_and(|ms| d1_unix_now_ms() >= ms);
    let kind = kind.or_else(|| expired.then_some(AtomicInterruptKind::Deadline));
    let Some(kind) = kind else {
        return Ok(());
    };
    match phase {
        AtomicInterruptPhase::AroundCommit => Err(ambiguous_d1("session interrupt at HTTP return")),
        AtomicInterruptPhase::BeforeBegin | AtomicInterruptPhase::BetweenStatements => {
            let msg = match kind {
                AtomicInterruptKind::Cancel => "cancelled: atomic session cancelled",
                AtomicInterruptKind::Deadline => "deadline_exceeded: atomic deadline elapsed",
            };
            Err(DbErr::Custom(msg.into()))
        }
    }
}

/// Current unix time in milliseconds.
fn d1_unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Maps 1:1 wire proofs onto expanded companion groups (companions have no proof).
fn proofs_for_expanded<'a>(
    proofs: &'a [bookclerk_plugin_abi::ResolvedStatement],
    groups: &[usize],
) -> Result<Vec<Option<&'a bookclerk_plugin_abi::ResolvedStatement>>, DbErr> {
    let expanded_len: usize = groups.iter().copied().sum();
    if proofs.is_empty() {
        return Err(DbErr::Custom(
            "host execute envelope proofs must match statement count".into(),
        ));
    }
    if proofs.len() != groups.len() {
        return Err(DbErr::Custom(
            "host execute envelope proofs must match statement count".into(),
        ));
    }
    let mut out = Vec::with_capacity(expanded_len);
    for (proof, &g) in proofs.iter().zip(groups.iter()) {
        out.push(Some(proof));
        for _ in 1..g {
            out.push(None);
        }
    }
    Ok(out)
}

/// Rejects lowered D1 SQL that exceeds Cloudflare's physical statement limit.
fn ensure_d1_physical_sql(sql: &str) -> Result<(), DbErr> {
    if sql.len() > D1_MAX_SQL_STATEMENT_BYTES as usize {
        return Err(DbErr::Custom(format!(
            "lowered SQL is {} bytes; D1 physical statement limit is {D1_MAX_SQL_STATEMENT_BYTES}",
            sql.len()
        )));
    }
    Ok(())
}

/// D1 HTTP statement for one typed statement: `Bytes` placeholders are
/// rewritten to `unhex(?)` with hex-encoded text params so D1 stores true
/// BLOBs (JSON has no binary scalar). All other values map directly; text is
/// never re-encoded, so a `Text` cell can never masquerade as bytes.
pub(crate) fn d1_typed_statement(
    sql: &str,
    params: &[DbValue],
    proof: Option<&bookclerk_plugin_abi::ResolvedStatement>,
) -> Result<SqlStmt, DbErr> {
    let pre = lowered_statement_preflight_len_proven(sql, params.len(), proof)
        .map_err(|err| DbErr::Custom(format!("D1 physical SQL preflight: {err}")))?;
    if pre > D1_MAX_SQL_STATEMENT_BYTES as usize {
        return Err(DbErr::Custom(format!(
            "D1 physical SQL preflight is {pre} bytes; D1 physical statement limit is {D1_MAX_SQL_STATEMENT_BYTES}"
        )));
    }
    let sql =
        bookclerk_db_exec::lower_canonical_sql_typed(sea_orm::DatabaseBackend::Sqlite, sql, proof)
            .map_err(|err| DbErr::Custom(err.to_string()))?;
    let sql = wrap_bytes_placeholders(&sql, params);
    ensure_d1_physical_sql(&sql)?;
    let binds = params
        .iter()
        .map(|v| match v {
            DbValue::Null(_) => JsonValue::Null,
            DbValue::Boolean(b) => JsonValue::Bool(*b),
            DbValue::Int64(n) => JsonValue::from(*n),
            DbValue::Float64(n) => JsonValue::from(*n),
            DbValue::Text(s) => JsonValue::String(s.clone()),
            DbValue::Bytes(b) => JsonValue::String(hex::encode(b)),
        })
        .collect();
    Ok((sql, binds))
}

/// Rewrites the i-th `?` placeholder to `unhex(?)` when parameter `i` is
/// [`DbValue::Bytes`]. Quoted strings / identifiers are copied verbatim.
fn wrap_bytes_placeholders(sql: &str, params: &[DbValue]) -> String {
    if !params.iter().any(|p| matches!(p, DbValue::Bytes(_))) {
        return sql.to_string();
    }
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut i = 0usize;
    let mut param = 0usize;
    let mut in_squote = false;
    let mut in_dquote = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_squote {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    out.push_str("''");
                    i += 2;
                    continue;
                }
                in_squote = false;
            }
            out.push(c as char);
            i += 1;
            continue;
        }
        if in_dquote {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    out.push_str("\"\"");
                    i += 2;
                    continue;
                }
                in_dquote = false;
            }
            out.push(c as char);
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_squote = true;
                out.push('\'');
                i += 1;
            }
            b'"' => {
                in_dquote = true;
                out.push('"');
                i += 1;
            }
            b'?' => {
                if matches!(params.get(param), Some(DbValue::Bytes(_))) {
                    out.push_str("unhex(?)");
                } else {
                    out.push('?');
                }
                param += 1;
                i += 1;
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap_or('\0');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Waits before a D1 retry, honoring `Retry-After` or a capped exponential backoff.
async fn sleep_before_d1_retry_bounded(
    attempt: usize,
    retry_after: Option<Duration>,
    deadline_unix_ms: Option<u64>,
) -> std::result::Result<(), DbErr> {
    let delay = retry_after.unwrap_or_else(|| {
        Duration::from_millis((50u64.saturating_mul(3u64.saturating_pow(attempt as u32))).min(400))
    });
    let delay = delay.min(Duration::from_secs(5));
    if let Some(dl) = deadline_unix_ms {
        let now = d1_unix_now_ms();
        if now >= dl {
            return Err(DbErr::Custom(
                "deadline_exceeded: atomic deadline elapsed".into(),
            ));
        }
        let remain = Duration::from_millis(dl - now);
        tokio::time::sleep(delay.min(remain)).await;
        if d1_unix_now_ms() >= dl {
            return Err(DbErr::Custom(
                "deadline_exceeded: atomic deadline elapsed".into(),
            ));
        }
        return Ok(());
    }
    tokio::time::sleep(delay).await;
    Ok(())
}

/// True when `err` is a post-commit declared-type / result recovery failure.
///
/// SeaORM prefixes [`DbErr::Custom`] with `Custom Error: `, so match anywhere
/// rather than at the start of `Display`.
fn is_post_commit_unavailable(err: &DbErr) -> bool {
    err.to_string().contains("unavailable:")
}

/// True when `err` is a D1 ambiguous-commit marker.
pub fn is_ambiguous_d1(err: &DbErr) -> bool {
    err.to_string().contains("D1 ambiguous")
}

/// Maps a D1 [`DbErr`] onto the guest ABI: retryable/ambiguous → `unavailable`,
/// client 4xx → `invalid_params`, engine codes via the shared mapper.
#[must_use]
pub fn plugin_error_from_d1(err: DbErr) -> PluginError {
    if bookclerk_db_exec::is_guest_receipt_result_lost(&err) {
        return PluginError::unavailable(err.to_string());
    }
    if is_ambiguous_d1(&err) {
        return PluginError::unavailable(err.to_string());
    }
    if is_post_commit_unavailable(&err) {
        return PluginError::unavailable(err.to_string());
    }
    if let Some(status) = permanent_http_status(&err) {
        if (400..500).contains(&status) {
            return PluginError::invalid_params(err.to_string());
        }
    }
    bookclerk_plugin_sdk::database_adapter::plugin_error_from_engine(err)
}

/// Extracts a permanent `D1 HTTP {status}` code from a [`DbErr`], if present.
fn permanent_http_status(err: &DbErr) -> Option<u16> {
    let text = err.to_string();
    let idx = text.find("D1 HTTP ")?;
    text[idx + "D1 HTTP ".len()..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Builds a [`DbErr`] whose message marks the batch as possibly already committed.
fn ambiguous_d1(msg: impl std::fmt::Display) -> DbErr {
    DbErr::Custom(format!("D1 ambiguous response: {msg}"))
}

/// Fails closed on DML `RETURNING` that D1 HTTP cannot prove is at most one row.
///
/// # Errors
///
/// Returns when SQL is multi-statement, `max_rows != 1`, or `VALUES` is not a
/// single tuple.
fn reject_unbounded_returning_typed(
    statements: &[TypedDbStatement],
) -> std::result::Result<(), DbErr> {
    let cap = DbCapabilities::advertised_d1().max_result_rows;
    for (i, stmt) in statements.iter().enumerate() {
        if sql_has_top_level_semicolon(&stmt.sql) {
            return Err(DbErr::Custom(format!(
                "D1 statement {i} contains multiple SQL statements; maxResultRows is {cap}"
            )));
        }
        let looks_returning = matches!(stmt.kind, DbPlanStatementKind::Returning);
        if !looks_returning {
            continue;
        }
        if stmt.max_rows != 1 {
            return Err(DbErr::Custom(format!(
                "D1 statement {i} Returning is not proven bounded; maxResultRows is {cap}"
            )));
        }
        if has_top_level_keyword(&stmt.sql, "VALUES") {
            let tuples = count_top_level_values_tuples(&stmt.sql);
            if tuples != 1 {
                return Err(DbErr::Custom(format!(
                    "D1 statement {i} Returning VALUES is not a single tuple ({tuples}); maxResultRows is {cap}"
                )));
            }
        }
    }
    Ok(())
}

/// Maps one D1 HTTP JSON cell onto a typed [`DbValue`] (strings stay text).
///
/// BLOB cells arrive as JSON arrays of byte integers (Cloudflare D1
/// type-conversion: BLOB reads as arrays) and decode to [`DbValue::Bytes`].
///
/// # Errors
///
/// Returns when the cell is a non-byte array, an object, or a non-finite
/// number.
fn d1_json_cell_to_db_value(v: &JsonValue) -> Result<DbValue, String> {
    match v {
        JsonValue::Null => Ok(DbValue::Null(DbType::Unspecified)),
        JsonValue::Bool(b) => Ok(DbValue::Boolean(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Ok(DbValue::Int64(i));
            }
            if let Some(u) = n.as_u64() {
                let i = i64::try_from(u)
                    .map_err(|_| format!("unsigned integer {u} overflows int64"))?;
                return Ok(DbValue::Int64(i));
            }
            let f = n
                .as_f64()
                .ok_or_else(|| "number is not a finite float64".to_string())?;
            if !f.is_finite() {
                return Err("float64 value is not finite".into());
            }
            Ok(DbValue::Float64(f))
        }
        JsonValue::String(s) => Ok(DbValue::Text(s.clone())),
        JsonValue::Array(cells) => {
            let mut bytes = Vec::with_capacity(cells.len());
            for cell in cells {
                let n = cell
                    .as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| "array cell is not a D1 BLOB byte array".to_string())?;
                bytes.push(n);
            }
            Ok(DbValue::Bytes(bytes))
        }
        JsonValue::Object(_) => Err("objects are not a baseline DbValue".into()),
    }
}

/// Column metadata for a D1 HTTP result page.
///
/// JSON row objects are unordered, so the positional column order comes from
/// the SELECT list whenever the parsed identifiers cover the row keys
/// exactly; otherwise the first row's keys are used. Empty pages (and `{}`
/// all-null objects that omit keys) fall back to the SELECT-list identifiers.
fn d1_result_columns(sql: &str, raw_rows: &[JsonValue]) -> Vec<DbColumn> {
    let unspecified = |name: String| DbColumn {
        name,
        db_type: DbType::Unspecified,
    };
    let select_names = select_list_column_names(sql);
    if let Some(map) = raw_rows.first().and_then(JsonValue::as_object) {
        if !map.is_empty() {
            if select_names.len() == map.len()
                && select_names.iter().all(|name| map.contains_key(name))
            {
                return select_names.into_iter().map(unspecified).collect();
            }
            return map.keys().cloned().map(unspecified).collect();
        }
    }
    select_names.into_iter().map(unspecified).collect()
}

/// Fills [`DbType`] from the first non-null cell in each column.
fn refine_column_types(columns: &mut [DbColumn], rows: &[DbRow]) {
    for (i, col) in columns.iter_mut().enumerate() {
        if col.db_type != DbType::Unspecified {
            continue;
        }
        for row in rows {
            match row.values.get(i) {
                Some(DbValue::Null(_)) | None => continue,
                Some(DbValue::Boolean(_)) => col.db_type = DbType::Bool,
                Some(DbValue::Int64(_)) => col.db_type = DbType::Int64,
                Some(DbValue::Float64(_)) => col.db_type = DbType::Float64,
                Some(DbValue::Text(_)) => col.db_type = DbType::Text,
                Some(DbValue::Bytes(_)) => col.db_type = DbType::Bytes,
            }
            break;
        }
    }
}

/// SELECT-list identifiers used when D1 HTTP returns no row objects.
fn select_list_column_names(sql: &str) -> Vec<String> {
    let select = match select_list_slice(sql) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut names = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let bytes = select.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                if let Some(name) = select_item_name(&select[start..i]) {
                    names.push(name);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    if let Some(name) = select_item_name(&select[start..]) {
        names.push(name);
    }
    names
}

/// True when `bytes[i..]` is keyword `kw` not continued as an identifier.
fn starts_keyword(bytes: &[u8], i: usize, kw: &[u8]) -> bool {
    bytes[i..].starts_with(kw) && !ident_cont_at(bytes, i + kw.len())
}

/// Text of the main SELECT list (depth-0): through `FROM`, or through
/// `WHERE`/`GROUP`/`HAVING`/`ORDER`/`LIMIT`/`UNION`/`EXCEPT`/`INTERSECT`/
/// `WINDOW` when there is no `FROM` (literal `SELECT …` rows).
fn select_list_slice(sql: &str) -> Option<&str> {
    let upper = sql.to_ascii_uppercase();
    let mut depth = 0i32;
    let bytes = upper.as_bytes();
    let mut i = 0usize;
    let mut select_at = None;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'S' if depth == 0 && starts_keyword(bytes, i, b"SELECT") => {
                select_at = Some(i + 6);
                i += 6;
            }
            b'F' if depth == 0 && select_at.is_some() && starts_keyword(bytes, i, b"FROM") => {
                return Some(sql[select_at?..i].trim());
            }
            b'W' if depth == 0
                && select_at.is_some()
                && (starts_keyword(bytes, i, b"WHERE") || starts_keyword(bytes, i, b"WINDOW")) =>
            {
                return Some(sql[select_at?..i].trim());
            }
            b'G' if depth == 0 && select_at.is_some() && starts_keyword(bytes, i, b"GROUP") => {
                return Some(sql[select_at?..i].trim());
            }
            b'H' if depth == 0 && select_at.is_some() && starts_keyword(bytes, i, b"HAVING") => {
                return Some(sql[select_at?..i].trim());
            }
            b'O' if depth == 0 && select_at.is_some() && starts_keyword(bytes, i, b"ORDER") => {
                return Some(sql[select_at?..i].trim());
            }
            b'L' if depth == 0 && select_at.is_some() && starts_keyword(bytes, i, b"LIMIT") => {
                return Some(sql[select_at?..i].trim());
            }
            b'U' if depth == 0 && select_at.is_some() && starts_keyword(bytes, i, b"UNION") => {
                return Some(sql[select_at?..i].trim());
            }
            b'E' if depth == 0 && select_at.is_some() && starts_keyword(bytes, i, b"EXCEPT") => {
                return Some(sql[select_at?..i].trim());
            }
            b'I' if depth == 0 && select_at.is_some() && starts_keyword(bytes, i, b"INTERSECT") => {
                return Some(sql[select_at?..i].trim());
            }
            _ => i += 1,
        }
    }
    select_at.map(|at| sql[at..].trim())
}

/// True when `bytes[i]` continues an identifier (`[A-Za-z0-9_]`).
fn ident_cont_at(bytes: &[u8], i: usize) -> bool {
    bytes
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Last identifier in a SELECT item (`col`, `table.col`, `expr AS alias`).
fn select_item_name(item: &str) -> Option<String> {
    let item = item.trim();
    if item.is_empty() || item == "*" || item.ends_with(".*") {
        return None;
    }
    let upper = item.to_ascii_uppercase();
    let token = if let Some(idx) = upper.rfind(" AS ") {
        item[idx + 4..].trim()
    } else {
        item.rsplit([' ', '.']).next().unwrap_or(item).trim()
    };
    let token = token.trim_matches(|c| c == '"' || c == '`' || c == '\'');
    if token.is_empty()
        || token == "*"
        || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(token.to_string())
}

/// Source column of a SELECT item when it is a proven direct reference.
///
/// `col`, `table.col`, and `expr AS alias` where `expr` is one of those
/// forms. Computed expressions (`n + 1`, `NOT flag`) return `None` so the
/// engine type is kept instead of matching the output alias to a physical
/// column (the `SELECT n AS flag` boolean collision).
fn select_item_proven_column(item: &str) -> Option<String> {
    let item = item.trim();
    if item.is_empty() || item == "*" || item.ends_with(".*") {
        return None;
    }
    let upper = item.to_ascii_uppercase();
    let expr = if let Some(idx) = upper.rfind(" AS ") {
        item[..idx].trim()
    } else {
        item
    };
    let expr = expr.trim_matches(|c| c == '"' || c == '`' || c == '\'');
    if expr.is_empty() || expr.contains('(') || expr.contains('+') || expr.contains(' ') {
        return None;
    }
    let col = expr.rsplit('.').next().unwrap_or(expr).trim();
    let col = col.trim_matches(|c| c == '"' || c == '`' || c == '\'');
    if col.is_empty() || col == "*" || !col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(col.to_ascii_lowercase())
}

/// Proven source column per SELECT-list item (same split as [`select_list_column_names`]).
fn select_list_proven_columns(sql: &str) -> Vec<Option<String>> {
    let select = match select_list_slice(sql) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut cols = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let bytes = select.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                cols.push(select_item_proven_column(&select[start..i]));
                start = i + 1;
            }
            _ => {}
        }
    }
    cols.push(select_item_proven_column(&select[start..]));
    cols
}

/// Applies declared types to SELECT items proven to be direct columns.
///
/// Declared metadata wins over JSON-inferred types (D1 stores booleans as
/// integers, so a `BOOLEAN` column would otherwise stay `Int64`).
fn apply_proven_declared_types(
    stmt: &mut StatementResult,
    sql: &str,
    map: &std::collections::HashMap<String, DbType>,
) {
    let proven = select_list_proven_columns(sql);
    for col_idx in 0..stmt.columns.len() {
        let key = proven
            .get(col_idx)
            .and_then(|c| c.as_deref())
            .map(str::to_string);
        let Some(key) = key else { continue };
        let Some(&ty) = map.get(&key) else { continue };
        stmt.columns[col_idx].db_type = ty;
        for row in &mut stmt.rows {
            if let Some(cell) = row.values.get_mut(col_idx) {
                *cell = bookclerk_plugin_abi::normalize_db_value_for_column(cell.clone(), ty);
            }
        }
    }
}

/// Parses a D1 HTTP batch body into [`ExecuteReply`] and encodes before return.
///
/// # Errors
///
/// Returns [`DbErr`] when the body is malformed, a row fails conversion, or
/// the encoded reply exceeds `maxAtomicResultBytes` (ambiguous after HTTP).
fn parse_typed_batch(
    req: &ExecuteRequest,
    value: &JsonValue,
    started: std::time::Instant,
) -> std::result::Result<ExecuteReply, DbErr> {
    let Some(arr) = value.get("result").and_then(JsonValue::as_array) else {
        return Err(ambiguous_d1("batch response missing result array"));
    };
    if arr.len() != req.statements.len() {
        return Err(ambiguous_d1(format!(
            "expected {} statement results, got {}",
            req.statements.len(),
            arr.len()
        )));
    }
    let caps = DbCapabilities::advertised_d1();
    let row_cap = usize::try_from(caps.max_result_rows).unwrap_or(1_000);
    let mut statements = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        if entry.get("success").and_then(JsonValue::as_bool) == Some(false) {
            return Err(DbErr::Custom(format!(
                "D1 batch statement {i} failed: {entry}"
            )));
        }
        let kind = req.statements[i].kind;
        let selection = req.statements[i].result_selection;
        let changes = entry
            .get("meta")
            .and_then(|m| m.get("changes"))
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let stmt_result = match selection {
            DbResultSelection::AffectedRows | DbResultSelection::Discard => {
                let n = if matches!(selection, DbResultSelection::Discard)
                    || matches!(kind, DbPlanStatementKind::Select)
                {
                    0
                } else {
                    changes
                };
                StatementResult::from_affected(n)
            }
            DbResultSelection::Rows => {
                let raw_rows = entry
                    .get("results")
                    .and_then(JsonValue::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if raw_rows.len() > row_cap {
                    return Err(ambiguous_d1(format!(
                        "D1 statement {i} returned {} rows; maxResultRows is {row_cap}",
                        raw_rows.len()
                    )));
                }
                let mut columns: Vec<DbColumn> =
                    d1_result_columns(&req.statements[i].sql, raw_rows);
                let mut rows = Vec::new();
                for row in raw_rows {
                    let Some(map) = row.as_object() else {
                        return Err(ambiguous_d1(format!(
                            "D1 statement {i} row is not an object"
                        )));
                    };
                    let mut values = Vec::with_capacity(columns.len());
                    for col in &columns {
                        let cell = map.get(&col.name).unwrap_or(&JsonValue::Null);
                        values.push(d1_json_cell_to_db_value(cell).map_err(DbErr::Custom)?);
                    }
                    rows.push(DbRow { values });
                }
                refine_column_types(&mut columns, &rows);
                let mut result = StatementResult::from_rows(columns, rows).map_err(ambiguous_d1)?;
                result.rows_affected = match kind {
                    DbPlanStatementKind::Select => 0,
                    DbPlanStatementKind::Returning => {
                        u64::try_from(result.rows.len()).unwrap_or(u64::MAX)
                    }
                    DbPlanStatementKind::Execute => changes,
                };
                result
            }
        };
        if caps.max_result_bytes > 0 {
            let used = encoded_statement_result_bytes(&stmt_result)
                .map_err(ambiguous_d1)?
                .len();
            let cap = usize::try_from(caps.max_result_bytes).unwrap_or(usize::MAX);
            if used > cap {
                return Err(ambiguous_d1(format!(
                    "query result is {used} bytes; maxResultBytes is {}",
                    caps.max_result_bytes
                )));
            }
        }
        statements.push(stmt_result);
    }
    let db_execution_us = d1_sql_duration_us(value);
    let reply = ExecuteReply {
        operation_id: req.operation_id.clone(),
        statements,
        timing: DbTiming {
            attempt_elapsed_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            db_execution_us: db_execution_us.unwrap_or(0),
            db_timing_source: db_execution_us
                .map(|_| "d1_sql_duration".to_string())
                .unwrap_or_default(),
        },
    };
    reply.validate_positional().map_err(ambiguous_d1)?;
    match encoded_execute_reply_bytes(&reply) {
        Ok(bytes) => {
            let cap = usize::try_from(caps.max_atomic_result_bytes).unwrap_or(0);
            if cap > 0 && bytes.len() > cap {
                return Err(ambiguous_d1(format!(
                    "atomic result is {} bytes; maxAtomicResultBytes is {cap}",
                    bytes.len()
                )));
            }
        }
        Err(err) => {
            return Err(ambiguous_d1(format!(
                "failed to encode ExecuteReply after D1 HTTP commit: {err}"
            )));
        }
    }
    Ok(reply)
}

/// True when a top-level semicolon would start another statement.
fn sql_has_top_level_semicolon(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    let mut in_squote = false;
    let mut in_dquote = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_squote {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_squote = false;
            }
            i += 1;
            continue;
        }
        if in_dquote {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_dquote = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_squote = true,
            b'"' => in_dquote = true,
            b'(' => depth = depth.saturating_add(1),
            b')' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => {
                let rest = sql[i + 1..].trim();
                if !rest.is_empty() {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Number of top-level `VALUES` tuples (`(1),(2)` → 2). `0` when none parsed.
fn count_top_level_values_tuples(sql: &str) -> usize {
    let Some(idx) = top_level_keyword_index(sql, "VALUES") else {
        return 0;
    };
    let bytes = sql.as_bytes();
    let mut i = idx + "VALUES".len();
    let mut depth = 0usize;
    let mut tuples = 0usize;
    let mut in_squote = false;
    let mut in_dquote = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_squote {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_squote = false;
            }
            i += 1;
            continue;
        }
        if in_dquote {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_dquote = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_squote = true,
            b'"' => in_dquote = true,
            b'(' => {
                if depth == 0 {
                    tuples = tuples.saturating_add(1);
                }
                depth = depth.saturating_add(1);
            }
            b')' => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0 {
                    let rest = &sql[i..];
                    if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case("RETURNING") {
                        break;
                    }
                    if c == b';' {
                        break;
                    }
                }
            }
        }
        i += 1;
    }
    tuples
}

/// Byte offset of a top-level keyword, if present.
fn top_level_keyword_index(sql: &str, keyword: &str) -> Option<usize> {
    let mut found = None;
    for_each_top_level_keyword(sql, |idx, kw| {
        if kw.eq_ignore_ascii_case(keyword) {
            found = Some(idx);
        }
    });
    found
}

/// True when `keyword` appears at parenthesis depth 0 (not inside quotes).
fn has_top_level_keyword(sql: &str, keyword: &str) -> bool {
    let mut found = false;
    for_each_top_level_keyword(sql, |_, kw| {
        if kw.eq_ignore_ascii_case(keyword) {
            found = true;
        }
    });
    found
}

/// Invokes `on_keyword` for each unquoted, top-level identifier.
fn for_each_top_level_keyword(sql: &str, mut on_keyword: impl FnMut(usize, &str)) {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    let mut in_squote = false;
    let mut in_dquote = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_squote {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_squote = false;
            }
            i += 1;
            continue;
        }
        if in_dquote {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_dquote = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_squote = true;
                i += 1;
            }
            b'"' => {
                in_dquote = true;
                i += 1;
            }
            b'(' => {
                depth = depth.saturating_add(1);
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0 && c.is_ascii_alphabetic() => {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                on_keyword(start, &sql[start..i]);
            }
            _ => i += 1,
        }
    }
}

/// Sums D1 `sql_duration_ms` timings from a batch response and returns microseconds.
fn d1_sql_duration_us(raw: &JsonValue) -> Option<u64> {
    let arr = raw.get("result")?.as_array()?;
    let mut ms = 0.0_f64;
    let mut any = false;
    for entry in arr {
        if let Some(duration) = entry
            .get("meta")
            .and_then(|m| m.get("timings"))
            .and_then(|t| t.get("sql_duration_ms"))
            .and_then(JsonValue::as_f64)
        {
            ms += duration;
            any = true;
        }
    }
    any.then_some((ms * 1000.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn proofs_for_expanded_keeps_original_and_skips_companions() {
        let original = bookclerk_plugin_abi::ResolvedStatement::bound_empty("SELECT 1");
        let create = bookclerk_plugin_abi::ResolvedStatement::bound_empty(
            "CREATE TABLE IF NOT EXISTS t (n INTEGER)",
        );
        let proofs = [original, create];
        let mapped = proofs_for_expanded(&proofs, &[1, 3]).expect("map");
        assert_eq!(mapped.len(), 4);
        assert!(mapped[0].is_some());
        assert!(mapped[1].is_some());
        assert!(mapped[2].is_none());
        assert!(mapped[3].is_none());
        proofs_for_expanded(&[], &[1, 2]).expect_err("missing proofs must fail closed");
        proofs_for_expanded(
            &[bookclerk_plugin_abi::ResolvedStatement::bound_empty(
                "SELECT 1",
            )],
            &[1, 1],
        )
        .expect_err("misaligned sidecar");
    }

    fn typed_stmt(sql: &str, kind: DbPlanStatementKind, max_rows: u32) -> TypedDbStatement {
        TypedDbStatement {
            sql: sql.into(),
            parameters: vec![],
            kind,
            max_rows,
            result_selection: if kind == DbPlanStatementKind::Execute {
                DbResultSelection::AffectedRows
            } else {
                DbResultSelection::Rows
            },
        }
    }

    fn typed_req(op: &str, statements: Vec<TypedDbStatement>) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: op.into(),
            request_hash: String::new(),
            statements,
            deadline_unix_ms: 0,
        }
    }

    fn typed_select(sql: &str) -> ExecuteRequest {
        typed_req("d1", vec![typed_stmt(sql, DbPlanStatementKind::Select, 8)])
    }

    #[test]
    fn d1_typed_statement_rejects_over_physical_sql() {
        let sql = format!(
            "SELECT '{}'",
            "a".repeat(bookclerk_plugin_abi::D1_MAX_SQL_STATEMENT_BYTES as usize)
        );
        let err = d1_typed_statement(&sql, &[], None).expect_err("physical cap");
        assert!(
            err.to_string().contains("physical statement limit"),
            "{err}"
        );
        d1_typed_statement("SELECT 1", &[], None).expect("short SQL");
    }

    #[test]
    fn d1_typed_statement_rejects_overflow_at_preflight() {
        let mut sql = String::from("SELECT 1");
        for _ in 0..400 {
            sql.push_str("+1");
        }
        let req = bookclerk_plugin_abi::ExecuteRequest {
            operation_id: "t".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![bookclerk_plugin_abi::TypedDbStatement {
                sql: sql.clone(),
                parameters: vec![],
                kind: bookclerk_plugin_abi::DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: bookclerk_plugin_abi::DbResultSelection::Rows,
            }],
        };
        let proof = bookclerk_plugin_abi::typecheck_execute_request_proofs(
            &req,
            &bookclerk_plugin_abi::SqlTypeEnv::new(),
        )
        .expect("typecheck")
        .remove(0);
        let err = d1_typed_statement(&sql, &[], Some(&proof)).expect_err("overflow preflight");
        let msg = err.to_string();
        assert!(
            msg.contains("D1 physical SQL preflight"),
            "must fail before lowering: {msg}"
        );
        assert!(
            !msg.contains("lowered SQL is"),
            "must not be the post-lower 100KiB check: {msg}"
        );
    }

    #[test]
    fn d1_typed_statement_rejects_column_overflow_when_host_is_bypassed() {
        let mut sql = String::from("SELECT n");
        for _ in 0..400 {
            sql.push_str("+n");
        }
        sql.push_str(" FROM t");
        let mut env = bookclerk_plugin_abi::SqlTypeEnv::new();
        env.insert_table(
            "t",
            vec![("n".into(), bookclerk_plugin_abi::SqlType::Integer)],
        );
        let req = bookclerk_plugin_abi::ExecuteRequest {
            operation_id: "t".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![bookclerk_plugin_abi::TypedDbStatement {
                sql: sql.clone(),
                parameters: vec![],
                kind: bookclerk_plugin_abi::DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: bookclerk_plugin_abi::DbResultSelection::Rows,
            }],
        };
        let proof = bookclerk_plugin_abi::typecheck_execute_request_proofs(&req, &env)
            .expect("column typecheck")
            .remove(0);
        assert!(!proof.integer_arith_sites.is_empty());
        let err = d1_typed_statement(&sql, &[], Some(&proof)).expect_err("adapter preflight");
        let msg = err.to_string();
        assert!(
            msg.contains("D1 physical SQL preflight"),
            "adapter must reject oversized column arithmetic independently: {msg}"
        );
    }

    #[test]
    fn missing_result_array_is_ambiguous() {
        let req = typed_req("op-1", vec![]);
        let started = std::time::Instant::now();
        let err = parse_typed_batch(&req, &json!({}), started).unwrap_err();
        assert!(is_ambiguous_d1(&err), "{err}");
    }

    #[test]
    fn statement_failure_is_not_ambiguous() {
        let req = typed_req(
            "op-fail",
            vec![typed_stmt(
                "INSERT INTO t (k) VALUES ('a')",
                DbPlanStatementKind::Execute,
                0,
            )],
        );
        let value = json!({
            "result": [{ "success": false, "error": "constraint" }]
        });
        let err = parse_typed_batch(&req, &value, std::time::Instant::now()).unwrap_err();
        assert!(!is_ambiguous_d1(&err), "{err}");
    }

    #[test]
    fn parse_caps_result_rows() {
        let req = typed_select("SELECT 1");
        let mut rows = Vec::new();
        for i in 0..1_050 {
            rows.push(json!({ "n": i }));
        }
        let value = json!({
            "result": [{ "success": true, "results": rows, "meta": { "changes": 0 } }]
        });
        let started = std::time::Instant::now();
        let err = parse_typed_batch(&req, &value, started).unwrap_err();
        assert!(
            err.to_string().contains("maxResultRows"),
            "D1 must fail closed on over-cap rows: {err}"
        );
        assert!(
            is_ambiguous_d1(&err),
            "overflow after HTTP commit must be ambiguous: {err}"
        );
        let at_cap = json!({
            "result": [{ "success": true, "results": rows[..1000].to_vec(), "meta": { "changes": 0 } }]
        });
        let exec = parse_typed_batch(&req, &at_cap, started).unwrap();
        assert_eq!(exec.statements[0].rows.len(), 1_000);
    }

    #[test]
    fn host_insert_returning_is_proven_single_row() {
        let sql = "INSERT OR IGNORE INTO event_deliveries (id) \
             SELECT ? WHERE EXISTS (SELECT 1 FROM domain_events WHERE id = ?) RETURNING id";
        reject_unbounded_returning_typed(&[typed_stmt(sql, DbPlanStatementKind::Returning, 1)])
            .unwrap();
    }

    #[test]
    fn recursive_insert_returning_is_rejected_before_http() {
        let sql = "WITH RECURSIVE t(id) AS (SELECT 0 UNION ALL SELECT id+1 FROM t WHERE id < 5) \
             INSERT INTO vec_ret_ins (id) SELECT id FROM t RETURNING id";
        let err =
            reject_unbounded_returning_typed(&[typed_stmt(sql, DbPlanStatementKind::Returning, 0)])
                .unwrap_err();
        assert!(err.to_string().contains("maxResultRows"), "{err}");
        assert!(!is_ambiguous_d1(&err), "{err}");
    }

    #[test]
    fn multi_values_returning_is_not_proven() {
        let sql = "INSERT INTO t(id) VALUES (1),(2) RETURNING id";
        let err =
            reject_unbounded_returning_typed(&[typed_stmt(sql, DbPlanStatementKind::Returning, 1)])
                .unwrap_err();
        assert!(err.to_string().contains("VALUES"), "{err}");
        assert!(err.to_string().contains("maxResultRows"), "{err}");
        assert_eq!(count_top_level_values_tuples(sql), 2);
    }

    #[test]
    fn semicolon_joined_sql_is_rejected() {
        let sql = "INSERT INTO t(id) VALUES (1); INSERT INTO t(id) VALUES (2)";
        let err =
            reject_unbounded_returning_typed(&[typed_stmt(sql, DbPlanStatementKind::Execute, 0)])
                .unwrap_err();
        assert!(err.to_string().contains("multiple SQL statements"), "{err}");
    }

    #[test]
    fn empty_select_uses_select_list_column_names() {
        let req = typed_select("SELECT id, title FROM books WHERE 0");
        let value = json!({
            "result": [{ "success": true, "results": [], "meta": { "changes": 0 } }]
        });
        let reply = parse_typed_batch(&req, &value, std::time::Instant::now()).unwrap();
        let names: Vec<&str> = reply.statements[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["id", "title"]);
        assert!(reply.statements[0].rows.is_empty());
    }

    #[test]
    fn select_without_from_keeps_select_list_order() {
        let req = typed_select("SELECT '汉语' AS cjk, 'مرحبا' AS ar");
        let value = json!({
            "result": [{
                "success": true,
                "results": [{ "ar": "مرحبا", "cjk": "汉语" }],
                "meta": { "changes": 0 }
            }]
        });
        let reply = parse_typed_batch(&req, &value, std::time::Instant::now()).unwrap();
        let names: Vec<&str> = reply.statements[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["cjk", "ar"]);
        assert_eq!(
            reply.statements[0].rows[0].values,
            vec![DbValue::Text("汉语".into()), DbValue::Text("مرحبا".into())]
        );
    }

    #[test]
    fn all_null_row_keeps_column_names_and_null_cells() {
        let req = typed_select("SELECT id, title FROM books");
        let value = json!({
            "result": [{
                "success": true,
                "results": [{ "id": null, "title": null }],
                "meta": { "changes": 0 }
            }]
        });
        let reply = parse_typed_batch(&req, &value, std::time::Instant::now()).unwrap();
        assert_eq!(reply.statements[0].columns.len(), 2);
        assert_eq!(
            reply.statements[0].rows[0].values,
            vec![
                DbValue::Null(DbType::Unspecified),
                DbValue::Null(DbType::Unspecified)
            ]
        );
    }

    #[test]
    fn text_starting_with_b64_stays_text() {
        let req = typed_select("SELECT note FROM books");
        for note in ["b64:not-bytes", "b64:YWJj"] {
            let value = json!({
                "result": [{
                    "success": true,
                    "results": [{ "note": note }],
                    "meta": { "changes": 0 }
                }]
            });
            let reply = parse_typed_batch(&req, &value, std::time::Instant::now()).unwrap();
            assert_eq!(
                reply.statements[0].rows[0].values[0],
                DbValue::Text(note.into()),
                "{note}"
            );
            assert_eq!(reply.statements[0].columns[0].db_type, DbType::Text);
        }
    }

    #[test]
    fn proven_column_ignores_alias_collision() {
        assert_eq!(select_item_proven_column("n AS flag").as_deref(), Some("n"));
        assert_eq!(select_item_proven_column("flag").as_deref(), Some("flag"));
        assert_eq!(select_item_proven_column("x.flag").as_deref(), Some("flag"));
        assert_eq!(select_item_proven_column("n + 1 AS flag"), None);
        assert_eq!(select_item_proven_column("NOT flag"), None);
        let sql = "SELECT n AS flag, flag FROM x";
        assert_eq!(
            select_list_proven_columns(sql),
            vec![Some("n".into()), Some("flag".into())]
        );
        let mut stmt = StatementResult {
            rows: vec![DbRow {
                values: vec![DbValue::Int64(1), DbValue::Int64(1)],
            }],
            columns: vec![
                DbColumn {
                    name: "flag".into(),
                    db_type: DbType::Unspecified,
                },
                DbColumn {
                    name: "flag".into(),
                    db_type: DbType::Unspecified,
                },
            ],
            rows_affected: 0,
        };
        let mut map = std::collections::HashMap::new();
        map.insert("flag".into(), DbType::Bool);
        map.insert("n".into(), DbType::Int64);
        apply_proven_declared_types(&mut stmt, sql, &map);
        assert_eq!(stmt.rows[0].values[0], DbValue::Int64(1));
        assert_eq!(stmt.rows[0].values[1], DbValue::Boolean(true));
        assert_eq!(stmt.columns[0].db_type, DbType::Int64);
        assert_eq!(stmt.columns[1].db_type, DbType::Bool);
    }
}
