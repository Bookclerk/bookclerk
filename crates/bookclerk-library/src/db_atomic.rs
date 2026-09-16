//! Named atomic execution for SQLite and PostgreSQL.
//!
//! Compiles host-owned SQL plans and runs them as one native transaction
//! with a durable `operationId` receipt. Timing is measured around the
//! transaction and is not part of the request hash.

use crate::atomic_ops::{atomic_status, DbAtomicParams, DbAtomicResult};
use crate::sql_plan::DbAtomicRequest;
use chrono::{DateTime, Utc};
use sea_orm::{DatabaseConnection, DbBackend};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::error::{LibraryError, Result};
use crate::models::{PortalIdentity, UserRecord};
use crate::secrets::EncryptedSecretRecord;
use crate::{bytes_to_b64_string, SessionClientInfo};

/// Runs a host-authored generic plan as one native SQL transaction.
///
/// # Errors
///
/// Returns [`LibraryError`] for engine failures. Application outcomes
/// (`lastOwner`, `empty`, …) are returned as [`DbAtomicResult`], not errors.
pub async fn execute_db_atomic(
    db: &DatabaseConnection,
    req: DbAtomicRequest,
) -> Result<DbAtomicResult> {
    let timing_source = match db.get_database_backend() {
        DbBackend::Postgres => "postgres_txn",
        _ => "sqlite_txn",
    };
    let plan = req.plan.as_ref().ok_or_else(|| {
        LibraryError::Other(anyhow::anyhow!(
            "atomic execute requires a host-authored executePlan"
        ))
    })?;
    let hash = req.request_hash.as_deref().unwrap_or("");
    crate::sql_plan::execute_plan_on(db, plan, hash, &req.operation_id, timing_source).await
}

/// Compiles `params` and runs the plan as one native SQL transaction.
///
/// # Errors
///
/// Returns [`LibraryError`] for engine failures or invalid named operations.
pub async fn execute_named_atomic(
    db: &DatabaseConnection,
    operation_id: &str,
    params: &DbAtomicParams,
) -> Result<DbAtomicResult> {
    let now = Utc::now().to_rfc3339();
    let compiled = crate::sql_plan::compile_named_request(operation_id, params, &now)
        .map_err(LibraryError::Orm)?;
    execute_db_atomic(db, compiled.into_request(operation_id)).await
}

/// Compiles a named op with a stable consume-once / library operation id.
async fn run_named(db: &DatabaseConnection, operation: DbAtomicParams) -> Result<DbAtomicResult> {
    let operation_id = db_atomic_operation_id(&operation);
    execute_named_atomic(db, &operation_id, &operation).await
}

/// Deletes a user in one native atomic transaction (fails closed on last owner).
pub(crate) async fn delete_user(db: &DatabaseConnection, id: i64) -> Result<()> {
    unit_from_atomic(
        run_named(db, DbAtomicParams::DeleteUser { user_id: id }).await?,
        format!("user {id}"),
    )
}

/// Sets a user's status (`active`/`disabled`) inside a atomic transaction.
pub(crate) async fn set_user_status(
    db: &DatabaseConnection,
    id: i64,
    status: crate::UserStatus,
) -> Result<UserRecord> {
    user_from_atomic(
        run_named(
            db,
            DbAtomicParams::SetUserStatus {
                user_id: id,
                status: status.as_str().to_string(),
            },
        )
        .await?,
        format!("user {id}"),
    )
}

/// Replaces or clears a user's Argon2 password hash inside a atomic transaction.
pub(crate) async fn set_user_password_hash(
    db: &DatabaseConnection,
    id: i64,
    password_hash: Option<&str>,
) -> Result<UserRecord> {
    user_from_atomic(
        run_named(
            db,
            DbAtomicParams::SetUserPasswordHash {
                user_id: id,
                password_hash: password_hash.map(str::to_string),
            },
        )
        .await?,
        format!("user {id}"),
    )
}

/// Changes a user's portal role inside a atomic transaction (fails closed on last owner).
pub(crate) async fn set_user_role(
    db: &DatabaseConnection,
    id: i64,
    role: crate::UserRole,
) -> Result<UserRecord> {
    user_from_atomic(
        run_named(
            db,
            DbAtomicParams::SetUserRole {
                user_id: id,
                role: role.as_str().to_string(),
            },
        )
        .await?,
        format!("user {id}"),
    )
}

/// Consumes a claim ticket and mints a portal session in one atomic transaction.
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
        run_named(
            db,
            DbAtomicParams::RedeemClaimTicket {
                token_hash: token_hash.to_string(),
                session_hash: session_hash.to_string(),
                expires_at: expires_at.to_rfc3339(),
                user_agent: client.and_then(|c| c.user_agent.clone()),
                device_type: client.map(|c| c.device_type.clone()),
                client_label: client.map(|c| c.client_label.clone()),
                new_password_hash: new_password_hash.map(str::to_string),
                password_fingerprint: password_fingerprint.map(str::to_string),
            },
        )
        .await?,
    )
}

/// Consumes one-time OIDC RP state (PKCE verifier, nonce) keyed by hashed `state`.
pub(crate) async fn take_oidc_rp_state(
    db: &DatabaseConnection,
    state_hash: &str,
) -> Result<Option<(String, String, String, String, Option<i64>)>> {
    let result = run_named(
        db,
        DbAtomicParams::TakeOidcRpState {
            state_hash: state_hash.to_string(),
        },
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

/// Promotes a sealed TOTP secret to `primary` and sets `totp_enabled` in one atomic transaction.
pub(crate) async fn confirm_totp_enrollment(
    db: &DatabaseConnection,
    user_id: i64,
    record: &EncryptedSecretRecord,
) -> Result<()> {
    unit_from_atomic(
        run_named(db, confirm_totp_params(user_id, record)).await?,
        "user".into(),
    )
}

/// Deletes TOTP secrets and clears `totp_enabled` in one atomic transaction.
pub(crate) async fn disable_user_totp(db: &DatabaseConnection, user_id: i64) -> Result<()> {
    unit_from_atomic(
        run_named(db, DbAtomicParams::DisableUserTotp { user_id }).await?,
        "user".into(),
    )
}

/// Consumes a one-time WebAuthn challenge row for login or registration.
pub(crate) async fn take_webauthn_challenge(
    db: &DatabaseConnection,
    challenge_id: &str,
    kind: &str,
) -> Result<Option<(Option<i64>, String)>> {
    let result = run_named(
        db,
        DbAtomicParams::TakeWebauthnChallenge {
            challenge_id: challenge_id.to_string(),
            kind: kind.to_string(),
        },
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

/// Caller-owned idempotency key for an atomic attempt.
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
///
/// # Errors
///
/// Returns an error when the operation fails.
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

/// Wire params for confirming TOTP with an already-sealed primary secret.
fn confirm_totp_params(user_id: i64, record: &EncryptedSecretRecord) -> DbAtomicParams {
    DbAtomicParams::ConfirmTotpEnrollment {
        user_id,
        format: record.format.clone(),
        ciphertext: bytes_to_b64_string(&record.ciphertext),
        cipher_algorithm: record.cipher_algorithm.clone(),
        cipher_nonce: record.cipher_nonce.as_deref().map(bytes_to_b64_string),
        kdf_algorithm: record.kdf_algorithm.clone(),
        kdf_salt: record.kdf_salt.as_deref().map(bytes_to_b64_string),
        kdf_m_cost: record.kdf_m_cost.map(i64::from),
        kdf_t_cost: record.kdf_t_cost.map(i64::from),
        kdf_p_cost: record.kdf_p_cost.map(i64::from),
        created_at: record.created_at.clone(),
    }
}

/// Turns an atomic status back into `LibraryError` for crate-internal callers.
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

/// Accepts an OK unit result or maps a non-OK status onto `LibraryError`.
fn unit_from_atomic(result: DbAtomicResult, not_found: String) -> Result<()> {
    if let Some(err) = app_err(&result.status, not_found) {
        return Err(err);
    }
    Ok(())
}

/// Decodes a user record from an OK payload, or maps status onto `LibraryError`.
fn user_from_atomic(result: DbAtomicResult, not_found: String) -> Result<UserRecord> {
    if let Some(err) = app_err(&result.status, not_found) {
        return Err(err);
    }
    decode_payload(result.payload, "user")
}

/// Decodes a portal identity from an OK payload, or maps status onto `LibraryError`.
fn identity_from_atomic(result: DbAtomicResult) -> Result<PortalIdentity> {
    if let Some(err) = app_err(&result.status, "claim ticket not found".into()) {
        return Err(err);
    }
    decode_payload(result.payload, "portal identity")
}

/// Deserializes an OK payload; missing or malformed JSON fails closed.
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
/// Payload of a consumed OIDC RP state row (one-time; hashed `state` is the key).
struct AtomicOidcRpState {
    /// IdP id that started this authorize.
    provider_id: String,
    /// PKCE verifier needed for the token exchange (never logged).
    pkce_verifier: String,
    /// Nonce expected in the id_token.
    nonce: String,
    /// `login` or `elevate`.
    purpose: String,
    #[serde(default)]
    /// Owner user id when purpose is elevate; absent for login.
    user_id: Option<i64>,
}

#[derive(serde::Deserialize)]
/// Payload of a consumed WebAuthn challenge (one-time).
struct AtomicWebauthnChallenge {
    #[serde(default)]
    /// User id for registration; absent for discoverable login.
    user_id: Option<i64>,
    /// Serialized webauthn-rs challenge state needed to finish the ceremony.
    state_json: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LibraryStore, UserRole};
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
        let first = execute_named_atomic(
            &db,
            "op-take-1",
            &DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(first.status, atomic_status::OK);
        assert!(!first.replayed);
        assert_eq!(first.payload.as_ref().unwrap()["pkce_verifier"], "verifier");
        assert!(store.take_oidc_rp_state("abc").await.unwrap().is_none());

        let second = execute_named_atomic(
            &db,
            "op-take-1",
            &DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(second.status, atomic_status::OK);
        assert!(second.replayed);
        assert_eq!(second.payload, first.payload);

        let conflict = execute_named_atomic(
            &db,
            "op-take-1",
            &DbAtomicParams::TakeOidcRpState {
                state_hash: "other".into(),
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
