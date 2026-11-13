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

use crate::control_plane::batch;
use crate::error::{LibraryError, Result};
use crate::models::{AccountRecord, BookRecord};
use crate::secrets::{
    build_sealed_record, delete_secret, forget_cached_secret, get_secret, list_secrets,
    secret_account_type, secret_kind, unseal_secret, upsert_secret, EncryptedSecretRecord,
};
use crate::store::{LibraryStore, NewBook, ScannedTitle};

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
        self.store.list_accounts_for_plugin(&self.plugin_key).await
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

    /// Upsert a scanned title as this plugin's, and resolve its listing.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or write fails.
    pub async fn upsert_scanned_title(&self, title: &ScannedTitle) -> Result<BookRecord> {
        let mut title = title.clone();
        title.book.plugin_key = self.plugin_key.clone();
        title.book.source = self.source_id.clone();
        self.store.upsert_scanned_title(&title).await
    }

    /// Upsert a page of scanned titles as this plugin's, and resolve their
    /// listings as one page (see [`LibraryStore::upsert_scanned_titles`]).
    ///
    /// # Errors
    ///
    /// Returns an error when a flat row cannot be read or written.
    pub async fn upsert_scanned_titles(&self, titles: &[ScannedTitle]) -> Result<Vec<BookRecord>> {
        let titles: Vec<ScannedTitle> = titles
            .iter()
            .map(|title| {
                let mut title = title.clone();
                title.book.plugin_key = self.plugin_key.clone();
                title.book.source = self.source_id.clone();
                title
            })
            .collect();
        self.store.upsert_scanned_titles(&titles).await
    }

    /// Where one of this plugin's accounts' scan stands.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn scan_state(&self, account_id: &str) -> Result<Option<crate::ScanState>> {
        self.store.scan_state(&self.plugin_key, account_id).await
    }

    /// Checkpoints a pass of one of this plugin's accounts after a page.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn save_scan_checkpoint(
        &self,
        account_id: &str,
        cursor: &str,
        pass_token: &str,
        pass_id: &str,
    ) -> Result<()> {
        self.store
            .save_scan_checkpoint(&self.plugin_key, account_id, cursor, pass_token, pass_id)
            .await
    }

    /// Stamps scan pass `pass_id` on one of this plugin's accounts' holdings
    /// of `product_ids` (see [`LibraryStore::mark_holdings_seen`]).
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    pub async fn mark_holdings_seen(
        &self,
        account_id: &str,
        pass_id: &str,
        product_ids: &[String],
    ) -> Result<()> {
        self.store
            .mark_holdings_seen(&self.plugin_key, account_id, pass_id, product_ids)
            .await
    }

    /// Removes one of this plugin's accounts' holdings that `selection`
    /// names, publishing `holding_removed` for each (see
    /// [`LibraryStore::remove_account_holdings`]).
    ///
    /// # Returns
    ///
    /// How many holdings were selected for removal.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or write fails.
    pub async fn remove_account_holdings(
        &self,
        account_id: &str,
        selection: crate::HoldingSelection<'_>,
    ) -> Result<usize> {
        self.store
            .remove_account_holdings(&self.plugin_key, account_id, selection)
            .await
    }

    /// Records the end of a pass of one of this plugin's accounts.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn finish_scan_pass(
        &self,
        account_id: &str,
        sync_token: &str,
        complete: bool,
    ) -> Result<()> {
        self.store
            .finish_scan_pass(&self.plugin_key, account_id, sync_token, complete)
            .await
    }

    /// Forgets the sync token and resume cursor of one of this plugin's
    /// accounts.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn reset_scan_state(&self, account_id: &str) -> Result<()> {
        self.store
            .reset_scan_state(&self.plugin_key, account_id)
            .await
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

    /// The branch that keeps this account's login fresh for this library
    /// (the branch it was shared with, once that branch has pushed a
    /// sign-in here; see [`crate::control_plane::confirmed_keeping_branch`]),
    /// or `None` when this install keeps it. For a single-keeper storefront the host then saves
    /// none of its own refreshes of the account: the branch pushes them.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn keeping_branch(&self, account_id: &str) -> Result<Option<String>> {
        crate::control_plane::confirmed_keeping_branch(&self.store, &self.plugin_key, account_id)
            .await
    }

    /// Stores the credentials a guest handed back after refreshing them
    /// (`ScanPage.refreshedCredentials`, `PlainFetch.refreshedCredentials`)
    /// in place of `sent`, the credentials the host gave it.
    ///
    /// Written only while the stored credentials are still `sent`: when the
    /// account was signed in again (or refreshed by another call) after
    /// `sent` was read, the newer credentials stay and nothing is written.
    /// The check and the write are one host batch guarded on the sealed row
    /// read, so a sign-in that lands in between wins. Nothing is written
    /// when the account has no stored credentials (it was disconnected),
    /// nor once a branch has taken the account's sign-in over for this
    /// library (it has applied a push from one, a `node:` row in
    /// `account_credential_revisions`), checked in the same batch so a
    /// first push that lands in between wins too.
    /// The new value is registered with log redaction before it is stored.
    ///
    /// # Arguments
    ///
    /// * `account_id` - The account the guest refreshed.
    /// * `sent` - The credentials the host sent the guest.
    /// * `refreshed` - The credentials the guest handed back, already
    ///   checked as one JSON object within the ABI bound.
    ///
    /// # Returns
    ///
    /// True when the refreshed credentials were stored.
    ///
    /// # Errors
    ///
    /// Returns an error when `refreshed` is not JSON, or a read, the seal or
    /// the write fails.
    pub async fn replace_refreshed_credentials(
        &self,
        account_id: &str,
        sent: &Value,
        refreshed: &[u8],
    ) -> Result<bool> {
        let parsed: Value = serde_json::from_slice(refreshed)
            .map_err(|e| LibraryError::Invalid(format!("refreshed credentials: {e}")))?;
        bookclerk_config::register_secret_bytes(refreshed);
        let name = format!("{account_id}.plugin.auth");
        let Some(current) = self
            .get_secret_record(secret_kind::SOURCE_AUTH, account_id, &name)
            .await?
        else {
            return Ok(false);
        };
        let stored: Value = serde_json::from_slice(&unseal_secret(&current)?)
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("credentials JSON: {e}")))?;
        if &stored != sent {
            return Ok(false);
        }
        if stored == parsed {
            return Ok(true);
        }
        let bytes = serde_json::to_vec(&parsed)
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("credentials JSON: {e}")))?;
        let record = build_sealed_record(
            &bytes,
            secret_kind::SOURCE_AUTH,
            &self.plugin_key,
            secret_account_type::INTEGRATION,
            account_id,
            &name,
        )?;
        let opt_int = |value: Option<u32>| batch::int_opt(value.map(i64::from));
        let now = chrono::Utc::now().to_rfc3339();
        let reply = self
            .store
            .execute_host_batch(batch::request(
                &format!("refreshed-credentials-{}", uuid::Uuid::new_v4()),
                vec![batch::exec(
                    "UPDATE encrypted_secrets
                        SET format = ?, ciphertext = ?, kdf_algorithm = ?, kdf_salt = ?,
                            kdf_m_cost = ?, kdf_t_cost = ?, kdf_p_cost = ?,
                            cipher_algorithm = ?, cipher_nonce = ?, updated_at = ?
                      WHERE kind = ? AND provider = ? AND account_type = ?
                        AND account_id = ? AND name = ? AND ciphertext = ?
                        AND NOT EXISTS (SELECT 1 FROM account_credential_revisions
                                         WHERE plugin_key = ? AND account_id = ?
                                           AND peer LIKE 'node:%')",
                    vec![
                        batch::text(&record.format),
                        batch::blob(&record.ciphertext),
                        batch::text_opt(record.kdf_algorithm.as_deref()),
                        batch::blob_opt(record.kdf_salt.as_deref()),
                        opt_int(record.kdf_m_cost),
                        opt_int(record.kdf_t_cost),
                        opt_int(record.kdf_p_cost),
                        batch::text_opt(record.cipher_algorithm.as_deref()),
                        batch::blob_opt(record.cipher_nonce.as_deref()),
                        batch::text(&now),
                        batch::text(secret_kind::SOURCE_AUTH),
                        batch::text(&self.plugin_key),
                        batch::text(secret_account_type::INTEGRATION),
                        batch::text(account_id),
                        batch::text(&name),
                        batch::blob(&current.ciphertext),
                        batch::text(&self.plugin_key),
                        batch::text(account_id),
                    ],
                )],
            ))
            .await?;
        let written = batch::rows_affected(&reply, 0) == 1;
        if written {
            forget_cached_secret(&record);
        }
        Ok(written)
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
    use crate::test_support::{sqlite, Profile};

    #[tokio::test]
    async fn refreshed_credentials_replace_only_what_was_sent() {
        let _dek = master_key_test_read_lock_async().await;
        ensure_shared_test_dek();
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let scope = kit.store().scope("platform:bookclerk/audible", "audible");
            let name = profile.name();
            let sent = serde_json::json!({"token": "old"});
            scope.save_credentials_json("alice", &sent).await.unwrap();

            // The stored credentials are still the ones sent: replaced.
            assert!(
                scope
                    .replace_refreshed_credentials("alice", &sent, br#"{"token":"new"}"#)
                    .await
                    .unwrap(),
                "{name}"
            );
            let renewed = serde_json::json!({"token": "new"});
            assert_eq!(
                scope.load_credentials_json("alice").await.unwrap(),
                Some(renewed.clone()),
                "{name}"
            );

            // A refresh of credentials that were replaced since (a sign-in,
            // or another refresh) leaves the newer ones.
            assert!(
                !scope
                    .replace_refreshed_credentials("alice", &sent, br#"{"token":"stale"}"#)
                    .await
                    .unwrap(),
                "{name}"
            );
            assert_eq!(
                scope.load_credentials_json("alice").await.unwrap(),
                Some(renewed.clone()),
                "{name}"
            );

            // Handing back what is already stored writes nothing and is fine.
            assert!(
                scope
                    .replace_refreshed_credentials("alice", &renewed, br#"{"token":"new"}"#)
                    .await
                    .unwrap(),
                "{name}"
            );

            // A disconnected account (no stored credentials) gets nothing.
            assert!(
                !scope
                    .replace_refreshed_credentials("bob", &sent, br#"{"token":"x"}"#)
                    .await
                    .unwrap(),
                "{name}"
            );
            assert_eq!(scope.load_credentials_json("bob").await.unwrap(), None);

            // Bytes that are not JSON are refused.
            assert!(scope
                .replace_refreshed_credentials("alice", &renewed, b"not json")
                .await
                .is_err());
        }
    }

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

    /// The scan-pass and holdings calls act on this plugin's account.
    #[tokio::test]
    async fn scope_runs_a_scan_pass_for_its_account() {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let libro = store.scope("platform:bookclerk/libro", "libro");
        libro.ensure_account("bob", "us", None).await.unwrap();
        for product in ["ISBN1", "ISBN2"] {
            libro
                .upsert_book(&NewBook::minimal(product, "bob", "us", product))
                .await
                .unwrap();
        }

        libro
            .save_scan_checkpoint("bob", "c1", "", "p1")
            .await
            .unwrap();
        let state = libro.scan_state("bob").await.unwrap().unwrap();
        assert_eq!(
            (state.resume_cursor.as_str(), state.pass_id.as_str()),
            ("c1", "p1")
        );

        libro
            .mark_holdings_seen("bob", "p1", &["ISBN1".to_string()])
            .await
            .unwrap();
        let removed = libro
            .remove_account_holdings("bob", crate::HoldingSelection::UnseenBy("p1"))
            .await
            .unwrap();
        assert_eq!(removed, 1, "ISBN2 was not stamped");
        libro.finish_scan_pass("bob", "t1", true).await.unwrap();
        assert_eq!(
            libro.scan_state("bob").await.unwrap().unwrap().sync_token,
            "t1"
        );
        libro.reset_scan_state("bob").await.unwrap();
    }
}
