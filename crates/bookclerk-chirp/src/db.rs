//! DB-backed credential storage for Chirp accounts.
//!
//! Credentials live in `encrypted_secrets` under `provider =` the plugin id
//! (`chirp`), accessed only through [`SourceScope`].

use bookclerk_library::SourceScope;

use crate::auth::ChirpAuthFile;
use crate::error::{ChirpError, Result};

fn auth_name(account_id: &str) -> String {
    format!("{account_id}.chirp.auth")
}

/// Persist a [`ChirpAuthFile`] via the plugin scope.
pub async fn save_auth_to_db(
    auth: &ChirpAuthFile,
    scope: &SourceScope,
    account_id: &str,
) -> Result<()> {
    let json = serde_json::to_vec(auth)
        .map_err(|e| ChirpError::auth(format!("failed to serialize Chirp auth: {e}")))?;
    scope
        .save_source_auth(account_id, &auth_name(account_id), &json)
        .await
        .map_err(|e| ChirpError::auth(format!("failed to save Chirp auth to DB: {e}")))?;
    tracing::info!(account = %account_id, "Chirp auth stored in encrypted_secrets (sealed-v1)");
    Ok(())
}

/// Load a [`ChirpAuthFile`] for this plugin only.
pub async fn load_auth_from_db(
    scope: &SourceScope,
    account_id: &str,
) -> Result<Option<ChirpAuthFile>> {
    let Some(plaintext) = scope
        .load_source_auth(account_id, &auth_name(account_id))
        .await
        .map_err(|e| ChirpError::auth(format!("DB lookup failed for {account_id}: {e}")))?
    else {
        return Ok(None);
    };
    let auth: ChirpAuthFile = serde_json::from_slice(&plaintext).map_err(|e| {
        ChirpError::auth(format!("failed to parse Chirp auth for {account_id}: {e}"))
    })?;
    Ok(Some(auth))
}

/// List Chirp accounts for this plugin only.
pub async fn list_auth_from_db(scope: &SourceScope) -> Result<Vec<(String, ChirpAuthFile)>> {
    let records = scope
        .list_source_auth()
        .await
        .map_err(|e| ChirpError::auth(format!("DB list failed: {e}")))?;
    let mut out = Vec::new();
    for record in records {
        let Some(account_id) = record.account_id.clone() else {
            continue;
        };
        match bookclerk_library::unseal_secret(&record) {
            Ok(plaintext) => {
                if let Ok(auth) = serde_json::from_slice::<ChirpAuthFile>(&plaintext) {
                    out.push((account_id, auth));
                } else {
                    tracing::warn!(
                        account = %account_id,
                        "Chirp auth unsealed but JSON parse failed — skipping"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    account = %account_id,
                    err = %e,
                    "failed to unseal Chirp auth in list — skipping"
                );
            }
        }
    }
    Ok(out)
}

/// Remove a Chirp account secret.
pub async fn delete_auth_from_db(scope: &SourceScope, account_id: &str) -> Result<()> {
    scope
        .delete_source_auth(account_id, &auth_name(account_id))
        .await
        .map_err(|e| {
            ChirpError::auth(format!(
                "failed to delete Chirp auth from DB for {account_id}: {e}"
            ))
        })
}
