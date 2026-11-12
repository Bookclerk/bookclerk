//! Store accounts that follow their person: the operator's per-member
//! rule, gathering a member's shareable accounts for a branch, and
//! importing them on the branch, under every database profile.

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::*;
use crate::entities::{account_links, account_shares, accounts};
use crate::master_key::master_key_test_lock_async;
use crate::models::{UserRole, UserStatus};
use crate::secrets::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret, upsert_secret,
};
use crate::store::LibraryStore;
use crate::test_support::{sqlite, Profile};
use bookclerk_node_proto::MAX_BUNDLE_ACCOUNTS;

/// A storefront that declares `credentials = "shareable"`.
const SHAREABLE: &str = "platform:bookclerk/bookclerk-plugin-source-libro#libro";

/// A storefront that keeps one refresh keeper (`single_keeper`).
const KEEPER: &str = "platform:bookclerk/bookclerk-plugin-source-audible#audible";

/// Whether `plugin_key` is [`SHAREABLE`].
fn shareable(plugin_key: &str) -> bool {
    plugin_key == SHAREABLE
}

/// Connects `account_id` of `plugin_key` for `user_id`, with one secret
/// holding `value`.
async fn connect(
    store: &LibraryStore,
    user_id: i64,
    plugin_key: &str,
    account_id: &str,
    value: &[u8],
) {
    store
        .upsert_account(plugin_key, account_id, "us", Some("Reader"), true, "store")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user_id, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, plugin_key, account_id, "store")
        .await
        .unwrap();
    let record = build_sealed_record(
        value,
        secret_kind::SOURCE_AUTH,
        plugin_key,
        secret_account_type::INTEGRATION,
        account_id,
        "auth",
    )
    .unwrap();
    upsert_secret(store.db(), &record).await.unwrap();
}

/// The unsealed `auth` secret of an account, if any.
async fn secret_of(store: &LibraryStore, plugin_key: &str, account_id: &str) -> Option<Vec<u8>> {
    get_secret(
        store.db(),
        secret_kind::SOURCE_AUTH,
        Some(plugin_key),
        secret_account_type::INTEGRATION,
        Some(account_id),
        "auth",
    )
    .await
    .unwrap()
    .map(|record| unseal_secret(&record).unwrap())
}

#[tokio::test]
async fn the_operator_decides_per_member_and_no_rule_allows() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let sam = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        assert!(
            member_may_connect_accounts(store, sam.id).await.unwrap(),
            "{name}"
        );
        assert!(
            member_account_rules(store).await.unwrap().is_empty(),
            "{name}"
        );

        set_member_may_connect_accounts(store, sam.id, false)
            .await
            .unwrap();
        assert!(
            !member_may_connect_accounts(store, sam.id).await.unwrap(),
            "{name}"
        );
        let identity = store
            .ensure_local_portal_identity(sam.id, None)
            .await
            .unwrap();
        assert!(
            !identity_may_connect_accounts(store, identity.id)
                .await
                .unwrap(),
            "{name}"
        );
        assert_eq!(
            member_account_rules(store).await.unwrap().get(&sam.id),
            Some(&false),
            "{name}"
        );

        set_member_may_connect_accounts(store, sam.id, true)
            .await
            .unwrap();
        assert!(
            member_may_connect_accounts(store, sam.id).await.unwrap(),
            "{name}"
        );

        // A disabled member and an unknown user may not connect.
        store
            .set_user_status(sam.id, UserStatus::Disabled)
            .await
            .unwrap();
        assert!(
            !member_may_connect_accounts(store, sam.id).await.unwrap(),
            "{name}"
        );
        assert!(
            !member_may_connect_accounts(store, 9_999).await.unwrap(),
            "{name}"
        );
        assert!(matches!(
            set_member_may_connect_accounts(store, 9_999, true).await,
            Err(LibraryError::NotFound(_))
        ));
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

#[tokio::test]
async fn only_the_members_own_shareable_connected_accounts_travel() {
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
        let ben = store
            .create_user(UserRole::Member, Some("Ben"), None)
            .await
            .unwrap();
        connect(store, ana.id, SHAREABLE, "ana@example.com", b"ana-token").await;
        connect(store, ana.id, KEEPER, "ana-audible", b"keeper-token").await;
        connect(store, ana.id, SHAREABLE, "ana-old@example.com", b"old").await;
        store
            .revoke_credentials(SHAREABLE, "ana-old@example.com")
            .await
            .unwrap();
        connect(store, ben.id, SHAREABLE, "ben@example.com", b"ben-token").await;

        let carried = accounts_to_carry(store, ana.id, &shareable).await.unwrap();
        assert_eq!(carried.len(), 1, "{name}");
        let account = &carried[0];
        assert_eq!(account.plugin_key, SHAREABLE, "{name}");
        assert_eq!(account.account_id, "ana@example.com", "{name}");
        assert_eq!(account.marketplace, "us", "{name}");
        assert_eq!(account.label.as_deref(), Some("Reader"), "{name}");
        assert_eq!(account.secrets.len(), 1, "{name}");
        assert_eq!(account.secrets[0].kind, secret_kind::SOURCE_AUTH, "{name}");
        assert_eq!(account.secrets[0].name, "auth", "{name}");
        assert_eq!(account.secrets[0].value.expose(), b"ana-token", "{name}");

        record_accounts_sent(store, "node-1", &carried)
            .await
            .unwrap();
        let shares = account_shares::Entity::find()
            .all(store.db())
            .await
            .unwrap();
        assert_eq!(shares.len(), 1, "{name}");
        assert_eq!(shares[0].peer, node_peer("node-1"), "{name}");
        assert_eq!(shares[0].direction, SHARE_SENT, "{name}");
        // A repeat refreshes the row instead of adding one.
        record_accounts_sent(store, "node-1", &carried)
            .await
            .unwrap();
        assert_eq!(
            account_shares::Entity::find()
                .all(store.db())
                .await
                .unwrap()
                .len(),
            1,
            "{name}"
        );
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

#[tokio::test]
async fn a_branch_imports_carried_accounts_for_its_person_once() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    let library = {
        let kit = sqlite::memory(Profile::ALL[0]).await;
        let store = kit.store();
        let ana = store
            .create_user(UserRole::Owner, Some("Ana"), None)
            .await
            .unwrap();
        connect(store, ana.id, SHAREABLE, "ana@example.com", b"ana-token").await;
        connect(
            store,
            ana.id,
            SHAREABLE,
            "shared@example.com",
            b"from-library",
        )
        .await;
        accounts_to_carry(store, ana.id, &shareable).await.unwrap()
    };
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let me = store
            .create_user(UserRole::Owner, Some("Ana"), None)
            .await
            .unwrap();
        let flatmate = store
            .create_user(UserRole::Member, Some("Cai"), None)
            .await
            .unwrap();
        // Someone else here already connected one of the accounts.
        connect(
            store,
            flatmate.id,
            SHAREABLE,
            "shared@example.com",
            b"cai-token",
        )
        .await;

        let report = import_carried_accounts(store, me.id, "cluster-x", &library)
            .await
            .unwrap();
        assert_eq!(
            report.imported,
            vec![(SHAREABLE.to_string(), "ana@example.com".to_string())],
            "{name}"
        );
        assert_eq!(
            report.taken,
            vec![(SHAREABLE.to_string(), "shared@example.com".to_string())],
            "{name}"
        );
        assert!(!report.refused_by_rule, "{name}");

        // The imported account is connected, scanning, linked to the
        // person, and its secret re-sealed here.
        let row = store
            .get_account(SHAREABLE, "ana@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.connection_status, "active", "{name}");
        assert!(row.scan_enabled, "{name}");
        assert_eq!(
            secret_of(store, SHAREABLE, "ana@example.com")
                .await
                .as_deref(),
            Some(&b"ana-token"[..]),
            "{name}"
        );
        let identity = store
            .ensure_local_portal_identity(me.id, None)
            .await
            .unwrap();
        let links = store.list_account_links(identity.id).await.unwrap();
        assert_eq!(links.len(), 1, "{name}");
        let shares = account_shares::Entity::find()
            .filter(account_shares::Column::AccountId.eq("ana@example.com"))
            .all(store.db())
            .await
            .unwrap();
        assert_eq!(shares.len(), 1, "{name}");
        assert_eq!(shares[0].peer, link_peer("cluster-x"), "{name}");
        assert_eq!(shares[0].direction, SHARE_RECEIVED, "{name}");

        // The other person's account and secret are untouched, with no
        // share recorded for it.
        assert_eq!(
            secret_of(store, SHAREABLE, "shared@example.com")
                .await
                .as_deref(),
            Some(&b"cai-token"[..]),
            "{name}"
        );
        assert_eq!(
            account_links::Entity::find()
                .filter(account_links::Column::AccountId.eq("shared@example.com"))
                .all(store.db())
                .await
                .unwrap()
                .len(),
            1,
            "{name}"
        );
        assert!(account_shares::Entity::find()
            .filter(account_shares::Column::AccountId.eq("shared@example.com"))
            .all(store.db())
            .await
            .unwrap()
            .is_empty());

        // A second join leaves the account as the person has it here.
        let again = import_carried_accounts(store, me.id, "cluster-y", &library[..1])
            .await
            .unwrap();
        assert_eq!(
            again.already_here,
            vec![(SHAREABLE.to_string(), "ana@example.com".to_string())],
            "{name}"
        );
        assert!(again.imported.is_empty(), "{name}");
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

#[tokio::test]
async fn an_operator_who_refuses_the_member_stores_nothing() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    let library = {
        let kit = sqlite::memory(Profile::ALL[0]).await;
        let store = kit.store();
        let ana = store
            .create_user(UserRole::Owner, Some("Ana"), None)
            .await
            .unwrap();
        connect(store, ana.id, SHAREABLE, "ana@example.com", b"ana-token").await;
        accounts_to_carry(store, ana.id, &shareable).await.unwrap()
    };
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let me = store
            .create_user(UserRole::Member, Some("Ana"), None)
            .await
            .unwrap();
        set_member_may_connect_accounts(store, me.id, false)
            .await
            .unwrap();
        let report = import_carried_accounts(store, me.id, "cluster-x", &library)
            .await
            .unwrap();
        assert!(report.refused_by_rule, "{name}");
        assert!(report.imported.is_empty(), "{name}");
        assert!(
            accounts::Entity::find()
                .all(store.db())
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        assert!(
            secret_of(store, SHAREABLE, "ana@example.com")
                .await
                .is_none(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn an_account_the_bundle_refuses_stays_and_the_others_travel() {
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
        connect(store, ana.id, SHAREABLE, "ana@example.com", b"ana-token").await;
        // A label longer than a bundle allows travels without its label.
        connect(store, ana.id, SHAREABLE, "long@example.com", b"long").await;
        store
            .upsert_account(
                SHAREABLE,
                "long@example.com",
                "us",
                Some(&"x".repeat(300)),
                true,
                "store",
            )
            .await
            .unwrap();
        // An empty secret can't travel at all.
        connect(store, ana.id, SHAREABLE, "empty@example.com", b"").await;

        let carried = accounts_to_carry(store, ana.id, &shareable).await.unwrap();
        let ids: Vec<(&str, Option<&str>)> = carried
            .iter()
            .map(|account| (account.account_id.as_str(), account.label.as_deref()))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("ana@example.com", Some("Reader")),
                ("long@example.com", None)
            ],
            "{name}"
        );
    }
}

#[tokio::test]
async fn an_account_brought_without_a_secret_is_not_connected() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let me = store
            .create_user(UserRole::Owner, Some("Ana"), None)
            .await
            .unwrap();
        let bare = bookclerk_node_proto::CarriedAccount {
            plugin_key: SHAREABLE.to_string(),
            account_id: "bare@example.com".to_string(),
            source: "libro".to_string(),
            marketplace: "us".to_string(),
            label: None,
            secrets: Vec::new(),
        };
        let report = import_carried_accounts(store, me.id, "cluster-x", &[bare])
            .await
            .unwrap();
        assert_eq!(
            report.failed,
            vec![(SHAREABLE.to_string(), "bare@example.com".to_string())],
            "{name}"
        );
        assert!(report.imported.is_empty(), "{name}");
        assert!(
            store
                .get_account(SHAREABLE, "bare@example.com")
                .await
                .unwrap()
                .is_none(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn an_account_whose_secret_does_not_open_stays_and_the_others_travel() {
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
        // A JSON token bundle, as a storefront stores it.
        connect(
            store,
            ana.id,
            SHAREABLE,
            "ana@example.com",
            br#"{"access_token":"carried-access-7f3a91"}"#,
        )
        .await;
        connect(store, ana.id, SHAREABLE, "broken@example.com", b"broken").await;
        // One that sorts after the broken account still travels.
        connect(store, ana.id, SHAREABLE, "zoe@example.com", b"zoe-token").await;
        // Damage the second account's sealed secret so it no longer opens.
        let mut record = get_secret(
            store.db(),
            secret_kind::SOURCE_AUTH,
            Some(SHAREABLE),
            secret_account_type::INTEGRATION,
            Some("broken@example.com"),
            "auth",
        )
        .await
        .unwrap()
        .unwrap();
        for byte in record.ciphertext.iter_mut().rev().take(4) {
            *byte ^= 0xff;
        }
        upsert_secret(store.db(), &record).await.unwrap();

        let carried = accounts_to_carry(store, ana.id, &shareable).await.unwrap();
        let ids: Vec<&str> = carried.iter().map(|a| a.account_id.as_str()).collect();
        assert_eq!(ids, vec!["ana@example.com", "zoe@example.com"], "{name}");
        // The plaintext this process now holds is scrubbed from its logs,
        // the token inside the bundle included.
        assert!(
            bookclerk_config::contains_registered_secret("token=carried-access-7f3a91"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn sent_shares_are_recorded_in_batches_every_profile_accepts() {
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
        // More than two batches' worth.
        for n in 0..45 {
            connect(
                store,
                ana.id,
                SHAREABLE,
                &format!("ana{n}@example.com"),
                b"token",
            )
            .await;
        }
        let carried = accounts_to_carry(store, ana.id, &shareable).await.unwrap();
        assert_eq!(carried.len(), 45, "{name}");
        record_accounts_sent(store, "node-1", &carried)
            .await
            .unwrap();
        let shares = account_shares::Entity::find()
            .filter(account_shares::Column::Direction.eq(SHARE_SENT))
            .all(store.db())
            .await
            .unwrap();
        assert_eq!(shares.len(), 45, "{name}");
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

#[tokio::test]
async fn a_refused_logins_account_is_taken_back_only_while_unlinked() {
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
        // Linked: another login connected it meanwhile.
        connect(store, ana.id, SHAREABLE, "ana@example.com", b"ana-token").await;
        // Unlinked: only the refused login stored it.
        store
            .upsert_account(SHAREABLE, "left@example.com", "us", None, true, "store")
            .await
            .unwrap();
        let record = build_sealed_record(
            b"left-token",
            secret_kind::SOURCE_AUTH,
            SHAREABLE,
            secret_account_type::INTEGRATION,
            "left@example.com",
            "auth",
        )
        .unwrap();
        upsert_secret(store.db(), &record).await.unwrap();

        for id in ["ana@example.com", "left@example.com"] {
            revoke_unlinked_account(store, SHAREABLE, id).await.unwrap();
        }
        let linked = store
            .get_account(SHAREABLE, "ana@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(linked.connection_status, "active", "{name}");
        assert!(
            secret_of(store, SHAREABLE, "ana@example.com")
                .await
                .is_some(),
            "{name}"
        );
        let left = store
            .get_account(SHAREABLE, "left@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(left.connection_status, "revoked", "{name}");
        assert!(!left.scan_enabled, "{name}");
        assert!(
            secret_of(store, SHAREABLE, "left@example.com")
                .await
                .is_none(),
            "{name}"
        );
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

#[tokio::test]
async fn an_import_batch_stores_nothing_once_the_rule_is_turned_off() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    let library = {
        let kit = sqlite::memory(Profile::ALL[0]).await;
        let store = kit.store();
        let ana = store
            .create_user(UserRole::Owner, Some("Ana"), None)
            .await
            .unwrap();
        connect(store, ana.id, SHAREABLE, "ana@example.com", b"ana-token").await;
        accounts_to_carry(store, ana.id, &shareable).await.unwrap()
    };
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let me = store
            .create_user(UserRole::Member, Some("Ana"), None)
            .await
            .unwrap();
        let identity = store
            .ensure_local_portal_identity(me.id, Some("Ana"))
            .await
            .unwrap();
        // The import read the rule as allowed; the operator turns it off
        // before this account's batch runs.
        set_member_may_connect_accounts(store, me.id, false)
            .await
            .unwrap();
        let peer = link_peer("cluster-x");
        let kept = carried_accounts::import_one(store, me.id, identity.id, &peer, &library[0])
            .await
            .unwrap();
        assert!(!kept, "{name}");
        assert!(
            accounts::Entity::find()
                .all(store.db())
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        assert!(
            secret_of(store, SHAREABLE, "ana@example.com")
                .await
                .is_none(),
            "{name}"
        );
        assert!(
            account_links::Entity::find()
                .all(store.db())
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        // A person disabled meanwhile is stopped the same way.
        set_member_may_connect_accounts(store, me.id, true)
            .await
            .unwrap();
        store
            .set_user_status(me.id, UserStatus::Disabled)
            .await
            .unwrap();
        let kept = carried_accounts::import_one(store, me.id, identity.id, &peer, &library[0])
            .await
            .unwrap();
        assert!(!kept, "{name}");
        // Active again with the rule on, the same batch keeps the account.
        store
            .set_user_status(me.id, UserStatus::Active)
            .await
            .unwrap();
        let kept = carried_accounts::import_one(store, me.id, identity.id, &peer, &library[0])
            .await
            .unwrap();
        assert!(kept, "{name}");
    }
}

#[tokio::test]
async fn disconnecting_everywhere_withdraws_only_the_persons_links() {
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
        let ben = store
            .create_user(UserRole::Member, Some("Ben"), None)
            .await
            .unwrap();
        connect(store, ana.id, SHAREABLE, "ana@example.com", b"ana-token").await;
        connect(store, ben.id, SHAREABLE, "ben@example.com", b"ben-token").await;

        // Not Ana's: nothing changes.
        assert_eq!(
            withdraw_person_account(store, ana.id, SHAREABLE, "ben@example.com")
                .await
                .unwrap(),
            Withdrawn::NotTheirs,
            "{name}"
        );
        assert!(
            secret_of(store, SHAREABLE, "ben@example.com")
                .await
                .is_some(),
            "{name}"
        );

        // Ana's own: her link goes, and the account with it.
        assert_eq!(
            withdraw_person_account(store, ana.id, SHAREABLE, "ana@example.com")
                .await
                .unwrap(),
            Withdrawn::Unlinked,
            "{name}"
        );
        assert_eq!(
            store
                .count_account_links_for_account(SHAREABLE, "ana@example.com")
                .await
                .unwrap(),
            0,
            "{name}"
        );
        let account = store
            .get_account(SHAREABLE, "ana@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(account.connection_status, "revoked", "{name}");
        assert!(!account.scan_enabled, "{name}");
        assert!(
            secret_of(store, SHAREABLE, "ana@example.com")
                .await
                .is_none(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn an_offer_from_a_branch_is_recorded_against_that_node() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    let offered = {
        let kit = sqlite::memory(Profile::ALL[0]).await;
        let store = kit.store();
        let ana = store
            .create_user(UserRole::Owner, Some("Ana"), None)
            .await
            .unwrap();
        connect(store, ana.id, SHAREABLE, "ana@example.com", b"ana-token").await;
        accounts_to_carry(store, ana.id, &shareable).await.unwrap()
    };
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let me = store
            .create_user(UserRole::Member, Some("Ana"), None)
            .await
            .unwrap();
        let report = import_offered_accounts(store, me.id, "branch-node", &offered)
            .await
            .unwrap();
        assert_eq!(report.imported.len(), 1, "{name}");
        assert_eq!(
            account_share_peers(store, SHAREABLE, "ana@example.com")
                .await
                .unwrap(),
            vec![node_peer("branch-node")],
            "{name}"
        );
        // A branch that offers on to a library records the link it went to.
        record_accounts_sent_to(store, &link_peer("cluster-up"), &offered)
            .await
            .unwrap();
        assert_eq!(
            account_share_peers(store, SHAREABLE, "ana@example.com")
                .await
                .unwrap(),
            vec![link_peer("cluster-up"), node_peer("branch-node")],
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_picked_account_travels_even_past_the_bundle_limit() {
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
        for n in 0..=MAX_BUNDLE_ACCOUNTS {
            connect(
                store,
                ana.id,
                SHAREABLE,
                &format!("ana{n}@example.com"),
                b"t",
            )
            .await;
        }
        // Unpicked, one account is left behind by the limit.
        let all = accounts_to_carry(store, ana.id, &shareable).await.unwrap();
        assert_eq!(all.len(), MAX_BUNDLE_ACCOUNTS, "{name}");
        let left_behind = (0..=MAX_BUNDLE_ACCOUNTS)
            .map(|n| format!("ana{n}@example.com"))
            .find(|id| !all.iter().any(|account| &account.account_id == id))
            .unwrap();
        // Picked, it travels.
        let keep =
            |plugin_key: &str, account_id: &str| shareable(plugin_key) && account_id == left_behind;
        let picked = accounts_to_carry_where(store, ana.id, &keep).await.unwrap();
        assert_eq!(picked.len(), 1, "{name}");
        assert_eq!(picked[0].account_id, left_behind, "{name}");
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}
