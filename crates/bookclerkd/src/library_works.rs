//! The library's works: `GET /api/library/works` pages the page index
//! (`library_work_rows`), `GET /api/library/works/{id}` reads one work
//! with its editions, store listings, holdings and the caller's own state,
//! `PUT /api/library/works/{id}/state` changes the caller's own finished
//! flag, ratings and tags, `GET /api/library/works/facets/{facet}` lists
//! the values a page can be filtered by, and `GET /api/library/editions/{id}/cover` serves the cover
//! an acquire wrote beside an edition's file.
//!
//! Pages use a keyset cursor, so a page costs the same at any depth and a
//! row added or removed between two requests never shifts the next page.
//! The index is filled by the `host:library-index` consumer and the
//! `library_index_rebuild` job ([`crate::library_index`]).
//!
//! With `q`, the node's works search index
//! ([`bookclerk_search::WorkSearchIndex`]) matches works best first, and the
//! page index supplies their rows and applies the filters; the cursor is
//! then a position in the matches, valid only for the same query and
//! filters. Relevance is computed again for each page, so unlike the
//! keyset pages a search page can repeat or skip a work when the index
//! changes between two requests.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::{
    Abridgement, AcquireStatus, EditionDetail, EditionListing, Entitlement, ExpressionKind,
    FacetValuesOf, HoldingHolder, LibraryWorkRow, ListingHolding, MediaKind, UserTitleState,
    WorkCursor, WorkDetail, WorkFacet, WorkFilter, WorkKind, WorkPageQuery, WorkScope, WorkSort,
};
use bookclerk_search::{SearchError, WorkSearchIndex};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::http_error::ErrorMessage;

/// Default page size of `GET /api/library/works`.
const DEFAULT_LIMIT: u64 = 40;

/// Largest page size of `GET /api/library/works`.
const MAX_LIMIT: u64 = 200;

/// An error answer of these routes: status and JSON message.
pub(crate) type WorksError = (StatusCode, Json<ErrorMessage>);

/// How a page of works is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkSortParam {
    /// By title, A to Z, ignoring a leading "The", "A" or "An".
    #[default]
    Title,
    /// By the first author's sort name, then title.
    Author,
    /// Most recently added first.
    Added,
    /// By first series and position in it, then title; works in no series
    /// last.
    Series,
    /// Most recently purchased first; works no store reported a purchase
    /// time for last.
    Purchased,
    /// Longest runtime first; works with no known runtime last.
    Length,
}

impl From<WorkSortParam> for WorkSort {
    fn from(value: WorkSortParam) -> Self {
        match value {
            WorkSortParam::Title => Self::Title,
            WorkSortParam::Author => Self::Author,
            WorkSortParam::Added => Self::Added,
            WorkSortParam::Series => Self::Series,
            WorkSortParam::Purchased => Self::Purchased,
            WorkSortParam::Length => Self::Length,
        }
    }
}

/// What `facet` and `value` filter works by, or whose values
/// `GET /api/library/works/facets/{facet}` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkFacetParam {
    /// An author (a host, for a podcast).
    Author,
    /// A narrator.
    Narrator,
    /// A series; an empty `value` matches works in no series.
    Series,
    /// A genre (store category).
    Genre,
    /// A publisher.
    Publisher,
    /// The storefront plugin a holding comes through.
    Source,
    /// One of the caller's own tags.
    Tag,
    /// A library tag: one an Administrator set on the work, or one read
    /// from a sidecar file, the same for everyone.
    LibraryTag,
    /// A collection: a named part of the library, by storage key prefix.
    Collection,
    /// The caller's progress: `value` is `finished` or `not_finished`. Not
    /// a value list.
    Progress,
}

impl WorkFacetParam {
    /// The page index facet this names, when it is one.
    fn indexed(self) -> Option<WorkFacet> {
        match self {
            Self::Author => Some(WorkFacet::Author),
            Self::Narrator => Some(WorkFacet::Narrator),
            Self::Series => Some(WorkFacet::Series),
            Self::Genre => Some(WorkFacet::Genre),
            Self::Publisher => Some(WorkFacet::Publisher),
            Self::Source => Some(WorkFacet::Source),
            Self::LibraryTag => Some(WorkFacet::Tag),
            Self::Collection => Some(WorkFacet::Collection),
            Self::Tag | Self::Progress => None,
        }
    }
}

/// The filter of a `facet` and `value` pair.
///
/// # Arguments
///
/// * `facet` - What to filter by; no filter without one.
/// * `value` - The value; missing reads as empty.
/// * `user_id` - The caller (`users.id`), for tags and progress.
///
/// # Errors
///
/// Returns a 400 answer for a progress value other than `finished` or
/// `not_finished`, or an empty value for any facet but series.
fn work_filter(
    facet: Option<WorkFacetParam>,
    value: Option<&str>,
    user_id: Option<i64>,
) -> Result<Option<WorkFilter>, WorksError> {
    let Some(facet) = facet else {
        return Ok(None);
    };
    let value = value.unwrap_or_default().trim();
    if value.is_empty() && facet != WorkFacetParam::Series {
        return Err(bad_request("`facet` needs a `value`"));
    }
    if let Some(indexed) = facet.indexed() {
        return Ok(Some(WorkFilter::Facet {
            facet: indexed,
            value: value.to_string(),
        }));
    }
    Ok(Some(match facet {
        WorkFacetParam::Progress => WorkFilter::Finished {
            user_id,
            finished: match value {
                "finished" => true,
                "not_finished" => false,
                _ => {
                    return Err(bad_request(
                        "a progress `value` is `finished` or `not_finished`",
                    ))
                }
            },
        },
        WorkFacetParam::Tag => WorkFilter::Tag {
            user_id,
            tag: value.to_string(),
        },
        // Answered by `indexed` above.
        WorkFacetParam::Author
        | WorkFacetParam::Narrator
        | WorkFacetParam::Series
        | WorkFacetParam::Genre
        | WorkFacetParam::Publisher
        | WorkFacetParam::Source
        | WorkFacetParam::LibraryTag
        | WorkFacetParam::Collection => return Ok(None),
    }))
}

/// Book or podcast (`works.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkKindJson {
    /// A book, in any expression and on any carrier.
    Book,
    /// A podcast show.
    Podcast,
}

impl From<WorkKind> for WorkKindJson {
    fn from(value: WorkKind) -> Self {
        match value {
            WorkKind::Book => Self::Book,
            WorkKind::Podcast => Self::Podcast,
        }
    }
}

impl From<WorkKindJson> for WorkKind {
    fn from(value: WorkKindJson) -> Self {
        match value {
            WorkKindJson::Book => Self::Book,
            WorkKindJson::Podcast => Self::Podcast,
        }
    }
}

/// Carrier of an edition (`editions.media_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MediaKindJson {
    /// Downloadable audio.
    Audio,
    /// A downloadable document (EPUB, PDF, CBZ, …).
    Ebook,
    /// A printed edition.
    Print,
}

impl From<MediaKind> for MediaKindJson {
    fn from(value: MediaKind) -> Self {
        match value {
            MediaKind::Audio => Self::Audio,
            MediaKind::Ebook => Self::Ebook,
            MediaKind::Print => Self::Print,
        }
    }
}

impl From<MediaKindJson> for MediaKind {
    fn from(value: MediaKindJson) -> Self {
        match value {
            MediaKindJson::Audio => Self::Audio,
            MediaKindJson::Ebook => Self::Ebook,
            MediaKindJson::Print => Self::Print,
        }
    }
}

/// How an expression realises its work (`expressions.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExpressionKindJson {
    /// The written text.
    Text,
    /// A reading by one or more narrators.
    Narration,
    /// A full-cast dramatisation.
    Dramatisation,
    /// Comics, graphic novels, manga.
    Graphic,
    /// Audio that is itself the first realisation (podcast episodes, …).
    OriginalAudio,
}

impl From<ExpressionKind> for ExpressionKindJson {
    fn from(value: ExpressionKind) -> Self {
        match value {
            ExpressionKind::Text => Self::Text,
            ExpressionKind::Narration => Self::Narration,
            ExpressionKind::Dramatisation => Self::Dramatisation,
            ExpressionKind::Graphic => Self::Graphic,
            ExpressionKind::OriginalAudio => Self::OriginalAudio,
        }
    }
}

/// Whether an expression is abridged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AbridgementJson {
    /// The store did not say.
    Unknown,
    /// The full text.
    Unabridged,
    /// A shortened text.
    Abridged,
}

impl From<Abridgement> for AbridgementJson {
    fn from(value: Abridgement) -> Self {
        match value {
            Abridgement::Unknown => Self::Unknown,
            Abridgement::Unabridged => Self::Unabridged,
            Abridgement::Abridged => Self::Abridged,
        }
    }
}

/// On what terms a holder has a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntitlementJson {
    /// Bought outright.
    Purchase,
    /// Included with a store membership while it lasts.
    Plus,
    /// Borrowed for a time.
    Loan,
    /// Given away by the store.
    Free,
    /// The holder's own copy, uploaded rather than bought.
    Own,
}

impl From<Entitlement> for EntitlementJson {
    fn from(value: Entitlement) -> Self {
        match value {
            Entitlement::Purchase => Self::Purchase,
            Entitlement::Plus => Self::Plus,
            Entitlement::Loan => Self::Loan,
            Entitlement::Free => Self::Free,
            Entitlement::Own => Self::Own,
        }
    }
}

/// Query string of `GET /api/library/works`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct WorksQuery {
    /// Search text (Lucene-style); matches titles, authors, narrators,
    /// series and store ids. Works then come best match first and `sort`
    /// is not used.
    q: Option<String>,
    /// Order of the page (default `title`).
    #[param(inline)]
    sort: Option<WorkSortParam>,
    /// Only works with an edition on this carrier.
    #[param(inline)]
    media_kind: Option<MediaKindJson>,
    /// Only works of this kind.
    #[param(inline)]
    kind: Option<WorkKindJson>,
    /// Only works whose furthest acquire state is this one.
    #[param(inline)]
    status: Option<AcquireStatus>,
    /// Only works held by this store account id.
    account: Option<String>,
    /// Only works with `value` under this facet. Values match ignoring
    /// case; `tag` and `progress` read the caller's own state.
    #[param(inline)]
    facet: Option<WorkFacetParam>,
    /// The value `facet` must have, as `GET /api/library/works/facets/{facet}`
    /// lists it (or `finished` / `not_finished` for `progress`).
    value: Option<String>,
    /// `next_cursor` of the previous page, under the same `sort` (or the
    /// same `q`).
    cursor: Option<String>,
    /// Page size, clamped to 1–200 (default 40).
    limit: Option<u64>,
}

/// One work in a page of `GET /api/library/works`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct WorkSummaryJson {
    /// Work id.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    id: String,
    /// Book or podcast.
    kind: WorkKindJson,
    /// Display title.
    #[schema(example = "The Left Hand of Darkness")]
    title: String,
    /// Authors (hosts for a podcast), joined for display.
    #[schema(example = "Ursula K. Le Guin")]
    authors: Option<String>,
    /// First series the work belongs to.
    #[schema(example = "Hainish Cycle")]
    series: Option<String>,
    /// Position in `series`, as the store gave it.
    #[schema(example = "4")]
    series_position: Option<String>,
    /// The edition whose cover the list shows.
    #[schema(example = "7c1d2e3f-4a5b-4c6d-8e7f-9a0b1c2d3e4f")]
    cover_edition_id: Option<String>,
    /// Same-origin path of that cover, when it has one.
    #[schema(example = "/api/covers/eyJ1IjoiaHR0cHM6Ly9pbWFnZXMuZXhhbXBsZS5jb20vYy5qcGcifQ")]
    cover_path: Option<String>,
    /// Carriers of the work's editions.
    media_kinds: Vec<MediaKindJson>,
    /// The furthest acquire state any edition reached.
    acquire_status: AcquireStatus,
    /// How many editions the work has.
    #[schema(example = 2)]
    edition_count: u64,
    /// How many holdings the work's listings have.
    #[schema(example = 3)]
    holding_count: u64,
    /// RFC 3339 time of the newest holding.
    #[schema(example = "2026-11-14T09:30:00.000000Z")]
    added_at: String,
    /// Runtime in minutes of the longest held edition, when known.
    #[schema(example = 1_212)]
    length_minutes: Option<u64>,
    /// RFC 3339 time of the newest purchase a store reported.
    #[schema(example = "2024-03-02T18:04:11Z")]
    purchased_at: Option<String>,
}

/// A page of `GET /api/library/works`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct WorksPage {
    /// The works, in the requested order.
    works: Vec<WorkSummaryJson>,
    /// Pass as `cursor` for the next page; absent on the last page.
    #[schema(example = "eyJzb3J0IjoidGl0bGUiLCJrZXkiOiJsZWZ0IGhhbmQiLCJ3b3JrX2lkIjoiMGI4ZiJ9")]
    next_cursor: Option<String>,
}

/// Who holds a listing.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum HolderJson {
    /// A store account.
    Account {
        /// The account's storefront plugin.
        #[schema(example = "audible")]
        plugin_key: String,
        /// The account id at that plugin.
        #[schema(example = "acct-7f3a")]
        account_id: String,
    },
    /// A branch library's node.
    Node {
        /// The node id.
        #[schema(example = "9b1c7d2e-4f3a-4b6c-8d1e-2f3a4b5c6d7e")]
        node_id: String,
    },
    /// A patron who uploaded the book.
    Patron {
        /// The person's user id.
        #[schema(example = 3)]
        user_id: i64,
    },
}

/// One holding of a listing.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct HoldingJson {
    /// Holding id.
    #[schema(example = "4d5e6f70-8192-4a3b-9c4d-5e6f708192a3")]
    id: String,
    /// Who holds the listing.
    holder: HolderJson,
    /// On what terms.
    entitlement: EntitlementJson,
    /// RFC 3339 purchase time, when the store reported one.
    #[schema(example = "2024-03-02T18:04:11Z")]
    purchased_at: Option<String>,
    /// RFC 3339 time the holding was first recorded.
    #[schema(example = "2026-11-14T09:30:00.000000Z")]
    added_at: String,
}

/// One store listing of an edition.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ListingJson {
    /// Listing id.
    #[schema(example = "3c4d5e6f-7081-4923-8a4b-5c6d7e8f9012")]
    id: String,
    /// The storefront plugin that lists it.
    #[schema(example = "audible")]
    plugin_key: String,
    /// Store marketplace; empty when the store has one.
    #[schema(example = "us")]
    marketplace: String,
    /// The store's product id.
    #[schema(example = "B002V0QK4C")]
    product_id: String,
    /// The product page, when known.
    #[schema(example = "https://www.audible.com/pd/B002V0QK4C")]
    url: Option<String>,
    /// The listing's holdings.
    holdings: Vec<HoldingJson>,
}

/// One edition of a work.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct EditionJson {
    /// Edition id.
    #[schema(example = "7c1d2e3f-4a5b-4c6d-8e7f-9a0b1c2d3e4f")]
    id: String,
    /// The work it realises: this work, or one of a show's episodes.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    work_id: String,
    /// The expression it carries.
    #[schema(example = "5a6b7c8d-9e0f-4a1b-8c2d-3e4f5a6b7c8d")]
    expression_id: String,
    /// How the expression realises the work.
    expression_kind: ExpressionKindJson,
    /// The expression's language tag, when known.
    #[schema(example = "en")]
    language: Option<String>,
    /// Whether the expression is abridged.
    abridgement: AbridgementJson,
    /// Carrier of the edition.
    media_kind: MediaKindJson,
    /// The edition's title.
    #[schema(example = "The Left Hand of Darkness")]
    title: String,
    /// The edition's subtitle.
    #[schema(example = "A Hainish Novel")]
    subtitle: Option<String>,
    /// Publication date as the store gave it.
    #[schema(example = "2009-11-25")]
    published_at: Option<String>,
    /// Running time in minutes, for audio.
    #[schema(example = 682)]
    duration_minutes: Option<i64>,
    /// Same-origin path of the edition's cover, when it has one.
    #[schema(example = "/api/covers/eyJ1IjoiaHR0cHM6Ly9pbWFnZXMuZXhhbXBsZS5jb20vYy5qcGcifQ")]
    cover_path: Option<String>,
    /// The edition's acquire state.
    acquire_status: AcquireStatus,
    /// The edition's store listings.
    listings: Vec<ListingJson>,
}

/// The caller's own state for a work.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct WorkStateJson {
    /// Whether the caller finished the work.
    #[schema(example = true)]
    is_finished: bool,
    /// Overall rating, 0 to 5.
    #[schema(example = 4.5)]
    rating_overall: Option<f64>,
    /// Narration rating, 0 to 5.
    #[schema(example = 4.0)]
    rating_performance: Option<f64>,
    /// Story rating, 0 to 5.
    #[schema(example = 5.0)]
    rating_story: Option<f64>,
    /// The caller's tags, as one serialized string.
    #[schema(example = "classics")]
    tags: Option<String>,
}

/// `GET /api/library/works/{id}` answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct WorkDetailJson {
    /// Work id.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    id: String,
    /// Book or podcast.
    kind: WorkKindJson,
    /// Display title.
    #[schema(example = "The Left Hand of Darkness")]
    title: String,
    /// Blurb or synopsis (may contain HTML).
    #[schema(
        example = "An envoy to the planet Gethen must win the trust of a people who have no fixed gender."
    )]
    description: Option<String>,
    /// Authors (hosts for a podcast), in billing order.
    #[schema(example = json!(["Ursula K. Le Guin"]))]
    authors: Vec<String>,
    /// First series the work belongs to.
    #[schema(example = "Hainish Cycle")]
    series: Option<String>,
    /// Position in `series`.
    #[schema(example = "4")]
    series_position: Option<String>,
    /// Editions; a show's episodes' editions included.
    editions: Vec<EditionJson>,
    /// The caller's own state; absent for the operator or when the caller
    /// recorded nothing.
    state: Option<WorkStateJson>,
    /// Whether the caller can keep their own state for the work
    /// (`PUT /api/library/works/{id}/state`): false when the session has no
    /// library user, such as the operator token.
    #[schema(example = true)]
    can_edit_state: bool,
}

/// The signed-in caller's user (`users.id`); `None` for an operator token
/// or a session with no user.
pub(crate) async fn caller_user_id(state: &AppState, headers: &HeaderMap) -> Option<i64> {
    crate::auth::caller_portal_identity(state, headers)
        .await
        .and_then(|identity| identity.user_id)
}

/// The patron scope filter of the caller: `None` for the operator, an
/// Owner or Administrator, or a patron whose scope hides nothing.
///
/// Only a caller positively identified as the operator, or as a user whose
/// stored role is never scoped, sees the whole library. A session linked to
/// no user (or to a user that no longer exists) gets the library default.
///
/// The caller, their user and their scope are read from `library`, the
/// handle the route then reads its rows from, so a database reload between
/// the two cannot pair one store's scope with another's rows.
///
/// # Errors
///
/// A 500 when the caller, the user or their scope cannot be read, and a
/// 401 when the session ended since it was admitted: the library fails
/// closed rather than show a patron books outside their scope.
pub(crate) async fn caller_scope(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    headers: &HeaderMap,
    route: &str,
) -> Result<Option<WorkScope>, WorksError> {
    let user_id = match crate::auth::caller_subject(state, library, headers).await {
        crate::auth::CallerSubject::Operator => return Ok(None),
        crate::auth::CallerSubject::Person(user_id) => user_id,
        crate::auth::CallerSubject::SignedOut => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorMessage::new("sign in again")),
            ))
        }
        crate::auth::CallerSubject::Unavailable => {
            tracing::error!(route, "the caller could not be identified");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new("the library could not be read")),
            ));
        }
    };
    let user = match user_id {
        Some(user_id) => library
            .get_user(user_id)
            .await
            .map_err(|err| read_failed(route, &err))?,
        None => None,
    };
    match user {
        Some(user) => bookclerk_library::control_plane::work_scope_for_user(library, &user)
            .await
            .map_err(|err| read_failed(route, &err)),
        None => bookclerk_library::control_plane::default_work_scope(library)
            .await
            .map_err(|err| read_failed(route, &err)),
    }
}

/// Refuses a work outside the caller's scope with the same 404 an unknown
/// work gets, so a patron cannot tell a hidden book from a missing one.
///
/// # Errors
///
/// A 404 when the work is outside `scope`, a 500 when it cannot be read.
pub(crate) async fn require_in_scope(
    library: &bookclerk_library::LibraryStore,
    scope: Option<&WorkScope>,
    work_id: &str,
    kind: bookclerk_library::EntityKind,
    id: &str,
    route: &str,
) -> Result<(), WorksError> {
    let Some(scope) = scope else {
        return Ok(());
    };
    if library
        .work_in_scope(work_id, scope)
        .await
        .map_err(|err| read_failed(route, &err))?
    {
        Ok(())
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new(
                bookclerk_library::LibraryError::not_found(kind, id),
            )),
        ))
    }
}

/// Logs a failed read and returns the 500 answer.
pub(crate) fn read_failed(route: &str, err: &bookclerk_library::LibraryError) -> WorksError {
    tracing::error!(error = %err, route, "library works read failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorMessage::new("the library could not be read")),
    )
}

/// The wire form of one index row.
fn summary(row: LibraryWorkRow, cover_path: Option<String>) -> WorkSummaryJson {
    WorkSummaryJson {
        id: row.work_id,
        kind: row.kind.into(),
        title: row.title,
        authors: row.authors,
        series: row.series,
        series_position: row.series_position,
        cover_edition_id: row.cover_edition_id,
        cover_path,
        media_kinds: row.media_kinds.into_iter().map(Into::into).collect(),
        acquire_status: row.acquire_status,
        edition_count: row.edition_count,
        holding_count: row.holding_count,
        added_at: row.added_at,
        length_minutes: row.length_minutes,
        purchased_at: row.purchased_at,
    }
}

/// The wire form of one holding.
fn holding_json(holding: ListingHolding) -> HoldingJson {
    HoldingJson {
        id: holding.id,
        holder: match holding.holder {
            HoldingHolder::Account {
                plugin_key,
                account_id,
            } => HolderJson::Account {
                plugin_key,
                account_id,
            },
            HoldingHolder::Node { node_id } => HolderJson::Node { node_id },
            HoldingHolder::Patron { user_id } => HolderJson::Patron { user_id },
        },
        entitlement: holding.entitlement.into(),
        purchased_at: holding.purchased_at,
        added_at: holding.added_at,
    }
}

/// The wire form of one listing.
fn listing_json(listing: EditionListing) -> ListingJson {
    ListingJson {
        id: listing.id,
        plugin_key: listing.plugin_key,
        marketplace: listing.marketplace,
        product_id: listing.product_id,
        url: listing.url,
        holdings: listing.holdings.into_iter().map(holding_json).collect(),
    }
}

/// The wire form of one edition, with its cover path signed by `covers`.
fn edition_json(edition: EditionDetail, covers: &crate::covers::CoverService) -> EditionJson {
    EditionJson {
        cover_path: edition
            .cover_url
            .as_deref()
            .and_then(|url| covers.path_for(url)),
        id: edition.id,
        work_id: edition.work_id,
        expression_id: edition.expression_id,
        expression_kind: edition.expression_kind.into(),
        language: edition.language,
        abridgement: edition.abridgement.into(),
        media_kind: edition.media_kind.into(),
        title: edition.title,
        subtitle: edition.subtitle,
        published_at: edition.published_at,
        duration_minutes: edition.duration_minutes,
        acquire_status: edition.acquire_status,
        listings: edition.listings.into_iter().map(listing_json).collect(),
    }
}

/// The wire form of a work's detail and the caller's state.
fn detail_json(
    detail: WorkDetail,
    state: Option<UserTitleState>,
    can_edit_state: bool,
    covers: &crate::covers::CoverService,
) -> WorkDetailJson {
    WorkDetailJson {
        id: detail.id,
        kind: detail.kind.into(),
        title: detail.title,
        description: detail.description,
        authors: detail.authors,
        series: detail.series,
        series_position: detail.series_position,
        editions: detail
            .editions
            .into_iter()
            .map(|e| edition_json(e, covers))
            .collect(),
        state: state.map(state_json),
        can_edit_state,
    }
}

/// Matches of a search one request examines at most; a page whose
/// filters reject them all comes back short, with a cursor past them.
const SEARCH_SCAN_BUDGET: usize = 1_000;

/// Match ids read from the page index per query while filling a search
/// page.
const SEARCH_CHUNK: usize = 200;

/// Where the next page of a search starts: how many of the query's
/// matches earlier pages consumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SearchCursor {
    /// The query the cursor was issued for; refused under another.
    q: String,
    /// The filters it was issued under ([`search_filter_key`]); refused
    /// under others.
    filters: String,
    /// Matches consumed so far.
    at: usize,
}

/// A stable key of the filters a search page applies (not its limit), so
/// a cursor is refused under other filters.
fn search_filter_key(filters: &WorkPageQuery) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        filters.media_kind,
        filters.kind,
        filters.status,
        filters.account,
        filters.filter,
        filters.scope
    )
}

impl SearchCursor {
    /// The opaque token: URL-safe base64 of the JSON.
    fn encode(&self) -> String {
        use base64::Engine as _;
        // Serializing a string and a number cannot fail.
        let json = serde_json::to_vec(self).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    /// Parses a token from [`Self::encode`]; `None` when it is not one.
    fn decode(token: &str) -> Option<Self> {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.as_bytes())
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// A 400 answer with `message`.
fn bad_request(message: impl Into<String>) -> WorksError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorMessage::new(message.into())),
    )
}

/// Fills one search page from `window` (match ids, best first): looks up
/// `chunk` ids at a time with `lookup`, which returns the rows that pass
/// the filters in the same order, until `limit` rows are found or the
/// window runs out.
///
/// # Returns
///
/// The page's rows and how many of the window's matches it used: just
/// past the last match it took when the page filled, or the whole window
/// when it did not (the next page then starts past the window).
///
/// # Errors
///
/// Propagates the first `lookup` failure.
async fn fill_search_page<T, E, F, Fut>(
    window: &[String],
    limit: usize,
    chunk: usize,
    id_of: fn(&T) -> &str,
    mut lookup: F,
) -> Result<(Vec<T>, usize), E>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<T>, E>>,
{
    let mut rows = Vec::new();
    let mut used = 0;
    if limit == 0 {
        return Ok((rows, used));
    }
    for part in window.chunks(chunk.max(1)) {
        let found = lookup(part.to_vec()).await?;
        let wanted = limit - rows.len();
        if found.len() >= wanted {
            // The page fills inside this chunk: stop just past the last
            // match it takes.
            let last = id_of(&found[wanted - 1]).to_string();
            used += part
                .iter()
                .position(|id| *id == last)
                .map_or(part.len(), |i| i + 1);
            rows.extend(found.into_iter().take(wanted));
            return Ok((rows, used));
        }
        rows.extend(found);
        used += part.len();
    }
    Ok((rows, used))
}

/// One page of `q`'s matches that pass `filters`, best first.
///
/// A window of at most [`SEARCH_SCAN_BUDGET`] matches is read from the
/// cursor's position and walked in chunks; the page index drops the ones
/// the filters exclude. A page holds at most `limit` rows, and the next
/// cursor points just past the last match it used, or past the window
/// when the filters rejected too many to fill the page.
///
/// # Errors
///
/// A 400 for a query that does not parse or a cursor of another query or
/// other filters, a 503 before the node's works index is first built, a
/// 500 when the index or the library cannot be read.
async fn search_page(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    q: &str,
    cursor: Option<&str>,
    filters: &WorkPageQuery,
) -> Result<(Vec<LibraryWorkRow>, Option<String>), WorksError> {
    let filter_key = search_filter_key(filters);
    let start = match cursor {
        Some(token) => match SearchCursor::decode(token) {
            Some(cursor) if cursor.q == q && cursor.filters == filter_key => cursor.at,
            Some(_) => {
                return Err(bad_request(
                    "page cursor belongs to another query or other filters",
                ))
            }
            None => return Err(bad_request("invalid page cursor")),
        },
        None => 0,
    };
    let dir = state.config.read().await.paths().search_index_dir.clone();
    let window = WorkSearchIndex::open_and_match_ids(dir, q.into(), start, SEARCH_SCAN_BUDGET)
        .await
        .map_err(|err| match err {
            SearchError::Query(message) => bad_request(format!("invalid search query: {message}")),
            SearchError::NotReady => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorMessage::new(
                    "the search index is being built; try again shortly",
                )),
            ),
            other => {
                tracing::warn!(error = %other, "GET /api/library/works: search failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorMessage::new("the search index could not be read")),
                )
            }
        })?;
    let library = library.clone();
    let limit = usize::try_from(filters.limit).unwrap_or(usize::MAX);
    let (rows, used) = fill_search_page(
        &window.ids,
        limit,
        SEARCH_CHUNK.max(limit),
        |row: &LibraryWorkRow| row.work_id.as_str(),
        |chunk| {
            let library = library.clone();
            async move { library.library_works_by_ids(&chunk, filters).await }
        },
    )
    .await
    .map_err(|err| read_failed("GET /api/library/works", &err))?;
    let at = start.saturating_add(used);
    let next = (at < window.total).then(|| {
        SearchCursor {
            q: q.to_string(),
            filters: filter_key,
            at,
        }
        .encode()
    });
    Ok((rows, next))
}

/// Lists the library's works, one page at a time.
///
/// Each work appears once however many editions, stores and accounts hold
/// it; a podcast is listed as its show. Pass `next_cursor` back as
/// `cursor` (with the same `sort`) for the next page. With `q`, works that
/// match come best first; pass `next_cursor` back with the same `q`.
#[utoipa::path(
    get,
    path = "/api/library/works",
    params(WorksQuery),
    responses(
        (status = 200, description = "A page of works.", body = WorksPage),
        (status = 400, description = "A filter, the query or the cursor is invalid.", body = ErrorMessage, example = json!({"error": "invalid page cursor"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
        (status = 503, description = "A search (`q`) arrived before the node's works search index was first built; that answer carries no `Retry-After`.", body = ErrorMessage, headers(("Retry-After" = u64, description = "Seconds to wait before asking again; sent only when the sign-in could not be checked.")), example = json!({"error": "the search index is being built; try again shortly"})),
    ),
)]
pub(crate) async fn list_works(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<WorksQuery>,
) -> Result<Json<WorksPage>, WorksError> {
    let cursor = query.cursor.as_deref().filter(|c| !c.is_empty());
    let user_id = match query.facet {
        Some(WorkFacetParam::Tag | WorkFacetParam::Progress) => {
            caller_user_id(&state, &headers).await
        }
        _ => None,
    };
    let filter = work_filter(query.facet, query.value.as_deref(), user_id)?;
    let library = state.library_snapshot().await;
    let mut filters = WorkPageQuery {
        sort: query.sort.unwrap_or_default().into(),
        media_kind: query.media_kind.map(Into::into),
        kind: query.kind.map(Into::into),
        status: query.status,
        account: query.account.filter(|a| !a.is_empty()),
        filter,
        scope: caller_scope(&state, &library, &headers, "GET /api/library/works").await?,
        after: None,
        limit: query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
    };
    let (rows, next_cursor) = match query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => search_page(&state, &library, q, cursor, &filters).await?,
        None => {
            if let Some(token) = cursor {
                filters.after =
                    Some(WorkCursor::decode(token).map_err(|err| bad_request(err.to_string()))?);
            }
            let page = library
                .library_works_page(&filters)
                .await
                .map_err(|err| match err {
                    bookclerk_library::LibraryError::Invalid(message) => bad_request(message),
                    other => read_failed("GET /api/library/works", &other),
                })?;
            (page.works, page.next.map(|c| c.encode()))
        }
    };
    let cover_ids: Vec<String> = rows
        .iter()
        .filter_map(|w| w.cover_edition_id.clone())
        .collect();
    let urls = library
        .edition_cover_urls(&cover_ids)
        .await
        .map_err(|err| read_failed("GET /api/library/works", &err))?;
    let works = rows
        .into_iter()
        .map(|row| {
            let cover_path = row
                .cover_edition_id
                .as_ref()
                .and_then(|id| urls.get(id))
                .and_then(|url| state.covers.path_for(url));
            summary(row, cover_path)
        })
        .collect();
    Ok(Json(WorksPage { works, next_cursor }))
}

/// Reads one work with its editions, listings, holdings and the caller's
/// own state.
#[utoipa::path(
    get,
    path = "/api/library/works/{id}",
    params(("id" = String, Path, description = "Work id (a podcast's show).")),
    responses(
        (status = 200, description = "The work.", body = WorkDetailJson),
        (status = 404, description = "No such work.", body = ErrorMessage, example = json!({"error": "work 0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b not found"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn get_work(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<WorkDetailJson>, WorksError> {
    let library = state.library_snapshot().await;
    let detail = library
        .library_work_detail(&id)
        .await
        .map_err(|err| read_failed("GET /api/library/works/{id}", &err))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorMessage::new(
                    bookclerk_library::LibraryError::not_found(
                        bookclerk_library::EntityKind::Work,
                        &id,
                    ),
                )),
            )
        })?;
    let scope = caller_scope(&state, &library, &headers, "GET /api/library/works/{id}").await?;
    require_in_scope(
        &library,
        scope.as_ref(),
        &detail.id,
        bookclerk_library::EntityKind::Work,
        &id,
        "GET /api/library/works/{id}",
    )
    .await?;
    let user_id = caller_user_id(&state, &headers).await;
    let user_state = match user_id {
        Some(user_id) => library
            .user_title_state(user_id, &detail.id)
            .await
            .map_err(|err| read_failed("GET /api/library/works/{id}", &err))?,
        None => None,
    };
    Ok(Json(detail_json(
        detail,
        user_state,
        user_id.is_some(),
        &state.covers,
    )))
}

/// Longest `tags` string `PUT /api/library/works/{id}/state` accepts, in
/// bytes (the minimum database profile's cell size).
const MAX_TAGS_BYTES: usize = 1024;

/// Most distinct tags one work's state may carry, so the write (two state
/// statements, one delete and one insert per tag) stays one batch under
/// the minimum database profile's 40 statements.
const MAX_TAGS: usize = 32;

/// Longest single tag, in bytes.
const MAX_TAG_BYTES: usize = 64;

/// `PUT /api/library/works/{id}/state` body: the fields of the caller's
/// own state to change. A field left out keeps its stored value; `null`
/// clears a rating or the tags.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkStateEdit {
    /// Whether the caller finished the work.
    #[serde(default)]
    #[schema(example = true)]
    is_finished: Option<bool>,
    /// Overall rating, 0 to 5; `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<f64>, nullable, example = 4.5)]
    rating_overall: Option<Option<f64>>,
    /// Narration rating, 0 to 5; `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<f64>, nullable, example = 4.0)]
    rating_performance: Option<Option<f64>>,
    /// Story rating, 0 to 5; `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<f64>, nullable, example = 5.0)]
    rating_story: Option<Option<f64>>,
    /// Tags separated by whitespace (at most 32, each at most 64 bytes,
    /// 1024 bytes in all); `null` or an empty string clears them.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>, nullable, example = "classics reread")]
    tags: Option<Option<String>>,
}

/// Deserializes a field that was present, so `Option<Option<T>>` tells an
/// explicit `null` (`Some(None)`) from a missing field (`None`, by
/// `serde(default)`).
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Checks one rating field and maps it to the store's edit.
///
/// # Arguments
///
/// * `name` - The field's wire name, for the error message.
/// * `value` - The field as sent.
///
/// # Errors
///
/// A 400 answer when the rating is not a number from 0 to 5.
fn rating_edit(
    name: &str,
    value: Option<Option<f64>>,
) -> Result<bookclerk_library::FieldEdit<f64>, WorksError> {
    use bookclerk_library::FieldEdit;
    match value {
        None => Ok(FieldEdit::Keep),
        Some(None) => Ok(FieldEdit::Clear),
        Some(Some(rating)) if rating.is_finite() && (0.0..=5.0).contains(&rating) => {
            Ok(FieldEdit::Set(rating))
        }
        Some(Some(_)) => Err(bad_request(format!("{name} must be from 0 to 5"))),
    }
}

/// Checks the tags field and maps it to the store's edit: whitespace runs
/// collapse to one space and an empty value clears the tags.
///
/// # Errors
///
/// A 400 answer when the tags are too long, too many, or contain a control
/// character.
fn tags_edit(
    value: Option<Option<String>>,
) -> Result<bookclerk_library::FieldEdit<String>, WorksError> {
    use bookclerk_library::FieldEdit;
    let Some(value) = value else {
        return Ok(FieldEdit::Keep);
    };
    let Some(raw) = value else {
        return Ok(FieldEdit::Clear);
    };
    if raw.len() > MAX_TAGS_BYTES {
        return Err(bad_request("tags must be at most 1024 bytes"));
    }
    let tags: Vec<&str> = raw.split_whitespace().collect();
    if tags.is_empty() {
        return Ok(FieldEdit::Clear);
    }
    if tags.iter().any(|tag| tag.chars().any(char::is_control)) {
        return Err(bad_request("tags must not contain control characters"));
    }
    if tags.iter().any(|tag| tag.len() > MAX_TAG_BYTES) {
        return Err(bad_request("each tag must be at most 64 bytes"));
    }
    let distinct: std::collections::BTreeSet<String> = tags
        .iter()
        .map(|tag| bookclerk_library::facet_key(tag))
        .collect();
    if distinct.len() > MAX_TAGS {
        return Err(bad_request("a work takes at most 32 tags"));
    }
    Ok(FieldEdit::Set(tags.join(" ")))
}

/// Changes the caller's own state for a work: finished, ratings and tags.
///
/// The state belongs to the signed-in library user, so members who share
/// a storefront account each keep their own. Fields left out keep their
/// value.
#[utoipa::path(
    put,
    path = "/api/library/works/{id}/state",
    params(("id" = String, Path, description = "Work id (a podcast's show).")),
    request_body = WorkStateEdit,
    responses(
        (status = 200, description = "The caller's state after the change.", body = WorkStateJson),
        (status = 400, description = "A rating is outside 0 to 5, or the tags are too long or too many.", body = ErrorMessage, example = json!({"error": "rating_overall must be from 0 to 5"})),
        (status = 403, description = "The session has no library user to keep the state for.", body = ErrorMessage, example = json!({"error": "this session has no library user"})),
        (status = 404, description = "No such work.", body = ErrorMessage, example = json!({"error": "work 0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b not found"})),
        (status = 500, description = "The state could not be written.", body = ErrorMessage, example = json!({"error": "the state could not be written"})),
    ),
)]
pub(crate) async fn put_work_state(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<WorkStateEdit>,
) -> Result<Json<WorkStateJson>, WorksError> {
    let edit = bookclerk_library::UserStateEdit {
        is_finished: body.is_finished,
        rating_overall: rating_edit("rating_overall", body.rating_overall)?,
        rating_performance: rating_edit("rating_performance", body.rating_performance)?,
        rating_story: rating_edit("rating_story", body.rating_story)?,
        tags: tags_edit(body.tags)?,
    };
    let Some(user_id) = caller_user_id(&state, &headers).await else {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorMessage::new("this session has no library user")),
        ));
    };
    let library = state.library_snapshot().await;
    let stored = library
        .edit_user_title_state(user_id, &id, &edit)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "work state write failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new("the state could not be written")),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorMessage::new(
                    bookclerk_library::LibraryError::not_found(
                        bookclerk_library::EntityKind::Work,
                        &id,
                    ),
                )),
            )
        })?;
    Ok(Json(state_json(stored)))
}

/// The wire form of one user's state.
fn state_json(s: UserTitleState) -> WorkStateJson {
    WorkStateJson {
        is_finished: s.is_finished,
        rating_overall: s.rating_overall,
        rating_performance: s.rating_performance,
        rating_story: s.rating_story,
        tags: s.tags,
    }
}

/// Default page size of `GET /api/library/works/facets/{facet}`.
const DEFAULT_FACET_LIMIT: u64 = 200;

/// Largest page size of `GET /api/library/works/facets/{facet}`.
const MAX_FACET_LIMIT: u64 = 500;

/// Query string of `GET /api/library/works/facets/{facet}`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct FacetValuesQuery {
    /// `next_cursor` of the previous page of the same facet.
    cursor: Option<String>,
    /// Page size, clamped to 1–500 (default 200).
    limit: Option<u64>,
}

/// One value of a facet.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct FacetValueJson {
    /// The value, as `value` of `GET /api/library/works` takes it.
    #[schema(example = "Science Fiction")]
    value: String,
    /// How many held works have it.
    #[schema(example = 12)]
    works: u64,
}

/// A page of `GET /api/library/works/facets/{facet}`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct FacetValuesJson {
    /// The values, in order of their lower-cased spelling.
    values: Vec<FacetValueJson>,
    /// Pass as `cursor` for the next page; absent on the last page.
    #[schema(example = "c2NpZW5jZSBmaWN0aW9u")]
    next_cursor: Option<String>,
}

/// Encodes a facet key as an opaque cursor.
fn facet_cursor(key: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.as_bytes())
}

/// Decodes [`facet_cursor`].
fn facet_cursor_key(token: &str) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// Lists the values a facet has across the library's held works, with
/// how many works have each, for the library page's filter menus. `tag`
/// lists the caller's own tags (none for an operator token).
#[utoipa::path(
    get,
    path = "/api/library/works/facets/{facet}",
    params(
        ("facet" = inline(WorkFacetParam), Path, description = "The facet; `progress` has no value list."),
        FacetValuesQuery,
    ),
    responses(
        (status = 200, description = "A page of values.", body = FacetValuesJson),
        (status = 400, description = "`progress` was asked for, or the cursor is invalid.", body = ErrorMessage, example = json!({"error": "invalid page cursor"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn list_facet_values(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(facet): AxumPath<WorkFacetParam>,
    Query(query): Query<FacetValuesQuery>,
) -> Result<Json<FacetValuesJson>, WorksError> {
    const ROUTE: &str = "GET /api/library/works/facets/{facet}";
    let after = match query.cursor.as_deref().filter(|c| !c.is_empty()) {
        Some(token) => {
            Some(facet_cursor_key(token).ok_or_else(|| bad_request("invalid page cursor"))?)
        }
        None => None,
    };
    let limit = query
        .limit
        .unwrap_or(DEFAULT_FACET_LIMIT)
        .clamp(1, MAX_FACET_LIMIT);
    let of = match facet.indexed() {
        Some(indexed) => FacetValuesOf::Facet(indexed),
        None if facet == WorkFacetParam::Tag => match caller_user_id(&state, &headers).await {
            Some(user_id) => FacetValuesOf::Tags(user_id),
            None => {
                return Ok(Json(FacetValuesJson {
                    values: Vec::new(),
                    next_cursor: None,
                }))
            }
        },
        None => {
            return Err(bad_request(
                "progress has no value list: its values are `finished` and `not_finished`",
            ))
        }
    };
    let library = state.library_snapshot().await;
    let scope = caller_scope(&state, &library, &headers, ROUTE).await?;
    let page = library
        .library_facet_values(of, after.as_deref(), limit, scope.as_ref())
        .await
        .map_err(|err| read_failed(ROUTE, &err))?;
    Ok(Json(FacetValuesJson {
        values: page
            .values
            .into_iter()
            .map(|v| FacetValueJson {
                value: v.value,
                works: v.works,
            })
            .collect(),
        next_cursor: page.next.as_deref().map(facet_cursor),
    }))
}

/// Serves the JPEG cover written beside an edition's acquired file on this
/// node's local destination.
#[utoipa::path(
    get,
    path = "/api/library/editions/{id}/cover",
    params(("id" = String, Path, description = "Edition id (an `id` in a work's `editions`).")),
    responses(
        (status = 200, description = "The cover.", content_type = "image/jpeg"),
        (status = 404, description = "No such edition, or no cover of it on this node: it is not acquired, this node runs no local destination, or the acquire wrote no cover.", body = ErrorMessage, examples(("unknown" = (summary = "No such edition", value = json!({"error": "edition 5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d not found"}))), ("no_cover" = (summary = "No cover of it here", value = json!({"error": "edition 5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d has no cover on this node"}))))),
        (status = 500, description = "The library or the cover could not be read.", body = ErrorMessage, examples(("library" = (summary = "The library could not be read", value = json!({"error": "the library could not be read"}))), ("cover" = (summary = "The cover is there but could not be read", value = json!({"error": "the cover could not be read"}))))),
    ),
)]
pub(crate) async fn get_edition_cover(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, WorksError> {
    const ROUTE: &str = "GET /api/library/editions/{id}/cover";
    let library = state.library_snapshot().await;
    let scope = caller_scope(&state, &library, &headers, ROUTE).await?;
    let Some(work_id) = library
        .work_of_edition(&id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new(
                bookclerk_library::LibraryError::not_found(
                    bookclerk_library::EntityKind::Edition,
                    &id,
                ),
            )),
        ));
    };
    require_in_scope(
        &library,
        scope.as_ref(),
        &work_id,
        bookclerk_library::EntityKind::Edition,
        &id,
        ROUTE,
    )
    .await?;
    let no_cover = || {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new(format!(
                "edition {id} has no cover on this node"
            ))),
        )
    };
    let storage_key = library
        .edition_acquisition(&id)
        .await
        .map_err(|err| read_failed(ROUTE, &err))?
        .and_then(|acquisition| acquisition.storage_key)
        .ok_or_else(no_cover)?;
    // Read from a destination that recorded the cover first, then from the
    // rest: the destination the acquisition wrote first need not lead the
    // deployed list once more instances are added.
    let cover_key = bookclerk_acquire::sidecar_key(&storage_key, "jpg");
    let deployed = state.destinations.read().await.destinations();
    let mut recorded = Vec::new();
    let mut others = Vec::new();
    for destination in deployed {
        let holds = library
            .edition_file(destination.id.as_str(), &cover_key)
            .await
            .map_err(|err| read_failed(ROUTE, &err))?
            .is_some();
        if holds {
            recorded.push(destination);
        } else {
            others.push(destination);
        }
    }
    let mut found = None;
    for destination in recorded.into_iter().chain(others) {
        match destination.backend.get(&cover_key).await {
            Ok(bytes) => {
                found = Some(bytes);
                break;
            }
            Err(bookclerk_storage::StorageError::NotFound(_)) => {}
            // The cover may be there but unreadable: a failure, not a 404.
            Err(err) => {
                tracing::error!(
                    error = %err,
                    route = ROUTE,
                    destination = %destination.id,
                    "edition cover read failed"
                );
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorMessage::new("the cover could not be read")),
                ));
            }
        }
    }
    let bytes = found.ok_or_else(no_cover)?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/jpeg")],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
#[path = "library_works_tests.rs"]
mod tests;
