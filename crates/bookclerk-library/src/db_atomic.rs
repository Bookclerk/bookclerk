//! Named `dbAtomic` execution for SQLite and PostgreSQL.
//!
//! Runs the same security operations as the D1 HTTP batch planner, but in a
//! native SeaORM transaction, and records an idempotent receipt keyed by
//! `operationId`. Timing is measured around the handler / transaction and is
//! not part of the request hash.

use std::time::Instant;

use bookclerk_plugin_abi::{
    atomic_status, DbAtomicParams, DbAtomicRequest, DbAtomicResult, DbAtomicTiming,
};
use chrono::{DateTime, Duration, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, DatabaseTransaction, DbBackend, EntityTrait,
    QueryFilter, TransactionTrait,
};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::entities::db_atomic_receipts;
use crate::error::{LibraryError, Result};
use crate::models::{PortalIdentity, UserRecord};
use crate::store::LibraryStore;
use crate::SessionClientInfo;

const RECEIPT_TTL_HOURS: i64 = 24;

/// Runs `req` as one native SQL transaction, inserting or replaying a receipt.
///
/// # Errors
///
/// Returns [`LibraryError`] for engine failures. Application outcomes
/// (`lastOwner`, `empty`, …) are returned as [`DbAtomicResult`], not errors.
pub async fn execute_db_atomic(
    db: &DatabaseConnection,
    req: DbAtomicRequest,
) -> Result<DbAtomicResult> {
    let started = Instant::now();
    let timing_source = match db.get_database_backend() {
        DbBackend::Postgres => "postgres_txn",
        _ => "sqlite_txn",
    };
    let txn = db.begin().await.map_err(LibraryError::Orm)?;
    let sql_started = Instant::now();
    let result = execute_in_txn(&txn, &req).await;
    let db_execution_us = u64::try_from(sql_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mut out = match result {
        Ok(value) => {
            txn.commit().await.map_err(LibraryError::Orm)?;
            if let Some(fault) = crate::take_txn_fault() {
                return Err(LibraryError::Orm(sea_orm::DbErr::Custom(fault)));
            }
            value
        }
        Err(err) => {
            let _ = txn.rollback().await;
            let _ = crate::take_txn_fault();
            return Err(err);
        }
    };
    out.operation_id = req.operation_id.clone();
    out.timing = Some(DbAtomicTiming {
        attempt_elapsed_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        db_execution_us: Some(db_execution_us),
        db_timing_source: Some(timing_source.into()),
    });
    Ok(out)
}

pub(crate) async fn delete_user(db: &DatabaseConnection, id: i64) -> Result<()> {
    unit_from_atomic(
        execute_db_atomic(db, request(DbAtomicParams::DeleteUser { user_id: id })).await?,
        format!("user {id}"),
    )
}

pub(crate) async fn set_user_status(
    db: &DatabaseConnection,
    id: i64,
    status: crate::UserStatus,
) -> Result<UserRecord> {
    user_from_atomic(
        execute_db_atomic(
            db,
            request(DbAtomicParams::SetUserStatus {
                user_id: id,
                status: status.as_str().to_string(),
            }),
        )
        .await?,
        format!("user {id}"),
    )
}

pub(crate) async fn set_user_password_hash(
    db: &DatabaseConnection,
    id: i64,
    password_hash: Option<&str>,
) -> Result<UserRecord> {
    user_from_atomic(
        execute_db_atomic(
            db,
            request(DbAtomicParams::SetUserPasswordHash {
                user_id: id,
                password_hash: password_hash.map(str::to_string),
            }),
        )
        .await?,
        format!("user {id}"),
    )
}

pub(crate) async fn set_user_role(
    db: &DatabaseConnection,
    id: i64,
    role: crate::UserRole,
) -> Result<UserRecord> {
    user_from_atomic(
        execute_db_atomic(
            db,
            request(DbAtomicParams::SetUserRole {
                user_id: id,
                role: role.as_str().to_string(),
            }),
        )
        .await?,
        format!("user {id}"),
    )
}

pub(crate) async fn redeem_claim_ticket_to_session(
    db: &DatabaseConnection,
    token_hash: &str,
    session_hash: &str,
    expires_at: DateTime<Utc>,
    client: Option<&SessionClientInfo>,
    new_password_hash: Option<&str>,
    password_fingerprint: Option<&str>,
) -> Result<PortalIdentity> {
    identity_from_atomic(
        execute_db_atomic(
            db,
            request(DbAtomicParams::RedeemClaimTicket {
                token_hash: token_hash.to_string(),
                session_hash: session_hash.to_string(),
                expires_at: expires_at.to_rfc3339(),
                user_agent: client.and_then(|c| c.user_agent.clone()),
                device_type: client.map(|c| c.device_type.clone()),
                client_label: client.map(|c| c.client_label.clone()),
                new_password_hash: new_password_hash.map(str::to_string),
                password_fingerprint: password_fingerprint.map(str::to_string),
            }),
        )
        .await?,
    )
}

pub(crate) async fn take_oidc_rp_state(
    db: &DatabaseConnection,
    state_hash: &str,
) -> Result<Option<(String, String, String, String, Option<i64>)>> {
    let result = execute_db_atomic(
        db,
        request(DbAtomicParams::TakeOidcRpState {
            state_hash: state_hash.to_string(),
        }),
    )
    .await?;
    if result.status == atomic_status::EMPTY {
        return Ok(None);
    }
    if let Some(err) = app_err(&result.status, "oidc rp state".into()) {
        return Err(err);
    }
    let row: AtomicOidcRpState = decode_payload(result.payload, "oidc rp state")?;
    Ok(Some((
        row.provider_id,
        row.pkce_verifier,
        row.nonce,
        row.purpose,
        row.user_id,
    )))
}

pub(crate) async fn take_webauthn_challenge(
    db: &DatabaseConnection,
    challenge_id: &str,
    kind: &str,
) -> Result<Option<(Option<i64>, String)>> {
    let result = execute_db_atomic(
        db,
        request(DbAtomicParams::TakeWebauthnChallenge {
            challenge_id: challenge_id.to_string(),
            kind: kind.to_string(),
        }),
    )
    .await?;
    if result.status == atomic_status::EMPTY {
        return Ok(None);
    }
    if let Some(err) = app_err(&result.status, "webauthn challenge".into()) {
        return Err(err);
    }
    let row: AtomicWebauthnChallenge = decode_payload(result.payload, "webauthn challenge")?;
    Ok(Some((row.user_id, row.state_json)))
}

/// Caller-owned idempotency key for a `dbAtomic` attempt.
///
/// Consume-once keys are derived from the operation so an HTTP/RPC retry
/// resumes the same receipt. Claim redeem includes the session hash; callers
/// must derive that session from the ticket plus browser nonce (see
/// [`crate::derive_claim_session_token`]) so a new HTTP request after a lost
/// reply reuses this id. Other ops mint a UUID for this attempt.
#[must_use]
pub fn db_atomic_operation_id(op: &DbAtomicParams) -> String {
    match op {
        DbAtomicParams::TakeOidcRpState { state_hash } => {
            format!("takeOidcRpState:{state_hash}")
        }
        DbAtomicParams::TakeWebauthnChallenge { challenge_id, kind } => {
            format!("takeWebauthnChallenge:{challenge_id}:{kind}")
        }
        DbAtomicParams::RedeemClaimTicket {
            token_hash,
            session_hash,
            ..
        } => format!("redeemClaimTicket:{token_hash}:{session_hash}"),
        _ => uuid::Uuid::new_v4().to_string(),
    }
}

/// SHA-256 hex digest of the idempotency-relevant fields of `op`.
///
/// Claim redeem omits `expires_at`, client metadata, and the randomized Argon2
/// `new_password_hash`. Idempotency uses `password_fingerprint` (HMAC of the
/// plaintext password) so a retry with a fresh Argon2 salt still matches.
pub fn db_atomic_request_hash(op: &DbAtomicParams) -> Result<String> {
    let bytes = match op {
        DbAtomicParams::RedeemClaimTicket {
            token_hash,
            session_hash,
            password_fingerprint,
            ..
        } => serde_json::to_vec(&serde_json::json!({
            "op": "redeemClaimTicket",
            "token_hash": token_hash,
            "session_hash": session_hash,
            "password_fingerprint": password_fingerprint,
        })),
        other => serde_json::to_vec(other),
    }
    .map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn request(operation: DbAtomicParams) -> DbAtomicRequest {
    DbAtomicRequest {
        operation_id: db_atomic_operation_id(&operation),
        operation,
    }
}

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

async fn execute_in_txn(
    txn: &DatabaseTransaction,
    req: &DbAtomicRequest,
) -> Result<DbAtomicResult> {
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let expires_at = (now + Duration::hours(RECEIPT_TTL_HOURS)).to_rfc3339();
    let hash = db_atomic_request_hash(&req.operation)?;

    db_atomic_receipts::Entity::delete_many()
        .filter(db_atomic_receipts::Column::ExpiresAt.lte(now_str.clone()))
        .filter(db_atomic_receipts::Column::OperationId.ne(req.operation_id.clone()))
        .exec(txn)
        .await
        .map_err(LibraryError::Orm)?;

    if let Some(existing) = db_atomic_receipts::Entity::find_by_id(req.operation_id.clone())
        .one(txn)
        .await
        .map_err(LibraryError::Orm)?
    {
        return Ok(result_from_receipt(&existing, &hash, true));
    }

    let savepoint = txn.begin().await.map_err(LibraryError::Orm)?;
    let outcome = run_operation(&savepoint, &req.operation).await;
    let (status, payload, keep) = match outcome {
        Ok(pair) => (pair.0, pair.1, true),
        Err(err) => match map_app_status(&err) {
            Some(status) => (status.to_string(), None, false),
            None => {
                let _ = savepoint.rollback().await;
                return Err(err);
            }
        },
    };
    if keep {
        savepoint.commit().await.map_err(LibraryError::Orm)?;
    } else {
        let _ = savepoint.rollback().await;
    }

    let payload_text = payload
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
    let receipt = db_atomic_receipts::ActiveModel {
        operation_id: Set(req.operation_id.clone()),
        operation_kind: Set(operation_kind(&req.operation).to_string()),
        request_hash: Set(hash),
        status: Set(status.clone()),
        payload: Set(payload_text),
        created_at: Set(now_str.clone()),
        expires_at: Set(expires_at),
        consume_key: Set(None),
    };
    db_atomic_receipts::Entity::insert(receipt)
        .exec(txn)
        .await
        .map_err(LibraryError::Orm)?;

    let mut result = if status == atomic_status::OK {
        match payload {
            Some(value) => DbAtomicResult::ok(value),
            None => DbAtomicResult::ok_unit(),
        }
    } else {
        DbAtomicResult::with_status(&status)
    };
    result.replayed = false;
    result.receipt_created_at = Some(now_str);
    Ok(result)
}

fn result_from_receipt(
    row: &db_atomic_receipts::Model,
    expected_hash: &str,
    replayed: bool,
) -> DbAtomicResult {
    if row.request_hash != expected_hash {
        return DbAtomicResult::with_status(atomic_status::IDEMPOTENCY_CONFLICT);
    }
    let payload = row
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let mut result = if row.status == atomic_status::OK {
        match payload {
            Some(value) => DbAtomicResult::ok(value),
            None => DbAtomicResult::ok_unit(),
        }
    } else {
        DbAtomicResult::with_status(&row.status)
    };
    result.replayed = replayed;
    result.receipt_created_at = Some(row.created_at.clone());
    result
}

async fn run_operation(
    txn: &DatabaseTransaction,
    op: &DbAtomicParams,
) -> Result<(String, Option<JsonValue>)> {
    match op {
        DbAtomicParams::DeleteUser { user_id } => {
            LibraryStore::delete_user_on(txn, *user_id).await?;
            Ok((atomic_status::OK.to_string(), None))
        }
        DbAtomicParams::SetUserStatus { user_id, status } => {
            let parsed = crate::UserStatus::parse(status).ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!("invalid user status `{status}`"))
            })?;
            let user = LibraryStore::set_user_status_on(txn, *user_id, parsed).await?;
            Ok((atomic_status::OK.to_string(), Some(to_json(&user)?)))
        }
        DbAtomicParams::SetUserPasswordHash {
            user_id,
            password_hash,
        } => {
            let user =
                LibraryStore::set_user_password_hash_on(txn, *user_id, password_hash.as_deref())
                    .await?;
            Ok((atomic_status::OK.to_string(), Some(to_json(&user)?)))
        }
        DbAtomicParams::SetUserRole { user_id, role } => {
            let parsed = crate::UserRole::parse(role).ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!("invalid user role `{role}`"))
            })?;
            let user = LibraryStore::set_user_role_on(txn, *user_id, parsed).await?;
            Ok((atomic_status::OK.to_string(), Some(to_json(&user)?)))
        }
        DbAtomicParams::RedeemClaimTicket {
            token_hash,
            session_hash,
            expires_at,
            user_agent,
            device_type,
            client_label,
            new_password_hash,
            password_fingerprint: _,
        } => {
            let expires = DateTime::parse_from_rfc3339(expires_at)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
            let client = session_client(user_agent, device_type, client_label);
            let identity = LibraryStore::redeem_claim_ticket_to_session_on(
                txn,
                token_hash,
                session_hash,
                expires,
                client.as_ref(),
                new_password_hash.as_deref(),
            )
            .await?;
            Ok((atomic_status::OK.to_string(), Some(to_json(&identity)?)))
        }
        DbAtomicParams::TakeOidcRpState { state_hash } => {
            match LibraryStore::take_oidc_rp_state_on(txn, state_hash).await? {
                Some((provider_id, pkce_verifier, nonce, purpose, user_id)) => Ok((
                    atomic_status::OK.to_string(),
                    Some(serde_json::json!({
                        "provider_id": provider_id,
                        "pkce_verifier": pkce_verifier,
                        "nonce": nonce,
                        "purpose": purpose,
                        "user_id": user_id,
                    })),
                )),
                None => Ok((atomic_status::EMPTY.to_string(), None)),
            }
        }
        DbAtomicParams::TakeWebauthnChallenge { challenge_id, kind } => {
            match LibraryStore::take_webauthn_challenge_on(txn, challenge_id, kind).await? {
                Some((user_id, state_json)) => Ok((
                    atomic_status::OK.to_string(),
                    Some(serde_json::json!({
                        "user_id": user_id,
                        "state_json": state_json,
                    })),
                )),
                None => Ok((atomic_status::EMPTY.to_string(), None)),
            }
        }
    }
}

fn session_client(
    user_agent: &Option<String>,
    device_type: &Option<String>,
    client_label: &Option<String>,
) -> Option<SessionClientInfo> {
    if user_agent.is_none() && device_type.is_none() && client_label.is_none() {
        return None;
    }
    Some(SessionClientInfo {
        user_agent: user_agent.clone(),
        device_type: device_type.clone().unwrap_or_else(|| "unknown".into()),
        client_label: client_label.clone().unwrap_or_else(|| "unknown".into()),
    })
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<JsonValue> {
    serde_json::to_value(value).map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))
}

fn map_app_status(err: &LibraryError) -> Option<&'static str> {
    match err {
        LibraryError::NotFound(_) => Some(atomic_status::NOT_FOUND),
        LibraryError::LastOwner => Some(atomic_status::LAST_OWNER),
        LibraryError::Other(inner) => {
            let text = inner.to_string();
            if text.contains("claim ticket invalid") {
                Some(atomic_status::CLAIM_INVALID)
            } else if text.contains("password required") {
                Some(atomic_status::PASSWORD_REQUIRED)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn app_err(status: &str, not_found: String) -> Option<LibraryError> {
    match status {
        s if s == atomic_status::OK => None,
        s if s == atomic_status::NOT_FOUND => Some(LibraryError::NotFound(not_found)),
        s if s == atomic_status::LAST_OWNER => Some(LibraryError::LastOwner),
        s if s == atomic_status::CLAIM_INVALID => Some(LibraryError::Other(anyhow::anyhow!(
            "claim ticket invalid, expired, or already redeemed"
        ))),
        s if s == atomic_status::PASSWORD_REQUIRED => Some(LibraryError::Other(anyhow::anyhow!(
            "password required — set a password to finish claim login"
        ))),
        s if s == atomic_status::IDEMPOTENCY_CONFLICT => Some(LibraryError::Other(
            anyhow::anyhow!("database atomic operation_id reused with a different request"),
        )),
        other => Some(LibraryError::Other(anyhow::anyhow!(
            "database atomic operation failed: {other}"
        ))),
    }
}

fn unit_from_atomic(result: DbAtomicResult, not_found: String) -> Result<()> {
    if let Some(err) = app_err(&result.status, not_found) {
        return Err(err);
    }
    Ok(())
}

fn user_from_atomic(result: DbAtomicResult, not_found: String) -> Result<UserRecord> {
    if let Some(err) = app_err(&result.status, not_found) {
        return Err(err);
    }
    decode_payload(result.payload, "user")
}

fn identity_from_atomic(result: DbAtomicResult) -> Result<PortalIdentity> {
    if let Some(err) = app_err(&result.status, "claim ticket not found".into()) {
        return Err(err);
    }
    decode_payload(result.payload, "portal identity")
}

fn decode_payload<T: serde::de::DeserializeOwned>(
    payload: Option<JsonValue>,
    what: &str,
) -> Result<T> {
    let value = payload.ok_or_else(|| {
        LibraryError::Other(anyhow::anyhow!(
            "database atomic operation ok without {what} payload"
        ))
    })?;
    serde_json::from_value(value).map_err(|err| {
        LibraryError::Other(anyhow::anyhow!("database atomic {what} payload: {err}"))
    })
}

#[derive(serde::Deserialize)]
struct AtomicOidcRpState {
    provider_id: String,
    pkce_verifier: String,
    nonce: String,
    purpose: String,
    #[serde(default)]
    user_id: Option<i64>,
}

#[derive(serde::Deserialize)]
struct AtomicWebauthnChallenge {
    #[serde(default)]
    user_id: Option<i64>,
    state_json: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserRole;
    use chrono::Duration as ChronoDuration;

    #[tokio::test]
    async fn take_oidc_receipt_replays_after_commit() {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        let store = LibraryStore::from_connection(db.clone());
        let nonce = ["n", "once"].concat();
        store
            .insert_oidc_rp_state(
                "abc",
                "corp",
                "verifier",
                &nonce,
                "login",
                None,
                Utc::now() + ChronoDuration::minutes(5),
            )
            .await
            .unwrap();
        let req = DbAtomicRequest {
            operation_id: "op-take-1".into(),
            operation: DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
        };
        let first = execute_db_atomic(&db, req.clone()).await.unwrap();
        assert_eq!(first.status, atomic_status::OK);
        assert!(!first.replayed);
        assert_eq!(first.payload.as_ref().unwrap()["pkce_verifier"], "verifier");
        assert!(store.take_oidc_rp_state("abc").await.unwrap().is_none());

        let second = execute_db_atomic(&db, req).await.unwrap();
        assert_eq!(second.status, atomic_status::OK);
        assert!(second.replayed);
        assert_eq!(second.payload, first.payload);

        let conflict = execute_db_atomic(
            &db,
            DbAtomicRequest {
                operation_id: "op-take-1".into(),
                operation: DbAtomicParams::TakeOidcRpState {
                    state_hash: "other".into(),
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(conflict.status, atomic_status::IDEMPOTENCY_CONFLICT);
        let _ = UserRole::Owner;
    }

    #[test]
    fn consume_once_and_redeem_ids_are_stable() {
        let oidc = DbAtomicParams::TakeOidcRpState {
            state_hash: "abc".into(),
        };
        assert_eq!(db_atomic_operation_id(&oidc), db_atomic_operation_id(&oidc));
        assert_eq!(db_atomic_operation_id(&oidc), "takeOidcRpState:abc");

        let webauthn = DbAtomicParams::TakeWebauthnChallenge {
            challenge_id: "chal".into(),
            kind: "login".into(),
        };
        assert_eq!(
            db_atomic_operation_id(&webauthn),
            "takeWebauthnChallenge:chal:login"
        );

        let redeem = DbAtomicParams::RedeemClaimTicket {
            token_hash: "ticket".into(),
            session_hash: "session".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            user_agent: None,
            device_type: None,
            client_label: None,
            new_password_hash: None,
            password_fingerprint: None,
        };
        assert_eq!(
            db_atomic_operation_id(&redeem),
            "redeemClaimTicket:ticket:session"
        );

        let delete = DbAtomicParams::DeleteUser { user_id: 1 };
        assert_ne!(
            db_atomic_operation_id(&delete),
            db_atomic_operation_id(&delete)
        );
    }

    #[test]
    fn redeem_request_hash_ignores_expires_and_client_metadata() {
        let a = DbAtomicParams::RedeemClaimTicket {
            token_hash: "ticket".into(),
            session_hash: "session".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            user_agent: Some("Mozilla/5.0".into()),
            device_type: None,
            client_label: None,
            new_password_hash: None,
            password_fingerprint: None,
        };
        let b = DbAtomicParams::RedeemClaimTicket {
            token_hash: "ticket".into(),
            session_hash: "session".into(),
            expires_at: "2099-01-01T00:00:01Z".into(),
            user_agent: Some("retry".into()),
            device_type: Some("desktop".into()),
            client_label: Some("retry".into()),
            new_password_hash: None,
            password_fingerprint: None,
        };
        assert_eq!(
            db_atomic_request_hash(&a).unwrap(),
            db_atomic_request_hash(&b).unwrap()
        );
        let other_session = DbAtomicParams::RedeemClaimTicket {
            token_hash: "ticket".into(),
            session_hash: "other".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            user_agent: None,
            device_type: None,
            client_label: None,
            new_password_hash: None,
            password_fingerprint: None,
        };
        assert_ne!(
            db_atomic_request_hash(&a).unwrap(),
            db_atomic_request_hash(&other_session).unwrap()
        );
    }

    #[test]
    fn redeem_request_hash_ignores_argon2_salt_and_binds_fingerprint() {
        let a = DbAtomicParams::RedeemClaimTicket {
            token_hash: "ticket".into(),
            session_hash: "session".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            user_agent: None,
            device_type: None,
            client_label: None,
            new_password_hash: Some("argon2-salt-a".into()),
            password_fingerprint: Some("fp-one".into()),
        };
        let b = DbAtomicParams::RedeemClaimTicket {
            token_hash: "ticket".into(),
            session_hash: "session".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            user_agent: None,
            device_type: None,
            client_label: None,
            new_password_hash: Some("argon2-salt-b".into()),
            password_fingerprint: Some("fp-one".into()),
        };
        assert_eq!(
            db_atomic_request_hash(&a).unwrap(),
            db_atomic_request_hash(&b).unwrap()
        );
        let other_fp = DbAtomicParams::RedeemClaimTicket {
            token_hash: "ticket".into(),
            session_hash: "session".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            user_agent: None,
            device_type: None,
            client_label: None,
            new_password_hash: Some("argon2-salt-a".into()),
            password_fingerprint: Some("fp-two".into()),
        };
        assert_ne!(
            db_atomic_request_hash(&a).unwrap(),
            db_atomic_request_hash(&other_fp).unwrap()
        );
    }
}
