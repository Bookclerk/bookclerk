//! Wiremock fixtures for GraphicAudio login, products, links, Magento ZIP, Browser Player.

use std::io::{Cursor, Write};

use libation_graphicaudio::{
    fetch_title_materials, load_auth, GraphicAudioAccess, GraphicAudioClient, GraphicAudioSource,
    LOGIN_PATH, PRODUCTS_PATH, REMOVE_PATH,
};
use libation_library::LibraryStore;
use libation_source::{ContentSource, LoginOptions, ScanOptions, SourceFetch, SourceKind};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

#[tokio::test]
async fn login_saves_ga_auth_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(LOGIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "ga-token-xyz"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let source =
        GraphicAudioSource::with_base_url(server.uri()).with_access(GraphicAudioAccess::Device);
    let account = source
        .login(
            dir.path(),
            LoginOptions {
                marketplace: "us".into(),
                label: Some("GA".into()),
                email: Some("reader@example.com".into()),
                password: Some("secret".into()),
                force: true,
            },
        )
        .await
        .unwrap();

    assert_eq!(account.source, SourceKind::GraphicAudio);
    assert_eq!(account.account_id, "reader@example.com");

    let auth = load_auth(&libation_graphicaudio::auth_file_for_account(
        dir.path(),
        Some("GA"),
        "reader@example.com",
    ))
    .unwrap();
    assert_eq!(auth.token, "ga-token-xyz");
    assert!(!auth.client_id.is_empty());
}

#[tokio::test]
async fn scan_skips_samples_upserts_owned() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PRODUCTS_PATH))
        .and(header("authorization", "tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "Id": "1",
                "Type": "sample",
                "ProductName": "Sample Only",
                "Author": "A"
            },
            {
                "Id": "99",
                "Type": "owned",
                "ProductName": "Owned Title",
                "Author": "Ada Author",
                "Series": "Saga",
                "Episode": "2",
                "Genre": "Sci-Fi",
                "Purchased Date": "2024-06-01"
            }
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let auth_path = libation_graphicaudio::auth_file_for_account(dir.path(), None, "a@ex.com");
    libation_graphicaudio::save_auth(
        &auth_path,
        &libation_graphicaudio::GraphicAudioAuthFile {
            token: "tok".into(),
            client_id: "dev".into(),
            email: "a@ex.com".into(),
            marketplace: "us".into(),
            label: None,
        },
    )
    .unwrap();

    let db = dir.path().join("library.db");
    let store = LibraryStore::open(&db).unwrap();
    let source =
        GraphicAudioSource::with_base_url(server.uri()).with_access(GraphicAudioAccess::Device);
    let summary = source
        .scan(dir.path(), &store, ScanOptions::default())
        .await
        .unwrap();
    assert_eq!(summary.books_upserted, 1);
    let books = store.list_books(None).unwrap();
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].product_id, "99");
    assert_eq!(books[0].source, "graphicaudio");
}

#[tokio::test]
async fn fetch_title_downloads_hi_mp3() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/links"))
        .and(query_param("product", "99"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "Lo": format!("{}/media/lo.mp3", server.uri()),
            "Hi": format!("{}/media/hi.mp3", server.uri())
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/media/hi.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3\x03hi-audio"))
        .expect(1)
        .mount(&server)
        .await;

    let client = GraphicAudioClient::new(server.uri()).with_token("tok");
    let cache = tempfile::tempdir().unwrap();
    let plain = fetch_title_materials(&client, "99", cache.path())
        .await
        .unwrap();
    assert_eq!(plain.parts.len(), 1);
    let bytes = std::fs::read(&plain.parts[0].path).unwrap();
    assert!(bytes.starts_with(b"ID3"));
}

#[tokio::test]
async fn fetch_title_via_content_source() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PRODUCTS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "Id": "7",
                "Type": "owned",
                "ProductName": "Seven"
            }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/links"))
        .and(query_param("product", "7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "Hi": format!("{}/m.mp3", server.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/m.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3x"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    libation_graphicaudio::save_auth(
        &libation_graphicaudio::auth_file_for_account(dir.path(), None, "u@ex.com"),
        &libation_graphicaudio::GraphicAudioAuthFile {
            token: "tok".into(),
            client_id: "dev".into(),
            email: "u@ex.com".into(),
            marketplace: "us".into(),
            label: None,
        },
    )
    .unwrap();

    let source =
        GraphicAudioSource::with_base_url(server.uri()).with_fetch_mode(GraphicAudioAccess::Device);
    let cache = dir.path().join("cache");
    let fetch = source
        .fetch_title(
            dir.path(),
            "u@ex.com",
            "7",
            &libation_source::FetchOptions {
                download: libation_source::DownloadOptions::default(),
                cache_dir: cache,
            },
        )
        .await
        .unwrap();
    match fetch {
        SourceFetch::Plain(p) => assert_eq!(p.parts.len(), 1),
        SourceFetch::Encrypted(_) => panic!("expected plain"),
    }
}

#[tokio::test]
async fn remove_activation_posts_client_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(REMOVE_PATH))
        .and(header("authorization", "tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = GraphicAudioClient::new(server.uri()).with_token("tok");
    client.remove_activation("libation-device-1").await.unwrap();
}

#[tokio::test]
async fn magento_zip_fetch_via_content_source() {
    let store = MockServer::start().await;
    let access = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/customer/account/login/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<input name="form_key" type="hidden" value="fk123"/>"#),
        )
        .mount(&store)
        .await;
    Mock::given(method("POST"))
        .and(path("/customer/account/loginPost/"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/customer/account/"))
        .mount(&store)
        .await;
    Mock::given(method("GET"))
        .and(path("/customer/account/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="/customer/account/logout/">Log Out</a> My Account"#),
        )
        .mount(&store)
        .await;

    let link_path = "/downloadable/download/link/id/ABC/";
    let products_html = format!(
        r#"<html><title>My Downloadable Products</title>
        <tr>
          <td><strong class="product-name">Owned Title</strong>
          <a href="{uri}{link}" class="action download">M4B Zip Download</a></td>
          <td>Available</td><td>2</td>
        </tr></html>"#,
        uri = store.uri(),
        link = link_path
    );
    Mock::given(method("GET"))
        .and(path("/downloadable/customer/products/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(products_html))
        .mount(&store)
        .await;

    let mut zip_buf = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut zip_buf);
        zip.start_file("book.m4b", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"ftypM4Bfake").unwrap();
        zip.finish().unwrap();
    }
    let zip_bytes = zip_buf.into_inner();

    Mock::given(method("GET"))
        .and(path(link_path))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/cdn/book.zip", store.uri())),
        )
        .mount(&store)
        .await;
    Mock::given(method("GET"))
        .and(path("/cdn/book.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
        .mount(&store)
        .await;

    Mock::given(method("GET"))
        .and(path(PRODUCTS_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "Id": "99",
                "Type": "owned",
                "ProductName": "Owned Title"
            }])),
        )
        .mount(&access)
        .await;

    let dir = tempfile::tempdir().unwrap();
    libation_graphicaudio::save_auth(
        &libation_graphicaudio::auth_file_for_account(dir.path(), None, "u@ex.com"),
        &libation_graphicaudio::GraphicAudioAuthFile {
            token: "tok".into(),
            client_id: "dev".into(),
            email: "u@ex.com".into(),
            marketplace: "us".into(),
            label: None,
        },
    )
    .unwrap();

    let source = GraphicAudioSource::with_base_url(access.uri())
        .with_store_url(store.uri())
        .with_fetch_mode(GraphicAudioAccess::Zip)
        .with_magento_password("secret");
    let fetch = source
        .fetch_title(
            dir.path(),
            "u@ex.com",
            "99",
            &libation_source::FetchOptions {
                download: libation_source::DownloadOptions::default(),
                cache_dir: dir.path().join("cache"),
            },
        )
        .await
        .unwrap();
    match fetch {
        SourceFetch::Plain(p) => {
            assert!(p.m4b_path.is_some(), "expected m4b_path from Magento ZIP");
            let bytes = std::fs::read(p.m4b_path.unwrap()).unwrap();
            assert!(bytes.starts_with(b"ftyp") || bytes.windows(4).any(|w| w == b"ftyp"));
        }
        SourceFetch::Encrypted(_) => panic!("expected plain"),
    }
}

#[tokio::test]
async fn browser_player_fetch_via_content_source() {
    let store = MockServer::start().await;
    let access = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/customer/account/login/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<input name="form_key" type="hidden" value="fk123"/>"#),
        )
        .mount(&store)
        .await;
    Mock::given(method("POST"))
        .and(path("/customer/account/loginPost/"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/customer/account/"))
        .mount(&store)
        .await;
    Mock::given(method("GET"))
        .and(path("/customer/account/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="/customer/account/logout/">Log Out</a>"#),
        )
        .mount(&store)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/index/content_library"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<tr class="my-library-item" data-product-id="5273">
                <a href="/library/player/listen/title/demo-book/">Play</a>
               </tr>"#,
        ))
        .mount(&store)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/player/listen/title/demo-book/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<audio id="audio-player" src="{uri}/media/hi.m4a"></audio>"#,
            uri = store.uri()
        )))
        .mount(&store)
        .await;
    Mock::given(method("GET"))
        .and(path("/media/hi.m4a"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ftypisombrowser"))
        .mount(&store)
        .await;

    Mock::given(method("GET"))
        .and(path(PRODUCTS_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "Id": "5273",
                "Type": "owned",
                "ProductName": "Demo Book"
            }])),
        )
        .mount(&access)
        .await;

    let dir = tempfile::tempdir().unwrap();
    libation_graphicaudio::save_auth(
        &libation_graphicaudio::auth_file_for_account(dir.path(), None, "u@ex.com"),
        &libation_graphicaudio::GraphicAudioAuthFile {
            token: "tok".into(),
            client_id: "dev".into(),
            email: "u@ex.com".into(),
            marketplace: "us".into(),
            label: None,
        },
    )
    .unwrap();

    let source = GraphicAudioSource::with_base_url(access.uri())
        .with_store_url(store.uri())
        .with_fetch_mode(GraphicAudioAccess::Web)
        .with_magento_password("secret");
    let fetch = source
        .fetch_title(
            dir.path(),
            "u@ex.com",
            "5273",
            &libation_source::FetchOptions {
                download: libation_source::DownloadOptions::default(),
                cache_dir: dir.path().join("cache"),
            },
        )
        .await
        .unwrap();
    match fetch {
        SourceFetch::Plain(p) => {
            assert_eq!(p.parts.len(), 1);
            assert!(std::fs::read(&p.parts[0].path)
                .unwrap()
                .starts_with(b"ftyp"));
        }
        SourceFetch::Encrypted(_) => panic!("expected plain"),
    }
}
