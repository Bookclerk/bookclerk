//! Tests for the granted `POST /origin/fetch` and `POST /origin/connect`
//! routes: the connection side
//! (frame parsing, streaming the reply) and the vat side (grant checks, the
//! library's response checked before it goes back).

use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bookclerk_plugin_abi::origin::{encode_origin_frame, OriginResponseHead};
use bookclerk_plugin_abi::{
    Origin, OriginConnected, OriginHeader, OriginRefusal, OriginReply, OriginRequest, PluginError,
    PluginErrorCode, SocketInfo,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::super::{handle_conn, GrantedCmd, GrantedSlot, GrantedTable};
use super::{dispatch, dispatch_connect, OriginOpened};

/// A framed `POST` head for `target` followed by `body`.
fn framed(target: &str, body: &[u8]) -> Vec<u8> {
    let mut out = encode_origin_frame(&OriginRequest {
        method: "POST".into(),
        target: target.into(),
        headers: vec![OriginHeader {
            name: "content-type".into(),
            value: "text/plain".into(),
        }],
        client_address: Some("203.0.113.9".into()),
    })
    .unwrap();
    out.extend_from_slice(body);
    out
}

/// `POST /origin/fetch` under the test grant with `body` as its HTTP body.
fn origin_request(body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "POST /origin/fetch HTTP/1.1\r\nAuthorization: Bearer grant-1\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Sends `request` to the granted server and answers each `OriginFetch`
/// with `answer`, given the checked request and its whole body. Returns
/// the raw response and whether anything was dispatched.
async fn origin_exchange(
    request: Vec<u8>,
    answer: fn(&str, Vec<u8>) -> Result<OriginOpened, PluginError>,
) -> (Vec<u8>, bool) {
    let (cmds_tx, mut cmds_rx) = mpsc::channel(8);
    let dispatched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&dispatched);
    tokio::spawn(async move {
        while let Some(cmd) = cmds_rx.recv().await {
            if let GrantedCmd::OriginFetch {
                invocation,
                request,
                mut body_rx,
                resp,
            } = cmd
            {
                assert_eq!(invocation, "grant-1");
                assert_eq!(request.client, Some("203.0.113.9".parse().unwrap()));
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                let mut body = Vec::new();
                while let Some(chunk) = body_rx.recv().await {
                    body.extend_from_slice(&chunk.unwrap());
                }
                let _ = resp.send(answer(&request.target, body));
            }
        }
    });
    let (client, server) = tokio::io::duplex(1 << 16);
    let serve = tokio::spawn(async move { handle_conn(server, "bridge", cmds_tx).await });
    let (mut reader, mut writer) = tokio::io::split(client);
    writer.write_all(&request).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_to_end(&mut out))
        .await
        .expect("http response")
        .unwrap();
    drop(writer);
    let _ = serve.await;
    (out, dispatched.load(std::sync::atomic::Ordering::SeqCst))
}

/// Joins the chunks of a chunked HTTP body.
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n").unwrap();
        let len =
            usize::from_str_radix(std::str::from_utf8(&body[..line_end]).unwrap(), 16).unwrap();
        body = &body[line_end + 2..];
        if len == 0 {
            return out;
        }
        out.extend_from_slice(&body[..len]);
        body = &body[len + 2..];
    }
}

/// Splits a raw response into its head text and its body bytes.
fn split_response(raw: &[u8]) -> (String, &[u8]) {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (
        String::from_utf8_lossy(&raw[..end]).into_owned(),
        &raw[end + 4..],
    )
}

#[tokio::test]
async fn origin_route_streams_the_framed_head_and_the_body_both_ways() {
    let (raw, dispatched) = origin_exchange(
        origin_request(&framed("/api/status?x=1", b"hello")),
        |target, body| {
            assert_eq!(target, "/api/status?x=1");
            assert_eq!(body, b"hello");
            let (tx, rx) = mpsc::channel(2);
            tx.try_send(Ok(b"wor".to_vec())).unwrap();
            tx.try_send(Ok(b"ld".to_vec())).unwrap();
            Ok(OriginOpened {
                head: OriginResponseHead {
                    status: 201,
                    headers: vec![OriginHeader {
                        name: "x-a".into(),
                        value: "1".into(),
                    }],
                },
                body_rx: Some(rx),
            })
        },
    )
    .await;
    assert!(dispatched);
    let (head, body) = split_response(&raw);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(head.contains("transfer-encoding: chunked"), "{head}");
    let body = dechunk(body);
    let (json, rest) = bookclerk_plugin_abi::origin::split_origin_frame(&body)
        .unwrap()
        .unwrap();
    let head = bookclerk_plugin_abi::origin::decode_origin_response_frame(json).unwrap();
    assert_eq!(head.status, 201);
    assert_eq!(head.headers[0].name, "x-a");
    assert_eq!(rest, b"world");
}

#[tokio::test]
async fn a_response_without_a_body_ends_after_the_head() {
    let (raw, _) = origin_exchange(origin_request(&framed("/", b"")), |_, body| {
        assert!(body.is_empty());
        Ok(OriginOpened {
            head: OriginResponseHead {
                status: 204,
                headers: Vec::new(),
            },
            body_rx: None,
        })
    })
    .await;
    let (_, body) = split_response(&raw);
    let body = dechunk(body);
    let (_, rest) = bookclerk_plugin_abi::origin::split_origin_frame(&body)
        .unwrap()
        .unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn a_bad_head_is_refused_before_the_library_sees_it() {
    let (raw, dispatched) = origin_exchange(origin_request(&framed("//evil", b"")), |_, _| {
        panic!("must not dispatch")
    })
    .await;
    assert!(!dispatched);
    let (head, body) = split_response(&raw);
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    assert!(String::from_utf8_lossy(body).contains("invalid_params"));
}

#[tokio::test]
async fn an_oversize_frame_prefix_is_refused() {
    let (raw, dispatched) =
        origin_exchange(origin_request(&[0xff, 0xff, 0xff, 0xff, b'{']), |_, _| {
            panic!("must not dispatch")
        })
        .await;
    assert!(!dispatched);
    assert!(String::from_utf8_lossy(&raw).contains("invalid_params"));
}

#[tokio::test]
async fn a_body_that_ends_inside_the_head_is_refused() {
    let whole = framed("/", b"");
    let (raw, dispatched) = origin_exchange(origin_request(&whole[..whole.len() - 3]), |_, _| {
        panic!("must not dispatch")
    })
    .await;
    assert!(!dispatched);
    assert!(String::from_utf8_lossy(&raw).contains("ended inside its head"));
}

#[tokio::test]
async fn a_library_refusal_is_answered_as_the_plugin_error() {
    let (raw, dispatched) = origin_exchange(origin_request(&framed("/", b"")), |_, _| {
        Err(PluginError::new(PluginErrorCode::Unavailable, "not now"))
    })
    .await;
    assert!(dispatched);
    let (head, body) = split_response(&raw);
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(String::from_utf8_lossy(body).contains("not now"));
}

/// An `Origin` that echoes the request body back with `status`, and
/// answers an upgrade with a socket (`status` 101) or a refusal with
/// `status`.
struct Echo {
    /// Status the reply carries.
    status: u16,
}

#[async_trait::async_trait(?Send)]
impl Origin for Echo {
    async fn fetch(
        &self,
        _request: bookclerk_plugin_abi::origin::CheckedOriginRequest,
        body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    ) -> Result<OriginReply, PluginError> {
        let mut bytes = Vec::new();
        if let Some(mut body) = body {
            body.read_to_end(&mut bytes).await.unwrap();
        }
        Ok(OriginReply {
            status: self.status,
            headers: Vec::new(),
            body: Some(Box::pin(std::io::Cursor::new(bytes))),
        })
    }

    async fn connect(
        &self,
        request: bookclerk_plugin_abi::origin::CheckedOriginRequest,
        attachment: Vec<u8>,
    ) -> Result<OriginConnected, PluginError> {
        Ok(if self.status == 101 {
            OriginConnected::Socket(SocketInfo {
                socket: "o1".into(),
                url: format!("https://library.example{}", request.target),
                protocol: None,
                attachment,
            })
        } else {
            OriginConnected::Refused(OriginRefusal {
                status: self.status,
                headers: Vec::new(),
                body: b"no".to_vec(),
            })
        })
    }
}

/// A checked request for the vat-side tests.
fn checked() -> bookclerk_plugin_abi::origin::CheckedOriginRequest {
    bookclerk_plugin_abi::origin::check_origin_request(&OriginRequest {
        method: "GET".into(),
        target: "/".into(),
        headers: Vec::new(),
        client_address: None,
    })
    .unwrap()
}

/// A body channel carrying `bytes` and then closing.
fn body_of(bytes: &[u8]) -> mpsc::Receiver<Result<Vec<u8>, String>> {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(Ok(bytes.to_vec())).unwrap();
    rx
}

/// A table holding `slot` under `grant-1`.
fn table_with(slot: GrantedSlot) -> GrantedTable {
    Rc::new(RefCell::new(HashMap::from([("grant-1".to_owned(), slot)])))
}

#[tokio::test]
async fn dispatch_serves_through_the_granted_origin() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let table = table_with(GrantedSlot::origin_only(
                Rc::new(Echo { status: 200 }),
                Instant::now() + Duration::from_secs(60),
            ));
            let opened = dispatch(&table, "grant-1".into(), checked(), body_of(b"ping"))
                .await
                .unwrap();
            assert_eq!(opened.head.status, 200);
            let mut rx = opened.body_rx.unwrap();
            let mut body = Vec::new();
            while let Some(chunk) = rx.recv().await {
                body.extend_from_slice(&chunk.unwrap());
            }
            assert_eq!(body, b"ping");
        })
        .await;
}

#[tokio::test]
async fn dispatch_refuses_unknown_expired_and_ungranted_slots() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let empty: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
            let err = dispatch(&empty, "grant-1".into(), checked(), body_of(b""))
                .await
                .err()
                .unwrap();
            assert_eq!(err.code, PluginErrorCode::Unauthorized);

            let expired = table_with(GrantedSlot::origin_only(
                Rc::new(Echo { status: 200 }),
                Instant::now() - Duration::from_secs(1),
            ));
            let err = dispatch(&expired, "grant-1".into(), checked(), body_of(b""))
                .await
                .err()
                .unwrap();
            assert_eq!(err.code, PluginErrorCode::Unauthorized);
            assert!(expired.borrow().is_empty());

            let ungranted = table_with(GrantedSlot::sockets_free(
                Instant::now() + Duration::from_secs(60),
            ));
            let err = dispatch(&ungranted, "grant-1".into(), checked(), body_of(b""))
                .await
                .err()
                .unwrap();
            assert_eq!(err.code, PluginErrorCode::Forbidden);
        })
        .await;
}

#[tokio::test]
async fn dispatch_refuses_a_bad_library_status() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let table = table_with(GrantedSlot::origin_only(
                Rc::new(Echo { status: 42 }),
                Instant::now() + Duration::from_secs(60),
            ));
            assert!(dispatch(&table, "grant-1".into(), checked(), body_of(b""))
                .await
                .is_err());
        })
        .await;
}

/// An upgrade head for the vat-side tests.
fn upgrade() -> bookclerk_plugin_abi::origin::CheckedOriginRequest {
    bookclerk_plugin_abi::origin::check_origin_connect(&OriginRequest {
        method: "GET".into(),
        target: "/live".into(),
        headers: Vec::new(),
        client_address: Some("203.0.113.9".into()),
    })
    .unwrap()
}

#[tokio::test]
async fn dispatch_connect_answers_the_socket_or_a_checked_refusal() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let live = Instant::now() + Duration::from_secs(60);
            let table = table_with(GrantedSlot::origin_only(
                Rc::new(Echo { status: 101 }),
                live,
            ));
            let connected = dispatch_connect(&table, "grant-1".into(), upgrade(), b"a".to_vec())
                .await
                .unwrap();
            assert!(
                matches!(connected, OriginConnected::Socket(ref info) if info.attachment == b"a")
            );

            let table = table_with(GrantedSlot::origin_only(
                Rc::new(Echo { status: 403 }),
                live,
            ));
            let connected = dispatch_connect(&table, "grant-1".into(), upgrade(), Vec::new())
                .await
                .unwrap();
            assert!(matches!(
                connected,
                OriginConnected::Refused(OriginRefusal { status: 403, .. })
            ));

            // A refusal that breaks a rule does not reach the isolate.
            let table = table_with(GrantedSlot::origin_only(
                Rc::new(Echo { status: 150 }),
                live,
            ));
            assert!(
                dispatch_connect(&table, "grant-1".into(), upgrade(), Vec::new())
                    .await
                    .is_err()
            );

            let ungranted = table_with(GrantedSlot::sockets_free(live));
            let err = dispatch_connect(&ungranted, "grant-1".into(), upgrade(), Vec::new())
                .await
                .err()
                .unwrap();
            assert_eq!(err.code, PluginErrorCode::Forbidden);
        })
        .await;
}

/// `POST /origin/connect` under the test grant with `body`, answered with
/// `answer`. Returns the raw response and whether anything was dispatched.
async fn connect_exchange(body: &[u8], answer: OriginConnected) -> (Vec<u8>, bool) {
    let mut request = format!(
        "POST /origin/connect HTTP/1.1\r\nAuthorization: Bearer grant-1\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    let (cmds_tx, mut cmds_rx) = mpsc::channel(8);
    let dispatched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&dispatched);
    tokio::spawn(async move {
        while let Some(cmd) = cmds_rx.recv().await {
            if let GrantedCmd::OriginConnect {
                invocation,
                request,
                attachment,
                resp,
            } = cmd
            {
                assert_eq!(invocation, "grant-1");
                assert_eq!(request.method, "GET");
                assert_eq!(attachment, b"pair");
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = resp.send(Ok(answer.clone()));
            }
        }
    });
    let (client, server) = tokio::io::duplex(1 << 16);
    let serve = tokio::spawn(async move { handle_conn(server, "bridge", cmds_tx).await });
    let (mut reader, mut writer) = tokio::io::split(client);
    writer.write_all(&request).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_to_end(&mut out))
        .await
        .expect("http response")
        .unwrap();
    drop(writer);
    let _ = serve.await;
    (out, dispatched.load(std::sync::atomic::Ordering::SeqCst))
}

#[tokio::test]
async fn connect_route_answers_the_socket_as_json() {
    let body = br#"{"request":{"method":"GET","target":"/live","headers":[{"name":"Sec-WebSocket-Key","value":"x"}],"clientAddress":"203.0.113.9"},"attachment":"cGFpcg=="}"#;
    let socket = OriginConnected::Socket(SocketInfo {
        socket: "o3".into(),
        url: "https://library.example/live".into(),
        protocol: Some("v1".into()),
        attachment: b"pair".to_vec(),
    });
    let (raw, dispatched) = connect_exchange(body, socket.clone()).await;
    assert!(dispatched);
    let (head, payload) = split_response(&raw);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let answered: OriginConnected = serde_json::from_slice(payload).unwrap();
    assert_eq!(answered, socket);
}

#[tokio::test]
async fn connect_route_refuses_a_bad_upgrade_before_dispatch() {
    let refusal = OriginConnected::Refused(OriginRefusal::default());
    for body in [
        &br#"{"request":{"method":"POST","target":"/live","headers":[]},"attachment":"cGFpcg=="}"#[..],
        br#"{"request":{"method":"GET","target":"/live","headers":[{"name":"Upgrade","value":"websocket"}]}}"#,
        br#"{"request":{"method":"GET","target":"/live","headers":[]},"extra":1}"#,
        b"not json",
    ] {
        let (raw, dispatched) = connect_exchange(body, refusal.clone()).await;
        assert!(!dispatched, "{}", String::from_utf8_lossy(body));
        let (head, _) = split_response(&raw);
        assert!(!head.starts_with("HTTP/1.1 200"), "{head}");
    }
}
