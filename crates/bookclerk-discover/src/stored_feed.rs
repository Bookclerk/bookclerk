//! The stored form of a ranked Discover feed: card references joined to the
//! candidate pool when the page reads them.
//!
//! A ranked feed is stored as one `discover_feeds` cell, and the database
//! plugins cap one cell (and one result) at 256 KiB. Most of a card's bytes
//! are shared catalog metadata (the description above all) that the
//! subject's `discover_candidates` rows already hold. So the stored feed
//! keeps what is personal to the subject (shelves, order, score, reasons,
//! shelf tags, prices, wishers) plus the card's identity (title, authors,
//! series, ids), and leaves out each metadata field that reading it back
//! from the pool gives again. [`join_feed`] puts those fields back when the
//! page reads the feed.
//!
//! A card refers to its storefront editions, `(source, product_id)`, not to
//! a pool row's key, so the join can move to another store of listing
//! metadata (a shared table of store listings, say) without changing the
//! stored format: only [`CandidatePool`] changes.
//!
//! The rule is exact: a field is left out only when [`pool_value`] returns
//! the same value for it, and [`join_feed`] fills a missing field from the
//! same lookup. A card whose pool row is gone by the time the page reads it
//! (a newer fetch replaced it; the feed is then ranked again) shows without
//! those fields until the new ranking lands.

use std::collections::HashMap;

use bookclerk_library::StoredCandidate;

use crate::candidates::StorefrontCandidate;
use crate::recommend::Recommendation;
use crate::shelves::{fit_feed_within, DiscoverFeed};

/// Format of the stored feed this release writes and reads.
///
/// A stored feed with another format (or none: the full cards an older
/// release stored) does not decode, counts as missing and is ranked again.
pub const STORED_FEED_FORMAT: u32 = 2;

/// A stored feed as written: the format tag beside the feed, whose cards
/// leave out the metadata the pool holds.
#[derive(serde::Deserialize)]
struct StoredFeedEnvelope {
    /// Stored feed format; must be [`STORED_FEED_FORMAT`].
    format: u32,
    /// The shelves, with pool metadata left out of the cards.
    feed: DiscoverFeed,
}

/// The subject's stored candidates, by storefront edition.
#[derive(Debug, Default)]
pub struct CandidatePool {
    /// `(source, product_id)` of each row's primary edition → its candidate.
    /// Rows are read oldest fetch first, so the newest row wins a tie.
    by_edition: HashMap<(String, String), StorefrontCandidate>,
}

impl CandidatePool {
    /// Builds the pool from stored candidate rows, skipping rows that do not
    /// decode.
    ///
    /// # Arguments
    ///
    /// * `rows` - The subject's `discover_candidates` rows, oldest fetch first.
    ///
    /// # Returns
    ///
    /// The candidates keyed by their primary storefront edition.
    #[must_use]
    pub fn from_rows(rows: &[StoredCandidate]) -> Self {
        let mut by_edition = HashMap::with_capacity(rows.len());
        for row in rows {
            match serde_json::from_str::<StorefrontCandidate>(&row.candidate_json) {
                Ok(c) => {
                    by_edition.insert((c.source.clone(), c.product_id.clone()), c);
                }
                Err(err) => tracing::debug!(error = %err, "skip an unreadable stored candidate"),
            }
        }
        Self { by_edition }
    }

    /// The pool rows behind one card, the card's own edition first, then its
    /// other store editions in card order.
    ///
    /// # Arguments
    ///
    /// * `card` - The card to look up.
    ///
    /// # Returns
    ///
    /// The matching candidates; empty when none is stored.
    fn rows_for<'a>(&'a self, card: &Recommendation) -> Vec<&'a StorefrontCandidate> {
        let mut keys: Vec<(&str, &str)> = Vec::with_capacity(card.store_editions.len() + 1);
        if let (Some(source), Some(product)) = (
            card.candidate_source.as_deref(),
            card.candidate_product_id.as_deref(),
        ) {
            keys.push((source, product));
        }
        for edition in &card.store_editions {
            let key = (edition.source.as_str(), edition.product_id.as_str());
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys.into_iter()
            .filter_map(|(source, product)| {
                self.by_edition
                    .get(&(source.to_owned(), product.to_owned()))
            })
            .collect()
    }
}

/// The first value a field has among `rows`, in order.
///
/// # Arguments
///
/// * `rows` - Pool rows behind a card, from [`CandidatePool::rows_for`].
/// * `get` - Reads the field from one candidate.
///
/// # Returns
///
/// The field's value from the first row that has one.
fn pool_value<'a, T: ?Sized>(
    rows: &[&'a StorefrontCandidate],
    get: impl Fn(&'a StorefrontCandidate) -> Option<&'a T>,
) -> Option<&'a T> {
    rows.iter().find_map(|c| get(c))
}

/// Applies `$action!(card_field, candidate_field)` to every metadata field
/// the pool holds for a card. Each pair names a `Recommendation` field and
/// the `StorefrontCandidate` field it is copied from when a card is ranked.
macro_rules! pool_fields {
    ($action:ident) => {
        $action!(cover_url, cover_url);
        $action!(subtitle, subtitle);
        $action!(description, description);
        $action!(publisher, publisher);
        $action!(published_at, published_at);
        $action!(genres, categories);
        $action!(language, language);
        $action!(narrators, narrators);
    };
}

/// Leaves out of `card` each metadata field the pool gives back unchanged.
///
/// # Arguments
///
/// * `card` - The card to shorten.
/// * `pool` - The subject's candidate pool.
fn strip_card(card: &mut Recommendation, pool: &CandidatePool) {
    let rows = pool.rows_for(card);
    if rows.is_empty() {
        return;
    }
    macro_rules! strip {
        ($field:ident, $from:ident) => {
            if card.$field.is_some()
                && card.$field.as_deref() == pool_value(&rows, |c| c.$from.as_deref())
            {
                card.$field = None;
            }
        };
    }
    pool_fields!(strip);
    if card.length_minutes.is_some()
        && card.length_minutes.as_ref() == pool_value(&rows, |c| c.length_minutes.as_ref())
    {
        card.length_minutes = None;
    }
}

/// Fills each missing metadata field of `card` from the pool.
///
/// # Arguments
///
/// * `card` - The card read from a stored feed.
/// * `pool` - The subject's candidate pool.
fn join_card(card: &mut Recommendation, pool: &CandidatePool) {
    let rows = pool.rows_for(card);
    if rows.is_empty() {
        return;
    }
    macro_rules! join {
        ($field:ident, $from:ident) => {
            if card.$field.is_none() {
                card.$field = pool_value(&rows, |c| c.$from.as_deref()).map(str::to_owned);
            }
        };
    }
    pool_fields!(join);
    if card.length_minutes.is_none() {
        card.length_minutes = pool_value(&rows, |c| c.length_minutes.as_ref()).copied();
    }
}

/// Removes the `null` fields of each shelf and card in an encoded feed.
///
/// Every field a shelf or card leaves out is an `Option`, which decodes as `None`
/// when absent, so dropping the `null` keeps the stored feed small without
/// changing what it decodes to.
///
/// # Arguments
///
/// * `feed` - The feed encoded as a JSON value.
fn drop_null_card_fields(feed: &mut serde_json::Value) {
    let Some(shelves) = feed.get_mut("shelves").and_then(|s| s.as_array_mut()) else {
        return;
    };
    for shelf in shelves {
        if let Some(fields) = shelf.as_object_mut() {
            fields.retain(|_, value| !value.is_null());
        }
        let Some(items) = shelf.get_mut("items").and_then(|i| i.as_array_mut()) else {
            continue;
        };
        for item in items {
            if let Some(card) = item.as_object_mut() {
                card.retain(|_, value| !value.is_null());
            }
        }
    }
}

/// Encodes one feed in the stored form.
///
/// # Arguments
///
/// * `feed` - The feed, with pool fields already left out.
///
/// # Returns
///
/// The JSON stored in `discover_feeds.feed_json`.
///
/// # Errors
///
/// Returns the JSON error when the feed does not serialize.
fn encode_envelope(feed: &DiscoverFeed) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(feed)?;
    drop_null_card_fields(&mut value);
    serde_json::to_string(&serde_json::json!({
        "format": STORED_FEED_FORMAT,
        "feed": value,
    }))
}

/// Encodes a ranked feed for storage, leaving out what the pool holds and
/// keeping the result within `budget` bytes once stored.
///
/// The budget is a backstop: with pool metadata left out a feed is a small
/// fraction of the cell limit, and only a feed that still exceeds it is
/// shortened as [`fit_feed_within`] describes.
///
/// # Arguments
///
/// * `feed` - The ranked feed; its cards lose their pool fields.
/// * `pool` - The subject's candidate pool the feed was ranked from.
/// * `budget` - Most bytes the stored feed may take as an escaped JSON string.
///
/// # Returns
///
/// The JSON to store.
///
/// # Errors
///
/// Returns the JSON error when the feed does not serialize.
pub fn encode_stored_feed(
    feed: &mut DiscoverFeed,
    pool: &CandidatePool,
    budget: usize,
) -> Result<String, serde_json::Error> {
    for card in feed.shelves.iter_mut().flat_map(|s| s.items.iter_mut()) {
        strip_card(card, pool);
    }
    fit_feed_within(feed, budget, encode_envelope)
}

/// Decodes a stored feed, without its pool metadata yet.
///
/// # Arguments
///
/// * `json` - `discover_feeds.feed_json`.
///
/// # Returns
///
/// `None` when it is not a feed of [`STORED_FEED_FORMAT`].
#[must_use]
pub fn decode_stored_feed(json: &str) -> Option<DiscoverFeed> {
    let envelope = serde_json::from_str::<StoredFeedEnvelope>(json).ok()?;
    (envelope.format == STORED_FEED_FORMAT).then_some(envelope.feed)
}

/// Whether any card of `feed` may have pool metadata to join.
///
/// # Arguments
///
/// * `feed` - A decoded stored feed.
///
/// # Returns
///
/// `true` when a card names a storefront edition.
#[must_use]
pub fn feed_needs_pool(feed: &DiscoverFeed) -> bool {
    feed.shelves
        .iter()
        .flat_map(|s| &s.items)
        .any(|card| card.candidate_product_id.is_some() || !card.store_editions.is_empty())
}

/// Puts the pool metadata back on every card of a decoded stored feed.
///
/// # Arguments
///
/// * `feed` - The decoded feed.
/// * `pool` - The subject's candidate pool as it is now.
pub fn join_feed(feed: &mut DiscoverFeed, pool: &CandidatePool) {
    for card in feed.shelves.iter_mut().flat_map(|s| s.items.iter_mut()) {
        join_card(card, pool);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::StoreEdition;
    use crate::shelves::DiscoverShelf;
    use bookclerk_library::SeedScope;

    /// A pool candidate with every metadata field set.
    fn candidate(source: &str, product: &str, description: &str) -> StorefrontCandidate {
        serde_json::from_value(serde_json::json!({
            "source": source,
            "product_id": product,
            "title": format!("Title {product}"),
            "authors": "Ada Author",
            "narrators": "Nia Narrator",
            "cover_url": format!("https://covers.example/{product}.jpg"),
            "subtitle": "A Subtitle",
            "description": description,
            "publisher": "Pub House",
            "length_minutes": 600,
            "published_at": "2020-01-02",
            "categories": "Fantasy, Epic",
            "language": "en",
            "origin": "related",
        }))
        .unwrap()
    }

    /// The card a ranking makes from `c`.
    fn card_from(c: &StorefrontCandidate) -> Recommendation {
        Recommendation {
            title: c.title.clone(),
            authors: c.authors.clone(),
            narrators: c.narrators.clone(),
            candidate_source: Some(c.source.clone()),
            candidate_product_id: Some(c.product_id.clone()),
            store_editions: vec![StoreEdition::new(&c.source, &c.product_id)],
            cover_url: c.cover_url.clone(),
            subtitle: c.subtitle.clone(),
            description: c.description.clone(),
            publisher: c.publisher.clone(),
            length_minutes: c.length_minutes,
            published_at: c.published_at.clone(),
            genres: c.categories.clone(),
            language: c.language.clone(),
            score: 1.5,
            reasons: vec!["matches liked author (Ada Author)".into()],
            categories: vec!["author".into()],
            ..Default::default()
        }
    }

    /// A stored row holding `c`.
    fn row(c: &StorefrontCandidate) -> StoredCandidate {
        StoredCandidate {
            source: c.source.clone(),
            candidate_key: c.product_id.clone(),
            seed_scope: SeedScope::Own,
            candidate_json: serde_json::to_string(c).unwrap(),
            fetched_at: chrono::Utc::now(),
        }
    }

    /// A feed with one shelf of `items`.
    fn feed(items: Vec<Recommendation>) -> DiscoverFeed {
        DiscoverFeed {
            shelves: vec![DiscoverShelf {
                id: "author:ada".into(),
                title: "More from Ada Author".into(),
                subtitle: None,
                items,
            }],
            ..Default::default()
        }
    }

    /// Cards round-trip through the stored form exactly, and the stored
    /// feed carries no description.
    #[test]
    fn a_stored_feed_reads_back_the_cards_it_was_given() {
        let long = "A long blurb. ".repeat(120);
        let a = candidate("audible", "B001", &long);
        let b = candidate("libro", "978001", "Short blurb.");
        let pool = CandidatePool::from_rows(&[row(&a), row(&b)]);
        let original = feed(vec![card_from(&a), card_from(&b)]);

        let mut stored = original.clone();
        let json = encode_stored_feed(&mut stored, &pool, 192 * 1024).unwrap();
        assert!(!json.contains("A long blurb"), "{json}");
        assert!(!json.contains("covers.example"), "{json}");
        assert!(!json.contains("null"), "{json}");
        assert!(json.contains("matches liked author"));

        let mut read = decode_stored_feed(&json).unwrap();
        assert!(feed_needs_pool(&read));
        join_feed(&mut read, &pool);
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
    }

    /// A field whose card value differs from the pool's, such as metadata
    /// merged in from another store, stays in the stored card.
    #[test]
    fn a_field_the_pool_does_not_give_back_is_kept() {
        let a = candidate("audible", "B001", "Pool blurb.");
        let pool = CandidatePool::from_rows(&[row(&a)]);
        let mut card = card_from(&a);
        card.description = Some("Merged blurb from another store.".into());
        let mut stored = feed(vec![card.clone()]);
        let json = encode_stored_feed(&mut stored, &pool, 192 * 1024).unwrap();
        assert!(json.contains("Merged blurb"));
        let mut read = decode_stored_feed(&json).unwrap();
        join_feed(&mut read, &pool);
        assert_eq!(
            read.shelves[0].items[0].description, card.description,
            "the stored value wins"
        );
    }

    /// Another store edition's row fills a field the card's own row lacks.
    #[test]
    fn another_edition_fills_what_the_first_lacks() {
        let mut a = candidate("audible", "B001", "Blurb.");
        a.publisher = None;
        let b = candidate("libro", "978001", "Other blurb.");
        let pool = CandidatePool::from_rows(&[row(&a), row(&b)]);
        let mut card = card_from(&a);
        card.publisher.clone_from(&b.publisher);
        card.store_editions
            .push(StoreEdition::new("libro", "978001"));
        let original = feed(vec![card]);
        let mut stored = original.clone();
        let json = encode_stored_feed(&mut stored, &pool, 192 * 1024).unwrap();
        assert!(!json.contains("Pub House"), "{json}");
        let mut read = decode_stored_feed(&json).unwrap();
        join_feed(&mut read, &pool);
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
    }

    /// A card with no pool row (a wish, or a row replaced since) keeps its
    /// metadata in the stored feed, and a card whose row is gone at read
    /// time still shows its title.
    #[test]
    fn cards_without_a_pool_row_keep_or_lose_only_metadata() {
        let a = candidate("audible", "B001", "Blurb.");
        let mut wish = card_from(&candidate("chirp", "c-9", "Wished blurb."));
        wish.from_request = true;
        let pool = CandidatePool::from_rows(&[row(&a)]);
        let mut stored = feed(vec![card_from(&a), wish]);
        let json = encode_stored_feed(&mut stored, &pool, 192 * 1024).unwrap();
        assert!(json.contains("Wished blurb."));

        let mut read = decode_stored_feed(&json).unwrap();
        join_feed(&mut read, &CandidatePool::default());
        let first = &read.shelves[0].items[0];
        assert_eq!(first.title, "Title B001");
        assert!(first.description.is_none());
        assert_eq!(
            read.shelves[0].items[1].description.as_deref(),
            Some("Wished blurb.")
        );
    }

    /// A feed in another format, or the full cards an older release stored,
    /// does not decode.
    #[test]
    fn other_formats_do_not_decode() {
        let old = serde_json::to_string(&feed(Vec::new())).unwrap();
        assert!(decode_stored_feed(&old).is_none());
        let newer =
            serde_json::json!({ "format": STORED_FEED_FORMAT + 1, "feed": feed(Vec::new()) });
        assert!(decode_stored_feed(&newer.to_string()).is_none());
        assert!(decode_stored_feed("not json").is_none());
        let current = serde_json::json!({ "format": STORED_FEED_FORMAT, "feed": feed(Vec::new()) });
        assert!(decode_stored_feed(&current.to_string()).is_some());
    }

    /// An unreadable row is skipped, and a pool with none is empty.
    #[test]
    fn an_unreadable_row_is_skipped() {
        let mut bad = row(&candidate("audible", "B001", "Blurb."));
        bad.candidate_json = "{".into();
        let pool = CandidatePool::from_rows(&[bad]);
        assert!(pool.by_edition.is_empty());
        assert!(!feed_needs_pool(&feed(vec![Recommendation::default()])));
    }

    /// The measured demo feed (eleven shelves, 15 cards on a full one, half
    /// the cards with a 1,600-character description) was over the 192 KiB
    /// stored budget as full cards. Stored as references it is a fraction
    /// of that, and keeps every card and reason.
    #[test]
    fn the_measured_demo_feed_fits_with_every_card() {
        let blurb = "Blurb text. ".repeat(137);
        let lens = [15, 15, 15, 15, 4, 15, 15, 15, 3, 15, 4];
        let mut rows = Vec::new();
        let mut original = DiscoverFeed::default();
        for (n, len) in lens.into_iter().enumerate() {
            let mut items = Vec::new();
            for i in 0..len {
                let mut c = candidate("audible", &format!("B{n:02}{i:02}"), &blurb);
                if i % 2 == 1 {
                    c.description = None;
                }
                c.categories = Some("Science Fiction & Fantasy, Epic, Adventure, ".repeat(6));
                let mut card = card_from(&c);
                card.reasons = (0..5)
                    .map(|r| format!("reason {r} for this card, words"))
                    .collect();
                card.seed_categories = Some("Fiction, Fantasy, Epic".into());
                rows.push(row(&c));
                items.push(card);
            }
            original.shelves.push(DiscoverShelf {
                id: format!("shelf{n}"),
                title: format!("Shelf {n}"),
                subtitle: None,
                items,
            });
        }
        let full = serde_json::to_string(&serde_json::to_string(&original).unwrap())
            .unwrap()
            .len();
        assert!(full > 192 * 1024, "fixture is only {full} bytes");

        let pool = CandidatePool::from_rows(&rows);
        let mut stored = original.clone();
        let json = encode_stored_feed(&mut stored, &pool, 192 * 1024).unwrap();
        let bytes = serde_json::to_string(&json).unwrap().len();
        assert!(bytes < 96 * 1024, "stored feed is {bytes} bytes");
        let mut read = decode_stored_feed(&json).unwrap();
        join_feed(&mut read, &pool);
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
    }
}
