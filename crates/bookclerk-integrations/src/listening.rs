//! Shared helpers for integration listening / progress sync.

use std::collections::BTreeSet;

use bookclerk_library::{
    LibraryStore, ListeningIdentifiers, ListeningProgressRecord, NewListeningProgress,
    PublishDomainEventSpec,
};

use crate::error::Result;
use crate::types::ListeningProgressSnapshot;

/// Upsert provider-agnostic listening snapshots into `listening_progress`.
///
/// `provider` should be the integration id (`audiobookshelf`, plugin id, …).
/// The whole sync costs a few bulk statements, not a round trip chain per
/// item: portal identities and stored rows are read in chunks, each item's
/// ASIN and ISBN resolve to a work through the indexed `edition_identifiers`
/// table (titles are never compared), and the rows are written as atomic
/// batches sized to the database plugin's limits
/// ([`LibraryStore::upsert_listening_progress_batch`]).
///
/// A row is keyed by the work its identifiers resolved to. An item whose
/// identifiers name no work in the library is not stored: there is no work
/// to hold its progress, and the next sync stores it once the library has
/// one.
///
/// When the sync changes what Discover reads from any stored row (see
/// `TasteState`: who the row belongs to, which work it is about, and how far
/// they got), publishes one `listening_synced`; a sync that changes nothing
/// Discover reads queues no refresh.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `provider` - Integration id.
/// * `items` - Snapshots the integration reported, in its order.
///
/// # Returns
///
/// How many snapshots were stored: the items whose identifiers resolved
/// to a work (reports of one row merge).
///
/// # Errors
///
/// Returns an error when a store read or write fails. Batches written
/// before the failure stay, so a failed write still publishes
/// `listening_synced` (naming no identity: everyone refreshes).
pub async fn upsert_listening_snapshots(
    library: &LibraryStore,
    provider: &str,
    items: &[ListeningProgressSnapshot],
) -> Result<usize> {
    if items.is_empty() {
        return Ok(0);
    }
    let users: BTreeSet<String> = items
        .iter()
        .filter(|item| item.identity_id.is_none())
        .map(|item| item.external_user_id.clone())
        .collect();
    let identities = library.portal_identity_ids(provider, &users).await?;
    let keys: Vec<ListeningIdentifiers> = items
        .iter()
        .map(|item| ListeningIdentifiers {
            asin: item.asin.clone(),
            isbn: item.isbn.clone(),
        })
        .collect();
    let matches = library.match_listening_items(&keys).await?;
    let rows: Vec<NewListeningProgress> = items
        .iter()
        .zip(matches)
        .filter_map(|(item, matched)| {
            Some(NewListeningProgress {
                identity_id: item
                    .identity_id
                    .or_else(|| identities.get(&item.external_user_id).copied()),
                provider: provider.into(),
                external_user_id: item.external_user_id.clone(),
                work_id: matched.work_id?,
                edition_id: matched.edition_id,
                external_item_id: item.external_item_id.clone(),
                progress: item.progress,
                current_time_seconds: item.current_time_seconds,
                duration_seconds: item.duration_seconds,
                is_finished: item.is_finished,
                last_listened_at: item.last_listened_at,
            })
        })
        .collect();
    if rows.is_empty() {
        return Ok(0);
    }
    let writes = match library.upsert_listening_progress_batch(&rows).await {
        Ok(writes) => writes,
        Err(err) => {
            // Batches before the failing one stay written, and the next sync
            // would see those rows as unchanged: tell Discover now, for
            // everyone, rather than lose the change.
            publish_listening_synced(library, provider, &BTreeSet::new()).await;
            return Err(err.into());
        }
    };

    let mut identities = BTreeSet::new();
    let mut changed = false;
    for write in &writes {
        let before = write.before.as_ref().map(TasteState::of);
        let after = TasteState::of_written(&write.after);
        if before.as_ref() != Some(&after) {
            changed = true;
            identities.extend(after.identity_id);
            identities.extend(before.as_ref().and_then(|b| b.identity_id));
        }
    }
    if changed {
        publish_listening_synced(library, provider, &identities).await;
    }
    Ok(rows.len())
}

/// What Discover reads from one stored listening row: whose it is, which
/// work it is about (a seed, whose credits weigh liked authors), and how far
/// they got ([`listening_stage`]; Discover weighs progress, not the exact
/// second).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TasteState {
    /// Portal identity the row is stored under, when any.
    identity_id: Option<i64>,
    /// Work the row is about.
    work_id: String,
    /// [`listening_stage`] of the stored progress.
    stage: u8,
}

impl TasteState {
    /// The taste-relevant part of a row as it was just written.
    fn of_written(row: &NewListeningProgress) -> Self {
        Self {
            identity_id: row.identity_id,
            work_id: row.work_id.clone(),
            stage: listening_stage(
                row.progress,
                row.current_time_seconds,
                row.duration_seconds,
                row.is_finished,
            ),
        }
    }

    /// The taste-relevant part of a stored row.
    fn of(row: &ListeningProgressRecord) -> Self {
        Self {
            identity_id: row.identity_id,
            work_id: row.work_id.clone(),
            stage: listening_stage(
                row.progress,
                row.current_time_seconds,
                row.duration_seconds,
                row.is_finished,
            ),
        }
    }
}

/// How far someone got in one item, as Discover's engagement reads it:
/// `0` not started, `1`–`4` the quarter they are in (full playback not yet
/// marked finished is the fourth), `5` finished.
///
/// Progress comes from the reported fraction, or from playback position
/// over duration when the fraction is missing; a position with no duration
/// counts as started.
///
/// # Arguments
///
/// * `progress` - Reported fraction in `0.0..=1.0`, when any.
/// * `current_time_seconds` - Playback position in seconds, when any.
/// * `duration_seconds` - Item length in seconds, when any.
/// * `is_finished` - Whether the provider marked the item finished.
fn listening_stage(
    progress: Option<f64>,
    current_time_seconds: Option<f64>,
    duration_seconds: Option<f64>,
    is_finished: bool,
) -> u8 {
    if is_finished {
        return 5;
    }
    let seconds = current_time_seconds.unwrap_or(0.0);
    let from_time = match duration_seconds {
        Some(duration) if duration > 0.0 => seconds / duration,
        _ => 0.0,
    };
    let progress = progress
        .filter(|p| *p > 0.0)
        .unwrap_or(from_time)
        .clamp(0.0, 1.0);
    // Full playback without the finished flag stays in the last quarter:
    // Discover treats an explicitly finished book differently.
    if progress >= 0.75 {
        4
    } else if progress >= 0.5 {
        3
    } else if progress >= 0.25 {
        2
    } else if progress > 0.0 || seconds > 0.0 {
        1
    } else {
        0
    }
}

/// Publishes `listening_synced` for one sync, best effort (the rows are
/// already written; Discover's daily refresh still catches up on failure).
///
/// Every publication is a new event (the dedup key carries a fresh UUID):
/// the caller only calls this when a stored row changed.
///
/// # Arguments
///
/// * `library` - Open library store.
/// * `provider` - Integration id that synced.
/// * `identities` - Portal identities whose rows changed, before or after.
async fn publish_listening_synced(
    library: &LibraryStore,
    provider: &str,
    identities: &BTreeSet<i64>,
) {
    let payload = serde_json::json!({
        "provider": provider,
        "identityIds": identities.iter().collect::<Vec<_>>(),
    });
    let spec = PublishDomainEventSpec {
        event_type: bookclerk_library::LISTENING_SYNCED.into(),
        schema_version: 1,
        source: provider.to_string(),
        dedup_key: format!(
            "{}:{provider}:{}",
            bookclerk_library::LISTENING_SYNCED,
            uuid::Uuid::new_v4()
        ),
        payload: payload.to_string(),
        ordering_key: provider.to_string(),
        ..PublishDomainEventSpec::default()
    };
    if let Err(err) = library.publish_domain_event(spec).await {
        tracing::warn!(provider, error = %err, "could not publish listening_synced");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ASIN of the library title `item` names in these tests.
    fn asin_of(item: &str) -> String {
        format!("B00ITEM{:0>3}", item.to_ascii_uppercase())
    }

    /// One synced row for `user` on `item` at `progress`, naming the
    /// library title [`library`] holds for `item` by its ASIN.
    fn snapshot(user: &str, item: &str, progress: f64) -> ListeningProgressSnapshot {
        ListeningProgressSnapshot {
            external_user_id: user.into(),
            external_item_id: item.into(),
            title: Some(format!("Title {item}")),
            asin: Some(asin_of(item)),
            progress: Some(progress),
            ..ListeningProgressSnapshot::default()
        }
    }

    /// A memory library holding one title per item in `items`.
    async fn library(items: &[&str]) -> LibraryStore {
        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        );
        library
            .upsert_account("audible", "a1", "us", None, true, "audible")
            .await
            .expect("account");
        for item in items {
            library
                .upsert_book(&bookclerk_library::NewBook::minimal(
                    asin_of(item),
                    "a1",
                    "US",
                    format!("Title {item}"),
                ))
                .await
                .expect("book");
        }
        library
    }

    /// `listening_synced` rows in the outbox.
    async fn synced_events(library: &LibraryStore) -> Vec<bookclerk_library::DomainEventRecord> {
        library
            .list_domain_events_for_queue(bookclerk_library::CORE_EVENTS_QUEUE, 50)
            .await
            .expect("events")
            .into_iter()
            .filter(|e| e.event_type == bookclerk_library::LISTENING_SYNCED)
            .collect()
    }

    #[tokio::test]
    async fn a_sync_publishes_only_when_what_people_read_changed() {
        let library = library(&["a", "b", "c"]).await;
        assert_eq!(
            upsert_listening_snapshots(&library, "audiobookshelf", &[])
                .await
                .expect("empty sync"),
            0
        );
        assert!(synced_events(&library).await.is_empty(), "nothing written");

        let first = [snapshot("u1", "a", 0.1), snapshot("u2", "b", 0.6)];
        upsert_listening_snapshots(&library, "audiobookshelf", &first)
            .await
            .expect("sync");
        let events = synced_events(&library).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "audiobookshelf");
        let payload: serde_json::Value = serde_json::from_str(&events[0].payload).expect("payload");
        assert_eq!(payload["provider"], "audiobookshelf");

        // A few more minutes in the same quarter is not a taste change.
        let same = [snapshot("u1", "a", 0.2), snapshot("u2", "b", 0.7)];
        upsert_listening_snapshots(&library, "audiobookshelf", &same)
            .await
            .expect("sync");
        assert_eq!(synced_events(&library).await.len(), 1, "coalesced");

        // Finishing a book is.
        let mut finished = snapshot("u2", "b", 0.7);
        finished.is_finished = true;
        upsert_listening_snapshots(&library, "audiobookshelf", &[same[0].clone(), finished])
            .await
            .expect("sync");
        assert_eq!(synced_events(&library).await.len(), 2);

        // Unfinishing it and finishing it again are two more changes, even
        // though the last state matches an earlier event.
        upsert_listening_snapshots(&library, "audiobookshelf", &same)
            .await
            .expect("sync");
        assert_eq!(synced_events(&library).await.len(), 3);
        let mut again = snapshot("u2", "b", 0.7);
        again.is_finished = true;
        let current = [same[0].clone(), again];
        upsert_listening_snapshots(&library, "audiobookshelf", &current)
            .await
            .expect("sync");
        assert_eq!(synced_events(&library).await.len(), 4);

        // Starting a book someone had not started is a change too.
        let unstarted = [snapshot("u3", "c", 0.0)];
        upsert_listening_snapshots(&library, "other", &unstarted)
            .await
            .expect("sync");
        upsert_listening_snapshots(&library, "other", &[snapshot("u3", "c", 0.05)])
            .await
            .expect("sync");
        assert_eq!(synced_events(&library).await.len(), 6);

        // A sync that reports only some people (alternating subsets) changes
        // nothing stored, so it publishes nothing.
        for subset in [&current[..1], &current[1..], &current[..1]] {
            upsert_listening_snapshots(&library, "audiobookshelf", subset)
                .await
                .expect("sync");
        }
        assert_eq!(synced_events(&library).await.len(), 6, "subsets coalesce");

        // Two reports of one row in one sync compare first-before to
        // last-after: back where it started is no change.
        let jitter = [snapshot("u1", "a", 0.9), snapshot("u1", "a", 0.2)];
        upsert_listening_snapshots(&library, "audiobookshelf", &jitter)
            .await
            .expect("sync");
        assert_eq!(synced_events(&library).await.len(), 6, "net unchanged");

        // A row that gains an identity is a change for that person, and the
        // event names whose rows changed.
        let dana = library
            .upsert_portal_identity("audiobookshelf", "u1", Some("Dana"))
            .await
            .expect("identity");
        upsert_listening_snapshots(&library, "audiobookshelf", &same[..1])
            .await
            .expect("sync");
        let events = synced_events(&library).await;
        assert_eq!(events.len(), 7);
        let named: Vec<serde_json::Value> = events
            .iter()
            .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).expect("payload"))
            .map(|p| p["identityIds"].clone())
            .filter(|ids| ids.as_array().is_some_and(|a| !a.is_empty()))
            .collect();
        assert_eq!(named, vec![serde_json::json!([dana.id])]);

        // The same item now naming another work is a change: Discover
        // seeds from the work.
        let mut moved = same[0].clone();
        moved.asin = Some(asin_of("c"));
        upsert_listening_snapshots(&library, "audiobookshelf", &[moved])
            .await
            .expect("sync");
        assert_eq!(synced_events(&library).await.len(), 8);
    }

    #[tokio::test]
    async fn items_are_stored_by_identifier_and_never_by_title() {
        let library = library(&["a"]).await;
        let dune = library
            .upsert_book(&bookclerk_library::NewBook::minimal(
                "B00DUNE001",
                "a1",
                "US",
                "Dune",
            ))
            .await
            .expect("book");
        let work = library
            .work_id_for_book(&dune.uuid)
            .await
            .expect("work")
            .expect("a work");
        let dana = library
            .upsert_portal_identity("audiobookshelf", "u1", Some("Dana"))
            .await
            .expect("identity");

        let mut by_asin = snapshot("u1", "a", 0.3);
        by_asin.asin = Some("b00dune001".into());
        // Same title, no identifiers: titles are never compared, and an
        // item that names no work is not stored.
        let mut by_title = snapshot("u2", "b", 0.3);
        by_title.title = Some("Dune".into());
        by_title.asin = None;
        let stored = upsert_listening_snapshots(&library, "audiobookshelf", &[by_asin, by_title])
            .await
            .expect("sync");
        assert_eq!(stored, 1);

        let a = library
            .get_listening_progress("audiobookshelf", "u1", "a")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(a.work_id, work);
        assert!(a.edition_id.is_some(), "one edition carries the ASIN");
        assert_eq!(a.identity_id, Some(dana.id));
        assert!(library
            .get_listening_progress("audiobookshelf", "u2", "b")
            .await
            .expect("read")
            .is_none());

        // A sync where nothing resolves stores and publishes nothing.
        let before = synced_events(&library).await.len();
        let mut unknown = snapshot("u2", "b", 0.5);
        unknown.asin = Some("B0UNKNOWN1".into());
        assert_eq!(
            upsert_listening_snapshots(&library, "audiobookshelf", &[unknown])
                .await
                .expect("sync"),
            0
        );
        assert_eq!(synced_events(&library).await.len(), before);
    }

    #[tokio::test]
    async fn a_failed_write_still_tells_discover() {
        let library = library(&["a"]).await;
        let cap = usize::try_from(library.db_capabilities().max_request_bytes).expect("cap");
        let mut huge = snapshot("u1", "a", 0.3);
        huge.external_item_id = "x".repeat(cap + 1);
        huge.asin = Some(asin_of("a"));
        assert!(
            upsert_listening_snapshots(&library, "audiobookshelf", &[huge])
                .await
                .is_err()
        );
        let events = synced_events(&library).await;
        assert_eq!(events.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&events[0].payload).expect("payload");
        assert_eq!(payload["identityIds"], serde_json::json!([]));
    }

    #[test]
    fn stages_follow_what_engagement_reads() {
        let stage = |progress: Option<f64>, seconds: Option<f64>, duration: Option<f64>| {
            listening_stage(progress, seconds, duration, false)
        };
        assert_eq!(stage(None, None, None), 0);
        assert_eq!(stage(Some(0.0), Some(0.0), Some(100.0)), 0);
        assert_eq!(stage(Some(0.1), None, None), 1, "started is not unstarted");
        assert_eq!(
            stage(None, Some(30.0), None),
            1,
            "a position alone is started"
        );
        assert_eq!(
            stage(None, Some(30.0), Some(100.0)),
            2,
            "fraction from time"
        );
        assert_eq!(stage(Some(0.5), None, None), 3);
        assert_eq!(stage(Some(0.8), None, None), 4);
        assert_eq!(stage(Some(1.0), None, None), 4, "only the flag finishes");
        assert_eq!(stage(None, Some(100.0), Some(100.0)), 4);
        assert_eq!(listening_stage(Some(0.0), None, None, true), 5);
    }
}
