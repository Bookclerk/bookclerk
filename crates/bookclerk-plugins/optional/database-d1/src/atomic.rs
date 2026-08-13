//! Named atomic library operations for Cloudflare D1 HTTP `batch()`.
//!
//! Each [`DbAtomicParams`] variant is compiled to a list of SQL statements
//! that D1 runs as **one** SQL transaction. Control flow lives in `WHERE`
//! clauses (and a status `SELECT`) so the guest never needs mid-transaction
//! round-trips. A statement failure aborts the HTTP batch and rolls back.

use bookclerk_plugin_sdk::{atomic_status, DbAtomicParams, DbAtomicResult};
use sea_orm::DbErr;
use serde_json::{json, Value as JsonValue};

use super::d1::D1Proxy;

/// One statement in a D1 HTTP batch body.
pub(crate) type SqlStmt = (String, Vec<JsonValue>);

/// Planned batch plus the index of the application-status `SELECT`.
struct AtomicPlan {
    statements: Vec<SqlStmt>,
    outcome_index: usize,
    payload_index: Option<usize>,
}

impl D1Proxy {
    /// Runs a named library operation as one D1 HTTP batch (one SQL transaction).
    pub async fn run_atomic(
        &self,
        op: DbAtomicParams,
    ) -> std::result::Result<DbAtomicResult, DbErr> {
        let now = chrono::Utc::now().to_rfc3339();
        let plan = plan_atomic(&op, &now);
        let raw = self.run_batch(&plan.statements).await?;
        let results = parse_batch_results(&raw)?;
        Ok(interpret_atomic(&plan, &results))
    }
}

/// Builds the D1 batch for `op`. `now` is the RFC 3339 timestamp shared by
/// every statement in the batch (consume correlation, `updated_at`, sessions).
fn plan_atomic(op: &DbAtomicParams, now: &str) -> AtomicPlan {
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
    }
}

fn sql(text: &str, params: Vec<JsonValue>) -> SqlStmt {
    (text.to_string(), params)
}

fn j_i64(n: i64) -> JsonValue {
    JsonValue::from(n)
}

fn j_str(s: &str) -> JsonValue {
    JsonValue::String(s.to_string())
}

fn j_opt_str(s: Option<&str>) -> JsonValue {
    match s {
        Some(v) => JsonValue::String(v.to_string()),
        None => JsonValue::Null,
    }
}

/// Last active owner predicate. Binds `user_id` twice.
fn last_owner_sql() -> &'static str {
    "((SELECT role FROM users WHERE id = ?) = 'owner' \
      AND (SELECT status FROM users WHERE id = ?) = 'active' \
      AND (SELECT COUNT(*) FROM users WHERE role = 'owner' AND status = 'active') <= 1)"
}

fn last_owner_params(user_id: i64) -> Vec<JsonValue> {
    vec![j_i64(user_id), j_i64(user_id)]
}

fn allow_mutate_sql() -> String {
    format!(
        "EXISTS (SELECT 1 FROM users WHERE id = ?) AND NOT {}",
        last_owner_sql()
    )
}

fn allow_mutate_params(user_id: i64) -> Vec<JsonValue> {
    let mut p = vec![j_i64(user_id)];
    p.extend(last_owner_params(user_id));
    p
}

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

    for table in ["oidc_refresh_tokens", "oidc_auth_codes"] {
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
    }
}

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
    }
}

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
    }
}

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
    }
}

#[allow(clippy::too_many_arguments)]
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
    }
}

#[derive(Debug, Clone)]
struct BatchStmtResult {
    rows: Vec<JsonValue>,
}

fn parse_batch_results(value: &JsonValue) -> std::result::Result<Vec<BatchStmtResult>, DbErr> {
    let Some(arr) = value.get("result").and_then(JsonValue::as_array) else {
        return Err(DbErr::Custom(
            "D1 batch response missing result array".into(),
        ));
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

fn interpret_atomic(plan: &AtomicPlan, results: &[BatchStmtResult]) -> DbAtomicResult {
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
