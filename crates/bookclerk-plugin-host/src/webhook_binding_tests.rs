//! The `WEBHOOKS` binding over a real library store.

use super::*;
use bookclerk_library::control_plane::{
    find_webhook, store_webhook_request, WebhookRequest, WebhookStoreOutcome, WebhookUrlToken,
};
use bookclerk_plugin_abi::{PluginErrorCode, WebhookHeader};

/// A store with the host schema and a master key that seals URL tokens.
async fn memory_store() -> LibraryStore {
    let dir = tempfile::tempdir().expect("tempdir");
    bookclerk_library::configure_master_key(dir.path()).expect("master key");
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .expect("memory store");
    bookclerk_library::apply_host_schema(&db)
        .await
        .expect("host schema");
    LibraryStore::from_connection(db)
}

fn manifest(webhooks: &str) -> PluginManifest {
    PluginManifest::parse(&format!(
        r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
{webhooks}"#
    ))
    .expect("manifest")
}

const EVENTS: &str = r#"
[[webhooks]]
name = "events"
[webhooks.signature]
scheme = "none"
"#;

fn grant(bindings: &[&str]) -> PluginGrant {
    PluginGrant {
        bindings: bindings.iter().map(|s| (*s).to_string()).collect(),
        ..PluginGrant::empty()
    }
}

#[tokio::test]
async fn the_binding_follows_manifest_grant_and_session() {
    let _dek = crate::PROCESS_DEK_TEST_LOCK.lock().await;
    let store = memory_store().await;
    let instance = PluginInstanceId::mint();
    let declared = manifest(EVENTS);
    let granted_all = grant(&[WEBHOOKS_GRANT_BINDING]);
    assert!(granted(
        Some(store.clone()),
        Some(instance.clone()),
        &manifest(""),
        &granted_all
    )
    .is_none());
    assert!(granted(
        Some(store.clone()),
        Some(instance.clone()),
        &declared,
        &grant(&[])
    )
    .is_none());
    assert!(granted(None, Some(instance.clone()), &declared, &granted_all).is_none());
    assert!(granted(Some(store.clone()), None, &declared, &granted_all).is_none());
    assert!(granted(Some(store), Some(instance), &declared, &granted_all).is_some());
}

#[tokio::test]
async fn urls_need_a_public_origin_and_a_declared_name() {
    let _dek = crate::PROCESS_DEK_TEST_LOCK.lock().await;
    let store = memory_store().await;
    let instance = PluginInstanceId::mint();
    let hook = granted(
        Some(store.clone()),
        Some(instance.clone()),
        &manifest(EVENTS),
        &grant(&[WEBHOOKS_GRANT_BINDING]),
    )
    .expect("granted");

    set_webhook_public_origin(None);
    assert_eq!(hook.url("events".into()).await.expect("url"), None);

    set_webhook_public_origin(Some("https://library.example.org/"));
    assert_eq!(
        webhook_public_origin().as_deref(),
        Some("https://library.example.org")
    );
    let url = hook.url("events".into()).await.expect("url").expect("some");
    let again = hook.url("events".into()).await.expect("url").expect("some");
    assert_eq!(url, again, "the URL is stable");
    let token = url
        .strip_prefix("https://library.example.org/hooks/")
        .and_then(|rest| rest.strip_suffix("/events"))
        .expect("url shape");
    let token = WebhookUrlToken::parse(token).expect("token");
    let target = find_webhook(&store, &token)
        .await
        .expect("lookup")
        .expect("found");
    assert_eq!(target.instance_id, instance);
    assert_eq!(target.name, "events");

    let undeclared = hook.url("other".into()).await.expect_err("undeclared");
    assert_eq!(undeclared.code, PluginErrorCode::NotFound);
    let malformed = hook.url("Bad Name".into()).await.expect_err("malformed");
    assert_eq!(malformed.code, PluginErrorCode::InvalidParams);
    set_webhook_public_origin(Some(""));
    assert_eq!(webhook_public_origin(), None);
}

#[tokio::test]
async fn deliveries_are_pulled_and_acknowledged() {
    let _dek = crate::PROCESS_DEK_TEST_LOCK.lock().await;
    let store = memory_store().await;
    let instance = PluginInstanceId::mint();
    let hook = granted(
        Some(store.clone()),
        Some(instance.clone()),
        &manifest(EVENTS),
        &grant(&[WEBHOOKS_GRANT_BINDING]),
    )
    .expect("granted");
    let pull = WebhookPullRequest {
        name: "events".into(),
        batch_size: 10,
        visibility_timeout_ms: 30_000,
    };
    let empty = hook.pull(pull.clone()).await.expect("pull");
    assert!(empty.deliveries.is_empty());

    let token = webhook_url_token(&store, &instance, "events")
        .await
        .expect("token");
    let target = find_webhook(&store, &token)
        .await
        .expect("lookup")
        .expect("found");
    let outcome = store_webhook_request(
        &store,
        &target,
        &WebhookRequest {
            headers: vec![WebhookHeader {
                name: "content-type".into(),
                value: "application/json".into(),
            }],
            body: b"{\"ok\":true}".to_vec(),
            received_at_ms: chrono::Utc::now().timestamp_millis(),
            retention_ms: 60_000,
        },
    )
    .await
    .expect("store");
    assert!(matches!(
        outcome,
        WebhookStoreOutcome::Stored { waiting: 1 }
    ));

    let page = hook.pull(pull.clone()).await.expect("pull");
    assert_eq!(page.deliveries.len(), 1);
    let delivery = &page.deliveries[0];
    assert_eq!(delivery.body, b"{\"ok\":true}");
    assert_eq!(delivery.attempt, 1);
    hook.ack(WebhookAckRequest {
        name: "events".into(),
        acks: vec![delivery.lease_id.clone()],
        retries: Vec::new(),
    })
    .await
    .expect("ack");
    assert!(hook
        .pull(pull.clone())
        .await
        .expect("pull")
        .deliveries
        .is_empty());

    let undeclared = hook
        .pull(WebhookPullRequest {
            name: "other".into(),
            ..pull.clone()
        })
        .await
        .expect_err("undeclared");
    assert_eq!(undeclared.code, PluginErrorCode::NotFound);
    let zero = hook
        .pull(WebhookPullRequest {
            batch_size: 0,
            ..pull
        })
        .await
        .expect_err("zero batch");
    assert_eq!(zero.code, PluginErrorCode::InvalidParams);
    let bad_ack = hook
        .ack(WebhookAckRequest {
            name: "other".into(),
            acks: Vec::new(),
            retries: Vec::new(),
        })
        .await
        .expect_err("undeclared ack");
    assert_eq!(bad_ack.code, PluginErrorCode::NotFound);
}

#[test]
fn an_endpoint_never_prints_its_secret() {
    let endpoint = WebhookEndpoint {
        row: manifest(EVENTS).webhooks[0].clone(),
        secret: Some(zeroize::Zeroizing::new(b"hunter2".to_vec())),
    };
    let printed = format!("{endpoint:?}");
    assert!(!printed.contains("hunter2"));
    assert!(printed.contains("[redacted]"));
}
