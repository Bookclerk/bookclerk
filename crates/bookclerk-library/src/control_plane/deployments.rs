//! Desired plugin deployments and host-local observations.
//!
//! A deployment row means one host should have the instance installed and
//! running. Observation rows are written only by that host's reconciler.
//! Desired writes do not update observations.

use chrono::{Duration, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::batch::{self, exec, int, query, request, text};
use super::documents::ConfigActor;
use super::instances::{load_plugin_instance, PluginInstanceId};
use crate::entities::{bookclerk_receipts, plugin_deployment_observations, plugin_deployments};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Desired state stored by this slice. There is no `absent` or drain.
pub const DESIRED_PRESENT: &str = "present";

/// `bookclerk_receipts.operation_kind` for a deployment replacement.
const REPLACE_DEPLOYMENT_KIND: &str = "replacePluginDeployment";

/// Receipt status for a committed replacement.
const STATUS_OK: &str = "ok";

/// Receipt status when the expected revision did not match inside the batch.
const STATUS_CONFLICT: &str = "conflict";

/// Maximum UTF-8 size of an observation detail string.
pub const MAX_OBSERVATION_DETAIL_BYTES: usize = 512;

/// Observed deployment status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentStatus {
    /// Installer commit succeeded, or the ledger already had this plugin key.
    Installed,
    /// Session spawn returned. Health is not confirmed on this tick.
    Running,
    /// Health succeeded with the applied config revision.
    Healthy,
    /// Install, grant, config, secrets, spawn, or health failed.
    Error,
}

impl DeploymentStatus {
    /// Stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Running => "running",
            Self::Healthy => "healthy",
            Self::Error => "error",
        }
    }

    /// Parses stored text.
    ///
    /// # Errors
    ///
    /// Returns an error when `raw` is not one of the four statuses.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "installed" => Ok(Self::Installed),
            "running" => Ok(Self::Running),
            "healthy" => Ok(Self::Healthy),
            "error" => Ok(Self::Error),
            _ => Err(LibraryError::Other(anyhow::anyhow!(
                "invalid deployment observation status `{raw}`"
            ))),
        }
    }
}

/// One desired deployment row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDeployment {
    /// Deployment id.
    pub deployment_id: String,
    /// Instance to run.
    pub plugin_instance_id: PluginInstanceId,
    /// Target host.
    pub host_id: String,
    /// Always [`DESIRED_PRESENT`] in this slice.
    pub desired: String,
    /// Compare-and-swap revision.
    pub revision: i64,
    /// RFC 3339 commit time.
    pub updated_at: String,
    /// Audit identity of the last desired-state writer.
    pub updated_by: String,
}

/// One host's observation of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentObservation {
    /// Deployment id.
    pub deployment_id: String,
    /// Observing host.
    pub host_id: String,
    /// Process incarnation.
    pub incarnation: String,
    /// Last status.
    pub status: DeploymentStatus,
    /// Bounded detail. Empty when healthy.
    pub detail: String,
    /// Config revision applied by spawn, if a spawn has happened.
    pub applied_config_revision: Option<i64>,
    /// RFC 3339 observation time.
    pub observed_at: String,
}

/// Outcome of [`replace_plugin_deployment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentReplace {
    /// The desired row is the committed body.
    Applied(PluginDeployment),
    /// `expected_revision` was not current. No observation row was written.
    Conflict {
        /// Revision currently stored.
        current_revision: i64,
    },
    /// This operation id already committed at `revision`.
    Replayed {
        /// Revision of the original commit.
        revision: i64,
    },
}

/// Inserts `(instance, host, present)` when that pair is absent.
///
/// A second call returns the original row and does not bump the revision.
///
/// # Errors
///
/// Returns an error when the actor may not write, the instance is missing, or the insert fails.
pub async fn ensure_plugin_deployment(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
    host_id: &str,
) -> Result<PluginDeployment> {
    authorize_create(actor)?;
    validate_host_id(host_id)?;
    if load_plugin_instance(store, instance_id).await?.is_none() {
        return Err(LibraryError::NotFound(format!(
            "plugin instance {instance_id}"
        )));
    }
    if let Some(existing) = find_deployment_pair(store, instance_id, host_id).await? {
        return Ok(existing);
    }
    let deployment_id = Uuid::new_v4().hyphenated().to_string();
    let now = Utc::now().to_rfc3339();
    let model = plugin_deployments::ActiveModel {
        deployment_id: Set(deployment_id.clone()),
        plugin_instance_id: Set(instance_id.as_str().to_string()),
        host_id: Set(host_id.to_string()),
        desired: Set(DESIRED_PRESENT.to_string()),
        revision: Set(1),
        updated_at: Set(now),
        updated_by: Set(actor.audit_id().to_string()),
    };
    match plugin_deployments::Entity::insert(model)
        .exec(store.db())
        .await
    {
        Ok(_) => load_deployment_by_id(store, &deployment_id)
            .await?
            .ok_or_else(|| LibraryError::NotFound(deployment_id)),
        Err(err) => {
            if let Some(existing) = find_deployment_pair(store, instance_id, host_id).await? {
                return Ok(existing);
            }
            Err(LibraryError::Orm(err))
        }
    }
}

/// Replaces desired state when `expected_revision` is current.
///
/// This slice only accepts `present`. The batch carries a revision predicate
/// and a `replacePluginDeployment` receipt. It does not update observation rows.
///
/// # Errors
///
/// Returns an error when the actor may not replace, the deployment is missing,
/// or the operation id was already used for a different body.
pub async fn replace_plugin_deployment(
    store: &LibraryStore,
    actor: &ConfigActor,
    deployment_id: &str,
    expected_revision: i64,
    operation_id: &str,
) -> Result<DeploymentReplace> {
    authorize_replace(actor)?;
    validate_operation_id(operation_id)?;
    if expected_revision < 1 {
        return Err(invalid("expected revision must be >= 1"));
    }
    let request_hash = deployment_hash(deployment_id, expected_revision, actor.audit_id());
    if let Some(outcome) = existing_receipt(store, operation_id, &request_hash).await? {
        return Ok(outcome);
    }
    let current = load_deployment_by_id(store, deployment_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin deployment {deployment_id}")))?;
    if current.desired != DESIRED_PRESENT {
        return Err(invalid("deployment desired state must be present"));
    }
    if current.revision != expected_revision {
        if let Some(outcome) = existing_receipt(store, operation_id, &request_hash).await? {
            return Ok(outcome);
        }
        return Ok(DeploymentReplace::Conflict {
            current_revision: current.revision,
        });
    }
    let now = Utc::now().to_rfc3339();
    let expires = (Utc::now() + Duration::hours(24)).to_rfc3339();
    let slot = format!("deploy-cas:{operation_id}");
    let next_revision = expected_revision + 1;
    let reply = batch::execute_host_batch(
        store,
        request(
            operation_id,
            vec![
                exec(
                    "INSERT OR IGNORE INTO bookclerk_slots (slot_key, bump) \
                     SELECT ?, 0 WHERE NOT EXISTS ( \
                        SELECT 1 FROM bookclerk_receipts WHERE operation_id = ? \
                     )",
                    vec![text(&slot), text(operation_id)],
                ),
                exec(
                    "UPDATE plugin_deployments SET
                        desired = ?, revision = revision + 1, updated_at = ?, updated_by = ?
                     WHERE deployment_id = ? AND revision = ? AND desired = ?
                       AND NOT EXISTS (
                            SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?
                       )",
                    vec![
                        text(DESIRED_PRESENT),
                        text(&now),
                        text(actor.audit_id()),
                        text(deployment_id),
                        int(expected_revision),
                        text(DESIRED_PRESENT),
                        text(operation_id),
                    ],
                ),
                exec(
                    "UPDATE bookclerk_slots SET bump = 1
                     WHERE slot_key = ? AND bump = 0
                       AND EXISTS (
                            SELECT 1 FROM plugin_deployments
                             WHERE deployment_id = ? AND revision = ? AND updated_at = ?
                       )",
                    vec![
                        text(&slot),
                        text(deployment_id),
                        int(next_revision),
                        text(&now),
                    ],
                ),
                exec(
                    "INSERT INTO bookclerk_receipts (
                        operation_id, operation_kind, request_hash, status, payload,
                        created_at, expires_at, consume_key
                     ) SELECT ?, ?, ?,
                        CASE WHEN s.bump = 1 THEN 'ok' ELSE 'conflict' END,
                        json_object('revision', d.revision),
                        ?, ?, NULL
                       FROM bookclerk_slots AS s
                       JOIN plugin_deployments AS d ON d.deployment_id = ?
                      WHERE s.slot_key = ?
                        AND NOT EXISTS (
                            SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?
                        )",
                    vec![
                        text(operation_id),
                        text(REPLACE_DEPLOYMENT_KIND),
                        text(&request_hash),
                        text(&now),
                        text(&expires),
                        text(deployment_id),
                        text(&slot),
                        text(operation_id),
                    ],
                ),
                exec(
                    "DELETE FROM bookclerk_slots WHERE slot_key = ?",
                    vec![text(&slot)],
                ),
                query(
                    "SELECT status, request_hash, payload FROM bookclerk_receipts \
                     WHERE operation_id = ?",
                    vec![text(operation_id)],
                    1,
                ),
            ],
        ),
    )
    .await?;
    let rows = batch::rows(&reply, 5);
    let Some(row) = rows.first() else {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "plugin deployment replace did not record a receipt"
        )));
    };
    let status = batch::text_cell(row, 0)?;
    let stored_hash = batch::text_cell(row, 1)?;
    let payload = batch::text_cell(row, 2)?;
    if stored_hash != request_hash {
        return Err(LibraryError::Conflict(format!(
            "idempotency conflict: operation {operation_id} was already used for a different deployment write"
        )));
    }
    let receipt: ReceiptPayload = serde_json::from_str(&payload).map_err(|err| {
        LibraryError::Other(anyhow::anyhow!("invalid deployment receipt payload: {err}"))
    })?;
    if status == STATUS_CONFLICT {
        return Ok(DeploymentReplace::Conflict {
            current_revision: receipt.revision,
        });
    }
    if status != STATUS_OK {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "invalid deployment receipt status {status}"
        )));
    }
    let stored = load_deployment_by_id(store, deployment_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(deployment_id.to_string()))?;
    if stored.revision == next_revision {
        Ok(DeploymentReplace::Applied(stored))
    } else {
        Ok(DeploymentReplace::Replayed {
            revision: receipt.revision,
        })
    }
}

/// Present deployments whose `host_id` is `host_id`.
///
/// Rows for any other host are not returned.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_present_deployments_for_host(
    store: &LibraryStore,
    host_id: &str,
) -> Result<Vec<PluginDeployment>> {
    let rows = plugin_deployments::Entity::find()
        .filter(plugin_deployments::Column::HostId.eq(host_id))
        .filter(plugin_deployments::Column::Desired.eq(DESIRED_PRESENT))
        .order_by_asc(plugin_deployments::Column::DeploymentId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    rows.into_iter().map(deployment_from_model).collect()
}

/// True when any deployment row names `instance_id`.
///
/// Key-based spawn must not pick this instance. The deployment reconciler
/// passes the instance id and the already resolved bindings.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn instance_has_deployment(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<bool> {
    let found = plugin_deployments::Entity::find()
        .filter(plugin_deployments::Column::PluginInstanceId.eq(instance_id.as_str()))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(found.is_some())
}

/// Canonical plugin keys with a present deployment on `host_id`.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn present_plugin_keys_for_host(
    store: &LibraryStore,
    host_id: &str,
) -> Result<Vec<String>> {
    let deployments = list_present_deployments_for_host(store, host_id).await?;
    let mut keys = Vec::new();
    for deployment in deployments {
        if let Some(instance) = load_plugin_instance(store, &deployment.plugin_instance_id).await? {
            keys.push(instance.plugin_key);
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

/// Loads one deployment.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_deployment(
    store: &LibraryStore,
    deployment_id: &str,
) -> Result<Option<PluginDeployment>> {
    load_deployment_by_id(store, deployment_id).await
}

/// Loads the observation for one deployment on one host.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_observation(
    store: &LibraryStore,
    deployment_id: &str,
    host_id: &str,
) -> Result<Option<DeploymentObservation>> {
    let row = plugin_deployment_observations::Entity::find_by_id((
        deployment_id.to_string(),
        host_id.to_string(),
    ))
    .one(store.db())
    .await
    .map_err(LibraryError::Orm)?;
    row.map(observation_from_model).transpose()
}

/// Upserts one local observation.
///
/// A failed upsert does not roll back an install. The caller retries on the next tick.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn upsert_observation(
    store: &LibraryStore,
    observation: &DeploymentObservation,
) -> Result<()> {
    let detail = bounded_observation_detail(&observation.detail);
    let model = plugin_deployment_observations::ActiveModel {
        deployment_id: Set(observation.deployment_id.clone()),
        host_id: Set(observation.host_id.clone()),
        incarnation: Set(observation.incarnation.clone()),
        status: Set(observation.status.as_str().to_string()),
        detail: Set(detail),
        applied_config_revision: Set(observation.applied_config_revision),
        observed_at: Set(observation.observed_at.clone()),
    };
    plugin_deployment_observations::Entity::insert(model)
        .on_conflict(
            OnConflict::columns([
                plugin_deployment_observations::Column::DeploymentId,
                plugin_deployment_observations::Column::HostId,
            ])
            .update_columns([
                plugin_deployment_observations::Column::Incarnation,
                plugin_deployment_observations::Column::Status,
                plugin_deployment_observations::Column::Detail,
                plugin_deployment_observations::Column::AppliedConfigRevision,
                plugin_deployment_observations::Column::ObservedAt,
            ])
            .to_owned(),
        )
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// Truncates detail to 512 bytes and drops ASCII control characters.
#[must_use]
pub fn bounded_observation_detail(detail: &str) -> String {
    let mut out = String::new();
    for ch in detail.chars() {
        if ch.is_control() {
            continue;
        }
        let next_len = out.len() + ch.len_utf8();
        if next_len > MAX_OBSERVATION_DETAIL_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

/// Loads one deployment by primary key.
///
/// # Errors
///
/// Returns an error when the read fails.
async fn load_deployment_by_id(
    store: &LibraryStore,
    deployment_id: &str,
) -> Result<Option<PluginDeployment>> {
    let row = plugin_deployments::Entity::find_by_id(deployment_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    row.map(deployment_from_model).transpose()
}

/// Finds the unique `(instance, host)` row.
///
/// # Errors
///
/// Returns an error when the read fails.
async fn find_deployment_pair(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    host_id: &str,
) -> Result<Option<PluginDeployment>> {
    let row = plugin_deployments::Entity::find()
        .filter(plugin_deployments::Column::PluginInstanceId.eq(instance_id.as_str()))
        .filter(plugin_deployments::Column::HostId.eq(host_id))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    row.map(deployment_from_model).transpose()
}

/// Maps a deployment row, failing when the instance id is not a UUID.
fn deployment_from_model(row: plugin_deployments::Model) -> Result<PluginDeployment> {
    Ok(PluginDeployment {
        deployment_id: row.deployment_id,
        plugin_instance_id: PluginInstanceId::parse(&row.plugin_instance_id)?,
        host_id: row.host_id,
        desired: row.desired,
        revision: row.revision,
        updated_at: row.updated_at,
        updated_by: row.updated_by,
    })
}

/// Maps an observation row.
fn observation_from_model(
    row: plugin_deployment_observations::Model,
) -> Result<DeploymentObservation> {
    Ok(DeploymentObservation {
        deployment_id: row.deployment_id,
        host_id: row.host_id,
        incarnation: row.incarnation,
        status: DeploymentStatus::parse(&row.status)?,
        detail: row.detail,
        applied_config_revision: row.applied_config_revision,
        observed_at: row.observed_at,
    })
}

/// Resolves a prior deployment receipt.
///
/// # Errors
///
/// Returns an error when the operation id was used for a different body or kind.
async fn existing_receipt(
    store: &LibraryStore,
    operation_id: &str,
    request_hash: &str,
) -> Result<Option<DeploymentReplace>> {
    let Some(receipt) = bookclerk_receipts::Entity::find_by_id(operation_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    if receipt.operation_kind != REPLACE_DEPLOYMENT_KIND || receipt.request_hash != request_hash {
        return Err(LibraryError::Conflict(format!(
            "idempotency conflict: operation {operation_id} was already used for a different deployment write"
        )));
    }
    let payload = receipt.payload.unwrap_or_default();
    let parsed: ReceiptPayload = serde_json::from_str(&payload).map_err(|err| {
        LibraryError::Other(anyhow::anyhow!("invalid deployment receipt payload: {err}"))
    })?;
    if receipt.status == STATUS_CONFLICT {
        return Ok(Some(DeploymentReplace::Conflict {
            current_revision: parsed.revision,
        }));
    }
    if receipt.status != STATUS_OK {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "invalid deployment receipt status {}",
            receipt.status
        )));
    }
    Ok(Some(DeploymentReplace::Replayed {
        revision: parsed.revision,
    }))
}

/// Stable request hash for one deployment replacement.
fn deployment_hash(deployment_id: &str, expected_revision: i64, actor: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [
        deployment_id,
        DESIRED_PRESENT,
        &expected_revision.to_string(),
        actor,
        REPLACE_DEPLOYMENT_KIND,
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0xff]);
    }
    hex::encode(hasher.finalize())
}

/// Operator and bootstrap may insert a deployment.
fn authorize_create(actor: &ConfigActor) -> Result<()> {
    match actor {
        ConfigActor::Operator { .. } | ConfigActor::Bootstrap => Ok(()),
        ConfigActor::Administrator { .. } | ConfigActor::Member { .. } => Err(unauthorized()),
    }
}

/// Only an operator may replace desired state.
fn authorize_replace(actor: &ConfigActor) -> Result<()> {
    match actor {
        ConfigActor::Operator { .. } => Ok(()),
        ConfigActor::Bootstrap | ConfigActor::Administrator { .. } | ConfigActor::Member { .. } => {
            Err(unauthorized())
        }
    }
}

/// Rejects an empty host id.
fn validate_host_id(host_id: &str) -> Result<()> {
    if host_id.is_empty() || host_id.len() > 128 || host_id.contains('\u{0000}') {
        return Err(invalid("host id must be 1..=128 characters"));
    }
    Ok(())
}

/// Rejects an empty or oversized operation id.
fn validate_operation_id(operation_id: &str) -> Result<()> {
    if operation_id.is_empty() || operation_id.len() > 128 || operation_id.contains('\u{0000}') {
        return Err(invalid("operation id must be 1..=128 characters"));
    }
    Ok(())
}

/// Error for a write the actor is not allowed to perform.
fn unauthorized() -> LibraryError {
    LibraryError::Other(anyhow::anyhow!(
        "unauthorized configuration write: operator authority is required"
    ))
}

/// Error for a deployment value outside this slice.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("invalid configuration: {detail}"))
}

/// JSON stored on the deployment receipt.
#[derive(Debug, Deserialize)]
struct ReceiptPayload {
    /// Revision observed when the receipt was written.
    revision: i64,
}
