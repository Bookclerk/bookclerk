//! Wiremock fixtures for Chirp GraphQL login, library, and single audiobook.

use bookclerk_library::configure_master_key;
use bookclerk_plugin_source_chirp::{
    fetch_title_materials, load_auth_from_db, save_auth_to_db, ChirpAuthFile, ChirpClient,
    ChirpSource,
};
use bookclerk_source::{ContentSource, LoginOptions, ScanOptions};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn graphql_response(body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn op_name(req: &Request) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(&req.body).ok()?;
    v.get("operationName")
        .and_then(|o| o.as_str())
        .map(str::to_string)
}

fn dek_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn setup_dek() -> (tokio::sync::MutexGuard<'static, ()>, TempDir) {
    let guard = dek_lock().lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::remove_var(bookclerk_library::MASTER_KEY_AUTH_PASSWORD_ENV);
    configure_master_key(dir.path()).unwrap();
    (guard, dir)
}

#[tokio::test]
async fn signin_saves_auth_to_db() {
    let (_guard, _dek_dir) = setup_dek().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(|req: &Request| {
            assert_eq!(op_name(req).as_deref(), Some("signIn"));
            graphql_response(serde_json::json!({
                "data": {
                    "signIn": {
                        "user": {
                            "id": "42",
                            "token": "jwt-access",
                            "webToken": "jwt-web",
                            "email": "reader@example.com"
                        }
                    }
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let store = bookclerk_plugin_database::sqlite::open_store_memory()
        .await
        .unwrap();
    let source = ChirpSource::with_graphql_url(server.uri());
    let account = source
        .login(
            &store.scope("chirp"),
            LoginOptions {
                marketplace: "us".into(),
                label: Some("Chirp".into()),
                email: Some("reader@example.com".into()),
                password: Some("secret".into()),
                force: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(account.source, "chirp");
    // user_id = "42" is used as account_id by ChirpAuthFile::account_id().
    assert_eq!(account.account_id, "42");

    // Verify credentials were persisted to the DB.
    let auth = load_auth_from_db(&store.scope("chirp"), "42")
        .await
        .unwrap()
        .expect("auth must be present in DB");
    assert_eq!(auth.access_token, "jwt-access");
    assert_eq!(auth.user_id.as_deref(), Some("42"));
}

#[tokio::test]
async fn empty_library_scan_upserts_zero() {
    let (_guard, _dek_dir) = setup_dek().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(|req: &Request| {
            assert_eq!(
                op_name(req).as_deref(),
                Some("AndroidCurrentUserAudiobooks")
            );
            graphql_response(serde_json::json!({
                "data": { "currentUserAudiobooks": [] }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    // Credentials are now stored in the DB (not files).
    let store = bookclerk_plugin_database::sqlite::open_store_memory()
        .await
        .unwrap();
    save_auth_to_db(
        &ChirpAuthFile {
            access_token: "tok".into(),
            web_token: None,
            email: "a@ex.com".into(),
            user_id: Some("9".into()),
            marketplace: "us".into(),
            label: None,
        },
        &store.scope("chirp"),
        "9",
    )
    .await
    .unwrap();

    let source = ChirpSource::with_graphql_url(server.uri());
    let summary = source
        .scan(&store.scope("chirp"), ScanOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.books_upserted, 0);
    assert_eq!(summary.accounts, 1);
}

#[tokio::test]
async fn library_scan_upserts_books() {
    let (_guard, _dek_dir) = setup_dek().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(|req: &Request| {
            assert_eq!(
                op_name(req).as_deref(),
                Some("AndroidCurrentUserAudiobooks")
            );
            graphql_response(serde_json::json!({
                "data": {
                    "currentUserAudiobooks": [{
                        "id": "ua-1",
                        "archived": false,
                        "playable": true,
                        "audiobook": {
                            "id": "ab-100",
                            "displayTitle": "Deal Book",
                            "displayAuthors": "Author A",
                            "displayNarrators": "Narrator N",
                            "durationMs": 3_600_000,
                            "abridged": false,
                            "publisher": "Pub",
                            "releasedOn": "2024-01-01"
                        }
                    }]
                }
            }))
        })
        .mount(&server)
        .await;

    // Credentials are now stored in the DB (not files).
    let store = bookclerk_plugin_database::sqlite::open_store_memory()
        .await
        .unwrap();
    save_auth_to_db(
        &ChirpAuthFile {
            access_token: "tok".into(),
            web_token: None,
            email: "a@ex.com".into(),
            user_id: Some("9".into()),
            marketplace: "us".into(),
            label: None,
        },
        &store.scope("chirp"),
        "9",
    )
    .await
    .unwrap();

    let source = ChirpSource::with_graphql_url(server.uri());
    let summary = source
        .scan(&store.scope("chirp"), ScanOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.books_upserted, 1);
    let books = store.list_books(None).await.unwrap();
    assert_eq!(books[0].product_id, "ab-100");
    assert_eq!(books[0].source, "chirp");
}

#[tokio::test]
async fn fetch_title_downloads_tracks() {
    let server = MockServer::start().await;
    let media_url = format!("{}/t1.mp3", server.uri());
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(move |req: &Request| {
            assert_eq!(op_name(req).as_deref(), Some("AndroidSingleAudiobook"));
            graphql_response(serde_json::json!({
                "data": {
                    "audiobook": {
                        "id": "ab-1",
                        "displayTitle": "T",
                        "tracks": [{
                            "id": "tr-1",
                            "mediaUrl": media_url,
                            "chapterNumber": 1,
                            "partNumber": 1,
                            "durationMs": 1000,
                            "offsetFromBookStartMs": 0,
                            "displayName": "Chapter 1"
                        }]
                    }
                }
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/t1.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3track"))
        .mount(&server)
        .await;

    let client = ChirpClient::new(server.uri()).with_token("tok");
    let cache = tempfile::tempdir().unwrap();
    let plain = fetch_title_materials(&client, "ab-1", cache.path())
        .await
        .unwrap();
    assert_eq!(plain.parts.len(), 1);
    assert_eq!(plain.chapters.len(), 1);
}

#[tokio::test]
async fn fetch_via_content_source() {
    let (_guard, _dek_dir) = setup_dek().await;
    let server = MockServer::start().await;
    let media_url = format!("{}/x.mp3", server.uri());
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(move |_req: &Request| {
            graphql_response(serde_json::json!({
                "data": {
                    "audiobook": {
                        "id": "ab-2",
                        "tracks": [{
                            "id": "t",
                            "mediaUrl": media_url,
                            "durationMs": 1,
                            "offsetFromBookStartMs": 0,
                            "displayName": "One"
                        }]
                    }
                }
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/x.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3"))
        .mount(&server)
        .await;

    // Credentials are now stored in the DB (not files).
    let store = bookclerk_plugin_database::sqlite::open_store_memory()
        .await
        .unwrap();
    save_auth_to_db(
        &ChirpAuthFile {
            access_token: "tok".into(),
            web_token: None,
            email: "u@ex.com".into(),
            user_id: Some("1".into()),
            marketplace: "us".into(),
            label: None,
        },
        &store.scope("chirp"),
        "1",
    )
    .await
    .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let source = ChirpSource::with_graphql_url(server.uri());
    let fetch = source
        .fetch_title(
            &store.scope("chirp"),
            "1",
            "ab-2",
            &bookclerk_source::FetchOptions {
                download: bookclerk_source::DownloadOptions::default(),
                cache_dir: cache.path().to_path_buf(),
                files_dir: cache.path().to_path_buf(),
            },
        )
        .await
        .unwrap();
    assert_eq!(fetch.parts.len(), 1);
}
