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
    ensure_plugin_deployment, load_cluster_link, load_cluster_link_keys, load_instance_config,
    load_node_status, load_observation, load_or_create_node_identity, load_remote_plugin_consent,
    mint_enrollment_link, node_queue_name, publish_node_message, replace_instance_config,
    revoke_remote_plugin, seal_instance_secret, set_secret_ref, ClusterLink, ConfigActor,
    DeploymentStatus, DesiredState, EnrollmentActor, NodeHeartbeat, NodeMessage,
    PluginInstanceConfigV1, PluginInstanceId, RemoteApprover, SettingValue,
};
use bookclerk_library::secrets::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret, upsert_secret,
};
use bookclerk_library::{
    EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, JobState, LibraryStore, UserRole,
};
use bookclerk_node_client::{
    join_cluster, ClientTiming, DialError, HeartbeatSource, HostClient, HostClientConfig,
    HostClientHandle, JoinError, OwnerEndpoint,
};
use bookclerk_node_proto::{SecretRef, ARTIFACTS_PATH, CLOSE_SUPERSEDED};
use bookclerk_plugin_host::{approve_remote_request, PluginGrant, RemoteNarrowing};
use tokio::sync::watch;

use super::consent::{notify_consent_changed, LinkSupervisor};
use super::remote_bindings::RemoteCatalog;
use super::TransportRunner;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// Plugin key of the owner's store account.
const PLUGIN: &str = "path:file:///tmp/store";

/// Serves `app` on a loopback port, as the daemon's listeners do.
async fn serve(app: axum::Router) -> SocketAddr {
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
async fn host_store(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .unwrap();
    bookclerk_library::apply_host_schema(&db).await.unwrap();
    LibraryStore::from_connection(db)
}

/// Fixed heartbeat measurements.
struct FixedSampler;

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
        }
    }
}

/// Fast timers for tests.
fn timing(backoff_min: Duration) -> ClientTiming {
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
async fn owner_account(store: &LibraryStore, token: &[u8]) -> (i64, SecretRef) {
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
async fn public_origin(state: &Arc<crate::api::AppState>, origin: &str) -> String {
    let mut cfg = state.config.write().await;
    cfg.integrations.public_origin = Some(origin.to_string());
    crate::origin::effective_origin_from_config(&cfg, None)
}

/// Mints a link for `member` at `origin` and joins it from the host.
async fn join(
    owner: &LibraryStore,
    host: &LibraryStore,
    member: i64,
    origin: &str,
    pin: Option<&str>,
) -> Result<ClusterLink, JoinError> {
    let minted = mint_enrollment_link(
        owner,
        &EnrollmentActor::Operator { id: "test".into() },
        member,
        "",
        None,
    )
    .await
    .unwrap();
    join_cluster(host, &minted.url(origin, pin), "1.0.0").await
}

/// Starts a host client for `link` running `transport` (claiming scans),
/// with its link supervisor keeping the catalog and consent current.
async fn start_client(
    host: &LibraryStore,
    host_files: &std::path::Path,
    link: ClusterLink,
    transport: EchoTransport,
    backoff_min: Duration,
) -> (HostClientHandle, watch::Sender<bool>) {
    let keys = load_cluster_link_keys(host, &link).await.unwrap();
    let catalog = Arc::new(RemoteCatalog::default());
    let (client, handle) = HostClient::new(
        HostClientConfig {
            link,
            keys,
            software_version: "1.0.0".into(),
            incarnation: uuid::Uuid::new_v4().to_string(),
            max_jobs: 1,
            claim_kinds: vec!["scan".into()],
            timing: timing(backoff_min),
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
/// grant approved by the cluster's operator), sets its instance to
/// `mode = zip` with a sealed `token`, and deploys it to `host_node`.
async fn deploy_store(
    owner: &LibraryStore,
    owner_files: &std::path::Path,
    host_node: &str,
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
        host_node,
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
/// recorded, as `bookclerk clusters approve` does.
async fn approve_store(host: &LibraryStore, cluster_id: &str) {
    eventually("the owner's request to be recorded on the host", || async {
        load_remote_plugin_consent(host, cluster_id, PLUGIN)
            .await
            .unwrap()
            .is_some()
    })
    .await;
    let reviewed = load_remote_plugin_consent(host, cluster_id, PLUGIN)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.requested_version, "0.1.0");
    approve_remote_request(
        host,
        cluster_id,
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
async fn eventually<F, Fut>(what: &str, mut check: F)
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
async fn connected(handle: &HostClientHandle, count: u64) {
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
struct Captured(Arc<Mutex<Vec<u8>>>);

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
        load_cluster_link(&host, &link.owner_cluster_id)
            .await
            .unwrap()
            .as_ref(),
        Some(&link)
    );
    let node_id = link.remote_node_id.clone();
    let cluster_id = link.owner_cluster_id.clone();
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
    approve_store(&host, &cluster_id).await;
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
    let cluster_id = link.owner_cluster_id.clone();
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
    approve_store(&host, &cluster_id).await;

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
    let cluster_id = link.owner_cluster_id.clone();
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
    let request = load_remote_plugin_consent(&host, &cluster_id, PLUGIN)
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
    approve_store(&host, &cluster_id).await;
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
    let cluster_id = link.owner_cluster_id.clone();
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
    approve_store(&host, &cluster_id).await;
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

/// Revoking consent while the host runs a job (as `bookclerk clusters
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
    let cluster_id = link.owner_cluster_id.clone();
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
    approve_store(&host, &cluster_id).await;
    let job_id = enqueue_scan(&owner).await;
    eventually("the host to start the job", || async {
        !transport.runs().is_empty()
    })
    .await;
    assert_eq!(job(&owner, &job_id).await.state, JobState::Running);

    revoke_remote_plugin(&host, &cluster_id, PLUGIN)
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

/// A database swap stops the client of the previous store's link and
/// starts one for the new store's link.
#[tokio::test]
async fn a_database_swap_restarts_cluster_links_on_the_new_store() {
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
    super::spawn_cluster_links(Arc::clone(&host_state));
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
