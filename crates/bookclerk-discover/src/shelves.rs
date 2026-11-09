//! Netflix-style Discover shelves built from scored storefront candidates.

use std::collections::{HashMap, HashSet};

use crate::identity::recommendation_map_key;
use crate::recommend::Recommendation;

/// One horizontal Discover row (series to finish, more by author, …).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct DiscoverShelf {
    /// Stable identifier for this item.
    #[schema(example = "author:ursula-k-le-guin")]
    pub id: String,
    /// Display title as shown on the storefront or library card.
    #[schema(example = "More from Ursula K. Le Guin")]
    pub title: String,
    /// Optional subtitle when the catalog distinguishes it from the title.
    #[schema(example = "Because you’ve liked their work")]
    pub subtitle: Option<String>,
    /// Ordered collection of child entries for this response.
    pub items: Vec<Recommendation>,
}

/// A shelf kind the operator can ignore (all offered by default).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ShelfKindInfo {
    /// Stable kind id (`finish_series`, `author`, `genre`, `from_store`, …).
    #[schema(example = "finish_series")]
    pub id: String,
    /// Operator-visible label for this account or item.
    #[schema(example = "Finish these series")]
    pub label: String,
}

/// Full Discover page payload: ordered shelves of personalized candidates.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, utoipa::ToSchema)]
pub struct DiscoverFeed {
    /// Ordered horizontal shelves that make up the Discover page.
    pub shelves: Vec<DiscoverShelf>,
    /// Catalog of shelf kinds for ignore prefs (empty `disabled_shelves` = all on).
    #[serde(default)]
    pub shelf_kinds: Vec<ShelfKindInfo>,
    /// Whether these are the caller's current shelves, set by the daemon's
    /// read of the stored feed. Absent in a stored feed and from the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "ready")]
    pub state: Option<FeedState>,
    /// RFC 3339 time the shelves were ranked; absent while `state` is `preparing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "2026-11-06T00:30:00Z")]
    pub computed_at: Option<String>,
}

/// How current a stored Discover feed is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FeedState {
    /// The stored shelves are current.
    Ready,
    /// Stored shelves are shown while a newer fetch or ranking is queued.
    Refreshing,
    /// No shelves are stored yet; a fetch is queued and the page shows placeholders.
    Preparing,
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
    /// Titles on the subject's own open wishlist, newest first, that name
    /// the "Because you wishlisted" shelves.
    pub wish_titles: Vec<String>,
    /// Lowercase storefront ids the subject has an account with, compared
    /// at member price when a card is placed on a store shelf.
    pub linked_sources: HashSet<String>,
}

/// Maximum items kept on one Discover shelf after score sort and de-dupe.
const SHELF_CAP: usize = 48;
/// How many “more from {Author}” shelves to emit from top taste weights.
const AUTHOR_SHELVES: usize = 3;
/// How many “if you like {Author}” shelves to emit from top taste weights.
const BECAUSE_SHELVES: usize = 2;
/// How many “because you like {Genre}” shelves to emit from top categories.
const GENRE_SHELVES: usize = 3;
/// How many “because you wishlisted {Title}” shelves to emit, newest wish first.
const WISH_SHELVES: usize = 3;
/// Most titles of one series on a shelf that is not about series, so one
/// long series cannot crowd the others out.
pub const SERIES_CAP: usize = 2;

/// Shelf kinds Discover can emit (for config / UI ignore lists).
#[must_use]
pub fn shelf_kind_catalog() -> Vec<ShelfKindInfo> {
    vec![
        kind("finish_series", "Finish these series"),
        kind("keep_listening", "Pick up where you left off"),
        kind("wish", "Because you wishlisted {Title}"),
        kind("author", "More from {Author}"),
        kind("because", "If you like {Author}"),
        kind("narrator", "Narrated by {Narrator}"),
        kind("genre", "Because you like {Genre}"),
        kind("starter_genre", "Popular in {Genre}"),
        kind("similar_taste", "Similar to books you finish"),
        kind("chirp_deals", "Chirp deals"),
        kind("from_store", "Picked for you on {Store}"),
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

/// How a shelf treats several titles of one series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeriesRule {
    /// At most [`SERIES_CAP`] titles per series; the rest wait for another shelf.
    Cap,
    /// Every title may stay, but series take turns so the top of the shelf
    /// shows several of them rather than one in a row.
    Interleave,
}

/// Partitions scored recommendations into Discover shelves.
///
/// Every title lands on one shelf. Shelves claim titles in the order they
/// are built: the series shelves, the subject's wishes, authors, narrators,
/// genres, starter genres, similar taste, Chirp deals, then one shelf per
/// store for whatever is left, each title under the store where it costs
/// the subject least (member price on stores they have an account with,
/// list price elsewhere, its own store when no price is known). Wished
/// titles are claimed first and shown last, on the wishlist shelf alone.
/// Shelves other than the series shelves keep at most [`SERIES_CAP`] titles
/// of one series; the series shelves rotate between series instead.
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
    let mut builder = FeedBuilder {
        recs,
        cap,
        disabled: disabled_shelves,
        claimed: HashSet::new(),
        shelves: Vec::new(),
    };

    // Wished titles belong on the wishlist shelf alone: claim them now and
    // add the shelf at the end. Wished titles the shelf or series cap left
    // off are claimed too, so they never reach another shelf.
    let wished = |r: &Recommendation| has_category(r, "requests") || r.from_request;
    let wishlist = builder.take(wished, SeriesRule::Cap);
    builder.claimed.extend(
        recs.iter()
            .filter(|r| wished(r))
            .map(recommendation_map_key),
    );

    builder.push(
        "finish_series",
        "Finish these series",
        Some("Gaps in series you already own"),
        |r| has_category(r, "finish_series"),
        SeriesRule::Interleave,
    );

    builder.push(
        "keep_listening",
        "Pick up where you left off",
        Some("Series you’re actively listening to"),
        |r| has_category(r, "keep_listening"),
        SeriesRule::Interleave,
    );

    // Titles the stores found from the subject's own wishes, newest wish first.
    for wish in taste.wish_titles.iter().take(WISH_SHELVES) {
        let id = format!("wish:{}", slugish(&wish.to_lowercase()));
        builder.push(
            &id,
            &format!("Because you wishlisted {wish}"),
            Some("Related to a title on your wishlist"),
            |r| seeded_by_wish(r, wish),
            SeriesRule::Cap,
        );
    }

    // More from top liked authors.
    let mut authors: Vec<_> = taste.authors.values().cloned().collect();
    authors.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (display, _) in authors.into_iter().take(AUTHOR_SHELVES) {
        let key = display.to_lowercase();
        let id = format!("author:{}", slugish(&key));
        let title = format!("More from {display}");
        builder.push(
            &id,
            &title,
            Some("Because you’ve liked their work"),
            |r| has_category(r, "author") && names_include(r.authors.as_deref(), &display),
            SeriesRule::Cap,
        );
    }

    // “If you like X, try these” — related/similar hits that are not by X.
    let mut because_authors: Vec<_> = taste.authors.values().cloned().collect();
    because_authors.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (display, _) in because_authors.into_iter().take(BECAUSE_SHELVES) {
        let id = format!("because:{}", slugish(&display.to_lowercase()));
        let title = format!("If you like {display}");
        builder.push(
            &id,
            &title,
            Some("Related titles and similar tastes — new voices"),
            |r| because_you_like(r, &display, taste),
            SeriesRule::Cap,
        );
    }

    // Narrators you love.
    let mut narrators: Vec<_> = taste.narrators.values().cloned().collect();
    narrators.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((display, _)) = narrators.into_iter().next() {
        let id = format!("narrator:{}", slugish(&display.to_lowercase()));
        builder.push(
            &id,
            &format!("Narrated by {display}"),
            Some("More from a narrator you enjoy"),
            |r| has_category(r, "narrator") && names_include(r.narrators.as_deref(), &display),
            SeriesRule::Cap,
        );
    }

    // Genre / subject shelves from local taste categories.
    let mut categories: Vec<_> = taste.categories.values().cloned().collect();
    categories.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (display, _) in categories.into_iter().take(GENRE_SHELVES) {
        let id = format!("genre:{}", slugish(&display.to_lowercase()));
        builder.push(
            &id,
            &format!("Because you like {display}"),
            Some("Genre and subject overlap from titles you’ve finished"),
            |r| category_overlap(r, &display),
            SeriesRule::Cap,
        );
    }

    builder.push(
        "similar_taste",
        "Similar to books you finish",
        Some("Embedding / taste overlap"),
        |r| has_category(r, "similar_taste"),
        SeriesRule::Cap,
    );

    // Starter genres, browsed while the taste was thin: one shelf per genre,
    // in the order the stores were asked.
    for genre in starter_genres(recs) {
        let id = format!("starter_genre:{}", slugish(&genre.to_lowercase()));
        builder.push(
            &id,
            &format!("Popular in {genre}"),
            Some("Popular titles while Discover learns your taste"),
            |r| {
                r.starter_genre
                    .as_deref()
                    .is_some_and(|g| g.eq_ignore_ascii_case(&genre))
            },
            SeriesRule::Cap,
        );
    }

    builder.push(
        "chirp_deals",
        "Chirp deals right now",
        Some("Top and free deals from Chirp"),
        |r| has_category(r, "chirp_deals"),
        SeriesRule::Cap,
    );

    // Whatever is left goes under the store where it costs the subject
    // least; the stores already in the library come first.
    let placed: HashMap<String, String> = recs
        .iter()
        .filter(|r| !builder.claimed.contains(&recommendation_map_key(r)))
        .filter_map(|r| {
            best_store_for(r, &taste.linked_sources).map(|s| (recommendation_map_key(r), s))
        })
        .collect();
    let mut sources: Vec<String> = placed.values().cloned().collect();
    sources.sort();
    sources.dedup();
    sources.sort_by_key(|s| !taste.owned_sources.contains(s));
    for source in sources {
        let display = store_display_name(&source);
        let id = format!("from_{source}");
        builder.push(
            &id,
            &format!("Picked for you on {display}"),
            Some("Ranked by your taste, shown where it costs least"),
            |r| placed.get(&recommendation_map_key(r)) == Some(&source),
            SeriesRule::Cap,
        );
    }

    builder.push_items(
        "requests",
        "On the wishlist",
        Some("Titles people have wishlisted"),
        wishlist,
    );

    let FeedBuilder { mut shelves, .. } = builder;
    // Keep at least a "Top picks" dump if everything is empty but recs exist.
    // Wished titles stay off it: they belong to the wishlist shelf alone,
    // even when that shelf is hidden.
    let mut top: Vec<Recommendation> = if shelves.is_empty() {
        recs.iter().filter(|r| !wished(r)).cloned().collect()
    } else {
        Vec::new()
    };
    if !top.is_empty() && !shelf_is_disabled("top_picks", disabled_shelves) {
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
        state: None,
        computed_at: None,
    }
}

/// Builds the shelves of one feed, claiming each title for the first shelf
/// that takes it.
struct FeedBuilder<'a> {
    /// Every scored recommendation.
    recs: &'a [Recommendation],
    /// Most items per shelf.
    cap: usize,
    /// Shelf ids the subject hides.
    disabled: &'a [String],
    /// Map keys of the titles a shelf has already taken.
    claimed: HashSet<String>,
    /// The shelves built so far, in page order.
    shelves: Vec<DiscoverShelf>,
}

impl FeedBuilder<'_> {
    /// Takes the best unclaimed titles that satisfy `pred`, at most `cap`,
    /// and claims them.
    fn take(
        &mut self,
        pred: impl Fn(&Recommendation) -> bool,
        rule: SeriesRule,
    ) -> Vec<Recommendation> {
        let mut sorted: Vec<&Recommendation> = self
            .recs
            .iter()
            .filter(|r| pred(r) && !self.claimed.contains(&recommendation_map_key(r)))
            .collect();
        sorted.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // De-dupe by product key before any series rule, so a merged
        // duplicate does not use up a series' places.
        let mut seen = HashSet::new();
        sorted.retain(|r| seen.insert(recommendation_map_key(r)));
        let picked = match rule {
            SeriesRule::Cap => cap_series(&sorted, self.cap),
            SeriesRule::Interleave => interleave_series(&sorted, self.cap),
        };
        for r in &picked {
            self.claimed.insert(recommendation_map_key(r));
        }
        picked
    }

    /// Adds the shelf of the best unclaimed titles that satisfy `pred`,
    /// unless it is hidden or would be empty.
    fn push(
        &mut self,
        id: &str,
        title: &str,
        subtitle: Option<&str>,
        pred: impl Fn(&Recommendation) -> bool,
        rule: SeriesRule,
    ) {
        if shelf_is_disabled(id, self.disabled) {
            return;
        }
        let items = self.take(pred, rule);
        self.push_items(id, title, subtitle, items);
    }

    /// Adds a shelf of titles already taken, unless it is hidden or empty.
    fn push_items(
        &mut self,
        id: &str,
        title: &str,
        subtitle: Option<&str>,
        items: Vec<Recommendation>,
    ) {
        if items.is_empty() || shelf_is_disabled(id, self.disabled) {
            return;
        }
        self.shelves.push(DiscoverShelf {
            id: id.to_string(),
            title: title.to_string(),
            subtitle: subtitle.map(str::to_string),
            items,
        });
    }
}

/// The series a title belongs to, as a comparison key, when it names one.
fn series_key(r: &Recommendation) -> Option<String> {
    r.series
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

/// The first `cap` of `sorted` (best first) with at most [`SERIES_CAP`]
/// titles of one series; titles without a series are not limited.
fn cap_series(sorted: &[&Recommendation], cap: usize) -> Vec<Recommendation> {
    let mut per_series: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(cap.min(sorted.len()));
    for r in sorted {
        if out.len() >= cap {
            break;
        }
        if let Some(series) = series_key(r) {
            let n = per_series.entry(series).or_insert(0);
            if *n >= SERIES_CAP {
                continue;
            }
            *n += 1;
        }
        out.push((*r).clone());
    }
    out
}

/// Up to `cap` of `sorted` (best first), taking one title from each series
/// in turn so the shelf opens with several series rather than one.
///
/// Series are ordered by their best title; a title without a series is a
/// group of its own.
fn interleave_series(sorted: &[&Recommendation], cap: usize) -> Vec<Recommendation> {
    let mut groups: Vec<std::collections::VecDeque<&Recommendation>> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for r in sorted {
        match series_key(r) {
            Some(series) => {
                let at = *index.entry(series).or_insert_with(|| {
                    groups.push(std::collections::VecDeque::new());
                    groups.len() - 1
                });
                groups[at].push_back(*r);
            }
            None => groups.push(std::collections::VecDeque::from([*r])),
        }
    }
    let mut out = Vec::with_capacity(cap.min(sorted.len()));
    while out.len() < cap {
        let mut took = false;
        for group in &mut groups {
            if out.len() >= cap {
                break;
            }
            if let Some(r) = group.pop_front() {
                out.push(r.clone());
                took = true;
            }
        }
        if !took {
            break;
        }
    }
    out
}

/// True when the stores found `r` by expanding the wish `wish_title`, and
/// `r` is not that wished title itself.
fn seeded_by_wish(r: &Recommendation, wish_title: &str) -> bool {
    !r.from_request
        && r.seed_title
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(wish_title))
        && !crate::identity::works_match(&r.title, None, wish_title, None)
}

/// The starter genres among `recs`, in the order they first appear by score.
fn starter_genres(recs: &[Recommendation]) -> Vec<String> {
    let mut sorted: Vec<&Recommendation> =
        recs.iter().filter(|r| r.starter_genre.is_some()).collect();
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out: Vec<String> = Vec::new();
    for r in sorted {
        if let Some(genre) = r.starter_genre.as_deref() {
            if !out.iter().any(|g| g.eq_ignore_ascii_case(genre)) {
                out.push(genre.to_string());
            }
        }
    }
    out
}

/// The store whose shelf shows `r`: the priced edition that costs the
/// subject least (member price on `linked` stores, list price elsewhere),
/// else the store the card came from.
fn best_store_for(r: &Recommendation, linked: &HashSet<String>) -> Option<String> {
    let cheapest = r
        .purchase_hints
        .iter()
        .filter_map(|hint| {
            let source = hint.source.trim().to_ascii_lowercase();
            if source.is_empty() {
                return None;
            }
            let as_member = linked.contains(&source);
            crate::purchase::effective_price_cents(hint, as_member).map(|cents| (cents, source))
        })
        .min_by_key(|(cents, _)| *cents)
        .map(|(_, source)| source);
    cheapest.or_else(|| {
        r.candidate_source
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
    })
}

/// True when `names` (a delimited list of people) includes `name`.
fn names_include(names: Option<&str>, name: &str) -> bool {
    names
        .map(|list| {
            split_people(list)
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name))
        })
        .unwrap_or(false)
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

/// Fewest items a shelf keeps while [`encode_feed_within`] shrinks a feed.
const FIT_MIN_ITEMS: usize = 4;

/// Characters of a description kept once [`encode_feed_within`] has shrunk
/// every shelf to [`FIT_MIN_ITEMS`] and the feed is still too large.
const FIT_DESCRIPTION_CHARS: usize = 300;

/// Encodes `feed` as JSON that stays within `budget` bytes once stored,
/// shrinking it first.
///
/// A stored feed is one database cell, and the database plugins refuse a
/// cell or result over their `maxResultBytes` (256 KiB for the first-party
/// adapters). They count a row as JSON, where the feed is itself a JSON
/// string, so every quote and backslash in it takes two bytes and a control
/// character six. The budget is checked against that escaped form, which
/// is never shorter than the feed itself. Each round cuts every shelf to three quarters of the longest
/// shelf, down to [`FIT_MIN_ITEMS`]. Then descriptions are shortened to
/// [`FIT_DESCRIPTION_CHARS`]. Then shelves are dropped from the end. The
/// top of each shelf is kept, since shelves are sorted best first.
///
/// # Arguments
///
/// * `feed` - Feed to encode; trimmed in place when it is too large.
/// * `budget` - Most bytes the encoded feed may take as an escaped JSON
///   string.
///
/// # Returns
///
/// The encoded feed (unescaped). An empty feed is returned even when `budget` is
/// smaller than it.
///
/// # Errors
///
/// Returns the JSON encoding error.
pub fn encode_feed_within(
    feed: &mut DiscoverFeed,
    budget: usize,
) -> Result<String, serde_json::Error> {
    let mut descriptions_cut = false;
    loop {
        let json = serde_json::to_string(feed)?;
        if serde_json::to_string(&json)?.len() <= budget {
            return Ok(json);
        }
        let longest = feed
            .shelves
            .iter()
            .map(|s| s.items.len())
            .max()
            .unwrap_or(0);
        if longest > FIT_MIN_ITEMS {
            let cap = (longest * 3 / 4).max(FIT_MIN_ITEMS);
            for shelf in &mut feed.shelves {
                shelf.items.truncate(cap);
            }
            continue;
        }
        if !descriptions_cut {
            descriptions_cut = true;
            for item in feed.shelves.iter_mut().flat_map(|s| s.items.iter_mut()) {
                if let Some(text) = item.description.as_mut() {
                    if let Some((end, _)) = text.char_indices().nth(FIT_DESCRIPTION_CHARS) {
                        text.truncate(end);
                        text.push('…');
                    }
                }
            }
            continue;
        }
        if feed.shelves.pop().is_none() {
            return Ok(json);
        }
    }
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

    /// Shelf ids of `feed`, in page order.
    fn ids(feed: &DiscoverFeed) -> Vec<&str> {
        feed.shelves.iter().map(|s| s.id.as_str()).collect()
    }

    /// Titles on the shelf `id` of `feed` (empty when there is no such shelf).
    fn titles<'a>(feed: &'a DiscoverFeed, id: &str) -> Vec<&'a str> {
        feed.shelves
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.items.iter().map(|r| r.title.as_str()).collect())
            .unwrap_or_default()
    }

    /// A taste that likes Ada Author.
    fn ada_taste() -> ShelfTaste {
        let mut taste = ShelfTaste::default();
        taste
            .authors
            .insert("ada author".into(), ("Ada Author".into(), 5.0));
        taste
    }

    #[test]
    fn every_title_lands_on_one_shelf() {
        let recs = vec![
            rec("By Ada", 9.0, &["author", "from_store"]),
            rec("Only Store", 3.0, &["from_store"]),
        ];
        let feed = build_discover_feed(&recs, &ada_taste(), 8, &[]);
        assert_eq!(ids(&feed), vec!["author:ada-author", "from_audible"]);
        assert_eq!(titles(&feed, "author:ada-author"), vec!["By Ada"]);
        // The author shelf claimed "By Ada", so the store shelf has the rest.
        assert_eq!(titles(&feed, "from_audible"), vec!["Only Store"]);
    }

    #[test]
    fn a_series_fills_at_most_two_places_off_the_series_shelf() {
        let recs: Vec<Recommendation> = (0..5)
            .map(|i| {
                rec(
                    &format!("Dune {i}"),
                    10.0 - f64::from(i),
                    &["author", "from_store"],
                )
            })
            .collect();
        let feed = build_discover_feed(&recs, &ada_taste(), 8, &[]);
        assert_eq!(
            titles(&feed, "author:ada-author"),
            vec!["Dune 0", "Dune 1"],
            "two of the series on the author shelf"
        );
        assert_eq!(
            titles(&feed, "from_audible"),
            vec!["Dune 2", "Dune 3"],
            "the store shelf takes the next two"
        );
    }

    #[test]
    fn the_series_shelf_rotates_between_series() {
        let mut recs: Vec<Recommendation> = (0..3)
            .map(|i| rec(&format!("A{i}"), 10.0 - f64::from(i), &["finish_series"]))
            .collect();
        for i in 0..2 {
            let mut r = rec(&format!("B{i}"), 5.0 - f64::from(i), &["finish_series"]);
            r.series = Some("Other Series".into());
            recs.push(r);
        }
        let feed = build_discover_feed(&recs, &ShelfTaste::default(), 8, &[]);
        assert_eq!(
            titles(&feed, "finish_series"),
            vec!["A0", "B0", "A1", "B1", "A2"]
        );
    }

    #[test]
    fn a_wish_names_its_own_shelf_without_the_wished_title() {
        let taste = ShelfTaste {
            wish_titles: vec!["Dune".into()],
            ..ShelfTaste::default()
        };
        let seeded = |title: &str| Recommendation {
            seed_title: Some("Dune".into()),
            ..rec(title, 4.0, &["because", "from_store"])
        };
        let recs = vec![
            seeded("Dune Messiah"),
            seeded("Dune"),
            rec("Unrelated", 6.0, &["from_store"]),
        ];
        let feed = build_discover_feed(&recs, &taste, 8, &[]);
        assert_eq!(ids(&feed), vec!["wish:dune", "from_audible"]);
        assert_eq!(titles(&feed, "wish:dune"), vec!["Dune Messiah"]);
        // The wished title itself and the unrelated card go to the store.
        assert_eq!(titles(&feed, "from_audible"), vec!["Unrelated", "Dune"]);
    }

    #[test]
    fn wished_titles_show_only_on_the_wishlist_shelf() {
        let mut wished = rec("Wanted", 50.0, &["author", "requests"]);
        wished.from_request = true;
        let recs = vec![wished, rec("By Ada", 2.0, &["author"])];
        let feed = build_discover_feed(&recs, &ada_taste(), 8, &[]);
        assert_eq!(ids(&feed), vec!["author:ada-author", "requests"]);
        assert_eq!(titles(&feed, "author:ada-author"), vec!["By Ada"]);
        assert_eq!(titles(&feed, "requests"), vec!["Wanted"]);
    }

    #[test]
    fn a_hidden_wishlist_keeps_wished_titles_off_top_picks() {
        let wished = Recommendation {
            from_request: true,
            ..rec("Wanted", 50.0, &["requests"])
        };
        let hidden = vec![String::from("requests")];
        let feed = build_discover_feed(&[wished], &ShelfTaste::default(), 8, &hidden);
        assert!(feed.shelves.is_empty(), "{:?}", ids(&feed));
    }

    #[test]
    fn wished_titles_past_the_series_cap_stay_off_other_shelves() {
        let wished = |title: &str| Recommendation {
            from_request: true,
            ..rec(title, 50.0, &["author", "requests"])
        };
        let recs = vec![
            wished("Book One"),
            wished("Book Two"),
            wished("Book Three"),
            rec("By Ada", 2.0, &["author"]),
        ];
        let feed = build_discover_feed(&recs, &ada_taste(), 8, &[]);
        assert_eq!(titles(&feed, "requests").len(), SERIES_CAP);
        for shelf in feed.shelves.iter().filter(|s| s.id != "requests") {
            for item in &shelf.items {
                assert!(!item.from_request, "{} on {}", item.title, shelf.id);
            }
        }
    }

    #[test]
    fn starter_genres_get_a_shelf_each() {
        let browsed = |title: &str, genre: &str| Recommendation {
            starter_genre: Some(genre.into()),
            series: None,
            ..rec(title, 2.0, &["starter_genre", "from_store"])
        };
        let recs = vec![
            browsed("Hobbit", "Fantasy"),
            browsed("Gone Girl", "Mystery & Thriller"),
        ];
        let feed = build_discover_feed(&recs, &ShelfTaste::default(), 8, &[]);
        assert_eq!(
            ids(&feed),
            vec!["starter_genre:fantasy", "starter_genre:mystery-thriller"]
        );
        assert_eq!(feed.shelves[0].title, "Popular in Fantasy");
    }

    #[test]
    fn leftovers_go_to_the_store_where_they_cost_the_subject_least() {
        let priced = |source: &str, list: Option<i64>, price: Option<i64>, member: Option<i64>| {
            crate::purchase::PurchaseHint {
                source: source.into(),
                product_id: "p".into(),
                list_price_cents: list,
                price_cents: price,
                member_price_cents: member,
                ..crate::purchase::PurchaseHint::link(source, "p", None, None)
            }
        };
        let mut r = rec("Priced", 2.0, &["from_store"]);
        r.purchase_hints = vec![
            priced("audible", None, Some(1_500), Some(900)),
            priced("libro", Some(1_000), Some(500), Some(500)),
        ];
        let recs = vec![r];
        // Nobody is a member: list prices, so Libro's 10.00 beats 15.00.
        let feed = build_discover_feed(&recs, &ShelfTaste::default(), 8, &[]);
        assert_eq!(ids(&feed), vec!["from_libro"]);
        // An Audible member pays 9.00 there, under Libro's list price.
        let mut taste = ShelfTaste::default();
        taste.linked_sources.insert("audible".into());
        let feed = build_discover_feed(&recs, &taste, 8, &[]);
        assert_eq!(ids(&feed), vec!["from_audible"]);
        // No price known: the card's own store.
        let plain = vec![rec("Plain", 1.0, &["from_store"])];
        let feed = build_discover_feed(&plain, &ShelfTaste::default(), 8, &[]);
        assert_eq!(ids(&feed), vec!["from_audible"]);
    }

    /// A feed of many long items is cut down until it fits its budget,
    /// keeping the best item of every shelf.
    #[test]
    fn a_large_feed_is_trimmed_to_its_budget() {
        let recs: Vec<Recommendation> = (0..240)
            .map(|i| Recommendation {
                description: Some("d".repeat(1_200)),
                ..rec(&format!("Book{i:03}"), f64::from(i), &["finish_series"])
            })
            .collect();
        let mut feed = build_discover_feed(&recs, &ShelfTaste::default(), 48, &[]);
        let before = serde_json::to_string(&feed).unwrap().len();
        let budget = 20_000;
        assert!(before > budget, "fixture is only {before} bytes");
        let json = encode_feed_within(&mut feed, budget).unwrap();
        assert!(json.len() <= budget, "{} bytes", json.len());
        let shelf = feed.shelves.first().expect("a shelf survives");
        assert_eq!(shelf.items[0].title, "Book239");
        let decoded: DiscoverFeed = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.shelves.len(), feed.shelves.len());
    }

    /// When four items per shelf are still too large, long descriptions are
    /// shortened before any shelf is dropped.
    #[test]
    fn descriptions_are_shortened_before_shelves_are_dropped() {
        let recs: Vec<Recommendation> = (0..4)
            .map(|i| Recommendation {
                description: Some("é".repeat(2_000)),
                ..rec(&format!("Book{i}"), f64::from(i), &["finish_series"])
            })
            .collect();
        let mut feed = build_discover_feed(&recs, &ShelfTaste::default(), 8, &[]);
        let shelves = feed.shelves.len();
        let budget = 6_000;
        let json = encode_feed_within(&mut feed, budget).unwrap();
        assert!(json.len() <= budget, "{} bytes", json.len());
        assert_eq!(feed.shelves.len(), shelves);
        let text = feed.shelves[0].items[0].description.as_deref().unwrap();
        assert_eq!(text.chars().count(), FIT_DESCRIPTION_CHARS + 1);
        assert!(text.ends_with('…'));
    }

    /// A feed under budget is encoded unchanged, and an impossible budget
    /// still terminates with an empty feed.
    #[test]
    fn a_small_feed_is_kept_and_a_tiny_budget_empties_it() {
        let recs = vec![rec("Book1", 1.0, &["finish_series"])];
        let mut feed = build_discover_feed(&recs, &ShelfTaste::default(), 8, &[]);
        let whole = serde_json::to_string(&feed).unwrap();
        assert_eq!(encode_feed_within(&mut feed, 1 << 20).unwrap(), whole);
        let json = encode_feed_within(&mut feed, 1).unwrap();
        assert!(feed.shelves.is_empty());
        assert_eq!(json, serde_json::to_string(&feed).unwrap());
    }

    /// Builds a shelf of `n` items titled `<id>-<index>`, best first.
    fn shelf(id: &str, n: usize, description: &str) -> DiscoverShelf {
        DiscoverShelf {
            id: id.into(),
            title: id.into(),
            subtitle: None,
            items: (0..n)
                .map(|i| Recommendation {
                    description: Some(description.into()),
                    ..rec(&format!("{id}-{i}"), 0.0, &[])
                })
                .collect(),
        }
    }

    /// Trimming keeps every shelf's leading items, shortens descriptions on
    /// every shelf, and drops shelves from the end, never the front.
    #[test]
    fn several_shelves_are_trimmed_from_the_end() {
        let long = "d".repeat(2_000);
        let mut feed = DiscoverFeed {
            shelves: vec![
                shelf("a", 12, &long),
                shelf("b", 12, &long),
                shelf("c", 12, &long),
            ],
            shelf_kinds: Vec::new(),
            state: None,
            computed_at: None,
        };
        let json = encode_feed_within(&mut feed, 6_000).unwrap();
        assert!(json.len() <= 6_000, "{} bytes", json.len());
        let ids: Vec<&str> = feed.shelves.iter().map(|s| s.id.as_str()).collect();
        assert!(!ids.is_empty() && ids.len() < 3, "{ids:?}");
        assert_eq!(ids, ["a", "b", "c"][..ids.len()]);
        for s in &feed.shelves {
            assert_eq!(s.items.len(), FIT_MIN_ITEMS);
            assert_eq!(s.items[0].title, format!("{}-0", s.id));
            let text = s.items[0].description.as_deref().unwrap();
            assert_eq!(text.chars().count(), FIT_DESCRIPTION_CHARS + 1);
        }

        let mut roomy = DiscoverFeed {
            shelves: vec![
                shelf("a", 12, "x"),
                shelf("b", 12, "x"),
                shelf("c", 12, "x"),
            ],
            shelf_kinds: Vec::new(),
            state: None,
            computed_at: None,
        };
        let whole = serde_json::to_string(&roomy).unwrap().len();
        encode_feed_within(&mut roomy, whole * 2 / 3).unwrap();
        assert_eq!(roomy.shelves.len(), 3);
        for s in &roomy.shelves {
            assert!(s.items.len() < 12 && s.items.len() >= FIT_MIN_ITEMS);
            assert_eq!(s.items[0].title, format!("{}-0", s.id));
        }
    }

    /// The budget counts the feed as the database stores it, a JSON string,
    /// so quotes and backslashes count twice.
    #[test]
    fn the_budget_counts_escaped_bytes() {
        let mut feed = DiscoverFeed {
            shelves: vec![shelf("a", 8, &"\\\"\\".repeat(200))],
            shelf_kinds: Vec::new(),
            state: None,
            computed_at: None,
        };
        let budget = 16_000;
        let json = encode_feed_within(&mut feed, budget).unwrap();
        assert!(!feed.shelves.is_empty());
        let stored = serde_json::to_string(&json).unwrap();
        assert!(stored.len() <= budget, "{} bytes stored", stored.len());
        assert!(stored.len() > json.len() * 3 / 2);
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
        let recs = vec![
            Recommendation {
                seed_categories: Some("Mystery".into()),
                candidate_source: Some("libro".into()),
                categories: vec!["genre".into(), "from_store".into()],
                ..rec("MysteryBook", 15.0, &[])
            },
            // Matches no personal shelf, so the Libro shelf shows it.
            Recommendation {
                candidate_source: Some("libro".into()),
                series: None,
                ..rec("JustLibro", 2.0, &["from_store"])
            },
        ];
        let mut taste = ShelfTaste::default();
        taste
            .categories
            .insert("mystery".into(), ("Mystery".into(), 3.0));
        taste.owned_sources.insert("libro".into());

        let feed = build_discover_feed(&recs, &taste, 8, &[]);
        assert!(feed.shelves.iter().any(|s| s.id.starts_with("genre:")));
        assert!(feed.shelves.iter().any(|s| s.id == "from_libro"));
    }

    #[test]
    fn requests_shelf_keeps_wishers() {
        use bookclerk_library::QueueWisher;
        let mut r = rec("Wished Book", 12.0, &["requests"]);
        r.from_request = true;
        r.wish_count = 2;
        r.wishers = vec![
            QueueWisher {
                display_name: Some("Ada".into()),
                identity_id: Some(1),
                ..Default::default()
            },
            QueueWisher {
                display_name: Some("Bob".into()),
                identity_id: Some(2),
                ..Default::default()
            },
        ];
        let feed = build_discover_feed(&[r], &ShelfTaste::default(), 8, &[]);
        let shelf = feed
            .shelves
            .iter()
            .find(|s| s.id == "requests")
            .expect("requests shelf");
        assert_eq!(shelf.items[0].wishers.len(), 2);
        assert_eq!(shelf.items[0].wish_count, 2);
    }
}
