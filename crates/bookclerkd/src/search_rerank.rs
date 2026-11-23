//! Store search results ordered with their meaning.
//!
//! A store orders its results by its own relevance. When this node has the
//! library's embedding model, `GET /api/discover/search` also asks how
//! close each result on the page is to the query in meaning, and returns
//! the page in an order that blends the two ([`blended_order`]). The page
//! is ordered before it is returned, so nothing moves after a client shows
//! it.
//!
//! A result is embedded from its title, authors and series alone: a search
//! page carries little else, and short texts keep a page of new results
//! under a second on the default model. The vectors are never stored. A
//! store's catalog is not the library's, so they live in a bounded cache
//! in memory ([`crate::work_vectors::WorkVectors::cached_hits`]) and a
//! result seen again costs nothing.
//!
//! The model gets a short time ([`RERANK_BUDGET`]) inside the search's own
//! budget. When it misses it, has no model here or fails, the page keeps
//! the store's order: a search never fails or waits long for this.
//!
//! A model plugin runs its calls one after another, and a call a search
//! stopped waiting for still runs. So the model works for a search in a
//! task of its own that holds one of a few slots
//! ([`crate::work_vectors::SEARCH_WORK_SLOTS`]) until the model has
//! answered: what it made is kept for the next search of that page, and a
//! search that finds every slot taken asks the model for nothing. See
//! `docs/internals/discovery.md`, *Catalog search*.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_discover::{embed_text, CatalogSearchHit, DiscoverError, EmbedFields};

use crate::api::AppState;
use crate::embedding::ModelEmbedder;

/// The longest a page waits for the model. A page of 24 results never seen
/// before takes about 0.7 s with the default model on one core
/// (`docs/internals/embedding-benchmarks.md`); a sleeping model plugin
/// that must first load its model misses this, and that page keeps the
/// store's order.
const RERANK_BUDGET: Duration = Duration::from_secs(3);

/// The longest the model may take for one search's call once it is
/// asked. Longer than a search waits ([`RERANK_BUDGET`]): the slot the call
/// holds is given back when the model has answered, so the slots count
/// what the model still has to do.
const MODEL_DEADLINE: Duration = bookclerk_integrations::AI_RUN_TIMEOUT;

/// Below this much time left in the search's budget the model is not
/// asked at all.
const LEAST_BUDGET: Duration = Duration::from_millis(250);

/// The text a store result is embedded from: its title with its subtitle,
/// its authors and its series.
fn hit_text(hit: &CatalogSearchHit) -> String {
    let title = match hit.subtitle.as_deref().map(str::trim) {
        Some(subtitle) if !subtitle.is_empty() => format!("{}: {subtitle}", hit.title.trim()),
        _ => hit.title.clone(),
    };
    embed_text(&EmbedFields {
        title: &title,
        authors: hit.authors.as_deref(),
        series: hit.series.as_deref(),
        genres: None,
        description: None,
    })
}

/// The cosine of the angle between two vectors; 0 when either has no
/// direction or they differ in length.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut a_norm, mut b_norm) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        a_norm += x * x;
        b_norm += y * y;
    }
    let norms = (a_norm * b_norm).sqrt();
    if norms > 0.0 && norms.is_finite() {
        // A cosine is within -1..=1, which an `f32` holds.
        #[allow(clippy::cast_possible_truncation)]
        let cosine = (dot / norms) as f32;
        cosine
    } else {
        0.0
    }
}

/// The order a page is shown in, as positions in the store's order.
///
/// Each result has two places: its place in the store's order, and its
/// place when the page is ordered by `scores` (closest in meaning first).
/// The page is ordered by the sum of the two, so a result must do well in
/// both to lead, and the store's order decides between equals. A first
/// setting, not yet measured on real searches.
///
/// # Arguments
///
/// * `scores` - Each result's closeness to the query, in the store's order.
fn blended_order(scores: &[f32]) -> Vec<usize> {
    let mut by_meaning: Vec<usize> = (0..scores.len()).collect();
    by_meaning.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b)));
    let mut meaning_place = vec![0_usize; scores.len()];
    for (place, position) in by_meaning.into_iter().enumerate() {
        meaning_place[position] = place;
    }
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by_key(|position| (position + meaning_place[*position], *position));
    order
}

/// How close each of `texts` is to `query`, when the vectors of all of
/// them are kept from earlier searches: no model is asked.
async fn kept_scores(
    state: &AppState,
    model: &ModelEmbedder,
    query: &str,
    texts: &[String],
) -> Option<Vec<f32>> {
    let space = model.vector_space();
    let query_vector = state.work_vectors.cached_query(space, query).await?;
    state
        .work_vectors
        .cached_hits(space, texts)
        .await
        .into_iter()
        .map(|vector| Some(cosine(&query_vector, vector?.as_slice())))
        .collect()
}

/// How close each of `texts` (the texts of a page's results) is to
/// `query` in meaning, in their order.
///
/// # Errors
///
/// Fails when the model cannot embed the query or a result within
/// `deadline`, or answers with too few vectors.
async fn scores(
    state: &AppState,
    model: &ModelEmbedder,
    query: &str,
    texts: &[String],
    deadline: Duration,
) -> Result<Vec<f32>, DiscoverError> {
    let space = model.vector_space().clone();
    let query_vector = match state.work_vectors.cached_query(&space, query).await {
        Some(vector) => vector,
        None => {
            let vector = Arc::new(model.embed_query(query, deadline).await?);
            state
                .work_vectors
                .cache_query(&space, query, Arc::clone(&vector))
                .await;
            vector
        }
    };
    let mut vectors = state.work_vectors.cached_hits(&space, texts).await;
    let mut missing: Vec<String> = texts
        .iter()
        .zip(&vectors)
        .filter(|(_, vector)| vector.is_none())
        .map(|(text, _)| text.clone())
        .collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        let made = model.embed_documents(&missing, deadline).await?;
        if made.len() != missing.len() {
            return Err(DiscoverError::Embed(format!(
                "the model returned {} vectors for {} texts",
                made.len(),
                missing.len()
            )));
        }
        let made: HashMap<String, Arc<Vec<f32>>> = missing
            .into_iter()
            .zip(made.into_iter().map(Arc::new))
            .collect();
        for (text, vector) in texts.iter().zip(&mut vectors) {
            if vector.is_none() {
                *vector = made.get(text).cloned();
            }
        }
        state
            .work_vectors
            .cache_hits(&space, made.into_iter().collect())
            .await;
    }
    Ok(vectors
        .iter()
        .map(|vector| {
            vector
                .as_deref()
                .map_or(0.0, |vector| cosine(&query_vector, vector))
        })
        .collect())
}

/// Orders one page of store results with their meaning, when this node
/// has the library's embedding model and it answers in time.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `query` - The search text.
/// * `hits` - The page in the store's order; reordered in place.
/// * `budget` - Time left in the search's own budget.
///
/// # Returns
///
/// Whether the page was reordered. `false` leaves `hits` untouched.
pub(crate) async fn by_meaning(
    state: &Arc<AppState>,
    query: &str,
    hits: &mut Vec<CatalogSearchHit>,
    budget: Duration,
) -> bool {
    let budget = budget.min(RERANK_BUDGET);
    if hits.len() < 2 || budget < LEAST_BUDGET {
        return false;
    }
    let texts: Vec<String> = hits.iter().map(hit_text).collect();
    let asked = async {
        let router = Arc::clone(&state.deployment_runtime().ai);
        let Some(model) = ModelEmbedder::current(&router).await else {
            return Ok(None);
        };
        if let Some(kept) = kept_scores(state, &model, query, &texts).await {
            return Ok(Some(kept));
        }
        // The model works in a task of its own, holding a slot until it
        // has answered: this search may stop waiting first, and the call
        // it left behind still occupies the model.
        let Ok(slot) = Arc::clone(&state.work_vectors.search_work.0).try_acquire_owned() else {
            return Err(DiscoverError::Embed(
                "the model is busy with earlier searches".to_string(),
            ));
        };
        let (state, query) = (Arc::clone(state), query.to_string());
        tokio::spawn(async move {
            let _slot = slot;
            scores(&state, &model, &query, &texts, MODEL_DEADLINE).await
        })
        .await
        .unwrap_or_else(|err| {
            Err(DiscoverError::Embed(format!(
                "the search's model task: {err}"
            )))
        })
        .map(Some)
    };
    let scores = match tokio::time::timeout(budget, asked).await {
        Ok(Ok(Some(scores))) => scores,
        // No model on this node: the store's order is all there is.
        Ok(Ok(None)) => return false,
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "store search keeps the store's order: the model failed");
            return false;
        }
        Err(_) => {
            tracing::debug!(
                results = hits.len(),
                "store search keeps the store's order: the model did not answer in time"
            );
            return false;
        }
    };
    let mut taken: Vec<Option<CatalogSearchHit>> =
        std::mem::take(hits).into_iter().map(Some).collect();
    hits.extend(
        blended_order(&scores)
            .into_iter()
            .filter_map(|position| taken.get_mut(position).and_then(Option::take)),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::tests::{daemon, FakeModel};

    /// A store result titled `title`.
    fn hit(title: &str) -> CatalogSearchHit {
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
    }

    /// The titles of `hits`, in order.
    fn titles(hits: &[CatalogSearchHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.title.as_str()).collect()
    }

    #[test]
    fn a_result_leads_when_both_orders_agree_and_the_store_breaks_ties() {
        // Equal in meaning: the store's order stands.
        assert_eq!(blended_order(&[0.5, 0.5, 0.5]), vec![0, 1, 2]);
        // The last result is closest in meaning and the first is furthest:
        // every sum ties, so the store's order still stands.
        assert_eq!(blended_order(&[0.1, 0.5, 0.9]), vec![0, 1, 2]);
        // The store's third and fourth results are the closest in meaning:
        // each passes one result that is far from the query, and the
        // store's first result keeps its lead over the third on the tie.
        assert_eq!(blended_order(&[0.1, 0.0, 0.9, 0.8]), vec![0, 2, 1, 3]);
        // A leading result that is the furthest of a longer page drops
        // behind the ones both orders like.
        assert_eq!(
            blended_order(&[0.0, 0.9, 0.8, 0.7, 0.6, 0.5]),
            vec![1, 2, 0, 3, 4, 5]
        );
        assert!(blended_order(&[]).is_empty());
    }

    #[test]
    fn a_cosine_ignores_length_and_survives_odd_vectors() {
        assert!((cosine(&[2.0, 0.0], &[0.5, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 3.0]).abs() < 1e-6);
        assert!(cosine(&[0.0, 0.0], &[1.0, 0.0]).abs() < f32::EPSILON);
        assert!(cosine(&[1.0], &[1.0, 0.0]).abs() < f32::EPSILON);
        assert!((cosine(&[1e30, 0.0], &[1e30, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_result_is_embedded_from_title_subtitle_authors_and_series() {
        let mut result = hit("Dune");
        result.subtitle = Some("  ".into());
        assert_eq!(hit_text(&result), "Dune\nA. Writer");
        result.subtitle = Some("Deluxe Edition".into());
        result.series = Some("Dune Chronicles".into());
        result.description = Some("Never embedded.".into());
        assert_eq!(
            hit_text(&result),
            "Dune: Deluxe Edition\nA. Writer\nDune Chronicles"
        );
    }

    #[tokio::test]
    async fn without_a_model_the_page_keeps_the_stores_order() {
        let files = tempfile::tempdir().unwrap();
        let (state, _library) = daemon(&files, &[], None).await;
        let mut hits = vec![hit("Emma"), hit("Dragon Flight")];
        assert!(!by_meaning(&state, "Dragon", &mut hits, Duration::from_secs(5)).await);
        assert_eq!(titles(&hits), ["Emma", "Dragon Flight"]);
    }

    #[tokio::test]
    async fn a_page_is_ordered_with_its_meaning_and_embedded_once() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, _library) = daemon(&files, &[], Some(&model)).await;
        let page = || {
            vec![
                hit("Emma"),
                hit("Persuasion"),
                hit("Dragon Flight"),
                hit("Dragon Song"),
            ]
        };
        let mut hits = page();
        assert!(by_meaning(&state, "Dragon tales", &mut hits, Duration::from_secs(5)).await);
        // Each result about dragons passes one that is not; the store's
        // first result keeps its place on the tie.
        assert_eq!(
            titles(&hits),
            ["Emma", "Dragon Flight", "Persuasion", "Dragon Song"]
        );
        // The query and the four results.
        assert_eq!(model.texts_embedded(), 5);
        // The same page again asks the model for nothing.
        let mut again = page();
        assert!(by_meaning(&state, "Dragon tales", &mut again, Duration::from_secs(5)).await);
        assert_eq!(titles(&again), titles(&hits));
        assert_eq!(model.texts_embedded(), 5);
    }

    #[tokio::test]
    async fn a_search_asks_a_busy_model_for_nothing_new() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, _library) = daemon(&files, &[], Some(&model)).await;
        let page = || vec![hit("Emma"), hit("Dragon Flight"), hit("Dragon Song")];
        let budget = Duration::from_secs(5);
        // Earlier searches hold every slot: their model calls are still
        // under way.
        let slots = Arc::clone(&state.work_vectors.search_work.0);
        let held = slots
            .acquire_many(u32::try_from(crate::work_vectors::SEARCH_WORK_SLOTS).unwrap())
            .await
            .unwrap();
        let mut hits = page();
        assert!(!by_meaning(&state, "Dragon tales", &mut hits, budget).await);
        assert_eq!(titles(&hits), ["Emma", "Dragon Flight", "Dragon Song"]);
        assert_eq!(model.texts_embedded(), 0);
        drop(held);
        // With a slot free the page is ordered, and the slot comes back.
        let mut hits = page();
        assert!(by_meaning(&state, "Dragon tales", &mut hits, budget).await);
        let asked = model.texts_embedded();
        assert_eq!(asked, 4);
        assert_eq!(
            slots.available_permits(),
            crate::work_vectors::SEARCH_WORK_SLOTS
        );
        // A page whose vectors are kept needs no slot and no model.
        let held = slots
            .acquire_many(u32::try_from(crate::work_vectors::SEARCH_WORK_SLOTS).unwrap())
            .await
            .unwrap();
        let mut again = page();
        assert!(by_meaning(&state, "Dragon tales", &mut again, budget).await);
        assert_eq!(titles(&again), titles(&hits));
        assert_eq!(model.texts_embedded(), asked);
        drop(held);
    }

    #[tokio::test]
    async fn too_little_time_or_one_result_asks_the_model_for_nothing() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, _library) = daemon(&files, &[], Some(&model)).await;
        let mut hits = vec![hit("Emma"), hit("Dragon Flight")];
        assert!(!by_meaning(&state, "Dragon", &mut hits, Duration::from_millis(10)).await);
        let mut one = vec![hit("Dragon Flight")];
        assert!(!by_meaning(&state, "Dragon", &mut one, Duration::from_secs(5)).await);
        assert_eq!(titles(&hits), ["Emma", "Dragon Flight"]);
        assert_eq!(model.texts_embedded(), 0);
    }
}
