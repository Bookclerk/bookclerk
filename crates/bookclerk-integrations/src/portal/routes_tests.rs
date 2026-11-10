//! Portal route tests: the typed success bodies (`PortalOk`, `SourceConnected`,
//! `OAuthStartUrl`) and the per-route refusals the document declares, driven
//! through the router with an in-memory library and stub sources.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use bookclerk_config::{Config, IntegrationsConfig};
use bookclerk_library::{LibraryStore, UserRole};
use bookclerk_source::{
    ContentSource, LoginOptions, OAuthProgress, PortalAuthMode, SourceAccount, SourceBrand,
    SourceError, SourceRegistry,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tower::ServiceExt;

use super::{portal_spa_router, PortalState, SESSION_COOKIE};
use crate::registry::IntegrationRegistry;
use crate::tickets::session_for_identity;

/// Brand every stub source reports.
const BRAND: SourceBrand = SourceBrand {
    id: "stub",
    name: "Stub",
    bg: "#000000",
    fg: "#ffffff",
    accent: "#111111",
    icon_url: "https://example.invalid/icon",
};

/// The login URL the OAuth stub hands the portal.
const LOGIN_URL: &str = "https://login.example/oauth?state=stub";

/// A source that signs in however the test says, without a store.
struct StubSource {
    /// Display id and alias target (`libro`, `audible`).
    id: &'static str,
    /// Provenance-qualified plugin key.
    key: &'static str,
    /// Portal sign-in mode.
    mode: PortalAuthMode,
    /// When set, every login fails with this store message.
    refusal: Option<&'static str>,
}

#[async_trait::async_trait]
impl ContentSource for StubSource {
    fn id(&self) -> &str {
        self.id
    }

    fn plugin_key(&self) -> &str {
        self.key
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        self.mode
    }

    fn portal_brand(&self) -> SourceBrand {
        BRAND
    }

    async fn login(
        &self,
        _scope: &bookclerk_library::SourceScope,
        opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        if let Some(refusal) = self.refusal {
            return Err(SourceError::auth(refusal));
        }
        Ok(SourceAccount {
            account_id: opts.email.unwrap_or_else(|| "oauth-user".into()),
            source: self.id.into(),
            marketplace: opts.marketplace,
            label: opts.label,
            scan_enabled: true,
        })
    }

    async fn login_with_oauth_progress(
        &self,
        scope: &bookclerk_library::SourceScope,
        opts: LoginOptions,
        on_progress: &(dyn Fn(OAuthProgress) + Send + Sync),
    ) -> bookclerk_source::Result<SourceAccount> {
        on_progress(OAuthProgress::LoginUrl {
            url: LOGIN_URL.into(),
            qr: None,
        });
        self.login(scope, opts).await
    }

    async fn list_accounts(
        &self,
        _scope: &bookclerk_library::SourceScope,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        Ok(Vec::new())
    }

    async fn scan(
        &self,
        _scope: &bookclerk_library::SourceScope,
        _opts: bookclerk_source::ScanOptions,
    ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
        Err(SourceError::api("the portal stub does not scan"))
    }

    async fn fetch_title(
        &self,
        _scope: &bookclerk_library::SourceScope,
        _account_id: &str,
        _title_id: &str,
        _opts: &bookclerk_source::FetchOptions,
    ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
        Err(SourceError::api("the portal stub does not fetch"))
    }
}

/// A password store (`libro`) and an OAuth store (`audible`), both signing in.
fn stores() -> Vec<Arc<dyn ContentSource>> {
    vec![
        Arc::new(StubSource {
            id: "libro",
            key: "test:source-libro",
            mode: PortalAuthMode::Password,
            refusal: None,
        }),
        Arc::new(StubSource {
            id: "audible",
            key: "test:source-audible",
            mode: PortalAuthMode::Oauth,
            refusal: None,
        }),
    ]
}

/// The portal router over an in-memory library, with a signed-in session.
///
/// # Returns
///
/// The router, the library, and the `Cookie` header value of a live session.
async fn harness(sources: Vec<Arc<dyn ContentSource>>) -> (Router, LibraryStore, String) {
    let library = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = library
        .create_user(UserRole::Member, Some("Reader"), None)
        .await
        .unwrap();
    let identity = library
        .ensure_local_portal_identity(user.id, Some("Reader"))
        .await
        .unwrap();
    let session = session_for_identity(&library, &IntegrationsConfig::default(), &identity)
        .await
        .unwrap();
    let mut registry = SourceRegistry::new();
    for source in sources {
        registry.register(source);
    }
    let state = PortalState {
        config: Arc::new(RwLock::new(Config::default())),
        library: Arc::new(RwLock::new(library.clone())),
        integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
        sources: Arc::new(RwLock::new(registry)),
        image_src: Arc::new(|url: &str| Some(format!("/signed/{url}"))),
    };
    let app = portal_spa_router(state);
    (app, library, format!("{SESSION_COOKIE}={session}"))
}

/// Sends one request and decodes its JSON body.
///
/// # Returns
///
/// The status, the response headers and the decoded body.
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

/// The credentials the password tests send.
fn credentials() -> Value {
    let password = ["pass", "word"].concat();
    json!({"email": "reader@example.com", "password": password})
}

/// Logout answers `PortalOk`, expires the cookie, and the session is gone afterwards.
#[tokio::test]
async fn logout_clears_the_cookie_and_ends_the_session() {
    let (app, _library, cookie) = harness(stores()).await;

    let (status, headers, body) = call(&app, "POST", "/logout", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ok": true}));
    let set_cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");

    let (status, _, body) = call(&app, "GET", "/me", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"error": "session expired"}));

    let (status, _, body) = call(&app, "GET", "/me", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"error": "not signed in"}));

    // Signing out without a session is still a success.
    let (status, _, body) = call(&app, "POST", "/logout", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ok": true}));
}

/// A password login answers `SourceConnected` on both routes, the link shows
/// under connections, and revoke unlinks it once.
#[tokio::test]
async fn password_login_connects_the_account_and_revoke_unlinks_it() {
    let (app, library, cookie) = harness(stores()).await;
    let connected = json!({
        "ok": true,
        "account_id": "reader@example.com",
        "source": "libro",
        "plugin_key": "test:source-libro",
    });

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/libro/login",
        Some(&cookie),
        Some(credentials()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, connected);

    // The legacy alias answers the same body.
    let (status, _, body) = call(
        &app,
        "POST",
        "/libro/login",
        Some(&cookie),
        Some(credentials()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, connected);

    let (status, _, body) = call(&app, "GET", "/connections", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["connections"].as_array().map(Vec::len), Some(1));
    // Brand logos go out through the daemon's image hook, never as the
    // plugin's remote URL.
    assert_eq!(
        body["connections"][0]["brand"]["logo"],
        "/signed/https://example.invalid/icon"
    );
    let (status, _, body) = call(&app, "GET", "/sources", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["sources"][0]["brand"]["logo"],
        "/signed/https://example.invalid/icon"
    );

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/libro/connections/reader@example.com/revoke",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"ok": true}));
    let account = library
        .list_accounts()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.account_id == "reader@example.com")
        .unwrap();
    assert_eq!(account.connection_status, "revoked");

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/libro/connections/reader@example.com/revoke",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"error": "account not linked to this identity"})
    );
}

/// A failed credential delete answers 503 and keeps the link (with scanning
/// already off), and the repeated revoke finishes the cleanup instead of
/// answering "not linked".
#[tokio::test]
async fn revoke_keeps_the_link_when_the_credential_delete_fails() {
    use sea_orm::ConnectionTrait;

    let (app, library, cookie) = harness(stores()).await;
    let revoke = "/sources/libro/connections/reader@example.com/revoke";
    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/libro/login",
        Some(&cookie),
        Some(credentials()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Hide the secrets table so the credential delete fails.
    library
        .db()
        .execute_unprepared("ALTER TABLE encrypted_secrets RENAME TO hidden_secrets")
        .await
        .unwrap();
    let (status, _, body) = call(&app, "POST", revoke, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let message = body["error"].as_str().unwrap_or_default();
    assert!(message.starts_with("revoke cleanup failed: "), "{message}");
    let account = library
        .list_accounts()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.account_id == "reader@example.com")
        .unwrap();
    // Scanning went off before the failed step; the link stays for the retry.
    assert_eq!(account.connection_status, "revoked");
    assert!(!account.scan_enabled);
    let (status, _, body) = call(&app, "GET", "/connections", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["connections"].as_array().map(Vec::len), Some(1));

    library
        .db()
        .execute_unprepared("ALTER TABLE hidden_secrets RENAME TO encrypted_secrets")
        .await
        .unwrap();
    let (status, _, body) = call(&app, "POST", revoke, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"ok": true}));
    let (status, _, body) = call(&app, "GET", "/connections", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["connections"].as_array().map(Vec::len), Some(0));
    let account = library
        .list_accounts()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.account_id == "reader@example.com")
        .unwrap();
    assert_eq!(account.connection_status, "revoked");
}

/// Each refusal a password login documents carries its exact message.
#[tokio::test]
async fn password_login_refusals_name_their_cause() {
    let mut sources = stores();
    sources.push(Arc::new(StubSource {
        id: "chirp",
        key: "test:source-chirp",
        mode: PortalAuthMode::Password,
        refusal: Some("internal: authentication error: oauth/token failed (401 Unauthorized): {\"error\":\"invalid_grant\"}"),
    }));
    let (app, _library, cookie) = harness(sources).await;

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/libro/login",
        None,
        Some(credentials()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"error": "not signed in"}));

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/nope/login",
        Some(&cookie),
        Some(credentials()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({"error": "unknown source"}));

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/audible/login",
        Some(&cookie),
        Some(credentials()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"error": "this source uses OAuth; call /oauth/start instead"})
    );

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/chirp/login",
        Some(&cookie),
        Some(credentials()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"error": "internal: authentication error: oauth/token failed (401 Unauthorized): {\"error\":\"invalid_grant\"}"})
    );
}

/// An OAuth start answers `OAuthStartUrl` on both routes and refuses a
/// password source and a signed-out caller.
#[tokio::test]
async fn oauth_start_returns_the_login_url() {
    let (app, _library, cookie) = harness(stores()).await;

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/audible/oauth/start",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"url": LOGIN_URL}));

    // The legacy alias answers the same body.
    let (status, _, body) = call(&app, "POST", "/audible/start", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"url": LOGIN_URL}));

    let (status, _, body) = call(
        &app,
        "POST",
        "/sources/libro/oauth/start",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"error": "this source uses password login; call /login instead"})
    );

    let (status, _, body) = call(&app, "POST", "/sources/audible/oauth/start", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"error": "not signed in"}));
}
