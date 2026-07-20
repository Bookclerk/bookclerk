//! Background scan / auto-liberate scheduler.

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::api::AppState;
use crate::jobs::{run_liberate, run_scan};

/// Spawn the periodic scan loop (interval from config; 0 disables).
pub fn spawn_scheduler(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let interval_mins = {
                let cfg = state.config.read().await;
                cfg.library.scan_interval_minutes
            };
            if interval_mins == 0 {
                info!("scheduler disabled (scan_interval_minutes = 0)");
                // Sleep long and re-check in case config is reloaded later.
                tokio::time::sleep(Duration::from_secs(300)).await;
                continue;
            }

            let sleep_for = Duration::from_secs(interval_mins.saturating_mul(60));
            info!(?sleep_for, "scheduler sleeping until next scan");
            tokio::time::sleep(sleep_for).await;

            match run_scan(&state, None).await {
                Ok(detail) => info!(%detail, "scheduled scan complete"),
                Err(err) => warn!(error = %err, "scheduled scan failed"),
            }

            let auto = {
                let cfg = state.config.read().await;
                cfg.library.auto_liberate
            };
            if auto {
                match run_liberate(&state, None, None).await {
                    Ok(detail) => info!(%detail, "scheduled auto-liberate complete"),
                    Err(err) => warn!(error = %err, "scheduled auto-liberate failed"),
                }
            }
        }
    });
}
