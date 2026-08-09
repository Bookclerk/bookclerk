//! S3 destination credentials in `encrypted_secrets`.
//!
//! Payload JSON is stored as `kind = "s3"`, `provider = "s3"`,
//! `account_type = "operator"`, `account_id = "default"`, `name = "default"`,
//! using `sealed-v1` (process DEK via `master.key`). Legacy `json-encrypted`
//! rows are still readable. Legacy `json` plaintext rows are rejected.
//!
//! Operator secrets are isolated from integration (store-account) secrets by
//! `account_type`. `delete_secrets_for_account` only touches
//! `account_type = "integration"` rows, so S3 credentials survive any store
//! account revocation.
//!
//! Credential resolution when building the S3 backend (see [`crate::s3`]):
//! 1. `BOOKCLERK_AWS_ACCESS_KEY_ID` + `BOOKCLERK_AWS_SECRET_ACCESS_KEY` env override
//!    (wins when both are set; empty string counts as set)
//! 2. This `encrypted_secrets` row (sealed-v1 or migrated)
//! 3. AWS SDK default provider chain

use bookclerk_library::{
    build_sealed_record, delete_secret, secret_account_type, secret_kind, unseal_secret,
    upsert_secret,
};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

use crate::error::{Result, StorageError};

/// Canonical secret name for the default S3 destination credentials.
pub const S3_SECRET_NAME: &str = "default";

/// Canonical account_id for the default S3 destination credentials.
///
/// S3 credentials belong to the operator, not to any store account. The
/// `account_type` column (`"operator"`) provides the primary isolation;
/// `account_id` is `"default"` to identify the default credential set.
pub const S3_SECRET_ACCOUNT_ID: &str = "default";

/// Bookclerk-namespaced env vars for S3 credential override.
///
/// These win over the DB row. Empty string counts as set (intentional override).
/// Do NOT treat bare `AWS_*` as Bookclerk override — the SDK chain may still
/// use those later.
pub const ENV_AWS_ACCESS_KEY_ID: &str = "BOOKCLERK_AWS_ACCESS_KEY_ID";
pub const ENV_AWS_SECRET_ACCESS_KEY: &str = "BOOKCLERK_AWS_SECRET_ACCESS_KEY";
pub const ENV_AWS_SESSION_TOKEN: &str = "BOOKCLERK_AWS_SESSION_TOKEN";

/// Static AWS-style credentials for the S3 destination.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct S3Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &redact_access_key(&self.access_key_id))
            .field("secret_access_key", &"***")
            .field("session_token", &self.session_token.as_ref().map(|_| "***"))
            .field("label", &self.label)
            .finish()
    }
}

fn redact_access_key(access_key_id: &str) -> String {
    if access_key_id.len() <= 4 {
        return "****".into();
    }
    let (prefix, rest) = access_key_id.split_at(4);
    format!("{prefix}{}", "*".repeat(rest.len().min(8)))
}

fn map_lib(err: bookclerk_library::LibraryError) -> StorageError {
    StorageError::S3(format!("encrypted_secrets: {err}"))
}

/// Persist S3 credentials into `encrypted_secrets` using sealed-v1.
pub async fn save_s3_credentials(db: &DatabaseConnection, creds: &S3Credentials) -> Result<()> {
    if creds.access_key_id.trim().is_empty() || creds.secret_access_key.trim().is_empty() {
        return Err(StorageError::S3(
            "S3 access_key_id and secret_access_key must not be empty".into(),
        ));
    }
    bookclerk_config::register_secret(&creds.access_key_id);
    bookclerk_config::register_secret(&creds.secret_access_key);
    if let Some(token) = &creds.session_token {
        bookclerk_config::register_secret(token);
    }

    let json = serde_json::to_vec(creds)
        .map_err(|e| StorageError::S3(format!("failed to serialize S3 credentials: {e}")))?;

    let record = build_sealed_record(
        &json,
        secret_kind::S3,
        "s3",
        secret_account_type::OPERATOR,
        S3_SECRET_ACCOUNT_ID,
        S3_SECRET_NAME,
    )
    .map_err(map_lib)?;

    upsert_secret(db, &record).await.map_err(map_lib)?;
    Ok(())
}

/// Load S3 credentials from `encrypted_secrets`, if present.
///
/// Fails closed when the record exists but cannot be unsealed (wrong master key
/// or corrupted data) — callers must not fall through to the AWS SDK chain in
/// that case.
///
/// Returns `None` only when no DB row exists at all.
pub async fn load_s3_credentials(db: &DatabaseConnection) -> Result<Option<S3Credentials>> {
    let store = bookclerk_library::SecretStore::new(db);
    let Some(record) = store
        .get(
            secret_kind::S3,
            Some("s3"),
            secret_account_type::OPERATOR,
            Some(S3_SECRET_ACCOUNT_ID),
            S3_SECRET_NAME,
        )
        .await
        .map_err(map_lib)?
    else {
        return Ok(None);
    };

    let json = unseal_secret(&record).map_err(|e| {
        StorageError::S3(format!(
            "S3 credentials in encrypted_secrets could not be unsealed (fail closed): {e}"
        ))
    })?;

    let creds: S3Credentials = serde_json::from_slice(&json)
        .map_err(|e| StorageError::S3(format!("invalid S3 credentials JSON: {e}")))?;
    bookclerk_config::register_secret(&creds.access_key_id);
    bookclerk_config::register_secret(&creds.secret_access_key);
    if let Some(token) = &creds.session_token {
        bookclerk_config::register_secret(token);
    }
    Ok(Some(creds))
}

/// Delete stored S3 credentials from `encrypted_secrets`.
pub async fn delete_s3_credentials(db: &DatabaseConnection) -> Result<()> {
    delete_secret(
        db,
        secret_kind::S3,
        Some("s3"),
        secret_account_type::OPERATOR,
        Some(S3_SECRET_ACCOUNT_ID),
        S3_SECRET_NAME,
    )
    .await
    .map_err(map_lib)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{configure_master_key, list_secrets};
    use tempfile::tempdir;

    fn setup_dek() {
        let dir = tempdir().unwrap();
        std::env::remove_var(bookclerk_library::MASTER_KEY_AUTH_PASSWORD_ENV);
        configure_master_key(dir.path()).unwrap();
    }

    #[tokio::test]
    async fn roundtrip_sealed_v1() {
        setup_dek();
        let db = bookclerk_plugin_database::sqlite::open_memory()
            .await
            .unwrap();
        let creds = S3Credentials {
            access_key_id: "AKIAEXAMPLE".into(),
            secret_access_key: "secret".into(),
            session_token: Some("tok".into()),
            label: Some("minio".into()),
        };
        save_s3_credentials(&db, &creds).await.unwrap();
        let loaded = load_s3_credentials(&db).await.unwrap().unwrap();
        assert_eq!(loaded, creds);

        // Rotation replaces the same composite key.
        let rotated = S3Credentials {
            access_key_id: "AKIAROTATED".into(),
            secret_access_key: "new-secret".into(),
            session_token: None,
            label: Some("minio".into()),
        };
        save_s3_credentials(&db, &rotated).await.unwrap();
        let all = list_secrets(&db, secret_kind::S3).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(load_s3_credentials(&db).await.unwrap().unwrap(), rotated);

        delete_s3_credentials(&db).await.unwrap();
        assert!(load_s3_credentials(&db).await.unwrap().is_none());
    }
}
