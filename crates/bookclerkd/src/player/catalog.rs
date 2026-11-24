//! Catalog routes of the player API: the library, its items, series,
//! authors and narrators, search, shelves, stats and covers.
//!
//! Every list goes through the library's page index with the patron's
//! scope, exactly as the library's own pages do, and keeps only audio
//! editions the library acquired (`bookclerk_library::PlayerItem`). An
//! author, series or narrator id is its page index key in URL-safe base64
//! ([`facet_id`]), so the same id works as a filter value.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_library::{
    facet_key, AcquireStatus, ChapterSpan, FacetValue, FacetValuesOf, MediaKind, PlayerItem,
    WorkFacet, WorkFilter, WorkPageQuery, WorkSort,
};
use bookclerk_player_api::ids::{facet_id, is_entity_id, parse_facet_id, WHOLE_LIBRARY_ID};
use bookclerk_player_api::request::{
    parse_batch_get, parse_detail_query, parse_list_query, DetailQuery, ListFilter, ListQuery,
    ListSort, MAX_BATCH_BODY_BYTES, MAX_PAGE_LIMIT,
};
use bookclerk_player_api::time::ms_to_secs;
use bookclerk_player_api::wire::{
    last_first, plain_text, title_ignore_prefix, Author, AuthorSeries, Authors, BookMedia,
    BookMetadata, Chapter, EmptyPage, FilterData, ItemPage, Libraries, Library, LibraryItem,
    LibraryItems, LibrarySettings, LibraryStats, LibraryWithFilterData, MediaProgress, NamedRef,
    Narrator, Narrators, SearchBook, SearchResults, SearchSeries, Series, SeriesPage,
    SeriesProgress, SeriesRef, Shelf, ShelfEntities, MEDIA_TYPE_BOOK,
};
use serde::Serialize;

use super::listening::{
    audio_files_wire, library_files_wire, plan_tracks, progress_by_edition, progress_wire,
    tracks_wire, Planned,
};
use super::sign_in::read_body;
use super::{Patron, PlayerError, PlayerResult};
use crate::api::AppState;

/// The library's name in apps.
const LIBRARY_NAME: &str = "Library";

/// Most values of one facet a list reads.
const MAX_FACET_VALUES: u64 = 5000;

/// Facet values read per page of the index.
const FACET_PAGE: u64 = 500;

/// Most items a series, author or shelf lists.
const MAX_GROUP_ITEMS: u64 = 200;

/// Items a personalized shelf shows.
const SHELF_ITEMS: u64 = 10;

/// Matches of each kind a search returns when the app names no limit.
const DEFAULT_SEARCH_LIMIT: u64 = 12;

/// Most progress rows read to attach progress or build shelves.
const MAX_PROGRESS_ROWS: u64 = 500;

/// Items the stats walk at most, to add up runtimes.
const MAX_STATS_ITEMS: u64 = 20_000;

/// Items per page of the stats walk.
const STATS_PAGE: u64 = 500;

/// A 404 unless `id` names the one library the API lists.
fn require_library(id: &str) -> PlayerResult<()> {
    if id == WHOLE_LIBRARY_ID {
        Ok(())
    } else {
        Err(PlayerError::not_found())
    }
}

/// The page index query every list starts from: acquired audio inside the
/// patron's scope.
fn base_query(patron: &Patron) -> WorkPageQuery {
    WorkPageQuery {
        media_kind: Some(MediaKind::Audio),
        status: Some(AcquireStatus::Acquired),
        scope: patron.scope.clone(),
        ..WorkPageQuery::default()
    }
}

/// The items of `edition_ids` the patron may see, in the given order.
///
/// # Errors
///
/// A 500 when the library cannot be read.
pub(crate) async fn visible_items(
    patron: &Patron,
    edition_ids: &[String],
    route: &str,
) -> PlayerResult<Vec<PlayerItem>> {
    let items = patron
        .library
        .player_items(edition_ids)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?;
    let Some(scope) = patron.scope.as_ref() else {
        return Ok(items);
    };
    let mut visible = Vec::with_capacity(items.len());
    for item in items {
        if patron
            .library
            .work_in_scope(&item.work_id, scope)
            .await
            .map_err(|err| PlayerError::failed(route, &err))?
        {
            visible.push(item);
        }
    }
    Ok(visible)
}

/// The item `id` when the patron may see it.
///
/// # Errors
///
/// A 404 for an unknown, unacquired or hidden item, a 500 when the library
/// cannot be read.
pub(crate) async fn visible_item(
    patron: &Patron,
    id: &str,
    route: &str,
) -> PlayerResult<PlayerItem> {
    if !is_entity_id(id) {
        return Err(PlayerError::not_found());
    }
    visible_items(patron, &[id.to_owned()], route)
        .await?
        .into_iter()
        .next()
        .ok_or_else(PlayerError::not_found)
}

/// Values joined as apps show a list of names.
fn joined<'a>(values: impl Iterator<Item = &'a str>) -> String {
    values.collect::<Vec<_>>().join(", ")
}

/// An item's metadata as apps read it.
pub(crate) fn metadata_wire(item: &PlayerItem) -> BookMetadata {
    let series: Vec<SeriesRef> = item
        .series
        .iter()
        .enumerate()
        .map(|(i, s)| SeriesRef {
            id: facet_id(&s.key),
            name: s.value.clone(),
            sequence: if i == 0 {
                item.series_position.clone()
            } else {
                None
            },
        })
        .collect();
    let series_name = series
        .first()
        .map(|s| match &s.sequence {
            Some(sequence) => format!("{} #{sequence}", s.name),
            None => s.name.clone(),
        })
        .unwrap_or_default();
    let published_year = item
        .published_at
        .as_deref()
        .and_then(|date| date.get(..4))
        .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_owned);
    BookMetadata {
        title: item.title.clone(),
        title_ignore_prefix: title_ignore_prefix(&item.title),
        subtitle: item.subtitle.clone(),
        authors: item
            .authors
            .iter()
            .map(|a| NamedRef {
                id: facet_id(&a.key),
                name: a.value.clone(),
            })
            .collect(),
        author_name: joined(item.authors.iter().map(|a| a.value.as_str())),
        author_name_lf: item
            .authors
            .iter()
            .map(|a| last_first(&a.value))
            .collect::<Vec<_>>()
            .join(", "),
        narrators: item.narrators.iter().map(|n| n.value.clone()).collect(),
        narrator_name: joined(item.narrators.iter().map(|n| n.value.as_str())),
        series,
        series_name,
        genres: item.genres.clone(),
        published_year,
        published_date: item.published_at.clone(),
        publisher: item.publishers.first().cloned(),
        description_plain: item.description.as_deref().map(plain_text),
        description: item.description.clone(),
        isbn: item.isbn.clone(),
        asin: item.asin.clone(),
        language: item.language.clone(),
        explicit: false,
        abridged: item.abridged,
    }
}

/// Metadata for an edition the patron may no longer see: no names.
pub(crate) fn empty_metadata(edition_id: &str) -> BookMetadata {
    let _ = edition_id;
    BookMetadata {
        title: String::new(),
        title_ignore_prefix: String::new(),
        subtitle: None,
        authors: Vec::new(),
        author_name: String::new(),
        author_name_lf: String::new(),
        narrators: Vec::new(),
        narrator_name: String::new(),
        series: Vec::new(),
        series_name: String::new(),
        genres: Vec::new(),
        published_year: None,
        published_date: None,
        publisher: None,
        description: None,
        description_plain: None,
        isbn: None,
        asin: None,
        language: None,
        explicit: false,
        abridged: false,
    }
}

/// The chapters `planned` plays of an edition, as spans ending where the
/// next starts and the last at the copy's length (or, when that is 0, the
/// end of the timeline the list was measured on); empty when it has none.
/// A copy whose length is far from that timeline plays the chapters in its
/// own files ([`bookclerk_library::LibraryStore::chapters_for_copy`]).
///
/// # Errors
///
/// A 500 when the library cannot be read.
pub(crate) async fn chapter_spans(
    patron: &Patron,
    edition_id: &str,
    planned: &Planned,
    route: &str,
) -> PlayerResult<Vec<ChapterSpan>> {
    let resolved = patron
        .library
        .chapters_for_copy(edition_id, planned.destination(), planned.keys())
        .await
        .map_err(|err| PlayerError::failed(route, &err))?;
    Ok(resolved.map_or_else(Vec::new, |resolved| {
        let end = Some(planned.duration_ms())
            .filter(|ms| *ms > 0)
            .or(resolved.runtime_ms);
        resolved.list.spans(end)
    }))
}

/// `spans` as apps read chapters: numbered from 0, in seconds.
pub(crate) fn chapters_wire(spans: &[ChapterSpan]) -> Vec<Chapter> {
    spans
        .iter()
        .enumerate()
        .map(|(index, span)| Chapter {
            id: u64::try_from(index).unwrap_or(u64::MAX),
            start: ms_to_secs(span.start_ms),
            end: ms_to_secs(span.end_ms),
            title: span.title.clone(),
        })
        .collect()
}

/// Puts `spans` on an expanded item: its chapters and their count. An
/// item that is not expanded keeps no chapter list, as apps expect.
pub(crate) fn with_chapters(mut item: LibraryItem, spans: &[ChapterSpan]) -> LibraryItem {
    item.media.num_chapters = u64::try_from(spans.len()).unwrap_or(u64::MAX);
    if item.media.chapters.is_some() {
        item.media.chapters = Some(chapters_wire(spans));
    }
    item
}

/// An item as apps read it; with `planned`, expanded with its tracks and
/// files.
pub(crate) fn item_wire(
    item: &PlayerItem,
    progress: Option<MediaProgress>,
    planned: Option<&Planned>,
) -> LibraryItem {
    let added = item.added_at.timestamp_millis();
    let updated = item.updated_at.timestamp_millis();
    let duration_ms = planned
        .map(Planned::duration_ms)
        .or_else(|| item.runtime_minutes.map(|m| m.saturating_mul(60_000)))
        .unwrap_or(0);
    let size = planned.map_or(0, Planned::size);
    // An acquired audio edition has at least one file; the count is exact
    // once the tracks were read.
    let tracks = planned.map_or(1, |p| u64::try_from(p.tracks.len()).unwrap_or(u64::MAX));
    LibraryItem {
        id: item.edition_id.clone(),
        ino: item.edition_id.clone(),
        old_library_item_id: None,
        library_id: WHOLE_LIBRARY_ID.to_owned(),
        folder_id: WHOLE_LIBRARY_ID.to_owned(),
        path: item.title.clone(),
        rel_path: item.title.clone(),
        is_file: false,
        mtime_ms: updated,
        ctime_ms: added,
        birthtime_ms: added,
        added_at: added,
        updated_at: updated,
        is_missing: false,
        is_invalid: false,
        media_type: MEDIA_TYPE_BOOK,
        media: BookMedia {
            id: item.edition_id.clone(),
            library_item_id: planned.map(|_| item.edition_id.clone()),
            metadata: metadata_wire(item),
            cover_path: Some(format!("/api/items/{}/cover", item.edition_id)),
            tags: Vec::new(),
            num_tracks: tracks,
            num_audio_files: tracks,
            num_chapters: 0,
            duration: ms_to_secs(duration_ms),
            size,
            audio_files: planned.map(|p| audio_files_wire(p, updated)),
            chapters: planned.map(|_| Vec::new()),
            tracks: planned.map(|p| tracks_wire(&item.edition_id, p, updated)),
        },
        num_files: tracks,
        size,
        library_files: planned.map(|p| library_files_wire(p, updated)),
        user_media_progress: progress,
        series_sequence: None,
    }
}

/// The patron's progress rows by edition.
///
/// # Errors
///
/// A 500 when the library cannot be read.
async fn progress_map(
    patron: &Patron,
    route: &str,
) -> PlayerResult<HashMap<String, MediaProgress>> {
    Ok(progress_by_edition(patron, true, MAX_PROGRESS_ROWS, route)
        .await?
        .into_iter()
        .map(|p| (p.edition_id.clone(), progress_wire(patron.user_id(), p)))
        .collect())
}

/// Items as apps read them, with progress from `progress` when given.
fn items_wire(
    items: &[PlayerItem],
    progress: Option<&HashMap<String, MediaProgress>>,
) -> Vec<LibraryItem> {
    items
        .iter()
        .map(|item| {
            item_wire(
                item,
                progress.and_then(|p| p.get(&item.edition_id).cloned()),
                None,
            )
        })
        .collect()
}

/// The library as apps read it.
fn library_wire() -> Library {
    Library {
        id: WHOLE_LIBRARY_ID.to_owned(),
        name: LIBRARY_NAME.to_owned(),
        folders: Vec::new(),
        display_order: 1,
        icon: "audiobookshelf",
        media_type: MEDIA_TYPE_BOOK,
        provider: "audible",
        settings: LibrarySettings {
            cover_aspect_ratio: 1,
            disable_watcher: true,
            audiobooks_only: true,
            hide_single_book_series: false,
            only_show_later_books_in_continue_series: false,
            epubs_allow_scripted_content: false,
            mark_as_finished_percent_complete: None,
            mark_as_finished_time_remaining: 30.0,
        },
        last_scan: None,
        last_scan_version: None,
        created_at: 0,
        last_update: chrono::Utc::now().timestamp_millis(),
    }
}

/// Lists the libraries the patron may open: the one library.
#[utoipa::path(
    get,
    path = "/player/api/libraries",
    responses((status = 200, description = "The library.", body = Libraries)),
)]
pub(crate) async fn list_libraries() -> Json<Libraries> {
    Json(Libraries {
        libraries: vec![library_wire()],
    })
}

/// `GET /player/api/libraries/{id}` answer: the library, with its filter
/// values when asked for (`include=filterdata`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub(crate) enum LibraryAnswer {
    /// With `include=filterdata`.
    WithFilterData(Box<LibraryWithFilterData>),
    /// Otherwise.
    Plain(Box<Library>),
}

/// Reads the library.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}",
    params(
        ("id" = String, Path, description = "Library id."),
        ("include" = Option<String>, Query, description = "`filterdata` to add the filter values."),
    ),
    responses(
        (status = 200, description = "The library.", body = LibraryAnswer),
        (status = 400, description = "A query value is out of range.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_library(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> PlayerResult<Json<LibraryAnswer>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}";
    require_library(&id)?;
    let query = detail(&uri)?;
    if !query.include_filterdata {
        return Ok(Json(LibraryAnswer::Plain(Box::new(library_wire()))));
    }
    Ok(Json(LibraryAnswer::WithFilterData(Box::new(
        LibraryWithFilterData {
            filterdata: filter_data(&patron, ROUTE).await?,
            issues: 0,
            num_user_playlists: 0,
            library: library_wire(),
        },
    ))))
}

/// The query of a detail route, or a 400.
fn detail(uri: &Uri) -> PlayerResult<DetailQuery> {
    parse_detail_query(uri.query().unwrap_or_default()).map_err(|_| PlayerError::bad_request())
}

/// The query of a list route, or a 400.
fn list(uri: &Uri) -> PlayerResult<ListQuery> {
    parse_list_query(uri.query().unwrap_or_default()).map_err(|_| PlayerError::bad_request())
}

/// Every value of `of` among the works the patron may see, up to
/// [`MAX_FACET_VALUES`], by key.
///
/// # Errors
///
/// A 500 when the library cannot be read.
async fn facet_values(
    patron: &Patron,
    of: FacetValuesOf,
    route: &str,
) -> PlayerResult<Vec<FacetValue>> {
    let mut values = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = patron
            .library
            .library_facet_values(of, after.as_deref(), FACET_PAGE, patron.scope.as_ref())
            .await
            .map_err(|err| PlayerError::failed(route, &err))?;
        values.extend(page.values);
        match page.next {
            Some(next) if u64::try_from(values.len()).unwrap_or(u64::MAX) < MAX_FACET_VALUES => {
                after = Some(next);
            }
            _ => break,
        }
    }
    values.truncate(usize::try_from(MAX_FACET_VALUES).unwrap_or(usize::MAX));
    Ok(values)
}

/// Values as the filter data's named list.
fn named(values: &[FacetValue]) -> Vec<NamedRef> {
    values
        .iter()
        .map(|v| NamedRef {
            id: facet_id(&v.key),
            name: v.value.clone(),
        })
        .collect()
}

/// The filter values apps offer.
///
/// # Errors
///
/// A 500 when the library cannot be read.
async fn filter_data(patron: &Patron, route: &str) -> PlayerResult<FilterData> {
    let authors = facet_values(patron, FacetValuesOf::Facet(WorkFacet::Author), route).await?;
    let series = facet_values(patron, FacetValuesOf::Facet(WorkFacet::Series), route).await?;
    let narrators = facet_values(patron, FacetValuesOf::Facet(WorkFacet::Narrator), route).await?;
    let genres = facet_values(patron, FacetValuesOf::Facet(WorkFacet::Genre), route).await?;
    let publishers =
        facet_values(patron, FacetValuesOf::Facet(WorkFacet::Publisher), route).await?;
    let tags = facet_values(patron, FacetValuesOf::Tags(patron.user_id()), route).await?;
    let (_, book_count) = patron
        .library
        .player_items_page(&base_query(patron), 0, 1, false)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?;
    let count = |v: &[FacetValue]| u64::try_from(v.len()).unwrap_or(u64::MAX);
    Ok(FilterData {
        author_count: count(&authors),
        series_count: count(&series),
        authors: named(&authors),
        series: named(&series),
        genres: genres.into_iter().map(|v| v.value).collect(),
        tags: tags.into_iter().map(|v| v.value).collect(),
        narrators: narrators.into_iter().map(|v| v.value).collect(),
        languages: Vec::new(),
        publishers: publishers.into_iter().map(|v| v.value).collect(),
        published_decades: Vec::new(),
        book_count,
        podcast_count: 0,
        number_episodes: 0,
        loaded_at: chrono::Utc::now().timestamp_millis(),
    })
}

/// Reads the library's filter values.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/filterdata",
    params(("id" = String, Path, description = "Library id.")),
    responses(
        (status = 200, description = "The filter values.", body = FilterData),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_library_filterdata(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Json<FilterData>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}/filterdata";
    require_library(&id)?;
    Ok(Json(filter_data(&patron, ROUTE).await?))
}

/// The page index filter an app's filter names, or `None` for one that
/// matches nothing here.
fn work_filter(patron: &Patron, filter: &ListFilter) -> Option<Option<WorkFilter>> {
    let facet = |facet: WorkFacet, value: String| Some(Some(WorkFilter::Facet { facet, value }));
    match filter {
        ListFilter::Author(id) => facet(WorkFacet::Author, parse_facet_id(id)?),
        ListFilter::Series(id) => facet(WorkFacet::Series, parse_facet_id(id)?),
        ListFilter::NoSeries => facet(WorkFacet::Series, String::new()),
        ListFilter::Genre(value) => facet(WorkFacet::Genre, value.clone()),
        ListFilter::Narrator(value) => facet(WorkFacet::Narrator, value.clone()),
        ListFilter::Publisher(value) => facet(WorkFacet::Publisher, value.clone()),
        ListFilter::Tag(tag) => Some(Some(WorkFilter::Tag {
            user_id: Some(patron.user_id()),
            tag: tag.clone(),
        })),
        ListFilter::Finished => Some(Some(WorkFilter::Finished {
            user_id: Some(patron.user_id()),
            finished: true,
        })),
        ListFilter::NotFinished => Some(Some(WorkFilter::Finished {
            user_id: Some(patron.user_id()),
            finished: false,
        })),
        ListFilter::InProgress => Some(None),
        ListFilter::NotStarted | ListFilter::Unsupported => None,
    }
}

/// The page index order an app's sort names, and whether to read it
/// backwards for the app's direction.
fn work_sort(sort: ListSort, descending: bool) -> (WorkSort, bool) {
    match sort {
        ListSort::Title => (WorkSort::Title, descending),
        ListSort::Author => (WorkSort::Author, descending),
        ListSort::Series => (WorkSort::Series, descending),
        // These read newest or longest first by nature.
        ListSort::Added => (WorkSort::Added, !descending),
        ListSort::Duration => (WorkSort::Length, !descending),
    }
}

/// Lists the library's items, one page at a time.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/items",
    params(
        ("id" = String, Path, description = "Library id."),
        ("limit" = Option<u64>, Query, description = "Items per page, at most 1000; 0 for all (up to 1000)."),
        ("page" = Option<u64>, Query, description = "Page number, from 0."),
        ("sort" = Option<String>, Query, description = "`media.metadata.title`, `media.metadata.authorName`, `addedAt`, `media.duration` or `sequence`."),
        ("desc" = Option<u8>, Query, description = "1 to reverse the order."),
        ("filter" = Option<String>, Query, description = "`group.base64value`: authors, series, genres, tags, narrators, publishers or progress."),
        ("include" = Option<String>, Query, description = "`progress` to add the patron's progress."),
        ("minified" = Option<u8>, Query, description = "Accepted; items are always the short shape."),
    ),
    responses(
        (status = 200, description = "A page of items.", body = ItemPage),
        (status = 400, description = "A query value is out of range.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn list_library_items(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> PlayerResult<Json<ItemPage>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}/items";
    require_library(&id)?;
    let query = list(&uri)?;
    let limit = if query.limit == 0 {
        MAX_PAGE_LIMIT
    } else {
        query.limit
    };
    let offset = query.page.saturating_mul(limit);
    let filter = match &query.filter {
        Some(filter) => work_filter(&patron, filter),
        None => Some(None),
    };
    let (items, total) = match filter {
        None => (Vec::new(), 0),
        Some(None) if query.filter == Some(ListFilter::InProgress) => {
            let rows = progress_by_edition(&patron, false, MAX_PROGRESS_ROWS, ROUTE).await?;
            let ids: Vec<String> = rows.into_iter().map(|p| p.edition_id).collect();
            let all = visible_items(&patron, &ids, ROUTE).await?;
            let total = u64::try_from(all.len()).unwrap_or(u64::MAX);
            let page = all
                .into_iter()
                .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .collect();
            (page, total)
        }
        Some(filter) => {
            let (sort, reversed) = work_sort(query.sort, query.descending);
            let page_query = WorkPageQuery {
                sort,
                filter,
                ..base_query(&patron)
            };
            patron
                .library
                .player_items_page(&page_query, offset, limit, reversed)
                .await
                .map_err(|err| PlayerError::failed(ROUTE, &err))?
        }
    };
    let progress = if query.include_progress {
        Some(progress_map(&patron, ROUTE).await?)
    } else {
        None
    };
    Ok(Json(ItemPage {
        results: items_wire(&items, progress.as_ref()),
        total,
        limit,
        page: query.page,
        sort_by: None,
        sort_desc: query.descending,
        filter_by: None,
        media_type: MEDIA_TYPE_BOOK,
        minified: query.minified,
        collapseseries: false,
        include: if query.include_progress {
            "progress".to_owned()
        } else {
            String::new()
        },
        offset,
    }))
}

/// The items of one facet value, in series order.
///
/// # Errors
///
/// A 500 when the library cannot be read.
async fn items_of(
    patron: &Patron,
    facet: WorkFacet,
    key: &str,
    route: &str,
) -> PlayerResult<Vec<PlayerItem>> {
    let query = WorkPageQuery {
        sort: WorkSort::Series,
        filter: Some(WorkFilter::Facet {
            facet,
            value: key.to_owned(),
        }),
        ..base_query(patron)
    };
    Ok(patron
        .library
        .player_items_page(&query, 0, MAX_GROUP_ITEMS, false)
        .await
        .map_err(|err| PlayerError::failed(route, &err))?
        .0)
}

/// A series' items as apps read them, each with its place in the series.
fn series_books(
    key: &str,
    items: &[PlayerItem],
    progress: Option<&HashMap<String, MediaProgress>>,
) -> Vec<LibraryItem> {
    items
        .iter()
        .map(|item| {
            let mut wire = item_wire(
                item,
                progress.and_then(|p| p.get(&item.edition_id).cloned()),
                None,
            );
            if item.series.first().is_some_and(|s| s.key == key) {
                wire.series_sequence = item.series_position.clone();
            }
            wire
        })
        .collect()
}

/// A series as apps read it.
fn series_wire(
    key: &str,
    name: &str,
    items: &[PlayerItem],
    progress: Option<&HashMap<String, MediaProgress>>,
) -> Series {
    let added = items
        .iter()
        .map(|i| i.added_at.timestamp_millis())
        .min()
        .unwrap_or(0);
    let updated = items
        .iter()
        .map(|i| i.updated_at.timestamp_millis())
        .max()
        .unwrap_or(0);
    Series {
        id: facet_id(key),
        name: name.to_owned(),
        name_ignore_prefix: title_ignore_prefix(name),
        description: None,
        added_at: added,
        updated_at: updated,
        library_id: WHOLE_LIBRARY_ID.to_owned(),
        books: Some(series_books(key, items, progress)),
        progress: progress.map(|progress| {
            let ids: Vec<String> = items.iter().map(|i| i.edition_id.clone()).collect();
            let finished: Vec<String> = ids
                .iter()
                .filter(|id| progress.get(*id).is_some_and(|p| p.is_finished))
                .cloned()
                .collect();
            SeriesProgress {
                is_finished: !ids.is_empty() && finished.len() == ids.len(),
                library_item_ids: ids,
                library_item_ids_finished: finished,
            }
        }),
    }
}

/// Lists the library's series, one page at a time.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/series",
    params(
        ("id" = String, Path, description = "Library id."),
        ("limit" = Option<u64>, Query, description = "Series per page; 0 for all (up to 1000)."),
        ("page" = Option<u64>, Query, description = "Page number, from 0."),
        ("desc" = Option<u8>, Query, description = "1 for Z to A."),
    ),
    responses(
        (status = 200, description = "A page of series, each with its items.", body = SeriesPage),
        (status = 400, description = "A query value is out of range.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn list_library_series(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> PlayerResult<Json<SeriesPage>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}/series";
    require_library(&id)?;
    let query = list(&uri)?;
    let limit = if query.limit == 0 {
        MAX_PAGE_LIMIT
    } else {
        query.limit
    };
    let mut values = facet_values(&patron, FacetValuesOf::Facet(WorkFacet::Series), ROUTE).await?;
    if query.descending {
        values.reverse();
    }
    let total = u64::try_from(values.len()).unwrap_or(u64::MAX);
    let progress = if query.include_progress {
        Some(progress_map(&patron, ROUTE).await?)
    } else {
        None
    };
    let mut results = Vec::new();
    for value in values
        .iter()
        .skip(usize::try_from(query.page.saturating_mul(limit)).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
    {
        let items = items_of(&patron, WorkFacet::Series, &value.key, ROUTE).await?;
        results.push(series_wire(
            &value.key,
            &value.value,
            &items,
            progress.as_ref(),
        ));
    }
    Ok(Json(SeriesPage {
        results,
        total,
        limit,
        page: query.page,
        sort_by: None,
        sort_desc: query.descending,
        filter_by: None,
        minified: query.minified,
        include: if query.include_progress {
            "progress".to_owned()
        } else {
            String::new()
        },
    }))
}

/// Reads one series with its items and the patron's progress through it.
#[utoipa::path(
    get,
    path = "/player/api/series/{id}",
    params(("id" = String, Path, description = "Series id.")),
    responses(
        (status = 200, description = "The series.", body = Series),
        (status = 404, description = "No such series the patron may see.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_series(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Json<Series>> {
    const ROUTE: &str = "GET /player/api/series/{id}";
    let key = parse_facet_id(&id).ok_or_else(PlayerError::not_found)?;
    let items = items_of(&patron, WorkFacet::Series, &key, ROUTE).await?;
    let name = items
        .iter()
        .flat_map(|i| i.series.iter())
        .find(|s| s.key == key)
        .map(|s| s.value.clone())
        .ok_or_else(PlayerError::not_found)?;
    let progress = progress_map(&patron, ROUTE).await?;
    Ok(Json(series_wire(&key, &name, &items, Some(&progress))))
}

/// An author as apps read them.
fn author_wire(value: &FacetValue) -> Author {
    Author {
        id: facet_id(&value.key),
        asin: None,
        name: value.value.clone(),
        last_first: last_first(&value.value),
        description: None,
        image_path: None,
        library_id: WHOLE_LIBRARY_ID.to_owned(),
        added_at: 0,
        updated_at: 0,
        num_books: value.works,
        library_items: None,
        series: None,
    }
}

/// Lists the library's authors.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/authors",
    params(("id" = String, Path, description = "Library id.")),
    responses(
        (status = 200, description = "Every author, by name (up to 5000).", body = Authors),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn list_library_authors(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Json<Authors>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}/authors";
    require_library(&id)?;
    let values = facet_values(&patron, FacetValuesOf::Facet(WorkFacet::Author), ROUTE).await?;
    Ok(Json(Authors {
        authors: values.iter().map(author_wire).collect(),
    }))
}

/// Reads one author, with their items and series when asked for.
#[utoipa::path(
    get,
    path = "/player/api/authors/{id}",
    params(
        ("id" = String, Path, description = "Author id."),
        ("include" = Option<String>, Query, description = "`items` and/or `series`."),
    ),
    responses(
        (status = 200, description = "The author.", body = Author),
        (status = 400, description = "A query value is out of range.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such author the patron may see.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_author(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> PlayerResult<Json<Author>> {
    const ROUTE: &str = "GET /player/api/authors/{id}";
    let key = parse_facet_id(&id).ok_or_else(PlayerError::not_found)?;
    let query = detail(&uri)?;
    let items = items_of(&patron, WorkFacet::Author, &key, ROUTE).await?;
    let name = items
        .iter()
        .flat_map(|i| i.authors.iter())
        .find(|a| a.key == key)
        .map(|a| a.value.clone())
        .ok_or_else(PlayerError::not_found)?;
    let mut author = author_wire(&FacetValue {
        key: key.clone(),
        value: name,
        works: u64::try_from(items.len()).unwrap_or(u64::MAX),
    });
    if query.include_series {
        let mut series: Vec<AuthorSeries> = Vec::new();
        for item in &items {
            let Some(first) = item.series.first() else {
                continue;
            };
            let id = facet_id(&first.key);
            let mut wire = item_wire(item, None, None);
            wire.series_sequence = item.series_position.clone();
            match series.iter_mut().find(|s| s.id == id) {
                Some(entry) => entry.items.push(wire),
                None => series.push(AuthorSeries {
                    id,
                    name: first.value.clone(),
                    items: vec![wire],
                }),
            }
        }
        author.series = Some(series);
    }
    if query.include_items {
        author.library_items = Some(items_wire(&items, None));
    }
    Ok(Json(author))
}

/// Lists the library's narrators.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/narrators",
    params(("id" = String, Path, description = "Library id.")),
    responses(
        (status = 200, description = "Every narrator, by name (up to 5000).", body = Narrators),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn list_library_narrators(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Json<Narrators>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}/narrators";
    require_library(&id)?;
    let values = facet_values(&patron, FacetValuesOf::Facet(WorkFacet::Narrator), ROUTE).await?;
    Ok(Json(Narrators {
        narrators: values
            .into_iter()
            .map(|v| Narrator {
                id: facet_id(&v.key),
                name: v.value,
                num_books: v.works,
            })
            .collect(),
    }))
}

/// An empty page echoing the app's paging.
fn empty_page(uri: &Uri) -> PlayerResult<EmptyPage> {
    let query = list(uri)?;
    Ok(EmptyPage {
        results: Vec::new(),
        total: 0,
        limit: query.limit,
        page: query.page,
    })
}

/// Lists the library's collections: none yet.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/collections",
    params(("id" = String, Path, description = "Library id.")),
    responses(
        (status = 200, description = "An empty page.", body = EmptyPage),
        (status = 400, description = "A query value is out of range.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn list_library_collections(
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> PlayerResult<Json<EmptyPage>> {
    require_library(&id)?;
    Ok(Json(empty_page(&uri)?))
}

/// Lists the patron's playlists in the library: none yet.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/playlists",
    params(("id" = String, Path, description = "Library id.")),
    responses(
        (status = 200, description = "An empty page.", body = EmptyPage),
        (status = 400, description = "A query value is out of range.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn list_library_playlists(
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> PlayerResult<Json<EmptyPage>> {
    require_library(&id)?;
    Ok(Json(empty_page(&uri)?))
}

/// A shelf of items.
fn book_shelf(
    id: &'static str,
    label: &'static str,
    key: &'static str,
    items: Vec<LibraryItem>,
) -> Shelf {
    Shelf {
        id,
        label,
        label_string_key: key,
        kind: "book",
        total: u64::try_from(items.len()).unwrap_or(u64::MAX),
        entities: ShelfEntities::Books(items),
    }
}

/// Reads the patron's home shelves: continue listening, recently added
/// and listen again.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/personalized",
    params(("id" = String, Path, description = "Library id.")),
    responses(
        (status = 200, description = "The shelves that have items.", body = Vec<Shelf>),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_library_personalized(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Json<Vec<Shelf>>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}/personalized";
    require_library(&id)?;
    let rows = progress_by_edition(&patron, true, MAX_PROGRESS_ROWS, ROUTE).await?;
    let progress: HashMap<String, MediaProgress> = rows
        .iter()
        .map(|p| {
            (
                p.edition_id.clone(),
                progress_wire(patron.user_id(), p.clone()),
            )
        })
        .collect();
    let shelf_len = usize::try_from(SHELF_ITEMS).unwrap_or(usize::MAX);
    let (finished, started): (Vec<_>, Vec<_>) = rows.iter().partition(|p| p.is_finished);
    let ids = |rows: Vec<&bookclerk_library::PlaybackProgress>| -> Vec<String> {
        rows.into_iter()
            .take(shelf_len)
            .map(|p| p.edition_id.clone())
            .collect()
    };
    let continuing = visible_items(&patron, &ids(started), ROUTE).await?;
    let again = visible_items(&patron, &ids(finished), ROUTE).await?;
    let recent_query = WorkPageQuery {
        sort: WorkSort::Added,
        ..base_query(&patron)
    };
    let (recent, _) = patron
        .library
        .player_items_page(&recent_query, 0, SHELF_ITEMS, false)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    let shelves = [
        (
            "continue-listening",
            "Continue Listening",
            "LabelContinueListening",
            continuing,
        ),
        (
            "recently-added",
            "Recently Added",
            "LabelRecentlyAdded",
            recent,
        ),
        ("listen-again", "Listen Again", "LabelListenAgain", again),
    ]
    .into_iter()
    .filter(|(_, _, _, items)| !items.is_empty())
    .map(|(id, label, key, items)| book_shelf(id, label, key, items_wire(&items, Some(&progress))))
    .collect();
    Ok(Json(shelves))
}

/// Searches the library's items, series, authors and narrators.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/search",
    params(
        ("id" = String, Path, description = "Library id."),
        ("q" = String, Query, description = "The search terms."),
        ("limit" = Option<u64>, Query, description = "Matches of each kind, default 12."),
    ),
    responses(
        (status = 200, description = "The matches.", body = SearchResults),
        (status = 400, description = "A query value is out of range, or the terms do not parse.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
        (status = 503, description = "The search index is being built; retry.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn search_library(
    State(state): State<Arc<AppState>>,
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> PlayerResult<Json<SearchResults>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}/search";
    require_library(&id)?;
    let query = detail(&uri)?;
    let limit = query.limit.unwrap_or(DEFAULT_SEARCH_LIMIT).max(1);
    let take = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut results = SearchResults {
        book: Vec::new(),
        podcast: Vec::new(),
        narrators: Vec::new(),
        tags: Vec::new(),
        genres: Vec::new(),
        series: Vec::new(),
        authors: Vec::new(),
    };
    let Some(q) = query.search else {
        return Ok(Json(results));
    };
    let filters = WorkPageQuery {
        limit,
        ..base_query(&patron)
    };
    let (rows, _) = crate::library_works::search_page(
        &state,
        &patron.library,
        &q,
        crate::library_works::SearchModeParam::Words,
        None,
        &filters,
    )
    .await
    .map_err(|(status, _)| match status {
        StatusCode::BAD_REQUEST => PlayerError::bad_request(),
        StatusCode::SERVICE_UNAVAILABLE => PlayerError::unavailable(),
        _ => PlayerError::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
    })?;
    let work_ids: Vec<String> = rows.into_iter().map(|r| r.work_id).collect();
    let editions = patron
        .library
        .player_editions_of_works(&work_ids)
        .await
        .map_err(|err| PlayerError::failed(ROUTE, &err))?;
    let edition_ids: Vec<String> = work_ids
        .iter()
        .filter_map(|w| editions.get(w).cloned())
        .collect();
    let items = visible_items(&patron, &edition_ids, ROUTE).await?;
    results.book = items_wire(&items, None)
        .into_iter()
        .map(|library_item| SearchBook { library_item })
        .collect();
    let needle = facet_key(&q);
    let matching = |values: Vec<FacetValue>| -> Vec<FacetValue> {
        values
            .into_iter()
            .filter(|v| v.key.contains(&needle))
            .take(take)
            .collect()
    };
    let authors =
        matching(facet_values(&patron, FacetValuesOf::Facet(WorkFacet::Author), ROUTE).await?);
    results.authors = authors.iter().map(author_wire).collect();
    let narrators =
        matching(facet_values(&patron, FacetValuesOf::Facet(WorkFacet::Narrator), ROUTE).await?);
    results.narrators = narrators
        .into_iter()
        .map(|v| Narrator {
            id: facet_id(&v.key),
            name: v.value,
            num_books: v.works,
        })
        .collect();
    let series =
        matching(facet_values(&patron, FacetValuesOf::Facet(WorkFacet::Series), ROUTE).await?);
    for value in series {
        let items = items_of(&patron, WorkFacet::Series, &value.key, ROUTE).await?;
        let mut wire = series_wire(&value.key, &value.value, &items, None);
        let books = wire.books.take().unwrap_or_default();
        results.series.push(SearchSeries {
            series: wire,
            books,
        });
    }
    Ok(Json(results))
}

/// Adds up the library's items and their runtimes.
#[utoipa::path(
    get,
    path = "/player/api/libraries/{id}/stats",
    params(("id" = String, Path, description = "Library id.")),
    responses(
        (status = 200, description = "Totals over the items the patron may see (runtimes over the first 20000).", body = LibraryStats),
        (status = 404, description = "No such library.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_library_stats(
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Json<LibraryStats>> {
    const ROUTE: &str = "GET /player/api/libraries/{id}/stats";
    require_library(&id)?;
    let query = base_query(&patron);
    let mut offset = 0u64;
    let mut minutes = 0u64;
    let total_items = loop {
        let (items, total) = patron
            .library
            .player_items_page(&query, offset, STATS_PAGE, false)
            .await
            .map_err(|err| PlayerError::failed(ROUTE, &err))?;
        minutes =
            minutes.saturating_add(items.iter().filter_map(|i| i.runtime_minutes).sum::<u64>());
        offset = offset.saturating_add(STATS_PAGE);
        if offset >= total || offset >= MAX_STATS_ITEMS {
            break total;
        }
    };
    let authors = facet_values(&patron, FacetValuesOf::Facet(WorkFacet::Author), ROUTE).await?;
    let genres = facet_values(&patron, FacetValuesOf::Facet(WorkFacet::Genre), ROUTE).await?;
    Ok(Json(LibraryStats {
        total_items,
        total_authors: u64::try_from(authors.len()).unwrap_or(u64::MAX),
        total_genres: u64::try_from(genres.len()).unwrap_or(u64::MAX),
        total_duration: ms_to_secs(minutes.saturating_mul(60_000)),
        num_audio_tracks: total_items,
        total_size: 0,
    }))
}

/// Reads one item; expanded, with its tracks and files.
#[utoipa::path(
    get,
    path = "/player/api/items/{id}",
    params(
        ("id" = String, Path, description = "Item id."),
        ("expanded" = Option<u8>, Query, description = "1 to add tracks and files."),
        ("include" = Option<String>, Query, description = "`progress` to add the patron's progress."),
    ),
    responses(
        (status = 200, description = "The item.", body = LibraryItem),
        (status = 400, description = "A query value is out of range.", body = String, content_type = "text/plain"),
        (status = 404, description = "No such item the patron may see.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_item(
    State(state): State<Arc<AppState>>,
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
    uri: Uri,
) -> PlayerResult<Json<LibraryItem>> {
    const ROUTE: &str = "GET /player/api/items/{id}";
    let query = detail(&uri)?;
    let item = visible_item(&patron, &id, ROUTE).await?;
    let progress = if query.include_progress {
        patron
            .library
            .playback_progress(patron.user_id(), &item.edition_id)
            .await
            .map_err(|err| PlayerError::failed(ROUTE, &err))?
            .map(|p| progress_wire(patron.user_id(), p))
    } else {
        None
    };
    let planned = if query.expanded {
        match plan_tracks(&state, &patron, &item, ROUTE).await {
            Ok(planned) => planned,
            // Storage that cannot be asked right now leaves the item
            // without tracks rather than failing the read.
            Err(err) if err.status() == StatusCode::SERVICE_UNAVAILABLE => None,
            Err(err) => return Err(err),
        }
    } else {
        None
    };
    let wire = item_wire(&item, progress, planned.as_ref());
    match planned {
        Some(planned) => {
            let spans = chapter_spans(&patron, &item.edition_id, &planned, ROUTE).await?;
            Ok(Json(with_chapters(wire, &spans)))
        }
        None => Ok(Json(wire)),
    }
}

/// Reads several items at once.
#[utoipa::path(
    post,
    path = "/player/api/items/batch/get",
    request_body(content = super::bodies::BatchGetBody, description = "The items to read, at most 100."),
    responses(
        (status = 200, description = "The items the patron may see, in the given order.", body = LibraryItems),
        (status = 400, description = "The body is malformed.", body = String, content_type = "text/plain"),
        (status = 413, description = "The body is too long.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn post_items_batch_get(
    Extension(patron): Extension<Patron>,
    body: Body,
) -> PlayerResult<Json<LibraryItems>> {
    const ROUTE: &str = "POST /player/api/items/batch/get";
    let body = read_body(body, MAX_BATCH_BODY_BYTES).await?;
    let ids = parse_batch_get(&body).map_err(|_| PlayerError::bad_request())?;
    let items = visible_items(&patron, &ids, ROUTE).await?;
    Ok(Json(LibraryItems {
        library_items: items_wire(&items, None),
    }))
}

/// Serves an item's cover.
#[utoipa::path(
    get,
    path = "/player/api/items/{id}/cover",
    params(("id" = String, Path, description = "Item id.")),
    responses(
        (status = 200, description = "The cover.", content_type = "image/jpeg"),
        (status = 404, description = "No such item, or no cover of it on this node.", body = String, content_type = "text/plain"),
        (status = 500, description = "The library or the cover could not be read.", body = String, content_type = "text/plain"),
    ),
)]
pub(crate) async fn get_item_cover(
    State(state): State<Arc<AppState>>,
    Extension(patron): Extension<Patron>,
    AxumPath(id): AxumPath<String>,
) -> PlayerResult<Response> {
    const ROUTE: &str = "GET /player/api/items/{id}/cover";
    let item = visible_item(&patron, &id, ROUTE).await?;
    match crate::library_works::edition_cover_bytes(&state, &patron.library, &item.edition_id).await
    {
        Ok(Some(bytes)) => Ok((
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            bytes,
        )
            .into_response()),
        Ok(None) => Err(PlayerError::not_found()),
        Err(crate::library_works::CoverReadError::Library(err)) => {
            Err(PlayerError::failed(ROUTE, &err))
        }
        Err(crate::library_works::CoverReadError::Storage) => Err(PlayerError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
        )),
    }
}
