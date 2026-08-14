//! DB-backed credential storage for Audible accounts.
//!
//! Credentials live in `encrypted_secrets` under `provider =` the plugin id
//! (`audible`), accessed only through [`SourceScope`] — the same boundary as
//! third-party source plugins.
//!
//! Each credential is stored as `format = "sealed-v1"` wrapping a
//! `Protection::Plain` audible-rs envelope. The outer XChaCha20-Poly1305 seal
//! (process DEK from `master.key`) provides at-rest protection; audible-rs
//! inner encryption is intentionally bypassed so key derivation happens once
//! at startup rather than on every credential access.

use audible_rs::auth::Authenticator;
use bookclerk_library::{
    secret_kind, unseal_secret, EncryptedSecretRecord, SourceScope, FORMAT_SEALED_V1,
};

use crate::error::{AudibleError, Result};

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Secret name for an Audible account (`<account>.audible.auth`) under `encrypted_secrets`.
fn audible_name(account_name: &str) -> String {
    format!("{account_name}.audible.auth")
}

// ── Save ─────────────────────────────────────────────────────────────────────

/// Persist an [`Authenticator`] via the Audible [`SourceScope`].
///
/// The audible-rs envelope is serialized with `Protection::Plain` (no inner
/// Argon2), then sealed with the process DEK (`sealed-v1`).
///
/// # Errors
///
/// Returns an error when the operation fails.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
pub async fn save_authenticator_to_db(
    auth: &Authenticator,
    scope: &SourceScope,
    account_name: &str,
) -> Result<()> {
    let plain_bytes = tokio::task::spawn_blocking({
        let data = auth.export_value();
        move || {
            audible_rs::auth::authfile::write(
                &data,
                audible_rs::auth::authfile::Protection::Plain,
                None,
            )
            .map_err(|e| AudibleError::Auth(e.to_string()))
        }
    })
    .await
    .expect("blocking authfile write must not panic")?;

    let name = audible_name(account_name);
    scope
        .save_source_auth(account_name, &name, plain_bytes.as_bytes())
        .await
        .map_err(|e| AudibleError::Auth(format!("failed to save audible auth to DB: {e}")))?;

    crate::download::invalidate_account_client_cache(account_name);
    tracing::info!(account = %account_name, "audible auth stored in encrypted_secrets (sealed-v1)");
    Ok(())
}

// ── Load ─────────────────────────────────────────────────────────────────────

/// Load an [`Authenticator`] from the scoped `encrypted_secrets` table.
///
/// Registers a write-back callback so that token refreshes and cookie
/// exchanges persist back to the DB automatically.
///
/// Returns `None` when no secret for the given `account_name` exists.
///
/// # Errors
///
/// Returns an error when the operation fails.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
pub async fn load_authenticator_from_db(
    scope: &SourceScope,
    account_name: &str,
) -> Result<Option<Authenticator>> {
    let name = audible_name(account_name);
    let record = scope
        .get_source_auth_record(account_name, &name)
        .await
        .map_err(|e| AudibleError::Auth(format!("DB lookup failed for {account_name}: {e}")))?;

    let Some(record) = record else {
        return Ok(None);
    };

    let plain_bytes = unseal_record_for_audible(&record, account_name)?;

    let mut auth = tokio::task::spawn_blocking(move || {
        Authenticator::load_from_bytes(&plain_bytes, None)
            .map_err(|e| AudibleError::Auth(format!("failed to decode audible auth: {e}")))
    })
    .await
    .expect("blocking authfile decode must not panic")?;

    // Register async token-refresh write-back so refreshes persist to DB.
    // Full save → overwrite; save_merged → RMW merge onto current DB row.
    let scope_clone = scope.clone();
    let account_name_owned = account_name.to_string();
    auth.set_write_back_fn(move |value: serde_json::Value, merge_scope| {
        let scope_inner = scope_clone.clone();
        let acct = account_name_owned.clone();
        async move {
            let name = audible_name(&acct);
            let to_write = if let Some(merge_scope) = merge_scope {
                let existing = scope_inner
                    .get_source_auth_record(&acct, &name)
                    .await
                    .map_err(|e| audible_rs::auth::AuthError::InvalidData(e.to_string()))?;
                match existing {
                    Some(record) => {
                        let plain_bytes = unseal_record_for_audible(&record, &acct)
                            .map_err(|e| audible_rs::auth::AuthError::InvalidData(e.to_string()))?;
                        let current_auth = tokio::task::spawn_blocking(move || {
                            Authenticator::load_from_bytes(&plain_bytes, None).map_err(|e| {
                                audible_rs::auth::AuthError::InvalidData(e.to_string())
                            })
                        })
                        .await
                        .expect("blocking authfile decode must not panic")?;
                        let mut base = current_auth.export_value();
                        audible_rs::auth::merge_auth_json(&mut base, value, &merge_scope)?;
                        base
                    }
                    None => value,
                }
            } else {
                value
            };

            let plain_bytes = audible_rs::auth::authfile::write(
                &to_write,
                audible_rs::auth::authfile::Protection::Plain,
                None,
            )
            .map_err(|e| audible_rs::auth::AuthError::InvalidData(e.to_string()))?;

            scope_inner
                .save_source_auth(&acct, &name, plain_bytes.as_bytes())
                .await
                .map_err(|e| audible_rs::auth::AuthError::InvalidData(e.to_string()))?;
            crate::download::invalidate_account_client_cache(&acct);
            tracing::debug!(account = %acct, "audible token refreshed → encrypted_secrets");
            Ok(())
        }
    });

    register_authenticator_secrets(&auth);
    Ok(Some(auth))
}

/// Unseal an audible auth record (sealed-v1 or legacy formats).
fn unseal_record_for_audible(
    record: &EncryptedSecretRecord,
    account_name: &str,
) -> Result<Vec<u8>> {
    match record.format.as_str() {
        FORMAT_SEALED_V1 => unseal_secret(record).map_err(|e| {
            AudibleError::Auth(format!(
                "failed to unseal audible auth for {account_name}: {e}"
            ))
        }),
        "audible-rs-auth" => {
            // Legacy: audible-rs envelope with its own inner encryption.
            // Loaded raw; audible-rs will decrypt using BOOKCLERK_AUTH_PASSWORD if needed.
            Ok(record.ciphertext.clone())
        }
        other => Err(AudibleError::Auth(format!(
            "unsupported audible auth format {other:?} for account {account_name}"
        ))),
    }
}

// ── List ─────────────────────────────────────────────────────────────────────

/// List all Audible accounts stored in the DB for this scope.
///
/// Returns `(account_id, name)` tuples extracted from `encrypted_secrets` rows.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn list_audible_accounts_from_db(scope: &SourceScope) -> Result<Vec<(String, String)>> {
    let records = scope
        .list_source_auth()
        .await
        .map_err(|e| AudibleError::Auth(format!("DB list failed: {e}")))?;
    Ok(records
        .into_iter()
        .filter_map(|r| {
            let account_id = r.account_id?;
            Some((account_id, r.name))
        })
        .collect())
}

// ── Delete ───────────────────────────────────────────────────────────────────

/// Remove an Audible account secret from the DB.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn delete_audible_account_from_db(scope: &SourceScope, account_name: &str) -> Result<()> {
    let name = audible_name(account_name);
    scope
        .delete_source_auth(account_name, &name)
        .await
        .map_err(|e| {
            AudibleError::Auth(format!(
                "failed to delete audible auth from DB for {account_name}: {e}"
            ))
        })
}

// ── Widevine CDM ─────────────────────────────────────────────────────────────

/// Persist a raw Widevine `.wvd` device blob into `encrypted_secrets`.
///
/// `account_id` identifies which account the CDM was provisioned for.
/// The blob is sealed with the process DEK (`sealed-v1`).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn save_widevine_cdm_to_db(
    scope: &SourceScope,
    account_id: &str,
    wvd_bytes: &[u8],
) -> Result<()> {
    let name = format!("{account_id}.wvd");
    scope
        .save_secret(secret_kind::WIDEVINE, account_id, &name, wvd_bytes)
        .await
        .map_err(|e| AudibleError::Widevine(format!("failed to save Widevine CDM to DB: {e}")))?;
    tracing::info!(account = %account_id, "Widevine CDM stored in encrypted_secrets (sealed-v1)");
    Ok(())
}

/// Load a Widevine `.wvd` device blob from `encrypted_secrets`.
///
/// Returns `None` when no CDM for `account_id` is stored yet.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn load_widevine_cdm_from_db(
    scope: &SourceScope,
    account_id: &str,
) -> Result<Option<Vec<u8>>> {
    let name = format!("{account_id}.wvd");
    let record = scope
        .get_secret_record(secret_kind::WIDEVINE, account_id, &name)
        .await
        .map_err(|e| {
            AudibleError::Widevine(format!(
                "DB lookup failed for Widevine CDM {account_id}: {e}"
            ))
        })?;

    let Some(record) = record else {
        return Ok(None);
    };

    match record.format.as_str() {
        FORMAT_SEALED_V1 => {
            let bytes = unseal_secret(&record).map_err(|e| {
                AudibleError::Widevine(format!(
                    "failed to unseal Widevine CDM for {account_id}: {e}"
                ))
            })?;
            Ok(Some(bytes))
        }
        "wvd" => {
            // Legacy: raw WVD bytes without outer encryption.
            Ok(Some(record.ciphertext))
        }
        other => Err(AudibleError::Widevine(format!(
            "unsupported Widevine CDM format {other:?} for account {account_id}"
        ))),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Registers access/refresh tokens with the redaction set so logs never print them.
fn register_authenticator_secrets(auth: &Authenticator) {
    use secrecy::ExposeSecret;
    if let Some(t) = auth.access_token() {
        bookclerk_config::register_secret(t.expose_secret());
    }
    if let Some(t) = auth.refresh_token() {
        bookclerk_config::register_secret(t.expose_secret());
    }
}
