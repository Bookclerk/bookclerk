//! Book samples: the cache, the start rule, ranges, and the routes through
//! the daemon's router.

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::{header, Method, StatusCode};
use bookclerk_library::{AcquireStatus, FileRole, LibraryStore, NewBook};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;

/// The operator token of these tests' daemon.
const TOKEN: &str = "op-token-samples";

/// Destination key of the edition's audio. The name says M4B, which makes
/// it a track; the worker reads the bytes (a WAV) by their content.
const KEY: &str = "Ann Author/First/First.m4b";

#[test]
fn a_short_first_chapter_is_skipped() {
    assert_eq!(sample_start(&[]), 0);
    assert_eq!(sample_start(&[0]), 0);
    // Opening credits of 40 s: the sample starts at chapter two.
    assert_eq!(sample_start(&[0, 40_000, 900_000]), 40_000);
    // A real first chapter: the sample starts at the beginning.
    assert_eq!(sample_start(&[0, OPENING_MAX_MS, 900_000]), 0);
    assert_eq!(sample_start(&[5_000, 20_000]), 20_000);
}

#[test]
fn only_top_level_chapters_mark_the_opening() {
    let chapter = |start_ms: u64, depth: u32| bookclerk_library::Chapter {
        title: format!("at {start_ms}"),
        start_ms,
        depth,
    };
    // Part One holds a 40 s first sub-chapter; the opening is the whole
    // part, so the sample starts at the beginning.
    let parts = [
        chapter(0, 0),
        chapter(0, 1),
        chapter(40_000, 1),
        chapter(1_200_000, 0),
    ];
    assert_eq!(top_level_starts(&parts), vec![0, 1_200_000]);
    assert_eq!(sample_start(&top_level_starts(&parts)), 0);
    let credits = [chapter(0, 0), chapter(30_000, 0), chapter(60_000, 1)];
    assert_eq!(sample_start(&top_level_starts(&credits)), 30_000);
}

#[tokio::test]
async fn a_flight_lock_is_forgotten_only_by_its_last_holder() {
    let service = SampleService::detached();
    let first = service.ticket("a.mp3");
    let second = service.ticket("a.mp3");
    assert!(
        Arc::ptr_eq(&first.lock, &second.lock),
        "one lock per sample"
    );
    drop(first);
    assert!(
        service.flights.lock().unwrap().contains_key("a.mp3"),
        "a waiter still needs the same lock"
    );
    drop(second);
    assert!(service.flights.lock().unwrap().is_empty());

    // A ticket whose lock the map no longer holds never removes the
    // entry that replaced it.
    let stale = service.ticket("b.mp3");
    let replacement = Arc::new(tokio::sync::Mutex::new(()));
    service
        .flights
        .lock()
        .unwrap()
        .insert("b.mp3".into(), Arc::clone(&replacement));
    drop(stale);
    assert!(service.flights.lock().unwrap().contains_key("b.mp3"));
}

#[tokio::test]
async fn a_cancelled_waiter_leaves_no_lock_behind() {
    let service = Arc::new(SampleService::detached());
    let holder = service.ticket("c.mp3");
    let guard = holder.lock.lock().await;
    let waiting = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            let ticket = service.ticket("c.mp3");
            let _guard = ticket.lock.lock().await;
        })
    };
    // Let the waiter register, then cancel it while it waits.
    while Arc::strong_count(&holder.lock) < 3 {
        tokio::task::yield_now().await;
    }
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    drop(guard);
    drop(holder);
    assert!(service.flights.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_ending_together_leave_no_lock_behind() {
    let dir = tempfile::tempdir().unwrap();
    let service = Arc::new(SampleService::open(dir.path().join("samples")).unwrap());
    for round in 0..50 {
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let service = Arc::clone(&service);
                let key = format!("edition\n{round}");
                tokio::spawn(async move {
                    service
                        .sample(&key, || async { Ok(vec![1]) })
                        .await
                        .unwrap()
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        assert!(service.flights.lock().unwrap().is_empty());
    }
}

/// A request cancelled while its sample is being written leaves no lock
/// behind once the write finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_cancelled_during_the_write_leaves_no_lock_behind() {
    let dir = tempfile::tempdir().unwrap();
    let service = Arc::new(SampleService::open(dir.path().join("samples")).unwrap());
    for round in 0..20 {
        let request = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                let key = format!("edition\n{round}");
                service.sample(&key, || async { Ok(vec![2; 4096]) }).await
            })
        };
        tokio::task::yield_now().await;
        request.abort();
        let _ = request.await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !service.flights.lock().unwrap().is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a lock outlived its write"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

#[tokio::test]
async fn a_range_answer_of_the_wrong_length_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let backend = bookclerk_storage::LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
    std::fs::write(dir.path().join("f.bin"), [7u8; 32]).unwrap();
    let file = StoredFile {
        backend: Box::new(backend),
        key: "f.bin".into(),
    };
    let part = file
        .read_range(ByteSpan { offset: 4, len: 8 })
        .await
        .unwrap();
    assert_eq!(part, vec![7u8; 8]);
    assert!(file
        .read_range(ByteSpan { offset: 28, len: 8 })
        .await
        .is_err());
}

#[test]
fn store_tokens_open_only_on_the_node_that_signed_them() {
    let service = SampleService::detached();
    let path = service
        .path_for(" https://samples.example/b/a.mp3 ")
        .unwrap();
    let token = path.strip_prefix("/api/samples/").unwrap();
    assert_eq!(
        service.open_token(token).as_deref(),
        Some("https://samples.example/b/a.mp3")
    );
    assert_eq!(SampleService::detached().open_token(token), None);
    assert_eq!(service.path_for("http://samples.example/a.mp3"), None);
    assert_eq!(service.open_token("not-a-token"), None);
}

#[tokio::test]
async fn a_sample_is_made_once_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    let service = SampleService::open(dir.path().join("samples")).unwrap();
    let made = AtomicUsize::new(0);
    for _ in 0..3 {
        let bytes = service
            .sample("edition\nx", || async {
                made.fetch_add(1, Ordering::SeqCst);
                Ok(vec![1, 2, 3])
            })
            .await
            .unwrap();
        assert_eq!(bytes, vec![1, 2, 3]);
    }
    assert_eq!(made.load(Ordering::SeqCst), 1);
    // A failure is answered and not kept.
    let failed = service
        .sample("edition\ny", || async {
            Err(SampleFailure::Refused("not audio".into()))
        })
        .await;
    assert_eq!(failed, Err(SampleFailure::Refused("not audio".into())));
    let again = service
        .sample("edition\ny", || async { Ok(vec![9]) })
        .await
        .unwrap();
    assert_eq!(again, vec![9]);
    // A service with no folder makes the sample every time.
    let detached = SampleService::detached();
    let made = AtomicUsize::new(0);
    for _ in 0..2 {
        detached
            .sample("edition\nx", || async {
                made.fetch_add(1, Ordering::SeqCst);
                Ok(vec![1])
            })
            .await
            .unwrap();
    }
    assert_eq!(made.load(Ordering::SeqCst), 2);
}

#[test]
fn the_cache_drops_the_least_recently_served_samples_first() {
    let dir = tempfile::tempdir().unwrap();
    let old = SystemTime::now() - std::time::Duration::from_secs(3600);
    for (name, age) in [("a.mp3", 3), ("b.mp3", 2), ("c.mp3", 1)] {
        let path = dir.path().join(name);
        std::fs::write(&path, vec![0u8; 100]).unwrap();
        let when = old + std::time::Duration::from_secs(60 * (10 - age));
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
    std::fs::write(dir.path().join(KEY_FILE), [0u8; 32]).unwrap();
    trim(dir.path(), 200);
    assert!(!dir.path().join("a.mp3").exists());
    assert!(dir.path().join("b.mp3").exists());
    assert!(dir.path().join("c.mp3").exists());
    assert!(dir.path().join(KEY_FILE).exists());
}

/// Reads a response's status, headers and body.
async fn read(response: Response) -> (StatusCode, HeaderMap, Vec<u8>) {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

#[tokio::test]
async fn a_sample_is_served_whole_or_by_range() {
    let bytes: Vec<u8> = (0..10u8).collect();
    let (status, headers, body) =
        read(mp3_response(bytes.clone(), None, STORE_CACHE_CONTROL)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(body, bytes);
    let range = HeaderValue::from_static("bytes=2-4");
    let (status, headers, body) = read(mp3_response(
        bytes.clone(),
        Some(&range),
        STORE_CACHE_CONTROL,
    ))
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes 2-4/10");
    assert_eq!(body, vec![2, 3, 4]);
    let past = HeaderValue::from_static("bytes=10-");
    let (status, headers, _) = read(mp3_response(bytes, Some(&past), STORE_CACHE_CONTROL)).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes */10");
}

/// A daemon with one output destination and one member.
struct World {
    /// The daemon's router.
    app: axum::Router,
    /// Its library.
    store: LibraryStore,
    /// Riley's session cookie.
    riley: String,
    /// The output destination's folder.
    out: tempfile::TempDir,
    /// The daemon's files folder; removed when the test ends.
    _files: tempfile::TempDir,
}

/// Builds the daemon and signs Riley in.
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
    store
        .upsert_portal_identity("test", "riley-ext", Some("riley-ext"))
        .await
        .unwrap();
    store
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .unwrap();
    World {
        app: crate::api::router(Arc::clone(&state), None),
        riley: crate::auth::tests::portal_cookie_for(&store, "test", "riley-ext").await,
        store,
        out,
        _files: files,
    }
}

/// Scans a title onto `acct`; with `audio`, stores it at [`KEY`] and
/// records the edition as acquired. Answers the edition's id.
async fn edition(world: &World, asin: &str, audio: Option<&[u8]>) -> String {
    let mut book = NewBook::minimal(asin, "acct", "us", "First");
    book.authors = Some("Ann Author".into());
    book.length_minutes = Some(600);
    let saved = world.store.upsert_book(&book).await.unwrap();
    if let Some(audio) = audio {
        let path = world.out.path().join(KEY);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, audio).unwrap();
        world
            .store
            .set_acquire_status(asin, "acct", AcquireStatus::Acquired, Some(KEY), None)
            .await
            .unwrap();
        let owner = world
            .store
            .file_owner_of_holding(&saved.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert!(world
            .store
            .record_edition_file("local", KEY, &owner, FileRole::Audio)
            .await
            .unwrap());
    }
    let work = world
        .store
        .work_of_holding(&saved.holding_id)
        .await
        .unwrap()
        .expect("a work");
    world.store.refresh_library_works(&[work]).await.unwrap();
    world
        .store
        .edition_of_holding(&saved.holding_id)
        .await
        .unwrap()
        .expect("an edition")
}

/// Ten seconds of 8 kHz mono WAV.
fn wav() -> Vec<u8> {
    let frames = 80_000u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + frames * 2).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    for value in [16u32, 0x0001_0001, 8_000, 16_000, 0x0010_0002] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(frames * 2).to_le_bytes());
    for frame in 0..frames {
        out.extend_from_slice(&i16::try_from(frame % 100).unwrap().to_le_bytes());
    }
    out
}

/// Sends one request as `cookie`, or as the operator without one.
async fn send(
    world: &World,
    method: Method,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
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
    read(response).await
}

/// A member hears a sample of an edition this node holds, re-encoded as
/// MP3; it records no progress and opens no session. What is not theirs to
/// hear, or not here, is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_hears_a_sample_that_is_not_listening() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let world = world().await;
    let held = edition(&world, "B0SAMP0001", Some(&wav())).await;
    let unheld = edition(&world, "B0SAMP0002", None).await;
    let riley = Some(world.riley.as_str());

    let uri = |id: &str| format!("/api/library/editions/{id}/sample");
    let (status, _, _) = send(&world, Method::GET, &uri(&held), None, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the operator has no library user"
    );
    let (status, _, body) = send(&world, Method::GET, &uri(&unheld), riley, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"],
        "no_sample"
    );
    let (status, _, _) = send(&world, Method::GET, &uri("nope"), riley, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    if bookclerk_media::pool().is_isolated() {
        let (status, headers, body) = send(&world, Method::GET, &uri(&held), riley, None).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(headers[header::CONTENT_TYPE], "audio/mpeg");
        assert_eq!(headers[header::CACHE_CONTROL], EDITION_CACHE_CONTROL);
        assert_eq!(body[0], 0xff, "an MPEG audio frame, not the WAV");
    } else {
        assert!(
            std::env::var_os("BOOKCLERK_REQUIRE_TEST_GUESTS").is_none(),
            "the media worker is not built"
        );
        let (status, _, _) = send(&world, Method::GET, &uri(&held), riley, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    // Nothing of it is listening state: no play session was opened, not
    // even one closed straight away, so listening stats see nothing.
    {
        use sea_orm::{EntityTrait, PaginatorTrait};
        let sessions = bookclerk_library::entities::playback_sessions::Entity::find()
            .count(world.store.db())
            .await
            .unwrap();
        assert_eq!(sessions, 0, "a sample opens no play session");
    }
    let (status, _, body) = send(
        &world,
        Method::GET,
        "/api/playback/progress?include_finished=true",
        riley,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["items"], json!([]));
    let (status, _, body) = send(
        &world,
        Method::GET,
        &format!("/api/library/editions/{held}/playback"),
        riley,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let manifest: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        manifest.get("progress"),
        Some(&Value::Null),
        "the manifest states no progress"
    );
}

/// A store sample path this node did not issue is refused, and a title no
/// store offers a sample of gets no path.
#[tokio::test]
async fn store_samples_need_a_path_this_node_issued() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let world = world().await;
    let riley = Some(world.riley.as_str());
    let foreign = SampleService::detached()
        .path_for("https://samples.example/a.mp3")
        .unwrap();
    let (status, _, body) = send(&world, Method::GET, &foreign, riley, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"],
        "unknown_sample"
    );
    let (status, _, body) = send(
        &world,
        Method::POST,
        "/api/discover/title-sample",
        riley,
        Some(json!({"title": "First", "store_editions": [{"source": "libro", "product_id": "9780000000001"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({}));
    let (status, _, _) = send(
        &world,
        Method::POST,
        "/api/discover/title-sample",
        riley,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
