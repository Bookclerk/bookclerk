//! Durable operator API token in `encrypted_secrets`.
//!
//! Resolution for daemon/CLI (when `[daemon.auth].enabled`):
//! 1. `BOOKCLERK_OPERATOR_TOKEN` env override (not written to DB)
//! 2. This sealed row
//! 3. One-time import from a legacy `operator.token` file (then delete the file)
//! 4. Mint a new token and seal it here
//!
//! Payload is UTF-8 plaintext of the token string (sealed-v1 via process DEK).

use std::path::{Path, PathBuf};

use bookclerk_config::{generate_operator_token, register_secret, validate_operator_token, Config};
use sea_orm::DatabaseConnection;
use tracing::{info, warn};

use crate::error::{LibraryError, Result};
use crate::secrets::{
    build_sealed_record, secret_account_type, secret_kind, unseal_secret, upsert_secret,
    SecretStore,
};

/// Wraps a message as [`LibraryError::Other`] for token resolve/seal failures.
fn err(msg: impl Into<String>) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!(msg.into()))
}

/// Canonical secret name for the operator API token.
pub const OPERATOR_TOKEN_SECRET_NAME: &str = "default";

/// Canonical `account_id` for the operator API token row.
pub const OPERATOR_TOKEN_ACCOUNT_ID: &str = "default";

/// How an operator token was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOperatorToken {
    /// Value from `BOOKCLERK_OPERATOR_TOKEN`.
    Env,
    /// Value from `encrypted_secrets`.
    Database,
    /// Imported from a legacy on-disk token file into the database.
    LegacyFile,
    /// Freshly minted and stored in the database.
    Generated,
}

/// Read `BOOKCLERK_OPERATOR_TOKEN` when set and non-empty.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn env_operator_token() -> Result<Option<String>> {
    match std::env::var("BOOKCLERK_OPERATOR_TOKEN") {
        Ok(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                let token = validate_operator_token(trimmed, "BOOKCLERK_OPERATOR_TOKEN")
                    .map_err(|e| err(e.to_string()))?;
                register_secret(&token);
                Ok(Some(token))
            }
        }
        Err(_) => Ok(None),
    }
}

/// Load the operator token from `encrypted_secrets`, if present.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn load_operator_token(db: &DatabaseConnection) -> Result<Option<String>> {
    let store = SecretStore::new(db);
    let Some(record) = store
        .get(
            secret_kind::OPERATOR_TOKEN,
            Some("daemon"),
            secret_account_type::OPERATOR,
            Some(OPERATOR_TOKEN_ACCOUNT_ID),
            OPERATOR_TOKEN_SECRET_NAME,
        )
        .await?
    else {
        return Ok(None);
    };

    let bytes = unseal_secret(&record).map_err(|e| {
        err(format!(
            "operator token in encrypted_secrets could not be unsealed: {e}"
        ))
    })?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("operator token is not valid UTF-8: {e}")))?;
    let token = validate_operator_token(raw.trim(), "encrypted_secrets operator_token")
        .map_err(|e| err(e.to_string()))?;
    register_secret(&token);
    Ok(Some(token))
}

/// Persist an operator token into `encrypted_secrets` (sealed-v1).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn save_operator_token(db: &DatabaseConnection, token: &str) -> Result<()> {
    let token = validate_operator_token(token, "operator token").map_err(|e| err(e.to_string()))?;
    register_secret(&token);
    let record = build_sealed_record(
        token.as_bytes(),
        secret_kind::OPERATOR_TOKEN,
        "daemon",
        secret_account_type::OPERATOR,
        OPERATOR_TOKEN_ACCOUNT_ID,
        OPERATOR_TOKEN_SECRET_NAME,
    )?;
    upsert_secret(db, &record).await?;
    Ok(())
}

/// Mint a new operator token, store it, and return it.
///
/// Refuses when `BOOKCLERK_OPERATOR_TOKEN` is set (env override would still win).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn rotate_operator_token(db: &DatabaseConnection) -> Result<String> {
    if env_operator_token()?.is_some() {
        return Err(err(
            "cannot rotate operator token while BOOKCLERK_OPERATOR_TOKEN is set; \
             unset the env override first",
        ));
    }
    let token = generate_operator_token().map_err(|e| err(e.to_string()))?;
    save_operator_token(db, &token).await?;
    Ok(token)
}

/// Resolve the effective operator token for an enabled daemon auth config.
///
/// When `create` is true, mints and stores a token if none exists (daemon startup).
/// When `create` is false, returns `Ok(None)` if no env/DB/legacy source provides one.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn resolve_operator_token(
    config: &Config,
    db: &DatabaseConnection,
    create: bool,
) -> Result<Option<(String, ResolveOperatorToken)>> {
    if let Some(token) = env_operator_token()? {
        return Ok(Some((token, ResolveOperatorToken::Env)));
    }

    if let Some(token) = load_operator_token(db).await? {
        return Ok(Some((token, ResolveOperatorToken::Database)));
    }

    if let Some(token) = migrate_legacy_token_file(config, db).await? {
        return Ok(Some((token, ResolveOperatorToken::LegacyFile)));
    }

    if !create {
        return Ok(None);
    }

    let token = generate_operator_token().map_err(|e| err(e.to_string()))?;
    save_operator_token(db, &token).await?;
    info!("generated operator API token (stored in encrypted_secrets)");
    Ok(Some((token, ResolveOperatorToken::Generated)))
}

/// Read-or-create for daemon startup when auth is enabled.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn read_or_create_operator_token(
    config: &Config,
    db: &DatabaseConnection,
) -> Result<(String, ResolveOperatorToken)> {
    match resolve_operator_token(config, db, true).await? {
        Some(pair) => Ok(pair),
        None => Err(err("operator token resolution returned empty after create")),
    }
}

/// Imports `operator.token` into sealed `encrypted_secrets` and deletes or renames the leftover file.
async fn migrate_legacy_token_file(
    config: &Config,
    db: &DatabaseConnection,
) -> Result<Option<String>> {
    let path = legacy_operator_token_path(config);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        err(format!(
            "failed to read legacy operator token file {}: {e}",
            path.display()
        ))
    })?;
    let token = validate_operator_token(
        raw.trim(),
        &format!("legacy operator token file {}", path.display()),
    )
    .map_err(|e| err(e.to_string()))?;
    save_operator_token(db, &token).await?;
    match std::fs::remove_file(&path) {
        Ok(()) => info!(
            path = %path.display(),
            "migrated legacy operator.token into encrypted_secrets and deleted the file"
        ),
        Err(e) => warn!(
            path = %path.display(),
            error = %e,
            "migrated legacy operator.token into encrypted_secrets but failed to delete the file; \
             it will be ignored on subsequent startups"
        ),
    }
    // Rename marker so a failed delete does not keep re-importing a leftover file.
    if path.is_file() {
        let leftover = path.with_extension("token.migrated");
        let _ = std::fs::rename(&path, &leftover);
    }
    Ok(Some(token))
}

/// Historical path used before DB-backed tokens (`daemon.auth.token_file` default).
fn legacy_operator_token_path(config: &Config) -> PathBuf {
    config.paths().files_dir.join("operator.token")
}

/// Best-effort check whether a path looks like a leftover token file (tests/docs).
pub fn legacy_operator_token_file(files_dir: &Path) -> PathBuf {
    files_dir.join("operator.token")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master_key::{ensure_shared_test_dek, master_key_test_read_lock_async};
    use bookclerk_config::{secrets_registry_test_lock, Paths};
    use tempfile::tempdir;

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Shared process DEK for sealed-v1 operator-token tests (held for the full test).
    async fn setup_dek() -> tokio::sync::RwLockReadGuard<'static, ()> {
        let guard = master_key_test_read_lock_async().await;
        ensure_shared_test_dek();
        guard
    }

    async fn test_db() -> DatabaseConnection {
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("sqlite memory")
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn mints_and_reloads_from_db() {
        let _registry = secrets_registry_test_lock();
        let _env = ENV_LOCK.lock().await;
        let _dek = setup_dek().await;
        let prev = std::env::var_os("BOOKCLERK_OPERATOR_TOKEN");
        std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN");

        let dir = tempdir().unwrap();
        let db = test_db().await;
        let cfg = Config {
            paths: Some(Paths::from_files_dir(dir.path().to_path_buf())),
            ..Config::default()
        };

        let (token, how) = read_or_create_operator_token(&cfg, &db).await.unwrap();
        assert_eq!(how, ResolveOperatorToken::Generated);
        assert_eq!(token.len(), 64);

        let (again, how2) = read_or_create_operator_token(&cfg, &db).await.unwrap();
        assert_eq!(how2, ResolveOperatorToken::Database);
        assert_eq!(again, token);

        match prev {
            Some(v) => std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", v),
            None => std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN"),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn env_overrides_db() {
        let _registry = secrets_registry_test_lock();
        let _env = ENV_LOCK.lock().await;
        let _dek = setup_dek().await;
        let prev = std::env::var_os("BOOKCLERK_OPERATOR_TOKEN");

        let dir = tempdir().unwrap();
        let db = test_db().await;
        let cfg = Config {
            paths: Some(Paths::from_files_dir(dir.path().to_path_buf())),
            ..Config::default()
        };
        let (db_token, _) = read_or_create_operator_token(&cfg, &db).await.unwrap();

        std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", "env-override-token-value-001");
        let (token, how) = read_or_create_operator_token(&cfg, &db).await.unwrap();
        assert_eq!(how, ResolveOperatorToken::Env);
        assert_eq!(token, "env-override-token-value-001");
        assert_ne!(token, db_token);

        match prev {
            Some(v) => std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", v),
            None => std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN"),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rotate_refuses_under_env_override() {
        let _registry = secrets_registry_test_lock();
        let _env = ENV_LOCK.lock().await;
        let _dek = setup_dek().await;
        let prev = std::env::var_os("BOOKCLERK_OPERATOR_TOKEN");
        std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", "env-override-token-value-002");

        let db = test_db().await;
        let err = rotate_operator_token(&db).await.unwrap_err().to_string();
        assert!(err.contains("BOOKCLERK_OPERATOR_TOKEN"));

        match prev {
            Some(v) => std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", v),
            None => std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN"),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn migrates_legacy_file() {
        let _registry = secrets_registry_test_lock();
        let _env = ENV_LOCK.lock().await;
        let _dek = setup_dek().await;
        let prev = std::env::var_os("BOOKCLERK_OPERATOR_TOKEN");
        std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN");

        let dir = tempdir().unwrap();
        let db = test_db().await;
        let cfg = Config {
            paths: Some(Paths::from_files_dir(dir.path().to_path_buf())),
            ..Config::default()
        };
        let legacy = dir.path().join("operator.token");
        std::fs::write(&legacy, "legacy-file-token-abcdefghijklmnopqrstuvwxyz12\n").unwrap();

        let (token, how) = read_or_create_operator_token(&cfg, &db).await.unwrap();
        assert_eq!(how, ResolveOperatorToken::LegacyFile);
        assert_eq!(token, "legacy-file-token-abcdefghijklmnopqrstuvwxyz12");
        assert!(!legacy.is_file());
        let loaded = load_operator_token(&db).await.unwrap().unwrap();
        assert_eq!(loaded, token);

        match prev {
            Some(v) => std::env::set_var("BOOKCLERK_OPERATOR_TOKEN", v),
            None => std::env::remove_var("BOOKCLERK_OPERATOR_TOKEN"),
        }
    }
}
