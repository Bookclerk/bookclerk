//! Shared helpers for integration listening / progress sync.

use std::collections::BTreeSet;

use bookclerk_library::{BookRecord, LibraryStore, NewListeningProgress, PublishDomainEventSpec};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::types::ListeningProgressSnapshot;

/// Upsert provider-agnostic listening snapshots into `listening_progress`.
///
/// `provider` should be the integration id (`audiobookshelf`, plugin id, …).
/// When anything was written and what the sync says each person has started
/// or finished (see `taste_line`) differs from the provider's previous
/// `listening_synced`, publishes a new one; a sync that changes nothing
/// Discover reads queues no refresh.
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
    let mut taste = BTreeSet::new();
    let mut identities = BTreeSet::new();
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

        let identity_id = item.identity_id.or(identity_id);
        identities.extend(identity_id);
        taste.insert(taste_line(item, identity_id));
        library
            .upsert_listening_progress(&NewListeningProgress {
                identity_id,
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
        upserted += 1;
    }
    if upserted > 0 {
        publish_listening_synced(library, provider, &taste, &identities).await;
    }
    Ok(upserted)
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
/// * `item` - The synced snapshot.
fn listening_stage(item: &ListeningProgressSnapshot) -> u8 {
    if item.is_finished {
        return 5;
    }
    let seconds = item.current_time_seconds.unwrap_or(0.0);
    let from_time = match item.duration_seconds {
        Some(duration) if duration > 0.0 => seconds / duration,
        _ => 0.0,
    };
    let progress = item
        .progress
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

/// One snapshot's part of the sync digest: who, which item, and their
/// [`listening_stage`] (Discover weighs progress, not the exact second).
///
/// # Arguments
///
/// * `item` - The synced snapshot.
/// * `identity_id` - The portal identity the row was stored under, when any.
fn taste_line(item: &ListeningProgressSnapshot, identity_id: Option<i64>) -> String {
    let identity = identity_id.map(|id| id.to_string()).unwrap_or_default();
    format!(
        "{identity}\u{1f}{}\u{1f}{}\u{1f}{}",
        item.external_user_id,
        item.external_item_id,
        listening_stage(item)
    )
}

/// Publishes `listening_synced` for one sync, best effort (the rows are
/// already written; Discover's daily refresh still catches up on failure).
///
/// # Arguments
///
/// * `library` - Open library store.
/// * `provider` - Integration id that synced.
/// * `taste` - The sync's [`taste_line`]s, sorted.
/// * `identities` - Portal identities whose rows the sync wrote.
async fn publish_listening_synced(
    library: &LibraryStore,
    provider: &str,
    taste: &BTreeSet<String>,
    identities: &BTreeSet<i64>,
) {
    let mut digest = Sha256::new();
    for line in taste {
        digest.update(line.as_bytes());
        digest.update(b"\n");
    }
    let digest = hex::encode(digest.finalize());
    // Compare with the provider's last event only, so a state that comes back
    // after a change (finished, unfinished, finished again) is a new event.
    match library
        .latest_domain_event(bookclerk_library::LISTENING_SYNCED, provider)
        .await
    {
        Ok(Some(last)) if last_digest(&last.payload).as_deref() == Some(digest.as_str()) => {
            return;
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(provider, error = %err, "could not read the last listening_synced");
        }
    }
    let payload = serde_json::json!({
        "provider": provider,
        "identityIds": identities.iter().collect::<Vec<_>>(),
        "digest": digest,
    });
    let spec = PublishDomainEventSpec {
        event_type: bookclerk_library::LISTENING_SYNCED.into(),
        schema_version: 1,
        source: provider.to_string(),
        dedup_key: format!(
            "{}:{digest}:{}",
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

/// The `digest` a published `listening_synced` payload carries, when any.
fn last_digest(payload: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .get("digest")?
        .as_str()
        .map(str::to_string)
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
        upsert_listening_snapshots(&library, "audiobookshelf", &[same[0].clone(), again])
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
    }

    #[test]
    fn stages_follow_what_engagement_reads() {
        let stage = |progress: Option<f64>, seconds: Option<f64>, duration: Option<f64>| {
            listening_stage(&ListeningProgressSnapshot {
                progress,
                current_time_seconds: seconds,
                duration_seconds: duration,
                ..ListeningProgressSnapshot::default()
            })
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
        let mut finished = snapshot("u", "i", 0.0);
        finished.is_finished = true;
        assert_eq!(listening_stage(&finished), 5);
        assert!(taste_line(&finished, Some(7)).starts_with("7\u{1f}u\u{1f}i\u{1f}5"));
        assert!(taste_line(&finished, None).starts_with('\u{1f}'));
        assert_eq!(last_digest(r#"{"digest":"ab"}"#).as_deref(), Some("ab"));
        assert_eq!(last_digest("{}"), None);
        assert_eq!(last_digest("not json"), None);
    }
}
