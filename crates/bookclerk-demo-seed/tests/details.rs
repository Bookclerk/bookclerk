//! The seed fills a lean search row with its storefront's details record,
//! exercised against the Libro.fm plugin and a mock catalog API.

// Test code: unwrap is fine here.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use async_trait::async_trait;
use bookclerk_demo_seed::with_details;
use bookclerk_library::SourceScope;
use bookclerk_plugin_source_libro::{explore_search_path, LibroSource as LibroCatalog};
use bookclerk_source::{
    CatalogHit, ContentSource, FetchOptions, LoginOptions, PortalAuthMode, ScanOptions,
    ScanSummary, SourceAccount, SourceBrand, SourceError, SourceFetch,
};
use tokio::time::Instant;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The Libro.fm catalog behind the host's [`ContentSource`] trait.
///
/// In the demo the seed reaches the storefront through its guest; this test
/// drives the guest's catalog code directly, and only `catalog_detail` is
/// ever called.
struct LibroSource(LibroCatalog);

impl LibroSource {
    /// Catalog client aimed at a mock API.
    fn with_base_url(base_url: String) -> Self {
        Self(LibroCatalog::with_base_url(base_url))
    }
}

/// The error every account entrypoint returns; the seed never calls them.
fn unused() -> SourceError {
    SourceError::api("not used by with_details")
}

#[async_trait]
impl ContentSource for LibroSource {
    fn id(&self) -> &str {
        "libro"
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        PortalAuthMode::Password
    }

    fn portal_brand(&self) -> SourceBrand {
        SourceBrand {
            id: "libro",
            name: "Libro.fm",
            bg: "#1F4E3D",
            fg: "#F4F1EA",
            accent: "#2F6B53",
            icon_url: "",
        }
    }

    async fn login(
        &self,
        _scope: &SourceScope,
        _opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        Err(unused())
    }

    async fn list_accounts(
        &self,
        _scope: &SourceScope,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        Err(unused())
    }

    async fn scan(
        &self,
        _scope: &SourceScope,
        _opts: ScanOptions,
    ) -> bookclerk_source::Result<ScanSummary> {
        Err(unused())
    }

    async fn fetch_title(
        &self,
        _scope: &SourceScope,
        _account_id: &str,
        _title_id: &str,
        _opts: &FetchOptions,
    ) -> bookclerk_source::Result<SourceFetch> {
        Err(unused())
    }

    async fn catalog_detail(
        &self,
        product_id: &str,
    ) -> bookclerk_source::Result<Option<CatalogHit>> {
        self.0.catalog_detail(product_id).await
    }
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
    let hit = with_details(&source, lean("9781478975311"), later()).await;
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
    let hit = with_details(&source, lean("9781478975311"), later()).await;
    assert_eq!(hit.publisher, None);
    // A product id that is not an ISBN finds nothing.
    let hit = with_details(&source, lean("not-an-isbn"), later()).await;
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
        &source,
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
    let hit = with_details(&source, lean("9781478975311"), later()).await;
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
    let hit = with_details(&source, slugged, later()).await;
    assert_eq!(hit.publisher.as_deref(), Some("Hachette Audio"));
    assert_eq!(hit.product_id, "9781478975311-circe");
}
