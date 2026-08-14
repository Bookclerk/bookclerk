//! Named atomic library operations for Cloudflare D1 HTTP `batch()`.
//!
//! Each [`DbAtomicParams`] variant is compiled to a list of SQL statements
//! that D1 runs as **one** SQL transaction. Control flow lives in `WHERE`
//! clauses (and a status `SELECT`) so the guest never needs mid-transaction
//! round-trips. Consume-once ops use `DELETE … RETURNING` so concurrent
//! callers cannot both observe the unused row. A statement failure aborts
//! the HTTP batch and rolls back.

use std::time::Duration;

use bookclerk_plugin_sdk::{
    atomic_status, DbAtomicParams, DbAtomicRequest, DbAtomicResult, DbAtomicTiming,
};
use sea_orm::DbErr;
use serde_json::{json, Value as JsonValue};

use super::d1::D1Proxy;

/// One statement in a D1 HTTP batch body.
pub(crate) type SqlStmt = (String, Vec<JsonValue>);

/// Planned batch plus the index of the application-status `SELECT`.
struct AtomicPlan {
    /// Ordered D1 HTTP batch statements that run as one SQL transaction.
    statements: Vec<SqlStmt>,
    /// Index of the application-status `SELECT` inside [`AtomicPlan::statements`].
    outcome_index: usize,
    /// Index of the payload `SELECT` when the op returns a user or identity row.
    payload_index: Option<usize>,
    /// `DELETE … RETURNING` consume-once; when set, expiry uses this cutoff.
    consume_once: Option<(ConsumeOnceKind, String)>,
    /// When set, interpret from this `SELECT` of `db_atomic_receipts`.
    receipt_select_index: Option<usize>,
    /// Receipt `SELECT` immediately after prune; a row means this attempt is a replay.
    prior_receipt_index: Option<usize>,
    /// SHA-256 hex of the idempotency-relevant request; compared on receipt replay.
    expected_hash: Option<String>,
}

/// Bindings shared by every receipt prune/select/insert in one atomic attempt.
struct ReceiptCtx {
    /// Caller-owned idempotency key reused across HTTP retries of the same attempt.
    operation_id: String,
    /// SHA-256 hex of the operation payload; a mismatch is an idempotency conflict.
    request_hash: String,
    /// Wire operation name stored on the receipt (`deleteUser`, `redeemClaimTicket`, …).
    kind: &'static str,
    /// RFC 3339 timestamp shared by every statement in this batch.
    now: String,
    /// RFC 3339 cutoff after which this receipt may be pruned (24 hours from `now`).
    expires_at: String,
}

#[derive(Debug, Clone, Copy)]
/// Which consume-once table a `DELETE … RETURNING` plan targets.
enum ConsumeOnceKind {
    /// OIDC RP state row consumed from `oidc_rp_states`.
    OidcRpState,
    /// WebAuthn challenge row consumed from `webauthn_challenges`.
    WebauthnChallenge,
}

/// Maximum D1 HTTP batch attempts, including retries after ambiguous responses.
const ATOMIC_HTTP_ATTEMPTS: usize = 3;

impl D1Proxy {
    /// Runs a named library operation as one D1 HTTP batch (one SQL transaction).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn run_atomic(
        &self,
        req: DbAtomicRequest,
    ) -> std::result::Result<DbAtomicResult, DbErr> {
        let started = std::time::Instant::now();
        let now = chrono::Utc::now().to_rfc3339();
        let plan = plan_atomic(&req, &now)?;
        let mut last_err = None;
        for attempt in 0..ATOMIC_HTTP_ATTEMPTS {
            let raw = match self.run_batch(&plan.statements).await {
                Ok(value) => value,
                Err(err) if err.is_retryable() && attempt + 1 < ATOMIC_HTTP_ATTEMPTS => {
                    sleep_before_d1_retry(attempt, err.retry_after()).await;
                    last_err = Some(DbErr::from(err));
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            match parse_and_validate_batch(&plan, &raw, &req.operation_id) {
                Ok(results) => {
                    let mut result = interpret_atomic(&plan, &results);
                    let db_execution_us = d1_sql_duration_us(&raw);
                    result.operation_id = req.operation_id;
                    result.timing = Some(DbAtomicTiming {
                        attempt_elapsed_us: u64::try_from(started.elapsed().as_micros())
                            .unwrap_or(u64::MAX),
                        db_execution_us,
                        db_timing_source: db_execution_us.map(|_| "d1_sql_duration".into()),
                    });
                    return Ok(result);
                }
                Err(err) if is_ambiguous_d1(&err) && attempt + 1 < ATOMIC_HTTP_ATTEMPTS => {
                    sleep_before_d1_retry(attempt, None).await;
                    last_err = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_err.unwrap_or_else(|| ambiguous_d1("exhausted retries")))
    }
}

/// Waits before a D1 retry, honoring `Retry-After` or a capped exponential backoff.
async fn sleep_before_d1_retry(attempt: usize, retry_after: Option<Duration>) {
    let delay = retry_after.unwrap_or_else(|| {
        Duration::from_millis((50u64.saturating_mul(3u64.saturating_pow(attempt as u32))).min(400))
    });
    tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
}

/// True when a D1 HTTP/parse failure may have already committed the batch.
///
/// Permanent 4xx responses are encoded as `D1 HTTP {status}: …` and are not
/// retryable. Transport, incomplete 2xx, JSON parse, and 408/429/5xx use the
/// `D1 ambiguous response` prefix.
pub fn is_ambiguous_d1(err: &DbErr) -> bool {
    err.to_string().contains("D1 ambiguous")
}

/// Maps a D1 [`DbErr`] onto the guest ABI: retryable/ambiguous → `unavailable`,
/// client 4xx → `invalid_params`, other failures → `internal`.
#[must_use]
pub fn plugin_error_from_d1(err: DbErr) -> bookclerk_plugin_sdk::PluginError {
    if is_ambiguous_d1(&err) {
        return bookclerk_plugin_sdk::PluginError::unavailable(err.to_string());
    }
    if let Some(status) = permanent_http_status(&err) {
        if (400..500).contains(&status) {
            return bookclerk_plugin_sdk::PluginError::invalid_params(err.to_string());
        }
    }
    bookclerk_plugin_sdk::PluginError::internal(err.to_string())
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

/// SHA-256 hex of the idempotency-relevant fields of `op`, mapped to [`DbErr`].
fn request_hash(op: &DbAtomicParams) -> std::result::Result<String, DbErr> {
    bookclerk_library::db_atomic_request_hash(op).map_err(|err| DbErr::Custom(err.to_string()))
}

/// Wire `operationKind` string stored on `db_atomic_receipts` for `op`.
fn operation_kind(op: &DbAtomicParams) -> &'static str {
    match op {
        DbAtomicParams::DeleteUser { .. } => "deleteUser",
        DbAtomicParams::SetUserStatus { .. } => "setUserStatus",
        DbAtomicParams::SetUserPasswordHash { .. } => "setUserPasswordHash",
        DbAtomicParams::SetUserRole { .. } => "setUserRole",
        DbAtomicParams::RedeemClaimTicket { .. } => "redeemClaimTicket",
        DbAtomicParams::TakeOidcRpState { .. } => "takeOidcRpState",
        DbAtomicParams::TakeWebauthnChallenge { .. } => "takeWebauthnChallenge",
    }
}

/// RFC 3339 timestamp 24 hours after `now`, or `now` when the input is unparseable.
fn receipt_expiry(now: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(now)
        .map(|dt| (dt + chrono::Duration::hours(24)).to_rfc3339())
        .unwrap_or_else(|_| now.to_string())
}

/// Builds the D1 batch for `op`. `now` is the RFC 3339 timestamp shared by
/// every statement in the batch (consume correlation, `updated_at`, sessions).
fn plan_atomic(req: &DbAtomicRequest, now: &str) -> std::result::Result<AtomicPlan, DbErr> {
    let inner = plan_inner(&req.operation, now);
    let ctx = ReceiptCtx {
        operation_id: req.operation_id.clone(),
        request_hash: request_hash(&req.operation)?,
        kind: operation_kind(&req.operation),
        now: now.to_string(),
        expires_at: receipt_expiry(now),
    };
    Ok(match &req.operation {
        DbAtomicParams::TakeOidcRpState { state_hash } => wrap_consume_oidc(state_hash, now, &ctx),
        DbAtomicParams::TakeWebauthnChallenge { challenge_id, kind } => {
            wrap_consume_webauthn(challenge_id, kind, now, &ctx)
        }
        DbAtomicParams::DeleteUser { .. } => wrap_status_op(inner, &ctx, PayloadKind::None),
        DbAtomicParams::SetUserStatus { user_id, .. }
        | DbAtomicParams::SetUserPasswordHash { user_id, .. }
        | DbAtomicParams::SetUserRole { user_id, .. } => {
            wrap_status_op(inner, &ctx, PayloadKind::User { user_id: *user_id })
        }
        DbAtomicParams::RedeemClaimTicket { .. } => {
            wrap_status_op(inner, &ctx, PayloadKind::Identity)
        }
    })
}

/// Compiles `op` into the inner SQL statements before receipt wrapping.
fn plan_inner(op: &DbAtomicParams, now: &str) -> AtomicPlan {
    match op {
        DbAtomicParams::DeleteUser { user_id } => plan_delete_user(*user_id),
        DbAtomicParams::SetUserStatus { user_id, status } => {
            plan_set_user_status(*user_id, status, now)
        }
        DbAtomicParams::SetUserPasswordHash {
            user_id,
            password_hash,
        } => plan_set_user_password_hash(*user_id, password_hash.as_deref(), now),
        DbAtomicParams::SetUserRole { user_id, role } => plan_set_user_role(*user_id, role, now),
        DbAtomicParams::RedeemClaimTicket {
            token_hash,
            session_hash,
            expires_at,
            user_agent,
            device_type,
            client_label,
            new_password_hash,
            password_fingerprint: _,
        } => plan_redeem_claim(
            token_hash,
            session_hash,
            expires_at,
            user_agent.as_deref(),
            device_type.as_deref(),
            client_label.as_deref(),
            new_password_hash.as_deref(),
            now,
        ),
        DbAtomicParams::TakeOidcRpState { state_hash } => plan_take_oidc_rp_state(state_hash, now),
        DbAtomicParams::TakeWebauthnChallenge { challenge_id, kind } => {
            plan_take_webauthn_challenge(challenge_id, kind, now)
        }
    }
}

/// Pairs a SQL text with JSON bind parameters for a D1 batch statement.
fn sql(text: &str, params: Vec<JsonValue>) -> SqlStmt {
    (text.to_string(), params)
}

/// JSON number bind for a signed 64-bit integer column.
fn j_i64(n: i64) -> JsonValue {
    JsonValue::from(n)
}

/// JSON string bind; the value is copied into the batch body.
fn j_str(s: &str) -> JsonValue {
    JsonValue::String(s.to_string())
}

/// JSON string bind, or JSON null when the optional value is absent.
fn j_opt_str(s: Option<&str>) -> JsonValue {
    match s {
        Some(v) => JsonValue::String(v.to_string()),
        None => JsonValue::Null,
    }
}

/// Which receipt payload `SELECT` to attach after a status-gated write.
enum PayloadKind {
    /// No payload row; the receipt stores status only.
    None,
    /// Scoped to a concrete library user id.
    User {
        /// Library user id for this atomic scope.
        user_id: i64,
    },
    /// Portal-identity JSON payload after a successful claim redeem.
    Identity,
}

/// Deletes expired receipts except the current `operation_id` so a replay can still match.
fn prune_receipts(ctx: &ReceiptCtx) -> SqlStmt {
    sql(
        "DELETE FROM db_atomic_receipts WHERE expires_at <= ? AND operation_id != ?",
        vec![j_str(&ctx.now), j_str(&ctx.operation_id)],
    )
}

/// Selects the receipt row for this `operation_id`, if one already exists.
fn select_receipt(ctx: &ReceiptCtx) -> SqlStmt {
    sql(
        "SELECT operation_id, request_hash, status, payload, created_at \
         FROM db_atomic_receipts WHERE operation_id = ?",
        vec![j_str(&ctx.operation_id)],
    )
}

/// Appends a `NOT EXISTS` receipt gate so writes skip when this attempt already ran.
fn gate_write(sql_text: String, mut params: Vec<JsonValue>, operation_id: &str) -> SqlStmt {
    let trimmed = sql_text.trim_start();
    let is_write = trimmed.starts_with("INSERT")
        || trimmed.starts_with("UPDATE")
        || trimmed.starts_with("DELETE");
    if !is_write {
        return (sql_text, params);
    }
    params.push(j_str(operation_id));
    (
        format!(
            "{sql_text} AND NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)"
        ),
        params,
    )
}

/// Restricts a later write to run only after this attempt's receipt claimed `ok`.
fn gate_claimed_ok(sql_text: String, mut params: Vec<JsonValue>, operation_id: &str) -> SqlStmt {
    let trimmed = sql_text.trim_start();
    let is_write = trimmed.starts_with("INSERT")
        || trimmed.starts_with("UPDATE")
        || trimmed.starts_with("DELETE");
    if !is_write {
        return (sql_text, params);
    }
    params.push(j_str(operation_id));
    (
        format!(
            "{sql_text} AND EXISTS (\
                SELECT 1 FROM db_atomic_receipts \
                 WHERE operation_id = ? AND status = '{ok}'\
            )",
            ok = atomic_status::OK,
        ),
        params,
    )
}

/// SQL that builds the guest user JSON object (password present as a boolean).
fn user_payload_json_sql() -> &'static str {
    "SELECT json_object(\
        'id', id, 'role', role, 'status', status, \
        'display_name', display_name, 'login_name', login_name, 'email', email, \
        'has_password', json(CASE WHEN password_hash IS NOT NULL AND password_hash != '' THEN 'true' ELSE 'false' END), \
        'security_version', security_version, 'created_at', created_at, 'updated_at', updated_at\
     ) AS payload FROM users WHERE id = ?"
}

/// Opening SQL for wrapping a portal-identity subquery as receipt payload JSON.
fn identity_payload_json_sql() -> &'static str {
    "SELECT json_object(\
        'id', id, 'provider', provider, 'external_user_id', external_user_id, \
        'label', label, 'user_id', user_id, 'created_at', created_at\
     ) AS payload FROM ("
}

/// Wraps a status-gated plan with prune, prior-receipt select, gated writes, and a final receipt select.
fn wrap_status_op(plan: AtomicPlan, ctx: &ReceiptCtx, payload: PayloadKind) -> AtomicPlan {
    let outcome_index = plan.outcome_index;
    let payload_index = plan.payload_index;
    let outcome = plan.statements[outcome_index].clone();
    let payload_stmt = payload_index.and_then(|idx| plan.statements.get(idx).cloned());
    let mut statements = vec![prune_receipts(ctx), select_receipt(ctx)];
    let prior_receipt_index = Some(statements.len() - 1);
    for (i, (sql_text, params)) in plan.statements.into_iter().enumerate() {
        if Some(i) == payload_index {
            continue;
        }
        if i == outcome_index {
            // Claim status now so later mutations cannot change the receipt.
            statements.push(receipt_insert_from_outcome(ctx, &outcome));
            continue;
        }
        if i < outcome_index {
            statements.push(gate_write(sql_text, params, &ctx.operation_id));
        } else {
            statements.push(gate_claimed_ok(sql_text, params, &ctx.operation_id));
        }
    }
    if let Some(update) = receipt_payload_update(ctx, payload, payload_stmt) {
        statements.push(update);
    }
    statements.push(select_receipt(ctx));
    let receipt_select_index = statements.len() - 1;
    AtomicPlan {
        statements,
        outcome_index: 0,
        payload_index: None,
        consume_once: None,
        receipt_select_index: Some(receipt_select_index),
        prior_receipt_index,
        expected_hash: Some(ctx.request_hash.clone()),
    }
}

/// Inserts a receipt whose status comes from the plan's outcome `SELECT`, skipping if one exists.
fn receipt_insert_from_outcome(ctx: &ReceiptCtx, outcome: &SqlStmt) -> SqlStmt {
    let insert_sql = format!(
        "INSERT INTO db_atomic_receipts (\
            operation_id, operation_kind, request_hash, status, payload, created_at, expires_at\
         ) SELECT ?, ?, ?, o.status, NULL, ?, ? FROM ({}) o \
         WHERE NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)",
        outcome.0
    );
    let mut params = vec![
        j_str(&ctx.operation_id),
        j_str(ctx.kind),
        j_str(&ctx.request_hash),
        j_str(&ctx.now),
        j_str(&ctx.expires_at),
    ];
    params.extend(outcome.1.clone());
    params.push(j_str(&ctx.operation_id));
    (insert_sql, params)
}

/// Updates an `ok` receipt with user or identity JSON when the payload is still null.
fn receipt_payload_update(
    ctx: &ReceiptCtx,
    payload: PayloadKind,
    payload_stmt: Option<SqlStmt>,
) -> Option<SqlStmt> {
    match payload {
        PayloadKind::None => None,
        PayloadKind::User { user_id } => {
            let sql = format!(
                "UPDATE db_atomic_receipts SET payload = ({}) \
                 WHERE operation_id = ? AND status = '{ok}' AND payload IS NULL",
                user_payload_json_sql(),
                ok = atomic_status::OK,
            );
            Some((sql, vec![j_i64(user_id), j_str(&ctx.operation_id)]))
        }
        PayloadKind::Identity => {
            let (inner_sql, inner_params) = payload_stmt?;
            let sql = format!(
                "UPDATE db_atomic_receipts SET payload = ({}{inner_sql})) \
                 WHERE operation_id = ? AND status = '{ok}' AND payload IS NULL",
                identity_payload_json_sql(),
                ok = atomic_status::OK,
            );
            let mut params = inner_params;
            params.push(j_str(&ctx.operation_id));
            Some((sql, params))
        }
    }
}

/// Builds the consume-once batch that copies then deletes an OIDC RP state row.
fn wrap_consume_oidc(state_hash: &str, now: &str, ctx: &ReceiptCtx) -> AtomicPlan {
    wrap_consume(
        ctx,
        "takeOidcRpState",
        "oidc_rp_states",
        "state_hash = ?",
        vec![j_str(state_hash)],
        format!("oidc:{state_hash}"),
        "json_object(\
            'provider_id', d.provider_id, 'pkce_verifier', d.pkce_verifier, \
            'nonce', d.nonce, 'purpose', d.purpose, 'user_id', d.user_id)",
        now,
    )
}

/// Builds the consume-once batch that copies then deletes a WebAuthn challenge row.
fn wrap_consume_webauthn(
    challenge_id: &str,
    kind: &str,
    now: &str,
    ctx: &ReceiptCtx,
) -> AtomicPlan {
    wrap_consume(
        ctx,
        "takeWebauthnChallenge",
        "webauthn_challenges",
        "challenge_id = ? AND kind = ?",
        vec![j_str(challenge_id), j_str(kind)],
        format!("webauthn:{challenge_id}:{kind}"),
        "json_object('user_id', d.user_id, 'state_json', d.state_json)",
        now,
    )
}

#[allow(clippy::too_many_arguments)]
/// Copies a consume-once row into a receipt (unique `consume_key`), then deletes the source.
fn wrap_consume(
    ctx: &ReceiptCtx,
    kind: &str,
    table: &str,
    where_sql: &str,
    where_params: Vec<JsonValue>,
    consume_key: String,
    ok_json: &str,
    now: &str,
) -> AtomicPlan {
    // SQLite (and D1) reject `DELETE … RETURNING` in a subquery. Copy the row
    // into the receipt first with a unique `consume_key` so a second caller
    // cannot also observe it, then delete the source row.
    let insert_from_row = format!(
        "INSERT OR IGNORE INTO db_atomic_receipts (\
            operation_id, operation_kind, request_hash, status, payload, created_at, expires_at, consume_key\
         ) SELECT ?, ?, ?, \
            CASE WHEN d.expires_at <= ? THEN '{empty}' ELSE '{ok}' END, \
            CASE WHEN d.expires_at <= ? THEN NULL ELSE {ok_json} END, \
            ?, ?, ? \
           FROM {table} AS d \
          WHERE {where_sql} \
            AND NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)",
        empty = atomic_status::EMPTY,
        ok = atomic_status::OK,
    );
    let mut from_row_params = vec![
        j_str(&ctx.operation_id),
        j_str(kind),
        j_str(&ctx.request_hash),
        j_str(now),
        j_str(now),
        j_str(&ctx.now),
        j_str(&ctx.expires_at),
        j_str(&consume_key),
    ];
    from_row_params.extend(where_params.clone());
    from_row_params.push(j_str(&ctx.operation_id));

    let delete_sql = format!(
        "DELETE FROM {table} \
         WHERE {where_sql} \
           AND EXISTS (\
             SELECT 1 FROM db_atomic_receipts \
              WHERE operation_id = ? AND created_at = ?\
           )"
    );
    let mut delete_params = where_params;
    delete_params.push(j_str(&ctx.operation_id));
    delete_params.push(j_str(&ctx.now));

    let insert_empty = format!(
        "INSERT INTO db_atomic_receipts (\
            operation_id, operation_kind, request_hash, status, payload, created_at, expires_at, consume_key\
         ) SELECT ?, ?, ?, '{empty}', NULL, ?, ?, NULL \
          WHERE NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)",
        empty = atomic_status::EMPTY,
    );
    let empty_params = vec![
        j_str(&ctx.operation_id),
        j_str(kind),
        j_str(&ctx.request_hash),
        j_str(&ctx.now),
        j_str(&ctx.expires_at),
        j_str(&ctx.operation_id),
    ];
    let statements = vec![
        prune_receipts(ctx),
        select_receipt(ctx),
        (insert_from_row, from_row_params),
        (delete_sql, delete_params),
        (insert_empty, empty_params),
        select_receipt(ctx),
    ];
    AtomicPlan {
        statements,
        outcome_index: 0,
        payload_index: None,
        consume_once: None,
        receipt_select_index: Some(5),
        prior_receipt_index: Some(1),
        expected_hash: Some(ctx.request_hash.clone()),
    }
}

/// Last active owner predicate. Binds `user_id` twice.
fn last_owner_sql() -> &'static str {
    "((SELECT role FROM users WHERE id = ?) = 'owner' \
      AND (SELECT status FROM users WHERE id = ?) = 'active' \
      AND (SELECT COUNT(*) FROM users WHERE role = 'owner' AND status = 'active') <= 1)"
}

/// Bind parameters for [`last_owner_sql`], which references `user_id` twice.
fn last_owner_params(user_id: i64) -> Vec<JsonValue> {
    vec![j_i64(user_id), j_i64(user_id)]
}

/// `WHERE` fragment that allows mutation only when the user exists and is not the last active owner.
fn allow_mutate_sql() -> String {
    format!(
        "EXISTS (SELECT 1 FROM users WHERE id = ?) AND NOT {}",
        last_owner_sql()
    )
}

/// Bind parameters for [`allow_mutate_sql`]: user id plus the last-owner pair.
fn allow_mutate_params(user_id: i64) -> Vec<JsonValue> {
    let mut p = vec![j_i64(user_id)];
    p.extend(last_owner_params(user_id));
    p
}

/// Plans cascading deletes for a user, refusing when they are the last active owner.
fn plan_delete_user(user_id: i64) -> AtomicPlan {
    let mut statements = vec![
        sql(
            "UPDATE users SET updated_at = updated_at \
             WHERE role = 'owner' AND status = 'active'",
            vec![],
        ),
        sql(
            &format!(
                "SELECT CASE \
                   WHEN NOT EXISTS (SELECT 1 FROM users WHERE id = ?) THEN '{not_found}' \
                   WHEN {last_owner} THEN '{last_owner_st}' \
                   ELSE '{ok}' \
                 END AS status",
                not_found = atomic_status::NOT_FOUND,
                last_owner = last_owner_sql(),
                last_owner_st = atomic_status::LAST_OWNER,
                ok = atomic_status::OK,
            ),
            {
                let mut p = vec![j_i64(user_id)];
                p.extend(last_owner_params(user_id));
                p
            },
        ),
    ];
    let outcome_index = 1;
    let allow = allow_mutate_sql();
    let allow_p = allow_mutate_params(user_id);

    let identity_deletes = [
        "DELETE FROM account_links WHERE identity_id IN \
         (SELECT id FROM portal_identities WHERE user_id = ?) AND ",
        "DELETE FROM title_requests WHERE identity_id IN \
         (SELECT id FROM portal_identities WHERE user_id = ?) AND ",
        "DELETE FROM claim_tickets WHERE identity_id IN \
         (SELECT id FROM portal_identities WHERE user_id = ?) AND ",
        "DELETE FROM portal_sessions WHERE identity_id IN \
         (SELECT id FROM portal_identities WHERE user_id = ?) AND ",
        "DELETE FROM listening_progress WHERE identity_id IN \
         (SELECT id FROM portal_identities WHERE user_id = ?) AND ",
        "DELETE FROM user_preferences WHERE identity_id IN \
         (SELECT id FROM portal_identities WHERE user_id = ?) AND ",
    ];
    for prefix in identity_deletes {
        let mut params = vec![j_i64(user_id)];
        params.extend(allow_p.clone());
        statements.push(sql(&format!("{prefix}{allow}"), params));
    }

    let mut elevated_p = vec![j_i64(user_id)];
    elevated_p.extend(allow_p.clone());
    statements.push(sql(
        &format!("DELETE FROM operator_sessions WHERE elevated_from_user_id = ? AND {allow}"),
        elevated_p,
    ));

    let mut impersonate_p = vec![j_i64(user_id)];
    impersonate_p.extend(allow_p.clone());
    statements.push(sql(
        &format!(
            "UPDATE operator_sessions SET impersonating_user_id = NULL \
             WHERE impersonating_user_id = ? AND {allow}"
        ),
        impersonate_p,
    ));

    for table in [
        "oidc_refresh_tokens",
        "oidc_auth_codes",
        "webauthn_credentials",
        "webauthn_challenges",
        "oidc_rp_states",
    ] {
        let mut p = vec![j_i64(user_id)];
        p.extend(allow_p.clone());
        statements.push(sql(
            &format!("DELETE FROM {table} WHERE user_id = ? AND {allow}"),
            p,
        ));
    }

    let mut ident_p = vec![j_i64(user_id)];
    ident_p.extend(allow_p.clone());
    statements.push(sql(
        &format!("DELETE FROM portal_identities WHERE user_id = ? AND {allow}"),
        ident_p,
    ));

    let subject = format!("user:{user_id}");
    let mut prefs_p = vec![j_str(&subject)];
    prefs_p.extend(allow_p.clone());
    statements.push(sql(
        &format!("DELETE FROM user_preferences WHERE subject_key = ? AND {allow}"),
        prefs_p,
    ));

    let mut user_p = vec![j_i64(user_id)];
    user_p.extend(last_owner_params(user_id));
    statements.push(sql(
        &format!(
            "DELETE FROM users WHERE id = ? AND NOT {}",
            last_owner_sql()
        ),
        user_p,
    ));

    AtomicPlan {
        statements,
        outcome_index,
        payload_index: None,
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Plans a status update that refuses disabling the last active owner and drops elevated sessions.
fn plan_set_user_status(user_id: i64, status: &str, now: &str) -> AtomicPlan {
    let last_owner_disable = format!("(? = 'disabled' AND {})", last_owner_sql());
    let mut outcome_params = vec![j_i64(user_id), j_str(status)];
    outcome_params.extend(last_owner_params(user_id));
    let statements = vec![
        sql(
            "UPDATE users SET updated_at = updated_at \
             WHERE role = 'owner' AND status = 'active'",
            vec![],
        ),
        sql(
            &format!(
                "SELECT CASE \
                   WHEN NOT EXISTS (SELECT 1 FROM users WHERE id = ?) THEN '{not_found}' \
                   WHEN {last_owner_disable} THEN '{last_owner}' \
                   ELSE '{ok}' \
                 END AS status",
                not_found = atomic_status::NOT_FOUND,
                last_owner = atomic_status::LAST_OWNER,
                ok = atomic_status::OK,
            ),
            outcome_params,
        ),
        {
            let mut p = vec![j_str(status), j_str(now), j_i64(user_id), j_str(status)];
            p.extend(last_owner_params(user_id));
            sql(
                &format!(
                    "UPDATE users SET status = ?, updated_at = ? \
                     WHERE id = ? AND NOT {last_owner_disable}"
                ),
                p,
            )
        },
        sql(
            "DELETE FROM operator_sessions \
             WHERE elevated_from_user_id = ? AND ? = 'disabled' \
               AND EXISTS (SELECT 1 FROM users WHERE id = ? AND status = 'disabled')",
            vec![j_i64(user_id), j_str(status), j_i64(user_id)],
        ),
        sql(
            "SELECT id, role, status, display_name, login_name, email, password_hash, \
                    security_version, created_at, updated_at \
             FROM users WHERE id = ?",
            vec![j_i64(user_id)],
        ),
    ];
    AtomicPlan {
        statements,
        outcome_index: 1,
        payload_index: Some(4),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Plans a password-hash write that bumps `security_version` and clears elevated sessions.
fn plan_set_user_password_hash(user_id: i64, password_hash: Option<&str>, now: &str) -> AtomicPlan {
    AtomicPlan {
        statements: vec![
            sql(
                &format!(
                    "SELECT CASE \
                       WHEN NOT EXISTS (SELECT 1 FROM users WHERE id = ?) THEN '{not_found}' \
                       ELSE '{ok}' \
                     END AS status",
                    not_found = atomic_status::NOT_FOUND,
                    ok = atomic_status::OK,
                ),
                vec![j_i64(user_id)],
            ),
            sql(
                "UPDATE users SET password_hash = ?, security_version = security_version + 1, \
                        updated_at = ? \
                 WHERE id = ?",
                vec![j_opt_str(password_hash), j_str(now), j_i64(user_id)],
            ),
            sql(
                "DELETE FROM operator_sessions WHERE elevated_from_user_id = ?",
                vec![j_i64(user_id)],
            ),
            sql(
                "SELECT id, role, status, display_name, login_name, email, password_hash, \
                        security_version, created_at, updated_at \
                 FROM users WHERE id = ?",
                vec![j_i64(user_id)],
            ),
        ],
        outcome_index: 0,
        payload_index: Some(3),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Plans a role change that refuses demoting the last active owner and drops elevated sessions.
fn plan_set_user_role(user_id: i64, role: &str, now: &str) -> AtomicPlan {
    let last_owner_demote = format!("(? != 'owner' AND {})", last_owner_sql());
    let mut outcome_params = vec![j_i64(user_id), j_str(role)];
    outcome_params.extend(last_owner_params(user_id));
    AtomicPlan {
        statements: vec![
            sql(
                "UPDATE users SET updated_at = updated_at \
                 WHERE role = 'owner' AND status = 'active'",
                vec![],
            ),
            sql(
                &format!(
                    "SELECT CASE \
                       WHEN NOT EXISTS (SELECT 1 FROM users WHERE id = ?) THEN '{not_found}' \
                       WHEN {last_owner_demote} THEN '{last_owner}' \
                       ELSE '{ok}' \
                     END AS status",
                    not_found = atomic_status::NOT_FOUND,
                    last_owner = atomic_status::LAST_OWNER,
                    ok = atomic_status::OK,
                ),
                outcome_params,
            ),
            {
                let mut p = vec![j_i64(user_id), j_i64(user_id), j_str(role), j_str(role)];
                p.extend(last_owner_params(user_id));
                sql(
                    &format!(
                        "DELETE FROM operator_sessions \
                         WHERE elevated_from_user_id = ? \
                           AND EXISTS (SELECT 1 FROM users WHERE id = ? AND role != ?) \
                           AND NOT {last_owner_demote}"
                    ),
                    p,
                )
            },
            {
                let mut p = vec![j_str(role), j_str(now), j_i64(user_id), j_str(role)];
                p.extend(last_owner_params(user_id));
                sql(
                    &format!(
                        "UPDATE users SET role = ?, updated_at = ? \
                         WHERE id = ? AND NOT {last_owner_demote}"
                    ),
                    p,
                )
            },
            sql(
                "SELECT id, role, status, display_name, login_name, email, password_hash, \
                        security_version, created_at, updated_at \
                 FROM users WHERE id = ?",
                vec![j_i64(user_id)],
            ),
        ],
        outcome_index: 1,
        payload_index: Some(4),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

#[allow(clippy::too_many_arguments)]
/// Marks a claim ticket redeemed, optionally sets a local password, and inserts the portal session.
fn plan_redeem_claim(
    token_hash: &str,
    session_hash: &str,
    expires_at: &str,
    user_agent: Option<&str>,
    device_type: Option<&str>,
    client_label: Option<&str>,
    new_password_hash: Option<&str>,
    now: &str,
) -> AtomicPlan {
    let password_provided = i64::from(new_password_hash.is_some());
    let token = j_str(token_hash);
    let now_v = j_str(now);
    AtomicPlan {
        statements: vec![
            sql(
                "UPDATE claim_tickets SET redeemed_at = ? \
                 WHERE token_hash = ? AND redeemed_at IS NULL AND expires_at > ? \
                   AND identity_id IS NOT NULL \
                   AND EXISTS (SELECT 1 FROM portal_identities WHERE id = claim_tickets.identity_id) \
                   AND NOT EXISTS ( \
                         SELECT 1 FROM portal_identities i \
                         WHERE i.id = claim_tickets.identity_id \
                           AND i.provider = 'local' \
                           AND i.user_id IS NOT NULL \
                           AND NOT EXISTS (SELECT 1 FROM users WHERE id = i.user_id) \
                       ) \
                   AND NOT EXISTS ( \
                         SELECT 1 FROM portal_identities i \
                         JOIN users u ON u.id = i.user_id \
                         WHERE i.id = claim_tickets.identity_id \
                           AND i.provider = 'local' \
                           AND u.password_hash IS NULL \
                           AND ? = 0 \
                       )",
                vec![
                    now_v.clone(),
                    token.clone(),
                    now_v.clone(),
                    JsonValue::from(password_provided),
                ],
            ),
            sql(
                "DELETE FROM operator_sessions \
                 WHERE elevated_from_user_id IN ( \
                         SELECT i.user_id FROM claim_tickets t \
                         JOIN portal_identities i ON i.id = t.identity_id \
                         JOIN users u ON u.id = i.user_id \
                         WHERE t.token_hash = ? AND t.redeemed_at = ? \
                           AND i.provider = 'local' \
                           AND u.password_hash IS NULL \
                           AND ? = 1 \
                       )",
                vec![
                    token.clone(),
                    now_v.clone(),
                    JsonValue::from(password_provided),
                ],
            ),
            sql(
                "UPDATE users SET password_hash = ?, \
                        security_version = security_version + 1, \
                        updated_at = ? \
                 WHERE ? = 1 AND password_hash IS NULL AND id IN ( \
                         SELECT i.user_id FROM claim_tickets t \
                         JOIN portal_identities i ON i.id = t.identity_id \
                         WHERE t.token_hash = ? AND t.redeemed_at = ? \
                           AND i.provider = 'local' AND i.user_id IS NOT NULL \
                       )",
                vec![
                    j_opt_str(new_password_hash),
                    now_v.clone(),
                    JsonValue::from(password_provided),
                    token.clone(),
                    now_v.clone(),
                ],
            ),
            sql(
                "INSERT INTO portal_sessions ( \
                    token_hash, identity_id, expires_at, created_at, last_used_at, \
                    user_agent, device_type, client_label \
                 ) \
                 SELECT ?, t.identity_id, ?, ?, ?, ?, ?, ? \
                   FROM claim_tickets t \
                  WHERE t.token_hash = ? AND t.redeemed_at = ? AND t.identity_id IS NOT NULL",
                vec![
                    j_str(session_hash),
                    j_str(expires_at),
                    now_v.clone(),
                    now_v.clone(),
                    j_opt_str(user_agent),
                    j_opt_str(device_type),
                    j_opt_str(client_label),
                    token.clone(),
                    now_v.clone(),
                ],
            ),
            sql(
                &format!(
                    "SELECT CASE \
                       WHEN EXISTS ( \
                              SELECT 1 FROM claim_tickets t \
                              JOIN portal_sessions s ON s.token_hash = ? \
                             WHERE t.token_hash = ? AND t.redeemed_at = ? \
                           ) THEN '{ok}' \
                       WHEN EXISTS ( \
                              SELECT 1 FROM claim_tickets t \
                              JOIN portal_identities i ON i.id = t.identity_id \
                              JOIN users u ON u.id = i.user_id \
                             WHERE t.token_hash = ? AND t.redeemed_at IS NULL \
                               AND t.expires_at > ? \
                               AND i.provider = 'local' \
                               AND u.password_hash IS NULL \
                               AND ? = 0 \
                           ) THEN '{password_required}' \
                       ELSE '{claim_invalid}' \
                     END AS status",
                    ok = atomic_status::OK,
                    password_required = atomic_status::PASSWORD_REQUIRED,
                    claim_invalid = atomic_status::CLAIM_INVALID,
                ),
                vec![
                    j_str(session_hash),
                    token.clone(),
                    now_v.clone(),
                    token.clone(),
                    now_v.clone(),
                    JsonValue::from(password_provided),
                ],
            ),
            sql(
                "SELECT i.id, i.provider, i.external_user_id, i.label, i.user_id, i.created_at \
                   FROM portal_identities i \
                   JOIN claim_tickets t ON t.identity_id = i.id \
                  WHERE t.token_hash = ? AND t.redeemed_at = ?",
                vec![token, now_v],
            ),
        ],
        outcome_index: 4,
        payload_index: Some(5),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Inner `DELETE … RETURNING` for an OIDC RP state; wrapping adds the receipt copy.
fn plan_take_oidc_rp_state(state_hash: &str, now: &str) -> AtomicPlan {
    AtomicPlan {
        statements: vec![sql(
            "DELETE FROM oidc_rp_states \
             WHERE state_hash = ? \
             RETURNING provider_id, pkce_verifier, nonce, purpose, user_id, expires_at",
            vec![j_str(state_hash)],
        )],
        outcome_index: 0,
        payload_index: Some(0),
        consume_once: Some((ConsumeOnceKind::OidcRpState, now.to_string())),
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Inner `DELETE … RETURNING` for a WebAuthn challenge; wrapping adds the receipt copy.
fn plan_take_webauthn_challenge(challenge_id: &str, kind: &str, now: &str) -> AtomicPlan {
    AtomicPlan {
        statements: vec![sql(
            "DELETE FROM webauthn_challenges \
             WHERE challenge_id = ? AND kind = ? \
             RETURNING user_id, state_json, expires_at",
            vec![j_str(challenge_id), j_str(kind)],
        )],
        outcome_index: 0,
        payload_index: Some(0),
        consume_once: Some((ConsumeOnceKind::WebauthnChallenge, now.to_string())),
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

#[derive(Debug, Clone)]
/// Rows returned by one statement in a D1 HTTP batch response.
struct BatchStmtResult {
    /// Result rows for this statement; empty when the statement did not return rows.
    rows: Vec<JsonValue>,
}

/// Parses a D1 batch response and checks receipt rows match `operation_id` and statement count.
fn parse_and_validate_batch(
    plan: &AtomicPlan,
    value: &JsonValue,
    operation_id: &str,
) -> std::result::Result<Vec<BatchStmtResult>, DbErr> {
    let results = parse_batch_results(value)?;
    if results.len() != plan.statements.len() {
        return Err(ambiguous_d1(format!(
            "expected {} statement results, got {}",
            plan.statements.len(),
            results.len()
        )));
    }
    if let Some(idx) = plan.prior_receipt_index {
        if let Some(row) = results.get(idx).and_then(|r| r.rows.first()) {
            validate_receipt_row(row, operation_id)?;
        }
    }
    if let Some(idx) = plan.receipt_select_index {
        let Some(row) = results.get(idx).and_then(|r| r.rows.first()) else {
            return Err(ambiguous_d1("missing final receipt row"));
        };
        validate_receipt_row(row, operation_id)?;
    }
    Ok(results)
}

/// Reads a required non-empty string field from a receipt row, or marks the response ambiguous.
fn required_receipt_string<'a>(
    row: &'a JsonValue,
    field: &str,
) -> std::result::Result<&'a str, DbErr> {
    row.get(field)
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ambiguous_d1(format!("malformed receipt row: missing {field}")))
}

/// Requires `operation_id`, `request_hash`, `status`, and `created_at`, and checks the id matches.
fn validate_receipt_row(
    row: &JsonValue,
    expected_operation_id: &str,
) -> std::result::Result<(), DbErr> {
    let op_id = required_receipt_string(row, "operation_id")?;
    let _hash = required_receipt_string(row, "request_hash")?;
    let _status = required_receipt_string(row, "status")?;
    let _created = required_receipt_string(row, "created_at")?;
    if op_id != expected_operation_id {
        return Err(ambiguous_d1(format!(
            "receipt operation_id {op_id} != {expected_operation_id}"
        )));
    }
    Ok(())
}

/// Parses the D1 `result` array; a `success: false` entry is a hard statement failure.
fn parse_batch_results(value: &JsonValue) -> std::result::Result<Vec<BatchStmtResult>, DbErr> {
    let Some(arr) = value.get("result").and_then(JsonValue::as_array) else {
        return Err(ambiguous_d1("batch response missing result array"));
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        if entry.get("success").and_then(JsonValue::as_bool) == Some(false) {
            return Err(DbErr::Custom(format!(
                "D1 batch statement {i} failed: {entry}"
            )));
        }
        let rows = entry
            .get("results")
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_default();
        out.push(BatchStmtResult { rows });
    }
    Ok(out)
}

/// Maps batch rows onto a [`DbAtomicResult`], preferring a prior or final receipt when present.
fn interpret_atomic(plan: &AtomicPlan, results: &[BatchStmtResult]) -> DbAtomicResult {
    if let Some(idx) = plan.prior_receipt_index {
        if let Some(row) = results.get(idx).and_then(|r| r.rows.first()) {
            return interpret_receipt(Some(row), plan.expected_hash.as_deref().unwrap_or(""), true);
        }
    }
    if let Some(idx) = plan.receipt_select_index {
        return interpret_receipt(
            results.get(idx).and_then(|r| r.rows.first()),
            plan.expected_hash.as_deref().unwrap_or(""),
            false,
        );
    }
    if let Some((kind, now)) = &plan.consume_once {
        return interpret_consume_once(*kind, now, results.get(plan.outcome_index));
    }
    let Some(outcome) = results.get(plan.outcome_index).and_then(|r| r.rows.first()) else {
        return DbAtomicResult::with_status(atomic_status::CLAIM_INVALID);
    };
    let status = outcome
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or(atomic_status::CLAIM_INVALID);
    if status != atomic_status::OK {
        return DbAtomicResult::with_status(status);
    }
    let Some(payload_index) = plan.payload_index else {
        return DbAtomicResult::ok_unit();
    };
    let Some(row) = results.get(payload_index).and_then(|r| r.rows.first()) else {
        return DbAtomicResult::with_status(atomic_status::NOT_FOUND);
    };
    if row.get("provider").is_some() {
        DbAtomicResult::ok(identity_payload(row))
    } else {
        DbAtomicResult::ok(user_payload(row))
    }
}

/// Decodes a receipt row, flagging an idempotency conflict when `request_hash` differs.
fn interpret_receipt(
    row: Option<&JsonValue>,
    expected_hash: &str,
    replayed: bool,
) -> DbAtomicResult {
    let Some(row) = row else {
        return DbAtomicResult::with_status(atomic_status::EMPTY);
    };
    let stored_hash = row
        .get("request_hash")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if stored_hash != expected_hash {
        return DbAtomicResult::with_status(atomic_status::IDEMPOTENCY_CONFLICT);
    }
    let status = row
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or(atomic_status::EMPTY);
    let created_at = row
        .get("created_at")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let mut result = if status == atomic_status::OK {
        match decode_receipt_payload(row.get("payload")) {
            Some(payload) => DbAtomicResult::ok(payload),
            None => DbAtomicResult::ok_unit(),
        }
    } else {
        DbAtomicResult::with_status(status)
    };
    result.replayed = replayed;
    result.receipt_created_at = created_at;
    result
}

/// Parses a receipt `payload` cell, accepting JSON objects or a JSON-encoded string.
fn decode_receipt_payload(value: Option<&JsonValue>) -> Option<JsonValue> {
    match value {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::String(s)) => serde_json::from_str(s)
            .ok()
            .or_else(|| Some(JsonValue::String(s.clone()))),
        Some(other) => Some(other.clone()),
    }
}

/// Interprets a consume-once `RETURNING` row as empty when missing or already expired.
fn interpret_consume_once(
    kind: ConsumeOnceKind,
    now: &str,
    result: Option<&BatchStmtResult>,
) -> DbAtomicResult {
    let Some(row) = result.and_then(|r| r.rows.first()) else {
        return DbAtomicResult::with_status(atomic_status::EMPTY);
    };
    let expired = row
        .get("expires_at")
        .and_then(JsonValue::as_str)
        .map(|expires_at| expires_at <= now)
        .unwrap_or(true);
    if expired {
        return DbAtomicResult::with_status(atomic_status::EMPTY);
    }
    match kind {
        ConsumeOnceKind::OidcRpState => DbAtomicResult::ok(oidc_rp_state_payload(row)),
        ConsumeOnceKind::WebauthnChallenge => DbAtomicResult::ok(webauthn_challenge_payload(row)),
    }
}

/// Guest user JSON: `has_password` is derived; the hash itself is never returned.
fn user_payload(row: &JsonValue) -> JsonValue {
    let has_password = match row.get("password_hash") {
        Some(JsonValue::Null) | None => false,
        Some(JsonValue::String(s)) => !s.is_empty(),
        Some(_) => true,
    };
    json!({
        "id": row.get("id").cloned().unwrap_or(JsonValue::Null),
        "role": row.get("role").cloned().unwrap_or(JsonValue::Null),
        "status": row.get("status").cloned().unwrap_or(JsonValue::Null),
        "display_name": row.get("display_name").cloned().unwrap_or(JsonValue::Null),
        "login_name": row.get("login_name").cloned().unwrap_or(JsonValue::Null),
        "email": row.get("email").cloned().unwrap_or(JsonValue::Null),
        "has_password": has_password,
        "security_version": row.get("security_version").cloned().unwrap_or(JsonValue::from(0)),
        "created_at": row.get("created_at").cloned().unwrap_or(JsonValue::Null),
        "updated_at": row.get("updated_at").cloned().unwrap_or(JsonValue::Null),
    })
}

/// Guest portal-identity JSON after a successful claim redeem.
fn identity_payload(row: &JsonValue) -> JsonValue {
    json!({
        "id": row.get("id").cloned().unwrap_or(JsonValue::Null),
        "provider": row.get("provider").cloned().unwrap_or(JsonValue::Null),
        "external_user_id": row.get("external_user_id").cloned().unwrap_or(JsonValue::Null),
        "label": row.get("label").cloned().unwrap_or(JsonValue::Null),
        "user_id": row.get("user_id").cloned().unwrap_or(JsonValue::Null),
        "created_at": row.get("created_at").cloned().unwrap_or(JsonValue::Null),
    })
}

/// Guest OIDC RP-state JSON (provider, PKCE verifier, nonce, purpose, user).
fn oidc_rp_state_payload(row: &JsonValue) -> JsonValue {
    json!({
        "provider_id": row.get("provider_id").cloned().unwrap_or(JsonValue::Null),
        "pkce_verifier": row.get("pkce_verifier").cloned().unwrap_or(JsonValue::Null),
        "nonce": row.get("nonce").cloned().unwrap_or(JsonValue::Null),
        "purpose": row.get("purpose").cloned().unwrap_or(JsonValue::Null),
        "user_id": row.get("user_id").cloned().unwrap_or(JsonValue::Null),
    })
}

/// Guest WebAuthn challenge JSON (`user_id` plus opaque `state_json`).
fn webauthn_challenge_payload(row: &JsonValue) -> JsonValue {
    json!({
        "user_id": row.get("user_id").cloned().unwrap_or(JsonValue::Null),
        "state_json": row.get("state_json").cloned().unwrap_or(JsonValue::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::{atomic_status, DbAtomicParams, DbAtomicRequest};
    use rusqlite::Connection;

    fn migrate(conn: &Connection) {
        for sql in bookclerk_library::migrations::migration_sql() {
            conn.execute_batch(sql).unwrap();
        }
    }

    fn json_to_rusqlite(v: &JsonValue) -> rusqlite::types::Value {
        match v {
            JsonValue::Null => rusqlite::types::Value::Null,
            JsonValue::Bool(b) => rusqlite::types::Value::Integer(i64::from(*b)),
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    rusqlite::types::Value::Integer(i)
                } else if let Some(f) = n.as_f64() {
                    rusqlite::types::Value::Real(f)
                } else {
                    rusqlite::types::Value::Text(n.to_string())
                }
            }
            JsonValue::String(s) => rusqlite::types::Value::Text(s.clone()),
            other => rusqlite::types::Value::Text(other.to_string()),
        }
    }

    fn run_plan(conn: &Connection, plan: &AtomicPlan) -> DbAtomicResult {
        let txn = conn.unchecked_transaction().unwrap();
        let mut results = Vec::new();
        for (sql_text, params) in &plan.statements {
            let binds: Vec<rusqlite::types::Value> = params.iter().map(json_to_rusqlite).collect();
            let mut stmt = txn.prepare(sql_text).unwrap();
            let col_count = stmt.column_count();
            let names: Vec<String> = stmt
                .column_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            let mut rows_out = Vec::new();
            if col_count == 0 {
                stmt.execute(rusqlite::params_from_iter(binds.iter()))
                    .unwrap();
            } else {
                let mut rows = stmt
                    .query(rusqlite::params_from_iter(binds.iter()))
                    .unwrap();
                while let Some(row) = rows.next().unwrap() {
                    let mut obj = serde_json::Map::new();
                    for (i, name) in names.iter().enumerate() {
                        let val = row.get_ref(i).unwrap();
                        let json = match val {
                            rusqlite::types::ValueRef::Null => JsonValue::Null,
                            rusqlite::types::ValueRef::Integer(n) => JsonValue::from(n),
                            rusqlite::types::ValueRef::Real(n) => JsonValue::from(n),
                            rusqlite::types::ValueRef::Text(t) => {
                                JsonValue::String(String::from_utf8_lossy(t).into_owned())
                            }
                            rusqlite::types::ValueRef::Blob(_) => JsonValue::Null,
                        };
                        obj.insert(name.clone(), json);
                    }
                    rows_out.push(JsonValue::Object(obj));
                }
            }
            results.push(BatchStmtResult { rows: rows_out });
        }
        txn.commit().unwrap();
        interpret_atomic(plan, &results)
    }

    fn seed_user(conn: &Connection, role: &str, status: &str, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO users (role, status, display_name, security_version, created_at, updated_at) \
             VALUES (?, ?, ?, 0, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            rusqlite::params![role, status, name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn last_owner_delete_is_refused_and_keeps_the_row() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let owner = seed_user(&conn, "owner", "active", "Only");
        let plan = plan_delete_user(owner);
        let result = run_plan(&conn, &plan);
        assert_eq!(result.status, atomic_status::LAST_OWNER);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users WHERE id = ?", [owner], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn last_owner_disable_and_demote_are_refused() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let owner = seed_user(&conn, "owner", "active", "Only");
        let now = "2024-01-01T00:00:00Z";
        let disable = run_plan(&conn, &plan_set_user_status(owner, "disabled", now));
        assert_eq!(disable.status, atomic_status::LAST_OWNER);
        let demote = run_plan(&conn, &plan_set_user_role(owner, "member", now));
        assert_eq!(demote.status, atomic_status::LAST_OWNER);
        let status: String = conn
            .query_row(
                "SELECT status || ',' || role FROM users WHERE id = ?",
                [owner],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "active,owner");
    }

    #[test]
    fn second_owner_can_be_removed() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let a = seed_user(&conn, "owner", "active", "A");
        let b = seed_user(&conn, "owner", "active", "B");
        let result = run_plan(&conn, &plan_delete_user(b));
        assert_eq!(result.status, atomic_status::OK);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let remaining: i64 = conn
            .query_row("SELECT id FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, a);
    }

    fn seed_claim(conn: &Connection, password: Option<&str>) -> (i64, i64, String) {
        let user = seed_user(conn, "member", "active", "Invitee");
        if let Some(hash) = password {
            conn.execute(
                "UPDATE users SET password_hash = ? WHERE id = ?",
                rusqlite::params![hash, user],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO portal_identities (provider, external_user_id, label, user_id, created_at) \
             VALUES ('local', 'invitee', 'Invitee', ?, '2020-01-01T00:00:00Z')",
            [user],
        )
        .unwrap();
        let identity = conn.last_insert_rowid();
        let token = "ticket-hash";
        conn.execute(
            "INSERT INTO claim_tickets (token_hash, identity_id, expires_at, created_by, created_at) \
             VALUES (?, ?, '2099-01-01T00:00:00Z', 'test', '2020-01-01T00:00:00Z')",
            rusqlite::params![token, identity],
        )
        .unwrap();
        (user, identity, token.to_string())
    }

    #[test]
    fn redeem_winner_consumes_ticket_loser_is_a_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let (user, identity, token) = seed_claim(&conn, None);
        let now = "2024-06-01T12:00:00.000000000Z";
        let winner = plan_redeem_claim(
            &token,
            "session-a",
            "2099-01-01T00:00:00Z",
            None,
            None,
            None,
            Some("argon2-hash"),
            now,
        );
        let result = run_plan(&conn, &winner);
        assert_eq!(result.status, atomic_status::OK);
        assert_eq!(result.payload.as_ref().unwrap()["id"], identity);
        let hash: String = conn
            .query_row(
                "SELECT password_hash FROM users WHERE id = ?",
                [user],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash, "argon2-hash");

        let loser = plan_redeem_claim(
            &token,
            "session-b",
            "2099-01-01T00:00:00Z",
            None,
            None,
            None,
            Some("other-hash"),
            "2024-06-01T12:00:00.000000001Z",
        );
        let lost = run_plan(&conn, &loser);
        assert_eq!(lost.status, atomic_status::CLAIM_INVALID);
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM portal_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
        let hash2: String = conn
            .query_row(
                "SELECT password_hash FROM users WHERE id = ?",
                [user],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash2, "argon2-hash");
        let sessions_b: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM portal_sessions WHERE token_hash = 'session-b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sessions_b, 0);
    }

    #[test]
    fn redeem_without_required_password_does_not_consume() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let (user, identity, token) = seed_claim(&conn, None);
        let plan = plan_redeem_claim(
            &token,
            "session-x",
            "2099-01-01T00:00:00Z",
            None,
            None,
            None,
            None,
            "2024-06-01T12:00:00Z",
        );
        let result = run_plan(&conn, &plan);
        assert_eq!(result.status, atomic_status::PASSWORD_REQUIRED);
        let redeemed: Option<String> = conn
            .query_row(
                "SELECT redeemed_at FROM claim_tickets WHERE token_hash = ?",
                rusqlite::params![token],
                |r| r.get(0),
            )
            .unwrap();
        assert!(redeemed.is_none());
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM portal_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 0);
        let hash: Option<String> = conn
            .query_row(
                "SELECT password_hash FROM users WHERE id = ?",
                [user],
                |r| r.get(0),
            )
            .unwrap();
        assert!(hash.is_none());
        let _ = identity;
    }

    fn seed_oidc_rp_state(
        conn: &Connection,
        state_hash: &str,
        expires_at: &str,
        user_id: Option<i64>,
    ) {
        let nonce = ["n", "once", "-", "1"].concat();
        conn.execute(
            "INSERT INTO oidc_rp_states \
             (state_hash, provider_id, pkce_verifier, nonce, purpose, user_id, expires_at, created_at) \
             VALUES (?, 'corp', 'verifier', ?, 'login', ?, ?, '2020-01-01T00:00:00Z')",
            rusqlite::params![state_hash, nonce, user_id, expires_at],
        )
        .unwrap();
    }

    fn seed_webauthn_challenge(
        conn: &Connection,
        challenge_id: &str,
        kind: &str,
        expires_at: &str,
    ) {
        conn.execute(
            "INSERT INTO webauthn_challenges \
             (challenge_id, user_id, kind, state_json, expires_at, created_at) \
             VALUES (?, 9, ?, '{\"x\":1}', ?, '2020-01-01T00:00:00Z')",
            rusqlite::params![challenge_id, kind, expires_at],
        )
        .unwrap();
    }

    #[test]
    fn take_oidc_rp_state_returns_payload_and_deletes() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_oidc_rp_state(&conn, "abc", "2099-01-01T00:00:00Z", Some(42));
        let now = "2024-06-01T00:00:00Z";
        let result = run_plan(&conn, &plan_take_oidc_rp_state("abc", now));
        assert_eq!(result.status, atomic_status::OK);
        let payload = result.payload.as_ref().unwrap();
        assert_eq!(payload["provider_id"], "corp");
        assert_eq!(payload["pkce_verifier"], "verifier");
        assert_eq!(payload["nonce"], ["n", "once", "-", "1"].concat());
        assert_eq!(payload["purpose"], "login");
        assert_eq!(payload["user_id"], 42);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM oidc_rp_states", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn take_oidc_rp_state_second_take_is_empty() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_oidc_rp_state(&conn, "abc", "2099-01-01T00:00:00Z", None);
        let now = "2024-06-01T00:00:00Z";
        let first = run_plan(&conn, &plan_take_oidc_rp_state("abc", now));
        assert_eq!(first.status, atomic_status::OK);
        let second = run_plan(&conn, &plan_take_oidc_rp_state("abc", now));
        assert_eq!(second.status, atomic_status::EMPTY);
        assert!(second.payload.is_none());
    }

    #[test]
    fn take_oidc_rp_state_expired_deletes_and_returns_empty() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_oidc_rp_state(&conn, "abc", "2020-01-01T00:00:00Z", Some(1));
        let result = run_plan(
            &conn,
            &plan_take_oidc_rp_state("abc", "2024-06-01T00:00:00Z"),
        );
        assert_eq!(result.status, atomic_status::EMPTY);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM oidc_rp_states", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn take_webauthn_challenge_returns_payload_and_deletes() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_webauthn_challenge(&conn, "chal-1", "login", "2099-01-01T00:00:00Z");
        let result = run_plan(
            &conn,
            &plan_take_webauthn_challenge("chal-1", "login", "2024-06-01T00:00:00Z"),
        );
        assert_eq!(result.status, atomic_status::OK);
        let payload = result.payload.as_ref().unwrap();
        assert_eq!(payload["user_id"], 9);
        assert_eq!(payload["state_json"], "{\"x\":1}");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM webauthn_challenges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn take_webauthn_challenge_wrong_kind_does_not_delete() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_webauthn_challenge(&conn, "chal-1", "login", "2099-01-01T00:00:00Z");
        let result = run_plan(
            &conn,
            &plan_take_webauthn_challenge("chal-1", "register", "2024-06-01T00:00:00Z"),
        );
        assert_eq!(result.status, atomic_status::EMPTY);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM webauthn_challenges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn take_webauthn_challenge_expired_deletes_and_returns_empty() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_webauthn_challenge(&conn, "chal-1", "login", "2020-01-01T00:00:00Z");
        let result = run_plan(
            &conn,
            &plan_take_webauthn_challenge("chal-1", "login", "2024-06-01T00:00:00Z"),
        );
        assert_eq!(result.status, atomic_status::EMPTY);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM webauthn_challenges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    fn test_req(op: DbAtomicParams, id: &str) -> DbAtomicRequest {
        DbAtomicRequest {
            operation_id: id.to_string(),
            operation: op,
        }
    }

    #[test]
    fn delete_user_removes_webauthn_and_oidc_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let keep = seed_user(&conn, "owner", "active", "Keep");
        let doomed = seed_user(&conn, "owner", "active", "Go");
        conn.execute(
            "INSERT INTO webauthn_credentials (user_id, credential_id, passkey_json, created_at) \
             VALUES (?, 'cred-reuse', '{}', '2020-01-01T00:00:00Z')",
            [doomed],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO webauthn_challenges (challenge_id, user_id, kind, state_json, expires_at, created_at) \
             VALUES ('chal-del', ?, 'login', '{}', '2099-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            [doomed],
        )
        .unwrap();
        let nonce = ["n", "once"].concat();
        conn.execute(
            "INSERT INTO oidc_rp_states \
             (state_hash, provider_id, pkce_verifier, nonce, purpose, user_id, expires_at, created_at) \
             VALUES ('state-del', 'corp', 'v', ?, 'elevate', ?, '2099-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            rusqlite::params![nonce, doomed],
        )
        .unwrap();
        let result = run_plan(&conn, &plan_delete_user(doomed));
        assert_eq!(result.status, atomic_status::OK);
        let creds: i64 = conn
            .query_row("SELECT COUNT(*) FROM webauthn_credentials", [], |r| {
                r.get(0)
            })
            .unwrap();
        let chals: i64 = conn
            .query_row("SELECT COUNT(*) FROM webauthn_challenges", [], |r| r.get(0))
            .unwrap();
        let states: i64 = conn
            .query_row("SELECT COUNT(*) FROM oidc_rp_states", [], |r| r.get(0))
            .unwrap();
        assert_eq!((creds, chals, states), (0, 0, 0));
        let _ = keep;
    }

    #[test]
    fn delete_user_plan_atomic_ok_replay_then_not_found() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let _keep = seed_user(&conn, "owner", "active", "Keep");
        let doomed = seed_user(&conn, "owner", "active", "Go");
        let now = "2024-06-01T00:00:00Z";
        let req = test_req(DbAtomicParams::DeleteUser { user_id: doomed }, "del-1");
        let plan = plan_atomic(&req, now).unwrap();
        let first = run_plan(&conn, &plan);
        assert_eq!(first.status, atomic_status::OK);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users WHERE id = ?", [doomed], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);

        let second = run_plan(&conn, &plan);
        assert_eq!(second.status, atomic_status::OK);
        assert!(second.replayed);

        let again = plan_atomic(
            &test_req(DbAtomicParams::DeleteUser { user_id: doomed }, "del-2"),
            now,
        )
        .unwrap();
        let third = run_plan(&conn, &again);
        assert_eq!(third.status, atomic_status::NOT_FOUND);
    }

    #[test]
    fn last_owner_delete_plan_atomic_keeps_the_row() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let owner = seed_user(&conn, "owner", "active", "Only");
        let now = "2024-06-01T00:00:00Z";
        let plan = plan_atomic(
            &test_req(DbAtomicParams::DeleteUser { user_id: owner }, "del-owner"),
            now,
        )
        .unwrap();
        let result = run_plan(&conn, &plan);
        assert_eq!(result.status, atomic_status::LAST_OWNER);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM users WHERE id = ?", [owner], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn redeem_plan_atomic_replays_identity_after_lost_response() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let (_user, identity, token) = seed_claim(&conn, Some("already-set"));
        let now = "2024-06-01T12:00:00Z";
        let req = test_req(
            DbAtomicParams::RedeemClaimTicket {
                token_hash: token,
                session_hash: "session-a".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                user_agent: None,
                device_type: None,
                client_label: None,
                new_password_hash: None,
                password_fingerprint: None,
            },
            "redeem-1",
        );
        let plan = plan_atomic(&req, now).unwrap();
        let first = run_plan(&conn, &plan);
        assert_eq!(first.status, atomic_status::OK);
        assert_eq!(first.payload.as_ref().unwrap()["id"], identity);
        let second = run_plan(&conn, &plan);
        assert_eq!(second.status, atomic_status::OK);
        assert!(second.replayed);
        assert_eq!(second.payload, first.payload);
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM portal_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
    }

    #[test]
    fn take_oidc_receipt_replays_after_commit() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_oidc_rp_state(&conn, "abc", "2099-01-01T00:00:00Z", Some(42));
        let now = "2024-06-01T00:00:00Z";
        let req = test_req(
            DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
            "op-take-1",
        );
        let plan = plan_atomic(&req, now).unwrap();
        let first = run_plan(&conn, &plan);
        assert_eq!(first.status, atomic_status::OK);
        assert!(!first.replayed);
        assert_eq!(first.payload.as_ref().unwrap()["pkce_verifier"], "verifier");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM oidc_rp_states", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);

        let second = run_plan(&conn, &plan);
        assert_eq!(second.status, atomic_status::OK);
        assert!(second.replayed);
        assert_eq!(second.payload, first.payload);

        let conflict = plan_atomic(
            &test_req(
                DbAtomicParams::TakeOidcRpState {
                    state_hash: "other".into(),
                },
                "op-take-1",
            ),
            now,
        )
        .unwrap();
        let lost = run_plan(&conn, &conflict);
        assert_eq!(lost.status, atomic_status::IDEMPOTENCY_CONFLICT);
    }

    fn envelope_for_plan(plan: &AtomicPlan, final_row: JsonValue) -> JsonValue {
        let results: Vec<JsonValue> = plan
            .statements
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if Some(i) == plan.receipt_select_index {
                    json!({ "success": true, "results": [final_row.clone()] })
                } else {
                    json!({ "success": true, "results": [] })
                }
            })
            .collect();
        json!({ "success": true, "result": results })
    }

    #[test]
    fn malformed_final_receipt_row_with_correct_count_is_ambiguous() {
        let req = test_req(
            DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
            "op-malformed",
        );
        let plan = plan_atomic(&req, "2024-06-01T00:00:00Z").unwrap();
        let value = envelope_for_plan(
            &plan,
            json!({
                "operation_id": "op-malformed",
                "status": "ok",
                "created_at": "2024-06-01T00:00:00Z"
            }),
        );
        let err = parse_and_validate_batch(&plan, &value, &req.operation_id).unwrap_err();
        assert!(is_ambiguous_d1(&err), "{err}");
        assert!(err.to_string().contains("request_hash"), "{err}");
    }

    #[test]
    fn valid_different_request_hash_is_idempotency_conflict() {
        let req = test_req(
            DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
            "op-conflict",
        );
        let plan = plan_atomic(&req, "2024-06-01T00:00:00Z").unwrap();
        let value = envelope_for_plan(
            &plan,
            json!({
                "operation_id": "op-conflict",
                "request_hash": "different-hash",
                "status": "ok",
                "created_at": "2024-06-01T00:00:00Z"
            }),
        );
        let parsed = parse_and_validate_batch(&plan, &value, &req.operation_id).unwrap();
        let result = interpret_atomic(&plan, &parsed);
        assert_eq!(result.status, atomic_status::IDEMPOTENCY_CONFLICT);
    }
}
