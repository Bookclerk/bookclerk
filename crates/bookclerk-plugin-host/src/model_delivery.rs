//! Gives model plugins their model files.
//!
//! A model plugin package pins the models it can run in its
//! `models.json` ([`ModelOffers`]) and runs with no network, so the host
//! downloads the chosen model into the plugin's data directory
//! (`models/<model id>`) and verifies every file against the pin
//! ([`bookclerk_plugin_catalog::fetch_model`]). This module decides *when*:
//! one background worker per deployed model plugin installs the model the
//! instance's `model` setting names, retries a failed download with a
//! growing wait, and removes the models the plugin no longer uses once
//! the chosen one is in place.
//!
//! Nothing here runs unless the daemon hands a [`ModelDelivery`] to
//! [`crate::LiveDeploymentRuntime::set_model_delivery`]; a host that never
//! does (the CLI, tests) starts no download.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bookclerk_integrations::AiRouter;
use bookclerk_plugin_abi::AiTask;
use bookclerk_plugin_catalog::{
    fetch_model, model_installed, remove_other_models, ModelOffer, ModelOffers, ModelRegistry,
};
use tokio::sync::watch;

/// Key of the instance setting that names the chosen model.
pub const MODEL_SETTING_KEY: &str = "model";

/// Directory in a model plugin's data directory that holds one directory
/// per installed model.
pub const MODELS_DIR: &str = "models";

/// Wait before the first retry of a failed download.
const FIRST_RETRY: Duration = Duration::from_secs(60);

/// Longest wait between retries of a failing download.
const LONGEST_RETRY: Duration = Duration::from_secs(60 * 60);

/// Where one model stands on this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelState {
    /// Not on this node and not wanted: another model is chosen.
    NotInstalled,
    /// Chosen, and the host has not looked at the files yet.
    Checking,
    /// Chosen and being downloaded.
    Downloading {
        /// Bytes on disk so far.
        received_bytes: u64,
        /// Bytes of the whole model.
        total_bytes: u64,
    },
    /// Chosen, and the last download failed; the host tries again.
    Waiting {
        /// Why the last download failed, without addresses or tokens.
        detail: String,
    },
    /// Chosen but not on this node, and this node does not download
    /// models (`plugins.models.download = false`).
    DownloadsOff,
    /// Every file is on this node and matches the pin.
    Installed,
}

/// One offered model and where it stands on this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStatus {
    /// The model id (`bekko-a8m`).
    pub model_id: String,
    /// The offer from the plugin package: label, sizes, pin.
    pub offer: ModelOffer,
    /// Where the model stands on this node.
    pub state: ModelState,
}

/// The models of one deployed model plugin on this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginModels {
    /// The deployed plugin instance.
    pub plugin_instance_id: String,
    /// Canonical plugin key of the plugin.
    pub plugin_key: String,
    /// The model the instance's setting chose (the package's default when
    /// the setting names no offered model).
    pub chosen: String,
    /// Memory the plugin's jail lets it use, in bytes. A model that needs
    /// more cannot run on this node.
    pub memory_ceiling_bytes: u64,
    /// Every offered model, ascending by model id.
    pub models: Vec<ModelStatus>,
}

/// What one deployed model plugin should have, as the reconciler saw it at
/// the last spawn.
#[derive(Debug, Clone)]
pub struct ModelWant {
    /// The deployed plugin instance.
    pub plugin_instance_id: String,
    /// Canonical plugin key of the plugin.
    pub plugin_key: String,
    /// The plugin's id (`plugin.toml` `id`), which model names start with.
    pub plugin_id: String,
    /// Tasks the plugin's models perform; the chosen model becomes the
    /// library's model for each.
    pub serves: Vec<AiTask>,
    /// The models the package offers.
    pub offers: ModelOffers,
    /// The instance's `model` setting, when set.
    pub chosen: Option<String>,
    /// The plugin's data directory (its `HOME`).
    pub data_dir: PathBuf,
    /// Memory the plugin's jail lets it use, in bytes.
    pub memory_ceiling_bytes: u64,
}

impl ModelWant {
    /// The model to install: the setting's when the package offers it,
    /// else the package's default.
    fn model_id(&self) -> &str {
        self.chosen
            .as_deref()
            .filter(|id| self.offers.get(id).is_some())
            .unwrap_or_else(|| self.offers.default_model())
    }
}

/// One tracked plugin instance: what it wants and where the chosen model
/// stands.
struct Tracked {
    /// The latest want, for the status listing.
    want: ModelWant,
    /// Where the chosen model stands.
    state: ModelState,
    /// Sends the worker a newer want; dropping it ends the worker.
    wants: watch::Sender<ModelWant>,
}

/// State shared by the delivery handle and its workers.
struct Shared {
    /// Whether this node downloads models at all.
    download: bool,
    /// Address to reach the registry at instead of the one the package
    /// names (a mirror), as `https://host`.
    mirror: Option<String>,
    /// The router whose model choice and listings follow the installs.
    ai: Arc<AiRouter>,
    /// Tracked plugin instances by instance id.
    tracked: Mutex<HashMap<String, Tracked>>,
}

impl Shared {
    /// Records where the chosen model of `plugin_instance_id` stands, when
    /// the instance is still tracked and still wants `model_id`.
    fn set_state(&self, plugin_instance_id: &str, model_id: &str, state: ModelState) {
        let mut tracked = self.tracked.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = tracked.get_mut(plugin_instance_id) {
            if entry.want.model_id() == model_id {
                entry.state = state;
            }
        }
    }

    /// The registry `offers` are downloaded from: the mirror when one is
    /// configured and well formed, else the package's.
    fn registry(&self, offers: &ModelOffers) -> ModelRegistry {
        let named = offers.registry();
        match &self.mirror {
            Some(mirror) => named.at(mirror).unwrap_or_else(|err| {
                tracing::warn!(error = %err, "plugins.models.registry is not usable; using the package's registry");
                named
            }),
            None => named,
        }
    }
}

/// Downloads and tracks the models of this node's model plugins.
#[derive(Clone)]
pub struct ModelDelivery {
    /// State shared with the workers.
    shared: Arc<Shared>,
}

impl ModelDelivery {
    /// A delivery with no plugin tracked yet.
    ///
    /// # Arguments
    ///
    /// * `ai` - The node's model router; an installed chosen model becomes
    ///   the model its tasks' aliases name.
    /// * `download` - False to never download (`plugins.models.download`);
    ///   models an operator placed by hand are still found.
    /// * `mirror` - Address to reach the registry at instead of the one
    ///   the package names (`plugins.models.registry`).
    #[must_use]
    pub fn new(ai: Arc<AiRouter>, download: bool, mirror: Option<String>) -> Self {
        Self {
            shared: Arc::new(Shared {
                download,
                mirror,
                ai,
                tracked: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Takes the want of a plugin instance that is about to be spawned and
    /// makes sure a worker is installing its chosen model.
    ///
    /// The chosen model becomes the library's model for the plugin's
    /// tasks right away; until it is installed the router falls back to a
    /// model the plugin already lists. Must be called inside a Tokio
    /// runtime.
    ///
    /// # Arguments
    ///
    /// * `want` - What the instance should have.
    pub fn ensure(&self, want: ModelWant) {
        for task in &want.serves {
            self.shared.ai.choose(
                *task,
                Some((want.plugin_id.clone(), want.model_id().to_owned())),
            );
        }
        let mut tracked = self
            .shared
            .tracked
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = tracked.get_mut(&want.plugin_instance_id) {
            let changed = entry.want.model_id() != want.model_id()
                || entry.want.data_dir != want.data_dir
                || entry.want.offers != want.offers;
            if changed {
                entry.state = ModelState::Checking;
            }
            entry.want = want.clone();
            // An unchanged want does not wake the worker: it would hash
            // the installed model again at every respawn.
            if !changed && !entry.wants.is_closed() {
                return;
            }
            if entry.wants.send(want.clone()).is_ok() {
                return;
            }
        }
        let (wants, receiver) = watch::channel(want.clone());
        let id = want.plugin_instance_id.clone();
        tracked.insert(
            id.clone(),
            Tracked {
                want,
                state: ModelState::Checking,
                wants,
            },
        );
        drop(tracked);
        tokio::spawn(work(Arc::clone(&self.shared), id, receiver));
    }

    /// Stops following a plugin instance that no longer runs on this node.
    /// A download in flight finishes its current attempt and is not
    /// retried; the files stay for the next deployment.
    ///
    /// # Arguments
    ///
    /// * `plugin_instance_id` - The retired instance.
    pub fn forget(&self, plugin_instance_id: &str) {
        self.shared
            .tracked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(plugin_instance_id);
    }

    /// Whether this node downloads models.
    #[must_use]
    pub fn downloads(&self) -> bool {
        self.shared.download
    }

    /// The models of every tracked model plugin, ascending by instance id.
    #[must_use]
    pub fn status(&self) -> Vec<PluginModels> {
        let tracked = self
            .shared
            .tracked
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<PluginModels> = tracked
            .values()
            .map(|entry| {
                let chosen = entry.want.model_id().to_owned();
                let models = entry
                    .want
                    .offers
                    .models()
                    .map(|(model_id, offer)| ModelStatus {
                        model_id: model_id.to_owned(),
                        offer: offer.clone(),
                        state: if model_id == chosen {
                            entry.state.clone()
                        } else {
                            ModelState::NotInstalled
                        },
                    })
                    .collect();
                PluginModels {
                    plugin_instance_id: entry.want.plugin_instance_id.clone(),
                    plugin_key: entry.want.plugin_key.clone(),
                    chosen,
                    memory_ceiling_bytes: entry.want.memory_ceiling_bytes,
                    models,
                }
            })
            .collect();
        out.sort_by(|a, b| a.plugin_instance_id.cmp(&b.plugin_instance_id));
        out
    }
}

/// The wait before retry number `failures` (1 for the first).
fn retry_wait(failures: u32) -> Duration {
    let doubled = FIRST_RETRY.saturating_mul(1 << failures.saturating_sub(1).min(16));
    doubled.min(LONGEST_RETRY)
}

/// A failure as the picker shows it: the error's text with secrets
/// redacted and its length bounded.
fn failure_detail(err: &dyn std::fmt::Display) -> String {
    let text = bookclerk_config::redact_str(&err.to_string());
    text.chars().take(300).collect()
}

/// One attempt's outcome.
enum Attempt {
    /// The chosen model is installed and verified.
    Installed,
    /// This node does not download and the model is not there.
    DownloadsOff,
    /// The download failed.
    Failed(String),
}

/// Checks for, and when missing downloads, the model `want` names.
async fn attempt(shared: &Arc<Shared>, want: &ModelWant) -> Attempt {
    let model_id = want.model_id().to_owned();
    let Some(offer) = want.offers.get(&model_id).cloned() else {
        return Attempt::Failed("the plugin package offers no such model".into());
    };
    let models_dir = want.data_dir.join(MODELS_DIR);
    let installed = {
        let offer = offer.clone();
        let model_dir = models_dir.join(&model_id);
        tokio::task::spawn_blocking(move || model_installed(&offer, &model_dir)).await
    };
    match installed {
        Ok(true) => return Attempt::Installed,
        Ok(false) => {}
        Err(err) => return Attempt::Failed(failure_detail(&err)),
    }
    if !shared.download {
        return Attempt::DownloadsOff;
    }
    let total_bytes = offer.download_bytes;
    shared.set_state(
        &want.plugin_instance_id,
        &model_id,
        ModelState::Downloading {
            received_bytes: 0,
            total_bytes,
        },
    );
    let registry = shared.registry(&want.offers);
    let fetched = {
        let shared = Arc::clone(shared);
        let instance = want.plugin_instance_id.clone();
        let model_id = model_id.clone();
        tokio::task::spawn_blocking(move || {
            let progress = |received_bytes: u64, total_bytes: u64| {
                shared.set_state(
                    &instance,
                    &model_id,
                    ModelState::Downloading {
                        received_bytes,
                        total_bytes,
                    },
                );
            };
            fetch_model(&registry, &model_id, &offer, &models_dir, &progress)
        })
        .await
    };
    match fetched {
        Ok(Ok(())) => Attempt::Installed,
        Ok(Err(err)) => Attempt::Failed(failure_detail(&err)),
        Err(err) => Attempt::Failed(failure_detail(&err)),
    }
}

/// Removes every model but `keep` from `models_dir`; a failure (a file the
/// plugin still holds open on Windows) is logged and tried again at the
/// next spawn.
async fn remove_others(models_dir: &Path, keep: &str) {
    let models_dir = models_dir.to_path_buf();
    let keep = keep.to_owned();
    let removed =
        tokio::task::spawn_blocking(move || remove_other_models(&models_dir, &[keep.as_str()]))
            .await;
    match removed {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::warn!(error = %err, "unused model files could not be removed"),
        Err(err) => tracing::warn!(error = %err, "removing unused model files did not finish"),
    }
}

/// The worker of one plugin instance: installs the wanted model, retries
/// failures, and ends when the instance is forgotten.
async fn work(
    shared: Arc<Shared>,
    plugin_instance_id: String,
    mut wants: watch::Receiver<ModelWant>,
) {
    let mut failures = 0_u32;
    loop {
        let want = wants.borrow_and_update().clone();
        let model_id = want.model_id().to_owned();
        let wait = match attempt(&shared, &want).await {
            Attempt::Installed => {
                failures = 0;
                remove_others(&want.data_dir.join(MODELS_DIR), &model_id).await;
                shared.set_state(&plugin_instance_id, &model_id, ModelState::Installed);
                // The plugin lists the new directory from now on; the
                // router must ask it again.
                shared.ai.forget_listings().await;
                tracing::info!(plugin = %want.plugin_key, model = %model_id, "model is installed");
                None
            }
            Attempt::DownloadsOff => {
                shared.set_state(&plugin_instance_id, &model_id, ModelState::DownloadsOff);
                None
            }
            Attempt::Failed(detail) => {
                failures = failures.saturating_add(1);
                let wait = retry_wait(failures);
                tracing::warn!(
                    plugin = %want.plugin_key,
                    model = %model_id,
                    retry_in_secs = wait.as_secs(),
                    error = %detail,
                    "model download failed"
                );
                shared.set_state(
                    &plugin_instance_id,
                    &model_id,
                    ModelState::Waiting { detail },
                );
                Some(wait)
            }
        };
        let changed = match wait {
            Some(wait) => tokio::select! {
                changed = wants.changed() => changed,
                () = tokio::time::sleep(wait) => Ok(()),
            },
            None => wants.changed().await,
        };
        if changed.is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[path = "model_delivery_tests.rs"]
mod tests;
