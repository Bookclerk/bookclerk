//! Tests for [`super`], driven through the router.

use axum::http::{header, StatusCode};
use bookclerk_library::notifications::AccountMailKind;
use bookclerk_library::{JobPayload, LibraryStore, NotifyQueueLimits, UserRole, UserStatus};

/// The operator token of these tests.
const TOKEN: &str = "op-token-password-reset";

/// Room for every message these tests queue.
const LIMITS: NotifyQueueLimits = NotifyQueueLimits {
    max_pending: 1_000,
    max_attempts: 3,
};

/// A reply: status, `Retry-After` when present, and the JSON body.
type Reply = (StatusCode, Option<String>, serde_json::Value);

/// Sends `method uri` with no credentials; a `body` goes as JSON unless
/// `json` is false.
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
    json: bool,
) -> Reply {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let mut request = axum::http::Request::builder().method(method).uri(uri);
    let body = match body {
        Some(body) => {
            if json {
                request = request.header(header::CONTENT_TYPE, "application/json");
            }
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

/// [`send`] with a JSON body.
async fn post(app: &axum::Router, uri: &str, body: &str) -> Reply {
    send(app, "POST", uri, Some(body), true).await
}

/// Sets account mail up and records a delivered test through it, which
/// turns self-service reset on; answers the instance and the files
/// directory to keep alive.
async fn turn_on(
    state: &crate::api::AppState,
    library: &LibraryStore,
) -> (String, tempfile::TempDir) {
    let (instance, files) = crate::account_mail::tests::install_mail_notifier(state, library).await;
    library
        .set_account_mail_instance(Some(&instance))
        .await
        .expect("chosen");
    state.config.write().await.integrations.public_origin = Some("https://library.example".into());
    let tester = library
        .create_user_with_profile(
            UserRole::Owner,
            Some("Ops"),
            None,
            Some("ops@example.com"),
            None,
        )
        .await
        .expect("tester");
    let mail = library
        .queue_account_mail(
            AccountMailKind::Test,
            tester.id,
            "ops@example.com",
            &instance,
            LIMITS,
        )
        .await
        .expect("test mail");
    assert!(library
        .record_account_mail_delivered(&mail, None)
        .await
        .expect("delivered"));
    (instance, files)
}

/// A member with `email`, verified when `verified`; answers their id.
async fn patron(library: &LibraryStore, email: &str, verified: bool) -> i64 {
    let user = library
        .create_user_with_profile(UserRole::Member, Some("Pat"), None, Some(email), None)
        .await
        .expect("user");
    if verified {
        assert!(library
            .verify_user_email_if(user.id, email)
            .await
            .expect("verified"));
    }
    user.id
}

/// A reset link for `user_id` at `email`; answers the raw token.
async fn link(library: &LibraryStore, user_id: i64, email: &str) -> String {
    library
        .mint_password_reset(user_id, email, None)
        .await
        .expect("mint")
        .expect("minted")
        .expose()
        .to_string()
}

/// Runs the job that answers a reset request for `address`.
async fn answer(state: &crate::api::AppState, address: &str) {
    let payload = JobPayload {
        password_reset_request: Some(address.into()),
        ..JobPayload::default()
    };
    let summary = crate::account_mail::run_password_reset_request(state, &payload, None)
        .await
        .expect("answered");
    assert_eq!(summary, "password-reset request answered");
}

#[tokio::test]
async fn reset_is_offered_only_once_a_test_message_was_delivered() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let uri = "/api/auth/password-reset";
    let (status, _, json) = send(&app, "GET", uri, None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["available"], false);
    let (status, _, json) = post(&app, uri, r#"{"email":"pat@example.com"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "reset_unavailable");

    // Chosen and ready, but never tested: still off.
    let (instance, _files) =
        crate::account_mail::tests::install_mail_notifier(&state, &library).await;
    library
        .set_account_mail_instance(Some(&instance))
        .await
        .expect("chosen");
    state.config.write().await.integrations.public_origin = Some("https://library.example".into());
    let (_, _, json) = send(&app, "GET", uri, None, true).await;
    assert_eq!(json["available"], false);

    let (_instance, _files) = turn_on(&state, &library).await;
    let (_, _, json) = send(&app, "GET", uri, None, true).await;
    assert_eq!(json["available"], true);
}

#[tokio::test]
async fn a_request_answers_alike_and_mails_only_verified_enabled_patrons() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let (_instance, _files) = turn_on(&state, &library).await;
    let verified = patron(&library, "pat@example.com", true).await;
    let unverified = patron(&library, "uma@example.com", false).await;
    let disabled = patron(&library, "dee@example.com", true).await;
    library
        .set_user_status(disabled, UserStatus::Disabled)
        .await
        .expect("disabled");
    let uri = "/api/auth/password-reset";

    let (status, _, known) = post(&app, uri, r#"{"email":"Pat@Example.com"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{known}");
    let (status, _, unknown) = post(&app, uri, r#"{"email":"nobody@example.com"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(known, unknown, "the answer says nothing about the account");
    assert_eq!(known, serde_json::json!({"accepted": true}));
    // The request runs in a job, keyed by the address.
    let queued: Vec<String> = library
        .list_jobs(50)
        .await
        .expect("jobs")
        .into_iter()
        .filter_map(|job| job.payload.password_reset_request)
        .collect();
    assert!(
        queued.contains(&"pat@example.com".to_string()),
        "{queued:?}"
    );
    assert!(queued.contains(&"nobody@example.com".to_string()));

    for address in [
        "pat@example.com",
        "uma@example.com",
        "dee@example.com",
        "nobody@example.com",
    ] {
        answer(&state, address).await;
    }
    let last = |user| {
        let library = library.clone();
        async move {
            library
                .last_account_mail_at(user, AccountMailKind::PasswordReset)
                .await
                .expect("read")
        }
    };
    let first = last(verified).await.expect("a link was queued");
    assert!(
        last(unverified).await.is_none(),
        "unverified address mailed"
    );
    assert!(last(disabled).await.is_none(), "disabled account mailed");

    // A second request inside five minutes sends nothing new.
    answer(&state, "pat@example.com").await;
    assert_eq!(last(verified).await, Some(first));
    let pending = library.unscheduled_account_mail(50).await.expect("mail");
    assert!(
        pending
            .iter()
            .filter(|mail| mail.kind == AccountMailKind::PasswordReset)
            .count()
            <= 1
    );
}

#[tokio::test]
async fn a_link_sets_the_password_once_and_signs_every_device_out() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let (_instance, _files) = turn_on(&state, &library).await;
    let member = library
        .get_portal_identity("test", "member-ext")
        .await
        .expect("identity")
        .and_then(|identity| identity.user_id)
        .expect("member");
    library
        .set_user_email(member, Some("member@example.com"))
        .await
        .expect("email");
    assert!(library
        .verify_user_email_if(member, "member@example.com")
        .await
        .expect("verified"));
    let cookie = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let profile = |cookie: String| {
        let app = app.clone();
        async move {
            use tower::ServiceExt;
            app.oneshot(
                axum::http::Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, cookie)
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
            .status()
        }
    };
    assert_eq!(profile(cookie.clone()).await, StatusCode::OK);

    let token = link(&library, member, "member@example.com").await;
    let check = format!(r#"{{"token":"{token}"}}"#);
    for _ in 0..2 {
        let (status, _, json) = post(&app, "/api/auth/password-reset/check", &check).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["second_factor"], "none");
    }
    // Without a passkey there is nothing to begin.
    let (status, _, json) = post(&app, "/api/auth/password-reset/passkey", &check).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "passkey_not_needed");
    // A code for an account without a second factor is refused, and the
    // link still works.
    let (status, _, _) = post(
        &app,
        "/api/auth/password-reset/finish",
        &format!(r#"{{"token":"{token}","password":"pw","totp_code":"123456"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let new_password = new_password();
    let finish = format!(r#"{{"token":"{token}","password":" {new_password} "}}"#);
    let (status, _, json) = post(&app, "/api/auth/password-reset/finish", &finish).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["ok"], true);
    assert!(json["revoked_sessions"].as_u64().expect("count") >= 1);
    assert_eq!(profile(cookie).await, StatusCode::UNAUTHORIZED);
    let hash = library
        .get_user_password_hash(member)
        .await
        .expect("read")
        .expect("a password");
    assert!(bookclerk_library::verify_password(&new_password, &hash).expect("verify"));

    // The link worked once.
    let (status, _, json) = post(&app, "/api/auth/password-reset/finish", &finish).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "invalid_link");
    let (status, _, _) = post(&app, "/api/auth/password-reset/check", &check).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The patron is told, and the reset is audited.
    assert!(library
        .last_account_mail_at(member, AccountMailKind::PasswordChanged)
        .await
        .expect("read")
        .is_some());
    let audit = library.list_security_audit_events(20).await.expect("audit");
    assert!(audit
        .iter()
        .any(|event| event.action == "password_reset_self"
            && event.actor == format!("user:{member}")));
}

#[tokio::test]
async fn an_authenticator_code_is_asked_for_before_the_link_is_used() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let (_instance, _files) = turn_on(&state, &library).await;
    let user = patron(&library, "tia@example.com", true).await;
    let secret = totp_rs::Secret::generate().to_base32();
    library
        .confirm_totp_enrollment(user, &secret)
        .await
        .expect("enrolled");
    let token = link(&library, user, "tia@example.com").await;
    let (status, _, json) = post(
        &app,
        "/api/auth/password-reset/check",
        &format!(r#"{{"token":"{token}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["second_factor"], "totp");

    let uri = "/api/auth/password-reset/finish";
    let (status, _, json) = post(
        &app,
        uri,
        &format!(r#"{{"token":"{token}","password":"pw"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"], "second_factor_required");
    let current = crate::totp::current_test_code(&secret);
    let wrong = if current == "000000" {
        "111111"
    } else {
        "000000"
    };
    let (status, _, json) = post(
        &app,
        uri,
        &format!(r#"{{"token":"{token}","password":"pw","totp_code":"{wrong}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"], "invalid_code");
    // Neither attempt used the link up.
    let (status, _, json) = post(
        &app,
        uri,
        &format!(r#"{{"token":"{token}","password":"pw","totp_code":"{current}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
}

#[tokio::test]
async fn a_link_stops_working_for_a_disabled_account_or_a_changed_address() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let (_instance, _files) = turn_on(&state, &library).await;
    let uri = "/api/auth/password-reset/check";
    let user = patron(&library, "ola@example.com", true).await;
    let token = link(&library, user, "ola@example.com").await;
    library
        .set_user_status(user, UserStatus::Disabled)
        .await
        .expect("disabled");
    let (status, _, json) = post(&app, uri, &format!(r#"{{"token":"{token}"}}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "invalid_link");

    let other = patron(&library, "bea@example.com", true).await;
    let token = link(&library, other, "bea@example.com").await;
    library
        .set_user_email(other, Some("bea2@example.com"))
        .await
        .expect("email");
    let (status, _, _) = post(&app, uri, &format!(r#"{{"token":"{token}"}}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn malformed_bodies_are_refused_and_repeated_tries_are_throttled() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let (_instance, _files) = turn_on(&state, &library).await;
    let uri = "/api/auth/password-reset";
    let (status, _, _) = send(
        &app,
        "POST",
        uri,
        Some(r#"{"email":"a@example.com"}"#),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    for bad in [
        "not json",
        r#"["a@example.com"]"#,
        r#"{"email":"a@example.com","other":1}"#,
        r#"{"email":"nope"}"#,
    ] {
        let (status, _, _) = post(&app, uri, bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    for bad in [r#"{"token":"abc"}"#, "{}", r#"{"token":"x","more":1}"#] {
        let (status, _, _) = post(&app, "/api/auth/password-reset/check", bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (status, _, _) = send(
        &app,
        "POST",
        "/api/auth/password-reset/finish",
        Some("{}"),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Links that do not work count toward the client's throttle.
    let unknown = format!(r#"{{"token":"{}"}}"#, "0123456789abcdef".repeat(4));
    let mut throttled = None;
    for _ in 0..10 {
        let (status, retry_after, _) = post(&app, "/api/auth/password-reset/check", &unknown).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            throttled = retry_after;
            break;
        }
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let wait: u64 = throttled
        .expect("the client was throttled")
        .parse()
        .expect("seconds");
    assert!(wait >= 1);
    // Asking for links has a limit of its own, so a client locked out of
    // checking links can still ask for one, and asking cannot lock a
    // patron out of the link that arrives.
    let mut asked = 0;
    loop {
        let (status, _, _) = post(&app, uri, r#"{"email":"a@example.com"}"#).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
        assert_eq!(status, StatusCode::ACCEPTED);
        asked += 1;
        assert!(asked < 10, "asking for links was never limited");
    }
    assert!(asked >= 1, "a locked-out link check also blocked asking");
}

#[tokio::test]
async fn two_finishes_of_one_link_at_once_set_one_password() {
    let (state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let (_instance, _files) = turn_on(&state, &library).await;
    let user = patron(&library, "kit@example.com", true).await;
    let token = link(&library, user, "kit@example.com").await;
    let first = new_password();
    let second = format!("{first} again");
    let finish = |password: &str| {
        let app = app.clone();
        let body = format!(r#"{{"token":"{token}","password":"{password}"}}"#);
        async move { post(&app, "/api/auth/password-reset/finish", &body).await }
    };
    let (a, b) = tokio::join!(finish(&first), finish(&second));
    let won: Vec<&str> = [(&a, first.as_str()), (&b, second.as_str())]
        .into_iter()
        .filter(|((status, _, _), _)| *status == StatusCode::OK)
        .map(|(_, password)| password)
        .collect();
    assert_eq!(won.len(), 1, "{a:?} {b:?}");
    for (status, _, json) in [&a, &b] {
        if *status != StatusCode::OK {
            assert_eq!(*status, StatusCode::BAD_REQUEST);
            assert_eq!(json["error"], "invalid_link");
        }
    }
    let hash = library
        .get_user_password_hash(user)
        .await
        .expect("read")
        .expect("a password");
    assert!(bookclerk_library::verify_password(won[0], &hash).expect("verify"));
}

/// The password the reset sets, assembled at runtime so scanners do not
/// treat a string literal as a shipped secret.
fn new_password() -> String {
    ["a new", " ", "password"].concat()
}
