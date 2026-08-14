//! Netflix-style Discover shelves built from scored storefront candidates.

use std::collections::{HashMap, HashSet};

use crate::identity::recommendation_map_key;
use crate::recommend::Recommendation;

/// One horizontal Discover row (series to finish, more by author, …).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiscoverShelf {
    /// Stable identifier for this item.
    pub id: String,
    /// Display title as shown on the storefront or library card.
    pub title: String,
    /// Optional subtitle when the catalog distinguishes it from the title.
    pub subtitle: Option<String>,
    /// Ordered collection of child entries for this response.
    pub items: Vec<Recommendation>,
}

/// A shelf kind the operator can ignore (all offered by default).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShelfKindInfo {
    /// Stable kind id (`finish_series`, `author`, `genre`, `from_store`, …).
    pub id: String,
    /// Operator-visible label for this account or item.
    pub label: String,
}

/// Full Discover page payload: ordered shelves of personalized candidates.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct DiscoverFeed {
    /// Ordered horizontal shelves that make up the Discover page.
    pub shelves: Vec<DiscoverShelf>,
    /// Catalog of shelf kinds for ignore prefs (empty `disabled_shelves` = all on).
    #[serde(default)]
    pub shelf_kinds: Vec<ShelfKindInfo>,
}

/// Taste context used to title and fill personalized shelves.
#[derive(Debug, Clone, Default)]
pub struct ShelfTaste {
    /// Lowercase author → (display name, weight).
    pub authors: HashMap<String, (String, f64)>,
    /// Lowercase narrator → (display name, weight).
    pub narrators: HashMap<String, (String, f64)>,
    /// Lowercase category/subject → (display name, weight).
    pub categories: HashMap<String, (String, f64)>,
    /// Seed title (lowercase) → authors on that seed (display).
    pub seed_authors_by_title: HashMap<String, Vec<String>>,
    /// Storefronts represented in the owned library (`audible`, `libro`, …).
    pub owned_sources: HashSet<String>,
}

/// Maximum items kept on one Discover shelf after score sort and de-dupe.
const SHELF_CAP: usize = 48;
/// How many “more from {Author}” shelves to emit from top taste weights.
const AUTHOR_SHELVES: usize = 3;
/// How many “if you like {Author}” shelves to emit from top taste weights.
const BECAUSE_SHELVES: usize = 2;
/// How many “because you like {Genre}” shelves to emit from top categories.
const GENRE_SHELVES: usize = 3;

/// Shelf kinds Discover can emit (for config / UI ignore lists).
#[must_use]
pub fn shelf_kind_catalog() -> Vec<ShelfKindInfo> {
    vec![
        kind("finish_series", "Finish these series"),
        kind("keep_listening", "Pick up where you left off"),
        kind("author", "More from {Author}"),
        kind("because", "If you like {Author}"),
        kind("narrator", "Narrated by {Narrator}"),
        kind("genre", "Because you like {Genre}"),
        kind("from_store", "From stores you use"),
        kind("chirp_deals", "Chirp deals"),
        kind("similar_taste", "Similar to books you finish"),
        kind("requests", "Wishlist"),
        kind("top_picks", "Top picks for you"),
    ]
}

/// Builds one catalog entry for the operator ignore-list UI.
fn kind(id: &str, label: &str) -> ShelfKindInfo {
    ShelfKindInfo {
        id: id.to_string(),
        label: label.to_string(),
    }
}

/// Whether `shelf_id` matches an ignore entry (exact, kind prefix, or `from_store`).
///
/// # Arguments
///
/// * `shelf_id` - Shelf id to test against the ignore list.
/// * `disabled` - Ignore-list entries from user preferences.
///
/// # Returns
///
/// `true` when the predicate holds.
#[must_use]
pub fn shelf_is_disabled(shelf_id: &str, disabled: &[String]) -> bool {
    if disabled.is_empty() {
        return false;
    }
    let id = shelf_id.to_ascii_lowercase();
    for raw in disabled {
        let d = raw.trim().to_ascii_lowercase();
        if d.is_empty() {
            continue;
        }
        if id == d {
            return true;
        }
        if id.starts_with(&format!("{d}:")) {
            return true;
        }
        if d == "from_store" && id.starts_with("from_") {
            return true;
        }
        // Allow `from_audible` style entries to match `from_audible` shelves.
        if d.starts_with("from_") && id == d {
            return true;
        }
    }
    false
}

/// Partition scored recommendations into Discover shelves (items may repeat).
///
/// # Arguments
///
/// * `recs` - Scored recommendations to partition into shelves.
/// * `taste` - Local taste context used to title personalized shelves.
/// * `per_shelf` - Max items per shelf (clamped).
/// * `disabled_shelves` - Shelf kind ids the operator has ignored.
///
/// # Returns
///
/// `DiscoverFeed` result.
#[must_use]
pub fn build_discover_feed(
    recs: &[Recommendation],
    taste: &ShelfTaste,
    per_shelf: usize,
    disabled_shelves: &[String],
) -> DiscoverFeed {
    let cap = per_shelf.clamp(4, SHELF_CAP);
    let mut shelves = Vec::new();

    push_shelf(
        &mut shelves,
        "finish_series",
        "Finish these series",
        Some("Gaps in series you already own"),
        filter_sorted(recs, |r| has_category(r, "finish_series")),
        cap,
        disabled_shelves,
    );

    push_shelf(
        &mut shelves,
        "keep_listening",
        "Pick up where you left off",
        Some("Series you’re actively listening to"),
        filter_sorted(recs, |r| has_category(r, "keep_listening")),
        cap,
        disabled_shelves,
    );

    // More from top liked authors.
    let mut authors: Vec<_> = taste.authors.values().cloned().collect();
    authors.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (display, _) in authors.into_iter().take(AUTHOR_SHELVES) {
        let key = display.to_lowercase();
        let id = format!("author:{}", slugish(&key));
        let title = format!("More from {display}");
        push_shelf(
            &mut shelves,
            &id,
            &title,
            Some("Because you’ve liked their work"),
            filter_sorted(recs, |r| {
                has_category(r, "author")
                    && r.authors
                        .as_deref()
                        .map(|a| {
                            split_people(a)
                                .iter()
                                .any(|n| n.eq_ignore_ascii_case(&display))
                        })
                        .unwrap_or(false)
            }),
            cap,
            disabled_shelves,
        );
    }

    // “If you like X, try these” — related/similar hits that are not by X.
    let mut because_authors: Vec<_> = taste.authors.values().cloned().collect();
    because_authors.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (display, _) in because_authors.into_iter().take(BECAUSE_SHELVES) {
        let id = format!("because:{}", slugish(&display.to_lowercase()));
        let title = format!("If you like {display}");
        push_shelf(
            &mut shelves,
            &id,
            &title,
            Some("Related titles and similar tastes — new voices"),
            filter_sorted(recs, |r| because_you_like(r, &display, taste)),
            cap,
            disabled_shelves,
        );
    }

    // Narrators you love.
    let mut narrators: Vec<_> = taste.narrators.values().cloned().collect();
    narrators.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((display, _)) = narrators.into_iter().next() {
        let id = format!("narrator:{}", slugish(&display.to_lowercase()));
        push_shelf(
            &mut shelves,
            &id,
            &format!("Narrated by {display}"),
            Some("More from a narrator you enjoy"),
            filter_sorted(recs, |r| {
                has_category(r, "narrator")
                    && r.narrators
                        .as_deref()
                        .map(|n| {
                            split_people(n)
                                .iter()
                                .any(|x| x.eq_ignore_ascii_case(&display))
                        })
                        .unwrap_or(false)
            }),
            cap,
            disabled_shelves,
        );
    }

    // Genre / subject shelves from local taste categories.
    let mut categories: Vec<_> = taste.categories.values().cloned().collect();
    categories.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (display, _) in categories.into_iter().take(GENRE_SHELVES) {
        let id = format!("genre:{}", slugish(&display.to_lowercase()));
        push_shelf(
            &mut shelves,
            &id,
            &format!("Because you like {display}"),
            Some("Genre and subject overlap from titles you’ve finished"),
            filter_sorted(recs, |r| category_overlap(r, &display)),
            cap,
            disabled_shelves,
        );
    }

    // From stores you already use.
    let mut sources: Vec<String> = taste.owned_sources.iter().cloned().collect();
    sources.sort();
    for source in sources {
        let display = store_display_name(&source);
        let id = format!("from_{source}");
        push_shelf(
            &mut shelves,
            &id,
            &format!("From {display}"),
            Some("More from a storefront already in your library"),
            filter_sorted(recs, |r| {
                has_category(r, "from_store")
                    && r.candidate_source
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&source))
            }),
            cap,
            disabled_shelves,
        );
    }

    push_shelf(
        &mut shelves,
        "chirp_deals",
        "Chirp deals right now",
        Some("Top and free deals from Chirp"),
        filter_sorted(recs, |r| has_category(r, "chirp_deals")),
        cap,
        disabled_shelves,
    );

    push_shelf(
        &mut shelves,
        "similar_taste",
        "Similar to books you finish",
        Some("Embedding / taste overlap"),
        filter_sorted(recs, |r| has_category(r, "similar_taste")),
        cap,
        disabled_shelves,
    );

    push_shelf(
        &mut shelves,
        "requests",
        "On the wishlist",
        Some("Titles people have wishlisted"),
        filter_sorted(recs, |r| has_category(r, "requests") || r.from_request),
        cap,
        disabled_shelves,
    );

    // Drop empty shelves; keep at least a "Top picks" dump if everything empty but recs exist.
    shelves.retain(|s| !s.items.is_empty());
    if shelves.is_empty() && !recs.is_empty() && !shelf_is_disabled("top_picks", disabled_shelves) {
        let mut top = recs.to_vec();
        top.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        top.truncate(cap);
        shelves.push(DiscoverShelf {
            id: String::from("top_picks"),
            title: String::from("Top picks for you"),
            subtitle: Some(String::from("Best overall matches right now")),
            items: top,
        });
    }

    DiscoverFeed {
        shelves,
        shelf_kinds: shelf_kind_catalog(),
    }
}

/// True when the recommendation lists `kind` among its categories (case-insensitive).
fn has_category(r: &Recommendation, kind: &str) -> bool {
    r.categories.iter().any(|c| c.eq_ignore_ascii_case(kind))
}

/// True when a genre-tagged rec shares a seed category with the liked genre.
fn category_overlap(r: &Recommendation, liked_category: &str) -> bool {
    if !has_category(r, "genre") {
        return false;
    }
    let want = liked_category.to_ascii_lowercase();
    r.seed_categories
        .as_deref()
        .map(|cats| {
            split_people(cats).iter().any(|c| {
                c.eq_ignore_ascii_case(liked_category) || c.to_ascii_lowercase().contains(&want)
            })
        })
        .unwrap_or(false)
}

/// Operator-facing storefront name (`audible` → `Audible`); unknown ids are title-cased.
fn store_display_name(source: &str) -> String {
    match source {
        "audible" => String::from("Audible"),
        "libro" => String::from("Libro.fm"),
        "chirp" => String::from("Chirp"),
        "graphicaudio" => String::from("GraphicAudio"),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
                None => other.to_string(),
            }
        }
    }
}

/// True when a rec is related/similar to a liked author but not by that author.
fn because_you_like(r: &Recommendation, liked_author: &str, taste: &ShelfTaste) -> bool {
    if r.from_request || has_category(r, "author") {
        return false;
    }
    // Prefer structured tags when present; fall back to origin/reason parsing.
    if !(has_category(r, "because")
        || has_category(r, "similar_taste")
        || r.reasons
            .iter()
            .any(|x| x.contains("related") || x.contains("similar to titles you finish")))
    {
        return false;
    }
    // Must not be another book by the same liked author.
    if r.authors
        .as_deref()
        .map(|a| {
            split_people(a)
                .iter()
                .any(|n| n.eq_ignore_ascii_case(liked_author))
        })
        .unwrap_or(false)
    {
        return false;
    }

    let liked = liked_author.to_lowercase();
    // Seeded from a title by that author (related / series expand).
    let from_their_seed = r.reasons.iter().any(|origin_line| {
        // origins look like `libro related to “Title”` — match seed title map.
        for (seed_title, authors) in &taste.seed_authors_by_title {
            if !authors.iter().any(|a| a.eq_ignore_ascii_case(liked_author)) {
                continue;
            }
            if origin_line.to_lowercase().contains(seed_title) {
                return true;
            }
        }
        false
    });
    if from_their_seed {
        return true;
    }

    // Strong similarity / storefront related without being the same author.
    let similar = r
        .reasons
        .iter()
        .any(|x| x.contains("similar to titles you finish") || x.contains("related"));
    if similar {
        // Prefer when overall taste includes this author heavily — already gated by shelf.
        return !liked.is_empty();
    }
    false
}

/// Filters recommendations by `pred` and sorts remaining items by descending score.
fn filter_sorted(
    recs: &[Recommendation],
    pred: impl Fn(&Recommendation) -> bool,
) -> Vec<Recommendation> {
    let mut out: Vec<_> = recs.iter().filter(|r| pred(r)).cloned().collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Appends a non-empty, de-duped shelf unless its id is on the ignore list.
fn push_shelf(
    shelves: &mut Vec<DiscoverShelf>,
    id: &str,
    title: &str,
    subtitle: Option<&str>,
    mut items: Vec<Recommendation>,
    cap: usize,
    disabled: &[String],
) {
    if shelf_is_disabled(id, disabled) {
        return;
    }
    items.truncate(cap);
    // De-dupe within shelf by product key.
    let mut seen = HashSet::new();
    items.retain(|r| seen.insert(recommendation_map_key(r)));
    if items.is_empty() {
        return;
    }
    shelves.push(DiscoverShelf {
        id: id.to_string(),
        title: title.to_string(),
        subtitle: subtitle.map(str::to_string),
        items,
    });
}

/// Splits a contributor/category string on `,`, `;`, `&`, and `/`.
fn split_people(s: &str) -> Vec<String> {
    s.split([',', ';', '&', '/'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Lowercase ASCII slug used in shelf ids (`More from Jane Doe` → `jane-doe`).
fn slugish(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Flatten shelves into a unique score-sorted list (CLI / legacy).
///
/// # Arguments
///
/// * `feed` - Discover feed to flatten.
/// * `limit` - Maximum number of results to return.
///
/// # Returns
///
/// Collected results (may be empty).
#[must_use]
pub fn flatten_feed(feed: &DiscoverFeed, limit: usize) -> Vec<Recommendation> {
    let mut by_key: HashMap<String, Recommendation> = HashMap::new();
    for shelf in &feed.shelves {
        for r in &shelf.items {
            let key = recommendation_map_key(r);
            by_key
                .entry(key)
                .and_modify(|e| {
                    if r.score > e.score {
                        *e = r.clone();
                    }
                })
                .or_insert_with(|| r.clone());
        }
    }
    let mut out: Vec<_> = by_key.into_values().collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(title: &str, score: f64, categories: &[&str]) -> Recommendation {
        let mut r = Recommendation {
            title: title.into(),
            authors: Some("Ada Author".into()),
            series: Some("Test Series".into()),
            series_index: Some("3".into()),
            asin: Some(title.into()),
            score,
            candidate_source: Some("audible".into()),
            candidate_product_id: Some(title.into()),
            categories: categories.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        };
        r.work_key = recommendation_map_key(&r);
        r
    }

    #[test]
    fn builds_finish_and_because_shelves() {
        let recs = vec![
            rec("Book3", 30.0, &["finish_series"]),
            Recommendation {
                authors: Some("Other Writer".into()),
                reasons: vec![
                    "libro related to “Seed By Ada”".into(),
                    "similar to titles you finish (sim 0.40)".into(),
                ],
                categories: vec!["similar_taste".into()],
                ..rec("OtherBook", 18.0, &[])
            },
        ];
        let mut taste = ShelfTaste::default();
        taste
            .authors
            .insert("ada author".into(), ("Ada Author".into(), 5.0));
        taste
            .seed_authors_by_title
            .insert("seed by ada".into(), vec!["Ada Author".into()]);

        let feed = build_discover_feed(&recs, &taste, 8, &[]);
        assert!(feed.shelves.iter().any(|s| s.id == "finish_series"));
        assert!(feed
            .shelves
            .iter()
            .any(|s| s.id.starts_with("because:") && !s.items.is_empty()));
        assert!(!feed.shelf_kinds.is_empty());
    }

    #[test]
    fn disabled_shelves_hide_kinds() {
        let recs = vec![
            rec("Book3", 30.0, &["finish_series"]),
            Recommendation {
                seed_categories: Some("Science Fiction".into()),
                candidate_source: Some("chirp".into()),
                categories: vec!["genre".into(), "from_store".into(), "chirp_deals".into()],
                ..rec("DealBook", 12.0, &[])
            },
        ];
        let mut taste = ShelfTaste::default();
        taste
            .categories
            .insert("science fiction".into(), ("Science Fiction".into(), 4.0));
        taste.owned_sources.insert("chirp".into());

        let feed = build_discover_feed(
            &recs,
            &taste,
            8,
            &[
                String::from("finish_series"),
                String::from("genre"),
                String::from("from_store"),
                String::from("chirp_deals"),
            ],
        );
        assert!(!feed.shelves.iter().any(|s| s.id == "finish_series"));
        assert!(!feed.shelves.iter().any(|s| s.id.starts_with("genre:")));
        assert!(!feed.shelves.iter().any(|s| s.id.starts_with("from_")));
        assert!(!feed.shelves.iter().any(|s| s.id == "chirp_deals"));
    }

    #[test]
    fn genre_and_store_and_deals_shelves() {
        let recs = vec![Recommendation {
            seed_categories: Some("Mystery".into()),
            candidate_source: Some("libro".into()),
            categories: vec!["genre".into(), "from_store".into()],
            ..rec("MysteryBook", 15.0, &[])
        }];
        let mut taste = ShelfTaste::default();
        taste
            .categories
            .insert("mystery".into(), ("Mystery".into(), 3.0));
        taste.owned_sources.insert("libro".into());

        let feed = build_discover_feed(&recs, &taste, 8, &[]);
        assert!(feed.shelves.iter().any(|s| s.id.starts_with("genre:")));
        assert!(feed.shelves.iter().any(|s| s.id == "from_libro"));
    }
}
