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
    list_clerk_links, load_clerk_link, record_cluster_link, window_view, ClerkLinkState,
    ClusterLinkKeys, LiveTopic, NewClusterLink,
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
            "/api/config/join/window",
            get(crate::cluster_link::join::get_join_window),
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
            uuid: None,
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
/// answers the library's cluster id.
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
    signed_in["card"]["cluster_id"]
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

    let cluster_id = sign_in(&clerk, &library_state, &origin, member).await;
    let (_, _, cards) = call(&clerk, "GET", "/api/config/clerk", None).await;
    assert_eq!(cards["cards"].as_array().unwrap().len(), 1, "{cards}");
    assert_eq!(cards["cards"][0]["clerk_name"], "Laptop");

    let (stop, shutdown) = watch::channel(false);
    let reader = tokio::spawn(
        ClerkReader {
            library: clerk_store.clone(),
            cluster_id: cluster_id.clone(),
        }
        .run(shutdown),
    );
    eventually("the first read", 20, || async {
        titles(&clerk_store).await == vec!["Title B0CLERK001"]
    })
    .await;
    let (status, _, window) = call(&clerk, "GET", "/api/config/join/window", None).await;
    assert_eq!(status, StatusCode::OK, "{window}");
    assert_eq!(window["cards"][0]["cluster_id"], cluster_id.as_str());
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
        load_clerk_link(&clerk_store, &cluster_id)
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
        Some(json!({ "cluster_id": cluster_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["library_signed_out"], false);
    assert!(list_clerk_links(&clerk_store).await.unwrap().is_empty());
    let (status, _, _) = call(
        &clerk,
        "POST",
        "/api/config/clerk/sign-out",
        Some(json!({ "cluster_id": cluster_id })),
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
    let cluster = bookclerk_library::control_plane::load_cluster_row(&library_store)
        .await
        .unwrap()
        .unwrap();
    record_cluster_link(
        &clerk_store,
        &NewClusterLink {
            owner_cluster_id: cluster.cluster_id.clone(),
            owner_origin: origin.clone(),
            remote_node_id: "node-1".into(),
            spki_pin: None,
        },
        &ClusterLinkKeys::generate(),
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
            Some(json!({ "cluster_id": "x" })),
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
    let cluster_id = sign_in(&clerk, &library_state, &origin, member).await;
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
        Some(json!({ "cluster_id": cluster_id })),
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
        WindowWork,
    };
    WindowWork {
        work: format!("w-{asin}"),
        identifiers: vec![LookupIdentifier {
            scheme: LookupScheme::Asin,
            value: asin.into(),
            marketplace: "us".into(),
        }],
        editions: vec![LookupEdition {
            store: "audible".into(),
            marketplace: "us".into(),
            product_id: asin.into(),
            availability: Availability::Owned,
            formats: vec![LookupFormat::Audio],
            metadata: CatalogMetadata {
                title: format!("Title {asin}"),
                ..CatalogMetadata::default()
            },
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
async fn asked(socket: &mut FakeSocket) -> Option<i64> {
    use futures::StreamExt as _;
    while let Some(Ok(message)) = socket.next().await {
        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
            let frame = bookclerk_node_proto::parse_clerk_frame(text.as_bytes())
                .expect("the clerk sends frames the library reads");
            let bookclerk_node_proto::ClerkFrame::Books { after } = frame;
            return Some(after);
        }
    }
    None
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
/// answers its state, store and the library's cluster id.
async fn clerk_with_card(
    files: &tempfile::TempDir,
    origin: &str,
) -> (Arc<AppState>, LibraryStore, String) {
    let (state, store) = crate::node_api::tests::test_state(files).await;
    bookclerk_library::control_plane::record_clerk_link(
        &store,
        &bookclerk_library::control_plane::NewClerkLink {
            library_cluster_id: "fake-library".into(),
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

/// Runs one session of a reader for `cluster_id` on `store`.
async fn one_session(store: &LibraryStore, cluster_id: &str) -> anyhow::Result<super::SessionEnd> {
    let (_stop, mut shutdown) = watch::channel(false);
    tokio::time::timeout(
        Duration::from_secs(20),
        ClerkReader {
            library: store.clone(),
            cluster_id: cluster_id.into(),
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
        assert_eq!(asked(&mut socket).await, Some(0));
        say(
            &mut socket,
            LibraryFrame::BooksPage {
                after: 0,
                works: vec![work("B0PAGE0001")],
                next: Some(5),
            },
        )
        .await;
        assert_eq!(asked(&mut socket).await, Some(5));
        say(
            &mut socket,
            LibraryFrame::BooksPage {
                after: 5,
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
    let (_state, store, cluster) = clerk_with_card(&files, &origin).await;
    assert_eq!(
        one_session(&store, &cluster).await.unwrap(),
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
        let (_state, store, cluster) = clerk_with_card(&files, &origin).await;
        assert_eq!(
            one_session(&store, &cluster).await.unwrap(),
            super::SessionEnd::SignedOut
        );
        let link = load_clerk_link(&store, &cluster).await.unwrap().unwrap();
        assert_eq!(link.state, ClerkLinkState::SignedOut);
        // A signed-out link is not dialed again.
        assert_eq!(
            one_session(&store, &cluster).await.unwrap(),
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
    ];
    for (what, which) in scripts {
        let origin = fake_library(Upgrade::Accept, move |mut socket| async move {
            use futures::SinkExt as _;
            assert_eq!(asked(&mut socket).await, Some(0));
            match which {
                0 => say_raw(&mut socket, "{\"v\":1,\"type\":\"nope\"}").await,
                1 => {
                    say(
                        &mut socket,
                        LibraryFrame::BooksPage {
                            after: 9,
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
                            after: 0,
                            works: Vec::new(),
                            next: Some(0),
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
                            after: 0,
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
        let (_state, store, cluster) = clerk_with_card(&files, &origin).await;
        assert!(
            one_session(&store, &cluster).await.is_err(),
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
                    after,
                    works: vec![work("B0SUPER001")],
                    next: None,
                },
            )
            .await;
        }
    })
    .await;
    let files = tempfile::tempdir().unwrap();
    let (state, store, cluster) = clerk_with_card(&files, &origin).await;

    // A session ends when it is told to stop.
    let (stop, mut shutdown) = watch::channel(false);
    let reader = ClerkReader {
        library: store.clone(),
        cluster_id: cluster.clone(),
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
    bookclerk_library::control_plane::clear_window_books(&store, &cluster)
        .await
        .unwrap();
    super::spawn_clerk_links(Arc::clone(&state));
    eventually("the supervisor's read", 20, || async {
        titles(&store).await == vec!["Title B0SUPER001"]
    })
    .await;
    bookclerk_library::control_plane::sign_out_clerk_link(&store, &cluster)
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
