//! Cluster secret-root join rules.
//!
//! The fingerprint stored in `cluster_identity` is the SHA-256 of the unwrapped
//! DEK. A host that finds an existing fingerprint must present that DEK. It
//! must not mint a second root. There is no KMS adapter in this slice: joining
//! means possessing `master.key` (raw or password-wrapped) for that DEK.

use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::batch::{self, exec, int, request, text};
use super::identity::preferred_cluster_id;
use crate::entities::cluster_identity;
use crate::error::{LibraryError, Result};
use crate::host_schema::current_schema_state;
use crate::master_key::{
    configure_master_key_with, discard_minted_master_key, master_key_fingerprint,
    resolve_master_key_detailed, uncache_master_key, MasterKey,
};
use crate::secrets::FORMAT_SEALED_V1;
use crate::store::LibraryStore;
use std::path::Path;

/// Singleton primary key for [`cluster_identity`].
const CLUSTER_ROW_ID: i64 = 1;

/// Cluster id and fingerprint of the secret root aligned for this process.
///
/// The unwrapped DEK stays in the process cache (`configure_master_key_with`).
/// Callers that need the key use [`crate::require_master_key`].
#[derive(Clone, Debug)]
pub struct ClusterSecret {
    /// Cluster id stored beside the fingerprint.
    pub cluster_id: String,
    /// SHA-256 hex of the DEK.
    pub secret_fingerprint: String,
}

/// Ensures this process's DEK is the cluster secret root.
///
/// * Existing fingerprint: the local key must match. A missing file is not minted.
/// * No fingerprint and sealed secrets present: the local key must unseal one
///   of them, then the fingerprint is recorded. A missing or wrong key fails
///   without writing a new root.
/// * No fingerprint and no sealed secrets: the local key (minted when absent)
///   is recorded. Concurrent initializers: one fingerprint wins; a loser that
///   minted a different key deletes that new file and fails.
///
/// On success the accepted DEK is installed in the process cache. The returned
/// value is only the cluster id and fingerprint.
///
/// # Errors
///
/// Returns an error when bootstrap material is missing or does not match the
/// database. Existing rows are not updated on failure.
pub async fn align_cluster_root(
    store: &LibraryStore,
    files_dir: &Path,
    password: Option<&str>,
) -> Result<ClusterSecret> {
    let existing = load_cluster_row(store).await?;
    if existing.is_some() && !crate::master_key::master_key_path(files_dir).is_file() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "secret root missing: database already has a cluster secret fingerprint and {dir}/master.key is absent",
            dir = files_dir.display()
        )));
    }

    let resolved = resolve_master_key_detailed(files_dir, password)?;
    let fingerprint = master_key_fingerprint(&resolved.key);
    let outcome = match existing {
        Some(row) => verify_existing(&row, &fingerprint, files_dir, &resolved)?,
        None => initialize_root(store, files_dir, &resolved, &fingerprint).await?,
    };
    configure_master_key_with(files_dir, password)?;
    Ok(outcome)
}

/// Accepts a local DEK that matches the stored fingerprint.
fn verify_existing(
    row: &cluster_identity::Model,
    fingerprint: &str,
    files_dir: &Path,
    resolved: &crate::master_key::MasterKeyResolution,
) -> Result<ClusterSecret> {
    if row.secret_fingerprint != fingerprint {
        forget_rejected_key(files_dir, resolved);
        return Err(LibraryError::Other(anyhow::anyhow!(
            "secret root mismatch: local master key does not match the cluster secret fingerprint"
        )));
    }
    Ok(ClusterSecret {
        cluster_id: row.cluster_id.clone(),
        secret_fingerprint: row.secret_fingerprint.clone(),
    })
}

/// Inserts the first fingerprint, or adopts the winner of a concurrent insert.
async fn initialize_root(
    store: &LibraryStore,
    files_dir: &Path,
    resolved: &crate::master_key::MasterKeyResolution,
    fingerprint: &str,
) -> Result<ClusterSecret> {
    match probe_sealed_secrets(store, &resolved.key).await? {
        SecretProbe::Reject => {
            forget_rejected_key(files_dir, resolved);
            return Err(LibraryError::Other(anyhow::anyhow!(
                "secret root mismatch: database has sealed secrets this master key cannot unseal"
            )));
        }
        SecretProbe::None | SecretProbe::Matches => {}
    }

    let preferred = preferred_cluster_id(files_dir)?;
    let cluster_id = preferred.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let state = current_schema_state(store.db()).await?;
    let now = Utc::now().to_rfc3339();
    let operation_id = format!(
        "cluster-secret-{}",
        &fingerprint[..12.min(fingerprint.len())]
    );
    batch::execute_host_batch(
        store,
        request(
            &operation_id,
            vec![exec(
                "INSERT OR IGNORE INTO cluster_identity (
                    id, cluster_id, secret_fingerprint, schema_state, created_at
                ) VALUES (?, ?, ?, ?, ?)",
                vec![
                    int(CLUSTER_ROW_ID),
                    text(&cluster_id),
                    text(fingerprint),
                    text(&state.display()),
                    text(&now),
                ],
            )],
        ),
    )
    .await?;

    let row = load_cluster_row(store).await?.ok_or_else(|| {
        LibraryError::Other(anyhow::anyhow!(
            "secret root missing: cluster identity insert did not persist"
        ))
    })?;
    if row.secret_fingerprint != fingerprint {
        forget_rejected_key(files_dir, resolved);
        return Err(LibraryError::Other(anyhow::anyhow!(
            "secret root mismatch: another initializer committed a different cluster secret"
        )));
    }
    Ok(ClusterSecret {
        cluster_id: row.cluster_id,
        secret_fingerprint: row.secret_fingerprint,
    })
}

/// Reads the singleton cluster row when present.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_cluster_row(store: &LibraryStore) -> Result<Option<cluster_identity::Model>> {
    cluster_identity::Entity::find_by_id(CLUSTER_ROW_ID)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)
}

/// Drops a rejected DEK from the cache and deletes it when this call created the file.
fn forget_rejected_key(files_dir: &Path, resolved: &crate::master_key::MasterKeyResolution) {
    if resolved.minted {
        let _ = discard_minted_master_key(files_dir, &resolved.key);
    }
    uncache_master_key(&resolved.key);
}

/// Result of opening one existing `sealed-v1` row with a candidate DEK.
enum SecretProbe {
    /// No `sealed-v1` rows exist yet.
    None,
    /// At least one sealed row opens with this DEK.
    Matches,
    /// A sealed row exists and this DEK cannot open it.
    Reject,
}

/// Checks whether `key` can unseal an existing sealed secret.
async fn probe_sealed_secrets(store: &LibraryStore, key: &MasterKey) -> Result<SecretProbe> {
    use crate::entities::encrypted_secrets;
    let row = encrypted_secrets::Entity::find()
        .filter(encrypted_secrets::Column::Format.eq(FORMAT_SEALED_V1))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let Some(row) = row else {
        return Ok(SecretProbe::None);
    };
    let Some(nonce) = row.cipher_nonce.as_deref() else {
        return Ok(SecretProbe::Reject);
    };
    match crate::master_key::unseal_with_dek(&row.ciphertext, nonce, key) {
        Ok(_) => Ok(SecretProbe::Matches),
        Err(_) => Ok(SecretProbe::Reject),
    }
}
