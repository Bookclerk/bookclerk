//! What the per-node works search index holds for one work.
//!
//! `GET /api/library/works?q=` matches works in a node-local Tantivy index
//! (`bookclerk-search`), one document per top-level work. This module reads
//! the text a work's document is built from: its title, its editions'
//! titles and subtitles, authors, narrators, series and every identifier
//! its listings and editions carry. The index decides how to tokenize it.

use std::collections::{BTreeSet, HashMap, HashSet};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::library_model::find_chunked_paged;
use super::work_batch::WorkModel;
use super::LibraryStore;
use crate::entities::edition_identifiers;
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
        let mut docs = self.work_search_documents(&[work_id.to_string()]).await?;
        Ok(docs.pop().unwrap_or_else(|| (work_id.to_string(), None)))
    }

    /// The search documents of the top-level works of `work_ids`, read
    /// together in a few chunked lookups.
    ///
    /// # Arguments
    ///
    /// * `work_ids` - Works; an episode stands for its show.
    ///
    /// # Returns
    ///
    /// One entry per distinct top-level work, in id order: its id and its
    /// document, or `None` for the document when the work is gone or no
    /// longer held (its document is then removed from the index).
    ///
    /// # Errors
    ///
    /// Returns [`crate::LibraryError::Orm`] when a read fails.
    pub async fn work_search_documents(
        &self,
        work_ids: &[String],
    ) -> Result<Vec<(String, Option<WorkSearchDoc>)>> {
        let tops = self.top_level_works(work_ids).await?;
        let mut models = self.work_models(&tops).await?;
        let held: Vec<&WorkModel> = tops
            .iter()
            .filter_map(|top| models.get(top))
            .filter(|model| {
                let inputs = &model.inputs;
                inputs
                    .holdings
                    .iter()
                    .any(|h| inputs.listing_editions.contains_key(&h.listing_id))
            })
            .collect();
        let edition_ids: Vec<String> = held
            .iter()
            .flat_map(|model| model.inputs.editions.iter().map(|e| e.id.clone()))
            .collect();
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
        let mut identifiers_of: HashMap<&str, Vec<&str>> = HashMap::new();
        for identifier in &identifiers {
            identifiers_of
                .entry(identifier.edition_id.as_str())
                .or_default()
                .push(identifier.value.as_str());
        }
        let held: HashSet<String> = held
            .iter()
            .filter_map(|model| model.inputs.work.as_ref().map(|w| w.id.clone()))
            .collect();
        let mut out = Vec::with_capacity(tops.len());
        for top in tops {
            let doc = match models.remove(&top) {
                Some(model) if held.contains(&top) => search_doc(model, &identifiers_of),
                _ => None,
            };
            out.push((top, doc));
        }
        Ok(out)
    }
}

/// Builds a held work's search document from its model and its editions'
/// identifier values.
fn search_doc(
    model: WorkModel,
    identifiers_of: &HashMap<&str, Vec<&str>>,
) -> Option<WorkSearchDoc> {
    let WorkModel { inputs, names } = model;
    let work = inputs.work.as_ref()?;
    let mut titles = BTreeSet::new();
    for edition in &inputs.editions {
        titles.insert(edition.title.clone());
        titles.extend(edition.subtitle.clone());
    }
    for listing in &inputs.listings {
        titles.insert(listing.title.clone());
        titles.extend(listing.subtitle.clone());
    }
    titles.remove(&work.title);
    titles.retain(|t| !t.trim().is_empty());

    let mut ids = BTreeSet::new();
    for listing in &inputs.listings {
        ids.insert(listing.product_id.to_ascii_lowercase());
    }
    for edition in &inputs.editions {
        for value in identifiers_of
            .get(edition.id.as_str())
            .into_iter()
            .flatten()
        {
            ids.insert(value.to_ascii_lowercase());
        }
    }
    ids.retain(|id| !id.is_empty());

    Some(WorkSearchDoc {
        work_id: work.id.clone(),
        title: work.title.clone(),
        other_titles: titles.into_iter().collect(),
        authors: names.authors.into_iter().collect(),
        narrators: names.narrators.into_iter().collect(),
        series: names.series.into_iter().collect(),
        identifiers: ids.into_iter().collect(),
    })
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

#[cfg(test)]
#[path = "work_search_tests.rs"]
mod tests;
