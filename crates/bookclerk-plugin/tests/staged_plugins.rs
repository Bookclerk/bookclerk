//! Host↔guest handshake against staged first-party plugin artifacts.
//!
//! Requires binaries under `BOOKCLERK_PLUGIN_ARTIFACTS` (CI stages them via
//! `scripts/stage-first-party-plugins.sh`). When unset, the test is skipped.

use std::path::PathBuf;

use bookclerk_config::{Config, Paths};
use bookclerk_plugin::{
    discover_plugins, methods, CatalogHitDto, HealthDto, PluginClient, PluginKind,
    SearchCatalogParams,
};

fn artifacts_dir() -> Option<PathBuf> {
    std::env::var_os("BOOKCLERK_PLUGIN_ARTIFACTS").map(PathBuf::from)
}

#[tokio::test]
async fn staged_first_party_plugins_handshake() {
    let Some(artifacts) = artifacts_dir() else {
        eprintln!("skipping: set BOOKCLERK_PLUGIN_ARTIFACTS after stage-first-party-plugins.sh");
        return;
    };
    assert!(
        artifacts.is_dir(),
        "BOOKCLERK_PLUGIN_ARTIFACTS is not a directory: {}",
        artifacts.display()
    );

    let files = tempfile::tempdir().expect("temp files dir");
    // Point discovery at the staged tree via BOOKCLERK_PLUGIN_DIRS.
    std::env::set_var("BOOKCLERK_PLUGIN_DIRS", artifacts.as_os_str());
    let config = Config {
        paths: Some(Paths::from_files_dir(files.path().to_path_buf())),
        ..Default::default()
    };

    let plugins = discover_plugins(&config).expect("discover");
    let ids: Vec<_> = plugins.iter().map(|p| p.manifest.id.as_str()).collect();
    for expected in [
        "echo",
        "audible",
        "libro",
        "chirp",
        "graphicaudio",
        "audiobookshelf",
    ] {
        assert!(
            ids.contains(&expected),
            "expected plugin `{expected}` in {ids:?}"
        );
    }

    for plugin in &plugins {
        let client = PluginClient::spawn(
            &plugin.manifest.id,
            &plugin.command,
            &plugin.manifest.args,
            &plugin.root,
            serde_json::json!({}),
        )
        .await
        .unwrap_or_else(|e| panic!("spawn {}: {e}", plugin.manifest.id));
        let hs = client.handshake();
        assert_eq!(hs.id, plugin.manifest.id);
        assert_eq!(hs.api_version, bookclerk_plugin::PLUGIN_API_VERSION);
        match plugin.manifest.kind {
            PluginKind::Source => assert_eq!(hs.kind, "source"),
            PluginKind::Integration => assert_eq!(hs.kind, "integration"),
            other => panic!("unexpected kind {other:?}"),
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
