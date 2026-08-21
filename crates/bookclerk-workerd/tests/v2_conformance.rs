//! Identical v2 vectors against three backends.
//!
//! 1. Workerd author class (fixture `v2-stream`)
//! 2. Workerd → native broker → Cap'n Proto guest (`local`)
//! 3. Direct Cap'n Proto fallback (`local`)
//!
//! Never set `BOOKCLERK_V2_SKIP_WORKERD`. CI must ship `target/debug/workerd`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bookclerk_plugin_abi::v2::{
    connect_plugin, Destination, DestinationContext, DomainEvent, EventResult, Integration,
    IntegrationContext, PluginClient, WriteOptions, MAX_EVENT_PAYLOAD_BYTES, PRODUCT_API_VERSION,
};
use bookclerk_workerd::pin::binary_name;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

fn find_workerd() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    launcher
        .parent()
        .map(|dir| dir.join(binary_name()))
        .filter(|p| p.is_file())
}

fn find_echo_guest() -> Option<PathBuf> {
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    let dir = launcher.parent()?;
    let candidate = dir.join("bookclerk-plugin-echo-native-rust");
    candidate.is_file().then_some(candidate)
}

fn find_local_guest() -> Option<PathBuf> {
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    let dir = launcher.parent()?;
    let candidate = dir.join("bookclerk-plugin-destination-local");
    candidate.is_file().then_some(candidate)
}

async fn destination_roundtrip(client: &PluginClient) {
    let dest = client
        .destination(DestinationContext::default())
        .await
        .expect("destination factory");
    dest.put(
        "conformance/hello",
        Box::pin(std::io::Cursor::new(b"abc".to_vec())),
        WriteOptions::default(),
    )
    .await
    .expect("put");
    let got = dest.get("conformance/hello", None).await.expect("get");
    let mut buf = Vec::new();
    let mut body = got.body;
    body.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf, b"abc");
    let head = dest.head("conformance/hello").await.expect("head");
    assert!(head.is_some(), "head after put");
    drop(dest);
    let dest = client
        .destination(DestinationContext::default())
        .await
        .expect("destination after dispose");
    let got = dest
        .get("conformance/hello", None)
        .await
        .expect("get after dispose");
    let mut buf2 = Vec::new();
    let mut body2 = got.body;
    body2.read_to_end(&mut buf2).await.unwrap();
    assert_eq!(buf2, b"abc", "bytes survive capability dispose");
}

#[tokio::test(flavor = "current_thread")]
async fn workerd_author_conformance_vectors() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_V2_SKIP_WORKERD."
        );
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-stream");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &fixture)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("TMPDIR", tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn bookclerk-workerd");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            destination_roundtrip(&client).await;
            let _ = child.kill().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn direct_capnp_local_conformance_vectors() {
    let Some(guest) = find_local_guest() else {
        panic!(
            "bookclerk-plugin-destination-local missing beside bookclerk-workerd; \
             run `cargo build -p bookclerk-plugin-destination-local -p bookclerk-workerd`"
        );
    };
    let tmp = tempfile::tempdir().expect("tmpdir");
    let out = tmp.path().join("out");
    std::fs::create_dir_all(&out).expect("out");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(&guest)
                .env("BOOKCLERK_OUTPUT_LOCAL_ROOT", &out)
                .env("TMPDIR", tmp.path())
                .env("HOME", tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn local guest");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(30), client.describe())
                .await
                .expect("describe timed out")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            assert_eq!(desc.id, "local");
            destination_roundtrip(&client).await;
            let _ = child.kill().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_behind_workerd_local_conformance_vectors() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_V2_SKIP_WORKERD."
        );
    };
    let Some(guest) = find_local_guest() else {
        panic!(
            "bookclerk-plugin-destination-local missing; run `cargo build -p bookclerk-plugin-destination-local`"
        );
    };
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path().join("plugin");
    std::fs::create_dir_all(&root).expect("plugin root");
    std::fs::write(
        root.join("plugin.toml"),
        r#"api_version = 2
id = "local"
kind = "output"
runtime = "native"
command = "./bookclerk-plugin-destination-local"

[capabilities.network]
mode = "deny"
"#,
    )
    .expect("plugin.toml");
    let out = tmp.path().join("out");
    std::fs::create_dir_all(&out).expect("out");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &root)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("BOOKCLERK_NATIVE_BACKEND", &guest)
                .env("BOOKCLERK_OUTPUT_LOCAL_ROOT", &out)
                .env("TMPDIR", tmp.path())
                .env("HOME", tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn native-behind-workerd");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out — native-behind-workerd failed to start")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            assert_eq!(desc.id, "local");
            destination_roundtrip(&client).await;
            let _ = child.kill().await;
        })
        .await;
}

fn sample_event(event_type: &str) -> DomainEvent {
    DomainEvent {
        event_id: "e1".into(),
        event_type: event_type.into(),
        schema_version: 1,
        occurred_at_unix_ms: 1,
        deduplication_key: "k".into(),
        delivery_attempt: 1,
        payload: b"{}".to_vec(),
        ..DomainEvent::default()
    }
}

async fn event_result_vectors(client: &PluginClient) {
    let integration = client
        .integration(IntegrationContext::default())
        .await
        .expect("integration factory");
    assert_eq!(
        integration
            .on_event(sample_event("book_acquired"))
            .await
            .expect("ack"),
        EventResult::Ack
    );
    assert_eq!(
        integration
            .on_event(sample_event("test_retry"))
            .await
            .expect("retry"),
        EventResult::Retry {
            retry_at_unix_ms: 1,
            reason: "echo retry".into(),
        }
    );
    assert_eq!(
        integration
            .on_event(sample_event("test_reject"))
            .await
            .expect("reject"),
        EventResult::Reject {
            reason: "echo reject".into(),
        }
    );
    assert_eq!(
        integration
            .on_event(sample_event("test_dead_letter"))
            .await
            .expect("deadLetter"),
        EventResult::DeadLetter {
            reason: "echo dead letter".into(),
        }
    );
    assert_eq!(
        integration
            .on_event(sample_event("test_suspend"))
            .await
            .expect("suspend"),
        EventResult::Suspended {
            checkpoint_json: r#"{"n":1}"#.into(),
            checkpoint_schema_version: 1,
            wake_at_unix_ms: 1,
            wake_on_event_type: String::new(),
            wake_on_filter_json: String::new(),
        }
    );
    let mut oversized = sample_event("book_acquired");
    oversized.payload = vec![0; MAX_EVENT_PAYLOAD_BYTES as usize + 1];
    let err = integration
        .on_event(oversized)
        .await
        .expect_err("oversized payload");
    assert_eq!(
        err.code,
        bookclerk_plugin_abi::PluginErrorCode::PayloadTooLarge
    );
}

#[tokio::test(flavor = "current_thread")]
async fn workerd_author_event_vectors() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_V2_SKIP_WORKERD."
        );
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-events");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &fixture)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("TMPDIR", tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn bookclerk-workerd events fixture");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            assert_eq!(desc.id, "v2_events");
            event_result_vectors(&client).await;
            let _ = child.kill().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn direct_capnp_echo_event_vectors() {
    let Some(guest) = find_echo_guest() else {
        panic!(
            "bookclerk-plugin-echo-native-rust missing beside bookclerk-workerd; \
             run `cargo build -p bookclerk-plugin-echo-native-rust -p bookclerk-workerd`"
        );
    };
    let tmp = tempfile::tempdir().expect("tmpdir");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(&guest)
                .env("TMPDIR", tmp.path())
                .env("HOME", tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn echo native rust");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(30), client.describe())
                .await
                .expect("describe timed out")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            assert_eq!(desc.id, "echo_native_rust");
            event_result_vectors(&client).await;
            let _ = child.kill().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_behind_workerd_echo_event_vectors() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_V2_SKIP_WORKERD."
        );
    };
    let Some(guest) = find_echo_guest() else {
        panic!(
            "bookclerk-plugin-echo-native-rust missing; run `cargo build -p bookclerk-plugin-echo-native-rust`"
        );
    };
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path().join("plugin");
    std::fs::create_dir_all(&root).expect("plugin root");
    std::fs::write(
        root.join("plugin.toml"),
        r#"api_version = 2
id = "echo_native_rust"
kind = "integration"
runtime = "native"
command = "./bookclerk-plugin-echo-native-rust"

[capabilities.network]
mode = "deny"

[capabilities.methods]
list = ["describe", "integration", "health", "onEvent"]

[capabilities.events]
subscriptions = [
  { type = "book_acquired", schema_versions = [1], supports_suspend = true },
]
"#,
    )
    .expect("plugin.toml");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &root)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("BOOKCLERK_NATIVE_BACKEND", &guest)
                .env("TMPDIR", tmp.path())
                .env("HOME", tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn native-behind-workerd echo");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out — native-behind-workerd echo failed to start")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            event_result_vectors(&client).await;
            let _ = child.kill().await;
        })
        .await;
}
