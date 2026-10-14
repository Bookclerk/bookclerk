//! This node's periodic heartbeat into `node_status`.
//!
//! Every [`NODE_HEARTBEAT_INTERVAL`] the daemon samples detected capacity and
//! measured use, and the library adds this node's budget (`node.runtime`),
//! the reservations of its running guests, and the availability average.
//! Other nodes and the placement scheduler read the row from the shared
//! database; nodes never contact each other.

use std::sync::Arc;

use bookclerk_library::control_plane::{
    load_or_create_node_identity, record_node_heartbeat, NodeSampler, NodeStatus,
    NODE_HEARTBEAT_INTERVAL,
};
use tracing::warn;

use crate::api::AppState;

/// Starts the heartbeat loop. The first heartbeat is written at once.
pub fn spawn_heartbeat(state: Arc<AppState>) {
    tokio::spawn(async move {
        let files_dir = state.config.read().await.paths().files_dir.clone();
        let mut sampler = Some(NodeSampler::new(&files_dir));
        loop {
            if let Some(taken) = sampler.take() {
                let (back, status) = beat(&state, taken).await;
                sampler = Some(back);
                if let Err(err) = status {
                    warn!(error = %err, "node heartbeat failed");
                }
            }
            tokio::time::sleep(NODE_HEARTBEAT_INTERVAL).await;
        }
    });
}

/// Samples on a blocking thread, then records the heartbeat.
async fn beat(
    state: &AppState,
    mut sampler: NodeSampler,
) -> (NodeSampler, anyhow::Result<NodeStatus>) {
    let (sampler, heartbeat) = match tokio::task::spawn_blocking(move || {
        let heartbeat = sampler.sample();
        (sampler, heartbeat)
    })
    .await
    {
        Ok(pair) => pair,
        Err(err) => {
            let files_dir = state.config.read().await.paths().files_dir.clone();
            return (
                NodeSampler::new(&files_dir),
                Err(anyhow::anyhow!("heartbeat sample panicked: {err}")),
            );
        }
    };
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let node_id = match load_or_create_node_identity(&files_dir) {
        Ok(identity) => identity.node_id,
        Err(err) => return (sampler, Err(err.into())),
    };
    let library = state.library_snapshot().await;
    let status = record_node_heartbeat(&library, &node_id, &heartbeat)
        .await
        .map_err(anyhow::Error::from);
    (sampler, status)
}
