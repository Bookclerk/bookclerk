//! Multi-storefront catalog search for Discover typeahead.
//!
//! Queries every registered [`ContentSource::search_catalog`], then merges
//! hits by bibliographic identity (`work_map_key`) — no pricing.

use std::collections::HashMap;

use bookclerk_source::{CatalogSearchOpts, SourceRegistry};

use crate::candidates::StorefrontCandidate;
use crate::error::Result;
use crate::identity::{
    identities_match, merge_candidate_metadata, push_edition, work_map_key, StoreEdition,
    WorkIdentity,
};

/// One autocomplete suggestion (possibly spanning multiple storefronts).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CatalogSearchHit {
    pub work_key: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub store_editions: Vec<StoreEdition>,
    /// Storefronts that matched (deduped source ids).
    pub sources: Vec<String>,
}

/// Search every configured storefront catalog and merge by work identity.
pub async fn catalog_search(
    registry: &SourceRegistry,
    query: &str,
    region: &str,
    limit: usize,
) -> Result<Vec<CatalogSearchHit>> {
    let q = query.trim();
    if q.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let per_store = limit.clamp(8, 24);
    let region = region.trim().to_ascii_lowercase();
    let region = if region.is_empty() {
        String::from("us")
    } else {
        region
    };

    let search_opts = CatalogSearchOpts {
        query: q.to_string(),
        region: region.clone(),
        limit: per_store,
    };

    let mut by_key: HashMap<String, StorefrontCandidate> = HashMap::new();
    for source in registry.all() {
        let id = source.id();
        match source.search_catalog(&search_opts).await {
            Ok(hits) => {
                for hit in hits {
                    upsert_hit(
                        &mut by_key,
                        StorefrontCandidate {
                            source: id.to_string(),
                            product_id: hit.product_id,
                            title: hit.title,
                            authors: hit.authors,
                            narrators: hit.narrators,
                            series: hit.series,
                            series_index: hit.series_index,
                            asin: hit.asin,
                            isbn: hit.isbn,
                            seed_categories: None,
                            origin: String::from("catalog search"),
                            seed_title: None,
                            store_editions: Vec::new(),
                        },
                    );
                }
            }
            Err(err) => tracing::debug!(source = %id, error = %err, "catalog search store failed"),
        }
    }

    let mut out: Vec<CatalogSearchHit> = by_key
        .into_iter()
        .map(|(work_key, c)| {
            let mut sources: Vec<String> =
                c.store_editions.iter().map(|e| e.source.clone()).collect();
            sources.sort();
            sources.dedup();
            CatalogSearchHit {
                work_key,
                title: c.title,
                authors: c.authors,
                narrators: c.narrators,
                series: c.series,
                asin: c.asin,
                isbn: c.isbn,
                store_editions: c.store_editions,
                sources,
            }
        })
        .collect();

    // Prefer multi-store agreement, then title proximity to the query.
    let q_lower = q.to_ascii_lowercase();
    out.sort_by(|a, b| {
        b.sources
            .len()
            .cmp(&a.sources.len())
            .then_with(|| {
                let a_match = a.title.to_ascii_lowercase().starts_with(&q_lower) as u8;
                let b_match = b.title.to_ascii_lowercase().starts_with(&q_lower) as u8;
                b_match.cmp(&a_match)
            })
            .then_with(|| a.title.len().cmp(&b.title.len()))
            .then_with(|| a.title.cmp(&b.title))
    });
    out.truncate(limit);
    Ok(out)
}

fn upsert_hit(map: &mut HashMap<String, StorefrontCandidate>, mut hit: StorefrontCandidate) {
    push_edition(
        &mut hit.store_editions,
        StoreEdition::new(&hit.source, &hit.product_id),
    );
    if let Some(isbn) = hit.isbn.as_mut() {
        let n = bookclerk_enrich::canonicalize_isbn(isbn);
        if !n.is_empty() {
            *isbn = n;
        }
    }

    let match_key = map.iter().find_map(|(key, existing)| {
        if identities_match(
            WorkIdentity::new(
                hit.asin.as_deref(),
                hit.isbn.as_deref(),
                &hit.title,
                hit.authors.as_deref(),
            ),
            WorkIdentity::new(
                existing.asin.as_deref(),
                existing.isbn.as_deref(),
                &existing.title,
                existing.authors.as_deref(),
            ),
        ) {
            Some(key.clone())
        } else {
            None
        }
    });

    if let Some(old_key) = match_key {
        let mut existing = map.remove(&old_key).expect("just found");
        merge_candidate_metadata(&mut existing, &hit);
        let new_key = work_map_key(
            existing.asin.as_deref(),
            existing.isbn.as_deref(),
            &existing.title,
            existing.authors.as_deref(),
            Some(existing.source.as_str()),
            Some(existing.product_id.as_str()),
        );
        map.insert(new_key, existing);
        return;
    }

    let key = work_map_key(
        hit.asin.as_deref(),
        hit.isbn.as_deref(),
        &hit.title,
        hit.authors.as_deref(),
        Some(hit.source.as_str()),
        Some(hit.product_id.as_str()),
    );
    map.insert(key, hit);
}
