//! The plugin's `ai` entrypoint over the model directories on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bookclerk_plugin_sdk::ai::{is_ai_model_id, pack_embedding_vectors};
use bookclerk_plugin_sdk::{
    manifest_capabilities, AiModelList, AiModels, AiOutput, AiRunOk, AiRunParams,
    AiTextEmbeddingsOutput, AiTextRole, Bindings, Entrypoints, HealthOk, Invocation,
    PluginDescribe, PluginError, PluginWorker, ScalarLimits, FEATURE_SCALAR_LIMITS, MAX_AI_MODELS,
    PRODUCT_API_VERSION,
};

use crate::model::{inspect, load, Listing, LoadedModel, ModelError};
use crate::text::Role;
use crate::{engine, ID, MODELS_DIR, RUNTIME_LIBRARY};

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// What every invocation of the plugin shares: where its files are and
/// the models already in memory.
#[derive(Debug)]
struct Shared {
    /// `models/` in the plugin's data directory; `None` when the plugin
    /// was started without one.
    models_dir: Option<PathBuf>,
    /// The ONNX Runtime shared library; `None` when the plugin's own
    /// location is unknown.
    library: Option<PathBuf>,
    /// Loaded models by model id. A model is loaded on its first run and
    /// loaded again when the files on disk give another revision.
    loaded: Mutex<BTreeMap<String, Arc<LoadedModel>>>,
}

/// Root capability of the `embedder` guest.
#[derive(Debug, Clone)]
pub struct EmbedderRoot {
    /// State shared with every entrypoint this root opens.
    shared: Arc<Shared>,
}

impl EmbedderRoot {
    /// A plugin over explicit locations.
    ///
    /// # Arguments
    ///
    /// * `models_dir` - Directory with one directory per model.
    /// * `library` - The ONNX Runtime shared library.
    #[must_use]
    pub fn new(models_dir: Option<PathBuf>, library: Option<PathBuf>) -> Self {
        Self {
            shared: Arc::new(Shared {
                models_dir,
                library,
                loaded: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// The plugin as the host starts it: models under `models/` in its
    /// data directory (its `HOME`), ONNX Runtime beside its executable.
    #[must_use]
    pub fn from_environment() -> Self {
        let models_dir = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(MODELS_DIR));
        let library = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(RUNTIME_LIBRARY)));
        Self::new(models_dir, library)
    }
}

/// The model directories found and what was wrong with the others.
#[derive(Debug, Default)]
struct Found {
    /// Usable models, ascending by model id.
    listings: Vec<Listing>,
    /// One line per directory that was skipped.
    problems: Vec<String>,
}

impl Shared {
    /// Looks at every model directory. Cheap: no model is loaded.
    fn found(&self) -> Found {
        let mut found = Found::default();
        let Some(models_dir) = &self.models_dir else {
            return found;
        };
        let Ok(entries) = std::fs::read_dir(models_dir) else {
            // No directory yet: no model has been installed.
            return found;
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| is_ai_model_id(name))
            .collect();
        names.sort_unstable();
        let cap = usize::try_from(MAX_AI_MODELS).unwrap_or(usize::MAX);
        for name in names {
            match inspect(&models_dir.join(&name)) {
                Ok(listing) if found.listings.len() < cap => found.listings.push(listing),
                Ok(_) => found.problems.push(format!("{name}: over the model limit")),
                Err(err) => found.problems.push(format!("{name}: {err}")),
            }
        }
        found
    }

    /// The model in memory for a listing, loading it when it is not there
    /// or was loaded from other files.
    fn loaded(&self, model: &str, listing: &Listing) -> Result<Arc<LoadedModel>> {
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(current) = loaded.get(model) {
            if current.revision() == listing.revision() {
                return Ok(Arc::clone(current));
            }
        }
        // Drop the old model first so two copies are never held at once.
        loaded.remove(model);
        let library = self
            .library
            .as_deref()
            .ok_or_else(|| PluginError::unavailable("ONNX Runtime library location unknown"))?;
        let environment = engine::runtime(library).map_err(unavailable)?;
        let fresh = Arc::new(load(listing, environment).map_err(unavailable)?);
        loaded.insert(model.to_owned(), Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Runs one request; blocking.
    fn run(&self, params: &AiRunParams) -> Result<AiRunOk> {
        let texts = &params.input.text_embeddings.texts;
        if texts.is_empty() {
            return Err(PluginError::invalid_params(
                "this plugin's models take text embeddings input",
            ));
        }
        let dir = self
            .models_dir
            .as_deref()
            .filter(|_| is_ai_model_id(&params.model))
            .map(|models_dir| models_dir.join(&params.model))
            .filter(|dir| dir.is_dir())
            .ok_or_else(|| PluginError::not_found(format!("no model `{}`", params.model)))?;
        let listing = inspect(&dir).map_err(unavailable)?;
        let model = self.loaded(&params.model, &listing)?;
        let mut vectors = Vec::with_capacity(texts.len());
        let mut fallbacks = Vec::new();
        for (index, text) in texts.iter().enumerate() {
            let role = match text.role {
                AiTextRole::Document => Role::Document,
                AiTextRole::Query => Role::Query,
            };
            let embedding = model.embed(&text.text, role).map_err(internal)?;
            if embedding.fallback {
                fallbacks.push(
                    u32::try_from(index)
                        .map_err(|_| PluginError::invalid_params("too many texts"))?,
                );
            }
            vectors.push(embedding.vector);
        }
        Ok(AiRunOk {
            output: AiOutput {
                text_embeddings: AiTextEmbeddingsOutput {
                    dimensions: model.dimensions(),
                    vectors: pack_embedding_vectors(&vectors),
                    revision: model.revision().to_owned(),
                    fallbacks,
                },
            },
            model: None,
        })
    }
}

/// A model that cannot be loaded now: files missing, changed or refused.
fn unavailable(err: impl std::fmt::Display) -> PluginError {
    PluginError::unavailable(err.to_string())
}

/// A loaded model that failed on a text.
fn internal(err: ModelError) -> PluginError {
    PluginError::internal(err.to_string())
}

/// The `ai` entrypoint of one invocation.
#[derive(Debug)]
struct Embedder {
    /// The plugin's shared state.
    shared: Arc<Shared>,
}

#[async_trait(?Send)]
impl AiModels for Embedder {
    async fn health(&self) -> Result<HealthOk> {
        let found = self.shared.found();
        let library_present = self.shared.library.as_deref().is_some_and(Path::is_file);
        let mut detail = if library_present {
            format!("{} model(s) installed", found.listings.len())
        } else {
            "the ONNX Runtime library is missing beside the plugin".to_owned()
        };
        for problem in &found.problems {
            detail.push_str("; ");
            detail.push_str(problem);
        }
        Ok(HealthOk {
            ok: library_present,
            detail,
        })
    }

    async fn models(&self) -> Result<AiModelList> {
        Ok(AiModelList {
            models: self
                .shared
                .found()
                .listings
                .iter()
                .map(Listing::info)
                .collect(),
        })
    }

    async fn run(&self, params: AiRunParams) -> Result<AiRunOk> {
        let shared = Arc::clone(&self.shared);
        // Loading a model and running it take tens of milliseconds to
        // seconds; off the RPC thread, so health checks still answer.
        tokio::task::spawn_blocking(move || shared.run(&params))
            .await
            .map_err(|err| PluginError::internal(format!("model run stopped: {err}")))?
    }
}

#[async_trait(?Send)]
impl PluginWorker for EmbedderRoot {
    async fn describe(&self) -> Result<PluginDescribe> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: ID.into(),
            display_name: Some("Text embeddings".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            ..PluginDescribe::default()
        })
    }

    async fn open(&self, _invocation: Invocation, _bindings: Bindings) -> Result<Entrypoints> {
        Ok(Entrypoints {
            ai: Some(Box::new(Embedder {
                shared: Arc::clone(&self.shared),
            })),
            ..Entrypoints::default()
        })
    }
}
