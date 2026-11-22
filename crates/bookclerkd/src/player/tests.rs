//! The player API through the daemon's router, as an app signed in with a
//! player card.

use std::sync::Arc;

use axum::http::{header, Method, StatusCode};
use base64::Engine as _;
use bookclerk_library::{AcquireStatus, FileRole, LibraryStore, NewBook};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Digest as _;
use tower::ServiceExt;

/// The operator token of these tests' daemon.
const TOKEN: &str = "op-token-player";

/// Destination key of the item's audio.
const KEY: &str = "Ann Author/First/First.m4b";

/// Riley's sign-in name.
const RILEY: &str = "riley";

/// A daemon with one output destination, one acquired audio edition and
/// two members.
struct World {
    /// The daemon's router.
    app: axum::Router,
    /// Its library.
    store: LibraryStore,
    /// Riley's browser session cookie.
    riley_cookie: String,
    /// The acquired edition (the item).
    item: String,
    /// The output destination's folder; removed when the test ends.
    _out: tempfile::TempDir,
    /// The daemon's files folder; removed when the test ends.
    _files: tempfile::TempDir,
}

/// 100 kB of audio-shaped bytes.
fn audio() -> Vec<u8> {
    (0..100_000u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect()
}

/// Builds the daemon and acquires "First" by Ann Author.
async fn world() -> World {
    let files = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let (state, store) = crate::node_api::tests::test_state(&files).await;
    *state.auth.write().await = Arc::new(crate::auth::OperatorAuthState::new(
        TOKEN.into(),
        1,
        true,
        5,
        30,
    ));
    crate::node_api::uploads::set_test_destination(files.path(), out.path());
    let identity = store
        .upsert_portal_identity("test", "riley-ext", Some("Riley"))
        .await
        .unwrap();
    let riley = identity.user_id.expect("a bridged user");
    store.set_user_login_name(riley, Some(RILEY)).await.unwrap();
    store
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .unwrap();
    let mut book = NewBook::minimal("B0FIRST", "acct", "us", "The First");
    book.authors = Some("Ann Author".into());
    book.narrators = Some("Nia Narrator".into());
    book.length_minutes = Some(600);
    let saved = store.upsert_book(&book).await.unwrap();
    let path = out.path().join(KEY);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, audio()).unwrap();
    store
        .set_acquire_status("B0FIRST", "acct", AcquireStatus::Acquired, Some(KEY), None)
        .await
        .unwrap();
    let owner = store
        .file_owner_of_holding(&saved.holding_id)
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .record_edition_file("local", KEY, &owner, FileRole::Audio)
        .await
        .unwrap());
    let work = store
        .work_of_holding(&saved.holding_id)
        .await
        .unwrap()
        .unwrap();
    store.refresh_library_works(&[work]).await.unwrap();
    let item = store
        .edition_of_holding(&saved.holding_id)
        .await
        .unwrap()
        .unwrap();
    World {
        app: crate::api::router(Arc::clone(&state), None),
        riley_cookie: crate::auth::tests::portal_cookie_for(&store, "test", "riley-ext").await,
        store,
        item,
        _out: out,
        _files: files,
    }
}

/// What a request answered.
struct Answer {
    /// The status.
    status: StatusCode,
    /// The response headers.
    headers: axum::http::HeaderMap,
    /// The body's bytes.
    bytes: Vec<u8>,
}

impl Answer {
    /// The body as JSON, `null` when it is not JSON.
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

/// How a request signs in.
enum As<'a> {
    /// Nobody.
    Nobody,
    /// A card's access token as `Authorization: Bearer`.
    Card(&'a str),
    /// A browser session cookie, from the app's own origin.
    Cookie(&'a str),
}

/// Sends one request through the router.
async fn send(
    world: &World,
    method: Method,
    uri: &str,
    who: As<'_>,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Answer {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "library.test");
    request = match who {
        As::Nobody => request,
        As::Card(token) => request.header(header::AUTHORIZATION, format!("Bearer {token}")),
        As::Cookie(cookie) => request
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, "http://library.test"),
    };
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(body.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = world
        .app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    Answer {
        status: response.status(),
        headers: response.headers().clone(),
        bytes: response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    }
}

/// Makes Riley a player password in Account settings; answers it.
async fn player_password(world: &World) -> String {
    let made = send(
        world,
        Method::POST,
        "/api/auth/player-passwords",
        As::Cookie(&world.riley_cookie),
        Some(json!({ "label": "Phone" })),
        &[],
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.json());
    made.json()["secret"].as_str().unwrap().to_owned()
}

/// Signs Riley in with a fresh player password; answers the access and
/// refresh tokens.
async fn sign_in(world: &World) -> (String, String) {
    let password = player_password(world).await;
    let login = send(
        world,
        Method::POST,
        "/player/login",
        As::Nobody,
        Some(json!({ "username": RILEY, "password": password })),
        &[("x-return-tokens", "true")],
    )
    .await;
    assert_eq!(login.status, StatusCode::OK);
    let user = &login.json()["user"];
    (
        user["accessToken"].as_str().unwrap().to_owned(),
        user["refreshToken"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn status_and_ping_need_no_card() {
    let world = world().await;
    let status = send(&world, Method::GET, "/player/status", As::Nobody, None, &[]).await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.json()["app"], "audiobookshelf");
    assert_eq!(status.json()["authMethods"], json!(["local", "openid"]));
    let ping = send(&world, Method::GET, "/player/ping", As::Nobody, None, &[]).await;
    assert_eq!(ping.json(), json!({ "success": true }));
}

#[tokio::test]
async fn only_a_player_password_signs_in() {
    let world = world().await;
    let password = player_password(&world).await;
    for (case, (username, password)) in [
        ("someone-else", password.as_str()),
        (RILEY, "not-a-player-password"),
        (RILEY, "23456-Z2ZZB-CDEFG-HJKLM"),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = send(
            &world,
            Method::POST,
            "/player/login",
            As::Nobody,
            Some(json!({ "username": username, "password": password })),
            &[],
        )
        .await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "case {case}");
    }
    let login = send(
        &world,
        Method::POST,
        "/player/login",
        As::Nobody,
        Some(json!({ "username": "RILEY", "password": password.to_lowercase() })),
        &[],
    )
    .await;
    assert_eq!(login.status, StatusCode::OK);
    assert_eq!(login.headers[header::CACHE_CONTROL], "no-store");
    let token = login.json()["user"]["token"].as_str().unwrap().to_owned();
    let me = send(
        &world,
        Method::GET,
        "/player/api/me",
        As::Card(&token),
        None,
        &[],
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["username"], RILEY);
}

#[tokio::test]
async fn carded_routes_refuse_sessions_and_missing_cards() {
    let world = world().await;
    for who in [As::Nobody, As::Cookie(&world.riley_cookie), As::Card(TOKEN)] {
        let refused = send(&world, Method::GET, "/player/api/libraries", who, None, &[]).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    }
    let (access, _) = sign_in(&world).await;
    // A card is not a session anywhere else.
    let elsewhere = send(
        &world,
        Method::GET,
        "/api/library/works",
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(elsewhere.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn refresh_rotates_and_logout_ends_the_card() {
    let world = world().await;
    let (access, refresh) = sign_in(&world).await;
    let renewed = send(
        &world,
        Method::POST,
        "/player/auth/refresh",
        As::Nobody,
        None,
        &[("x-refresh-token", refresh.as_str())],
    )
    .await;
    assert_eq!(renewed.status, StatusCode::OK);
    let user = renewed.json()["user"].clone();
    let new_access = user["accessToken"].as_str().unwrap().to_owned();
    let new_refresh = user["refreshToken"].as_str().unwrap().to_owned();
    assert_ne!(new_access, access);
    assert_ne!(new_refresh, refresh);
    let me = send(
        &world,
        Method::GET,
        "/player/api/me",
        As::Card(&new_access),
        None,
        &[],
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);
    let out = send(
        &world,
        Method::POST,
        "/player/logout",
        As::Nobody,
        None,
        &[("x-refresh-token", new_refresh.as_str())],
    )
    .await;
    assert_eq!(out.status, StatusCode::OK);
    let me = send(
        &world,
        Method::GET,
        "/player/api/me",
        As::Card(&new_access),
        None,
        &[],
    )
    .await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn items_list_and_expand_with_tracks() {
    let world = world().await;
    let (access, _) = sign_in(&world).await;
    let card = As::Card(&access);
    let libraries = send(
        &world,
        Method::GET,
        "/player/api/libraries",
        card,
        None,
        &[],
    )
    .await;
    let library = libraries.json()["libraries"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let page = send(
        &world,
        Method::GET,
        &format!("/player/api/libraries/{library}/items?limit=10&sort=media.metadata.title"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{:?}", page.json());
    assert_eq!(page.json()["total"], 1);
    let first = &page.json()["results"][0];
    assert_eq!(first["id"], world.item.as_str());
    assert_eq!(first["media"]["metadata"]["title"], "The First");
    assert_eq!(first["media"]["metadata"]["authorName"], "Ann Author");
    // The author id is a filter value.
    let author = first["media"]["metadata"]["authors"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let filter = base64::engine::general_purpose::STANDARD.encode(&author);
    let filtered = send(
        &world,
        Method::GET,
        &format!("/player/api/libraries/{library}/items?filter=authors.{filter}"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(filtered.json()["total"], 1);
    let none = send(
        &world,
        Method::GET,
        &format!("/player/api/libraries/{library}/items?filter=progress.bm90LXN0YXJ0ZWQ"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(none.json()["total"], 0);
    let item = send(
        &world,
        Method::GET,
        &format!("/player/api/items/{}?expanded=1", world.item),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(item.status, StatusCode::OK);
    let tracks = item.json()["media"]["tracks"].clone();
    assert_eq!(tracks.as_array().map(Vec::len), Some(1));
    assert_eq!(
        tracks[0]["contentUrl"],
        format!("/api/items/{}/file/0", world.item)
    );
    assert_eq!(tracks[0]["duration"], 36000.0);
    let missing = send(
        &world,
        Method::GET,
        "/player/api/items/nope",
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let wrong_library = send(
        &world,
        Method::GET,
        "/player/api/libraries/other/items",
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(wrong_library.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audio_streams_with_a_token_in_the_url() {
    let world = world().await;
    let (access, _) = sign_in(&world).await;
    let uri = format!("/player/api/items/{}/file/0?token={access}", world.item);
    let ranged = send(
        &world,
        Method::GET,
        &uri,
        As::Nobody,
        None,
        &[("range", "bytes=10-19")],
    )
    .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.bytes, audio()[10..20]);
    assert_eq!(ranged.headers[header::CONTENT_TYPE], "audio/mp4");
    let past = send(
        &world,
        Method::GET,
        &format!("/player/api/items/{}/file/1?token={access}", world.item),
        As::Nobody,
        None,
        &[],
    )
    .await;
    assert_eq!(past.status, StatusCode::NOT_FOUND);
    let download = send(
        &world,
        Method::GET,
        &format!("/player/api/items/{}/file/0/download", world.item),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(download.status, StatusCode::OK);
    assert!(download.headers[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .starts_with("attachment"));
    let wrong = send(
        &world,
        Method::GET,
        &format!(
            "/player/api/items/{}/file/0?token={}",
            world.item,
            "a".repeat(43)
        ),
        As::Nobody,
        None,
        &[],
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn play_sync_close_and_progress() {
    let world = world().await;
    let (access, _) = sign_in(&world).await;
    let play = send(
        &world,
        Method::POST,
        &format!("/player/api/items/{}/play", world.item),
        As::Card(&access),
        Some(json!({ "deviceInfo": { "clientName": "Absorb" }, "forceDirectPlay": true })),
        &[],
    )
    .await;
    assert_eq!(play.status, StatusCode::OK, "{:?}", play.json());
    let session = play.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(play.json()["audioTracks"].as_array().map(Vec::len), Some(1));
    let sync = send(
        &world,
        Method::POST,
        &format!("/player/api/session/{session}/sync"),
        As::Card(&access),
        Some(json!({ "currentTime": 120.5, "timeListened": 10, "duration": 36000 })),
        &[],
    )
    .await;
    assert_eq!(sync.status, StatusCode::OK);
    let progress = send(
        &world,
        Method::GET,
        &format!("/player/api/me/progress/{}", world.item),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(progress.json()["currentTime"], 120.5);
    let close = send(
        &world,
        Method::POST,
        &format!("/player/api/session/{session}/close"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(close.status, StatusCode::OK);
    let finished = send(
        &world,
        Method::PATCH,
        &format!("/player/api/me/progress/{}", world.item),
        As::Card(&access),
        Some(json!({ "isFinished": true })),
        &[],
    )
    .await;
    assert_eq!(finished.status, StatusCode::OK);
    let progress = send(
        &world,
        Method::GET,
        &format!("/player/api/me/progress/{}", world.item),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(progress.json()["isFinished"], true);
    let sessions = send(
        &world,
        Method::GET,
        "/player/api/me/listening-sessions?itemsPerPage=5",
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(sessions.json()["total"], 1);
    assert_eq!(sessions.json()["sessions"][0]["displayTitle"], "The First");
    let other = send(
        &world,
        Method::POST,
        "/player/api/session/not-mine/sync",
        As::Card(&access),
        Some(json!({ "currentTime": 1, "timeListened": 1 })),
        &[],
    )
    .await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);
    // The store kept the same progress the web app reads.
    let user = world
        .store
        .get_user_by_login_name(RILEY)
        .await
        .unwrap()
        .unwrap();
    let stored = world
        .store
        .playback_progress(user.id, &world.item)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.is_finished);
}

/// A patron's apps learn only of the progress and bookmarks of books
/// their scope shows them: the signed-in user lists none of a hidden
/// book's, and its progress route answers as for an unknown item.
#[tokio::test]
async fn progress_of_a_book_outside_the_scope_is_not_listed() {
    use bookclerk_library::control_plane::{
        set_patron_scope, PatronScope, RuleEffect, ScopeField, ScopeRule,
    };
    let world = world().await;
    let (access, _) = sign_in(&world).await;
    let patched = send(
        &world,
        Method::PATCH,
        &format!("/player/api/me/progress/{}", world.item),
        As::Card(&access),
        Some(json!({ "currentTime": 42, "duration": 36000 })),
        &[],
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK);
    let marked = send(
        &world,
        Method::POST,
        &format!("/player/api/me/item/{}/bookmark", world.item),
        As::Card(&access),
        Some(json!({ "title": "Here", "time": 42 })),
        &[],
    )
    .await;
    assert_eq!(marked.status, StatusCode::OK);
    let me = get(&world, &access, "/player/api/me").await;
    assert_eq!(me.json()["mediaProgress"].as_array().map(Vec::len), Some(1));
    assert_eq!(me.json()["bookmarks"].as_array().map(Vec::len), Some(1));

    let riley = world
        .store
        .upsert_portal_identity("test", "riley-ext", Some("Riley"))
        .await
        .unwrap()
        .user_id
        .unwrap();
    set_patron_scope(
        &world.store,
        riley,
        Some(PatronScope {
            rules: vec![ScopeRule {
                field: ScopeField::Author,
                value: "Ann Author".into(),
                effect: RuleEffect::Exclude,
            }],
            explicit: Default::default(),
        }),
    )
    .await
    .unwrap();
    let me = get(&world, &access, "/player/api/me").await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["mediaProgress"], json!([]));
    assert_eq!(me.json()["bookmarks"], json!([]));
    let progress = get(
        &world,
        &access,
        &format!("/player/api/me/progress/{}", world.item),
    )
    .await;
    assert_eq!(progress.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bookmarks_are_found_by_time() {
    let world = world().await;
    let (access, _) = sign_in(&world).await;
    let uri = format!("/player/api/me/item/{}/bookmark", world.item);
    let added = send(
        &world,
        Method::POST,
        &uri,
        As::Card(&access),
        Some(json!({ "time": 61.25, "title": "Chapter two" })),
        &[],
    )
    .await;
    assert_eq!(added.status, StatusCode::OK);
    assert_eq!(added.json()["time"], 61.25);
    let renamed = send(
        &world,
        Method::PATCH,
        &uri,
        As::Card(&access),
        Some(json!({ "time": 61.25, "title": "Part two" })),
        &[],
    )
    .await;
    assert_eq!(renamed.json()["title"], "Part two");
    let gone = send(
        &world,
        Method::DELETE,
        &format!("{uri}/61.25"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(gone.status, StatusCode::OK);
    let again = send(
        &world,
        Method::DELETE,
        &format!("{uri}/61.25"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_browser_sign_in_returns_to_the_app_with_a_code() {
    let world = world().await;
    let verifier = "v".repeat(64);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let start = send(
        &world,
        Method::GET,
        &format!(
            "/player/auth/openid?response_type=code&client_id=Absorb&redirect_uri=absorb%3A%2F%2Foauth&code_challenge={challenge}&code_challenge_method=S256&state=xyz"
        ),
        As::Nobody,
        None,
        &[],
    )
    .await;
    assert_eq!(start.status, StatusCode::FOUND);
    let location = start.headers[header::LOCATION].to_str().unwrap().to_owned();
    assert!(location.contains("/apps/approve?"), "{location}");
    let consent_redirect = url::Url::parse(&location)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "redirect_uri")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    let refused = send(
        &world,
        Method::GET,
        &format!("/player/auth/openid?response_type=code&redirect_uri=evil%3A%2F%2Fx&code_challenge={challenge}&code_challenge_method=S256"),
        As::Nobody,
        None,
        &[],
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.bytes, b"Invalid redirect_uri");
    let consent = send(
        &world,
        Method::POST,
        "/api/auth/player-consent",
        As::Cookie(&world.riley_cookie),
        Some(json!({
            "redirect_uri": consent_redirect,
            "code_challenge": challenge,
            "state": "xyz",
            "approve": true,
        })),
        &[],
    )
    .await;
    assert_eq!(consent.status, StatusCode::OK, "{:?}", consent.json());
    let redirect = consent.json()["redirect"].as_str().unwrap().to_owned();
    let back = send(&world, Method::GET, &redirect, As::Nobody, None, &[]).await;
    assert_eq!(back.status, StatusCode::FOUND);
    let app = url::Url::parse(back.headers[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(app.scheme(), "absorb");
    let code = app
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    let wrong = send(
        &world,
        Method::GET,
        &format!(
            "/player/auth/openid/callback?code={code}&code_verifier={}",
            "w".repeat(64)
        ),
        As::Nobody,
        None,
        &[],
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    // A code is spent by its first exchange, right or wrong.
    let spent = send(
        &world,
        Method::GET,
        &format!("/player/auth/openid/callback?code={code}&code_verifier={verifier}"),
        As::Nobody,
        None,
        &[],
    )
    .await;
    assert_eq!(spent.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn shelves_search_and_series_answer_in_shape() {
    let world = world().await;
    let (access, _) = sign_in(&world).await;
    let library = bookclerk_player_api::ids::WHOLE_LIBRARY_ID;
    let shelves = send(
        &world,
        Method::GET,
        &format!("/player/api/libraries/{library}/personalized"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(shelves.status, StatusCode::OK);
    assert_eq!(shelves.json()[0]["id"], "recently-added");
    let authors = send(
        &world,
        Method::GET,
        &format!("/player/api/libraries/{library}/authors"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    let author = authors.json()["authors"][0].clone();
    assert_eq!(author["name"], "Ann Author");
    let one = send(
        &world,
        Method::GET,
        &format!(
            "/player/api/authors/{}?include=items",
            author["id"].as_str().unwrap()
        ),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(one.json()["libraryItems"].as_array().map(Vec::len), Some(1));
    for empty in ["collections", "playlists"] {
        let answer = send(
            &world,
            Method::GET,
            &format!("/player/api/libraries/{library}/{empty}"),
            As::Card(&access),
            None,
            &[],
        )
        .await;
        assert_eq!(answer.json()["total"], 0, "{empty}");
    }
    let filterdata = send(
        &world,
        Method::GET,
        &format!("/player/api/libraries/{library}?include=filterdata"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(filterdata.json()["filterdata"]["bookCount"], 1);
    assert_eq!(
        filterdata.json()["filterdata"]["narrators"],
        json!(["Nia Narrator"])
    );
    let stats = send(
        &world,
        Method::GET,
        &format!("/player/api/libraries/{library}/stats"),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(stats.json()["totalItems"], 1);
    assert_eq!(stats.json()["totalDuration"], 36000.0);
}

#[tokio::test]
async fn the_socket_refuses_long_polling_and_old_revisions() {
    let world = world().await;
    for path in ["/player/socket.io/", "/player/socket.io"] {
        for query in ["EIO=4&transport=polling", "EIO=3&transport=websocket"] {
            let uri = format!("{path}?{query}");
            let answer = send(&world, Method::GET, &uri, As::Nobody, None, &[]).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{uri}");
        }
    }
}

/// A GET with the card; answers status and JSON.
async fn get(world: &World, access: &str, uri: &str) -> Answer {
    send(world, Method::GET, uri, As::Card(access), None, &[]).await
}

#[tokio::test]
async fn every_browse_route_answers_in_shape() {
    let world = world().await;
    let (access, _) = sign_in(&world).await;
    let library = bookclerk_player_api::ids::WHOLE_LIBRARY_ID;
    let libraries = get(&world, &access, "/player/api/libraries").await;
    assert_eq!(libraries.json()["libraries"][0]["id"], library);
    let plain = get(&world, &access, &format!("/player/api/libraries/{library}")).await;
    assert_eq!(plain.json()["id"], library);
    for missing in [
        "/player/api/libraries/other".to_owned(),
        "/player/api/libraries/other/items".to_owned(),
        "/player/api/items/not-an-item".to_owned(),
        "/player/api/series/c2VyaWVz".to_owned(),
        "/player/api/authors/bm9ib2R5".to_owned(),
    ] {
        let answer = get(&world, &access, &missing).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{missing}");
    }
    let filterdata = get(
        &world,
        &access,
        &format!("/player/api/libraries/{library}/filterdata"),
    )
    .await;
    assert_eq!(filterdata.json()["authorCount"], 1);
    let narrators = get(
        &world,
        &access,
        &format!("/player/api/libraries/{library}/narrators"),
    )
    .await;
    assert_eq!(narrators.json()["narrators"][0]["name"], "Nia Narrator");
    let series = get(
        &world,
        &access,
        &format!("/player/api/libraries/{library}/series?limit=10&page=0"),
    )
    .await;
    assert_eq!(series.status, StatusCode::OK);
    assert_eq!(series.json()["total"], 0);
    for query in [
        "sort=media.metadata.title&desc=1&limit=5&page=0",
        "sort=addedAt&limit=5",
        "filter=progress.aW4tcHJvZ3Jlc3M&limit=5",
        "filter=progress.ZmluaXNoZWQ&limit=5",
        "filter=narrators.TmlhIE5hcnJhdG9y&limit=5",
    ] {
        let items = get(
            &world,
            &access,
            &format!("/player/api/libraries/{library}/items?{query}"),
        )
        .await;
        assert_eq!(items.status, StatusCode::OK, "{query}");
    }
    let search = get(
        &world,
        &access,
        &format!("/player/api/libraries/{library}/search?q=First&limit=5"),
    )
    .await;
    // Search needs the library's search index, which this daemon may not
    // have open; it answers 503 then, as documented.
    assert!(
        matches!(
            search.status,
            StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
        ),
        "{:?}",
        search.status
    );
    let bad = get(
        &world,
        &access,
        &format!("/player/api/libraries/{library}/items?limit=99999"),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let batch = send(
        &world,
        Method::POST,
        "/player/api/items/batch/get",
        As::Card(&access),
        Some(json!({ "libraryItemIds": [world.item, "unknown"] })),
        &[],
    )
    .await;
    assert_eq!(
        batch.json()["libraryItems"].as_array().map(Vec::len),
        Some(1)
    );
    let malformed = send(
        &world,
        Method::POST,
        "/player/api/items/batch/get",
        As::Card(&access),
        Some(json!({ "libraryItemIds": "x" })),
        &[],
    )
    .await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    let me = get(&world, &access, "/player/api/me").await;
    assert_eq!(me.json()["username"], RILEY);
    let authorize = send(
        &world,
        Method::POST,
        "/player/api/authorize",
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(authorize.json()["user"]["username"], RILEY);
}

#[tokio::test]
async fn offline_sessions_batches_and_downloads() {
    let world = world().await;
    let (access, refresh) = sign_in(&world).await;
    let local = send(
        &world,
        Method::POST,
        "/player/api/session/local",
        As::Card(&access),
        Some(json!({
            "id": "offline-1",
            "libraryItemId": world.item,
            "currentTime": 300,
            "timeListening": 60,
            "duration": 36000,
            "updatedAt": 1_763_600_000_000_i64,
        })),
        &[],
    )
    .await;
    assert_eq!(local.status, StatusCode::OK, "{:?}", local.json());
    let all = send(
        &world,
        Method::POST,
        "/player/api/session/local-all",
        As::Card(&access),
        Some(json!({ "sessions": [
            { "id": "offline-2", "libraryItemId": world.item, "currentTime": 400 },
            { "id": "offline-3", "libraryItemId": "../x", "currentTime": 1 },
        ] })),
        &[],
    )
    .await;
    assert_eq!(all.status, StatusCode::OK, "{:?}", all.json());
    let results = all.json()["results"].clone();
    assert_eq!(results[0]["success"], true);
    assert_eq!(results[1]["success"], false);
    let play = send(
        &world,
        Method::POST,
        &format!("/player/api/items/{}/play", world.item),
        As::Card(&access),
        Some(json!({ "mediaPlayer": "exoplayer" })),
        &[],
    )
    .await;
    assert_eq!(play.status, StatusCode::OK);
    let sessions = get(
        &world,
        &access,
        "/player/api/me/listening-sessions?itemsPerPage=10&page=0",
    )
    .await;
    assert!(sessions.json()["total"].as_u64().unwrap_or(0) >= 1);
    let stats = get(&world, &access, "/player/api/me/listening-stats").await;
    assert_eq!(stats.status, StatusCode::OK);
    let in_progress = get(&world, &access, "/player/api/me/items-in-progress").await;
    assert_eq!(
        in_progress.json()["libraryItems"].as_array().map(Vec::len),
        Some(1)
    );
    let batch = send(
        &world,
        Method::PATCH,
        "/player/api/me/progress/batch/update",
        As::Card(&access),
        Some(json!([{ "libraryItemId": world.item, "currentTime": 500, "duration": 36000 }])),
        &[],
    )
    .await;
    assert_eq!(batch.status, StatusCode::OK);
    let progress = get(
        &world,
        &access,
        &format!("/player/api/me/progress/{}", world.item),
    )
    .await;
    assert_eq!(progress.json()["currentTime"], 500.0);
    let cleared = send(
        &world,
        Method::DELETE,
        &format!("/player/api/me/progress/{}", world.item),
        As::Card(&access),
        None,
        &[],
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK);
    let gone = get(
        &world,
        &access,
        &format!("/player/api/me/progress/{}", world.item),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    let download = format!("/player/api/items/{}/file/0/download", world.item);
    let saved = get(&world, &access, &download).await;
    assert_eq!(saved.status, StatusCode::OK);
    assert!(saved.headers[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .starts_with("attachment"));
    let riley = world
        .store
        .get_user_by_login_name(RILEY)
        .await
        .unwrap()
        .unwrap();
    bookclerk_library::control_plane::set_member_control_overrides(
        &world.store,
        riley.id,
        bookclerk_library::control_plane::ControlOverrides {
            download: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let refused = get(&world, &access, &download).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let wrong = send(
        &world,
        Method::POST,
        "/player/auth/refresh",
        As::Nobody,
        None,
        &[("x-refresh-token", "not-a-token")],
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let missing = send(
        &world,
        Method::POST,
        "/player/auth/refresh",
        As::Nobody,
        None,
        &[],
    )
    .await;
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
    let rotated = send(
        &world,
        Method::POST,
        "/player/auth/refresh",
        As::Nobody,
        None,
        &[("x-refresh-token", refresh.as_str())],
    )
    .await;
    assert_eq!(rotated.status, StatusCode::OK);
    assert_eq!(rotated.json()["user"]["permissions"]["download"], false);
}

#[tokio::test]
async fn the_socket_signs_in_and_sends_progress() {
    use futures::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message;

    let world = world().await;
    let (access, _) = sign_in(&world).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = world.app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let url = format!(
        "ws://127.0.0.1:{}/player/socket.io/?EIO=4&transport=websocket",
        addr.port()
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert!(recv(&mut socket).await.starts_with("0{"));
    socket
        .send(Message::Text("40/admin,".into()))
        .await
        .unwrap();
    let text = recv(&mut socket).await;
    assert!(text.starts_with("44"), "{text}");
    socket.send(Message::Text("40".into())).await.unwrap();
    assert!(recv(&mut socket).await.starts_with("40{"));
    socket
        .send(Message::Text(r#"42["auth","not-a-card"]"#.into()))
        .await
        .unwrap();
    assert!(recv(&mut socket).await.contains("auth_failed"));
    socket
        .send(Message::Text(format!(r#"42["auth","{access}"]"#).into()))
        .await
        .unwrap();
    assert!(recv(&mut socket).await.contains("\"init\""));
    socket.send(Message::Text("2".into())).await.unwrap();
    assert_eq!(recv(&mut socket).await, "3");
    socket
        .send(Message::Text(r#"42["ping"]"#.into()))
        .await
        .unwrap();
    assert!(recv(&mut socket).await.contains("pong"));
    socket.send(Message::Text("3".into())).await.unwrap();
    socket
        .send(Message::Text(r#"42["something_else",{}]"#.into()))
        .await
        .unwrap();
    let patched = send(
        &world,
        Method::PATCH,
        &format!("/player/api/me/progress/{}", world.item),
        As::Card(&access),
        Some(json!({ "currentTime": 42, "duration": 36000 })),
        &[],
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK);
    let event = recv(&mut socket).await;
    assert!(event.contains("user_item_progress_updated"), "{event}");
    socket.send(Message::Text("41".into())).await.unwrap();
}

/// The next text frame other than the server's heartbeat.
async fn recv<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures::StreamExt as _;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("a frame in time")
            .expect("an open socket")
            .unwrap();
        if let Message::Text(text) = frame {
            let text = text.as_str().to_owned();
            if text != "2" {
                return text;
            }
        }
    }
}
