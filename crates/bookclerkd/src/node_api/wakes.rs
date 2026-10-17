//! Forwarding a host's `node.<host_id>` queue over its socket (H-4 design,
//! section 3, *Wakeups*).
//!
//! While a host's socket is live, the owner node holding it registers
//! itself as the consumer of that host's queue (catalog row
//! `node:<host_id>` on this owner node) and heartbeats the row with the
//! session. Each claimed delivery is sent as a `wake` frame; it is acked
//! only once the host answers `ack` with the same frame id. When the
//! session ends, deliveries still waiting for an ack are released without
//! spending an attempt, and the catalog row is disabled.
//!
//! Backpressure: a delivery is claimed only after a slot in the session's
//! bounded send queue is reserved for its frame, and no more than
//! [`SEND_QUEUE_FRAMES`] wakes wait for acks at once. When the queue is
//! full the forwarder claims nothing; the deliveries stay pending (or, if
//! claimed earlier, expire) and are redelivered later.
//!
//! Only messages a host acts on are forwarded (`deployment.changed`,
//! `install.changed`, `configuration.changed`); others are acked at once,
//! and a body that is not a node message is dead-lettered.

use std::collections::HashMap;

use axum::extract::ws::Message;
use bookclerk_library::control_plane::{
    is_valid_node_id, node_consumer_id, node_queue_name, NodeMessage,
};
use bookclerk_library::{EventCatalogSubscription, EventDeliveryFence, LibraryStore};
use bookclerk_node_proto::{Body, Frame, SEND_QUEUE_FRAMES};
use tokio::sync::mpsc;

/// Lease on a forwarded delivery; an unacked wake is redelivered after it.
pub(crate) const WAKE_LEASE_SECS: u64 = 60;

/// Prefix of a wake frame's id; the rest is the delivery id.
const WAKE_ID_PREFIX: &str = "wake:";

/// Forwards one host's queue over one connection.
pub(crate) struct WakeForwarder {
    /// This owner node (the catalog row's node).
    owner_node_id: String,
    /// The host whose queue is forwarded.
    host_id: String,
    /// Catalog `plugin_id` claimed under (`node:<host_id>`).
    consumer: String,
    /// Lease owner written on claimed deliveries.
    lease_owner: String,
    /// Wakes sent and not yet acked, by frame id.
    in_flight: HashMap<String, EventDeliveryFence>,
}

impl WakeForwarder {
    /// A forwarder for `host_id` on owner node `owner_node_id`, or `None`
    /// when the host id cannot name a queue.
    pub(crate) fn new(owner_node_id: &str, host_id: &str, serial: u64) -> Option<Self> {
        if owner_node_id.is_empty() || !is_valid_node_id(host_id) {
            return None;
        }
        Some(Self {
            owner_node_id: owner_node_id.to_string(),
            host_id: host_id.to_string(),
            consumer: node_consumer_id(host_id),
            lease_owner: format!("wake-{owner_node_id}-{serial}"),
            in_flight: HashMap::new(),
        })
    }

    /// Heartbeats (or, with `enabled = false`, disables) this owner node's
    /// consumer row for the host's queue.
    pub(crate) async fn heartbeat(&self, library: &LibraryStore, enabled: bool) {
        let subs = [EventCatalogSubscription::queue(node_queue_name(
            &self.host_id,
        ))];
        if let Err(err) = library
            .upsert_event_subscriber(&self.owner_node_id, &self.consumer, &subs, enabled)
            .await
        {
            tracing::warn!(host_id = %self.host_id, error = %err, "host queue catalog upsert failed");
        }
    }

    /// Wakes waiting for an ack.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Claims and sends as many wakes as the send queue has room for.
    /// Returns how many frames were queued.
    pub(crate) async fn forward(
        &mut self,
        library: &LibraryStore,
        outbox: &mpsc::Sender<Message>,
    ) -> usize {
        let mut sent = 0;
        while self.in_flight.len() < SEND_QUEUE_FRAMES {
            // Reserve the frame's slot before claiming, so a claimed
            // delivery always has room to go out.
            let Ok(slot) = outbox.try_reserve() else {
                break;
            };
            let claimed = library
                .claim_next_event_delivery(
                    &self.lease_owner,
                    WAKE_LEASE_SECS,
                    &uuid::Uuid::new_v4().to_string(),
                    std::slice::from_ref(&self.consumer),
                    u32::try_from(SEND_QUEUE_FRAMES).unwrap_or(u32::MAX),
                    &self.owner_node_id,
                )
                .await;
            let delivery = match claimed {
                Ok(Some(delivery)) => delivery,
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(host_id = %self.host_id, error = %err, "host queue claim failed");
                    break;
                }
            };
            let fence = delivery.fence();
            let event = match library.get_domain_event(&delivery.event_id).await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    let _ = library
                        .dead_letter_event_delivery(&fence, "parent event missing")
                        .await;
                    continue;
                }
                Err(err) => {
                    tracing::warn!(host_id = %self.host_id, error = %err, "host queue read failed");
                    let _ = library
                        .release_unexecuted_event_delivery(&fence, false)
                        .await;
                    break;
                }
            };
            let message = match serde_json::from_str::<NodeMessage>(&event.payload) {
                Ok(message) => message,
                Err(err) => {
                    let _ = library
                        .dead_letter_event_delivery(&fence, &format!("unknown node message: {err}"))
                        .await;
                    continue;
                }
            };
            if !matches!(
                message,
                NodeMessage::DeploymentChanged { .. }
                    | NodeMessage::InstallChanged { .. }
                    | NodeMessage::ConfigurationChanged { .. }
            ) {
                // Nothing a host acts on (placement runs on owner nodes;
                // revocation closes the socket through the owner's queue).
                let _ = library.ack_event_delivery(&fence).await;
                continue;
            }
            let id = format!("{WAKE_ID_PREFIX}{}", delivery.id);
            let body = Body::Wake {
                message: serde_json::to_value(&message).unwrap_or_default(),
            };
            slot.send(Message::Text(Frame::new(id.clone(), body).to_text().into()));
            self.in_flight.insert(id, fence);
            sent += 1;
        }
        sent
    }

    /// Handles the host's `ack` for frame `id`. Returns `false` for an id
    /// this connection did not send (or already acked).
    pub(crate) async fn ack(&mut self, library: &LibraryStore, id: &str) -> bool {
        let Some(fence) = self.in_flight.remove(id) else {
            return false;
        };
        match library.ack_event_delivery(&fence).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(host_id = %self.host_id, %id, "wake lease lost before ack");
            }
            Err(err) => {
                tracing::warn!(host_id = %self.host_id, error = %err, "host queue ack failed");
            }
        }
        true
    }

    /// Releases every unacked wake for prompt redelivery and disables this
    /// owner node's consumer row.
    pub(crate) async fn shutdown(mut self, library: &LibraryStore) {
        for (_, fence) in self.in_flight.drain() {
            let _ = library
                .release_unexecuted_event_delivery(&fence, false)
                .await;
        }
        self.heartbeat(library, false).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::catalog_subscribers_for_event;
    use bookclerk_library::control_plane::publish_node_message;

    const OWNER: &str = "owner-node";
    const HOST: &str = "3f2b8c1e-7a4d-4e5f-9b0a-1c2d3e4f5a6b";

    async fn memory_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        )
    }

    /// Publishes one deployment wake to the host's queue and dispatches it.
    async fn publish(store: &LibraryStore, revision: i64) {
        publish_node_message(
            store,
            HOST,
            &NodeMessage::DeploymentChanged {
                deployment_id: "dep-1".into(),
                plugin_instance_id: "inst-1".into(),
                desired: "present".into(),
                revision,
            },
        )
        .await
        .unwrap();
        let catalog = store.list_live_event_subscribers().await.unwrap();
        for event in store
            .list_domain_events_for_queue(&node_queue_name(HOST), 100)
            .await
            .unwrap()
        {
            let subscribers = catalog_subscribers_for_event(&catalog, &event);
            store
                .dispatch_event_deliveries(
                    &event.id,
                    &subscribers,
                    &uuid::Uuid::new_v4().to_string(),
                )
                .await
                .unwrap();
        }
    }

    async fn count(store: &LibraryStore, state: &str) -> usize {
        store
            .list_event_deliveries_for_queue(&node_queue_name(HOST), Some(state), 100)
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn a_full_send_queue_stops_forwarding_until_the_host_reads() {
        let store = memory_store().await;
        let mut forwarder = WakeForwarder::new(OWNER, HOST, 1).unwrap();
        forwarder.heartbeat(&store, true).await;
        publish(&store, 1).await;
        assert_eq!(count(&store, "pending").await, 1);

        // A host that stopped reading: the queue is full of other frames.
        let (outbox, mut socket) = mpsc::channel(SEND_QUEUE_FRAMES);
        for _ in 0..SEND_QUEUE_FRAMES {
            outbox.try_send(Message::Ping(Default::default())).unwrap();
        }
        assert_eq!(forwarder.forward(&store, &outbox).await, 0);
        assert_eq!(
            count(&store, "pending").await,
            1,
            "nothing is claimed while the queue is full"
        );

        // The host reads one frame: one wake goes out, leased until acked.
        socket.recv().await.unwrap();
        assert_eq!(forwarder.forward(&store, &outbox).await, 1);
        assert_eq!(count(&store, "running").await, 1);
        let mut wake = None;
        while let Ok(message) = socket.try_recv() {
            if let Message::Text(text) = message {
                wake = Some(Frame::parse(text.as_str()).unwrap());
            }
        }
        let wake = wake.expect("the wake frame was queued");
        assert!(
            matches!(&wake.body, Body::Wake { message } if message["type"] == "deployment.changed")
        );
        assert!(!forwarder.ack(&store, "wake:unknown").await);
        assert!(forwarder.ack(&store, &wake.id).await);
        assert_eq!(count(&store, "acked").await, 1);
        assert_eq!(forwarder.in_flight(), 0);

        // An unacked wake is released when the session ends.
        publish(&store, 2).await;
        assert_eq!(forwarder.forward(&store, &outbox).await, 1);
        assert_eq!(count(&store, "running").await, 1);
        forwarder.shutdown(&store).await;
        assert_eq!(count(&store, "running").await, 0);
        assert_eq!(count(&store, "pending").await, 1);
    }
}
