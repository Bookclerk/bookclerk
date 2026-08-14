//! Storefront candidate discovery (titles not yet owned).
//!
//! Seeds from local taste (finished / rated / listening), then expands via
//! registered [`ContentSource`] plugins only (`expand_candidates` / `list_deals`).
//!
//! Local embeddings and ownership filters evaluate those remote hits.

use std::collections::{HashMap, HashSet};

use bookclerk_library::{BookRecord, LibraryStore};
use bookclerk_source::{CatalogHit, ExpandSeed, SourceRegistry};

use crate::error::Result;
use crate::identity::{
    candidate_map_key, hard_work_key, identities_match, merge_candidate_metadata, push_edition,
    StoreEdition, WorkIdentity,
};

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
    /// Position within the Audible page that produced this hit (for merge rank).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audible_rank: Option<u32>,
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
    seeds: &[BookRecord],
    owned_asins: &HashSet<String>,
    owned_isbns: &HashSet<String>,
    owned_product_keys: &HashSet<String>,
    opts: &CandidateFetchOptions,
) -> Result<Vec<StorefrontCandidate>> {
    let mut by_key: HashMap<String, StorefrontCandidate> = HashMap::new();
    let mut remote_calls = 0usize;

    let seeds: Vec<&BookRecord> = seeds.iter().take(opts.seed_limit).collect();

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
            series_asin: seed.series_asin.clone(),
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
                            && looks_like_series_set(&hit)
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
    StorefrontCandidate {
        source: source_id.to_string(),
        product_id: hit.product_id,
        title: hit.title,
        authors: hit.authors,
        narrators: hit.narrators,
        series: hit.series,
        series_index: hit.series_index,
        asin: hit.asin,
        isbn: hit.isbn,
        cover_url: hit.cover_url,
        seed_categories: None,
        origin: hit.origin,
        seed_title: None,
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
        audible_rank: None,
    }
}

/// True when the title looks like a boxed series set (usually dropped from recs).
fn looks_like_series_set(hit: &CatalogHit) -> bool {
    let title = hit.title.to_ascii_lowercase();
    title.contains("series set") || title.ends_with(" set")
}

/// Copies seed title/categories onto a candidate when those fields are still empty.
fn apply_seed(mut c: StorefrontCandidate, seed: &BookRecord) -> StorefrontCandidate {
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

/// Inserts a candidate unless its ASIN, ISBN, or source:product id is already owned.
fn insert_candidate(
    map: &mut HashMap<String, StorefrontCandidate>,
    mut c: StorefrontCandidate,
    owned_asins: &HashSet<String>,
    owned_isbns: &HashSet<String>,
    owned_product_keys: &HashSet<String>,
) {
    if let Some(asin) = c.asin.as_deref() {
        if owned_asins.contains(&asin.to_ascii_uppercase()) {
            return;
        }
    }
    if let Some(isbn) = c.isbn.clone() {
        let norm = bookclerk_enrich::canonicalize_isbn(&isbn);
        if !norm.is_empty() {
            c.isbn = Some(norm.clone());
            if owned_isbns.contains(&norm) || owned_isbns.contains(&isbn) {
                return;
            }
        } else if owned_isbns.contains(&isbn) {
            return;
        }
    }
    let source_key = format!("{}:{}", c.source, c.product_id);
    if owned_product_keys.contains(&source_key)
        || owned_asins.contains(&c.product_id.to_ascii_uppercase())
        || owned_isbns.contains(&c.product_id)
        || owned_product_keys.contains(&c.product_id)
    {
        return;
    }

    push_edition(
        &mut c.store_editions,
        StoreEdition::new(&c.source, &c.product_id),
    );

    // Prefer merging into an existing hard- or soft-matched work.
    let match_key = map.iter().find_map(|(key, existing)| {
        if let Some(hard) = hard_work_key(c.asin.as_deref(), c.isbn.as_deref()) {
            if key == &hard
                || hard_work_key(existing.asin.as_deref(), existing.isbn.as_deref()).as_deref()
                    == Some(hard.as_str())
            {
                return Some(key.clone());
            }
        }
        if identities_match(
            WorkIdentity::new(
                c.asin.as_deref(),
                c.isbn.as_deref(),
                &c.title,
                c.authors.as_deref(),
            )
            .with_series(c.series.as_deref())
            .with_series_index(c.series_index.as_deref()),
            WorkIdentity::new(
                existing.asin.as_deref(),
                existing.isbn.as_deref(),
                &existing.title,
                existing.authors.as_deref(),
            )
            .with_series(existing.series.as_deref())
            .with_series_index(existing.series_index.as_deref()),
        ) {
            return Some(key.clone());
        }
        None
    });

    if let Some(old_key) = match_key {
        let mut existing = map.remove(&old_key).expect("just found");
        merge_candidate_metadata(&mut existing, &c);
        merge_category_strings(&mut existing.seed_categories, c.seed_categories.as_deref());
        // Prefer keeping the incoming product as primary when it carries ISBN.
        if c.isbn.is_some() && existing.isbn.is_some() && c.source == "libro" {
            existing.source = c.source.clone();
            existing.product_id = c.product_id.clone();
        }
        let new_key = candidate_map_key(&existing);
        map.insert(new_key, existing);
        return;
    }

    let key = candidate_map_key(&c);
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
    books: &[BookRecord],
    listening_engagement_by_uuid: &HashMap<String, f64>,
) -> Vec<BookRecord> {
    let mut scored: Vec<(i32, &BookRecord)> = books
        .iter()
        // Podcasts are out of v1 Discover (no expand / suggestion seeds).
        .filter(|b| {
            !bookclerk_library::is_podcast_parent(&b.content_kind)
                && !bookclerk_library::is_episode(&b.content_kind)
        })
        .map(|b| {
            let mut s = 0;
            if b.is_finished {
                s += 50;
            }
            if let Some(w) = listening_engagement_by_uuid.get(&b.uuid) {
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
            if b.series_asin.is_some() {
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
    fn insert_candidate_consolidates_isbn_and_soft_match() {
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
                audible_rank: None,
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
                audible_rank: None,
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
                audible_rank: None,
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
}
