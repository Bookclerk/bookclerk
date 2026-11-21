//! Periodic scan / auto-acquire / listening-sync / copy-retry /
//! destination-sync scheduler (queue producers).

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::ScanRequester;
use bookclerk_library::JobTrigger;
use tracing::{info, warn};

use crate::api::AppState;
use crate::job_worker::jittered_delay;
use crate::jobs::{enqueue_acquire, enqueue_listen_sync, enqueue_scan};

/// Spawn periodic library scan and optional listening-sync loops.
pub fn spawn_scheduler(state: Arc<AppState>) {
    spawn_scan_loop(state.clone());
    spawn_listen_sync_loop(state.clone());
    spawn_copy_retry_loop(state.clone());
    crate::destination_sync::spawn_destination_sync_loop(state.clone());
    spawn_discover_refresh_loop(state);
}

/// How often due copy retries are looked for. A copy's first retry is due
/// five minutes after it failed (`bookclerk_library::COPY_RETRY_BASE_DELAY`).
const COPY_RETRY_CHECK: Duration = Duration::from_secs(5 * 60);

/// Every few minutes, queues an acquire for each edition whose failed copy
/// on some destination is due for another attempt.
fn spawn_copy_retry_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(jittered_delay(COPY_RETRY_CHECK)).await;
            match crate::jobs::queue_copy_retries(state.clone()).await {
                Ok(0) => {}
                Ok(queued) => info!(queued, "copy retries enqueued"),
                Err(err) => warn!(error = %err, "copy retry enqueue failed"),
            }
        }
    });
}

/// How often stored Discover data is checked for the daily refresh.
const DISCOVER_REFRESH_CHECK: Duration = Duration::from_secs(60 * 60);

/// Hourly, queues a candidate fetch for every subject with a stored feed
/// whose candidates are older than a day (prices and deals drift).
fn spawn_discover_refresh_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(jittered_delay(DISCOVER_REFRESH_CHECK)).await;
            crate::discover_jobs::queue_daily_refresh(&state).await;
        }
    });
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

            match enqueue_scan(
                state.clone(),
                ScanRequester::Operator,
                None,
                None,
                JobTrigger::Scheduler,
            )
            .await
            {
                Ok(admit) => info!(?admit, "scheduled scan enqueued"),
                Err(err) => warn!(error = %err, "scheduled scan enqueue failed"),
            }

            let auto = {
                let cfg = state.config.read().await;
                cfg.library.auto_acquire
            };
            if auto {
                match enqueue_acquire(
                    state.clone(),
                    ScanRequester::Operator,
                    // Auto-acquire: every download waits for its account's pace.
                    crate::jobs::AcquireAsk {
                        paced: true,
                        ..crate::jobs::AcquireAsk::default()
                    },
                    JobTrigger::Scheduler,
                )
                .await
                {
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
