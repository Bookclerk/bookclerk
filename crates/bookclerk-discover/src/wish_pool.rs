//! Places the store editions a wish names in the shared catalog pool.
//!
//! When someone wishes for a title, the server asks each store about the
//! edition the wish names (`catalog_detail`) and records the store's own
//! answer as a `store_listings` row, as a Discover answer is. The pool
//! therefore only ever holds what a store reported: the text and prices a
//! browser sends with the wish stay on the wish's own rows, so one person's
//! wish never changes what everyone else sees.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::{LibraryStore, ListingOffer, OfferAudience, WishWork};
use bookclerk_source::{ContentSource, SourceRegistry};
use chrono::Utc;
use tokio::task::JoinSet;

use crate::pool::catalog_listing;
use crate::purchase::PurchaseHint;

/// Longest one store may take to describe one edition before the wish is
/// saved without that edition's listing.
const EDITION_LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);

/// Most store editions looked up for one wish. A wish names a few; the cap
/// bounds what a crafted request can make the server fetch.
pub const MAX_WISH_EDITIONS: usize = 8;

/// One store edition a wish names, as placed in the shared pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PooledEdition {
    /// Storefront plugin id the wish named (`libro`), lower-cased.
    pub source: String,
    /// The store's product id the wish named.
    pub product_id: String,
    /// The `store_listings` row the store's answer resolved to.
    pub listing_id: String,
    /// The work and edition that listing resolved to, when it has a work.
    pub work: Option<WishWork>,
}

/// Asks each store about the editions a wish names and records the answers
/// in the shared pool.
///
/// Editions are looked up concurrently, each within
/// `EDITION_LOOKUP_TIMEOUT`. An edition whose store is not deployed, does
/// not answer in time, fails or does not know the product is left out: the
/// wish is still saved, with that edition's row naming no listing. Server
/// side purchase hints for the same editions add the member price they
/// quote to the pooled listing, since they also come from the store.
///
/// # Arguments
///
/// * `library` - The library whose pool records the listings.
/// * `registry` - The deployed storefronts.
/// * `editions` - `(source, product_id)` pairs, in the wish's order; blank
///   and repeated pairs are skipped, and at most [`MAX_WISH_EDITIONS`] are
///   asked about.
/// * `hints` - Purchase hints the server resolved for the wish.
/// * `region` - The marketplace the stores are asked about.
///
/// # Returns
///
/// One entry per edition the pool now holds, in the order given.
pub async fn pool_wish_editions(
    library: &LibraryStore,
    registry: &SourceRegistry,
    editions: &[(String, String)],
    hints: &[PurchaseHint],
    region: &str,
) -> Vec<PooledEdition> {
    let mut seen = BTreeSet::new();
    let wanted: Vec<(String, String)> = editions
        .iter()
        .map(|(s, p)| (s.trim().to_ascii_lowercase(), p.trim().to_string()))
        .filter(|(s, p)| !s.is_empty() && !p.is_empty())
        .filter(|pair| seen.insert(pair.clone()))
        .take(MAX_WISH_EDITIONS)
        .collect();
    let mut tasks: JoinSet<(usize, String, Option<bookclerk_source::CatalogHit>)> = JoinSet::new();
    for (index, (source_id, product_id)) in wanted.iter().enumerate() {
        let Some(source) = registry.get(source_id) else {
            continue;
        };
        let product_id = product_id.clone();
        tasks.spawn(async move {
            let plugin_key = source.plugin_key().to_string();
            (index, plugin_key, lookup(source, &product_id).await)
        });
    }
    let mut answers: Vec<(usize, String, bookclerk_source::CatalogHit)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, plugin_key, Some(hit))) = joined {
            answers.push((index, plugin_key, hit));
        }
    }
    answers.sort_by_key(|(index, _, _)| *index);
    if answers.is_empty() {
        return Vec::new();
    }
    let now = Utc::now();
    let listings: Vec<_> = answers
        .iter()
        .map(|(_, plugin_key, hit)| catalog_listing(plugin_key, region, hit, now))
        .collect();
    let recorded = match library.record_catalog_listings(&listings).await {
        Ok(recorded) => recorded,
        Err(err) => {
            tracing::warn!(error = %err, "could not store a wish's store editions");
            return Vec::new();
        }
    };
    let placed: Vec<(usize, String)> = answers
        .iter()
        .zip(recorded.ids)
        .filter_map(|((index, _, _), id)| Some((*index, id?)))
        .collect();
    let ids: Vec<String> = placed.iter().map(|(_, id)| id.clone()).collect();
    let records = match library.catalog_listings(&ids).await {
        Ok(records) => records,
        Err(err) => {
            tracing::warn!(error = %err, "could not read a wish's pooled editions");
            return Vec::new();
        }
    };
    let mut out = Vec::with_capacity(placed.len());
    let mut offers = Vec::new();
    for (index, listing_id) in placed {
        let Some(record) = records.iter().find(|r| r.id == listing_id) else {
            continue;
        };
        let (source, product_id) = wanted[index].clone();
        if let Some(offer) = member_offer(hints, &source, &product_id) {
            offers.push((listing_id.clone(), offer));
        }
        out.push(PooledEdition {
            source,
            product_id,
            work: record.work_id.clone().map(|work_id| WishWork {
                work_id,
                edition_id: Some(record.edition_id.clone()),
            }),
            listing_id,
        });
    }
    if !offers.is_empty() {
        if let Err(err) = library.record_listing_offers(&offers).await {
            tracing::debug!(error = %err, "could not store a wish's member prices");
        }
    }
    out
}

/// Asks `source` to describe `product_id`, within `EDITION_LOOKUP_TIMEOUT`.
///
/// # Returns
///
/// The store's answer, or `None` when it failed, timed out or does not know
/// the product (the failure is logged).
async fn lookup(
    source: Arc<dyn ContentSource>,
    product_id: &str,
) -> Option<bookclerk_source::CatalogHit> {
    match tokio::time::timeout(EDITION_LOOKUP_TIMEOUT, source.catalog_detail(product_id)).await {
        Ok(Ok(hit)) => hit,
        Ok(Err(err)) => {
            tracing::debug!(store = %source.id(), error = %err, "wish edition lookup failed");
            None
        }
        Err(_) => {
            tracing::debug!(store = %source.id(), "wish edition lookup timed out");
            None
        }
    }
}

/// The member price a server-side purchase hint quotes for one edition.
///
/// # Returns
///
/// A `member` offer quoted now, or `None` when no hint for the edition
/// carries a member price.
fn member_offer(hints: &[PurchaseHint], source: &str, product_id: &str) -> Option<ListingOffer> {
    let hint = hints.iter().find(|h| {
        h.source.trim().eq_ignore_ascii_case(source) && h.product_id.trim() == product_id
    })?;
    let offer = ListingOffer {
        audience: OfferAudience::Member,
        price_cents: hint.member_price_cents,
        price_label: hint.member_price_label.clone(),
        currency: hint.currency.clone(),
        observed_at: Utc::now(),
    };
    (!offer.is_empty()).then_some(offer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_source::CatalogHit;
    use std::sync::Mutex;

    /// A store that describes products from a fixed list, or fails.
    struct Store {
        /// Store id.
        id: &'static str,
        /// Products it knows, by product id.
        known: Vec<CatalogHit>,
        /// When true every lookup fails, as a broken store would.
        fails: bool,
        /// Product ids it was asked about.
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl ContentSource for Store {
        fn id(&self) -> &str {
            self.id
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            bookclerk_source::PortalAuthMode::Password
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            bookclerk_source::SourceBrand {
                id: "fake",
                name: "Fake",
                bg: "#000000",
                fg: "#ffffff",
                accent: "#000000",
                icon_url: "",
            }
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Ok(Vec::new())
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Ok(bookclerk_source::ScanSummary::default())
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            _opts: &bookclerk_source::FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn catalog_detail(
            &self,
            product_id: &str,
        ) -> bookclerk_source::Result<Option<CatalogHit>> {
            self.calls.lock().unwrap().push(product_id.to_string());
            if self.fails {
                return Err(bookclerk_source::SourceError::api("store is down"));
            }
            Ok(self
                .known
                .iter()
                .find(|h| h.product_id == product_id)
                .cloned())
        }
    }

    /// A store's own description of `product`.
    fn hit(product: &str, title: &str, price_cents: i64) -> CatalogHit {
        CatalogHit {
            product_id: product.to_string(),
            title: title.to_string(),
            description: Some(format!("The store's blurb for {title}.")),
            price_cents: Some(price_cents),
            currency: Some(String::from("USD")),
            ..CatalogHit::default()
        }
    }

    /// Registers `stores` and returns each one's call log.
    fn registry(stores: Vec<Store>) -> (SourceRegistry, Vec<Arc<Mutex<Vec<String>>>>) {
        let mut registry = SourceRegistry::new();
        let mut logs = Vec::new();
        for store in stores {
            logs.push(store.calls.clone());
            registry.register(Arc::new(store));
        }
        (registry, logs)
    }

    /// A store named `id` knowing `known`.
    fn store(id: &'static str, known: Vec<CatalogHit>, fails: bool) -> Store {
        Store {
            id,
            known,
            fails,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[tokio::test]
    async fn a_wish_pools_only_what_the_stores_report() {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let (registry, logs) = registry(vec![
            store("libro", vec![hit("L1", "Dune", 1999)], false),
            store("chirp", Vec::new(), true),
        ]);
        let hints = vec![PurchaseHint {
            source: String::from("libro"),
            product_id: String::from("L1"),
            member_price_cents: Some(1299),
            member_price_label: Some(String::from("$12.99")),
            currency: Some(String::from("USD")),
            ..PurchaseHint::default()
        }];
        let editions = vec![
            (String::from(" Libro "), String::from("L1")),
            // Repeated, unknown store, a failing store and a blank pair are
            // all skipped without failing the wish.
            (String::from("libro"), String::from("L1")),
            (String::from("nowhere"), String::from("N1")),
            (String::from("chirp"), String::from("C1")),
            (String::new(), String::from("X")),
        ];
        let pooled = pool_wish_editions(&library, &registry, &editions, &hints, "us").await;

        assert_eq!(*logs[0].lock().unwrap(), vec!["L1"]);
        assert_eq!(*logs[1].lock().unwrap(), vec!["C1"]);
        assert_eq!(pooled.len(), 1);
        let edition = &pooled[0];
        assert_eq!(
            (edition.source.as_str(), edition.product_id.as_str()),
            ("libro", "L1")
        );
        assert!(edition.work.is_some(), "the listing resolved to a work");
        let record = library
            .catalog_listings(std::slice::from_ref(&edition.listing_id))
            .await
            .unwrap()
            .remove(0);
        // The pool holds the store's text and both store-quoted prices.
        assert_eq!(record.title, "Dune");
        assert_eq!(
            record.detail.description.as_deref(),
            Some("The store's blurb for Dune.")
        );
        let price = |audience| record.offer(audience).and_then(|o| o.price_cents);
        assert_eq!(price(OfferAudience::NonMember), Some(1999));
        assert_eq!(price(OfferAudience::Member), Some(1299));
        assert_eq!(
            edition.work.as_ref().unwrap().work_id,
            record.work_id.clone().unwrap()
        );
    }

    #[tokio::test]
    async fn a_wish_asks_about_at_most_the_edition_cap() {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let (registry, logs) = registry(vec![store("libro", Vec::new(), false)]);
        let editions: Vec<(String, String)> = (0..MAX_WISH_EDITIONS + 5)
            .map(|i| (String::from("libro"), format!("P{i}")))
            .collect();
        let pooled = pool_wish_editions(&library, &registry, &editions, &[], "us").await;
        assert!(pooled.is_empty(), "the store knows none of them");
        assert_eq!(logs[0].lock().unwrap().len(), MAX_WISH_EDITIONS);
    }
}
