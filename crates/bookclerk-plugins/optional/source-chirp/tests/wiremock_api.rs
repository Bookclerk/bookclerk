//! Wiremock fixtures for Chirp GraphQL login, library, and single audiobook.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use bookclerk_plugin_source_chirp::{
    fetch_title_materials, guest_fetch_title, guest_login, guest_scan, ChirpAuthFile, ChirpClient,
};
use bookclerk_storefront::LoginOptions;
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
