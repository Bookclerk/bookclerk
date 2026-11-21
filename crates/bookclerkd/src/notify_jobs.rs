//! The `notify_send` job and the sweep that keeps every delivery scheduled.
//!
//! The router ([`bookclerk_library::LibraryStore::route_notification`])
//! writes one delivery-log row per destination and enqueues its send job.
//! Each job sends one notification to one destination through its
//! notifier instance and records the outcome on the delivery:
//!
//! - delivered: the destination's failure count resets;
//! - `rate_limited` or `unavailable` (or a notifier that could not be
//!   reached): the job is parked until the service's `Retry-After`, or an
//!   exponential backoff, without spending a claim; after
//!   [`MAX_SEND_ATTEMPTS`] tries the delivery fails;
//! - any other refusal: the delivery fails, the destination's failure count
//!   rises, and the destination pauses at
//!   [`bookclerk_library::notifications::PAUSE_AFTER_FAILURES`].
//!
//! A delivery whose destination is gone or paused, or whose notifier
//! instance does not run here, is dropped without a send.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::notifications::{
    link_for, DeliveryState, FailureEffect, SendWork, MAX_SEND_ATTEMPTS,
};
use bookclerk_library::{JobPayload, NotifyQueueLimits};
use bookclerk_plugin_abi::notifications::{check_send_params, classify_send_reply, SendOutcome};
use bookclerk_plugin_abi::{NotifyDestination, NotifySendParams};
use tracing::{info, warn};

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{job_cancelled, JobFailedForGood, JOB_SUSPENDED_DETAIL_PREFIX};

/// First retry wait when the service names none; doubles per attempt.
const RETRY_BASE: Duration = Duration::from_secs(30);

/// Longest retry wait the backoff reaches.
const RETRY_MAX: Duration = Duration::from_secs(60 * 60);

/// How often the sweep looks for deliveries without a live send job.
pub const NOTIFY_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Most deliveries one sweep pass schedules.
const NOTIFY_SWEEP_BATCH: u64 = 100;

/// The backoff before retry number `attempts + 1`: [`RETRY_BASE`] doubled
/// per attempt made, capped at [`RETRY_MAX`].
///
/// # Arguments
///
/// * `attempts` - Attempts already made (1 after the first failure).
#[must_use]
pub fn retry_backoff(attempts: i64) -> Duration {
    let doublings = u32::try_from(attempts.saturating_sub(1).clamp(0, 16)).unwrap_or(16);
    RETRY_BASE
        .saturating_mul(2u32.saturating_pow(doublings))
        .min(RETRY_MAX)
}

/// The queue limits a send job is admitted under: the live `[jobs]` ones.
pub async fn notify_queue_limits(state: &AppState) -> NotifyQueueLimits {
    let (max_pending, max_attempts) = crate::jobs::queue_limits(state).await;
    NotifyQueueLimits {
        max_pending,
        max_attempts,
    }
}

/// Runs one `notify_send` job: sends the delivery its payload names.
///
/// # Arguments
///
/// * `state` - Daemon state (library, integrations, config).
/// * `payload` - The job payload; `notify_delivery` names the delivery.
/// * `ctx` - Lease and cancel token; needed to park a retry.
///
/// # Returns
///
/// A one-line summary for the job row, or the suspended marker when the
/// job was parked for a retry.
///
/// # Errors
///
/// [`JobFailedForGood`] when the delivery failed (so the job is not
/// retried: the delivery log already says why); another error when the
/// library could not be read or written, which the job's own retries
/// cover.
pub async fn run_notify_send(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let delivery_id = payload
        .notify_delivery
        .as_deref()
        .ok_or_else(|| JobFailedForGood("notify_send names no delivery".into()))?;
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let Some(work) = library.notify_send_work(delivery_id).await? else {
        return Ok(format!("delivery {delivery_id} is gone"));
    };
    if work.state != DeliveryState::Pending {
        return Ok(format!(
            "delivery {delivery_id} is already {}",
            work.state.as_str()
        ));
    }
    let Some(destination) = work.destination.as_ref() else {
        library
            .record_notify_dropped(delivery_id, "the destination was removed")
            .await?;
        return Ok(format!(
            "delivery {delivery_id} dropped: destination removed"
        ));
    };
    if destination.paused_at.is_some() {
        library
            .record_notify_dropped(delivery_id, "the destination is paused")
            .await?;
        return Ok(format!(
            "delivery {delivery_id} dropped: destination paused"
        ));
    }
    let notifier = {
        let integrations = state.integrations.read().await;
        integrations
            .all()
            .iter()
            .find(|i| i.plugin_instance_id() == Some(work.plugin_instance_id.as_str()))
            .cloned()
    };
    let Some(notifier) = notifier.filter(|n| n.supports_notifier()) else {
        library
            .record_notify_dropped(
                delivery_id,
                "the notifier instance is not running or no longer delivers notifications",
            )
            .await?;
        return Ok(format!(
            "delivery {delivery_id} dropped: notifier {} not running",
            work.plugin_instance_id
        ));
    };
    let params = send_params(state, &work).await;
    if let Err(err) = check_send_params(&params) {
        // A stored message or address the checks refuse cannot improve on
        // a retry. It is the host's fault, not the destination's, so it is
        // dropped rather than failed and never counts toward a pause.
        let reason = format!("the stored notification is not sendable: {err}");
        library.record_notify_dropped(delivery_id, &reason).await?;
        return Err(JobFailedForGood(reason).into());
    }
    let reply = notifier.notify_send(params).await;
    let outcome = match &reply {
        Ok(answer) => classify_send_reply(answer),
        Err(err) => SendOutcome::Retry {
            after: None,
            reason: format!("the notifier could not be reached: {err}"),
        },
    };
    match outcome {
        SendOutcome::Delivered { provider_id } => {
            library
                .record_notify_delivered(delivery_id, provider_id.as_deref())
                .await?;
            Ok(format!("delivered to {}", destination.label))
        }
        SendOutcome::Failed { reason } => fail(state, &work, &reason).await,
        SendOutcome::Retry { after, reason } => {
            let Some(attempts) = library.record_notify_retry(delivery_id, &reason).await? else {
                return Ok(format!("delivery {delivery_id} finished elsewhere"));
            };
            if attempts >= MAX_SEND_ATTEMPTS {
                return fail(
                    state,
                    &work,
                    &format!("gave up after {attempts} attempts: {reason}"),
                )
                .await;
            }
            let wait = after.unwrap_or_else(|| retry_backoff(attempts));
            let Some(ctx) = ctx else {
                anyhow::bail!("{reason}");
            };
            let until = chrono::Utc::now()
                + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::hours(1));
            let progress = format!("attempt {attempts} of {MAX_SEND_ATTEMPTS}: {reason}");
            if !library.defer_job(&ctx.fence, until, &progress).await? {
                anyhow::bail!("could not park the send (lease fence lost)");
            }
            Ok(format!("{JOB_SUSPENDED_DETAIL_PREFIX}{until} ({progress})"))
        }
    }
}

/// Records a failed delivery and ends the job for good.
async fn fail(state: &AppState, work: &SendWork, reason: &str) -> anyhow::Result<String> {
    let library = state.library.read().await.clone();
    if library
        .record_notify_failed(&work.delivery_id, reason)
        .await?
        == Some(FailureEffect::Paused)
    {
        warn!(
            destination = work.destination.as_ref().map_or("", |d| d.id.as_str()),
            notifier = %work.plugin_instance_id,
            "notification destination paused after repeated failures"
        );
    }
    Err(JobFailedForGood(reason.to_string()).into())
}

/// The params of one send: the stored message with its link built from
/// the public origin, and the destination.
async fn send_params(state: &AppState, work: &SendWork) -> NotifySendParams {
    let origin = state.config.read().await.integrations.public_origin.clone();
    let stored = &work.notification;
    let link = link_for(origin.as_deref(), stored.notification.link_path.as_deref());
    let destination = work
        .destination
        .as_ref()
        .map_or_else(NotifyDestination::default, |d| NotifyDestination {
            kind: d.kind.wire(),
            destination_id: d.id.clone(),
            address_json: d.address_json.clone(),
        });
    NotifySendParams {
        message: stored.notification.message(&stored.id, link),
        destination,
    }
}

/// Starts the sweep: every [`NOTIFY_SWEEP_INTERVAL`] it drops deliveries
/// whose send job ended without a result and enqueues the send job of each
/// delivery that has none (its enqueue failed or found the queue full).
///
/// Every daemon runs it; the send jobs are deduplicated per delivery, so
/// two daemons sweeping at once enqueue each job once.
pub fn spawn_notify_sweep(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(NOTIFY_SWEEP_INTERVAL).await;
            if let Err(err) = sweep_once(&state).await {
                warn!(error = %err, "notification sweep failed");
            }
        }
    });
}

/// One sweep pass.
///
/// # Errors
///
/// An error when a library read or write fails.
pub async fn sweep_once(state: &AppState) -> anyhow::Result<()> {
    let library = state.library.read().await.clone();
    let dropped = library.drop_orphaned_notify_deliveries().await?;
    if dropped > 0 {
        info!(
            dropped,
            "dropped deliveries whose send job ended without a result"
        );
    }
    let limits = notify_queue_limits(state).await;
    for delivery in library
        .unscheduled_notify_deliveries(NOTIFY_SWEEP_BATCH)
        .await?
    {
        if library
            .enqueue_notify_send(&delivery, limits)
            .await?
            .is_none()
        {
            // The queue is full; the next pass tries again.
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use bookclerk_integrations::{Integration, IntegrationContext, IntegrationHealth};
    use bookclerk_library::notifications::{
        DestinationKind, NewDestination, NewNotification, Route, PAUSE_AFTER_FAILURES,
    };
    use bookclerk_library::LibraryStore;
    use bookclerk_plugin_abi::{
        DomainEvent, EventResult, NotificationAlertAction, NotificationAudience,
        NotificationSeverity, NotifySent, PluginError,
    };

    /// A notifier that answers each send with the next scripted reply and
    /// records what it was sent.
    struct FakeNotifier {
        /// Replies, first one next.
        replies: Mutex<Vec<Result<NotifySent, PluginError>>>,
        /// Every send it received.
        sent: Mutex<Vec<NotifySendParams>>,
    }

    #[async_trait]
    impl Integration for FakeNotifier {
        fn id(&self) -> &str {
            "fake-notifier"
        }

        fn plugin_instance_id(&self) -> Option<&str> {
            Some("fake-1")
        }

        fn supports_notifier(&self) -> bool {
            true
        }

        async fn notify_send(
            &self,
            params: NotifySendParams,
        ) -> bookclerk_integrations::Result<Result<NotifySent, PluginError>> {
            self.sent.lock().expect("sent").push(params);
            Ok(self.replies.lock().expect("replies").remove(0))
        }

        async fn start(&self, _ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
            Ok(())
        }

        async fn deliver_domain_event(
            &self,
            _event: DomainEvent,
        ) -> bookclerk_integrations::Result<EventResult> {
            Ok(EventResult::Ack)
        }

        async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
            Ok(IntegrationHealth {
                id: "fake-notifier".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }
    }

    /// A daemon with `replies` scripted on a running notifier `fake-1`,
    /// and a channel on it routed for `test`; answers the state, the
    /// notifier and the channel.
    async fn daemon(
        replies: Vec<Result<NotifySent, PluginError>>,
    ) -> (Arc<AppState>, Arc<FakeNotifier>, String) {
        let (state, _app, library) =
            crate::auth::tests::phase2_harness_unseeded("op-token-notify-send").await;
        let fake = Arc::new(FakeNotifier {
            replies: Mutex::new(replies),
            sent: Mutex::new(Vec::new()),
        });
        state.integrations.write().await.register(fake.clone());
        let channel = library
            .create_notify_destination(&NewDestination {
                plugin_instance_id: "fake-1".into(),
                kind: DestinationKind::Channel,
                user_id: None,
                label: "Ops".into(),
                address_json: String::new(),
            })
            .await
            .expect("channel")
            .id;
        library
            .set_notify_routes(
                "test",
                &[Route {
                    destination_id: channel.clone(),
                    min_severity: NotificationSeverity::Info,
                }],
            )
            .await
            .expect("routes");
        (state, fake, channel)
    }

    /// Routes one `test` notification; answers the payload of its send job.
    async fn routed(library: &LibraryStore) -> JobPayload {
        routed_under(library, 1_000).await
    }

    /// [`routed`] with a queue that admits `max_pending` jobs.
    async fn routed_under(library: &LibraryStore, max_pending: i64) -> JobPayload {
        let delivery = library
            .route_notification(
                &NewNotification {
                    notification_type: "test".into(),
                    audience: NotificationAudience::Operational,
                    severity: NotificationSeverity::Warning,
                    title: "Scan failed".into(),
                    text: "The scan stopped.".into(),
                    link_path: Some("/jobs".into()),
                    fields: Vec::new(),
                    group_key: None,
                    alert_action: NotificationAlertAction::None,
                    alert_key: None,
                    source: String::new(),
                    patron: None,
                },
                NotifyQueueLimits {
                    max_pending,
                    max_attempts: 3,
                },
            )
            .await
            .expect("routed")
            .deliveries
            .remove(0);
        JobPayload {
            notify_delivery: Some(delivery.id),
            ..JobPayload::default()
        }
    }

    #[tokio::test]
    async fn a_delivered_send_is_recorded_with_its_link() {
        let (state, fake, _) = daemon(vec![Ok(NotifySent {
            provider_id: Some("msg-1".into()),
        })])
        .await;
        state.config.write().await.integrations.public_origin =
            Some("https://library.example".into());
        let library = state.library_snapshot().await;
        let payload = routed(&library).await;
        let id = payload.notify_delivery.clone().unwrap();
        run_notify_send(&state, &payload, None).await.expect("sent");
        let work = library.notify_send_work(&id).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Delivered);
        let link = fake.sent.lock().unwrap()[0].message.link.clone();
        assert_eq!(link.as_deref(), Some("https://library.example/jobs"));
        // A second run of the same job sends nothing more.
        run_notify_send(&state, &payload, None)
            .await
            .expect("no-op");
        assert_eq!(fake.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_retry_counts_an_attempt_and_a_refusal_fails_for_good() {
        let mut replies = vec![Err(PluginError::rate_limited(
            "slow down",
            Some(std::time::Duration::from_secs(5)),
        ))];
        for _ in 0..PAUSE_AFTER_FAILURES {
            replies.push(Err(PluginError::invalid_params("bad webhook")));
        }
        let (state, _fake, channel) = daemon(replies).await;
        let library = state.library_snapshot().await;
        let payload = routed(&library).await;
        let id = payload.notify_delivery.clone().unwrap();
        // Without a lease the parked retry falls back to the job's own retry.
        assert!(run_notify_send(&state, &payload, None).await.is_err());
        let work = library.notify_send_work(&id).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Pending);
        assert_eq!(work.attempts, 1);

        let err = run_notify_send(&state, &payload, None).await.unwrap_err();
        assert!(err.downcast_ref::<JobFailedForGood>().is_some());
        let work = library.notify_send_work(&id).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Failed);
        for _ in 1..PAUSE_AFTER_FAILURES {
            let payload = routed(&library).await;
            let _ = run_notify_send(&state, &payload, None).await;
        }
        let paused = library.notify_destination(&channel).await.unwrap().unwrap();
        assert!(paused.paused_at.is_some());
    }

    #[tokio::test]
    async fn a_paused_destination_or_a_missing_notifier_drops_the_send() {
        let (state, fake, channel) = daemon(Vec::new()).await;
        let library = state.library_snapshot().await;
        let payload = routed(&library).await;
        let id = payload.notify_delivery.clone().unwrap();
        let _retired = state.integrations.write().await.take_instance("fake-1");
        run_notify_send(&state, &payload, None)
            .await
            .expect("dropped");
        let work = library.notify_send_work(&id).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Dropped);

        state.integrations.write().await.register(fake.clone());
        let payload = routed(&library).await;
        let id = payload.notify_delivery.clone().unwrap();
        library.delete_notify_destination(&channel).await.unwrap();
        run_notify_send(&state, &payload, None).await.expect("gone");
        assert!(library.notify_send_work(&id).await.unwrap().is_none());
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_sweep_schedules_unscheduled_deliveries() {
        let (state, _fake, _) = daemon(Vec::new()).await;
        let library = state.library_snapshot().await;
        routed_under(&library, 0).await;
        assert_eq!(
            library
                .unscheduled_notify_deliveries(10)
                .await
                .unwrap()
                .len(),
            1
        );
        sweep_once(&state).await.expect("sweep");
        assert!(library
            .unscheduled_notify_deliveries(10)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_backoff_doubles_and_stops_at_an_hour() {
        assert_eq!(retry_backoff(0), RETRY_BASE);
        assert_eq!(retry_backoff(1), RETRY_BASE);
        assert_eq!(retry_backoff(2), RETRY_BASE * 2);
        assert_eq!(retry_backoff(3), RETRY_BASE * 4);
        assert_eq!(retry_backoff(9), RETRY_MAX);
        assert_eq!(retry_backoff(i64::MAX), RETRY_MAX);
    }
}
