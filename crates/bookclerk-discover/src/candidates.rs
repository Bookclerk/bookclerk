//! Storefront candidate discovery (titles not yet owned).
//!
//! Seeds from local taste (finished / rated / listening), then expands via:
//! - Libro.fm `explore/audiobook_details` → `related_audiobooks`
//! - Audible public catalog (author / series keyword / series ASIN / narrator)
//! - Chirp GraphQL (related, series, author summary, catalog search)
//! - GraphicAudio Magento (product related + series pages + catalog search)
//!
//! Local embeddings and ownership filters evaluate those remote hits.

use std::collections::{HashMap, HashSet};

use bookclerk_chirp::ChirpClient;
use bookclerk_enrich::{
    public_http_client, search_catalog_by_narrator, search_catalog_by_series_asin,
    search_catalog_products, CatalogProduct,
};
use bookclerk_graphicaudio::{
    catalog_http_client, expand_from_product_id, expand_from_search, MagentoCatalogProduct,
};
use bookclerk_library::{BookRecord, LibraryStore};
use serde::Deserialize;
use serde_json::Value;

use crate::error::Result;
use crate::identity::{
    candidate_map_key, hard_work_key, merge_candidate_metadata, push_edition, works_match,
    StoreEdition,
};

/// A purchase candidate discovered from a storefront catalog (not owned locally).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StorefrontCandidate {
    pub source: String,
    pub product_id: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    /// Categories/subjects copied from the taste seed that produced this hit.
    pub seed_categories: Option<String>,
    /// How this candidate was found (related-to seed, author search, …).
    pub origin: String,
    pub seed_title: Option<String>,
    /// Known storefront editions of this work (including the primary source).
    #[serde(default)]
    pub store_editions: Vec<StoreEdition>,
}

/// Options for storefront candidate expansion.
#[derive(Debug, Clone)]
pub struct CandidateFetchOptions {
    pub region: String,
    /// Max local seed titles to expand from (finished / rated first).
    pub seed_limit: usize,
    /// Cap remote HTTP calls across all storefronts.
    pub max_remote_calls: usize,
    pub include_libro_related: bool,
    pub include_audible_author_search: bool,
    pub include_audible_series_search: bool,
    pub include_audible_series_asin: bool,
    pub include_audible_narrator_search: bool,
    pub include_chirp: bool,
    pub include_graphicaudio: bool,
    /// Fetch Chirp top/free deals into the candidate pool.
    pub include_chirp_deals: bool,
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
            include_libro_related: true,
            include_audible_author_search: true,
            include_audible_series_search: true,
            include_audible_series_asin: true,
            include_audible_narrator_search: false,
            include_chirp: true,
            include_graphicaudio: true,
            include_chirp_deals: true,
            exclude_graphicaudio_series_sets: false,
        }
    }
}

/// Expand storefront catalogs from local taste seeds; drop already-owned ids.
pub async fn gather_storefront_candidates(
    _library: &LibraryStore,
    seeds: &[BookRecord],
    owned_asins: &HashSet<String>,
    owned_isbns: &HashSet<String>,
    owned_product_keys: &HashSet<String>,
    opts: &CandidateFetchOptions,
) -> Result<Vec<StorefrontCandidate>> {
    let http = public_http_client()?;
    let chirp = ChirpClient::default();
    let ga_http = catalog_http_client().ok();
    let mut by_key: HashMap<String, StorefrontCandidate> = HashMap::new();
    let mut remote_calls = 0usize;

    let seeds: Vec<&BookRecord> = seeds.iter().take(opts.seed_limit).collect();

    for seed in &seeds {
        if remote_calls >= opts.max_remote_calls {
            break;
        }

        // Libro related titles (best signal when we have an ISBN).
        if opts.include_libro_related {
            if let Some(isbn) = seed
                .isbn
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if remote_calls < opts.max_remote_calls {
                    match libro_related(&http, isbn).await {
                        Ok(related) => {
                            remote_calls += 1;
                            for mut c in related {
                                c.origin = format!("libro related to “{}”", seed.title);
                                insert_candidate(
                                    &mut by_key,
                                    apply_seed(c, seed),
                                    owned_asins,
                                    owned_isbns,
                                    owned_product_keys,
                                );
                            }
                        }
                        Err(err) => {
                            remote_calls += 1;
                            tracing::debug!(isbn, error = %err, "libro related lookup failed");
                        }
                    }
                }
            }
        }

        // Audible: exact series ASIN listing (strongest series signal).
        if opts.include_audible_series_asin {
            if let Some(series_asin) = seed
                .series_asin
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if remote_calls < opts.max_remote_calls {
                    match search_catalog_by_series_asin(&http, &opts.region, series_asin).await {
                        Ok(products) => {
                            remote_calls += 1;
                            let series_label = seed
                                .series
                                .clone()
                                .unwrap_or_else(|| series_asin.to_string());
                            for p in products {
                                let c = audible_candidate(
                                    p,
                                    format!("audible series ASIN ({series_asin})"),
                                    Some(series_label.clone()),
                                    seed.authors.clone(),
                                );
                                insert_candidate(
                                    &mut by_key,
                                    apply_seed(c, seed),
                                    owned_asins,
                                    owned_isbns,
                                    owned_product_keys,
                                );
                            }
                        }
                        Err(err) => {
                            remote_calls += 1;
                            tracing::debug!(
                                series_asin,
                                error = %err,
                                "audible series_asin search failed"
                            );
                        }
                    }
                }
            }
        }

        // Audible: more by same author.
        if opts.include_audible_author_search {
            if let Some(author) = primary_author(seed.authors.as_deref()) {
                if remote_calls < opts.max_remote_calls {
                    match search_catalog_products(&http, &opts.region, "", Some(author), None).await
                    {
                        Ok(products) => {
                            remote_calls += 1;
                            for p in products {
                                let c = audible_candidate(
                                    p,
                                    format!("audible author search ({author})"),
                                    seed.series.clone(),
                                    Some(author.to_string()),
                                );
                                insert_candidate(
                                    &mut by_key,
                                    apply_seed(c, seed),
                                    owned_asins,
                                    owned_isbns,
                                    owned_product_keys,
                                );
                            }
                        }
                        Err(err) => {
                            remote_calls += 1;
                            tracing::debug!(author, error = %err, "audible author search failed");
                        }
                    }
                }
            }
        }

        // Audible: series keyword expansion (when we lack series_asin).
        if opts.include_audible_series_search
            && seed.series_asin.as_deref().is_none_or(str::is_empty)
        {
            if let Some(series) = seed
                .series
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if remote_calls < opts.max_remote_calls {
                    match search_catalog_products(&http, &opts.region, "", None, Some(series)).await
                    {
                        Ok(products) => {
                            remote_calls += 1;
                            for p in products {
                                let c = audible_candidate(
                                    p,
                                    format!("audible series search (“{series}”)"),
                                    Some(series.to_string()),
                                    seed.authors.clone(),
                                );
                                insert_candidate(
                                    &mut by_key,
                                    apply_seed(c, seed),
                                    owned_asins,
                                    owned_isbns,
                                    owned_product_keys,
                                );
                            }
                        }
                        Err(err) => {
                            remote_calls += 1;
                            tracing::debug!(series, error = %err, "audible series search failed");
                        }
                    }
                }
            }
        }

        // Audible: narrator search (opt-in; noisier).
        if opts.include_audible_narrator_search {
            if let Some(narrator) = primary_author(seed.narrators.as_deref()) {
                if remote_calls < opts.max_remote_calls {
                    match search_catalog_by_narrator(&http, &opts.region, narrator).await {
                        Ok(products) => {
                            remote_calls += 1;
                            for p in products {
                                let c = audible_candidate(
                                    p,
                                    format!("audible narrator search ({narrator})"),
                                    seed.series.clone(),
                                    seed.authors.clone(),
                                );
                                insert_candidate(
                                    &mut by_key,
                                    apply_seed(c, seed),
                                    owned_asins,
                                    owned_isbns,
                                    owned_product_keys,
                                );
                            }
                        }
                        Err(err) => {
                            remote_calls += 1;
                            tracing::debug!(
                                narrator,
                                error = %err,
                                "audible narrator search failed"
                            );
                        }
                    }
                }
            }
        }

        // Chirp: related / series / author / search (public GraphQL).
        if opts.include_chirp && remote_calls < opts.max_remote_calls {
            remote_calls += expand_chirp(
                &chirp,
                seed,
                &mut by_key,
                owned_asins,
                owned_isbns,
                owned_product_keys,
                opts.max_remote_calls.saturating_sub(remote_calls),
            )
            .await;
        }

        // GraphicAudio: Magento related + series / search.
        if opts.include_graphicaudio && remote_calls < opts.max_remote_calls {
            if let Some(ga_http) = ga_http.as_ref() {
                remote_calls += expand_graphicaudio(
                    ga_http,
                    seed,
                    &mut by_key,
                    &OwnedFilters {
                        asins: owned_asins,
                        isbns: owned_isbns,
                        product_keys: owned_product_keys,
                    },
                    opts.exclude_graphicaudio_series_sets,
                    opts.max_remote_calls.saturating_sub(remote_calls),
                )
                .await;
            }
        }
    }

    // Chirp deals / promos (once per run; not per-seed).
    if opts.include_chirp_deals && remote_calls < opts.max_remote_calls {
        remote_calls += gather_chirp_deals(
            &chirp,
            &mut by_key,
            owned_asins,
            owned_isbns,
            owned_product_keys,
            opts.max_remote_calls.saturating_sub(remote_calls),
        )
        .await;
    }

    tracing::info!(
        seeds = seeds.len(),
        remote_calls,
        candidates = by_key.len(),
        "gathered storefront recommendation candidates"
    );
    Ok(by_key.into_values().collect())
}

fn audible_candidate(
    p: CatalogProduct,
    origin: String,
    series_fallback: Option<String>,
    authors_fallback: Option<String>,
) -> StorefrontCandidate {
    StorefrontCandidate {
        source: String::from("audible"),
        product_id: p.asin.clone(),
        title: p.title.unwrap_or_else(|| p.asin.clone()),
        authors: p.authors.or(authors_fallback),
        narrators: p.narrators,
        series: p.series.or(series_fallback),
        series_index: p.series_sequence,
        asin: Some(p.asin),
        isbn: None,
        seed_categories: None,
        origin,
        seed_title: None,
        store_editions: Vec::new(),
    }
}

async fn expand_chirp(
    client: &ChirpClient,
    seed: &BookRecord,
    by_key: &mut HashMap<String, StorefrontCandidate>,
    owned_asins: &HashSet<String>,
    owned_isbns: &HashSet<String>,
    owned_product_keys: &HashSet<String>,
    budget: usize,
) -> usize {
    let mut used = 0usize;
    if budget == 0 {
        return 0;
    }

    // Chirp-owned seeds: related titles + series siblings via product id.
    if seed.source == "chirp" && !seed.product_id.is_empty() && used < budget {
        match client.related_audiobooks(&seed.product_id).await {
            Ok(related) => {
                used += 1;
                for book in related.related {
                    insert_candidate(
                        by_key,
                        apply_seed(
                            chirp_candidate(&book, format!("chirp related to “{}”", seed.title)),
                            seed,
                        ),
                        owned_asins,
                        owned_isbns,
                        owned_product_keys,
                    );
                }
                if let Some(series) = related.series {
                    if used < budget {
                        match client.series_catalog(&series.slug).await {
                            Ok(Some(catalog)) => {
                                used += 1;
                                for book in catalog.audiobooks {
                                    insert_candidate(
                                        by_key,
                                        apply_seed(
                                            chirp_candidate(
                                                &book,
                                                format!("chirp series (“{}”)", catalog.series.name),
                                            ),
                                            seed,
                                        ),
                                        owned_asins,
                                        owned_isbns,
                                        owned_product_keys,
                                    );
                                }
                            }
                            Ok(None) => used += 1,
                            Err(err) => {
                                used += 1;
                                tracing::debug!(
                                    slug = %series.slug,
                                    error = %err,
                                    "chirp series catalog failed"
                                );
                            }
                        }
                    }
                }
            }
            Err(err) => {
                used += 1;
                tracing::debug!(
                    id = %seed.product_id,
                    error = %err,
                    "chirp related lookup failed"
                );
            }
        }
    }

    // Series title → Chirp series slug guesses (any seed source).
    if used < budget {
        if let Some(series) = seed
            .series
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match client.resolve_series_catalog(series).await {
                Ok(Some(catalog)) => {
                    used += 1;
                    for book in catalog.audiobooks {
                        insert_candidate(
                            by_key,
                            apply_seed(
                                chirp_candidate(
                                    &book,
                                    format!("chirp series (“{}”)", catalog.series.name),
                                ),
                                seed,
                            ),
                            owned_asins,
                            owned_isbns,
                            owned_product_keys,
                        );
                    }
                }
                Ok(None) => used += 1,
                Err(err) => {
                    used += 1;
                    tracing::debug!(series, error = %err, "chirp series resolve failed");
                }
            }
        }
    }

    // Author → typeahead slug → summary titles.
    if used < budget {
        if let Some(author) = primary_author(seed.authors.as_deref()) {
            match client.resolve_author_slug(author).await {
                Ok(Some(slug)) => {
                    used += 1;
                    if used < budget {
                        match client.author_summary(&slug).await {
                            Ok(Some(catalog)) => {
                                used += 1;
                                for book in catalog.audiobooks {
                                    insert_candidate(
                                        by_key,
                                        apply_seed(
                                            chirp_candidate(
                                                &book,
                                                format!("chirp author ({})", catalog.author.name),
                                            ),
                                            seed,
                                        ),
                                        owned_asins,
                                        owned_isbns,
                                        owned_product_keys,
                                    );
                                }
                            }
                            Ok(None) => used += 1,
                            Err(err) => {
                                used += 1;
                                tracing::debug!(
                                    slug,
                                    error = %err,
                                    "chirp author summary failed"
                                );
                            }
                        }
                    }
                }
                Ok(None) => used += 1,
                Err(err) => {
                    used += 1;
                    tracing::debug!(author, error = %err, "chirp author resolve failed");
                }
            }
        }
    }

    // Fallback catalog search by title (useful for non-Chirp seeds).
    if used < budget && seed.source != "chirp" {
        let q = match primary_author(seed.authors.as_deref()) {
            Some(a) => format!("{} {a}", seed.title),
            None => seed.title.clone(),
        };
        match client.search_catalog(&q, 1, 8).await {
            Ok(books) => {
                used += 1;
                for book in books {
                    insert_candidate(
                        by_key,
                        apply_seed(
                            chirp_candidate(
                                &book,
                                format!("chirp catalog search (“{}”)", seed.title),
                            ),
                            seed,
                        ),
                        owned_asins,
                        owned_isbns,
                        owned_product_keys,
                    );
                }
            }
            Err(err) => {
                used += 1;
                tracing::debug!(error = %err, "chirp catalog search failed");
            }
        }
    }

    used
}

async fn gather_chirp_deals(
    client: &ChirpClient,
    by_key: &mut HashMap<String, StorefrontCandidate>,
    owned_asins: &HashSet<String>,
    owned_isbns: &HashSet<String>,
    owned_product_keys: &HashSet<String>,
    budget: usize,
) -> usize {
    let mut used = 0usize;
    if budget == 0 {
        return 0;
    }

    match client.top_deals(16).await {
        Ok(books) => {
            used += 1;
            for book in books {
                insert_candidate(
                    by_key,
                    chirp_candidate(&book, String::from("chirp top deals")),
                    owned_asins,
                    owned_isbns,
                    owned_product_keys,
                );
            }
        }
        Err(err) => {
            used += 1;
            tracing::debug!(error = %err, "chirp top deals failed");
        }
    }

    if used < budget {
        match client.free_deals().await {
            Ok(books) => {
                used += 1;
                for book in books.into_iter().take(16) {
                    insert_candidate(
                        by_key,
                        chirp_candidate(&book, String::from("chirp free deals")),
                        owned_asins,
                        owned_isbns,
                        owned_product_keys,
                    );
                }
            }
            Err(err) => {
                used += 1;
                tracing::debug!(error = %err, "chirp free deals failed");
            }
        }
    }

    used
}

fn chirp_candidate(
    book: &bookclerk_chirp::CatalogAudiobook,
    origin: String,
) -> StorefrontCandidate {
    StorefrontCandidate {
        source: String::from("chirp"),
        product_id: book.id.clone(),
        title: book.title(),
        authors: book.display_authors.clone(),
        narrators: book.display_narrators.clone(),
        series: book.series_name(),
        series_index: book.series_audiobook.as_ref().and_then(|s| {
            s.display_number
                .clone()
                .or_else(|| s.number.map(|n| n.to_string()))
        }),
        asin: None,
        isbn: None,
        seed_categories: None,
        origin,
        seed_title: None,
        store_editions: Vec::new(),
    }
}

/// Already-owned identifiers used to drop storefront hits.
struct OwnedFilters<'a> {
    asins: &'a HashSet<String>,
    isbns: &'a HashSet<String>,
    product_keys: &'a HashSet<String>,
}

async fn expand_graphicaudio(
    http: &reqwest::Client,
    seed: &BookRecord,
    by_key: &mut HashMap<String, StorefrontCandidate>,
    owned: &OwnedFilters<'_>,
    exclude_series_sets: bool,
    budget: usize,
) -> usize {
    let mut used = 0usize;
    if budget == 0 {
        return 0;
    }

    if seed.source == "graphicaudio" && !seed.product_id.is_empty() && used < budget {
        match expand_from_product_id(http, None, &seed.product_id).await {
            Ok(products) => {
                used += 1;
                for p in products {
                    if exclude_series_sets && p.is_series_set() {
                        continue;
                    }
                    insert_candidate(
                        by_key,
                        apply_seed(
                            ga_candidate(
                                &p,
                                format!("graphicaudio related/series for “{}”", seed.title),
                            ),
                            seed,
                        ),
                        owned.asins,
                        owned.isbns,
                        owned.product_keys,
                    );
                }
            }
            Err(err) => {
                used += 1;
                tracing::debug!(
                    id = %seed.product_id,
                    error = %err,
                    "graphicaudio product expand failed"
                );
            }
        }
    }

    // Series / title Magento search (works for GA seeds and cross-store taste).
    if used < budget {
        let query = seed
            .series
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(seed.title.as_str());
        // Prefer GA-flavored seeds or explicit series; skip noisy Audible-only titles
        // unless the seed already came from GraphicAudio.
        let worth = seed.source == "graphicaudio"
            || seed
                .series
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
        if worth {
            match expand_from_search(http, None, query).await {
                Ok(products) => {
                    used += 1;
                    for p in products {
                        if exclude_series_sets && p.is_series_set() {
                            continue;
                        }
                        insert_candidate(
                            by_key,
                            apply_seed(
                                ga_candidate(
                                    &p,
                                    format!("graphicaudio catalog search (“{query}”)"),
                                ),
                                seed,
                            ),
                            owned.asins,
                            owned.isbns,
                            owned.product_keys,
                        );
                    }
                }
                Err(err) => {
                    used += 1;
                    tracing::debug!(query, error = %err, "graphicaudio search failed");
                }
            }
        }
    }

    used
}

fn ga_candidate(p: &MagentoCatalogProduct, origin: String) -> StorefrontCandidate {
    StorefrontCandidate {
        source: String::from("graphicaudio"),
        product_id: p.product_id.clone(),
        title: p.title.clone(),
        authors: None,
        narrators: None,
        series: p.series.clone(),
        series_index: None,
        asin: None,
        isbn: None,
        seed_categories: None,
        origin,
        seed_title: None,
        store_editions: Vec::new(),
    }
}

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
        if works_match(
            &c.title,
            c.authors.as_deref(),
            &existing.title,
            existing.authors.as_deref(),
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

fn primary_author(authors: Option<&str>) -> Option<&str> {
    authors?
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

#[derive(Debug, Deserialize)]
struct LibroDetailsResponse {
    #[serde(default)]
    data: Option<LibroDetailsData>,
}

#[derive(Debug, Deserialize)]
struct LibroDetailsData {
    #[serde(default)]
    related_audiobooks: Vec<Value>,
}

async fn libro_related(http: &reqwest::Client, isbn: &str) -> Result<Vec<StorefrontCandidate>> {
    let url = format!("https://libro.fm/explore/audiobook_details/{isbn}");
    let resp = http.get(&url).send().await?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let body: LibroDetailsResponse = resp
        .json()
        .await
        .map_err(|e| crate::error::DiscoverError::message(format!("libro related parse: {e}")))?;
    let related = body.data.map(|d| d.related_audiobooks).unwrap_or_default();
    Ok(related.iter().filter_map(parse_libro_book).collect())
}

fn parse_libro_book(v: &Value) -> Option<StorefrontCandidate> {
    let isbn = v
        .get("isbn")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let authors = v
        .get("authors")
        .and_then(|a| {
            if let Some(s) = a.as_str() {
                Some(s.to_string())
            } else if let Some(arr) = a.as_array() {
                let names: Vec<&str> = arr
                    .iter()
                    .filter_map(|x| x.as_str().or_else(|| x.get("name").and_then(Value::as_str)))
                    .collect();
                if names.is_empty() {
                    None
                } else {
                    Some(names.join(", "))
                }
            } else {
                None
            }
        })
        .or_else(|| v.get("author").and_then(Value::as_str).map(str::to_string));
    let narrators = v
        .get("narrators")
        .and_then(Value::as_str)
        .map(str::to_string);
    let series = v.get("series").and_then(|s| {
        s.as_str()
            .map(str::to_string)
            .or_else(|| s.get("name").and_then(Value::as_str).map(str::to_string))
    });
    Some(StorefrontCandidate {
        source: String::from("libro"),
        product_id: isbn.clone(),
        title,
        authors,
        narrators,
        series,
        series_index: None,
        asin: None,
        isbn: Some(isbn),
        seed_categories: None,
        origin: String::from("libro related"),
        seed_title: None,
        store_editions: Vec::new(),
    })
}

/// Pick local seed books for storefront expansion (finished / high-rated first).
#[must_use]
pub fn select_taste_seeds(
    books: &[BookRecord],
    listening_engagement_by_uuid: &HashMap<String, f64>,
) -> Vec<BookRecord> {
    let mut scored: Vec<(i32, &BookRecord)> = books
        .iter()
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
    use serde_json::json;

    #[test]
    fn parse_libro_related_object() {
        let v = json!({
            "isbn": "9781234567890",
            "title": "Next Title",
            "authors": [{"name": "Ada Author"}],
            "series": {"name": "Test Series"}
        });
        let c = parse_libro_book(&v).unwrap();
        assert_eq!(c.isbn.as_deref(), Some("9781234567890"));
        assert_eq!(c.authors.as_deref(), Some("Ada Author"));
        assert_eq!(c.series.as_deref(), Some("Test Series"));
    }

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
                seed_categories: None,
                origin: "audible author".into(),
                seed_title: None,
                store_editions: Vec::new(),
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
                seed_categories: None,
                origin: "libro related".into(),
                seed_title: None,
                store_editions: Vec::new(),
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
                seed_categories: None,
                origin: "chirp search".into(),
                seed_title: None,
                store_editions: Vec::new(),
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
