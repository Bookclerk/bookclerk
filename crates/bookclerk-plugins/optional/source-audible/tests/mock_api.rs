//! Mocked Audible API tests (wiremock) for CI — no live network.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::unwrap_used)]

use audible_rs::api::client::Client;
use audible_rs::auth::Authenticator;
use bookclerk_plugin_source_audible::{
    collect_account_books, deregister_device, fetch_chapter_info, request_content_license,
    summarize_license, Bitrate,
};
use url::Url;
use wiremock::matchers::{body_json, body_string_contains, header, method, path, query_param};
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
    let license = request_content_license(&client, "us", "B00EXAMPLE1", Bitrate::High)
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
    let license = request_content_license(&client, "us", "B00DENIED01", Bitrate::Normal)
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
    let err = request_content_license(&client, "us", "B00WIDEVINE", Bitrate::High)
        .await
        .unwrap_err();
    assert!(err.is_no_aaxc_asset(), "expected NoAaxcAsset, got: {err}");
}

#[tokio::test]
async fn scan_account_returns_one_row_per_library_title() {
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

    let client = synthetic_client(&server);
    let (rows, pages) = collect_account_books(&client, "amzn1.account.TEST", "us", 50, true, true)
        .await
        .unwrap();

    assert_eq!(pages, 1);
    let titles: Vec<(&str, &str)> = rows
        .iter()
        .map(|row| (row.product_id.as_str(), row.title.as_str()))
        .collect();
    assert_eq!(
        titles,
        [
            ("B00EXAMPLE1", "First Book"),
            ("B00EXAMPLE2", "Second Book")
        ]
    );
    assert!(rows
        .iter()
        .all(|row| row.account_id == "amzn1.account.TEST"));
}

#[tokio::test]
async fn scan_rows_carry_structured_records() {
    use bookclerk_plugin_sdk::{
        Abridgement, CreditRole, ExpressionKind, IdentifierScheme, MediaKind, WorkKind,
    };

    let server = MockServer::start().await;
    let mut episode = library_item("B0EPISODE1", "Pilot");
    episode["content_delivery_type"] = serde_json::json!("PodcastEpisode");
    episode["relationships"] = serde_json::json!([
        {"asin": "B0SHOW", "relationship_to_product": "parent", "relationship_type": "podcast"}
    ]);
    let mut book = library_item("B0BOOK1", "Mistborn");
    book["authors"] = serde_json::json!([{"name": "Brandon Sanderson"}, {"name": " "}]);
    book["isbn"] = serde_json::json!("9781427209955");
    book["is_abridged"] = serde_json::json!(false);
    book["language"] = serde_json::json!("english");
    book["series"] = serde_json::json!([
        {"asin": "B0SERIES1", "title": "Mistborn", "sequence": "1"},
        {"asin": "B0SERIES2", "title": "Cosmere", "sequence": ""}
    ]);
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "items": [episode, book] })),
        )
        .mount(&server)
        .await;

    let client = synthetic_client(&server);
    let (rows, _) = collect_account_books(&client, "amzn1.account.TEST", "uk", 50, true, true)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);

    let episode = &rows[0];
    assert_eq!(episode.work_kind, WorkKind::PodcastEpisode);
    assert_eq!(episode.expression_kind, ExpressionKind::PodcastAudio);
    assert_eq!(episode.media_kind, MediaKind::PodcastAudio);
    assert_eq!(episode.parent_product_id.as_deref(), Some("B0SHOW"));
    assert_eq!(episode.abridgement, Abridgement::Unknown);

    let book = &rows[1];
    assert_eq!(book.work_kind, WorkKind::Book);
    assert_eq!(book.expression_kind, ExpressionKind::Narration);
    assert_eq!(book.media_kind, MediaKind::Audiobook);
    assert_eq!(book.parent_product_id, None);
    assert_eq!(book.abridgement, Abridgement::Unabridged);
    assert_eq!(book.language.as_deref(), Some("english"));
    let authors: Vec<&str> = book
        .credits
        .iter()
        .filter(|c| c.role == CreditRole::Author)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(authors, ["Brandon Sanderson"]);
    assert!(book
        .credits
        .iter()
        .any(|c| c.role == CreditRole::Narrator && c.name == "Test Narrator"));
    let asin = &book.identifiers[0];
    assert_eq!(
        (
            asin.scheme,
            asin.value.as_str(),
            asin.marketplace.as_deref()
        ),
        (IdentifierScheme::Asin, "B0BOOK1", Some("uk"))
    );
    assert_eq!(book.identifiers[1].scheme, IdentifierScheme::Isbn);
    assert_eq!(book.series_entries.len(), 2);
    assert_eq!(
        book.series_entries[0].store_series_id.as_deref(),
        Some("B0SERIES1")
    );
    assert_eq!(book.series_entries[1].position, None);
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
    let info = fetch_chapter_info(&client, "us", "B00EXAMPLE1", Bitrate::High, "tree")
        .await
        .unwrap();
    assert_eq!(
        info.get("chapters")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(2)
    );
}

#[tokio::test]
async fn revoke_deregisters_only_this_device() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/deregister"))
        .and(header("x-amz-access-token", "Atna|synthetic"))
        .and(body_json(serde_json::json!({
            "deregister_all_existing_accounts": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let client = synthetic_client(&server);
    deregister_device(&client).await.unwrap();
}

#[tokio::test]
async fn revoke_reports_a_rejected_deregistration() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/deregister"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;

    let client = synthetic_client(&server);
    assert!(deregister_device(&client).await.is_err());
}
