//! `bookclerk clusters` through the real binary: the enrolled links (no
//! key material), the consent list, approve with narrowing and the
//! reviewed-version check, revoke, leave, and `join` refusing a link that
//! is not one, or one on the command line.

mod common;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use bookclerk_library::control_plane::{
    load_cluster_link_keys, load_remote_plugin_consent, record_cluster_link,
    record_remote_plugin_request, ClusterLinkKeys, NewClusterLink,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_host::PluginGrant;
use common::{run_bookclerk, stage_sqlite_plugin, stderr, stdout, write_config};
use serde_json::{json, Value};

/// Owner cluster id of the seeded link.
const CLUSTER: &str = "cluster-a";

/// Plugin key of the seeded request.
const PLUGIN: &str = "path:file:///tmp/store#store";

/// A files directory with the sqlite guest approved and the database
/// enrolled.
fn enrolled() -> tempfile::TempDir {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    let enrolled = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    assert!(enrolled.status.success(), "{}", stderr(&enrolled));
    files_dir
}

async fn open_store(files: &Path) -> LibraryStore {
    bookclerk_library::configure_master_key(files).expect("load the minted master key");
    let db = bookclerk_plugin_database_sqlite::open(&files.join("library.db"))
        .await
        .expect("open library.db");
    LibraryStore::from_connection(db)
}

/// Records a link to `CLUSTER` and that cluster's request to run `PLUGIN`
/// with two domains and two bindings.
async fn seed(store: &LibraryStore) {
    record_cluster_link(
        store,
        &NewClusterLink {
            owner_cluster_id: CLUSTER.into(),
            owner_origin: "https://owner.example".into(),
            remote_node_id: "node-7".into(),
            spki_pin: None,
        },
        &ClusterLinkKeys::generate(),
    )
    .await
    .expect("cluster link");
    let grant = PluginGrant {
        schema_version: bookclerk_plugin_host::GRANT_SCHEMA_VERSION,
        plugin_key: PLUGIN.into(),
        plugin_id: "store".into(),
        network_mode: "outbound".into(),
        domains: [
            "api.store.example".to_string(),
            "cdn.store.example".to_string(),
        ]
        .into(),
        manifest_domains: [
            "api.store.example".to_string(),
            "cdn.store.example".to_string(),
        ]
        .into(),
        bindings: ["config".to_string(), "secrets".to_string()].into(),
        approved_at: "2026-10-01T00:00:00Z".into(),
        ..PluginGrant::empty()
    };
    record_remote_plugin_request(
        store,
        CLUSTER,
        PLUGIN,
        "1.2.0",
        &serde_json::to_string(&grant).unwrap(),
    )
    .await
    .expect("plugin request");
}

fn json_out(output: &std::process::Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|err| panic!("json output ({err}): {}", stdout(output)))
}

/// `bookclerk clusters join` with `input` on stdin.
fn join_with_stdin(files: &Path, input: &str) -> std::process::Output {
    let mut child = Command::new(common::bookclerk_bin())
        .env("BOOKCLERK_FILES_DIR", files)
        .env_remove("BOOKCLERK_PLUGIN_DIRS")
        .env_remove("BOOKCLERK_AUTH_PASSWORD")
        .env_remove("BOOKCLERK_CONFIG")
        .env_remove("BOOKCLERK_OPERATOR_TOKEN")
        .args(["--format", "json", "clusters", "join"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bookclerk");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().expect("wait for bookclerk")
}

#[tokio::test]
async fn list_approve_revoke_and_leave() {
    let files_dir = enrolled();
    let files = files_dir.path();
    let store = open_store(files).await;
    seed(&store).await;

    let listed = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "list"],
        &[],
    ));
    assert_eq!(
        listed["clusters"],
        json!([{
            "cluster_id": CLUSTER,
            "owner_origin": "https://owner.example",
            "node_id": "node-7",
            "pinned": false,
            "state": "active",
            "enrolled_at": listed["clusters"][0]["enrolled_at"],
        }]),
        "a link without key material"
    );

    let consents = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "consents"],
        &[],
    ));
    let rows = consents["consents"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "awaiting_host_consent");
    assert_eq!(rows[0]["requested_version"], "1.2.0");
    let other = json_out(&run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "clusters",
            "consents",
            "--cluster",
            "cluster-b",
        ],
        &[],
    ));
    assert!(other["consents"].as_array().unwrap().is_empty());

    // Another version than the one reviewed, or a domain the request does
    // not ask for, is refused and stores nothing.
    for extra in [
        &["--version", "1.1.0"][..],
        &["--version", "1.2.0", "--deny-domain", "unasked.example"][..],
    ] {
        let mut args = vec!["clusters", "approve", PLUGIN];
        args.extend_from_slice(extra);
        let refused = run_bookclerk(files, &args, &[]);
        assert!(!refused.status.success(), "{extra:?}: {}", stdout(&refused));
    }
    assert!(load_remote_plugin_consent(&store, CLUSTER, PLUGIN)
        .await
        .unwrap()
        .unwrap()
        .approved_at
        .is_none());

    let approved = json_out(&run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "clusters",
            "approve",
            PLUGIN,
            "--version",
            "1.2.0",
            "--deny-domain",
            "cdn.store.example",
            "--deny-binding",
            "secrets",
        ],
        &[],
    ));
    let consent = &approved["consent"];
    assert_eq!(consent["state"], "consented");
    assert_eq!(consent["approved_by"], "cli");
    assert_eq!(consent["grant"]["domains"], json!(["api.store.example"]));
    assert_eq!(consent["grant"]["bindings"], json!(["config"]));

    let revoked = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "revoke", PLUGIN],
        &[],
    ));
    assert_eq!(revoked["consent"]["state"], "awaiting_host_consent");
    assert!(revoked["consent"]["grant"].is_null());
    let unknown = run_bookclerk(files, &["clusters", "revoke", "other"], &[]);
    assert!(!unknown.status.success());

    let unknown = run_bookclerk(files, &["clusters", "leave", "--cluster", "nope"], &[]);
    assert!(!unknown.status.success());
    let left = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "leave"],
        &[],
    ));
    assert_eq!(left["cluster"]["state"], "left");
    let link = bookclerk_library::control_plane::load_cluster_link(&store, CLUSTER)
        .await
        .unwrap()
        .unwrap();
    assert!(
        load_cluster_link_keys(&store, &link).await.is_err(),
        "the node keys are gone"
    );
}

#[tokio::test]
async fn join_takes_the_link_only_from_stdin_and_refuses_a_non_link() {
    let files_dir = enrolled();
    let files = files_dir.path();

    // No positional link: the secret never rides on argv.
    let argv = run_bookclerk(
        files,
        &["clusters", "join", "https://owner.example/enroll#bcn1x"],
        &[],
    );
    assert!(!argv.status.success(), "{}", stdout(&argv));

    let empty = join_with_stdin(files, "");
    assert!(!empty.status.success());
    assert!(
        stderr(&empty).contains("no enrollment link"),
        "{}",
        stderr(&empty)
    );

    let secret = "bcn1-not-a-real-token-secret";
    let bad = join_with_stdin(files, &format!("https://owner.example/enroll#{secret}\n"));
    assert!(!bad.status.success());
    let err = stderr(&bad);
    assert!(err.contains("not an enrollment link"), "{err}");
    assert!(!err.contains(secret), "the link is not echoed: {err}");
    assert!(!stdout(&bad).contains(secret));

    let store = open_store(files).await;
    assert!(bookclerk_library::control_plane::list_cluster_links(&store)
        .await
        .unwrap()
        .is_empty());
}
