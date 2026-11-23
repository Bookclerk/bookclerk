//! What the host's player API (`/player`, `docs/adr/player-api.md`) reads
//! of the catalog: the item an app sees for an edition, the edition a work
//! is listed by, and the caller's play sessions.
//!
//! An **item** is an audio edition this library acquired. A work is listed
//! by one such edition: the one acquired most recently. Authors, narrators,
//! series, genres and publishers come from the page index's values for the
//! work, so they are the same names the library's own filters offer and
//! the same keys a player filter matches.

use std::collections::{BTreeMap, HashMap};

use bookclerk_resolve::MediaKind;
use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, EntityTrait, PaginatorTrait as _, QueryFilter, QueryOrder, QuerySelect,
};

use super::library_index::{WorkFacet, WorkPageQuery};
use super::library_model::find_chunked_paged;
use super::playback::{Bookmark, PlaybackSession};
use super::{parse_dt, LibraryStore};
use crate::entities::{
    bookmarks, edition_acquisitions, edition_identifiers, editions, expressions,
    library_work_facets, library_work_rows, playback_sessions, works,
};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;

/// One value of the page index on an item: its key and how it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerFacet {
    /// The value's key ([`crate::facet_key`]), what a filter matches.
    pub key: String,
    /// The value as shown.
    pub value: String,
}

/// What the player API shows of one item ([`LibraryStore::player_items`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerItem {
    /// The edition: the item id.
    pub edition_id: String,
    /// Its work.
    pub work_id: String,
    /// The edition's title.
    pub title: String,
    /// Its subtitle.
    pub subtitle: Option<String>,
    /// The work's description (may hold markup).
    pub description: Option<String>,
    /// The publication date as the store gave it.
    pub published_at: Option<String>,
    /// The runtime the catalog states, in whole minutes.
    pub runtime_minutes: Option<u64>,
    /// The expression's language tag.
    pub language: Option<String>,
    /// True when the expression is abridged.
    pub abridged: bool,
    /// Authors, in the order the work's row lists them.
    pub authors: Vec<PlayerFacet>,
    /// Narrators.
    pub narrators: Vec<PlayerFacet>,
    /// Series, the work's first series first.
    pub series: Vec<PlayerFacet>,
    /// The work's place in its first series, as written.
    pub series_position: Option<String>,
    /// Genres.
    pub genres: Vec<String>,
    /// Publishers.
    pub publishers: Vec<String>,
    /// The edition's ISBN, when known.
    pub isbn: Option<String>,
    /// The edition's ASIN, when known.
    pub asin: Option<String>,
    /// When the library acquired it.
    pub added_at: DateTime<Utc>,
    /// When its record last changed.
    pub updated_at: DateTime<Utc>,
}

/// Orders `facets` by where their value first appears in `display` (the
/// row's joined names), unlisted ones last by key.
fn in_display_order(mut facets: Vec<PlayerFacet>, display: Option<&str>) -> Vec<PlayerFacet> {
    let display = display.unwrap_or_default().to_lowercase();
    facets.sort_by_key(|f| (display.find(&f.key).unwrap_or(usize::MAX), f.key.clone()));
    facets
}

impl LibraryStore {
    /// The edition each work is listed by in the player API: of its audio
    /// editions this library acquired, the one acquired last. A work with
    /// none is left out.
    ///
    /// # Arguments
    ///
    /// * `work_ids` - Works (top-level works of the page index).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn player_editions_of_works(
        &self,
        work_ids: &[String],
    ) -> Result<HashMap<String, String>> {
        let expressions = find_chunked_paged::<expressions::Entity, _>(
            self,
            work_ids,
            &[expressions::Column::Id],
            |part| {
                expressions::Entity::find()
                    .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
            },
        )
        .await?;
        let work_of: HashMap<String, String> =
            expressions.into_iter().map(|e| (e.id, e.work_id)).collect();
        let expression_ids: Vec<String> = work_of.keys().cloned().collect();
        let editions = find_chunked_paged::<editions::Entity, _>(
            self,
            &expression_ids,
            &[editions::Column::Id],
            |part| {
                editions::Entity::find()
                    .filter(editions::Column::ExpressionId.is_in(part.iter().cloned()))
                    .filter(editions::Column::MediaKind.eq(MediaKind::Audio.as_str()))
            },
        )
        .await?;
        let edition_work: HashMap<String, String> = editions
            .into_iter()
            .filter_map(|e| work_of.get(&e.expression_id).map(|w| (e.id, w.clone())))
            .collect();
        let edition_ids: Vec<String> = edition_work.keys().cloned().collect();
        let acquired = find_chunked_paged::<edition_acquisitions::Entity, _>(
            self,
            &edition_ids,
            &[edition_acquisitions::Column::EditionId],
            |part| {
                edition_acquisitions::Entity::find()
                    .filter(edition_acquisitions::Column::EditionId.is_in(part.iter().cloned()))
                    .filter(
                        edition_acquisitions::Column::Status.eq(AcquireStatus::Acquired.as_str()),
                    )
            },
        )
        .await?;
        let mut best: HashMap<String, (String, String)> = HashMap::new();
        for row in acquired {
            let Some(work) = edition_work.get(&row.edition_id) else {
                continue;
            };
            let newer = best.get(work).is_none_or(|(at, id)| {
                (row.updated_at.as_str(), row.edition_id.as_str()) > (at.as_str(), id.as_str())
            });
            if newer {
                best.insert(work.clone(), (row.updated_at, row.edition_id));
            }
        }
        Ok(best.into_iter().map(|(work, (_, id))| (work, id)).collect())
    }

    /// The items of `edition_ids`, in the same order: audio editions this
    /// library acquired. An unknown, unacquired or non-audio edition is
    /// left out.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn player_items(&self, edition_ids: &[String]) -> Result<Vec<PlayerItem>> {
        let acquired: HashMap<String, edition_acquisitions::Model> =
            find_chunked_paged::<edition_acquisitions::Entity, _>(
                self,
                edition_ids,
                &[edition_acquisitions::Column::EditionId],
                |part| {
                    edition_acquisitions::Entity::find()
                        .filter(edition_acquisitions::Column::EditionId.is_in(part.iter().cloned()))
                        .filter(
                            edition_acquisitions::Column::Status
                                .eq(AcquireStatus::Acquired.as_str()),
                        )
                },
            )
            .await?
            .into_iter()
            .map(|a| (a.edition_id.clone(), a))
            .collect();
        let ids: Vec<String> = edition_ids
            .iter()
            .filter(|id| acquired.contains_key(*id))
            .cloned()
            .collect();
        let editions: HashMap<String, editions::Model> = find_chunked_paged::<editions::Entity, _>(
            self,
            &ids,
            &[editions::Column::Id],
            |part| {
                editions::Entity::find()
                    .filter(editions::Column::Id.is_in(part.iter().cloned()))
                    .filter(editions::Column::MediaKind.eq(MediaKind::Audio.as_str()))
            },
        )
        .await?
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
        let expression_ids: Vec<String> =
            editions.values().map(|e| e.expression_id.clone()).collect();
        let expressions: HashMap<String, expressions::Model> =
            find_chunked_paged::<expressions::Entity, _>(
                self,
                &expression_ids,
                &[expressions::Column::Id],
                |part| {
                    expressions::Entity::find()
                        .filter(expressions::Column::Id.is_in(part.iter().cloned()))
                },
            )
            .await?
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();
        let mut work_ids: Vec<String> = expressions.values().map(|e| e.work_id.clone()).collect();
        work_ids.sort();
        work_ids.dedup();
        let works: HashMap<String, works::Model> =
            find_chunked_paged::<works::Entity, _>(self, &work_ids, &[works::Column::Id], |part| {
                works::Entity::find().filter(works::Column::Id.is_in(part.iter().cloned()))
            })
            .await?
            .into_iter()
            .map(|w| (w.id.clone(), w))
            .collect();
        // An episode's facets sit on its show's row.
        let top_of = |work_id: &str| -> String {
            works
                .get(work_id)
                .and_then(|w| w.parent_work_id.clone())
                .unwrap_or_else(|| work_id.to_string())
        };
        let mut top_ids: Vec<String> = work_ids.iter().map(|w| top_of(w)).collect();
        top_ids.sort();
        top_ids.dedup();
        let rows: HashMap<String, library_work_rows::Model> =
            find_chunked_paged::<library_work_rows::Entity, _>(
                self,
                &top_ids,
                &[library_work_rows::Column::WorkId],
                |part| {
                    library_work_rows::Entity::find()
                        .filter(library_work_rows::Column::WorkId.is_in(part.iter().cloned()))
                },
            )
            .await?
            .into_iter()
            .map(|r| (r.work_id.clone(), r))
            .collect();
        let mut facets: HashMap<String, BTreeMap<&'static str, Vec<PlayerFacet>>> = HashMap::new();
        for row in find_chunked_paged::<library_work_facets::Entity, _>(
            self,
            &top_ids,
            &[
                library_work_facets::Column::WorkId,
                library_work_facets::Column::Facet,
                library_work_facets::Column::Key,
            ],
            |part| {
                library_work_facets::Entity::find()
                    .filter(library_work_facets::Column::WorkId.is_in(part.iter().cloned()))
            },
        )
        .await?
        {
            let Some(facet) = [
                WorkFacet::Author,
                WorkFacet::Narrator,
                WorkFacet::Series,
                WorkFacet::Genre,
                WorkFacet::Publisher,
            ]
            .into_iter()
            .map(WorkFacet::as_str)
            .find(|name| *name == row.facet) else {
                continue;
            };
            facets
                .entry(row.work_id)
                .or_default()
                .entry(facet)
                .or_default()
                .push(PlayerFacet {
                    key: row.key,
                    value: row.value,
                });
        }
        let mut identifiers: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
        for row in find_chunked_paged::<edition_identifiers::Entity, _>(
            self,
            &ids,
            &[
                edition_identifiers::Column::EditionId,
                edition_identifiers::Column::Scheme,
                edition_identifiers::Column::Value,
                edition_identifiers::Column::Marketplace,
            ],
            |part| {
                edition_identifiers::Entity::find()
                    .filter(edition_identifiers::Column::EditionId.is_in(part.iter().cloned()))
                    .filter(edition_identifiers::Column::Scheme.is_in(["isbn", "asin"]))
            },
        )
        .await?
        {
            let entry = identifiers.entry(row.edition_id).or_default();
            match row.scheme.as_str() {
                "isbn" => {
                    entry.0.get_or_insert(row.value);
                }
                _ => {
                    entry.1.get_or_insert(row.value);
                }
            }
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            let (Some(edition), Some(acquisition)) = (editions.get(id), acquired.get(id)) else {
                continue;
            };
            let Some(expression) = expressions.get(&edition.expression_id) else {
                continue;
            };
            let work = works.get(&expression.work_id);
            let top = top_of(&expression.work_id);
            let row = rows.get(&top);
            let mut of_work = facets.get(&top).cloned().unwrap_or_default();
            let mut take = |facet: WorkFacet| of_work.remove(facet.as_str()).unwrap_or_default();
            let authors = in_display_order(
                take(WorkFacet::Author),
                row.and_then(|r| r.authors.as_deref()),
            );
            let narrators = take(WorkFacet::Narrator);
            let series = in_display_order(
                take(WorkFacet::Series),
                row.and_then(|r| r.series.as_deref()),
            );
            let genres = take(WorkFacet::Genre)
                .into_iter()
                .map(|f| f.value)
                .collect();
            let publishers = take(WorkFacet::Publisher)
                .into_iter()
                .map(|f| f.value)
                .collect();
            let (isbn, asin) = identifiers.remove(id).unwrap_or_default();
            out.push(PlayerItem {
                edition_id: edition.id.clone(),
                work_id: expression.work_id.clone(),
                title: edition.title.clone(),
                subtitle: edition.subtitle.clone(),
                description: work.and_then(|w| w.description.clone()),
                published_at: edition.published_at.clone(),
                runtime_minutes: edition
                    .duration_minutes
                    .and_then(|m| u64::try_from(m).ok())
                    .filter(|m| *m > 0),
                language: expression.language.clone(),
                abridged: expression.abridgement == "abridged",
                authors,
                narrators,
                series,
                series_position: row.and_then(|r| r.series_position.clone()),
                genres,
                publishers,
                isbn,
                asin,
                added_at: parse_dt(&acquisition.updated_at),
                updated_at: parse_dt(&edition.updated_at),
            });
        }
        Ok(out)
    }

    /// The items of one page of works ([`Self::library_works_at`]), each
    /// work by the edition [`Self::player_editions_of_works`] picks, and
    /// how many works the query matches in all. A work with no acquired
    /// audio edition is left out of the page.
    ///
    /// # Arguments
    ///
    /// * `query` - Order and filters of the page index.
    /// * `offset` - Works to skip.
    /// * `limit` - Most works to read.
    /// * `reversed` - True to read the order backwards.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn player_items_page(
        &self,
        query: &WorkPageQuery,
        offset: u64,
        limit: u64,
        reversed: bool,
    ) -> Result<(Vec<PlayerItem>, u64)> {
        let (rows, total) = self
            .library_works_at(query, offset, limit, reversed)
            .await?;
        let work_ids: Vec<String> = rows.iter().map(|r| r.work_id.clone()).collect();
        let editions = self.player_editions_of_works(&work_ids).await?;
        let edition_ids: Vec<String> = work_ids
            .iter()
            .filter_map(|w| editions.get(w).cloned())
            .collect();
        Ok((self.player_items(&edition_ids).await?, total))
    }

    /// One person's play sessions, newest first: `limit` from `offset`,
    /// and how many they have in all.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn list_playback_sessions(
        &self,
        user_id: i64,
        offset: u64,
        limit: u64,
    ) -> Result<(Vec<PlaybackSession>, u64)> {
        use playback_sessions::Column as S;
        let select = playback_sessions::Entity::find().filter(S::UserId.eq(user_id));
        let total = select
            .clone()
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        if limit == 0 || offset >= total {
            return Ok((Vec::new(), total));
        }
        let select = select.order_by_desc(S::StartedAt).order_by_desc(S::Id);
        let page = u64::try_from(crate::control_plane::result_page(self)).unwrap_or(1);
        let rows = crate::control_plane::read_shrinking(page, Some(limit), |at, n| {
            let select = select.clone();
            async move {
                select
                    .offset(offset.saturating_add(at))
                    .limit(n)
                    .all(self.db())
                    .await
                    .map_err(LibraryError::Orm)
            }
        })
        .await?;
        Ok((
            rows.into_iter().map(super::playback::map_session).collect(),
            total,
        ))
    }
}

impl LibraryStore {
    /// Every bookmark one person made, at most `cap`, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn list_user_bookmarks(&self, user_id: i64, cap: u64) -> Result<Vec<Bookmark>> {
        let select = bookmarks::Entity::find()
            .filter(bookmarks::Column::UserId.eq(user_id))
            .order_by_asc(bookmarks::Column::CreatedAt);
        Ok(crate::control_plane::read_paged(self, select, Some(cap))
            .await?
            .into_iter()
            .map(super::playback::map_bookmark)
            .collect())
    }
}

#[cfg(test)]
#[path = "player_catalog_tests.rs"]
mod tests;
