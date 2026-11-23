//! The taste radar of one book: `POST /api/discover/taste/radar` says how
//! close a book is to each part of the caller's taste.
//!
//! The axes are the genres the caller's own books lean to
//! ([`bookclerk_discover::taste_axes`]), each pointing at the mean of
//! their books of that genre in the embedding space. A book is placed on
//! an axis by how far its vector stands out toward that mean among all
//! the library's works ([`score`]), and each axis names the caller's own
//! books nearest the one asked about, so a wedge can say why.
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
use bookclerk_discover::{embed_text, EmbedFields, RadarSeed};
use bookclerk_library::{DiscoverSubject, LibraryStore, WorkPageQuery};
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

/// Subjects whose axes are kept. A subject's axes are built from every
/// seed work of their taste, so they are kept between the books a person
/// opens.
const AXIS_CACHE_ENTRIES: usize = 32;

/// Most of the caller's own books an axis names for one book.
const AXIS_EXAMPLES: usize = 3;

/// How many standard deviations from the library's mean similarity to an
/// axis a book stands when its score is 0: a book further from the axis
/// than most of the library.
const EMPTY_STANDING: f32 = -1.0;

/// The standing at which a book's score is 1. A first setting, not yet
/// measured on real libraries.
const FULL_STANDING: f32 = 3.0;

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
    /// The caller's taste has too few books that share a genre to make
    /// three axes, or no taste is stored for them yet.
    NoTaste,
}

/// Body of `POST /api/discover/taste/radar`: a library work by id, or a
/// store title by what is known of it.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub(crate) struct RadarRequest {
    /// The library work to place. When set, the other fields are not read.
    work_id: Option<String>,
    /// Display title of a store title; required without `work_id`.
    title: Option<String>,
    /// Its authors, as one comma-separated line.
    authors: Option<String>,
    /// Its series names, as one comma-separated line.
    series: Option<String>,
    /// Its genres or categories, as one line.
    genres: Option<String>,
    /// Its synopsis; only the first 1,200 characters are read.
    description: Option<String>,
}

/// One of the caller's own books on an axis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub(crate) struct RadarExample {
    /// The work.
    work_id: String,
    /// Its display title.
    title: String,
}

/// One axis of a radar, with the book placed on it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct RadarAxis {
    /// The genre that names the axis, as the caller's books carry it.
    name: String,
    /// How close the book is to the caller's books of this genre, 0 to 1:
    /// 0 is further from them than most of the library, 1 is as close as
    /// the closest few.
    score: f32,
    /// How many of the caller's books make the axis.
    books: usize,
    /// The caller's books of this genre nearest the book asked about,
    /// nearest first, without that book itself.
    examples: Vec<RadarExample>,
}

/// Answer of `POST /api/discover/taste/radar`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct TasteRadar {
    /// `ready` when `axes` places the book; otherwise why it is empty.
    state: RadarState,
    /// Three to six axes, the genre most of the caller's books carry
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

/// One axis as it is kept: where it points and what a similarity to it
/// is measured against.
struct KeptAxis {
    /// The genre that names it.
    name: String,
    /// The mean of the vectors of the caller's books of the genre.
    centroid: Vec<f32>,
    /// Mean and standard deviation of every library work's similarity to
    /// `centroid`; `None` in a library too small to have a spread.
    spread: Option<(f32, f32)>,
    /// The caller's books of the genre, each with its title.
    works: Vec<RadarExample>,
}

/// The axes of one subject's taste, read against one matrix.
struct KeptAxes {
    /// The subject's key.
    subject: String,
    /// When the taste they were built from was stored.
    computed_at: DateTime<Utc>,
    /// The matrix they were read against. A rebuilt matrix (new vectors,
    /// another space, another library store) is another `Arc`.
    matrix: Arc<WorkMatrix>,
    /// The axes; empty when the taste makes none.
    axes: Vec<KeptAxis>,
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

/// A book's score on an axis, 0 to 1, from its similarity to the axis.
///
/// A model's cosines sit in a narrow band, so the similarity itself says
/// little; how far the book stands out among the library's works does.
/// The score runs from [`EMPTY_STANDING`] to [`FULL_STANDING`] standard
/// deviations from the library's mean similarity to the axis.
///
/// # Arguments
///
/// * `similarity` - Cosine of the book and the axis.
/// * `spread` - Mean and standard deviation of the library's
///   similarities to the axis. Without one, or with no deviation, the
///   similarity itself is the score.
fn score(similarity: f32, spread: Option<(f32, f32)>) -> f32 {
    let score = match spread {
        Some((mean, deviation)) if deviation > f32::EPSILON => {
            let standing = (similarity - mean) / deviation;
            (standing - EMPTY_STANDING) / (FULL_STANDING - EMPTY_STANDING)
        }
        _ => similarity,
    };
    if score.is_finite() {
        score.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Builds the axes of the taste whose seed works are `seed_ids`, against
/// `matrix`.
///
/// # Errors
///
/// A 500 when the seed works cannot be read.
async fn build_axes(
    library: &LibraryStore,
    matrix: &WorkMatrix,
    seed_ids: &[String],
) -> Result<Vec<KeptAxis>, Response> {
    let with_vector: Vec<String> = seed_ids
        .iter()
        .filter(|id| matrix.vector_of(id).is_some())
        .cloned()
        .collect();
    let works = library
        .works_by_ids(&with_vector)
        .await
        .map_err(|err| read_failed(ROUTE, &err).into_response())?;
    let seeds: Vec<RadarSeed> = with_vector
        .iter()
        .filter_map(|id| {
            let work = works.get(id)?;
            Some(RadarSeed {
                work_id: id.clone(),
                genres: work.categories.clone().or_else(|| work.subjects.clone()),
                vector: matrix.vector_of(id)?.to_vec(),
            })
        })
        .collect();
    Ok(bookclerk_discover::taste_axes(&seeds)
        .into_iter()
        .map(|axis| KeptAxis {
            spread: matrix.spread(&axis.centroid),
            works: axis
                .work_ids
                .into_iter()
                .filter_map(|work_id| {
                    let title = works.get(&work_id)?.title.clone();
                    Some(RadarExample { work_id, title })
                })
                .collect(),
            name: axis.name,
            centroid: axis.centroid,
        })
        .collect())
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
                score: score(cosine(book, &axis.centroid), axis.spread),
                books: axis.works.len(),
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
/// The axes are the genres most of the caller's own books carry. Each
/// axis says how close the book is to those books in meaning, as the
/// library's embedding model reads them, and names the nearest of them.
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
    let Some(seeds) = bookclerk_discover::stored_taste_seeds(&library, &subject)
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
    let cache = &state.work_vectors.radars;
    let kept = match cache.get(&subject_key, seeds.computed_at, &matrix).await {
        Some(kept) => kept,
        None => {
            let kept = Arc::new(KeptAxes {
                axes: build_axes(&library, &matrix, &seeds.work_ids).await?,
                subject: subject_key,
                computed_at: seeds.computed_at,
                matrix: Arc::clone(&matrix),
            });
            cache.put(Arc::clone(&kept)).await;
            kept
        }
    };
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
    /// book. The fake model reads a text with "Dragon" in it one way and
    /// every other text another.
    const BOOKS: [(&str, &str, bool); 6] = [
        ("A Dragon Tale", "Fantasy, Humor", true),
        ("The Dragon Returns", "Fantasy, Mystery", true),
        ("Bread at Home", "Cooking, Humor", true),
        ("A Year of Soup", "Cooking, Mystery", true),
        ("Knots for Sailors", "Cooking", true),
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

    /// Stores the operator's taste, as a candidate fetch does.
    async fn store_taste(library: &LibraryStore) {
        bookclerk_discover::refresh_subject_taste(
            library,
            &DiscoverSubject::Operator,
            &bookclerk_discover::RecommendOptions::default(),
        )
        .await
        .unwrap();
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

    #[test]
    fn a_score_is_the_standing_among_the_library() {
        let spread = Some((0.5, 0.1));
        assert!(
            (score(0.4, spread) - 0.0).abs() < 1e-6,
            "one deviation below"
        );
        assert!(
            (score(0.5, spread) - 0.25).abs() < 1e-6,
            "the library's mean"
        );
        assert!((score(0.6, spread) - 0.5).abs() < 1e-6);
        assert!((score(0.9, spread) - 1.0).abs() < 1e-6, "past three above");
        assert!((score(0.1, spread) - 0.0).abs() < 1e-6);
        // Without a spread the similarity itself, kept within 0 to 1.
        assert!((score(0.7, None) - 0.7).abs() < 1e-6);
        assert!((score(0.7, Some((0.5, 0.0))) - 0.7).abs() < 1e-6);
        assert!((score(-0.2, None) - 0.0).abs() < 1e-6);
        assert!((score(f32::NAN, None) - 0.0).abs() < 1e-6);
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

        store_taste(&library).await;
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

    #[tokio::test]
    async fn a_library_work_is_placed_on_the_genres_of_the_callers_books() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = library(&files, Some(&model)).await;
        store_taste(&library).await;
        let song = work(&library, "Dragon Song").await;
        assert_eq!(
            radar(&state, song).await.unwrap().state,
            RadarState::Embedding,
            "the library is not embedded yet"
        );
        crate::embedding::run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();

        let answer = radar(&state, work(&library, "Dragon Song").await)
            .await
            .unwrap();
        assert_eq!(answer.state, RadarState::Ready);
        let names: Vec<&str> = answer.axes.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["Cooking", "Fantasy", "Humor", "Mystery"],
            "the genre most books carry first; Poetry is not the caller's"
        );
        let (cooking, fantasy) = (&answer.axes[0], &answer.axes[1]);
        // Half the library reads as dragons: the song stands one deviation
        // above the mean toward Fantasy and one below toward Cooking.
        assert!((fantasy.score - 0.5).abs() < 1e-3, "{}", fantasy.score);
        assert!(cooking.score.abs() < 1e-3, "{}", cooking.score);
        assert_eq!(fantasy.books, 2);
        assert_eq!(examples(fantasy), ["A Dragon Tale", "The Dragon Returns"]);
        assert_eq!(cooking.books, 3);
        assert_eq!(cooking.examples.len(), AXIS_EXAMPLES);

        // A book is not its own example.
        let tale = radar(&state, work(&library, "A Dragon Tale").await)
            .await
            .unwrap();
        assert_eq!(examples(&tale.axes[1]), ["The Dragon Returns"]);
        assert_eq!(tale.axes[1].books, 2);
    }

    #[tokio::test]
    async fn a_store_title_is_embedded_once_and_placed() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = library(&files, Some(&model)).await;
        store_taste(&library).await;
        crate::embedding::run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        let embedded = model.texts_embedded();
        let flight = || RadarRequest {
            title: Some("Dragon Flight".into()),
            authors: Some("A. Writer".into()),
            description: Some("A long way up.".into()),
            ..RadarRequest::default()
        };
        let answer = radar(&state, flight()).await.unwrap();
        assert_eq!(answer.state, RadarState::Ready);
        assert!((answer.axes[1].score - 0.5).abs() < 1e-3);
        assert_eq!(
            examples(&answer.axes[1]),
            ["A Dragon Tale", "The Dragon Returns"]
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
