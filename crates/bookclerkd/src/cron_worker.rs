//! Per-node cron ticker for plugin `[triggers] crons`.
//!
//! Every daemon ticks every [`CRON_TICK`]. For each integration this node
//! runs that is bound to a plugin instance and declares crons, it finds the
//! newest fire time at or before now and, when the schedule's cursor is
//! behind it, claims that fire with
//! [`bookclerk_library::control_plane::claim_cron_fire`]. The claim is a
//! compare-and-swap in the shared database, so when several nodes run the
//! same instance exactly one of them admits the `plugin_scheduled` job; the
//! job queue then routes it to a node that runs the instance.
//!
//! Missed fires coalesce: a node that was down through several fire times
//! claims only the newest. A schedule seen for the first time starts at its
//! newest past fire, so it waits for the next one.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{claim_cron_fire, ensure_cron_cursor, CronFireClaim};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_manifest::{CronSchedule, UnixMinute};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::api::AppState;

/// How often every daemon checks its cron schedules. Fires are whole
/// minutes, so a fire is claimed at most this long after it comes due.
pub const CRON_TICK: Duration = Duration::from_secs(15);

/// One schedule of one instance this node runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ScheduleKey {
    /// Plugin instance id.
    instance: String,
    /// Normalized cron source.
    cron: String,
}

/// This node's view of each schedule's cursor, so a tick touches the
/// database only when a fire is due.
#[derive(Debug, Default)]
pub struct CronTicker {
    /// Last fire time (Unix ms) this node knows the cursor has reached.
    known: HashMap<ScheduleKey, i64>,
}

/// Outcome of one schedule on one tick, for tests and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickResult {
    /// Nothing due: the cursor is at or past the newest fire.
    Idle,
    /// This node admitted the fire as a job.
    Fired {
        /// New `plugin_scheduled` job id.
        job_id: String,
    },
    /// The fire came due while the previous fire was still active; the
    /// cursor moved and no job was added.
    Skipped,
    /// Another node claimed the fire first.
    Lost,
}

impl CronTicker {
    /// Handles one schedule at `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns an error when the cursor read or the claim fails.
    pub async fn tick_schedule(
        &mut self,
        library: &LibraryStore,
        instance: &str,
        schedule: &CronSchedule,
        now_ms: i64,
    ) -> bookclerk_library::Result<TickResult> {
        let Some(due) = schedule
            .latest_at_or_before(UnixMinute::from_unix_ms(now_ms))
            .and_then(UnixMinute::to_unix_ms)
        else {
            return Ok(TickResult::Idle);
        };
        let key = ScheduleKey {
            instance: instance.to_string(),
            cron: schedule.as_str().to_string(),
        };
        if self.known.get(&key).is_some_and(|last| *last >= due) {
            return Ok(TickResult::Idle);
        }
        let cursor = ensure_cron_cursor(library, instance, schedule.as_str(), due).await?;
        if cursor.last_fire_ms >= due {
            self.known.insert(key, cursor.last_fire_ms);
            return Ok(TickResult::Idle);
        }
        let claim = claim_cron_fire(
            library,
            instance,
            schedule.as_str(),
            cursor.last_fire_ms,
            due,
        )
        .await?;
        match claim {
            CronFireClaim::Claimed { job_id } => {
                self.known.insert(key, due);
                Ok(match job_id {
                    Some(job_id) => TickResult::Fired { job_id },
                    None => TickResult::Skipped,
                })
            }
            CronFireClaim::Lost => {
                // Re-read next tick: the winner moved the cursor.
                self.known.remove(&key);
                Ok(TickResult::Lost)
            }
        }
    }

    /// Forgets schedules no longer in `live`, so a redeployed instance
    /// re-reads its cursor.
    fn retain(&mut self, live: &[(String, CronSchedule)]) {
        self.known.retain(|key, _| {
            live.iter()
                .any(|(instance, cron)| *instance == key.instance && cron.as_str() == key.cron)
        });
    }

    /// One pass over every schedule this node runs.
    pub async fn pass(&mut self, state: &AppState) {
        let schedules: Vec<(String, CronSchedule)> = {
            let integrations = state.integrations.read().await;
            integrations
                .all()
                .iter()
                .filter_map(|integration| {
                    let instance = integration.plugin_instance_id()?.to_string();
                    Some(
                        integration
                            .cron_schedules()
                            .into_iter()
                            .map(move |cron| (instance.clone(), cron)),
                    )
                })
                .flatten()
                .collect()
        };
        self.retain(&schedules);
        if schedules.is_empty() {
            return;
        }
        let library = state.library_snapshot().await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        for (instance, cron) in &schedules {
            match self.tick_schedule(&library, instance, cron, now_ms).await {
                Ok(TickResult::Fired { job_id }) => {
                    info!(plugin_instance_id = %instance, cron = %cron, %job_id, "cron fired");
                }
                Ok(TickResult::Skipped) => {
                    warn!(
                        plugin_instance_id = %instance,
                        cron = %cron,
                        "cron fire skipped: the previous fire is still queued or running"
                    );
                }
                Ok(TickResult::Lost) => {
                    debug!(plugin_instance_id = %instance, cron = %cron, "cron fire claimed elsewhere");
                }
                Ok(TickResult::Idle) => {}
                Err(err) => {
                    warn!(plugin_instance_id = %instance, cron = %cron, error = %err, "cron tick failed");
                }
            }
        }
    }
}

/// Starts this daemon's cron ticker.
pub fn spawn_cron_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CRON_TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut ticker = CronTicker::default();
        loop {
            tick.tick().await;
            ticker.pass(&state).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::control_plane::{create_plugin_instance, ConfigActor};

    /// In-memory store with one instance.
    async fn store_with_instance() -> (LibraryStore, String) {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite"),
        );
        let instance = create_plugin_instance(
            &store,
            &ConfigActor::Operator {
                id: "operator".into(),
            },
            "platform:bookclerk/nightly",
            "",
        )
        .await
        .expect("instance")
        .id
        .as_str()
        .to_string();
        (store, instance)
    }

    /// Unix ms of minute `m` after the epoch, plus `s` seconds.
    fn at(m: i64, s: i64) -> i64 {
        m * 60_000 + s * 1000
    }

    #[tokio::test]
    async fn a_new_schedule_waits_then_fires_once_per_due_time() {
        let (store, instance) = store_with_instance().await;
        let cron = CronSchedule::parse("*/30 * * * *").expect("cron");
        let mut ticker = CronTicker::default();
        // First sight at minute 40: the newest past fire (30) is skipped.
        assert_eq!(
            ticker
                .tick_schedule(&store, &instance, &cron, at(40, 5))
                .await
                .unwrap(),
            TickResult::Idle
        );
        // Minute 60 is due.
        let fired = ticker
            .tick_schedule(&store, &instance, &cron, at(60, 1))
            .await
            .unwrap();
        let TickResult::Fired { job_id } = fired else {
            panic!("expected a fire: {fired:?}");
        };
        let job = store.get_job(&job_id).await.unwrap().expect("job");
        assert_eq!(job.payload.scheduled_time_ms, Some(at(60, 0)));
        // A later tick in the same minute does nothing.
        assert_eq!(
            ticker
                .tick_schedule(&store, &instance, &cron, at(60, 20))
                .await
                .unwrap(),
            TickResult::Idle
        );
    }

    #[tokio::test]
    async fn two_nodes_fire_once_and_missed_fires_coalesce() {
        let (store, instance) = store_with_instance().await;
        let cron = CronSchedule::parse("*/30 * * * *").expect("cron");
        let mut a = CronTicker::default();
        let mut b = CronTicker::default();
        a.tick_schedule(&store, &instance, &cron, at(1, 0))
            .await
            .unwrap();
        b.tick_schedule(&store, &instance, &cron, at(1, 0))
            .await
            .unwrap();
        // Both are behind by three fires (30, 60, 90); one claims 90 only.
        let first = a
            .tick_schedule(&store, &instance, &cron, at(95, 0))
            .await
            .unwrap();
        assert!(matches!(first, TickResult::Fired { .. }), "{first:?}");
        let second = b
            .tick_schedule(&store, &instance, &cron, at(95, 0))
            .await
            .unwrap();
        assert_eq!(second, TickResult::Idle);
        let cursor =
            bookclerk_library::control_plane::load_cron_cursor(&store, &instance, cron.as_str())
                .await
                .unwrap()
                .expect("cursor");
        assert_eq!(cursor.last_fire_ms, at(90, 0));
    }

    #[tokio::test]
    async fn a_fire_due_while_the_last_is_queued_is_skipped() {
        let (store, instance) = store_with_instance().await;
        let cron = CronSchedule::parse("* * * * *").expect("cron");
        let mut ticker = CronTicker::default();
        ticker
            .tick_schedule(&store, &instance, &cron, at(1, 0))
            .await
            .unwrap();
        let first = ticker
            .tick_schedule(&store, &instance, &cron, at(2, 0))
            .await
            .unwrap();
        assert!(matches!(first, TickResult::Fired { .. }), "{first:?}");
        assert_eq!(
            ticker
                .tick_schedule(&store, &instance, &cron, at(3, 0))
                .await
                .unwrap(),
            TickResult::Skipped
        );
    }

    /// An instance-bound integration that declares crons and records fires.
    struct CronProbe {
        /// Plugin instance id it runs as.
        instance: String,
        /// Declared schedules.
        crons: Vec<CronSchedule>,
        /// Fires it received.
        fired: std::sync::Mutex<Vec<bookclerk_integrations::ScheduledInvocation>>,
    }

    #[async_trait::async_trait]
    impl bookclerk_integrations::Integration for CronProbe {
        fn id(&self) -> &str {
            "nightly"
        }

        fn plugin_instance_id(&self) -> Option<&str> {
            Some(&self.instance)
        }

        async fn start(
            &self,
            _ctx: bookclerk_integrations::IntegrationContext,
        ) -> bookclerk_integrations::Result<()> {
            Ok(())
        }

        async fn health(
            &self,
        ) -> bookclerk_integrations::Result<bookclerk_integrations::IntegrationHealth> {
            Ok(bookclerk_integrations::IntegrationHealth {
                id: "nightly".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        async fn deliver_domain_event(
            &self,
            _event: bookclerk_integrations::DomainEvent,
        ) -> bookclerk_integrations::Result<bookclerk_integrations::EventResult> {
            Ok(bookclerk_integrations::EventResult::Ack)
        }

        fn cron_schedules(&self) -> Vec<CronSchedule> {
            self.crons.clone()
        }

        async fn run_scheduled_cancelable(
            &self,
            invocation: bookclerk_integrations::ScheduledInvocation,
            _cancel: Arc<std::sync::atomic::AtomicBool>,
        ) -> bookclerk_integrations::Result<()> {
            self.fired.lock().expect("fired lock").push(invocation);
            Ok(())
        }
    }

    /// Daemon state over `store` running one [`CronProbe`] for `instance`.
    async fn state_with_probe(
        store: LibraryStore,
        instance: &str,
        cron: &CronSchedule,
    ) -> (AppState, Arc<CronProbe>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[daemon]\nlisten = \"127.0.0.1:8787\"\n\n[daemon.auth]\nenabled = false\n",
        )
        .expect("config");
        let cfg = bookclerk_config::Config::load(Some(dir.path().to_path_buf()), None)
            .expect("config load");
        let mut app = crate::config_authority::control_plane_test_state(store, cfg);
        let probe = Arc::new(CronProbe {
            instance: instance.to_string(),
            crons: vec![cron.clone()],
            fired: std::sync::Mutex::new(Vec::new()),
        });
        let mut registry = bookclerk_integrations::IntegrationRegistry::new();
        registry.register(Arc::clone(&probe) as Arc<dyn bookclerk_integrations::Integration>);
        app.integrations = Arc::new(tokio::sync::RwLock::new(registry));
        (app, probe, dir)
    }

    #[tokio::test]
    async fn a_pass_reads_each_running_schedule_and_seeds_its_cursor() {
        let (store, instance) = store_with_instance().await;
        let cron = CronSchedule::parse("* * * * *").expect("cron");
        let (state, _probe, _dir) = state_with_probe(store.clone(), &instance, &cron).await;
        let mut ticker = CronTicker::default();
        ticker.pass(&state).await;
        let cursor =
            bookclerk_library::control_plane::load_cron_cursor(&store, &instance, cron.as_str())
                .await
                .unwrap();
        assert!(cursor.is_some(), "the first pass seeds the cursor");
        assert_eq!(ticker.known.len(), 1);
    }

    #[tokio::test]
    async fn a_scheduled_job_runs_the_handler_and_a_dropped_schedule_is_a_no_op() {
        let (store, instance) = store_with_instance().await;
        let cron = CronSchedule::parse("0 3 * * *").expect("cron");
        let (state, probe, _dir) = state_with_probe(store, &instance, &cron).await;
        let payload = bookclerk_library::JobPayload {
            plugin_instance_id: Some(instance.clone()),
            cron: Some(cron.as_str().to_string()),
            scheduled_time_ms: Some(at(180, 0)),
            ..bookclerk_library::JobPayload::default()
        };
        let out = crate::jobs::run_plugin_scheduled(&state, &payload, None)
            .await
            .unwrap();
        assert!(out.contains("ran"), "{out}");
        {
            let fired = probe.fired.lock().unwrap();
            assert_eq!(fired.len(), 1);
            assert_eq!(fired[0].cron, cron);
            assert_eq!(fired[0].scheduled_time_ms, at(180, 0));
            assert_eq!(fired[0].attempt, 1);
        }

        let removed = bookclerk_library::JobPayload {
            cron: Some("0 4 * * *".into()),
            ..payload.clone()
        };
        let out = crate::jobs::run_plugin_scheduled(&state, &removed, None)
            .await
            .unwrap();
        assert!(out.contains("dropped"), "{out}");
        assert_eq!(probe.fired.lock().unwrap().len(), 1);

        let elsewhere = bookclerk_library::JobPayload {
            plugin_instance_id: Some("other".into()),
            ..payload.clone()
        };
        let err = crate::jobs::run_plugin_scheduled(&state, &elsewhere, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not running on this node"),
            "{err}"
        );

        for broken in [
            bookclerk_library::JobPayload {
                plugin_instance_id: None,
                ..payload.clone()
            },
            bookclerk_library::JobPayload {
                cron: None,
                ..payload.clone()
            },
            bookclerk_library::JobPayload {
                scheduled_time_ms: None,
                ..payload.clone()
            },
            bookclerk_library::JobPayload {
                cron: Some("not cron".into()),
                ..payload.clone()
            },
        ] {
            assert!(crate::jobs::run_plugin_scheduled(&state, &broken, None)
                .await
                .is_err());
        }
    }

    #[test]
    fn retain_forgets_undeployed_schedules() {
        let cron = CronSchedule::parse("0 * * * *").expect("cron");
        let mut ticker = CronTicker::default();
        ticker.known.insert(
            ScheduleKey {
                instance: "a".into(),
                cron: cron.as_str().into(),
            },
            1,
        );
        ticker.known.insert(
            ScheduleKey {
                instance: "b".into(),
                cron: cron.as_str().into(),
            },
            1,
        );
        ticker.retain(&[("a".into(), cron)]);
        assert_eq!(ticker.known.len(), 1);
    }
}
