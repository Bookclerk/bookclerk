//! Reading the shared request queue from its stored ranking.
//!
//! The `discover_request_queue` job ranks the queue against household taste
//! ([`crate::rank_request_queue_taste`]) and stores one row per entry. A
//! read joins those rows with the open requests: titles, covers, wishers and
//! wish counts come from the requests themselves, and the wish-count boost is
//! applied to the live count, so the read never touches the library's books,
//! listening history or an embedder. See `docs/internals/discovery.md`,
//! *Request queue*.

use std::collections::HashMap;

use bookclerk_library::{GlobalQueueEntry, LibraryStore, QueueRank};

use crate::error::Result;
use crate::recommend::RankedQueueEntry;

/// How old the stored ranking may get before a read asks for a new pass,
/// so taste changes no event announces (a book removed, a rating edited)
/// still reach the queue within a day.
pub const REQUEST_QUEUE_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);

/// The key a queue entry's stored ranking is filed under: the library work
/// its wishes are for. Every wish names a work, so one work is one entry
/// whatever identifiers each wisher's store gave.
#[must_use]
pub fn queue_entry_key(entry: &GlobalQueueEntry) -> String {
    entry.work_id.clone()
}

/// The shared request queue as a read builds it.
#[derive(Debug, Clone, Default)]
pub struct StoredRequestQueue {
    /// Open entries the library does not hold, best first.
    pub entries: Vec<RankedQueueEntry>,
    /// True when the stored ranking is missing an entry or older than
    /// [`REQUEST_QUEUE_MAX_AGE`], so the caller should queue a ranking pass.
    pub stale: bool,
}

/// Builds the ranked queue from the open requests and the stored ranking.
///
/// An entry with no stored ranking yet (wished since the last pass) is shown
/// with no taste score, so wish count alone places it until the next pass;
/// an entry the last pass found in the library is hidden.
///
/// # Arguments
///
/// * `library` - Open library store.
///
/// # Errors
///
/// Returns the library read error.
pub async fn read_request_queue(library: &LibraryStore) -> Result<StoredRequestQueue> {
    let queue = library.list_global_request_queue().await?;
    if queue.is_empty() {
        return Ok(StoredRequestQueue::default());
    }
    let stored = library.list_request_queue_ranks().await?;
    let cutoff = chrono::Utc::now() - REQUEST_QUEUE_MAX_AGE;
    let mut stale = stored.iter().any(|row| row.ranked_at < cutoff);
    let ranks: HashMap<String, QueueRank> = stored
        .into_iter()
        .map(|row| (row.rank.entry_key.clone(), row.rank))
        .collect();
    Ok(StoredRequestQueue {
        entries: join_ranks(queue, &ranks, &mut stale),
        stale,
    })
}

/// Joins queue entries with their stored rankings and sorts them.
///
/// # Arguments
///
/// * `queue` - Open queue entries, one per work.
/// * `ranks` - Stored rankings by [`queue_entry_key`].
/// * `stale` - Set when an entry has no stored ranking.
fn join_ranks(
    queue: Vec<GlobalQueueEntry>,
    ranks: &HashMap<String, QueueRank>,
    stale: &mut bool,
) -> Vec<RankedQueueEntry> {
    let mut ranked = Vec::with_capacity(queue.len());
    for entry in queue {
        match ranks.get(&queue_entry_key(&entry)) {
            Some(rank) if rank.owned => {}
            Some(rank) => ranked.push(RankedQueueEntry::from_global(
                entry,
                rank.taste_score,
                rank.reasons.clone(),
            )),
            None => {
                *stale = true;
                ranked.push(RankedQueueEntry::from_global(entry, 0.0, Vec::new()));
            }
        }
    }
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.wish_count.cmp(&a.wish_count))
            .then_with(|| b.last_requested_at.cmp(&a.last_requested_at))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An open queue entry for `title` on work `work_id`, with `wishes` wishes.
    fn entry(work_id: &str, title: &str, wishes: i64) -> GlobalQueueEntry {
        GlobalQueueEntry {
            work_key: format!("soft:{title}"),
            work_id: work_id.into(),
            title: title.into(),
            authors: None,
            asin: None,
            isbn: None,
            cover_url: None,
            description: None,
            subtitle: None,
            narrators: None,
            series: None,
            series_index: None,
            publisher: None,
            length_minutes: None,
            published_at: None,
            genres: None,
            language: None,
            store_editions: Vec::new(),
            purchase_hints: Vec::new(),
            wish_count: wishes,
            wishers: Vec::new(),
            sample_uuids: Vec::new(),
            first_requested_at: chrono::Utc::now(),
            last_requested_at: chrono::Utc::now(),
        }
    }

    /// A stored ranking of `key`.
    fn rank(key: &str, owned: bool, score: f64, reason: &str) -> (String, QueueRank) {
        (
            key.to_string(),
            QueueRank {
                entry_key: key.into(),
                work_id: Some(key.into()),
                owned,
                taste_score: score,
                reasons: vec![reason.into()],
            },
        )
    }

    #[test]
    fn an_entry_keys_by_its_work() {
        assert_eq!(queue_entry_key(&entry("w1", "A", 1)), "w1");
    }

    /// Stored taste orders the queue, the live wish count still dominates,
    /// held titles are hidden and an unranked entry marks the read stale.
    #[test]
    fn the_read_joins_stored_taste_with_live_wishes() {
        let queue = vec![
            entry("w-liked", "Liked", 1),
            entry("w-plain", "Plain", 1),
            entry("w-popular", "Popular", 3),
            entry("w-held", "Held", 5),
        ];
        let ranks: HashMap<String, QueueRank> = [
            rank("w-liked", false, 0.8, "matches liked author (Ada)"),
            rank("w-plain", false, 0.1, "x"),
            rank("w-held", true, 0.0, "x"),
            rank("w-popular", false, 0.0, "y"),
        ]
        .into_iter()
        .collect();
        let mut stale = false;
        let joined = join_ranks(queue.clone(), &ranks, &mut stale);
        let titles: Vec<&str> = joined.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["Popular", "Liked", "Plain"]);
        assert!(!stale);
        assert_eq!(
            joined[1].reasons,
            vec!["matches liked author (Ada)", "on the wishlist"]
        );
        assert_eq!(joined[0].reasons, vec!["y", "wishlisted by 3 people"]);

        let mut stale = false;
        let mut fresh = queue;
        fresh.push(entry("w-new", "New", 1));
        let joined = join_ranks(fresh, &ranks, &mut stale);
        assert!(stale, "an entry wished since the last pass needs one");
        assert!(joined
            .iter()
            .any(|e| e.title == "New" && e.taste_score == 0.0));
    }
}
