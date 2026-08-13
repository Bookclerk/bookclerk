//! Claim redeem must fail closed when the sqlite proxy cannot begin or commit.

use bookclerk_library::{LibraryStore, UserRole};
use chrono::Utc;

fn invite_password() -> String {
    ["invite", "-", "password", "-", "ok"].concat()
}

async fn claim_store() -> (LibraryStore, i64, i64, String) {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Invitee"), None)
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user.id, Some("Invitee"))
        .await
        .unwrap();
    let ticket_hash = "inject-claim-ticket-hash".to_string();
    store
        .insert_claim_ticket(
            &ticket_hash,
            Some(identity.id),
            Utc::now() + chrono::Duration::hours(1),
            "test",
        )
        .await
        .unwrap();
    (store, user.id, identity.id, ticket_hash)
}

#[tokio::test]
async fn injected_db_begin_failure_does_not_redeem() {
    let (store, user_id, identity_id, ticket_hash) = claim_store().await;
    let hash = bookclerk_library::hash_password(&invite_password()).unwrap();
    let expires = Utc::now() + chrono::Duration::hours(12);

    bookclerk_library::inject_begin_failures(1);
    let err = store
        .redeem_claim_ticket_to_session(
            &ticket_hash,
            "session-hash-begin-fail",
            expires,
            None,
            Some(hash.as_str()),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("begin failed"),
        "expected begin failure, got {err}"
    );
    assert!(store
        .get_user_password_hash(user_id)
        .await
        .unwrap()
        .is_none());
    assert!(store
        .list_portal_sessions_for_identity(identity_id)
        .await
        .unwrap()
        .is_empty());
    let ticket = store
        .get_claim_ticket_by_hash(&ticket_hash)
        .await
        .unwrap()
        .unwrap();
    assert!(ticket.redeemed_at.is_none());

    store
        .redeem_claim_ticket_to_session(
            &ticket_hash,
            "session-hash-begin-ok",
            expires,
            None,
            Some(hash.as_str()),
            None,
        )
        .await
        .unwrap();
    assert!(store
        .get_user_password_hash(user_id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn injected_db_commit_failure_does_not_redeem() {
    let (store, user_id, identity_id, ticket_hash) = claim_store().await;
    let hash = bookclerk_library::hash_password(&invite_password()).unwrap();
    let expires = Utc::now() + chrono::Duration::hours(12);

    bookclerk_library::inject_commit_failures(1);
    let err = store
        .redeem_claim_ticket_to_session(
            &ticket_hash,
            "session-hash-commit-fail",
            expires,
            None,
            Some(hash.as_str()),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("commit failed"),
        "expected commit failure, got {err}"
    );
    assert!(store
        .get_user_password_hash(user_id)
        .await
        .unwrap()
        .is_none());
    assert!(store
        .list_portal_sessions_for_identity(identity_id)
        .await
        .unwrap()
        .is_empty());
    let ticket = store
        .get_claim_ticket_by_hash(&ticket_hash)
        .await
        .unwrap()
        .unwrap();
    assert!(ticket.redeemed_at.is_none());

    store
        .redeem_claim_ticket_to_session(
            &ticket_hash,
            "session-hash-commit-ok",
            expires,
            None,
            Some(hash.as_str()),
            None,
        )
        .await
        .unwrap();
    assert!(store
        .get_user_password_hash(user_id)
        .await
        .unwrap()
        .is_some());
}
