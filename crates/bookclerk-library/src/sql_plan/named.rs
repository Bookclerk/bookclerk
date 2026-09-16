//! Host compiler for named `DbAtomicParams` into a generic SQL plan.
//!
//! Control flow lives in `WHERE` clauses (and a status `SELECT`) so D1 and
//! other non-interactive backends never need mid-transaction round-trips.
//! Consume-once ops use `DELETE … RETURNING`. Receipt wrapping is host SQL.

use crate::atomic_ops::{atomic_status, DbAtomicParams};
use bookclerk_plugin_abi::DbPlanStatementKind;
use sea_orm::DbErr;
use serde_json::Value as JsonValue;

#[cfg(test)]
use crate::atomic_ops::DbAtomicResult;
#[cfg(test)]
use serde_json::json;

use super::{wire_plan, CompiledAtomic};

/// One statement in a planned batch (SQL, binds, and structural kind).
#[derive(Clone, Debug)]
pub(crate) struct SqlStmt {
    /// Dialect-neutral SQL text.
    pub sql: String,
    /// Ordered JSON binds.
    pub binds: Vec<JsonValue>,
    /// Host-authored kind; adapters must not reparse SQL.
    pub kind: DbPlanStatementKind,
    /// Proven row upper bound (`0` = unproven). Host `Returning` is `1`.
    pub max_rows: u32,
}

/// Planned batch plus the index of the application-status `SELECT`.
struct AtomicPlan {
    /// Ordered statements that run as one SQL transaction.
    statements: Vec<SqlStmt>,
    /// Index of the application-status `SELECT` inside [`AtomicPlan::statements`].
    outcome_index: usize,
    /// Index of the payload `SELECT` when the op returns a user or identity row.
    payload_index: Option<usize>,
    /// `DELETE … RETURNING` consume-once; when set, expiry uses this cutoff.
    #[allow(dead_code)]
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
    /// Caller-owned idempotency key reused across retries of the same attempt.
    operation_id: String,
    /// SHA-256 hex of the operation payload; a mismatch is an idempotency conflict.
    request_hash: String,
    /// Wire operation name stored on the receipt (`deleteUser`, …).
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

/// Compiles a named atomic request into canonical Bookclerk SQL.
///
/// Adapters lower placeholders and functions at execute time.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when the named operation is missing or invalid.
pub fn compile_named_request(
    operation_id: &str,
    params: &DbAtomicParams,
    now: &str,
) -> std::result::Result<CompiledAtomic, DbErr> {
    let inner = plan_atomic(operation_id, params, now)?;
    let expected_hash = inner.expected_hash.clone().unwrap_or_default();
    // Canonical Bookclerk SQL (`?` placeholders, SQLite-shaped helpers).
    // Adapters lower placeholders and functions at execute time.
    Ok(CompiledAtomic {
        plan: wire_plan(
            inner.statements,
            inner.outcome_index,
            inner.payload_index,
            inner.prior_receipt_index,
            inner.receipt_select_index,
        ),
        expected_hash,
    })
}

/// Compiles a host-side CAS claim for one pending `event_deliveries` row.
///
/// # Errors
///
/// Returns [`DbErr::Custom`] when wrapping the plan fails.
#[allow(clippy::too_many_arguments)]
pub fn compile_claim_event_delivery(
    operation_id: &str,
    delivery_id: &str,
    owner: &str,
    lease_secs: i64,
    plugin_id: &str,
    resource_class: &str,
    max_in_flight: i64,
    now: &str,
) -> std::result::Result<CompiledAtomic, DbErr> {
    let inner = plan_claim_event_delivery_cas(
        delivery_id,
        owner,
        lease_secs,
        plugin_id,
        resource_class,
        max_in_flight,
        now,
    );
    let request_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"claimEventDelivery:");
        hasher.update(delivery_id.as_bytes());
        hasher.update(b":");
        hasher.update(owner.as_bytes());
        hasher.update(b":");
        hasher.update(lease_secs.to_string().as_bytes());
        hasher.update(b":");
        hasher.update(plugin_id.as_bytes());
        hasher.update(b":");
        hasher.update(resource_class.as_bytes());
        hasher.update(b":");
        hasher.update(max_in_flight.to_string().as_bytes());
        hex::encode(hasher.finalize())
    };
    let ctx = ReceiptCtx {
        operation_id: operation_id.to_string(),
        request_hash: request_hash.clone(),
        kind: "claimEventDelivery",
        now: now.to_string(),
        expires_at: receipt_expiry(now),
    };
    let wrapped = wrap_status_op(inner, &ctx, PayloadKind::JsonFromPlan);
    Ok(CompiledAtomic {
        plan: wire_plan(
            wrapped.statements,
            wrapped.outcome_index,
            wrapped.payload_index,
            wrapped.prior_receipt_index,
            wrapped.receipt_select_index,
        ),
        expected_hash: request_hash,
    })
}

/// SHA-256 hex of the idempotency-relevant fields of `op`, mapped to [`DbErr`].
fn request_hash(op: &DbAtomicParams) -> std::result::Result<String, DbErr> {
    crate::db_atomic_request_hash(op).map_err(|err| DbErr::Custom(err.to_string()))
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
        DbAtomicParams::EnqueueJob { .. } => "enqueueJob",
        DbAtomicParams::ClaimNextJob { .. } => "claimNextJob",
        DbAtomicParams::ReserveJobTemp { .. } => "reserveJobTemp",
        DbAtomicParams::ConfirmTotpEnrollment { .. } => "confirmTotpEnrollment",
        DbAtomicParams::DisableUserTotp { .. } => "disableUserTotp",
        DbAtomicParams::PublishDomainEvent { .. } => "publishDomainEvent",
        DbAtomicParams::SetAcquireStatus { .. } => "setAcquireStatus",
        DbAtomicParams::DispatchEventDeliveries { .. } => "dispatchEventDeliveries",
        DbAtomicParams::ClaimNextEventDelivery { .. } => "claimNextEventDelivery",
    }
}

/// RFC 3339 timestamp 24 hours after `now`, or `now` when the input is unparseable.
fn receipt_expiry(now: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(now)
        .map(|dt| (dt + chrono::Duration::hours(24)).to_rfc3339())
        .unwrap_or_else(|_| now.to_string())
}

/// Reject oversized or blank `publishDomainEvent` fields before hashing.
fn validate_publish_domain_event(op: &DbAtomicParams) -> std::result::Result<(), DbErr> {
    let DbAtomicParams::PublishDomainEvent {
        event_type,
        dedup_key,
        payload,
        ..
    } = op
    else {
        return Ok(());
    };
    const MAX_PAYLOAD: usize = 65_536;
    if payload.len() > MAX_PAYLOAD {
        return Err(DbErr::Custom(format!(
            "domain event payload of {} bytes exceeds {MAX_PAYLOAD}",
            payload.len()
        )));
    }
    if event_type.trim().is_empty() || dedup_key.trim().is_empty() {
        return Err(DbErr::Custom(
            "domain event type and dedup_key are required".into(),
        ));
    }
    Ok(())
}

/// Builds the D1 batch for `op`. `now` is the RFC 3339 timestamp shared by
/// every statement in the batch (consume correlation, `updated_at`, sessions).
fn plan_atomic(
    operation_id: &str,
    operation: &DbAtomicParams,
    now: &str,
) -> std::result::Result<AtomicPlan, DbErr> {
    validate_publish_domain_event(operation)?;
    let inner = plan_inner(operation, now);
    let ctx = ReceiptCtx {
        operation_id: operation_id.to_string(),
        request_hash: request_hash(operation)?,
        kind: operation_kind(operation),
        now: now.to_string(),
        expires_at: receipt_expiry(now),
    };
    Ok(match operation {
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
        DbAtomicParams::EnqueueJob { .. }
        | DbAtomicParams::ClaimNextJob { .. }
        | DbAtomicParams::ReserveJobTemp { .. }
        | DbAtomicParams::PublishDomainEvent { .. }
        | DbAtomicParams::SetAcquireStatus { .. }
        | DbAtomicParams::DispatchEventDeliveries { .. }
        | DbAtomicParams::ClaimNextEventDelivery { .. } => {
            wrap_status_op(inner, &ctx, PayloadKind::JsonFromPlan)
        }
        DbAtomicParams::ConfirmTotpEnrollment { .. } | DbAtomicParams::DisableUserTotp { .. } => {
            wrap_status_op(inner, &ctx, PayloadKind::None)
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
        DbAtomicParams::EnqueueJob {
            kind,
            payload_json,
            priority,
            max_attempts,
            max_pending,
            run_after,
        } => plan_enqueue_job(
            kind,
            payload_json,
            *priority,
            *max_attempts,
            *max_pending,
            run_after.as_deref(),
            now,
        ),
        DbAtomicParams::ClaimNextJob {
            resource_class,
            owner,
            lease_secs,
        } => plan_claim_next_job(resource_class, owner, *lease_secs, now),
        DbAtomicParams::ReserveJobTemp {
            job_id,
            path,
            reserved_bytes,
            quota_bytes,
        } => plan_reserve_job_temp(job_id, path, *reserved_bytes, *quota_bytes, now),
        DbAtomicParams::ConfirmTotpEnrollment {
            user_id,
            format,
            ciphertext,
            cipher_algorithm,
            cipher_nonce,
            kdf_algorithm,
            kdf_salt,
            kdf_m_cost,
            kdf_t_cost,
            kdf_p_cost,
            created_at,
        } => plan_confirm_totp_enrollment(
            *user_id,
            format,
            ciphertext,
            cipher_algorithm.as_deref(),
            cipher_nonce.as_deref(),
            kdf_algorithm.as_deref(),
            kdf_salt.as_deref(),
            *kdf_m_cost,
            *kdf_t_cost,
            *kdf_p_cost,
            created_at,
            now,
        ),
        DbAtomicParams::DisableUserTotp { user_id } => plan_disable_user_totp(*user_id, now),
        DbAtomicParams::PublishDomainEvent {
            id,
            event_type,
            schema_version,
            account_id,
            source,
            correlation_id,
            causation_id,
            dedup_key,
            payload,
            ordering_key,
        } => {
            let minted = if id.trim().is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                id.trim().to_string()
            };
            plan_publish_domain_event(
                &minted,
                event_type,
                *schema_version,
                account_id,
                source,
                correlation_id,
                causation_id,
                dedup_key,
                payload,
                ordering_key,
                now,
            )
        }
        DbAtomicParams::SetAcquireStatus {
            book_uuid,
            status,
            storage_key,
            error_message,
            event_id,
            event_type,
            schema_version,
            event_account_id,
            source,
            correlation_id,
            causation_id,
            dedup_key,
            payload,
            ordering_key,
        } => {
            let minted = if event_id.trim().is_empty() && !event_type.trim().is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                event_id.trim().to_string()
            };
            plan_set_acquire_status(
                book_uuid,
                status,
                storage_key.as_deref(),
                error_message.as_deref(),
                &minted,
                event_type,
                *schema_version,
                event_account_id,
                source,
                correlation_id,
                causation_id,
                dedup_key,
                payload,
                ordering_key,
                now,
            )
        }
        DbAtomicParams::DispatchEventDeliveries {
            event_id,
            subscribers_json,
            mark_dispatched,
        } => plan_dispatch_event_deliveries(event_id, subscribers_json, *mark_dispatched, now),
        DbAtomicParams::ClaimNextEventDelivery {
            owner,
            lease_secs,
            plugin_ids_json,
            max_in_flight,
        } => {
            plan_claim_next_event_delivery(owner, *lease_secs, plugin_ids_json, *max_in_flight, now)
        }
    }
}

/// Pairs SQL text with JSON binds and a structural kind.
fn sql(text: &str, params: Vec<JsonValue>) -> SqlStmt {
    let sql = text.to_string();
    let kind = authored_kind(&sql);
    let max_rows = proven_max_rows(kind, &sql);
    SqlStmt {
        sql,
        binds: params,
        kind,
        max_rows,
    }
}

/// Proven row upper bound: 1-row `Returning`, or `Select … LIMIT 1`.
fn proven_max_rows(kind: DbPlanStatementKind, sql: &str) -> u32 {
    match kind {
        DbPlanStatementKind::Returning => 1,
        DbPlanStatementKind::Select if select_limit_is_one(sql) => 1,
        _ => 0,
    }
}

/// True when the statement's trailing limit is exactly `LIMIT 1`.
fn select_limit_is_one(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let upper = trimmed.to_ascii_uppercase();
    let Some(rest) = upper.strip_suffix("LIMIT 1") else {
        return false;
    };
    rest.ends_with(|c: char| c.is_ascii_whitespace() || c == ')')
}

/// Host-authored kind from the SQL this helper just wrote (not adapter sniffing).
pub(crate) fn authored_kind(sql: &str) -> DbPlanStatementKind {
    bookclerk_plugin_abi::guest_statement_kind(sql)
}

/// Byte offset of a top-level `RETURNING` keyword, if present.
fn find_returning_clause(sql: &str) -> Option<usize> {
    last_top_level_keyword(sql, "RETURNING")
}

/// True when `keyword` appears at parenthesis depth 0 (not inside quotes).
fn has_top_level_keyword(sql: &str, keyword: &str) -> bool {
    last_top_level_keyword(sql, keyword).is_some()
}

/// Start index of the last top-level match of `keyword` (ASCII, case-insensitive).
fn last_top_level_keyword(sql: &str, keyword: &str) -> Option<usize> {
    let want = keyword.to_ascii_uppercase();
    let mut last = None;
    for_each_top_level_keyword(sql, |idx, kw| {
        if kw.eq_ignore_ascii_case(&want) {
            last = Some(idx);
        }
    });
    last
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

/// Inserts a write gate before top-level `ORDER BY` / `LIMIT` / `RETURNING`.
///
/// `INSERT … VALUES` is rewritten to `INSERT … SELECT` first so a trailing
/// `WHERE` is valid SQL. Splicing before `ORDER BY`/`LIMIT` keeps
/// `INSERT … SELECT … ORDER BY … LIMIT … RETURNING` valid after the gate.
pub(super) fn apply_write_predicate(sql: &str, kind: DbPlanStatementKind, pred: &str) -> String {
    let sql = rewrite_insert_values_to_select(sql);
    let returning_at = match kind {
        DbPlanStatementKind::Returning => find_returning_clause(&sql),
        _ => None,
    };
    let (body, returning) = if let Some(idx) = returning_at {
        (sql[..idx].trim_end(), Some(sql[idx..].trim_start()))
    } else {
        (sql.as_str(), None)
    };
    let cut = find_first_top_level_keyword(body, "ORDER")
        .or_else(|| find_first_top_level_keyword(body, "LIMIT"))
        .unwrap_or(body.len());
    let head = body[..cut].trim_end();
    let tail = body[cut..].trim_start();
    let mut out = format!("{head} {} {pred}", where_or_and(head));
    if !tail.is_empty() {
        out.push(' ');
        out.push_str(tail);
    }
    if let Some(ret) = returning {
        out.push(' ');
        out.push_str(ret);
    }
    out
}

/// Rewrites a top-level `INSERT … VALUES (…)` tuple into `INSERT … SELECT …`.
///
/// Multi-row `VALUES (…), (…)` and `DEFAULT VALUES` are left unchanged.
pub(super) fn rewrite_insert_values_to_select(sql: &str) -> String {
    if find_first_top_level_keyword(sql, "SELECT").is_some() {
        return sql.to_string();
    }
    let Some(values_at) = find_first_top_level_keyword(sql, "VALUES") else {
        return sql.to_string();
    };
    let after = skip_ascii_ws(sql, values_at + "VALUES".len());
    if sql.as_bytes().get(after) != Some(&b'(') {
        return sql.to_string();
    }
    let Some(end) = skip_balanced_parens(sql, after) else {
        return sql.to_string();
    };
    if sql[end..].trim_start().starts_with(',') {
        return sql.to_string();
    }
    let inner = sql[after + 1..end - 1].trim();
    format!("{}SELECT {inner}{}", &sql[..values_at], &sql[end..])
}

/// Byte offset of the first top-level keyword matching `want`, if present.
fn find_first_top_level_keyword(sql: &str, want: &str) -> Option<usize> {
    let mut found = None;
    for_each_top_level_keyword(sql, |idx, kw| {
        if found.is_none() && kw.eq_ignore_ascii_case(want) {
            found = Some(idx);
        }
    });
    found
}

/// Advances `i` past ASCII whitespace.
fn skip_ascii_ws(sql: &str, mut i: usize) -> usize {
    let bytes = sql.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Index just past a balanced `(...)` group starting at `start`.
fn skip_balanced_parens(sql: &str, start: usize) -> Option<usize> {
    if sql.as_bytes().get(start) != Some(&b'(') {
        return None;
    }
    let bytes = sql.as_bytes();
    let mut depth = 0usize;
    let mut i = start;
    let mut in_s = false;
    let mut in_d = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_s {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_s = false;
            }
            i += 1;
            continue;
        }
        if in_d {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_d = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_s = true;
                i += 1;
            }
            b'"' => {
                in_d = true;
                i += 1;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// `AND` when `sql` already has a top-level `WHERE`; otherwise `WHERE`.
fn where_or_and(sql: &str) -> &'static str {
    if has_top_level_keyword(sql, "WHERE") {
        "AND"
    } else {
        "WHERE"
    }
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

/// JSON number bind, or a typed BigInt null when the optional value is absent.
fn j_opt_i64(n: Option<i64>) -> JsonValue {
    match n {
        Some(v) => JsonValue::from(v),
        None => crate::sql_plan::sea_null("BigInt"),
    }
}

/// JSON `b64:` blob bind, or a typed Bytes null when the optional value is absent.
fn j_opt_blob(s: Option<&str>) -> JsonValue {
    match s {
        Some(v) => JsonValue::String(v.to_string()),
        None => crate::sql_plan::sea_null("Bytes"),
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
    /// Use the inner plan's payload `SELECT` as receipt JSON.
    JsonFromPlan,
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
fn gate_write(mut stmt: SqlStmt, operation_id: &str) -> SqlStmt {
    if !matches!(
        stmt.kind,
        DbPlanStatementKind::Execute | DbPlanStatementKind::Returning
    ) {
        return stmt;
    }
    stmt.binds.push(j_str(operation_id));
    stmt.sql = apply_write_predicate(
        &stmt.sql,
        stmt.kind,
        "NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)",
    );
    stmt
}

/// Like [`gate_write`], and only when this attempt's outcome `SELECT` is `ok`.
///
/// Uses the live outcome subquery, not a prior `db_atomic_receipts` row, so a
/// committed `ok` receipt cannot authorize replay mutations.
fn gate_write_when_outcome_ok(mut stmt: SqlStmt, operation_id: &str, outcome: &SqlStmt) -> SqlStmt {
    if !matches!(
        stmt.kind,
        DbPlanStatementKind::Execute | DbPlanStatementKind::Returning
    ) {
        return stmt;
    }
    stmt.binds.push(j_str(operation_id));
    stmt.binds.extend(outcome.binds.clone());
    let pred = format!(
        "NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?) \
         AND EXISTS (SELECT 1 FROM ({}) o WHERE o.status = '{ok}')",
        outcome.sql,
        ok = atomic_status::OK,
    );
    stmt.sql = apply_write_predicate(&stmt.sql, stmt.kind, &pred);
    stmt
}

/// Scratch `db_serialization_slots` key that holds this attempt's outcome bump.
fn outcome_scratch_key(operation_id: &str) -> String {
    format!("atomic-outcome:{operation_id}")
}

/// Maps an outcome status expression to a durable integer bump.
fn status_to_bump_sql(status_sql: &str) -> String {
    format!(
        "CASE {status_sql} \
            WHEN '{ok}' THEN 1 \
            WHEN '{empty}' THEN 2 \
            WHEN '{not_found}' THEN 3 \
            WHEN '{last_owner}' THEN 4 \
            WHEN '{claim_invalid}' THEN 5 \
            WHEN '{password_required}' THEN 6 \
            WHEN '{conflict}' THEN 7 \
            WHEN '{duplicate}' THEN 8 \
            WHEN '{queue_full}' THEN 9 \
            ELSE 0 END",
        ok = atomic_status::OK,
        empty = atomic_status::EMPTY,
        not_found = atomic_status::NOT_FOUND,
        last_owner = atomic_status::LAST_OWNER,
        claim_invalid = atomic_status::CLAIM_INVALID,
        password_required = atomic_status::PASSWORD_REQUIRED,
        conflict = atomic_status::IDEMPOTENCY_CONFLICT,
        duplicate = atomic_status::DUPLICATE,
        queue_full = atomic_status::QUEUE_FULL,
    )
}

/// Maps a scratch bump back to an outcome status string.
fn bump_to_status_sql(bump_sql: &str) -> String {
    format!(
        "CASE {bump_sql} \
            WHEN 1 THEN '{ok}' \
            WHEN 2 THEN '{empty}' \
            WHEN 3 THEN '{not_found}' \
            WHEN 4 THEN '{last_owner}' \
            WHEN 5 THEN '{claim_invalid}' \
            WHEN 6 THEN '{password_required}' \
            WHEN 7 THEN '{conflict}' \
            WHEN 8 THEN '{duplicate}' \
            WHEN 9 THEN '{queue_full}' \
            ELSE '{claim_invalid}' END",
        ok = atomic_status::OK,
        empty = atomic_status::EMPTY,
        not_found = atomic_status::NOT_FOUND,
        last_owner = atomic_status::LAST_OWNER,
        claim_invalid = atomic_status::CLAIM_INVALID,
        password_required = atomic_status::PASSWORD_REQUIRED,
        conflict = atomic_status::IDEMPOTENCY_CONFLICT,
        duplicate = atomic_status::DUPLICATE,
        queue_full = atomic_status::QUEUE_FULL,
    )
}

/// Inserts the outcome scratch row unless this `operation_id` already has a receipt.
fn snapshot_outcome_insert(ctx: &ReceiptCtx) -> SqlStmt {
    sql(
        "INSERT OR IGNORE INTO db_serialization_slots (slot_key, bump) \
         SELECT ?, 0 WHERE NOT EXISTS (\
            SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?\
         )",
        vec![
            j_str(&outcome_scratch_key(&ctx.operation_id)),
            j_str(&ctx.operation_id),
        ],
    )
}

/// Records the live outcome status before later writes can invalidate it.
fn snapshot_outcome_update(ctx: &ReceiptCtx, outcome: &SqlStmt) -> SqlStmt {
    let bump = status_to_bump_sql("o.status");
    let mut params = outcome.binds.clone();
    params.push(j_str(&outcome_scratch_key(&ctx.operation_id)));
    sql(
        &format!(
            "UPDATE db_serialization_slots SET bump = (\
                SELECT {bump} FROM ({}) o\
             ) WHERE slot_key = ? AND bump = 0",
            outcome.sql,
        ),
        params,
    )
}

/// Inserts the receipt from the pre-write outcome scratch, not a later re-read.
fn receipt_insert_from_snapshot(ctx: &ReceiptCtx) -> SqlStmt {
    let status = bump_to_status_sql("s.bump");
    sql(
        &format!(
            "INSERT INTO db_atomic_receipts (\
                operation_id, operation_kind, request_hash, status, payload, created_at, expires_at\
             ) SELECT ?, ?, ?, {status}, NULL, ?, ? \
               FROM db_serialization_slots s \
               WHERE s.slot_key = ? \
                 AND NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)"
        ),
        vec![
            j_str(&ctx.operation_id),
            j_str(ctx.kind),
            j_str(&ctx.request_hash),
            j_str(&ctx.now),
            j_str(&ctx.expires_at),
            j_str(&outcome_scratch_key(&ctx.operation_id)),
            j_str(&ctx.operation_id),
        ],
    )
}

/// Drops the outcome scratch row after the receipt is durable.
fn clear_outcome_snapshot(ctx: &ReceiptCtx) -> SqlStmt {
    sql(
        "DELETE FROM db_serialization_slots WHERE slot_key = ?",
        vec![j_str(&outcome_scratch_key(&ctx.operation_id))],
    )
}

/// SQL that builds the guest user JSON object (password present as a boolean).
fn user_payload_json_sql() -> &'static str {
    "SELECT json_object(\
        'id', id, 'role', role, 'status', status, \
        'display_name', display_name, 'login_name', login_name, 'email', email, \
        'has_password', json(CASE WHEN password_hash IS NOT NULL AND password_hash != '' THEN 'true' ELSE 'false' END), \
        'security_version', security_version, 'created_at', created_at, 'updated_at', updated_at, \
        'last_seen_at', last_seen_at, 'avatar_source', avatar_source\
     ) AS payload FROM users WHERE id = ?"
}

/// Opening SQL for wrapping a portal-identity subquery as receipt payload JSON.
fn identity_payload_json_sql() -> &'static str {
    "SELECT json_object(\
        'id', id, 'provider', provider, 'external_user_id', external_user_id, \
        'label', label, 'user_id', user_id, 'created_at', created_at, 'picture_url', picture_url\
     ) AS payload FROM ("
}

/// Wraps a status-gated plan with prune, prior-receipt select, gated writes, and a final receipt select.
fn wrap_status_op(plan: AtomicPlan, ctx: &ReceiptCtx, payload: PayloadKind) -> AtomicPlan {
    let outcome_index = plan.outcome_index;
    let payload_index = plan.payload_index;
    let outcome = plan.statements[outcome_index].clone();
    let payload_stmt = payload_index.and_then(|idx| plan.statements.get(idx).cloned());
    let mut statements = vec![prune_receipts(ctx), select_receipt(ctx)];
    let prior_receipt_index = Some(1);
    for (i, stmt) in plan.statements.into_iter().enumerate() {
        if Some(i) == payload_index {
            continue;
        }
        if i == outcome_index {
            statements.push(snapshot_outcome_insert(ctx));
            statements.push(snapshot_outcome_update(ctx, &outcome));
            continue;
        }
        if i < outcome_index {
            statements.push(gate_write(stmt, &ctx.operation_id));
        } else {
            statements.push(gate_write_when_outcome_ok(
                stmt,
                &ctx.operation_id,
                &outcome,
            ));
        }
    }
    statements.push(receipt_insert_from_snapshot(ctx));
    statements.push(clear_outcome_snapshot(ctx));
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

/// Execute-kind statement from already-owned SQL text.
fn exec_sql(sql: String, binds: Vec<JsonValue>) -> SqlStmt {
    SqlStmt {
        sql,
        binds,
        kind: DbPlanStatementKind::Execute,
        max_rows: 0,
    }
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
            Some(exec_sql(
                sql,
                vec![j_i64(user_id), j_str(&ctx.operation_id)],
            ))
        }
        PayloadKind::Identity => {
            let payload = payload_stmt?;
            let sql = format!(
                "UPDATE db_atomic_receipts SET payload = ({}{}) AS ident) \
                 WHERE operation_id = ? AND status = '{ok}' AND payload IS NULL",
                identity_payload_json_sql(),
                payload.sql,
                ok = atomic_status::OK,
            );
            let mut params = payload.binds;
            params.push(j_str(&ctx.operation_id));
            Some(exec_sql(sql, params))
        }
        PayloadKind::JsonFromPlan => {
            let payload = payload_stmt?;
            let sql = format!(
                "UPDATE db_atomic_receipts SET payload = ({}) \
                 WHERE operation_id = ? AND status IN ('{ok}', '{dup}') AND payload IS NULL",
                payload.sql,
                ok = atomic_status::OK,
                dup = atomic_status::DUPLICATE,
            );
            let mut params = payload.binds;
            params.push(j_str(&ctx.operation_id));
            Some(exec_sql(sql, params))
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
        sql(&insert_from_row, from_row_params),
        sql(&delete_sql, delete_params),
        sql(&insert_empty, empty_params),
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

    let mut totp_p = vec![j_str(&user_id.to_string())];
    totp_p.extend(allow_p.clone());
    statements.push(sql(
        &format!(
            "DELETE FROM encrypted_secrets WHERE kind = 'totp' AND account_type = 'user' \
             AND account_id = ? AND {allow}"
        ),
        totp_p,
    ));

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
                    security_version, created_at, updated_at, last_seen_at, avatar_source \
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
                        security_version, created_at, updated_at, last_seen_at, avatar_source \
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

/// Plans TOTP confirm: replace primary secret, drop pending, set `totp_enabled`.
#[allow(clippy::too_many_arguments)]
fn plan_confirm_totp_enrollment(
    user_id: i64,
    format: &str,
    ciphertext: &str,
    cipher_algorithm: Option<&str>,
    cipher_nonce: Option<&str>,
    kdf_algorithm: Option<&str>,
    kdf_salt: Option<&str>,
    kdf_m_cost: Option<i64>,
    kdf_t_cost: Option<i64>,
    kdf_p_cost: Option<i64>,
    created_at: &str,
    now: &str,
) -> AtomicPlan {
    let account_id = user_id.to_string();
    let created = if created_at.is_empty() {
        now
    } else {
        created_at
    };
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
                "DELETE FROM encrypted_secrets \
                 WHERE kind = 'totp' AND provider = 'local' AND account_type = 'user' \
                   AND account_id = ? AND name IN ('primary', 'pending')",
                vec![j_str(&account_id)],
            ),
            sql(
                "INSERT INTO encrypted_secrets (\
                    kind, provider, account_type, account_id, name, format, ciphertext, \
                    kdf_algorithm, kdf_salt, kdf_m_cost, kdf_t_cost, kdf_p_cost, \
                    cipher_algorithm, cipher_nonce, created_at, updated_at\
                 ) SELECT 'totp', 'local', 'user', ?, 'primary', ?, ?, \
                    ?, ?, ?, ?, ?, \
                    ?, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM users WHERE id = ?)",
                vec![
                    j_str(&account_id),
                    j_str(format),
                    j_str(ciphertext),
                    j_opt_str(kdf_algorithm),
                    j_opt_blob(kdf_salt),
                    j_opt_i64(kdf_m_cost),
                    j_opt_i64(kdf_t_cost),
                    j_opt_i64(kdf_p_cost),
                    j_opt_str(cipher_algorithm),
                    j_opt_blob(cipher_nonce),
                    j_str(created),
                    j_str(now),
                    j_i64(user_id),
                ],
            ),
            sql(
                "UPDATE users SET totp_enabled = 1, updated_at = ? WHERE id = ?",
                vec![j_str(now), j_i64(user_id)],
            ),
        ],
        outcome_index: 0,
        payload_index: None,
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Plans TOTP disable: drop secrets and clear `totp_enabled`.
fn plan_disable_user_totp(user_id: i64, now: &str) -> AtomicPlan {
    let account_id = user_id.to_string();
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
                "DELETE FROM encrypted_secrets \
                 WHERE kind = 'totp' AND provider = 'local' AND account_type = 'user' \
                   AND account_id = ? AND name IN ('primary', 'pending')",
                vec![j_str(&account_id)],
            ),
            sql(
                "UPDATE users SET totp_enabled = 0, updated_at = ? WHERE id = ?",
                vec![j_str(now), j_i64(user_id)],
            ),
        ],
        outcome_index: 0,
        payload_index: None,
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
                        security_version, created_at, updated_at, last_seen_at, avatar_source \
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
                "SELECT i.id, i.provider, i.external_user_id, i.label, i.user_id, i.created_at, \
                        i.picture_url \
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

/// Write-locks the portable `job-queue` serialization slot for this batch.
fn plan_lock_job_queue() -> Vec<SqlStmt> {
    plan_lock_slot(super::JOB_QUEUE_SLOT)
}

/// Inserts the slot row if missing, then bumps it so COUNT+mutate serializes.
fn plan_lock_slot(slot_key: &str) -> Vec<SqlStmt> {
    vec![
        sql(
            "INSERT OR IGNORE INTO db_serialization_slots (slot_key, bump) \
             SELECT ?, 0 WHERE NOT EXISTS (\
                SELECT 1 FROM db_serialization_slots WHERE slot_key = ?\
             )",
            vec![j_str(slot_key), j_str(slot_key)],
        ),
        sql(
            "UPDATE db_serialization_slots SET bump = bump + 1 WHERE slot_key = ?",
            vec![j_str(slot_key)],
        ),
    ]
}

/// Marks pending rows that cannot be decoded as `invalid_job` before claim.
///
/// `json(payload)` aborts a D1 batch. Rewriting the envelope in the same
/// transaction keeps a malformed highest-priority row from poisoning claim.
/// Version is compared as TEXT (`json_extract || ''`) because SQLite
/// `json_extract` of a JSON number is INTEGER (`1 = '1'` is false) while
/// Postgres `#>>` is TEXT, and SQL v1 forbids CAST TEXT↔INTEGER.
fn plan_mark_unreadable_pending_jobs(now: &str) -> SqlStmt {
    sql(
        "UPDATE jobs SET \
            state = 'failed', \
            kind = 'invalid', \
            resource_class = 'network', \
            payload = '{\"v\":1}', \
            error_kind = 'invalid_job', \
            error_message = CASE \
                WHEN json_valid(payload) = 0 THEN 'malformed job payload JSON' \
                WHEN IFNULL(json_extract(payload, '$.v') || '', '') != '1' \
                    THEN 'unsupported job payload version' \
                WHEN resource_class NOT IN ('network', 'media', 'transcription', 'indexing') \
                    THEN 'unknown job resource class' \
                ELSE 'unknown job kind' \
            END, \
            finished_at = ?, \
            updated_at = ?, \
            lease_owner = NULL, \
            lease_expires_at = NULL \
         WHERE state = 'pending' AND ( \
            json_valid(payload) = 0 \
            OR IFNULL(json_extract(payload, '$.v') || '', '') != '1' \
            OR kind NOT IN ('scan', 'acquire', 'listen_sync', 'integration_scan') \
            OR resource_class NOT IN ('network', 'media', 'transcription', 'indexing') \
         )",
        vec![j_str(now), j_str(now)],
    )
}

/// Dedup + cap + insert for a durable job row.
fn plan_enqueue_job(
    kind: &str,
    payload_json: &str,
    priority: i64,
    max_attempts: i64,
    max_pending: i64,
    run_after: Option<&str>,
    now: &str,
) -> AtomicPlan {
    let run_after = run_after.unwrap_or(now);
    let dedup = match kind {
        "scan" => format!("scan:account={}", json_account_from_payload(payload_json)),
        "acquire" => format!(
            "acquire:title={}:account={}",
            json_title_from_payload(payload_json),
            json_account_from_payload(payload_json)
        ),
        "listen_sync" => "listen_sync".into(),
        "integration_scan" => format!(
            "integration_scan:id={}:force={}",
            json_integration_from_payload(payload_json),
            json_force_from_payload(payload_json)
        ),
        other => format!("{other}:unknown"),
    };
    let job_id = format!("{}-{}", kind, uuid::Uuid::new_v4());
    let mut statements = plan_lock_job_queue();
    statements.push(sql(
        "INSERT INTO jobs (\
                    id, kind, state, priority, resource_class, payload, progress, \
                    attempt_count, max_attempts, run_after, lease_owner, lease_expires_at, \
                    dedup_key, error_kind, error_message, cancel_requested, \
                    created_at, updated_at, started_at, finished_at, lease_generation\
                 ) SELECT ?, ?, 'pending', ?, 'network', ?, \
                    NULL, 0, ?, ?, NULL, NULL, ?, NULL, NULL, 0, ?, ?, NULL, NULL, 0 \
                 WHERE NOT EXISTS (\
                    SELECT 1 FROM jobs WHERE dedup_key = ? AND state IN ('pending', 'running')\
                 ) AND (SELECT COUNT(*) FROM jobs WHERE state IN ('pending', 'running')) < ?",
        vec![
            j_str(&job_id),
            j_str(kind),
            j_i64(priority),
            j_str(payload_json),
            j_i64(max_attempts.max(1)),
            j_str(run_after),
            j_str(&dedup),
            j_str(now),
            j_str(now),
            j_str(&dedup),
            j_i64(max_pending.max(0)),
        ],
    ));
    statements.push(sql(
                "SELECT CASE \
                    WHEN EXISTS (SELECT 1 FROM jobs WHERE dedup_key = ? AND state IN ('pending','running') \
                         AND created_at = ?) THEN 'ok' \
                    WHEN EXISTS (SELECT 1 FROM jobs WHERE dedup_key = ? AND state IN ('pending','running')) \
                         THEN 'duplicate' \
                    ELSE 'queueFull' END AS status",
                vec![j_str(&dedup), j_str(now), j_str(&dedup)],
            ));
    statements.push(sql(
        "SELECT json_object('id', id) AS payload FROM jobs \
                 WHERE dedup_key = ? AND state IN ('pending','running') \
                 ORDER BY created_at ASC LIMIT 1",
        vec![j_str(&dedup)],
    ));
    AtomicPlan {
        statements,
        outcome_index: 3,
        payload_index: Some(4),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Conditional claim of the next pending job in `resource_class`.
fn plan_claim_next_job(
    resource_class: &str,
    owner: &str,
    lease_secs: i64,
    now: &str,
) -> AtomicPlan {
    let expires = chrono::DateTime::parse_from_rfc3339(now)
        .map(|dt| (dt + chrono::Duration::seconds(lease_secs.max(5))).to_rfc3339())
        .unwrap_or_else(|_| now.to_string());
    let mut statements = plan_lock_job_queue();
    statements.push(plan_mark_unreadable_pending_jobs(now));
    statements.push(sql(
        "UPDATE jobs SET \
                    state = 'running', \
                    attempt_count = attempt_count + 1, \
                    lease_owner = ?, \
                    lease_expires_at = ?, \
                    lease_generation = lease_generation + 1, \
                    started_at = COALESCE(started_at, ?), \
                    updated_at = ?, \
                    error_kind = NULL, \
                    error_message = NULL \
                 WHERE id = (\
                    SELECT id FROM jobs \
                     WHERE resource_class = ? AND state = 'pending' AND run_after <= ? \
                       AND cancel_requested = 0 \
                       AND json_valid(payload) = 1 \
                       AND IFNULL(json_extract(payload, '$.v') || '', '') = '1' \
                       AND kind IN ('scan', 'acquire', 'listen_sync', 'integration_scan') \
                       AND resource_class IN ('network', 'media', 'transcription', 'indexing') \
                     ORDER BY priority DESC, created_at ASC LIMIT 1\
                 ) AND state = 'pending'",
        vec![
            j_str(owner),
            j_str(&expires),
            j_str(now),
            j_str(now),
            j_str(resource_class),
            j_str(now),
        ],
    ));
    statements.push(sql(
                "SELECT CASE WHEN EXISTS (\
                    SELECT 1 FROM jobs WHERE lease_owner = ? AND state = 'running' AND updated_at = ?\
                 ) THEN 'ok' ELSE 'empty' END AS status",
                vec![j_str(owner), j_str(now)],
            ));
    statements.push(sql(
                "SELECT json_object(\
                    'id', id, 'kind', kind, 'state', state, 'priority', priority, \
                    'resource_class', resource_class, 'payload', json(payload), \
                    'progress', progress, 'attempt_count', attempt_count, \
                    'max_attempts', max_attempts, 'run_after', run_after, \
                    'lease_owner', lease_owner, 'lease_expires_at', lease_expires_at, \
                    'dedup_key', dedup_key, 'error_kind', error_kind, \
                    'error_message', error_message, \
                    'cancel_requested', json(CASE WHEN cancel_requested != 0 THEN 'true' ELSE 'false' END), \
                    'created_at', created_at, 'updated_at', updated_at, \
                    'started_at', started_at, 'finished_at', finished_at, \
                    'lease_generation', lease_generation\
                 ) AS payload FROM jobs \
                 WHERE lease_owner = ? AND state = 'running' AND updated_at = ? \
                 ORDER BY started_at DESC LIMIT 1",
                vec![j_str(owner), j_str(now)],
            ));
    AtomicPlan {
        statements,
        outcome_index: 4,
        payload_index: Some(5),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Reserve scratch bytes when the sum of reservations stays at or under the quota.
fn plan_reserve_job_temp(
    job_id: &str,
    path: &str,
    reserved_bytes: i64,
    quota_bytes: i64,
    now: &str,
) -> AtomicPlan {
    let mut statements = plan_lock_job_queue();
    statements.push(sql(
        "INSERT OR IGNORE INTO job_temp_paths (job_id, path, created_at, reserved_bytes) \
                 SELECT ?, ?, ?, 0 WHERE NOT EXISTS (\
                    SELECT 1 FROM job_temp_paths WHERE job_id = ? AND path = ?\
                 )",
        vec![
            j_str(job_id),
            j_str(path),
            j_str(now),
            j_str(job_id),
            j_str(path),
        ],
    ));
    statements.push(sql(
        "UPDATE job_temp_paths SET reserved_bytes = ? \
                 WHERE job_id = ? AND path = ? \
                   AND (SELECT COALESCE(SUM(reserved_bytes), 0) FROM job_temp_paths) \
                       - reserved_bytes + ? <= ?",
        vec![
            j_i64(reserved_bytes),
            j_str(job_id),
            j_str(path),
            j_i64(reserved_bytes),
            j_i64(quota_bytes),
        ],
    ));
    statements.push(sql(
                "SELECT CASE WHEN EXISTS (\
                    SELECT 1 FROM job_temp_paths WHERE job_id = ? AND path = ? AND reserved_bytes = ?\
                 ) THEN 'ok' ELSE 'notFound' END AS status",
                vec![j_str(job_id), j_str(path), j_i64(reserved_bytes)],
            ));
    AtomicPlan {
        statements,
        outcome_index: 4,
        payload_index: None,
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Insert a domain event unless `(account_id, source, event_type, dedup_key)` already exists.
#[allow(clippy::too_many_arguments)]
fn plan_publish_domain_event(
    id: &str,
    event_type: &str,
    schema_version: i64,
    account_id: &str,
    source: &str,
    correlation_id: &str,
    causation_id: &str,
    dedup_key: &str,
    payload: &str,
    ordering_key: &str,
    now: &str,
) -> AtomicPlan {
    AtomicPlan {
        statements: vec![
            sql(
                "INSERT INTO domain_events (\
                    id, event_type, schema_version, occurred_at, account_id, source, \
                    correlation_id, causation_id, dedup_key, payload, ordering_key, \
                    dispatch_state, created_at, wake_pending\
                 ) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 1 \
                 WHERE NOT EXISTS (\
                    SELECT 1 FROM domain_events \
                    WHERE account_id = ? AND source = ? AND event_type = ? AND dedup_key = ?\
                 )",
                vec![
                    j_str(id),
                    j_str(event_type),
                    j_i64(schema_version.max(1)),
                    j_str(now),
                    j_str(account_id),
                    j_str(source),
                    j_str(correlation_id),
                    j_str(causation_id),
                    j_str(dedup_key),
                    j_str(payload),
                    j_str(ordering_key),
                    j_str(now),
                    j_str(account_id),
                    j_str(source),
                    j_str(event_type),
                    j_str(dedup_key),
                ],
            ),
            sql(
                "SELECT CASE \
                    WHEN EXISTS (SELECT 1 FROM domain_events WHERE id = ?) THEN 'ok' \
                    WHEN EXISTS (SELECT 1 FROM domain_events \
                         WHERE account_id = ? AND source = ? AND event_type = ? AND dedup_key = ?) \
                         THEN 'duplicate' \
                    ELSE 'notFound' END AS status",
                vec![
                    j_str(id),
                    j_str(account_id),
                    j_str(source),
                    j_str(event_type),
                    j_str(dedup_key),
                ],
            ),
            sql(
                "SELECT json_object('id', id) AS payload FROM domain_events \
                 WHERE account_id = ? AND source = ? AND event_type = ? AND dedup_key = ? LIMIT 1",
                vec![
                    j_str(account_id),
                    j_str(source),
                    j_str(event_type),
                    j_str(dedup_key),
                ],
            ),
        ],
        outcome_index: 1,
        payload_index: Some(2),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Update a book and optionally insert `book_acquired` in the same batch.
#[allow(clippy::too_many_arguments)]
fn plan_set_acquire_status(
    book_uuid: &str,
    status: &str,
    storage_key: Option<&str>,
    error_message: Option<&str>,
    event_id: &str,
    event_type: &str,
    schema_version: i64,
    event_account_id: &str,
    source: &str,
    correlation_id: &str,
    causation_id: &str,
    dedup_key: &str,
    payload: &str,
    ordering_key: &str,
    now: &str,
) -> AtomicPlan {
    AtomicPlan {
        statements: vec![
            sql(
                "UPDATE books SET acquire_status = ?, storage_key = ?, error_message = ?, updated_at = ? \
                 WHERE uuid = ?",
                vec![
                    j_str(status),
                    j_opt_str(storage_key),
                    j_opt_str(error_message),
                    j_str(now),
                    j_str(book_uuid),
                ],
            ),
            sql(
                "INSERT INTO domain_events (\
                    id, event_type, schema_version, occurred_at, account_id, source, \
                    correlation_id, causation_id, dedup_key, payload, ordering_key, \
                    dispatch_state, created_at, wake_pending\
                 ) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 1 \
                 WHERE ? != '' AND NOT EXISTS (\
                    SELECT 1 FROM domain_events \
                    WHERE account_id = ? AND source = ? AND event_type = ? AND dedup_key = ?\
                 ) AND EXISTS (SELECT 1 FROM books WHERE uuid = ?)",
                vec![
                    j_str(event_id),
                    j_str(event_type),
                    j_i64(schema_version.max(1)),
                    j_str(now),
                    j_str(event_account_id),
                    j_str(source),
                    j_str(correlation_id),
                    j_str(causation_id),
                    j_str(dedup_key),
                    j_str(payload),
                    j_str(ordering_key),
                    j_str(now),
                    j_str(event_type),
                    j_str(event_account_id),
                    j_str(source),
                    j_str(event_type),
                    j_str(dedup_key),
                    j_str(book_uuid),
                ],
            ),
            sql(
                "SELECT CASE WHEN EXISTS (SELECT 1 FROM books WHERE uuid = ?) \
                    THEN 'ok' ELSE 'notFound' END AS status",
                vec![j_str(book_uuid)],
            ),
        ],
        outcome_index: 2,
        payload_index: None,
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Create deliveries from a JSON subscriber array; mark dispatched only on the last page.
fn plan_dispatch_event_deliveries(
    event_id: &str,
    subscribers_json: &str,
    mark_dispatched: bool,
    now: &str,
) -> AtomicPlan {
    let subs: Vec<JsonValue> = serde_json::from_str(subscribers_json).unwrap_or_default();
    let mut statements = Vec::new();
    let mut page_plugins: Vec<String> = Vec::new();
    for sub in &subs {
        let plugin_id = sub
            .get("pluginId")
            .or_else(|| sub.get("plugin_id"))
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .trim();
        if plugin_id.is_empty() {
            continue;
        }
        let resource = sub
            .get("resourceClass")
            .or_else(|| sub.get("resource_class"))
            .and_then(JsonValue::as_str)
            .unwrap_or("network");
        if resource != "network" {
            continue;
        }
        let delivery_id = format!("{event_id}:{plugin_id}");
        page_plugins.push(plugin_id.to_string());
        statements.push(sql(
            "INSERT OR IGNORE INTO event_deliveries (\
                id, event_id, plugin_id, idempotency_key, state, attempt_count, max_attempts, \
                lease_owner, lease_expires_at, lease_generation, run_after, invocation_sequence, \
                resume_pending, checkpoint_json, checkpoint_schema_version, ordering_key, \
                outcome, error_message, created_at, updated_at, cancel_requested, resource_class, \
                wake_event_type, wake_filter_json, wake_grants_json\
             ) SELECT ?, ?, ?, ?, 'pending', 0, 8, NULL, NULL, 0, ?, 0, 0, NULL, 0, \
                COALESCE((SELECT ordering_key FROM domain_events WHERE id = ?), ''), \
                NULL, NULL, ?, ?, 0, 'network', '', '', '' \
             WHERE EXISTS (SELECT 1 FROM domain_events WHERE id = ?) \
             RETURNING id",
            vec![
                j_str(&delivery_id),
                j_str(event_id),
                j_str(plugin_id),
                j_str(&delivery_id),
                j_str(now),
                j_str(event_id),
                j_str(now),
                j_str(now),
                j_str(event_id),
            ],
        ));
    }
    if mark_dispatched {
        statements.push(sql(
            "INSERT OR IGNORE INTO event_outbox_stats (\
                id, retries_total, suspensions_total, dead_letters_total, \
                dispatch_latency_ms_sum, dispatch_count, handler_latency_ms_sum, handler_count\
             ) SELECT 1, 0, 0, 0, 0, 0, 0, 0 WHERE TRUE",
            vec![],
        ));
        statements.push(sql(
            "UPDATE event_outbox_stats SET \
                dispatch_count = dispatch_count + 1, \
                dispatch_latency_ms_sum = dispatch_latency_ms_sum + MAX(0, \
                    CAST((julianday(?) - julianday((SELECT created_at FROM domain_events WHERE id = ?))) * 86400000 AS INTEGER)\
                ) \
             WHERE id = 1 AND EXISTS (\
                SELECT 1 FROM domain_events WHERE id = ? AND dispatch_state = 'pending'\
             )",
            vec![j_str(now), j_str(event_id), j_str(event_id)],
        ));
        statements.push(sql(
            "UPDATE domain_events SET dispatch_state = 'dispatched' \
             WHERE id = ? AND dispatch_state = 'pending'",
            vec![j_str(event_id)],
        ));
    }
    let outcome_index = statements.len();
    statements.push(sql(
        "SELECT CASE WHEN EXISTS (SELECT 1 FROM domain_events WHERE id = ?) \
            THEN 'ok' ELSE 'notFound' END AS status",
        vec![j_str(event_id)],
    ));
    let payload_index = statements.len();
    let mut created_binds = vec![j_str(event_id), j_str(now)];
    let plugin_placeholders = if page_plugins.is_empty() {
        created_binds.push(j_str(""));
        "?".to_string()
    } else {
        created_binds.extend(page_plugins.iter().map(|p| j_str(p)));
        vec!["?"; page_plugins.len()].join(", ")
    };
    statements.push(sql(
        &format!(
            "SELECT json_object('created', \
                (SELECT COUNT(*) FROM event_deliveries \
                 WHERE event_id = ? AND created_at = ? AND plugin_id IN ({plugin_placeholders}))) \
             AS payload"
        ),
        created_binds,
    ));
    AtomicPlan {
        statements,
        outcome_index,
        payload_index: Some(payload_index),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Conditional CAS claim of one pending event delivery (host already filtered eligibility).
fn plan_claim_event_delivery_cas(
    delivery_id: &str,
    owner: &str,
    lease_secs: i64,
    plugin_id: &str,
    resource_class: &str,
    max_in_flight: i64,
    now: &str,
) -> AtomicPlan {
    let expires = chrono::DateTime::parse_from_rfc3339(now)
        .map(|dt| (dt + chrono::Duration::seconds(lease_secs.max(5))).to_rfc3339())
        .unwrap_or_else(|_| now.to_string());
    let class = if resource_class.trim().is_empty() {
        "network"
    } else {
        resource_class.trim()
    };
    let slot = super::event_inflight_slot(plugin_id, class);
    let mut statements = plan_lock_slot(&slot);
    statements.push(sql(
        "UPDATE event_deliveries SET \
            state = 'running', \
            attempt_count = CASE WHEN resume_pending = 1 THEN MAX(attempt_count, 1) ELSE attempt_count + 1 END, \
            resume_pending = 0, \
            lease_owner = ?, \
            lease_expires_at = ?, \
            lease_generation = lease_generation + 1, \
            updated_at = ? \
         WHERE id = ? AND state = 'pending' \
           AND ( \
                SELECT COUNT(*) FROM event_deliveries running \
                WHERE running.plugin_id = ? \
                  AND running.state = 'running' \
                  AND COALESCE(running.resource_class, 'network') = ? \
           ) < ?",
        vec![
            j_str(owner),
            j_str(&expires),
            j_str(now),
            j_str(delivery_id),
            j_str(plugin_id),
            j_str(class),
            j_i64(max_in_flight.max(0)),
        ],
    ));
    statements.push(sql(
        "SELECT CASE WHEN EXISTS (\
            SELECT 1 FROM event_deliveries \
            WHERE id = ? AND state = 'running' AND lease_owner = ? AND updated_at = ?\
         ) THEN 'ok' ELSE 'empty' END AS status",
        vec![j_str(delivery_id), j_str(owner), j_str(now)],
    ));
    statements.push(sql(
        "SELECT json_object(\
            'id', id, 'event_id', event_id, 'plugin_id', plugin_id, \
            'idempotency_key', idempotency_key, 'state', state, \
            'attempt_count', attempt_count, 'max_attempts', max_attempts, \
            'lease_owner', lease_owner, 'lease_expires_at', lease_expires_at, \
            'lease_generation', lease_generation, 'run_after', run_after, \
            'invocation_sequence', invocation_sequence, \
            'resume_pending', json(CASE WHEN resume_pending != 0 THEN 'true' ELSE 'false' END), \
            'checkpoint_json', checkpoint_json, \
            'checkpoint_schema_version', checkpoint_schema_version, \
            'ordering_key', ordering_key, 'outcome', outcome, \
            'error_message', error_message, 'created_at', created_at, 'updated_at', updated_at, \
            'cancel_requested', json(CASE WHEN cancel_requested != 0 THEN 'true' ELSE 'false' END), \
            'resource_class', COALESCE(resource_class, 'network'), \
            'wake_event_type', COALESCE(wake_event_type, ''), \
            'wake_filter_json', COALESCE(wake_filter_json, ''), \
            'wake_grants_json', COALESCE(wake_grants_json, '')\
         ) AS payload FROM event_deliveries \
         WHERE id = ? AND state = 'running' AND lease_owner = ? AND updated_at = ?",
        vec![j_str(delivery_id), j_str(owner), j_str(now)],
    ));
    let outcome_index = statements.len() - 2;
    let payload_index = statements.len() - 1;
    AtomicPlan {
        statements,
        outcome_index,
        payload_index: Some(payload_index),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Conditional claim of the next pending event delivery.
fn plan_claim_next_event_delivery(
    owner: &str,
    lease_secs: i64,
    plugin_ids_json: &str,
    max_in_flight: i64,
    now: &str,
) -> AtomicPlan {
    let expires = chrono::DateTime::parse_from_rfc3339(now)
        .map(|dt| (dt + chrono::Duration::seconds(lease_secs.max(5))).to_rfc3339())
        .unwrap_or_else(|_| now.to_string());
    let ids_json = if plugin_ids_json.trim().is_empty() {
        "[]"
    } else {
        plugin_ids_json
    };
    AtomicPlan {
        statements: vec![
            sql(
                "UPDATE event_deliveries SET \
                    state = 'rejected', \
                    outcome = 'reject', \
                    error_message = 'unknown event resource class `' || resource_class || '`', \
                    updated_at = ? \
                 WHERE id IN (\
                    SELECT id FROM event_deliveries \
                    WHERE state = 'pending' \
                      AND COALESCE(resource_class, 'network') != 'network' \
                      AND COALESCE(resource_class, '') != '' \
                    LIMIT 32\
                 )",
                vec![j_str(now)],
            ),
            sql(
                "UPDATE event_deliveries SET \
                    state = 'running', \
                    attempt_count = CASE WHEN resume_pending = 1 THEN MAX(attempt_count, 1) ELSE attempt_count + 1 END, \
                    resume_pending = 0, \
                    lease_owner = ?, \
                    lease_expires_at = ?, \
                    lease_generation = lease_generation + 1, \
                    updated_at = ? \
                 WHERE id = (\
                    SELECT d.id FROM event_deliveries d \
                    WHERE d.state = 'pending' AND d.run_after <= ? \
                      AND d.plugin_id IN (SELECT value FROM json_each(?)) \
                      AND COALESCE(d.resource_class, 'network') = 'network' \
                      AND NOT EXISTS (\
                        SELECT 1 FROM event_deliveries earlier \
                        WHERE earlier.plugin_id = d.plugin_id \
                          AND earlier.ordering_key = d.ordering_key \
                          AND earlier.ordering_key != '' \
                          AND earlier.created_at < d.created_at \
                          AND earlier.state IN ('pending', 'running')\
                      ) \
                      AND ( \
                        SELECT COUNT(*) FROM event_deliveries running \
                        WHERE running.plugin_id = d.plugin_id \
                          AND running.state = 'running' \
                          AND COALESCE(running.resource_class, 'network') = COALESCE(d.resource_class, 'network') \
                      ) < ? \
                    ORDER BY d.created_at ASC LIMIT 1\
                 ) AND state = 'pending'",
                vec![
                    j_str(owner),
                    j_str(&expires),
                    j_str(now),
                    j_str(now),
                    j_str(ids_json),
                    j_i64(max_in_flight.max(0)),
                ],
            ),
            sql(
                "SELECT CASE WHEN EXISTS (\
                    SELECT 1 FROM event_deliveries WHERE lease_owner = ? AND state = 'running' \
                    AND updated_at = ?\
                 ) THEN 'ok' ELSE 'empty' END AS status",
                vec![j_str(owner), j_str(now)],
            ),
            sql(
                "SELECT json_object(\
                    'id', id, 'event_id', event_id, 'plugin_id', plugin_id, \
                    'idempotency_key', idempotency_key, 'state', state, \
                    'attempt_count', attempt_count, 'max_attempts', max_attempts, \
                    'lease_owner', lease_owner, 'lease_expires_at', lease_expires_at, \
                    'lease_generation', lease_generation, 'run_after', run_after, \
                    'invocation_sequence', invocation_sequence, \
                    'resume_pending', json(CASE WHEN resume_pending != 0 THEN 'true' ELSE 'false' END), \
                    'checkpoint_json', checkpoint_json, \
                    'checkpoint_schema_version', checkpoint_schema_version, \
                    'ordering_key', ordering_key, 'outcome', outcome, \
                    'error_message', error_message, 'created_at', created_at, 'updated_at', updated_at, \
                    'cancel_requested', json(CASE WHEN cancel_requested != 0 THEN 'true' ELSE 'false' END), \
                    'resource_class', COALESCE(resource_class, 'network'), \
                    'wake_event_type', COALESCE(wake_event_type, ''), \
                    'wake_filter_json', COALESCE(wake_filter_json, ''), \
                    'wake_grants_json', COALESCE(wake_grants_json, '')\
                 ) AS payload FROM event_deliveries \
                 WHERE lease_owner = ? AND state = 'running' AND updated_at = ? \
                 ORDER BY lease_generation DESC LIMIT 1",
                vec![j_str(owner), j_str(now)],
            ),
        ],
        outcome_index: 2,
        payload_index: Some(3),
        consume_once: None,
        receipt_select_index: None,
        prior_receipt_index: None,
        expected_hash: None,
    }
}

/// Best-effort account filter from a job payload JSON string.
fn json_account_from_payload(payload_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(payload_json)
        .ok()
        .and_then(|v| {
            v.get("account")
                .and_then(|a| a.as_str())
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "all".into())
}

/// Best-effort title filter from a job payload JSON string.
fn json_title_from_payload(payload_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(payload_json)
        .ok()
        .and_then(|v| v.get("title").and_then(|a| a.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "all".into())
}

/// Best-effort `force` flag from a job payload JSON string (`0` or `1`).
fn json_force_from_payload(payload_json: &str) -> u8 {
    u8::from(
        serde_json::from_str::<serde_json::Value>(payload_json)
            .ok()
            .and_then(|v| v.get("force").and_then(JsonValue::as_bool))
            .unwrap_or(false),
    )
}

/// Best-effort integration id from a job payload JSON string.
fn json_integration_from_payload(payload_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(payload_json)
        .ok()
        .and_then(|v| {
            v.get("integration_id")
                .and_then(|a| a.as_str())
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "all".into())
}

#[cfg(test)]
#[derive(Debug, Clone)]
/// Rows returned by one statement in a D1 HTTP batch response.
struct BatchStmtResult {
    /// Result rows for this statement; empty when the statement did not return rows.
    rows: Vec<JsonValue>,
}

#[cfg(test)]
/// Builds a [`DbErr`] whose message marks the batch as possibly already committed.
fn ambiguous_d1(msg: impl std::fmt::Display) -> DbErr {
    DbErr::Custom(format!("D1 ambiguous response: {msg}"))
}

#[cfg(test)]
/// True when a D1 HTTP/parse failure may have already committed the batch.
fn is_ambiguous_d1(err: &DbErr) -> bool {
    err.to_string().contains("D1 ambiguous")
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
/// Parses a receipt `payload` cell, accepting JSON objects or a JSON-encoded string.
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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
        "last_seen_at": row.get("last_seen_at").cloned().unwrap_or(JsonValue::Null),
        "avatar_source": row.get("avatar_source").cloned().unwrap_or(JsonValue::Null),
    })
}

/// Guest portal-identity JSON after a successful claim redeem.
#[cfg(test)]
fn identity_payload(row: &JsonValue) -> JsonValue {
    json!({
        "id": row.get("id").cloned().unwrap_or(JsonValue::Null),
        "provider": row.get("provider").cloned().unwrap_or(JsonValue::Null),
        "external_user_id": row.get("external_user_id").cloned().unwrap_or(JsonValue::Null),
        "label": row.get("label").cloned().unwrap_or(JsonValue::Null),
        "user_id": row.get("user_id").cloned().unwrap_or(JsonValue::Null),
        "created_at": row.get("created_at").cloned().unwrap_or(JsonValue::Null),
        "picture_url": row.get("picture_url").cloned().unwrap_or(JsonValue::Null),
    })
}

/// Guest OIDC RP-state JSON (provider, PKCE verifier, nonce, purpose, user).
#[cfg(test)]
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
#[cfg(test)]
fn webauthn_challenge_payload(row: &JsonValue) -> JsonValue {
    json!({
        "user_id": row.get("user_id").cloned().unwrap_or(JsonValue::Null),
        "state_json": row.get("state_json").cloned().unwrap_or(JsonValue::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;

    struct NamedReq {
        operation_id: String,
        operation: DbAtomicParams,
    }

    fn test_req(op: DbAtomicParams, id: &str) -> NamedReq {
        NamedReq {
            operation_id: id.into(),
            operation: op,
        }
    }

    fn plan_req(req: &NamedReq, now: &str) -> std::result::Result<AtomicPlan, sea_orm::DbErr> {
        plan_atomic(&req.operation_id, &req.operation, now)
    }

    fn assert_host_plan_typechecks(plan: &crate::sql_plan::DbAtomicPlan) {
        let env = crate::migrations::host_sql_type_env();
        for (i, stmt) in plan.statements.iter().enumerate() {
            let req = bookclerk_plugin_abi::ExecuteRequest {
                operation_id: "t".into(),
                request_hash: String::new(),
                deadline_unix_ms: 0,
                statements: vec![bookclerk_plugin_abi::TypedDbStatement {
                    sql: stmt.sql.clone(),
                    parameters: Vec::new(),
                    kind: stmt.kind,
                    max_rows: 0,
                    result_selection: bookclerk_plugin_abi::DbResultSelection::Discard,
                }],
            };
            bookclerk_plugin_abi::typecheck_execute_request_proofs(&req, &env)
                .unwrap_or_else(|err| panic!("stmt {i}: {err}\n{}", stmt.sql));
        }
    }

    fn migrate(conn: &Connection) {
        for sql in crate::migrations::migration_sql() {
            conn.execute_batch(sql).unwrap();
        }
    }

    #[test]
    fn wrapped_password_writes_require_absent_receipt() {
        let compiled = super::compile_named_request(
            "pw-op",
            &DbAtomicParams::SetUserPasswordHash {
                user_id: 1,
                password_hash: Some("h".into()),
            },
            "2024-06-01T00:00:00Z",
        )
        .unwrap();
        let update = compiled
            .plan
            .statements
            .iter()
            .find(|s| s.sql.contains("SET password_hash"))
            .expect("password update");
        assert!(
            update
                .sql
                .contains("NOT EXISTS (SELECT 1 FROM db_atomic_receipts"),
            "post-outcome write must skip when a receipt already exists: {}",
            update.sql
        );
        assert!(
            !update
                .sql
                .contains("FROM db_atomic_receipts WHERE operation_id = ? AND status = 'ok'"),
            "must not treat a prior ok receipt as this transaction's claim: {}",
            update.sql
        );
        assert!(
            update.sql.contains("o.status"),
            "post-outcome write must require this attempt's outcome ok: {}",
            update.sql
        );
        let insert_idx = compiled
            .plan
            .statements
            .iter()
            .position(|s| s.sql.contains("INSERT INTO db_atomic_receipts"))
            .unwrap();
        let update_idx = compiled
            .plan
            .statements
            .iter()
            .position(|s| s.sql.contains("SET password_hash"))
            .unwrap();
        assert!(
            update_idx < insert_idx,
            "receipt insert must follow domain writes"
        );
        assert_host_plan_typechecks(&compiled.plan);
    }

    #[test]
    fn wrapped_delete_user_snapshots_outcome_before_row_delete() {
        let compiled = super::compile_named_request(
            "del-op",
            &DbAtomicParams::DeleteUser { user_id: 1 },
            "2024-06-01T00:00:00Z",
        )
        .unwrap();
        let snapshot_idx = compiled
            .plan
            .statements
            .iter()
            .position(|s| s.sql.contains("UPDATE db_serialization_slots SET bump"))
            .expect("outcome snapshot");
        let delete_idx = compiled
            .plan
            .statements
            .iter()
            .position(|s| s.sql.contains("DELETE FROM users WHERE id"))
            .expect("user delete");
        let insert_idx = compiled
            .plan
            .statements
            .iter()
            .position(|s| s.sql.contains("INSERT INTO db_atomic_receipts"))
            .unwrap();
        assert!(
            snapshot_idx < delete_idx,
            "outcome must be snapshotted before DELETE FROM users"
        );
        assert!(
            delete_idx < insert_idx,
            "receipt insert must follow domain writes"
        );
        assert!(
            compiled.plan.statements[insert_idx]
                .sql
                .contains("FROM db_serialization_slots"),
            "receipt must use the pre-write snapshot: {}",
            compiled.plan.statements[insert_idx].sql
        );
        assert_host_plan_typechecks(&compiled.plan);
    }

    #[test]
    fn claim_plan_typechecks_against_host_schema() {
        let compiled = super::compile_named_request(
            "claim-op",
            &DbAtomicParams::ClaimNextJob {
                resource_class: "network".into(),
                owner: "worker-1".into(),
                lease_secs: 60,
            },
            "2024-06-01T00:00:00Z",
        )
        .unwrap();
        assert_host_plan_typechecks(&compiled.plan);
    }

    #[test]
    fn wrapped_redeem_claim_typechecks_against_host_schema() {
        let compiled = super::compile_named_request(
            "redeem-op",
            &DbAtomicParams::RedeemClaimTicket {
                token_hash: "aa".repeat(32),
                session_hash: "bb".repeat(32),
                expires_at: "2024-06-02T00:00:00Z".into(),
                user_agent: None,
                device_type: None,
                client_label: None,
                new_password_hash: None,
                password_fingerprint: None,
            },
            "2024-06-01T00:00:00Z",
        )
        .unwrap();
        assert_host_plan_typechecks(&compiled.plan);
    }

    #[test]
    fn write_gate_lands_before_top_level_returning() {
        let sql = "INSERT INTO t (id) SELECT ? WHERE EXISTS (SELECT 1 FROM u WHERE id = ?) \
             RETURNING id";
        let gated = apply_write_predicate(
            sql,
            DbPlanStatementKind::Returning,
            "NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)",
        );
        let returning = gated.to_ascii_uppercase().rfind("RETURNING").unwrap();
        let gate = gated
            .find("NOT EXISTS (SELECT 1 FROM db_atomic_receipts")
            .unwrap();
        assert!(
            gate < returning,
            "receipt gate must not become a RETURNING expression: {gated}"
        );
        assert_eq!(
            authored_kind(&gated),
            DbPlanStatementKind::Returning,
            "{gated}"
        );
    }

    #[test]
    fn write_gate_lands_before_order_by_limit() {
        let sql = "INSERT INTO t (id) SELECT ? AS id ORDER BY id LIMIT 1 RETURNING id";
        let gated = apply_write_predicate(
            sql,
            DbPlanStatementKind::Returning,
            "NOT EXISTS (SELECT 1 FROM db_atomic_receipts WHERE operation_id = ?)",
        );
        let order = gated.to_ascii_uppercase().find("ORDER BY").unwrap();
        let gate = gated
            .find("NOT EXISTS (SELECT 1 FROM db_atomic_receipts")
            .unwrap();
        assert!(
            gate < order,
            "receipt gate must precede ORDER BY/LIMIT: {gated}"
        );
    }

    #[test]
    fn wrapped_dispatch_insert_gates_before_returning() {
        let compiled = super::compile_named_request(
            "disp-op",
            &DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-1".into(),
                subscribers_json: r#"[{"pluginId":"a"}]"#.into(),
                mark_dispatched: true,
            },
            "2024-06-01T00:00:00Z",
        )
        .unwrap();
        let insert = compiled
            .plan
            .statements
            .iter()
            .find(|s| s.sql.contains("INSERT") && s.sql.to_ascii_uppercase().contains("RETURNING"))
            .expect("dispatch insert returning");
        assert_eq!(insert.kind, DbPlanStatementKind::Returning);
        assert_eq!(
            insert.max_rows, 1,
            "host Returning must carry a 1-row proof"
        );
        let returning = insert.sql.to_ascii_uppercase().rfind("RETURNING").unwrap();
        let gate = insert
            .sql
            .find("NOT EXISTS (SELECT 1 FROM db_atomic_receipts")
            .expect("receipt gate");
        assert!(
            gate < returning,
            "hash-conflict retry must not insert behind RETURNING: {}",
            insert.sql
        );
    }

    #[test]
    fn proven_max_rows_for_limit_one_select_and_returning() {
        assert_eq!(
            super::proven_max_rows(
                DbPlanStatementKind::Returning,
                "INSERT INTO t (id) VALUES (1) RETURNING id"
            ),
            1
        );
        assert_eq!(
            super::proven_max_rows(
                DbPlanStatementKind::Select,
                "SELECT id FROM t ORDER BY id LIMIT 1"
            ),
            1
        );
        assert_eq!(
            super::proven_max_rows(DbPlanStatementKind::Select, "SELECT id FROM t LIMIT 2"),
            0
        );
        assert_eq!(
            super::proven_max_rows(
                DbPlanStatementKind::Execute,
                "INSERT INTO t (id) VALUES (1)"
            ),
            0
        );
    }

    #[test]
    fn dispatch_hash_conflict_does_not_insert_new_subscriber() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        let publish = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: "evt-hash".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct".into(),
                source: String::new(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:hash".into(),
                payload: "{}".into(),
                ordering_key: String::new(),
            },
            "evt-hash-pub",
        );
        assert_eq!(
            run_plan(&conn, &plan_req(&publish, now).unwrap()).status,
            atomic_status::OK
        );
        let first = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-hash".into(),
                subscribers_json: r#"[{"pluginId":"a"}]"#.into(),
                mark_dispatched: true,
            },
            "disp-same-op",
        );
        let a = run_plan(&conn, &plan_req(&first, now).unwrap());
        assert_eq!(a.status, atomic_status::OK);
        let second = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-hash".into(),
                subscribers_json: r#"[{"pluginId":"b"}]"#.into(),
                mark_dispatched: true,
            },
            "disp-same-op",
        );
        let conflict = run_plan(&conn, &plan_req(&second, now).unwrap());
        assert_eq!(conflict.status, atomic_status::IDEMPOTENCY_CONFLICT);
        let plugins: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT plugin_id FROM event_deliveries \
                     WHERE event_id = 'evt-hash' ORDER BY plugin_id",
                )
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(plugins, vec!["a".to_string()]);
        assert!(
            !plugins.iter().any(|p| p == "b"),
            "mismatched-hash dispatch must not insert subscriber b: {plugins:?}"
        );
    }

    fn json_to_rusqlite(v: &JsonValue) -> rusqlite::types::Value {
        if crate::sql_plan::sea_null_kind(v).is_some() {
            return rusqlite::types::Value::Null;
        }
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
            JsonValue::String(s) => {
                if let Some(bytes) = crate::b64_string_to_bytes(s) {
                    rusqlite::types::Value::Blob(bytes)
                } else {
                    rusqlite::types::Value::Text(s.clone())
                }
            }
            other => rusqlite::types::Value::Text(other.to_string()),
        }
    }

    fn run_plan(conn: &Connection, plan: &AtomicPlan) -> DbAtomicResult {
        let txn = conn.unchecked_transaction().unwrap();
        let mut results = Vec::new();
        for stmt in &plan.statements {
            let binds: Vec<rusqlite::types::Value> =
                stmt.binds.iter().map(json_to_rusqlite).collect();
            let mut prepared = txn.prepare(&stmt.sql).unwrap();
            let col_count = prepared.column_count();
            let names: Vec<String> = prepared
                .column_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            let mut rows_out = Vec::new();
            if col_count == 0 {
                prepared
                    .execute(rusqlite::params_from_iter(binds.iter()))
                    .unwrap();
            } else {
                let mut rows = prepared
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
        let plan = plan_req(&req, now).unwrap();
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

        let again = plan_req(
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
        let plan = plan_req(
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
        let plan = plan_req(&req, now).unwrap();
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
        let plan = plan_req(&req, now).unwrap();
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

        let conflict = plan_req(
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
        let plan = plan_req(&req, "2024-06-01T00:00:00Z").unwrap();
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
        let plan = plan_req(&req, "2024-06-01T00:00:00Z").unwrap();
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

    fn seed_pending_job(conn: &Connection, id: &str, kind: &str, payload: &str, priority: i64) {
        seed_pending_job_class(conn, id, kind, "network", payload, priority);
    }

    fn seed_pending_job_class(
        conn: &Connection,
        id: &str,
        kind: &str,
        resource_class: &str,
        payload: &str,
        priority: i64,
    ) {
        conn.execute(
            "INSERT INTO jobs (\
                id, kind, state, priority, resource_class, payload, progress, \
                attempt_count, max_attempts, run_after, lease_owner, lease_expires_at, \
                dedup_key, error_kind, error_message, cancel_requested, \
                created_at, updated_at, started_at, finished_at, lease_generation\
             ) VALUES (?, ?, 'pending', ?, ?, ?, NULL, 0, 3, \
                '2020-01-01T00:00:00Z', NULL, NULL, ?, NULL, NULL, 0, \
                '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z', NULL, NULL, 0)",
            rusqlite::params![id, kind, priority, resource_class, payload, id],
        )
        .unwrap();
    }

    fn claim_row(conn: &Connection, id: &str) -> (String, String, Option<String>, Option<String>) {
        conn.query_row(
            "SELECT state, kind, error_kind, payload FROM jobs WHERE id = ?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    }

    #[test]
    fn claim_marks_malformed_json_invalid_without_aborting_batch() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_pending_job(&conn, "bad-json", "scan", "{not-json", 10);
        seed_pending_job(&conn, "good", "scan", r#"{"v":1}"#, 0);
        let result = run_plan(
            &conn,
            &plan_claim_next_job("network", "worker-1", 60, "2024-06-01T00:00:00Z"),
        );
        assert_eq!(result.status, atomic_status::OK);
        let (state, kind, error, payload) = claim_row(&conn, "bad-json");
        assert_eq!(state, "failed");
        assert_eq!(kind, "invalid");
        assert_eq!(error.as_deref(), Some("invalid_job"));
        assert_eq!(payload.as_deref(), Some(r#"{"v":1}"#));
        let claimed: String = conn
            .query_row("SELECT id FROM jobs WHERE state = 'running'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(claimed, "good");
    }

    #[test]
    fn claim_marks_unknown_kind_and_unsupported_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_pending_job(&conn, "bad-kind", "nope", r#"{"v":1}"#, 5);
        seed_pending_job(&conn, "bad-ver", "scan", r#"{"v":99}"#, 4);
        let result = run_plan(
            &conn,
            &plan_claim_next_job("network", "worker-1", 60, "2024-06-01T00:00:00Z"),
        );
        assert_eq!(result.status, atomic_status::EMPTY);
        for id in ["bad-kind", "bad-ver"] {
            let (state, kind, error, _) = claim_row(&conn, id);
            assert_eq!(state, "failed");
            assert_eq!(kind, "invalid");
            assert_eq!(error.as_deref(), Some("invalid_job"));
        }
    }

    #[test]
    fn claim_marks_unknown_resource_class_and_still_claims_valid() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        seed_pending_job_class(&conn, "bad-class", "scan", "not_a_class", r#"{"v":1}"#, 10);
        seed_pending_job(&conn, "good", "scan", r#"{"v":1}"#, 0);
        let result = run_plan(
            &conn,
            &plan_claim_next_job("network", "worker-1", 60, "2024-06-01T00:00:00Z"),
        );
        assert_eq!(result.status, atomic_status::OK);
        let (state, kind, error, _) = claim_row(&conn, "bad-class");
        assert_eq!(state, "failed");
        assert_eq!(kind, "invalid");
        assert_eq!(error.as_deref(), Some("invalid_job"));
        let claimed: String = conn
            .query_row("SELECT id FROM jobs WHERE state = 'running'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(claimed, "good");
    }

    #[test]
    fn enqueue_keeps_forced_integration_scan_distinct() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        let normal = run_plan(
            &conn,
            &plan_enqueue_job(
                "integration_scan",
                r#"{"v":1,"integration_id":"echo"}"#,
                0,
                3,
                8,
                None,
                now,
            ),
        );
        assert_eq!(normal.status, atomic_status::OK);
        let forced = run_plan(
            &conn,
            &plan_enqueue_job(
                "integration_scan",
                r#"{"v":1,"integration_id":"echo","force":true}"#,
                0,
                3,
                8,
                None,
                "2024-06-01T00:00:01Z",
            ),
        );
        assert_eq!(forced.status, atomic_status::OK);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let again = run_plan(
            &conn,
            &plan_enqueue_job(
                "integration_scan",
                r#"{"v":1,"integration_id":"echo","force":true}"#,
                0,
                3,
                8,
                None,
                "2024-06-01T00:00:02Z",
            ),
        );
        assert_eq!(again.status, atomic_status::DUPLICATE);
    }

    fn confirm_totp_params(user_id: i64) -> DbAtomicParams {
        DbAtomicParams::ConfirmTotpEnrollment {
            user_id,
            format: "sealed-v1".into(),
            ciphertext: "b64:AA==".into(),
            cipher_algorithm: Some("xchacha20poly1305".into()),
            cipher_nonce: Some("b64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into()),
            kdf_algorithm: None,
            kdf_salt: None,
            kdf_m_cost: None,
            kdf_t_cost: None,
            kdf_p_cost: None,
            created_at: "2024-06-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn totp_optional_blob_and_int_binds_are_typed_nulls() {
        let req = test_req(confirm_totp_params(7), "totp-typed-null");
        let compiled =
            compile_named_request(&req.operation_id, &req.operation, "2024-06-01T00:00:00Z")
                .unwrap();
        let insert = compiled
            .plan
            .statements
            .iter()
            .find(|s| s.sql.contains("INSERT INTO encrypted_secrets"))
            .expect("totp plan inserts encrypted_secrets");
        assert_eq!(insert.binds[4], json!({ "$sea_null": "Bytes" }));
        assert_eq!(insert.binds[5], json!({ "$sea_null": "BigInt" }));
        assert_eq!(insert.binds[6], json!({ "$sea_null": "BigInt" }));
        assert_eq!(insert.binds[7], json!({ "$sea_null": "BigInt" }));
        assert!(insert.binds[2]
            .as_str()
            .is_some_and(|s| s.starts_with("b64:")));
        assert!(insert.binds[9]
            .as_str()
            .is_some_and(|s| s.starts_with("b64:")));
        assert!(
            insert.sql.contains('?') && !insert.sql.contains('$'),
            "host compiler must emit canonical SQL, not $n:\n{}",
            insert.sql
        );
        let lowered = bookclerk_db_exec::lower_canonical_to_postgres(&insert.sql);
        assert!(
            lowered.contains("$5"),
            "adapter lowering must number blob binds:\n{lowered}"
        );
    }

    fn seed_totp_secret(conn: &Connection, user_id: i64, name: &str) {
        conn.execute(
            "INSERT INTO encrypted_secrets (\
                kind, provider, account_type, account_id, name, format, ciphertext, \
                created_at, updated_at\
             ) VALUES ('totp', 'local', 'user', ?, ?, 'sealed-v1', x'00', \
                '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            rusqlite::params![user_id.to_string(), name],
        )
        .unwrap();
    }

    fn totp_names(conn: &Connection, user_id: i64) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM encrypted_secrets \
                 WHERE kind = 'totp' AND account_id = ? ORDER BY name",
            )
            .unwrap();
        stmt.query_map([user_id.to_string()], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn totp_enabled(conn: &Connection, user_id: i64) -> i64 {
        conn.query_row(
            "SELECT totp_enabled FROM users WHERE id = ?",
            [user_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn confirm_totp_missing_user_skips_secret_writes() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let other = seed_user(&conn, "member", "active", "Keep");
        seed_totp_secret(&conn, other, "pending");
        seed_totp_secret(&conn, 999, "pending");
        let req = test_req(confirm_totp_params(999), "totp-missing");
        let result = run_plan(&conn, &plan_req(&req, "2024-06-01T00:00:00Z").unwrap());
        assert_eq!(result.status, atomic_status::NOT_FOUND);
        assert_eq!(totp_names(&conn, 999), vec!["pending".to_string()]);
        assert_eq!(totp_names(&conn, other), vec!["pending".to_string()]);
    }

    #[test]
    fn confirm_totp_then_disable_round_trip_and_replay() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let user = seed_user(&conn, "member", "active", "Totp");
        seed_totp_secret(&conn, user, "pending");
        let now = "2024-06-01T00:00:00Z";
        let req = test_req(confirm_totp_params(user), "totp-confirm");
        let plan = plan_req(&req, now).unwrap();
        let first = run_plan(&conn, &plan);
        assert_eq!(first.status, atomic_status::OK);
        assert_eq!(totp_names(&conn, user), vec!["primary".to_string()]);
        assert_eq!(totp_enabled(&conn, user), 1);

        let replay = run_plan(&conn, &plan);
        assert_eq!(replay.status, atomic_status::OK);
        assert!(replay.replayed);
        assert_eq!(totp_names(&conn, user), vec!["primary".to_string()]);
        assert_eq!(totp_enabled(&conn, user), 1);

        let disable_req = test_req(
            DbAtomicParams::DisableUserTotp { user_id: user },
            "totp-disable",
        );
        let disable_plan = plan_req(&disable_req, now).unwrap();
        let disabled = run_plan(&conn, &disable_plan);
        assert_eq!(disabled.status, atomic_status::OK);
        assert!(totp_names(&conn, user).is_empty());
        assert_eq!(totp_enabled(&conn, user), 0);

        let disable_replay = run_plan(&conn, &disable_plan);
        assert_eq!(disable_replay.status, atomic_status::OK);
        assert!(disable_replay.replayed);
        assert!(totp_names(&conn, user).is_empty());
        assert_eq!(totp_enabled(&conn, user), 0);
    }

    #[test]
    fn disable_totp_missing_user_skips_writes() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let other = seed_user(&conn, "member", "active", "Keep");
        seed_totp_secret(&conn, other, "primary");
        conn.execute("UPDATE users SET totp_enabled = 1 WHERE id = ?", [other])
            .unwrap();
        let req = test_req(
            DbAtomicParams::DisableUserTotp { user_id: 999 },
            "totp-disable-missing",
        );
        let result = run_plan(&conn, &plan_req(&req, "2024-06-01T00:00:00Z").unwrap());
        assert_eq!(result.status, atomic_status::NOT_FOUND);
        assert_eq!(totp_names(&conn, other), vec!["primary".to_string()]);
        assert_eq!(totp_enabled(&conn, other), 1);
    }

    #[test]
    fn publish_dispatch_claim_event_delivery_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        let publish = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: "evt-1".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct".into(),
                source: "audible".into(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:u1".into(),
                payload: r#"{"titleId":"u1"}"#.into(),
                ordering_key: "u1".into(),
            },
            "evt-pub",
        );
        let created = run_plan(&conn, &plan_req(&publish, now).unwrap());
        assert_eq!(created.status, atomic_status::OK);
        let ordering: String = conn
            .query_row(
                "SELECT ordering_key FROM domain_events WHERE id = 'evt-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ordering, "u1");
        let source: String = conn
            .query_row(
                "SELECT source FROM domain_events WHERE id = 'evt-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(source, "audible");
        let wake_pending: i64 = conn
            .query_row(
                "SELECT wake_pending FROM domain_events WHERE id = 'evt-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wake_pending, 1);
        let dup = run_plan(&conn, &plan_req(&publish, now).unwrap());
        assert_eq!(dup.status, atomic_status::OK);
        assert!(dup.replayed);

        let dispatch = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-1".into(),
                subscribers_json: r#"[{"pluginId":"echo"}]"#.into(),
                mark_dispatched: true,
            },
            "evt-disp",
        );
        let dispatched = run_plan(&conn, &plan_req(&dispatch, now).unwrap());
        assert_eq!(dispatched.status, atomic_status::OK);

        let claim = test_req(
            DbAtomicParams::ClaimNextEventDelivery {
                owner: "worker-1".into(),
                lease_secs: 60,
                plugin_ids_json: r#"["echo"]"#.into(),
                max_in_flight: 1,
            },
            "evt-claim",
        );
        let claimed = run_plan(&conn, &plan_req(&claim, now).unwrap());
        assert_eq!(claimed.status, atomic_status::OK);
        let payload = claimed.payload.expect("claim payload");
        assert_eq!(payload["plugin_id"], "echo");
        assert_eq!(payload["state"], "running");

        let empty = test_req(
            DbAtomicParams::ClaimNextEventDelivery {
                owner: "worker-empty".into(),
                lease_secs: 60,
                plugin_ids_json: "[]".into(),
                max_in_flight: 1,
            },
            "evt-claim-empty",
        );
        let skipped = run_plan(&conn, &plan_req(&empty, now).unwrap());
        assert_eq!(skipped.status, atomic_status::EMPTY);
    }

    #[test]
    fn publish_domain_event_namespaces_dedup_by_account_and_source() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        let a = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: "evt-ns-a".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct-a".into(),
                source: String::new(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:ns".into(),
                payload: "{}".into(),
                ordering_key: String::new(),
            },
            "evt-ns-a",
        );
        assert_eq!(
            run_plan(&conn, &plan_req(&a, now).unwrap()).status,
            atomic_status::OK
        );
        let b = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: "evt-ns-b".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct-b".into(),
                source: String::new(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:ns".into(),
                payload: "{}".into(),
                ordering_key: String::new(),
            },
            "evt-ns-b",
        );
        assert_eq!(
            run_plan(&conn, &plan_req(&b, now).unwrap()).status,
            atomic_status::OK
        );
        let c = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: "evt-ns-src".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct-a".into(),
                source: "audible".into(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:ns".into(),
                payload: "{}".into(),
                ordering_key: String::new(),
            },
            "evt-ns-src",
        );
        assert_eq!(
            run_plan(&conn, &plan_req(&c, now).unwrap()).status,
            atomic_status::OK
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM domain_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let dup = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: "evt-ns-dup".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct-a".into(),
                source: String::new(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:ns".into(),
                payload: "{}".into(),
                ordering_key: String::new(),
            },
            "evt-ns-dup",
        );
        assert_eq!(
            run_plan(&conn, &plan_req(&dup, now).unwrap()).status,
            atomic_status::DUPLICATE
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM domain_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn dispatch_late_join_second_plugin_does_not_idempotency_conflict() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        let publish = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: "evt-late".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct".into(),
                source: String::new(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:late".into(),
                payload: r#"{"titleId":"late"}"#.into(),
                ordering_key: "late".into(),
            },
            "evt-late-pub",
        );
        assert_eq!(
            run_plan(&conn, &plan_req(&publish, now).unwrap()).status,
            atomic_status::OK
        );

        let first = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-late".into(),
                subscribers_json: r#"[{"pluginId":"echo"}]"#.into(),
                mark_dispatched: false,
            },
            "reconcile-evt-late-echo",
        );
        let a = run_plan(&conn, &plan_req(&first, now).unwrap());
        assert_eq!(a.status, atomic_status::OK);
        assert_ne!(a.status, atomic_status::IDEMPOTENCY_CONFLICT);

        let second = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-late".into(),
                subscribers_json: r#"[{"pluginId":"audiobookshelf"}]"#.into(),
                mark_dispatched: true,
            },
            "reconcile-evt-late-audiobookshelf",
        );
        let ab = run_plan(&conn, &plan_req(&second, now).unwrap());
        assert_eq!(ab.status, atomic_status::OK);
        assert_ne!(ab.status, atomic_status::IDEMPOTENCY_CONFLICT);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_deliveries WHERE event_id = 'evt-late'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        let plugins: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT plugin_id FROM event_deliveries WHERE event_id = 'evt-late' ORDER BY plugin_id")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            plugins,
            vec!["audiobookshelf".to_string(), "echo".to_string()]
        );
    }

    #[test]
    fn claim_respects_per_plugin_in_flight_cap() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        for (id, key) in [("evt-cap-a", "ka"), ("evt-cap-b", "kb")] {
            let publish = test_req(
                DbAtomicParams::PublishDomainEvent {
                    id: id.into(),
                    event_type: "book_acquired".into(),
                    schema_version: 1,
                    account_id: "acct".into(),
                    source: String::new(),
                    correlation_id: String::new(),
                    causation_id: String::new(),
                    dedup_key: format!("book_acquired:{key}"),
                    payload: "{}".into(),
                    ordering_key: key.into(),
                },
                &format!("{id}-pub"),
            );
            assert_eq!(
                run_plan(&conn, &plan_req(&publish, now).unwrap()).status,
                atomic_status::OK
            );
            let dispatch = test_req(
                DbAtomicParams::DispatchEventDeliveries {
                    event_id: id.into(),
                    subscribers_json: r#"[{"pluginId":"echo"}]"#.into(),
                    mark_dispatched: true,
                },
                &format!("dispatch-{id}-echo"),
            );
            assert_eq!(
                run_plan(&conn, &plan_req(&dispatch, now).unwrap()).status,
                atomic_status::OK
            );
        }
        let first = test_req(
            DbAtomicParams::ClaimNextEventDelivery {
                owner: "cap-w1".into(),
                lease_secs: 60,
                plugin_ids_json: r#"["echo"]"#.into(),
                max_in_flight: 1,
            },
            "evt-cap-claim-1",
        );
        let claimed = run_plan(&conn, &plan_req(&first, now).unwrap());
        assert_eq!(claimed.status, atomic_status::OK);
        let second = test_req(
            DbAtomicParams::ClaimNextEventDelivery {
                owner: "cap-w2".into(),
                lease_secs: 60,
                plugin_ids_json: r#"["echo"]"#.into(),
                max_in_flight: 1,
            },
            "evt-cap-claim-2",
        );
        let blocked = run_plan(&conn, &plan_req(&second, now).unwrap());
        assert_eq!(blocked.status, atomic_status::EMPTY);
    }

    #[test]
    fn set_acquire_status_publishes_outbox_in_same_batch() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        conn.execute(
            "INSERT INTO accounts (account_id, marketplace, source, created_at, updated_at) \
             VALUES ('user-1', 'us', 'audible', '2024-06-01T00:00:00Z', '2024-06-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO books (uuid, source, account_id, product_id, marketplace, title, created_at, updated_at) \
             VALUES ('b1', 'audible', 'user-1', 'B00X', 'us', 'T', '2024-06-01T00:00:00Z', '2024-06-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let req = test_req(
            DbAtomicParams::SetAcquireStatus {
                book_uuid: "b1".into(),
                status: "acquired".into(),
                storage_key: Some("Author/T/book.m4b".into()),
                error_message: None,
                event_id: "evt-acq".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                event_account_id: "user-1".into(),
                source: "audible".into(),
                correlation_id: "b1".into(),
                causation_id: String::new(),
                dedup_key: "book_acquired:b1".into(),
                payload: r#"{"titleId":"b1"}"#.into(),
                ordering_key: "b1".into(),
            },
            "acq-1",
        );
        let result = run_plan(&conn, &plan_req(&req, now).unwrap());
        assert_eq!(result.status, atomic_status::OK);
        let status: String = conn
            .query_row(
                "SELECT acquire_status FROM books WHERE uuid = 'b1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "acquired");
        let ordering: String = conn
            .query_row(
                "SELECT ordering_key FROM domain_events WHERE id = 'evt-acq'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ordering, "b1");

        let missing = test_req(
            DbAtomicParams::SetAcquireStatus {
                book_uuid: "missing".into(),
                status: "acquired".into(),
                storage_key: None,
                error_message: None,
                event_id: "evt-missing".into(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                event_account_id: "user-1".into(),
                source: "audible".into(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:missing".into(),
                payload: "{}".into(),
                ordering_key: "missing".into(),
            },
            "acq-missing",
        );
        let not_found = run_plan(&conn, &plan_req(&missing, now).unwrap());
        assert_eq!(not_found.status, atomic_status::NOT_FOUND);
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM domain_events WHERE id = 'evt-missing'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
    }

    #[test]
    fn publish_domain_event_mints_empty_id_and_rejects_oversized_payload() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        let publish = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: String::new(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct".into(),
                source: String::new(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:mint".into(),
                payload: r#"{"titleId":"mint"}"#.into(),
                ordering_key: "mint".into(),
            },
            "evt-mint",
        );
        let created = run_plan(&conn, &plan_req(&publish, now).unwrap());
        assert_eq!(created.status, atomic_status::OK);
        let id: String = conn
            .query_row("SELECT id FROM domain_events", [], |r| r.get(0))
            .unwrap();
        assert!(!id.is_empty(), "empty event id must be minted");
        let payload_id = created
            .payload
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert_eq!(payload_id, id);

        let huge = test_req(
            DbAtomicParams::PublishDomainEvent {
                id: String::new(),
                event_type: "book_acquired".into(),
                schema_version: 1,
                account_id: "acct".into(),
                source: String::new(),
                correlation_id: String::new(),
                causation_id: String::new(),
                dedup_key: "book_acquired:huge".into(),
                payload: "x".repeat(65_537),
                ordering_key: "huge".into(),
            },
            "evt-huge",
        );
        let err = match plan_req(&huge, now) {
            Ok(_) => panic!("oversized payload must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("exceeds"),
            "unexpected error: {err}"
        );
    }

    fn seed_domain_event(conn: &Connection, id: &str, now: &str) {
        conn.execute(
            "INSERT INTO domain_events (\
                id, event_type, schema_version, occurred_at, account_id, source, correlation_id, \
                causation_id, dedup_key, payload, ordering_key, dispatch_state, created_at, \
                wake_pending\
             ) VALUES (?, 'book_acquired', 1, ?, 'acct', '', '', '', ?, '{}', ?, 'pending', ?, 0)",
            rusqlite::params![id, now, format!("book_acquired:{id}"), id, now],
        )
        .unwrap();
    }

    fn seed_pending_delivery(conn: &Connection, event_id: &str, plugin_id: &str, now: &str) {
        let id = format!("{event_id}:{plugin_id}");
        conn.execute(
            "INSERT INTO event_deliveries (\
                id, event_id, plugin_id, idempotency_key, state, attempt_count, max_attempts, \
                lease_owner, lease_expires_at, lease_generation, run_after, invocation_sequence, \
                resume_pending, checkpoint_json, checkpoint_schema_version, ordering_key, \
                outcome, error_message, created_at, updated_at, cancel_requested, resource_class, \
                wake_event_type, wake_filter_json, wake_grants_json\
             ) VALUES (?, ?, ?, ?, 'pending', 0, 8, NULL, NULL, 0, ?, 0, 0, NULL, 0, '', \
                NULL, NULL, ?, ?, 0, 'network', '', '', '')",
            rusqlite::params![id, event_id, plugin_id, id, now, now, now],
        )
        .unwrap();
    }

    fn run_compiled(
        conn: &Connection,
        compiled: &crate::sql_plan::CompiledAtomic,
    ) -> DbAtomicResult {
        let plan = AtomicPlan {
            statements: compiled
                .plan
                .statements
                .iter()
                .map(|s| SqlStmt {
                    sql: s.sql.clone(),
                    binds: s.binds.clone(),
                    kind: s.kind,
                    max_rows: s.max_rows,
                })
                .collect(),
            outcome_index: compiled.plan.outcome_index as usize,
            payload_index: compiled.plan.payload_index.map(|i| i as usize),
            consume_once: None,
            receipt_select_index: compiled.plan.receipt_select_index.map(|i| i as usize),
            prior_receipt_index: compiled.plan.prior_receipt_index.map(|i| i as usize),
            expected_hash: Some(compiled.expected_hash.clone()),
        };
        run_plan(conn, &plan)
    }

    #[test]
    fn claim_cas_same_owner_already_running_is_empty() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        seed_domain_event(&conn, "evt-cas", now);
        let id = "evt-cas:echo";
        conn.execute(
            "INSERT INTO event_deliveries (\
                id, event_id, plugin_id, idempotency_key, state, attempt_count, max_attempts, \
                lease_owner, lease_expires_at, lease_generation, run_after, invocation_sequence, \
                resume_pending, checkpoint_json, checkpoint_schema_version, ordering_key, \
                outcome, error_message, created_at, updated_at, cancel_requested, resource_class, \
                wake_event_type, wake_filter_json, wake_grants_json\
             ) VALUES (?, 'evt-cas', 'echo', ?, 'running', 1, 8, 'same-owner', ?, 1, ?, 0, 0, \
                NULL, 0, '', NULL, NULL, ?, '2024-05-01T00:00:00Z', 0, 'network', '', '', '')",
            rusqlite::params![id, id, "2099-01-01T00:00:00Z", now, now],
        )
        .unwrap();
        let compiled = compile_claim_event_delivery(
            "cas-fresh-op",
            id,
            "same-owner",
            60,
            "echo",
            "network",
            1,
            now,
        )
        .unwrap();
        let result = run_compiled(&conn, &compiled);
        assert_eq!(result.status, atomic_status::EMPTY);
    }

    #[test]
    fn claim_hash_includes_lease_and_conflicts_on_change() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        seed_domain_event(&conn, "evt-hash", now);
        seed_pending_delivery(&conn, "evt-hash", "echo", now);
        let id = "evt-hash:echo";
        let first = compile_claim_event_delivery(
            "claim-lease-op",
            id,
            "owner-h",
            60,
            "echo",
            "network",
            1,
            now,
        )
        .unwrap();
        let claimed = run_compiled(&conn, &first);
        assert_eq!(claimed.status, atomic_status::OK);
        let replay_changed = compile_claim_event_delivery(
            "claim-lease-op",
            id,
            "owner-h",
            120,
            "echo",
            "network",
            1,
            now,
        )
        .unwrap();
        let conflict = run_compiled(&conn, &replay_changed);
        assert_eq!(conflict.status, atomic_status::IDEMPOTENCY_CONFLICT);
    }

    #[test]
    fn dispatch_created_is_per_plan_delta() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        seed_domain_event(&conn, "evt-delta", now);
        let two_first = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-two".into(),
                subscribers_json: r#"[{"pluginId":"echo"},{"pluginId":"audiobookshelf"}]"#.into(),
                mark_dispatched: true,
            },
            "disp-two-first",
        );
        seed_domain_event(&conn, "evt-two", now);
        let both_first = run_plan(&conn, &plan_req(&two_first, now).unwrap());
        assert_eq!(both_first.status, atomic_status::OK);
        assert_eq!(both_first.payload.as_ref().unwrap()["created"], 2);

        let one = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-delta".into(),
                subscribers_json: r#"[{"pluginId":"echo"}]"#.into(),
                mark_dispatched: false,
            },
            "disp-one",
        );
        let first = run_plan(&conn, &plan_req(&one, now).unwrap());
        assert_eq!(first.status, atomic_status::OK);
        assert_eq!(first.payload.as_ref().unwrap()["created"], 1);

        let replay_same = run_plan(&conn, &plan_req(&one, now).unwrap());
        assert!(replay_same.replayed);
        assert_eq!(replay_same.payload.as_ref().unwrap()["created"], 1);

        let later = "2024-06-01T00:00:01Z";
        let one_again = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-delta".into(),
                subscribers_json: r#"[{"pluginId":"echo"}]"#.into(),
                mark_dispatched: false,
            },
            "disp-one-replay",
        );
        let second = run_plan(&conn, &plan_req(&one_again, later).unwrap());
        assert_eq!(second.status, atomic_status::OK);
        assert!(!second.replayed);
        assert_eq!(
            second.payload.as_ref().unwrap()["created"],
            0,
            "INSERT OR IGNORE of an existing subscriber is a zero delta"
        );

        let two = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-delta".into(),
                subscribers_json: r#"[{"pluginId":"echo"},{"pluginId":"audiobookshelf"}]"#.into(),
                mark_dispatched: true,
            },
            "disp-two",
        );
        let both = run_plan(&conn, &plan_req(&two, later).unwrap());
        assert_eq!(both.status, atomic_status::OK);
        assert_eq!(
            both.payload.as_ref().unwrap()["created"],
            1,
            "only the new subscriber counts on this page"
        );
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_deliveries WHERE event_id = 'evt-delta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        let state: String = conn
            .query_row(
                "SELECT dispatch_state FROM domain_events WHERE id = 'evt-delta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "dispatched");
    }

    #[test]
    fn dispatch_intermediate_page_leaves_parent_pending() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        seed_domain_event(&conn, "evt-page", now);
        let first_page = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-page".into(),
                subscribers_json: r#"[{"pluginId":"echo"}]"#.into(),
                mark_dispatched: false,
            },
            "page-0",
        );
        let a = run_plan(&conn, &plan_req(&first_page, now).unwrap());
        assert_eq!(a.status, atomic_status::OK);
        let state: String = conn
            .query_row(
                "SELECT dispatch_state FROM domain_events WHERE id = 'evt-page'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "pending");
        let last_page = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-page".into(),
                subscribers_json: r#"[{"pluginId":"audiobookshelf"}]"#.into(),
                mark_dispatched: true,
            },
            "page-1",
        );
        let b = run_plan(&conn, &plan_req(&last_page, now).unwrap());
        assert_eq!(b.status, atomic_status::OK);
        assert_eq!(b.payload.as_ref().unwrap()["created"], 1);
        let state: String = conn
            .query_row(
                "SELECT dispatch_state FROM domain_events WHERE id = 'evt-page'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "dispatched");
    }

    #[test]
    fn dispatch_twenty_five_subscribers_inserts_every_row() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn);
        let now = "2024-06-01T00:00:00Z";
        seed_domain_event(&conn, "evt-25", now);
        let subs: Vec<serde_json::Value> = (0..25)
            .map(|i| json!({ "pluginId": format!("plugin-{i:02}") }))
            .collect();
        let req = test_req(
            DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt-25".into(),
                subscribers_json: serde_json::to_string(&subs).unwrap(),
                mark_dispatched: true,
            },
            "disp-25",
        );
        let result = run_plan(&conn, &plan_req(&req, now).unwrap());
        assert_eq!(result.status, atomic_status::OK);
        assert_eq!(result.payload.as_ref().unwrap()["created"], 25);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_deliveries WHERE event_id = 'evt-25'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 25);
        let state: String = conn
            .query_row(
                "SELECT dispatch_state FROM domain_events WHERE id = 'evt-25'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "dispatched");
    }
}
