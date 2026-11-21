//! Library cards end to end in one process: a clerk install signs in to a
//! served library with a card (start, the library's consent, callback),
//! its reader reads the person's books over the clerk socket into "Your
//! books in other libraries", reads again when the library's books change,
//! and signs the link out when the library ends the card; signing out here
//! drops the card and the books.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::routing::{get, post};
use bookclerk_library::control_plane::{
    list_clerk_links, load_clerk_link, record_library_link, window_view, ClerkLinkState,
    LibraryLinkKeys, LiveTopic, NewLibraryLink,
};
use bookclerk_library::{AcquireStatus, LibraryStore, NewBook, UserRole};
use serde_json::{json, Value};
use tokio::sync::watch;

use super::ClerkReader;
use crate::api::AppState;
use crate::auth::UserCaller;
use crate::live::poke;
use crate::node_api::branch::tests::{call, LOOPBACK};

/// The store plugin the test books come from.
const PLUGIN: &str = "path:file:///tmp/store#audible";

/// Serves `app` on a loopback port, as the daemon's listener does.
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

/// A library served on loopback with one member. Answers its state, store,
/// origin and the member.
async fn library(files: &tempfile::TempDir) -> (Arc<AppState>, LibraryStore, String, i64) {
    let (state, store) = crate::node_api::tests::test_state(files).await;
    let addr = serve(crate::api::router(Arc::clone(&state), None)).await;
    state.config.write().await.integrations.public_origin =
        Some(format!("http://127.0.0.1:{}", addr.port()));
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
    (
        state,
        store,
        format!("http://localhost:{}", addr.port()),
        member.id,
    )
}

/// The clerk install's card routes as `caller` reaches them.
fn clerk_routes(state: &Arc<AppState>, caller: UserCaller) -> axum::Router {
    axum::Router::new()
        .route("/api/config/clerk", get(super::api::get_clerk_cards))
        .route(
            "/api/config/clerk/inspect",
            post(super::api::post_clerk_inspect),
        )
        .route(
            "/api/config/clerk/start",
            post(super::api::post_clerk_start),
        )
        .route(
            "/api/config/clerk/callback",
            post(super::api::post_clerk_callback),
        )
        .route(
            "/api/config/clerk/sign-out",
            post(super::api::post_clerk_sign_out),
        )
        .route(
            "/api/config/clerk/request",
            post(super::requests::post_clerk_request),
        )
        .route(
            "/api/config/join/window",
            get(crate::library_link::join::get_join_window),
        )
        .route(
            "/api/config/join",
            get(crate::library_link::join::get_join_status),
        )
        .layer(axum::Extension(caller))
        .with_state(Arc::clone(state))
}

/// The library's member approves the authorize URL's request; answers the
/// callback's query pairs.
async fn consent(
    library: &Arc<AppState>,
    member: i64,
    authorize_url: &str,
) -> std::collections::HashMap<String, String> {
    let url = url::Url::parse(authorize_url).unwrap();
    assert_eq!(url.path(), "/oidc/authorize");
    let mut body = serde_json::Map::new();
    for (name, value) in url.query_pairs() {
        body.insert(name.into_owned(), json!(value.into_owned()));
    }
    body.insert("approve".into(), json!(true));
    let peer: SocketAddr = LOOPBACK.0.parse().unwrap();
    let local: SocketAddr = LOOPBACK.1.parse().unwrap();
    let app = axum::Router::new()
        .route(
            "/api/auth/clerk-consent",
            post(crate::node_api::clerk::post_clerk_consent),
        )
        .layer(axum::Extension(UserCaller::User { user_id: member }))
        .layer(axum::Extension(crate::node_api::transport::LocalAddr(
            local,
        )))
        .layer(axum::extract::connect_info::MockConnectInfo(peer))
        .with_state(Arc::clone(library));
    let (status, _, answer) = call(
        &app,
        "POST",
        "/api/auth/clerk-consent",
        Some(Value::Object(body)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let redirect = url::Url::parse(answer["redirect"].as_str().unwrap()).unwrap();
    assert_eq!(redirect.path(), bookclerk_node_proto::CLERK_CALLBACK_PATH);
    redirect.query_pairs().into_owned().collect()
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

/// Waits up to `secs` seconds for `check`.
async fn eventually<F, Fut>(what: &str, secs: u64, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The titles in the clerk's merged view.
async fn titles(store: &LibraryStore) -> Vec<String> {
    window_view(store)
        .await
        .unwrap()
        .iter()
        .map(|entry| entry.title().to_string())
        .collect()
}

/// Signs `clerk` in to `origin` as `member` through the card routes;
/// answers the library id.
async fn sign_in(
    clerk: &axum::Router,
    library_state: &Arc<AppState>,
    origin: &str,
    member: i64,
) -> String {
    let (status, _, inspected) = call(
        clerk,
        "POST",
        "/api/config/clerk/inspect",
        Some(json!({ "address": origin })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{inspected}");
    let (status, headers, started) = call(
        clerk,
        "POST",
        "/api/config/clerk/start",
        Some(json!({ "address": origin, "clerk_name": "Laptop" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(headers[axum::http::header::CACHE_CONTROL], "no-store");
    assert_eq!(started["clerk_name"], "Laptop");
    let back = consent(
        library_state,
        member,
        started["authorize_url"].as_str().unwrap(),
    )
    .await;
    let (status, _, signed_in) = call(
        clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "code": back["code"], "state": back["state"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{signed_in}");
    assert_eq!(signed_in["card"]["state"], "active");
    assert_eq!(signed_in["card"]["display_name"], "Riley");
    // The sign-in is spent.
    let (status, _, again) = call(
        clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "code": back["code"], "state": back["state"] })),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{again}");
    signed_in["card"]["library_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_card_signs_in_reads_the_books_and_signs_out_when_the_library_ends_it() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let library_files = tempfile::tempdir().unwrap();
    let (library_state, library_store, origin, member) = library(&library_files).await;
    hold(&library_store, member, "riley-acct", "B0CLERK001").await;
    let clerk_files = tempfile::tempdir().unwrap();
    let (clerk_state, clerk_store) = crate::node_api::tests::test_state(&clerk_files).await;
    let clerk = clerk_routes(&clerk_state, UserCaller::Operator);
    let (_, _, status) = call(&clerk, "GET", "/api/config/join", None).await;
    assert_eq!(status["fresh"], true, "{status}");

    let library_id = sign_in(&clerk, &library_state, &origin, member).await;
    // A card is enough for the welcome screen not to come back.
    let (_, _, status) = call(&clerk, "GET", "/api/config/join", None).await;
    assert_eq!(status["fresh"], false, "{status}");
    let (_, _, cards) = call(&clerk, "GET", "/api/config/clerk", None).await;
    assert_eq!(cards["cards"].as_array().unwrap().len(), 1, "{cards}");
    assert_eq!(cards["cards"][0]["clerk_name"], "Laptop");

    let (stop, shutdown) = watch::channel(false);
    let reader = tokio::spawn(
        ClerkReader {
            library: clerk_store.clone(),
            library_id: library_id.clone(),
        }
        .run(shutdown),
    );
    eventually("the first read", 20, || async {
        titles(&clerk_store).await == vec!["Title B0CLERK001"]
    })
    .await;
    let (status, _, window) = call(&clerk, "GET", "/api/config/join/window", None).await;
    assert_eq!(status, StatusCode::OK, "{window}");
    assert_eq!(window["cards"][0]["library_id"], library_id.as_str());
    assert_eq!(window["books"].as_array().unwrap().len(), 1);

    // A change at the library is read again.
    hold(&library_store, member, "riley-acct", "B0CLERK002").await;
    poke(LiveTopic::Library);
    eventually("the second read", 20, || async {
        titles(&clerk_store).await.len() == 2
    })
    .await;

    // The library ends the card: the link signs out and the books go.
    let identity = library_store
        .ensure_local_portal_identity(member, None)
        .await
        .unwrap();
    let sessions = library_store
        .list_portal_session_records_for_identity(identity.id)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    for session in sessions {
        assert!(library_store
            .delete_clerk_session(&session.token_hash)
            .await
            .unwrap());
    }
    eventually("the sign-out", 30, || async {
        load_clerk_link(&clerk_store, &library_id)
            .await
            .unwrap()
            .is_some_and(|link| link.state == ClerkLinkState::SignedOut)
    })
    .await;
    assert!(titles(&clerk_store).await.is_empty());
    tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("the reader stops once signed out")
        .unwrap();
    drop(stop);

    // Signing out here removes the library from the list (the library had
    // already ended the card).
    let (status, _, out) = call(
        &clerk,
        "POST",
        "/api/config/clerk/sign-out",
        Some(json!({ "library_id": library_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["library_signed_out"], false);
    assert!(list_clerk_links(&clerk_store).await.unwrap().is_empty());
    let (status, _, _) = call(
        &clerk,
        "POST",
        "/api/config/clerk/sign-out",
        Some(json!({ "library_id": library_id })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_library_this_install_joined_is_refused_and_members_cannot_sign_in() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let library_files = tempfile::tempdir().unwrap();
    let (_library_state, library_store, origin, _member) = library(&library_files).await;
    let clerk_files = tempfile::tempdir().unwrap();
    let (clerk_state, clerk_store) = crate::node_api::tests::test_state(&clerk_files).await;
    let clerk = clerk_routes(&clerk_state, UserCaller::Operator);

    // An unknown sign-in has expired.
    let (status, _, expired) = call(
        &clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "code": "c", "state": "nope" })),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{expired}");
    assert_eq!(expired["error"], "join_expired");

    // An active branch link to the library refuses a card for it.
    let library = bookclerk_library::control_plane::load_library_row(&library_store)
        .await
        .unwrap()
        .unwrap();
    record_library_link(
        &clerk_store,
        &NewLibraryLink {
            parent_library_id: library.library_id.clone(),
            owner_origin: origin.clone(),
            remote_node_id: "node-1".into(),
            spki_pin: None,
        },
        &LibraryLinkKeys::generate(),
    )
    .await
    .unwrap();
    let (status, _, refused) = call(
        &clerk,
        "POST",
        "/api/config/clerk/start",
        Some(json!({ "address": origin })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "already_joined");

    // A member of the clerk install may not use the card routes.
    let member = clerk_store
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .unwrap()
        .id;
    let as_member = clerk_routes(&clerk_state, UserCaller::User { user_id: member });
    for (method, uri, body) in [
        ("GET", "/api/config/clerk", None),
        (
            "POST",
            "/api/config/clerk/inspect",
            Some(json!({ "address": origin })),
        ),
        (
            "POST",
            "/api/config/clerk/start",
            Some(json!({ "address": origin })),
        ),
        (
            "POST",
            "/api/config/clerk/callback",
            Some(json!({ "code": "c", "state": "s" })),
        ),
        (
            "POST",
            "/api/config/clerk/sign-out",
            Some(json!({ "library_id": "x" })),
        ),
    ] {
        let (status, _, _) = call(&as_member, method, uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
}

#[tokio::test]
async fn signing_out_here_ends_the_card_at_the_library() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let library_files = tempfile::tempdir().unwrap();
    let (library_state, library_store, origin, member) = library(&library_files).await;
    let clerk_files = tempfile::tempdir().unwrap();
    let (clerk_state, clerk_store) = crate::node_api::tests::test_state(&clerk_files).await;
    let clerk = clerk_routes(&clerk_state, UserCaller::Operator);
    let library_id = sign_in(&clerk, &library_state, &origin, member).await;
    let identity = library_store
        .ensure_local_portal_identity(member, None)
        .await
        .unwrap();
    let sessions = || async {
        library_store
            .list_portal_session_records_for_identity(identity.id)
            .await
            .unwrap()
            .len()
    };
    assert_eq!(sessions().await, 1);

    let (status, _, out) = call(
        &clerk,
        "POST",
        "/api/config/clerk/sign-out",
        Some(json!({ "library_id": library_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["library_signed_out"], true);
    assert_eq!(sessions().await, 0, "the library ended the card");
    assert!(list_clerk_links(&clerk_store).await.unwrap().is_empty());
}

/// A library's side of a clerk socket, as a fake library plays it.
type FakeSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// How the fake library answers the upgrade.
#[derive(Clone, Copy)]
enum Upgrade {
    /// Selects the clerk subprotocol.
    Accept,
    /// Refuses with this status.
    Refuse(u16),
}

/// Serves a fake library on loopback that answers each upgrade as
/// `upgrade` and then plays `script` on the socket; answers its origin.
async fn fake_library<F, Fut>(upgrade: Upgrade, script: F) -> String
where
    F: Fn(FakeSocket) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let script = Arc::new(script);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let script = Arc::clone(&script);
            tokio::spawn(async move {
                let answer = |_: &Request, mut response: Response| match upgrade {
                    Upgrade::Accept => {
                        response.headers_mut().insert(
                            "sec-websocket-protocol",
                            bookclerk_node_proto::CLERK_SUBPROTOCOL.parse().unwrap(),
                        );
                        Ok(response)
                    }
                    Upgrade::Refuse(status) => {
                        let mut refused = ErrorResponse::new(None);
                        *refused.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
                        Err(refused)
                    }
                };
                if let Ok(socket) = tokio_tungstenite::accept_hdr_async(stream, answer).await {
                    script(socket).await;
                }
            });
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// One work holding one owned audio edition `asin`.
fn work(asin: &str) -> bookclerk_node_proto::WindowWork {
    use bookclerk_node_proto::{
        Availability, CatalogMetadata, LookupEdition, LookupFormat, LookupIdentifier, LookupScheme,
        WindowEdition, WindowWork,
    };
    WindowWork {
        work: format!("w-{asin}"),
        identifiers: vec![LookupIdentifier {
            scheme: LookupScheme::Asin,
            value: asin.into(),
            marketplace: "us".into(),
        }],
        editions: vec![WindowEdition {
            edition: LookupEdition {
                store: "audible".into(),
                marketplace: "us".into(),
                product_id: asin.into(),
                availability: Availability::Owned,
                formats: vec![LookupFormat::Audio],
                metadata: CatalogMetadata {
                    title: format!("Title {asin}"),
                    ..CatalogMetadata::default()
                },
            },
            yours: true,
            copy: None,
        }],
    }
}

/// Sends a library frame.
async fn say(socket: &mut FakeSocket, frame: bookclerk_node_proto::LibraryFrame) {
    use futures::SinkExt as _;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            frame.encode().into(),
        ))
        .await
        .unwrap();
}

/// Sends raw text.
async fn say_raw(socket: &mut FakeSocket, text: &str) {
    use futures::SinkExt as _;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
        .await
        .unwrap();
}

/// The cursor of the next `books` frame the clerk sends.
async fn asked(socket: &mut FakeSocket) -> Option<String> {
    use futures::StreamExt as _;
    while let Some(Ok(message)) = socket.next().await {
        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
            let frame = bookclerk_node_proto::parse_clerk_frame(text.as_bytes())
                .expect("the clerk sends frames the library reads");
            let bookclerk_node_proto::ClerkFrame::Books { after } = frame else {
                panic!("this clerk asks only for pages");
            };
            return Some(after.as_str().to_owned());
        }
    }
    None
}

/// The cursor `text`.
fn cursor(text: &str) -> bookclerk_node_proto::WindowCursor {
    bookclerk_node_proto::WindowCursor::parse(text).expect("a cursor")
}

/// Closes the socket with `code`.
async fn close(socket: &mut FakeSocket, code: u16) {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    let _ = socket
        .close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        }))
        .await;
}

/// A clerk install holding an active card for the library at `origin`;
/// answers its state, store and the library id.
async fn clerk_with_card(
    files: &tempfile::TempDir,
    origin: &str,
) -> (Arc<AppState>, LibraryStore, String) {
    let (state, store) = crate::node_api::tests::test_state(files).await;
    bookclerk_library::control_plane::record_clerk_link(
        &store,
        &bookclerk_library::control_plane::NewClerkLink {
            library_id: "fake-library".into(),
            origin: origin.into(),
            spki_pin: None,
            library_name: "fake".into(),
            remote_user_id: 7,
            display_name: None,
            clerk_name: "Laptop".into(),
        },
        &bookclerk_library::control_plane::LibraryCard::new("c".repeat(43)).unwrap(),
    )
    .await
    .unwrap();
    (state, store, "fake-library".into())
}

/// As [`clerk_with_card`], for a library with library id `library_id`, so
/// its reader is the only one this process lists under that id.
async fn clerk_with_card_of(
    files: &tempfile::TempDir,
    origin: &str,
    library_id: &str,
) -> LibraryStore {
    let (_state, store) = crate::node_api::tests::test_state(files).await;
    bookclerk_library::control_plane::record_clerk_link(
        &store,
        &bookclerk_library::control_plane::NewClerkLink {
            library_id: library_id.into(),
            origin: origin.into(),
            spki_pin: None,
            library_name: "fake".into(),
            remote_user_id: 7,
            display_name: None,
            clerk_name: "Laptop".into(),
        },
        &bookclerk_library::control_plane::LibraryCard::new("c".repeat(43)).unwrap(),
    )
    .await
    .unwrap();
    store
}

/// The next frame the clerk sends, of any kind.
async fn heard(socket: &mut FakeSocket) -> Option<bookclerk_node_proto::ClerkFrame> {
    use futures::StreamExt as _;
    while let Some(Ok(message)) = socket.next().await {
        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
            return Some(
                bookclerk_node_proto::parse_clerk_frame(text.as_bytes())
                    .expect("the clerk sends frames the library reads"),
            );
        }
    }
    None
}

/// A copy the library grants of `product_id`, with one 10-byte audio file
/// downloaded from `source_store`.
fn granted_copy(product_id: &str, source_store: &str) -> bookclerk_node_proto::ClerkCopy {
    use bookclerk_node_proto::{ClerkCopy, CopyFile, CopyRole, CopyToken, WindowCopy};
    ClerkCopy {
        store: "audible".into(),
        marketplace: "us".into(),
        product_id: product_id.into(),
        copy_id: "6f1c2b7e-0d4a-4e8b-9a51-2c3d4e5f6a7b".into(),
        token: CopyToken::new("dG9rZW4"),
        expires_in_secs: 3600,
        copy: WindowCopy {
            source_store: Some(source_store.into()),
            content_type: "audio/mp4".into(),
            bitrate_kbps: Some(64),
            lossless: false,
            version: "v1".into(),
        },
        files: vec![CopyFile {
            name: "Title.m4b".into(),
            role: CopyRole::Audio,
            size: 10,
            content_type: Some("audio/mp4".into()),
            probe: None,
        }],
    }
}

/// Runs one session of a reader for `library_id` on `store`.
async fn one_session(store: &LibraryStore, library_id: &str) -> anyhow::Result<super::SessionEnd> {
    let (_stop, mut shutdown) = watch::channel(false);
    tokio::time::timeout(
        Duration::from_secs(20),
        ClerkReader {
            library: store.clone(),
            library_id: library_id.into(),
        }
        .session(&mut shutdown),
    )
    .await
    .expect("the session ends")
}

#[tokio::test]
async fn a_read_pages_through_and_a_lost_socket_keeps_the_books() {
    use bookclerk_node_proto::{ClerkTopic, LibraryFrame};
    let _dek = crate::auth::tests::process_dek_lock().await;
    let origin = fake_library(Upgrade::Accept, |mut socket| async move {
        say(
            &mut socket,
            LibraryFrame::Hello {
                topics: ClerkTopic::ALL.into_iter().collect(),
            },
        )
        .await;
        assert_eq!(asked(&mut socket).await.as_deref(), Some(""));
        say(
            &mut socket,
            LibraryFrame::BooksPage {
                after: cursor(""),
                works: vec![work("B0PAGE0001")],
                next: Some(cursor("p5")),
            },
        )
        .await;
        assert_eq!(asked(&mut socket).await.as_deref(), Some("p5"));
        say(
            &mut socket,
            LibraryFrame::BooksPage {
                after: cursor("p5"),
                works: vec![work("B0PAGE0002")],
                next: None,
            },
        )
        .await;
        // A change to another topic is not a reason to read again.
        say(
            &mut socket,
            LibraryFrame::Changed {
                topics: [ClerkTopic::Discover].into_iter().collect(),
            },
        )
        .await;
        close(&mut socket, 1000).await;
    })
    .await;
    let files = tempfile::tempdir().unwrap();
    let (_state, store, library) = clerk_with_card(&files, &origin).await;
    assert_eq!(
        one_session(&store, &library).await.unwrap(),
        super::SessionEnd::Lost
    );
    assert_eq!(
        titles(&store).await,
        vec!["Title B0PAGE0001", "Title B0PAGE0002"]
    );
}

#[tokio::test]
async fn a_library_that_ends_the_card_signs_it_out() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    for upgrade in [Upgrade::Refuse(401), Upgrade::Accept] {
        let origin = fake_library(upgrade, |mut socket| async move {
            close(&mut socket, crate::live::CLOSE_UNAUTHORIZED).await;
        })
        .await;
        let files = tempfile::tempdir().unwrap();
        let (_state, store, library) = clerk_with_card(&files, &origin).await;
        assert_eq!(
            one_session(&store, &library).await.unwrap(),
            super::SessionEnd::SignedOut
        );
        let link = load_clerk_link(&store, &library).await.unwrap().unwrap();
        assert_eq!(link.state, ClerkLinkState::SignedOut);
        // A signed-out link is not dialed again.
        assert_eq!(
            one_session(&store, &library).await.unwrap(),
            super::SessionEnd::Inactive
        );
    }
}

#[tokio::test]
async fn a_misbehaving_library_ends_the_session_with_an_error() {
    use bookclerk_node_proto::{ClerkSocketError, LibraryFrame};
    let _dek = crate::auth::tests::process_dek_lock().await;
    // Each script misbehaves once the clerk asks for the first page.
    let scripts: Vec<(&str, u8)> = vec![
        ("an unreadable frame", 0),
        ("a page for another cursor", 1),
        ("a cursor that does not move", 2),
        ("a binary frame", 3),
        ("a page nobody asked for", 4),
        ("an error nobody asked for", 5),
    ];
    for (what, which) in scripts {
        let origin = fake_library(Upgrade::Accept, move |mut socket| async move {
            use futures::SinkExt as _;
            assert_eq!(asked(&mut socket).await.as_deref(), Some(""));
            match which {
                0 => say_raw(&mut socket, "{\"v\":1,\"type\":\"nope\"}").await,
                1 => {
                    say(
                        &mut socket,
                        LibraryFrame::BooksPage {
                            after: cursor("p9"),
                            works: Vec::new(),
                            next: None,
                        },
                    )
                    .await;
                }
                2 => {
                    say(
                        &mut socket,
                        LibraryFrame::BooksPage {
                            after: cursor(""),
                            works: Vec::new(),
                            next: Some(cursor("")),
                        },
                    )
                    .await;
                }
                3 => {
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Binary(
                            vec![1, 2, 3].into(),
                        ))
                        .await
                        .unwrap();
                }
                5 => {
                    // Answer the page, then an error for nothing asked.
                    say(
                        &mut socket,
                        LibraryFrame::BooksPage {
                            after: cursor(""),
                            works: Vec::new(),
                            next: None,
                        },
                    )
                    .await;
                    say(
                        &mut socket,
                        LibraryFrame::Error {
                            code: ClerkSocketError::Unavailable,
                        },
                    )
                    .await;
                }
                _ => {
                    // Answer with an error, then send a page anyway.
                    say(
                        &mut socket,
                        LibraryFrame::Error {
                            code: ClerkSocketError::Unavailable,
                        },
                    )
                    .await;
                    say(
                        &mut socket,
                        LibraryFrame::BooksPage {
                            after: cursor(""),
                            works: Vec::new(),
                            next: None,
                        },
                    )
                    .await;
                }
            }
            // Hold the socket until the clerk hangs up.
            while asked(&mut socket).await.is_some() {}
        })
        .await;
        let files = tempfile::tempdir().unwrap();
        let (_state, store, library) = clerk_with_card(&files, &origin).await;
        assert!(
            one_session(&store, &library).await.is_err(),
            "{what} was accepted"
        );
    }
}

#[tokio::test]
async fn the_supervisor_starts_and_stops_readers_and_shutdown_ends_a_session() {
    use bookclerk_node_proto::LibraryFrame;
    let _dek = crate::auth::tests::process_dek_lock().await;
    let origin = fake_library(Upgrade::Accept, |mut socket| async move {
        while let Some(after) = asked(&mut socket).await {
            say(
                &mut socket,
                LibraryFrame::BooksPage {
                    after: cursor(&after),
                    works: vec![work("B0SUPER001")],
                    next: None,
                },
            )
            .await;
        }
    })
    .await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, library) = clerk_with_card(&files, &origin).await;

    // A session ends when it is told to stop.
    let (stop, mut shutdown) = watch::channel(false);
    let reader = ClerkReader {
        library: store.clone(),
        library_id: library.clone(),
    };
    let session = tokio::spawn(async move { reader.session(&mut shutdown).await });
    eventually("a read", 20, || async {
        titles(&store).await == vec!["Title B0SUPER001"]
    })
    .await;
    stop.send(true).unwrap();
    assert_eq!(session.await.unwrap().unwrap(), super::SessionEnd::Shutdown);

    // The supervisor starts a reader for the active card and stops it once
    // the card is signed out here.
    bookclerk_library::control_plane::clear_window_books(&store, &library)
        .await
        .unwrap();
    super::spawn_clerk_links(Arc::clone(&state));
    eventually("the supervisor's read", 20, || async {
        titles(&store).await == vec!["Title B0SUPER001"]
    })
    .await;
    bookclerk_library::control_plane::sign_out_clerk_link(&store, &library)
        .await
        .unwrap();
    super::notify_clerk_links_changed();
    eventually("the books to go", 20, || async {
        titles(&store).await.is_empty()
    })
    .await;
}

#[tokio::test]
async fn a_callback_without_consent_or_code_is_refused_and_old_sign_ins_are_dropped() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let library_files = tempfile::tempdir().unwrap();
    let (_library_state, _library_store, origin, _member) = library(&library_files).await;
    let clerk_files = tempfile::tempdir().unwrap();
    let (clerk_state, _clerk_store) = crate::node_api::tests::test_state(&clerk_files).await;
    let clerk = clerk_routes(&clerk_state, UserCaller::Operator);

    // Not a library, or a name that cannot sign in.
    let (status, _, bad) = call(
        &clerk,
        "POST",
        "/api/config/clerk/start",
        Some(json!({ "address": "not a host" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    let (status, _, bad) = call(
        &clerk,
        "POST",
        "/api/config/clerk/start",
        Some(json!({ "address": origin, "clerk_name": "x".repeat(65) })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");

    let start = || async {
        let (status, _, started) = call(
            &clerk,
            "POST",
            "/api/config/clerk/start",
            Some(json!({ "address": origin })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{started}");
        let url = url::Url::parse(started["authorize_url"].as_str().unwrap()).unwrap();
        url.query_pairs()
            .find(|(name, _)| name == "state")
            .map(|(_, state)| state.into_owned())
            .unwrap()
    };
    // The library's person said no.
    let state = start().await;
    let (status, _, denied) = call(
        &clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "state": state, "error": "access_denied" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    assert_eq!(denied["error"], "denied");
    // No code.
    let state = start().await;
    let (status, _, no_code) = call(
        &clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "state": state, "code": " " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{no_code}");
    // A code the library never issued.
    let state = start().await;
    let (status, _, refused) = call(
        &clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "state": state, "code": "never-issued" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "refused");

    // Past the limit of pending sign-ins, the oldest is dropped.
    let oldest = start().await;
    for _ in 0..super::api::MAX_PENDING_CLERKS {
        start().await;
    }
    let (status, _, expired) = call(
        &clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "state": oldest, "code": "c" })),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{expired}");
}

/// The listing an acquire on the clerk asks with for `product_id`.
fn listing(product_id: &str) -> bookclerk_acquire::LibraryCopyListing<'_> {
    bookclerk_acquire::LibraryCopyListing {
        store: "audible",
        plugin_key: PLUGIN,
        account_id: "riley-acct",
        marketplace: "us",
        product_id,
    }
}

/// An acquire's `copy.grant` goes over the card's socket between pages,
/// each answer goes to the frame it answers in the order asked (an
/// `error` is no copy, `copy.granted` is the copy), a copy from another
/// store is passed over, nothing is asked of a library whose reader is not
/// connected, and a copy of another edition ends the session.
#[tokio::test]
async fn a_grant_is_answered_in_order_and_a_copy_of_another_edition_ends_the_session() {
    use bookclerk_acquire::LibraryCopies as _;
    use bookclerk_node_proto::{ClerkFrame, ClerkSocketError, LibraryFrame};

    use super::copies::{reader_of, CardCopies};

    let _dek = crate::auth::tests::process_dek_lock().await;
    let origin = fake_library(Upgrade::Accept, |mut socket| async move {
        // The read on connect, answered once the first grant is asked, so
        // the library answers the page before the grant.
        assert!(matches!(
            heard(&mut socket).await,
            Some(ClerkFrame::Books { .. })
        ));
        let Some(ClerkFrame::CopyGrant { product_id, .. }) = heard(&mut socket).await else {
            panic!("an acquire asks for a copy");
        };
        assert_eq!(product_id, "B0GRANT001");
        say(
            &mut socket,
            LibraryFrame::BooksPage {
                after: cursor(""),
                works: vec![work("B0GRANT001")],
                next: None,
            },
        )
        .await;
        say(
            &mut socket,
            LibraryFrame::Error {
                code: ClerkSocketError::NotFound,
            },
        )
        .await;
        // Asked again: granted.
        assert!(matches!(
            heard(&mut socket).await,
            Some(ClerkFrame::CopyGrant { .. })
        ));
        say(
            &mut socket,
            LibraryFrame::CopyGranted(Box::new(granted_copy("B0GRANT001", "audible"))),
        )
        .await;
        // Asked a third time: a copy from another store.
        assert!(matches!(
            heard(&mut socket).await,
            Some(ClerkFrame::CopyGrant { .. })
        ));
        say(
            &mut socket,
            LibraryFrame::CopyGranted(Box::new(granted_copy("B0GRANT001", "libro"))),
        )
        .await;
        // Asked a fourth time: a copy of another edition.
        assert!(matches!(
            heard(&mut socket).await,
            Some(ClerkFrame::CopyGrant { .. })
        ));
        say(
            &mut socket,
            LibraryFrame::CopyGranted(Box::new(granted_copy("B0OTHER001", "audible"))),
        )
        .await;
        while heard(&mut socket).await.is_some() {}
    })
    .await;
    let files = tempfile::tempdir().unwrap();
    let store = clerk_with_card_of(&files, &origin, "fake-grants").await;
    let copies = CardCopies::new(store.clone());
    // No reader: the library is not asked.
    assert!(copies.copy_of(listing("B0GRANT001")).await.is_none());

    let (_stop, mut shutdown) = watch::channel(false);
    let session = {
        let store = store.clone();
        tokio::spawn(async move {
            ClerkReader {
                library: store,
                library_id: "fake-grants".into(),
            }
            .session(&mut shutdown)
            .await
        })
    };
    eventually("the reader to connect", 20, || async {
        reader_of("fake-grants").is_some()
    })
    .await;

    // Not found: no copy, and the page asked before it still lands.
    assert!(copies.copy_of(listing("B0GRANT001")).await.is_none());
    assert_eq!(titles(&store).await, vec!["Title B0GRANT001"]);

    let copy = copies
        .copy_of(listing("B0GRANT001"))
        .await
        .expect("the library grants its copy");
    assert_eq!(copy.source_store(), Some("audible"));
    let files_listed: Vec<(&str, bookclerk_library::FileRole, u64)> = copy
        .files()
        .iter()
        .map(|file| (file.name.as_str(), file.role, file.size))
        .collect();
    assert_eq!(
        files_listed,
        [("Title.m4b", bookclerk_library::FileRole::Audio, 10)]
    );
    // A file the grant does not list is refused before anything is asked.
    assert!(copy
        .fetch_file(1, &files.path().join("none"))
        .await
        .is_err());
    // The grant's token is kept out of logs while the copy is held, and
    // leaves the redaction registry with it.
    assert!(bookclerk_config::contains_registered_secret("dG9rZW4"));
    drop(copy);
    assert!(!bookclerk_config::contains_registered_secret("dG9rZW4"));

    // A copy from another store is offered when no library has one from
    // the title's own, with what the library says of it, so the acquire
    // can compare it with its own download.
    let other = copies
        .copy_of(listing("B0GRANT001"))
        .await
        .expect("a copy from another store is offered");
    assert_eq!(other.source_store(), Some("libro"));
    let probe = other.probe();
    assert_eq!(probe.content_type, "audio/mp4");
    assert_eq!(probe.bitrate_kbps, Some(64));
    assert!(!probe.lossless);
    assert_eq!(probe.source_store.as_deref(), Some("libro"));
    drop(other);
    assert!(reader_of("fake-grants").is_some());

    assert!(copies.copy_of(listing("B0GRANT001")).await.is_none());
    let ended = tokio::time::timeout(Duration::from_secs(20), session)
        .await
        .expect("the session ends")
        .unwrap();
    assert!(ended.is_err(), "a copy of another edition was accepted");
    // The reader is no longer listed once its socket ends.
    assert!(reader_of("fake-grants").is_none());
}

/// A grant that arrives after the clerk signed in again was granted to the
/// old card: it is not used, and the session dialed with that card ends.
#[tokio::test]
async fn a_grant_answered_after_a_new_sign_in_is_not_used() {
    use bookclerk_acquire::LibraryCopies as _;
    use bookclerk_node_proto::{ClerkFrame, LibraryFrame};

    use super::copies::{reader_of, CardCopies};

    let _dek = crate::auth::tests::process_dek_lock().await;
    let asked = Arc::new(tokio::sync::Notify::new());
    let answer = Arc::new(tokio::sync::Notify::new());
    let origin = {
        let (asked, answer) = (Arc::clone(&asked), Arc::clone(&answer));
        fake_library(Upgrade::Accept, move |mut socket| {
            let (asked, answer) = (Arc::clone(&asked), Arc::clone(&answer));
            async move {
                assert!(matches!(
                    heard(&mut socket).await,
                    Some(ClerkFrame::Books { .. })
                ));
                say(
                    &mut socket,
                    LibraryFrame::BooksPage {
                        after: cursor(""),
                        works: vec![],
                        next: None,
                    },
                )
                .await;
                assert!(matches!(
                    heard(&mut socket).await,
                    Some(ClerkFrame::CopyGrant { .. })
                ));
                asked.notify_one();
                // Held until the clerk has signed in again.
                answer.notified().await;
                say(
                    &mut socket,
                    LibraryFrame::CopyGranted(Box::new(granted_copy("B0LATE0001", "audible"))),
                )
                .await;
                while heard(&mut socket).await.is_some() {}
            }
        })
        .await
    };
    let files = tempfile::tempdir().unwrap();
    let store = clerk_with_card_of(&files, &origin, "fake-late-grant").await;

    let (_stop, mut shutdown) = watch::channel(false);
    let session = {
        let store = store.clone();
        tokio::spawn(async move {
            ClerkReader {
                library: store,
                library_id: "fake-late-grant".into(),
            }
            .session(&mut shutdown)
            .await
        })
    };
    eventually("the reader to connect", 20, || async {
        reader_of("fake-late-grant").is_some()
    })
    .await;

    let acquire = {
        let copies = CardCopies::new(store.clone());
        tokio::spawn(async move { copies.copy_of(listing("B0LATE0001")).await.is_some() })
    };
    asked.notified().await;
    // Signed in again while the grant is on its way.
    let link = load_clerk_link(&store, "fake-late-grant")
        .await
        .unwrap()
        .unwrap();
    bookclerk_library::control_plane::record_clerk_link(
        &store,
        &bookclerk_library::control_plane::NewClerkLink {
            library_id: link.library_id.clone(),
            origin: link.origin.clone(),
            spki_pin: link.spki_pin.clone(),
            library_name: link.library_name.clone(),
            remote_user_id: link.remote_user_id,
            display_name: link.display_name.clone(),
            clerk_name: link.clerk_name.clone(),
        },
        &bookclerk_library::control_plane::LibraryCard::new("d".repeat(43)).unwrap(),
    )
    .await
    .unwrap();
    answer.notify_one();

    let took = tokio::time::timeout(Duration::from_secs(20), acquire)
        .await
        .expect("the acquire is answered")
        .unwrap();
    assert!(!took, "a copy granted to the old card was used");
    assert!(!bookclerk_config::contains_registered_secret("dG9rZW4"));
    let ended = tokio::time::timeout(Duration::from_secs(20), session)
        .await
        .expect("the session ends")
        .unwrap();
    assert_eq!(ended.unwrap(), super::SessionEnd::Lost);
}

/// A clerk's own acquire takes the copy the library holds of its person's
/// book: the reader asks over the card, the files download with the
/// grant's token, and the taken copy is recorded as the library's.
#[tokio::test]
async fn a_card_takes_the_librarys_copy_of_its_persons_book() {
    use bookclerk_acquire::LibraryCopies as _;
    use bookclerk_library::control_plane::books_imported_from_link;

    use super::copies::{install_copies, reader_of, CardCopies};

    let _dek = crate::auth::tests::process_dek_lock().await;
    let library_files = tempfile::tempdir().unwrap();
    let library_out = tempfile::tempdir().unwrap();
    let (library_state, library_store, origin, member) = library(&library_files).await;
    crate::node_api::uploads::set_test_destination(library_files.path(), library_out.path());
    hold(&library_store, member, "riley-acct", "B0COPY0001").await;
    hold(&library_store, member, "riley-acct", "B0COPY0002").await;
    // The library acquired the first from Audible and probed it.
    const KEY: &str = "Ann Author/Title/Title.m4b";
    let audio: Vec<u8> = (0..70_000u32).map(|i| (i % 239) as u8).collect();
    std::fs::create_dir_all(library_out.path().join("Ann Author/Title")).unwrap();
    std::fs::write(library_out.path().join(KEY), &audio).unwrap();
    library_store
        .set_acquire_status(
            "B0COPY0001",
            "riley-acct",
            AcquireStatus::Acquired,
            Some(KEY),
            None,
        )
        .await
        .unwrap();
    library_store
        .set_acquire_status(
            "B0COPY0002",
            "riley-acct",
            AcquireStatus::NotAcquired,
            None,
            None,
        )
        .await
        .unwrap();
    let book = library_store
        .held_title("B0COPY0001", "riley-acct")
        .await
        .unwrap()
        .unwrap();
    let owner = library_store
        .file_owner_of_holding(&book.holding_id)
        .await
        .unwrap()
        .unwrap();
    assert!(library_store
        .record_edition_file("local", KEY, &owner, bookclerk_library::FileRole::Audio)
        .await
        .unwrap());
    assert!(library_store
        .set_edition_file_probe(
            "local",
            KEY,
            &owner,
            &bookclerk_library::FileProbe {
                content_type: "audio/mp4".into(),
                bitrate_kbps: Some(64),
                lossless: false,
                source_store: Some("audible".into()),
            },
        )
        .await
        .unwrap());

    let clerk_files = tempfile::tempdir().unwrap();
    let (clerk_state, clerk_store) = crate::node_api::tests::test_state(&clerk_files).await;
    let clerk = clerk_routes(&clerk_state, UserCaller::Operator);
    let library_id = sign_in(&clerk, &library_state, &origin, member).await;
    let (stop, shutdown) = watch::channel(false);
    let reader = tokio::spawn(
        ClerkReader {
            library: clerk_store.clone(),
            library_id: library_id.clone(),
        }
        .run(shutdown),
    );
    eventually("the reader to connect", 20, || async {
        reader_of(&library_id).is_some()
    })
    .await;

    // Asked as the clerk's acquires ask: its branch links (none here),
    // then its cards.
    let copies = install_copies(&clerk_store);
    let copy = copies
        .copy_of(listing("B0COPY0001"))
        .await
        .expect("the library holds the person's copy");
    assert_eq!(copy.source_store(), Some("audible"));
    assert_eq!(copy.files().len(), 1);
    assert_eq!(copy.files()[0].size, 70_000);
    let taken = tempfile::tempdir().unwrap();
    let dest = taken.path().join("000.m4b");
    copy.fetch_file(0, &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), audio);

    // The title is on a store account connected to the clerk.
    clerk_store
        .upsert_account(PLUGIN, "riley-acct", "us", None, true, PLUGIN)
        .await
        .unwrap();
    copy.taken().await.unwrap();
    assert_eq!(
        books_imported_from_link(&clerk_store, &library_id)
            .await
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [(
            PLUGIN.to_string(),
            "us".to_string(),
            "B0COPY0001".to_string(),
            "riley-acct".to_string()
        )]
    );

    // A book the library has not acquired has no copy, and the socket
    // keeps serving.
    assert!(copies.copy_of(listing("B0COPY0002")).await.is_none());
    assert!(copies.copy_of(listing("B0COPY0001")).await.is_some());

    // Signed in again (a new card): the session dialed with the old card
    // grants nothing and makes way for one with the new card.
    let link = load_clerk_link(&clerk_store, &library_id)
        .await
        .unwrap()
        .unwrap();
    bookclerk_library::control_plane::record_clerk_link(
        &clerk_store,
        &bookclerk_library::control_plane::NewClerkLink {
            library_id: library_id.clone(),
            origin: link.origin.clone(),
            spki_pin: link.spki_pin.clone(),
            library_name: link.library_name.clone(),
            remote_user_id: link.remote_user_id,
            display_name: link.display_name.clone(),
            clerk_name: link.clerk_name.clone(),
        },
        &bookclerk_library::control_plane::LibraryCard::new("d".repeat(43)).unwrap(),
    )
    .await
    .unwrap();
    assert!(CardCopies::new(clerk_store.clone())
        .copy_of(listing("B0COPY0001"))
        .await
        .is_none());

    let _ = stop.send(true);
    tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("the reader stops")
        .unwrap();
}

/// The library's member approves a branch join's authorize URL; answers
/// the callback's query pairs.
async fn branch_consent(
    library: &Arc<AppState>,
    member: i64,
    authorize_url: &str,
) -> std::collections::HashMap<String, String> {
    let url = url::Url::parse(authorize_url).unwrap();
    let mut body = serde_json::Map::new();
    for (name, value) in url.query_pairs() {
        body.insert(name.into_owned(), json!(value.into_owned()));
    }
    body.insert("approve".into(), json!(true));
    let app = crate::node_api::branch::tests::routes(
        library,
        UserCaller::User { user_id: member },
        LOOPBACK,
    );
    let (status, _, answer) = call(
        &app,
        "POST",
        "/api/auth/branch-consent",
        Some(Value::Object(body)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let redirect = url::Url::parse(answer["redirect"].as_str().unwrap()).unwrap();
    redirect.query_pairs().into_owned().collect()
}

/// A clerk becomes a branch of the library it holds a card for, and back:
/// the branch join replaces the card (ending it at the library), and a
/// card sign-in from the branch leaves the branch (telling the library).
#[tokio::test]
async fn a_card_becomes_a_branch_and_a_branch_becomes_a_card() {
    use bookclerk_library::control_plane::load_library_link;

    let _dek = crate::auth::tests::process_dek_lock().await;
    let library_files = tempfile::tempdir().unwrap();
    let (library_state, library_store, origin, member) = library(&library_files).await;
    let clerk_files = tempfile::tempdir().unwrap();
    let (clerk_state, clerk_store) = crate::node_api::tests::test_state(&clerk_files).await;
    crate::library_link::spawn_library_links(Arc::clone(&clerk_state));
    let routes_as = |caller: UserCaller| {
        clerk_routes(&clerk_state, caller).merge(
            axum::Router::new()
                .route(
                    "/api/config/join/start",
                    post(crate::library_link::join::post_join_start),
                )
                .route(
                    "/api/config/join/callback",
                    post(crate::library_link::join::post_join_callback),
                )
                .layer(axum::Extension(caller))
                .with_state(Arc::clone(&clerk_state)),
        )
    };
    let clerk = routes_as(UserCaller::Operator);
    let library_id = sign_in(&clerk, &library_state, &origin, member).await;
    let identity = library_store
        .ensure_local_portal_identity(member, None)
        .await
        .unwrap();
    let card_sessions = || async {
        library_store
            .list_portal_session_records_for_identity(identity.id)
            .await
            .unwrap()
            .len()
    };
    assert_eq!(card_sessions().await, 1);

    // Joining from a card the install does not hold, or as a shared
    // branch, is refused.
    let (status, _, unknown) = call(
        &clerk,
        "POST",
        "/api/config/join/start",
        Some(json!({ "from_card": "not-a-card" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");
    let (status, _, shared) = call(
        &clerk,
        "POST",
        "/api/config/join/start",
        Some(json!({ "from_card": library_id, "shared_branch": true })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{shared}");

    // The card becomes a branch: its address, pin and name carry over.
    let (status, _, started) = call(
        &clerk,
        "POST",
        "/api/config/join/start",
        Some(json!({ "from_card": library_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["branch_name"], "Laptop");
    assert_eq!(started["library"]["origin"], origin.as_str());
    let back = branch_consent(
        &library_state,
        member,
        started["authorize_url"].as_str().unwrap(),
    )
    .await;
    let (status, _, joined) = call(
        &clerk,
        "POST",
        "/api/config/join/callback",
        Some(json!({ "code": back["code"], "state": back["state"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["replaced_card"]["card_removed"], true);
    assert_eq!(joined["replaced_card"]["library_signed_out"], true);
    assert!(list_clerk_links(&clerk_store).await.unwrap().is_empty());
    assert_eq!(card_sessions().await, 0, "the library ended the card");
    let link = load_library_link(&clerk_store, &library_id)
        .await
        .unwrap()
        .unwrap();
    assert!(link.is_active());

    // Only the operator may give the branch up for a card, and only for
    // a library this install is a branch of.
    let owner = clerk_store
        .create_user(UserRole::Owner, Some("Pat"), None)
        .await
        .unwrap()
        .id;
    let (status, _, refused) = call(
        &routes_as(UserCaller::User { user_id: owner }),
        "POST",
        "/api/config/clerk/start",
        Some(json!({ "from_branch": library_id })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    let (status, _, unknown) = call(
        &clerk,
        "POST",
        "/api/config/clerk/start",
        Some(json!({ "from_branch": "not-a-branch" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");

    // A sign-in started for an enrollment the install has since replaced
    // with a new join does not give the new branch up.
    let (status, _, started) = call(
        &clerk,
        "POST",
        "/api/config/clerk/start",
        Some(json!({ "from_branch": library_id, "clerk_name": "Laptop" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    super::api::rejoin_pending_branches("an-earlier-node");
    let back = consent(
        &library_state,
        member,
        started["authorize_url"].as_str().unwrap(),
    )
    .await;
    let (status, _, stale) = call(
        &clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "code": back["code"], "state": back["state"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(stale["error"], "enrollment_changed");
    assert!(load_library_link(&clerk_store, &library_id)
        .await
        .unwrap()
        .unwrap()
        .is_active());

    // The branch becomes a card: the library hands the card over, then
    // the branch is left and the library told.
    let (status, _, started) = call(
        &clerk,
        "POST",
        "/api/config/clerk/start",
        Some(json!({ "from_branch": library_id, "clerk_name": "Laptop" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let back = consent(
        &library_state,
        member,
        started["authorize_url"].as_str().unwrap(),
    )
    .await;
    let (status, _, signed_in) = call(
        &clerk,
        "POST",
        "/api/config/clerk/callback",
        Some(json!({ "code": back["code"], "state": back["state"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{signed_in}");
    assert_eq!(signed_in["left_branch"]["owner_notified"], true);
    assert_eq!(signed_in["card"]["state"], "active");
    let link = load_library_link(&clerk_store, &library_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.state, "left");
    assert_eq!(card_sessions().await, 1);
    let credential = bookclerk_library::control_plane::load_node_credential(
        &library_store,
        &link.remote_node_id,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        credential.revoked_at.is_some(),
        "the library revoked the branch it was told left"
    );
}

/// A clerk asks its library with its card for a copy of a book, from the
/// library's branches or downloaded on the library's own accounts: the
/// library's own member routes decide, and the clerk passes on what they
/// said.
#[tokio::test]
async fn a_card_asks_its_library_for_a_copy() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let library_files = tempfile::tempdir().unwrap();
    let (library_state, library_store, origin, member) = library(&library_files).await;
    let clerk_files = tempfile::tempdir().unwrap();
    let (clerk_state, clerk_store) = crate::node_api::tests::test_state(&clerk_files).await;
    let clerk = clerk_routes(&clerk_state, UserCaller::Operator);
    let work = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b";

    // No card for that library yet, and malformed ids, stop here.
    let (status, _, refused) = call(
        &clerk,
        "POST",
        "/api/config/clerk/request",
        Some(json!({ "library_id": "lib-unknown", "work_id": work, "from": "branch" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    assert_eq!(refused["error"], "not_found");
    let library_id = sign_in(&clerk, &library_state, &origin, member).await;
    for (body, field) in [
        (
            json!({ "library_id": library_id, "work_id": "not a work", "from": "branch" }),
            "work_id",
        ),
        (
            json!({ "library_id": library_id, "work_id": work, "from": "branch", "node_id": "no spaces" }),
            "node_id",
        ),
    ] {
        let (status, _, refused) =
            call(&clerk, "POST", "/api/config/clerk/request", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["detail"], format!("invalid {field}"));
    }

    // No branch of the library lists the work: the library says so.
    let (status, _, missing) = call(
        &clerk,
        "POST",
        "/api/config/clerk/request",
        Some(json!({ "library_id": library_id, "work_id": work, "from": "branch" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert_eq!(missing["error"], "not_listed");

    // A book only a branch of the library holds: the library takes the
    // request, and the clerk's list offers it with the library's work id.
    let listed =
        crate::branch_requests::tests::work_only_a_branch_holds(&library_store, member).await;
    let (status, _, sent) = call(
        &clerk,
        "POST",
        "/api/config/clerk/request",
        Some(json!({ "library_id": library_id, "work_id": listed, "from": "branch" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["library_id"], library_id.as_str());
    let (pages, _) = bookclerk_library::control_plane::person_books_page(
        &library_store,
        member,
        &bookclerk_node_proto::WindowCursor::parse("zz-branch").unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(pages.len(), 1);
    let stamp = bookclerk_library::control_plane::window_read_stamp();
    bookclerk_library::control_plane::record_window_page(&clerk_store, &library_id, &stamp, &pages)
        .await
        .unwrap();
    let (status, _, view) = call(&clerk, "GET", "/api/config/join/window", None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let edition = &view["books"][0]["editions"][0];
    assert_eq!(edition["request"]["work_id"], listed.as_str(), "{view}");
    assert_eq!(edition["request"]["from"], "branch", "{view}");
    assert_eq!(edition["yours"], false);

    // A book on one of the library's own accounts with no copy yet: the
    // clerk's list offers to have the library download it, and the library
    // queues the download on that account.
    let held = crate::download_requests::tests::work_held_without_a_copy(&library_store).await;
    let (pages, _) = bookclerk_library::control_plane::person_books_page(
        &library_store,
        member,
        &bookclerk_node_proto::WindowCursor::start(),
    )
    .await
    .unwrap();
    let stamp = bookclerk_library::control_plane::window_read_stamp();
    bookclerk_library::control_plane::record_window_page(&clerk_store, &library_id, &stamp, &pages)
        .await
        .unwrap();
    let (status, _, view) = call(&clerk, "GET", "/api/config/join/window", None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let offered: Vec<&serde_json::Value> = view["books"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|book| book["editions"].as_array().unwrap())
        .filter(|edition| edition["request"]["from"] == "library")
        .collect();
    assert_eq!(offered.len(), 1, "{view}");
    assert_eq!(offered[0]["request"]["work_id"], held.as_str(), "{view}");
    let (status, _, refused) = call(
        &clerk,
        "POST",
        "/api/config/clerk/request",
        Some(json!({ "library_id": library_id, "work_id": held, "from": "library", "node_id": "branch-b" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, _, sent) = call(
        &clerk,
        "POST",
        "/api/config/clerk/request",
        Some(json!({ "library_id": library_id, "work_id": held, "from": "library" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let (status, _, missing) = call(
        &clerk,
        "POST",
        "/api/config/clerk/request",
        Some(json!({ "library_id": library_id, "work_id": work, "from": "library" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert_eq!(missing["error"], "not_listed");

    // The library's member controls turn requests off for the person.
    // Controls apply only with operator auth on; the card still signs in.
    *library_state.auth.write().await = Arc::new(crate::auth::OperatorAuthState::new(
        "op-token-requests".into(),
        12,
        true,
        5,
        30,
    ));
    bookclerk_library::control_plane::set_member_control_overrides(
        &library_store,
        member,
        bookclerk_library::control_plane::ControlOverrides {
            request_books: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (status, _, refused) = call(
        &clerk,
        "POST",
        "/api/config/clerk/request",
        Some(json!({ "library_id": library_id, "work_id": work, "from": "branch" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "not_allowed");

    // Only the operator or an Owner sends requests with the card.
    let as_member = clerk_routes(&clerk_state, UserCaller::User { user_id: 9_999 });
    let (status, _, _) = call(
        &as_member,
        "POST",
        "/api/config/clerk/request",
        Some(json!({ "library_id": library_id, "work_id": work, "from": "branch" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
