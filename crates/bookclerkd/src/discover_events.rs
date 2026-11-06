//! Discover's host consumer of taste events.
//!
//! Stored shelves go stale when someone's taste changes: a wish is added or
//! dropped (`wishlist_changed`), a listening sync shows someone started or
//! finished a book (`listening_synced`), or the household acquires a book
//! (`book_acquired`). The daemon registers one cluster-wide consumer,
//! [`DISCOVER_CONSUMER_ID`], on the durable outbox for those types, and each
//! delivery queues the Discover jobs the change affects, each claimable only
//! after [`EVENT_DEBOUNCE`] so a burst of changes folds into one refresh per
//! person. See `docs/internals/discovery.md`, *Taste events*.

use std::time::Duration;

use bookclerk_library::{
    DiscoverSubject, DomainEventRecord, EventCatalogSubscription, EventDeliveryRecord, JobState,
    JobTrigger, LibraryStore, LISTENING_SYNCED, WISHLIST_CHANGED,
};
use chrono::Utc;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::api::AppState;
use crate::discover_jobs::{enqueue_discover_candidates, enqueue_discover_feed};
use crate::jobs::AdmitJob;

/// Catalog id of Discover's consumer. The `host:` scheme is not a plugin
/// key provenance (`cargo:`, `platform:`, `path:` …) nor a node consumer
/// (`node:`), so it never collides with a plugin's deliveries. Every node
/// registers the same id; the catalog collapses them into one subscriber,
/// so each event is handled once in the cluster.
pub const DISCOVER_CONSUMER_ID: &str = "host:discover";

/// How long a queued refresh waits before it may run, so the changes of
/// one sync or one browsing session fold into a single job per person.
pub const EVENT_DEBOUNCE: Duration = Duration::from_secs(30);

/// Redeliveries before a delivery is dead-lettered. A delivery is retried
/// (after [`EVENT_DEBOUNCE`]) only while a job it needs has already started,
/// so twenty retries cover ten minutes of a slow storefront fetch.
const MAX_RETRIES: u32 = 20;

/// The event types that change what Discover shows someone.
pub const TASTE_EVENT_TYPES: [&str; 3] = [WISHLIST_CHANGED, LISTENING_SYNCED, "book_acquired"];

/// True when `plugin_id` is Discover's consumer.
#[must_use]
pub fn is_discover_consumer_id(plugin_id: &str) -> bool {
    plugin_id == DISCOVER_CONSUMER_ID
}

/// The consumer's catalog subscriptions: schema 1 of each taste event.
#[must_use]
pub fn catalog_subscriptions() -> Vec<EventCatalogSubscription> {
    TASTE_EVENT_TYPES
        .iter()
        .map(|event_type| {
            let mut sub = EventCatalogSubscription::new(*event_type, vec![1]);
            sub.max_retries = Some(MAX_RETRIES);
            sub
        })
        .collect()
}

/// Heartbeats this node's row for Discover's consumer.
///
/// # Arguments
///
/// * `library` - Open library store.
/// * `node_id` - This node's catalog id.
pub async fn upsert_catalog_row(library: &LibraryStore, node_id: &str) {
    if let Err(err) = library
        .upsert_event_subscriber(
            node_id,
            DISCOVER_CONSUMER_ID,
            &catalog_subscriptions(),
            true,
        )
        .await
    {
        warn!(error = %err, "discover consumer catalog upsert failed");
    }
}

/// What a taste event changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Taste {
    /// One person's wishlist (`wishlist_changed`): their seeds, and the
    /// household wish counts everyone's ranking reads.
    Wishlist {
        /// Portal identity that wished, or `None` for an operator wish
        /// (which seeds no one's storefront fetch).
        identity_id: Option<i64>,
    },
    /// Something the whole household reads (`listening_synced`,
    /// `book_acquired`): seeds and owned books for anyone, including the
    /// household fallback.
    Household,
}

/// The part of a `wishlist_changed` payload the consumer reads.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WishlistPayload {
    /// The entry's uuid (required, so a foreign body is refused).
    #[allow(dead_code)] // Read only to validate the payload's shape.
    request_id: String,
    /// The portal identity that wished, when any.
    #[serde(default)]
    identity_id: Option<i64>,
}

/// Parses an event into the taste it changed.
///
/// # Errors
///
/// Returns why the event cannot be handled: a type this consumer does not
/// subscribe to, or a `wishlist_changed` body that is not the host's.
fn parse_taste(event: &DomainEventRecord) -> Result<Taste, String> {
    match event.event_type.as_str() {
        WISHLIST_CHANGED => serde_json::from_str::<WishlistPayload>(&event.payload)
            .map(|p| Taste::Wishlist {
                identity_id: p.identity_id,
            })
            .map_err(|err| format!("bad wishlist_changed payload: {err}")),
        LISTENING_SYNCED | "book_acquired" => Ok(Taste::Household),
        other => Err(format!("not a taste event: {other}")),
    }
}

/// The Discover jobs one event queues.
#[derive(Debug, Default, PartialEq, Eq)]
struct RefreshPlan {
    /// Subjects whose storefront candidates are fetched again (each fetch
    /// then queues its own ranking).
    fetch: Vec<DiscoverSubject>,
    /// Subjects whose stored candidates are only ranked again.
    rank: Vec<DiscoverSubject>,
}

/// Decides which subjects an event refreshes, and how.
///
/// A wish changes its owner's seeds, so the owner's candidates are fetched
/// again; everyone else's shelves read household wish counts, so they are
/// re-ranked. A household change can move anyone's seeds, so every subject
/// with stored shelves fetches again; the shared store-call cache answers
/// the calls that did not change. Apart from the wish owner, a subject with
/// no stored shelves is left to its first read.
///
/// # Arguments
///
/// * `taste` - What the event changed.
/// * `owner` - The wish owner's subject, when the identity still exists.
/// * `with_feeds` - Every subject with stored shelves.
fn plan_refresh(
    taste: Taste,
    owner: Option<DiscoverSubject>,
    with_feeds: Vec<DiscoverSubject>,
) -> RefreshPlan {
    match taste {
        Taste::Wishlist { .. } => {
            let rank = with_feeds
                .into_iter()
                .filter(|subject| Some(subject) != owner.as_ref())
                .collect();
            RefreshPlan {
                fetch: owner.into_iter().collect(),
                rank,
            }
        }
        Taste::Household => RefreshPlan {
            fetch: with_feeds,
            rank: Vec::new(),
        },
    }
}

/// The subject a portal identity's Discover rows are stored under: its
/// user's when it is linked to one, otherwise its own.
///
/// # Errors
///
/// Returns the library error.
async fn identity_subject(
    library: &LibraryStore,
    identity_id: i64,
) -> bookclerk_library::Result<Option<DiscoverSubject>> {
    Ok(library
        .get_portal_identity_by_id(identity_id)
        .await?
        .map(|identity| match identity.user_id {
            Some(user_id) => DiscoverSubject::User(user_id),
            None => DiscoverSubject::Portal(identity.id),
        }))
}

/// What queueing a plan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Queued {
    /// Every job is new, or folded into one that has not started yet.
    Done,
    /// A job the plan needs has already started or finished (it may have read the data
    /// before this change) or the queue is full, so the delivery comes
    /// back after [`EVENT_DEBOUNCE`] to queue it again.
    Later,
}

/// Queues the jobs an event's taste change needs.
///
/// # Errors
///
/// Returns a library or admission error.
async fn queue_for(
    state: &AppState,
    library: &LibraryStore,
    taste: Taste,
) -> anyhow::Result<Queued> {
    let owner = match taste {
        Taste::Wishlist {
            identity_id: Some(id),
        } => identity_subject(library, id).await?,
        Taste::Wishlist { identity_id: None } | Taste::Household => None,
    };
    let with_feeds = library.list_discover_feed_subjects().await?;
    let plan = plan_refresh(taste, owner, with_feeds);
    let mut outcome = Queued::Done;
    // Host-initiated, like the daily refresh: no person asked for it.
    for subject in &plan.fetch {
        let admitted =
            enqueue_discover_candidates(state, subject, JobTrigger::Scheduler, EVENT_DEBOUNCE)
                .await?;
        if needs_retry(library, &admitted).await? {
            outcome = Queued::Later;
        }
    }
    for subject in &plan.rank {
        let admitted =
            enqueue_discover_feed(state, subject, JobTrigger::Scheduler, EVENT_DEBOUNCE).await?;
        if needs_retry(library, &admitted).await? {
            outcome = Queued::Later;
        }
    }
    debug!(
        fetch = plan.fetch.len(),
        rank = plan.rank.len(),
        ?outcome,
        "discover refresh queued for a taste event"
    );
    Ok(outcome)
}

/// True when an admission did not leave a job that will see this change:
/// the queue was full, or the duplicate it folded into is no longer
/// pending (running, finished between admission and this read, or gone),
/// so it may have read its inputs before the change.
///
/// # Errors
///
/// Returns the library error.
async fn needs_retry(library: &LibraryStore, admitted: &AdmitJob) -> anyhow::Result<bool> {
    match admitted {
        AdmitJob::Created(_) => Ok(false),
        AdmitJob::QueueFull => Ok(true),
        AdmitJob::Duplicate(id) => Ok(!library
            .get_job(id)
            .await?
            .is_some_and(|job| job.state == JobState::Pending)),
    }
}

/// Handles one claimed delivery for Discover's consumer: queue the
/// refreshes, then ack; retry later when a needed job has already started;
/// dead-letter a body this consumer cannot read.
///
/// # Arguments
///
/// * `state` - Daemon state (for job admission).
/// * `library` - Open library store.
/// * `event` - The delivered event.
/// * `delivery` - The claimed delivery.
pub async fn run_discover_delivery(
    state: &AppState,
    library: &LibraryStore,
    event: &DomainEventRecord,
    delivery: &EventDeliveryRecord,
) {
    let fence = delivery.fence();
    let taste = match parse_taste(event) {
        Ok(taste) => taste,
        Err(reason) => {
            warn!(event_id = %event.id, %reason, "discover consumer refused an event");
            let _ = library.dead_letter_event_delivery(&fence, &reason).await;
            return;
        }
    };
    let result = match queue_for(state, library, taste).await {
        Ok(Queued::Done) => library.ack_event_delivery(&fence).await,
        Ok(Queued::Later) => {
            let run_after = chrono::Duration::from_std(EVENT_DEBOUNCE)
                .map(|d| Utc::now() + d)
                .unwrap_or_else(|_| Utc::now());
            library
                .retry_event_delivery(
                    &fence,
                    run_after,
                    "a discover job for this change already started",
                )
                .await
        }
        Err(err) => {
            warn!(event_id = %event.id, error = %err, "discover consumer could not queue a refresh");
            library
                .fail_event_delivery(&fence, &format!("{err:#}"))
                .await
        }
    };
    if let Err(err) = result {
        warn!(delivery_id = %delivery.id, error = %err, "discover delivery update failed");
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_library::{
        catalog_subscribers_for_event, JobKind, JobRecord, NewTitleRequest, PublishDomainEventSpec,
        RequestStatus, SeedScope,
    };

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
    fn events_parse_into_the_taste_they_changed() {
        assert_eq!(
            parse_taste(&event(
                WISHLIST_CHANGED,
                r#"{"requestId":"r","identityId":4,"status":"open"}"#
            )),
            Ok(Taste::Wishlist {
                identity_id: Some(4)
            })
        );
        assert_eq!(
            parse_taste(&event(WISHLIST_CHANGED, r#"{"requestId":"r"}"#)),
            Ok(Taste::Wishlist { identity_id: None })
        );
        assert!(parse_taste(&event(WISHLIST_CHANGED, "{}")).is_err());
        assert_eq!(
            parse_taste(&event(LISTENING_SYNCED, "not json")),
            Ok(Taste::Household)
        );
        assert_eq!(
            parse_taste(&event("book_acquired", "{}")),
            Ok(Taste::Household)
        );
        assert!(parse_taste(&event("edition_linked", "{}")).is_err());
    }

    #[test]
    fn a_wish_fetches_for_its_owner_and_reranks_everyone_else() {
        let owner = DiscoverSubject::User(1);
        let others = vec![
            owner.clone(),
            DiscoverSubject::User(2),
            DiscoverSubject::Operator,
        ];
        let wish = Taste::Wishlist {
            identity_id: Some(9),
        };
        assert_eq!(
            plan_refresh(wish, Some(owner.clone()), others.clone()),
            RefreshPlan {
                fetch: vec![owner],
                rank: vec![DiscoverSubject::User(2), DiscoverSubject::Operator],
            }
        );
        // An operator wish (or one from a deleted identity) seeds no one.
        assert_eq!(
            plan_refresh(Taste::Wishlist { identity_id: None }, None, others.clone()).fetch,
            Vec::new()
        );
        assert_eq!(
            plan_refresh(Taste::Household, None, others.clone()),
            RefreshPlan {
                fetch: others,
                rank: Vec::new(),
            }
        );
    }

    #[test]
    fn the_consumer_never_collides_with_a_plugin_or_node() {
        assert!(is_discover_consumer_id(DISCOVER_CONSUMER_ID));
        assert!(!bookclerk_library::control_plane::is_node_consumer_id(
            DISCOVER_CONSUMER_ID
        ));
        assert!(bookclerk_plugin_catalog::PluginKey::parse(DISCOVER_CONSUMER_ID).is_err());
        let subs = catalog_subscriptions();
        let types: Vec<&str> = subs.iter().map(|s| s.event_type.as_str()).collect();
        assert_eq!(types, TASTE_EVENT_TYPES);
        assert!(subs
            .iter()
            .all(|s| s.max_attempts() == Some(MAX_RETRIES + 1)));
    }

    #[tokio::test]
    async fn only_a_pending_duplicate_absorbs_a_change() {
        let files = tempfile::tempdir().expect("tempdir");
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        assert!(
            !needs_retry(&library, &AdmitJob::Created(String::from("j")))
                .await
                .expect("check")
        );
        assert!(needs_retry(&library, &AdmitJob::QueueFull)
            .await
            .expect("check"));
        // A duplicate that finished (or vanished) before the read may have
        // read its inputs before the change.
        assert!(
            needs_retry(&library, &AdmitJob::Duplicate(String::from("gone")))
                .await
                .expect("check")
        );
        let admitted = enqueue_discover_feed(
            &state,
            &DiscoverSubject::Operator,
            JobTrigger::Scheduler,
            EVENT_DEBOUNCE,
        )
        .await
        .expect("admit");
        let AdmitJob::Created(id) = admitted else {
            panic!("expected a new job, got {admitted:?}");
        };
        assert!(!needs_retry(&library, &AdmitJob::Duplicate(id))
            .await
            .expect("check"));
    }

    /// Dispatches the newest outbox row of `event_type` and claims it as
    /// Discover's consumer.
    async fn claim(
        library: &LibraryStore,
        event_type: &str,
    ) -> (DomainEventRecord, EventDeliveryRecord) {
        let event = library
            .list_domain_events_for_queue(bookclerk_library::CORE_EVENTS_QUEUE, 100)
            .await
            .expect("events")
            .into_iter()
            .filter(|e| e.event_type == event_type)
            .max_by_key(|e| e.occurred_at)
            .expect("an event of that type");
        let catalog = library
            .list_live_event_subscribers()
            .await
            .expect("catalog");
        let subscribers = catalog_subscribers_for_event(&catalog, &event);
        assert_eq!(subscribers.len(), 1, "one consumer across the cluster");
        library
            .dispatch_event_deliveries(&event.id, &subscribers, &uuid::Uuid::new_v4().to_string())
            .await
            .expect("dispatch");
        let delivery = library
            .claim_next_event_delivery(
                "test-owner",
                60,
                &uuid::Uuid::new_v4().to_string(),
                &[DISCOVER_CONSUMER_ID.to_string()],
                4,
                "node-a",
            )
            .await
            .expect("claim")
            .expect("a delivery for the discover consumer");
        (event, delivery)
    }

    /// Pending or running Discover jobs.
    async fn discover_jobs(library: &LibraryStore) -> Vec<JobRecord> {
        library
            .list_jobs(100)
            .await
            .expect("jobs")
            .into_iter()
            .filter(|j| matches!(j.kind, JobKind::DiscoverCandidates | JobKind::DiscoverFeed))
            .filter(|j| matches!(j.state, JobState::Pending | JobState::Running))
            .collect()
    }

    #[tokio::test]
    async fn taste_events_queue_debounced_refreshes() {
        let files = tempfile::tempdir().expect("tempdir");
        let (state, library) = crate::node_api::tests::test_state(&files).await;
        // Two nodes register; the catalog keeps one subscriber.
        upsert_catalog_row(&library, "node-a").await;
        upsert_catalog_row(&library, "node-b").await;

        let alice = library
            .create_user(bookclerk_library::UserRole::Member, Some("Alice"), None)
            .await
            .expect("user");
        let bob = library
            .create_user(bookclerk_library::UserRole::Member, Some("Bob"), None)
            .await
            .expect("user");
        let identity = library
            .link_portal_identity_to_user("audiobookshelf", "u1", alice.id, Some("alice"))
            .await
            .expect("identity");
        let owner = DiscoverSubject::User(alice.id);
        let other = DiscoverSubject::User(bob.id);
        for subject in [&owner, &other] {
            library
                .put_discover_feed(subject, "{}", SeedScope::Own, None)
                .await
                .expect("feed");
        }

        // Alice wishes for a book: her candidates are fetched again, Bob's
        // shelves re-ranked, both after the debounce.
        let wish = library
            .create_title_request(&NewTitleRequest {
                uuid: None,
                identity_id: Some(identity.id),
                title: "Hail Mary".into(),
                authors: Some("Andy Weir".into()),
                asin: Some("B00HAIL".into()),
                isbn: None,
                notes: None,
                status: RequestStatus::Open,
                work_key: String::new(),
                work_id: None,
                resolved_book_uuid: None,
                cover_url: None,
            })
            .await
            .expect("wish");
        let (event, delivery) = claim(&library, WISHLIST_CHANGED).await;
        run_discover_delivery(&state, &library, &event, &delivery).await;
        let jobs = discover_jobs(&library).await;
        let mut queued: Vec<(JobKind, String)> = jobs
            .iter()
            .map(|j| (j.kind, j.payload.subject.clone().unwrap_or_default()))
            .collect();
        queued.sort_by_key(|(kind, subject)| (kind.as_str().to_string(), subject.clone()));
        assert_eq!(
            queued,
            vec![
                (JobKind::DiscoverCandidates, owner.key()),
                (JobKind::DiscoverFeed, other.key()),
            ]
        );
        assert!(
            jobs.iter().all(|j| j.run_after > Utc::now()),
            "refreshes wait out the debounce"
        );
        let acked = library
            .get_event_delivery(&delivery.id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(acked.state, "acked");

        // Alice's fetch starts; dropping the wish now must not be lost in
        // the running job, so the delivery comes back later.
        let fetch = jobs
            .iter()
            .find(|j| j.kind == JobKind::DiscoverCandidates)
            .expect("fetch job");
        // Skip the debounce so the worker can claim it now.
        {
            use sea_orm::ConnectionTrait;
            library
                .db()
                .execute_unprepared(&format!(
                    "UPDATE jobs SET run_after = '{}' WHERE id = '{}'",
                    (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339(),
                    fetch.id
                ))
                .await
                .expect("make due");
        }
        library
            .claim_job_by_id(&fetch.id, fetch.lease_generation, "worker", 60)
            .await
            .expect("claim job")
            .expect("claimed");
        library
            .update_title_request_status(&wish.uuid, RequestStatus::Cancelled, None)
            .await
            .expect("cancel");
        let (event, delivery) = claim(&library, WISHLIST_CHANGED).await;
        run_discover_delivery(&state, &library, &event, &delivery).await;
        let retried = library
            .get_event_delivery(&delivery.id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(retried.state, "pending", "{retried:?}");
        assert!(retried.run_after > Utc::now());

        // A household change fetches again for everyone with shelves.
        library
            .publish_domain_event(PublishDomainEventSpec {
                event_type: LISTENING_SYNCED.into(),
                schema_version: 1,
                source: String::from("audiobookshelf"),
                dedup_key: String::from("listening_synced:test"),
                payload: String::from(r#"{"provider":"audiobookshelf","identityIds":[]}"#),
                ..PublishDomainEventSpec::default()
            })
            .await
            .expect("publish");
        let (event, delivery) = claim(&library, LISTENING_SYNCED).await;
        run_discover_delivery(&state, &library, &event, &delivery).await;
        let fetches: Vec<String> = discover_jobs(&library)
            .await
            .into_iter()
            .filter(|j| j.kind == JobKind::DiscoverCandidates)
            .filter_map(|j| j.payload.subject)
            .collect();
        assert!(fetches.contains(&other.key()), "{fetches:?}");
        let state_after = library
            .get_event_delivery(&delivery.id)
            .await
            .expect("read")
            .expect("row")
            .state;
        // Alice's fetch is still running, so this one also comes back.
        assert_eq!(state_after, "pending");

        // A wishlist body that is not the host's is parked, not retried.
        library
            .publish_domain_event(PublishDomainEventSpec {
                event_type: WISHLIST_CHANGED.into(),
                schema_version: 1,
                dedup_key: String::from("wishlist_changed:bogus"),
                payload: String::from("{}"),
                ..PublishDomainEventSpec::default()
            })
            .await
            .expect("publish");
        let (event, delivery) = claim(&library, WISHLIST_CHANGED).await;
        assert_eq!(event.dedup_key, "wishlist_changed:bogus");
        run_discover_delivery(&state, &library, &event, &delivery).await;
        let parked = library
            .get_event_delivery(&delivery.id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(parked.state, "dead_letter");
    }
}
