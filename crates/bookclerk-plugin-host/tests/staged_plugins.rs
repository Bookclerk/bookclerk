//! Host↔guest handshake against staged optional/example artifacts (+ platform in FILES_DIR).
//!
//! Requires:
//! - `BOOKCLERK_PLUGIN_ARTIFACTS` — optional + examples (`cargo stage-plugins --optional --examples`)
//! - platform guests under `$BOOKCLERK_FILES_DIR/plugins/` (`cargo install-platform`)

use std::path::PathBuf;

use bookclerk_config::{Config, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, methods, CatalogHitDto, HealthDto, PluginClient,
    PluginGrantStore, PluginKind, SearchCatalogParams,
};

fn artifacts_dir() -> Option<PathBuf> {
    std::env::var_os("BOOKCLERK_PLUGIN_ARTIFACTS").map(PathBuf::from)
}

#[tokio::test]
async fn staged_first_party_plugins_handshake() {
    let Some(artifacts) = artifacts_dir() else {
        eprintln!(
            "skipping: set BOOKCLERK_PLUGIN_ARTIFACTS after `cargo stage-plugins --optional --examples`"
        );
        return;
    };
    assert!(
        artifacts.is_dir(),
        "BOOKCLERK_PLUGIN_ARTIFACTS is not a directory: {}",
        artifacts.display()
    );

    let files = match std::env::var_os("BOOKCLERK_FILES_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            eprintln!("skipping: set BOOKCLERK_FILES_DIR after `cargo install-platform`");
            return;
        }
    };
    assert!(
        files.join("plugins").join("sqlite").is_dir(),
        "platform sqlite missing under {}/plugins (run cargo install-platform)",
        files.display()
    );

    // Optional/examples from artifacts; platform from FILES_DIR/plugins.
    std::env::set_var("BOOKCLERK_PLUGIN_DIRS", artifacts.as_os_str());
    let config = Config {
        paths: Some(Paths::from_files_dir(files)),
        ..Default::default()
    };

    let plugins = discover_plugins(&config).expect("discover");
    // External spawn requires covering grants (platform sqlite/local auto-grant;
    // optional/examples need an explicit approve snapshot for this smoke test).
    let mut grants = PluginGrantStore::load(&config.paths().files_dir).expect("load grants");
    for plugin in &plugins {
        grants.upsert(consent_request(&plugin.manifest));
    }
    grants.save(&config.paths().files_dir).expect("save grants");

    let ids: Vec<_> = plugins.iter().map(|p| p.manifest.id.as_str()).collect();
    for expected in [
        // platform (FILES_DIR)
        "local",
        "sqlite",
        // optional (artifacts)
        "audible",
        "libro",
        "chirp",
        "graphicaudio",
        "audiobookshelf",
        "s3",
        "d1",
        "postgres",
        // examples
        "echo-native-rust",
        "echo-native-node",
        "echo-native-python",
        "echo-workerd-ts",
        "echo-workerd-python",
        "echo-workerd-rust",
    ] {
        assert!(
            ids.contains(&expected),
            "expected plugin `{expected}` in {ids:?}"
        );
    }

    for plugin in &plugins {
        let client = PluginClient::spawn(plugin, &config, serde_json::json!({}))
            .await
            .unwrap_or_else(|e| panic!("spawn {}: {e}", plugin.manifest.id));
        let hs = client.handshake();
        assert_eq!(hs.id, plugin.manifest.id);
        assert_eq!(hs.api_version, bookclerk_plugin_host::PLUGIN_API_VERSION);
        match plugin.manifest.kind {
            PluginKind::Source => assert_eq!(hs.kind, "source"),
            PluginKind::Integration => assert_eq!(hs.kind, "integration"),
            PluginKind::Output => assert_eq!(hs.kind, "output"),
            PluginKind::Database => assert_eq!(hs.kind, "database"),
        }
        let health: HealthDto = client
            .call(methods::HEALTH, serde_json::json!({}))
            .await
            .expect("health");
        // ABS (and similar) report ok=false until base_url/api_key are configured;
        // the handshake smoke test only requires the method to answer.
        if plugin.manifest.id != "audiobookshelf" {
            assert!(
                health.ok,
                "{} health not ok: {:?}",
                plugin.manifest.id, health
            );
        }
        // Real workerd isolate (not a JS-less shim): Echo guests must return module detail.
        let expected_detail = match plugin.manifest.id.as_str() {
            "echo-workerd-ts" => Some("echo workerd plugin ready"),
            "echo-workerd-python" => Some("echo workerd python plugin ready"),
            "echo-workerd-rust" => Some("echo workerd rust wasm plugin ready"),
            "echo-native-node" => Some("echo-native-node ready"),
            "echo-native-python" => Some("echo-native-python ready"),
            _ => None,
        };
        if let Some(detail) = expected_detail {
            assert_eq!(
                health.detail.as_deref(),
                Some(detail),
                "{} must run under real workerd (run cargo ensure-workerd)",
                plugin.manifest.id
            );
        }

        // Catalog RPC smoke: must return Ok (empty vec is fine) without crashing.
        if plugin.manifest.kind == PluginKind::Source
            && client.has_capability(methods::SEARCH_CATALOG)
        {
            let hits: Vec<CatalogHitDto> = client
                .call(
                    methods::SEARCH_CATALOG,
                    serde_json::to_value(SearchCatalogParams {
                        query: "test".into(),
                        region: "us".into(),
                        limit: 1,
                        page: 1,
                        sort: None,
                        field: None,
                        language: None,
                    })
                    .expect("search params"),
                )
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "{} search_catalog must succeed (empty ok): {e}",
                        plugin.manifest.id
                    )
                });
            assert!(
                hits.len() <= 1,
                "{} search_catalog returned more than limit: {}",
                plugin.manifest.id,
                hits.len()
            );
        }

        // Chirp deals do not need credentials; skip fetch_title (needs auth).
        if plugin.manifest.id == "chirp" && client.has_capability(methods::LIST_DEALS) {
            let deals: Vec<CatalogHitDto> = client
                .call(methods::LIST_DEALS, serde_json::json!({ "limit": 1 }))
                .await
                .unwrap_or_else(|e| panic!("chirp list_deals must succeed (empty ok): {e}"));
            assert!(
                deals.len() <= 1,
                "chirp list_deals over limit: {}",
                deals.len()
            );
        }
    }

    std::env::remove_var("BOOKCLERK_PLUGIN_DIRS");
}
