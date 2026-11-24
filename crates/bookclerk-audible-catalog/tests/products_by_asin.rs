//! `fetch_products_by_asins` against a mock catalog: the request it
//! sends, and the top-level genres it reads from each title's ladders.
// Test code: a failed assertion is the test failing, not a documented panic.
#![allow(clippy::missing_panics_doc)]

use bookclerk_audible_catalog::{fetch_products_by_asins, public_http_client};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An answer for two titles: one filed under two genres (one of them
/// twice), one with no ladders.
fn answer() -> String {
    r#"{"products":[
        {"asin":"B086WMZ9WR","title":"Cryptonomicon","category_ladders":[
            {"ladder":[{"name":"Literature & Fiction"},{"name":"Genre Fiction"}],"root":"Genres"},
            {"ladder":[{"name":"Science Fiction & Fantasy"},{"name":"Science Fiction"}],"root":"Genres"},
            {"ladder":[{"name":"Literature & Fiction"},{"name":"Historical Fiction"}],"root":"Genres"}
        ]},
        {"asin":"B019P7DVPE","title":"Calamity",
         "sample_url":"https://samples.audible.com/bk/adbl/000001/bk_adbl_000001_sample.mp3"}
    ]}"#
    .to_string()
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn titles_carry_each_top_level_genre_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/catalog/products"))
        .and(query_param("asins", "B086WMZ9WR,B019P7DVPE"))
        // `sample_url` comes only with the `sample` group, not `media`.
        .and(query_param(
            "response_groups",
            "product_attrs,product_desc,contributors,series,product_extended_attrs,media,category_ladders,sample",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(answer()))
        .expect(1)
        .mount(&server)
        .await;
    let http = public_http_client().expect("client");
    let titles =
        fetch_products_by_asins(&http, &server.uri(), &[" b086wmz9wr ", "B019P7DVPE", "x"])
            .await
            .expect("titles");
    assert_eq!(titles.len(), 2);
    assert_eq!(
        titles[0].genre_roots,
        ["Literature & Fiction", "Science Fiction & Fantasy"]
    );
    assert!(titles[1].genre_roots.is_empty());
    assert_eq!(titles[0].sample_url, None);
    assert_eq!(
        titles[1].sample_url.as_deref(),
        Some("https://samples.audible.com/bk/adbl/000001/bk_adbl_000001_sample.mp3")
    );
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn no_asin_asks_nothing() {
    let server = MockServer::start().await;
    let http = public_http_client().expect("client");
    let none = fetch_products_by_asins(&http, &server.uri(), &["not an asin"])
        .await
        .expect("no request");
    assert!(none.is_empty());
    assert!(server
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}
