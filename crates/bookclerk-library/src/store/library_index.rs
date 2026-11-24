//! The library's page index (`library_work_rows`): one row per held
//! top-level work.
//!
//! List views page this one table instead of joining works, expressions,
//! editions, listings and holdings for every request
//! (`docs/adr/library-data-model.md`, decision 8 and *Consequences*). A
//! row is derived data: [`LibraryStore::refresh_library_works`] reads a
//! work's model and writes its row (a work with no holding left keeps a
//! row with a holding count of 0, which pages skip, as the refresh fence),
//! or deletes it once the work is gone. The daemon's library-wide `host:library-index` consumer
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
//! authors, narrators, series, genres, publishers, sources, library tags
//! and collections), fenced by the row's stamp, so a page can filter by one value
//! without joining the model. A person's own tags and progress filter
//! through `user_work_tags` and `user_title_state` instead.

use std::collections::{BTreeSet, HashMap, HashSet};

use bookclerk_resolve::{MediaKind, WorkKind};
use chrono::Utc;
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::library_model::find_chunked_paged;
use super::work_search::WorkNames;
use super::write_batches::{CountedBatches, WriteBatches};
use super::LibraryStore;
use crate::control_plane::batch;
use crate::entities::{
    account_links, contributors, credits, edition_acquisitions, editions, expressions, holdings,
    library_index_builds, library_work_facets, library_work_rows, patron_scope_keys,
    portal_identities, series, series_entries, store_listings, user_title_state, user_work_tags,
    works,
};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;

/// Layout of `library_work_rows` this binary writes. A library with no
/// finished build of this layout is rebuilt; bump it when a change to how
/// rows are derived needs every row rewritten.
///
/// - 2: every work with an edition names a cover edition, whose cover may
///   come from a store listing or the work, so libraries indexed under 1
///   (no cover edition unless the edition row had a cover) are rebuilt.
/// - 3: rows count the held editions an acquire would still queue
///   (`acquirable_editions`).
pub const LIBRARY_INDEX_LAYOUT: i64 = 3;

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
    /// Library tags (`work_tags`): set by an Administrator or read from a
    /// sidecar, the same for everyone.
    Tag,
    /// Collections (`core.collections`) one of the work's files is in.
    Collection,
    /// Whether a store marks one of the work's listings explicit (adult)
    /// content: the single value `explicit`, which a patron scope that
    /// hides explicit books excludes. Not offered as a Library filter.
    Explicit,
}

impl WorkFacet {
    /// Every facet, in the order a work's values are written.
    pub const ALL: [Self; 9] = [
        Self::Author,
        Self::Narrator,
        Self::Series,
        Self::Genre,
        Self::Publisher,
        Self::Source,
        Self::Tag,
        Self::Collection,
        Self::Explicit,
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
            Self::Tag => "tag",
            Self::Collection => "collection",
            Self::Explicit => "explicit",
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
    /// Only works inside this patron scope; `None` for a caller who sees
    /// the whole library.
    pub scope: Option<WorkScope>,
    /// Continue after this cursor.
    pub after: Option<WorkCursor>,
    /// Most rows to return; at least 1.
    pub limit: u64,
}

/// What a catalog item the library does not hold says about itself, read
/// by [`WorkScope::admits_unheld`]. People are comma- or
/// semicolon-separated, genres as the page index splits them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnheldItem<'a> {
    /// The item's authors, as one comma-separated string.
    pub authors: Option<&'a str>,
    /// The item's narrators, as one comma-separated string.
    pub narrators: Option<&'a str>,
    /// The series name, without its position.
    pub series: Option<&'a str>,
    /// Genre labels, separated by `,`, `;` or `|`.
    pub genres: Option<&'a str>,
    /// Whether a store marks the item explicit (adult) content; `None`
    /// when no store said.
    pub is_explicit: Option<bool>,
}

impl UnheldItem<'_> {
    /// The item's `(facet, key)` pairs, keyed as the page index keys a
    /// held work's values.
    fn facet_keys(&self) -> Vec<(WorkFacet, String)> {
        let people = |text: Option<&str>| -> Vec<String> {
            text.unwrap_or_default()
                .split([',', ';'])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(facet_key)
                .collect()
        };
        let mut keys: Vec<(WorkFacet, String)> = Vec::new();
        keys.extend(
            people(self.authors)
                .into_iter()
                .map(|k| (WorkFacet::Author, k)),
        );
        keys.extend(
            people(self.narrators)
                .into_iter()
                .map(|k| (WorkFacet::Narrator, k)),
        );
        if let Some(series) = self.series.map(str::trim).filter(|s| !s.is_empty()) {
            keys.push((WorkFacet::Series, facet_key(series)));
        }
        keys.extend(
            split_genres(self.genres.unwrap_or_default()).map(|g| (WorkFacet::Genre, facet_key(g))),
        );
        if self.is_explicit == Some(true) {
            keys.push((
                WorkFacet::Explicit,
                crate::control_plane::EXPLICIT_FACET_KEY.to_string(),
            ));
        }
        keys
    }
}

/// The part of the library a patron may see, as the page index applies it.
///
/// A work passes when one of the patron's own store accounts holds it, or
/// when it matches no exclude value and, if there are include values,
/// matches at least one of them. Values are [`facet_key`]s of the page
/// index's facets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkScope {
    /// `(facet, key)` pairs a work must match one of; empty lets every
    /// work through.
    pub include: Vec<(WorkFacet, String)>,
    /// `(facet, key)` pairs that hide a work, whatever `include` says.
    pub exclude: Vec<(WorkFacet, String)>,
    /// The patron (`users.id`) whose linked store accounts' books always
    /// pass; `None` for a caller with no accounts of their own.
    pub owner: Option<i64>,
}

impl WorkScope {
    /// True when the scope hides nothing.
    #[must_use]
    pub fn sees_everything(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// True when a catalog item the library does not hold may be shown to
    /// the patron: Discover shelves and storefront search results.
    ///
    /// The item's own metadata stands in for the page index's facets, with
    /// the same rule: no exclude value matches and, when there are include
    /// values, one of them does. An item carries no library tag or
    /// collection, so a scope that includes only by those admits no
    /// catalog item, and excludes by them never hide one.
    ///
    /// # Arguments
    ///
    /// * `item` - What the store listing says about the item.
    #[must_use]
    pub fn admits_unheld(&self, item: &UnheldItem<'_>) -> bool {
        let keys = item.facet_keys();
        let matches = |pairs: &[(WorkFacet, String)]| {
            pairs
                .iter()
                .any(|(facet, key)| keys.iter().any(|(f, k)| f == facet && k == key))
        };
        !matches(&self.exclude) && (self.include.is_empty() || matches(&self.include))
    }

    /// The scope's values as `(effect, facet, key)` rows, sorted and
    /// without duplicates, as `patron_scope_keys` stores them.
    fn key_rows(&self) -> BTreeSet<(&'static str, &'static str, &str)> {
        let include = self
            .include
            .iter()
            .map(|(facet, key)| (SCOPE_INCLUDE, facet.as_str(), key.as_str()));
        let exclude = self
            .exclude
            .iter()
            .map(|(facet, key)| (SCOPE_EXCLUDE, facet.as_str(), key.as_str()));
        include.chain(exclude).collect()
    }

    /// Content id of the scope's include and exclude values: lowercase hex
    /// SHA-256 over its `(effect, facet, key)` rows, sorted and without
    /// duplicates, each field length-prefixed, so two
    /// scopes share an id exactly when they hold the same values.
    #[must_use]
    pub fn key_set_id(&self) -> String {
        use sha2::{Digest as _, Sha256};
        let mut hash = Sha256::new();
        for (effect, facet, key) in self.key_rows() {
            for field in [effect, facet, key] {
                let len = u64::try_from(field.len()).unwrap_or(u64::MAX);
                hash.update(len.to_be_bytes());
                hash.update(field.as_bytes());
            }
        }
        hex::encode(hash.finalize())
    }
}

/// `patron_scope_keys.effect` of an include value.
const SCOPE_INCLUDE: &str = "include";

/// `patron_scope_keys.effect` of an exclude value.
const SCOPE_EXCLUDE: &str = "exclude";

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
    /// How many held editions an acquire would still queue (neither
    /// acquired, queued nor downloading); the list offers Acquire while it
    /// is above 0.
    pub acquirable_editions: u64,
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
            acquirable_editions: count(model.acquirable_editions),
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
    /// The work's library tags as written (`work_tags.value`), every
    /// source, in key order.
    pub tags: Vec<String>,
    /// The collections one of the work's edition files is in, in the
    /// collections' order.
    pub collections: Vec<String>,
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
    // The cover edition: held editions first, then one with a cover of its
    // own or a store's on one of its listings, then acquired, then audio,
    // then by id so the choice is stable. Every work with an edition gets
    // one, so a cover that arrives after this row was written (store text
    // and a title's enrichment are written after the link that refreshed
    // it) still shows: the API reads the edition's cover, else its work's,
    // when it serves the row (`edition_cover_urls`).
    let has_cover = |e: &editions::Model| {
        super::work_detail::edition_cover(
            std::iter::once(e.cover_url.as_deref()).chain(
                inputs
                    .listings
                    .iter()
                    .filter(|l| l.edition_id == e.id)
                    .map(|l| l.cover_url.as_deref()),
            ),
        )
        .is_some()
    };
    let cover_edition_id = inputs
        .editions
        .iter()
        .min_by_key(|e| {
            (
                !held_editions.contains(e.id.as_str()),
                !has_cover(e),
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
    // has one. A runtime of 0 or less is unknown, not a runtime.
    let length_minutes = inputs
        .editions
        .iter()
        .filter(|e| held_editions.contains(e.id.as_str()))
        .filter_map(|e| e.duration_minutes)
        .filter(|minutes| *minutes > 0)
        .max()
        .or_else(|| {
            inputs
                .listings
                .iter()
                .filter(|l| held_listings.contains(l.id.as_str()))
                .filter_map(|l| l.length_minutes)
                .filter(|minutes| *minutes > 0)
                .max()
        })
        .unwrap_or(0);
    let purchased_at = held
        .iter()
        .filter_map(|h| h.purchased_at.as_deref())
        .max()
        .unwrap_or_default();
    // Held editions an acquire by edition would still queue: never tried,
    // failed or reset, as opposed to acquired or on the way.
    let acquirable_editions = held_editions
        .iter()
        .filter(|id| {
            matches!(
                statuses
                    .get(**id)
                    .copied()
                    .unwrap_or(AcquireStatus::NotAcquired),
                AcquireStatus::NotAcquired | AcquireStatus::Error
            )
        })
        .count();
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
        facets_ready: 0,
        acquirable_editions: i64::try_from(acquirable_editions).unwrap_or(i64::MAX),
    })
}

/// One work's write in a refresh: its id, its row (`None` when the work is
/// gone, so its row is deleted) and its filter values.
pub(crate) type WorkRowWrite = (
    String,
    Option<library_work_rows::Model>,
    Vec<(WorkFacet, String, String)>,
);

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
        if listing.is_explicit == Some(1) {
            values.push((
                WorkFacet::Explicit,
                crate::control_plane::EXPLICIT_FACET_KEY,
            ));
        }
    }
    values.extend(inputs.tags.iter().map(|t| (WorkFacet::Tag, t.as_str())));
    values.extend(
        inputs
            .collections
            .iter()
            .map(|c| (WorkFacet::Collection, c.as_str())),
    );
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
    pub(super) fn result_row_cap(&self) -> u64 {
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

    /// Reads everything `work_id`'s row is derived from (empty when the
    /// work is gone).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    #[cfg(test)]
    pub(crate) async fn work_row_inputs(&self, work_id: &str) -> Result<WorkRowInputs> {
        Ok(self
            .work_models(&[work_id.to_string()])
            .await?
            .remove(work_id)
            .map(|model| model.inputs)
            .unwrap_or_default())
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
                &[credits::Column::Position, credits::Column::ContributorId],
                |part| {
                    credits::Entity::find()
                        .filter(credits::Column::TargetKind.eq(kind))
                        .filter(credits::Column::TargetId.is_in(part.iter().cloned()))
                        .filter(credits::Column::Role.eq(role))
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
                    &[contributors::Column::Id],
                    |part| {
                        contributors::Entity::find()
                            .filter(contributors::Column::Id.is_in(part.iter().cloned()))
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
        let tops = self.top_level_works(work_ids).await?;
        if tops.is_empty() {
            return Ok(0);
        }
        // Stamped before anything is read, so a refresh that read the works
        // later always wins.
        let refreshed_at = stamp(Utc::now());
        let mut models = self.work_models(&tops).await?;
        let mut writes = Vec::with_capacity(tops.len());
        for work_id in tops {
            let model = models.remove(&work_id).unwrap_or_default();
            let row = build_work_row(&model.inputs, &refreshed_at);
            let facets = match &row {
                Some(row) if row.holding_count > 0 => {
                    build_work_facets(&model.inputs, &model.names)
                }
                _ => Vec::new(),
            };
            writes.push((work_id, row, facets));
        }
        let changed = self.write_work_rows(&writes, &refreshed_at).await?;
        if changed > 0 {
            self.bump_live_topic(crate::control_plane::LiveTopic::Library)
                .await?;
        }
        Ok(changed)
    }

    /// Writes (or, for a work that is gone, deletes) each work's row and its
    /// filter values, unless a refresh that started later already wrote it.
    ///
    /// Each row, with the removal of its old values, goes in one atomic
    /// group; the rows and values of all the works share as few batches as
    /// the database's limits allow. Every value write checks that the row
    /// still carries this refresh's stamp, so once a later refresh has
    /// written a row, this one's values for it are dropped.
    ///
    /// # Arguments
    ///
    /// * `writes` - Each work's id, its row (`None` when the work is gone)
    ///   and its filter values.
    /// * `refreshed_at` - This refresh's stamp.
    ///
    /// # Returns
    ///
    /// How many rows were written or deleted.
    async fn write_work_rows(&self, writes: &[WorkRowWrite], refreshed_at: &str) -> Result<u64> {
        // Holds while the row carries this refresh's stamp.
        const FENCE: &str =
            "EXISTS (SELECT 1 FROM library_work_rows WHERE work_id = ? AND refreshed_at = ?)";
        let mut plan =
            CountedBatches::new(WriteBatches::new(self.db_capabilities(), "library index"));
        for (work_id, row, facets) in writes {
            let Some(row) = row else {
                // The work is gone; its values went with it.
                plan.push_group(
                    vec![batch::exec(
                        "DELETE FROM library_work_rows WHERE work_id = ? AND refreshed_at <= ?",
                        vec![batch::text(work_id), batch::text(refreshed_at)],
                    )],
                    0,
                )?;
                continue;
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
                    batch::int(row.acquirable_editions),
                    batch::text(&row.refreshed_at),
                ]
            };
            let mut insert = vec![batch::text(&row.work_id)];
            insert.extend(values());
            insert.push(batch::text(&row.work_id));
            let mut update = values();
            update.push(batch::text(&row.work_id));
            update.push(batch::text(refreshed_at));
            // The row's `UPDATE` (the group's second statement) counts it.
            plan.push_group(
                vec![
                    batch::exec(
                        "INSERT OR IGNORE INTO library_work_rows (\
                            work_id, kind, title, sort_title, authors, sort_author, series, \
                            series_position, cover_edition_id, media_kinds, acquire_status, \
                            edition_count, holding_count, added_at, sort_series, length_minutes, \
                            purchased_at, acquirable_editions, refreshed_at, facets_ready\
                         ) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0 \
                         WHERE EXISTS (SELECT 1 FROM works WHERE id = ?)",
                        insert,
                    ),
                    batch::exec(
                        "UPDATE library_work_rows SET kind = ?, title = ?, sort_title = ?, \
                            authors = ?, sort_author = ?, series = ?, series_position = ?, \
                            cover_edition_id = ?, media_kinds = ?, acquire_status = ?, \
                            edition_count = ?, holding_count = ?, added_at = ?, sort_series = ?, \
                            length_minutes = ?, purchased_at = ?, acquirable_editions = ?, \
                            refreshed_at = ?, facets_ready = 0 \
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
                ],
                1,
            )?;
            for (facet, key, value) in facets {
                plan.push(batch::exec(
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
                ))?;
            }
            // Batches run in order, so this lands after every facet above;
            // until it does, scoped patrons don't see the work.
            plan.push(batch::exec(
                "UPDATE library_work_rows SET facets_ready = 1 \
                 WHERE work_id = ? AND refreshed_at = ?",
                vec![batch::text(work_id), batch::text(refreshed_at)],
            ))?;
        }
        let mut changed = 0;
        for (statements, counted) in plan.finish() {
            let operation_id = format!("library-index-{}", Uuid::new_v4());
            let reply = self
                .execute_host_batch(batch::request(&operation_id, statements))
                .await?;
            for index in counted {
                changed += batch::rows_affected(&reply, index).min(1);
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

    /// Drops the finished build of [`LIBRARY_INDEX_LAYOUT`], so the next
    /// heartbeat queues a rebuild of every row: for a change, such as the
    /// library's collections, that the rows' derivation reads but no
    /// work's events carry.
    ///
    /// # Errors
    ///
    /// Returns a batch error when the write fails.
    pub async fn drop_library_index_build(&self) -> Result<()> {
        let operation_id = format!("library-index-drop-{}", Uuid::new_v4());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![batch::exec(
                "DELETE FROM library_index_builds WHERE layout = ?",
                vec![batch::int(LIBRARY_INDEX_LAYOUT)],
            )],
        ))
        .await?;
        Ok(())
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

    /// The top-level works of `edition_ids` that are held (their page index
    /// row counts a holding), in id order: the works whose rows a change to
    /// those editions' covers or text can show on the library page.
    ///
    /// A few chunked lookups for the whole set, so a large catalog answer
    /// that mostly holds unheld works costs no per-edition reads.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn held_works_of_editions(&self, edition_ids: &[String]) -> Result<Vec<String>> {
        let ids: Vec<String> = edition_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let editions =
            super::library_model::find_chunked::<editions::Entity, _>(self, &ids, |part| {
                editions::Entity::find().filter(editions::Column::Id.is_in(part.iter().cloned()))
            })
            .await?;
        let expression_ids: Vec<String> = editions
            .into_iter()
            .map(|e| e.expression_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let work_ids: Vec<String> = super::library_model::find_chunked::<expressions::Entity, _>(
            self,
            &expression_ids,
            |part| {
                expressions::Entity::find()
                    .filter(expressions::Column::Id.is_in(part.iter().cloned()))
            },
        )
        .await?
        .into_iter()
        .map(|x| x.work_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
        let tops = self.top_level_works(&work_ids).await?;
        let mut held: Vec<String> = super::library_model::find_chunked::<
            library_work_rows::Entity,
            _,
        >(self, &tops, |part| {
            library_work_rows::Entity::find()
                .filter(library_work_rows::Column::WorkId.is_in(part.iter().cloned()))
                .filter(library_work_rows::Column::HoldingCount.gt(0))
        })
        .await?
        .into_iter()
        .map(|row| row.work_id)
        .collect();
        held.sort();
        Ok(held)
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

    /// Writes the `patron_scope_keys` rows of `scope`'s value set when they
    /// are not all there yet, so the Library filter can join them.
    ///
    /// Rows are keyed by the set's content id and never change, so the
    /// write is idempotent: one `INSERT OR IGNORE` per value, grouped into
    /// as few batches as the database plugin's limits allow. A read finds
    /// the rows already there after the first use of a scope.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the count fails, or the write
    /// error.
    pub(crate) async fn ensure_scope_keys(&self, scope: Option<&WorkScope>) -> Result<()> {
        use sea_orm::PaginatorTrait as _;
        let Some(scope) = scope.filter(|scope| !scope.sees_everything()) else {
            return Ok(());
        };
        let key_set = scope.key_set_id();
        let rows = scope.key_rows();
        let stored = patron_scope_keys::Entity::find()
            .filter(patron_scope_keys::Column::ScopeHash.eq(key_set.as_str()))
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        if usize::try_from(stored).is_ok_and(|stored| stored >= rows.len()) {
            return Ok(());
        }
        let mut batches = WriteBatches::new(self.db_capabilities(), "patron scope key");
        let mut full = Vec::new();
        for (effect, facet, key) in rows {
            let statement = batch::exec(
                "INSERT OR IGNORE INTO patron_scope_keys (scope_hash, effect, facet, key) \
                 VALUES (?, ?, ?, ?)",
                vec![
                    batch::text(&key_set),
                    batch::text(effect),
                    batch::text(facet),
                    batch::text(key),
                ],
            );
            if let Some(done) = batches.push(statement)? {
                full.push(done);
            }
        }
        full.extend(batches.finish());
        for statements in full {
            let operation_id = format!("patron-scope-keys-{}", Uuid::new_v4().simple());
            self.execute_host_batch(batch::request(&operation_id, statements))
                .await?;
        }
        Ok(())
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
        self.ensure_scope_keys(query.scope.as_ref()).await?;
        let mut found: HashMap<String, LibraryWorkRow> =
            find_chunked_paged::<library_work_rows::Entity, _>(self, ids, &[C::WorkId], |part| {
                filtered_rows(query).filter(C::WorkId.is_in(part.iter().cloned()))
            })
            .await?
            .into_iter()
            .filter_map(LibraryWorkRow::from_model)
            .map(|row| (row.work_id.clone(), row))
            .collect();
        Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// Whether a work is inside a patron scope: its top-level work (the
    /// show, for an episode) is a held work of the page index that passes
    /// `scope`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn work_in_scope(&self, work_id: &str, scope: &WorkScope) -> Result<bool> {
        use library_work_rows::Column as C;
        self.ensure_scope_keys(Some(scope)).await?;
        let top = works::Entity::find_by_id(work_id.to_string())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .map_or_else(
                || work_id.to_string(),
                |work| work.parent_work_id.unwrap_or(work.id),
            );
        let query = WorkPageQuery {
            scope: Some(scope.clone()),
            ..WorkPageQuery::default()
        };
        Ok(filtered_rows(&query)
            .filter(C::WorkId.eq(top))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .is_some())
    }

    /// Whether an edition is inside a patron scope: no scope admits every
    /// edition, and a scope admits one whose work passes
    /// [`Self::work_in_scope`]. An edition that belongs to no work is
    /// outside every scope, so a scoped patron never learns of it.
    ///
    /// # Arguments
    ///
    /// * `edition_id` - The edition (`editions.id`).
    /// * `scope` - The patron's filter, `None` when nothing is hidden.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails; callers fail closed.
    pub async fn edition_in_scope(
        &self,
        edition_id: &str,
        scope: Option<&WorkScope>,
    ) -> Result<bool> {
        let Some(scope) = scope else {
            return Ok(true);
        };
        match self.work_of_edition(edition_id).await? {
            Some(work_id) => self.work_in_scope(&work_id, scope).await,
            None => Ok(false),
        }
    }

    /// One page of the values a facet has across the held works, by key.
    ///
    /// # Arguments
    ///
    /// * `of` - The facet, or the user whose tags to list.
    /// * `after` - The `next` key of the previous page; `None` to start.
    /// * `limit` - Most values to return; at least 1, further capped at the
    ///   database's result-row limit.
    /// * `scope` - Count only the works inside this patron scope; `None`
    ///   for a caller who sees the whole library.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn library_facet_values(
        &self,
        of: FacetValuesOf,
        after: Option<&str>,
        limit: u64,
        scope: Option<&WorkScope>,
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
        self.ensure_scope_keys(scope).await?;
        let cap = self.result_row_cap();
        let limit = limit.max(1).min(cap);
        let probe = limit.saturating_add(1).min(cap);
        let held = || {
            let mut held = Condition::all().add(library_work_rows::Column::HoldingCount.gt(0));
            if let Some(scope) = scope {
                held = held.add(scope_condition(scope));
            }
            Query::select()
                .column(library_work_rows::Column::WorkId)
                .from(library_work_rows::Entity)
                .cond_where(held)
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
                if scope.is_some_and(|scope| !scope.sees_everything()) {
                    select = select.filter(F::WorkId.in_subquery(held()));
                }
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
        self.ensure_scope_keys(query.scope.as_ref()).await?;
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

impl LibraryStore {
    /// One page of the library's works by position: `limit` rows from
    /// `offset` in `query`'s order (reversed when `reversed`), and how
    /// many rows the query matches in all. `query.after` and
    /// `query.limit` are not read.
    ///
    /// For callers that page by number, such as player apps; the library's
    /// own pages use [`Self::library_works_page`], whose cursors hold
    /// still while rows are added.
    ///
    /// # Arguments
    ///
    /// * `query` - Order and filters.
    /// * `offset` - Rows to skip.
    /// * `limit` - Most rows to return.
    /// * `reversed` - True to read the order backwards.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn library_works_at(
        &self,
        query: &WorkPageQuery,
        offset: u64,
        limit: u64,
        reversed: bool,
    ) -> Result<(Vec<LibraryWorkRow>, u64)> {
        use library_work_rows::Column as C;
        use sea_orm::{Order, PaginatorTrait as _};
        self.ensure_scope_keys(query.scope.as_ref()).await?;
        let total = filtered_rows(query)
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        if limit == 0 || offset >= total {
            return Ok((Vec::new(), total));
        }
        let (asc, desc) = if reversed {
            (Order::Desc, Order::Asc)
        } else {
            (Order::Asc, Order::Desc)
        };
        let columns: &[(C, &Order)] = match query.sort {
            WorkSort::Title => &[(C::SortTitle, &asc), (C::WorkId, &asc)],
            WorkSort::Author => &[
                (C::SortAuthor, &asc),
                (C::SortTitle, &asc),
                (C::WorkId, &asc),
            ],
            WorkSort::Added => &[(C::AddedAt, &desc), (C::WorkId, &desc)],
            WorkSort::Series => &[
                (C::SortSeries, &asc),
                (C::SortTitle, &asc),
                (C::WorkId, &asc),
            ],
            WorkSort::Purchased => &[(C::PurchasedAt, &desc), (C::WorkId, &desc)],
            WorkSort::Length => &[(C::LengthMinutes, &desc), (C::WorkId, &desc)],
        };
        let mut select = filtered_rows(query);
        for (column, order) in columns {
            select = select.order_by(*column, (*order).clone());
        }
        let page = u64::try_from(crate::control_plane::result_page(self)).unwrap_or(1);
        let models = crate::control_plane::read_shrinking(page, Some(limit), |at, n| {
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
            models
                .into_iter()
                .filter_map(LibraryWorkRow::from_model)
                .collect(),
            total,
        ))
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
    if let Some(scope) = &query.scope {
        select = select.filter(scope_condition(scope));
    }
    select
}

/// The condition a [`WorkScope`] puts on `library_work_rows`; empty (no
/// restriction) when the scope hides nothing.
fn scope_condition(scope: &WorkScope) -> Condition {
    use library_work_rows::Column as C;
    if scope.sees_everything() {
        return Condition::all();
    }
    let key_set = scope.key_set_id();
    // A row whose facets are still being replaced could slip past an
    // exclude rule, so a scope admits only rows with all their facets.
    let mut permitted = Condition::all().add(C::FacetsReady.eq(1));
    if !scope.include.is_empty() {
        permitted =
            permitted.add(C::WorkId.in_subquery(scope_works_subquery(&key_set, SCOPE_INCLUDE)));
    }
    if !scope.exclude.is_empty() {
        permitted =
            permitted.add(C::WorkId.not_in_subquery(scope_works_subquery(&key_set, SCOPE_EXCLUDE)));
    }
    match scope.owner {
        None => permitted,
        Some(user_id) => Condition::any()
            .add(permitted)
            .add(C::WorkId.in_subquery(owner_works_subquery(user_id))),
    }
}

/// The works with any of a stored scope's values of one effect: the facet
/// rows joined to `patron_scope_keys`, two binds whatever the rule count.
///
/// # Arguments
///
/// * `key_set` - [`WorkScope::key_set_id`] of a scope whose rows
///   [`LibraryStore::ensure_scope_keys`] wrote.
/// * `effect` - [`SCOPE_INCLUDE`] or [`SCOPE_EXCLUDE`].
fn scope_works_subquery(key_set: &str, effect: &str) -> sea_orm::sea_query::SelectStatement {
    use library_work_facets::Column as F;
    use patron_scope_keys::Column as K;
    use sea_orm::sea_query::ExprTrait as _;
    Query::select()
        .column((library_work_facets::Entity, F::WorkId))
        .from(library_work_facets::Entity)
        .inner_join(
            patron_scope_keys::Entity,
            Condition::all()
                .add(
                    Expr::col((patron_scope_keys::Entity, K::Facet))
                        .equals((library_work_facets::Entity, F::Facet)),
                )
                .add(
                    Expr::col((patron_scope_keys::Entity, K::Key))
                        .equals((library_work_facets::Entity, F::Key)),
                ),
        )
        .and_where(Expr::col((patron_scope_keys::Entity, K::ScopeHash)).eq(Expr::val(key_set)))
        .and_where(Expr::col((patron_scope_keys::Entity, K::Effect)).eq(Expr::val(effect)))
        .to_owned()
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
    held_works_subquery(
        Condition::all()
            .add(Expr::col((holdings::Entity, holdings::Column::AccountId)).eq(Expr::val(account))),
    )
}

/// The top-level works a store account linked to `user_id` holds a
/// listing of, through any of the user's portal identities: one bind
/// whatever the number of accounts.
fn owner_works_subquery(user_id: i64) -> sea_orm::sea_query::SelectStatement {
    use sea_orm::sea_query::ExprTrait as _;
    let mut select = held_works_subquery(
        Condition::all().add(
            Expr::col((portal_identities::Entity, portal_identities::Column::UserId))
                .eq(Expr::val(user_id)),
        ),
    );
    select
        .inner_join(
            account_links::Entity,
            Condition::all()
                .add(
                    Expr::col((account_links::Entity, account_links::Column::PluginKey))
                        .equals((holdings::Entity, holdings::Column::PluginKey)),
                )
                .add(
                    Expr::col((account_links::Entity, account_links::Column::AccountId))
                        .equals((holdings::Entity, holdings::Column::AccountId)),
                ),
        )
        .inner_join(
            portal_identities::Entity,
            Expr::col((portal_identities::Entity, portal_identities::Column::Id))
                .equals((account_links::Entity, account_links::Column::IdentityId)),
        );
    select
}

/// The top-level works of the holdings that pass `holding`: the works of
/// the held editions, and the shows of held episodes.
fn held_works_subquery(holding: Condition) -> sea_orm::sea_query::SelectStatement {
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
        .cond_where(holding)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog item's metadata reads as the page index keys a held
    /// work's: people split on commas, genres on `;`, keys case-folded,
    /// and only a store's explicit mark counts as explicit.
    #[test]
    fn a_catalog_item_is_judged_by_its_own_metadata() {
        let item = UnheldItem {
            authors: Some("Ann Writer, Bo Pen"),
            narrators: Some("Cy Voice"),
            series: Some(" The Long Road "),
            genres: Some("Fantasy; Children"),
            is_explicit: None,
        };
        let scope = |include: &[(WorkFacet, &str)], exclude: &[(WorkFacet, &str)]| WorkScope {
            include: include.iter().map(|(f, v)| (*f, facet_key(v))).collect(),
            exclude: exclude.iter().map(|(f, v)| (*f, facet_key(v))).collect(),
            owner: None,
        };
        assert!(WorkScope::default().admits_unheld(&item));
        assert!(scope(&[(WorkFacet::Genre, "CHILDREN")], &[]).admits_unheld(&item));
        assert!(scope(&[(WorkFacet::Series, "the long road")], &[]).admits_unheld(&item));
        assert!(!scope(&[(WorkFacet::Genre, "Mystery")], &[]).admits_unheld(&item));
        assert!(!scope(&[], &[(WorkFacet::Narrator, "cy voice")]).admits_unheld(&item));
        assert!(!scope(&[], &[(WorkFacet::Author, "Bo Pen")]).admits_unheld(&item));
        // Library tags and collections never match an item the library
        // does not hold.
        assert!(scope(&[], &[(WorkFacet::Tag, "kids")]).admits_unheld(&item));
        assert!(!scope(&[(WorkFacet::Collection, "Kids")], &[]).admits_unheld(&item));
        let hide = scope(&[], &[(WorkFacet::Explicit, "explicit")]);
        assert!(hide.admits_unheld(&item), "no flag stays visible");
        assert!(hide.admits_unheld(&UnheldItem {
            is_explicit: Some(false),
            ..item
        }));
        assert!(!hide.admits_unheld(&UnheldItem {
            is_explicit: Some(true),
            ..item
        }));
    }

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
            enrich_source: None,
            enrich_confidence: None,
            enrich_updated_at: None,
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
            user_id: None,
            entitlement: "purchase".into(),
            purchased_at: None,
            seen_pass: None,
            share_ref: None,
            share_title: None,
            share_digest: None,
            delivered_digest: None,
            delivered_at: None,
            refused_digest: None,
            refused_at: None,
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
            holding_id: "t".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            pdf_status: AcquireStatus::NotAcquired.as_str().into(),
            pdf_storage_key: None,
            pdf_holding_id: None,
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
            tags: Vec::new(),
            collections: Vec::new(),
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
        assert_eq!(
            row.acquirable_editions, 1,
            "the failed held edition can be acquired again; the queued one is on its way \
             and the unheld one is not the patron's"
        );
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
        assert_eq!(
            row.acquirable_editions, 1,
            "the held audio edition beside the acquired ebook is still to acquire"
        );
        assert_eq!(row.authors, None);
        assert_eq!(row.sort_author, "");
    }

    fn listing(id: &str, edition: &str, length: Option<i64>) -> store_listings::Model {
        store_listings::Model {
            id: id.into(),
            plugin_key: "p".into(),
            marketplace: "us".into(),
            product_id: id.into(),
            edition_id: edition.into(),
            title: "The Dispossessed".into(),
            categories: None,
            subtitle: None,
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_product_id: None,
            description: None,
            publisher: None,
            published_at: None,
            length_minutes: length,
            language: None,
            cover_url: None,
            url: None,
            rating_overall: None,
            rating_count: None,
            is_abridged: None,
            is_explicit: None,
            observed_at: "2026-01-01T00:00:00Z".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn a_listing_cover_makes_its_edition_the_cover_edition() {
        let mut covered = listing("l2", "e2", None);
        covered.cover_url = Some("https://img.example/e2.jpg".into());
        let mut blank = listing("l1", "e1", None);
        blank.cover_url = Some("  ".into());
        let inputs = WorkRowInputs {
            work: Some(work("w1")),
            editions: vec![edition("e1", "audio", false), edition("e2", "audio", false)],
            listing_editions: HashMap::from([
                ("l1".into(), "e1".into()),
                ("l2".into(), "e2".into()),
            ]),
            listings: vec![blank, covered],
            holdings: vec![
                holding("h1", "l1", "2026-02-01T00:00:00Z"),
                holding("h2", "l2", "2026-02-01T00:00:00Z"),
            ],
            ..WorkRowInputs::default()
        };
        let row = build_work_row(&inputs, "now").expect("row");
        assert_eq!(row.cover_edition_id.as_deref(), Some("e2"));
        let none = WorkRowInputs {
            listings: vec![listing("l1", "e1", None)],
            editions: vec![edition("e1", "audio", false)],
            ..inputs
        };
        assert_eq!(
            build_work_row(&none, "now")
                .expect("row")
                .cover_edition_id
                .as_deref(),
            // With no cover known, the best held edition is still named, so
            // a cover stored later (or the work's) shows without a refresh.
            Some("e1")
        );
    }

    #[test]
    fn a_zero_edition_runtime_falls_back_to_the_listings() {
        let mut zero = edition("e1", "audio", false);
        zero.duration_minutes = Some(0);
        let inputs = WorkRowInputs {
            work: Some(work("w1")),
            editions: vec![zero],
            listing_editions: HashMap::from([("l1".into(), "e1".into())]),
            listings: vec![listing("l1", "e1", Some(600))],
            holdings: vec![holding("h1", "l1", "2026-02-01T00:00:00Z")],
            ..WorkRowInputs::default()
        };
        assert_eq!(build_work_row(&inputs, "now").unwrap().length_minutes, 600);
        let unknown = WorkRowInputs {
            listings: vec![listing("l1", "e1", Some(0))],
            ..inputs
        };
        assert_eq!(build_work_row(&unknown, "now").unwrap().length_minutes, 0);
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
            facets_ready: 1,
            acquirable_editions: -1,
        };
        let row = LibraryWorkRow::from_model(model.clone()).unwrap();
        assert_eq!(row.acquirable_editions, 0, "a negative count reads as none");
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
