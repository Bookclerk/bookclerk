//! The client against a scripted owner on loopback: a heartbeat or claim
//! the owner leaves unanswered is retried on the same connection, a wake is
//! acked only once a subscriber received it, a late `work` for a timed-out
//! claim is handed back, and a job whose lease goes unconfirmed (or whose
//! client shuts down) is cancelled.

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
            timing: timing(),
        },
        runner,
        Box::new(FixedSampler),
    )
    .unwrap();
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
            nonce: encode_nonce(&[9u8; 32]),
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
