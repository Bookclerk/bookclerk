//! Wiremock fixtures for the signed-out catalog calls (search, detail,
//! related titles and purchase links) against the app's `explore/search`.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used)]

use bookclerk_plugin_source_libro::{explore_search_path, LibroSource, CATALOG_USER_AGENT};
use bookclerk_source::{CatalogSearchOpts, ContentSource, ExpandSeed, PurchaseHintOpts};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A seed with every optional field empty.
fn seed(source: &str, product_id: &str, title: &str, authors: Option<&str>) -> ExpandSeed {
    ExpandSeed {
        source: source.into(),
        product_id: product_id.into(),
        title: title.into(),
        authors: authors.map(str::to_string),
        narrators: None,
        series: None,
        series_asin: None,
        asin: None,
        isbn: None,
        region: String::new(),
    }
}

/// One search row shaped like the live signed-out API.
fn row(isbn: u64, title: &str, author: &str) -> serde_json::Value {
    serde_json::json!({
        "title": title,
        "isbn": isbn,
        "authors": [author],
        "cover_url": format!("//covers.libro.fm/{isbn}_1120.jpg"),
        "catalog_info": {"bookseller_pick": false, "new_release": false, "coming_soon": false},
        "audiobook_info": {"narrators": ["Nia Narrator"]}
    })
}

/// A search page envelope around `rows`.
fn page(rows: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "user_info": {"signed_in": false},
        "audiobook_collection": {"page": 1, "total_pages": 1, "audiobooks": rows}
    })
}

/// Mounts a search answer for exactly `q`, requiring Bookclerk's own user agent.
async fn mount_search(server: &MockServer, q: &str, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(explore_search_path()))
        .and(query_param("q", q))
        .and(header("user-agent", CATALOG_USER_AGENT))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn search_uses_the_app_api_with_an_honest_user_agent() {
    let server = MockServer::start().await;
    mount_search(
        &server,
        "dune",
        page(vec![
            row(9781427201447, "Dune", "Frank Herbert"),
            row(9781427201448, "Dune Messiah", "Frank Herbert"),
        ]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let hits = source
        .search_catalog(&CatalogSearchOpts {
            query: "dune".into(),
            limit: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Dune");
    assert_eq!(hits[0].origin, "search");
    assert_eq!(
        hits[0].cover_url.as_deref(),
        Some("https://covers.libro.fm/9781427201447_1120.jpg")
    );
}

#[tokio::test]
async fn search_reports_a_refusal_instead_of_scraping_html() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(explore_search_path()))
        .respond_with(ResponseTemplate::new(403).set_body_string("<html>403</html>"))
        .mount(&server)
        .await;
    let source = LibroSource::with_base_url(server.uri());
    let err = source
        .search_catalog(&CatalogSearchOpts {
            query: "dune".into(),
            limit: 5,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("403"), "{err}");
    // An empty query never reaches the network.
    let none = source
        .search_catalog(&CatalogSearchOpts {
            query: "  ".into(),
            limit: 5,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn detail_keeps_only_the_exact_isbn() {
    let server = MockServer::start().await;
    mount_search(
        &server,
        "9781427201447",
        page(vec![
            row(9780000000001, "Something Else", "Other"),
            row(9781427201447, "Dune", "Frank Herbert"),
        ]),
    )
    .await;
    mount_search(
        &server,
        "9780000000099",
        page(vec![row(9780000000001, "Fuzzy", "Other")]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let hit = source
        .catalog_detail("9781427201447-dune")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hit.title, "Dune");
    assert_eq!(hit.narrators().as_deref(), Some("Nia Narrator"));
    assert!(source
        .catalog_detail("9780000000099")
        .await
        .unwrap()
        .is_none());
    assert!(source.catalog_detail("B0ASIN0000").await.unwrap().is_none());
}

#[tokio::test]
async fn expand_finds_more_by_the_author_without_the_seed() {
    let server = MockServer::start().await;
    mount_search(
        &server,
        "Frank Herbert",
        page(vec![
            row(9781427201447, "Dune", "Frank Herbert"),
            row(9781427201448, "Dune Messiah", "Frank Herbert"),
            row(9781427201449, "Children of Dune", "Frank Herbert"),
        ]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let seed = seed(
        "libro",
        "9781427201447",
        "Dune",
        Some("Frank Herbert, Brian Herbert"),
    );
    let hits = source.expand_candidates(&seed, 2).await.unwrap();
    let titles: Vec<_> = hits.iter().map(|h| h.title.as_str()).collect();
    assert_eq!(titles, ["Dune Messiah", "Children of Dune"]);
    assert_eq!(hits[0].origin, "libro more by Frank Herbert");
    // A Libro seed without authors has nothing to search for.
    let bare = ExpandSeed {
        authors: None,
        ..seed
    };
    assert!(source.expand_candidates(&bare, 2).await.unwrap().is_empty());
}

#[tokio::test]
async fn expand_searches_title_and_author_for_other_stores() {
    let server = MockServer::start().await;
    mount_search(
        &server,
        "Dune Frank Herbert",
        page(vec![row(9781427201447, "Dune", "Frank Herbert")]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let seed = seed("audible", "B002V1OF70", "Dune", Some("Frank Herbert"));
    let hits = source.expand_candidates(&seed, 3).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].origin.starts_with("libro catalog search"));
}

#[tokio::test]
async fn purchase_hint_prefers_the_isbn_then_falls_back_to_title() {
    let server = MockServer::start().await;
    mount_search(
        &server,
        "9781427201447",
        page(vec![row(9781427201447, "Dune", "Frank Herbert")]),
    )
    .await;
    // An ISBN Libro.fm does not sell, then a title search that finds a copy.
    mount_search(&server, "9780000000099", page(vec![])).await;
    mount_search(
        &server,
        "Dune Frank Herbert",
        page(vec![row(
            9781427201450,
            "Dune (Movie Tie-In)",
            "Frank Herbert",
        )]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());

    let by_isbn = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("9781427201447".into()),
            title: Some("Dune".into()),
            with_price: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_isbn.product_id, "9781427201447");
    assert_eq!(
        by_isbn.url.as_deref(),
        Some("https://libro.fm/audiobooks/9781427201447")
    );
    assert_eq!(by_isbn.price_cents, None);

    let by_title = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("9780000000099".into()),
            title: Some("Dune".into()),
            authors: Some("Frank Herbert".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_title.product_id, "9781427201450");
    assert_eq!(by_title.title.as_deref(), Some("Dune (Movie Tie-In)"));

    // An ASIN is never treated as an ISBN, and no title means no search.
    let asin_only = source
        .purchase_hint(&PurchaseHintOpts {
            product_id: Some("B002V1OF70".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(asin_only.is_none());
}

#[tokio::test]
async fn purchase_hint_treats_a_failed_title_search_as_no_match() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(explore_search_path()))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let source = LibroSource::with_base_url(server.uri());
    let hint = source
        .purchase_hint(&PurchaseHintOpts {
            title: Some("Dune".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(hint.is_none());
}
