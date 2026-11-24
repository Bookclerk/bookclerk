//! "In your library": whether the library already holds a catalog search
//! hit, shown on the hit's row.
//!
//! Search is a store browser, so unlike Discover's shelves it keeps books
//! the library holds; the row says so instead. A hit is matched to the
//! library through the data model, never by comparing titles in the
//! browser:
//!
//! 1. **This recording** when one of the hit's identifiers belongs to a
//!    held edition: its ASIN or ISBN identifies an edition an account or a
//!    branch holds a listing of, or one of its store products is a held
//!    listing ([`LibraryStore::held_identifiers`]).
//! 2. **Another edition** when the hit's work is held but none of those
//!    identifiers is: the work of a store listing the shared listing pool
//!    holds for one of its store editions, or else the work the library's
//!    resolver places the hit in, exactly as it would place a scanned
//!    listing ([`LibraryStore::work_for`]), and an account or a branch
//!    holds an edition of that work ([`LibraryStore::held_works`]). That is
//!    another recording of the book, or the same recording bought from
//!    another store.
//!
//! The mark is filled when a search page is read, never stored, so a book
//! the library gains or loses shows on the next search.

use std::collections::{BTreeSet, HashMap};

use bookclerk_library::control_plane::works_of_listings;
use bookclerk_library::{LibraryStore, TitleQuery};

use crate::catalog_search::CatalogSearchHit;
use crate::error::Result;

/// How the library holds a catalog search hit.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LibraryCopy {
    /// The library holds this edition: one of the hit's ASIN, ISBN or
    /// store products belongs to a held edition.
    ThisEdition,
    /// The library holds another edition of the hit's book (another
    /// recording, or this recording from another store), but none with the
    /// hit's identifiers.
    OtherEdition,
}

/// The identifiers of `hit` that can name a held edition: its ASIN, its
/// ISBN as given and canonical, and each store edition's product id, which
/// is also checked as an ASIN and an ISBN (Audible's product ids are ASINs,
/// Libro.fm's are ISBNs).
fn identifiers_of(hit: &CatalogSearchHit) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut asins: Vec<String> = hit.asin.iter().cloned().collect();
    let mut isbns: Vec<String> = Vec::new();
    if let Some(isbn) = hit.isbn.as_deref() {
        isbns.push(isbn.to_string());
        let canonical = bookclerk_enrich::canonicalize_isbn(isbn);
        if !canonical.is_empty() {
            isbns.push(canonical);
        }
    }
    let products: Vec<String> = hit
        .store_editions
        .iter()
        .map(|e| e.product_id.clone())
        .filter(|p| !p.trim().is_empty())
        .collect();
    asins.extend(products.iter().cloned());
    isbns.extend(products.iter().cloned());
    (asins, isbns, products)
}

/// Whether one of `hit`'s identifiers is among the held ones.
fn holds_this_edition(hit: &CatalogSearchHit, held: &bookclerk_library::HeldIdentifiers) -> bool {
    let (asins, isbns, products) = identifiers_of(hit);
    asins
        .iter()
        .any(|a| held.asins.contains(&a.to_ascii_uppercase()))
        || isbns.iter().any(|i| held.isbns.contains(i))
        || products.iter().any(|p| held.product_ids.contains(p))
}

/// Sets `in_library` on every hit of a catalog search page.
///
/// A hit is this edition when one of its ASIN, ISBN or store products
/// belongs to a held edition, and another edition when only its work (from
/// the listing pool, or else the library's resolver) is held. A hit the library
/// does not hold gets `None`.
///
/// # Arguments
///
/// * `library` - The library whose holdings, pool and resolver are read.
/// * `hits` - The page's hits.
/// * `plugin_key_of` - The PluginKey a storefront id's listings are pooled
///   under, or `None` for a source that is not deployed.
///
/// # Errors
///
/// Returns the library read error.
pub async fn join_search_library_copies(
    library: &LibraryStore,
    hits: &mut [CatalogSearchHit],
    plugin_key_of: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    if hits.is_empty() {
        return Ok(());
    }
    let (mut asins, mut isbns, mut products) = (Vec::new(), Vec::new(), Vec::new());
    for hit in hits.iter() {
        let (a, i, p) = identifiers_of(hit);
        asins.extend(a);
        isbns.extend(i);
        products.extend(p);
    }
    let held = library.held_identifiers(&asins, &isbns, &products).await?;
    let mut copy: Vec<Option<LibraryCopy>> = hits
        .iter()
        .map(|hit| holds_this_edition(hit, &held).then_some(LibraryCopy::ThisEdition))
        .collect();

    // The rest: their work, from the pool first, else from the resolver.
    let keyed =
        |source: &str, product: &str| plugin_key_of(source).map(|key| (key, product.to_string()));
    let listings: Vec<(String, String)> = hits
        .iter()
        .zip(&copy)
        .filter(|(_, c)| c.is_none())
        .flat_map(|(h, _)| {
            h.store_editions
                .iter()
                .filter_map(|e| keyed(&e.source, &e.product_id))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let work_of_listing = if listings.is_empty() {
        HashMap::new()
    } else {
        works_of_listings(library, &listings).await?
    };
    let mut work_of_hit: Vec<Option<String>> = Vec::with_capacity(hits.len());
    for (hit, c) in hits.iter().zip(&copy) {
        if c.is_some() {
            work_of_hit.push(None);
            continue;
        }
        let pooled = hit
            .store_editions
            .iter()
            .filter_map(|e| keyed(&e.source, &e.product_id))
            .find_map(|key| work_of_listing.get(&key).cloned());
        let work = match pooled {
            Some(work) => Some(work),
            None if hit.title.trim().is_empty() => None,
            None => {
                library
                    .work_for(&TitleQuery {
                        title: &hit.title,
                        authors: hit.authors.as_deref(),
                        asin: hit.asin.as_deref(),
                        isbn: hit.isbn.as_deref(),
                        series: hit.series.as_deref(),
                        series_index: hit.series_index.as_deref(),
                    })
                    .await?
            }
        };
        work_of_hit.push(work);
    }
    let works: Vec<String> = work_of_hit
        .iter()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let held_works = library.held_works(&works).await?;
    for (c, work) in copy.iter_mut().zip(work_of_hit) {
        if work.is_some_and(|w| held_works.contains(&w)) {
            *c = Some(LibraryCopy::OtherEdition);
        }
    }
    for (hit, c) in hits.iter_mut().zip(copy) {
        hit.in_library = c;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A search hit from `source` for `product`, titled `title`, by Frank
    /// Herbert.
    fn hit(source: &str, product: &str, title: &str) -> CatalogSearchHit {
        serde_json::from_value(serde_json::json!({
            "work_key": format!("{source}:{product}"),
            "title": title,
            "authors": "Frank Herbert",
            "narrators": null,
            "series": null,
            "asin": null,
            "isbn": null,
            "store_editions": [{"source": source, "product_id": product}],
            "sources": [source],
        }))
        .unwrap()
    }

    /// Search marks the edition the library holds, another store's or
    /// another recording's edition of that book, and nothing on a sequel
    /// or an unknown book; a library with no books marks nothing.
    #[tokio::test]
    async fn search_hits_say_whether_the_library_holds_them() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let page = || {
            let mut audible = hit("audible", "B002V1OF70", "Dune");
            audible.asin = Some("B002V1OF70".into());
            let mut chirp = hit("chirp", "chirp-dune", "Dune");
            chirp.isbn = Some("9781427201447".into());
            vec![
                audible,
                chirp,
                hit("libro", "messiah", "Dune Messiah"),
                hit("libro", "unknown", "The Dosadi Experiment"),
                hit("libro", "untitled", "  "),
            ]
        };
        let marks = |hits: &[CatalogSearchHit]| {
            hits.iter()
                .map(|h| h.in_library)
                .collect::<Vec<Option<LibraryCopy>>>()
        };
        let plugin_key = |id: &str| Some(id.to_string());

        let mut hits = page();
        join_search_library_copies(&store, &mut hits, plugin_key)
            .await
            .unwrap();
        assert_eq!(marks(&hits), [None; 5], "a new library holds nothing");

        store
            .upsert_account("audible", "acct", "us", Some("Main"), true, "audible")
            .await
            .unwrap();
        let mut dune = bookclerk_library::NewBook::minimal("B002V1OF70", "acct", "us", "Dune");
        dune.authors = Some("Frank Herbert".into());
        dune.asin = Some("B002V1OF70".into());
        dune.series = Some("Dune".into());
        dune.series_index = Some("1".into());
        store.upsert_book(&dune).await.unwrap();

        let mut hits = page();
        join_search_library_copies(&store, &mut hits, plugin_key)
            .await
            .unwrap();
        assert_eq!(
            marks(&hits),
            [
                Some(LibraryCopy::ThisEdition),
                Some(LibraryCopy::OtherEdition),
                None,
                None,
                None,
            ]
        );

        // A hit whose ASIN is only in its store editions' product id is
        // still the held edition.
        let mut hits = vec![hit("audible", "B002V1OF70", "Dune: Book One")];
        join_search_library_copies(&store, &mut hits, plugin_key)
            .await
            .unwrap();
        assert_eq!(marks(&hits), [Some(LibraryCopy::ThisEdition)]);

        let mut none: Vec<CatalogSearchHit> = Vec::new();
        join_search_library_copies(&store, &mut none, plugin_key)
            .await
            .unwrap();
    }

    /// The mark is on the wire as a snake-case word, and absent when the
    /// library holds neither.
    #[test]
    fn the_mark_serializes_as_a_word_and_is_left_out_when_absent() {
        let mut h = hit("audible", "B1", "Dune");
        assert!(!serde_json::to_string(&h).unwrap().contains("in_library"));
        h.in_library = Some(LibraryCopy::OtherEdition);
        assert!(serde_json::to_string(&h)
            .unwrap()
            .contains(r#""in_library":"other_edition""#));
    }
}
