//! This host's periodic heartbeat into `host_status`.
//!
//! Every [`HOST_HEARTBEAT_INTERVAL`] the daemon samples detected capacity and
//! measured use, and the library adds this host's budget (`host.runtime`),
//! the reservations of its running guests, and the availability average.
//! Other hosts and the placement scheduler read the row from the shared
//! database; hosts never contact each other.

use std::sync::Arc;

use bookclerk_library::control_plane::{
    load_or_create_host_identity, record_host_heartbeat, HostSampler, HostStatus,
    HOST_HEARTBEAT_INTERVAL,
};
use tracing::warn;

use crate::api::AppState;

/// Starts the heartbeat loop. The first heartbeat is written at once.
pub fn spawn_heartbeat(state: Arc<AppState>) {
    tokio::spawn(async move {
        let files_dir = state.config.read().await.paths().files_dir.clone();
        let mut sampler = Some(HostSampler::new(&files_dir));
        loop {
            if let Some(taken) = sampler.take() {
                let (back, status) = beat(&state, taken).await;
                sampler = Some(back);
                if let Err(err) = status {
                    warn!(error = %err, "host heartbeat failed");
                }
            }
            tokio::time::sleep(HOST_HEARTBEAT_INTERVAL).await;
        }
    });
}

/// Samples on a blocking thread, then records the heartbeat.
async fn beat(
    state: &AppState,
    mut sampler: HostSampler,
) -> (HostSampler, anyhow::Result<HostStatus>) {
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
                HostSampler::new(&files_dir),
                Err(anyhow::anyhow!("heartbeat sample panicked: {err}")),
            );
        }
    };
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let host_id = match load_or_create_host_identity(&files_dir) {
        Ok(identity) => identity.host_id,
        Err(err) => return (sampler, Err(err.into())),
    };
    let library = state.library_snapshot().await;
    let status = record_host_heartbeat(&library, &host_id, &heartbeat)
        .await
        .map_err(anyhow::Error::from);
    (sampler, status)
}
