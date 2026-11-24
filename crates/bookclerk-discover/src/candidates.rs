//! Storefront candidate discovery (titles not yet owned).
//!
//! Seeds from local taste (finished / rated / listening), then expands via
//! registered [`ContentSource`] plugins only (`expand_candidates` / `list_deals`).
//!
//! Local embeddings and ownership filters evaluate those remote hits.

use std::collections::{HashMap, HashSet};

use bookclerk_library::{LibraryStore, TasteTitle};
use bookclerk_source::{CatalogHit, ExpandSeed, SourceRegistry};

use crate::error::Result;
use crate::identity::{candidate_map_key, merge_candidate_metadata, push_edition, StoreEdition};

/// A purchase candidate discovered from a storefront catalog (not owned locally).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StorefrontCandidate {
    /// Canonical storefront / plugin id (`audible`, `libro`, `chirp`, …).
    pub source: String,
    /// Storefront-native product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Display title as shown on the storefront or library card.
    pub title: String,
    /// Comma-separated author names when the storefront provides them.
    pub authors: Option<String>,
    /// Comma-separated narrator names when known.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a named series.
    pub series: Option<String>,
    /// Position within the series (e.g. `1`, `1.5`) when known.
    pub series_index: Option<String>,
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    pub asin: Option<String>,
    /// Canonical ISBN-13 (or ISBN-10 normalized) when published.
    pub isbn: Option<String>,
    /// Public cover image URL when a storefront provided one.
    #[serde(default)]
    pub cover_url: Option<String>,
    /// Categories/subjects copied from the taste seed that produced this hit.
    pub seed_categories: Option<String>,
    /// How this candidate was found (related-to seed, author search, …).
    pub origin: String,
    /// Owned-library title that seeded this related / similar candidate.
    pub seed_title: Option<String>,
    /// Starter genre this candidate was browsed from (display name), set
    /// only on hits of a starter-genre search for a thin taste.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starter_genre: Option<String>,
    /// Known storefront editions of this work (including the primary source).
    #[serde(default)]
    pub store_editions: Vec<StoreEdition>,
    /// Bibliographic extras from the storefront catalog payload (optional).
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Publisher or storefront synopsis; may be truncated for embeddings.
    #[serde(default)]
    pub description: Option<String>,
    /// Publisher imprint as reported by the catalog.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Audiobook runtime in whole minutes when known.
    #[serde(default)]
    pub length_minutes: Option<i64>,
    /// Publication date string from the catalog (ISO or storefront format).
    #[serde(default)]
    pub published_at: Option<String>,
    /// Genre / subject tags as a single delimited string when known.
    #[serde(default)]
    pub categories: Option<String>,
    /// BCP-47 / storefront language code (`en`, `de`, …) when known.
    #[serde(default)]
    pub language: Option<String>,
    /// List or deal price in integer cents of [`Self::currency`].
    #[serde(default)]
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for price fields (`USD`, `EUR`, …).
    #[serde(default)]
    pub currency: Option<String>,
    /// Human-readable price string from the storefront (e.g. `$14.95`).
    #[serde(default)]
    pub price_label: Option<String>,
    /// Community overall rating when a storefront provided one.
    #[serde(default)]
    pub rating_overall: Option<f64>,
    /// Number of ratings backing [`Self::rating_overall`] when known.
    #[serde(default)]
    pub rating_count: Option<i64>,
    /// Abridged flag when the storefront provided one.
    #[serde(default)]
    pub is_abridged: Option<bool>,
    /// Whether the storefront marks the edition explicit (adult) content;
    /// `None` when it did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_explicit: Option<bool>,
    /// The storefront's own 1-based search rank for this hit (its declared
    /// `CatalogHit.rankHint`); catalog search merges stores by it for
    /// relevance and popularity sorts. `None` when no store ranked it.
    #[serde(
        default,
        alias = "audible_rank",
        skip_serializing_if = "Option::is_none"
    )]
    pub store_rank: Option<u32>,
    /// The work the pool resolved this listing into, when the candidate was
    /// read from the shared listing pool. Lets the ranking drop titles the
    /// library holds by work identity without resolving the title again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    /// The expression (`expressions.id`) the pool resolved this listing
    /// into: two candidates with the same expression are the same recording
    /// sold by different stores, so one card carries both prices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression_id: Option<String>,
    /// The publisher each store listing merged into this row named, one
    /// entry per listing that named one, in arrival order. Empty until a
    /// second listing merges in; [`Self::publisher`] is then the name most
    /// of them agree on (see `merge_candidate_metadata`). Never sent.
    #[serde(skip)]
    pub publisher_votes: Vec<String>,
}

/// Options for storefront candidate expansion.
#[derive(Debug, Clone)]
pub struct CandidateFetchOptions {
    /// Marketplace / region code (`us`, `uk`, …) for catalog lookups.
    pub region: String,
    /// Max local seed titles to expand from (finished / rated first).
    pub seed_limit: usize,
    /// Cap remote HTTP calls across all storefronts.
    pub max_remote_calls: usize,
    /// Call [`bookclerk_source::ContentSource::expand_candidates`] on registered Audible.
    pub include_audible: bool,
    /// Call [`bookclerk_source::ContentSource::expand_candidates`] on registered Libro.fm.
    pub include_libro: bool,
    /// Call [`bookclerk_source::ContentSource::expand_candidates`] on registered Chirp.
    pub include_chirp: bool,
    /// Call [`bookclerk_source::ContentSource::expand_candidates`] on registered GraphicAudio.
    pub include_graphicaudio: bool,
    /// Fetch deals via [`bookclerk_source::ContentSource::list_deals`] on all registered sources.
    pub include_deals: bool,
    /// When true, drop GraphicAudio Magento series-set SKUs from candidates.
    /// Default is false (sets are kept).
    pub exclude_graphicaudio_series_sets: bool,
}

impl Default for CandidateFetchOptions {
    fn default() -> Self {
        Self {
            region: String::from("us"),
            seed_limit: 8,
            max_remote_calls: 32,
            include_audible: true,
            include_libro: true,
            include_chirp: true,
            include_graphicaudio: true,
            include_deals: true,
            exclude_graphicaudio_series_sets: false,
        }
    }
}

/// Expand storefront catalogs from local taste seeds; drop already-owned ids.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `registry` - Configured content-source or integration registry.
/// * `seeds` - `seeds` input for this call.
/// * `owned_asins` - String `owned_asins` for this call.
/// * `owned_isbns` - String `owned_isbns` for this call.
/// * `owned_product_keys` - String `owned_product_keys` for this call.
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `Vec<StorefrontCandidate>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn gather_storefront_candidates(
    _library: &LibraryStore,
    registry: &SourceRegistry,
    seeds: &[TasteTitle],
    owned_asins: &HashSet<String>,
    owned_isbns: &HashSet<String>,
    owned_product_keys: &HashSet<String>,
    opts: &CandidateFetchOptions,
) -> Result<Vec<StorefrontCandidate>> {
    let mut by_key: HashMap<String, StorefrontCandidate> = HashMap::new();
    let mut remote_calls = 0usize;

    let seeds: Vec<&TasteTitle> = seeds.iter().take(opts.seed_limit).collect();

    for seed in &seeds {
        if remote_calls >= opts.max_remote_calls {
            break;
        }

        let expand_seed = ExpandSeed {
            source: seed.source.clone(),
            product_id: seed.product_id.clone(),
            title: seed.title.clone(),
            authors: seed.authors.clone(),
            narrators: seed.narrators.clone(),
            series: seed.series.clone(),
            series_asin: seed.series_store_id.clone(),
            asin: seed.asin.clone(),
            isbn: seed.isbn.clone(),
            region: opts.region.clone(),
        };

        for source in registry.all() {
            if remote_calls >= opts.max_remote_calls {
                break;
            }
            let id = source.id();
            if !source_enabled(id, opts) {
                continue;
            }
            let hit_limit = opts.max_remote_calls.saturating_sub(remote_calls).min(24);
            match source.expand_candidates(&expand_seed, hit_limit).await {
                Ok(hits) => {
                    remote_calls += 1;
                    for hit in hits {
                        if opts.exclude_graphicaudio_series_sets
                            && id.eq_ignore_ascii_case("graphicaudio")
                            && looks_like_series_set(&hit.title)
                        {
                            continue;
                        }
                        insert_candidate(
                            &mut by_key,
                            apply_seed(hit_to_candidate(id, hit), seed),
                            owned_asins,
                            owned_isbns,
                            owned_product_keys,
                        );
                    }
                }
                Err(err) => {
                    remote_calls += 1;
                    tracing::debug!(source = %id, error = %err, "source expand_candidates failed");
                }
            }
        }
    }

    // Deals / promos (once per run; not per-seed).
    if opts.include_deals && remote_calls < opts.max_remote_calls {
        for source in registry.all() {
            if remote_calls >= opts.max_remote_calls {
                break;
            }
            let id = source.id();
            let deal_limit = opts.max_remote_calls.saturating_sub(remote_calls).min(32);
            match source.list_deals(deal_limit).await {
                Ok(hits) => {
                    remote_calls += 1;
                    for hit in hits {
                        insert_candidate(
                            &mut by_key,
                            hit_to_candidate(id, hit),
                            owned_asins,
                            owned_isbns,
                            owned_product_keys,
                        );
                    }
                }
                Err(err) => {
                    remote_calls += 1;
                    tracing::debug!(source = %id, error = %err, "source list_deals failed");
                }
            }
        }
    }

    tracing::info!(
        seeds = seeds.len(),
        remote_calls,
        candidates = by_key.len(),
        "gathered storefront recommendation candidates"
    );
    Ok(by_key.into_values().collect())
}

/// Whether this storefront id is included in the current candidate fetch options.
fn source_enabled(id: &str, opts: &CandidateFetchOptions) -> bool {
    if id.eq_ignore_ascii_case("audible") {
        return opts.include_audible;
    }
    if id.eq_ignore_ascii_case("libro") {
        return opts.include_libro;
    }
    if id.eq_ignore_ascii_case("chirp") {
        return opts.include_chirp;
    }
    if id.eq_ignore_ascii_case("graphicaudio") {
        return opts.include_graphicaudio;
    }
    true
}

/// Projects a catalog hit into a [`StorefrontCandidate`] (HTML entities decoded).
pub(crate) fn hit_to_candidate(source_id: &str, hit: CatalogHit) -> StorefrontCandidate {
    let hit = hit.decode_html_entities();
    let (authors, narrators) = (hit.authors(), hit.narrators());
    let (series, series_index) = (hit.series(), hit.series_index());
    let (asin, isbn) = (hit.asin(), hit.isbn());
    StorefrontCandidate {
        source: source_id.to_string(),
        product_id: hit.product_id,
        title: hit.title,
        authors,
        narrators,
        series,
        series_index,
        asin,
        isbn,
        cover_url: hit.cover_url,
        seed_categories: None,
        origin: hit.origin,
        seed_title: None,
        starter_genre: None,
        store_editions: Vec::new(),
        subtitle: hit.subtitle,
        description: hit.description,
        publisher: hit.publisher,
        length_minutes: hit.length_minutes,
        published_at: hit.published_at,
        categories: hit.categories,
        language: hit.language,
        price_cents: hit.price_cents,
        currency: hit.currency,
        price_label: hit.price_label,
        rating_overall: hit.rating_overall,
        rating_count: hit.rating_count,
        is_abridged: hit.is_abridged,
        is_explicit: hit.is_explicit,
        store_rank: None,
        work_id: None,
        expression_id: None,
        publisher_votes: Vec::new(),
    }
}

/// True when the title looks like a boxed series set (usually dropped from recs).
pub(crate) fn looks_like_series_set(title: &str) -> bool {
    let title = title.to_ascii_lowercase();
    title.contains("series set") || title.ends_with(" set")
}

/// One taste seed for storefront expansion: a book the subject has, or a
/// title on their wishlist.
#[derive(Debug, Clone)]
pub struct SeedTitle {
    /// What the storefront is asked about.
    pub seed: ExpandSeed,
    /// The seed's genres or subjects, copied onto candidates that lack their own.
    pub categories: Option<String>,
}

impl SeedTitle {
    /// A seed from a library book.
    #[must_use]
    pub fn from_book(book: &TasteTitle, region: &str) -> Self {
        Self {
            seed: ExpandSeed {
                source: book.source.clone(),
                product_id: book.product_id.clone(),
                title: book.title.clone(),
                authors: book.authors.clone(),
                narrators: book.narrators.clone(),
                series: book.series.clone(),
                series_asin: book.series_store_id.clone(),
                asin: book.asin.clone(),
                isbn: book.isbn.clone(),
                region: region.to_string(),
            },
            categories: book
                .categories
                .as_ref()
                .or(book.subjects.as_ref())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }

    /// A seed from an open wishlist title (no store of its own; stores
    /// expand it by title, author and identifiers).
    #[must_use]
    pub fn from_wish(
        title: &str,
        authors: Option<String>,
        asin: Option<String>,
        isbn: Option<String>,
    ) -> Self {
        Self {
            seed: ExpandSeed {
                source: String::from("wishlist"),
                product_id: asin.clone().or_else(|| isbn.clone()).unwrap_or_default(),
                title: title.to_string(),
                authors,
                narrators: None,
                series: None,
                series_asin: None,
                asin,
                isbn,
                region: String::from("us"),
            },
            categories: None,
        }
    }
}

/// Copies a [`SeedTitle`]'s title and categories onto a candidate when still empty.
pub(crate) fn apply_seed_title(
    mut c: StorefrontCandidate,
    seed: &SeedTitle,
) -> StorefrontCandidate {
    if c.seed_title.is_none() {
        c.seed_title = Some(seed.seed.title.clone());
    }
    if c.seed_categories.is_none() {
        c.seed_categories.clone_from(&seed.categories);
    }
    c
}

/// Copies seed title/categories onto a candidate when those fields are still empty.
fn apply_seed(mut c: StorefrontCandidate, seed: &TasteTitle) -> StorefrontCandidate {
    if c.seed_title.is_none() {
        c.seed_title = Some(seed.title.clone());
    }
    if c.seed_categories.is_none() {
        c.seed_categories = seed
            .categories
            .as_ref()
            .or(seed.subjects.as_ref())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    c
}

/// Dedupes comma/semicolon/pipe-separated category tokens into `into`.
fn merge_category_strings(into: &mut Option<String>, extra: Option<&str>) {
    let Some(extra) = extra.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    match into {
        None => *into = Some(extra.to_string()),
        Some(existing) => {
            let mut parts: Vec<String> = existing
                .split([',', ';', '|'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            for part in extra.split([',', ';', '|']) {
                let t = part.trim();
                if t.is_empty() {
                    continue;
                }
                if !parts.iter().any(|p| p.eq_ignore_ascii_case(t)) {
                    parts.push(t.to_string());
                }
            }
            *existing = parts.join("; ");
        }
    }
}

/// True when the library already holds the candidate's title, by ASIN,
/// canonical ISBN or `source:product` key.
///
/// # Arguments
///
/// * `c` - The candidate.
/// * `owned_asins` - Upper-cased ASINs the library holds.
/// * `owned_isbns` - ISBNs the library holds (canonical and as stored).
/// * `owned_product_keys` - `source:product` keys and product ids it holds.
pub(crate) fn candidate_is_owned(
    c: &StorefrontCandidate,
    owned_asins: &HashSet<String>,
    owned_isbns: &HashSet<String>,
    owned_product_keys: &HashSet<String>,
) -> bool {
    if let Some(asin) = c.asin.as_deref() {
        if owned_asins.contains(&asin.to_ascii_uppercase()) {
            return true;
        }
    }
    if let Some(isbn) = c.isbn.as_deref() {
        let norm = bookclerk_enrich::canonicalize_isbn(isbn);
        if owned_isbns.contains(isbn) || (!norm.is_empty() && owned_isbns.contains(&norm)) {
            return true;
        }
    }
    let source_key = format!("{}:{}", c.source, c.product_id);
    owned_product_keys.contains(&source_key)
        || owned_asins.contains(&c.product_id.to_ascii_uppercase())
        || owned_isbns.contains(&c.product_id)
        || owned_product_keys.contains(&c.product_id)
}

/// Inserts a candidate unless its ASIN, ISBN, or source:product id is already owned.
pub(crate) fn insert_candidate(
    map: &mut HashMap<String, StorefrontCandidate>,
    mut c: StorefrontCandidate,
    owned_asins: &HashSet<String>,
    owned_isbns: &HashSet<String>,
    owned_product_keys: &HashSet<String>,
) {
    if candidate_is_owned(&c, owned_asins, owned_isbns, owned_product_keys) {
        return;
    }
    if let Some(isbn) = c.isbn.as_deref() {
        let norm = bookclerk_enrich::canonicalize_isbn(isbn);
        if !norm.is_empty() {
            c.isbn = Some(norm);
        }
    }

    push_edition(
        &mut c.store_editions,
        StoreEdition::new(&c.source, &c.product_id),
    );

    let match_key = same_recording_key(map, &c);

    if let Some(old_key) = match_key {
        let mut existing = map.remove(&old_key).expect("just found");
        merge_candidate_metadata(&mut existing, &c);
        merge_category_strings(&mut existing.seed_categories, c.seed_categories.as_deref());
        // Prefer keeping the incoming product as primary when it carries ISBN.
        if c.isbn.is_some() && existing.isbn.is_some() && c.source == "libro" {
            existing.source = c.source.clone();
            existing.product_id = c.product_id.clone();
        }
        insert_keyed(map, existing);
        return;
    }

    insert_keyed(map, c);
}

/// Most minutes two stores' runtimes for one recording may differ by.
///
/// Stores round a recording's length differently (Audible to the nearest
/// minute, Chirp down from milliseconds, Libro.fm from seconds), so the
/// same files read 370 and 370.5 minutes: rounding alone moves a runtime
/// by less than one minute. Two recordings of one book by the same
/// narrator, such as an abridgement, differ by hours.
pub(crate) const RUNTIME_TOLERANCE_MINUTES: i64 = 1;

/// The key of the candidate in `map` that lists the same recording as `c`.
///
/// Two listings are one recording when the shared listing pool resolved
/// both into the same expression, when they share an ISBN or ASIN, or when
/// they are the same store product. Failing all of those, two listings are
/// one recording when [`same_recording_by_runtime`]
/// holds: same book, same narrators, matching runtime and nothing that
/// tells them apart. A shared identifier always wins over a runtime match.
///
/// # Arguments
///
/// * `map` - Candidates by key.
/// * `c` - The incoming candidate, its ISBN already canonical.
///
/// # Returns
///
/// The matching candidate's key, or `None` when `c` is a recording `map`
/// does not hold yet.
pub(crate) fn same_recording_key(
    map: &HashMap<String, StorefrontCandidate>,
    c: &StorefrontCandidate,
) -> Option<String> {
    // ISBN and ASIN are compared each on their own: a listing with both
    // and one with only the ASIN are still one recording.
    let (c_isbn, c_asin) = (isbn_of(c), asin_of(c));
    map.iter()
        .find_map(|(key, existing)| {
            let shares =
                |mine: &Option<String>, theirs: Option<String>| mine.is_some() && *mine == theirs;
            if shares(&c_isbn, isbn_of(existing)) || shares(&c_asin, asin_of(existing)) {
                return Some(key.clone());
            }
            let same_listing = c.source.eq_ignore_ascii_case(&existing.source)
                && c.product_id.eq_ignore_ascii_case(&existing.product_id);
            let same_expression = matches!(
                (c.expression_id.as_deref(), existing.expression_id.as_deref()),
                (Some(a), Some(b)) if !a.trim().is_empty() && a == b
            );
            (same_listing || same_expression).then(|| key.clone())
        })
        .or_else(|| {
            map.iter()
                .find(|(_, existing)| same_recording_by_runtime(existing, c))
                .map(|(key, _)| key.clone())
        })
}

/// The candidate's ISBN, canonical, when it has one.
fn isbn_of(c: &StorefrontCandidate) -> Option<String> {
    c.isbn
        .as_deref()
        .map(bookclerk_enrich::canonicalize_isbn)
        .filter(|s| !s.is_empty())
}

/// The candidate's ASIN, upper-cased, when it has one.
fn asin_of(c: &StorefrontCandidate) -> Option<String> {
    c.asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_uppercase)
}

/// Whether two values that are both known differ.
fn known_and_different<T: PartialEq>(a: Option<T>, b: Option<T>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a != b)
}

/// The candidate's subtitle, cleaned for comparison, when it has one.
fn subtitle_key(c: &StorefrontCandidate) -> Option<String> {
    c.subtitle
        .as_deref()
        .map(|s| bookclerk_resolve::normalize::clean_title(s, true))
        .filter(|s| !s.is_empty())
}

/// Sorted, cleaned names from a display string of names, joined; empty
/// when it names nobody.
fn name_set_key(display: Option<&str>) -> String {
    let set: std::collections::BTreeSet<String> =
        bookclerk_resolve::names::split_display_names(display.unwrap_or(""))
            .iter()
            .map(|n| bookclerk_resolve::normalize::clean_author(n))
            .filter(|n| !n.is_empty())
            .collect();
    set.into_iter().collect::<Vec<_>>().join("\u{1e}")
}

/// Whether two listings are one recording though they share no
/// identifier.
///
/// Stores rarely share a record number: Chirp lists no ISBN and Audible
/// gives one only for some titles. One store can also sell one recording
/// as two products, when two distributors deliver it (Chirp lists Cyril
/// Taylor-Carr's *Aleister Crowley* from both Icon Audio Arts and Author's
/// Republic, 2 seconds apart). Listings are one recording when all of
/// these hold:
///
/// * both report a runtime, within [`RUNTIME_TOLERANCE_MINUTES`];
/// * they name the same work: the resolver's work key (cleaned title
///   without edition markers such as ` - Abridged`, and authors) matches;
/// * they credit the same, non-empty set of narrators;
/// * nothing tells them apart: no two different ISBNs or ASINs, no
///   abridged against unabridged, no two different languages, and no two
///   different expressions in the shared listing pool;
/// * when one store lists both (a row already merged from several stores
///   counts as each of them), no two different subtitles either. Stores
///   word a subtitle differently, so this is checked only within a store.
///
/// # Arguments
///
/// * `a` - One listing.
/// * `b` - The other listing.
///
/// # Returns
///
/// `true` when the two listings are one recording.
pub(crate) fn same_recording_by_runtime(a: &StorefrontCandidate, b: &StorefrontCandidate) -> bool {
    // A merged row lists every store it holds in its store editions.
    let stores = |c: &StorefrontCandidate| -> HashSet<String> {
        c.store_editions
            .iter()
            .map(|e| e.source.to_ascii_lowercase())
            .chain(std::iter::once(c.source.to_ascii_lowercase()))
            .collect()
    };
    let one_store = !stores(a).is_disjoint(&stores(b));
    let (Some(len_a), Some(len_b)) = (a.length_minutes, b.length_minutes) else {
        return false;
    };
    if (len_a - len_b).abs() > RUNTIME_TOLERANCE_MINUTES {
        return false;
    }
    let language = |c: &StorefrontCandidate| {
        c.language
            .as_deref()
            .and_then(bookclerk_resolve::normalize_language)
    };
    let expression = |c: &StorefrontCandidate| {
        c.expression_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if known_and_different(isbn_of(a), isbn_of(b))
        || known_and_different(asin_of(a), asin_of(b))
        || known_and_different(a.is_abridged, b.is_abridged)
        || known_and_different(language(a), language(b))
        || known_and_different(expression(a), expression(b))
        || (one_store && known_and_different(subtitle_key(a), subtitle_key(b)))
    {
        return false;
    }
    let work_key = |c: &StorefrontCandidate| {
        let authors =
            bookclerk_resolve::names::split_display_names(c.authors.as_deref().unwrap_or(""));
        bookclerk_resolve::work_match_key(&c.title, authors.iter().map(String::as_str))
    };
    let work = work_key(a);
    let narrators = name_set_key(a.narrators.as_deref());
    !work.is_empty()
        && work == work_key(b)
        && !narrators.is_empty()
        && narrators == name_set_key(b.narrators.as_deref())
}

/// Inserts `c` under its map key, kept apart from another candidate
/// already stored under the same key.
///
/// A key without an ISBN or ASIN names only the title and authors, and two
/// listings with the same title are not merged on it (they may be different
/// recordings, or different languages), so the later one is qualified with
/// its store and product instead of replacing the first. Both a new
/// candidate and a merged one are reinserted through here.
///
/// # Arguments
///
/// * `map` - Candidates by key; `c` must not be in it.
/// * `c` - The candidate to store.
pub(crate) fn insert_keyed(map: &mut HashMap<String, StorefrontCandidate>, c: StorefrontCandidate) {
    let mut key = candidate_map_key(&c);
    if map.contains_key(&key) {
        key = format!("{key}|{}:{}", c.source, c.product_id);
    }
    map.insert(key, c);
}

/// Pick local seed books for storefront expansion (finished / high-rated first).
///
/// # Arguments
///
/// * `books` - `books` input for this call.
/// * `listening_engagement_by_uuid` - String `listening_engagement_by_uuid` for this call.
///
/// # Returns
///
/// Collected results (may be empty).
#[must_use]
pub fn select_taste_seeds(
    books: &[TasteTitle],
    listening_engagement_by_uuid: &HashMap<String, f64>,
) -> Vec<TasteTitle> {
    let mut scored: Vec<(i32, &TasteTitle)> = books
        .iter()
        // Podcasts are out of v1 Discover (no expand / suggestion seeds).
        .filter(|b| !b.is_podcast)
        .map(|b| {
            let mut s = 0;
            if b.is_finished {
                s += 50;
            }
            if let Some(w) = listening_engagement_by_uuid.get(&b.work_id) {
                // Continuous hours-weighted engagement → up to ~+40 seed priority.
                s += ((*w / 6.0) * 40.0).round() as i32;
            }
            if let Some(r) = b.rating_overall {
                if r >= 4.0 {
                    s += 30;
                } else if r >= 3.0 {
                    s += 10;
                }
            }
            if b.isbn.is_some() {
                s += 5;
            }
            if b.asin.is_some() {
                s += 3;
            }
            // Prefer seeds that unlock Chirp / GA / Audible series ASIN expansion.
            if b.source == "chirp" || b.source == "graphicaudio" {
                s += 8;
            }
            if b.series_store_id.is_some() {
                s += 6;
            }
            if b.series.is_some() {
                s += 2;
            }
            (s, b)
        })
        .filter(|(s, _)| *s > 0)
        .collect();
    scored.sort_by_key(|b| std::cmp::Reverse(b.0));
    scored.into_iter().map(|(_, b)| b.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wish_seeds_by_its_identifiers_and_lends_its_title() {
        let wish = SeedTitle::from_wish(
            "Dune",
            Some("Frank Herbert".into()),
            None,
            Some("9780441172719".into()),
        );
        assert_eq!(wish.seed.source, "wishlist");
        assert_eq!(wish.seed.product_id, "9780441172719");
        assert_eq!(wish.seed.authors.as_deref(), Some("Frank Herbert"));
        let by_asin = SeedTitle::from_wish("Dune", None, Some("B002".into()), Some("978".into()));
        assert_eq!(by_asin.seed.product_id, "B002");

        let hit = hit_to_candidate(
            "libro",
            CatalogHit {
                product_id: String::from("p9"),
                title: String::from("Children of Dune"),
                work_kind: bookclerk_source::records::WorkKind::Book,
                expression_kind: bookclerk_source::records::ExpressionKind::Narration,
                media_kind: bookclerk_source::records::MediaKind::Audio,
                ..CatalogHit::default()
            },
        );
        let seeded = apply_seed_title(hit, &wish);
        assert_eq!(seeded.seed_title.as_deref(), Some("Dune"));
        // A title the candidate already carries is kept.
        let other = SeedTitle::from_wish("Emma", None, None, None);
        assert_eq!(
            apply_seed_title(seeded, &other).seed_title.as_deref(),
            Some("Dune")
        );
    }

    #[test]
    fn insert_candidate_consolidates_one_recording_across_stores() {
        // The pool resolved all three listings into one expression (one
        // recording), though only two share no identifier at all.
        let mut map = HashMap::new();
        let owned_asins = HashSet::new();
        let owned_isbns = HashSet::new();
        let owned_products = HashSet::new();

        insert_candidate(
            &mut map,
            StorefrontCandidate {
                source: "audible".into(),
                product_id: "B00HAIL".into(),
                title: "Project Hail Mary".into(),
                authors: Some("Andy Weir".into()),
                narrators: None,
                series: None,
                series_index: None,
                asin: Some("B00HAIL".into()),
                isbn: None,
                cover_url: None,
                seed_categories: None,
                origin: "audible author".into(),
                seed_title: None,
                starter_genre: None,
                store_editions: Vec::new(),
                subtitle: None,
                description: None,
                publisher: None,
                length_minutes: None,
                published_at: None,
                categories: None,
                language: None,
                price_cents: None,
                currency: None,
                price_label: None,
                rating_overall: None,
                rating_count: None,
                is_abridged: None,
                is_explicit: None,
                store_rank: None,
                work_id: None,
                expression_id: Some("x-hail".into()),
                publisher_votes: Vec::new(),
            },
            &owned_asins,
            &owned_isbns,
            &owned_products,
        );
        insert_candidate(
            &mut map,
            StorefrontCandidate {
                source: "libro".into(),
                product_id: "9781234567890".into(),
                title: "Project Hail Mary: A Novel".into(),
                authors: Some("Andy Weir".into()),
                narrators: None,
                series: None,
                series_index: None,
                asin: None,
                isbn: Some("978-1234567890".into()),
                cover_url: None,
                seed_categories: None,
                origin: "libro related".into(),
                seed_title: None,
                starter_genre: None,
                store_editions: Vec::new(),
                subtitle: None,
                description: None,
                publisher: None,
                length_minutes: None,
                published_at: None,
                categories: None,
                language: None,
                price_cents: None,
                currency: None,
                price_label: None,
                rating_overall: None,
                rating_count: None,
                is_abridged: None,
                is_explicit: None,
                store_rank: None,
                work_id: None,
                expression_id: Some("x-hail".into()),
                publisher_votes: Vec::new(),
            },
            &owned_asins,
            &owned_isbns,
            &owned_products,
        );
        insert_candidate(
            &mut map,
            StorefrontCandidate {
                source: "chirp".into(),
                product_id: "999".into(),
                title: "Project Hail Mary".into(),
                authors: Some("Andy Weir".into()),
                narrators: None,
                series: None,
                series_index: None,
                asin: None,
                isbn: None,
                cover_url: None,
                seed_categories: None,
                origin: "chirp search".into(),
                seed_title: None,
                starter_genre: None,
                store_editions: Vec::new(),
                subtitle: None,
                description: None,
                publisher: None,
                length_minutes: None,
                published_at: None,
                categories: None,
                language: None,
                price_cents: None,
                currency: None,
                price_label: None,
                rating_overall: None,
                rating_count: None,
                is_abridged: None,
                is_explicit: None,
                store_rank: None,
                work_id: None,
                expression_id: Some("x-hail".into()),
                publisher_votes: Vec::new(),
            },
            &owned_asins,
            &owned_isbns,
            &owned_products,
        );

        assert_eq!(
            map.len(),
            1,
            "expected one consolidated work, got {:?}",
            map.keys()
        );
        let c = map.values().next().unwrap();
        assert_eq!(c.asin.as_deref(), Some("B00HAIL"));
        assert_eq!(c.isbn.as_deref(), Some("9781234567890"));
        assert_eq!(c.store_editions.len(), 3);
    }

    /// Two listings with the same title and author but no shared
    /// identifier or expression (a narration and a full-cast
    /// dramatisation) stay two candidates.
    #[test]
    fn same_title_without_shared_identity_stays_apart() {
        let listing = |source: &str, product_id: &str, narrators: &str| StorefrontCandidate {
            source: source.into(),
            product_id: product_id.into(),
            title: "The Sunlit Man".into(),
            authors: Some("Brandon Sanderson".into()),
            narrators: Some(narrators.into()),
            series: None,
            series_index: None,
            asin: None,
            isbn: None,
            cover_url: None,
            seed_categories: None,
            origin: "search".into(),
            seed_title: None,
            starter_genre: None,
            store_editions: Vec::new(),
            subtitle: None,
            description: None,
            publisher: None,
            length_minutes: None,
            published_at: None,
            categories: None,
            language: None,
            price_cents: None,
            currency: None,
            price_label: None,
            rating_overall: None,
            rating_count: None,
            is_abridged: None,
            is_explicit: None,
            store_rank: None,
            work_id: Some("w-sunlit".into()),
            expression_id: Some(format!("x-{product_id}")),
            publisher_votes: Vec::new(),
        };
        let none = HashSet::new();
        let mut map = HashMap::new();
        insert_candidate(
            &mut map,
            listing("chirp", "ga-1", "Full cast"),
            &none,
            &none,
            &none,
        );
        insert_candidate(
            &mut map,
            listing("libro", "li-1", "William DeMeritt"),
            &none,
            &none,
            &none,
        );
        assert_eq!(map.len(), 2, "{:?}", map.keys());
    }

    #[test]
    fn editions_in_different_languages_stay_separate() {
        let edition = |product_id: &str, language: Option<&str>| StorefrontCandidate {
            source: "chirp".into(),
            product_id: product_id.into(),
            title: "Dune".into(),
            authors: Some("Frank Herbert".into()),
            narrators: None,
            series: None,
            series_index: None,
            asin: None,
            isbn: None,
            cover_url: None,
            seed_categories: None,
            origin: "chirp search".into(),
            seed_title: None,
            starter_genre: None,
            store_editions: Vec::new(),
            subtitle: None,
            description: None,
            publisher: None,
            length_minutes: None,
            published_at: None,
            categories: None,
            language: language.map(str::to_string),
            price_cents: None,
            currency: None,
            price_label: None,
            rating_overall: None,
            rating_count: None,
            is_abridged: None,
            is_explicit: None,
            store_rank: None,
            work_id: None,
            expression_id: language
                .filter(|l| *l == "Spanish")
                .map(|_| String::from("x-es")),
            publisher_votes: Vec::new(),
        };
        let none = HashSet::new();
        let mut map = HashMap::new();
        for c in [
            edition("en", Some("English")),
            edition("es", Some("Spanish")),
            edition("es-2", Some("Spanish")),
            edition("unknown", None),
        ] {
            insert_candidate(&mut map, c, &none, &none, &none);
        }
        // Same title and author, but only the two Spanish listings share an
        // expression: they merge, and the English one and the one with no
        // language stay their own candidates rather than replacing each
        // other under the shared title key.
        assert_eq!(map.len(), 3, "{:?}", map.keys());
        let languages: HashSet<Option<String>> = map.values().map(|c| c.language.clone()).collect();
        assert!(languages.contains(&Some("English".into())));
        assert!(languages.contains(&Some("Spanish".into())));
    }
}
