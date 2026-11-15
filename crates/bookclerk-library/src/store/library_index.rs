//! The library's page index (`library_work_rows`): one row per held
//! top-level work.
//!
//! List views page this one table instead of joining works, expressions,
//! editions, listings and holdings for every request
//! (`docs/adr/library-data-model.md`, decision 8 and *Consequences*). A
//! row is derived data: [`LibraryStore::refresh_library_works`] reads a
//! work's model and writes its row (a work with no holding left keeps a
//! row with a holding count of 0, which pages skip, as the refresh fence),
//! or deletes it once the work is gone. The daemon's cluster-wide `host:library-index` consumer
//! calls it for the works an event touched, and a rebuild
//! ([`LibraryStore::rebuild_library_index_page`]) walks every work when the
//! index has no finished build of [`LIBRARY_INDEX_LAYOUT`].
//!
//! Two refreshes of one work can race on different nodes. Each stamps the
//! row with the time it started reading, and a write only replaces a row
//! stamped no later, so a slow refresh never overwrites a newer one.
//!
//! A podcast episode is a child work: its show is the work that gets a row,
//! and the show's row counts the episodes' editions and holdings.
//!
//! The same refresh writes the work's filter values (`library_work_facets`:
//! authors, narrators, series, genres, publishers and sources), fenced by
//! the row's stamp, so a page can filter by one value without joining the
//! model. Tags and progress are per person and filter through
//! `user_work_tags` and `user_title_state` instead.

use std::collections::{BTreeSet, HashMap, HashSet};

use bookclerk_resolve::{MediaKind, WorkKind};
use chrono::Utc;
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::library_model::find_chunked_paged;
use super::work_search::WorkNames;
use super::write_batches::WriteBatches;
use super::LibraryStore;
use crate::control_plane::batch;
use crate::entities::{
    contributors, credits, edition_acquisitions, editions, expressions, holdings,
    library_index_builds, library_work_facets, library_work_rows, series, series_entries,
    store_listings, user_title_state, user_work_tags, works,
};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;

/// Layout of `library_work_rows` this binary writes. A library with no
/// finished build of this layout is rebuilt; bump it when a change to how
/// rows are derived needs every row rewritten.
pub const LIBRARY_INDEX_LAYOUT: i64 = 1;

/// RFC 3339 now, with microseconds so stamps order as text.
fn stamp(at: chrono::DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Most works one rebuild page refreshes.
const REBUILD_PAGE: u64 = 100;

/// Leading articles a title's sort key drops.
const ARTICLES: [&str; 3] = ["the ", "a ", "an "];

/// The sort key of a title: lower case, with one leading English article
/// dropped, so "The Hobbit" sorts under h.
///
/// # Arguments
///
/// * `title` - The display title.
#[must_use]
pub fn sort_title(title: &str) -> String {
    let lower = title.trim().to_lowercase();
    for article in ARTICLES {
        if let Some(rest) = lower.strip_prefix(article) {
            let rest = rest.trim_start();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    lower
}

/// How a page of library works is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSort {
    /// By title sort key, A to Z.
    #[default]
    Title,
    /// By the first author's sort name, then title.
    Author,
    /// Newest holding first ("recently added").
    Added,
    /// By first series name and position, then title; works in no series
    /// last.
    Series,
    /// Newest purchase first; works no store reported a purchase time for
    /// last.
    Purchased,
    /// Longest runtime first; works with no known runtime last.
    Length,
}

/// A filter value list of the page index (`library_work_facets`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkFacet {
    /// Authors (hosts, for a podcast).
    Author,
    /// Narrators.
    Narrator,
    /// Series names.
    Series,
    /// Genres (store categories).
    Genre,
    /// Publishers.
    Publisher,
    /// Storefront plugins the work is held through.
    Source,
}

impl WorkFacet {
    /// Every facet, in the order a work's values are written.
    pub const ALL: [Self; 6] = [
        Self::Author,
        Self::Narrator,
        Self::Series,
        Self::Genre,
        Self::Publisher,
        Self::Source,
    ];

    /// The stored `facet` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Author => "author",
            Self::Narrator => "narrator",
            Self::Series => "series",
            Self::Genre => "genre",
            Self::Publisher => "publisher",
            Self::Source => "source",
        }
    }
}

/// The key a facet value or tag is stored and matched under: trimmed and
/// lower-cased, so "Fantasy" and "fantasy " are one value.
///
/// # Arguments
///
/// * `value` - A value as shown or as a client sent it.
#[must_use]
pub fn facet_key(value: &str) -> String {
    value.trim().to_lowercase()
}

/// One filter on a page of library works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkFilter {
    /// Works with this value under a facet (matched by [`facet_key`]). An
    /// empty series value matches works in no series.
    Facet {
        /// The facet.
        facet: WorkFacet,
        /// The value.
        value: String,
    },
    /// Works the user tagged with this tag (matched by [`facet_key`]). With
    /// no user (an operator token), nothing matches.
    Tag {
        /// The user (`users.id`) whose tags count.
        user_id: Option<i64>,
        /// The tag.
        tag: String,
    },
    /// Works the user marked finished, or did not. With no user, no work is
    /// finished.
    Finished {
        /// The user (`users.id`) whose state counts.
        user_id: Option<i64>,
        /// `true` for finished works, `false` for the rest.
        finished: bool,
    },
}

/// Where the next page of a [`WorkSort`] order starts: the sort values of
/// the last row returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCursor {
    /// The order the cursor belongs to; a cursor is refused under another.
    pub sort: WorkSort,
    /// The last row's primary sort value (sort title, sort author, added-at
    /// stamp, series key, purchase stamp or runtime in minutes).
    pub key: String,
    /// The last row's sort title; set for [`WorkSort::Author`] and
    /// [`WorkSort::Series`], whose ties break on title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The last row's work id, the final tie-break.
    pub work_id: String,
}

impl WorkCursor {
    /// The opaque token a client sends back: URL-safe base64 of the JSON.
    #[must_use]
    pub fn encode(&self) -> String {
        use base64::Engine as _;
        // Serializing a struct of strings cannot fail.
        let json = serde_json::to_vec(self).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    /// Parses a token from [`Self::encode`].
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] when the token is not one this
    /// library issued.
    pub fn decode(token: &str) -> Result<Self> {
        use base64::Engine as _;
        let invalid = || LibraryError::Invalid("invalid page cursor".into());
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.as_bytes())
            .map_err(|_| invalid())?;
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }
}

/// Filters and position of one page of library works.
#[derive(Debug, Clone, Default)]
pub struct WorkPageQuery {
    /// Order of the page.
    pub sort: WorkSort,
    /// Only works with an edition of this media kind.
    pub media_kind: Option<MediaKind>,
    /// Only works of this kind.
    pub kind: Option<WorkKind>,
    /// Only works whose furthest acquire state is this one.
    pub status: Option<AcquireStatus>,
    /// Only works held by this store account id (any plugin).
    pub account: Option<String>,
    /// Only works that pass this filter.
    pub filter: Option<WorkFilter>,
    /// Continue after this cursor.
    pub after: Option<WorkCursor>,
    /// Most rows to return; at least 1.
    pub limit: u64,
}

/// One row of the page index, as list views show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryWorkRow {
    /// The work.
    pub work_id: String,
    /// Book or podcast.
    pub kind: WorkKind,
    /// Display title.
    pub title: String,
    /// Authors (or hosts) joined for display.
    pub authors: Option<String>,
    /// First series name.
    pub series: Option<String>,
    /// Position in [`Self::series`].
    pub series_position: Option<String>,
    /// The edition whose cover the list shows.
    pub cover_edition_id: Option<String>,
    /// Media kinds of the work's editions, sorted.
    pub media_kinds: Vec<MediaKind>,
    /// The furthest acquire state any edition reached.
    pub acquire_status: AcquireStatus,
    /// How many editions the work has.
    pub edition_count: u64,
    /// How many holdings the work's listings have.
    pub holding_count: u64,
    /// RFC 3339 time of the newest holding.
    pub added_at: String,
    /// Runtime in minutes of the longest held edition, when known.
    pub length_minutes: Option<u64>,
    /// RFC 3339 time of the newest purchase a store reported.
    pub purchased_at: Option<String>,
}

/// One page of library works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPage {
    /// The rows, in the requested order.
    pub works: Vec<LibraryWorkRow>,
    /// Where the next page starts; `None` on the last page.
    pub next: Option<WorkCursor>,
}

/// Whose values a facet value list reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacetValuesOf {
    /// A facet of the page index.
    Facet(WorkFacet),
    /// One user's tags (`users.id`).
    Tags(i64),
}

/// One value of a facet value list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetValue {
    /// The value's [`facet_key`], the list's order and cursor.
    pub key: String,
    /// The value as shown (one of its spellings).
    pub value: String,
    /// How many held works have it.
    pub works: u64,
}

/// One page of a facet value list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetValuesPage {
    /// The values, by key.
    pub values: Vec<FacetValue>,
    /// The last key returned, where the next page starts; `None` on the
    /// last page.
    pub next: Option<String>,
}

/// Parses a stored comma-joined media kind set, skipping unknown values.
fn parse_media_kinds(stored: &str) -> Vec<MediaKind> {
    stored
        .split(',')
        .filter_map(|kind| MediaKind::parse(kind.trim()))
        .collect()
}

/// A stored count as `u64`; a negative value (never written) reads as 0.
fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

impl LibraryWorkRow {
    /// Reads a stored row; `None` when a stored enum is not one this binary
    /// knows (the row is skipped rather than shown wrong).
    fn from_model(model: library_work_rows::Model) -> Option<Self> {
        Some(Self {
            kind: WorkKind::parse(&model.kind)?,
            acquire_status: AcquireStatus::parse(&model.acquire_status)?,
            media_kinds: parse_media_kinds(&model.media_kinds),
            edition_count: count(model.edition_count),
            holding_count: count(model.holding_count),
            work_id: model.work_id,
            title: model.title,
            authors: model.authors,
            series: model.series,
            series_position: model.series_position,
            cover_edition_id: model.cover_edition_id,
            added_at: model.added_at,
            length_minutes: (model.length_minutes > 0).then(|| count(model.length_minutes)),
            purchased_at: (!model.purchased_at.is_empty()).then_some(model.purchased_at),
        })
    }
}

/// How far along the acquire pipeline a status is, for picking a work's
/// furthest state: acquired beats downloading beats queued beats a failure
/// beats never tried.
fn acquire_rank(status: AcquireStatus) -> u8 {
    match status {
        AcquireStatus::NotAcquired => 0,
        AcquireStatus::Error => 1,
        AcquireStatus::Queued => 2,
        AcquireStatus::Downloading => 3,
        AcquireStatus::Acquired => 4,
    }
}

/// Everything a work's row is derived from, read by
/// [`LibraryStore::work_row_inputs`].
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkRowInputs {
    /// The work (a top-level work).
    pub work: Option<works::Model>,
    /// Its editions, including its episodes' editions for a show.
    pub editions: Vec<editions::Model>,
    /// Listing id to edition id, for the editions above.
    pub listing_editions: HashMap<String, String>,
    /// The listings of those editions, in id order.
    pub listings: Vec<store_listings::Model>,
    /// Holdings of those listings.
    pub holdings: Vec<holdings::Model>,
    /// Display names of the work's authors (or hosts), in billing order.
    pub authors: Vec<String>,
    /// Sort name of the first author.
    pub first_author_sort: Option<String>,
    /// First series name and the work's position in it.
    pub series: Option<(String, Option<String>)>,
    /// Acquire rows of the editions.
    pub acquisitions: Vec<edition_acquisitions::Model>,
}

/// Derives a work's page index row; `None` when the work is gone (the row
/// is then deleted).
///
/// A work with no holding left keeps a row with a `holding_count` of 0,
/// which list views skip. The row is the refresh fence: its `refreshed_at`
/// stops a refresh that read the work while it was still held from writing
/// it back.
///
/// # Arguments
///
/// * `inputs` - What [`LibraryStore::work_row_inputs`] read.
/// * `refreshed_at` - When the read started (the row's `refreshed_at`).
pub(crate) fn build_work_row(
    inputs: &WorkRowInputs,
    refreshed_at: &str,
) -> Option<library_work_rows::Model> {
    let work = inputs.work.as_ref()?;
    let held: Vec<&holdings::Model> = inputs
        .holdings
        .iter()
        .filter(|h| inputs.listing_editions.contains_key(&h.listing_id))
        .collect();
    let added_at = held
        .iter()
        .map(|h| h.created_at.as_str())
        .max()
        .unwrap_or(work.created_at.as_str());
    let held_editions: HashSet<&str> = held
        .iter()
        .filter_map(|h| inputs.listing_editions.get(&h.listing_id))
        .map(String::as_str)
        .collect();
    let media: BTreeSet<&str> = inputs
        .editions
        .iter()
        .map(|e| e.media_kind.as_str())
        .collect();
    let statuses: HashMap<&str, AcquireStatus> = inputs
        .acquisitions
        .iter()
        .filter_map(|a| Some((a.edition_id.as_str(), AcquireStatus::parse(&a.status)?)))
        .collect();
    let acquire_status = statuses
        .values()
        .copied()
        .max_by_key(|s| acquire_rank(*s))
        .unwrap_or(AcquireStatus::NotAcquired);
    // The cover: a held edition with a cover, acquired editions first, then
    // audio, then by id so the choice is stable.
    let cover_edition_id = inputs
        .editions
        .iter()
        .filter(|e| e.cover_url.as_deref().is_some_and(|url| !url.is_empty()))
        .min_by_key(|e| {
            (
                !held_editions.contains(e.id.as_str()),
                statuses.get(e.id.as_str()) != Some(&AcquireStatus::Acquired),
                e.media_kind != MediaKind::Audio.as_str(),
                e.id.clone(),
            )
        })
        .map(|e| e.id.clone());
    let authors = (!inputs.authors.is_empty()).then(|| inputs.authors.join(", "));
    let sort_author = inputs
        .first_author_sort
        .clone()
        .unwrap_or_default()
        .to_lowercase();
    let (series, series_position) = match &inputs.series {
        Some((name, position)) => (Some(name.clone()), position.clone()),
        None => (None, None),
    };
    let held_listings: HashSet<&str> = held.iter().map(|h| h.listing_id.as_str()).collect();
    // The longest held edition's runtime; a listing's when no held edition
    // has one.
    let length_minutes = inputs
        .editions
        .iter()
        .filter(|e| held_editions.contains(e.id.as_str()))
        .filter_map(|e| e.duration_minutes)
        .max()
        .or_else(|| {
            inputs
                .listings
                .iter()
                .filter(|l| held_listings.contains(l.id.as_str()))
                .filter_map(|l| l.length_minutes)
                .max()
        })
        .unwrap_or(0)
        .max(0);
    let purchased_at = held
        .iter()
        .filter_map(|h| h.purchased_at.as_deref())
        .max()
        .unwrap_or_default();
    Some(library_work_rows::Model {
        work_id: work.id.clone(),
        kind: work.kind.clone(),
        title: work.title.clone(),
        sort_title: sort_title(&work.title),
        authors,
        sort_author,
        series,
        series_position,
        cover_edition_id,
        media_kinds: media.into_iter().collect::<Vec<_>>().join(","),
        acquire_status: acquire_status.as_str().to_string(),
        edition_count: i64::try_from(inputs.editions.len()).unwrap_or(i64::MAX),
        holding_count: i64::try_from(held.len()).unwrap_or(i64::MAX),
        added_at: added_at.to_string(),
        sort_series: series_sort_key(inputs.series.as_ref()),
        length_minutes,
        purchased_at: purchased_at.to_string(),
        refreshed_at: refreshed_at.to_string(),
    })
}

/// Separates the series name from the position in a series sort key; below
/// every printable character, so "Dune" sorts before "Dune Chronicles".
const SERIES_KEY_SEPARATOR: char = '\u{1}';

/// The series sort key of a work: `0`, the lower-cased series name, a
/// separator and the position key; `1` for a work in no series, so those
/// sort last.
///
/// The position key is the first number in the position, its whole part
/// zero-padded to ten digits so "2" sorts before "10", or `~` (after every
/// digit) when the position has no number.
///
/// # Arguments
///
/// * `series` - The work's first series name and its position there.
fn series_sort_key(series: Option<&(String, Option<String>)>) -> String {
    let Some((name, position)) = series else {
        return "1".to_string();
    };
    let position = position.as_deref().and_then(first_number).map_or_else(
        || "~".to_string(),
        |(whole, fraction)| match fraction {
            Some(fraction) => format!("{whole:0>10}.{fraction}"),
            None => format!("{whole:0>10}"),
        },
    );
    format!(
        "0{}{SERIES_KEY_SEPARATOR}{position}",
        name.trim().to_lowercase()
    )
}

/// The first number in `text` (as in "Book 3" or "#3.5"): its whole part
/// without leading zeros and its fractional digits, if any.
fn first_number(text: &str) -> Option<(String, Option<String>)> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let rest = &text[start..];
    let whole_len = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let whole = rest[..whole_len].trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let fraction = rest[whole_len..].strip_prefix('.').and_then(|tail| {
        let len = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        (len > 0).then(|| tail[..len].to_string())
    });
    Some((whole.to_string(), fraction))
}

/// Longest facet value (in bytes) the index stores; a longer one is left
/// out rather than fail the row's write against the database's cell limit.
const MAX_FACET_VALUE_BYTES: usize = 256;

/// Splits a store's category text into genres: on commas, semicolons and
/// bars, trimmed, empty pieces dropped.
fn split_genres(text: &str) -> impl Iterator<Item = &str> {
    text.split([',', ';', '|'])
        .map(str::trim)
        .filter(|g| !g.is_empty())
}

/// Derives a work's filter values: `(facet, key, value)`, one per key and
/// facet, the first spelling of a key kept. An unheld work has none.
///
/// # Arguments
///
/// * `inputs` - What [`LibraryStore::work_row_inputs`] read.
/// * `names` - The work's authors, narrators, series and publishers
///   ([`LibraryStore::work_names`]).
pub(crate) fn build_work_facets(
    inputs: &WorkRowInputs,
    names: &WorkNames,
) -> Vec<(WorkFacet, String, String)> {
    let held_listings: HashSet<&str> = inputs
        .holdings
        .iter()
        .filter(|h| inputs.listing_editions.contains_key(&h.listing_id))
        .map(|h| h.listing_id.as_str())
        .collect();
    let Some(work) = inputs.work.as_ref() else {
        return Vec::new();
    };
    if held_listings.is_empty() {
        return Vec::new();
    }
    let mut values: Vec<(WorkFacet, &str)> = Vec::new();
    values.extend(
        names
            .authors
            .iter()
            .map(|n| (WorkFacet::Author, n.as_str())),
    );
    values.extend(
        names
            .narrators
            .iter()
            .map(|n| (WorkFacet::Narrator, n.as_str())),
    );
    values.extend(names.series.iter().map(|n| (WorkFacet::Series, n.as_str())));
    values.extend(
        names
            .publishers
            .iter()
            .map(|n| (WorkFacet::Publisher, n.as_str())),
    );
    values.extend(
        split_genres(work.categories.as_deref().unwrap_or_default()).map(|g| (WorkFacet::Genre, g)),
    );
    for listing in &inputs.listings {
        values.extend(
            split_genres(listing.categories.as_deref().unwrap_or_default())
                .map(|g| (WorkFacet::Genre, g)),
        );
        if held_listings.contains(listing.id.as_str()) {
            values.push((WorkFacet::Source, listing.plugin_key.as_str()));
        }
    }
    let mut seen: HashSet<(WorkFacet, String)> = HashSet::new();
    let mut out = Vec::new();
    for (facet, value) in values {
        let value = value.trim();
        let key = facet_key(value);
        if key.is_empty() || value.len() > MAX_FACET_VALUE_BYTES {
            continue;
        }
        if seen.insert((facet, key.clone())) {
            out.push((facet, key, value.to_string()));
        }
    }
    out
}

impl LibraryStore {
    /// Most rows one read may return: the database plugin's advertised
    /// `maxResultRows`, or no cap when it advertises none. At least 1.
    fn result_row_cap(&self) -> u64 {
        match self.db_capabilities().max_result_rows {
            0 => u64::MAX,
            rows => u64::from(rows),
        }
    }

    /// The top-level works of `work_ids`: a podcast episode's show stands
    /// in for the episode, and ids with no work are kept (their rows are
    /// deleted by the refresh).
    pub(super) async fn top_level_works(&self, work_ids: &[String]) -> Result<Vec<String>> {
        let found =
            super::library_model::find_chunked::<works::Entity, _>(self, work_ids, |part| {
                works::Entity::find().filter(works::Column::Id.is_in(part.iter().cloned()))
            })
            .await?;
        let parents: HashMap<String, Option<String>> = found
            .into_iter()
            .map(|w| (w.id, w.parent_work_id))
            .collect();
        let mut out: Vec<String> = work_ids
            .iter()
            .map(|id| match parents.get(id) {
                Some(Some(parent)) => parent.clone(),
                _ => id.clone(),
            })
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Reads everything `work_id`'s row is derived from.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub(crate) async fn work_row_inputs(&self, work_id: &str) -> Result<WorkRowInputs> {
        let db = self.db();
        let Some(work) = works::Entity::find_by_id(work_id.to_string())
            .one(db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(WorkRowInputs::default());
        };
        let mut work_ids = vec![work.id.clone()];
        let episodes = find_chunked_paged::<works::Entity, _>(self, &work_ids, |part| {
            works::Entity::find()
                .filter(works::Column::ParentWorkId.is_in(part.iter().cloned()))
                .order_by_asc(works::Column::Id)
        })
        .await?;
        work_ids.extend(episodes.into_iter().map(|w| w.id));
        let expression_ids: Vec<String> =
            find_chunked_paged::<expressions::Entity, _>(self, &work_ids, |part| {
                expressions::Entity::find()
                    .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
                    .order_by_asc(expressions::Column::Id)
            })
            .await?
            .into_iter()
            .map(|x| x.id)
            .collect();
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
        let listing_editions: HashMap<String, String> = listings
            .iter()
            .map(|l| (l.id.clone(), l.edition_id.clone()))
            .collect();
        let listing_ids: Vec<String> = listing_editions.keys().cloned().collect();
        let holdings = find_chunked_paged::<holdings::Entity, _>(self, &listing_ids, |part| {
            holdings::Entity::find()
                .filter(holdings::Column::ListingId.is_in(part.iter().cloned()))
                .order_by_asc(holdings::Column::Id)
        })
        .await?;
        let acquisitions = super::library_model::find_chunked::<edition_acquisitions::Entity, _>(
            self,
            &edition_ids,
            |part| {
                edition_acquisitions::Entity::find()
                    .filter(edition_acquisitions::Column::EditionId.is_in(part.iter().cloned()))
            },
        )
        .await?;
        let (authors, first_author_sort) = self.work_authors(&work, &editions).await?;
        let series = self.work_series(&work.id).await?;
        Ok(WorkRowInputs {
            work: Some(work),
            editions,
            listing_editions,
            listings,
            holdings,
            authors,
            first_author_sort,
            series,
            acquisitions,
        })
    }

    /// The display names of a work's authors (hosts for a podcast) in
    /// billing order, and the first one's sort name. The work's own credits
    /// come first; without any, the first edition (by id) that has some.
    pub(super) async fn work_authors(
        &self,
        work: &works::Model,
        editions: &[editions::Model],
    ) -> Result<(Vec<String>, Option<String>)> {
        let role = if work.kind == WorkKind::Podcast.as_str() {
            "host"
        } else {
            "author"
        };
        let mut targets: Vec<(&str, &str)> = vec![("work", work.id.as_str())];
        targets.extend(editions.iter().map(|e| ("edition", e.id.as_str())));
        for (kind, id) in targets {
            // Paged, so a target with more credits than the database
            // returns per statement still reads them all.
            let credits = super::library_model::find_chunked_paged::<credits::Entity, _>(
                self,
                &[id.to_string()],
                |part| {
                    credits::Entity::find()
                        .filter(credits::Column::TargetKind.eq(kind))
                        .filter(credits::Column::TargetId.is_in(part.iter().cloned()))
                        .filter(credits::Column::Role.eq(role))
                        .order_by_asc(credits::Column::Position)
                        .order_by_asc(credits::Column::ContributorId)
                },
            )
            .await?;
            if credits.is_empty() {
                continue;
            }
            let ids: Vec<String> = credits.iter().map(|c| c.contributor_id.clone()).collect();
            let people: HashMap<String, contributors::Model> =
                super::library_model::find_chunked_paged::<contributors::Entity, _>(
                    self,
                    &ids,
                    |part| {
                        contributors::Entity::find()
                            .filter(contributors::Column::Id.is_in(part.iter().cloned()))
                            .order_by_asc(contributors::Column::Id)
                    },
                )
                .await?
                .into_iter()
                .map(|c| (c.id.clone(), c))
                .collect();
            let ordered: Vec<&contributors::Model> =
                ids.iter().filter_map(|id| people.get(id)).collect();
            let first = ordered.first().map(|c| c.sort_name.clone());
            return Ok((ordered.into_iter().map(|c| c.name.clone()).collect(), first));
        }
        Ok((Vec::new(), None))
    }

    /// The first series (by name key) a work belongs to, with its position.
    pub(super) async fn work_series(
        &self,
        work_id: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        let Some(entry) = series_entries::Entity::find()
            .filter(series_entries::Column::WorkId.eq(work_id))
            .order_by_asc(series_entries::Column::SeriesId)
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let name = series::Entity::find_by_id(entry.series_id.clone())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(|s| s.name);
        Ok(name.map(|name| (name, entry.position)))
    }

    /// Refreshes the page index rows of `work_ids` (an episode refreshes its
    /// show): writes each work's row (an unheld work's with a holding count
    /// of 0, which pages skip) and deletes the row of a work that is gone.
    ///
    /// # Arguments
    ///
    /// * `work_ids` - Works whose editions, listings or holdings changed.
    ///
    /// # Returns
    ///
    /// How many rows were written or deleted (a write skipped because a
    /// later refresh already wrote the row is not counted).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] or a batch error when a read or write
    /// fails.
    pub async fn refresh_library_works(&self, work_ids: &[String]) -> Result<u64> {
        let mut changed = 0;
        for work_id in self.top_level_works(work_ids).await? {
            let refreshed_at = stamp(Utc::now());
            let inputs = self.work_row_inputs(&work_id).await?;
            let row = build_work_row(&inputs, &refreshed_at);
            let facets = match (&inputs.work, &row) {
                (Some(work), Some(row)) if row.holding_count > 0 => {
                    build_work_facets(&inputs, &self.work_names(work, &inputs).await?)
                }
                _ => Vec::new(),
            };
            changed += self
                .write_work_row(&work_id, row, &facets, &refreshed_at)
                .await?;
        }
        if changed > 0 {
            self.bump_live_topic(crate::control_plane::LiveTopic::Library)
                .await?;
        }
        Ok(changed)
    }

    /// Writes (or, for a work that is gone, deletes) one work's row and its
    /// filter values, unless a refresh that started later already wrote it.
    ///
    /// The row, the removal of its old values and as many new values as
    /// fit go in one atomic batch; values past the database's batch limits
    /// follow in further batches. Every value write checks that the row
    /// still carries this refresh's stamp, so once a later refresh has
    /// written the row, the rest of this one's values are dropped.
    async fn write_work_row(
        &self,
        work_id: &str,
        row: Option<library_work_rows::Model>,
        facets: &[(WorkFacet, String, String)],
        refreshed_at: &str,
    ) -> Result<u64> {
        let operation_id = |n: usize| format!("library-index-{n}-{}", Uuid::new_v4());
        let Some(row) = row else {
            // The work is gone; its values went with it.
            let reply = self
                .execute_host_batch(batch::request(
                    &operation_id(0),
                    vec![batch::exec(
                        "DELETE FROM library_work_rows WHERE work_id = ? AND refreshed_at <= ?",
                        vec![batch::text(work_id), batch::text(refreshed_at)],
                    )],
                ))
                .await?;
            return Ok(batch::rows_affected(&reply, 0).min(1));
        };
        let values = || {
            vec![
                batch::text(&row.kind),
                batch::text(&row.title),
                batch::text(&row.sort_title),
                batch::text_opt(row.authors.as_deref()),
                batch::text(&row.sort_author),
                batch::text_opt(row.series.as_deref()),
                batch::text_opt(row.series_position.as_deref()),
                batch::text_opt(row.cover_edition_id.as_deref()),
                batch::text(&row.media_kinds),
                batch::text(&row.acquire_status),
                batch::int(row.edition_count),
                batch::int(row.holding_count),
                batch::text(&row.added_at),
                batch::text(&row.sort_series),
                batch::int(row.length_minutes),
                batch::text(&row.purchased_at),
                batch::text(&row.refreshed_at),
            ]
        };
        let mut insert = vec![batch::text(&row.work_id)];
        insert.extend(values());
        insert.push(batch::text(&row.work_id));
        let mut update = values();
        update.push(batch::text(&row.work_id));
        update.push(batch::text(refreshed_at));
        // Holds while the row carries this refresh's stamp.
        const FENCE: &str =
            "EXISTS (SELECT 1 FROM library_work_rows WHERE work_id = ? AND refreshed_at = ?)";
        let row_statements = vec![
            batch::exec(
                "INSERT OR IGNORE INTO library_work_rows (\
                    work_id, kind, title, sort_title, authors, sort_author, series, \
                    series_position, cover_edition_id, media_kinds, acquire_status, \
                    edition_count, holding_count, added_at, sort_series, length_minutes, \
                    purchased_at, refreshed_at\
                 ) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM works WHERE id = ?)",
                insert,
            ),
            batch::exec(
                "UPDATE library_work_rows SET kind = ?, title = ?, sort_title = ?, \
                    authors = ?, sort_author = ?, series = ?, series_position = ?, \
                    cover_edition_id = ?, media_kinds = ?, acquire_status = ?, \
                    edition_count = ?, holding_count = ?, added_at = ?, sort_series = ?, \
                    length_minutes = ?, purchased_at = ?, refreshed_at = ? \
                 WHERE work_id = ? AND refreshed_at <= ?",
                update,
            ),
            batch::exec(
                &format!("DELETE FROM library_work_facets WHERE work_id = ? AND {FENCE}"),
                vec![
                    batch::text(work_id),
                    batch::text(work_id),
                    batch::text(refreshed_at),
                ],
            ),
        ];
        let mut batches = Vec::new();
        let mut pending = WriteBatches::new(self.db_capabilities(), "library index");
        batches.extend(pending.push_group(row_statements)?);
        for (facet, key, value) in facets {
            batches.extend(pending.push(batch::exec(
                &format!(
                    "INSERT OR IGNORE INTO library_work_facets (work_id, facet, key, value) \
                     SELECT ?, ?, ?, ? WHERE {FENCE}"
                ),
                vec![
                    batch::text(work_id),
                    batch::text(facet.as_str()),
                    batch::text(key),
                    batch::text(value),
                    batch::text(work_id),
                    batch::text(refreshed_at),
                ],
            ))?);
        }
        batches.extend(pending.finish());
        let mut changed = 0;
        for (n, statements) in batches.into_iter().enumerate() {
            let reply = self
                .execute_host_batch(batch::request(&operation_id(n), statements))
                .await?;
            if n == 0 {
                // The row's `UPDATE` is the batch's second statement.
                changed = batch::rows_affected(&reply, 1).min(1);
                if changed == 0 {
                    // A later refresh owns the row and its values.
                    break;
                }
            }
        }
        Ok(changed)
    }

    /// Refreshes one page of the rebuild: the works with an id after
    /// `after`, in id order, as [`LibraryStore::refresh_library_works`]
    /// does.
    ///
    /// # Arguments
    ///
    /// * `after` - The last work id of the previous page; `None` to start.
    ///
    /// # Returns
    ///
    /// The last work id of this page, or `None` when the walk is done.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] or a batch error when a read or write
    /// fails.
    pub async fn rebuild_library_index_page(&self, after: Option<&str>) -> Result<Option<String>> {
        let ids = self.top_level_work_ids_page(after, REBUILD_PAGE).await?;
        let Some(last) = ids.last().cloned() else {
            // Rows of works past the last id (none can exist: rows cascade
            // with their work) need no sweep.
            return Ok(None);
        };
        self.refresh_library_works(&ids).await?;
        Ok(Some(last))
    }

    /// One page of top-level work ids (podcast shows, not their episodes)
    /// in id order, for a walk over every work.
    ///
    /// # Arguments
    ///
    /// * `after` - The last id of the previous page; `None` to start.
    /// * `limit` - Most ids to return, further capped at the database's
    ///   result-row limit.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn top_level_work_ids_page(
        &self,
        after: Option<&str>,
        limit: u64,
    ) -> Result<Vec<String>> {
        let mut query = works::Entity::find()
            .filter(works::Column::ParentWorkId.is_null())
            .order_by_asc(works::Column::Id)
            .limit(limit.max(1).min(self.result_row_cap()));
        if let Some(after) = after {
            query = query.filter(works::Column::Id.gt(after));
        }
        Ok(query
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|w| w.id)
            .collect())
    }

    /// True when the page index has a finished build of
    /// [`LIBRARY_INDEX_LAYOUT`].
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn library_index_built(&self) -> Result<bool> {
        Ok(
            library_index_builds::Entity::find_by_id(LIBRARY_INDEX_LAYOUT)
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?
                .is_some(),
        )
    }

    /// Records a finished build of [`LIBRARY_INDEX_LAYOUT`].
    ///
    /// # Errors
    ///
    /// Returns a batch error when the write fails.
    pub async fn finish_library_index_build(&self) -> Result<()> {
        let operation_id = format!("library-index-build-{}", Uuid::new_v4());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![batch::exec(
                "INSERT OR IGNORE INTO library_index_builds (layout, finished_at) VALUES (?, ?)",
                vec![
                    batch::int(LIBRARY_INDEX_LAYOUT),
                    batch::text(&stamp(Utc::now())),
                ],
            )],
        ))
        .await?;
        Ok(())
    }

    /// The works an edition belongs to (one, unless it was just moved).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn work_of_edition(&self, edition_id: &str) -> Result<Option<String>> {
        let Some(edition) = editions::Entity::find_by_id(edition_id.to_string())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(expressions::Entity::find_by_id(edition.expression_id)
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(|x| x.work_id))
    }

    /// The work a listing's edition belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn work_of_listing(&self, listing_id: &str) -> Result<Option<String>> {
        let Some(listing) = store_listings::Entity::find_by_id(listing_id.to_string())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        self.work_of_edition(&listing.edition_id).await
    }

    /// The rows of `ids` that pass `query`'s filters, in the order of
    /// `ids` (a search's relevance order); `query`'s order, cursor and
    /// limit are not used. Unheld works and ids with no row are left out.
    ///
    /// # Arguments
    ///
    /// * `ids` - Top-level work ids.
    /// * `query` - The filters to apply.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn library_works_by_ids(
        &self,
        ids: &[String],
        query: &WorkPageQuery,
    ) -> Result<Vec<LibraryWorkRow>> {
        use library_work_rows::Column as C;
        let mut found: HashMap<String, LibraryWorkRow> =
            find_chunked_paged::<library_work_rows::Entity, _>(self, ids, |part| {
                filtered_rows(query)
                    .filter(C::WorkId.is_in(part.iter().cloned()))
                    .order_by_asc(C::WorkId)
            })
            .await?
            .into_iter()
            .filter_map(LibraryWorkRow::from_model)
            .map(|row| (row.work_id.clone(), row))
            .collect();
        Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// One page of the values a facet has across the held works, by key.
    ///
    /// # Arguments
    ///
    /// * `of` - The facet, or the user whose tags to list.
    /// * `after` - The `next` key of the previous page; `None` to start.
    /// * `limit` - Most values to return; at least 1, further capped at the
    ///   database's result-row limit.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn library_facet_values(
        &self,
        of: FacetValuesOf,
        after: Option<&str>,
        limit: u64,
    ) -> Result<FacetValuesPage> {
        /// One group of the read, decoded by column name.
        #[derive(Debug, sea_orm::FromQueryResult)]
        struct Group {
            /// The value's key.
            key: String,
            /// One spelling of the value.
            value: String,
            /// Works in the group.
            works: i64,
        }
        let cap = self.result_row_cap();
        let limit = limit.max(1).min(cap);
        let probe = limit.saturating_add(1).min(cap);
        let held = || {
            Query::select()
                .column(library_work_rows::Column::WorkId)
                .from(library_work_rows::Entity)
                .and_where(library_work_rows::Column::HoldingCount.gt(0))
                .to_owned()
        };
        let mut groups = match of {
            FacetValuesOf::Facet(facet) => {
                use library_work_facets::Column as F;
                let mut select = library_work_facets::Entity::find()
                    .select_only()
                    .column(F::Key)
                    .column_as(F::Value.min(), "value")
                    .column_as(F::WorkId.count(), "works")
                    .filter(F::Facet.eq(facet.as_str()));
                if let Some(after) = after {
                    select = select.filter(F::Key.gt(after));
                }
                select
                    .group_by(F::Key)
                    .order_by_asc(F::Key)
                    .limit(probe)
                    .into_model::<Group>()
                    .all(self.db())
                    .await
            }
            FacetValuesOf::Tags(user_id) => {
                use user_work_tags::Column as T;
                let mut select = user_work_tags::Entity::find()
                    .select_only()
                    .column_as(T::TagKey, "key")
                    .column_as(T::Tag.min(), "value")
                    .column_as(T::WorkId.count(), "works")
                    .filter(T::UserId.eq(user_id))
                    .filter(T::WorkId.in_subquery(held()));
                if let Some(after) = after {
                    select = select.filter(T::TagKey.gt(after));
                }
                select
                    .group_by(T::TagKey)
                    .order_by_asc(T::TagKey)
                    .limit(probe)
                    .into_model::<Group>()
                    .all(self.db())
                    .await
            }
        }
        .map_err(LibraryError::Orm)?;
        let fetched = u64::try_from(groups.len()).unwrap_or(u64::MAX);
        let more = fetched > limit || (probe == limit && fetched == limit);
        groups.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more {
            groups.last().map(|g| g.key.clone())
        } else {
            None
        };
        Ok(FacetValuesPage {
            values: groups
                .into_iter()
                .map(|g| FacetValue {
                    key: g.key,
                    value: g.value,
                    works: count(g.works),
                })
                .collect(),
            next,
        })
    }

    /// One page of the library's works from the page index.
    ///
    /// # Arguments
    ///
    /// * `query` - Order, filters, cursor and page size.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] when the cursor belongs to another
    /// order, or [`LibraryError::Orm`] when the read fails.
    pub async fn library_works_page(&self, query: &WorkPageQuery) -> Result<WorkPage> {
        use library_work_rows::Column as C;
        let cap = self.result_row_cap();
        let limit = query.limit.max(1).min(cap);
        // One row past the page tells whether another follows; an adapter
        // that answers fewer rows than that gets a cursor after every full
        // page (the last one may come back empty).
        let probe = limit.saturating_add(1).min(cap);
        let mut select = filtered_rows(query);
        if let Some(after) = &query.after {
            if after.sort != query.sort {
                return Err(LibraryError::Invalid(
                    "page cursor belongs to another sort order".into(),
                ));
            }
            select = select.filter(match query.sort {
                WorkSort::Title => Condition::any().add(C::SortTitle.gt(&after.key)).add(
                    Condition::all()
                        .add(C::SortTitle.eq(&after.key))
                        .add(C::WorkId.gt(&after.work_id)),
                ),
                WorkSort::Author => {
                    let title = after.title.clone().unwrap_or_default();
                    Condition::any()
                        .add(C::SortAuthor.gt(&after.key))
                        .add(
                            Condition::all()
                                .add(C::SortAuthor.eq(&after.key))
                                .add(C::SortTitle.gt(&title)),
                        )
                        .add(
                            Condition::all()
                                .add(C::SortAuthor.eq(&after.key))
                                .add(C::SortTitle.eq(&title))
                                .add(C::WorkId.gt(&after.work_id)),
                        )
                }
                WorkSort::Added => Condition::any().add(C::AddedAt.lt(&after.key)).add(
                    Condition::all()
                        .add(C::AddedAt.eq(&after.key))
                        .add(C::WorkId.lt(&after.work_id)),
                ),
                WorkSort::Series => {
                    let title = after.title.clone().unwrap_or_default();
                    Condition::any()
                        .add(C::SortSeries.gt(&after.key))
                        .add(
                            Condition::all()
                                .add(C::SortSeries.eq(&after.key))
                                .add(C::SortTitle.gt(&title)),
                        )
                        .add(
                            Condition::all()
                                .add(C::SortSeries.eq(&after.key))
                                .add(C::SortTitle.eq(&title))
                                .add(C::WorkId.gt(&after.work_id)),
                        )
                }
                WorkSort::Purchased => Condition::any().add(C::PurchasedAt.lt(&after.key)).add(
                    Condition::all()
                        .add(C::PurchasedAt.eq(&after.key))
                        .add(C::WorkId.lt(&after.work_id)),
                ),
                WorkSort::Length => {
                    let minutes: i64 = after
                        .key
                        .parse()
                        .map_err(|_| LibraryError::Invalid("invalid page cursor".into()))?;
                    Condition::any().add(C::LengthMinutes.lt(minutes)).add(
                        Condition::all()
                            .add(C::LengthMinutes.eq(minutes))
                            .add(C::WorkId.lt(&after.work_id)),
                    )
                }
            });
        }
        select = match query.sort {
            WorkSort::Title => select.order_by_asc(C::SortTitle).order_by_asc(C::WorkId),
            WorkSort::Author => select
                .order_by_asc(C::SortAuthor)
                .order_by_asc(C::SortTitle)
                .order_by_asc(C::WorkId),
            WorkSort::Added => select.order_by_desc(C::AddedAt).order_by_desc(C::WorkId),
            WorkSort::Series => select
                .order_by_asc(C::SortSeries)
                .order_by_asc(C::SortTitle)
                .order_by_asc(C::WorkId),
            WorkSort::Purchased => select
                .order_by_desc(C::PurchasedAt)
                .order_by_desc(C::WorkId),
            WorkSort::Length => select
                .order_by_desc(C::LengthMinutes)
                .order_by_desc(C::WorkId),
        };
        let mut models = select
            .limit(probe)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        let fetched = u64::try_from(models.len()).unwrap_or(u64::MAX);
        let more = fetched > limit || (probe == limit && fetched == limit);
        models.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more {
            models.last().map(|last| WorkCursor {
                sort: query.sort,
                key: match query.sort {
                    WorkSort::Title => last.sort_title.clone(),
                    WorkSort::Author => last.sort_author.clone(),
                    WorkSort::Added => last.added_at.clone(),
                    WorkSort::Series => last.sort_series.clone(),
                    WorkSort::Purchased => last.purchased_at.clone(),
                    WorkSort::Length => last.length_minutes.to_string(),
                },
                title: matches!(query.sort, WorkSort::Author | WorkSort::Series)
                    .then(|| last.sort_title.clone()),
                work_id: last.work_id.clone(),
            })
        } else {
            None
        };
        Ok(WorkPage {
            works: models
                .into_iter()
                .filter_map(LibraryWorkRow::from_model)
                .collect(),
            next,
        })
    }
}

/// The rows a page of `query` reads from: held works that pass its media
/// kind, kind, status and account filters (not its order or cursor).
fn filtered_rows(query: &WorkPageQuery) -> sea_orm::Select<library_work_rows::Entity> {
    use library_work_rows::Column as C;
    let mut select = library_work_rows::Entity::find().filter(C::HoldingCount.gt(0));
    if let Some(kind) = query.media_kind {
        let kind = kind.as_str();
        select = select.filter(
            Condition::any()
                .add(C::MediaKinds.eq(kind))
                .add(C::MediaKinds.like(format!("{kind},%")))
                .add(C::MediaKinds.like(format!("%,{kind}")))
                .add(C::MediaKinds.like(format!("%,{kind},%"))),
        );
    }
    if let Some(kind) = query.kind {
        select = select.filter(C::Kind.eq(kind.as_str()));
    }
    if let Some(status) = query.status {
        select = select.filter(C::AcquireStatus.eq(status.as_str()));
    }
    if let Some(account) = &query.account {
        select = select.filter(C::WorkId.in_subquery(account_works_subquery(account)));
    }
    if let Some(filter) = &query.filter {
        select = select.filter(filter_condition(filter));
    }
    select
}

/// The condition a [`WorkFilter`] puts on `library_work_rows`.
fn filter_condition(filter: &WorkFilter) -> Condition {
    use library_work_rows::Column as C;
    match filter {
        WorkFilter::Facet { facet, value } => {
            let key = facet_key(value);
            let mut works = Query::select()
                .column(library_work_facets::Column::WorkId)
                .from(library_work_facets::Entity)
                .and_where(library_work_facets::Column::Facet.eq(facet.as_str()))
                .to_owned();
            if *facet == WorkFacet::Series && key.is_empty() {
                // In no series: no series value at all.
                Condition::all().add(C::WorkId.not_in_subquery(works))
            } else {
                works.and_where(library_work_facets::Column::Key.eq(key));
                Condition::all().add(C::WorkId.in_subquery(works))
            }
        }
        WorkFilter::Tag { user_id, tag } => match user_id {
            Some(user_id) => Condition::all().add(
                C::WorkId.in_subquery(
                    Query::select()
                        .column(user_work_tags::Column::WorkId)
                        .from(user_work_tags::Entity)
                        .and_where(user_work_tags::Column::UserId.eq(*user_id))
                        .and_where(user_work_tags::Column::TagKey.eq(facet_key(tag)))
                        .to_owned(),
                ),
            ),
            // No user, no tags: the work id is never null, so nothing matches.
            None => Condition::all().add(C::WorkId.is_null()),
        },
        WorkFilter::Finished { user_id, finished } => {
            let Some(user_id) = user_id else {
                return if *finished {
                    Condition::all().add(C::WorkId.is_null())
                } else {
                    Condition::all()
                };
            };
            let done = Query::select()
                .column(user_title_state::Column::WorkId)
                .from(user_title_state::Entity)
                .and_where(user_title_state::Column::UserId.eq(*user_id))
                .and_where(user_title_state::Column::IsFinished.ne(0))
                .to_owned();
            if *finished {
                Condition::all().add(C::WorkId.in_subquery(done))
            } else {
                Condition::all().add(C::WorkId.not_in_subquery(done))
            }
        }
    }
}

/// The top-level works an account id holds a listing of: the works of the
/// held editions, and the shows of held episodes.
fn account_works_subquery(account: &str) -> sea_orm::sea_query::SelectStatement {
    use sea_orm::sea_query::ExprTrait as _;
    Query::select()
        .expr(Expr::cust(
            "CASE WHEN works.parent_work_id IS NULL THEN works.id ELSE works.parent_work_id END",
        ))
        .from(holdings::Entity)
        .inner_join(
            store_listings::Entity,
            Expr::col((store_listings::Entity, store_listings::Column::Id))
                .equals((holdings::Entity, holdings::Column::ListingId)),
        )
        .inner_join(
            editions::Entity,
            Expr::col((editions::Entity, editions::Column::Id))
                .equals((store_listings::Entity, store_listings::Column::EditionId)),
        )
        .inner_join(
            expressions::Entity,
            Expr::col((expressions::Entity, expressions::Column::Id))
                .equals((editions::Entity, editions::Column::ExpressionId)),
        )
        .inner_join(
            works::Entity,
            Expr::col((works::Entity, works::Column::Id))
                .equals((expressions::Entity, expressions::Column::WorkId)),
        )
        .and_where(
            Expr::col((holdings::Entity, holdings::Column::AccountId)).eq(Expr::val(account)),
        )
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_titles_drop_one_leading_article() {
        assert_eq!(sort_title("The Hobbit"), "hobbit");
        assert_eq!(sort_title("A Wizard of Earthsea"), "wizard of earthsea");
        assert_eq!(
            sort_title("An Instance of the Fingerpost"),
            "instance of the fingerpost"
        );
        assert_eq!(sort_title("The"), "the");
        assert_eq!(sort_title("Theory of Everything"), "theory of everything");
    }

    #[test]
    fn cursors_round_trip_and_reject_garbage() {
        let cursor = WorkCursor {
            sort: WorkSort::Author,
            key: "le guin, ursula".into(),
            title: Some("left hand of darkness".into()),
            work_id: "w1".into(),
        };
        assert_eq!(WorkCursor::decode(&cursor.encode()).unwrap(), cursor);
        assert!(WorkCursor::decode("not a cursor!").is_err());
        assert!(
            WorkCursor::decode("e30").is_err(),
            "an empty object is not a cursor"
        );
    }

    fn work(id: &str) -> works::Model {
        works::Model {
            id: id.into(),
            kind: "book".into(),
            title: "The Dispossessed".into(),
            match_key: String::new(),
            parent_work_id: None,
            description: None,
            subjects: None,
            categories: None,
            cover_url: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn edition(id: &str, media: &str, cover: bool) -> editions::Model {
        editions::Model {
            id: id.into(),
            expression_id: "x1".into(),
            media_kind: media.into(),
            title: "The Dispossessed".into(),
            subtitle: None,
            published_at: None,
            duration_minutes: None,
            cover_url: cover.then(|| format!("https://img.example/{id}.jpg")),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn holding(id: &str, listing: &str, at: &str) -> holdings::Model {
        holdings::Model {
            id: id.into(),
            listing_id: listing.into(),
            holder: format!("account:{id}"),
            holder_kind: "account".into(),
            plugin_key: Some("p".into()),
            account_id: Some(id.into()),
            node_id: None,
            entitlement: "purchase".into(),
            purchased_at: None,
            seen_pass: None,
            share_ref: None,
            share_title: None,
            share_digest: None,
            delivered_digest: None,
            delivered_at: None,
            created_at: at.into(),
            updated_at: at.into(),
        }
    }

    fn acquisition(edition: &str, status: AcquireStatus) -> edition_acquisitions::Model {
        edition_acquisitions::Model {
            edition_id: edition.into(),
            status: status.as_str().into(),
            storage_key: None,
            error_message: None,
            title_uuid: "t".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn a_work_without_holdings_keeps_an_unheld_row() {
        let inputs = WorkRowInputs {
            work: Some(work("w1")),
            editions: vec![edition("e1", "audio", true)],
            listing_editions: HashMap::from([("l1".into(), "e1".into())]),
            ..WorkRowInputs::default()
        };
        let row = build_work_row(&inputs, "now").expect("an unheld work keeps its fence row");
        assert_eq!(row.holding_count, 0);
        assert_eq!(row.added_at, work("w1").created_at);
        assert_eq!(build_work_row(&WorkRowInputs::default(), "now"), None);
    }

    #[test]
    fn a_row_sums_the_works_editions_and_holdings() {
        let inputs = WorkRowInputs {
            work: Some(work("w1")),
            editions: vec![
                edition("e1", "ebook", true),
                edition("e2", "audio", true),
                edition("e3", "audio", false),
            ],
            listing_editions: HashMap::from([
                ("l1".into(), "e1".into()),
                ("l2".into(), "e2".into()),
                ("l3".into(), "e3".into()),
            ]),
            listings: Vec::new(),
            holdings: vec![
                holding("h1", "l1", "2026-02-01T00:00:00Z"),
                holding("h2", "l2", "2026-03-01T00:00:00Z"),
                holding("h3", "gone", "2026-04-01T00:00:00Z"),
            ],
            authors: vec!["Ursula K. Le Guin".into()],
            first_author_sort: Some("Le Guin, Ursula K.".into()),
            series: Some(("Hainish Cycle".into(), Some("6".into()))),
            acquisitions: vec![
                acquisition("e1", AcquireStatus::Error),
                acquisition("e2", AcquireStatus::Queued),
            ],
        };
        let row = build_work_row(&inputs, "now").unwrap();
        assert_eq!(row.sort_title, "dispossessed");
        assert_eq!(row.media_kinds, "audio,ebook");
        assert_eq!(
            row.acquire_status, "queued",
            "queued is further than a failure"
        );
        assert_eq!(row.edition_count, 3);
        assert_eq!(
            row.holding_count, 2,
            "a holding of an unknown listing is not counted"
        );
        assert_eq!(row.added_at, "2026-03-01T00:00:00Z");
        assert_eq!(
            row.cover_edition_id.as_deref(),
            Some("e2"),
            "held audio with a cover"
        );
        assert_eq!(row.authors.as_deref(), Some("Ursula K. Le Guin"));
        assert_eq!(row.sort_author, "le guin, ursula k.");
        assert_eq!(row.series.as_deref(), Some("Hainish Cycle"));
        assert_eq!(row.series_position.as_deref(), Some("6"));
        assert_eq!(row.refreshed_at, "now");
    }

    #[test]
    fn an_acquired_edition_wins_the_cover() {
        let inputs = WorkRowInputs {
            work: Some(work("w1")),
            editions: vec![edition("e1", "ebook", true), edition("e2", "audio", true)],
            listing_editions: HashMap::from([
                ("l1".into(), "e1".into()),
                ("l2".into(), "e2".into()),
            ]),
            holdings: vec![
                holding("h1", "l1", "2026-02-01T00:00:00Z"),
                holding("h2", "l2", "2026-02-01T00:00:00Z"),
            ],
            acquisitions: vec![acquisition("e1", AcquireStatus::Acquired)],
            ..WorkRowInputs::default()
        };
        let row = build_work_row(&inputs, "now").unwrap();
        assert_eq!(row.cover_edition_id.as_deref(), Some("e1"));
        assert_eq!(row.acquire_status, "acquired");
        assert_eq!(row.authors, None);
        assert_eq!(row.sort_author, "");
    }

    #[test]
    fn stored_rows_read_back_typed() {
        let model = library_work_rows::Model {
            work_id: "w1".into(),
            kind: "podcast".into(),
            title: "T".into(),
            sort_title: "t".into(),
            authors: None,
            sort_author: String::new(),
            series: None,
            series_position: None,
            cover_edition_id: None,
            media_kinds: "audio,unknown".into(),
            acquire_status: "downloading".into(),
            edition_count: 2,
            holding_count: -1,
            added_at: "a".into(),
            sort_series: "1".into(),
            length_minutes: 0,
            purchased_at: String::new(),
            refreshed_at: "r".into(),
        };
        let row = LibraryWorkRow::from_model(model.clone()).unwrap();
        assert_eq!(row.kind, WorkKind::Podcast);
        assert_eq!(row.media_kinds, vec![MediaKind::Audio]);
        assert_eq!(row.acquire_status, AcquireStatus::Downloading);
        assert_eq!(row.holding_count, 0);
        let bad = library_work_rows::Model {
            kind: "film".into(),
            ..model
        };
        assert_eq!(LibraryWorkRow::from_model(bad), None);
    }
}

#[cfg(test)]
#[path = "library_index_tests.rs"]
mod store_tests;
