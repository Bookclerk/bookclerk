//! Mocked Audible API tests (wiremock) for CI — no live network.

use audible_rs::api::client::Client;
use audible_rs::auth::Authenticator;
use libation_audible::{
    fetch_chapter_info, request_content_license, scan_account_into_library, summarize_license,
};
use libation_config::AudioQuality;
use libation_library::LibraryStore;
use reqwest::Url;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn synthetic_client(server: &MockServer) -> Client {
    let auth = Authenticator::from_value(serde_json::json!({
        "country_code": "us",
        "identity": {"customer_id": "amzn1.account.TEST"},
        "bearer": {
            "access_token": "Atna|synthetic",
            "refresh_token": "Atnr|synthetic",
            "expires": 9999999999.0
        }
    }))
    .unwrap();
    let url = Url::parse(&server.uri()).unwrap();
    Client::builder(auth)
        .api_base_override(url.clone())
        .auth_base_override(url)
        .build()
        .unwrap()
}

fn library_item(asin: &str, title: &str) -> serde_json::Value {
    serde_json::json!({
        "asin": asin,
        "title": title,
        "status": "Active",
        "authors": [{"name": "Test Author"}],
        "narrators": [{"name": "Test Narrator"}],
    })
}

#[tokio::test]
async fn license_grant_is_summarized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/1.0/content/B00EXAMPLE1/licenserequest"))
        .and(body_string_contains("Adrm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_license": {
                "status_code": "Granted",
                "asin": "B00EXAMPLE1",
                "drm_type": "Adrm",
                "license_response": "BASE64VOUCHER==",
                "content_metadata": {
                    "content_url": {"offline_url": "https://cds.example/x.aaxc?Policy=abc"},
                    "content_reference": {
                        "content_format": "AAX_44_64",
                        "content_size_in_bytes": 42u64
                    }
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = synthetic_client(&server);
    let license = request_content_license(&client, "us", "B00EXAMPLE1", AudioQuality::High)
        .await
        .unwrap();
    let summary = summarize_license(&license);
    assert!(summary.granted);
    assert!(summary.has_voucher);
    assert_eq!(summary.drm_type.as_deref(), Some("Adrm"));
    assert_eq!(summary.content_size, Some(42));
}

#[tokio::test]
async fn license_denial_surfaces_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/1.0/content/B00DENIED01/licenserequest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_license": {
                "status_code": "Denied",
                "asin": "B00DENIED01",
                "message": "Customer does not own this content.",
                "content_metadata": {}
            }
        })))
        .mount(&server)
        .await;

    let client = synthetic_client(&server);
    let license = request_content_license(&client, "us", "B00DENIED01", AudioQuality::Normal)
        .await
        .unwrap();
    let summary = summarize_license(&license);
    assert!(!summary.granted);
    assert_eq!(
        summary.denial_message.as_deref(),
        Some("Customer does not own this content.")
    );
}

#[tokio::test]
async fn license_000307_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/1.0/content/B00WIDEVINE/licenserequest"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error_code": "000307",
            "message": "Unable to retrieve asset details"
        })))
        .mount(&server)
        .await;

    let client = synthetic_client(&server);
    let err = request_content_license(&client, "us", "B00WIDEVINE", AudioQuality::High)
        .await
        .unwrap_err();
    assert!(err.is_no_aaxc_asset(), "expected NoAaxcAsset, got: {err}");
}

#[tokio::test]
async fn scan_account_upserts_library_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("status", "Active"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [
                library_item("B00EXAMPLE1", "First Book"),
                library_item("B00EXAMPLE2", "Second Book"),
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let library = LibraryStore::open_in_memory().unwrap();
    library
        .upsert_account("amzn1.account.TEST", "us", Some("Main"), true)
        .unwrap();

    let client = synthetic_client(&server);
    let (books, pages) = scan_account_into_library(
        &library,
        &client,
        "amzn1.account.TEST",
        "us",
        50,
        true,
        true,
    )
    .await
    .unwrap();

    assert_eq!(pages, 1);
    assert_eq!(books, 2);
    assert!(library
        .get_book("B00EXAMPLE1", "amzn1.account.TEST")
        .unwrap()
        .is_some());
    assert_eq!(
        library
            .get_book("B00EXAMPLE2", "amzn1.account.TEST")
            .unwrap()
            .unwrap()
            .title,
        "Second Book"
    );
}

#[tokio::test]
async fn chapter_metadata_is_fetched() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/content/B00EXAMPLE1/metadata"))
        .and(query_param("quality", "High"))
        .and(query_param("drm_type", "Adrm"))
        .and(query_param("chapter_titles_type", "Tree"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_metadata": {
                "chapter_info": {
                    "chapters": [
                        {"title": "Intro", "start_offset_ms": 0},
                        {"title": "Chapter 1", "start_offset_ms": 60000}
                    ]
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = synthetic_client(&server);
    let info = fetch_chapter_info(&client, "us", "B00EXAMPLE1", AudioQuality::High, "tree")
        .await
        .unwrap();
    assert_eq!(
        info.get("chapters")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(2)
    );
}
