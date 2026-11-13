//! The seed fills a lean search row with its storefront's details record,
//! exercised against the Libro.fm plugin and a mock catalog API.

// Test code: unwrap is fine here.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use bookclerk_demo_seed::with_details;
use bookclerk_plugin_source_libro::{explore_search_path, LibroSource};
use bookclerk_source::CatalogHit;
use tokio::time::Instant;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The Libro.fm catalog-detail lookup `with_details` calls.
async fn detail(
    source: &LibroSource,
    product_id: String,
) -> bookclerk_source::Result<Option<CatalogHit>> {
    source.catalog_detail(&product_id).await
}

/// Path of `explore/audiobook_details/{isbn}` beside the search path.
fn details_path(isbn: &str) -> String {
    explore_search_path().replace(
        "explore/search",
        &format!("explore/audiobook_details/{isbn}"),
    )
}

/// A lean search row for `isbn`, as the search would return it.
fn lean(isbn: &str) -> CatalogHit {
    CatalogHit {
        product_id: isbn.into(),
        title: "Circe".into(),
        work_kind: bookclerk_source::records::WorkKind::Book,
        expression_kind: bookclerk_source::records::ExpressionKind::Narration,
        media_kind: bookclerk_source::records::MediaKind::Audio,
        ..CatalogHit::default()
    }
}

/// A details body for `isbn`.
fn details(isbn: u64) -> serde_json::Value {
    serde_json::json!({
        "data": {
            "audiobook": {
                "title": "Circe",
                "isbn": isbn,
                "authors": ["Madeline Miller"],
                "description": "<p>A bold reimagining.</p>",
                "publisher": "Hachette Audio"
            },
            "purchase_info": {"price": "33.59", "for_sale": true}
        }
    })
}

/// A deadline far enough away not to matter.
fn later() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

#[tokio::test]
async fn a_lean_hit_takes_the_full_record() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(details_path("9781478975311")))
        .respond_with(ResponseTemplate::new(200).set_body_json(details(9781478975311)))
        .mount(&server)
        .await;
    let source = LibroSource::with_base_url(server.uri());
    let hit = with_details(|id| detail(&source, id), lean("9781478975311"), later()).await;
    assert_eq!(hit.publisher.as_deref(), Some("Hachette Audio"));
    assert_eq!(hit.price_cents, Some(3359));
}

#[tokio::test]
async fn the_hit_stays_when_details_name_another_product_or_miss() {
    let server = MockServer::start().await;
    // Details for another ISBN than the one searched.
    Mock::given(method("GET"))
        .and(path(details_path("9781478975311")))
        .respond_with(ResponseTemplate::new(200).set_body_json(details(9780000000002)))
        .mount(&server)
        .await;
    let source = LibroSource::with_base_url(server.uri());
    let hit = with_details(|id| detail(&source, id), lean("9781478975311"), later()).await;
    assert_eq!(hit.publisher, None);
    // A product id that is not an ISBN finds nothing.
    let hit = with_details(|id| detail(&source, id), lean("not-an-isbn"), later()).await;
    assert_eq!(hit.product_id, "not-an-isbn");
}

#[tokio::test]
async fn the_hit_stays_when_details_fail_or_run_out_of_time() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(details_path("9781478975311")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(details(9781478975311))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let source = LibroSource::with_base_url(server.uri());
    let hit = with_details(
        |id| detail(&source, id),
        lean("9781478975311"),
        Instant::now() + Duration::from_millis(50),
    )
    .await;
    assert_eq!(hit.publisher, None);
    // Details and the search fallback both refused: the catalog call errs.
    let refusing = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&refusing)
        .await;
    let source = LibroSource::with_base_url(refusing.uri());
    let hit = with_details(|id| detail(&source, id), lean("9781478975311"), later()).await;
    assert_eq!(hit.title, "Circe");
    assert_eq!(hit.publisher, None);
}

#[tokio::test]
async fn a_slug_keyed_hit_takes_details_with_the_same_isbn() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(details_path("9781478975311")))
        .respond_with(ResponseTemplate::new(200).set_body_json(details(9781478975311)))
        .mount(&server)
        .await;
    let source = LibroSource::with_base_url(server.uri());
    let mut slugged = lean("9781478975311-circe");
    slugged.identifiers = bookclerk_source::records::identifier(
        bookclerk_source::records::IdentifierScheme::Isbn,
        "9781478975311",
        None,
    )
    .into_iter()
    .collect();
    let hit = with_details(|id| detail(&source, id), slugged, later()).await;
    assert_eq!(hit.publisher.as_deref(), Some("Hachette Audio"));
    assert_eq!(hit.product_id, "9781478975311-circe");
}
