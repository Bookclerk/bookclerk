//! Tests of the account-mail routes, driven through the router.

use axum::http::{header, StatusCode};
use bookclerk_library::notifications::AccountMailKind;
use bookclerk_library::{LibraryStore, NotifyQueueLimits, UserRole};

/// The operator token of these tests.
const TOKEN: &str = "op-token-account-mail";

/// The public origin [`set_origin`] gives the library.
const ORIGIN: &str = "https://library.example";

/// A reply: status, `Retry-After` when present, and the JSON body.
type Reply = (StatusCode, Option<String>, serde_json::Value);

/// Sends `method uri` with `body`, as the operator or with `cookie`.
async fn call(
    app: &axum::Router,
    cookie: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> Reply {
    call_from(app, cookie, None, method, uri, body).await
}

/// [`call`] from a page at `origin`, as a browser sends it once the
/// library has a public origin.
async fn call_from(
    app: &axum::Router,
    cookie: Option<&str>,
    origin: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> Reply {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let mut request = axum::http::Request::builder().method(method).uri(uri);
    request = match cookie {
        Some(cookie) => request.header(header::COOKIE, cookie),
        None => request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
    };
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(body.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, retry_after, json)
}

/// The user behind the harness identity `external`.
async fn user_of(library: &LibraryStore, external: &str) -> i64 {
    library
        .get_portal_identity("test", external)
        .await
        .expect("identity read")
        .and_then(|identity| identity.user_id)
        .expect("a seeded user")
}

/// Installs a notifier plugin that sends account mail under a temporary
/// files directory and makes one instance of it.
///
/// # Returns
///
/// The instance id and the directory, which must outlive the test.
pub(crate) async fn install_mail_notifier(
    state: &crate::api::AppState,
    library: &LibraryStore,
) -> (String, tempfile::TempDir) {
    let files = tempfile::tempdir().expect("files dir");
    let plugin = files.path().join("plugins").join("mailer");
    std::fs::create_dir_all(&plugin).expect("plugin dir");
    std::fs::write(
        plugin.join("plugin.toml"),
        r#"api_version = 3
id = "mailer"
name = "Mailer"
version = "0.1.0"
runtime = "native"
command = "./mailer"
entrypoints = ["notifier"]

[notifier]
destinations = ["personal"]
account_mail = true

[[notifier.address_fields]]
key = "email"
label = "Email address"
required = true

[capabilities.network]
mode = "outbound"

[vars]

[secrets]
"#,
    )
    .expect("manifest");
    std::fs::write(plugin.join("mailer"), b"#!/bin/sh\n").expect("command");
    state.config.write().await.paths = Some(bookclerk_config::Paths::from_files_dir(
        files.path().to_path_buf(),
    ));
    let config = state.config.read().await.clone();
    let found = bookclerk_plugin_host::discover_plugins(&config).expect("discovery");
    let key = found
        .iter()
        .find(|p| p.manifest.id == "mailer")
        .expect("the mailer is discovered")
        .plugin_key()
        .canonical()
        .to_string();
    let instance = bookclerk_library::control_plane::create_plugin_instance(
        library,
        &bookclerk_library::control_plane::ConfigActor::Bootstrap,
        &key,
        "Mail",
    )
    .await
    .expect("instance");
    (instance.id.as_str().to_string(), files)
}

/// Sets the library's public origin.
async fn set_origin(state: &crate::api::AppState) {
    state.config.write().await.integrations.public_origin = Some(ORIGIN.into());
}

/// Chooses `instance` as the account-mail notifier over the API.
async fn choose(app: &axum::Router, instance: &str) -> serde_json::Value {
    let (status, _, json) = call(
        app,
        None,
        "PUT",
        "/api/notifications/account-mail",
        Some(&format!(r#"{{"plugin_instance_id":"{instance}"}}"#)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
}

#[tokio::test]
async fn an_administrator_chooses_a_notifier_that_sends_account_mail() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    let uri = "/api/notifications/account-mail";

    let (status, _, _) = call(&app, Some(&member), "GET", uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, json) = call(&app, Some(&admin), "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ready"], false);
    assert_eq!(json["not_ready"], "not_configured");

    // A malformed body and an instance that sends no account mail.
    let (status, _, _) = call(&app, None, "PUT", uri, Some(r#"{"other":1}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = call(&app, None, "PUT", uri, Some(r#"["slack-1"]"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = call(&app, None, "PUT", uri, Some("{}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, json) = call(
        &app,
        None,
        "PUT",
        uri,
        Some(r#"{"plugin_instance_id":"slack-1"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "that notifier does not send account mail");

    let (instance, _files) = install_mail_notifier(&state, &library).await;
    let json = choose(&app, &instance).await;
    assert_eq!(json["plugin_instance_id"], instance.as_str());
    assert_eq!(json["not_ready"], "no_public_origin");
    set_origin(&state).await;
    let (_, _, json) = call(&app, None, "GET", uri, None).await;
    assert_eq!(json["ready"], true);
    assert!(json["not_ready"].is_null());

    // An instance whose plugin is gone is no longer ready.
    library
        .set_account_mail_instance(Some("gone-1"))
        .await
        .expect("set");
    let (_, _, json) = call(&app, None, "GET", uri, None).await;
    assert_eq!(json["not_ready"], "notifier_missing");

    let (status, _, json) = call(
        &app,
        None,
        "PUT",
        uri,
        Some(r#"{"plugin_instance_id":null}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["not_ready"], "not_configured");
    assert!(library.account_mail_settings().await.unwrap().is_none());
}

#[tokio::test]
async fn a_test_goes_to_the_callers_own_address() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    let admin_id = user_of(&library, "administrator-ext").await;
    let uri = "/api/notifications/account-mail/test";

    // The operator has no address of its own.
    let (status, _, _) = call(&app, None, "POST", uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, json) = call(&app, Some(&admin), "POST", uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "add an email address to your profile first");

    // Adding an address before account mail is set up queues nothing.
    let (status, _, json) = call(
        &app,
        Some(&admin),
        "PATCH",
        "/api/auth/profile",
        Some(r#"{"email":"admin@example.com"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(library
        .last_account_mail_at(admin_id, AccountMailKind::VerifyEmail)
        .await
        .unwrap()
        .is_none());
    let (status, _, json) = call(&app, Some(&admin), "POST", uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        json["error"],
        "account mail is not set up; choose a notifier first"
    );

    // A test carries no link, so it needs no public origin.
    let (instance, _files) = install_mail_notifier(&state, &library).await;
    choose(&app, &instance).await;
    let (status, _, json) = call(&app, Some(&admin), "POST", uri, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    let mail = library
        .account_mail(json["mail_id"].as_str().expect("mail id"))
        .await
        .unwrap()
        .expect("mail");
    assert_eq!(mail.kind, AccountMailKind::Test);
    assert_eq!(mail.address, "admin@example.com");
    assert_eq!(mail.plugin_instance_id, instance);
}

#[tokio::test]
async fn a_patron_asks_for_a_verification_link_once_every_five_minutes() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let member_id = user_of(&library, "member-ext").await;
    let uri = "/api/auth/profile/email/verification";

    let (status, _, _) = call(&app, None, "POST", uri, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the operator is no patron"
    );
    let (status, _, json) = call(&app, Some(&member), "POST", uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "there is no address to verify");

    library
        .set_user_email(member_id, Some("member@example.com"))
        .await
        .expect("email");
    let (status, _, json) = call(&app, Some(&member), "POST", uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        json["error"],
        "account mail is not set up; choose a notifier first"
    );

    let (instance, _files) = install_mail_notifier(&state, &library).await;
    choose(&app, &instance).await;
    let (status, _, json) = call(&app, Some(&member), "POST", uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "set the library's public address first");

    set_origin(&state).await;
    let (status, _, json) = call_from(&app, Some(&member), Some(ORIGIN), "POST", uri, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    let (status, retry_after, _) =
        call_from(&app, Some(&member), Some(ORIGIN), "POST", uri, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let wait: i64 = retry_after.expect("Retry-After").parse().expect("seconds");
    assert!((1..=300).contains(&wait), "{wait}");

    // A new address queues its own link.
    let (status, _, json) = call_from(
        &app,
        Some(&member),
        Some(ORIGIN),
        "PATCH",
        "/api/auth/profile",
        Some(r#"{"email":"member2@example.com"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(library
        .unscheduled_account_mail(10)
        .await
        .unwrap()
        .is_empty());

    library
        .verify_user_email_if(member_id, "member2@example.com")
        .await
        .expect("verified");
    let (status, _, json) = call_from(&app, Some(&member), Some(ORIGIN), "POST", uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "your email address is already verified");
}

#[tokio::test]
async fn a_verification_link_works_without_signing_in() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let member_id = user_of(&library, "member-ext").await;
    library
        .set_user_email(member_id, Some("member@example.com"))
        .await
        .expect("email");
    let uri = "/api/auth/verify-email";
    let public = |body: String| {
        let app = app.clone();
        async move {
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body))
                .expect("request");
            app.oneshot(request).await.expect("response").status()
        }
    };
    assert_eq!(
        public(r#"{"token":"nope"}"#.to_string()).await,
        StatusCode::BAD_REQUEST
    );
    let unknown = format!(r#"{{"token":"{}"}}"#, "a".repeat(64));
    assert_eq!(public(unknown).await, StatusCode::BAD_REQUEST);
    // The extractor still reads an object only, with no other keys.
    let positional = format!(r#"["{}"]"#, "a".repeat(64));
    assert_eq!(public(positional).await, StatusCode::BAD_REQUEST);
    let extra = format!(r#"{{"token":"{}","next":"/"}}"#, "a".repeat(64));
    assert_eq!(public(extra).await, StatusCode::BAD_REQUEST);
    {
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .body(axum::body::Body::from(format!(
                r#"{{"token":"{}"}}"#,
                "a".repeat(64)
            )))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    let token = library
        .mint_email_verification(member_id, "member@example.com", None)
        .await
        .expect("token")
        .expect("minted");
    let body = format!(r#"{{"token":"{}"}}"#, token.expose());
    assert_eq!(public(body.clone()).await, StatusCode::OK);
    assert!(
        library
            .get_user(member_id)
            .await
            .unwrap()
            .unwrap()
            .email_verified
    );
    // It works once.
    assert_eq!(public(body).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_administrator_mails_invites_once_account_mail_is_ready() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let quiet = library
        .create_user(UserRole::Member, Some("No address"), None)
        .await
        .expect("user");

    // Before account mail is ready the invite comes back as a link.
    let (status, _, json) = call(
        &app,
        None,
        "POST",
        "/api/users",
        Some(r#"{"email":"early@example.com"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["invite_emailed"], false);
    assert!(json["invite_url"].is_string());
    let early = json["user"]["id"].as_i64().expect("id");
    let (status, _, json) = call(
        &app,
        None,
        "POST",
        &format!("/api/users/{early}/invite-email"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        json["error"],
        "account mail is not set up; choose a notifier first"
    );

    let (instance, _files) = install_mail_notifier(&state, &library).await;
    choose(&app, &instance).await;
    set_origin(&state).await;

    let (status, _, json) = call(
        &app,
        None,
        "POST",
        "/api/users",
        Some(r#"{"email":"sam@example.com"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["invite_emailed"], true);
    assert!(json["invite_url"].is_null());
    assert!(json["claim_ticket"].is_null());
    let sam = json["user"]["id"].as_i64().expect("id");
    assert_eq!(json["user"]["email_verified"], false);
    assert!(library
        .last_account_mail_at(sam, AccountMailKind::Invite)
        .await
        .unwrap()
        .is_some());

    let (status, _, json) = call(
        &app,
        None,
        "POST",
        &format!("/api/users/{early}/invite-email"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    assert!(json["mail_id"].is_string());
    let (status, _, json) = call(
        &app,
        None,
        "POST",
        &format!("/api/users/{}/invite-email", quiet.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "the user has no email address");
    let (status, _, _) = call(&app, None, "POST", "/api/users/999999/invite-email", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An Administrator changing an address queues a link to the new one.
    let (status, _, json) = call(
        &app,
        None,
        "PATCH",
        &format!("/api/users/{}", quiet.id),
        Some(r#"{"email":"quiet@example.com"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(library
        .last_account_mail_at(quiet.id, AccountMailKind::VerifyEmail)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn the_sweep_enqueues_mail_that_found_the_queue_full() {
    let (state, _app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let user = library
        .create_user(UserRole::Member, Some("Late"), None)
        .await
        .expect("user");
    let full = NotifyQueueLimits {
        max_pending: 0,
        max_attempts: 3,
    };
    let id = library
        .queue_account_mail(
            AccountMailKind::Test,
            user.id,
            "late@example.com",
            "mail-1",
            full,
        )
        .await
        .expect("queued");
    assert_eq!(library.unscheduled_account_mail(10).await.unwrap().len(), 1);
    super::sweep_once(&state, &library, 10)
        .await
        .expect("sweep");
    assert!(library
        .unscheduled_account_mail(10)
        .await
        .unwrap()
        .is_empty());
    let mail = library.account_mail(&id).await.unwrap().expect("mail");
    assert_eq!(
        mail.state,
        bookclerk_library::notifications::DeliveryState::Pending
    );
}
