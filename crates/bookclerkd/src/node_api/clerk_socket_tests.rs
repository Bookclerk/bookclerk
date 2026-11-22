//! The clerk socket: who is admitted, what a card hears, the pages of its
//! person's books, and the close once the card ends.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderValue;
use bookclerk_library::control_plane::LiveTopic;
use bookclerk_library::{hash_token, AcquireStatus, LibraryStore, NewBook, UserRole};
use bookclerk_node_proto::{
    parse_library_frame, ClerkFrame, ClerkSocketError, ClerkTopic, LibraryFrame,
    CLERK_CONNECT_PATH, CLERK_SUBPROTOCOL,
};
use futures::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::Message;

use crate::api::AppState;
use crate::live::{poke, CLOSE_UNAUTHORIZED, REAUTH_INTERVAL};

/// The operator token of the served daemon.
const TOKEN: &str = "clerk-socket-operator-token";

/// The store plugin the test books come from.
const PLUGIN: &str = "path:file:///tmp/store#audible";

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
    let app = crate::api::router(Arc::clone(&state), None)
        .layer(axum::Extension(crate::node_api::transport::LocalAddr(addr)));
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

/// The test's socket.
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens the socket with `headers`; answers the socket or the refusal's
/// HTTP status.
async fn open(addr: SocketAddr, headers: &[(&str, &str)]) -> Result<Socket, u16> {
    let mut request = format!("ws://127.0.0.1:{}{CLERK_CONNECT_PATH}", addr.port())
        .into_client_request()
        .unwrap();
    for (name, value) in headers {
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

/// What the socket did next within `wait`.
#[derive(Debug, PartialEq)]
enum Next {
    /// A library frame, checked by the clerk's own decoder.
    Frame(LibraryFrame),
    /// The socket closed with this code (0 when none was sent).
    Closed(u16),
    /// Nothing arrived.
    Quiet,
}

/// The next frame within `wait`.
async fn next(socket: &mut Socket, wait: Duration) -> Next {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, socket.next()).await {
            Err(_) => return Next::Quiet,
            Ok(Some(Ok(Message::Text(text)))) => {
                let frame = parse_library_frame(text.as_bytes())
                    .unwrap_or_else(|| panic!("the clerk refuses {text}"));
                return Next::Frame(frame);
            }
            Ok(Some(Ok(Message::Close(frame)))) => {
                return Next::Closed(frame.map(|f| u16::from(f.code)).unwrap_or(0))
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_)) | None) => return Next::Closed(0),
        }
    }
}

/// A member with a library card; answers the user id and the card token.
async fn member_with_card(store: &LibraryStore, name: &str) -> (i64, String) {
    let user = store
        .create_user(UserRole::Member, Some(name), None)
        .await
        .unwrap()
        .id;
    let identity = store
        .ensure_local_portal_identity(user, None)
        .await
        .unwrap();
    let token: String = std::iter::repeat_n(name.chars().next().unwrap(), 43).collect();
    store
        .insert_clerk_session(
            &hash_token(&token),
            identity.id,
            chrono::Utc::now() + chrono::Duration::days(1),
            "laptop",
        )
        .await
        .unwrap();
    (user, token)
}

/// Connects `account` for `user` and records an acquired book `asin` on it.
async fn hold(store: &LibraryStore, user: i64, account: &str, asin: &str) {
    store
        .upsert_account(PLUGIN, account, "us", None, true, "audible")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, account, "test")
        .await
        .unwrap();
    store
        .upsert_book(&NewBook {
            product_id: asin.into(),
            plugin_key: PLUGIN.into(),
            source: "audible".into(),
            account_id: account.into(),
            asin: Some(asin.into()),
            isbn: None,
            marketplace: "us".into(),
            title: format!("Title {asin}"),
            authors: Some("Ann Author".into()),
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: Some(600),
            is_abridged: false,
            content_kind: "book".into(),
            categories: None,
            subtitle: None,
            published_at: None,
        })
        .await
        .unwrap();
    store
        .set_acquire_status(asin, account, AcquireStatus::Acquired, None, None)
        .await
        .unwrap();
}

/// The card header for `token`.
fn card(token: &str) -> String {
    format!("Bookclerk-Clerk {token}")
}

#[tokio::test]
async fn only_a_library_card_offering_the_subprotocol_is_admitted() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, addr) = served(&files).await;
    let (_, token) = member_with_card(&store, "riley").await;
    let bearer = format!("Bearer {TOKEN}");
    let unknown = card(&"z".repeat(43));
    let card = card(&token);

    assert_eq!(
        open(addr, &[("sec-websocket-protocol", CLERK_SUBPROTOCOL)])
            .await
            .err(),
        Some(401),
        "no card"
    );
    assert_eq!(
        open(
            addr,
            &[
                ("authorization", &bearer),
                ("sec-websocket-protocol", CLERK_SUBPROTOCOL)
            ]
        )
        .await
        .err(),
        Some(401),
        "the operator's token is not a card"
    );
    assert_eq!(
        open(
            addr,
            &[
                ("authorization", &unknown),
                ("sec-websocket-protocol", CLERK_SUBPROTOCOL)
            ]
        )
        .await
        .err(),
        Some(401),
        "a card that is not signed in"
    );
    assert_eq!(
        open(addr, &[("authorization", &card)]).await.err(),
        Some(400),
        "no subprotocol"
    );
    assert!(open(
        addr,
        &[
            ("authorization", &card),
            ("sec-websocket-protocol", CLERK_SUBPROTOCOL)
        ]
    )
    .await
    .is_ok());
}

#[tokio::test]
async fn a_card_reads_the_librarys_books_and_hears_their_topics_until_it_ends() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, addr) = served(&files).await;
    let (riley, token) = member_with_card(&store, "riley").await;
    let (sam, _) = member_with_card(&store, "sam").await;
    hold(&store, riley, "riley-acct", "B0RILEY001").await;
    hold(&store, sam, "sam-acct", "B0SAM00001").await;

    let header = card(&token);
    let mut socket = open(
        addr,
        &[
            ("authorization", &header),
            ("sec-websocket-protocol", CLERK_SUBPROTOCOL),
        ],
    )
    .await
    .expect("a card is admitted");
    assert_eq!(
        next(&mut socket, Duration::from_secs(5)).await,
        Next::Frame(LibraryFrame::Hello {
            topics: ClerkTopic::ALL.into_iter().collect(),
        })
    );

    socket
        .send(Message::Text(
            ClerkFrame::Books {
                after: bookclerk_node_proto::WindowCursor::start(),
            }
            .encode()
            .into(),
        ))
        .await
        .unwrap();
    let Next::Frame(LibraryFrame::BooksPage {
        after,
        works,
        next: cursor,
    }) = next(&mut socket, Duration::from_secs(5)).await
    else {
        panic!("a books frame is answered with a page");
    };
    assert_eq!(
        (after, cursor),
        (bookclerk_node_proto::WindowCursor::start(), None)
    );
    let mut products: Vec<(&str, bool)> = works
        .iter()
        .flat_map(|work| {
            work.editions
                .iter()
                .map(|e| (e.edition.product_id.as_str(), e.yours))
        })
        .collect();
    products.sort_unstable();
    assert_eq!(
        products,
        vec![("B0RILEY001", true), ("B0SAM00001", false)],
        "every book of the library, Riley's marked"
    );

    for bad in [
        r#"{"v":1,"type":"books","after":0}"#,
        r#"{"v":1,"type":"claim"}"#,
        "not json",
    ] {
        socket.send(Message::Text(bad.into())).await.unwrap();
        assert_eq!(
            next(&mut socket, Duration::from_secs(5)).await,
            Next::Frame(LibraryFrame::Error {
                code: ClerkSocketError::Invalid,
            }),
            "{bad}"
        );
    }

    poke(LiveTopic::Jobs);
    poke(LiveTopic::Users);
    poke(LiveTopic::Library);
    let mut heard = Vec::new();
    while let Next::Frame(LibraryFrame::Changed { topics }) =
        next(&mut socket, Duration::from_secs(2)).await
    {
        heard.extend(topics);
        if heard.contains(&ClerkTopic::Library) {
            break;
        }
    }
    assert_eq!(
        heard,
        vec![ClerkTopic::Library],
        "operator topics are not reported"
    );

    assert!(store
        .delete_clerk_session(&hash_token(&token))
        .await
        .unwrap());
    let mut closed = None;
    for _ in 0..10 {
        match next(&mut socket, REAUTH_INTERVAL * 4).await {
            Next::Closed(code) => {
                closed = Some(code);
                break;
            }
            Next::Frame(_) => continue,
            Next::Quiet => break,
        }
    }
    assert_eq!(
        closed,
        Some(CLOSE_UNAUTHORIZED),
        "the socket closes once the card ends"
    );
}

#[tokio::test]
async fn a_binary_frame_closes_the_socket() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, addr) = served(&files).await;
    let (_, token) = member_with_card(&store, "riley").await;
    let header = card(&token);
    let mut socket = open(
        addr,
        &[
            ("authorization", &header),
            ("sec-websocket-protocol", CLERK_SUBPROTOCOL),
        ],
    )
    .await
    .unwrap();
    assert!(matches!(
        next(&mut socket, Duration::from_secs(5)).await,
        Next::Frame(LibraryFrame::Hello { .. })
    ));
    socket
        .send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut socket, Duration::from_secs(5)).await,
        Next::Closed(1003)
    );
}

#[tokio::test]
async fn a_person_who_must_enroll_a_second_factor_is_refused_and_closed() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, addr) = served(&files).await;
    let (_, token) = member_with_card(&store, "riley").await;
    let header = card(&token);
    let headers = [
        ("authorization", header.as_str()),
        ("sec-websocket-protocol", CLERK_SUBPROTOCOL),
    ];
    let mut socket = open(addr, &headers).await.expect("a card is admitted");
    assert!(matches!(
        next(&mut socket, Duration::from_secs(5)).await,
        Next::Frame(LibraryFrame::Hello { .. })
    ));

    state.config.write().await.daemon.auth.require_second_factor = true;
    assert_eq!(
        open(addr, &headers).await.err(),
        Some(403),
        "a new socket is refused until the person enrolls"
    );
    let mut closed = None;
    for _ in 0..10 {
        match next(&mut socket, REAUTH_INTERVAL * 4).await {
            Next::Closed(code) => {
                closed = Some(code);
                break;
            }
            Next::Frame(_) => continue,
            Next::Quiet => break,
        }
    }
    assert_eq!(
        closed,
        Some(CLOSE_UNAUTHORIZED),
        "an open socket closes at the next card check"
    );
}

#[tokio::test]
async fn a_full_house_is_refused_with_retry_after() {
    // Every socket test holds the process key lock, so no other socket is
    // admitted while this one holds every permit.
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, addr) = served(&files).await;
    let (_, token) = member_with_card(&store, "riley").await;
    let header = card(&token);
    let headers = [
        ("authorization", header.as_str()),
        ("sec-websocket-protocol", CLERK_SUBPROTOCOL),
    ];
    let all = u32::try_from(super::MAX_CLERK_SOCKETS).unwrap();
    let permits = Arc::clone(&super::SOCKETS)
        .try_acquire_many_owned(all)
        .unwrap();
    assert_eq!(open(addr, &headers).await.err(), Some(503));
    drop(permits);
    assert!(
        open(addr, &headers).await.is_ok(),
        "admitted once a place frees"
    );
}

#[test]
fn only_library_and_discover_reach_a_clerk() {
    use super::clerk_topic;
    let reported: Vec<ClerkTopic> = [
        LiveTopic::Library,
        LiveTopic::Discover,
        LiveTopic::Jobs,
        LiveTopic::Deliveries,
        LiveTopic::Nodes,
        LiveTopic::Config,
        LiveTopic::Consents,
        LiveTopic::Libraries,
        LiveTopic::Users,
    ]
    .into_iter()
    .filter_map(clerk_topic)
    .collect();
    assert_eq!(reported, vec![ClerkTopic::Library, ClerkTopic::Discover]);
}

#[test]
fn the_subprotocol_is_found_in_any_offer() {
    use super::offers_subprotocol;
    use axum::http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderMap};
    let offer = |values: &[&[u8]]| {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_bytes(value).unwrap(),
            );
        }
        offers_subprotocol(&headers)
    };
    assert!(offer(&[CLERK_SUBPROTOCOL.as_bytes()]));
    assert!(offer(&[format!("chat, {CLERK_SUBPROTOCOL}").as_bytes()]));
    assert!(offer(&[b"chat", CLERK_SUBPROTOCOL.as_bytes()]));
    assert!(!offer(&[]));
    assert!(!offer(&[b"chat, bookclerk-clerk.v2"]));
    assert!(!offer(&[b"bookclerk-clerk.v1x"]));
    assert!(!offer(&[b"\xffbookclerk-clerk.v1"]), "not UTF-8");
}

#[tokio::test]
async fn a_page_is_not_served_once_the_card_ends() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, addr) = served(&files).await;
    let (riley, token) = member_with_card(&store, "riley").await;
    hold(&store, riley, "riley-acct", "B0RILEY001").await;
    let header = card(&token);
    let mut socket = open(
        addr,
        &[
            ("authorization", &header),
            ("sec-websocket-protocol", CLERK_SUBPROTOCOL),
        ],
    )
    .await
    .unwrap();
    assert!(matches!(
        next(&mut socket, Duration::from_secs(5)).await,
        Next::Frame(LibraryFrame::Hello { .. })
    ));
    assert!(store
        .delete_clerk_session(&hash_token(&token))
        .await
        .unwrap());
    socket
        .send(Message::Text(
            ClerkFrame::Books {
                after: bookclerk_node_proto::WindowCursor::start(),
            }
            .encode()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        next(&mut socket, Duration::from_secs(5)).await,
        Next::Closed(CLOSE_UNAUTHORIZED),
        "the page request closes the socket instead of reading books"
    );
}

/// Log output captured in memory.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

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

/// One GET of a copy's file: the status line and headers as lowercase
/// text, and the body.
async fn get_copy_file(
    addr: SocketAddr,
    path: &str,
    authorization: Option<&str>,
    range: Option<&str>,
) -> (String, Vec<u8>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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

/// The next thing the socket did within ten seconds that is not a
/// `changed` frame, which the library may send at any time.
async fn answer_of(socket: &mut Socket) -> Next {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match next(socket, left).await {
            Next::Frame(LibraryFrame::Changed { .. }) => {}
            other => return other,
        }
    }
}

/// Sends `frame` and answers what came back.
async fn ask(socket: &mut Socket, frame: &ClerkFrame) -> Next {
    socket
        .send(Message::Text(frame.encode().into()))
        .await
        .unwrap();
    answer_of(socket).await
}

/// A card takes the copy the library holds of an edition another member
/// provided: `copy.grant` answers what the copy is, its files with their
/// sizes and a token; each file downloads whole or by range with it and
/// with nothing else; an edition without a copy is `not_found`; signing
/// the card out refuses the next download; and at TRACE neither the
/// token nor the card reaches a log this daemon writes. The WebSocket
/// library's own records are left out of that check: another test in this
/// binary may have installed a plain `log` bridge, through which this
/// test's own client traces the frame it received. The daemon's bridge
/// drops those records (`tests/websocket_trace_logs.rs` checks it with the
/// production log setup).
#[tokio::test]
async fn a_card_downloads_the_librarys_copy_until_it_is_signed_out() {
    use bookclerk_node_proto::{CopyRole, COPIES_PATH};

    let _dek = crate::auth::tests::process_dek_lock().await;
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);

    let files = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let (_state, store, addr) = served(&files).await;
    crate::node_api::uploads::set_test_destination(files.path(), out.path());
    let (riley, _) = member_with_card(&store, "riley").await;
    let (_, token) = member_with_card(&store, "sam").await;
    hold(&store, riley, "riley-acct", "B0RILEY001").await;
    hold(&store, riley, "riley-acct", "B0RILEY002").await;

    // The library acquired the first: its audio and a cover lie on the
    // destination and are recorded as the edition's files.
    const KEY: &str = "Ann Author/Title/Title.m4b";
    const COVER: &str = "Ann Author/Title/cover.jpg";
    let audio: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::create_dir_all(out.path().join("Ann Author/Title")).unwrap();
    std::fs::write(out.path().join(KEY), &audio).unwrap();
    std::fs::write(out.path().join(COVER), b"cover").unwrap();
    store
        .set_acquire_status(
            "B0RILEY001",
            "riley-acct",
            AcquireStatus::Acquired,
            Some(KEY),
            None,
        )
        .await
        .unwrap();
    store
        .set_acquire_status(
            "B0RILEY002",
            "riley-acct",
            AcquireStatus::NotAcquired,
            None,
            None,
        )
        .await
        .unwrap();
    let book = store
        .held_title("B0RILEY001", "riley-acct")
        .await
        .unwrap()
        .unwrap();
    let owner = store
        .file_owner_of_holding(&book.holding_id)
        .await
        .unwrap()
        .unwrap();
    for (key, role) in [
        (KEY, bookclerk_library::FileRole::Audio),
        (COVER, bookclerk_library::FileRole::Cover),
    ] {
        assert!(store
            .record_edition_file("local", key, &owner, role)
            .await
            .unwrap());
    }

    let header = card(&token);
    let mut socket = open(
        addr,
        &[
            ("authorization", &header),
            ("sec-websocket-protocol", CLERK_SUBPROTOCOL),
        ],
    )
    .await
    .expect("a card is admitted");
    assert!(matches!(
        next(&mut socket, Duration::from_secs(5)).await,
        Next::Frame(LibraryFrame::Hello { .. })
    ));
    let grant = |product_id: &str| ClerkFrame::CopyGrant {
        store: "audible".into(),
        marketplace: "us".into(),
        product_id: product_id.into(),
    };

    // Not acquired, and not in the library at all: one answer.
    for product in ["B0RILEY002", "B0NOBODY01"] {
        assert_eq!(
            ask(&mut socket, &grant(product)).await,
            Next::Frame(LibraryFrame::Error {
                code: ClerkSocketError::NotFound,
            }),
            "{product}"
        );
    }
    // A malformed ask is not answered as a copy.
    socket
        .send(Message::Text(
            r#"{"v":1,"type":"copy.grant","store":"Not A Store","product_id":"B0RILEY001"}"#.into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        answer_of(&mut socket).await,
        Next::Frame(LibraryFrame::Error {
            code: ClerkSocketError::Invalid,
        })
    );

    let Next::Frame(LibraryFrame::CopyGranted(granted)) =
        ask(&mut socket, &grant("B0RILEY001")).await
    else {
        panic!("a copy.grant for a held copy is answered with copy.granted");
    };
    assert_eq!(
        (
            granted.store.as_str(),
            granted.marketplace.as_str(),
            granted.product_id.as_str(),
            granted.expires_in_secs
        ),
        ("audible", "us", "B0RILEY001", 3600)
    );
    let listed: Vec<(&str, CopyRole, u64)> = granted
        .files
        .iter()
        .map(|file| (file.name.as_str(), file.role, file.size))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("Title.m4b", CopyRole::Audio, 100_000),
            ("cover.jpg", CopyRole::Cover, 5),
        ]
    );
    let copy_token = granted.token.expose().to_owned();
    let authorization = format!("Bookclerk-Copy {copy_token}");
    let file = |n: usize| format!("{COPIES_PATH}{}/files/{n}", granted.copy_id);

    let (head, body) = get_copy_file(addr, &file(0), Some(&authorization), None).await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(body, audio);
    let (head, body) =
        get_copy_file(addr, &file(0), Some(&authorization), Some("bytes=10-19")).await;
    assert!(head.starts_with("http/1.1 206"), "{head}");
    assert_eq!(body, &audio[10..20]);
    let (head, body) = get_copy_file(addr, &file(1), Some(&authorization), None).await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(body, b"cover");

    // The card itself, in either scheme, is not a copy token, and nor is
    // no token.
    let as_copy = format!("Bookclerk-Copy {token}");
    for authorization in [None, Some(header.as_str()), Some(as_copy.as_str())] {
        let (head, _) = get_copy_file(addr, &file(0), authorization, None).await;
        assert!(
            head.starts_with("http/1.1 401"),
            "{authorization:?}: {head}"
        );
    }

    // Signed out: the same copy token opens nothing, and the socket's
    // next ask closes it.
    assert!(store
        .delete_clerk_session(&hash_token(&token))
        .await
        .unwrap());
    let (head, _) = get_copy_file(addr, &file(0), Some(&authorization), None).await;
    assert!(head.starts_with("http/1.1 401"), "{head}");
    assert_eq!(
        ask(&mut socket, &grant("B0RILEY001")).await,
        Next::Closed(CLOSE_UNAUTHORIZED)
    );

    let logged = String::from_utf8_lossy(&logs.0.lock().unwrap())
        .lines()
        .filter(|line| !line.contains("tungstenite::"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        logged.contains("granted a copy of an edition to a library card"),
        "{logged}"
    );
    assert!(
        !logged.contains(&copy_token),
        "the copy token reached a log"
    );
    assert!(!logged.contains(&token), "the card reached a log");
    assert!(
        !logged.contains(&hash_token(&token)),
        "the card's hash reached a log"
    );
}
