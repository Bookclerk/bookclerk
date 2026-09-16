//! Identical contract vectors against three backends.
//!
//! 1. Workerd author class (fixture `stream`)
//! 2. Workerd control plane → typed Cap'n Proto passthrough to a native guest (`local`)
//! 3. Direct Cap'n Proto fallback (`local`)
//!
//! Never set `BOOKCLERK_SKIP_WORKERD`. CI must ship `target/debug/workerd`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use std::sync::{Arc, Mutex};

use bookclerk_plugin_abi::{
    connect_plugin, AdapterExecuteRequest, ByteRange, Cancellation, CatalogDetailParams, CliArg,
    CliInvokeParams, ContentSource, CopyResult, Database, DbCapabilities, DbColumn,
    DbPlanStatementKind, DbResultSelection, DbRow, DbTiming, DbType, DbValue, Destination,
    DestinationClient, DomainEvent, Entrypoint, EventConsumer, EventConsumerClient, EventPublisher,
    EventResult, ExecuteReply, ExecuteRequest, GuestDatabase, GuestReceiptPersist, HostBindings,
    Invocation, JobInvocation, JobOutcome, ListOptions, ListPage, LoginParams, ObjectMetadata,
    PluginCli, PluginClient, PluginError, PluginErrorCode, PluginEvent, ProgressSink, PublishOk,
    PurchaseHintParams, PutResult, ReadResult, ResolvedStatement, SearchCatalogParams, Source,
    StatementResult, TypedDbStatement, WriteOptions, MAX_EVENT_PAYLOAD_BYTES, PRODUCT_API_VERSION,
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

/// Opens the guest's `storage` entrypoint for one operator-wide invocation.
async fn open_storage(client: &PluginClient, id: &str) -> DestinationClient {
    client
        .open(
            &Invocation {
                id: id.into(),
                ..Default::default()
            },
            HostBindings::default(),
        )
        .await
        .expect("open")
        .storage
        .expect("guest exports `storage`")
}

/// Opens the guest's event consumer for one operator-wide invocation.
async fn open_event_consumer(client: &PluginClient) -> EventConsumerClient {
    client
        .open(
            &Invocation {
                id: "events".into(),
                ..Default::default()
            },
            HostBindings::default(),
        )
        .await
        .expect("open")
        .event_consumer
        .expect("guest exports an event consumer")
}

/// Delivers one event and returns its single result.
async fn deliver(
    consumer: &EventConsumerClient,
    event: DomainEvent,
) -> bookclerk_plugin_abi::Result<EventResult> {
    let mut results = consumer.event(vec![event]).await?;
    assert_eq!(results.len(), 1, "one result per delivered event");
    Ok(results.pop().expect("one result"))
}

async fn destination_roundtrip(client: &PluginClient) {
    let dest = open_storage(client, "roundtrip").await;
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
    let dest = open_storage(client, "after-dispose").await;
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
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
        );
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stream");
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
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
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
        r#"api_version = 3
id = "local"
runtime = "native"
command = "./bookclerk-plugin-destination-local"
entrypoints = ["storage"]

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
    let consumer = open_event_consumer(client).await;
    assert_eq!(
        deliver(&consumer, sample_event("book_acquired"))
            .await
            .expect("ack"),
        EventResult::Ack
    );
    assert_eq!(
        deliver(&consumer, sample_event("test_retry"))
            .await
            .expect("retry"),
        EventResult::Retry {
            retry_at_unix_ms: 1,
            reason: "echo retry".into(),
        }
    );
    assert_eq!(
        deliver(&consumer, sample_event("test_reject"))
            .await
            .expect("reject"),
        EventResult::Reject {
            reason: "echo reject".into(),
        }
    );
    assert_eq!(
        deliver(&consumer, sample_event("test_dead_letter"))
            .await
            .expect("deadLetter"),
        EventResult::DeadLetter {
            reason: "echo dead letter".into(),
        }
    );
    assert_eq!(
        deliver(&consumer, sample_event("test_suspend"))
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
    let err = deliver(&consumer, oversized)
        .await
        .expect_err("oversized payload");
    assert_eq!(
        err.code,
        bookclerk_plugin_abi::PluginErrorCode::PayloadTooLarge
    );
}

/// Host-side `EVENTS` publisher for the contract: records what the guest
/// published and fails closed on anything but the one granted producer.
#[derive(Default)]
struct RecordingPublisher {
    published: Mutex<Vec<PluginEvent>>,
}

#[async_trait::async_trait(?Send)]
impl EventPublisher for RecordingPublisher {
    async fn publish(&self, event: PluginEvent) -> bookclerk_plugin_abi::Result<PublishOk> {
        if event.event_type != "fixture_pinged" {
            return Err(PluginError::forbidden(format!(
                "`{}` is not a granted producer",
                event.event_type
            )));
        }
        let mut published = self.published.lock().expect("recorder lock");
        let n = published.len();
        published.push(event);
        Ok(PublishOk {
            event_id: format!("evt-{n}"),
            duplicate: n > 0,
        })
    }
}

/// `EVENTS` reaches the author only through the adapter's granted channel:
/// the publish lands on the host publisher with the guest's fields intact,
/// ungranted types fail closed, and an `open` without `Bindings.events`
/// leaves the author without the binding.
async fn events_binding_vectors(client: &PluginClient) {
    let recorder = Arc::new(RecordingPublisher::default());
    let publisher: Arc<dyn EventPublisher> = Arc::clone(&recorder) as Arc<dyn EventPublisher>;
    let opened = client
        .open(
            &Invocation {
                id: "events-granted".into(),
                account_id: "acct".into(),
                correlation_id: "corr-1".into(),
                ..Default::default()
            },
            HostBindings {
                events: Some(publisher),
                ..HostBindings::default()
            },
        )
        .await
        .expect("open with EVENTS");
    let consumer = opened.event_consumer.expect("event consumer");
    let mut trigger = sample_event("test_publish");
    trigger.event_id = "trigger-1".into();
    trigger.payload = br#"{"n":7}"#.to_vec();
    trigger.correlation_id = "corr-from-event".into();
    let result = deliver(&consumer, trigger).await.expect("deliver");
    let EventResult::Reject { reason } = result else {
        panic!("fixture reports the publish outcome as a reject reason: {result:?}");
    };
    let ok: PublishOk = serde_json::from_str(&reason).unwrap_or_else(|err| {
        panic!("publish outcome must be a PublishOk JSON, got `{reason}`: {err}")
    });
    assert_eq!(ok.event_id, "evt-0");
    assert!(!ok.duplicate);
    {
        let published = recorder.published.lock().expect("recorder lock");
        assert_eq!(published.len(), 1, "one publish reached the host");
        let event = &published[0];
        assert_eq!(event.event_type, "fixture_pinged");
        assert_eq!(event.deduplication_key, "pinged:trigger-1");
        assert_eq!(event.correlation_id, "corr-from-event");
        assert_eq!(event.schema_version, 1);
        let payload: serde_json::Value = serde_json::from_slice(&event.payload).expect("json");
        assert_eq!(payload, serde_json::json!({ "from": "trigger-1", "n": 7 }));
    }
    let again = deliver(&consumer, sample_event("test_publish"))
        .await
        .expect("deliver");
    let EventResult::Reject { reason } = again else {
        panic!("unexpected {again:?}");
    };
    let ok: PublishOk = serde_json::from_str(&reason).expect("PublishOk");
    assert!(ok.duplicate, "host duplicate flag reaches the author");

    let forbidden = deliver(&consumer, sample_event("test_publish_forbidden"))
        .await
        .expect("deliver");
    assert_eq!(
        forbidden,
        EventResult::Reject {
            reason: "publish failed: forbidden".into(),
        },
        "ungranted producer fails closed with the host's wire code"
    );
    assert_eq!(recorder.published.lock().expect("recorder lock").len(), 2);

    let plain = open_event_consumer(client).await;
    assert_eq!(
        deliver(&plain, sample_event("test_publish"))
            .await
            .expect("deliver"),
        EventResult::Reject {
            reason: "no EVENTS binding".into(),
        },
        "no Bindings.events → no EVENTS on the author env"
    );
}

/// Host-side named `[[databases]]` binding for the contract: echoes the
/// first bound parameter of every statement as one `int64` row and records
/// what it was asked to run.
#[derive(Default)]
struct EchoDatabase {
    requests: Mutex<Vec<ExecuteRequest>>,
}

#[async_trait::async_trait(?Send)]
impl GuestDatabase for EchoDatabase {
    async fn execute(&self, request: ExecuteRequest) -> bookclerk_plugin_abi::Result<ExecuteReply> {
        let statements = request
            .statements
            .iter()
            .map(|stmt| StatementResult {
                rows: vec![DbRow {
                    values: vec![stmt
                        .parameters
                        .first()
                        .cloned()
                        .unwrap_or(DbValue::Int64(0))],
                }],
                columns: vec![DbColumn {
                    name: "n".into(),
                    db_type: DbType::Int64,
                }],
                rows_affected: 0,
            })
            .collect();
        let operation_id = request.operation_id.clone();
        self.requests.lock().expect("requests").push(request);
        Ok(ExecuteReply {
            operation_id,
            statements,
            timing: DbTiming::default(),
        })
    }
}

/// A named `[[databases]]` binding granted at `open` reaches the author as
/// `env.DB` on every call of that `open`; the SQL and bound parameters cross
/// the granted `/db/execute` channel typed, and an `open` without the
/// binding leaves the author without it.
async fn databases_binding_vectors(client: &PluginClient) {
    let echo = Arc::new(EchoDatabase::default());
    let opened = client
        .open(
            &Invocation {
                id: "db-granted".into(),
                ..Default::default()
            },
            HostBindings {
                databases: vec![("DB".into(), Arc::clone(&echo) as Arc<dyn GuestDatabase>)],
                ..HostBindings::default()
            },
        )
        .await
        .expect("open with DB");
    let consumer = opened.event_consumer.expect("event consumer");
    let result = deliver(&consumer, sample_event("test_database"))
        .await
        .expect("deliver");
    let EventResult::Reject { reason } = result else {
        panic!("fixture reports the first row as a reject reason: {result:?}");
    };
    let row: serde_json::Value = serde_json::from_str(&reason)
        .unwrap_or_else(|err| panic!("first row must be JSON, got `{reason}`: {err}"));
    assert_eq!(row["n"]["value"], serde_json::json!(41), "row: {row}");
    {
        let requests = echo.requests.lock().expect("requests");
        assert_eq!(
            requests.len(),
            1,
            "one typed batch reached the host binding"
        );
        assert_eq!(requests[0].statements.len(), 1);
        assert_eq!(requests[0].statements[0].sql, "SELECT ? AS n");
        assert_eq!(
            requests[0].statements[0].parameters,
            vec![DbValue::Int64(41)]
        );
    }

    let plain = open_event_consumer(client).await;
    assert_eq!(
        deliver(&plain, sample_event("test_database"))
            .await
            .expect("deliver"),
        EventResult::Reject {
            reason: "no DB binding".into(),
        },
        "no Bindings.databases → no env.DB on the author"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn workerd_author_event_vectors() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
        );
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/events");
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
            assert_eq!(desc.id, "events_fixture");
            event_result_vectors(&client).await;
            events_binding_vectors(&client).await;
            databases_binding_vectors(&client).await;
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
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
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
        r#"api_version = 3
id = "echo_native_rust"
runtime = "native"
command = "./bookclerk-plugin-echo-native-rust"

[capabilities.network]
mode = "deny"

[[events.consumers]]
type = "book_acquired"
schema_versions = [1]
supports_suspend = true
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

fn find_sqlite_guest() -> Option<PathBuf> {
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    let dir = launcher.parent()?;
    let candidate = dir.join("bookclerk-plugin-database-sqlite");
    candidate.is_file().then_some(candidate)
}

/// Spawns `bookclerk-workerd` in native mode over `guest` with the manifest
/// at `root`, plus extra guest environment.
fn spawn_native_behind_workerd(
    workerd: &Path,
    root: &Path,
    guest: &Path,
    tmp: &Path,
    extra_env: &[(&str, &Path)],
) -> tokio::process::Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    cmd.env("BOOKCLERK_PLUGIN_ROOT", root)
        .env("BOOKCLERK_WORKERD_BIN", workerd)
        .env("BOOKCLERK_NATIVE_BACKEND", guest)
        .env("TMPDIR", tmp)
        .env("HOME", tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.spawn().expect("spawn native-behind-workerd")
}

/// One typed `SELECT 1` batch with a hash-bound proof, as the host would send.
fn select_one_request() -> AdapterExecuteRequest {
    let sql = "SELECT 1";
    let request = ExecuteRequest {
        operation_id: "conformance-select-one".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.into(),
            parameters: Vec::new(),
            kind: DbPlanStatementKind::Select,
            max_rows: 1,
            result_selection: DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    };
    let envelope = AdapterExecuteRequest::new(request, GuestReceiptPersist::default())
        .with_proofs(vec![ResolvedStatement::bound_empty(sql)]);
    envelope.require_proofs().expect("proof bound to SELECT 1");
    envelope
}

/// Opens the guest's database adapter, runs `SELECT 1`, and returns the
/// capabilities plus the single statement result.
async fn database_select_one(client: &PluginClient) -> (DbCapabilities, StatementResult) {
    let opened = client
        .open(
            &Invocation {
                id: "database".into(),
                ..Default::default()
            },
            HostBindings::default(),
        )
        .await
        .expect("open");
    let adapter = opened
        .database_adapter
        .expect("guest exports `databaseAdapter`");
    let session = adapter.open_session().await.expect("open_session");
    let capabilities = session.capabilities().await.expect("capabilities");
    let reply = session
        .execute(select_one_request())
        .await
        .expect("execute SELECT 1");
    assert_eq!(reply.operation_id, "conformance-select-one");
    assert_eq!(reply.statements.len(), 1, "one statement result");
    let result = reply.statements.into_iter().next().expect("one result");
    assert_eq!(result.rows.len(), 1, "SELECT 1 yields one row");
    assert_eq!(result.rows[0].values, vec![DbValue::Int64(1)]);
    session.close().await.expect("close");
    (capabilities, result)
}

#[tokio::test(flavor = "current_thread")]
async fn native_behind_workerd_sqlite_database_vectors() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
        );
    };
    let Some(guest) = find_sqlite_guest() else {
        panic!(
            "bookclerk-plugin-database-sqlite missing; run `cargo build -p bookclerk-plugin-database-sqlite`"
        );
    };
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path().join("plugin");
    std::fs::create_dir_all(&root).expect("plugin root");
    std::fs::write(
        root.join("plugin.toml"),
        r#"api_version = 3
id = "sqlite"
runtime = "native"
command = "./bookclerk-plugin-database-sqlite"
entrypoints = ["databaseAdapter"]

[capabilities.network]
mode = "deny"
"#,
    )
    .expect("plugin.toml");
    let direct_db = tmp.path().join("direct.sqlite");
    let workerd_db = tmp.path().join("workerd.sqlite");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut direct = Command::new(&guest)
                .env("BOOKCLERK_SQLITE_PATH", &direct_db)
                .env("TMPDIR", tmp.path())
                .env("HOME", tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn sqlite guest");
            let stdin = direct.stdin.take().expect("stdin");
            let stdout = direct.stdout.take().expect("stdout");
            let (direct_client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let direct_desc =
                tokio::time::timeout(Duration::from_secs(30), direct_client.describe())
                    .await
                    .expect("direct describe timed out")
                    .expect("direct describe");
            let (direct_caps, direct_result) = database_select_one(&direct_client).await;
            let _ = direct.kill().await;

            let mut child = spawn_native_behind_workerd(
                &workerd,
                &root,
                &guest,
                tmp.path(),
                &[("BOOKCLERK_SQLITE_PATH", workerd_db.as_path())],
            );
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out — native-behind-workerd sqlite failed to start")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            assert_eq!(desc.id, "sqlite");
            assert_eq!(desc.id, direct_desc.id, "manifest and guest agree on id");
            let (caps, result) = database_select_one(&client).await;
            assert_eq!(caps, direct_caps, "capabilities identical on both paths");
            assert_eq!(
                result.rows, direct_result.rows,
                "rows identical on both paths"
            );
            assert_eq!(
                result.columns, direct_result.columns,
                "columns identical on both paths"
            );
            let _ = child.kill().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_behind_workerd_open_nulls_undeclared_entrypoints() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
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
        r#"api_version = 3
id = "local"
runtime = "native"
command = "./bookclerk-plugin-destination-local"
entrypoints = ["cli"]

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
            let mut child = spawn_native_behind_workerd(
                &workerd,
                &root,
                &guest,
                tmp.path(),
                &[("BOOKCLERK_OUTPUT_LOCAL_ROOT", out.as_path())],
            );
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out — native-behind-workerd failed to start")
                .expect("describe");
            assert_eq!(
                desc.capabilities.entrypoints,
                vec![Entrypoint::Cli],
                "manifest is authoritative on entrypoints"
            );
            let opened = client
                .open(
                    &Invocation {
                        id: "undeclared".into(),
                        ..Default::default()
                    },
                    HostBindings::default(),
                )
                .await
                .expect("open");
            assert!(
                opened.storage.is_none(),
                "storage is nulled when the manifest does not declare it"
            );
            assert!(
                opened.job_runner.is_none(),
                "jobRunner is nulled without jobs or storage in the manifest"
            );
            assert!(opened.cli.is_none(), "guest exports no cli");
            let _ = child.kill().await;
        })
        .await;
}

/// Spawns the workerd author launcher over `fixture`, connects the Cap'n
/// control plane, and pins `describe()` to the manifest id. Call inside a
/// `LocalSet`.
async fn spawn_author(
    workerd: &Path,
    fixture: &Path,
    tmp: &Path,
    expect_id: &str,
) -> (tokio::process::Child, PluginClient) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
        .env("BOOKCLERK_PLUGIN_ROOT", fixture)
        .env("BOOKCLERK_WORKERD_BIN", workerd)
        .env("TMPDIR", tmp)
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
    assert_eq!(desc.id, expect_id);
    (child, client)
}

fn require_workerd() -> PathBuf {
    find_workerd().unwrap_or_else(|| {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
        )
    })
}

/// Every `ContentSource` / `PluginCli` method travels as one typed `/invoke`
/// round trip: partial author objects land as defaults, `null` optional
/// results land as `None`, and thrown `PluginError`s keep their wire code.
#[tokio::test(flavor = "current_thread")]
async fn workerd_author_storefront_vectors() {
    let workerd = require_workerd();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/storefront");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut child, client) =
                spawn_author(&workerd, &fixture, tmp.path(), "storefront_fixture").await;
            let opened = client
                .open(
                    &Invocation {
                        id: "storefront".into(),
                        ..Default::default()
                    },
                    HostBindings::default(),
                )
                .await
                .expect("open");
            assert!(opened.storage.is_none(), "storage is not declared");
            assert!(opened.event_consumer.is_none(), "no event consumer");
            assert!(opened.job_runner.is_none(), "no job runner");
            assert!(opened.database_adapter.is_none(), "no database adapter");
            assert!(opened.remote_library.is_none(), "no remote library");
            assert!(opened.oidc.is_none(), "no oidc");
            let storefront = opened.storefront.expect("storefront entrypoint");
            let cli = opened.cli.expect("cli entrypoint");

            let health = storefront.health().await.expect("health");
            assert!(health.ok);
            assert!(
                health.detail.starts_with("env="),
                "author env reaches the named entrypoint: {}",
                health.detail
            );
            assert_eq!(
                storefront.diagnose().await.expect("diagnose"),
                vec!["line one".to_string(), "line two".to_string()]
            );

            let accounts = storefront.list_accounts().await.expect("listAccounts");
            assert_eq!(accounts.len(), 2);
            assert_eq!(accounts[0].account_id, "acct-1");
            assert_eq!(accounts[0].label.as_deref(), Some("One"));
            assert!(accounts[0].scan_enabled);
            assert_eq!(accounts[1].marketplace, "uk");
            assert_eq!(accounts[1].label, None, "absent optional text is None");
            assert!(!accounts[1].scan_enabled, "absent bool is the default");

            let hits = storefront
                .search_catalog(SearchCatalogParams {
                    query: "dune".into(),
                    limit: 5,
                    page: 2,
                    ..Default::default()
                })
                .await
                .expect("searchCatalog");
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].product_id, "hit:dune:5:2");
            assert_eq!(hits[0].title, "dune");
            assert_eq!(hits[0].authors.as_deref(), Some("A. Author"));
            assert_eq!(hits[0].narrators, None);

            assert_eq!(
                storefront
                    .purchase_hint(PurchaseHintParams::default())
                    .await
                    .expect("purchaseHint"),
                None,
                "author `null` is `found = false`"
            );
            assert_eq!(
                storefront
                    .catalog_detail(CatalogDetailParams {
                        product_id: "missing".into(),
                        ..Default::default()
                    })
                    .await
                    .expect("catalogDetail missing"),
                None
            );
            let detail = storefront
                .catalog_detail(CatalogDetailParams {
                    product_id: "p1".into(),
                    ..Default::default()
                })
                .await
                .expect("catalogDetail")
                .expect("found");
            assert_eq!(detail.product_id, "p1");
            assert_eq!(detail.title, "Detail p1");

            let err = storefront
                .login(LoginParams {
                    marketplace: "us".into(),
                    ..Default::default()
                })
                .await
                .expect_err("login fails");
            assert_eq!(err.code, PluginErrorCode::Unauthorized);
            assert_eq!(err.message, "no credentials for us");

            let schema = cli.describe().await.expect("cli describe");
            assert_eq!(schema.commands.len(), 1);
            assert_eq!(schema.commands[0].name, "echo");
            assert_eq!(
                schema.commands[0].about.as_deref(),
                Some("Echo the arguments")
            );
            let out = cli
                .invoke(CliInvokeParams {
                    command: "echo".into(),
                    args: vec![
                        CliArg {
                            name: "a".into(),
                            value: "1".into(),
                        },
                        CliArg {
                            name: "b".into(),
                            value: "2".into(),
                        },
                    ],
                })
                .await
                .expect("cli invoke");
            assert_eq!(out.exit_code, 3);
            assert_eq!(out.stdout, "echo:a=1;b=2");
            assert_eq!(out.stderr, "");
            let _ = child.kill().await;
        })
        .await;
}

/// Host-side capabilities the job test grants to the author.
#[derive(Default)]
struct MemoryObjects {
    objects: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
}

#[async_trait::async_trait(?Send)]
impl Source for MemoryObjects {
    async fn open(&self, key: &str) -> bookclerk_plugin_abi::Result<ReadResult> {
        let bytes = self
            .objects
            .lock()
            .expect("objects")
            .get(key)
            .cloned()
            .ok_or_else(|| PluginError::not_found(format!("no object {key}")))?;
        Ok(ReadResult {
            meta: ObjectMetadata {
                key: key.into(),
                size: bytes.len() as u64,
                content_type: Some("text/plain".into()),
                ..ObjectMetadata::default()
            },
            body: Box::pin(std::io::Cursor::new(bytes)),
        })
    }
}

#[async_trait::async_trait(?Send)]
impl Destination for MemoryObjects {
    async fn head(&self, key: &str) -> bookclerk_plugin_abi::Result<Option<ObjectMetadata>> {
        Ok(self
            .objects
            .lock()
            .expect("objects")
            .get(key)
            .map(|bytes| ObjectMetadata {
                key: key.into(),
                size: bytes.len() as u64,
                ..ObjectMetadata::default()
            }))
    }

    async fn list(&self, _options: ListOptions) -> bookclerk_plugin_abi::Result<ListPage> {
        Err(PluginError::unsupported("list"))
    }

    async fn get(
        &self,
        key: &str,
        _range: Option<ByteRange>,
    ) -> bookclerk_plugin_abi::Result<ReadResult> {
        Source::open(self, key).await
    }

    async fn put(
        &self,
        key: &str,
        mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        _options: WriteOptions,
    ) -> bookclerk_plugin_abi::Result<PutResult> {
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)
            .await
            .map_err(|err| PluginError::internal(err.to_string()))?;
        let n = bytes.len() as u64;
        self.objects
            .lock()
            .expect("objects")
            .insert(key.into(), bytes);
        Ok(PutResult {
            key: key.into(),
            bytes_written: n,
            ..PutResult::default()
        })
    }

    async fn copy(&self, _from: &str, _to: &str) -> bookclerk_plugin_abi::Result<CopyResult> {
        Err(PluginError::unsupported("copy"))
    }

    async fn delete(&self, key: &str) -> bookclerk_plugin_abi::Result<()> {
        self.objects.lock().expect("objects").remove(key);
        Ok(())
    }
}

/// Records `(percent, message)` reports.
#[derive(Default)]
struct RecordingProgress {
    reports: Mutex<Vec<(u32, String)>>,
}

#[async_trait::async_trait(?Send)]
impl ProgressSink for RecordingProgress {
    async fn report(&self, percent: f32, message: &str) -> bookclerk_plugin_abi::Result<()> {
        self.reports
            .lock()
            .expect("reports")
            .push((percent.round() as u32, message.into()));
        Ok(())
    }
}

/// Cancellation flag the test flips while the author is waiting.
#[derive(Default)]
struct FlagCancel {
    cancelled: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait(?Send)]
impl Cancellation for FlagCancel {
    async fn poll(&self) -> bookclerk_plugin_abi::Result<bool> {
        Ok(self.cancelled.load(std::sync::atomic::Ordering::SeqCst))
    }
}

fn job_invocation(id: &str, payload: serde_json::Value) -> JobInvocation {
    JobInvocation {
        payload_schema_version: 1,
        invocation_id: id.into(),
        command_type: "stream_copy".into(),
        payload_json: payload.to_string(),
        idempotency_key: format!("idem-{id}"),
        attempt: 1,
        correlation_id: String::new(),
        causation_id: None,
        deadline_unix_ms: 0,
        checkpoint: None,
        invocation_sequence: 0,
        step_id: None,
    }
}

/// `JobRunner.job` carries the four granted capabilities in the Cap'n
/// capability table; the author reads the source, writes the destination,
/// reports progress, and observes host cancellation through `job.signal`.
#[tokio::test(flavor = "current_thread")]
async fn workerd_author_job_vectors() {
    let workerd = require_workerd();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stream");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut child, client) =
                spawn_author(&workerd, &fixture, tmp.path(), "stream_fixture").await;
            let opened = client
                .open(
                    &Invocation {
                        id: "job-1".into(),
                        account_id: "acct".into(),
                        ..Default::default()
                    },
                    HostBindings::default(),
                )
                .await
                .expect("open");
            let runner = opened.job_runner.expect("job runner");

            let input = Arc::new(MemoryObjects::default());
            input
                .objects
                .lock()
                .expect("objects")
                .insert("in/a".into(), b"abc".to_vec());
            let output = Arc::new(MemoryObjects::default());
            let progress = Arc::new(RecordingProgress::default());
            let outcome = runner
                .job(
                    &job_invocation(
                        "copy-1",
                        serde_json::json!({ "from": "in/a", "to": "out/a" }),
                    ),
                    Arc::clone(&input) as Arc<dyn Source>,
                    Arc::clone(&output) as Arc<dyn Destination>,
                    Arc::clone(&progress) as Arc<dyn ProgressSink>,
                    Arc::new(FlagCancel::default()) as Arc<dyn Cancellation>,
                )
                .await
                .expect("job");
            assert_eq!(
                outcome,
                JobOutcome::Completed {
                    message: "copied in/a -> out/a".into(),
                    bytes_copied: 3,
                }
            );
            assert_eq!(
                output.objects.lock().expect("objects").get("out/a"),
                Some(&b"abc".to_vec()),
                "bytes crossed source → author → destination"
            );
            assert_eq!(
                *progress.reports.lock().expect("reports"),
                vec![
                    (0, "opening".to_string()),
                    (10, "copying".to_string()),
                    (100, "done".to_string()),
                ]
            );

            let cancel = Arc::new(FlagCancel::default());
            let progress = Arc::new(RecordingProgress::default());
            let flip = {
                let cancel = Arc::clone(&cancel);
                let progress = Arc::clone(&progress);
                async move {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                    while progress.reports.lock().expect("reports").is_empty() {
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "author never reported it was waiting"
                        );
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    cancel
                        .cancelled
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            };
            let invocation = job_invocation("cancel-1", serde_json::json!({ "awaitCancel": true }));
            let run = runner.job(
                &invocation,
                Arc::clone(&input) as Arc<dyn Source>,
                Arc::clone(&output) as Arc<dyn Destination>,
                Arc::clone(&progress) as Arc<dyn ProgressSink>,
                Arc::clone(&cancel) as Arc<dyn Cancellation>,
            );
            let (outcome, ()) = tokio::join!(run, flip);
            assert_eq!(
                outcome.expect("cancelled run still yields an outcome"),
                JobOutcome::Cancelled {
                    message: "host cancelled the copy".into(),
                },
                "a thrown `cancelled` error is the cancelled outcome"
            );
            assert_eq!(
                progress.reports.lock().expect("reports")[0],
                (0, "waiting for cancel".to_string())
            );
            let _ = child.kill().await;
        })
        .await;
}

/// One `EventConsumer.event` call with a batch keeps one result per event in
/// delivery order.
#[tokio::test(flavor = "current_thread")]
async fn workerd_author_event_batch_vectors() {
    let workerd = require_workerd();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/events");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut child, client) =
                spawn_author(&workerd, &fixture, tmp.path(), "events_fixture").await;
            let consumer = open_event_consumer(&client).await;
            let mut first = sample_event("book_acquired");
            first.event_id = "b1".into();
            let mut second = sample_event("test_retry");
            second.event_id = "b2".into();
            let mut third = sample_event("test_reject");
            third.event_id = "b3".into();
            let results = consumer
                .event(vec![first, second, third])
                .await
                .expect("batch");
            assert_eq!(
                results,
                vec![
                    EventResult::Ack,
                    EventResult::Retry {
                        retry_at_unix_ms: 1,
                        reason: "echo retry".into(),
                    },
                    EventResult::Reject {
                        reason: "echo reject".into(),
                    },
                ]
            );
            assert_eq!(
                consumer.event(Vec::new()).await.expect("empty batch"),
                Vec::<EventResult>::new(),
                "an empty batch yields no results"
            );
            let _ = child.kill().await;
        })
        .await;
}

/// `Database.openSession` exports an isolate-held session the launcher
/// addresses with `X-Bookclerk-Target`; sessions survive between `/invoke`
/// calls and `close` releases them.
#[tokio::test(flavor = "current_thread")]
async fn workerd_author_database_adapter_vectors() {
    let workerd = require_workerd();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dbadapter");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut child, client) =
                spawn_author(&workerd, &fixture, tmp.path(), "dbadapter_fixture").await;
            let (caps, result) = database_select_one(&client).await;
            assert!(caps.atomic_batch);
            assert_eq!(caps.max_binds, 32);
            assert_eq!(result.columns.len(), 1);
            assert_eq!(result.columns[0].name, "1");

            let opened = client
                .open(
                    &Invocation {
                        id: "database-2".into(),
                        ..Default::default()
                    },
                    HostBindings::default(),
                )
                .await
                .expect("open");
            let adapter = opened.database_adapter.expect("database adapter");
            let session = adapter.open_session().await.expect("second session");
            assert_eq!(
                session.bootstrap().await.expect("bootstrap").engine,
                "memory opened=2 closed=1",
                "sessions are isolate objects with their own lifecycle"
            );
            let err = session
                .execute(select_two_request())
                .await
                .expect_err("memory engine rejects anything but SELECT 1");
            assert_eq!(err.code, PluginErrorCode::Unsupported);
            session.close().await.expect("close");
            let after = session.execute(select_one_request()).await;
            assert!(
                after.is_err(),
                "a closed session is gone from the isolate: {after:?}"
            );
            let _ = child.kill().await;
        })
        .await;
}

/// A proven `SELECT 2` the memory fixture refuses.
fn select_two_request() -> AdapterExecuteRequest {
    let sql = "SELECT 2";
    let request = ExecuteRequest {
        operation_id: "conformance-select-two".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.into(),
            parameters: Vec::new(),
            kind: DbPlanStatementKind::Select,
            max_rows: 1,
            result_selection: DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    };
    AdapterExecuteRequest::new(request, GuestReceiptPersist::default())
        .with_proofs(vec![ResolvedStatement::bound_empty(sql)])
}
