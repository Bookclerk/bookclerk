//! Wiremock fixtures for Chirp GraphQL login, library, and single audiobook.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use bookclerk_plugin_source_chirp::{
    fetch_title_materials, guest_fetch_title, guest_login, guest_scan, ChirpAuthFile, ChirpClient,
    ChirpSource,
};
use bookclerk_storefront::{
    CatalogSearchOpts, LoginOptions, PurchaseHintOpts, SourceError, DEFAULT_CATALOG_SEARCH_LIMIT,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn graphql_response(body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn op_name(req: &Request) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(&req.body).ok()?;
    v.get("operationName")
        .and_then(|o| o.as_str())
        .map(str::to_string)
}

fn test_password() -> String {
    ["sec", "ret"].concat()
}

/// Host-injected credential map for one stored Chirp account.
fn host_credentials(account_id: &str, auth: &ChirpAuthFile) -> BTreeMap<String, serde_json::Value> {
    let mut map = BTreeMap::new();
    map.insert(account_id.to_string(), serde_json::to_value(auth).unwrap());
    map
}

/// A stored Chirp account the way the host keeps it.
fn stored_auth(user_id: &str, email: &str) -> ChirpAuthFile {
    ChirpAuthFile {
        access_token: "tok".into(),
        web_token: None,
        email: email.into(),
        user_id: Some(user_id.into()),
        marketplace: "us".into(),
        label: None,
    }
}

#[tokio::test]
async fn signin_returns_credentials_for_the_host() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(|req: &Request| {
            assert_eq!(op_name(req).as_deref(), Some("signIn"));
            graphql_response(serde_json::json!({
                "data": {
                    "signIn": {
                        "user": {
                            "id": "42",
                            "token": "jwt-access",
                            "webToken": "jwt-web",
                            "email": "reader@example.com"
                        }
                    }
                }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let (account_id, _marketplace, label, scan_enabled, credentials) = guest_login(
        &server.uri(),
        LoginOptions {
            marketplace: "us".into(),
            label: Some("Chirp".into()),
            email: Some("reader@example.com".into()),
            password: Some(test_password()),
            force: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // user_id = "42" is used as account_id by ChirpAuthFile::account_id().
    assert_eq!(account_id, "42");
    assert_eq!(label.as_deref(), Some("Chirp"));
    assert!(scan_enabled);

    // The guest hands the credential document back for the host to seal.
    let auth: ChirpAuthFile = serde_json::from_value(credentials).unwrap();
    assert_eq!(auth.access_token, "jwt-access");
    assert_eq!(auth.user_id.as_deref(), Some("42"));
}

#[tokio::test]
async fn empty_library_scan_returns_no_books() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(|req: &Request| {
            assert_eq!(
                op_name(req).as_deref(),
                Some("AndroidCurrentUserAudiobooks")
            );
            graphql_response(serde_json::json!({
                "data": { "currentUserAudiobooks": [] }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let credentials = host_credentials("9", &stored_auth("9", "a@ex.com"));
    let (books, accounts, _pages) = guest_scan(&server.uri(), &credentials, &[], 0)
        .await
        .unwrap();
    assert!(books.is_empty());
    assert_eq!(accounts, 1);
}

#[tokio::test]
async fn library_scan_returns_scan_rows() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(|req: &Request| {
            assert_eq!(
                op_name(req).as_deref(),
                Some("AndroidCurrentUserAudiobooks")
            );
            graphql_response(serde_json::json!({
                "data": {
                    "currentUserAudiobooks": [{
                        "id": "ua-1",
                        "archived": false,
                        "playable": true,
                        "audiobook": {
                            "id": "ab-100",
                            "displayTitle": "Deal Book",
                            "displayAuthors": "Author A",
                            "displayNarrators": "Narrator N",
                            "durationMs": 3_600_000,
                            "abridged": false,
                            "publisher": "Pub",
                            "releasedOn": "2024-01-01"
                        }
                    }]
                }
            }))
        })
        .mount(&server)
        .await;

    let credentials = host_credentials("9", &stored_auth("9", "a@ex.com"));
    let (books, accounts, _pages) = guest_scan(&server.uri(), &credentials, &[], 0)
        .await
        .unwrap();
    assert_eq!(accounts, 1);
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].product_id, "ab-100");
    assert_eq!(books[0].account_id, "9");
}

#[tokio::test]
async fn fetch_title_downloads_tracks() {
    let server = MockServer::start().await;
    let media_url = format!("{}/t1.mp3", server.uri());
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(move |req: &Request| {
            assert_eq!(op_name(req).as_deref(), Some("AndroidSingleAudiobook"));
            graphql_response(serde_json::json!({
                "data": {
                    "audiobook": {
                        "id": "ab-1",
                        "displayTitle": "T",
                        "tracks": [{
                            "id": "tr-1",
                            "mediaUrl": media_url,
                            "chapterNumber": 1,
                            "partNumber": 1,
                            "durationMs": 1000,
                            "offsetFromBookStartMs": 0,
                            "displayName": "Chapter 1"
                        }]
                    }
                }
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/t1.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3track"))
        .mount(&server)
        .await;

    let client = ChirpClient::new(server.uri()).with_token("tok");
    let cache = tempfile::tempdir().unwrap();
    let plain = fetch_title_materials(&client, "ab-1", cache.path())
        .await
        .unwrap();
    assert_eq!(plain.parts.len(), 1);
    assert_eq!(plain.chapters.len(), 1);
}

#[tokio::test]
async fn guest_fetch_uses_host_credentials() {
    let server = MockServer::start().await;
    let media_url = format!("{}/x.mp3", server.uri());
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(move |_req: &Request| {
            graphql_response(serde_json::json!({
                "data": {
                    "audiobook": {
                        "id": "ab-2",
                        "tracks": [{
                            "id": "t",
                            "mediaUrl": media_url,
                            "durationMs": 1,
                            "offsetFromBookStartMs": 0,
                            "displayName": "One"
                        }]
                    }
                }
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/x.mp3"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3"))
        .mount(&server)
        .await;

    let credentials = serde_json::to_value(stored_auth("1", "u@ex.com")).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let fetch = guest_fetch_title(&server.uri(), &credentials, "ab-2", cache.path())
        .await
        .unwrap();
    assert_eq!(fetch.parts.len(), 1);
}

/// A price query for the Chirp product `id`.
fn priced(id: &str) -> PurchaseHintOpts {
    PurchaseHintOpts {
        product_id: Some(id.into()),
        title: Some(format!("Title {id}")),
        region: "us".into(),
        with_price: true,
        ..PurchaseHintOpts::default()
    }
}

#[tokio::test]
async fn a_page_of_titles_is_priced_in_one_aliased_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(|req: &Request| {
            assert_eq!(op_name(req).as_deref(), Some("BookclerkAudiobookPricings"));
            graphql_response(serde_json::json!({ "data": {
                "a0": { "id": "1", "currentProduct": {
                    "id": "p1", "discountPrice": "$3.99", "discountedPriceCents": 399,
                    "listingPrice": "$19.99", "isFreeListing": false
                }},
                "a1": null
            }}))
        })
        .expect(1)
        .mount(&server)
        .await;
    let hints = ChirpSource::with_graphql_url(server.uri())
        .purchase_hints(&[priced("1"), priced("2")])
        .await
        .unwrap();
    assert_eq!(hints.len(), 2);
    assert!(hints[0].as_ref().unwrap().price_label.is_some());
    assert!(hints[1].as_ref().unwrap().price_label.is_none());
}

#[tokio::test]
async fn many_titles_are_split_across_queries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(graphql_response(serde_json::json!({ "data": {} })))
        .expect(3)
        .mount(&server)
        .await;
    let opts: Vec<PurchaseHintOpts> = (0..120).map(|i| priced(&i.to_string())).collect();
    let hints = ChirpSource::with_graphql_url(server.uri())
        .purchase_hints(&opts)
        .await
        .unwrap();
    assert_eq!(hints.len(), 120);
}

#[tokio::test]
async fn a_429_reaches_the_host_with_its_wait() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&server)
        .await;
    let source = ChirpSource::with_graphql_url(server.uri());
    for result in [
        source.purchase_hints(&[priced("1")]).await.map(|_| ()),
        source.purchase_hint(&priced("1")).await.map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(SourceError::RateLimited(Some(w))) if w.as_secs() == 30),
            "{result:?}"
        );
    }
}

/// A 429 on the title lookup (typeahead) reaches the host too, and stops
/// the batch at once.
#[tokio::test]
async fn a_429_on_a_title_lookup_reaches_the_host() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "20"))
        .expect(1)
        .mount(&server)
        .await;
    let by_title = |title: &str| PurchaseHintOpts {
        title: Some(title.into()),
        region: "us".into(),
        with_price: true,
        ..PurchaseHintOpts::default()
    };
    let result = ChirpSource::with_graphql_url(server.uri())
        .purchase_hints(&[by_title("First"), by_title("Second")])
        .await;
    assert!(
        matches!(result, Err(SourceError::RateLimited(Some(w))) if w.as_secs() == 20),
        "{:?}",
        result.map(|h| h.len())
    );
}

#[tokio::test]
async fn a_failed_pricing_query_leaves_the_titles_unpriced() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let source = ChirpSource::with_graphql_url(server.uri());
    let hints = source.purchase_hints(&[priced("1")]).await.unwrap();
    assert!(hints[0].as_ref().unwrap().price_label.is_none());
    let hint = source.purchase_hint(&priced("1")).await.unwrap().unwrap();
    assert!(hint.price_label.is_none());
    // Nothing to price asks nothing.
    let unpriced = PurchaseHintOpts {
        with_price: false,
        ..priced("1")
    };
    assert_eq!(source.purchase_hints(&[unpriced]).await.unwrap().len(), 1);
}

#[tokio::test]
async fn search_with_a_zero_limit_returns_the_default_page() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(|req: &Request| {
            assert_eq!(op_name(req).as_deref(), Some("BookclerkCatalogSearch"));
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(
                body.pointer("/variables/pageSize"),
                Some(&serde_json::json!(DEFAULT_CATALOG_SEARCH_LIMIT))
            );
            let objects: Vec<serde_json::Value> = (0..25)
                .map(|i| {
                    serde_json::json!({
                        "id": format!("book-{i}"),
                        "displayTitle": format!("Title {i}"),
                    })
                })
                .collect();
            graphql_response(serde_json::json!({
                "data": { "audiobooks": { "totalCount": 25, "objects": objects } }
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    // Page 2 skips typeahead, so the single request is the catalog search.
    let hits = ChirpSource::with_graphql_url(server.uri())
        .search_catalog(&CatalogSearchOpts {
            query: "title".into(),
            limit: 0,
            page: 2,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), DEFAULT_CATALOG_SEARCH_LIMIT);
    assert_eq!(hits[0].title, "Title 0");
}
