//! `bookclerk hosts label` and `bookclerk plugins deploy` write placement
//! intent; a placed instance refuses hand-written enable/disable until
//! `--manual` hands it back.

mod common;

use std::path::{Path, PathBuf};

use common::{run_bookclerk, stage_sqlite_plugin, stderr, stdout, write_config};

/// Writes a native fixture plugin archive and its package JSON.
fn fixture_package(dir: &Path) -> (PathBuf, PathBuf) {
    let stage = dir.join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
        stage.join("plugin.toml"),
        "api_version = 3\nid = \"fxplace\"\nversion = \"0.1.0\"\nruntime = \"native\"\n\
         command = \"./guest\"\nentrypoints = [\"cli\"]\n\n[capabilities.network]\n\
         mode = \"deny\"\n\n[vars]\n",
    )
    .unwrap();
    std::fs::write(stage.join("guest"), "#!/bin/sh\necho ok\n").unwrap();
    let archive = dir.join("fxplace.tar.gz");
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .args(["plugin.toml", "guest"])
        .status()
        .expect("tar");
    assert!(status.success());
    let package = dir.join("fxplace.json");
    std::fs::write(
        &package,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "api_version": 3,
            "kind": "integration",
            "id": "fxplace",
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
fn deploy_places_an_instance_and_manual_hands_it_back() {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    let work = tempfile::tempdir().unwrap();
    let (archive, package) = fixture_package(work.path());
    json(&run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "plugins",
            "install",
            "fxplace",
            "--archive",
            archive.to_str().unwrap(),
            "--manifest",
            package.to_str().unwrap(),
            "--allow-unverified-publisher",
            "--approve-capabilities",
        ],
        &[],
    ));

    // This host is registered with its target; no daemon runs, so it is not
    // live and nothing qualifies yet.
    let hosts = json(&run_bookclerk(
        files,
        &["--format", "json", "hosts", "list"],
        &[],
    ));
    let rows = hosts["hosts"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["this_host"], true);
    assert_eq!(rows[0]["live"], false);
    assert_eq!(
        rows[0]["target"],
        bookclerk_plugin_catalog::host_bookclerk_target()
    );
    let bad = run_bookclerk(files, &["hosts", "label", "--add", "NAS"], &[]);
    assert!(!bad.status.success(), "labels are lowercase");
    let reserved = run_bookclerk(files, &["hosts", "label", "--add", "host=x"], &[]);
    assert!(
        !reserved.status.success(),
        "implicit label names are reserved"
    );
    let labelled = json(&run_bookclerk(
        files,
        &[
            "--format", "json", "hosts", "label", "--add", "nas", "--add", "zone=eu",
        ],
        &[],
    ));
    assert_eq!(labelled["labels"], serde_json::json!(["nas", "zone=eu"]));

    let placed = json(&run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "plugins",
            "deploy",
            "fxplace",
            "--replicas",
            "2",
            "--on",
            "nas",
        ],
        &[],
    ));
    assert_eq!(placed["placement"]["replicas"], 2);
    assert_eq!(
        placed["placement"]["constraints"],
        serde_json::json!(["nas"])
    );
    assert_eq!(placed["placement"]["revision"], 1);
    assert_eq!(placed["eligible_hosts"], serde_json::json!([]));
    let again = json(&run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "plugins",
            "deploy",
            "fxplace",
            "--replicas",
            "1",
        ],
        &[],
    ));
    assert_eq!(
        again["placement"]["revision"], 2,
        "deploy replaces in place"
    );

    let refused = run_bookclerk(files, &["plugins", "enable", "fxplace"], &[]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("placed by the scheduler"),
        "{}",
        stderr(&refused)
    );
    let listed = run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "plugins",
            "instances",
            "list",
            "--plugin",
            "fxplace",
        ],
        &[],
    );
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(
        stdout(&listed).contains("\"placement\""),
        "{}",
        stdout(&listed)
    );

    let manual = json(&run_bookclerk(
        files,
        &[
            "--format", "json", "plugins", "deploy", "fxplace", "--manual",
        ],
        &[],
    ));
    assert_eq!(manual["deleted"], true);
    let enabled = run_bookclerk(files, &["plugins", "enable", "fxplace"], &[]);
    assert!(enabled.status.success(), "{}", stderr(&enabled));

    let cleared = json(&run_bookclerk(
        files,
        &["--format", "json", "hosts", "label", "--remove", "nas"],
        &[],
    ));
    assert_eq!(cleared["labels"], serde_json::json!(["zone=eu"]));
}
