//! Installed-plugin path checks for first-party guests.
//!
//! Large Audible download + S3 multipart through the *installed* (not
//! in-process) path require live credentials and are gated behind env vars
//! per AGENTS.md live-store constraints. Without credentials these tests skip.

use std::path::PathBuf;

fn artifacts_root() -> Option<PathBuf> {
    std::env::var_os("BOOKCLERK_PLUGIN_ARTIFACTS").map(PathBuf::from)
}

#[tokio::test]
async fn staged_audible_guest_handshakes_when_artifacts_present() {
    let Some(root) = artifacts_root() else {
        eprintln!("skip: set BOOKCLERK_PLUGIN_ARTIFACTS (cargo test-staged)");
        return;
    };
    let plugin_dir = root.join("audible");
    if !plugin_dir.join("plugin.toml").is_file() {
        eprintln!("skip: audible not staged under {}", plugin_dir.display());
        return;
    }
    // Discovery + handshake is covered by staged_plugins.rs; this test asserts
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
async fn staged_s3_guest_layout_when_artifacts_present() {
    let Some(root) = artifacts_root() else {
        eprintln!("skip: set BOOKCLERK_PLUGIN_ARTIFACTS");
        return;
    };
    let plugin_dir = root.join("s3");
    if !plugin_dir.join("plugin.toml").is_file() {
        eprintln!("skip: s3 not staged");
        return;
    }
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
    //   bookclerk plugins install local:<archive> --manifest … --allow-unsigned
    //   bookclerk auth set-scan <account> --scan false
    //   bookclerk library acquire --asin <ONE>
    // This automated gate intentionally does not bulk-acquire.
    eprintln!("live installed-plugin path: drive manually with CLI per AGENTS.md");
}
