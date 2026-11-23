//! Tests for [`super::ModelDelivery`] against a registry on loopback.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_integrations::{AiRouter, IntegrationRegistry};
use bookclerk_plugin_abi::AiTask;
use bookclerk_plugin_catalog::model_fetch_testing::{serve, Pack};
use bookclerk_plugin_catalog::ModelOffers;

use super::*;

/// A router over no plugins.
fn router() -> Arc<AiRouter> {
    Arc::new(AiRouter::new(Arc::new(tokio::sync::RwLock::new(
        IntegrationRegistry::new(),
    ))))
}

/// A model list that offers every pack under its id; the first is the
/// default.
fn offers(packs: &[(&str, &Pack)]) -> ModelOffers {
    let models: serde_json::Map<String, serde_json::Value> = packs
        .iter()
        .map(|(id, pack)| {
            let offer = pack.offer();
            (
                (*id).to_owned(),
                serde_json::json!({
                    "label": offer.label,
                    "summary": offer.summary,
                    "model_json": {
                        "bytes": offer.model_json.bytes,
                        "sha256": offer.model_json.sha256.as_str(),
                    },
                    "download_bytes": offer.download_bytes,
                    "memory_bytes": offer.memory_bytes,
                }),
            )
        })
        .collect();
    let json = serde_json::json!({
        "registry": "registry.example/acme/models",
        "default": packs[0].0,
        "models": models,
    });
    ModelOffers::parse(&serde_json::to_vec(&json).expect("json")).expect("offers")
}

/// The want of instance `pi_1` for `chosen` out of `offers`.
fn want(offers: &ModelOffers, chosen: Option<&str>, data_dir: &Path) -> ModelWant {
    ModelWant {
        plugin_instance_id: "pi_1".into(),
        plugin_key: "bookclerk/embedder".into(),
        plugin_id: "embedder".into(),
        serves: vec![AiTask::TextEmbeddings],
        offers: offers.clone(),
        chosen: chosen.map(str::to_owned),
        data_dir: data_dir.to_path_buf(),
        memory_ceiling_bytes: 512 << 20,
    }
}

/// Blobs of every pack together.
fn blobs(packs: &[&Pack]) -> BTreeMap<String, Vec<u8>> {
    packs.iter().flat_map(|pack| pack.blobs()).collect()
}

/// Waits until the chosen model of the only tracked instance reaches a
/// state `done` accepts, and returns the status then.
async fn settled(delivery: &ModelDelivery, done: impl Fn(&ModelState) -> bool) -> PluginModels {
    for _ in 0..400 {
        if let Some(status) = delivery.status().into_iter().next() {
            let chosen = status
                .models
                .iter()
                .find(|model| model.model_id == status.chosen)
                .expect("the chosen model is offered");
            if done(&chosen.state) {
                return status;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the model did not settle: {:?}", delivery.status());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_model_is_downloaded_and_a_new_choice_replaces_it() {
    let (tiny, other) = (Pack::new("tiny"), Pack::new("other"));
    let offers = offers(&[("tiny", &tiny), ("other", &other)]);
    let (base, seen) = serve(blobs(&[&tiny, &other]), None);
    let home = tempfile::tempdir().expect("tempdir");
    let delivery = ModelDelivery::new(router(), true, Some(base));
    assert!(delivery.downloads());

    // No setting, or one that names no offered model: the default.
    delivery.ensure(want(&offers, Some("gone"), home.path()));
    let status = settled(&delivery, |state| *state == ModelState::Installed).await;
    assert_eq!(status.chosen, "tiny");
    assert_eq!(status.memory_ceiling_bytes, 512 << 20);
    assert_eq!(
        status
            .models
            .iter()
            .map(|model| (model.model_id.as_str(), &model.state))
            .collect::<Vec<_>>(),
        [
            ("other", &ModelState::NotInstalled),
            ("tiny", &ModelState::Installed)
        ]
    );
    let models = home.path().join(MODELS_DIR);
    assert!(models.join("tiny/model.json").is_file());

    // The same want again asks the registry for nothing.
    let asked = seen.lock().expect("seen").len();
    delivery.ensure(want(&offers, None, home.path()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(seen.lock().expect("seen").len(), asked);

    // A new choice is installed, and only then is the old model removed.
    delivery.ensure(want(&offers, Some("other"), home.path()));
    let status = settled(&delivery, |state| *state == ModelState::Installed).await;
    assert_eq!(status.chosen, "other");
    assert!(models.join("other/model.json").is_file());
    assert!(!models.join("tiny").exists());

    delivery.forget("pi_1");
    assert!(delivery.status().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_does_not_download_still_finds_a_placed_model() {
    let tiny = Pack::new("tiny");
    let offers = offers(&[("tiny", &tiny)]);
    let home = tempfile::tempdir().expect("tempdir");
    let delivery = ModelDelivery::new(router(), false, None);
    assert!(!delivery.downloads());

    delivery.ensure(want(&offers, None, home.path()));
    settled(&delivery, |state| *state == ModelState::DownloadsOff).await;
    delivery.forget("pi_1");

    let dir = home.path().join(MODELS_DIR).join("tiny");
    std::fs::create_dir_all(&dir).expect("model dir");
    for (name, content) in &tiny.files {
        std::fs::write(dir.join(name), content).expect("model file");
    }
    delivery.ensure(want(&offers, None, home.path()));
    settled(&delivery, |state| *state == ModelState::Installed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_download_waits_and_says_why() {
    let tiny = Pack::new("tiny");
    let offers = offers(&[("tiny", &tiny)]);
    // A registry that serves no blob: the token is handed out, the
    // description is not there.
    let (base, _) = serve(BTreeMap::new(), None);
    let home = tempfile::tempdir().expect("tempdir");
    let delivery = ModelDelivery::new(router(), true, Some(base));

    delivery.ensure(want(&offers, None, home.path()));
    let status = settled(&delivery, |state| {
        matches!(state, ModelState::Waiting { .. })
    })
    .await;
    let ModelState::Waiting { detail } = &status.models[0].state else {
        unreachable!("settled on Waiting");
    };
    assert!(!detail.is_empty() && detail.chars().count() <= 300);
    assert!(!home.path().join(MODELS_DIR).join("tiny").exists());
}

#[test]
fn retries_wait_longer_each_time_up_to_an_hour() {
    assert_eq!(retry_wait(1), Duration::from_secs(60));
    assert_eq!(retry_wait(2), Duration::from_secs(120));
    assert_eq!(retry_wait(5), Duration::from_secs(960));
    assert_eq!(retry_wait(7), Duration::from_secs(3600));
    assert_eq!(retry_wait(u32::MAX), Duration::from_secs(3600));
}

#[test]
fn an_unusable_mirror_falls_back_to_the_packages_registry() {
    let tiny = Pack::new("tiny");
    let offers = offers(&[("tiny", &tiny)]);
    let shared = |mirror: Option<&str>| Shared {
        download: true,
        mirror: mirror.map(str::to_owned),
        ai: router(),
        tracked: Mutex::new(HashMap::new()),
    };
    assert_eq!(shared(None).registry(&offers), offers.registry());
    assert_eq!(
        shared(Some("ftp://mirror.example/x")).registry(&offers),
        offers.registry()
    );
    assert_ne!(
        shared(Some("https://mirror.example")).registry(&offers),
        offers.registry()
    );
}

/// The embedder's own package files, as a plugin tree under `root`.
fn embedder_tree(root: &Path) -> crate::DiscoveredPlugin {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../bookclerk-plugins/platform/ai-embedder");
    for name in ["plugin.toml", "models.json"] {
        std::fs::copy(source.join(name), root.join(name)).expect("package file");
    }
    let text = std::fs::read_to_string(root.join("plugin.toml")).expect("plugin.toml");
    let manifest = bookclerk_plugin_manifest::parse(&text).expect("manifest");
    crate::DiscoveredPlugin::for_test(manifest, root.to_path_buf(), root.join("guest"))
}

/// A spawn of instance `pi_1` with `settings` as its resolved document.
fn spawn(settings: &serde_json::Value) -> crate::DeploymentSpawn {
    crate::DeploymentSpawn {
        plugin_instance_id: "pi_1".into(),
        plugin_key: "embedder".into(),
        plugin_root: None,
        config: bookclerk_plugin_abi::ExtensibleConfig::json(settings),
        secrets: bookclerk_plugin_abi::ExtensibleConfig::json(&serde_json::json!({})),
        config_revision: 1,
        installed: None,
    }
}

#[test]
fn only_a_platform_plugin_has_its_models_installed() {
    let files = tempfile::tempdir().expect("tempdir");
    let root = tempfile::tempdir().expect("tempdir");
    let config = bookclerk_config::Config {
        paths: Some(bookclerk_config::Paths::from_files_dir(
            files.path().to_path_buf(),
        )),
        ..Default::default()
    };
    let mut plugin = embedder_tree(root.path());
    let request = spawn(&serde_json::json!({ "model": "harrier-270m-q4" }));

    // A development tree is not a platform plugin, whatever it is named.
    assert!(model_want(&plugin, &config, &request).is_none());

    // The same tree as the verified platform `embedder`.
    let platform = bookclerk_plugin_catalog::PLATFORM_ARTIFACTS
        .iter()
        .find(|artifact| artifact.manifest_id == "embedder")
        .expect("the embedder is a platform plugin")
        .plugin_key();
    plugin.identity.plugin_key = platform.clone();
    plugin.identity.artifact.plugin_key = platform;
    assert!(
        model_want(&plugin, &config, &request).is_none(),
        "the platform key alone is not enough"
    );
    plugin.identity.provenance = bookclerk_plugin_catalog::PluginProvenance::PlatformBundled;
    let want = model_want(&plugin, &config, &request).expect("a platform plugin's want");
    assert_eq!(want.plugin_instance_id, "pi_1");
    assert_eq!(want.plugin_id, "embedder");
    assert_eq!(want.serves, [AiTask::TextEmbeddings]);
    assert_eq!(want.chosen.as_deref(), Some("harrier-270m-q4"));
    assert_eq!(want.model_id(), "harrier-270m-q4");
    assert_eq!(want.offers.default_model(), "bekko-a8m");
    assert!(want.data_dir.starts_with(files.path()));
    // The default jail ceiling, which both published models fit.
    assert_eq!(want.memory_ceiling_bytes, 512 << 20);
    for (_, offer) in want.offers.models() {
        assert!(offer.memory_bytes <= want.memory_ceiling_bytes);
    }

    // No setting: the package's default. No list: nothing to install.
    let unset = model_want(&plugin, &config, &spawn(&serde_json::json!({}))).expect("want");
    assert_eq!(
        (unset.chosen.as_deref(), unset.model_id()),
        (None, "bekko-a8m")
    );
    std::fs::write(root.path().join("models.json"), b"[]").expect("write");
    assert!(model_want(&plugin, &config, &request).is_none());
    std::fs::remove_file(root.path().join("models.json")).expect("remove");
    assert!(model_want(&plugin, &config, &request).is_none());
}
