//! The reads a listing snapshot makes, answered from the database or from a
//! page-level pool of rows read ahead.
//!
//! [`LibraryStore::listing_snapshot_and_row`] asks a [`SnapshotReader`] for
//! each set of rows the resolver's [`resolve::Snapshot`] contract names. A
//! reader without a pool runs one query per ask. A reader with a
//! [`ListingPool`] answers every ask the pool covers from memory and queries
//! only the rest, so the snapshot is the same either way: the pool records
//! which keys it read (including keys that matched no row) and never guesses
//! about a key it did not read.
//!
//! [`ListingPool::load`] reads, for a whole scan page, the union of what each
//! listing's snapshot will ask for, in the same stages the snapshot reads
//! them, so a page of titles that are already in the library costs a few
//! dozen queries instead of a dozen per title.

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_resolve as resolve;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};

use super::library_model::{find_chunked, find_chunked_by};
use super::LibraryStore;
use crate::column_read::SelectColumnExt;
use crate::entities::{
    credits, edition_identifiers, editions, expressions, holdings, resolution_overrides,
    series_entries, store_listings, work_identifiers, works,
};
use crate::error::{LibraryError, Result};

/// A listing key as stored: plugin key, stored marketplace, product id.
type ListingKey = (String, String, String);

/// An edition identifier key: scheme, value, marketplace.
type EditionIdentifierKey = (String, String, String);

/// A work identifier key: scheme, value.
type WorkIdentifierKey = (String, String);

/// The stored form of a listing key.
fn listing_key(key: &resolve::ListingKey) -> ListingKey {
    (
        key.plugin_key.clone(),
        key.marketplace.clone(),
        key.product_id.clone(),
    )
}

/// The stored form of an edition identifier key.
pub(super) fn edition_identifier_key(key: &resolve::IdentifierKey) -> EditionIdentifierKey {
    (
        key.scheme.as_str().to_string(),
        key.value.clone(),
        key.marketplace.clone(),
    )
}

/// The stored form of a work identifier key.
pub(super) fn work_identifier_key(key: &resolve::IdentifierKey) -> WorkIdentifierKey {
    (key.scheme.as_str().to_string(), key.value.clone())
}

/// Rows read ahead for a page of listings, with the keys each set covers.
///
/// Every map or set records the keys that were read, whether or not a row
/// matched, so an absent row is known to be absent rather than unread.
#[derive(Debug, Default)]
pub(crate) struct ListingPool {
    /// Listings by key; `None` when the key matched no row.
    listings: BTreeMap<ListingKey, Option<store_listings::Model>>,
    /// Listing ids whose `pin_edition` overrides were read.
    pins_read: BTreeSet<String>,
    /// `pin_edition` overrides of the listings in `pins_read`.
    pins: Vec<resolution_overrides::Model>,
    /// Edition identifier keys whose matching rows were read.
    edition_identifier_keys_read: BTreeSet<EditionIdentifierKey>,
    /// Edition ids whose identifier rows were read.
    edition_identifiers_of_read: BTreeSet<String>,
    /// Every edition identifier row read, by primary key.
    edition_identifiers: BTreeMap<(String, EditionIdentifierKey), edition_identifiers::Model>,
    /// Work identifier rows by key; `None` when the key matched no row.
    work_identifiers: BTreeMap<WorkIdentifierKey, Option<work_identifiers::Model>>,
    /// Editions by id; `None` when the id matched no row.
    editions: BTreeMap<String, Option<editions::Model>>,
    /// Expressions read by id; `None` when the id matched no row.
    expressions: BTreeMap<String, Option<expressions::Model>>,
    /// Work ids whose expressions were read.
    expressions_of_works_read: BTreeSet<String>,
    /// Expressions read as some work's, by id.
    work_expressions: BTreeMap<String, expressions::Model>,
    /// Work ids by `(match key, kind)`.
    match_keys: BTreeMap<(String, String), Vec<String>>,
    /// Works by id; `None` when the id matched no row.
    works: BTreeMap<String, Option<works::Model>>,
    /// Work ids whose isolation overrides were read.
    isolations_read: BTreeSet<String>,
    /// Isolation overrides of the works in `isolations_read`.
    isolations: Vec<resolution_overrides::Model>,
    /// Holdings by `(listing id, stored holder)`; `None` when there is none.
    holdings: BTreeMap<(String, String), Option<holdings::Model>>,
    /// `(level, target id)` pairs whose first-billed credit was looked up,
    /// with whether one is stored.
    credited: BTreeMap<(String, String), bool>,
    /// Work ids whose series entries were looked up, with whether any is
    /// stored.
    series: BTreeMap<String, bool>,
}

impl ListingPool {
    /// The stored work an edition reaches, when the pool read the edition,
    /// its expression and the work.
    pub(crate) fn work_of_edition(&self, edition_id: &str) -> Option<&works::Model> {
        let edition = self.editions.get(edition_id)?.as_ref()?;
        let expression = self
            .expressions
            .get(&edition.expression_id)
            .and_then(Option::as_ref)
            .or_else(|| self.work_expressions.get(&edition.expression_id))?;
        self.works.get(&expression.work_id)?.as_ref()
    }

    /// Reads, for every listing in `listings` held by `holder`, the rows its
    /// snapshot asks for, in the order the snapshot reads them.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub(crate) async fn load(
        store: &LibraryStore,
        listings: &[&resolve::Listing],
        holder: &resolve::Holder,
    ) -> Result<Self> {
        let db = SnapshotReader { store, pool: None };
        let mut pool = Self::default();

        // The listings and their parent shows.
        let mut keys: BTreeSet<ListingKey> = BTreeSet::new();
        for listing in listings {
            keys.insert(listing_key(&listing.key));
            if let Some(parent) = listing.parent_key() {
                keys.insert(listing_key(&parent));
            }
        }
        let keys: Vec<ListingKey> = keys.into_iter().collect();
        for key in &keys {
            pool.listings.insert(key.clone(), None);
        }
        for row in db.listings_by_keys(&keys).await? {
            let key = (
                row.plugin_key.clone(),
                row.marketplace.clone(),
                row.product_id.clone(),
            );
            pool.listings.insert(key, Some(row));
        }
        let own_ids: Vec<String> = listings
            .iter()
            .filter_map(|l| pool.listings.get(&listing_key(&l.key))?.as_ref())
            .map(|row| row.id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        // Pins on the stored listings.
        pool.pins = db.pins(&own_ids).await?;
        pool.pins_read.extend(own_ids.iter().cloned());

        // Edition identifier rows sharing one of the listings' keys.
        let ed_keys: BTreeSet<EditionIdentifierKey> = listings
            .iter()
            .flat_map(|l| l.edition_identifiers().map(edition_identifier_key))
            .collect();
        let ed_keys: Vec<EditionIdentifierKey> = ed_keys.into_iter().collect();
        for row in db.edition_identifiers_matching(&ed_keys).await? {
            pool.insert_edition_identifier(row);
        }
        pool.edition_identifier_keys_read.extend(ed_keys);

        // The editions those rows, the listings and the pins reach.
        let mut edition_ids: BTreeSet<String> = BTreeSet::new();
        edition_ids.extend(
            pool.listings
                .values()
                .flatten()
                .map(|row| row.edition_id.clone()),
        );
        edition_ids.extend(pool.pins.iter().filter_map(|m| m.target_id.clone()));
        edition_ids.extend(
            pool.edition_identifiers
                .values()
                .map(|row| row.edition_id.clone()),
        );
        let edition_ids: Vec<String> = edition_ids.into_iter().collect();
        for id in &edition_ids {
            pool.editions.insert(id.clone(), None);
        }
        for row in db.editions(&edition_ids).await? {
            pool.editions.insert(row.id.clone(), Some(row));
        }
        for row in db.edition_identifiers_of(&edition_ids).await? {
            pool.insert_edition_identifier(row);
        }
        pool.edition_identifiers_of_read
            .extend(edition_ids.iter().cloned());

        // Work identifier rows sharing one of the listings' keys.
        let work_keys: BTreeSet<WorkIdentifierKey> = listings
            .iter()
            .flat_map(|l| l.work_identifiers().map(work_identifier_key))
            .collect();
        let work_keys: Vec<WorkIdentifierKey> = work_keys.into_iter().collect();
        for key in &work_keys {
            pool.work_identifiers.insert(key.clone(), None);
        }
        for row in db.work_identifiers_matching(&work_keys).await? {
            pool.work_identifiers
                .insert((row.scheme.clone(), row.value.clone()), Some(row));
        }

        // The editions' expressions.
        let expression_ids: Vec<String> = pool
            .editions
            .values()
            .flatten()
            .map(|e| e.expression_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for id in &expression_ids {
            pool.expressions.insert(id.clone(), None);
        }
        for row in db.expressions(&expression_ids).await? {
            pool.expressions.insert(row.id.clone(), Some(row));
        }

        // Works: the expressions', the identifier rows' and the match keys'.
        let mut work_ids: BTreeSet<String> = pool
            .expressions
            .values()
            .flatten()
            .map(|x| x.work_id.clone())
            .collect();
        work_ids.extend(
            pool.work_identifiers
                .values()
                .flatten()
                .map(|row| row.work_id.clone()),
        );
        let match_keys: BTreeSet<(String, String)> = listings
            .iter()
            .filter_map(|l| {
                let key = l.work_match_key();
                (!key.is_empty()).then(|| (key, l.work_kind.as_str().to_string()))
            })
            .collect();
        let keys: Vec<String> = match_keys
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut matched: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for row in db.works_with_match_keys(&keys).await? {
            matched
                .entry((row.match_key.clone(), row.kind.clone()))
                .or_default()
                .push(row.id.clone());
            work_ids.insert(row.id.clone());
            pool.works.insert(row.id.clone(), Some(row));
        }
        for pair in match_keys {
            let ids = matched.remove(&pair).unwrap_or_default();
            pool.match_keys.insert(pair, ids);
        }
        let work_ids: Vec<String> = work_ids.into_iter().collect();
        for id in &work_ids {
            pool.works.insert(id.clone(), None);
        }
        for row in db.works(&work_ids).await? {
            pool.works.insert(row.id.clone(), Some(row));
        }
        for row in db.expressions_of_works(&work_ids).await? {
            pool.work_expressions.insert(row.id.clone(), row);
        }
        pool.expressions_of_works_read
            .extend(work_ids.iter().cloned());
        pool.isolations = db.isolations(&work_ids).await?;
        pool.isolations_read.extend(work_ids.iter().cloned());

        // The holder's holdings of the stored listings.
        let stored_holder = holder.stored();
        for id in &own_ids {
            pool.holdings
                .insert((id.clone(), stored_holder.clone()), None);
        }
        for row in db.holdings(&own_ids, &stored_holder).await? {
            pool.holdings
                .insert((row.listing_id.clone(), row.holder.clone()), Some(row));
        }

        // Which works, expressions and editions are credited, and which
        // works have series.
        let all_expression_ids: Vec<String> = pool
            .expressions
            .values()
            .flatten()
            .map(|x| x.id.clone())
            .chain(pool.work_expressions.keys().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for (level, ids) in [
            (resolve::Level::Work, &work_ids),
            (resolve::Level::Expression, &all_expression_ids),
            (resolve::Level::Edition, &edition_ids),
        ] {
            let found: BTreeSet<String> = db.first_credits(level, ids).await?.into_iter().collect();
            for id in ids.iter() {
                pool.credited
                    .insert((level.as_str().to_string(), id.clone()), found.contains(id));
            }
        }
        let with_series: BTreeSet<String> =
            db.works_with_series(&work_ids).await?.into_iter().collect();
        for id in &work_ids {
            pool.series.insert(id.clone(), with_series.contains(id));
        }
        Ok(pool)
    }

    /// Adds an edition identifier row under its primary key.
    fn insert_edition_identifier(&mut self, row: edition_identifiers::Model) {
        let key = (
            row.edition_id.clone(),
            (
                row.scheme.clone(),
                row.value.clone(),
                row.marketplace.clone(),
            ),
        );
        self.edition_identifiers.insert(key, row);
    }
}

/// Splits `keys` into those `covered` answers and those it does not.
fn split_covered<K: Clone>(keys: &[K], covered: impl Fn(&K) -> bool) -> (Vec<&K>, Vec<K>) {
    let mut hit = Vec::new();
    let mut miss = Vec::new();
    for key in keys {
        if covered(key) {
            hit.push(key);
        } else {
            miss.push(key.clone());
        }
    }
    (hit, miss)
}

/// Answers the reads of one listing snapshot: from `pool` where it covers
/// them, from the database otherwise.
#[derive(Clone, Copy)]
pub(crate) struct SnapshotReader<'a> {
    /// The store whose connection the uncovered reads use.
    pub(crate) store: &'a LibraryStore,
    /// Rows read ahead for the page, if any.
    pub(crate) pool: Option<&'a ListingPool>,
}

impl SnapshotReader<'_> {
    /// The connection uncovered reads use.
    fn db(&self) -> &sea_orm::DatabaseConnection {
        &self.store.db
    }

    /// The listing stored under `key`, if any.
    pub(crate) async fn listing(
        &self,
        key: &resolve::ListingKey,
    ) -> Result<Option<store_listings::Model>> {
        let key = listing_key(key);
        if let Some(found) = self.pool.and_then(|p| p.listings.get(&key)) {
            return Ok(found.clone());
        }
        Ok(self
            .listings_by_keys(std::slice::from_ref(&key))
            .await?
            .into_iter()
            .next())
    }

    /// Listings stored under any of `keys` (database only).
    async fn listings_by_keys(&self, keys: &[ListingKey]) -> Result<Vec<store_listings::Model>> {
        // Each key binds a plugin key, a marketplace and a product id.
        find_chunked_by(self.store, keys, 3, |part| {
            let mut any = Condition::any();
            for (plugin_key, marketplace, product_id) in part {
                any = any.add(
                    Condition::all()
                        .add(store_listings::Column::PluginKey.eq(plugin_key.as_str()))
                        .add(store_listings::Column::Marketplace.eq(marketplace.as_str()))
                        .add(store_listings::Column::ProductId.eq(product_id.as_str())),
                );
            }
            store_listings::Entity::find().filter(any)
        })
        .await
    }

    /// `pin_edition` overrides on the listings in `listing_ids`.
    pub(crate) async fn pins(
        &self,
        listing_ids: &[String],
    ) -> Result<Vec<resolution_overrides::Model>> {
        let (hit, miss) = split_covered(listing_ids, |id| {
            self.pool.is_some_and(|p| p.pins_read.contains(id))
        });
        let mut out: Vec<resolution_overrides::Model> = Vec::new();
        if let Some(pool) = self.pool {
            out.extend(
                pool.pins
                    .iter()
                    .filter(|m| hit.contains(&&m.subject_id))
                    .cloned(),
            );
        }
        out.extend(
            find_chunked(self.store, &miss, |part| {
                resolution_overrides::Entity::find()
                    .filter(resolution_overrides::Column::SubjectKind.eq("listing"))
                    .filter(resolution_overrides::Column::Action.eq("pin_edition"))
                    .filter(resolution_overrides::Column::SubjectId.is_in(part.iter().cloned()))
            })
            .await?,
        );
        Ok(out)
    }

    /// Edition identifier rows matching any of `keys`.
    pub(crate) async fn edition_identifiers_matching(
        &self,
        keys: &[EditionIdentifierKey],
    ) -> Result<Vec<edition_identifiers::Model>> {
        let (hit, miss) = split_covered(keys, |key| {
            self.pool
                .is_some_and(|p| p.edition_identifier_keys_read.contains(key))
        });
        let mut out = Vec::new();
        if let Some(pool) = self.pool {
            let hit: BTreeSet<&EditionIdentifierKey> = hit.into_iter().collect();
            out.extend(
                pool.edition_identifiers
                    .iter()
                    .filter(|((_, key), _)| hit.contains(key))
                    .map(|(_, row)| row.clone()),
            );
        }
        // Each key binds a scheme, a value and a marketplace.
        out.extend(
            find_chunked_by(self.store, &miss, 3, |part| {
                let mut any = Condition::any();
                for (scheme, value, marketplace) in part {
                    any = any.add(
                        Condition::all()
                            .add(edition_identifiers::Column::Scheme.eq(scheme.as_str()))
                            .add(edition_identifiers::Column::Value.eq(value.as_str()))
                            .add(edition_identifiers::Column::Marketplace.eq(marketplace.as_str())),
                    );
                }
                edition_identifiers::Entity::find().filter(any)
            })
            .await?,
        );
        Ok(out)
    }

    /// Editions with any of `ids`.
    pub(crate) async fn editions(&self, ids: &[String]) -> Result<Vec<editions::Model>> {
        let (hit, miss) = split_covered(ids, |id| {
            self.pool.is_some_and(|p| p.editions.contains_key(id))
        });
        let mut out: Vec<editions::Model> = hit
            .into_iter()
            .filter_map(|id| self.pool?.editions.get(id)?.clone())
            .collect();
        out.extend(
            find_chunked(self.store, &miss, |part| {
                editions::Entity::find().filter(editions::Column::Id.is_in(part.iter().cloned()))
            })
            .await?,
        );
        Ok(out)
    }

    /// Identifier rows of the editions in `edition_ids`.
    pub(crate) async fn edition_identifiers_of(
        &self,
        edition_ids: &[String],
    ) -> Result<Vec<edition_identifiers::Model>> {
        let (hit, miss) = split_covered(edition_ids, |id| {
            self.pool
                .is_some_and(|p| p.edition_identifiers_of_read.contains(id))
        });
        let mut out = Vec::new();
        if let Some(pool) = self.pool {
            let hit: BTreeSet<&String> = hit.into_iter().collect();
            out.extend(
                pool.edition_identifiers
                    .iter()
                    .filter(|((edition_id, _), _)| hit.contains(edition_id))
                    .map(|(_, row)| row.clone()),
            );
        }
        out.extend(
            find_chunked(self.store, &miss, |part| {
                edition_identifiers::Entity::find()
                    .filter(edition_identifiers::Column::EditionId.is_in(part.iter().cloned()))
            })
            .await?,
        );
        Ok(out)
    }

    /// Work identifier rows matching any of `keys`.
    pub(crate) async fn work_identifiers_matching(
        &self,
        keys: &[WorkIdentifierKey],
    ) -> Result<Vec<work_identifiers::Model>> {
        let (hit, miss) = split_covered(keys, |key| {
            self.pool
                .is_some_and(|p| p.work_identifiers.contains_key(key))
        });
        let mut out: Vec<work_identifiers::Model> = hit
            .into_iter()
            .filter_map(|key| self.pool?.work_identifiers.get(key)?.clone())
            .collect();
        // Each key binds a scheme and a value.
        out.extend(
            find_chunked_by(self.store, &miss, 2, |part| {
                let mut any = Condition::any();
                for (scheme, value) in part {
                    any = any.add(
                        Condition::all()
                            .add(work_identifiers::Column::Scheme.eq(scheme.as_str()))
                            .add(work_identifiers::Column::Value.eq(value.as_str())),
                    );
                }
                work_identifiers::Entity::find().filter(any)
            })
            .await?,
        );
        Ok(out)
    }

    /// Expressions with any of `ids`.
    pub(crate) async fn expressions(&self, ids: &[String]) -> Result<Vec<expressions::Model>> {
        let (hit, miss) = split_covered(ids, |id| {
            self.pool.is_some_and(|p| p.expressions.contains_key(id))
        });
        let mut out: Vec<expressions::Model> = hit
            .into_iter()
            .filter_map(|id| self.pool?.expressions.get(id)?.clone())
            .collect();
        out.extend(
            find_chunked(self.store, &miss, |part| {
                expressions::Entity::find()
                    .filter(expressions::Column::Id.is_in(part.iter().cloned()))
            })
            .await?,
        );
        Ok(out)
    }

    /// Ids of the works with `match_key` and `kind`.
    pub(crate) async fn works_matching(&self, match_key: &str, kind: &str) -> Result<Vec<String>> {
        if let Some(ids) = self
            .pool
            .and_then(|p| p.match_keys.get(&(match_key.to_string(), kind.to_string())))
        {
            return Ok(ids.clone());
        }
        works::Entity::find()
            .filter(works::Column::MatchKey.eq(match_key))
            .filter(works::Column::Kind.eq(kind))
            .into_column::<String, _>(works::Column::Id)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)
    }

    /// Works whose matcher key is any of `keys`, of every kind, read in
    /// chunks the backend accepts, so a page costs a few reads however
    /// many keys it lists.
    pub(crate) async fn works_with_match_keys(&self, keys: &[String]) -> Result<Vec<works::Model>> {
        find_chunked(self.store, keys, |part| {
            works::Entity::find().filter(works::Column::MatchKey.is_in(part.iter().cloned()))
        })
        .await
    }

    /// Works with any of `ids`.
    pub(crate) async fn works(&self, ids: &[String]) -> Result<Vec<works::Model>> {
        let (hit, miss) = split_covered(ids, |id| {
            self.pool.is_some_and(|p| p.works.contains_key(id))
        });
        let mut out: Vec<works::Model> = hit
            .into_iter()
            .filter_map(|id| self.pool?.works.get(id)?.clone())
            .collect();
        out.extend(
            find_chunked(self.store, &miss, |part| {
                works::Entity::find().filter(works::Column::Id.is_in(part.iter().cloned()))
            })
            .await?,
        );
        Ok(out)
    }

    /// Expressions of the works in `work_ids`.
    pub(crate) async fn expressions_of_works(
        &self,
        work_ids: &[String],
    ) -> Result<Vec<expressions::Model>> {
        let (hit, miss) = split_covered(work_ids, |id| {
            self.pool
                .is_some_and(|p| p.expressions_of_works_read.contains(id))
        });
        let mut out = Vec::new();
        if let Some(pool) = self.pool {
            let hit: BTreeSet<&String> = hit.into_iter().collect();
            out.extend(
                pool.work_expressions
                    .values()
                    .filter(|x| hit.contains(&x.work_id))
                    .cloned(),
            );
        }
        out.extend(
            find_chunked(self.store, &miss, |part| {
                expressions::Entity::find()
                    .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
            })
            .await?,
        );
        Ok(out)
    }

    /// Isolation overrides on the works in `work_ids`.
    pub(crate) async fn isolations(
        &self,
        work_ids: &[String],
    ) -> Result<Vec<resolution_overrides::Model>> {
        let (hit, miss) = split_covered(work_ids, |id| {
            self.pool.is_some_and(|p| p.isolations_read.contains(id))
        });
        let mut out = Vec::new();
        if let Some(pool) = self.pool {
            out.extend(
                pool.isolations
                    .iter()
                    .filter(|m| hit.contains(&&m.subject_id))
                    .cloned(),
            );
        }
        out.extend(
            find_chunked(self.store, &miss, |part| {
                resolution_overrides::Entity::find()
                    .filter(resolution_overrides::Column::SubjectKind.eq("work"))
                    .filter(resolution_overrides::Column::Action.eq("isolate"))
                    .filter(resolution_overrides::Column::SubjectId.is_in(part.iter().cloned()))
            })
            .await?,
        );
        Ok(out)
    }

    /// The holdings of the listings in `listing_ids` by the stored `holder`.
    pub(crate) async fn holdings(
        &self,
        listing_ids: &[String],
        holder: &str,
    ) -> Result<Vec<holdings::Model>> {
        let (hit, miss) = split_covered(listing_ids, |id| {
            self.pool
                .is_some_and(|p| p.holdings.contains_key(&(id.clone(), holder.to_string())))
        });
        let mut out: Vec<holdings::Model> = hit
            .into_iter()
            .filter_map(|id| {
                self.pool?
                    .holdings
                    .get(&(id.clone(), holder.to_string()))?
                    .clone()
            })
            .collect();
        out.extend(
            find_chunked(self.store, &miss, |part| {
                holdings::Entity::find()
                    .filter(holdings::Column::Holder.eq(holder))
                    .filter(holdings::Column::ListingId.is_in(part.iter().cloned()))
            })
            .await?,
        );
        Ok(out)
    }

    /// The targets among `ids` at `level` whose first-billed credit is
    /// stored.
    pub(crate) async fn first_credits(
        &self,
        level: resolve::Level,
        ids: &[String],
    ) -> Result<Vec<String>> {
        let kind = level.as_str().to_string();
        let (hit, miss) = split_covered(ids, |id| {
            self.pool
                .is_some_and(|p| p.credited.contains_key(&(kind.clone(), id.clone())))
        });
        let mut out: Vec<String> = hit
            .into_iter()
            .filter(|id| {
                self.pool
                    .is_some_and(|p| p.credited.get(&(kind.clone(), (*id).clone())) == Some(&true))
            })
            .cloned()
            .collect();
        out.extend(
            find_chunked(self.store, &miss, |part| {
                credits::Entity::find()
                    .filter(credits::Column::TargetKind.eq(kind.as_str()))
                    .filter(credits::Column::TargetId.is_in(part.iter().cloned()))
                    .filter(credits::Column::Position.eq(0))
            })
            .await?
            .into_iter()
            .map(|c| c.target_id),
        );
        Ok(out)
    }

    /// The works among `work_ids` that have a series entry.
    pub(crate) async fn works_with_series(&self, work_ids: &[String]) -> Result<Vec<String>> {
        let (hit, miss) = split_covered(work_ids, |id| {
            self.pool.is_some_and(|p| p.series.contains_key(id))
        });
        let mut out: Vec<String> = hit
            .into_iter()
            .filter(|id| self.pool.is_some_and(|p| p.series.get(*id) == Some(&true)))
            .cloned()
            .collect();
        out.extend(
            find_chunked(self.store, &miss, |part| {
                series_entries::Entity::find()
                    .filter(series_entries::Column::WorkId.is_in(part.iter().cloned()))
            })
            .await?
            .into_iter()
            .map(|e| e.work_id),
        );
        Ok(out)
    }
}
