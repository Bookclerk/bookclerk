//! The library's page index (`library_work_rows`), kept current by library
//! events.
//!
//! `GET /api/library/works` pages one row per held work from this index
//! (see [`bookclerk_library::LibraryStore::library_works_page`]). Two pieces
//! keep it current:
//!
//! - **Event consumer.** The daemon registers one cluster-wide consumer,
//!   [`LIBRARY_INDEX_CONSUMER_ID`], on the durable outbox for the library
//!   model's events ([`INDEX_EVENT_TYPES`]). Every node registers the same
//!   id, so the catalog collapses them into one subscriber and each event
//!   is handled once in the cluster. An edition event refreshes the
//!   edition's work, a holding event the work of its listing, and
//!   `book_acquired` the work of its edition (or title), and
//!   `edition_acquire_changed` the work of its edition.
//! - **Rebuild job.** When the library has no finished build of
//!   [`bookclerk_library::LIBRARY_INDEX_LAYOUT`] (a new library, a database
//!   copied to another backend, a new layout), the heartbeat queues one
//!   `library_index_rebuild` job, which any node claims and which walks
//!   every work in pages.
//!
//! Rows are refreshed from the model as it is when the refresh reads it, so
//! a delivery that runs late, twice, or beside the rebuild still leaves the
//! right row. See `docs/internals/jobs.md` (*Library index*).

use std::time::Duration;

use bookclerk_library::{
    DomainEventRecord, EnqueueJobSpec, EventCatalogSubscription, EventDeliveryRecord, JobKind,
    JobPayload, JobTrigger, LibraryStore,
};
use chrono::Utc;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};

/// Catalog id of the page index consumer. The `host:` scheme is not a
/// plugin key provenance nor a node queue consumer (`node:`), so it never
/// collides with a plugin's deliveries.
pub const LIBRARY_INDEX_CONSUMER_ID: &str = "host:library-index";

/// The event types that change a work's row.
///
/// Every one is a host event type
/// ([`bookclerk_plugin_manifest::HOST_EVENT_TYPES`]), which no plugin may
/// produce, so the payloads come from the host.
pub const INDEX_EVENT_TYPES: [&str; 6] = [
    "edition_linked",
    "edition_acquire_changed",
    "edition_updated",
    "holding_added",
    "holding_removed",
    "book_acquired",
];

/// Redeliveries before a delivery is dead-lettered. A failed refresh is a
/// database error, which the rebuild would hit as well, so a few retries
/// suffice.
const MAX_RETRIES: u32 = 10;

/// How long after a rebuild was admitted the heartbeat waits before it
/// queues another while the index is still unbuilt, so a rebuild that keeps
/// failing is retried every few minutes rather than every beat.
const REBUILD_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Claim priority of a rebuild, above the default 0: the library page is
/// empty or stale until it runs.
const REBUILD_PRIORITY: i64 = 50;

/// True when `plugin_id` is the page index consumer.
#[must_use]
pub fn is_library_index_consumer_id(plugin_id: &str) -> bool {
    plugin_id == LIBRARY_INDEX_CONSUMER_ID
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

/// Heartbeats this node's row for the page index consumer.
///
/// # Arguments
///
/// * `library` - Open library store.
/// * `node_id` - This node's catalog id.
pub async fn upsert_catalog_row(library: &LibraryStore, node_id: &str) {
    if let Err(err) = library
        .upsert_event_subscriber(
            node_id,
            LIBRARY_INDEX_CONSUMER_ID,
            &catalog_subscriptions(),
            true,
        )
        .await
    {
        warn!(error = %err, "library index consumer catalog upsert failed");
    }
}

/// What an event changed, in the terms the index refreshes by.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    /// An edition's metadata, links or acquire state (`edition_linked`,
    /// `edition_updated`, `edition_acquire_changed`).
    Edition(String),
    /// One listing's holders (`holding_added`, `holding_removed`).
    Listing(String),
    /// An acquire (`book_acquired`): its edition when the payload names
    /// one, else its library title.
    Acquired {
        /// The acquired edition.
        edition_id: Option<String>,
        /// The acquired title's `books.uuid`.
        title_id: String,
    },
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
    /// The acquired edition, when the title has one.
    #[serde(default)]
    edition_id: Option<String>,
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
        "edition_linked" | "edition_updated" | "edition_acquire_changed" => {
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
            .map(|p| Change::Acquired {
                edition_id: p.edition_id.filter(|id| !id.is_empty()),
                title_id: p.title_id,
            })
            .map_err(bad),
        other => Err(format!("not a library index event: {other}")),
    }
}

/// The work a change touched; `None` when the model no longer has it (an
/// edition or listing deleted since, whose work's row the next event or
/// rebuild settles).
///
/// # Errors
///
/// Returns the library error of a failed read.
async fn changed_work(
    library: &LibraryStore,
    change: &Change,
) -> bookclerk_library::Result<Option<String>> {
    match change {
        Change::Edition(edition_id) => library.work_of_edition(edition_id).await,
        Change::Listing(listing_id) => library.work_of_listing(listing_id).await,
        Change::Acquired {
            edition_id: Some(edition_id),
            ..
        } => library.work_of_edition(edition_id).await,
        Change::Acquired {
            edition_id: None,
            title_id,
        } => library.work_id_for_book(title_id).await,
    }
}

/// Handles one claimed delivery for the page index consumer: refresh the
/// work the event touched, then ack; retry a failed refresh; dead-letter a
/// body this consumer cannot read.
///
/// # Arguments
///
/// * `library` - Open library store.
/// * `event` - The delivered event.
/// * `delivery` - The claimed delivery.
pub async fn run_library_index_delivery(
    library: &LibraryStore,
    event: &DomainEventRecord,
    delivery: &EventDeliveryRecord,
) {
    let fence = delivery.fence();
    let change = match parse_change(event) {
        Ok(change) => change,
        Err(reason) => {
            warn!(event_id = %event.id, %reason, "library index consumer refused an event");
            let _ = library.dead_letter_event_delivery(&fence, &reason).await;
            return;
        }
    };
    let outcome = match changed_work(library, &change).await {
        Ok(Some(work_id)) => library
            .refresh_library_works(std::slice::from_ref(&work_id))
            .await
            .map(|written| (Some(work_id), written)),
        Ok(None) => Ok((None, 0)),
        Err(err) => Err(err),
    };
    let result = match outcome {
        Ok((work_id, written)) => {
            debug!(event_id = %event.id, ?change, ?work_id, written, "library index refreshed");
            library.ack_event_delivery(&fence).await
        }
        Err(err) => {
            warn!(event_id = %event.id, error = %err, "library index refresh failed");
            library.fail_event_delivery(&fence, &err.to_string()).await
        }
    };
    if let Err(err) = result {
        warn!(delivery_id = %delivery.id, error = %err, "library index delivery update failed");
    }
}

/// The payload of the rebuild job.
fn rebuild_payload() -> JobPayload {
    JobPayload {
        trigger: JobTrigger::Scheduler,
        ..JobPayload::default()
    }
}

/// Admits the `library_index_rebuild` job.
///
/// # Returns
///
/// The admission outcome; a rebuild already queued or running is a
/// [`AdmitJob::Duplicate`].
///
/// # Errors
///
/// Returns the admission error.
pub(crate) async fn enqueue_rebuild(state: &AppState) -> anyhow::Result<AdmitJob> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    admit(
        state,
        EnqueueJobSpec {
            kind: JobKind::LibraryIndexRebuild,
            payload: rebuild_payload(),
            priority: REBUILD_PRIORITY,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Queues the rebuild when the library has no finished build of the
/// current layout and none was admitted in the last [`REBUILD_BACKOFF`].
/// The heartbeat calls it once its beat is recorded.
///
/// # Arguments
///
/// * `state` - The daemon.
pub(crate) async fn queue_rebuild_if_missing(state: &AppState) {
    let library = state.library_snapshot().await;
    match library.library_index_built().await {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            warn!(error = %err, "could not read the library index build state");
            return;
        }
    }
    let since = chrono::Duration::from_std(REBUILD_BACKOFF)
        .map(|d| Utc::now() - d)
        .unwrap_or_else(|_| Utc::now());
    let key = JobKind::LibraryIndexRebuild.dedup_key(&rebuild_payload());
    match library.job_admitted_since(&key, since).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            warn!(error = %err, "could not read recent library index rebuilds");
            return;
        }
    }
    match enqueue_rebuild(state).await {
        Ok(AdmitJob::Created(id)) => debug!(job_id = %id, "library index rebuild queued"),
        Ok(AdmitJob::Duplicate(_)) => {}
        Ok(AdmitJob::QueueFull) => warn!("job queue is full; library index rebuild not queued"),
        Err(err) => warn!(error = %err, "could not queue the library index rebuild"),
    }
}

/// Runs the `library_index_rebuild` job: refreshes every top-level work's
/// row in pages, then records the build.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `ctx` - The claim, for progress and cancellation.
///
/// # Returns
///
/// A one-line summary for the job row.
///
/// # Errors
///
/// Fails when the job is cancelled or the library cannot be read or
/// written; the job's retries start the walk again.
pub(crate) async fn run_library_index_rebuild(
    state: &AppState,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let library = state.library_snapshot().await;
    let mut after: Option<String> = None;
    let mut pages = 0u64;
    loop {
        if job_cancelled(ctx, &library).await {
            anyhow::bail!("cancelled");
        }
        let Some(last) = library.rebuild_library_index_page(after.as_deref()).await? else {
            break;
        };
        pages += 1;
        if let Some(ctx) = ctx {
            let _ = library
                .set_job_progress(&ctx.fence, &format!("{pages} page(s) of works indexed"))
                .await;
        }
        after = Some(last);
    }
    library.finish_library_index_build().await?;
    Ok(format!("library index rebuilt: {pages} page(s) of works"))
}

#[cfg(test)]
mod tests {
    use bookclerk_library::{catalog_subscribers_for_event, NewBook, WorkPageQuery};

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
        for kind in [
            "edition_linked",
            "edition_updated",
            "edition_acquire_changed",
        ] {
            assert_eq!(
                parse_change(&event(kind, r#"{"editionId":"ed"}"#)),
                Ok(Change::Edition("ed".into()))
            );
        }
        for kind in ["holding_added", "holding_removed"] {
            assert_eq!(
                parse_change(&event(kind, r#"{"listingId":"l","holder":"x"}"#)),
                Ok(Change::Listing("l".into()))
            );
        }
        assert_eq!(
            parse_change(&event(
                "book_acquired",
                r#"{"titleId":"t","editionId":"ed"}"#
            )),
            Ok(Change::Acquired {
                edition_id: Some("ed".into()),
                title_id: "t".into()
            })
        );
        assert_eq!(
            parse_change(&event("book_acquired", r#"{"titleId":"t","editionId":""}"#)),
            Ok(Change::Acquired {
                edition_id: None,
                title_id: "t".into()
            })
        );
        assert!(parse_change(&event("edition_linked", "{}")).is_err());
        assert!(parse_change(&event("wishlist_changed", "{}")).is_err());
    }

    #[test]
    fn the_consumer_subscribes_to_every_index_event() {
        assert!(is_library_index_consumer_id(LIBRARY_INDEX_CONSUMER_ID));
        assert!(!is_library_index_consumer_id("host:discover"));
        let subs = catalog_subscriptions();
        let types: Vec<&str> = subs.iter().map(|s| s.event_type.as_str()).collect();
        assert_eq!(types, INDEX_EVENT_TYPES);
        assert!(types
            .iter()
            .all(|t| bookclerk_plugin_manifest::is_host_event_type(t)));
        assert!(subs
            .iter()
            .all(|s| s.max_attempts() == Some(MAX_RETRIES + 1)));
    }

    /// Delivers every pending outbox event to the page index consumer.
    async fn deliver_all(library: &LibraryStore, node: &str) -> usize {
        let consumer = LIBRARY_INDEX_CONSUMER_ID.to_string();
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
                .lease_next_event_delivery(
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
            run_library_index_delivery(library, &event, &delivery).await;
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

    /// Every title on the page index's first page.
    async fn titles(library: &LibraryStore) -> Vec<String> {
        library
            .library_works_page(&WorkPageQuery {
                limit: 50,
                ..WorkPageQuery::default()
            })
            .await
            .expect("page")
            .works
            .into_iter()
            .map(|w| w.title)
            .collect()
    }

    #[tokio::test]
    async fn a_scanned_title_reaches_the_index_through_its_events() {
        let files = tempfile::tempdir().expect("tempdir");
        let (_state, library) = crate::node_api::tests::test_state(&files).await;
        let node = "node-a";
        upsert_catalog_row(&library, node).await;
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .expect("account");
        let book = library
            .upsert_book(&NewBook::minimal(
                "B0EVENT01",
                "acct",
                "us",
                "Evented Title",
            ))
            .await
            .expect("book");
        assert!(deliver_all(&library, node).await >= 1);
        assert_eq!(titles(&library).await, ["Evented Title"]);

        let work = library.work_id_for_book(&book.uuid).await.unwrap();
        let edition = library.edition_of_book(&book.uuid).await.unwrap().unwrap();
        for change in [
            Change::Edition(edition.clone()),
            Change::Acquired {
                edition_id: Some(edition),
                title_id: "unused".into(),
            },
            Change::Acquired {
                edition_id: None,
                title_id: book.uuid.clone(),
            },
        ] {
            assert_eq!(
                changed_work(&library, &change).await.unwrap(),
                work,
                "{change:?}"
            );
        }
        assert_eq!(
            changed_work(&library, &Change::Listing("gone".into()))
                .await
                .unwrap(),
            None
        );

        // The holding goes, and its event takes the row with it.
        library
            .remove_account_holdings("audible", "acct", bookclerk_library::HoldingSelection::All)
            .await
            .expect("remove");
        assert!(deliver_all(&library, node).await >= 1);
        assert!(titles(&library).await.is_empty());
        // Events alone do not make a built index.
        assert!(!library.library_index_built().await.unwrap());
    }

    #[tokio::test]
    async fn the_heartbeat_queues_one_rebuild_until_the_index_is_built() {
        let files = tempfile::tempdir().expect("tempdir");
        let (state, library) = crate::node_api::tests::test_state(&files).await;
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
        queue_rebuild_if_missing(&state).await;
        let key = JobKind::LibraryIndexRebuild.dedup_key(&rebuild_payload());
        assert!(library
            .job_admitted_since(&key, Utc::now() - chrono::Duration::minutes(1))
            .await
            .unwrap());
        // A second beat inside the backoff queues nothing new, and an
        // admission folds into the queued rebuild.
        queue_rebuild_if_missing(&state).await;
        let AdmitJob::Duplicate(id) = enqueue_rebuild(&state).await.expect("admit") else {
            panic!("the queued rebuild absorbs another");
        };
        let job = library.get_job(&id).await.expect("read").expect("row");
        assert_eq!(job.kind, JobKind::LibraryIndexRebuild);

        let summary = run_library_index_rebuild(&state, None)
            .await
            .expect("rebuild");
        assert!(summary.contains("1 page"), "{summary}");
        assert_eq!(titles(&library).await, ["Rebuilt Title"]);
        assert!(library.library_index_built().await.unwrap());
        // A built index needs no rebuild.
        queue_rebuild_if_missing(&state).await;
    }
}
