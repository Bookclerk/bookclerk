//! Durable domain-event dispatcher and fenced delivery worker.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use bookclerk_integrations::{
    DomainEvent, EventResult, EventSubscription, Integration, QueueBatch, QueueMessage,
    QueueResult, QueueSubscription,
};
use bookclerk_library::control_plane::{route_ranks, RouteRank};
use bookclerk_library::{
    catalog_subscribers_for_event, subscription_matches_event, wake_grants_from_subscriptions,
    EventCatalogSubscription, LibraryStore, PublishDomainEventSpec, QUEUE_CONTENT_TYPE_BYTES,
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
/// Force a bounded late-join reconcile at least every ~60s catalog TTL.
const RECONCILE_BACKSTOP_TICKS: u32 = 12;
/// Page size for `wake_pending` replay after pending dispatch.
const WAKE_PENDING_PAGE: u64 = 32;

/// Dispatch undispatched outbox rows and run configured delivery workers.
pub fn start_event_runtime(state: Arc<AppState>) {
    tokio::spawn(async move {
        upsert_event_subscriber_catalog(&state).await;
        let workers = state.config.read().await.events.concurrency.max(1);
        info!(event_workers = workers, "starting durable event dispatcher");
        spawn_dispatcher(state.clone());
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
            crate::node_queue::upsert_catalog_row(&library, &files_dir).await;
            let node_id = event_node_id(&files_dir);
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

/// Claim and run deliveries, reclaiming expired leases between idle waits.
fn spawn_delivery_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let owner = format!("event-{}", Uuid::new_v4());
        let mut idle = tokio::time::interval(Duration::from_secs(5));
        idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ranks = RankCache::default();
        loop {
            let library = state.library_snapshot().await;
            if let Err(err) = library.reclaim_expired_event_deliveries().await {
                warn!(error = %err, "event delivery reclaim failed");
            }
            let files_dir = state.config.read().await.paths().files_dir.clone();
            let max_in_flight = state.config.read().await.events.concurrency.max(1);
            let node_id = event_node_id(&files_dir);
            let mut claim_ids = loaded_plugin_ids(&state).await;
            claim_ids.extend(crate::node_queue::consumer_id(&files_dir));
            let groups = claim_groups(
                &claim_ids,
                ranks.get(&library, &files_dir, &claim_ids).await,
            );
            match claim_delivery(&library, &owner, &groups, max_in_flight, &node_id).await {
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
///
/// Catalog `plugin_id` is the plugin instance id when the loaded integration
/// has one. Otherwise it is the provenance-qualified PluginKey. Two instances
/// of one key each get their own row so event delivery does not collapse them.
pub async fn upsert_event_subscriber_catalog(state: &AppState) {
    let cfg = state.config.read().await.clone();
    let node_id = event_node_id(&cfg.paths().files_dir);
    let loaded = {
        let integrations = state.integrations.read().await;
        integrations
            .all()
            .iter()
            .map(|integration| LoadedSubscriber {
                key: durable_plugin_identity(integration.plugin_key(), integration.id()),
                instance_id: integration
                    .plugin_instance_id()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string),
                alias: integration.id().to_string(),
                runtime_subs: catalog_from_integration(integration.as_ref()),
            })
            .collect::<Vec<_>>()
    };
    let instance_keys: std::collections::HashSet<String> = loaded
        .iter()
        .filter(|row| row.instance_id.is_some())
        .map(|row| row.key.clone())
        .collect();
    let mut granted_by_key =
        std::collections::HashMap::<String, Vec<EventCatalogSubscription>>::new();
    let discovered = tokio::task::spawn_blocking({
        let cfg = cfg.clone();
        move || bookclerk_plugin_host::discover_plugins(&cfg)
    })
    .await;
    let library = state.library_snapshot().await;
    let mut catalogued = std::collections::HashSet::<String>::new();
    match discovered {
        Ok(Ok(plugins)) => {
            for plugin in plugins {
                if !plugin.manifest.consumes_events() && plugin.manifest.queues.consumers.is_empty()
                {
                    continue;
                }
                // Not loaded on this node: no present deployment runs it here,
                // so its catalog row is recorded disabled until one does.
                let enabled = false;
                let plugin_id = plugin.plugin_key().canonical().to_string();
                let grant = bookclerk_plugin_host::PluginGrantStore::load(&cfg.paths().files_dir)
                    .ok()
                    .and_then(|store| {
                        store
                            .get_by_plugin_key(plugin.plugin_key().canonical())
                            .cloned()
                    });
                let subs = granted_catalog_subscriptions(&plugin, grant.as_ref());
                if instance_keys.contains(&plugin_id) {
                    granted_by_key.insert(plugin_id, subs);
                    continue;
                }
                // Always upsert — including an empty list — so a narrowed or
                // revoked grant clears the prior catalog row immediately instead
                // of leaving deliveries live until the heartbeat TTL expires.
                if let Err(err) = library
                    .upsert_event_subscriber(&node_id, &plugin_id, &subs, enabled)
                    .await
                {
                    warn!(
                        plugin_key = %plugin_id,
                        alias = %plugin.manifest.id,
                        error = %err,
                        "event subscriber catalog upsert failed"
                    );
                }
                catalogued.insert(plugin_id);
            }
        }
        Ok(Err(err)) => {
            warn!(error = %err, "plugin discovery for event catalog failed");
        }
        Err(err) => {
            warn!(error = %err, "plugin discovery task for event catalog failed");
        }
    }
    for row in &loaded {
        if let Some(instance_id) = &row.instance_id {
            let subs = granted_by_key
                .get(&row.key)
                .cloned()
                .unwrap_or_else(|| row.runtime_subs.clone());
            if let Err(err) = library
                .upsert_event_subscriber(&node_id, instance_id, &subs, true)
                .await
            {
                warn!(
                    plugin_instance_id = %instance_id,
                    plugin_key = %row.key,
                    alias = %row.alias,
                    error = %err,
                    "instance event subscriber catalog upsert failed"
                );
            }
            continue;
        }
        // Discovered guests are also loaded into `state.integrations`. Their
        // grant-filtered catalog row was written above; do not overwrite it
        // with the full runtime/manifest subscription list.
        if catalogued.contains(&row.key) {
            continue;
        }
        if let Err(err) = library
            .upsert_event_subscriber(&node_id, &row.key, &row.runtime_subs, true)
            .await
        {
            warn!(
                plugin_key = %row.key,
                alias = %row.alias,
                error = %err,
                "loaded integration catalog upsert failed"
            );
        }
    }
}

/// One loaded integration captured before discovery so catalog rows can use
/// its instance id.
struct LoadedSubscriber {
    /// PluginKey, or the display alias when the key is empty.
    key: String,
    /// Present when this process spawned that deployment.
    instance_id: Option<String>,
    /// Display alias for logs.
    alias: String,
    /// Runtime subscriptions used when discovery did not grant a catalog.
    runtime_subs: Vec<EventCatalogSubscription>,
}

/// This node's id, the key of its rows in the subscriber catalog. Falls back
/// to a process-lifetime id when the node identity cannot be read, so a
/// catalog row never matches another node's.
fn event_node_id(files_dir: &Path) -> String {
    static FALLBACK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    crate::node_queue::node_id(files_dir)
        .unwrap_or_else(|| FALLBACK.get_or_init(|| Uuid::new_v4().to_string()).clone())
}

/// Map `[[events.consumers]]` rows onto the durable catalog JSON shape.
fn catalog_from_manifest(
    plugin: &bookclerk_plugin_host::DiscoveredPlugin,
) -> Vec<EventCatalogSubscription> {
    plugin
        .manifest
        .events
        .consumers
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
            queue: String::new(),
            max_retries: s.max_retries,
            dead_letter_queue: None,
            retry_delay_secs: 0,
        })
        .collect()
}

/// Map `[[queues.consumers]]` rows onto the durable catalog JSON shape.
fn queue_catalog_from_manifest(
    plugin: &bookclerk_plugin_host::DiscoveredPlugin,
) -> Vec<EventCatalogSubscription> {
    plugin
        .manifest
        .queues
        .consumers
        .iter()
        .map(|c| {
            queue_catalog_row(
                &c.queue,
                c.max_retries,
                c.dead_letter_queue.clone(),
                c.retry_delay,
            )
        })
        .collect()
}

/// One catalog row for a plugin-owned queue consumer.
fn queue_catalog_row(
    queue: &str,
    max_retries: u32,
    dead_letter_queue: Option<String>,
    retry_delay_secs: u32,
) -> EventCatalogSubscription {
    EventCatalogSubscription {
        max_retries: Some(max_retries),
        dead_letter_queue,
        retry_delay_secs,
        ..EventCatalogSubscription::queue(queue)
    }
}

/// Catalog rows the operator grant actually authorizes (manifest ∩ grant):
/// event consumers matched on type, schemas, and filter; queue consumers
/// matched on the `queue_consumer:<queue>` grant token.
fn granted_catalog_subscriptions(
    plugin: &bookclerk_plugin_host::DiscoveredPlugin,
    stored: Option<&bookclerk_plugin_host::PluginGrant>,
) -> Vec<EventCatalogSubscription> {
    let requested = bookclerk_plugin_host::consent_request(&plugin.manifest, plugin.plugin_key());
    let effective = stored.map(|g| bookclerk_plugin_host::effective_grant(g, &requested));
    let admitted = effective
        .as_ref()
        .map(|g| g.consumers.clone())
        .unwrap_or_default();
    let mut rows = filter_catalog_by_granted_consumers(&catalog_from_manifest(plugin), &admitted);
    rows.extend(
        queue_catalog_from_manifest(plugin)
            .into_iter()
            .filter(|row| {
                effective
                    .as_ref()
                    .is_some_and(|g| g.allows_queue_consumer(row.queue_name()))
            }),
    );
    rows
}

/// Catalog rows a loaded integration advertises at runtime: its event
/// subscriptions plus one row per `[[queues.consumers]]` queue.
fn catalog_from_integration(integration: &dyn Integration) -> Vec<EventCatalogSubscription> {
    let mut rows = catalog_from_runtime(&integration.event_subscriptions());
    rows.extend(catalog_from_queue_subscriptions(
        &integration.queue_subscriptions(),
    ));
    rows
}

/// Map runtime `[[queues.consumers]]` rows onto the catalog shape.
fn catalog_from_queue_subscriptions(subs: &[QueueSubscription]) -> Vec<EventCatalogSubscription> {
    subs.iter()
        .map(|s| {
            queue_catalog_row(
                &s.queue,
                s.max_retries,
                s.dead_letter_queue.clone(),
                s.retry_delay_secs,
            )
        })
        .collect()
}

/// Keep catalog rows that match a granted consumer on type, schemas, and
/// filter, applying the grant's `supports_suspend` narrowing onto the row.
fn filter_catalog_by_granted_consumers(
    catalog: &[EventCatalogSubscription],
    admitted: &std::collections::BTreeSet<bookclerk_plugin_host::GrantedEventConsumer>,
) -> Vec<EventCatalogSubscription> {
    catalog
        .iter()
        .filter_map(|s| {
            let catalog_filter = bookclerk_plugin_host::canonical_event_filter(&s.filter);
            admitted.iter().find_map(|c| {
                if c.event_type != s.event_type {
                    return None;
                }
                if !s
                    .schema_versions
                    .iter()
                    .all(|v| c.schema_versions.contains(v))
                {
                    return None;
                }
                if c.filter != catalog_filter {
                    return None;
                }
                let mut row = s.clone();
                row.supports_suspend = s.supports_suspend && c.supports_suspend;
                Some(row)
            })
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
            queue: String::new(),
            max_retries: None,
            dead_letter_queue: None,
            retry_delay_secs: 0,
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
        let op = format!("dispatch-{}", event.id);
        let n = library
            .dispatch_event_deliveries(&event.id, &subs, &op)
            .await?;
        info!(
            event_id = %event.id,
            event_type = %event.event_type,
            deliveries = n,
            "dispatched domain event"
        );
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

/// Durable catalog / delivery identity: PluginKey when present, else alias.
///
/// Empty keys (in-crate test doubles) fall back to the display alias. Product
/// guests always have a provenance-qualified key.
fn durable_plugin_identity(plugin_key: &str, alias: &str) -> String {
    let key = plugin_key.trim();
    if key.is_empty() {
        alias.to_string()
    } else {
        key.to_string()
    }
}

/// Plugin ids currently loaded on this process (PluginKey, plus unique aliases).
///
/// Alias ids are included only when a single loaded guest uses that display
/// name so alias-era deliveries can drain after the catalog switches to
/// PluginKey. Colliding aliases never share a claim id.
async fn loaded_plugin_ids(state: &AppState) -> Vec<String> {
    let integrations = state.integrations.read().await;
    let rows: Vec<LoadedClaimId> = integrations
        .all()
        .iter()
        .map(|integration| LoadedClaimId {
            instance_id: integration
                .plugin_instance_id()
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            plugin_key: integration.plugin_key().to_string(),
            alias: integration.id().to_string(),
        })
        .collect();
    claim_ids_for_loaded(&rows)
}

/// One loaded integration's claim identity.
struct LoadedClaimId {
    /// Plugin instance id, when the catalog has switched off the plugin key.
    instance_id: Option<String>,
    /// Provenance-qualified plugin key. Empty for in-crate doubles.
    plugin_key: String,
    /// Display alias.
    alias: String,
}

/// Claim ids for loaded integrations.
///
/// Each guest contributes its instance id, or the plugin key when it has no
/// instance id. A display alias is included only when one guest uses it.
/// The plugin key is also included when exactly one loaded guest has that
/// key, so deliveries queued before the catalog switched to instance ids
/// still drain. Two instances of one key do not share that claim id.
fn claim_ids_for_loaded(rows: &[LoadedClaimId]) -> Vec<String> {
    let mut ids = Vec::new();
    for row in rows {
        let key = match row.instance_id.as_deref() {
            Some(instance_id) => instance_id.to_string(),
            None => durable_plugin_identity(&row.plugin_key, &row.alias),
        };
        ids.push(key.clone());
        if row.alias != key
            && rows
                .iter()
                .filter(|other| other.alias.eq_ignore_ascii_case(&row.alias))
                .count()
                == 1
        {
            ids.push(row.alias.clone());
        }
    }
    let mut key_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for row in rows {
        let plugin_key = row.plugin_key.trim();
        if plugin_key.is_empty() {
            continue;
        }
        *key_counts.entry(plugin_key.to_string()).or_default() += 1;
    }
    for (plugin_key, count) in key_counts {
        if count == 1 && !ids.iter().any(|id| id == &plugin_key) {
            ids.push(plugin_key);
        }
    }
    ids
}

/// How long a cached route ranking stays valid (heartbeats come every 15 s).
const RANK_CACHE_TTL: Duration = Duration::from_secs(10);

/// This node's route ranks for its loaded instances, refreshed every
/// [`RANK_CACHE_TTL`].
#[derive(Default)]
struct RankCache {
    /// When the ranks were read, and for which claim ids.
    read: Option<(Instant, Vec<String>)>,
    /// Instance id → this node's rank among live nodes running it.
    ranks: BTreeMap<String, RouteRank>,
}

impl RankCache {
    /// Current ranks; on a read failure every id is treated as rank 0, so
    /// work still flows (ranking only orders hosts, it never blocks one).
    async fn get(
        &mut self,
        library: &LibraryStore,
        files_dir: &std::path::Path,
        claim_ids: &[String],
    ) -> &BTreeMap<String, RouteRank> {
        let fresh = self
            .read
            .as_ref()
            .is_some_and(|(at, ids)| at.elapsed() < RANK_CACHE_TTL && ids == claim_ids);
        if !fresh {
            let node_id = crate::node_queue::node_id(files_dir).unwrap_or_default();
            self.ranks = match route_ranks(library, &node_id, claim_ids).await {
                Ok(ranks) => ranks,
                Err(err) => {
                    warn!(error = %err, "delivery route ranks unavailable; claiming unranked");
                    BTreeMap::new()
                }
            };
            self.read = Some((Instant::now(), claim_ids.to_vec()));
        }
        &self.ranks
    }
}

/// Claim ids grouped by how long this node lets their deliveries wait:
/// ids this node is best placed for (and ids that are not routed, such as
/// plugin keys and the host's own queue) first with no wait, then each
/// lower rank with [`bookclerk_library::control_plane::ROUTE_DEFER_PER_RANK`]
/// more.
fn claim_groups(
    claim_ids: &[String],
    ranks: &BTreeMap<String, RouteRank>,
) -> Vec<(Duration, Vec<String>)> {
    let mut by_defer: BTreeMap<Duration, Vec<String>> = BTreeMap::new();
    for id in claim_ids {
        let defer = ranks.get(id).map_or(Duration::ZERO, |rank| rank.defer());
        by_defer.entry(defer).or_default().push(id.clone());
    }
    by_defer.into_iter().collect()
}

/// Claim the next ready delivery, best-ranked ids first, replaying a lost
/// RPC within [`CLAIM_REPLAY_BUDGET`].
async fn claim_delivery(
    library: &LibraryStore,
    owner: &str,
    groups: &[(Duration, Vec<String>)],
    max_in_flight: u32,
    node_id: &str,
) -> Result<Option<bookclerk_library::EventDeliveryRecord>, bookclerk_library::LibraryError> {
    for (defer, plugin_ids) in groups {
        if plugin_ids.is_empty() {
            continue;
        }
        let due_by = chrono::Utc::now()
            - chrono::Duration::from_std(*defer).unwrap_or(chrono::Duration::zero());
        if let Some(row) =
            claim_group(library, owner, plugin_ids, max_in_flight, node_id, due_by).await?
        {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

/// One claim over `plugin_ids` for deliveries due by `due_by`.
async fn claim_group(
    library: &LibraryStore,
    owner: &str,
    plugin_ids: &[String],
    max_in_flight: u32,
    node_id: &str,
    due_by: chrono::DateTime<chrono::Utc>,
) -> Result<Option<bookclerk_library::EventDeliveryRecord>, bookclerk_library::LibraryError> {
    let operation_id = Uuid::new_v4().to_string();
    let deadline = Instant::now() + CLAIM_REPLAY_BUDGET;
    loop {
        match library
            .claim_next_event_delivery_due_by(
                owner,
                LEASE_SECS,
                &operation_id,
                plugin_ids,
                max_in_flight.max(1),
                node_id,
                due_by,
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
    if bookclerk_library::control_plane::is_node_consumer_id(&delivery.plugin_id) {
        crate::node_queue::run_node_delivery(library, &event, &delivery).await;
        return;
    }
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
    let local_subs = catalog_from_integration(integration.as_ref());
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
    if event.is_queue_message() {
        run_queue_delivery(
            library,
            integration.as_ref(),
            &event,
            &delivery,
            &local_subs,
        )
        .await;
        return;
    }
    let domain = domain_event_for_delivery(&event, &delivery);
    let cancel = Arc::new(AtomicBool::new(false));
    let operator_cancel = Arc::new(AtomicBool::new(false));
    let heartbeat = spawn_delivery_heartbeat(
        library.clone(),
        fence.clone(),
        Arc::clone(&cancel),
        Arc::clone(&operator_cancel),
    );
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

/// Heartbeats a claimed delivery until the handler returns: renews the lease,
/// honours an operator cancel request, and trips `cancel` when the fence is
/// lost. The caller aborts the task once the handler has returned.
fn spawn_delivery_heartbeat(
    library: LibraryStore,
    fence: bookclerk_library::EventDeliveryFence,
    cancel: Arc<AtomicBool>,
    operator_cancel: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
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
}

/// Runs one claimed delivery of a plugin-owned queue message: builds the
/// single-message [`QueueBatch`], calls the integration's `queue(batch)`
/// trigger, and persists the per-message outcome. A retry past the
/// consumer's `max_retries` dead-letters the delivery and, when the
/// subscription names a `dead_letter_queue`, forwards a copy of the message
/// there first.
async fn run_queue_delivery(
    library: &LibraryStore,
    integration: &dyn Integration,
    event: &bookclerk_library::DomainEventRecord,
    delivery: &bookclerk_library::EventDeliveryRecord,
    local_subs: &[EventCatalogSubscription],
) {
    let fence = delivery.fence();
    let subscription = local_subs
        .iter()
        .find(|s| s.is_queue() && s.queue_name() == event.queue_name)
        .cloned();
    let batch = QueueBatch {
        queue: event.queue_name.clone(),
        messages: vec![queue_message_for_delivery(event, delivery)],
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let operator_cancel = Arc::new(AtomicBool::new(false));
    let heartbeat = spawn_delivery_heartbeat(
        library.clone(),
        fence.clone(),
        Arc::clone(&cancel),
        Arc::clone(&operator_cancel),
    );
    let started = Instant::now();
    let outcome = integration
        .deliver_queue_batch_cancelable(batch, Arc::clone(&cancel))
        .await;
    heartbeat.abort();
    let _ = library
        .record_event_handler_latency(i64::try_from(started.elapsed().as_millis()).unwrap_or(0))
        .await;
    if operator_cancel.load(Ordering::SeqCst) {
        warn!(delivery_id = %delivery.id, "queue delivery cancelled by operator");
        let _ = library
            .reject_event_delivery(&fence, "cancelled by operator")
            .await;
        return;
    }
    if cancel.load(Ordering::SeqCst) {
        warn!(
            delivery_id = %delivery.id,
            "queue delivery fence lost; ignoring handler result"
        );
        return;
    }
    let result = match outcome {
        Ok(mut results) => match results.pop() {
            Some(result) if results.is_empty() => result,
            _ => {
                warn!(
                    delivery_id = %delivery.id,
                    plugin = %delivery.plugin_id,
                    "queue handler returned a result list of the wrong length; retrying"
                );
                QueueResult::Retry(bookclerk_integrations::QueueRetry {
                    delay_seconds: 0,
                    reason: "queue handler returned the wrong number of results".into(),
                })
            }
        },
        Err(err) => {
            warn!(
                delivery_id = %delivery.id,
                plugin = %delivery.plugin_id,
                queue = %event.queue_name,
                error = %err,
                "queue delivery invocation failed"
            );
            // A failure on the last attempt dead-letters the delivery, so the
            // message goes to the consumer's dead-letter queue first, as an
            // exhausted retry does.
            if delivery.attempt_count >= delivery.max_attempts {
                if let Some(dlq) = subscription
                    .as_ref()
                    .and_then(|s| s.dead_letter_queue.clone())
                {
                    forward_to_dead_letter_queue(library, event, &dlq).await;
                }
            }
            let _ = library.fail_event_delivery(&fence, &err.to_string()).await;
            return;
        }
    };
    let persist = match result {
        QueueResult::Ack => {
            info!(
                delivery_id = %delivery.id,
                plugin = %delivery.plugin_id,
                queue = %event.queue_name,
                outcome = "ack",
                "queue delivery completed"
            );
            library.ack_event_delivery(&fence).await
        }
        QueueResult::Retry(retry) => {
            let exhausted = delivery.attempt_count >= delivery.max_attempts;
            if exhausted {
                warn!(
                    delivery_id = %delivery.id,
                    plugin = %delivery.plugin_id,
                    queue = %event.queue_name,
                    outcome = "dead_letter",
                    reason = %retry.reason,
                    attempts = delivery.attempt_count,
                    "queue delivery retry exhausted"
                );
                if let Some(dlq) = subscription
                    .as_ref()
                    .and_then(|s| s.dead_letter_queue.clone())
                {
                    forward_to_dead_letter_queue(library, event, &dlq).await;
                }
            } else {
                info!(
                    delivery_id = %delivery.id,
                    plugin = %delivery.plugin_id,
                    queue = %event.queue_name,
                    outcome = "retry",
                    reason = %retry.reason,
                    "queue delivery retry scheduled"
                );
            }
            let delay = if retry.delay_seconds > 0 {
                i64::from(retry.delay_seconds)
            } else {
                subscription
                    .as_ref()
                    .map(|s| i64::from(s.retry_delay_secs))
                    .filter(|d| *d > 0)
                    .unwrap_or(5)
            };
            let wake = Utc::now() + chrono::Duration::seconds(delay);
            library
                .retry_event_delivery(&fence, wake, &retry.reason)
                .await
        }
    };
    if let Err(err) = persist {
        warn!(delivery_id = %delivery.id, error = %err, "queue delivery persist failed");
    }
}

/// Copies an exhausted queue message onto its consumer's dead-letter queue.
async fn forward_to_dead_letter_queue(
    library: &LibraryStore,
    event: &bookclerk_library::DomainEventRecord,
    dead_letter_queue: &str,
) {
    let spec = PublishDomainEventSpec {
        id: String::new(),
        event_type: String::new(),
        schema_version: event.schema_version,
        account_id: event.account_id.clone(),
        source: event.source.clone(),
        correlation_id: event.correlation_id.clone(),
        causation_id: event.id.clone(),
        dedup_key: format!("dlq:{}", event.id),
        payload: event.payload.clone(),
        ordering_key: event.ordering_key.clone(),
        queue_name: dead_letter_queue.to_string(),
        content_type: event.content_type.clone(),
        delay_seconds: 0,
    };
    match library.publish_domain_event(spec).await {
        Ok(_) => info!(
            message_id = %event.id,
            queue = %event.queue_name,
            dead_letter_queue,
            "queue message forwarded to dead-letter queue"
        ),
        Err(err) => warn!(
            message_id = %event.id,
            queue = %event.queue_name,
            dead_letter_queue,
            error = %err,
            "dead-letter queue forward failed"
        ),
    }
}

/// Map an outbox row plus the claimed delivery onto an ABI [`QueueMessage`].
///
/// `bytes` bodies are stored base64-encoded and decoded here; `json` and
/// `text` bodies travel as their UTF-8 bytes.
fn queue_message_for_delivery(
    event: &bookclerk_library::DomainEventRecord,
    delivery: &bookclerk_library::EventDeliveryRecord,
) -> QueueMessage {
    use base64::Engine as _;
    let body = if event.content_type == QUEUE_CONTENT_TYPE_BYTES {
        base64::engine::general_purpose::STANDARD
            .decode(event.payload.as_bytes())
            .unwrap_or_default()
    } else {
        event.payload.clone().into_bytes()
    };
    QueueMessage {
        id: event.id.clone(),
        timestamp_unix_ms: u64::try_from(event.created_at.timestamp_millis()).unwrap_or(0),
        body,
        content_type: event.content_type.clone(),
        attempts: u32::try_from(delivery.attempt_count.max(1)).unwrap_or(1),
        source: event.source.clone(),
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
        DomainEventRecord, EventCatalogSubscription, EventDeliveryRecord, EventSubscriber,
        InProcessSqliteAtomic, PublishDomainEventOutcome, PublishDomainEventSpec,
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
            queue_name: bookclerk_library::CORE_EVENTS_QUEUE.into(),
            content_type: bookclerk_library::QUEUE_CONTENT_TYPE_JSON.into(),
            deliver_after: None,
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
            queue_name: bookclerk_library::CORE_EVENTS_QUEUE.into(),
        }
    }

    #[test]
    fn catalog_rows_and_the_node_queue_share_the_node_id() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = event_node_id(dir.path());
        assert_eq!(
            Some(catalog.clone()),
            crate::node_queue::node_id(dir.path())
        );
        assert_eq!(catalog, event_node_id(dir.path()));
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
    fn catalog_uses_granted_consumers_not_full_manifest() {
        let catalog = vec![
            EventCatalogSubscription {
                event_type: "book_acquired".into(),
                schema_versions: vec![1],
                supports_suspend: true,
                resource_class: "network".into(),
                filter: None,
                queue: String::new(),
                max_retries: None,
                dead_letter_queue: None,
                retry_delay_secs: 0,
            },
            EventCatalogSubscription {
                event_type: "book_deleted".into(),
                schema_versions: vec![1],
                supports_suspend: false,
                resource_class: "network".into(),
                filter: None,
                queue: String::new(),
                max_retries: None,
                dead_letter_queue: None,
                retry_delay_secs: 0,
            },
        ];
        let mut admitted = std::collections::BTreeSet::new();
        admitted.insert(bookclerk_plugin_host::GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: true,
            filter: None,
        });
        let kept = filter_catalog_by_granted_consumers(&catalog, &admitted);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].event_type, "book_acquired");
        let empty =
            filter_catalog_by_granted_consumers(&catalog, &std::collections::BTreeSet::new());
        assert!(empty.is_empty());
    }

    #[test]
    fn catalog_applies_grant_supports_suspend_narrowing() {
        let catalog = vec![EventCatalogSubscription {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: true,
            resource_class: "network".into(),
            filter: None,
            queue: String::new(),
            max_retries: None,
            dead_letter_queue: None,
            retry_delay_secs: 0,
        }];
        let mut admitted = std::collections::BTreeSet::new();
        admitted.insert(bookclerk_plugin_host::GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: None,
        });
        let kept = filter_catalog_by_granted_consumers(&catalog, &admitted);
        assert_eq!(kept.len(), 1);
        assert!(!kept[0].supports_suspend);
    }

    #[test]
    fn catalog_requires_matching_consumer_filter() {
        let catalog = vec![
            EventCatalogSubscription {
                event_type: "book_acquired".into(),
                schema_versions: vec![1],
                supports_suspend: false,
                resource_class: "network".into(),
                filter: None,
                queue: String::new(),
                max_retries: None,
                dead_letter_queue: None,
                retry_delay_secs: 0,
            },
            EventCatalogSubscription {
                event_type: "book_acquired".into(),
                schema_versions: vec![1],
                supports_suspend: false,
                resource_class: "network".into(),
                filter: Some(serde_json::json!({"titleId":"u1"})),
                queue: String::new(),
                max_retries: None,
                dead_letter_queue: None,
                retry_delay_secs: 0,
            },
        ];
        let mut admitted = std::collections::BTreeSet::new();
        admitted.insert(bookclerk_plugin_host::GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: Some(r#"{"titleId":"u1"}"#.into()),
        });
        let kept = filter_catalog_by_granted_consumers(&catalog, &admitted);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].filter.as_ref(),
            Some(&serde_json::json!({"titleId":"u1"}))
        );
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
            ..PublishDomainEventSpec::default()
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

    #[tokio::test]
    async fn dispatch_pending_fault_after_first_page_keeps_parent_pending() {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        let store = LibraryStore::from_connection(db.clone());
        store
            .upsert_event_subscriber(
                "node-a",
                "echo",
                &[EventCatalogSubscription::new("book_acquired", vec![1])],
                true,
            )
            .await
            .unwrap();
        store
            .upsert_event_subscriber(
                "node-a",
                "audiobookshelf",
                &[EventCatalogSubscription::new("book_acquired", vec![1])],
                true,
            )
            .await
            .unwrap();
        let created = store
            .publish_domain_event(publish_spec("book_acquired:pending-fault"))
            .await
            .unwrap();
        let PublishDomainEventOutcome::Created { id } = created else {
            panic!("{created:?}");
        };
        let store = store.with_atomic_txn(std::sync::Arc::new(InProcessSqliteAtomic { db }));
        bookclerk_library::set_dispatch_chunk_for_test(Some(1));
        bookclerk_library::inject_dispatch_page_failures(1);
        let err = dispatch_pending(&store, 7, "", false, 0, "test-node")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("injected dispatch page failure"),
            "{err}"
        );
        let event = store.get_domain_event(&id).await.unwrap().unwrap();
        assert_eq!(event.dispatch_state, "pending");
        let echo = store
            .get_event_delivery(&format!("{id}:echo"))
            .await
            .unwrap();
        let abs = store
            .get_event_delivery(&format!("{id}:audiobookshelf"))
            .await
            .unwrap();
        assert_eq!(
            echo.is_some() as u8 + abs.is_some() as u8,
            1,
            "exactly one of two subscribers must be inserted before the fault"
        );
        bookclerk_library::inject_dispatch_page_failures(0);
        dispatch_pending(&store, 7, "", false, 0, "test-node")
            .await
            .unwrap();
        bookclerk_library::set_dispatch_chunk_for_test(None);
        let event = store.get_domain_event(&id).await.unwrap().unwrap();
        assert_eq!(event.dispatch_state, "dispatched");
        assert!(store
            .get_event_delivery(&format!("{id}:echo"))
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_delivery(&format!("{id}:audiobookshelf"))
            .await
            .unwrap()
            .is_some());
    }

    fn discovered_echo(root: &std::path::Path) -> bookclerk_plugin_host::DiscoveredPlugin {
        std::fs::create_dir_all(root).unwrap();
        let toml = r#"
api_version = 3
id = "echo"
version = "0.1.0"
runtime = "native"
command = "./guest"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[[events.consumers]]
type = "book_acquired"
schema_versions = [1]
"#;
        std::fs::write(root.join("plugin.toml"), toml).unwrap();
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").unwrap();
        let manifest = bookclerk_plugin_host::PluginManifest::parse(toml).unwrap();
        bookclerk_plugin_host::DiscoveredPlugin::new(manifest, root.to_path_buf(), command)
    }

    fn discovered_queue_echo(root: &std::path::Path) -> bookclerk_plugin_host::DiscoveredPlugin {
        std::fs::create_dir_all(root).unwrap();
        let toml = r#"
api_version = 3
id = "qecho"
version = "0.1.0"
runtime = "native"
command = "./guest"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[[queues.consumers]]
queue = "echo"
max_retries = 2
dead_letter_queue = "echo-dlq"
retry_delay = 7

[[queues.consumers]]
queue = "other"
"#;
        std::fs::write(root.join("plugin.toml"), toml).unwrap();
        let command = root.join("guest");
        std::fs::write(&command, b"#!/bin/sh\n").unwrap();
        let manifest = bookclerk_plugin_host::PluginManifest::parse(toml).unwrap();
        bookclerk_plugin_host::DiscoveredPlugin::new(manifest, root.to_path_buf(), command)
    }

    #[test]
    fn queue_catalog_rows_follow_the_consumer_grant() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = discovered_queue_echo(dir.path());
        assert!(
            granted_catalog_subscriptions(&plugin, None).is_empty(),
            "no stored grant means no queue rows"
        );
        let requested =
            bookclerk_plugin_host::consent_request(&plugin.manifest, plugin.plugin_key());
        assert!(requested.allows_queue_consumer("echo"));
        let mut narrowed = requested.clone();
        narrowed
            .bindings
            .retain(|b| b != &bookclerk_plugin_host::queue_consumer_grant("other"));
        let rows = granted_catalog_subscriptions(&plugin, Some(&narrowed));
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert!(row.is_queue());
        assert_eq!(row.queue_name(), "echo");
        assert_eq!(row.max_retries, Some(2));
        assert_eq!(row.max_attempts(), Some(3));
        assert_eq!(row.dead_letter_queue.as_deref(), Some("echo-dlq"));
        assert_eq!(row.retry_delay_secs, 7);
        let full = granted_catalog_subscriptions(&plugin, Some(&requested));
        assert_eq!(
            full.iter().map(|r| r.queue_name()).collect::<Vec<_>>(),
            ["echo", "other"]
        );
    }

    #[test]
    fn queue_message_decodes_bytes_bodies_and_copies_attempts() {
        let mut event = sample_event();
        event.queue_name = "echo".into();
        event.content_type = bookclerk_library::QUEUE_CONTENT_TYPE_BYTES.into();
        event.payload = "/wA=".into();
        event.source = "producer".into();
        let mut delivery = sample_delivery();
        delivery.attempt_count = 3;
        let message = queue_message_for_delivery(&event, &delivery);
        assert_eq!(message.id, "evt-1");
        assert_eq!(message.body, vec![0xff, 0x00]);
        assert_eq!(message.content_type, "bytes");
        assert_eq!(message.attempts, 3);
        assert_eq!(message.source, "producer");
        assert_eq!(message.timestamp_unix_ms, 1_700_000_000_000);

        event.content_type = bookclerk_library::QUEUE_CONTENT_TYPE_JSON.into();
        event.payload = r#"{"n":1}"#.into();
        delivery.attempt_count = 0;
        let message = queue_message_for_delivery(&event, &delivery);
        assert_eq!(message.body, br#"{"n":1}"#);
        assert_eq!(message.attempts, 1, "attempts start at 1");
    }

    #[test]
    fn runtime_queue_subscriptions_map_onto_catalog_rows() {
        let rows = catalog_from_queue_subscriptions(&[QueueSubscription {
            queue: "echo".into(),
            max_retries: 4,
            dead_letter_queue: None,
            retry_delay_secs: 0,
        }]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_queue());
        assert_eq!(rows[0].queue_name(), "echo");
        assert_eq!(rows[0].max_retries, Some(4));
        let event = {
            let mut e = sample_event();
            e.queue_name = "echo".into();
            e
        };
        assert!(subscription_matches_event(&rows[0], &event));
        assert!(!subscription_matches_event(&rows[0], &sample_event()));
    }

    #[test]
    fn durable_plugin_identity_prefers_plugin_key() {
        assert_eq!(
            durable_plugin_identity("path:file:///tmp/a#echo", "echo"),
            "path:file:///tmp/a#echo"
        );
        assert_eq!(durable_plugin_identity("", "echo"), "echo");
        assert_eq!(durable_plugin_identity("   ", "echo"), "echo");
    }

    #[test]
    /// Invalid installation state (defense in depth): catalog identity stays on
    /// PluginKey even when two installs share a display alias.
    fn event_catalog_keys_same_alias_installs_on_distinct_plugin_keys() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let left = discovered_echo(a.path());
        let right = discovered_echo(b.path());
        assert_eq!(left.manifest.id, "echo");
        assert_eq!(right.manifest.id, "echo");
        assert_ne!(
            left.plugin_key().canonical(),
            right.plugin_key().canonical()
        );
        assert_eq!(
            durable_plugin_identity(left.plugin_key().canonical(), left.alias()),
            left.plugin_key().canonical()
        );
        assert_eq!(
            durable_plugin_identity(right.plugin_key().canonical(), right.alias()),
            right.plugin_key().canonical()
        );
    }

    #[tokio::test]
    /// Invalid installation state (defense in depth): dispatch must not collapse
    /// two PluginKeys that share a display alias onto one catalog row.
    async fn dispatch_does_not_collapse_same_alias_plugin_keys() {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        let store = LibraryStore::from_connection(db.clone());
        let left = "path:file:///tmp/a#echo";
        let right = "path:file:///tmp/b#echo";
        store
            .upsert_event_subscriber(
                "node-a",
                left,
                &[EventCatalogSubscription::new("book_acquired", vec![1])],
                true,
            )
            .await
            .unwrap();
        store
            .upsert_event_subscriber(
                "node-a",
                right,
                &[EventCatalogSubscription::new("book_acquired", vec![1])],
                true,
            )
            .await
            .unwrap();
        let created = store
            .publish_domain_event(publish_spec("book_acquired:alias-collision"))
            .await
            .unwrap();
        let PublishDomainEventOutcome::Created { id } = created else {
            panic!("{created:?}");
        };
        dispatch_pending(&store, 7, "", false, 0, "test-node")
            .await
            .unwrap();
        assert!(store
            .get_event_delivery(&format!("{id}:{left}"))
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_delivery(&format!("{id}:{right}"))
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_delivery(&format!("{id}:echo"))
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn claim_groups_put_best_ranked_and_unrouted_ids_first() {
        let ranks = BTreeMap::from([
            (
                "instance-best".to_string(),
                RouteRank {
                    rank: 0,
                    nodes: vec!["here".into(), "there".into()],
                },
            ),
            (
                "instance-second".to_string(),
                RouteRank {
                    rank: 1,
                    nodes: vec!["there".into(), "here".into()],
                },
            ),
            (
                "instance-third".to_string(),
                RouteRank {
                    rank: 2,
                    nodes: vec!["a".into(), "b".into(), "here".into()],
                },
            ),
        ]);
        let ids: Vec<String> = [
            "instance-third",
            "path:echo",
            "instance-second",
            "instance-best",
            "node:here",
        ]
        .map(String::from)
        .to_vec();
        let step = bookclerk_library::control_plane::ROUTE_DEFER_PER_RANK;
        assert_eq!(
            super::claim_groups(&ids, &ranks),
            vec![
                (
                    Duration::ZERO,
                    vec![
                        "path:echo".to_string(),
                        "instance-best".to_string(),
                        "node:here".to_string()
                    ]
                ),
                (step, vec!["instance-second".to_string()]),
                (step * 2, vec!["instance-third".to_string()]),
            ]
        );
        assert_eq!(
            super::claim_groups(&ids, &BTreeMap::new()),
            vec![(Duration::ZERO, ids.clone())]
        );
    }

    #[test]
    fn one_instance_still_claims_the_plugin_key() {
        let rows = vec![
            super::LoadedClaimId {
                instance_id: Some("instance-a".into()),
                plugin_key: "path:ga".into(),
                alias: "graphicaudio".into(),
            },
            super::LoadedClaimId {
                instance_id: Some("instance-b".into()),
                plugin_key: "path:other".into(),
                alias: "audible".into(),
            },
        ];
        let ids = super::claim_ids_for_loaded(&rows);
        assert!(ids.iter().any(|id| id == "instance-a"));
        assert!(ids.iter().any(|id| id == "path:ga"));
        assert!(ids.iter().any(|id| id == "path:other"));
    }

    #[test]
    fn two_instances_of_one_key_do_not_claim_that_key() {
        let rows = vec![
            super::LoadedClaimId {
                instance_id: Some("instance-a".into()),
                plugin_key: "path:ga".into(),
                alias: "graphicaudio".into(),
            },
            super::LoadedClaimId {
                instance_id: Some("instance-b".into()),
                plugin_key: "path:ga".into(),
                alias: "graphicaudio".into(),
            },
        ];
        let ids = super::claim_ids_for_loaded(&rows);
        assert!(ids.iter().any(|id| id == "instance-a"));
        assert!(ids.iter().any(|id| id == "instance-b"));
        assert!(!ids.iter().any(|id| id == "path:ga"));
        assert!(!ids.iter().any(|id| id == "graphicaudio"));
    }

    #[tokio::test]
    async fn claim_accepts_plugin_key_queued_before_instance_id() {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let created = store
            .publish_domain_event(publish_spec("book_acquired:claim-key"))
            .await
            .unwrap();
        let PublishDomainEventOutcome::Created { id } = created else {
            panic!("{created:?}");
        };
        store
            .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("path:ga")], "claim-key")
            .await
            .unwrap();
        let claimed = store
            .claim_next_event_delivery(
                "claim-key",
                60,
                &uuid::Uuid::new_v4().to_string(),
                &["instance-a".into(), "path:ga".into()],
                32,
                "",
            )
            .await
            .unwrap();
        assert!(
            claimed.is_some(),
            "unique instance still drains the plugin key"
        );

        let second = store
            .publish_domain_event(publish_spec("book_acquired:claim-ambiguous"))
            .await
            .unwrap();
        let PublishDomainEventOutcome::Created { id: second_id } = second else {
            panic!("{second:?}");
        };
        store
            .dispatch_event_deliveries(
                &second_id,
                &[EventSubscriber::plugin("path:ga")],
                "claim-ambiguous",
            )
            .await
            .unwrap();
        let missed = store
            .claim_next_event_delivery(
                "claim-ambiguous",
                60,
                &uuid::Uuid::new_v4().to_string(),
                &["instance-a".into(), "instance-b".into()],
                32,
                "",
            )
            .await
            .unwrap();
        assert!(
            missed.is_none(),
            "two instances must not claim the shared key"
        );
    }

    /// A queue consumer whose every invocation fails (the trait's default
    /// `deliver_queue_batch_cancelable`).
    struct FailingConsumer;

    #[async_trait::async_trait]
    impl Integration for FailingConsumer {
        fn id(&self) -> &str {
            "echo"
        }

        async fn start(
            &self,
            _ctx: bookclerk_integrations::IntegrationContext,
        ) -> bookclerk_integrations::Result<()> {
            Ok(())
        }

        async fn stop(&self) -> bookclerk_integrations::Result<()> {
            Ok(())
        }

        async fn health(
            &self,
        ) -> bookclerk_integrations::Result<bookclerk_integrations::IntegrationHealth> {
            Ok(bookclerk_integrations::IntegrationHealth {
                id: "echo".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        async fn deliver_domain_event(
            &self,
            _event: DomainEvent,
        ) -> bookclerk_integrations::Result<EventResult> {
            Ok(EventResult::Ack)
        }
    }

    #[tokio::test]
    async fn queue_failure_on_the_last_attempt_goes_to_the_dead_letter_queue() {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let published = store
            .publish_domain_event(PublishDomainEventSpec {
                dedup_key: "q:last-attempt".into(),
                payload: r#"{"n":1}"#.into(),
                queue_name: "echo".into(),
                ..PublishDomainEventSpec::default()
            })
            .await
            .unwrap();
        let PublishDomainEventOutcome::Created { id: event_id } = published else {
            panic!("{published:?}");
        };
        store
            .dispatch_event_deliveries(
                &event_id,
                &[EventSubscriber {
                    max_attempts: Some(1),
                    ..EventSubscriber::plugin("echo")
                }],
                "q-last-attempt",
            )
            .await
            .unwrap();
        let delivery = store
            .claim_next_event_delivery(
                "worker",
                60,
                &uuid::Uuid::new_v4().to_string(),
                &["echo".into()],
                32,
                "",
            )
            .await
            .unwrap()
            .expect("claim");
        assert_eq!(delivery.attempt_count, delivery.max_attempts);
        let event = store.get_domain_event(&event_id).await.unwrap().unwrap();
        let mut consumer = EventCatalogSubscription::queue("echo");
        consumer.dead_letter_queue = Some("echo-dlq".into());

        run_queue_delivery(&store, &FailingConsumer, &event, &delivery, &[consumer]).await;

        let row = store
            .get_event_delivery(&delivery.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "dead_letter");
        let dead = store
            .list_domain_events_for_queue("echo-dlq", 10)
            .await
            .unwrap();
        assert_eq!(dead.len(), 1, "{dead:?}");
        assert_eq!(dead[0].causation_id, event_id);
        assert_eq!(dead[0].payload, r#"{"n":1}"#);
    }
}
