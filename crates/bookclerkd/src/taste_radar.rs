//! The taste radar of one book: `POST /api/discover/taste/radar` says how
//! close a book lands to each part of the caller's taste.
//!
//! The axes are the caller's taste axes: their books and open wishes
//! grouped by meaning when their taste was last stored
//! ([`bookclerk_discover::stored_taste_axes`]), the same ones their
//! shelves are ranked against. Each axis points at the weighted mean of
//! its works in the embedding space. A book's score on an axis is where it
//! lands between the caller's other books (0) and the axis's own (1)
//! ([`bookclerk_discover::AxisScale::fit`]), and each axis names its
//! works nearest the book asked about, so a wedge can say why.
//!
//! A library work is read from this node's index of work vectors
//! ([`crate::work_vectors`]) and needs no model here. A store title is
//! embedded from the fields the client sends, as a candidate is, by this
//! node's model; its vector is kept in memory only. Nothing is stored.
//! See `docs/internals/discovery.md`, *The taste radar*.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_discover::{axis_scales, embed_text, AxisScale, EmbedFields, StoredAxes, TasteAxis};
use bookclerk_library::{DiscoverSubject, LibraryStore, VectorSpace, WorkPageQuery};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::api::AppState;
use crate::http_error::ErrorMessage;
use crate::library_meaning::{matrix, require_visible_work, unavailable, MeaningState, Standing};
use crate::library_works::{caller_scope, read_failed};
use crate::search_rerank::cosine;
use crate::work_vectors::WorkMatrix;

/// The route these errors are logged under.
const ROUTE: &str = "POST /api/discover/taste/radar";

/// How long a radar waits for the model to embed a store title. A
/// sleeping model plugin first loads the model, which takes seconds on a
/// small machine.
const EMBED_DEADLINE: Duration = Duration::from_secs(15);

/// Most bytes the fields of a store title may take together. Only the
/// first 1,200 characters of a description are embedded; this keeps a
/// request from carrying far more than that.
const MAX_TITLE_BYTES: usize = 16 * 1024;

/// Subjects whose axes are kept. An axis's scale is measured from every
/// work on the subject's axes, so the scales are kept between the books a
/// person opens.
const AXIS_CACHE_ENTRIES: usize = 32;

/// Most of the caller's own books an axis names for one book.
const AXIS_EXAMPLES: usize = 3;

/// Why a radar has axes or has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RadarState {
    /// The book is placed on every axis.
    Ready,
    /// No embedding model is there for it: none is installed or its
    /// plugin is off (for a store title, on the node that answered).
    NoModel,
    /// The library, or this work, is still being embedded with the model.
    Embedding,
    /// Fewer than two of the caller's books and wishes have a vector, or
    /// no taste is stored for them yet.
    NoTaste,
}

/// Body of `POST /api/discover/taste/radar`: a library work by id, or a
/// store title by what is known of it.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub(crate) struct RadarRequest {
    /// The library work to place. When set, the other fields are not read.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    work_id: Option<String>,
    /// Display title of a store title; required without `work_id`.
    #[schema(example = "The Left Hand of Darkness")]
    title: Option<String>,
    /// Its authors, as one comma-separated line.
    #[schema(example = "Ursula K. Le Guin")]
    authors: Option<String>,
    /// Its series names, as one comma-separated line.
    #[schema(example = "Hainish Cycle")]
    series: Option<String>,
    /// Its genres or categories, as one line.
    #[schema(example = "Science Fiction, Classics")]
    genres: Option<String>,
    /// Its synopsis; only the first 1,200 characters are read.
    #[schema(example = "An envoy is sent to a planet of perpetual winter.")]
    description: Option<String>,
}

/// One of the caller's own books or wishes on an axis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub(crate) struct RadarExample {
    /// The work.
    #[schema(example = "7c1d2e3f-4a5b-4c6d-8e7f-9a0b1c2d3e4f")]
    work_id: String,
    /// Its display title.
    #[schema(example = "The Dispossessed")]
    title: String,
}

/// One axis of a radar, with the book placed on it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct RadarAxis {
    /// What the axis's works have in common and the caller's other works
    /// lack: a genre, a series, a phrase their descriptions share, or
    /// "Like" and the title of the work at its middle.
    #[schema(example = "Science Fiction")]
    name: String,
    /// How close the book lands to where the axis points, 0 to 1: 0 is no
    /// closer than the caller's works on their other axes, 1 is as close
    /// as the axis's own works.
    #[schema(example = 0.72)]
    score: f32,
    /// How many of the caller's books and wishes make the axis.
    #[schema(example = 9)]
    books: usize,
    /// The works of the axis nearest the book asked about, nearest
    /// first, without that book itself.
    examples: Vec<RadarExample>,
}

/// Answer of `POST /api/discover/taste/radar`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct TasteRadar {
    /// `ready` when `axes` places the book; otherwise why it is empty.
    state: RadarState,
    /// One to six axes, the one that weighs most in the caller's taste
    /// first. Empty unless `state` is `ready`.
    axes: Vec<RadarAxis>,
}

impl TasteRadar {
    /// A radar without axes, for `state`.
    fn empty(state: RadarState) -> Json<Self> {
        Json(Self {
            state,
            axes: Vec::new(),
        })
    }
}

/// One axis as it is kept: its name, its scale and its works.
struct KeptAxis {
    /// What names it.
    name: String,
    /// Where it points and what a closeness to it is measured against.
    scale: AxisScale,
    /// How many works make the axis, with or without a vector here.
    books: usize,
    /// Its works that have a vector and a title here, each with its title.
    works: Vec<RadarExample>,
}

/// The axes of one subject's taste, read against one matrix.
pub(crate) struct KeptAxes {
    /// The subject's key.
    subject: String,
    /// When the taste they were built from was stored.
    computed_at: DateTime<Utc>,
    /// The matrix they were read against. A rebuilt matrix (new vectors,
    /// another space, another library store) is another `Arc`.
    matrix: Arc<WorkMatrix>,
    /// The space `matrix` holds.
    space: VectorSpace,
    /// The axes; empty when the taste makes none.
    axes: Vec<KeptAxis>,
}

impl KeptAxes {
    /// How well a vector of `space` fits the taste as a whole
    /// ([`bookclerk_discover::taste_fit`]): 0 for a taste without axes, a
    /// vector of another space, or a book near none of the axes.
    pub(crate) fn fit(&self, space: &VectorSpace, vector: &[f32]) -> f32 {
        if !self.space.same_as(space) {
            return 0.0;
        }
        bookclerk_discover::taste_fit(self.axes.iter().filter_map(|axis| {
            let closeness = axis.scale.closeness(vector)?;
            Some(axis.scale.pull(closeness))
        }))
    }
}

/// The axes of the subjects asked about lately, oldest first.
#[derive(Default)]
pub(crate) struct AxisCache {
    /// The kept axes.
    kept: Mutex<VecDeque<Arc<KeptAxes>>>,
}

impl AxisCache {
    /// Forgets every subject's axes.
    pub(crate) async fn clear(&self) {
        self.kept.lock().await.clear();
    }

    /// The axes kept for `subject`, when they were built from the taste
    /// stored at `computed_at` against `matrix`.
    async fn get(
        &self,
        subject: &str,
        computed_at: DateTime<Utc>,
        matrix: &Arc<WorkMatrix>,
    ) -> Option<Arc<KeptAxes>> {
        self.kept
            .lock()
            .await
            .iter()
            .find(|kept| {
                kept.subject == subject
                    && kept.computed_at == computed_at
                    && Arc::ptr_eq(&kept.matrix, matrix)
            })
            .cloned()
    }

    /// Keeps `axes` in place of the subject's earlier ones, forgetting the
    /// oldest subject beyond [`AXIS_CACHE_ENTRIES`].
    async fn put(&self, axes: Arc<KeptAxes>) {
        let mut kept = self.kept.lock().await;
        kept.retain(|earlier| earlier.subject != axes.subject);
        while kept.len() >= AXIS_CACHE_ENTRIES {
            kept.pop_front();
        }
        kept.push_back(axes);
    }
}

/// Measures the stored `axes` of a taste against `matrix`.
///
/// # Returns
///
/// The axes with a work in `matrix`.
///
/// # Errors
///
/// A 500 when the axes' works cannot be read.
async fn keep_axes(
    library: &LibraryStore,
    matrix: &WorkMatrix,
    axes: &[TasteAxis],
) -> Result<Vec<KeptAxis>, Response> {
    let ids: Vec<String> = axes
        .iter()
        .flat_map(|axis| axis.members.iter().map(|member| member.work_id.clone()))
        .filter(|id| matrix.vector_of(id).is_some())
        .collect();
    let works = library
        .works_by_ids(&ids)
        .await
        .map_err(|err| read_failed(ROUTE, &err).into_response())?;
    Ok(axis_scales(axes, |id| matrix.vector_of(id))
        .into_iter()
        .zip(axes)
        .filter_map(|(scale, axis)| {
            Some(KeptAxis {
                name: axis.name.clone(),
                scale: scale?,
                books: axis.members.len(),
                works: axis
                    .members
                    .iter()
                    .filter(|member| matrix.vector_of(&member.work_id).is_some())
                    .filter_map(|member| {
                        Some(RadarExample {
                            work_id: member.work_id.clone(),
                            title: works.get(&member.work_id)?.title.clone(),
                        })
                    })
                    .collect(),
            })
        })
        .collect())
}

/// The axes of `taste` measured against `matrix`, from the cache when the
/// subject's taste and the matrix are the ones last measured.
///
/// # Errors
///
/// A 500 when the axes' works cannot be read.
async fn kept_axes(
    state: &AppState,
    library: &LibraryStore,
    subject_key: String,
    taste: &StoredAxes,
    standing: &Standing,
    matrix: &Arc<WorkMatrix>,
) -> Result<Arc<KeptAxes>, Response> {
    let cache = &state.work_vectors.radars;
    if let Some(kept) = cache.get(&subject_key, taste.computed_at, matrix).await {
        return Ok(kept);
    }
    let kept = Arc::new(KeptAxes {
        axes: keep_axes(library, matrix, &taste.axes).await?,
        subject: subject_key,
        computed_at: taste.computed_at,
        matrix: Arc::clone(matrix),
        space: standing.reading.clone(),
    });
    cache.put(Arc::clone(&kept)).await;
    Ok(kept)
}

/// The caller's taste axes, for a route that only orders by them.
///
/// # Returns
///
/// `None` when the caller has no stored taste or no axes, no model's
/// vectors serve the library, or anything fails on the way: a search is
/// then ordered without the taste.
pub(crate) async fn caller_axes(
    state: &AppState,
    library: &LibraryStore,
    headers: &HeaderMap,
) -> Option<Arc<KeptAxes>> {
    /// The route these reads are logged under.
    const ROUTE: &str = "GET /api/discover/search";
    let (subject_key, _) = crate::auth::prefs_subject_for_caller(state, headers)
        .await
        .ok()?;
    let subject = DiscoverSubject::parse(&subject_key).unwrap_or(DiscoverSubject::Operator);
    let taste = bookclerk_discover::stored_taste_axes(library, &subject)
        .await
        .ok()
        .flatten()
        .filter(|taste| !taste.axes.is_empty())?;
    let standing = Standing::read(state, library, ROUTE).await.ok()?;
    if standing.similar() != MeaningState::Ready {
        return None;
    }
    let matrix = matrix(state, library, &standing.reading, ROUTE)
        .await
        .ok()?;
    let kept = kept_axes(state, library, subject_key, &taste, &standing, &matrix)
        .await
        .ok()?;
    (!kept.axes.is_empty()).then_some(kept)
}

/// Refuses a request that names neither a work nor a title, or whose
/// title fields are too long.
///
/// # Errors
///
/// A 400 saying which.
fn check(request: &RadarRequest) -> Result<(), Response> {
    let bad_request = |message: String| {
        (StatusCode::BAD_REQUEST, Json(ErrorMessage::new(message))).into_response()
    };
    let named =
        |field: &Option<String>| field.as_deref().is_some_and(|text| !text.trim().is_empty());
    if named(&request.work_id) {
        return Ok(());
    }
    if !named(&request.title) {
        return Err(bad_request(
            "a radar needs a work_id or a title".to_string(),
        ));
    }
    let bytes: usize = [
        &request.title,
        &request.authors,
        &request.series,
        &request.genres,
        &request.description,
    ]
    .into_iter()
    .map(|field| field.as_deref().map_or(0, str::len))
    .sum();
    if bytes > MAX_TITLE_BYTES {
        return Err(bad_request(format!(
            "a title's fields take at most {MAX_TITLE_BYTES} bytes together"
        )));
    }
    Ok(())
}

/// The vector of the store title in `request`, embedded by this node's
/// model as a candidate is.
///
/// # Returns
///
/// The vector, or the state that says why there is none.
///
/// # Errors
///
/// A 503 when the model did not answer in time.
async fn title_vector(
    state: &AppState,
    standing: &Standing,
    request: &RadarRequest,
) -> Result<Result<Arc<Vec<f32>>, RadarState>, Response> {
    let title = request.title.as_deref().map(str::trim).unwrap_or_default();
    let model = match (standing.search(), &standing.model) {
        (MeaningState::Ready, Some(model)) => model,
        (MeaningState::Embedding, _) => return Ok(Err(RadarState::Embedding)),
        _ => return Ok(Err(RadarState::NoModel)),
    };
    let text = embed_text(&EmbedFields {
        title,
        authors: request.authors.as_deref(),
        series: request.series.as_deref(),
        genres: request.genres.as_deref(),
        description: request.description.as_deref(),
    });
    let space = model.vector_space();
    let texts = [text];
    if let Some(Some(vector)) = state
        .work_vectors
        .cached_hits(space, &texts)
        .await
        .into_iter()
        .next()
    {
        return Ok(Ok(vector));
    }
    let vector = model
        .embed_documents(&texts, EMBED_DEADLINE)
        .await
        .ok()
        .and_then(|vectors| vectors.into_iter().next())
        .map(Arc::new)
        .ok_or_else(|| {
            tracing::warn!("{ROUTE}: the model did not embed the title");
            unavailable("the embedding model did not answer; try again shortly").into_response()
        })?;
    let [text] = texts;
    state
        .work_vectors
        .cache_hits(space, vec![(text, Arc::clone(&vector))])
        .await;
    Ok(Ok(vector))
}

/// The axes with a book placed on each.
///
/// # Arguments
///
/// * `axes` - The caller's axes.
/// * `matrix` - The matrix the axes were read against.
/// * `book` - The book's vector.
/// * `work_id` - The book, when it is a library work, so it is not named
///   as its own example.
fn placed(
    axes: &[KeptAxis],
    matrix: &WorkMatrix,
    book: &[f32],
    work_id: Option<&str>,
) -> Vec<RadarAxis> {
    axes.iter()
        .map(|axis| {
            let mut nearest: Vec<(f32, &RadarExample)> = axis
                .works
                .iter()
                .filter(|work| Some(work.work_id.as_str()) != work_id)
                .filter_map(|work| {
                    let vector = matrix.vector_of(&work.work_id)?;
                    Some((cosine(book, vector), work))
                })
                .collect();
            nearest.sort_by(|a, b| {
                b.0.total_cmp(&a.0)
                    .then_with(|| a.1.work_id.cmp(&b.1.work_id))
            });
            RadarAxis {
                name: axis.name.clone(),
                score: axis
                    .scale
                    .closeness(book)
                    .map_or(0.0, |closeness| axis.scale.fit(closeness)),
                books: axis.books,
                examples: nearest
                    .into_iter()
                    .take(AXIS_EXAMPLES)
                    .map(|(_, work)| work.clone())
                    .collect(),
            }
        })
        .collect()
}

/// Places one book on the caller's taste radar.
///
/// The axes are the caller's taste: their books and open wishes, grouped
/// by meaning as the library's embedding model reads them. Each axis says
/// how close the book lands to where its works point, and names the
/// nearest of them.
/// Send `work_id` for a library work, or what is known of a store title.
#[utoipa::path(
    post,
    path = "/api/discover/taste/radar",
    request_body = RadarRequest,
    responses(
        (status = 200, description = "The radar, or why it has no axes.", body = TasteRadar),
        (status = 400, description = "Neither a work nor a title was sent, or the title's fields are too long.", body = ErrorMessage, example = json!({"error": "a radar needs a work_id or a title"})),
        (status = 401, description = "The caller has no portal identity, or their session ended.", body = String, content_type = "text/plain", example = json!("Unauthorized")),
        (status = 404, description = "No such work.", body = ErrorMessage, example = json!({"error": "work 0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b not found"})),
        (status = 500, description = "The library could not be read.", body = ErrorMessage, example = json!({"error": "the library could not be read"})),
        (status = 503, description = "The embedding model did not answer in time.", body = ErrorMessage, example = json!({"error": "the embedding model did not answer; try again shortly"})),
    ),
)]
pub(crate) async fn taste_radar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<RadarRequest>,
) -> Result<Json<TasteRadar>, Response> {
    check(&request)?;
    let library = state.library_snapshot().await;
    let work_id = request
        .work_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if let Some(id) = work_id {
        let filters = WorkPageQuery {
            scope: caller_scope(&state, &library, &headers, ROUTE)
                .await
                .map_err(IntoResponse::into_response)?,
            limit: 1,
            ..WorkPageQuery::default()
        };
        require_visible_work(&library, &filters, id, ROUTE)
            .await
            .map_err(IntoResponse::into_response)?;
    }
    let (subject_key, _) = crate::auth::prefs_subject_for_caller(&state, &headers)
        .await
        .map_err(IntoResponse::into_response)?;
    let subject = DiscoverSubject::parse(&subject_key).unwrap_or(DiscoverSubject::Operator);
    let Some(taste) = bookclerk_discover::stored_taste_axes(&library, &subject)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, route = ROUTE, "could not read the stored taste");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorMessage::new("the library could not be read")),
            )
                .into_response()
        })?
    else {
        return Ok(TasteRadar::empty(RadarState::NoTaste));
    };
    let standing = Standing::read(&state, &library, ROUTE)
        .await
        .map_err(IntoResponse::into_response)?;
    match standing.similar() {
        MeaningState::Ready => {}
        MeaningState::NoModel => return Ok(TasteRadar::empty(RadarState::NoModel)),
        MeaningState::Embedding => return Ok(TasteRadar::empty(RadarState::Embedding)),
    }
    let matrix = matrix(&state, &library, &standing.reading, ROUTE)
        .await
        .map_err(IntoResponse::into_response)?;
    let kept = kept_axes(&state, &library, subject_key, &taste, &standing, &matrix).await?;
    if kept.axes.is_empty() {
        return Ok(TasteRadar::empty(RadarState::NoTaste));
    }
    let book: Arc<Vec<f32>> = match work_id {
        Some(id) => match matrix.vector_of(id) {
            Some(vector) => Arc::new(vector.to_vec()),
            // Scanned since the library was embedded; its turn comes.
            None => return Ok(TasteRadar::empty(RadarState::Embedding)),
        },
        None => match title_vector(&state, &standing, &request).await? {
            Ok(vector) => vector,
            Err(state) => return Ok(TasteRadar::empty(state)),
        },
    };
    Ok(Json(TasteRadar {
        state: RadarState::Ready,
        axes: placed(&kept.axes, &matrix, &book, work_id),
    }))
}

#[cfg(test)]
mod tests {
    use bookclerk_integrations::Integration;
    use bookclerk_library::{NewBook, UserBookFields, UserRole};

    use super::*;
    use crate::embedding::tests::{payload, FakeModel};

    /// The test library: title, genres and whether someone finished the
    /// book. The fake model reads a text with "Dragon" in it one way, one
    /// with "Starship" another and every other text a third.
    const BOOKS: [(&str, &str, bool); 7] = [
        ("A Dragon Tale", "Fantasy", true),
        ("The Dragon Returns", "Fantasy, Humor", true),
        ("Bread at Home", "Cooking", true),
        ("A Year of Soup", "Cooking, Humor", true),
        ("Starship Down", "Science Fiction", true),
        ("The Last Starship", "Science Fiction", true),
        ("Dragon Song", "Poetry", false),
    ];

    /// A daemon over [`BOOKS`], with `model` deployed when given.
    async fn library(
        files: &tempfile::TempDir,
        model: Option<&Arc<FakeModel>>,
    ) -> (Arc<AppState>, LibraryStore) {
        let (state, library) = crate::node_api::tests::test_state(files).await;
        library
            .upsert_account("libro", "acct", "us", None, true, "libro")
            .await
            .unwrap();
        for (n, (title, genres, _)) in BOOKS.iter().enumerate() {
            let id = format!("B{n:06}");
            let mut book = NewBook::minimal(&id, "acct", "us", *title);
            book.source = "libro".into();
            book.plugin_key = "libro".into();
            book.categories = Some((*genres).to_string());
            library.upsert_book(&book).await.unwrap();
        }
        let reader = library
            .create_user(UserRole::Member, Some("Reader"), None)
            .await
            .unwrap();
        let works = library.list_works_page(None, 100).await.unwrap();
        for work in &works {
            let finished = BOOKS
                .iter()
                .any(|(title, _, read)| *read && *title == work.title);
            if finished {
                let fields = UserBookFields {
                    is_finished: Some(true),
                    ..UserBookFields::default()
                };
                library
                    .set_user_title_state(reader.id, &work.id, &fields)
                    .await
                    .unwrap();
            }
        }
        let ids: Vec<String> = works.into_iter().map(|work| work.id).collect();
        library.refresh_library_works(&ids).await.unwrap();
        if let Some(model) = model {
            state
                .integrations
                .write()
                .await
                .register(Arc::clone(model) as Arc<dyn Integration>);
        }
        (state, library)
    }

    /// Stores the operator's taste, as a candidate fetch does, with the
    /// vectors a ranking on this node reads.
    async fn store_taste(state: &AppState, library: &LibraryStore) {
        let node = crate::embedding::node_embedder(state).await;
        let vectors = crate::embedding::vector_source(library, node)
            .await
            .unwrap();
        bookclerk_discover::refresh_subject_taste(
            library,
            &DiscoverSubject::Operator,
            &bookclerk_discover::RecommendOptions {
                vectors,
                ..bookclerk_discover::RecommendOptions::default()
            },
        )
        .await
        .unwrap();
    }

    /// Embeds the library with the node's model, then stores the taste
    /// again, as the ranking that follows an embedding does.
    async fn embed(state: &Arc<AppState>, library: &LibraryStore) {
        crate::embedding::run_discover_embed(state, &payload(state).await, None)
            .await
            .unwrap();
        store_taste(state, library).await;
    }

    /// The radar of `request`, or the status it was refused with.
    async fn radar(state: &Arc<AppState>, request: RadarRequest) -> Result<TasteRadar, StatusCode> {
        taste_radar(State(state.clone()), HeaderMap::new(), Json(request))
            .await
            .map(|Json(radar)| radar)
            .map_err(|response| response.status())
    }

    /// A request for the library work titled `title`.
    async fn work(library: &LibraryStore, title: &str) -> RadarRequest {
        let id = library
            .list_works_page(None, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|work| work.title == title)
            .unwrap()
            .id;
        RadarRequest {
            work_id: Some(id),
            ..RadarRequest::default()
        }
    }

    /// The titles an axis names, sorted.
    fn examples(axis: &RadarAxis) -> Vec<&str> {
        let mut titles: Vec<&str> = axis.examples.iter().map(|e| e.title.as_str()).collect();
        titles.sort_unstable();
        titles
    }

    #[tokio::test]
    async fn a_radar_says_why_it_has_no_axes() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = library(&files, None).await;
        let song = work(&library, "Dragon Song").await;
        let id = song.work_id.clone();
        let answer = radar(&state, song).await.unwrap();
        assert_eq!(answer.state, RadarState::NoTaste, "no taste is stored yet");
        assert!(answer.axes.is_empty());

        store_taste(&state, &library).await;
        let again = RadarRequest {
            work_id: id,
            ..RadarRequest::default()
        };
        assert_eq!(
            radar(&state, again).await.unwrap().state,
            RadarState::NoModel
        );

        assert_eq!(
            radar(&state, RadarRequest::default()).await.unwrap_err(),
            StatusCode::BAD_REQUEST,
            "neither a work nor a title"
        );
        let missing = RadarRequest {
            work_id: Some("no-such-work".into()),
            ..RadarRequest::default()
        };
        assert_eq!(
            radar(&state, missing).await.unwrap_err(),
            StatusCode::NOT_FOUND
        );
    }

    /// The axis of `radar` named `name`.
    fn axis<'a>(radar: &'a TasteRadar, name: &str) -> &'a RadarAxis {
        radar
            .axes
            .iter()
            .find(|axis| axis.name == name)
            .unwrap_or_else(|| panic!("no axis {name}: {:?}", radar.axes))
    }

    #[tokio::test]
    async fn a_library_work_is_placed_on_the_axes_of_the_callers_taste() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = library(&files, Some(&model)).await;
        store_taste(&state, &library).await;
        let song = work(&library, "Dragon Song").await;
        assert_eq!(
            radar(&state, song).await.unwrap().state,
            RadarState::Embedding,
            "the library is not embedded yet"
        );
        embed(&state, &library).await;

        let answer = radar(&state, work(&library, "Dragon Song").await)
            .await
            .unwrap();
        assert_eq!(answer.state, RadarState::Ready);
        let mut names: Vec<&str> = answer.axes.iter().map(|a| a.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["Cooking", "Fantasy", "Science Fiction"],
            "the books grouped by meaning, each group named by the genre \
             the others lack; Humor is shared, and Poetry is carried by \
             one book, which nobody finished"
        );
        // The song lands where the dragon books do, and no closer to the
        // others than the caller's books from other axes are.
        let fantasy = axis(&answer, "Fantasy");
        assert!((fantasy.score - 1.0).abs() < 1e-3, "{}", fantasy.score);
        assert!(axis(&answer, "Cooking").score.abs() < 1e-3);
        assert!(axis(&answer, "Science Fiction").score.abs() < 1e-3);
        // The song is in the library, so it is a work of the axis too, at
        // the half point of a book nobody finished. It is not its own
        // example.
        assert_eq!(fantasy.books, 3);
        assert_eq!(examples(fantasy), ["A Dragon Tale", "The Dragon Returns"]);

        let tale = radar(&state, work(&library, "A Dragon Tale").await)
            .await
            .unwrap();
        assert_eq!(
            examples(axis(&tale, "Fantasy")),
            ["Dragon Song", "The Dragon Returns"]
        );
        assert_eq!(axis(&tale, "Fantasy").books, 3);
    }

    #[tokio::test]
    async fn a_wish_is_a_work_of_its_axis() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = library(&files, Some(&model)).await;
        library
            .create_title_request(&bookclerk_library::NewTitleRequest {
                uuid: None,
                identity_id: None,
                title: "Dragon Rider".into(),
                authors: None,
                asin: Some("B0WISH0001".into()),
                isbn: None,
                notes: None,
                status: bookclerk_library::RequestStatus::Open,
                resolved_edition_id: None,
                cover_url: None,
            })
            .await
            .unwrap();
        embed(&state, &library).await;
        let answer = radar(&state, work(&library, "Dragon Song").await)
            .await
            .unwrap();
        let fantasy = axis(&answer, "Fantasy");
        assert_eq!(fantasy.books, 4, "three books and the wish");
        assert_eq!(
            examples(fantasy),
            ["A Dragon Tale", "Dragon Rider", "The Dragon Returns"]
        );
    }

    #[tokio::test]
    async fn a_search_page_breaks_ties_by_the_searchers_taste() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = library(&files, Some(&model)).await;
        let headers = HeaderMap::new();
        assert!(
            caller_axes(&state, &library, &headers).await.is_none(),
            "no stored taste, no axes"
        );
        embed(&state, &library).await;
        let kept = caller_axes(&state, &library, &headers).await.unwrap();
        assert_eq!(kept.axes.len(), 3);
        // A taste for dragons alone, in the model's space.
        let dragons = Arc::new(KeptAxes {
            subject: kept.subject.clone(),
            computed_at: kept.computed_at,
            matrix: Arc::clone(&kept.matrix),
            space: kept.space.clone(),
            axes: vec![KeptAxis {
                name: "Fantasy".into(),
                scale: AxisScale {
                    centroid: vec![1.0, 0.0, 0.5],
                    near: 1.0,
                    far: 0.2,
                    strength: 1.0,
                },
                books: 2,
                works: Vec::new(),
            }],
        });
        assert!((dragons.fit(&kept.space, &[1.0, 0.0, 0.5]) - 1.0).abs() < 1e-6);
        assert!(dragons.fit(&kept.space, &[0.0, 1.0, 0.5]).abs() < 1e-6);
        let other = VectorSpace {
            model: "@embedder/other".into(),
            ..kept.space.clone()
        };
        assert!(
            dragons.fit(&other, &[1.0, 0.0, 0.5]).abs() < 1e-6,
            "another model's vectors are not measured"
        );

        let hit = |title: &str| -> bookclerk_discover::CatalogSearchHit {
            serde_json::from_value(serde_json::json!({
                "work_key": format!("soft:{title}"),
                "title": title,
                "authors": "A. Writer",
                "narrators": null,
                "series": null,
                "asin": null,
                "isbn": null,
                "store_editions": [],
                "sources": ["libro"],
            }))
            .unwrap()
        };
        let page = || {
            vec![
                hit("Emma"),
                hit("Persuasion"),
                hit("Dragon Flight"),
                hit("Dragon Song"),
            ]
        };
        let titles = |hits: &[bookclerk_discover::CatalogSearchHit]| {
            hits.iter()
                .map(|hit| hit.title.clone())
                .collect::<Vec<String>>()
        };
        let budget = Duration::from_secs(5);
        // Without the taste the store's first result keeps its place on
        // each tie; with it the dragon of each pair leads.
        let mut plain = page();
        crate::search_rerank::by_meaning(&state, "Dragon tales", &mut plain, None, budget).await;
        assert_eq!(
            titles(&plain),
            ["Emma", "Dragon Flight", "Persuasion", "Dragon Song"]
        );
        let mut tasted = page();
        crate::search_rerank::by_meaning(
            &state,
            "Dragon tales",
            &mut tasted,
            Some(&dragons),
            budget,
        )
        .await;
        assert_eq!(
            titles(&tasted),
            ["Dragon Flight", "Emma", "Dragon Song", "Persuasion"]
        );
    }

    #[tokio::test]
    async fn a_store_title_is_embedded_once_and_placed() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = library(&files, Some(&model)).await;
        embed(&state, &library).await;
        let embedded = model.texts_embedded();
        let flight = || RadarRequest {
            title: Some("Dragon Flight".into()),
            authors: Some("A. Writer".into()),
            description: Some("A long way up.".into()),
            ..RadarRequest::default()
        };
        let answer = radar(&state, flight()).await.unwrap();
        assert_eq!(answer.state, RadarState::Ready);
        let fantasy = axis(&answer, "Fantasy");
        assert!((fantasy.score - 1.0).abs() < 1e-3);
        assert_eq!(
            examples(fantasy),
            ["A Dragon Tale", "Dragon Song", "The Dragon Returns"]
        );
        assert_eq!(model.texts_embedded(), embedded + 1);
        radar(&state, flight()).await.unwrap();
        assert_eq!(model.texts_embedded(), embedded + 1, "kept in memory");

        let long = RadarRequest {
            title: Some("Dragon Flight".into()),
            description: Some("x".repeat(MAX_TITLE_BYTES)),
            ..RadarRequest::default()
        };
        assert_eq!(
            radar(&state, long).await.unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }
}
