//! The library model's view of one title an account holds, as the acquire
//! pipeline reads it (book model step 6, PR 4c).
//!
//! An [`AcquireTitle`] is built from the account's holding of a store
//! listing, the listing's store text, the edition it sells, that edition's
//! identifiers and its acquire state.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::LibraryStore;
use crate::entities::{
    accounts, edition_acquisitions, edition_identifiers, editions, expressions, holdings,
    store_listings, works,
};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;

/// The spellings of `value` an exact, indexed comparison tries: as given,
/// ASCII-lowercased and ASCII-uppercased, without duplicates.
///
/// Store ids are compared case-insensitively by the callers. Stored ids are
/// uniformly cased in practice (ASINs upper, uuids lower, ISBN digits), so
/// these three spellings find a row through its index where `lower(col)`
/// would scan the table; callers still check the match case-insensitively.
fn case_variants(value: &str) -> Vec<String> {
    let mut out = vec![value.to_string()];
    for variant in [value.to_ascii_lowercase(), value.to_ascii_uppercase()] {
        if !out.contains(&variant) {
            out.push(variant);
        }
    }
    out
}

/// One title an account holds, as acquire reads it from the library model.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquireTitle {
    /// `holdings.id` of the account's holding.
    pub holding_id: String,
    /// The edition the listing sells: the key the title's files and acquire
    /// state belong to.
    pub edition_id: String,
    /// `store_listings.id`.
    pub listing_id: String,
    /// Provenance-qualified PluginKey of the store.
    pub plugin_key: String,
    /// Display alias of the store's plugin (`audible`, `libro`, …), as the
    /// account records it.
    pub source: String,
    /// The holding account's id at that store.
    pub account_id: String,
    /// Lower-cased marketplace (empty for a store with one).
    pub marketplace: String,
    /// The store's own product id: what its download and fetch APIs take.
    pub product_id: String,
    /// The edition's ASIN, preferring one scoped to the marketplace.
    pub asin: Option<String>,
    /// The edition's lowest ISBN, when it has one.
    pub isbn: Option<String>,
    /// Title as the store lists it.
    pub title: String,
    /// Subtitle as the store lists it, else the edition's.
    pub subtitle: Option<String>,
    /// Authors as the store prints them, comma-separated.
    pub authors: Option<String>,
    /// Narrators as the store prints them, comma-separated.
    pub narrators: Option<String>,
    /// Series name as the store lists it.
    pub series: Option<String>,
    /// Position in that series as the store writes it.
    pub series_index: Option<String>,
    /// The store's product id for the series or podcast show.
    pub series_product_id: Option<String>,
    /// Publisher as the store lists it.
    pub publisher: Option<String>,
    /// Store category labels (serialized as the scan stored them).
    pub categories: Option<String>,
    /// Running time in whole minutes, from the store or the edition.
    pub length_minutes: Option<i64>,
    /// Language as the store lists it.
    pub language: Option<String>,
    /// True when the store or the expression marks it abridged.
    pub is_abridged: bool,
    /// What the title is, in the scan's `content_kind` labels: `episode` for a
    /// podcast episode, `podcast` for a show, `book` otherwise.
    pub content_kind: &'static str,
    /// When the account bought the title, when the store reported it.
    pub purchased_at: Option<DateTime<Utc>>,
    /// When this library first recorded the account's holding of the
    /// title; the added date when the store reports no purchase time.
    pub added_at: Option<DateTime<Utc>>,
    /// Publication time as the store reported it, else the edition's.
    pub published_at: Option<DateTime<Utc>>,
    /// The edition's acquire state; not acquired when no title of it was
    /// ever queued.
    pub status: AcquireStatus,
    /// Primary destination key of the edition's audio once acquired.
    pub storage_key: Option<String>,
    /// The account holding (`holdings.id`) whose title's write set the
    /// edition's audio state; `None` when no title of it was ever queued,
    /// or when that holding no longer exists (its account was revoked or
    /// its title relinked), so no title owns the state any more.
    pub acquired_by: Option<String>,
    /// The edition's companion PDF state.
    pub pdf_status: AcquireStatus,
    /// Destination key of the companion PDF once acquired.
    pub pdf_storage_key: Option<String>,
    /// The account holding (`holdings.id`) whose title's write set the
    /// PDF state; `None` as for [`Self::acquired_by`].
    pub pdf_acquired_by: Option<String>,
}

/// One page of [`LibraryStore::acquire_titles_page`].
#[derive(Debug, Clone, PartialEq)]
pub struct AcquireTitlePage {
    /// The page's titles, in holding order.
    pub titles: Vec<AcquireTitle>,
    /// The cursor of the next page; `None` once the walk is done.
    pub next: Option<String>,
}

impl AcquireTitle {
    /// True when this title's holding set the edition's audio state: the
    /// one whose writes change it once acquired.
    #[must_use]
    pub fn acquired_here(&self) -> bool {
        self.acquired_by.as_deref() == Some(self.holding_id.as_str())
    }

    /// True when this title's holding set the edition's companion PDF
    /// state.
    #[must_use]
    pub fn pdf_acquired_here(&self) -> bool {
        self.pdf_acquired_by.as_deref() == Some(self.holding_id.as_str())
    }

    /// Naming and display fallback: ASIN, else ISBN, else the store's
    /// product id.
    #[must_use]
    pub fn asin_or_isbn(&self) -> &str {
        self.asin
            .as_deref()
            .or(self.isbn.as_deref())
            .unwrap_or(self.product_id.as_str())
    }
}

/// An RFC 3339 or date-only (`YYYY-MM-DD`) time, at midnight UTC for a
/// date; `None` when blank or unreadable.
fn parse_time(text: Option<&str>) -> Option<DateTime<Utc>> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Some(at.with_timezone(&Utc));
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc())
}

/// The rows of the library model one page of holdings reads, keyed for
/// assembling each holding's [`AcquireTitle`].
#[derive(Default)]
struct TitleRows {
    /// Store listings by id.
    listings: HashMap<String, store_listings::Model>,
    /// Editions by id.
    editions: HashMap<String, editions::Model>,
    /// Expressions by id.
    expressions: HashMap<String, expressions::Model>,
    /// Works by id.
    works: HashMap<String, works::Model>,
    /// Each edition's identifiers.
    identifiers: HashMap<String, Vec<edition_identifiers::Model>>,
    /// Edition acquire state by edition id.
    acquisitions: HashMap<String, edition_acquisitions::Model>,
    /// The holdings named by `acquisitions` (audio or PDF writer) that
    /// still exist.
    live_writers: HashSet<String>,
    /// Account display aliases by (plugin key, account).
    sources: HashMap<(String, String), String>,
}

/// Sorted, de-duplicated copy of `ids`.
fn distinct(ids: impl Iterator<Item = String>) -> Vec<String> {
    let mut ids: Vec<String> = ids.collect();
    ids.sort();
    ids.dedup();
    ids
}

/// True when `holding` belongs to one of the accounts `named`
/// (`(plugin key, account id)`).
fn held_by(named: &BTreeSet<(String, String)>, holding: &holdings::Model) -> bool {
    match (holding.plugin_key.as_ref(), holding.account_id.as_ref()) {
        (Some(plugin_key), Some(account_id)) => {
            named.contains(&(plugin_key.clone(), account_id.clone()))
        }
        _ => false,
    }
}

impl LibraryStore {
    /// The accounts an account filter names, as `(plugin_key,
    /// account_id)`: every account whose id or label is `needle`, ignoring
    /// ASCII case, the way the scan executor and the job trust checks
    /// resolve a job's account. A library has few accounts, so they are
    /// read in one query.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    async fn accounts_matching(&self, needle: &str) -> Result<BTreeSet<(String, String)>> {
        Ok(accounts::Entity::find()
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .filter(|row| {
                row.account_id.eq_ignore_ascii_case(needle)
                    || row
                        .label
                        .as_deref()
                        .is_some_and(|label| label.eq_ignore_ascii_case(needle))
            })
            .map(|row| (row.plugin_key, row.account_id))
            .collect())
    }

    /// The title `account_id` holds at a store, as acquire reads it.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The store's PluginKey.
    /// * `account_id` - The account at that store.
    /// * `marketplace` - The store's marketplace (any case; blank for one).
    /// * `product_id` - The store's product id.
    ///
    /// # Returns
    ///
    /// `None` when the store's listing is not resolved yet or the account
    /// does not hold it.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn acquire_title(
        &self,
        plugin_key: &str,
        account_id: &str,
        marketplace: &str,
        product_id: &str,
    ) -> Result<Option<AcquireTitle>> {
        let marketplace = bookclerk_resolve::stored_marketplace(marketplace);
        let product_id = bookclerk_resolve::stored_product_id(product_id);
        let Some(listing) = store_listings::Entity::find()
            .filter(store_listings::Column::PluginKey.eq(plugin_key))
            .filter(store_listings::Column::Marketplace.eq(marketplace.as_str()))
            .filter(store_listings::Column::ProductId.eq(product_id.as_str()))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let Some(holding) = super::holdings::find_holdings()
            .filter(holdings::Column::ListingId.eq(listing.id.as_str()))
            .filter(holdings::Column::HolderKind.eq("account"))
            .filter(holdings::Column::PluginKey.eq(plugin_key))
            .filter(holdings::Column::AccountId.eq(account_id))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(self.acquire_titles_of(vec![holding]).await?.pop())
    }

    /// The titles accounts hold that `id` names, as acquire reads them, in
    /// holding order: what an acquire job given one title visits.
    ///
    /// `id` matches, ignoring ASCII case, a listing's store product id, an
    /// ASIN or ISBN of the listing's edition (every listing of that
    /// edition), or a holding id.
    ///
    /// # Arguments
    ///
    /// * `account_id` - Only the titles of the accounts this id or label
    ///   names (ignoring ASCII case), when set.
    /// * `id` - The product id, ASIN, ISBN, or holding id.
    ///
    /// # Returns
    ///
    /// Every matching account holding's title; empty when nothing matches.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn acquire_titles_named(
        &self,
        account_id: Option<&str>,
        id: &str,
    ) -> Result<Vec<AcquireTitle>> {
        use super::library_model::{find_chunked, find_chunked_paged};
        let id = bookclerk_resolve::stored_product_id(id);
        if id.is_empty() {
            return Ok(Vec::new());
        }
        let named = match account_id {
            Some(needle) => {
                let named = self.accounts_matching(needle).await?;
                if named.is_empty() {
                    return Ok(Vec::new());
                }
                Some(named)
            }
            None => None,
        };
        let account_ids: Vec<String> = named
            .iter()
            .flatten()
            .map(|(_, account)| account.clone())
            .collect();
        let variants = case_variants(&id);
        let mut edition_ids: Vec<String> = edition_identifiers::Entity::find()
            .filter(edition_identifiers::Column::Scheme.is_in(["asin", "isbn"]))
            .filter(edition_identifiers::Column::Value.is_in(variants.clone()))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|m| m.edition_id)
            .collect();
        edition_ids = distinct(edition_ids.into_iter());
        let mut listing_ids: Vec<String> = store_listings::Entity::find()
            .filter(store_listings::Column::ProductId.is_in(variants.clone()))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|m| m.id)
            .collect();
        listing_ids.extend(
            find_chunked::<store_listings::Entity, _>(self, &edition_ids, |part| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::EditionId.is_in(part.to_vec()))
            })
            .await?
            .into_iter()
            .map(|m| m.id),
        );
        let listing_ids = distinct(listing_ids.into_iter());
        let holding_ids = variants;
        let account = |query: sea_orm::Select<holdings::Entity>| match named {
            Some(_) => query.filter(holdings::Column::AccountId.is_in(account_ids.clone())),
            None => query,
        };
        // Every account's holding of a listing may be more rows than one
        // read returns, so the holdings are read in pages.
        let mut holdings = find_chunked_paged::<holdings::Entity, _>(
            self,
            &listing_ids,
            &[holdings::Column::Id],
            |part| {
                account(
                    super::holdings::find_holdings()
                        .filter(holdings::Column::HolderKind.eq("account"))
                        .filter(holdings::Column::ListingId.is_in(part.to_vec())),
                )
            },
        )
        .await?;
        holdings.extend(
            account(
                super::holdings::find_holdings()
                    .filter(holdings::Column::HolderKind.eq("account"))
                    .filter(holdings::Column::Id.is_in(holding_ids)),
            )
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?,
        );
        if let Some(named) = &named {
            // The same id on another store is another account.
            holdings.retain(|holding| held_by(named, holding));
        }
        holdings.sort_by(|a, b| a.id.cmp(&b.id));
        holdings.dedup_by(|a, b| a.id == b.id);
        self.acquire_titles_of(holdings).await
    }

    /// The titles accounts hold of the store listings `listing_ids`, as
    /// acquire reads them, in holding order: every account's copy of those
    /// listings.
    ///
    /// # Arguments
    ///
    /// * `listing_ids` - `store_listings.id` values.
    ///
    /// # Returns
    ///
    /// One title per account holding of a listing; empty when none is held.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn acquire_titles_of_listings(
        &self,
        listing_ids: &[String],
    ) -> Result<Vec<AcquireTitle>> {
        use super::library_model::find_chunked_paged;
        let listing_ids = distinct(listing_ids.iter().cloned());
        // Every account's holding of a listing may be more rows than one
        // read returns, so the holdings are read in pages.
        let mut holdings = find_chunked_paged::<holdings::Entity, _>(
            self,
            &listing_ids,
            &[holdings::Column::Id],
            |part| {
                super::holdings::find_holdings()
                    .filter(holdings::Column::HolderKind.eq("account"))
                    .filter(holdings::Column::ListingId.is_in(part.to_vec()))
            },
        )
        .await?;
        holdings.sort_by(|a, b| a.id.cmp(&b.id));
        self.acquire_titles_of(holdings).await
    }

    /// The held titles of edition `edition_id`: one per account holding of
    /// any of its listings, in holding id order.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn held_titles_of_edition(&self, edition_id: &str) -> Result<Vec<AcquireTitle>> {
        use super::library_model::find_chunked_paged;
        let listings = find_chunked_paged::<store_listings::Entity, _>(
            self,
            &[edition_id.to_string()],
            &[store_listings::Column::Id],
            |part| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::EditionId.is_in(part.to_vec()))
            },
        )
        .await?;
        let ids: Vec<String> = listings.into_iter().map(|l| l.id).collect();
        self.acquire_titles_of_listings(&ids).await
    }

    /// The title of one account holding (`holdings.id`), as acquire reads
    /// it.
    ///
    /// # Returns
    ///
    /// `None` when no account holding has that id (a node holding, a
    /// holding withdrawn, or an id this library never had) or its listing
    /// is gone.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn acquire_title_of_holding(&self, holding_id: &str) -> Result<Option<AcquireTitle>> {
        let Some(holding) = super::holdings::find_holdings()
            .filter(holdings::Column::Id.eq(holding_id))
            .filter(holdings::Column::HolderKind.eq("account"))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(self.acquire_titles_of(vec![holding]).await?.pop())
    }

    /// The accounts that hold at least one title, as `(plugin_key,
    /// account_id)`: what an acquire of every title in its scope touches.
    ///
    /// # Arguments
    ///
    /// * `account_id` - Only the accounts this id or label names (ignoring
    ///   ASCII case), when set.
    /// * `plugin_key` - Only accounts on this plugin, when set.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn accounts_holding_titles(
        &self,
        account_id: Option<&str>,
        plugin_key: Option<&str>,
    ) -> Result<BTreeSet<(String, String)>> {
        let mut query = accounts::Entity::find().order_by_asc(accounts::Column::Id);
        if let Some(plugin_key) = plugin_key {
            query = query.filter(accounts::Column::PluginKey.eq(plugin_key));
        }
        let named = match account_id {
            Some(needle) => Some(self.accounts_matching(needle).await?),
            None => None,
        };
        let mut out = BTreeSet::new();
        // One indexed existence read per account: a library has few
        // accounts, and each may hold more titles than one read returns.
        for account in query.all(self.db()).await.map_err(LibraryError::Orm)? {
            if named.as_ref().is_some_and(|named| {
                !named.contains(&(account.plugin_key.clone(), account.account_id.clone()))
            }) {
                continue;
            }
            let held = super::holdings::find_holdings()
                .filter(holdings::Column::HolderKind.eq("account"))
                .filter(holdings::Column::PluginKey.eq(account.plugin_key.as_str()))
                .filter(holdings::Column::AccountId.eq(account.account_id.as_str()))
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?;
            if held.is_some() {
                out.insert((account.plugin_key, account.account_id));
            }
        }
        Ok(out)
    }

    /// One page of the titles accounts hold, as acquire reads them, in
    /// holding order: what reconcile and storage matching walk.
    ///
    /// # Arguments
    ///
    /// * `account_id` - Only the titles of the accounts this id or label
    ///   names (ignoring ASCII case), when set.
    /// * `after` - The previous page's [`AcquireTitlePage::next`]; `None`
    ///   for the first page.
    /// * `limit` - Most holdings the page reads (1 to 64, and at most the
    ///   database's `maxResultRows`).
    ///
    /// # Returns
    ///
    /// The page's titles and the cursor of the next page. A holding whose
    /// listing is gone has no title, so a page can be short or empty while
    /// more follow: walk until `next` is `None`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn acquire_titles_page(
        &self,
        account_id: Option<&str>,
        after: Option<&str>,
        limit: u64,
    ) -> Result<AcquireTitlePage> {
        let want = match self.db_capabilities().max_result_rows {
            0 => limit.clamp(1, 64),
            rows => limit.clamp(1, 64).min(u64::from(rows).max(1)),
        };
        let mut query = super::holdings::find_holdings()
            .filter(holdings::Column::HolderKind.eq("account"))
            .order_by_asc(holdings::Column::Id)
            .limit(want);
        let named = match account_id {
            Some(needle) => {
                let named = self.accounts_matching(needle).await?;
                if named.is_empty() {
                    return Ok(AcquireTitlePage {
                        titles: Vec::new(),
                        next: None,
                    });
                }
                let ids: Vec<String> = named.iter().map(|(_, id)| id.clone()).collect();
                query = query.filter(holdings::Column::AccountId.is_in(ids));
                Some(named)
            }
            None => None,
        };
        if let Some(after) = after {
            query = query.filter(holdings::Column::Id.gt(after));
        }
        let mut holdings = query.all(self.db()).await.map_err(LibraryError::Orm)?;
        let full = u64::try_from(holdings.len()).unwrap_or(u64::MAX) >= want;
        let next = holdings
            .last()
            .filter(|_| full)
            .map(|holding| holding.id.clone());
        if let Some(named) = &named {
            // The same id on another store is another account.
            holdings.retain(|holding| held_by(named, holding));
        }
        Ok(AcquireTitlePage {
            titles: self.acquire_titles_of(holdings).await?,
            next,
        })
    }

    /// Every account title, in holding order: [`Self::acquire_titles_page`]
    /// walked to its end.
    ///
    /// # Arguments
    ///
    /// * `account_id` - Only the titles of the accounts this id or label
    ///   names (ignoring ASCII case), when set.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn all_acquire_titles(&self, account_id: Option<&str>) -> Result<Vec<AcquireTitle>> {
        let mut titles = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self
                .acquire_titles_page(account_id, after.as_deref(), 64)
                .await?;
            titles.extend(page.titles);
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(titles),
            }
        }
    }

    /// The titles of `holdings` (account holdings), in their order, read in
    /// one chunked query per table.
    pub(crate) async fn acquire_titles_of(
        &self,
        holdings: Vec<holdings::Model>,
    ) -> Result<Vec<AcquireTitle>> {
        if holdings.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self.title_rows(&holdings).await?;
        Ok(holdings
            .into_iter()
            .filter_map(|holding| rows.title(holding))
            .collect())
    }

    /// Reads the rows the titles of `holdings` are built from.
    async fn title_rows(&self, holdings: &[holdings::Model]) -> Result<TitleRows> {
        use super::library_model::{find_chunked, find_chunked_paged};
        let mut rows = TitleRows::default();
        let listing_ids = distinct(holdings.iter().map(|h| h.listing_id.clone()));
        rows.listings = find_chunked::<store_listings::Entity, _>(self, &listing_ids, |part| {
            store_listings::Entity::find().filter(store_listings::Column::Id.is_in(part.to_vec()))
        })
        .await?
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();
        let edition_ids = distinct(rows.listings.values().map(|l| l.edition_id.clone()));
        rows.editions = find_chunked::<editions::Entity, _>(self, &edition_ids, |part| {
            editions::Entity::find().filter(editions::Column::Id.is_in(part.to_vec()))
        })
        .await?
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();
        let expression_ids = distinct(rows.editions.values().map(|e| e.expression_id.clone()));
        rows.expressions = find_chunked::<expressions::Entity, _>(self, &expression_ids, |part| {
            expressions::Entity::find().filter(expressions::Column::Id.is_in(part.to_vec()))
        })
        .await?
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();
        let work_ids = distinct(rows.expressions.values().map(|e| e.work_id.clone()));
        rows.works = find_chunked::<works::Entity, _>(self, &work_ids, |part| {
            works::Entity::find().filter(works::Column::Id.is_in(part.to_vec()))
        })
        .await?
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();
        for identifier in find_chunked_paged::<edition_identifiers::Entity, _>(
            self,
            &edition_ids,
            &[
                edition_identifiers::Column::EditionId,
                edition_identifiers::Column::Scheme,
                edition_identifiers::Column::Marketplace,
                edition_identifiers::Column::Value,
            ],
            |part| {
                edition_identifiers::Entity::find()
                    .filter(edition_identifiers::Column::EditionId.is_in(part.to_vec()))
            },
        )
        .await?
        {
            rows.identifiers
                .entry(identifier.edition_id.clone())
                .or_default()
                .push(identifier);
        }
        rows.acquisitions =
            find_chunked::<edition_acquisitions::Entity, _>(self, &edition_ids, |part| {
                edition_acquisitions::Entity::find()
                    .filter(edition_acquisitions::Column::EditionId.is_in(part.to_vec()))
            })
            .await?
            .into_iter()
            .map(|m| (m.edition_id.clone(), m))
            .collect();
        let writer_ids =
            distinct(rows.acquisitions.values().flat_map(|a| {
                std::iter::once(a.holding_id.clone()).chain(a.pdf_holding_id.clone())
            }));
        rows.live_writers = find_chunked::<holdings::Entity, _>(self, &writer_ids, |part| {
            super::holdings::find_holdings().filter(holdings::Column::Id.is_in(part.to_vec()))
        })
        .await?
        .into_iter()
        .map(|m| m.id)
        .collect();
        let account_ids = distinct(holdings.iter().filter_map(|h| h.account_id.clone()));
        rows.sources = find_chunked_paged::<accounts::Entity, _>(
            self,
            &account_ids,
            &[accounts::Column::Id],
            |part| {
                accounts::Entity::find().filter(accounts::Column::AccountId.is_in(part.to_vec()))
            },
        )
        .await?
        .into_iter()
        .map(|m| ((m.plugin_key, m.account_id), m.source))
        .collect();
        Ok(rows)
    }
}

impl TitleRows {
    /// The title of one account holding; `None` when the holding names no
    /// account or its listing is gone.
    fn title(&self, holding: holdings::Model) -> Option<AcquireTitle> {
        let plugin_key = holding.plugin_key?;
        let account_id = holding.account_id?;
        let listing = self.listings.get(&holding.listing_id)?.clone();
        let edition = self.editions.get(&listing.edition_id);
        let expression = edition.and_then(|e| self.expressions.get(&e.expression_id));
        let work = expression.and_then(|e| self.works.get(&e.work_id));
        let identifiers = self
            .identifiers
            .get(&listing.edition_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let acquisition = self.acquisitions.get(&listing.edition_id);
        let source = self
            .sources
            .get(&(plugin_key.clone(), account_id.clone()))
            .cloned()
            .unwrap_or_default();

        let asins = || identifiers.iter().filter(|m| m.scheme == "asin");
        let asin = asins()
            .find(|m| m.marketplace == listing.marketplace)
            .or_else(|| asins().find(|m| m.marketplace.is_empty()))
            .or_else(|| asins().next())
            .map(|m| m.value.clone());
        let isbn = identifiers
            .iter()
            .filter(|m| m.scheme == "isbn")
            .map(|m| m.value.clone())
            .min();
        let (status, storage_key, acquired_by, pdf_status, pdf_storage_key, pdf_acquired_by) =
            match acquisition {
                Some(row) => (
                    AcquireStatus::parse(&row.status).unwrap_or_default(),
                    row.storage_key.clone(),
                    Some(row.holding_id.clone()).filter(|id| self.live_writers.contains(id)),
                    AcquireStatus::parse(&row.pdf_status).unwrap_or_default(),
                    row.pdf_storage_key.clone(),
                    row.pdf_holding_id
                        .clone()
                        .filter(|id| self.live_writers.contains(id)),
                ),
                None => (
                    AcquireStatus::NotAcquired,
                    None,
                    None,
                    AcquireStatus::NotAcquired,
                    None,
                    None,
                ),
            };
        let (edition_subtitle, edition_published, edition_minutes) = edition
            .map(|e| {
                (
                    e.subtitle.clone(),
                    e.published_at.clone(),
                    e.duration_minutes,
                )
            })
            .unwrap_or_default();
        let abridged_expression = expression
            .is_some_and(|e| e.abridgement == bookclerk_resolve::Abridgement::Abridged.as_str());
        let content_kind = match work {
            Some(w) if w.kind == bookclerk_resolve::WorkKind::Podcast.as_str() => {
                // An episode names its show's product on the listing even
                // before the show's work is linked as its parent.
                if w.parent_work_id.is_some() || listing.series_product_id.is_some() {
                    "episode"
                } else {
                    "podcast"
                }
            }
            _ => "book",
        };
        Some(AcquireTitle {
            holding_id: holding.id,
            edition_id: listing.edition_id,
            listing_id: listing.id,
            plugin_key: listing.plugin_key,
            source,
            account_id,
            marketplace: listing.marketplace,
            product_id: listing.product_id,
            asin,
            isbn,
            title: listing.title,
            subtitle: listing.subtitle.or(edition_subtitle),
            authors: listing.authors,
            narrators: listing.narrators,
            series: listing.series,
            series_index: listing.series_index,
            series_product_id: listing.series_product_id,
            publisher: listing.publisher,
            categories: listing.categories,
            length_minutes: listing.length_minutes.or(edition_minutes),
            language: listing.language,
            is_abridged: listing.is_abridged == Some(1) || abridged_expression,
            content_kind,
            purchased_at: parse_time(holding.purchased_at.as_deref()),
            added_at: parse_time(Some(holding.created_at.as_str())),
            published_at: parse_time(
                listing
                    .published_at
                    .as_deref()
                    .or(edition_published.as_deref()),
            ),
            status,
            storage_key,
            acquired_by,
            pdf_status,
            pdf_storage_key,
            pdf_acquired_by,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewBook;

    #[test]
    fn case_variants_keep_the_given_spelling_first_and_drop_duplicates() {
        assert_eq!(case_variants("b0ABc"), vec!["b0ABc", "b0abc", "B0ABC"]);
        assert_eq!(case_variants("B0ABC"), vec!["B0ABC", "b0abc"]);
        assert_eq!(case_variants("9781"), vec!["9781"]);
    }

    #[test]
    fn times_read_as_rfc3339_or_a_date() {
        assert_eq!(
            parse_time(Some("2024-03-01T10:00:00+02:00")).map(|t| t.to_rfc3339()),
            Some("2024-03-01T08:00:00+00:00".to_string())
        );
        assert_eq!(
            parse_time(Some("2024-03-01")).map(|t| t.to_rfc3339()),
            Some("2024-03-01T00:00:00+00:00".to_string())
        );
        assert_eq!(parse_time(Some("  ")), None);
        assert_eq!(parse_time(Some("March")), None);
        assert_eq!(parse_time(None), None);
    }

    #[tokio::test]
    async fn titles_are_named_by_any_id_under_every_profile() {
        for profile in crate::test_support::Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            for account in ["acct", "other"] {
                store
                    .upsert_account("audible", account, "us", None, true, "audible")
                    .await
                    .unwrap();
                store
                    .upsert_book(&NewBook::minimal("B0NAME001", account, "us", "Named"))
                    .await
                    .unwrap();
            }
            let mut isbn = NewBook::minimal("9780000000017", "acct", "us", "Printed");
            isbn.isbn = Some("9780000000017".into());
            let flat = store.upsert_book(&isbn).await.unwrap();
            let accounts = |titles: Vec<AcquireTitle>| {
                titles
                    .into_iter()
                    .map(|t| (t.account_id, t.product_id))
                    .collect::<Vec<_>>()
            };

            let both = store.acquire_titles_named(None, "b0name001").await.unwrap();
            assert_eq!(both.len(), 2, "{name}");
            let mine = store
                .acquire_titles_named(Some("other"), "B0NAME001")
                .await
                .unwrap();
            assert_eq!(
                accounts(mine.clone()),
                vec![("other".to_string(), "B0NAME001".to_string())],
                "{name}"
            );
            let by_holding = store
                .acquire_titles_named(None, &mine[0].holding_id)
                .await
                .unwrap();
            assert_eq!(accounts(by_holding), accounts(mine), "{name}");
            let by_uuid = store
                .acquire_titles_named(None, &flat.holding_id)
                .await
                .unwrap();
            assert_eq!(
                accounts(by_uuid),
                vec![("acct".to_string(), "9780000000017".to_string())],
                "{name}"
            );
            assert!(store
                .acquire_titles_named(None, "B0NOTHERE")
                .await
                .unwrap()
                .is_empty());
            assert!(store
                .acquire_titles_named(None, "  ")
                .await
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn the_view_carries_what_was_recorded_under_every_profile() {
        for profile in crate::test_support::Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            store
                .upsert_account("audible", "acct", "us", None, true, "audible")
                .await
                .unwrap();
            store
                .upsert_account("audible", "other", "us", None, true, "audible")
                .await
                .unwrap();
            let mut book = NewBook::minimal("B0VIEW001", "acct", "US", "Viewed");
            book.authors = Some("Ann Author".into());
            book.narrators = Some("Ned Narrator".into());
            book.series = Some("Saga".into());
            book.series_index = Some("2".into());
            book.series_asin = Some("B0SAGA".into());
            book.publisher = Some("House".into());
            book.categories = Some("Fantasy".into());
            book.length_minutes = Some(605);
            book.is_abridged = true;
            book.purchased_at = Some("2023-05-06T07:08:09Z".parse().unwrap());
            book.published_at = Some("2020-01-02T00:00:00Z".parse().unwrap());
            let flat = store.upsert_book(&book).await.unwrap();
            store
                .set_acquire_status(
                    "B0VIEW001",
                    "acct",
                    AcquireStatus::Acquired,
                    Some("Ann Author/Viewed.m4b"),
                    None,
                )
                .await
                .unwrap();
            store
                .set_pdf_status("B0VIEW001", "acct", AcquireStatus::Queued, None)
                .await
                .unwrap();

            let view = store
                .acquire_title("audible", "acct", "US", "B0VIEW001")
                .await
                .unwrap()
                .unwrap();
            let name = profile.name();
            assert_eq!(view.holding_id, flat.holding_id, "{name}");
            assert_eq!(
                Some(view.edition_id.clone()),
                store.edition_of_holding(&flat.holding_id).await.unwrap()
            );
            assert_eq!(view.plugin_key, "audible");
            assert_eq!(view.account_id, "acct");
            assert_eq!(view.marketplace, "us");
            assert_eq!(view.product_id, "B0VIEW001");
            assert_eq!(view.asin.as_deref(), Some("B0VIEW001"), "{name}");
            assert_eq!(view.isbn, None);
            assert_eq!(view.asin_or_isbn(), "B0VIEW001");
            assert_eq!(view.title, book.title);
            assert_eq!(view.authors, book.authors);
            assert_eq!(view.narrators, book.narrators);
            assert_eq!(view.series, book.series);
            assert_eq!(view.series_index, book.series_index);
            assert_eq!(view.series_product_id, book.series_asin);
            assert_eq!(view.publisher, book.publisher);
            assert_eq!(view.categories, book.categories);
            assert_eq!(view.length_minutes, book.length_minutes, "{name}");
            assert_eq!(view.is_abridged, book.is_abridged);
            assert_eq!(view.content_kind, "book");
            assert_eq!(view.purchased_at, book.purchased_at, "{name}");
            assert!(
                view.added_at.is_some(),
                "{name}: the holding's first record"
            );
            assert_eq!(view.published_at, book.published_at, "{name}");
            assert_eq!(view.status, AcquireStatus::Acquired);
            assert_eq!(view.storage_key.as_deref(), Some("Ann Author/Viewed.m4b"));
            assert_eq!(view.pdf_status, AcquireStatus::Queued);
            assert_eq!(view.pdf_storage_key, None);
            assert!(view.acquired_here(), "{name}");
            assert!(view.pdf_acquired_here(), "{name}");

            // Another account of the store that does not hold it, and an
            // unknown product, have no view.
            assert_eq!(
                store
                    .acquire_title("audible", "other", "us", "B0VIEW001")
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                store
                    .acquire_title("audible", "acct", "us", "B0NOPE")
                    .await
                    .unwrap(),
                None
            );
        }
    }

    #[tokio::test]
    async fn an_isbn_store_title_names_its_isbn() {
        let kit = crate::test_support::sqlite::memory(crate::test_support::Profile::ALL[0]).await;
        let store = kit.store();
        store
            .upsert_account("libro", "acct", "", None, true, "libro")
            .await
            .unwrap();
        let mut book = NewBook::minimal("9780000000019", "acct", "", "Printed");
        book.plugin_key = "libro".into();
        book.source = "libro".into();
        book.asin = None;
        book.isbn = Some("9780000000019".into());
        let flat = store.upsert_book(&book).await.unwrap();
        let view = store
            .acquire_title(&flat.plugin_key, "acct", "", "9780000000019")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.asin, flat.asin);
        assert_eq!(view.isbn, flat.isbn);
        assert_eq!(view.asin_or_isbn(), flat.asin_or_isbn());
        assert_eq!(view.status, AcquireStatus::NotAcquired);
        assert_eq!(view.storage_key, None);
    }

    #[tokio::test]
    async fn a_podcast_episode_and_its_show_name_their_kinds() {
        let kit = crate::test_support::sqlite::memory(crate::test_support::Profile::ALL[0]).await;
        let store = kit.store();
        store
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut show = NewBook::minimal("B0SHOW", "acct", "us", "Show");
        show.content_kind = "podcast".into();
        store.upsert_book(&show).await.unwrap();
        let mut episode = NewBook::minimal("B0EP0001", "acct", "us", "Episode 1");
        episode.content_kind = "episode".into();
        episode.series_asin = Some("B0SHOW".into());
        store.upsert_book(&episode).await.unwrap();
        let kind = |product: &'static str| {
            let store = store.clone();
            async move {
                store
                    .acquire_title("audible", "acct", "us", product)
                    .await
                    .unwrap()
                    .unwrap()
                    .content_kind
            }
        };
        assert_eq!(kind("B0SHOW").await, "podcast");
        assert_eq!(kind("B0EP0001").await, "episode");

        // An episode whose show is not in the library yet is still one.
        let mut orphan = NewBook::minimal("B0EP0002", "acct", "us", "Episode 2");
        orphan.content_kind = "episode".into();
        orphan.series_asin = Some("B0NOSHOW".into());
        store.upsert_book(&orphan).await.unwrap();
        assert_eq!(kind("B0EP0002").await, "episode");
    }

    #[tokio::test]
    async fn a_walk_of_pages_reads_every_held_title_under_every_profile() {
        for profile in crate::test_support::Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            for account in ["acct", "other"] {
                store
                    .upsert_account("audible", account, "us", None, true, "audible")
                    .await
                    .unwrap();
            }
            for product in ["B0PAGE001", "B0PAGE002", "B0PAGE003"] {
                store
                    .upsert_book(&NewBook::minimal(product, "acct", "us", product))
                    .await
                    .unwrap();
            }
            // The other account holds one of the same products.
            store
                .upsert_book(&NewBook::minimal("B0PAGE002", "other", "us", "B0PAGE002"))
                .await
                .unwrap();

            let walk = |account: Option<&'static str>| {
                let store = store.clone();
                async move {
                    let mut titles = Vec::new();
                    let mut after = None;
                    let mut pages = 0;
                    loop {
                        let page = store
                            .acquire_titles_page(account, after.as_deref(), 2)
                            .await
                            .unwrap();
                        pages += 1;
                        titles.extend(page.titles);
                        match page.next {
                            Some(next) => after = Some(next),
                            None => break,
                        }
                    }
                    (titles, pages)
                }
            };

            let (all, pages) = walk(None).await;
            assert_eq!(all.len(), 4, "{name}");
            assert!(pages >= 2, "{name}: paged by two");
            for title in &all {
                let single = store
                    .acquire_title(
                        &title.plugin_key,
                        &title.account_id,
                        &title.marketplace,
                        &title.product_id,
                    )
                    .await
                    .unwrap();
                assert_eq!(single.as_ref(), Some(title), "{name}");
                assert_eq!(title.source, "audible", "{name}");
                assert!(!title.holding_id.is_empty(), "{name}");
            }
            let mut held: Vec<_> = all
                .iter()
                .map(|t| (t.account_id.as_str(), t.product_id.as_str()))
                .collect();
            held.sort_unstable();
            assert_eq!(
                held,
                vec![
                    ("acct", "B0PAGE001"),
                    ("acct", "B0PAGE002"),
                    ("acct", "B0PAGE003"),
                    ("other", "B0PAGE002"),
                ],
                "{name}"
            );

            let (other, _) = walk(Some("other")).await;
            assert_eq!(other.len(), 1, "{name}");
            assert_eq!(other[0].account_id, "other");
            // Two accounts' titles of one product share its edition.
            let shared: Vec<_> = all.iter().filter(|t| t.product_id == "B0PAGE002").collect();
            assert_eq!(shared[0].edition_id, shared[1].edition_id, "{name}");
            assert_ne!(shared[0].holding_id, shared[1].holding_id, "{name}");
        }
    }
}
