//! Singleton maintenance: periodic cluster-wide cleanup, one node per duty.
//!
//! Every daemon ticks each duty every [`MAINTENANCE_TICK`]. A duty runs
//! under its own cluster lease ([`Duty::lease`]); only the holder runs it,
//! at once when it takes the lease and then every [`MAINTENANCE_INTERVAL`].
//! The holder renews on every tick, so a daemon that stops ticking loses the
//! lease after [`MAINTENANCE_LEASE_TTL`] and the next daemon to tick takes
//! over with a larger fencing token. Each destructive step checks the lease
//! first, so a holder that stalled past its expiry is refused before it
//! deletes anything.
//!
//! Node-local cleanup (scratch directories, this node's leases) is not
//! here: it stays with the job and event workers on every node.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    current_process_incarnation, load_or_create_node_identity, LeaseFence, SingletonDuty,
    EVENT_RETENTION_LEASE, JOB_RETENTION_LEASE, KV_EXPIRY_LEASE, SESSION_EXPIRY_LEASE,
};
use bookclerk_library::{EventPruneStep, LibraryStore};
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::api::AppState;

/// How often every daemon polls its maintenance duties. Well inside
/// [`MAINTENANCE_LEASE_TTL`] so the holder keeps its leases.
pub const MAINTENANCE_TICK: Duration = Duration::from_secs(60);

/// How long a maintenance lease lasts without renewal.
pub const MAINTENANCE_LEASE_TTL: Duration = Duration::from_secs(180);

/// How often the holder runs each duty.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);

/// One cluster-wide maintenance duty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duty {
    /// Terminal jobs past `jobs.retention_days`.
    JobRetention,
    /// Terminal deliveries, empty events, and stale subscriber catalog rows
    /// past the `events` retention windows.
    EventRetention,
    /// Expired plugin KV rows (they already read as missing).
    KvExpiry,
    /// Expired operator sessions, OIDC RP states, and WebAuthn challenges.
    SessionExpiry,
}

impl Duty {
    /// Every duty, in the order a pass runs them.
    pub const ALL: [Duty; 4] = [
        Duty::JobRetention,
        Duty::EventRetention,
        Duty::KvExpiry,
        Duty::SessionExpiry,
    ];

    /// The cluster lease this duty runs under.
    #[must_use]
    pub fn lease(self) -> &'static str {
        match self {
            Duty::JobRetention => JOB_RETENTION_LEASE,
            Duty::EventRetention => EVENT_RETENTION_LEASE,
            Duty::KvExpiry => KV_EXPIRY_LEASE,
            Duty::SessionExpiry => SESSION_EXPIRY_LEASE,
        }
    }
}

/// This node's view of every duty.
#[derive(Debug)]
pub struct Maintenance {
    /// Each duty with its lease state on this node.
    duties: Vec<(Duty, SingletonDuty)>,
}

impl Maintenance {
    /// Duties for `node_id` with the given lease TTL and run interval.
    #[must_use]
    pub fn new(node_id: &str, ttl: Duration, interval: Duration) -> Self {
        Self {
            duties: Duty::ALL
                .into_iter()
                .map(|duty| {
                    (
                        duty,
                        SingletonDuty::new(duty.lease(), node_id, ttl, interval),
                    )
                })
                .collect(),
        }
    }

    /// One tick: renews or takes each lease and runs the duties that are due
    /// here. Returns the duties this node ran.
    pub async fn pass(&mut self, state: &AppState) -> Vec<Duty> {
        let library = state.library_snapshot().await;
        let (job_days, event_days, dead_letter_days) = {
            let cfg = state.config.read().await;
            (
                cfg.jobs.retention_days,
                cfg.events.retention_days,
                cfg.events.dead_letter_retention_days,
            )
        };
        let windows = Windows {
            job_days,
            event_days,
            dead_letter_days,
        };
        let mut ran = Vec::new();
        for (duty, singleton) in &mut self.duties {
            let mut fence = match singleton.poll(&library).await {
                Ok(Some(fence)) => fence,
                Ok(None) => continue,
                Err(err) => {
                    warn!(error = %err, lease = duty.lease(), "maintenance lease unavailable");
                    continue;
                }
            };
            match run_duty(*duty, &library, &mut fence, windows).await {
                Ok(removed) => {
                    debug!(
                        lease = duty.lease(),
                        token = fence.lease().token,
                        removed,
                        "maintenance ran"
                    );
                }
                Err(err) => warn!(error = %err, lease = duty.lease(), "maintenance duty failed"),
            }
            singleton.mark_ran();
            ran.push(*duty);
        }
        ran
    }
}

/// Retention windows read from configuration for one pass.
#[derive(Debug, Clone, Copy)]
struct Windows {
    /// `jobs.retention_days`.
    job_days: u64,
    /// `events.retention_days`.
    event_days: u64,
    /// `events.dead_letter_retention_days`.
    dead_letter_days: u64,
}

/// Runs one duty, checking the lease before each destructive step.
async fn run_duty(
    duty: Duty,
    library: &LibraryStore,
    fence: &mut LeaseFence<'_>,
    windows: Windows,
) -> bookclerk_library::Result<u64> {
    fence.check().await?;
    match duty {
        Duty::JobRetention => library.prune_terminal_jobs(windows.job_days).await,
        Duty::EventRetention => {
            let mut removed = 0;
            for (index, step) in EventPruneStep::ALL.into_iter().enumerate() {
                if index > 0 {
                    fence.check().await?;
                }
                removed += library
                    .prune_event_step(step, windows.event_days, windows.dead_letter_days)
                    .await?;
            }
            Ok(removed)
        }
        Duty::KvExpiry => library.kv_prune_expired().await,
        Duty::SessionExpiry => {
            let mut removed = library.prune_expired_operator_sessions().await?;
            fence.check().await?;
            removed += library.prune_expired_oidc_rp_states().await?;
            fence.check().await?;
            removed += library.prune_expired_webauthn_challenges().await?;
            fence.check().await?;
            // An abandoned join sign-in is otherwise purged only when the
            // next one starts.
            removed +=
                bookclerk_library::control_plane::purge_expired_pending_joins(library).await?;
            Ok(removed)
        }
    }
}

/// Starts this daemon's maintenance loop.
pub fn spawn_maintenance(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(MAINTENANCE_TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut maintenance = None;
        loop {
            tick.tick().await;
            if maintenance.is_none() {
                let files_dir = state.config.read().await.paths().files_dir.clone();
                match load_or_create_node_identity(&files_dir) {
                    Ok(identity) => {
                        // Two processes can share a files directory (and so a
                        // node id) during an overlapping restart; the holder
                        // must tell them apart, or the second would renew the
                        // first's live lease with the same token.
                        let holder =
                            format!("{}/{}", identity.node_id, current_process_incarnation());
                        maintenance = Some(Maintenance::new(
                            &holder,
                            MAINTENANCE_LEASE_TTL,
                            MAINTENANCE_INTERVAL,
                        ));
                    }
                    Err(err) => {
                        warn!(error = %err, "node identity unavailable; maintenance idle");
                        continue;
                    }
                }
            }
            if let Some(maintenance) = maintenance.as_mut() {
                maintenance.pass(&state).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bookclerk_config::Config;
    use bookclerk_library::control_plane::load_lease;

    use super::{Duty, Maintenance};

    async fn state_on(
        db_path: &std::path::Path,
        files: &std::path::Path,
    ) -> (bookclerk_library::LibraryStore, Arc<crate::api::AppState>) {
        std::fs::write(files.join("config.toml"), "").unwrap();
        let db = bookclerk_plugin_database_sqlite::open(db_path)
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let config =
            Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).unwrap();
        let state = Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            config,
        ));
        (store, state)
    }

    #[tokio::test]
    async fn one_daemon_runs_each_duty_and_another_takes_over_on_expiry() {
        let shared = tempfile::tempdir().unwrap();
        let files_a = tempfile::tempdir().unwrap();
        let files_b = tempfile::tempdir().unwrap();
        let db_path = shared.path().join("library.db");
        let (store, state_a) = state_on(&db_path, files_a.path()).await;
        let (_, state_b) = state_on(&db_path, files_b.path()).await;
        // Each pass must finish well inside the TTL, or a lease lapses mid
        // test and the other node legitimately takes it; slow CI runners
        // need seconds, not milliseconds, here.
        let ttl = Duration::from_secs(3);
        let hour = Duration::from_secs(3600);
        let mut a = Maintenance::new("node-a", ttl, hour);
        let mut b = Maintenance::new("node-b", ttl, hour);

        assert_eq!(a.pass(&state_a).await, Duty::ALL.to_vec());
        assert!(b.pass(&state_b).await.is_empty());
        // Both keep ticking within the interval: nobody reruns.
        assert!(a.pass(&state_a).await.is_empty());
        assert!(b.pass(&state_b).await.is_empty());
        for duty in Duty::ALL {
            let lease = load_lease(&store, duty.lease()).await.unwrap().unwrap();
            assert_eq!((lease.holder.as_str(), lease.token), ("node-a", 1));
        }

        // Node A stops ticking; once its leases expire node B takes every
        // duty with a larger token and runs it at once.
        tokio::time::sleep(ttl + Duration::from_millis(500)).await;
        assert_eq!(b.pass(&state_b).await, Duty::ALL.to_vec());
        assert!(a.pass(&state_a).await.is_empty());
        for duty in Duty::ALL {
            let lease = load_lease(&store, duty.lease()).await.unwrap().unwrap();
            assert_eq!((lease.holder.as_str(), lease.token), ("node-b", 2));
        }
    }
}
