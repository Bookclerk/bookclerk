//! Background scan / auto-acquire / listening-sync scheduler.

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use crate::api::AppState;
use crate::jobs::{run_acquire, run_scan};

/// Spawn periodic library scan and optional listening-sync loops.
pub fn spawn_scheduler(state: Arc<AppState>) {
    spawn_scan_loop(state.clone());
    spawn_listen_sync_loop(state);
}

fn spawn_scan_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let interval_mins = {
                let cfg = state.config.read().await;
                cfg.library.scan_interval_minutes
            };
            if interval_mins == 0 {
                info!("scan scheduler disabled (scan_interval_minutes = 0)");
                // Sleep long and re-check in case config is reloaded later.
                tokio::time::sleep(Duration::from_secs(300)).await;
                continue;
            }

            let sleep_for = Duration::from_secs(interval_mins.saturating_mul(60));
            info!(?sleep_for, "scheduler sleeping until next scan");
            tokio::time::sleep(sleep_for).await;

            match run_scan(&state, None).await {
                Ok(detail) => info!(%detail, "scheduled scan complete"),
                Err(err) => {
                    error!(error = %err, "scheduled scan failed");
                    if let Some(diag) = bookclerk_config::diagnostics_global() {
                        diag.request_upload("job_failed");
                    }
                }
            }

            let auto = {
                let cfg = state.config.read().await;
                cfg.library.auto_acquire
            };
            if auto {
                match run_acquire(&state, None, None).await {
                    Ok(detail) => info!(%detail, "scheduled auto-acquire complete"),
                    Err(err) => {
                        error!(error = %err, "scheduled auto-acquire failed");
                        if let Some(diag) = bookclerk_config::diagnostics_global() {
                            diag.request_upload("job_failed");
                        }
                    }
                }
            }
        }
    });
}

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

            let sleep_for = Duration::from_secs(interval_mins.saturating_mul(60));
            info!(?sleep_for, "scheduler sleeping until next listening sync");
            tokio::time::sleep(sleep_for).await;

            let summary = state
                .integrations
                .sync_listening_progress_all(&state.library)
                .await;
            if summary.by_provider.is_empty() {
                info!("scheduled listening sync skipped (no capable integrations)");
                continue;
            }
            info!(
                upserted = summary.upserted,
                providers = summary.by_provider.len(),
                "scheduled listening sync complete"
            );
            for p in &summary.by_provider {
                if let Some(err) = &p.error {
                    warn!(id = %p.id, %err, "listening sync provider failed");
                }
            }
        }
    });
}
