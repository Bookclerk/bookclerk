//! Metadata enrichment over the library model's editions (book model step
//! 6, PR 4d).
//!
//! An enricher (a metadata provider, or the Audible catalog match) fills
//! what a held edition lacks: its work's description, subjects and cover,
//! its expression's language and its own cover. The edition records who
//! filled it (`enrich_source`, `enrich_confidence`, `enrich_updated_at`), so
//! the next run skips it.

use std::collections::{HashMap, HashSet};

use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use sea_orm::{QueryOrder, QuerySelect};

use super::{now_str, parse_dt, LibraryStore};
use crate::entities::{
    edition_enrich_attempts, edition_identifiers, editions, expressions, holdings, store_listings,
    works,
};
use crate::error::{LibraryError, Result};
use crate::store::library_model::{find_chunked, find_chunked_paged};
use crate::WorkEnrichment;

/// One held edition, with what an enricher needs to look it up and the
/// fields it may fill.
#[derive(Debug, Clone, PartialEq)]
pub struct EnrichCandidate {
    /// `editions.id`.
    pub edition_id: String,
    /// The edition's work (`works.id`), when its expression still names one.
    pub work_id: Option<String>,
    /// The edition's display title.
    pub title: String,
    /// The edition's subtitle, else a listing's.
    pub subtitle: Option<String>,
    /// The credited authors as the first listing of the edition prints
    /// them, when it does.
    pub authors: Option<String>,
    /// The credited narrators as the first listing of the edition prints
    /// them, when it does.
    pub narrators: Option<String>,
    /// The edition's running time in whole minutes, else a listing's.
    pub duration_minutes: Option<i64>,
    /// The marketplace of the edition's first listing (lower case, such as
    /// `us`), which scopes an ASIN found for it.
    pub marketplace: String,
    /// The edition's ISBN (the smallest when it has several).
    pub isbn: Option<String>,
    /// The edition's ASIN (the smallest when it has several), which a
    /// store listing or an earlier catalog match recorded.
    pub asin: Option<String>,
    /// The work's description, else a listing's.
    pub description: Option<String>,
    /// The work's subjects.
    pub subjects: Option<String>,
    /// The edition's cover, else a listing's.
    pub cover_url: Option<String>,
    /// The expression's language, else a listing's.
    pub language: Option<String>,
    /// Who already filled the edition (`editions.enrich_source`).
    pub enrich_source: Option<String>,
}

/// One page of [`LibraryStore::enrich_candidates_page`].
#[derive(Debug, Clone, PartialEq)]
pub struct EnrichCandidatePage {
    /// The page's held editions, in edition id order.
    pub candidates: Vec<EnrichCandidate>,
    /// The cursor of the next page; `None` once the walk is done.
    pub next: Option<String>,
}

/// An ASIN a catalog match found for an edition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionAsin {
    /// The ASIN as the catalog printed it (normalised when stored).
    pub value: String,
    /// The marketplace the ASIN is sold in (such as `us`).
    pub marketplace: String,
}

/// What an enricher fills on one edition; `None` fields are left alone.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EditionEnrichment {
    /// An ASIN to record as the edition's identifier. An ASIN another
    /// edition already holds stays there.
    pub asin: Option<EditionAsin>,
    /// Description for the work, when it has none.
    pub description: Option<String>,
    /// Subjects for the work, when it has none.
    pub subjects: Option<String>,
    /// Category labels for the work, when it has none.
    pub categories: Option<String>,
    /// Cover for the edition (and the work), when it has none.
    pub cover_url: Option<String>,
    /// Language for the expression, when it has none.
    pub language: Option<String>,
    /// Who filled it, recorded as `editions.enrich_source`.
    pub source: String,
    /// How sure the enricher was, from 0.0 to 1.0.
    pub confidence: f64,
}

/// How a gap-fill attempt that left an edition's gaps open ended
/// (`edition_enrich_attempts.outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichAttemptOutcome {
    /// A record filled some of the gaps, and others stay open.
    Partial,
    /// Every enricher answered, and no record matched or filled a gap.
    Unmatched,
}

impl EnrichAttemptOutcome {
    /// Every outcome, in the order the table's check lists them.
    pub const ALL: [Self; 2] = [Self::Partial, Self::Unmatched];

    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Unmatched => "unmatched",
        }
    }
}

/// `value` unless it is missing or blank.
fn present(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// `offered` when `stored` is missing or blank, else `None`.
fn lacking(stored: Option<&str>, offered: Option<&String>) -> Option<String> {
    offered
        .filter(|_| stored.is_none_or(|v| v.trim().is_empty()))
        .cloned()
}

impl LibraryStore {
    /// One page of the held editions an enricher walks, in edition id order.
    ///
    /// # Arguments
    ///
    /// * `after` - The previous page's [`EnrichCandidatePage::next`]; `None`
    ///   for the first page.
    /// * `limit` - Most editions the page reads (1 to 200, and at most the
    ///   database's `maxResultRows`).
    ///
    /// # Returns
    ///
    /// The page's editions that a holding still holds, and the cursor of
    /// the next page. An edition nobody holds is left out, so a page can be
    /// short or empty while more follow: walk until `next` is `None`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn enrich_candidates_page(
        &self,
        after: Option<&str>,
        limit: u64,
    ) -> Result<EnrichCandidatePage> {
        let want = match self.db_capabilities().max_result_rows {
            0 => limit.clamp(1, 200),
            rows => limit.clamp(1, 200).min(u64::from(rows).max(1)),
        };
        let mut query = editions::Entity::find()
            .order_by_asc(editions::Column::Id)
            .limit(want);
        if let Some(after) = after {
            query = query.filter(editions::Column::Id.gt(after));
        }
        let page = query.all(self.db()).await.map_err(LibraryError::Orm)?;
        let full = u64::try_from(page.len()).unwrap_or(u64::MAX) >= want;
        let next = page.last().filter(|_| full).map(|e| e.id.clone());
        if page.is_empty() {
            return Ok(EnrichCandidatePage {
                candidates: Vec::new(),
                next,
            });
        }

        let edition_ids: Vec<String> = page.iter().map(|e| e.id.clone()).collect();
        // An edition may have more listings than one read returns.
        let mut listings = find_chunked_paged::<store_listings::Entity, _>(
            self,
            &edition_ids,
            &[store_listings::Column::Id],
            |part| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::EditionId.is_in(part.to_vec()))
            },
        )
        .await?;
        listings.sort_by(|a, b| a.id.cmp(&b.id));
        let listing_ids: Vec<String> = listings.iter().map(|l| l.id.clone()).collect();
        // A listing may have more holdings than one read returns.
        let held: HashSet<String> = find_chunked_paged::<holdings::Entity, _>(
            self,
            &listing_ids,
            &[holdings::Column::Id],
            |part| {
                super::holdings::find_holdings()
                    .filter(holdings::Column::ListingId.is_in(part.to_vec()))
            },
        )
        .await?
        .into_iter()
        .map(|h| h.listing_id)
        .collect();
        let mut by_edition: HashMap<&str, Vec<&store_listings::Model>> = HashMap::new();
        for listing in &listings {
            by_edition
                .entry(listing.edition_id.as_str())
                .or_default()
                .push(listing);
        }
        let held_editions: Vec<&editions::Model> = page
            .iter()
            .filter(|e| {
                by_edition
                    .get(e.id.as_str())
                    .is_some_and(|ls| ls.iter().any(|l| held.contains(&l.id)))
            })
            .collect();

        let expression_ids: Vec<String> = held_editions
            .iter()
            .map(|e| e.expression_id.clone())
            .collect();
        let expressions: HashMap<String, expressions::Model> =
            find_chunked::<expressions::Entity, _>(self, &expression_ids, |part| {
                expressions::Entity::find().filter(expressions::Column::Id.is_in(part.to_vec()))
            })
            .await?
            .into_iter()
            .map(|x| (x.id.clone(), x))
            .collect();
        let work_ids: Vec<String> = expressions.values().map(|x| x.work_id.clone()).collect();
        let works: HashMap<String, works::Model> =
            find_chunked::<works::Entity, _>(self, &work_ids, |part| {
                works::Entity::find().filter(works::Column::Id.is_in(part.to_vec()))
            })
            .await?
            .into_iter()
            .map(|w| (w.id.clone(), w))
            .collect();
        let held_ids: Vec<String> = held_editions.iter().map(|e| e.id.clone()).collect();
        // The smallest ISBN and ASIN of each edition, keyed by scheme.
        let mut smallest: HashMap<(String, String), String> = HashMap::new();
        for identifier in find_chunked_paged::<edition_identifiers::Entity, _>(
            self,
            &held_ids,
            &[
                edition_identifiers::Column::EditionId,
                edition_identifiers::Column::Scheme,
                edition_identifiers::Column::Value,
                edition_identifiers::Column::Marketplace,
            ],
            |part| {
                edition_identifiers::Entity::find()
                    .filter(edition_identifiers::Column::EditionId.is_in(part.to_vec()))
                    .filter(edition_identifiers::Column::Scheme.is_in(["isbn", "asin"]))
            },
        )
        .await?
        {
            let entry = smallest
                .entry((identifier.edition_id, identifier.scheme))
                .or_insert_with(|| identifier.value.clone());
            if identifier.value < *entry {
                *entry = identifier.value;
            }
        }
        let identifier = |edition_id: &str, scheme: &str| {
            smallest
                .get(&(edition_id.to_string(), scheme.to_string()))
                .cloned()
        };

        let candidates = held_editions
            .into_iter()
            .map(|edition| {
                let listings = by_edition
                    .get(edition.id.as_str())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let from_listing = |field: fn(&store_listings::Model) -> Option<String>| {
                    listings.iter().find_map(|l| present(field(l)))
                };
                let expression = expressions.get(&edition.expression_id);
                let work = expression.and_then(|x| works.get(&x.work_id));
                EnrichCandidate {
                    edition_id: edition.id.clone(),
                    work_id: work.map(|w| w.id.clone()),
                    title: edition.title.clone(),
                    subtitle: present(edition.subtitle.clone())
                        .or_else(|| from_listing(|l| l.subtitle.clone())),
                    authors: from_listing(|l| l.authors.clone()),
                    narrators: from_listing(|l| l.narrators.clone()),
                    duration_minutes: edition
                        .duration_minutes
                        .or_else(|| listings.iter().find_map(|l| l.length_minutes)),
                    marketplace: listings
                        .first()
                        .map(|l| l.marketplace.clone())
                        .unwrap_or_default(),
                    isbn: identifier(&edition.id, "isbn"),
                    asin: identifier(&edition.id, "asin"),
                    description: present(work.and_then(|w| w.description.clone()))
                        .or_else(|| from_listing(|l| l.description.clone())),
                    subjects: present(work.and_then(|w| w.subjects.clone())),
                    cover_url: present(edition.cover_url.clone())
                        .or_else(|| from_listing(|l| l.cover_url.clone())),
                    language: present(expression.and_then(|x| x.language.clone()))
                        .or_else(|| from_listing(|l| l.language.clone())),
                    enrich_source: edition.enrich_source.clone(),
                }
            })
            .collect();
        Ok(EnrichCandidatePage { candidates, next })
    }

    /// Fills what edition `edition_id` lacks from `fill` and records who
    /// filled it.
    ///
    /// The work takes the description, subjects and cover it lacks (a work
    /// another edition already described keeps its own values), the
    /// expression the language it lacks, and the edition the cover it
    /// lacks. The edition's provenance is written last, so a write
    /// that fails before it leaves the edition unmarked and the next run
    /// tries again.
    ///
    /// # Arguments
    ///
    /// * `edition_id` - The edition filled.
    /// * `fill` - The values offered and who offers them.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] when the edition is gone, or
    /// [`LibraryError::Orm`] when a read or write fails.
    pub async fn enrich_edition(&self, edition_id: &str, fill: &EditionEnrichment) -> Result<()> {
        let Some(edition) = editions::Entity::find_by_id(edition_id.to_string())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Err(LibraryError::not_found(
                crate::EntityKind::Edition,
                edition_id,
            ));
        };
        let expression = expressions::Entity::find_by_id(edition.expression_id.clone())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(expression) = &expression {
            if let Some(work) = works::Entity::find_by_id(expression.work_id.clone())
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?
            {
                let work_fill = WorkEnrichment {
                    description: lacking(work.description.as_deref(), fill.description.as_ref()),
                    subjects: lacking(work.subjects.as_deref(), fill.subjects.as_ref()),
                    categories: lacking(work.categories.as_deref(), fill.categories.as_ref()),
                    cover_url: lacking(work.cover_url.as_deref(), fill.cover_url.as_ref()),
                };
                if !work_fill.is_empty() {
                    self.set_work_enrichment(&work.id, &work_fill).await?;
                }
            }
            if let Some(language) = lacking(expression.language.as_deref(), fill.language.as_ref())
            {
                let mut am: expressions::ActiveModel = expression.clone().into();
                am.language = Set(Some(language));
                am.updated_at = Set(now_str());
                am.update(self.db()).await.map_err(LibraryError::Orm)?;
            }
        }

        if let Some(asin) = &fill.asin {
            self.add_edition_asin(edition_id, asin).await?;
        }

        let cover = lacking(edition.cover_url.as_deref(), fill.cover_url.as_ref());
        let mut am: editions::ActiveModel = edition.into();
        if let Some(cover) = cover {
            am.cover_url = Set(Some(cover));
        }
        am.enrich_source = Set(Some(fill.source.clone()));
        am.enrich_confidence = Set(Some(fill.confidence));
        let now = now_str();
        am.enrich_updated_at = Set(Some(now.clone()));
        am.updated_at = Set(now);
        am.update(self.db()).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// The editions among `edition_ids` that `enricher` tried at or after
    /// `since` without filling every gap
    /// ([`LibraryStore::record_enrich_attempt`]).
    ///
    /// # Arguments
    ///
    /// * `enricher` - Which enricher asked (such as `metadata`).
    /// * `edition_ids` - The editions to check.
    /// * `since` - Attempts before this time no longer count.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn enrich_attempted_since(
        &self,
        enricher: &str,
        edition_ids: &[String],
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<HashSet<String>> {
        let rows = find_chunked::<edition_enrich_attempts::Entity, _>(self, edition_ids, |part| {
            edition_enrich_attempts::Entity::find()
                .filter(edition_enrich_attempts::Column::Enricher.eq(enricher))
                .filter(edition_enrich_attempts::Column::EditionId.is_in(part.iter().cloned()))
        })
        .await?;
        Ok(rows
            .into_iter()
            .filter(|row| parse_dt(&row.attempted_at) >= since)
            .map(|row| row.edition_id)
            .collect())
    }

    /// Records that `enricher` tried `edition_id` now and left gaps open,
    /// replacing its earlier attempt.
    ///
    /// # Arguments
    ///
    /// * `edition_id` - The edition asked about.
    /// * `enricher` - Which enricher asked (such as `metadata`).
    /// * `outcome` - How the attempt ended.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails (an edition that
    /// is gone fails its foreign key).
    pub async fn record_enrich_attempt(
        &self,
        edition_id: &str,
        enricher: &str,
        outcome: EnrichAttemptOutcome,
    ) -> Result<()> {
        use sea_orm::sea_query::OnConflict;
        let am = edition_enrich_attempts::ActiveModel {
            edition_id: Set(edition_id.to_string()),
            enricher: Set(enricher.to_string()),
            outcome: Set(outcome.as_str().to_string()),
            attempted_at: Set(now_str()),
        };
        edition_enrich_attempts::Entity::insert(am)
            .on_conflict(
                OnConflict::columns([
                    edition_enrich_attempts::Column::EditionId,
                    edition_enrich_attempts::Column::Enricher,
                ])
                .update_columns([
                    edition_enrich_attempts::Column::Outcome,
                    edition_enrich_attempts::Column::AttemptedAt,
                ])
                .to_owned(),
            )
            .exec_without_returning(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Records `asin` as an identifier of edition `edition_id`; an ASIN
    /// another edition already holds in that marketplace stays there.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    async fn add_edition_asin(&self, edition_id: &str, asin: &EditionAsin) -> Result<()> {
        use sea_orm::sea_query::OnConflict;
        let Some(key) = bookclerk_resolve::IdentifierKey::new(
            bookclerk_resolve::IdentifierScheme::Asin,
            &asin.value,
            Some(&asin.marketplace),
            &asin.marketplace,
        ) else {
            return Ok(());
        };
        let am = edition_identifiers::ActiveModel {
            edition_id: Set(edition_id.to_string()),
            scheme: Set(key.scheme.as_str().to_string()),
            value: Set(key.value.clone()),
            marketplace: Set(key.marketplace.clone()),
            strength: Set(key.strength().as_str().to_string()),
            strong_key: Set(key.strong_key()),
            created_at: Set(now_str()),
        };
        // The first edition to hold a strong identifier keeps it: either
        // conflict (the same row, or the strong key on another edition)
        // leaves the table as it is.
        edition_identifiers::Entity::insert(am)
            .on_conflict(OnConflict::new().do_nothing().to_owned())
            .exec_without_returning(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewBook;
    use crate::test_support::Profile;

    /// Walks every page of candidates with a one-edition page.
    async fn walk(store: &LibraryStore) -> Vec<EnrichCandidate> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .enrich_candidates_page(after.as_deref(), 1)
                .await
                .unwrap();
            out.extend(page.candidates);
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn held_editions_are_walked_and_filled_under_every_profile() {
        for profile in Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            for account in ["acct", "other", "gone"] {
                store
                    .upsert_account("libro", account, "us", None, true, "libro")
                    .await
                    .unwrap();
            }
            for account in ["acct", "other"] {
                let mut book = NewBook::minimal("9780441172719", account, "us", "Dune");
                book.plugin_key = "libro".into();
                book.source = "libro".into();
                book.asin = None;
                book.isbn = Some("9780441172719".into());
                book.authors = Some("Frank Herbert".into());
                store.upsert_book(&book).await.unwrap();
            }
            let mut unheld = NewBook::minimal("9780553293357", "gone", "us", "Foundation");
            unheld.plugin_key = "libro".into();
            unheld.source = "libro".into();
            unheld.asin = None;
            store.upsert_book(&unheld).await.unwrap();
            store.revoke_credentials("libro", "gone").await.unwrap();

            let candidates = walk(store).await;
            assert_eq!(candidates.len(), 1, "{name}: only the held edition");
            let dune = &candidates[0];
            assert_eq!(dune.title, "Dune", "{name}");
            assert_eq!(dune.isbn.as_deref(), Some("9780441172719"), "{name}");
            assert_eq!(dune.authors.as_deref(), Some("Frank Herbert"), "{name}");
            assert!(dune.work_id.is_some(), "{name}");
            assert_eq!(dune.description, None, "{name}");
            assert_eq!(dune.enrich_source, None, "{name}");

            store
                .enrich_edition(
                    &dune.edition_id,
                    &EditionEnrichment {
                        description: Some("Spice".into()),
                        subjects: Some("Fiction".into()),
                        cover_url: Some("https://img.example/dune.jpg".into()),
                        language: Some("en".into()),
                        source: "metadata:openlibrary".into(),
                        confidence: 0.7,
                        ..EditionEnrichment::default()
                    },
                )
                .await
                .unwrap();
            let filled = walk(store).await.remove(0);
            assert_eq!(filled.description.as_deref(), Some("Spice"), "{name}");
            assert_eq!(filled.subjects.as_deref(), Some("Fiction"), "{name}");
            assert_eq!(
                filled.cover_url.as_deref(),
                Some("https://img.example/dune.jpg"),
                "{name}"
            );
            assert_eq!(filled.language.as_deref(), Some("en"), "{name}");
            assert_eq!(
                filled.enrich_source.as_deref(),
                Some("metadata:openlibrary"),
                "{name}"
            );

            // A later enricher only takes what is still missing.
            store
                .enrich_edition(
                    &dune.edition_id,
                    &EditionEnrichment {
                        description: Some("Other".into()),
                        language: Some("fr".into()),
                        source: "audible".into(),
                        confidence: 0.9,
                        ..EditionEnrichment::default()
                    },
                )
                .await
                .unwrap();
            let again = walk(store).await.remove(0);
            assert_eq!(again.description.as_deref(), Some("Spice"), "{name}");
            assert_eq!(again.language.as_deref(), Some("en"), "{name}");
            assert_eq!(again.enrich_source.as_deref(), Some("audible"), "{name}");

            assert!(matches!(
                store
                    .enrich_edition("no-such-edition", &EditionEnrichment::default())
                    .await,
                Err(LibraryError::NotFound { .. })
            ));
        }
    }

    #[tokio::test]
    async fn a_matched_asin_is_recorded_once_under_every_profile() {
        for profile in Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            store
                .upsert_account("libro", "acct", "us", None, true, "libro")
                .await
                .unwrap();
            for (product, title) in [("9780441172719", "Dune"), ("9780553293357", "Foundation")] {
                let mut book = NewBook::minimal(product, "acct", "us", title);
                book.plugin_key = "libro".into();
                book.source = "libro".into();
                book.asin = None;
                book.narrators = Some("Scott Brick".into());
                book.length_minutes = Some(1260);
                store.upsert_book(&book).await.unwrap();
            }
            let candidates = walk(store).await;
            assert_eq!(candidates.len(), 2, "{name}");
            let first = &candidates[0];
            assert_eq!(first.asin, None, "{name}");
            assert_eq!(first.marketplace, "us", "{name}");
            assert_eq!(first.narrators.as_deref(), Some("Scott Brick"), "{name}");
            assert_eq!(first.duration_minutes, Some(1260), "{name}");

            let fill = EditionEnrichment {
                asin: Some(EditionAsin {
                    value: "b00test01".into(),
                    marketplace: "us".into(),
                }),
                categories: Some("Science Fiction".into()),
                source: "audible".into(),
                confidence: 0.95,
                ..EditionEnrichment::default()
            };
            store
                .enrich_edition(&first.edition_id, &fill)
                .await
                .unwrap();
            // Recording it again, or on another edition, changes nothing.
            store
                .enrich_edition(&first.edition_id, &fill)
                .await
                .unwrap();
            store
                .enrich_edition(&candidates[1].edition_id, &fill)
                .await
                .unwrap();

            let after = walk(store).await;
            assert_eq!(after[0].asin.as_deref(), Some("B00TEST01"), "{name}");
            assert_eq!(after[1].asin, None, "{name}: the first edition keeps it");
            let work_id = after[0].work_id.clone().unwrap();
            let work = works::Entity::find_by_id(work_id)
                .one(store.db())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                work.categories.as_deref(),
                Some("Science Fiction"),
                "{name}"
            );
            assert_eq!(
                store
                    .held_titles_of_edition(&after[0].edition_id)
                    .await
                    .unwrap()
                    .len(),
                1,
                "{name}"
            );
        }
    }

    #[test]
    fn blank_values_count_as_missing() {
        assert_eq!(present(Some("  ".into())), None);
        assert_eq!(present(Some("x".into())), Some("x".into()));
        let offered = Some("new".to_string());
        assert_eq!(lacking(Some(" "), offered.as_ref()), offered);
        assert_eq!(lacking(None, offered.as_ref()), offered);
        assert_eq!(lacking(Some("old"), offered.as_ref()), None);
    }
}
