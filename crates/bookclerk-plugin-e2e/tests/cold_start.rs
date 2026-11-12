//! Measures how long each staged first-party guest takes to start, for the
//! on-demand activation decision (`docs/adr/event-delivery-model.md`).
//!
//! Ignored by default: it only measures. Run it against a staged
//! installation (the same prerequisites as `tests/staged_plugins.rs`):
//!
//! ```text
//! cargo install-platform --release && cargo stage-plugins --optional --examples --release
//! BOOKCLERK_FILES_DIR=… BOOKCLERK_PLUGIN_ARTIFACTS=… \
//!   cargo test --release -p bookclerk-plugin-e2e --test cold_start -- --ignored --nocapture
//! ```
//!
//! | Variable | Effect |
//! | --- | --- |
//! | `BOOKCLERK_COLD_START_RUNS` | Spawns per guest (default 5; the first is the coldest) |
//! | `BOOKCLERK_COLD_START_OUT` | Directory for `cold-start.md` and `cold-start.json` (default: print only) |
//! | `BOOKCLERK_COLD_START_CONTEXT` | One line describing the host (CPU limit, machine) for the report |
//! | `BOOKCLERK_STAGED_PLUGINS` | Measure only these ids (plus platform guests) |
//!
//! Each run times the production path: `PluginSession::spawn_for_account`
//! (front door, jail, runtime boot, handshake, and the guest's first
//! `describe`, which the spawn waits for), then one warm `describe` and the
//! first credential-free entrypoint call (`health`). Dropping the session
//! only signals shutdown, so stop time is not measured.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bookclerk_config::{Config, Paths};
use bookclerk_plugin_e2e::startup::{
    json_report, markdown_report, summarize, PluginStartup, StartupSample,
};
use bookclerk_plugin_e2e::{skip_or_fail, StagedScope};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, Entrypoint, PluginFamily, PluginGrantStore, PluginSession,
    HOST_SHARED_ACCOUNT, OPERATOR_ACCOUNT,
};
use bookclerk_plugin_manifest::{PluginManifest, PluginRuntimeKind};

/// Longest one spawn may take before the run fails.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(45);
/// Longest one RPC may take before the run fails.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// How the guest runs, for grouping: `workerd/<js|python>` or
/// `native/<launcher>`.
fn runtime_label(manifest: &PluginManifest) -> String {
    match manifest.runtime {
        PluginRuntimeKind::Workerd => {
            let python = manifest
                .workerd
                .as_ref()
                .is_some_and(|w| w.main_module.ends_with(".py"));
            if python {
                "workerd/python".into()
            } else {
                "workerd/js".into()
            }
        }
        PluginRuntimeKind::Native => {
            let launcher = manifest
                .command
                .as_ref()
                .and_then(|c| c.file_name())
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if launcher.starts_with("node") {
                "native/node".into()
            } else if launcher.starts_with("python") {
                "native/python".into()
            } else {
                "native/binary".into()
            }
        }
    }
}

/// Lowercase family names for the report.
fn family_labels(families: &[PluginFamily]) -> Vec<String> {
    families
        .iter()
        .map(|f| match f {
            PluginFamily::Source => "source",
            PluginFamily::Integration => "integration",
            PluginFamily::Output => "output",
            PluginFamily::Database => "database",
        })
        .map(str::to_string)
        .collect()
}

/// Settles one timed `health` call into its latency sample.
///
/// # Arguments
///
/// * `id` - Plugin id, for the failure message.
/// * `started` - When the call began.
/// * `outcome` - The call wrapped in [`RPC_TIMEOUT`].
///
/// # Returns
///
/// The latency when the call answered, or `None` when it answered with an
/// error (logged): an error is not a latency sample, and a guest may refuse
/// `health` without credentials, as the staged suite allows.
///
/// # Panics
///
/// When the call timed out, so a hung entrypoint fails the run instead of
/// looking like one without a measurable call.
fn settle_first_call<T, E: std::fmt::Display>(
    id: &str,
    started: Instant,
    outcome: Result<Result<T, E>, tokio::time::error::Elapsed>,
) -> Option<Duration> {
    match outcome {
        Err(_) => panic!("health {id} timed out after {RPC_TIMEOUT:?}"),
        Ok(Err(e)) => {
            eprintln!("cold_start: {id} health failed, first call not sampled: {e}");
            None
        }
        Ok(Ok(_)) => Some(started.elapsed()),
    }
}

/// Times the first credential-free entrypoint call, when the guest has one.
///
/// # Returns
///
/// `None` when the guest has no `health` entrypoint or its call failed (see
/// [`settle_first_call`]).
async fn first_call(id: &str, session: &PluginSession) -> Option<Duration> {
    let started = Instant::now();
    if session.has_entrypoint(Entrypoint::Storefront) {
        let outcome = tokio::time::timeout(
            RPC_TIMEOUT,
            session.storefront(|stub| async move { stub.health().await }),
        )
        .await;
        settle_first_call(id, started, outcome)
    } else if session.has_entrypoint(Entrypoint::RemoteLibrary) {
        let outcome = tokio::time::timeout(
            RPC_TIMEOUT,
            session.remote_library(|stub| async move { stub.health().await }),
        )
        .await;
        settle_first_call(id, started, outcome)
    } else if session.has_entrypoint(Entrypoint::Metadata) {
        let outcome = tokio::time::timeout(
            RPC_TIMEOUT,
            session.metadata(|stub| async move { stub.health().await }),
        )
        .await;
        settle_first_call(id, started, outcome)
    } else if session.has_entrypoint(Entrypoint::Covers) {
        let outcome = tokio::time::timeout(
            RPC_TIMEOUT,
            session.covers(|stub| async move { stub.health().await }),
        )
        .await;
        settle_first_call(id, started, outcome)
    } else {
        None
    }
}

#[tokio::test]
#[ignore = "measurement only; needs a staged installation"]
async fn measure_staged_plugin_cold_start() {
    let scope = StagedScope::from_env().unwrap_or_else(|e| panic!("{e}"));
    let Some(artifacts) = std::env::var_os("BOOKCLERK_PLUGIN_ARTIFACTS").map(PathBuf::from) else {
        skip_or_fail("BOOKCLERK_PLUGIN_ARTIFACTS is unset (run `cargo stage-plugins`)");
        return;
    };
    let Some(files) = std::env::var_os("BOOKCLERK_FILES_DIR").map(PathBuf::from) else {
        skip_or_fail("BOOKCLERK_FILES_DIR is unset (run `cargo install-platform`)");
        return;
    };
    assert!(
        artifacts.is_dir(),
        "BOOKCLERK_PLUGIN_ARTIFACTS is not a directory: {}",
        artifacts.display()
    );
    assert!(
        files.is_dir(),
        "BOOKCLERK_FILES_DIR is not a directory: {}",
        files.display()
    );
    let runs: usize = std::env::var("BOOKCLERK_COLD_START_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(5);
    let context = std::env::var("BOOKCLERK_COLD_START_CONTEXT")
        .unwrap_or_else(|_| format!("{runs} spawns per guest, sequential, on an unlimited host."));

    std::env::set_var("BOOKCLERK_PLUGIN_DIRS", artifacts.as_os_str());
    let config = Config {
        paths: Some(Paths::from_files_dir(files)),
        ..Default::default()
    };
    let plugins = discover_plugins(&config).expect("discover");
    let ids: Vec<_> = plugins.iter().map(|p| p.manifest.id.as_str()).collect();
    for expected in scope.required_ids() {
        assert!(
            ids.contains(&expected.as_str()),
            "expected plugin `{expected}` in {ids:?}"
        );
    }
    let mut grants = PluginGrantStore::load(&config.paths().files_dir).expect("load grants");
    for plugin in &plugins {
        grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    }
    grants.save(&config.paths().files_dir).expect("save grants");

    let mut measured = Vec::new();
    for plugin in plugins.iter().filter(|p| scope.includes(&p.manifest.id)) {
        if plugin.manifest.unimplemented_surface().is_some() {
            continue;
        }
        let families = plugin.manifest.families();
        let account = if families.contains(&PluginFamily::Source)
            || families.contains(&PluginFamily::Integration)
        {
            HOST_SHARED_ACCOUNT
        } else {
            OPERATOR_ACCOUNT
        };
        let mut samples = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = Instant::now();
            let session = tokio::time::timeout(
                SPAWN_TIMEOUT,
                PluginSession::spawn_for_account(plugin, &config, serde_json::json!({}), account),
            )
            .await
            .unwrap_or_else(|_| panic!("spawn {} timed out", plugin.manifest.id))
            .unwrap_or_else(|e| panic!("spawn {}: {e}", plugin.manifest.id));
            let spawn = started.elapsed();
            // The spawn already waited for the guest's first `describe`;
            // this one is a warm round trip.
            let described = Instant::now();
            tokio::time::timeout(RPC_TIMEOUT, session.describe())
                .await
                .unwrap_or_else(|_| panic!("describe {} timed out", plugin.manifest.id))
                .unwrap_or_else(|e| panic!("describe {}: {e}", plugin.manifest.id));
            let warm_describe = described.elapsed();
            let first_call = first_call(&plugin.manifest.id, &session).await;
            drop(session);
            samples.push(StartupSample {
                spawn,
                warm_describe,
                first_call,
            });
        }
        let stats = summarize(&samples).expect("at least one run");
        eprintln!(
            "cold_start: {} median ready {:.0} ms (first {:.0} ms)",
            plugin.manifest.id, stats.median_ready_ms, stats.first_ready_ms
        );
        measured.push(PluginStartup {
            id: plugin.manifest.id.clone(),
            runtime: runtime_label(&plugin.manifest),
            families: family_labels(&families),
            stats,
        });
    }
    measured.sort_by(|a, b| a.id.cmp(&b.id));
    let markdown = markdown_report(&measured, &context);
    println!("{markdown}");
    if let Some(out) = std::env::var_os("BOOKCLERK_COLD_START_OUT").map(PathBuf::from) {
        std::fs::create_dir_all(&out).expect("report dir");
        std::fs::write(out.join("cold-start.md"), &markdown).expect("write markdown");
        std::fs::write(
            out.join("cold-start.json"),
            serde_json::to_vec_pretty(&json_report(&measured, &context)).expect("json"),
        )
        .expect("write json");
    }
}
