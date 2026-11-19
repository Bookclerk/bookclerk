//! Reads the models of many top-level works at once, for the page index
//! and the works search index.
//!
//! A work's page index row, its filter values and its search document are
//! derived from its editions, listings, holdings, acquire rows, credits and
//! series. Reading those one work at a time costs about twenty round trips
//! to the database plugin per work, which made a rebuild of a 10,000-work
//! library take minutes (#1001). [`LibraryStore::work_models`] reads them for
//! a whole set of works in a fixed number of chunked lookups, so a page of
//! works costs about as many round trips as one work did.

use std::collections::{BTreeSet, HashMap, HashSet};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::library_index::WorkRowInputs;
use super::library_model::{find_chunked, find_chunked_paged};
use super::work_search::WorkNames;
use super::LibraryStore;
use crate::entities::{
    contributors, credits, edition_acquisitions, editions, expressions, holdings, series,
    series_entries, store_listings, work_tags, works,
};
use crate::error::Result;
use crate::WorkKind;

/// Credit target kinds the derived rows read.
const CREDIT_TARGET_KINDS: [&str; 3] = ["work", "expression", "edition"];

/// Credit roles the derived rows read.
const CREDIT_ROLES: [&str; 4] = ["author", "host", "narrator", "publisher"];

/// What one top-level work's derived rows are built from.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkModel {
    /// The work's editions, listings, holdings, first authors and series.
    pub inputs: WorkRowInputs,
    /// Every author, narrator, series and publisher name of the work.
    pub names: WorkNames,
}

/// The credits read for a batch, by `(target kind, target id, role)`, each
/// list in billing order (position, then contributor id).
type CreditIndex = HashMap<(String, String, String), Vec<credits::Model>>;

/// Adds each comma-separated, trimmed, non-empty name in `text` to `out`.
fn add_names(out: &mut BTreeSet<String>, text: Option<&str>) {
    for name in text.unwrap_or_default().split(',') {
        let name = name.trim();
        if !name.is_empty() {
            out.insert(name.to_string());
        }
    }
}

/// The role a work's authors are credited in: `host` for a podcast,
/// `author` otherwise.
fn author_role(work: &works::Model) -> &'static str {
    if work.kind == WorkKind::Podcast.as_str() {
        "host"
    } else {
        "author"
    }
}

/// Builds one work's [`WorkModel`] from the rows a batch read.
struct Assembler<'a> {
    /// Editions by the top-level work they belong to, each list in id order.
    editions_of: HashMap<&'a str, Vec<&'a editions::Model>>,
    /// Listings by edition id, each list in id order.
    listings_of: HashMap<&'a str, Vec<&'a store_listings::Model>>,
    /// Holdings by listing id, each list in id order.
    holdings_of: HashMap<&'a str, Vec<&'a holdings::Model>>,
    /// Acquire rows by edition id.
    acquisitions_of: HashMap<&'a str, Vec<&'a edition_acquisitions::Model>>,
    /// Credits by target and role.
    credits: CreditIndex,
    /// Contributors by id.
    people: HashMap<&'a str, &'a contributors::Model>,
    /// Series entries by work id, each list in series id order.
    entries_of: HashMap<&'a str, Vec<&'a series_entries::Model>>,
    /// Series names by series id.
    series_names: HashMap<&'a str, &'a str>,
    /// Library tags by work id, each list in key order, one per key.
    tags_of: HashMap<&'a str, Vec<&'a str>>,
}

impl Assembler<'_> {
    /// The credits of one target in one role, in billing order.
    fn credits(&self, kind: &str, id: &str, role: &str) -> &[credits::Model] {
        self.credits
            .get(&(kind.to_string(), id.to_string(), role.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    /// Names of the contributors credited in `role` on any of `targets`.
    fn credited_names<'t>(
        &self,
        targets: impl IntoIterator<Item = (&'t str, &'t str)>,
        role: &str,
        out: &mut BTreeSet<String>,
    ) {
        for (kind, id) in targets {
            for credit in self.credits(kind, id, role) {
                if let Some(person) = self.people.get(credit.contributor_id.as_str()) {
                    out.insert(person.name.clone());
                }
            }
        }
    }

    /// The work's authors (hosts for a podcast) in billing order, and the
    /// first one's sort name: the work's own credits, or without any, the
    /// first edition's (by id) that has some.
    fn authors(
        &self,
        work: &works::Model,
        editions: &[editions::Model],
    ) -> (Vec<String>, Option<String>) {
        let role = author_role(work);
        let targets = std::iter::once(("work", work.id.as_str()))
            .chain(editions.iter().map(|e| ("edition", e.id.as_str())));
        for (kind, id) in targets {
            let credits = self.credits(kind, id, role);
            if credits.is_empty() {
                continue;
            }
            let ordered: Vec<&contributors::Model> = credits
                .iter()
                .filter_map(|c| self.people.get(c.contributor_id.as_str()).copied())
                .collect();
            let first = ordered.first().map(|c| c.sort_name.clone());
            return (ordered.into_iter().map(|c| c.name.clone()).collect(), first);
        }
        (Vec::new(), None)
    }

    /// Builds the model of `work`.
    fn model(&self, work: &works::Model) -> WorkModel {
        let editions: Vec<editions::Model> = self
            .editions_of
            .get(work.id.as_str())
            .map(|list| list.iter().map(|e| (*e).clone()).collect())
            .unwrap_or_default();
        let mut listings: Vec<store_listings::Model> = editions
            .iter()
            .flat_map(|e| self.listings_of.get(e.id.as_str()).into_iter().flatten())
            .map(|l| (*l).clone())
            .collect();
        listings.sort_by(|a, b| a.id.cmp(&b.id));
        let listing_editions: HashMap<String, String> = listings
            .iter()
            .map(|l| (l.id.clone(), l.edition_id.clone()))
            .collect();
        let mut holdings: Vec<holdings::Model> = listings
            .iter()
            .flat_map(|l| self.holdings_of.get(l.id.as_str()).into_iter().flatten())
            .map(|h| (*h).clone())
            .collect();
        holdings.sort_by(|a, b| a.id.cmp(&b.id));
        let acquisitions: Vec<edition_acquisitions::Model> = editions
            .iter()
            .flat_map(|e| {
                self.acquisitions_of
                    .get(e.id.as_str())
                    .into_iter()
                    .flatten()
            })
            .map(|a| (*a).clone())
            .collect();
        let (authors, first_author_sort) = self.authors(work, &editions);
        let entries = self
            .entries_of
            .get(work.id.as_str())
            .map_or(&[][..], Vec::as_slice);
        let series = entries.first().and_then(|entry| {
            self.series_names
                .get(entry.series_id.as_str())
                .map(|name| ((*name).to_string(), entry.position.clone()))
        });

        let mut names = WorkNames::default();
        let narrator_targets = editions.iter().map(|e| ("edition", e.id.as_str())).chain(
            editions
                .iter()
                .map(|e| ("expression", e.expression_id.as_str())),
        );
        self.credited_names(narrator_targets, "narrator", &mut names.narrators);
        let author_targets = std::iter::once(("work", work.id.as_str()))
            .chain(editions.iter().map(|e| ("edition", e.id.as_str())));
        self.credited_names(author_targets, author_role(work), &mut names.authors);
        self.credited_names(
            editions.iter().map(|e| ("edition", e.id.as_str())),
            "publisher",
            &mut names.publishers,
        );
        for entry in entries {
            if let Some(name) = self.series_names.get(entry.series_id.as_str()) {
                names.series.insert((*name).to_string());
            }
        }
        for listing in &listings {
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

        WorkModel {
            inputs: WorkRowInputs {
                work: Some(work.clone()),
                editions,
                listing_editions,
                listings,
                holdings,
                authors,
                first_author_sort,
                series,
                acquisitions,
                tags: self
                    .tags_of
                    .get(work.id.as_str())
                    .map(|list| list.iter().map(|t| (*t).to_string()).collect())
                    .unwrap_or_default(),
            },
            names,
        }
    }
}

impl LibraryStore {
    /// Reads the models of `work_ids` (top-level works) in chunked lookups:
    /// a few statements per kind of row for the whole set, not per work.
    ///
    /// # Arguments
    ///
    /// * `work_ids` - Top-level works (a podcast's show, not its episodes).
    ///
    /// # Returns
    ///
    /// Each work's model by id. A work that is gone is left out; a show's
    /// model covers its episodes' editions.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LibraryError::Orm`] when a read fails.
    pub(crate) async fn work_models(
        &self,
        work_ids: &[String],
    ) -> Result<HashMap<String, WorkModel>> {
        let ids: Vec<String> = work_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let found = find_chunked::<works::Entity, _>(self, &ids, |part| {
            works::Entity::find().filter(works::Column::Id.is_in(part.iter().cloned()))
        })
        .await?;
        let top_ids: Vec<String> = found.iter().map(|w| w.id.clone()).collect();
        let episodes = find_chunked_paged::<works::Entity, _>(self, &top_ids, |part| {
            works::Entity::find()
                .filter(works::Column::ParentWorkId.is_in(part.iter().cloned()))
                .order_by_asc(works::Column::Id)
        })
        .await?;
        // Each work, episode or not, to the top-level work its rows count for.
        let mut top_of: HashMap<String, String> =
            top_ids.iter().map(|id| (id.clone(), id.clone())).collect();
        for episode in &episodes {
            if let Some(parent) = &episode.parent_work_id {
                top_of.insert(episode.id.clone(), parent.clone());
            }
        }
        let all_work_ids: Vec<String> = top_of.keys().cloned().collect();
        let expressions =
            find_chunked_paged::<expressions::Entity, _>(self, &all_work_ids, |part| {
                expressions::Entity::find()
                    .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
                    .order_by_asc(expressions::Column::Id)
            })
            .await?;
        let work_of_expression: HashMap<&str, &str> = expressions
            .iter()
            .map(|x| (x.id.as_str(), x.work_id.as_str()))
            .collect();
        let expression_ids: Vec<String> = expressions.iter().map(|x| x.id.clone()).collect();
        let editions = find_chunked_paged::<editions::Entity, _>(self, &expression_ids, |part| {
            editions::Entity::find()
                .filter(editions::Column::ExpressionId.is_in(part.iter().cloned()))
                .order_by_asc(editions::Column::Id)
        })
        .await?;
        let edition_ids: Vec<String> = editions.iter().map(|e| e.id.clone()).collect();
        let listings =
            find_chunked_paged::<store_listings::Entity, _>(self, &edition_ids, |part| {
                store_listings::Entity::find()
                    .filter(store_listings::Column::EditionId.is_in(part.iter().cloned()))
                    .order_by_asc(store_listings::Column::Id)
            })
            .await?;
        let listing_ids: Vec<String> = listings.iter().map(|l| l.id.clone()).collect();
        let holdings = find_chunked_paged::<holdings::Entity, _>(self, &listing_ids, |part| {
            holdings::Entity::find()
                .filter(holdings::Column::ListingId.is_in(part.iter().cloned()))
                .order_by_asc(holdings::Column::Id)
        })
        .await?;
        let acquisitions =
            find_chunked::<edition_acquisitions::Entity, _>(self, &edition_ids, |part| {
                edition_acquisitions::Entity::find()
                    .filter(edition_acquisitions::Column::EditionId.is_in(part.iter().cloned()))
            })
            .await?;
        // One credit lookup for every target the rows read: the works, the
        // editions and the editions' expressions.
        let mut targets: BTreeSet<String> = top_ids.iter().cloned().collect();
        targets.extend(edition_ids.iter().cloned());
        targets.extend(editions.iter().map(|e| e.expression_id.clone()));
        let targets: Vec<String> = targets.into_iter().collect();
        let mut credit_rows = find_chunked_paged::<credits::Entity, _>(self, &targets, |part| {
            credits::Entity::find()
                .filter(credits::Column::TargetKind.is_in(CREDIT_TARGET_KINDS))
                .filter(credits::Column::TargetId.is_in(part.iter().cloned()))
                .filter(credits::Column::Role.is_in(CREDIT_ROLES))
                .order_by_asc(credits::Column::TargetKind)
                .order_by_asc(credits::Column::TargetId)
                .order_by_asc(credits::Column::ContributorId)
                .order_by_asc(credits::Column::Role)
        })
        .await?;
        credit_rows.sort_by(|a, b| {
            (a.position, a.contributor_id.as_str()).cmp(&(b.position, b.contributor_id.as_str()))
        });
        let people_ids: Vec<String> = credit_rows
            .iter()
            .map(|c| c.contributor_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let people = find_chunked::<contributors::Entity, _>(self, &people_ids, |part| {
            contributors::Entity::find()
                .filter(contributors::Column::Id.is_in(part.iter().cloned()))
        })
        .await?;
        let entries = find_chunked_paged::<series_entries::Entity, _>(self, &top_ids, |part| {
            series_entries::Entity::find()
                .filter(series_entries::Column::WorkId.is_in(part.iter().cloned()))
                .order_by_asc(series_entries::Column::WorkId)
                .order_by_asc(series_entries::Column::SeriesId)
        })
        .await?;
        let series_ids: Vec<String> = entries
            .iter()
            .map(|e| e.series_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let series_rows = find_chunked::<series::Entity, _>(self, &series_ids, |part| {
            series::Entity::find().filter(series::Column::Id.is_in(part.iter().cloned()))
        })
        .await?;
        let tag_rows = find_chunked_paged::<work_tags::Entity, _>(self, &top_ids, |part| {
            work_tags::Entity::find()
                .filter(work_tags::Column::WorkId.is_in(part.iter().cloned()))
                .order_by_asc(work_tags::Column::WorkId)
                .order_by_asc(work_tags::Column::Key)
                .order_by_asc(work_tags::Column::Source)
        })
        .await?;

        let mut assembler = Assembler {
            editions_of: HashMap::new(),
            listings_of: HashMap::new(),
            holdings_of: HashMap::new(),
            acquisitions_of: HashMap::new(),
            credits: HashMap::new(),
            people: people.iter().map(|p| (p.id.as_str(), p)).collect(),
            entries_of: HashMap::new(),
            series_names: series_rows
                .iter()
                .map(|s| (s.id.as_str(), s.name.as_str()))
                .collect(),
            tags_of: HashMap::new(),
        };
        for edition in &editions {
            let top = work_of_expression
                .get(edition.expression_id.as_str())
                .and_then(|work| top_of.get(*work));
            if let Some(top) = top {
                assembler
                    .editions_of
                    .entry(top.as_str())
                    .or_default()
                    .push(edition);
            }
        }
        for listing in &listings {
            assembler
                .listings_of
                .entry(listing.edition_id.as_str())
                .or_default()
                .push(listing);
        }
        for holding in &holdings {
            assembler
                .holdings_of
                .entry(holding.listing_id.as_str())
                .or_default()
                .push(holding);
        }
        for acquisition in &acquisitions {
            assembler
                .acquisitions_of
                .entry(acquisition.edition_id.as_str())
                .or_default()
                .push(acquisition);
        }
        for credit in credit_rows {
            assembler
                .credits
                .entry((
                    credit.target_kind.clone(),
                    credit.target_id.clone(),
                    credit.role.clone(),
                ))
                .or_default()
                .push(credit);
        }
        for entry in &entries {
            assembler
                .entries_of
                .entry(entry.work_id.as_str())
                .or_default()
                .push(entry);
        }
        for tag in &tag_rows {
            let list = assembler.tags_of.entry(tag.work_id.as_str()).or_default();
            // Rows come in key order, so a key two sources share is adjacent;
            // the first spelling stands.
            let last_key = list
                .last()
                .map(|v| crate::store::library_index::facet_key(v));
            if last_key.as_deref() != Some(tag.key.as_str()) {
                list.push(tag.value.as_str());
            }
        }
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        Ok(found
            .iter()
            .filter(|w| wanted.contains(w.id.as_str()))
            .map(|work| (work.id.clone(), assembler.model(work)))
            .collect())
    }
}
