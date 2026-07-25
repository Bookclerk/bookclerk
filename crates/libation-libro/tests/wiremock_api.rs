//! Wiremock fixtures for Libro.fm oauth, library, and download-manifest.

use std::io::{Cursor, Write};

use libation_library::LibraryStore;
use libation_libro::{
    fetch_title_materials, load_auth, save_auth, scan_account_into_library, LibroAuthFile,
    LibroClient, LibroSource, APP_VER, DOWNLOAD_MANIFEST_PATH, LIBRARY_PATH, PACKAGED_M4B_PATH,
    USER_AGENT_VALUE,
};
use libation_source::{ContentSource, LoginOptions, ScanOptions, SourceFetch};
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn sample_audiobook(isbn: &str, title: &str) -> serde_json::Value {
    serde_json::json!({
        "isbn": isbn,
        "title": title,
        "authors": ["Ada Author"],
        "subtitle": "A Tale",
        "publisher": "Indie Press",
        "publication_date": "2024-01-15",
        "abridged": false,
        "series": "Test Series",
        "series_num": 1,
        "cover_url": "https://cdn.example/cover.jpg",
        "genres": [{"name": "Fiction"}],
        "audiobook_info": {
            "narrators": ["Ned Narrator"],
            "duration": 3661,
            "track_count": 2,
            "parts_count": 1
        },
        "user_metadata": {
            "added_at": "2024-06-01T12:00:00Z",
            "hidden": false
        }
    })
}

fn zip_with_mp3() -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut cursor);
        zip.start_file("01 - Intro.mp3", SimpleFileOptions::default())
            .unwrap();
        // Minimal MPEG frame-ish payload (ID3 tag header is enough for sniffing elsewhere).
        zip.write_all(b"ID3\x03\x00\x00\x00\x00\x00\x00fake-mp3-bytes")
            .unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

#[tokio::test]
async fn oauth_token_login_saves_auth_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(header("X-LibroFm-AppVer", APP_VER))
        .and(header("user-agent", USER_AGENT_VALUE))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test-token-abc",
            "token_type": "Bearer",
            "created_at": 1_700_000_000,
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let source = LibroSource::with_base_url(server.uri());
    let account = source
        .login(
            dir.path(),
            LoginOptions {
                marketplace: "us".into(),
                label: Some("Main".into()),
                email: Some("reader@example.com".into()),
                password: Some("secret".into()),
                force: true,
            },
        )
        .await
        .unwrap();

    assert_eq!(account.source, "libro");
    assert_eq!(account.account_id, "reader@example.com");

    let auth = load_auth(&libation_libro::auth_file_for_account(
        dir.path(),
        Some("Main"),
        "reader@example.com",
    ))
    .unwrap();
    assert_eq!(auth.access_token, "test-token-abc");
    assert!(auth.expires_at.is_some());
}

#[tokio::test]
async fn library_page_upserts_libro_books() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(LIBRARY_PATH))
        .and(query_param("page", "1"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "page": 1,
            "total_pages": 1,
            "audiobooks": [
                sample_audiobook("9781234567890", "Example Book"),
                {
                    "isbn": 9789999999999i64,
                    "title": "Numeric Isbn Book",
                    "authors": ["Ada Author"],
                    "audiobook_info": { "narrators": ["Ned Narrator"], "duration": 120 }
                }
            ],
            "tags": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let store = LibraryStore::open_in_memory().unwrap();
    store
        .upsert_account_with_source("reader@example.com", "us", Some("Main"), true, "libro")
        .unwrap();

    let client = LibroClient::new(server.uri()).with_token("tok");
    let (books, pages) = scan_account_into_library(&store, &client, "reader@example.com", "us")
        .await
        .unwrap();

    assert_eq!(pages, 1);
    assert_eq!(books, 2);

    let found = store.find_books_by_isbn("9781234567890").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].source, "libro");
    assert_eq!(found[0].product_id, "9781234567890");
    assert!(found[0].asin.is_none());
    assert_eq!(found[0].authors.as_deref(), Some("Ada Author"));
    assert_eq!(found[0].length_minutes, Some(61));
}

#[tokio::test]
async fn download_manifest_format_m4b_preferred() {
    let server = MockServer::start().await;
    let m4b_url = format!("{}/cdn/book.m4b", server.uri());

    // format=m4b → single .m4b part + tracks (Android MediaFormat.M4B).
    Mock::given(method("GET"))
        .and(path(DOWNLOAD_MANIFEST_PATH))
        .and(query_param("isbn", "9780000000001"))
        .and(query_param("client_version", APP_VER))
        .and(query_param("format", "m4b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "isbn": "9780000000001",
            "parts": [{"url": m4b_url, "size_bytes": 12}],
            "tracks": [
                {"number": 1, "length_msec": 5000, "chapter_title": "All"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    // packaged_m4b must not be needed when format=m4b succeeds.
    let packaged = PACKAGED_M4B_PATH.replace("{isbn}", "9780000000001");
    Mock::given(method("GET"))
        .and(path(packaged.as_str()))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/cdn/book.m4b"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"xxxxftypM4Bfake".to_vec()))
        .expect(1)
        .mount(&server)
        .await;

    let cache = tempfile::tempdir().unwrap();
    let client = LibroClient::new(server.uri()).with_token("tok");
    let plain = fetch_title_materials(&client, "9780000000001", cache.path())
        .await
        .unwrap();

    assert!(plain.parts.is_empty());
    let m4b = plain.m4b_path.expect("m4b path");
    assert_eq!(m4b.extension().unwrap(), "m4b");
    assert_eq!(plain.chapters[0].0, "All");
}

#[tokio::test]
async fn download_manifest_extracts_mp3_parts() {
    let server = MockServer::start().await;
    let zip_bytes = zip_with_mp3();

    // format=m4b with no M4B part → fall through to packaged_m4b / ZIP.
    Mock::given(method("GET"))
        .and(path(DOWNLOAD_MANIFEST_PATH))
        .and(query_param("isbn", "9781111111111"))
        .and(query_param("client_version", APP_VER))
        .and(query_param("format", "m4b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "isbn": "9781111111111",
            "parts": [],
            "tracks": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let packaged = PACKAGED_M4B_PATH.replace("{isbn}", "9781111111111");
    Mock::given(method("GET"))
        .and(path(packaged.as_str()))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(DOWNLOAD_MANIFEST_PATH))
        .and(query_param("isbn", "9781111111111"))
        .and(query_param("client_version", APP_VER))
        .and(query_param_is_missing("format"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "isbn": "9781111111111",
            "parts": [{
                "url": format!("{}/cdn/part0.zip", server.uri()),
                "size_bytes": zip_bytes.len()
            }],
            "tracks": [
                {"number": 1, "length_msec": 10000, "chapter_title": "Intro"},
                {"number": 2, "length_msec": 20000, "chapter_title": "Chapter One"}
            ],
            "size_bytes": zip_bytes.len()
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/cdn/part0.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
        .mount(&server)
        .await;

    let cache = tempfile::tempdir().unwrap();
    let client = LibroClient::new(server.uri()).with_token("tok");
    let plain = fetch_title_materials(&client, "9781111111111", cache.path())
        .await
        .unwrap();

    assert!(plain.m4b_path.is_none());
    assert_eq!(plain.parts.len(), 1);
    assert!(plain.parts[0].path.extension().unwrap() == "mp3");
    assert_eq!(plain.chapters.len(), 2);
    assert_eq!(plain.chapters[0].0, "Intro");
    assert_eq!(plain.chapters[0].1, 0);
    assert_eq!(plain.chapters[1].1, 10_000);
}

#[tokio::test]
async fn packaged_m4b_used_when_format_m4b_has_no_m4b_part() {
    let server = MockServer::start().await;
    let m4b_url = format!("{}/cdn/book.m4b", server.uri());

    Mock::given(method("GET"))
        .and(path(DOWNLOAD_MANIFEST_PATH))
        .and(query_param("isbn", "9782222222222"))
        .and(query_param("client_version", APP_VER))
        .and(query_param("format", "m4b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "isbn": "9782222222222",
            "parts": [],
            "tracks": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let packaged = PACKAGED_M4B_PATH.replace("{isbn}", "9782222222222");
    Mock::given(method("GET"))
        .and(path(packaged.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "m4b_url": m4b_url
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Chapters come from a ZIP-format manifest after packaged_m4b download.
    Mock::given(method("GET"))
        .and(path(DOWNLOAD_MANIFEST_PATH))
        .and(query_param("isbn", "9782222222222"))
        .and(query_param("client_version", APP_VER))
        .and(query_param_is_missing("format"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "isbn": "9782222222222",
            "parts": [],
            "tracks": [{"number": 1, "length_msec": 5000, "chapter_title": "All"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/cdn/book.m4b"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"xxxxftypM4Bfake".to_vec()))
        .mount(&server)
        .await;

    let cache = tempfile::tempdir().unwrap();
    let client = LibroClient::new(server.uri()).with_token("tok");
    let plain = fetch_title_materials(&client, "9782222222222", cache.path())
        .await
        .unwrap();

    assert!(plain.parts.is_empty());
    let m4b = plain.m4b_path.expect("m4b path");
    assert!(m4b.extension().unwrap() == "m4b");
    assert_eq!(plain.chapters[0].0, "All");
}

#[tokio::test]
async fn content_source_scan_and_fetch_title() {
    let server = MockServer::start().await;
    let zip_bytes = zip_with_mp3();

    Mock::given(method("GET"))
        .and(path(LIBRARY_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "page": 1,
            "total_pages": 1,
            "audiobooks": [sample_audiobook("9783333333333", "Scan Book")],
            "tags": []
        })))
        .mount(&server)
        .await;

    let packaged = PACKAGED_M4B_PATH.replace("{isbn}", "9783333333333");
    Mock::given(method("GET"))
        .and(path(packaged.as_str()))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(DOWNLOAD_MANIFEST_PATH))
        .and(query_param("isbn", "9783333333333"))
        .and(query_param("client_version", APP_VER))
        .and(query_param("format", "m4b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "isbn": "9783333333333",
            "parts": [],
            "tracks": []
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(DOWNLOAD_MANIFEST_PATH))
        .and(query_param("isbn", "9783333333333"))
        .and(query_param("client_version", APP_VER))
        .and(query_param_is_missing("format"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "isbn": "9783333333333",
            "parts": [{"url": format!("{}/cdn/p.zip", server.uri()), "size_bytes": 1}],
            "tracks": []
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/cdn/p.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
        .mount(&server)
        .await;

    let files = tempfile::tempdir().unwrap();
    let auth_path = libation_libro::auth_file_for_account(files.path(), None, "scan@example.com");
    save_auth(
        &auth_path,
        &LibroAuthFile {
            access_token: "tok".into(),
            token_type: "Bearer".into(),
            expires_at: None,
            email: "scan@example.com".into(),
            user_id: None,
            marketplace: "us".into(),
            label: None,
        },
    )
    .unwrap();

    let source = LibroSource::with_base_url(server.uri());
    let accounts = source.list_accounts(files.path()).await.unwrap();
    assert_eq!(accounts.len(), 1);

    let store = LibraryStore::open_in_memory().unwrap();
    let summary = source
        .scan(files.path(), &store, ScanOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.accounts, 1);
    assert_eq!(summary.books_upserted, 1);

    let cache = tempfile::tempdir().unwrap();
    let fetch = source
        .fetch_title(
            files.path(),
            "scan@example.com",
            "9783333333333",
            &libation_source::FetchOptions {
                download: Default::default(),
                cache_dir: cache.path().to_path_buf(),
            },
        )
        .await
        .unwrap();

    match fetch {
        SourceFetch::Plain(plain) => assert!(!plain.parts.is_empty()),
        SourceFetch::Encrypted(_) => panic!("expected plain fetch"),
    }
}
