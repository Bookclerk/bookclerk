//! Per-host queues (`host.<host_id>`) that wake host subsystems.
//!
//! Every write that changes what a host should run publishes one small JSON
//! message onto that host's queue on the outbox: a deployment write goes to
//! the deployment's host, a configuration document write goes to every
//! registered host. The daemon registers itself as the only consumer of its
//! own queue and wakes the deployment and configuration reconcilers on each
//! delivery instead of re-reading the database on a fixed tick. Any process
//! that writes (the CLI included) is a producer only; consuming is the
//! daemon's job.
//!
//! Messages are wakeups, not state: a reconciler always re-reads the
//! authoritative rows, so a lost or duplicated message costs at most one
//! extra pass, and a slow backstop pass covers a host whose catalog row was
//! not live when the message was dispatched.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::models::{is_valid_queue_name, PublishDomainEventSpec, HOST_QUEUE_PREFIX};
use crate::LibraryStore;

/// Catalog `plugin_id` prefix of a host's own consumer (`host:<host_id>`).
pub const HOST_CONSUMER_PREFIX: &str = "host:";

/// One message on a `host.<host_id>` queue. The body is the JSON of this
/// enum (`{"type": "deployment.changed", ...}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HostMessage {
    /// A deployment targeting this host was created, replaced, or deleted.
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
    /// The active release of a plugin moved (install, update, rollback,
    /// remove) or its consent grant changed.
    #[serde(rename = "release.changed", rename_all = "camelCase")]
    ReleaseChanged {
        /// Canonical plugin key.
        plugin_key: String,
        /// Active version; empty when the pointer was cleared.
        version: String,
        /// Pointer revision (0 for a grant change or a clear).
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

impl HostMessage {
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
            // Grant approvals and clears carry revision 0; a fresh id keeps
            // each one a distinct wakeup.
            Self::ReleaseChanged {
                plugin_key,
                version,
                revision,
            } => format!(
                "release.changed:{plugin_key}:{version}:{revision}:{}",
                uuid::Uuid::new_v4()
            ),
        }
    }
}

/// Queue a host consumes: `host.<host_id>` when the id fits the queue
/// grammar (host ids are UUIDs), else `host.h-<sha256 prefix>` so any
/// host id still maps to one stable, valid queue.
#[must_use]
pub fn host_queue_name(host_id: &str) -> String {
    let direct = format!("{HOST_QUEUE_PREFIX}{}", host_id.to_ascii_lowercase());
    if is_valid_queue_name(&direct) {
        return direct;
    }
    let digest = Sha256::digest(host_id.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("{HOST_QUEUE_PREFIX}h-{hex}")
}

/// Catalog id of a host's own consumer (`host:<host_id>`).
#[must_use]
pub fn host_consumer_id(host_id: &str) -> String {
    format!("{HOST_CONSUMER_PREFIX}{host_id}")
}

/// True when a catalog / delivery `plugin_id` is a host consumer rather than
/// a plugin.
#[must_use]
pub fn is_host_consumer_id(plugin_id: &str) -> bool {
    plugin_id.starts_with(HOST_CONSUMER_PREFIX)
}

/// Publishes `message` onto `host.<host_id>`.
///
/// # Errors
///
/// Returns an error when the outbox insert fails.
pub async fn publish_host_message(
    store: &LibraryStore,
    host_id: &str,
    message: &HostMessage,
) -> Result<()> {
    let payload = serde_json::to_string(message).map_err(|err| {
        crate::error::LibraryError::Other(anyhow::anyhow!("host message encode: {err}"))
    })?;
    store
        .publish_domain_event(PublishDomainEventSpec {
            schema_version: 1,
            dedup_key: message.dedup_key(),
            payload,
            queue_name: host_queue_name(host_id),
            ..PublishDomainEventSpec::default()
        })
        .await?;
    Ok(())
}

/// Publishes `message` onto every registered host's queue.
///
/// # Errors
///
/// Returns an error when the host list or an outbox insert fails.
pub async fn publish_to_all_hosts(store: &LibraryStore, message: &HostMessage) -> Result<()> {
    for host_id in super::list_host_ids(store).await? {
        publish_host_message(store, &host_id, message).await?;
    }
    Ok(())
}

/// Best-effort wakeup after a committed write: a failure is logged, never
/// returned, because the write itself already succeeded and the
/// reconciler's backstop pass picks the change up anyway.
pub(crate) async fn notify_host(store: &LibraryStore, host_id: &str, message: HostMessage) {
    if let Err(err) = publish_host_message(store, host_id, &message).await {
        tracing::warn!(host_id, error = %err, "host queue wakeup publish failed");
    }
}

/// Best-effort wakeup of every host after a committed write.
pub(crate) async fn notify_all_hosts(store: &LibraryStore, message: HostMessage) {
    if let Err(err) = publish_to_all_hosts(store, &message).await {
        tracing::warn!(error = %err, "host queue wakeup publish failed");
    }
}
