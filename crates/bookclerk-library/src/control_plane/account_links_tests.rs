//! Linking a signed-in store account and revoking that link, serialized
//! with a refused login's cleanup, under every database profile.

use super::*;
use crate::master_key::master_key_test_lock_async;
use crate::models::UserRole;
use crate::secrets::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, upsert_secret,
};
use crate::store::LibraryStore;
use crate::test_support::{sqlite, Profile};

/// The storefront the tests sign in to.
const STORE: &str = "platform:bookclerk/bookclerk-plugin-source-libro#libro";

/// Stores the account row and its sealed sign-in, as a store login does
/// before the portal links it.
async fn sign_in(store: &LibraryStore, account_id: &str) {
    store
        .upsert_account(STORE, account_id, "us", None, true, "libro")
        .await
        .unwrap();
    let record = build_sealed_record(
        b"login-token",
        secret_kind::SOURCE_AUTH,
        STORE,
        secret_account_type::INTEGRATION,
        account_id,
        "auth",
    )
    .unwrap();
    upsert_secret(store.db(), &record).await.unwrap();
}

/// Whether the account's sign-in is stored.
async fn signed_in(store: &LibraryStore, account_id: &str) -> bool {
    get_secret(
        store.db(),
        secret_kind::SOURCE_AUTH,
        Some(STORE),
        secret_account_type::INTEGRATION,
        Some(account_id),
        "auth",
    )
    .await
    .unwrap()
    .is_some()
}

/// A new member's portal identity id.
async fn identity(store: &LibraryStore, name: &str) -> i64 {
    let user = store
        .create_user(UserRole::Member, Some(name), None)
        .await
        .unwrap();
    store
        .ensure_local_portal_identity(user.id, None)
        .await
        .unwrap()
        .id
}

/// The account's connection status and whether scans are on.
async fn status(store: &LibraryStore, account_id: &str) -> (String, bool) {
    let account = store.get_account(STORE, account_id).await.unwrap().unwrap();
    (account.connection_status, account.scan_enabled)
}

#[tokio::test]
async fn a_login_links_only_while_its_sign_in_is_stored() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let sam = identity(store, "Sam").await;
        let kim = identity(store, "Kim").await;

        // No account and no sign-in: nothing to link.
        assert_eq!(
            link_signed_in_account(store, sam, STORE, "sam@example.com", "libro")
                .await
                .unwrap(),
            SignInLink::SignInTakenBack,
            "{name}"
        );

        sign_in(store, "sam@example.com").await;
        store
            .revoke_credentials(STORE, "sam@example.com")
            .await
            .unwrap();
        sign_in(store, "sam@example.com").await;
        assert_eq!(
            link_signed_in_account(store, sam, STORE, "sam@example.com", "libro")
                .await
                .unwrap(),
            SignInLink::Linked,
            "{name}"
        );
        // A login of a revoked account brings it back with scans on.
        assert_eq!(
            status(store, "sam@example.com").await,
            ("active".to_string(), true),
            "{name}"
        );
        assert_eq!(
            link_signed_in_account(store, sam, STORE, "sam@example.com", "libro")
                .await
                .unwrap(),
            SignInLink::AlreadyYours,
            "{name}"
        );
        assert_eq!(
            link_signed_in_account(store, kim, STORE, "sam@example.com", "libro")
                .await
                .unwrap(),
            SignInLink::LinkedToSomeoneElse,
            "{name}"
        );
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

/// #917: a refused login's cleanup that runs after an allowed login stored
/// its sign-in but before it linked leaves no link to a dead account; and a
/// cleanup after the link keeps the account.
#[tokio::test]
async fn a_refused_logins_cleanup_and_an_allowed_login_never_interleave() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let sam = identity(store, "Sam").await;

        // Credentials before link: the cleanup takes them back, so the
        // allowed login is told instead of linking an account with no secret.
        sign_in(store, "both@example.com").await;
        revoke_unlinked_account(store, STORE, "both@example.com")
            .await
            .unwrap();
        assert_eq!(
            link_signed_in_account(store, sam, STORE, "both@example.com", "libro")
                .await
                .unwrap(),
            SignInLink::SignInTakenBack,
            "{name}"
        );
        assert!(
            store.list_account_links(sam).await.unwrap().is_empty(),
            "{name}"
        );
        assert_eq!(
            status(store, "both@example.com").await,
            ("revoked".to_string(), false),
            "{name}"
        );

        // Signing in again connects it.
        sign_in(store, "both@example.com").await;
        assert_eq!(
            link_signed_in_account(store, sam, STORE, "both@example.com", "libro")
                .await
                .unwrap(),
            SignInLink::Linked,
            "{name}"
        );
        // Link before the cleanup: the cleanup keeps the account.
        revoke_unlinked_account(store, STORE, "both@example.com")
            .await
            .unwrap();
        assert!(signed_in(store, "both@example.com").await, "{name}");
        assert_eq!(
            status(store, "both@example.com").await,
            ("active".to_string(), true),
            "{name}"
        );
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

/// #840: revoking a link marks the account revoked, deletes its sign-in
/// and removes the link in one batch, and refuses as a whole once the link
/// is gone.
#[tokio::test]
async fn revoking_a_link_is_one_batch_guarded_on_the_link() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    crate::configure_master_key(dir.path()).expect("master key");
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let sam = identity(store, "Sam").await;

        sign_in(store, "sam@example.com").await;
        link_signed_in_account(store, sam, STORE, "sam@example.com", "libro")
            .await
            .unwrap();
        assert_eq!(
            revoke_account_link(store, sam, STORE, "sam@example.com")
                .await
                .unwrap(),
            LinkRevoked::Revoked,
            "{name}"
        );
        assert!(!signed_in(store, "sam@example.com").await, "{name}");
        assert!(
            store.list_account_links(sam).await.unwrap().is_empty(),
            "{name}"
        );
        assert_eq!(
            status(store, "sam@example.com").await,
            ("revoked".to_string(), false),
            "{name}"
        );
        assert_eq!(
            revoke_account_link(store, sam, STORE, "sam@example.com")
                .await
                .unwrap(),
            LinkRevoked::NotLinked,
            "{name}"
        );

        // An account Sam does not hold (another login of it is between
        // storing its sign-in and linking) is left alone.
        sign_in(store, "other@example.com").await;
        assert_eq!(
            revoke_account_link(store, sam, STORE, "other@example.com")
                .await
                .unwrap(),
            LinkRevoked::NotLinked,
            "{name}"
        );
        assert!(signed_in(store, "other@example.com").await, "{name}");
        assert_eq!(
            status(store, "other@example.com").await,
            ("active".to_string(), true),
            "{name}"
        );

        // A reconnect that re-sealed the sign-in after the revoke's earlier
        // steps is revoked with the link, never left unlinked and live.
        sign_in(store, "sam@example.com").await;
        link_signed_in_account(store, sam, STORE, "sam@example.com", "libro")
            .await
            .unwrap();
        store
            .revoke_credentials(STORE, "sam@example.com")
            .await
            .unwrap();
        sign_in(store, "sam@example.com").await;
        link_signed_in_account(store, sam, STORE, "sam@example.com", "libro")
            .await
            .unwrap();
        assert_eq!(
            revoke_account_link(store, sam, STORE, "sam@example.com")
                .await
                .unwrap(),
            LinkRevoked::Revoked,
            "{name}"
        );
        assert!(!signed_in(store, "sam@example.com").await, "{name}");
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

#[test]
fn account_slots_never_collide() {
    assert_ne!(account_slot("a:b", "c"), account_slot("a", "b:c"));
    assert_eq!(account_slot("p", "x"), account_slot("p", "x"));
}
