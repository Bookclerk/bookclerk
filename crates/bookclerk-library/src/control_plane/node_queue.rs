//! Per-node queues (`node.<node_id>`) that wake node subsystems.
//!
//! Every write that changes what a node should run publishes one small JSON
//! message onto that node's queue on the outbox: a deployment write goes to
//! the deployment's node, a configuration document write goes to every
//! registered node. The daemon registers itself as the only consumer of its
//! own queue and wakes the deployment and configuration reconcilers on each
//! delivery instead of re-reading the database on a fixed tick. Any process
//! that writes (the CLI included) is a producer only; consuming is the
//! daemon's job.
//!
//! Messages are wakeups, not state: a reconciler always re-reads the
//! authoritative rows, so a lost or duplicated message costs at most one
//! extra pass, and a slow backstop pass covers a node whose catalog row was
//! not live when the message was dispatched.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::models::{is_valid_queue_name, PublishDomainEventSpec, NODE_QUEUE_PREFIX};
use crate::LibraryStore;

/// Catalog `plugin_id` prefix of a node's own consumer (`node:<node_id>`).
pub const NODE_CONSUMER_PREFIX: &str = "node:";

/// One message on a `node.<node_id>` queue. The body is the JSON of this
/// enum (`{"type": "deployment.changed", ...}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NodeMessage {
    /// A deployment targeting this node was created, replaced, or deleted.
    #[serde(rename = "deployment.changed", rename_all = "camelCase")]
    DeploymentChanged {
        /// Deployment row id.
        deployment_id: String,
        /// Instance the deployment runs.
        plugin_instance_id: String,
        /// `present`, `absent`, or `deleted`.
        desired: String,
        /// Revision after the write (0 for a delete).
        revision: i64,
    },
    /// The active version of a plugin moved (install, update, rollback,
    /// uninstall) or its consent grant changed.
    #[serde(rename = "install.changed", rename_all = "camelCase")]
    InstallChanged {
        /// Canonical plugin key.
        plugin_key: String,
        /// Active version; empty when the pointer was cleared.
        version: String,
        /// Pointer revision (0 for a grant change or a clear).
        revision: i64,
    },
    /// Placement intent, or a fact it depends on, changed: an instance's
    /// replicas or constraints, or a node's labels. Only the node holding
    /// the scheduler lease acts on it; every other node ignores it.
    #[serde(rename = "placement.changed", rename_all = "camelCase")]
    PlacementChanged {
        /// `instance:<id>` or `node:<id>`.
        subject: String,
        /// Placement revision (0 for a node label change or a delete).
        revision: i64,
    },
    /// A configuration document changed (cluster settings or an instance
    /// document).
    #[serde(rename = "configuration.changed", rename_all = "camelCase")]
    ConfigurationChanged {
        /// Document scope type.
        scope_type: String,
        /// Document scope id.
        scope_id: String,
        /// Document namespace.
        namespace: String,
        /// Revision after the write.
        revision: i64,
    },
}

impl NodeMessage {
    /// Stable dedup key: a retried write that commits the same revision does
    /// not publish twice.
    fn dedup_key(&self) -> String {
        match self {
            Self::DeploymentChanged {
                deployment_id,
                desired,
                revision,
                ..
            } => format!("deployment.changed:{deployment_id}:{desired}:{revision}"),
            Self::ConfigurationChanged {
                scope_type,
                scope_id,
                namespace,
                revision,
            } => format!("configuration.changed:{scope_type}:{scope_id}:{namespace}:{revision}"),
            Self::PlacementChanged { subject, revision } => format!(
                "placement.changed:{subject}:{revision}:{}",
                uuid::Uuid::new_v4()
            ),
            // Grant approvals and clears carry revision 0; a fresh id keeps
            // each one a distinct wakeup.
            Self::InstallChanged {
                plugin_key,
                version,
                revision,
            } => format!(
                "install.changed:{plugin_key}:{version}:{revision}:{}",
                uuid::Uuid::new_v4()
            ),
        }
    }
}

/// Queue a node consumes: `node.<node_id>`. Registration accepts only node
/// ids for which this is a valid queue name ([`is_valid_node_id`]).
#[must_use]
pub fn node_queue_name(node_id: &str) -> String {
    format!("{NODE_QUEUE_PREFIX}{node_id}")
}

/// True when `node_id` can name a node: `node.<node_id>` is a valid queue
/// name. Minted ids are lowercase UUIDs, which always qualify.
#[must_use]
pub fn is_valid_node_id(node_id: &str) -> bool {
    !node_id.is_empty() && is_valid_queue_name(&node_queue_name(node_id))
}

/// Catalog id of a node's own consumer (`node:<node_id>`).
#[must_use]
pub fn node_consumer_id(node_id: &str) -> String {
    format!("{NODE_CONSUMER_PREFIX}{node_id}")
}

/// True when a catalog / delivery `plugin_id` is a node consumer rather than
/// a plugin.
#[must_use]
pub fn is_node_consumer_id(plugin_id: &str) -> bool {
    plugin_id.starts_with(NODE_CONSUMER_PREFIX)
}

/// Publishes `message` onto `node.<node_id>`.
///
/// # Errors
///
/// Returns an error when the outbox insert fails.
pub async fn publish_node_message(
    store: &LibraryStore,
    node_id: &str,
    message: &NodeMessage,
) -> Result<()> {
    let payload = serde_json::to_string(message).map_err(|err| {
        crate::error::LibraryError::Other(anyhow::anyhow!("node message encode: {err}"))
    })?;
    store
        .publish_domain_event(PublishDomainEventSpec {
            schema_version: 1,
            dedup_key: message.dedup_key(),
            payload,
            queue_name: node_queue_name(node_id),
            ..PublishDomainEventSpec::default()
        })
        .await?;
    Ok(())
}

/// Publishes `message` onto every registered node's queue.
///
/// # Errors
///
/// Returns an error when the node list or an outbox insert fails.
pub async fn publish_to_all_nodes(store: &LibraryStore, message: &NodeMessage) -> Result<()> {
    for node_id in super::list_node_ids(store).await? {
        publish_node_message(store, &node_id, message).await?;
    }
    Ok(())
}

/// Best-effort wakeup after a committed write: a failure is logged, never
/// returned, because the write itself already succeeded and the
/// reconciler's backstop pass picks the change up anyway.
pub(crate) async fn notify_node(store: &LibraryStore, node_id: &str, message: NodeMessage) {
    if let Err(err) = publish_node_message(store, node_id, &message).await {
        tracing::warn!(node_id, error = %err, "node queue wakeup publish failed");
    }
}

/// Best-effort wakeup of every node after a committed write.
pub(crate) async fn notify_all_nodes(store: &LibraryStore, message: NodeMessage) {
    if let Err(err) = publish_to_all_nodes(store, &message).await {
        tracing::warn!(error = %err, "node queue wakeup publish failed");
    }
}
