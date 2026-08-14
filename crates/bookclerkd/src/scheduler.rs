//! Periodic scan / auto-acquire / listening-sync scheduler (queue producers).

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::JobTrigger;
use tracing::{info, warn};

use crate::api::AppState;
use crate::job_worker::jittered_delay;
use crate::jobs::{enqueue_acquire, enqueue_listen_sync, enqueue_scan};

/// Spawn periodic library scan and optional listening-sync loops.
pub fn spawn_scheduler(state: Arc<AppState>) {
    spawn_scan_loop(state.clone());
    spawn_listen_sync_loop(state);
}

/// Periodically admits a library scan (and optional auto-acquire) with jitter.
fn spawn_scan_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let interval_mins = {
                let cfg = state.config.read().await;
                cfg.library.scan_interval_minutes
            };
            if interval_mins == 0 {
                info!("scan scheduler disabled (scan_interval_minutes = 0)");
                tokio::time::sleep(Duration::from_secs(300)).await;
                continue;
            }

            let sleep_for = jittered_delay(Duration::from_secs(interval_mins.saturating_mul(60)));
            info!(?sleep_for, "scheduler sleeping until next scan");
            tokio::time::sleep(sleep_for).await;

            match enqueue_scan(state.clone(), None, JobTrigger::Scheduler).await {
                Ok(admit) => info!(?admit, "scheduled scan enqueued"),
                Err(err) => warn!(error = %err, "scheduled scan enqueue failed"),
            }

            let auto = {
                let cfg = state.config.read().await;
                cfg.library.auto_acquire
            };
            if auto {
                match enqueue_acquire(state.clone(), None, None, JobTrigger::Scheduler).await {
                    Ok(admit) => info!(?admit, "scheduled auto-acquire enqueued"),
                    Err(err) => warn!(error = %err, "scheduled auto-acquire enqueue failed"),
                }
            }
        }
    });
}

/// Periodically admits a listening-progress sync with jitter.
fn spawn_listen_sync_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let interval_mins = {
                let cfg = state.config.read().await;
                cfg.discovery.listen_sync_interval_minutes
            };
            if interval_mins == 0 {
                info!("listening sync scheduler disabled (listen_sync_interval_minutes = 0)");
                tokio::time::sleep(Duration::from_secs(300)).await;
                continue;
            }

            let sleep_for = jittered_delay(Duration::from_secs(interval_mins.saturating_mul(60)));
            info!(?sleep_for, "scheduler sleeping until next listening sync");
            tokio::time::sleep(sleep_for).await;

            match enqueue_listen_sync(state.clone(), JobTrigger::Scheduler).await {
                Ok(admit) => info!(?admit, "scheduled listening sync enqueued"),
                Err(err) => warn!(error = %err, "scheduled listening sync enqueue failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use crate::jobs::AdmitJob;
    use bookclerk_library::{EnqueueJobSpec, JobKind, JobPayload, JobTrigger, LibraryStore};

    #[tokio::test]
    async fn scheduler_helpers_enqueue_rather_than_run() {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let first = store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Scan,
                payload: JobPayload {
                    account: None,
                    title: None,
                    trigger: JobTrigger::Scheduler,
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 8,
                run_after: None,
            })
            .await
            .unwrap();
        let bookclerk_library::EnqueueOutcome::Created { id } = first else {
            panic!("expected created");
        };
        let again = store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Scan,
                payload: JobPayload {
                    account: None,
                    title: None,
                    trigger: JobTrigger::Scheduler,
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 8,
                run_after: None,
            })
            .await
            .unwrap();
        assert_eq!(AdmitJob::from(again), AdmitJob::Duplicate(id));
    }
}
