//! The playback routes through the daemon's router, as a signed-in member
//! and as the operator.

use std::sync::Arc;

use axum::http::{header, Method, StatusCode};
use bookclerk_library::{AcquireStatus, FileRole, LibraryStore, NewBook};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;

/// The operator token of these tests' daemon.
const TOKEN: &str = "op-token-playback";

/// Destination key of the first edition's audio.
const KEY: &str = "Ann Author/First/First.m4b";

/// A daemon with one output destination and two members.
struct World {
    /// The daemon's router.
    app: axum::Router,
    /// Its library.
    store: LibraryStore,
    /// Riley's session cookie.
    riley: String,
    /// Sam's session cookie.
    sam: String,
    /// Riley's user id.
    riley_id: i64,
    /// The output destination's folder.
    out: tempfile::TempDir,
    /// The daemon's files folder; removed when the test ends.
    _files: tempfile::TempDir,
}

/// Builds the daemon, signs both members in and connects Riley's store
/// account `acct`.
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
    let mut ids = Vec::new();
    for external in ["riley-ext", "sam-ext"] {
        let identity = store
            .upsert_portal_identity("test", external, Some(external))
            .await
            .unwrap();
        ids.push(identity.user_id.expect("a bridged user"));
    }
    store
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .unwrap();
    World {
        app: crate::api::router(Arc::clone(&state), None),
        riley: crate::auth::tests::portal_cookie_for(&store, "test", "riley-ext").await,
        sam: crate::auth::tests::portal_cookie_for(&store, "test", "sam-ext").await,
        riley_id: ids[0],
        store,
        out,
        _files: files,
    }
}

/// Scans `title` by `author` (product `asin`) onto `acct`; when `key` is
/// given, writes `audio` there on the destination and records the edition
/// as acquired with that file. Answers the edition's id.
async fn edition(
    world: &World,
    asin: &str,
    title: &str,
    author: &str,
    key: Option<(&str, &[u8])>,
) -> String {
    let mut book = NewBook::minimal(asin, "acct", "us", title);
    book.authors = Some(author.into());
    book.length_minutes = Some(600);
    let saved = world.store.upsert_book(&book).await.unwrap();
    if let Some((key, audio)) = key {
        let path = world.out.path().join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, audio).unwrap();
        world
            .store
            .set_acquire_status(asin, "acct", AcquireStatus::Acquired, Some(key), None)
            .await
            .unwrap();
        let owner = world
            .store
            .file_owner_of_book(&saved.uuid)
            .await
            .unwrap()
            .unwrap();
        assert!(world
            .store
            .record_edition_file("local", key, &owner, FileRole::Audio)
            .await
            .unwrap());
    }
    let work = world
        .store
        .work_id_for_book(&saved.uuid)
        .await
        .unwrap()
        .expect("a work");
    world.store.refresh_library_works(&[work]).await.unwrap();
    world
        .store
        .edition_of_book(&saved.uuid)
        .await
        .unwrap()
        .expect("an edition")
}

/// 100 kB of audio-shaped bytes.
fn audio() -> Vec<u8> {
    (0..100_000u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect()
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

/// Sends one request through the router. `cookie` signs a member in;
/// without one the operator token is sent. A member's request states the
/// app's own origin, as the browser does.
async fn send(
    world: &World,
    method: Method,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
    range: Option<&str>,
) -> Answer {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "library.test");
    request = match cookie {
        Some(cookie) => request
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, "http://library.test"),
        None => request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
    };
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
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

/// `GET uri` as `cookie`.
async fn get(world: &World, uri: &str, cookie: Option<&str>) -> Answer {
    send(world, Method::GET, uri, cookie, None, None).await
}

/// `POST uri` with `body` as `cookie`.
async fn post(world: &World, uri: &str, cookie: Option<&str>, body: Value) -> Answer {
    send(world, Method::POST, uri, cookie, Some(body), None).await
}

/// Opens a session on `edition` as `cookie` and answers its JSON.
async fn open(world: &World, cookie: &str, edition: &str) -> Value {
    let answer = post(
        world,
        "/api/playback/sessions",
        Some(cookie),
        json!({"edition_id": edition, "player_name": "Firefox on Linux"}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    answer.json()
}

/// A member opens a session, streams the file whole and by range, reports
/// progress, and closes at the end: the edition is finished, the session
/// streams no more, and the next session starts over.
#[tokio::test]
async fn a_member_plays_an_edition_from_open_to_close() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let world = world().await;
    let bytes = audio();
    let id = edition(
        &world,
        "B0PLAY0001",
        "First",
        "Ann Author",
        Some((KEY, &bytes)),
    )
    .await;
    let riley = world.riley.as_str();

    let manifest = get(
        &world,
        &format!("/api/library/editions/{id}/playback"),
        Some(riley),
    )
    .await;
    assert_eq!(manifest.status, StatusCode::OK, "{}", manifest.json());
    let manifest = manifest.json();
    assert_eq!(manifest["playable"], true);
    assert_eq!(manifest["title"], "First");
    // The catalog's ten hours, until a player reports its own.
    assert_eq!(manifest["duration_ms"], 36_000_000);
    assert_eq!(manifest["tracks"].as_array().unwrap().len(), 1);
    assert_eq!(manifest["tracks"][0]["content_type"], "audio/mp4");
    assert!(manifest["tracks"][0].get("path").is_none());
    assert_eq!(manifest["progress"], Value::Null);

    let session = open(&world, riley, &id).await;
    assert_eq!(session["position_ms"], 0);
    assert_eq!(session["edition_id"], id.as_str());
    assert_eq!(session["idle_secs"], STREAM_IDLE_SECS);
    let session_id = session["session_id"].as_str().unwrap().to_string();
    let track = session["tracks"][0]["path"].as_str().unwrap().to_string();
    assert_eq!(
        track,
        format!("/api/playback/sessions/{session_id}/tracks/0")
    );

    let whole = get(&world, &track, Some(riley)).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.headers[header::CONTENT_TYPE], "audio/mp4");
    assert_eq!(whole.headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(whole.bytes, bytes);
    let part = send(
        &world,
        Method::GET,
        &track,
        Some(riley),
        None,
        Some("bytes=1000-1999"),
    )
    .await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        part.headers[header::CONTENT_RANGE],
        "bytes 1000-1999/100000"
    );
    assert_eq!(part.bytes, &bytes[1000..2000]);
    let tail = send(
        &world,
        Method::GET,
        &track,
        Some(riley),
        None,
        Some("bytes=-10"),
    )
    .await;
    assert_eq!(tail.bytes, &bytes[99_990..]);

    let sync = format!("/api/playback/sessions/{session_id}/sync");
    let close = format!("/api/playback/sessions/{session_id}/close");
    let answer = post(
        &world,
        &sync,
        Some(riley),
        json!({"position_ms": 754_000, "listened_ms": 15_000, "duration_ms": 41_400_000}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    let synced = answer.json();
    assert_eq!(synced["finished_now"], false);
    assert_eq!(synced["progress"]["position_ms"], 754_000);
    assert_eq!(synced["progress"]["duration_ms"], 41_400_000);
    assert_eq!(synced["progress"]["is_finished"], false);
    assert!(synced["progress"]["work_id"].is_string());

    // Continue listening lists it; the manifest now carries the player's
    // own runtime.
    let shelf = get(&world, "/api/playback/progress", Some(riley))
        .await
        .json();
    assert_eq!(shelf["items"].as_array().unwrap().len(), 1);
    assert_eq!(shelf["items"][0]["edition_id"], id.as_str());
    let manifest = get(
        &world,
        &format!("/api/library/editions/{id}/playback"),
        Some(riley),
    )
    .await
    .json();
    assert_eq!(manifest["duration_ms"], 41_400_000);
    assert_eq!(manifest["progress"]["position_ms"], 754_000);

    // Progress forgotten while the session plays starts again with the
    // next report.
    let forgotten = send(
        &world,
        Method::DELETE,
        &format!("/api/playback/progress/{id}"),
        Some(riley),
        None,
        None,
    )
    .await;
    assert_eq!(forgotten.status, StatusCode::NO_CONTENT);
    let answer = post(
        &world,
        &sync,
        Some(riley),
        json!({"position_ms": 760_000, "duration_ms": 41_400_000}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    assert_eq!(answer.json()["progress"]["position_ms"], 760_000);

    // The last report lands inside the final half minute.
    let answer = post(
        &world,
        &close,
        Some(riley),
        json!({"position_ms": 41_390_000, "listened_ms": 5_000}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    assert_eq!(answer.json()["finished_now"], true);
    assert_eq!(answer.json()["progress"]["is_finished"], true);

    // A closed session neither streams nor takes reports.
    let after = get(&world, &track, Some(riley)).await;
    assert_eq!(after.status, StatusCode::CONFLICT);
    assert_eq!(after.json()["error"], "session_ended");
    let after = post(&world, &sync, Some(riley), json!({"position_ms": 1})).await;
    assert_eq!(after.status, StatusCode::CONFLICT);
    assert_eq!(after.json()["error"], "session_ended");

    // Finished editions leave the shelf unless asked for.
    let shelf = get(&world, "/api/playback/progress", Some(riley))
        .await
        .json();
    assert_eq!(shelf["items"].as_array().unwrap().len(), 0);
    let shelf = get(
        &world,
        "/api/playback/progress?include_finished=true",
        Some(riley),
    )
    .await
    .json();
    assert_eq!(shelf["items"][0]["is_finished"], true);
    // The work is finished for Riley too.
    let work = synced["progress"]["work_id"].as_str().unwrap();
    let state = world
        .store
        .user_title_state(world.riley_id, work)
        .await
        .unwrap();
    assert!(state.is_some_and(|state| state.is_finished));

    // Playing a finished edition again starts over.
    assert_eq!(open(&world, riley, &id).await["position_ms"], 0);
}

/// Who and what the routes refuse: the operator (no library user), an
/// unknown or unacquired edition, another person's session, a path that
/// names no track, a malformed report, a changed file set and an edition
/// a patron scope hides.
#[tokio::test]
async fn the_routes_refuse_what_the_caller_may_not_play() {
    use bookclerk_library::control_plane::{
        set_patron_scope, PatronScope, RuleEffect, ScopeField, ScopeRule,
    };

    let _dek = crate::auth::tests::process_dek_lock().await;
    let world = world().await;
    let bytes = audio();
    let id = edition(
        &world,
        "B0PLAY0001",
        "First",
        "Ann Author",
        Some((KEY, &bytes)),
    )
    .await;
    let unacquired = edition(&world, "B0PLAY0002", "Second", "Ben Writer", None).await;
    let (riley, sam) = (world.riley.as_str(), world.sam.as_str());
    let manifest = |id: &str| format!("/api/library/editions/{id}/playback");

    // The operator token has no listening state.
    let answer = get(&world, &manifest(&id), None).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    let answer = post(
        &world,
        "/api/playback/sessions",
        None,
        json!({"edition_id": id}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(
        get(&world, "/api/playback/progress", None).await.status,
        StatusCode::FORBIDDEN
    );

    // Unknown and unacquired editions.
    let answer = get(&world, &manifest("no-such-edition"), Some(riley)).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.json()["error"], "edition no-such-edition not found");
    let answer = get(&world, &manifest(&unacquired), Some(riley)).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.json()["playable"], false);
    assert_eq!(answer.json()["tracks"], json!([]));
    let answer = post(
        &world,
        "/api/playback/sessions",
        Some(riley),
        json!({"edition_id": unacquired}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.json()["error"], "not_playable");
    // A body with a field the route does not know is not read.
    let answer = post(
        &world,
        "/api/playback/sessions",
        Some(riley),
        json!({"edition_id": id, "speed": 2}),
    )
    .await;
    assert!(answer.status.is_client_error(), "{}", answer.status);

    // A session is its opener's alone.
    let session = open(&world, riley, &id).await;
    let session_id = session["session_id"].as_str().unwrap();
    let track = format!("/api/playback/sessions/{session_id}/tracks/0");
    let sync = format!("/api/playback/sessions/{session_id}/sync");
    let answer = get(&world, &track, Some(sam)).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.json()["error"], "unknown_session");
    let answer = post(&world, &sync, Some(sam), json!({"position_ms": 5})).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&world, &track, None).await.status,
        StatusCode::FORBIDDEN
    );
    // A request with neither a session nor the token is not signed in.
    let anonymous = world
        .app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(&track)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    // Paths that name no track.
    for path in [
        format!("/api/playback/sessions/{session_id}/tracks/1"),
        format!("/api/playback/sessions/{session_id}/tracks/01"),
        format!("/api/playback/sessions/{session_id}/tracks/512"),
        format!(
            "/api/playback/sessions/{}/tracks/0",
            session_id.to_uppercase()
        ),
        "/api/playback/sessions/0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b/tracks/0".to_string(),
    ] {
        let answer = get(&world, &path, Some(riley)).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
    }

    // Reports the parser refuses.
    for body in [
        json!({}),
        json!([1]),
        json!({"position_ms": -1}),
        json!({"position_ms": 1, "speed": 2}),
        json!({"position_ms": 5_184_000_001u64}),
    ] {
        let answer = post(&world, &sync, Some(riley), body.clone()).await;
        assert!(answer.status.is_client_error(), "{body}: {}", answer.status);
        assert_ne!(answer.status, StatusCode::CONFLICT, "{body}");
    }
    // A cross-site report is stopped before the route.
    let cross_site = world
        .app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(Method::POST)
                .uri(&sync)
                .header(header::HOST, "library.test")
                .header(header::ORIGIN, "https://elsewhere.test")
                .header(header::COOKIE, riley)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(r#"{"position_ms": 9}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);

    // The scope is read on every request: once the edition is hidden the
    // open session streams no more and the edition reads as unknown.
    assert_eq!(
        get(&world, &track, Some(riley)).await.status,
        StatusCode::OK
    );
    let hide = |scope: Option<PatronScope>| set_patron_scope(&world.store, world.riley_id, scope);
    hide(Some(PatronScope {
        rules: vec![ScopeRule {
            field: ScopeField::Author,
            value: "Ben Writer".into(),
            effect: RuleEffect::Include,
        }],
    }))
    .await
    .unwrap();
    let answer = get(&world, &track, Some(riley)).await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert_eq!(answer.json()["error"], "session_ended");
    assert_eq!(
        get(&world, &manifest(&id), Some(riley)).await.status,
        StatusCode::NOT_FOUND
    );
    let answer = post(
        &world,
        "/api/playback/sessions",
        Some(riley),
        json!({"edition_id": id}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    let shelf_before = get(
        &world,
        "/api/playback/progress?include_finished=true",
        Some(riley),
    )
    .await
    .json();
    assert_eq!(
        shelf_before["items"],
        json!([]),
        "hidden rows are not listed"
    );
    hide(None).await.unwrap();
    assert_eq!(
        get(&world, &track, Some(riley)).await.status,
        StatusCode::OK
    );

    // The edition's files change under the session: it ends, and a new
    // one lists both tracks.
    let book = world
        .store
        .get_book("B0PLAY0001", "acct")
        .await
        .unwrap()
        .unwrap();
    let owner = world
        .store
        .file_owner_of_book(&book.uuid)
        .await
        .unwrap()
        .unwrap();
    const PART: &str = "Ann Author/First/First - Part 2.mp3";
    std::fs::write(world.out.path().join(PART), b"second part").unwrap();
    assert!(world
        .store
        .record_edition_file("local", PART, &owner, FileRole::Part)
        .await
        .unwrap());
    let answer = get(&world, &track, Some(riley)).await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    let session = open(&world, riley, &id).await;
    assert_eq!(session["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(session["tracks"][1]["content_type"], "audio/mpeg");
    let second = get(
        &world,
        session["tracks"][1]["path"].as_str().unwrap(),
        Some(riley),
    )
    .await;
    assert_eq!(second.bytes, b"second part");
}

/// Progress set and forgotten outright, paged by cursor, and bookmarks
/// that only their maker reads or changes.
#[tokio::test]
async fn a_member_keeps_progress_and_bookmarks_of_their_own() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let world = world().await;
    let first = edition(&world, "B0PLAY0001", "First", "Ann Author", None).await;
    let second = edition(&world, "B0PLAY0002", "Second", "Ben Writer", None).await;
    let (riley, sam) = (world.riley.as_str(), world.sam.as_str());
    let put = |uri: String, cookie: &'static str, body: Value| {
        let world = &world;
        async move {
            let cookie = if cookie == "riley" {
                &world.riley
            } else {
                &world.sam
            };
            send(world, Method::PUT, &uri, Some(cookie), Some(body), None).await
        }
    };

    // Progress needs no copy on this node: it is the person's own note.
    for (id, position) in [(&first, 1_000), (&second, 2_000)] {
        let answer = put(
            format!("/api/playback/progress/{id}"),
            "riley",
            json!({"position_ms": position, "duration_ms": 3_600_000}),
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
        assert_eq!(answer.json()["progress"]["position_ms"], position);
        assert_eq!(answer.json()["finished_now"], false);
    }
    let answer = put(
        format!("/api/playback/progress/{first}"),
        "riley",
        json!({"position_ms": 5_184_000_001u64}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    let answer = put(
        "/api/playback/progress/no-such-edition".to_string(),
        "riley",
        json!({"position_ms": 1}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);

    // Most recent first, one row a page.
    let page = get(&world, "/api/playback/progress?limit=1", Some(riley))
        .await
        .json();
    assert_eq!(page["items"][0]["edition_id"], second.as_str());
    let cursor = page["next_cursor"].as_str().expect("a second page");
    let page = get(
        &world,
        &format!("/api/playback/progress?limit=1&cursor={cursor}"),
        Some(riley),
    )
    .await
    .json();
    assert_eq!(page["items"][0]["edition_id"], first.as_str());
    assert!(page.get("next_cursor").is_none());
    for bad in ["not-base64!", "bm90LWEtY3Vyc29y", &"A".repeat(300)] {
        let answer = get(
            &world,
            &format!("/api/playback/progress?cursor={bad}"),
            Some(riley),
        )
        .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    // Sam has none of it.
    let page = get(&world, "/api/playback/progress", Some(sam))
        .await
        .json();
    assert_eq!(page["items"], json!([]));

    // Marked finished by hand, then forgotten.
    let answer = put(
        format!("/api/playback/progress/{first}"),
        "riley",
        json!({"is_finished": true}),
    )
    .await;
    assert_eq!(answer.json()["finished_now"], true);
    let answer = send(
        &world,
        Method::DELETE,
        &format!("/api/playback/progress/{first}"),
        Some(riley),
        None,
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    let page = get(
        &world,
        "/api/playback/progress?include_finished=true",
        Some(riley),
    )
    .await
    .json();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);

    // Bookmarks.
    let bookmarks = format!("/api/library/editions/{first}/bookmarks");
    let answer = post(
        &world,
        &bookmarks,
        Some(riley),
        json!({"position_ms": 9_000, "title": "  The storm  "}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    assert_eq!(answer.json()["title"], "The storm");
    let mark = answer.json()["id"].as_str().unwrap().to_string();
    post(&world, &bookmarks, Some(riley), json!({"position_ms": 500})).await;
    for body in [
        json!({"position_ms": 1, "title": "a\u{7}b"}),
        json!({"position_ms": 1, "title": "x".repeat(MAX_BOOKMARK_TITLE_BYTES + 1)}),
        json!({"position_ms": 5_184_000_001u64}),
    ] {
        let answer = post(&world, &bookmarks, Some(riley), body).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    }
    let listed = get(&world, &bookmarks, Some(riley)).await.json();
    let positions: Vec<u64> = listed["bookmarks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|bookmark| bookmark["position_ms"].as_u64())
        .collect();
    assert_eq!(positions, [500, 9_000], "in timeline order");
    assert_eq!(
        get(&world, &bookmarks, Some(sam)).await.json()["bookmarks"],
        json!([])
    );
    assert_eq!(
        get(&world, &bookmarks, None).await.status,
        StatusCode::FORBIDDEN
    );
    // The manifest carries them.
    let manifest = get(
        &world,
        &format!("/api/library/editions/{first}/playback"),
        Some(riley),
    )
    .await
    .json();
    assert_eq!(manifest["bookmarks"].as_array().unwrap().len(), 2);

    let one = format!("/api/playback/bookmarks/{mark}");
    let answer = put(one.clone(), "sam", json!({"title": "mine now"})).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    let answer = send(&world, Method::DELETE, &one, Some(sam), None, None).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    let answer = put(
        one.clone(),
        "riley",
        json!({"title": "Landfall", "position_ms": 9_500}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    assert_eq!(answer.json()["title"], "Landfall");
    assert_eq!(answer.json()["position_ms"], 9_500);
    let answer = put(
        "/api/playback/bookmarks/not-an-id".to_string(),
        "riley",
        json!({"title": "x"}),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    let answer = send(&world, Method::DELETE, &one, Some(riley), None, None).await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    let answer = send(&world, Method::DELETE, &one, Some(riley), None, None).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
}

/// A track states the probe's media type when it is an audio one, else
/// one read from the key's extension.
#[test]
fn a_track_states_a_media_type_a_player_can_use() {
    let track = |key: &str, content_type: Option<&str>| PlaybackTrack {
        key: key.into(),
        content_type: content_type.map(str::to_string),
        bitrate_kbps: None,
        lossless: false,
    };
    for (key, probed, expected) in [
        ("a/b.m4b", Some("audio/mp4"), "audio/mp4"),
        ("a/b.bin", Some("audio/flac"), "audio/flac"),
        ("a/b.bin", Some("Audio/MP4; codecs=mp4a.40.2"), "audio/mp4"),
        ("a/b.bin", Some("video/mp4"), "application/octet-stream"),
        ("a/b.m4b", Some("text/html"), "audio/mp4"),
        ("a/b.m4b", Some("audio/\nmp4"), "audio/mp4"),
        ("a/b.MP3", None, "audio/mpeg"),
        ("a/b.flac", None, "audio/flac"),
        ("a/b.opus", None, "audio/ogg"),
        ("a/b.ogg", None, "audio/ogg"),
        ("a/b.aac", None, "audio/aac"),
        ("a/b.wav", None, "audio/wav"),
        ("a.dir/b", None, "application/octet-stream"),
        ("a/b.exe", None, "application/octet-stream"),
    ] {
        assert_eq!(track_content_type(&track(key, probed)), expected, "{key}");
    }
}

/// A page cursor round-trips, and anything else is refused.
#[test]
fn a_progress_cursor_round_trips() {
    let cursor = ProgressCursor {
        updated_at: "2026-11-19T09:02:41+00:00".into(),
        edition_id: "5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d".into(),
    };
    let token = progress_cursor(&cursor);
    let back = progress_cursor_of(&token).expect("a cursor");
    assert_eq!(back.updated_at, cursor.updated_at);
    assert_eq!(back.edition_id, cursor.edition_id);
    let encode = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
    for bad in [
        String::new(),
        encode("no separator"),
        encode("yesterday|edition"),
        encode("2026-11-19T09:02:41+00:00|"),
        "****".to_string(),
    ] {
        assert!(progress_cursor_of(&bad).is_none(), "{bad}");
    }
}
