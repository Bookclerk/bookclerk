//! The host's model router: one place that runs a model by name.
//!
//! Models are served by plugins with the `ai` entrypoint
//! ([`Integration::supports_ai`]). The router finds the deployed plugin
//! that serves a named model, or the model the library chose for a task
//! when the name is a task alias, runs it under a deadline and checks the
//! reply before anyone sees it. The `AI` binding of every plugin and the
//! host's own features call it the same way, so a model's reply is trusted
//! nowhere.
//!
//! The router reaches the model plugins deployed on this node. Nothing in
//! this process loads a model.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use bookclerk_plugin_abi::ai::{check_binding_run_params, check_run_output, AiError, AiModelName};
use bookclerk_plugin_abi::{AiModel, AiRunOk, AiRunParams, AiTask, PluginError, PluginErrorCode};
use tracing::warn;

use crate::registry::IntegrationRegistry;
use crate::traits::Integration;

/// Longest one model run may take before the router gives up on it. A
/// full batch on the largest first-party model on one slow core fits; a
/// caller with a shorter deadline passes its own.
pub const AI_RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// Longest a model plugin may take to list its models. Listing loads no
/// model, so this mostly covers waking a sleeping plugin.
pub const AI_LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// One model a deployed plugin serves, as the router lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedModel {
    /// Id of the plugin that serves the model (`plugin.toml` `id`).
    pub plugin: String,
    /// The model as the plugin listed it.
    pub model: AiModel,
}

impl RoutedModel {
    /// The name callers use for this model: `@<plugin id>/<model id>`.
    #[must_use]
    pub fn name(&self) -> String {
        format!("@{}/{}", self.plugin, self.model.model_id)
    }
}

/// A cached model listing of one deployed plugin.
struct Listing {
    /// The integration the listing came from. A reload replaces the
    /// integration, and its listing with it.
    of: Weak<dyn Integration>,
    /// The models it listed.
    models: Arc<Vec<AiModel>>,
}

/// Routes model runs to the deployed model plugins.
pub struct AiRouter {
    /// The node's live integrations; model plugins are the ones that
    /// export `ai`.
    integrations: Arc<tokio::sync::RwLock<IntegrationRegistry>>,
    /// Listings by plugin id, so a run does not list first.
    listings: tokio::sync::Mutex<HashMap<String, Listing>>,
    /// The model the library chose for each task. A task without a choice
    /// goes to the first deployed model that performs it.
    chosen: std::sync::RwLock<HashMap<AiTask, (String, String)>>,
    /// Counts the times the models served here may have changed
    /// ([`Self::forget_listings`]), for whoever keeps something derived
    /// from a model current ([`Self::changes`]).
    changed: tokio::sync::watch::Sender<u64>,
}

impl AiRouter {
    /// A router over the node's live integrations.
    ///
    /// # Arguments
    ///
    /// * `integrations` - The registry the deployment reconciler keeps.
    ///
    /// # Returns
    ///
    /// The router, with no model chosen for any task.
    #[must_use]
    pub fn new(integrations: Arc<tokio::sync::RwLock<IntegrationRegistry>>) -> Self {
        Self {
            integrations,
            listings: tokio::sync::Mutex::new(HashMap::new()),
            chosen: std::sync::RwLock::new(HashMap::new()),
            changed: tokio::sync::watch::Sender::new(0),
        }
    }

    /// Sets or clears the model a task alias resolves to.
    ///
    /// # Arguments
    ///
    /// * `task` - The task (`@library/text-embeddings` for text embeddings).
    /// * `model` - The chosen model as `(plugin id, model id)`, or `None`
    ///   to fall back to the first deployed model of the task.
    pub fn choose(&self, task: AiTask, model: Option<(String, String)>) {
        let mut chosen = self
            .chosen
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match model {
            Some(model) => {
                chosen.insert(task, model);
            }
            None => {
                chosen.remove(&task);
            }
        }
    }

    /// Drops every cached listing, so the next run or listing asks each
    /// plugin again. The host calls this after it installed or removed a
    /// model's files, which changes what a running plugin lists.
    pub async fn forget_listings(&self) {
        self.listings.lock().await.clear();
        self.changed
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    /// A receiver that changes each time the models served on this node
    /// may have changed: a model was installed or removed
    /// ([`Self::forget_listings`]).
    ///
    /// # Returns
    ///
    /// The receiver; its value is a count with no meaning of its own.
    #[must_use]
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// The deployed model plugins, in registration order.
    async fn plugins(&self) -> Vec<Arc<dyn Integration>> {
        self.integrations.read().await.model_plugins()
    }

    /// One plugin's listing, from the cache when the same integration
    /// produced it.
    ///
    /// # Errors
    ///
    /// The plugin's error, or `deadline_exceeded` when it did not answer
    /// within [`AI_LIST_TIMEOUT`].
    async fn listing(
        &self,
        plugin: &Arc<dyn Integration>,
        refresh: bool,
    ) -> Result<Arc<Vec<AiModel>>, PluginError> {
        let key = plugin.id().to_string();
        if !refresh {
            let listings = self.listings.lock().await;
            if let Some(listing) = listings.get(&key) {
                let same = listing
                    .of
                    .upgrade()
                    .is_some_and(|of| Arc::ptr_eq(&of, plugin));
                if same {
                    return Ok(Arc::clone(&listing.models));
                }
            }
        }
        let list = tokio::time::timeout(AI_LIST_TIMEOUT, plugin.ai_models())
            .await
            .unwrap_or_else(|_| {
                Err(PluginError::deadline_exceeded(format!(
                    "model plugin `{key}` did not list its models in time"
                )))
            })?;
        let models = Arc::new(list.models);
        self.listings.lock().await.insert(
            key,
            Listing {
                of: Arc::downgrade(plugin),
                models: Arc::clone(&models),
            },
        );
        Ok(models)
    }

    /// Every model the deployed model plugins serve. A plugin that cannot
    /// list is left out and logged.
    ///
    /// # Returns
    ///
    /// The models, by plugin in registration order.
    pub async fn models(&self) -> Vec<RoutedModel> {
        let mut out = Vec::new();
        for plugin in self.plugins().await {
            match self.listing(&plugin, false).await {
                Ok(models) => out.extend(models.iter().map(|model| RoutedModel {
                    plugin: plugin.id().to_string(),
                    model: model.clone(),
                })),
                Err(err) => warn!(
                    plugin = plugin.plugin_key(),
                    error = %err,
                    "model plugin could not list its models"
                ),
            }
        }
        out
    }

    /// The model a task alias names now.
    ///
    /// # Arguments
    ///
    /// * `task` - The task.
    ///
    /// # Returns
    ///
    /// The chosen model when it is deployed, else the first deployed model
    /// of the task; `None` when nothing deployed performs the task.
    pub async fn model_for(&self, task: AiTask) -> Option<RoutedModel> {
        let models = self.models().await;
        let chosen = self
            .chosen
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&task)
            .cloned();
        let of_task = |m: &&RoutedModel| m.model.task == task;
        chosen
            .and_then(|(plugin, model)| {
                models
                    .iter()
                    .filter(of_task)
                    .find(|m| m.plugin == plugin && m.model.model_id == model)
            })
            .or_else(|| models.iter().find(of_task))
            .cloned()
    }

    /// Finds the plugin and listed model a name stands for.
    async fn resolve(
        &self,
        name: &AiModelName,
    ) -> Result<(Arc<dyn Integration>, AiModel), PluginError> {
        let (plugin_id, model_id) = match name {
            AiModelName::Model { plugin, model } => (plugin.clone(), model.clone()),
            AiModelName::Alias(task) => {
                let routed = self.model_for(*task).await.ok_or_else(|| {
                    PluginError::not_found(format!("no deployed model plugin serves `{name}`"))
                })?;
                (routed.plugin, routed.model.model_id)
            }
        };
        let missing = || PluginError::not_found(format!("no deployed plugin serves `{name}`"));
        let plugin = self
            .plugins()
            .await
            .into_iter()
            .find(|plugin| plugin.id() == plugin_id)
            .ok_or_else(missing)?;
        let models = self
            .listing(&plugin, false)
            .await
            .map_err(|err| plugin_failure(plugin.as_ref(), &err))?;
        let model = models
            .iter()
            .find(|model| model.model_id == model_id)
            .cloned()
            .ok_or_else(missing)?;
        Ok((plugin, model))
    }

    /// Runs a model by name and returns its checked output.
    ///
    /// # Arguments
    ///
    /// * `caller` - Id of the plugin that asks, or `None` for the host's
    ///   own features. A plugin cannot run its own models through the
    ///   router: it would wait on itself.
    /// * `params` - The model name (`@<plugin id>/<model id>` or a task
    ///   alias) and the input.
    /// * `timeout` - Longest the run may take; capped at
    ///   [`AI_RUN_TIMEOUT`].
    ///
    /// # Returns
    ///
    /// The output with `model` set to the full name of the model that ran.
    ///
    /// # Errors
    ///
    /// `invalid_params` for a malformed run or an input of another task
    /// than the model's, `not_found` when nothing deployed serves the
    /// model, `deadline_exceeded` when the plugin does not answer in time,
    /// `unavailable` when it fails or sends a reply that fails
    /// [`check_run_output`], and the plugin's own `invalid_params`,
    /// `not_found`, `rate_limited` and `unavailable`.
    pub async fn run(
        &self,
        caller: Option<&str>,
        params: AiRunParams,
        timeout: Duration,
    ) -> Result<AiRunOk, PluginError> {
        let (name, task) = check_binding_run_params(&params)?;
        let (plugin, model) = self.resolve(&name).await?;
        if model.task != task {
            return Err(PluginError::invalid_params(format!(
                "the input is for `{}` but `{name}` performs `{}`",
                task.wire_name(),
                model.task.wire_name()
            )));
        }
        if caller == Some(plugin.id()) {
            return Err(PluginError::invalid_params(
                "a plugin cannot run its own models through `AI`",
            ));
        }
        let full_name = format!("@{}/{}", plugin.id(), model.model_id);
        let input = params.input;
        let run = plugin.ai_run(AiRunParams {
            model: model.model_id.clone(),
            input: input.clone(),
        });
        let mut reply = tokio::time::timeout(timeout.min(AI_RUN_TIMEOUT), run)
            .await
            .unwrap_or_else(|_| {
                Err(PluginError::deadline_exceeded(format!(
                    "`{full_name}` did not answer in time"
                )))
            })
            .map_err(|err| plugin_failure(plugin.as_ref(), &err))?;
        let mut checked = check_run_output(&model, &input, &reply);
        if matches!(
            checked,
            Err(AiError::RevisionChanged | AiError::DimensionsChanged)
        ) {
            // The plugin may have reloaded with other language packs since
            // it was listed: list again and judge the reply by that.
            let fresh = self
                .listing(&plugin, true)
                .await
                .map_err(|err| plugin_failure(plugin.as_ref(), &err))?;
            checked = fresh
                .iter()
                .find(|fresh| fresh.model_id == model.model_id)
                .ok_or(AiError::RevisionChanged)
                .and_then(|fresh| check_run_output(fresh, &input, &reply));
        }
        if let Err(err) = checked {
            warn!(
                plugin = plugin.plugin_key(),
                model = %full_name,
                error = %err,
                "model plugin sent a malformed reply"
            );
            return Err(PluginError::unavailable(format!(
                "`{full_name}` sent a malformed reply"
            )));
        }
        reply.model = Some(full_name);
        Ok(reply)
    }
}

/// What a caller is told when a model plugin fails: the plugin's own
/// answer for the codes that are about the call, and a plain `unavailable`
/// for everything else, so a caller never reads another plugin's internal
/// error text.
fn plugin_failure(plugin: &dyn Integration, err: &PluginError) -> PluginError {
    match err.code {
        PluginErrorCode::InvalidParams
        | PluginErrorCode::NotFound
        | PluginErrorCode::RateLimited
        | PluginErrorCode::DeadlineExceeded
        | PluginErrorCode::Unsupported => err.clone(),
        PluginErrorCode::Unauthorized
        | PluginErrorCode::Forbidden
        | PluginErrorCode::Unavailable
        | PluginErrorCode::Internal
        | PluginErrorCode::PayloadTooLarge
        | PluginErrorCode::InvalidCursor
        | PluginErrorCode::Cancelled
        | PluginErrorCode::Conflict
        | PluginErrorCode::LimitExceeded
        | PluginErrorCode::NetworkNotGranted
        | PluginErrorCode::Unknown => {
            warn!(
                plugin = plugin.plugin_key(),
                error = %err,
                "model plugin call failed"
            );
            PluginError::unavailable(format!("model plugin `{}` is unavailable", plugin.id()))
        }
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use bookclerk_plugin_abi::ai::pack_embedding_vectors;
    use bookclerk_plugin_abi::{
        AiEmbedText, AiInput, AiModelList, AiOutput, AiTextEmbeddingsInput, AiTextEmbeddingsOutput,
        AiTextRole, DomainEvent, EventResult,
    };

    use super::*;
    use crate::error::Result;
    use crate::traits::IntegrationContext;
    use crate::types::IntegrationHealth;

    /// How a fake model plugin answers a run.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        /// One vector per text, as listed.
        Answers,
        /// One vector too few.
        ShortReply,
        /// A vector holding a NaN.
        NotFinite,
        /// Answers under a revision it has not listed yet, and lists that
        /// revision from then on.
        Reloaded,
        /// Fails with an internal error that names a private path.
        Fails,
        /// Never answers.
        Hangs,
    }

    /// A model plugin that serves one two-number embedding model.
    struct Fake {
        id: &'static str,
        model: &'static str,
        mode: Mode,
        lists: AtomicUsize,
        runs: AtomicUsize,
    }

    impl Fake {
        fn new(id: &'static str, model: &'static str, mode: Mode) -> Arc<Self> {
            Arc::new(Self {
                id,
                model,
                mode,
                lists: AtomicUsize::new(0),
                runs: AtomicUsize::new(0),
            })
        }

        fn revision(&self) -> String {
            let reloaded = self.mode == Mode::Reloaded && self.runs.load(Ordering::SeqCst) > 0;
            if reloaded { "r2" } else { "r1" }.to_string()
        }
    }

    #[async_trait]
    impl Integration for Fake {
        fn id(&self) -> &str {
            self.id
        }

        async fn start(&self, _ctx: IntegrationContext) -> Result<()> {
            Ok(())
        }

        async fn deliver_domain_event(&self, _event: DomainEvent) -> Result<EventResult> {
            Ok(EventResult::Ack)
        }

        async fn health(&self) -> Result<IntegrationHealth> {
            Ok(IntegrationHealth {
                id: self.id.into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        fn supports_ai(&self) -> bool {
            true
        }

        async fn ai_models(&self) -> std::result::Result<AiModelList, PluginError> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            Ok(AiModelList {
                models: vec![AiModel {
                    model_id: self.model.into(),
                    task: AiTask::TextEmbeddings,
                    revision: self.revision(),
                    dimensions: Some(2),
                    max_tokens: Some(512),
                    languages: vec!["en".into()],
                }],
            })
        }

        async fn ai_run(&self, params: AiRunParams) -> std::result::Result<AiRunOk, PluginError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            assert_eq!(params.model, self.model, "called by its own model id");
            let texts = params.input.text_embeddings.texts.len();
            let mut vectors = vec![vec![0.25_f32, -0.5]; texts];
            match self.mode {
                Mode::Answers | Mode::Reloaded => {}
                Mode::ShortReply => {
                    vectors.pop();
                }
                Mode::NotFinite => vectors[0][1] = f32::NAN,
                Mode::Fails => {
                    return Err(PluginError::internal("/srv/models/secret.onnx missing"))
                }
                Mode::Hangs => std::future::pending::<()>().await,
            }
            Ok(AiRunOk {
                output: AiOutput {
                    text_embeddings: AiTextEmbeddingsOutput {
                        dimensions: 2,
                        vectors: pack_embedding_vectors(&vectors),
                        revision: self.revision(),
                        fallbacks: Vec::new(),
                    },
                },
                model: None,
            })
        }
    }

    /// An integration that serves no model.
    struct Plain;

    #[async_trait]
    impl Integration for Plain {
        fn id(&self) -> &str {
            "plain"
        }

        async fn start(&self, _ctx: IntegrationContext) -> Result<()> {
            Ok(())
        }

        async fn deliver_domain_event(&self, _event: DomainEvent) -> Result<EventResult> {
            Ok(EventResult::Ack)
        }

        async fn health(&self) -> Result<IntegrationHealth> {
            Ok(IntegrationHealth {
                id: "plain".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }
    }

    fn router(plugins: &[Arc<Fake>]) -> AiRouter {
        let mut registry = IntegrationRegistry::new();
        registry.register(Arc::new(Plain));
        for plugin in plugins {
            registry.register(Arc::clone(plugin) as Arc<dyn Integration>);
        }
        AiRouter::new(Arc::new(tokio::sync::RwLock::new(registry)))
    }

    fn run(model: &str, texts: &[&str]) -> AiRunParams {
        AiRunParams {
            model: model.into(),
            input: AiInput {
                text_embeddings: AiTextEmbeddingsInput {
                    texts: texts
                        .iter()
                        .map(|text| AiEmbedText {
                            text: (*text).to_string(),
                            role: AiTextRole::Document,
                        })
                        .collect(),
                },
            },
        }
    }

    const SOON: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_named_model_runs_and_the_reply_says_who_answered() {
        let embedder = Fake::new("embedder", "tiny", Mode::Answers);
        let router = router(&[Arc::clone(&embedder)]);
        let ok = router
            .run(Some("caller"), run("@embedder/tiny", &["a", "b"]), SOON)
            .await
            .unwrap();
        assert_eq!(ok.model.as_deref(), Some("@embedder/tiny"));
        assert_eq!(ok.output.text_embeddings.vectors.len(), 16);
        router
            .run(None, run("@embedder/tiny", &["c"]), SOON)
            .await
            .unwrap();
        assert_eq!(embedder.lists.load(Ordering::SeqCst), 1, "listed once");
        assert_eq!(embedder.runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn forgetting_the_listings_tells_whoever_watches_the_models() {
        let embedder = Fake::new("embedder", "tiny", Mode::Answers);
        let router = router(&[Arc::clone(&embedder)]);
        let mut changes = router.changes();
        assert!(!changes.has_changed().unwrap(), "nothing changed yet");
        router.model_for(AiTask::TextEmbeddings).await.unwrap();
        router.forget_listings().await;
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();
        router.model_for(AiTask::TextEmbeddings).await.unwrap();
        assert_eq!(embedder.lists.load(Ordering::SeqCst), 2, "listed again");
        assert!(!changes.has_changed().unwrap(), "a listing is no change");
    }

    #[tokio::test]
    async fn an_alias_goes_to_the_chosen_model_or_the_first_of_the_task() {
        let first = Fake::new("first", "small", Mode::Answers);
        let second = Fake::new("second", "large", Mode::Answers);
        let router = router(&[Arc::clone(&first), Arc::clone(&second)]);
        let alias = "@library/text-embeddings";
        let ok = router.run(None, run(alias, &["a"]), SOON).await.unwrap();
        assert_eq!(ok.model.as_deref(), Some("@first/small"));
        router.choose(
            AiTask::TextEmbeddings,
            Some(("second".into(), "large".into())),
        );
        let ok = router.run(None, run(alias, &["a"]), SOON).await.unwrap();
        assert_eq!(ok.model.as_deref(), Some("@second/large"));
        router.choose(
            AiTask::TextEmbeddings,
            Some(("gone".into(), "model".into())),
        );
        let routed = router.model_for(AiTask::TextEmbeddings).await.unwrap();
        assert_eq!(routed.name(), "@first/small", "a missing choice falls back");
        router.choose(AiTask::TextEmbeddings, None);
        assert_eq!(router.models().await.len(), 2);
    }

    #[tokio::test]
    async fn unknown_models_and_malformed_runs_are_refused_before_any_plugin_runs() {
        let embedder = Fake::new("embedder", "tiny", Mode::Answers);
        let router = router(&[Arc::clone(&embedder)]);
        for (params, code) in [
            (run("@embedder/other", &["a"]), PluginErrorCode::NotFound),
            (run("@nobody/tiny", &["a"]), PluginErrorCode::NotFound),
            (run("@plain/tiny", &["a"]), PluginErrorCode::NotFound),
            (run("tiny", &["a"]), PluginErrorCode::InvalidParams),
            (run("@embedder/tiny", &[]), PluginErrorCode::InvalidParams),
        ] {
            let err = router.run(None, params, SOON).await.unwrap_err();
            assert_eq!(err.code, code, "{}", err.message);
        }
        let own = router
            .run(Some("embedder"), run("@embedder/tiny", &["a"]), SOON)
            .await
            .unwrap_err();
        assert_eq!(own.code, PluginErrorCode::InvalidParams);
        assert_eq!(embedder.runs.load(Ordering::SeqCst), 0);
        let empty = AiRouter::new(Arc::new(tokio::sync::RwLock::new(
            IntegrationRegistry::new(),
        )));
        let err = empty
            .run(None, run("@library/text-embeddings", &["a"]), SOON)
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::NotFound);
    }

    #[tokio::test]
    async fn malformed_replies_never_reach_the_caller() {
        for mode in [Mode::ShortReply, Mode::NotFinite] {
            let router = router(&[Fake::new("embedder", "tiny", mode)]);
            let err = router
                .run(None, run("@embedder/tiny", &["a", "b"]), SOON)
                .await
                .unwrap_err();
            assert_eq!(err.code, PluginErrorCode::Unavailable);
            assert_eq!(err.message, "`@embedder/tiny` sent a malformed reply");
        }
    }

    #[tokio::test]
    async fn a_reloaded_model_is_listed_again_and_its_reply_accepted() {
        let embedder = Fake::new("embedder", "tiny", Mode::Reloaded);
        let router = router(&[Arc::clone(&embedder)]);
        let ok = router
            .run(None, run("@embedder/tiny", &["a"]), SOON)
            .await
            .unwrap();
        assert_eq!(ok.output.text_embeddings.revision, "r2");
        assert_eq!(embedder.lists.load(Ordering::SeqCst), 2);
        let listed = router.model_for(AiTask::TextEmbeddings).await.unwrap();
        assert_eq!(listed.model.revision, "r2");
    }

    #[tokio::test]
    async fn a_failing_plugin_is_unavailable_without_its_error_text() {
        let router = router(&[Fake::new("embedder", "tiny", Mode::Fails)]);
        let err = router
            .run(None, run("@embedder/tiny", &["a"]), SOON)
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::Unavailable);
        assert_eq!(err.message, "model plugin `embedder` is unavailable");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_plugin_ends_at_the_deadline() {
        let router = router(&[Fake::new("embedder", "tiny", Mode::Hangs)]);
        let err = router
            .run(None, run("@embedder/tiny", &["a"]), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::DeadlineExceeded);
    }
}
