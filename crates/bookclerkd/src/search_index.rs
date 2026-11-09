//! This node's full-text search index, kept current by library events.
//!
//! `GET /api/library/books?q=` reads a Tantivy index under the node's own
//! `search_index/`, so every daemon keeps its own copy. Two pieces keep it
//! current without a CLI run:
//!
//! - **Event consumer.** Each node registers its own consumer,
//!   `host:search-index:<node id>`, on the durable outbox for the library
//!   model's events ([`INDEX_EVENT_TYPES`]). Because the id names the node,
//!   every node receives every event (unlike Discover's cluster-wide
//!   consumer). An edition event refreshes the titles of each listing that
//!   sells the edition, a holding event the titles of its listing, and
//!   `book_acquired` its one title. A refresh deletes the listing's
//!   documents and re-adds the titles that still sell it, so
//!   `holding_removed`, which book model step 5 adds with the same
//!   `listingId` payload as `holding_added`, drops a title that is gone;
//!   the consumer already parses it, and that step subscribes to it.
//! - **Rebuild job.** When the index has no finished rebuild of the current
//!   layout (a new node, a deleted directory, an older layout), the
//!   heartbeat queues a `search_index_rebuild` job pinned to this node
//!   ([`bookclerk_library::JobPayload::node_id`]), which reads the whole
//!   library in pages. Events delivered while it runs wait for it.
//!
//! See `docs/internals/jobs.md` (*Search index*).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::Context;
use bookclerk_library::{
    DomainEventRecord, EnqueueJobSpec, EventCatalogSubscription, EventDeliveryRecord, JobKind,
    JobPayload, JobTrigger, LibraryStore, ListingRef,
};
use bookclerk_search::{SearchEngine, SearchError};
use chrono::Utc;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};

/// Prefix of a node's search index consumer id. The `host:` scheme is not a
/// plugin key provenance nor a node queue consumer (`node:`), so it never
/// collides with a plugin's deliveries.
const CONSUMER_PREFIX: &str = "host:search-index:";

/// The event types that change what the index holds.
///
/// Every one is a host event type
/// ([`bookclerk_plugin_manifest::HOST_EVENT_TYPES`]), which no plugin may
/// produce, so the payloads come from the host. `holding_removed` already
/// parses ([`parse_change`]); book model step 5, which publishes it, adds it
/// to the host types and here.
pub const INDEX_EVENT_TYPES: [&str; 4] = [
    "edition_linked",
    "edition_updated",
    "holding_added",
    "book_acquired",
];

/// How long a delivery waits before it is tried again when the index is
/// busy with another writer (a rebuild, or the CLI).
const BUSY_RETRY: Duration = Duration::from_secs(10);

/// Redeliveries before a delivery is dead-lettered. A rebuild of a large
/// library can hold the index for minutes, and each wait is
/// [`BUSY_RETRY`], so sixty retries cover ten minutes.
const MAX_RETRIES: u32 = 60;

/// How long after a rebuild was admitted the heartbeat waits before it
/// queues another for an index that is still incomplete, so a rebuild that
/// keeps failing is retried every few minutes rather than every beat.
const REBUILD_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Claim priority of a rebuild, above the default 0: search on this node is
/// empty or stale until it runs.
const REBUILD_PRIORITY: i64 = 50;

/// One writer per index directory in this process, by directory.
///
/// A rebuild holds its directory's gate for its whole run, and a delivery
/// that finds it taken is retried after [`BUSY_RETRY`] rather than holding
/// its lease while it waits. The CLI is another process; Tantivy's own lock
/// refuses it, which surfaces as [`SearchError::Busy`].
static WRITE_GATES: LazyLock<std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// The write gate of the index in `dir`.
fn write_gate(dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut gates = WRITE_GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(gates.entry(dir.to_path_buf()).or_default())
}

/// The search index consumer id of `node_id`.
#[must_use]
pub fn consumer_id(node_id: &str) -> String {
    format!("{CONSUMER_PREFIX}{node_id}")
}

/// True when `plugin_id` is a node's search index consumer.
#[must_use]
pub fn is_search_consumer_id(plugin_id: &str) -> bool {
    plugin_id.starts_with(CONSUMER_PREFIX)
}

/// The consumer's catalog subscriptions: schema 1 of each index event.
#[must_use]
pub fn catalog_subscriptions() -> Vec<EventCatalogSubscription> {
    INDEX_EVENT_TYPES
        .iter()
        .map(|event_type| {
            let mut sub = EventCatalogSubscription::new(*event_type, vec![1]);
            sub.max_retries = Some(MAX_RETRIES);
            sub
        })
        .collect()
}

/// Heartbeats this node's row for its search index consumer.
///
/// # Arguments
///
/// * `library` - Open library store.
/// * `node_id` - This node's catalog id.
pub async fn upsert_catalog_row(library: &LibraryStore, node_id: &str) {
    if let Err(err) = library
        .upsert_event_subscriber(
            node_id,
            &consumer_id(node_id),
            &catalog_subscriptions(),
            true,
        )
        .await
    {
        warn!(error = %err, "search index consumer catalog upsert failed");
    }
}

/// What an event changed, in the terms the index refreshes by.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    /// An edition's metadata or listings (`edition_linked`,
    /// `edition_updated`): every listing that sells it.
    Edition(String),
    /// One listing's holders (`holding_added`, `holding_removed`).
    Listing(String),
    /// One library title (`book_acquired`).
    Title(String),
}

/// The edition events' payload.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditionPayload {
    /// The edition.
    edition_id: String,
}

/// The holding events' payload.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HoldingPayload {
    /// The listing whose holders changed.
    listing_id: String,
}

/// The part of `book_acquired` the consumer reads.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcquiredPayload {
    /// The acquired title's `books.uuid`.
    title_id: String,
}

/// Parses an event into the change it describes.
///
/// # Errors
///
/// Returns why the event cannot be handled: a type this consumer does not
/// subscribe to, or a body that is not the host's.
fn parse_change(event: &DomainEventRecord) -> Result<Change, String> {
    let bad = |err: serde_json::Error| format!("bad {} payload: {err}", event.event_type);
    match event.event_type.as_str() {
        "edition_linked" | "edition_updated" => {
            serde_json::from_str::<EditionPayload>(&event.payload)
                .map(|p| Change::Edition(p.edition_id))
                .map_err(bad)
        }
        "holding_added" | "holding_removed" => {
            serde_json::from_str::<HoldingPayload>(&event.payload)
                .map(|p| Change::Listing(p.listing_id))
                .map_err(bad)
        }
        "book_acquired" => serde_json::from_str::<AcquiredPayload>(&event.payload)
            .map(|p| Change::Title(p.title_id))
            .map_err(bad),
        other => Err(format!("not a search index event: {other}")),
    }
}

/// Writes one change into the index.
///
/// # Returns
///
/// How many documents were written.
///
/// # Errors
///
/// Returns [`SearchError::Busy`] when another writer holds the index, or
/// the library or index error.
async fn apply_change(
    engine: &SearchEngine,
    library: &LibraryStore,
    change: &Change,
) -> bookclerk_search::Result<usize> {
    match change {
        Change::Edition(edition_id) => {
            let listings = library.listing_refs_of_edition(edition_id).await?;
            engine.refresh_listings(library, &listings).await
        }
        Change::Listing(listing_id) => {
            // A listing that is gone has no titles left to show; its
            // documents go with the next rebuild.
            let listings: Vec<ListingRef> = library
                .listing_ref_by_id(listing_id)
                .await?
                .into_iter()
                .collect();
            engine.refresh_listings(library, &listings).await
        }
        Change::Title(uuid) => {
            engine
                .refresh_titles(library, std::slice::from_ref(uuid))
                .await
        }
    }
}

/// Handles one claimed delivery for this node's search index consumer:
/// refresh the documents the event touched, then ack; retry shortly when
/// the index is busy; dead-letter a body this consumer cannot read.
///
/// # Arguments
///
/// * `state` - Daemon state (for the index directory).
/// * `library` - Open library store.
/// * `event` - The delivered event.
/// * `delivery` - The claimed delivery.
pub async fn run_search_delivery(
    state: &AppState,
    library: &LibraryStore,
    event: &DomainEventRecord,
    delivery: &EventDeliveryRecord,
) {
    let fence = delivery.fence();
    let change = match parse_change(event) {
        Ok(change) => change,
        Err(reason) => {
            warn!(event_id = %event.id, %reason, "search index consumer refused an event");
            let _ = library.dead_letter_event_delivery(&fence, &reason).await;
            return;
        }
    };
    let dir = state.config.read().await.paths().search_index_dir.clone();
    let gate = write_gate(&dir);
    let outcome = match gate.try_lock() {
        Err(_) => Err(SearchError::Busy),
        Ok(_gate) => match SearchEngine::open(&dir) {
            Ok(engine) => apply_change(&engine, library, &change).await,
            Err(err) => Err(err),
        },
    };
    let result = match outcome {
        Ok(written) => {
            debug!(event_id = %event.id, ?change, written, "search index refreshed");
            library.ack_event_delivery(&fence).await
        }
        Err(SearchError::Busy) => {
            let run_after = chrono::Duration::from_std(BUSY_RETRY)
                .map(|d| Utc::now() + d)
                .unwrap_or_else(|_| Utc::now());
            library
                .retry_event_delivery(&fence, run_after, "search index busy with another writer")
                .await
        }
        Err(err) => {
            warn!(event_id = %event.id, error = %err, "search index refresh failed");
            library.fail_event_delivery(&fence, &err.to_string()).await
        }
    };
    if let Err(err) = result {
        warn!(delivery_id = %delivery.id, error = %err, "search index delivery update failed");
    }
}

/// Admits a `search_index_rebuild` job pinned to `node_id`.
///
/// # Returns
///
/// The admission outcome; a rebuild already queued or running for the node
/// is a [`AdmitJob::Duplicate`].
///
/// # Errors
///
/// Returns the admission error.
pub(crate) async fn enqueue_rebuild(state: &AppState, node_id: &str) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    admit(
        state,
        EnqueueJobSpec {
            kind: JobKind::SearchIndexRebuild,
            payload: rebuild_payload(node_id),
            priority: REBUILD_PRIORITY,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// The payload of `node_id`'s rebuild job.
fn rebuild_payload(node_id: &str) -> JobPayload {
    JobPayload {
        trigger: JobTrigger::Scheduler,
        node_id: Some(node_id.to_owned()),
        ..JobPayload::default()
    }
}

/// Queues a rebuild for `node_id` when its index has no finished rebuild of
/// the current layout and no rebuild was admitted in the last
/// [`REBUILD_BACKOFF`]. The heartbeat calls it once its beat is recorded.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `node_id` - This node.
pub(crate) async fn queue_rebuild_if_missing(state: &AppState, node_id: &str) {
    let dir = state.config.read().await.paths().search_index_dir.clone();
    if SearchEngine::is_complete(&dir) {
        return;
    }
    let library = state.library_snapshot().await;
    let since = chrono::Duration::from_std(REBUILD_BACKOFF)
        .map(|d| Utc::now() - d)
        .unwrap_or_else(|_| Utc::now());
    let key = JobKind::SearchIndexRebuild.dedup_key(&rebuild_payload(node_id));
    match library.job_admitted_since(&key, since).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            warn!(error = %err, "could not read recent search index rebuilds");
            return;
        }
    }
    match enqueue_rebuild(state, node_id).await {
        Ok(AdmitJob::Created(id)) => debug!(job_id = %id, "search index rebuild queued"),
        Ok(AdmitJob::Duplicate(_)) => {}
        Ok(AdmitJob::QueueFull) => warn!("job queue is full; search index rebuild not queued"),
        Err(err) => warn!(error = %err, "could not queue the search index rebuild"),
    }
}

/// Runs a `search_index_rebuild` job on the node it is pinned to.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `payload` - The job's payload, naming its node.
/// * `ctx` - The claim, for cancellation.
///
/// # Returns
///
/// A one-line summary for the job row.
///
/// # Errors
///
/// Fails when the job names another node (or none), when it is cancelled,
/// or when the library cannot be read or the index written (including
/// another process holding the index, which the job's retries cover).
pub(crate) async fn run_search_index_rebuild(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let this_node = bookclerk_library::control_plane::load_or_create_node_identity(&files_dir)
        .context("node identity")?
        .node_id;
    match payload.node_id.as_deref() {
        Some(pinned) if pinned == this_node => {}
        Some(pinned) => {
            anyhow::bail!("search_index_rebuild for node {pinned} claimed on {this_node}")
        }
        None => anyhow::bail!("search_index_rebuild names no node"),
    }
    let library = state.library_snapshot().await;
    if job_cancelled(ctx, &library).await {
        anyhow::bail!("cancelled");
    }
    let dir = state.config.read().await.paths().search_index_dir.clone();
    let gate = write_gate(&dir);
    let _held = gate.lock().await;
    let engine = SearchEngine::open(&dir)?;
    let indexed = engine.rebuild(&library).await?;
    Ok(format!("search index rebuilt: {indexed} title(s)"))
}

#[cfg(test)]
mod tests {
    use bookclerk_library::{catalog_subscribers_for_event, JobState, NewBook};

    use super::*;

    /// An outbox record with `event_type` and `payload`, for the parser.
    fn event(event_type: &str, payload: &str) -> DomainEventRecord {
        serde_json::from_value(serde_json::json!({
            "id": "e",
            "event_type": event_type,
            "schema_version": 1,
            "occurred_at": Utc::now(),
            "account_id": "",
            "correlation_id": "",
            "causation_id": "",
            "dedup_key": "",
            "payload": payload,
            "ordering_key": "",
            "dispatch_state": "pending",
            "created_at": Utc::now(),
        }))
        .expect("record")
    }

    #[test]
    fn events_parse_into_what_they_changed() {
        assert_eq!(
            parse_change(&event(
                "edition_linked",
                r#"{"editionId":"ed","listingId":"l"}"#
            )),
            Ok(Change::Edition("ed".into()))
        );
        assert_eq!(
            parse_change(&event("edition_updated", r#"{"editionId":"ed"}"#)),
            Ok(Change::Edition("ed".into()))
        );
        for kind in ["holding_added", "holding_removed"] {
            assert_eq!(
                parse_change(&event(kind, r#"{"listingId":"l","holder":"account:a"}"#)),
                Ok(Change::Listing("l".into()))
            );
        }
        assert_eq!(
            parse_change(&event("book_acquired", r#"{"titleId":"t","pathKeys":[]}"#)),
            Ok(Change::Title("t".into()))
        );
        assert!(parse_change(&event("edition_linked", "{}")).is_err());
        assert!(parse_change(&event("wishlist_changed", "{}")).is_err());
    }

    #[test]
    fn each_node_has_its_own_consumer_that_collides_with_nothing() {
        let a = consumer_id("node-a");
        assert_ne!(a, consumer_id("node-b"));
        assert!(is_search_consumer_id(&a));
        assert!(!is_search_consumer_id(
            crate::discover_events::DISCOVER_CONSUMER_ID
        ));
        assert!(!bookclerk_library::control_plane::is_node_consumer_id(&a));
        assert!(bookclerk_plugin_catalog::PluginKey::parse(&a).is_err());
        let subs = catalog_subscriptions();
        let types: Vec<&str> = subs.iter().map(|s| s.event_type.as_str()).collect();
        assert_eq!(types, INDEX_EVENT_TYPES);
        assert!(INDEX_EVENT_TYPES
            .iter()
            .all(|t| bookclerk_plugin_manifest::is_host_event_type(t)));
        assert!(subs
            .iter()
            .all(|s| s.max_attempts() == Some(MAX_RETRIES + 1)));
    }

    /// Delivers every pending outbox event to this node's consumer.
    async fn deliver_all(state: &AppState, library: &LibraryStore, node: &str) -> usize {
        let consumer = consumer_id(node);
        let mut delivered = 0;
        while let Some(event) = library.next_undispatched_event().await.expect("next") {
            let catalog = library
                .list_live_event_subscribers()
                .await
                .expect("catalog");
            let subscribers = catalog_subscribers_for_event(&catalog, &event)
                .into_iter()
                .filter(|s| s.plugin_id == consumer)
                .collect::<Vec<_>>();
            library
                .dispatch_event_deliveries(
                    &event.id,
                    &subscribers,
                    &uuid::Uuid::new_v4().to_string(),
                )
                .await
                .expect("dispatch");
            if subscribers.is_empty() {
                continue;
            }
            let delivery = library
                .claim_next_event_delivery(
                    "test-owner",
                    60,
                    &uuid::Uuid::new_v4().to_string(),
                    std::slice::from_ref(&consumer),
                    4,
                    node,
                )
                .await
                .expect("claim")
                .expect("a delivery");
            run_search_delivery(state, library, &event, &delivery).await;
            let after = library
                .get_event_delivery(&delivery.id)
                .await
                .expect("read")
                .expect("row");
            assert_eq!(after.state, "acked", "{}", event.event_type);
            delivered += 1;
        }
        delivered
    }

    #[tokio::test]
    async fn a_scanned_title_reaches_the_index_through_its_events() {
        let files = tempfile::tempdir().expect("tempdir");
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        let node = "node-a";
        upsert_catalog_row(&library, node).await;
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .expect("account");
        library
            .upsert_book(&NewBook::minimal(
                "B0EVENT01",
                "acct",
                "us",
                "Evented Title",
            ))
            .await
            .expect("book");

        assert!(deliver_all(&state, &library, node).await >= 1);
        let dir = state.config.read().await.paths().search_index_dir.clone();
        let hits = SearchEngine::open(&dir)
            .expect("open")
            .search("evented", 10)
            .expect("search");
        assert_eq!(hits.len(), 1);
        // Events alone do not make a rebuilt index.
        assert!(!SearchEngine::is_complete(&dir));
    }

    #[tokio::test]
    async fn a_busy_index_sends_the_delivery_back_and_a_bad_body_is_parked() {
        let files = tempfile::tempdir().expect("tempdir");
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        let node = "node-a";
        upsert_catalog_row(&library, node).await;
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .expect("account");
        library
            .upsert_book(&NewBook::minimal("B0BUSY001", "acct", "us", "Busy Title"))
            .await
            .expect("book");
        let consumer = consumer_id(node);
        let event = library
            .next_undispatched_event()
            .await
            .expect("next")
            .expect("an event");
        let catalog = library
            .list_live_event_subscribers()
            .await
            .expect("catalog");
        let subscribers = catalog_subscribers_for_event(&catalog, &event)
            .into_iter()
            .filter(|s| s.plugin_id == consumer)
            .collect::<Vec<_>>();
        library
            .dispatch_event_deliveries(&event.id, &subscribers, "op")
            .await
            .expect("dispatch");
        let delivery = library
            .claim_next_event_delivery("o", 60, "op2", std::slice::from_ref(&consumer), 4, node)
            .await
            .expect("claim")
            .expect("delivery");
        let dir = state.config.read().await.paths().search_index_dir.clone();
        {
            let gate = write_gate(&dir);
            let _held = gate.lock().await;
            run_search_delivery(&state, &library, &event, &delivery).await;
        }
        let row = library
            .get_event_delivery(&delivery.id)
            .await
            .expect("read")
            .expect("row");
        assert_ne!(row.state, "acked");
        assert_ne!(row.state, "dead_letter");

        let delivery = publish_and_claim(&library, &event.event_type, "{}", &consumer, node).await;
        let bogus = library
            .get_domain_event(&delivery.event_id)
            .await
            .expect("read")
            .expect("row");
        run_search_delivery(&state, &library, &bogus, &delivery).await;
        let row = library
            .get_event_delivery(&delivery.id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.state, "dead_letter");
    }

    /// Publishes an `event_type` event with `payload`, dispatches it to
    /// `consumer` and claims the delivery as `node`.
    async fn publish_and_claim(
        library: &LibraryStore,
        event_type: &str,
        payload: &str,
        consumer: &str,
        node: &str,
    ) -> EventDeliveryRecord {
        let outcome = library
            .publish_domain_event(bookclerk_library::PublishDomainEventSpec {
                event_type: event_type.into(),
                schema_version: 1,
                dedup_key: uuid::Uuid::new_v4().to_string(),
                payload: payload.into(),
                ..bookclerk_library::PublishDomainEventSpec::default()
            })
            .await
            .expect("publish");
        let id = match outcome {
            bookclerk_library::PublishDomainEventOutcome::Created { id } => id,
            other => panic!("unexpected outcome {other:?}"),
        };
        let stored = library
            .get_domain_event(&id)
            .await
            .expect("read")
            .expect("row");
        let catalog = library
            .list_live_event_subscribers()
            .await
            .expect("catalog");
        let subscribers = catalog_subscribers_for_event(&catalog, &stored)
            .into_iter()
            .filter(|s| s.plugin_id == consumer)
            .collect::<Vec<_>>();
        library
            .dispatch_event_deliveries(&stored.id, &subscribers, "op3")
            .await
            .expect("dispatch");
        library
            .claim_next_event_delivery("o", 60, "op4", &[consumer.to_string()], 4, node)
            .await
            .expect("claim")
            .expect("delivery")
    }

    #[tokio::test]
    async fn a_missing_index_queues_one_rebuild_that_runs_only_on_its_node() {
        let files = tempfile::tempdir().expect("tempdir");
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        let files_dir = state.config.read().await.paths().files_dir.clone();
        let node = bookclerk_library::control_plane::load_or_create_node_identity(&files_dir)
            .expect("identity")
            .node_id;
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .expect("account");
        library
            .upsert_book(&NewBook::minimal(
                "B0REBUILD",
                "acct",
                "us",
                "Rebuilt Title",
            ))
            .await
            .expect("book");

        queue_rebuild_if_missing(&state, &node).await;
        let key = JobKind::SearchIndexRebuild.dedup_key(&rebuild_payload(&node));
        assert!(library
            .job_admitted_since(&key, Utc::now() - chrono::Duration::minutes(1))
            .await
            .expect("read"));
        let AdmitJob::Duplicate(id) = enqueue_rebuild(&state, &node).await.expect("admit") else {
            panic!("the queued rebuild absorbs another");
        };
        let job = library.get_job(&id).await.expect("read").expect("row");
        assert_eq!(job.kind, JobKind::SearchIndexRebuild);
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.payload.node_id.as_deref(), Some(node.as_str()));

        assert!(
            run_search_index_rebuild(&state, &rebuild_payload("elsewhere"), None)
                .await
                .is_err()
        );
        assert!(
            run_search_index_rebuild(&state, &JobPayload::default(), None)
                .await
                .is_err()
        );
        let summary = run_search_index_rebuild(&state, &job.payload, None)
            .await
            .expect("rebuild");
        assert_eq!(summary, "search index rebuilt: 1 title(s)");
        let dir = state.config.read().await.paths().search_index_dir.clone();
        assert!(SearchEngine::is_complete(&dir));
        assert_eq!(
            SearchEngine::open(&dir)
                .expect("open")
                .search("rebuilt", 10)
                .expect("search")
                .len(),
            1
        );
    }
}
