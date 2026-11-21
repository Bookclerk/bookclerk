//! Tests of the notifications routes, driven through the router.

use axum::http::{header, StatusCode};
use bookclerk_library::notifications::{
    DestinationKind, FailedAttempt, NewDestination, NewNotification,
};
use bookclerk_library::{LibraryStore, NotifyQueueLimits};
use bookclerk_plugin_abi::{NotificationAlertAction, NotificationAudience, NotificationSeverity};

/// The operator token of these tests.
const TOKEN: &str = "op-token-notify";

/// Room for every job these tests enqueue.
const LIMITS: NotifyQueueLimits = NotifyQueueLimits {
    max_pending: 1_000,
    max_attempts: 3,
};

/// Sends `method uri` with `body`, as the operator or with `cookie`.
async fn call(
    app: &axum::Router,
    cookie: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let mut request = axum::http::Request::builder().method(method).uri(uri);
    request = match cookie {
        Some(cookie) => request.header(header::COOKIE, cookie),
        None => request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
    };
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
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// A channel on a notifier instance, made directly in the library (no
/// notifier plugin is installed in the harness).
async fn channel(library: &LibraryStore, label: &str) -> String {
    library
        .create_notify_destination(&NewDestination {
            plugin_instance_id: "slack-1".into(),
            kind: DestinationKind::Channel,
            user_id: None,
            label: label.into(),
            address_json: String::new(),
        })
        .await
        .expect("channel")
        .id
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

/// A patron notification for `patron`.
fn for_patron(patron: i64, title: &str) -> NewNotification {
    NewNotification {
        notification_type: "discover:new_book".into(),
        audience: NotificationAudience::Patron,
        severity: NotificationSeverity::Info,
        title: title.into(),
        text: "Out now.".into(),
        link_path: Some("/works/1".into()),
        fields: Vec::new(),
        group_key: None,
        alert_action: NotificationAlertAction::None,
        alert_key: None,
        source: "discover".into(),
        patron: Some(patron),
    }
}

#[tokio::test]
async fn only_an_administrator_reaches_the_routing_routes() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    for uri in [
        "/api/notifications/catalog",
        "/api/notifications/channels",
        "/api/notifications/routes",
        "/api/notifications/offers",
        "/api/notifications/deliveries",
    ] {
        let (status, _) = call(&app, Some(&member), "GET", uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        let (status, _) = call(&app, Some(&admin), "GET", uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    let (status, catalog) = call(&app, None, "GET", "/api/notifications/catalog", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(catalog["types"][0]["id"], "test");
    assert_eq!(catalog["types"][0]["audience"], "operational");
}

#[tokio::test]
async fn an_administrator_routes_tests_and_reads_the_log() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let ops = channel(&library, "Ops").await;

    // No notifier is installed, so a channel cannot be made over the API.
    let (status, json) = call(
        &app,
        None,
        "POST",
        "/api/notifications/channels",
        Some(r#"{"plugin_instance_id":"slack-1","label":"Ops"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");

    // Routes name known types and existing channels with a known severity.
    let body = serde_json::json!({"routes": [{"destination_id": ops, "min_severity": "warning"}]})
        .to_string();
    let (status, json) = call(
        &app,
        None,
        "PUT",
        "/api/notifications/routes/nope:unknown",
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    let (status, _) = call(
        &app,
        None,
        "PUT",
        "/api/notifications/routes/test",
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, routes) = call(&app, None, "GET", "/api/notifications/routes", None).await;
    assert_eq!(routes["routes"]["test"][0]["min_severity"], "warning");
    let bad_severity =
        serde_json::json!({"routes": [{"destination_id": ops, "min_severity": "loud"}]})
            .to_string();
    let (status, _) = call(
        &app,
        None,
        "PUT",
        "/api/notifications/routes/test",
        Some(&bad_severity),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let missing = r#"{"routes": [{"destination_id": "missing", "min_severity": "info"}]}"#;
    let (status, _) = call(
        &app,
        None,
        "PUT",
        "/api/notifications/routes/test",
        Some(missing),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // `test` is operational: patrons cannot be offered it.
    let (status, _) = call(
        &app,
        None,
        "PUT",
        "/api/notifications/offers/test",
        Some(r#"{"offers": []}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A test goes to the one channel and lands in the log.
    let mut sent_ids = Vec::new();
    for _ in 0..3 {
        let (status, sent) = call(
            &app,
            None,
            "POST",
            &format!("/api/notifications/channels/{ops}/test"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{sent}");
        sent_ids.push(
            sent["delivery_id"]
                .as_str()
                .expect("a delivery id")
                .to_string(),
        );
    }
    let (status, page) = call(
        &app,
        None,
        "GET",
        &format!("/api/notifications/deliveries?destination_id={ops}&limit=2"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["deliveries"].as_array().unwrap().len(), 2);
    assert_eq!(page["deliveries"][0]["state"], "pending");
    let cursor = page["next_cursor"].as_str().expect("a next page");
    let (_, rest) = call(
        &app,
        None,
        "GET",
        &format!("/api/notifications/deliveries?destination_id={ops}&limit=2&cursor={cursor}"),
        None,
    )
    .await;
    assert_eq!(rest["deliveries"].as_array().unwrap().len(), 1);
    assert!(rest["next_cursor"].is_null(), "{rest}");
    // Each test's delivery shows exactly once across the two pages.
    let mut listed: Vec<String> = page["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .chain(rest["deliveries"].as_array().unwrap())
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect();
    listed.sort();
    sent_ids.sort();
    assert_eq!(listed, sent_ids);
    let (status, _) = call(
        &app,
        None,
        "GET",
        "/api/notifications/deliveries?cursor=not-a-cursor",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Rename, resume and delete; a personal destination is not a channel.
    let (status, renamed) = call(
        &app,
        None,
        "PATCH",
        &format!("/api/notifications/channels/{ops}"),
        Some(r#"{"label":"Ops room"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renamed["label"], "Ops room");
    // Failures pause the channel; a paused channel refuses a test until
    // it is resumed.
    for _ in 0..bookclerk_library::notifications::PAUSE_AFTER_FAILURES {
        let (status, sent) = call(
            &app,
            None,
            "POST",
            &format!("/api/notifications/channels/{ops}/test"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        library
            .record_notify_failed(
                sent["delivery_id"].as_str().unwrap(),
                "refused",
                FailedAttempt::New,
                None,
            )
            .await
            .expect("failed");
    }
    let (status, _) = call(
        &app,
        None,
        "POST",
        &format!("/api/notifications/channels/{ops}/test"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, resumed) = call(
        &app,
        None,
        "POST",
        &format!("/api/notifications/channels/{ops}/resume"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resumed["paused_at"].is_null());
    let (status, _) = call(
        &app,
        None,
        "POST",
        &format!("/api/notifications/channels/{ops}/test"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = call(
        &app,
        None,
        "DELETE",
        &format!("/api/notifications/channels/{ops}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, log) = call(&app, None, "GET", "/api/notifications/deliveries", None).await;
    assert!(log["deliveries"].as_array().unwrap().is_empty());
    let (status, _) = call(
        &app,
        None,
        "DELETE",
        &format!("/api/notifications/channels/{ops}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_offer_for_an_operational_type_reaches_no_patron() {
    use bookclerk_library::notifications::PatronOffer;

    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    // A stored offer left behind when its type became operational (`test`
    // is the host's operational type).
    library
        .set_patron_offers(
            "test",
            &[PatronOffer {
                plugin_instance_id: "telegram-1".into(),
                default_on: true,
            }],
        )
        .await
        .expect("offer stored");
    let (status, settings) = call(&app, Some(&member), "GET", "/api/me/notifications", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(settings["types"].as_array().unwrap().is_empty());

    let catalog = super::NotificationCatalog {
        types: vec![
            super::NotificationTypeJson {
                id: "relay:new_book".into(),
                audience: NotificationAudience::Patron.wire_name().into(),
                severity: "info".into(),
                title: "New book".into(),
                source: "relay".into(),
            },
            super::NotificationTypeJson {
                id: "relay:sync_failed".into(),
                audience: NotificationAudience::Operational.wire_name().into(),
                severity: "warning".into(),
                title: "Sync failed".into(),
                source: "relay".into(),
            },
        ],
        notifiers: Vec::new(),
    };
    assert!(catalog.patron_type("relay:new_book").is_some());
    assert!(catalog.patron_type("relay:sync_failed").is_none());
    assert!(catalog.patron_type("relay:gone").is_none());
}

#[tokio::test]
async fn a_patron_reads_their_inbox_and_keeps_to_their_own_destinations() {
    let (_state, app, library) = crate::auth::tests::phase2_harness(TOKEN).await;
    let member = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let admin = crate::auth::tests::portal_cookie_for(&library, "test", "administrator-ext").await;
    let member_id = user_of(&library, "member-ext").await;
    let admin_id = user_of(&library, "administrator-ext").await;
    for title in ["One", "Two"] {
        library
            .route_notification(&for_patron(member_id, title), LIMITS)
            .await
            .expect("routed");
    }
    library
        .route_notification(&for_patron(admin_id, "Theirs"), LIMITS)
        .await
        .expect("routed");

    let (status, inbox) = call(
        &app,
        Some(&member),
        "GET",
        "/api/me/notifications/inbox",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inbox["unread"], 2);
    let entries = inbox["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e["title"] != "Theirs"));
    let first = entries[0]["id"].as_str().unwrap().to_string();

    // Another patron's notification is not in this inbox.
    let (_, theirs) = call(
        &app,
        Some(&admin),
        "GET",
        "/api/me/notifications/inbox",
        None,
    )
    .await;
    let theirs_id = theirs["entries"][0]["id"].as_str().unwrap().to_string();
    let (status, _) = call(
        &app,
        Some(&member),
        "POST",
        "/api/me/notifications/inbox/read",
        Some(&serde_json::json!({"notification_id": theirs_id}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, marked) = call(
        &app,
        Some(&member),
        "POST",
        "/api/me/notifications/inbox/read",
        Some(&serde_json::json!({"notification_id": first}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(marked["marked"], 1);
    let (_, marked) = call(
        &app,
        Some(&member),
        "POST",
        "/api/me/notifications/inbox/read",
        Some("{}"),
    )
    .await;
    assert_eq!(marked["marked"], 1);

    // The operator token alone has no inbox.
    let (status, _) = call(&app, None, "GET", "/api/me/notifications/inbox", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Someone else's destination answers like a missing one.
    let theirs = library
        .create_notify_destination(&NewDestination {
            plugin_instance_id: "telegram-1".into(),
            kind: DestinationKind::Personal,
            user_id: Some(admin_id),
            label: "Phone".into(),
            address_json: r#"{"chat_id":"1"}"#.into(),
        })
        .await
        .expect("destination")
        .id;
    let (status, _) = call(
        &app,
        Some(&member),
        "DELETE",
        &format!("/api/me/notifications/destinations/{theirs}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        Some(&member),
        "POST",
        &format!("/api/me/notifications/destinations/{theirs}/resume"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, resumed) = call(
        &app,
        Some(&admin),
        "POST",
        &format!("/api/me/notifications/destinations/{theirs}/resume"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resumed["id"], theirs.as_str());
    let (status, _) = call(
        &app,
        Some(&member),
        "PUT",
        "/api/me/notifications/choices/discover:new_book",
        Some(&serde_json::json!({"enabled": true, "destination_ids": [theirs]}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        Some(&admin),
        "DELETE",
        &format!("/api/me/notifications/destinations/{theirs}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // A choice and its removal; settings list no offered types yet.
    let (status, _) = call(
        &app,
        Some(&member),
        "PUT",
        "/api/me/notifications/choices/discover:new_book",
        Some(r#"{"enabled": false}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, settings) = call(&app, Some(&member), "GET", "/api/me/notifications", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(settings["types"].as_array().unwrap().is_empty());
    let (status, _) = call(
        &app,
        Some(&member),
        "DELETE",
        "/api/me/notifications/choices/discover:new_book",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // No notifier is installed to take an address.
    let (status, _) = call(
        &app,
        Some(&member),
        "POST",
        "/api/me/notifications/destinations",
        Some(r#"{"plugin_instance_id":"telegram-1","address":{"chat_id":"1"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[test]
fn a_notifier_answer_keeps_its_address_and_gets_a_safe_label() {
    use bookclerk_plugin_abi::notifications::AddressFieldRule;
    use bookclerk_plugin_abi::NotifyValidated;
    let rules = [AddressFieldRule {
        key: "chat_id".into(),
        required: true,
    }];
    let answer = |label: Option<&str>| NotifyValidated {
        address_json: r#"{"chat_id":"123"}"#.into(),
        label: label.map(str::to_string),
    };
    // A suggested label that is not one line gives way to the fallback
    // rather than refusing the normalised address.
    let (address, label) =
        super::accept_validated(answer(Some("two\nlines")), &rules, None, "Telegram")
            .expect("accepted");
    assert_eq!(address.to_json(), r#"{"chat_id":"123"}"#);
    assert_eq!(label, "Telegram");
    // A usable suggestion is kept; the patron's own label wins over it.
    let (_, label) =
        super::accept_validated(answer(Some("@ada")), &rules, None, "Telegram").expect("accepted");
    assert_eq!(label, "@ada");
    let (_, label) = super::accept_validated(
        answer(Some("@ada")),
        &rules,
        Some("My phone".into()),
        "Telegram",
    )
    .expect("accepted");
    assert_eq!(label, "My phone");
    // An address the declared fields refuse is still refused.
    let wrong = NotifyValidated {
        address_json: r#"{"other":"1"}"#.into(),
        label: None,
    };
    assert!(super::accept_validated(wrong, &rules, None, "Telegram").is_err());
}

#[test]
fn a_personal_address_must_fit_the_declared_fields() {
    use bookclerk_plugin_abi::notifications::AddressFieldRule;
    let rules = [
        AddressFieldRule {
            key: "chat_id".into(),
            required: true,
        },
        AddressFieldRule {
            key: "topic".into(),
            required: false,
        },
    ];
    let address = |json: serde_json::Value| {
        serde_json::from_value::<std::collections::BTreeMap<String, String>>(json)
    };
    let accepted = address(serde_json::json!({"chat_id": "1"})).expect("strings");
    assert!(super::personal_address(&accepted, &rules).is_ok());
    for refused in [
        serde_json::json!({}),
        serde_json::json!({"chat_id": "1", "other": "x"}),
    ] {
        let parsed = address(refused.clone()).expect("strings");
        assert!(
            super::personal_address(&parsed, &rules).is_err(),
            "{refused}"
        );
    }
    // Values that are not strings never reach the check: the body refuses them.
    for refused in [
        serde_json::json!({"chat_id": 1}),
        serde_json::json!({"chat_id": {"nested": "x"}}),
    ] {
        assert!(address(refused.clone()).is_err(), "{refused}");
    }
}

#[test]
fn a_personal_address_with_a_repeated_key_is_refused() {
    let body = |address: &str| {
        serde_json::from_str::<super::NewPersonalBody>(&format!(
            r#"{{"plugin_instance_id":"telegram-1","address":{address}}}"#
        ))
    };
    let err = body(r#"{"chat_id":"111","chat_id":"222"}"#).unwrap_err();
    assert!(
        err.to_string().contains("repeats the key `chat_id`"),
        "{err}"
    );
    let ok = body(r#"{"chat_id":"111","thread":"7"}"#).expect("unique keys");
    assert_eq!(ok.address.len(), 2);
}
