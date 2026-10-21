//! On the owner: what a Host node may read about the plugins deployed to
//! it (H-4 design, section 3; step 8).
//!
//! A Host acts on its `node.<id>` wakes by reading back what changed. The
//! node API serves those reads from here, always scoped to deployments
//! that target the Host and should run there:
//!
//! - [`remote_deployments_for_node`]: the deployments, each with its
//!   instance's plugin and that plugin's active version (manifest, settings
//!   schema, artifacts, and the cluster operator's approved grant);
//! - [`remote_instance_for_node`]: one deployed instance, for its
//!   `CONFIG` and `SECRETS`;
//! - [`artifact_served_to_node`]: whether an archive stored in the library
//!   belongs to a version the Host runs;
//! - [`record_remote_observation`]: the Host's observation of one of its
//!   deployments.
//!
//! A node trusted with nothing ([`super::NodeTrust::Nothing`]: revoked,
//! ownerless, its owner disabled) reads nothing.

use super::deployments::{
    list_present_deployments_for_node, load_deployment, upsert_observation, DeploymentObservation,
    DeploymentStatus, PluginDeployment,
};
use super::instances::{default_instance_for_key, load_plugin_instance, PluginInstanceId};
use super::trust::{node_trust, Claimant, NodeTrust};
use super::versions::{load_active_version, PluginVersion, ARTIFACT_BLOB_SCHEME};
use crate::error::Result;
use crate::store::LibraryStore;

/// One deployment a Host should run, with what it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDeploymentEntry {
    /// The desired row (targets the Host and should run).
    pub deployment: PluginDeployment,
    /// The instance's plugin key.
    pub plugin_key: String,
    /// True when the instance is its plugin's default instance, which
    /// unpinned jobs use.
    pub default_instance: bool,
    /// The plugin's active version; `None` when nothing is installed.
    pub version: Option<PluginVersion>,
}

/// The deployments that target `node_id` and should run, in deployment-id
/// order. Empty for a node trusted with nothing.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn remote_deployments_for_node(
    store: &LibraryStore,
    node_id: &str,
) -> Result<Vec<RemoteDeploymentEntry>> {
    if node_trust(store, node_id, Claimant::Remote).await? == NodeTrust::Nothing {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for deployment in list_present_deployments_for_node(store, node_id).await? {
        let Some(instance) = load_plugin_instance(store, &deployment.plugin_instance_id).await?
        else {
            continue;
        };
        let default_instance = default_instance_for_key(store, &instance.plugin_key)
            .await?
            .is_some_and(|default| default.id == instance.id);
        let version = load_active_version(store, &instance.plugin_key).await?;
        entries.push(RemoteDeploymentEntry {
            deployment,
            plugin_key: instance.plugin_key,
            default_instance,
            version,
        });
    }
    Ok(entries)
}

/// A deployed instance a Host asks about, with the node's trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstance {
    /// The deployment on the Host.
    pub deployment: PluginDeployment,
    /// The instance's plugin key.
    pub plugin_key: String,
    /// The plugin's active version.
    pub version: Option<PluginVersion>,
    /// What the node is trusted with now. Instance secrets belong to the
    /// cluster's operator (instances carry no owner yet), so only
    /// [`NodeTrust::All`] receives them.
    pub trust: NodeTrust,
}

/// `instance_id` when it has a deployment on `node_id` that should run and
/// the node is trusted with something; `None` otherwise (an unknown
/// instance and one deployed elsewhere read the same).
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn remote_instance_for_node(
    store: &LibraryStore,
    node_id: &str,
    instance_id: &str,
) -> Result<Option<RemoteInstance>> {
    let Ok(id) = PluginInstanceId::parse(instance_id) else {
        return Ok(None);
    };
    let trust = node_trust(store, node_id, Claimant::Remote).await?;
    if trust == NodeTrust::Nothing {
        return Ok(None);
    }
    let Some(deployment) = list_present_deployments_for_node(store, node_id)
        .await?
        .into_iter()
        .find(|deployment| deployment.plugin_instance_id == id)
    else {
        return Ok(None);
    };
    let Some(instance) = load_plugin_instance(store, &id).await? else {
        return Ok(None);
    };
    let version = load_active_version(store, &instance.plugin_key).await?;
    Ok(Some(RemoteInstance {
        deployment,
        plugin_key: instance.plugin_key,
        version,
        trust,
    }))
}

/// True when the archive `archive_sha256` is stored in the library and is
/// an artifact of the active version of a plugin deployed to `node_id`.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn artifact_served_to_node(
    store: &LibraryStore,
    node_id: &str,
    archive_sha256: &str,
) -> Result<bool> {
    let blob_url = format!("{ARTIFACT_BLOB_SCHEME}{archive_sha256}");
    Ok(remote_deployments_for_node(store, node_id)
        .await?
        .iter()
        .filter_map(|entry| entry.version.as_ref())
        .flat_map(|version| version.artifacts.iter())
        .any(|artifact| artifact.archive_sha256 == archive_sha256 && artifact.url == blob_url))
}

/// Statuses a Host may report: it materializes owner plugins and reports
/// errors (`awaiting_host_consent` among them); it keeps no guest running
/// between jobs, so `running` and `healthy` are not its to claim.
fn host_reportable(status: DeploymentStatus) -> bool {
    matches!(
        status,
        DeploymentStatus::Materialized | DeploymentStatus::Error
    )
}

/// One observation a Host reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObservation<'a> {
    /// Deployment id.
    pub deployment_id: &'a str,
    /// Observed status.
    pub status: DeploymentStatus,
    /// Detail (bounded on write).
    pub detail: &'a str,
    /// Config revision the Host applied, if any.
    pub applied_config_revision: Option<i64>,
}

/// Records a Host's observation of one of its deployments. Answers `false`
/// (and writes nothing) for a deployment that does not target the node, a
/// status a Host may not report, or a node trusted with nothing.
///
/// # Errors
///
/// Returns an error when a read or the write fails.
pub async fn record_remote_observation(
    store: &LibraryStore,
    node_id: &str,
    incarnation: &str,
    observation: &RemoteObservation<'_>,
) -> Result<bool> {
    if !host_reportable(observation.status) {
        return Ok(false);
    }
    if node_trust(store, node_id, Claimant::Remote).await? == NodeTrust::Nothing {
        return Ok(false);
    }
    let Some(deployment) = load_deployment(store, observation.deployment_id).await? else {
        return Ok(false);
    };
    if deployment.node_id != node_id {
        return Ok(false);
    }
    upsert_observation(
        store,
        &DeploymentObservation {
            deployment_id: deployment.deployment_id,
            node_id: node_id.to_string(),
            incarnation: incarnation.to_string(),
            status: observation.status,
            detail: observation.detail.to_string(),
            applied_config_revision: observation.applied_config_revision,
            observed_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .await?;
    Ok(true)
}
