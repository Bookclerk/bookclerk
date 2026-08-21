//! Durable domain-event dispatcher and fenced delivery worker.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bookclerk_integrations::{DomainEvent, EventResult, EventSubscription};
use bookclerk_library::{
    catalog_subscribers_for_event, subscription_matches_event, wake_grants_from_subscriptions,
    EventCatalogSubscription, LibraryStore,
};
use chrono::{TimeZone, Utc};
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;
use crate::job_worker::{
    apply_heartbeat_tick, classify_heartbeat, HeartbeatDecision, HeartbeatTick,
};

/// How long a worker may replay a lost `claim_next_event_delivery` RPC.
const CLAIM_REPLAY_BUDGET: Duration = Duration::from_secs(2);
/// Delay between bounded delivery-claim replay attempts.
const CLAIM_REPLAY_DELAY: Duration = Duration::from_millis(200);
/// Lease duration granted to a delivery worker on claim.
const LEASE_SECS: u64 = 60;
/// Coarse independent cadence for event retention (not the dispatcher tick).
const EVENT_PRUNE_INTERVAL: Duration = Duration::from_secs(3600);
/// Force a bounded late-join reconcile at least every ~60s catalog TTL.
const RECONCILE_BACKSTOP_TICKS: u32 = 12;
/// Page size for `wake_pending` replay after pending dispatch.
const WAKE_PENDING_PAGE: u64 = 32;

/// Dispatch undispatched outbox rows and run configured delivery workers.
pub fn start_event_runtime(state: Arc<AppState>) {
    tokio::spawn(async move {
        let files_dir = state.config.read().await.paths().files_dir.clone();
        let node_id = resolve_event_node_id(&files_dir);
        let _ = state.event_node_id.set(node_id);
        upsert_event_subscriber_catalog(&state).await;
        let (retention_days, dead_letter_retention_days, workers) = {
            let cfg = state.config.read().await;
            (
                cfg.events.retention_days,
                cfg.events.dead_letter_retention_days,
                cfg.events.concurrency.max(1),
            )
        };
        let library = state.library_snapshot().await;
        if let Err(err) = library
            .prune_event_deliveries(retention_days, dead_letter_retention_days)
            .await
        {
            warn!(error = %err, "event retention prune failed");
        }
        info!(event_workers = workers, "starting durable event dispatcher");
        spawn_dispatcher(state.clone());
        spawn_event_pruner(state.clone());
        for _ in 0..workers {
            spawn_delivery_worker(state.clone());
        }
    });
}

/// Tick the outbox dispatcher on notify and a 5s idle interval.
fn spawn_dispatcher(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut idle = tokio::time::interval(Duration::from_secs(5));
        idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_catalog_fp = String::new();
        let mut last_reconcile_empty = false;
        let mut ticks_since_reconcile = 0u32;
        loop {
            upsert_event_subscriber_catalog(&state).await;
            let library = state.library_snapshot().await;
            let (retention_days, files_dir) = {
                let cfg = state.config.read().await;
                (cfg.events.retention_days, cfg.paths().files_dir.clone())
            };
            let node_id = event_node_id(&state, &files_dir);
            let more = match dispatch_pending(
                &library,
                retention_days,
                &last_catalog_fp,
                last_reconcile_empty,
                ticks_since_reconcile,
                &node_id,
            )
            .await
            {
                Ok((fp, empty, reconciled, more)) => {
                    last_catalog_fp = fp;
                    last_reconcile_empty = empty;
                    ticks_since_reconcile = if reconciled {
                        0
                    } else {
                        ticks_since_reconcile.saturating_add(1)
                    };
                    more
                }
                Err(err) => {
                    warn!(error = %err, "event dispatch failed");
                    last_reconcile_empty = false;
                    false
                }
            };
            state.job_notify.notify_waiters();
            if more {
                continue;
            }
            tokio::select! {
                () = state.job_notify.notified() => {}
                _ = idle.tick() => {}
            }
        }
    });
}

/// Prune terminal deliveries and expired catalog rows on a coarse cadence.
fn spawn_event_pruner(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut idle = tokio::time::interval(EVENT_PRUNE_INTERVAL);
        idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        idle.tick().await;
        loop {
            let (retention_days, dead_letter_retention_days) = {
                let cfg = state.config.read().await;
                (
                    cfg.events.retention_days,
                    cfg.events.dead_letter_retention_days,
                )
            };
            let library = state.library_snapshot().await;
            if let Err(err) = library
                .prune_event_deliveries(retention_days, dead_letter_retention_days)
                .await
            {
                warn!(error = %err, "event retention prune failed");
            }
            idle.tick().await;
        }
    });
}

/// Claim and run deliveries, reclaiming expired leases between idle waits.
fn spawn_delivery_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let owner = format!("event-{}", Uuid::new_v4());
        let mut idle = tokio::time::interval(Duration::from_secs(5));
        idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let library = state.library_snapshot().await;
            if let Err(err) = library.reclaim_expired_event_deliveries().await {
                warn!(error = %err, "event delivery reclaim failed");
            }
            let files_dir = state.config.read().await.paths().files_dir.clone();
            let max_in_flight = state.config.read().await.events.concurrency.max(1);
            let node_id = event_node_id(&state, &files_dir);
            match claim_delivery(
                &library,
                &owner,
                &loaded_plugin_ids(&state).await,
                max_in_flight,
                &node_id,
            )
            .await
            {
                Ok(Some(delivery)) => {
                    run_delivery(&state, &library, delivery).await;
                }
                Ok(None) => {
                    tokio::select! {
                        () = state.job_notify.notified() => {}
                        _ = idle.tick() => {}
                    }
                }
                Err(err) => {
                    warn!(error = %err, "claim_next_event_delivery failed");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });
}

/// Upsert this node's discovered + loaded integration subscriptions.
pub async fn upsert_event_subscriber_catalog(state: &AppState) {
    let cfg = state.config.read().await.clone();
    let node_id = event_node_id(state, &cfg.paths().files_dir);
    let discovered = tokio::task::spawn_blocking({
        let cfg = cfg.clone();
        move || bookclerk_plugin_host::discover_plugins(&cfg)
    })
    .await;
    let library = state.library_snapshot().await;
    match discovered {
        Ok(Ok(plugins)) => {
            for plugin in plugins {
                if plugin.manifest.kind.as_str() != "integration" {
                    continue;
                }
                let enabled = cfg.integrations.is_enabled(&plugin.manifest.id);
                let subs = catalog_from_manifest(&plugin);
                if let Err(err) = library
                    .upsert_event_subscriber(&node_id, &plugin.manifest.id, &subs, enabled)
                    .await
                {
                    warn!(
                        plugin = %plugin.manifest.id,
                        error = %err,
                        "event subscriber catalog upsert failed"
                    );
                }
            }
        }
        Ok(Err(err)) => {
            warn!(error = %err, "plugin discovery for event catalog failed");
        }
        Err(err) => {
            warn!(error = %err, "plugin discovery task for event catalog failed");
        }
    }
    let integrations = state.integrations.read().await;
    for integration in integrations.all() {
        let subs = catalog_from_runtime(&integration.event_subscriptions());
        if let Err(err) = library
            .upsert_event_subscriber(&node_id, integration.id(), &subs, true)
            .await
        {
            warn!(
                plugin = %integration.id(),
                error = %err,
                "loaded integration catalog upsert failed"
            );
        }
    }
}

/// Process-stable per-files-dir node id used as the catalog heartbeat key.
fn event_node_id(state: &AppState, files_dir: &Path) -> String {
    state
        .event_node_id
        .get_or_init(|| resolve_event_node_id(files_dir))
        .clone()
}

/// Read or mint the node id once. Persistence is best-effort.
fn resolve_event_node_id(files_dir: &Path) -> String {
    let path = files_dir.join("event_node_id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let id = Uuid::new_v4().to_string();
    if let Err(err) = std::fs::create_dir_all(files_dir) {
        warn!(error = %err, "event_node_id directory create failed");
        return id;
    }
    if let Err(err) = std::fs::write(&path, &id) {
        warn!(error = %err, "event_node_id write failed");
    }
    id
}

/// Map `plugin.toml` subscriptions onto the durable catalog JSON shape.
fn catalog_from_manifest(
    plugin: &bookclerk_plugin_host::DiscoveredPlugin,
) -> Vec<EventCatalogSubscription> {
    plugin
        .manifest
        .capabilities
        .events
        .subscriptions
        .iter()
        .map(|s| EventCatalogSubscription {
            event_type: s.event_type.clone(),
            schema_versions: s.schema_versions.clone(),
            supports_suspend: s.supports_suspend,
            resource_class: if s.resource_class.trim().is_empty() {
                "network".into()
            } else {
                s.resource_class.clone()
            },
            filter: s.filter.clone().filter(|v| !v.is_null()),
        })
        .collect()
}

/// Map loaded in-process / guest runtime subscriptions onto the catalog shape.
fn catalog_from_runtime(subs: &[EventSubscription]) -> Vec<EventCatalogSubscription> {
    subs.iter()
        .map(|s| EventCatalogSubscription {
            event_type: s.event_type.clone(),
            schema_versions: s.schema_versions.clone(),
            supports_suspend: s.supports_suspend,
            resource_class: if s.resource_class.trim().is_empty() {
                "network".into()
            } else {
                s.resource_class.clone()
            },
            filter: s.filter.clone(),
        })
        .collect()
}

/// Create deliveries from the live catalog for pending events, then late-join.
///
/// Caps undispatched dispatch per tick (`WAKE_PENDING_PAGE`) so a backlog cannot
/// starve wake processing or the catalog heartbeat. Always runs a wake slice
/// after that cap. The outer loop continues when either undispatched remain or
/// `wake still_pending`.
pub(crate) async fn dispatch_pending(
    library: &LibraryStore,
    retention_days: u64,
    last_catalog_fp: &str,
    last_reconcile_empty: bool,
    ticks_since_reconcile: u32,
    _node_id: &str,
) -> Result<(String, bool, bool, bool), bookclerk_library::LibraryError> {
    let catalog = library.list_live_event_subscribers().await?;
    let fingerprint = catalog_fingerprint(&catalog);
    let mut dispatched = 0u64;
    let mut dispatch_more = false;
    loop {
        let Some(event) = library.next_undispatched_event().await? else {
            break;
        };
        let subs = catalog_subscribers_for_event(&catalog, &event);
        if subs.is_empty() {
            let op = format!("dispatch-{}", event.id);
            let n = library
                .dispatch_event_deliveries(&event.id, &[], &op)
                .await?;
            info!(
                event_id = %event.id,
                event_type = %event.event_type,
                deliveries = n,
                "dispatched domain event"
            );
        } else {
            let mut n = 0u32;
            for sub in &subs {
                let op = format!("dispatch-{}-{}", event.id, sub.plugin_id);
                n += library
                    .dispatch_event_deliveries(&event.id, std::slice::from_ref(sub), &op)
                    .await?;
            }
            info!(
                event_id = %event.id,
                event_type = %event.event_type,
                deliveries = n,
                "dispatched domain event"
            );
        }
        dispatched = dispatched.saturating_add(1);
        if dispatched >= WAKE_PENDING_PAGE {
            dispatch_more = true;
            break;
        }
    }
    let wake_token = Uuid::new_v4().to_string();
    let wakes = library
        .process_pending_wakes(WAKE_PENDING_PAGE, &wake_token, LEASE_SECS)
        .await?;
    let created_after =
        Utc::now() - chrono::Duration::days(i64::try_from(retention_days.max(1)).unwrap_or(7));
    let skip_reconcile = should_skip_catalog_reconcile(
        &fingerprint,
        last_catalog_fp,
        last_reconcile_empty,
        ticks_since_reconcile,
    );
    let n = if skip_reconcile {
        0
    } else {
        library.reconcile_catalog_deliveries(created_after).await?
    };
    if n > 0 {
        info!(deliveries = n, "reconciled late-join event deliveries");
    }
    let empty = if skip_reconcile {
        last_reconcile_empty
    } else {
        n == 0
    };
    Ok((
        fingerprint,
        empty,
        !skip_reconcile,
        dispatch_more || wakes.still_pending,
    ))
}

/// True when the process-local skip cache may omit this tick's late-join scan.
fn should_skip_catalog_reconcile(
    fingerprint: &str,
    last_catalog_fp: &str,
    last_reconcile_empty: bool,
    ticks_since_reconcile: u32,
) -> bool {
    fingerprint == last_catalog_fp
        && last_reconcile_empty
        && ticks_since_reconcile < RECONCILE_BACKSTOP_TICKS
}

/// Stable fingerprint of live plugin ids, enablement, and subscription JSON.
fn catalog_fingerprint(catalog: &[bookclerk_library::EventSubscriberCatalogRecord]) -> String {
    let mut parts: Vec<String> = catalog
        .iter()
        .map(|row| {
            let subs = serde_json::to_string(&row.subscriptions).unwrap_or_else(|_| "[]".into());
            format!("{}:{}:{subs}", row.plugin_id, row.enabled)
        })
        .collect();
    parts.sort();
    parts.join("|")
}

/// Plugin ids currently loaded on this process.
async fn loaded_plugin_ids(state: &AppState) -> Vec<String> {
    let integrations = state.integrations.read().await;
    integrations
        .all()
        .iter()
        .map(|i| i.id().to_string())
        .collect()
}

/// Claim the next ready delivery, replaying a lost RPC within [`CLAIM_REPLAY_BUDGET`].
async fn claim_delivery(
    library: &LibraryStore,
    owner: &str,
    plugin_ids: &[String],
    max_in_flight: u32,
    node_id: &str,
) -> Result<Option<bookclerk_library::EventDeliveryRecord>, bookclerk_library::LibraryError> {
    if plugin_ids.is_empty() {
        return Ok(None);
    }
    let operation_id = Uuid::new_v4().to_string();
    let deadline = Instant::now() + CLAIM_REPLAY_BUDGET;
    loop {
        match library
            .claim_next_event_delivery(
                owner,
                LEASE_SECS,
                &operation_id,
                plugin_ids,
                max_in_flight.max(1),
                node_id,
            )
            .await
        {
            Ok(row) => return Ok(row),
            Err(bookclerk_library::LibraryError::Unavailable(msg)) => {
                if Instant::now() >= deadline {
                    return Err(bookclerk_library::LibraryError::Unavailable(format!(
                        "claim replay budget exhausted: {msg}"
                    )));
                }
                tokio::time::sleep(CLAIM_REPLAY_DELAY).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Invoke `Integration.onEvent` and persist the fenced [`EventResult`].
async fn run_delivery(
    state: &AppState,
    library: &LibraryStore,
    delivery: bookclerk_library::EventDeliveryRecord,
) {
    let fence = delivery.fence();
    let Some(event) = library
        .get_domain_event(&delivery.event_id)
        .await
        .ok()
        .flatten()
    else {
        let _ = library
            .dead_letter_event_delivery(&fence, "parent event missing")
            .await;
        return;
    };
    let integration = {
        let integrations = state.integrations.read().await;
        integrations.get(&delivery.plugin_id)
    };
    let Some(integration) = integration else {
        let restore_resume = delivery.checkpoint_json.is_some();
        let _ = library
            .release_unexecuted_event_delivery(&fence, restore_resume)
            .await;
        return;
    };
    let local_subs = catalog_from_runtime(&integration.event_subscriptions());
    if !local_subs
        .iter()
        .any(|s| subscription_matches_event(s, &event))
    {
        let restore_resume = delivery.checkpoint_json.is_some();
        warn!(
            delivery_id = %delivery.id,
            plugin = %delivery.plugin_id,
            "local catalog does not match claimed event; releasing unused claim"
        );
        let _ = library
            .release_unexecuted_event_delivery(&fence, restore_resume)
            .await;
        return;
    }
    let domain = domain_event_for_delivery(&event, &delivery);
    let cancel = Arc::new(AtomicBool::new(false));
    let operator_cancel = Arc::new(AtomicBool::new(false));
    let heartbeat = {
        let library = library.clone();
        let fence = fence.clone();
        let cancel = Arc::clone(&cancel);
        let operator_cancel = Arc::clone(&operator_cancel);
        tokio::spawn(async move {
            let lease = Duration::from_secs(LEASE_SECS);
            let mut confirmed_until = Instant::now() + lease;
            let mut tick = tokio::time::interval(Duration::from_secs(LEASE_SECS.max(10) / 3));
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match library
                    .event_delivery_cancel_requested(&fence.delivery_id)
                    .await
                {
                    Ok(true) => {
                        warn!(
                            delivery_id = %fence.delivery_id,
                            "event delivery cancel requested; cancelling onEvent"
                        );
                        operator_cancel.store(true, Ordering::SeqCst);
                        cancel.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(false) => {}
                    Err(err) => {
                        warn!(
                            delivery_id = %fence.delivery_id,
                            error = %err,
                            "event_delivery_cancel_requested failed; continuing heartbeat"
                        );
                    }
                }
                let classified =
                    classify_heartbeat(library.heartbeat_event_delivery(&fence, LEASE_SECS).await);
                if let HeartbeatTick::Transient(err) = &classified {
                    warn!(
                        delivery_id = %fence.delivery_id,
                        error = %err,
                        "heartbeat_event_delivery failed; will retry until confirmed lease expires"
                    );
                }
                match apply_heartbeat_tick(&classified, Instant::now(), confirmed_until, lease) {
                    HeartbeatDecision::Continue {
                        confirmed_until: next,
                    } => {
                        confirmed_until = next;
                    }
                    HeartbeatDecision::StopFenceLost => {
                        warn!(
                            delivery_id = %fence.delivery_id,
                            "event delivery fence lost; cancelling onEvent"
                        );
                        cancel.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        })
    };
    let started = Instant::now();
    let result = match integration
        .deliver_domain_event_cancelable(domain, Arc::clone(&cancel))
        .await
    {
        Ok(result) => result,
        Err(err) => {
            heartbeat.abort();
            let _ = library
                .record_event_handler_latency(
                    i64::try_from(started.elapsed().as_millis()).unwrap_or(0),
                )
                .await;
            if operator_cancel.load(Ordering::SeqCst) {
                warn!(
                    delivery_id = %delivery.id,
                    "event delivery cancelled by operator"
                );
                let _ = library
                    .reject_event_delivery(&fence, "cancelled by operator")
                    .await;
                return;
            }
            if cancel.load(Ordering::SeqCst) {
                warn!(
                    delivery_id = %delivery.id,
                    "event delivery fence lost; ignoring handler result"
                );
                return;
            }
            warn!(
                delivery_id = %delivery.id,
                plugin = %delivery.plugin_id,
                error = %err,
                "event delivery invocation failed"
            );
            let _ = library.fail_event_delivery(&fence, &err.to_string()).await;
            return;
        }
    };
    heartbeat.abort();
    let _ = library
        .record_event_handler_latency(i64::try_from(started.elapsed().as_millis()).unwrap_or(0))
        .await;
    if operator_cancel.load(Ordering::SeqCst) {
        warn!(
            delivery_id = %delivery.id,
            "event delivery cancelled by operator"
        );
        let _ = library
            .reject_event_delivery(&fence, "cancelled by operator")
            .await;
        return;
    }
    if cancel.load(Ordering::SeqCst) {
        warn!(
            delivery_id = %delivery.id,
            "event delivery fence lost; ignoring handler result"
        );
        return;
    }
    let persist = match result {
        EventResult::Ack => {
            info!(
                delivery_id = %delivery.id,
                plugin = %delivery.plugin_id,
                outcome = "ack",
                "event delivery completed"
            );
            library.ack_event_delivery(&fence).await
        }
        EventResult::Retry {
            retry_at_unix_ms,
            reason,
        } => {
            let wake = unix_ms_to_utc(retry_at_unix_ms)
                .unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(5));
            if delivery.attempt_count >= delivery.max_attempts {
                warn!(
                    delivery_id = %delivery.id,
                    plugin = %delivery.plugin_id,
                    outcome = "dead_letter",
                    %reason,
                    attempts = delivery.attempt_count,
                    "event delivery retry exhausted"
                );
            } else {
                info!(
                    delivery_id = %delivery.id,
                    plugin = %delivery.plugin_id,
                    outcome = "retry",
                    %reason,
                    "event delivery retry scheduled"
                );
            }
            library.retry_event_delivery(&fence, wake, &reason).await
        }
        EventResult::Reject { reason } => {
            warn!(
                delivery_id = %delivery.id,
                plugin = %delivery.plugin_id,
                outcome = "reject",
                %reason,
                "event delivery rejected"
            );
            library.reject_event_delivery(&fence, &reason).await
        }
        EventResult::DeadLetter { reason } => {
            warn!(
                delivery_id = %delivery.id,
                plugin = %delivery.plugin_id,
                outcome = "dead_letter",
                %reason,
                "event delivery dead-lettered"
            );
            library.dead_letter_event_delivery(&fence, &reason).await
        }
        EventResult::Suspended {
            checkpoint_json,
            checkpoint_schema_version,
            wake_at_unix_ms,
            wake_on_event_type,
            wake_on_filter_json,
        } => {
            let allowed = suspend_allowed(
                &integration.event_subscriptions(),
                &event.event_type,
                event.schema_version,
            );
            if !allowed {
                warn!(
                    delivery_id = %delivery.id,
                    plugin = %delivery.plugin_id,
                    "event suspend not advertised; treating as reject"
                );
                library
                    .reject_event_delivery(&fence, "suspend not advertised for this event type")
                    .await
            } else {
                match wake_grants_from_subscriptions(
                    &catalog_from_runtime(&integration.event_subscriptions()),
                    &wake_on_event_type,
                    &wake_on_filter_json,
                ) {
                    Err(reason) => {
                        warn!(
                            delivery_id = %delivery.id,
                            plugin = %delivery.plugin_id,
                            wake_on_event_type = %wake_on_event_type,
                            reason,
                            "wake grant rejected; treating as reject"
                        );
                        library.reject_event_delivery(&fence, reason).await
                    }
                    Ok(grants) => {
                        let grants_json = if grants.is_empty() {
                            String::new()
                        } else {
                            serde_json::to_string(&grants).unwrap_or_default()
                        };
                        let wake = if !wake_on_event_type.trim().is_empty() && wake_at_unix_ms == 0
                        {
                            far_future_wake()
                        } else {
                            unix_ms_to_utc(wake_at_unix_ms)
                                .unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(30))
                        };
                        info!(
                            delivery_id = %delivery.id,
                            plugin = %delivery.plugin_id,
                            outcome = "suspended",
                            "event delivery suspended"
                        );
                        library
                            .suspend_event_delivery(
                                &fence,
                                &checkpoint_json,
                                i64::from(checkpoint_schema_version),
                                wake,
                                &wake_on_event_type,
                                &wake_on_filter_json,
                                &grants_json,
                            )
                            .await
                    }
                }
            }
        }
    };
    if let Err(err) = persist {
        warn!(delivery_id = %delivery.id, error = %err, "event delivery persist failed");
    }
}

/// True when a subscription for this exact type and schema advertises suspend.
fn suspend_allowed(subs: &[EventSubscription], event_type: &str, schema_version: i64) -> bool {
    subs.iter().any(|s| {
        s.event_type == event_type
            && s.supports_suspend
            && s.schema_versions
                .iter()
                .any(|v| i64::from(*v) == schema_version)
    })
}

/// Parse a unix-ms timestamp into UTC, ignoring overflows.
fn unix_ms_to_utc(ms: u64) -> Option<chrono::DateTime<Utc>> {
    let ms = i64::try_from(ms).ok()?;
    Utc.timestamp_millis_opt(ms).single()
}

/// Sentinel `run_after` for event-only suspends (`wakeAtUnixMs == 0`).
fn far_future_wake() -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339("9999-12-31T23:59:59+00:00")
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now() + chrono::Duration::days(36500))
}

/// Map an outbox envelope plus the claimed delivery onto an ABI [`DomainEvent`].
///
/// Claim clears `resume_pending` on the row; a stored checkpoint still means
/// this invocation continues a prior [`EventResult::Suspended`].
fn domain_event_for_delivery(
    event: &bookclerk_library::DomainEventRecord,
    delivery: &bookclerk_library::EventDeliveryRecord,
) -> DomainEvent {
    DomainEvent {
        event_id: event.id.clone(),
        event_type: event.event_type.clone(),
        schema_version: u32::try_from(event.schema_version).unwrap_or(1),
        occurred_at_unix_ms: u64::try_from(event.occurred_at.timestamp_millis()).unwrap_or(0),
        account_id: event.account_id.clone(),
        source: event.source.clone(),
        correlation_id: event.correlation_id.clone(),
        causation_id: event.causation_id.clone(),
        deduplication_key: delivery.idempotency_key.clone(),
        delivery_attempt: u32::try_from(delivery.attempt_count.max(1)).unwrap_or(1),
        payload: event.payload.as_bytes().to_vec(),
        checkpoint_json: delivery.checkpoint_json.clone().unwrap_or_default(),
        checkpoint_schema_version: u32::try_from(delivery.checkpoint_schema_version).unwrap_or(0),
        invocation_sequence: u32::try_from(delivery.invocation_sequence).unwrap_or(0),
        resume_pending: delivery.checkpoint_json.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{
        DomainEventRecord, EventDeliveryRecord, EventSubscriber, PublishDomainEventOutcome,
        PublishDomainEventSpec,
    };
    use chrono::TimeZone;

    fn sample_event() -> DomainEventRecord {
        DomainEventRecord {
            id: "evt-1".into(),
            event_type: "book_acquired".into(),
            schema_version: 1,
            occurred_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            account_id: "acct".into(),
            source: String::new(),
            correlation_id: "corr".into(),
            causation_id: String::new(),
            dedup_key: "book_acquired:u1".into(),
            payload: r#"{"titleId":"u1"}"#.into(),
            ordering_key: "u1".into(),
            dispatch_state: "dispatched".into(),
            created_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            wake_pending: false,
        }
    }

    fn sample_delivery() -> EventDeliveryRecord {
        EventDeliveryRecord {
            id: "evt-1:echo".into(),
            event_id: "evt-1".into(),
            plugin_id: "echo".into(),
            idempotency_key: "evt-1:echo".into(),
            state: "running".into(),
            attempt_count: 1,
            max_attempts: 8,
            lease_owner: Some("worker".into()),
            lease_expires_at: None,
            lease_generation: 1,
            run_after: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            invocation_sequence: 0,
            resume_pending: false,
            checkpoint_json: None,
            checkpoint_schema_version: 0,
            ordering_key: "u1".into(),
            outcome: None,
            error_message: None,
            created_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            updated_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            cancel_requested: false,
            resource_class: "network".into(),
            wake_event_type: String::new(),
            wake_filter_json: String::new(),
            wake_grants_json: String::new(),
        }
    }

    #[test]
    fn event_node_id_reuses_persisted_value() {
        let dir = tempfile::tempdir().unwrap();
        let first = resolve_event_node_id(dir.path());
        let second = resolve_event_node_id(dir.path());
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("event_node_id"))
                .unwrap()
                .trim(),
            first
        );
    }

    #[test]
    fn first_delivery_has_empty_checkpoint() {
        let domain = domain_event_for_delivery(&sample_event(), &sample_delivery());
        assert!(domain.checkpoint_json.is_empty());
        assert_eq!(domain.checkpoint_schema_version, 0);
        assert_eq!(domain.invocation_sequence, 0);
        assert!(!domain.resume_pending);
        assert_eq!(domain.payload, br#"{"titleId":"u1"}"#);
    }

    #[test]
    fn resume_delivery_copies_checkpoint_onto_domain_event() {
        let mut delivery = sample_delivery();
        delivery.checkpoint_json = Some(r#"{"offset":1}"#.into());
        delivery.checkpoint_schema_version = 2;
        delivery.invocation_sequence = 1;
        delivery.resume_pending = false;
        let domain = domain_event_for_delivery(&sample_event(), &delivery);
        assert_eq!(domain.checkpoint_json, r#"{"offset":1}"#);
        assert_eq!(domain.checkpoint_schema_version, 2);
        assert_eq!(domain.invocation_sequence, 1);
        assert!(domain.resume_pending);
    }

    #[test]
    fn suspend_requires_matching_schema_version() {
        let subs = vec![
            EventSubscription {
                event_type: "book_acquired".into(),
                schema_versions: vec![1],
                supports_suspend: false,
                resource_class: "network".into(),
                filter: None,
            },
            EventSubscription {
                event_type: "book_acquired".into(),
                schema_versions: vec![2],
                supports_suspend: true,
                resource_class: "network".into(),
                filter: None,
            },
        ];
        assert!(!suspend_allowed(&subs, "book_acquired", 1));
        assert!(suspend_allowed(&subs, "book_acquired", 2));
        assert!(!suspend_allowed(&subs, "other", 2));
    }

    #[test]
    fn wake_type_must_be_a_declared_subscription() {
        let subs = catalog_from_runtime(&[EventSubscription {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: true,
            resource_class: "network".into(),
            filter: None,
        }]);
        assert!(wake_grants_from_subscriptions(&subs, "", "")
            .unwrap()
            .is_empty());
        assert!(wake_grants_from_subscriptions(&subs, "book_acquired", "")
            .unwrap()
            .iter()
            .any(|g| g.schema_versions == vec![1]));
        assert_eq!(
            wake_grants_from_subscriptions(&subs, "listen_progress", ""),
            Err("wake event type not subscribed")
        );
    }

    #[test]
    fn wake_grants_keep_subscription_schema_and_filter() {
        let subs = catalog_from_runtime(&[EventSubscription {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: true,
            resource_class: "network".into(),
            filter: Some(serde_json::json!({"source": "audible"})),
        }]);
        let grants = wake_grants_from_subscriptions(&subs, "book_acquired", "").unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].schema_versions, vec![1]);
        assert_eq!(
            grants[0].filter,
            Some(serde_json::json!({"source": "audible"}))
        );
        assert_eq!(
            wake_grants_from_subscriptions(&subs, "book_acquired", r#"{"source":"libro"}"#),
            Err("wake filter not within subscription")
        );
        let narrowed =
            wake_grants_from_subscriptions(&subs, "book_acquired", r#"{"titleId":"u1"}"#).unwrap();
        assert_eq!(
            narrowed[0].filter,
            Some(serde_json::json!({"source":"audible","titleId":"u1"}))
        );
    }

    #[test]
    fn skip_reconcile_invalidates_after_error_and_backstop() {
        assert!(should_skip_catalog_reconcile("fp", "fp", true, 0));
        assert!(!should_skip_catalog_reconcile("fp", "fp", false, 0));
        assert!(!should_skip_catalog_reconcile("fp", "other", true, 0));
        assert!(should_skip_catalog_reconcile("fp", "fp", true, 11));
        assert!(!should_skip_catalog_reconcile("fp", "fp", true, 12));
    }

    fn publish_spec(dedup: &str) -> PublishDomainEventSpec {
        PublishDomainEventSpec {
            id: String::new(),
            event_type: "book_acquired".into(),
            schema_version: 1,
            account_id: "acct".into(),
            source: String::new(),
            correlation_id: String::new(),
            causation_id: String::new(),
            dedup_key: dedup.into(),
            payload: r#"{"titleId":"t","source":"audible"}"#.into(),
            ordering_key: String::new(),
        }
    }

    #[tokio::test]
    async fn dispatch_pending_caps_undispatched_and_still_wakes() {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let first = store
            .publish_domain_event(publish_spec("book_acquired:cap-sleeper"))
            .await
            .unwrap();
        let PublishDomainEventOutcome::Created { id: sleeper_event } = first else {
            panic!("{first:?}");
        };
        store
            .dispatch_event_deliveries(
                &sleeper_event,
                &[EventSubscriber::plugin("echo")],
                "cap-sleeper",
            )
            .await
            .unwrap();
        let claimed = store
            .claim_next_event_delivery(
                "cap-park",
                60,
                &uuid::Uuid::new_v4().to_string(),
                &["echo".into()],
                32,
                "",
            )
            .await
            .unwrap()
            .expect("claim sleeper");
        let future = Utc::now() + chrono::Duration::days(30);
        assert!(store
            .suspend_event_delivery(
                &claimed.fence(),
                r#"{"offset":1}"#,
                1,
                future,
                "book_acquired",
                r#"{"source":"audible"}"#,
                "",
            )
            .await
            .unwrap());
        let sleeper_id = claimed.id.clone();

        for i in 0..40 {
            store
                .publish_domain_event(publish_spec(&format!("book_acquired:cap-backlog-{i}")))
                .await
                .unwrap();
        }

        let (_, _, _, more) = dispatch_pending(&store, 7, "", false, 0, "test-node")
            .await
            .unwrap();
        assert!(more, "undispatched backlog should request another tick");
        assert!(store.next_undispatched_event().await.unwrap().is_some());
        let woken = store
            .get_event_delivery(&sleeper_id)
            .await
            .unwrap()
            .unwrap();
        assert!(woken.resume_pending);
        assert!(woken.run_after <= Utc::now() + chrono::Duration::seconds(2));
    }
}
