//! Plugin-scoped library access.
//!
//! First-party and third-party content sources share the same boundary:
//! a [`SourceScope`] can only read/write rows whose `plugin_key` / `provider`
//! matches the provenance-qualified PluginKey it was opened for. Rows are not
//! tied to a plugin instance: every instance of that plugin, on any host,
//! sees the same accounts, credentials, and books. Host code that needs the
//! full store keeps its own [`LibraryStore`] handle — this type does not
//! expose one.

use serde_json::Value;

use crate::error::{LibraryError, Result};
use crate::models::{AccountRecord, BookRecord};
use crate::secrets::{
    build_sealed_record, delete_secret, get_secret, list_secrets, secret_account_type, secret_kind,
    unseal_secret, upsert_secret, EncryptedSecretRecord,
};
use crate::store::{LibraryStore, NewBook};

/// Host-enforced view of [`LibraryStore`] for one content-source plugin.
///
/// Both in-repo first-party adapters and external plugins must use this type
/// (via a `bookclerk_source::ContentSource` adapter or the external host
/// adapter). Secrets and books for other plugin keys are invisible. A
/// third-party package that reuses a first-party alias has a different key
/// and therefore sees nothing.
#[derive(Clone)]
pub struct SourceScope {
    /// Full library store; all accessors filter to [`Self::plugin_key`].
    store: LibraryStore,
    /// Provenance-qualified PluginKey that owns rows visible through this scope.
    plugin_key: String,
    /// Display alias (`audible`, `libro`, …) written to the `source` column.
    source_id: String,
}

impl std::fmt::Debug for SourceScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceScope")
            .field("plugin_key", &self.plugin_key)
            .field("source_id", &self.source_id)
            .finish_non_exhaustive()
    }
}

impl SourceScope {
    /// Build a scope for `plugin_key` over `store`, labelling rows `source_id`.
    #[must_use]
    pub fn new(
        store: LibraryStore,
        plugin_key: impl Into<String>,
        source_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            plugin_key: plugin_key.into(),
            source_id: source_id.into(),
        }
    }

    /// Provenance-qualified PluginKey this scope isolates on.
    #[must_use]
    pub fn plugin_key(&self) -> &str {
        &self.plugin_key
    }

    /// Display alias (`audible`, `libro`, external `plugin.toml` id, …).
    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.source_id
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
            .upsert_account(
                &self.plugin_key,
                account_id,
                marketplace,
                label,
                scan_enabled,
                &self.source_id,
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
            .ensure_account(
                &self.plugin_key,
                account_id,
                marketplace,
                label,
                &self.source_id,
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
            .filter(|a| a.plugin_key == self.plugin_key)
            .collect())
    }

    /// Fetch one account if it belongs to this plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_account(&self, account_id: &str) -> Result<Option<AccountRecord>> {
        self.store.get_account(&self.plugin_key, account_id).await
    }

    // ── Books ────────────────────────────────────────────────────────────────

    /// Upsert a book; `plugin_key` and `source` are forced to this plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn upsert_book(&self, book: &NewBook) -> Result<BookRecord> {
        let mut book = book.clone();
        book.plugin_key = self.plugin_key.clone();
        book.source = self.source_id.clone();
        self.store.upsert_book(&book).await
    }

    /// Look up one of this plugin's books by uuid, product id, ASIN, or ISBN.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_book(&self, title_id: &str, account_id: &str) -> Result<Option<BookRecord>> {
        self.store
            .get_book_in(Some(&self.plugin_key), title_id, account_id)
            .await
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
        let record = build_sealed_record(
            plaintext,
            secret_kind::SOURCE_AUTH,
            &self.plugin_key,
            secret_account_type::INTEGRATION,
            account_id,
            name,
        )?;
        upsert_secret(self.store.db(), &record).await
    }

    /// Seal and store an arbitrary secret kind under this plugin's provider.
    ///
    /// Used for Widevine CDMs (`secret_kind::WIDEVINE`) and similar. Provider is
    /// always this plugin key.
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
        let record = build_sealed_record(
            plaintext,
            kind,
            &self.plugin_key,
            secret_account_type::INTEGRATION,
            account_id,
            name,
        )?;
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
        let record = get_secret(
            self.store.db(),
            kind,
            Some(&self.plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
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
        get_secret(
            self.store.db(),
            kind,
            Some(&self.plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
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
                r.provider.as_deref() == Some(self.plugin_key.as_str())
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
        delete_secret(
            self.store.db(),
            secret_kind::SOURCE_AUTH,
            Some(&self.plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
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
        delete_secret(
            self.store.db(),
            kind,
            Some(&self.plugin_key),
            secret_account_type::INTEGRATION,
            Some(account_id),
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
    /// Plugin-scoped view — first-party and third-party sources use this.
    ///
    /// `plugin_key` isolates rows; `source_id` is the display alias written
    /// to the `source` column.
    #[must_use]
    pub fn scope(
        &self,
        plugin_key: impl Into<String>,
        source_id: impl Into<String>,
    ) -> SourceScope {
        SourceScope::new(self.clone(), plugin_key, source_id)
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
        let audible = store.scope("platform:bookclerk/audible", "audible");
        let libro = store.scope("platform:bookclerk/libro", "libro");

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
        foreign.plugin_key = "platform:bookclerk/audible".into();
        libro.upsert_book(&foreign).await.unwrap();

        let books = store.list_books(None).await.unwrap();
        let libro_book = books.iter().find(|b| b.product_id == "ISBN1").unwrap();
        assert_eq!(libro_book.source, "libro");
        assert_eq!(libro_book.plugin_key, "platform:bookclerk/libro");

        // A third-party package that reuses the `audible` alias has a
        // different key and sees none of these rows.
        let impostor = store.scope("path:file:///opt/audible", "audible");
        assert!(impostor.list_accounts().await.unwrap().is_empty());
        assert!(impostor
            .load_source_auth("alice", "alice.audible.auth")
            .await
            .unwrap()
            .is_none());
    }
}
