//! The client against a scripted owner on loopback: a heartbeat or claim
//! the owner leaves unanswered is retried on the same connection, a wake is
//! acked only once a subscriber received it, a late `work` for a timed-out
//! claim is handed back, a job whose lease goes unconfirmed (or whose
//! client shuts down) is cancelled, and a waiting report keeps renewing.

use bookclerk_library::control_plane::{ClusterLinkKeys, CLUSTER_LINK_ACTIVE};
use bookclerk_node_proto::{box_public_key, encode_nonce, SUBPROTOCOL};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::WebSocketStream;

use super::*;

/// Owner cluster id of the scripted owner.
const CLUSTER: &str = "cluster-1";

/// This host's node id there.
const NODE: &str = "3f2b8c1e-7a4d-4e5f-9b0a-1c2d3e4f5a6b";

/// The plugin every scripted job runs, consented on every test client.
const PLUGIN: &str = "path:file:///plugins/store#store";

/// A runner for claims the scripted owner never grants.
struct NoJobs;

impl HostJobRunner for NoJobs {
    async fn run(&self, _ctx: HostJobCtx) -> Result<String, HostJobError> {
        Err(HostJobError::new(
            "unexpected",
            "the scripted owner grants no work",
        ))
    }
}

/// A runner that records the lease deadline its job was handed, then fails.
#[derive(Clone, Default)]
struct RecordsDeadline {
    /// The handed deadline, once a job started.
    deadline: Arc<Mutex<Option<std::time::SystemTime>>>,
}

impl HostJobRunner for RecordsDeadline {
    async fn run(&self, ctx: HostJobCtx) -> Result<String, HostJobError> {
        *lock(&self.deadline) = Some(ctx.job.lease_deadline);
        Err(HostJobError::new("recorded", "the deadline was recorded"))
    }
}

/// A runner that runs until its job is cancelled, and records both.
#[derive(Clone, Default)]
struct UntilCancelled {
    /// Set once a job started.
    started: Arc<AtomicBool>,
    /// Set once a started job saw its cancellation.
    cancelled: Arc<AtomicBool>,
}

impl HostJobRunner for UntilCancelled {
    async fn run(&self, ctx: HostJobCtx) -> Result<String, HostJobError> {
        self.started.store(true, Ordering::SeqCst);
        while !ctx.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.cancelled.store(true, Ordering::SeqCst);
        Err(HostJobError::new("cancelled", "the job was cancelled"))
    }
}

/// A token the [`QuotesToken`] runner leaks into its error.
const QUOTED_TOKEN: &str = "quoted-store-token_0123456789abcdefABCDEF";

/// A runner whose error quotes a registered secret.
struct QuotesToken;

impl HostJobRunner for QuotesToken {
    async fn run(&self, _ctx: HostJobCtx) -> Result<String, HostJobError> {
        Err(HostJobError::new(
            "refresh",
            format!("token {QUOTED_TOKEN} was rejected"),
        ))
    }
}

/// Fixed heartbeat measurements.
struct FixedSampler;

impl HeartbeatSource for FixedSampler {
    fn sample(&mut self) -> NodeHeartbeat {
        NodeHeartbeat {
            incarnation: "inc".into(),
            started_at: "2026-10-17T00:00:00Z".into(),
            interval: Duration::from_secs(1),
            cpu_millicores: 1000,
            memory_bytes: None,
            cpu_used_millicores: None,
            memory_available_bytes: None,
            files_dir_free_bytes: None,
        }
    }
}

/// The owner side of one accepted socket.
type OwnerSide = WebSocketStream<tokio::net::TcpStream>;

/// Fast timers: requests time out after 300 ms.
fn timing() -> ClientTiming {
    ClientTiming {
        heartbeat_interval: Duration::from_millis(100),
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        request_timeout: Duration::from_millis(300),
        ..ClientTiming::default()
    }
}

/// Starts a client for an owner listening on `port`.
fn start(port: u16, max_jobs: usize) -> (HostClientHandle, watch::Sender<bool>) {
    let (handle, stop, _) = start_with(port, max_jobs, NoJobs);
    (handle, stop)
}

/// Starts a client running jobs through `runner`; the join handle yields
/// its exit.
fn start_with<R: HostJobRunner>(
    port: u16,
    max_jobs: usize,
    runner: R,
) -> (
    HostClientHandle,
    watch::Sender<bool>,
    JoinHandle<ClientExit>,
) {
    start_configured(port, max_jobs, false, runner)
}

/// [`start_with`], with claims waiting for a deployments refresh when
/// `claims_after_refresh` is set.
fn start_configured<R: HostJobRunner>(
    port: u16,
    max_jobs: usize,
    claims_after_refresh: bool,
    runner: R,
) -> (
    HostClientHandle,
    watch::Sender<bool>,
    JoinHandle<ClientExit>,
) {
    let link = ClusterLink {
        owner_cluster_id: CLUSTER.into(),
        owner_origin: format!("http://127.0.0.1:{port}"),
        remote_node_id: NODE.into(),
        spki_pin: None,
        state: CLUSTER_LINK_ACTIVE.into(),
        enrolled_at: "2026-10-17T00:00:00Z".into(),
    };
    let (client, handle) = HostClient::new(
        HostClientConfig {
            link,
            keys: ClusterLinkKeys::generate(),
            software_version: "1.0.0".into(),
            incarnation: "inc".into(),
            max_jobs,
            claim_kinds: vec!["scan".into()],
            claims_after_refresh,
            timing: timing(),
        },
        runner,
        Box::new(FixedSampler),
    )
    .unwrap();
    handle.set_consented_plugins(BTreeSet::from([PLUGIN.to_string()]));
    let (stop, shutdown) = watch::channel(false);
    let exit = tokio::spawn(client.run(shutdown));
    (handle, stop, exit)
}

/// A `work` frame granting job `job_id` under `fence` for `lease_secs`.
fn work(job_id: &str, fence: i64, lease_secs: u64) -> Body {
    Body::Work {
        item: ITEM_JOB.into(),
        job: bookclerk_node_proto::WorkJob {
            id: job_id.into(),
            kind: "scan".into(),
            payload: serde_json::json!({}),
            attempt: 1,
        },
        fence,
        lease_secs,
        sealed_secrets: None,
        plugin_keys: vec![PLUGIN.into()],
        resumed: false,
    }
}

/// Waits up to `within` for `flag` to turn true.
async fn wait_for(flag: &AtomicBool, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    flag.load(Ordering::SeqCst)
}

/// The next text frame from the host.
async fn next_frame(socket: &mut OwnerSide) -> Frame {
    loop {
        match socket.next().await.expect("the host is connected").unwrap() {
            Message::Text(text) => return Frame::parse(text.as_str()).unwrap(),
            Message::Close(_) => panic!("the host closed the socket"),
            _ => {}
        }
    }
}

/// Sends `body` as frame `id`.
async fn send_frame(socket: &mut OwnerSide, id: &str, body: Body) {
    socket
        .send(Message::Text(Frame::new(id, body).to_text().into()))
        .await
        .unwrap();
}

/// Accepts the host's one socket and welcomes it (the scripted owner does
/// not check the signature).
async fn accept(listener: &TcpListener) -> OwnerSide {
    let (tcp, _) = listener.accept().await.unwrap();
    // The callback's error type is tungstenite's, not ours.
    #[allow(clippy::result_large_err)]
    let select = |_: &Request, mut response: Response| {
        response.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static(SUBPROTOCOL),
        );
        Ok(response)
    };
    let mut socket = tokio_tungstenite::accept_hdr_async(tcp, select)
        .await
        .unwrap();
    assert!(matches!(
        next_frame(&mut socket).await.body,
        Body::Hello { .. }
    ));
    send_frame(
        &mut socket,
        "challenge",
        Body::Challenge {
            nonce: encode_nonce(&rand::random::<[u8; 32]>()),
            cluster_id: CLUSTER.into(),
        },
    )
    .await;
    assert!(matches!(
        next_frame(&mut socket).await.body,
        Body::Auth { .. }
    ));
    send_frame(
        &mut socket,
        "welcome",
        Body::Welcome {
            node_id: NODE.into(),
            cluster_id: CLUSTER.into(),
            box_key: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(box_public_key(&[3u8; 32])),
        },
    )
    .await;
    socket
}

/// Reads host frames until `until` passes, answering per `answer`, and
/// returns the frames seen.
async fn read_for(
    socket: &mut OwnerSide,
    until: Duration,
    mut answer: impl FnMut(&Frame) -> Option<Body>,
) -> Vec<Frame> {
    let deadline = tokio::time::Instant::now() + until;
    let mut seen = Vec::new();
    while let Ok(frame) = tokio::time::timeout_at(deadline, next_frame(socket)).await {
        if let Some(body) = answer(&frame) {
            send_frame(socket, &frame.id, body).await;
        }
        seen.push(frame);
    }
    seen
}

/// An unanswered heartbeat and an unanswered claim are retried on the same
/// connection instead of ending their loops.
#[tokio::test]
async fn unanswered_heartbeats_and_claims_are_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (handle, _stop) = start(listener.local_addr().unwrap().port(), 1);
    let mut socket = accept(&listener).await;

    // The first heartbeat and the first claim time out; later ones are
    // answered.
    let mut first_beat = true;
    let mut first_claim = true;
    let frames = read_for(&mut socket, Duration::from_secs(3), |frame| {
        match &frame.body {
            Body::Heartbeat { .. } if std::mem::take(&mut first_beat) => None,
            Body::Heartbeat { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if std::mem::take(&mut first_claim) => None,
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 1,
            }),
            _ => None,
        }
    })
    .await;
    let beats = frames
        .iter()
        .filter(|frame| matches!(frame.body, Body::Heartbeat { .. }))
        .count();
    let claims = frames
        .iter()
        .filter(|frame| matches!(frame.body, Body::Claim { .. }))
        .count();
    assert!(beats >= 3, "heartbeats continue after a timeout: {beats}");
    assert!(claims >= 2, "claims continue after a timeout: {claims}");
    assert_eq!(*handle.connections().borrow(), 1, "on the same connection");
}

/// A wake no subscriber receives is not acked; once a subscriber is
/// listening, the next wake reaches it and is acked.
#[tokio::test]
async fn a_wake_without_a_subscriber_is_not_acked() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (handle, _stop) = start(listener.local_addr().unwrap().port(), 0);
    let mut socket = accept(&listener).await;
    let wake = || Body::Wake {
        message: serde_json::json!({"type": "deployment.changed", "revision": 1}),
    };
    let accept_beats =
        |frame: &Frame| matches!(frame.body, Body::Heartbeat { .. }).then_some(Body::Accepted {});

    send_frame(&mut socket, "wake:1", wake()).await;
    let frames = read_for(&mut socket, Duration::from_millis(800), accept_beats).await;
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame.body, Body::Ack {})),
        "a wake nobody received was acked"
    );

    let mut wakes = handle.subscribe_wakes();
    send_frame(&mut socket, "wake:2", wake()).await;
    let frames = read_for(&mut socket, Duration::from_millis(800), accept_beats).await;
    let acked: Vec<&str> = frames
        .iter()
        .filter(|frame| matches!(frame.body, Body::Ack {}))
        .map(|frame| frame.id.as_str())
        .collect();
    assert_eq!(acked, ["wake:2"]);
    assert_eq!(wakes.try_recv().unwrap()["type"], "deployment.changed");
}

/// A `work` that arrives after its claim timed out is not dropped: the host
/// fails it with [`CLAIM_ABANDONED`] under that work's fence, so the owner
/// frees the lease now.
#[tokio::test]
async fn a_late_work_for_a_timed_out_claim_is_handed_back() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runner = UntilCancelled::default();
    let (_handle, _stop, _exit) =
        start_with(listener.local_addr().unwrap().port(), 1, runner.clone());
    let mut socket = accept(&listener).await;

    // Leave the first claim unanswered past the request timeout.
    let mut first_claim = None;
    let frames = read_for(
        &mut socket,
        Duration::from_millis(500),
        |frame| match &frame.body {
            Body::Heartbeat { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if first_claim.is_none() => {
                first_claim = Some(frame.id.clone());
                None
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            _ => None,
        },
    )
    .await;
    assert!(frames
        .iter()
        .any(|frame| matches!(frame.body, Body::Claim { .. })));
    let first_claim = first_claim.expect("the host claimed");

    // The owner's answer to it arrives late.
    send_frame(&mut socket, &first_claim, work("job-late", 7, 60)).await;
    let frames = read_for(
        &mut socket,
        Duration::from_millis(800),
        |frame| match &frame.body {
            Body::Heartbeat { .. } | Body::Fail { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            _ => None,
        },
    )
    .await;
    let failed: Vec<(&str, i64, &str)> = frames
        .iter()
        .filter_map(|frame| match &frame.body {
            Body::Fail {
                job_id,
                fence,
                error_kind,
                ..
            } => Some((job_id.as_str(), *fence, error_kind.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(failed, [("job-late", 7, CLAIM_ABANDONED)]);
    assert!(
        !runner.started.load(Ordering::SeqCst),
        "the late work was run"
    );
}

/// A job keeps running while the owner renews its lease, and is cancelled
/// as lost (its result discarded) once the confirmed lease runs out with
/// renewals left unanswered.
#[tokio::test]
async fn a_job_whose_lease_goes_unconfirmed_is_cancelled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runner = UntilCancelled::default();
    let (handle, _stop, _exit) =
        start_with(listener.local_addr().unwrap().port(), 1, runner.clone());
    let mut socket = accept(&listener).await;

    // Grant a one-second lease and renew it for two seconds.
    let mut granted = false;
    let frames = read_for(&mut socket, Duration::from_secs(2), |frame| {
        match &frame.body {
            Body::Heartbeat { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                Some(work("job-1", 3, 1))
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            Body::Renew { .. } => Some(Body::Renewed {
                lease_secs: 1,
                cancel: false,
            }),
            _ => None,
        }
    })
    .await;
    assert!(frames
        .iter()
        .any(|frame| matches!(frame.body, Body::Renew { .. })));
    assert!(runner.started.load(Ordering::SeqCst));
    assert!(
        !runner.cancelled.load(Ordering::SeqCst),
        "a renewed lease cancelled the job"
    );

    // The owner stops answering renewals; the connection stays up.
    let frames = read_for(&mut socket, Duration::from_secs(2), |frame| {
        matches!(frame.body, Body::Heartbeat { .. }).then_some(Body::Accepted {})
    })
    .await;
    assert!(
        runner.cancelled.load(Ordering::SeqCst),
        "the job ran past its unconfirmed lease"
    );
    assert!(handle.held_fences().is_empty());
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame.body, Body::Complete { .. } | Body::Fail { .. })),
        "a lost job's result was reported"
    );
}

/// Shutting the client down cancels the jobs it holds instead of leaving
/// their tasks running.
#[tokio::test]
async fn shutdown_cancels_held_jobs() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runner = UntilCancelled::default();
    let (handle, stop, exit) = start_with(listener.local_addr().unwrap().port(), 1, runner.clone());
    let mut socket = accept(&listener).await;
    let mut granted = false;
    read_for(
        &mut socket,
        Duration::from_millis(500),
        |frame| match &frame.body {
            Body::Heartbeat { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                Some(work("job-1", 3, 60))
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            _ => None,
        },
    )
    .await;
    assert!(wait_for(&runner.started, Duration::from_secs(1)).await);
    assert_eq!(handle.held_fences().len(), 1);

    stop.send(true).unwrap();
    assert_eq!(exit.await.unwrap(), ClientExit::Shutdown);
    assert!(
        wait_for(&runner.cancelled, Duration::from_secs(1)).await,
        "the job outlived its client"
    );
    assert!(handle.held_fences().is_empty());
}

/// Aborting the client's task (as a database swap does when a client does
/// not stop in time) still cancels the jobs it holds.
#[tokio::test]
async fn aborting_the_client_cancels_held_jobs() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runner = UntilCancelled::default();
    let (handle, _stop, exit) =
        start_with(listener.local_addr().unwrap().port(), 1, runner.clone());
    let mut socket = accept(&listener).await;
    let mut granted = false;
    read_for(
        &mut socket,
        Duration::from_millis(500),
        |frame| match &frame.body {
            Body::Heartbeat { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                Some(work("job-1", 3, 60))
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            _ => None,
        },
    )
    .await;
    assert!(wait_for(&runner.started, Duration::from_secs(1)).await);
    assert_eq!(handle.held_fences().len(), 1);

    exit.abort();
    assert!(exit.await.unwrap_err().is_cancelled());
    assert!(
        wait_for(&runner.cancelled, Duration::from_secs(1)).await,
        "the job outlived its aborted client"
    );
}

/// A finished job whose report the owner leaves unanswered keeps renewing
/// its lease, so a slow report does not let the fence lapse.
#[tokio::test]
async fn renewals_continue_while_a_report_waits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (handle, _stop, _exit) = start_with(listener.local_addr().unwrap().port(), 1, NoJobs);
    let mut socket = accept(&listener).await;

    // Grant a one-second lease; the runner fails at once, and its report
    // goes unanswered for longer than the lease.
    let mut granted = false;
    let frames = read_for(
        &mut socket,
        Duration::from_millis(2500),
        |frame| match &frame.body {
            Body::Heartbeat { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                Some(work("job-1", 3, 1))
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            Body::Renew { .. } => Some(Body::Renewed {
                lease_secs: 1,
                cancel: false,
            }),
            _ => None,
        },
    )
    .await;
    let first_report = frames
        .iter()
        .position(|frame| matches!(frame.body, Body::Fail { .. }))
        .expect("the job was reported");
    assert!(
        frames[first_report..]
            .iter()
            .any(|frame| matches!(frame.body, Body::Renew { .. })),
        "the lease was not renewed while the report waited"
    );
    assert_eq!(
        handle.held_fences().len(),
        1,
        "the job was dropped mid-report"
    );

    // The owner accepts the retried report; the job is released.
    read_for(&mut socket, Duration::from_secs(1), |frame| {
        match &frame.body {
            Body::Heartbeat { .. } | Body::Fail { .. } => Some(Body::Accepted {}),
            Body::Renew { .. } => Some(Body::Renewed {
                lease_secs: 1,
                cancel: false,
            }),
            _ => None,
        }
    })
    .await;
    assert!(handle.held_fences().is_empty());
}

/// The deadline handed to the runner is the lease counted from before the
/// claim was sent, not from when the job started, so work honouring it
/// stops before the fence can lapse.
#[tokio::test]
async fn the_handed_deadline_counts_from_the_claim() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runner = RecordsDeadline::default();
    let before = std::time::SystemTime::now();
    let (_handle, _stop, _exit) =
        start_with(listener.local_addr().unwrap().port(), 1, runner.clone());
    let mut socket = accept(&listener).await;
    let mut granted = false;
    read_for(
        &mut socket,
        Duration::from_millis(1500),
        |frame| match &frame.body {
            Body::Heartbeat { .. } | Body::Fail { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                Some(work("job-1", 3, 30))
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            Body::Renew { .. } => Some(Body::Renewed {
                lease_secs: 30,
                cancel: false,
            }),
            _ => None,
        },
    )
    .await;
    let deadline = lock(&runner.deadline).expect("the job ran");
    assert!(deadline >= before + Duration::from_secs(30));
    assert!(
        deadline <= std::time::SystemTime::now() + Duration::from_secs(30),
        "the deadline was counted from after the claim"
    );
}

/// A job whose secrets do not open keeps renewing its lease while its
/// fail report goes unanswered, so the fail is not lost to a reclaim.
#[tokio::test]
async fn renewals_continue_while_an_unopenable_fail_waits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (handle, _stop, _exit) = start_with(listener.local_addr().unwrap().port(), 1, NoJobs);
    let mut socket = accept(&listener).await;

    let mut granted = false;
    let frames = read_for(
        &mut socket,
        Duration::from_millis(2500),
        |frame| match &frame.body {
            Body::Heartbeat { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                let Body::Work {
                    item,
                    job,
                    fence,
                    lease_secs,
                    resumed,
                    plugin_keys,
                    ..
                } = work("job-1", 3, 1)
                else {
                    unreachable!("work() builds a work frame");
                };
                Some(Body::Work {
                    item,
                    job,
                    fence,
                    lease_secs,
                    sealed_secrets: Some("not-a-sealed-box".into()),
                    resumed,
                    plugin_keys,
                })
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            Body::Renew { .. } => Some(Body::Renewed {
                lease_secs: 1,
                cancel: false,
            }),
            _ => None,
        },
    )
    .await;
    let first_report = frames
        .iter()
        .position(|frame| {
            matches!(&frame.body, Body::Fail { error_kind, .. } if error_kind == SECRETS_UNOPENABLE)
        })
        .expect("the unopenable job was reported");
    assert!(
        frames[first_report..]
            .iter()
            .any(|frame| matches!(frame.body, Body::Renew { .. })),
        "the lease was not renewed while the fail waited"
    );
    assert_eq!(
        handle.held_fences().len(),
        1,
        "the job was dropped mid-report"
    );
}

/// A resume renews a held job for the lease the owner names, not the
/// job's original grant: an owner whose lease got shorter while the host
/// was away, and that then leaves renewals unanswered, gets the job
/// cancelled once that shorter lease runs out.
#[tokio::test]
async fn a_resume_counts_the_lease_the_owner_names() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runner = UntilCancelled::default();
    let (handle, _stop, _exit) =
        start_with(listener.local_addr().unwrap().port(), 1, runner.clone());
    let mut socket = accept(&listener).await;

    // Grant a one-hour lease, then drop the connection.
    let mut granted = false;
    read_for(
        &mut socket,
        Duration::from_millis(500),
        |frame| match &frame.body {
            Body::Heartbeat { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                Some(work("job-1", 3, 3600))
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            _ => None,
        },
    )
    .await;
    assert!(wait_for(&runner.started, Duration::from_secs(1)).await);
    drop(socket);

    // The host resumes; the owner renews the fence for one second only
    // and then leaves every renewal unanswered.
    let mut socket = accept(&listener).await;
    send_frame(
        &mut socket,
        "resumed",
        Body::Resumed {
            renewed: vec![HeldFence {
                kind: ITEM_JOB.into(),
                id: "job-1".into(),
                fence: 3,
            }],
            lost: Vec::new(),
            unchecked: Vec::new(),
            lease_secs: Some(1),
        },
    )
    .await;
    read_for(&mut socket, Duration::from_secs(3), |frame| {
        matches!(frame.body, Body::Heartbeat { .. }).then_some(Body::Accepted {})
    })
    .await;
    assert!(
        runner.cancelled.load(Ordering::SeqCst),
        "the job ran past the lease its resume named"
    );
    assert!(handle.held_fences().is_empty());
}

/// Handed secrets are registered for redaction before a runner sees them,
/// so a store token in a job's log or error is scrubbed on this host.
#[test]
fn handed_secrets_are_registered_for_redaction() {
    let _registry = bookclerk_config::secrets_registry_test_lock();
    let token = "handed-store-token_0123456789abcdefABCDEF";
    let entry = SecretEntry {
        secret_ref: SecretRef {
            kind: "source_auth".into(),
            provider: "libro".into(),
            account_id: "acct-1".into(),
            name: "token".into(),
        },
        value: bookclerk_node_proto::SecretValue::new(token.as_bytes().to_vec()),
    };
    let line = format!("refresh failed for {token}");
    assert!(!bookclerk_config::contains_registered_secret(&line));
    register_for_redaction(&[entry]);
    assert!(!bookclerk_config::redact_str(&line).contains(token));

    let inner = "bundled-access-token_0123456789abcdefABCDEF";
    let bundle = format!(r#"{{"tokens":{{"access_token":"{inner}"}},"expires":3600}}"#);
    register_value_for_redaction(bundle.as_bytes());
    assert!(!bookclerk_config::redact_str(&format!("Bearer {inner}")).contains(inner));
}

/// A job whose grant already ran out when it would start is never run
/// and never reported: its fence may already belong to another host.
#[tokio::test]
async fn an_expired_grant_is_never_started() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runner = RecordsDeadline::default();
    let (handle, _stop, _exit) =
        start_with(listener.local_addr().unwrap().port(), 1, runner.clone());
    let mut socket = accept(&listener).await;
    let mut granted = false;
    let frames = read_for(
        &mut socket,
        Duration::from_millis(1000),
        |frame| match &frame.body {
            Body::Heartbeat { .. } | Body::Fail { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                Some(work("job-1", 3, 0))
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            _ => None,
        },
    )
    .await;
    assert!(lock(&runner.deadline).is_none(), "the expired job ran");
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame.body, Body::Fail { .. } | Body::Complete { .. })),
        "the expired job was reported"
    );
    assert!(handle.held_fences().is_empty());
}

/// A runner error that quotes a registered secret reaches the owner
/// scrubbed.
#[tokio::test]
async fn a_failed_report_is_scrubbed_of_secrets() {
    bookclerk_config::register_secret(QUOTED_TOKEN);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_handle, _stop, _exit) = start_with(listener.local_addr().unwrap().port(), 1, QuotesToken);
    let mut socket = accept(&listener).await;
    let mut granted = false;
    let frames = read_for(
        &mut socket,
        Duration::from_millis(1500),
        |frame| match &frame.body {
            Body::Heartbeat { .. } | Body::Fail { .. } => Some(Body::Accepted {}),
            Body::Claim { .. } if !std::mem::replace(&mut granted, true) => {
                Some(work("job-1", 3, 30))
            }
            Body::Claim { .. } => Some(Body::NoWork {
                retry_after_secs: 300,
            }),
            Body::Renew { .. } => Some(Body::Renewed {
                lease_secs: 30,
                cancel: false,
            }),
            _ => None,
        },
    )
    .await;
    let message = frames
        .iter()
        .find_map(|frame| match &frame.body {
            Body::Fail { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("the job was reported");
    assert!(message.contains("was rejected"), "{message}");
    assert!(
        !message.contains(QUOTED_TOKEN),
        "the report carried the token"
    );
}

/// Under `claims_after_refresh` a connection claims nothing until the
/// deployments were re-read on it; a read on an older connection does not
/// count.
#[tokio::test]
async fn claims_wait_for_the_connections_deployments_refresh() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (handle, _stop, _exit) =
        start_configured(listener.local_addr().unwrap().port(), 1, true, NoJobs);
    let mut socket = accept(&listener).await;
    let answer = |frame: &Frame| match &frame.body {
        Body::Heartbeat { .. } => Some(Body::Accepted {}),
        Body::Claim { .. } => Some(Body::NoWork {
            retry_after_secs: 300,
        }),
        _ => None,
    };
    handle.deployments_refreshed(0);
    let before = read_for(&mut socket, Duration::from_millis(500), answer).await;
    assert!(
        !before
            .iter()
            .any(|frame| matches!(frame.body, Body::Claim { .. })),
        "claimed before the deployments were re-read"
    );
    handle.deployments_refreshed(*handle.connections().borrow());
    let after = read_for(&mut socket, Duration::from_millis(500), answer).await;
    assert!(
        after
            .iter()
            .any(|frame| matches!(frame.body, Body::Claim { .. })),
        "no claim after the refresh"
    );
}
