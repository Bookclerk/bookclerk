//! Tests for the sealed database credential: each store, replacement,
//! failing closed, and following a master-key move.

use std::sync::Arc;

use bookclerk_keystore::{KeychainPointer, KeystoreErrorKind, MemoryVault, PointerStore};
use tempfile::tempdir;

use super::*;
use crate::master_key::{master_key_test_lock, MasterKey};

/// A fresh random test DEK; each call returns a different key.
fn dek() -> MasterKey {
    MasterKey::from_test_bytes(rand::random())
}

/// In-memory stores for credential items, installed process-wide while the
/// master-key test lock is held.
struct Stores {
    /// The keychain stand-in.
    keychain: Arc<MemoryVault>,
    /// The `systemd-creds` stand-in (one for every directory).
    creds: Arc<MemoryVault>,
}

impl Stores {
    /// Installs fresh in-memory stores.
    fn install() -> Self {
        let keychain = Arc::new(MemoryVault::new());
        let creds = Arc::new(MemoryVault::new());
        let factory_creds = Arc::clone(&creds);
        set_database_credential_vaults(
            keychain.clone(),
            Arc::new(move |_dir: &Path| -> Arc<dyn Vault> { factory_creds.clone() }),
        );
        Self { keychain, creds }
    }
}

impl Drop for Stores {
    fn drop(&mut self) {
        set_database_credential_vaults(
            Arc::new(OsKeychain::for_kind(ItemKind::DatabaseCredential)),
            Arc::new(|dir: &Path| -> Arc<dyn Vault> {
                Arc::new(SystemdCredsVault::new(dir).for_kind(ItemKind::DatabaseCredential))
            }),
        );
    }
}

/// Writes a `master.key` of the given store into `dir` (its contents only
/// matter for the header the credential follows).
fn write_master_key(dir: &Path, store: KeyStoreKind) {
    let bytes = match store {
        KeyStoreKind::File => {
            let mut raw = b"BCK1".to_vec();
            raw.extend_from_slice(&[7u8; 32]);
            raw
        }
        KeyStoreKind::Keychain => pointer(PointerStore::Keychain),
        KeyStoreKind::SystemdCreds => pointer(PointerStore::SystemdCreds),
    };
    std::fs::write(dir.join("master.key"), bytes).unwrap();
}

/// A `master.key` pointer into `store`.
fn pointer(store: PointerStore) -> Vec<u8> {
    KeychainPointer {
        store,
        item: KeychainItemId::random(),
        fingerprint: [0u8; 32],
    }
    .encode()
    .to_vec()
}

/// A credential for tests.
fn credential(token: &str) -> DatabaseCredential {
    DatabaseCredential::new(token, Some("tok_123")).unwrap()
}

#[test]
fn a_file_install_keeps_the_sealed_token_in_the_file() {
    let _lock = master_key_test_lock();
    let stores = Stores::install();
    let dir = tempdir().unwrap();
    write_master_key(dir.path(), KeyStoreKind::File);
    let key = dek();

    assert!(load_database_credential(dir.path(), &key)
        .unwrap()
        .is_none());
    let kind = store_database_credential(dir.path(), &key, &credential("secret-token")).unwrap();
    assert_eq!(kind, KeyStoreKind::File);
    let raw = std::fs::read(database_credential_path(dir.path())).unwrap();
    assert_eq!(&raw[..4], b"BCD1");
    assert!(
        !raw.windows(12).any(|w| w == b"secret-token"),
        "the token is sealed"
    );
    assert!(stores.keychain.is_empty() && stores.creds.is_empty());
    let back = load_database_credential(dir.path(), &key).unwrap().unwrap();
    assert_eq!(back.token(), "secret-token");
    assert_eq!(back.token_id(), Some("tok_123"));
}

#[test]
fn a_keychain_install_keeps_only_a_pointer_in_the_file() {
    let _lock = master_key_test_lock();
    let stores = Stores::install();
    let dir = tempdir().unwrap();
    write_master_key(dir.path(), KeyStoreKind::Keychain);
    let key = dek();

    let kind = store_database_credential(dir.path(), &key, &credential("first")).unwrap();
    assert_eq!(kind, KeyStoreKind::Keychain);
    let raw = std::fs::read(database_credential_path(dir.path())).unwrap();
    assert_eq!(&raw[..4], b"BCD3");
    assert_eq!(raw.len(), 4 + 32 + 16, "fingerprint and item id only");
    assert_eq!(stores.keychain.len(), 1);

    // Replacing deletes the old item.
    store_database_credential(dir.path(), &key, &credential("second")).unwrap();
    assert_eq!(stores.keychain.len(), 1);
    let back = load_database_credential(dir.path(), &key).unwrap().unwrap();
    assert_eq!(back.token(), "second");

    assert!(remove_database_credential(dir.path()).unwrap());
    assert!(stores.keychain.is_empty());
    assert!(!remove_database_credential(dir.path()).unwrap());
}

#[test]
fn a_systemd_creds_install_seals_the_item_with_the_tpm_store() {
    let _lock = master_key_test_lock();
    let stores = Stores::install();
    let dir = tempdir().unwrap();
    write_master_key(dir.path(), KeyStoreKind::SystemdCreds);
    let key = dek();

    let kind = store_database_credential(dir.path(), &key, &credential("tpm-token")).unwrap();
    assert_eq!(kind, KeyStoreKind::SystemdCreds);
    let raw = std::fs::read(database_credential_path(dir.path())).unwrap();
    assert_eq!(&raw[..4], b"BCD4");
    assert_eq!(stores.creds.len(), 1);
    assert!(stores.keychain.is_empty());
    let back = load_database_credential(dir.path(), &key).unwrap().unwrap();
    assert_eq!(back.token(), "tpm-token");
}

#[test]
fn reading_fails_closed() {
    let _lock = master_key_test_lock();
    let stores = Stores::install();
    let dir = tempdir().unwrap();
    write_master_key(dir.path(), KeyStoreKind::Keychain);
    let key = dek();
    store_database_credential(dir.path(), &key, &credential("tok")).unwrap();

    let other = load_database_credential(dir.path(), &dek()).unwrap_err();
    assert!(
        other.to_string().contains("different master key"),
        "{other}"
    );

    stores
        .keychain
        .fail_with(Some(KeystoreErrorKind::AccessDenied));
    let locked = load_database_credential(dir.path(), &key).unwrap_err();
    assert!(locked.to_string().contains("could not be read"), "{locked}");
    stores.keychain.fail_with(None);

    let path = database_credential_path(dir.path());
    std::fs::write(&path, b"BCD3short").unwrap();
    assert!(load_database_credential(dir.path(), &key).is_err());
    std::fs::write(&path, b"nope").unwrap();
    assert!(load_database_credential(dir.path(), &key).is_err());
    std::fs::write(&path, b"XXXX").unwrap();
    assert!(load_database_credential(dir.path(), &key).is_err());

    // A file record whose blob is too short to hold a nonce.
    let mut raw = b"BCD1".to_vec();
    raw.extend_from_slice(&fingerprint(&key));
    raw.extend_from_slice(&[0u8; 8]);
    std::fs::write(&path, &raw).unwrap();
    let short = load_database_credential(dir.path(), &key).unwrap_err();
    assert!(short.to_string().contains("truncated"), "{short}");
}

#[test]
fn a_store_that_refuses_the_item_leaves_the_old_credential() {
    let _lock = master_key_test_lock();
    let stores = Stores::install();
    let dir = tempdir().unwrap();
    write_master_key(dir.path(), KeyStoreKind::Keychain);
    let key = dek();
    store_database_credential(dir.path(), &key, &credential("kept")).unwrap();

    stores
        .keychain
        .fail_with(Some(KeystoreErrorKind::Unavailable));
    assert!(store_database_credential(dir.path(), &key, &credential("lost")).is_err());
    stores.keychain.fail_with(None);
    let back = load_database_credential(dir.path(), &key).unwrap().unwrap();
    assert_eq!(back.token(), "kept");
}

#[test]
fn storing_needs_a_master_key() {
    let _lock = master_key_test_lock();
    let _stores = Stores::install();
    let dir = tempdir().unwrap();
    let err = store_database_credential(dir.path(), &dek(), &credential("tok")).unwrap_err();
    assert!(err.to_string().contains("no master key"), "{err}");
}

#[test]
fn the_credential_follows_the_master_key_to_another_store() {
    let _lock = master_key_test_lock();
    let stores = Stores::install();
    let dir = tempdir().unwrap();
    write_master_key(dir.path(), KeyStoreKind::File);
    let key = dek();
    store_database_credential(dir.path(), &key, &credential("moving")).unwrap();

    // Already where the key is: nothing changes.
    rehome_database_credential(dir.path(), &key).unwrap();
    assert!(stores.keychain.is_empty());

    write_master_key(dir.path(), KeyStoreKind::Keychain);
    rehome_database_credential(dir.path(), &key).unwrap();
    assert_eq!(stores.keychain.len(), 1);
    let raw = std::fs::read(database_credential_path(dir.path())).unwrap();
    assert_eq!(&raw[..4], b"BCD3");

    write_master_key(dir.path(), KeyStoreKind::File);
    rehome_database_credential(dir.path(), &key).unwrap();
    assert!(stores.keychain.is_empty(), "the keychain item was deleted");
    let back = load_database_credential(dir.path(), &key).unwrap().unwrap();
    assert_eq!(back.token(), "moving");

    // Nothing stored: nothing to do.
    let empty = tempdir().unwrap();
    rehome_database_credential(empty.path(), &key).unwrap();
}

#[test]
fn tokens_and_ids_are_validated() {
    assert!(DatabaseCredential::new("", None).is_err());
    assert!(DatabaseCredential::new("   ", None).is_err());
    assert!(DatabaseCredential::new("has space", None).is_err());
    assert!(DatabaseCredential::new(&"x".repeat(MAX_TOKEN_LEN + 1), None).is_err());
    assert!(DatabaseCredential::new("tok", Some("bad id")).is_err());
    assert!(DatabaseCredential::new("tok", Some(&"a".repeat(MAX_TOKEN_ID_LEN + 1))).is_err());
    let ok = DatabaseCredential::new("  tok  ", Some("  ")).unwrap();
    assert_eq!(ok.token(), "tok");
    assert_eq!(ok.token_id(), None);
    let debug = format!(
        "{:?}",
        DatabaseCredential::new("hidden", Some("id-1")).unwrap()
    );
    assert!(
        !debug.contains("hidden") && debug.contains("id-1"),
        "{debug}"
    );
}

#[test]
fn the_d1_token_comes_from_the_environment_or_the_stored_credential() {
    let _lock = master_key_test_lock();
    let _stores = Stores::install();
    let dir = tempdir().unwrap();
    write_master_key(dir.path(), KeyStoreKind::File);
    let key = dek();
    let missing = resolve_d1_token_with(None, dir.path(), Some(&key)).unwrap_err();
    assert!(
        missing.to_string().contains("database setup d1"),
        "{missing}"
    );
    store_database_credential(dir.path(), &key, &credential("stored-token")).unwrap();
    assert!(d1_api_token_configured(dir.path()));
    assert_eq!(
        resolve_d1_token_with(None, dir.path(), Some(&key))
            .unwrap()
            .as_str(),
        "stored-token"
    );
    assert_eq!(
        resolve_d1_token_with(Some("from-env".into()), dir.path(), Some(&key))
            .unwrap()
            .as_str(),
        "from-env",
        "the environment wins"
    );
    assert!(resolve_d1_token_with(None, dir.path(), None).is_err());
}
