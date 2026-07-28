//! Canonical identity for discovery candidates / recommendations.
//!
//! Prefer bibliographic ids (ISBN, then ASIN), then soft title+author matching
//! so the same book from multiple storefronts collapses to one card.

use bookclerk_enrich::{
    canonicalize_isbn, clean_author_for_compares, clean_title_for_compares, levenshtein_similarity,
};
use bookclerk_library::GlobalQueueEntry;

use crate::candidates::StorefrontCandidate;
use crate::recommend::Recommendation;

/// One storefront edition of a work (for multi-store purchase links).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub struct StoreEdition {
    pub source: String,
    pub product_id: String,
}

impl StoreEdition {
    #[must_use]
    pub fn new(source: impl Into<String>, product_id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            product_id: product_id.into(),
        }
    }
}

/// Hard bibliographic key: canonical ISBN first, then ASIN.
///
/// Note: ISBN is not published by every storefront (Chirp / GraphicAudio /
/// Audible public search often omit it). Soft title+author matching then applies.
#[must_use]
pub fn hard_work_key(asin: Option<&str>, isbn: Option<&str>) -> Option<String> {
    if let Some(isbn) = isbn.map(canonicalize_isbn).filter(|s| !s.is_empty()) {
        return Some(format!("isbn:{isbn}"));
    }
    let asin = asin.map(str::trim).filter(|s| !s.is_empty())?;
    Some(format!("asin:{}", asin.to_ascii_uppercase()))
}

/// Bibliographic identity slice used for merge decisions.
#[derive(Debug, Clone, Copy)]
pub struct WorkIdentity<'a> {
    pub asin: Option<&'a str>,
    pub isbn: Option<&'a str>,
    pub title: &'a str,
    pub authors: Option<&'a str>,
}

impl<'a> WorkIdentity<'a> {
    #[must_use]
    pub fn new(
        asin: Option<&'a str>,
        isbn: Option<&'a str>,
        title: &'a str,
        authors: Option<&'a str>,
    ) -> Self {
        Self {
            asin,
            isbn,
            title,
            authors,
        }
    }
}

/// Whether two bibliographic identities refer to the same work.
#[must_use]
pub fn identities_match(a: WorkIdentity<'_>, b: WorkIdentity<'_>) -> bool {
    let key_a = hard_work_key(a.asin, a.isbn);
    let key_b = hard_work_key(b.asin, b.isbn);
    if let (Some(ka), Some(kb)) = (&key_a, &key_b) {
        if ka == kb {
            return true;
        }
        // Same work can be keyed as isbn:… in one place and asin:… in another.
        // Only soft-merge when titles agree — never collapse unrelated hard keys.
        let cross = (ka.starts_with("isbn:") && kb.starts_with("asin:"))
            || (ka.starts_with("asin:") && kb.starts_with("isbn:"));
        if cross {
            return works_match(a.title, a.authors, b.title, b.authors);
        }
        return false;
    }
    // One or both sides lack a hard key — soft title+author match.
    // Also merge when ASINs match exactly even if titles differ slightly.
    if let (Some(aa), Some(ab)) = (
        a.asin.map(str::trim).filter(|s| !s.is_empty()),
        b.asin.map(str::trim).filter(|s| !s.is_empty()),
    ) {
        if aa.eq_ignore_ascii_case(ab) {
            return true;
        }
    }
    if let (Some(ia), Some(ib)) = (
        a.isbn.map(canonicalize_isbn).filter(|s| !s.is_empty()),
        b.isbn.map(canonicalize_isbn).filter(|s| !s.is_empty()),
    ) {
        if ia == ib {
            return true;
        }
    }
    works_match(a.title, a.authors, b.title, b.authors)
}

/// Soft key from cleaned title + primary author (exact string match only).
#[must_use]
pub fn soft_work_key(title: &str, authors: Option<&str>) -> String {
    let t = clean_title_for_compares(title, false);
    let a = primary_author_cleaned(authors).unwrap_or_default();
    format!("soft:{t}|{a}")
}

/// Best stable map key for a recommendation / candidate.
#[must_use]
pub fn work_map_key(
    asin: Option<&str>,
    isbn: Option<&str>,
    title: &str,
    authors: Option<&str>,
    source: Option<&str>,
    product_id: Option<&str>,
) -> String {
    if let Some(k) = hard_work_key(asin, isbn) {
        return k;
    }
    if !title.trim().is_empty() {
        let soft = soft_work_key(title, authors);
        // Avoid collapsing unrelated empties into one bucket.
        if soft != "soft:|" {
            return soft;
        }
    }
    match (source, product_id) {
        (Some(s), Some(p)) if !s.is_empty() && !p.is_empty() => format!("{s}:{p}"),
        (_, Some(p)) if !p.is_empty() => format!("product:{p}"),
        _ => format!("title:{}", title.trim().to_ascii_lowercase()),
    }
}

#[must_use]
pub fn candidate_map_key(c: &StorefrontCandidate) -> String {
    work_map_key(
        c.asin.as_deref(),
        c.isbn.as_deref(),
        &c.title,
        c.authors.as_deref(),
        Some(c.source.as_str()),
        Some(c.product_id.as_str()),
    )
}

#[must_use]
pub fn recommendation_map_key(r: &Recommendation) -> String {
    work_map_key(
        r.asin.as_deref(),
        r.isbn.as_deref(),
        &r.title,
        r.authors.as_deref(),
        r.candidate_source.as_deref(),
        r.candidate_product_id.as_deref(),
    )
}

/// Whether two titles should be treated as the same work.
#[must_use]
pub fn works_match(
    title_a: &str,
    authors_a: Option<&str>,
    title_b: &str,
    authors_b: Option<&str>,
) -> bool {
    let ta = clean_title_for_compares(title_a, false);
    let tb = clean_title_for_compares(title_b, false);
    if ta.is_empty() || tb.is_empty() {
        return false;
    }
    let title_sim = levenshtein_similarity(&ta, &tb);
    let contained = title_contains_other(&ta, &tb);
    let authors = (
        primary_author_cleaned(authors_a),
        primary_author_cleaned(authors_b),
    );
    match authors {
        (Some(a), Some(b)) => {
            let author_ok = a == b || levenshtein_similarity(&a, &b) >= 0.85;
            if !author_ok {
                return false;
            }
            // Exact / near-exact titles, or short/long variants ("Hail Mary" /
            // "Project Hail Mary") when the author is the same.
            title_sim >= 0.90 || contained
        }
        // One side missing author: require a tighter title match.
        (None, None) => title_sim >= 0.96 || (contained && title_sim >= 0.75),
        _ => title_sim >= 0.95 || (contained && title_sim >= 0.70),
    }
}

/// True when the shorter cleaned title is a whole-word subset of the longer one.
fn title_contains_other(a: &str, b: &str) -> bool {
    let (shorter, longer) = if a.chars().count() <= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    if shorter.chars().count() < 4 {
        return false;
    }
    if longer == shorter
        || longer.starts_with(&format!("{shorter} "))
        || longer.ends_with(&format!(" {shorter}"))
    {
        return true;
    }
    longer.contains(&format!(" {shorter} "))
}

fn primary_author_cleaned(authors: Option<&str>) -> Option<String> {
    let raw = authors?
        .split([',', ';', '&', '/'])
        .map(str::trim)
        .find(|s| !s.is_empty())?;
    let cleaned = clean_author_for_compares(raw);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Merge storefront metadata into `into`, preferring filled fields / richer text.
pub fn merge_candidate_metadata(into: &mut StorefrontCandidate, from: &StorefrontCandidate) {
    push_edition(
        &mut into.store_editions,
        StoreEdition::new(&from.source, &from.product_id),
    );
    if into.asin.is_none() {
        into.asin = from.asin.clone();
    }
    if into.isbn.is_none() {
        into.isbn = from.isbn.clone().map(|i| {
            let n = canonicalize_isbn(&i);
            if n.is_empty() {
                i
            } else {
                n
            }
        });
    } else if let Some(isbn) = into.isbn.as_mut() {
        let n = canonicalize_isbn(isbn);
        if !n.is_empty() {
            *isbn = n;
        }
    }
    fill_opt_string(&mut into.authors, from.authors.as_deref());
    fill_opt_string(&mut into.narrators, from.narrators.as_deref());
    fill_opt_string(&mut into.series, from.series.as_deref());
    fill_opt_string(&mut into.series_index, from.series_index.as_deref());
    if from.title.len() > into.title.len() && !from.title.is_empty() {
        // Prefer the longer title when both exist (often includes subtitle).
        into.title = from.title.clone();
    }
    if !from.origin.is_empty() && !into.origin.contains(&from.origin) {
        into.origin = format!("{}; {}", into.origin, from.origin);
    }
}

/// Merge a scored recommendation into an existing card.
pub fn merge_recommendation(into: &mut Recommendation, mut from: Recommendation) {
    for ed in std::mem::take(&mut from.store_editions) {
        push_edition(&mut into.store_editions, ed);
    }
    if let (Some(s), Some(p)) = (
        from.candidate_source.as_deref(),
        from.candidate_product_id.as_deref(),
    ) {
        push_edition(&mut into.store_editions, StoreEdition::new(s, p));
    }
    if let (Some(s), Some(p)) = (
        into.candidate_source.as_deref(),
        into.candidate_product_id.as_deref(),
    ) {
        push_edition(&mut into.store_editions, StoreEdition::new(s, p));
    }

    if from.score > into.score {
        into.score = from.score;
        // Keep the higher-scoring store as the primary candidate identity.
        if from.candidate_source.is_some() {
            into.candidate_source = from.candidate_source.clone();
            into.candidate_product_id = from.candidate_product_id.clone();
        }
        if from.title.len() >= into.title.len() {
            into.title = from.title.clone();
        }
    } else {
        // Multi-store agreement is itself a signal.
        into.score += 0.35;
    }

    if into.asin.is_none() {
        into.asin = from.asin.clone();
    }
    if into.isbn.is_none() {
        into.isbn = from.isbn.clone().map(|i| {
            let n = canonicalize_isbn(&i);
            if n.is_empty() {
                i
            } else {
                n
            }
        });
    }
    fill_opt_string(&mut into.authors, from.authors.as_deref());
    fill_opt_string(&mut into.narrators, from.narrators.as_deref());
    fill_opt_string(&mut into.series, from.series.as_deref());
    fill_opt_string(&mut into.series_index, from.series_index.as_deref());
    fill_opt_string(&mut into.seed_categories, from.seed_categories.as_deref());
    if into.work_id.is_none() {
        into.work_id = from.work_id.clone();
    }
    if from.from_request {
        into.from_request = true;
        if into.request_uuid.is_none() {
            into.request_uuid = from.request_uuid.clone();
        }
    }
    for cat in from.categories {
        push_shelf_category(&mut into.categories, &cat);
    }
    for reason in from.reasons {
        if !into.reasons.iter().any(|r| r == &reason) {
            into.reasons.push(reason);
        }
    }
    for hint in from.purchase_hints {
        if !into.purchase_hints.iter().any(|h| {
            h.source.eq_ignore_ascii_case(&hint.source)
                && h.product_id.eq_ignore_ascii_case(&hint.product_id)
        }) {
            into.purchase_hints.push(hint);
        }
    }
    into.work_key = recommendation_map_key(into);
}

pub fn push_edition(editions: &mut Vec<StoreEdition>, edition: StoreEdition) {
    if edition.source.trim().is_empty() || edition.product_id.trim().is_empty() {
        return;
    }
    if editions.iter().any(|e| {
        e.source.eq_ignore_ascii_case(&edition.source)
            && e.product_id.eq_ignore_ascii_case(&edition.product_id)
    }) {
        return;
    }
    // Prefer one edition per source (keep first / primary).
    if editions
        .iter()
        .any(|e| e.source.eq_ignore_ascii_case(&edition.source))
    {
        return;
    }
    editions.push(edition);
}

/// Append a stable shelf-kind tag (`finish_series`, `author`, …) if missing.
pub fn push_shelf_category(categories: &mut Vec<String>, kind: &str) {
    let kind = kind.trim();
    if kind.is_empty() {
        return;
    }
    if categories.iter().any(|c| c.eq_ignore_ascii_case(kind)) {
        return;
    }
    categories.push(kind.to_ascii_lowercase());
}

/// Merge wishlist queue rows that share ISBN/ASIN or soft title+author identity.
///
/// Sums `wish_count` and prefers ISBN-keyed `work_key` when available.
#[must_use]
pub fn merge_global_queue_entries(entries: Vec<GlobalQueueEntry>) -> Vec<GlobalQueueEntry> {
    let mut merged: Vec<GlobalQueueEntry> = Vec::new();
    for entry in entries {
        if let Some(existing) = merged.iter_mut().find(|e| {
            identities_match(
                WorkIdentity::new(
                    e.asin.as_deref(),
                    e.isbn.as_deref(),
                    &e.title,
                    e.authors.as_deref(),
                ),
                WorkIdentity::new(
                    entry.asin.as_deref(),
                    entry.isbn.as_deref(),
                    &entry.title,
                    entry.authors.as_deref(),
                ),
            )
        }) {
            existing.wish_count += entry.wish_count;
            for uuid in entry.sample_uuids {
                if existing.sample_uuids.len() >= 8 {
                    break;
                }
                if !existing.sample_uuids.contains(&uuid) {
                    existing.sample_uuids.push(uuid);
                }
            }
            if entry.first_requested_at < existing.first_requested_at {
                existing.first_requested_at = entry.first_requested_at;
            }
            if entry.last_requested_at > existing.last_requested_at {
                existing.last_requested_at = entry.last_requested_at;
                existing.title = entry.title;
                if entry.authors.is_some() {
                    existing.authors = entry.authors;
                }
            }
            if existing.asin.is_none() {
                existing.asin = entry.asin;
            }
            if existing.isbn.is_none() {
                existing.isbn = entry.isbn.map(|i| {
                    let n = canonicalize_isbn(&i);
                    if n.is_empty() {
                        i
                    } else {
                        n
                    }
                });
            } else if let Some(isbn) = existing.isbn.as_mut() {
                let n = canonicalize_isbn(isbn);
                if !n.is_empty() {
                    *isbn = n;
                }
            }
            // Prefer the strongest bibliographic key.
            existing.work_key = work_map_key(
                existing.asin.as_deref(),
                existing.isbn.as_deref(),
                &existing.title,
                existing.authors.as_deref(),
                None,
                None,
            );
        } else {
            let mut entry = entry;
            if let Some(isbn) = entry.isbn.as_mut() {
                let n = canonicalize_isbn(isbn);
                if !n.is_empty() {
                    *isbn = n;
                }
            }
            entry.work_key = work_map_key(
                entry.asin.as_deref(),
                entry.isbn.as_deref(),
                &entry.title,
                entry.authors.as_deref(),
                None,
                None,
            );
            merged.push(entry);
        }
    }
    merged
}

fn fill_opt_string(slot: &mut Option<String>, incoming: Option<&str>) {
    let Some(v) = incoming.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    match slot {
        None => *slot = Some(v.to_string()),
        Some(existing) if existing.len() < v.len() => *slot = Some(v.to_string()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isbn_preferred_over_asin() {
        let key = hard_work_key(Some("B00TEST"), Some("978-1-234-56789-0")).unwrap();
        assert_eq!(key, "isbn:9781234567890");
    }

    #[test]
    fn isbn10_and_isbn13_share_hard_key() {
        // ISBN-10 0-306-40615-2 → ISBN-13 9780306406157
        let a = hard_work_key(None, Some("0-306-40615-2")).unwrap();
        let b = hard_work_key(None, Some("978-0-306-40615-7")).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "isbn:9780306406157");
    }

    #[test]
    fn asin_and_isbn_soft_merge_when_titles_match() {
        assert!(identities_match(
            WorkIdentity::new(
                Some("B00HAIL"),
                None,
                "Project Hail Mary",
                Some("Andy Weir"),
            ),
            WorkIdentity::new(
                None,
                Some("9781234567890"),
                "Project Hail Mary: A Novel",
                Some("Andy Weir"),
            ),
        ));
    }

    #[test]
    fn short_and_long_titles_merge_same_author() {
        assert!(works_match(
            "Hail Mary",
            Some("Andy Weir"),
            "Project Hail Mary",
            Some("Andy Weir"),
        ));
    }

    #[test]
    fn merge_queue_sums_wish_counts_across_asin_isbn() {
        use chrono::Utc;
        let now = Utc::now();
        let merged = merge_global_queue_entries(vec![
            GlobalQueueEntry {
                work_key: "asin:B00HAIL".into(),
                title: "Hail Mary".into(),
                authors: Some("Andy Weir".into()),
                asin: Some("B00HAIL".into()),
                isbn: None,
                wish_count: 1,
                sample_uuids: vec!["a".into()],
                first_requested_at: now,
                last_requested_at: now,
            },
            GlobalQueueEntry {
                work_key: "isbn:9781234567890".into(),
                title: "Project Hail Mary".into(),
                authors: Some("Andy Weir".into()),
                asin: None,
                isbn: Some("9781234567890".into()),
                wish_count: 2,
                sample_uuids: vec!["b".into()],
                first_requested_at: now,
                last_requested_at: now,
            },
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].wish_count, 3);
        assert!(merged[0].work_key.starts_with("isbn:"));
    }

    #[test]
    fn soft_match_same_title_author() {
        assert!(works_match(
            "Project Hail Mary",
            Some("Andy Weir"),
            "Project Hail Mary: A Novel",
            Some("Andy Weir")
        ));
    }

    #[test]
    fn soft_match_rejects_different_author() {
        assert!(!works_match(
            "Project Hail Mary",
            Some("Andy Weir"),
            "Project Hail Mary",
            Some("Someone Else")
        ));
    }

    #[test]
    fn merge_keeps_editions_per_source() {
        let mut editions = vec![StoreEdition::new("audible", "B00A")];
        push_edition(&mut editions, StoreEdition::new("libro", "9781"));
        push_edition(&mut editions, StoreEdition::new("audible", "B00B"));
        assert_eq!(editions.len(), 2);
        assert_eq!(editions[1].source, "libro");
    }
}
