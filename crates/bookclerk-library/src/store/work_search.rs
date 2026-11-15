//! What the per-node works search index holds for one work.
//!
//! `GET /api/library/works?q=` matches works in a node-local Tantivy index
//! (`bookclerk-search`), one document per top-level work. This module reads
//! the text a work's document is built from: its title, its editions'
//! titles and subtitles, authors, narrators, series and every identifier
//! its listings and editions carry. The index decides how to tokenize it.

use std::collections::BTreeSet;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::library_index::WorkRowInputs;
use super::library_model::find_chunked_paged;
use super::LibraryStore;
use crate::entities::{contributors, credits, edition_identifiers, series, series_entries, works};
use crate::error::Result;
use crate::WorkKind;

/// The searchable text of one held top-level work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkSearchDoc {
    /// The work (a podcast's show, never an episode).
    pub work_id: String,
    /// The work's title.
    pub title: String,
    /// Distinct edition and listing titles and subtitles other than
    /// [`Self::title`] (episode titles, for a show).
    pub other_titles: Vec<String>,
    /// Authors (hosts, for a podcast), from the model's credits and the
    /// listings' author text.
    pub authors: Vec<String>,
    /// Narrators, from the listings' narrator text.
    pub narrators: Vec<String>,
    /// Series names, from the model and the listings.
    pub series: Vec<String>,
    /// Product ids of the listings and identifier values (ISBN, ASIN and
    /// others) of the editions, lowercased.
    pub identifiers: Vec<String>,
}

/// Adds each comma-separated, trimmed, non-empty name in `text` to `out`.
fn add_names(out: &mut BTreeSet<String>, text: Option<&str>) {
    for name in text.unwrap_or_default().split(',') {
        let name = name.trim();
        if !name.is_empty() {
            out.insert(name.to_string());
        }
    }
}

impl LibraryStore {
    /// The search document of `work_id`'s top-level work.
    ///
    /// # Arguments
    ///
    /// * `work_id` - A work; an episode stands for its show.
    ///
    /// # Returns
    ///
    /// The top-level work's id and its document, or `None` for the document
    /// when the work is gone or no longer held (its document is then
    /// removed from the index).
    ///
    /// # Errors
    ///
    /// Returns [`crate::LibraryError::Orm`] when a read fails.
    pub async fn work_search_document(
        &self,
        work_id: &str,
    ) -> Result<(String, Option<WorkSearchDoc>)> {
        let top = self
            .top_level_works(&[work_id.to_string()])
            .await?
            .pop()
            .unwrap_or_else(|| work_id.to_string());
        let inputs = self.work_row_inputs(&top).await?;
        let Some(work) = inputs.work.as_ref() else {
            return Ok((top, None));
        };
        let held = inputs
            .holdings
            .iter()
            .any(|h| inputs.listing_editions.contains_key(&h.listing_id));
        if !held {
            return Ok((top, None));
        }
        let edition_ids: Vec<String> = inputs.editions.iter().map(|e| e.id.clone()).collect();
        let listings = &inputs.listings;
        let identifiers =
            find_chunked_paged::<edition_identifiers::Entity, _>(self, &edition_ids, |part| {
                edition_identifiers::Entity::find()
                    .filter(edition_identifiers::Column::EditionId.is_in(part.iter().cloned()))
                    .order_by_asc(edition_identifiers::Column::EditionId)
                    .order_by_asc(edition_identifiers::Column::Scheme)
                    .order_by_asc(edition_identifiers::Column::Value)
                    .order_by_asc(edition_identifiers::Column::Marketplace)
            })
            .await?;
        let names = self.work_names(work, &inputs).await?;

        let mut titles = BTreeSet::new();
        for edition in &inputs.editions {
            titles.insert(edition.title.clone());
            titles.extend(edition.subtitle.clone());
        }
        for listing in listings {
            titles.insert(listing.title.clone());
            titles.extend(listing.subtitle.clone());
        }
        titles.remove(&work.title);
        titles.retain(|t| !t.trim().is_empty());

        let mut ids = BTreeSet::new();
        for listing in listings {
            ids.insert(listing.product_id.to_ascii_lowercase());
        }
        ids.extend(identifiers.iter().map(|i| i.value.to_ascii_lowercase()));
        ids.retain(|id| !id.is_empty());

        Ok((
            top,
            Some(WorkSearchDoc {
                work_id: work.id.clone(),
                title: work.title.clone(),
                other_titles: titles.into_iter().collect(),
                authors: names.authors.into_iter().collect(),
                narrators: names.narrators.into_iter().collect(),
                series: names.series.into_iter().collect(),
                identifiers: ids.into_iter().collect(),
            }),
        ))
    }
}

/// Every author, narrator, series and publisher name of a work, each once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkNames {
    /// Authors (hosts, for a podcast), from the model's credits on the work
    /// and its editions and the listings' author text.
    pub authors: BTreeSet<String>,
    /// Narrators, from the credits on the editions and expressions and the
    /// listings' narrator text.
    pub narrators: BTreeSet<String>,
    /// Series names, from the model and the listings.
    pub series: BTreeSet<String>,
    /// Publishers, from the credits on the editions and the listings'
    /// publisher text.
    pub publishers: BTreeSet<String>,
}

impl LibraryStore {
    /// Every author, narrator, series and publisher name of the work `inputs` were
    /// read for, not only the ones the page index row shows.
    ///
    /// # Arguments
    ///
    /// * `work` - The top-level work.
    /// * `inputs` - What [`LibraryStore::work_row_inputs`] read for it.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LibraryError::Orm`] when a read fails.
    pub(crate) async fn work_names(
        &self,
        work: &works::Model,
        inputs: &WorkRowInputs,
    ) -> Result<WorkNames> {
        let mut people_targets: Vec<String> =
            inputs.editions.iter().map(|e| e.id.clone()).collect();
        people_targets.extend(inputs.editions.iter().map(|e| e.expression_id.clone()));
        let narrator_names = self
            .credited_names(&["edition", "expression"], &people_targets, "narrator")
            .await?;
        let author_role = if work.kind == WorkKind::Podcast.as_str() {
            "host"
        } else {
            "author"
        };
        let mut author_targets = vec![work.id.clone()];
        author_targets.extend(inputs.editions.iter().map(|e| e.id.clone()));
        let author_names = self
            .credited_names(&["work", "edition"], &author_targets, author_role)
            .await?;
        let edition_ids: Vec<String> = inputs.editions.iter().map(|e| e.id.clone()).collect();
        let publisher_names = self
            .credited_names(&["edition"], &edition_ids, "publisher")
            .await?;
        let mut names = WorkNames {
            authors: author_names.into_iter().collect(),
            narrators: narrator_names.into_iter().collect(),
            series: self.series_names(&work.id).await?.into_iter().collect(),
            publishers: publisher_names.into_iter().collect(),
        };
        for listing in &inputs.listings {
            add_names(&mut names.authors, listing.authors.as_deref());
            add_names(&mut names.narrators, listing.narrators.as_deref());
            if let Some(name) = listing.publisher.as_deref().map(str::trim) {
                if !name.is_empty() {
                    names.publishers.insert(name.to_string());
                }
            }
            if let Some(name) = listing.series.as_deref().map(str::trim) {
                if !name.is_empty() {
                    names.series.insert(name.to_string());
                }
            }
        }
        Ok(names)
    }
}

impl LibraryStore {
    /// Names of the contributors credited in `role` on any of `targets`
    /// (ids of the `kinds` of target), each once.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LibraryError::Orm`] when a read fails.
    async fn credited_names(
        &self,
        kinds: &[&str],
        targets: &[String],
        role: &str,
    ) -> Result<Vec<String>> {
        let targets: Vec<String> = targets
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let kinds: Vec<String> = kinds.iter().map(|k| (*k).to_string()).collect();
        let credited = find_chunked_paged::<credits::Entity, _>(self, &targets, |part| {
            credits::Entity::find()
                .filter(credits::Column::TargetKind.is_in(kinds.iter().cloned()))
                .filter(credits::Column::TargetId.is_in(part.iter().cloned()))
                .filter(credits::Column::Role.eq(role))
                .order_by_asc(credits::Column::TargetKind)
                .order_by_asc(credits::Column::TargetId)
                .order_by_asc(credits::Column::ContributorId)
                .order_by_asc(credits::Column::Role)
        })
        .await?;
        let ids: Vec<String> = credited
            .into_iter()
            .map(|c| c.contributor_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(
            find_chunked_paged::<contributors::Entity, _>(self, &ids, |part| {
                contributors::Entity::find()
                    .filter(contributors::Column::Id.is_in(part.iter().cloned()))
                    .order_by_asc(contributors::Column::Id)
            })
            .await?
            .into_iter()
            .map(|c| c.name)
            .collect(),
        )
    }

    /// Names of every series `work_id` belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LibraryError::Orm`] when a read fails.
    async fn series_names(&self, work_id: &str) -> Result<Vec<String>> {
        let entries =
            find_chunked_paged::<series_entries::Entity, _>(self, &[work_id.to_string()], |part| {
                series_entries::Entity::find()
                    .filter(series_entries::Column::WorkId.is_in(part.iter().cloned()))
                    .order_by_asc(series_entries::Column::SeriesId)
            })
            .await?;
        let ids: Vec<String> = entries.into_iter().map(|e| e.series_id).collect();
        Ok(find_chunked_paged::<series::Entity, _>(self, &ids, |part| {
            series::Entity::find()
                .filter(series::Column::Id.is_in(part.iter().cloned()))
                .order_by_asc(series::Column::Id)
        })
        .await?
        .into_iter()
        .map(|s| s.name)
        .collect())
    }
}

#[cfg(test)]
#[path = "work_search_tests.rs"]
mod tests;
