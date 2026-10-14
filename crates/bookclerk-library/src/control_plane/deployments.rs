//! Desired plugin deployments and node-local observations.
//!
//! A deployment row means one node should have the instance installed and
//! running. Observation rows are written only by that node's reconciler.
//! Desired writes do not update observations.

use super::node_queue::NodeMessage;
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

/// What a deployment row asks its node to do, and who decided it.
///
/// Operators write [`DesiredState::Present`] and [`DesiredState::Absent`]
/// (`plugins deploy|undeploy`, the Settings switch). The placement scheduler
/// writes [`DesiredState::Placed`] and [`DesiredState::Unplaced`] for an
/// instance it owns. A node runs the instance for `present` and `placed`
/// and retires it for `absent` and `unplaced`; the split only records whose
/// decision the row carries, so "the operator turned this off" never reads
/// like "the scheduler put the replica elsewhere".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DesiredState {
    /// An operator enabled the instance on this node.
    Present,
    /// An operator disabled the instance on this node.
    Absent,
    /// The scheduler placed a replica on this node.
    Placed,
    /// The scheduler holds no replica on this node (scaled away, node no
    /// longer qualifies, or not chosen).
    Unplaced,
}

impl DesiredState {
    /// Every state, in stored-text order.
    pub const ALL: [Self; 4] = [Self::Present, Self::Absent, Self::Placed, Self::Unplaced];

    /// Stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
            Self::Placed => "placed",
            Self::Unplaced => "unplaced",
        }
    }

    /// Parses stored or API text.
    ///
    /// # Errors
    ///
    /// Returns an error when `raw` names no state.
    pub fn parse(raw: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == raw.trim())
            .ok_or_else(|| invalid("desired must be `present`, `absent`, `placed`, or `unplaced`"))
    }

    /// True when the node should run the instance.
    #[must_use]
    pub fn runs(self) -> bool {
        matches!(self, Self::Present | Self::Placed)
    }

    /// True for the scheduler's states.
    #[must_use]
    pub fn is_scheduled(self) -> bool {
        matches!(self, Self::Placed | Self::Unplaced)
    }

    /// The operator state for an enable (`true`) or disable switch.
    #[must_use]
    pub fn operator(enabled: bool) -> Self {
        if enabled {
            Self::Present
        } else {
            Self::Absent
        }
    }

    /// The scheduler state for a replica kept (`true`) or not.
    #[must_use]
    pub fn scheduled(placed: bool) -> Self {
        if placed {
            Self::Placed
        } else {
            Self::Unplaced
        }
    }

    /// The operator state that keeps what this row does today: `placed`
    /// becomes `present` and `unplaced` becomes `absent` when an instance
    /// is handed back from the scheduler.
    #[must_use]
    pub fn handed_back(self) -> Self {
        match self {
            Self::Placed => Self::Present,
            Self::Unplaced => Self::Absent,
            other => other,
        }
    }
}

impl std::fmt::Display for DesiredState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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
    /// The node has the plugin's tree (materialized from the installed
    /// version, or host-local); the spawn comes next.
    Materialized,
    /// Session spawn returned. Health is not confirmed on this tick.
    Running,
    /// Health succeeded with the applied config revision.
    Healthy,
    /// Install, grant, config, secrets, spawn, or health failed.
    Error,
    /// Desired state is `absent`; the node stopped the guest.
    Retired,
}

impl DeploymentStatus {
    /// Stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Materialized => "materialized",
            Self::Running => "running",
            Self::Healthy => "healthy",
            Self::Error => "error",
            Self::Retired => "retired",
        }
    }

    /// Parses stored text.
    ///
    /// # Errors
    ///
    /// Returns an error when `raw` is not one of the five statuses.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "materialized" => Ok(Self::Materialized),
            "running" => Ok(Self::Running),
            "healthy" => Ok(Self::Healthy),
            "error" => Ok(Self::Error),
            "retired" => Ok(Self::Retired),
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
    /// Target node.
    pub node_id: String,
    /// What the node should do, and whose decision it is.
    pub desired: DesiredState,
    /// Compare-and-swap revision.
    pub revision: i64,
    /// RFC 3339 commit time.
    pub updated_at: String,
    /// Audit identity of the last desired-state writer.
    pub updated_by: String,
}

/// One node's observation of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentObservation {
    /// Deployment id.
    pub deployment_id: String,
    /// Observing node.
    pub node_id: String,
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

/// Inserts `(instance, node, desired)` when that pair has no row.
///
/// A second call returns the original row and does not bump the revision.
///
/// # Errors
///
/// Returns an error when the actor may not write `desired`, the instance is
/// missing, or the insert fails.
pub async fn ensure_plugin_deployment(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
    node_id: &str,
    desired: DesiredState,
) -> Result<PluginDeployment> {
    authorize_create(actor, desired)?;
    validate_node_id(node_id)?;
    if load_plugin_instance(store, instance_id).await?.is_none() {
        return Err(LibraryError::NotFound(format!(
            "plugin instance {instance_id}"
        )));
    }
    if let Some(existing) = find_deployment_pair(store, instance_id, node_id).await? {
        return Ok(existing);
    }
    let deployment_id = Uuid::new_v4().hyphenated().to_string();
    let now = Utc::now().to_rfc3339();
    let model = plugin_deployments::ActiveModel {
        deployment_id: Set(deployment_id.clone()),
        plugin_instance_id: Set(instance_id.as_str().to_string()),
        node_id: Set(node_id.to_string()),
        desired: Set(desired.as_str().to_string()),
        revision: Set(1),
        updated_at: Set(now),
        updated_by: Set(actor.audit_id().to_string()),
    };
    match plugin_deployments::Entity::insert(model)
        .exec(store.db())
        .await
    {
        Ok(_) => {
            let created = load_deployment_by_id(store, &deployment_id)
                .await?
                .ok_or_else(|| LibraryError::NotFound(deployment_id))?;
            notify_deployment(store, &created).await;
            Ok(created)
        }
        Err(err) => {
            if let Some(existing) = find_deployment_pair(store, instance_id, node_id).await? {
                return Ok(existing);
            }
            Err(LibraryError::Orm(err))
        }
    }
}

/// Replaces desired state when `expected_revision` is current.
///
/// The batch carries a revision predicate and a `replacePluginDeployment`
/// receipt. It does not update observation rows.
///
/// # Errors
///
/// Returns an error when the actor may not write `desired`, the deployment
/// is missing, or the operation id was already used for a different body.
pub async fn replace_plugin_deployment(
    store: &LibraryStore,
    actor: &ConfigActor,
    deployment_id: &str,
    expected_revision: i64,
    desired: DesiredState,
    operation_id: &str,
) -> Result<DeploymentReplace> {
    authorize_replace(actor, desired)?;
    validate_operation_id(operation_id)?;
    let desired = desired.as_str();
    if expected_revision < 1 {
        return Err(invalid("expected revision must be >= 1"));
    }
    let request_hash = deployment_hash(deployment_id, expected_revision, desired, actor.audit_id());
    if let Some(outcome) = existing_receipt(store, operation_id, &request_hash).await? {
        return Ok(outcome);
    }
    let current = load_deployment_by_id(store, deployment_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin deployment {deployment_id}")))?;
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
                     WHERE deployment_id = ? AND revision = ?
                       AND NOT EXISTS (
                            SELECT 1 FROM bookclerk_receipts WHERE operation_id = ?
                       )",
                    vec![
                        text(desired),
                        text(&now),
                        text(actor.audit_id()),
                        text(deployment_id),
                        int(expected_revision),
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
        notify_deployment(store, &stored).await;
        Ok(DeploymentReplace::Applied(stored))
    } else {
        Ok(DeploymentReplace::Replayed {
            revision: receipt.revision,
        })
    }
}

/// Present deployments whose `node_id` is `node_id`.
///
/// Rows for any other node are not returned.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_present_deployments_for_node(
    store: &LibraryStore,
    node_id: &str,
) -> Result<Vec<PluginDeployment>> {
    let rows = plugin_deployments::Entity::find()
        .filter(plugin_deployments::Column::NodeId.eq(node_id))
        .filter(
            plugin_deployments::Column::Desired.is_in(
                DesiredState::ALL
                    .into_iter()
                    .filter(|state| state.runs())
                    .map(DesiredState::as_str),
            ),
        )
        .order_by_asc(plugin_deployments::Column::DeploymentId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    rows.into_iter().map(deployment_from_model).collect()
}

/// Every deployment row targeting `node_id`, present or absent.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_deployments_for_node(
    store: &LibraryStore,
    node_id: &str,
) -> Result<Vec<PluginDeployment>> {
    let rows = plugin_deployments::Entity::find()
        .filter(plugin_deployments::Column::NodeId.eq(node_id))
        .order_by_asc(plugin_deployments::Column::DeploymentId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    rows.into_iter().map(deployment_from_model).collect()
}

/// Every deployment row in the cluster.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_all_deployments(store: &LibraryStore) -> Result<Vec<PluginDeployment>> {
    let rows = plugin_deployments::Entity::find()
        .order_by_asc(plugin_deployments::Column::DeploymentId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    rows.into_iter().map(deployment_from_model).collect()
}

/// Every deployment row for `instance_id`, on any node.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_deployments_for_instance(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<Vec<PluginDeployment>> {
    let rows = plugin_deployments::Entity::find()
        .filter(plugin_deployments::Column::PluginInstanceId.eq(instance_id.as_str()))
        .order_by_asc(plugin_deployments::Column::NodeId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    rows.into_iter().map(deployment_from_model).collect()
}

/// Every observation row for `deployment_id`, from any node.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn list_observations_for_deployment(
    store: &LibraryStore,
    deployment_id: &str,
) -> Result<Vec<DeploymentObservation>> {
    let rows = plugin_deployment_observations::Entity::find()
        .filter(plugin_deployment_observations::Column::DeploymentId.eq(deployment_id))
        .order_by_asc(plugin_deployment_observations::Column::NodeId)
        .all(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    rows.into_iter().map(observation_from_model).collect()
}

/// The deployment of `instance_id` on `node_id`, if any.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_deployment_for_node(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    node_id: &str,
) -> Result<Option<PluginDeployment>> {
    find_deployment_pair(store, instance_id, node_id).await
}

/// Deletes an absent deployment and its observations.
///
/// # Errors
///
/// Returns an error when the actor may not write, the row is still present,
/// or the delete fails.
pub async fn delete_plugin_deployment(
    store: &LibraryStore,
    actor: &ConfigActor,
    deployment_id: &str,
) -> Result<()> {
    let current = load_deployment_by_id(store, deployment_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin deployment {deployment_id}")))?;
    authorize_replace(actor, current.desired)?;
    if current.desired.runs() {
        return Err(LibraryError::Conflict(format!(
            "deployment is still desired {}; stop it first",
            current.desired
        )));
    }
    plugin_deployment_observations::Entity::delete_many()
        .filter(plugin_deployment_observations::Column::DeploymentId.eq(deployment_id))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    plugin_deployments::Entity::delete_by_id(deployment_id.to_string())
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    super::node_queue::notify_node(
        store,
        &current.node_id,
        NodeMessage::DeploymentChanged {
            deployment_id: current.deployment_id.clone(),
            plugin_instance_id: current.plugin_instance_id.as_str().to_string(),
            desired: "deleted".into(),
            revision: 0,
        },
    )
    .await;
    Ok(())
}

/// Wakes the deployment's node after a committed desired-state write.
async fn notify_deployment(store: &LibraryStore, deployment: &PluginDeployment) {
    super::node_queue::notify_node(
        store,
        &deployment.node_id,
        NodeMessage::DeploymentChanged {
            deployment_id: deployment.deployment_id.clone(),
            plugin_instance_id: deployment.plugin_instance_id.as_str().to_string(),
            desired: deployment.desired.as_str().to_string(),
            revision: deployment.revision,
        },
    )
    .await;
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

/// Canonical plugin keys with a present deployment on `node_id`.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn present_plugin_keys_for_node(
    store: &LibraryStore,
    node_id: &str,
) -> Result<Vec<String>> {
    let deployments = list_present_deployments_for_node(store, node_id).await?;
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

/// Loads the observation for one deployment on one node.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_observation(
    store: &LibraryStore,
    deployment_id: &str,
    node_id: &str,
) -> Result<Option<DeploymentObservation>> {
    let row = plugin_deployment_observations::Entity::find_by_id((
        deployment_id.to_string(),
        node_id.to_string(),
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
        node_id: Set(observation.node_id.clone()),
        incarnation: Set(observation.incarnation.clone()),
        status: Set(observation.status.as_str().to_string()),
        detail: Set(detail),
        applied_config_revision: Set(observation.applied_config_revision),
        observed_at: Set(observation.observed_at.clone()),
        // Recorded separately after a successful spawn; never reset here.
        reserved_memory_bytes: sea_orm::ActiveValue::NotSet,
        reserved_cpu_millicores: sea_orm::ActiveValue::NotSet,
    };
    plugin_deployment_observations::Entity::insert(model)
        .on_conflict(
            OnConflict::columns([
                plugin_deployment_observations::Column::DeploymentId,
                plugin_deployment_observations::Column::NodeId,
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

/// Finds the unique `(instance, node)` row.
///
/// # Errors
///
/// Returns an error when the read fails.
async fn find_deployment_pair(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    node_id: &str,
) -> Result<Option<PluginDeployment>> {
    let row = plugin_deployments::Entity::find()
        .filter(plugin_deployments::Column::PluginInstanceId.eq(instance_id.as_str()))
        .filter(plugin_deployments::Column::NodeId.eq(node_id))
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
        node_id: row.node_id,
        desired: DesiredState::parse(&row.desired)?,
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
        node_id: row.node_id,
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
fn deployment_hash(
    deployment_id: &str,
    expected_revision: i64,
    desired: &str,
    actor: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        deployment_id,
        desired,
        &expected_revision.to_string(),
        actor,
        REPLACE_DEPLOYMENT_KIND,
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0xff]);
    }
    hex::encode(hasher.finalize())
}

/// Who may create a row in each state: operators and bootstrap enrollment
/// write the operator states, the scheduler writes its own, and enrollment
/// may record a scheduler-owned instance as `unplaced` on a new node.
fn authorize_create(actor: &ConfigActor, desired: DesiredState) -> Result<()> {
    let allowed = match actor {
        ConfigActor::Operator { .. } => !desired.is_scheduled(),
        ConfigActor::Bootstrap => desired != DesiredState::Placed,
        ConfigActor::Scheduler => desired.is_scheduled(),
        ConfigActor::Administrator { .. }
        | ConfigActor::Member { .. }
        | ConfigActor::NodeOwner(_) => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(unauthorized_state(actor, desired))
    }
}

/// Operators replace with operator states, the scheduler with its own.
fn authorize_replace(actor: &ConfigActor, desired: DesiredState) -> Result<()> {
    let allowed = match actor {
        ConfigActor::Operator { .. } => !desired.is_scheduled(),
        ConfigActor::Scheduler => desired.is_scheduled(),
        ConfigActor::Bootstrap
        | ConfigActor::Administrator { .. }
        | ConfigActor::Member { .. }
        | ConfigActor::NodeOwner(_) => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(unauthorized_state(actor, desired))
    }
}

/// Error for an actor writing a state it does not own.
fn unauthorized_state(actor: &ConfigActor, desired: DesiredState) -> LibraryError {
    match actor {
        ConfigActor::Operator { .. } | ConfigActor::Scheduler | ConfigActor::Bootstrap => {
            LibraryError::Forbidden(format!(
                "unauthorized configuration write: {} may not set a deployment `{desired}`",
                actor.audit_id()
            ))
        }
        ConfigActor::Administrator { .. }
        | ConfigActor::Member { .. }
        | ConfigActor::NodeOwner(_) => unauthorized(),
    }
}

/// Rejects an empty or oversized node id.
fn validate_node_id(node_id: &str) -> Result<()> {
    if node_id.is_empty() || node_id.len() > 128 || node_id.contains('\u{0000}') {
        return Err(invalid("node id must be 1..=128 characters"));
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
    LibraryError::Forbidden(
        "unauthorized configuration write: operator authority is required".into(),
    )
}

/// Error for a deployment value outside this slice.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid configuration: {detail}"))
}

/// JSON stored on the deployment receipt.
#[derive(Debug, Deserialize)]
struct ReceiptPayload {
    /// Revision observed when the receipt was written.
    revision: i64,
}
