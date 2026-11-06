//! Shared helpers for integration listening / progress sync.

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_library::{
    BookRecord, LibraryStore, ListeningProgressRecord, NewListeningProgress, PublishDomainEventSpec,
};

use crate::error::Result;
use crate::types::ListeningProgressSnapshot;

/// Upsert provider-agnostic listening snapshots into `listening_progress`.
///
/// `provider` should be the integration id (`audiobookshelf`, plugin id, …).
/// When the sync changes what Discover reads from any stored row (see
/// `TasteState`: who the row belongs to, which owned book and work it matched,
/// its authors, and how far they got), publishes one `listening_synced`; a sync that changes
/// nothing Discover reads queues no refresh.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `provider` - Integration id.
/// * `items` - `items` input for this call.
///
/// # Returns
///
/// On success, the inner `usize` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn upsert_listening_snapshots(
    library: &LibraryStore,
    provider: &str,
    items: &[ListeningProgressSnapshot],
) -> Result<usize> {
    // Load once per sync — matching is O(items × books) over in-memory rows,
    // not O(items) full-table DB reads.
    let books = library.list_books(None).await?;
    let mut upserted = 0usize;
    // Per stored row: its state before this sync (first sighting) and after
    // (last write), so a row the sync reports twice compares end to end.
    let mut rows: BTreeMap<(String, String), (Option<TasteState>, TasteState)> = BTreeMap::new();
    for item in items {
        let identity_id = library
            .get_portal_identity(provider, &item.external_user_id)
            .await
            .ok()
            .flatten()
            .map(|i| i.id);

        let book_uuid = match_book_uuid_in(
            &books,
            item.asin.as_deref(),
            item.isbn.as_deref(),
            item.title.as_deref(),
        );
        let work_id = if let Some(ref uuid) = book_uuid {
            library.work_id_for_book(uuid).await.ok().flatten()
        } else {
            None
        };

        let before = library
            .get_listening_progress(provider, &item.external_user_id, &item.external_item_id)
            .await?
            .as_ref()
            .map(TasteState::of);
        let stored = library
            .upsert_listening_progress(&NewListeningProgress {
                identity_id: item.identity_id.or(identity_id),
                provider: provider.into(),
                external_user_id: item.external_user_id.clone(),
                book_uuid,
                work_id,
                external_item_id: item.external_item_id.clone(),
                title: item.title.clone(),
                authors: item.authors.clone(),
                asin: item.asin.clone(),
                isbn: item.isbn.clone(),
                progress: item.progress,
                current_time_seconds: item.current_time_seconds,
                duration_seconds: item.duration_seconds,
                is_finished: item.is_finished,
                last_listened_at: item.last_listened_at,
            })
            .await?;
        let after = TasteState::of(&stored);
        rows.entry((stored.external_user_id, stored.external_item_id))
            .and_modify(|(_, last)| *last = after.clone())
            .or_insert((before, after));
        upserted += 1;
    }
    let mut identities = BTreeSet::new();
    let mut changed = false;
    for (before, after) in rows.values() {
        if before.as_ref() != Some(after) {
            changed = true;
            identities.extend(after.identity_id);
            identities.extend(before.as_ref().and_then(|b| b.identity_id));
        }
    }
    if changed {
        publish_listening_synced(library, provider, &identities).await;
    }
    Ok(upserted)
}

/// What Discover reads from one stored listening row: whose it is, which
/// owned book and work it matched (seeds), its authors, and how far they got
/// ([`listening_stage`]; Discover weighs progress, not the exact second).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TasteState {
    /// Portal identity the row is stored under, when any.
    identity_id: Option<i64>,
    /// Owned book the row matched, when any.
    book_uuid: Option<String>,
    /// Work the row resolved to (an embedding seed), when any.
    work_id: Option<String>,
    /// Authors the sync reported (they weigh liked authors), when any.
    authors: Option<String>,
    /// [`listening_stage`] of the stored progress.
    stage: u8,
}

impl TasteState {
    /// The taste-relevant part of a stored row.
    fn of(row: &ListeningProgressRecord) -> Self {
        Self {
            identity_id: row.identity_id,
            book_uuid: row.book_uuid.clone(),
            work_id: row.work_id.clone(),
            authors: row.authors.clone(),
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

/// Best-effort match to an owned library book via ASIN, ISBN, or exact title.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `isbn` - Optional ISBN (any punctuation; normalized internally).
/// * `title` - Display title.
///
/// # Returns
///
/// On success, the inner `Option<String>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn match_book_uuid(
    library: &LibraryStore,
    asin: Option<&str>,
    isbn: Option<&str>,
    title: Option<&str>,
) -> Result<Option<String>> {
    let books = library.list_books(None).await?;
    Ok(match_book_uuid_in(&books, asin, isbn, title))
}

/// Matches a snapshot to an owned book by ASIN, then ISBN, then exact title (case-insensitive).
fn match_book_uuid_in(
    books: &[BookRecord],
    asin: Option<&str>,
    isbn: Option<&str>,
    title: Option<&str>,
) -> Option<String> {
    if let Some(asin) = asin {
        if let Some(b) = books.iter().find(|b| {
            b.asin
                .as_deref()
                .map(|a| a.eq_ignore_ascii_case(asin))
                .unwrap_or(false)
        }) {
            return Some(b.uuid.clone());
        }
    }
    if let Some(isbn) = isbn {
        if let Some(b) = books.iter().find(|b| b.isbn.as_deref() == Some(isbn)) {
            return Some(b.uuid.clone());
        }
    }
    if let Some(title) = title {
        let title_l = title.to_lowercase();
        if let Some(b) = books.iter().find(|b| b.title.to_lowercase() == title_l) {
            return Some(b.uuid.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One synced row for `user` on `item` at `progress`.
    fn snapshot(user: &str, item: &str, progress: f64) -> ListeningProgressSnapshot {
        ListeningProgressSnapshot {
            external_user_id: user.into(),
            external_item_id: item.into(),
            title: Some(format!("Title {item}")),
            progress: Some(progress),
            ..ListeningProgressSnapshot::default()
        }
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
        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        );
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

        // Authors filled in later feed liked authors, so they are a change.
        let mut credited = same[0].clone();
        credited.authors = Some("Ursula K. Le Guin".into());
        upsert_listening_snapshots(&library, "audiobookshelf", &[credited])
            .await
            .expect("sync");
        assert_eq!(synced_events(&library).await.len(), 8);
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
