//! The placement scheduler: turns `plugin_placements` intent into
//! deployments.
//!
//! Every daemon runs this loop, but only the one holding the cluster's
//! `scheduler` lease acts; the others find the lease held and wait. The
//! holder renews it on every pass, so a dead holder's lease expires and the
//! next daemon to try takes over with a larger fencing token. A pass runs on
//! a `placement.changed` or `release.changed` wakeup and at least every
//! [`PLACEMENT_INTERVAL`], which also notices hosts whose daemons stopped
//! heartbeating.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    acquire_lease, schedule_placements, ScheduleReport, SCHEDULER_LEASE, SCHEDULER_LEASE_TTL,
};
use tracing::{debug, info, warn};

use crate::api::AppState;

/// Longest gap between scheduler passes; well inside
/// [`SCHEDULER_LEASE_TTL`] so the holder keeps its lease.
pub const PLACEMENT_INTERVAL: Duration = Duration::from_secs(20);

/// Starts the scheduler loop.
pub fn spawn_placement_scheduler(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            run_placement_pass(&state).await;
            tokio::select! {
                () = crate::host_queue::placement_wakeup() => {}
                () = tokio::time::sleep(PLACEMENT_INTERVAL) => {}
            }
        }
    });
}

/// One pass: take or renew the lease, then schedule. Returns `None` when
/// another host holds the lease or the pass failed.
pub async fn run_placement_pass(state: &AppState) -> Option<ScheduleReport> {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let host_id = crate::host_queue::host_id(&files_dir)?;
    let library = state.library_snapshot().await;
    match acquire_lease(&library, SCHEDULER_LEASE, &host_id, SCHEDULER_LEASE_TTL).await {
        Ok(Some(lease)) => debug!(token = lease.token, "holding the scheduler lease"),
        Ok(None) => return None,
        Err(err) => {
            warn!(error = %err, "scheduler lease unavailable");
            return None;
        }
    }
    match schedule_placements(&library).await {
        Ok(report) => {
            if report.added + report.removed + report.conflicts > 0 {
                info!(
                    placements = report.placements,
                    added = report.added,
                    removed = report.removed,
                    conflicts = report.conflicts,
                    "placement pass changed deployments"
                );
            }
            Some(report)
        }
        Err(err) => {
            warn!(error = %err, "placement pass failed");
            None
        }
    }
}
