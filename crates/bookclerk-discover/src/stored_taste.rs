//! The stored form of one subject's Discover taste.
//!
//! The candidate fetch reads the library once to load a subject's taste
//! ([`crate::precompute::load_subject_taste`]) and stores what the ranking
//! needs from it in one `discover_tastes` cell: the reader's languages,
//! the liked authors, narrators, categories and series
//! ([`TasteScores`]), the seed works whose embeddings make the centroid,
//! and the shelf context ([`ShelfTaste`]). A ranking then reads that cell
//! instead of the whole library, so its cost follows the subject's
//! candidates, not the library's size.
//!
//! The cell is capped like a stored feed: the database plugins refuse a
//! cell or result over 256 KiB, so a taste whose JSON would be larger
//! keeps only its heaviest entries ([`StoredTaste::encode`]). Entries that
//! are cut carry the least weight, so they move a score the least.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};

use crate::precompute::SubjectTaste;
use crate::recommend::{build_taste_scores, reader_languages, shelf_taste, TasteScores};
use crate::shelves::ShelfTaste;

/// Format of the stored taste this release writes and reads.
///
/// A stored taste with another format does not decode, counts as missing
/// and is loaded from the library again.
pub const STORED_TASTE_FORMAT: u32 = 1;

/// Most bytes a stored taste's JSON may take once escaped as a JSON string.
///
/// Like [`crate::precompute::STORED_FEED_MAX_BYTES`], this stays under the
/// first-party plugins' 256 KiB cell and result cap with room for the row's
/// other columns.
pub const STORED_TASTE_MAX_BYTES: usize = 128 * 1024;

/// How long a stored taste is used before a ranking loads it again.
///
/// Every taste event (a wish, a listening sync, an acquire, a holding
/// added or removed) fetches the subjects it affects again, and each fetch
/// stores a fresh taste. Finishes, ratings and tags send no event, so a
/// taste older than this is loaded again to pick them up.
#[must_use]
pub fn stored_taste_max_age() -> Duration {
    Duration::hours(24)
}

/// What a ranking needs from one subject's taste.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredTaste {
    /// [`STORED_TASTE_FORMAT`] when written.
    format: u32,
    /// Normalized codes of the languages the reader listens in.
    pub(crate) languages: BTreeSet<String>,
    /// Liked authors, narrators, categories and series, and the seed works.
    pub(crate) scores: TasteScores,
    /// Shelf titles and placement context, wish titles and linked stores
    /// included.
    pub(crate) shelf: ShelfTaste,
}

impl StoredTaste {
    /// The stored form of `taste`.
    pub(crate) fn of(taste: &SubjectTaste) -> Self {
        let mut shelf = shelf_taste(&taste.books, &taste.weights, &taste.listening);
        shelf.wish_titles.clone_from(&taste.wish_titles);
        shelf.linked_sources.clone_from(&taste.linked_sources);
        Self {
            format: STORED_TASTE_FORMAT,
            languages: reader_languages(&taste.books).into_iter().collect(),
            scores: build_taste_scores(&taste.books, &taste.weights, &taste.listening),
            shelf,
        }
    }

    /// The reader's languages, as ranking filters them.
    pub(crate) fn language_set(&self) -> HashSet<String> {
        self.languages.iter().cloned().collect()
    }

    /// Encodes the taste as JSON no larger than `max_bytes` once escaped as
    /// a JSON string, cutting every map and set to its heaviest entries,
    /// halving the count kept until it fits.
    ///
    /// # Errors
    ///
    /// Returns the encoder's error.
    pub(crate) fn encode(&mut self, max_bytes: usize) -> serde_json::Result<String> {
        loop {
            let json = serde_json::to_string(self)?;
            let escaped = serde_json::to_string(&json)?.len();
            let largest = self.largest_part();
            if escaped <= max_bytes || largest == 0 {
                return Ok(json);
            }
            self.keep_heaviest(largest / 2);
        }
    }

    /// Decodes a stored taste, or `None` when it is not this release's
    /// format.
    pub(crate) fn decode(json: &str) -> Option<Self> {
        serde_json::from_str::<Self>(json)
            .ok()
            .filter(|t| t.format == STORED_TASTE_FORMAT)
    }

    /// Entries in the largest cuttable map or set.
    fn largest_part(&self) -> usize {
        [
            self.scores.liked_authors.len(),
            self.scores.liked_narrators.len(),
            self.scores.liked_categories.len(),
            self.scores.series_affinity.len(),
            self.scores.seed_work_ids.len(),
            self.shelf.authors.len(),
            self.shelf.narrators.len(),
            self.shelf.categories.len(),
            self.shelf.seed_authors_by_title.len(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }

    /// Keeps at most `keep` entries of every cuttable map and set: the
    /// heaviest by weight (series by owned and listened titles), ties and
    /// unweighted entries by key.
    fn keep_heaviest(&mut self, keep: usize) {
        keep_top(&mut self.scores.liked_authors, keep, |w| *w);
        keep_top(&mut self.scores.liked_narrators, keep, |w| *w);
        keep_top(&mut self.scores.liked_categories, keep, |w| *w);
        keep_top(&mut self.scores.series_affinity, keep, |a| {
            f64::from(u32::try_from(a.owned_count).unwrap_or(u32::MAX)) + a.listen_engagement_sum
        });
        keep_top(&mut self.shelf.authors, keep, |(_, w)| *w);
        keep_top(&mut self.shelf.narrators, keep, |(_, w)| *w);
        keep_top(&mut self.shelf.categories, keep, |(_, w)| *w);
        keep_top(&mut self.shelf.seed_authors_by_title, keep, |_| 0.0);
        self.scores.seed_work_ids = std::mem::take(&mut self.scores.seed_work_ids)
            .into_iter()
            .take(keep)
            .collect();
    }
}

/// Keeps the `keep` entries of `map` with the highest `weight`, ties by key.
fn keep_top<V>(map: &mut HashMap<String, V>, keep: usize, weight: impl Fn(&V) -> f64) {
    if map.len() <= keep {
        return;
    }
    let mut entries: Vec<(String, V)> = std::mem::take(map).into_iter().collect();
    entries
        .sort_by(|(ka, va), (kb, vb)| weight(vb).total_cmp(&weight(va)).then_with(|| ka.cmp(kb)));
    entries.truncate(keep);
    map.extend(entries);
}

/// Whether a taste computed at `computed_at` is still used at `now`.
pub(crate) fn is_fresh(computed_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(computed_at) < stored_taste_max_age()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A taste with `n` liked authors weighted by index, and the same
    /// shelf authors.
    fn wide(n: usize) -> StoredTaste {
        let mut taste = StoredTaste {
            format: STORED_TASTE_FORMAT,
            ..Default::default()
        };
        for i in 0..n {
            let name = format!("author number {i:06} with a long name");
            let weight = f64::from(u32::try_from(i).unwrap());
            taste.scores.liked_authors.insert(name.clone(), weight);
            taste.shelf.authors.insert(name.clone(), (name, weight));
            taste.scores.seed_work_ids.insert(format!("work-{i:06}"));
        }
        taste.languages.insert("en".into());
        taste
    }

    #[test]
    fn a_small_taste_round_trips_whole() {
        let mut taste = wide(10);
        let json = taste.encode(STORED_TASTE_MAX_BYTES).unwrap();
        assert_eq!(StoredTaste::decode(&json), Some(wide(10)));
    }

    #[test]
    fn a_large_taste_keeps_its_heaviest_entries_within_the_budget() {
        let mut taste = wide(10_000);
        let json = taste.encode(STORED_TASTE_MAX_BYTES).unwrap();
        assert!(serde_json::to_string(&json).unwrap().len() <= STORED_TASTE_MAX_BYTES);
        let back = StoredTaste::decode(&json).unwrap();
        assert!(back.scores.liked_authors.len() < 10_000);
        assert!(!back.scores.liked_authors.is_empty());
        let lightest_kept = back
            .scores
            .liked_authors
            .values()
            .copied()
            .fold(f64::INFINITY, f64::min);
        // Every cut author weighed less than every kept one.
        let kept = back.scores.liked_authors.len();
        assert_eq!(
            lightest_kept,
            f64::from(u32::try_from(10_000 - kept).unwrap())
        );
        assert_eq!(back.languages, BTreeSet::from(["en".to_string()]));
    }

    #[test]
    fn another_format_does_not_decode() {
        let mut taste = wide(1);
        taste.format = STORED_TASTE_FORMAT + 1;
        let json = serde_json::to_string(&taste).unwrap();
        assert_eq!(StoredTaste::decode(&json), None);
        assert_eq!(StoredTaste::decode("not json"), None);
    }

    #[test]
    fn a_taste_is_fresh_for_a_day() {
        let now = Utc::now();
        assert!(is_fresh(now - Duration::hours(23), now));
        assert!(!is_fresh(now - Duration::hours(25), now));
    }
}
