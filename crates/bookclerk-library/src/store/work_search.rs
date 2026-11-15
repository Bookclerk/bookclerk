//! What the per-node works search index holds for one work.
//!
//! `GET /api/library/works?q=` matches works in a node-local Tantivy index
//! (`bookclerk-search`), one document per top-level work. This module reads
//! the text a work's document is built from: its title, its editions'
//! titles and subtitles, authors, narrators, series and every identifier
//! its listings and editions carry. The index decides how to tokenize it.

use std::collections::BTreeSet;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::library_model::find_chunked_paged;
use super::LibraryStore;
use crate::entities::{contributors, credits, edition_identifiers, store_listings};
use crate::error::Result;

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
        let listings =
            find_chunked_paged::<store_listings::Entity, _>(self, &edition_ids, |part| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::EditionId.is_in(part.iter().cloned()))
                    .order_by_asc(store_listings::Column::Id)
            })
            .await?;
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

        let narrator_names = self.narrator_names(&inputs.editions).await?;

        let mut titles = BTreeSet::new();
        for edition in &inputs.editions {
            titles.insert(edition.title.clone());
            titles.extend(edition.subtitle.clone());
        }
        for listing in &listings {
            titles.insert(listing.title.clone());
            titles.extend(listing.subtitle.clone());
        }
        titles.remove(&work.title);
        titles.retain(|t| !t.trim().is_empty());

        let mut authors: BTreeSet<String> = inputs.authors.iter().cloned().collect();
        let mut narrators: BTreeSet<String> = narrator_names.into_iter().collect();
        let mut series: BTreeSet<String> =
            inputs.series.iter().map(|(name, _)| name.clone()).collect();
        let mut ids = BTreeSet::new();
        for listing in &listings {
            add_names(&mut authors, listing.authors.as_deref());
            add_names(&mut narrators, listing.narrators.as_deref());
            if let Some(name) = listing.series.as_deref().map(str::trim) {
                if !name.is_empty() {
                    series.insert(name.to_string());
                }
            }
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
                authors: authors.into_iter().collect(),
                narrators: narrators.into_iter().collect(),
                series: series.into_iter().collect(),
                identifiers: ids.into_iter().collect(),
            }),
        ))
    }
}

impl LibraryStore {
    /// Names of the narrators credited on `editions` or their expressions.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LibraryError::Orm`] when a read fails.
    async fn narrator_names(
        &self,
        editions: &[crate::entities::editions::Model],
    ) -> Result<Vec<String>> {
        let mut targets: BTreeSet<String> = editions.iter().map(|e| e.id.clone()).collect();
        targets.extend(editions.iter().map(|e| e.expression_id.clone()));
        let targets: Vec<String> = targets.into_iter().collect();
        let credited = find_chunked_paged::<credits::Entity, _>(self, &targets, |part| {
            credits::Entity::find()
                .filter(credits::Column::TargetKind.is_in(["edition", "expression"]))
                .filter(credits::Column::TargetId.is_in(part.iter().cloned()))
                .filter(credits::Column::Role.eq("narrator"))
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
}

#[cfg(test)]
#[path = "work_search_tests.rs"]
mod tests;
