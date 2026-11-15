//! The library's works: `GET /api/library/works` pages the page index
//! (`library_work_rows`) and `GET /api/library/works/{id}` reads one work
//! with its editions, store listings, holdings and the caller's own state.
//!
//! Pages use a keyset cursor, so a page costs the same at any depth and a
//! row added or removed between two requests never shifts the next page.
//! The index is filled by the `host:library-index` consumer and the
//! `library_index_rebuild` job ([`crate::library_index`]).
//!
//! With `q`, the node's works search index
//! ([`bookclerk_search::WorkSearchIndex`]) matches works best first, and the
//! page index supplies their rows and applies the filters; the cursor is
//! then a position in the matches, valid only for the same query.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::{
    Abridgement, AcquireStatus, EditionDetail, EditionListing, Entitlement, ExpressionKind,
    HoldingHolder, LibraryWorkRow, ListingHolding, MediaKind, UserTitleState, WorkCursor,
    WorkDetail, WorkKind, WorkPageQuery, WorkSort,
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
type WorksError = (StatusCode, Json<ErrorMessage>);

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
}

impl From<WorkSortParam> for WorkSort {
    fn from(value: WorkSortParam) -> Self {
        match value {
            WorkSortParam::Title => Self::Title,
            WorkSortParam::Author => Self::Author,
            WorkSortParam::Added => Self::Added,
        }
    }
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
}

impl From<Entitlement> for EntitlementJson {
    fn from(value: Entitlement) -> Self {
        match value {
            Entitlement::Purchase => Self::Purchase,
            Entitlement::Plus => Self::Plus,
            Entitlement::Loan => Self::Loan,
            Entitlement::Free => Self::Free,
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
}

/// Logs a failed read and returns the 500 answer.
fn read_failed(route: &str, err: &bookclerk_library::LibraryError) -> WorksError {
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
        state: state.map(|s| WorkStateJson {
            is_finished: s.is_finished,
            rating_overall: s.rating_overall,
            rating_performance: s.rating_performance,
            rating_story: s.rating_story,
            tags: s.tags,
        }),
    }
}

/// Where the next page of a search starts: how many of the query's
/// matches earlier pages consumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SearchCursor {
    /// The query the cursor was issued for; refused under another.
    q: String,
    /// Matches consumed so far.
    at: usize,
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

/// One page of `q`'s matches that pass `filters`, best first.
///
/// The matches are walked from the cursor's position in chunks; the page
/// index drops the ones the filters exclude, so a page holds at most
/// `limit` rows and the next cursor points just past the last match it
/// used.
///
/// # Errors
///
/// A 400 for a query that does not parse or a cursor of another query, a
/// 500 when the index or the library cannot be read.
async fn search_page(
    state: &AppState,
    q: &str,
    cursor: Option<&str>,
    filters: &WorkPageQuery,
) -> Result<(Vec<LibraryWorkRow>, Option<String>), WorksError> {
    let start = match cursor {
        Some(token) => match SearchCursor::decode(token) {
            Some(cursor) if cursor.q == q => cursor.at,
            Some(_) => return Err(bad_request("page cursor belongs to another query")),
            None => return Err(bad_request("invalid page cursor")),
        },
        None => 0,
    };
    let dir = state.config.read().await.paths().search_index_dir.clone();
    let matches = WorkSearchIndex::open_and_match_ids(WorkSearchIndex::dir_beside(&dir), q.into())
        .await
        .map_err(|err| match err {
            SearchError::Query(message) => bad_request(format!("invalid search query: {message}")),
            other => {
                tracing::warn!(error = %other, "GET /api/library/works: search failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorMessage::new("the search index could not be read")),
                )
            }
        })?;
    let library = state.library_snapshot().await;
    let limit = usize::try_from(filters.limit).unwrap_or(usize::MAX);
    let mut rows = Vec::new();
    let mut at = start.min(matches.len());
    while rows.len() < limit && at < matches.len() {
        let chunk = &matches[at..matches.len().min(at.saturating_add(limit))];
        let found = library
            .library_works_by_ids(chunk, filters)
            .await
            .map_err(|err| read_failed("GET /api/library/works", &err))?;
        let wanted = limit - rows.len();
        if found.len() > wanted {
            // The page fills inside this chunk: stop just past the last
            // match it takes.
            let last = &found[wanted - 1].work_id;
            let used = chunk
                .iter()
                .position(|id| id == last)
                .map_or(chunk.len(), |i| i + 1);
            rows.extend(found.into_iter().take(wanted));
            at += used;
            break;
        }
        rows.extend(found);
        at += chunk.len();
    }
    let next = (at < matches.len()).then(|| {
        SearchCursor {
            q: q.to_string(),
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
        (status = 400, description = "A filter or the cursor is invalid.", body = ErrorMessage, example = json!({"error": "invalid page cursor"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn list_works(
    State(state): State<Arc<AppState>>,
    Query(query): Query<WorksQuery>,
) -> Result<Json<WorksPage>, WorksError> {
    let cursor = query.cursor.as_deref().filter(|c| !c.is_empty());
    let mut filters = WorkPageQuery {
        sort: query.sort.unwrap_or_default().into(),
        media_kind: query.media_kind.map(Into::into),
        kind: query.kind.map(Into::into),
        status: query.status,
        account: query.account.filter(|a| !a.is_empty()),
        after: None,
        limit: query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
    };
    let library = state.library_snapshot().await;
    let (rows, next_cursor) = match query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => search_page(&state, q, cursor, &filters).await?,
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
    let user_id = crate::auth::caller_portal_identity(&state, &headers)
        .await
        .and_then(|identity| identity.user_id);
    let user_state = match user_id {
        Some(user_id) => library
            .user_title_state(user_id, &detail.id)
            .await
            .map_err(|err| read_failed("GET /api/library/works/{id}", &err))?,
        None => None,
    };
    Ok(Json(detail_json(detail, user_state, &state.covers)))
}

#[cfg(test)]
#[path = "library_works_tests.rs"]
mod tests;
