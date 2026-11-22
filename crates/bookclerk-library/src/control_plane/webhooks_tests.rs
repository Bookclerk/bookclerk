//! Webhook tokens and stored requests under every database profile.

use super::*;
use crate::master_key::{ensure_shared_test_dek, master_key_test_read_lock_async};
use crate::test_support::Profile;

/// A body that spans several chunks under every profile but the first-party
/// one, and does not compress.
fn body(len: usize) -> Vec<u8> {
    let mut state: u32 = 0x2545_f491;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn request(body: Vec<u8>, received_at_ms: i64) -> WebhookRequest {
    WebhookRequest {
        headers: vec![WebhookHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        body,
        received_at_ms,
        retention_ms: 60_000,
    }
}

#[tokio::test]
async fn a_token_is_minted_once_and_finds_its_webhook() {
    let _dek = master_key_test_read_lock_async().await;
    ensure_shared_test_dek();
    for profile in Profile::ALL {
        let kit = crate::test_support::sqlite::memory(profile).await;
        let store = kit.store();
        let instance = PluginInstanceId::mint();
        let first = webhook_url_token(store, &instance, "events").await.unwrap();
        let again = webhook_url_token(store, &instance, "events").await.unwrap();
        assert_eq!(first, again, "{}", profile.name());
        let other = webhook_url_token(store, &instance, "other").await.unwrap();
        assert_ne!(first, other);
        assert_eq!(
            find_webhook(store, &first).await.unwrap(),
            Some(WebhookTarget {
                instance_id: instance.clone(),
                name: "events".into(),
            })
        );
        let unknown = WebhookUrlToken::parse(&"A".repeat(43)).unwrap();
        assert_eq!(find_webhook(store, &unknown).await.unwrap(), None);
        assert!(!format!("{first:?}").contains(first.as_str()));
        assert_eq!(kit.refused_begins(), 0, "{}", profile.name());
    }
}

#[test]
fn tokens_parse_only_their_own_shape() {
    assert!(WebhookUrlToken::parse(&"a".repeat(43)).is_some());
    assert!(WebhookUrlToken::parse(&"a".repeat(42)).is_none());
    assert!(WebhookUrlToken::parse(&format!("{}/", "a".repeat(42))).is_none());
    assert_eq!(WebhookUrlToken::mint().as_str().len(), 43);
}

#[tokio::test]
async fn requests_round_trip_through_pull_and_ack() {
    let _dek = master_key_test_read_lock_async().await;
    ensure_shared_test_dek();
    for profile in Profile::ALL {
        let kit = crate::test_support::sqlite::memory(profile).await;
        let store = kit.store();
        let instance = PluginInstanceId::mint();
        webhook_url_token(store, &instance, "events").await.unwrap();
        let target = WebhookTarget {
            instance_id: instance.clone(),
            name: "events".into(),
        };
        let big = body(20 * 1024);
        assert_eq!(
            store_webhook_request(store, &target, &request(big.clone(), 1_000))
                .await
                .unwrap(),
            WebhookStoreOutcome::Stored { waiting: 1 }
        );
        assert_eq!(
            store_webhook_request(store, &target, &request(b"{}".to_vec(), 2_000))
                .await
                .unwrap(),
            WebhookStoreOutcome::Stored { waiting: 2 }
        );
        assert_eq!(
            waiting_webhook_requests(store, &instance, "events")
                .await
                .unwrap(),
            2
        );

        // A profile with a one-row result cap leases one request a pull.
        let mut page = WebhookPullPage::default();
        loop {
            let next = pull_webhook_requests(store, &instance, "events", 10, 30_000, 3_000)
                .await
                .unwrap();
            assert!(!next.missed);
            if next.deliveries.is_empty() {
                break;
            }
            page.deliveries.extend(next.deliveries);
        }
        assert_eq!(page.deliveries.len(), 2, "{}", profile.name());
        assert_eq!(page.deliveries[0].body, big, "{}", profile.name());
        assert_eq!(page.deliveries[0].received_at_unix_ms, 1_000);
        assert_eq!(page.deliveries[0].attempt, 1);
        assert_eq!(page.deliveries[0].headers[0].name, "content-type");
        assert_eq!(page.deliveries[1].body, b"{}");
        for delivery in &page.deliveries {
            bookclerk_plugin_abi::webhooks::check_webhook_delivery(delivery).unwrap();
        }

        // Leased deliveries stay hidden until the lease runs out.
        let hidden = pull_webhook_requests(store, &instance, "events", 10, 30_000, 4_000)
            .await
            .unwrap();
        assert!(hidden.deliveries.is_empty());

        let first = page.deliveries[0].lease_id.clone();
        let second = page.deliveries[1].lease_id.clone();
        ack_webhook_requests(store, &instance, "events", &[first], &[second])
            .await
            .unwrap();
        let again = pull_webhook_requests(store, &instance, "events", 10, 30_000, 5_000)
            .await
            .unwrap();
        assert_eq!(again.deliveries.len(), 1, "the retry is offered again");
        assert_eq!(again.deliveries[0].body, b"{}");
        assert_eq!(again.deliveries[0].attempt, 2);
        ack_webhook_requests(
            store,
            &instance,
            "events",
            &[again.deliveries[0].lease_id.clone()],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(
            waiting_webhook_requests(store, &instance, "events")
                .await
                .unwrap(),
            0
        );
        assert_eq!(kit.refused_begins(), 0, "{}", profile.name());
    }
}

#[tokio::test]
async fn an_expired_request_is_dropped_and_reported_missed_once() {
    let _dek = master_key_test_read_lock_async().await;
    ensure_shared_test_dek();
    let kit = crate::test_support::sqlite::memory(Profile::D1).await;
    let store = kit.store();
    let instance = PluginInstanceId::mint();
    webhook_url_token(store, &instance, "events").await.unwrap();
    let target = WebhookTarget {
        instance_id: instance.clone(),
        name: "events".into(),
    };
    store_webhook_request(store, &target, &request(body(10 * 1024), 1_000))
        .await
        .unwrap();
    let page = pull_webhook_requests(store, &instance, "events", 10, 1_000, 61_000)
        .await
        .unwrap();
    assert!(page.deliveries.is_empty());
    assert!(page.missed);
    let page = pull_webhook_requests(store, &instance, "events", 10, 1_000, 62_000)
        .await
        .unwrap();
    assert!(!page.missed, "missed is reported once");
}

#[tokio::test]
async fn a_page_stops_at_its_byte_budget() {
    let _dek = master_key_test_read_lock_async().await;
    ensure_shared_test_dek();
    let kit = crate::test_support::sqlite::memory(Profile::FirstParty).await;
    let store = kit.store();
    let instance = PluginInstanceId::mint();
    webhook_url_token(store, &instance, "events").await.unwrap();
    let target = WebhookTarget {
        instance_id: instance.clone(),
        name: "events".into(),
    };
    let size = bookclerk_plugin_abi::MAX_WEBHOOK_BODY_BYTES as usize;
    for at in 0..5 {
        store_webhook_request(store, &target, &request(body(size), 1_000 + at))
            .await
            .unwrap();
    }
    let page = pull_webhook_requests(store, &instance, "events", 10, 30_000, 2_000)
        .await
        .unwrap();
    assert_eq!(page.deliveries.len(), MAX_WEBHOOK_PULL_PAGE_BYTES / size);
}

#[tokio::test]
async fn deleting_an_instance_removes_its_webhooks() {
    let _dek = master_key_test_read_lock_async().await;
    ensure_shared_test_dek();
    let kit = crate::test_support::sqlite::memory(Profile::Minimum).await;
    let store = kit.store();
    let instance = PluginInstanceId::mint();
    let token = webhook_url_token(store, &instance, "events").await.unwrap();
    let target = WebhookTarget {
        instance_id: instance.clone(),
        name: "events".into(),
    };
    store_webhook_request(store, &target, &request(body(2_000), 1_000))
        .await
        .unwrap();
    delete_instance_webhooks(store, &instance).await.unwrap();
    assert_eq!(find_webhook(store, &token).await.unwrap(), None);
    assert_eq!(
        waiting_webhook_requests(store, &instance, "events")
            .await
            .unwrap(),
        0
    );
}
