//! Wiremock fixtures for Libro.fm oauth, library, and download-manifest.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::unwrap_used)]

use std::io::{Cursor, Write};

use bookclerk_plugin_source_libro::{
    collect_account_books, fetch_title_materials, guest_fetch_title, guest_login, guest_scan_page,
    LibroAuthFile, LibroClient, LibroContainer, APP_VER, DOWNLOAD_MANIFEST_PATH, LIBRARY_PATH,
    PACKAGED_M4B_PATH, USER_AGENT_VALUE,
};
use bookclerk_storefront::records::{self, CreditRole, IdentifierScheme};
use bookclerk_storefront::LoginOptions;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn test_password() -> String {
    ["sec", "ret"].concat()
}

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
async fn oauth_token_login_returns_credentials_for_the_host() {
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

    let (account_id, marketplace, label, scan_enabled, credentials) = guest_login(
        &server.uri(),
        LoginOptions {
            marketplace: "us".into(),
            label: Some("Main".into()),
            email: Some("reader@example.com".into()),
            password: Some(test_password()),
            force: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // user_id is None in the Libro auth, so account_id falls back to email.
    assert_eq!(account_id, "reader@example.com");
    assert_eq!(marketplace, "us");
    assert_eq!(label.as_deref(), Some("Main"));
    assert!(scan_enabled);

    // The guest hands the credential document back for the host to seal.
    let auth: LibroAuthFile = serde_json::from_value(credentials).unwrap();
    assert_eq!(auth.access_token, "test-token-abc");
    assert!(auth.expires_at.is_some());
}

#[tokio::test]
async fn library_page_maps_libro_books_to_scan_rows() {
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

    let client = LibroClient::new(server.uri()).with_token("tok");
    let (books, pages) = collect_account_books(&client, "reader@example.com", "us")
        .await
        .unwrap();

    assert_eq!(pages, 1);
    assert_eq!(books.len(), 2);

    let found = books
        .iter()
        .find(|book| book.product_id == "9781234567890")
        .expect("string ISBN row");
    assert_eq!(found.account_id, "reader@example.com");
    assert_eq!(
        records::identifier_value(&found.identifiers, IdentifierScheme::Isbn).as_deref(),
        Some("9781234567890")
    );
    assert!(records::asin_for_marketplace(&found.identifiers, "us").is_none());
    assert_eq!(
        records::joined_names(&found.credits, CreditRole::Author).as_deref(),
        Some("Ada Author")
    );
    assert_eq!(found.length_minutes, Some(61));
    assert!(
        books.iter().any(|book| book.product_id == "9789999999999"),
        "numeric ISBNs become product ids"
    );
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
async fn guest_scan_and_fetch_title_use_host_credentials() {
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

    // The host injects each account's credential document; the guest never
    // reads a database.
    let auth = LibroAuthFile {
        access_token: "tok".into(),
        token_type: "Bearer".into(),
        expires_at: None,
        email: "scan@example.com".into(),
        user_id: None,
        marketplace: "us".into(),
        label: None,
    };
    let credentials = serde_json::to_value(&auth).unwrap();
    let params = bookclerk_plugin_sdk::ScanParams {
        account_id: "scan@example.com".into(),
        credential: bookclerk_plugin_sdk::AccountCredential {
            account_id: "scan@example.com".into(),
            credentials: serde_json::to_vec(&credentials).unwrap(),
        },
        ..bookclerk_plugin_sdk::ScanParams::default()
    };

    let page = guest_scan_page(&server.uri(), &params).await.unwrap();
    assert!(page.complete);
    assert!(page.next_cursor.is_empty());
    assert_eq!(page.store_pages, 1);
    assert_eq!(page.books.len(), 1);
    assert_eq!(page.books[0].product_id, "9783333333333");
    assert_eq!(page.books[0].account_id, "scan@example.com");

    let cache = tempfile::tempdir().unwrap();
    let fetch = guest_fetch_title(
        &server.uri(),
        &credentials,
        "9783333333333",
        cache.path(),
        LibroContainer::M4b,
    )
    .await
    .unwrap();

    assert!(!fetch.parts.is_empty());
}

#[tokio::test]
async fn scan_pages_walk_the_library_and_complete_on_the_last_page() {
    let server = MockServer::start().await;
    for (page, isbn) in [(1, "9780000000001"), (2, "9780000000002")] {
        Mock::given(method("GET"))
            .and(path(LIBRARY_PATH))
            .and(query_param("page", page.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": page,
                "total_pages": 2,
                "audiobooks": [sample_audiobook(isbn, "Paged")],
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let auth = serde_json::json!({
        "access_token": "tok",
        "token_type": "Bearer",
        "email": "paged@example.com",
        "marketplace": "us",
    });
    let params = bookclerk_plugin_sdk::ScanParams {
        account_id: "paged@example.com".into(),
        credential: bookclerk_plugin_sdk::AccountCredential {
            account_id: "paged@example.com".into(),
            credentials: serde_json::to_vec(&auth).unwrap(),
        },
        ..bookclerk_plugin_sdk::ScanParams::default()
    };

    let first = guest_scan_page(&server.uri(), &params).await.unwrap();
    assert_eq!(first.books[0].product_id, "9780000000001");
    assert_eq!(first.next_cursor, "sp1.0.2");
    assert!(!first.complete);
    let second = bookclerk_plugin_sdk::ScanParams {
        cursor: first.next_cursor.clone(),
        ..params.clone()
    };
    let last = guest_scan_page(&server.uri(), &second).await.unwrap();
    assert_eq!(last.books[0].product_id, "9780000000002");
    assert!(last.next_cursor.is_empty());
    assert!(last.complete);

    let foreign = bookclerk_plugin_sdk::ScanParams {
        account_id: "someone-else".into(),
        ..params.clone()
    };
    assert!(guest_scan_page(&server.uri(), &foreign).await.is_err());

    // The old page-number cursor of a pass in progress restarts the pass.
    let stale = bookclerk_plugin_sdk::ScanParams {
        cursor: "page:2".into(),
        ..params
    };
    let err = guest_scan_page(&server.uri(), &stale).await.unwrap_err();
    assert_eq!(
        err.code,
        bookclerk_plugin_sdk::PluginErrorCode::InvalidCursor
    );
}
