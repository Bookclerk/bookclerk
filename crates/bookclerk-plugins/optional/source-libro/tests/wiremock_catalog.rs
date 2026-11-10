//! Wiremock fixtures for the signed-out catalog calls (search, detail,
//! related titles and purchase links) against the app's `explore/search`.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used)]

use bookclerk_plugin_source_libro::APP_VER;
use bookclerk_plugin_source_libro::{explore_search_path, LibroSource, CATALOG_USER_AGENT};
use bookclerk_storefront::{
    CatalogSearchOpts, ExpandSeed, PurchaseHintOpts, DEFAULT_CATALOG_SEARCH_LIMIT,
};
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

/// Path of `explore/audiobook_details/{isbn}` beside the search path.
fn details_path(isbn: &str) -> String {
    explore_search_path().replace(
        "explore/search",
        &format!("explore/audiobook_details/{isbn}"),
    )
}

/// Mounts a details answer for `isbn`, requiring the app's AppVer header
/// and Bookclerk's own user agent.
async fn mount_details(server: &MockServer, isbn: &str, status: u16, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(details_path(isbn)))
        .and(header("x-librofm-appver", APP_VER))
        .and(header("user-agent", CATALOG_USER_AGENT))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

/// A details body for `isbn` with a price, sale flag and related rows.
fn details(
    isbn: u64,
    title: &str,
    price: &str,
    for_sale: bool,
    related: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "user_info": {"signed_in": false},
        "data": {
            "audiobook": {
                "title": title,
                "isbn": isbn,
                "authors": ["Frank Herbert"],
                "description": "<p>Desert planet.</p>",
                "publisher": "Macmillan Audio",
                "series": "Dune",
                "series_num": 1,
                "abridged": false,
                "genres": [{"name": "Science Fiction"}],
                "audiobook_info": {"narrators": ["Scott Brick"], "duration": 75720}
            },
            "related_audiobooks": related,
            "purchase_info": {"price": price, "for_sale": for_sale}
        }
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
async fn search_with_a_zero_limit_returns_the_default_page() {
    let server = MockServer::start().await;
    let rows = (0..25u64)
        .map(|i| row(9781427201400 + i, &format!("Dune {i}"), "Frank Herbert"))
        .collect();
    mount_search(&server, "dune", page(rows)).await;
    let source = LibroSource::with_base_url(server.uri());
    let hits = source
        .search_catalog(&CatalogSearchOpts {
            query: "dune".into(),
            limit: 0,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), DEFAULT_CATALOG_SEARCH_LIMIT);
    assert_eq!(hits[0].title, "Dune 0");
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

#[tokio::test]
async fn detail_prefers_the_full_details_record() {
    let server = MockServer::start().await;
    mount_details(
        &server,
        "9781427201447",
        200,
        details(9781427201447, "Dune", "29.99", true, vec![]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let hit = source
        .catalog_detail("9781427201447")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hit.origin, "libro details");
    assert_eq!(hit.description.as_deref(), Some("<p>Desert planet.</p>"));
    assert_eq!(hit.series().as_deref(), Some("Dune"));
    assert_eq!(hit.length_minutes, Some(1262));
}

#[tokio::test]
async fn detail_falls_back_to_search_when_details_fail() {
    let server = MockServer::start().await;
    mount_details(&server, "9781427201447", 500, serde_json::json!({})).await;
    mount_search(
        &server,
        "9781427201447",
        page(vec![row(9781427201447, "Dune", "Frank Herbert")]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let hit = source
        .catalog_detail("9781427201447")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hit.origin, "libro catalog");
}

#[tokio::test]
async fn expand_uses_the_apps_related_titles_for_libro_seeds() {
    let server = MockServer::start().await;
    mount_details(
        &server,
        "9781427201447",
        200,
        details(
            9781427201447,
            "Dune",
            "29.99",
            true,
            vec![
                row(9781427201447, "Dune", "Frank Herbert"),
                row(9780441013593, "Hyperion", "Dan Simmons"),
            ],
        ),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let hits = source
        .expand_candidates(&seed("libro", "9781427201447", "Dune", None), 5)
        .await
        .unwrap();
    let titles: Vec<_> = hits.iter().map(|h| h.title.as_str()).collect();
    assert_eq!(titles, ["Hyperion"]);
    assert_eq!(hits[0].origin, "libro related to “Dune”");
}

#[tokio::test]
async fn purchase_hint_carries_the_list_price_from_details() {
    let server = MockServer::start().await;
    mount_details(
        &server,
        "9781427201447",
        200,
        details(9781427201447, "Dune", "29.99", true, vec![]),
    )
    .await;
    // Not for sale: the ISBN gives no hint, so the title search decides.
    mount_details(
        &server,
        "9780000000099",
        200,
        details(9780000000099, "Dune (Region Locked)", "9.99", false, vec![]),
    )
    .await;
    mount_search(
        &server,
        "Dune",
        page(vec![row(9781427201447, "Dune", "Frank Herbert")]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());

    let priced = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("9781427201447".into()),
            with_price: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(priced.price_cents, Some(2999));
    assert_eq!(priced.list_price_label.as_deref(), Some("$29.99"));
    assert_eq!(priced.currency.as_deref(), Some("USD"));

    let unpriced = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("9781427201447".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unpriced.price_cents, None);

    let via_title = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("9780000000099".into()),
            title: Some("Dune".into()),
            with_price: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(via_title.product_id, "9781427201447");
    assert_eq!(via_title.price_cents, Some(2999));
}

#[tokio::test]
async fn purchase_hint_never_offers_an_edition_details_refused() {
    let server = MockServer::start().await;
    mount_details(
        &server,
        "9780000000099",
        200,
        details(9780000000099, "Dune (Region Locked)", "9.99", false, vec![]),
    )
    .await;
    // The title search ranks the refused edition first.
    mount_search(
        &server,
        "Dune",
        page(vec![
            row(9780000000099, "Dune (Region Locked)", "Frank Herbert"),
            row(9781427201450, "Dune (Movie Tie-In)", "Frank Herbert"),
        ]),
    )
    .await;
    mount_search(
        &server,
        "Dune Messiah",
        page(vec![row(9780000000098, "Dune Messiah", "Frank Herbert")]),
    )
    .await;
    mount_details(
        &server,
        "9780000000098",
        200,
        details(9780000000098, "Dune Messiah", "9.99", false, vec![]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());

    let next = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("9780000000099".into()),
            title: Some("Dune".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.product_id, "9781427201450");

    // A URL-only caller skips an unavailable title match for the next row.
    mount_search(
        &server,
        "Dune Messiah Frank Herbert",
        page(vec![
            row(9780000000098, "Dune Messiah", "Frank Herbert"),
            row(9780000000097, "Dune Messiah (Anniversary)", "Frank Herbert"),
        ]),
    )
    .await;
    let skipped = source
        .purchase_hint(&PurchaseHintOpts {
            title: Some("Dune Messiah".into()),
            authors: Some("Frank Herbert".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(skipped.product_id, "9780000000097");
    assert_eq!(skipped.price_cents, None);

    // A title match whose own details say it is not for sale gives no hint.
    let refused = source
        .purchase_hint(&PurchaseHintOpts {
            title: Some("Dune Messiah".into()),
            with_price: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(refused.is_none());
}

#[tokio::test]
async fn purchase_hint_falls_back_to_title_when_the_isbn_lookup_fails() {
    let server = MockServer::start().await;
    mount_details(&server, "9781427201447", 500, serde_json::json!({})).await;
    Mock::given(method("GET"))
        .and(path(explore_search_path()))
        .and(query_param("q", "9781427201447"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    mount_search(
        &server,
        "Dune",
        page(vec![row(
            9781427201450,
            "Dune (Movie Tie-In)",
            "Frank Herbert",
        )]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let hint = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("9781427201447".into()),
            title: Some("Dune".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hint.product_id, "9781427201450");
}

#[tokio::test]
async fn expand_searches_the_title_alone_without_authors() {
    let server = MockServer::start().await;
    mount_search(
        &server,
        "Dune",
        page(vec![row(9781427201447, "Dune", "Frank Herbert")]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let titled = seed("audible", "B002V1OF70", " Dune ", None);
    let hits = source.expand_candidates(&titled, 3).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].origin.starts_with("libro catalog search"));
    // No title and no author leaves nothing to search for.
    let blank = seed("audible", "B002V1OF70", "  ", None);
    assert!(source
        .expand_candidates(&blank, 3)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn purchase_hint_searches_titles_when_details_lack_the_audiobook() {
    let server = MockServer::start().await;
    // Details answer for sale but without an audiobook record.
    mount_details(
        &server,
        "9781427201447",
        200,
        serde_json::json!({"data": {"purchase_info": {"for_sale": true, "price": "9.99"}}}),
    )
    .await;
    mount_search(
        &server,
        "Dune",
        page(vec![row(
            9781427201450,
            "Dune (Movie Tie-In)",
            "Frank Herbert",
        )]),
    )
    .await;
    // No details are mounted for the title match, so the price lookup misses.
    let source = LibroSource::with_base_url(server.uri());
    let hint = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("9781427201447".into()),
            title: Some("Dune".into()),
            with_price: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hint.product_id, "9781427201450");
    assert_eq!(hint.price_cents, None);
}

#[tokio::test]
async fn purchase_hint_offers_a_title_match_without_a_valid_isbn() {
    let server = MockServer::start().await;
    mount_search(
        &server,
        "Odd",
        page(vec![serde_json::json!({"isbn": "12345", "title": "Odd"})]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let hint = source
        .purchase_hint(&PurchaseHintOpts {
            title: Some("Odd".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hint.product_id, "12345");
}

#[tokio::test]
async fn an_isbn_10_finds_its_isbn_13_search_row() {
    let server = MockServer::start().await;
    // Details miss (not mounted), so the exact-ISBN search decides.
    mount_search(
        &server,
        "0306406152",
        page(vec![row(9780306406157, "Signal Theory", "A. Author")]),
    )
    .await;
    let source = LibroSource::with_base_url(server.uri());
    let hit = source.catalog_detail("0306406152").await.unwrap().unwrap();
    assert_eq!(hit.title, "Signal Theory");
    let hint = source
        .purchase_hint(&PurchaseHintOpts {
            isbn: Some("0-306-40615-2".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hint.product_id, "9780306406157");
}
