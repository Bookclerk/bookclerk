//! The client against a scripted owner on loopback: a heartbeat or claim
//! the owner leaves unanswered is retried on the same connection, and a
//! wake is acked only once a subscriber received it.

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
        NoJobs,
        Box::new(FixedSampler),
    )
    .unwrap();
    let (stop, shutdown) = watch::channel(false);
    tokio::spawn(client.run(shutdown));
    (handle, stop)
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
