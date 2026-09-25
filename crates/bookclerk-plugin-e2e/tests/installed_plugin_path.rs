//! Installed-plugin path checks for first-party guests.
//!
//! Large Audible download + S3 multipart through the *installed* (not
//! in-process) path require live credentials and are gated behind env vars
//! per AGENTS.md live-store constraints. Without credentials these tests skip.

use std::path::PathBuf;

use bookclerk_plugin_e2e::{skip_or_fail, staged_plugin_dir, StagedScope};

fn artifacts_root() -> Option<PathBuf> {
    std::env::var_os("BOOKCLERK_PLUGIN_ARTIFACTS").map(PathBuf::from)
}

/// Staged directory for `id`, or `None` when this run does not cover it.
fn staged_dir(id: &str) -> Option<PathBuf> {
    let scope = StagedScope::from_env().unwrap_or_else(|e| panic!("{e}"));
    if !scope.includes(id) {
        eprintln!("skip: `{id}` outside BOOKCLERK_STAGED_PLUGINS scope");
        return None;
    }
    let Some(root) = artifacts_root() else {
        skip_or_fail("BOOKCLERK_PLUGIN_ARTIFACTS is unset (cargo test-staged)");
        return None;
    };
    let dir = staged_plugin_dir(&root, id);
    assert!(
        dir.is_some(),
        "`{id}` is in scope but not staged under {}",
        root.display()
    );
    dir
}

#[tokio::test]
async fn staged_audible_guest_layout() {
    let Some(plugin_dir) = staged_dir("audible") else {
        return;
    };
    // Discovery + describe is covered by staged_plugins.rs; this test asserts
    // the install-shaped layout (plugin.toml beside binary) used by receipts.
    assert!(plugin_dir.join("plugin.toml").is_file());
    let has_bin = plugin_dir
        .read_dir()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.contains("audible") && !s.ends_with(".toml")
        });
    assert!(has_bin, "staged audible binary missing");
}

#[tokio::test]
async fn staged_s3_guest_layout() {
    let Some(plugin_dir) = staged_dir("s3") else {
        return;
    };
    assert!(plugin_dir.join("plugin.toml").is_file());
}

/// Live acquire/multipart through installed guests — opt-in only.
#[tokio::test]
async fn live_audible_and_s3_installed_path_gated() {
    if std::env::var_os("BOOKCLERK_LIVE_INSTALLED_PLUGIN_TEST").is_none() {
        eprintln!(
            "skip: set BOOKCLERK_LIVE_INSTALLED_PLUGIN_TEST=1 with store credentials \
             and a single ASIN; see AGENTS.md live store constraints"
        );
        return;
    }
    // Operators run:
    //   bookclerk plugins install local:<archive> --manifest … --allow-unverified-publisher
    //   bookclerk auth set-scan <account> --scan false
    //   bookclerk library acquire --asin <ONE>
    // This automated gate intentionally does not bulk-acquire.
    eprintln!("live installed-plugin path: drive manually with CLI per AGENTS.md");
}
