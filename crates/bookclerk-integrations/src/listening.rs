//! Shared helpers for integration listening / progress sync.

use std::collections::BTreeSet;

use bookclerk_library::{BookRecord, LibraryStore, NewListeningProgress, PublishDomainEventSpec};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::types::ListeningProgressSnapshot;

/// Upsert provider-agnostic listening snapshots into `listening_progress`.
///
/// `provider` should be the integration id (`audiobookshelf`, plugin id, …).
/// When anything was written, publishes `listening_synced` keyed by a digest
/// of what the sync says each person has started or finished (see
/// `taste_line`), so a sync that changes nothing Discover reads coalesces
/// into the previous event instead of queueing another refresh.
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

/// One snapshot's part of the sync digest: who, which item, and how far
/// they got, in quarters (Discover weighs progress, not the exact second).
///
/// # Arguments
///
/// * `item` - The synced snapshot.
/// * `identity_id` - The portal identity the row was stored under, when any.
fn taste_line(item: &ListeningProgressSnapshot, identity_id: Option<i64>) -> String {
    let progress = item.progress.unwrap_or(0.0);
    let quarter = if item.is_finished || progress >= 1.0 {
        4
    } else if progress >= 0.75 {
        3
    } else if progress >= 0.5 {
        2
    } else if progress >= 0.25 {
        1
    } else {
        0
    };
    let identity = identity_id.map(|id| id.to_string()).unwrap_or_default();
    format!(
        "{identity}\u{1f}{}\u{1f}{}\u{1f}{quarter}",
        item.external_user_id, item.external_item_id
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
    let payload = serde_json::json!({
        "provider": provider,
        "identityIds": identities.iter().collect::<Vec<_>>(),
    });
    let spec = PublishDomainEventSpec {
        event_type: bookclerk_library::LISTENING_SYNCED.into(),
        schema_version: 1,
        source: provider.to_string(),
        dedup_key: format!("{}:{digest}", bookclerk_library::LISTENING_SYNCED),
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
    }

    #[test]
    fn progress_is_counted_in_quarters() {
        let quarter = |p: f64| {
            taste_line(&snapshot("u", "i", p), Some(7))
                .rsplit('\u{1f}')
                .next()
                .map(str::to_string)
        };
        assert_eq!(quarter(0.0).as_deref(), Some("0"));
        assert_eq!(quarter(0.3).as_deref(), Some("1"));
        assert_eq!(quarter(0.5).as_deref(), Some("2"));
        assert_eq!(quarter(0.8).as_deref(), Some("3"));
        assert_eq!(quarter(1.0).as_deref(), Some("4"));
        assert!(taste_line(&snapshot("u", "i", 0.0), Some(7)).starts_with("7\u{1f}"));
        assert!(taste_line(&snapshot("u", "i", 0.0), None).starts_with('\u{1f}'));
    }
}
