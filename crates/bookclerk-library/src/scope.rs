//! Plugin-scoped library access.
//!
//! First-party and third-party content sources share the same boundary:
//! a [`SourceScope`] can only read/write rows owned by one plugin instance
//! (`plugin_instance_id`). Two instances of the same storefront key therefore
//! see disjoint accounts, secrets, and books; `source` / `provider` carry the
//! plugin key for display only. Host code that needs the full store keeps its own
//! [`LibraryStore`] handle — this type does not expose one.

use serde_json::Value;

use crate::error::{LibraryError, Result};
use crate::models::{AccountRecord, BookRecord};
use crate::secrets::{
    build_sealed_record, delete_secret_for_instance, get_secret_for_instance, list_secrets,
    secret_account_type, secret_kind, unseal_secret, upsert_secret, EncryptedSecretRecord,
};
use crate::store::{LibraryStore, NewBook};

/// Host-enforced view of [`LibraryStore`] for one deployed content-source plugin instance.
///
/// Both in-repo first-party adapters and external JSON-RPC plugins must use
/// this type (via a `bookclerk_source::ContentSource` adapter or the external
/// host adapter). Secrets, accounts, and books for other instances (even of
/// the same plugin key) are invisible.
#[derive(Clone)]
pub struct SourceScope {
    /// Full library store; all accessors filter to [`Self::plugin_instance_id`].
    store: LibraryStore,
    /// Plugin instance that owns rows visible through this scope.
    plugin_instance_id: String,
    /// Plugin key / storefront id (`audible`, `libro`, …) written to `source` / `provider`.
    plugin_key: String,
}

impl std::fmt::Debug for SourceScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceScope")
            .field("plugin_instance_id", &self.plugin_instance_id)
            .field("plugin_key", &self.plugin_key)
            .finish_non_exhaustive()
    }
}

impl SourceScope {
    /// Build a scope for one plugin instance over `store`.
    #[must_use]
    pub fn new(
        store: LibraryStore,
        plugin_instance_id: impl Into<String>,
        plugin_key: impl Into<String>,
    ) -> Self {
        Self {
            store,
            plugin_instance_id: plugin_instance_id.into(),
            plugin_key: plugin_key.into(),
        }
    }

    /// Plugin key / storefront id for guest-facing labels (`audible`, `libro`, …).
    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.plugin_key
    }

    /// Plugin instance whose rows this scope reads and writes.
    #[must_use]
    pub fn plugin_instance_id(&self) -> &str {
        &self.plugin_instance_id
    }

    // ── Accounts ─────────────────────────────────────────────────────────────

    /// Upsert an account row; `source` is forced to this plugin id.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn upsert_account(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
    ) -> Result<AccountRecord> {
        self.store
            .upsert_account_for_instance(
                &self.plugin_instance_id,
                account_id,
                marketplace,
                label,
                scan_enabled,
                &self.plugin_key,
            )
            .await
    }

    /// Ensure an account exists without flipping `scan_enabled` on conflict.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn ensure_account(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
    ) -> Result<AccountRecord> {
        self.store
            .ensure_account_for_instance(
                &self.plugin_instance_id,
                account_id,
                marketplace,
                label,
                &self.plugin_key,
            )
            .await
    }

    /// Lists all store accounts in the library database.
    ///
    ///
    /// # Returns
    ///
    /// `Result<Vec<AccountRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        let all = self.store.list_accounts().await?;
        Ok(all
            .into_iter()
            .filter(|a| a.plugin_instance_id == self.plugin_instance_id)
            .collect())
    }

    /// Fetch one account if it belongs to this plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_account(&self, account_id: &str) -> Result<Option<AccountRecord>> {
        self.store
            .get_account_in_instance(&self.plugin_instance_id, account_id)
            .await
    }

    // ── Books ────────────────────────────────────────────────────────────────

    /// Upsert a book; `source` is forced to this plugin id.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn upsert_book(&self, book: &NewBook) -> Result<BookRecord> {
        let mut book = book.clone();
        book.source = self.plugin_key.clone();
        book.plugin_instance_id = self.plugin_instance_id.clone();
        self.store.upsert_book(&book).await
    }

    // ── Secrets (provider locked to plugin id) ───────────────────────────────

    /// Seal and store a `source_auth` credential for `account_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn save_source_auth(
        &self,
        account_id: &str,
        name: &str,
        plaintext: &[u8],
    ) -> Result<()> {
        let mut record = build_sealed_record(
            plaintext,
            secret_kind::SOURCE_AUTH,
            &self.plugin_key,
            secret_account_type::INTEGRATION,
            account_id,
            name,
        )?;
        record
            .plugin_instance_id
            .clone_from(&self.plugin_instance_id);
        upsert_secret(self.store.db(), &record).await
    }

    /// Seal and store an arbitrary secret kind under this plugin's provider.
    ///
    /// Used for Widevine CDMs (`secret_kind::WIDEVINE`) and similar. Provider is
    /// always this plugin id.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn save_secret(
        &self,
        kind: &str,
        account_id: &str,
        name: &str,
        plaintext: &[u8],
    ) -> Result<()> {
        let mut record = build_sealed_record(
            plaintext,
            kind,
            &self.plugin_key,
            secret_account_type::INTEGRATION,
            account_id,
            name,
        )?;
        record
            .plugin_instance_id
            .clone_from(&self.plugin_instance_id);
        upsert_secret(self.store.db(), &record).await
    }

    /// Load and unseal a `source_auth` credential for this plugin only.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn load_source_auth(&self, account_id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        self.load_secret(secret_kind::SOURCE_AUTH, account_id, name)
            .await
    }

    /// Load and unseal a secret for this plugin only.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn load_secret(
        &self,
        kind: &str,
        account_id: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>> {
        let record = get_secret_for_instance(
            self.store.db(),
            kind,
            Some(&self.plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
            &self.plugin_instance_id,
            name,
        )
        .await?;
        match record {
            Some(rec) => Ok(Some(unseal_secret(&rec)?)),
            None => Ok(None),
        }
    }

    /// Fetch the sealed record (for formats that need custom unseal, e.g. Audible).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_source_auth_record(
        &self,
        account_id: &str,
        name: &str,
    ) -> Result<Option<EncryptedSecretRecord>> {
        self.get_secret_record(secret_kind::SOURCE_AUTH, account_id, name)
            .await
    }

    /// Fetch a sealed record of any kind for this plugin (custom unseal / legacy formats).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_secret_record(
        &self,
        kind: &str,
        account_id: &str,
        name: &str,
    ) -> Result<Option<EncryptedSecretRecord>> {
        get_secret_for_instance(
            self.store.db(),
            kind,
            Some(&self.plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
            &self.plugin_instance_id,
            name,
        )
        .await
    }

    /// List `source_auth` secrets for this plugin only (never other providers).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_source_auth(&self) -> Result<Vec<EncryptedSecretRecord>> {
        let all = list_secrets(self.store.db(), secret_kind::SOURCE_AUTH).await?;
        Ok(all
            .into_iter()
            .filter(|r| {
                r.plugin_instance_id == self.plugin_instance_id
                    && r.account_type == secret_account_type::INTEGRATION
            })
            .collect())
    }

    /// Delete a `source_auth` secret for this plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_source_auth(&self, account_id: &str, name: &str) -> Result<()> {
        delete_secret_for_instance(
            self.store.db(),
            secret_kind::SOURCE_AUTH,
            Some(&self.plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
            &self.plugin_instance_id,
            name,
        )
        .await
    }

    /// Delete a secret of any kind for this plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_secret(&self, kind: &str, account_id: &str, name: &str) -> Result<()> {
        delete_secret_for_instance(
            self.store.db(),
            kind,
            Some(&self.plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
            &self.plugin_instance_id,
            name,
        )
        .await
    }

    /// Upsert opaque JSON credentials (external plugin login blob).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn save_credentials_json(&self, account_id: &str, credentials: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(credentials)
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("credentials JSON: {e}")))?;
        let name = format!("{account_id}.plugin.auth");
        self.save_source_auth(account_id, &name, &bytes).await
    }

    /// Load opaque JSON credentials for an external (or first-party) account.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn load_credentials_json(&self, account_id: &str) -> Result<Option<Value>> {
        let name = format!("{account_id}.plugin.auth");
        let Some(bytes) = self.load_source_auth(account_id, &name).await? else {
            return Ok(None);
        };
        let value = serde_json::from_slice(&bytes)
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("credentials JSON: {e}")))?;
        Ok(Some(value))
    }
}

impl LibraryStore {
    /// Transitional unscoped view: one implicit instance per storefront key.
    ///
    /// The instance id is `plugin_key` itself. Prefer [`Self::scope_instance`]
    /// whenever a deployed plugin instance id is known.
    #[must_use]
    pub fn scope(&self, plugin_key: impl Into<String>) -> SourceScope {
        let plugin_key = plugin_key.into();
        SourceScope::new(self.clone(), plugin_key.clone(), plugin_key)
    }

    /// Instance-scoped view — accounts, secrets, and books are keyed by `plugin_instance_id`.
    #[must_use]
    pub fn scope_instance(
        &self,
        plugin_instance_id: impl Into<String>,
        plugin_key: impl Into<String>,
    ) -> SourceScope {
        SourceScope::new(self.clone(), plugin_instance_id, plugin_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master_key::{ensure_shared_test_dek, master_key_test_read_lock_async};

    #[tokio::test]
    async fn scope_isolates_secrets_and_books() {
        let _dek = master_key_test_read_lock_async().await;
        ensure_shared_test_dek();
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let audible = store.scope("audible");
        let libro = store.scope("libro");

        audible
            .save_source_auth("alice", "alice.audible.auth", b"audible-secret")
            .await
            .unwrap();
        libro
            .save_source_auth("bob", "bob.libro.auth", b"libro-secret")
            .await
            .unwrap();

        assert_eq!(
            audible
                .load_source_auth("alice", "alice.audible.auth")
                .await
                .unwrap()
                .as_deref(),
            Some(b"audible-secret".as_slice())
        );
        assert!(audible
            .load_source_auth("bob", "bob.libro.auth")
            .await
            .unwrap()
            .is_none());
        assert_eq!(audible.list_source_auth().await.unwrap().len(), 1);
        assert_eq!(libro.list_source_auth().await.unwrap().len(), 1);

        audible.ensure_account("alice", "us", None).await.unwrap();
        libro.ensure_account("bob", "us", None).await.unwrap();

        audible
            .upsert_book(&NewBook::minimal("ASIN1", "alice", "us", "A"))
            .await
            .unwrap();
        let mut foreign = NewBook::minimal("ISBN1", "bob", "us", "B");
        foreign.source = "audible".into(); // attacker claim — forced back to libro
        libro.upsert_book(&foreign).await.unwrap();

        let books = store.list_books(None).await.unwrap();
        let libro_book = books.iter().find(|b| b.product_id == "ISBN1").unwrap();
        assert_eq!(libro_book.source, "libro");
    }

    #[tokio::test]
    async fn two_instances_of_one_key_are_isolated() {
        let _dek = master_key_test_read_lock_async().await;
        ensure_shared_test_dek();
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let a = store.scope_instance("inst-a", "graphicaudio");
        let b = store.scope_instance("inst-b", "graphicaudio");
        assert_eq!(a.source_id(), "graphicaudio");
        assert_eq!(a.plugin_instance_id(), "inst-a");

        a.save_source_auth("same", "same.auth", b"secret-a")
            .await
            .unwrap();
        b.save_source_auth("same", "same.auth", b"secret-b")
            .await
            .unwrap();
        assert_eq!(
            a.load_source_auth("same", "same.auth")
                .await
                .unwrap()
                .as_deref(),
            Some(b"secret-a".as_slice())
        );
        assert_eq!(
            b.load_source_auth("same", "same.auth")
                .await
                .unwrap()
                .as_deref(),
            Some(b"secret-b".as_slice())
        );
        assert_eq!(a.list_source_auth().await.unwrap().len(), 1);
        assert_eq!(b.list_source_auth().await.unwrap().len(), 1);

        a.upsert_account("same", "us", Some("A"), true)
            .await
            .unwrap();
        b.upsert_account("same", "us", Some("B"), true)
            .await
            .unwrap();
        assert_eq!(a.list_accounts().await.unwrap().len(), 1);
        assert_eq!(
            b.get_account("same")
                .await
                .unwrap()
                .unwrap()
                .label
                .as_deref(),
            Some("B")
        );

        a.upsert_book(&NewBook::minimal("P1", "same", "us", "From A"))
            .await
            .unwrap();
        b.upsert_book(&NewBook::minimal("P1", "same", "us", "From B"))
            .await
            .unwrap();
        let books = store.list_books(None).await.unwrap();
        assert_eq!(books.len(), 2);
        let mut instances: Vec<_> = books
            .iter()
            .map(|b| b.plugin_instance_id.as_str())
            .collect();
        instances.sort_unstable();
        assert_eq!(instances, ["inst-a", "inst-b"]);
        assert!(books.iter().all(|b| b.source == "graphicaudio"));

        a.delete_source_auth("same", "same.auth").await.unwrap();
        assert!(a
            .load_source_auth("same", "same.auth")
            .await
            .unwrap()
            .is_none());
        assert!(b
            .load_source_auth("same", "same.auth")
            .await
            .unwrap()
            .is_some());
    }
}
