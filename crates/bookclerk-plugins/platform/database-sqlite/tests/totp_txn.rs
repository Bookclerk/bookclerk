//! TOTP enroll/disable must fail closed when the sqlite proxy cannot commit.
//!
//! `LibraryStore::confirm_totp_enrollment` / `disable_user_totp` run through
//! native `dbAtomic` (`execute_db_atomic`) when no guest backend is attached.

use bookclerk_library::{
    build_sealed_record, list_secrets, secret_account_type, secret_kind, upsert_secret,
    LibraryStore, UserRole,
};

const TOTP_SECRET: &str = "JBSWY3DPEHPK3PXP";

async fn totp_secret_names(store: &LibraryStore, user_id: i64) -> Vec<String> {
    let uid = user_id.to_string();
    let mut names: Vec<String> = list_secrets(store.db(), secret_kind::TOTP)
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.account_id.as_deref() == Some(uid.as_str()))
        .map(|row| row.name)
        .collect();
    names.sort();
    names
}

async fn store_pending_totp(store: &LibraryStore, user_id: i64) {
    let record = build_sealed_record(
        TOTP_SECRET.as_bytes(),
        secret_kind::TOTP,
        "local",
        secret_account_type::USER,
        &user_id.to_string(),
        "pending",
    )
    .unwrap();
    upsert_secret(store.db(), &record).await.unwrap();
}

#[tokio::test]
async fn injected_commit_failure_rolls_back_totp_enroll_and_disable() {
    let dir = tempfile::tempdir().unwrap();
    bookclerk_library::configure_master_key(dir.path()).unwrap();
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Totp Atomic"), None)
        .await
        .unwrap();
    store_pending_totp(&store, user.id).await;
    bookclerk_library::inject_commit_failures(1);
    let enroll_err = store
        .confirm_totp_enrollment(user.id, TOTP_SECRET)
        .await
        .unwrap_err();
    assert!(
        enroll_err.to_string().contains("commit failed"),
        "expected enroll commit failure, got {enroll_err}"
    );
    assert!(!store.get_user(user.id).await.unwrap().unwrap().totp_enabled);
    assert_eq!(totp_secret_names(&store, user.id).await, vec!["pending"]);

    store
        .confirm_totp_enrollment(user.id, TOTP_SECRET)
        .await
        .unwrap();
    assert!(store.get_user(user.id).await.unwrap().unwrap().totp_enabled);
    assert_eq!(totp_secret_names(&store, user.id).await, vec!["primary"]);

    bookclerk_library::inject_commit_failures(1);
    let disable_err = store.disable_user_totp(user.id).await.unwrap_err();
    assert!(
        disable_err.to_string().contains("commit failed"),
        "expected disable commit failure, got {disable_err}"
    );
    assert!(store.get_user(user.id).await.unwrap().unwrap().totp_enabled);
    assert_eq!(totp_secret_names(&store, user.id).await, vec!["primary"]);

    store.disable_user_totp(user.id).await.unwrap();
    assert!(!store.get_user(user.id).await.unwrap().unwrap().totp_enabled);
    assert!(totp_secret_names(&store, user.id).await.is_empty());
}
