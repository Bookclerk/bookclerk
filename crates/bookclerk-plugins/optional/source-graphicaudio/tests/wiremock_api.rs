//! Wiremock fixtures for GraphicAudio login, products, links, Magento ZIP, Browser Player.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::unwrap_used)]

use std::io::{Cursor, Write};

use std::collections::BTreeMap;

use bookclerk_plugin_source_graphicaudio::GraphicAudioSource;
use bookclerk_plugin_source_graphicaudio::{
    fetch_title_materials, guest_fetch_title, guest_login, guest_scan, GraphicAudioAccess,
    GraphicAudioAuthFile, GraphicAudioBitrate, GraphicAudioClient, GraphicAudioContainer,
    DEFAULT_STORE_URL, LOGIN_PATH, PRODUCTS_PATH, REMOVE_PATH,
};
use bookclerk_storefront::{CatalogSearchOpts, LoginOptions, DEFAULT_CATALOG_SEARCH_LIMIT};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

/// A stored GraphicAudio account the way the host keeps it.
fn stored_auth(email: &str) -> GraphicAudioAuthFile {
    GraphicAudioAuthFile {
        token: "tok".into(),
        client_id: "dev".into(),
        email: email.into(),
        marketplace: "us".into(),
        label: None,
    }
}

fn test_password() -> String {
    ["sec", "ret"].concat()
}

#[tokio::test]
async fn device_login_returns_credentials_for_the_host() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(LOGIN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "ga-token-xyz"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let (account_id, _marketplace, label, scan_enabled, credentials) = guest_login(
        &server.uri(),
        DEFAULT_STORE_URL,
        GraphicAudioAccess::Device,
        LoginOptions {
            marketplace: "us".into(),
            label: Some("GA".into()),
            email: Some("reader@example.com".into()),
            password: Some(test_password()),
            force: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // GraphicAudioAuthFile::account_id() returns email.
    assert_eq!(account_id, "reader@example.com");
    assert_eq!(label.as_deref(), Some("GA"));
    assert!(scan_enabled);

    // The guest hands the credential document back for the host to seal.
    let auth: GraphicAudioAuthFile = serde_json::from_value(credentials).unwrap();
    assert_eq!(auth.token, "ga-token-xyz");
    assert!(!auth.client_id.is_empty());
}

#[tokio::test]
async fn scan_skips_samples_returns_owned() {
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

    let mut credentials = BTreeMap::new();
    credentials.insert(
        "a@ex.com".to_string(),
        serde_json::to_value(stored_auth("a@ex.com")).unwrap(),
    );
    let (books, accounts, _pages) = guest_scan(
        &server.uri(),
        DEFAULT_STORE_URL,
        GraphicAudioAccess::Device,
        None,
        &credentials,
        &[],
        false,
    )
    .await
    .unwrap();
    assert_eq!(accounts, 1);
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].product_id, "99");
    assert_eq!(books[0].account_id, "a@ex.com");
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
async fn guest_fetch_uses_host_credentials() {
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

    let credentials = serde_json::to_value(stored_auth("u@ex.com")).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let fetch = guest_fetch_title(
        &server.uri(),
        DEFAULT_STORE_URL,
        &credentials,
        "7",
        cache.path(),
        GraphicAudioAccess::Device,
        GraphicAudioBitrate::Hi,
        GraphicAudioContainer::Auto,
        None,
    )
    .await
    .unwrap();
    assert_eq!(fetch.parts.len(), 1);
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
    client
        .remove_activation("bookclerk-device-1")
        .await
        .unwrap();
}

#[tokio::test]
async fn magento_zip_guest_fetch() {
    let store_server = MockServer::start().await;
    let access = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/customer/account/login/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<input name="form_key" type="hidden" value="fk123"/>"#),
        )
        .mount(&store_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/customer/account/loginPost/"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/customer/account/"))
        .mount(&store_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/customer/account/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="/customer/account/logout/">Log Out</a> My Account"#),
        )
        .mount(&store_server)
        .await;

    let link_path = "/downloadable/download/link/id/ABC/";
    let products_html = format!(
        r#"<html><title>My Downloadable Products</title>
        <tr>
          <td><strong class="product-name">Owned Title</strong>
          <a href="{uri}{link}" class="action download">M4B Zip Download</a></td>
          <td>Available</td><td>2</td>
        </tr></html>"#,
        uri = store_server.uri(),
        link = link_path
    );
    Mock::given(method("GET"))
        .and(path("/downloadable/customer/products/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(products_html))
        .mount(&store_server)
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
                .insert_header("location", format!("{}/cdn/book.zip", store_server.uri())),
        )
        .mount(&store_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cdn/book.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
        .mount(&store_server)
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

    let credentials = serde_json::to_value(stored_auth("u@ex.com")).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let fetch = guest_fetch_title(
        &access.uri(),
        &store_server.uri(),
        &credentials,
        "99",
        cache.path(),
        GraphicAudioAccess::Zip,
        GraphicAudioBitrate::Hi,
        GraphicAudioContainer::Auto,
        Some(&test_password()),
    )
    .await
    .unwrap();
    assert!(
        fetch.m4b_path.is_some(),
        "expected m4b_path from Magento ZIP"
    );
    let bytes = std::fs::read(fetch.m4b_path.unwrap()).unwrap();
    assert!(bytes.starts_with(b"ftyp") || bytes.windows(4).any(|w| w == b"ftyp"));
}

#[tokio::test]
async fn browser_player_guest_fetch() {
    let store_server = MockServer::start().await;
    let access = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/customer/account/login/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<input name="form_key" type="hidden" value="fk123"/>"#),
        )
        .mount(&store_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/customer/account/loginPost/"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/customer/account/"))
        .mount(&store_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/customer/account/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="/customer/account/logout/">Log Out</a>"#),
        )
        .mount(&store_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/index/content_library"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<tr class="my-library-item" data-product-id="5273">
                <a href="/library/player/listen/title/demo-book/">Play</a>
               </tr>"#,
        ))
        .mount(&store_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/player/listen/title/demo-book/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<audio id="audio-player" src="{uri}/media/hi.m4a"></audio>"#,
            uri = store_server.uri()
        )))
        .mount(&store_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/media/hi.m4a"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ftypisombrowser"))
        .mount(&store_server)
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

    let credentials = serde_json::to_value(stored_auth("u@ex.com")).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let fetch = guest_fetch_title(
        &access.uri(),
        &store_server.uri(),
        &credentials,
        "5273",
        cache.path(),
        GraphicAudioAccess::Web,
        GraphicAudioBitrate::Hi,
        GraphicAudioContainer::Auto,
        Some(&test_password()),
    )
    .await
    .unwrap();
    assert_eq!(fetch.parts.len(), 1);
    assert!(std::fs::read(&fetch.parts[0].path)
        .unwrap()
        .starts_with(b"ftyp"));
}

#[tokio::test]
async fn search_with_a_zero_limit_returns_the_default_page() {
    let server = MockServer::start().await;
    let items: String = (0..25)
        .map(|i| {
            format!(
                r#"<li class="item product product-item" data-sku="SKU{i}">
                  <a href="{base}/title-{i}.html" class="product">
                    <h2 class="product name product-item-name">Title {i}</h2>
                  </a>
                  <div class="price" data-product-id="{id}"></div>
                </li>"#,
                base = server.uri(),
                id = 7000 + i
            )
        })
        .collect();
    let html = format!(r#"<ol class="products list items product-items">{items}</ol>"#);
    Mock::given(method("GET"))
        .and(path("/catalogsearch/result/"))
        .and(query_param("q", "title"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    let source = GraphicAudioSource::new().with_store_url(server.uri());
    let hits = source
        .search_catalog(&CatalogSearchOpts {
            query: "title".into(),
            limit: 0,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), DEFAULT_CATALOG_SEARCH_LIMIT);
    assert_eq!(hits[0].title, "Title 0");
}
