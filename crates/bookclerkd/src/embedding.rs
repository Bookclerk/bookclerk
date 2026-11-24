//! Text vectors on this node: the embedder Discover uses, and the job that
//! embeds the library after a model lands or changes.
//!
//! The daemon loads no model. [`ModelEmbedder`] runs the model the library
//! chose for text embeddings in its plugin, through the node's model
//! router, and the hash stand-in takes its place while no plugin serves
//! one here ([`node_embedder`]).
//!
//! Vectors of two models, or of two revisions of one, are never compared.
//! A ranking reads the space [`bookclerk_discover::serving_space`] picks,
//! so the vectors a library had keep serving while a `discover_embed` job
//! ([`run_discover_embed`]) embeds every work with the new model; when it
//! is done, every subject's feed is ranked again. See
//! `docs/internals/discovery.md`, *Embeddings*.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use bookclerk_discover::{DiscoverError, Embedder, VectorSource};
use bookclerk_integrations::{AiRouter, AI_RUN_TIMEOUT};
use bookclerk_library::{
    EnqueueJobSpec, JobKind, JobPayload, JobTrigger, LibraryStore, VectorSpace,
};
use bookclerk_plugin_abi::ai::embedding_vectors;
use bookclerk_plugin_abi::{
    AiEmbedText, AiInput, AiRunParams, AiTask, AiTextEmbeddingsInput, AiTextRole,
    MAX_AI_EMBED_TEXT_BYTES,
};
use chrono::Utc;

use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::jobs::{admit, job_cancelled, queue_limits, AdmitJob};

/// Texts sent to the model in one run. Small, so one run stays far inside
/// the router's deadline on the slowest first-party model and a cancelled
/// job stops soon.
const MODEL_BATCH_TEXTS: usize = 16;

/// How long a `discover_embed` job embeds before it lets a waiting ranking
/// run and looks for cancellation.
const EMBED_SLICE: Duration = Duration::from_secs(10);

/// Least time between two `discover_embed` jobs a ranking admits for a
/// model that already serves, so works whose text changed are embedded
/// again without a job per ranking.
const EMBED_RECHECK_INTERVAL: chrono::Duration = chrono::Duration::hours(1);

/// Unix seconds of the last `discover_embed` job this daemon admitted from
/// a ranking (0: never).
static LAST_EMBED_RECHECK: AtomicI64 = AtomicI64::new(0);

/// The model the library chose for text embeddings, run in its plugin.
pub(crate) struct ModelEmbedder {
    /// The node's model router.
    router: Arc<AiRouter>,
    /// The model's full name, `@<plugin id>/<model id>`.
    name: String,
    /// The space of its vectors: the name, and the revision it listed.
    space: VectorSpace,
    /// Numbers in each vector.
    dimensions: usize,
}

impl ModelEmbedder {
    /// The model `@library/text-embeddings` names on this node now.
    ///
    /// # Arguments
    ///
    /// * `router` - The node's model router.
    ///
    /// # Returns
    ///
    /// The embedder, or `None` when no plugin deployed here serves a text
    /// embedding model (none is installed yet, or the plugin is off).
    pub(crate) async fn current(router: &Arc<AiRouter>) -> Option<Self> {
        let routed = router.model_for(AiTask::TextEmbeddings).await?;
        let dimensions = routed
            .model
            .dimensions
            .and_then(|dimensions| usize::try_from(dimensions).ok())
            .filter(|dimensions| *dimensions > 0)?;
        let name = routed.name();
        Some(Self {
            router: Arc::clone(router),
            space: VectorSpace {
                model: name.clone(),
                revision: routed.model.revision.clone(),
                packs: routed.model.languages.join(","),
            },
            name,
            dimensions,
        })
    }
}

/// `text` as the model takes it: never empty, and cut at a character
/// boundary to the most bytes one text may have.
fn model_text(text: &str) -> String {
    let limit = usize::try_from(MAX_AI_EMBED_TEXT_BYTES).unwrap_or(usize::MAX);
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end].trim();
    if cut.is_empty() {
        String::from("untitled")
    } else {
        cut.to_string()
    }
}

#[async_trait]
impl Embedder for ModelEmbedder {
    fn space(&self) -> &VectorSpace {
        &self.space
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn max_batch(&self) -> usize {
        MODEL_BATCH_TEXTS
    }

    async fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, DiscoverError> {
        self.run(texts, AiTextRole::Document, AI_RUN_TIMEOUT).await
    }
}

impl ModelEmbedder {
    /// Runs the model on `texts`, each marked as `role`.
    ///
    /// # Errors
    ///
    /// Fails when the model cannot be reached, misses `deadline`, or now
    /// answers with another revision than this embedder's space.
    async fn run(
        &self,
        texts: &[String],
        role: AiTextRole,
        deadline: Duration,
    ) -> Result<Vec<Vec<f32>>, DiscoverError> {
        let params = AiRunParams {
            model: self.name.clone(),
            input: AiInput {
                text_embeddings: AiTextEmbeddingsInput {
                    texts: texts
                        .iter()
                        .map(|text| AiEmbedText {
                            text: model_text(text),
                            role,
                        })
                        .collect(),
                },
            },
        };
        let reply = self
            .router
            .run(None, params, deadline)
            .await
            .map_err(|err| DiscoverError::Embed(format!("`{}`: {err}", self.name)))?;
        let output = reply.output.text_embeddings;
        // The router checked the reply against the model's listing; a
        // model that reloaded since this embedder was made answers with
        // another revision, and its vectors belong to another space.
        if output.revision != self.space.revision {
            return Err(DiscoverError::Embed(format!(
                "`{}` changed while it was embedding",
                self.name
            )));
        }
        Ok(embedding_vectors(&output))
    }

    /// The vectors of `texts`, each embedded as a document, a batch at a
    /// time.
    ///
    /// # Errors
    ///
    /// Fails as [`Self::run`] does; `deadline` bounds each batch.
    pub(crate) async fn embed_documents(
        &self,
        texts: &[String],
        deadline: Duration,
    ) -> Result<Vec<Vec<f32>>, DiscoverError> {
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(MODEL_BATCH_TEXTS) {
            vectors.extend(self.run(batch, AiTextRole::Document, deadline).await?);
        }
        Ok(vectors)
    }

    /// The space of this model's vectors.
    pub(crate) fn vector_space(&self) -> &VectorSpace {
        &self.space
    }

    /// The vector of a search query: `text` embedded as a query, which
    /// the model places near the documents that answer it.
    ///
    /// # Errors
    ///
    /// Fails as [`Self::run`] does, or when the model answers without a
    /// vector.
    pub(crate) async fn embed_query(
        &self,
        text: &str,
        deadline: Duration,
    ) -> Result<Vec<f32>, DiscoverError> {
        self.run(&[text.to_string()], AiTextRole::Query, deadline)
            .await?
            .into_iter()
            .next()
            .filter(|vector| !vector.is_empty())
            .ok_or_else(|| DiscoverError::Embed(format!("`{}` returned no vector", self.name)))
    }
}

/// The embedder this node makes vectors with: the library's text embedding
/// model when a plugin serves one here, else the hash stand-in.
///
/// # Arguments
///
/// * `state` - The daemon.
pub(crate) async fn node_embedder(state: &AppState) -> Box<dyn Embedder> {
    let router = Arc::clone(&state.deployment_runtime().ai);
    match ModelEmbedder::current(&router).await {
        Some(model) => Box::new(model),
        None => bookclerk_discover::open_embedder(),
    }
}

/// Where a ranking on this node gets its vectors.
///
/// The space is the one that serves the library now. Texts with no stored
/// vector are embedded by `node` when it makes vectors of that space, by
/// the stand-in when the stand-in serves, and by nothing otherwise (this
/// node cannot make the serving model's vectors: it ranks what is stored).
///
/// # Arguments
///
/// * `library` - The library.
/// * `node` - This node's embedder ([`node_embedder`]).
///
/// # Errors
///
/// Returns the library read error.
pub(crate) async fn vector_source(
    library: &LibraryStore,
    node: Box<dyn Embedder>,
) -> anyhow::Result<VectorSource> {
    let serving = bookclerk_discover::serving_space(library, node.space()).await?;
    let embedder = if serving.same_as(node.space()) {
        Some(node)
    } else if bookclerk_discover::is_stand_in(&serving) {
        Some(bookclerk_discover::open_embedder())
    } else {
        None
    };
    Ok(VectorSource::new(serving, embedder))
}

/// This node's id.
///
/// # Errors
///
/// Fails when the node identity cannot be read or created.
async fn this_node(state: &AppState) -> anyhow::Result<String> {
    let files_dir = state.config.read().await.paths().files_dir.clone();
    Ok(
        bookclerk_library::control_plane::load_or_create_node_identity(&files_dir)
            .context("node identity")?
            .node_id,
    )
}

/// Admits a `discover_embed` job pinned to this node.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `trigger` - What asked for it.
///
/// # Errors
///
/// Returns the node identity or admission error.
pub(crate) async fn enqueue_discover_embed(
    state: &AppState,
    trigger: JobTrigger,
) -> anyhow::Result<AdmitJob> {
    let node_id = this_node(state).await?;
    let (max_pending, max_attempts) = queue_limits(state).await;
    admit(
        state,
        EnqueueJobSpec {
            kind: JobKind::DiscoverEmbed,
            payload: JobPayload {
                trigger,
                node_id: Some(node_id),
                ..JobPayload::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
}

/// Whether a ranking that found `current` already serving should admit a
/// `discover_embed` job anyway, claiming the slot when it should: at most
/// one an [`EMBED_RECHECK_INTERVAL`] on this daemon.
fn recheck_due(now: chrono::DateTime<Utc>) -> bool {
    let now_secs = now.timestamp();
    let last = LAST_EMBED_RECHECK.load(Ordering::Relaxed);
    now_secs.saturating_sub(last) >= EMBED_RECHECK_INTERVAL.num_seconds()
        && LAST_EMBED_RECHECK
            .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// Called by a ranking: queues the embedding of the library when this
/// node's model does not serve yet, or has not looked for changed works
/// for a while. Does nothing on a node without a model.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `current` - The space of this node's embedder.
/// * `serving` - The space the ranking reads.
pub(crate) async fn embed_if_needed(
    state: &AppState,
    current: &VectorSpace,
    serving: &VectorSpace,
) {
    if bookclerk_discover::is_stand_in(current) {
        return;
    }
    if serving.same_as(current) && !recheck_due(Utc::now()) {
        return;
    }
    if let Err(err) = enqueue_discover_embed(state, JobTrigger::Scheduler).await {
        tracing::warn!(error = %err, "could not queue the embedding of the library");
    }
}

/// Runs a `discover_embed` job: embeds every work whose vector is missing
/// or stale with this node's model, then ranks every stored feed again
/// when the vectors rankings read changed.
///
/// The embedding runs in slices of [`EMBED_SLICE`] under the Discover
/// gate, so a ranking waits a slice at most, and the job stops between
/// slices when it is cancelled. Vectors written before a failure or a
/// cancellation stay, and the next job goes on from them.
///
/// # Arguments
///
/// * `state` - The daemon.
/// * `payload` - The job's payload, naming its node.
/// * `ctx` - The claim, for cancellation.
///
/// # Returns
///
/// A one-line summary for the job row.
///
/// # Errors
///
/// Fails when the job names another node (or none), when it is cancelled,
/// on a library error, or when the model fails.
pub(crate) async fn run_discover_embed(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let node = this_node(state).await?;
    match payload.node_id.as_deref() {
        Some(pinned) if pinned == node => {}
        Some(pinned) => anyhow::bail!("discover_embed for node {pinned} claimed on {node}"),
        None => anyhow::bail!("discover_embed names no node"),
    }
    let library = state.library_snapshot().await;
    let mut embedder = node_embedder(state).await;
    let space = embedder.space().clone();
    if bookclerk_discover::is_stand_in(&space) {
        // The model went away between the admission and now; rankings
        // make the stand-in's vectors themselves.
        return Ok(String::from("no text embedding model on this node"));
    }
    let served_before = bookclerk_discover::serving_space(&library, &space).await?;
    let mut written = 0usize;
    loop {
        if job_cancelled(ctx, &library).await {
            anyhow::bail!("cancelled");
        }
        let pass = {
            let _gate = state
                .discover_gate
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| anyhow::anyhow!("discover gate closed"))?;
            bookclerk_discover::embed_dirty_works(
                &library,
                embedder.as_mut(),
                Some(Instant::now() + EMBED_SLICE),
            )
            .await?
        };
        written += pass.written;
        if pass.finished {
            break;
        }
    }
    let serving = bookclerk_discover::serving_space(&library, &space).await?;
    // Searches on this node read the new vectors at once.
    state.work_vectors.changed().await;
    let pruned = bookclerk_discover::prune_vectors(&library, &serving)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "could not prune unused vectors");
            0
        });
    // Rankings read other vectors now: a work's changed, or the model
    // took over from what served before.
    let reranked = if written > 0 || !serving.same_as(&served_before) {
        rerank_every_feed(state, &library).await
    } else {
        0
    };
    tracing::info!(
        model = %space.model,
        revision = %space.revision,
        written,
        serving = serving.same_as(&space),
        "embedded the library"
    );
    Ok(format!(
        "model={} written={written} serving={} pruned={pruned} reranked={reranked}",
        space.model,
        serving.same_as(&space),
    ))
}

/// Queues a ranking for every subject that has a stored feed, and answers
/// how many were queued.
async fn rerank_every_feed(state: &AppState, library: &LibraryStore) -> usize {
    let subjects = match library.list_discover_feed_subjects().await {
        Ok(subjects) => subjects,
        Err(err) => {
            tracing::warn!(error = %err, "could not list the discover feeds to rank again");
            return 0;
        }
    };
    let mut queued = 0;
    for subject in subjects {
        match crate::discover_jobs::enqueue_discover_feed(
            state,
            &subject,
            JobTrigger::Scheduler,
            Duration::ZERO,
        )
        .await
        {
            Ok(_) => queued += 1,
            Err(err) => {
                tracing::warn!(subject = %subject.key(), error = %err, "could not queue the discover ranking");
            }
        }
    }
    queued
}

/// Starts the task that queues a `discover_embed` job at start and when
/// the models served on this node change: a model was installed, which is
/// when the library can first be embedded with it. A job that finds every
/// work embedded writes nothing.
///
/// # Arguments
///
/// * `state` - The daemon.
pub(crate) fn spawn_model_watcher(state: Arc<AppState>) {
    let mut changes = state.deployment_runtime().ai.changes();
    // Look once at start as well: a model that was already on disk may
    // have been listed before this task subscribed, and nothing would
    // report it again.
    changes.mark_changed();
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let router = Arc::clone(&state.deployment_runtime().ai);
            if ModelEmbedder::current(&router).await.is_none() {
                continue;
            }
            if let Err(err) = enqueue_discover_embed(&state, JobTrigger::Scheduler).await {
                tracing::warn!(error = %err, "could not queue the embedding of the library");
            }
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::AtomicUsize;

    use bookclerk_integrations::{Integration, IntegrationContext, IntegrationHealth};
    use bookclerk_library::{DiscoverSubject, JobState, NewBook, SeedScope, VECTOR_KIND_WORK};
    use bookclerk_plugin_abi::ai::pack_embedding_vectors;
    use bookclerk_plugin_abi::{
        AiModel, AiModelList, AiOutput, AiRunOk, AiTextEmbeddingsOutput, DomainEvent, EventResult,
        PluginError,
    };

    use super::*;

    /// A model plugin serving one three-number text embedding model, whose
    /// revision the test can change.
    pub(crate) struct FakeModel {
        /// The revision it lists and answers with.
        revision: std::sync::Mutex<String>,
        /// Texts embedded so far.
        texts: AtomicUsize,
        /// The revision it answers with when it differs from the listing.
        answers_as: std::sync::Mutex<Option<String>>,
    }

    impl FakeModel {
        /// A model at revision `r1`.
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                revision: std::sync::Mutex::new("r1".into()),
                texts: AtomicUsize::new(0),
                answers_as: std::sync::Mutex::new(None),
            })
        }

        /// Texts it has embedded so far.
        pub(crate) fn texts_embedded(&self) -> usize {
            self.texts.load(Ordering::SeqCst)
        }

        /// The revision it lists.
        fn revision(&self) -> String {
            self.revision.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Integration for FakeModel {
        fn id(&self) -> &str {
            "embedder"
        }

        async fn start(&self, _ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
            Ok(())
        }

        async fn deliver_domain_event(
            &self,
            _event: DomainEvent,
        ) -> bookclerk_integrations::Result<EventResult> {
            Ok(EventResult::Ack)
        }

        async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
            Ok(IntegrationHealth {
                id: "embedder".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        fn supports_ai(&self) -> bool {
            true
        }

        async fn ai_models(&self) -> Result<AiModelList, PluginError> {
            Ok(AiModelList {
                models: vec![AiModel {
                    model_id: "tiny".into(),
                    task: AiTask::TextEmbeddings,
                    revision: self.revision(),
                    dimensions: Some(3),
                    max_tokens: Some(512),
                    languages: vec!["en".into(), "de".into()],
                }],
            })
        }

        async fn ai_run(&self, params: AiRunParams) -> Result<AiRunOk, PluginError> {
            let texts = &params.input.text_embeddings.texts;
            assert!(texts.len() <= MODEL_BATCH_TEXTS, "batches stay small");
            assert!(texts.iter().all(|text| !text.text.is_empty()));
            self.texts.fetch_add(texts.len(), Ordering::SeqCst);
            let vectors: Vec<Vec<f32>> = texts
                .iter()
                .map(|text| {
                    if text.text.contains("Starship") {
                        // A third subject, for tastes with three leanings.
                        return vec![0.0, 0.0, 1.0];
                    }
                    let dragons = if text.text.contains("Dragon") {
                        1.0
                    } else {
                        0.0
                    };
                    vec![dragons, 1.0 - dragons, 0.5]
                })
                .collect();
            let revision = self
                .answers_as
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| self.revision());
            Ok(AiRunOk {
                output: AiOutput {
                    text_embeddings: AiTextEmbeddingsOutput {
                        dimensions: 3,
                        vectors: pack_embedding_vectors(&vectors),
                        revision,
                        fallbacks: Vec::new(),
                    },
                },
                model: None,
            })
        }
    }

    /// A daemon over a library of `titles`, with `model` deployed when given.
    pub(crate) async fn daemon(
        files: &tempfile::TempDir,
        titles: &[&str],
        model: Option<&Arc<FakeModel>>,
    ) -> (Arc<AppState>, LibraryStore) {
        let (state, library) = crate::node_api::tests::test_state(files).await;
        library
            .upsert_account("libro", "acct", "us", None, true, "libro")
            .await
            .unwrap();
        for (n, title) in titles.iter().enumerate() {
            let id = format!("B{n:06}");
            let mut book = NewBook::minimal(&id, "acct", "us", *title);
            book.source = "libro".into();
            book.plugin_key = "libro".into();
            library.upsert_book(&book).await.unwrap();
        }
        if let Some(model) = model {
            state
                .integrations
                .write()
                .await
                .register(Arc::clone(model) as Arc<dyn Integration>);
        }
        (state, library)
    }

    /// The payload of a `discover_embed` job for this node.
    pub(crate) async fn payload(state: &AppState) -> JobPayload {
        JobPayload {
            node_id: Some(this_node(state).await.unwrap()),
            ..JobPayload::default()
        }
    }

    /// Pending jobs of `kind`.
    async fn pending(library: &LibraryStore, kind: JobKind) -> usize {
        library
            .list_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == kind && job.state == JobState::Pending)
            .count()
    }

    #[test]
    fn a_text_is_cut_at_a_character_and_never_empty() {
        assert_eq!(model_text("  Dune  "), "Dune");
        assert_eq!(model_text("   "), "untitled");
        let limit = usize::try_from(MAX_AI_EMBED_TEXT_BYTES).unwrap();
        let long = "é".repeat(limit);
        let cut = model_text(&long);
        assert!(cut.len() <= limit);
        assert!(cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_recheck_is_claimed_once_an_interval() {
        // Far in the future, so no other test's real-time claim interferes.
        let t = Utc::now() + chrono::Duration::days(3650);
        assert!(recheck_due(t));
        assert!(!recheck_due(t + chrono::Duration::minutes(5)));
        assert!(recheck_due(t + EMBED_RECHECK_INTERVAL));
    }

    #[tokio::test]
    async fn a_node_without_a_model_uses_the_stand_in() {
        let files = tempfile::tempdir().unwrap();
        let (state, library) = daemon(&files, &["Dune"], None).await;
        let node = node_embedder(&state).await;
        assert!(bookclerk_discover::is_stand_in(node.space()));
        let source = vector_source(&library, node).await.unwrap();
        assert!(bookclerk_discover::is_stand_in(&source.space));
        let summary = run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        assert_eq!(summary, "no text embedding model on this node");
    }

    #[tokio::test]
    async fn the_job_embeds_the_library_and_the_model_takes_over() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let titles: Vec<String> = (0..40).map(|n| format!("A Dragon Tale {n}")).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let (state, library) = daemon(&files, &titles, Some(&model)).await;
        library
            .put_discover_feed(&DiscoverSubject::Operator, "{}", SeedScope::Own, None)
            .await
            .unwrap();

        // Before the job: the model is this node's, the stand-in serves,
        // and a ranking queues the embedding.
        let node = node_embedder(&state).await;
        let space = node.space().clone();
        assert_eq!(space.model, "@embedder/tiny");
        assert_eq!(space.revision, "r1");
        assert_eq!(space.packs, "en,de");
        let source = vector_source(&library, node).await.unwrap();
        assert!(bookclerk_discover::is_stand_in(&source.space));
        embed_if_needed(&state, &space, &source.space).await;
        embed_if_needed(&state, &space, &source.space).await;
        assert_eq!(pending(&library, JobKind::DiscoverEmbed).await, 1);

        let summary = run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        assert!(summary.contains("written=40"), "{summary}");
        assert!(summary.contains("serving=true"), "{summary}");
        assert!(summary.contains("reranked=1"), "{summary}");
        assert_eq!(model.texts.load(Ordering::SeqCst), 40);
        assert_eq!(pending(&library, JobKind::DiscoverFeed).await, 1);
        let ids: Vec<String> = library
            .list_works_page(None, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|work| work.id)
            .collect();
        let stored = library
            .vectors(VECTOR_KIND_WORK, &space, &ids)
            .await
            .unwrap();
        assert_eq!(stored.len(), 40);

        // The model serves now, and this node embeds candidates with it.
        let source = vector_source(&library, node_embedder(&state).await)
            .await
            .unwrap();
        assert!(source.space.same_as(&space));
        assert!(source.embedder.is_some());

        // Nothing changed: a second job embeds nothing and ranks nothing.
        let summary = run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        assert!(summary.contains("written=0"), "{summary}");
        assert!(summary.contains("reranked=0"), "{summary}");
        assert_eq!(model.texts.load(Ordering::SeqCst), 40);
    }

    #[tokio::test]
    async fn a_new_revision_serves_only_once_every_work_has_its_vector() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) =
            daemon(&files, &["A Dragon Tale", "Quiet Rivers"], Some(&model)).await;
        run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        let old = node_embedder(&state).await.space().clone();

        // The plugin reloads the model at another revision.
        *model.revision.lock().unwrap() = "r2".into();
        state.deployment_runtime().ai.forget_listings().await;
        let node = node_embedder(&state).await;
        let new = node.space().clone();
        assert_eq!(new.revision, "r2");
        assert!(!new.same_as(&old));
        // The old revision's vectors keep serving, and this node, which
        // cannot make them any more, embeds no candidate.
        let source = vector_source(&library, node).await.unwrap();
        assert!(source.space.same_as(&old));
        assert!(source.embedder.is_none());

        let summary = run_discover_embed(&state, &payload(&state).await, None)
            .await
            .unwrap();
        assert!(summary.contains("written=2"), "{summary}");
        assert!(summary.contains("serving=true"), "{summary}");
        let source = vector_source(&library, node_embedder(&state).await)
            .await
            .unwrap();
        assert!(source.space.same_as(&new));
        // The old revision's vectors are young, so they stay for now.
        let spaces = library.work_vector_spaces().await.unwrap();
        assert!(spaces.iter().any(|cover| cover.space.same_as(&old)));
    }

    #[tokio::test]
    async fn vectors_of_a_revision_the_model_did_not_list_are_refused() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = daemon(&files, &["Dune"], Some(&model)).await;
        let mut embedder = ModelEmbedder::current(&state.deployment_runtime().ai)
            .await
            .unwrap();
        // The model changes under an embedder made before.
        *model.revision.lock().unwrap() = "r2".into();
        state.deployment_runtime().ai.forget_listings().await;
        let err = embedder.embed(&[String::from("Dune")]).await.unwrap_err();
        assert!(err.to_string().contains("changed while it was embedding"));
        assert!(library.work_vector_spaces().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_job_runs_only_on_the_node_it_names() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, _library) = daemon(&files, &["Dune"], Some(&model)).await;
        let other = JobPayload {
            node_id: Some("another-node".into()),
            ..JobPayload::default()
        };
        assert!(run_discover_embed(&state, &other, None).await.is_err());
        assert!(run_discover_embed(&state, &JobPayload::default(), None)
            .await
            .is_err());
        assert_eq!(model.texts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_model_that_lands_queues_the_embedding() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = daemon(&files, &["Dune"], Some(&model)).await;
        spawn_model_watcher(Arc::clone(&state));
        state.deployment_runtime().ai.forget_listings().await;
        wait_for_embed_job(&library).await;
    }

    #[tokio::test]
    async fn a_model_already_there_at_start_queues_the_embedding() {
        let files = tempfile::tempdir().unwrap();
        let model = FakeModel::new();
        let (state, library) = daemon(&files, &["Dune"], Some(&model)).await;
        // Listed, and its change reported, before anything watches.
        state.deployment_runtime().ai.forget_listings().await;
        spawn_model_watcher(Arc::clone(&state));
        wait_for_embed_job(&library).await;
    }

    /// Waits until one `discover_embed` job is pending; panics when none
    /// comes.
    async fn wait_for_embed_job(library: &LibraryStore) {
        for _ in 0..200 {
            if pending(library, JobKind::DiscoverEmbed).await == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no discover_embed job was queued");
    }
}
