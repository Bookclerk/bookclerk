//! One refresh keeper per single-keeper account: which side keeps it, the
//! keeper's revisions, and a library applying only newer pushes from its
//! keeper branch, under every database profile.

use super::*;
use crate::master_key::master_key_test_lock_async;
use crate::models::UserRole;
use crate::secrets::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret, upsert_secret,
};
use crate::store::LibraryStore;
use crate::test_support::{sqlite, Profile};
use bookclerk_node_proto::{CarriedAccount, CarriedSecret, SecretValue};

/// A single-keeper storefront.
const KEEPER: &str = "platform:bookclerk/bookclerk-plugin-source-audible#audible";

/// Connects `account_id` for `user_id` with an `auth` secret of `value`.
async fn connect(store: &LibraryStore, user_id: i64, account_id: &str, value: &[u8]) {
    store
        .upsert_account(KEEPER, account_id, "us", None, true, "audible")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user_id, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, KEEPER, account_id, "audible")
        .await
        .unwrap();
    let record = build_sealed_record(
        value,
        secret_kind::SOURCE_AUTH,
        KEEPER,
        secret_account_type::INTEGRATION,
        account_id,
        "auth",
    )
    .unwrap();
    upsert_secret(store.db(), &record).await.unwrap();
}

/// The unsealed `auth` secret of an account.
async fn secret_of(store: &LibraryStore, account_id: &str) -> Vec<u8> {
    let record = get_secret(
        store.db(),
        secret_kind::SOURCE_AUTH,
        Some(KEEPER),
        secret_account_type::INTEGRATION,
        Some(account_id),
        "auth",
    )
    .await
    .unwrap()
    .unwrap();
    unseal_secret(&record).unwrap()
}

/// A pushed sign-in of `account_id` holding `value`.
fn pushed(account_id: &str, value: &[u8]) -> CarriedAccount {
    CarriedAccount {
        plugin_key: KEEPER.into(),
        account_id: account_id.into(),
        source: "audible".into(),
        marketplace: "us".into(),
        label: None,
        secrets: vec![CarriedSecret {
            kind: secret_kind::SOURCE_AUTH.into(),
            name: "auth".into(),
            value: SecretValue::new(value.to_vec()),
        }],
    }
}

#[tokio::test]
async fn a_library_applies_only_newer_pushes_from_its_keeper_branch() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let ana = store
            .create_user(UserRole::Member, Some("Ana"), None)
            .await
            .unwrap();
        let sam = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        connect(store, ana.id, "ana", b"v1").await;
        connect(store, sam.id, "sam", b"s1").await;

        // Not shared with the branch yet: refused, and no branch keeps it.
        assert_eq!(keeping_branch(store, KEEPER, "ana").await.unwrap(), None);
        let outcome = apply_keeper_update(store, ana.id, "b1", &pushed("ana", b"v2"), 10)
            .await
            .unwrap();
        assert_eq!(outcome, KeeperUpdate::NotShared, "{name}");
        assert_eq!(secret_of(store, "ana").await, b"v1", "{name}");

        record_accounts_sent_to(store, &node_peer("b1"), &[pushed("ana", b"")])
            .await
            .unwrap();
        record_accounts_sent_to(store, &node_peer("b1"), &[pushed("sam", b"")])
            .await
            .unwrap();
        assert_eq!(
            keeping_branch(store, KEEPER, "ana")
                .await
                .unwrap()
                .as_deref(),
            Some("b1"),
            "{name}"
        );
        let outcome = apply_keeper_update(store, ana.id, "b1", &pushed("ana", b"v2"), 10)
            .await
            .unwrap();
        assert_eq!(outcome, KeeperUpdate::Applied, "{name}");
        assert_eq!(secret_of(store, "ana").await, b"v2", "{name}");

        // The same or an older revision changes nothing.
        for revision in [10, 9] {
            let outcome =
                apply_keeper_update(store, ana.id, "b1", &pushed("ana", b"old"), revision)
                    .await
                    .unwrap();
            assert_eq!(outcome, KeeperUpdate::Stale, "{name} {revision}");
        }
        assert_eq!(secret_of(store, "ana").await, b"v2", "{name}");

        // Another branch, or another person's account, is not the keeper.
        let outcome = apply_keeper_update(store, ana.id, "b2", &pushed("ana", b"x"), 99)
            .await
            .unwrap();
        assert_eq!(outcome, KeeperUpdate::NotShared, "{name}");
        let outcome = apply_keeper_update(store, ana.id, "b1", &pushed("sam", b"x"), 99)
            .await
            .unwrap();
        assert_eq!(outcome, KeeperUpdate::NotShared, "{name}");
        assert_eq!(secret_of(store, "sam").await, b"s1", "{name}");

        // A newer one applies.
        let outcome = apply_keeper_update(store, ana.id, "b1", &pushed("ana", b"v3"), 11)
            .await
            .unwrap();
        assert_eq!(outcome, KeeperUpdate::Applied, "{name}");
        assert_eq!(secret_of(store, "ana").await, b"v3", "{name}");

        // Malformed pushes are refused before anything is written.
        assert!(
            apply_keeper_update(store, ana.id, "b1", &pushed("ana", b"x"), 0)
                .await
                .is_err()
        );
        let mut empty = pushed("ana", b"x");
        empty.secrets.clear();
        assert!(apply_keeper_update(store, ana.id, "b1", &empty, 50)
            .await
            .is_err());

        // Disconnected everywhere: the library forgets its keeper, so a
        // later connection may pick another branch.
        record_accounts_sent_to(store, &link_peer("c9"), &[pushed("ana", b"")])
            .await
            .unwrap();
        withdraw_person_account(store, ana.id, KEEPER, "ana")
            .await
            .unwrap();
        assert_eq!(
            keeping_branch(store, KEEPER, "ana").await.unwrap(),
            None,
            "{name}"
        );
        // What this install shared with a library it joined stays, so a
        // disconnect retried later still reaches it.
        assert!(keeps_for_link(store, KEEPER, "ana", "c9").await.unwrap());
        // Sam's account, still linked, keeps its keeper.
        assert_eq!(
            keeping_branch(store, KEEPER, "sam")
                .await
                .unwrap()
                .as_deref(),
            Some("b1"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_keeper_tracks_what_it_pushed_to_each_library() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let ana = store
            .create_user(UserRole::Owner, Some("Ana"), None)
            .await
            .unwrap();
        connect(store, ana.id, "ana", b"v1").await;
        assert!(sign_in_revision(store, KEEPER, "ana")
            .await
            .unwrap()
            .is_some_and(|rev| rev > 0));
        assert_eq!(
            sign_in_revision(store, KEEPER, "nobody").await.unwrap(),
            None
        );
        assert!(kept_shares(store).await.unwrap().is_empty(), "{name}");
        assert!(!keeps_for_link(store, KEEPER, "ana", "c1").await.unwrap());

        record_accounts_sent_to(store, &link_peer("c1"), &[pushed("ana", b"")])
            .await
            .unwrap();
        assert!(keeps_for_link(store, KEEPER, "ana", "c1").await.unwrap());
        assert_eq!(
            kept_shares(store).await.unwrap(),
            vec![KeptShare {
                plugin_key: KEEPER.into(),
                account_id: "ana".into(),
                cluster_id: "c1".into(),
                pushed: 0,
                observed: 0,
            }],
            "{name}"
        );
        assert_eq!(
            kept_shares(store).await.unwrap()[0].next_revision(20),
            Some(20)
        );
        record_pushed_revision(store, KEEPER, "ana", "c1", 20, 20)
            .await
            .unwrap();
        // Never lowered.
        record_pushed_revision(store, KEEPER, "ana", "c1", 5, 5)
            .await
            .unwrap();
        let share = kept_shares(store).await.unwrap().remove(0);
        assert_eq!((share.pushed, share.observed), (20, 20), "{name}");
        // The same sign-in is not sent again; a new one is, even one whose
        // time went back, above the last revision sent.
        assert_eq!(share.next_revision(20), None, "{name}");
        assert_eq!(share.next_revision(30), Some(30), "{name}");
        assert_eq!(share.next_revision(12), Some(21), "{name}");

        // Several shares and several secrets read in pages within the
        // profile's result limit (one row on the minimum profile).
        assert_eq!(kept_shares(store).await.unwrap().len(), 1, "{name}");
        record_accounts_sent_to(store, &link_peer("c2"), &[pushed("ana", b"")])
            .await
            .unwrap();
        record_accounts_sent_to(store, &node_peer("b7"), &[pushed("ana", b"")])
            .await
            .unwrap();
        let clusters: Vec<String> = kept_shares(store)
            .await
            .unwrap()
            .into_iter()
            .map(|share| share.cluster_id)
            .collect();
        assert_eq!(clusters, ["c1", "c2"], "{name}");
        let before = sign_in_revision(store, KEEPER, "ana").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = build_sealed_record(
            b"other",
            secret_kind::SOURCE_AUTH,
            KEEPER,
            secret_account_type::INTEGRATION,
            "ana",
            "other",
        )
        .unwrap();
        upsert_secret(store.db(), &second).await.unwrap();
        let after = sign_in_revision(store, KEEPER, "ana").await.unwrap();
        assert!(after > before, "{name}: {before:?} -> {after:?}");
    }
}
