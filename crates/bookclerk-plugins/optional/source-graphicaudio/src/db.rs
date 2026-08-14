//! DB-backed credential storage for GraphicAudio accounts.
//!
//! Credentials live in `encrypted_secrets` under `provider =` the plugin id
//! (`graphicaudio`), accessed only through [`SourceScope`].

use bookclerk_library::SourceScope;

use crate::auth::GraphicAudioAuthFile;
use crate::error::{GraphicAudioError, Result};

/// `encrypted_secrets` blob name for a GraphicAudio account (`{id}.ga.auth`); sealed via [`SourceScope`].
fn auth_name(account_id: &str) -> String {
    format!("{account_id}.ga.auth")
}

/// Persist a [`GraphicAudioAuthFile`] via the plugin scope.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn save_auth_to_db(
    auth: &GraphicAudioAuthFile,
    scope: &SourceScope,
    account_id: &str,
) -> Result<()> {
    let json = serde_json::to_vec(auth).map_err(|e| {
        GraphicAudioError::auth(format!("failed to serialize GraphicAudio auth: {e}"))
    })?;
    scope
        .save_source_auth(account_id, &auth_name(account_id), &json)
        .await
        .map_err(|e| {
            GraphicAudioError::auth(format!("failed to save GraphicAudio auth to DB: {e}"))
        })?;
    tracing::info!(account = %account_id, "GraphicAudio auth stored in encrypted_secrets (sealed-v1)");
    Ok(())
}

/// Load a [`GraphicAudioAuthFile`] for this plugin only.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn load_auth_from_db(
    scope: &SourceScope,
    account_id: &str,
) -> Result<Option<GraphicAudioAuthFile>> {
    let Some(plaintext) = scope
        .load_source_auth(account_id, &auth_name(account_id))
        .await
        .map_err(|e| GraphicAudioError::auth(format!("DB lookup failed for {account_id}: {e}")))?
    else {
        return Ok(None);
    };
    let auth: GraphicAudioAuthFile = serde_json::from_slice(&plaintext).map_err(|e| {
        GraphicAudioError::auth(format!(
            "failed to parse GraphicAudio auth for {account_id}: {e}"
        ))
    })?;
    Ok(Some(auth))
}

/// List GraphicAudio accounts for this plugin only.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn list_auth_from_db(scope: &SourceScope) -> Result<Vec<(String, GraphicAudioAuthFile)>> {
    let records = scope
        .list_source_auth()
        .await
        .map_err(|e| GraphicAudioError::auth(format!("DB list failed: {e}")))?;
    let mut out = Vec::new();
    for record in records {
        let Some(account_id) = record.account_id.clone() else {
            continue;
        };
        match bookclerk_library::unseal_secret(&record) {
            Ok(plaintext) => {
                if let Ok(auth) = serde_json::from_slice::<GraphicAudioAuthFile>(&plaintext) {
                    out.push((account_id, auth));
                } else {
                    tracing::warn!(
                        account = %account_id,
                        "GraphicAudio auth unsealed but JSON parse failed — skipping"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    account = %account_id,
                    err = %e,
                    "failed to unseal GraphicAudio auth in list — skipping"
                );
            }
        }
    }
    Ok(out)
}

/// Remove a GraphicAudio account secret.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn delete_auth_from_db(scope: &SourceScope, account_id: &str) -> Result<()> {
    scope
        .delete_source_auth(account_id, &auth_name(account_id))
        .await
        .map_err(|e| {
            GraphicAudioError::auth(format!(
                "failed to delete GraphicAudio auth from DB for {account_id}: {e}"
            ))
        })
}
