//! `bookclerk plugins install|approve|versions|rollback|remove` install and
//! move cluster versions without touching this host's plugin tree.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::{Path, PathBuf};

use common::{run_bookclerk, stage_sqlite_plugin, stderr, stdout, write_config};

/// Writes a native fixture plugin archive for `version` and its package JSON.
fn fixture_package(dir: &Path, version: &str) -> (PathBuf, PathBuf) {
    let stage = dir.join(format!("stage-{version}"));
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
        stage.join("plugin.toml"),
        format!(
            "api_version = 3\nid = \"fxcli\"\nversion = \"{version}\"\nruntime = \"native\"\n\
             command = \"./guest\"\nentrypoints = [\"cli\"]\n\n[capabilities.network]\n\
             mode = \"deny\"\n\n[vars]\n\n[[settings]]\nkey = \"mode\"\ntype = \"string\"\n\
             default = \"plain\"\n"
        ),
    )
    .unwrap();
    std::fs::write(stage.join("guest"), format!("#!/bin/sh\necho {version}\n")).unwrap();
    let archive = dir.join(format!("fxcli-{version}.tar.gz"));
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .args(["plugin.toml", "guest"])
        .status()
        .expect("tar");
    assert!(status.success());
    let package = dir.join(format!("fxcli-{version}.json"));
    std::fs::write(
        &package,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "api_version": 3,
            "kind": "integration",
            "id": "fxcli",
            "artifacts": [{
                "target": bookclerk_plugin_catalog::host_bookclerk_target(),
                "url": format!("file://{}", archive.display()),
                "archive_sha256": "ab".repeat(32),
                "executable": "guest"
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    (archive, package)
}

fn json(output: &std::process::Output) -> serde_json::Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_str(&stdout(output)).unwrap_or_else(|err| {
        panic!("json output ({err}): {}", stdout(output));
    })
}

#[test]
fn install_rollback_and_uninstall_move_the_active_version() {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    let work = tempfile::tempdir().unwrap();
    let (archive_1, package_1) = fixture_package(work.path(), "0.1.0");
    let (archive_2, package_2) = fixture_package(work.path(), "0.2.0");
    let install = |archive: &Path, package: &Path, extra: &[&str]| {
        let mut args = vec![
            "--format",
            "json",
            "plugins",
            "install",
            "fxcli",
            "--archive",
            archive.to_str().unwrap(),
            "--manifest",
            package.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        run_bookclerk(files, &args, &[])
    };

    let refused = install(&archive_1, &package_1, &[]);
    assert!(
        !refused.status.success(),
        "unverified publishers need the flag"
    );

    let first = json(&install(
        &archive_1,
        &package_1,
        &["--allow-unverified-publisher"],
    ));
    assert_eq!(first["version"], "0.1.0");
    assert_eq!(first["active"], true, "the first version becomes active");
    assert_eq!(first["approved"], false, "consent is pending");
    assert!(
        first["artifacts"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with("blob:sha256:"),
        "a small archive is stored in the library"
    );

    let approve = run_bookclerk(files, &["plugins", "approve", "fxcli", "--yes"], &[]);
    assert!(approve.status.success(), "{}", stderr(&approve));

    // A second version installs without moving the pointer, then
    // `--activate` moves it and carries the approval forward.
    let parked = json(&install(
        &archive_2,
        &package_2,
        &["--allow-unverified-publisher"],
    ));
    assert_eq!(parked["active"], false);
    let second = json(&install(
        &archive_2,
        &package_2,
        &["--allow-unverified-publisher", "--activate"],
    ));
    assert_eq!(
        second["created"], false,
        "same content is not installed twice"
    );
    assert_eq!(second["active"], true);
    assert_eq!(second["approved"], true, "unchanged consent carries over");

    let versions = json(&run_bookclerk(
        files,
        &["--format", "json", "plugins", "versions", "fxcli"],
        &[],
    ));
    let rows = versions.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let active: Vec<_> = rows.iter().filter(|r| r["active"] == true).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["version"], "0.2.0");

    let rollback = json(&run_bookclerk(
        files,
        &["--format", "json", "plugins", "rollback", "fxcli", "0.1.0"],
        &[],
    ));
    assert_eq!(rollback["version"], "0.1.0");

    let deploy = run_bookclerk(files, &["plugins", "deploy", "fxcli"], &[]);
    assert!(deploy.status.success(), "{}", stderr(&deploy));

    let uninstalled = json(&run_bookclerk(
        files,
        &["--format", "json", "plugins", "uninstall", "fxcli"],
        &[],
    ));
    assert_eq!(uninstalled["deployments_undeployed"], 1);
    let after = json(&run_bookclerk(
        files,
        &["--format", "json", "plugins", "versions", "fxcli"],
        &[],
    ));
    assert!(after
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["active"] == false));

    // Publishing never wrote a host-local install.
    for entry in std::fs::read_dir(files.join("plugins")).unwrap() {
        let path = entry.unwrap().path();
        if let Ok(text) = std::fs::read_to_string(path.join("plugin.toml")) {
            assert!(
                !text.contains("fxcli"),
                "no host-local tree at {}",
                path.display()
            );
        }
    }
    let ledger = std::fs::read_to_string(files.join("install-ledger.json")).unwrap_or_default();
    assert!(!ledger.contains("fxcli"));
}
