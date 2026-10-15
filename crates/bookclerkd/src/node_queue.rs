//! Consumer side of this node's `node.<node_id>` queue.
//!
//! Deployment and configuration writes (from this daemon, another node, or
//! the CLI) publish a [`NodeMessage`] onto the target node's queue. The
//! daemon registers itself as that queue's only consumer, and each delivery
//! wakes the deployment or configuration reconciler instead of a fixed 5 s
//! re-read. Reconcilers still run one slow backstop pass so a message
//! dispatched while this node's catalog row was not live is not lost for
//! long.

use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use bookclerk_library::control_plane::{node_consumer_id, node_queue_name, NodeMessage};
use bookclerk_library::{
    DomainEventRecord, EventCatalogSubscription, EventDeliveryRecord, LibraryStore,
};
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::api::AppState;

/// Backstop reconcile cadence when no node-queue wakeup arrives.
pub const RECONCILE_BACKSTOP: Duration = Duration::from_secs(60);

/// Wakes the deployment reconciler.
static DEPLOYMENT_WAKEUP: LazyLock<Notify> = LazyLock::new(Notify::new);
/// Wakes the configuration (`core.events`) reconciler.
static CONFIG_WAKEUP: LazyLock<Notify> = LazyLock::new(Notify::new);
/// Wakes the placement scheduler (it acts only while holding the lease).
static PLACEMENT_WAKEUP: LazyLock<Notify> = LazyLock::new(Notify::new);
/// This process's node id, read once from `node-identity.json`.
static NODE_ID: OnceLock<String> = OnceLock::new();

/// Waits until a deployment wakeup arrives (a stored permit returns at once).
pub async fn deployment_wakeup() {
    DEPLOYMENT_WAKEUP.notified().await;
}

/// Waits until a configuration wakeup arrives.
pub async fn config_wakeup() {
    CONFIG_WAKEUP.notified().await;
}

/// Waits until a placement wakeup arrives.
pub async fn placement_wakeup() {
    PLACEMENT_WAKEUP.notified().await;
}

/// Wakes the reconcilers a message concerns. `notify_one` stores a permit,
/// so a wakeup that lands mid-pass runs one more pass afterwards.
pub fn wake_for(message: &NodeMessage) {
    match message {
        NodeMessage::DeploymentChanged { .. } => {
            DEPLOYMENT_WAKEUP.notify_one();
        }
        NodeMessage::InstallChanged { .. } => {
            // A new install can add or drop targets, so placements may move.
            DEPLOYMENT_WAKEUP.notify_one();
            PLACEMENT_WAKEUP.notify_one();
        }
        NodeMessage::PlacementChanged { .. } => {
            PLACEMENT_WAKEUP.notify_one();
        }
        NodeMessage::ConfigurationChanged { .. } => {
            // Cluster settings feed the config reconciler; instance documents
            // change what a running guest was spawned with.
            CONFIG_WAKEUP.notify_one();
            DEPLOYMENT_WAKEUP.notify_one();
        }
        NodeMessage::NodeRevoked { node_id } => {
            // The owner node holding this host's socket closes it (4403).
            debug!(%node_id, "host node credential revoked");
            crate::node_api::ws::close_node_sessions(node_id, bookclerk_node_proto::CLOSE_REVOKED);
        }
    }
}

/// This node's id, or `None` when the identity file cannot be read.
pub fn node_id(files_dir: &std::path::Path) -> Option<String> {
    if let Some(id) = NODE_ID.get() {
        return Some(id.clone());
    }
    match bookclerk_library::control_plane::load_or_create_node_identity(files_dir) {
        Ok(identity) => Some(NODE_ID.get_or_init(|| identity.node_id).clone()),
        Err(err) => {
            warn!(error = %err, "node identity unavailable; node queue consumer disabled");
            None
        }
    }
}

/// Catalog id this node claims its own queue's deliveries under.
pub fn consumer_id(files_dir: &std::path::Path) -> Option<String> {
    node_id(files_dir).map(|id| node_consumer_id(&id))
}

/// Heartbeats this node's consumer row: one subscription to `node.<node_id>`.
pub async fn upsert_catalog_row(library: &LibraryStore, files_dir: &std::path::Path) {
    let Some(node_id) = node_id(files_dir) else {
        return;
    };
    let subs = [EventCatalogSubscription::queue(node_queue_name(&node_id))];
    if let Err(err) = library
        .upsert_event_subscriber(&node_id, &node_consumer_id(&node_id), &subs, true)
        .await
    {
        warn!(error = %err, "node queue consumer catalog upsert failed");
    }
}

/// Handles one claimed delivery for this node's consumer: wake the matching
/// reconciler and ack. A body that is not a [`NodeMessage`] is dead-lettered
/// (a newer node may publish a type this one does not know; the backstop
/// pass still converges).
pub async fn run_node_delivery(
    library: &LibraryStore,
    event: &DomainEventRecord,
    delivery: &EventDeliveryRecord,
) {
    let fence = delivery.fence();
    match serde_json::from_str::<NodeMessage>(&event.payload) {
        Ok(message) => {
            debug!(event_id = %event.id, ?message, "node queue wakeup");
            wake_for(&message);
            if let Err(err) = library.ack_event_delivery(&fence).await {
                warn!(delivery_id = %delivery.id, error = %err, "node queue ack failed");
            }
        }
        Err(err) => {
            warn!(event_id = %event.id, error = %err, "unknown node queue message");
            let _ = library
                .dead_letter_event_delivery(&fence, &format!("unknown node message: {err}"))
                .await;
        }
    }
}

/// Runs `pass` now, then again after each wakeup or [`RECONCILE_BACKSTOP`].
pub fn spawn_woken_loop<F, Fut>(state: Arc<AppState>, wait: fn() -> WakeFuture, mut pass: F)
where
    F: FnMut(Arc<AppState>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    tokio::spawn(async move {
        loop {
            pass(Arc::clone(&state)).await;
            tokio::select! {
                () = wait() => {}
                () = tokio::time::sleep(RECONCILE_BACKSTOP) => {}
            }
        }
    });
}

/// Boxed wakeup future for [`spawn_woken_loop`].
pub type WakeFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// [`deployment_wakeup`] as a [`WakeFuture`].
pub fn deployment_wait() -> WakeFuture {
    Box::pin(deployment_wakeup())
}

/// [`config_wakeup`] as a [`WakeFuture`].
pub fn config_wait() -> WakeFuture {
    Box::pin(config_wakeup())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::control_plane::publish_node_message;
    use bookclerk_library::{catalog_subscribers_for_event, PublishDomainEventSpec};

    async fn memory_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        )
    }

    /// Dispatches `event_id` to live subscribers and claims the delivery as
    /// `consumer`.
    async fn dispatch_and_claim(
        store: &LibraryStore,
        event_id: &str,
        consumer: &str,
    ) -> (DomainEventRecord, EventDeliveryRecord) {
        let event = store
            .get_domain_event(event_id)
            .await
            .expect("read")
            .expect("row");
        let catalog = store.list_live_event_subscribers().await.expect("catalog");
        let subscribers = catalog_subscribers_for_event(&catalog, &event);
        assert_eq!(subscribers.len(), 1, "only this node consumes its queue");
        store
            .dispatch_event_deliveries(&event.id, &subscribers, &uuid::Uuid::new_v4().to_string())
            .await
            .expect("dispatch");
        let delivery = store
            .claim_next_event_delivery(
                "test-owner",
                60,
                &uuid::Uuid::new_v4().to_string(),
                &[consumer.to_string()],
                4,
                "node-a",
            )
            .await
            .expect("claim")
            .expect("a delivery for the node consumer");
        (event, delivery)
    }

    #[tokio::test]
    async fn node_deliveries_are_claimed_by_the_node_and_acked() {
        let store = memory_store().await;
        let node = "3f2b8c1e-7a4d-4e5f-9b0a-1c2d3e4f5a6b";
        let consumer = node_consumer_id(node);
        store
            .upsert_event_subscriber(
                "node-a",
                &consumer,
                &[EventCatalogSubscription::queue(node_queue_name(node))],
                true,
            )
            .await
            .expect("catalog");
        publish_node_message(
            &store,
            node,
            &NodeMessage::DeploymentChanged {
                deployment_id: "dep-1".into(),
                plugin_instance_id: "inst-1".into(),
                desired: "present".into(),
                revision: 1,
            },
        )
        .await
        .expect("publish");
        let row = store
            .list_domain_events_for_queue(&node_queue_name(node), 10)
            .await
            .expect("rows")
            .pop()
            .expect("one message");
        let (event, delivery) = dispatch_and_claim(&store, &row.id, &consumer).await;
        assert_eq!(delivery.plugin_id, consumer);
        run_node_delivery(&store, &event, &delivery).await;
        let acked = store
            .list_event_deliveries_for_queue(&node_queue_name(node), Some("acked"), 10)
            .await
            .expect("deliveries");
        assert_eq!(
            acked.len(),
            1,
            "the wakeup is acked once the reconciler is woken"
        );

        // A body this node does not understand is parked, not retried forever.
        let bogus = store
            .publish_domain_event(PublishDomainEventSpec {
                schema_version: 1,
                dedup_key: "bogus".into(),
                payload: r#"{"type":"future.message"}"#.into(),
                queue_name: node_queue_name(node),
                ..PublishDomainEventSpec::default()
            })
            .await
            .expect("publish");
        let bogus_id = match bogus {
            bookclerk_library::PublishDomainEventOutcome::Created { id } => id,
            other => panic!("unexpected outcome {other:?}"),
        };
        let (event, delivery) = dispatch_and_claim(&store, &bogus_id, &consumer).await;
        run_node_delivery(&store, &event, &delivery).await;
        let parked = store
            .list_event_deliveries_for_queue(&node_queue_name(node), Some("dead_letter"), 10)
            .await
            .expect("deliveries");
        assert_eq!(parked.len(), 1);
    }

    #[tokio::test]
    async fn deployment_and_configuration_messages_wake_the_right_loops() {
        wake_for(&NodeMessage::DeploymentChanged {
            deployment_id: "d".into(),
            plugin_instance_id: "i".into(),
            desired: "present".into(),
            revision: 2,
        });
        tokio::time::timeout(Duration::from_secs(1), deployment_wakeup())
            .await
            .expect("deployment permit stored");
        wake_for(&NodeMessage::ConfigurationChanged {
            scope_type: "cluster".into(),
            scope_id: "c".into(),
            namespace: "core.events".into(),
            revision: 3,
        });
        tokio::time::timeout(Duration::from_secs(1), config_wakeup())
            .await
            .expect("config permit stored");
        tokio::time::timeout(Duration::from_secs(1), deployment_wakeup())
            .await
            .expect("instance documents also wake the deployment loop");
    }
}
