//! The library by meaning: `GET /api/library/works?q=…&search=meaning`
//! finds works whose description is about the query,
//! `GET /api/library/works/{id}/similar` lists the works most like one,
//! and `GET /api/library/works/meaning` says whether each is ready.
//!
//! Both read this node's index of work vectors
//! ([`crate::work_vectors`]) in the space that serves the library. A
//! search also needs the library's embedding model on this node, to turn
//! the query into a vector of that space; "more like this" compares two
//! stored vectors and needs no model here. Neither is offered on the hash
//! stand-in, whose vectors carry no meaning. The daemon loads no model:
//! with no model plugin the routes say so, and the search by words is
//! unchanged. See `docs/internals/discovery.md`, *Searching by meaning*.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use bookclerk_library::{VectorSpace, WorkPageQuery};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::embedding::ModelEmbedder;
use crate::http_error::ErrorMessage;
use crate::library_works::{
    caller_scope, read_failed, require_in_scope, summaries, WorkSummaryJson, WorksError,
};
use crate::work_vectors::{standing_out, WorkMatrix, MATCH_STANDING};

/// How long a search waits for the model to embed its query. A sleeping
/// model plugin first loads the model, which takes seconds on a small
/// machine.
const QUERY_DEADLINE: Duration = Duration::from_secs(15);

/// Longest query a search by meaning takes, in bytes. A query is a
/// phrase or a sentence; the model reads no more than a passage anyway.
const MAX_QUERY_BYTES: usize = 1024;

/// Similar works listed when the request names no `limit`.
const DEFAULT_SIMILAR: usize = 12;

/// Most similar works one request lists.
const MAX_SIMILAR: usize = 40;

/// Nearest works looked up at a time for one similar-works answer: more
/// than it lists, since the caller's scope and unheld works drop some.
const SIMILAR_WINDOW: usize = 200;

/// The share of the library's works (numerator, denominator) a model's
/// space must hold for these routes to read it before it serves.
const NEARLY_ALL: (u64, u64) = (9, 10);

/// Whether a way of reading the library by meaning can be used now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MeaningState {
    /// It answers.
    Ready,
    /// No embedding model is there for it: none is installed or its
    /// plugin is off (for a search, on the node that answered).
    NoModel,
    /// The library is still being embedded with the model.
    Embedding,
}

/// Answer of `GET /api/library/works/meaning`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct MeaningStatus {
    /// Whether `search=meaning` answers on this node.
    search: MeaningState,
    /// Whether `GET /api/library/works/{id}/similar` lists works.
    similar: MeaningState,
}

/// Answer of `GET /api/library/works/{id}/similar`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct SimilarWorks {
    /// `ready` when `works` was looked up; otherwise why it is empty.
    state: MeaningState,
    /// The held works most like the one asked about, most alike first.
    /// Empty when the work has no vector yet.
    works: Vec<WorkSummaryJson>,
}

/// Query string of `GET /api/library/works/{id}/similar`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct SimilarQuery {
    /// Most works to list, clamped to 1–40 (default 12).
    limit: Option<usize>,
}

/// What this node can do by meaning right now.
pub(crate) struct Standing {
    /// This node's embedding model, when a plugin serves one here.
    pub(crate) model: Option<ModelEmbedder>,
    /// The space that serves the library's rankings.
    pub(crate) serving: VectorSpace,
    /// The space these routes read: `serving`, or a model's space that
    /// misses only the newest works while the stand-in serves.
    pub(crate) reading: VectorSpace,
}

/// The model space to read while the stand-in serves: one that holds a
/// vector for at least [`NEARLY_ALL`] of the works. A ranking wants every
/// work embedded; a search that misses the books scanned a minute ago is
/// better than no search until their embedding ends.
///
/// # Arguments
///
/// * `library` - The library store.
/// * `current` - The space of this node's model, preferred when it
///   qualifies; otherwise the space written most recently.
///
/// # Errors
///
/// Returns the library read error.
async fn nearly_covering(
    library: &bookclerk_library::LibraryStore,
    current: &VectorSpace,
) -> bookclerk_library::Result<Option<VectorSpace>> {
    let works = library.count_works().await?;
    let mut spaces: Vec<_> = library
        .work_vector_spaces()
        .await?
        .into_iter()
        .filter(|coverage| {
            !bookclerk_discover::is_stand_in(&coverage.space)
                && coverage.works > 0
                && coverage.works.saturating_mul(NEARLY_ALL.1) >= works.saturating_mul(NEARLY_ALL.0)
        })
        .collect();
    spaces.sort_by_key(|coverage| {
        (
            !coverage.space.same_as(current),
            std::cmp::Reverse(coverage.newest),
        )
    });
    Ok(spaces.into_iter().next().map(|coverage| coverage.space))
}

impl Standing {
    /// Reads the node's model and the spaces.
    ///
    /// # Errors
    ///
    /// A 500 when the library cannot be read.
    pub(crate) async fn read(
        state: &AppState,
        library: &bookclerk_library::LibraryStore,
        route: &str,
    ) -> Result<Self, WorksError> {
        let failed = |err: &dyn std::fmt::Display| {
            tracing::error!(error = %err, route, "could not read the vector spaces");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new("the library could not be read")),
            )
        };
        let router = Arc::clone(&state.deployment_runtime().ai);
        let model = ModelEmbedder::current(&router).await;
        let current = model
            .as_ref()
            .map_or_else(bookclerk_discover::stand_in_space, |model| {
                model.vector_space().clone()
            });
        let serving = bookclerk_discover::serving_space(library, &current)
            .await
            .map_err(|err| failed(&err))?;
        let reading = if bookclerk_discover::is_stand_in(&serving) {
            nearly_covering(library, &current)
                .await
                .map_err(|err| failed(&err))?
                .unwrap_or_else(|| serving.clone())
        } else {
            serving.clone()
        };
        Ok(Self {
            model,
            serving,
            reading,
        })
    }

    /// Whether a search by meaning answers here.
    pub(crate) fn search(&self) -> MeaningState {
        match &self.model {
            None => MeaningState::NoModel,
            Some(model) if model.vector_space().same_as(&self.reading) => MeaningState::Ready,
            Some(_) => MeaningState::Embedding,
        }
    }

    /// Whether similar works can be listed.
    pub(crate) fn similar(&self) -> MeaningState {
        if !bookclerk_discover::is_stand_in(&self.reading) {
            MeaningState::Ready
        } else if self.model.is_some() {
            MeaningState::Embedding
        } else {
            MeaningState::NoModel
        }
    }
}

/// A 503 answer with `message`.
pub(crate) fn unavailable(message: &str) -> WorksError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorMessage::new(message)),
    )
}

/// The index of the serving space.
///
/// # Errors
///
/// A 500 when the vectors cannot be read.
pub(crate) async fn matrix(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    serving: &VectorSpace,
    route: &str,
) -> Result<Arc<WorkMatrix>, WorksError> {
    state
        .work_vectors
        .current(library, serving)
        .await
        .map_err(|err| read_failed(route, &err))
}

/// The works that match `q` by meaning, best first: every work that
/// stands out from the rest of the library for this query
/// ([`MATCH_STANDING`]).
///
/// # Errors
///
/// A 400 for a query over [`MAX_QUERY_BYTES`]; a 503 when this node has
/// no embedding model, the library is not embedded with it yet, or the
/// model did not answer in time; a 500 when the library cannot be read.
pub(crate) async fn matches(
    state: &AppState,
    library: &bookclerk_library::LibraryStore,
    q: &str,
) -> Result<Vec<String>, WorksError> {
    /// The route these errors are logged under.
    const ROUTE: &str = "GET /api/library/works";
    if q.len() > MAX_QUERY_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorMessage::new(format!(
                "a search by meaning takes at most {MAX_QUERY_BYTES} bytes"
            ))),
        ));
    }
    let standing = Standing::read(state, library, ROUTE).await?;
    if let Some(model) = &standing.model {
        // Works without a vector in the model's space are embedded in a
        // job; make sure one is queued.
        crate::embedding::embed_if_needed(state, model.vector_space(), &standing.serving).await;
    }
    let model = match (standing.search(), standing.model) {
        (MeaningState::Ready, Some(model)) => model,
        (MeaningState::Embedding, Some(_)) => {
            return Err(unavailable(
                "the library is still being prepared for search by meaning; try again later",
            ));
        }
        _ => {
            return Err(unavailable(
                "search by meaning needs an embedding model on this node",
            ))
        }
    };
    let space = model.vector_space().clone();
    let query = match state.work_vectors.cached_query(&space, q).await {
        Some(vector) => vector,
        None => {
            let vector = model.embed_query(q, QUERY_DEADLINE).await.map_err(|err| {
                tracing::warn!(error = %err, "GET /api/library/works: could not embed the query");
                unavailable("the embedding model did not answer; try again shortly")
            })?;
            let vector = Arc::new(vector);
            state
                .work_vectors
                .cache_query(&space, q, Arc::clone(&vector))
                .await;
            vector
        }
    };
    let matrix = matrix(state, library, &space, ROUTE).await?;
    Ok(standing_out(matrix.ranked(&query, None), MATCH_STANDING)
        .into_iter()
        .map(|scored| scored.work_id)
        .collect())
}

/// Refuses a work the caller cannot see: one outside the scope of
/// `filters` answers as one that does not exist, as the work's own route
/// does.
///
/// # Errors
///
/// A 404 for a work that does not exist or is outside the scope; a 500
/// when the library cannot be read.
pub(crate) async fn require_visible_work(
    library: &bookclerk_library::LibraryStore,
    filters: &WorkPageQuery,
    id: &str,
    route: &str,
) -> Result<(), WorksError> {
    if !library
        .library_works_by_ids(&[id.to_string()], filters)
        .await
        .map_err(|err| read_failed(route, &err))?
        .is_empty()
    {
        return Ok(());
    }
    // Held by nobody (no row in the page index) or outside the scope.
    require_in_scope(
        library,
        filters.scope.as_ref(),
        id,
        bookclerk_library::EntityKind::Work,
        id,
        route,
    )
    .await?;
    if library
        .library_work_detail(id)
        .await
        .map_err(|err| read_failed(route, &err))?
        .is_none()
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorMessage::new(
                bookclerk_library::LibraryError::not_found(bookclerk_library::EntityKind::Work, id),
            )),
        ));
    }
    Ok(())
}

/// Says whether search by meaning and similar works are ready.
///
/// A client asks before it offers either. `search` is about the node
/// that answers: it needs the library's embedding model there.
#[utoipa::path(
    get,
    path = "/api/library/works/meaning",
    responses(
        (status = 200, description = "What is ready.", body = MeaningStatus),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn get_meaning_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<MeaningStatus>, WorksError> {
    let library = state.library_snapshot().await;
    let standing = Standing::read(&state, &library, "GET /api/library/works/meaning").await?;
    Ok(Json(MeaningStatus {
        search: standing.search(),
        similar: standing.similar(),
    }))
}

/// Lists the held works most like one work.
///
/// Likeness is how close the works' descriptions are in meaning, as the
/// library's embedding model reads them. The caller sees only works
/// inside their own scope.
#[utoipa::path(
    get,
    path = "/api/library/works/{id}/similar",
    params(
        ("id" = String, Path, description = "Work id (a podcast's show)."),
        SimilarQuery,
    ),
    responses(
        (status = 200, description = "The similar works, or why there are none.", body = SimilarWorks),
        (status = 404, description = "No such work.", body = ErrorMessage, example = json!({"error": "work 0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b not found"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
    ),
)]
pub(crate) async fn list_similar_works(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SimilarQuery>,
) -> Result<Json<SimilarWorks>, WorksError> {
    /// The route these errors are logged under.
    const ROUTE: &str = "GET /api/library/works/{id}/similar";
    let library = state.library_snapshot().await;
    let scope = caller_scope(&state, &library, &headers, ROUTE).await?;
    let filters = WorkPageQuery {
        scope,
        limit: 1,
        ..WorkPageQuery::default()
    };
    require_visible_work(&library, &filters, &id, ROUTE).await?;
    let standing = Standing::read(&state, &library, ROUTE).await?;
    let similar = standing.similar();
    if similar != MeaningState::Ready {
        return Ok(Json(SimilarWorks {
            state: similar,
            works: Vec::new(),
        }));
    }
    let matrix = matrix(&state, &library, &standing.reading, ROUTE).await?;
    let ranked = match matrix.vector_of(&id) {
        Some(vector) => matrix.ranked(vector, Some(&id)),
        None => Vec::new(),
    };
    // The scope and unheld works drop some of the nearest, so read them a
    // window at a time, nearest first, until the list is full.
    let wanted = query.limit.unwrap_or(DEFAULT_SIMILAR).clamp(1, MAX_SIMILAR);
    // Sized by the constant, not by the number the caller sent.
    let mut rows = Vec::with_capacity(MAX_SIMILAR);
    for window in ranked.chunks(SIMILAR_WINDOW) {
        let ids: Vec<String> = window.iter().map(|s| s.work_id.clone()).collect();
        rows.extend(
            library
                .library_works_by_ids(&ids, &filters)
                .await
                .map_err(|err| read_failed(ROUTE, &err))?,
        );
        if rows.len() >= wanted {
            break;
        }
    }
    rows.truncate(wanted);
    Ok(Json(SimilarWorks {
        state: MeaningState::Ready,
        works: summaries(&state, &library, rows, ROUTE).await?,
    }))
}

#[cfg(test)]
mod tests {
    use bookclerk_library::{JobKind, JobState, LibraryStore};

    use super::*;
    use crate::embedding::tests::{daemon, payload, FakeModel};
    use crate::library_works::{list_works, SearchModeParam, WorksPage, WorksQuery};

    /// Titles of the test library: two about dragons, as the fake model
    /// reads them, and ten about something else.
    const TITLES: [&str; 12] = [
        "A Dragon Tale",
        "The Dragon Returns",
        "Bread at Home",
        "Knots for Sailors",
        "A Short History of Salt",
        "Gardening in Shade",
        "The Night Train",
        "Letters from Oslo",
        "Maps of the Tide",
        "The Clockmaker",
        "Winter Beekeeping",
        "A Year of Soup",
    ];

    /// A daemon over [`TITLES`], every work in the page index.
    async fn library(
        files: &tempfile::TempDir,
        model: Option<&Arc<FakeModel>>,
    ) -> (Arc<AppState>, LibraryStore) {
        let (state, library) = daemon(files, &TITLES, model).await;
        let ids: Vec<String> = library
            .list_works_page(None, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|work| work.id)
            .collect();
        library.refresh_library_works(&ids).await.unwrap();
        (state, library)
    }

    /// A search by meaning for `q`, `limit` works from `cursor`.
    fn by_meaning(q: &str, limit: Option<u64>, cursor: Option<String>) -> WorksQuery {
        let mut query = WorksQuery::default();
        query.q = Some(q.into());
        query.search = Some(SearchModeParam::Meaning);
        query.limit = limit;
        query.cursor = cursor;
        query
    }

    /// Lists works with `query`: the page, or the refusal.
    async fn list(
        state: &Arc<AppState>,
        query: WorksQuery,
    ) -> Result<WorksPage, (StatusCode, String)> {
        list_works(State(state.clone()), HeaderMap::new(), Query(query))
            .await
            .map(|Json(page)| page)
            .map_err(|(status, Json(body))| (status, body.error))
    }

    /// What `GET /api/library/works/meaning` says.
    async fn status(state: &Arc<AppState>) -> (MeaningState, MeaningState) {
        let Json(status) = get_meaning_status(State(state.clone())).await.unwrap();
        (status.search, status.similar)
    }

    /// The works similar to `id`.
    async fn similar(state: &Arc<AppState>, id: &str, limit: Option<usize>) -> SimilarWorks {
        list_similar_works(
            State(state.clone()),
            HeaderMap::new(),
            AxumPath(id.to_string()),
            Query(SimilarQuery { limit }),
        )
        .await
        .map(|Json(similar)| similar)
        .map_err(|(status, Json(body))| (status, body.error))
        .unwrap()
    }

    /// The id of the work titled `title`.
    async fn work_titled(state: &Arc<AppState>, title: &str) -> String {
        list(state, WorksQuery::default())
            .await
            .unwrap()
            .works
            .into_iter()
            .find(|work| work.title == title)
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn without_a_model_nothing_is_read_by_meaning() {
        let files = tempfile::tempdir().unwrap();
        let (state, _library) = library(&files, None).await;
        assert_eq!(
            status(&state).await,
            (MeaningState::NoModel, MeaningState::NoModel)
        );
        let (code, message) = list(&state, by_meaning("dragons", None, None))
            .await
            .unwrap_err();
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(message.contains("needs an embedding model"), "{message}");
        let bread = work_titled(&state, "Bread at Home").await;
        let answer = similar(&state, &bread, None).await;
        assert_eq!(answer.state, MeaningState::NoModel);
        assert!(answer.works.is_empty());
    }

    #[tokio::test]
    async fn a_library_not_embedded_yet_says_so_and_queues_the_embedding() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = library(&files, Some(&model)).await;
        assert_eq!(
            status(&state).await,
            (MeaningState::Embedding, MeaningState::Embedding)
        );
        let (code, message) = list(&state, by_meaning("dragons", None, None))
            .await
            .unwrap_err();
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(message.contains("still being prepared"), "{message}");
        let queued = library
            .list_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == JobKind::DiscoverEmbed && job.state == JobState::Pending)
            .count();
        assert_eq!(queued, 1, "the search asked for the embedding");
        let bread = work_titled(&state, "Bread at Home").await;
        assert_eq!(
            similar(&state, &bread, None).await.state,
            MeaningState::Embedding
        );
    }

    #[tokio::test]
    async fn a_search_by_meaning_lists_what_stands_out_and_pages_it() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, _library) = library(&files, Some(&model)).await;
        crate::embedding::run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        assert_eq!(
            status(&state).await,
            (MeaningState::Ready, MeaningState::Ready)
        );

        // The two works the model reads as about dragons, and no other.
        let page = list(&state, by_meaning("Dragon stories", None, None))
            .await
            .unwrap();
        let mut titles: Vec<&str> = page.works.iter().map(|w| w.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, ["A Dragon Tale", "The Dragon Returns"]);
        assert!(page.next_cursor.is_none());

        // One at a time, by cursor; the query is embedded once.
        let embedded = model.texts_embedded();
        let first = list(&state, by_meaning("Dragon stories", Some(1), None))
            .await
            .unwrap();
        assert_eq!(first.works.len(), 1);
        let cursor = first.next_cursor.clone().expect("a second page");
        let second = list(
            &state,
            by_meaning("Dragon stories", Some(1), Some(cursor.clone())),
        )
        .await
        .unwrap();
        assert_eq!(second.works.len(), 1);
        assert_ne!(first.works[0].id, second.works[0].id);
        assert_eq!(
            model.texts_embedded(),
            embedded,
            "the pages reuse the vector"
        );

        // A cursor of a search by meaning is not one of a search by words.
        let mut by_words = WorksQuery::default();
        by_words.q = Some("Dragon stories".into());
        by_words.cursor = Some(cursor);
        let (code, _) = list(&state, by_words).await.unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);

        // A query is a phrase, not a document.
        let (code, message) = list(
            &state,
            by_meaning(&"x".repeat(MAX_QUERY_BYTES + 1), None, None),
        )
        .await
        .unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(message.contains("at most"), "{message}");
    }

    #[tokio::test]
    async fn a_book_scanned_since_the_embedding_does_not_turn_the_search_off() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = library(&files, Some(&model)).await;
        crate::embedding::run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        let before = list(&state, by_meaning("Dragon stories", None, None))
            .await
            .unwrap();
        assert_eq!(before.works.len(), 2);

        // A scan adds a book: the model's space no longer covers the
        // library, so rankings fall back, but the search keeps answering
        // from the vectors it has and asks for the new book's.
        let mut book =
            bookclerk_library::NewBook::minimal("B999999", "acct", "us", "The Dragon Sleeps");
        book.source = "libro".into();
        book.plugin_key = "libro".into();
        let saved = library.upsert_book(&book).await.unwrap();
        let work = library
            .work_of_holding(&saved.holding_id)
            .await
            .unwrap()
            .unwrap();
        library.refresh_library_works(&[work]).await.unwrap();
        assert_eq!(
            status(&state).await,
            (MeaningState::Ready, MeaningState::Ready)
        );
        let page = list(&state, by_meaning("Dragon stories", None, None))
            .await
            .unwrap();
        assert_eq!(page.works.len(), 2, "the new book has no vector yet");
        let queued = library
            .list_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == JobKind::DiscoverEmbed && job.state == JobState::Pending)
            .count();
        assert_eq!(queued, 1, "the search asked for the embedding");

        // Once it is embedded, this node's index picks it up at once.
        crate::embedding::run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        let page = list(&state, by_meaning("Dragon stories", None, None))
            .await
            .unwrap();
        assert_eq!(page.works.len(), 3);
    }

    #[tokio::test]
    async fn similar_works_come_most_alike_first_without_the_work_itself() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, _library) = library(&files, Some(&model)).await;
        crate::embedding::run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        let tale = work_titled(&state, "A Dragon Tale").await;
        let answer = similar(&state, &tale, None).await;
        assert_eq!(answer.state, MeaningState::Ready);
        assert_eq!(answer.works.len(), TITLES.len() - 1);
        assert_eq!(answer.works[0].title, "The Dragon Returns");
        assert!(answer.works.iter().all(|work| work.id != tale));
        // The limit is honoured, and clamped to at least one work.
        assert_eq!(similar(&state, &tale, Some(3)).await.works.len(), 3);
        assert_eq!(similar(&state, &tale, Some(0)).await.works.len(), 1);

        let (code, message) = list_similar_works(
            State(state.clone()),
            HeaderMap::new(),
            AxumPath("no-such-work".to_string()),
            Query(SimilarQuery::default()),
        )
        .await
        .map(|_| ())
        .map_err(|(status, Json(body))| (status, body.error))
        .unwrap_err();
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(message.contains("no-such-work"), "{message}");
    }
}
