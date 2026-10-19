//! The host client against the step-6 owner, in one process: enroll, run a
//! job end to end through the daemon's job transport with the owner's
//! sealed secret and a token write-back, ack a wake, resume a held fence
//! after a disconnect and abandon one reclaimed while away, refuse a wrong
//! SPKI pin, keep every token out of TRACE logs, and follow a database
//! swap to the new store's links.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bookclerk_library::control_plane::{
    load_cluster_link, load_cluster_link_keys, load_node_status, mint_enrollment_link,
    node_queue_name, publish_node_message, ClusterLink, EnrollmentActor, NodeHeartbeat,
    NodeMessage,
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
use bookclerk_node_proto::{SecretRef, CLOSE_SUPERSEDED};
use tokio::sync::watch;

use super::TransportRunner;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// Plugin key of the owner's store account.
const PLUGIN: &str = "path:file:///tmp/store#store";

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
        let index = {
            let mut runs = self.runs.lock().unwrap();
            runs.push(Run {
                job_id: host.job.job.id.clone(),
                fence: host.job.fence,
                token,
                cancelled: false,
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
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
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

/// Starts a host client for `link` running `transport`.
async fn start_client(
    host: &LibraryStore,
    link: ClusterLink,
    transport: EchoTransport,
    backoff_min: Duration,
) -> (HostClientHandle, watch::Sender<bool>) {
    let keys = load_cluster_link_keys(host, &link).await.unwrap();
    let (client, handle) = HostClient::new(
        HostClientConfig {
            link,
            keys,
            software_version: "1.0.0".into(),
            incarnation: uuid::Uuid::new_v4().to_string(),
            max_jobs: 1,
            timing: timing(backoff_min),
        },
        TransportRunner::new(transport),
        Box::new(FixedSampler),
    )
    .unwrap();
    let (stop, shutdown) = watch::channel(false);
    tokio::spawn(client.run(shutdown));
    (handle, stop)
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
    let transport = EchoTransport {
        refresh_to: Some("riley-token-v2"),
        ..EchoTransport::default()
    };
    let (handle, _stop) =
        start_client(&host, link, transport.clone(), Duration::from_millis(200)).await;
    connected(&handle, 1).await;
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
    let wake = tokio::time::timeout(Duration::from_secs(20), wakes.recv())
        .await
        .expect("the wake arrived")
        .unwrap();
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
    for value in ["riley-token-v1", "riley-token-v2"] {
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
    let transport = EchoTransport {
        block: true,
        ..EchoTransport::default()
    };
    let (handle, _stop) =
        start_client(&host, link, transport.clone(), Duration::from_millis(1500)).await;
    connected(&handle, 1).await;

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
    let (state, owner) = crate::node_api::tests::test_state_with(
        &owner_files,
        "[daemon.node_api]\nmode = \"self_signed\"\ntls_listen = \"127.0.0.1:0\"\n",
    )
    .await;
    let cfg = state.config.read().await.clone();
    let started = crate::node_api::tls::start(&state, &cfg, owner_files.path()).await;
    let tls = started.local.expect("the TLS listener is bound");
    let origin = format!("https://localhost:{}", tls.port());
    state.config.write().await.integrations.public_origin = Some(origin.clone());
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
        link,
        EchoTransport::default(),
        Duration::from_millis(200),
    )
    .await;
    connected(&handle, 1).await;
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
