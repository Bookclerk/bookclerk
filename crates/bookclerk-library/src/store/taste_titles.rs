//! The library's works as Discover reads them: one [`TasteTitle`] per work
//! the library holds, built from holdings, listings, editions and works
//! rather than from the flat `books` row.
//!
//! A work held through several accounts, stores or editions is one title.
//! Its bibliographic fields come from the work (credits, series, subjects,
//! description) and from one representative edition and listing; a
//! person's finished flag, ratings and tags come from `user_title_state`
//! and are applied afterwards ([`apply_user_states`]), so one household
//! read serves every subject.
//!
//! Every read is chunked by key and sized to the database plugin's limits
//! ([`super::library_model::find_chunked`]); the holdings themselves are
//! read in keyset pages.

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_resolve as resolve;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::library_model::{find_chunked, find_chunked_paged};
use super::{parse_dt, LibraryStore};
use crate::entities::{
    accounts, books, credits, edition_files, edition_identifiers, editions, expressions, holdings,
    series, series_entries, store_listings, user_title_state,
};
use crate::error::{LibraryError, Result};
use crate::models::{TasteHolder, TasteTitle, UserTitleState};

/// Most per-person state rows read per keyset page; narrowed and halved
/// like [`HOLDINGS_PAGE`].
const STATE_PAGE: u64 = 500;

/// Most holdings read per keyset page; the page is narrowed further to the
/// database plugin's `maxResultRows`, and halved when a reply is refused
/// as over a negotiated limit.
const HOLDINGS_PAGE: u64 = 500;

/// The columns of one holding a taste read needs.
#[derive(Debug, sea_orm::FromQueryResult)]
struct HoldingRow {
    /// `holdings.id`, the keyset cursor.
    id: String,
    /// The listing held.
    listing_id: String,
    /// The account's plugin; `None` for a branch (node) holding.
    plugin_key: Option<String>,
    /// The account id; `None` for a branch (node) holding.
    account_id: Option<String>,
    /// When the account bought the title, RFC 3339, when known.
    purchased_at: Option<String>,
}

/// One edition's place in the model, read once per edition.
#[derive(Debug, Clone)]
struct EditionPlace {
    /// The edition row.
    edition: editions::Model,
    /// Its expression's work.
    work_id: String,
    /// Its expression's language.
    language: Option<String>,
    /// Whether its expression is abridged.
    abridged: bool,
}

/// One held listing of a work, a candidate for the representative.
#[derive(Debug, Clone)]
struct HeldListing {
    /// The listing row.
    listing: store_listings::Model,
    /// The newest purchase date any holder of it recorded.
    purchased_at: Option<String>,
    /// Whether its edition has files at a destination.
    acquired: bool,
}

impl LibraryStore {
    /// Every work the library holds, one [`TasteTitle`] each, in work id
    /// order, with no person's state applied.
    ///
    /// A work is held when an account or a branch holds a listing of one
    /// of its editions. [`TasteTitle::holders`] lists the accounts that
    /// hold it (a branch-held work has none).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn taste_titles(&self) -> Result<Vec<TasteTitle>> {
        let held = self.held_listing_rows().await?;
        if held.is_empty() {
            return Ok(Vec::new());
        }
        let listing_ids: Vec<String> = held.keys().cloned().collect();
        let listings = find_chunked(self, &listing_ids, |part| {
            store_listings::Entity::find()
                .filter(store_listings::Column::Id.is_in(part.iter().cloned()))
        })
        .await?;
        let edition_ids: Vec<String> = listings
            .iter()
            .map(|l| l.edition_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let places = self.edition_places(&edition_ids).await?;
        let acquired = self.editions_with_files(&edition_ids).await?;
        let account_ids: Vec<String> = held
            .values()
            .flatten()
            .filter_map(|r| r.account_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let sources = self.account_sources(&account_ids).await?;

        // Group held listings by work.
        let mut by_work: BTreeMap<String, Vec<HeldListing>> = BTreeMap::new();
        let mut holders_of: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
        for listing in listings {
            let Some(place) = places.get(&listing.edition_id) else {
                continue;
            };
            let rows = held.get(&listing.id).map(Vec::as_slice).unwrap_or_default();
            let purchased_at = rows.iter().filter_map(|r| r.purchased_at.clone()).max();
            let holders = holders_of.entry(place.work_id.clone()).or_default();
            for row in rows {
                if let (Some(plugin_key), Some(account_id)) = (&row.plugin_key, &row.account_id) {
                    holders.insert((plugin_key.clone(), account_id.clone()));
                }
            }
            by_work
                .entry(place.work_id.clone())
                .or_default()
                .push(HeldListing {
                    acquired: acquired.contains(&listing.edition_id),
                    listing,
                    purchased_at,
                });
        }

        let work_ids: Vec<String> = by_work.keys().cloned().collect();
        let works = self.works_by_ids(&work_ids).await?;
        let primary_series = self.primary_series(&work_ids).await?;
        let reps: BTreeMap<String, HeldListing> = by_work
            .into_iter()
            .filter_map(|(work, mut candidates)| {
                candidates.sort_by(representative_order);
                Some((work, candidates.into_iter().next()?))
            })
            .collect();
        let rep_editions: Vec<String> = reps
            .values()
            .map(|r| r.listing.edition_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let identifiers = self.edition_ids_by_scheme(&rep_editions).await?;
        let publishers = self.edition_publishers(&rep_editions).await?;
        let series_store_ids = self.series_store_ids(reps.values()).await?;

        let mut out = Vec::with_capacity(reps.len());
        for (work_id, rep) in reps {
            let Some(work) = works.get(&work_id) else {
                continue;
            };
            let Some(place) = places.get(&rep.listing.edition_id) else {
                continue;
            };
            let edition_id = rep.listing.edition_id.clone();
            let ids = identifiers.get(&edition_id);
            let holders: Vec<TasteHolder> = holders_of
                .remove(&work_id)
                .unwrap_or_default()
                .into_iter()
                .map(|(plugin_key, account_id)| TasteHolder {
                    source: sources
                        .get(&(plugin_key.clone(), account_id.clone()))
                        .cloned()
                        .unwrap_or_else(|| plugin_key.clone()),
                    plugin_key,
                    account_id,
                })
                .collect();
            let source = holders
                .iter()
                .find(|h| h.plugin_key == rep.listing.plugin_key)
                .map_or_else(|| rep.listing.plugin_key.clone(), |h| h.source.clone());
            let series_store_id = series_store_ids
                .get(&(
                    rep.listing.plugin_key.clone(),
                    rep.listing.product_id.clone(),
                ))
                .cloned();
            out.push(TasteTitle {
                work_id: work_id.clone(),
                edition_id,
                plugin_key: rep.listing.plugin_key.clone(),
                source,
                product_id: rep.listing.product_id.clone(),
                marketplace: rep.listing.marketplace.clone(),
                asin: ids.and_then(|m| m.get("asin").cloned()),
                isbn: ids.and_then(|m| m.get("isbn").cloned()),
                title: work.title.clone(),
                subtitle: place
                    .edition
                    .subtitle
                    .clone()
                    .or_else(|| rep.listing.subtitle.clone()),
                authors: join_names(&work.credits),
                narrators: join_names(&work.narrators),
                series: primary_series
                    .get(&work_id)
                    .map(|(name, _)| name.clone())
                    .or_else(|| work.series.first().cloned()),
                series_index: primary_series
                    .get(&work_id)
                    .and_then(|(_, position)| position.clone()),
                series_store_id,
                publisher: publishers.get(&place.edition.id).cloned(),
                categories: rep
                    .listing
                    .categories
                    .clone()
                    .or_else(|| work.categories.clone()),
                subjects: work.subjects.clone(),
                description: work.description.clone(),
                language: place.language.clone(),
                cover_url: place
                    .edition
                    .cover_url
                    .clone()
                    .or_else(|| work.cover_url.clone()),
                length_minutes: place.edition.duration_minutes,
                published_at: place.edition.published_at.clone(),
                is_abridged: place.abridged,
                is_podcast: is_podcast_kind(&work.kind) || work.parent_work_id.is_some(),
                purchased_at: rep.purchased_at.as_deref().map(parse_dt),
                acquired: rep.acquired,
                holders,
                is_finished: false,
                rating_overall: None,
                rating_performance: None,
                rating_story: None,
                tags: None,
            });
        }
        Ok(out)
    }

    /// The stored state of every user for the given works, read in chunks.
    ///
    /// # Arguments
    ///
    /// * `work_ids` - Works whose state to read.
    ///
    /// # Returns
    ///
    /// The stored rows, ordered by work then user.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn user_title_states_for_works(
        &self,
        work_ids: &[String],
    ) -> Result<Vec<UserTitleState>> {
        let ids: Vec<String> = work_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // Several people may hold state on one work, so the rows of a chunk
        // of works are read in keyset pages over (work, user) rather than
        // one read per chunk, which a one-row result cap would refuse.
        let mut rows: Vec<UserTitleState> = Vec::new();
        let mut page_size = STATE_PAGE.min(u64::from(self.max_result_rows.max(1)));
        for part in ids.chunks(self.lookup_chunk().max(1)) {
            let mut after: Option<(String, i64)> = None;
            loop {
                let mut query = user_title_state::Entity::find()
                    .filter(user_title_state::Column::WorkId.is_in(part.iter().cloned()))
                    .order_by_asc(user_title_state::Column::WorkId)
                    .order_by_asc(user_title_state::Column::UserId)
                    .limit(page_size);
                if let Some((work, user)) = &after {
                    query = query.filter(
                        Condition::any()
                            .add(user_title_state::Column::WorkId.gt(work.as_str()))
                            .add(
                                Condition::all()
                                    .add(user_title_state::Column::WorkId.eq(work.as_str()))
                                    .add(user_title_state::Column::UserId.gt(*user)),
                            ),
                    );
                }
                let page = match query.all(&self.db).await {
                    Ok(page) => page,
                    Err(err)
                        if page_size > 1
                            && bookclerk_plugin_abi::LimitExceeded::from_message(
                                &err.to_string(),
                            )
                            .is_some() =>
                    {
                        page_size /= 2;
                        continue;
                    }
                    Err(err) => return Err(LibraryError::Orm(err)),
                };
                let short = u64::try_from(page.len()).unwrap_or(u64::MAX) < page_size;
                after = page.last().map(|m| (m.work_id.clone(), m.user_id));
                rows.extend(page.into_iter().map(super::user_title_state::map_state));
                if short || after.is_none() {
                    break;
                }
            }
        }
        rows.sort_by(|a, b| {
            a.work_id
                .cmp(&b.work_id)
                .then_with(|| a.user_id.cmp(&b.user_id))
        });
        Ok(rows)
    }

    /// Every held listing id with the holdings that hold it, read in keyset
    /// pages over `holdings.id`.
    async fn held_listing_rows(&self) -> Result<BTreeMap<String, Vec<HoldingRow>>> {
        let mut out: BTreeMap<String, Vec<HoldingRow>> = BTreeMap::new();
        let mut after: Option<String> = None;
        let mut page_size = HOLDINGS_PAGE.min(u64::from(self.max_result_rows.max(1)));
        loop {
            let mut query = holdings::Entity::find()
                .select_only()
                .column(holdings::Column::Id)
                .column(holdings::Column::ListingId)
                .column(holdings::Column::PluginKey)
                .column(holdings::Column::AccountId)
                .column(holdings::Column::PurchasedAt)
                .order_by_asc(holdings::Column::Id)
                .limit(page_size);
            if let Some(after) = &after {
                query = query.filter(holdings::Column::Id.gt(after.as_str()));
            }
            let page = match query.into_model::<HoldingRow>().all(&self.db).await {
                Ok(page) => page,
                Err(err)
                    if page_size > 1
                        && bookclerk_plugin_abi::LimitExceeded::from_message(&err.to_string())
                            .is_some() =>
                {
                    page_size /= 2;
                    continue;
                }
                Err(err) => return Err(LibraryError::Orm(err)),
            };
            let short = u64::try_from(page.len()).unwrap_or(u64::MAX) < page_size;
            after = page.last().map(|r| r.id.clone());
            for row in page {
                out.entry(row.listing_id.clone()).or_default().push(row);
            }
            if short || after.is_none() {
                return Ok(out);
            }
        }
    }

    /// Each edition with its expression's work, language and abridgement.
    async fn edition_places(
        &self,
        edition_ids: &[String],
    ) -> Result<BTreeMap<String, EditionPlace>> {
        let editions = find_chunked(self, edition_ids, |part| {
            editions::Entity::find().filter(editions::Column::Id.is_in(part.iter().cloned()))
        })
        .await?;
        let expression_ids: Vec<String> = editions
            .iter()
            .map(|e| e.expression_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let expressions: BTreeMap<String, expressions::Model> =
            find_chunked(self, &expression_ids, |part| {
                expressions::Entity::find()
                    .filter(expressions::Column::Id.is_in(part.iter().cloned()))
            })
            .await?
            .into_iter()
            .map(|x| (x.id.clone(), x))
            .collect();
        Ok(editions
            .into_iter()
            .filter_map(|edition| {
                let expression = expressions.get(&edition.expression_id)?;
                Some((
                    edition.id.clone(),
                    EditionPlace {
                        work_id: expression.work_id.clone(),
                        language: expression.language.clone(),
                        abridged: expression.abridgement == resolve::Abridgement::Abridged.as_str(),
                        edition,
                    },
                ))
            })
            .collect())
    }

    /// The editions among `edition_ids` that have at least one recorded
    /// file.
    async fn editions_with_files(&self, edition_ids: &[String]) -> Result<BTreeSet<String>> {
        Ok(find_chunked_paged(self, edition_ids, |part| {
            edition_files::Entity::find()
                .filter(edition_files::Column::OwnerKind.eq("edition"))
                .filter(edition_files::Column::OwnerId.is_in(part.iter().cloned()))
                .order_by_asc(edition_files::Column::Destination)
                .order_by_asc(edition_files::Column::StorageKey)
        })
        .await?
        .into_iter()
        .map(|f| f.owner_id)
        .collect())
    }

    /// The storefront id of each holding account, by plugin key and
    /// account id, read in chunks of account ids.
    async fn account_sources(
        &self,
        account_ids: &[String],
    ) -> Result<BTreeMap<(String, String), String>> {
        Ok(find_chunked_paged(self, account_ids, |part| {
            accounts::Entity::find()
                .filter(accounts::Column::AccountId.is_in(part.iter().cloned()))
                .order_by_asc(accounts::Column::Id)
        })
        .await?
        .into_iter()
        .map(|a| ((a.plugin_key, a.account_id), a.source))
        .collect())
    }

    /// Each work's primary series entry: its series name and the work's
    /// display position there, taken from one entry so the two always
    /// describe the same series. The primary entry is the first with a
    /// position (by sort position, then series id), or else the first by
    /// series id.
    async fn primary_series(
        &self,
        work_ids: &[String],
    ) -> Result<BTreeMap<String, (String, Option<String>)>> {
        let mut entries = find_chunked_paged(self, work_ids, |part| {
            series_entries::Entity::find()
                .filter(series_entries::Column::WorkId.is_in(part.iter().cloned()))
                .order_by_asc(series_entries::Column::SeriesId)
                .order_by_asc(series_entries::Column::WorkId)
        })
        .await?;
        entries.sort_by(|a, b| {
            a.work_id
                .cmp(&b.work_id)
                .then_with(|| a.position.is_none().cmp(&b.position.is_none()))
                .then_with(|| {
                    a.sort_position
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&b.sort_position.unwrap_or(f64::INFINITY))
                })
                .then_with(|| a.series_id.cmp(&b.series_id))
        });
        let mut primary: BTreeMap<String, series_entries::Model> = BTreeMap::new();
        for entry in entries {
            primary.entry(entry.work_id.clone()).or_insert(entry);
        }
        let series_ids: Vec<String> = primary
            .values()
            .map(|e| e.series_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let names: BTreeMap<String, String> = find_chunked(self, &series_ids, |part| {
            series::Entity::find().filter(series::Column::Id.is_in(part.iter().cloned()))
        })
        .await?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();
        Ok(primary
            .into_iter()
            .filter_map(|(work_id, entry)| {
                let name = names.get(&entry.series_id)?.clone();
                Some((work_id, (name, entry.position)))
            })
            .collect())
    }

    /// Each edition's ASIN and ISBN, by scheme (the first of each, in value
    /// order).
    async fn edition_ids_by_scheme(
        &self,
        edition_ids: &[String],
    ) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
        let mut rows = find_chunked_paged(self, edition_ids, |part| {
            edition_identifiers::Entity::find()
                .filter(edition_identifiers::Column::EditionId.is_in(part.iter().cloned()))
                .filter(edition_identifiers::Column::Scheme.is_in([
                    resolve::IdentifierScheme::Asin.as_str(),
                    resolve::IdentifierScheme::Isbn.as_str(),
                ]))
                .order_by_asc(edition_identifiers::Column::EditionId)
                .order_by_asc(edition_identifiers::Column::Scheme)
                .order_by_asc(edition_identifiers::Column::Value)
                .order_by_asc(edition_identifiers::Column::Marketplace)
        })
        .await?;
        rows.sort_by(|a, b| a.value.cmp(&b.value));
        let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for row in rows {
            out.entry(row.edition_id)
                .or_default()
                .entry(row.scheme)
                .or_insert(row.value);
        }
        Ok(out)
    }

    /// Each edition's first credited publisher.
    async fn edition_publishers(&self, edition_ids: &[String]) -> Result<BTreeMap<String, String>> {
        use crate::entities::contributors;
        let mut rows = find_chunked_paged(self, edition_ids, |part| {
            credits::Entity::find()
                .filter(credits::Column::TargetKind.eq(resolve::Level::Edition.as_str()))
                .filter(credits::Column::Role.eq(resolve::CreditRole::Publisher.as_str()))
                .filter(credits::Column::TargetId.is_in(part.iter().cloned()))
                .order_by_asc(credits::Column::TargetId)
                .order_by_asc(credits::Column::ContributorId)
        })
        .await?;
        rows.sort_by_key(|c| c.position);
        let contributor_ids: Vec<String> = rows
            .iter()
            .map(|c| c.contributor_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let names: BTreeMap<String, String> = find_chunked(self, &contributor_ids, |part| {
            contributors::Entity::find()
                .filter(contributors::Column::Id.is_in(part.iter().cloned()))
        })
        .await?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect();
        let mut out = BTreeMap::new();
        for row in rows {
            if let Some(name) = names.get(&row.contributor_id) {
                out.entry(row.target_id).or_insert_with(|| name.clone());
            }
        }
        Ok(out)
    }

    /// The store's own series id of each representative listing, by
    /// plugin key and product id.
    ///
    /// The library model has no home for a store's series id yet, so it is
    /// read from the scanned title rows until `books` retires (book model
    /// step 6), which moves it onto the model.
    async fn series_store_ids<'a>(
        &self,
        reps: impl Iterator<Item = &'a HeldListing>,
    ) -> Result<BTreeMap<(String, String), String>> {
        let wanted: BTreeSet<(String, String)> = reps
            .map(|r| (r.listing.plugin_key.clone(), r.listing.product_id.clone()))
            .collect();
        let products: Vec<String> = wanted
            .iter()
            .map(|(_, product)| product.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let rows = find_chunked_paged(self, &products, |part| {
            books::Entity::find()
                .filter(books::Column::ProductId.is_in(part.iter().cloned()))
                .filter(books::Column::SeriesAsin.is_not_null())
                .order_by_asc(books::Column::Id)
        })
        .await?;
        let mut out = BTreeMap::new();
        for row in rows {
            let key = (row.plugin_key, row.product_id);
            if !wanted.contains(&key) {
                continue;
            }
            if let Some(id) = row.series_asin.filter(|s| !s.trim().is_empty()) {
                out.entry(key).or_insert(id);
            }
        }
        Ok(out)
    }
}

/// Applies the state of `users` to `titles`, merged per work.
///
/// A work is finished when any of the users finished it; each rating is
/// the mean of the users who gave one; tags are the users' tags joined,
/// each tag once. A title without state from any of `users` keeps the
/// unfinished, unrated, untagged default. `users` empty means every user
/// whose state is in `states` (the household).
///
/// # Arguments
///
/// * `titles` - Titles to fill, as [`LibraryStore::taste_titles`] returns
///   them.
/// * `states` - Stored state rows for the titles' works.
/// * `users` - Whose state counts; empty for everyone's.
pub fn apply_user_states(titles: &mut [TasteTitle], states: &[UserTitleState], users: &[i64]) {
    let counts = |s: &&UserTitleState| users.is_empty() || users.contains(&s.user_id);
    let mut by_work: BTreeMap<&str, Vec<&UserTitleState>> = BTreeMap::new();
    for state in states.iter().filter(counts) {
        by_work
            .entry(state.work_id.as_str())
            .or_default()
            .push(state);
    }
    for title in titles {
        let Some(rows) = by_work.get(title.work_id.as_str()) else {
            continue;
        };
        title.is_finished = rows.iter().any(|s| s.is_finished);
        title.rating_overall = mean(rows.iter().filter_map(|s| s.rating_overall));
        title.rating_performance = mean(rows.iter().filter_map(|s| s.rating_performance));
        title.rating_story = mean(rows.iter().filter_map(|s| s.rating_story));
        let mut tags: Vec<String> = Vec::new();
        for row in rows {
            for tag in row.tags.iter().flat_map(|t| t.split_whitespace()) {
                if !tags.iter().any(|t| t == tag) {
                    tags.push(tag.to_string());
                }
            }
        }
        title.tags = (!tags.is_empty()).then(|| tags.join(" "));
    }
}

/// The mean of `values` as `f32`, or `None` when there are none.
fn mean(values: impl Iterator<Item = f64>) -> Option<f32> {
    let (sum, n) = values.fold((0.0_f64, 0_u32), |(s, n), v| (s + v, n + 1));
    #[allow(clippy::cast_possible_truncation)] // A 0..=5 rating fits an f32.
    (n > 0).then(|| (sum / f64::from(n)) as f32)
}

/// Names joined with `", "`, or `None` when there are none.
fn join_names(names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| names.join(", "))
}

/// True for a stored podcast work kind (a show or an episode).
fn is_podcast_kind(kind: &str) -> bool {
    kind.starts_with("podcast")
}

/// The order that puts a work's representative listing first: one whose
/// edition has files, then the most recently bought, then the lowest
/// listing id (so the choice is stable).
fn representative_order(a: &HeldListing, b: &HeldListing) -> std::cmp::Ordering {
    b.acquired
        .cmp(&a.acquired)
        .then_with(|| b.purchased_at.cmp(&a.purchased_at))
        .then_with(|| a.listing.id.cmp(&b.listing.id))
}

impl LibraryStore {
    /// The works among `work_ids` the library holds: an account or a branch
    /// holds a listing of one of their editions.
    ///
    /// # Arguments
    ///
    /// * `work_ids` - Works to check.
    ///
    /// # Returns
    ///
    /// The held ones.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn held_works(&self, work_ids: &[String]) -> Result<BTreeSet<String>> {
        let ids: Vec<String> = work_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let work_of_expression: BTreeMap<String, String> = find_chunked_paged(self, &ids, |part| {
            expressions::Entity::find()
                .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
                .order_by_asc(expressions::Column::Id)
        })
        .await?
        .into_iter()
        .map(|x| (x.id, x.work_id))
        .collect();
        let expression_ids: Vec<String> = work_of_expression.keys().cloned().collect();
        let work_of_edition: BTreeMap<String, String> =
            find_chunked_paged(self, &expression_ids, |part| {
                editions::Entity::find()
                    .filter(editions::Column::ExpressionId.is_in(part.iter().cloned()))
                    .order_by_asc(editions::Column::Id)
            })
            .await?
            .into_iter()
            .filter_map(|e| Some((e.id, work_of_expression.get(&e.expression_id)?.clone())))
            .collect();
        let edition_ids: Vec<String> = work_of_edition.keys().cloned().collect();
        let work_of_listing: BTreeMap<String, String> =
            find_chunked_paged(self, &edition_ids, |part| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::EditionId.is_in(part.iter().cloned()))
                    .order_by_asc(store_listings::Column::Id)
            })
            .await?
            .into_iter()
            .filter_map(|l| Some((l.id, work_of_edition.get(&l.edition_id)?.clone())))
            .collect();
        let listing_ids: Vec<String> = work_of_listing.keys().cloned().collect();
        Ok(find_chunked_paged(self, &listing_ids, |part| {
            super::holdings::find_holdings()
                .filter(holdings::Column::ListingId.is_in(part.iter().cloned()))
                .order_by_asc(holdings::Column::Id)
        })
        .await?
        .into_iter()
        .filter_map(|h| work_of_listing.get(&h.listing_id).cloned())
        .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::UserRole;
    use crate::store::{NewBook, UserBookFields};
    use crate::test_support::{sqlite, Profile};

    /// Scans `product` on `account` (plugin `audible`) with an ASIN, an
    /// author, a narrator and a series.
    async fn scan(store: &LibraryStore, product: &str, account: &str, title: &str) -> String {
        let mut book = NewBook::minimal(product, account, "us", title);
        book.asin = Some(product.into());
        book.authors = Some("Ann Leckie".into());
        book.narrators = Some("Adjoa Andoh".into());
        book.series = Some("Imperial Radch".into());
        book.series_index = Some("1".into());
        book.series_asin = Some("B0SERIES".into());
        let row = store.upsert_book(&book).await.unwrap();
        store.work_id_for_book(&row.uuid).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_work_held_on_two_accounts_is_one_title_with_both_holders() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            for account in ["a1", "a2"] {
                store
                    .upsert_account("audible", account, "us", None, true, "audible")
                    .await
                    .unwrap();
            }
            assert!(store.taste_titles().await.unwrap().is_empty(), "{name}");
            let work = scan(store, "B0JUSTICE", "a1", "Ancillary Justice").await;
            let again = scan(store, "B0JUSTICE", "a2", "Ancillary Justice").await;
            assert_eq!(work, again, "{name}");
            let other = scan(store, "B0SWORD", "a2", "Ancillary Sword").await;

            let titles = store.taste_titles().await.unwrap();
            assert_eq!(titles.len(), 2, "{name}");
            let justice = titles.iter().find(|t| t.work_id == work).unwrap();
            assert_eq!(justice.title, "Ancillary Justice", "{name}");
            assert_eq!(justice.asin.as_deref(), Some("B0JUSTICE"), "{name}");
            assert_eq!(justice.product_id, "B0JUSTICE", "{name}");
            assert_eq!(justice.source, "audible", "{name}");
            assert_eq!(justice.authors.as_deref(), Some("Ann Leckie"), "{name}");
            assert_eq!(justice.narrators.as_deref(), Some("Adjoa Andoh"), "{name}");
            assert_eq!(justice.series.as_deref(), Some("Imperial Radch"), "{name}");
            assert_eq!(justice.series_index.as_deref(), Some("1"), "{name}");
            assert_eq!(
                justice.series_store_id.as_deref(),
                Some("B0SERIES"),
                "{name}"
            );
            assert!(!justice.acquired, "{name}");
            assert!(!justice.is_podcast, "{name}");
            assert!(!justice.is_finished, "{name}");
            let holders: Vec<&str> = justice
                .holders
                .iter()
                .map(|h| h.account_id.as_str())
                .collect();
            assert_eq!(holders, ["a1", "a2"], "{name}");

            let held = store
                .held_works(&[work.clone(), other.clone(), "no-such-work".into()])
                .await
                .unwrap();
            assert_eq!(held, BTreeSet::from([work.clone(), other]), "{name}");
            assert!(store.held_works(&[]).await.unwrap().is_empty(), "{name}");
        }
    }

    #[tokio::test]
    async fn state_is_merged_per_work_for_the_people_asked_about() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            store
                .upsert_account("audible", "a1", "us", None, true, "audible")
                .await
                .unwrap();
            let work = scan(store, "B0JUSTICE", "a1", "Ancillary Justice").await;
            let ana = store
                .create_user(UserRole::Owner, Some("Ana"), None)
                .await
                .unwrap();
            let ben = store
                .create_user(UserRole::Member, Some("Ben"), None)
                .await
                .unwrap();
            for (user, finished, rating, tags) in [
                (ana.id, true, 5.0, "space ai"),
                (ben.id, false, 3.0, "ai favourite"),
            ] {
                store
                    .set_user_title_state(
                        user,
                        &work,
                        &UserBookFields {
                            is_finished: Some(finished),
                            rating_overall: Some(rating),
                            tags: Some(tags.into()),
                            ..UserBookFields::default()
                        },
                    )
                    .await
                    .unwrap();
            }
            let base = store.taste_titles().await.unwrap();
            let states = store
                .user_title_states_for_works(&[work.clone(), work.clone()])
                .await
                .unwrap();
            assert_eq!(states.len(), 2, "{name}");

            let mut household = base.clone();
            apply_user_states(&mut household, &states, &[]);
            assert!(household[0].is_finished, "{name}");
            assert_eq!(household[0].rating_overall, Some(4.0), "{name}");
            assert_eq!(household[0].rating_story, None, "{name}");
            assert_eq!(
                household[0].tags.as_deref(),
                Some("space ai favourite"),
                "{name}"
            );

            let mut bens = base.clone();
            apply_user_states(&mut bens, &states, &[ben.id]);
            assert!(!bens[0].is_finished, "{name}");
            assert_eq!(bens[0].rating_overall, Some(3.0), "{name}");

            let mut nobody = base;
            apply_user_states(&mut nobody, &states, &[ben.id + ana.id + 100]);
            assert_eq!(nobody[0].rating_overall, None, "{name}");
            assert_eq!(nobody[0].tags, None, "{name}");
        }
    }

    #[tokio::test]
    async fn series_name_and_position_come_from_the_same_entry() {
        use sea_orm::{ActiveValue::Set, EntityTrait};
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            store
                .upsert_account("audible", "a1", "us", None, true, "audible")
                .await
                .unwrap();
            let work = scan(store, "B0JUSTICE", "a1", "Ancillary Justice").await;
            // A second series that sorts first by id and gives no position:
            // the title keeps the series that has one.
            series::Entity::insert(series::ActiveModel {
                id: Set("0-omnibus".into()),
                name: Set("Radch Omnibus".into()),
                name_key: Set("radch omnibus".into()),
                created_at: Set("2026-01-01T00:00:00Z".into()),
            })
            .exec(&store.db)
            .await
            .unwrap();
            series_entries::Entity::insert(series_entries::ActiveModel {
                series_id: Set("0-omnibus".into()),
                work_id: Set(work.clone()),
                position: Set(None),
                sort_position: Set(None),
                created_at: Set("2026-01-01T00:00:00Z".into()),
            })
            .exec(&store.db)
            .await
            .unwrap();

            let titles = store.taste_titles().await.unwrap();
            assert_eq!(titles.len(), 1, "{name}");
            assert_eq!(
                titles[0].series.as_deref(),
                Some("Imperial Radch"),
                "{name}"
            );
            assert_eq!(titles[0].series_index.as_deref(), Some("1"), "{name}");
        }
    }

    #[tokio::test]
    async fn an_edition_with_several_identifiers_reads_them_all_under_every_cap() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            store
                .upsert_account("audible", "a1", "us", None, true, "audible")
                .await
                .unwrap();
            // An ASIN and an ISBN on one edition are two rows for one key,
            // more than a one-row result holds.
            let mut book = NewBook::minimal("B0JUSTICE", "a1", "us", "Ancillary Justice");
            book.asin = Some("B0JUSTICE".into());
            book.isbn = Some("9780316246620".into());
            store.upsert_book(&book).await.unwrap();
            let titles = store.taste_titles().await.unwrap();
            assert_eq!(titles.len(), 1, "{name}");
            assert_eq!(titles[0].asin.as_deref(), Some("B0JUSTICE"), "{name}");
            assert_eq!(titles[0].isbn.as_deref(), Some("9780316246620"), "{name}");
        }
    }
}
