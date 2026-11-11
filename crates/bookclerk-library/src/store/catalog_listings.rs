//! The shared catalog pool: what storefronts sell, stored once as
//! `store_listings` rows with their prices in `listing_offers`.
//!
//! A storefront's catalog answer (a Discover expansion, a deals list, a
//! price quote) is resolved into the book model the same way a scan is
//! (`docs/adr/library-data-model.md`): each title gets a work, an
//! expression and an edition, and a `store_listings` row that nobody
//! holds. The store's own text for the listing (synopsis kept whole,
//! cover, product URL, credits as it prints them, ratings) is stored on
//! that row as parsed columns, and what it costs is stored per audience in
//! `listing_offers`. Per-person tables (`discover_candidates`, the stored
//! feed) and the storefront call cache keep references to listings, so
//! every member whose taste reaches a title shares one row.
//!
//! Work identity always comes from the resolver (shared identifiers, then
//! the work matcher), never from comparing display strings here.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use bookclerk_plugin_abi::TypedDbStatement;
use bookclerk_resolve as resolve;

use super::write_batches::WriteBatches;
use super::{now_str, parse_dt, LibraryStore};
use crate::control_plane::batch;
use crate::entities::{edition_identifiers, listing_offers, store_listings};
use crate::error::{LibraryError, Result};

/// How long a catalog listing counts as current: a store answer for a
/// listing seen more recently than this refreshes only its prices, not
/// its resolution or text, so overlapping tastes cost one resolve a day.
pub const CATALOG_LISTING_REFRESH: chrono::Duration = chrono::Duration::days(1);

/// Listings deleted per statement when unreferenced catalog listings are
/// pruned, so one delete never walks an unbounded set.
const PRUNE_BATCH: u64 = 500;

/// Who a [`ListingOffer`] price is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OfferAudience {
    /// Someone without an account at the store: the price Discover shows
    /// unless the person has an account there.
    NonMember,
    /// A member or subscriber of the store.
    Member,
}

impl OfferAudience {
    /// The stored `listing_offers.audience` text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonMember => "non_member",
            Self::Member => "member",
        }
    }

    /// Parses stored audience text; `None` for text a newer release wrote.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "non_member" => Some(Self::NonMember),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

/// One price a store quoted for one listing and audience.
#[derive(Debug, Clone, PartialEq)]
pub struct ListingOffer {
    /// Who pays it.
    pub audience: OfferAudience,
    /// Price in minor units of [`Self::currency`], when the store gave one.
    pub price_cents: Option<i64>,
    /// The store's formatted price (`$14.95`), when it gave one.
    pub price_label: Option<String>,
    /// ISO 4217 currency code.
    pub currency: Option<String>,
    /// When the store quoted it.
    pub observed_at: DateTime<Utc>,
}

impl ListingOffer {
    /// True when the offer carries neither a price nor a label.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.price_cents.is_none() && self.price_label.is_none()
    }
}

/// The store's own text for a listing, beyond the title the resolver sees.
///
/// Display only: credits and series here are as the store prints them, and
/// nothing matches on them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListingDetail {
    /// Subtitle.
    pub subtitle: Option<String>,
    /// Authors, comma-separated.
    pub authors: Option<String>,
    /// Narrators, comma-separated.
    pub narrators: Option<String>,
    /// Series name.
    pub series: Option<String>,
    /// Position in the series (`1`, `2.5`).
    pub series_index: Option<String>,
    /// Synopsis, kept whole.
    pub description: Option<String>,
    /// Publisher or imprint.
    pub publisher: Option<String>,
    /// Publication date as the store reports it.
    pub published_at: Option<String>,
    /// Runtime in whole minutes.
    pub length_minutes: Option<i64>,
    /// Language code (`en`).
    pub language: Option<String>,
    /// Cover image URL.
    pub cover_url: Option<String>,
    /// Product page URL.
    pub url: Option<String>,
    /// Community rating (0 to 5).
    pub rating_overall: Option<f64>,
    /// How many ratings back the rating.
    pub rating_count: Option<i64>,
    /// Abridged (`true`), unabridged (`false`) or unknown.
    pub is_abridged: Option<bool>,
}

/// One title a store's catalog answer reported.
#[derive(Debug, Clone)]
pub struct CatalogListing {
    /// What the resolver sees: key, kinds, title, identifiers, credits and
    /// series. Its plugin key and marketplace must be set.
    pub draft: resolve::ListingDraft,
    /// The store's own text for it.
    pub detail: ListingDetail,
    /// Prices the answer quoted (often none: search hits rarely price).
    pub offers: Vec<ListingOffer>,
}

/// A stored catalog listing, as Discover and the wishlist read it.
#[derive(Debug, Clone)]
pub struct CatalogListingRecord {
    /// `store_listings.id`.
    pub id: String,
    /// Provenance-qualified PluginKey of the store.
    pub plugin_key: String,
    /// Lower-cased marketplace.
    pub marketplace: String,
    /// The store's product id.
    pub product_id: String,
    /// The edition the listing sells.
    pub edition_id: String,
    /// That edition's work, when the rows are consistent.
    pub work_id: Option<String>,
    /// Title as the store lists it.
    pub title: String,
    /// Store category labels.
    pub categories: Option<String>,
    /// The store's own text.
    pub detail: ListingDetail,
    /// ASIN of the edition, preferring one scoped to the marketplace.
    pub asin: Option<String>,
    /// An ISBN of the edition.
    pub isbn: Option<String>,
    /// Stored prices, one per audience.
    pub offers: Vec<ListingOffer>,
    /// When a store last reported the listing.
    pub observed_at: DateTime<Utc>,
}

impl CatalogListingRecord {
    /// The stored offer for `audience`.
    #[must_use]
    pub fn offer(&self, audience: OfferAudience) -> Option<&ListingOffer> {
        self.offers.iter().find(|o| o.audience == audience)
    }
}

/// Text bind for an optional value, trimmed; blank becomes NULL.
fn text_trim(value: Option<&str>) -> bookclerk_plugin_abi::DbValue {
    batch::text_opt(value.map(str::trim).filter(|v| !v.is_empty()))
}

/// Real bind, or NULL when absent or not finite.
fn real_opt(value: Option<f64>) -> bookclerk_plugin_abi::DbValue {
    match value.filter(|v| v.is_finite()) {
        Some(v) => bookclerk_plugin_abi::DbValue::Float64(v),
        None => bookclerk_plugin_abi::DbValue::Null(bookclerk_plugin_abi::DbType::Float64),
    }
}

/// The statement that fills a listing's store text, keeping a stored
/// value the store left out this time.
fn detail_update(listing_id: &str, detail: &ListingDetail, now: &str) -> TypedDbStatement {
    batch::exec(
        "UPDATE store_listings SET \
         subtitle = COALESCE(?, subtitle), authors = COALESCE(?, authors), \
         narrators = COALESCE(?, narrators), series = COALESCE(?, series), \
         series_index = COALESCE(?, series_index), description = COALESCE(?, description), \
         publisher = COALESCE(?, publisher), published_at = COALESCE(?, published_at), \
         length_minutes = COALESCE(?, length_minutes), language = COALESCE(?, language), \
         cover_url = COALESCE(?, cover_url), url = COALESCE(?, url), \
         rating_overall = COALESCE(?, rating_overall), rating_count = COALESCE(?, rating_count), \
         is_abridged = COALESCE(?, is_abridged), observed_at = ?, updated_at = ? \
         WHERE id = ?",
        vec![
            text_trim(detail.subtitle.as_deref()),
            text_trim(detail.authors.as_deref()),
            text_trim(detail.narrators.as_deref()),
            text_trim(detail.series.as_deref()),
            text_trim(detail.series_index.as_deref()),
            text_trim(detail.description.as_deref()),
            text_trim(detail.publisher.as_deref()),
            text_trim(detail.published_at.as_deref()),
            batch::int_opt(detail.length_minutes),
            text_trim(detail.language.as_deref()),
            text_trim(detail.cover_url.as_deref()),
            text_trim(detail.url.as_deref()),
            real_opt(detail.rating_overall),
            batch::int_opt(detail.rating_count),
            batch::int_opt(detail.is_abridged.map(i64::from)),
            batch::text(now),
            batch::text(now),
            batch::text(listing_id),
        ],
    )
}

/// The two statements that upsert one offer: `UPDATE`, then
/// `INSERT OR IGNORE` (canonical SQL-v1 has no `DO UPDATE`). An offer
/// older than the stored one does not overwrite it.
fn offer_upsert(listing_id: &str, offer: &ListingOffer) -> [TypedDbStatement; 2] {
    let at = offer.observed_at.to_rfc3339();
    [
        batch::exec(
            "UPDATE listing_offers SET price_cents = ?, price_label = ?, currency = ?, \
             observed_at = ? WHERE listing_id = ? AND audience = ? AND observed_at <= ?",
            vec![
                batch::int_opt(offer.price_cents),
                text_trim(offer.price_label.as_deref()),
                text_trim(offer.currency.as_deref()),
                batch::text(&at),
                batch::text(listing_id),
                batch::text(offer.audience.as_str()),
                batch::text(&at),
            ],
        ),
        batch::exec(
            "INSERT OR IGNORE INTO listing_offers \
             (listing_id, audience, price_cents, price_label, currency, observed_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                batch::text(listing_id),
                batch::text(offer.audience.as_str()),
                batch::int_opt(offer.price_cents),
                text_trim(offer.price_label.as_deref()),
                text_trim(offer.currency.as_deref()),
                batch::text(&at),
            ],
        ),
    ]
}

/// The record a stored listing row reads as, before its work, identifiers
/// and offers are attached.
fn record_of(m: store_listings::Model) -> CatalogListingRecord {
    CatalogListingRecord {
        observed_at: parse_dt(&m.observed_at),
        detail: ListingDetail {
            subtitle: m.subtitle,
            authors: m.authors,
            narrators: m.narrators,
            series: m.series,
            series_index: m.series_index,
            description: m.description,
            publisher: m.publisher,
            published_at: m.published_at,
            length_minutes: m.length_minutes,
            language: m.language,
            cover_url: m.cover_url,
            url: m.url,
            rating_overall: m.rating_overall,
            rating_count: m.rating_count,
            is_abridged: m.is_abridged.map(|v| v != 0),
        },
        id: m.id,
        plugin_key: m.plugin_key,
        marketplace: m.marketplace,
        product_id: m.product_id,
        edition_id: m.edition_id,
        work_id: None,
        title: m.title,
        categories: m.categories,
        asin: None,
        isbn: None,
        offers: Vec::new(),
    }
}

/// What [`LibraryStore::record_catalog_listings`] stored for one answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordedListings {
    /// One entry per input, in order: the listing id, or `None` for a title
    /// that was not stored.
    pub ids: Vec<Option<String>>,
    /// How many valid titles could not be resolved this time (other writers
    /// kept changing their rows on every attempt). An answer with any is incomplete and should not be
    /// cached, so those titles are tried again.
    pub unresolved: usize,
}

impl LibraryStore {
    /// Stores what a store's catalog answer reported and returns each
    /// title's listing id.
    ///
    /// A title whose listing a store reported within
    /// [`CATALOG_LISTING_REFRESH`] keeps its resolution and text, and only
    /// its offers are written. Any other title is resolved into the book
    /// model as a catalog listing ([`Self::record_catalog_listing`]: work,
    /// expression, edition and listing row, no holding), then its store
    /// text is filled. Text and offers go to the database plugin as atomic
    /// host batches sized from its limits.
    ///
    /// # Arguments
    ///
    /// * `listings` - The titles, in the store's order.
    ///
    /// # Returns
    ///
    /// The listing ids, one per input in order. A title that does not form
    /// a valid listing (a blank title or key, or a field over the listing
    /// limits) gets `None`; so does one whose rows other writers kept
    /// changing on every attempt, which is also counted in
    /// [`RecordedListings::unresolved`] and does not stop the rest of the
    /// answer. A stored title that was not placed stays stale, so the next
    /// answer resolves it again.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a read or write of the answer's rows
    /// fails, including the resolver's own reads and writes; titles before
    /// the failure may already be stored.
    pub async fn record_catalog_listings(
        &self,
        listings: &[CatalogListing],
    ) -> Result<RecordedListings> {
        let parsed: Vec<Option<resolve::Listing>> = listings
            .iter()
            .map(|l| match resolve::Listing::from_draft(&l.draft) {
                Ok(listing) => Some(listing),
                Err(err) => {
                    tracing::debug!(
                        product = %l.draft.product_id,
                        error = %err,
                        "skip a catalog title that is not a valid listing"
                    );
                    None
                }
            })
            .collect();
        let products: Vec<String> = parsed
            .iter()
            .flatten()
            .map(|l| l.key.product_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut stored: HashMap<(String, String, String), store_listings::Model> = HashMap::new();
        for chunk in products.chunks(self.lookup_chunk()) {
            for row in store_listings::Entity::find()
                .filter(store_listings::Column::ProductId.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                stored.insert(
                    (
                        row.plugin_key.clone(),
                        row.marketplace.clone(),
                        row.product_id.clone(),
                    ),
                    row,
                );
            }
        }
        let now = Utc::now();
        let now_text = now_str();
        let is_fresh = |row: &store_listings::Model| {
            chrono::DateTime::parse_from_rfc3339(&row.observed_at)
                .is_ok_and(|seen| now.signed_duration_since(seen) <= CATALOG_LISTING_REFRESH)
        };
        // Stale rows this answer names are touched first, in their own
        // batches, so a concurrent prune (which only deletes old rows)
        // cannot remove one between the resolver reusing it and the writes
        // below that reference it. A row pruned before the touch is simply
        // resolved again. The touch moves a row to just past the refresh
        // window, not to now: far from the prune cutoff, yet still stale, so
        // a title whose resolution fails below is resolved again next time
        // instead of passing as fresh. A resolved row gets `now` from its
        // detail write.
        // `stored` was read by product id alone, so it can hold other
        // stores' rows sharing an id (an ISBN); only this answer's keys are
        // touched.
        let named: HashSet<(String, String, String)> = parsed
            .iter()
            .flatten()
            .map(|l| {
                (
                    l.key.plugin_key.clone(),
                    l.key.marketplace.clone(),
                    l.key.product_id.clone(),
                )
            })
            .collect();
        let stale: Vec<String> = stored
            .iter()
            .filter(|(key, row)| named.contains(*key) && !is_fresh(row))
            .map(|(_, row)| row.id.clone())
            .collect();
        let touched_text =
            (now - CATALOG_LISTING_REFRESH - chrono::Duration::seconds(1)).to_rfc3339();
        for chunk in stale.chunks(self.lookup_chunk()) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut params = vec![batch::text(&touched_text)];
            params.extend(chunk.iter().map(|id| batch::text(id)));
            self.run_catalog_batch(vec![batch::exec(
                &format!("UPDATE store_listings SET observed_at = ? WHERE id IN ({marks})"),
                params,
            )])
            .await?;
        }
        let mut ids = Vec::with_capacity(listings.len());
        let mut batches = WriteBatches::new(self.db_capabilities(), "catalog listing");
        let mut done: HashSet<String> = HashSet::new();
        // Titles resolved earlier in this call, so a second copy in the
        // same answer is not sent through the resolver again.
        let mut resolved_now: HashMap<(String, String, String), String> = HashMap::new();
        let mut unresolved = 0usize;
        for (input, listing) in listings.iter().zip(&parsed) {
            let Some(listing) = listing else {
                ids.push(None);
                continue;
            };
            let key = (
                listing.key.plugin_key.clone(),
                listing.key.marketplace.clone(),
                listing.key.product_id.clone(),
            );
            let known = resolved_now.get(&key).cloned().or_else(|| {
                stored
                    .get(&key)
                    .filter(|row| is_fresh(row))
                    .map(|row| row.id.clone())
            });
            let (id, resolved) = match known {
                Some(id) => (id, false),
                None => match self.try_link_listing(listing, None).await? {
                    Some(plan) => {
                        let id = plan.listing.id;
                        resolved_now.insert(key.clone(), id.clone());
                        (id, true)
                    }
                    None => {
                        // Other writers kept changing this title's rows; that
                        // one title does not cost the answer. Any other
                        // failure fails the call above.
                        tracing::warn!(
                            product = %listing.key.product_id,
                            "a catalog title kept changing under other writers; it is tried again next time"
                        );
                        unresolved += 1;
                        ids.push(None);
                        continue;
                    }
                },
            };
            // A title an answer lists twice is written once, with the
            // first copy's text and every copy's offers.
            if resolved && done.insert(id.clone()) {
                if let Some(full) = batches.push(detail_update(&id, &input.detail, &now_text))? {
                    self.run_catalog_batch(full).await?;
                }
            }
            for offer in input.offers.iter().filter(|o| !o.is_empty()) {
                if let Some(full) = batches.push_group(offer_upsert(&id, offer).to_vec())? {
                    self.run_catalog_batch(full).await?;
                }
            }
            ids.push(Some(id));
        }
        if let Some(last) = batches.finish() {
            self.run_catalog_batch(last).await?;
        }
        Ok(RecordedListings { ids, unresolved })
    }

    /// Stores prices a store quoted for listings already in the pool.
    ///
    /// # Arguments
    ///
    /// * `offers` - `(listing id, offer)` pairs. An offer older than the
    ///   stored one for the same audience is ignored; one for a listing no
    ///   longer stored fails its foreign key and is dropped with its batch,
    ///   while the other batches are still written.
    ///
    /// # Errors
    ///
    /// Returns the first batch's database error, after every batch was
    /// tried.
    pub async fn record_listing_offers(&self, offers: &[(String, ListingOffer)]) -> Result<()> {
        let mut batches = WriteBatches::new(self.db_capabilities(), "listing offer");
        let mut first_error = None;
        for (listing_id, offer) in offers.iter().filter(|(_, o)| !o.is_empty()) {
            if let Some(full) = batches.push_group(offer_upsert(listing_id, offer).to_vec())? {
                if let Err(err) = self.run_catalog_batch(full).await {
                    first_error.get_or_insert(err);
                }
            }
        }
        if let Some(last) = batches.finish() {
            if let Err(err) = self.run_catalog_batch(last).await {
                first_error.get_or_insert(err);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Sends one chunk of catalog writes as one atomic host batch.
    async fn run_catalog_batch(&self, statements: Vec<TypedDbStatement>) -> Result<()> {
        let operation_id = format!("catalog-listings-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(())
    }

    /// Reads stored listings by id, with their work, identifiers and offers.
    ///
    /// Rows are read in chunks bounded by the backend's bind limit; a chunk
    /// whose result would pass the database plugin's `maxResultBytes`
    /// (long synopses) is split and read again.
    ///
    /// # Arguments
    ///
    /// * `ids` - `store_listings` ids; duplicates are read once.
    ///
    /// # Returns
    ///
    /// The listings found, in the order of `ids` (first occurrence); ids no
    /// longer stored are left out.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn catalog_listings(&self, ids: &[String]) -> Result<Vec<CatalogListingRecord>> {
        let unique: Vec<String> = {
            let mut seen = BTreeSet::new();
            ids.iter()
                .filter(|id| seen.insert(id.as_str()))
                .cloned()
                .collect()
        };
        let mut rows: HashMap<String, CatalogListingRecord> = HashMap::new();
        let mut pending: Vec<Vec<String>> = unique
            .chunks(self.lookup_chunk())
            .map(<[String]>::to_vec)
            .collect();
        while let Some(chunk) = pending.pop() {
            let read = store_listings::Entity::find()
                .filter(store_listings::Column::Id.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm);
            match read {
                Ok(found) => {
                    for m in found {
                        rows.insert(m.id.clone(), record_of(m));
                    }
                }
                Err(err) if super::books_page::is_result_too_large(&err) && chunk.len() > 1 => {
                    let (a, b) = chunk.split_at(chunk.len() / 2);
                    pending.push(a.to_vec());
                    pending.push(b.to_vec());
                }
                Err(err) => return Err(err),
            }
        }
        let editions: BTreeSet<String> = rows.values().map(|r| r.edition_id.clone()).collect();
        let edition_list: Vec<String> = editions.iter().cloned().collect();
        let work_of = self.works_of_editions(editions).await?;
        let mut identifiers: HashMap<String, Vec<edition_identifiers::Model>> = HashMap::new();
        for chunk in edition_list.chunks(self.lookup_chunk()) {
            for m in edition_identifiers::Entity::find()
                .filter(edition_identifiers::Column::EditionId.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                identifiers.entry(m.edition_id.clone()).or_default().push(m);
            }
        }
        let listing_ids: Vec<String> = rows.keys().cloned().collect();
        let mut offers: HashMap<String, Vec<ListingOffer>> = HashMap::new();
        for chunk in listing_ids.chunks(self.lookup_chunk()) {
            for m in listing_offers::Entity::find()
                .filter(listing_offers::Column::ListingId.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                let Some(audience) = OfferAudience::parse(&m.audience) else {
                    continue;
                };
                offers.entry(m.listing_id).or_default().push(ListingOffer {
                    audience,
                    price_cents: m.price_cents,
                    price_label: m.price_label,
                    currency: m.currency,
                    observed_at: parse_dt(&m.observed_at),
                });
            }
        }
        for record in rows.values_mut() {
            record.work_id = work_of.get(&record.edition_id).cloned();
            if let Some(ids) = identifiers.get(&record.edition_id) {
                let asins = || ids.iter().filter(|m| m.scheme == "asin");
                record.asin = asins()
                    .find(|m| m.marketplace == record.marketplace)
                    .or_else(|| asins().find(|m| m.marketplace.is_empty()))
                    .or_else(|| asins().next())
                    .map(|m| m.value.clone());
                record.isbn = ids
                    .iter()
                    .filter(|m| m.scheme == "isbn")
                    .map(|m| m.value.clone())
                    .min();
            }
            if let Some(mut list) = offers.remove(&record.id) {
                list.sort_by_key(|o| o.audience);
                record.offers = list;
            }
        }
        Ok(unique.iter().filter_map(|id| rows.remove(id)).collect())
    }

    /// Deletes catalog listings nothing refers to any more.
    ///
    /// A listing is deleted when no store reported it since `cutoff` and
    /// nobody holds it, no Discover subject has it as a candidate, no
    /// cached storefront call lists it and no resolution override names
    /// it. Its offers go with it. The edition, expression and work it
    /// made stay, since other rows (a wish, a later scan) may use them.
    ///
    /// # Returns
    ///
    /// How many listings were deleted.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn prune_catalog_listings(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let cutoff = cutoff.to_rfc3339();
        let mut total = 0;
        loop {
            let statement = batch::exec(
                &format!(
                    "DELETE FROM store_listings WHERE id IN (\
                     SELECT l.id FROM store_listings l WHERE l.observed_at < ? \
                     AND NOT EXISTS (SELECT 1 FROM holdings h WHERE h.listing_id = l.id) \
                     AND NOT EXISTS (SELECT 1 FROM discover_candidates c WHERE c.listing_id = l.id) \
                     AND NOT EXISTS (SELECT 1 FROM discover_store_call_hits s WHERE s.listing_id = l.id) \
                     AND NOT EXISTS (SELECT 1 FROM resolution_overrides o \
                       WHERE o.subject_kind = 'listing' AND o.subject_id = l.id) \
                     LIMIT {PRUNE_BATCH})"
                ),
                vec![batch::text(&cutoff)],
            );
            let operation_id = format!("catalog-prune-{}", uuid::Uuid::new_v4());
            let reply = self
                .execute_host_batch(batch::request(&operation_id, vec![statement]))
                .await?;
            let deleted = batch::rows_affected(&reply, 0);
            total += deleted;
            if deleted < PRUNE_BATCH {
                return Ok(total);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::holdings;
    use crate::store::discover::{CandidateRef, DiscoverSubject, SeedScope};
    use sea_orm::PaginatorTrait as _;

    /// An empty in-memory library with the host schema.
    async fn store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    /// A Libro.fm-style catalog title with an ISBN, an author and a narrator.
    fn title(product: &str) -> CatalogListing {
        CatalogListing {
            draft: resolve::ListingDraft {
                plugin_key: String::from("libro"),
                marketplace: Some(String::from("us")),
                product_id: product.to_string(),
                title: format!("Title {product}"),
                identifiers: vec![(resolve::IdentifierScheme::Isbn, product.to_string(), None)],
                credits: vec![
                    (
                        String::from("Ann Author"),
                        resolve::CreditRole::Author,
                        None,
                    ),
                    (
                        String::from("Ned Narrator"),
                        resolve::CreditRole::Narrator,
                        None,
                    ),
                ],
                ..resolve::ListingDraft::default()
            },
            detail: ListingDetail {
                authors: Some(String::from("Ann Author")),
                narrators: Some(String::from("Ned Narrator")),
                description: Some(format!("About {product}.")),
                cover_url: Some(format!("https://covers.example/{product}.jpg")),
                rating_overall: Some(4.5),
                is_abridged: Some(false),
                ..ListingDetail::default()
            },
            offers: vec![ListingOffer {
                audience: OfferAudience::NonMember,
                price_cents: Some(1499),
                price_label: Some(String::from("$14.99")),
                currency: Some(String::from("USD")),
                observed_at: Utc::now(),
            }],
        }
    }

    #[tokio::test]
    async fn catalog_titles_resolve_into_the_pool_and_read_back() {
        let store = store().await;
        let mut bad = title("9780000000002");
        bad.draft.title = String::from("  ");
        let ids = store
            .record_catalog_listings(&[title("9780000000001"), bad])
            .await
            .unwrap()
            .ids;
        assert!(ids[1].is_none(), "a blank title is not a listing");
        let id = ids[0].clone().unwrap();
        let read = store
            .catalog_listings(&[id.clone(), id.clone()])
            .await
            .unwrap();
        assert_eq!(read.len(), 1);
        let got = &read[0];
        assert_eq!(got.product_id, "9780000000001");
        assert_eq!(got.title, "Title 9780000000001");
        assert_eq!(got.isbn.as_deref(), Some("9780000000001"));
        assert!(got.work_id.is_some(), "the resolver placed it in a work");
        assert_eq!(
            got.detail.description.as_deref(),
            Some("About 9780000000001.")
        );
        assert_eq!(got.detail.is_abridged, Some(false));
        assert_eq!(got.detail.rating_overall, Some(4.5));
        let offer = got.offer(OfferAudience::NonMember).unwrap();
        assert_eq!(offer.price_cents, Some(1499));
        assert!(got.offer(OfferAudience::Member).is_none());
        assert_eq!(
            holdings::Entity::find().count(store.db()).await.unwrap(),
            0,
            "nobody holds a catalog listing"
        );
    }

    #[tokio::test]
    async fn a_fresh_listing_keeps_its_text_and_takes_new_prices() {
        let store = store().await;
        let first = title("9780000000001");
        let id = store
            .record_catalog_listings(std::slice::from_ref(&first))
            .await
            .unwrap()
            .ids[0]
            .clone()
            .unwrap();
        let mut again = first;
        again.detail.description = Some(String::from("Changed."));
        again.offers = vec![ListingOffer {
            audience: OfferAudience::Member,
            price_cents: Some(999),
            price_label: None,
            currency: Some(String::from("USD")),
            observed_at: Utc::now(),
        }];
        let again_id = store.record_catalog_listings(&[again]).await.unwrap().ids[0]
            .clone()
            .unwrap();
        assert_eq!(again_id, id);
        let got = store.catalog_listings(&[id]).await.unwrap().remove(0);
        assert_eq!(
            got.detail.description.as_deref(),
            Some("About 9780000000001.")
        );
        assert_eq!(
            got.offer(OfferAudience::Member).unwrap().price_cents,
            Some(999)
        );
        assert_eq!(
            got.offer(OfferAudience::NonMember).unwrap().price_cents,
            Some(1499)
        );
        // An older quote never overwrites a newer one.
        store
            .record_listing_offers(&[(
                got.id.clone(),
                ListingOffer {
                    audience: OfferAudience::Member,
                    price_cents: Some(1),
                    price_label: None,
                    currency: None,
                    observed_at: Utc::now() - chrono::Duration::days(3),
                },
            )])
            .await
            .unwrap();
        let got = store.catalog_listings(&[got.id]).await.unwrap().remove(0);
        assert_eq!(
            got.offer(OfferAudience::Member).unwrap().price_cents,
            Some(999)
        );
    }

    #[tokio::test]
    async fn a_scan_of_a_catalog_listing_adds_only_the_holding() {
        let store = store().await;
        let catalog = title("9780000000001");
        let id = store
            .record_catalog_listings(std::slice::from_ref(&catalog))
            .await
            .unwrap()
            .ids[0]
            .clone()
            .unwrap();
        let listing = resolve::Listing::from_draft(&catalog.draft).unwrap();
        // A holding is only written for an account the library has.
        store
            .upsert_account(&listing.key.plugin_key, "a1", "us", None, true, "store")
            .await
            .unwrap();
        let holding = resolve::HoldingDraft {
            holder: resolve::Holder::Account(String::from("a1")),
            entitlement: resolve::Entitlement::Purchase,
            purchased_at: None,
        };
        let plan = store.record_listing(&listing, &holding).await.unwrap();
        assert_eq!(plan.listing.id, id);
        assert!(plan.holding_is_new);
        assert_eq!(holdings::Entity::find().count(store.db()).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn long_synopses_read_back_under_the_result_cap() {
        let store = store().await;
        let titles: Vec<CatalogListing> = (0..240)
            .map(|i| {
                let mut t = title(&format!("p{i:04}"));
                t.draft.identifiers.clear();
                t.detail.description = Some("x".repeat(3_000));
                t
            })
            .collect();
        let ids: Vec<String> = store
            .record_catalog_listings(&titles)
            .await
            .unwrap()
            .ids
            .into_iter()
            .map(Option::unwrap)
            .collect();
        let read = store.catalog_listings(&ids).await.unwrap();
        assert_eq!(read.len(), 240);
        assert_eq!(read[0].id, ids[0]);
        assert_eq!(read[239].id, ids[239]);
        assert!(read
            .iter()
            .all(|r| r.detail.description.as_ref().unwrap().len() == 3_000));
    }

    #[tokio::test]
    async fn an_answer_touches_only_its_own_stale_listings() {
        use sea_orm::sea_query::Expr;
        let store = store().await;
        let libro = title("9780000000001");
        let mut chirp = title("9780000000001");
        chirp.draft.plugin_key = String::from("chirp");
        let ids = store
            .record_catalog_listings(&[libro, chirp.clone()])
            .await
            .unwrap()
            .ids;
        let old = "2000-01-01T00:00:00+00:00";
        store_listings::Entity::update_many()
            .col_expr(store_listings::Column::ObservedAt, Expr::value(old))
            .exec(store.db())
            .await
            .unwrap();
        // Chirp answers again; Libro.fm's listing with the same ISBN product
        // id is not touched.
        let again = store.record_catalog_listings(&[chirp]).await.unwrap();
        assert_eq!(again.unresolved, 0);
        let seen = |id: &Option<String>| {
            let id = id.clone().unwrap();
            let store = &store;
            async move {
                store_listings::Entity::find_by_id(id)
                    .one(store.db())
                    .await
                    .unwrap()
                    .unwrap()
                    .observed_at
            }
        };
        assert_eq!(seen(&ids[0]).await, old, "another store's row stays old");
        assert_ne!(seen(&ids[1]).await, old, "the answer's own row is touched");
    }

    #[tokio::test]
    async fn a_failed_resolution_fails_the_call_and_leaves_the_listing_stale() {
        use sea_orm::sea_query::Expr;
        use sea_orm::ConnectionTrait as _;
        let store = store().await;
        let listing = title("9780000000002");
        let id = store
            .record_catalog_listings(std::slice::from_ref(&listing))
            .await
            .unwrap()
            .ids[0]
            .clone()
            .unwrap();
        let old = "2000-01-01T00:00:00+00:00";
        store_listings::Entity::update_many()
            .col_expr(store_listings::Column::ObservedAt, Expr::value(old))
            .exec(store.db())
            .await
            .unwrap();
        // The resolver's snapshot read now fails: that is a failure of the
        // call, not a title skipped for concurrent writers.
        store
            .db()
            .execute_unprepared("DROP TABLE resolution_overrides")
            .await
            .unwrap();
        let refused = store.record_catalog_listings(&[listing]).await;
        assert!(refused.is_err(), "{refused:?}");
        // The row was touched away from the prune cutoff, but it is still
        // stale, so the next answer resolves it instead of reusing it.
        let seen = store_listings::Entity::find_by_id(id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .observed_at;
        let seen = chrono::DateTime::parse_from_rfc3339(&seen).unwrap();
        let age = Utc::now().signed_duration_since(seen);
        assert!(age > CATALOG_LISTING_REFRESH, "still stale: {age}");
        assert!(
            age < chrono::Duration::days(2),
            "kept from the prune: {age}"
        );
    }

    #[tokio::test]
    async fn only_unreferenced_old_listings_are_pruned() {
        let store = store().await;
        let ids: Vec<String> = store
            .record_catalog_listings(&[title("9780000000001"), title("9780000000002")])
            .await
            .unwrap()
            .ids
            .into_iter()
            .map(Option::unwrap)
            .collect();
        store
            .replace_discover_candidates(
                &DiscoverSubject::User(1),
                "libro",
                SeedScope::Own,
                &[CandidateRef {
                    listing_id: ids[0].clone(),
                    origin: String::new(),
                    seed_title: None,
                    seed_categories: None,
                    starter_genre: None,
                }],
            )
            .await
            .unwrap();
        // Nothing is old enough yet.
        assert_eq!(
            store
                .prune_catalog_listings(Utc::now() - chrono::Duration::days(1))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .prune_catalog_listings(Utc::now() + chrono::Duration::days(1))
                .await
                .unwrap(),
            1
        );
        let left = store.catalog_listings(&ids).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, ids[0], "the candidate's listing stays");
        assert_eq!(
            listing_offers::Entity::find()
                .count(store.db())
                .await
                .unwrap(),
            1,
            "the pruned listing's offer went with it"
        );
    }
}
