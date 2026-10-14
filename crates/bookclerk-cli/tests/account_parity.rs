//! CLI parity with the Accounts portal: an account connected by plugin key is
//! what `bookclerk library` sees, and a second plugin that shares the
//! storefront alias owns neither the row nor the credentials.

mod common;

use bookclerk_library::{secret_account_type, secret_kind, LibraryStore};
use common::{run_bookclerk, stage_sqlite_plugin, stderr, stdout, write_config};

const ACCOUNT: &str = "alice@example.com";
const PLATFORM_KEY: &str = "platform:bookclerk-plugin-source-audible#audible";
const THIRD_PARTY_KEY: &str = "path:file:///opt/plugins/audible#audible";

async fn open_store(files: &std::path::Path) -> LibraryStore {
    bookclerk_library::configure_master_key(files).expect("load the minted master key");
    let db = bookclerk_plugin_database_sqlite::open(&files.join("library.db"))
        .await
        .expect("open library.db");
    LibraryStore::from_connection(db)
}

/// Connects `ACCOUNT` on `plugin_key` the way the portal does: an account row
/// plus a sealed `source_auth` secret owned by that plugin key.
async fn connect_like_the_portal(store: &LibraryStore, plugin_key: &str, token: &str) {
    store
        .upsert_account(plugin_key, ACCOUNT, "us", Some("Alice"), true, "audible")
        .await
        .expect("account row");
    store
        .scope(plugin_key, "audible")
        .save_source_auth(ACCOUNT, &format!("{ACCOUNT}.plugin.auth"), token.as_bytes())
        .await
        .expect("sealed credential");
}

async fn source_auth_owners(store: &LibraryStore) -> Vec<String> {
    let mut owners: Vec<String> =
        bookclerk_library::list_secrets(store.db(), secret_kind::SOURCE_AUTH)
            .await
            .unwrap()
            .into_iter()
            .filter(|row| row.account_type == secret_account_type::INTEGRATION)
            .filter_map(|row| row.provider)
            .collect();
    owners.sort();
    owners
}

#[tokio::test]
async fn cli_sees_portal_accounts_by_plugin_key_and_keeps_aliases_apart() {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    let enrolled = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    assert!(enrolled.status.success(), "{}", stderr(&enrolled));

    let store = open_store(files).await;
    connect_like_the_portal(&store, PLATFORM_KEY, "platform-token").await;
    connect_like_the_portal(&store, THIRD_PARTY_KEY, "third-party-token").await;
    assert_eq!(
        source_auth_owners(&store).await,
        vec![THIRD_PARTY_KEY.to_string(), PLATFORM_KEY.to_string()]
    );

    // Both rows are listed; neither plugin is deployed on this host, so they
    // come from the library rather than a running guest.
    let listed = run_bookclerk(files, &["library", "accounts", "--bare"], &[]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let rows = stdout(&listed)
        .lines()
        .filter(|line| line.contains(ACCOUNT))
        .count();
    assert_eq!(rows, 2, "two plugins own `{ACCOUNT}`:\n{}", stdout(&listed));

    // A bare id that matches both plugins is a conflict, not a guess.
    let ambiguous = run_bookclerk(
        files,
        &["library", "set-scan", ACCOUNT, "--scan", "false"],
        &[],
    );
    assert!(!ambiguous.status.success(), "{}", stdout(&ambiguous));
    let err = stderr(&ambiguous);
    assert!(err.contains("--source"), "expected a --source hint: {err}");

    // `--source` with the plugin key edits that plugin's row only.
    let scoped = run_bookclerk(
        files,
        &[
            "library",
            "set-scan",
            ACCOUNT,
            "--source",
            PLATFORM_KEY,
            "--scan",
            "false",
        ],
        &[],
    );
    assert!(scoped.status.success(), "{}", stderr(&scoped));
    let platform = store
        .find_account(ACCOUNT, Some(PLATFORM_KEY))
        .await
        .unwrap()
        .unwrap();
    let third_party = store
        .find_account(ACCOUNT, Some(THIRD_PARTY_KEY))
        .await
        .unwrap()
        .unwrap();
    assert!(!platform.scan_enabled);
    assert!(
        third_party.scan_enabled,
        "the other plugin's row is untouched"
    );

    // Revoking the third-party connection leaves the platform credential.
    let revoked = run_bookclerk(
        files,
        &[
            "library",
            "revoke-account",
            ACCOUNT,
            "--source",
            THIRD_PARTY_KEY,
        ],
        &[],
    );
    assert!(revoked.status.success(), "{}", stderr(&revoked));
    assert_eq!(
        source_auth_owners(&store).await,
        vec![PLATFORM_KEY.to_string()],
        "only the revoked plugin's credential is gone"
    );
    let platform = store
        .find_account(ACCOUNT, Some(PLATFORM_KEY))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(platform.connection_status, "revoked");
}
