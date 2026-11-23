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
    link_for, DeliveryState, FailedAttempt, FailureEffect, SendWork, StallOutcome,
    MAX_SEND_ATTEMPTS,
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

/// How long a send job may stay ready to run without being claimed
/// before the sweep fails its delivery, when its notifier instance runs
/// on no live node. A send waiting this long behind other work while its
/// notifier runs is left alone.
pub const NOTIFY_STALL_AFTER: Duration = Duration::from_secs(60 * 60);

/// The delivery-log reason of a send that waited out [`NOTIFY_STALL_AFTER`].
const STALLED_REASON: &str = "the notifier was not running for an hour";

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
/// * `payload` - The job payload; `notify_delivery` names the delivery, or
///   `account_mail` an account mail ([`crate::account_mail`]).
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
    if payload.account_mail.is_some() {
        return crate::account_mail::run_account_mail_send(state, payload, ctx).await;
    }
    let delivery_id = payload
        .notify_delivery
        .as_deref()
        .ok_or_else(|| JobFailedForGood("notify_send names no delivery".into()))?;
    let library = state.library.read().await.clone();
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    // Every outcome write is fenced by this job's claim, so a send that
    // lost its lease (while a replacement job took the delivery over)
    // records nothing.
    let fence = ctx.map(|ctx| &ctx.fence);
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
            .record_notify_dropped(delivery_id, "the destination was removed", fence)
            .await?;
        return Ok(format!(
            "delivery {delivery_id} dropped: destination removed"
        ));
    };
    if destination.paused_at.is_some() {
        library
            .record_notify_dropped(delivery_id, "the destination is paused", fence)
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
                fence,
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
        library
            .record_notify_dropped(delivery_id, &reason, fence)
            .await?;
        return Err(JobFailedForGood(reason).into());
    }
    // A delivery whose attempts already reached the limit (its final
    // failure write did not land) is finished without another send.
    if work.attempts >= MAX_SEND_ATTEMPTS {
        return fail(
            state,
            &work,
            &format!("gave up after {} attempts", work.attempts),
            FailedAttempt::AlreadyCounted,
            fence,
        )
        .await;
    }
    // Cancellation may have arrived while the delivery was read; no send
    // starts after it.
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
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
                .record_notify_delivered(delivery_id, provider_id.as_deref(), fence)
                .await?;
            Ok(format!("delivered to {}", destination.label))
        }
        SendOutcome::Failed { reason } => {
            fail(state, &work, &reason, FailedAttempt::New, fence).await
        }
        SendOutcome::Retry { after, reason } => {
            let Some(attempts) = library
                .record_notify_retry(delivery_id, &reason, fence)
                .await?
            else {
                return Ok(format!("delivery {delivery_id} finished elsewhere"));
            };
            if attempts >= MAX_SEND_ATTEMPTS {
                return fail(
                    state,
                    &work,
                    &format!("gave up after {attempts} attempts: {reason}"),
                    FailedAttempt::AlreadyCounted,
                    fence,
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
///
/// # Arguments
///
/// * `attempt` - Whether this answer spends an attempt of its own (a
///   refusal) or was already counted (the last of the retries).
/// * `fence` - The job's claim; a claim that lost the job records nothing.
async fn fail(
    state: &AppState,
    work: &SendWork,
    reason: &str,
    attempt: FailedAttempt,
    fence: Option<&bookclerk_library::JobFence>,
) -> anyhow::Result<String> {
    let library = state.library.read().await.clone();
    if library
        .record_notify_failed(&work.delivery_id, reason, attempt, fence)
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
    // Hold off a database swap while this pass works on its snapshot, so
    // nothing it enqueues lands on a superseded store.
    let _permit = crate::jobs::admission_permit(state).await?;
    let library = state.library.read().await.clone();
    let dropped = library.drop_orphaned_notify_deliveries().await?;
    if dropped > 0 {
        info!(
            dropped,
            "dropped deliveries whose send job ended without a result"
        );
    }
    // A failed stall check is logged and the pass goes on, so unscheduled
    // deliveries are still enqueued.
    let stall = chrono::Duration::from_std(NOTIFY_STALL_AFTER)?;
    if let Err(err) = fail_stalled_sends(&library, chrono::Utc::now(), stall).await {
        warn!(error = %err, "could not check for stalled notification sends");
    }
    let limits = notify_queue_limits(state).await;
    for delivery in library
        .unscheduled_notify_deliveries(NOTIFY_SWEEP_BATCH)
        .await?
    {
        // A send job that ended before its link was written means the
        // delivery is finished (cancelled or failed), not unscheduled.
        match library
            .drop_notify_delivery_if_job_ended(&delivery.id)
            .await
        {
            Ok(true) => continue,
            Ok(false) => {}
            Err(err) => {
                warn!(delivery = %delivery.id, error = %err, "notification sweep skipped a delivery");
                continue;
            }
        }
        match library.enqueue_notify_send(&delivery, limits).await {
            Ok(Some(_)) => {}
            // The queue is full; the next pass tries again.
            Ok(None) => break,
            // One delivery's failed write does not hold up the rest of the
            // page; the next pass tries it again.
            Err(err) => {
                warn!(delivery = %delivery.id, error = %err, "notification sweep skipped a delivery");
            }
        }
    }
    crate::account_mail::sweep_once(state, &library, NOTIFY_SWEEP_BATCH).await
}

/// Fails the deliveries whose send job has been ready to run for longer
/// than `stall` without being claimed because its notifier instance runs
/// on no live node: the job is cancelled and the delivery counts as a
/// failed attempt, so the destination pauses as it would for any other run
/// of failures. A job whose notifier is running somewhere is only waiting
/// behind other work and is left to run. A job with no pending delivery
/// left is cancelled, since it has nothing to send.
///
/// # Errors
///
/// An error when a read of stalled sends or of a notifier's deployments
/// fails; sends handled before it keep their writes.
async fn fail_stalled_sends(
    library: &bookclerk_library::LibraryStore,
    now: chrono::DateTime<chrono::Utc>,
    stall: chrono::Duration,
) -> anyhow::Result<()> {
    let ready_before = now - stall;
    let now_ms = now.timestamp_millis();
    // Whether each notifier runs, read once per pass.
    let mut running: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        let (page, next) = library
            .stalled_notify_sends(ready_before, after.as_deref(), NOTIFY_SWEEP_BATCH)
            .await?;
        for stalled in page {
            if let Some(notifier) = stalled.notifier.as_deref() {
                let up = match running.get(notifier) {
                    Some(up) => *up,
                    None => {
                        let up =
                            bookclerk_library::control_plane::instance_running_on_database_node(
                                library, notifier, now_ms,
                            )
                            .await?;
                        running.insert(notifier.to_string(), up);
                        up
                    }
                };
                if up {
                    continue;
                }
            }
            // One write cancels the job and fails its delivery; a claim that
            // got there first leaves both alone.
            match library
                .fail_stalled_notify_send(&stalled, STALLED_REASON)
                .await
            {
                Ok(StallOutcome::Failed(FailureEffect::Paused)) => warn!(
                    job = %stalled.job_id,
                    "notification destination paused after repeated failures"
                ),
                Ok(StallOutcome::Failed(FailureEffect::Counted))
                | Ok(StallOutcome::Dropped)
                | Ok(StallOutcome::Cancelled)
                | Ok(StallOutcome::NotStalled) => {}
                Err(err) => {
                    warn!(job = %stalled.job_id, error = %err, "could not fail a stalled send");
                }
            }
        }
        match next {
            Some(next) => after = Some(next),
            None => return Ok(()),
        }
    }
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

    /// Runs the stall check with sends ready before `ready_before` counted
    /// as having waited too long.
    async fn sweep_stalls(library: &LibraryStore, ready_before: chrono::DateTime<chrono::Utc>) {
        let now = chrono::Utc::now();
        fail_stalled_sends(library, now, now - ready_before)
            .await
            .expect("stall pass");
    }

    /// The pending send jobs ready before `ready_before`, first page.
    async fn stalled(
        library: &LibraryStore,
        ready_before: chrono::DateTime<chrono::Utc>,
    ) -> Vec<bookclerk_library::notifications::StalledSend> {
        library
            .stalled_notify_sends(ready_before, None, 10)
            .await
            .expect("stalled sends")
            .0
    }

    #[tokio::test]
    async fn a_send_no_notifier_claims_fails_and_counts_toward_a_pause() {
        let (state, fake, channel) = daemon(Vec::new()).await;
        let library = state.library_snapshot().await;
        let payload = routed_under(&library, 1_000).await;
        let delivery = payload.notify_delivery.expect("delivery");
        // Within the wait, nothing is failed.
        sweep_stalls(&library, chrono::Utc::now() - chrono::Duration::hours(1)).await;
        let work = library.notify_send_work(&delivery).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Pending);
        // Once the job has waited past the cutoff with its notifier running
        // on no node, it is cancelled and its delivery fails like any other
        // failed attempt.
        let later = chrono::Utc::now() + chrono::Duration::minutes(1);
        sweep_stalls(&library, later).await;
        let work = library.notify_send_work(&delivery).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Failed);
        assert_eq!(work.attempts, 1);
        let dest = work.destination.expect("destination");
        assert_eq!(dest.id, channel);
        assert_eq!(dest.failure_count, 1);
        assert!(fake.sent.lock().unwrap().is_empty());
        // Its job was cancelled, so nothing is left to claim it.
        assert!(stalled(&library, later).await.is_empty());
    }

    #[tokio::test]
    async fn a_send_waiting_behind_other_work_is_not_a_stall() {
        let (state, _fake, channel) = daemon(Vec::new()).await;
        let library = state.library_snapshot().await;
        // A notifier that runs on a live node: its send can still be
        // claimed, however long it has waited.
        let instance = bookclerk_library::control_plane::create_plugin_instance(
            &library,
            &bookclerk_library::control_plane::ConfigActor::Operator {
                id: "operator".into(),
            },
            "path:file:///tmp/notifier#notifier",
            "",
        )
        .await
        .expect("instance");
        let deployment = bookclerk_library::control_plane::ensure_plugin_deployment(
            &library,
            &bookclerk_library::control_plane::ConfigActor::Bootstrap,
            &instance.id,
            "node-a",
            bookclerk_library::control_plane::DesiredState::Present,
        )
        .await
        .expect("deployment");
        bookclerk_library::control_plane::upsert_observation(
            &library,
            &bookclerk_library::control_plane::DeploymentObservation {
                deployment_id: deployment.deployment_id,
                node_id: "node-a".into(),
                incarnation: "test".into(),
                status: bookclerk_library::control_plane::DeploymentStatus::Running,
                detail: String::new(),
                applied_config_revision: None,
                observed_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .await
        .expect("observation");
        bookclerk_library::test_support::register_test_node(&library, "node-a").await;
        bookclerk_library::control_plane::record_node_heartbeat(
            &library,
            "node-a",
            &bookclerk_library::control_plane::NodeHeartbeat {
                incarnation: "test".into(),
                started_at: chrono::Utc::now().to_rfc3339(),
                interval: std::time::Duration::from_secs(15),
                cpu_millicores: 1000,
                memory_bytes: None,
                cpu_used_millicores: None,
                memory_available_bytes: None,
                files_dir_free_bytes: None,
                cache: None,
                public_origin: None,
            },
        )
        .await
        .expect("heartbeat");
        let busy = library
            .create_notify_destination(&NewDestination {
                plugin_instance_id: instance.id.as_str().to_string(),
                kind: DestinationKind::Channel,
                user_id: None,
                label: "Busy".into(),
                address_json: String::new(),
            })
            .await
            .expect("channel")
            .id;
        library
            .set_notify_routes(
                "test",
                &[
                    Route {
                        destination_id: channel.clone(),
                        min_severity: NotificationSeverity::Info,
                    },
                    Route {
                        destination_id: busy.clone(),
                        min_severity: NotificationSeverity::Info,
                    },
                ],
            )
            .await
            .expect("routes");
        routed_under(&library, 1_000).await;
        let later = chrono::Utc::now() + chrono::Duration::minutes(1);
        sweep_stalls(&library, later).await;
        // Only the send whose notifier runs nowhere was failed; the other
        // is still pending, and its destination counts no failure.
        let left = stalled(&library, later).await;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].notifier.as_deref(), Some(instance.id.as_str()));
        let waiting = library
            .notify_send_work(left[0].delivery_id.as_deref().expect("delivery"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(waiting.state, DeliveryState::Pending);
        assert_eq!(waiting.destination.map(|d| d.failure_count), Some(0));
        let ops = library.notify_destination(&channel).await.unwrap().unwrap();
        assert_eq!(ops.failure_count, 1);
    }

    #[tokio::test]
    async fn a_stalled_job_without_a_delivery_is_cancelled_and_a_claimed_one_is_left_alone() {
        let (state, _fake, _) = daemon(Vec::new()).await;
        let library = state.library_snapshot().await;
        let later = chrono::Utc::now() + chrono::Duration::minutes(1);
        // A job whose delivery already finished is still cancelled, so such
        // jobs cannot fill every page ahead of ones that have a delivery.
        let orphan = routed_under(&library, 1_000)
            .await
            .notify_delivery
            .expect("delivery");
        assert!(library
            .record_notify_dropped(&orphan, "gone", None)
            .await
            .unwrap());
        let live = routed_under(&library, 1_000)
            .await
            .notify_delivery
            .expect("delivery");
        let read = stalled(&library, later).await;
        assert_eq!(read.len(), 2);
        assert_eq!(
            read.iter().filter(|s| s.delivery_id.is_none()).count(),
            1,
            "the finished delivery's job is listed without it"
        );
        let snapshot = read
            .iter()
            .find(|s| s.delivery_id.as_deref() == Some(live.as_str()))
            .expect("live job")
            .clone();
        // A worker claims the job after the sweep read it: the stale
        // snapshot changes nothing, and the claim keeps its lease.
        let claimed = library
            .claim_job_by_id(&snapshot.job_id, snapshot.lease_generation, "worker", 60)
            .await
            .unwrap()
            .expect("claimed");
        assert_eq!(
            library
                .fail_stalled_notify_send(&snapshot, STALLED_REASON)
                .await
                .unwrap(),
            StallOutcome::NotStalled
        );
        let job = library.get_job(&snapshot.job_id).await.unwrap().unwrap();
        assert_eq!(job.state, bookclerk_library::JobState::Running);
        assert_eq!(job.lease_owner.as_deref(), Some("worker"));
        let work = library.notify_send_work(&live).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Pending);
        // Handed back to the queue without a new run time, the job is
        // pending again at its old readiness; only its lease generation
        // tells the snapshot is stale, and that alone keeps it untouched.
        assert!(library
            .release_job_claim(&claimed.fence().expect("fence"), false)
            .await
            .unwrap());
        let job = library.get_job(&snapshot.job_id).await.unwrap().unwrap();
        assert_eq!(job.state, bookclerk_library::JobState::Pending);
        assert_eq!(
            stalled(&library, later)
                .await
                .into_iter()
                .find(|s| s.job_id == snapshot.job_id)
                .expect("still listed")
                .run_after,
            snapshot.run_after
        );
        assert_eq!(
            library
                .fail_stalled_notify_send(&snapshot, STALLED_REASON)
                .await
                .unwrap(),
            StallOutcome::NotStalled
        );
        let work = library.notify_send_work(&live).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Pending);
        // The sweep cancels both and fails only the live delivery.
        sweep_stalls(&library, later).await;
        assert!(stalled(&library, later).await.is_empty());
        let work = library.notify_send_work(&live).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Failed);
        let work = library.notify_send_work(&orphan).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Dropped);
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

    /// A user with `email`, and an account mail of `kind` queued for them
    /// through `fake-1`; answers the user id and the send job's payload.
    async fn queued_mail(
        library: &LibraryStore,
        kind: bookclerk_library::notifications::AccountMailKind,
        email: &str,
    ) -> (i64, JobPayload) {
        let user = library
            .create_user_with_profile(
                bookclerk_library::UserRole::Member,
                Some("Sam"),
                None,
                Some(email),
                None,
            )
            .await
            .expect("user");
        let limits = NotifyQueueLimits {
            max_pending: 1_000,
            max_attempts: 3,
        };
        let id = library
            .queue_account_mail(kind, user.id, email, "fake-1", limits)
            .await
            .expect("queued");
        (
            user.id,
            JobPayload {
                account_mail: Some(id),
                ..JobPayload::default()
            },
        )
    }

    /// The value of `name` in the query of `link`.
    fn query_value(link: &str, name: &str) -> String {
        let query = link.split_once('?').expect("query").1;
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .expect("value")
            .to_string()
    }

    #[tokio::test]
    async fn an_invite_mints_its_ticket_at_send_and_only_the_newest_works() {
        use bookclerk_library::notifications::AccountMailKind;
        let (state, fake, _) = daemon(vec![
            Err(PluginError::rate_limited("slow down", None)),
            Ok(NotifySent { provider_id: None }),
        ])
        .await;
        state.config.write().await.integrations.public_origin =
            Some("https://library.example".into());
        let library = state.library_snapshot().await;
        let (user_id, payload) =
            queued_mail(&library, AccountMailKind::Invite, "sam@example.com").await;
        let id = payload.account_mail.clone().unwrap();
        // Without a lease the parked retry falls back to the job's own
        // retry; the attempt is counted.
        assert!(run_notify_send(&state, &payload, None).await.is_err());
        run_notify_send(&state, &payload, None).await.expect("sent");
        let mail = library.account_mail(&id).await.unwrap().unwrap();
        assert_eq!(mail.state, DeliveryState::Delivered);
        assert_eq!(mail.attempts, 2);
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        for params in &sent {
            assert!(params.message.sensitive);
            assert_eq!(
                params.destination.kind,
                bookclerk_plugin_abi::NotifyDestinationKind::Personal
            );
            assert_eq!(
                params.destination.address_json,
                r#"{"email":"sam@example.com"}"#
            );
        }
        let links: Vec<String> = sent
            .iter()
            .map(|p| p.message.link.clone().expect("link"))
            .collect();
        assert!(links[1].starts_with("https://library.example/invite?ticket="));
        let first = query_value(&links[0], "ticket");
        let newest = query_value(&links[1], "ticket");
        assert_ne!(first, newest, "a retry mints a new ticket");
        let hash = |raw: &str| bookclerk_library::hash_token(raw);
        // The retry voided the first ticket.
        assert!(library
            .get_claim_ticket_by_hash(&hash(&first))
            .await
            .unwrap()
            .is_none());
        let ticket = library
            .get_claim_ticket_by_hash(&hash(&newest))
            .await
            .unwrap()
            .expect("ticket");
        assert_eq!(ticket.emailed_to.as_deref(), Some("sam@example.com"));
        // Using it proves the address.
        assert!(library
            .verify_user_email_if(user_id, "sam@example.com")
            .await
            .unwrap());
        assert!(
            library
                .get_user(user_id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
    }

    #[tokio::test]
    async fn a_verification_link_works_once_for_the_address_it_was_sent_to() {
        use bookclerk_library::notifications::{parse_verify_email_request, AccountMailKind};
        let (state, fake, _) = daemon(vec![
            Ok(NotifySent { provider_id: None }),
            Ok(NotifySent { provider_id: None }),
        ])
        .await;
        state.config.write().await.integrations.public_origin =
            Some("https://library.example/".into());
        let library = state.library_snapshot().await;
        let (user_id, payload) =
            queued_mail(&library, AccountMailKind::VerifyEmail, "kim@example.com").await;
        run_notify_send(&state, &payload, None).await.expect("sent");
        let link = fake.sent.lock().unwrap()[0].message.link.clone().unwrap();
        assert!(link.starts_with("https://library.example/verify-email?token="));
        let body = format!(r#"{{"token":"{}"}}"#, query_value(&link, "token"));
        let token = parse_verify_email_request(body.as_bytes()).expect("token");
        let now = chrono::Utc::now();
        assert_eq!(
            library.use_email_verification(&token, now).await.unwrap(),
            Some(user_id)
        );
        assert!(
            library
                .get_user(user_id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        assert_eq!(
            library.use_email_verification(&token, now).await.unwrap(),
            None
        );

        // A link for an address the patron has since changed verifies
        // nothing, and changing it clears the mark.
        let id = library
            .queue_account_mail(
                AccountMailKind::VerifyEmail,
                user_id,
                "kim@example.com",
                "fake-1",
                NotifyQueueLimits {
                    max_pending: 1_000,
                    max_attempts: 3,
                },
            )
            .await
            .unwrap();
        library
            .set_user_email(user_id, Some("kim@example.org"))
            .await
            .unwrap();
        assert!(
            !library
                .get_user(user_id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        let payload = JobPayload {
            account_mail: Some(id.clone()),
            ..JobPayload::default()
        };
        run_notify_send(&state, &payload, None)
            .await
            .expect("dropped");
        let mail = library.account_mail(&id).await.unwrap().unwrap();
        assert_eq!(mail.state, DeliveryState::Dropped);
        assert_eq!(fake.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_test_needs_no_public_origin_but_an_invite_does() {
        use bookclerk_library::notifications::AccountMailKind;
        let (state, fake, _) = daemon(vec![Ok(NotifySent { provider_id: None })]).await;
        let library = state.library_snapshot().await;
        library
            .set_account_mail_instance(Some("fake-1"))
            .await
            .unwrap();
        let (_, invite) = queued_mail(&library, AccountMailKind::Invite, "a@example.com").await;
        run_notify_send(&state, &invite, None)
            .await
            .expect("dropped");
        let mail = library
            .account_mail(invite.account_mail.as_deref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mail.state, DeliveryState::Dropped);
        assert!(fake.sent.lock().unwrap().is_empty());

        let (_, test) = queued_mail(&library, AccountMailKind::Test, "b@example.com").await;
        run_notify_send(&state, &test, None).await.expect("sent");
        let sent = fake.sent.lock().unwrap()[0].clone();
        assert!(sent.message.link.is_none());
        assert!(sent.message.sensitive);
        let settings = library.account_mail_settings().await.unwrap().unwrap();
        assert!(
            settings.tested_at.is_some(),
            "a delivered test marks the notifier tested"
        );
    }

    #[tokio::test]
    async fn an_account_mail_no_notifier_claims_fails_after_the_stall_wait() {
        use bookclerk_library::notifications::AccountMailKind;
        let (state, _fake, _) = daemon(Vec::new()).await;
        let library = state.library_snapshot().await;
        let (_, payload) = queued_mail(&library, AccountMailKind::Test, "c@example.com").await;
        let id = payload.account_mail.unwrap();
        let later = chrono::Utc::now() + chrono::Duration::minutes(1);
        let found = stalled(&library, later).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].account_mail_id.as_deref(), Some(id.as_str()));
        assert_eq!(found[0].notifier.as_deref(), Some("fake-1"));
        sweep_stalls(&library, later).await;
        let mail = library.account_mail(&id).await.unwrap().unwrap();
        assert_eq!(mail.state, DeliveryState::Failed);
        assert!(stalled(&library, later).await.is_empty());
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
