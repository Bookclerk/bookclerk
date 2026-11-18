//! `search_similar_products_at` against a mock catalog: the request it
//! sends, and what it keeps of the answer.
// Test code: a failed assertion is the test failing, not a documented panic.
#![allow(clippy::missing_panics_doc)]

use bookclerk_audible_catalog::{public_http_client, search_similar_products_at};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An answer listing the seed itself, two neighbours and a podcast.
fn answer() -> String {
    r#"{"similar_products":[
        {"asin":"B002UP1LXS","title":"The Great Hunt"},
        {"asin":"B002UUFOSG","title":"New Spring","authors":[{"name":"Robert Jordan"}],
         "category_ladders":[{"ladder":[{"name":"Science Fiction & Fantasy"},{"name":"Fantasy"}],"root":"Genres"}],
         "thesaurus_subject_keywords":["AI_edition_inclusion"]},
        {"asin":"B09T5BPLLD","title":"A Show","content_type":"Podcast"},
        {"asin":"B009KSQJ3U","title":"Gardens of the Moon"}
    ]}"#
    .to_string()
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn similar_titles_come_in_order_without_the_seed_or_podcasts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/catalog/products/B002UP1LXS/sims"))
        .and(query_param("similarity_type", "RawSimilarities"))
        .and(query_param("num_results", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_string(answer()))
        .expect(1)
        .mount(&server)
        .await;
    let http = public_http_client().expect("client");
    let titles = search_similar_products_at(&http, &server.uri(), " b002up1lxs ", 500)
        .await
        .expect("similar titles");
    let asins: Vec<&str> = titles.iter().map(|t| t.asin.as_str()).collect();
    assert_eq!(asins, ["B002UUFOSG", "B009KSQJ3U"]);
    assert_eq!(
        titles[0].categories.as_deref(),
        Some("Science Fiction & Fantasy; Fantasy")
    );
    assert_eq!(titles[1].categories, None);
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn a_non_asin_asks_nothing_and_a_store_error_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let http = public_http_client().expect("client");
    let none = search_similar_products_at(&http, &server.uri(), "not an asin", 10)
        .await
        .expect("no request");
    assert!(none.is_empty());
    assert!(server
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
    assert!(
        search_similar_products_at(&http, &server.uri(), "B002UP1LXS", 10)
            .await
            .is_err()
    );
}
