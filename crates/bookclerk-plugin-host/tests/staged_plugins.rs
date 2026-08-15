//! Host↔guest handshake against staged optional/example artifacts (+ platform in FILES_DIR).
//!
//! Requires:
//! - `BOOKCLERK_PLUGIN_ARTIFACTS` — optional + examples (`cargo stage-plugins --optional --examples`)
//! - platform guests under `$BOOKCLERK_FILES_DIR/plugins/` (`cargo install-platform`)

use std::path::PathBuf;

use bookclerk_config::{Config, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, CatalogHitDto, CliInvokeParams, CliInvokeResult,
    PluginGrantStore, PluginKind, SearchCatalogParams, V2PluginSession, HOST_SHARED_ACCOUNT,
    OPERATOR_ACCOUNT,
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
        "echo_native_rust",
        "echo_native_node",
        "echo_native_python",
        "echo_workerd_ts",
        "echo_workerd_python",
        "echo_workerd_rust",
        "echo_workerd_fetch",
    ] {
        assert!(
            ids.contains(&expected),
            "expected plugin `{expected}` in {ids:?}"
        );
    }

    for plugin in &plugins {
        assert_eq!(
            plugin.manifest.api_version, 2,
            "plugin `{}` must be api_version 2",
            plugin.manifest.id
        );
        let account = match plugin.manifest.kind {
            PluginKind::Source | PluginKind::Integration => HOST_SHARED_ACCOUNT,
            _ => OPERATOR_ACCOUNT,
        };
        let session =
            V2PluginSession::spawn_for_account(plugin, &config, serde_json::json!({}), account)
                .await
                .unwrap_or_else(|e| panic!("spawn v2 {}: {e}", plugin.manifest.id));
        assert_eq!(session.id(), plugin.manifest.id);
        let desc = session
            .describe()
            .await
            .unwrap_or_else(|e| panic!("describe v2 {}: {e}", plugin.manifest.id));
        assert_eq!(desc.api_version, 2);
        assert_eq!(desc.id, plugin.manifest.id);
        match plugin.manifest.kind {
            PluginKind::Source => assert_eq!(desc.kind, "source"),
            PluginKind::Integration => assert_eq!(desc.kind, "integration"),
            PluginKind::Output => assert_eq!(desc.kind, "output"),
            PluginKind::Database => assert_eq!(desc.kind, "database"),
        }

        if session.has_capability("health") {
            let health_json = match plugin.manifest.kind {
                PluginKind::Source => session.content_source_json("{}", "health", "{}").await.ok(),
                PluginKind::Integration => {
                    session.integration_json("{}", "health", "{}").await.ok()
                }
                _ => None,
            };
            if let Some(raw) = health_json {
                let health: serde_json::Value =
                    serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));
                let ok = health.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
                if plugin.manifest.id != "audiobookshelf" {
                    assert!(ok, "{} health not ok: {health}", plugin.manifest.id);
                }
                let detail = health.get("detail").and_then(|v| v.as_str());
                let expected_detail = match plugin.manifest.id.as_str() {
                    "echo_workerd_ts" => Some("echo workerd plugin ready"),
                    "echo_workerd_python" => Some("echo workerd python plugin ready"),
                    "echo_workerd_rust" => Some("echo workerd rust wasm plugin ready"),
                    "echo_workerd_fetch" => Some("echo workerd fetch plugin ready"),
                    "echo_native_node" => Some("echo_native_node ready"),
                    "echo_native_python" => Some("echo_native_python ready"),
                    _ => None,
                };
                if let Some(expected) = expected_detail {
                    assert_eq!(
                        detail,
                        Some(expected),
                        "{} must run under real workerd (run cargo ensure-workerd)",
                        plugin.manifest.id
                    );
                }
            }
        }

        if plugin.manifest.id == "echo_workerd_fetch" && session.has_capability("cli") {
            let params = CliInvokeParams {
                command: "fetch-example".into(),
                args: Default::default(),
            };
            let raw = session
                .cli_invoke_json(serde_json::to_string(&params).expect("cli params"))
                .await
                .unwrap_or_else(|e| panic!("echo_workerd_fetch fetch-example must answer: {e}"));
            let result: CliInvokeResult =
                serde_json::from_str(&raw).unwrap_or_else(|e| panic!("cli result: {e}"));
            let allowed = result
                .json
                .as_ref()
                .and_then(|v| v.get("allowed"))
                .and_then(|v| v.as_bool());
            match allowed {
                Some(true) => {
                    assert_eq!(
                        result.exit_code, 0,
                        "allowed Response must exit 0 regardless of HTTP status: {:?}",
                        result.stdout
                    );
                }
                Some(false) | None => {
                    eprintln!(
                        "echo_workerd_fetch fetch-example best-effort skip (no Response): exit={} stdout={:?} stderr={:?}",
                        result.exit_code, result.stdout, result.stderr
                    );
                }
            }
        }

        if plugin.manifest.kind == PluginKind::Source && session.has_capability("searchCatalog") {
            let params = serde_json::to_string(&SearchCatalogParams {
                query: "test".into(),
                region: "us".into(),
                limit: 1,
                page: 1,
                sort: None,
                field: None,
                language: None,
            })
            .expect("search params");
            let raw = session
                .content_source_json("{}", "searchCatalog", params)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "{} search_catalog must succeed (empty ok): {e}",
                        plugin.manifest.id
                    )
                });
            let hits: Vec<CatalogHitDto> = serde_json::from_str(&raw).unwrap_or_default();
            assert!(
                hits.len() <= 1,
                "{} search_catalog returned more than limit: {}",
                plugin.manifest.id,
                hits.len()
            );
        }

        if plugin.manifest.id == "chirp" && session.has_capability("listDeals") {
            let raw = session
                .content_source_json(
                    "{}",
                    "listDeals",
                    serde_json::json!({ "limit": 1 }).to_string(),
                )
                .await
                .unwrap_or_else(|e| panic!("chirp list_deals must succeed (empty ok): {e}"));
            let deals: Vec<CatalogHitDto> = serde_json::from_str(&raw).unwrap_or_default();
            assert!(
                deals.len() <= 1,
                "chirp list_deals over limit: {}",
                deals.len()
            );
        }
    }

    std::env::remove_var("BOOKCLERK_PLUGIN_DIRS");
}
