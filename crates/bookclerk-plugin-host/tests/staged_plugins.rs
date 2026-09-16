//! Host↔guest describe conformance against staged optional/example artifacts (+ platform in FILES_DIR).
//!
//! Requires:
//! - `BOOKCLERK_PLUGIN_ARTIFACTS` — optional + examples (`cargo stage-plugins --optional --examples`)
//! - platform guests under `$BOOKCLERK_FILES_DIR/plugins/` (`cargo install-platform`)

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bookclerk_config::{Config, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, CliInvokeParams, Entrypoint, PluginFamily, PluginGrantStore,
    PluginSession, SearchCatalogParams, HOST_SHARED_ACCOUNT, OPERATOR_ACCOUNT,
};
use bookclerk_plugin_sdk::{CatalogField, CatalogSort, ListDealsParams};

/// Per-plugin spawn/describe budget. A hung Cap'n Proto handshake must fail
/// the job rather than stall `fmt / clippy / test` until the runner timeout.
const STAGED_SPAWN_TIMEOUT: Duration = Duration::from_secs(45);
const STAGED_RPC_TIMEOUT: Duration = Duration::from_secs(30);

fn artifacts_dir() -> Option<PathBuf> {
    std::env::var_os("BOOKCLERK_PLUGIN_ARTIFACTS").map(PathBuf::from)
}

#[tokio::test]
async fn staged_first_party_plugins_describe() {
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
        files.join("install-ledger.json").is_file(),
        "platform install ledger missing under {} (run cargo install-platform)",
        files.display()
    );

    // Optional/examples from artifacts; platform from FILES_DIR/plugins.
    std::env::set_var("BOOKCLERK_PLUGIN_DIRS", artifacts.as_os_str());
    let config = Config {
        paths: Some(Paths::from_files_dir(files)),
        ..Default::default()
    };

    let discovered_at = Instant::now();
    eprintln!("staged_plugins: discovering under FILES_DIR + PLUGIN_DIRS");
    let plugins = discover_plugins(&config).expect("discover");
    eprintln!(
        "staged_plugins: discovered {} plugins in {:?}",
        plugins.len(),
        discovered_at.elapsed()
    );
    // External spawn requires covering grants (platform sqlite/local auto-grant;
    // optional/examples need an explicit approve snapshot for this smoke test).
    let mut grants = PluginGrantStore::load(&config.paths().files_dir).expect("load grants");
    for plugin in &plugins {
        grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
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
            plugin.manifest.api_version, 3,
            "plugin `{}` must be api_version 3",
            plugin.manifest.id
        );
        let families = plugin.manifest.families();
        let account = if families.contains(&PluginFamily::Source)
            || families.contains(&PluginFamily::Integration)
        {
            HOST_SHARED_ACCOUNT
        } else {
            OPERATOR_ACCOUNT
        };
        eprintln!(
            "staged_plugins: spawn {} ({})",
            plugin.manifest.id,
            plugin.plugin_key().canonical()
        );
        let spawned_at = Instant::now();
        let session = tokio::time::timeout(
            STAGED_SPAWN_TIMEOUT,
            PluginSession::spawn_for_account(plugin, &config, serde_json::json!({}), account),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "spawn {} timed out after {STAGED_SPAWN_TIMEOUT:?}",
                plugin.manifest.id
            )
        })
        .unwrap_or_else(|e| panic!("spawn {}: {e}", plugin.manifest.id));
        eprintln!(
            "staged_plugins: spawned {} in {:?}",
            plugin.manifest.id,
            spawned_at.elapsed()
        );
        assert_eq!(session.id(), plugin.plugin_key().canonical());
        assert_eq!(session.alias(), plugin.manifest.id);
        let desc = tokio::time::timeout(STAGED_RPC_TIMEOUT, session.describe())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "describe {} timed out after {STAGED_RPC_TIMEOUT:?}",
                    plugin.manifest.id
                )
            })
            .unwrap_or_else(|e| panic!("describe {}: {e}", plugin.manifest.id));
        assert_eq!(desc.api_version, 3);
        assert_eq!(desc.id, plugin.manifest.id);
        assert_eq!(
            desc.capabilities,
            plugin.manifest.capabilities(),
            "{} describe() capabilities must equal plugin.toml",
            plugin.manifest.id
        );

        {
            let health = if session.has_entrypoint(Entrypoint::Storefront) {
                tokio::time::timeout(
                    STAGED_RPC_TIMEOUT,
                    session.storefront(|stub| async move { stub.health().await }),
                )
                .await
                .ok()
                .and_then(Result::ok)
            } else if session.has_entrypoint(Entrypoint::RemoteLibrary) {
                tokio::time::timeout(
                    STAGED_RPC_TIMEOUT,
                    session.remote_library(|stub| async move { stub.health().await }),
                )
                .await
                .ok()
                .and_then(Result::ok)
            } else {
                None
            };
            if let Some(health) = health {
                if plugin.manifest.id != "audiobookshelf" {
                    assert!(
                        health.ok,
                        "{} health not ok: {health:?}",
                        plugin.manifest.id
                    );
                }
                let detail = Some(health.detail.as_str()).filter(|d| !d.is_empty());
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

        if plugin.manifest.id == "echo_workerd_fetch" && session.has_entrypoint(Entrypoint::Cli) {
            let params = CliInvokeParams {
                command: "fetch-example".into(),
                args: Default::default(),
            };
            let result = tokio::time::timeout(STAGED_RPC_TIMEOUT, session.cli_invoke(params))
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "echo_workerd_fetch fetch-example timed out after {STAGED_RPC_TIMEOUT:?}"
                    )
                })
                .unwrap_or_else(|e| panic!("echo_workerd_fetch fetch-example must answer: {e}"));
            let allowed = result
                .payload
                .json_value()
                .ok()
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

        if session.has_entrypoint(Entrypoint::Storefront) {
            let params = SearchCatalogParams {
                query: "test".into(),
                region: "us".into(),
                limit: 1,
                page: 1,
                sort: CatalogSort::Relevance,
                field: CatalogField::Any,
                language: None,
            };
            let hits = match tokio::time::timeout(
                STAGED_RPC_TIMEOUT,
                session.storefront(move |stub| async move { stub.search_catalog(params).await }),
            )
            .await
            {
                Err(_) => {
                    eprintln!(
                        "{} search_catalog skipped (timed out after {STAGED_RPC_TIMEOUT:?})",
                        plugin.manifest.id
                    );
                    continue;
                }
                Ok(Ok(hits)) => hits,
                Ok(Err(e)) if live_storefront_unavailable(&e) => {
                    eprintln!(
                        "{} search_catalog skipped (live storefront unavailable): {e}",
                        plugin.manifest.id
                    );
                    continue;
                }
                Ok(Err(e)) => panic!(
                    "{} search_catalog must succeed (empty ok): {e}",
                    plugin.manifest.id
                ),
            };
            assert!(
                hits.len() <= 1,
                "{} search_catalog returned more than limit: {}",
                plugin.manifest.id,
                hits.len()
            );
        }

        if plugin.manifest.id == "chirp" {
            let deals = match tokio::time::timeout(
                STAGED_RPC_TIMEOUT,
                session.storefront(|stub| async move {
                    stub.list_deals(ListDealsParams { limit: Some(1) }).await
                }),
            )
            .await
            {
                Err(_) => {
                    eprintln!("chirp list_deals skipped (timed out after {STAGED_RPC_TIMEOUT:?})");
                    continue;
                }
                Ok(Ok(deals)) => deals,
                Ok(Err(e)) if live_storefront_unavailable(&e) => {
                    eprintln!("chirp list_deals skipped (live storefront unavailable): {e}");
                    continue;
                }
                Ok(Err(e)) => panic!("chirp list_deals must succeed (empty ok): {e}"),
            };
            assert!(
                deals.len() <= 1,
                "chirp list_deals over limit: {}",
                deals.len()
            );
        }
    }

    std::env::remove_var("BOOKCLERK_PLUGIN_DIRS");
}

/// True when a staged-plugin smoke call failed because a live storefront was down.
fn live_storefront_unavailable(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    http_error_status(&msg)
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("connection refused")
        || msg.contains("dns error")
        || msg.contains("error sending request")
}

/// True when `msg` (already lowercased) reports an HTTP 4xx/5xx from reqwest or SDK HTTP.
fn http_error_status(msg: &str) -> bool {
    if msg.contains("http status") {
        return true;
    }
    // SDK HTTP: `HTTP 500 Internal Server Error for {url}`
    let Some(after) = msg.split("http ").nth(1) else {
        return false;
    };
    after
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<u16>()
        .is_ok_and(|code| (400..600).contains(&code))
}

#[test]
fn live_storefront_unavailable_matches_sdk_http_status() {
    assert!(live_storefront_unavailable(
        &"internal: enrichment error: HTTP 500 Internal Server Error for https://api.audible.com/1.0/catalog/search"
    ));
    assert!(live_storefront_unavailable(&"http status 503"));
    assert!(live_storefront_unavailable(&"GraphQL HTTP 403: forbidden"));
    assert!(!live_storefront_unavailable(
        &"audible search_catalog must succeed (empty ok)"
    ));
}
