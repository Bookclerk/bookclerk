//! The host client against the step-6 owner, in one process: enroll, run a
//! job end to end through the daemon's job transport with the owner's
//! sealed secret and a token write-back, ack a wake, resume a held fence
//! after a disconnect and abandon one reclaimed while away, refuse a wrong
//! SPKI pin, keep every token out of TRACE logs, and follow a database
//! swap to the new store's links.
//!
//! Host consent (step 8): an owner plugin deployed to the host is recorded
//! as a request and never claimed until the host's operator approves it;
//! once approved, its archive comes over the signed artifact endpoint, its
//! `CONFIG` and `SECRETS` over `instance.config` (again after a
//! `configuration.changed` wake), and they reach the job's bindings but not
//! the logs; a revoke mid-job cancels the run and gives the job back.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bookclerk_library::control_plane::{
    ensure_plugin_deployment, load_instance_config, load_library_link, load_library_link_keys,
    load_node_status, load_observation, load_or_create_node_identity, load_remote_plugin_consent,
    mint_enrollment_link, node_queue_name, publish_node_message, replace_instance_config,
    revoke_remote_plugin, seal_instance_secret, set_secret_ref, ConfigActor, DeploymentStatus,
    DesiredState, EnrollmentActor, LibraryLink, NodeHeartbeat, NodeMessage, PluginInstanceConfigV1,
    PluginInstanceId, RemoteApprover, SettingValue,
};
use bookclerk_library::secrets::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret, upsert_secret,
};
use bookclerk_library::{
    EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, JobState, LibraryStore, UserRole,
};
use bookclerk_node_client::{
    join_library, ClientTiming, DialError, HeartbeatSource, HostClient, HostClientConfig,
    HostClientHandle, JoinError, OwnerEndpoint,
};
use bookclerk_node_proto::{SecretRef, ARTIFACTS_PATH, CLOSE_SUPERSEDED};
use bookclerk_plugin_host::{approve_remote_request, PluginGrant, RemoteNarrowing};
use tokio::sync::watch;

use super::consent::{notify_consent_changed, LinkSupervisor};
use super::remote_bindings::RemoteCatalog;
use super::{AbortOnDrop, TransportRunner};
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// Plugin key of the owner's store account.
pub(super) const PLUGIN: &str = "path:file:///tmp/store";

/// Serves `app` on a loopback port, as the daemon's listeners do.
pub(super) async fn serve(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    let app = app.layer(axum::Extension(crate::node_api::transport::LocalAddr(
        local,
    )));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    local
}

/// A host install's own database (sealed with the process DEK the test
/// owner configured).
pub(super) async fn host_store(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .unwrap();
    bookclerk_library::apply_host_schema(&db).await.unwrap();
    LibraryStore::from_connection(db)
}

/// Fixed heartbeat measurements.
pub(super) struct FixedSampler;

impl HeartbeatSource for FixedSampler {
    fn sample(&mut self) -> NodeHeartbeat {
        NodeHeartbeat {
            incarnation: "host-inc".into(),
            started_at: "2026-10-17T00:00:00Z".into(),
            interval: Duration::from_secs(15),
            cpu_millicores: 4000,
            memory_bytes: Some(8 << 30),
            cpu_used_millicores: Some(100),
            memory_available_bytes: Some(4 << 30),
            files_dir_free_bytes: Some(1 << 40),
            cache: None,
            public_origin: None,
        }
    }
}

/// Fast timers for tests.
pub(super) fn timing(backoff_min: Duration) -> ClientTiming {
    ClientTiming {
        heartbeat_interval: Duration::from_millis(500),
        backoff_min,
        backoff_max: backoff_min * 2,
        request_timeout: Duration::from_secs(10),
        ..ClientTiming::default()
    }
}

/// One run of the stand-in store plugin.
#[derive(Debug, Clone)]
struct Run {
    /// Job id.
    job_id: String,
    /// Lease generation on the fence.
    fence: i64,
    /// The token it was handed.
    token: String,
    /// Whether it saw the cancel flag before it returned.
    cancelled: bool,
    /// The guest's `CONFIG` (JSON text).
    config: String,
    /// The guest's `SECRETS` (JSON text; empty when withheld).
    secrets: String,
    /// Account ids of the store credentials the guest got.
    credential_accounts: Vec<String>,
}

/// A stand-in for the Echo store plugin behind the daemon's job transport:
/// it reads the handed token from [`JobExecCtx::host`], optionally writes a
/// refreshed one back, and optionally blocks until released or cancelled.
#[derive(Clone, Default)]
struct EchoTransport {
    /// Token to write back, if any.
    refresh_to: Option<&'static str>,
    /// Block until [`Self::release`] or cancellation.
    block: bool,
    /// A cancelled blocked run stops cooperatively and returns `Ok` with
    /// what it had, instead of an error.
    succeed_on_cancel: bool,
    /// Lets blocked runs finish.
    release: Arc<AtomicBool>,
    /// Runs so far.
    runs: Arc<Mutex<Vec<Run>>>,
}

impl EchoTransport {
    /// Runs recorded so far.
    fn runs(&self) -> Vec<Run> {
        self.runs.lock().unwrap().clone()
    }
}

impl JobTransport for EchoTransport {
    async fn execute(&self, cmd: JobCommand, ctx: JobExecCtx) -> anyhow::Result<String> {
        assert_eq!(cmd.kind, JobKind::Scan);
        let host = ctx.host.clone().expect("a host job carries its handoff");
        assert_eq!(
            ctx.fence.owner,
            format!("host:{}", host.job.node_id),
            "the fence is the owner's remote fence"
        );
        let token_ref = host.secrets.refs().next().cloned().expect("a handed token");
        let token =
            String::from_utf8(host.secrets.get(&token_ref).unwrap().expose().to_vec()).unwrap();
        let bindings = ctx
            .host_bindings
            .clone()
            .expect("a host job carries its guest bindings");
        assert_eq!(bindings.plugin_key, PLUGIN);
        assert_eq!(bindings.grant.plugin_key, PLUGIN);
        assert!(bindings.root.join("plugin.toml").is_file(), "materialized");
        let text = |payload: &[u8]| String::from_utf8(payload.to_vec()).unwrap();
        let index = {
            let mut runs = self.runs.lock().unwrap();
            runs.push(Run {
                job_id: host.job.job.id.clone(),
                fence: host.job.fence,
                token,
                cancelled: false,
                config: text(&bindings.open.bindings.config.payload),
                secrets: text(&bindings.open.bindings.secrets.payload),
                credential_accounts: bindings
                    .credentials
                    .iter()
                    .map(|credential| credential.account_id.clone())
                    .collect(),
            });
            runs.len() - 1
        };
        if let Some(refreshed) = self.refresh_to {
            host.write_back(&token_ref, refreshed.as_bytes())
                .await
                .map_err(|err| anyhow::anyhow!("write-back: {err}"))?;
            let other = SecretRef {
                name: "not-handed".into(),
                ..token_ref.clone()
            };
            assert!(
                host.write_back(&other, b"x").await.is_err(),
                "only handed refs are written back"
            );
        }
        if self.block {
            loop {
                if ctx.is_cancelled() {
                    self.runs.lock().unwrap()[index].cancelled = true;
                    if self.succeed_on_cancel {
                        break;
                    }
                    anyhow::bail!("cancelled");
                }
                if self.release.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Ok(format!(
            "echo scanned {}",
            cmd.payload.account.unwrap_or_default()
        ))
    }
}

/// The owner side: a member with a linked store account, its sealed token,
/// and a queued scan of it. Answers the member id and the token's ref.
pub(super) async fn owner_account(store: &LibraryStore, token: &[u8]) -> (i64, SecretRef) {
    owner_account_as(store, token, UserRole::Member).await
}

/// [`owner_account`] for a user with `role`.
async fn owner_account_as(store: &LibraryStore, token: &[u8], role: UserRole) -> (i64, SecretRef) {
    let member = store.create_user(role, Some("Riley"), None).await.unwrap();
    store
        .upsert_account(PLUGIN, "riley-acct", "us", None, true, "test")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(member.id, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, "riley-acct", "test")
        .await
        .unwrap();
    let token_ref = SecretRef {
        kind: secret_kind::SOURCE_AUTH.into(),
        provider: PLUGIN.into(),
        account_id: "riley-acct".into(),
        name: "riley-acct.auth".into(),
    };
    let record = build_sealed_record(
        token,
        &token_ref.kind,
        &token_ref.provider,
        secret_account_type::INTEGRATION,
        &token_ref.account_id,
        &token_ref.name,
    )
    .unwrap();
    upsert_secret(store.db(), &record).await.unwrap();
    (member.id, token_ref)
}

/// Queues a scan of the member's account.
async fn enqueue_scan(store: &LibraryStore) -> String {
    match store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Scan,
            payload: JobPayload {
                account: Some("riley-acct".into()),
                ..Default::default()
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 256,
            run_after: None,
        })
        .await
        .unwrap()
    {
        EnqueueOutcome::Created { id } => id,
        other => panic!("expected a new job: {other:?}"),
    }
}

/// The stored value of `secret` on the owner.
async fn stored(store: &LibraryStore, secret: &SecretRef) -> String {
    let record = get_secret(
        store.db(),
        &secret.kind,
        Some(&secret.provider),
        secret_account_type::INTEGRATION,
        Some(&secret.account_id),
        &secret.name,
    )
    .await
    .unwrap()
    .unwrap();
    String::from_utf8(unseal_secret(&record).unwrap()).unwrap()
}

/// Sets the owner's public origin and answers the origin its links carry
/// (and its handshake verifies): the effective one, loopback spelled
/// `localhost`.
pub(super) async fn public_origin(state: &Arc<crate::api::AppState>, origin: &str) -> String {
    let mut cfg = state.config.write().await;
    cfg.integrations.public_origin = Some(origin.to_string());
    crate::origin::effective_origin_from_config(&cfg, None)
}

/// Mints a link for `member` at `origin` and joins it from the host.
pub(super) async fn join(
    owner: &LibraryStore,
    host: &LibraryStore,
    member: i64,
    origin: &str,
    pin: Option<&str>,
) -> Result<LibraryLink, JoinError> {
    let minted = mint_enrollment_link(
        owner,
        &EnrollmentActor::Operator { id: "test".into() },
        member,
        "",
        None,
    )
    .await
    .unwrap();
    join_library(host, &minted.url(origin, pin), "1.0.0").await
}

/// Starts a host client for `link` running `transport` (claiming scans),
/// with its link supervisor keeping the catalog and consent current.
async fn start_client(
    host: &LibraryStore,
    host_files: &std::path::Path,
    link: LibraryLink,
    transport: EchoTransport,
    backoff_min: Duration,
) -> (HostClientHandle, watch::Sender<bool>) {
    start_host_client(host, host_files, link, transport, &["scan"], backoff_min).await
}

/// [`start_client`] over any transport, claiming `kinds`.
async fn start_host_client<T: JobTransport + 'static>(
    host: &LibraryStore,
    host_files: &std::path::Path,
    link: LibraryLink,
    transport: T,
    kinds: &[&str],
    backoff_min: Duration,
) -> (HostClientHandle, watch::Sender<bool>) {
    let keys = load_library_link_keys(host, &link).await.unwrap();
    let catalog = Arc::new(RemoteCatalog::default());
    let (client, handle) = HostClient::new(
        HostClientConfig {
            link,
            keys,
            software_version: "1.0.0".into(),
            incarnation: uuid::Uuid::new_v4().to_string(),
            max_jobs: 1,
            claim_kinds: kinds.iter().map(|kind| (*kind).to_string()).collect(),
            claims_after_refresh: true,
            timing: timing(backoff_min),
            share: None,
        },
        TransportRunner::new(transport, Arc::clone(&catalog)),
        Box::new(FixedSampler),
    )
    .unwrap();
    let (stop, shutdown) = watch::channel(false);
    tokio::spawn(
        LinkSupervisor {
            library: host.clone(),
            files_dir: host_files.to_path_buf(),
            handle: handle.clone(),
            catalog,
            platform_payloads: Default::default(),
        }
        .run(shutdown.clone()),
    );
    tokio::spawn(client.run(shutdown));
    (handle, stop)
}

/// Instance secret of the owner's store plugin.
const INSTANCE_SECRET: &str = "store-instance-secret-v1";

/// `plugin.toml` of the owner's store plugin: a `mode` setting and a
/// `token` secret.
const STORE_MANIFEST: &str = r#"api_version = 3
id = "store"
version = "0.1.0"
runtime = "native"
command = "./echo"
entrypoints = ["storefront"]
media = { works = ["book"], lists = ["audio"], fetches = ["audio"] }

[capabilities.network]
mode = "deny"

[vars]

[[settings]]
key = "mode"
label = "Mode"
type = "enum"
values = [{ id = "web", label = "Web" }, { id = "zip", label = "ZIP" }]
default = "web"

[secrets]

[[settings]]
key = "token"
label = "Token"
type = "secret"
"#;

/// The owner's store plugin, installed, configured and deployed to a host.
struct Deployed {
    /// Its default instance.
    instance_id: PluginInstanceId,
    /// The deployment to the host.
    deployment_id: String,
    /// The version's settings schema.
    schema: bookclerk_plugin_abi::SettingsSchema,
}

/// Installs the store plugin on the owner (archive stored in the library,
/// grant approved by the library's operator), sets its instance to
/// `mode = zip` with a sealed `token`, and deploys it to `branch_node`.
async fn deploy_store(
    owner: &LibraryStore,
    owner_files: &std::path::Path,
    branch_node: &str,
) -> Deployed {
    let stage = owner_files.join("store-stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("plugin.toml"), STORE_MANIFEST).unwrap();
    std::fs::write(stage.join("echo"), b"#!/bin/sh\necho ok\n").unwrap();
    let archive = owner_files.join("store.tar.gz");
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .args(["plugin.toml", "echo"])
        .status()
        .unwrap();
    assert!(status.success());
    let package = bookclerk_plugin_catalog::BookclerkPackageManifest {
        schema_version: 1,
        protocol: None,
        api_version: 1,
        api_version_max: None,
        min_bookclerk: None,
        kind: bookclerk_plugin_catalog::PluginKind::Source,
        id: "store".into(),
        display_name: None,
        description: None,
        coordinate: None,
        artifacts: vec![bookclerk_plugin_catalog::ArtifactTarget {
            target: bookclerk_plugin_catalog::host_bookclerk_target().to_string(),
            url: "file://store.tar.gz".into(),
            archive_sha256: "ab".repeat(32),
            archive_root: ".".into(),
            executable: "echo".into(),
            executable_sha256: None,
        }],
        sandbox: Default::default(),
        links: Default::default(),
        yanked: false,
        released_at: None,
        publisher: None,
    };
    let grant = PluginGrant {
        schema_version: bookclerk_plugin_host::GRANT_SCHEMA_VERSION,
        plugin_key: PLUGIN.into(),
        plugin_id: "store".into(),
        bindings: ["config".to_string(), "secrets".to_string()].into(),
        network_mode: "deny".into(),
        approved_at: "2026-10-01T00:00:00Z".into(),
        ..PluginGrant::empty()
    };
    let actor = ConfigActor::Operator { id: "test".into() };
    let owner_node = load_or_create_node_identity(owner_files).unwrap().node_id;
    let added = bookclerk_plugin_host::add_package_version(
        owner,
        &actor,
        owner_files,
        &owner_node,
        bookclerk_plugin_host::VersionRequest {
            plugin_key: PLUGIN.into(),
            package,
            archive: Some(archive),
            target: None,
            coordinate: String::new(),
            offline: true,
            publisher_verified: false,
            grant: Some(grant),
            activate: bookclerk_plugin_host::Activate::IfNone,
        },
    )
    .await
    .unwrap();
    assert!(
        added.version.artifacts[0].url.starts_with("blob:sha256:"),
        "the archive is stored in the owner's library"
    );
    let instance = bookclerk_plugin_host::enroll_version(owner, &actor, &added.version)
        .await
        .unwrap();
    seal_instance_secret(owner, &instance.id, PLUGIN, "token", INSTANCE_SECRET)
        .await
        .unwrap();
    let schema = added.version.settings.clone();
    set_store_mode(owner, &instance.id, &schema, "zip").await;
    let deployment = ensure_plugin_deployment(
        owner,
        &actor,
        &instance.id,
        branch_node,
        DesiredState::Present,
    )
    .await
    .unwrap();
    Deployed {
        instance_id: instance.id,
        deployment_id: deployment.deployment_id,
        schema,
    }
}

/// Sets the store instance's `mode` (keeping its `token`); answers the new
/// document revision.
async fn set_store_mode(
    owner: &LibraryStore,
    instance_id: &PluginInstanceId,
    schema: &bookclerk_plugin_abi::SettingsSchema,
    mode: &str,
) -> i64 {
    let current = load_instance_config(owner, instance_id).await.unwrap();
    let mut body = PluginInstanceConfigV1::empty();
    body.settings
        .insert("mode".into(), SettingValue::String(mode.into()));
    set_secret_ref(&mut body, "token");
    match replace_instance_config(
        owner,
        &ConfigActor::Operator { id: "test".into() },
        instance_id,
        schema,
        current.revision,
        &body,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .unwrap()
    {
        bookclerk_library::control_plane::InstanceConfigReplace::Applied(document) => {
            document.revision
        }
        other => panic!("config not replaced: {other:?}"),
    }
}

/// Approves the owner's store plugin on the host once its request is
/// recorded, as `bookclerk libraries approve` does.
async fn approve_store(host: &LibraryStore, library_id: &str) {
    eventually("the owner's request to be recorded on the host", || async {
        load_remote_plugin_consent(host, library_id, PLUGIN)
            .await
            .unwrap()
            .is_some()
    })
    .await;
    let reviewed = load_remote_plugin_consent(host, library_id, PLUGIN)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.requested_version, "0.1.0");
    approve_remote_request(
        host,
        library_id,
        PLUGIN,
        &reviewed.request_digest(),
        &RemoteNarrowing::default(),
        RemoteApprover::Operator("test"),
    )
    .await
    .unwrap();
    notify_consent_changed();
}

/// Delivers what is queued for the host's node queue (no event worker
/// runs in these tests).
async fn deliver_wakes(owner: &LibraryStore, node_id: &str) {
    for event in owner
        .list_domain_events_for_queue(&node_queue_name(node_id), 50)
        .await
        .unwrap()
    {
        owner
            .dispatch_catalog_matches(&event, &uuid::Uuid::new_v4().to_string())
            .await
            .unwrap();
    }
}

/// The owner's record of the host's observation of `deployed`.
async fn observed(
    owner: &LibraryStore,
    deployed: &Deployed,
    node_id: &str,
) -> Option<bookclerk_library::control_plane::DeploymentObservation> {
    load_observation(owner, &deployed.deployment_id, node_id)
        .await
        .unwrap()
}

/// Polls `check` until it holds, for up to 20 s.
pub(super) async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until the client has been welcomed `count` times.
pub(super) async fn connected(handle: &HostClientHandle, count: u64) {
    let mut connections = handle.connections();
    tokio::time::timeout(
        Duration::from_secs(20),
        connections.wait_for(|seen| *seen >= count),
    )
    .await
    .expect("the host connected")
    .unwrap();
}

/// The owner's job row.
async fn job(store: &LibraryStore, id: &str) -> bookclerk_library::JobRecord {
    store.get_job(id).await.unwrap().unwrap()
}

/// Log output captured in memory (tracing events and bridged `log`
/// records, the way the daemon's subscriber sees them).
#[derive(Clone, Default)]
pub(super) struct Captured(pub(super) Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A member's host enrolls over loopback, connects with its sealed keys,
/// heartbeats into the owner's `node_status`, claims the member's scan,
/// runs it through the daemon's job transport with the token opened from
/// the sealed bundle, writes a refreshed token back, completes under the
/// fence, and acks a wake; at TRACE, with tungstenite's frame tracing
/// bridged in, no token reaches the logs, plain or base64.
#[tokio::test]
async fn a_host_runs_its_owners_scan_end_to_end() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let _ = tracing_log::LogTracer::init();

    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let host = host_store(host_files.path()).await;
    let (member, token_ref) = owner_account(&owner, b"riley-token-v1").await;

    let link = join(&owner, &host, member, &origin, None).await.unwrap();
    assert_eq!(
        link.owner_origin,
        format!("http://localhost:{}", addr.port())
    );
    assert_eq!(
        load_library_link(&host, &link.parent_library_id)
            .await
            .unwrap()
            .as_ref(),
        Some(&link)
    );
    let node_id = link.remote_node_id.clone();
    let library_id = link.parent_library_id.clone();
    deploy_store(&owner, owner_files.path(), &node_id).await;
    let transport = EchoTransport {
        refresh_to: Some("riley-token-v2"),
        ..EchoTransport::default()
    };
    let (handle, _stop) = start_client(
        &host,
        host_files.path(),
        link,
        transport.clone(),
        Duration::from_millis(200),
    )
    .await;
    connected(&handle, 1).await;
    approve_store(&host, &library_id).await;
    eventually("the host's heartbeat", || async {
        load_node_status(&owner, &node_id)
            .await
            .unwrap()
            .is_some_and(|status| status.cpu_millicores == 4000)
    })
    .await;

    let job_id = enqueue_scan(&owner).await;
    eventually("the scan to finish on the host", || async {
        job(&owner, &job_id).await.state == JobState::Succeeded
    })
    .await;
    let done = job(&owner, &job_id).await;
    assert_eq!(done.progress.as_deref(), Some("echo scanned riley-acct"));
    let runs = transport.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].job_id, job_id);
    assert_eq!(runs[0].token, "riley-token-v1");
    assert_eq!(runs[0].credential_accounts, vec!["riley-acct".to_string()]);
    assert!(runs[0].config.contains("zip"), "{}", runs[0].config);
    assert!(
        runs[0].secrets.is_empty(),
        "a member's host is not handed the instance's secrets"
    );
    assert_eq!(stored(&owner, &token_ref).await, "riley-token-v2");
    // The owner commits before its `ok` reaches the host.
    eventually("the reported fence to be released", || async {
        handle.held_fences().is_empty()
    })
    .await;

    // A wake on the host's node queue reaches subscribers and is acked.
    let mut wakes = handle.subscribe_wakes();
    let queue = node_queue_name(&node_id);
    let consumer = format!("node:{node_id}");
    eventually("the owner to consume the host's queue", || async {
        owner
            .list_live_event_subscribers()
            .await
            .unwrap()
            .iter()
            .any(|row| row.enabled && row.plugin_id == consumer)
    })
    .await;
    publish_node_message(
        &owner,
        &node_id,
        &NodeMessage::DeploymentChanged {
            deployment_id: "dep-1".into(),
            plugin_instance_id: "inst-1".into(),
            desired: "present".into(),
            revision: 1,
        },
    )
    .await
    .unwrap();
    for event in owner
        .list_domain_events_for_queue(&queue, 10)
        .await
        .unwrap()
    {
        owner
            .dispatch_catalog_matches(&event, &uuid::Uuid::new_v4().to_string())
            .await
            .unwrap();
    }
    let wake = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let wake = wakes.recv().await.unwrap();
            if wake["deploymentId"] == "dep-1" {
                return wake;
            }
        }
    })
    .await
    .expect("the wake arrived");
    assert_eq!(wake["type"], "deployment.changed");
    eventually("the wake to be acked", || async {
        !owner
            .list_event_deliveries_for_queue(&queue, Some("acked"), 10)
            .await
            .unwrap()
            .is_empty()
    })
    .await;

    let logs = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(
        logs.contains("claimed a job from the owner"),
        "the capture sees the client's logs"
    );
    assert!(
        logs.contains("Received message"),
        "the capture sees the WebSocket library's frame tracing"
    );
    for value in ["riley-token-v1", "riley-token-v2", INSTANCE_SECRET] {
        assert!(!logs.contains(value), "{value} reached the logs");
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
        assert!(
            !logs.contains(&encoded),
            "{value} reached the logs as base64"
        );
    }
}

/// A fence still held when the connection drops is resumed on reconnect
/// and the job finishes under it; a fence reclaimed while the host was
/// away is lost on reconnect, its run is cancelled and its result
/// discarded.
#[tokio::test]
async fn a_reconnecting_host_resumes_live_fences_and_drops_lost_ones() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    state.config.write().await.jobs.lease_seconds = 3;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let host = host_store(host_files.path()).await;
    let (member, _token) = owner_account(&owner, b"riley-token-v1").await;
    let link = join(&owner, &host, member, &origin, None).await.unwrap();
    let node_id = link.remote_node_id.clone();
    let library_id = link.parent_library_id.clone();
    deploy_store(&owner, owner_files.path(), &node_id).await;
    let transport = EchoTransport {
        block: true,
        ..EchoTransport::default()
    };
    let (handle, _stop) = start_client(
        &host,
        host_files.path(),
        link,
        transport.clone(),
        Duration::from_millis(1500),
    )
    .await;
    connected(&handle, 1).await;
    approve_store(&host, &library_id).await;

    // Phase 1: the connection drops mid-job; the hello resumes the fence.
    let first = enqueue_scan(&owner).await;
    eventually("the host to start the job", || async {
        !transport.runs().is_empty()
    })
    .await;
    let fence = transport.runs()[0].fence;
    crate::node_api::ws::close_node_sessions(&node_id, CLOSE_SUPERSEDED);
    connected(&handle, 2).await;
    assert_eq!(handle.held_fences().len(), 1, "the fence survived");
    // Renewals carry on past the 3 s lease under the resumed fence.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let running = job(&owner, &first).await;
    assert_eq!(running.state, JobState::Running);
    assert_eq!(running.lease_generation, fence);
    transport.release.store(true, Ordering::SeqCst);
    eventually("the resumed job to finish", || async {
        job(&owner, &first).await.state == JobState::Succeeded
    })
    .await;
    assert_eq!(job(&owner, &first).await.lease_generation, fence);

    // Phase 2: while the host is away the lease expires and the owner
    // reclaims it; the reconnecting host learns the fence is lost.
    transport.release.store(false, Ordering::SeqCst);
    let second = enqueue_scan(&owner).await;
    eventually("the host to start the second job", || async {
        transport.runs().len() == 2
    })
    .await;
    let lost_fence = transport.runs()[1].fence;
    // The owner's node API goes away (as when its certificate lapses), so
    // the host cannot reconnect until the lease has expired.
    state.config.write().await.daemon.node_api.mode = bookclerk_config::NodeApiMode::Acme;
    crate::node_api::ws::close_node_sessions(&node_id, CLOSE_SUPERSEDED);
    eventually("the host to notice the disconnect", || async {
        !handle.is_connected()
    })
    .await;
    eventually("the owner to reclaim the expired lease", || async {
        owner.reclaim_expired_leases().await.unwrap() == 1
    })
    .await;
    assert_eq!(handle.connections().borrow().to_owned(), 2);
    state.config.write().await.daemon.node_api.mode = bookclerk_config::NodeApiMode::Loopback;
    connected(&handle, 3).await;
    eventually("the lost run to be cancelled", || async {
        transport.runs()[1].cancelled
    })
    .await;
    let after = job(&owner, &second).await;
    assert_ne!(
        (after.state, after.lease_generation),
        (JobState::Succeeded, lost_fence),
        "nothing completed under the lost fence"
    );
    assert!(handle
        .held_fences()
        .iter()
        .all(|held| held.id != second || held.fence != lost_fence));
}

/// A link pinned to a certificate the owner does not serve is refused in
/// the TLS handshake: enrollment never reaches the owner and the socket
/// never opens. The right pin enrolls and connects over TLS.
#[tokio::test]
async fn a_wrong_spki_pin_is_refused() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    // The node API serves only the origin its TLS listener started for, so
    // the port is picked before the listener starts. The certificate names
    // `localhost`.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let origin = format!("https://localhost:{port}");
    let (state, owner) = crate::node_api::tests::test_state_with(
        &owner_files,
        &format!(
            "[integrations]\npublic_origin = \"{origin}\"\n\n\
             [daemon.node_api]\nmode = \"self_signed\"\ntls_listen = \"127.0.0.1:{port}\"\n"
        ),
    )
    .await;
    let cfg = state.config.read().await.clone();
    let started = crate::node_api::tls::start(&state, &cfg, owner_files.path()).await;
    let tls = started.local.expect("the TLS listener is bound");
    assert_eq!(tls.port(), port);
    let pin = bookclerk_config::read_node_api_spki_pin(owner_files.path()).unwrap();
    let wrong = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
    let host = host_store(host_files.path()).await;
    let (member, _token) = owner_account(&owner, b"riley-token-v1").await;

    // Wrong pin: refused before the link is spent.
    let refused = join(&owner, &host, member, &origin, Some(&wrong)).await;
    assert!(
        matches!(refused, Err(JoinError::Dial(DialError::Tls(_)))),
        "{refused:?}"
    );
    let links = bookclerk_library::control_plane::list_enrollment_links(
        &owner,
        &EnrollmentActor::Operator { id: "test".into() },
    )
    .await
    .unwrap();
    assert!(links.iter().all(|link| link.redeemed_at.is_none()));
    let endpoint = OwnerEndpoint::new(&origin, Some(&wrong)).unwrap();
    assert!(matches!(
        endpoint.connect_socket().await,
        Err(DialError::Tls(_))
    ));
    // Unpinned, the self-signed certificate does not chain to a webpki root.
    let unpinned = OwnerEndpoint::new(&origin, None).unwrap();
    assert!(matches!(
        unpinned.connect_socket().await,
        Err(DialError::Tls(_))
    ));

    // The right pin enrolls and connects over TLS.
    let link = join(&owner, &host, member, &origin, Some(&pin))
        .await
        .unwrap();
    assert_eq!(link.spki_pin.as_deref(), Some(pin.as_str()));
    let (handle, _stop) = start_client(
        &host,
        host_files.path(),
        link,
        EchoTransport::default(),
        Duration::from_millis(200),
    )
    .await;
    connected(&handle, 1).await;
}

/// An owner plugin deployed to an Administrator's host is recorded as a
/// request, reported `awaiting_host_consent`, and its queued scan is never
/// claimed; its archive is served only on a signed request. Once the
/// host's operator approves it, the archive comes over the artifact
/// endpoint, `CONFIG` and `SECRETS` over `instance.config`, and the scan
/// runs with both in its bindings; a `configuration.changed` wake makes
/// the host fetch the new revision. At TRACE, no secret reaches the logs.
#[tokio::test]
async fn an_owner_plugin_runs_only_after_host_consent() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let _ = tracing_log::LogTracer::init();

    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let host = host_store(host_files.path()).await;
    let (admin, _token) =
        owner_account_as(&owner, b"riley-token-v1", UserRole::Administrator).await;
    let link = join(&owner, &host, admin, &origin, None).await.unwrap();
    let node_id = link.remote_node_id.clone();
    let library_id = link.parent_library_id.clone();
    let owner_origin = link.owner_origin.clone();
    let deployed = deploy_store(&owner, owner_files.path(), &node_id).await;
    let archive_sha = bookclerk_library::control_plane::load_active_version(&owner, PLUGIN)
        .await
        .unwrap()
        .unwrap()
        .artifacts[0]
        .archive_sha256
        .clone();
    let first = enqueue_scan(&owner).await;
    let transport = EchoTransport::default();
    let (handle, _stop) = start_client(
        &host,
        host_files.path(),
        link,
        transport.clone(),
        Duration::from_millis(200),
    )
    .await;
    connected(&handle, 1).await;

    // Unconsented: recorded, reported, never claimed.
    eventually("the host to report it awaits consent", || async {
        observed(&owner, &deployed, &node_id)
            .await
            .is_some_and(|seen| {
                seen.status == DeploymentStatus::Error && seen.detail == "awaiting_host_consent"
            })
    })
    .await;
    let request = load_remote_plugin_consent(&host, &library_id, PLUGIN)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.requested_version, "0.1.0");
    assert!(!request.is_approved());
    assert!(handle.consented_plugins().is_empty());
    assert!(
        bookclerk_library::control_plane::load_artifact_blob(&host, &archive_sha)
            .await
            .unwrap()
            .is_none(),
        "nothing is fetched before consent"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(job(&owner, &first).await.state, JobState::Pending);
    assert!(
        transport.runs().is_empty(),
        "an unconsented plugin never runs"
    );

    // The artifact endpoint serves only signed requests for archives
    // deployed to the node.
    assert!(handle.fetch_artifact(&"cd".repeat(32)).await.is_err());
    let endpoint = OwnerEndpoint::new(&owner_origin, None).unwrap();
    let path = format!("{ARTIFACTS_PATH}{archive_sha}");
    let forged = format!(
        "Bookclerk-Node {node_id} {} AAAA",
        chrono::Utc::now().timestamp()
    );
    for authorization in ["", "Bookclerk-Node", forged.as_str()] {
        let (status, body) = endpoint
            .get_authorized(&path, authorization, 4096)
            .await
            .unwrap();
        assert_eq!(status, 404, "{authorization}");
        assert!(!body.starts_with(&[0x1f, 0x8b]), "no archive bytes");
    }

    // Approved: fetched, configured, claimed, run with its bindings.
    approve_store(&host, &library_id).await;
    eventually("the scan to finish on the host", || async {
        job(&owner, &first).await.state == JobState::Succeeded
    })
    .await;
    let runs = transport.runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].config.contains("zip"), "{}", runs[0].config);
    let secrets: serde_json::Value = serde_json::from_str(&runs[0].secrets).unwrap();
    assert_eq!(secrets["token"], INSTANCE_SECRET);
    assert_eq!(runs[0].credential_accounts, vec!["riley-acct".to_string()]);
    assert!(
        bookclerk_library::control_plane::load_artifact_blob(&host, &archive_sha)
            .await
            .unwrap()
            .is_some(),
        "the archive came over the artifact endpoint"
    );
    let revision = load_instance_config(&owner, &deployed.instance_id)
        .await
        .unwrap()
        .revision;
    eventually("the host to report it materialized", || async {
        observed(&owner, &deployed, &node_id)
            .await
            .is_some_and(|seen| {
                seen.status == DeploymentStatus::Materialized
                    && seen.applied_config_revision == Some(revision)
            })
    })
    .await;

    // A configuration change wakes the host, which fetches the revision.
    let changed = set_store_mode(&owner, &deployed.instance_id, &deployed.schema, "web").await;
    assert!(changed > revision);
    eventually("the owner to consume the host's queue", || async {
        let consumer = format!("node:{node_id}");
        owner
            .list_live_event_subscribers()
            .await
            .unwrap()
            .iter()
            .any(|row| row.enabled && row.plugin_id == consumer)
    })
    .await;
    deliver_wakes(&owner, &node_id).await;
    eventually("the host to apply the new revision", || async {
        observed(&owner, &deployed, &node_id)
            .await
            .is_some_and(|seen| seen.applied_config_revision == Some(changed))
    })
    .await;
    let second = enqueue_scan(&owner).await;
    eventually("the second scan to finish", || async {
        job(&owner, &second).await.state == JobState::Succeeded
    })
    .await;
    assert!(transport.runs()[1].config.contains("web"));

    let logs = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(logs.contains("Received message"), "the capture sees frames");
    for value in ["riley-token-v1", INSTANCE_SECRET] {
        assert!(!logs.contains(value), "{value} reached the logs");
        for engine in [
            base64::engine::general_purpose::URL_SAFE_NO_PAD,
            base64::engine::general_purpose::STANDARD,
        ] {
            let encoded = engine.encode(value);
            assert!(
                !logs.contains(&encoded),
                "{value} reached the logs as base64"
            );
        }
    }
}

/// A consented plugin whose archive could not be fetched (the owner's
/// copy was gone for a moment) is retried with backoff and becomes
/// claimable once the archive is back, with no wake or consent change.
#[tokio::test]
async fn a_transient_fetch_failure_is_retried() {
    use sea_orm::ConnectionTrait as _;
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let host = host_store(host_files.path()).await;
    let (member, _token) = owner_account(&owner, b"riley-token-v1").await;
    let link = join(&owner, &host, member, &origin, None).await.unwrap();
    let node_id = link.remote_node_id.clone();
    let library_id = link.parent_library_id.clone();
    let deployed = deploy_store(&owner, owner_files.path(), &node_id).await;
    let archive_sha = bookclerk_library::control_plane::load_active_version(&owner, PLUGIN)
        .await
        .unwrap()
        .unwrap()
        .artifacts[0]
        .archive_sha256
        .clone();
    let archive = bookclerk_library::control_plane::load_artifact_blob(&owner, &archive_sha)
        .await
        .unwrap()
        .unwrap();
    owner
        .db()
        .execute_unprepared("DELETE FROM plugin_artifacts")
        .await
        .unwrap();
    let transport = EchoTransport::default();
    let (handle, _stop) = start_client(
        &host,
        host_files.path(),
        link,
        transport.clone(),
        Duration::from_millis(200),
    )
    .await;
    connected(&handle, 1).await;
    approve_store(&host, &library_id).await;
    eventually("the host to report the failed fetch", || async {
        observed(&owner, &deployed, &node_id)
            .await
            .is_some_and(|seen| {
                seen.status == DeploymentStatus::Error
                    && seen.detail.contains("could not be fetched")
            })
    })
    .await;
    assert!(handle.consented_plugins().is_empty());

    bookclerk_library::control_plane::put_artifact_blob(&owner, &archive)
        .await
        .unwrap();
    eventually("the retried fetch to make the plugin claimable", || async {
        handle.consented_plugins().contains(PLUGIN)
    })
    .await;
    let job_id = enqueue_scan(&owner).await;
    eventually("the scan to finish on the host", || async {
        job(&owner, &job_id).await.state == JobState::Succeeded
    })
    .await;
    assert!(observed(&owner, &deployed, &node_id)
        .await
        .is_some_and(|seen| seen.status == DeploymentStatus::Materialized));
}

/// Revoking consent while the host runs a job (as `bookclerk libraries
/// revoke` does, with no notification) cancels the run and gives the job
/// back unspent; nothing of the plugin is claimed again, and the host
/// reports it awaits consent.
#[tokio::test]
async fn revoking_consent_mid_job_gives_the_job_back() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let host = host_store(host_files.path()).await;
    let (member, _token) = owner_account(&owner, b"riley-token-v1").await;
    let link = join(&owner, &host, member, &origin, None).await.unwrap();
    let node_id = link.remote_node_id.clone();
    let library_id = link.parent_library_id.clone();
    let deployed = deploy_store(&owner, owner_files.path(), &node_id).await;
    let transport = EchoTransport {
        block: true,
        ..EchoTransport::default()
    };
    let (handle, _stop) = start_client(
        &host,
        host_files.path(),
        link,
        transport.clone(),
        Duration::from_millis(200),
    )
    .await;
    connected(&handle, 1).await;
    approve_store(&host, &library_id).await;
    let job_id = enqueue_scan(&owner).await;
    eventually("the host to start the job", || async {
        !transport.runs().is_empty()
    })
    .await;
    assert_eq!(job(&owner, &job_id).await.state, JobState::Running);

    revoke_remote_plugin(&host, &library_id, PLUGIN)
        .await
        .unwrap()
        .unwrap();
    eventually("the run to be cancelled", || async {
        transport.runs()[0].cancelled
    })
    .await;
    eventually("the job to be given back", || async {
        job(&owner, &job_id).await.state == JobState::Pending && handle.held_fences().is_empty()
    })
    .await;
    let back = job(&owner, &job_id).await;
    assert_eq!(back.attempt_count, 0, "a given-back job is not spent");
    assert!(back.lease_owner.is_none());
    eventually("the host to report it awaits consent again", || async {
        observed(&owner, &deployed, &node_id)
            .await
            .is_some_and(|seen| seen.detail == "awaiting_host_consent")
    })
    .await;
    assert!(handle.consented_plugins().is_empty());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(transport.runs().len(), 1, "not claimed again");
    assert_eq!(job(&owner, &job_id).await.state, JobState::Pending);
}

/// An operator's cancel stops a host run that then returns what it had
/// so far: the run ends cancelled, never as a success.
#[tokio::test]
async fn an_operator_cancel_is_never_reported_as_success() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    // Renewals (which carry the cancel) every second.
    state.config.write().await.jobs.lease_seconds = 3;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let host = host_store(host_files.path()).await;
    let (member, _token) = owner_account(&owner, b"riley-token-v1").await;
    let link = join(&owner, &host, member, &origin, None).await.unwrap();
    let node_id = link.remote_node_id.clone();
    let library_id = link.parent_library_id.clone();
    deploy_store(&owner, owner_files.path(), &node_id).await;
    let transport = EchoTransport {
        block: true,
        succeed_on_cancel: true,
        ..EchoTransport::default()
    };
    let (handle, _stop) = start_client(
        &host,
        host_files.path(),
        link,
        transport.clone(),
        Duration::from_millis(200),
    )
    .await;
    connected(&handle, 1).await;
    approve_store(&host, &library_id).await;
    let job_id = enqueue_scan(&owner).await;
    eventually("the host to start the job", || async {
        !transport.runs().is_empty()
    })
    .await;
    owner.request_job_cancel(&job_id).await.unwrap();
    eventually("the job to end", || async {
        job(&owner, &job_id).await.state != JobState::Running
    })
    .await;
    assert!(transport.runs()[0].cancelled, "the run saw the cancel");
    assert_eq!(job(&owner, &job_id).await.state, JobState::Cancelled);
}

/// Step 9: a host's scan rows and acquired files go back to the owner.
mod results_and_uploads {
    use std::path::{Path, PathBuf};

    use bookclerk_library::control_plane::{
        authorize_upload, claim_remote_job, grant_upload, JobClaimLimits, RemoteClaimSettings,
        UploadDestination, UploadGrant, UploadGrantRequest,
    };
    use bookclerk_library::{JobResourceClass, NewBook};
    use bookclerk_node_proto::{Body, UploadFile};
    use bookclerk_plugin_abi::ScanBook;
    use bookclerk_plugin_host::{HandedPass, HandedScan, RefreshedCredentialSink};
    use bookclerk_source::DownloadOptions;
    use bookclerk_storage::StorageBackend;
    use sha2::Digest as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::super::host_jobs::{
        destination_outbox, AcquiredTitle, HostGuests, HostJobTransport, HostScan, HostTitle,
    };
    use super::super::remote_bindings::HostJobBindings;
    use super::*;

    /// The key an acquire names a title's audio with under `options`.
    ///
    /// # Arguments
    ///
    /// * `title` - The title's product id, title and authors.
    fn planned_key(
        (product_id, title, authors): (&str, &str, Option<&str>),
        call_number: Option<&str>,
        options: &DownloadOptions,
    ) -> String {
        let templates = options.naming_templates();
        bookclerk_acquire::storage_key(
            &bookclerk_acquire::NamingContext {
                asin: product_id.to_string(),
                title: title.to_string(),
                authors: authors.map(str::to_string),
                call_number: call_number.map(str::to_string),
                ..Default::default()
            },
            Some(templates.folder.as_str()),
            Some(templates.file.as_str()),
            "m4b",
        )
    }

    /// Title `book_uuid`'s call number in `library`, which the default
    /// naming profile writes into its file names
    /// (`Author/Title <id>/Title <id> [<call number>].m4b`).
    async fn call_number(library: &LibraryStore, book_uuid: &str) -> String {
        bookclerk_acquire::call_number_of(library, book_uuid)
            .await
            .unwrap()
            .expect("the title has an edition")
    }

    /// The owner's one `local` destination, its title scope `folder`.
    fn local_scope(folder: &str) -> Vec<UploadDestination> {
        vec![UploadDestination {
            id: "local".into(),
            title_folders: vec![folder.into()],
            title_names: None,
        }]
    }

    /// A stand-in for the owner's store guest behind the host transport:
    /// a scan answers `rows`; an acquire writes `payload` to each planned
    /// destination's outbox, named with that destination's templates.
    #[derive(Clone, Default)]
    struct StubGuests {
        /// Rows a scan returns.
        rows: Vec<ScanBook>,
        /// What successive scans return instead of `rows` (no pass reports),
        /// first to last; once empty, scans return `rows`.
        script: Arc<Mutex<std::collections::VecDeque<HandedScan>>>,
        /// The sync tokens each scan was handed.
        tokens: Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>,
        /// Bytes of every acquired file.
        payload: Arc<Vec<u8>>,
        /// This host's own local destination root (H-2), if any.
        local: Option<PathBuf>,
        /// Per-job scratch root.
        work: PathBuf,
        /// Account ids of the credentials each run was handed.
        credentials: Arc<Mutex<Vec<Vec<String>>>>,
        /// Each scan's `(import_episodes, import_plus_titles)`.
        scans: Arc<Mutex<Vec<(bool, bool)>>>,
        /// A scan never ends on its own (it ignores its cancel flag, so
        /// only the transport can stop it).
        block_scan: bool,
        /// Set once a blocked scan started.
        entered: Arc<AtomicBool>,
        /// Set when a blocked scan's call was dropped.
        dropped: Arc<AtomicBool>,
        /// The link's "Keep copies of other members' books".
        keep_member_copies: bool,
        /// Each owner's title imported into this install's library: the
        /// owner's book uuid and the `(destination, key)` files kept.
        imported: Arc<Mutex<Vec<ImportedTitle>>>,
        /// Credentials the guest hands back as refreshed during a scan,
        /// for `riley-acct` and for an account the owner did not hand.
        refresh: Option<&'static str>,
        /// A second refresh of `riley-acct` in the same scan.
        refresh_again: Option<&'static str>,
        /// What the sink said its guests hold for `riley-acct` after each
        /// refresh.
        latest_seen: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
        /// This install's own copies of accounts it keeps.
        keeper: Option<Arc<dyn super::super::host_jobs::LocalKeeper>>,
    }

    /// A [`super::super::host_jobs::LocalKeeper`] that keeps every account,
    /// fails its first `failures` saves and records each call's
    /// `(sent, refreshed)`.
    #[derive(Default)]
    struct FlakyKeeper {
        /// Saves still to fail.
        failures: Mutex<usize>,
        /// Every call's `(sent, refreshed)`.
        calls: Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
    }

    #[async_trait::async_trait]
    impl super::super::host_jobs::LocalKeeper for FlakyKeeper {
        async fn keep(
            &self,
            _plugin_key: &str,
            _account_id: &str,
            sent: &[u8],
            refreshed: &[u8],
        ) -> Option<super::super::host_jobs::KeptHere> {
            self.calls
                .lock()
                .unwrap()
                .push((sent.to_vec(), refreshed.to_vec()));
            let mut failures = self.failures.lock().unwrap();
            Some(if *failures > 0 {
                *failures -= 1;
                super::super::host_jobs::KeptHere::Failed
            } else {
                super::super::host_jobs::KeptHere::Saved
            })
        }
    }

    /// An owner's title a [`StubGuests`] imported: the owner's holding id and
    /// the `(destination, key)` files kept.
    type ImportedTitle = (String, Vec<(bookclerk_config::DestinationId, String)>);

    /// Sets its flag when dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl StubGuests {
        fn note(&self, bindings: &HostJobBindings) {
            self.credentials.lock().unwrap().push(
                bindings
                    .credentials
                    .iter()
                    .map(|credential| credential.account_id.clone())
                    .collect(),
            );
        }
    }

    impl HostGuests for StubGuests {
        async fn scan(
            &self,
            bindings: &HostJobBindings,
            refreshed: Arc<dyn RefreshedCredentialSink>,
            scan: HostScan,
            _cancel: Arc<AtomicBool>,
        ) -> anyhow::Result<HandedScan> {
            self.note(bindings);
            if let Some(credentials) = self.refresh {
                refreshed
                    .refreshed("riley-acct", credentials.as_bytes().to_vec())
                    .await;
                self.latest_seen
                    .lock()
                    .unwrap()
                    .push(refreshed.latest("riley-acct"));
                refreshed
                    .refreshed("not-handed", credentials.as_bytes().to_vec())
                    .await;
            }
            if let Some(credentials) = self.refresh_again {
                refreshed
                    .refreshed("riley-acct", credentials.as_bytes().to_vec())
                    .await;
                self.latest_seen
                    .lock()
                    .unwrap()
                    .push(refreshed.latest("riley-acct"));
            }
            self.scans
                .lock()
                .unwrap()
                .push((scan.import_episodes, scan.import_plus_titles));
            self.tokens.lock().unwrap().push(scan.sync_tokens.clone());
            if self.block_scan {
                let _flag = DropFlag(Arc::clone(&self.dropped));
                self.entered.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            let scripted = self.script.lock().unwrap().pop_front();
            Ok(scripted.unwrap_or_else(|| HandedScan {
                rows: self.rows.clone(),
                passes: Vec::new(),
            }))
        }

        async fn acquire(
            &self,
            bindings: &HostJobBindings,
            _refreshed: Arc<dyn RefreshedCredentialSink>,
            title: &HostTitle,
            outbox: &Path,
            _cancel: Arc<AtomicBool>,
        ) -> anyhow::Result<AcquiredTitle> {
            self.note(bindings);
            let mut keys = Vec::new();
            for (index, destination) in title.destinations.iter().enumerate() {
                let key = planned_key(
                    (
                        &title.book.product_id,
                        &title.book.title,
                        title.book.authors.as_deref(),
                    ),
                    title.call_number.as_deref(),
                    &destination.options,
                );
                let path = destination_outbox(outbox, index).join(&key);
                tokio::fs::create_dir_all(path.parent().unwrap()).await?;
                tokio::fs::write(&path, self.payload.as_slice()).await?;
                keys.push(vec![key]);
            }
            Ok(AcquiredTitle {
                primary_key: keys[0][0].clone(),
                pdf_key: None,
                keys,
            })
        }

        async fn keeps_member_copies(&self) -> bool {
            self.keep_member_copies
        }

        fn local_keeper(&self) -> Option<Arc<dyn super::super::host_jobs::LocalKeeper>> {
            self.keeper.clone()
        }

        async fn local_destinations(
            &self,
        ) -> anyhow::Result<Vec<super::super::host_jobs::LocalDestination>> {
            Ok(self
                .local
                .iter()
                .map(|root| {
                    (
                        bookclerk_config::DestinationId::new("local").unwrap(),
                        Box::new(bookclerk_storage::LocalFsBackend::new(root.clone()).unwrap())
                            as Box<dyn StorageBackend>,
                    )
                })
                .collect())
        }

        async fn import_owner_title(&self, copy: super::super::host_jobs::OwnerTitleCopy<'_>) {
            self.imported
                .lock()
                .unwrap()
                .push((copy.holding_id.to_owned(), copy.files.to_vec()));
        }

        fn work_dir(&self) -> PathBuf {
            self.work.clone()
        }
    }

    /// A guest scan row.
    fn row(account_id: &str, product_id: &str) -> ScanBook {
        ScanBook {
            account_id: account_id.into(),
            product_id: product_id.into(),
            title: format!("Title {product_id}"),
            work_kind: bookclerk_plugin_abi::WorkKind::Book,
            expression_kind: bookclerk_plugin_abi::ExpressionKind::Narration,
            media_kind: bookclerk_plugin_abi::MediaKind::Audio,
            ..ScanBook::default()
        }
    }

    /// An owner library row of the store plugin.
    fn owner_book(account_id: &str, product_id: &str) -> NewBook {
        NewBook {
            product_id: product_id.into(),
            plugin_key: PLUGIN.into(),
            source: "test".into(),
            account_id: account_id.into(),
            asin: None,
            isbn: None,
            marketplace: "us".into(),
            title: format!("Title {product_id}"),
            authors: Some("Author".into()),
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: "book".into(),
            categories: None,
            subtitle: None,
            published_at: None,
        }
    }

    /// Queues an acquire of the member's account.
    async fn enqueue_acquire(store: &LibraryStore) -> String {
        match store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Acquire,
                payload: JobPayload {
                    account: Some("riley-acct".into()),
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 256,
                run_after: None,
            })
            .await
            .unwrap()
        {
            EnqueueOutcome::Created { id } => id,
            other => panic!("expected a new job: {other:?}"),
        }
    }

    /// Every file under `dir`.
    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&next) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    /// An owner and a member's host linked to it, the store plugin
    /// deployed to the host.
    struct Linked {
        owner_files: tempfile::TempDir,
        host_files: tempfile::TempDir,
        state: Arc<crate::api::AppState>,
        owner: LibraryStore,
        host: LibraryStore,
        addr: SocketAddr,
        link: LibraryLink,
    }

    async fn linked() -> Linked {
        let owner_files = tempfile::tempdir().unwrap();
        let host_files = tempfile::tempdir().unwrap();
        let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
        let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
        let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
        let host = host_store(host_files.path()).await;
        let (member, _token) = owner_account(&owner, b"riley-token-v1").await;
        let link = join(&owner, &host, member, &origin, None).await.unwrap();
        deploy_store(&owner, owner_files.path(), &link.remote_node_id).await;
        Linked {
            owner_files,
            host_files,
            state,
            owner,
            host,
            addr,
            link,
        }
    }

    /// Starts the host's client over the step-9 transport with `guests`.
    async fn start(world: &Linked, guests: StubGuests) -> (HostClientHandle, watch::Sender<bool>) {
        let started = start_host_client(
            &world.host,
            world.host_files.path(),
            world.link.clone(),
            HostJobTransport::new(guests),
            &["scan", "acquire"],
            Duration::from_millis(200),
        )
        .await;
        connected(&started.0, 1).await;
        approve_store(&world.host, &world.link.parent_library_id).await;
        started
    }

    /// A member's host scans with the guest and returns its rows in fenced
    /// batches; the rows land in the owner's library under the store
    /// plugin, the guest got only the job's account's credentials and the
    /// owner's import settings (not the host's defaults), and a batch sent
    /// afterwards under the finished job's fence is refused.
    #[tokio::test]
    async fn a_host_scan_returns_rows_that_land_in_the_owners_library() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        {
            let mut cfg = world.state.config.write().await;
            cfg.library.import_episodes = false;
            cfg.library.import_plus_titles = true;
        }
        let guests = StubGuests {
            rows: vec![row("riley-acct", "p1"), row("riley-acct", "p2")],
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (handle, _stop) = start(&world, guests.clone()).await;

        let job_id = enqueue_scan(&world.owner).await;
        eventually("the host scan to finish", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        let done = job(&world.owner, &job_id).await;
        assert!(
            done.progress
                .as_deref()
                .unwrap_or("")
                .contains("2 book upsert(s)"),
            "{:?}",
            done.progress
        );
        let books = world
            .owner
            .all_acquire_titles(Some("riley-acct"))
            .await
            .unwrap();
        for product in ["p1", "p2"] {
            let book = books
                .iter()
                .find(|book| book.product_id == product)
                .unwrap_or_else(|| panic!("{product} landed in the owner's library"));
            assert_eq!(book.plugin_key, PLUGIN);
        }
        assert_eq!(
            guests.credentials.lock().unwrap().as_slice(),
            &[vec!["riley-acct".to_string()]]
        );
        assert_eq!(guests.scans.lock().unwrap().as_slice(), &[(false, true)]);

        let answer = handle
            .request(Body::ScanResults {
                job_id: job_id.clone(),
                fence: done.lease_generation,
                plugin_key: PLUGIN.into(),
                batch: 9,
                books: vec![serde_json::json!({
                    "accountId": "riley-acct",
                    "productId": "late",
                    "title": "Late"
                })],
            })
            .await
            .expect("the owner answers");
        assert!(
            matches!(answer.body, Body::Lost { .. }),
            "a stale fence's batch is refused: {answer:?}"
        );
        assert!(world
            .owner
            .all_acquire_titles(Some("riley-acct"))
            .await
            .unwrap()
            .iter()
            .all(|book| book.product_id != "late"));
    }

    /// A host scan's passes keep the owner's sync state: a full pass's
    /// token is stored and handed back with the account's next host scan,
    /// a delta pass's removals take those holdings, and a complete full
    /// pass prunes the holdings its rows did not list.
    #[tokio::test]
    async fn host_scan_passes_keep_the_owners_sync_state() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let pass = |token: &str, removed: &[&str], complete: bool, full: bool| HandedPass {
            account_id: "riley-acct".into(),
            removed: removed.iter().map(|id| (*id).to_string()).collect(),
            sync_token: token.into(),
            complete,
            full,
        };
        let scans = [
            HandedScan {
                rows: vec![
                    row("riley-acct", "p1"),
                    row("riley-acct", "p2"),
                    row("riley-acct", "p3"),
                ],
                passes: vec![pass("t1", &[], true, true)],
            },
            HandedScan {
                rows: vec![row("riley-acct", "p4")],
                passes: vec![pass("t2", &["p2"], false, false)],
            },
            HandedScan {
                rows: vec![row("riley-acct", "p1")],
                passes: vec![pass("t3", &[], true, true)],
            },
        ];
        let guests = StubGuests {
            script: Arc::new(Mutex::new(scans.into_iter().collect())),
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;
        let token = || async {
            world
                .owner
                .scan_state(PLUGIN, "riley-acct")
                .await
                .unwrap()
                .map(|state| state.sync_token)
                .unwrap_or_default()
        };
        let mut expected_tokens = Vec::new();
        for (handed, stored) in [("", "t1"), ("t1", "t2"), ("t2", "t3")] {
            let job_id = enqueue_scan(&world.owner).await;
            eventually("the host scan to finish", || async {
                job(&world.owner, &job_id).await.state == JobState::Succeeded
            })
            .await;
            assert_eq!(token().await, stored);
            expected_tokens.push(if handed.is_empty() {
                std::collections::HashMap::new()
            } else {
                std::collections::HashMap::from([("riley-acct".to_string(), handed.to_string())])
            });
        }
        assert_eq!(*guests.tokens.lock().unwrap(), expected_tokens);
        // Only p1 is still held: p2 left in the delta pass, and the last
        // full pass listed p1 alone.
        let held = world
            .owner
            .remove_account_holdings(
                PLUGIN,
                "riley-acct",
                bookclerk_library::HoldingSelection::All,
            )
            .await
            .unwrap();
        assert_eq!(held, 1);
    }

    /// Credentials the guest refreshed during a host scan are written back
    /// to the owner as the handed account's row; a refresh for an account
    /// the owner did not hand is dropped, and the scan still succeeds.
    #[tokio::test]
    async fn a_host_scan_writes_refreshed_credentials_back_to_the_owner() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let guests = StubGuests {
            rows: vec![row("riley-acct", "p1")],
            work: world.host_files.path().join("library-work"),
            refresh: Some(r#"{"token":"riley-token-v2"}"#),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests).await;

        let job_id = enqueue_scan(&world.owner).await;
        eventually("the host scan to finish", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        let token_ref = SecretRef {
            kind: secret_kind::SOURCE_AUTH.into(),
            provider: PLUGIN.into(),
            account_id: "riley-acct".into(),
            name: "riley-acct.auth".into(),
        };
        assert_eq!(
            stored(&world.owner, &token_ref).await,
            r#"{"token":"riley-token-v2"}"#
        );
    }

    /// An account this host keeps has a job's refresh saved here, never
    /// written back. A save that fails (and fails its retry) still leaves
    /// the job's guests with the new credentials, and the next refresh is
    /// compared with the copy really stored here, so it is saved.
    #[tokio::test]
    async fn a_failed_local_save_keeps_the_guests_credentials_and_retries() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let token_ref = SecretRef {
            kind: secret_kind::SOURCE_AUTH.into(),
            provider: PLUGIN.into(),
            account_id: "riley-acct".into(),
            name: "riley-acct.auth".into(),
        };
        let handed = stored(&world.owner, &token_ref).await;
        let keeper = Arc::new(FlakyKeeper {
            failures: Mutex::new(2),
            ..FlakyKeeper::default()
        });
        let guests = StubGuests {
            rows: vec![row("riley-acct", "p1")],
            work: world.host_files.path().join("library-work"),
            refresh: Some(r#"{"token":"riley-token-v2"}"#),
            refresh_again: Some(r#"{"token":"riley-token-v3"}"#),
            keeper: Some(keeper.clone()),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;

        let job_id = enqueue_scan(&world.owner).await;
        eventually("the host scan to finish", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        // The guests hold each refresh, saved or not.
        assert_eq!(
            *guests.latest_seen.lock().unwrap(),
            vec![
                Some(br#"{"token":"riley-token-v2"}"#.to_vec()),
                Some(br#"{"token":"riley-token-v3"}"#.to_vec()),
            ]
        );
        // The failed save and its retry, then the next refresh compared
        // with the copy really stored (the handed one), and saved.
        let calls = keeper.calls.lock().unwrap().clone();
        let sent: Vec<&[u8]> = calls.iter().map(|(sent, _)| sent.as_slice()).collect();
        assert_eq!(sent, [handed.as_bytes(); 3]);
        assert_eq!(calls[2].1, br#"{"token":"riley-token-v3"}"#);
        // Nothing went back to the owner.
        assert_eq!(stored(&world.owner, &token_ref).await, handed);
    }

    /// A job whose last refresh could not be saved here, with no refresh
    /// after it, saves it when the job ends rather than leaving it only in
    /// the guests' memory.
    #[tokio::test]
    async fn a_failed_local_save_is_retried_when_the_job_ends() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let token_ref = SecretRef {
            kind: secret_kind::SOURCE_AUTH.into(),
            provider: PLUGIN.into(),
            account_id: "riley-acct".into(),
            name: "riley-acct.auth".into(),
        };
        let handed = stored(&world.owner, &token_ref).await;
        let keeper = Arc::new(FlakyKeeper {
            failures: Mutex::new(2),
            ..FlakyKeeper::default()
        });
        let guests = StubGuests {
            rows: vec![row("riley-acct", "p1")],
            work: world.host_files.path().join("library-work"),
            refresh: Some(r#"{"token":"riley-token-v2"}"#),
            keeper: Some(keeper.clone()),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;

        let job_id = enqueue_scan(&world.owner).await;
        eventually("the host scan to finish", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        // The failed save and its retry during the job, then one more
        // when it ended, which saved it.
        eventually("the save at the job's end", || async {
            keeper.calls.lock().unwrap().len() == 3
        })
        .await;
        let calls = keeper.calls.lock().unwrap().clone();
        let sent: Vec<&[u8]> = calls.iter().map(|(sent, _)| sent.as_slice()).collect();
        assert_eq!(sent, [handed.as_bytes(); 3]);
        assert_eq!(calls[2].1, br#"{"token":"riley-token-v2"}"#);
        assert_eq!(*keeper.failures.lock().unwrap(), 0);
        // Nothing went back to the owner.
        assert_eq!(stored(&world.owner, &token_ref).await, handed);
    }

    /// A member's host whose guest returns a row for another user's
    /// account gets the whole batch refused; nothing of it is written and
    /// the scan fails.
    #[tokio::test]
    async fn a_members_host_cannot_write_rows_for_an_account_it_does_not_own() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let sam = world
            .owner
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        world
            .owner
            .upsert_account(PLUGIN, "sam-acct", "us", None, true, "test")
            .await
            .unwrap();
        let identity = world
            .owner
            .ensure_local_portal_identity(sam.id, None)
            .await
            .unwrap();
        world
            .owner
            .link_account(identity.id, PLUGIN, "sam-acct", "test")
            .await
            .unwrap();
        let guests = StubGuests {
            rows: vec![row("riley-acct", "r1"), row("sam-acct", "s1")],
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests).await;

        let job_id = enqueue_scan(&world.owner).await;
        eventually("the scan to fail on the refused batch", || async {
            job(&world.owner, &job_id)
                .await
                .error_message
                .is_some_and(|message| message.contains("account_not_in_job"))
        })
        .await;
        assert_ne!(job(&world.owner, &job_id).await.state, JobState::Succeeded);
        assert!(world
            .owner
            .all_acquire_titles(Some("sam-acct"))
            .await
            .unwrap()
            .is_empty());
        assert!(
            world
                .owner
                .all_acquire_titles(Some("riley-acct"))
                .await
                .unwrap()
                .is_empty(),
            "nothing of the refused batch is written"
        );
    }

    /// A member's host acquires the owner's pending title and streams it
    /// to the owner, which writes it straight to its local destination:
    /// the title is acquired under its key, the job completes with the
    /// closing upload, the host's own local destination got its copy
    /// (H-2), no copy of the file exists anywhere under the owner's files
    /// dir, and at TRACE no token or upload credential reaches the logs.
    #[tokio::test]
    async fn a_streamed_acquire_reaches_the_owners_destination_without_staging() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        let _ = tracing_log::LogTracer::init();

        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        let host_local = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let payload: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
        let guests = StubGuests {
            payload: Arc::new(payload.clone()),
            local: Some(host_local.path().to_path_buf()),
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;

        let job_id = enqueue_acquire(&world.owner).await;
        eventually("the host acquire to complete", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        let acquired = world
            .owner
            .acquire_title_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(acquired.status, bookclerk_library::AcquireStatus::Acquired);
        let key = acquired.storage_key.clone().unwrap_or_default();
        let id = call_number(&world.owner, &book.holding_id).await;
        assert_eq!(
            key,
            format!("Author/Title t1/Title t1 [{id}].m4b"),
            "named as a local acquire would"
        );
        assert_eq!(
            std::fs::read(owner_out.path().join(&key)).unwrap(),
            payload,
            "the owner's destination has the file"
        );
        assert_eq!(
            std::fs::read(host_local.path().join(&key)).unwrap(),
            payload,
            "the host's own destination has its copy"
        );
        let holding = holding_of(&world.owner, &book.holding_id).await;
        assert_eq!(
            guests.imported.lock().unwrap().as_slice(),
            &[(
                holding,
                vec![(
                    bookclerk_config::DestinationId::new("local").unwrap(),
                    key.clone()
                )]
            )],
            "the owner's own title joins the branch's library with its copy"
        );
        assert!(job(&world.owner, &job_id)
            .await
            .progress
            .unwrap_or_default()
            .contains("acquired=1"));
        assert_eq!(
            guests.credentials.lock().unwrap().as_slice(),
            &[vec!["riley-acct".to_string()]]
        );
        for path in files_under(world.owner_files.path()) {
            let len = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            if len == payload.len() as u64 {
                assert_ne!(
                    std::fs::read(&path).unwrap(),
                    payload,
                    "the owner staged the file at {}",
                    path.display()
                );
            }
        }
        assert!(
            files_under(&world.host_files.path().join("library-work")).is_empty(),
            "the host's outbox is cleaned up"
        );

        let logs = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
        assert!(
            logs.contains("streamed a title to the owner"),
            "the capture sees the host"
        );
        assert!(
            logs.contains("granted a host upload"),
            "the capture sees the owner"
        );
        for value in ["riley-token-v1", INSTANCE_SECRET, "Bookclerk-Upload "] {
            assert!(!logs.contains(value), "{value} reached the logs");
            let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
            assert!(
                !logs.contains(&encoded),
                "{value} reached the logs as base64"
            );
        }
    }

    /// Acquires Sam's title on an Administrator's branch that asks for
    /// every member's work, with the link's "Keep copies of other members'
    /// books" set to `keep`, and answers whether the branch's own
    /// destination got a copy. The owner's destination always gets it.
    async fn branch_keeps_another_members_title(keep: bool) -> bool {
        let world = linked().await;
        let riley = world
            .owner
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .find(|user| user.display_name.as_deref() == Some("Riley"))
            .expect("the branch's owner")
            .id;
        world
            .owner
            .set_user_role(riley, UserRole::Administrator)
            .await
            .unwrap();
        let sam = world
            .owner
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        world
            .owner
            .upsert_account(PLUGIN, "sam-acct", "us", None, true, "test")
            .await
            .unwrap();
        let identity = world
            .owner
            .ensure_local_portal_identity(sam.id, None)
            .await
            .unwrap();
        world
            .owner
            .link_account(identity.id, PLUGIN, "sam-acct", "test")
            .await
            .unwrap();
        let record = build_sealed_record(
            b"sam-token",
            secret_kind::SOURCE_AUTH,
            PLUGIN,
            secret_account_type::INTEGRATION,
            "sam-acct",
            "sam-acct.auth",
        )
        .unwrap();
        upsert_secret(world.owner.db(), &record).await.unwrap();
        let owner_out = tempfile::tempdir().unwrap();
        let host_local = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("sam-acct", "s1"))
            .await
            .unwrap();
        let guests = StubGuests {
            payload: Arc::new(b"sam's audio".to_vec()),
            local: Some(host_local.path().to_path_buf()),
            work: world.host_files.path().join("library-work"),
            keep_member_copies: keep,
            ..StubGuests::default()
        };
        let imported = Arc::clone(&guests.imported);
        // The branch's operator turns on "Download for other members"; the
        // link's consent check tells the running client.
        bookclerk_library::control_plane::set_library_link_work(
            &world.host,
            &world.link.parent_library_id,
            true,
            keep,
        )
        .await
        .unwrap();
        let (handle, _stop) = start(&world, guests).await;
        eventually("the client to claim every member's work", || async {
            handle.claim_scope() == bookclerk_node_proto::ClaimScope::AllMembers
        })
        .await;
        let job_id = match world
            .owner
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Acquire,
                payload: JobPayload {
                    account: Some("sam-acct".into()),
                    ..Default::default()
                },
                priority: 0,
                max_attempts: 3,
                max_pending: 256,
                run_after: None,
            })
            .await
            .unwrap()
        {
            EnqueueOutcome::Created { id } => id,
            other => panic!("expected a new job: {other:?}"),
        };
        eventually("the branch to acquire Sam's title", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        let key = world
            .owner
            .acquire_title_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap()
            .storage_key
            .unwrap_or_default();
        assert_eq!(
            std::fs::read(owner_out.path().join(&key)).unwrap(),
            b"sam's audio",
            "the library's destination has the file"
        );
        assert!(
            imported.lock().unwrap().is_empty(),
            "another member's title never joins the branch's library"
        );
        host_local.path().join(&key).exists()
    }

    /// A branch keeps no copy of a book it acquired for another member
    /// unless "Keep copies of other members' books" is on.
    #[tokio::test]
    async fn another_members_book_is_kept_only_when_asked() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        assert!(!branch_keeps_another_members_title(false).await);
        assert!(branch_keeps_another_members_title(true).await);
    }

    /// A title whose edition another account's title already acquired is
    /// already in the library: the owner's plan leaves it out, as a local
    /// acquire does, so the host is not sent to fetch it, and the job
    /// completes with the edition still at its key.
    #[tokio::test]
    async fn a_host_is_not_sent_for_an_edition_the_owner_already_holds() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        world
            .owner
            .scope(PLUGIN, "test")
            .ensure_account("kim-acct", "us", None)
            .await
            .unwrap();
        let held = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("kim-acct", "t1"))
            .await
            .unwrap();
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let key = "Author/Title t1/Title t1.m4b";
        std::fs::create_dir_all(owner_out.path().join("Author/Title t1")).unwrap();
        std::fs::write(owner_out.path().join(key), b"audio").unwrap();
        world
            .owner
            .set_acquire_status(
                &held.holding_id,
                "kim-acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some(key),
                None,
            )
            .await
            .unwrap();
        let guests = StubGuests {
            payload: Arc::new(b"fetched".to_vec()),
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;

        let job_id = enqueue_acquire(&world.owner).await;
        eventually("the host acquire to complete", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        let edition = world
            .owner
            .edition_of_holding(&book.holding_id)
            .await
            .unwrap()
            .expect("the title has an edition");
        let shared = world
            .owner
            .edition_acquisition(&edition)
            .await
            .unwrap()
            .expect("the edition is acquired");
        assert_eq!(shared.status, bookclerk_library::AcquireStatus::Acquired);
        assert_eq!(shared.storage_key.as_deref(), Some(key));
        assert!(
            guests.credentials.lock().unwrap().is_empty(),
            "the host fetched nothing"
        );
        assert_eq!(std::fs::read(owner_out.path().join(key)).unwrap(), b"audio");
    }

    /// Two of the member's accounts own the same edition, and one job
    /// covers both: the host fetches and uploads it once, then asks for a
    /// new plan, which leaves the second account's title out because its
    /// edition is acquired now.
    #[tokio::test]
    async fn a_host_fetches_an_edition_two_accounts_own_once() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let riley = world
            .owner
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .find(|user| user.display_name.as_deref() == Some("Riley"))
            .unwrap();
        world
            .owner
            .upsert_account(PLUGIN, "kim-acct", "us", Some("riley-acct"), true, "test")
            .await
            .unwrap();
        let identity = world
            .owner
            .ensure_local_portal_identity(riley.id, None)
            .await
            .unwrap();
        world
            .owner
            .link_account(identity.id, PLUGIN, "kim-acct", "test")
            .await
            .unwrap();
        let record = build_sealed_record(
            b"kim-token-v1",
            secret_kind::SOURCE_AUTH,
            PLUGIN,
            secret_account_type::INTEGRATION,
            "kim-acct",
            "kim-acct.auth",
        )
        .unwrap();
        upsert_secret(world.owner.db(), &record).await.unwrap();
        let mut books = Vec::new();
        for account in ["riley-acct", "kim-acct"] {
            books.push(
                world
                    .owner
                    .scope(PLUGIN, "test")
                    .upsert_book(&owner_book(account, "t1"))
                    .await
                    .unwrap(),
            );
        }
        let guests = StubGuests {
            payload: Arc::new(b"fetched".to_vec()),
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;

        // The second account carries the first one's id as its label, so
        // one job names both.
        let job_id = enqueue_acquire(&world.owner).await;
        eventually("the host acquire to complete", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        let mut editions = Vec::new();
        for book in &books {
            editions.push(
                world
                    .owner
                    .edition_of_holding(&book.holding_id)
                    .await
                    .unwrap()
                    .expect("the title has an edition"),
            );
        }
        assert_eq!(editions[0], editions[1], "one edition on two accounts");
        let acquired = world
            .owner
            .edition_acquisition(&editions[0])
            .await
            .unwrap()
            .expect("the edition is acquired");
        assert_eq!(acquired.status, bookclerk_library::AcquireStatus::Acquired);
        assert!(acquired.storage_key.is_some());
        assert_eq!(
            guests.credentials.lock().unwrap().len(),
            1,
            "the host fetched the edition once"
        );
    }

    /// A shared node with two destinations whose naming templates differ
    /// gets each destination written under its own key, as a local
    /// acquire would: the plan carries both destinations' options, the
    /// file crosses the network once, the title is recorded under the
    /// primary destination's key, and neither destination receives the
    /// other's key.
    #[tokio::test]
    async fn each_owner_destination_gets_its_own_key() {
        use crate::node_api::uploads::{set_test_destinations, TestDestination};
        use bookclerk_config::{DestinationId, DestinationNaming};

        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        let s3_out = tempfile::tempdir().unwrap();
        set_test_destinations(
            world.owner_files.path(),
            vec![
                TestDestination {
                    id: DestinationId::new("local").unwrap(),
                    root: local_out.path().to_path_buf(),
                    naming: DestinationNaming {
                        folder_template: Some("<author>/<title>".into()),
                        file_template: Some("<title>".into()),
                        ..DestinationNaming::default()
                    },
                },
                TestDestination {
                    id: DestinationId::new("s3").unwrap(),
                    root: s3_out.path().to_path_buf(),
                    naming: DestinationNaming {
                        folder_template: Some("library/<asin>".into()),
                        file_template: Some("<asin>".into()),
                        ..DestinationNaming::default()
                    },
                },
            ],
        );
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let guests = StubGuests {
            payload: Arc::new(payload.clone()),
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;

        let job_id = enqueue_acquire(&world.owner).await;
        eventually("the host acquire to complete", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        // Each destination writes its own metadata sidecar; count objects.
        let objects = |dir: &Path| {
            files_under(dir)
                .into_iter()
                .filter(|path| !path.to_string_lossy().ends_with(".bookclerk-meta.json"))
                .collect::<Vec<_>>()
        };
        let local_files = objects(local_out.path());
        let s3_files = objects(s3_out.path());
        assert_eq!(local_files.len(), 1, "{local_files:?}");
        assert_eq!(s3_files.len(), 1, "{s3_files:?}");
        let local_key = local_files[0]
            .strip_prefix(local_out.path())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let s3_key = s3_files[0]
            .strip_prefix(s3_out.path())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(local_key, "Author/Title t1/Title t1.m4b");
        assert_eq!(s3_key, "library/t1/t1.m4b");
        assert_eq!(std::fs::read(&local_files[0]).unwrap(), payload);
        assert_eq!(std::fs::read(&s3_files[0]).unwrap(), payload);
        let acquired = world
            .owner
            .acquire_title_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            acquired.storage_key.as_deref(),
            Some(local_key.as_str()),
            "the title is recorded under the primary destination's key"
        );
    }

    /// Two filesystem stand-in destinations on the owner, `local` and
    /// `s3`, rooted at `local_out` and `s3_out` and naming a title
    /// `<author>/<title>/<title>` and `library/<asin>/<asin>`.
    fn two_destinations(world: &Linked, local_out: &Path, s3_out: &Path) {
        use crate::node_api::uploads::{set_test_destinations, TestDestination};
        use bookclerk_config::{DestinationId, DestinationNaming};

        set_test_destinations(
            world.owner_files.path(),
            vec![
                TestDestination {
                    id: DestinationId::new("local").unwrap(),
                    root: local_out.to_path_buf(),
                    naming: DestinationNaming {
                        folder_template: Some("<author>/<title>".into()),
                        file_template: Some("<title>".into()),
                        ..DestinationNaming::default()
                    },
                },
                TestDestination {
                    id: DestinationId::new("s3").unwrap(),
                    root: s3_out.to_path_buf(),
                    naming: DestinationNaming {
                        folder_template: Some("library/<asin>".into()),
                        file_template: Some("<asin>".into()),
                        ..DestinationNaming::default()
                    },
                },
            ],
        );
    }

    /// Stores a filter on the owner's destination `instance` that refuses
    /// every book of the store account `account`.
    async fn exclude_account(store: &LibraryStore, instance: &str, account: &str) {
        use bookclerk_config::{DestinationFilter, FilterEffect, FilterField, FilterRule};

        bookclerk_library::control_plane::set_destination_filter(
            store,
            &bookclerk_library::control_plane::ConfigActor::Operator {
                id: "operator:test".into(),
            },
            instance,
            DestinationFilter {
                rules: vec![FilterRule {
                    field: FilterField::Account,
                    value: account.into(),
                    effect: FilterEffect::Exclude,
                }],
            },
        )
        .await
        .unwrap();
    }

    /// Stored files under `dir`, without the metadata sidecars each
    /// destination writes.
    fn objects(dir: &Path) -> Vec<PathBuf> {
        files_under(dir)
            .into_iter()
            .filter(|path| !path.to_string_lossy().ends_with(".bookclerk-meta.json"))
            .collect()
    }

    /// A branch's acquire for the library writes each title only to the
    /// destinations whose filter takes it: the plan lists that set per
    /// title, and the destination that refuses the title gets nothing.
    #[tokio::test]
    async fn a_hosts_acquire_reaches_only_the_destinations_that_take_the_title() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        let s3_out = tempfile::tempdir().unwrap();
        two_destinations(&world, local_out.path(), s3_out.path());
        exclude_account(&world.owner, "s3", "riley-acct").await;
        world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let guests = StubGuests {
            payload: Arc::new(payload.clone()),
            work: world.host_files.path().join("cluster-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;

        let job_id = enqueue_acquire(&world.owner).await;
        eventually("the host acquire to complete", || async {
            job(&world.owner, &job_id).await.state == JobState::Succeeded
        })
        .await;
        let local_files = objects(local_out.path());
        assert_eq!(local_files.len(), 1, "{local_files:?}");
        assert_eq!(std::fs::read(&local_files[0]).unwrap(), payload);
        assert!(
            objects(s3_out.path()).is_empty(),
            "the destination that refuses the title gets nothing"
        );
        // The commit recorded the library's copy on the destination that
        // took it, under its key there, and none on the other.
        let title = world
            .owner
            .acquire_titles_named(None, "t1")
            .await
            .unwrap()
            .remove(0);
        let copies = world
            .owner
            .copies_of_editions(std::slice::from_ref(&title.edition_id))
            .await
            .unwrap();
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert_eq!(copies[0].destination, "local");
        assert_eq!(copies[0].state, bookclerk_library::CopyState::Stored);
        assert_eq!(copies[0].storage_key, title.storage_key);
    }

    /// The owner's plan lists, for each title, the destinations whose
    /// filter takes it, in the plan's order, beside every destination's
    /// naming options.
    #[tokio::test]
    async fn the_plan_lists_each_titles_target_set() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        let s3_out = tempfile::tempdir().unwrap();
        two_destinations(&world, local_out.path(), s3_out.path());
        exclude_account(&world.owner, "s3", "riley-acct").await;
        world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let (handle, _stop) = start(&world, StubGuests::default()).await;
        let answer = handle
            .request(Body::AcquirePlan { job_id, fence })
            .await
            .expect("the owner answers");
        let Body::AcquireTargets {
            targets,
            destinations,
        } = answer.body
        else {
            panic!("{answer:?}");
        };
        assert_eq!(
            destinations
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            ["local", "s3"]
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].destinations, Some(vec!["local".to_string()]));
    }

    /// A title no destination takes fails on the host before anything is
    /// fetched or written, as a local acquire of it would.
    #[tokio::test]
    async fn a_title_no_destination_takes_fails_on_the_host() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        let s3_out = tempfile::tempdir().unwrap();
        two_destinations(&world, local_out.path(), s3_out.path());
        exclude_account(&world.owner, "local", "riley-acct").await;
        exclude_account(&world.owner, "s3", "riley-acct").await;
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let guests = StubGuests {
            work: world.host_files.path().join("cluster-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests.clone()).await;

        let job_id = enqueue_acquire(&world.owner).await;
        eventually("the host acquire to fail", || async {
            job(&world.owner, &job_id).await.state == JobState::Failed
        })
        .await;
        assert!(objects(local_out.path()).is_empty());
        assert!(objects(s3_out.path()).is_empty());
        let row = world
            .owner
            .acquire_title_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(row.status, bookclerk_library::AcquireStatus::Acquired);
    }

    /// An upload that names a destination whose filter refuses the title is
    /// refused at its grant, whatever the host asks.
    #[tokio::test]
    async fn a_grant_naming_a_destination_that_refuses_the_title_is_refused() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        let s3_out = tempfile::tempdir().unwrap();
        two_destinations(&world, local_out.path(), s3_out.path());
        exclude_account(&world.owner, "s3", "riley-acct").await;
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let (handle, _stop) = start(&world, StubGuests::default()).await;
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let mut file = local_file("Author/Title t1/Title t1.m4b", b"audio");
        file.copies.push(bookclerk_node_proto::FileCopy {
            destination: "s3".into(),
            key: "library/t1/t1.m4b".into(),
        });
        let answer = handle
            .request(Body::UploadGrant {
                job_id: job_id.clone(),
                fence,
                holding_id: holding_of(&world.owner, &book.holding_id).await,
                primary_key: "Author/Title t1/Title t1.m4b".into(),
                pdf_key: None,
                files: vec![file],
            })
            .await
            .expect("the owner answers");
        assert!(
            format!("{answer:?}").contains("destinations_changed"),
            "{answer:?}"
        );
        assert!(objects(local_out.path()).is_empty());
        assert!(objects(s3_out.path()).is_empty());
    }

    /// A filter edited after a title's upload was granted changes its
    /// target set: its next file and its commit are refused, and what it
    /// had received is deleted, so the title is never stored against the
    /// filter now in force.
    #[tokio::test]
    async fn an_upload_is_refused_after_a_filter_edit() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let scope = local_scope("Author/Title t1");
        let files = vec![
            local_file("Author/Title t1/t1.m4b", b"audio"),
            local_file("Author/Title t1/cover.jpg", b"jpeg"),
        ];
        let (upload_id, token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &files, &scope).await;
        let put = put_raw(world.addr, &upload_id, &token, 0, b"audio").await;
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        exclude_account(&world.owner, "local", "riley-acct").await;
        let second = put_raw(world.addr, &upload_id, &token, 1, b"jpeg").await;
        assert!(second.starts_with("HTTP/1.1 422"), "{second}");
        assert!(second.contains("destinations_changed"), "{second}");
        assert!(
            files_under(owner_out.path()).is_empty(),
            "the refused title's received file is deleted"
        );
        let committed = commit_raw(world.addr, &upload_id, &token, "{}").await;
        assert!(!committed.starts_with("HTTP/1.1 200"), "{committed}");
    }

    /// A commit is refused once a filter edited after the title's last
    /// file refuses it, and what it received is deleted.
    #[tokio::test]
    async fn a_commit_is_refused_after_a_filter_edit() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let scope = local_scope("Author/Title t1");
        let files = vec![local_file("Author/Title t1/t1.m4b", b"audio")];
        let (upload_id, token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &files, &scope).await;
        let put = put_raw(world.addr, &upload_id, &token, 0, b"audio").await;
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        exclude_account(&world.owner, "local", "riley-acct").await;
        let committed = commit_raw(world.addr, &upload_id, &token, "{}").await;
        assert!(committed.starts_with("HTTP/1.1 422"), "{committed}");
        assert!(committed.contains("destinations_changed"), "{committed}");
        assert!(
            files_under(owner_out.path()).is_empty(),
            "the refused title's received file is deleted"
        );
        let row = world
            .owner
            .acquire_title_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(row.status, bookclerk_library::AcquireStatus::Acquired);
    }

    /// The manifest of a title acquired into two destinations: identical
    /// bytes cross once with a key per destination, a file only one
    /// destination has gets one copy, and the outbox backend's metadata
    /// sidecars and temp files are not uploaded.
    #[tokio::test]
    async fn the_upload_manifest_sends_each_content_once_with_a_key_per_destination() {
        use super::super::host_jobs::{outbox_keys, upload_manifest, HostDestination};

        let outbox = tempfile::tempdir().unwrap();
        let write = |index: usize, key: &str, body: &[u8]| {
            let path = destination_outbox(outbox.path(), index).join(key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write(0, "A/T/T.m4b", b"audio");
        write(0, "A/T/T.m4b.bookclerk-meta.json", b"{}");
        write(0, "A/T/.T.jpg.bookclerk-tmp-1", b"partial");
        write(0, "A/T/cover.jpg", b"cover-local");
        write(1, "lib/t1/t1.m4b", b"audio");
        let keys = vec![
            outbox_keys(&destination_outbox(outbox.path(), 0))
                .await
                .unwrap(),
            outbox_keys(&destination_outbox(outbox.path(), 1))
                .await
                .unwrap(),
        ];
        assert_eq!(
            keys,
            vec![
                vec!["A/T/T.m4b".to_string(), "A/T/cover.jpg".to_string()],
                vec!["lib/t1/t1.m4b".to_string()],
            ]
        );
        let destination = |id: &str| HostDestination {
            id: bookclerk_config::DestinationId::new(id).unwrap(),
            options: Default::default(),
        };
        let destinations = vec![destination("local"), destination("s3")];
        let acquired = AcquiredTitle {
            primary_key: "A/T/T.m4b".into(),
            pdf_key: None,
            keys,
        };
        let manifest = upload_manifest(&destinations, outbox.path(), &acquired, None)
            .await
            .unwrap();
        assert_eq!(manifest.len(), 2, "{manifest:?}");
        let audio = &manifest[0].0;
        assert_eq!(audio.key_on("local"), Some("A/T/T.m4b"));
        assert_eq!(audio.key_on("s3"), Some("lib/t1/t1.m4b"));
        assert_eq!(audio.size, 5);
        let cover = &manifest[1].0;
        assert_eq!(cover.copies.len(), 1);
        assert_eq!(cover.key_on("local"), Some("A/T/cover.jpg"));

        let missing = AcquiredTitle {
            keys: vec![acquired.keys[0].clone(), Vec::new()],
            ..acquired.clone()
        };
        assert!(
            upload_manifest(&destinations, outbox.path(), &missing, None)
                .await
                .is_err(),
            "a destination with no file fails the title"
        );
    }

    /// The manifest states what a probe found in each audio file (media
    /// type from the bytes, bitrate, the store it came from); a file the
    /// probe cannot read and a file that is not audio carry none and keep
    /// the media type of their extension.
    #[tokio::test]
    async fn the_upload_manifest_carries_each_audio_files_probe() {
        use super::super::host_jobs::{outbox_keys, upload_manifest_probed_by, HostDestination};

        let outbox = tempfile::tempdir().unwrap();
        let write = |key: &str, body: &[u8]| {
            let path = destination_outbox(outbox.path(), 0).join(key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        let tagged = bookclerk_tags::fixtures::tagged_m4b();
        write("A/T/T.m4b", &tagged);
        write("A/T/T - Part 2.mp3", b"not audio at all");
        write("A/T/cover.jpg", b"cover");
        let acquired = AcquiredTitle {
            primary_key: "A/T/T.m4b".into(),
            pdf_key: None,
            keys: vec![outbox_keys(&destination_outbox(outbox.path(), 0))
                .await
                .unwrap()],
        };
        let destinations = vec![HostDestination {
            id: bookclerk_config::DestinationId::new("local").unwrap(),
            options: Default::default(),
        }];
        let in_process = bookclerk_media::MediaPool::in_process();
        let manifest = |store: &'static str, media: bookclerk_media::MediaPool| {
            let (destinations, acquired) = (&destinations, &acquired);
            let outbox = outbox.path();
            async move {
                upload_manifest_probed_by(destinations, outbox, acquired, Some(store), &media)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|(file, _)| file)
                    .collect::<Vec<_>>()
            }
        };
        let files = manifest("audible", in_process).await;
        let of = |key: &str| {
            files
                .iter()
                .find(|file| file.key_on("local") == Some(key))
                .unwrap_or_else(|| panic!("{key} is in the manifest"))
        };
        let audio = of("A/T/T.m4b");
        assert_eq!(audio.content_type.as_deref(), Some("audio/mp4"));
        let probe = audio.probe.as_ref().expect("the audio file was probed");
        assert_eq!(probe.source_store.as_deref(), Some("audible"));
        assert!(!probe.lossless);
        assert!(probe.bitrate_kbps.is_some_and(|kbps| kbps > 0));
        assert!(audio.is_well_formed());
        let unreadable = of("A/T/T - Part 2.mp3");
        assert_eq!(unreadable.content_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(unreadable.probe, None);
        let cover = of("A/T/cover.jpg");
        assert_eq!(cover.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(cover.probe, None);

        // A store name that is not a plain id is left out, not sent.
        let files = manifest("Not A Store", bookclerk_media::MediaPool::in_process()).await;
        let probe = files
            .iter()
            .find_map(|file| file.probe.as_ref())
            .expect("the audio file was probed");
        assert_eq!(probe.source_store, None);
    }

    /// A key an upload was granted, then written by a local acquire of
    /// another title before the file arrived, is refused and left as the
    /// acquire wrote it.
    #[tokio::test]
    async fn an_upload_does_not_overwrite_a_key_taken_since_its_grant() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), local_out.path());
        let title = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        let other = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t3"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let node_id = world.link.remote_node_id.clone();
        let claim = claim_remote_job(
            &world.owner,
            &node_id,
            JobResourceClass::Network,
            RemoteClaimSettings {
                lease_secs: 30,
                limits: JobClaimLimits {
                    class: 8,
                    plugin_instance: 8,
                },
            },
            None,
        )
        .await
        .unwrap()
        .expect("the member's host may claim the acquire");
        assert_eq!(claim.job.id, job_id);
        let owner_node = bookclerk_library::control_plane::load_or_create_node_identity(
            world.owner_files.path(),
        )
        .unwrap()
        .node_id;
        const KEY: &str = "Author/Title t2/Title t2.m4b";
        let file = UploadFile {
            copies: vec![bookclerk_node_proto::FileCopy {
                destination: "local".into(),
                key: KEY.into(),
            }],
            size: 3,
            sha256: sha2::Sha256::digest(b"abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            content_type: None,
            probe: None,
        };
        let UploadGrant::Granted {
            upload_id, token, ..
        } = grant_upload(
            &world.owner,
            &node_id,
            &job_id,
            claim.job.lease_generation,
            &owner_node,
            &UploadGrantRequest {
                holding_id: &holding_of(&world.owner, &title.holding_id).await,
                primary_key: KEY,
                pdf_key: None,
                files: std::slice::from_ref(&file),
                destinations: &[UploadDestination {
                    id: "local".into(),
                    title_folders: vec!["Author/Title t2".into()],
                    title_names: None,
                }],
            },
        )
        .await
        .unwrap()
        else {
            panic!("granted");
        };
        // The grant's claim is given back and a local acquire of another
        // title records the key, then writes it (an acquire never takes a
        // key a live edition holds).
        take_granted_key(&world.owner, KEY, &upload_id, &other.holding_id).await;
        let mut tcp = tokio::net::TcpStream::connect(world.addr).await.unwrap();
        let head = format!(
            "PUT /api/node/v1/uploads/{upload_id}/files/0 HTTP/1.1\r\n\
             Host: localhost:{}\r\nAuthorization: Bookclerk-Upload {}\r\n\
             Content-Length: 3\r\nConnection: close\r\n\r\nabc",
            world.addr.port(),
            token.as_str(),
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.starts_with("HTTP/1.1 422"), "{answer}");
        assert!(answer.contains("key_taken"), "{answer}");
        assert!(
            !local_out.path().join(KEY).exists(),
            "the upload wrote nothing over the other title's key"
        );
    }

    /// An upload granted for one destination set is refused, and writes
    /// nothing, once the receiving node's set changed: the host named the
    /// files for the destinations it was planned with.
    #[tokio::test]
    async fn an_upload_is_refused_after_the_destinations_change() {
        use crate::node_api::uploads::{set_test_destinations, TestDestination};
        use bookclerk_config::{DestinationId, DestinationNaming};

        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        let s3_out = tempfile::tempdir().unwrap();
        set_test_destinations(
            world.owner_files.path(),
            vec![
                TestDestination {
                    id: DestinationId::new("local").unwrap(),
                    root: local_out.path().to_path_buf(),
                    naming: DestinationNaming::default(),
                },
                TestDestination {
                    id: DestinationId::new("s3").unwrap(),
                    root: s3_out.path().to_path_buf(),
                    naming: DestinationNaming::default(),
                },
            ],
        );
        let second = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let node_id = world.link.remote_node_id.clone();
        let claim = claim_remote_job(
            &world.owner,
            &node_id,
            JobResourceClass::Network,
            RemoteClaimSettings {
                lease_secs: 30,
                limits: JobClaimLimits {
                    class: 8,
                    plugin_instance: 8,
                },
            },
            None,
        )
        .await
        .unwrap()
        .expect("the member's host may claim the acquire");
        assert_eq!(claim.job.id, job_id);
        let owner_node = bookclerk_library::control_plane::load_or_create_node_identity(
            world.owner_files.path(),
        )
        .unwrap()
        .node_id;
        let file = UploadFile {
            copies: vec![
                bookclerk_node_proto::FileCopy {
                    destination: "local".into(),
                    key: "Author/Title t2/Title t2.m4b".into(),
                },
                bookclerk_node_proto::FileCopy {
                    destination: "s3".into(),
                    key: "library/t2/t2.m4b".into(),
                },
            ],
            size: 3,
            sha256: sha2::Sha256::digest(b"abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            content_type: None,
            probe: None,
        };
        let UploadGrant::Granted {
            upload_id, token, ..
        } = grant_upload(
            &world.owner,
            &node_id,
            &job_id,
            claim.job.lease_generation,
            &owner_node,
            &UploadGrantRequest {
                holding_id: &holding_of(&world.owner, &second.holding_id).await,
                primary_key: "Author/Title t2/Title t2.m4b",
                pdf_key: None,
                files: std::slice::from_ref(&file),
                destinations: &[
                    UploadDestination {
                        id: "local".into(),
                        title_folders: vec!["Author/Title t2".into()],
                        title_names: None,
                    },
                    UploadDestination {
                        id: "s3".into(),
                        title_folders: vec!["library/t2".into()],
                        title_names: None,
                    },
                ],
            },
        )
        .await
        .unwrap()
        else {
            panic!("granted");
        };
        // The node drops its s3 destination before the file arrives.
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), local_out.path());
        let mut tcp = tokio::net::TcpStream::connect(world.addr).await.unwrap();
        let head = format!(
            "PUT /api/node/v1/uploads/{upload_id}/files/0 HTTP/1.1\r\n\
             Host: localhost:{}\r\nAuthorization: Bookclerk-Upload {}\r\n\
             Content-Length: 3\r\nConnection: close\r\n\r\nabc",
            world.addr.port(),
            token.as_str(),
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.starts_with("HTTP/1.1 422"), "{answer}");
        assert!(answer.contains("destinations_changed"), "{answer}");
        assert!(
            !local_out.path().join("Author/Title t2").exists(),
            "nothing is written to a changed destination set"
        );
    }

    /// A title whose files all arrived is still refused at its commit, and
    /// what it received is deleted, once the receiving node's destination
    /// set changed after its last file: the new destination never got it.
    #[tokio::test]
    async fn a_commit_is_refused_after_the_destinations_change() {
        use crate::node_api::uploads::{set_test_destinations, TestDestination};
        use bookclerk_config::{DestinationId, DestinationNaming};

        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        let s3_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let scope = local_scope("Author/Title t1");
        let files = vec![local_file("Author/Title t1/t1.m4b", b"audio")];
        let (upload_id, token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &files, &scope).await;
        let put = put_raw(world.addr, &upload_id, &token, 0, b"audio").await;
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        assert!(owner_out.path().join("Author/Title t1/t1.m4b").is_file());
        // The node deploys an s3 destination before the commit.
        set_test_destinations(
            world.owner_files.path(),
            vec![
                TestDestination {
                    id: DestinationId::new("local").unwrap(),
                    root: owner_out.path().to_path_buf(),
                    naming: DestinationNaming::default(),
                },
                TestDestination {
                    id: DestinationId::new("s3").unwrap(),
                    root: s3_out.path().to_path_buf(),
                    naming: DestinationNaming::default(),
                },
            ],
        );
        let committed = commit_raw(world.addr, &upload_id, &token, "{}").await;
        assert!(committed.starts_with("HTTP/1.1 422"), "{committed}");
        assert!(committed.contains("destinations_changed"), "{committed}");
        let row = world
            .owner
            .acquire_title_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(row.status, bookclerk_library::AcquireStatus::Acquired);
        assert!(
            !owner_out.path().join("Author/Title t1/t1.m4b").exists(),
            "the refused title's received file is deleted"
        );
    }

    /// A file that arrives once the receiving shared node has no output
    /// destination deployed is answered `unavailable`, so the branch node
    /// retries; nothing is written.
    #[tokio::test]
    async fn a_file_is_unavailable_while_no_destination_is_deployed() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let scope = local_scope("Author/Title t1");
        let files = vec![local_file("Author/Title t1/t1.m4b", b"audio")];
        let (upload_id, token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &files, &scope).await;
        // The node undeploys its only destination before the file arrives.
        crate::node_api::uploads::set_test_destinations(world.owner_files.path(), Vec::new());
        let put = put_raw(world.addr, &upload_id, &token, 0, b"audio").await;
        assert!(put.starts_with("HTTP/1.1 503"), "{put}");
        assert!(put.contains("unavailable"), "{put}");
        assert!(!owner_out.path().join("Author/Title t1/t1.m4b").exists());
    }

    /// Sends `body` as the part of file 0 of `upload_id` that `range`
    /// names (a `Content-Range` value) over plain HTTP; answers the raw
    /// response.
    async fn put_part(
        addr: SocketAddr,
        upload_id: &str,
        token: &str,
        range: &str,
        body: &[u8],
    ) -> String {
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "PUT /api/node/v1/uploads/{upload_id}/files/0 HTTP/1.1\r\n\
             Host: localhost:{}\r\nAuthorization: Bookclerk-Upload {token}\r\n\
             Content-Range: {range}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            addr.port(),
            body.len()
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        tcp.write_all(body).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// Grants a one-file upload of `bytes` at `Author/Title t1/t1.m4b`
    /// into a local stand-in destination; answers the world, the
    /// destination's root, and the upload id and token.
    async fn granted_local_file(bytes: &[u8]) -> (Linked, tempfile::TempDir, String, String) {
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let scope = local_scope("Author/Title t1");
        let files = vec![local_file("Author/Title t1/t1.m4b", bytes)];
        let (upload_id, token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &files, &scope).await;
        (world, owner_out, upload_id, token)
    }

    /// A file sent in parts (each its own request, as through a relay that
    /// caps request bodies) is one write: the destination gets the whole,
    /// verified file once the last part arrives, and the upload commits.
    #[tokio::test]
    async fn a_file_sent_in_parts_is_written_once_whole() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        const FILE: &[u8] = b"audio-in-parts!";
        let (world, owner_out, upload_id, token) = granted_local_file(FILE).await;
        let stored = owner_out.path().join("Author/Title t1/t1.m4b");
        let first = put_part(world.addr, &upload_id, &token, "bytes 0-4/15", &FILE[..5]).await;
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(first.contains("\"partial\""), "{first}");
        assert!(first.contains("\"offset\":5"), "{first}");
        let second = put_part(world.addr, &upload_id, &token, "bytes 5-9/15", &FILE[5..10]).await;
        assert!(second.contains("\"partial\""), "{second}");
        assert!(!stored.exists(), "nothing lands before the last part");
        let last = put_part(
            world.addr,
            &upload_id,
            &token,
            "bytes 10-14/15",
            &FILE[10..],
        )
        .await;
        assert!(last.starts_with("HTTP/1.1 200"), "{last}");
        assert!(last.contains("\"received\""), "{last}");
        assert_eq!(std::fs::read(&stored).unwrap(), FILE);
        // A part of a file already received is answered from the record.
        let again = put_part(world.addr, &upload_id, &token, "bytes 0-4/15", &FILE[..5]).await;
        assert!(again.contains("\"received\""), "{again}");
        let committed = commit_raw(world.addr, &upload_id, &token, "{}").await;
        assert!(committed.starts_with("HTTP/1.1 200"), "{committed}");
    }

    /// A part that does not start where the open file stands (a gap, or
    /// no earlier part on this node) is told where to resume and changes
    /// nothing; the status query says the same, and the file then lands
    /// whole from there.
    #[tokio::test]
    async fn a_part_out_of_order_is_told_where_to_resume() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        const FILE: &[u8] = b"audio-in-parts!";
        let (world, owner_out, upload_id, token) = granted_local_file(FILE).await;
        let stored = owner_out.path().join("Author/Title t1/t1.m4b");
        let status = put_part(world.addr, &upload_id, &token, "bytes */15", b"").await;
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        assert!(status.contains("\"offset\":0"), "{status}");
        let orphan = put_part(world.addr, &upload_id, &token, "bytes 5-9/15", &FILE[5..10]).await;
        assert!(orphan.starts_with("HTTP/1.1 503"), "{orphan}");
        assert!(orphan.contains("\"resume\""), "{orphan}");
        assert!(orphan.contains("\"offset\":0"), "{orphan}");
        let first = put_part(world.addr, &upload_id, &token, "bytes 0-4/15", &FILE[..5]).await;
        assert!(first.contains("\"partial\""), "{first}");
        assert!(first.contains("\"offset\":5"), "{first}");
        let gap = put_part(
            world.addr,
            &upload_id,
            &token,
            "bytes 10-14/15",
            &FILE[10..],
        )
        .await;
        assert!(gap.starts_with("HTTP/1.1 503"), "{gap}");
        assert!(gap.contains("\"offset\":5"), "{gap}");
        let status = put_part(world.addr, &upload_id, &token, "bytes */15", b"").await;
        assert!(status.contains("\"offset\":5"), "{status}");
        for (range, body) in [
            ("bytes 5-9/15", &FILE[5..10]),
            ("bytes 10-14/15", &FILE[10..]),
        ] {
            let put = put_part(world.addr, &upload_id, &token, range, body).await;
            assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        }
        assert_eq!(std::fs::read(&stored).unwrap(), FILE);
        let status = put_part(world.addr, &upload_id, &token, "bytes */15", b"").await;
        assert!(status.contains("\"received\""), "{status}");
    }

    /// A part whose connection breaks part way keeps what reached the
    /// write: the status query names the next byte (inside the part), and
    /// only the rest is sent again.
    #[tokio::test]
    async fn a_broken_part_resumes_from_its_last_byte() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        const FILE: &[u8] = b"audio-in-parts!";
        let (world, owner_out, upload_id, token) = granted_local_file(FILE).await;
        let stored = owner_out.path().join("Author/Title t1/t1.m4b");
        let first = put_part(world.addr, &upload_id, &token, "bytes 0-4/15", &FILE[..5]).await;
        assert!(first.contains("\"offset\":5"), "{first}");
        // Declares bytes 5-14 but sends only 5-7, then the connection drops.
        let mut tcp = tokio::net::TcpStream::connect(world.addr).await.unwrap();
        let head = format!(
            "PUT /api/node/v1/uploads/{upload_id}/files/0 HTTP/1.1\r\n\
             Host: localhost:{}\r\nAuthorization: Bookclerk-Upload {token}\r\n\
             Content-Range: bytes 5-14/15\r\nContent-Length: 10\r\nConnection: close\r\n\r\n",
            world.addr.port(),
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        tcp.write_all(&FILE[5..8]).await.unwrap();
        tcp.flush().await.unwrap();
        drop(tcp);
        eventually("the owner to keep the broken part's bytes", || {
            let (upload_id, token) = (upload_id.clone(), token.clone());
            async move {
                put_part(world.addr, &upload_id, &token, "bytes */15", b"")
                    .await
                    .contains("\"offset\":8")
            }
        })
        .await;
        let rest = put_part(world.addr, &upload_id, &token, "bytes 8-14/15", &FILE[8..]).await;
        assert!(rest.starts_with("HTTP/1.1 200"), "{rest}");
        assert!(rest.contains("\"received\""), "{rest}");
        assert_eq!(std::fs::read(&stored).unwrap(), FILE);
    }

    /// A part whose bytes do not match the manifest fails the whole file
    /// once it is found out (here at the last part's digest check), and
    /// nothing is written.
    #[tokio::test]
    async fn a_file_sent_in_parts_is_verified_whole() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        const FILE: &[u8] = b"audio-in-parts!";
        let (world, owner_out, upload_id, token) = granted_local_file(FILE).await;
        let first = put_part(
            world.addr,
            &upload_id,
            &token,
            "bytes 0-9/15",
            b"AUDIO-IN-P",
        )
        .await;
        assert!(first.contains("\"partial\""), "{first}");
        let last = put_part(
            world.addr,
            &upload_id,
            &token,
            "bytes 10-14/15",
            &FILE[10..],
        )
        .await;
        assert!(last.starts_with("HTTP/1.1 422"), "{last}");
        assert!(last.contains("content_mismatch"), "{last}");
        assert!(files_under(owner_out.path()).is_empty());
    }

    /// A range that is not exactly a part of the declared file is refused
    /// before anything is opened.
    #[tokio::test]
    async fn an_upload_part_with_a_bad_range_is_refused() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        const FILE: &[u8] = b"audio-in-parts!";
        let (world, owner_out, upload_id, token) = granted_local_file(FILE).await;
        for range in [
            "bytes 0-4/16",
            "bytes 0-15/15",
            "bytes=0-4/15",
            "bytes 4-0/15",
        ] {
            let put = put_part(world.addr, &upload_id, &token, range, &FILE[..5]).await;
            assert!(put.starts_with("HTTP/1.1 422"), "{range}: {put}");
            assert!(put.contains("invalid_range"), "{range}: {put}");
        }
        // A part whose body is not the length its range names.
        let short = put_part(world.addr, &upload_id, &token, "bytes 0-4/15", &FILE[..4]).await;
        assert!(short.contains("size_mismatch"), "{short}");
        assert!(files_under(owner_out.path()).is_empty());
    }

    /// A commit that arrives once the receiving shared node has no output
    /// destination deployed is answered `unavailable`, and the title is
    /// not marked acquired.
    #[tokio::test]
    async fn a_commit_is_unavailable_while_no_destination_is_deployed() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let scope = local_scope("Author/Title t1");
        let files = vec![local_file("Author/Title t1/t1.m4b", b"audio")];
        let (upload_id, token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &files, &scope).await;
        let put = put_raw(world.addr, &upload_id, &token, 0, b"audio").await;
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        // The node undeploys its only destination before the commit.
        crate::node_api::uploads::set_test_destinations(world.owner_files.path(), Vec::new());
        let committed = commit_raw(world.addr, &upload_id, &token, "{}").await;
        assert!(committed.starts_with("HTTP/1.1 503"), "{committed}");
        assert!(committed.contains("unavailable"), "{committed}");
        let row = world
            .owner
            .acquire_title_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(row.status, bookclerk_library::AcquireStatus::Acquired);
    }

    /// A fence lost while a file streams aborts the upload: the owner
    /// answers `lost` before the body ends, its destination keeps nothing
    /// (not even a temp file), and the upload's token no longer opens it.
    #[tokio::test]
    async fn a_fence_lost_mid_upload_aborts_the_upload() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let node_id = world.link.remote_node_id.clone();
        let claim = claim_remote_job(
            &world.owner,
            &node_id,
            JobResourceClass::Network,
            RemoteClaimSettings {
                lease_secs: 1,
                limits: JobClaimLimits {
                    class: 8,
                    plugin_instance: 8,
                },
            },
            None,
        )
        .await
        .unwrap()
        .expect("the member's host may claim the acquire");
        assert_eq!(claim.job.id, job_id);
        let payload = vec![7u8; 4 << 20];
        let file = UploadFile {
            copies: vec![bookclerk_node_proto::FileCopy {
                destination: "local".into(),
                key: "Author/Title t1/t1.m4b".into(),
            }],
            size: payload.len() as u64,
            sha256: sha2::Sha256::digest(&payload)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            content_type: Some("audio/mp4".into()),
            probe: None,
        };
        let owner_node = bookclerk_library::control_plane::load_or_create_node_identity(
            world.owner_files.path(),
        )
        .unwrap()
        .node_id;
        let UploadGrant::Granted {
            upload_id, token, ..
        } = grant_upload(
            &world.owner,
            &node_id,
            &job_id,
            claim.job.lease_generation,
            &owner_node,
            &UploadGrantRequest {
                holding_id: &holding_of(&world.owner, &book.holding_id).await,
                primary_key: "Author/Title t1/t1.m4b",
                pdf_key: None,
                files: std::slice::from_ref(&file),
                destinations: &local_scope("Author/Title t1"),
            },
        )
        .await
        .unwrap()
        else {
            panic!("granted");
        };

        let mut tcp = tokio::net::TcpStream::connect(world.addr).await.unwrap();
        let head = format!(
            "PUT /api/node/v1/uploads/{upload_id}/files/0 HTTP/1.1\r\n\
             Host: localhost:{}\r\nAuthorization: Bookclerk-Upload {}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            world.addr.port(),
            token.as_str(),
            payload.len()
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        tcp.write_all(&payload[..1 << 20]).await.unwrap();
        tcp.flush().await.unwrap();
        // The host stalls past its lease; the owner reclaims the job.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(world.owner.reclaim_expired_leases().await.unwrap(), 1);
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut answer))
            .await
            .expect("the owner answers before the body ends")
            .unwrap();
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.starts_with("HTTP/1.1 409"), "{answer}");
        assert!(answer.contains("\"lost\""), "{answer}");
        eventually("the destination to discard its partial write", || async {
            files_under(owner_out.path()).is_empty()
        })
        .await;
        assert!(authorize_upload(&world.owner, &upload_id, &token, false)
            .await
            .unwrap()
            .is_none());
        let _ = &world.state;
    }

    /// Sends `body` as declared file `index` of `upload_id` over plain
    /// HTTP; answers the raw response.
    async fn put_raw(
        addr: SocketAddr,
        upload_id: &str,
        token: &str,
        index: usize,
        body: &[u8],
    ) -> String {
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "PUT /api/node/v1/uploads/{upload_id}/files/{index} HTTP/1.1\r\n\
             Host: localhost:{}\r\nAuthorization: Bookclerk-Upload {token}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            addr.port(),
            body.len()
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        tcp.write_all(body).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// Grants an upload of one local file `key` of `bytes` for `book_uuid`
    /// in `job_id` under `fence`, in the scope of the key's folder; answers
    /// its id and token.
    async fn grant_one(
        world: &Linked,
        job_id: &str,
        fence: i64,
        book_uuid: &str,
        key: &str,
        bytes: &[u8],
    ) -> (String, String) {
        let folder = key.rsplit_once('/').map_or("", |(folder, _)| folder);
        let UploadGrant::Granted {
            upload_id, token, ..
        } = grant_upload(
            &world.owner,
            &world.link.remote_node_id,
            job_id,
            fence,
            &shared_node_id(world),
            &UploadGrantRequest {
                holding_id: &holding_of(&world.owner, book_uuid).await,
                primary_key: key,
                pdf_key: None,
                files: &[local_file(key, bytes)],
                destinations: &local_scope(folder),
            },
        )
        .await
        .unwrap()
        else {
            panic!("granted");
        };
        (upload_id, token.to_string())
    }

    /// Opens a PUT of file 0 of `upload_id` declaring `len` bytes and sends
    /// `first` of them; the rest follows on the returned stream.
    async fn put_start(
        addr: SocketAddr,
        upload_id: &str,
        token: &str,
        len: usize,
        first: &[u8],
    ) -> tokio::net::TcpStream {
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "PUT /api/node/v1/uploads/{upload_id}/files/0 HTTP/1.1\r\n\
             Host: localhost:{}\r\nAuthorization: Bookclerk-Upload {token}\r\n\
             Content-Length: {len}\r\nConnection: close\r\n\r\n",
            addr.port(),
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        tcp.write_all(first).await.unwrap();
        tcp.flush().await.unwrap();
        tcp
    }

    /// Sends `rest` of a started PUT and answers the raw response.
    async fn put_finish(mut tcp: tokio::net::TcpStream, rest: &[u8]) -> String {
        tcp.write_all(rest).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// A key taken by another title while an upload's file streams (its
    /// acquire recorded the key, then wrote it) stops the upload short of
    /// the end of its stream: it is refused `key_taken` and the other
    /// title's file is left as that acquire wrote it.
    #[tokio::test]
    async fn an_upload_stops_short_of_a_key_taken_while_it_streamed() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), local_out.path());
        let title = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        let other = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t3"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 30).await;
        const KEY: &str = "Author/Title t2/Title t2.m4b";
        let (upload_id, token) =
            grant_one(&world, &job_id, fence, &title.holding_id, KEY, b"abc").await;
        let tcp = put_start(world.addr, &upload_id, &token, 3, b"ab").await;
        eventually("the destination to start writing", || async {
            !files_under(local_out.path()).is_empty()
        })
        .await;
        take_granted_key(&world.owner, KEY, &upload_id, &other.holding_id).await;
        std::fs::write(local_out.path().join(KEY), b"xyz").unwrap();
        let answer = put_finish(tcp, b"c").await;
        assert!(answer.starts_with("HTTP/1.1 422"), "{answer}");
        assert!(answer.contains("key_taken"), "{answer}");
        assert_eq!(std::fs::read(local_out.path().join(KEY)).unwrap(), b"xyz");
        eventually("the aborted write's temp file to go", || async {
            files_under(local_out.path()).len() == 1
        })
        .await;
    }

    /// A PUT handler dropped after the whole body arrived, while it waits
    /// to finish the file (here: for the locks on its keys), commits
    /// nothing: the destinations see the stream fail, not end, and the
    /// partial write is discarded.
    #[tokio::test]
    async fn a_dropped_upload_put_commits_nothing() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), local_out.path());
        let title = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 30).await;
        const KEY: &str = "Author/Title t2/Title t2.m4b";
        let (upload_id, _) =
            grant_one(&world, &job_id, fence, &title.holding_id, KEY, b"abc").await;
        let session = bookclerk_library::control_plane::load_upload(&world.owner, &upload_id)
            .await
            .unwrap()
            .unwrap();
        let destinations = crate::node_api::uploads::owner_destinations(&world.state)
            .await
            .unwrap();
        crate::node_api::uploads::drop_tee_waiting_for_its_locks(
            world.owner.clone(),
            session,
            0,
            axum::body::Body::from("abc"),
            destinations,
        )
        .await;
        eventually("the dropped write to be discarded", || async {
            files_under(local_out.path()).is_empty()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !local_out.path().join(KEY).exists(),
            "a dropped PUT never commits its file"
        );
        assert!(load_upload_received(&world.owner, &upload_id)
            .await
            .is_empty());
    }

    /// The files `upload_id` recorded as received.
    async fn load_upload_received(store: &LibraryStore, upload_id: &str) -> Vec<usize> {
        bookclerk_library::control_plane::load_upload(store, upload_id)
            .await
            .unwrap()
            .unwrap()
            .received
            .into_iter()
            .collect()
    }

    /// A retried PUT of a file whose first attempt is still streaming
    /// waits for that attempt, then is answered from what it recorded: the
    /// two never write (or clean up) the same key at once.
    #[tokio::test]
    async fn a_retried_upload_put_waits_for_the_first_attempt() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let local_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), local_out.path());
        let title = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 30).await;
        const KEY: &str = "Author/Title t2/Title t2.m4b";
        let (upload_id, token) =
            grant_one(&world, &job_id, fence, &title.holding_id, KEY, b"abc").await;
        let first = put_start(world.addr, &upload_id, &token, 3, b"ab").await;
        eventually("the first attempt to start writing", || async {
            !files_under(local_out.path()).is_empty()
        })
        .await;
        let retry = tokio::spawn({
            let upload_id = upload_id.clone();
            let token = token.clone();
            let addr = world.addr;
            async move { put_raw(addr, &upload_id, &token, 0, b"abc").await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !retry.is_finished(),
            "the retry waits while the first attempt streams"
        );
        let answer = put_finish(first, b"c").await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        let answer = retry.await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.contains("\"received\""), "{answer}");
        assert_eq!(std::fs::read(local_out.path().join(KEY)).unwrap(), b"abc");
    }

    /// One declared file of `bytes` on the owner's local destination.
    fn local_file(key: &str, bytes: &[u8]) -> UploadFile {
        UploadFile {
            copies: vec![bookclerk_node_proto::FileCopy {
                destination: "local".into(),
                key: key.into(),
            }],
            size: bytes.len() as u64,
            sha256: sha2::Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            content_type: None,
            probe: None,
        }
    }

    /// The account holding of the owner's books row `book_uuid` (the id an
    /// upload is granted for); empty when it has none.
    async fn holding_of(store: &LibraryStore, book_uuid: &str) -> String {
        store
            .acquire_title_of_holding(book_uuid)
            .await
            .unwrap()
            .map(|title| title.holding_id)
            .unwrap_or_default()
    }

    /// The owner's own node id.
    fn shared_node_id(world: &Linked) -> String {
        bookclerk_library::control_plane::load_or_create_node_identity(world.owner_files.path())
            .unwrap()
            .node_id
    }

    /// Claims the queued acquire for the host directly, under `lease_secs`.
    async fn claim_acquire(world: &Linked, lease_secs: u64) -> i64 {
        claim_remote_job(
            &world.owner,
            &world.link.remote_node_id,
            JobResourceClass::Network,
            RemoteClaimSettings {
                lease_secs,
                limits: JobClaimLimits {
                    class: 8,
                    plugin_instance: 8,
                },
            },
            None,
        )
        .await
        .unwrap()
        .expect("the member's host may claim the acquire")
        .job
        .lease_generation
    }

    /// Revoking consent while the guest's scan is running drops the guest
    /// call itself (the stand-in ignores its cancel flag, so only the
    /// transport can stop it): the handed credentials are not used past
    /// the revoke, and the job is given back unspent.
    #[tokio::test]
    async fn a_revoke_mid_scan_drops_the_guest_call() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let guests = StubGuests {
            block_scan: true,
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (handle, _stop) = start(&world, guests.clone()).await;
        let job_id = enqueue_scan(&world.owner).await;
        eventually("the guest scan to start", || async {
            guests.entered.load(Ordering::SeqCst)
        })
        .await;
        assert!(!guests.dropped.load(Ordering::SeqCst));
        revoke_remote_plugin(&world.host, &world.link.parent_library_id, PLUGIN)
            .await
            .unwrap()
            .unwrap();
        eventually("the guest call to be dropped", || async {
            guests.dropped.load(Ordering::SeqCst)
        })
        .await;
        eventually("the job to be given back", || async {
            job(&world.owner, &job_id).await.state == JobState::Pending
                && handle.held_fences().is_empty()
        })
        .await;
        assert_eq!(job(&world.owner, &job_id).await.attempt_count, 0);
    }

    /// A title this host's own destination refused is not uploaded: the
    /// owner's destination gets nothing, the title stays unacquired, and
    /// the job fails rather than succeeding with a destination missing the
    /// title.
    #[tokio::test]
    async fn a_title_this_hosts_destination_refused_is_not_uploaded() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let host_local = tempfile::tempdir().unwrap();
        // A file where the title's folder would go: the copy fails.
        std::fs::write(host_local.path().join("Author"), b"in the way").unwrap();
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let guests = StubGuests {
            payload: Arc::new(b"audio".to_vec()),
            local: Some(host_local.path().to_path_buf()),
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests).await;
        let job_id = enqueue_acquire(&world.owner).await;
        eventually("the acquire to fail its title", || async {
            job(&world.owner, &job_id)
                .await
                .error_message
                .is_some_and(|message| message.contains("acquired=0 failed=1"))
        })
        .await;
        assert_ne!(job(&world.owner, &job_id).await.state, JobState::Succeeded);
        assert!(
            files_under(owner_out.path()).is_empty(),
            "nothing reached the owner"
        );
        let row = world
            .owner
            .acquire_title_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(row.status, bookclerk_library::AcquireStatus::Acquired);
    }

    /// An upload aborted after some of its files arrived (its fence lost
    /// before a later file) leaves none of them in the destination.
    #[tokio::test]
    async fn a_fence_lost_before_a_later_file_deletes_the_received_ones() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 2).await;
        let files = vec![
            local_file("Author/Title t1/t1.m4b", b"audio"),
            local_file("Author/Title t1/cover.jpg", b"jpeg"),
        ];
        let UploadGrant::Granted {
            upload_id, token, ..
        } = grant_upload(
            &world.owner,
            &world.link.remote_node_id,
            &job_id,
            fence,
            &shared_node_id(&world),
            &UploadGrantRequest {
                holding_id: &holding_of(&world.owner, &book.holding_id).await,
                primary_key: "Author/Title t1/t1.m4b",
                pdf_key: None,
                files: &files,
                destinations: &local_scope("Author/Title t1"),
            },
        )
        .await
        .unwrap()
        else {
            panic!("granted");
        };
        let first = put_raw(world.addr, &upload_id, &token, 0, b"audio").await;
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(owner_out.path().join("Author/Title t1/t1.m4b").is_file());

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(world.owner.reclaim_expired_leases().await.unwrap(), 1);
        let second = put_raw(world.addr, &upload_id, &token, 1, b"jpeg").await;
        assert!(second.starts_with("HTTP/1.1 409"), "{second}");
        assert!(
            files_under(owner_out.path()).is_empty(),
            "the received file is deleted with the abort: {:?}",
            files_under(owner_out.path())
        );
    }

    /// A title sent again replaces its earlier upload, and what that one
    /// had received is deleted before the new one takes files (a key the
    /// new one declares is its own now, and it writes it again).
    #[tokio::test]
    async fn a_resent_title_deletes_what_its_superseded_upload_received() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let dir = "Author/Title t1";
        let id = call_number(&world.owner, &book.holding_id).await;
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let files = vec![
            local_file(&format!("{dir}/Title t1 [{id}].m4b"), b"audio"),
            local_file(&format!("{dir}/Title t1 [{id}].jpg"), b"jpeg"),
        ];
        let UploadGrant::Granted {
            upload_id, token, ..
        } = grant_upload(
            &world.owner,
            &world.link.remote_node_id,
            &job_id,
            fence,
            &shared_node_id(&world),
            &UploadGrantRequest {
                holding_id: &holding_of(&world.owner, &book.holding_id).await,
                primary_key: &format!("{dir}/Title t1 [{id}].m4b"),
                pdf_key: None,
                files: &files,
                destinations: &local_scope(dir),
            },
        )
        .await
        .unwrap()
        else {
            panic!("granted");
        };
        let first = put_raw(world.addr, &upload_id, &token, 1, b"jpeg").await;
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(owner_out
            .path()
            .join(format!("{dir}/Title t1 [{id}].jpg"))
            .is_file());

        // The host (connected now; the job is already running) re-sends
        // the title over its socket.
        let (handle, _stop) = start(&world, StubGuests::default()).await;
        let answer = handle
            .request(Body::UploadGrant {
                job_id: job_id.clone(),
                fence,
                holding_id: holding_of(&world.owner, &book.holding_id).await,
                primary_key: format!("{dir}/Title t1 [{id}].m4b"),
                pdf_key: None,
                files: vec![files[0].clone()],
            })
            .await
            .expect("the owner answers");
        assert!(
            matches!(answer.body, Body::UploadGranted { .. }),
            "{answer:?}"
        );
        assert!(
            files_under(owner_out.path()).is_empty(),
            "the superseded upload's file is deleted"
        );
        assert!(authorize_upload(&world.owner, &upload_id, &token, false)
            .await
            .unwrap()
            .is_none());
    }

    /// A release (a consent revoke or a host giving a job back) aborts the
    /// job's open uploads and deletes what they had already received.
    #[tokio::test]
    async fn a_release_deletes_what_the_jobs_uploads_received() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let files = vec![
            local_file("Author/Title t1/t1.m4b", b"audio"),
            local_file("Author/Title t1/cover.jpg", b"jpeg"),
        ];
        let UploadGrant::Granted {
            upload_id, token, ..
        } = grant_upload(
            &world.owner,
            &world.link.remote_node_id,
            &job_id,
            fence,
            &shared_node_id(&world),
            &UploadGrantRequest {
                holding_id: &holding_of(&world.owner, &book.holding_id).await,
                primary_key: "Author/Title t1/t1.m4b",
                pdf_key: None,
                files: &files,
                destinations: &local_scope("Author/Title t1"),
            },
        )
        .await
        .unwrap()
        else {
            panic!("granted");
        };
        let first = put_raw(world.addr, &upload_id, &token, 0, b"audio").await;
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(owner_out.path().join("Author/Title t1/t1.m4b").is_file());

        let (handle, _stop) = start(&world, StubGuests::default()).await;
        let answer = handle
            .request(Body::Release {
                job_id: job_id.clone(),
                fence,
                reason: "awaiting_host_consent".into(),
                resumed: false,
            })
            .await
            .expect("the owner answers");
        assert!(matches!(answer.body, Body::Accepted {}), "{answer:?}");
        assert!(
            files_under(owner_out.path()).is_empty(),
            "the released job's received file is deleted: {:?}",
            files_under(owner_out.path())
        );
        assert!(authorize_upload(&world.owner, &upload_id, &token, false)
            .await
            .unwrap()
            .is_none());
        assert_eq!(job(&world.owner, &job_id).await.state, JobState::Pending);
    }

    /// The owner computes each title's key scope from its own row: over
    /// the socket, a grant whose key lies in another title's folder, or
    /// names a file another title has, is refused, and the title's own
    /// keys are granted.
    #[tokio::test]
    async fn the_owner_binds_a_grants_keys_to_the_granted_title() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let dir = "Author/Title t1";
        let id = call_number(&world.owner, &book.holding_id).await;
        let other = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        world
            .owner
            .set_acquire_status(
                &other.product_id,
                "riley-acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some("Author/Title t2/Title t2.m4b"),
                None,
            )
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let (handle, _stop) = start(&world, StubGuests::default()).await;
        let holding_id = holding_of(&world.owner, &book.holding_id).await;
        let grant = |files: Vec<UploadFile>| {
            let handle = handle.clone();
            let job_id = job_id.clone();
            let holding_id = holding_id.clone();
            async move {
                let primary_key = files[0].copies[0].key.clone();
                handle
                    .request(Body::UploadGrant {
                        job_id,
                        fence,
                        holding_id,
                        primary_key,
                        pdf_key: None,
                        files,
                    })
                    .await
                    .expect("the owner answers")
                    .body
            }
        };
        let refusal = |body: &Body| match body {
            Body::Error { message, .. } => message.clone(),
            other => panic!("expected a refusal: {other:?}"),
        };
        for (key, reason) in [
            ("Author/Title t2/cover.jpg", "key_out_of_scope"),
            ("Title t1.m4b", "key_out_of_scope"),
            ("../Author/Title t1/t1.m4b", "invalid_manifest"),
        ] {
            let body = grant(vec![local_file(key, b"audio")]).await;
            assert_eq!(refusal(&body), reason, "{key}");
        }
        // A file another title has, written into this title's folder by a
        // shared-folder template, is refused as well.
        world
            .owner
            .set_acquire_status(
                &other.product_id,
                "riley-acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some("Author/Title t2/Title t2 [t2].m4b"),
                None,
            )
            .await
            .unwrap();
        record_for_book(
            &world.owner,
            "local",
            &format!("{dir}/Title t1 [{id}].cue"),
            &other.holding_id,
        )
        .await
        .unwrap();
        // The folder template uses `<title short>`, so the scope treats the
        // folder as shared and only the title's planned names are in scope
        // there: another title's names are refused by name, and a planned
        // name another title holds by its claim.
        for (key, reason) in [
            (format!("{dir}/Title t2.m4b"), "key_out_of_scope"),
            (format!("{dir}/Title t2.jpg"), "key_out_of_scope"),
            (format!("{dir}/Title t1 [{id}].cue"), "key_of_another_title"),
        ] {
            let body = grant(vec![
                local_file(&format!("{dir}/Title t1 [{id}].m4b"), b"audio"),
                local_file(&key, b"other"),
            ])
            .await;
            assert_eq!(refusal(&body), reason, "{key}");
        }
        let body = grant(vec![
            local_file(&format!("{dir}/Title t1 [{id}].m4b"), b"audio"),
            local_file(&format!("{dir}/Title t1 [{id}].jpg"), b"jpeg"),
        ])
        .await;
        assert!(matches!(body, Body::UploadGranted { .. }), "{body:?}");
        // A title the plan would not hand out (its audio is acquired) is
        // refused even under its own keys, so it is never overwritten.
        let body = handle
            .request(Body::UploadGrant {
                job_id: job_id.clone(),
                fence,
                holding_id: holding_of(&world.owner, &other.holding_id).await,
                primary_key: "Author/Title t2/Title t2.m4b".into(),
                pdf_key: None,
                files: vec![local_file("Author/Title t2/Title t2.m4b", b"other")],
            })
            .await
            .expect("the owner answers")
            .body;
        assert_eq!(refusal(&body), "title_not_planned");
        assert!(files_under(owner_out.path()).is_empty());
    }

    /// A guest call that ends while the job is cancelled (a guest that
    /// stopped cooperatively and returned what it had) is answered
    /// `cancelled`, never with its result.
    #[tokio::test]
    async fn work_that_ends_after_a_cancel_is_cancelled() {
        use super::super::host_jobs::until_cancelled;

        let cancel = AtomicBool::new(false);
        let out = until_cancelled(&cancel, async {
            cancel.store(true, Ordering::SeqCst);
            Ok(vec![1, 2])
        })
        .await;
        assert!(out.is_err_and(|err| err.to_string() == "cancelled"));
        let calm = AtomicBool::new(false);
        assert_eq!(until_cancelled(&calm, async { Ok(3) }).await.unwrap(), 3);
    }

    /// Owner-supplied job and title ids become work directories only as one
    /// plain segment; anything that could leave the work dir is refused
    /// before any path is used.
    #[test]
    fn owner_ids_are_used_as_paths_only_as_one_plain_segment() {
        use super::super::host_jobs::{is_work_id, work_path, MAX_WORK_ID_BYTES};

        let root = Path::new("/files/library-work");
        assert_eq!(
            work_path(root, "acquire-0b3c6c1e-6f0e-4d7b-9f39-1a2b3c4d5e6f").unwrap(),
            root.join("acquire-0b3c6c1e-6f0e-4d7b-9f39-1a2b3c4d5e6f")
        );
        for bad in [
            "",
            ".",
            "..",
            "../escape",
            "a/../../b",
            "a/b",
            "a\\b",
            "/etc",
            "/",
            "C:",
            "C:\\Windows",
            "\\\\server\\share",
            "~",
            "a b",
            "a\0b",
            "t1.library",
        ] {
            assert!(!is_work_id(bad), "{bad:?}");
            assert!(work_path(root, bad).is_err(), "{bad:?}");
        }
        assert!(is_work_id(&"a".repeat(MAX_WORK_ID_BYTES)));
        assert!(!is_work_id(&"a".repeat(MAX_WORK_ID_BYTES + 1)));
    }

    /// `POST`s a commit of `upload_id` with `body`; answers the raw HTTP
    /// response.
    async fn commit_raw(addr: SocketAddr, upload_id: &str, token: &str, body: &str) -> String {
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /api/node/v1/uploads/{upload_id}/commit HTTP/1.1\r\n\
             Host: localhost:{}\r\nAuthorization: Bookclerk-Upload {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            addr.port(),
            body.len()
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        tcp.write_all(body.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// Grants `files` of the title `book_uuid` under `fence` directly in
    /// the owner's library; answers the upload id, token and the uploads
    /// it superseded.
    async fn grant_direct(
        world: &Linked,
        job_id: &str,
        fence: i64,
        book_uuid: &str,
        files: &[UploadFile],
        scope: &[UploadDestination],
    ) -> (
        String,
        String,
        Vec<bookclerk_library::control_plane::UploadSession>,
    ) {
        let primary_key = files.first().map_or("", |file| file.copies[0].key.as_str());
        match grant_upload(
            &world.owner,
            &world.link.remote_node_id,
            job_id,
            fence,
            &shared_node_id(world),
            &UploadGrantRequest {
                holding_id: &holding_of(&world.owner, book_uuid).await,
                primary_key,
                pdf_key: None,
                files,
                destinations: scope,
            },
        )
        .await
        .unwrap()
        {
            UploadGrant::Granted {
                upload_id,
                token,
                superseded,
                ..
            } => (upload_id, token.to_string(), superseded),
            other => panic!("expected a grant: {other:?}"),
        }
    }

    /// A cleanup of an aborted upload that runs after a later upload of the
    /// title wrote the same key (a stale abort, after the job's permits were
    /// given back) leaves the later file alone and deletes only what is
    /// still the aborted upload's own.
    #[tokio::test]
    async fn a_stale_cleanup_never_deletes_a_later_uploads_file() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let scope = local_scope("Author/Title t1");
        let old_files = vec![
            local_file("Author/Title t1/t1.m4b", b"old audio"),
            local_file("Author/Title t1/old.jpg", b"old jpeg"),
        ];
        let (old_id, old_token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &old_files, &scope).await;
        for (index, body) in [(0, &b"old audio"[..]), (1, &b"old jpeg"[..])] {
            let put = put_raw(world.addr, &old_id, &old_token, index, body).await;
            assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        }
        // The replacement claims the audio key and writes it again.
        let new_files = vec![local_file("Author/Title t1/t1.m4b", b"new audio")];
        let (new_id, new_token, superseded) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &new_files, &scope).await;
        let put = put_raw(world.addr, &new_id, &new_token, 0, b"new audio").await;
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        // Only now does the old upload's cleanup run.
        let [stale] = superseded.as_slice() else {
            panic!("the old upload is superseded: {superseded:?}");
        };
        crate::node_api::uploads::discard_received(&world.state, &world.owner, stale).await;
        assert_eq!(
            std::fs::read(owner_out.path().join("Author/Title t1/t1.m4b")).unwrap(),
            b"new audio",
            "the replacement's file stays"
        );
        assert!(
            !owner_out.path().join("Author/Title t1/old.jpg").exists(),
            "the stale upload's own file is deleted"
        );
    }

    /// The closing commit that completes the job aborts an upload of the
    /// job still open (a title re-sent but never committed) and deletes
    /// what it received, leaving the committed title's files.
    #[tokio::test]
    async fn the_closing_commit_discards_a_leftover_open_upload() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let scope = local_scope("Author/Title t1");
        let files = vec![local_file("Author/Title t1/t1.m4b", b"audio")];
        let (upload_id, token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &files, &scope).await;
        let put = put_raw(world.addr, &upload_id, &token, 0, b"audio").await;
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        let committed = commit_raw(world.addr, &upload_id, &token, "{}").await;
        assert!(committed.starts_with("HTTP/1.1 200"), "{committed}");
        // A re-send of the title that never commits.
        let extra = vec![local_file("Author/Title t1/extra.jpg", b"jpeg")];
        let (left_id, left_token, _) =
            grant_direct(&world, &job_id, fence, &book.holding_id, &extra, &scope).await;
        let put = put_raw(world.addr, &left_id, &left_token, 0, b"jpeg").await;
        assert!(put.starts_with("HTTP/1.1 200"), "{put}");
        assert!(owner_out.path().join("Author/Title t1/extra.jpg").is_file());

        let (closing_id, closing_token, _) =
            grant_direct(&world, &job_id, fence, "", &[], &[]).await;
        let closed = commit_raw(
            world.addr,
            &closing_id,
            &closing_token,
            r#"{"finish":true,"detail":"done"}"#,
        )
        .await;
        assert!(closed.starts_with("HTTP/1.1 200"), "{closed}");
        assert!(
            closed.contains("\"job_completed\":true") || closed.contains("\"jobCompleted\":true"),
            "{closed}"
        );
        assert_eq!(job(&world.owner, &job_id).await.state, JobState::Succeeded);
        assert!(
            !owner_out.path().join("Author/Title t1/extra.jpg").exists(),
            "the leftover upload's file is deleted"
        );
        assert!(owner_out.path().join("Author/Title t1/t1.m4b").is_file());
        assert!(authorize_upload(&world.owner, &left_id, &left_token, false)
            .await
            .unwrap()
            .is_none());
    }

    /// `acquire.plan` hands a host only titles whose audio is not acquired:
    /// a title whose audio is acquired but whose companion PDF is missing
    /// is not sent (a host would fetch its audio again), matching what the
    /// closing commit waits for.
    #[tokio::test]
    async fn the_plan_leaves_a_missing_pdf_to_the_owner() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        world.state.config.write().await.output.download_pdf = true;
        let fresh = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        let audio_only = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        world
            .owner
            .set_acquire_status(
                &audio_only.product_id,
                "riley-acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some("Author/Title t2/Title t2.m4b"),
                None,
            )
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let (handle, _stop) = start(&world, StubGuests::default()).await;
        let answer = handle
            .request(Body::AcquirePlan {
                job_id: job_id.clone(),
                fence,
            })
            .await
            .expect("the owner answers");
        let Body::AcquireTargets { targets, .. } = answer.body else {
            panic!("expected targets: {answer:?}");
        };
        let planned: Vec<&str> = targets.iter().map(|t| t.holding_id.as_str()).collect();
        assert_eq!(
            planned,
            vec![holding_of(&world.owner, &fresh.holding_id).await.as_str()]
        );
    }

    /// `acquire.plan` sends an episode's podcast parent row with it, so a
    /// host names the episode under the parent's folder as the owner does.
    #[tokio::test]
    async fn the_plan_sends_an_episodes_podcast_parent() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        world.state.config.write().await.library.download_episodes = true;
        let show = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&NewBook {
                content_kind: "podcast".into(),
                ..owner_book("riley-acct", "show")
            })
            .await
            .unwrap();
        let episode = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&NewBook {
                content_kind: "episode".into(),
                series_asin: Some("show".into()),
                ..owner_book("riley-acct", "ep1")
            })
            .await
            .unwrap();
        let job_id = enqueue_acquire(&world.owner).await;
        let fence = claim_acquire(&world, 60).await;
        let (handle, _stop) = start(&world, StubGuests::default()).await;
        let answer = handle
            .request(Body::AcquirePlan {
                job_id: job_id.clone(),
                fence,
            })
            .await
            .expect("the owner answers");
        let Body::AcquireTargets { targets, .. } = answer.body else {
            panic!("expected targets: {answer:?}");
        };
        let episode_holding = holding_of(&world.owner, &episode.holding_id).await;
        let target = targets
            .iter()
            .find(|t| t.holding_id == episode_holding)
            .expect("the episode is planned");
        let parent = target.parent.as_ref().expect("its parent is sent");
        assert_eq!(parent.product_id, show.product_id);
    }

    /// A title the owner refuses after this host copied it to its own
    /// destination (here: its key is another title's file) leaves no copy
    /// on this host either.
    #[tokio::test]
    async fn a_title_the_owner_refuses_is_removed_from_this_hosts_destination() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let host_local = tempfile::tempdir().unwrap();
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        // Another, already acquired title of the library wrote the file
        // this title's acquire names.
        let other = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        world
            .owner
            .set_acquire_status(
                &other.product_id,
                "riley-acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some("Author/Title t2/Title t2.m4b"),
                None,
            )
            .await
            .unwrap();
        let options = crate::node_api::uploads::owner_destinations(&world.state)
            .await
            .unwrap()
            .remove(0)
            .options;
        let short = bookclerk_acquire::call_number_of(&world.owner, &book.holding_id)
            .await
            .unwrap();
        let key = planned_key(
            (&book.product_id, &book.title, book.authors.as_deref()),
            short.as_deref(),
            &options,
        );
        record_for_book(&world.owner, "local", &key, &other.holding_id)
            .await
            .unwrap();
        let guests = StubGuests {
            payload: Arc::new(b"audio".to_vec()),
            local: Some(host_local.path().to_path_buf()),
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (_handle, _stop) = start(&world, guests).await;
        let job_id = enqueue_acquire(&world.owner).await;
        eventually("the acquire to fail its title", || async {
            job(&world.owner, &job_id)
                .await
                .error_message
                .is_some_and(|message| message.contains("acquired=0 failed=1"))
        })
        .await;
        assert!(
            files_under(host_local.path()).is_empty(),
            "the host's own copy is removed: {:?}",
            files_under(host_local.path())
        );
        assert!(files_under(owner_out.path()).is_empty());
    }

    /// Each fence generation of a job has its own scratch dir, so a stale
    /// run unwinding after its lease expired removes only its own
    /// generation's scratch, never the replacement run's on the same host.
    #[test]
    fn each_fence_generation_has_its_own_job_dir() {
        use super::super::host_jobs::job_work_path;

        let root = Path::new("/files/library-work");
        let stale = job_work_path(root, "job-1", 3).unwrap();
        let fresh = job_work_path(root, "job-1", 4).unwrap();
        assert_eq!(stale, root.join("job-1-g3"));
        assert_ne!(stale, fresh);
        assert!(!fresh.starts_with(&stale) && !stale.starts_with(&fresh));
        assert_eq!(fresh.parent(), Some(root));
        assert!(job_work_path(root, "../escape", 1).is_err());
    }

    /// A title's pipeline scratch is its own, inside the job dir the host
    /// removes when a job ends or is dropped, never the host-wide cache.
    #[test]
    fn a_titles_pipeline_scratch_sits_in_the_job_dir() {
        use super::super::host_jobs::{title_cache_dir, work_path};

        let job_dir = work_path(Path::new("/files/library-work"), "job-1").unwrap();
        let outbox = work_path(&job_dir, "t1").unwrap();
        let cache = title_cache_dir(&outbox);
        assert!(cache.starts_with(&job_dir), "{cache:?}");
        assert_ne!(cache, outbox);
        assert_ne!(cache, outbox.with_extension("library"));
        assert_ne!(cache, title_cache_dir(&work_path(&job_dir, "t2").unwrap()));
    }

    /// Each copy to this host's own destination is recorded as it is
    /// written, and an uncommitted title's copies are removed only while
    /// the fence is held: once it is lost, a replacement run may own them.
    #[tokio::test]
    async fn local_copies_are_removed_only_under_a_held_fence() {
        use super::super::host_jobs::{copy_to_local_destinations, remove_local_copies};

        let host_local = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let guests = StubGuests {
            local: Some(host_local.path().to_path_buf()),
            ..StubGuests::default()
        };
        let outbox = work.path().join("t1");
        let key = "Author/Title/Title.m4b".to_string();
        let source = destination_outbox(&outbox, 0).join(&key);
        tokio::fs::create_dir_all(source.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&source, b"audio").await.unwrap();
        let acquired = AcquiredTitle {
            primary_key: key.clone(),
            pdf_key: None,
            keys: vec![vec![key.clone()]],
        };
        let copied = Mutex::new(Vec::new());
        let placed = copy_to_local_destinations(&guests, "t1", &outbox, &acquired, &copied)
            .await
            .unwrap();
        assert_eq!(
            placed,
            vec![(
                bookclerk_config::DestinationId::new("local").unwrap(),
                key.clone()
            )]
        );
        let copies = copied.into_inner().unwrap();
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].1, key);
        let copy = host_local.path().join(&key);

        remove_local_copies("t1", &copies, true).await;
        assert!(copy.is_file(), "a lost fence leaves the copy");
        remove_local_copies("t1", &copies, false).await;
        assert!(!copy.exists(), "a held fence removes it");

        // A key already on the destination (another writer's file, or an
        // earlier acquire of the title) is not this run's to roll back.
        tokio::fs::create_dir_all(copy.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&copy, b"earlier").await.unwrap();
        let copied = Mutex::new(Vec::new());
        copy_to_local_destinations(&guests, "t1", &outbox, &acquired, &copied)
            .await
            .unwrap();
        let copies = copied.into_inner().unwrap();
        assert!(copies.is_empty(), "a key that was there is not recorded");
        remove_local_copies("t1", &copies, false).await;
        assert!(
            copy.is_file(),
            "the rollback leaves a key it did not create"
        );
    }
    /// One GET of a copy's file on the owner's listener: the status line
    /// and headers as text, and the body.
    async fn get_copy_file(
        addr: SocketAddr,
        path: &str,
        authorization: Option<&str>,
        range: Option<&str>,
    ) -> (String, Vec<u8>) {
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut head = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n",
            addr.port()
        );
        if let Some(authorization) = authorization {
            head.push_str(&format!("Authorization: {authorization}\r\n"));
        }
        if let Some(range) = range {
            head.push_str(&format!("Range: {range}\r\n"));
        }
        head.push_str("\r\n");
        tcp.write_all(head.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        let split = answer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a complete response head");
        (
            String::from_utf8_lossy(&answer[..split]).to_ascii_lowercase(),
            answer[split + 4..].to_vec(),
        )
    }

    /// A branch's own acquire is offered the copy its library holds, with
    /// the store the library downloaded it from, only while the link keeps
    /// copies on this computer and its client is connected; a copy it took
    /// is recorded as the library's, so it is never shared back.
    #[tokio::test]
    async fn an_acquire_is_offered_the_librarys_copy_only_while_the_link_keeps_copies() {
        use bookclerk_acquire::{LibraryCopies as _, LibraryCopyListing};
        use bookclerk_library::control_plane::{
            books_imported_from_link, set_library_link_keep_copies,
        };
        use sea_orm::ConnectionTrait as _;

        use super::super::library_copies::LinkCopies;

        let _dek = crate::auth::tests::process_dek_lock().await;
        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let scope = world.owner.scope(PLUGIN, "test");
        let book = scope
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        scope
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        // The library acquired t1 from the `test` store and probed it.
        const KEY: &str = "Author/Title t1/Title t1.m4b";
        let audio: Vec<u8> = (0..90_000u32).map(|i| (i % 241) as u8).collect();
        std::fs::create_dir_all(owner_out.path().join("Author/Title t1")).unwrap();
        std::fs::write(owner_out.path().join(KEY), &audio).unwrap();
        world
            .owner
            .set_acquire_status(
                "t1",
                "riley-acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some(KEY),
                None,
            )
            .await
            .unwrap();
        let owner_of = world
            .owner
            .file_owner_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert!(world
            .owner
            .record_edition_file("local", KEY, &owner_of, bookclerk_library::FileRole::Audio)
            .await
            .unwrap());
        assert!(world
            .owner
            .set_edition_file_probe(
                "local",
                KEY,
                &owner_of,
                &bookclerk_library::FileProbe {
                    content_type: "audio/mp4".into(),
                    bitrate_kbps: Some(64),
                    lossless: false,
                    source_store: Some("test".into()),
                },
            )
            .await
            .unwrap());

        let guests = StubGuests {
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (handle, _stop) = start(&world, guests).await;
        let library = world.link.parent_library_id.clone();
        let listing = |product_id: &'static str| LibraryCopyListing {
            store: "test",
            plugin_key: PLUGIN,
            account_id: "riley-acct",
            marketplace: "us",
            product_id,
        };
        let grants = || async {
            world
                .owner
                .db()
                .query_one_raw(sea_orm::Statement::from_string(
                    world.owner.db().get_database_backend(),
                    "SELECT COUNT(*) AS n FROM copy_grants",
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<i64>("", "n")
                .unwrap()
        };
        let copies = {
            let handle = handle.clone();
            LinkCopies::over(world.host.clone(), move |_| Some(handle.clone()))
        };

        // "Keep copies on this computer" is on for a new link: the copy
        // is offered, downloads whole, and is recorded once taken.
        let copy = copies
            .copy_of(listing("t1"))
            .await
            .expect("the library holds t1");
        assert_eq!(copy.source_store(), Some("test"));
        let listed: Vec<(&str, bookclerk_library::FileRole, u64)> = copy
            .files()
            .iter()
            .map(|file| (file.name.as_str(), file.role, file.size))
            .collect();
        assert_eq!(
            listed,
            [("Title t1.m4b", bookclerk_library::FileRole::Audio, 90_000)]
        );
        let taken = tempfile::tempdir().unwrap();
        let dest = taken.path().join("000.m4b");
        copy.fetch_file(0, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), audio);
        assert!(copy
            .fetch_file(1, &taken.path().join("none"))
            .await
            .is_err());
        assert!(books_imported_from_link(&world.host, &library)
            .await
            .unwrap()
            .is_empty());
        // The title is on a store account connected to the branch.
        world
            .host
            .upsert_account(PLUGIN, "riley-acct", "us", None, true, PLUGIN)
            .await
            .unwrap();
        copy.taken().await.unwrap();
        assert_eq!(
            books_imported_from_link(&world.host, &library)
                .await
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            [(
                PLUGIN.to_string(),
                "us".to_string(),
                "t1".to_string(),
                "riley-acct".to_string()
            )]
        );

        // A title the library has not acquired is not offered.
        assert!(copies.copy_of(listing("t2")).await.is_none());

        // With the setting off, or with no connected client, the library
        // is not even asked.
        let asked = grants().await;
        assert_eq!(asked, 1);
        assert!(set_library_link_keep_copies(&world.host, &library, false)
            .await
            .unwrap());
        assert!(copies.copy_of(listing("t1")).await.is_none());
        assert!(set_library_link_keep_copies(&world.host, &library, true)
            .await
            .unwrap());
        let no_client = LinkCopies::over(world.host.clone(), |_| None);
        assert!(no_client.copy_of(listing("t1")).await.is_none());
        assert_eq!(grants().await, asked);
        // On again, it is.
        assert!(copies.copy_of(listing("t1")).await.is_some());
        assert_eq!(grants().await, asked + 1);

        // A copy the library downloaded from another store than the
        // title's is offered with what the library's probe found, so the
        // acquire can compare it with its own download. It is offered
        // only because no library has a copy from the title's own store.
        let reprobe = |source_store: Option<&'static str>| {
            let owner_of = owner_of.clone();
            let owner = world.owner.clone();
            async move {
                assert!(owner
                    .set_edition_file_probe(
                        "local",
                        KEY,
                        &owner_of,
                        &bookclerk_library::FileProbe {
                            content_type: "audio/mp4".into(),
                            bitrate_kbps: Some(64),
                            lossless: false,
                            source_store: source_store.map(str::to_string),
                        },
                    )
                    .await
                    .unwrap());
            }
        };
        reprobe(Some("elsewhere")).await;
        let other = copies
            .copy_of(listing("t1"))
            .await
            .expect("a copy from another store is offered for comparison");
        assert_eq!(other.source_store(), Some("elsewhere"));
        let probe = other.probe();
        assert_eq!(
            (
                probe.content_type.as_str(),
                probe.bitrate_kbps,
                probe.lossless
            ),
            ("audio/mp4", Some(64), false)
        );
        assert_eq!(grants().await, asked + 2);

        // A copy whose store the library does not know is granted but
        // never offered: there is nothing to say whose download it is.
        reprobe(None).await;
        assert!(copies.copy_of(listing("t1")).await.is_none());
        assert_eq!(grants().await, asked + 3);
    }

    /// A branch takes the copy its library already holds: `copy.grant`
    /// answers what the copy is and its files with their sizes, each file
    /// downloads whole or by range with the sealed token, and nothing
    /// else opens them. The download stops with the branch's trust, and at
    /// TRACE the token reaches no log.
    #[tokio::test]
    async fn a_branch_downloads_the_copy_its_library_holds() {
        use bookclerk_node_proto::{CopyRole, COPIES_PATH};

        let _dek = crate::auth::tests::process_dek_lock().await;
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        let _ = tracing_log::LogTracer::init();

        let world = linked().await;
        let owner_out = tempfile::tempdir().unwrap();
        crate::node_api::uploads::set_test_destination(world.owner_files.path(), owner_out.path());
        let book = world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t1"))
            .await
            .unwrap();
        world
            .owner
            .scope(PLUGIN, "test")
            .upsert_book(&owner_book("riley-acct", "t2"))
            .await
            .unwrap();
        // The library acquired t1: its audio and a cover lie on the
        // destination and are recorded as the edition's files.
        const KEY: &str = "Author/Title t1/Title t1.m4b";
        const COVER: &str = "Author/Title t1/cover.jpg";
        let audio: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::create_dir_all(owner_out.path().join("Author/Title t1")).unwrap();
        std::fs::write(owner_out.path().join(KEY), &audio).unwrap();
        std::fs::write(owner_out.path().join(COVER), b"").unwrap();
        world
            .owner
            .set_acquire_status(
                "t1",
                "riley-acct",
                bookclerk_library::AcquireStatus::Acquired,
                Some(KEY),
                None,
            )
            .await
            .unwrap();
        let owner_of = world
            .owner
            .file_owner_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        for (key, role) in [
            (KEY, bookclerk_library::FileRole::Audio),
            (COVER, bookclerk_library::FileRole::Cover),
        ] {
            assert!(world
                .owner
                .record_edition_file("local", key, &owner_of, role)
                .await
                .unwrap());
        }

        let guests = StubGuests {
            work: world.host_files.path().join("library-work"),
            ..StubGuests::default()
        };
        let (handle, _stop) = start(&world, guests).await;

        // Not acquired, and not in the library at all: one answer.
        assert!(handle
            .copy_grant("test", "us", "t2")
            .await
            .unwrap()
            .is_none());
        assert!(handle
            .copy_grant("test", "us", "nope")
            .await
            .unwrap()
            .is_none());
        assert!(handle
            .copy_grant("test", "uk", "t1")
            .await
            .unwrap()
            .is_none());
        // A malformed ask is not answered as a copy.
        assert!(matches!(
            handle.copy_grant("Not A Store", "us", "t1").await,
            Err(bookclerk_node_client::OwnerCallError::Unavailable)
        ));

        let granted = handle
            .copy_grant("test", "us", "t1")
            .await
            .unwrap()
            .expect("the library holds t1");
        assert_eq!(granted.copy.content_type, "audio/mp4");
        let listed: Vec<(&str, CopyRole, u64)> = granted
            .files
            .iter()
            .map(|file| (file.name.as_str(), file.role, file.size))
            .collect();
        assert_eq!(
            listed,
            [
                ("Title t1.m4b", CopyRole::Audio, 200_000),
                ("cover.jpg", CopyRole::Cover, 0),
            ]
        );
        let authorization = granted.authorization().unwrap();
        let token = authorization
            .strip_prefix("Bookclerk-Copy ")
            .unwrap()
            .to_string();
        let file = |n: usize| format!("{COPIES_PATH}{}/files/{n}", granted.copy_id);

        // The whole file, then ranges of it.
        let (head, body) = get_copy_file(world.addr, &file(0), Some(&authorization), None).await;
        assert!(head.starts_with("http/1.1 200"), "{head}");
        assert!(head.contains("content-length: 200000"), "{head}");
        assert!(head.contains("accept-ranges: bytes"), "{head}");
        assert!(head.contains("cache-control: no-store"), "{head}");
        assert_eq!(body, audio);
        for (range, status, content_range, want) in [
            ("bytes=5-9", "206", "bytes 5-9/200000", &audio[5..10]),
            (
                "bytes=199990-",
                "206",
                "bytes 199990-199999/200000",
                &audio[199_990..],
            ),
            (
                "bytes=-3",
                "206",
                "bytes 199997-199999/200000",
                &audio[199_997..],
            ),
            (
                "bytes=100000-999999",
                "206",
                "bytes 100000-199999/200000",
                &audio[100_000..],
            ),
            // Not a range this endpoint reads: the whole file.
            ("bytes=0-1,5-6", "200", "", &audio[..]),
        ] {
            let (head, body) =
                get_copy_file(world.addr, &file(0), Some(&authorization), Some(range)).await;
            assert!(
                head.starts_with(&format!("http/1.1 {status}")),
                "{range}: {head}"
            );
            assert_eq!(
                head.contains("content-range"),
                !content_range.is_empty(),
                "{range}: {head}"
            );
            assert!(head.contains(content_range), "{range}: {head}");
            assert_eq!(body, want, "{range}");
        }
        let (head, body) = get_copy_file(
            world.addr,
            &file(0),
            Some(&authorization),
            Some("bytes=200000-"),
        )
        .await;
        assert!(head.starts_with("http/1.1 416"), "{head}");
        assert!(head.contains("content-range: bytes */200000"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("range_not_satisfiable"));
        // The empty cover.
        let (head, body) = get_copy_file(world.addr, &file(1), Some(&authorization), None).await;
        assert!(head.starts_with("http/1.1 200"), "{head}");
        assert!(head.contains("content-length: 0"), "{head}");
        assert!(body.is_empty());

        // The link's own download brings each file to disk, resumes one
        // that was cut short, and asks nothing for a file the grant does
        // not list.
        let taken = tempfile::tempdir().unwrap();
        let dest = taken.path().join("audio.part");
        handle.fetch_copy_file(&granted, 0, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), audio);
        std::fs::write(&dest, &audio[..70_000]).unwrap();
        handle.fetch_copy_file(&granted, 0, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), audio);
        let cover = taken.path().join("cover.part");
        handle.fetch_copy_file(&granted, 1, &cover).await.unwrap();
        assert!(std::fs::read(&cover).unwrap().is_empty());
        assert!(matches!(
            handle
                .fetch_copy_file(&granted, 2, &taken.path().join("none"))
                .await,
            Err(bookclerk_node_client::CopyFetchError::NoSuchFile)
        ));
        assert!(!taken.path().join("none").exists());

        let (head, body) = get_copy_file(world.addr, &file(2), Some(&authorization), None).await;
        assert!(head.starts_with("http/1.1 404"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("unknown_file"));
        let (head, _) =
            get_copy_file(world.addr, &file(usize::MAX), Some(&authorization), None).await;
        assert!(head.starts_with("http/1.1 404"), "{head}");
        let other = format!("{COPIES_PATH}00000000-0000-0000-0000-000000000000/files/0");
        let upload_scheme = format!("Bookclerk-Upload {token}");
        for (path, authorization) in [
            (file(0), None),
            (file(0), Some(upload_scheme.as_str())),
            (file(0), Some("Bookclerk-Copy nope")),
            (other, Some(authorization.as_str())),
        ] {
            let (head, body) = get_copy_file(world.addr, &path, authorization, None).await;
            assert!(
                head.starts_with("http/1.1 401"),
                "{path} {authorization:?}: {head}"
            );
            assert!(!body.windows(8).any(|window| window == &audio[..8]));
        }

        // Granted by another node: this one does not serve it.
        {
            use sea_orm::ConnectionTrait as _;
            world
                .owner
                .db()
                .execute_unprepared("UPDATE copy_grants SET serving_node_id = 'another-node'")
                .await
                .unwrap();
            let (head, _) = get_copy_file(world.addr, &file(0), Some(&authorization), None).await;
            assert!(head.starts_with("http/1.1 421"), "{head}");
            // The branch's trust ends: the same token opens nothing.
            world
                .owner
                .db()
                .execute_unprepared(
                    "UPDATE node_credentials SET revoked_at = '2026-01-01T00:00:00Z'",
                )
                .await
                .unwrap();
        }
        let (head, _) = get_copy_file(world.addr, &file(0), Some(&authorization), None).await;
        assert!(head.starts_with("http/1.1 401"), "{head}");

        let logged = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
        assert!(logged.contains("granted a copy of an edition"), "{logged}");
        assert!(!logged.contains(&token), "the copy token reached a log");
    }
}

/// A database swap stops the client of the previous store's link and
/// starts one for the new store's link.
#[tokio::test]
async fn a_database_swap_restarts_library_links_on_the_new_store() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let swapped_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let (member, _token) = owner_account(&owner, b"riley-token-v1").await;

    // The host daemon, enrolled once into its first store.
    let (host_state, first_store) = crate::node_api::tests::test_state(&host_files).await;
    let first = join(&owner, &first_store, member, &origin, None)
        .await
        .unwrap();
    super::spawn_library_links(Arc::clone(&host_state));
    eventually("the first store's link to connect", || async {
        crate::node_api::ws::node_session_live(&first.remote_node_id)
    })
    .await;

    // A config reload swaps in a store that holds another link.
    let second_store = host_store(swapped_files.path()).await;
    let second = join(&owner, &second_store, member, &origin, None)
        .await
        .unwrap();
    *host_state.library.write().await = second_store;
    host_state.library_swaps.send_modify(|swaps| *swaps += 1);
    eventually("the new store's link to connect", || async {
        crate::node_api::ws::node_session_live(&second.remote_node_id)
    })
    .await;
    eventually("the previous store's link to disconnect", || async {
        !crate::node_api::ws::node_session_live(&first.remote_node_id)
    })
    .await;
    // It stays down: nothing dials the previous store's link again.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!crate::node_api::ws::node_session_live(
        &first.remote_node_id
    ));
}

/// `bookclerk libraries leave` against a running daemon: the watcher stops
/// the link's client once the link is no longer active, and on the store's
/// shutdown (a database swap) stops the client too and returns.
#[tokio::test]
async fn leaving_stops_the_running_client() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    // The owner's test state configures the process DEK the link seals with.
    let owner_files = tempfile::tempdir().unwrap();
    let _owner = crate::node_api::tests::test_state(&owner_files).await;
    let host_files = tempfile::tempdir().unwrap();
    let host = host_store(host_files.path()).await;
    bookclerk_library::control_plane::record_library_link(
        &host,
        &bookclerk_library::control_plane::NewLibraryLink {
            parent_library_id: "library-leave".into(),
            owner_origin: "https://owner.example".into(),
            remote_node_id: "node-leave".into(),
            spki_pin: None,
        },
        &bookclerk_library::control_plane::LibraryLinkKeys::generate(),
    )
    .await
    .unwrap();

    // Shutdown first: the watcher forwards it to the client and returns.
    let (stop, mut stopped) = watch::channel(false);
    let stop = Arc::new(stop);
    let (shut, shutdown) = watch::channel(false);
    let watcher = tokio::spawn(super::stop_when_inactive(
        host.clone(),
        "library-leave".into(),
        "node-leave".into(),
        Arc::clone(&stop),
        shutdown,
    ));
    shut.send(true).unwrap();
    let why = tokio::time::timeout(Duration::from_secs(5), watcher)
        .await
        .expect("the watcher returns on shutdown")
        .unwrap();
    assert_eq!(why, super::LinkStop::Shutdown);
    assert!(*stopped.borrow_and_update(), "shutdown stops the client");

    // While active, nothing stops; after leaving, the client is stopped.
    let (stop, mut stopped) = watch::channel(false);
    let stop = Arc::new(stop);
    let (_shut, shutdown) = watch::channel(false);
    let watcher = tokio::spawn(super::stop_when_inactive(
        host.clone(),
        "library-leave".into(),
        "node-leave".into(),
        Arc::clone(&stop),
        shutdown,
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), stopped.changed())
            .await
            .is_err(),
        "an active link keeps its client"
    );
    bookclerk_library::control_plane::leave_library_link(&host, "library-leave", None)
        .await
        .unwrap()
        .expect("the link");
    tokio::time::timeout(super::consent::CONSENT_POLL * 3, stopped.changed())
        .await
        .expect("the client is stopped after leaving")
        .unwrap();
    assert!(*stopped.borrow());
    assert_eq!(watcher.await.unwrap(), super::LinkStop::Inactive);
}

/// A link re-enrolled as another node (`bookclerk libraries join` again)
/// stops the client running as the previous node, and says so.
#[tokio::test]
async fn a_rejoin_as_another_node_stops_the_previous_client() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let _owner = crate::node_api::tests::test_state(&owner_files).await;
    let host_files = tempfile::tempdir().unwrap();
    let host = host_store(host_files.path()).await;
    let enroll = |node: &str| bookclerk_library::control_plane::NewLibraryLink {
        parent_library_id: "library-rejoin".into(),
        owner_origin: "https://owner.example".into(),
        remote_node_id: node.into(),
        spki_pin: None,
    };
    let keys = bookclerk_library::control_plane::LibraryLinkKeys::generate;
    bookclerk_library::control_plane::record_library_link(&host, &enroll("node-old"), &keys())
        .await
        .unwrap();

    let (stop, mut stopped) = watch::channel(false);
    let (_shut, shutdown) = watch::channel(false);
    let watcher = tokio::spawn(super::stop_when_inactive(
        host.clone(),
        "library-rejoin".into(),
        "node-old".into(),
        Arc::new(stop),
        shutdown,
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), stopped.changed())
            .await
            .is_err(),
        "the link as enrolled keeps its client"
    );
    bookclerk_library::control_plane::record_library_link(&host, &enroll("node-new"), &keys())
        .await
        .unwrap();
    tokio::time::timeout(super::consent::CONSENT_POLL * 3, stopped.changed())
        .await
        .expect("the previous node's client is stopped")
        .unwrap();
    assert!(*stopped.borrow());
    assert_eq!(watcher.await.unwrap(), super::LinkStop::Replaced);
}

/// Through the running supervisor: after the host joins the same library
/// again, the client of the previous node disconnects and one for the new
/// node connects, without a restart or a database swap.
#[tokio::test]
async fn a_rejoined_link_is_dialed_as_the_new_node() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let (member, _token) = owner_account(&owner, b"riley-token-v1").await;

    let (host_state, host) = crate::node_api::tests::test_state(&host_files).await;
    let first = join(&owner, &host, member, &origin, None).await.unwrap();
    super::spawn_library_links(Arc::clone(&host_state));
    eventually("the first node to connect", || async {
        crate::node_api::ws::node_session_live(&first.remote_node_id)
    })
    .await;

    let second = join(&owner, &host, member, &origin, None).await.unwrap();
    assert_eq!(second.parent_library_id, first.parent_library_id);
    assert_ne!(second.remote_node_id, first.remote_node_id);
    eventually("the new node to connect", || async {
        crate::node_api::ws::node_session_live(&second.remote_node_id)
    })
    .await;
    eventually("the previous node to disconnect", || async {
        !crate::node_api::ws::node_session_live(&first.remote_node_id)
    })
    .await;
}

/// A link's supervisor and watcher are aborted with their client's task:
/// dropping the guard (as aborting the task that holds it does) aborts the
/// guarded task, and so does dropping a [`super::join_or_abort`] wait.
#[tokio::test]
async fn a_guarded_task_is_aborted_with_its_guard() {
    /// Waits until `task` finished.
    async fn finished(task: &tokio::task::AbortHandle) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the guarded task is aborted");
    }
    let guard = AbortOnDrop(Some(tokio::spawn(std::future::pending::<()>())));
    let aborted = guard
        .0
        .as_ref()
        .map(tokio::task::JoinHandle::abort_handle)
        .unwrap();
    drop(guard);
    finished(&aborted).await;
    let pending = tokio::spawn(std::future::pending::<()>());
    let waited = pending.abort_handle();
    let wait = tokio::spawn(super::join_or_abort(pending));
    tokio::task::yield_now().await;
    assert!(!waited.is_finished());
    wait.abort();
    finished(&waited).await;
}

/// Records `key` on `destination` as a local acquire of `book_uuid`'s
/// edition wrote it.
/// Gives back the claim upload `upload_id` holds on `key` on the local
/// destination, then records `key` for `book_uuid`'s edition: another
/// title's acquire taking a key once the grant no longer holds it.
async fn take_granted_key(
    store: &bookclerk_library::LibraryStore,
    key: &str,
    upload_id: &str,
    book_uuid: &str,
) {
    store
        .release_edition_file("local", key, upload_id, None)
        .await
        .unwrap();
    record_for_book(store, "local", key, book_uuid)
        .await
        .unwrap();
}

async fn record_for_book(
    store: &bookclerk_library::LibraryStore,
    destination: &str,
    key: &str,
    book_uuid: &str,
) -> bookclerk_library::Result<()> {
    let owner = store
        .file_owner_of_holding(book_uuid)
        .await?
        .expect("the book resolves to an edition");
    store
        .record_edition_file(
            destination,
            key,
            &owner,
            bookclerk_library::FileRole::for_key(key, None),
        )
        .await
        .map(|recorded| assert!(recorded, "{key} is another edition's"))
}

/// A deferred plugin-state purge is cancelled once the store it read is
/// swapped out (its stop flag turns true) or its supervisor is gone, so it
/// never acts on a store that no longer owns the links.
#[test]
fn a_deferred_purge_is_cancelled_when_its_store_stops() {
    let (stop, stopped) = watch::channel(false);
    assert!(!super::purge_cancelled(&stopped));
    stop.send(true).ok();
    assert!(super::purge_cancelled(&stopped));

    let (stop, stopped) = watch::channel(false);
    drop(stop);
    assert!(super::purge_cancelled(&stopped));
}

/// A library this install left loses its plugin state, a library it is
/// still a member of keeps it, and a deferred purge whose store was
/// swapped out deletes nothing.
#[tokio::test]
async fn departed_libraries_lose_their_plugin_state() {
    use bookclerk_library::control_plane::{
        leave_library_link, list_library_links, record_library_link, LibraryLinkKeys,
        NewLibraryLink,
    };
    use bookclerk_plugin_host::LinkStateDir;

    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store) = crate::node_api::tests::test_state(&files).await;
    let state_dir = |library: &str| {
        files
            .path()
            .join("plugin-state")
            .join(LinkStateDir::for_library(library).as_str())
    };
    for (library, node) in [("library-left", "node-left"), ("library-kept", "node-kept")] {
        record_library_link(
            &store,
            &NewLibraryLink {
                parent_library_id: library.into(),
                owner_origin: "https://owner.example".into(),
                remote_node_id: node.into(),
                spki_pin: None,
            },
            &LibraryLinkKeys::generate(),
        )
        .await
        .unwrap();
        std::fs::create_dir_all(state_dir(library).join("plugin").join("data")).unwrap();
    }
    leave_library_link(&store, "library-left", None)
        .await
        .unwrap()
        .expect("the link");

    // A deferred purge whose store already stopped returns without deleting.
    let (stop, stopped) = watch::channel(true);
    super::purge_after_grace(
        Arc::clone(&state),
        store.clone(),
        "library-left".into(),
        stopped,
    )
    .await;
    drop(stop);
    assert!(
        state_dir("library-left").exists(),
        "a cancelled purge keeps the state"
    );

    let links = list_library_links(&store).await.unwrap();
    super::purge_departed_link_state(&state, &store, &links).await;
    assert!(
        !state_dir("library-left").exists(),
        "a departed library's state goes"
    );
    assert!(
        state_dir("library-kept").join("plugin").exists(),
        "an active library's state stays"
    );
}

/// "Keep copies on this computer" off changes nothing about the library's
/// work: the consented plugin stays and the branch keeps claiming jobs.
#[tokio::test]
async fn a_branch_that_keeps_no_copies_still_runs_its_librarys_work() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let owner_files = tempfile::tempdir().unwrap();
    let host_files = tempfile::tempdir().unwrap();
    let (state, owner) = crate::node_api::tests::test_state(&owner_files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    let origin = public_origin(&state, &format!("http://127.0.0.1:{}", addr.port())).await;
    let host = host_store(host_files.path()).await;
    let (admin, _token) =
        owner_account_as(&owner, b"riley-token-v1", UserRole::Administrator).await;
    let link = join(&owner, &host, admin, &origin, None).await.unwrap();
    let node_id = link.remote_node_id.clone();
    let library_id = link.parent_library_id.clone();
    deploy_store(&owner, owner_files.path(), &node_id).await;
    let first = enqueue_scan(&owner).await;
    let transport = EchoTransport::default();
    let (handle, _stop) = start_client(
        &host,
        host_files.path(),
        link,
        transport.clone(),
        Duration::from_millis(200),
    )
    .await;
    connected(&handle, 1).await;
    approve_store(&host, &library_id).await;
    eventually("the scan to finish on the host", || async {
        job(&owner, &first).await.state == JobState::Succeeded
    })
    .await;

    assert!(
        bookclerk_library::control_plane::set_library_link_keep_copies(&host, &library_id, false)
            .await
            .unwrap()
    );
    notify_consent_changed();
    let second = enqueue_scan(&owner).await;
    eventually("the second scan to finish with copies off", || async {
        job(&owner, &second).await.state == JobState::Succeeded
    })
    .await;
    assert!(!handle.consented_plugins().is_empty());
    assert_eq!(transport.runs().len(), 2);
}

/// The owner's grant and a host's acquire name a title from the same
/// fields: an acquire request built from the owner's holding view and one
/// built from the host's scratch row of the target the owner sent agree,
/// and the target's dates survive the trip (a malformed one is dropped).
#[tokio::test]
async fn the_grant_and_the_host_name_a_title_from_the_same_fields() {
    use super::host_jobs::{acquire_request, scratch_book};
    let dir = tempfile::tempdir().unwrap();
    let owner = host_store(dir.path()).await;
    owner
        .upsert_account("audible", "riley-acct", "us", None, true, "audible")
        .await
        .unwrap();
    let mut book = bookclerk_library::NewBook::minimal("B0NAMES01", "riley-acct", "us", "Dune");
    book.authors = Some("Frank Herbert".into());
    book.narrators = Some("Scott Brick".into());
    book.series = Some("Dune Chronicles".into());
    book.series_index = Some("1".into());
    let row = owner.upsert_book(&book).await.unwrap();
    let holding = owner
        .acquire_title_of_holding(&row.holding_id)
        .await
        .unwrap()
        .expect("the title's holding")
        .holding_id;
    let title = owner
        .acquire_title_of_holding(&holding)
        .await
        .unwrap()
        .expect("the holding's title");
    let files = tempfile::tempdir().unwrap();
    std::fs::write(files.path().join("config.toml"), "").unwrap();
    let cfg = bookclerk_config::Config::load(
        Some(files.path().to_path_buf()),
        Some(files.path().join("config.toml")),
    )
    .unwrap();
    let options = bookclerk_source::DownloadOptions::default();
    let granted = acquire_request(&cfg, (&title).into(), &options);

    let target = bookclerk_node_proto::TargetTitle {
        product_id: title.product_id.clone(),
        source: title.source.clone(),
        marketplace: title.marketplace.clone(),
        asin: title.asin.clone(),
        isbn: None,
        title: title.title.clone(),
        subtitle: None,
        authors: title.authors.clone(),
        narrators: title.narrators.clone(),
        series: title.series.clone(),
        series_index: title.series_index.clone(),
        series_product_id: None,
        publisher: None,
        categories: None,
        length_minutes: Some(1260),
        is_abridged: false,
        content_kind: title.content_kind.to_string(),
        purchased_at: Some("not a date".into()),
        published_at: Some("2020-01-02T03:04:05+00:00".into()),
    };
    let insert = scratch_book(&title.plugin_key, &title.account_id, &target);
    assert_eq!(insert.series_asin, None);
    assert_eq!(insert.purchased_at, None, "a malformed date is left out");
    assert_eq!(
        insert.published_at.map(|at| at.to_rfc3339()),
        Some("2020-01-02T03:04:05+00:00".into())
    );
    let scratch_dir = tempfile::tempdir().unwrap();
    let scratch = host_store(scratch_dir.path()).await;
    scratch
        .scope(insert.plugin_key.as_str(), insert.source.as_str())
        .ensure_account(&insert.account_id, &insert.marketplace, None)
        .await
        .unwrap();
    let scratch_row = scratch.upsert_book(&insert).await.unwrap();
    let hosted = acquire_request(&cfg, (&scratch_row).into(), &options);
    let naming = |r: &bookclerk_acquire::AcquireRequest| {
        (
            r.asin.clone(),
            r.source.clone(),
            r.account_id.clone(),
            r.title.clone(),
            r.authors.clone(),
            r.narrators.clone(),
            r.series.clone(),
            r.series_index.clone(),
        )
    };
    assert_eq!(naming(&granted), naming(&hosted));
    // The owner records under its holding, and the host under its scratch
    // library's own holding of the title.
    assert_eq!(granted.holding_id.as_deref(), Some(holding.as_str()));
    assert_eq!(
        hosted.holding_id.as_deref(),
        Some(scratch_row.holding_id.as_str())
    );
}

/// A title's target set on the host is the plan's destinations its list
/// names, in the plan's order, or every one without a list; an empty list
/// or one that does not fit the plan fails the title.
#[test]
fn the_host_writes_a_title_only_to_its_target_set() {
    use super::host_jobs::{target_set, HostDestination};
    use bookclerk_node_proto::{AcquireTarget, PlanDestination};

    let planned: Vec<PlanDestination> = ["local", "nas"]
        .into_iter()
        .map(|id| PlanDestination {
            id: id.into(),
            options: serde_json::to_value(bookclerk_source::DownloadOptions::default()).unwrap(),
        })
        .collect();
    let parsed: Vec<HostDestination> = planned
        .iter()
        .cloned()
        .map(|plan| HostDestination::from_plan(plan).unwrap())
        .collect();
    let target = |ids: Option<Vec<&str>>| -> AcquireTarget {
        let mut value = serde_json::json!({
            "holding_id": "b",
            "plugin_key": "store",
            "account_id": "a",
            "title": {"product_id": "p", "source": "store", "marketplace": "us", "title": "T", "content_kind": "book"},
        });
        if let Some(ids) = ids {
            value["destinations"] = serde_json::json!(ids);
        }
        serde_json::from_value(value).unwrap()
    };
    let ids = |set: Vec<HostDestination>| -> Vec<String> {
        set.into_iter().map(|d| d.id.to_string()).collect()
    };
    assert_eq!(
        ids(target_set(&target(None), &planned, &parsed).unwrap()),
        ["local", "nas"]
    );
    assert_eq!(
        ids(target_set(&target(Some(vec!["nas"])), &planned, &parsed).unwrap()),
        ["nas"]
    );
    let none = target_set(&target(Some(vec![])), &planned, &parsed).unwrap_err();
    assert!(
        none.to_string().contains("no destination accepts"),
        "{none}"
    );
    assert!(target_set(&target(Some(vec!["s3"])), &planned, &parsed).is_err());
    assert!(target_set(&target(Some(vec!["nas", "local"])), &planned, &parsed).is_err());
}

/// An upload's destinations are taken from this node's in the grant's
/// order; one no longer deployed (or named twice) gives none.
#[test]
fn an_uploads_destinations_come_back_in_grant_order_or_not_at_all() {
    use crate::node_api::uploads::{destination_ids, granted_destinations, OwnerDestination};

    let root = tempfile::tempdir().unwrap();
    let deployed = || -> Vec<OwnerDestination> {
        ["local", "nas", "s3"]
            .into_iter()
            .map(|id| OwnerDestination {
                id: id.into(),
                backend: Box::new(
                    bookclerk_storage::LocalFsBackend::new(root.path().to_path_buf()).unwrap(),
                ),
                options: bookclerk_source::DownloadOptions::default(),
            })
            .collect()
    };
    let granted = |ids: &[&str]| -> Vec<String> { ids.iter().map(|id| id.to_string()).collect() };
    let picked = granted_destinations(deployed(), &granted(&["s3", "local"])).unwrap();
    assert_eq!(destination_ids(&picked), ["s3", "local"]);
    assert!(granted_destinations(deployed(), &granted(&["r2"])).is_none());
    assert!(granted_destinations(deployed(), &granted(&["nas", "nas"])).is_none());
}
