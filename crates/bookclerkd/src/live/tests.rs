//! `/api/live` tests: exact Origin match, admission, topics per caller,
//! change frames, re-authentication, and binary refusal.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue};
use bookclerk_library::{EnqueueJobSpec, JobKind, JobPayload};
use bookclerk_library::{LibraryStore, UserRole};
use futures::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::Message;

use super::*;

/// The operator token of the test daemon.
const TOKEN: &str = "live-test-token";

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in pairs {
        out.append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    out
}

#[test]
fn origin_must_match_the_public_origin_exactly() {
    let public = Some("https://books.example");
    let ok = headers(&[
        ("origin", "https://books.example"),
        ("host", "books.example"),
    ]);
    assert!(origin_allowed(&ok, public));
    let trailing = Some("https://books.example/");
    assert!(
        origin_allowed(&ok, trailing),
        "a configured trailing slash is not part of the origin"
    );
    for bad in [
        "http://books.example",
        "https://books.example:8443",
        "https://evil.example",
        "https://books.example.evil.example",
        "null",
        "",
    ] {
        let h = headers(&[("origin", bad), ("host", "books.example")]);
        assert!(!origin_allowed(&h, public), "{bad:?}");
    }
    assert!(
        !origin_allowed(&headers(&[("host", "books.example")]), public),
        "no Origin"
    );
    let twice = headers(&[
        ("origin", "https://books.example"),
        ("origin", "https://books.example"),
    ]);
    assert!(!origin_allowed(&twice, public), "a repeated Origin");
}

#[test]
fn without_a_public_origin_only_the_loopback_host_itself_matches() {
    for host in [
        "127.0.0.1:8787",
        "localhost:5173",
        "[::1]:8787",
        "localhost",
    ] {
        let h = headers(&[("origin", &format!("http://{host}")), ("host", host)]);
        assert!(origin_allowed(&h, None), "{host}");
    }
    let cases: &[&[(&str, &str)]] = &[
        // Another site's page dialing the loopback daemon.
        &[
            ("origin", "https://evil.example"),
            ("host", "127.0.0.1:8787"),
        ],
        // A port that is not the page's.
        &[
            ("origin", "http://127.0.0.1:9999"),
            ("host", "127.0.0.1:8787"),
        ],
        // https against a plain listener's Host.
        &[
            ("origin", "https://127.0.0.1:8787"),
            ("host", "127.0.0.1:8787"),
        ],
        // DNS rebinding: the names agree, but the host is not loopback.
        &[
            ("origin", "http://evil.example:8787"),
            ("host", "evil.example:8787"),
        ],
        &[
            ("origin", "http://192.168.1.4:8787"),
            ("host", "192.168.1.4:8787"),
        ],
        // Behind a proxy without a configured public origin.
        &[
            ("origin", "http://127.0.0.1:8787"),
            ("host", "127.0.0.1:8787"),
            ("x-forwarded-proto", "https"),
        ],
        // No Host.
        &[("origin", "http://127.0.0.1:8787")],
    ];
    for case in cases {
        assert!(!origin_allowed(&headers(case), None), "{case:?}");
    }
}

/// A bootstrapped daemon with operator auth on, served on loopback.
async fn served(files: &tempfile::TempDir) -> (Arc<AppState>, LibraryStore, SocketAddr) {
    let (state, store) = crate::node_api::tests::test_state(files).await;
    *state.auth.write().await = Arc::new(crate::auth::OperatorAuthState::new(
        TOKEN.into(),
        1,
        true,
        5,
        30,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::api::router(Arc::clone(&state), None);
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (state, store, addr)
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens the socket with `origin` and extra headers; answers the socket or
/// the refusal's HTTP status.
async fn open(addr: SocketAddr, origin: &str, extra: &[(&str, &str)]) -> Result<Socket, u16> {
    let mut request = format!("ws://127.0.0.1:{}{LIVE_PATH}", addr.port())
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", HeaderValue::from_str(origin).unwrap());
    for (name, value) in extra {
        request.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Err(response.status().as_u16())
        }
        Err(other) => panic!("unexpected socket error: {other}"),
    }
}

/// The next JSON text frame within `wait`.
async fn next_frame(socket: &mut Socket, wait: Duration) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, socket.next()).await {
            Err(_) => return None,
            Ok(Some(Ok(Message::Text(text)))) => {
                return Some(serde_json::from_str(text.as_str()).unwrap())
            }
            Ok(Some(Ok(Message::Close(frame)))) => {
                return Some(serde_json::json!({
                    "type": "closed",
                    "code": frame.map(|f| u16::from(f.code)).unwrap_or(0),
                }))
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_)) | None) => return Some(serde_json::json!({ "type": "closed" })),
        }
    }
}

fn topics(frame: &serde_json::Value) -> Vec<String> {
    frame["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect()
}

async fn enqueue(store: &LibraryStore) {
    store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Scan,
            payload: JobPayload::default(),
            priority: 0,
            max_attempts: 3,
            max_pending: 64,
            run_after: None,
        })
        .await
        .unwrap();
}

/// Another origin is refused before the upgrade, even for the operator;
/// the right origin without credentials is 401.
#[tokio::test]
async fn a_foreign_origin_or_no_session_is_refused() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, _store, addr) = served(&files).await;
    let own = format!("http://127.0.0.1:{}", addr.port());
    let bearer = format!("Bearer {TOKEN}");
    assert_eq!(
        open(addr, "https://evil.example", &[("authorization", &bearer)])
            .await
            .err(),
        Some(403)
    );
    assert_eq!(
        open(addr, "http://127.0.0.1:1", &[("authorization", &bearer)])
            .await
            .err(),
        Some(403),
        "a different port is a different origin"
    );
    assert_eq!(open(addr, &own, &[]).await.err(), Some(401));
    assert_eq!(
        open(addr, &own, &[("authorization", "Bearer wrong")])
            .await
            .err(),
        Some(401)
    );
}

/// The operator hears every topic: a job queued in the shared database is
/// reported as `jobs`, a consent change at once as `consents`, and a
/// binary frame closes the socket.
#[tokio::test]
async fn the_operator_hears_job_and_consent_changes() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, addr) = served(&files).await;
    let own = format!("http://127.0.0.1:{}", addr.port());
    let bearer = format!("Bearer {TOKEN}");
    let mut socket = open(addr, &own, &[("authorization", &bearer)])
        .await
        .expect("admitted");
    let hello = next_frame(&mut socket, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["v"], 1);
    let all: Vec<String> = LiveTopic::ALL
        .iter()
        .map(|t| t.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut heard = topics(&hello);
    heard.sort();
    assert_eq!(heard, all);
    assert!(
        !hello.to_string().contains(TOKEN),
        "frames never carry credentials"
    );

    // Let the baseline read land, then change the jobs table.
    tokio::time::sleep(POLL_INTERVAL + Duration::from_millis(300)).await;
    enqueue(&store).await;
    let changed = next_frame(&mut socket, POLL_INTERVAL * 3).await.unwrap();
    assert_eq!(changed["type"], "changed", "{changed}");
    assert!(topics(&changed).contains(&"jobs".to_string()), "{changed}");
    assert_eq!(
        changed.as_object().unwrap().len(),
        3,
        "type, v and topics only"
    );

    crate::cluster_link::consent::notify_consent_changed();
    let poked = next_frame(&mut socket, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(topics(&poked), vec!["consents".to_string()]);

    socket
        .send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    let closed = next_frame(&mut socket, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(closed["type"], "closed");
    assert_eq!(closed["code"], 1003);
}

/// A member hears only `library` and `nodes`: a jobs poke is filtered
/// out, a library poke comes through; signing the member out closes the
/// socket with 4401.
#[tokio::test]
async fn a_member_hears_only_their_topics_until_signed_out() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, addr) = served(&files).await;
    let identity = store
        .upsert_portal_identity("test", "member-ext", Some("Member"))
        .await
        .unwrap();
    let user_id = identity.user_id.expect("a bridged user");
    assert_eq!(
        store.get_user(user_id).await.unwrap().unwrap().role,
        UserRole::Member
    );
    let cookie = crate::auth::tests::portal_cookie_for(&store, "test", "member-ext").await;
    let own = format!("http://127.0.0.1:{}", addr.port());
    let mut socket = open(addr, &own, &[("cookie", &cookie)])
        .await
        .expect("a signed-in member is admitted");
    let hello = next_frame(&mut socket, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        topics(&hello),
        vec!["library".to_string(), "nodes".to_string()]
    );

    poke(LiveTopic::Jobs);
    poke(LiveTopic::Consents);
    poke(LiveTopic::Library);
    let first = next_frame(&mut socket, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(
        topics(&first),
        vec!["library".to_string()],
        "operator topics are filtered out"
    );

    store
        .delete_portal_sessions_for_user(user_id)
        .await
        .unwrap();
    let mut closed = None;
    for _ in 0..10 {
        match next_frame(&mut socket, REAUTH_INTERVAL * 4).await {
            Some(frame) if frame["type"] == "closed" => {
                closed = Some(frame);
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    let closed = closed.expect("the socket closes once the session is gone");
    assert_eq!(closed["code"], CLOSE_UNAUTHORIZED);
}
